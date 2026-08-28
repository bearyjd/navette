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

impl BridgeHandle {
    /// Whether the worker thread has exited, whether cleanly (via `stop`)
    /// or on its own (a connection failure inside `run_bridge`).
    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
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
        // A worker thread that exited on its own (wprs socket gone, `run_bridge`
        // returned an error) leaves a stale entry behind: reap it before the
        // `contains_key` check below, or a dead session could never restart.
        workers.retain(|_, handle| !handle.is_finished());
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
        self.workers.lock().is_ok_and(|workers| {
            workers
                .get(session)
                .is_some_and(|handle| !handle.is_finished())
        })
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
            tracing::debug!(width, height, "applying debounced viewport resize");
            worker
                .input
                .apply(
                    0,
                    MediaInput::ViewportResize { width, height },
                    &worker.scene,
                    &transport,
                )
                .ok();
            force_keyframe_on_all_streams(&mut worker.streams);
            resize = None;
        }
    }
    // A client that stays connected across a reconnect never sends
    // `Disconnected`, so whatever it last pressed would otherwise read as
    // held forever on the guest once this worker exits and a fresh one
    // starts with empty tracking. Best-effort: if the transport already
    // failed outright, these sends land nowhere, but that's no worse than
    // the silent loss this replaces, and a `stop`-triggered exit (the
    // transport still healthy) recovers cleanly.
    worker.input.release_all_held(&transport);
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
            SceneEvent::SurfaceCommitted(key) => {
                // Only the committed surface's own window is re-encoded. A
                // subsurface commit still recomposites its toplevel ancestor
                // (its pixels land in that frame), but unrelated toplevels are
                // untouched: encoding all of them on every commit would stream
                // every open window at the busiest window's commit rate.
                let Some(toplevel) = worker.scene.toplevel_ancestor(key) else {
                    tracing::trace!(?key, "commit belongs to no toplevel; nothing to encode");
                    continue;
                };
                let Ok(frame) = worker.scene.compose_toplevel(toplevel) else {
                    continue;
                };
                if let Err(error) = encode_frame(
                    session,
                    media,
                    &mut worker.streams,
                    toplevel,
                    normalize_frame(frame),
                ) {
                    tracing::warn!(?toplevel, %error, "failed to encode captured frame");
                }
            }
            SceneEvent::SurfaceDestroyed(key) => {
                worker.input.surface_destroyed(key);
                end_stream(session, media, &mut worker.streams, key)
            }
            SceneEvent::ClientDisconnected(client_id) => {
                worker.input.client_disconnected(client_id);
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

/// Forces the next frame on every live stream to be a keyframe and marks it
/// as a discontinuity, e.g. after a viewport resize invalidates in-flight
/// encoder state for every stream.
fn force_keyframe_on_all_streams(streams: &mut HashMap<SurfaceKey, StreamState>) {
    for stream in streams.values_mut() {
        stream.force_keyframe = true;
        stream.discontinuity = true;
    }
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

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    use navette_protocol::SessionStatus;
    use wprs::serialization::geometry::Point;
    use wprs::serialization::wayland::{
        Buffer, BufferAssignment, BufferData, BufferFormat, BufferMetadata, Role, SubSurfaceState,
        SubsurfacePosition, SurfaceRequest, SurfaceRequestPayload, SurfaceState, WlSurfaceId,
    };
    use wprs::serialization::xdg_shell::{XdgToplevelId, XdgToplevelState};
    use wprs::serialization::{ClientId, RecvType, Request};

    use super::*;
    use crate::media::MediaAttachment;

    fn session_fixture(name: &str, socket_path: impl Into<String>) -> Session {
        Session {
            name: name.into(),
            app_id: "firefox".into(),
            app_pid: 10,
            daemon_pid: 11,
            wayland_display: format!("navette-{name}"),
            socket_path: socket_path.into(),
            created_at_ms: 100,
            last_attached_at_ms: None,
            client_count: 0,
            status: SessionStatus::Running,
        }
    }

    fn surface_state(client: u64, surface: u64) -> SurfaceState {
        SurfaceState {
            client: ClientId(client),
            id: WlSurfaceId(surface),
            buffer: None,
            role: Some(Role::XdgToplevel(XdgToplevelState {
                id: XdgToplevelId(1),
                parent: None,
                title: None,
                app_id: None,
                decoration_mode: None,
                maximized: None,
                fullscreen: None,
            })),
            buffer_scale: 1,
            buffer_transform: None,
            opaque_region: None,
            input_region: None,
            z_ordered_children: Vec::new(),
            damage: None,
            output_ids: Vec::new(),
            viewport_state: None,
            xdg_surface_state: None,
        }
    }

    fn external_buffer(width: i32, height: i32, format: BufferFormat) -> BufferAssignment {
        BufferAssignment::New(Buffer {
            metadata: BufferMetadata {
                width,
                height,
                stride: width * 4,
                format,
            },
            data: BufferData::External,
        })
    }

    fn commit(state: SurfaceState) -> RecvType<Request> {
        RecvType::Object(Request::Surface(SurfaceRequest {
            client: state.client,
            surface: state.id,
            payload: SurfaceRequestPayload::Commit(state),
        }))
    }

    /// Builds a scene with one real, committed 64x64 toplevel per
    /// `(client_id, surface_id)` pair. 64x64 is both a realistic size for
    /// the real `ffmpeg` encode these tests drive and a pixel count that
    /// avoids an alignment bug in the vendored `wprs` pixel filter's SIMD
    /// path (it mishandles buffers whose pixel count is >=32 and not a
    /// multiple of 16; 64x64 = 4096 pixels is safely a multiple of 16).
    fn scene_with_toplevels(surfaces: &[(u64, u64)]) -> Scene {
        let mut scene = Scene::default();
        for &(client, surface) in surfaces {
            scene
                .apply(RecvType::RawBuffer(vec![0; 64 * 64 * 4]))
                .unwrap();
            let mut state = surface_state(client, surface);
            state.buffer = Some(external_buffer(64, 64, BufferFormat::Xrgb8888));
            scene.apply(commit(state)).unwrap();
        }
        scene
    }

    /// A 64x64 toplevel that lists a 4x4 subsurface child, plus that child.
    /// Both pixel counts (4096 and 16) stay clear of the vendored `wprs`
    /// filter's alignment bug described on `scene_with_toplevels`.
    fn scene_with_toplevel_and_subsurface(client: u64, parent: u64, child: u64) -> Scene {
        let mut scene = Scene::default();
        scene
            .apply(RecvType::RawBuffer(vec![0; 64 * 64 * 4]))
            .unwrap();
        let mut parent_state = surface_state(client, parent);
        parent_state.buffer = Some(external_buffer(64, 64, BufferFormat::Xrgb8888));
        parent_state.z_ordered_children.push(SubsurfacePosition {
            id: WlSurfaceId(child),
            position: Point { x: 0, y: 0 },
        });
        scene.apply(commit(parent_state)).unwrap();

        scene
            .apply(RecvType::RawBuffer(vec![0; 4 * 4 * 4]))
            .unwrap();
        let mut child_state = surface_state(client, child);
        child_state.role = Some(Role::SubSurface(SubSurfaceState {
            parent: WlSurfaceId(parent),
            location: Point { x: 0, y: 0 },
            sync: true,
        }));
        child_state.buffer = Some(external_buffer(4, 4, BufferFormat::Argb8888));
        scene.apply(commit(child_state)).unwrap();
        scene
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    async fn recv_packet(client: &MediaAttachment) -> std::sync::Arc<MediaPacket> {
        tokio::time::timeout(Duration::from_secs(10), client.recv())
            .await
            .expect("a packet should have been published before the timeout")
            .expect("media channel closed unexpectedly")
    }

    #[test]
    fn normalize_frame_pads_odd_dimensions_and_preserves_pixel_rows() {
        // 3x3 source: three rows, each with a distinguishable byte value, so
        // a bug that uses the wrong row stride for source or target (or
        // swaps them) lands each row's pixels at the wrong offset instead of
        // silently reproducing the same output.
        let mut pixels = Vec::with_capacity(3 * 3 * 4);
        pixels.extend(std::iter::repeat_n(0x11, 3 * 4)); // row 0
        pixels.extend(std::iter::repeat_n(0x22, 3 * 4)); // row 1
        pixels.extend(std::iter::repeat_n(0x33, 3 * 4)); // row 2
        let frame = Frame {
            width: 3,
            height: 3,
            pixels,
        };

        let normalized = normalize_frame(frame);

        assert_eq!(normalized.width, 4);
        assert_eq!(normalized.height, 4);
        assert_eq!(normalized.pixels.len(), 4 * 4 * 4);

        // Source stride is 3 pixels/row (12 bytes), target stride is the
        // padded 4 pixels/row (16 bytes) -- row 1 must land at target byte
        // offset 16, not 12, and row 2 at 32, not 24.
        assert_eq!(&normalized.pixels[0..12], &[0x11; 12], "row 0 pixels");
        assert_eq!(&normalized.pixels[12..16], &[0; 4], "row 0 padding column");
        assert_eq!(&normalized.pixels[16..28], &[0x22; 12], "row 1 pixels");
        assert_eq!(&normalized.pixels[28..32], &[0; 4], "row 1 padding column");
        assert_eq!(&normalized.pixels[32..44], &[0x33; 12], "row 2 pixels");
        assert_eq!(&normalized.pixels[44..48], &[0; 4], "row 2 padding column");
        // The entire padded row 3 is zero-fill.
        assert_eq!(&normalized.pixels[48..64], &[0; 16], "padding row");
    }

    #[test]
    fn normalize_frame_leaves_even_dimensions_unchanged() {
        let frame = Frame {
            width: 4,
            height: 2,
            pixels: vec![7; 4 * 2 * 4],
        };

        let normalized = normalize_frame(frame.clone());

        assert_eq!(normalized, frame);
    }

    #[test]
    fn start_reaps_a_dead_worker_and_restarts_the_session() {
        let media = MediaHub::default();
        let manager = BridgeManager::new(media.clone());
        let dir = tempfile::tempdir().unwrap();

        let dead_socket = dir.path().join("no-such-socket");
        let dead_session = session_fixture("dead", dead_socket.to_string_lossy().into_owned());
        manager.start(&dead_session).unwrap();
        // We deliberately don't assert `media.attach` succeeds here.
        // `register_session` does run synchronously inside `start`, before the
        // worker thread spawns, so the session *is* registered by the time
        // `start` returns -- but connecting to a socket nobody is listening on
        // fails immediately, so the spawned worker can reach its own
        // `unregister_session` call just as fast, racing this thread's next
        // statement. That race is real, not hypothetical: it passed
        // consistently on the developer machine this was written on but
        // failed on CI's runner. The test's actual claim -- that a dead
        // worker's session is restartable -- doesn't need this intermediate
        // assertion; the wait loop and the post-restart assertion below are
        // enough, and both are synchronized on the same predicate `start`
        // itself uses.

        // Connecting to a socket nobody is listening on fails immediately, so
        // the worker thread exits almost at once. `start()`'s reap decision
        // is keyed on `JoinHandle::is_finished()`, which only flips after the
        // spawned closure's last statement (`unregister_session`) returns --
        // so the test must wait on that same predicate via
        // `BridgeManager::is_running`, not on a looser proxy signal like
        // `media.attach` succeeding/failing, which can flip earlier.
        let deadline = Instant::now() + Duration::from_secs(5);
        while manager.is_running(&dead_session.name) {
            assert!(
                Instant::now() < deadline,
                "worker never exited and reaped itself"
            );
            thread::sleep(Duration::from_millis(5));
        }

        // A live listener keeps the restarted worker's connection open so the
        // second `start()` doesn't race its own worker's exit.
        let live_socket = dir.path().join("live-socket");
        let _listener = UnixListener::bind(&live_socket).unwrap();
        let live_session = session_fixture("dead", live_socket.to_string_lossy().into_owned());
        manager.start(&live_session).unwrap();

        assert!(
            media.attach(&live_session.name).is_ok(),
            "a dead worker's session should be restartable, not silently no-op"
        );
        manager.stop("dead");
    }

    #[tokio::test]
    async fn scene_commit_creates_independent_streams_with_config_before_video() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        let scene = scene_with_toplevels(&[(1, 1), (1, 2)]);
        let mut worker = WorkerState {
            scene,
            streams: HashMap::new(),
            input: InputState::default(),
        };
        let key1 = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let key2 = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        // Each toplevel is encoded off its own commit, so both must commit
        // for both streams to exist.
        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![
                SceneEvent::SurfaceCommitted(key1),
                SceneEvent::SurfaceCommitted(key2),
            ],
        );

        assert_eq!(worker.streams.len(), 2);
        let stream1_id = worker.streams[&key1].id;
        let stream2_id = worker.streams[&key2].id;
        assert_ne!(stream1_id, stream2_id, "each toplevel gets its own stream");
        assert_eq!(worker.streams[&key1].sequence, 2);
        assert_eq!(worker.streams[&key2].sequence, 2);

        let mut by_stream: HashMap<u64, Vec<(MediaKind, u64, bool)>> = HashMap::new();
        for _ in 0..4 {
            let packet = recv_packet(&client).await;
            by_stream.entry(packet.header.stream_id).or_default().push((
                packet.header.kind,
                packet.header.sequence,
                packet.header.flags.discontinuity(),
            ));
        }
        for stream_id in [stream1_id, stream2_id] {
            let packets = by_stream
                .remove(&stream_id)
                .expect("each stream published packets");
            assert_eq!(packets.len(), 2);
            assert_eq!(packets[0].0, MediaKind::StreamConfig);
            assert_eq!(packets[1].0, MediaKind::Video);
            assert!(
                packets[0].1 < packets[1].1,
                "sequence must ascend within a stream"
            );
            assert!(packets[0].2, "a stream's first frame is a discontinuity");
            assert!(packets[1].2, "a stream's first frame is a discontinuity");
        }

        // Publishing another frame to just one stream must not disturb the
        // other's sequence numbering.
        let frame = normalize_frame(worker.scene.compose_toplevel(key1).unwrap());
        encode_frame("s1", &media, &mut worker.streams, key1, frame).unwrap();
        assert!(worker.streams[&key1].sequence > 2);
        assert_eq!(
            worker.streams[&key2].sequence, 2,
            "publishing to one stream must not disturb the other's sequencing"
        );

        // The resize path forces every live stream to re-key on its next frame.
        force_keyframe_on_all_streams(&mut worker.streams);
        assert!(worker.streams[&key1].force_keyframe);
        assert!(worker.streams[&key1].discontinuity);
        assert!(worker.streams[&key2].force_keyframe);
        assert!(worker.streams[&key2].discontinuity);
    }

    /// A commit belongs to one window. Re-encoding every open toplevel on
    /// every commit makes each window stream at the busiest window's commit
    /// rate, burning encode time and bandwidth on frames nothing changed in.
    #[tokio::test]
    async fn commit_encodes_only_the_committed_surfaces_own_toplevel() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        let scene = scene_with_toplevels(&[(1, 1), (1, 2)]);
        let mut worker = WorkerState {
            scene,
            streams: HashMap::new(),
            input: InputState::default(),
        };
        let key1 = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let key2 = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![SceneEvent::SurfaceCommitted(key1)],
        );

        assert_eq!(
            worker.streams.len(),
            1,
            "only the committed toplevel may be encoded"
        );
        let stream1 = worker.streams[&key1].id;
        for expected in [MediaKind::StreamConfig, MediaKind::Video] {
            let packet = recv_packet(&client).await;
            assert_eq!(packet.header.stream_id, stream1);
            assert_eq!(packet.header.kind, expected);
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.recv())
                .await
                .is_err(),
            "an untouched toplevel must publish nothing on another window's commit"
        );

        // The other window starts streaming on its own commit, and repeated
        // commits to it leave the first window's stream alone.
        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![SceneEvent::SurfaceCommitted(key2)],
        );
        assert_eq!(worker.streams.len(), 2);
        let sequence1 = worker.streams[&key1].sequence;
        for expected in [MediaKind::StreamConfig, MediaKind::Video] {
            let packet = recv_packet(&client).await;
            assert_eq!(packet.header.stream_id, worker.streams[&key2].id);
            assert_eq!(packet.header.kind, expected);
        }

        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![SceneEvent::SurfaceCommitted(key2)],
        );
        assert_eq!(
            worker.streams[&key1].sequence, sequence1,
            "a commit to one window must not advance another window's stream"
        );
        let packet = recv_packet(&client).await;
        assert_eq!(packet.header.stream_id, worker.streams[&key2].id);
    }

    /// `Scene::compose_toplevel` draws subsurfaces into their toplevel's
    /// frame, so a subsurface commit has to recomposite that toplevel rather
    /// than be dropped for not being a toplevel itself.
    #[tokio::test]
    async fn subsurface_commit_reencodes_its_toplevel_ancestor() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        let scene = scene_with_toplevel_and_subsurface(1, 1, 2);
        let mut worker = WorkerState {
            scene,
            streams: HashMap::new(),
            input: InputState::default(),
        };
        let toplevel = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let child = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![SceneEvent::SurfaceCommitted(child)],
        );

        assert!(
            worker.streams.contains_key(&toplevel),
            "a subsurface commit must re-encode its toplevel ancestor"
        );
        assert!(
            !worker.streams.contains_key(&child),
            "a subsurface has no stream of its own"
        );
        assert_eq!(worker.streams.len(), 1);
        let stream = worker.streams[&toplevel].id;
        for expected in [MediaKind::StreamConfig, MediaKind::Video] {
            let packet = recv_packet(&client).await;
            assert_eq!(packet.header.stream_id, stream);
            assert_eq!(packet.header.kind, expected);
        }

        let sequence = worker.streams[&toplevel].sequence;
        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![SceneEvent::SurfaceCommitted(child)],
        );
        assert_eq!(worker.streams[&toplevel].id, stream);
        assert!(
            worker.streams[&toplevel].sequence > sequence,
            "each subsurface commit publishes another frame on the same stream"
        );
        assert_eq!(recv_packet(&client).await.header.stream_id, stream);
    }

    #[tokio::test]
    async fn destroy_and_disconnect_end_only_the_affected_stream() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        // Two different clients, so `ClientDisconnected` for one must not
        // touch the other's stream.
        let scene = scene_with_toplevels(&[(1, 1), (2, 1)]);
        let mut worker = WorkerState {
            scene,
            streams: HashMap::new(),
            input: InputState::default(),
        };
        let key_a = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let key_b = SurfaceKey {
            client_id: 2,
            surface_id: 1,
        };

        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![
                SceneEvent::SurfaceCommitted(key_a),
                SceneEvent::SurfaceCommitted(key_b),
            ],
        );
        assert_eq!(worker.streams.len(), 2);
        for _ in 0..4 {
            recv_packet(&client).await; // drain the initial config+video pairs
        }

        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![SceneEvent::ClientDisconnected(1)],
        );
        assert!(!worker.streams.contains_key(&key_a));
        assert!(worker.streams.contains_key(&key_b));
        assert_eq!(
            worker.streams[&key_b].sequence, 2,
            "client B's stream must be untouched by client A's disconnect"
        );
        let end_a = recv_packet(&client).await;
        assert_eq!(end_a.header.kind, MediaKind::StreamEnd);

        handle_scene_events(
            "s1",
            &media,
            &mut worker,
            vec![SceneEvent::SurfaceDestroyed(key_b)],
        );
        assert!(worker.streams.is_empty());
        let end_b = recv_packet(&client).await;
        assert_eq!(end_b.header.kind, MediaKind::StreamEnd);
        assert_ne!(end_a.header.stream_id, end_b.header.stream_id);
    }
}
