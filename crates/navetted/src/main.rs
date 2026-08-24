//! Navette orchestration daemon. Scaffolding only — the supervisor,
//! session registry, XDG app index, and Navette API (see
//! `docs/prp/startup.md` §4.1) are implemented starting at milestone M1.

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("navetted: not yet implemented");
}
