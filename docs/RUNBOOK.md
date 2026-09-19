# Runbook

Operating `navetted` and its clients. For first-time M1 setup and the safety
boundaries around the unauthenticated API, see
[`docs/operators/m1.md`](operators/m1.md).

## Deployment

There is no container, orchestrator, or deploy pipeline. `navetted` is a
single binary run on the host whose apps you want to shuttle.

```bash
cargo build --release
./target/release/navetted --bind 127.0.0.1:9417
```

Building is optional. Every `v*` tag publishes
`navette-<tag>-x86_64-unknown-linux-gnu.tar.gz` (plus `SHA256SUMS`) on the
GitHub Releases page via `.github/workflows/release.yml`: `navetted`,
`navette`, and the matching `wprsd` and `xwayland-xdg-shell` built on Ubuntu
22.04 so they run on Debian 12 / Ubuntu 22.04 or newer. For a headless cloud
VM, [`docs/operators/cloud-host.md`](operators/cloud-host.md) wraps that
tarball in `contrib/cloud/navette-host-init.sh` and a `cloud-init.yaml`:
tailnet join, service user, user unit, firewall, pairing command.

To reach it from a phone or another machine, bind a **tailnet** address:

```bash
./target/release/navetted --bind <tailnet-ip>:9417 --allow-remote
```

> `--allow-remote` acknowledges that binding beyond loopback exposes the API
> to the whole network the address is reachable on, not only the tailnet.
> Every route now requires the bearer token described in Pairing below — there
> is no unauthenticated control surface any more — but the transport itself is
> still plaintext: there is no TLS termination or gateway in the path, so a
> tailnet (WireGuard-encrypted) remains the intended boundary.

### Client connection

```bash
navette ls                       # via NAVETTE_URL or --url
navette run <app-id>
navette-viewer --url ws://<host>:9417
```

Android: scan the daemon's pairing QR (or enter host/token manually), pick an
app from the drawer. See Pairing below.

## Pairing

Every route requires a bearer token — `/healthz` included, no loopback
exemption. This is what an operator needs to generate, present, and rotate
one.

### Where the token lives

`$XDG_STATE_HOME/navette/token` (falls back to `~/.local/state/navette/token`
when `XDG_STATE_HOME` is unset), created mode `0600` the first time
`navetted` starts and finds none there. It is deliberately its own file, not
folded into `registry.json` — that file sets no explicit mode and so lands at
the umask default (typically `0644`), which is not a place to keep a secret.

A token file that exists but cannot be read or parsed is a **fatal startup
error**, not a trigger to mint a fresh one: silently regenerating on a
corrupt read would invalidate every paired client the moment a disk hiccup or
a truncated write touched the file, and the only symptom would be every
device failing at once. Fail loudly, name the path, and let a human decide to
rotate.

A token file that is **group- or other-readable is refused**, by the daemon and
by the clients alike, naming the path and the mode. It is created `0600`; a
later `chmod` or a restore from a backup that flattened modes is the way it
stops being one, and a secret every local account can read is not a credential.
Fix with `chmod 600` on the path the error names.

**Only `navetted` and `navette token` ever create the file.** `navette` and
`navette-viewer` read it and error if it is absent, naming `navette token`.
This asymmetry is deliberate. If a client minted one, then deleting the token
file under a running daemon would play out like this: the daemon still holds
the old value in memory, `navette ls` writes a brand-new one and gets a 401,
and restarting the daemon to "fix" that makes it adopt the new value —
invalidating every paired phone, with nothing in the logs explaining why.

### `navette token`

Local admin command: it reads the daemon's token file directly rather than
dialling the daemon over the API, so it works whether or not `navetted` is
currently running, and it only ever operates on **this machine's** token file.
Add `--token-file <path>` if `navetted` was started with one.

`--url` is a global flag and so applies here too — but it means something
different for this command than for `navette ls`. Nothing is dialled; `--url`
is read purely as *a description of the endpoint the phone should dial*, and
**both** the host and the port in the QR come from it. It defaults to
`ws://127.0.0.1:9417/v1/ws`.

```bash
navette token                                       # print it
navette token --qr                                  # print it, then render a QR
navette token --qr --advertise-host tower.ts.net    # QR says tower.ts.net:9417
navette token --qr --url ws://tower.ts.net:19417/v1/ws  # QR says tower.ts.net:19417
navette token --rotate                              # generate a new one
navette token --rotate --qr --advertise-host <host> # rotate and re-pair in one step
```

- **`--qr`** renders a `navette://pair?host=…&port=…&token=…` URI as a
  terminal QR code, so one scan on the phone delivers the whole connection,
  not just the secret. The token always prints first, above the QR — if
  rendering the QR fails, that degrades to a warning rather than losing the
  token, so a QR failure never means the operator doesn't get the token at
  all.
- **`--advertise-host`** overrides the **host** the phone should dial. The
  command cannot work this out on its own: the phone reaches the daemon over
  the tailnet, at a MagicDNS name or tailnet IP that has nothing to do with the
  machine's own hostname — and this command never talks to `navetted`, so it
  cannot see what address it bound either. What it has is `--url`, and the
  rule is:

  | `--url` host | `--advertise-host` | QR host |
  |---|---|---|
  | loopback (the default), or `localhost` | absent | **error**, naming the flag |
  | unspecified — `0.0.0.0`, `::` | absent | **error**, naming the flag |
  | loopback or unspecified | given | the flag's value |
  | a specific routable address | absent | the `--url` host |
  | a specific routable address | given | the flag's value |

  The errors are deliberate: a QR saying `127.0.0.1` scans cleanly and then
  fails to connect, which presents as an auth bug. `0.0.0.0` and `::` are the
  likelier mistake — they are what you put in `--bind` to expose the daemon —
  and they name what the daemon *binds*, not anywhere a phone can dial.

  **`--advertise-host` does not carry a port.** The port in the QR always comes
  from `--url`, so a daemon on a non-default port needs `--url` set, with or
  without the flag. `--url` at its default gives 9417; a `--url` with **no**
  port gives the scheme's default (`ws://` → 80, `wss://` → 443), not 9417,
  because that is the endpoint such a URL actually names:

  ```bash
  # navetted --bind 0.0.0.0:19417 --allow-remote, phone dials tower.ts.net
  navette token --qr --url ws://tower.ts.net:19417/v1/ws
  # equivalently, keeping --url loopback and naming the host explicitly:
  navette token --qr --url ws://127.0.0.1:19417/v1/ws --advertise-host tower.ts.net
  ```
- **`--rotate`** generates a fresh token and invalidates every paired
  client — Android, `navette`, `navette-viewer`, all of them, immediately on
  disk.

### Rotation takes effect on daemon restart

The running `navetted` holds the token in memory and does not watch the
token file, so `--rotate` changes what's on disk immediately but the running
daemon keeps accepting the *old* token until it's restarted. **After
rotating, both steps are required:**

1. Restart `navetted`.
2. Re-pair every client that isn't reading the fresh file itself — the
   Android app, and any `navette`/`navette-viewer` invocation using
   `--token`/`NAVETTE_TOKEN` rather than the local file.

There is no partial or soft rotation. Any device not re-paired after the
restart gets a 401 on its next request.

### Remote clients need an explicit token

`navette` and `navette-viewer` read the local token file by default, but
only when `--url` points at loopback. The moment `--url` points somewhere
else, an explicit `--token` (or `NAVETTE_TOKEN`) is **required** — there is
no silent fallback to the local file on a remote URL, because sending this
host's token to a remote daemon would hand the credential to whatever is
actually listening there.

```bash
navette --url ws://100.x.x.x:9417/v1/ws --token <token> ls
NAVETTE_TOKEN=<token> navette-viewer --url ws://100.x.x.x:9417
```

A rejected token reports itself as such and names `navette token`; it is not a
bare `HTTP error: 401`, which used to read like the daemon was down.

Both clients also take `--token-file <path>`, matching `navetted`'s. Use it
whenever the daemon was started with one — otherwise the clients read the
default path and present a token the daemon will reject.

Android pairs by scanning the QR from `navette token --qr`, or by manual
host/token entry as a fallback for devices without Google Play Services. The
app stores an encrypted host registry and resumes its active host at launch.
Use **Computers** from the workbench (or **Saved computers** from a failed or
unauthorized connection screen) to add, select, or delete hosts. Re-pairing
the same normalized host and port replaces that host's rotated token; another
port is a distinct host entry. Selecting a host closes the prior control
connection and leaves its session before connecting to the new one. Deleting
the active host returns to the host list without silently choosing another.

## Wake-on-LAN

A wake-on-LAN magic packet is a UDP broadcast — six `0xFF` bytes and the
target's MAC sixteen times over — and broadcasts do not cross the tailnet. A
phone or laptop that is only on the tailnet cannot wake a machine on the home
LAN by itself, so `navetted` relays: it sends the packet from the LAN it is
already on. That is the whole feature. Something on that LAN has to be awake
to do the relaying, so this wakes *other* hosts from the daemon's network, not
the daemon's own host.

### `navette wake`

```bash
navette wake aa:bb:cc:dd:ee:ff                       # from this machine, to 255.255.255.255:9
navette wake aa-bb-cc-dd-ee-ff --broadcast 192.168.1.255 --port 7
navette wake aabbccddeeff --via                      # ask navetted at --url to send it
navette --url ws://tower.ts.net:9417/v1/ws --token <token> wake aa:bb:cc:dd:ee:ff --via
```

The MAC is accepted as `aa:bb:cc:dd:ee:ff`, `aa-bb-cc-dd-ee-ff`, or
`aabbccddeeff`, in either case. It is validated before anything is sent or
dialled, in both modes, so a typo is reported as a typo.

- **Without `--via`** the packet leaves *this* machine. That only reaches the
  sleeping host if this machine is on its LAN; from the tailnet it goes
  nowhere, silently — a broadcast that reaches nobody is not an error at the
  socket. Nothing is dialled and no token is resolved, so it works with a
  remote `--url` and no `--token`, like `navette token` does.
- **With `--via`** the CLI does `POST /v1/wake` to the daemon at `--url`,
  which sends the packet from *its* LAN. This is the path for a machine that is
  on the tailnet only. It needs the same token any other daemon call needs —
  the local file for a loopback `--url`, an explicit `--token` or
  `NAVETTE_TOKEN` otherwise, exactly as in [Remote clients need an explicit
  token](#remote-clients-need-an-explicit-token). `--broadcast` and `--port`
  are forwarded when given and otherwise left to the daemon's defaults.
- **`--broadcast`** is an IPv4 literal, default `255.255.255.255`. That
  default does **not** go out every interface: Linux routes the limited
  broadcast from an unbound socket out whichever *one* interface the routing
  table picks, normally the default route's, and `ip route get 255.255.255.255`
  shows which. On a single-NIC host with a plain default route that is the
  LAN and the default works. On a multi-homed host, or on any host using a
  Tailscale exit node (whose default route is `tailscale0`), it is the wrong
  interface and the packet wakes nothing. Pass the LAN's subnet-directed
  broadcast, `--broadcast 192.168.1.255` or whatever `ip -4 addr` reports as
  `brd` on the LAN interface — or, for the relay, set it once with
  `--wake-broadcast` (below) so the phone gets it too. **`--port`** is
  1–65535, default 9. The NIC matches on the payload, not the port, so the
  default is right unless the target's firmware says otherwise.

A `204` from the daemon, or `magic packet sent` locally, means the datagram
left a socket. Neither the CLI nor the daemon can know whether the host woke;
check with a ping a few seconds later.

### Configuring the relay: `navetted --wake-broadcast`

The phone never sends a `broadcast` — it only knows the MAC — so every wake
from the app goes to the daemon's default. Given the routing behaviour above,
**a relay host that is multi-homed or behind an exit node must be started with
its LAN's subnet broadcast**, or the app's wake button silently does nothing:

```bash
./target/release/navetted --bind <tailnet-ip>:9417 --allow-remote --wake-broadcast 192.168.1.255
```

The value is checked at startup against the same rule the request path
applies (next section) and a public address is refused with a message naming
the rule, so a typo fails where you can see it rather than turning every tap
into a packet to the internet. A request that names its own `broadcast` still
overrides this default. Also note, on the phone side: re-pairing the relay on
a **new port** creates a new host entry in the app's registry, and any wake
targets configured under the old entry stay pointed at it until that entry is
deleted and the targets are set up again under the new one.

**If nothing wakes**, the packet is almost never the problem. In order of
likelihood:

1. **WoL is off in the target's firmware or driver.** It must be enabled in
   the BIOS/UEFI (*Wake on LAN*, *Power On By PCI-E*, or similar) **and** the
   NIC must have it armed at the OS level — on Linux, `ethtool <iface>` must
   show `Wake-on: g`; set it with `ethtool -s <iface> wol g` if it shows `d`.
   Many distributions reset it to `d` on every boot unless a NetworkManager
   connection profile or a udev rule re-arms it.
2. **The target is on Wi-Fi.** Wake-on-Wireless-LAN is rarely supported and
   rarer still to work. Use a wired interface.
3. **The target was powered off, not suspended,** and the firmware only wakes
   from S3. Some boards wake from S5 (soft-off) only with a separate setting.
4. **The relay sent it out the wrong interface.** The default
   `255.255.255.255` follows the routing table — see `--broadcast` above —
   and a relay behind an exit node sends it up the tunnel. Start `navetted`
   with `--wake-broadcast <LAN subnet broadcast>`.
5. **The relay is on a different broadcast domain** from the target — a VLAN,
   a guest network, a container bridge. A subnet-directed `--broadcast` helps
   only if the daemon host has an interface on that subnet.

## Health checks

<!-- AUTO-GENERATED: from crates/navetted/src/api.rs routes -->

| Endpoint | Method | Purpose |
|---|---|---|
| `/healthz` | GET | Liveness |
| `/v1/ws` | WS | Control socket (JSON request/response) |
| `/v1/sessions/{session}/media` | WS | Media socket (binary frames + JSON text) |
| `/v1/sessions/{session}/thumbnail` | GET | Latest snapshot of a live session as `image/jpeg` (≤320 px wide). `ETag` + `Cache-Control: no-cache`; a matching `If-None-Match` gets **304**. **404** when the session is not running or has not been snapshotted yet. |
| `/v1/apps/{id}/icon` | GET | The app's PNG icon as `image/png`. `ETag` + `Cache-Control: max-age=3600`; a matching `If-None-Match` gets **304**. **404** for an unknown app or one with no resolvable PNG. |
| `/v1/wake` | POST | Relay a wake-on-LAN magic packet onto the daemon's LAN |

<!-- END AUTO-GENERATED -->

**Thumbnails and icons.** A session's thumbnail is taken on its encode thread
from the last composited frame: on the first frame, then every 10 s while
frames arrive, and once more when the last media client detaches, so the
drawer shows the session as it was left. Frames keep arriving (and thumbnails
keep refreshing) while nobody is attached, as long as the application keeps
painting. Thumbnails live in memory only — a daemon restart loses them, and
the route 404s until the session paints again, so after a restart every tile
in the phone's drawer shows the app icon (or its initial) until that session
next paints. A **304** carries the `ETag` and no body. Icons are looked up
when the app index loads, PNG only (no SVG, no `index.theme` inheritance): an
absolute `Icon=` path is used as-is if it ends in `.png`; otherwise
`icons/hicolor/{128x128,96x96,64x64,48x48,256x256,32x32}/apps/<name>.png`
then `pixmaps/<name>.png`, in each of `$XDG_DATA_HOME`, `$XDG_DATA_DIRS`, and
`/var/lib/flatpak/exports/share`, first hit wins. A file is served only if it
is at most 1 MiB and starts with the PNG signature; at most four icon reads
run concurrently, later requests wait. A phone whose drawer shows a
placeholder for an app that has an icon on the host almost always has an
SVG-only theme for that app.

**`POST /v1/wake`** takes a JSON body of `{"mac": "aa:bb:cc:dd:ee:ff"}`, with
optional `"broadcast"` (an IPv4 literal, default the daemon's
`--wake-broadcast`, itself defaulting to `255.255.255.255`) and `"port"`
(1–65535, default 9). Responses:

| Status | When |
|---|---|
| **204** | The datagram left the daemon's socket. No body. |
| **400** | Body is not JSON, or the MAC, broadcast address or port does not parse (port 0 included), or the broadcast address is outside the allowed ranges. The body names the reason. |
| **401** | No or wrong bearer token, like every route. |
| **403** | The request carried an `Origin` header, i.e. came from a browser, like every route. |
| **413** | Body over axum's 2 MiB default limit. |
| **429** | Both relay slots are busy: the daemon sends at most two magic packets concurrently. Retry after a moment. |
| **500** | The send itself failed (no route, socket error). Check the daemon log. |

**Allowed broadcast addresses.** The daemon relays for any authenticated
client and must not be a UDP reflector for arbitrary addresses, so
`broadcast` is accepted only if it is `255.255.255.255`, an RFC 1918 private
address (which covers subnet-directed broadcasts such as `192.168.1.255`),
link-local (`169.254/16`), loopback, or in the CGNAT range `100.64.0.0/10`
that tailnets use. Anything else — a public address, `0.0.0.0`, multicast —
is a 400 whose body states this rule. `--wake-broadcast` is held to the same
rule at startup.

There is no way to learn from the response whether the host woke; see
[Wake-on-LAN](#wake-on-lan) for what a silent failure usually means.

```bash
curl -fsS -X POST -H "Authorization: Bearer $(navette token)" \
  -H 'Content-Type: application/json' \
  -d '{"mac":"aa:bb:cc:dd:ee:ff","broadcast":"192.168.1.255"}' \
  http://127.0.0.1:9417/v1/wake
```

`/healthz` requires the bearer token like every other route, so a bare `curl`
gets a 401 and `-f` exits non-zero — reporting a perfectly healthy daemon as
dead. Send the header:

```bash
curl -fsS -H "Authorization: Bearer $(navette token)" http://127.0.0.1:9417/healthz
```

`navette token` prints the grouped form (`ABCD-1234-…`); the daemon strips the
dashes before comparing, so it can be passed through as-is.

**Run this as the user `navetted` runs as.** `navette token` resolves the token
from `$XDG_STATE_HOME`/`$HOME`, and — unlike `navette ls` — it *creates* the
file when it finds none, because it is the local admin command that legitimately
mints one. So a liveness check running under a monitoring service account reads
*that* account's state directory, mints a stray token there, prints it, and
401s: a healthy daemon reported as dead, which is the whole failure this check
exists to avoid.

The token file is `0600` and owned by the daemon user, so there is no form of
this check that an unprivileged monitoring account can run on its own. It needs
root or the daemon user either way:

```bash
# -i, not -u: a login shell resets HOME. With plain `sudo -u`, HOME survives
# whenever sudoers sets always_set_home off or env_keep includes HOME, and
# `navette token` then mints a stray token under the *invoking* user.
curl -fsS -H "Authorization: Bearer $(sudo -iu navette navette token)" \
  http://127.0.0.1:9417/healthz

# Or read the file directly, which mints nothing at all — the safer option for
# a monitoring hook, and the one to prefer if the check runs unattended.
curl -fsS -H "Authorization: Bearer $(sudo cat ~navette/.local/state/navette/token)" \
  http://127.0.0.1:9417/healthz
```

If `navetted` was started with `--token-file`, pass the same path to the
client: `navette token --token-file <path>`, or read that path directly.

There is no metrics endpoint, no Prometheus scrape, and no alerting
integration. Observability is `RUST_LOG` output plus the client-side HUD.

### Reading the HUD

Long-press with two fingers on Android, or the viewer's HUD toggle:

```
FPS · KBPS · DEC · AGE · DROP · DISC   (+ RTT on Android)
```

| Metric | Means | Suspect when |
|---|---|---|
| `FPS` | frames composited per second | 0 with a live session → encoder or bridge stall |
| `KBPS` | payload throughput | high with low FPS → oversized frames |
| `DEC` | decode latency | climbing → client-side decode pressure |
| `AGE` | time since last frame | climbing → link or daemon stalled |
| `RTT` | phone↔daemon round trip | blank → stale (link down); measured by the socket task, not the bridge |
| `DROP` | dropped packets | non-zero → queue eviction, see below |
| `DISC` | sequence discontinuities | increments on live resize, expected |

`DEC` is **not** comparable between Android and the Linux viewer — the viewer
measures synchronous decode, Android measures queue-to-present across an async
`MediaCodec`.

## Common issues

**Session won't start / app missing from the drawer.**
`navetted` scans XDG `.desktop` directories at startup. Check the app has a
desktop entry and that `XDG_DATA_DIRS` is what you expect. `navette ls` shows
what was discovered.

**Stream is black, or nothing renders.**
Most often the encoder. VAAPI needs a render node (`/dev/dri/renderD*`) the
daemon can open; without one, fall back to software:

```bash
RUST_LOG=debug ./target/release/navetted ...   # look for the backend name
```

`EncoderBackend` is `Vaapi | Libx264 | Fake`. `Libx264` is the software path
and needs no GPU.

**`DROP` climbing on a healthy link.**
The client queue evicts under pressure and re-requests a keyframe. Sustained
drops mean the socket is not draining — check client CPU and link quality.
Note that sequence numbers are **per stream**; a HUD comparing across streams
would inflate this, which is a bug class this project has already hit.

**Keyboard input reaches the compositor but not the app.**
The guest app needs focus. Tap through the client once to give it keyboard
focus before typing.

**Viewer crashes on startup.**
The pinned `minifb` crashes on non-XKB keymaps. Known, unfixed.

**Measurements look wrong after a rebuild.**
A `navetted` left running from an earlier session is often built from an older
commit. Confirm before trusting any measurement:

```bash
find crates -name '*.rs' -newer target/release/navetted | head
```

Any output means the binary predates the source. Restart the daemon.

**Android: sideloaded APK behaves like an old build.**
Set `debuggableVariants = []` and clear `/tmp/metro-cache`.

## Rollback

No deploy system, so rollback is git plus a rebuild:

```bash
git log --oneline -20
git checkout <last-good-sha>
cargo build --release
```

Restart the daemon. Sessions do not survive a daemon restart — only the
registry file does, so running guest apps are lost. Warn users before
restarting a daemon with live sessions.

State that persists across restarts:

| What | Where |
|---|---|
| Session registry | `$XDG_STATE_HOME` or `--state-file` |
| CLI attach records | `~/.config/navette/` |

Everything else — media queues, clipboard state, stream sequences — is
in-memory and rebuilt on reconnect.

## Escalation

Single-maintainer experimental project. There is no on-call rotation, no
paging, and no SLA. Failures are triaged through GitHub issues against
`bearyjd/navette`.

Before filing, capture:

- `RUST_LOG=debug` daemon output around the failure
- the HUD reading at the time
- `git rev-parse HEAD` and whether the running binary matches it
- the encoder backend in use
