//! The real, on-screen [`Window`] implementation.
//!
//! `minifb` was chosen for one reason: the viewer already has fully decoded
//! BGRA pictures, so all it needs is a resizable OS window that takes a raw
//! pixel buffer and reports keyboard and mouse state — which is precisely
//! minifb's entire API, with X11 and Wayland backends and no GPU, surface, or
//! event-loop machinery to adopt. A `winit`-based stack would bring an
//! event-loop ownership model and a separate blitting crate for no gain in a
//! throwaway validation tool.
//!
//! Nothing in this module is exercised by the test suite: it needs a display,
//! and the viewer's behaviour is tested against
//! [`RecordingWindow`](crate::window::RecordingWindow) instead. Keep the logic
//! here to translation only.

use std::collections::BTreeSet;

use minifb::{Key, KeyRepeat, MouseButton, MouseMode, ScaleMode, WindowOptions};

use crate::decoder::DecodedFrame;
use crate::hud::HudSample;
use crate::overlay;
use crate::window::{Modifiers, Window, WindowError, WindowEvent, WindowFactory, WindowSpec};

/// evdev button codes, the same namespace the media protocol allowlists.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

const HUD_MARGIN: usize = 4;
const HUD_STYLE: overlay::TextStyle = overlay::TextStyle {
    scale: 2,
    foreground: 0x0000_ff66,
    background: 0x0011_1111,
};

const TRACKED_BUTTONS: [(MouseButton, u32); 3] = [
    (MouseButton::Left, BTN_LEFT),
    (MouseButton::Middle, BTN_MIDDLE),
    (MouseButton::Right, BTN_RIGHT),
];

/// Opens real OS windows. This is the factory the viewer binary uses.
pub fn native_window_factory() -> WindowFactory {
    Box::new(|spec: &WindowSpec| Ok(Box::new(NativeWindow::open(spec)?) as Box<dyn Window>))
}

pub struct NativeWindow {
    window: minifb::Window,
    /// Scratch `0RGB` buffer the decoded BGRA frame is converted into. Kept
    /// between frames so a steady stream does not reallocate.
    buffer: Vec<u32>,
    /// Set by `present` when `buffer` holds a picture not yet shown, and
    /// cleared once `poll_events` has passed it to minifb. Only one minifb
    /// update call is allowed to run per loop iteration (see `poll_events`),
    /// so `present` cannot call it directly and instead leaves this for the
    /// next poll to pick up.
    pending_present: Option<(usize, usize)>,
    size: (u32, u32),
    pointer: Option<(f64, f64)>,
    buttons: [bool; TRACKED_BUTTONS.len()],
    /// Keys this window has reported pressed and not yet reported released.
    /// Reconciled against minifb's own down-state every poll and drained
    /// into synthetic releases on `close`, so a transition this window
    /// missed cannot leave a key stuck down in the guest.
    held_keys: BTreeSet<Key>,
    modifiers: Modifiers,
    open: bool,
}

impl NativeWindow {
    pub fn open(spec: &WindowSpec) -> Result<Self, WindowError> {
        let window = minifb::Window::new(
            &spec.title(),
            spec.width.max(1) as usize,
            spec.height.max(1) as usize,
            WindowOptions {
                resize: true,
                scale_mode: ScaleMode::Stretch,
                ..WindowOptions::default()
            },
        )
        .map_err(|error| WindowError::Open(error.to_string()))?;
        let mut window = Self {
            window,
            buffer: Vec::new(),
            pending_present: None,
            size: (spec.width, spec.height),
            pointer: None,
            buttons: [false; TRACKED_BUTTONS.len()],
            held_keys: BTreeSet::new(),
            modifiers: Modifiers::default(),
            open: true,
        };
        // Frames arrive at whatever rate the bridge encodes them; minifb must
        // not add a cadence of its own on top.
        window.window.set_target_fps(0);
        Ok(window)
    }

    fn convert(&mut self, frame: &DecodedFrame) -> Option<(usize, usize)> {
        let width = frame.width as usize;
        let height = frame.height as usize;
        let pixels = width.checked_mul(height)?;
        if frame.pixels.len() < pixels.checked_mul(4)? {
            tracing::warn!(
                width = frame.width,
                height = frame.height,
                got = frame.pixels.len(),
                "discarding a frame shorter than its declared dimensions"
            );
            return None;
        }
        self.buffer.resize(pixels, 0);
        for (target, source) in self
            .buffer
            .iter_mut()
            .zip(frame.pixels.as_chunks::<4>().0.iter())
        {
            // Decoded frames are tightly packed BGRA; minifb wants 0RGB.
            *target =
                (u32::from(source[2]) << 16) | (u32::from(source[1]) << 8) | u32::from(source[0]);
        }
        Some((width, height))
    }

    fn pointer_motion(&mut self) -> Option<WindowEvent> {
        let (x, y) = self.window.get_mouse_pos(MouseMode::Clamp)?;
        let position = (f64::from(x), f64::from(y));
        if self.pointer == Some(position) {
            return None;
        }
        self.pointer = Some(position);
        Some(WindowEvent::PointerMotion {
            x: position.0,
            y: position.1,
        })
    }

    fn button_changes(&mut self, events: &mut Vec<WindowEvent>) {
        for (index, (button, code)) in TRACKED_BUTTONS.into_iter().enumerate() {
            let pressed = self.window.get_mouse_down(button);
            if self.buttons[index] == pressed {
                continue;
            }
            self.buttons[index] = pressed;
            events.push(WindowEvent::PointerButton {
                button: code,
                pressed,
            });
        }
    }

    /// Recomputes modifier state and reports it only when it changed.
    ///
    /// minifb exposes which keys are held, not which locks are engaged, so
    /// caps and num lock are tracked as toggles flipped by their own key
    /// presses. That drifts from the desktop's real lock state if it was
    /// changed while the window was unfocused — acceptable for a validation
    /// client, and the only thing here that is not directly observed.
    fn modifier_changes(&mut self, pressed: &[Key], events: &mut Vec<WindowEvent>) {
        let modifiers = Modifiers {
            ctrl: self.window.is_key_down(Key::LeftCtrl) || self.window.is_key_down(Key::RightCtrl),
            alt: self.window.is_key_down(Key::LeftAlt) || self.window.is_key_down(Key::RightAlt),
            shift: self.window.is_key_down(Key::LeftShift)
                || self.window.is_key_down(Key::RightShift),
            caps_lock: self.modifiers.caps_lock ^ pressed.contains(&Key::CapsLock),
            logo: self.window.is_key_down(Key::LeftSuper)
                || self.window.is_key_down(Key::RightSuper),
            num_lock: self.modifiers.num_lock ^ pressed.contains(&Key::NumLock),
            layout_index: 0,
        };
        if modifiers == self.modifiers {
            return;
        }
        self.modifiers = modifiers;
        events.push(WindowEvent::Modifiers(modifiers));
    }

    fn resize(&mut self) -> Option<WindowEvent> {
        let (width, height) = self.window.get_size();
        let size = (
            u32::try_from(width).unwrap_or(u32::MAX),
            u32::try_from(height).unwrap_or(u32::MAX),
        );
        if size == self.size {
            return None;
        }
        self.size = size;
        Some(WindowEvent::Resized {
            width: size.0,
            height: size.1,
        })
    }
}

impl Window for NativeWindow {
    /// Converts and draws the HUD into the scratch buffer, but does not hand
    /// it to minifb. minifb's own docs say only one of `update()` /
    /// `update_with_buffer()` should be called per window: each one clears
    /// `scroll_x`/`scroll_y` and advances `keys_down_duration` (the state
    /// `is_key_index_pressed`/`is_key_index_released` are single-cycle pulses
    /// derived from) before it processes new platform events. Calling
    /// `update_with_buffer` here as well as `update()` in `poll_events` would
    /// run two of those cycles per loop iteration, and a press, release or
    /// scroll delta that landed in this cycle would be shifted or cleared
    /// before `poll_events` ever reads it — silently dropping input at
    /// whichever rate `present` is invoked, which for a 30-60fps stream
    /// against a 125Hz poll is not a rare edge case. So `present` only
    /// prepares the buffer; `poll_events` is the sole place a minifb update
    /// runs, and it always runs exactly one.
    fn present(&mut self, frame: &DecodedFrame, hud: &HudSample) {
        if !self.open {
            return;
        }
        let Some((width, height)) = self.convert(frame) else {
            return;
        };
        overlay::draw_text(
            &mut self.buffer,
            (width, height),
            (HUD_MARGIN, HUD_MARGIN),
            &hud.to_string(),
            HUD_STYLE,
        );
        self.pending_present = Some((width, height));
    }

    fn poll_events(&mut self) -> Vec<WindowEvent> {
        if !self.open {
            return Vec::new();
        }
        if !self.window.is_open() {
            self.open = false;
            return vec![WindowEvent::CloseRequested];
        }
        // Exactly one minifb update per call, whichever kind is due: the
        // frame `present` prepared, or (on a tick with nothing new to show)
        // a bare pump of the platform event queue so an idle stream's window
        // still responds. See the note on `present` for why running both in
        // the same cycle is what used to lose input.
        match self.pending_present.take() {
            Some((width, height)) => {
                if let Err(error) = self.window.update_with_buffer(&self.buffer, width, height) {
                    tracing::warn!(%error, "failed to present a frame");
                }
            }
            None => self.window.update(),
        }

        let mut events = Vec::new();
        events.extend(self.pointer_motion());
        self.button_changes(&mut events);
        if let Some((horizontal, vertical)) = self.window.get_scroll_wheel()
            && (horizontal != 0.0 || vertical != 0.0)
        {
            events.push(WindowEvent::PointerAxis {
                horizontal: f64::from(horizontal),
                vertical: f64::from(vertical),
            });
        }
        let pressed = self.window.get_keys_pressed(KeyRepeat::Yes);
        self.modifier_changes(&pressed, &mut events);
        for key in pressed {
            if let Some(keycode) = evdev_code(key) {
                self.held_keys.insert(key);
                events.push(WindowEvent::Key {
                    keycode,
                    pressed: true,
                });
            }
        }
        for key in self.window.get_keys_released() {
            self.held_keys.remove(&key);
            if let Some(keycode) = evdev_code(key) {
                events.push(WindowEvent::Key {
                    keycode,
                    pressed: false,
                });
            }
        }
        // Self-heals a release this window's own press/release edges missed:
        // anything still in `held_keys` that minifb no longer reports down is
        // released here instead of staying wrong for the rest of the
        // session.
        let stale: Vec<Key> = self
            .held_keys
            .iter()
            .copied()
            .filter(|key| !self.window.is_key_down(*key))
            .collect();
        for key in stale {
            self.held_keys.remove(&key);
            if let Some(keycode) = evdev_code(key) {
                events.push(WindowEvent::Key {
                    keycode,
                    pressed: false,
                });
            }
        }
        events.extend(self.resize());
        events
    }

    fn close(&mut self) -> Vec<WindowEvent> {
        // minifb tears the OS window down on drop, which happens as soon as
        // the session lets go of this box; until then, stop touching it.
        self.open = false;
        let mut released: Vec<WindowEvent> = std::mem::take(&mut self.held_keys)
            .into_iter()
            .filter_map(|key| {
                evdev_code(key).map(|keycode| WindowEvent::Key {
                    keycode,
                    pressed: false,
                })
            })
            .collect();
        for (index, (_, code)) in TRACKED_BUTTONS.into_iter().enumerate() {
            if std::mem::replace(&mut self.buttons[index], false) {
                released.push(WindowEvent::PointerButton {
                    button: code,
                    pressed: false,
                });
            }
        }
        released
    }
}

/// Maps a minifb key to its Linux evdev code, the namespace the media
/// protocol allowlists (`0..=767`) and wprs ultimately replays.
fn evdev_code(key: Key) -> Option<u32> {
    Some(match key {
        Key::Escape => 1,
        Key::Key1 => 2,
        Key::Key2 => 3,
        Key::Key3 => 4,
        Key::Key4 => 5,
        Key::Key5 => 6,
        Key::Key6 => 7,
        Key::Key7 => 8,
        Key::Key8 => 9,
        Key::Key9 => 10,
        Key::Key0 => 11,
        Key::Minus => 12,
        Key::Equal => 13,
        Key::Backspace => 14,
        Key::Tab => 15,
        Key::Q => 16,
        Key::W => 17,
        Key::E => 18,
        Key::R => 19,
        Key::T => 20,
        Key::Y => 21,
        Key::U => 22,
        Key::I => 23,
        Key::O => 24,
        Key::P => 25,
        Key::LeftBracket => 26,
        Key::RightBracket => 27,
        Key::Enter => 28,
        Key::LeftCtrl => 29,
        Key::A => 30,
        Key::S => 31,
        Key::D => 32,
        Key::F => 33,
        Key::G => 34,
        Key::H => 35,
        Key::J => 36,
        Key::K => 37,
        Key::L => 38,
        Key::Semicolon => 39,
        Key::Apostrophe => 40,
        Key::Backquote => 41,
        Key::LeftShift => 42,
        Key::Backslash => 43,
        Key::Z => 44,
        Key::X => 45,
        Key::C => 46,
        Key::V => 47,
        Key::B => 48,
        Key::N => 49,
        Key::M => 50,
        Key::Comma => 51,
        Key::Period => 52,
        Key::Slash => 53,
        Key::RightShift => 54,
        Key::NumPadAsterisk => 55,
        Key::LeftAlt => 56,
        Key::Space => 57,
        Key::CapsLock => 58,
        Key::F1 => 59,
        Key::F2 => 60,
        Key::F3 => 61,
        Key::F4 => 62,
        Key::F5 => 63,
        Key::F6 => 64,
        Key::F7 => 65,
        Key::F8 => 66,
        Key::F9 => 67,
        Key::F10 => 68,
        Key::NumLock => 69,
        Key::ScrollLock => 70,
        Key::NumPad7 => 71,
        Key::NumPad8 => 72,
        Key::NumPad9 => 73,
        Key::NumPadMinus => 74,
        Key::NumPad4 => 75,
        Key::NumPad5 => 76,
        Key::NumPad6 => 77,
        Key::NumPadPlus => 78,
        Key::NumPad1 => 79,
        Key::NumPad2 => 80,
        Key::NumPad3 => 81,
        Key::NumPad0 => 82,
        Key::NumPadDot => 83,
        Key::F11 => 87,
        Key::F12 => 88,
        Key::NumPadEnter => 96,
        Key::RightCtrl => 97,
        Key::NumPadSlash => 98,
        Key::RightAlt => 100,
        Key::Home => 102,
        Key::Up => 103,
        Key::PageUp => 104,
        Key::Left => 105,
        Key::Right => 106,
        Key::End => 107,
        Key::Down => 108,
        Key::PageDown => 109,
        Key::Insert => 110,
        Key::Delete => 111,
        Key::Pause => 119,
        Key::LeftSuper => 125,
        Key::RightSuper => 126,
        Key::Menu => 127,
        // F13..F15 and `Unknown` have no place in the evdev range the bridge
        // accepts; dropping them beats guessing a code.
        _ => return None,
    })
}
