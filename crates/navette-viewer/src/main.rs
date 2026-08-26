use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use navette_viewer::{
    MediaClient, ViewerSession, ffmpeg_decoder_factory, media_url, native_window_factory,
};

/// How often each window's input queue is drained. Fast enough that pointer
/// motion feels continuous, cheap enough to run alongside decoding.
const POLL_INTERVAL: Duration = Duration::from_millis(8);

#[derive(Parser, Debug)]
#[command(name = "navette-viewer", version, about)]
struct Cli {
    /// Session to view.
    session: String,

    /// navetted WebSocket endpoint.
    #[arg(long, env = "NAVETTE_URL", default_value = "ws://127.0.0.1:9417")]
    url: String,

    /// FFmpeg executable used for decoding.
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let url = media_url(&cli.url, &cli.session);
    let mut client = MediaClient::connect(&url, ffmpeg_decoder_factory(cli.ffmpeg))
        .await
        .with_context(|| format!("failed to attach to {url}"))?;
    tracing::info!(session = %cli.session, %url, "attached to session media");

    // Windows are not `Send`, so they stay on the thread `block_on` is
    // driving: this future is never moved to a worker.
    let mut session = ViewerSession::new(native_window_factory());
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            event = client.next_event() => {
                let Some(event) = event else {
                    tracing::info!("session media closed");
                    return Ok(());
                };
                session.handle(event, Instant::now());
            }
            _ = poll.tick() => {
                for input in session.poll(Instant::now()) {
                    // Sending never waits, so this handler always returns to
                    // the select and keeps draining events. One rejected or
                    // undeliverable event must not end the session; the
                    // connection closing is what ends it.
                    if let Err(error) = client.send_input(input) {
                        tracing::warn!(%error, "dropping an input event");
                    }
                }
            }
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for interrupts")?;
                tracing::info!("detaching");
                return Ok(());
            }
        }
    }
}
