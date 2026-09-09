<!-- Generated: 2026-09-08 | Files scanned: 9 (navetted) + 5 (navette-bridge) | Token estimate: ~900 -->

# Backend — `navetted`

Single async binary. axum over tokio, plus a calloop event loop running the
wprs client role in-process.

> **Scanned from `feat/clipboard-sync`.** Clipboard-related structure below —
> the third `select!` arm, `publish_message` and the per-client message deque,
> the `Request::Data` partition in the bridge loop, and `clipboard.rs` itself —
> lands with PR #19 and is not yet on `master`. Everything else is current.

## Routes

```
GET  /healthz                        → health
ANY  /v1/ws                          → websocket        (control, JSON)
ANY  /v1/sessions/{session}/media    → media_websocket  (binary + JSON text)
```

`crates/navetted/src/api.rs:80-83`

### Control socket — `api.rs:270`

```
while let Some(msg) = receiver.next().await → dispatch → Response
```

Strict request/response. **No server-initiated source and no channel to the
bridge.** This is why clipboard could not live here.

```
RequestCommand::ListApps      → app_index
ListSessions | Run | Kill     → registry + supervisor
Attach | Detach               → registry
SetClipboard | GetClipboard   → in-memory stub  [deleted by PR #19]
```

### Media socket — `api.rs:140`

A three-arm `tokio::select!`, duplex:

```
attachment.recv()          → Message::Binary  (media packets)
attachment.recv_message()  → Message::Text    (MediaServerMessage JSON)
receiver.next()            → MediaInput       → Ping answered inline,
                                                everything else submit_input
```

`Ping` is intercepted as the first match arm, ahead of `submit_input`, so RTT
measures the link and not the bridge loop's queue depth.

## Module → responsibility

| File | Lines | Responsibility |
|---|---|---|
| `api.rs` | ~800 | axum router, both socket loops, request dispatch |
| `bridge.rs` | ~1400 | calloop loop, batch drain, composite flush, publish |
| `media.rs` | ~750 | `MediaHub`, `ClientQueue`, per-client fan-out |
| `clipboard.rs` | ~410 | pure clipboard state machine (no I/O, no wprs types) — **arrives with PR #19** |
| `registry.rs` | ~250 | session records, persisted to disk |
| `supervisor.rs` | — | spawn/reap wprsd per session |
| `app_index.rs` | ~180 | XDG `.desktop` scan |

## MediaHub

```
register_session(name) -> mpsc::Receiver<MediaCommand>
attach(session)        -> MediaAttachment
publish(session, packet)          → fans Arc<MediaPacket> to every ClientQueue
publish_message(session, msg)     → fans MediaServerMessage, infallible
```

`ClientQueue` holds **two** independent paths:

| | deque | notify | bound | overflow |
|---|---|---|---|---|
| packets | `VecDeque<Arc<MediaPacket>>` | `notify` | capacity | evict, keep configs, re-request keyframe |
| messages | `VecDeque<MediaServerMessage>` | `message_notify` | 64 | drop oldest |

Separate `Notify` objects are load-bearing: a shared one lets a parked packet
reader consume a message's permit and re-park, stranding the message reader.
Pinned by `a_message_wake_reaches_the_message_reader_despite_a_pending_packet_read`.

## Bridge loop — `bridge.rs`

```
calloop channel → pending.drain(..)
  → Request::Data partitioned out → ClipboardSync  (transport in scope here)
  → remainder → apply_scene_messages → scene.apply() → SceneEvents
  → flush_composites()  (one composite per surface per batch)
  → encode → media.publish(session, packet)
```

Input is drained **mid-batch**, not at batch boundaries — draining only at
boundaries restores unbounded input latency.

## Clipboard state machine — `clipboard.rs` (arrives with PR #19)

Deliberately free of wprs types, transport, clock and async, so every decision
is a plain unit test and `bridge.rs` is only translation.

```
on_guest(GuestEvent)          -> SyncAction
on_phone_clipboard(String)    -> SyncAction

GuestEvent  = SelectionOffered{mime_types} | TransferFromGuest{bytes} | PasteRequested
SyncAction  = Nothing | AskGuestFor{mime} | PushToPhone{text}
            | OfferToGuest{mime_types} | AnswerGuest{bytes}
```

State: `phone_text`, `awaiting_guest_transfer`, and two **one-shot** echo
tokens (`echo_from_guest`, `echo_from_phone`) consumed on first match. A
retained token would silently suppress the same text legitimately copied later.

`PasteRequested` has no early return — `AnswerGuest` is structurally
guaranteed, because an unanswered pull hangs the pasting guest app.

## Encoder — `navette-bridge/src/encoder.rs`

```
EncoderBackend = Vaapi { device } | Libx264 | Fake
```

`Fake` exists for tests. Both real backends run `-bf 0`.

## Verification gate

```
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

`--all-targets` matters: `cargo build --workspace` does not compile test
targets and has let exhaustive-match breaks through.
