# Navette Android

M3 (docs/ROADMAP.md, Phase 1; docs/prp/PRP-plan.md §4.3, §6): project
scaffold, the drawer screen (Running sessions + Apps, talking to `navetted`'s
existing control-channel API), and the session screen -- MediaCodec H.264
decode onto a `SurfaceView` with touch, hardware-keyboard and on-screen-IME
input forwarded back over the media socket.

Deliberately still out: clipboard sync, multi-window (a session's non-primary
streams are ignored), middle-click, auto-reconnect, and portrait support --
the session screen is landscape-locked, because `MediaInput::ViewportResize`
is server-validated to a desktop-shaped `320..3840` x `240..2160` that a
phone's portrait size fails outright.

## What's here

- Gradle/Kotlin/Compose project skeleton (`app/`), Material 3, minSdk 26.
- `protocol/ControlProtocol.kt` -- Kotlin mirror of
  `crates/navette-protocol/src/lib.rs`'s control-channel wire types. The
  wire shape (flatten + internal tagging) isn't natively expressible with
  kotlinx.serialization's structural polymorphism, so `ControlCodec`
  hand-builds/parses the JSON directly. Round-trip tests in
  `src/test/kotlin` use the exact fixtures `navette-protocol`'s own Rust
  tests assert against -- if the wire shape ever changes there, these are
  meant to drift out of sync loudly, not silently.
- `net/NavetteClient.kt` -- one WebSocket connection to a `navetted` host,
  request/response correlated by `request_id`. No reconnect/backoff, no
  multi-host registry -- out of scope for this slice.
- `net/MediaProtocol.kt` -- Kotlin mirror of
  `crates/navette-protocol/src/media.rs`: the 44-byte big-endian frame
  header, `StreamConfig`, and the `MediaInput` JSON types, with every
  validation bound the bridge enforces reproduced client-side. Its tests use
  the Rust module's own fixtures, same as the control protocol's.
- `net/MediaClient.kt` -- the media WebSocket: binary frames in as
  `MediaPacket`s, `MediaInput` out as JSON text.
- `media/` -- `StreamGate` (picks one stream per session and ignores the
  rest), `AnnexB` (pulls SPS/PPS out of `codec_config` for `csd-0`/`csd-1`),
  and `H264Decoder` (`MediaCodec` in async mode, decoding to a `Surface`).
- `ui/` -- three screens switched on state, not a navigation graph (Connect,
  Drawer, Session). Navigation Compose was considered and left out: with a
  back-stack no deeper than session-to-drawer, a third branch is simpler than
  a nav graph plus a dependency. Revisit at a fourth screen.
- `ui/session/` -- `SessionScreen` plus the pure logic it uses:
  `InputMapper` (touch/keys/IME text to `MediaInput`, viewport clamping,
  touch rescaling, scroll units), `KeycodeMap` (Android keycodes and
  printable ASCII to raw Linux evdev codes, transcribed from
  `linux/input-event-codes.h`), `GestureInterpreter` (the multi-touch state
  machine, as a pure `(state, event) -> (state, effects)` step) and
  `ViewTransform` (the client-side zoom/pan model and its maths).

### Gestures

| Fingers | Gesture | Effect |
|---|---|---|
| 1 | tap | left click |
| 1 | drag | pointer drag (button held) |
| 2 | pinch | zoom the view, 1x to 4x, anchored to the fingers |
| 2 | drag, zoomed in | pan the view |
| 2 | drag, at 1x | scroll the guest (`MediaInput::PointerAxis`) |
| 2 | quick, still tap | right click at the first finger |

One finger always drives the guest pointer; two always drive the view or
the guest's scroll wheel. Zoom is entirely client-side -- a scale and
translate on the `SurfaceView`. The guest window, the encoder and the wire
format are untouched, which is why this slice changed no Rust at all, and
also why zooming past 1:1 is soft: the frames are the same size as before
and the GPU upscales them.

### Reconnect

A dropped media socket no longer strands the session on a dead screen. The
socket is pinged every 5s, so a silently-dropped tailnet route surfaces as a
failure within seconds; the screen then shows "Reconnecting..." and retries
up to five times on a linear backoff (1s, 2s, ...). Each retry is a clean
rebuild -- a fresh controller and `SurfaceView` via a Compose nonce, reusing
the same lifecycle a first attach uses rather than re-opening a torn-down
socket -- and the server replays codec config and the latest keyframe to
every new attachment (`crates/navetted/src/media.rs`), so a retry that lands
while the session is still alive resumes the picture on its own. Zoom and pan
survive the rebuild. If the budget runs out, a manual "Reconnect" button
takes over. The guest window closing (a real `StreamEnd`) and a decoder
failure are terminal and never retried automatically.

The rules live in `ReconnectPolicy` (pure, unit-tested); the screen owns the
counters. Two of its properties are worth knowing because each was a real bug
once:

- **The retry loop runs only while the screen is in the foreground**
  (`repeatOnLifecycle(STARTED)`), and the budget refills every time it
  returns there. Folding the phone raises the keyguard and stops the app with
  its network restricted; a loop that kept retrying behind it burned every
  attempt on 10s connect timeouts, so swiping back landed on a dead
  "Disconnected" screen. Now it pauses, and reconnects fresh on resume.
- **The budget refills on a decoded frame, not on the socket opening.** A
  server that accepts the socket and closes it at once would satisfy an
  open-based reset on every attempt and be retried forever.

## Verified

- `./gradlew clean assembleDebug testDebugUnitTest lintDebug` -- clean.
- Live wire-compatibility smoke test against a real running `navetted`
  (subprotocol negotiation, `list_apps`, `list_sessions`, and an error
  envelope for a `kill` on a nonexistent session) -- not just static
  fixtures.
- On a real device (Pixel 9 Pro Fold, Android 17, against a live `navetted`
  with a Firefox session -- see `docs/HANDOFF.md`): the connect, drawer and
  attach flow; `MediaCodec` decode of the real VA-API stream; landscape lock
  and its release; the disconnected state when `navetted` dies mid-session;
  the on-screen keyboard toggle; and, after the `client_id`-as-`ULong` fix,
  one-finger tap and drag landing where they should in the guest.
- On the same device, the two assumptions the gesture slice is built on,
  each measured before anything was built on it: a tap at screen `x=2400`
  under a static 2x view scale arrived at the touch listener as `x=1200`, so
  touch coordinates are inverse-mapped through the view transform by the
  framework; and the video layer genuinely follows a `SurfaceView`
  scale+translate (the picture was magnified, the Compose overlay was not),
  so no `TextureView` fallback is needed.
- **Every gesture, on a Pixel 10 Pro Fold (Android 17), against a live
  Firefox session** -- driven by synthetic multi-touch through
  `/dev/uinput` (`adb shell uinput -`), since `adb shell input` has no
  multi-pointer form. To give the virtual touchscreen a live viewport to
  bind to, the fold was forced open (`adb shell cmd device_state state 2`)
  so the rotation-stable 2076x2152 inner display was the active default;
  the virtual panel was sized to it so device coordinates map 1:1 to screen
  pixels. Confirmed by screenshot at each step, with no `MediaClient`
  rejection at any point:
  - One-finger tap navigated a link (fast-tap path); one-finger drag
    selected text.
  - Pinch zoomed in to ~2.4x, anchored to the focal point; pinch back out
    snapped to exactly fit-to-screen with the pan reset.
  - Two-finger drag while zoomed panned, clamped to the content edges.
  - Two-finger drag at 1:1 scrolled the guest, content following the fingers
    (correct direction). Magnitude is on the fast side -- ~400px of travel
    scrolled a full page -- but usable; `SCROLL_UNITS_PER_PIXEL` is the knob.
  - Clicking while zoomed 2.16x landed precisely on Firefox's "Restore
    Session" button.
  - Two-finger tap opened Firefox's own right-click context menu at the tap
    point, app staying foreground.
- **Reconnect, on the same device.** With the video live, the phone's wifi
  was dropped: the socket failed within seconds, the screen showed
  "Connecting.../Reconnecting...", and after wifi returned the video resumed
  **on its own** with no interaction. Separately, a longer outage exhausted
  the retry budget, surfaced the manual "Reconnect" button, and a tap on it
  rebuilt the connection and resumed the video. No bridge rejections either
  time.
- **Fold cycle, on the same device, three consecutive runs.** Attached on the
  cover display, then unfolded (a live resize -- the decoder restarted at
  the inner display's size with no socket drop), folded back (the fold raises
  a keyguard and stops the app; the socket dies behind it), and swiped back
  in: the video returned on its own every time, with no manual button, and
  the decoder followed with a second live resize back to the cover's size.
  Before the lifecycle-aware retry loop this exact cycle ended on a dead
  "Disconnected: failed to connect ... after 10000ms" screen.

- **The performance HUD, on a Pixel 10 Pro Fold (Android 17), against a live
  Firefox session.** Toggled by a two-finger long-press held past 250ms
  (`TAP_TIMEOUT_MS`), both fingers still; repeating the same gesture hides it
  again. Driven by a synthetic `/dev/uinput` two-finger touchscreen (`adb
  shell uinput -`) calibrated against the device's real touch-coordinate
  transform, confirmed with the `pointer_location` debug overlay before
  relying on it. The seven fields, each observed changing as expected:
  - `FPS`/`KBPS` climbed with an actively-scrolling guest (16.7fps / 2746kbps)
    and decayed to `0.0`/`0` within a few seconds of the guest going idle.
  - `DEC` (decode time) and `AGE` (time since the last decoded frame) moved
    independently of the above, staying live during idle periods.
  - `RTT` read a plausible tailnet figure (20-63ms across several samples)
    and turned blank (`--`), not frozen or zero, within about five seconds of
    the media socket's ping/pong going unanswered (`RTT_STALE_MS`) -- both
    against a dropped link and against a daemon that doesn't understand `ping`
    at all (see Known limitations).
  - `DROP` read `0` within every controller's lifetime -- an ordinary attach,
    a resize, and after a reconnect. A real reconnect rebuilds the
    controller and resets every counter (`DISC` was seen going 6 → 0 → 2
    across one), so a post-reconnect `DROP 0` covers only the new
    attachment, not the session as a whole.
  - `DISC` incremented exactly once for a real live resize (forced via
    `adb shell cmd device_state state 2/reset`, which also reproduces the
    fold-triggers-a-keyguard behaviour noted below) and did not increment for
    input or idle activity alone.
  - A quick two-finger tap (under 250ms) still opened the guest's own
    right-click context menu at the tap point -- the HUD gesture did not
    regress the existing two-finger tap.
  - **12 real enter/leave cycles** against the live session (each confirmed
    by a fresh `H264Decoder: decoder started at ...` logcat line, not just a
    key event -- an earlier pass that only counted key events had silently
    drifted off-app after a `KEYCODE_BACK` past the drawer and measured
    nothing) produced no hang, no ANR, and no surface-abandon warning; same
    process the whole time. This is the reproduction for the codec-callback/
    teardown lock hazard fixed earlier in this work.
  - A wifi drop (~8s) climbed `AGE`, blanked `RTT`, and the session recovered
    on its own once wifi returned, consistent with the existing reconnect
    behaviour above.

## Not verified

No AVD/emulator binary is available in the environment this was built in
(SDK, `adb` and `sdkmanager` are present; `emulator` is not).

What *is* unit-tested is the logic underneath the screen: the wire
protocol, `StreamGate`, `AnnexB`, `InputMapper`/`KeycodeMap`,
`GestureInterpreter`, `ViewTransform`, and `MediaClient` against a
`MockWebServer`.

Still open from the session-screen slice:

- A physical Bluetooth/USB keyboard as such, plus Enter and the arrow keys.
  (adb-injected key events, which take the same `onKeyEvent` path, covered
  letters, Shift, `,` `!`, space and Backspace -- see Verified.)
- The on-screen keyboard types correctly, including an autocomplete-triggered
  replacement. (A live resize while attached, and surviving the stream
  reconfigure it causes, is now covered: unfolding the Pixel 10 mid-session
  is exactly that, and it works -- see Verified.)
- Rapid session entry and exit stays responsive: `surfaceDestroyed` can block
  briefly on a codec start in progress, which is a deliberate trade against
  rendering into a released `Surface`.

Not yet exercised with real fingers (injection covered the logic; a human
should still sanity-check feel): pinch/pan/scroll smoothness under a real
hand, and that no stray click reaches the guest when a pinch starts.

## Known robustness notes

- The media socket now auto-reconnects (see Reconnect above), so a dropped
  link recovers on its own. A `MainActivity` recreation -- which moving the
  app between the fold's inner and cover displays forces, since a display
  change is not in the activity's `configChanges` -- still tears the whole
  screen down and back up rather than reconnecting in place; that is a
  heavier event than a socket drop and is not what reconnect targets. It does
  not arise in normal single-display use.
- When the app dies without cleanly closing its media socket, the stale
  attachment is released host-side on the socket's own close; a fresh attach
  gets a new `client_id` regardless (multiple attachments per session are
  supported), so this is at most a brief transient, not a stuck slot.

## Known limitations

- **Zoom past 1:1 is soft.** It is a client-side upscale of the frames the
  bridge already sends. Sharp zoom would need a crop/render-region protocol
  message and encoder work, and was deliberately not built.
- **The guest cannot be scrolled while zoomed in** -- pinch back to 1:1
  first. The two-finger drag mode is latched from the zoom level when the
  second finger lands (zoomed: pan; 1:1: scroll) and never changes
  mid-gesture. The intended fix, deferred: let a pan that hits the content
  edge in one axis start scrolling the guest in that axis.
- **A left press is sent 60ms after the finger lands, not immediately**
  (`PRESS_ARM_MS`). That is what lets a second finger cancel it, so a pinch
  no longer begins with a stray left click; a drag therefore starts a frame
  or two later than it used to. A tap shorter than the window still clicks.
- **The HUD's `DEC` includes codec queueing and is not comparable to the
  desktop viewer's `DEC`.** The Android figure is measured around the whole
  `MediaCodec` async round trip (submit to callback), where the desktop
  viewer's is a narrower decode-only measurement. Don't read them side by
  side as the same metric.
- **The HUD's `KBPS` counts payload bytes; the desktop viewer's counts
  payload plus the 44-byte media header** (`client.rs:342`). Under 1% at real
  bitrates, so it changes no reading anyone acts on, but the two figures are
  not byte-identical if you diff the clients.
- **`RTT` is answered by `navetted`'s media socket task, not the bridge
  loop**, so it stays low even while the bridge loop itself is stalled or
  falling behind -- a healthy `RTT` next to a climbing `AGE` means exactly
  that: the transport is fine, the pipeline behind it isn't keeping up. This
  is why `AGE` sits on the overlay beside it rather than `RTT` alone being
  trusted as the latency signal.

## Building

```sh
cd android
./gradlew assembleDebug
```

Needs `local.properties` with `sdk.dir=<path to Android SDK>` (git-ignored,
machine-specific -- not checked in).

## Deliberate choices worth knowing

- **Cleartext (`ws://`) is intentional**, not an oversight -- see the
  comment on `app/src/main/res/xml/network_security_config.xml`. It matches
  `navetted`'s own security model: the control API binds to a tailnet
  address only and never spoke `wss://` either. TLS on top of an
  already-WireGuard-encrypted tailnet wouldn't move the actual trust
  boundary, which is tailnet membership.
- **No Navigation Compose dependency** -- three screens switched on state,
  not a nav graph. See `ui/` above for why; revisit at a fourth screen.
- **Full codec teardown on every stream reconfigure**, rather than
  `MediaCodec`'s adaptive playback. Adaptive needs a max-size hint committed
  upfront and is device-conditional, so it would need this path as a fallback
  anyway; the Rust `StreamRouter` this mirrors also rebuilds rather than
  resizing in place. Cost is a black flash on resize, against a server-side
  reconfigure that already costs hundreds of milliseconds.
- **The gesture interpreter works in screen space, and the zoom transform is
  applied to the `SurfaceView` synchronously from the touch path** -- not
  through Compose state and an `AndroidView` `update` lambda. The framework
  inverse-maps every touch through the view's matrix before the listener
  sees it (measured, see Verified), so the controller lifts each pointer back
  into screen space using the transform it believes is applied. The two must
  be the same matrix at that instant; a transform that landed a frame later
  via recomposition would have every pinch step read the fingers through a
  stale one. In local space, a finger that has not moved would appear to
  move whenever the zoom changed under it -- a feedback loop.
- Package `com.greponlabs.navette`, per `docs/prp/PRP-plan.md`'s stated org
  (Grepon Labs LLC).
