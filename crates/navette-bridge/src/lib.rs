//! In-process wprs client role for non-Wayland clients (Android, iOS,
//! browser): receives surface commits from a session's `wprsd`,
//! composites per-toplevel, and encodes damage as H.264 (VA-API,
//! software x264 fallback) for streaming over the Navette API's binary
//! media channel (see `docs/prp/startup.md`, decision D5).
//!
//! No wprs dependency yet — wprs adoption is gated on the M0
//! adopt-vs-fork risk-retirement milestone (`docs/prp/startup.md` §6),
//! which hasn't run.
