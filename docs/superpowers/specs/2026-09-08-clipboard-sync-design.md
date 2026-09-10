# Clipboard Sync — Design

**Status:** approved for planning
**Date:** 2026-09-08
**Phase:** 2 (lead item)
**Branch:** `feat/clipboard-sync`, stacked on `feat/android-perf-hud` (PR #18)

## Goal

Text copied in a guest window appears on the phone's clipboard, and text
copied on the phone can be pasted into a guest window. Automatic in both
directions.

This replaces a stub. `crates/navetted/src/api.rs:378-392` holds an
`Arc<Mutex<Option<String>>>` that `SetClipboard` writes and `GetClipboard`
reads back. It never reaches wprs, never reaches the guest, and has no
Android caller — `ControlProtocol.kt:61-66` and `:92-93` declare the types
and nothing calls them. Its only tests assert the self-echo.

## Why the media socket, not the control socket

The control socket cannot do this, and the reason is structural rather
than incidental.

`api.rs:270` is `while let Some(message) = receiver.next().await` — a
strict request/response pump. The server speaks only in reply. There is no
server-initiated source and no channel from the control socket to the
bridge at all.

`api.rs:140` is a `tokio::select!` over `attachment.recv()` and
`receiver.next()`. It is already duplex, already carries a
server-initiated source, and `MediaCommand` (`media.rs:39`) is already the
socket-task→bridge path.

`MediaInput` already carries keyboard, pointer, viewport and ping;
`MediaKind::Metrics` already carries non-video data. This is the session
socket, not a video socket. The name is legacy; the role is right.

**Consequence:** the control-protocol clipboard surface is removed, not
extended. Keeping it beside a working implementation would ship two
clipboards, one of them fake.

## Wire shapes

Two additions. Both ride existing machinery.

```rust
// crates/navette-protocol/src/media.rs, in MediaInput (phone -> daemon)
SetClipboard {
    text: String,
},

// crates/navette-protocol/src/media.rs, in MediaServerMessage (daemon -> phone)
Clipboard {
    text: String,
},
```

Kotlin mirrors in `android/.../net/MediaProtocol.kt`, `@SerialName("set_clipboard")`
and `@SerialName("clipboard")`.

### No `MEDIA_VERSION` bump

Justified separately per direction, because the two directions have
opposite compatibility properties:

- **`MediaInput::SetClipboard`** is client→server. An old daemon answers an
  unknown tag with `invalid_input` and does not disconnect. Same argument
  that carried `MediaInput::Ping`, and it was tested on device against a
  `master` daemon on the HUD branch.
- **`MediaServerMessage::Clipboard`** is server→client, sent as
  `Message::Text`. `MediaClient.onMessage` logs unrecognized server text
  without tearing the connection down — the property the HUD branch
  verified empirically.

**A new `MediaKind` was rejected for exactly this reason.**
`MediaProtocol.kt:192` does `MediaKind.fromWire(kindByte) ?: throw
MediaDecodeException(MediaDecodeError.UnknownKind(kindByte))`. A binary
`MediaKind::Clipboard` would make every old client throw on every clipboard
frame. Server→client binary is a breaking change; server→client text is
not.

### New plumbing

`attachment.recv()` yields binary packets only, so a bridge-originated push
has no path to the socket task. Add a per-client message queue carrying
`MediaServerMessage`, fanned out by the hub the same way packets are, and a
third arm in the `select!` at `api.rs:140` that sends it as
`Message::Text`.

This is the only genuinely new structure in the feature.

## Where the handling lives

In `crates/navetted/src/bridge.rs`, **not** `crates/navette-bridge/src/scene.rs`.

`scene.apply()` only *returns* `Vec<SceneEvent>`; it has no transport and
cannot send. Every `transport.send(Event::...)` in the tree is in
`navette-bridge`'s `transport.rs` and `input.rs`, never `scene.rs`. So
clipboard, which must send, cannot be handled there.

`bridge.rs:393` passes each raw `RecvType<Request>` to `scene.apply()`, and
at the batch-drain site (`bridge.rs:247-250`) `transport` is already in
scope. **`Request::Data` is partitioned out of the batch there**, before it
reaches `apply_scene_messages` — the same interception pattern `api.rs`
uses for `Ping` ahead of `submit_input`.

This keeps clipboard state out of the `navette-bridge` crate entirely.
`scene.rs:365`'s `Request::Data(_) => Ok(Vec::new())` stays as a backstop;
after interception nothing reaches it.

## State ownership

Plain fields on the bridge loop in `crates/navetted/src/bridge.rs`. No
lock: calloop is single-threaded, and both the guest's pull and our answer
arrive there. Putting this state behind an `Arc<Mutex<>>` elsewhere would
add a lock crossing to every paste for nothing.

```rust
/// The phone's latest clipboard text, retained to answer a guest paste
/// that may arrive seconds later, or never.
phone_text: Option<String>,

/// Bumped on each guest SetSelection, so a late TransferData is
/// attributed to the current offer.
guest_offer_generation: u64,

/// One-shot echo tokens. Set when we send in a direction; consumed by
/// the first matching inbound value. See the echo loop below -- these
/// must be cleared on match, not retained.
echo_token_to_guest: Option<String>,
echo_token_to_phone: Option<String>,
```

`guest_offer_generation` deliberately does not promise more than the
protocol provides. wprs has no correlation id on `TransferData`, and wprsd
itself keeps a single `selection_pipe` slot per `DataSource` — it
overwrites rather than correlates. The generation counter matches that
model instead of inventing a stronger guarantee.

## Flow: guest → phone

The bridge plays wprs's *local* role, so it receives `Request::*` and sends
`Event::*`.

1. `Request::Data(DataRequest::SourceRequest(DataSourceRequest::SetSelection(DataSource::Selection, meta)))`
   arrives. Today this is dropped at `crates/navette-bridge/src/scene.rs:365`
   (`Request::Data(_) => Ok(Vec::new())`).
2. Bump `guest_offer_generation`. Pick a MIME from `meta.mime_types` by the
   preference order below. **If none matches, stop here** — the guest
   offered no text form, which is a real state, not an empty string.
3. Send `Event::Data(DataEvent::SourceEvent(DataSourceEvent::MimeTypeSendRequestedByDestination(DataSource::Selection, chosen)))`.
4. `Request::Data(DataRequest::TransferData(DataSource::Selection, DataToTransfer(bytes)))`
   arrives.
5. Reject if over the size cap. Decode as UTF-8; drop on failure.
6. Apply echo suppression (below). If it survives, push
   `MediaServerMessage::Clipboard { text }` to every attached client.

**MIME preference order**, first match wins:

```
text/plain;charset=utf-8
UTF8_STRING
text/plain
STRING
TEXT
```

`UTF8_STRING`, `STRING` and `TEXT` are X11 atoms and arrive from XWayland
guests, which is the common case for browsers.

## Flow: phone → guest

1. Android sends `MediaInput::SetClipboard { text }`.
2. Bridge stores `phone_text = Some(text)` and sends
   `Event::Data(DataEvent::DestinationEvent(DataDestinationEvent::SelectionSet(DataSource::Selection, SourceMetadata::from_mime_types(vec![...]))))`,
   offering the five MIME types above. wprsd handles this at
   `server/client_handlers.rs:880`, calling `set_data_device_selection`, and
   guest apps see the offer.
3. A guest app pastes.
   `Request::Data(DataRequest::DestinationRequest(DataDestinationRequest::RequestDataTransfer(DataSource::Selection, mime)))`
   arrives — at an arbitrary later time.
4. Bridge answers
   `Event::Data(DataEvent::TransferData(DataSource::Selection, DataToTransfer(bytes)))`.

## On attach

The media hub replays every stream's config and keyframe when a client
attaches (`media.rs:150-153`). **Clipboard deliberately has no equivalent.**

Guest→phone is not replayed. If the guest copied while no phone was
attached, the phone's clipboard is untouched until the guest copies again.
Two reasons: overwriting the phone's own clipboard on every attach would
clobber whatever the user had there, and not replaying means guest
clipboard content is never retained past the moment it is forwarded.

Phone→guest needs no replay. `phone_text` lives on the bridge loop, which
outlives any single attachment, and wprsd already holds the offer we sent
via `set_data_device_selection` — so a guest paste still works across a
detach and reattach.

## Hazards

These are the three failure modes the implementation must design against,
not discover.

### The echo loop

Phone sets clipboard → we offer it to the guest → the guest's compositor
sets its selection → if wprsd sends `SetSelection` back to us, we push it
to the phone → Android's listener fires on our own write → phone sets
clipboard. Forever.

**Mitigation:** a **one-shot** token per direction. When we send a value,
store it. The first inbound value that matches is dropped *and the token is
cleared*. Everything after forwards normally.

The one-shot part is the whole mitigation. A token that is merely *compared*
and retained is a silent-drop bug: copy "foo" on the phone, and "foo"
legitimately copied on the guest is suppressed forever after. That bug
survives naive tests because tests use distinct strings, so the
discriminating case is named explicitly in the testing section.

This works whether or not wprsd actually echoes, which is why it is
specified unconditionally rather than made to depend on open question 1.

This is the most likely defect in the feature.

### The unanswered pull

wprsd `take()`s the pipe fd when it forwards `RequestDataTransfer`. If we
never send `TransferData`, that pipe is never written **and never closed**,
and the pasting guest app blocks on a read forever.

**Every pull must be answered.** `phone_text == None` answers with empty
bytes. There is no path through step 4 of the phone→guest flow that returns
without sending.

### Size

Clipboards reach megabytes and arrive from outside the trust boundary in
both directions.

**Cap: 1 MiB.** Phone→daemon is rejected in `MediaInput::validate()` with
`invalid_input`, not truncated — a silently truncated paste is worse than a
refused one. Guest→phone over the cap is dropped, logging the byte count
only.

## Privacy

wprs redacts `DataToTransfer` in its `Debug` impl behind
`args::get_log_priv_data()`. navette matches that posture: **clipboard
content never reaches a log line at any level.** Log lengths, MIME types,
error kinds and byte counts; never bytes or text. This applies to the
Android side equally.

## Android

`ClipboardManager.OnPrimaryClipChangedListener` while the app is focused,
plus a read on `Lifecycle.Event.ON_RESUME`.

The resume read is not a fallback — it is the path most real transfers take.
Since Android 10 `getPrimaryClip()` returns null when the app is not
focused, and copying text from another app necessarily unfocuses Navette,
so the listener alone would miss exactly the case the feature exists for.

**Accepted cost:** Android 12+ shows a system toast when an app reads
clipboard content it did not write. Automatic phone→guest sync means this
toast appears when returning to Navette with new clipboard content. This is
inherent to the chosen sync model, not an implementation defect.

Guest→phone sets the phone clipboard via `ClipboardManager.setPrimaryClip`,
which does not toast.

## Scope

**In:** `text/plain` in its five spellings, `DataSource::Selection`,
automatic both directions.

**Out, deliberately:**

- **`DataSource::Primary`** — the X11/Wayland middle-click primary
  selection. Android has no primary selection to mirror.
- **Drag and drop.** It rides these same `DataRequest`/`DataEvent` enums and
  will look adjacent to anyone reading the code. It needs pointer-position
  correlation and a phone-side drop target, which is a different feature.
- **Rich types** — `text/html`, `image/png`. The MIME negotiation built here
  extends to them; nothing in this design blocks it.

## Removals

From `crates/navette-protocol/src/lib.rs`: `RequestCommand::SetClipboard`
(`:44`), `RequestCommand::GetClipboard` (`:47`), `ResponseResult::Clipboard`
(`:106`).

From `crates/navetted/src/api.rs`: the `clipboard` field on the state struct
(`:35`), both dispatch arms (`:378-392`), and the self-echo test.

From `android/.../protocol/ControlProtocol.kt`: `SetClipboard` (`:62`),
`GetClipboard` (`:66`), `Clipboard` (`:93`).

Two consumers come out with it, neither of which is a caller:

- `crates/navette-cli/src/lib.rs:100` — a `render_result` arm that exists
  only because the variant does. `navette-cli` has no clipboard
  subcommand; `Command` (`main.rs:43`) is `Ls`/`Run`/`Attach`/`Detach`/`Kill`.
- `android/.../protocol/ControlProtocolTest.kt:33-35` — serialization
  round-trip assertions for the removed types.

Nothing anywhere *sends* `SetClipboard` or `GetClipboard`. Verified by
grep across `crates/` and `android/`.

## Testing

**Rust unit** — MIME selection including the no-text-offered case; UTF-8
decode failure; **a pull answered when `phone_text` is `None`**, which is
the hang regression.

Two tests carry more weight than the rest, because each pins a bug that
would otherwise pass a naive suite:

- **The echo token is one-shot.** Sync a value in one direction, then have
  the *same text* legitimately copied on the other side, and assert it still
  propagates. A retained-token implementation passes every distinct-string
  test and fails only this one.
- **The size cap has its own `validate()` arm.** `MediaInput::validate()`
  ends in `_ => Ok(())`, so a missing `SetClipboard` arm silently accepts
  any length. The test must assert that an over-cap `SetClipboard` is
  *rejected*, not that a normal one is accepted.

**Rust integration** — a scripted wprs `Request` sequence through the bridge
loop asserting the emitted `Event` sequence, for both flows. Covers the
generation counter under a re-offer arriving before a transfer completes.

**Android unit** — protocol round-trip for both new messages; unknown
server text still ignored; the resume-read path; suppression of a
clipboard change we ourselves wrote.

**On device, and none of the above substitutes for it** — copy in Firefox
and paste on the phone; copy on the phone and paste into Firefox; confirm no
echo loop by watching for repeated traffic after a single copy; paste in a
guest app while the phone clipboard is empty and confirm the app does not
hang.

## Open questions, to resolve during implementation

Neither blocks planning; both are answered by observation, not argument.

1. **Does wprsd echo `SetSelection` back to us after
   `set_data_device_selection`?** Drives how much the loop suppression
   actually has to do. The mitigation is specified unconditionally, so the
   answer changes test emphasis, not design.
2. **What MIME spellings do real guest apps offer?** The preference order
   above is derived from the Wayland and X11 conventions; on-device
   observation with Firefox and a GTK app confirms or reorders it.
