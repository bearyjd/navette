use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use navette_viewer::{
    InputRelay, MediaClient, ViewerSession, ffmpeg_decoder_factory, media_url,
    native_window_factory,
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
    init_tracing();
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
    // Which refused inputs are retried, which are coalesced, and which are
    // let go is `InputRelay`'s policy -- it lives in the library so it can be
    // unit-tested. See that module for why the distinction that matters is
    // edge versus absolute state, not press versus release.
    let mut relay = InputRelay::new();
    // Diagnostic-only: how late each tick fires relative to `POLL_INTERVAL`.
    // `MissedTickBehavior::Delay` means a late tick does not tell `interval`
    // to catch up, so a lag here is direct evidence of the poll loop being
    // kept busy by something else (frame conversion/present, HUD work)
    // rather than idling on `select!`. Measured against a real session this
    // reaches hundreds of milliseconds during a resize -- see docs/HANDOFF.md.
    let mut last_tick = Instant::now();

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
                let now = Instant::now();
                let lag = now
                    .saturating_duration_since(last_tick)
                    .saturating_sub(POLL_INTERVAL);
                if lag > Duration::from_millis(4) {
                    tracing::debug!(lag_ms = lag.as_millis(), "poll tick fired late");
                }
                last_tick = now;
                let report = relay.dispatch(session.poll(now), now, |input| {
                    // Sending never waits, so this handler always returns to
                    // the select and keeps draining events. One rejected or
                    // undeliverable event must not end the session; the
                    // connection closing is what ends it.
                    client.send_input(input)
                });
                for lag in report.redelivered {
                    tracing::debug!(
                        redelivery_lag_ms = lag.as_millis(),
                        "delivered a retried release"
                    );
                }
                for (_, error) in report.discarded {
                    tracing::warn!(%error, "dropping an input event");
                }
                if !report.abandoned.is_empty() {
                    tracing::warn!(
                        count = report.abandoned.len(),
                        "gave up on releases the connection kept refusing"
                    );
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

/// Installs the log subscriber, defaulting to `info` and letting `RUST_LOG`
/// override.
///
/// `fmt::init()` alone is not equivalent. Without the `env-filter` feature it
/// ignores `RUST_LOG` entirely and pins the level at `info`, so the `debug!`
/// diagnostics in the poll loop can never be turned on -- they compile, and
/// then emit nothing no matter how the binary is run. Enabling the feature
/// and stopping there is also wrong in the other direction: `from_default_env`
/// falls back to `error` when `RUST_LOG` is unset, which would silence the
/// ordinary startup logs the operator guide tells people to look for. So:
/// `info` unless asked otherwise.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
