# Navette Android

M3 (docs/ROADMAP.md, Phase 1; docs/prp/PRP-plan.md §4.3, §6): project
scaffold, the drawer screen (Running sessions + Apps, talking to `navetted`'s
existing control-channel API), and the session screen -- MediaCodec H.264
decode onto a `SurfaceView` with touch, hardware-keyboard and on-screen-IME
input forwarded back over the media socket.

Deliberately still out: clipboard sync, multi-window (a session's non-primary
streams are ignored), long-press-as-right-click, pinch-zoom, auto-reconnect,
and portrait support -- the session screen is landscape-locked, because
`MediaInput::ViewportResize` is server-validated to a desktop-shaped
`320..3840` x `240..2160` that a phone's portrait size fails outright.

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
- `ui/session/` -- `SessionScreen` plus the pure input mapping it uses:
  `InputMapper` (touch/keys/IME text to `MediaInput`, viewport clamping,
  touch rescaling) and `KeycodeMap` (Android keycodes and printable ASCII to
  raw Linux evdev codes, transcribed from `linux/input-event-codes.h`).

## Verified

- `./gradlew clean assembleDebug testDebugUnitTest lintDebug` -- clean.
- Live wire-compatibility smoke test against a real running `navetted`
  (subprotocol negotiation, `list_apps`, `list_sessions`, and an error
  envelope for a `kill` on a nonexistent session) -- not just static
  fixtures.

## Not verified

No on-device or emulator run, for either slice: no AVD/emulator binary is
available in the environment this was built in (SDK, `adb` and `sdkmanager`
are present; `emulator` is not).

For the session screen that gap covers real behaviour, not just polish --
`MediaCodec` decode, `Surface` lifecycle, touch placement, and IME commit
handling have no JVM-testable surface at all. What *is* unit-tested is the
logic underneath them: the wire protocol, `StreamGate`, `AnnexB`,
`InputMapper`/`KeycodeMap`, and `MediaClient` against a `MockWebServer`.
Still needing a physical device:

- Video renders and updates live, and survives a stream reconfigure.
- Tap and drag land in the right place in the guest.
- A hardware keyboard types correctly (letters, Shift, punctuation, Enter,
  Backspace, arrows).
- The on-screen keyboard types correctly, including an autocomplete-triggered
  replacement.
- Killing `navetted` while attached shows a disconnected state, not a hang.
- Leaving the session returns to the drawer with the session still running,
  and the app is no longer landscape-locked afterwards.
- Rapid session entry and exit stays responsive: `surfaceDestroyed` can block
  briefly on a codec start in progress, which is a deliberate trade against
  rendering into a released `Surface`.

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
- Package `com.greponlabs.navette`, per `docs/prp/PRP-plan.md`'s stated org
  (Grepon Labs LLC).
