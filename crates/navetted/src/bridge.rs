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
    MediaFlags, MediaHeader, MediaInput, MediaKind, MediaPacket, MediaServerMessage, StreamConfig,
};
use wprs::serialization::wayland::{
    DataDestinationEvent, DataDestinationRequest, DataEvent, DataRequest, DataSource,
    DataSourceEvent, DataSourceRequest, DataToTransfer, SourceMetadata,
};
use wprs::serialization::{Event, RecvType, Request};

use crate::blobs::BlobStore;
use crate::clipboard::{ClipboardSync, GuestEvent, SyncAction};
use crate::media::{MediaCommand, MediaHub};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);
/// Report an iteration, or a waiting keystroke, at or above this. 20ms is
/// above the loop's 10ms dispatch floor and well below wprsd's 200ms key
/// repeat delay, which is the threshold that actually matters: a release
/// later than that makes the guest repeat the key.
const LOOP_LAG_THRESHOLD_US: u128 = 20_000;
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct BridgeManager {
    media: MediaHub,
    blobs: BlobStore,
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
    pub fn new(media: MediaHub, blobs: BlobStore) -> Self {
        Self {
            media,
            blobs,
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
        let blobs = self.blobs.clone();
        let name = session.name.clone();
        let worker_name = name.clone();
        let socket = PathBuf::from(&session.socket_path);
        let thread = thread::Builder::new()
            .name(format!("navette-bridge-{name}"))
            .spawn(move || {
                if let Err(error) = run_bridge(
                    &worker_name,
                    socket,
                    media.clone(),
                    blobs,
                    input,
                    worker_stop,
                ) {
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
    input: InputState,
    /// Composited frames go here rather than being encoded inline. See
    /// `EncodeQueue` for why.
    encode: Arc<EncodeQueue>,
    /// Toplevels that need recompositing before this iteration ends.
    ///
    /// A commit does not composite immediately. wprsd delivers a burst of
    /// commits per iteration during a resize -- measured at 10 for one window,
    /// each composite ~40ms -- and the encode queue then coalesces them to a
    /// single frame per surface, so all but the last composite is work whose
    /// result is discarded. Meanwhile the loop drains input only after the
    /// batch, so a keystroke waits behind every one of them (measured: up to
    /// 528ms). Collecting here and compositing once per surface at the end of
    /// the batch removes the discarded work rather than relocating it.
    ///
    /// Note the flush order is by `SurfaceKey`, not by arrival: coalescing
    /// means a surface has no single arrival time anyway. Nothing depends on
    /// the order today -- each surface owns a separate stream, so frames for
    /// different surfaces are independent -- but a future requirement to
    /// composite in arrival order needs a different container, not a tweak.
    pending_composites: std::collections::BTreeSet<SurfaceKey>,
    /// Clipboard state for this session. Plain state, not behind a lock:
    /// calloop is single-threaded, and both the guest's requests and the
    /// phone's input are applied on it.
    clipboard: ClipboardSync,
}

/// Where one session's work goes out: the wprs link for the guest side, the
/// media hub for the phone side, and the encode queue. Bundled because the
/// clipboard needs all three at once and `pump_input` is already at
/// clippy's argument limit.
struct SessionIo<'a> {
    transport: &'a WprsTransport,
    media: &'a MediaHub,
    blobs: &'a BlobStore,
    encode: &'a EncodeQueue,
    session: &'a str,
}

fn run_bridge(
    session: &str,
    socket: PathBuf,
    media: MediaHub,
    blobs: BlobStore,
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
    let encode = Arc::new(EncodeQueue::new());
    let encoder_thread = {
        let queue = Arc::clone(&encode);
        let session = session.to_owned();
        let media = media.clone();
        thread::Builder::new()
            .name("navette-encode".to_owned())
            .spawn(move || encode_loop(session, media, queue))
            .context("failed to start the encode thread")?
    };
    let mut worker = WorkerState {
        scene: Scene::default(),
        input: InputState::default(),
        encode: Arc::clone(&encode),
        pending_composites: std::collections::BTreeSet::new(),
        clipboard: ClipboardSync::new(),
    };
    let io = SessionIo {
        transport: &transport,
        media: &media,
        blobs: &blobs,
        encode: &encode,
        session,
    };
    let mut resize: Option<(Instant, u32, u32)> = None;

    // The loop is wrapped so the flush below runs on *every* exit. The
    // `?` on `dispatch` used to return straight out of this function, and
    // that -- a worker dying while navetted survives, so a later `start`
    // can bring a fresh one up against a live wprsd -- is precisely the
    // case the flush exists for. It was reachable only on the two paths
    // where flushing achieves nothing.
    let outcome = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) && transport.is_connected() {
            // Phase-split iteration timing. `apply` and `compose` are timed
            // separately because they are different costs with different
            // fixes, and `worst_input_wait_us` is tracked separately from both
            // because it is the one that decides whether the guest repeats a
            // key: an iteration may legitimately run long, so long as nothing
            // was waiting on it. Input is pumped between every unit of work
            // below, so the wait is bounded by one message or one composite
            // rather than by the batch.
            let iteration_start = Instant::now();
            event_loop.dispatch(Some(Duration::from_millis(10)), &mut pending)?;
            let dispatch_us = iteration_start.elapsed().as_micros();
            let mut apply_us = 0u128;
            let mut compose_us = 0u128;
            let mut messages = 0u32;
            let mut composites = 0u32;
            let mut stats = InputStats::default();
            // One closure shared by every call site below, so the seven-argument
            // `pump_input` call is written once instead of three times drifting
            // independently.
            let mut pump =
                |input: &mut InputState, scene: &Scene, clipboard: &mut ClipboardSync| {
                    pump_input(
                        &mut commands,
                        input,
                        scene,
                        &io,
                        &mut resize,
                        &mut stats,
                        clipboard,
                    );
                };
            let scene_start = Instant::now();
            let batch = pending.drain(..).filter_map(|event| match event {
                ChannelEvent::Msg(message) => Some(message),
                _ => None,
            });
            let batch = take_clipboard_requests(&mut worker.clipboard, batch, &io);
            let applied = apply_scene_messages(&mut worker, batch, &mut pump);
            messages += applied.0;
            apply_us += applied.1;
            // One composite per surface for the whole batch, not one per commit.
            let compose_start = Instant::now();
            composites += flush_composites(&mut worker, &mut pump);
            compose_us += compose_start.elapsed().as_micros();
            let scene_us = scene_start.elapsed().as_micros();
            // A final pump catches anything that arrived after the last unit of
            // work above.
            pump(&mut worker.input, &worker.scene, &mut worker.clipboard);
            let InputStats {
                inputs,
                worst_wait_us: worst_input_wait_us,
                busy_us: input_us,
            } = stats;
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
                worker.encode.submit(EncodeCommand::ForceKeyframeAll);
                resize = None;
            }
            // Report only long iterations, plus every iteration in
            // which a keystroke actually waited. A slow iteration with no
            // input pending costs nothing, so the two are logged together to
            // tell "the loop was slow" from "the loop was slow while input
            // was waiting" -- which is the whole question.
            let total_us = iteration_start.elapsed().as_micros();
            if total_us >= LOOP_LAG_THRESHOLD_US || worst_input_wait_us >= LOOP_LAG_THRESHOLD_US {
                tracing::debug!(
                    total_us,
                    dispatch_us,
                    scene_us,
                    apply_us,
                    compose_us,
                    input_us,
                    messages,
                    composites,
                    inputs,
                    worst_input_wait_us,
                    "bridge loop iteration ran long"
                );
            }
        }
        Ok(())
    })();
    // A client that stays connected across a reconnect never sends
    // `Disconnected`, so whatever it last pressed would otherwise read as
    // held forever on the guest once this worker exits and a fresh one
    // starts with empty tracking. Best-effort: if the transport already
    // failed outright, these sends land nowhere, but that's no worse than
    // the silent loss this replaces, and both a `stop`-triggered exit and a
    // dispatch error (the transport possibly still healthy) recover cleanly.
    //
    // Note this deliberately desyncs the bridge from a viewer that is still
    // running: the viewer keeps its own `held_keys`, so its next real release
    // arrives for a keycode this side no longer tracks and shows up on the
    // untracked-release path. That is expected here, not an anomaly.
    worker.input.release_all_held(&transport);
    // Stop the encode thread and wait for it. Joining can take as long as one
    // in-flight encode -- an FFmpeg spawn in the worst case -- but this runs
    // as the session's own worker is unwinding, and letting the thread outlive
    // it would leave FFmpeg processes owned by nobody.
    encode.stop();
    if encoder_thread.join().is_err() {
        tracing::warn!("encode thread panicked");
    }
    outcome
}

/// Applies one batch of scene events, recording which toplevels need
/// recompositing. Call `flush_composites` once the batch is complete.
fn handle_scene_events(worker: &mut WorkerState, events: Vec<SceneEvent>) {
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
                // Deferred to `flush_composites`, so repeated commits to one
                // window in a single batch composite once, not once each.
                worker.pending_composites.insert(toplevel);
            }
            SceneEvent::SurfaceDestroyed(key) => {
                worker.input.surface_destroyed(key);
                // Drop any composite still owed to this surface: submitting it
                // after the `EndStream` below would open a fresh stream for a
                // window that is gone. A destroyed *subsurface* is not in this
                // set (it holds toplevels), so its ancestor stays pending and
                // still recomposites, which is what should happen.
                worker.pending_composites.remove(&key);
                worker.encode.submit(EncodeCommand::EndStream { key });
            }
            SceneEvent::ClientDisconnected(client_id) => {
                worker.input.client_disconnected(client_id);
                worker
                    .pending_composites
                    .retain(|key| key.client_id != client_id);
                // The encode thread owns the stream map, so it decides which
                // of its streams belonged to this client.
                worker
                    .encode
                    .submit(EncodeCommand::ClientGone { client_id });
            }
            SceneEvent::CursorChanged | SceneEvent::CapabilitiesChanged => {}
        }
    }
}

/// Applies one batch of wprs messages, running `between` after each -- so input
/// is served between messages, not after the batch. Returns how many messages
/// were applied and how long `Scene::apply` took in total.
///
/// `between` runs after a rejected message too: a malformed message still
/// consumed time, and input queued behind it should not wait for the rest of
/// the batch because of it.
fn apply_scene_messages(
    worker: &mut WorkerState,
    messages: impl IntoIterator<Item = wprs::serialization::RecvType<wprs::serialization::Request>>,
    mut between: impl FnMut(&mut InputState, &Scene, &mut ClipboardSync),
) -> (u32, u128) {
    let mut count = 0;
    let mut apply_us = 0;
    for message in messages {
        count += 1;
        let started = Instant::now();
        let applied = worker.scene.apply(message);
        apply_us += started.elapsed().as_micros();
        match applied {
            Ok(events) => handle_scene_events(worker, events),
            Err(error) => tracing::warn!(%error, "rejected wprs scene message"),
        }
        between(&mut worker.input, &worker.scene, &mut worker.clipboard);
    }
    (count, apply_us)
}

/// Carries out the clipboard requests in one batch and returns everything
/// else, for `apply_scene_messages`.
///
/// Clipboard is handled here rather than in `Scene::apply` because
/// `Scene::apply` only *returns* scene events and holds no transport, and
/// every clipboard request has to be answered on one -- the same
/// interception `api.rs` does for `Ping` ahead of `submit_input`.
/// `scene.rs`'s own `Request::Data` arm stays as a backstop; after this
/// nothing reaches it.
///
/// Clipboard requests are therefore carried out ahead of the batch's scene
/// messages instead of in arrival order. The two are independent -- no
/// clipboard decision reads the scene, and no scene message reads the
/// clipboard -- so only the order *among* clipboard requests matters, and
/// that is preserved.
fn take_clipboard_requests(
    clipboard: &mut ClipboardSync,
    messages: impl IntoIterator<Item = RecvType<Request>>,
    io: &SessionIo<'_>,
) -> Vec<RecvType<Request>> {
    let mut rest = Vec::new();
    for message in messages {
        match message {
            RecvType::Object(Request::Data(request)) => {
                handle_guest_data(clipboard, request, io);
            }
            other => rest.push(other),
        }
    }
    rest
}

/// Translates one wprs data request into a `ClipboardSync` event and carries
/// out what it decides. Pure translation: every clipboard *decision* lives in
/// `crate::clipboard`, and nothing here inspects or logs the content.
fn handle_guest_data(clipboard: &mut ClipboardSync, request: DataRequest, io: &SessionIo<'_>) {
    let event = match request {
        DataRequest::SourceRequest(DataSourceRequest::SetSelection(
            DataSource::Selection,
            metadata,
        )) => GuestEvent::SelectionOffered {
            mime_types: metadata.mime_types,
        },
        DataRequest::TransferData(DataSource::Selection, data) => {
            match clipboard.pending_guest_blob_mime() {
                Some(mime) => {
                    let stored = io
                        .blobs
                        .begin_write(io.session, &mime)
                        .and_then(|mut writer| {
                            writer.write_chunk(&data.0)?;
                            writer.finish()
                        });
                    match stored {
                        Ok(blob) => GuestEvent::TransferBlobFromGuest { blob },
                        Err(_) => GuestEvent::TransferBlobFailed,
                    }
                }
                None => GuestEvent::TransferFromGuest { bytes: data.0 },
            }
        }
        DataRequest::DestinationRequest(DataDestinationRequest::RequestDataTransfer(
            DataSource::Selection,
            mime,
        )) => GuestEvent::PasteRequested { mime },
        // Primary selection and drag-and-drop ride these same enums and are
        // out of scope: they fall through without touching clipboard state.
        _ => return,
    };

    let action = clipboard.on_guest(event);
    apply_sync_action(clipboard, action, io);
}

/// Carries out one decision.
///
/// Nothing in here may log the action or its payload: `SyncAction` and
/// `GuestEvent` both render clipboard content under `{:?}`, so a `?action`
/// would put a user's clipboard in a log line. MIME types, byte counts and
/// variant names only.
///
/// Every `AnswerGuest` must reach `send`. wprsd has already `take()`n the
/// pipe fd by the time a paste reaches us; an answer that never goes out
/// leaves that pipe unwritten and unclosed, and the pasting guest
/// application blocks on read forever. Hence no `?` and no early return on
/// this path.
///
/// `clipboard` is only otherwise done deciding by the time this runs --
/// `on_guest`/`on_phone_clipboard` have already returned `action` -- so
/// this is the one place left to correct a guess `ClipboardSync` had to
/// make with the transport knowledge it deliberately does not have. See
/// `PushToPhone`'s arm and `ClipboardSync::forget_phone_echo`.
fn apply_sync_action(clipboard: &mut ClipboardSync, action: SyncAction, io: &SessionIo<'_>) {
    match action {
        SyncAction::Nothing => {}
        SyncAction::AskGuestFor { mime } => {
            io.transport.send(Event::Data(DataEvent::SourceEvent(
                DataSourceEvent::MimeTypeSendRequestedByDestination(DataSource::Selection, mime),
            )));
        }
        SyncAction::PushToPhone { text } => {
            let reached = io
                .media
                .publish_message(io.session, MediaServerMessage::Clipboard { text });
            // ClipboardSync installed an echo token anticipating the phone
            // would receive this and might echo it straight back. Nobody
            // did, so there is nothing to echo -- undo the guess, or the
            // next genuine phone copy of this same text is mistaken for
            // that phantom echo and silently dropped.
            if reached == 0 {
                clipboard.forget_phone_echo();
            }
        }
        SyncAction::PushBlobToPhone { blob } => {
            let reached = io
                .media
                .publish_message(io.session, MediaServerMessage::ClipboardBlob { blob });
            if reached == 0 {
                clipboard.forget_phone_echo();
            }
        }
        SyncAction::OfferToGuest { mime_types } => {
            io.transport.send(Event::Data(DataEvent::DestinationEvent(
                DataDestinationEvent::SelectionSet(
                    DataSource::Selection,
                    SourceMetadata::from_mime_types(mime_types),
                ),
            )));
        }
        SyncAction::AnswerGuest { bytes } => {
            io.transport.send(Event::Data(DataEvent::TransferData(
                DataSource::Selection,
                DataToTransfer(bytes),
            )));
        }
        SyncAction::AnswerGuestBlob { blob } => {
            let bytes = io.blobs.read(io.session, &blob).unwrap_or_default();
            io.transport.send(Event::Data(DataEvent::TransferData(
                DataSource::Selection,
                DataToTransfer(bytes),
            )));
        }
    }
}

/// Composites every toplevel owed one and submits the frames, returning how
/// many were composited. One composite per surface however many commits it
/// received, which is the point.
///
/// `between` runs after each composite. With several windows this loop is the
/// longest uninterrupted stretch in an iteration -- one composite per window,
/// ~45ms each -- so input has to be served inside it, not only after it.
fn flush_composites(
    worker: &mut WorkerState,
    mut between: impl FnMut(&mut InputState, &Scene, &mut ClipboardSync),
) -> u32 {
    let mut composites = 0;
    for toplevel in std::mem::take(&mut worker.pending_composites) {
        if let Ok(frame) = worker.scene.compose_toplevel(toplevel) {
            composites += 1;
            worker.encode.submit(EncodeCommand::Frame {
                key: toplevel,
                frame: normalize_frame(frame),
            });
        }
        // After the composite rather than before, which means a
        // `ForceKeyframeAll` submitted by a pump lands *between* two windows'
        // frames: the later window re-keys this batch, the earlier one next
        // batch. Deliberate. Pumping first would only move the same asymmetry
        // onto the last composite, and no consumer compares keyframe timing
        // across streams -- each surface owns its own stream and encoder.
        between(&mut worker.input, &worker.scene, &mut worker.clipboard);
    }
    composites
}

/// What one iteration's input pumping cost and carried.
#[derive(Default)]
struct InputStats {
    inputs: u32,
    /// The longest any single input sat queued before being applied. This is
    /// the number that decides whether the guest repeats a key, not
    /// `busy_us` and not the iteration total.
    worst_wait_us: u128,
    /// Time spent applying input, summed across **every** pump in the
    /// iteration -- between each message, each composite, and once at the end.
    /// Logs from before input was pumped more than once per iteration report
    /// this field as a single drain phase, so the two are not comparable.
    busy_us: u128,
}

/// Applies whatever input is queued right now, and returns immediately when
/// there is none.
///
/// Called between every unit of work in the loop rather than once at the end of
/// a batch. That is the whole point: applying input costs ~4us, but a keystroke
/// that has to wait for a batch inherits the batch's cost, which scales with
/// window count (each commit decodes a full framebuffer, each window owes a
/// composite). Once that total clears the 200ms key repeat delay wprsd
/// advertises, the guest starts repeating the held key. Bounding the wait to
/// one unit of work removes the dependence on how many windows are painting,
/// instead of just lowering the constant.
fn pump_input(
    commands: &mut tokio::sync::mpsc::Receiver<MediaCommand>,
    input_state: &mut InputState,
    scene: &Scene,
    io: &SessionIo<'_>,
    resize: &mut Option<(Instant, u32, u32)>,
    stats: &mut InputStats,
    clipboard: &mut ClipboardSync,
) {
    let started = Instant::now();
    while let Ok(command) = commands.try_recv() {
        if let MediaCommand::Input { queued_at, .. } = &command {
            stats.inputs += 1;
            stats.worst_wait_us = stats.worst_wait_us.max(queued_at.elapsed().as_micros());
        }
        match command {
            MediaCommand::Input {
                attachment_id,
                input: MediaInput::ViewportResize { width, height },
                ..
            } => {
                let _ = attachment_id;
                *resize = Some((Instant::now(), width, height));
            }
            MediaCommand::Input {
                attachment_id: _,
                input: MediaInput::RequestKeyframe,
                ..
            } => {
                io.encode.submit(EncodeCommand::ForceKeyframeAll);
            }
            // Intercepted ahead of `InputState::apply`: a clipboard value is
            // neither a pointer nor a keyboard event and targets no surface,
            // so there is nothing for the input layer to scope it to.
            MediaCommand::Input {
                input: MediaInput::SetClipboard { text },
                ..
            } => {
                let action = clipboard.on_phone_clipboard(text);
                apply_sync_action(clipboard, action, io);
            }
            MediaCommand::Input {
                input: MediaInput::SetClipboardBlob { blob },
                ..
            } => {
                let action = clipboard.on_phone_blob(blob);
                apply_sync_action(clipboard, action, io);
            }
            MediaCommand::Input {
                attachment_id,
                input,
                ..
            } => {
                if let Err(error) = input_state.apply(attachment_id, input, scene, io.transport) {
                    tracing::warn!(%error, "rejected scoped media input");
                }
            }
            MediaCommand::Disconnected { attachment_id } => {
                input_state.disconnect(attachment_id, io.transport)
            }
        }
    }
    stats.busy_us += started.elapsed().as_micros();
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

/// Work handed to the encode thread.
///
/// Ordering between these matters: a frame composited before a surface was
/// destroyed must not be encoded and published after that surface's
/// `EndStream`, so they share one queue rather than travelling separately.
enum EncodeCommand {
    Frame {
        key: SurfaceKey,
        frame: Frame,
    },
    ForceKeyframeAll,
    EndStream {
        key: SurfaceKey,
    },
    /// Every stream belonging to a client that went away. The encode thread
    /// owns the stream map, so it is the only thing that can enumerate them.
    ClientGone {
        client_id: u64,
    },
}

/// Queue between the bridge loop and the encode thread.
///
/// Encoding is moved off the bridge loop because it blocks: `encode` writes a
/// whole frame into FFmpeg's stdin, and creating or *reconfiguring* a stream
/// spawns a fresh FFmpeg process and waits for it, ~600ms. That loop also
/// drains `MediaCommand::Input`, so every keystroke queued during a resize
/// waited for the encoder to come back — long enough for the guest's own key
/// repeat (wprsd advertises a 200ms delay) to fire and duplicate characters.
///
/// Frames coalesce per surface: submitting one for a key already queued
/// replaces it in place, keeping its queue position so ordering against
/// `EndStream` survives. That is what bounds the queue — at most one pending
/// frame per stream — and it is the right policy here, unlike on the input
/// path, because a superseded frame is worth nothing while a superseded
/// keystroke is lost data.
struct EncodeQueue {
    inner: Mutex<QueueInner>,
    signal: std::sync::Condvar,
}

struct QueueInner {
    queue: std::collections::VecDeque<EncodeCommand>,
    /// Surfaces whose queued frame was replaced. The next frame actually
    /// encoded for one of these is flagged as a discontinuity, so the viewer's
    /// HUD reports the gap rather than silently under-counting.
    superseded: std::collections::BTreeSet<SurfaceKey>,
    stopped: bool,
}

impl EncodeQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(QueueInner {
                queue: std::collections::VecDeque::new(),
                superseded: std::collections::BTreeSet::new(),
                stopped: false,
            }),
            signal: std::sync::Condvar::new(),
        }
    }

    fn submit(&self, command: EncodeCommand) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        match &command {
            EncodeCommand::Frame { key, .. } => {
                let key = *key;
                if let Some(slot) = inner.queue.iter_mut().find(
                    |queued| matches!(queued, EncodeCommand::Frame { key: queued, .. } if *queued == key),
                ) {
                    *slot = command;
                    inner.superseded.insert(key);
                    self.signal.notify_one();
                    return;
                }
            }
            EncodeCommand::EndStream { key } => {
                // Nothing composited before the destroy is worth encoding now.
                let key = *key;
                inner.queue.retain(
                    |queued| !matches!(queued, EncodeCommand::Frame { key: queued, .. } if *queued == key),
                );
                inner.superseded.remove(&key);
            }
            EncodeCommand::ClientGone { client_id } => {
                let client_id = *client_id;
                inner.queue.retain(|queued| {
                    !matches!(queued, EncodeCommand::Frame { key, .. } if key.client_id == client_id)
                });
                inner.superseded.retain(|key| key.client_id != client_id);
            }
            EncodeCommand::ForceKeyframeAll => {}
        }
        inner.queue.push_back(command);
        self.signal.notify_one();
    }

    /// Blocks until there is work or the queue is stopped.
    ///
    /// The lock is released before the caller encodes, so a slow encode — an
    /// FFmpeg spawn in the worst case — never makes `submit` wait. That is the
    /// whole point of the queue; keep it that way.
    fn next(&self) -> Option<(EncodeCommand, bool)> {
        let mut inner = self.inner.lock().ok()?;
        loop {
            if let Some(next) = pop_next(&mut inner) {
                return Some(next);
            }
            if inner.stopped {
                return None;
            }
            inner = self.signal.wait(inner).ok()?;
        }
    }

    /// Takes the next command if there is one, without blocking.
    #[cfg(test)]
    fn try_next(&self) -> Option<(EncodeCommand, bool)> {
        let mut inner = self.inner.lock().ok()?;
        pop_next(&mut inner)
    }

    fn stop(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.stopped = true;
        }
        self.signal.notify_all();
    }
}

/// Takes the next command off a locked queue, reporting whether a frame was
/// dropped in favour of the one being returned. The single place that pop and
/// `superseded` bookkeeping happen, so the blocking and non-blocking paths
/// cannot drift apart.
fn pop_next(inner: &mut QueueInner) -> Option<(EncodeCommand, bool)> {
    let command = inner.queue.pop_front()?;
    let superseded = match &command {
        EncodeCommand::Frame { key, .. } => inner.superseded.remove(key),
        _ => false,
    };
    Some((command, superseded))
}

/// Applies one command to the stream map. Shared by the encode thread and by
/// tests, which drive it synchronously so they can inspect the result.
fn apply_encode_command(
    session: &str,
    media: &MediaHub,
    streams: &mut HashMap<SurfaceKey, StreamState>,
    command: EncodeCommand,
    superseded: bool,
) {
    match command {
        EncodeCommand::Frame { key, frame } => {
            if superseded && let Some(stream) = streams.get_mut(&key) {
                // A frame for this surface was dropped in favour of this one,
                // so the stream really is missing data and must say so.
                stream.discontinuity = true;
            }
            if let Err(error) = encode_frame(session, media, streams, key, frame) {
                tracing::warn!(?key, %error, "failed to encode captured frame");
            }
        }
        EncodeCommand::ForceKeyframeAll => force_keyframe_on_all_streams(streams),
        EncodeCommand::EndStream { key } => end_stream(session, media, streams, key),
        EncodeCommand::ClientGone { client_id } => {
            let keys: Vec<SurfaceKey> = streams
                .keys()
                .filter(|key| key.client_id == client_id)
                .copied()
                .collect();
            for key in keys {
                end_stream(session, media, streams, key);
            }
        }
    }
}

/// Owns every encoder for one session, on a thread of its own.
fn encode_loop(session: String, media: MediaHub, queue: Arc<EncodeQueue>) {
    let mut streams: HashMap<SurfaceKey, StreamState> = HashMap::new();
    while let Some((command, superseded)) = queue.next() {
        apply_encode_command(&session, &media, &mut streams, command, superseded);
    }
    tracing::debug!("encode pipeline stopped");
}

/// Drains everything currently queued, through the same code path the encode
/// thread uses. Lets a test submit work and then inspect what it produced,
/// without the timing of a real thread.
#[cfg(test)]
fn drain_encode_queue(
    session: &str,
    media: &MediaHub,
    queue: &EncodeQueue,
    streams: &mut HashMap<SurfaceKey, StreamState>,
) {
    while let Some((command, superseded)) = queue.try_next() {
        apply_encode_command(session, media, streams, command, superseded);
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
    use wprs::serialization::{ClientId, Serializer};

    use super::*;
    use crate::clipboard::OFFERED_MIME_TYPES;
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

    fn worker_fixture(scene: Scene) -> WorkerState {
        WorkerState {
            scene,
            input: InputState::default(),
            encode: Arc::new(EncodeQueue::new()),
            pending_composites: std::collections::BTreeSet::new(),
            clipboard: ClipboardSync::new(),
        }
    }

    /// One bridge-loop iteration's worth of scene work: apply the batch, then
    /// composite once per surface -- the same order `run_bridge` uses. Tests go
    /// through this rather than calling the two halves separately, so that a
    /// commit and a destroy landing in one batch are ordered here exactly as
    /// they are in production.
    fn apply_scene_batch(worker: &mut WorkerState, events: Vec<SceneEvent>) {
        handle_scene_events(worker, events);
        // No-op hook: production pumps input between composites, but these
        // tests assert on what composition produces, not on input timing.
        flush_composites(worker, |_, _, _| {});
    }

    /// The stream map the encode thread would own, held locally so a test can
    /// inspect it. Encoding is no longer synchronous with `handle_scene_events`,
    /// so a test drains between the steps whose effects it wants to separate —
    /// draining only at the end would let two frames for one surface coalesce
    /// into one, which is correct behaviour but not what those tests measure.
    fn streams_fixture() -> HashMap<SurfaceKey, StreamState> {
        HashMap::new()
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
        let dir = tempfile::tempdir().unwrap();
        let manager = BridgeManager::new(media.clone(), BlobStore::new(dir.path().join("blobs")));

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
        let mut worker = worker_fixture(scene);
        let mut streams = streams_fixture();
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
        apply_scene_batch(
            &mut worker,
            vec![
                SceneEvent::SurfaceCommitted(key1),
                SceneEvent::SurfaceCommitted(key2),
            ],
        );
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);

        assert_eq!(streams.len(), 2);
        let stream1_id = streams[&key1].id;
        let stream2_id = streams[&key2].id;
        assert_ne!(stream1_id, stream2_id, "each toplevel gets its own stream");
        assert_eq!(streams[&key1].sequence, 2);
        assert_eq!(streams[&key2].sequence, 2);

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
        encode_frame("s1", &media, &mut streams, key1, frame).unwrap();
        assert!(streams[&key1].sequence > 2);
        assert_eq!(
            streams[&key2].sequence, 2,
            "publishing to one stream must not disturb the other's sequencing"
        );

        // The resize path forces every live stream to re-key on its next frame.
        force_keyframe_on_all_streams(&mut streams);
        assert!(streams[&key1].force_keyframe);
        assert!(streams[&key1].discontinuity);
        assert!(streams[&key2].force_keyframe);
        assert!(streams[&key2].discontinuity);
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
        let mut worker = worker_fixture(scene);
        let mut streams = streams_fixture();
        let key1 = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let key2 = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(key1)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);

        assert_eq!(
            streams.len(),
            1,
            "only the committed toplevel may be encoded"
        );
        let stream1 = streams[&key1].id;
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
        // commits to it leave the first window's stream alone. Each commit is
        // drained before the next is submitted: two queued frames for one
        // surface would coalesce, which is right in production but would make
        // this test measure one frame where it means to measure two.
        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(key2)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        assert_eq!(streams.len(), 2);
        let sequence1 = streams[&key1].sequence;
        for expected in [MediaKind::StreamConfig, MediaKind::Video] {
            let packet = recv_packet(&client).await;
            assert_eq!(packet.header.stream_id, streams[&key2].id);
            assert_eq!(packet.header.kind, expected);
        }

        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(key2)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        assert_eq!(
            streams[&key1].sequence, sequence1,
            "a commit to one window must not advance another window's stream"
        );
        let packet = recv_packet(&client).await;
        assert_eq!(packet.header.stream_id, streams[&key2].id);
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
        let mut worker = worker_fixture(scene);
        let mut streams = streams_fixture();
        let toplevel = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let child = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };

        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(child)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);

        assert!(
            streams.contains_key(&toplevel),
            "a subsurface commit must re-encode its toplevel ancestor"
        );
        assert!(
            !streams.contains_key(&child),
            "a subsurface has no stream of its own"
        );
        assert_eq!(streams.len(), 1);
        let stream = streams[&toplevel].id;
        for expected in [MediaKind::StreamConfig, MediaKind::Video] {
            let packet = recv_packet(&client).await;
            assert_eq!(packet.header.stream_id, stream);
            assert_eq!(packet.header.kind, expected);
        }

        let sequence = streams[&toplevel].sequence;
        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(child)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        assert_eq!(streams[&toplevel].id, stream);
        assert!(
            streams[&toplevel].sequence > sequence,
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
        let mut worker = worker_fixture(scene);
        let mut streams = streams_fixture();
        let key_a = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let key_b = SurfaceKey {
            client_id: 2,
            surface_id: 1,
        };

        apply_scene_batch(
            &mut worker,
            vec![
                SceneEvent::SurfaceCommitted(key_a),
                SceneEvent::SurfaceCommitted(key_b),
            ],
        );
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        assert_eq!(streams.len(), 2);
        for _ in 0..4 {
            recv_packet(&client).await; // drain the initial config+video pairs
        }

        apply_scene_batch(&mut worker, vec![SceneEvent::ClientDisconnected(1)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        assert!(!streams.contains_key(&key_a));
        assert!(streams.contains_key(&key_b));
        assert_eq!(
            streams[&key_b].sequence, 2,
            "client B's stream must be untouched by client A's disconnect"
        );
        let end_a = recv_packet(&client).await;
        assert_eq!(end_a.header.kind, MediaKind::StreamEnd);

        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceDestroyed(key_b)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        assert!(streams.is_empty());
        let end_b = recv_packet(&client).await;
        assert_eq!(end_b.header.kind, MediaKind::StreamEnd);
        assert_ne!(end_a.header.stream_id, end_b.header.stream_id);
    }

    /// Everything the media hub published, read until it goes quiet. Lets a
    /// test assert on how *many* packets a step produced, which is the whole
    /// question for coalescing.
    async fn drain_packets(client: &MediaAttachment) -> Vec<(MediaKind, bool)> {
        let mut packets = Vec::new();
        while let Ok(Some(packet)) =
            tokio::time::timeout(Duration::from_millis(250), client.recv()).await
        {
            packets.push((packet.header.kind, packet.header.flags.discontinuity()));
        }
        packets
    }

    /// Two frames queued for one surface collapse to one — that is what bounds
    /// the queue without an arbitrary cap. The survivor must be flagged as a
    /// discontinuity, or the viewer's HUD `DISC` counter under-reports a gap
    /// that really happened.
    #[tokio::test]
    async fn a_superseded_frame_collapses_into_one_encode_flagged_as_a_discontinuity() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        let mut worker = worker_fixture(scene_with_toplevels(&[(1, 1)]));
        let mut streams = streams_fixture();
        let key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };

        // Establish the stream first: a brand-new stream's first frame is
        // always a discontinuity, which would mask the flag under test.
        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(key)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        drain_packets(&client).await;
        assert!(
            !streams[&key].discontinuity,
            "an established stream starts this test with no pending gap"
        );

        // Two commits, no drain between them, so the second frame replaces the
        // first while it is still queued.
        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(key)]);
        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(key)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);

        let published = drain_packets(&client).await;
        let video: Vec<_> = published
            .iter()
            .filter(|(kind, _)| *kind == MediaKind::Video)
            .collect();
        assert_eq!(
            video.len(),
            1,
            "two frames queued for one surface must encode once, not twice"
        );
        assert!(
            video[0].1,
            "the frame that replaced another must report the gap as a discontinuity"
        );
    }

    /// A frame composited before a surface was destroyed must not be encoded
    /// and published after that surface's stream has ended. Sharing one queue
    /// is what makes the ordering decidable; dropping the frame is what makes
    /// it cheap.
    #[tokio::test]
    async fn a_frame_composited_before_a_destroy_never_publishes_after_the_stream_ends() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        let mut worker = worker_fixture(scene_with_toplevels(&[(1, 1)]));
        let mut streams = streams_fixture();
        let key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };

        apply_scene_batch(&mut worker, vec![SceneEvent::SurfaceCommitted(key)]);
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);
        drain_packets(&client).await;

        // Commit then destroy in one batch: the frame is queued behind nothing
        // and the destroy lands while it is still waiting.
        apply_scene_batch(
            &mut worker,
            vec![
                SceneEvent::SurfaceCommitted(key),
                SceneEvent::SurfaceDestroyed(key),
            ],
        );
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);

        let published = drain_packets(&client).await;
        assert!(
            !published.iter().any(|(kind, _)| *kind == MediaKind::Video),
            "a frame from before the destroy must not be encoded once the stream is gone"
        );
        assert_eq!(
            published
                .iter()
                .filter(|(kind, _)| *kind == MediaKind::StreamEnd)
                .count(),
            1,
            "the stream still has to end exactly once"
        );
        assert!(streams.is_empty());
    }

    /// Deferring composition to the end of a batch creates an ordering hazard
    /// the immediate version could not have: a commit and a destroy for the
    /// same surface in one batch would composite *after* the `EndStream`, and
    /// since the queue's `EndStream` handling only drops frames already queued,
    /// that late frame would open a fresh stream for a window that is gone.
    #[tokio::test]
    async fn a_commit_and_destroy_in_one_batch_never_composites_the_destroyed_surface() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        let mut worker = worker_fixture(scene_with_toplevels(&[(1, 1)]));
        let mut streams = streams_fixture();
        let key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };

        // Never streamed before, so anything published here can only come from
        // the commit that shares a batch with the destroy.
        apply_scene_batch(
            &mut worker,
            vec![
                SceneEvent::SurfaceCommitted(key),
                SceneEvent::SurfaceDestroyed(key),
            ],
        );
        drain_encode_queue("s1", &media, &worker.encode, &mut streams);

        assert!(
            worker.pending_composites.is_empty(),
            "a destroyed surface must not stay owed a composite"
        );
        assert!(
            drain_packets(&client).await.is_empty(),
            "a surface destroyed in the same batch as its commit must publish nothing"
        );
        assert!(streams.is_empty());
    }

    /// The other half of the latency bound: input must be served between
    /// *messages* too, and even after a message that was rejected.
    ///
    /// `Scene::apply` decodes a whole framebuffer per commit, so a batch is
    /// unbounded work; a keystroke that waits for the batch inherits that
    /// bound. The middle message here is a second consecutive raw buffer,
    /// which the scene rejects as unpaired -- a rejected message still cost
    /// time, so input behind it must not wait for the rest of the batch.
    #[test]
    fn applying_a_message_batch_serves_input_between_each_message() {
        let mut worker = worker_fixture(Scene::default());
        let batch = vec![
            RecvType::RawBuffer(vec![0; 8]),
            RecvType::RawBuffer(vec![0; 8]),
            RecvType::RawBuffer(vec![0; 8]),
        ];

        let mut served = 0;
        let (count, _) = apply_scene_messages(&mut worker, batch, |_, _, _| served += 1);

        assert_eq!(count, 3);
        assert_eq!(
            served, 3,
            "input must be pumped once per message, including after a rejected one"
        );
    }

    /// Input must be served *between* composites, not only after them.
    ///
    /// This is the property that bounds input latency. One composite per window
    /// is already the floor after coalescing, so a batch of them costs
    /// (windows x ~45ms); a keystroke that waits for the whole batch inherits
    /// that, and past three windows it clears the 200ms key repeat delay wprsd
    /// advertises and the guest starts repeating. Serving input between each
    /// composite caps the wait at one composite regardless of window count.
    #[test]
    fn flush_composites_serves_input_between_each_composite() {
        let mut worker = worker_fixture(scene_with_toplevels(&[(1, 1), (1, 2), (1, 3)]));
        for surface_id in [1, 2, 3] {
            handle_scene_events(
                &mut worker,
                vec![SceneEvent::SurfaceCommitted(SurfaceKey {
                    client_id: 1,
                    surface_id,
                })],
            );
        }

        let mut served = 0;
        let composites = flush_composites(&mut worker, |_, _, _| served += 1);

        assert_eq!(composites, 3, "three windows owe three composites");
        assert_eq!(
            served, 3,
            "input must be pumped once per composite, not once for the whole batch"
        );
    }

    /// The point of the coalescing: many commits to one window in a single
    /// batch cost one composite, not one each.
    #[tokio::test]
    async fn repeated_commits_in_one_batch_composite_once() {
        let mut worker = worker_fixture(scene_with_toplevels(&[(1, 1)]));
        let key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };

        handle_scene_events(
            &mut worker,
            (0..10).map(|_| SceneEvent::SurfaceCommitted(key)).collect(),
        );
        assert_eq!(
            worker.pending_composites.len(),
            1,
            "ten commits to one window owe one composite"
        );
        assert_eq!(
            flush_composites(&mut worker, |_, _, _| {}),
            1,
            "and compositing the batch runs exactly once"
        );
        assert!(worker.pending_composites.is_empty());
    }

    /// A composite that fails still cost wall-clock time (it walked the scene
    /// graph before erroring), so input queued behind it must not wait for the
    /// next surface's composite too. Mirrors
    /// `applying_a_message_batch_serves_input_between_each_message`, which
    /// pins the same property for a rejected message; this is the equivalent
    /// for `flush_composites`, whose old `let Ok(frame) = ... else { continue };`
    /// shape would have skipped the pump entirely on failure.
    #[test]
    fn a_failed_composite_still_serves_input_before_the_next_one() {
        let mut worker = worker_fixture(scene_with_toplevels(&[(1, 2)]));
        // Never committed, so `compose_toplevel` fails with `UnknownSurface`.
        worker.pending_composites.insert(SurfaceKey {
            client_id: 1,
            surface_id: 1,
        });
        worker.pending_composites.insert(SurfaceKey {
            client_id: 1,
            surface_id: 2,
        });

        let mut served = 0;
        let composites = flush_composites(&mut worker, |_, _, _| served += 1);

        assert_eq!(composites, 1, "only the committed surface composites");
        assert_eq!(
            served, 2,
            "input must be pumped after the failure too, not only after the success"
        );
    }

    /// A headless stand-in for `wprsd`: gives a `WprsTransport` something to
    /// talk to and hands back the raw `Event`s it sent, so a test can see
    /// what the clipboard wiring actually put on the wire.
    ///
    /// `navette-bridge/src/input.rs` has a fixture of the same shape, built
    /// on `Serializer::new_server`. That is not usable here.
    /// `new_server` binds through `wprs::utils::bind_user_socket`, which
    /// widens the process-wide umask (`umask(S_IXUSR | S_IRWXG | S_IRWXO)`)
    /// around the bind; a `mkdir` from another test thread landing in that
    /// window comes back without its own owner-execute bit, so the directory
    /// is untraversable and unrelated tests fail with `EACCES`. Measured at
    /// five failures in ten runs of this binary. `navette-bridge` gets away
    /// with it because every temp dir in *that* binary is created under the
    /// same lock; the twenty-odd in this one are not, and serializing them
    /// all on a wprs quirk is not a trade worth making.
    ///
    /// So neither end binds a wprs socket: both connect as clients to one
    /// plain `UnixListener`, and two relay threads splice the accepted
    /// streams together. wprs is designed to run over an SSH-forwarded
    /// socket and so passes no file descriptors, only framed bytes, which is
    /// what makes a byte relay a faithful stand-in for the real link.
    struct FakeWprsd {
        events: calloop::channel::Channel<RecvType<Event>>,
        _server: Serializer<Request, Event>,
        _dir: tempfile::TempDir,
    }

    impl FakeWprsd {
        fn connect() -> (WprsTransport, Self) {
            let dir = tempfile::tempdir().expect("create temp dir for the relay socket");
            let socket = dir.path().join("wprs.sock");
            let listener = UnixListener::bind(&socket).expect("bind the relay socket");
            let transport = WprsTransport::connect(&socket).expect("connect the bridge end");
            let mut server: Serializer<Request, Event> =
                Serializer::new_client(&socket).expect("connect the fake wprsd end");
            // Both connects completed against the backlog, so two accepts
            // succeed here. Which end each one is does not matter: the relay
            // below is symmetric, so it pairs them either way.
            let (one, _) = listener.accept().expect("accept the first end");
            let (other, _) = listener.accept().expect("accept the second end");
            splice(&one, &other);
            splice(&other, &one);
            let events = server.reader().expect("fake wprsd reader already taken");
            (
                transport,
                Self {
                    events,
                    _server: server,
                    _dir: dir,
                },
            )
        }

        /// The next event the transport sent, skipping the connection
        /// preamble (`WprsClientConnect`, `Output`) `WprsTransport::connect`
        /// emits on its own.
        fn recv(&self) -> Event {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.events.try_recv() {
                    Ok(RecvType::Object(Event::WprsClientConnect | Event::Output(_)))
                    | Ok(RecvType::RawBuffer(_)) => continue,
                    Ok(RecvType::Object(event)) => return event,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        assert!(
                            Instant::now() < deadline,
                            "timed out waiting for a wprs event"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("fake wprsd channel disconnected")
                    }
                }
            }
        }

        /// Asserts nothing further arrives -- used to prove a would-be event
        /// was never sent rather than merely delayed.
        ///
        /// Drains to empty rather than inspecting one event. `connect` leaves
        /// two preamble events queued ahead of anything a test provokes, so a
        /// single `try_recv` would consume one of those, report success, and
        /// never look at the event it exists to catch.
        fn assert_no_further_events(&self) {
            thread::sleep(Duration::from_millis(20));
            loop {
                match self.events.try_recv() {
                    Ok(RecvType::Object(Event::WprsClientConnect | Event::Output(_)))
                    | Ok(RecvType::RawBuffer(_)) => continue,
                    Ok(RecvType::Object(event)) => {
                        // Names the variant only: an event carrying clipboard
                        // text must not be rendered into a panic message
                        // either.
                        panic!("expected no further events, got {}", event_name(&event))
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => return,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("fake wprsd channel disconnected")
                    }
                }
            }
        }
    }

    /// Copies one direction of the relay until its source closes.
    fn splice(from: &std::os::unix::net::UnixStream, to: &std::os::unix::net::UnixStream) {
        let mut from = from.try_clone().expect("clone the relay source");
        let mut to = to.try_clone().expect("clone the relay sink");
        thread::spawn(move || {
            let _ = std::io::copy(&mut from, &mut to);
            let _ = to.shutdown(std::net::Shutdown::Write);
        });
    }

    /// The variant name of an event, with no payload. Clipboard text must
    /// not reach a log line or an assertion message.
    fn event_name(event: &Event) -> &'static str {
        match event {
            Event::WprsClientConnect => "WprsClientConnect",
            Event::Output(_) => "Output",
            Event::PointerFrame(_) => "PointerFrame",
            Event::KeyboardEvent(_) => "KeyboardEvent",
            Event::Toplevel(_) => "Toplevel",
            Event::Popup(_) => "Popup",
            Event::Data(_) => "Data",
            Event::Surface(_) => "Surface",
        }
    }

    fn data_request(request: DataRequest) -> RecvType<Request> {
        RecvType::Object(Request::Data(request))
    }

    /// The next server message the hub published. Mirrors `recv_packet`,
    /// which does the same for media packets.
    async fn recv_message(client: &MediaAttachment) -> MediaServerMessage {
        tokio::time::timeout(Duration::from_secs(5), client.recv_message())
            .await
            .expect("a server message should have been published before the timeout")
            .expect("media channel closed unexpectedly")
    }

    /// Everything one clipboard test needs, driven through the same two
    /// entry points `run_bridge` uses: `take_clipboard_requests` for what
    /// the guest sends, and `pump_input` for what the phone sends.
    struct ClipboardFixture {
        worker: WorkerState,
        transport: WprsTransport,
        wprsd: FakeWprsd,
        media: MediaHub,
        _blob_dir: tempfile::TempDir,
        blobs: BlobStore,
        // `Option` so a test can simulate nobody being attached (`self.client
        // = None`) without the partial-move `ClipboardFixture` would suffer
        // as a plain `MediaAttachment` field -- every other method here
        // takes `&mut self`, which needs the whole struct initialized.
        client: Option<MediaAttachment>,
        commands: tokio::sync::mpsc::Receiver<MediaCommand>,
    }

    impl ClipboardFixture {
        fn new() -> Self {
            let media = MediaHub::default();
            let blob_dir = tempfile::tempdir().unwrap();
            let blobs = BlobStore::new(blob_dir.path().join("blobs"));
            let commands = media.register_session("s1");
            let client = media.attach("s1").expect("attach to the fixture session");
            let (transport, wprsd) = FakeWprsd::connect();
            Self {
                worker: worker_fixture(Scene::default()),
                transport,
                wprsd,
                media,
                _blob_dir: blob_dir,
                blobs,
                client: Some(client),
                commands,
            }
        }

        /// One batch through the loop's partition, returning what is left
        /// for `apply_scene_messages`.
        fn send_requests(&mut self, messages: Vec<RecvType<Request>>) -> Vec<RecvType<Request>> {
            let io = SessionIo {
                transport: &self.transport,
                media: &self.media,
                blobs: &self.blobs,
                encode: &self.worker.encode,
                session: "s1",
            };
            take_clipboard_requests(&mut self.worker.clipboard, messages, &io)
        }

        /// One input pump, the same call the loop makes between units of
        /// work.
        fn pump(&mut self) {
            let io = SessionIo {
                transport: &self.transport,
                media: &self.media,
                blobs: &self.blobs,
                encode: &self.worker.encode,
                session: "s1",
            };
            let mut resize = None;
            let mut stats = InputStats::default();
            pump_input(
                &mut self.commands,
                &mut self.worker.input,
                &self.worker.scene,
                &io,
                &mut resize,
                &mut stats,
                &mut self.worker.clipboard,
            );
        }
    }

    /// A guest copy travels: the offer comes in, the bridge asks the guest
    /// for the text, the transfer comes in, and the text reaches the media
    /// hub. None of it goes through `Scene::apply`, which is the point --
    /// `Scene::apply` only returns scene events and has no transport to ask
    /// on.
    #[tokio::test]
    async fn a_guest_copy_reaches_the_media_hub() {
        let mut fixture = ClipboardFixture::new();

        fixture.send_requests(vec![data_request(DataRequest::SourceRequest(
            DataSourceRequest::SetSelection(
                DataSource::Selection,
                SourceMetadata::from_mime_types(vec!["text/plain".to_string()]),
            ),
        ))]);

        assert!(
            matches!(
                fixture.wprsd.recv(),
                Event::Data(DataEvent::SourceEvent(
                    DataSourceEvent::MimeTypeSendRequestedByDestination(DataSource::Selection, mime)
                )) if mime == "text/plain"
            ),
            "the bridge must ask the guest for the offered text"
        );

        fixture.send_requests(vec![data_request(DataRequest::TransferData(
            DataSource::Selection,
            DataToTransfer(b"hello".to_vec()),
        ))]);

        assert_eq!(
            recv_message(fixture.client.as_ref().expect("fixture client attached")).await,
            MediaServerMessage::Clipboard {
                text: "hello".into()
            },
            "the guest's clipboard text must reach the media hub"
        );
    }

    #[tokio::test]
    async fn a_guest_image_copy_is_stored_then_published_as_a_descriptor() {
        let mut fixture = ClipboardFixture::new();
        fixture.send_requests(vec![data_request(DataRequest::SourceRequest(
            DataSourceRequest::SetSelection(
                DataSource::Selection,
                SourceMetadata::from_mime_types(vec!["image/png".to_string()]),
            ),
        ))]);
        assert!(matches!(
            fixture.wprsd.recv(),
            Event::Data(DataEvent::SourceEvent(
                DataSourceEvent::MimeTypeSendRequestedByDestination(DataSource::Selection, mime)
            )) if mime == "image/png"
        ));

        fixture.send_requests(vec![data_request(DataRequest::TransferData(
            DataSource::Selection,
            DataToTransfer(b"png".to_vec()),
        ))]);
        let MediaServerMessage::ClipboardBlob { blob } =
            recv_message(fixture.client.as_ref().expect("fixture client attached")).await
        else {
            panic!("guest image must publish a descriptor, not bytes");
        };
        assert_eq!(blob.mime, "image/png");
        assert_eq!(fixture.blobs.read("s1", &blob).unwrap(), b"png");
    }

    #[test]
    fn a_phone_image_descriptor_offers_image_mimes_and_missing_data_answers_empty() {
        let mut fixture = ClipboardFixture::new();
        let mut writer = fixture.blobs.begin_write("s1", "image/png").unwrap();
        writer.write_chunk(b"png").unwrap();
        let blob = writer.finish().unwrap();
        fixture
            .client
            .as_ref()
            .expect("fixture client attached")
            .submit_input(MediaInput::SetClipboardBlob { blob: blob.clone() })
            .unwrap();
        fixture.pump();
        assert!(matches!(
            fixture.wprsd.recv(),
            Event::Data(DataEvent::DestinationEvent(
                DataDestinationEvent::SelectionSet(DataSource::Selection, metadata)
            )) if metadata.mime_types == vec!["image/png", "image/jpeg", "image/webp"]
        ));
        fixture.send_requests(vec![data_request(DataRequest::DestinationRequest(
            DataDestinationRequest::RequestDataTransfer(
                DataSource::Selection,
                "image/png".to_string(),
            ),
        ))]);
        assert!(matches!(
            fixture.wprsd.recv(),
            Event::Data(DataEvent::TransferData(
                DataSource::Selection,
                DataToTransfer(bytes)
            )) if bytes == b"png"
        ));

        let missing = navette_protocol::media::BlobDescriptor {
            id: "11111111111111111111111111111111".into(),
            mime: "image/png".into(),
            size: 3,
        };
        fixture
            .client
            .as_ref()
            .expect("fixture client attached")
            .submit_input(MediaInput::SetClipboardBlob { blob: missing })
            .unwrap();
        fixture.pump();
        let _offered = fixture.wprsd.recv();
        fixture.send_requests(vec![data_request(DataRequest::DestinationRequest(
            DataDestinationRequest::RequestDataTransfer(
                DataSource::Selection,
                "image/png".to_string(),
            ),
        ))]);
        assert!(matches!(
            fixture.wprsd.recv(),
            Event::Data(DataEvent::TransferData(
                DataSource::Selection,
                DataToTransfer(bytes)
            )) if bytes.is_empty()
        ));
    }

    /// The `bridge.rs` wiring for `ClipboardSync::forget_phone_echo`:
    /// `clipboard.rs`'s own unit test proves the state machine method,
    /// `media.rs`'s proves `publish_message`'s return value, and this
    /// proves the two are actually connected. With nobody attached to
    /// receive a guest copy, a later genuine phone copy of the same text
    /// must still reach the guest -- not be mistaken for the echo of a
    /// push that, in fact, nobody ever received.
    #[test]
    fn a_guest_push_reaching_nobody_forgets_its_echo_so_the_same_text_reaches_the_guest_later() {
        let mut fixture = ClipboardFixture::new();
        // Nobody is attached to receive the push about to happen. An
        // assignment, not `drop(fixture.client)`: the latter partially
        // moves the field out, and every method below takes `&mut self`,
        // which needs `fixture` whole.
        fixture.client = None;

        fixture.send_requests(vec![data_request(DataRequest::SourceRequest(
            DataSourceRequest::SetSelection(
                DataSource::Selection,
                SourceMetadata::from_mime_types(vec!["text/plain".to_string()]),
            ),
        ))]);
        assert!(matches!(
            fixture.wprsd.recv(),
            Event::Data(DataEvent::SourceEvent(
                DataSourceEvent::MimeTypeSendRequestedByDestination(DataSource::Selection, _)
            ))
        ));

        fixture.send_requests(vec![data_request(DataRequest::TransferData(
            DataSource::Selection,
            DataToTransfer(b"hello".to_vec()),
        ))]);

        // The phone attaches later and genuinely copies the same text.
        let client = fixture
            .media
            .attach("s1")
            .expect("re-attach to the fixture session");
        client
            .submit_input(MediaInput::SetClipboard {
                text: "hello".into(),
            })
            .expect("the fixture session accepts input");
        fixture.pump();

        let offered: Vec<String> = OFFERED_MIME_TYPES
            .iter()
            .map(|mime| (*mime).to_string())
            .collect();
        assert!(
            matches!(
                fixture.wprsd.recv(),
                Event::Data(DataEvent::DestinationEvent(
                    DataDestinationEvent::SelectionSet(DataSource::Selection, metadata)
                )) if metadata.mime_types == offered
            ),
            "a genuine phone copy must not be mistaken for the echo of a push nobody received"
        );
    }

    /// The other direction, end to end: the phone's copy is offered to the
    /// guest, and the guest's later paste is answered with that text.
    #[test]
    fn a_phone_copy_is_offered_and_answered() {
        let mut fixture = ClipboardFixture::new();

        fixture
            .client
            .as_ref()
            .expect("fixture client attached")
            .submit_input(MediaInput::SetClipboard {
                text: "hello".into(),
            })
            .expect("the fixture session accepts input");
        fixture.pump();

        let offered: Vec<String> = OFFERED_MIME_TYPES
            .iter()
            .map(|mime| (*mime).to_string())
            .collect();
        assert!(
            matches!(
                fixture.wprsd.recv(),
                Event::Data(DataEvent::DestinationEvent(
                    DataDestinationEvent::SelectionSet(DataSource::Selection, metadata)
                )) if metadata.mime_types == offered
            ),
            "the phone's clipboard must be offered to the guest"
        );

        fixture.send_requests(vec![data_request(DataRequest::DestinationRequest(
            DataDestinationRequest::RequestDataTransfer(
                DataSource::Selection,
                "text/plain".to_string(),
            ),
        ))]);

        assert!(
            matches!(
                fixture.wprsd.recv(),
                Event::Data(DataEvent::TransferData(
                    DataSource::Selection,
                    DataToTransfer(bytes)
                )) if bytes.as_slice() == b"hello".as_slice()
            ),
            "the guest's paste must be answered with the phone's text"
        );
    }

    /// The hang regression, at the wiring level. wprsd `take()`s the pipe fd
    /// when it forwards the paste, so a paste we never answer leaves that
    /// pipe unwritten and unclosed and the pasting guest application blocks
    /// on read forever. `clipboard.rs` proves the decision is always to
    /// answer; this proves the answer reaches the wire even when there is
    /// nothing to say.
    #[test]
    fn a_paste_with_no_phone_copy_is_still_answered_on_the_wire() {
        let mut fixture = ClipboardFixture::new();

        fixture.send_requests(vec![data_request(DataRequest::DestinationRequest(
            DataDestinationRequest::RequestDataTransfer(
                DataSource::Selection,
                "text/plain".to_string(),
            ),
        ))]);

        assert!(
            matches!(
                fixture.wprsd.recv(),
                Event::Data(DataEvent::TransferData(
                    DataSource::Selection,
                    DataToTransfer(bytes)
                )) if bytes.is_empty()
            ),
            "a paste with nothing to answer with must still be answered, or the guest hangs"
        );
    }

    /// Only `DataSource::Selection` is the clipboard: primary selection and
    /// drag-and-drop ride the same enums and must produce nothing. And every
    /// message that is not a data request has to come back out of the
    /// partition, or the scene stops being drawn.
    #[test]
    fn primary_selection_is_ignored_and_scene_messages_pass_through() {
        let mut fixture = ClipboardFixture::new();

        let rest = fixture.send_requests(vec![
            RecvType::RawBuffer(vec![0; 8]),
            data_request(DataRequest::SourceRequest(DataSourceRequest::SetSelection(
                DataSource::Primary,
                SourceMetadata::from_mime_types(vec!["text/plain".to_string()]),
            ))),
            commit(surface_state(1, 1)),
        ]);

        assert_eq!(
            rest.len(),
            2,
            "only data requests are taken out of the batch"
        );
        assert!(matches!(rest[0], RecvType::RawBuffer(_)));
        assert!(matches!(rest[1], RecvType::Object(Request::Surface(_))));
        fixture.wprsd.assert_no_further_events();
    }

    /// The real thread, not the synchronous drain every other test uses: it
    /// has to finish the work already queued, then stop and be joinable. A
    /// `stop()`/`join()` deadlock here would otherwise only ever show up as a
    /// hung session teardown on real hardware.
    #[tokio::test]
    async fn the_encode_thread_finishes_queued_work_then_stops_and_joins() {
        if !ffmpeg_available() {
            return;
        }
        let media = MediaHub::default();
        let _input = media.register_session("s1");
        let client = media.attach("s1").unwrap();

        let mut scene = scene_with_toplevels(&[(1, 1)]);
        let key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };
        let frame = normalize_frame(scene.compose_toplevel(key).unwrap());

        let queue = Arc::new(EncodeQueue::new());
        let thread = {
            let queue = Arc::clone(&queue);
            let media = media.clone();
            thread::spawn(move || encode_loop("s1".to_owned(), media, queue))
        };

        queue.submit(EncodeCommand::Frame { key, frame });
        // Stopping with work still queued must drain it rather than discard
        // it -- `run_bridge` stops the thread on every session teardown.
        queue.stop();
        thread.join().expect("the encode thread must not panic");

        let published = drain_packets(&client).await;
        let kinds: Vec<MediaKind> = published.iter().map(|(kind, _)| *kind).collect();
        assert_eq!(
            kinds,
            vec![MediaKind::StreamConfig, MediaKind::Video],
            "the queued frame must be encoded and published before the thread exits"
        );
    }
}
