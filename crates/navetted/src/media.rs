use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

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

#[derive(Clone, Debug)]
pub enum MediaCommand {
    Input {
        attachment_id: u64,
        input: MediaInput,
        /// When this command was handed to the queue, so the bridge loop can
        /// report how long a keystroke actually waited before being applied.
        /// This is what distinguishes "the loop was slow" from "the loop was
        /// slow while input was waiting"; it found the composite storm and is
        /// kept for the next time this area regresses. Deliberately excluded
        /// from `PartialEq` -- two commands are the same command regardless of
        /// when they were queued, and tests compare them by value.
        queued_at: Instant,
    },
    Disconnected {
        attachment_id: u64,
    },
}

/// Hand-written so the `queued_at` stamp does not take part in
/// equality: two commands carrying the same input are the same command
/// whatever time they were queued, and tests compare them by value.
impl PartialEq for MediaCommand {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Input {
                    attachment_id: left,
                    input: left_input,
                    ..
                },
                Self::Input {
                    attachment_id: right,
                    input: right_input,
                    ..
                },
            ) => left == right && left_input == right_input,
            (
                Self::Disconnected {
                    attachment_id: left,
                },
                Self::Disconnected {
                    attachment_id: right,
                },
            ) => left == right,
            _ => false,
        }
    }
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
        // An ended stream must stop being bootstrapped: `attach` replays every
        // entry in `streams`, so keeping a dead one there would hand each new
        // client a `StreamConfig` for a stream no packet will ever follow --
        // and the viewer eagerly spawns a decoder per replayed config. The
        // removal happens only after the fan-out above so currently attached
        // clients still receive the `StreamEnd` itself and can tear down.
        if packet.header.kind == MediaKind::StreamEnd {
            session_state.streams.remove(&packet.header.stream_id);
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
                queued_at: Instant::now(),
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
        stream_packet(1, kind, sequence, keyframe)
    }

    fn stream_packet(
        stream_id: u64,
        kind: MediaKind,
        sequence: u64,
        keyframe: bool,
    ) -> MediaPacket {
        MediaPacket::new(
            MediaHeader {
                kind,
                flags: MediaFlags::new(keyframe, false),
                stream_id,
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

    /// A stream that has ended must disappear from the bootstrap replay, or
    /// every future attachment is handed a `StreamConfig` for a stream no
    /// packet will ever follow -- which costs the viewer an idle decoder
    /// subprocess per dead stream. The `StreamEnd` packet itself must still
    /// reach the clients attached at the time so they can tear down.
    #[tokio::test]
    async fn stream_end_stops_the_bootstrap_replay_but_still_reaches_attached_clients() {
        let hub = MediaHub::default();
        let _input = hub.register_session("one");
        let attached = hub.attach("one").unwrap();

        hub.publish("one", stream_packet(1, MediaKind::StreamConfig, 1, false))
            .unwrap();
        hub.publish("one", stream_packet(1, MediaKind::Video, 2, true))
            .unwrap();
        // A second stream stays live throughout: only the ended stream may
        // drop out of the replay.
        hub.publish("one", stream_packet(2, MediaKind::StreamConfig, 1, false))
            .unwrap();
        hub.publish("one", stream_packet(2, MediaKind::Video, 2, true))
            .unwrap();
        hub.publish("one", stream_packet(1, MediaKind::StreamEnd, 3, false))
            .unwrap();

        let received = {
            let mut received = Vec::new();
            for _ in 0..5 {
                let packet = attached.recv().await.unwrap();
                received.push((packet.header.stream_id, packet.header.kind));
            }
            received
        };
        assert_eq!(
            received,
            vec![
                (1, MediaKind::StreamConfig),
                (1, MediaKind::Video),
                (2, MediaKind::StreamConfig),
                (2, MediaKind::Video),
                (1, MediaKind::StreamEnd),
            ],
            "an already-attached client must still see the stream's end"
        );

        // A client attaching afterwards is bootstrapped with the live stream
        // only.
        let late = hub.attach("one").unwrap();
        let config = late.recv().await.unwrap();
        assert_eq!(
            (config.header.stream_id, config.header.kind),
            (2, MediaKind::StreamConfig),
            "the ended stream must not be replayed to a new client"
        );
        let keyframe = late.recv().await.unwrap();
        assert_eq!(
            (keyframe.header.stream_id, keyframe.header.kind),
            (2, MediaKind::Video)
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), late.recv())
                .await
                .is_err(),
            "the replay must end with the live stream's keyframe"
        );
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
                input: MediaInput::RequestKeyframe,
                // Ignored by `PartialEq`; any instant will do.
                queued_at: Instant::now()
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
