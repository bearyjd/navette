# PRP — Navette

**Product Requirements & Plan**
Name: **Navette** (Fr. *shuttle* — the apps stay put on the host; the shuttle carries your window to whatever device you're holding). Name final.
*Lineage note:* Navette v1 was an unrelated mobile frontend for AI coding CLIs, retired 2026 for lack of a place to live. This is a clean restart of the name; the `navette`/`navetted` client/daemon convention carries over.
Org: Grepon Labs LLC · License: AGPL-3.0-only (orchestration layer; upstream wprs is Apache-2.0, compatible) · Status: Draft v0.1

---

## 1. Problem statement

There is no "tmux for GUI apps" with a complete product experience. Terminal users take for granted that sessions live on the box, survive disconnects, and can be attached from any device. GUI apps on Wayland have no equivalent: waypipe forwards apps natively but sessions die with the connection; VNC/RDP remotes a whole desktop in a viewport, not individual native windows; and nothing offers a phone-class client at all.

**wprs** (wayland-transpositor/wprs, Apache-2.0, Rust/Smithay) solves the hard core: a server-side compositor (`wprsd`) that serializes Wayland state instead of rendering it, and a stateless local client (`wprsc`) that recreates each remote window as a genuine native window on the local compositor. Sessions survive client disconnects and client restarts. What wprs does *not* provide:

1. **Session/app orchestration** — no named sessions, no registry, no `ls`, no per-app lifecycle. One daemon, one undifferentiated pool.
2. **Discovery UX** — no way to browse what apps the remote host *could* run (its installed application menu) or what's *already* running, and pick one.
3. **Any non-Linux client** — `wprsc` is itself a Wayland client; Android/iOS/macOS/Windows have nothing to attach to.
4. **Stable protocol** — the wprs wire format is explicitly unstable across versions and even rustc builds (rkyv struct serialization).
5. **Transport/lifecycle polish** — raw SSH socket forwarding; no Tailscale-native story, no supervision, no multi-host.

Navette is the orchestration and client layer that turns wprs into a product: named per-app sessions across hosts, an app drawer built from the remote host's XDG application menu plus its live session list, and a first-class Android client — making a cloud VM feel like a desktop from any device.

## 2. Vision

> `navette ls` shows every GUI session on every one of your boxes. Tap Firefox in the app drawer on your phone; it launches on a Hetzner VM and opens as a window. Walk to the tower, attach the same session as a native Wayland window, resize it, detach. The app never died. Cloud instances become desktops without ever being desktops.

## 3. Goals / Non-goals

**Goals**
- Named, per-app GUI sessions with tmux-grade lifecycle: `run`, `ls`, `attach`, `detach`, `kill`, across multiple hosts.
- App drawer: enumerate the remote host's installed applications (XDG desktop entries — the same data KDE/GNOME menus are built from) and its running Navette sessions; one tap to start-or-attach.
- Native-feel desktop attach on Linux via wprs (real local Wayland windows, not pixel viewports).
- First-class Android client (Kotlin/Compose) that attaches to the same sessions.
- Stable Navette protocol between clients and the daemon; the daemon absorbs wprs protocol instability internally.
- Tailscale-first transport; SSH fallback.
- Cloud recipe: one script/cloud-init to make any headless VM a Navette host.

**Non-goals (v1)**
- Reimplementing Wayland state serialization (that's wprs's job; we adopt, contribute, or — only if forced — fork).
- Multi-user hosts, session sharing/collaboration, audio (deferred, see §9). iOS (M6) and Windows/macOS desktop clients (M7) are roadmapped post-v1, not v1.

## 4. Architecture

Three components. French names follow the family convention: the daemon is `navetted`, clients are `navette` (CLI/desktop) and Navette for Android.

```
┌─ phone (Navette Android) ──────┐        ┌─ remote host ────────────────────────────┐
│ Compose UI: drawer + sessions │◄──WS──►│ navetted (Rust, supervisor)               │
│ MediaCodec H.264 decode       │  TS/   │  ├─ XDG app index (.desktop entries)     │
└───────────────────────────────┘  SSH   │  ├─ session registry (name→wprsd inst.)  │
┌─ laptop (navette CLI) ─────────┐        │  ├─ wprsd instance per session ── app    │
│ launches wprsc → native       │◄──SSH──│  │    (Smithay compositor, state store)  │
│ Wayland windows via wprs      │  socket│  └─ encoder bridge (wprs-client role →   │
└───────────────────────────────┘        │       VA-API H.264 for phone clients)    │
                                         └──────────────────────────────────────────┘
```

### 4.1 `navetted` — orchestration daemon (Rust)

- **Supervisor**: spawns one `wprsd` instance per named session (own `WAYLAND_DISPLAY` socket, own state), launches the app into it, monitors, restarts policy-driven. Per-process isolation: one crashing app/compositor cannot take down siblings. systemd user service with `loginctl enable-linger`.
- **Registry**: SQLite (or flat RON) mapping `session name → {wprsd socket, app id, pid, created, last-attached, client-count}`. Backs `ls/run/attach/detach/kill`.
- **App index**: parse XDG desktop entries per the Desktop Entry + Menu specs (`/usr/share/applications`, `~/.local/share/applications`, flatpak exports at `/var/lib/flatpak/exports/share/applications`). Extract `Name`, `Icon`, `Exec` (field-code-stripped), `Categories`, `NoDisplay`/`Hidden`. Resolve icons via icon-theme lookup; ship PNG at 2–3 sizes over the API. This is exactly the data source KDE/GNOME menus render, so the drawer matches the host's real menu.
- **Navette API (stable)**: WebSocket + JSON control channel (list apps, list sessions, run, kill, attach-request) and binary media channels. Versioned; additive evolution only. This is the contract phones and future clients build against — wprs's unstable rkyv wire format never crosses it.
- **Encoder bridge (for non-Wayland clients)**: `navetted` implements the wprs *client* role in-process against each session's `wprsd` (link the wprs crates directly — same repo, same build, sidestepping wire instability), receives surface commits, composites per-toplevel, encodes damage via VA-API H.264 (AV1 later), streams over the media channel. Input events from the phone are translated back into wprs input. Rationale for pixel-stream on mobile rather than porting the full state protocol: Android has no Wayland compositor to recreate objects into, and hardware H.264 decode (MediaCodec) is cheap, battery-friendly, and resolution-independent.

### 4.2 `navette` — CLI + desktop client (Rust)

- Thin. `navette <host> run firefox`, `navette ls`, `navette attach work-browser`, `navette detach`, `navette kill`.
- On Linux desktops, attach = orchestrate stock `wprsc` against the session's forwarded socket → real native windows, zero pixel-streaming. Navette adds only the naming/registry/multi-host layer on top.
- Transport: prefer Tailscale (direct dial to `navetted`'s tailnet address); fall back to SSH socket forwarding (wprs's default pattern).
- Optional desktop drawer later (`navette menu` TUI first; GUI drawer is post-v1).

### 4.3 Navette Android (Kotlin/Compose)

- **Drawer screen**: two sections — *Running* (live sessions, thumbnail + name + host) and *Apps* (remote XDG menu, searchable, category chips). Tap running → attach. Tap app → `run` + auto-attach. This is the "KDE/GNOME menu on your phone" requirement.
- **Session screen**: SurfaceView + MediaCodec decode; per-toplevel windows rendered as tabs or freeform panes; pinch-zoom; input mapping (touch→pointer, long-press→right-click, hardware keyboard, clipboard sync via API channel).
- **Resize-follows-viewport**: client reports logical size; `navetted` resizes the session's virtual output; app relayouts. No letterboxing.
- Connection over Tailscale (JD's established Navette/navetted pattern); reconnect is trivial because sessions persist by construction.
- Shares protocol code generation with `navetted` (JSON schema → Kotlin serialization).

### 4.4 Cloud story

- `navette-host-init`: cloud-init/user-data + shell script that installs wprs + navetted, enables linger, joins tailnet (auth key), opens nothing publicly. Any Hetzner/EC2/GCP headless VM becomes an attachable desktop in ~2 minutes.
- Because sessions persist server-side and clients are stateless, the same cloud Firefox/IDE session is reachable from tower, ThinkPad, and phone interchangeably — the differentiating demo.

## 5. Key design decisions

| # | Decision | Choice | Rationale |
|---|----------|--------|-----------|
| D1 | Build vs adopt serialization core | **Adopt wprs** | The Smithay state-store compositor is the hard 80%, done, Apache-2.0. Contribute upstream (dmabuf, protocols); fork only if upstream stalls against our needs. |
| D2 | Session granularity | **One wprsd per named session** | Crash isolation, per-session resize/state, tmux-clean semantics. Cost: N compositor processes (each is lightweight — no rendering). |
| D3 | Phone rendering path | **Daemon-side H.264 encode** | No Wayland on Android; hardware decode universal; protocol-instability firewall stays server-side. Revisit AV1 + a state-replay client if profiling demands. |
| D4 | Client↔daemon protocol | **New stable Navette API (WS+JSON control, binary media)** | wprs wire format is explicitly unstable; a product needs a contract. Matches navetted's proven WebSocket pattern. |
| D5 | wprs integration mode | **Link wprs crates in-process for the bridge; exec stock wprsd per session** | In-process client role avoids wire-format coupling across builds (one workspace, one lockfile); exec'd wprsd keeps isolation. |
| D6 | App discovery | **XDG desktop entries + icon theme resolution** | The canonical, DE-agnostic source; identical data to KDE/GNOME menus; works on headless hosts with apps installed but no DE running. |
| D7 | Transport | **Tailscale-first, SSH fallback** | Matches existing homelab fabric; removes port-forward friction for the cloud story. |

## 6. Milestones

- **M0 — Adopt-vs-fork gate (risk retirement).** Install wprsd on Atlas or Tower; `wprs run` Cowork/Firefox from the ThinkPad; detach/reattach; assess shm-only rendering (no dmabuf) on the real app set; measure LAN + tailnet latency and CPU. *Exit criteria:* daily-drivable for ≥2 target apps → proceed on stock wprs; else scope the upstream contribution (dmabuf) before M1. Also validate two concurrent wprsd instances under one user.
- **M1 — navetted core.** Supervisor + registry + XDG app index + stable WS API (list/run/kill). `navette` CLI with `ls/run/attach/detach/kill`, attach via orchestrated wprsc over SSH-forwarded sockets. Tailscale dial-in.
- **M2 — Encoder bridge.** In-process wprs client role; per-toplevel VA-API H.264 streams; input translation; virtual-output resize API. Validate with a throwaway desktop pixel viewer.
- **M3 — Navette Android.** Compose drawer (Running + Apps), session screen with MediaCodec decode, touch/keyboard input, clipboard, resize-follows-viewport, reconnect UX.
- **M4 — Cloud + polish.** `navette-host-init` cloud recipe; multi-host registry in clients; icons/thumbnails; docs; demo video (phone → Hetzner Firefox → walk to tower → native attach of the same session).
- **M5 (stretch).** Audio (PipeWire capture per session → Opus channel), AV1, upstream dmabuf work, desktop GUI drawer.
- **M6 — Navette iOS.** SwiftUI port of the drawer + session screens. The architecture makes this cheap by design: the phone client is a stable-API consumer (WS/JSON control + H.264 media), so iOS is VideoToolbox decode + input mapping — no Wayland knowledge, no protocol coupling. Tailscale has a mature iOS client, so transport carries over. Main new work: touch/keyboard input model under iOS constraints, clipboard integration, and App Store distribution posture (an AGPL app streaming from your own server is fine for the Store; sideload/TestFlight as fallback).
- **M7 — Navette Desktop (Windows / macOS / Linux stream client).** One cross-platform client speaking the same stable API as the phone. Two client tiers, made explicit in the product:
  - **Native mode (Linux only):** wprs object replay via wprsc → genuine local Wayland windows. The premium experience; unchanged.
  - **Stream mode (Windows, macOS, and Linux-without-Wayland):** per-toplevel H.264/AV1 streams rendered one-OS-window-per-remote-toplevel, with resize-follows-window driving the remote virtual output — so even the stream tier keeps per-app windows, never a desktop-in-a-viewport. Decode via Media Foundation/D3D11 (Windows), VideoToolbox (macOS), VA-API (Linux).
  - Implementation options, decided at M7 kickoff: (a) **Compose Multiplatform** — promote the Android app to the everywhere-app, one Kotlin codebase for Android/Windows/macOS/Linux; or (b) a Rust client (winit/wgpu + platform decode) sharing crates with `navette` CLI. Leaning (a) for UI reuse, (b) if decode-latency measurements demand it.
  - Tailscale ships on all three platforms, so transport, auth, and the "no public listener" model carry over untouched.

## 7. Devil's advocate

- **"This is just a wrapper; wprs could add all of it."** Upstream is low-velocity (~150 commits lifetime) and scoped as plumbing, not product. The registry/drawer/mobile layer is precisely what plumbing projects never ship. Risk accepted; mitigation is contributing upstream where the line is blurry (multi-instance ergonomics) and keeping Navette's value in the API + clients.
- **"shm-only rendering makes GPU apps unusable."** True for games/CAD; target workload is browsers, IDEs, Electron (Cowork), terminals — all fine in software rendering. M0 measures this on the real app set before any build. Dmabuf upstream work is the long-term fix and would benefit the whole ecosystem.
- **"Pixel streaming on the phone betrays the native-feel thesis."** The thesis is *native on desktops* (delivered via wprs objects) and *present-and-persistent on phones*, where "native Wayland windows" is not even a coherent goal. Resize-follows-viewport + hardware decode makes the phone experience feel like an app, not a desktop-in-a-postage-stamp.
- **"In-process linking of wprs chains us to their internals."** Yes — deliberately, at one pinned revision inside one workspace, which is the *only* stable way to consume an unstable protocol. The blast radius of an upstream bump is a compile error in our tree, not a runtime break on someone's phone.
- **"wprsd restart still kills apps — persistence is partial."** Correct and inherited: state lives in wprsd. Mitigations: per-session isolation limits blast radius; supervisor makes wprsd long-lived; document clearly. True checkpoint/restore of GUI apps (CRIU-class) is out of scope.
- **"Security surface: navetted can see and inject into every session."** Same trust model as wprs (and tmux): sockets in `$XDG_RUNTIME_DIR`, user-only; no listener on public interfaces; tailnet or SSH is the auth boundary; the WS API binds to tailnet address only. Threat-model doc required before M3 ships to a phone.

## 8. Risks

| Risk | L | I | Mitigation |
|------|---|---|------------|
| wprs abandoned upstream | M | M | Apache-2.0 → vendored fork path is clean; core already works |
| Multi-instance wprsd friction (socket/env assumptions) | M | M | Validate at M0; patches likely small and upstreamable |
| VA-API encode variability across hosts | M | L | Software x264 fallback; per-host capability probe |
| Electron/Chromium quirks under wprs (webauthn known-broken) | M | M | M0 test matrix; document unsupported flows |
| Scope creep toward "remote desktop suite" | H | M | Non-goals list is binding; audio and extra clients gated behind M4 |

## 9. Deferred

Audio, session sharing (multi-attach is native to the model but UX undefined), thumbnails-as-live-previews, per-session resource limits (cgroups), CRIU exploration. (iOS promoted to M6; Windows/macOS/Linux desktop stream clients promoted to M7.)

## 10. Execution notes — model/subagent routing

Per the standing routing convention (Opus brain, downshift + delegate elsewhere): most of this PRP is architecture and stays on-brain; the milestones split cleanly by how much judgment vs. mechanical work they contain.

| Milestone | Nature | Routing |
|---|---|---|
| M0 (adopt-vs-fork gate) | Judgment call under uncertainty — reading wprs internals, assessing shm-only rendering, deciding fork vs. contribute | **Opus brain**, in-conversation. Not delegable; the whole plan pivots on this read. |
| M1 (navetted core: supervisor, registry, XDG index, WS API, CLI) | Well-specified, mechanical Rust — multi-file but low-ambiguity once the API shape is fixed | **Claude Code CLI subprocess**, `--allowedTools Read,Edit,Write,Bash`, `--max-turns 15-20` per component (registry, XDG parser, CLI separately). API schema itself drafted on-brain first, then handed off literally. |
| M2 (encoder bridge: VA-API H.264, input translation, resize API) | Security- and correctness-sensitive (in-process linking per D5, input injection surface) | **Claude Code CLI** for implementation, **+ Codex CLI adversarial pass** per Rule 1 (security-sensitive coding gets a second opinion) — particularly the input-translation and socket-permission code. |
| M3 (Navette Android) | Large, mostly-mechanical Compose/Kotlin surface once the API contract exists | **Claude Code CLI subprocess**, feature-by-feature (drawer, session screen, decode pipeline, clipboard) at `--max-turns 15-25` each. Protocol/schema codegen decisions stay on-brain. |
| M4 (cloud-init recipe, multi-host registry, icons) | Small, mechanical | **Downshift to `cheap-code`** for the cloud-init script and icon-resolution boilerplate; CLI subprocess for the registry extension. |
| M5 (audio, AV1, dmabuf upstream contribution) | dmabuf work is genuinely hard (upstream Smithay/wprs internals) | dmabuf: **Opus brain** for design, then **Claude Code** for implementation against wprs's own contribution guidelines. Audio/AV1: **Claude Code CLI**, standard delegation. |
| M6 (iOS) | New platform, mechanical once M3's API-consumer pattern exists | **Claude Code CLI subprocess**, Swift/SwiftUI equivalents of the Android build-out. |
| M7 (Windows/macOS/Linux desktop clients) | Cross-platform decode/windowing — the Compose-vs-Rust choice is a judgment call; implementation after is mechanical | Kickoff decision (Compose Multiplatform vs. Rust/wgpu): **Opus brain**. Build-out: **Claude Code CLI subprocess** per platform. |

General rules carried over unchanged: any milestone touching input injection, socket permissions, or the trust boundary (§7 threat-model item) gets the Codex adversarial pass in addition to Claude Code, per Rule 1. PRP revisions, naming, and roadmap prioritization (this document and `ROADMAP-navette.md`) stay on-brain always, per Rule 2's "anything the user is actively discussing" carve-out — they're not delegated regardless of how mechanical a given edit looks, since they're negotiated in conversation.

## 11. Open questions

1. ~~Final name~~ — resolved: **Navette** (repurposed from retired v1).
2. Does wprsd tolerate many instances per user today, or is a small upstream patch needed? (M0)
3. Icon strategy for flatpak/snap apps with themed icons on headless hosts — bundle a fallback icon theme?
4. Should `navette run` support arbitrary `Exec=` commands (not just desktop entries)? (Leaning yes: `navette run --cmd`.)
5. Repo layout: single workspace `navette/` with `navetted`, `navette-cli`, `bridge` crates + `android/` — or split app repo per Navette precedent?
