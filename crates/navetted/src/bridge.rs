use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use navette_bridge::{
    Encoder, EncoderConfig, FfmpegEncoder, Frame, InputState, Scene, SceneEvent, SurfaceKey,
    WprsTransport,
};
use navette_protocol::Session;
use navette_protocol::media::{
    MediaFlags, MediaHeader, MediaInput, MediaKind, MediaPacket, StreamConfig,
};

use crate::media::{MediaCommand, MediaHub};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct BridgeManager {
    media: MediaHub,
    workers: Arc<Mutex<HashMap<String, BridgeHandle>>>,
}

struct BridgeHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl BridgeManager {
    pub fn new(media: MediaHub) -> Self {
        Self {
            media,
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn start(&self, session: &Session) -> Result<()> {
        let mut workers = self
            .workers
            .lock()
            .map_err(|_| anyhow!("bridge manager lock poisoned"))?;
        if workers.contains_key(&session.name) {
            return Ok(());
        }
        let input = self.media.register_session(session.name.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let media = self.media.clone();
        let name = session.name.clone();
        let worker_name = name.clone();
        let socket = PathBuf::from(&session.socket_path);
        let thread = thread::Builder::new()
            .name(format!("navette-bridge-{name}"))
            .spawn(move || {
                if let Err(error) =
                    run_bridge(&worker_name, socket, media.clone(), input, worker_stop)
                {
                    tracing::error!(session = %worker_name, %error, "session bridge stopped");
                }
                media.unregister_session(&worker_name);
            })
            .context("failed to spawn bridge thread")?;
        workers.insert(
            name,
            BridgeHandle {
                stop,
                thread: Some(thread),
            },
        );
        Ok(())
    }

    pub fn stop(&self, session: &str) {
        let handle = self
            .workers
            .lock()
            .ok()
            .and_then(|mut workers| workers.remove(session));
        if let Some(mut handle) = handle {
            handle.stop.store(true, Ordering::Release);
            if let Some(thread) = handle.thread.take() {
                let _ = thread.join();
            }
        }
        self.media.unregister_session(session);
    }

    pub fn is_running(&self, session: &str) -> bool {
        self.workers
            .lock()
            .is_ok_and(|workers| workers.contains_key(session))
    }
}

impl Drop for BridgeManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.workers) != 1 {
            return;
        }
        let names = self
            .workers
            .lock()
            .map(|workers| workers.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for name in names {
            self.stop(&name);
        }
    }
}

struct StreamState {
    id: u64,
    sequence: u64,
    encoder: FfmpegEncoder,
    force_keyframe: bool,
    discontinuity: bool,
    started: Instant,
}

struct WorkerState {
    scene: Scene,
    streams: HashMap<SurfaceKey, StreamState>,
    input: InputState,
}

fn run_bridge(
    session: &str,
    socket: PathBuf,
    media: MediaHub,
    mut commands: tokio::sync::mpsc::Receiver<MediaCommand>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut transport = WprsTransport::connect(&socket)?;
    let receiver = transport
        .take_receiver()
        .context("wprs receiver was already taken")?;
    let mut event_loop: EventLoop<
        Vec<calloop::channel::Event<wprs::serialization::RecvType<wprs::serialization::Request>>>,
    > = EventLoop::try_new()?;
    event_loop
        .handle()
        .insert_source(receiver, |event, _, pending| pending.push(event))
        .map_err(|_| anyhow!("failed to register wprs source"))?;
    let mut pending = Vec::new();
    let mut worker = WorkerState {
        scene: Scene::default(),
        streams: HashMap::new(),
        input: InputState::default(),
    };
    let mut resize: Option<(Instant, u32, u32)> = None;

    while !stop.load(Ordering::Acquire) && transport.is_connected() {
        event_loop.dispatch(Some(Duration::from_millis(10)), &mut pending)?;
        for event in pending.drain(..) {
            if let ChannelEvent::Msg(message) = event {
                match worker.scene.apply(message) {
                    Ok(events) => handle_scene_events(session, &media, &mut worker, events),
                    Err(error) => tracing::warn!(%error, "rejected wprs scene message"),
                }
            }
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                MediaCommand::Input {
                    attachment_id,
                    input: MediaInput::ViewportResize { width, height },
                } => {
                    let _ = attachment_id;
                    resize = Some((Instant::now(), width, height));
                }
                MediaCommand::Input {
                    attachment_id: _,
                    input: MediaInput::RequestKeyframe,
                } => {
                    for stream in worker.streams.values_mut() {
                        stream.force_keyframe = true;
                    }
                }
                MediaCommand::Input {
                    attachment_id,
                    input,
                } => {
                    if let Err(error) =
                        worker
                            .input
                            .apply(attachment_id, input, &worker.scene, &transport)
                    {
                        tracing::warn!(%error, "rejected scoped media input");
                    }
                }
                MediaCommand::Disconnected { attachment_id } => {
                    worker.input.disconnect(attachment_id, &transport)
                }
            }
        }
        if let Some((requested, width, height)) = resize
            && requested.elapsed() >= RESIZE_DEBOUNCE
        {
            worker
                .input
                .apply(
                    0,
                    MediaInput::ViewportResize { width, height },
                    &worker.scene,
                    &transport,
                )
                .ok();
            for stream in worker.streams.values_mut() {
                stream.force_keyframe = true;
                stream.discontinuity = true;
            }
            resize = None;
        }
    }
    Ok(())
}

fn handle_scene_events(
    session: &str,
    media: &MediaHub,
    worker: &mut WorkerState,
    events: Vec<SceneEvent>,
) {
    for event in events {
        match event {
            SceneEvent::SurfaceCommitted(_) => {
                for key in worker.scene.toplevels() {
                    let Ok(frame) = worker.scene.compose_toplevel(key) else {
                        continue;
                    };
                    if let Err(error) = encode_frame(
                        session,
                        media,
                        &mut worker.streams,
                        key,
                        normalize_frame(frame),
                    ) {
                        tracing::warn!(?key, %error, "failed to encode captured frame");
                    }
                }
            }
            SceneEvent::SurfaceDestroyed(key) => {
                end_stream(session, media, &mut worker.streams, key)
            }
            SceneEvent::ClientDisconnected(client_id) => {
                let keys = worker
                    .streams
                    .keys()
                    .filter(|key| key.client_id == client_id)
                    .copied()
                    .collect::<Vec<_>>();
                for key in keys {
                    end_stream(session, media, &mut worker.streams, key);
                }
            }
            SceneEvent::CursorChanged | SceneEvent::CapabilitiesChanged => {}
        }
    }
}

fn encode_frame(
    session: &str,
    media: &MediaHub,
    streams: &mut HashMap<SurfaceKey, StreamState>,
    key: SurfaceKey,
    frame: Frame,
) -> Result<()> {
    let stream = match streams.entry(key) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let config = EncoderConfig {
                width: frame.width,
                height: frame.height,
                ..EncoderConfig::default()
            };
            entry.insert(StreamState {
                id: NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed),
                sequence: 0,
                encoder: FfmpegEncoder::new("ffmpeg", config)?,
                force_keyframe: true,
                discontinuity: true,
                started: Instant::now(),
            })
        }
    };
    if stream.encoder.config().width != frame.width
        || stream.encoder.config().height != frame.height
    {
        stream.encoder.reconfigure(EncoderConfig {
            width: frame.width,
            height: frame.height,
            ..stream.encoder.config()
        })?;
        stream.force_keyframe = true;
        stream.discontinuity = true;
    }
    let encoded = stream.encoder.encode(&frame, stream.force_keyframe)?;
    stream.force_keyframe = false;
    if let Some(codec_config) = encoded.codec_config {
        stream.sequence = stream.sequence.saturating_add(1);
        let payload = StreamConfig {
            client_id: key.client_id,
            surface_id: key.surface_id,
            codec_config,
        }
        .encode()?;
        media.publish(
            session,
            packet(stream, MediaKind::StreamConfig, false, payload)?,
        )?;
    }
    stream.sequence = stream.sequence.saturating_add(1);
    media.publish(
        session,
        packet(stream, MediaKind::Video, encoded.keyframe, encoded.annex_b)?,
    )?;
    stream.discontinuity = false;
    Ok(())
}

fn end_stream(
    session: &str,
    media: &MediaHub,
    streams: &mut HashMap<SurfaceKey, StreamState>,
    key: SurfaceKey,
) {
    if let Some(mut stream) = streams.remove(&key) {
        stream.sequence = stream.sequence.saturating_add(1);
        if let Ok(packet) = packet(&stream, MediaKind::StreamEnd, false, Vec::new()) {
            let _ = media.publish(session, packet);
        }
    }
}

fn packet(
    stream: &StreamState,
    kind: MediaKind,
    keyframe: bool,
    payload: Vec<u8>,
) -> Result<MediaPacket> {
    Ok(MediaPacket::new(
        MediaHeader {
            kind,
            flags: MediaFlags::new(keyframe, stream.discontinuity),
            stream_id: stream.id,
            sequence: stream.sequence,
            timestamp_us: u64::try_from(stream.started.elapsed().as_micros()).unwrap_or(u64::MAX),
            payload_len: 0,
            width: stream.encoder.config().width,
            height: stream.encoder.config().height,
        },
        payload,
    )?)
}

fn normalize_frame(frame: Frame) -> Frame {
    let width = frame.width.saturating_add(frame.width % 2);
    let height = frame.height.saturating_add(frame.height % 2);
    if width == frame.width && height == frame.height {
        return frame;
    }
    let mut pixels = vec![0; width as usize * height as usize * 4];
    for row in 0..frame.height as usize {
        let source = row * frame.width as usize * 4;
        let target = row * width as usize * 4;
        pixels[target..target + frame.width as usize * 4]
            .copy_from_slice(&frame.pixels[source..source + frame.width as usize * 4]);
    }
    Frame {
        width,
        height,
        pixels,
    }
}
