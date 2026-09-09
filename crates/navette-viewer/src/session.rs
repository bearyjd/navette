//! Wiring between one media session's streams and the local desktop.
//!
//! Each active toplevel stream gets its own native window; a window's input is
//! addressed to its own stream's surface and to no other; and viewport changes
//! are coalesced before they reach the wire. All of it is synchronous and
//! clock-injected so it can be driven from a test without a display or a
//! socket.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use navette_protocol::media::MediaInput;

use crate::hud::{HudSample, StreamHud};
use crate::router::{StreamEvent, StreamFrame};
use crate::window::{Modifiers, Window, WindowEvent, WindowFactory, WindowSpec};

/// How long a window's size must hold still before the change is reported.
///
/// The bridge debounces resizes again at 100ms server-side; going slightly
/// longer here means a drag turns into one message rather than a stream of
/// them the server then has to absorb.
pub const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);

/// Viewport bounds the bridge accepts. Anything outside is clamped rather than
/// sent and rejected.
pub const MIN_VIEWPORT: (u32, u32) = (320, 240);
pub const MAX_VIEWPORT: (u32, u32) = (3840, 2160);

/// How often each stream's performance figures are recomputed and logged.
const HUD_INTERVAL: Duration = Duration::from_secs(1);

struct StreamWindow {
    /// The surface identity this window's input is addressed to. It comes
    /// from this stream's own frames and is never read from another stream.
    client_id: u64,
    surface_id: u64,
    window: Box<dyn Window>,
    hud: HudSample,
    hud_computed: Option<Instant>,
}

/// One media session's windows.
pub struct ViewerSession {
    factory: WindowFactory,
    windows: BTreeMap<u64, StreamWindow>,
    huds: BTreeMap<u64, StreamHud>,
    /// Streams that must never open a window again: the toplevel has ended,
    /// or the user closed the window while frames were still arriving. Stream
    /// ids come from a counter the bridge never rewinds, so this only ever
    /// holds streams this session really saw.
    ignored: BTreeSet<u64>,
    /// One viewport for the whole session: `ViewportResize` carries no surface
    /// identity, and the bridge applies it to the session's output. Debouncing
    /// it per window would turn one drag into a burst of session-wide
    /// resizes.
    resize: ResizeDebounce,
}

impl ViewerSession {
    pub fn new(factory: WindowFactory) -> Self {
        Self {
            factory,
            windows: BTreeMap::new(),
            huds: BTreeMap::new(),
            ignored: BTreeSet::new(),
            resize: ResizeDebounce::default(),
        }
    }

    /// Number of windows currently on screen.
    pub fn open_windows(&self) -> usize {
        self.windows.len()
    }

    pub fn is_open(&self, stream_id: u64) -> bool {
        self.windows.contains_key(&stream_id)
    }

    /// A stream's last-computed HUD sample, if it has an open window.
    ///
    /// Test-only: it exists so a test can observe `refresh_hud`'s effect
    /// directly, rather than by capturing `tracing` output, which is
    /// unreliable across a parallel test binary (a callsite's interest gets
    /// cached process-wide the first time *any* test — with or without a
    /// subscriber installed — reaches it).
    #[cfg(test)]
    fn hud_snapshot(&self, stream_id: u64) -> Option<HudSample> {
        self.windows.get(&stream_id).map(|stream| stream.hud)
    }

    /// Applies one event from the media client.
    pub fn handle(&mut self, event: StreamEvent, now: Instant) {
        match event {
            // Accounting for a stream nobody is watching any more is dropped
            // rather than accumulated: a late packet for an ended stream must
            // not resurrect its bookkeeping.
            StreamEvent::Packet(packet) if !self.ignored.contains(&packet.stream_id) => self
                .huds
                .entry(packet.stream_id)
                .or_default()
                .record_packet(now, &packet),
            StreamEvent::Packet(_) => {}
            StreamEvent::Frame(frame) => self.present(frame, now),
            StreamEvent::Ended { stream_id } => self.end(stream_id),
            // The client has already asked the bridge for a fresh keyframe;
            // this stream's window keeps its last picture until one arrives,
            // and every other window is untouched.
            StreamEvent::DecodeFailed { stream_id } => {
                tracing::warn!(stream_id, "decode failed; awaiting a fresh keyframe");
            }
        }
    }

    /// Drains every window's input, refreshes each stream's HUD on its own
    /// clock, and reports what should go on the wire.
    pub fn poll(&mut self, now: Instant) -> Vec<MediaInput> {
        let mut inputs = Vec::new();
        let mut resized = Vec::new();
        let mut dismissed = Vec::new();
        for (stream_id, stream) in &mut self.windows {
            for event in stream.window.poll_events() {
                match event {
                    WindowEvent::Resized { width, height } => resized.push((width, height)),
                    WindowEvent::CloseRequested => dismissed.push(*stream_id),
                    other => inputs.extend(to_input(stream.client_id, stream.surface_id, other)),
                }
            }
        }
        // The HUD's ~1Hz log line is the evidence path for a stalling
        // stream, so it must fire on a time boundary rather than only when a
        // frame arrives — see `refresh_hud`. Checked on every poll tick;
        // `refresh_hud` itself is a no-op until the interval has elapsed.
        let stream_ids: Vec<u64> = self.windows.keys().copied().collect();
        for stream_id in stream_ids {
            self.refresh_hud(stream_id, now);
        }
        // Last writer wins: there is one viewport, so if several windows were
        // resized in the same pass the most recent size is the one that
        // eventually goes out.
        for (width, height) in resized {
            self.resize.observe(width, height, now);
        }
        for stream_id in dismissed {
            inputs.extend(self.dismiss(stream_id));
        }
        inputs.extend(self.resize.take_due(now));
        inputs
    }

    fn present(&mut self, frame: StreamFrame, now: Instant) {
        let stream_id = frame.stream_id;
        if self.ignored.contains(&stream_id) {
            return;
        }
        // A stream this session has not seen before is a toplevel that just
        // appeared, and gets a window of its own.
        if !self.windows.contains_key(&stream_id) && !self.open(&frame) {
            return;
        }
        let Some(stream) = self.windows.get_mut(&stream_id) else {
            return;
        };
        // A stream can be reconfigured onto a different surface; the window's
        // identity follows the frames it is actually showing.
        stream.client_id = frame.client_id;
        stream.surface_id = frame.surface_id;

        self.huds.entry(stream_id).or_default().record_frame(now);
        self.refresh_hud(stream_id, now);

        let Some(stream) = self.windows.get_mut(&stream_id) else {
            return;
        };
        stream.window.present(&frame.frame, &stream.hud);
    }

    /// Recomputes and logs a stream's HUD figures, if the interval has
    /// elapsed since the last time this ran for it.
    ///
    /// `present` and `poll` both drive this on the same clock: `present`
    /// keeps the on-screen overlay reasonably fresh whenever a frame
    /// happens to land, and `poll` — which runs on a timer regardless of
    /// whether frames are arriving — is what keeps the ~1Hz `tracing::info!`
    /// line firing. That line is the evidence path for a stalling stream,
    /// and the one scenario where it actually matters is a stream stalling
    /// *because* the hub is dropping its packets, which is exactly the
    /// scenario where frames stop arriving. Gating this only on frame
    /// arrival, as it used to be, made the diagnostic go dark right when it
    /// was needed.
    fn refresh_hud(&mut self, stream_id: u64, now: Instant) {
        let due = self.windows.get(&stream_id).is_some_and(|stream| {
            stream
                .hud_computed
                .is_none_or(|at| now.saturating_duration_since(at) >= HUD_INTERVAL)
        });
        if !due {
            return;
        }
        let Some(sample) = self.huds.get_mut(&stream_id).map(|hud| hud.sample(now)) else {
            return;
        };
        if let Some(stream) = self.windows.get_mut(&stream_id) {
            stream.hud = sample;
            stream.hud_computed = Some(now);
            tracing::info!(stream_id, hud = %sample, "stream performance");
        }
    }

    /// Opens a window for a stream's first frame. A failure is reported and
    /// not fatal: the next frame tries again, and every other window carries
    /// on regardless.
    fn open(&mut self, frame: &StreamFrame) -> bool {
        let spec = WindowSpec {
            stream_id: frame.stream_id,
            width: frame.frame.width,
            height: frame.frame.height,
        };
        match (self.factory)(&spec) {
            Ok(window) => {
                tracing::info!(
                    stream_id = spec.stream_id,
                    client_id = frame.client_id,
                    surface_id = frame.surface_id,
                    width = spec.width,
                    height = spec.height,
                    "opened a window for a new toplevel"
                );
                self.windows.insert(
                    spec.stream_id,
                    StreamWindow {
                        client_id: frame.client_id,
                        surface_id: frame.surface_id,
                        window,
                        hud: HudSample::default(),
                        hud_computed: None,
                    },
                );
                true
            }
            Err(error) => {
                tracing::error!(stream_id = spec.stream_id, %error, "failed to open a window");
                false
            }
        }
    }

    /// The toplevel closed: its window goes away and its accounting with it.
    ///
    /// No release synthesis is needed here the way `dismiss` needs it: the
    /// application itself is gone, so there is nothing left in the guest to
    /// leave a key stuck down in.
    fn end(&mut self, stream_id: u64) {
        if let Some(mut stream) = self.windows.remove(&stream_id) {
            stream.window.close();
            tracing::info!(stream_id, "stream ended; window closed");
        }
        self.huds.remove(&stream_id);
        self.ignored.insert(stream_id);
    }

    /// The user closed the window while the toplevel is still alive. Frames
    /// keep arriving until the application itself goes away; they are dropped
    /// rather than allowed to pop the window back open.
    ///
    /// The stream stays attached — only the window closes — so the bridge's
    /// per-attachment release-on-detach safety net never fires here: there is
    /// no detach. Anything the window still considers held is released
    /// explicitly instead, addressed to the surface it was actually held on.
    fn dismiss(&mut self, stream_id: u64) -> Vec<MediaInput> {
        let mut inputs = Vec::new();
        if let Some(mut stream) = self.windows.remove(&stream_id) {
            for event in stream.window.close() {
                inputs.extend(to_input(stream.client_id, stream.surface_id, event));
            }
        }
        self.ignored.insert(stream_id);
        tracing::info!(stream_id, "window dismissed; ignoring further frames");
        inputs
    }
}

/// Translates one window event into the input message for that window's own
/// surface.
///
/// The identity is a parameter rather than something read back out of the
/// event, so there is no path by which one window's event can be addressed to
/// another window's surface.
fn to_input(client_id: u64, surface_id: u64, event: WindowEvent) -> Option<MediaInput> {
    match event {
        WindowEvent::PointerMotion { x, y } => Some(MediaInput::PointerMotion {
            client_id,
            surface_id,
            x,
            y,
        }),
        WindowEvent::PointerButton { button, pressed } => Some(MediaInput::PointerButton {
            client_id,
            surface_id,
            button,
            pressed,
        }),
        WindowEvent::PointerAxis {
            horizontal,
            vertical,
        } => Some(MediaInput::PointerAxis {
            client_id,
            surface_id,
            horizontal,
            vertical,
        }),
        WindowEvent::Key { keycode, pressed } => Some(MediaInput::KeyboardKey {
            client_id,
            surface_id,
            keycode,
            pressed,
        }),
        WindowEvent::Modifiers(Modifiers {
            ctrl,
            alt,
            shift,
            caps_lock,
            logo,
            num_lock,
            layout_index,
        }) => Some(MediaInput::KeyboardModifiers {
            client_id,
            surface_id,
            ctrl,
            alt,
            shift,
            caps_lock,
            logo,
            num_lock,
            layout_index,
        }),
        // A resize is session-wide, not per surface, and closing a window is
        // purely local; neither is translated here.
        WindowEvent::Resized { .. } | WindowEvent::CloseRequested => None,
    }
}

/// Collapses a run of window resizes into one clamped report.
#[derive(Debug, Default)]
struct ResizeDebounce {
    pending: Option<Pending>,
    last_sent: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    size: (u32, u32),
    since: Instant,
}

impl ResizeDebounce {
    /// Notes the newest size. Each new size restarts the quiet period, so a
    /// drag reports once it settles rather than at every step.
    fn observe(&mut self, width: u32, height: u32, now: Instant) {
        self.pending = Some(Pending {
            size: clamp_viewport(width, height),
            since: now,
        });
    }

    /// Reports the settled size, once, if it differs from what was last sent.
    fn take_due(&mut self, now: Instant) -> Option<MediaInput> {
        let pending = self.pending?;
        if now.saturating_duration_since(pending.since) < RESIZE_DEBOUNCE {
            return None;
        }
        self.pending = None;
        if self.last_sent == Some(pending.size) {
            return None;
        }
        self.last_sent = Some(pending.size);
        Some(MediaInput::ViewportResize {
            width: pending.size.0,
            height: pending.size.1,
        })
    }
}

/// Holds a requested viewport inside the bounds the bridge accepts.
///
/// The server validates this too, but a message it is certain to reject is
/// one worth not sending: a window dragged tiny would otherwise turn every
/// resize into a protocol error on the socket.
fn clamp_viewport(width: u32, height: u32) -> (u32, u32) {
    (
        width.clamp(MIN_VIEWPORT.0, MAX_VIEWPORT.0),
        height.clamp(MIN_VIEWPORT.1, MAX_VIEWPORT.1),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use navette_protocol::media::{
        MediaFlags, MediaHeader, MediaKind, MediaPacket, StreamConfig as WireStreamConfig,
    };

    use super::*;
    use crate::decoder::{DecodedFrame, Decoder, DecoderConfig, DecoderError, DecoderMetrics};
    use crate::router::{StreamPacket, StreamRouter};
    use crate::window::{WindowError, WindowRecorder};

    const BTN_LEFT: u32 = 0x110;
    const KEY_A: u32 = 30;
    const CODEC_CONFIG: [u8; 5] = [0, 0, 0, 1, 0x67];

    /// Hands out [`WindowRecorder`]s and keeps them addressable by stream, so
    /// a test can drive and inspect a window the session owns.
    #[derive(Clone, Default)]
    struct Desktop {
        recorders: Rc<RefCell<BTreeMap<u64, WindowRecorder>>>,
        refuse: Rc<RefCell<BTreeSet<u64>>>,
    }

    impl Desktop {
        fn factory(&self) -> WindowFactory {
            let recorders = Rc::clone(&self.recorders);
            let refuse = Rc::clone(&self.refuse);
            Box::new(move |spec: &WindowSpec| {
                if refuse.borrow().contains(&spec.stream_id) {
                    return Err(WindowError::Open("no display".to_string()));
                }
                let recorder = WindowRecorder::new();
                let window = recorder.window(spec.clone());
                recorders.borrow_mut().insert(spec.stream_id, recorder);
                Ok(Box::new(window) as Box<dyn Window>)
            })
        }

        fn window(&self, stream_id: u64) -> WindowRecorder {
            self.recorders
                .borrow()
                .get(&stream_id)
                .cloned()
                .unwrap_or_else(|| panic!("no window was opened for stream {stream_id}"))
        }

        fn opened(&self) -> Vec<u64> {
            self.recorders.borrow().keys().copied().collect()
        }
    }

    fn frame(stream_id: u64, client_id: u64, surface_id: u64, shade: u8) -> StreamEvent {
        StreamEvent::Frame(StreamFrame {
            stream_id,
            client_id,
            surface_id,
            timestamp_us: 0,
            frame: DecodedFrame {
                width: 320,
                height: 240,
                pixels: vec![shade; 320 * 240 * 4],
                decode_time: Duration::ZERO,
            },
        })
    }

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn identity_of(input: &MediaInput) -> Option<(u64, u64)> {
        match input {
            MediaInput::PointerMotion {
                client_id,
                surface_id,
                ..
            }
            | MediaInput::PointerButton {
                client_id,
                surface_id,
                ..
            }
            | MediaInput::PointerAxis {
                client_id,
                surface_id,
                ..
            }
            | MediaInput::KeyboardKey {
                client_id,
                surface_id,
                ..
            }
            | MediaInput::KeyboardModifiers {
                client_id,
                surface_id,
                ..
            } => Some((*client_id, *surface_id)),
            MediaInput::ViewportResize { .. }
            | MediaInput::RequestKeyframe
            | MediaInput::Ping { .. }
            | MediaInput::SetClipboard { .. } => None,
        }
    }

    #[test]
    fn each_toplevel_stream_gets_its_own_window() {
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();

        session.handle(frame(1, 11, 12, 1), base);
        session.handle(frame(2, 21, 22, 2), base);
        // A second frame on a known stream reuses that stream's window rather
        // than opening another.
        session.handle(frame(1, 11, 12, 3), at(base, 33));

        assert_eq!(desktop.opened(), vec![1, 2]);
        assert_eq!(session.open_windows(), 2);
        assert_eq!(desktop.window(1).presented().len(), 2);
        assert_eq!(desktop.window(2).presented().len(), 1);
        assert_eq!(
            desktop.window(2).spec(),
            Some(WindowSpec {
                stream_id: 2,
                width: 320,
                height: 240
            })
        );
    }

    #[test]
    fn a_window_that_cannot_be_opened_does_not_take_the_session_down() {
        let desktop = Desktop::default();
        desktop.refuse.borrow_mut().insert(1);
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();

        session.handle(frame(1, 11, 12, 1), base);
        assert_eq!(session.open_windows(), 0);

        session.handle(frame(2, 21, 22, 2), base);
        assert_eq!(session.open_windows(), 1);
        assert!(session.is_open(2));
    }

    #[test]
    fn two_streams_input_never_crosses_between_their_windows() {
        // The identities are deliberately confusable — stream A is client 11 /
        // surface 12 and stream B is client 12 / surface 11 — so a
        // translation that reached for the wrong window's field, or swapped
        // the two, produces an identity that still exists in the session and
        // would slip past a test using obviously distinct numbers.
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);
        session.handle(frame(2, 12, 11, 2), base);

        // Different event kinds on each window in the same poll, so a mixup
        // cannot hide behind two identical-looking messages.
        desktop
            .window(1)
            .inject(WindowEvent::PointerMotion { x: 5.0, y: 6.0 });
        desktop.window(2).inject(WindowEvent::Key {
            keycode: KEY_A,
            pressed: true,
        });
        desktop.window(2).inject(WindowEvent::PointerButton {
            button: BTN_LEFT,
            pressed: true,
        });

        let inputs = session.poll(base);
        assert_eq!(inputs.len(), 3);
        for input in &inputs {
            let identity = identity_of(input).expect("every one of these carries an identity");
            let expected = match input {
                MediaInput::PointerMotion { .. } => (11, 12),
                _ => (12, 11),
            };
            assert_eq!(
                identity, expected,
                "{input:?} was addressed to the wrong surface"
            );
        }

        // Windows only ever report their own stream's identity, so no message
        // can carry a pairing that belongs to neither window.
        let seen: BTreeSet<(u64, u64)> = inputs.iter().filter_map(identity_of).collect();
        assert_eq!(seen, BTreeSet::from([(11, 12), (12, 11)]));
    }

    #[test]
    fn a_burst_of_resizes_collapses_into_one_report() {
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);

        // Sixty resize events over a 600ms drag, polled after each one.
        let mut sent = Vec::new();
        for step in 0..60 {
            desktop.window(1).inject(WindowEvent::Resized {
                width: 640 + step * 4,
                height: 480 + step * 2,
            });
            sent.extend(session.poll(at(base, step as u64 * 10)));
        }
        assert!(
            sent.is_empty(),
            "nothing should be reported while the drag is still moving, got {sent:?}"
        );

        // The drag settles; one message goes out, carrying the final size.
        sent.extend(session.poll(at(base, 590 + RESIZE_DEBOUNCE.as_millis() as u64)));
        assert_eq!(
            sent,
            vec![MediaInput::ViewportResize {
                width: 640 + 59 * 4,
                height: 480 + 59 * 2,
            }]
        );

        // Polling again does not repeat it, and neither does re-observing the
        // size the window already settled on.
        assert!(session.poll(at(base, 2000)).is_empty());
        desktop.window(1).inject(WindowEvent::Resized {
            width: 640 + 59 * 4,
            height: 480 + 59 * 2,
        });
        session.poll(at(base, 2000));
        assert!(session.poll(at(base, 3000)).is_empty());
    }

    #[test]
    fn a_resize_burst_across_two_windows_still_reports_one_session_viewport() {
        // `ViewportResize` carries no surface identity: the bridge applies it
        // to the session's whole output. Debouncing per window would let two
        // windows resized together produce two session-wide resizes.
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);
        session.handle(frame(2, 21, 22, 2), base);

        for step in 0..10 {
            desktop.window(1).inject(WindowEvent::Resized {
                width: 800 + step,
                height: 600,
            });
            desktop.window(2).inject(WindowEvent::Resized {
                width: 1024,
                height: 768 + step,
            });
            assert!(session.poll(at(base, step as u64 * 10)).is_empty());
        }

        let settled = session.poll(at(base, 90 + RESIZE_DEBOUNCE.as_millis() as u64));
        assert_eq!(
            settled.len(),
            1,
            "expected one session resize, got {settled:?}"
        );
    }

    #[test]
    fn resizes_are_clamped_to_the_bounds_the_bridge_accepts() {
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);

        desktop.window(1).inject(WindowEvent::Resized {
            width: 1,
            height: 1,
        });
        session.poll(base);
        let clamped = session.poll(at(base, 500));
        assert_eq!(
            clamped,
            vec![MediaInput::ViewportResize {
                width: MIN_VIEWPORT.0,
                height: MIN_VIEWPORT.1
            }]
        );

        desktop.window(1).inject(WindowEvent::Resized {
            width: 100_000,
            height: 100_000,
        });
        session.poll(at(base, 600));
        let clamped = session.poll(at(base, 1100));
        assert_eq!(
            clamped,
            vec![MediaInput::ViewportResize {
                width: MAX_VIEWPORT.0,
                height: MAX_VIEWPORT.1
            }]
        );
        // Clamping is what keeps the wire clean: both messages are messages
        // the server would have accepted.
        for input in clamped {
            assert!(input.validate().is_ok());
        }
    }

    #[test]
    fn a_stream_ending_closes_only_its_own_window() {
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);
        session.handle(frame(2, 21, 22, 2), base);

        session.handle(StreamEvent::Ended { stream_id: 1 }, at(base, 10));
        assert!(desktop.window(1).is_closed());
        assert!(!desktop.window(2).is_closed());
        assert_eq!(session.open_windows(), 1);

        // Late frames for the ended stream are dropped rather than reopening
        // its window, while the surviving stream keeps drawing.
        let before = desktop.window(1).presented().len();
        session.handle(frame(1, 11, 12, 9), at(base, 20));
        session.handle(frame(2, 21, 22, 9), at(base, 20));
        assert_eq!(desktop.window(1).presented().len(), before);
        assert_eq!(desktop.window(2).presented().len(), 2);
        assert_eq!(session.open_windows(), 1);

        // Its input is gone with it: a stale event on the closed window
        // produces nothing.
        desktop
            .window(1)
            .inject(WindowEvent::PointerMotion { x: 1.0, y: 1.0 });
        assert!(session.poll(at(base, 30)).is_empty());
    }

    #[test]
    fn closing_a_window_stops_its_stream_without_disturbing_the_others() {
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);
        session.handle(frame(2, 21, 22, 2), base);

        desktop.window(1).inject(WindowEvent::CloseRequested);
        assert!(session.poll(at(base, 10)).is_empty());
        assert!(desktop.window(1).is_closed());
        assert_eq!(session.open_windows(), 1);

        // The toplevel is still alive and still sending, but the window the
        // user closed stays closed instead of popping back open.
        session.handle(frame(1, 11, 12, 3), at(base, 20));
        assert_eq!(session.open_windows(), 1);
        assert_eq!(desktop.window(1).presented().len(), 1);

        // The surviving window is untouched throughout, and still sends its
        // own input.
        session.handle(frame(2, 21, 22, 3), at(base, 20));
        assert_eq!(desktop.window(2).presented().len(), 2);
        desktop.window(2).inject(WindowEvent::Key {
            keycode: KEY_A,
            pressed: false,
        });
        assert_eq!(
            session.poll(at(base, 30)),
            vec![MediaInput::KeyboardKey {
                client_id: 21,
                surface_id: 22,
                keycode: KEY_A,
                pressed: false,
            }]
        );
    }

    #[test]
    fn dismissing_a_window_synthesizes_releases_for_whatever_it_still_holds() {
        // The stream stays attached when only the window closes, so the
        // bridge's per-attachment release-on-detach backstop never fires:
        // there is no detach. A key or button still held at close time must
        // be released explicitly instead, or it stays stuck down in the
        // guest for the rest of the session.
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);

        desktop.window(1).inject(WindowEvent::Key {
            keycode: KEY_A,
            pressed: true,
        });
        desktop.window(1).inject(WindowEvent::PointerButton {
            button: BTN_LEFT,
            pressed: true,
        });
        desktop.window(1).inject(WindowEvent::CloseRequested);

        let inputs = session.poll(at(base, 10));
        assert!(desktop.window(1).is_closed());
        assert_eq!(session.open_windows(), 0);
        assert_eq!(
            inputs,
            vec![
                // The press events themselves were real input and are
                // forwarded like any other, ahead of the synthesized
                // releases `close` produces once the window actually shuts.
                MediaInput::KeyboardKey {
                    client_id: 11,
                    surface_id: 12,
                    keycode: KEY_A,
                    pressed: true,
                },
                MediaInput::PointerButton {
                    client_id: 11,
                    surface_id: 12,
                    button: BTN_LEFT,
                    pressed: true,
                },
                MediaInput::KeyboardKey {
                    client_id: 11,
                    surface_id: 12,
                    keycode: KEY_A,
                    pressed: false,
                },
                MediaInput::PointerButton {
                    client_id: 11,
                    surface_id: 12,
                    button: BTN_LEFT,
                    pressed: false,
                },
            ]
        );
    }

    #[test]
    fn a_dismissed_windows_synthesized_release_is_addressed_to_its_own_surface_only() {
        // `dismiss` is a new translation call site distinct from `poll`'s own
        // loop, so the identity-never-crosses guarantee has to be reproven
        // here specifically rather than assumed from the general-purpose
        // test above. Same deliberately confusable identities: stream A is
        // client 11 / surface 12, stream B is client 12 / surface 11.
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);
        session.handle(frame(2, 12, 11, 2), base);

        desktop.window(1).inject(WindowEvent::Key {
            keycode: KEY_A,
            pressed: true,
        });
        desktop.window(1).inject(WindowEvent::CloseRequested);

        let inputs = session.poll(at(base, 10));
        assert!(desktop.window(1).is_closed());
        assert!(!desktop.window(2).is_closed());
        assert_eq!(session.open_windows(), 1);
        assert!(session.is_open(2));

        let release = inputs
            .iter()
            .find(|input| matches!(input, MediaInput::KeyboardKey { pressed: false, .. }))
            .expect("the still-held key must be released on dismissal");
        assert_eq!(
            identity_of(release),
            Some((11, 12)),
            "the synthesized release must be addressed to window 1's own surface, \
             never to the confusable identity window 2 happens to carry"
        );
    }

    #[test]
    fn a_key_released_before_dismissal_is_not_released_a_second_time() {
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();
        session.handle(frame(1, 11, 12, 1), base);

        desktop.window(1).inject(WindowEvent::Key {
            keycode: KEY_A,
            pressed: true,
        });
        // Drained (and observed as held) before the release and the close
        // request arrive.
        session.poll(base);
        desktop.window(1).inject(WindowEvent::Key {
            keycode: KEY_A,
            pressed: false,
        });
        desktop.window(1).inject(WindowEvent::CloseRequested);

        let inputs = session.poll(at(base, 10));
        assert_eq!(
            inputs,
            vec![MediaInput::KeyboardKey {
                client_id: 11,
                surface_id: 12,
                keycode: KEY_A,
                pressed: false,
            }],
            "the explicit release must not be followed by a synthesized duplicate"
        );
    }

    /// Decodes only access units that start with an Annex-B start code, and
    /// reports a hard failure on anything else — the way a real decoder
    /// reacts to a corrupt stream.
    struct PickyDecoder {
        config: DecoderConfig,
    }

    impl Decoder for PickyDecoder {
        fn config(&self) -> &DecoderConfig {
            &self.config
        }

        fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<DecodedFrame>, DecoderError> {
            if !access_unit.starts_with(&[0, 0, 0, 1]) {
                return Err(DecoderError::ProcessExited);
            }
            Ok(vec![DecodedFrame {
                width: self.config.width,
                height: self.config.height,
                pixels: vec![0; self.config.frame_len()],
                decode_time: Duration::from_millis(3),
            }])
        }

        fn drain(&mut self) -> Vec<DecodedFrame> {
            Vec::new()
        }

        fn reconfigure(&mut self, config: DecoderConfig) -> Result<(), DecoderError> {
            self.config = config.validate()?;
            Ok(())
        }

        fn metrics(&self) -> DecoderMetrics {
            DecoderMetrics::default()
        }
    }

    fn wire_packet(
        kind: MediaKind,
        stream_id: u64,
        sequence: u64,
        payload: Vec<u8>,
    ) -> MediaPacket {
        MediaPacket::new(
            MediaHeader {
                kind,
                flags: MediaFlags::new(kind == MediaKind::Video, false),
                stream_id,
                sequence,
                timestamp_us: sequence * 1000,
                payload_len: 0,
                width: 320,
                height: 240,
            },
            payload,
        )
        .expect("fixture packet is within protocol bounds")
    }

    fn wire_config(stream_id: u64, sequence: u64, client_id: u64, surface_id: u64) -> MediaPacket {
        wire_packet(
            MediaKind::StreamConfig,
            stream_id,
            sequence,
            WireStreamConfig {
                client_id,
                surface_id,
                codec_config: CODEC_CONFIG.to_vec(),
            }
            .encode()
            .expect("fixture configuration is within protocol bounds"),
        )
    }

    #[test]
    fn garbage_on_the_wire_does_not_take_down_a_window_or_its_neighbours() {
        // Driven through the real router so this exercises what actually
        // happens when a corrupt access unit reaches a decoder, rather than a
        // hand-made `DecodeFailed`.
        let mut router = StreamRouter::new(Box::new(|config: &DecoderConfig| {
            Ok(Box::new(PickyDecoder {
                config: config.clone().validate()?,
            }) as Box<dyn Decoder>)
        }));
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();

        for packet in [
            wire_config(1, 1, 11, 12),
            wire_config(2, 1, 21, 22),
            wire_packet(MediaKind::Video, 1, 2, vec![0, 0, 0, 1, 0x65]),
            wire_packet(MediaKind::Video, 2, 2, vec![0, 0, 0, 1, 0x65]),
            // Corrupt: neither a start code nor anything a decoder can use.
            wire_packet(MediaKind::Video, 1, 3, vec![0xde, 0xad, 0xbe, 0xef]),
            // A truncated payload the protocol accepts but the codec cannot.
            wire_packet(MediaKind::Video, 1, 4, vec![0x41]),
            wire_packet(MediaKind::Video, 2, 3, vec![0, 0, 0, 1, 0x41]),
        ] {
            for event in router.handle(&packet) {
                session.handle(event, base);
            }
        }

        // The failed stream keeps its window and its last good picture; the
        // healthy stream is entirely unaffected.
        assert_eq!(session.open_windows(), 2);
        assert!(!desktop.window(1).is_closed());
        assert_eq!(desktop.window(1).presented().len(), 1);
        assert_eq!(desktop.window(2).presented().len(), 2);

        // Input still flows from the surviving window.
        desktop.window(2).inject(WindowEvent::Key {
            keycode: KEY_A,
            pressed: true,
        });
        assert_eq!(
            session.poll(at(base, 10)),
            vec![MediaInput::KeyboardKey {
                client_id: 21,
                surface_id: 22,
                keycode: KEY_A,
                pressed: true,
            }]
        );

        // And the stream recovers when the bridge answers with a fresh
        // configuration and keyframe, reusing the window that stayed open.
        for packet in [
            wire_config(1, 5, 11, 12),
            wire_packet(MediaKind::Video, 1, 6, vec![0, 0, 0, 1, 0x65]),
        ] {
            for event in router.handle(&packet) {
                session.handle(event, at(base, 20));
            }
        }
        assert_eq!(desktop.window(1).presented().len(), 2);
        assert_eq!(desktop.opened(), vec![1, 2]);
    }

    #[test]
    fn the_hud_reaches_the_window_and_is_recomputed_about_once_a_second() {
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();

        session.handle(
            StreamEvent::Packet(StreamPacket {
                stream_id: 1,
                kind: MediaKind::Video,
                sequence: 1,
                wire_bytes: 10_000,
                discontinuity: false,
                decoder: Some(DecoderMetrics {
                    last_decode_time: Duration::from_millis(3),
                    ..DecoderMetrics::default()
                }),
            }),
            base,
        );
        for step in 0..30 {
            session.handle(frame(1, 11, 12, step as u8), at(base, step * 33));
        }

        let first = desktop
            .window(1)
            .presented()
            .first()
            .cloned()
            .expect("the first frame opens the window and is drawn");
        // The very first sample spans no time at all, so it reads zero rather
        // than dividing by an empty window.
        assert_eq!(first.hud.fps, 0.0);
        assert_eq!(first.hud.decode_time, Duration::from_millis(3));

        // Every frame inside the first second reuses that sample; the figures
        // are recomputed once the interval elapses.
        let drawn = desktop.window(1).presented();
        assert!(
            drawn.iter().take(30).all(|shown| shown.hud == first.hud),
            "the HUD must not be recomputed on every frame"
        );

        session.handle(frame(1, 11, 12, 99), at(base, 1000));
        let latest = desktop
            .window(1)
            .last_presented()
            .expect("a frame was just drawn");
        assert!(
            (latest.hud.fps - 30.0).abs() < 2.0,
            "expected ~30 fps after a second of frames, got {}",
            latest.hud.fps
        );
    }

    #[test]
    fn the_hud_refresh_runs_on_pollings_own_clock_not_only_when_a_frame_arrives() {
        // `refresh_hud` feeds both the on-screen overlay and the ~1Hz
        // `tracing::info!` line, and the one scenario where that log line
        // actually matters is a stream stalling *because* the hub is
        // dropping its packets — exactly the scenario in which frames stop
        // arriving. So the recompute it performs must happen on a time
        // boundary `poll` can reach on its own, not only when `present` is
        // called from a frame arrival.
        let desktop = Desktop::default();
        let mut session = ViewerSession::new(desktop.factory());
        let base = Instant::now();

        session.handle(frame(1, 11, 12, 1), base);
        assert_eq!(
            session.hud_snapshot(1).map(|hud| hud.dropped_packets),
            Some(0),
            "no packets have been observed yet"
        );

        // The stream stalls here: only packets arrive from now on, no more
        // frames. Three baseline packets establish the sequence, then a real
        // gap of six.
        for sequence in 1..=3u64 {
            session.handle(
                StreamEvent::Packet(StreamPacket {
                    stream_id: 1,
                    kind: MediaKind::Video,
                    sequence,
                    wire_bytes: 1_000,
                    discontinuity: false,
                    decoder: None,
                }),
                at(base, 100),
            );
        }
        session.handle(
            StreamEvent::Packet(StreamPacket {
                stream_id: 1,
                kind: MediaKind::Video,
                sequence: 10,
                wire_bytes: 1_000,
                discontinuity: false,
                decoder: None,
            }),
            at(base, 200),
        );

        // Before the HUD interval elapses, polling must not force an early
        // recompute.
        session.poll(at(base, 500));
        assert_eq!(
            session.hud_snapshot(1).map(|hud| hud.dropped_packets),
            Some(0),
            "the HUD must not be refreshed before its interval elapses"
        );

        // Once the interval elapses, `poll` alone — with no new frame in
        // between — refreshes it.
        session.poll(at(base, 1_100));
        assert_eq!(
            session.hud_snapshot(1).map(|hud| hud.dropped_packets),
            Some(6),
            "poll() must refresh the HUD on its own clock, not only when a frame arrives"
        );
    }
}
