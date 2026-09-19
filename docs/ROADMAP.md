# Navette — Feature Roadmap

Companion to `PRP-navette.md`. Informed by a feature survey of the remote-desktop market (RustDesk, AnyDesk, TeamViewer, Parsec, Moonlight/Sunshine, NoMachine, Chrome Remote Desktop, xpra, Microsoft RDP). Draft v0.1.

---

## 1. Market read

The market splits into three camps, and Navette sits in none of them — which is the point:

| Camp | Exemplars | Model | What they optimize |
|---|---|---|---|
| Support/access | RustDesk, AnyDesk, TeamViewer, Chrome RD | Mirror a whole desktop | Ease of connection, fleet management, unattended access |
| Performance streaming | Parsec, Moonlight/Sunshine, DeskIn | Stream a whole display | Latency, frame rate, color, input devices |
| Seamless apps | xpra (X11), wprs (Wayland) | Per-app windows | Native integration, persistence |

Camp 3 has the best interaction model and the worst product completeness. Camps 1–2 define user expectations ("table stakes"). The roadmap strategy: **inherit camp 3's model via wprs, then close the table-stakes gap borrowed from camps 1–2, in priority order.**

Two structural advantages fall out of Navette's architecture for free and should be marketed, not built:

- **Unattended access & persistence** — camp 1's headline paid feature is inherent here: sessions live server-side by construction; there is nothing to "leave running and unlocked."
- **Multi-monitor** — camp 1/2 wrestle with monitor mapping; Navette's per-toplevel native windows mean the *local* compositor handles monitors, tiling, and scaling natively. Non-feature.

## 2. Feature taxonomy from the survey

**Table stakes** (every serious product has these; users will expect them):
clipboard sync (text → images/files), file transfer, audio, reconnect resilience, multiple concurrent sessions, security posture users can explain (E2EE, PIN/2FA, permission model), mobile clients, codec efficiency (H.264 baseline; AV1/HEVC where hardware allows), input completeness (keyboard layouts, IME, touch, scroll).

**Differentiators by camp** (adopt selectively):
- From Parsec/Moonlight: hardware-encode tuning, virtual display resize (already core to Navette), gamepad passthrough, high-refresh mode, performance HUD.
- From RustDesk/AnyDesk: address book (= Navette's multi-host registry), wake-on-LAN, TCP tunnel/port forward, session request/consent flows, LDAP/SSO (enterprise-only, skip).
- From NoMachine/TeamViewer: session recording, multi-user collaboration (defer), printing (skip).
- From xpra: detach/reattach semantics (core), start-new-or-attach drawer (core).

**Deliberate anti-features** (define the product by what it refuses):
whole-desktop mirroring, attended-support workflows (session codes, technician queues), remote printing, fleet/MSP management, public relay infrastructure (Tailscale/SSH only — no Navette-operated relays, no accounts).

## 3. Roadmap

Phases map onto PRP milestones; each phase lists market-derived features added beyond the PRP baseline.

### Phase 0 — Core proof (PRP M0–M1) · "it works"
- wprs adopt-vs-fork gate; supervisor, registry, XDG app index, stable WS API, CLI (`ls/run/attach/detach/kill`), Tailscale-first transport.
- *Market additions:* session metadata for the drawer (icon, title, last-attached).

### Phase 1 — Phone attach (M2–M3) · "the demo"
- Encoder bridge (VA-API H.264), Android drawer + session screen, resize-follows-viewport, reconnect UX.
- *Market additions:* ~~performance HUD (fps/bitrate/latency overlay, Parsec-style — invaluable for tuning and for credibility)~~ **done** — `SessionHud` in `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionHud.kt`; **input completeness pass** (keyboard layouts, compose/IME basics, momentum scroll); **PIN on attach** option layered above tailnet auth (defense in depth for a phone that leaves the house).

### Phase 2 — Table stakes closure (M4) · "daily driver"
- Cloud host recipe, ~~multi-host registry~~ **done** (PR #31, 2026-09-14), icons/thumbnails.
- *Market additions:* ~~**text clipboard sync** (moved from Phase 0 — the control-channel protocol exists on both sides today, `SetClipboard`/`GetClipboard` in `crates/navetted/src/api.rs`, but the daemon only echoes an in-memory `Mutex<Option<String>>` back to itself; it never reaches wprs, the guest, or the host's Wayland clipboard, and no Android UI calls it yet)~~ **done** — bidirectional text sync via `ClipboardSync` (`crates/navetted/src/clipboard.rs`), wired through the wprs data device (`crates/navetted/src/bridge.rs`) and the Android session screen (`ClipboardBridge.kt`), on-device-verified 2026-09-08. Rich types (`text/html`, images) and drag-and-drop remain out of scope; the MIME negotiation built here (`OFFERED_MIME_TYPES`) is their foundation; ~~**file transfer** (per-session drop target + `navette cp`, over the API channel — biggest gap vs. every camp-1 product)~~ **done** — `FileTransferStore` (`crates/navetted/src/file_transfers.rs`), `navette cp`, Android document picker → `FileTransferCoordinator.kt`; PR #33, 2026-09-19, unit-tested only (no on-device run yet); ~~**image clipboard**~~ **done** — session-scoped blob transport, PR #32, 2026-09-16; **wake-on-LAN** (`navette wake tower` — trivial, delightful for homelab); ~~software x264 fallback for hosts without VA-API~~ **done** — `EncoderBackend::Libx264` in `crates/navette-bridge/src/encoder.rs`; **session thumbnails as live previews** in the drawer.

### Phase 3 — Experience depth (M5) · "feels premium"
- *From PRP:* audio (PipeWire → Opus), AV1/HEVC where hardware allows, upstream dmabuf contribution, desktop GUI drawer.
- *Market additions:* **high-refresh mode** (uncap to 120 Hz on capable paths); **gamepad passthrough** (Moonlight's audience overlaps homelab users); **per-session quality profiles** (latency-first vs. quality-first vs. battery-first); **TCP tunnel** (`navette tunnel <session> <port>` — borrowed from RustDesk/AnyDesk, natural fit for dev workflows).

### Phase 4 — Reach (M6+) · "any device"
- iOS client (SwiftUI + VideoToolbox, per PRP M6).
- *Market additions:* **browser client** (WebCodecs H.264 decode + WebSocket — Chrome RD proves the demand; makes any borrowed computer a client with zero install); **session recording** (server-side, encode already exists — opt-in, for demos/audit); evaluate **collaboration/multi-attach UX** (protocol supports it; product story undefined).

### Explicitly rejected (revisit only with strong pull)
Remote printing · attended-support/session-code flows · MSP/fleet console · LDAP/SSO · Navette-operated relay or account infrastructure · whole-desktop mode.

## 4. Sequencing rationale

1. **Clipboard before file transfer before audio** — frequency-of-need ordering observed across camp-1 products' own changelogs and complaint threads.
2. **HUD early** — every performance-camp product treats metrics as a tuning prerequisite, not a luxury; it also de-risks encoder work in Phase 1–2.
3. **Browser client late but planned** — highest reach-per-effort after the encode path is mature; depends on nothing else.
4. **Anti-features enforced** — camp-1 products carry decades of support-workflow surface that Navette's single-operator, tailnet-only model makes irrelevant; refusing it keeps the codebase shippable by one person.

## 5. North-star scorecard (v1.0 definition of done)

Navette 1.0 = Phase 2 complete: a homelab operator can, from Android or any Linux desktop, browse a host's real app menu, start-or-attach named GUI sessions that survive weeks and network changes, move text/images/files in and out, wake a sleeping box, and explain the security model in one sentence ("everything rides my tailnet; nothing listens publicly").
