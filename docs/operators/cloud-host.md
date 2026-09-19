# Cloud host recipe

Turn a headless VM into a Navette host you can attach to from the phone or
any Linux client on your tailnet. One user-data file, about two minutes.

## What you get

- `navetted`, `navette`, `wprsd` and `xwayland-xdg-shell` in `/usr/local/bin`,
  from the release tarball built by `.github/workflows/release.yml`.
- A `navette` service user with linger enabled, running `navetted` as a user
  unit (`/etc/systemd/user/navetted.service`) that restarts on failure and
  survives reboots.
- The daemon bound to the host's **tailnet IPv4 only**. Nothing of Navette's
  listens on the public address; if `ufw` is present it is enabled to admit
  only `tailscale0`, SSH and Tailscale's own WireGuard port.
- On Debian, Firefox preinstalled (`firefox-esr`) so the drawer is not empty.
  Ubuntu gets no default app; see [Adding apps](#adding-apps) for why. Every
  GUI app you `apt install` afterwards appears too, after a daemon restart.
- Software H.264 encoding (`libx264`) since cloud VMs have no GPU; the
  bridge probes VA-API first and falls back on its own.

## Prerequisites

- A **Tailscale auth key**: [admin console → Settings → Keys](https://login.tailscale.com/admin/settings/keys).
  Make it **single-use, pre-approved and ephemeral or short-lived**: cloud
  user-data is readable from the instance metadata service by anything running
  on the VM, so treat the key as spent the moment the VM boots.
- An **x86_64 VM running Debian 12 or Ubuntu 22.04+**. Debian 12 is the
  smoother choice: Ubuntu's `firefox` deb is a snap shim, and snap-confined
  apps cannot reach wprsd's Wayland socket at all (details under Adding
  apps). 2 vCPU / 4 GB is plenty for a browser session; x264 is CPU-bound, so
  more cores mean smoother video.
- A published Navette release tag (`vX.Y.Z`) to install.

## Hetzner walkthrough

1. **Create the server.** Debian 12 image, any x86 type, your SSH key. Under
   *Cloud config* paste [`contrib/cloud/cloud-init.yaml`](../../contrib/cloud/cloud-init.yaml)
   with the two values filled in:

   ```yaml
   NAVETTE_VERSION=v0.1.0
   TS_AUTHKEY=tskey-auth-...
   ```

   EC2 (*Advanced details → User data*) and GCP (`user-data` metadata key)
   take the same file unchanged.

2. **Wait about two minutes.** The server appears in the Tailscale admin
   console under its hostname when the recipe has joined the tailnet. Progress
   is in `/var/log/cloud-init-output.log`; the recipe's own output, ending with
   the pairing command, is in `/var/log/navette-host-init.log`. Neither log
   contains the token: only `navette token`, run as the service user, prints
   it.

3. **Pair the phone.** SSH in and run the command the log ends with:

   ```bash
   sudo -H -u navette navette token --qr --advertise-host <host>.<tailnet>.ts.net
   ```

   It prints the token and a QR code; scan it from the Android client's
   pairing screen. `-H` pins `HOME` to the service user's regardless of
   sudoers policy: the token file lives in `~navette/.local/state/navette/`,
   and a `navette token` run with root's `HOME` would mint a second token the
   daemon has never seen.

Open the drawer, tap Firefox. The session persists on the VM between attaches
and between devices.

## Running the script by hand

`cloud-init.yaml` only bootstraps: it downloads the release tarball and its
`SHA256SUMS`, verifies, extracts
[`contrib/cloud/navette-host-init.sh`](../../contrib/cloud/navette-host-init.sh)
from the verified tarball into `/usr/local/sbin/navette-host-init`, and runs
it against that local copy. The script does the work, and you can run it on
any existing Debian/Ubuntu box:

```bash
sudo NAVETTE_VERSION=v0.1.0 TS_AUTHKEY=tskey-auth-... ./navette-host-init.sh
sudo NAVETTE_VERSION=v0.1.0 TS_AUTHKEY=... ./navette-host-init.sh --dry-run   # print, touch nothing
```

Inputs are environment variables; `--help` lists them. `NAVETTE_APPS` is the
space-separated apt list of GUI apps to preinstall (`""` for none);
`NAVETTE_PORT` moves the daemon off 9417; `NAVETTE_TARBALL` points at a local
or `https://` tarball instead of GitHub and must be verifiable, either by a
`SHA256SUMS` beside it or by `NAVETTE_TARBALL_SHA256=<hex>`; without one the
script refuses. Re-running is safe: packages, user, linger and firewall rules
are no-ops when already in place; the tarball is fetched and all four binaries
reinstalled every time, but the daemon is restarted only when one of them, the
wrapper or the unit actually changed.

## Updates

Re-run with a new `NAVETTE_VERSION`. Tailscale is already logged in, so no
auth key is needed:

```bash
sudo NAVETTE_VERSION=v0.2.0 /usr/local/sbin/navette-host-init
```

On a cloud-init host you can instead edit `/etc/navette-host-init.env` and run
`/usr/local/sbin/navette-host-bootstrap`, which also refreshes the script
itself from the new tarball. Either way the new tarball is downloaded and
verified, the binaries swapped, and `navetted` restarted only if something
installed actually changed. A restart stops the daemon's whole cgroup, running
`wprsd` sessions included, so relaunch apps from the drawer afterwards. The
installed version and wprs revision are in
`/usr/local/lib/navette/{VERSION,WPRS_REV}`.

## What listens where

| Address | Port | What |
|---------|------|------|
| tailnet IPv4 (`tailscale ip -4`) | `NAVETTE_PORT` (9417) | `navetted` WebSocket API, bearer token required |
| all interfaces | 41641/udp | `tailscaled` WireGuard; answers nothing unauthenticated |
| public IPv4/IPv6 | 22/tcp | sshd (your provider's default) |
| public IPv4/IPv6 | anything else | nothing |

`/usr/local/bin/navetted-tailnet` resolves the tailnet address at every start
and refuses to run without one; the unit's `Restart=always` retries every
five seconds until Tailscale is up. Transport is plaintext WebSocket inside
WireGuard, the same trust model as [`RUNBOOK.md`](../RUNBOOK.md#deployment)
describes for a home host. If `ufw` is installed the script sets *deny
incoming*, *allow in on tailscale0*, *allow 22/tcp*, *allow 41641/udp* (so
peers reach WireGuard directly instead of relaying video through DERP), then
enables it; an sshd on a port other than 22 needs its own rule first or you
lock yourself out. If `ufw` is absent the script says so and leaves the
firewall to you.

## Adding apps

```bash
sudo apt install gimp
sudo systemctl --user -M navette@.host restart navetted
```

Anything that installs a `.desktop` file under `/usr/share/applications` shows
up in the drawer after that restart: `navetted` scans the XDG application
directories once at startup, and only lists entries whose executable is on the
unit's `PATH`. X11-only apps work through `xwayland-xdg-shell`.

**Snaps do not work.** snapd's `wayland` interface only lets a confined app
open sockets named `$XDG_RUNTIME_DIR/wayland-N`; navetted's per-session
displays are named `navette-<session>`, so a snap Firefox launches and dies
without ever connecting. That is why Ubuntu hosts get no default app, and why
`apt install firefox` on Ubuntu 22.04+ (a snap shim) is not the answer. Install
the deb from Mozilla's apt repository instead
([support.mozilla.org: Install Firefox on Linux → Debian-based](https://support.mozilla.org/kb/install-firefox-linux#w_install-firefox-deb-package-for-debian-based-distributions)),
which publishes `firefox` from `packages.mozilla.org`; then restart the unit.

## Troubleshooting

```bash
sudo systemctl --user -M navette@.host status navetted      # -M user@ talks to that user's manager
sudo journalctl _SYSTEMD_USER_UNIT=navetted.service -f  # daemon, wprsd and ffmpeg output
tailscale status                                       # Running? our IP? peers?
tailscale ip -4                                        # what the wrapper binds
ffmpeg -hide_banner -encoders | grep -E 'x264|vaapi'   # libx264 must be listed
cat /usr/local/lib/navette/VERSION /usr/local/lib/navette/WPRS_REV
sudo cat /var/log/navette-host-init.log                # the recipe's own run (cloud-init hosts only)
```

- **Unit flapping with "no tailnet IPv4 yet"**: Tailscale is not logged in.
  `tailscale status` says `NeedsLogin` when the auth key was already used or
  expired; get a fresh key and `sudo tailscale up --authkey=...`.
- **`systemctl --user -M navette@.host` says "Failed to connect to bus"**: the user
  manager is not running; `loginctl show-user navette` should show
  `Linger=yes`. Re-run the script, or `sudo loginctl enable-linger navette`.
  (Plain `sudo -u navette systemctl --user` fails the same way for a different
  reason: sudo does not set `XDG_RUNTIME_DIR`. Use `-M navette@.host`.)
- **Drawer is empty**: no `.desktop` entry under `/usr/share/applications` has
  its executable on the unit's `PATH`, or apps were installed after the daemon
  started; install something and restart the unit.
- **App never appears / session dies at once**: the journal shows whether
  `wprsd` or `xwayland-xdg-shell` failed to exec; both must be in
  `/usr/local/bin` and `Xwayland` in `/usr/bin` (`apt install xwayland`).
- **Video never starts**: check the `ffmpeg -encoders` line above; the
  `ffmpeg` package on Debian and Ubuntu ships `libx264`, but a hand-built
  ffmpeg may not.
- **Pairing QR says the wrong host or port**: `--advertise-host` only
  overrides the host. With a non-default `NAVETTE_PORT`, pass
  `--url ws://127.0.0.1:<port>/v1/ws --advertise-host <host>` as the script's
  final output does; see the table in [`RUNBOOK.md`](../RUNBOOK.md#pairing).
