//! Disposable Linux validation client for a Navette session's media endpoint.
//!
//! The crate connects to `/v1/sessions/{session}/media`, routes each
//! toplevel's H.264 stream to its own decoder, shows each of them in its own
//! native window, and forwards that window's input back to the session.
//! Decoding and windowing both sit behind traits ([`Decoder`], [`Window`]) so
//! the protocol, routing, input and HUD behaviour can be exercised headlessly
//! in CI.

pub mod client;
pub mod decoder;
pub mod hud;
pub mod native;
pub mod overlay;
pub mod router;
pub mod session;
pub mod window;

pub use client::{ClientError, MediaClient};
pub use decoder::{
    DecodedFrame, Decoder, DecoderConfig, DecoderError, DecoderMetrics, FakeDecoder, FfmpegDecoder,
};
pub use hud::{HudSample, StreamHud};
pub use native::{NativeWindow, native_window_factory};
pub use router::{DecoderFactory, StreamEvent, StreamFrame, StreamPacket, StreamRouter};
pub use session::ViewerSession;
pub use window::{
    Modifiers, Presented, RecordingWindow, Window, WindowError, WindowEvent, WindowFactory,
    WindowRecorder, WindowSpec,
};

/// Builds the media endpoint URL for `session` on a daemon reachable at
/// `daemon_url` (for example `ws://127.0.0.1:9417`).
pub fn media_url(daemon_url: &str, session: &str) -> String {
    format!(
        "{}/v1/sessions/{session}/media",
        daemon_url.trim_end_matches('/')
    )
}

/// Decoder factory that spawns a real FFmpeg subprocess per stream.
pub fn ffmpeg_decoder_factory(executable: impl Into<String>) -> DecoderFactory {
    let executable = executable.into();
    Box::new(move |config: &DecoderConfig| {
        Ok(Box::new(FfmpegDecoder::new(executable.clone(), config.clone())?) as Box<dyn Decoder>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_url_joins_without_doubling_the_separator() {
        assert_eq!(
            media_url("ws://127.0.0.1:9417", "work"),
            "ws://127.0.0.1:9417/v1/sessions/work/media"
        );
        assert_eq!(
            media_url("ws://127.0.0.1:9417/", "work"),
            "ws://127.0.0.1:9417/v1/sessions/work/media"
        );
    }
}
