use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use calloop::EventLoop;
use calloop::channel::Event;
use navette_bridge::{Scene, WprsTransport};

fn main() -> Result<()> {
    let socket = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: capture_probe WPRS_SOCKET [SECONDS]")?;
    let seconds = env::args()
        .nth(2)
        .map(|value| value.parse::<u64>())
        .transpose()
        .context("SECONDS must be an integer")?
        .unwrap_or(5);

    let mut transport = WprsTransport::connect(&socket)?;
    let receiver = transport
        .take_receiver()
        .context("wprs receiver was already taken")?;
    let mut event_loop: EventLoop<Scene> = EventLoop::try_new()?;
    event_loop
        .handle()
        .insert_source(receiver, |event, _, scene| {
            if let Event::Msg(message) = event
                && let Err(error) = scene.apply(message)
            {
                eprintln!("capture warning: {error}");
            }
        })
        .map_err(|_| anyhow!("failed to register wprs transport source"))?;

    let mut scene = Scene::default();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while transport.is_connected() && Instant::now() < deadline {
        event_loop.dispatch(Some(Duration::from_millis(50)), &mut scene)?;
    }
    println!(
        "surfaces={} toplevels={} connected={}",
        scene.surface_count(),
        scene.toplevels().len(),
        transport.is_connected()
    );
    Ok(())
}
