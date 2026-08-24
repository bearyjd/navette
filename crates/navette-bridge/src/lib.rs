//! Headless capture and media bridge for a Navette session.

pub mod scene;
pub mod transport;

pub use scene::{Frame, PixelFormat, Scene, SceneError, SceneEvent, SurfaceKey, ToplevelInfo};
pub use transport::WprsTransport;
