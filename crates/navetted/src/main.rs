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

    /// Acknowledge that binding the API beyond loopback exposes it to the whole
    /// network, not only the tailnet. The API requires a token, but the transport
    /// is plaintext.
    #[arg(long)]
    allow_remote: bool,

    /// Override the persistent session registry path.
    #[arg(long)]
    state_file: Option<PathBuf>,

    /// Override the API token file path.
    #[arg(long)]
    token_file: Option<PathBuf>,

    /// Override XDG_RUNTIME_DIR for session sockets.
    #[arg(long)]
    runtime_dir: Option<PathBuf>,

    /// wprsd executable to supervise.
    #[arg(long, default_value = "wprsd")]
    wprsd: String,
}

/// Installs the log subscriber, defaulting to `info` and letting `RUST_LOG`
/// override.
///
/// `fmt::init()` alone is not equivalent. Without the `env-filter` feature it
/// ignores `RUST_LOG` entirely and pins the level at `info`, so any `debug!`
/// diagnostic compiles and then emits nothing however the binary is run.
/// Enabling the feature and stopping there is wrong the other way:
/// `from_default_env` falls back to `error` when `RUST_LOG` is unset, which
/// would silence the ordinary startup logs the operator guide points people
/// at. So: `info` unless asked otherwise.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let arguments = Arguments::parse();
    if !arguments.bind.ip().is_loopback() && !arguments.allow_remote {
        bail!(
            "refusing non-loopback bind {}; pass --allow-remote to acknowledge the transport is plaintext",
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
    let token_path = match arguments.token_file {
        Some(path) => path,
        None => navette_auth::default_token_path().context("cannot determine a token path")?,
    };
    let auth = Arc::new(
        navette_auth::AuthToken::load_or_create(&token_path)
            .context("failed to load the API token")?,
    );
    // Never log the value itself.
    tracing::info!(path = %token_path.display(), "API token loaded");
    let state = ApiState::new(apps, supervisor, auth);
    state.start_existing_bridges();
    axum::serve(listener, router(state))
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
