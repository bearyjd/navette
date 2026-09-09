<!-- Generated: 2026-09-08 | Files scanned: 2 (navette-protocol) + registry/app_index | Token estimate: ~800 -->

# Data

**There is no database.** No SQL, no ORM, no migrations. State is either on the
wire, in memory for the life of a session, or a small file on disk. This file
documents those three instead.

## 1. Wire format — media frames

Binary, big-endian, 44-byte fixed header + payload.
`crates/navette-protocol/src/media.rs`

```
offset size field
   0    4   magic          "NVTM"
   4    2   version        u16   (MEDIA_VERSION)
   6    1   kind           u8
   7    1   flags          u8    (keyframe, ...)
   8    8   stream_id      u64
  16    8   sequence       u64   ← per-stream, NOT global
  24    8   timestamp_us   u64
  32    4   payload_len    u32
  36    4   width          u32
  40    4   height         u32
  ------------------------------ 44
```

```
MediaKind = StreamConfig(1) | Video(2) | StreamEnd(3) | Metrics(4)
```

**Sequence is per-stream.** Diffing sequences across two streams is
meaningless and inflates drop counts. Adding a new `MediaKind` is a breaking
change — Android throws on unknown kinds (`MediaProtocol.kt:192`).

## 2. Wire format — JSON messages

Both directions on the media socket, serde tag `type`, snake_case.

```
MediaInput (client → daemon)
  pointer_motion | pointer_button | pointer_axis
  keyboard_key   | keyboard_modifiers
  viewport_resize | request_keyframe | ping
  set_clipboard                                  ← pending PR #19

MediaServerMessage (daemon → client)
  error | pong
  clipboard                                      ← pending PR #19
```

Validated at `MediaInput::validate()` — the single funnel is
`MediaAttachment::submit_input` (`media.rs:293`), so nothing unvalidated
reaches the bridge.

| Field | Bound |
|---|---|
| pointer x/y | must be finite |
| button | `0x110..=0x11f` |
| keycode | `<= 767` |
| layout_index | `< MAX_LAYOUTS` |
| viewport | `320..=3840` × `240..=2160` |
| clipboard text | `<= MAX_CLIPBOARD_BYTES` (1 MiB, UTF-8 bytes) |

`validate()` ends in `_ => Ok(())`, so a variant **without** an explicit arm is
silently accepted. New bounded variants need their own arm.

## 3. Wire format — control socket

```
Request  { request_id, command }   RequestCommand: list_apps | list_sessions
                                   | run | kill | attach | detach
Response { request_id, outcome }   ResponseOutcome: ok{result} | error{code,message}
```

## 4. Wire format — wprs (upstream, rkyv)

navette speaks wprs's protocol in the **client** role: receives `Request::*`,
sends `Event::*`. Clipboard rides `Request::Data` / `Event::Data`.

```
guest → phone:  SourceRequest(SetSelection(Selection, meta))   [in]
                SourceEvent(MimeTypeSendRequestedByDestination) [out]
                TransferData(Selection, bytes)                  [in]

phone → guest:  DestinationEvent(SelectionSet(Selection, meta)) [out]
                DestinationRequest(RequestDataTransfer(..))     [in]
                TransferData(Selection, bytes)                  [out]
```

`SourceMetadata { mime_types: Vec<String>, dnd_actions: u32 }` — a list, not a
promise. "No text form offered" is a real state.

Text MIME preference, first match wins; the guest's **own spelling** is
returned:

```
text/plain;charset=utf-8 → UTF8_STRING → text/plain → STRING → TEXT
```

The last three are X11 atoms, arriving from XWayland guests.

## 5. On-disk state

| Path | Written by | Contents |
|---|---|---|
| registry file | `registry.rs:211-235` | session records, newline-delimited |
| XDG `.desktop` dirs | read-only | scanned by `app_index.rs` |
| `~/.config/navette/` | CLI | attach records (`AttachRecord`) |

No migrations. The registry is rewritten wholesale.

## 6. In-memory session state

```
HubState
 └─ sessions: HashMap<String, SessionMedia>
     └─ clients: HashMap<u64, Arc<ClientQueue>>
         ├─ packets:  VecDeque<Arc<MediaPacket>>   (bounded, evicts)
         ├─ messages: VecDeque<MediaServerMessage> (64, drops oldest)
         └─ needs_keyframe: HashSet<u64>           (per stream)
```

Clipboard state lives on the bridge loop (single-threaded calloop, no lock):
`phone_text`, `awaiting_guest_transfer`, and two one-shot echo tokens.

**Nothing here survives a daemon restart** except the registry file.
