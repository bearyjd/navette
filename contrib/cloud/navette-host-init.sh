#!/usr/bin/env bash
# navette-host-init: turn a headless Debian/Ubuntu x86_64 VM into a Navette host.
#
# Installs the release tarball (navetted, navette, wprsd, xwayland-xdg-shell)
# to /usr/local/bin, joins the tailnet, creates a lingering service user and
# runs navetted as a user unit bound to the tailnet address only. Nothing is
# exposed publicly. Re-running is safe; re-running with a new NAVETTE_VERSION
# is how you upgrade.
#
# Inputs (environment):
#   NAVETTE_VERSION  required; release tag, e.g. v0.1.0
#   TS_AUTHKEY       Tailscale auth key; required unless already logged in or SKIP_TAILSCALE=1
#   TS_HOSTNAME      tailnet machine name           (default: system hostname)
#   NAVETTE_USER     service user, uid >= 1000      (default: navette)
#   NAVETTE_PORT     navetted port on the tailnet IP (default: 9417)
#   NAVETTE_APPS     apt packages of GUI apps to preinstall, space-separated
#                    (default: firefox-esr on Debian, none on Ubuntu; "" for none)
#   NAVETTE_TARBALL  local path or https:// URL of a release tarball instead of
#                    GitHub; needs SHA256SUMS beside it or NAVETTE_TARBALL_SHA256
#   NAVETTE_TARBALL_SHA256  expected sha256 (hex) of NAVETTE_TARBALL
#   SKIP_TAILSCALE=1 test only: no Tailscale, bind 127.0.0.1 instead
#
# Flags: --dry-run (print what each step would do, touch nothing), --help
set -euo pipefail
# A restrictive inherited umask (cloud-init's bootstrap runs under 077) would
# leave the Tailscale apt keyring unreadable by apt's _apt user.
umask 022

NAVETTE_VERSION="${NAVETTE_VERSION:-}"
TS_AUTHKEY="${TS_AUTHKEY:-}"
TS_HOSTNAME="${TS_HOSTNAME:-$(hostname)}"
NAVETTE_USER="${NAVETTE_USER:-navette}"
NAVETTE_PORT="${NAVETTE_PORT:-9417}"
NAVETTE_TARBALL="${NAVETTE_TARBALL:-}"
NAVETTE_TARBALL_SHA256="${NAVETTE_TARBALL_SHA256:-}"
SKIP_TAILSCALE="${SKIP_TAILSCALE:-0}"
DRY_RUN=0

readonly RELEASE_BASE="https://github.com/bearyjd/navette/releases/download"
readonly TARGET="x86_64-unknown-linux-gnu"
readonly BIN_DIR="/usr/local/bin"
readonly WRAPPER="$BIN_DIR/navetted-tailnet"
readonly STAMP_DIR="/usr/local/lib/navette"
readonly UNIT_PATH="/etc/systemd/user/navetted.service"
readonly DEFAULT_PORT=9417
readonly MIN_SERVICE_UID=1000
readonly MAX_SERVICE_UID=60000  # login.defs UID_MAX; excludes nobody (65534)
readonly MANAGER_WAIT_SECONDS=15
readonly BINS=(navetted navette wprsd xwayland-xdg-shell)
# Xwayland is what xwayland-xdg-shell execs for X11 apps; libxkbcommon0 is the
# only shared library the wprs binaries link; dbus-user-session gives the
# lingering user a session bus; jq parses `tailscale status --json`.
readonly RUNTIME_PKGS=(ffmpeg xwayland libxkbcommon0 dbus-user-session fonts-dejavu-core ca-certificates curl jq)
# https only, including across redirects (GitHub release downloads redirect).
readonly CURL=(curl --proto '=https' --proto-redir '=https' --retry 3 -fsSL)

CHANGED=0          # set when an installed file differs from the previous run
OS_ID=""
OS_CODENAME=""
SERVICE_HOME=""
WORK_DIR=""

cleanup() { if [[ -n "$WORK_DIR" ]]; then rm -rf "$WORK_DIR"; fi; }
trap cleanup EXIT

log() { printf '==> %s\n' "$*"; }
note() { printf '    %s\n' "$*"; }
die() { printf 'navette-host-init: %s\n' "$*" >&2; exit 1; }

# Runs a command, or prints it under --dry-run.
run() {
    if (( DRY_RUN )); then note "would run: $*"; else "$@"; fi
}

# Installs stdin to $1 with mode $2 (and optional owner $3), tracking CHANGED.
write_file() {
    local path="$1" mode="$2" owner="${3:-root:root}" tmp
    if (( DRY_RUN )); then note "would write $path (mode $mode, owner $owner)"; cat >/dev/null; return; fi
    tmp="$(mktemp)"
    cat >"$tmp"
    if ! cmp -s "$tmp" "$path" 2>/dev/null; then CHANGED=1; fi
    install -D -m "$mode" -o "${owner%%:*}" -g "${owner##*:}" "$tmp" "$path"
    rm -f "$tmp"
}

have_systemd() { [[ -d /run/systemd/system ]] && command -v systemctl >/dev/null; }

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

parse_args() {
    local arg
    for arg in "$@"; do
        case "$arg" in
            --dry-run) DRY_RUN=1 ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown argument: $arg (see --help)" ;;
        esac
    done
}

validate_service_user() {
    local uid
    [[ "$NAVETTE_USER" =~ ^[a-z_][a-z0-9_-]{0,31}$ ]] || die "NAVETTE_USER '$NAVETTE_USER' is not a valid user name"
    [[ "$NAVETTE_USER" != root ]] || die "NAVETTE_USER must not be root"
    if uid="$(id -u "$NAVETTE_USER" 2>/dev/null)"; then
        (( uid >= MIN_SERVICE_UID && uid < MAX_SERVICE_UID )) \
            || die "NAVETTE_USER '$NAVETTE_USER' is not a regular account (uid $uid); the service must run as a regular user"
    fi
}

validate_tarball_override() {
    [[ -n "$NAVETTE_TARBALL" ]] || return 0
    [[ "$NAVETTE_TARBALL" != http://* ]] || die "NAVETTE_TARBALL must be an https:// URL or a local path"
    if [[ -n "$NAVETTE_TARBALL_SHA256" && ! "$NAVETTE_TARBALL_SHA256" =~ ^[0-9a-fA-F]{64}$ ]]; then
        die "NAVETTE_TARBALL_SHA256 must be 64 hex digits"
    fi
}

validate_inputs() {
    log "Validating inputs"
    [[ -n "$NAVETTE_VERSION" ]] || die "NAVETTE_VERSION is required (a release tag such as v0.1.0)"
    [[ "$NAVETTE_VERSION" =~ ^[A-Za-z0-9._-]+$ ]] || die "NAVETTE_VERSION '$NAVETTE_VERSION' is not a tag"
    if [[ "$SKIP_TAILSCALE" != 1 && -z "$TS_AUTHKEY" ]] && ! tailscale_running; then
        die "TS_AUTHKEY is required (or set SKIP_TAILSCALE=1 for a loopback-only test install)"
    fi
    [[ "$NAVETTE_PORT" =~ ^[0-9]+$ && "$NAVETTE_PORT" -ge 1 && "$NAVETTE_PORT" -le 65535 ]] \
        || die "NAVETTE_PORT must be 1-65535, got '$NAVETTE_PORT'"
    validate_service_user
    validate_tarball_override
    if [[ -z "$NAVETTE_TARBALL" && "$(uname -m)" != x86_64 ]]; then
        die "releases are built for x86_64 only; this host is $(uname -m)"
    fi
    (( DRY_RUN )) || [[ "$EUID" -eq 0 ]] || die "run as root (sudo)"
    note "version=$NAVETTE_VERSION user=$NAVETTE_USER port=$NAVETTE_PORT tailscale=$([[ "$SKIP_TAILSCALE" == 1 ]] && echo skipped || echo "$TS_HOSTNAME")"
}

# Prints one field of /etc/os-release without polluting this shell.
os_release_field() {
    # shellcheck source=/dev/null
    ( . /etc/os-release && printf '%s' "${!1:-}" )
}

check_distro() {
    log "Checking distribution"
    [[ -r /etc/os-release ]] || die "/etc/os-release missing; only Debian/Ubuntu are supported"
    local like
    OS_ID="$(os_release_field ID)"
    like="$(os_release_field ID_LIKE)"
    OS_CODENAME="$(os_release_field VERSION_CODENAME)"
    [[ "$OS_ID" == debian || " $like " == *" debian "* || " $like " == *" ubuntu "* ]] \
        || die "unsupported distribution '$OS_ID'; this recipe targets Debian 12 / Ubuntu 22.04+"
    note "$OS_ID ($OS_CODENAME)"
}

default_apps() {
    # Debian ships Firefox as a deb (firefox-esr). Ubuntu's `firefox` deb is a
    # snap shim, and snap's wayland interface only admits sockets named
    # wayland-N, never navetted's navette-<session>, so it cannot connect;
    # Ubuntu hosts get no default app (see docs/operators/cloud-host.md).
    if [[ "$OS_ID" == debian ]]; then echo "firefox-esr"; else echo ""; fi
}

install_packages() {
    local app_list="${NAVETTE_APPS-$(default_apps)}"
    local -a pkgs=("${RUNTIME_PKGS[@]}") apps=()
    local token
    # NAVETTE_APPS is a space-separated list by contract; split it without
    # globbing, then refuse anything apt could read as an option.
    set -f
    # shellcheck disable=SC2206
    apps=($app_list)
    set +f
    for token in "${apps[@]}"; do
        [[ "$token" =~ ^[a-z0-9][a-z0-9+.-]+$ ]] || die "NAVETTE_APPS entry '$token' is not an apt package name"
    done
    pkgs+=("${apps[@]}")
    log "Installing packages: ${pkgs[*]}"
    export DEBIAN_FRONTEND=noninteractive
    run apt-get -q -o DPkg::Lock::Timeout=300 update
    run apt-get -q -y -o DPkg::Lock::Timeout=300 install -- "${pkgs[@]}"
}

tailscale_state() {
    tailscale status --json 2>/dev/null | jq -r '.BackendState // empty' || true
}

# True once this host is logged in; upgrades then need no TS_AUTHKEY.
tailscale_running() {
    command -v tailscale >/dev/null && [[ "$(tailscale_state)" == Running ]]
}

install_tailscale_package() {
    if command -v tailscale >/dev/null; then
        note "tailscale already installed"
        return
    fi
    case "$OS_ID" in
        debian|ubuntu) ;;
        *) die "no Tailscale apt repo for '$OS_ID'; install tailscale manually and re-run" ;;
    esac
    [[ -n "$OS_CODENAME" ]] || die "VERSION_CODENAME missing from /etc/os-release"
    local base="https://pkgs.tailscale.com/stable/$OS_ID/$OS_CODENAME"
    run "${CURL[@]}" "$base.noarmor.gpg" -o /usr/share/keyrings/tailscale-archive-keyring.gpg
    run "${CURL[@]}" "$base.tailscale-keyring.list" -o /etc/apt/sources.list.d/tailscale.list
    run apt-get -q -o DPkg::Lock::Timeout=300 update
    run apt-get -q -y -o DPkg::Lock::Timeout=300 install tailscale
}

join_tailnet() {
    log "Joining tailnet as $TS_HOSTNAME"
    if (( DRY_RUN )); then
        note "would run: tailscale up --authkey=<redacted> --hostname=$TS_HOSTNAME (unless already Running)"
        return
    fi
    if tailscale_running; then
        note "already logged in"
        return
    fi
    # tailscaled was started by the package postinst a moment ago and may not
    # be accepting connections yet; a bad key still fails within 15s.
    local attempt
    for attempt in 1 2 3 4 5; do
        tailscale up --authkey="$TS_AUTHKEY" --hostname="$TS_HOSTNAME" && return
        note "tailscale up failed (attempt $attempt/5); retrying in 3s"
        sleep 3
    done
    die "tailscale up failed; check the auth key (single-use keys are spent on first use)"
}

install_tailscale() {
    if [[ "$SKIP_TAILSCALE" == 1 ]]; then
        log "Skipping Tailscale (SKIP_TAILSCALE=1); navetted will bind loopback only"
        return
    fi
    log "Installing Tailscale"
    install_tailscale_package
    join_tailnet
}

is_url() { [[ "$1" == https://* ]]; }

# Fetches tarball + SHA256SUMS into $1, echoing the tarball's basename.
# A NAVETTE_TARBALL is verified against a SHA256SUMS beside it or an explicit
# NAVETTE_TARBALL_SHA256; without either it is refused, never trusted.
fetch_release() {
    local dir="$1" src base
    if [[ -n "$NAVETTE_TARBALL" ]]; then
        src="$NAVETTE_TARBALL"
        base="$(basename -- "${src%%\?*}")"
        if is_url "$src"; then
            "${CURL[@]}" -o "$dir/$base" "$src"
            "${CURL[@]}" -o "$dir/SHA256SUMS" "${src%/*}/SHA256SUMS" 2>/dev/null || rm -f "$dir/SHA256SUMS"
        else
            [[ -f "$src" ]] || die "NAVETTE_TARBALL '$src' does not exist"
            cp -- "$src" "$dir/$base"
            [[ -f "$(dirname -- "$src")/SHA256SUMS" ]] && cp -- "$(dirname -- "$src")/SHA256SUMS" "$dir/"
        fi
        if [[ -n "$NAVETTE_TARBALL_SHA256" ]]; then
            printf '%s  %s\n' "$NAVETTE_TARBALL_SHA256" "$base" > "$dir/SHA256SUMS"
        fi
        [[ -f "$dir/SHA256SUMS" ]] \
            || die "no SHA256SUMS beside NAVETTE_TARBALL and no NAVETTE_TARBALL_SHA256 given; refusing an unverified tarball"
    else
        base="navette-$NAVETTE_VERSION-$TARGET.tar.gz"
        "${CURL[@]}" -o "$dir/$base" "$RELEASE_BASE/$NAVETTE_VERSION/$base"
        "${CURL[@]}" -o "$dir/SHA256SUMS" "$RELEASE_BASE/$NAVETTE_VERSION/SHA256SUMS"
    fi
    echo "$base"
}

install_release() {
    log "Installing Navette $NAVETTE_VERSION to $BIN_DIR"
    if (( DRY_RUN )); then
        note "would fetch ${NAVETTE_TARBALL:-$RELEASE_BASE/$NAVETTE_VERSION/navette-$NAVETTE_VERSION-$TARGET.tar.gz}"
        note "would verify SHA256SUMS, then install ${BINS[*]} (0755 root) and $STAMP_DIR/{VERSION,WPRS_REV}"
        return
    fi
    local tmp base bindir bin
    WORK_DIR="$(mktemp -d)"
    tmp="$WORK_DIR"
    base="$(fetch_release "$tmp")"
    (cd "$tmp" && sha256sum -c --ignore-missing SHA256SUMS) || die "checksum verification failed for $base"
    mkdir "$tmp/x"
    tar -xzf "$tmp/$base" -C "$tmp/x"
    bindir="$(find "$tmp/x" -type d -name bin -print -quit)"
    [[ -n "$bindir" ]] || die "no bin/ directory inside $base"
    for bin in "${BINS[@]}"; do
        [[ -f "$bindir/$bin" ]] || die "tarball lacks bin/$bin"
        if ! cmp -s "$bindir/$bin" "$BIN_DIR/$bin" 2>/dev/null; then CHANGED=1; fi
        install -m 0755 -o root -g root "$bindir/$bin" "$BIN_DIR/$bin"
    done
    install -d -m 0755 "$STAMP_DIR"
    for bin in VERSION WPRS_REV; do
        [[ -f "$bindir/../$bin" ]] && install -m 0644 "$bindir/../$bin" "$STAMP_DIR/$bin"
    done
    note "installed ${BINS[*]}$( [[ -f "$STAMP_DIR/WPRS_REV" ]] && echo " (wprs $(cut -c1-12 "$STAMP_DIR/WPRS_REV"))" )"
}

create_user() {
    log "Creating service user $NAVETTE_USER"
    if id -u "$NAVETTE_USER" >/dev/null 2>&1; then
        note "user exists"
    else
        run useradd --create-home --shell /bin/bash "$NAVETTE_USER"
    fi
    SERVICE_HOME="$(getent passwd "$NAVETTE_USER" 2>/dev/null | cut -d: -f6 || true)"
    SERVICE_HOME="${SERVICE_HOME:-/home/$NAVETTE_USER}"
    if have_systemd; then
        run loginctl enable-linger "$NAVETTE_USER"
    else
        # logind's on-disk record; honoured at first boot when building an image.
        note "no systemd: recording linger in /var/lib/systemd/linger instead"
        run install -d -m 0755 /var/lib/systemd/linger
        run touch "/var/lib/systemd/linger/$NAVETTE_USER"
    fi
}

write_wrapper() {
    log "Writing $WRAPPER"
    local resolve
    if [[ "$SKIP_TAILSCALE" == 1 ]]; then
        resolve='ip=127.0.0.1  # TEST ONLY: installed with SKIP_TAILSCALE=1; unreachable from any phone'
    else
        # shellcheck disable=SC2016 # expanded by the wrapper at run time, not here
        resolve='ip="$(tailscale ip -4 2>/dev/null | sed -n 1p || true)"'
    fi
    write_file "$WRAPPER" 0755 <<EOF
#!/usr/bin/env bash
# Generated by navette-host-init.sh; re-run it to regenerate rather than editing.
# Binds navetted to this host's tailnet IPv4 only. Exits 1 while no tailnet
# address exists yet so the user unit's Restart=always keeps retrying.
set -euo pipefail
port=$NAVETTE_PORT
$resolve
if [[ -z "\$ip" ]]; then
    echo "navetted-tailnet: no tailnet IPv4 yet; is tailscaled up and logged in?" >&2
    exit 1
fi
exec $BIN_DIR/navetted --bind "\$ip:\$port" --allow-remote --wprsd $BIN_DIR/wprsd "\$@"
EOF
}

write_unit() {
    log "Writing $UNIT_PATH"
    # Root-owned in /etc/systemd/user, which every user manager reads, so
    # nothing root writes lives in the service user's home. Derived from
    # contrib/systemd/navetted.service: no graphical-session ordering on a
    # headless host, and PATH is pinned so wprsd finds xwayland-xdg-shell,
    # Xwayland and ffmpeg.
    write_file "$UNIT_PATH" 0644 <<EOF
[Unit]
Description=Navette GUI session daemon (tailnet-bound cloud host)
Documentation=https://github.com/bearyjd/navette
# The wrapper exits 1 until the tailnet address exists; never rate-limit that.
StartLimitIntervalSec=0

[Service]
Type=simple
ExecStart=$WRAPPER
Restart=always
RestartSec=5
Environment=PATH=/usr/local/bin:/usr/bin:/bin

[Install]
WantedBy=default.target
EOF
}

# systemctl --user for the service user, from root.
user_systemctl() {
    local uid="$1"; shift
    runuser -u "$NAVETTE_USER" -- env XDG_RUNTIME_DIR="/run/user/$uid" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" systemctl --user "$@"
}

wait_for_user_manager() {
    local uid="$1" i
    for (( i = 0; i < MANAGER_WAIT_SECONDS; i++ )); do
        [[ -S "/run/user/$uid/systemd/private" ]] && return 0
        (( i == 0 )) && systemctl start "user@$uid.service" 2>/dev/null || true
        sleep 1
    done
    return 1
}

# What `systemctl --user enable` would create, made by the user themselves so
# root never writes into their home; picked up at first boot.
enable_without_systemd() {
    note "skipping service activation (no systemd); enabling via default.target.wants for first boot"
    local wants="$SERVICE_HOME/.config/systemd/user/default.target.wants"
    runuser -u "$NAVETTE_USER" -- mkdir -p "$wants"
    runuser -u "$NAVETTE_USER" -- ln -sfn "$UNIT_PATH" "$wants/navetted.service"
}

enable_service() {
    log "Enabling navetted user service"
    if (( DRY_RUN )); then
        note "would enable + start navetted.service as $NAVETTE_USER (restart if installed files changed)"
        return
    fi
    if ! have_systemd; then
        enable_without_systemd
        return
    fi
    local uid
    uid="$(id -u "$NAVETTE_USER")"
    wait_for_user_manager "$uid" || die "user manager for uid $uid did not start within ${MANAGER_WAIT_SECONDS}s"
    user_systemctl "$uid" daemon-reload
    user_systemctl "$uid" enable navetted.service
    if user_systemctl "$uid" is-active --quiet navetted.service; then
        if (( CHANGED )); then note "restarting (installed files changed)"; user_systemctl "$uid" restart navetted.service
        else note "already running"; fi
    else
        user_systemctl "$uid" start navetted.service
    fi
}

configure_firewall() {
    log "Configuring firewall"
    if ! command -v ufw >/dev/null; then
        note "ufw not installed; skipping (navetted listens on the tailnet address only regardless)"
        return
    fi
    if [[ "$SKIP_TAILSCALE" == 1 ]]; then
        note "SKIP_TAILSCALE=1; leaving ufw untouched"
        return
    fi
    # 22/tcp: an sshd on another port needs its own rule before this, or
    # `ufw --force enable` locks you out. 41641/udp: tailscaled's WireGuard
    # port, so peers connect directly instead of relaying video through DERP;
    # WireGuard answers nothing unauthenticated.
    run ufw default deny incoming
    run ufw allow in on tailscale0
    run ufw allow 22/tcp
    run ufw allow 41641/udp
    run ufw --force enable
}

print_summary() {
    local ip="127.0.0.1" host="$TS_HOSTNAME" dns
    if [[ "$SKIP_TAILSCALE" != 1 ]] && ! (( DRY_RUN )); then
        ip="$(tailscale ip -4 2>/dev/null | sed -n 1p || true)"
        dns="$(tailscale status --json 2>/dev/null | jq -r '.Self.DNSName // empty' || true)"
        [[ -n "$dns" ]] && host="${dns%.}"
    fi
    log "Done"
    note "tailnet:   $host ${ip:+($ip)}"
    note "navetted:  ws://${ip:-<tailnet-ip>}:$NAVETTE_PORT/v1/ws (tailnet only; nothing is exposed publicly)"
    note "service:   sudo systemctl --user -M $NAVETTE_USER@.host status navetted"
    note "logs:      sudo journalctl _SYSTEMD_USER_UNIT=navetted.service -f"
    echo
    echo "Pair a phone (prints the token, then a QR to scan):"
    if [[ "$NAVETTE_PORT" == "$DEFAULT_PORT" ]]; then
        echo "  sudo -H -u $NAVETTE_USER navette token --qr --advertise-host $host"
    else
        echo "  sudo -H -u $NAVETTE_USER navette token --qr --url ws://127.0.0.1:$NAVETTE_PORT/v1/ws --advertise-host $host"
    fi
}

main() {
    parse_args "$@"
    (( DRY_RUN )) && log "DRY RUN: nothing will be changed"
    validate_inputs
    check_distro
    install_packages
    install_tailscale
    # Spent by now; keep it out of the environment the service user's
    # systemctl calls below inherit.
    unset TS_AUTHKEY
    install_release
    create_user
    write_wrapper
    write_unit
    enable_service
    configure_firewall
    print_summary
}

main "$@"
