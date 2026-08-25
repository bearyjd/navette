//! Headless capture and media bridge for a Navette session.

pub mod encoder;
pub mod input;
pub mod scene;
pub mod transport;

pub use encoder::{
    EncodedFrame, Encoder, EncoderBackend, EncoderConfig, EncoderError, EncoderMetrics,
    FakeEncoder, FfmpegEncoder, FrameQueue,
};
pub use input::{InputState, InputTranslationError};
pub use scene::{Frame, PixelFormat, Scene, SceneError, SceneEvent, SurfaceKey, ToplevelInfo};
pub use transport::WprsTransport;
