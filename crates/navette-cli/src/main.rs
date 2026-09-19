use std::env;
use std::fs;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use navette_auth::SecretString;
use navette_cli::{Client, FileTransferState, render_result};
use navette_protocol::{AttachInfo, RequestCommand, ResponseResult};
use navette_wake::{DEFAULT_BROADCAST, DEFAULT_PORT, MacAddress};
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

    /// API token. Defaults to the local token file for a loopback --url.
    #[arg(long, env = "NAVETTE_TOKEN", global = true)]
    token: Option<SecretString>,

    /// Override the API token file path. Must match navetted's --token-file.
    #[arg(long, global = true)]
    token_file: Option<PathBuf>,

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
    /// Copy a regular file into a running guest's drop directory.
    Cp {
        /// Destination session name.
        session: String,
        /// Regular file to upload.
        source: PathBuf,
        /// Filename exposed to the guest. Defaults to the source basename.
        #[arg(long)]
        name: Option<String>,
        /// Return after the daemon has accepted the upload without waiting for delivery.
        #[arg(long)]
        no_wait: bool,
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
    /// Send a wake-on-LAN magic packet, from this machine or via the daemon.
    ///
    /// By default the packet leaves this machine directly, which only works
    /// from the sleeping host's own LAN. Pass --via to have navetted send it
    /// from its LAN instead, for when this machine is on the tailnet only.
    Wake {
        /// MAC address of the host to wake: aa:bb:cc:dd:ee:ff, aa-bb-cc-dd-ee-ff or aabbccddeeff.
        mac: String,
        /// IPv4 broadcast address to send to. Defaults to 255.255.255.255.
        #[arg(long)]
        broadcast: Option<String>,
        /// UDP port to send to (1-65535). Defaults to 9.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
        /// Ask the daemon at --url to send the packet from its LAN.
        #[arg(long)]
        via: bool,
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
    if let Some(outcome) = run_local_command(&cli) {
        return outcome;
    }

    let token = navette_auth::resolve_token(
        &cli.url,
        cli.token.as_ref().map(SecretString::as_str),
        cli.token_file.as_deref(),
    )?;
    let client = Client::new(&cli.url, token);
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
        Command::Cp {
            session,
            source,
            name,
            no_wait,
        } => copy_file(&client, &session, &source, name.as_deref(), no_wait).await,
        Command::Wake {
            mac,
            broadcast,
            port,
            via: _,
        } => {
            // Parsed a second time only to unpack it: `run_local_command`
            // already refused anything here that does not parse.
            let target = parse_wake_target(&mac, broadcast.as_deref(), port)?;
            wake_via_daemon(&client, &cli.url, target).await
        }
        Command::Token { .. } => unreachable!("handled by run_local_command"),
    }
}

/// Runs the commands that must not force token resolution, ahead of it.
///
/// `token` is a local admin command that reads the daemon's token file
/// directly and never dials the daemon; `wake` without `--via` sends from
/// this machine and never dials it either. Resolving a token for them would,
/// on a remote `--url`, demand an explicit `--token` that is never sent
/// anywhere. `wake --via` does dial the daemon, but its MAC and broadcast are
/// validated here first so a typo is reported as a typo rather than as a
/// missing token; it then returns `None` and continues to the daemon path.
fn run_local_command(cli: &Cli) -> Option<Result<()>> {
    match &cli.command {
        Command::Token {
            qr,
            rotate,
            advertise_host,
        } => Some(show_token(
            &cli.url,
            *qr,
            *rotate,
            advertise_host.as_deref(),
            cli.token_file.as_deref(),
        )),
        Command::Wake {
            mac,
            broadcast,
            port,
            via,
        } => match parse_wake_target(mac, broadcast.as_deref(), *port) {
            Err(error) => Some(Err(error)),
            Ok(_) if *via => None,
            Ok(target) => Some(wake_directly(target)),
        },
        Command::Ls
        | Command::Run { .. }
        | Command::Attach { .. }
        | Command::Detach { .. }
        | Command::Kill { .. }
        | Command::Cp { .. } => None,
    }
}

/// A wake request as the operator gave it. Omitted fields stay `None` so that
/// on the `--via` path the daemon applies its own defaults, and only the
/// direct path fills them in here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WakeTarget {
    mac: MacAddress,
    broadcast: Option<Ipv4Addr>,
    port: Option<u16>,
}

/// Validates everything `wake` needs before any socket is opened or any token
/// resolved, in both modes. The port needs no check: clap already refuses 0.
fn parse_wake_target(mac: &str, broadcast: Option<&str>, port: Option<u16>) -> Result<WakeTarget> {
    let mac = mac
        .parse()
        .with_context(|| format!("invalid MAC address {mac:?}"))?;
    let broadcast = broadcast
        .map(|literal| {
            literal.parse::<Ipv4Addr>().with_context(|| {
                format!(
                    "invalid broadcast address {literal:?}: expected an IPv4 literal such as 192.168.1.255"
                )
            })
        })
        .transpose()?;
    Ok(WakeTarget {
        mac,
        broadcast,
        port,
    })
}

fn wake_directly(target: WakeTarget) -> Result<()> {
    let broadcast = target.broadcast.unwrap_or(DEFAULT_BROADCAST);
    let port = target.port.unwrap_or(DEFAULT_PORT);
    navette_wake::send_magic_packet(target.mac, broadcast, port)
        .with_context(|| format!("failed to send a magic packet to {broadcast}:{port}"))?;
    println!("magic packet sent to {} via {broadcast}:{port}", target.mac);
    Ok(())
}

async fn wake_via_daemon(client: &Client, url: &str, target: WakeTarget) -> Result<()> {
    client
        .request_wake(target.mac, target.broadcast, target.port)
        .await?;
    println!("wake request sent via {}", daemon_host(url));
    Ok(())
}

/// The host part of `--url` for the success line; the whole URL if it does
/// not parse, which `Client` will already have refused before this prints.
fn daemon_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

async fn copy_file(
    client: &Client,
    session: &str,
    source: &Path,
    name: Option<&str>,
    no_wait: bool,
) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(source)
        .await
        .with_context(|| format!("failed to read {}", source.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", source.display());
    }
    let size = metadata.len();
    if size == 0 || size > MAX_FILE_BYTES {
        bail!("{} must be between 1 byte and 64 MiB", source.display());
    }
    let name = match name {
        Some(name) => name,
        None => source
            .file_name()
            .and_then(|name| name.to_str())
            .context("source filename must be valid UTF-8; pass --name")?,
    };
    validate_file_name(name)?;
    let mime = mime_for_name(name);
    let preflight = client.preflight_file(session, name, mime, size).await?;
    let transfer_id = preflight.transfer_id.clone();

    let mut interrupt = std::pin::pin!(tokio::signal::ctrl_c());
    let uploaded = tokio::select! {
        result = &mut interrupt => {
            let _ = client.cancel_file(session, &transfer_id).await;
            result.context("failed to wait for Ctrl-C")?;
            bail!("file transfer cancelled");
        }
        result = client.upload_file(&preflight.upload_url, source, size) => result,
    };
    let uploaded = match uploaded {
        Ok(status) => status,
        Err(error) => {
            let _ = client.cancel_file(session, &transfer_id).await;
            return Err(error);
        }
    };
    if no_wait {
        print_file_status(&uploaded);
        return Ok(());
    }

    let mut status = uploaded;
    loop {
        match status.state {
            FileTransferState::Delivered => {
                print_file_status(&status);
                return Ok(());
            }
            FileTransferState::Failed | FileTransferState::Cancelled => {
                let _ = client.cancel_file(session, &transfer_id).await;
                bail!("file transfer {transfer_id} ended as {:?}", status.state);
            }
            FileTransferState::AwaitingUpload
            | FileTransferState::Queued
            | FileTransferState::Materializing => {}
        }
        tokio::select! {
            result = &mut interrupt => {
                let _ = client.cancel_file(session, &transfer_id).await;
                result.context("failed to wait for Ctrl-C")?;
                bail!("file transfer cancelled");
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        match client.file_status(session, &transfer_id).await {
            Ok(next_status) => {
                status = next_status;
                if matches!(status.state, FileTransferState::Delivered) {
                    print_file_status(&status);
                    return Ok(());
                }
                if matches!(
                    status.state,
                    FileTransferState::Failed | FileTransferState::Cancelled
                ) {
                    let _ = client.cancel_file(session, &transfer_id).await;
                    bail!("file transfer {transfer_id} ended as {:?}", status.state);
                }
            }
            Err(error) => {
                let _ = client.cancel_file(session, &transfer_id).await;
                return Err(error);
            }
        }
    }
}

fn validate_file_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        bail!("invalid file name: {name:?}");
    }
    Ok(())
}

fn mime_for_name(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension) if extension.eq_ignore_ascii_case("pdf") => "application/pdf",
        Some(extension) if extension.eq_ignore_ascii_case("txt") => "text/plain",
        Some(extension) if extension.eq_ignore_ascii_case("json") => "application/json",
        Some(extension) if extension.eq_ignore_ascii_case("png") => "image/png",
        Some(extension)
            if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") =>
        {
            "image/jpeg"
        }
        _ => "application/octet-stream",
    }
}

fn print_file_status(status: &navette_cli::FileTransferStatus) {
    println!(
        "{}\t{}\t{:?}\t{}/{}",
        status.transfer_id, status.name, status.state, status.bytes_received, status.size
    );
}

fn show_token(
    url: &str,
    qr: bool,
    rotate: bool,
    advertise_host: Option<&str>,
    token_file: Option<&Path>,
) -> Result<()> {
    // Resolve everything QR rendering needs before touching the token file:
    // `--rotate` invalidates every paired client, and `load_or_create` may
    // write a brand-new token to disk, so a fallible check like the advertise
    // host must run first. Otherwise a failure here would leave the operator
    // with an invalidated or newly-minted token they were never shown.
    let advertised_endpoint = if qr {
        Some(advertised_endpoint(url, advertise_host)?)
    } else {
        None
    };

    // Honours `--token-file` for the same reason the client paths do: an
    // operator running `navetted --token-file /X` who is shown the token from
    // the default path gets a QR the daemon rejects, which presents as a
    // pairing bug rather than a mismatched flag.
    let path = match token_file {
        Some(path) => path.to_path_buf(),
        None => navette_auth::default_token_path().context("cannot determine a token path")?,
    };
    let token = if rotate {
        let token = navette_auth::AuthToken::rotate(&path)?;
        eprintln!("Token rotated. Every paired client must pair again.");
        eprintln!("Restart navetted for this to take effect: it holds the value in memory.");
        token
    } else {
        navette_auth::AuthToken::load_or_create(&path)?
    };

    // The token is printed BEFORE anything else that can fail. Host resolution
    // already happens above the rotate, but `QrCode::new` is fallible too, and
    // leaving it between the rotation and the print reopens the same hole one
    // call later: the operator loses every pairing and never sees the
    // replacement. The QR cannot be built earlier -- its payload contains the
    // token -- so the ordering is the fix.
    println!("{}", token.render_grouped());

    if let Some((host, port)) = advertised_endpoint {
        let uri = pairing_uri(&host, port, &token.render());
        match qrcode::QrCode::new(uri.as_bytes()) {
            Ok(code) => {
                println!(
                    "{}",
                    code.render::<qrcode::render::unicode::Dense1x2>().build()
                );
            }
            // Degrade rather than fail: the token above is what pairing needs,
            // and the QR is a convenience for typing it.
            Err(error) => {
                eprintln!("could not render a QR code ({error}); pair with the token above");
            }
        }
    }
    Ok(())
}

/// The `(host, port)` pair the QR tells the phone to dial.
///
/// Both come from `--url`; `--advertise-host` overrides the host only, and
/// carries no port of its own. That asymmetry is the thing operators get wrong
/// — `--advertise-host tower.ts.net` against a daemon on a non-default port
/// yields a QR saying 9417 — so it is pinned here rather than left implicit in
/// `show_token`. The table in docs/RUNBOOK.md documents exactly this function.
///
/// The port is `port_or_known_default()`, not `port().unwrap_or(9417)`. The QR
/// must describe the endpoint `--url` describes, and `ws://tower/v1/ws` means
/// port 80 to every WebSocket client alive — substituting 9417 there would have
/// the QR quietly advertise somewhere `--url` does not point. A user who typed
/// that has a broken URL, and it should fail where they can see it rather than
/// be rewritten into a QR that scans and then cannot connect.
fn advertised_endpoint(url: &str, advertise_host: Option<&str>) -> Result<(String, u16)> {
    let parsed = url::Url::parse(url).context("could not parse --url")?;
    let port = parsed
        .port_or_known_default()
        .context("--url has no port and its scheme has no default; give it an explicit port")?;
    let host = resolve_advertise_host(url, advertise_host)?;
    Ok((host, port))
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
            // Match on `url::Host`, not on `host_str()` — the same fix
            // `resolve_token` carries in navette-auth, for the same reason: for
            // an IPv6 URL `host_str()` returns the *bracketed* form `[::1]`,
            // which `IpAddr::parse` rejects, so a string-parsing check quietly
            // classifies `ws://[::1]:9417/...` as a reachable address and emits
            // a QR pointing the phone at its own loopback.
            //
            // Unspecified addresses are refused alongside loopback: `0.0.0.0`
            // and `::` name what the daemon binds, not somewhere a phone can
            // dial. They are the more likely mistake of the two, since that is
            // exactly what an operator exposing the daemon to a tailnet puts in
            // `--bind`.
            let is_undialable = match parsed.host() {
                Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(address)) => address.is_loopback() || address.is_unspecified(),
                Some(url::Host::Ipv6(address)) => address.is_loopback() || address.is_unspecified(),
                None => false,
            };
            if is_undialable {
                bail!(
                    "--url points at {host}, which the phone cannot dial — loopback and unspecified addresses describe this host's own binding, not a reachable destination; pass --advertise-host with the name or address the phone should dial"
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
    fn parses_copy_with_a_safe_name_and_no_wait() {
        let cli = Cli::try_parse_from([
            "navette",
            "cp",
            "work",
            "/tmp/report.pdf",
            "--name",
            "guest-report.pdf",
            "--no-wait",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Cp { session, source, name, no_wait }
                if session == "work"
                    && source.as_path() == Path::new("/tmp/report.pdf")
                    && name.as_deref() == Some("guest-report.pdf")
                    && no_wait
        ));
    }

    #[test]
    fn parses_wake_with_a_mac() {
        let cli = Cli::try_parse_from(["navette", "wake", "aa:bb:cc:dd:ee:ff"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Wake { mac, broadcast: None, port: None, via: false }
                if mac == "aa:bb:cc:dd:ee:ff"
        ));
    }

    #[test]
    fn parses_wake_via_with_broadcast_and_port() {
        let cli = Cli::try_parse_from([
            "navette",
            "wake",
            "aa-bb-cc-dd-ee-ff",
            "--via",
            "--broadcast",
            "192.168.1.255",
            "--port",
            "7",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Wake { mac, broadcast: Some(broadcast), port: Some(7), via: true }
                if mac == "aa-bb-cc-dd-ee-ff" && broadcast == "192.168.1.255"
        ));
    }

    #[test]
    fn rejects_wake_port_zero_and_a_missing_mac_at_parse_time() {
        // Port 0 is not a UDP destination; the daemon answers it with 400 and
        // clap refuses it here before anything is sent or dialled.
        assert!(
            Cli::try_parse_from(["navette", "wake", "aa:bb:cc:dd:ee:ff", "--port", "0"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["navette", "wake", "aa:bb:cc:dd:ee:ff", "--port", "65536"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["navette", "wake"]).is_err());
    }

    #[test]
    fn wake_target_validation_rejects_a_bad_mac_and_broadcast_before_dialling() {
        // Like `validate_file_name`, this runs before any network activity —
        // and in `wake`'s case before token resolution, so a bad MAC on a
        // remote --url is reported as a bad MAC, not as a missing --token.
        for mac in [
            "nope",
            "",
            "aa:bb:cc:dd:ee",
            "aa:bb-cc:dd:ee:ff",
            "gg:bb:cc:dd:ee:ff",
        ] {
            let error = parse_wake_target(mac, None, None).unwrap_err();
            assert!(
                error.to_string().contains("invalid MAC address"),
                "{mac:?}: {error}"
            );
        }
        for broadcast in ["", "192.168.1", "ff02::1", "broadcast", "192.168.1.255:9"] {
            let error = parse_wake_target("aa:bb:cc:dd:ee:ff", Some(broadcast), None).unwrap_err();
            assert!(
                error.to_string().contains("invalid broadcast address"),
                "{broadcast:?}: {error}"
            );
        }
    }

    #[test]
    fn wake_target_keeps_omitted_fields_unset_for_the_daemon() {
        // Direct sends fill in the defaults at send time; the --via body must
        // leave them out so the daemon's own defaults apply.
        assert_eq!(
            parse_wake_target("AABBCCDDEEFF", None, None).unwrap(),
            WakeTarget {
                mac: "aa:bb:cc:dd:ee:ff".parse().unwrap(),
                broadcast: None,
                port: None,
            }
        );
        assert_eq!(
            parse_wake_target("aa:bb:cc:dd:ee:ff", Some("10.0.0.255"), Some(7)).unwrap(),
            WakeTarget {
                mac: "aa:bb:cc:dd:ee:ff".parse().unwrap(),
                broadcast: Some(Ipv4Addr::new(10, 0, 0, 255)),
                port: Some(7),
            }
        );
    }

    #[test]
    fn local_commands_run_before_token_resolution_and_daemon_commands_fall_through() {
        // Direct wake: handled locally, packet actually leaves, no token.
        let receiver = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let port = receiver.local_addr().unwrap().port().to_string();
        let direct = Cli::try_parse_from([
            "navette",
            "--url",
            "ws://tower.example:9417/v1/ws",
            "wake",
            "aa:bb:cc:dd:ee:ff",
            "--broadcast",
            "127.0.0.1",
            "--port",
            &port,
        ])
        .unwrap();
        assert!(matches!(run_local_command(&direct), Some(Ok(()))));
        let mut buffer = [0; 256];
        assert_eq!(receiver.recv_from(&mut buffer).unwrap().0, 102);

        // `--via` with a bad MAC: refused here, before any token is resolved.
        let bad_via = Cli::try_parse_from(["navette", "wake", "nope", "--via"]).unwrap();
        assert!(matches!(run_local_command(&bad_via), Some(Err(_))));

        // `--via` with a good MAC falls through to the daemon path.
        let via = Cli::try_parse_from(["navette", "wake", "aa:bb:cc:dd:ee:ff", "--via"]).unwrap();
        assert!(run_local_command(&via).is_none());

        // Ordinary daemon commands are never handled here.
        let ls = Cli::try_parse_from(["navette", "ls"]).unwrap();
        assert!(run_local_command(&ls).is_none());
    }

    #[test]
    fn the_via_success_line_names_the_daemon_host() {
        assert_eq!(daemon_host("ws://tower.ts.net:9417/v1/ws"), "tower.ts.net");
        assert_eq!(daemon_host("ws://[fd7a::1]:9417/v1/ws"), "[fd7a::1]");
        assert_eq!(daemon_host("not a url"), "not a url");
    }

    #[test]
    fn file_name_validation_matches_the_server_destination_contract() {
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "subdir/file",
            "subdir\\file",
            "bad\nname",
        ] {
            assert!(
                validate_file_name(name).is_err(),
                "{name:?} must be rejected"
            );
        }
        assert!(validate_file_name("quarterly report.pdf").is_ok());
    }

    #[test]
    fn file_mime_uses_a_safe_fallback() {
        assert_eq!(mime_for_name("photo.JPEG"), "image/jpeg");
        assert_eq!(mime_for_name("archive.unknown"), "application/octet-stream");
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
    fn requires_the_flag_for_the_ipv6_loopback_address() {
        // The sibling of the `resolve_token` bug in navette-auth: `host_str()`
        // on an IPv6 URL returns the bracketed `[::1]`, which `IpAddr::parse`
        // rejects -- so the old string-parsing check called this reachable and
        // emitted a QR pointing the phone at its own loopback. This test fails
        // against that version.
        let error = resolve_advertise_host("ws://[::1]:9417/v1/ws", None).unwrap_err();
        assert!(error.to_string().contains("--advertise-host"), "{error}");
    }

    #[test]
    fn requires_the_flag_for_unspecified_addresses() {
        // `0.0.0.0` and `::` name what the daemon binds, not somewhere a phone
        // can dial -- and they are what an operator exposing the daemon to a
        // tailnet actually puts in --bind, so this is the likelier mistake.
        for url in [
            "ws://0.0.0.0:9417/v1/ws",
            "ws://[::]:9417/v1/ws",
            "ws://[::0]:9417/v1/ws",
        ] {
            let error = resolve_advertise_host(url, None).unwrap_err();
            assert!(
                error.to_string().contains("--advertise-host"),
                "{url} must be refused: {error}"
            );
        }
    }

    #[test]
    fn still_accepts_specific_routable_addresses() {
        // The companion to the two refusals above: a fix that rejected every
        // address would pass both and break the only path that works. A global
        // IPv6 address in particular must survive the new `url::Host` match.
        for (url, expected) in [
            ("ws://100.64.0.3:9417/v1/ws", "100.64.0.3"),
            ("ws://[fd7a::1]:9417/v1/ws", "[fd7a::1]"),
            ("ws://[2606:4700::1111]:9417/v1/ws", "[2606:4700::1111]"),
            ("ws://tower.ts.net:9417/v1/ws", "tower.ts.net"),
        ] {
            assert_eq!(
                resolve_advertise_host(url, None).unwrap(),
                expected,
                "expected {url} to be dialable"
            );
        }
    }

    #[test]
    fn the_flag_still_wins_over_an_undialable_url() {
        // Refusing the --url host must not refuse the whole command: naming the
        // host explicitly is exactly the documented remedy, including for the
        // 0.0.0.0 bind that provokes it.
        assert_eq!(
            resolve_advertise_host("ws://0.0.0.0:9417/v1/ws", Some("tower.ts.net")).unwrap(),
            "tower.ts.net"
        );
        assert_eq!(
            resolve_advertise_host("ws://[::1]:9417/v1/ws", Some("tower.ts.net")).unwrap(),
            "tower.ts.net"
        );
    }

    #[test]
    fn the_advertised_port_is_the_one_the_url_actually_means() {
        // `ws://tower/v1/ws` dials port 80 in every WebSocket client, so that
        // is what the QR must say. `port().unwrap_or(9417)` claimed 9417 --
        // a QR advertising somewhere --url does not point.
        //
        // The 80 case looks wrong at a glance, and that is the point: a user
        // who typed a portless --url has a broken URL, and it should fail
        // visibly rather than be silently rewritten into a scannable QR that
        // then cannot connect.
        assert_eq!(
            advertised_endpoint("ws://tower.ts.net:9417/v1/ws", None).unwrap(),
            ("tower.ts.net".to_owned(), 9417)
        );
        assert_eq!(
            advertised_endpoint("ws://tower.ts.net/v1/ws", None).unwrap(),
            ("tower.ts.net".to_owned(), 80)
        );
        assert_eq!(
            advertised_endpoint("wss://tower.ts.net/v1/ws", None).unwrap(),
            ("tower.ts.net".to_owned(), 443)
        );
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
        for host in ["tower.ts.net", "my-tower.ts.net", "100.64.0.3", "[fd7a::1]"] {
            assert_eq!(
                resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", Some(host)).unwrap(),
                host,
                "expected {host} to be accepted"
            );
        }
    }

    /// Pins the table in docs/RUNBOOK.md's `navette token` section, row for
    /// row. An operator following it must get a QR that dials the right place
    /// on the first try, and the trap is that `--advertise-host` overrides the
    /// host but never the port.
    #[test]
    fn the_qr_endpoint_matches_what_the_runbook_documents() {
        const DEFAULT_URL: &str = "ws://127.0.0.1:9417/v1/ws";

        // Loopback --url, no flag: an error naming the flag, not a guess.
        let error = advertised_endpoint(DEFAULT_URL, None).unwrap_err();
        assert!(error.to_string().contains("--advertise-host"));

        // Loopback --url plus the flag: the flag's host, the --url port.
        assert_eq!(
            advertised_endpoint(DEFAULT_URL, Some("tower.ts.net")).unwrap(),
            ("tower.ts.net".to_owned(), 9417)
        );

        // A specific non-loopback --url, no flag: both come from --url.
        assert_eq!(
            advertised_endpoint("ws://tower.ts.net:19417/v1/ws", None).unwrap(),
            ("tower.ts.net".to_owned(), 19417)
        );

        // The flag wins on host, and the port still rides on --url. This is
        // the row the runbook's worked example exists for.
        assert_eq!(
            advertised_endpoint("ws://127.0.0.1:19417/v1/ws", Some("tower.ts.net")).unwrap(),
            ("tower.ts.net".to_owned(), 19417)
        );

        // A portless --url means the scheme's default port, not 9417. This row
        // asserted 9417 when it was first written, which encoded the bug in
        // `port().unwrap_or(9417)`: `ws://tower.ts.net/v1/ws` dials 80 in every
        // WebSocket client, so a QR saying 9417 described an endpoint --url did
        // not point at. See `the_advertised_port_is_the_one_the_url_actually_means`.
        assert_eq!(
            advertised_endpoint("ws://tower.ts.net/v1/ws", None).unwrap(),
            ("tower.ts.net".to_owned(), 80)
        );
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
