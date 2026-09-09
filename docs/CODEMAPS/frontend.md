<!-- Generated: 2026-09-08 | Files scanned: 23 Kotlin + 11 Rust (viewer) | Token estimate: ~950 -->

# Frontend — two clients

There is no web frontend. Two native clients speak the same media protocol.

## Android app (Kotlin / Compose)

Package `com.greponlabs.navette`, under `android/app/src/main/kotlin/`.

### Screen tree

```
MainActivity
 └─ NavetteApp                  ← nav host
     ├─ ConnectScreen           ← tailnet host entry
     ├─ DrawerScreen            ← app list, launch a session
     └─ SessionScreen           ← the live stream
         ├─ SessionOverlay      ← keyboard button, status
         └─ SessionHudOverlay   ← perf HUD, presentational only
```

### Session screen internals

`ui/session/SessionScreen.kt` (~918 lines — **over the project's 800-line
ceiling**; extracting a `SessionController` is a recorded follow-up).

| File | Responsibility |
|---|---|
| `GestureInterpreter.kt` | taps, drags, pinch, two-finger HUD toggle |
| `InputMapper.kt` | screen coords → surface coords |
| `ViewTransform.kt` | fit/scale/letterbox |
| `KeycodeMap.kt` | Android keycode → Linux evdev |
| `LandscapeLock.kt` | orientation policy |
| `ReconnectPolicy.kt` | backoff schedule |
| `SessionHud.kt` | metric arithmetic; **time always caller-supplied** |
| `SessionHudOverlay.kt` | renders a computed sample, calculates nothing |

`SessionHud` keeps **one accumulator per stream**, keyed by `streamId`. A
single global accumulator inflates `DROP` by the sequence spread between
streams — tens of thousands on a healthy two-window guest.

### Media path

```
net/MediaClient.kt      ← WebSocket, binary frames + JSON text
net/MediaProtocol.kt    ← 44-byte header decode, MediaInput/MediaServerMessage
media/StreamGate.kt     ← picks which stream is on screen
media/AnnexB.kt         ← NAL handling
media/H264Decoder.kt    ← MediaCodec, async callbacks on a HandlerThread
```

`MediaProtocol.kt:192` **throws** on an unknown `MediaKind`. Unknown *text*
messages are logged and ignored. This asymmetry is why server→client additions
travel as JSON text, never as a new binary kind.

### Threading

Four threads reach the session controller: UI, the socket reader, the
`MediaCodec` callback thread, and the reconnect timer. Every access goes
through the controller's single lock. `MediaCodec.release()` must not be held
across that lock while the callback thread takes it per frame.

Rule earned the hard way: *moving a lock boundary trades one race for another;
adding a check or capture inside an existing window cannot.*

### Types

`ULong` for every u64 wire field — signedness here has caused real bugs.
Non-integer fields use plain `String`.

## Linux viewer (`navette-viewer`)

minifb window, no compositor dependency.

```
main.rs → lib.rs
  client.rs    ← WebSocket, packet decode
  router.rs    ← stream → window routing
  session.rs   ← ViewerSession, one window per toplevel
  window.rs    ← minifb surface
  decoder.rs   ← H.264 decode
  relay.rs     ← input dispatch, coalescing + retry
  hud.rs       ← perf overlay (the Android HUD is a port of this)
  overlay.rs   ← composited chrome
  native.rs    ← platform glue
```

`relay.rs` classifies each input for recovery: `Retry` (key/button release),
`Coalesce(Modifiers|Viewport)` (absolute state — only the latest matters),
`Discard` otherwise.

## HUD metrics (both clients)

`FPS · KBPS · DEC · AGE · DROP · DISC`, plus `RTT` on Android only.

`DEC` is **not** comparable between the two clients: the viewer measures
synchronous decode, Android measures queue-to-`Presented` latency across an
async `MediaCodec`.

`RTT` is answered by the media socket task itself, never forwarded to the
bridge — a pong queued behind the bridge loop would measure the loop, not the
link. It blanks rather than freezing when stale (`RTT_STALE_MS`).

## Android test gate

```
./gradlew testDebugUnitTest --rerun-tasks
```

Read pass/fail counts from the JUnit XML under
`android/app/build/test-results/testDebugUnitTest/`. A cached
"BUILD SUCCESSFUL" can run **zero** tests and has produced a false pass here.
