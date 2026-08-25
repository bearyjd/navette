use anyhow::{Context, Result};
use clap::Parser;
use navette_viewer::{MediaClient, StreamEvent, ffmpeg_decoder_factory, media_url};

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

    loop {
        tokio::select! {
            event = client.next_event() => {
                let Some(event) = event else {
                    tracing::info!("session media closed");
                    return Ok(());
                };
                report(event);
            }
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for interrupts")?;
                tracing::info!("detaching");
                return Ok(());
            }
        }
    }
}

fn report(event: StreamEvent) {
    match event {
        StreamEvent::Frame(frame) => tracing::info!(
            stream_id = frame.stream_id,
            client_id = frame.client_id,
            surface_id = frame.surface_id,
            width = frame.frame.width,
            height = frame.frame.height,
            timestamp_us = frame.timestamp_us,
            "decoded frame"
        ),
        StreamEvent::Ended { stream_id } => tracing::info!(stream_id, "stream ended"),
        StreamEvent::DecodeFailed { stream_id } => {
            tracing::warn!(stream_id, "decode failed")
        }
    }
}
