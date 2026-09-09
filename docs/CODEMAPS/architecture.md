<!-- Generated: 2026-09-08 | Files scanned: 33 Rust + 23 Kotlin | Token estimate: ~750 -->

# Architecture

Remote desktop for **individual Wayland windows**, not whole screens. A Linux
daemon captures one app's surfaces, encodes H.264, and streams to a phone or a
desktop viewer over a tailnet.

> **Scanned from `feat/clipboard-sync`.** Clipboard structure — the `clipboard`
> module and the input/clipboard data flow below — lands with PR #19 and is not
> yet on `master`. Everything else is current.

## System

```
  GUEST APP (unmodified Wayland client)
        │  wayland protocol
        ▼
  wprsd  ── wprs wire protocol (rkyv) ──┐
  (upstream, separate process)          │
                                        ▼
  ┌─────────────── navetted (daemon) ───────────────┐
  │  supervisor ─ spawns/reaps wprsd per session    │
  │  registry   ─ session state, persisted to disk  │
  │  app_index  ─ XDG .desktop scan                 │
  │  bridge     ─ owns the wprs client role,        │
  │               drives navette-bridge in-process  │
  │  media      ─ per-client fan-out hub            │
  │  clipboard  ─ pure clipboard state machine      │
  │  api        ─ axum HTTP + two WebSockets        │
  └──────────────────┬──────────────────────────────┘
                     │  navette media protocol
        ┌────────────┴────────────┐
        ▼                         ▼
   ANDROID APP              navette-viewer
   (Compose)                (minifb, Linux)
```

`navette-bridge` is a **library**, not a process. `navetted` links it and owns
the calloop event loop, so there is no IPC hop between them.

## Crate boundaries

| Crate | Role | Depends on wprs |
|---|---|---|
| `navette-protocol` | wire types shared by daemon and clients | no |
| `navette-bridge` | wprs scene graph, input translation, H.264 encode | yes |
| `navetted` | daemon: sessions, media hub, HTTP/WS, clipboard | yes |
| `navette-viewer` | Linux client | no |
| `navette-cli` | `navette ls/run/attach/detach/kill` | no |

`navette-protocol` deliberately has no wprs dependency — clients never link it.

## Data flow: a frame

```
guest commits surface
  → wprsd sends Request::Surface
  → navetted/bridge.rs batch-drains the calloop channel
  → scene.apply() updates the scene graph, returns SceneEvents
  → flush_composites(): ONE composite per surface per batch
  → encoder (VAAPI | libx264 | fake) produces an Annex-B frame
  → MediaHub::publish fans Arc<MediaPacket> to each ClientQueue
  → socket task sends a 44-byte header + payload as a binary frame
```

Compositing is batched, not per-commit: one composite per surface for a whole
batch. Framebuffer decode is deferred to compose time, not commit time.

## Data flow: input and clipboard

```
client → MediaInput (JSON text) → submit_input (validates)
       → MediaCommand::Input → bridge loop
       → clipboard intercepted BEFORE scene.apply / InputState::apply
       → everything else → InputState::apply → transport.send(Event::*)
```

Clipboard is intercepted at the batch-drain site because `scene.apply()`
returns events and has no transport — it cannot send. Same interception
pattern `api.rs` uses for `Ping` ahead of `submit_input`.

## Key invariants

- **Per-stream sequence numbering.** One toplevel = one stream. Sequence gaps
  are only meaningful within a stream; comparing across streams inflates drop
  counts by tens of thousands.
- **The hub replays config + keyframe on attach**, so a late client can decode.
- **Every guest clipboard pull must be answered**, empty if need be — wprsd has
  already taken the pipe fd, and an unanswered pull hangs the pasting app.
- **No B-frames** (`-bf 0` on both real encoders), so decode order equals
  presentation order.

## See also

`backend.md` · `frontend.md` · `data.md` · `dependencies.md`
