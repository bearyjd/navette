use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use clap::Parser;
use navetted::api::{ApiState, router};
use navetted::app_index::AppIndex;
use navetted::registry::Registry;
use navetted::supervisor::{RealProcessRunner, Supervisor};

#[derive(Debug, Parser)]
#[command(about = "Navette host daemon")]
struct Arguments {
    /// Address on which to serve the control API.
    #[arg(long, default_value = "127.0.0.1:9417")]
    bind: SocketAddr,

    /// Acknowledge that binding the unauthenticated M1 API beyond loopback is unsafe.
    #[arg(long)]
    allow_remote: bool,

    /// Override the persistent session registry path.
    #[arg(long)]
    state_file: Option<PathBuf>,

    /// Override XDG_RUNTIME_DIR for session sockets.
    #[arg(long)]
    runtime_dir: Option<PathBuf>,

    /// wprsd executable to supervise.
    #[arg(long, default_value = "wprsd")]
    wprsd: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let arguments = Arguments::parse();
    if !arguments.bind.ip().is_loopback() && !arguments.allow_remote {
        bail!(
            "refusing non-loopback bind {}; pass --allow-remote to acknowledge M1 has no authentication",
            arguments.bind
        );
    }

    let runtime_dir = arguments
        .runtime_dir
        .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
        .context("XDG_RUNTIME_DIR is unset; pass --runtime-dir")?;
    let registry = match arguments.state_file {
        Some(path) => Registry::open(path),
        None => Registry::open_default(),
    }
    .context("failed to open session registry")?;

    let apps = Arc::new(AppIndex::load());
    let app_count = apps.len();
    let supervisor = Arc::new(Supervisor::new(
        Arc::new(RealProcessRunner),
        Arc::new(Mutex::new(registry)),
        runtime_dir,
        arguments.wprsd,
    ));
    supervisor
        .reconcile()
        .context("failed to reconcile persisted sessions")?;

    let listener = tokio::net::TcpListener::bind(arguments.bind)
        .await
        .with_context(|| format!("failed to bind {}", arguments.bind))?;
    tracing::info!(address = %arguments.bind, apps = app_count, "navetted listening");
    axum::serve(listener, router(ApiState::new(apps, supervisor)))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("control server failed")
}

async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
}
