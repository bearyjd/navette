use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use navette_cli::{Client, render_result};
use navette_protocol::{AttachInfo, RequestCommand, ResponseResult};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::process::{Child, Command as TokioCommand};

#[derive(Parser, Debug)]
#[command(name = "navette", version, about)]
struct Cli {
    /// navetted WebSocket endpoint.
    #[arg(
        long,
        env = "NAVETTE_URL",
        default_value = "ws://127.0.0.1:9417/v1/ws",
        global = true
    )]
    url: String,

    /// SSH host used to forward the wprs Unix socket for attach.
    #[arg(long, env = "NAVETTE_SSH", global = true)]
    ssh: Option<String>,

    /// wprsc executable.
    #[arg(long, default_value = "wprsc", global = true)]
    wprsc: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List all sessions.
    Ls,
    /// Start an app as a new session.
    Run {
        /// XDG desktop entry ID.
        app: String,
        /// Explicit session name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Attach to a running session until wprsc exits.
    Attach {
        /// Session name to attach to.
        session: String,
    },
    /// Stop a tracked local attachment without killing the remote app.
    Detach {
        /// Session to detach. May be omitted when exactly one attach is tracked.
        session: Option<String>,
    },
    /// Kill a session.
    Kill {
        /// Session name to kill.
        session: String,
    },
    /// Show the API token, optionally as a QR code for phone pairing.
    ///
    /// Local admin command: it reads the daemon's token file directly, so it
    /// only works on the host running navetted.
    Token {
        /// Render a QR code carrying host, port and token.
        #[arg(long)]
        qr: bool,
        /// Generate a new token, invalidating every paired client.
        #[arg(long)]
        rotate: bool,
        /// The name or address the phone should dial.
        #[arg(long)]
        advertise_host: Option<String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct AttachRecord {
    session: String,
    wprsc_pid: u32,
    ssh_pid: Option<u32>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = Client::new(&cli.url);
    match cli.command {
        Command::Ls => print_result(client.call(RequestCommand::ListSessions).await?),
        Command::Run { app, name } => print_result(
            client
                .call(RequestCommand::Run { app_id: app, name })
                .await?,
        ),
        Command::Attach { session } => {
            attach(&client, &cli.wprsc, cli.ssh.as_deref(), session).await
        }
        Command::Detach { session } => detach(&client, session.as_deref()).await,
        Command::Kill { session } => {
            print_result(client.call(RequestCommand::Kill { session }).await?)
        }
        Command::Token {
            qr,
            rotate,
            advertise_host,
        } => show_token(&cli.url, qr, rotate, advertise_host.as_deref()),
    }
}

fn show_token(url: &str, qr: bool, rotate: bool, advertise_host: Option<&str>) -> Result<()> {
    // Resolve everything QR rendering needs before touching the token file:
    // `--rotate` invalidates every paired client, and `load_or_create` may
    // write a brand-new token to disk, so a fallible check like the advertise
    // host must run first. Otherwise a failure here would leave the operator
    // with an invalidated or newly-minted token they were never shown.
    let advertised_endpoint = if qr {
        let parsed = url::Url::parse(url).context("could not parse --url")?;
        let port = parsed.port().unwrap_or(9417);
        let host = resolve_advertise_host(url, advertise_host)?;
        Some((host, port))
    } else {
        None
    };

    let path = navette_auth::default_token_path().context("cannot determine a token path")?;
    let token = if rotate {
        let token = navette_auth::AuthToken::rotate(&path)?;
        eprintln!("Token rotated. Every paired client must pair again.");
        eprintln!("Restart navetted for this to take effect: it holds the value in memory.");
        token
    } else {
        navette_auth::AuthToken::load_or_create(&path)?
    };

    if let Some((host, port)) = advertised_endpoint {
        let uri = pairing_uri(&host, port, &token.render());
        let code = qrcode::QrCode::new(uri.as_bytes())?;
        println!(
            "{}",
            code.render::<qrcode::render::unicode::Dense1x2>().build()
        );
    }
    println!("{}", token.render_grouped());
    Ok(())
}

fn pairing_uri(host: &str, port: u16, token: &str) -> String {
    format!("navette://pair?host={host}&port={port}&token={token}")
}

/// The daemon cannot derive its own reachable name: the phone connects over the
/// tailnet, where that is a MagicDNS name or tailnet IP, not `uname -n`, and
/// there is no Tailscale integration to ask. So use the URL host when it is
/// already a specific non-loopback address, and otherwise refuse.
fn resolve_advertise_host(url: &str, advertise: Option<&str>) -> Result<String> {
    let host = match advertise {
        Some(host) => host.to_owned(),
        None => {
            let parsed = url::Url::parse(url).context("could not parse --url")?;
            let host = parsed.host_str().context("--url has no host")?;
            let is_loopback = host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .map(|address| address.is_loopback())
                    .unwrap_or(false);
            if is_loopback {
                bail!(
                    "--url points at {host}, which the phone cannot reach; pass --advertise-host with the name or address the phone should dial"
                );
            }
            host.to_owned()
        }
    };

    // Validated once, at the single exit, because the property that matters is
    // what reaches `pairing_uri` -- not which branch produced it. Validating only
    // the explicit flag leaves the --url path open: `&` is NOT a forbidden host
    // code point in the WHATWG URL spec the `url` crate implements, so
    // `--url ws://tower&evil.example:9417/v1/ws` survives `host_str()` intact.
    //
    // Validate rather than percent-encode: `pairing_uri` interpolates this
    // straight into a query string, so `&`, `#`, `?` or `/` would yield a URI
    // the Android parser reads differently than intended. Encoding would force
    // the phone side to share a decoding convention; rejecting needs no
    // agreement between them. No real hostname or IP contains these characters
    // -- brackets and colons are allowed for IPv6 literals.
    let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']');
    if host.is_empty() || !host.chars().all(legal) {
        bail!("{host:?} is not a valid hostname or address");
    }
    Ok(host)
}

fn print_result(result: ResponseResult) -> Result<()> {
    let rendered = render_result(&result);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    Ok(())
}

async fn attach(client: &Client, wprsc: &str, ssh: Option<&str>, session: String) -> Result<()> {
    let response = client
        .call(RequestCommand::Attach {
            session: session.clone(),
        })
        .await?;
    let ResponseResult::Attach { attach } = response else {
        bail!("daemon returned an unexpected response to attach");
    };

    let outcome = run_attachment(wprsc, ssh, &attach).await;
    let detach_result = client.call(RequestCommand::Detach { session }).await;
    outcome?;
    detach_result?;
    Ok(())
}

async fn run_attachment(wprsc: &str, ssh: Option<&str>, attach: &AttachInfo) -> Result<()> {
    let record_path = attach_record_path(&attach.session)?;
    let mut forward = None;
    let mut forward_dir = None;
    let socket_path = if let Some(target) = ssh {
        let directory = TempDir::new().context("failed to create SSH forwarding directory")?;
        let local_socket = directory.path().join("wprs.sock");
        let mut child = spawn_process(
            "ssh",
            &[
                "-o".into(),
                "ExitOnForwardFailure=yes".into(),
                "-N".into(),
                "-L".into(),
                format!("{}:{}", local_socket.display(), attach.socket_path),
                target.into(),
            ],
        )?;
        wait_for_socket(&local_socket, &mut child).await?;
        forward = Some(child);
        forward_dir = Some(directory);
        local_socket
    } else {
        PathBuf::from(&attach.socket_path)
    };

    let mut wprsc_child =
        match spawn_process(wprsc, &[format!("--socket={}", socket_path.display())]) {
            Ok(child) => child,
            Err(error) => {
                stop_child(&mut forward).await;
                return Err(error);
            }
        };
    let record = AttachRecord {
        session: attach.session.clone(),
        wprsc_pid: wprsc_child.id().context("wprsc PID is unavailable")?,
        ssh_pid: forward.as_ref().and_then(Child::id),
    };
    if let Err(error) = persist_record(&record_path, &record) {
        let _ = wprsc_child.kill().await;
        let _ = wprsc_child.wait().await;
        stop_child(&mut forward).await;
        return Err(error);
    }

    let status = tokio::select! {
        status = wprsc_child.wait() => Some(status.context("failed to wait for wprsc")?),
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl-C")?;
            terminate_record(&record);
            None
        }
    };
    let _ = wprsc_child.wait().await;
    stop_child(&mut forward).await;
    drop(forward_dir);
    remove_record_if_owned(&record_path, record.wprsc_pid);

    if let Some(status) = status
        && !status.success()
    {
        bail!("wprsc exited with {status}");
    }
    Ok(())
}

async fn stop_child(child: &mut Option<Child>) {
    if let Some(mut child) = child.take() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

fn spawn_process(program: &str, args: &[String]) -> Result<Child> {
    let mut command = TokioCommand::new(program);
    command.args(args);
    command.as_std_mut().process_group(0);
    command
        .spawn()
        .with_context(|| format!("failed to start {program}"))
}

async fn wait_for_socket(path: &Path, child: &mut Child) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if path.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait().context("failed to poll ssh")? {
            bail!("ssh forwarding exited before creating its socket: {status}");
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            bail!("timed out waiting for SSH Unix-socket forwarding");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn detach(client: &Client, requested: Option<&str>) -> Result<()> {
    let (path, record) = find_record(requested)?;
    terminate_record(&record);
    let result = client
        .call(RequestCommand::Detach {
            session: record.session.clone(),
        })
        .await?;
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    print_result(result)
}

fn attach_record_root() -> Result<PathBuf> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is unset")?;
    Ok(PathBuf::from(runtime).join("navette/clients"))
}

fn attach_record_path(session: &str) -> Result<PathBuf> {
    validate_session_component(session)?;
    Ok(attach_record_root()?.join(format!("{session}.json")))
}

fn persist_record(path: &Path, record: &AttachRecord) -> Result<()> {
    let parent = path.parent().context("attach record has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    fs::write(path, serde_json::to_vec(record)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn find_record(requested: Option<&str>) -> Result<(PathBuf, AttachRecord)> {
    let root = attach_record_root()?;
    let paths = if let Some(session) = requested {
        validate_session_component(session)?;
        vec![root.join(format!("{session}.json"))]
    } else {
        fs::read_dir(&root)
            .context("no tracked attachments")?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect()
    };
    if paths.len() != 1 {
        bail!("specify a session when zero or multiple attachments are tracked");
    }
    let path = paths.into_iter().next().expect("length checked");
    let record = serde_json::from_slice(&fs::read(&path).with_context(|| {
        format!(
            "no tracked attachment for {}",
            requested.unwrap_or("session")
        )
    })?)
    .with_context(|| format!("invalid attach record {}", path.display()))?;
    Ok((path, record))
}

fn validate_session_component(session: &str) -> Result<()> {
    let valid = (1..=64).contains(&session.len())
        && session
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && session.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if !valid {
        bail!("invalid session name: {session}");
    }
    Ok(())
}

fn terminate_record(record: &AttachRecord) {
    terminate_owned_process(record.wprsc_pid, "wprsc");
    if let Some(pid) = record.ssh_pid {
        terminate_owned_process(pid, "ssh");
    }
}

fn terminate_owned_process(pid: u32, expected: &str) {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm"));
    if comm.is_ok_and(|comm| comm.trim() == expected)
        && let Ok(pid) = i32::try_from(pid)
    {
        let _ = kill(Pid::from_raw(-pid), Signal::SIGTERM);
    }
}

fn remove_record_if_owned(path: &Path, pid: u32) {
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(current) = serde_json::from_slice::<AttachRecord>(&bytes) else {
        return;
    };
    if current.wprsc_pid == pid {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ls() {
        let cli = Cli::try_parse_from(["navette", "ls"]).unwrap();
        assert!(matches!(cli.command, Command::Ls));
    }

    #[test]
    fn parses_run_with_app_and_name() {
        let cli = Cli::try_parse_from(["navette", "run", "firefox", "--name", "work"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run { app, name }
                if app == "firefox" && name.as_deref() == Some("work")
        ));
    }

    #[test]
    fn parses_attach_with_session() {
        let cli = Cli::try_parse_from(["navette", "attach", "work-browser"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Attach { session } if session == "work-browser"
        ));
    }

    #[test]
    fn parses_detach_without_session() {
        let cli = Cli::try_parse_from(["navette", "detach"]).unwrap();
        assert!(matches!(cli.command, Command::Detach { session: None }));
    }

    #[test]
    fn parses_detach_with_session() {
        let cli = Cli::try_parse_from(["navette", "detach", "work-browser"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Detach { session: Some(session) } if session == "work-browser"
        ));
    }

    #[test]
    fn parses_global_transport_options() {
        let cli = Cli::try_parse_from([
            "navette",
            "--url",
            "ws://tower:9417/v1/ws",
            "--ssh",
            "tower",
            "ls",
        ])
        .unwrap();
        assert_eq!(cli.url, "ws://tower:9417/v1/ws");
        assert_eq!(cli.ssh.as_deref(), Some("tower"));
    }

    #[test]
    fn rejects_run_without_app() {
        assert!(Cli::try_parse_from(["navette", "run"]).is_err());
    }

    #[test]
    fn rejects_session_path_traversal_for_tracking() {
        assert!(validate_session_component("../../record").is_err());
        assert!(validate_session_component("work_browser-2").is_ok());
    }

    #[test]
    fn parses_token_with_flags() {
        let cli = Cli::try_parse_from([
            "navette",
            "token",
            "--qr",
            "--rotate",
            "--advertise-host",
            "tower.ts.net",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Token { qr, rotate, advertise_host }
                if qr && rotate && advertise_host.as_deref() == Some("tower.ts.net")
        ));
    }

    #[test]
    fn builds_a_pairing_uri() {
        assert_eq!(
            pairing_uri("tower.example.ts.net", 9417, "ABCD1234ABCD1234ABCD1234"),
            "navette://pair?host=tower.example.ts.net&port=9417&token=ABCD1234ABCD1234ABCD1234"
        );
    }

    #[test]
    fn uses_a_specific_non_loopback_url_host_when_no_flag_is_given() {
        // Reachable by construction: the client is already talking to it.
        let host = resolve_advertise_host("ws://100.64.0.3:9417/v1/ws", None).unwrap();
        assert_eq!(host, "100.64.0.3");
    }

    #[test]
    fn requires_the_flag_when_the_url_host_is_loopback() {
        // A QR saying 127.0.0.1 scans cleanly and then fails to connect, which
        // presents as an auth bug. Refuse rather than guess.
        let error = resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", None).unwrap_err();
        assert!(error.to_string().contains("--advertise-host"));
    }

    #[test]
    fn the_flag_always_wins() {
        let host =
            resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", Some("tower.ts.net")).unwrap();
        assert_eq!(host, "tower.ts.net");
    }

    #[test]
    fn rejects_an_advertise_host_containing_an_ampersand() {
        // `&` would let a crafted --advertise-host inject an extra query
        // parameter into the navette://pair URI.
        let error = resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", Some("tower&evil.example"))
            .unwrap_err();
        assert!(error.to_string().contains("tower&evil.example"));
    }

    #[test]
    fn rejects_an_advertise_host_containing_a_hash() {
        // `#` would truncate the URI at the Android side's fragment parser,
        // silently dropping the token from what gets read.
        let error =
            resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", Some("tower#evil")).unwrap_err();
        assert!(error.to_string().contains("tower#evil"));
    }

    #[test]
    fn rejects_an_empty_advertise_host() {
        let error = resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", Some("")).unwrap_err();
        assert!(error.to_string().contains("not a valid hostname"));
    }

    #[test]
    fn rejects_an_ampersand_carried_in_via_url_when_no_flag_is_given() {
        // Validation must apply regardless of which branch produced the host:
        // `&` is not a forbidden host code point in the WHATWG URL spec, so it
        // survives `Url::host_str()` intact and would otherwise reach
        // `pairing_uri` unvalidated on this path. This is the regression this
        // test guards -- it fails against a version that only validates the
        // explicit `--advertise-host` branch.
        let error = resolve_advertise_host("ws://tower&evil.example:9417/v1/ws", None).unwrap_err();
        assert!(error.to_string().contains("tower&evil.example"));
    }

    #[test]
    fn accepts_ordinary_advertise_host_values() {
        for host in ["tower.ts.net", "100.64.0.3", "[fd7a::1]"] {
            assert_eq!(
                resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", Some(host)).unwrap(),
                host,
                "expected {host} to be accepted"
            );
        }
    }

    #[test]
    fn qr_encoder_accepts_a_realistic_pairing_uri() {
        // This is what catches a payload the encoder rejects outright, distinct
        // from the string-building checked by `builds_a_pairing_uri`.
        let uri = pairing_uri(
            "tower.example.ts.net",
            9417,
            "000G40R40M30E209185GR38E-ABCD",
        );
        assert!(qrcode::QrCode::new(uri.as_bytes()).is_ok());
    }
}
