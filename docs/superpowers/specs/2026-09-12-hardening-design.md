# Hardening: browser origin, API authentication, allocation ceilings — Design

**Status:** approved, not yet implemented
**Blocks:** `docs/superpowers/specs/2026-09-11-bulk-transport-design.md` (by decision,
not by dependency — see §1)

## Why this exists

While designing API-wide authentication, a live vulnerability turned up in shipped
code: **any web page the user visits can drive the daemon**, in the default
configuration, with no flags. That reordered the work. This branch closes it,
adds the authentication that was always marked as missing, and bounds an unbounded
allocation found on the way.

The three items ship as one branch by decision: one coherent security story, one
review pass, rather than a hotfix followed by a design cycle.

## 1. Scope and order

1. `Origin` rejection (§2) — the live hole.
2. API-wide bearer token (§3–§5).
3. wprs allocation ceilings (§6).

Ordered so the exploitable hole closes first even if review sends later items back.

Bulk transport is specced and parked at
`docs/superpowers/specs/2026-09-11-bulk-transport-design.md`, and waits for this
branch by choice. Its §7 argued that blob routes do not *widen* the boundary, which
remains true — but it also pointed at API-wide auth as the real remedy, and that
remedy is this document. **When this lands, that §7 goes stale and must be updated.**

**The daemon's enforcement and the Android pairing flow must land together.** The
moment §4 is enforced, every client that cannot present a token is locked out, and
the phone is the client that cannot read the token file. A partial merge — daemon
first, Android later — leaves the phone unable to connect at all. Either the whole
branch merges as a unit, or enforcement sits behind a flag until the clients catch
up. This design assumes the former, because it is a single-user project with a
sideloaded app; a plan that splits them must add the flag instead.

## 2. Reject browser-originated requests

**The finding.** `grep -rn "origin\|Origin\|cors\|Host"` across `crates/navetted/src/`
returns nothing, and the router (`api.rs:77-80`) carries no middleware of any kind.
`media_websocket` (`api.rs:84`) validates a session name and nothing else.
`websocket` (`api.rs:252`) requires a WebSocket subprotocol, which is **not** a
defense: a browser sets one with `new WebSocket(url, [...])`.

Browsers do not apply CORS preflight to WebSocket handshakes. They open the
connection and leave rejection to the server. So while `navetted` runs on loopback —
the documented default — any visited page can attach to `/v1/sessions/{s}/media`,
receive the screen, and inject input. Session names are user-chosen and guessable,
and the control socket enumerates them anyway.

**The rule.** Any request carrying an `Origin` header is refused with **403**, on
every route including `/healthz`. navette has no browser client, so this is a
blanket rejection, not a policy with an allowlist to maintain. That our own three
clients send no `Origin` becomes a test, not an assumption.

**`Host` validation is deliberately not added.** An earlier draft of this design
proposed it; that is reversed here. DNS rebinding is a browser attack, and a browser
always sends `Origin` on a WebSocket handshake, so the rebinding vector is already
closed by the rule above. A non-browser attacker does not need rebinding — it dials
the IP directly. Meanwhile a `Host` allowlist breaks the ordinary case, where a
client legitimately connects to `tower.tailnet.ts.net` and the daemon has no way to
know that name. It would be fiddly, breakage-prone, and buy nothing.

**Residual, named rather than left implicit.** `<img src>` issues a cross-site GET
carrying no `Origin`. It cannot read the response, and no GET route has side
effects, so there is nothing to gain. If a future route ever gains a side-effecting
GET, this residual becomes live — a constraint on future work, recorded here.

**Design consequence worth preserving.** An "unauthenticated loopback, token for
remote" split was very nearly proposed, on the reasoning that a loopback TCP port is
equivalent to a 0700 Unix socket. **It is not.** A 0700 socket is unreachable from a
web page; a loopback TCP port is not. The token in §4 therefore applies to every
route with **no loopback exemption**.

## 3. Token: generation, storage, presentation

**Value.** 120 bits from a CSPRNG, rendered Crockford base32 (excluding `I`, `L`,
`O`, `U`) as six groups of four — 24 characters, typable as the §5 fallback.

**Storage.** `$XDG_STATE_HOME/navette/token`, created with
`OpenOptions::new().create_new(true).mode(0o600)`, generated on first start when
absent. Its own file, **not** `registry.json`: that file sets no explicit mode
(`registry.rs` has no `mode(` or `set_permissions` call), so it lands at umask
default, typically 0644. Tightening `registry.json` as a side effect of this work is
out of scope; keeping the secret out of it is the point.

**A token file that exists but cannot be read or parsed is a fatal startup error**,
never a trigger to regenerate. Silently minting a fresh token on a malformed read
would invalidate every paired client the first time a disk hiccup or a truncated
write corrupted the file, and the operator would see only that every device stopped
working. Fail loudly, name the path, and let a human decide to rotate.

**Presentation.** `navette token` prints it. `navette token --qr` renders a QR in
the terminal. `navette token --rotate` regenerates, which invalidates every paired
client. **Rotation takes effect on daemon restart**, and the command says so: the
running daemon holds the value in memory and does not watch the file. Adding a
reload path would mean a config-watching mechanism nothing else needs.

**QR payload** is a `navette://pair?host=…&port=…&token=…` URI, so one scan delivers
the whole connection rather than only the secret. The daemon may be bound to
`0.0.0.0` and so may not know its own reachable name: `--advertise-host` overrides,
defaulting to the system hostname.

**Never logged**, at any level, in either language. Startup logs that a token was
loaded, never its value. This is the same discipline the clipboard branch
established for clipboard content, now covering a second secret.

## 4. Verification

`Authorization: Bearer <token>` on every route **except** `/healthz`, which stays
open as a liveness probe: it reveals only that navetted is running, and it is still
`Origin`-rejected under §2.

WebSocket handshakes carry headers, so one mechanism covers `/v1/ws`, the media
socket, and the future blob routes. `api.rs:595` already builds test requests with
custom headers, so the harness needs no new shape.

Comparison is **constant-time**, via the `subtle` crate's `ConstantTimeEq` rather
than a hand-rolled loop an optimizer is free to short-circuit. A byte-by-byte
early-exit compare on a secret hands out a timing oracle. Failure is a bare **401**
with no detail about why.

## 5. Clients

**CLI and desktop viewer** read the token file by default. When `--url` points
somewhere non-loopback, an explicit `--token` or `NAVETTE_TOKEN` is **required**:
silently sending the local host's token to a remote daemon would leak it to whatever
is listening there.

**Android** pairs by QR through `com.google.android.gms:play-services-code-scanner`.
This requires **no camera permission** — scanning runs inside Play Services' own UI —
so it needs no CameraX, no custom scanner screen, and no permission flow. The module
downloads on demand.

It does require Google Play Services, so **manual entry stays as a fallback** for
devices without them, which is also why §3 chose a typable token format.

Storage is `EncryptedSharedPreferences`. One honest caveat:
`androidx.security:security-crypto` has sat at 1.1.0-alpha for years and is
effectively in maintenance. It remains the house rule
(`~/.claude/rules/kotlin/security.md`) and the standard answer, and the token is
rotatable, so this follows it rather than hand-rolling Keystore code.

The app currently persists **nothing** — no DataStore, no SharedPreferences anywhere
under `android/app/src/main/kotlin/` — and `ConnectScreen.kt:24` defers a saved-host
registry to M4. This branch therefore adds a **minimal single-host credential
store**; M4 generalizes it to many hosts later.

Because the store holds one host, **scanning a new QR replaces whatever was
paired**, host and token together. That is the honest behaviour for a single-slot
store and it needs saying, because the alternative a user might expect — accumulating
hosts — is exactly what M4 adds and this does not. The replacement is visible in the
UI rather than silent.

## 6. wprs allocation ceilings

**The finding, verified.** `streaming_framed_decompress_with` reads
`uncompressed_size` via `usize::framed_read` — which is a **u32** on the wire
(`wprs/src/serialization/framing.rs:67-75`) — and hands it to `decompress_impl`,
which allocates exactly that much (`wprs/src/sharding_compression.rs:434-441`:
`if uncompressed_size > self.buffer.len() { self.buffer = DivBufShared::from(vec![0; uncompressed_size]); }`).
Ceiling is 4 GB, allocated before any content is validated. navetted links the
bridge in-process, so an OOM here takes down every session, not one.

This governs **every** message navetted reads and predates all clipboard work.

**Two call sites, two ceilings**, patched on our pin and upstreamable:

- `streaming_framed_decompress_with` (`:154`) — `MessageType::Object`. Carries
  metadata *and* clipboard `DataToTransfer`, since `bridge.rs:457` reads it as
  `RecvType::Object`. Bulk transport's future 64 MB blob sets the floor. **80 MB.**
- `streaming_framed_decompress_to_owned` (`:175`) — `MessageType::RawBuffer`.
  Framebuffers, roughly 33 MB for 4K at 32bpp. **128 MB** for multi-monitor headroom.

**Retention, confirmed rather than assumed.** `decompress_impl` only ever grows the
buffer — there is no shrink path — and `ShardingDecompressor::new`
(`serialization/mod.rs:301`) is constructed **once, before the read loop**, so it
lives for the whole connection. Both call sites share that single buffer.

The consequence is high-water-mark retention: one large message keeps the buffer
inflated for the connection's lifetime. With the ceilings in place, worst-case
retention becomes **the larger ceiling, 128 MB per connection**, instead of 4 GB.
That is acceptable, and the growth-without-shrink behaviour is a deliberate
reuse optimization for steady-size framebuffers rather than a bug. **No shrink path
is added** — bounding the high-water mark is what the ceilings already accomplish.

## 7. Failure modes

**A 401 is terminal, not retryable.** `ReconnectPolicy.kt` backs off and retries
today; without this distinction a rotated token produces an invisible infinite
reconnect loop — the phone spinning forever while the daemon refuses every attempt.
The policy must separate terminal authentication failure from transient network
failure, surface "pairing rejected", and offer a re-scan.

This is the integration point most likely to be got wrong and the one whose failure
is least visible, so it carries its own tests rather than riding on the auth tests.

A missing token file on a client is a clear startup error naming `navette token`,
not a silent unauthenticated attempt.

## 8. Testing

- `Origin` rejection on every route, and proof our own clients send no `Origin` —
  the rule is worthless if our own traffic trips it, and "OkHttp does not set it"
  is currently an assumption.
- Token accepted, token rejected, absent header rejected, all with 401 and no detail.
- Token file is created `0600`.
- The token reaches no log line, in either language, by the scan discipline the
  clipboard branch established.
- **401 is terminal in `ReconnectPolicy`** — asserted directly, since the failure
  mode is an invisible loop.
- wprs ceilings reject an oversize declared size and accept the legitimate maximum
  at each call site.
- QR payload round-trips through parse, including a rejected malformed URI.

## 9. Not in scope

- **TLS / `wss`.** The tailnet supplies WireGuard encryption in transit.
  `--allow-remote` onto a non-tailnet network stays plaintext and earns a loud
  startup warning rather than silent exposure.
- **Per-client tokens and revocation.** One daemon-wide rotatable token is
  proportionate to a single-user tool with three clients; per-client tokens need a
  management surface nothing is asking for.
- **`Host` validation**, per §2.
- **Tailscale peer identity as the auth mechanism.** `tailnet` appears only in
  Android UI copy; there is no Tailscale integration in any crate and the CLI is not
  installed on the development host. It would be a new hard dependency that still
  would not cover loopback, which now requires auth too.
- **M4's multi-host registry.** §5 adds the single-host store it will generalize.
- **Tightening `registry.json`'s mode.** Noted in §3, left to its own change.
