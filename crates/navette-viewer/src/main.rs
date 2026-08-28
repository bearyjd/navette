use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use navette_protocol::media::MediaInput;
use navette_viewer::{
    ClientError, MediaClient, ViewerSession, ffmpeg_decoder_factory, media_url,
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
    // A dropped press or motion event just means one input didn't register --
    // annoying, not corrupting. A dropped *release* (key or pointer button)
    // leaves the guest believing that key/button is still held, with nothing
    // left to ever tell it otherwise short of the whole session detaching.
    // Those get retried here instead of discarded on backpressure; anything
    // else keeps the original fire-and-forget drop.
    let mut pending_redelivery: VecDeque<(Instant, MediaInput)> = VecDeque::new();
    // Diagnostic-only, for the still-open resize+rapid-typing repeat
    // investigation (docs/HANDOFF.md "Still open"): how late each tick fires
    // relative to `POLL_INTERVAL`. `MissedTickBehavior::Delay` means a tick
    // that fires late does not tell `interval` to catch up, so a lag here is
    // direct evidence of the poll loop being kept busy by something else
    // (frame conversion/present, HUD work) rather than idling on `select!`.
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
                let due: Vec<(Option<Instant>, MediaInput)> = pending_redelivery
                    .drain(..)
                    .map(|(queued_at, input)| (Some(queued_at), input))
                    .chain(session.poll(now).into_iter().map(|input| (None, input)))
                    .collect();
                for (queued_at, input) in due {
                    // Sending never waits, so this handler always returns to
                    // the select and keeps draining events. One rejected or
                    // undeliverable event must not end the session; the
                    // connection closing is what ends it.
                    if let Err(error) = client.send_input(input.clone()) {
                        if matches!(error, ClientError::InputBackpressure) && must_redeliver(&input)
                        {
                            pending_redelivery.push_back((queued_at.unwrap_or(now), input));
                        } else {
                            tracing::warn!(%error, "dropping an input event");
                        }
                    } else if let Some(queued_at) = queued_at {
                        tracing::debug!(
                            redelivery_lag_ms = now.saturating_duration_since(queued_at).as_millis(),
                            "delivered a retried release"
                        );
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

/// Whether losing `input` to backpressure would leave the guest desynced
/// from what the physical keyboard/pointer is actually doing, rather than
/// just missing one input. Only a release qualifies: a dropped press or
/// motion is a missed input; a dropped release is a stuck key or button
/// with nothing left to correct it.
fn must_redeliver(input: &MediaInput) -> bool {
    matches!(
        input,
        MediaInput::KeyboardKey { pressed: false, .. }
            | MediaInput::PointerButton { pressed: false, .. }
    )
}
