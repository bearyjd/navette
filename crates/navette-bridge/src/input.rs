use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use navette_protocol::media::MediaInput;
use thiserror::Error;
use wprs::serialization::Event;
use wprs::serialization::geometry::{Point, Size};
use wprs::serialization::wayland::{
    AxisScroll, AxisSource, KeyInner, KeyState, KeyboardEvent, ModifierState, PointerEvent,
    PointerEventKind, WlSurfaceId,
};
use wprs::serialization::xdg_shell::{
    DecorationMode, ToplevelConfigure, ToplevelEvent, WindowState,
};

use crate::{Scene, SurfaceKey, WprsTransport};

#[derive(Debug, Error, Eq, PartialEq)]
pub enum InputTranslationError {
    #[error("input targets an unknown surface")]
    UnknownSurface,
    #[error("input target is not a toplevel")]
    NotToplevel,
}

#[derive(Debug, Default)]
pub struct InputState {
    serial: u32,
    focused: Option<SurfaceKey>,
    pressed_keys: BTreeMap<u64, BTreeSet<u32>>,
    pressed_buttons: BTreeMap<u64, BTreeSet<(SurfaceKey, u32)>>,
}

impl InputState {
    pub fn apply(
        &mut self,
        attachment_id: u64,
        input: MediaInput,
        scene: &Scene,
        transport: &WprsTransport,
    ) -> Result<(), InputTranslationError> {
        match input {
            MediaInput::PointerMotion {
                client_id,
                surface_id,
                x,
                y,
            } => {
                let key = validate_surface(scene, client_id, surface_id)?;
                let (width, height) = scene
                    .surface_dimensions(key)
                    .ok_or(InputTranslationError::UnknownSurface)?;
                let position = Point {
                    x: x.clamp(0.0, f64::from(width.saturating_sub(1))),
                    y: y.clamp(0.0, f64::from(height.saturating_sub(1))),
                };
                let mut events = Vec::new();
                if self.focused != Some(key) {
                    events.push(pointer_event(
                        key,
                        position,
                        PointerEventKind::Enter {
                            serial: self.next_serial(),
                        },
                    ));
                    self.focused = Some(key);
                }
                events.push(pointer_event(key, position, PointerEventKind::Motion));
                transport.send(Event::PointerFrame(events));
            }
            MediaInput::PointerButton {
                client_id,
                surface_id,
                button,
                pressed,
            } => {
                let key = validate_surface(scene, client_id, surface_id)?;
                let kind = if pressed {
                    self.pressed_buttons
                        .entry(attachment_id)
                        .or_default()
                        .insert((key, button));
                    PointerEventKind::Press {
                        button,
                        serial: self.next_serial(),
                    }
                } else {
                    if let Some(buttons) = self.pressed_buttons.get_mut(&attachment_id) {
                        buttons.remove(&(key, button));
                    }
                    PointerEventKind::Release {
                        button,
                        serial: self.next_serial(),
                    }
                };
                transport.send(Event::PointerFrame(vec![pointer_event(
                    key,
                    Point { x: 0.0, y: 0.0 },
                    kind,
                )]));
            }
            MediaInput::PointerAxis {
                client_id,
                surface_id,
                horizontal,
                vertical,
            } => {
                let key = validate_surface(scene, client_id, surface_id)?;
                let axis = |absolute| AxisScroll {
                    absolute,
                    discrete: 0,
                    stop: absolute == 0.0,
                };
                transport.send(Event::PointerFrame(vec![pointer_event(
                    key,
                    Point { x: 0.0, y: 0.0 },
                    PointerEventKind::Axis {
                        horizontal: axis(horizontal),
                        vertical: axis(vertical),
                        source: Some(AxisSource::Continuous),
                    },
                )]));
            }
            MediaInput::KeyboardKey {
                client_id,
                surface_id,
                keycode,
                pressed,
            } => {
                let key = validate_surface(scene, client_id, surface_id)?;
                self.focus_keyboard(key, transport);
                if pressed {
                    self.pressed_keys
                        .entry(attachment_id)
                        .or_default()
                        .insert(keycode);
                } else if let Some(keys) = self.pressed_keys.get_mut(&attachment_id) {
                    keys.remove(&keycode);
                }
                transport.send(Event::KeyboardEvent(KeyboardEvent::Key(KeyInner {
                    serial: self.next_serial(),
                    raw_code: keycode,
                    state: if pressed {
                        KeyState::Pressed
                    } else {
                        KeyState::Released
                    },
                })));
            }
            MediaInput::KeyboardModifiers {
                client_id,
                surface_id,
                ctrl,
                alt,
                shift,
                caps_lock,
                logo,
                num_lock,
                layout_index,
            } => {
                let key = validate_surface(scene, client_id, surface_id)?;
                self.focus_keyboard(key, transport);
                transport.send(Event::KeyboardEvent(KeyboardEvent::Modifiers {
                    modifier_state: ModifierState {
                        ctrl,
                        alt,
                        shift,
                        caps_lock,
                        logo,
                        num_lock,
                    },
                    layout_index,
                }));
            }
            MediaInput::ViewportResize { width, height } => {
                transport.update_output(width, height);
                for key in scene.toplevels() {
                    transport.send(Event::Toplevel(ToplevelEvent::Configure(
                        ToplevelConfigure {
                            surface_id: WlSurfaceId(key.surface_id),
                            new_size: Size {
                                w: NonZeroU32::new(width),
                                h: NonZeroU32::new(height),
                            },
                            suggested_bounds: Some(Size {
                                w: width,
                                h: height,
                            }),
                            decoration_mode: DecorationMode::Client,
                            state: WindowState::empty(),
                        },
                    )));
                }
            }
            MediaInput::RequestKeyframe => {}
        }
        Ok(())
    }

    pub fn disconnect(&mut self, attachment_id: u64, transport: &WprsTransport) {
        if let Some(buttons) = self.pressed_buttons.remove(&attachment_id) {
            for (key, button) in buttons {
                let serial = self.next_serial();
                transport.send(Event::PointerFrame(vec![pointer_event(
                    key,
                    Point { x: 0.0, y: 0.0 },
                    PointerEventKind::Release { button, serial },
                )]));
            }
        }
        if let Some(keys) = self.pressed_keys.remove(&attachment_id) {
            for raw_code in keys {
                let serial = self.next_serial();
                transport.send(Event::KeyboardEvent(KeyboardEvent::Key(KeyInner {
                    serial,
                    raw_code,
                    state: KeyState::Released,
                })));
            }
        }
    }

    fn focus_keyboard(&mut self, key: SurfaceKey, transport: &WprsTransport) {
        if self.focused != Some(key) {
            let serial = self.next_serial();
            transport.send(Event::KeyboardEvent(KeyboardEvent::Enter {
                serial,
                surface_id: WlSurfaceId(key.surface_id),
                keycodes: Vec::new(),
                keysyms: Vec::new(),
            }));
            self.focused = Some(key);
        }
    }

    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1).max(1);
        self.serial
    }
}

fn validate_surface(
    scene: &Scene,
    client_id: u64,
    surface_id: u64,
) -> Result<SurfaceKey, InputTranslationError> {
    let key = SurfaceKey {
        client_id,
        surface_id,
    };
    if !scene.toplevels().contains(&key) {
        return Err(if scene.surface_dimensions(key).is_some() {
            InputTranslationError::NotToplevel
        } else {
            InputTranslationError::UnknownSurface
        });
    }
    Ok(key)
}

fn pointer_event(key: SurfaceKey, position: Point<f64>, kind: PointerEventKind) -> PointerEvent {
    PointerEvent {
        surface_id: WlSurfaceId(key.surface_id),
        position,
        kind,
    }
}
