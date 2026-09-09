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

> `--allow-remote` exists because the M1 control API is **unauthenticated**.
> Binding beyond loopback exposes session control — list, run, kill — to
> anything that can reach the address. A tailnet is the intended boundary;
> there is no TLS termination, gateway, or auth in the path.

### Client connection

```bash
navette ls                       # via NAVETTE_URL or --url
navette run <app-id>
navette-viewer --url ws://<host>:9417
```

Android: enter the host in the connect screen, pick an app from the drawer.

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
