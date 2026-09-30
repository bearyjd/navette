# Plan: Mobile Keyboard and Logical Scale Presets

## Summary
The Android session screen streams fine but cannot be *worked in* from a
phone: the on-screen keyboard has no Esc/Tab/Ctrl/arrows, Gboard's
autocorrect and glide typing fight the hidden-field text diff (`InputMapper.imeTextDelta`),
the IME covers the bottom half of the stream, and a 2400×1080 guest viewport
renders desktop text at phone-unreadable size. This plan adds (1) a
`SessionKeyBar` with sticky Ctrl/Alt above the IME, (2) a deterministic IME
configuration (password-type field, bounded buffer, empty-field backspace),
(3) `Modifier.imePadding()` so the surface shrinks and the guest re-lays out
above the keyboard, and (4) logical **scale presets** 1×/1.5×/2×/3×: the phone
reports `surface / factor` as its viewport, the guest lays out for that small
screen, and `MediaCodec` upscales the decoded frame to fill the surface.
The preset is remembered per host in the pairing registry (schema v3, migrating
v2 the way v2 migrated v1). **No daemon change**: `ViewportResize` already
resizes the wl_output *and* every toplevel, and the encoder already
reconfigures to whatever size the guest paints.

## User Story
As someone attached to a Navette session from my phone, I want to type a
shell command with Tab completion, hit Ctrl-C, page through `less` with the
arrow keys, and read the terminal without pinching in — with the keyboard up
and the guest still fully visible above it — so the phone is a usable client
rather than a screen mirror.

## Problem → Solution
**Current**:
- Only characters `KeycodeMap.asciiCharToEvdev` can type reach the guest
  (`InputMapper.kt:125`); there is no way to send Esc, Tab, Ctrl+key, arrows,
  Home/End/PgUp/PgDn or Delete from the soft keyboard.
- The hidden `BasicTextField` (`SessionScreen.kt:499-506`) uses
  `KeyboardOptions.Default` (`KeyboardType.Text`, autocorrect on), so Gboard
  composes, suggests and glide-types; every composition update becomes a
  backspace-and-retype burst on the guest, and its buffer grows without bound.
  A Backspace on an empty field produces no diff and is lost.
- Nothing consumes `WindowInsets.ime`, so the IME overlays the `SurfaceView`
  (targetSdk 36 → edge-to-edge is enforced on Android 15+, and the window no
  longer resizes for the keyboard).
- The viewport sent is the surface's own pixel size
  (`SessionController.kt:571-572`), so the guest lays out for 2400×1080.

**Desired**:
- A scrollable key bar sits between the stream and the IME; Ctrl/Alt are
  sticky (tap = armed for the next key, tap again = disarm, long-press = locked).
- The hidden field is `KeyboardType.Password` + `autoCorrectEnabled = false`
  (no suggestions, no glide on Gboard), bounded to 64 characters, and an
  empty-field Backspace reaches the guest.
- The stream container has `imePadding()`; the surface shrinks, the existing
  `onSurfaceResized` → `ViewportResize` path resizes the guest above the IME.
- `SessionController` holds a `ViewScale`; `onSurfaceResized` sends
  `surface / factor` (even, clamped, aspect-preserving); pointer motion already
  rescales through `contentSize` (`rescaleToContent`), scroll deltas start to.
- A "Scale" menu on the session screen; the choice persists on
  `SavedPairing.viewScale` (registry v3). Phones default 2×, tablets 1×.

## Metadata
- **Complexity**: Large
- **Source PRD**: N/A
- **PRD Phase**: N/A
- **Estimated Files**: 21 (6 CREATE — 3 source + 3 test; 15 UPDATE — 8 source/config, 4 test, 3 docs). Zero Rust files.

---

## UX Design

### Before
```
landscape phone, IME raised                          (surface 2400×1080 px)
┌────────────────────────────────────────────────────────────────────────┐
│ File   Send file                                        Hide keyboard  │ ← TextButtons over video
│                                                                        │
│      guest laid out at 2400×1080 — desktop-sized text, unreadable      │
│                                                                        │
│┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄ bottom ~45% of the guest is UNDER the IME ┄┄┄┄┄│
│▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓│
│▓ Gboard (Text type): suggestions strip, autocorrect, glide            ▓│
│▓ q w e r t y u i o p   — no Esc, Tab, Ctrl, arrows anywhere           ▓│
└────────────────────────────────────────────────────────────────────────┘
```

### After
```
landscape phone, IME raised, Scale 2×                (surface 2400×1080 px)
┌────────────────────────────────────────────────────────────────────────┐
│ File   Send file                     Keys   Scale 2×    Hide keyboard  │
│                                                                        │
│   guest laid out at 1280×240 (visible 2400×~450 ÷ 1.875, see Task 2)   │
│   decoded frame upscaled by MediaCodec to fill the visible area        │
│   → readable terminal, fully above the keyboard                        │
├────────────────────────────────────────────────────────────────────────┤
│ Esc  Tab  [Ctrl] Alt  ↑  ↓  ←  →  Home  End  PgUp  PgDn  Del  ~  |  ▸  │ ← SessionKeyBar (scrolls)
├────────────────────────────────────────────────────────────────────────┤
│▓ Gboard (Password type): no suggestions, no autocorrect, no glide     ▓│
│▓ q w e r t y u i o p                                                  ▓│
└────────────────────────────────────────────────────────────────────────┘
[Ctrl] = armed (tinted); a locked modifier shows a filled chip.
```

### Interaction Changes
| Touchpoint | Before | After | Notes |
|---|---|---|---|
| Typing on Gboard | Text-type field; suggestions/autocorrect/glide produce backspace-retype bursts | Password-type field: one `commitText` per tap; diff is one press/release pair per char | `KeyboardType.Password` → `TYPE_TEXT_VARIATION_PASSWORD`; Compose 1.10.6 never sets `NO_SUGGESTIONS`, so Password is the lever (see External Documentation) |
| Backspace on an empty hidden field | Lost (no diff) | Reaches the guest as `KEY_BACKSPACE` via `onPreviewKeyEvent` on the field | Gboard falls back to `sendKeyEvent(KEYCODE_DEL)` when there is nothing to delete |
| Enter on Gboard | `\n` appended → `KEY_ENTER` (`KeycodeMap.kt:236`) | Unchanged (`ImeAction.Default` keeps `IME_FLAG_NO_ENTER_ACTION`) | Verified on device checklist |
| Esc / Tab / arrows / Home / End / PgUp / PgDn / Del | Impossible from the soft keyboard | One tap on the key bar → press/release pair | Same shape as `imeTextDelta` typing a char |
| Ctrl+letter, Alt+letter | Impossible | Tap Ctrl (armed) then a letter on Gboard or the bar → `KEY_LEFTCTRL` press, key press/release, `KEY_LEFTCTRL` release | **The raw modifier key press is load-bearing**: wprsd ignores ctrl/alt in `KeyboardEvent::Modifiers` (see Gotchas) |
| Long-press Ctrl/Alt | — | Locked until tapped again; every key goes out chorded | Chip filled while locked |
| IME shown | Covers the stream | Stream container shrinks (`imePadding()`), `onSurfaceResized` → guest re-lays out above the IME | One encoder reconfigure per show/hide (~0.8 s end-to-end, measured for the viewer) |
| Text size | 1:1 with a 2400×1080 desktop | Scale 1×/1.5×/2×/3× menu; phone default 2× | Change re-sends `ViewportResize` immediately (no 150 ms debounce) and persists per host |
| Pinch zoom / two-finger pan | Client-side view transform | Unchanged; scale and zoom compose (zoom is in surface pixels, scale is in guest pixels) | `ViewTransform` untouched |
| Two-finger scroll | 1 finger px = 1 guest px | 1 finger px = `1/effectiveScale` guest px (content follows the finger) | `sendScroll` gains the same content/surface rescale `sendMotion` already has |
| Hardware keyboard | `onKeyEvent` path | Unchanged; sticky modifiers do **not** apply to hardware keys (they have their own Ctrl) | Explicit non-goal |
| Pairing registry | v2 (wake) | v3 adds `viewScale`; v1/v2 decode; unknown scale value degrades to `null` | `{"version":3,"hosts":[]}` is now Valid, `4` is Future — update `PairingRegistryTest.kt:40` |

---

## Mandatory Reading

| Priority | File | Lines | Why |
|---|---|---|---|
| P0 | `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt` | 93-172 (state), 328-418 (layout: Box → `key(reconnectNonce){AndroidView}` + overlays), 463-524 (`ImeLayer`) | Every UI change lands here: the container that gets `imePadding()`, the hidden field, the top-right buttons |
| P0 | `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionController.kt` | 39 (`RESIZE_DEBOUNCE_MS = 150L`), 117-129 (`SessionUiState`), 139-154 (ctor), 557-574 (`onSurfaceResized`), 590-611 (`onTouchEvent`), 624-680 (`applyEffect`), 683-711 (`onKeyEvent`/`onImeText`), 841-879 (`sendMotion`/`sendButton`/`sendScroll`) | Owns viewport, pointer and key paths; `viewScale` and sticky modifiers live here |
| P0 | `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/InputMapper.kt` | 60-89 (axis + `scrollUnits`), 91-115 (`keyboardKey`/`keyboardModifiers`), 125-180 (`sendableText`/`imeTextDelta`), 197-226 (`rescaleToContent`/`clampViewport`) | Pure functions to extend: scaled viewport, chord, scroll rescale |
| P0 | `android/app/src/main/kotlin/com/greponlabs/navette/net/PairingRegistry.kt` | 16-19 (`SavedPairing`), 25-30 (`RegistryWire`/`HostWire`), 39-42 (VERSION, strict JSON), 44-55 (`encode`), 57-81 (`decode` + degrade rule), 84-90 (`upsert`), 105-115 (`setWake`) | Schema v3 mirrors every one of these |
| P0 | `android/app/src/main/kotlin/com/greponlabs/navette/net/PairingStore.kt` | 8-39 (interface, `setWake` at 35-36), 73-74 (`EncryptedPairingStore.setWake`), 78-91 (`mutableSnapshot` + the `ignoreUnknownKeys` TODO) | `setViewScale` goes next to `setWake` |
| P1 | `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/KeycodeMap.kt` | 23-107 (constants), 224-275 (`asciiCharToEvdev`) | Reuse the constants; the key bar's symbol keys go through `asciiCharToEvdev` |
| P1 | `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/ViewTransform.kt` | 23-33 (doc: layout size never changes on zoom), 123-129 (`localToScreen`/`screenToLocal`) | Proves zoom is a surface-pixel transform orthogonal to logical scale |
| P1 | `android/app/src/main/kotlin/com/greponlabs/navette/media/H264Decoder.kt` | 128 (`configure(format, surface, …)`), 311-323 (`releaseOutputBuffer(index, true)` + `Configured`), 373-384 (`displaySize` crop) | The display mechanism: MediaCodec renders into the SurfaceView's Surface with default scale-to-fit; `Configured` feeds `contentSize` |
| P1 | `android/app/src/main/kotlin/com/greponlabs/navette/ui/AppViewModel.kt` | 44-82 (`AppUiState`, `wakeRoute`), 113-125 (`AppEvent.SetWake`), 235-256 (`onEvent`), 387-397 (`setWake`) | `SetViewScale` mirrors `SetWake` exactly |
| P1 | `android/app/src/main/kotlin/com/greponlabs/navette/ui/NavetteApp.kt` | 25-47 | Where `SessionScreen` gets its parameters |
| P1 | `crates/navette-bridge/src/input.rs` | 147-199 (`KeyboardKey` → raw `KeyInner`), 201-224 (`KeyboardModifiers`), 226-245 (`ViewportResize` → `update_output` + `ToplevelConfigure` per toplevel) | The "no daemon change" evidence |
| P1 | `crates/navetted/src/bridge.rs` | 31 (`RESIZE_DEBOUNCE` 100 ms), 363-377 (debounced apply + `ForceKeyframeAll`), 727-733 (capture), 790-818 (`encode_frame` reconfigures on frame size change) | Rest of the evidence; the cost of each resize |
| P1 | `~/.cargo/git/checkouts/wprs-a1177b03fe300706/38c61fe/src/server/client_handlers.rs` (pinned rev `38c61feb…`, `crates/navetted/Cargo.toml:28`) | 267-300 (`set_key_state` → smithay `keyboard.input`), 431-468 (`Modifiers` handler) | **wprsd derives ctrl/alt/shift from raw key presses only**; `Modifiers` sets layout + caps/num lock |
| P2 | `android/app/src/test/kotlin/com/greponlabs/navette/net/PairingRegistryTest.kt` | 37-41, 45-54, 56-80, 96-122, 151-183 | The v2 test shapes to copy for v3 |
| P2 | `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/SessionControllerTest.kt` | 22-62 (`FakeMediaSessionClient`, `sentInputs` at 34), 66-91 | Controller test harness (`Dispatchers.setMain(StandardTestDispatcher)`, `runCurrent()`) |
| P2 | `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/InputMapperTest.kt` | 19-28 (helpers), 291-342 (modifiers/shift tests), 450-500 (rescale/clamp tests) | Pure-function test shapes |
| P2 | `android/app/src/test/kotlin/com/greponlabs/navette/ui/AppViewModelFixtures.kt` | 65-124 (`FakePairingStore`, `setWake` at 113-118) | Must gain `setViewScale` or nothing compiles |
| P2 | `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/GestureInterpreterTest.kt` | 16-49 (`Fingers` driver) | Pattern for testing a pure state machine (`StickyModifiers`) |
| P2 | `android/app/src/main/AndroidManifest.xml` | 35-44 (`<activity>` — no `windowSoftInputMode`) | Needs `adjustResize` for API < 30 |

## External Documentation
- **Compose IME insets** — `Modifier.imePadding()` and `WindowInsets.ime`:
  https://developer.android.com/develop/ui/compose/system/insets-ui .
  Setup: "Set the `windowSoftInputMode` to `adjustResize` to allow the app
  to receive IME insets" — https://developer.android.com/develop/ui/compose/system/setup-e2e .
  Android 15 (SDK 35) enforces edge-to-edge, so with `targetSdk = 36`
  (`android/app/build.gradle.kts:15`) the window no longer resizes for the
  IME on Android 15+; insets are the only mechanism —
  https://developer.android.com/develop/ui/compose/layouts/insets .
- **`KeyboardOptions`** (Compose Foundation 1.10.6 via BOM 2026.03.01):
  `KeyboardOptions(capitalization, autoCorrectEnabled: Boolean?, keyboardType, imeAction, …)`;
  the `autoCorrect: Boolean` parameter is deprecated. Reference:
  https://developer.android.com/reference/kotlin/androidx/compose/foundation/text/KeyboardOptions .
  "Some of these options may not be guaranteed if the software keyboard does
  not support them" — https://developer.android.com/develop/ui/compose/text/user-input .
  Verified against the 1.10.6 bytecode (`TextInputServiceAndroid_androidKt.update`):
  `Password` → `inputType = TYPE_CLASS_TEXT | TYPE_TEXT_VARIATION_PASSWORD` (129);
  `singleLine = false` adds `TYPE_TEXT_FLAG_MULTI_LINE` and, **only for
  `ImeAction.Default`**, `IME_FLAG_NO_ENTER_ACTION`; `autoCorrect` adds
  `TYPE_TEXT_FLAG_AUTO_CORRECT`; `TYPE_TEXT_FLAG_NO_SUGGESTIONS` is never set.
- **Android `InputType` / `EditorInfo`**:
  https://developer.android.com/reference/android/text/InputType#TYPE_TEXT_VARIATION_PASSWORD ,
  https://developer.android.com/reference/android/view/inputmethod/EditorInfo#IME_FLAG_NO_ENTER_ACTION
  ("the IME should insert a newline for Enter"). Gboard disables suggestions,
  autocorrect and glide typing on password fields.
- **`InputConnection`**: IMEs delete with `deleteSurroundingText` and may fall
  back to `sendKeyEvent(KEYCODE_DEL)` —
  https://developer.android.com/reference/android/view/inputmethod/InputConnection .
- **MediaCodec surface scaling**: with a Surface output the default video
  scaling mode is `VIDEO_SCALING_MODE_SCALE_TO_FIT` (the frame is stretched
  to the surface's dimensions) —
  https://developer.android.com/reference/android/media/MediaCodec#setVideoScalingMode(int) .
  `H264Decoder` never calls it (`H264Decoder.kt:128`, `:315`), so a
  1280×240 frame already fills a 2400×450 SurfaceView.
- **Phone vs tablet**: `Configuration.smallestScreenWidthDp` with the 600 dp
  tablet threshold —
  https://developer.android.com/reference/android/content/res/Configuration#smallestScreenWidthDp ,
  https://developer.android.com/guide/topics/large-screens/support-different-screen-sizes .
- **evdev constants**: `/usr/include/linux/input-event-codes.h` (already the
  source for `KeycodeMap.kt`, per its header comment at lines 16-20).

---

## Patterns to Mirror

### NAMING_CONVENTION
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/ui/session/InputMapper.kt:30-31, 89, 221
```kotlin
fun pointerMotion(clientId: Long, surfaceId: Long, x: Double, y: Double): MediaInput.PointerMotion =
    MediaInput.PointerMotion(clientId = clientId.toULong(), surfaceId = surfaceId.toULong(), x = x, y = y)
// ...
fun scrollUnits(fingerDelta: Float): Double = -fingerDelta * SCROLL_UNITS_PER_PIXEL
// ...
fun clampViewport(width: Int, height: Int): Pair<Int, Int>? {
```
Pure functions on an `object`, primitives in, `MediaInput` or `Pair` out,
`null` for "nothing to send". Constants are `const val SCREAMING_SNAKE`
with a doc comment saying where the number comes from. Test names are
backticked sentences (`InputMapperTest.kt:471` ``fun `viewport sizes are clamped into the range the bridge accepts`()``).

### ERROR_HANDLING (degrade-don't-discard, and runCatching at the store boundary)
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/net/PairingRegistry.kt:72-77
```kotlin
// No shipped version 1 writer ever emitted wake fields, so their presence is corruption, not an early adopter.
if (wire.version < FIRST_VERSION_WITH_WAKE && wire.hosts.any { it.mac != null || it.wakeViaId != null }) return RegistryDecode.Corrupt
val hosts = wire.hosts.map { item ->
    val pairing = validatedPairing(item.host, item.port, item.token) ?: return RegistryDecode.Corrupt
    val wake = item.mac?.let { mac -> item.wakeViaId?.let { via -> validatedWake(item.id, mac, via, ids) } }
    SavedPairing(item.id, pairing, wake)
}
```
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/ui/AppViewModel.kt:387-397
```kotlin
private fun setWake(hostId: String, wake: WakeTarget?) {
    val updated = runCatching { pairingStore.setWake(hostId, wake) }
        .onFailure { Log.w(TAG, "failed to save the wake target: ${it.message}") }
        .getOrNull()
    if (updated == null) {
        _state.update { it.copy(snackbarMessage = "Wake-up settings not saved.") }
        return
    }
    wakeJob?.cancel()
    _state.update { it.copy(registry = updated, wake = WakeUiState.Idle) }
}
```

### LOGGING_PATTERN
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/net/MediaClient.kt:171-176
```kotlin
private fun logDropped(input: MediaInput, reason: String): Boolean {
    // Variant name only: MediaInput.SetClipboard is a data class, so the
    // naive "$input" would render the user's clipboard text into logcat.
    Log.d(TAG, "dropped ${input::class.simpleName}: $reason")
    return false
}
```
`Log.w` for a failed persistence (`AppViewModel.kt:389`), `Log.d` for
breadcrumbs, `private const val TAG` in a companion (`AppViewModel.kt:577`).
Never log typed text. `SessionController` itself logs nothing today; keep it
that way (the client logs every dropped send).

### STATE/PERSISTENCE (the wake plumbing, end to end)
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/net/PairingRegistry.kt:16, 30, 105-115
```kotlin
data class SavedPairing(val id: String, val pairing: Pairing, val wake: WakeTarget? = null) {
// ...
private data class HostWire(val id: String, val host: String, val port: Int, val token: String, val mac: String? = null, val wakeViaId: String? = null)
// ...
fun setWake(registry: PairingRegistry, hostId: String, wake: WakeTarget?): PairingRegistry {
    require(registry.hosts.any { it.id == hostId }) { "unknown host" }
    // ...
    return registry.copy(hosts = registry.hosts.map { if (it.id == hostId) it.copy(wake = validated) else it })
}
```
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/net/PairingStore.kt:35-36, 73-74
```kotlin
/** Sets or clears how [hostId] is woken; see [PairingRegistryCodec.setWake] for what is rejected. */
fun setWake(hostId: String, wake: WakeTarget?): PairingRegistry
// ...
override fun setWake(hostId: String, wake: WakeTarget?): PairingRegistry =
    PairingRegistryCodec.setWake(mutableSnapshot(), hostId, wake).also(::writeRegistry)
```
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/ui/NavetteApp.kt:54
```kotlin
onSetWake = { id, wake -> viewModel.onEvent(AppEvent.SetWake(id, wake)) },
```
Codec op → store op → `AppEvent` → ViewModel `runCatching` → screen callback.
Immutable: every op returns a new `PairingRegistry`.

### SERVICE_PATTERN (SessionController → InputMapper → client)
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionController.kt:557-574
```kotlin
fun onSurfaceResized(width: Int, height: Int) {
    if (width <= 0 || height <= 0) return
    synchronized(lock) { surfaceSize = width to height }
    // A rotation or a fold must not leave the view panned off the content.
    transform = transform.clampedTo(width, height)
    applyTransform()
    resizeJob?.cancel()
    resizeJob =
        scope.launch {
            delay(RESIZE_DEBOUNCE_MS)
            val (clampedWidth, clampedHeight) = InputMapper.clampViewport(width, height) ?: return@launch
            client.sendInput(MediaInput.ViewportResize(clampedWidth, clampedHeight))
        }
}
```
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionController.kt:706-711
```kotlin
fun onImeText(previous: String, current: String) {
    val stream = gate.primary ?: return
    for (input in InputMapper.imeTextDelta(stream.clientId, stream.surfaceId, previous, current)) {
        client.sendInput(input)
    }
}
```
The controller reads `gate.primary` fresh per event, asks `InputMapper` for
the `MediaInput` list, and sends. Main-thread-only gesture state is a plain
`var` (`SessionController.kt:209-217`); anything the UI must render goes
through `_state.update { it.copy(...) }` (`:248-249`).

### TEST_STRUCTURE
// SOURCE: android/app/src/test/kotlin/com/greponlabs/navette/ui/session/SessionControllerTest.kt:66-91
```kotlin
@Test
fun `a fake media client drives controller connection state without platform media`() = runTest {
    Dispatchers.setMain(StandardTestDispatcher(testScheduler))
    try {
        val client = FakeMediaSessionClient()
        val controller =
            SessionController(
                mediaUrl = "ws://unused/v1/sessions/demo/media",
                token = "unused",
                transformHolder = ViewTransformHolder(),
                bridge = ClipboardBridge(),
                client = client,
            )
        controller.open()
        runCurrent()
        // ...
        controller.close()
    } finally {
        Dispatchers.resetMain()
    }
}
```
// SOURCE: android/app/src/test/kotlin/com/greponlabs/navette/net/PairingRegistryTest.kt:45-48
```kotlin
private fun hostJson(id: String, host: String, extra: String = "") =
    """{"id":"$id","host":"$host","port":9417,"token":"$token"$extra}"""

private fun v2(hosts: String, activeId: String = "a") = """{"version":2,"activeId":"$activeId","hosts":[$hosts]}"""
```
Hand-written fakes, no mocking framework (`AppViewModelFixtures.kt:24-29`).
Raw JSON literals for schema tests. `runTest` + `StandardTestDispatcher` +
`runCurrent()`/`advanceUntilIdle()` for anything with `delay`.

### DOC_COMMENT_STYLE
// SOURCE: android/app/src/main/kotlin/com/greponlabs/navette/ui/session/InputMapper.kt:68-81
```kotlin
/**
 * Guest scroll pixels per pixel of finger travel.
 *
 * The bridge forwards `horizontal`/`vertical` straight through as
 * `AxisScroll.absolute` tagged `AxisSource::Continuous`
 * (`crates/navette-bridge/src/input.rs:132-145`), and wprsd applies that
 * as the `wl_pointer.axis` value verbatim. ...
 */
const val SCROLL_UNITS_PER_PIXEL: Double = 1.0
```
Comments say *why*, cite the file:line on the other side of the wire, and
name the measured or observed behaviour that motivated the code.

---

## Files to Change

| File | Action | Justification |
|---|---|---|
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/ViewScale.kt` | CREATE | `ViewScale` enum (X1/X1_5/X2/X3), `fromFactor`, `defaultFor(smallestScreenWidthDp)` |
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/StickyModifiers.kt` | CREATE | Pure sticky Ctrl/Alt state (Off/Armed/Locked) + `HeldModifiers` value |
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionKeyBar.kt` | CREATE | The scrollable key row composable |
| `android/app/src/main/kotlin/com/greponlabs/navette/net/PairingRegistry.kt` | UPDATE | Schema v3: `SavedPairing.viewScale`, `HostWire.viewScale`, decode/encode/upsert/`setViewScale` |
| `android/app/src/main/kotlin/com/greponlabs/navette/net/PairingStore.kt` | UPDATE | `setViewScale` on the interface and `EncryptedPairingStore` |
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/InputMapper.kt` | UPDATE | `scaledViewport`, `keyChord`, `heldModifiers`, chorded `imeTextDelta`, scroll rescale doc |
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionController.kt` | UPDATE | `viewScale` + `setViewScale`, scaled `onSurfaceResized`, rescaled `sendScroll`, sticky modifiers, `onKeyBarKey`, chorded `onImeText` |
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/session/SessionScreen.kt` | UPDATE | `imePadding()` Column layout, `ImeLayer` keyboard options/`TextFieldValue`/bounded buffer/DEL preview, key bar + Keys/Scale buttons, new params |
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/AppViewModel.kt` | UPDATE | `AppEvent.SetViewScale`, `setViewScale`, `AppUiState.savedForPairing` |
| `android/app/src/main/kotlin/com/greponlabs/navette/ui/NavetteApp.kt` | UPDATE | Resolve the host's scale/default and pass callbacks to `SessionScreen` |
| `android/app/src/main/AndroidManifest.xml` | UPDATE | `android:windowSoftInputMode="adjustResize"` on `.MainActivity` (API < 30 needs it for IME insets) |
| `android/app/src/test/kotlin/com/greponlabs/navette/net/PairingRegistryTest.kt` | UPDATE | v3 tests; fix the Future literal at line 40 |
| `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/InputMapperTest.kt` | UPDATE | `scaledViewport`, `keyChord`, chorded delta, scroll rescale tests |
| `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/SessionControllerTest.kt` | UPDATE | Scaled `ViewportResize`, immediate re-send on `setViewScale`, sticky state publication |
| `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/ViewScaleTest.kt` | CREATE | `fromFactor`, `defaultFor` thresholds |
| `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/StickyModifiersTest.kt` | CREATE | Tap/lock/consume transitions |
| `android/app/src/test/kotlin/com/greponlabs/navette/ui/AppViewModelFixtures.kt` | UPDATE | `FakePairingStore.setViewScale` (+ `failOnSave`) |
| `android/app/src/test/kotlin/com/greponlabs/navette/ui/AppViewModelViewScaleTest.kt` | CREATE | `SetViewScale` persists / failed save leaves registry as stored (split like `AppViewModelWakeTest`) |
| `docs/ROADMAP.md` | UPDATE | Line 48: mark the IME/extra-keys half of "input completeness pass" done; layouts + momentum scroll remain |
| `docs/HANDOFF.md` | UPDATE | New dated section: facts below (wprsd modifiers, rescaleToContent, insets, encoder cost) |
| `docs/RUNBOOK.md` | UPDATE | "Common issues": IME/scale notes (tiny text → Scale; missing keys → Keys bar; pre-Android-15 devices) |

`MainActivity.kt` is **not** changed: `enableEdgeToEdge()` would alter the
Connect/Drawer/Hosts screens on pre-Android-15 devices; `adjustResize` in the
manifest is what the Compose docs require. On Android 15+ (edge-to-edge
enforced) the inset is reported and `imePadding()` applies it; on older
devices without edge-to-edge the window itself shrinks under `adjustResize`,
so the visible result is the same either way — the API 29/33 emulator item on
the device checklist is what confirms the older path.

## NOT Building
- **True HiDPI (`wl_output` scale / `buffer_scale`)** — follow-up. Would
  touch `crates/navette-bridge/src/transport.rs:58-85` (`output_info().scale_factor: 1`
  at line 72), the wprs fork's `compositor_utils::update_output`
  (`client_handlers.rs:596-608` at rev 38c61fe), `crates/navette-bridge/src/scene.rs:558`
  (`composite_children` ignores `SurfaceState.buffer_scale`; the `buffer_scale: 1`
  at `crates/navetted/src/bridge.rs:1221` and `scene.rs:731` are **test
  fixtures**, not production), and `crates/navetted/src/bridge.rs:790-818`
  (encoder size = composite size). Logical scale gives the same layout result
  today at lower encode cost; HiDPI would add sharpness.
- **Unicode / non-US text commit** — follow-up: a new `MediaInput` variant
  carrying text, validated in `crates/navette-protocol/src/media.rs:398-434` (`MediaInput::validate`),
  injected in `crates/navette-bridge/src/input.rs` via xkb keysym lookup
  (wprsd has no text-input path today). `asciiCharToEvdev` stays US-only
  (`KeycodeMap.kt:213-223`).
- **Hardware-keyboard changes** — `onKeyEvent` (`SessionController.kt:683-704`)
  is untouched; sticky modifiers do not apply to it.
- **Per-app scale, clipboard changes, momentum scroll, keyboard layouts.**
- **Syncing `imeRaised` with the system dismissing the IME** (back gesture
  leaves `imeRaised = true`; pre-existing). `WindowInsets.isImeVisible` is
  still `@ExperimentalLayoutApi` in foundation-layout 1.10.6.
- **A lenient version pre-parse** for `ignoreUnknownKeys = false`
  (`PairingStore.kt:85-86` TODO): a v3 blob read by a v2 build is Corrupt, not
  Future — pre-existing property of v2-read-by-v1 too; recorded, not fixed.

---

## Step-by-Step Tasks

### Task 1: Pairing registry schema v3 (`viewScale` per host)
- **ACTION**: In `PairingRegistry.kt`: add `val viewScale: ViewScale? = null`
  as the last parameter of `SavedPairing` (line 16) and include it in
  `toString()` (line 18); add `val viewScale: Float? = null` to `HostWire`
  (line 30) with a doc line "`viewScale` arrived in version 3; a value that is
  not one of the presets decodes as no preference". In `PairingRegistryCodec`
  (lines 39-42): `VERSION = 3`, keep `FIRST_VERSION_WITH_WAKE = 2`, add
  `FIRST_VERSION_WITH_VIEW_SCALE = 3`. `encode` (line 52): pass
  `saved.viewScale?.factor`. `decode`: after the line-73 check add
  ```kotlin
  // Mirrors the wake rule above: no v1 or v2 writer ever emitted viewScale.
  if (wire.version < FIRST_VERSION_WITH_VIEW_SCALE && wire.hosts.any { it.viewScale != null }) return RegistryDecode.Corrupt
  ```
  and at line 77 build `SavedPairing(item.id, pairing, wake, item.viewScale?.let(ViewScale::fromFactor))`
  — an unknown factor becomes `null` (degrade), never Corrupt. `upsert`
  (line 87): `SavedPairing(existing?.id ?: UUID.randomUUID().toString(), valid, existing?.wake, existing?.viewScale)`
  with the doc "the scale belongs to the machine's screen, not the token".
  Add
  ```kotlin
  /** Sets or clears (`scale == null`) the logical scale used when attaching to [hostId]. */
  fun setViewScale(registry: PairingRegistry, hostId: String, scale: ViewScale?): PairingRegistry {
      require(registry.hosts.any { it.id == hostId }) { "unknown host" }
      return registry.copy(hosts = registry.hosts.map { if (it.id == hostId) it.copy(viewScale = scale) else it })
  }
  ```
  `remove` (lines 97-102) needs no change (no cross-host reference).
  In `PairingStore.kt`: add `fun setViewScale(hostId: String, scale: ViewScale?): PairingRegistry`
  to the interface after line 36 and
  `override fun setViewScale(hostId: String, scale: ViewScale?): PairingRegistry = PairingRegistryCodec.setViewScale(mutableSnapshot(), hostId, scale).also(::writeRegistry)`
  after line 74. Update the "v2 today" wording in the comments at
  `PairingStore.kt:102-103` and `:116-120` to v3.
- **IMPLEMENT**: `ViewScale` itself is Task 2's file; create it first (it is
  three lines of enum) so this task compiles. `kotlinx.serialization` writes
  `2.0`/`1.5` for `Float`; `fromFactor` compares `Float` exactly, which is
  safe for these four literals.
- **MIRROR**: STATE/PERSISTENCE and ERROR_HANDLING above; the wake degrade
  rule at `PairingRegistry.kt:72-77`.
- **IMPORTS**: `com.greponlabs.navette.ui.session.ViewScale` in
  `PairingRegistry.kt` and `PairingStore.kt` (the first `net → ui.session`
  import in the module; see Notes).
- **GOTCHA**: `json = Json { ignoreUnknownKeys = false }` (`PairingRegistry.kt:42`)
  runs *before* the version check (line 68 vs 69), so a v3 blob with
  `viewScale` present read by today's v2 code is `Corrupt`, and
  `EncryptedPairingStore.loadRegistry` maps Corrupt to an empty registry
  that the next write overwrites (`PairingStore.kt:54-59`). Downgrading the
  APK after this ships loses the registry — same as v2→v1 did. Record in
  HANDOFF (Task 10); do not "fix" here.
  Also `PairingRegistryTest.kt:40` asserts `{"version":3,"hosts":[]}` is
  `Future`; it becomes Valid — change that literal to `4`.
- **VALIDATE**: `cd android && ./gradlew --console=plain :app:testDebugUnitTest --tests '*PairingRegistryTest*'`
  passes with the Task 8 tests.

### Task 2: `ViewScale`, scaled viewport, and scroll rescale
- **ACTION**: CREATE `ui/session/ViewScale.kt`:
  ```kotlin
  /** How much smaller than the surface the guest lays out; the decoded frame is upscaled to fill. */
  enum class ViewScale(val factor: Float, val label: String) {
      X1(1f, "1×"), X1_5(1.5f, "1.5×"), X2(2f, "2×"), X3(3f, "3×");

      companion object {
          /** `null` for a factor that is not a preset: a registry value written by a build with other presets. */
          fun fromFactor(factor: Float?): ViewScale? = entries.firstOrNull { it.factor == factor }

          /** Phones default to 2×, tablets (sw600dp and up, Android's own threshold) to 1×. Never stored. */
          fun defaultFor(smallestScreenWidthDp: Int): ViewScale = if (smallestScreenWidthDp < TABLET_MIN_SW_DP) X2 else X1
      }
  }

  const val TABLET_MIN_SW_DP: Int = 600
  ```
  In `InputMapper.kt` add, next to `clampViewport` (line 221):
  ```kotlin
  /**
   * The viewport to report for a [width]x[height] surface viewed at [factor].
   *
   * Divides by the largest divisor <= [factor] that keeps both dimensions at
   * or above the bridge's minimum (`crates/navette-protocol/src/media.rs:416-420`:
   * 320x240, mirrored by MIN_VIEWPORT_WIDTH/HEIGHT in MediaProtocol.kt:78-81), so the
   * clamp in [clampViewport] never changes the aspect ratio -- a clamped
   * height with an unclamped width would make MediaCodec's scale-to-fit
   * stretch the picture. Reachable in practice: a landscape phone with the
   * IME up has ~450 px of surface height, and 450/2 < 240.
   */
  fun scaledViewport(width: Int, height: Int, factor: Float): Pair<Int, Int>? {
      if (width <= 0 || height <= 0 || !factor.isFinite() || factor < 1f) return null
      val fit = minOf(factor.toDouble(), width.toDouble() / MIN_VIEWPORT_WIDTH, height.toDouble() / MIN_VIEWPORT_HEIGHT)
          .coerceAtLeast(1.0)
      return clampViewport((width / fit).toInt(), (height / fit).toInt())
  }
  ```
  In `SessionController.kt`: add a constructor parameter
  `initialViewScale: ViewScale = ViewScale.X1` after `pendingBlobAnnouncement`
  (line 153); a main-thread-only `private var viewScale = initialViewScale`
  next to `gestureState` (line 215); `viewScale: ViewScale` in
  `SessionUiState` (line 117-129) initialised from the ctor in `_state`.
  Replace lines 571-572 with a shared helper:
  ```kotlin
  private fun sendViewport(width: Int, height: Int) {
      val (viewportWidth, viewportHeight) = InputMapper.scaledViewport(width, height, viewScale.factor) ?: return
      client.sendInput(MediaInput.ViewportResize(viewportWidth, viewportHeight))
  }
  ```
  and add
  ```kotlin
  /** Re-sends the viewport at once: the user asked for it, so there is no burst to debounce. */
  fun setViewScale(scale: ViewScale) {
      if (scale == viewScale) return
      viewScale = scale
      _state.update { it.copy(viewScale = scale) }
      val (width, height) = synchronized(lock) { surfaceSize } ?: return
      resizeJob?.cancel()
      sendViewport(width, height)
  }
  ```
  In `sendScroll` (lines 868-879) rescale the finger delta with the same
  content/surface ratio `sendMotion` uses (lines 843-848):
  ```kotlin
  val (surfaceWidth, surfaceHeight) = synchronized(lock) { surfaceSize } ?: return
  val (contentWidth, contentHeight) = _state.value.contentSize ?: (surfaceWidth to surfaceHeight)
  val (guestDx, guestDy) = InputMapper.rescaleToContent(dx, dy, surfaceWidth, surfaceHeight, contentWidth, contentHeight) ?: return
  client.sendInput(InputMapper.pointerAxis(stream.clientId, stream.surfaceId, InputMapper.scrollUnits(guestDx.toFloat()), InputMapper.scrollUnits(guestDy.toFloat())))
  ```
  and rewrite the doc at lines 861-867 (it currently says "already
  screen-space pixel deltas, so they need no rescaling" — true only at 1×).
  Update `InputMapper.kt:68-81` (`SCROLL_UNITS_PER_PIXEL` doc) to say the
  delta is rescaled into guest pixels first.
- **IMPLEMENT**: **Pointer motion needs no change.** `sendMotion`
  (`SessionController.kt:841-852`) already maps surface pixels into the
  decoded frame's space via `InputMapper.rescaleToContent(..., contentWidth, contentHeight)`,
  and `contentSize` comes from `DecoderEvent.Configured` (`H264Decoder.kt:322`,
  crop-aware at `:373-384`). At 2× the frame is `surface/2`, so the ratio is
  `0.5` and every motion is divided by the scale for free. `ViewTransform`
  zoom is a surface-pixel `scaleX/scaleY/translation` on the View
  (`SessionController.kt:261-267`, `ViewTransform.kt:23-33`) and is
  inverse-mapped by the framework before `onTouchEvent`; it composes with
  logical scale without any code. The right-click anchor
  (`GestureInterpreter.kt:326`) and pan residual (`SessionController.kt:673-675`)
  flow through the same two sites (`applyEffect` Motion → `sendMotion`;
  `sendScroll`), so those two are the complete list.
- **MIRROR**: `clampViewport`'s doc and null contract (`InputMapper.kt:211-226`);
  `onSurfaceResized`'s debounce (`SessionController.kt:567-573`).
- **IMPORTS**: none new in `InputMapper.kt` (`MIN_VIEWPORT_*` already imported at lines 5-8).
- **GOTCHA**: The scale must be known at construction — `SessionScreen`
  passes it into the `remember`ed `SessionController(...)` at
  `SessionScreen.kt:141-147` — otherwise the first `onSurfaceResized` sends a
  1× viewport and the preset then sends a second one: two encoder restarts on
  attach. Later changes go through `LaunchedEffect(viewScale) { controller.setViewScale(viewScale) }`.
  Between `ViewportResize` going out and the first frame at the new size,
  `contentSize` is stale and pointer motion is mapped against the old frame
  — the existing transient window (`InputMapper.kt:190-192`), now also
  reachable by changing the scale mid-drag; accepted, as it was for resize.
- **VALIDATE**: `./gradlew :app:testDebugUnitTest --tests '*InputMapperTest*' --tests '*SessionControllerTest*' --tests '*ViewScaleTest*'`.

### Task 3: IME determinism in `ImeLayer`
- **ACTION**: In `SessionScreen.kt:488-524` switch the field to the
  `TextFieldValue` overload so composition is visible, configure the keyboard,
  bound the buffer and catch the empty-field Backspace:
  ```kotlin
  /** Characters the hidden field may hold before it is emptied; generous for a burst, small enough that a stale buffer never matters. */
  private const val IME_BUFFER_LIMIT = 64

  var typed by remember { mutableStateOf(TextFieldValue()) }

  BasicTextField(
      value = typed,
      onValueChange = { next ->
          val chorded = controller.onImeText(previous = typed.text, current = next.text)
          // Emptied only between compositions: a reset restarts the IME's
          // input session, which would drop a composition in flight.
          typed =
              if (next.composition == null && (chorded || InputMapper.sendableText(next.text).length > IME_BUFFER_LIMIT)) {
                  TextFieldValue()
              } else {
                  next
              }
      },
      keyboardOptions =
          KeyboardOptions(
              // Password, not Text + no-suggestions: Compose 1.10.6 never sets
              // TYPE_TEXT_FLAG_NO_SUGGESTIONS, and Gboard turns off suggestions,
              // autocorrect and glide typing only for the password variation.
              keyboardType = KeyboardType.Password,
              autoCorrectEnabled = false,
              // Default, not None: with singleLine = false this is what adds
              // IME_FLAG_NO_ENTER_ACTION, so Enter inserts "\n" and the diff
              // maps it to KEY_ENTER (KeycodeMap.asciiCharToEvdev('\n')).
              imeAction = ImeAction.Default,
          ),
      visualTransformation = VisualTransformation.None,
      modifier =
          Modifier
              .size(1.dp)
              .alpha(0f)
              .focusRequester(fieldFocus)
              // Gboard deletes with deleteSurroundingText while there is text;
              // on an empty field it sends KEYCODE_DEL as a key event instead,
              // which the diff cannot see. Preview, not onKeyEvent: the field's
              // own handler consumes DEL before it would bubble.
              .onPreviewKeyEvent { event ->
                  if (event.nativeKeyEvent.keyCode != KeyEvent.KEYCODE_DEL || typed.text.isNotEmpty()) return@onPreviewKeyEvent false
                  if (event.type == KeyEventType.KeyDown) controller.onKeyBarKey(KeycodeMap.KEY_BACKSPACE)
                  true
              },
  )
  ```
  `controller.onImeText` returns `Boolean` (Task 4: whether a sticky modifier
  consumed the typed text). Rewrite the `ImeLayer` doc at lines 463-487: the
  "**The field is never cleared**" paragraph (470-474) becomes "the field is
  emptied only between compositions and only after the delta for the current
  edit has been sent; a programmatic value change does not call
  `onValueChange`, so no backspaces are generated by the reset".
- **IMPLEMENT**: How the diff behaves today, for the implementer: each IME
  edit — commit, composition update, `deleteSurroundingText` — arrives as a
  whole new string; `imeTextDelta` (`InputMapper.kt:150-180`) backspaces past
  the common prefix and retypes; `\n` is a mapped character (`KeycodeMap.kt:236`)
  so Enter already works; `sendableText` drops what cannot be typed
  (`:125`). With `Password`, Gboard stops composing, so `next.composition`
  is normally `null` and the reset condition is reachable on every 65th
  character. `TextFieldValue()` reset → the legacy field calls
  `updateState` → `InputMethodManager.restartInput`; harmless with no
  composition pending.
- **MIRROR**: The existing `KeyboardOptions(keyboardType = KeyboardType.Ascii, imeAction = ImeAction.Done)`
  call style at `WakeTargetDialog.kt:112`.
- **IMPORTS**: `androidx.compose.foundation.text.KeyboardOptions`,
  `androidx.compose.ui.text.input.{KeyboardType, ImeAction, TextFieldValue, VisualTransformation}`,
  `androidx.compose.ui.input.key.{onPreviewKeyEvent, type, KeyEventType, nativeKeyEvent}`,
  `android.view.KeyEvent`.
- **GOTCHA**: `KeyboardOptions.autoCorrect` (Boolean) is deprecated in
  1.10.6; use `autoCorrectEnabled = false` (nullable Boolean). Do **not**
  set `singleLine = true`: that turns `ImeAction.Default` into
  `IME_ACTION_DONE` and Enter stops inserting `\n`. The IME is not obliged
  to honour any of this (Compose docs) — Gboard is the target; the device
  checklist covers it. If Enter does not arrive as `\n` on the device, extend
  the `onPreviewKeyEvent` branch to `KEYCODE_ENTER` → `KEY_ENTER` the same way.
  `typed.text.isNotEmpty()` in the preview check must read the raw text, not
  `sendableText`: an untypable leftover (curly quote) still gives Gboard
  something to `deleteSurroundingText`, which produces no diff *and* no key
  event — the pre-existing "deleting only the unmapped character sends
  nothing" case (`InputMapperTest.kt:413`).
- **VALIDATE**: Compiles; `InputMapperTest` unchanged and green; device
  checklist items "Gboard typing `echo test`", "Backspace on empty field",
  "Enter runs the command".

### Task 4: Sticky modifiers and chords (pure state + `InputMapper.keyChord`)
- **ACTION**: CREATE `ui/session/StickyModifiers.kt`:
  ```kotlin
  enum class StickyState { Off, Armed, Locked }

  /** The two toolbar modifiers; the evdev code is what wprsd's xkb state reacts to. */
  enum class StickyKey(val evdevCode: Int) { Ctrl(KeycodeMap.KEY_LEFTCTRL), Alt(KeycodeMap.KEY_LEFTALT) }

  /** Which modifiers wrap the next key. A value, like [ViewTransform]. */
  data class HeldModifiers(val ctrl: Boolean = false, val alt: Boolean = false) {
      val any: Boolean get() = ctrl || alt
      companion object { val NONE = HeldModifiers() }
  }

  /**
   * Tap = armed for the next key, tap again = off; long-press = locked until
   * tapped. [consumed] is what a sent key does to the state: an armed
   * modifier is spent, a locked one stays. Pure and immutable, so the
   * transitions are testable without a controller.
   */
  data class StickyModifiers(val ctrl: StickyState = StickyState.Off, val alt: StickyState = StickyState.Off) {
      fun tapped(key: StickyKey): StickyModifiers =
          update(key) { state -> if (state == StickyState.Off) StickyState.Armed else StickyState.Off }

      fun locked(key: StickyKey): StickyModifiers = update(key) { StickyState.Locked }

      fun consumed(): StickyModifiers = StickyModifiers(ctrl = spend(ctrl), alt = spend(alt))

      fun held(): HeldModifiers = HeldModifiers(ctrl = ctrl != StickyState.Off, alt = alt != StickyState.Off)

      private fun update(key: StickyKey, next: (StickyState) -> StickyState): StickyModifiers =
          when (key) {
              StickyKey.Ctrl -> copy(ctrl = next(ctrl))
              StickyKey.Alt -> copy(alt = next(alt))
          }

      private fun spend(state: StickyState): StickyState = if (state == StickyState.Armed) StickyState.Off else state
  }
  ```
  In `InputMapper.kt` add:
  ```kotlin
  /** The hardware path derives this from `metaState`; the toolbar supplies it directly. */
  fun heldModifiers(clientId: Long, surfaceId: Long, held: HeldModifiers): MediaInput.KeyboardModifiers =
      MediaInput.KeyboardModifiers(clientId.toULong(), surfaceId.toULong(), ctrl = held.ctrl, alt = held.alt,
          shift = false, capsLock = false, logo = false, numLock = false, layoutIndex = LAYOUT_INDEX)

  /**
   * One key, wrapped in whatever modifiers are held.
   *
   * The raw KEY_LEFTCTRL/KEY_LEFTALT press is the part that works: wprsd feeds
   * every `KeyboardEvent::Key` into smithay's xkb state (`client_handlers.rs:267-300`
   * at the pinned rev) and derives ctrl/alt/shift from *that*; its
   * `KeyboardEvent::Modifiers` handler (`:431-468`) only sets the layout and
   * toggles caps/num lock. Shift already works this way for the IME path
   * (see [imeTextDelta]). The Modifiers messages are sent anyway to match
   * what a hardware Ctrl produces (`SessionController.onKeyEvent`).
   */
  fun keyChord(clientId: Long, surfaceId: Long, evdevCode: Int, needsShift: Boolean, held: HeldModifiers): List<MediaInput> {
      val events = mutableListOf<MediaInput>()
      if (held.ctrl) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTCTRL, pressed = true)
      if (held.alt) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTALT, pressed = true)
      if (held.any) events += heldModifiers(clientId, surfaceId, held)
      if (needsShift) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTSHIFT, pressed = true)
      events += keyboardKey(clientId, surfaceId, evdevCode, pressed = true)
      events += keyboardKey(clientId, surfaceId, evdevCode, pressed = false)
      if (needsShift) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTSHIFT, pressed = false)
      if (held.any) events += heldModifiers(clientId, surfaceId, HeldModifiers.NONE)
      if (held.alt) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTALT, pressed = false)
      if (held.ctrl) events += keyboardKey(clientId, surfaceId, KeycodeMap.KEY_LEFTCTRL, pressed = false)
      return events
  }
  ```
  Give `imeTextDelta` (line 150) a trailing `held: HeldModifiers = HeldModifiers.NONE`
  parameter and replace the typing loop body (lines 168-176) with
  `events += keyChord(clientId, surfaceId, evdevCode, needsShift, held)` —
  identical output for `NONE` (the existing shift wrap is exactly the
  `needsShift` branch), so no existing test changes.
  In `SessionController.kt`: `private var sticky = StickyModifiers()` next
  to `gestureState` (line 215), `modifiers: StickyModifiers` in
  `SessionUiState`, and
  ```kotlin
  fun onModifierTapped(key: StickyKey) = publishSticky(sticky.tapped(key))
  fun onModifierLocked(key: StickyKey) = publishSticky(sticky.locked(key))

  /** A key bar key, or the empty-field Backspace: one chord, then the armed modifiers are spent. */
  fun onKeyBarKey(evdevCode: Int, needsShift: Boolean = false) {
      val stream = gate.primary ?: return
      for (input in InputMapper.keyChord(stream.clientId, stream.surfaceId, evdevCode, needsShift, sticky.held())) client.sendInput(input)
      publishSticky(sticky.consumed())
  }

  /** Returns whether a held modifier wrapped the typed text, so the screen can drop it from its diff base. */
  fun onImeText(previous: String, current: String): Boolean {
      val stream = gate.primary ?: return false
      val held = sticky.held()
      for (input in InputMapper.imeTextDelta(stream.clientId, stream.surfaceId, previous, current, held)) {
          client.sendInput(input)
      }
      val chorded = held.any && InputMapper.typedChars(previous, current) > 0
      if (chorded) publishSticky(sticky.consumed())
      return chorded
  }

  private fun publishSticky(next: StickyModifiers) {
      sticky = next
      _state.update { it.copy(modifiers = next) }
  }
  ```
- **IMPLEMENT**: Add to `InputMapper`, beside `sendableText` (line 125):
  ```kotlin
  /** How many characters [imeTextDelta] types for this edit -- zero for a pure deletion. */
  fun typedChars(previous: String, current: String): Int {
      val sentPrevious = sendableText(previous)
      val sentCurrent = sendableText(current)
      return sentCurrent.length - commonPrefixLength(sentPrevious, sentCurrent)
  }
  ```
  and have `imeTextDelta` reuse it (or the two share the prefix computation);
  `imeTextDelta`'s signature stays `List<MediaInput>` so the existing tests
  (`InputMapperTest.kt:26-27` `delta(previous, current)`) are untouched. Keep
  `commonPrefixLength` private (`InputMapper.kt:228`). Backspaces inside a
  chorded delta are sent **unchorded**: they are the IME reconciling its own
  buffer, not a user chord. The empty-field Backspace from Task 3 goes through
  `onKeyBarKey` and *is* chorded (Ctrl armed + Backspace = Ctrl+Backspace,
  what the user asked for), like any other bar key.
- **MIRROR**: `GestureInterpreter` as a pure `(state, event) → (state, effects)`
  value (`GestureInterpreter.kt:159-166`); `ViewTransform`'s "A value: every
  operation returns a new instance and none mutates" (`ViewTransform.kt:32-33`);
  hardware modifiers order at `SessionController.kt:694-699`.
- **IMPORTS**: none new in `InputMapper.kt` (`KeycodeMap` is same-package).
- **GOTCHA**: **Do not implement Ctrl as `KeyboardModifiers{ctrl=true}` alone** —
  it is a no-op at wprsd (`client_handlers.rs:431-468` at rev 38c61fe: the
  handler reads only `caps_lock`/`num_lock` and `layout_index`). The raw
  `KEY_LEFTCTRL` press/release is what changes the guest's modifier state.
  The bridge's `InputState` tracks pressed keys per attachment
  (`input.rs:163-183`) and releases them on disconnect (`:271-280`), so a
  chord interrupted by a socket drop cannot leave Ctrl stuck.
  After a chorded IME character the hidden field still holds that character
  (Gboard committed "c" for Ctrl+C); Task 3's `chorded` reset empties the
  field so a later Backspace does not delete a character the guest never
  typed as text.
- **VALIDATE**: `./gradlew :app:testDebugUnitTest --tests '*StickyModifiersTest*' --tests '*InputMapperTest*'`.

### Task 5: `SessionKeyBar` composable
- **ACTION**: CREATE `ui/session/SessionKeyBar.kt`:
  ```kotlin
  /** One key on the bar: a named evdev key, or a character typed through [KeycodeMap.asciiCharToEvdev]. */
  private sealed interface BarKey {
      val label: String
      data class Named(override val label: String, val evdevCode: Int) : BarKey
      data class Char(override val label: String, val char: kotlin.Char) : BarKey
      data class Sticky(override val label: String, val key: StickyKey) : BarKey
  }

  private val BAR_KEYS: List<BarKey> =
      listOf(
          BarKey.Named("Esc", KeycodeMap.KEY_ESC), BarKey.Named("Tab", KeycodeMap.KEY_TAB),
          BarKey.Sticky("Ctrl", StickyKey.Ctrl), BarKey.Sticky("Alt", StickyKey.Alt),
          BarKey.Named("↑", KeycodeMap.KEY_UP), BarKey.Named("↓", KeycodeMap.KEY_DOWN),
          BarKey.Named("←", KeycodeMap.KEY_LEFT), BarKey.Named("→", KeycodeMap.KEY_RIGHT),
          BarKey.Named("Home", KeycodeMap.KEY_HOME), BarKey.Named("End", KeycodeMap.KEY_END),
          BarKey.Named("PgUp", KeycodeMap.KEY_PAGEUP), BarKey.Named("PgDn", KeycodeMap.KEY_PAGEDOWN),
          BarKey.Named("Del", KeycodeMap.KEY_DELETE),
          BarKey.Char("~", '~'), BarKey.Char("|", '|'), BarKey.Char("-", '-'), BarKey.Char("/", '/'), BarKey.Char(":", ':'),
      )

  @Composable
  internal fun SessionKeyBar(modifiers: StickyModifiers, onKey: (evdevCode: Int, needsShift: Boolean) -> Unit,
                             onModifierTapped: (StickyKey) -> Unit, onModifierLocked: (StickyKey) -> Unit, modifier: Modifier = Modifier) {
      Row(modifier.fillMaxWidth().background(Color.Black).horizontalScroll(rememberScrollState()).padding(horizontal = 4.dp)) {
          for (key in BAR_KEYS) when (key) {
              is BarKey.Named -> TextButton(onClick = { onKey(key.evdevCode, false) }) { Text(key.label, color = Color.White) }
              is BarKey.Char -> {
                  val (code, shift) = KeycodeMap.asciiCharToEvdev(key.char) ?: continue  // every listed char is in the table
                  TextButton(onClick = { onKey(code, shift) }) { Text(key.label, color = Color.White) }
              }
              is BarKey.Sticky -> StickyKeyChip(key.label, state = when (key.key) { StickyKey.Ctrl -> modifiers.ctrl; StickyKey.Alt -> modifiers.alt },
                  onClick = { onModifierTapped(key.key) }, onLongClick = { onModifierLocked(key.key) })
          }
      }
  }
  ```
  `StickyKeyChip` = `Text` in a `Box` with `Modifier.combinedClickable(onClick, onLongClick)`;
  `Armed` tints the label `MaterialTheme.colorScheme.primary` (NavetteTeal in
  dark, `Theme.kt:10`), `Locked` adds `.background(Color.White.copy(alpha = 0.2f), RoundedCornerShape(6.dp))`.
  Constants: the evdev values are the existing `KeycodeMap` ones
  (`KEY_ESC=1 :23`, `KEY_TAB=15 :37`, `KEY_LEFTCTRL=29 :51`, `KEY_LEFTALT=56 :77`,
  `KEY_HOME=102 :96`, `KEY_UP=103 :97`, `KEY_PAGEUP=104 :98`, `KEY_LEFT=105 :99`,
  `KEY_RIGHT=106 :100`, `KEY_END=107 :101`, `KEY_DOWN=108 :102`,
  `KEY_PAGEDOWN=109 :103`, `KEY_DELETE=111 :105`) — all match the design's
  list; do not redeclare them. `~ | - / :` are in `asciiCharToEvdev`
  (`KeycodeMap.kt:252, 254, 239, 259, 248`).
- **IMPLEMENT**: No haptics: nothing in `android/app/src/main` uses
  `LocalHapticFeedback` (grep is empty), so none here either.
- **MIRROR**: `Text(..., color = Color.White)` inside `TextButton` as the
  session screen's button style (`SessionScreen.kt:442-447`, `:508-523`);
  `SessionHudOverlay`'s opaque black ground over video (`SessionHudOverlay.kt:38-41`).
- **IMPORTS**: `androidx.compose.foundation.{background, horizontalScroll, combinedClickable, rememberScrollState}`,
  `androidx.compose.foundation.layout.{Row, Box, fillMaxWidth, padding}`,
  `androidx.compose.foundation.shape.RoundedCornerShape`,
  `androidx.compose.material3.{Text, TextButton, MaterialTheme}`.
- **GOTCHA**: `combinedClickable` is stable in foundation 1.10.6 (no opt-in
  needed). Buttons must not take focus from the hidden field on touch — in
  touch mode Compose clickables do not request focus on tap, but the device
  checklist verifies the IME stays up after a bar tap. Every bar key goes
  through `controller.onKeyBarKey`, which returns silently while
  `gate.primary == null` (before the first `StreamConfig`), matching
  `onKeyEvent`'s early return (`SessionController.kt:684`).
- **VALIDATE**: `./gradlew :app:lintDebug` clean; device checklist "Tab
  completion", "Ctrl+C", "arrows in `less`".

### Task 6: IME insets — the surface shrinks above the keyboard
- **ACTION**: In `SessionScreen.kt` replace the single `Box` at lines
  328-417 with a `Column` that consumes the IME inset and hosts the stream
  box (weighted) and the key bar:
  ```kotlin
  Column(
      modifier =
          Modifier
              .fillMaxSize()
              .background(Color.Black)
              // Consumes WindowInsets.ime: the stream box below shrinks, its
              // onSizeChanged fires, and the existing resize path re-lays the
              // guest out above the keyboard. One encoder reconfigure per
              // show/hide (bridge.rs:790-818), gated by RESIZE_DEBOUNCE_MS.
              .imePadding(),
  ) {
      Box(
          modifier =
              Modifier
                  .weight(1f)
                  .fillMaxWidth()
                  .focusRequester(focusRequester)
                  .focusable()
                  .onKeyEvent { event -> controller.onKeyEvent(event.nativeKeyEvent) },
      ) {
          key(reconnectNonce) { AndroidView(/* unchanged factory */, modifier = Modifier.fillMaxSize().onSizeChanged { size -> controller.onSurfaceResized(size.width, size.height) }) }
          ImeLayer(...)            // unchanged position: TopEnd controls
          FileTransferLayer(...)
          SessionHudOverlay(...)
          SessionOverlay(...)
      }
      if (imeRaised || keyBarPinned) {
          SessionKeyBar(
              modifiers = state.modifiers,
              onKey = controller::onKeyBarKey,
              onModifierTapped = controller::onModifierTapped,
              onModifierLocked = controller::onModifierLocked,
          )
      }
  }
  ```
  with `var keyBarPinned by remember(pairing.host, sessionName) { mutableStateOf(false) }`
  next to `imeRaised` (line 168). Add
  `android:windowSoftInputMode="adjustResize"` to the `<activity>` at
  `AndroidManifest.xml:35-44` with a comment: "Required for IME insets on API
  < 30; on Android 15+ (targetSdk 36) edge-to-edge is enforced and
  `imePadding()` in SessionScreen is what moves the stream."
- **IMPLEMENT**: Nothing else changes in the resize path: `onSizeChanged`
  (`SessionScreen.kt:388`) and `surfaceChanged` (`SessionController.kt:531-533`)
  both call `onSurfaceResized`, which debounces 150 ms (`:39`, `:567-573`)
  and clamps the zoom transform to the new size (`:565`). The IME inset
  animates over several frames; the debounce means only the settled size is
  sent. The overlays stay inside the stream box so "Waiting for the first
  frame..." is centred on the video, not on the whole column.
- **MIRROR**: The `remember(pairing.host, sessionName)` keying for
  session-scoped UI state (`SessionScreen.kt:165-171`).
- **IMPORTS**: `androidx.compose.foundation.layout.{Column, fillMaxWidth, imePadding}`.
- **GOTCHA**: The Box that held `.focusRequester(focusRequester).focusable().onKeyEvent`
  (lines 333-335) must keep those modifiers — that is the hardware-keyboard
  focus target the reconnect effect re-asserts (`:265-272`). Put
  `imePadding()` on the `Column`, not on the `AndroidView`: the key bar has
  to sit inside the padded region *below* the stream, and the black ground
  must extend behind it. Do **not** add `enableEdgeToEdge()` to
  `MainActivity.kt` (see Files to Change). On Android 15+ the keyboard was
  overlaying the stream precisely because nothing consumed the inset; on
  older devices the Compose docs make `adjustResize` the precondition for
  IME insets being reported at all.
- **VALIDATE**: Device: raise the keyboard → the stream shrinks, the guest
  re-lays out (a `foot` prompt stays visible above Gboard), HUD `DISC`
  increments once; hide → it grows back. Pre-Android-15 device or emulator
  (API 29/33): same behaviour via window resize.

### Task 7: "Keys" and "Scale" controls on the session screen
- **ACTION**: In `ImeLayer` (`SessionScreen.kt:508-523`) the single
  `TextButton` at `Alignment.TopEnd` becomes a `Row(Modifier.align(Alignment.TopEnd).padding(8.dp))`
  holding: `TextButton { Text(if (keyBarPinned) "Hide keys" else "Keys") }`
  toggling `keyBarPinned`; a `ScaleMenuButton(current = viewScale, onSelect = onViewScaleChange)`;
  and the existing Keyboard toggle unchanged. `ScaleMenuButton` is a
  `TextButton` labelled `"Scale ${current.label}"` with a Material3
  `DropdownMenu` of four `DropdownMenuItem`s (`ViewScale.entries`), the
  current one marked; selecting calls `onSelect` and closes. A menu, not a
  cycle button: each step costs an encoder restart (~0.8 s), so the user
  should be able to jump straight to 3×.
  `SessionScreen` gains parameters after `pairing`:
  `viewScale: ViewScale, onViewScaleChange: (ViewScale) -> Unit`; passes
  `initialViewScale = viewScale` into `SessionController(...)` (line 141-147)
  and adds `LaunchedEffect(controller, viewScale) { controller.setViewScale(viewScale) }`.
- **IMPLEMENT**: Hoist `keyBarPinned`/`onKeyBarPinnedChange` into `ImeLayer`
  the way `imeRaised`/`onImeRaisedChange` already are (`:493-494`).
- **MIRROR**: `TextButton(onClick = onManageHosts) { Text("Computers") }`
  (`DrawerScreen.kt:119`) for plain text actions; `AlertDialog` +
  `RadioButton` in `WakeTargetDialog.kt` is the heavier alternative — a
  `DropdownMenu` is lighter and closer to a "quick control".
- **IMPORTS**: `androidx.compose.material3.{DropdownMenu, DropdownMenuItem}`,
  `androidx.compose.foundation.layout.Row`.
- **GOTCHA**: `viewScale` is a `remember`-key-free parameter: the controller
  is created with it once (`remember(pairing.host, pairing.port, sessionName, reconnectNonce)`)
  and later values reach it only through `setViewScale`. Selecting the
  already-selected preset must be a no-op (`setViewScale` returns early) so
  a stray tap does not restart the encoder.
- **VALIDATE**: Device: pick 2× → text roughly doubles in a screenshot
  (`adb shell screencap -p /sdcard/x.png && adb pull /sdcard/x.png`, per
  HANDOFF.md:1633-1634); pick 1× → back; HUD `DISC` +1 per change.

### Task 8: ViewModel plumbing — `AppEvent.SetViewScale` and the default
- **ACTION**: In `AppViewModel.kt`: add after `SetWake` (line 125)
  ```kotlin
  /** Remembers the logical scale for the saved host [hostId]; `null` clears it back to the device default. */
  data class SetViewScale(val hostId: String, val scale: ViewScale?) : AppEvent
  ```
  dispatch it in `onEvent` (after line 244) to
  ```kotlin
  /**
   * Guarded like [setWake]. Unlike a wake target, a scale that did not stick
   * has already taken effect for this session (the controller applied it),
   * so a failure is logged rather than surfaced: the session screen has no
   * snackbar host, and the drawer would show the message minutes later.
   */
  private fun setViewScale(hostId: String, scale: ViewScale?) {
      val updated = runCatching { pairingStore.setViewScale(hostId, scale) }
          .onFailure { Log.w(TAG, "failed to save the view scale: ${it.message}") }
          .getOrNull() ?: return
      _state.update { it.copy(registry = updated) }
  }
  ```
  Add to `AppUiState` (after `wakeRoute`, line 82) the lookup `wakeRoute`
  already does inline at line 78:
  ```kotlin
  /** The saved entry for [pairing], or `null` after a failed save (the pairing in use was never stored). */
  val savedForPairing: SavedPairing?
      get() = pairing?.let { current -> registry.hosts.firstOrNull { it.pairing.host == current.host && it.pairing.port == current.port } }
  ```
  In `NavetteApp.kt` (lines 38-47):
  ```kotlin
  val saved = state.savedForPairing
  val smallestWidthDp = LocalContext.current.resources.configuration.smallestScreenWidthDp
  SessionScreen(
      sessionName = activeSession,
      pairing = pairing,
      viewScale = saved?.viewScale ?: ViewScale.defaultFor(smallestWidthDp),
      // An unsaved pairing (failed keystore write) still gets the control; the
      // choice just lives for the session.
      onViewScaleChange = { scale -> saved?.let { viewModel.onEvent(AppEvent.SetViewScale(it.id, scale)) } },
      onLeave = { viewModel.onEvent(AppEvent.LeaveSession) },
  )
  ```
  In `AppViewModelFixtures.kt` `FakePairingStore` (lines 65-124) add
  ```kotlin
  override fun setViewScale(hostId: String, scale: ViewScale?): PairingRegistry {
      if (failOnSave) throw IllegalStateException("keystore unavailable")
      require(registry.hosts.any { it.id == hostId }) { "unknown host" }
      registry = registry.copy(hosts = registry.hosts.map { if (it.id == hostId) it.copy(viewScale = scale) else it })
      return registry
  }
  ```
  and carry `existing?.viewScale` in its `upsert` (line 94) like the real codec.
- **IMPLEMENT**: The default is resolved in the screen layer and never
  stored — `SavedPairing.viewScale == null` means "device default", which
  is what lets a tablet and a phone share a host entry (they do not, but the
  rule is cheap). `resources.configuration` is plain Android API, no Compose
  deprecation concerns (the project has no `LocalConfiguration` usage).
- **MIRROR**: `setWake` (`AppViewModel.kt:387-397`) and the `SetWake` event
  wiring (`NavetteApp.kt:54`).
- **IMPORTS**: `com.greponlabs.navette.ui.session.ViewScale` in
  `AppViewModel.kt`, `NavetteApp.kt`, `AppViewModelFixtures.kt`.
- **GOTCHA**: `NavetteApp` does not currently read `LocalContext` for
  anything but the ViewModel factory (line 26); the configuration read is
  cheap and recomposes on fold/unfold — a foldable's cover screen is
  `sw < 600`, its inner screen may not be, so the *default* can flip between
  1× and 2× mid-session on a device with no saved preference. Acceptable
  (the guest re-lays out); the per-host save pins it.
- **VALIDATE**: `./gradlew :app:testDebugUnitTest --tests '*AppViewModel*'`.

### Task 9: Tests
- **ACTION**: Write the tests in the Testing Strategy table below. Files:
  `PairingRegistryTest.kt` (v3 section mirroring lines 43-209: `v3(...)`
  helper next to `v2(...)` at line 48; change line 40's `Future` literal to
  version 4), `InputMapperTest.kt` (`scaledViewport`, `keyChord`, chorded
  `imeTextDelta`, `heldModifiers.validate()`), new `ViewScaleTest.kt`,
  new `StickyModifiersTest.kt` (drive it like `Fingers`,
  `GestureInterpreterTest.kt:16-49`), `SessionControllerTest.kt` (viewport
  at 2× after the debounce, immediate re-send on `setViewScale`, no-op on
  the same scale, sticky state published in `state`), new
  `AppViewModelViewScaleTest.kt` (persists; failed save leaves the registry
  as stored; unknown host id rejected by the fake).
- **IMPLEMENT**: Controller tests that need a `gate.primary` (key/IME sends)
  are **not** feasible on the JVM today: the packet loop runs on
  `Dispatchers.Default` (`SessionController.kt:280`), outside the test
  scheduler, and `FakeMediaSessionClient.nextPacket()` returns `null`
  (`SessionControllerTest.kt:46`). Keep every send-composition rule in
  `InputMapper`/`StickyModifiers` (pure) and test the controller only for
  `ViewportResize` (needs no primary; `onSurfaceResized` → `advanceUntilIdle()`)
  and for published state. Expected viewport for `onSurfaceResized(2400, 1080)`
  at `X2` is `ViewportResize(1200, 540)`; for `(2400, 450)` at `X2` it is
  `(1280, 240)` (fit = 450/240 = 1.875).
- **MIRROR**: TEST_STRUCTURE above; the exhaustive clamp loop at
  `InputMapperTest.kt:484-495` for "a scaled viewport is never one the bridge
  would reject" over surfaces `{320..3840} × {240..2160}` and all four factors.
- **IMPORTS**: as the neighbouring tests.
- **GOTCHA**: `Dispatchers.setMain(StandardTestDispatcher(testScheduler))`
  is what makes `delay(RESIZE_DEBOUNCE_MS)` virtual; use
  `testScheduler.advanceUntilIdle()` (or `advanceTimeBy(151)` + `runCurrent()`),
  never a real sleep. Assert on `client.sentInputs.filterIsInstance<MediaInput.ViewportResize>()`.
- **VALIDATE**: `cd android && ./gradlew --console=plain :app:testDebugUnitTest --rerun-tasks :app:lintDebug`
  — 402 existing + new tests green, lint clean.

### Task 10: Docs
- **ACTION**: `docs/ROADMAP.md:48`: change "**input completeness pass**
  (keyboard layouts, compose/IME basics, momentum scroll)" to strike the
  IME half — "~~extra keys + sticky Ctrl/Alt, deterministic IME, logical
  scale presets~~ **done** (`SessionKeyBar.kt`, `ViewScale.kt`); keyboard
  layouts and momentum scroll remain". `docs/HANDOFF.md`: append a dated
  section "Mobile keyboard + scale presets (2026-09-…)" after the Phase 2
  sweep (line 3114 onward) recording: wprsd ignores ctrl/alt in
  `KeyboardEvent::Modifiers` (raw key presses drive modifiers — the reason
  `keyChord` sends `KEY_LEFTCTRL`); pointer motion needed no change because
  `rescaleToContent` already divides by `contentSize/surface`; the
  min-viewport aspect rule in `scaledViewport`; IME insets on Android 15+
  vs `adjustResize` below; each IME show/hide is one encoder restart; the
  v3 registry and the downgrade-reads-Corrupt property; the not-testable-on-JVM
  controller key path. `docs/RUNBOOK.md` "Common issues" (line 421): three
  rows — "text too small on the phone → Scale menu (default 2× on phones)";
  "no Esc/Tab/Ctrl → Keys bar (shown with the keyboard; long-press Ctrl to
  lock)"; "keyboard covers the stream on an older phone → the app needs
  `adjustResize` (shipped in this build); on Android 15+ it is inset-driven".
- **IMPLEMENT / MIRROR**: The HANDOFF section style at `docs/HANDOFF.md:3114-3180`
  (facts the next person needs, file paths, what bit us).
- **IMPORTS / GOTCHA**: N/A. Keep the RUNBOOK table terse like "Reading the HUD" (`:399-419`).
- **VALIDATE**: Links/paths in the docs resolve (`ls` each cited file).

---

## Testing Strategy

### Unit Tests
| Test | Input | Expected Output | Edge Case? |
|---|---|---|---|
| `ViewScale.fromFactor` maps presets and rejects others | `1f, 1.5f, 2f, 3f, 2.5f, null` | `X1, X1_5, X2, X3, null, null` | Invalid registry value → degrade |
| `defaultFor` phone/tablet | `599`, `600`, `360`, `840` | `X2, X1, X2, X1` | Boundary at 600 |
| `scaledViewport` divides and rounds even | `(2400,1080,2f)` | `(1200, 540)` | — |
| `scaledViewport` keeps aspect at the floor | `(2400,450,2f)` | `(1280, 240)` — fit 1.875, not 2 | Min height reached with IME up |
| `scaledViewport` 3× lands exactly on the floor | `(1280,720,3f)` | `(426, 240)` — fit = min(3, 4, 3) = 3; 1280/3 = 426.67 → 426 (already even) | Height exactly at the minimum |
| `scaledViewport` odd result rounds down | `(2402,1080,2f)` | `(1200, 540)` — 1201 → 1200 | Even rounding after division |
| `scaledViewport` at 1× equals `clampViewport` | all sizes in the clamp loop | identical pairs | Regression guard |
| `scaledViewport` never rejected by the bridge | loop over `{320,321,1080,2400,3840}×{240,241,450,1080,2160}` × 4 factors | `MediaInput.ViewportResize(...).validate() == null` | Mirrors `InputMapperTest.kt:484-495` |
| `scaledViewport` degenerate | `(0, 720, 2f)`, `(1280, 720, 0.5f)`, `NaN` | `null` | Guard |
| `keyChord` plain key | `KEY_ESC, needsShift=false, NONE` | `[press ESC, release ESC]` | Same shape as `imeTextDelta` |
| `keyChord` with Ctrl | `KEY_C, false, ctrl` | `[press LEFTCTRL, Modifiers{ctrl}, press C, release C, Modifiers{}, release LEFTCTRL]` | Raw key wraps the Modifiers pair |
| `keyChord` with Ctrl+Alt and shift | `KEY_1, true, ctrl+alt` | ctrl outermost, alt inside, shift innermost; releases in reverse | Nesting order |
| every `keyChord` event validates | all bar keys × held combos | `validate() == null` | Mirrors `InputMapperTest.kt:440` |
| chorded `imeTextDelta` | `("", "c", ctrl)` | the Ctrl chord for `KEY_C` | Ctrl armed then IME char |
| chorded delta deletes unchorded | `("ab", "a", ctrl)` | `[press BACKSPACE, release BACKSPACE]` only | Backspace is never chorded |
| `imeTextDelta` default arg unchanged | existing tests at `InputMapperTest.kt:326-447` | unchanged | Regression |
| `StickyModifiers` tap arms, tap disarms | `tapped(Ctrl)`, `tapped(Ctrl)` | `Armed` then `Off` | — |
| long-press locks, tap unlocks | `locked(Ctrl)`, `tapped(Ctrl)` | `Locked` then `Off` | — |
| `consumed` spends Armed, keeps Locked | `ctrl=Armed, alt=Locked` → `consumed()` | `ctrl=Off, alt=Locked` | Locked survives keys |
| `held()` | `Armed`/`Locked`/`Off` | `true/true/false` | — |
| controller: viewport at 2× | `initialViewScale = X2`, `onSurfaceResized(2400,1080)`, `advanceUntilIdle()` | last `ViewportResize(1200, 540)` | — |
| controller: `setViewScale` re-sends immediately | after the above, `setViewScale(X1)`, `runCurrent()` (no advance) | `ViewportResize(2400, 1080)` present | Skips the debounce |
| controller: same scale is a no-op | `setViewScale(X2)` twice | one `ViewportResize` | No spurious encoder restart |
| controller: sticky state published | `onModifierTapped(Ctrl)` | `state.value.modifiers.ctrl == Armed` | UI binding |
| registry v3 round trip | `setViewScale(twoHosts, towerId, X2)` → encode → decode | `"version":3` in JSON; `viewScale == X2` on tower, `null` on nas | — |
| v2 payload decodes with no scale | `v2(...)` | `viewScale == null`, wake preserved | Migration |
| v1/v2 payload carrying `viewScale` is Corrupt | `{"version":2,…"viewScale":2.0}` | `Corrupt` | Mirrors `PairingRegistryTest.kt:64-69` |
| invalid scale value degrades | `v3(hostJson("a","tower",""","viewScale":2.5"""))` | Valid, `viewScale == null`, host kept | Degrade rule |
| version 4 is Future, 3 is Valid | `{"version":4,"hosts":[]}`, `{"version":3,"hosts":[]}` | `Future`, `Valid` | Fixes line 40 |
| `upsert` keeps the scale on re-pair | `setViewScale` then `upsert(Pairing("TOWER", 9417, newToken))` | scale kept, token rotated | Mirrors `:174-183` |
| `setViewScale(null)` clears; unknown host throws | — | equals pre-set registry; `IllegalArgumentException` | Mirrors `:151-172` |
| ViewModel `SetViewScale` persists | pair, `SetViewScale(id, X3)` | `state.registry.hosts[id].viewScale == X3` | — |
| ViewModel failed save leaves registry | `store.failOnSave = true` | registry unchanged, no crash | Mirrors `AppViewModelWakeTest.kt:202-219` |

### Edge Cases Checklist
- [ ] Scale change mid-drag: `setViewScale` cancels `resizeJob` and sends at once; the in-flight one-finger drag keeps mapping through the stale `contentSize` until the new `Configured` lands (pre-existing transient, `InputMapper.kt:190-192`)
- [ ] IME shown while zoomed: `onSurfaceResized` calls `transform.clampedTo(width, height)` (`SessionController.kt:565`) so the pan is pulled back onto the smaller content
- [ ] Ctrl armed then IME char: chord sent, sticky spent, hidden field emptied (Task 3 `chorded`)
- [ ] Long-press lock then five bar keys: five chords, still `Locked`
- [ ] Buffer reset with a pending composition (`next.composition != null`): no reset, field keeps growing past 64 until the composition ends
- [ ] v2 payload → v3 codec: valid, `viewScale == null`
- [ ] Invalid scale in payload: `null`, host kept
- [ ] Phone (sw 360) vs tablet (sw 800) default: 2× vs 1×
- [ ] 3× with the IME up on a phone: fit factor drops to keep 240 px of height (aspect preserved, no stretch)
- [ ] Reconnect with the bar pinned and Ctrl locked: `keyBarPinned` survives (session-keyed); the sticky state lives in the controller and resets on rebuild — acceptable, note in the key bar doc
- [ ] Concurrent access: `viewScale` and `sticky` are main-thread-only like `gestureState` (`SessionController.kt:205-208`); `surfaceSize` reads take `lock`
- [ ] `sendInput` returning `false` (no socket): the chord is dropped whole or in part exactly like `imeTextDelta` today; the bridge releases held keys on disconnect (`input.rs:271-280`)

---

## Validation Commands

### Android unit tests + lint
```bash
cd android && ./gradlew --console=plain :app:testDebugUnitTest --rerun-tasks :app:lintDebug
```
EXPECT: BUILD SUCCESSFUL; 402 existing tests + ~35 new, 0 failures; lint clean (no new warnings on `KeyboardOptions` — use `autoCorrectEnabled`).

### Rust workspace (must be untouched)
```bash
cargo test --workspace
```
EXPECT: 396 passed, unchanged — this plan modifies no Rust file.

### On-device checklist (the acceptance signal; landscape phone, Gboard)
- [ ] `cd android && ./gradlew :app:installDebug` on the phone (USB or `adb connect` over the tailnet)
- [ ] Daemon on the tailnet: `./target/release/navetted --bind <tailnet-ip>:9417 --allow-remote` (RUNBOOK.md:29); pair the phone; `navette run foot` (RUNBOOK.md:43)
- [ ] Attach: the default is **Scale 2×** on a phone; `foot`'s prompt is readable without pinching; screenshot for the PR (`adb shell screencap -p /sdcard/x.png && adb pull /sdcard/x.png`)
- [ ] Tap **Keyboard**: the stream shrinks to the area above Gboard, the guest re-lays out (prompt still visible), key bar appears between stream and IME; HUD `DISC` +1
- [ ] Type `echo test` + Enter on Gboard: exactly `echo test` appears (no doubled/dropped letters, no autocorrect), the command runs
- [ ] Backspace with nothing typed since the buffer emptied deletes a character in the guest
- [ ] Type `ls /us` then **Tab**: completes to `/usr/`
- [ ] `sleep 100` + Enter, tap **Ctrl**, type `c`: the sleep is interrupted (`^C`); Ctrl chip returns to plain
- [ ] `less /etc/passwd`: **↓ ↑ PgDn PgUp Home End** move; **q** quits (unchorded)
- [ ] Long-press **Ctrl** (locked), type `a` then `e` in `foot`: cursor jumps to line start and end (readline Ctrl-A/Ctrl-E); tap Ctrl to unlock
- [ ] Bar keys **~ | - / :** type the right characters
- [ ] Tapping a bar key does not dismiss Gboard or move focus
- [ ] **Scale** menu → 3× → 1× → 2×: each change re-lays the guest out once (`DISC` +1 each); text size changes accordingly; a tap on the guest still lands where the finger is at every preset (pointer mapping)
- [ ] Two-finger scroll in `less` at 2× follows the finger at roughly 1:1 (rescaled), not 2:1
- [ ] Pinch-zoom in at 2×, pan: unchanged behaviour; raise the IME while zoomed: view stays on content
- [ ] Leave the session, re-attach: 2× (or whatever was chosen) is remembered for this host; a second host keeps its own
- [ ] Hide the keyboard: stream grows back, `DISC` +1
- [ ] Pre-Android-15 device or API 29/33 emulator: the keyboard still pushes the stream up (`adjustResize` path)
- [ ] `adb logcat -s MediaClient:D`: no `refusing to send out-of-range input` during any of the above

---

## Acceptance Criteria
- [ ] All ten tasks completed
- [ ] `./gradlew :app:testDebugUnitTest :app:lintDebug` green; `cargo test --workspace` unchanged at 396
- [ ] Every on-device checklist item ticked on a real phone with Gboard
- [ ] Pairing registry v3 written; v1/v2 blobs load; an unknown scale degrades to `null`
- [ ] No Rust change in the diff
- [ ] Matches the UX design above (key bar between stream and IME; Scale menu; guest re-lays out above the keyboard)

## Completion Checklist
- [ ] Code follows discovered patterns (pure `InputMapper` functions; immutable `StickyModifiers`/`ViewScale`; degrade-don't-discard codec; `runCatching` at the store boundary)
- [ ] Error handling matches codebase style (`Log.w` on failed persistence; silent `return` while `gate.primary == null`)
- [ ] Logging follows codebase conventions (no typed text in logcat; `SessionController` still logs nothing)
- [ ] Tests follow test patterns (hand-written fakes, raw JSON literals, `runTest` + `StandardTestDispatcher`)
- [ ] No hardcoded values (`IME_BUFFER_LIMIT`, `TABLET_MIN_SW_DP`, `RESIZE_DEBOUNCE_MS`, `MIN_VIEWPORT_*` are the only knobs and each has a doc comment)
- [ ] Documentation updated (ROADMAP line 48, HANDOFF section, RUNBOOK rows)
- [ ] No unnecessary scope additions (HiDPI, Unicode commit, hardware keys, per-app scale, clipboard all untouched)
- [ ] Self-contained — no questions needed during implementation

## Risks
| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Gboard behaves differently from the flag analysis (Enter not `\n` on a password field, DEL not sent as a key event on an empty field) | Medium — IME behaviour is explicitly not guaranteed (Compose docs) | Medium — Enter or Backspace unusable | `onPreviewKeyEvent` branch already catches DEL; extend it to `KEYCODE_ENTER` if the device shows it; both are single-line changes in `ImeLayer` |
| Each IME show/hide is an encoder restart (~0.8 s measured for the viewer; ffmpeg respawn per `reconfigure`, `bridge.rs:810-816`) | Certain | Low-Medium — a black/frozen beat when the keyboard appears | 150 ms client debounce + 100 ms daemon debounce mean one restart per show/hide, not per animation frame; documented in HANDOFF/RUNBOOK |
| Min-viewport clamp changes aspect ratio and MediaCodec stretches the picture | Medium at 2×/3× with the IME up on a phone | Medium — distorted text | `scaledViewport` reduces the divisor to keep both dimensions ≥ min so the aspect never changes; exhaustive test loop |
| The `net → ui.session` import for `ViewScale` inverts the module's layering | Certain | Low | Two-line move to `net/ViewScale.kt` if the reviewer prefers; the plan follows the design's placement |
| A v3 registry read by a pre-v3 build is Corrupt → emptied on next write (`ignoreUnknownKeys = false`, `PairingRegistry.kt:42`, `PairingStore.kt:85-86` TODO) | Only on APK downgrade | High for that user (re-pair every host) | Pre-existing property of every schema bump; recorded in HANDOFF; the lenient pre-parse TODO stays a follow-up |
| Sticky Ctrl left armed by accident, next typed letter becomes a chord | Medium | Low — one unexpected shortcut | Armed state is visible (tinted chip) and spent by one key; locked requires a long-press |
| Controller key/IME paths remain untested on the JVM (`Dispatchers.Default` packet loop) | Certain | Low — all composition logic is in pure, tested functions | Recorded as a HANDOFF follow-up: a `dispatcher` ctor parameter for the packet loop would make a primary bootstrap testable |
| `smallestScreenWidthDp` default flips on a foldable with no saved scale | Low | Low — one extra re-layout | Saving any preset pins it |

## Notes
- **Facts in the brief that were wrong, corrected here**: the Android debounce
  is **150 ms** (`SessionController.kt:39`), the 100 ms one is the daemon's
  (`crates/navetted/src/bridge.rs:31`); `MediaInput` lives in
  `crates/navette-protocol/src/media.rs` (:365-385, bounds :416-420), not
  `crates/navetted/src/media.rs`; the injection code is
  `crates/navette-bridge/src/input.rs` (:147-245), not `navetted`;
  `bridge.rs:1221 buffer_scale: 1` is a **test fixture** (`surface_state` in
  `mod tests`), not a production hard-code — the production `scale_factor: 1`
  is `crates/navette-bridge/src/transport.rs:72`; pointer coordinates do
  **not** need dividing by the scale — `rescaleToContent` already does it
  from `contentSize`; the Android package is `com.greponlabs.navette`.
- **Why the display needs no work**: `H264Decoder` renders with
  `releaseOutputBuffer(index, true)` into the SurfaceView's Surface and never
  calls `setVideoScalingMode`, so MediaCodec's default scale-to-fit stretches
  the decoded frame to the view's bounds — the same mechanism that makes the
  "between resize and first new frame" window show a stretched picture today.
  Because the viewport is exactly `surface / fit` (even-rounded), the aspect
  ratios agree to within a pixel.
- **Why `Modifiers{ctrl}` alone would silently do nothing**: see the wprs
  fork at the pinned rev (`crates/navetted/Cargo.toml:28`,
  `~/.cargo/git/checkouts/wprs-a1177b03fe300706/38c61fe/src/server/client_handlers.rs:431-468`).
  The hardware path "works" with Ctrl because Android also delivers the
  physical `KEYCODE_CTRL_LEFT` as a key (`KeycodeMap.kt:166`), not because of
  the Modifiers message.
- **Follow-ups** (not in this plan):
  - *HiDPI (`wl_output` scale)*: `crates/navette-bridge/src/transport.rs:58-85`
    (`scale_factor`), wprs `compositor_utils::update_output`, `scene.rs:558`
    compositing honouring `buffer_scale`, `bridge.rs:790-818` encoder size
    (would double), Android `rescaleToContent` unchanged.
  - *Unicode text commit*: new `MediaInput::CommitText { text }` in
    `crates/navette-protocol/src/media.rs` + Kotlin `MediaProtocol.kt`,
    xkb keysym → keycode lookup (or a text-input protocol) in
    `crates/navette-bridge/src/input.rs`; `KeycodeMap.asciiCharToEvdev` stays
    the US-only fallback.
  - *Testable packet loop*: inject the packet-loop dispatcher into
    `SessionController` so `FakeMediaSessionClient.nextPacket()` can bootstrap
    a primary on the test scheduler and the key/IME paths get controller-level tests.
  - *`imeRaised` drift*: reconcile with `WindowInsets.ime` once
    `isImeVisible` leaves `@ExperimentalLayoutApi`.
