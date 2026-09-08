# Android Performance HUD Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show live stream metrics on the Android session screen — frame rate, bitrate, decode cost, frame age, network round trip, dropped packets and discontinuities — toggled by a two-finger long-press.

**Architecture:** Metric arithmetic lives in a pure Kotlin object with a caller-supplied clock, mirroring `crates/navette-viewer/src/hud.rs`, so every rolling window is asserted against a scripted clock with no Android dependency. Round-trip time adds one message in each direction to the media protocol (`MediaInput::Ping` / `MediaServerMessage::Pong`), answered by `navetted`'s media socket task — the only server-to-client JSON path that exists. The Compose overlay renders a computed sample and calculates nothing.

**Tech Stack:** Kotlin, Jetpack Compose, kotlinx.serialization, OkHttp WebSocket, JUnit; Rust, serde, axum, tokio-tungstenite.

**Spec:** `docs/superpowers/specs/2026-09-07-android-perf-hud-design.md`

## Global Constraints

- Kotlin files stay under 800 lines. `ui/session/SessionScreen.kt` is already 827 and must not grow materially; new code goes in new files.
- Never use `!!`. Prefer `val`. Exhaustive `when` over sealed types, no `else` branch.
- Rust: `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` must stay clean. No `unwrap()` outside tests.
- No `MEDIA_VERSION` bump. `MEDIA_VERSION` stays `1` and `MEDIA_WEBSOCKET_SUBPROTOCOL` stays `navette.media.v1`.
- Wire field names are `snake_case` on both sides. Rust `u64` maps to Kotlin `ULong`.
- One ping per second. `MAX_INPUT_MESSAGES_PER_SECOND` is 240 and is not changed.
- A missing, late or rate-limited pong means "no RTT sample" — never an error state, never a stale value shown as live, never an unbounded reading.
- Commit after every task. Conventional commits (`feat:`, `test:`, `docs:`).
- Every commit message ends with the line `Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr`.

## Deviation from the spec, already decided

The spec says "long-press on the video toggles it". A **single-finger** long-press is not implementable without a regression: `GestureInterpreter` arms the left press at `PRESS_ARM_MS = 60ms` and the controller sends it, so a one-finger hold would click the guest before any long-press threshold elapsed. Deferring the press to a long-press timeout would wreck input latency.

**Resolution, implemented in Task 7:** a **two-finger** long-press — both fingers still, held past `TAP_TIMEOUT_MS`, then lifted. `GestureState.TwoPointer` already tracks `startTimeMs` and `movedBeyondSlop`; that combination currently produces no effect at all (too slow to be the right-click tap, too still to be a pinch), so the gesture is free, and the second finger's arrival already cancels the left press so nothing reaches the guest.

---

### Task 1: Ping and Pong in the Rust protocol

**Files:**
- Modify: `crates/navette-protocol/src/media.rs:242-283` (`MediaInput`), `:286-315` (`validate`), `:334-338` (`MediaServerMessage`)
- Test: `crates/navette-protocol/src/media.rs` (`#[cfg(test)] mod tests`, same file)

**Interfaces:**
- Consumes: nothing.
- Produces: `MediaInput::Ping { nonce: u64 }` and `MediaServerMessage::Pong { nonce: u64 }`, serialising as `{"type":"ping","nonce":N}` and `{"type":"pong","nonce":N}`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `crates/navette-protocol/src/media.rs`:

```rust
#[test]
fn ping_round_trips_and_is_always_valid() {
    let ping = MediaInput::Ping { nonce: 42 };
    let encoded = serde_json::to_string(&ping).unwrap();
    assert_eq!(encoded, r#"{"type":"ping","nonce":42}"#);
    assert_eq!(serde_json::from_str::<MediaInput>(&encoded).unwrap(), ping);
    assert_eq!(ping.validate(), Ok(()));
}

#[test]
fn a_ping_nonce_has_no_invalid_value() {
    assert_eq!(MediaInput::Ping { nonce: 0 }.validate(), Ok(()));
    assert_eq!(MediaInput::Ping { nonce: u64::MAX }.validate(), Ok(()));
}

#[test]
fn pong_serialises_with_the_nonce_it_answers() {
    let encoded = serde_json::to_string(&MediaServerMessage::Pong { nonce: 7 }).unwrap();
    assert_eq!(encoded, r#"{"type":"pong","nonce":7}"#);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p navette-protocol ping`
Expected: FAIL — `no variant named Ping found for enum MediaInput`.

- [ ] **Step 3: Add the variants**

In `MediaInput`, after `RequestKeyframe`:

```rust
    RequestKeyframe,
    Ping {
        nonce: u64,
    },
```

In `MediaServerMessage`:

```rust
pub enum MediaServerMessage {
    Error { code: String, message: String },
    Pong { nonce: u64 },
}
```

`validate()` needs no new arm: its final `_ => Ok(())` already covers `Ping`, which is correct — there is no invalid nonce.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p navette-protocol`
Expected: PASS, with no other test regressed.

- [ ] **Step 5: Check formatting and lints**

Run: `cargo fmt --all -- --check && cargo clippy -p navette-protocol --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/navette-protocol/src/media.rs
git commit -m "$(cat <<'EOF'
feat(protocol): add media ping and pong for round-trip timing

A nonce echoed back by the server, so a client can measure its own
round trip without inferring one from frame arrivals. No MEDIA_VERSION
bump: an old daemon answers an unknown tag with invalid_input and keeps
the connection, which is the documented degradation path.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 2: `navetted` answers a ping from the socket task

**Files:**
- Modify: `crates/navetted/src/api.rs:181-185`
- Test: `crates/navetted/src/api.rs` (`mod tests`, same file)

**Interfaces:**
- Consumes: `MediaInput::Ping { nonce }`, `MediaServerMessage::Pong { nonce }` from Task 1.
- Produces: no new Rust API. A `ping` text frame is answered with a `pong` text frame and is **not** forwarded to the bridge as a `MediaCommand::Input`.

The ping is intercepted before `attachment.submit_input`. That is the whole point of the design: `MediaAttachment::recv` yields `Arc<MediaPacket>` only, so the socket task is the sole writer of server-to-client JSON, and answering here keeps the bridge loop untouched.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/navetted/src/api.rs`. It mirrors `media_websocket_replays_bootstrap_and_routes_validated_input`'s setup exactly:

```rust
#[tokio::test]
async fn media_websocket_answers_a_ping_without_troubling_the_bridge() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    add_running_session(&state, "work");
    let mut input = state.media.register_session("work");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    let mut request = format!("ws://{address}/v1/sessions/work/media")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        MEDIA_WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
    );
    let (mut socket, _response) = connect_async(request).await.unwrap();

    socket
        .send(ClientMessage::Text(r#"{"type":"ping","nonce":99}"#.into()))
        .await
        .unwrap();
    // Every wait here is bounded. An unbounded `socket.next()` for a reply
    // that does not exist yet hangs the red step instead of failing it, and
    // would hang all of CI on any future regression rather than reporting one.
    let pong = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for a pong -- the server did not answer the ping")
        .unwrap()
        .unwrap();
    assert_eq!(pong.to_text().unwrap(), r#"{"type":"pong","nonce":99}"#);

    // The bridge must never see a ping: it is answered at the socket, so a
    // stalled bridge loop cannot delay it -- and equally cannot be measured
    // by it. Sending a real input afterwards proves the channel still works
    // and that nothing from the ping is sitting ahead of it in the queue.
    socket
        .send(ClientMessage::Text(r#"{"type":"request_keyframe"}"#.into()))
        .await
        .unwrap();
    let forwarded = tokio::time::timeout(std::time::Duration::from_secs(5), input.recv())
        .await
        .expect("timed out waiting for the forwarded request_keyframe");
    assert_eq!(
        forwarded,
        Some(crate::media::MediaCommand::Input {
            attachment_id: 1,
            input: MediaInput::RequestKeyframe,
            queued_at: std::time::Instant::now()
        })
    );
    server.abort();
}
```

The setup mirrors `media_websocket_replays_bootstrap_and_routes_validated_input` exactly — `test_state` takes a `&TempDir`, and `add_running_session` is what makes the attach succeed — but deliberately publishes nothing, so there is no bootstrap replay to read past and the pong is the first frame the client sees. `TempDir` is already imported in this test module.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p navetted answers_a_ping -- --nocapture`
Expected: FAIL — the reply is `{"type":"error","code":"invalid_input",...}` before Task 1 is wired in here, or the assertion on `pong` fails.

- [ ] **Step 3: Intercept the ping**

Replace the match at `crates/navetted/src/api.rs:181-184`:

```rust
                            match serde_json::from_str::<MediaInput>(&text) {
                                Ok(input) => attachment.submit_input(input).err().map(media_hub_error),
                                Err(error) => Some(("invalid_input", format!("invalid input: {error}"))),
                            }
```

with:

```rust
                            match serde_json::from_str::<MediaInput>(&text) {
                                // Answered here rather than forwarded: the
                                // bridge has no JSON path back to a client
                                // (MediaAttachment::recv yields packets only),
                                // and a pong that queued behind the bridge
                                // loop would measure the loop, not the link.
                                Ok(MediaInput::Ping { nonce }) => {
                                    let pong = MediaServerMessage::Pong { nonce };
                                    let Ok(encoded) = serde_json::to_string(&pong) else {
                                        break;
                                    };
                                    if sender.send(Message::Text(encoded.into())).await.is_err() {
                                        break;
                                    }
                                    None
                                }
                                Ok(input) => attachment.submit_input(input).err().map(media_hub_error),
                                Err(error) => Some(("invalid_input", format!("invalid input: {error}"))),
                            }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p navetted`
Expected: PASS, including the pre-existing media socket tests.

- [ ] **Step 5: Check formatting and lints**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/navetted/src/api.rs
git commit -m "$(cat <<'EOF'
feat(navetted): answer a media ping from the socket task

Intercepted before submit_input, so a ping never reaches the bridge.
That is deliberate on both counts: the hub has no JSON path back to a
client, and a pong queued behind the bridge loop would report the
loop's latency rather than the link's.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 3: Ping and Pong in the Android protocol

**Files:**
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/net/MediaProtocol.kt:313-372` (`MediaInput`), `:391-416` (`validate`), `:418-424` (`MediaServerMessage`)
- Test: `android/app/src/test/kotlin/com/greponlabs/navette/net/MediaProtocolTest.kt`

**Interfaces:**
- Consumes: the wire shapes from Task 1.
- Produces: `MediaInput.Ping(nonce: ULong)` and `MediaServerMessage.Pong(nonce: ULong)`.

- [ ] **Step 1: Write the failing tests**

Add to `MediaProtocolTest.kt`:

```kotlin
@Test
fun `ping serialises exactly as the rust protocol expects`() {
    val encoded = mediaJson.encodeToString(MediaInput.serializer(), MediaInput.Ping(42uL))
    assertEquals("""{"type":"ping","nonce":42}""", encoded)
}

@Test
fun `a ping is always valid whatever its nonce`() {
    assertNull(MediaInput.Ping(0uL).validate())
    assertNull(MediaInput.Ping(ULong.MAX_VALUE).validate())
}

@Test
fun `pong parses from what navetted sends`() {
    val decoded = mediaJson.decodeFromString(MediaServerMessage.serializer(), """{"type":"pong","nonce":7}""")
    assertEquals(MediaServerMessage.Pong(7uL), decoded)
}

@Test
fun `an unknown server message is still rejected rather than guessed at`() {
    // MediaClient.onMessage relies on this failing, not throwing past its
    // runCatching -- it is what makes an old daemon's unknown reply a log
    // line instead of a crash.
    assertThrows(SerializationException::class.java) {
        mediaJson.decodeFromString(MediaServerMessage.serializer(), """{"type":"nonsense"}""")
    }
}
```

**Assertions are `org.junit.Assert`, never `kotlin.test`.** All twelve existing
test files in this module use JUnit's assertions and `kotlin.test` is not a
declared `testImplementation` dependency. Imports to add: `org.junit.Assert.assertNull`,
`org.junit.Assert.assertThrows`, and `kotlinx.serialization.SerializationException`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd android && ./gradlew testDebugUnitTest --tests '*MediaProtocolTest*'`
Expected: FAIL — `Unresolved reference: Ping`.

- [ ] **Step 3: Add the variants**

In `MediaInput`, after `RequestKeyframe`:

```kotlin
    @Serializable
    @SerialName("ping")
    data class Ping(val nonce: ULong) : MediaInput
```

In `MediaServerMessage`:

```kotlin
    @Serializable
    @SerialName("pong")
    data class Pong(val nonce: ULong) : MediaServerMessage
```

In `validate()`, add the arm the exhaustive `when` now demands, beside `MediaInput.RequestKeyframe -> null`:

```kotlin
        is MediaInput.Ping -> null
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd android && ./gradlew testDebugUnitTest --tests '*MediaProtocolTest*'`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/net/MediaProtocol.kt android/app/src/test/kotlin/com/greponlabs/navette/net/MediaProtocolTest.kt
git commit -m "$(cat <<'EOF'
feat(android): add ping and pong to the media protocol

ULong against the wire's u64, matching every other id on this protocol.
The unknown-server-message test pins the behaviour MediaClient.onMessage
depends on: an unrecognised reply must fail deserialisation so the
runCatching there turns it into a log line, not a crash.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 4: `SessionHud` — the metric arithmetic

**Files:**
- Create: `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionHud.kt`
- Test: `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/SessionHudTest.kt`

**Interfaces:**
- Consumes: `MediaPacket`, `MediaKind`, `MediaHeader` from `net/MediaProtocol.kt`.
- Produces:
  - `data class HudSample(val fps: Double, val bitrateBps: Double, val decodeMs: Double?, val ageMs: Long?, val rttMs: Long?, val droppedPackets: Long, val discontinuities: Long)`
  - `class SessionHud` with `fun recordPacket(nowMs: Long, packet: MediaPacket)`, `fun recordFed(timestampUs: Long, nowMs: Long)`, `fun recordPresented(timestampUs: Long, nowMs: Long)`, `fun recordPing(nonce: ULong, nowMs: Long)`, `fun recordPong(nonce: ULong, nowMs: Long)`, `fun sample(nowMs: Long): HudSample`
  - `fun HudSample.format(): String`

Time is always a parameter, never read from a clock — the same discipline as `hud.rs`, and the reason this is testable without Android.

- [ ] **Step 1: Write the failing tests**

Create `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/SessionHudTest.kt`:

```kotlin
package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.MediaFlags
import com.greponlabs.navette.net.MediaHeader
import com.greponlabs.navette.net.MediaKind
import com.greponlabs.navette.net.MediaPacket
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SessionHudTest {
    private fun packet(
        kind: MediaKind = MediaKind.VIDEO,
        sequence: Long,
        payload: Int = 0,
        discontinuity: Boolean = false,
    ) = MediaPacket(
        MediaHeader(
            kind = kind,
            flags = MediaFlags.of(keyframe = false, discontinuity = discontinuity),
            streamId = 1,
            sequence = sequence,
            timestampUs = 0,
            payloadLen = payload,
            width = 1920,
            height = 1080,
        ),
        ByteArray(payload),
    )

    @Test
    fun `frames outside the one-second window stop counting`() {
        val hud = SessionHud()
        repeat(10) { hud.recordPresented(timestampUs = it.toLong(), nowMs = 1000L + it * 100L) }
        // Ten frames spread over 900ms, sampled at the last one.
        assertTrue(hud.sample(1900L).fps > 9.0, "expected ~10fps, got ${hud.sample(1900L).fps}")
        // Three seconds later every one has aged out.
        assertEquals(0.0, hud.sample(4900L).fps)
    }

    @Test
    fun `bitrate counts only video payload inside the window`() {
        val hud = SessionHud()
        hud.recordPacket(1000L, packet(sequence = 1, payload = 1000))
        hud.recordPacket(1500L, packet(kind = MediaKind.METRICS, sequence = 2, payload = 9_000_000))
        // 1000 bytes = 8000 bits. Sampled at exactly 1000ms after the only
        // video packet, so the rate is over a full second and the expected
        // value is exact: rates divide by elapsed-since-oldest, as hud.rs's
        // own rate() does, not by the nominal window.
        assertEquals(8000.0, hud.sample(2000L).bitrateBps, 1.0)
    }

    @Test
    fun `the first three packets only establish a sequence baseline`() {
        val hud = SessionHud()
        // Mirrors the attach replay: a config and a keyframe at their original
        // sequence numbers, then live traffic resuming much later. That jump is
        // history this client was never sent, not a drop.
        hud.recordPacket(1000L, packet(kind = MediaKind.STREAM_CONFIG, sequence = 2))
        hud.recordPacket(1001L, packet(sequence = 3))
        hud.recordPacket(1002L, packet(sequence = 900))
        assertEquals(0L, hud.sample(1002L).droppedPackets)
    }

    @Test
    fun `a sequence gap after the baseline counts as dropped packets`() {
        val hud = SessionHud()
        listOf(1L, 2L, 3L, 4L).forEach { hud.recordPacket(1000L, packet(sequence = it)) }
        hud.recordPacket(1001L, packet(sequence = 8))
        // 5, 6 and 7 never arrived.
        assertEquals(3L, hud.sample(1001L).droppedPackets)
    }

    @Test
    fun `a repeated sequence is a replay, not a drop and not a rewind`() {
        val hud = SessionHud()
        listOf(1L, 2L, 3L, 4L, 5L).forEach { hud.recordPacket(1000L, packet(sequence = it)) }
        hud.recordPacket(1001L, packet(sequence = 3))
        hud.recordPacket(1002L, packet(sequence = 6))
        assertEquals(0L, hud.sample(1002L).droppedPackets)
    }

    @Test
    fun `discontinuity flags accumulate`() {
        val hud = SessionHud()
        hud.recordPacket(1000L, packet(sequence = 1, discontinuity = true))
        hud.recordPacket(1001L, packet(sequence = 2))
        hud.recordPacket(1002L, packet(sequence = 3, discontinuity = true))
        assertEquals(2L, hud.sample(1002L).discontinuities)
    }

    @Test
    fun `decode time is feed to presentation of the same access unit`() {
        val hud = SessionHud()
        hud.recordFed(timestampUs = 500L, nowMs = 1000L)
        hud.recordPresented(timestampUs = 500L, nowMs = 1012L)
        assertEquals(12.0, hud.sample(1012L).decodeMs)
    }

    @Test
    fun `a presentation with no matching feed leaves decode time alone`() {
        val hud = SessionHud()
        hud.recordFed(timestampUs = 500L, nowMs = 1000L)
        hud.recordPresented(timestampUs = 500L, nowMs = 1012L)
        hud.recordPresented(timestampUs = 999L, nowMs = 1030L)
        assertEquals(12.0, hud.sample(1030L).decodeMs, "an unmatched presentation must not overwrite a real reading")
    }

    @Test
    fun `frame age grows until the next frame arrives`() {
        val hud = SessionHud()
        hud.recordPresented(timestampUs = 1L, nowMs = 1000L)
        assertEquals(500L, hud.sample(1500L).ageMs)
        hud.recordPresented(timestampUs = 2L, nowMs = 1600L)
        assertEquals(0L, hud.sample(1600L).ageMs)
    }

    @Test
    fun `age and decode time are null before anything has been presented`() {
        val hud = SessionHud()
        assertNull(hud.sample(1000L).ageMs)
        assertNull(hud.sample(1000L).decodeMs)
    }

    @Test
    fun `rtt is the round trip of a matched nonce`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPong(nonce = 1uL, nowMs = 1043L)
        assertEquals(43L, hud.sample(1043L).rttMs)
    }

    @Test
    fun `a pong for a superseded nonce is discarded, not misattributed`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPing(nonce = 2uL, nowMs = 2000L)
        // The first ping's answer finally turns up after its successor went out.
        hud.recordPong(nonce = 1uL, nowMs = 2100L)
        assertNull(hud.sample(2100L).rttMs, "a stale nonce must not be timed against the live ping")
        hud.recordPong(nonce = 2uL, nowMs = 2110L)
        assertEquals(110L, hud.sample(2110L).rttMs)
    }

    @Test
    fun `an unanswered ping leaves rtt blank rather than growing without bound`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        assertNull(hud.sample(60_000L).rttMs)
    }

    @Test
    fun `a stale rtt reading expires rather than being shown as live`() {
        val hud = SessionHud()
        hud.recordPing(nonce = 1uL, nowMs = 1000L)
        hud.recordPong(nonce = 1uL, nowMs = 1020L)
        assertEquals(20L, hud.sample(1020L).rttMs)
        // Five seconds on with nothing answered, the last reading is history.
        assertNull(hud.sample(6100L).rttMs)
    }

    @Test
    fun `the formatted line shows every field and blanks what it lacks`() {
        val sample = HudSample(
            fps = 29.94,
            bitrateBps = 2_500_000.0,
            decodeMs = 4.2,
            ageMs = 33L,
            rttMs = null,
            droppedPackets = 3,
            discontinuities = 1,
        )
        assertEquals("FPS 29.9  KBPS 2500  DEC 4.2MS  AGE 33MS  RTT --  DROP 3  DISC 1", sample.format())
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd android && ./gradlew testDebugUnitTest --tests '*SessionHudTest*'`
Expected: FAIL — `Unresolved reference: SessionHud`.

If `MediaPacket`'s constructor or `MediaFlags.of` does not match the shape used in the `packet()` helper, fix the helper to match `net/MediaProtocol.kt` — do not change the protocol to suit the test.

- [ ] **Step 3: Write the implementation**

Create `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionHud.kt`:

```kotlin
package com.greponlabs.navette.ui.session

import com.greponlabs.navette.net.MediaKind
import com.greponlabs.navette.net.MediaPacket
import java.util.Locale

/** Rolling window every rate in a [HudSample] is measured over. */
const val HUD_WINDOW_MS: Long = 1000L

/**
 * How long a round-trip reading stays on screen before it is treated as
 * history. Pings go out once a second, so anything older than this means
 * several went unanswered and the number no longer describes the link.
 */
const val RTT_STALE_MS: Long = 5000L

/**
 * How many of a stream's first packets only establish the sequence baseline
 * instead of being audited for gaps.
 *
 * A direct port of `hud.rs`'s `BASELINE_PACKETS`, for the same reason:
 * attaching replays up to two packets per stream -- the stream's latest
 * configuration and its latest keyframe -- at the sequence numbers they were
 * originally published with, and live traffic then resumes from wherever the
 * stream has actually got to. That jump is history this client was never
 * sent, not a drop. Three covers the replay plus the first live packet.
 */
private const val BASELINE_PACKETS: Long = 3

/**
 * How many fed access units are remembered while waiting to be presented.
 *
 * Bounded because a decoder that stops presenting must not grow this without
 * limit. Small: the interval being measured is a handful of milliseconds, so
 * anything still unmatched after this many newer units is not going to be.
 */
private const val PENDING_FEEDS = 16

/** One stream's performance figures at a point in time. Nulls mean "no reading", never zero. */
data class HudSample(
    val fps: Double,
    val bitrateBps: Double,
    val decodeMs: Double?,
    val ageMs: Long?,
    val rttMs: Long?,
    val droppedPackets: Long,
    val discontinuities: Long,
)

/**
 * A compact single line, short enough to sit over a phone-sized picture.
 *
 * `--` rather than `0` wherever there is no reading: a zero round trip and an
 * unanswered ping are very different things, and the whole point of this
 * overlay is telling them apart.
 */
fun HudSample.format(): String {
    // Locale.ROOT throughout: the default locale renders a comma decimal
    // separator across much of the world, which would put "29,9" on the
    // overlay and make this function's output depend on the phone's region.
    val dec = decodeMs?.let { String.format(Locale.ROOT, "%.1fMS", it) } ?: "--"
    val age = ageMs?.let { "${it}MS" } ?: "--"
    val rtt = rttMs?.let { "${it}MS" } ?: "--"
    val rate = String.format(Locale.ROOT, "%.1f", fps)
    val kbps = String.format(Locale.ROOT, "%.0f", bitrateBps / 1000.0)
    return "FPS $rate  KBPS $kbps  DEC $dec  AGE $age  RTT $rtt  " +
        "DROP $droppedPackets  DISC $discontinuities"
}

/**
 * Accumulates one session's HUD inputs.
 *
 * Every figure is reconstructed from what the client already receives --
 * packet headers, decoder callbacks, and the pong answering a ping this class
 * was told about. Nothing here reads a clock: time arrives as a parameter, the
 * same discipline `crates/navette-viewer/src/hud.rs` keeps, so the rolling
 * windows are asserted against a scripted clock in a plain JVM test.
 *
 * **Not thread-safe.** The caller confines it to one thread, exactly as
 * `SessionController` confines its gesture state.
 */
class SessionHud {
    private val frames = ArrayDeque<Long>()
    private val videoBytes = ArrayDeque<Pair<Long, Int>>()
    private val pendingFeeds = ArrayDeque<Pair<Long, Long>>()

    private var lastSequence: Long? = null
    private var packets: Long = 0
    private var droppedPackets: Long = 0
    private var discontinuities: Long = 0

    private var decodeMs: Double? = null
    private var lastFrameAtMs: Long? = null

    private var pendingPing: Pair<ULong, Long>? = null
    private var rtt: Pair<Long, Long>? = null

    /** Records one packet's wire cost, sequence position and discontinuity flag. */
    fun recordPacket(nowMs: Long, packet: MediaPacket) {
        noteSequence(packet.header.sequence)
        if (packet.header.flags.discontinuity) discontinuities++
        if (packet.header.kind == MediaKind.VIDEO) {
            videoBytes.addLast(nowMs to packet.payload.size)
            trimBytes(nowMs)
        }
    }

    /** Records that an access unit went into the decoder. */
    fun recordFed(timestampUs: Long, nowMs: Long) {
        pendingFeeds.addLast(timestampUs to nowMs)
        while (pendingFeeds.size > PENDING_FEEDS) pendingFeeds.removeFirst()
    }

    /** Records that a decoded picture reached the surface. */
    fun recordPresented(timestampUs: Long, nowMs: Long) {
        frames.addLast(nowMs)
        trimFrames(nowMs)
        lastFrameAtMs = nowMs
        val fed = pendingFeeds.firstOrNull { it.first == timestampUs } ?: return
        pendingFeeds.remove(fed)
        decodeMs = (nowMs - fed.second).toDouble()
    }

    /** Records that a ping went out. Any earlier unanswered ping is abandoned. */
    fun recordPing(nonce: ULong, nowMs: Long) {
        pendingPing = nonce to nowMs
    }

    /** Times a pong against the ping it answers, ignoring any it does not. */
    fun recordPong(nonce: ULong, nowMs: Long) {
        val (pending, sentAt) = pendingPing ?: return
        if (pending != nonce) return
        pendingPing = null
        rtt = (nowMs - sentAt) to nowMs
    }

    /** Computes the current figures, discarding whatever has aged out. */
    fun sample(nowMs: Long): HudSample {
        trimFrames(nowMs)
        trimBytes(nowMs)
        val bits = videoBytes.sumOf { it.second.toDouble() * 8.0 }
        val liveRtt = rtt?.takeIf { nowMs - it.second <= RTT_STALE_MS }?.first
        return HudSample(
            fps = rate(frames.size.toDouble(), frames.firstOrNull(), nowMs),
            bitrateBps = rate(bits, videoBytes.firstOrNull()?.first, nowMs),
            decodeMs = decodeMs,
            ageMs = lastFrameAtMs?.let { nowMs - it },
            rttMs = liveRtt,
            droppedPackets = droppedPackets,
            discontinuities = discontinuities,
        )
    }

    /**
     * Counts packets the server never delivered.
     *
     * A sequence that does not advance is the hub replaying an older packet:
     * neither a drop nor a reason to rewind the baseline.
     */
    private fun noteSequence(sequence: Long) {
        val observed = packets
        packets++
        val last = lastSequence
        if (last == null) {
            lastSequence = sequence
            return
        }
        if (sequence <= last) return
        if (observed >= BASELINE_PACKETS) droppedPackets += sequence - last - 1
        lastSequence = sequence
    }

    private fun trimFrames(nowMs: Long) {
        while (frames.isNotEmpty() && nowMs - frames.first() > HUD_WINDOW_MS) frames.removeFirst()
    }

    private fun trimBytes(nowMs: Long) {
        while (videoBytes.isNotEmpty() && nowMs - videoBytes.first().first > HUD_WINDOW_MS) videoBytes.removeFirst()
    }

    /**
     * A rate over the window actually observed, not the nominal one: a stream
     * half a second old must not report half its true rate.
     */
    private fun rate(total: Double, oldestMs: Long?, nowMs: Long): Double {
        if (oldestMs == null || total == 0.0) return 0.0
        val elapsed = nowMs - oldestMs
        // Zero or negative span yields no rate at all, exactly as hud.rs:174-176
        // does. Flooring the divisor at 1ms instead would turn a frame presented
        // and sampled in the same millisecond -- routine at currentTimeMillis
        // granularity -- into "FPS 1000.0" on the overlay.
        if (elapsed <= 0L) return 0.0
        return total * 1000.0 / elapsed.toDouble()
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd android && ./gradlew testDebugUnitTest --tests '*SessionHudTest*'`
Expected: PASS, all fifteen.

If `frames outside the one-second window stop counting` disagrees on the exact fps, adjust the assertion's tolerance — not the windowing — and confirm the `rate` denominator matches `hud.rs`'s `rate()`.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionHud.kt android/app/src/test/kotlin/com/greponlabs/navette/ui/session/SessionHudTest.kt
git commit -m "$(cat <<'EOF'
feat(android): compute session HUD metrics

A port of hud.rs's arithmetic, including its one-second window and its
three-packet sequence baseline, so FPS, KBPS, DROP and DISC mean the
same thing on both clients. Time is a parameter throughout, which is
what lets every window be asserted against a scripted clock with no
Android in the test.

Nulls rather than zeros for the readings that can be absent: an
unanswered ping and a zero round trip are different facts, and telling
them apart is the point of the overlay.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 5: The decoder reports presented frames

**Files:**
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/media/H264Decoder.kt:11-27` (`DecoderEvent`), `:291-317` (`onOutputBufferAvailable`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `DecoderEvent.Presented(val timestampUs: Long)`, raised after a buffer is successfully rendered.

There is no `H264DecoderTest` — this class needs a real `MediaCodec` and cannot run on the JVM. That is pre-existing and is not fixed here. The new event is covered indirectly by `SessionHudTest` (which owns the arithmetic) and directly by the on-device check in Task 9. Say so rather than implying coverage that does not exist.

- [ ] **Step 1: Add the event**

In the `DecoderEvent` sealed interface:

```kotlin
    /** A decoded picture reached the surface. Carries the access unit's own timestamp so a HUD can pair it with the feed. */
    data class Presented(val timestampUs: Long) : DecoderEvent
```

- [ ] **Step 2: Raise it after a successful render**

`onOutputBufferAvailable` currently captures `sizeAwaitingPresentation` under the lock and raises `Configured` outside it. Add the presentation event on the same path, outside the lock for the same reason — no call out to a listener while holding it:

```kotlin
                if (size != null) onEvent(DecoderEvent.Configured(size.first, size.second))
                onEvent(DecoderEvent.Presented(info.presentationTimeUs))
```

Both `return` paths inside the `synchronized` block (wrong codec identity, and a failed `releaseOutputBuffer`) leave without raising it, which is correct: nothing reached the surface.

- [ ] **Step 3: Verify the exhaustive `when` compiles**

`SessionController.onDecoderEvent` matches on `DecoderEvent`. Adding a variant makes that `when` non-exhaustive.

Run: `cd android && ./gradlew compileDebugKotlin`
Expected: FAIL, naming `onDecoderEvent`'s `when` — this is the compiler proving the new event has to be handled. Task 6 handles it. To keep this task independently committable, add the branch now as a no-op that Task 6 fills in:

```kotlin
            is DecoderEvent.Presented -> Unit
```

- [ ] **Step 4: Run the build and the suite**

Run: `cd android && ./gradlew compileDebugKotlin testDebugUnitTest`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/media/H264Decoder.kt android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt
git commit -m "$(cat <<'EOF'
feat(android): report presented frames from the decoder

Carries the access unit's presentation timestamp so a consumer can pair
it with the feed that produced it. Raised outside the codec lock, on the
one path where a buffer actually reached the surface -- a wrong-identity
or failed render stays silent, because nothing was shown.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 6: Wire the HUD into the session controller

**Files:**
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/net/MediaClient.kt:354-361` (`onMessage`), and near `:314` (`requestKeyframe`) for the ping sender
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt:78-88` (`SessionUiState`), `:454-483` (`open`), `:485-499` (`close`), `:501-509` (`route`), `:565-592` (`onDecoderEvent`)

**Interfaces:**
- Consumes: `SessionHud`, `HudSample` (Task 4); `DecoderEvent.Presented` (Task 5); `MediaInput.Ping`, `MediaServerMessage.Pong` (Task 3).
- Produces: `SessionUiState.hud: HudSample?`, populated once per second while attached.

- [ ] **Step 1: Give `MediaClient` a pong listener and a ping sender**

`MediaClient` currently only logs server text. Add a settable listener and use it, keeping the existing "never take the connection down" behaviour exactly as it is:

```kotlin
    /**
     * Told about each pong, so a caller can time it against its own ping.
     * Set before [connect]; called on OkHttp's reader thread.
     */
    @Volatile
    var onPong: ((ULong) -> Unit)? = null
```

and in `onMessage(webSocket: WebSocket, text: String)`:

```kotlin
            override fun onMessage(webSocket: WebSocket, text: String) {
                // The bridge reports protocol problems as JSON text; they are
                // informational and must not take the connection down. An
                // unparseable body includes an old daemon's reply to a message
                // it does not know -- a ping, for one -- which is exactly why
                // this logs rather than fails.
                val reported =
                    runCatching { mediaJson.decodeFromString(MediaServerMessage.serializer(), text) }
                        .getOrNull()
                when (reported) {
                    is MediaServerMessage.Pong -> onPong?.invoke(reported.nonce)
                    is MediaServerMessage.Error, null -> Log.w(TAG, "media server reported: ${reported ?: text}")
                }
            }
```

Add a ping sender beside `requestKeyframe`:

```kotlin
    /** Sends one ping. The caller owns the nonce, so it can time the answer. */
    fun sendPing(nonce: ULong) = sendInput(MediaInput.Ping(nonce))
```

- [ ] **Step 2: Add the HUD to the UI state**

In `SessionUiState`:

```kotlin
    /** The latest metrics sample, or `null` before the first one. */
    val hud: HudSample? = null,
```

- [ ] **Step 3: Hold a `SessionHud` in the controller and feed it**

In `SessionController`, beside the other private state:

```kotlin
    // Confined to the packet loop's dispatcher and the decoder callback, the
    // same two writers `decoder` already has -- so it takes `lock` for the
    // same reason and in the same order.
    private val hud = SessionHud()
    private var hudJob: Job? = null
    private var pingNonce: ULong = 0uL
```

In `route(packet)`, record every packet before the gate sees it — the gate discards packets for streams it is not showing, but a drop is a drop and must still count:

```kotlin
    private fun route(packet: MediaPacket) {
        synchronized(lock) { hud.recordPacket(System.currentTimeMillis(), packet) }
        when (val event = gate.handle(packet)) {
            is StreamGateEvent.Bootstrap -> startDecoder(event.stream)
            is StreamGateEvent.Reconfigure -> startDecoder(event.stream)
            is StreamGateEvent.Video -> {
                synchronized(lock) { hud.recordFed(event.timestampUs, System.currentTimeMillis()) }
                synchronized(lock) { decoder }?.feed(event.accessUnit, event.timestampUs)
            }
            StreamGateEvent.Ended -> _state.update { it.copy(streamEnded = true) }
            null -> Unit
        }
    }
```

In `onDecoderEvent`, replace the `is DecoderEvent.Presented -> Unit` placeholder from Task 5:

```kotlin
            is DecoderEvent.Presented ->
                synchronized(lock) { hud.recordPresented(event.timestampUs, System.currentTimeMillis()) }
```

- [ ] **Step 4: Drive the ping and publish a sample once a second**

Install the pong listener **before** `client.connect()` — `connect()` starts OkHttp's reader thread, so the listener must already be in place:

```kotlin
        client.onPong = { nonce -> synchronized(lock) { hud.recordPong(nonce, System.currentTimeMillis()) } }
```

Then, after the connection collector is launched:

```kotlin
        hudJob?.cancel()
        hudJob =
            scope.launch {
                while (true) {
                    val nonce = ++pingNonce
                    synchronized(lock) { hud.recordPing(nonce, System.currentTimeMillis()) }
                    client.sendPing(nonce)
                    // Sampling after the ping rather than before means the
                    // reading on screen is at most one interval behind the
                    // link, not two.
                    delay(HUD_SAMPLE_INTERVAL_MS)
                    val sample = synchronized(lock) { hud.sample(System.currentTimeMillis()) }
                    _state.update { it.copy(hud = sample) }
                }
            }
```

with, beside the file's other constants:

```kotlin
/** How often the HUD pings and republishes. One second, matching [HUD_WINDOW_MS]. */
private const val HUD_SAMPLE_INTERVAL_MS: Long = 1000L
```

In `close()`, alongside the other job cancellations:

```kotlin
        hudJob?.cancel()
        client.onPong = null
```

- [ ] **Step 5: Verify the whole app still builds and every test passes**

Run: `cd android && ./gradlew compileDebugKotlin testDebugUnitTest`
Expected: PASS. `SessionScreen.kt` grows by roughly 25 lines; confirm with `wc -l` that it has not crossed a further threshold and note the number in the commit.

- [ ] **Step 6: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/net/MediaClient.kt android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt
git commit -m "$(cat <<'EOF'
feat(android): feed the session HUD from the live stream

Packets are recorded before the gate sees them: the gate discards
packets for streams it is not showing, but a gap in the sequence is a
drop whether or not the picture was wanted.

The ping goes out at the top of each interval and the sample is taken
at the bottom, so the round trip on screen is at most one interval
behind the link rather than two. onPong is cleared in close(), because
OkHttp's reader thread outlives the controller that set it.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 7: The two-finger long-press toggle

**Files:**
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/GestureInterpreter.kt` (`GestureEffect`, `rightClickIfTap`'s caller at `:181-187`)
- Test: `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/GestureInterpreterTest.kt`

**Interfaces:**
- Consumes: existing `GestureState.TwoPointer` fields `startTimeMs` and `movedBeyondSlop`.
- Produces: `GestureEffect.ToggleHud`.

See "Deviation from the spec" above for why this is two-fingered. A two-finger touch that is still but too slow to be a tap currently produces no effect at all, so nothing is displaced.

- [ ] **Step 1: Write the failing tests**

Add to `GestureInterpreterTest.kt`, using the file's own `Fingers` DSL — `twoFingersDown`, `after`, `move`, `pointerUp`, `drain`, `p`, and the `FINGER_A`/`FINGER_B` constants. All 32 existing gesture tests use it; do not hand-roll `TouchEvent` construction.

Only two tests are added. A third — "a quick two-finger tap is still a right-click, not a HUD toggle" — would be redundant: the existing `a quick still two-finger tap right-clicks at the first finger` (:262) asserts the **exact** effect list `[Motion, RightClick]`, which already fails if a toggle appears.

```kotlin
@Test
fun `two still fingers held past the tap timeout toggle the hud`() {
    val fingers = twoFingersDown(zoomed = false)

    fingers.after(TAP_TIMEOUT_MS + 1).pointerUp(FINGER_B, p(FINGER_A, 100f, 100f), p(FINGER_B, 300f, 100f))

    assertEquals(listOf<GestureEffect>(GestureEffect.ToggleHud), fingers.drain())
    assertSame(GestureState.Suppressed, fingers.state)
}

@Test
fun `fingers that moved do not toggle the hud however long they were down`() {
    val fingers = twoFingersDown(zoomed = false)
    fingers.move(p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))
    fingers.drain()

    fingers.after(TAP_TIMEOUT_MS + 1).pointerUp(FINGER_A, p(FINGER_A, 150f, 130f), p(FINGER_B, 350f, 130f))

    assertEquals(emptyList<GestureEffect>(), fingers.drain())
}
```

**The existing test at :272 is the boundary guard.** `a slow two-finger touch is
not a right-click` holds for *exactly* `TAP_TIMEOUT_MS` and asserts no effects.
The new condition is therefore strictly `held > TAP_TIMEOUT_MS` — strict on both
sides, leaving the boundary instant inert. Do not "tidy" it to `>=`: that would
silently rewrite a real existing assertion.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd android && ./gradlew testDebugUnitTest --tests '*GestureInterpreterTest*'`
Expected: FAIL — `Unresolved reference: ToggleHud`.

- [ ] **Step 3: Add the effect and the transition**

In `GestureEffect`:

```kotlin
    /**
     * Show or hide the performance HUD. Two still fingers held past
     * [TAP_TIMEOUT_MS]: too slow to be the right-click tap, too still to be a
     * pinch, so this gesture was previously inert. It cannot be a one-finger
     * long-press because the left press arms at [PRESS_ARM_MS] and would have
     * clicked the guest long before any hold threshold elapsed.
     */
    data object ToggleHud : GestureEffect
```

In `stepTwoPointer`'s `TouchAction.PointerUp` branch, replace `rightClickIfTap(state, event)` with a helper that decides between the three outcomes:

```kotlin
    /**
     * A two-finger lift is one of three things: a right-click (quick, still),
     * a HUD toggle (slow, still), or nothing at all (moved).
     */
    private fun twoPointerLift(state: GestureState.TwoPointer, event: TouchEvent): List<GestureEffect> {
        if (state.movedBeyondSlop || state.pinching) return emptyList()
        val held = event.eventTimeMs - state.startTimeMs
        if (held > TAP_TIMEOUT_MS) return listOf(GestureEffect.ToggleHud)
        return rightClickIfTap(state, event)
    }
```

and call it:

```kotlin
                    GestureStep(GestureState.Suppressed, twoPointerLift(state, event))
```

Keep `rightClickIfTap` as it is — it still owns the right-click's own conditions, and `twoPointerLift` only decides which question to ask.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd android && ./gradlew testDebugUnitTest --tests '*GestureInterpreterTest*'`
Expected: PASS, including every pre-existing right-click and pinch test. If a pre-existing test now fails, `twoPointerLift`'s guard is wrong — fix the guard, not the old test.

- [ ] **Step 5: Handle the effect in the controller**

`SessionController.applyEffect` matches exhaustively on `GestureEffect`, so this will not compile until handled. In `SessionScreen.kt`, add a toggle callback to the controller and route the effect to it:

```kotlin
    /** Set by the screen, which owns whether the HUD is showing. */
    var onToggleHud: (() -> Unit)? = null
```

and in `applyEffect`:

```kotlin
            GestureEffect.ToggleHud -> onToggleHud?.invoke()
```

- [ ] **Step 6: Run the whole suite**

Run: `cd android && ./gradlew compileDebugKotlin testDebugUnitTest`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/ui/session/GestureInterpreter.kt android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt android/app/src/test/kotlin/com/greponlabs/navette/ui/session/GestureInterpreterTest.kt
git commit -m "$(cat <<'EOF'
feat(android): toggle the HUD with a two-finger long-press

Two fingers, still, held past TAP_TIMEOUT_MS. That combination produced
no effect at all before -- too slow for the right-click tap, too still
for a pinch -- so nothing is displaced.

Not the one-finger long-press the design asked for, and deliberately:
the left press arms at PRESS_ARM_MS and is sent, so a one-finger hold
would click the guest before any hold threshold could elapse. Deferring
the press instead would cost input latency on every tap to buy a debug
affordance.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 8: The overlay

**Files:**
- Create: `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionHudOverlay.kt`
- Modify: `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt` (the `Box` that already hosts `SessionOverlay`, and the state hoisting beside `imeRaised`)

**Interfaces:**
- Consumes: `HudSample` and `format()` (Task 4); `SessionController.onToggleHud` (Task 7).
- Produces: `@Composable internal fun SessionHudOverlay(sample: HudSample?, visible: Boolean)`.

- [ ] **Step 1: Write the composable**

Create `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionHudOverlay.kt`:

```kotlin
package com.greponlabs.navette.ui.session

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * The performance HUD, drawn over the video.
 *
 * Renders a computed [HudSample] and calculates nothing -- every figure comes
 * from [SessionHud], which is where they can be tested. Monospaced, because a
 * proportional font makes a number that changes every second jitter sideways
 * and become much harder to read at a glance.
 *
 * Top-aligned: the bottom of this screen is where the IME and the gesture
 * surface live, and a HUD there would sit under the on-screen keyboard.
 */
@Composable
internal fun SessionHudOverlay(sample: HudSample?, visible: Boolean) {
    if (!visible || sample == null) return
    Box(modifier = Modifier.fillMaxWidth(), contentAlignment = Alignment.TopCenter) {
        Text(
            text = sample.format(),
            color = Color.White,
            fontFamily = FontFamily.Monospace,
            fontSize = 11.sp,
            modifier =
                Modifier
                    // Not transparent: this sits over live video, and white on
                    // a bright guest window is unreadable without a ground.
                    .background(Color.Black.copy(alpha = 0.6f))
                    .padding(horizontal = 8.dp, vertical = 4.dp),
        )
    }
}
```

- [ ] **Step 2: Hoist the visibility state and render it**

In `SessionScreen`, beside where `imeRaised` is hoisted — and keyed the same way, so a reconnect rebuild does not reset it:

```kotlin
    var hudVisible by rememberSaveable(host, sessionName) { mutableStateOf(false) }
```

Wire the controller's callback in the same `DisposableEffect`/`LaunchedEffect` that already binds the controller:

```kotlin
    LaunchedEffect(controller) { controller.onToggleHud = { hudVisible = !hudVisible } }
```

and render it inside the same `Box` that hosts `SessionOverlay`, **after** the `AndroidView` so it draws on top, and **before** `SessionOverlay` so a connection overlay is never hidden behind metrics:

```kotlin
        SessionHudOverlay(sample = state.hud, visible = hudVisible)
```

Match the file's existing imports for `rememberSaveable`, `mutableStateOf`, `getValue` and `setValue`; `imeRaised` is the model to copy.

- [ ] **Step 3: Build and run the suite**

Run: `cd android && ./gradlew compileDebugKotlin testDebugUnitTest`
Expected: PASS.

- [ ] **Step 4: Check the file sizes still obey the ceiling**

Run: `wc -l android/app/src/main/kotlin/com/greponlabs/navette/ui/session/*.kt`
Expected: the two new files well under 200 lines each; `SessionScreen.kt` grown by roughly 5 lines from Task 6's figure. If `SessionScreen.kt` has grown materially, extract rather than accept it.

- [ ] **Step 5: Commit**

```bash
git add android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionHudOverlay.kt android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt
git commit -m "$(cat <<'EOF'
feat(android): draw the performance HUD over the session

Renders a computed sample and calculates nothing. Monospaced so figures
that change every second do not jitter sideways, on a translucent ground
because white text over a bright guest window is otherwise unreadable.

Visibility is hoisted and keyed like imeRaised, so a reconnect rebuild
does not silently turn the HUD off while someone is watching it.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

### Task 9: Documentation and on-device verification

**Files:**
- Modify: `android/README.md` (its Verified / Not-verified / Known-limitations split)
- Modify: `docs/HANDOFF.md` (append a section)

- [ ] **Step 1: Run the full suite on both sides**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Run: `cd android && ./gradlew testDebugUnitTest`
Expected: all green. Record the Kotlin test count — it was 166 before this work.

- [ ] **Step 2: Build and install on the Pixel 10 Pro Fold**

Per `docs/HANDOFF.md`, debug builds need `debuggableVariants=[]` and a cleared Metro cache is *not* relevant here (that note is for RN projects) — follow `android/README.md`'s own build instructions.

Run: `cd android && ./gradlew installDebug`

- [ ] **Step 3: Verify against a live session**

Against the `mvp` session on `100.111.143.67:9417` (see the handoff's "Environment, as left"; restart `navetted` and `wprsd` if they are gone). Check each:

1. Two fingers, still, held ~1s then lifted → HUD appears. Repeat → it disappears.
2. A quick two-finger tap still right-clicks in the guest.
3. `FPS` and `KBPS` track a repainting guest and fall toward zero when it idles.
4. `RTT` shows a plausible tailnet figure — tens of milliseconds, not zero and not blank.
5. Turn wifi off for ~8 seconds: `AGE` climbs while the reconnect runs, and `RTT` blanks rather than freezing at its last value.
6. `DROP` reads 0 across an ordinary attach — the three-packet baseline is doing its job.
7. Rotate/unfold to force a resize: `DISC` increments.

- [ ] **Step 4: Verify the old-daemon path, which is the one that is only reasoned about so far**

Check out `master`'s `navetted` (without Task 2), build and run it, and attach the new app. Expected: the session works normally, `RTT` stays blank, and `adb logcat` shows `media server reported:` lines carrying `invalid_input`. This is exit criterion 4 and is the only way to know the compatibility claim is true rather than plausible.

- [ ] **Step 5: Update `android/README.md`**

Add the HUD to Verified, with the gesture and the field meanings; add to Known limitations that `DEC` includes codec queueing and is **not** comparable to the desktop viewer's `DEC`, and that `RTT` is answered by the media socket task so it stays low while the bridge loop stalls — which is what `AGE` is there to reveal.

- [ ] **Step 6: Append a handoff section**

Add to `docs/HANDOFF.md` a section recording: what landed, the measured on-device figures from Step 3, the old-daemon result from Step 4, and the two-finger deviation and why.

- [ ] **Step 7: Commit**

```bash
git add android/README.md docs/HANDOFF.md
git commit -m "$(cat <<'EOF'
docs: record the Android HUD, its numbers, and its blind spot

Includes the measured on-device figures and the old-daemon
compatibility check, which was reasoned about in the design and is now
observed. Documents plainly that DEC is not comparable to the desktop
viewer's and that RTT reads healthy through a bridge-loop stall -- the
reason AGE is on the overlay beside it.

Claude-Session: https://claude.ai/code/session_01NG5nNeZMVGuvo4d4NPMKPr
EOF
)"
```

---

## Self-review

**Spec coverage.** Every section maps to a task: the metric table → Tasks 4/5/6; the `DEC` caveat → Task 4's naming plus Task 9's README note; RTT and the socket-task decision → Tasks 1/2/3/6; the `AGE` mitigation → Task 4 and the Step 3 wifi check; protocol compatibility → Task 1's no-bump, Task 3's unknown-message test, Task 9 Step 4's live check; rate limiting and degradation → Task 4's stale/unanswered tests; Android structure → Tasks 4 and 8; the toggle → Task 7; testing → each task's own steps; all five exit criteria → Task 9 Steps 3-4.

**Placeholder scan.** No TBDs. Every code step carries real code; every test step carries real assertions.

**Type consistency.** `HudSample`'s seven fields are used identically in Task 4's implementation, its `format()` test, and Task 8's overlay. `recordPacket/recordFed/recordPresented/recordPing/recordPong/sample` are named identically in Tasks 4 and 6. `DecoderEvent.Presented(timestampUs)` is produced in Task 5 and consumed under the same name in Task 6. `GestureEffect.ToggleHud` is produced in Task 7 and consumed there. `MediaInput.Ping(nonce: ULong)` matches Rust's `u64`.

**One known unknown, stated rather than hidden:** Task 4's test helper builds a `MediaPacket`/`MediaHeader` from field names read at `net/MediaProtocol.kt:133-150`. If those constructors differ, the helper is what changes — never the protocol.
