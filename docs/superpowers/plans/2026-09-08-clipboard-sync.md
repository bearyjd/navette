# Clipboard Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Text copied in a guest window reaches the phone's clipboard, and text copied on the phone can be pasted into a guest window — automatically, in both directions.

**Architecture:** Clipboard rides the media socket, which is already duplex and already owns the socket-task→bridge path. A pure, transport-free state machine (`crates/navetted/src/clipboard.rs`) holds the decisions; `crates/navetted/src/bridge.rs` intercepts `Request::Data` out of the wprs batch before `scene.apply()` and drives that state machine. Server→client pushes travel as JSON text on a new per-client message queue that runs alongside the existing binary packet queue, leaving the video path untouched.

**Tech Stack:** Rust (tokio, calloop, serde, wprs), Kotlin/Compose (kotlinx.serialization, Android `ClipboardManager`).

**Spec:** `docs/superpowers/specs/2026-09-08-clipboard-sync-design.md`

## Global Constraints

- **Clipboard content never reaches a log line, at any level, on either side.** Log lengths, MIME types, error kinds, byte counts. Never bytes or text. wprs redacts `DataToTransfer` behind `args::get_log_priv_data()`; match that posture.
- **Size cap: `MAX_CLIPBOARD_BYTES = 1024 * 1024`** (1 MiB). Phone→daemon is *rejected*, never truncated. Guest→phone over cap is dropped.
- **No `MEDIA_VERSION` bump.** Do not change `MEDIA_VERSION`. Adding a new binary `MediaKind` is forbidden — `MediaProtocol.kt:192` throws on unknown kinds, which would break every old client.
- **Scope:** `text/plain` in five spellings only. `DataSource::Selection` only — never `Primary`, never `DnD`.
- **Every guest pull must be answered.** wprsd `take()`s the pipe fd on `RequestDataTransfer`; an unanswered pull leaves it never written and never closed, hanging the pasting guest app forever.
- Rust: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` must stay clean.
- Android: run `./gradlew testDebugUnitTest --rerun-tasks` and read the pass/fail counts out of the JUnit XML under `android/app/build/test-results/testDebugUnitTest/`. A cached "BUILD SUCCESSFUL" can run zero tests.

## MIME preference order

Used verbatim in Tasks 5 and 6. First match wins:

```
text/plain;charset=utf-8
UTF8_STRING
text/plain
STRING
TEXT
```

`UTF8_STRING`, `STRING` and `TEXT` are X11 atoms arriving from XWayland guests — the common case for browsers.

## File Structure

| File | Responsibility |
|---|---|
| `crates/navette-protocol/src/media.rs` | `MediaInput::SetClipboard`, `MediaServerMessage::Clipboard`, `MAX_CLIPBOARD_BYTES`, the `validate()` arm |
| `crates/navetted/src/clipboard.rs` | **New.** Pure state machine: MIME selection, echo tokens, the phone's retained text. No wprs types, no I/O, no transport. |
| `crates/navetted/src/media.rs` | Per-client server-message queue and `MediaHub::publish_message` |
| `crates/navetted/src/api.rs` | Third `select!` arm sending server messages as text; removal of the control-socket clipboard arms |
| `crates/navetted/src/bridge.rs` | Intercepts `Request::Data`, drives `ClipboardSync`, sends wprs `Event`s |
| `crates/navette-protocol/src/lib.rs` | Removal of the control-protocol clipboard surface |
| `crates/navette-cli/src/lib.rs` | Removal of the dead render arm |
| `android/.../net/MediaProtocol.kt` | Kotlin mirrors of both new messages |
| `android/.../ui/session/ClipboardBridge.kt` | **New.** Android clipboard read/write, listener, resume read, local echo suppression |
| `android/.../protocol/ControlProtocol.kt` | Removal of the control-protocol clipboard surface |

---

### Task 1: Protocol — clipboard messages and the size cap

**Files:**
- Modify: `crates/navette-protocol/src/media.rs` (`MediaInput` at `:242`, `MediaInput::validate` at `:289`, `InputValidationError` at `:319`, `MediaServerMessage` at `:339`)
- Test: `crates/navette-protocol/src/media.rs` (the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `MediaInput::SetClipboard { text: String }`, serde tag `"set_clipboard"`. `MediaServerMessage::Clipboard { text: String }`, serde tag `"clipboard"`. `pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;`. `InputValidationError::ClipboardTooLarge(usize)`.

**Why the explicit validate arm matters:** `MediaInput::validate()` ends in `_ => Ok(())`. A missing arm therefore accepts any length *silently*. The test below asserts rejection, not acceptance — an implementation that forgets the arm must fail it.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/navette-protocol/src/media.rs`:

```rust
#[test]
fn oversized_clipboard_is_rejected() {
    let input = MediaInput::SetClipboard {
        text: "a".repeat(MAX_CLIPBOARD_BYTES + 1),
    };
    assert_eq!(
        input.validate(),
        Err(InputValidationError::ClipboardTooLarge(
            MAX_CLIPBOARD_BYTES + 1
        ))
    );
}

#[test]
fn clipboard_at_the_cap_is_accepted() {
    let input = MediaInput::SetClipboard {
        text: "a".repeat(MAX_CLIPBOARD_BYTES),
    };
    assert_eq!(input.validate(), Ok(()));
}

#[test]
fn clipboard_cap_counts_utf8_bytes_not_chars() {
    // A 3-byte character: MAX/3 + 1 of them exceeds the cap on bytes
    // while being far under it on character count.
    let text = "\u{2603}".repeat(MAX_CLIPBOARD_BYTES / 3 + 1);
    assert!(text.chars().count() < MAX_CLIPBOARD_BYTES);
    assert!(matches!(
        MediaInput::SetClipboard { text }.validate(),
        Err(InputValidationError::ClipboardTooLarge(_))
    ));
}

#[test]
fn set_clipboard_deserializes_from_wire() {
    let input: MediaInput =
        serde_json::from_str(r#"{"type":"set_clipboard","text":"hello"}"#).unwrap();
    assert_eq!(
        input,
        MediaInput::SetClipboard {
            text: "hello".into()
        }
    );
}

#[test]
fn clipboard_server_message_serializes_to_wire() {
    let encoded = serde_json::to_string(&MediaServerMessage::Clipboard {
        text: "hello".into(),
    })
    .unwrap();
    assert_eq!(encoded, r#"{"type":"clipboard","text":"hello"}"#);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p navette-protocol clipboard`
Expected: FAIL — `no variant named SetClipboard`, `cannot find value MAX_CLIPBOARD_BYTES`.

- [ ] **Step 3: Add the constant and the error variant**

Near the other limit constants at the top of `crates/navette-protocol/src/media.rs`:

```rust
/// Largest clipboard payload accepted in either direction, in UTF-8 bytes.
/// Clipboards reach megabytes and arrive from outside the trust boundary;
/// an over-cap paste is refused rather than silently truncated, because a
/// half-pasted document is worse than a refused one.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
```

Add to `InputValidationError`:

```rust
    ClipboardTooLarge(usize),
```

- [ ] **Step 4: Add the enum variants**

Add to `MediaInput`, after `RequestKeyframe`:

```rust
    SetClipboard {
        text: String,
    },
```

Add to `MediaServerMessage`:

```rust
    Clipboard { text: String },
```

- [ ] **Step 5: Add the explicit validate arm**

In `MediaInput::validate()`, **before** the trailing `_ => Ok(())`:

```rust
            Self::SetClipboard { text } if text.len() > MAX_CLIPBOARD_BYTES => {
                Err(InputValidationError::ClipboardTooLarge(text.len()))
            }
```

`text.len()` on a `String` is the UTF-8 byte length, which is what the cap measures.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p navette-protocol clipboard`
Expected: PASS, 5 tests.

- [ ] **Step 7: Run the full crate suite and lints**

Run: `cargo test -p navette-protocol && cargo clippy -p navette-protocol --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/navette-protocol/src/media.rs
git commit -m "feat(protocol): clipboard messages on the media socket

MediaInput::SetClipboard and MediaServerMessage::Clipboard, plus a 1 MiB
cap with its own validate() arm -- validate() ends in _ => Ok(()), so a
missing arm accepts any length silently. The test asserts rejection.

No MEDIA_VERSION bump: an old daemon answers an unknown MediaInput tag
with invalid_input, and an old client logs unrecognized server text
without dropping the connection."
```

---

### Task 2: Kotlin protocol mirrors

**Files:**
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/net/MediaProtocol.kt`
- Test: `android/app/src/test/kotlin/com/greponlabs/navette/net/MediaProtocolTest.kt`

**Interfaces:**
- Consumes: the wire tags from Task 1 — `"set_clipboard"` with field `text`, `"clipboard"` with field `text`.
- Produces: `MediaInput.SetClipboard(val text: String)`, `MediaServerMessage.Clipboard(val text: String)`.

**Note on types:** use `String`, not `ULong` — this carries no u64. The `ULong` convention in this file exists only for 64-bit integer fields, where signedness has bitten this project before.

- [ ] **Step 1: Write the failing tests**

Add to `MediaProtocolTest.kt`:

```kotlin
@Test
fun setClipboardEncodesToWire() {
    val encoded = Json.encodeToString(
        MediaInput.serializer(),
        MediaInput.SetClipboard("hello"),
    )
    assertEquals("""{"type":"set_clipboard","text":"hello"}""", encoded)
}

@Test
fun clipboardServerMessageDecodesFromWire() {
    val decoded = Json.decodeFromString(
        MediaServerMessage.serializer(),
        """{"type":"clipboard","text":"hello"}""",
    )
    assertEquals(MediaServerMessage.Clipboard("hello"), decoded)
}

@Test
fun setClipboardPassesValidation() {
    assertNull(MediaInput.SetClipboard("hello").validate())
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd android && ./gradlew testDebugUnitTest --rerun-tasks --tests '*MediaProtocolTest*'`
Expected: FAIL — unresolved reference `SetClipboard`.

- [ ] **Step 3: Add the variants**

In `MediaProtocol.kt`, add to the `MediaInput` sealed interface:

```kotlin
    @Serializable
    @SerialName("set_clipboard")
    data class SetClipboard(val text: String) : MediaInput
```

Add to the `MediaServerMessage` sealed interface:

```kotlin
    @Serializable
    @SerialName("clipboard")
    data class Clipboard(val text: String) : MediaServerMessage
```

- [ ] **Step 4: Add the validate arm**

In `MediaInput.validate()`, alongside the existing `is MediaInput.Ping -> null`:

```kotlin
        is MediaInput.SetClipboard -> null
```

The size cap is enforced server-side in Task 1. The client does not duplicate it; Task 8 avoids sending over-cap text in the first place.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd android && ./gradlew testDebugUnitTest --rerun-tasks --tests '*MediaProtocolTest*'`
Expected: PASS. Confirm counts in `android/app/build/test-results/testDebugUnitTest/*.xml`.

- [ ] **Step 6: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/net/MediaProtocol.kt \
        android/app/src/test/kotlin/com/greponlabs/navette/net/MediaProtocolTest.kt
git commit -m "feat(android): clipboard message mirrors on the media protocol"
```

---

### Task 3: Per-client server-message queue

**Files:**
- Modify: `crates/navetted/src/media.rs` (`ClientQueueState` at `:305`, `ClientQueue` at `:298`/`:317`, `MediaHub` publish at `:170`, `MediaAttachment` at `:246`)
- Test: `crates/navetted/src/media.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `MediaServerMessage::Clipboard { text: String }` from Task 1.
- Produces: `MediaHub::publish_message(&self, session: &str, message: MediaServerMessage)`, `MediaAttachment::recv_message(&self) -> Option<MediaServerMessage>` (async).

**Design constraint:** do **not** change `ClientQueue::recv` or the `packets` deque. The binary video path carries this project's hardest-won concurrency history. Server messages get their own deque and their own `Notify` so a message wake can never be mistaken for a packet wake.

Unlike packets, server messages are **not** evicted under pressure and have no keyframe logic. Bound the deque at 64 messages and drop the *oldest* on overflow — a clipboard backlog means the socket is not draining, and the newest clipboard value is the only one that matters.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/navetted/src/media.rs`:

```rust
#[tokio::test]
async fn published_messages_reach_every_attached_client() {
    let hub = MediaHub::default();
    // register_session returns the command receiver; it must stay alive
    // for the session to remain registered.
    let _input = hub.register_session("one");
    let first = hub.attach("one").unwrap();
    let second = hub.attach("one").unwrap();

    hub.publish_message(
        "one",
        MediaServerMessage::Clipboard {
            text: "hello".into(),
        },
    );

    assert_eq!(
        first.recv_message().await,
        Some(MediaServerMessage::Clipboard {
            text: "hello".into()
        })
    );
    assert_eq!(
        second.recv_message().await,
        Some(MediaServerMessage::Clipboard {
            text: "hello".into()
        })
    );
}

#[tokio::test]
async fn messages_do_not_disturb_the_packet_queue() {
    let hub = MediaHub::default();
    let _input = hub.register_session("one");
    let client = hub.attach("one").unwrap();

    hub.publish_message(
        "one",
        MediaServerMessage::Clipboard {
            text: "hello".into(),
        },
    );

    // The packet queue must still be empty and must not have been woken.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), client.recv())
            .await
            .is_err(),
        "a server message must not wake the packet queue"
    );
}

#[tokio::test]
async fn message_queue_drops_oldest_when_saturated() {
    let hub = MediaHub::default();
    let _input = hub.register_session("one");
    let client = hub.attach("one").unwrap();

    for index in 0..MESSAGE_QUEUE_CAPACITY + 1 {
        hub.publish_message(
            "one",
            MediaServerMessage::Clipboard {
                text: format!("value-{index}"),
            },
        );
    }

    // The very first value is gone; the second is now at the head.
    assert_eq!(
        client.recv_message().await,
        Some(MediaServerMessage::Clipboard {
            text: "value-1".into()
        })
    );
}
```

Note the construction idiom, which matches the neighbouring tests at `media.rs:449`, `:470` and `:590`: `MediaHub::default()` then `hub.register_session("one")`, binding the returned `mpsc::Receiver<MediaCommand>` to `_input` so the session stays registered for the life of the test. Do not invent a constructor.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p navetted message`
Expected: FAIL — `no method named publish_message`.

- [ ] **Step 3: Add the queue state**

In `crates/navetted/src/media.rs`:

```rust
/// Server messages are small, rare, and order-sensitive only in that the
/// newest clipboard value wins. A backlog means the socket is not
/// draining, so the oldest is dropped rather than the newest.
const MESSAGE_QUEUE_CAPACITY: usize = 64;
```

Add to `ClientQueueState`:

```rust
    messages: VecDeque<MediaServerMessage>,
```

Add to `ClientQueue`:

```rust
    message_notify: Notify,
```

and initialise it in `ClientQueue::new` with `message_notify: Notify::new(),`.

- [ ] **Step 4: Add push and recv for messages**

Add to `impl ClientQueue`:

```rust
    fn push_message(&self, message: MediaServerMessage) {
        let mut state = self.state.lock().expect("media queue lock poisoned");
        if state.closed {
            return;
        }
        while state.messages.len() >= MESSAGE_QUEUE_CAPACITY {
            state.messages.pop_front();
        }
        state.messages.push_back(message);
        drop(state);
        self.message_notify.notify_one();
    }

    async fn recv_message(&self) -> Option<MediaServerMessage> {
        loop {
            let notified = self.message_notify.notified();
            {
                let mut state = self.state.lock().expect("media queue lock poisoned");
                if let Some(message) = state.messages.pop_front() {
                    return Some(message);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
```

In `ClientQueue::close`, wake message waiters too — add `self.message_notify.notify_waiters();` beside the existing `self.notify.notify_waiters();`.

- [ ] **Step 5: Add the hub and attachment entry points**

Add to the `MediaHub` impl, modelled on `publish` at `:170` and using the same lock-and-lookup shape:

```rust
    /// Fan a server message out to every client attached to `session`.
    /// Unlike `publish`, this is infallible and silent: a clipboard push
    /// to a session nobody is watching is a no-op, not an error.
    pub fn publish_message(&self, session: &str, message: MediaServerMessage) {
        let Ok(state) = self.inner.lock() else {
            return;
        };
        let Some(session_state) = state.sessions.get(session) else {
            return;
        };
        for queue in session_state.clients.values() {
            queue.push_message(message.clone());
        }
    }
```

Add to the `MediaAttachment` impl:

```rust
    pub async fn recv_message(&self) -> Option<MediaServerMessage> {
        self.queue.recv_message().await
    }
```

`MediaServerMessage` must derive `Clone` for the fan-out; it already does.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p navetted message`
Expected: PASS, 3 tests.

- [ ] **Step 7: Run the crate suite and lints**

Run: `cargo test -p navetted && cargo clippy -p navetted --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass. In particular every pre-existing media test must still pass — the packet path was not to change.

- [ ] **Step 8: Commit**

```bash
git add crates/navetted/src/media.rs
git commit -m "feat(media): per-client server-message queue

Server messages get their own deque and their own Notify, so a message
wake can never be mistaken for a packet wake. The binary packet path is
untouched; it carries this project's hardest-won concurrency history.

Bounded at 64, dropping oldest: a backlog means the socket is not
draining, and the newest clipboard value is the only one that matters."
```

---

### Task 4: Socket task sends server messages as text

**Files:**
- Modify: `crates/navetted/src/api.rs` (the `tokio::select!` at `:140`)
- Test: `crates/navetted/src/api.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `MediaAttachment::recv_message()` from Task 3.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/navetted/src/api.rs`, modelled on the existing ping/pong socket test:

```rust
#[tokio::test]
async fn published_server_messages_reach_the_socket_as_text() {
    // Use the same harness the pong test uses to stand up a media socket.
    let (hub, mut socket, _session) = media_socket_fixture().await;

    hub.publish_message(
        "mvp",
        MediaServerMessage::Clipboard {
            text: "hello".into(),
        },
    );

    let received = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("a clipboard message should arrive before the timeout")
        .expect("socket should stay open")
        .unwrap();

    assert_eq!(
        received,
        ClientMessage::Text(r#"{"type":"clipboard","text":"hello"}"#.into())
    );
}
```

`media_socket_fixture()` stands for whatever setup the neighbouring ping/pong test at `api.rs:788` already performs — reuse it rather than writing a new harness, and reuse its session name.

**Every socket wait in this file must be wrapped in `tokio::time::timeout`.** An unbounded `socket.next().await` on a message that correctly has not arrived yet hangs the suite for minutes; that happened during the HUD work.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p navetted published_server_messages`
Expected: FAIL — the message never arrives, timeout fires in ~5s.

- [ ] **Step 3: Add the third select arm**

In the `tokio::select!` at `crates/navetted/src/api.rs:140`, alongside the existing `packet = attachment.recv()` and the client-receive arm:

```rust
            message = attachment.recv_message() => {
                let Some(message) = message else { break; };
                let Ok(encoded) = serde_json::to_string(&message) else { break; };
                if sender.send(Message::Text(encoded.into())).await.is_err() {
                    break;
                }
            }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p navetted published_server_messages`
Expected: PASS.

- [ ] **Step 5: Run the crate suite and lints**

Run: `cargo test -p navetted && cargo clippy -p navetted --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/navetted/src/api.rs
git commit -m "feat(api): send server messages on the media socket

Third arm on the select loop. Clipboard pushes travel as JSON text
because an old client logs unrecognized text harmlessly, where an
unknown binary MediaKind would make it throw."
```

---

### Task 5: The clipboard state machine

**Files:**
- Create: `crates/navetted/src/clipboard.rs`
- Modify: `crates/navetted/src/lib.rs` (add `pub mod clipboard;` beside the existing `pub mod` declarations — the file uses `pub mod` throughout)
- Test: `crates/navetted/src/clipboard.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub struct ClipboardSync` with `ClipboardSync::new() -> Self` (also `Default`)
  - `pub enum GuestEvent { SelectionOffered { mime_types: Vec<String> }, TransferFromGuest { bytes: Vec<u8> }, PasteRequested }`
  - `pub enum SyncAction { Nothing, AskGuestFor { mime: String }, PushToPhone { text: String }, OfferToGuest { mime_types: Vec<String> }, AnswerGuest { bytes: Vec<u8> } }`
  - `pub fn on_guest(&mut self, event: GuestEvent) -> SyncAction`
  - `pub fn on_phone_clipboard(&mut self, text: String) -> SyncAction`
  - `pub const OFFERED_MIME_TYPES: [&str; 5]`

**This file contains no wprs types, no transport, and no I/O.** That is the point: every decision in the feature becomes a plain unit test, and Task 6 is reduced to translation.

**Echo token naming.** The spec calls these `echo_token_to_guest` / `echo_token_to_phone`. This plan names them `echo_from_guest` / `echo_from_phone` — same semantics, named for the direction the echo *arrives from*, which is the direction each one suppresses. Use these names.

- [ ] **Step 1: Write the failing tests**

Create `crates/navetted/src/clipboard.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn offer(mimes: &[&str]) -> GuestEvent {
        GuestEvent::SelectionOffered {
            mime_types: mimes.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    #[test]
    fn prefers_utf8_plain_text_over_other_spellings() {
        let mut sync = ClipboardSync::new();
        let action = sync.on_guest(offer(&["STRING", "text/plain", "text/plain;charset=utf-8"]));
        assert_eq!(
            action,
            SyncAction::AskGuestFor {
                mime: "text/plain;charset=utf-8".into()
            }
        );
    }

    #[test]
    fn falls_back_through_the_x11_atoms() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["TEXT", "STRING"])),
            SyncAction::AskGuestFor {
                mime: "STRING".into()
            }
        );
    }

    #[test]
    fn asks_using_the_guests_own_spelling() {
        // The guest offered a spaced variant. We must ask for the string it
        // actually offered, not our normalised form, or it will not match.
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["text/plain; charset=utf-8"])),
            SyncAction::AskGuestFor {
                mime: "text/plain; charset=utf-8".into()
            }
        );
    }

    #[test]
    fn an_offer_with_no_text_form_is_ignored() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(offer(&["image/png", "application/pdf"])),
            SyncAction::Nothing
        );
    }

    #[test]
    fn guest_transfer_is_pushed_to_the_phone() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::PushToPhone {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn invalid_utf8_from_the_guest_is_dropped() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: vec![0xff, 0xfe, 0xfd]
            }),
            SyncAction::Nothing
        );
    }

    #[test]
    fn oversized_guest_transfer_is_dropped() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: vec![b'a'; MAX_CLIPBOARD_BYTES + 1]
            }),
            SyncAction::Nothing
        );
    }

    #[test]
    fn phone_clipboard_is_offered_to_the_guest() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_phone_clipboard("hello".into()),
            SyncAction::OfferToGuest {
                mime_types: OFFERED_MIME_TYPES.iter().map(|m| (*m).to_string()).collect()
            }
        );
    }

    #[test]
    fn a_paste_is_answered_with_the_phones_text() {
        let mut sync = ClipboardSync::new();
        sync.on_phone_clipboard("hello".into());
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested),
            SyncAction::AnswerGuest {
                bytes: b"hello".to_vec()
            }
        );
    }

    /// The hang regression. wprsd takes the pipe fd when it forwards the
    /// paste; if we answer with nothing, that pipe is never written and
    /// never closed, and the pasting guest app blocks on read forever.
    #[test]
    fn a_paste_with_no_phone_text_is_still_answered() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(GuestEvent::PasteRequested),
            SyncAction::AnswerGuest { bytes: Vec::new() }
        );
    }

    #[test]
    fn our_own_value_coming_back_from_the_guest_is_suppressed() {
        let mut sync = ClipboardSync::new();
        sync.on_phone_clipboard("hello".into());
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::Nothing,
            "the value we just sent to the guest must not bounce back"
        );
    }

    #[test]
    fn our_own_value_coming_back_from_the_phone_is_suppressed() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        sync.on_guest(GuestEvent::TransferFromGuest {
            bytes: b"hello".to_vec(),
        });
        assert_eq!(
            sync.on_phone_clipboard("hello".into()),
            SyncAction::Nothing,
            "the value we just pushed to the phone must not bounce back"
        );
    }

    /// The discriminating test. A token that is merely compared and
    /// retained passes every distinct-string test above and fails only
    /// this one: the same text, legitimately copied again on the other
    /// side, must still propagate.
    #[test]
    fn the_echo_token_is_one_shot() {
        let mut sync = ClipboardSync::new();

        // Phone sends "hello"; the guest echoes it straight back.
        sync.on_phone_clipboard("hello".into());
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::Nothing
        );

        // Later, a user genuinely copies "hello" in a guest window again.
        // This must reach the phone.
        sync.on_guest(offer(&["text/plain"]));
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::PushToPhone {
                text: "hello".into()
            },
            "suppression is one-shot; the same text copied again must propagate"
        );
    }

    #[test]
    fn phone_echo_token_is_also_one_shot() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        sync.on_guest(GuestEvent::TransferFromGuest {
            bytes: b"hello".to_vec(),
        });
        assert_eq!(sync.on_phone_clipboard("hello".into()), SyncAction::Nothing);
        assert_eq!(
            sync.on_phone_clipboard("hello".into()),
            SyncAction::OfferToGuest {
                mime_types: OFFERED_MIME_TYPES.iter().map(|m| (*m).to_string()).collect()
            },
            "the second genuine copy of the same text must propagate"
        );
    }

    /// A transfer arriving with no offer in flight belongs to no request we
    /// made. wprsd keeps one pipe slot per source and overwrites rather
    /// than correlating, so we match that model instead of promising more.
    #[test]
    fn a_transfer_with_no_outstanding_request_is_dropped() {
        let mut sync = ClipboardSync::new();
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"hello".to_vec()
            }),
            SyncAction::Nothing
        );
    }

    #[test]
    fn a_re_offer_supersedes_the_previous_request() {
        let mut sync = ClipboardSync::new();
        sync.on_guest(offer(&["text/plain"]));
        sync.on_guest(offer(&["text/plain"]));
        // Exactly one transfer is consumed by the outstanding request.
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"second".to_vec()
            }),
            SyncAction::PushToPhone {
                text: "second".into()
            }
        );
        // A second transfer has no request left to answer.
        assert_eq!(
            sync.on_guest(GuestEvent::TransferFromGuest {
                bytes: b"stale".to_vec()
            }),
            SyncAction::Nothing
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Add `pub mod clipboard;` to `crates/navetted/src/lib.rs`, then run:
`cargo test -p navetted clipboard`
Expected: FAIL to compile — `cannot find struct ClipboardSync`.

- [ ] **Step 3: Write the implementation**

Put this **above** the test module in `crates/navetted/src/clipboard.rs`:

```rust
//! The clipboard decision layer.
//!
//! Deliberately free of wprs types, transport and I/O: every decision the
//! feature makes is a plain function of the events it has seen, so the
//! whole state machine is unit-testable and `bridge.rs` is reduced to
//! translation.
//!
//! Clipboard content never appears in a log line here or anywhere else.

use navette_protocol::media::MAX_CLIPBOARD_BYTES;

/// Offered to the guest when the phone sets a clipboard value, and
/// searched in this order when the guest offers one. `UTF8_STRING`,
/// `STRING` and `TEXT` are X11 atoms and arrive from XWayland guests,
/// which is the common case for browsers.
pub const OFFERED_MIME_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// Something the guest side did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GuestEvent {
    /// The guest set its selection and offered these MIME types.
    SelectionOffered { mime_types: Vec<String> },
    /// The guest sent the bytes we asked for.
    TransferFromGuest { bytes: Vec<u8> },
    /// A guest application pasted and wants our data.
    PasteRequested,
}

/// What the caller should do next. Exactly one action per event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncAction {
    Nothing,
    AskGuestFor { mime: String },
    PushToPhone { text: String },
    OfferToGuest { mime_types: Vec<String> },
    AnswerGuest { bytes: Vec<u8> },
}

#[derive(Debug, Default)]
pub struct ClipboardSync {
    /// The phone's latest clipboard text, retained to answer a guest paste
    /// that may arrive seconds later, or never.
    phone_text: Option<String>,
    /// Set when we ask the guest for data, cleared when a transfer
    /// consumes it. A transfer arriving with this unset answers no request
    /// we made and is dropped.
    awaiting_guest_transfer: bool,
    /// One-shot echo tokens, each named for the direction the echo arrives
    /// from. Set when we send in that direction; consumed by the first
    /// matching inbound value. They MUST be cleared on match: a retained
    /// token silently suppresses the same text legitimately copied later,
    /// forever.
    echo_from_guest: Option<String>,
    echo_from_phone: Option<String>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_guest(&mut self, event: GuestEvent) -> SyncAction {
        match event {
            GuestEvent::SelectionOffered { mime_types } => {
                match select_text_mime(&mime_types) {
                    Some(mime) => {
                        self.awaiting_guest_transfer = true;
                        SyncAction::AskGuestFor { mime }
                    }
                    // The guest offered no text form. That is a real
                    // state, not an empty string.
                    None => SyncAction::Nothing,
                }
            }
            GuestEvent::TransferFromGuest { bytes } => {
                if !self.awaiting_guest_transfer {
                    return SyncAction::Nothing;
                }
                self.awaiting_guest_transfer = false;

                if bytes.len() > MAX_CLIPBOARD_BYTES {
                    tracing::debug!(
                        bytes = bytes.len(),
                        "dropping oversized clipboard transfer from guest"
                    );
                    return SyncAction::Nothing;
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    tracing::debug!("dropping non-UTF-8 clipboard transfer from guest");
                    return SyncAction::Nothing;
                };

                if self.echo_from_guest.as_deref() == Some(text.as_str()) {
                    self.echo_from_guest = None;
                    return SyncAction::Nothing;
                }

                self.echo_from_phone = Some(text.clone());
                SyncAction::PushToPhone { text }
            }
            // Always answer. wprsd has taken the pipe fd; leaving it
            // unwritten and unclosed hangs the pasting guest app forever.
            GuestEvent::PasteRequested => SyncAction::AnswerGuest {
                bytes: self
                    .phone_text
                    .as_ref()
                    .map(|text| text.as_bytes().to_vec())
                    .unwrap_or_default(),
            },
        }
    }

    pub fn on_phone_clipboard(&mut self, text: String) -> SyncAction {
        if self.echo_from_phone.as_deref() == Some(text.as_str()) {
            self.echo_from_phone = None;
            return SyncAction::Nothing;
        }

        self.echo_from_guest = Some(text.clone());
        self.phone_text = Some(text);
        SyncAction::OfferToGuest {
            mime_types: OFFERED_MIME_TYPES
                .iter()
                .map(|mime| (*mime).to_string())
                .collect(),
        }
    }
}

/// Pick the guest's own spelling of the most preferred text MIME type it
/// offered. Comparison ignores ASCII whitespace and case so that
/// `text/plain; charset=utf-8` matches, but the returned string is the one
/// the guest actually offered — asking with a normalised spelling it never
/// advertised would not match.
fn select_text_mime(offered: &[String]) -> Option<String> {
    OFFERED_MIME_TYPES.iter().find_map(|preferred| {
        let wanted = normalize_mime(preferred);
        offered
            .iter()
            .find(|candidate| normalize_mime(candidate) == wanted)
            .cloned()
    })
}

fn normalize_mime(mime: &str) -> String {
    mime.chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p navetted clipboard`
Expected: PASS, 16 tests.

- [ ] **Step 5: Run the crate suite and lints**

Run: `cargo test -p navetted && cargo clippy -p navetted --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/navetted/src/clipboard.rs crates/navetted/src/lib.rs
git commit -m "feat(clipboard): the clipboard decision layer

A state machine with no wprs types, no transport and no I/O, so every
decision the feature makes is a plain unit test and bridge.rs is reduced
to translation.

The echo tokens are one-shot -- consumed on first match, not retained. A
retained token passes every distinct-string test and silently suppresses
the same text legitimately copied later, forever. That case has its own
test.

A paste is always answered, empty if need be: wprsd has taken the pipe
fd, and leaving it unwritten hangs the pasting guest app."
```

---

### Task 6: Bridge wiring

**Files:**
- Modify: `crates/navetted/src/bridge.rs` (batch drain at `:247-250`, `apply_scene_messages` at `:385`, the input apply path at `:497`)
- Test: `crates/navetted/src/bridge.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: `ClipboardSync`, `GuestEvent`, `SyncAction`, `OFFERED_MIME_TYPES` from Task 5; `MediaHub::publish_message` from Task 3; `MediaInput::SetClipboard` from Task 1.

**Placement, and why it is not in `navette-bridge`:** `scene.apply()` only *returns* `Vec<SceneEvent>` — it has no transport and cannot send. Every `transport.send(Event::…)` in the tree lives in `navette-bridge`'s `transport.rs` or `input.rs`, never `scene.rs`. `bridge.rs:393` has the raw `RecvType<Request>` in hand before `scene.apply()`, and `transport` is in scope at the batch-drain site, so `Request::Data` is partitioned out there — the same interception pattern `api.rs` uses for `Ping` ahead of `submit_input`. `scene.rs:365`'s `Request::Data(_) => Ok(Vec::new())` stays as a backstop; after interception nothing reaches it.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/navetted/src/bridge.rs`:

```rust
/// A guest copy travels: offer in, request out, bytes in, push to the
/// media hub. Uses the same bridge-loop harness the neighbouring tests
/// use rather than a new one.
#[tokio::test]
async fn a_guest_copy_reaches_the_media_hub() {
    use navette_protocol::media::MediaServerMessage;
    use wprs::serialization::wayland::{
        DataRequest, DataSource, DataSourceRequest, SourceMetadata,
    };
    use wprs::serialization::{RecvType, Request};

    let harness = bridge_harness().await;

    harness
        .send_request(RecvType::Object(Request::Data(DataRequest::SourceRequest(
            DataSourceRequest::SetSelection(
                DataSource::Selection,
                SourceMetadata::from_mime_types(vec!["text/plain".to_string()]),
            ),
        ))))
        .await;

    // The bridge must have asked the guest for the data.
    assert!(
        harness.sent_events().await.iter().any(|event| matches!(
            event,
            wprs::serialization::Event::Data(
                wprs::serialization::wayland::DataEvent::SourceEvent(_)
            )
        )),
        "the bridge must ask the guest for the offered text"
    );

    harness
        .send_request(RecvType::Object(Request::Data(DataRequest::TransferData(
            DataSource::Selection,
            wprs::serialization::wayland::DataToTransfer(b"hello".to_vec()),
        ))))
        .await;

    assert_eq!(
        harness.published_messages().await,
        vec![MediaServerMessage::Clipboard {
            text: "hello".into()
        }],
        "the guest's clipboard text must reach the media hub"
    );
}
```

`bridge_harness()`, `send_request`, `sent_events` and `published_messages` stand for the fixtures the existing bridge tests already use to drive the loop and drain the hub (see the helper described at `bridge.rs:1372`). Extend those helpers rather than building a parallel harness; if a needed accessor does not exist, add it to the existing helper.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p navetted a_guest_copy_reaches_the_media_hub`
Expected: FAIL — no event is sent, because `Request::Data` is still dropped.

- [ ] **Step 3: Hold a `ClipboardSync` on the bridge worker**

Add a `clipboard: ClipboardSync` field to the bridge worker struct that already owns `scene` and `input`, initialised with `ClipboardSync::new()`. It is plain state, not behind a lock: calloop is single-threaded and both the guest's pull and our answer arrive on it.

- [ ] **Step 4: Partition `Request::Data` out of the batch**

At the batch-drain site (`bridge.rs:247-250`), route `Request::Data` to the clipboard handler instead of into `apply_scene_messages`:

```rust
            let batch = pending
                .drain(..)
                .filter_map(|event| match event {
                    ChannelEvent::Msg(message) => Some(message),
                    _ => None,
                })
                .filter(|message| {
                    // Clipboard is intercepted here, before scene.apply(),
                    // because scene.apply() has no transport and cannot
                    // send. Same shape as api.rs intercepting Ping ahead
                    // of submit_input.
                    if let RecvType::Object(Request::Data(request)) = message {
                        handle_guest_data(
                            &mut worker.clipboard,
                            request.clone(),
                            &transport,
                            &media,
                            session,
                        );
                        return false;
                    }
                    true
                })
                .collect::<Vec<_>>()
                .into_iter();
```

Match the exact names the surrounding code uses for `transport`, `media` and the session identifier; take them from the `publish` call at `bridge.rs:792`.

- [ ] **Step 5: Write the translation function**

Add to `crates/navetted/src/bridge.rs`:

```rust
/// Translate a wprs data request into a `ClipboardSync` event, then carry
/// out whatever it decides. Pure translation: every decision lives in
/// `crate::clipboard`.
fn handle_guest_data(
    clipboard: &mut ClipboardSync,
    request: DataRequest,
    transport: &Transport,
    media: &MediaHub,
    session: &str,
) {
    let event = match request {
        DataRequest::SourceRequest(DataSourceRequest::SetSelection(
            DataSource::Selection,
            metadata,
        )) => GuestEvent::SelectionOffered {
            mime_types: metadata.mime_types,
        },
        DataRequest::TransferData(DataSource::Selection, data) => {
            GuestEvent::TransferFromGuest { bytes: data.0 }
        }
        DataRequest::DestinationRequest(DataDestinationRequest::RequestDataTransfer(
            DataSource::Selection,
            _mime,
        )) => GuestEvent::PasteRequested,
        // Primary selection, drag and drop, and every other data request
        // are out of scope.
        _ => return,
    };

    apply_sync_action(clipboard.on_guest(event), transport, media, session);
}

fn apply_sync_action(
    action: SyncAction,
    transport: &Transport,
    media: &MediaHub,
    session: &str,
) {
    match action {
        SyncAction::Nothing => {}
        SyncAction::AskGuestFor { mime } => {
            transport.send(Event::Data(DataEvent::SourceEvent(
                DataSourceEvent::MimeTypeSendRequestedByDestination(DataSource::Selection, mime),
            )));
        }
        SyncAction::PushToPhone { text } => {
            media.publish_message(session, MediaServerMessage::Clipboard { text });
        }
        SyncAction::OfferToGuest { mime_types } => {
            transport.send(Event::Data(DataEvent::DestinationEvent(
                DataDestinationEvent::SelectionSet(
                    DataSource::Selection,
                    SourceMetadata::from_mime_types(mime_types),
                ),
            )));
        }
        SyncAction::AnswerGuest { bytes } => {
            transport.send(Event::Data(DataEvent::TransferData(
                DataSource::Selection,
                DataToTransfer(bytes),
            )));
        }
    }
}
```

Use the transport's actual send method and type name as used at `bridge.rs` / `navette-bridge/src/transport.rs:29`; do not invent a wrapper.

- [ ] **Step 6: Route `MediaInput::SetClipboard`**

In the input apply path (`bridge.rs:497`, where `input_state.apply` handles a `MediaInput`), intercept `SetClipboard` before it reaches the pointer/keyboard handling — it is not a pointer or keyboard event and has no surface:

```rust
                if let MediaInput::SetClipboard { text } = input {
                    apply_sync_action(
                        worker.clipboard.on_phone_clipboard(text),
                        transport,
                        media,
                        session,
                    );
                    continue;
                }
```

Adapt the control flow (`continue` / early return) to the surrounding loop's shape.

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test -p navetted a_guest_copy_reaches_the_media_hub`
Expected: PASS.

- [ ] **Step 8: Add the phone→guest integration test**

```rust
#[tokio::test]
async fn a_phone_copy_is_offered_and_answered() {
    use wprs::serialization::wayland::{
        DataDestinationRequest, DataRequest, DataSource, DataToTransfer,
    };
    use wprs::serialization::{RecvType, Request};

    let harness = bridge_harness().await;

    harness
        .send_input(MediaInput::SetClipboard {
            text: "hello".into(),
        })
        .await;

    assert!(
        harness.sent_events().await.iter().any(|event| matches!(
            event,
            wprs::serialization::Event::Data(
                wprs::serialization::wayland::DataEvent::DestinationEvent(_)
            )
        )),
        "the phone's clipboard must be offered to the guest"
    );

    harness
        .send_request(RecvType::Object(Request::Data(
            DataRequest::DestinationRequest(DataDestinationRequest::RequestDataTransfer(
                DataSource::Selection,
                "text/plain".to_string(),
            )),
        )))
        .await;

    assert!(
        harness.sent_events().await.iter().any(|event| matches!(
            event,
            wprs::serialization::Event::Data(
                wprs::serialization::wayland::DataEvent::TransferData(
                    DataSource::Selection,
                    DataToTransfer(bytes),
                )
            ) if bytes == b"hello"
        )),
        "the guest's paste must be answered with the phone's text"
    );
}
```

Run: `cargo test -p navetted a_phone_copy_is_offered_and_answered`
Expected: PASS.

- [ ] **Step 9: Run the workspace suite and lints**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass.

- [ ] **Step 10: Commit**

```bash
git add crates/navetted/src/bridge.rs
git commit -m "feat(bridge): wire clipboard through the wprs data device

Request::Data is partitioned out of the batch before scene.apply(),
because scene.apply() only returns SceneEvents and has no transport --
the same interception api.rs uses for Ping ahead of submit_input. This
keeps clipboard state out of the navette-bridge crate entirely.

Both directions are pure translation; every decision stays in
crate::clipboard."
```

---

### Task 7: Remove the control-protocol clipboard surface

**Files:**
- Modify: `crates/navette-protocol/src/lib.rs` (`:44`, `:47`, `:106`)
- Modify: `crates/navetted/src/api.rs` (`:35`, `:45`, `:58`, `:378-392`, and the self-echo test at `:576-605`)
- Modify: `crates/navette-cli/src/lib.rs` (`:100`)
- Modify: `android/.../protocol/ControlProtocol.kt` (`:62`, `:66`, `:93`)
- Modify: `android/.../protocol/ControlProtocolTest.kt` (`:33-35`)

**Interfaces:** removes `RequestCommand::SetClipboard`, `RequestCommand::GetClipboard`, `ResponseResult::Clipboard` and their Kotlin mirrors. Nothing consumes them after this task.

**Why removal and not extension:** the control socket (`api.rs:270`) is a strict request/response pump with no server-initiated source and no channel to the bridge — which is precisely why the stub could only echo to itself. Leaving it beside a working implementation would ship two clipboards, one of them fake.

Nothing *sends* these commands. `navette-cli` has no clipboard subcommand — `Command` at `main.rs:43` is `Ls`/`Run`/`Attach`/`Detach`/`Kill` — so `lib.rs:100` is only an exhaustive-match render arm that exists because the variant does.

- [ ] **Step 1: Delete the Rust protocol variants**

Remove `SetClipboard { text: String }` and `GetClipboard` from `RequestCommand`, and `Clipboard { text: Option<String> }` from `ResponseResult`, in `crates/navette-protocol/src/lib.rs`.

- [ ] **Step 2: Delete the daemon state and dispatch arms**

In `crates/navetted/src/api.rs`, remove the `clipboard: Arc<Mutex<Option<String>>>` field (`:35`), its two initialisers (`:45`, `:58`), both dispatch arms (`:378-392`), and the `set_then_get_clipboard`-style test at `:576-605`. Drop any `Mutex`/`Arc` imports left unused.

- [ ] **Step 3: Delete the CLI render arm**

Remove `ResponseResult::Clipboard { text } => text.clone().unwrap_or_default(),` from `crates/navette-cli/src/lib.rs:100`. The match is exhaustive over `ResponseResult`, so removing the variant in Step 1 makes this arm a compile error until it goes.

- [ ] **Step 4: Delete the Kotlin mirrors and their tests**

Remove `SetClipboard` (`:62`), `GetClipboard` (`:66`) and `Clipboard` (`:93`) from `ControlProtocol.kt`, and the two round-trip cases at `ControlProtocolTest.kt:33-35`.

- [ ] **Step 5: Verify nothing references the removed surface**

Run:
```bash
grep -rn "SetClipboard\|GetClipboard\|ResponseResult::Clipboard\|RequestCommand.SetClipboard\|RequestCommand.GetClipboard" \
  crates/ android/app/src/main android/app/src/test | grep -v "media\|Media"
```
Expected: no output. The `grep -v` excludes the *media*-socket clipboard added in Tasks 1-2, which stays.

- [ ] **Step 6: Run both suites and lints**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Then: `cd android && ./gradlew testDebugUnitTest --rerun-tasks`
Expected: all pass. Read Android counts from the JUnit XML.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "refactor: remove the control-protocol clipboard stub

An Arc<Mutex<Option<String>>> that SetClipboard wrote and GetClipboard
read back. It never reached wprs and never reached the guest, because the
control socket is a request/response pump with no channel to the bridge
at all. Nothing sent it; navette-cli had only an exhaustive-match render
arm and Kotlin had only round-trip tests.

Leaving it beside the real implementation would ship two clipboards, one
of them fake."
```

---

### Task 8: Android clipboard integration

**Files:**
- Create: `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/ClipboardBridge.kt`
- Create: `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/ClipboardBridgeTest.kt`
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt` (wire the bridge to the session lifecycle and the media socket)

**Interfaces:**
- Consumes: `MediaInput.SetClipboard(text)` and `MediaServerMessage.Clipboard(text)` from Task 2.
- Produces: `ClipboardBridge`, with `onLocalClipboard(text: String): String?` and `onRemoteClipboard(text: String): String?`.

**Design:** `ClipboardBridge` holds the *decisions* and touches no Android framework class, exactly as `SessionHud` does — that is what makes it testable in a plain JVM test. `SessionScreen.kt` owns the `ClipboardManager` calls, the listener registration and the lifecycle observer.

**Why the resume read is not a fallback:** since Android 10 `getPrimaryClip()` returns null when the app is not focused, and copying text from another app necessarily unfocuses Navette. The listener alone would miss exactly the case the feature exists for.

**Local echo suppression:** when a remote push sets the clipboard via `setPrimaryClip`, the listener fires immediately. Without suppression that bounces straight back to the daemon. The daemon suppresses too (Task 5), but the Android side must not rely on it — the resume-read path carries no provenance.

- [ ] **Step 1: Write the failing tests**

Create `ClipboardBridgeTest.kt`:

```kotlin
package com.greponlabs.navette.ui.session

import kotlin.test.assertEquals
import kotlin.test.assertNull
import org.junit.Test

class ClipboardBridgeTest {
    @Test
    fun aLocalCopyIsForwarded() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onLocalClipboard("hello"))
    }

    @Test
    fun aRemotePushIsWrittenLocally() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onRemoteClipboard("hello"))
    }

    @Test
    fun ourOwnRemoteWriteIsNotForwardedBack() {
        val bridge = ClipboardBridge()
        bridge.onRemoteClipboard("hello")
        assertNull(
            bridge.onLocalClipboard("hello"),
            "the listener firing on our own setPrimaryClip must not bounce back",
        )
    }

    @Test
    fun theEchoTokenIsOneShot() {
        val bridge = ClipboardBridge()
        bridge.onRemoteClipboard("hello")
        assertNull(bridge.onLocalClipboard("hello"))
        assertEquals(
            "hello",
            bridge.onLocalClipboard("hello"),
            "the same text copied again by the user must propagate",
        )
    }

    @Test
    fun anUnchangedLocalClipboardIsNotResent() {
        val bridge = ClipboardBridge()
        assertEquals("hello", bridge.onLocalClipboard("hello"))
        assertNull(
            bridge.onLocalClipboard("hello"),
            "a resume read of unchanged content must not resend",
        )
    }

    @Test
    fun blankClipboardContentIsIgnored() {
        val bridge = ClipboardBridge()
        assertNull(bridge.onLocalClipboard(""))
    }

    @Test
    fun overCapTextIsNotSent() {
        val bridge = ClipboardBridge()
        assertNull(bridge.onLocalClipboard("a".repeat(MAX_CLIPBOARD_BYTES + 1)))
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd android && ./gradlew testDebugUnitTest --rerun-tasks --tests '*ClipboardBridgeTest*'`
Expected: FAIL — unresolved reference `ClipboardBridge`.

- [ ] **Step 3: Write `ClipboardBridge`**

Create `ClipboardBridge.kt`:

```kotlin
package com.greponlabs.navette.ui.session

/** Mirrors the daemon's cap. UTF-8 bytes, not characters. */
const val MAX_CLIPBOARD_BYTES: Int = 1024 * 1024

/**
 * Decides what to do with a clipboard change, and touches no Android
 * framework class -- which is what makes every decision here a plain JVM
 * test. SessionScreen owns the ClipboardManager calls.
 *
 * Clipboard content is never logged.
 */
class ClipboardBridge {
    /** The last value we wrote locally, suppressing the listener firing on
     *  our own write. One-shot: cleared on first match, so the same text
     *  genuinely copied again still propagates. */
    private var echoFromLocal: String? = null

    /** The last value we sent, so a resume read of unchanged content does
     *  not resend it. */
    private var lastSent: String? = null

    /** A local clipboard change. Returns the text to send, or null. */
    fun onLocalClipboard(text: String): String? {
        if (text.isEmpty()) return null
        if (text.toByteArray(Charsets.UTF_8).size > MAX_CLIPBOARD_BYTES) return null

        if (echoFromLocal == text) {
            echoFromLocal = null
            return null
        }
        if (lastSent == text) return null

        lastSent = text
        return text
    }

    /** A push from the daemon. Returns the text to write locally, or null. */
    fun onRemoteClipboard(text: String): String? {
        if (text.isEmpty()) return null
        echoFromLocal = text
        lastSent = text
        return text
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd android && ./gradlew testDebugUnitTest --rerun-tasks --tests '*ClipboardBridgeTest*'`
Expected: PASS, 7 tests.

- [ ] **Step 5: Wire it into the session**

In `SessionScreen.kt`, hold a `ClipboardBridge` alongside the other session state and:

1. Obtain `ClipboardManager` via `context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager`.
2. Register an `OnPrimaryClipChangedListener` that reads `clipboard.primaryClip?.getItemAt(0)?.coerceToText(context)?.toString()`, passes it to `bridge.onLocalClipboard`, and on a non-null result sends `MediaInput.SetClipboard(text)` over the media socket.
3. Add a `LifecycleEventObserver` for `Lifecycle.Event.ON_RESUME` performing the same read-and-forward. This is the path most real transfers take.
4. On `MediaServerMessage.Clipboard`, call `bridge.onRemoteClipboard(text)` and on a non-null result `clipboard.setPrimaryClip(ClipData.newPlainText("navette", text))`.
5. Unregister the listener and the observer in the same teardown that already stops the decoder.

Follow the existing lock and lifecycle conventions in this file. **Do not log clipboard content** in any branch.

- [ ] **Step 6: Run the full Android suite**

Run: `cd android && ./gradlew testDebugUnitTest --rerun-tasks`
Expected: all pass. Read the counts from the JUnit XML — a cached "BUILD SUCCESSFUL" can run zero tests.

- [ ] **Step 7: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/ui/session/ClipboardBridge.kt \
        android/app/src/test/kotlin/com/greponlabs/navette/ui/session/ClipboardBridgeTest.kt \
        android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt
git commit -m "feat(android): bidirectional clipboard on the session screen

ClipboardBridge holds the decisions and touches no framework class, so
every one of them is a plain JVM test.

Reads on ON_RESUME as well as on the listener, and the resume read is not
a fallback: since Android 10 getPrimaryClip() returns null when unfocused,
and copying from another app necessarily unfocuses Navette, so the
listener alone would miss the case the feature exists for.

Suppresses the listener firing on our own setPrimaryClip, one-shot, so the
same text genuinely copied again still propagates."
```

---

### Task 9: On-device verification and documentation

**Files:**
- Modify: `docs/HANDOFF.md`
- Modify: `docs/ROADMAP.md`

**Interfaces:** none. This task records what was measured and what remains.

- [ ] **Step 1: Build and install**

```bash
cargo build --release
cd android && ./gradlew installDebug
```

Before measuring anything, confirm the running daemon is not stale:
```bash
find crates -name "*.rs" -newer target/release/navetted | head
```
Any output means the binary predates the source — restart the daemon. A `navetted` left running from an earlier session is routinely built from an older commit.

- [ ] **Step 2: Run the on-device checks**

Record the result of each:

1. Copy text in Firefox on the guest; paste on the phone.
2. Copy text on the phone; paste into a guest window.
3. After a single copy in either direction, watch the logs for repeated clipboard traffic — **the echo-loop check**.
4. Paste in a guest app while the phone clipboard has never been set. The app must not hang. This is the unanswered-pull regression.
5. Copy a large document (over 1 MiB) on the phone; confirm it is refused, not truncated, and the session survives.
6. Note which MIME spellings real guests actually offer, resolving spec open question 2, and whether wprsd echoes `SetSelection` back to us, resolving open question 1.

- [ ] **Step 3: Write up the results**

Append a dated section to `docs/HANDOFF.md` with the measured results of all six checks — including any that failed or could not be run, stated plainly — and any follow-ups this branch is deliberately leaving.

Record the answers to both spec open questions. If wprsd does echo `SetSelection`, say so explicitly: it makes the one-shot echo token load-bearing in production rather than merely defensive.

- [ ] **Step 4: Update the roadmap**

In `docs/ROADMAP.md`, mark text clipboard sync in Phase 2 as done, and note that rich types (`text/html`, images) and drag-and-drop remain out of scope, with the MIME negotiation built here as their foundation.

- [ ] **Step 5: Commit**

```bash
git add docs/HANDOFF.md docs/ROADMAP.md
git commit -m "docs: clipboard sync verified on device"
```

---

## Self-Review

**1. Spec coverage**

| Spec section | Task |
|---|---|
| Wire shapes, no `MEDIA_VERSION` bump | 1, 2 |
| New plumbing (per-client message queue, third select arm) | 3, 4 |
| Where the handling lives (`bridge.rs`, not `scene.rs`) | 6 |
| State ownership (`phone_text`, generation, one-shot echo tokens) | 5 |
| Flow: guest → phone | 5, 6 |
| Flow: phone → guest | 5, 6 |
| On attach (deliberately no replay) | Covered by omission — no task adds replay, which is the specified behaviour. |
| Hazard: echo loop | 5 (one-shot tokens + the discriminating test), 8 (local suppression) |
| Hazard: unanswered pull | 5 (`a_paste_with_no_phone_text_is_still_answered`), 9 check 4 |
| Hazard: size | 1 (`validate()` arm), 5 (guest→phone drop), 8 (client-side) |
| Privacy | Global Constraints; enforced in 5, 6, 8 |
| Android listener + resume read + toast cost | 8 |
| Scope: text only, Selection only, no DnD | 5 (`OFFERED_MIME_TYPES`), 6 (`_ => return`) |
| Removals | 7 |
| Testing | Every task; on-device in 9 |
| Open questions 1 and 2 | 9 step 2 check 6 |

No gaps.

**2. Placeholder scan**

No "TBD", "TODO", "handle edge cases", or "similar to Task N". Three places name an existing fixture rather than reproducing it — `media_socket_fixture()` in Task 4, `bridge_harness()` in Task 6, and the `SessionScreen.kt` wiring in Task 8 step 5. Each says explicitly which existing code to model on and where it lives, because inventing a parallel harness beside a working one is the worse outcome.

**3. Type consistency**

- `MAX_CLIPBOARD_BYTES` — defined in Task 1, imported in Task 5, mirrored as a separate Kotlin constant in Task 8 (deliberately duplicated across the language boundary; the Kotlin one is documented as mirroring).
- `ClipboardSync` / `GuestEvent` / `SyncAction` / `OFFERED_MIME_TYPES` — defined in Task 5, consumed in Task 6 under exactly those names.
- `publish_message` / `recv_message` — defined in Task 3, consumed in Tasks 4 and 6.
- `MediaServerMessage::Clipboard { text }` — one shape throughout; Kotlin mirror `MediaServerMessage.Clipboard(val text: String)`.
- Echo token names `echo_from_guest` / `echo_from_phone` are used consistently in Task 5 and flagged in that task as a deliberate rename of the spec's `echo_token_to_*`.
