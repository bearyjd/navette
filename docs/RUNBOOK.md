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

### `navette token`

Local admin command: it reads the daemon's token file directly rather than
dialling the daemon over the API, so it works whether or not `navetted` is
currently running, and it only ever operates on **this machine's** token
file — there is no `--url` for it.

```bash
navette token                                       # print it
navette token --qr                                  # print it, then render a QR
navette token --qr --advertise-host <host>          # required if navetted binds 0.0.0.0 or loopback
navette token --rotate                              # generate a new one
navette token --rotate --qr --advertise-host <host> # rotate and re-pair in one step
```

- **`--qr`** renders a `navette://pair?host=…&port=…&token=…` URI as a
  terminal QR code, so one scan on the phone delivers the whole connection,
  not just the secret. The token always prints first, above the QR — if
  rendering the QR fails, that degrades to a warning rather than losing the
  token, so a QR failure never means the operator doesn't get the token at
  all.
- **`--advertise-host`** tells the command what host the phone should dial.
  `navetted` can't always work this out itself: the phone reaches it over the
  tailnet, at a MagicDNS name or tailnet IP that has nothing to do with the
  machine's own hostname. If `navetted` is bound to a specific non-loopback
  address, that address is used automatically. If it's bound to `0.0.0.0` or
  loopback, `--advertise-host` is **required** for `--qr`, and the command
  errors naming the flag rather than guessing a host that would scan cleanly
  and then fail to connect.
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

Android pairs by scanning the QR from `navette token --qr`, or by manual
host/token entry as a fallback for devices without Google Play Services.
Either path produces the same stored pairing; scanning a new QR (or entering
one manually) replaces whatever was paired before, since the app holds one
host at a time.

## Health checks

<!-- AUTO-GENERATED: from crates/navetted/src/api.rs routes -->

| Endpoint | Method | Purpose |
|---|---|---|
| `/healthz` | GET | Liveness |
| `/v1/ws` | WS | Control socket (JSON request/response) |
| `/v1/sessions/{session}/media` | WS | Media socket (binary frames + JSON text) |

<!-- END AUTO-GENERATED -->

```bash
curl -fsS http://127.0.0.1:9417/healthz
```

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
