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
    /// Pointer and keyboard focus are independent in Wayland, and wprs's
    /// server treats them independently too: only `KeyboardEvent::Enter`
    /// establishes keyboard focus, and pointer events never touch it. They
    /// must therefore be tracked separately -- sharing one field lets a
    /// pointer motion suppress the keyboard enter a surface needs before it
    /// can receive any key at all.
    pointer_focus: Option<SurfaceKey>,
    keyboard_focus: Option<SurfaceKey>,
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
                if self.pointer_focus != Some(key) {
                    events.push(pointer_event(
                        key,
                        position,
                        PointerEventKind::Enter {
                            serial: self.next_serial(),
                        },
                    ));
                    self.pointer_focus = Some(key);
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

    /// Releases the input `attachment_id` still held down. Neither focus
    /// field is cleared: focus is a property of the shared wprs seat, not of
    /// one attachment, so a client leaving must not revoke the focus other
    /// attached clients are still typing and pointing into.
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
        if self.keyboard_focus != Some(key) {
            let serial = self.next_serial();
            transport.send(Event::KeyboardEvent(KeyboardEvent::Enter {
                serial,
                surface_id: WlSurfaceId(key.surface_id),
                keycodes: Vec::new(),
                keysyms: Vec::new(),
            }));
            self.keyboard_focus = Some(key);
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::TryRecvError;
    use std::thread;
    use std::time::{Duration, Instant};

    use calloop::channel::Channel;
    use tempfile::TempDir;
    use wprs::serialization::wayland::{
        Buffer, BufferAssignment, BufferData, BufferFormat, BufferMetadata, Role, SubSurfaceState,
        SurfaceRequest, SurfaceRequestPayload, SurfaceState,
    };
    use wprs::serialization::xdg_shell::{XdgToplevelId, XdgToplevelState};
    use wprs::serialization::{ClientId, RecvType, Request, Serializer};

    use super::*;
    use crate::Scene;

    fn surface_state(client: u64, surface: u64, role: Option<Role>) -> SurfaceState {
        SurfaceState {
            client: ClientId(client),
            id: WlSurfaceId(surface),
            buffer: None,
            role,
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

    fn toplevel_role() -> Role {
        Role::XdgToplevel(XdgToplevelState {
            id: XdgToplevelId(1),
            parent: None,
            title: None,
            app_id: None,
            decoration_mode: None,
            maximized: None,
            fullscreen: None,
        })
    }

    fn subsurface_role(parent: u64) -> Role {
        Role::SubSurface(SubSurfaceState {
            parent: WlSurfaceId(parent),
            location: Point { x: 0, y: 0 },
            sync: true,
        })
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

    /// Builds a scene containing one toplevel per `(client_id, surface_id)`
    /// pair, each with a real `width`x`height` committed image so
    /// `validate_surface` treats it as a known toplevel.
    fn scene_with_toplevels(surfaces: &[(u64, u64, u32, u32)]) -> Scene {
        let mut scene = Scene::default();
        for &(client, surface, width, height) in surfaces {
            scene
                .apply(RecvType::RawBuffer(vec![
                    0;
                    width as usize
                        * height as usize
                        * 4
                ]))
                .unwrap();
            let mut state = surface_state(client, surface, Some(toplevel_role()));
            state.buffer = Some(external_buffer(
                width as i32,
                height as i32,
                BufferFormat::Xrgb8888,
            ));
            scene.apply(commit(state)).unwrap();
        }
        scene
    }

    /// A toplevel plus a committed (but non-toplevel) subsurface child, used
    /// to exercise `validate_surface`'s `NotToplevel` branch: the subsurface
    /// has an image (so it's a *known* surface) but isn't a toplevel.
    fn scene_with_toplevel_and_subsurface() -> Scene {
        let mut scene = scene_with_toplevels(&[(1, 1, 8, 8)]);
        scene.apply(RecvType::RawBuffer(vec![0; 4])).unwrap();
        let mut child = surface_state(1, 2, Some(subsurface_role(1)));
        child.buffer = Some(external_buffer(1, 1, BufferFormat::Argb8888));
        scene.apply(commit(child)).unwrap();
        scene
    }

    /// `wprs::utils::bind_user_socket` temporarily widens the process-wide
    /// umask around each bind, without synchronization. Tests run in
    /// parallel threads, so a concurrent `tempfile::tempdir()` (or any other
    /// file creation) racing against that window can be created with a
    /// mode that strips its own owner-execute bit, making the directory
    /// untraversable and any later bind inside it fail with `EACCES`.
    /// Serializing the temp-dir-creation-through-bind span here works
    /// around that (upstream, not ours to fix) race.
    static WPRSD_BIND_LOCK: Mutex<()> = Mutex::new(());

    /// A headless stand-in for `wprsd`: binds a real Unix socket, lets a
    /// `WprsTransport` connect to it, and hands back the raw `Event`s the
    /// transport sends so tests can assert on them without a mocking
    /// framework.
    struct FakeWprsd {
        events: Channel<RecvType<Event>>,
        _server: Serializer<Request, Event>,
        _dir: TempDir,
    }

    impl FakeWprsd {
        fn connect() -> (WprsTransport, Self) {
            let guard = WPRSD_BIND_LOCK
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let dir = tempfile::tempdir().expect("create temp dir for fake wprsd socket");
            let socket = dir.path().join("wprs.sock");
            let mut server: Serializer<Request, Event> =
                Serializer::new_server(&socket).expect("bind fake wprsd socket");
            drop(guard);
            let events = server.reader().expect("fake wprsd reader already taken");
            let transport = WprsTransport::connect(&socket).expect("connect to fake wprsd");
            (
                transport,
                Self {
                    events,
                    _server: server,
                    _dir: dir,
                },
            )
        }

        /// Returns the next event the transport sent, skipping the
        /// connection preamble (`WprsClientConnect`, `Output`) that
        /// `WprsTransport::connect` emits automatically.
        fn recv(&self) -> Event {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match self.events.try_recv() {
                    Ok(RecvType::Object(Event::WprsClientConnect | Event::Output(_))) => continue,
                    Ok(RecvType::Object(event)) => return event,
                    Ok(RecvType::RawBuffer(_)) => continue,
                    Err(TryRecvError::Empty) => {
                        assert!(
                            Instant::now() < deadline,
                            "timed out waiting for a wprs event"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(TryRecvError::Disconnected) => panic!("fake wprsd channel disconnected"),
                }
            }
        }
    }

    #[test]
    fn pointer_input_routes_to_focused_toplevel_and_enters_once_per_focus_change() {
        let scene = scene_with_toplevels(&[(1, 1, 8, 8), (1, 2, 8, 8)]);
        let key2 = SurfaceKey {
            client_id: 1,
            surface_id: 2,
        };
        let (transport, fake) = FakeWprsd::connect();
        let mut state = InputState::default();

        state
            .apply(
                7,
                MediaInput::PointerMotion {
                    client_id: 1,
                    surface_id: 1,
                    x: 3.0,
                    y: 4.0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => {
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].surface_id, WlSurfaceId(1));
                assert!(matches!(events[0].kind, PointerEventKind::Enter { .. }));
                assert!(matches!(events[1].kind, PointerEventKind::Motion));
                assert_eq!(events[1].position, Point { x: 3.0, y: 4.0 });
            }
            other => panic!("expected a pointer frame, got {other:?}"),
        }

        // A second motion to the same surface must not re-enter.
        state
            .apply(
                7,
                MediaInput::PointerMotion {
                    client_id: 1,
                    surface_id: 1,
                    x: 5.0,
                    y: 5.0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => {
                assert_eq!(events.len(), 1);
                assert!(matches!(events[0].kind, PointerEventKind::Motion));
            }
            other => panic!("expected a pointer frame, got {other:?}"),
        }

        // Motion to a different surface re-enters.
        state
            .apply(
                7,
                MediaInput::PointerMotion {
                    client_id: 1,
                    surface_id: 2,
                    x: 1.0,
                    y: 1.0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => {
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].surface_id, WlSurfaceId(2));
                assert!(matches!(events[0].kind, PointerEventKind::Enter { .. }));
            }
            other => panic!("expected a pointer frame, got {other:?}"),
        }

        state
            .apply(
                7,
                MediaInput::PointerButton {
                    client_id: 1,
                    surface_id: 2,
                    button: 0x110,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => {
                assert_eq!(events.len(), 1);
                assert!(matches!(
                    events[0].kind,
                    PointerEventKind::Press { button: 0x110, .. }
                ));
            }
            other => panic!("expected a pointer frame, got {other:?}"),
        }
        assert!(state.pressed_buttons[&7].contains(&(key2, 0x110)));

        state
            .apply(
                7,
                MediaInput::PointerButton {
                    client_id: 1,
                    surface_id: 2,
                    button: 0x110,
                    pressed: false,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => assert!(matches!(
                events[0].kind,
                PointerEventKind::Release { button: 0x110, .. }
            )),
            other => panic!("expected a pointer frame, got {other:?}"),
        }
        assert!(!state.pressed_buttons[&7].contains(&(key2, 0x110)));

        state
            .apply(
                7,
                MediaInput::PointerAxis {
                    client_id: 1,
                    surface_id: 2,
                    horizontal: 1.5,
                    vertical: -2.0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => {
                assert_eq!(events.len(), 1);
                match events[0].kind {
                    PointerEventKind::Axis {
                        horizontal,
                        vertical,
                        source,
                    } => {
                        assert_eq!(horizontal.absolute, 1.5);
                        assert_eq!(vertical.absolute, -2.0);
                        assert_eq!(source, Some(AxisSource::Continuous));
                    }
                    other => panic!("expected an axis event, got {other:?}"),
                }
            }
            other => panic!("expected a pointer frame, got {other:?}"),
        }
    }

    #[test]
    fn apply_rejects_input_targeting_an_unknown_surface() {
        let scene = scene_with_toplevels(&[(1, 1, 8, 8)]);
        let (transport, _fake) = FakeWprsd::connect();
        let mut state = InputState::default();

        let error = state
            .apply(
                1,
                MediaInput::PointerMotion {
                    client_id: 9,
                    surface_id: 9,
                    x: 0.0,
                    y: 0.0,
                },
                &scene,
                &transport,
            )
            .unwrap_err();
        assert_eq!(error, InputTranslationError::UnknownSurface);
    }

    #[test]
    fn apply_rejects_input_targeting_a_known_non_toplevel_surface() {
        let scene = scene_with_toplevel_and_subsurface();
        let (transport, _fake) = FakeWprsd::connect();
        let mut state = InputState::default();

        let error = state
            .apply(
                1,
                MediaInput::PointerButton {
                    client_id: 1,
                    surface_id: 2,
                    button: 0x110,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap_err();
        assert_eq!(error, InputTranslationError::NotToplevel);
    }

    #[test]
    fn keyboard_input_enters_focus_once_per_surface_change() {
        let scene = scene_with_toplevels(&[(1, 1, 8, 8), (1, 2, 8, 8)]);
        let (transport, fake) = FakeWprsd::connect();
        let mut state = InputState::default();

        state
            .apply(
                3,
                MediaInput::KeyboardKey {
                    client_id: 1,
                    surface_id: 1,
                    keycode: 30,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Enter { surface_id, .. }) => {
                assert_eq!(surface_id, WlSurfaceId(1));
            }
            other => panic!("expected a keyboard enter, got {other:?}"),
        }
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Key(KeyInner {
                raw_code: 30,
                state: KeyState::Pressed,
                ..
            })) => {}
            other => panic!("expected a key press, got {other:?}"),
        }
        assert!(state.pressed_keys[&3].contains(&30));

        // A second key on the already-focused surface must not re-enter.
        state
            .apply(
                3,
                MediaInput::KeyboardKey {
                    client_id: 1,
                    surface_id: 1,
                    keycode: 31,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Key(KeyInner {
                raw_code: 31,
                state: KeyState::Pressed,
                ..
            })) => {}
            other => panic!("expected a key press without a re-enter, got {other:?}"),
        }

        state
            .apply(
                3,
                MediaInput::KeyboardKey {
                    client_id: 1,
                    surface_id: 1,
                    keycode: 30,
                    pressed: false,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Key(KeyInner {
                raw_code: 30,
                state: KeyState::Released,
                ..
            })) => {}
            other => panic!("expected a key release, got {other:?}"),
        }
        assert!(!state.pressed_keys[&3].contains(&30));

        // Modifiers on the already-focused surface must not re-enter either.
        state
            .apply(
                3,
                MediaInput::KeyboardModifiers {
                    client_id: 1,
                    surface_id: 1,
                    ctrl: true,
                    alt: false,
                    shift: false,
                    caps_lock: false,
                    logo: false,
                    num_lock: false,
                    layout_index: 0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Modifiers { modifier_state, .. }) => {
                assert!(modifier_state.ctrl);
            }
            other => panic!("expected a modifiers event, got {other:?}"),
        }

        // Switching focus to a different surface re-enters.
        state
            .apply(
                3,
                MediaInput::KeyboardKey {
                    client_id: 1,
                    surface_id: 2,
                    keycode: 32,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Enter { surface_id, .. }) => {
                assert_eq!(surface_id, WlSurfaceId(2));
            }
            other => panic!("expected a re-enter on focus change, got {other:?}"),
        }
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Key(KeyInner { raw_code: 32, .. })) => {}
            other => panic!("expected a key press, got {other:?}"),
        }
    }

    /// Pointer focus and keyboard focus are independent: a pointer motion
    /// over a surface must not stand in for the `KeyboardEvent::Enter` that
    /// surface needs before wprs will route any key to it, and vice versa.
    /// The viewer always sends a pointer motion before any keyboard event, so
    /// conflating the two focus fields kills keyboard input outright.
    #[test]
    fn pointer_focus_does_not_suppress_keyboard_enter_on_the_same_surface() {
        let scene = scene_with_toplevels(&[(1, 1, 8, 8)]);
        let (transport, fake) = FakeWprsd::connect();
        let mut state = InputState::default();

        state
            .apply(
                7,
                MediaInput::PointerMotion {
                    client_id: 1,
                    surface_id: 1,
                    x: 2.0,
                    y: 2.0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => {
                assert!(matches!(events[0].kind, PointerEventKind::Enter { .. }));
            }
            other => panic!("expected a pointer frame, got {other:?}"),
        }

        // The very same surface now takes keyboard input: it must still be
        // entered for the keyboard, because the pointer enter above did not
        // give it keyboard focus server-side.
        state
            .apply(
                7,
                MediaInput::KeyboardKey {
                    client_id: 1,
                    surface_id: 1,
                    keycode: 30,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Enter { surface_id, .. }) => {
                assert_eq!(surface_id, WlSurfaceId(1));
            }
            other => panic!(
                "a pointer motion must not suppress the keyboard enter for the same surface, got {other:?}"
            ),
        }
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Key(KeyInner { raw_code: 30, .. })) => {}
            other => panic!("expected the key press after the enter, got {other:?}"),
        }
    }

    /// The mirror image: a keyboard enter must not consume the pointer's
    /// `Enter` for the same surface either.
    #[test]
    fn keyboard_focus_does_not_suppress_pointer_enter_on_the_same_surface() {
        let scene = scene_with_toplevels(&[(1, 1, 8, 8)]);
        let (transport, fake) = FakeWprsd::connect();
        let mut state = InputState::default();

        state
            .apply(
                7,
                MediaInput::KeyboardModifiers {
                    client_id: 1,
                    surface_id: 1,
                    ctrl: true,
                    alt: false,
                    shift: false,
                    caps_lock: false,
                    logo: false,
                    num_lock: false,
                    layout_index: 0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Enter { surface_id, .. }) => {
                assert_eq!(surface_id, WlSurfaceId(1));
            }
            other => panic!("expected a keyboard enter, got {other:?}"),
        }
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Modifiers { .. }) => {}
            other => panic!("expected the modifiers event, got {other:?}"),
        }

        state
            .apply(
                7,
                MediaInput::PointerMotion {
                    client_id: 1,
                    surface_id: 1,
                    x: 1.0,
                    y: 1.0,
                },
                &scene,
                &transport,
            )
            .unwrap();
        match fake.recv() {
            Event::PointerFrame(events) => {
                assert_eq!(events.len(), 2);
                assert!(
                    matches!(events[0].kind, PointerEventKind::Enter { .. }),
                    "a keyboard enter must not suppress the pointer enter for the same surface"
                );
                assert!(matches!(events[1].kind, PointerEventKind::Motion));
            }
            other => panic!("expected a pointer frame, got {other:?}"),
        }
    }

    #[test]
    fn viewport_resize_configures_every_current_toplevel() {
        let scene = scene_with_toplevels(&[(1, 1, 8, 8), (1, 2, 8, 8)]);
        let (transport, fake) = FakeWprsd::connect();
        let mut state = InputState::default();

        state
            .apply(
                0,
                MediaInput::ViewportResize {
                    width: 1920,
                    height: 1080,
                },
                &scene,
                &transport,
            )
            .unwrap();

        for expected_surface in [WlSurfaceId(1), WlSurfaceId(2)] {
            match fake.recv() {
                Event::Toplevel(ToplevelEvent::Configure(configure)) => {
                    assert_eq!(configure.surface_id, expected_surface);
                    assert_eq!(configure.new_size.w, NonZeroU32::new(1920));
                    assert_eq!(configure.new_size.h, NonZeroU32::new(1080));
                }
                other => panic!("expected a toplevel configure, got {other:?}"),
            }
        }
    }

    #[test]
    fn disconnect_releases_only_that_attachments_held_input() {
        let scene = scene_with_toplevels(&[(1, 1, 8, 8)]);
        let (transport, fake) = FakeWprsd::connect();
        let mut state = InputState::default();
        let key = SurfaceKey {
            client_id: 1,
            surface_id: 1,
        };

        state
            .apply(
                10,
                MediaInput::PointerButton {
                    client_id: 1,
                    surface_id: 1,
                    button: 0x110,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        fake.recv();
        state
            .apply(
                20,
                MediaInput::PointerButton {
                    client_id: 1,
                    surface_id: 1,
                    button: 0x111,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        fake.recv();
        state
            .apply(
                10,
                MediaInput::KeyboardKey {
                    client_id: 1,
                    surface_id: 1,
                    keycode: 30,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        fake.recv(); // keyboard enter
        fake.recv(); // key press
        state
            .apply(
                20,
                MediaInput::KeyboardKey {
                    client_id: 1,
                    surface_id: 1,
                    keycode: 31,
                    pressed: true,
                },
                &scene,
                &transport,
            )
            .unwrap();
        fake.recv(); // key press only: already focused

        assert!(state.pressed_buttons[&10].contains(&(key, 0x110)));
        assert!(state.pressed_buttons[&20].contains(&(key, 0x111)));
        assert!(state.pressed_keys[&10].contains(&30));
        assert!(state.pressed_keys[&20].contains(&31));

        state.disconnect(10, &transport);

        match fake.recv() {
            Event::PointerFrame(events) => {
                assert_eq!(events.len(), 1);
                assert!(matches!(
                    events[0].kind,
                    PointerEventKind::Release { button: 0x110, .. }
                ));
            }
            other => {
                panic!("expected a release for the disconnected attachment's button, got {other:?}")
            }
        }
        match fake.recv() {
            Event::KeyboardEvent(KeyboardEvent::Key(KeyInner {
                raw_code: 30,
                state: KeyState::Released,
                ..
            })) => {}
            other => {
                panic!("expected a release for the disconnected attachment's key, got {other:?}")
            }
        }

        assert!(!state.pressed_buttons.contains_key(&10));
        assert!(!state.pressed_keys.contains_key(&10));
        assert!(state.pressed_buttons[&20].contains(&(key, 0x111)));
        assert!(state.pressed_keys[&20].contains(&31));
    }
}
