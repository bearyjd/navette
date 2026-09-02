# Navette Android

M3's first slice (docs/ROADMAP.md, Phase 1; docs/prp/PRP-plan.md §4.3, §6):
project scaffold + the drawer screen (Running sessions + Apps, talking to
`navetted`'s existing control-channel API). The session screen -- MediaCodec
H.264 decode, touch/keyboard input, clipboard, resize-follows-viewport -- is
a separate, larger slice, not started here.

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
- `ui/` -- two screens, not a navigation graph yet (Connect, Drawer). A
  third screen (session) is the natural trigger to add real navigation.

## Verified

- `./gradlew assembleDebug testDebugUnitTest lintDebug` -- clean.
- Live wire-compatibility smoke test against a real running `navetted`
  (subprotocol negotiation, `list_apps`, `list_sessions`, and an error
  envelope for a `kill` on a nonexistent session) -- not just static
  fixtures. No on-device or emulator run yet; no AVD/emulator binary was
  available in the environment this was built in (SDK, `adb`, and
  `sdkmanager` are present; `emulator` is not).

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
- **No Navigation Compose dependency yet** -- two screens switched on by a
  connection-state `if`, not a nav graph. Worth adding once the session
  screen exists.
- Package `com.greponlabs.navette`, per `docs/prp/PRP-plan.md`'s stated org
  (Grepon Labs LLC).
