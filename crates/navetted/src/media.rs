use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use navette_protocol::media::{MediaInput, MediaKind, MediaPacket};
use thiserror::Error;
use tokio::sync::{Notify, mpsc};

const DEFAULT_CLIENT_QUEUE_CAPACITY: usize = 8;
const INPUT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct MediaHub {
    inner: Arc<Mutex<HubState>>,
    next_client_id: Arc<AtomicU64>,
    client_queue_capacity: usize,
}

#[derive(Default)]
struct HubState {
    sessions: HashMap<String, SessionMedia>,
}

struct SessionMedia {
    clients: HashMap<u64, Arc<ClientQueue>>,
    input: mpsc::Sender<MediaCommand>,
    streams: HashMap<u64, StreamBootstrap>,
}

#[derive(Default)]
struct StreamBootstrap {
    config: Option<Arc<MediaPacket>>,
    keyframe: Option<Arc<MediaPacket>>,
    last_sequence: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaCommand {
    Input {
        attachment_id: u64,
        input: MediaInput,
    },
    Disconnected {
        attachment_id: u64,
    },
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct PublishStats {
    pub sent: usize,
    pub dropped: usize,
}

impl Default for MediaHub {
    fn default() -> Self {
        Self::with_client_queue_capacity(DEFAULT_CLIENT_QUEUE_CAPACITY)
    }
}

impl MediaHub {
    pub fn with_client_queue_capacity(client_queue_capacity: usize) -> Self {
        assert!(client_queue_capacity >= 2);
        Self {
            inner: Arc::new(Mutex::new(HubState::default())),
            next_client_id: Arc::new(AtomicU64::new(1)),
            client_queue_capacity,
        }
    }

    pub fn register_session(&self, session: impl Into<String>) -> mpsc::Receiver<MediaCommand> {
        let (input, receiver) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let mut state = self.inner.lock().expect("media hub lock poisoned");
        if let Some(previous) = state.sessions.insert(
            session.into(),
            SessionMedia {
                clients: HashMap::new(),
                input,
                streams: HashMap::new(),
            },
        ) {
            for queue in previous.clients.values() {
                queue.close();
            }
        }
        receiver
    }

    pub fn unregister_session(&self, session: &str) {
        let removed = self
            .inner
            .lock()
            .expect("media hub lock poisoned")
            .sessions
            .remove(session);
        if let Some(removed) = removed {
            for queue in removed.clients.values() {
                queue.close();
            }
        }
    }

    pub fn attach(&self, session: &str) -> Result<MediaAttachment, MediaHubError> {
        let client_id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let queue = Arc::new(ClientQueue::new(self.client_queue_capacity));
        let mut state = self.inner.lock().map_err(|_| MediaHubError::Unavailable)?;
        let session_state = state
            .sessions
            .get_mut(session)
            .ok_or_else(|| MediaHubError::UnknownSession(session.to_string()))?;
        let mut stream_ids = session_state.streams.keys().copied().collect::<Vec<_>>();
        stream_ids.sort_unstable();
        for stream_id in stream_ids {
            let stream = &session_state.streams[&stream_id];
            if let Some(config) = stream.config.clone() {
                queue.push(config);
            }
            if let Some(keyframe) = stream.keyframe.clone() {
                queue.push(keyframe);
            }
        }
        session_state.clients.insert(client_id, Arc::clone(&queue));
        Ok(MediaAttachment {
            client_id,
            session: session.to_string(),
            queue,
            hub: Arc::downgrade(&self.inner),
        })
    }

    pub fn publish(
        &self,
        session: &str,
        packet: MediaPacket,
    ) -> Result<PublishStats, MediaHubError> {
        packet.validate().map_err(MediaHubError::InvalidPacket)?;
        let packet = Arc::new(packet);
        let mut state = self.inner.lock().map_err(|_| MediaHubError::Unavailable)?;
        let session_state = state
            .sessions
            .get_mut(session)
            .ok_or_else(|| MediaHubError::UnknownSession(session.to_string()))?;
        if packet.header.width == 0
            || packet.header.height == 0
            || packet.header.width > 8192
            || packet.header.height > 8192
        {
            return Err(MediaHubError::InvalidDimensions);
        }
        let stream = session_state
            .streams
            .entry(packet.header.stream_id)
            .or_default();
        if packet.header.kind != MediaKind::StreamConfig && stream.config.is_none() {
            return Err(MediaHubError::MissingConfig);
        }
        if stream
            .last_sequence
            .is_some_and(|sequence| packet.header.sequence <= sequence)
        {
            return Err(MediaHubError::NonMonotonicSequence);
        }
        stream.last_sequence = Some(packet.header.sequence);
        match packet.header.kind {
            MediaKind::StreamConfig => {
                stream.config = Some(Arc::clone(&packet));
                stream.keyframe = None;
            }
            MediaKind::Video if packet.header.flags.keyframe() => {
                stream.keyframe = Some(Arc::clone(&packet));
            }
            MediaKind::StreamEnd => {
                stream.keyframe = None;
            }
            _ => {}
        }

        let mut stats = PublishStats::default();
        for queue in session_state.clients.values() {
            match queue.push(Arc::clone(&packet)) {
                QueuePush::Sent { evicted } => {
                    stats.sent += 1;
                    stats.dropped += evicted;
                }
                QueuePush::Dropped { count } => stats.dropped += count,
                QueuePush::Closed => {}
            }
        }
        Ok(stats)
    }

    pub fn active_clients(&self, session: &str) -> usize {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.sessions.get(session).map(|state| state.clients.len()))
            .unwrap_or(0)
    }
}

pub struct MediaAttachment {
    client_id: u64,
    session: String,
    queue: Arc<ClientQueue>,
    hub: Weak<Mutex<HubState>>,
}

impl MediaAttachment {
    pub async fn recv(&self) -> Option<Arc<MediaPacket>> {
        self.queue.recv().await
    }

    pub fn submit_input(&self, input: MediaInput) -> Result<(), MediaHubError> {
        input.validate().map_err(MediaHubError::InvalidInput)?;
        let hub = self.hub.upgrade().ok_or(MediaHubError::Unavailable)?;
        let state = hub.lock().map_err(|_| MediaHubError::Unavailable)?;
        let session = state
            .sessions
            .get(&self.session)
            .ok_or_else(|| MediaHubError::UnknownSession(self.session.clone()))?;
        session
            .input
            .try_send(MediaCommand::Input {
                attachment_id: self.client_id,
                input,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => MediaHubError::InputBackpressure,
                mpsc::error::TrySendError::Closed(_) => MediaHubError::Unavailable,
            })
    }
}

impl Drop for MediaAttachment {
    fn drop(&mut self) {
        self.queue.close();
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let Ok(mut state) = hub.lock() else {
            return;
        };
        if let Some(session) = state.sessions.get_mut(&self.session) {
            session.clients.remove(&self.client_id);
            let _ = session.input.try_send(MediaCommand::Disconnected {
                attachment_id: self.client_id,
            });
        }
    }
}

struct ClientQueue {
    state: Mutex<ClientQueueState>,
    notify: Notify,
    capacity: usize,
}

#[derive(Default)]
struct ClientQueueState {
    packets: VecDeque<Arc<MediaPacket>>,
    needs_keyframe: HashSet<u64>,
    closed: bool,
}

enum QueuePush {
    Sent { evicted: usize },
    Dropped { count: usize },
    Closed,
}

impl ClientQueue {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(ClientQueueState::default()),
            notify: Notify::new(),
            capacity,
        }
    }

    fn push(&self, packet: Arc<MediaPacket>) -> QueuePush {
        let mut state = self.state.lock().expect("media queue lock poisoned");
        if state.closed {
            return QueuePush::Closed;
        }
        let is_video = packet.header.kind == MediaKind::Video;
        let is_keyframe = is_video && packet.header.flags.keyframe();
        if is_video && state.needs_keyframe.contains(&packet.header.stream_id) && !is_keyframe {
            return QueuePush::Dropped { count: 1 };
        }
        let mut evicted = 0;
        if state.packets.len() >= self.capacity {
            let previous_len = state.packets.len();
            let mut configs = HashMap::new();
            let mut video_streams = HashSet::new();
            for item in state.packets.iter().rev() {
                if item.header.kind == MediaKind::StreamConfig {
                    configs
                        .entry(item.header.stream_id)
                        .or_insert_with(|| Arc::clone(item));
                }
                if item.header.kind == MediaKind::Video {
                    video_streams.insert(item.header.stream_id);
                }
            }
            state.needs_keyframe.extend(video_streams);
            state.packets.clear();
            for config in configs.into_values().take(self.capacity.saturating_sub(1)) {
                state.packets.push_back(config);
            }
            evicted = previous_len - state.packets.len();
            state.needs_keyframe.insert(packet.header.stream_id);
            if is_video && !is_keyframe {
                return QueuePush::Dropped { count: evicted + 1 };
            }
        }
        if is_keyframe {
            state.needs_keyframe.remove(&packet.header.stream_id);
        }
        state.packets.push_back(packet);
        drop(state);
        self.notify.notify_one();
        QueuePush::Sent { evicted }
    }

    async fn recv(&self) -> Option<Arc<MediaPacket>> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().expect("media queue lock poisoned");
                if let Some(packet) = state.packets.pop_front() {
                    return Some(packet);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        self.notify.notify_waiters();
    }
}

#[derive(Debug, Error)]
pub enum MediaHubError {
    #[error("unknown media session: {0}")]
    UnknownSession(String),
    #[error("invalid media packet: {0}")]
    InvalidPacket(navette_protocol::media::MediaDecodeError),
    #[error("invalid input: {0}")]
    InvalidInput(navette_protocol::media::InputValidationError),
    #[error("input queue is full")]
    InputBackpressure,
    #[error("stream configuration has not been published")]
    MissingConfig,
    #[error("packet stream {actual} does not match active stream {expected}")]
    WrongStream { expected: u64, actual: u64 },
    #[error("media sequence must increase monotonically")]
    NonMonotonicSequence,
    #[error("coded dimensions are out of range")]
    InvalidDimensions,
    #[error("media service is unavailable")]
    Unavailable,
}

#[cfg(test)]
mod tests {
    use navette_protocol::media::{MediaFlags, MediaHeader};

    use super::*;

    fn packet(kind: MediaKind, sequence: u64, keyframe: bool) -> MediaPacket {
        MediaPacket::new(
            MediaHeader {
                kind,
                flags: MediaFlags::new(keyframe, false),
                stream_id: 1,
                sequence,
                timestamp_us: sequence,
                payload_len: 0,
                width: 1280,
                height: 720,
            },
            vec![sequence as u8],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn reconnect_starts_with_config_and_latest_keyframe() {
        let hub = MediaHub::default();
        let _input = hub.register_session("one");
        hub.publish("one", packet(MediaKind::StreamConfig, 1, false))
            .unwrap();
        hub.publish("one", packet(MediaKind::Video, 2, true))
            .unwrap();
        hub.publish("one", packet(MediaKind::Video, 3, false))
            .unwrap();

        let client = hub.attach("one").unwrap();
        assert_eq!(
            client.recv().await.unwrap().header.kind,
            MediaKind::StreamConfig
        );
        let keyframe = client.recv().await.unwrap();
        assert!(keyframe.header.flags.keyframe());
        assert_eq!(keyframe.header.sequence, 2);
    }

    #[tokio::test]
    async fn new_config_invalidates_old_keyframe_and_sequences_are_monotonic() {
        let hub = MediaHub::default();
        let _input = hub.register_session("one");
        hub.publish("one", packet(MediaKind::StreamConfig, 1, false))
            .unwrap();
        hub.publish("one", packet(MediaKind::Video, 2, true))
            .unwrap();
        hub.publish("one", packet(MediaKind::StreamConfig, 3, false))
            .unwrap();
        assert!(matches!(
            hub.publish("one", packet(MediaKind::Video, 3, true)),
            Err(MediaHubError::NonMonotonicSequence)
        ));
        let client = hub.attach("one").unwrap();
        assert_eq!(client.recv().await.unwrap().header.sequence, 3);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), client.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn slow_client_drops_delta_frames_until_a_fresh_keyframe() {
        let hub = MediaHub::with_client_queue_capacity(2);
        let _input = hub.register_session("one");
        let client = hub.attach("one").unwrap();
        hub.publish("one", packet(MediaKind::StreamConfig, 1, false))
            .unwrap();
        hub.publish("one", packet(MediaKind::Video, 2, true))
            .unwrap();
        assert_eq!(
            hub.publish("one", packet(MediaKind::Video, 3, false))
                .unwrap()
                .dropped,
            2
        );
        assert_eq!(
            hub.publish("one", packet(MediaKind::Video, 4, false))
                .unwrap()
                .dropped,
            1
        );
        hub.publish("one", packet(MediaKind::Video, 5, true))
            .unwrap();

        assert_eq!(
            client.recv().await.unwrap().header.kind,
            MediaKind::StreamConfig
        );
        assert_eq!(client.recv().await.unwrap().header.sequence, 5);
    }

    #[tokio::test]
    async fn input_is_scoped_and_disconnect_removes_client() {
        let hub = MediaHub::default();
        let mut one = hub.register_session("one");
        let mut two = hub.register_session("two");
        let client = hub.attach("one").unwrap();
        client.submit_input(MediaInput::RequestKeyframe).unwrap();
        assert_eq!(
            one.recv().await,
            Some(MediaCommand::Input {
                attachment_id: 1,
                input: MediaInput::RequestKeyframe
            })
        );
        assert!(two.try_recv().is_err());
        assert_eq!(hub.active_clients("one"), 1);
        drop(client);
        assert_eq!(
            one.recv().await,
            Some(MediaCommand::Disconnected { attachment_id: 1 })
        );
        assert_eq!(hub.active_clients("one"), 0);
        assert!(matches!(
            hub.attach("missing"),
            Err(MediaHubError::UnknownSession(_))
        ));
    }
}
