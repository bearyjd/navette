use std::num::NonZeroU32;
use std::path::Path;

use anyhow::{Context, Result};
use calloop::channel::Channel;
use wprs::serialization::geometry::{Point, Size};
use wprs::serialization::wayland::{
    KeyboardEvent, Mode, OutputEvent, OutputInfo, RepeatInfo, Subpixel, Transform,
};
use wprs::serialization::{Event, RecvType, Request, SendType, Serializer};

/// Recoverable, headless connection to one stock `wprsd` session.
pub struct WprsTransport {
    serializer: Serializer<Event, Request>,
    receiver: Option<Channel<RecvType<Request>>>,
}

impl WprsTransport {
    pub fn connect(socket: impl AsRef<Path>) -> Result<Self> {
        Self::connect_with_output(socket, 1280, 720)
    }

    pub fn connect_with_output(socket: impl AsRef<Path>, width: u32, height: u32) -> Result<Self> {
        let socket = socket.as_ref();
        let mut serializer = Serializer::new_client(socket)
            .with_context(|| format!("failed to connect to {}", socket.display()))?;
        let receiver = serializer
            .reader()
            .context("wprs transport receiver is unavailable")?;
        for event in connect_preamble(width, height) {
            serializer.writer().send(SendType::Object(event));
        }
        Ok(Self {
            serializer,
            receiver: Some(receiver),
        })
    }

    pub fn take_receiver(&mut self) -> Option<Channel<RecvType<Request>>> {
        self.receiver.take()
    }

    pub fn send(&self, event: Event) {
        self.serializer.writer().send(SendType::Object(event));
    }

    pub fn update_output(&self, width: u32, height: u32) {
        self.send(Event::Output(OutputEvent::Update(output_info(
            width, height,
        ))));
    }

    pub fn is_connected(&self) -> bool {
        self.serializer.other_end_connected()
    }
}

/// Key repeat for the guest: sway's and weston's defaults (600 ms, 25 keys/s).
///
/// wprsd's own seat is created with a 200 ms delay at 200 keys/s -- tuned for a
/// local keyboard, and hostile to a remote one: any release that reaches the
/// guest more than 200 ms after its press becomes one character every 5 ms
/// until it lands, so a single network or host stall turned one tap into
/// dozens of copies. wprsd applies a client's `RepeatInfo` to its seat, so the
/// bridge sets its own on every connection rather than patching the fork.
const KEY_REPEAT_DELAY_MS: u32 = 600;
const KEY_REPEAT_RATE_PER_SEC: NonZeroU32 = NonZeroU32::new(25).expect("non-zero");

/// Everything sent on a fresh connection, in order: the client handshake, the
/// virtual output, then the key repeat (which wprsd would otherwise leave at
/// its own default).
fn connect_preamble(width: u32, height: u32) -> [Event; 3] {
    [
        Event::WprsClientConnect,
        Event::Output(OutputEvent::New(output_info(width, height))),
        Event::KeyboardEvent(KeyboardEvent::RepeatInfo(RepeatInfo::Repeat {
            rate: KEY_REPEAT_RATE_PER_SEC,
            delay: KEY_REPEAT_DELAY_MS,
        })),
    ]
}

fn output_info(width: u32, height: u32) -> OutputInfo {
    OutputInfo {
        id: 1,
        model: "Navette Virtual Output".into(),
        make: "Grepon Labs".into(),
        location: Point { x: 0, y: 0 },
        physical_size: Size {
            w: i32::try_from(width / 4).unwrap_or(i32::MAX),
            h: i32::try_from(height / 4).unwrap_or(i32::MAX),
        },
        subpixel: Subpixel::None,
        transform: Transform::Normal,
        scale_factor: 1,
        mode: Mode {
            dimensions: Size {
                w: i32::try_from(width).unwrap_or(i32::MAX),
                h: i32::try_from(height).unwrap_or(i32::MAX),
            },
            refresh_rate: 60_000,
            current: true,
            preferred: true,
        },
        name: Some("NAVETTE-1".into()),
        description: Some("Navette Virtual Output".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preamble_sets_a_desktop_key_repeat_after_connecting_the_output() {
        let preamble = connect_preamble(1280, 720);
        assert!(matches!(preamble[0], Event::WprsClientConnect));
        assert!(matches!(preamble[1], Event::Output(OutputEvent::New(_))));
        match &preamble[2] {
            Event::KeyboardEvent(KeyboardEvent::RepeatInfo(RepeatInfo::Repeat { rate, delay })) => {
                assert_eq!((rate.get(), *delay), (25, 600));
            }
            other => panic!("expected repeat info, got {other:?}"),
        }
        assert_eq!(preamble.len(), 3);
    }

    #[test]
    fn virtual_output_uses_requested_logical_size() {
        let output = output_info(1920, 1080);
        assert_eq!(output.mode.dimensions, Size { w: 1920, h: 1080 });
        assert!(output.mode.current);
        assert!(output.mode.preferred);
    }
}
