use std::path::Path;

use anyhow::{Context, Result};
use calloop::channel::Channel;
use wprs::serialization::geometry::{Point, Size};
use wprs::serialization::wayland::{Mode, OutputEvent, OutputInfo, Subpixel, Transform};
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
        serializer
            .writer()
            .send(SendType::Object(Event::WprsClientConnect));
        serializer
            .writer()
            .send(SendType::Object(Event::Output(OutputEvent::New(
                output_info(width, height),
            ))));
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
    fn virtual_output_uses_requested_logical_size() {
        let output = output_info(1920, 1080);
        assert_eq!(output.mode.dimensions, Size { w: 1920, h: 1080 });
        assert!(output.mode.current);
        assert!(output.mode.preferred);
    }
}
