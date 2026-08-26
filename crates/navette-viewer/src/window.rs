//! Window and input adapters.
//!
//! The viewer opens one native window per active toplevel stream, which is
//! Navette's whole premise: applications arrive as individual windows on the
//! local desktop, not as a mirror of a remote one. Windows sit behind a trait
//! so the wiring between decoded frames, input, and the media socket can be
//! driven headlessly in CI, where no display exists.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;

use thiserror::Error;

use crate::decoder::DecodedFrame;
use crate::hud::HudSample;

/// Keyboard modifier state, as the media protocol carries it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub caps_lock: bool,
    pub logo: bool,
    pub num_lock: bool,
    pub layout_index: u32,
}

/// Something the user did to one window.
///
/// Deliberately identity-free: an event says what happened, never to which
/// surface. The stream that owns the window supplies the identity, so a
/// translation bug cannot address another stream's surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowEvent {
    /// Pointer position in window-local pixels.
    PointerMotion {
        x: f64,
        y: f64,
    },
    /// Button press or release, as an evdev `BTN_*` code.
    PointerButton {
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        horizontal: f64,
        vertical: f64,
    },
    /// Key press or release, as an evdev `KEY_*` code.
    Key {
        keycode: u32,
        pressed: bool,
    },
    Modifiers(Modifiers),
    Resized {
        width: u32,
        height: u32,
    },
    CloseRequested,
}

/// What a window is opened for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowSpec {
    pub stream_id: u64,
    pub width: u32,
    pub height: u32,
}

impl WindowSpec {
    /// The window title. Streams carry no application name yet, so the stream
    /// is the only thing there is to name it after.
    pub fn title(&self) -> String {
        format!("navette stream {}", self.stream_id)
    }
}

pub trait Window {
    /// Draws one decoded picture, with the stream's current performance
    /// figures. Whether and how those figures are rendered is the window's
    /// business; computing them is not.
    fn present(&mut self, frame: &DecodedFrame, hud: &HudSample);

    /// Drains everything the user has done since the last call.
    fn poll_events(&mut self) -> Vec<WindowEvent>;

    /// Closes the window and returns synthetic release events for any key or
    /// pointer button this window still considers held.
    ///
    /// A window can be closed while its stream stays attached (the user
    /// closed it, but the toplevel it belongs to is still running), so there
    /// is no `detach` for a release-on-detach safety net elsewhere to catch
    /// this on. Without a synthesized release here, a key held at close time
    /// stays down in the guest for the rest of the session.
    fn close(&mut self) -> Vec<WindowEvent>;
}

/// Opens a window for a newly seen stream. The real viewer hands back a
/// native window; tests hand back a [`RecordingWindow`].
pub type WindowFactory = Box<dyn FnMut(&WindowSpec) -> Result<Box<dyn Window>, WindowError>>;

#[derive(Debug, Error)]
pub enum WindowError {
    #[error("failed to open a window: {0}")]
    Open(String),
}

/// One `present` call, as [`RecordingWindow`] saw it.
#[derive(Clone, Debug, PartialEq)]
pub struct Presented {
    pub frame: DecodedFrame,
    pub hud: HudSample,
}

/// Test-side handle on a [`RecordingWindow`].
///
/// The session owns the window itself, so a test needs a second handle to feed
/// it synthetic events and to read back what it was asked to draw. Cloning a
/// recorder shares one window's state; call [`WindowRecorder::window`] once per
/// recorder.
#[derive(Clone, Debug, Default)]
pub struct WindowRecorder {
    state: Rc<RefCell<RecorderState>>,
}

#[derive(Debug, Default)]
struct RecorderState {
    spec: Option<WindowSpec>,
    presented: Vec<Presented>,
    pending: VecDeque<WindowEvent>,
    closed: bool,
    /// Keys and pointer buttons this window believes are currently held,
    /// tracked from the press/release events the last `poll_events` drained.
    /// Mirrors what a real window derives from live device state, so tests
    /// can exercise `close`'s release synthesis without a display.
    held_keys: BTreeSet<u32>,
    held_buttons: BTreeSet<u32>,
}

impl WindowRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the window this recorder observes.
    pub fn window(&self, spec: WindowSpec) -> RecordingWindow {
        self.state.borrow_mut().spec = Some(spec);
        RecordingWindow {
            state: Rc::clone(&self.state),
        }
    }

    /// Queues an event for the window's next `poll_events`.
    pub fn inject(&self, event: WindowEvent) {
        self.state.borrow_mut().pending.push_back(event);
    }

    /// Everything the window has been asked to draw, oldest first.
    pub fn presented(&self) -> Vec<Presented> {
        self.state.borrow().presented.clone()
    }

    /// The most recent picture drawn, if any.
    pub fn last_presented(&self) -> Option<Presented> {
        self.state.borrow().presented.last().cloned()
    }

    pub fn is_closed(&self) -> bool {
        self.state.borrow().closed
    }

    pub fn spec(&self) -> Option<WindowSpec> {
        self.state.borrow().spec.clone()
    }
}

/// A window that draws nowhere and replays events a test injected.
pub struct RecordingWindow {
    state: Rc<RefCell<RecorderState>>,
}

impl Window for RecordingWindow {
    fn present(&mut self, frame: &DecodedFrame, hud: &HudSample) {
        let mut state = self.state.borrow_mut();
        if state.closed {
            return;
        }
        state.presented.push(Presented {
            frame: frame.clone(),
            hud: *hud,
        });
    }

    fn poll_events(&mut self) -> Vec<WindowEvent> {
        let mut state = self.state.borrow_mut();
        let events: Vec<WindowEvent> = state.pending.drain(..).collect();
        for event in &events {
            match *event {
                WindowEvent::Key {
                    keycode,
                    pressed: true,
                } => {
                    state.held_keys.insert(keycode);
                }
                WindowEvent::Key {
                    keycode,
                    pressed: false,
                } => {
                    state.held_keys.remove(&keycode);
                }
                WindowEvent::PointerButton {
                    button,
                    pressed: true,
                } => {
                    state.held_buttons.insert(button);
                }
                WindowEvent::PointerButton {
                    button,
                    pressed: false,
                } => {
                    state.held_buttons.remove(&button);
                }
                _ => {}
            }
        }
        events
    }

    fn close(&mut self) -> Vec<WindowEvent> {
        let mut state = self.state.borrow_mut();
        state.closed = true;
        let mut released: Vec<WindowEvent> = std::mem::take(&mut state.held_keys)
            .into_iter()
            .map(|keycode| WindowEvent::Key {
                keycode,
                pressed: false,
            })
            .collect();
        released.extend(
            std::mem::take(&mut state.held_buttons)
                .into_iter()
                .map(|button| WindowEvent::PointerButton {
                    button,
                    pressed: false,
                }),
        );
        released
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn frame() -> DecodedFrame {
        DecodedFrame {
            width: 4,
            height: 2,
            pixels: vec![7; 4 * 2 * 4],
            decode_time: Duration::ZERO,
        }
    }

    #[test]
    fn the_recorder_sees_what_its_window_was_asked_to_draw_and_replays_injected_events() {
        let recorder = WindowRecorder::new();
        let mut window = recorder.window(WindowSpec {
            stream_id: 7,
            width: 4,
            height: 2,
        });
        assert_eq!(
            recorder.spec().map(|spec| spec.title()).as_deref(),
            Some("navette stream 7")
        );

        recorder.inject(WindowEvent::PointerMotion { x: 1.0, y: 2.0 });
        recorder.inject(WindowEvent::CloseRequested);
        assert_eq!(
            window.poll_events(),
            vec![
                WindowEvent::PointerMotion { x: 1.0, y: 2.0 },
                WindowEvent::CloseRequested,
            ]
        );
        // Events are drained, not replayed forever.
        assert!(window.poll_events().is_empty());

        window.present(&frame(), &HudSample::default());
        assert_eq!(recorder.presented().len(), 1);
        assert_eq!(
            recorder.last_presented().map(|drawn| drawn.frame),
            Some(frame())
        );

        // A closed window stops accepting pictures, so a test can tell a
        // stopped stream from a merely idle one.
        window.close();
        assert!(recorder.is_closed());
        window.present(&frame(), &HudSample::default());
        assert_eq!(recorder.presented().len(), 1);
    }

    #[test]
    fn closing_a_window_releases_keys_and_buttons_still_held() {
        let recorder = WindowRecorder::new();
        let mut window = recorder.window(WindowSpec {
            stream_id: 1,
            width: 4,
            height: 2,
        });

        recorder.inject(WindowEvent::Key {
            keycode: 30,
            pressed: true,
        });
        recorder.inject(WindowEvent::PointerButton {
            button: 0x110,
            pressed: true,
        });
        // Pressed and released before close: must not be reported as still
        // held.
        recorder.inject(WindowEvent::Key {
            keycode: 31,
            pressed: true,
        });
        recorder.inject(WindowEvent::Key {
            keycode: 31,
            pressed: false,
        });
        // The window only learns what is held from events it has actually
        // drained, same as a real window only knows what it observed.
        window.poll_events();

        assert_eq!(
            window.close(),
            vec![
                WindowEvent::Key {
                    keycode: 30,
                    pressed: false,
                },
                WindowEvent::PointerButton {
                    button: 0x110,
                    pressed: false,
                },
            ]
        );
        // Nothing is left to release a second time.
        assert_eq!(window.close(), Vec::new());
    }

    #[test]
    fn closing_a_window_with_nothing_held_releases_nothing() {
        let recorder = WindowRecorder::new();
        let mut window = recorder.window(WindowSpec {
            stream_id: 1,
            width: 4,
            height: 2,
        });
        recorder.inject(WindowEvent::PointerMotion { x: 0.0, y: 0.0 });
        window.poll_events();
        assert_eq!(window.close(), Vec::new());
    }
}
