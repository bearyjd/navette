# Handoff — M2 encoder bridge + viewer, as of 2026-08-28

## Where things stand

**M2 is merged to `master` and has now actually been run against a real
Wayland session for the first time — repeatedly, over two sessions.** The
first run found a scene-graph bug that made the whole pipeline
non-functional; that fix is committed and pushed (`f243047`). A second,
much longer live-testing session (human at the keyboard, not scripted)
found and fixed a pointer-alignment bug and dug deep into a keyboard
input bug that turned out to be two real bugs in the vendored `minifb`
crate itself — see "Live human-testing session (2026-08-28)" below for
the full story. **That session's fixes are uncommitted in the working
tree as of this writing** — `git status` will show 6 modified files.
One symptom (rapid typing during/after a resize still repeats
characters) is confirmed **not yet fully fixed** — see "Still open" at
the end of that section for exactly what's known and what isn't.

PR #1 through #6 (the whole encoder-bridge milestone) are all merged.
`master` is at `f243047`. No open navette PRs. One open **upstream** PR:
[emoon/rust_minifb#429](https://github.com/emoon/rust_minifb/pull/429) —
track this until merged, see that section for what to do once it lands.

## Real-hardware verification (2026-08-26)

Ran `navetted` + a freshly-built `wprsd`/`wprsc` (pinned rev
`5763d7464ac76103fd407921e711b17a2aac35b3`, matching `navette-bridge`'s
`Cargo.toml` at the time; the pin has since moved to `38c61fe`, see "wprs
allocation ceilings: landed") against this machine's live KDE Plasma Wayland session, and
`navette run org.mozilla.firefox` for real. This is the first time any of
this has touched real hardware.

**Found and fixed a bug that made the encoder bridge produce zero frames for
any real application, ever.** `wprsd`'s own `commit_impl` (upstream
`server/smithay_handlers.rs:771`) always appends a surface's own id to its
`z_ordered_children` list, marking where its own buffer sits in the
subsurface stacking order — this is documented in wprs's own `// TODO:` next
to the line. `navette-bridge`'s `Scene::apply_surface`
(`crates/navette-bridge/src/scene.rs`) built its `children` list directly
from that list without filtering the self-entry, so every toplevel's own
children list always included itself. `composite_children`'s cycle guard
(correctly) treats a surface being its own child as `SceneError::SurfaceCycle`
and aborts the composite — silently, since the caller
(`handle_scene_events` in `crates/navetted/src/bridge.rs`) swallows a
`compose_toplevel` error via `let Ok(frame) = ... else { continue }`. Net
effect: real surface commits flowed through the whole pipeline correctly
(confirmed via packet-level tracing — real 1332×772 Firefox buffers, correct
toplevel/subsurface role assignment) but composition failed on literally
every attempt, so no frame was ever encoded, no `ffmpeg` encoder process ever
spawned, and `navette-viewer` never received a single `StreamConfig`. No
error was ever logged anywhere, which is why 145 tests and three review
passes never caught it — every test fixture in `scene.rs` was constructed by
hand and none of them included this self-referencing entry, because nobody
knew real `wprsd` traffic contains one.

Fix: filter `child.id != state.id` when building `Scene::apply_surface`'s
children list (one line). Added a regression test,
`a_surfaces_self_entry_in_z_ordered_children_does_not_trip_the_cycle_guard`,
that mirrors the existing `composites_subsurface_with_alpha_and_clipping`
fixture but includes the self-entry — confirmed it fails with
`SceneError::SurfaceCycle` without the fix and passes with it.

After the fix, re-ran the same real Firefox session end to end and confirmed,
for the first time: `navette-bridge` spawns a real VA-API `ffmpeg` encoder
(`h264_vaapi` on `/dev/dri/renderD128`), `navette-viewer` spawns a real
decoder, opens a real native `minifb` window (1306×750, logged as "opened a
window for a new toplevel"), and streams real frames (`FPS 50.7 KBPS 1325`
during Firefox's initial paint, settling to `FPS 0.0` once Firefox stopped
repainting — expected idle behavior, not a failure). Full workspace test
suite (145 + 1 ignored, matching the prior baseline), clippy, and fmt all
still pass after the fix.

**Not yet verified:** the desktop this ran on was screen-locked throughout
(confirmed via `busctl --user call org.freedesktop.ScreenSaver ...
GetActive`), so nobody has visually looked at the window, clicked it, typed
into it, or resized it. The evidence above is log/process/wire-level, not a
human at a keyboard. That's still the literal next step. Also found, not
fixed: `wprsd`'s XWayland spawn fails in this environment
(`No such file or directory`) — doesn't block native-Wayland apps like
Firefox, but would block anything needing X11 compat.

The fix is committed (`f243047`) and pushed to `origin/master`.

## 30-minute endurance run (2026-08-26, post-fix)

Ran the fix against a fresh Firefox session for a full 30 minutes, driven by
a scripted client connected directly to the media WebSocket (no human
involved, so this covers the pipeline-level portion of PR6's endurance
criterion, not the visual one): ~20 viewport-resize cycles alternating
1024×600/1280×720, periodic keyframe requests, and pointer-motion input once
the stream's real `client_id`/`surface_id` were known from the daemon's own
`StreamConfig`.

Results: 236 video packets decoded, 31 successful stream (re)configs across
every resize, **zero** stream-end events, **zero** WebSocket reconnects,
**zero** client-observed errors, over the full 1800s. RSS sampled every 60s
across `navetted`, `wprsd`, Firefox, `navette-viewer`, and both `ffmpeg`
processes (encode + decode): all five oscillate with resize/keyframe events
but show no monotonic growth — `navette-viewer` in particular flattens
completely after ~4 minutes and stays flat for the remaining 25. Firefox's
own RSS grew ~4% over 30 minutes, consistent with ordinary browser
background activity, not a Navette-side leak. The daemon log's only panic
entries are the pre-existing, unrelated XWayland-spawn failure noted above.
No crashes, no restarts, no manual intervention needed the entire run.

This leaves PR6's endurance criterion functionally satisfied at the
pipeline level; only the LAN/tailnet latency measurement (needs a second
physical host) and the literal human-at-a-keyboard look remain.

## Tailnet latency measurement (2026-08-26)

Ran a real remote-host latency probe against the fix: a genuine second
machine (a DigitalOcean droplet in NYC3, reached at its Tailscale address)
connected to `navetted`'s media WebSocket through an SSH reverse tunnel
(`ssh -R 9417:127.0.0.1:9417`, the same transport pattern the M1 operator
guide already documents for cross-machine control). The remote host had
no `pip`/`websockets` and no passwordless `sudo` to install one, so the
probe is a ~200-line dependency-free WebSocket client (stdlib `socket` +
hand-rolled RFC 6455 framing) — kept in this session's scratchpad, not the
repo, since it's a one-off measurement tool rather than project code.

Baseline: `ping` to the droplet over the tailnet averaged 24ms (min 12ms,
max 113ms outlier, mdev 22ms) across 20 samples — real WireGuard-tunneled
network latency, not loopback.

A bare `request_keyframe` turned out not to be a useful latency probe:
it only sets a flag consumed on the *next* app repaint, and Firefox was
sitting idle (no repaints) by the time the probe connected, so most
requests never got a timely response. Switched to viewport resize instead
— confirmed during the endurance run above to reliably force a real
recomposite + encoder reconfigure — and timed the round trip from sending
`viewport_resize` to receiving the resulting `stream_config`. 7 clean
samples, tightly clustered: **768–832ms, avg ~796ms** (one early sample
timed out at >5s, most likely Firefox's background-tab paint throttling
on its first repaint after being idle since before the probe connected;
every resize after that landed in the same tight band). Zero unexpected
stream-ends, zero connection errors, one harmless probe-side timeout
already explained above.

Read on that number: it's dominated by application-side cost, not
network transit. The 24ms network RTT is a small fraction of the ~796ms
total — the rest is `navette-bridge`'s deliberate 100ms resize debounce
(`RESIZE_DEBOUNCE` in `crates/navetted/src/bridge.rs`) plus real Firefox
relayout/repaint time. That's a legitimate, useful data point for anyone
building the actual phone client: a resize won't feel instant, and a
"drag to resize" UI would want some transitional/skeleton treatment
rather than assuming ms-scale response.

Cleaned up: local `navetted`/session, the SSH reverse tunnel, and the
probe script on the remote host are all torn down. Nothing left running.

**With this, PR6's evidence gathering is functionally complete except the
literal human-at-a-keyboard look** — everything else in the gate
criteria (endurance, LAN/tailnet latency) now has real numbers behind it.

```
f907717 perf(bridge): O(1) scene ancestor resolution + CI display coverage (#6)
bbb319e feat(viewer): show sessions on screen with input, resize, and a HUD (#5)
8da5bda feat(bridge): encode bounded H.264 streams (#4)
722fa3a feat(media): add bounded session routing (#3)
47586d6 feat(bridge): capture and compose wprs scenes (#2)
b71b9f3 docs: design M2 encoder bridge (#1)
```

`cargo test --workspace`: green, 145 tests (1 correctly `#[ignore]`d
outside a display; CI's `viewer-display` job runs it under Xvfb).
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo fmt --all -- --check`: clean.

## What actually works right now

- `navetted` supervises named wprs sessions over a persistent registry,
  with a versioned WebSocket control API and an XDG app index.
  `navette-cli` drives it end to end (`ls/run/attach/detach/kill`).
- The bridge (`navette-bridge` + `navetted::bridge`) captures a session's
  live Wayland scene, composites each toplevel (including subsurfaces and
  popups), and H.264-encodes it (VA-API with libx264 fallback) as its own
  stream — one stream per open window, not one per session.
- Client input (pointer/keyboard/resize) is validated, scoped to the
  correct surface, and translated into real wprs protocol events.
- `navette-viewer` is a real Linux client: connects to a session's media
  WebSocket, decodes each stream, opens one native window per toplevel
  (via `minifb`), forwards input, and shows a performance HUD. **This is
  the piece that didn't exist at the start of this session** — building
  and hardening it was the bulk of the work.

## The one thing that has never been verified

**Superseded by "Real-hardware verification" above.** Short version: the
first real run found the encoder bridge produced zero frames for any real
app (fixed). What's left of the original gap is narrower now: a human
still hasn't looked at, clicked, or typed into the window — everything
verified above is log/process/wire-level, on a screen-locked desktop.

## Known gaps, deliberately not fixed

Two things were found and explicitly deferred rather than fixed blind:

1. **Manual GUI exercise + PR6 gate criteria** — the M2 plan's own exit
   bullet ("Firefox and Claude Desktop both display, accept input,
   resize, detach, and reattach") plus the 30-minute endurance run and
   LAN/tailnet latency measurements. All need real hardware. See
   `docs/superpowers/plans/2026-08-23-m2-encoder-bridge.md`'s PR6 section
   for exactly what the gate report needs.
2. Everything else from the multiple review passes this session (~30
   Minor findings — rate-limited logging, a couple of dead branches,
   doubled key auto-repeat, etc.) is documented in this session's PR
   descriptions and review comments on #5/#6, not tracked anywhere more
   durable. If you want these as tracked issues rather than scrollback,
   that's worth doing before they're lost.

## Where to look

- `docs/ROADMAP.md` — the market-informed feature roadmap (Phases 0-4).
  M2 = Phase 1 ("the demo"). Phase 2 ("daily driver") is table stakes:
  file transfer, clipboard, wake-on-LAN, multi-host registry.
- `docs/superpowers/plans/2026-08-23-m2-encoder-bridge.md` — the M2 plan,
  including PR6's exact gate criteria.
- `docs/superpowers/specs/2026-08-23-m2-encoder-bridge-design.md` — the
  design spec PR1-6 implement.
- PR #5 and #6 on GitHub carry the real review history — what was found,
  what was fixed, what was ruled acceptable and why. Worth reading before
  touching `crates/navette-bridge/src/scene.rs` or
  `crates/navette-viewer/src/native.rs` specifically; both have real
  subtlety documented in their doc comments and commit messages.

## Live human-testing session (2026-08-28)

The human-at-a-keyboard verification the previous section called out as
missing finally happened — and found real bugs, exactly as intended.
Session used fresh `navetted`/`navette-viewer` builds against a real
Firefox window on this machine's own KDE Wayland desktop.

### Bug 1 (fixed, committed nowhere yet): pointer misalignment after resize

**Symptom**: after resizing the `navette-viewer` window, clicks landed in
the wrong place.

**Root cause**: `crates/navette-viewer/src/native.rs`'s `pointer_motion()`
read `minifb::Window::get_mouse_pos()`, which reports coordinates in the
*live OS window's* pixel space — continuously updated by minifb on every
poll, independent of whatever buffer is actually being displayed. After a
resize, there's a real gap (measured ~800ms server round-trip: 100ms
debounce + Firefox relayout/repaint + encoder reconfigure) during which
`minifb` visually stretches the *old, stale-sized* buffer to fill the
*new* window size. A click during that gap reported coordinates in the
new window's pixel space, but the server clamped them against the old
frame's dimensions.

**Fix**: added a `content_size` field to `NativeWindow`, updated only when
a frame is actually presented (the ground truth for "what's on screen
right now"), and rescale reported pointer positions against that instead
of the live window size — see `rescale_to_content()` in `native.rs`, with
4 new unit tests. Confirmed working live after the fix.

### Bug 2 (fixed, PR open upstream): keyboard repeat and stuck keys

This one took several rounds to actually root-cause. Worth reading in
full if picking this back up, since three real, independent bugs were
found (two in `minifb`, not navette's own code) and the investigation
methodology is as important as the fixes.

**Round 1 symptom**: typing "test" produced "teeeeeeeeeestttttttttttt".

**Round 1 fix**: `native.rs:318` passed `KeyRepeat::Yes` to minifb's
`get_keys_pressed()`. minifb's own typematic repeat (250ms delay, then
~20Hz) was being forwarded as brand-new `WindowEvent::Key{pressed:true}`
events, each becoming a genuine new `wl_keyboard.key Pressed` sent all the
way to Firefox — but Wayland key repeat is the *receiving client's* job,
not the transport's. Changed to `KeyRepeat::No` (reports a key exactly
once per physical press). This fixed the severe flood.

**Round 2 symptom**: after that fix, typing still occasionally repeated,
and specifically "after a resize... stuck and repeating."

**What found the real cause**: a devil's-advocate review (`oh-my-claudecode:critic`
agent, opus) instructed to find what the round-1 fix *didn't* explain,
plus live instrumentation (temporary `tracing::debug!`/`eprintln!` calls
at every press/release/redelivery point, since removed). The review is
worth re-reading in full if this area breaks again — it correctly
predicted a falsifiable test ("type a capital letter — if nothing
appears, the keysym table is the cause") that confirmed the mechanism
before any fix was written.

**Root causes found, ranked by what's fixed vs still-open**:

1. **(Fixed, upstream PR open)** minifb's Wayland backend (`handle_key` in
   `os/posix/wayland.rs`) only matches *unshifted* keysyms — lowercase
   letters, unshifted punctuation. Every shifted keysym (capitals, `!`,
   `@`, etc.) falls through to `_ => return` and is silently dropped
   before `set_key_state` ever runs. Two consequences: Shift+key doesn't
   register at all, and — the actual stuck-key mechanism — a
   press/release pair straddling a Shift transition (ordinary fast-typing
   rollover: press a letter, press Shift for the next word, release the
   first letter while Shift is still down) drops only the *release*,
   leaving that key permanently "held" from Firefox's perspective, which
   then repeats it via its own timer forever.
2. **(Fixed, upstream PR open)** `update_with_buffer_stride` in the same
   backend returned early via `?` on a failed present, *before* ever
   calling `self.update()` — the sole call site that advances minifb's
   internal key-repeat timers. A failed present froze a held key's timer,
   so the next successful poll saw the same held key as "freshly pressed"
   again.
3. **(Fixed, our code)** `crates/navette-viewer/src/client.rs`'s
   `send_input` intentionally never blocks (documented, deliberate — see
   its doc comment on why blocking would deadlock the poll loop) and
   silently drops on a full 64-slot queue. A dropped *release* is
   unrecoverable mid-session. Fix (`main.rs`): a small retry queue
   (`pending_redelivery`) specifically for releases — everything else
   keeps the original fire-and-forget drop, only releases get retried
   until they land.
4. **(Fixed, our code)** `crates/navette-bridge/src/input.rs`'s
   `KeyboardKey` handler had no dedup — any client-side duplicate press
   was forwarded verbatim to the wire. Fixed: a redundant press (already
   tracked held, for the same attachment) is now a no-op instead of being
   sent. Also added `InputState::release_all_held`, called from
   `crates/navetted/src/bridge.rs`'s `run_bridge` right before it exits —
   a bridge worker restart (transport blip) used to build a fresh,
   empty `InputState`, forgetting whatever keys were held without ever
   releasing them. Best-effort flush through the dying transport now,
   rather than silent loss.
5. Also cleaned up: all temporary `TEMP-DIAG` instrumentation removed.

**The fix for #1 lives in a forked, patched `minifb`**, since it's a bug
in the third-party crate, not navette's own code — same pattern this
project already uses for `wprs`. Fork:
[bearyjd/rust_minifb](https://github.com/bearyjd/rust_minifb), branch
`navette-wayland-key-fixes`, two commits (one per fix, #1 and #2 above).
`crates/navette-viewer/Cargo.toml`'s `minifb` dependency is pinned to
that branch's commit SHA. **Upstream PR is open:
[emoon/rust_minifb#429](https://github.com/emoon/rust_minifb/pull/429) —
track this until it merges.** Once merged: switch `navette-viewer`'s
`minifb` dependency back to a plain crates.io version pin (whatever
release first includes the merge) and delete the fork, or at minimum
stop depending on it. Until then, the fork is a real, ongoing
maintenance liability — check it doesn't silently drift from upstream if
this sits for a while.

### Verification status

- **Confirmed fixed, live**: pointer misalignment after resize. Capitals
  and shifted symbols now type correctly (direct confirmation that fix #1
  above is real and correct).
- **Confirmed NOT fully fixed**: "rapid typing during/after a resize
  still repeats characters" — reported again by the human tester *after*
  all five fixes above were live. This means there's at least one more
  mechanism, separate from the keysym bug, specifically tied to
  resize + rapid typing (not rapid typing alone — an isolation test to
  confirm this was in progress when this session ended; check whether
  that result ever came back before assuming resize is causal rather
  than just correlated with when the tester happens to type fast).

### Still open — leads for whoever picks this up

Not yet investigated with live evidence, but real candidates surfaced by
the same devil's-advocate review and not yet ruled out:

- **Tick starvation (review's "M1")**: `main.rs`'s poll loop ticks every
  8ms via `tokio::select!`, but decode/convert/HUD work runs synchronously
  on the same thread, and `MissedTickBehavior::Delay` means a missed tick
  *slips* rather than catches up. Under heavy load (a resize triggers a
  burst of re-encoded frames, `DISC` counts climb — see the endurance run
  data earlier in this doc for what a busy stream's HUD numbers look
  like), a release riding a slipped tick could land after the guest's own
  repeat delay has already elapsed, costing one extra repeated character
  per event even though the release does eventually arrive.
- **Redelivery lag under sustained burst**: fix #3 above retries a
  dropped release until it lands, but doesn't guarantee *how quickly*.
  If resize-triggered load keeps the input queue saturated for multiple
  poll cycles, a retried release could still be delayed long enough for
  Firefox's repeat timer to fire a few extra times before the (correct,
  eventually-delivered) release lands. This would look like "repeats a
  few times then stops," not "stuck forever" — worth asking the human
  tester specifically which of those two they're seeing, since it points
  at a different mechanism than a permanently-lost release.
- Add fresh, targeted instrumentation before hypothesizing further (Iron
  Law: evidence before fixes). A reasonable starting point: log when
  `pending_redelivery` (main.rs) actually holds something and how long
  until it drains, and log server-side when `InputState::apply` sees a
  release for a keycode that was never marked pressed (a sign of a
  reordering or a redelivery landing later than expected).

## Continued investigation, same day (2026-08-28, second pass)

Picked up "Finish root-causing 'rapid typing + resize still repeats'" from
the section above's "Still open" list, following the systematic-debugging
Iron Law (evidence before fixes, no hypothesizing further without it).

### Instrumentation added (uncommitted, now part of the working-tree diff)

Two targeted diagnostic logs, matching exactly what the "Still open"
section above called for:

1. `crates/navette-viewer/src/main.rs` — the poll loop now tracks tick lag
   (`tracing::debug!("poll tick fired late")` when a tick fires >4ms after
   `POLL_INTERVAL`; direct evidence for or against the "tick starvation"
   hypothesis) and redelivery lag (`"delivered a retried release"`,
   logging how long a release sat in `pending_redelivery` before it
   actually got through).
2. `crates/navette-bridge/src/input.rs`'s `InputState::apply` now logs
   (`tracing::debug!("keyboard release for a keycode not tracked as
   pressed")`) whenever a keyboard release arrives for a keycode this
   attachment never had tracked as pressed — a signature of reordering or
   a redelivery landing later than expected. This required adding
   `tracing` as a new dependency of `navette-bridge`
   (`crates/navette-bridge/Cargo.toml`); the crate previously did no
   logging of its own at all, relying on `navetted` to log at its call
   sites, but threading a signal back through `InputState::apply`'s
   `Result` for a case that isn't an error would have been a much bigger
   diff for a temporary diagnostic.

None of this instrumentation has produced evidence yet — see below for
why.

### Blocked before reaching live reproduction

The plan (confirmed with the user — full live rebuild, not a static-only
pass) was to rebuild everything with the new instrumentation, stand up a
real `wprsd`/`navetted`/Firefox session on this machine's own KDE Wayland
desktop again, and script a rollover-during-resize burst with `ydotool`
(kernel-level input injection, so it works regardless of minifb's backend
and genuinely mimics a human's fast typing) instead of needing an actual
human at the keyboard this time.

Got as far as confirming this machine has a live Wayland session
(`WAYLAND_DISPLAY=wayland-0`, KDE) and the previously-built
`navetted`/`navette-viewer` binaries in `~/.local/bin` and
`target/debug/`. Then hit three real setup gaps, in the order they'd need
solving:

1. **This machine no longer has `libxkbcommon-devel` installed** — only
   the runtime `.so.0` (`libxkbcommon-1.13.1-2.fc44`), not the dev package
   that provides the unversioned `libxkbcommon.so` the linker needs. This
   blocks `cargo build` for `navetted` (and anything else linking
   `wprs`/`smithay`) entirely, independent of anything this session
   changed — confirmed by reproducing the same `rust-lld: error: unable
   to find library -lxkbcommon` on a clean build attempt. Needs `sudo dnf
   install -y libxkbcommon-devel`; asked the user to run it (a system
   package install needs sudo, so not something to do unprompted).
   **Still not done as of this writing** — rechecked with `rpm -q
   libxkbcommon-devel` moments before this update and it's still "not
   installed".
2. **`wprsd`/`wprsc` are not built anywhere on this machine right now.**
   They were built from the `bearyjd/wprs` fork in an earlier session
   (the same fork `navette-bridge`/`navetted` pull as a library
   dependency, pinned in `Cargo.toml`) but that checkout/build is gone.
   Standing up any real session needs cloning and building that fork's
   own binaries fresh.
3. **No synthetic-input path is ready.** `xdotool` is installed but
   targets X11 and likely can't address a native-Wayland minifb window.
   `ydotool` is installed but its daemon (`ydotoold`) isn't running, and
   `/dev/uinput` is `root:root` with no group access for the current
   user — needs privilege setup (`sudo`, or a `udev`/group rule) before
   `ydotool` can inject anything.

None of these are code problems; they're environment setup, and (1) and
(3) both need root. Whoever picks this back up should either get those
three things sorted first, or fall back to the "you drive the live test
yourself" option that was on the table but not chosen this round.

## ROOT CAUSE FOUND: "rapid typing + resize repeats characters" (2026-08-28, third pass)

**The still-open symptom is now root-caused, with a reproduction, a fix, and
before/after measurements. It is a third bug in minifb's Wayland backend,
independent of the two already in the fork.** Nothing in navette's own code
was at fault for this one.

### The bug

`Window::update()` in minifb's `src/os/posix/wayland.rs` runs
`self.key_handler.update()` *before* applying the keyboard events it just
dispatched:

```rust
pub fn update(&mut self) {
    self.try_dispatch_events();        // this cycle's key events are queued
    ...
    self.key_handler.update();         // advances state from the PREVIOUS keys[]
    for event in self.input.iter_keyboard_events() { ... set_key_state(...) }
}
```

`KeyHandler::update()` does two things that belong on *opposite* sides of
event application:

- `keys_prev[i] = keys[i]` — the snapshot `is_key_index_released`
  (`keys_prev && !keys`) compares against. Must happen **before** this
  cycle's events, or a release written into `keys` makes both sides false
  and the edge is destroyed before anyone reads it.
- `keys_down_duration[i]` — `is_key_index_pressed` reports a key only while
  this is exactly `0.0`. Must happen **after** this cycle's events, or a
  key pressed in this batch is still at its initial `-1.0` at read time.

Because both run before the batch, a press is never reported on the cycle it
arrives — only if the key is *still down* at the next update. That is
harmless at an 8ms poll (a 50ms keypress spans ~6 cycles) and is why every
previous test passed. It breaks once a poll cycle gets long enough that the
release lands in the very next batch: the press is lost outright, an
unpaired release is reported, and a key whose last observed edge was a press
stays latched down with `is_key_down()` still returning `true` — so
`native.rs`'s `held_keys` stale-key self-heal cannot see it either. The
guest never receives the release and its own repeat timer runs forever.

**That is the reported symptom exactly.** A resize is what makes the cycle
slow (decode + convert + present + HUD all run synchronously on the poll
thread, against a burst of re-encoded frames), which is why the symptom is
tied to resize; two key edges have to land in one cycle, which is why it
needs rapid typing. All five earlier fixes were downstream of the point
where the event dies, and none of the three probes added in the second pass
can see it — the release never reaches `pending_redelivery` or the bridge.

### How it was reproduced (no wprs/Firefox stack needed)

A standalone harness (`keyprobe`, in this session's scratchpad, not the
repo) opens a minifb window and mirrors `NativeWindow::poll_events` exactly
— one minifb update per cycle, then `get_keys_pressed(KeyRepeat::No)` and
`get_keys_released()` read once each — while `ydotool` injects a scripted
key sequence at the kernel. It logs every edge and flags duplicate presses,
unpaired releases, and keys still held at exit. `KEYPROBE_LOAD_MS`
simulates the synchronous per-cycle work a resize burst causes.

The runs below inject **identical** input (20 taps, 50ms hold — ordinary
human speed). The only variable is poll-cycle duration:

| poll cycle | minifb | dup presses | orphan releases | key latched at exit |
|---|---|---|---|---|
| fast (8ms) | fork as-is | 0 | 0 | no |
| **60ms** | **fork as-is** | 0 | **10** | **yes** |
| 60ms | naive: move `update()` after events | **17** | 0 | **yes** |
| **60ms** | **two-phase split (the fix)** | **0** | **0** | **no** |
| fast (8ms) | two-phase split | 0 | 0 | no |

Row 2 is the bug. Row 3 is why the obvious one-line fix is wrong — moving
the whole `update()` after the batch fixes presses and breaks releases
instead, because `keys_prev` then syncs to the post-release state.

### The fix

Split `KeyHandler::update()` into `snapshot_prev()` (before the batch) and
`advance_durations()` (after), keeping `update()` as both in sequence so the
X11/macOS/Windows backends, which apply their key events outside that
window, are untouched. The Wayland backend calls the two phases around its
keyboard-event loop. Patch saved this session at
`docs/patches/minifb-two-phase-keyhandler.patch`; verified by rows 4-5 above.

One residual, inherent to minifb's level-based key model and *not* fixed:
if an entire press+release fits inside a single poll cycle, both edges net
out and neither is reported. That costs a dropped character — but the key
no longer latches, so it can never become an infinite repeat. Keeping the
viewer's poll cycle short is what shrinks that window, which is worth doing
on its own merits (a 60ms cycle is bad input latency regardless).

### Not yet done

- **UPSTREAM MERGED (2026-08-29).**
  [emoon/rust_minifb#429](https://github.com/emoon/rust_minifb/pull/429)
  landed as `7724f43`, carrying all three Wayland fixes. The fork is gone:
  `navette-viewer` now pins `emoon/rust_minifb` directly at
  `3a711add95bc6b9ffd4db09c93c3550178359905`. Still a git pin rather than a
  version pin, because the newest crates.io release (0.28.0, 2025-01-20)
  predates the merge — switch to a plain version as soon as one ships with
  it. That SHA is two commits past ours (#430 reworked keysym derivation,
  #431 gave `Key` an explicit `#[repr(u8)]`, formalising what navette's
  evdev mapping already relied on); since both touch key handling, the pin
  was verified against the 480-edge rollover harness first — zero duplicate
  presses, zero unpaired releases, nothing latched, every key balanced
  (T 84/84, E 50/50, S 40/40, LeftShift 35/35).
- **Superseded:** the fix was previously carried on the fork as `0b54200`. `crates/navette-viewer/Cargo.toml` is repinned to
  `0b542006be72498c3af9bccab6d7ff1553764e73`; workspace is green against it
  (151 passed, 1 ignored, clippy and fmt clean). A persistent clone of the
  fork now lives at `../rust_minifb` next to this repo.
- Not yet re-verified end-to-end against a real Firefox session with a human
  typing. The evidence above is at the minifb layer, which is where the bug
  lives and where the fix was measured — but nobody has yet typed into a
  real navette window during a resize and confirmed the symptom is gone.
  **That is the single remaining step to close this out.**
- **Moving decode/convert off the viewer's poll thread is not done, and is now
  known to be required rather than optional** -- see the measured poll-cycle
  numbers below (real cycles reach 626ms; 14 cycles in one 60s run were long
  enough to swallow a whole keypress). This is the top open item for M2.
- minifb binds `wl_keyboard`/`wl_pointer` without checking `wl_seat`
  capabilities (`wayland.rs:401`) and panics on any non-XKB keymap
  (`wayland.rs:1225`). Neither affects a normal desktop, but both make it
  impossible to run the viewer under a headless compositor -- which is exactly
  what CI would want. Not fixed; a fourth candidate for the fork if headless
  testing is ever wanted.

### Verification against the pushed commit

The before/after table above was measured against a locally patched working
copy. Re-verified afterwards against the **actual pushed** fork commit
`0b54200` (no local `[patch]`; cargo resolved it from git — confirmed in the
harness's `Cargo.lock`), at the same 60ms cycle, using the multi-key driver
whose phases include rollover across Shift:

- Every key observed balanced exactly: `T` 4/4, `LeftShift` 2/2, `E` 1/1,
  `S` 1/1 across two runs.
- Zero duplicate presses, zero unpaired releases, nothing latched at exit —
  at the cycle length that, before the fix, produced 10 unpaired releases
  and a latched key.

Those first runs were a thin sample, because injecting with `ydotool` on the
real desktop meant the probe window kept losing focus and the multi-key phases
never completed. **That is now fixed and the multi-key case is properly
measured.** The harness runs the probe inside a `sway` nested in the host
session and injects with `wtype` over `virtual-keyboard-v1`, which targets
sway's own seat -- so nothing depends on any window holding focus, and the
host desktop is untouched.

Three things had to be right for that to work, each worth knowing:

- **Nested, not headless.** A headless wlroots seat advertises neither
  keyboard nor pointer capability, and minifb binds both unconditionally
  (`wayland.rs:401`, `(seat.get_keyboard(), seat.get_pointer())`, no
  capability check) -- a protocol error that disconnects the probe before it
  sees a key. That is arguably a fourth minifb bug; it was worked around
  rather than patched, to keep the code under test unmodified.
- **One injector process, not one per burst.** A one-shot `wtype` per burst
  creates and destroys a virtual keyboard each time, and that churn makes sway
  emit a keymap event minifb panics on outright (`unimplemented!("Only XKB
  keymaps are supported")`, `wayland.rs:1225`). All 480 edges now come from a
  single `wtype` invocation.
- **`Shift_L`, not `shift`.** `shift` is a modifier name for `wtype -M/-m`,
  not a key name for `-P/-p`. wtype validates every argument before sending
  anything, so one bad name silently injected *nothing* -- a run that looked
  like a clean pass but had measured nothing at all.

The measurement: 480 key edges over four phases -- single keys, rollover
across Shift, sustained three-key overlap, and deep four-key overlap -- at a
60ms cycle, run against the pre-fix and post-fix minifb with everything else
identical.

| key | pre-fix `8f19983` | post-fix `0b54200` |
|---|---|---|
| T | **33 press / 84 release** | 84 / 84 |
| E | **38 / 53** | 58 / 58 |
| S | **38 / 40** | 40 / 40 |
| LeftShift | 35 / 35 | 35 / 35 |
| orphan releases | **68** | **0** |
| duplicate presses | 0 | 0 |

The fix recovers 51 lost `T` presses and eliminates all 68 unpaired releases.
`LeftShift` balances in both because it is held across other keys and so always
spans many cycles -- which is itself a useful control: the bug only touches
keys whose edges fall close together.

Post-fix, the keys that go missing do so as *complete pairs* (every count
balances exactly), never as a half-edge. That is the documented residual
behaving as described: it drops keys, it never latches them.

### The real poll-cycle number (measured 2026-08-28)

Every "60ms" above was *chosen*, not measured. It has now been measured on the
real stack -- `wprsd` + `navetted` + Firefox + `navette-viewer` -- with the
viewer in a GPU-backed nested sway, resized by resizing its own toplevel (a
genuine `xdg_toplevel` configure, confirmed by 7 `decoder reconfigured stream`
events in the viewer log). Numbers are the committed `poll tick fired late`
probe, phase-split:

| phase | n | p50 | p90 | p99 | max | cycles >=50ms |
|---|---|---|---|---|---|---|
| startup | 13 | 20ms | 240ms | 508ms | 508ms | 2 |
| baseline | 60 | 18ms | 23ms | 213ms | 533ms | 2 |
| **resize** | 66 | 15ms | **125ms** | 531ms | **626ms** | **8** |
| all | 149 | 17ms | 30ms | 626ms | 626ms | 14 |

**The 60ms assumption was conservative by an order of magnitude.** Real cycles
reach 626ms. Even at rest the loop runs at ~17ms, twice its 8ms target, and a
resize takes p90 to 125ms.

**This settles the open question: the residual is a live defect, not a
theoretical one.** 14 cycles over a single 60-second run were long enough
(>=50ms) to swallow an entire ordinary keypress, 8 of them during resizes. At
626ms, several complete keystrokes could land inside one cycle and vanish. So
"move decode/convert off the viewer's poll thread" is **required work, not a
nice-to-have** -- it is what bounds the one failure mode the minifb fix
deliberately does not address.

Caveats, so the number is not over-read: the viewer ran in a nested compositor
rather than a native session (GPU-backed, but still nesting), and Firefox was
largely idle -- a busier guest would plausibly be worse, not better. Note also
that the worst baseline cycle (533ms) is nearly the worst resize cycle, so
resize is where the *density* of slow cycles is, not the only place they occur.

One trap worth recording: the first attempt at this reported "no late ticks"
while a raw `grep` found 159. `tracing`'s fmt layer colourises field names, so
`lag_ms`, `=` and the value are separated by ANSI escapes and a naive
`lag_ms=(\d+)` regex matches nothing. The analyser strips escapes now. A
parser that silently finds zero of something is indistinguishable from the
thing not happening -- always cross-check against a raw count.

### Poll-cycle work, verified on a real stack (2026-08-29)

The 626ms stalls are fixed, and the fix is not the one the plan predicted.
Attributing time inside the poll loop showed frame handling peaking at 17ms
and window polling at 15ms -- neither explains 622ms -- and the worst stalls
happened with only two events handled, ruling out tick starvation. Every
stall >=100ms fell within 0.23s of a decoder reconfiguration: `router.handle`
spawns a fresh FFmpeg process and waits ~600ms for it to prime, and it ran
inside an async task. Exactly one runtime worker at a time holds tokio's
I/O+time driver, and when that worker's task blocks nothing re-enters it, so
every timer in the process stops firing.

Decoding now owns a dedicated thread (`navette_viewer::client::decode`),
which removes that failure class rather than mitigating it, and keeps the
connection task free to drain input while the decoder works.

Measured against the real stack -- wprsd, navetted, a continuously-painting
guest, real FFmpeg, two decoder reconfigurations in the run:

| phase | p50 | p90 | max | cycles >=50ms |
|---|---|---|---|---|
| baseline | 14ms | 29ms | 34ms | 0 |
| resize | 15ms | 18ms | 39ms | 0 |
| all (n=186) | 15ms | 21ms | 39ms | **0** |

Against 622ms max and 14 keypress-length cycles before. The residual (a
press and release inside one cycle are dropped, though never latched) drops
back to theoretical at these numbers.

**Harness note, since this cost two sessions:** the measurement kept failing
because the guest stopped painting. Firefox throttles paint when idle, so the
decoder receives one access unit, never primes, and the viewer never opens a
window -- a run that measures nothing while looking like a bug. Use the
paint-loop guest (`scratchpad/paintloop.sh` behind a desktop entry in a
scratch `XDG_DATA_HOME`) instead. Also check for leftover `wprsd` processes
from earlier runs: one holding an X display makes new sessions die with
"failed to start xwayland: Could not find a free socket".

### Open review findings, accepted but not fixed

Recorded here because the review artifacts they came from
(`.claude/PRPs/reviews/`) are deliberately git-ignored and exist only on the
machine that produced them. Labels are the review's own; note they collide
with the M2 *milestone* name and are unrelated to it.

From the PR #7 review (input path):

- **Press can overtake a queued release of the same key.** The viewer runs a
  multi-threaded runtime and the connection task drains input on another
  worker, so a permit can free between two sends inside one tick. A release
  that failed and was re-queued can therefore be overtaken by a later press
  of the same key, which the bridge's dedup then drops as redundant — turning
  what used to be a harmless duplicate keydown into a silently lost
  keystroke.
- **The dedup enforces a per-attachment invariant on a global seat.**
  `pressed_keys` is keyed by attachment, but the wire carries no attachment
  identity, so two attachments pressing the same keycode both forward and the
  guest sees two keydowns with no release between — exactly what the dedup
  exists to prevent. Multi-attachment is supported and tested elsewhere.
- **`KeyRepeat::No` removed an accidental recovery.** Under `KeyRepeat::Yes` a
  press lost to backpressure was re-reported by minifb's own typematic
  repeat, so a held key healed itself. It no longer does, and a dropped press
  is not retried. The change is still correct; the point is that
  `must_redeliver`'s "a dropped press is only a missed input" was reasoned
  against behaviour the same change removed. Same failure surface as the
  press-and-release-inside-one-cycle gap above.
- **The bridge flush desyncs bridge from viewer.** `release_all_held` releases
  keys the physical keyboard may still hold and clears tracking, while a
  still-running viewer keeps its own `held_keys`. Its next real release then
  arrives for a keycode the bridge no longer tracks and lands on the
  untracked-release debug path — expected there, not an anomaly, and worth a
  note beside that log so it is not chased as a bug.

From the PR #10 review (decode path):

- **Input delivery latency is bounded by the packet queue.** Once
  `PACKET_QUEUE_CAPACITY` (or the byte budget) is exhausted the connection
  task parks handing over a packet and stops draining input again — the same
  stall, deferred. Closing it entirely means the router owning its own task
  and the socket read never waiting on it.

### Two notes for whoever reads this next

- **The previous session's "tick starvation" hypothesis was half right, and
  the half it got wrong is why it wasn't found sooner.** It correctly
  identified the mechanism — slow poll cycles — but predicted the
  consequence as a release *delayed* past the guest's repeat delay. The
  release is not delayed; it is destroyed inside minifb and never sent at
  all. Every probe added to chase the "delayed" version watches a stage the
  event never reaches, which is why the second pass's instrumentation
  produced nothing. Worth remembering that a correct mechanism with a wrong
  consequence still points instrumentation at the wrong layer.
- **Separate latent issue, deliberately not fixed here:** `main.rs`'s
  `pending_redelivery` drains into `due` ahead of newly polled events, but a
  pending release that fails again is pushed back while later events in the
  same batch keep being sent — so a subsequent press of the same key can
  reach the wire ahead of the stale release. This is *not* the repeat bug
  (the evidence above puts that entirely inside minifb, upstream of this
  code) and it would drop or reorder a character rather than repeat one.
  Left alone rather than churning unverified code on top of a confirmed
  finding.

## Pick up here (written 2026-08-29, end of session)

### The one open defect, fully localised

**Typing during a resize still repeats characters**, about one burst in six.
This is the milestone's headline symptom. It is *not* where it used to be.

Ruled out, with evidence, not reasoning:

- **minifb is clean.** Instrumenting `native.rs` at the `get_keys_pressed` /
  `get_keys_released` boundary during a live run showed six matched
  press/release pairs for the repeating key, one per burst. The upstream
  two-phase fix works.
- **The client is clean.** Same run: max poll cycle **7 ms**, zero dropped
  inputs, zero retried releases, zero bridge-side releases for untracked
  keycodes, no panic.
- **The repeat is bounded** (~95 characters, then it stops). `wprsd`
  advertises `repeat_delay=200 repeat_rate=200`, so a guest repeats a held key
  *itself* until the release arrives. A lost release repeats forever; a late
  one repeats and stops. So the release is **late by roughly half a second**,
  not lost.

**Where it is.** `run_bridge` (`crates/navetted/src/bridge.rs`) runs one loop
that does all of: dispatch wprs events, apply scene events — which
`compose_toplevel` *and* `encode_frame` — then drain `MediaCommand::Input`,
then the debounced resize. `encode_frame` calls `EncoderProcess::spawn` when a
stream is created **or reconfigured**, and a resize reconfigures. That is an
FFmpeg process spawn, ~600 ms, sitting on the loop that also delivers input.
Input queued behind it waits exactly as long.

This is the same defect fixed on the *client* in PRs #8 and #10 — input
delivery serialised behind expensive work on a shared loop — at the other end
of the pipe.

### The fix is started — branch `perf/encode-off-the-bridge-loop`

**Library compiles and is believed correct. The test module does not. Do not
merge as-is.** Pushed so it is not lost; not opened as a PR for that reason.

What is done: encoding moved to its own thread. `run_bridge` keeps
composition, which needs `&scene`, and submits composited frames to an
`EncodeQueue`; the thread owns every encoder and the stream map and publishes.
`apply_encode_command` is the single code path, shared by the thread and by a
`#[cfg(test)] drain_encode_queue` helper that runs it synchronously.

Four decisions worth not re-litigating:

- **Frames coalesce per surface.** Submitting one for a key already queued
  replaces it *in place*, keeping queue position so ordering against
  `EndStream` survives. This is also what bounds the queue — at most one
  pending frame per stream — so it needs no arbitrary cap. Dropping frames is
  correct here, unlike on the input path: a superseded frame is worth nothing,
  a superseded keystroke is lost data.
- **A replaced frame sets `discontinuity` on the next one encoded** for that
  surface, or the viewer's HUD `DISC` counter silently under-counts.
- **`EndStream`/`ClientGone` drop queued frames** for the affected surfaces.
  The viewer would discard such packets anyway (`session.rs`'s `ignored`),
  but encoding them is wasted work and noise.
- **The thread is stopped and joined** as `run_bridge` unwinds, so FFmpeg
  processes are never left owned by nobody.

**What remains: 45 compile errors in `bridge.rs`'s test module**, three
mechanical classes — 32 uses of `worker.streams`, 9 calls passing the old
4-argument `handle_scene_events`, 4 constructions of `WorkerState` with a
`streams` field. Encoding is no longer synchronous, so each affected test
needs a local stream map plus a `drain_encode_queue` call after the
submitting step. The existing assertions on stream state and published
packets should then hold unchanged. This is judgement per test, not a
find-and-replace: the drain has to go at the right point in each.

**Then verify, in this order:** `cargo test --workspace`; a unit test at the
thread boundary (submit `Frame`, `EndStream`, `Frame` for one key; assert
nothing publishes after the end); then `e2e-keys.sh` **several times** —
it scores 5/6 today and should be 6/6, and at one-in-six a single clean run
proves nothing.

### The fix, as far as it was designed

Move encoding to a worker thread; keep composition on the loop, because it
needs `&scene`. `MediaHub` is `Clone`, so publishing from the thread is fine.
The loop sends commands; the thread owns `streams` and every `FfmpegEncoder`.

Four things that a naive version gets wrong:

1. **Ordering against stream teardown.** `SurfaceDestroyed` currently ends a
   stream synchronously in the same iteration. Once frames are queued, an
   `EndStream` can arrive behind frames composited before the destroy, and the
   thread would publish video for an already-destroyed surface. The viewer has
   an `ignored` set (`session.rs:52`) that looks like it drops late packets,
   so this is probably benign-but-noisy — **verify that before relying on
   it**. Cheapest fix: drop queued frames for a key when `EndStream` is seen.
2. **Drop the *oldest* frame per key, not the newest.** A stale frame that
   will be superseded is worth less than the current one, and per-key
   coalescing stops a busy window starving a quiet one. Dropping frames is
   *correct* here — unlike the client side, where dropping input is not.
3. **A dropped frame must set `discontinuity` on the next one encoded.**
   Nothing sets it today because nothing is ever dropped. Miss this and the
   viewer's HUD `DISC` counter silently lies.
4. **`encode()` does `write_all` of ~3.7 MB into a pipe** and can block if
   FFmpeg is slow. Moving it to the thread is right, but then the thread
   blocks and the queue backs up — which is what (2) exists for. Confirm the
   encoder's existing reader thread cannot deadlock against a blocked writer.

A sketched shape: a single ordered `VecDeque<EncodeCommand>` behind a mutex
plus a condvar, where submitting a `Frame` for a key already queued *replaces
it in place* — newest wins, queue position preserved, so ordering against
control messages survives coalescing.

**Verification is already built.** `e2e-keys.sh` types six `fox` bursts during
a live resize and currently scores 5/6. After the fix it should be 6/6. Run it
several times: at one-in-six, a single clean run proves nothing. Also worth a
unit test at the thread boundary — send `Frame`, `EndStream`, `Frame` for the
same key and assert nothing publishes after the end.

### A separate and arguably worse defect

**`navette-viewer` aborts on a non-XKB keymap event.** minifb answers anything
that is not `XKB_V1` with `unimplemented!()` (`wayland.rs:1238`), which takes
the whole process down. Observed for real: a second virtual keyboard appearing
and going away mid-session killed the viewer outright. Any keyboard hotplug
plausibly does the same. This is a crash, not a latency problem, and it is not
folded into the work above.

Upstream fix would be to ignore unknown keymap formats rather than panic.
Locally, nothing guards it.

### Reproducing any of this

The harness needs two things that cost a session each to learn:

- **The guest must repaint continuously.** Firefox throttles paint when idle,
  so the decoder receives one access unit, never primes, and the viewer never
  opens a window — a run that measures nothing while looking like a product
  bug. Use a guest that paints on a timer. For keyboard work it must *also*
  record what it receives, and set `stty -icanon min 1`, or the TTY line
  discipline holds characters and a late Return looks like lost input.
- **Leftover `wprsd` processes hold X displays.** One surviving from an earlier
  run makes every new session die with `failed to start xwayland: Could not
  find a free socket`. Check `ps` and `/tmp/.X11-unix` before blaming the code.

Scripts live in the session scratchpad, not the repo: `e2e-keys.sh`
(end-to-end keyboard), `rollover.sh` (480-edge minifb key test),
`polllag.sh` + `analyse-lag.py` (poll-cycle measurement), `endurance.sh` +
`analyse-endurance.py` (the 30-minute gate). They are worth rebuilding from
these notes if lost; the notes are the expensive part.

### In flight

PR #11 (`docs/preserve-open-findings`) carries the M2 gate report, this
handoff, and the `.claude/` ignore. Open and mergeable at time of writing.

## Measured: the encode split helps a lot and does not fix it (2026-08-29, later)

`perf/encode-off-the-bridge-loop` is finished and green — the test module
compiles, and the encode thread, its condvar handshake, and the
`stop()`/`join()` teardown now have coverage that did not exist when the
library half landed. `cargo test --workspace`: 169 passed, 1 ignored; clippy
`-D warnings` and fmt clean.

**The section above predicted `e2e-keys.sh` would go 5/6 → 6/6 after the fix.
That prediction is wrong, and the run says so.** The defect still reproduces
on the branch. What changed is its *magnitude*, and by a lot.

Both sides measured the same day, same harness, same machine, rebuilding
`navetted` between switches and verifying the binary's mtime actually moved
(a stale `target/debug/navetted` is exactly what would fake a null result).
The metric is the length of a repeat burst — how many extra characters the
guest printed before the release finally landed — because it is continuous,
where a 6-point pass count is not:

| | usable runs | corrupted bursts | extra chars per burst | median | max |
|---|---|---|---|---|---|
| master | 4 | 5 | 93, 104, 123, 214, 362 | 123 | 362 |
| branch | 2 | 4 | 9, 10, 22, 25 | 22 | 25 |

**The ranges do not overlap.** The branch's *worst* burst is 3.7x shorter
than master's *best*. `wprsd` hardcodes its advertised repeat at
`add_keyboard(Default::default(), 200, 200)` (`wprs/src/bin/wprsd.rs:281`) —
200ms delay, 200 chars/sec — so these convert to roughly 465-1810ms of
lateness on master against 45-125ms on the branch. That the master numbers
bracket the ~600ms FFmpeg spawn is a good sign the diagnosis was right.

Two things this does **not** show, stated plainly so nobody over-reads it:

- **Frequency is not measurably changed** (master 5 bursts across 24 typed
  `fox` bursts, branch 4 across 12 — 21% against 33%, which is one burst
  either way at this n). Only severity moved.
- **n is small and the harness is flaky.** Three of nine runs aborted with
  `ABORT: no window` — the decoder never primes and the viewer never opens.
  `runs.sh` now reaps leftover `wprsd` *before* each run as well as after,
  and prints a reason instead of a blank line, because a silent run is
  indistinguishable from a clean one.

**So there is a second, smaller source of input lateness still on the bridge
loop, worth ~50-125ms.** The obvious candidate, not yet probed: composition
stayed on the loop (it needs `&scene`), and a resize burst means many commits,
each compositing a ~3MB frame before `MediaCommand::Input` is drained. The
cheap next step is the probe pattern this project already uses — time each
`run_bridge` iteration, log over a threshold, phase-tag it the way
`polllag.sh`/`analyse-lag.py` did for the client — and cross-check the parser
against a raw `grep -c`, per the ANSI-escape trap recorded earlier.

Read the branch as "removes a ~600ms input stall, shrinks the repeat burst by
5-14x, does not eliminate the defect". That is a real improvement and it is
worth landing on its own; it is not a fix for the headline symptom, and the
PR should not claim to be one.

## SOLVED: the residual was wasted composition, not composition (2026-08-29)

Branch `perf/localise-bridge-loop-residual`, off #12. **The headline symptom
is gone: three consecutive clean e2e runs, 6/6 each, 18 bursts, zero
corruption** — against 5-of-6 runs corrupted immediately before.

### What the probe showed

`MediaCommand::Input` now carries a `queued_at` stamp from the producer, so
the drain reports how long a keystroke *actually* waited instead of inferring
it from iteration duration. That distinction is what found this: it separates
"the loop was slow" from "the loop was slow while input was waiting".

Four runs, 218-227 records each, parser cross-checked against a raw substring
count per the ANSI trap recorded earlier. Applying input costs ~4us and
`dispatch` is a non-factor, so neither the input path nor the 10ms timeout was
implicated. Composition was, at up to 503ms in one iteration, with keystrokes
waiting up to **528ms** behind it.

**But the composite counter is what changed the fix.** Composites came out at
exactly half the message count (20->10, 18->9, 6->3 — two wprs messages per
commit), all for the *same* toplevel, ~40ms each. The encode queue coalesces
those to one frame per surface. So nine of every ten composites produced a
frame nothing ever encoded, while input sat behind all of them.

### The fix, and why the planned one was wrong

The plan said "move composition off the loop". That would have relocated 100%
of the cost at the price of getting `&scene` to another thread. Instead, stop
doing the work: a commit records that its toplevel *owes* a composite, and
`flush_composites` runs once at the end of the batch, compositing each owed
surface exactly once. Composition stays on the loop; a burst of N commits to
one window now costs one composite instead of N.

| | pre-fix max | post-fix max |
|---|---|---|
| `compose_us` | 503,420 | 100,785 |
| `worst_input_wait_us` | **528,176** | **64,973** |

**Why that eliminates the symptom rather than shrinking it.** wprsd advertises
`repeat_delay=200` (`wprsd.rs:281`), so a guest repeats a held key only if its
release is more than 200ms late. Pre-fix waits of 358-528ms cleared that bar
and repeated; post-fix the worst wait is ~65ms, comfortably under it, so the
repeat timer never starts. The margin is ~3x, which is why 6/6 reads as causal
rather than lucky.

### Ordering hazard, handled

Deferring composition creates a hazard the immediate version could not have: a
commit and a destroy for the same surface in one batch would composite *after*
the `EndStream`, and the queue's EndStream handling only drops frames already
queued — so that late frame would open a fresh stream for a dead window.
`SurfaceDestroyed` drops the owed composite and `ClientDisconnected` drops the
client's. A destroyed *subsurface* is deliberately not dropped: the set holds
toplevels, so its ancestor stays owed and still recomposites. Both guards are
covered by tests verified to fail when removed — the hazard is real, not
theoretical.

### Still true after this

The probe is kept rather than removed (same call as the client's committed
`poll tick fired late`); it is debug-level, so it costs an `Instant::now()`
per input and nothing else. The harness still aborts roughly a third of runs
with `no window`.

### The fix holds for ONE window. It does not hold for three.

Measured, not assumed, and it is the honest limit of everything above. A
three-window guest (`multiguest.sh` — all three painting continuously at
10Hz, *all three recording*, since only the focused viewer window receives
keys and which one sway focuses is not controllable) brings the defect back:

| windows | fox score | `apply_us` max | `compose_us` max | `worst_input_wait_us` max |
|---|---|---|---|---|
| 1 | 6/6, 6/6, 6/6 | 41,314 | 100,785 | **64,973** |
| 3 | 6/6, **3/6**, **3/6** | 239,288 | 168,001 | **339,490** |

Bursts up to 39 characters at three windows. The 200ms `repeat_delay` is the
line: single-window waits (~65ms) sit a comfortable 3x under it, three-window
waits (277-339ms) clear it, and the guest repeats.

**Coalescing behaves exactly as designed — that is the problem.** It bounds
composition to one composite *per window* per batch, so `compose_us` in a bad
iteration is ~135ms, almost exactly 3 x the ~45ms single-window figure. It
scales linearly with the number of painting windows, and nothing bounds it
below the repeat threshold.

**And `scene.apply` is now the larger half** (126-239ms against compose's
88-140ms). That was invisible at one window, where apply peaked at 41ms. It
is untouched by any work so far and is not a composition problem, so the
per-batch coalescing idea does not extend to it.

**The likely general fix, not attempted:** drain `MediaCommand::Input`
*between* messages rather than after the whole batch. That bounds input
latency to one message's apply+compose (~10-45ms) regardless of batch size or
window count, instead of to the batch total. It is a smaller change than
moving composition off the loop and it removes the scaling dependence rather
than lowering its constant. Check what applying input against a
mid-batch scene does to `input.apply`'s surface lookups first — that is a
correctness question, not a latency one.

So: land the coalescing (it is a strict improvement and removes ~90% of a
real waste), but **M2's gate should not be called closed on single-window
evidence.** The symptom is gone for one window and returns at three.

## FIXED at three windows too: bound the wait, don't shrink the work (2026-08-29)

Branch `perf/bound-input-latency`. Three-window guest: **4/4 runs at 6/6**,
against 6/6, 3/6, 3/6 before.

**The framing "fix the scene.apply bottleneck" was wrong, and optimising apply
would not have fixed this.** Input latency was unbounded *by construction*:
`run_bridge` drained `MediaCommand::Input` only after every message in a batch
and then every owed composite, so a keystroke inherited the whole batch's
cost. That cost scales with window count -- each commit decodes a full
framebuffer, each painting window owes a composite -- and once it clears
wprsd's 200ms repeat delay the guest repeats. Coalescing lowered the constant.
It never bounded the wait. That is why the symptom returned at three windows
even with composition already doing the minimum work possible.

`pump_input` now runs between messages *and* between composites.

| 3 windows | before | after |
|---|---|---|
| `worst_input_wait_us` max | 339,490 | **46,803 / 29,490** |
| `total_us` max | 375,819 | 355,980 / 392,356 |
| `apply_us` max | 239,288 | 186,316 / 202,674 |
| `compose_us` max | 168,001 | 171,472 / 189,480 |

**Read the second row before the first.** Total iteration time is *unchanged*
and composition is *unchanged* -- the batch is exactly as expensive as it was.
Input wait still fell 7-11x, to a ~4x margin under the 200ms line. That is the
signature of the right fix: latency decoupled from batch duration rather than
the batch made faster. It removes the dependence on window count instead of
lowering its constant, so a fourth and fifth window do not re-break it.

**Safety of draining mid-batch:** `InputState::apply` only does read-only point
lookups (`validate_surface`, `surface_dimensions`), and each message's scene
update completes atomically, so the scene is always internally consistent. For
pointer input it is arguably *more* correct -- coordinates are clamped against
the dimensions the client actually saw, not a frame composited later in the
same batch.

**Also fixed, separately:** `apply_surface` cloned the previous surface image
on every commit, but `decode_assignment` carries it forward only when the
commit brings no buffer of its own. Every ordinary repaint deep-copied a whole
framebuffer and dropped it. Guarding on `state.buffer.is_none()` accounts for
the ~20% fall in `apply_us` above. Real waste on the hottest path, but not the
root cause -- worth separating, because fixing only this would have left the
defect in place.

### The loop is still saturated, and the alarm for it is now gone

Stated as throughput, because milliseconds understate it and because the
symptom that used to make anyone look at this is fixed.

Three windows, post-fix, per-iteration total: **p50 31-33ms, p90 57-119ms, max
356-392ms**. A window that commits every iteration gets one composite per
iteration, so its frame rate tracks that: **roughly 30fps at the median,
8-17fps at p90, and ~2.6fps in the tail** -- the tail being exactly what a
resize burst looks like. `apply` (~186ms) and `compose` (~171ms) fill it.

Nothing there is a *symptom* now. Input stays responsive throughout, which is
the point of the fix. But the honest reading is that during a resize the guest
is a slideshow, and a guest with more windows is worse. Two things follow:

1. **Do not treat this as closed because typing works.** The repeat defect was
   the alarm for loop saturation and it was never a good one -- it fired at a
   threshold (wprsd's 200ms repeat delay) unrelated to throughput, so a loop at
   190ms per iteration produced ~5fps and rang nothing at all. That alarm is
   now permanently silent. The next signal will be someone saying "video is
   choppy", which is much harder to trace back to `run_bridge`.
2. **The ceiling is `Scene::apply` decoding a full framebuffer per commit**
   (`decode_image`: a `vec![0; ~3MB]` plus a per-byte `unfilter` pass). The
   discarded-clone fix took ~20% off it. The rest is real work, and reducing it
   means not decoding frames nobody composites, or decoding them off the loop.
   That is a throughput problem and wants the opposite treatment from the
   latency one: less interleaving, less work -- not more pumping.

Keep the two apart. Conflating them is what sent the previous two sessions to
the wrong layer.

## Decode was the ceiling, and half of it was wasted (2026-08-31)

Confirmed the suspicion above with a microbenchmark of `decode_image` in
isolation, at realistic window sizes:

| window size | zero-init alloc | `filtering::unfilter` (SIMD defilter) | full `decode_image` |
|---|---|---|---|
| 64x64 (test fixture) | ~0us | ~22us | ~16us |
| 800x600 | ~38us | ~1,779us | ~1,955us |
| 1920x1080 | ~203us | ~4,662us | ~3,397us |

`filtering::unfilter` -- the vendored `wprs` SIMD routine that reverses the
delta-encoded pixel filter wprsd applies before sending buffers over the
socket -- is 91-96% of decode's cost, and scales with pixel count as expected.
Allocation is noise. Nothing pathological in the vendored code; the cost is
real and roughly linear.

But *which* commits paid it was the actual problem, not the cost per call.
PR #13's own diagnostic already showed a resize burst sends ~10 commits to one
window per iteration, all superseding each other, and only the last one
before the batch's single composite ever mattered -- composite coalescing was
already fixed for exactly this shape. Nothing coalesced decode the same way:
every one of those 10 commits still carried a brand-new buffer (a resize
always reallocates), so `Scene::apply` unfiltered all 10 full framebuffers
even though 9 were thrown away before the batch's one composite. At
1920x1080 that is ~42ms of real CPU time per iteration, per window, spent
decoding images nobody ever looked at.

**Fixed** the same way PR #13 fixed composition: don't do the work eagerly.
`SurfaceNode.image` is now `SurfaceImage`, an enum of `None` /
`Pending { width, height, stride, format, filtered }` / `Decoded(Image)`.
A commit with a new buffer validates it synchronously (dimensions, stride,
buffer-length consistency all still checked at commit time, so a malformed
buffer is still rejected from `Scene::apply` immediately, exactly as before --
see `validate_buffer`) but stores the still-filtered bytes and stops there.
The actual SIMD defilter runs the first time something asks for pixels --
`Scene::compose_toplevel`, now `&mut self` instead of `&self` -- and the
result is cached, so a second composite in the same batch is free and a
superseded commit is never decoded at all.

Verified with the same kind of A/B this project has used before: a synthetic
10-commit resize burst to one 1920x1080 window, one compose, measured through
the real `Scene` API. Forcing eager decode (a one-line mutation reverting the
laziness) cost 64.3ms total; the fix costs 24.5ms -- a ~2.6x reduction from
eliminating 9 of 10 wasted decodes, in line with the isolated benchmark above.
Guarded by `a_second_new_buffer_before_any_compose_supersedes_the_first_without_decoding_it`,
mutation-verified to fail if decode is forced eager again.

## Re-verified live, and it's not just faster -- master doesn't recover (2026-08-31, later)

The old sway+wtype+viewer e2e harness from PRs #13/#14 was gone (ephemeral
`/tmp`), and its keyboard-scoring half measures a different problem (M2's
already-closed repeat defect). Rebuilt a narrower rig instead, scoped to what
this fix actually changes -- bridge-loop throughput, not keystroke scoring:
`navetted` + `wprsd` + a continuously-painting guest, read straight out of
`navetted`'s own iteration-timing trace. No viewer, no nested sway, no wtype.

- **Guest**: `crates/navette-viewer/examples/paintloop_guest.rs`, checked in
  this time rather than left in scratch -- three sessions have now paid to
  rebuild a lost harness (this file's own history above, twice; the
  keyprobe/e2e-keys lineage a third time). `PAINTLOOP_WINDOWS` native `minifb`
  windows (1280x720), each repainting flat out with `set_target_fps(0)` --
  no throttling of its own, deliberately harsher than a real resize (which
  settles after a few frames; this never does). Run under a session's
  `WAYLAND_DISPLAY` via a `.desktop` entry in a scratch `XDG_DATA_HOME`
  (`navetted` spawns `wprsd` and the guest itself, `supervisor.rs:175-197` --
  no manual wiring needed) and `XDG_CONFIG_HOME` pointing at a `wprsd.ron`
  with `enable_xwayland: false` (the guest is native Wayland; without this,
  `wprsd` panics trying to exec a `xwayland-xdg-shell` helper that isn't on
  this machine).
- **Measurement**: a `TEMP-DIAG` unconditional `tracing::trace!` mirroring
  the shipped `LOOP_LAG_THRESHOLD_US`-gated line, so every iteration is
  visible, not just the slow ones -- added identically to a `git worktree` at
  `7ce27f3` (master, pre-fix) and this branch, never committed to either.
  20-second runs, 1 and 3 windows, each side.

| | iterations logged (20s) | apply_us p50 | apply_us max | total_us p50 | total_us max | msgs/composite |
|---|---|---|---|---|---|---|
| master, 1 window | 27 | 1,230,085 | 1,294,591 | 1,236,656 | 1,301,820 | 756 |
| branch, 1 window | 2067 | 2,272 | 10,217 | 10,431 | 20,605 | 15.8 |
| master, 3 windows | 30 | 1,004,230 | 1,350,052 | 1,020,395 | 1,365,681 | 239 |
| branch, 3 windows | 896 | 4,180 | 10,299 | 27,790 | 37,853 | 10.7 |

That is not a percentage improvement, it is a phase change. On master,
`apply_us` **is** `total_us` (within a few percent) and both grow across
successive iterations -- 21ms, 41ms, 77ms, 138ms, 249ms, 496ms, 921ms,
1.3s, climbing with the message count each time (2, 12, 28, 58, 106, 188,
364, 742, 1024) -- because decode is slow enough that a fast guest outpaces
it, the backlog grows, and a bigger backlog makes the next iteration slower
still. It never recovers inside the 20s window; only 27-30 iterations happen
at all. On the branch the same guest holds a steady ~100-190ms window
(3-window) or ~10ms window (1-window) indefinitely -- ~30-100 iterations per
second, sustained, not degrading.

**Read this honestly, not as a clean win to bank without qualification.**
This guest never yields for a compositor frame callback the way a real
client's paint loop normally would, so it is a harsher and more sustained
load than an actual resize burst, which is self-limiting (the guest settles
at the new size and stops). The master numbers above are worse than the
historical 30fps/2.6fps resize figures for exactly that reason, and are not
directly comparable to them. What this run *does* prove, cleanly: `apply_us`
tracking `total_us` on master confirms decode is the mechanism, and the
divergence -- master compounding into runaway backlog, branch holding
steady -- confirms the fix removes a genuine unbounded-growth failure mode,
not just a constant-factor cost. Whether a real client can ever sustain
enough sequential commits to trigger this on master in practice is not
established here; what's established is that when it does, this fix is the
difference between recovering and not.

## M3 slice 1: Android scaffold + drawer screen, in review (2026-09-01)

PR #16 (`feat/android-drawer-scaffold`, open, CI green, `mergeStateStatus:
CLEAN`) is the first slice of M3 -- scope agreed with the user up front
(docs/prp/PRP-plan.md §6 milestone M3 is the whole Android client; too big
for one PR). This slice: Gradle/Kotlin/Compose project skeleton, a Kotlin
mirror of `navette-protocol`'s control-channel wire types
(`android/app/.../protocol/ControlProtocol.kt`), a WebSocket client
(`net/NavetteClient.kt`), and Connect/Drawer screens. No MediaCodec/session
screen yet -- that's the next, separate slice.

Environment note for whoever picks this up: Android SDK (platforms 31-36,
build-tools, cmake, ndk), `adb`, `sdkmanager`, and system `gradle` are all
present on this machine. No AVD and no `emulator` binary -- every claim
below is build/unit-test/live-smoke-test verified, **none is an on-device or
emulator run**. `android/local.properties` (git-ignored) needs
`sdk.dir=/home/user/android-sdk` to build here again.

**Verification chain, strongest to weakest:**
1. `./gradlew clean assembleDebug testDebugUnitTest lintDebug` -- clean.
2. Protocol round-trip tests use the *exact* JSON fixtures
   `navette-protocol`'s own Rust tests assert against
   (`request_fixtures_round_trip`, `response_fixtures_round_trip`,
   `unknown_fields_are_accepted_for_additive_evolution`) -- the two suites
   are meant to drift apart loudly if the wire shape ever changes, not
   silently.
3. **Live wire-compatibility smoke test** against a real running `navetted`
   (subprotocol negotiation, `list_apps` against the real XDG index,
   `list_sessions`, and an error envelope for `kill` on a nonexistent
   session) -- not just static fixtures.

**Two review passes so far, both by this session (caveat: not
independent):**

- **Self-review** (`.claude/PRPs/reviews/pr-16-review.md`) found two real
  `AppViewModel` bugs, both mutation-verified and fixed: a leaked
  `connectionState` collector on every reconnect (a `StateFlow` never
  completes on its own, so retrying after a `Failed` state left the old
  collector running for the rest of the ViewModel's lifetime), and a
  snackbar-dismiss race (an older message's delayed dismiss could clobber a
  newer one that arrived while the first was still showing). Also
  introduced `NavetteApi` as a constructor-injectable seam so
  `AppViewModel` has real tests now (5 of them) instead of none.
- **Devil's-advocate pass** (this session, same day) on the *full* PR diff
  found four more real issues, agreed but **not yet implemented**:
  1. `controlWebSocketUrl` does raw string interpolation
     (`"ws://$host:$port$path"`) with no host validation. A Tailscale IPv6
     address (`fd7a:115c:a1e0::1`) or a host string that already includes a
     port produces a malformed authority (`ws://fd7a:115c:a1e0::1:9417/...`)
     -- ambiguous colons, no bracket-wrapping. Worse: `NavetteClient.connect()`
     builds the `Request` unguarded, so this doesn't just fail to connect,
     it throws `IllegalArgumentException` out of `connect()` and crashes the
     tap.
  2. `NavetteClient.call()` has no timeout. Connection-loss is already
     covered (`onFailure`/`onClosed` fail every pending call), but "the
     connection stays healthy and `navetted` just never answers this one
     `request_id`" hangs the caller forever -- `isLoading` stuck `true`
     with no way out short of killing the app. `pending.remove(requestId)`
     should also move into a `finally` so a timed-out entry doesn't linger
     in the map.
  3. `AppViewModel.refresh()` awaits `ListApps` then `ListSessions`
     sequentially despite `NavetteClient` already supporting concurrent
     in-flight requests by `request_id` -- and nothing stops two `refresh()`
     calls from overlapping, so whichever response lands last wins
     regardless of freshness. Fix is the same shape as the
     already-fixed `connectionJob` leak: run both calls concurrently via
     `coroutineScope`/`async`, and track/cancel a `refreshJob` before each
     new `refresh()`.
  4. (Minor, maintainability) `FakeNavetteApi.close()` in the test file
     doesn't interrupt an in-flight `call()` the way the real client's
     `close()` does -- fine today since nothing exercises that, but worth a
     one-line comment so a future test doesn't assume otherwise.

  Full transcript with code snippets for each fix is in this session's
  conversation; not re-committed to a file since the PR isn't merged yet --
  apply them as commits on `feat/android-drawer-scaffold` before merging,
  the same way the two self-review rounds were.

**Not done, explicitly deferred (recorded in the self-review, not
blocking):** a `NavetteClient`-level test using `okhttp3:mockwebserver`
(only the live smoke test and, transitively through the fake, the
ViewModel's own tests exercise its contract); `onOpen` doesn't verify
`navetted` actually negotiated the `navette.v1` subprotocol it asked for
(low risk -- the live smoke test confirms it does today).

## What's next

Items 1-4 of the previous list are done and are kept below only as history.
As of 2026-08-29 the remaining work is:

1. **PR6's gate report.** The endurance and LAN/tailnet data exist, but were
   taken on 2026-08-26 against code that has since changed materially (PRs
   #7-#10 altered the scene graph, the input path, minifb, and moved decoding
   onto its own thread). Re-run the endurance gate against master before
   citing it. See `docs/superpowers/reports/`.
2. **A human at a keyboard, once.** Every key-edge result is from the minifb
   layer or a scripted harness. Nobody has typed into a real navette window
   during a resize since the fixes landed. The harness makes this cheap now.
3. **The open review findings** recorded above -- none blocking, all with a
   concrete failure scenario written down.
4. **Repin minifb to a crates.io version** once upstream ships a release
   containing #429. Blocked on emoon, not on us; 0.28.0 predates the merge.
5. **M3, the Android client** -- the milestone the roadmap treats as the real
   product moment. Slice 1 (scaffold + drawer screen) is in PR #16, open,
   with 4 agreed-but-unimplemented fixes from a devil's-advocate pass -- see
   the section above. Apply those, merge, then the session screen
   (MediaCodec H.264 decode, input, clipboard, resize-follows-viewport) is
   the next and much larger slice.

### Done (2026-08-29), kept for context

- Environment unblocked: `libxkbcommon-devel` was already present, `/dev/uinput`
  turned out writable via the `nobody` group, and the pinned wprs rev was
  already in cargo's git cache.
- "Rapid typing + resize repeats" root-caused, fixed upstream, and merged --
  it was minifb, not navette. See the section above.
- The uncommitted working-tree diff landed as PR #7.
- emoon/rust_minifb#429 merged upstream, so the fork is gone (PR #9).
- The 622ms poll-cycle stall root-caused and fixed (PRs #8 and #10), verified
  against a real stack.

   the one the roadmap treats as the real product moment — everything
   before it is proving the plumbing works.

## M3 slice 2: the session screen — implemented, on-device-verified, one real bug found and fixed (2026-09-06)

**Branch `feat/android-session-screen`, two commits, not pushed:** `7d089f1`
(the slice itself — MediaCodec decode, input, resize) and `8cc011b` (a real
bug found during on-device testing, see below). `git log --oneline -2` on
that branch shows both. Base was `master` @ `8448fa6` (PR #16, the drawer
slice).

### How this slice was built

Planned via `/prp-plan` (written to
`.claude/PRPs/plans/completed/android-session-screen.plan.md`), implemented
via `/prp-implement`, then carried through **three rounds** of independent
`code-reviewer` + `security-reviewer` passes before being called
merge-ready. Worth knowing if this pattern gets reused: rounds 1-2 each
found real HIGH-severity bugs that the *previous* round's own fix had
introduced (an IME backspace/typing divergence; a landscape-lock bug that
recreated the Activity; a receive-path memory bound with no byte budget;
then, fixing that, a data race the dispatcher change introduced; then a
byte-budget bypass on the "must never drop this" packet path). Round 3
broke that pattern — the implementer's own whole-system audit caught its
own bug (a semaphore-permit leak) before review did, which the reviewer
read as real evidence the surface had stabilized, not just a clean pass.
109 unit tests by the end of that process (up from 16 at the drawer slice),
`./gradlew clean assembleDebug testDebugUnitTest lintDebug` — the exact CI
command — green throughout.

**What was never verified until this session: does any of it actually work
on a real device.** No AVD/emulator exists in this environment (still
true — the drawer slice's own README already said so), so Tasks 8-10
(`H264Decoder`, `SessionScreen`, the touch/keyboard input path) had zero
on-device coverage until a phone was actually plugged in.

### On-device verification, this session (Pixel 9 Pro Fold, Android 17)

Stood up real end-to-end test infrastructure on this machine to make that
possible: cloned and built `wprsd`/`wprsc`/`xwayland-xdg-shell` fresh at
`../wprs` (sibling to this repo, pinned rev `5763d746` matching
`crates/navette-bridge/Cargo.toml` at the time; the pin has since moved to
`38c61fe`, so rebuild `../wprs` from that rev, not this one), built this repo's own
`navetted`/`navette` fresh (**do not use `~/.local/bin/navetted`** — that's
an unrelated binary of the same name, a "Claude Code" pairing daemon, not
this project's daemon; a real naming collision on this machine that cost a
few minutes to notice), and ran a live Firefox session against it.

**Confirmed working, for the first time, on real hardware:**
- The full connect → drawer → attach flow against a live `navetted`.
- **MediaCodec H.264 decode against the real VA-API-encoded stream** — the
  plan's own flagged highest-risk item (`csd-0`/`csd-1` handling) — works
  correctly, first try.
- Landscape lock engages and correctly releases back to portrait on leave.
- "Disconnected: connection failed" renders correctly when `navetted` dies
  mid-session (killed it outright to check) — a clean state, not a hang.
- The on-screen "Keyboard" toggle raises the real IME correctly.

**Found one real bug, root-caused and fixed (`8cc011b`):** touch and
keyboard input reached the wire correctly the whole time, but the bridge
silently rejected every single input event for this session. Root cause:
`MediaInput`'s `client_id`/`surface_id` are genuine unsigned 64-bit wire
values that can exceed `Long.MAX_VALUE` (a real session's `client_id` was
`15272202610726850855`) — Kotlin's `Long` serializes that as a *negative*
JSON decimal, and the bridge's `serde` deserializer correctly refuses a
`-` sign for a `u64` field. No crash, no client-visible error: input just
did nothing. Fixed by changing those fields to `ULong` (kotlinx.serialization
encodes it as the correct unsigned decimal). Full root-cause writeup,
including two false leads chased first (a Compose `AndroidView` touch-interop
theory that turned out not to be the actual cause, and why `tcpdump`
couldn't have proven anything about outgoing WebSocket frames — RFC 6455
masks client frames — plus why `adb shell input tap` is invisible to
`getevent`) is in this session's `/investigate` transcript and logged as
gstack learnings (`kotlinx-serialization-long-as-u64`,
`tcpdump-websocket-client-frames-masked`, `adb-input-tap-bypasses-evdev`,
`logcat-filter-by-tag-not-package`) if picking this back up.

**Verified live after the fix**: tapping a page element opened a genuine
new browser session (page navigated to Firefox's own start page — real
proof of a real click, not a coincidence); dragging moved the pointer
(confirmed via Firefox's own link-hover status-bar text tracking the drag
in real time); the bridge's `invalid_input` rejection is gone from the
logs entirely.

### What's still not verified

Only ran out of session time, not blocked on anything: hardware Bluetooth/USB
keyboard typing, on-screen IME with an actual autocomplete-triggered
replacement, and a live resize (rotating/resizing while attached). None of
these have known issues — they're just untested. The plan's Manual
Validation checklist in `android/README.md` has the full list.

### Environment note for whoever picks this up

If you want to re-run any of this: `wprsd`/`wprsc`/`xwayland-xdg-shell`
binaries live at `../wprs/target/release/` (relative to this repo) — put
that dir on `PATH` before starting `navetted`. Start it with
`--bind <tailnet-ip>:9417 --allow-remote` (loopback-only default can't
reach a phone). `RUST_LOG="info,navetted=debug,navette_bridge=debug"` is
worth it — it's what surfaced the `invalid_input` rejections once logcat
was filtered correctly (by the `MediaClient` TAG, *not* by grepping for the
app's package name — Android log lines don't contain it). A `navetted`
process bound to `100.111.143.67:9417` with debug logging may still be
running on this machine from this session (`ps aux | grep navetted`) with
a `phonetest` session (Firefox) still attached to it — check before
starting a second one.

## M3 slice 3: touch gestures and pinch-to-zoom -- implemented, CI-green, single-touch verified on device, multi-touch needs fingers (2026-09-06)

**Uncommitted, on `feat/android-session-screen`** (which is itself still
unpushed with the three slice-2 commits). Nine files: `GestureInterpreter.kt`,
`ViewTransform.kt` and their tests are new; `SessionScreen.kt`,
`InputMapper.kt`, `MediaProtocol.kt`, `InputMapperTest.kt` and
`android/README.md` are changed. **No Rust changed** -- that is a defining
property of the design, not an accident; `git diff --stat master...HEAD --
crates/` is empty and should stay empty for this slice.

### What it is

Pinch to zoom the video 1x-4x, anchored to the fingers; two-finger drag pans
when zoomed and scrolls the guest at 1:1; two-finger quick-still tap is a
right-click. One finger still drives the guest pointer exactly as before,
except the left press is now sent 60ms after the finger lands
(`PRESS_ARM_MS`) so a second finger can cancel it -- previously every pinch
began with a stray left click at the first finger. A tap shorter than 60ms
still clicks (the press goes out with the release). Zoom is purely
client-side: a scale+translate on the `SurfaceView`, the guest window never
resizes, the frames are upscaled by the GPU and so are soft past 1:1.
Gesture table and known limitations are in `android/README.md`.

Decisions taken with the user before building, each ruling out a plausible
alternative: local zoom over ctrl+scroll forwarding or a crop/render-region
protocol; one finger = pointer over drag-to-pan or trackpad mode; arming
delay over accepting the stray click; two-finger tap over an overlay toggle.
One taken while planning: the two-finger drag mode is **latched at the
second finger's landing from the zoom level at that instant** and never
changes mid-gesture -- so the guest cannot be scrolled while zoomed in.
The intended fix (a pan that hits the content edge in one axis starts
scrolling that axis) is deferred and named in the README.

### The two things measured on the device before anything was built

Both on the Pixel 9 Pro Fold, Android 17, against the live `navetted` +
Firefox `phonetest` session from slice 2 (both daemons were still running):

1. **Touch coordinates arrive inverse-mapped through the view transform.**
   With a throwaway static `scaleX = scaleY = 2f` on the `SurfaceView`, a
   tap at screen `(2400, 1060)` on the 2424x1080 landscape surface reached
   the touch listener as `event.x = 1200.0`. This is what dictates the
   architecture below.
2. **The video layer follows a `SurfaceView` scale/translate.** The
   screenshot showed the picture magnified 2x (toolbar and page text cut off
   mid-word at the right edge -- impossible at 1:1) while the Compose
   `Keyboard` overlay stayed normal size. So no `TextureView` fallback.

### Why the interpreter works in screen space and the transform is applied synchronously

Because of measurement 1, gesture maths in local (view) space would see a
stationary finger *move* whenever the zoom changed under it -- a feedback
loop during every pinch. So `SessionController.onTouchEvent` lifts each
pointer to screen space via `ViewTransform.localToScreen` using the
transform it believes is applied, and the interpreter works there. That
transform and the view's real matrix must be identical at that instant.
The plan had the transform flow through a `StateFlow` to an `AndroidView`
`update` lambda; that lands a frame later via recomposition, so every pinch
step would read the fingers through a stale matrix. The controller instead
holds the `SurfaceView` (bound in the factory, like `surfaceCallback`) and
sets scale/translation directly from the touch path. If someone "tidies"
this back into Compose state, the pinch will drift.

### Scroll: the plan's constant was wrong, and why

The plan reasoned "one wheel notch per 60px". The actual sink: the bridge
forwards `PointerAxis` as `AxisScroll.absolute` tagged
`AxisSource::Continuous` (`crates/navette-bridge/src/input.rs:132-145`) and
wprsd applies that verbatim as the `wl_pointer.axis` value
(`../wprs/src/server/client_handlers.rs:239-240`). For a continuous source
Wayland defines that value in surface-local pixels, so the natural ratio is
`1.0` (content follows the finger), negated because Wayland's positive axis
means "scroll down". `InputMapper.SCROLL_UNITS_PER_PIXEL` and the sign in
`scrollUnits` are the two things to tune if a real guest disagrees --
neither has been checked against one yet.

Two facts worth keeping beside this: `PointerButton` and `PointerAxis` are
both dispatched by the bridge at `Point { x: 0.0, y: 0.0 }` (`input.rs:121`,
`input.rs:139`) and only `PointerMotion` sets pointer focus
(`input.rs:81-90`). Every button and axis effect the interpreter emits is
therefore preceded by a motion. Break that pairing and input vanishes with
no error -- the same signature as the `8cc011b` u64 bug.

### Verification status

- `./gradlew --console=plain assembleDebug testDebugUnitTest lintDebug` --
  the exact CI command -- green. 156 unit tests (109 → 156; 28 interpreter,
  13 transform, 6 mapper, all first-run green). Lint: 0 errors, 21
  warnings, all pre-existing (checked by stashing).
- **Single-touch verified on a Pixel 10 Pro Fold** (Android 17, fresh
  install, the real build -- the Pixel 9 Pro Fold used for the Task 1
  measurements went away mid-session and still has the probe build with a
  static 2x scale on it; reinstall there before judging anything). Driven
  entirely with `adb shell input`, screenshots as evidence: attaches at a
  clean 1:1; a bare `input tap` on a link navigated (fast-tap path,
  press-with-release); a 351px/400ms `input swipe` across a sentence
  selected it starting ~53px in -- **that offset is the 60ms arming delay,
  visible** and worth knowing about if a drag ever "starts late"; adb key
  events after those gestures typed letters, Shift, `,` `!`, space and
  Backspace correctly. No `MediaClient` "media server reported" warning at
  any point, i.e. the bridge rejected nothing.
- **Multi-touch not verified.** `adb shell input` has no pinch. Pinch, pan,
  scroll, two-finger tap, click-while-zoomed, rotate-while-zoomed, and
  leave-mid-drag are the open items in `android/README.md` → Not verified.
  The two Task 1 assumptions were measured on the Pixel 9, not the Pixel
  10; both are AOSP framework behaviour and unlikely to differ, but if a
  pinch does nothing on the Pixel 10, re-run the probe (a static
  `scaleX = scaleY = 2f` on the `SurfaceView` plus a logcat line in
  `onTouchEvent`) before suspecting the interpreter.
- Driving the app via adb, for whoever does that: `adb shell uiautomator
  dump` sees Compose text nodes; on the cover display the host field is at
  about `(400, 1296)` and Connect at `(211, 1467)` in portrait, and the
  `phonetest` row at `(300, 500)` on the drawer. `adb exec-out screencap -p`
  gave an unparseable PNG on this device (multi-display); `adb shell
  screencap -p /sdcard/x.png` + `adb pull` works. The slice-2 note about
  filtering logcat by tag (`MediaClient`), not package, still applies; a
  bridge rejection shows up there as `media server reported: ...`.

### Not done

- Code review. Slice 2's pattern (independent `code-reviewer` +
  `security-reviewer`, three rounds, real bugs found in each of the first
  two) has not been run on this slice.
- Commit. Not asked for; the tree is left ready.
- Of the three slice-2 on-device items, the hardware-keyboard one is mostly
  closed by the adb key-event run above (same `onKeyEvent` path; Enter,
  arrows and an actual Bluetooth keyboard remain); IME autocomplete and live
  resize are still open, and now sit under a second slice of changes to the
  same file.

## M3 slice 3b: media auto-reconnect — implemented and on-device-verified (2026-09-06)

Same session as the gesture slice, committed on `feat/android-session-screen`
after it. Purely client-side (Android); **no Rust changed** — the server
already supported reconnect.

### Why it exists

Testing the gestures surfaced it repeatedly: any media-socket drop stranded
the session on a dead "Disconnected" screen with only "Back to sessions".
On a phone over a tailnet that is a real, frequent failure. The roadmap
(Phase 1) already lists reconnect UX; this is it.

### What it does

- The media socket is pinged every 5s (`MediaClient.PING_INTERVAL_SECONDS`,
  down from a keepalive-only 20s), so a silently-dropped link — a dead
  tailnet route stops delivering frames without closing the TCP socket —
  surfaces as an OkHttp failure within ~5-10s instead of up to 40s.
- On that failure the session screen retries up to
  `MAX_RECONNECT_ATTEMPTS` (5) times on a linear backoff
  (`RECONNECT_BASE_DELAY_MS` × attempt: 1s, 2s, …), showing
  "Reconnecting…"/"Connecting…". A working connection resets the budget.
- Exhausting the budget falls back to a manual "Reconnect" button.
- A real `StreamEnd` (guest window closed) and a decode error are terminal
  and never retried — that precedence is in `SessionOverlay`.

### The design choice that matters

Reconnect is a **clean rebuild**, not an in-place re-open. A Compose nonce
(`reconnectNonce`) keys both the `SessionController` (`remember(host,
sessionName, reconnectNonce)`) and the `SurfaceView` (`key(reconnectNonce) {
AndroidView … }`). Bumping it disposes the old controller (its tested
`close()`) and builds a fresh one against a fresh `SurfaceView`, reusing the
exact `open()`/`close()` lifecycle a first attach uses. This was deliberate:

- `MediaClient` is single-use — its packet `Channel` closes permanently in
  `endStream()`, so the same instance cannot re-open. A fresh client sidesteps
  that entirely.
- The `SurfaceView`'s holder callback binds to one controller; an
  already-created holder never re-fires `surfaceCreated` for a callback added
  later. So a controller swap **without** a new view would render nothing.
  Recreating the view via `key()` re-runs the factory against the new
  controller and rewires it. (There's a comment on the `key()` block saying
  exactly this — don't "optimise" the view recreation away.)

The retry orchestration lives in Compose `LaunchedEffect`s, not in the
controller, so the delicate `SessionController` concurrency (decoder
lifecycle, `lock` ordering, job cancellation) is untouched — the whole reason
the rebuild approach was chosen over teaching the controller to reconnect.

Server side, `crates/navetted/src/media.rs`'s `attach` assigns a fresh
`client_id` and replays codec config + latest keyframe to every new
attachment (its `reconnect_starts_with_config_and_latest_keyframe` test), so
a retry that lands while the session is alive resumes the picture with no
decoder-side special-casing — the replayed `StreamConfig` flows through the
same gate/decoder bootstrap a first attach does.

### Verified on device (Pixel 10 Pro Fold, live Firefox session)

- **Auto-recovery**: video live → wifi off ~8s → socket failed within
  seconds → "Connecting…"/"Reconnecting…" overlay → wifi back → video resumed
  **with no interaction**.
- **Exhaustion + manual**: a longer outage burned all five retries against the
  down network → "Reconnect" button appeared → after wifi returned, tapping
  it rebuilt and resumed the video.
- No `MediaClient` "media server reported" rejection in either run; the
  gesture happy path is unaffected (verified live before the drop test).

CI (`assembleDebug testDebugUnitTest lintDebug`) green throughout; the
reconnect logic is Compose-level and verified on device rather than
unit-tested, consistent with the rest of `SessionController`.

### Foldable gotcha: `cmd device_state state 2` disables the cover screen until reset

To give the virtual uinput touchscreen a live viewport, this session forced
the Pixel 10 Pro Fold's *emulated* device state to OPENED
(`adb shell cmd device_state state 2`). That override makes the OS ignore the
hinge: with the phone physically folded, the inner display stays the active
default and **the cover screen stays off** -- which reads, from the outside,
as "the front screen doesn't work." It survives app reinstalls and
force-stops; only `adb shell cmd device_state state reset` clears it
(`dumpsys device_state` shows `mOverrideState` / `Override Request active`
to confirm). The pointer-location debug overlay (`settings put system
pointer_location 1`) is similarly sticky. **Reset both before handing the
phone back.** Neither is a bug in the app.

## "The front screen doesn't work": root-caused and fixed (2026-09-06, late)

Reported against the Pixel 10 Pro Fold: the cover screen worked for other
apps but navette, when in focus there, "did not work." Three things looked
like that in sequence, and only the last one was the app's:

1. **My `cmd device_state state 2` override** (see the gotcha above) kept the
   inner display active with the phone folded, so the cover was simply off.
   Cleared; not the app.
2. **This phone's fold behaviour raises a dismissible keyguard on fold**
   (`PowerManagerService: Showing dismissible keyguard` in logcat) --
   "swipe up to continue." Same on a real fold, regardless of
   `fold_lock_behavior_setting` (which is a *System* key, not Secure; and the
   Pixel 9 that "works" has it at default too). Not the app either -- and not
   the difference between the phones.
3. **The app, after the fold → keyguard → swipe cycle, showed
   "Disconnected: failed to connect to … after 10000ms" with a manual
   Reconnect button.** Reproduced by emulating the fold with a live session
   (`state 0`, wait, `state reset`, `wm dismiss-keyguard`). Root cause: the
   reconnect loop kept retrying while the app was stopped behind the keyguard
   with its network restricted, burning all five attempts on 10s connect
   timeouts, so the screen the user swiped back to was already dead.

Fix (commit after this note): the retry loop runs inside
`repeatOnLifecycle(STARTED)`, so it pauses while the screen is hidden, and the
budget resets on every return to the foreground and on every decoded frame.
The policy is a pure `ReconnectPolicy` with tests.

**Verified on the Pixel 10, three consecutive runs of the same cycle** (attach
on the cover → emulated unfold → fold back → keyguard for 20s → dismiss →
navette to front): the session screen came back with live video every time,
no manual button, same PID throughout, no `Detach` sent (host
`client_count` unchanged). The preserved log of the third run shows the
mechanism: nothing while hidden; on resume `decoder started at 2204x2128`
(the rebuild, with the server replaying the inner-display config) and two
seconds later `2416x1132` (the live resize to the cover). The same build
also survived 15s backgrounded behind another app.

**One observation not explained:** the very first cycle on this build --
immediately after `adb install -r` had killed and restarted the process --
came back on the *Connect* screen with the host still filled, no error text,
and no session. Host filled means the same ViewModel; no error means the
control socket closed cleanly (`Disconnected`, not `Failed`); no session
means `activeSession` was cleared. Only `connect()` or `leaveSession()` do
that, and neither had an obvious trigger. It did not reproduce in three
further attempts, including one with the identical task ordering (another
app in front, then `am start -n`, which does *not* create a second activity
instance -- checked via `dumpsys activity activities`). The log for that run
was lost to my own `logcat -c`. If it recurs: keep the log, check
`navette ls`'s client count before/after (a `Detach` decrements it), and
`dumpsys activity activities` for a second `Hist` entry.

Unfold with a live session, incidentally, is a **live resize**, not a
reconnect: the activity survives (`configChanges` covers it), the decoder
restarts at the new size (2204x2128 seen in logcat), no socket drop. That
closes slice 2's open "live resize" item.

### Review findings on slices 3/3b, and what was done

An independent `code-reviewer` pass (after the on-device verification --
which is why these survived it) found 3 HIGH, 5 MEDIUM, 4 LOW. All HIGHs and
the MEDIUMs that were real bugs are fixed in the same commit:

- **HIGH -- stale snapshot double-charged the retry budget.** `collectAsState`
  keeps the dead controller's last value for a frame after the nonce swap, so
  a snapshot-keyed effect saw "still dropped" against the new controller and
  charged a second attempt per drop (5 became ~3), and could tear down a
  manual reconnect a second later. Fixed by having the effect wait on
  `controller.state.first { dropped }` -- the controller's own flow. That in
  turn required `SessionController.open()` to call `connect()` *before*
  launching the state collector, or the client's initial `Disconnected` would
  be published and read as a drop.
- **HIGH -- budget reset on socket-open meant a flapping link retried
  forever.** Reset now happens on `contentSize != null` (a decoded frame).
- **HIGH -- a button or axis could reach the wire with no preceding motion**
  (a tap during the 60ms arming window while the stream was still
  bootstrapping: motion dropped at `gate.primary == null`, press sent 60ms
  later once the config landed -- a click at the guest's top-left).
  `sendButton`/`sendScroll` are now gated on a motion having been delivered
  to the *same* surface (`motionSentTo`).
- **MEDIUM -- relative-only pinch slop swallowed right-clicks.** Fingers 40px
  apart latched a pinch on 2px of jitter, and a latched pinch cancels the
  two-finger-tap right-click. Added an absolute floor (`PINCH_SLOP_PX = 8f`)
  required alongside the ratio, plus a jittery-tap test that fails without it.
- **MEDIUM -- zoom/pan reset on every reconnect.** Hoisted into a
  `ViewTransformHolder` the screen owns and lends to each controller.
- **MEDIUM -- `fcb2747`'s message claimed an overlay change it did not make.**
  The rebuild's Connecting phase now genuinely reads "Reconnecting… (n/5)";
  the overlay moved to `SessionOverlay.kt`.
- **MEDIUM -- a 0x0 surface size could be stored.** Rejected at
  `onSurfaceResized`.
- **MEDIUM -- an `OkHttpClient` per reconnect, never shut down.** `close()`
  now shuts the dispatcher executor and evicts the pool.
- LOW, fixed: fingers landing on the same point could never pinch; a phantom
  tracked finger could click at a stale position.

Also from that review, kept as evidence: the controller's thread-confinement
holds for every new field; a mid-drag reconnect does not strand `BTN_LEFT`
in the guest, because `MediaAttachment::drop` → `InputState::disconnect`
releases it server-side.

### The last two LOWs, closed (2026-09-07)

- **A reconnect re-requested focus and so silently dropped a raised IME.**
  `LaunchedEffect(controller) { focusRequester.requestFocus() }` is keyed on
  the controller and so re-runs on every rebuild, pulling focus off the
  hidden IME text field regardless of whether the on-screen keyboard was up.
  `imeRaised` -- previously local to `ImeLayer` -- is now hoisted to
  `SessionScreen` (keyed on `(host, sessionName)`, so it survives a
  reconnect the same way the retry counters and `ViewTransformHolder` do),
  and the effect skips the surface-focus request while it is `true`.
- **`MediaClient.sendInput`'s `Boolean` return was discarded at every call
  site.** Rather than annotate a dozen call sites, the one place that
  actually swallows a failure silently -- `webSocket == null` or
  `WebSocket.send` itself declining -- now logs at debug (not warn: a
  dropped send during a known-bad connection is expected, it's the entire
  reason the retry loop exists) via a small `logDropped` helper both
  branches tail-call.

166 unit tests, CI green, no Rust touched.

### The IME-focus fix, first attempt, measured wrong on device (2026-09-07)

On-device re-verification of the fix above found it incomplete. Test: raise
the on-screen keyboard, drop wifi for 8s, restore it, watch the reconnect.

**What happened on the first build:** the video came back on its own
(correct), the "Hide keyboard" label stayed put (`imeRaised` correctly
survived, as designed) -- but the on-screen keyboard itself had vanished,
and `adb shell input text` afterward navigated the guest to a different
page instead of landing silently in the hidden field.
`uiautomator dump`'s focused node confirmed it: the full-screen surface
`Box`, not the hidden `BasicTextField`.

**Why "decline to steal focus" wasn't enough.** The fix only skipped
`focusRequester.requestFocus()` while `imeRaised`. But the `AndroidView`
holding the `SurfaceView` is itself recreated on every reconnect
(`key(reconnectNonce)`), and the platform's own focus-search assigns that
freshly-attached View native focus regardless of what Compose's
`FocusRequester` bookkeeping says. Nothing in the app requested that focus
move -- the view recreation did it as a side effect. Declining to make our
own request left the field wide open to it.

**The actual fix:** `fieldFocus` (previously private to `ImeLayer`) is
hoisted to `SessionScreen` alongside `imeRaised`, and the reconnect effect
now *actively* asserts the correct target every time it reruns --
`fieldFocus.requestFocus()` + `keyboard?.show()` when raised, the surface's
`focusRequester.requestFocus()` otherwise -- rather than merely omitting
the wrong one.

**Re-verified on the Pixel 10, same recipe:** keyboard visible throughout
the wifi-drop-and-restore cycle this time; `adb shell input text` after
reconnecting landed silently (no guest navigation, page unchanged); the
"Hide keyboard"/"Keyboard" toggle still flips cleanly afterward. 166 tests,
CI green, no Rust touched.

This is the shape of bug that only shows up by actually reconnecting on a
real device with the keyboard up -- neither the unit tests (pure Kotlin,
no Android View focus system) nor the independent code review caught it;
only driving the exact user action did.

## M3 (session screen) merged to master — PR #17 (2026-09-07)

**`master` is at `e2c4642`.** PR #17 squash-merged the whole session-screen
arc: slice 2 (decode + input, `7d089f1`/`8cc011b`), slice 3 (gestures,
`b12c1e2`), slice 3b (reconnect, `10cab7d` onward), and the fixes that came
out of an independent code review plus two rounds of on-device
re-verification (`34be57c`, `eecc9d0`, `e078816`) — full story in the
sections above. All 3 CI checks (`android`, `rust`, `viewer-display`) green
on the PR itself, not just locally. `feat/android-session-screen` is
deleted, both remotely and locally (`gh pr merge --delete-branch`).

**Per `docs/ROADMAP.md`'s Phase 1 ("Phone attach — the demo")**: encoder
bridge, drawer, session screen, resize-follows-viewport, and reconnect UX
are now all done. What's left in Phase 1: **performance HUD**
(fps/bitrate/latency overlay), the rest of the **input-completeness pass**
(keyboard layouts beyond US-QWERTY, IME autocomplete still unverified), and
**PIN-on-attach**. Phase 2 (clipboard, file transfer, wake-on-LAN, multi-host
registry) hasn't been started.

### Still open, honestly, from the test plan in #17
- Hardware Bluetooth/USB keyboard as an actual physical device (adb-injected
  key events cover the same `onKeyEvent` code path and are verified; a real
  keyboard itself is not).
- On-screen IME autocomplete-triggered replacement.

### Environment, as left
- `navetted` (pid varies per run) and `wprsd` for a session named **`mvp`**
  (Firefox) are both running on this machine, bound to
  `100.111.143.67:9417`. `mvp` currently shows 2 attachments in
  `navette ls` — stale from testing, harmless, will clear on its own when
  those sockets time out or the app is closed.
- **Pixel 10 Pro Fold** has the fully-merged build installed and was the
  device all the reconnect/IME re-verification ran against.
- **Pixel 9 Pro Fold** still has an old **probe build** from slice 3's Task 1
  measurement (a static 2x `SurfaceView` scale + a debug logcat line) —
  reinstall the real APK before using it for anything real.
- No lingering `device_state` overrides or debug `settings` on either
  phone; both were explicitly reset after use (see the fold-override
  gotcha above).
- Unrelated: the Pixel 10 has a third-party app, `com.ventouxlabs.bascule`
  ("Bascule" / VitalForge scale), with an "Always-on foreground fallback"
  service that periodically brings itself to the foreground on its own.
  Nothing to do with navette — if the phone unexpectedly shows Bascule
  instead of whatever you left running, that's why.

### Where to look
- `docs/HANDOFF.md` (this file) for the full session-by-session history,
  including two root-caused bugs (`invalid_input` u64 signedness; the
  fold/keyguard reconnect-budget exhaustion) and the two-pass IME-focus fix
  that only revealed itself on a real device.
- `android/README.md` for the current Verified/Not-verified/Known-limitations
  split, kept in sync with every slice.
- PR #17 on GitHub for the itemized commit-by-commit story and CI links.

## Android performance HUD: landed and on-device-verified, one real environment bug found along the way (2026-09-08)

**What landed.** A performance overlay on the session screen (fps, bitrate,
decode time, frame age, round-trip time, dropped packets, discontinuities),
toggled by a two-finger long-press held past 250ms; repeating the gesture
hides it. `crates/navette-protocol` grew a `MediaInput::Ping{nonce}` /
`MediaServerMessage::Pong{nonce}` pair, answered by a small task on `navetted`'s media
socket -- deliberately not routed through the bridge loop, so a stalled
bridge doesn't also kill the one signal that would reveal the stall (`AGE`
is the overlay's answer to that same gap: it stays live even when `RTT`
can't see a bridge-loop problem). Full suites green going in: `cargo test
--workspace` 182 passed / 1 pre-existing ignored; Android 188 passed (JUnit
XML in `app/build/test-results/testDebugUnitTest/`, both suites run with
`--rerun-tasks`).

**Measured on-device (Pixel 10 Pro Fold, Android 17, `mvp` / Firefox, real
tailnet at `100.111.143.67:9417`):**

| condition | FPS | KBPS | DEC | AGE | RTT | DROP | DISC |
|---|---|---|---|---|---|---|---|
| fresh two-finger-long-press toggle | 21.3 | 24 | 27.0ms | 502ms | -- (stale pre-rebuild daemon, see below) | 0 | 3 |
| idle, a few seconds later | 0.0 | 0 | 47.0ms | 5623ms | 34ms | 0 | 6 |
| actively scrolling | 16.7 | 2746 | 29.0ms | 21ms | 63ms | 0 | 6 |
| wifi dropped ~8s (still blank a few seconds after wifi came back, too) | 0.0 | 0 | -- | -- | -- (blank, not frozen) | 0 | 0 (fresh controller) |
| wifi restored, actually recovered | 0.0 | 0 | 58.0ms | 11556ms | 38ms | 0 | 2 |
| after a forced live resize | 0.0 | 0 | 23.0ms | 15305ms | 33ms | 0 | 5 (+1, this resize) |
| against the old (pre-ping) daemon | 0.0 | 0 | 22.0ms | 6366ms | -- (permanently blank) | 0 | 5 |
| daemon swapped back to the new (ping-supporting) binary | 0.0 | 0 | 51.0ms | 14630ms | 43ms | 0 | 2 |

`RTT` ranged 20-63ms across samples -- a plausible tailnet figure, never
zero or blank during a healthy link. `DROP` read `0` within every
controller's lifetime; a real reconnect rebuilds the controller and resets
every counter (`DISC` was observed going 6 → 0 → 2 across one reconnect), so
a post-reconnect `DROP 0` covers only the new attachment, not the whole
session -- worth knowing before reading a single sample as a session-wide
guarantee. A quick (<250ms) two-finger tap still opened the guest's own
right-click context menu, confirmed precisely on a real Firefox link, so the
new gesture didn't regress the existing one. A forced live resize incremented
`DISC` by exactly one and nothing else.

**12 real enter/leave cycles**, each confirmed by a fresh `H264Decoder:
decoder started at ...` logcat line rather than by key-event count alone --
worth stating plainly because a first pass at this check used a stale
landscape coordinate for the drawer's `mvp` row against an idle guest,
silently drifted off the app via a `KEYCODE_BACK` that landed on an unrelated
foreground app (the same `com.ventouxlabs.bascule` app noted in "Environment,
as left" below), and would have measured nothing while looking like a clean
pass. Re-run with the drawer's actual bounds, a scroll before each cycle so
frames were genuinely flowing, and the decoder-start count as the real
evidence: 12 cycles, 12 decoder starts, no hang, no ANR, no surface-abandon
warning, same process (`pidof`) throughout. This is the reproduction for the
codec-callback/teardown lock hazard fixed earlier in this arc.

**A real environment bug, found rather than assumed.** The `navetted`
handed off as "already running" for this task turned out to be a stale
release binary (`Sep 5 11:01`, predating this branch's ping-handler
commits) -- its media socket answered every `ping` with `invalid_input:
unknown variant \`ping\``, which is exactly the old-daemon symptom Step 4
was supposed to go looking for deliberately, showing up by accident first.
Rebuilding `navetted --release` from the branch tip and restarting it (same
bind address, existing `mvp` registry entry reconciled cleanly against the
still-running `wprsd` -- no session loss) fixed it; `RTT` went from
permanently blank to a live 20ms immediately, with no app-side interaction
needed, confirming the client's own reconnect/retry path handles a daemon
restart as just another transient drop.

**The two-finger gesture, driven for real, not deferred.** The known
scratchpad `uinput` driver from earlier sessions was gone, so it was rebuilt
from AOSP's `cmds/uinput` JSON schema (`register`/`inject`/`delay` over
`adb shell uinput -`) rather than deferring the check to a human. The
device's touch-coordinate transform was measured, not assumed: a raw touch
at portrait-native `(x, y)` lands on the landscape-locked session screen at
`(screenX, screenY) = (y, 1079 - x)`, confirmed with the `pointer_location`
debug overlay before relying on it for the real gesture sequences. Every
check in the brief that depends on multi-touch (long-press toggle both
directions, quick-tap right-click, the resize/`DISC` check, the ten-plus
enter/leave cycles) was driven this way and is not a deferred human-check.

**Step 4 (old-daemon compatibility), done with one deliberate deviation from
the brief.** `master`'s `navetted` (`390aaa9`, no ping handler) was built in
a throwaway `git worktree` and pointed at the *same* already-running `mvp`
session (via `--state-file` pointed at the real registry, default
runtime-dir, so it reconciled against the live `wprsd` instead of spawning a
new one) -- confirmed by the `invalid_input`/`unknown variant \`ping\`` log
lines and the permanently-blank `RTT` row in the table above, with the
session otherwise working normally (video decoded, `DROP` still `0`). The
brief asked for this on a *different port*; the Android app's Connect screen
has no port field and always appends the default `9417`
(`net/NavetteClient.kt`'s `controlWebSocketUrl(host, port = 9417)`, and a
combined `host:port` string is explicitly untrusted input there -- typing
one produces `Invalid URL port: "9417:9417"`, not a working override), so
there's no way to point the *Android app itself* at a non-default port
without a code change. Ran the old daemon on `9417` instead, with the real
(new) daemon stopped for the duration -- confirmed no double-supervision of
`mvp` by checking `navette ls` showed exactly one entry throughout -- and
swapped the new daemon back onto `9417` immediately after, confirming `RTT`
resumed live (`43ms`) with no app-side interaction. Two side quests this
uncovered, neither novel: `wprsd`'s own Xwayland spawn needs a config with
`enable_xwayland: false` in this environment (same underlying gap the M2
handoff already recorded -- XWayland spawn fails here) to avoid colliding
with the already-running session's own Xwayland display, and a stray
`navette-<name>`/`navette-<name>.lock` pair left in `/run/user/1000` by an
aborted `wprsd` attempt will hang the *next* attempt at the exact same
wayland-display name, silently, until removed.

**Left as found:** `navetted` (new binary, ping handler included) and
`wprsd` for `mvp` both running on `100.111.143.67:9417`; no `device_state`
override or `pointer_location` debug setting left on the Pixel 10 Pro Fold;
no daemons left on `9418`/`9419`; the throwaway `git worktree` and every
scratch file it produced (registry snapshots, a copied `xwayland-xdg-shell`
binary, stray runtime dirs) removed.

### Multi-window guest, verified on device (2026-09-08)

The one item the PR's test plan left unchecked. The per-stream counter fix
(`e1a1716`) had seven unit tests but had never run against a guest with two
toplevels — the exact case it was written for.

**Setup.** `foot` launched on the `mvp` session's display
(`WAYLAND_DISPLAY=navette-mvp`) printing a date once a second, alongside the
existing Firefox window, giving two live streams. A dependency-free
`websockets` probe read the media socket directly to confirm the shape before
touching the phone:

```
DISTINCT STREAMS: 2
  stream_id=1  seq 1467..1468  size=(2416, 1132)
  stream_id=2  seq 1..68       size=(696, 496)
SEQUENCE SPREAD between streams at attach: 1466
```

That spread is the quantity that used to corrupt `DROP`: pre-fix, a single
counter was shared across streams, so every transition from the low-sequence
stream to the high one added roughly that number.

**The app confirmed it was in the contaminating configuration**, from logcat:

```
D H264Decoder: decoder started at 2416x1132
I StreamGate: ignoring stream 2; this screen renders only the primary stream
```

Stream 2 is ignored for *rendering* but its packets still flow through
`route()` into the HUD, which is precisely the path that was wrong.

**Result — HUD read during active painting, both streams live:**

```
FPS 23.3  KBPS 3260  DEC 25.0MS  AGE 351MS  RTT 30MS  DROP 0  DISC 2
```

`DROP 0`, held across four samples over ~3 minutes. The two-finger long-press
toggled the HUD, re-confirming that gesture on the cover display.

**Two honest limits on this run.** `KBPS` reflecting *only* the sampled
stream was not independently isolated — `foot`'s byte contribution was small
relative to Firefox's and the two were not measured apart, so assertion 4 in
`SessionHudTest` remains the only evidence for that column. And getting
Firefox to repaint needed a tap through the app to give it keyboard focus
first; `wtype` alone reached the compositor but not the window, which is why
three earlier samples read `FPS 0.0` with `AGE` climbing — honest idle, not a
HUD fault.

**Environment restored**: `foot` killed, back to one stream, `navetted` and
`wprsd` still running for `mvp`, app force-stopped, no debug settings or
`device_state` override left on the phone.

### Known follow-ups from this branch

Findings that were reviewed, judged non-blocking, and deliberately carried
rather than fixed. Recorded here because the review workspace they were
tracked in is scratch and does not survive.

1. **`hudJob` pings and republishes forever once the reconnect retry budget
   is spent.** `close()` never runs in that state, so the loop keeps sending a
   1 Hz ping at a dead socket and republishing a sample whose `AGE` changes
   every tick, so conflation never suppresses it. Battery and recomposition
   cost behind a dead-session overlay; not a correctness defect. **This is the
   next piece of work on the HUD.**
2. **`SessionScreen.kt` is 918 lines against this project's 800-line
   ceiling** (it was 827 before this branch). Extracting `SessionController`
   is a larger change than the feature was and belongs on its own branch. The
   largest thing still owed on this file.
3. **`surfaceDestroyed` can return while a superseded decoder is still inside
   `MediaCodec.release()`** on another thread. Bounded by one `stop()`, and
   strictly better than what preceded it — hoisting teardown out of the
   controller lock closed a per-frame contention window against `release()`.
   No lock rearrangement inside `startDecoder` closes the residue; the real
   fix is confining `startDecoder` to one thread so the packet loop and
   `surfaceCreated` can never have two calls in flight, which would also
   delete the `superseded` capture apparatus. That is a rewrite, not a patch.
   12 on-device enter/leave cycles showed no hang, ANR or surface-abandon.
4. **`SessionHudOverlay` has no `maxLines`/overflow bound** and `11.sp` scales
   with the accessibility font setting. At ~2x scale the translucent ground
   can sit under the keyboard-toggle button. Cosmetic only:
   `Modifier.background()` registers no pointer-input node, so the button
   stays tappable regardless. One-line fix when convenient.
5. **Nothing pins that `sample()`'s global half survives a null `streamId`.**
   The code is correct — `fps`, `decodeMs`, `ageMs` and `rttMs` are computed
   outside the per-stream chain — but no test would notice if that stopped. A
   future "blank everything when there is no stream" simplification would pass
   all 195 tests while destroying the pre-bootstrap and post-`Ended`
   diagnostic the overlay exists for: a healthy `RTT` beside a climbing `AGE`.
   Costs one assertion on a sample already in hand.
6. **`gate.primary` is read outside `lock` while the sample is taken inside
   it.** A reconfigure landing in that window yields one sample whose
   per-stream figures describe the outgoing stream, self-correcting on the
   next tick. Not fixable by widening the lock — `StreamGate` is guarded by
   its own `@Volatile`, not by `SessionController.lock`.
7. **`SessionHud`'s zero-span `rate()` regression test pins `fps` but not
   `bitrateBps`**, though both go through the same function.
8. **`startDecoder` last-writer-wins ordering race — found late, fixed, not
   carried.** Listed here rather than silently closed because of how it was
   found: an independent Codex review, run after this branch's own nine
   reviews had all passed, spotted it. `startDecoder` is reachable from two
   threads and has two lock windows with a gap between them; an older call
   resuming inside either window would stop the newer decoder — via window
   1's unconditional capture, or via window 2's `superseded` — and either
   publish its own stale stream or leave the Surface with nothing until the
   next reconfigure. Every call now claims a generation under `lock` before
   touching anything, and both windows stand down for a superseded claim, so
   the newest call wins rather than whichever resumes last. Both halves are
   guarded; no residual window is known. Both checks sit inside existing lock
   windows and move no boundary — the constraint that matters on this
   function, where the two rounds that moved a boundary each traded one race
   for another, and the two that only added a check inside an existing window
   introduced nothing. The claim itself is a third, disjoint critical section:
   a single `Long` increment, no call-out while held, no nesting with
   `H264Decoder`'s own lock, and neither caller holds `lock` at the call site.
   It also stops an older call from blanking `contentSize` after a newer
   decoder has already reported its frame size — a second, smaller defect the
   fix was not aimed at.

   The `superseded` capture in window 2 is now provably unreachable (any call
   that publishes held the highest claim, and a lower claim returns before
   publishing) and is kept anyway, because a future edit could break that
   invariant with nothing to notice. That redundancy is the argument for item
   3 above rather than a defect on its own: confining `startDecoder` to one
   thread would delete the generation counter and the `superseded` capture
   both, and is still the shape this function wants.

Two deliberate non-changes, so nobody "fixes" them later:

- **`KBPS` counts payload bytes; the desktop viewer counts payload plus the
  44-byte header** (`client.rs:342`). Under 1% at real bitrates. The Kotlin
  matches this design's own metrics table, and changing it would make the
  measured figures recorded above non-reproducible.
- **The design spec contradicts itself about `KBPS`'s source** (its metrics
  table says `payload_len`; its parity paragraph says "exactly as `hud.rs`").
  Left as an honest artefact of the design pass rather than rewritten after
  the fact; the truth lives in `android/README.md`'s Known limitations, which
  is where anyone comparing the two clients will actually look.

## Clipboard sync: on-device verification, one real bug found in the Android reconnect path (2026-09-08)

**Rebuild/restart evidence.** The running `navetted` (PID 4032419, launched
~22 hours earlier, binary timestamped Sep 8 00:39) predated ten source files
including every clipboard file — confirmed stale before touching it.
`cargo build --release` succeeded; `find crates -name "*.rs" -newer
target/release/navetted` returned empty immediately after, and again after
every subsequent rebuild in this session. The stale daemon was killed and a
fresh one started against the same tailnet address, the same already-running
`wprsd` (session `mvp`, `org.mozilla.firefox`), confirmed via `navette ls`
surviving the daemon restart. Device: Pixel 10 Pro Fold, `57211FDCG0023C`,
folded (cover screen, landscape once attached) throughout.

Two temporary, content-free `tracing::debug!` lines (variant name and
byte/mime counts only, per the existing rule in `bridge.rs` that clipboard
content itself must never be logged) were added to `handle_guest_data`,
`apply_sync_action`, and the `SetClipboard` arm of `pump_input` to make the
state machine's actions observable without a wprsd-level log target. Used to
gather every finding below, then reverted before this commit — `git diff
crates/navetted/src/bridge.rs` is empty.

**The six checks:**

1. **Guest → phone: PASS.** Selected `fedoraproject.org/start/` in Firefox's
   address bar (an XWayland client) and copied with a real `Ctrl+C` sent to
   the phone's focused session. It arrived on the Android system clipboard
   and was confirmed two ways: Gboard's clipboard-suggestion strip showed
   `https://fedoraproject.org/start/` immediately, and tapping **Paste** in
   the stock Settings search field inserted it verbatim.
2. **Phone → guest: the Android UI path FAILED, 3/3 attempts — a real,
   deterministic bug, not flakiness. Fixed and re-verified the same day; see
   "Fixed and re-verified" below.** The underlying daemon/wprsd/XWayland
   pipeline was verified directly first (bypassing the then-broken UI) by
   opening a second WebSocket to the same session's media endpoint and
   sending `{"type":"set_clipboard","text": "ScriptProbeMarker"}` by hand:
   `clipboard: phone SetClipboard len=17` → `action OfferToGuest` with the
   five canonical MIME types, and a `Ctrl+V` into Firefox's address bar
   (real device, real guest) inserted `ScriptProbeMarker` correctly. So the
   wire protocol and the daemon-side state machine were sound on real
   hardware from the start; the break was entirely in the Android client's
   reconnect wiring, and is now fixed there.
3. **Echo-loop check: no repeated traffic observed on either direction, on
   real hardware.** Guest → phone: one address-bar copy produced two
   `SelectionOffered` events from Firefox milliseconds apart (it re-announces
   the same selection with an updated target list) and correspondingly two
   `TransferFromGuest` deliveries — but only the first produced
   `PushToPhone`; the second returned `Nothing` via the `awaiting_guest_transfer`
   guard, so the phone received the text exactly once despite Firefox's
   double announcement. Phone → guest (the direction the open question is
   about): after `OfferToGuest` fired for `ScriptProbeMarker`, several
   bridge-loop iterations passed with no further clipboard activity in the
   log before the guest actually pulled the data; after the guest's paste
   completed (`PasteRequested` → `AnswerGuest{bytes:17}`), several more
   iterations passed with still no `SelectionOffered` coming back from wprsd.
   No echo observed in either window.
4. **Unanswered-pull check: UNRUN — could not be driven to the scenario it
   tests, and here is exactly why.** The check needs a guest paste while
   `phone_text` has *never* been set on that session. Two blockers, both
   recorded honestly rather than papered over:
   - A genuinely fresh session (`check4`, then `check4b`, both
     `org.mozilla.firefox`) failed to start in this environment on both
     attempts: `session bridge stopped session=check4 error=failed to
     connect to /run/user/1000/navette/check4/wprs.sock`, with Firefox
     falling back to X11 (`DISPLAY=':0'`) inside its own sandbox. This
     reproduced identically for `check4b` after a longer wait, so it reads as
     environment-specific (this sandbox's second-`wprsd` spawn path), not a
     timing race and not a clipboard-code issue.
   - On the existing `mvp` session, restarting `navetted` does reset
     `ClipboardSync`'s in-memory `phone_text` to `None` (confirmed: the
     daemon has no on-disk state), but the guest's own paste was still
     satisfied with old text (`ScriptProbeMarker`) — and *zero*
     `PasteRequested` lines appeared in the log for that paste. Firefox/XWayland
     evidently retains its own copy of the last-transferred CLIPBOARD value
     and satisfies later pastes locally, without asking navetted again. That
     means once any transfer has ever happened on a session, this specific
     hang scenario can no longer be observed from the guest side on that
     session, by design of X11's clipboard-caching convention — a real
     constraint on how this check can ever be exercised on-device, not a
     defect. The unit-level guarantee
     (`a_paste_with_no_phone_text_is_still_answered` in `clipboard.rs`)
     remains the only verification of this path; it was not confirmed
     on-device this session.
5. **Oversized check (>1 MiB from the phone): originally reported as one
   band, actually spans two, and the first write-up here got the boundary
   wrong. Corrected below, after the controller's second review caught it.**
   The daemon enforces two different limits, not one: `MAX_INPUT_MESSAGE`
   (`crates/navette-protocol/src/media.rs:10`, **16 KiB**) is the size the
   graceful handler in `api.rs` refuses with an `Error` text frame on an
   otherwise-live connection; `MAX_INPUT_MESSAGE * 2` (`api.rs:127`, **32
   KiB**) is the WebSocket transport's own hard cap, enforced by
   axum/tokio-tungstenite on the read path *before* that graceful handler
   ever sees the frame. My original test sent 1,049,076 bytes — comfortably
   inside the second, harder band, not the first — and reported "refused,
   not truncated, session survives" without saying which band that was, or
   that the *daemon and `mvp` session* surviving is a different claim from
   *the sending connection* surviving; that connection is in fact torn down
   in this band, not gracefully refused.

   Re-tested both bands precisely, directly against the daemon, after this
   session's fix wave added a matching client-side guard (see "Fix wave"
   below):
   - **16–32 KiB (the graceful band):** a 20,000-byte `SetClipboard` got
     back `{"type":"error","code":"message_too_large","message":"input
     message exceeds 16384 bytes"}`, and the *same connection* answered a
     follow-up ping afterward — genuinely refused, not torn down.
   - **>32 KiB (the hard band):** a 40,000-byte `SetClipboard` reset the
     connection outright (`ConnectionResetError`, no close frame) — the
     symptom the controller described: video drops, that client
     reconnects. The daemon and `mvp` stayed up throughout
     (`/healthz` `ok`, `navette ls` still showing `mvp`), which is real but
     is a claim about the daemon, not about that connection.

   The fix (below) makes this moot for the real client: it now refuses to
   send anything whose *encoded* frame exceeds 16 KiB, so the Android app
   can no longer reach either band — this stays as a direct probe of the
   daemon's own behavior, not a description of what the shipped client can
   still trigger.
6. **MIME spellings, real guest.** Firefox (an XWayland client) offered, in
   one `SelectionOffered`: `text/plain;charset=utf-8`, `UTF8_STRING`,
   `COMPOUND_TEXT`, `TEXT`, `text/plain`, `STRING` — with
   `text/plain;charset=utf-8` and `text/plain` each appearing twice, and a
   second, immediately-following announcement adding `SAVE_TARGETS`.
   `select_text_mime`'s preference order picked `text/plain;charset=utf-8`
   both times, correctly matching the guest's own spelling rather than a
   normalised one. Resolves spec open question 2 for at least one real,
   common XWayland guest.

**Open question 1 — does wprsd echo `SetSelection` back to us on the
XWayland path? Not observed, under real but limited conditions.** Across two
separate windows on this hardware (a bare `OfferToGuest`, and an
`OfferToGuest` followed by a real guest paste through to `AnswerGuest`), no
`SelectionOffered` arrived back from wprsd attributable to our own push. This
is the first real evidence on this question in either direction — it settles
the *pure*-Wayland path from source (Task 6: `set_clipboard_selection` never
calls `handler.new_selection`) and now adds a negative on-device result for
XWayland specifically, one guest, one host, short observation windows. It
does not prove absence. Per the controller's asymmetry-of-harm ruling, this
makes removing the `echo_from_guest` one-shot token a candidate for a future
evidence-backed change rather than a guess — but one session's evidence is
thin, and `ClipboardSync` was deliberately left unchanged this session; the
decision stays with whoever owns that ruling.

**Device hygiene.** No `device_state` override was ever applied this
session (`mOverrideState=Optional.empty` confirmed both before touching the
device and again at the end) and `pointer_location` was already `0`. Nothing
to reset.

**Finding — phone → guest clipboard was silently dropped on every reconnect
through the real Android UI. Found, fixed, and re-verified on-device the
same day.** Reproduced 3/3 times, always with the identical Android logcat
line `MediaClient: dropped SetClipboard(text=...): no socket`. Root cause,
confirmed from source: `SessionScreen.kt`'s `DisposableEffect(controller)`
(line 177) registers a `LifecycleEventObserver` on the *already-resumed*
lifecycle (`lifecycleOwner.lifecycle.addObserver(clipboardObserver)`, line
196) — which, per `androidx.lifecycle` semantics, synchronously replays the
missed `ON_RESUME` event the instant it is added — and only calls
`controller.open()` on the very next line (197). `controller.open()` is what
calls `client.connect()`, which is what sets `MediaClient.webSocket`
non-null. So the resume-triggered clipboard read (`onLocalClipboardResume`,
added specifically so a copy made while Navette was backgrounded isn't
missed — see `ClipboardBridge`'s own doc comment) fires and calls
`sendInput` *before* a socket exists on every single reattach, not merely on
a lucky/unlucky timing window. `MediaClient.sendInput`
(`net/MediaClient.kt:153`) drops silently in that case (`logDropped`,
debug-level, "no socket") and nothing retried it. Net effect: the ordinary
real-world flow — copy something in another app, switch back to Navette —
lost that copy every time a reconnect happened on the way back in, which in
this environment was every time (each background period was long enough
that the media socket was torn down, observed as `WebSocket receive failed
error=IO error: Connection reset by peer` in `navetted`'s own log). It was
the one thing in this branch most likely to make a real user conclude
"clipboard sync doesn't work," since it hit the single most natural way to
use the feature.

### Fixed and re-verified, same day (2026-09-08, later)

**The fix.** A controller review flagged that reordering
`controller.open()` ahead of `addObserver` was tempting but not obviously
sufficient — `connect()`'s handshake is asynchronous, so even a send issued
right after it returns is not guaranteed to land on an open socket — and
asked for whichever hook actually closes the gap, verified on-device rather
than by reasoning, without relocating the lifecycle calls this file has
already burned four rounds getting right. The fix stayed entirely inside
`SessionController` (`SessionScreen.kt`, next to `onLocalClipboard` /
`onLocalClipboardResume`): both now route through a new
`sendClipboardOrRetryOnConnect(text)`, which sends immediately and, only if
`MediaClient.sendInput` returns `false`, waits on
`client.connectionState.first { it is ConnectionState.Connected }` and sends
once more. A second call before the wait resolves cancels the first
(`pendingClipboardResend: Job?`), so at most one retry is ever pending and
only the latest text is the one that eventually goes out. The retry is a
child of `scope`, so `close()`'s existing `scope.cancel()` already tears it
down — no new teardown path was added. `git diff` on this fix touches only
that one region of `SessionScreen.kt`; the `DisposableEffect`'s lifecycle
ordering is untouched.

**Re-verified on-device, 3/3.** Same repro that failed 3/3 before: copy a
fresh marker in Settings' search field, switch back to Navette (a real
reconnect every time — the media socket does not survive backgrounding in
this environment). All three attempts (`FixVerify1`, `FixVerify2`,
`FixVerify3`) still logged the initial `dropped SetClipboard(...): no
socket` — the drop itself is unchanged, and unavoidable given the
lifecycle-observer ordering the controller asked not to touch — but all
three then arrived correctly in the guest, confirmed by pasting into
Firefox's find-in-page bar (chosen to avoid the address bar's own
autocomplete, which produced a false positive during the original
verification) and reading back the exact marker text each time. Zero data
loss across 3/3, where before it was 3/3 permanent loss.

**Echo-loop re-check.** Re-ran check 3 with the fix in place, using the same
temporary content-free instrumentation as the original verification (added,
used, and fully reverted again — `git diff crates/navetted/src/bridge.rs` is
empty). Guest → phone: still exactly one `PushToPhone` despite Firefox's
double `SelectionOffered`, unchanged from before — this direction was never
touched. Phone → guest, the direction the fix changes: for each of two
copies tested after the fix (one carried over from the 3/3 re-run, one
fresh, `EchoRecheckMarker`), the daemon log shows **exactly one**
`clipboard: action OfferToGuest` per copy — the failed first attempt never
left the device at all, so there was nothing to duplicate, and the retry's
single resend is the only transmission that ever reaches the wire. No
looping, no double delivery.

**Unit test.** Added `sendInput before connect is dropped, not silently
queued for later` to `MediaClientTest.kt` (plain JVM, `MockWebServer`, this
project's existing no-mocking convention — no instrumentation needed). It
pins the exact one-layer-down behavior the fix depends on: a
`MediaClient.sendInput` call issued before `connect()` returns `false`
cleanly rather than queuing for later delivery. It does **not** reach
`SessionController.sendClipboardOrRetryOnConnect` — that class is
file-private in `SessionScreen.kt` and reaching it from a separate test file
would mean widening its visibility, which is a bigger change than this fix
warranted. Said plainly rather than writing a test that exercises
`SessionController` through some indirect, vacuous path: the retry logic
itself is verified only by the on-device 3/3 re-run above, not by a unit
test. Full suites green: `cargo test --workspace` 212 passed across all
crates / 1 pre-existing ignored / 0 failed; Android `testDebugUnitTest` 210
passed / 0 failed (209 before this test, +1 for the new one).

### Fix wave: five findings from the whole-branch review, all fixed and re-verified (2026-09-09)

The whole-branch review came back **Ship with fixes** — five findings, all
small and local, three of them in the same files this session had already
been touching. All five are fixed and re-verified on-device.

1. **Clipboard text was reaching logcat.** `MediaClient.kt`'s `logDropped`
   did `Log.d(TAG, "dropped $input: $reason")`; `MediaInput.SetClipboard` is
   a data class, so `$input` rendered `SetClipboard(text=<the user's
   clipboard>)`. Reached from both the no-socket and the socket-declined
   paths — the "Fixed and re-verified" section above quotes this exact line
   firing during the original 3/3 repro. Fixed to log the variant name
   only: `Log.d(TAG, "dropped ${input::class.simpleName}: $reason")`. Every
   logcat line captured during this session's re-verification (see check 2
   re-run below) reads `dropped SetClipboard: no socket` — confirmed
   directly, not just by reading the diff.
2. **`lastSent` was committed at decision time, not at confirmed delivery.**
   `ClipboardBridge.onLocalClipboard` wrote `lastSent = text` and returned;
   if the send then lost the race against the socket and `SessionScreen`'s
   retry (added earlier this session) also never resolved — `close()`
   cancelling it mid-wait, for instance — `lastSent` already held that text.
   The next resume reading the same still-undelivered clipboard would see
   `lastSent == text`, decide there was nothing to send, and that text would
   never reach the guest for the rest of the session, silently. Fixed by
   splitting decision from confirmation: `onLocalClipboard`/
   `onLocalClipboardResume` no longer touch `lastSent` at all; a new
   `ClipboardBridge.markSent(text)` does, called from
   `SessionController.sendClipboardOrRetryOnConnect` only where
   `client.sendInput` actually returned `true` — on both the immediate and
   the retried attempt.
3. **A phone copy over 32 KiB could tear down the media socket.** See the
   corrected check 5 above for the two-band breakdown this finding is
   about. Fixed with a matching client-side guard: `ClipboardBridge` now
   measures the *encoded* JSON frame (not the raw text — JSON escaping
   inflates quotes/backslashes 2x and control characters 6x, so a raw-byte
   guard would pass text that still cleared the limit) against
   `MAX_INPUT_MESSAGE_BYTES` (16 KiB, mirroring
   `crates/navette-protocol/src/media.rs:10`'s `MAX_INPUT_MESSAGE`), and
   refuses anything over it before `SessionController` ever calls
   `sendInput`. This **replaces** the old `MAX_CLIPBOARD_BYTES` (1 MiB)
   guard, which never actually protected anything — it compared raw text
   bytes against a limit far above where the transport itself acts.
4. **An empty guest transfer became the phone's clipboard.** `clipboard.rs`
   decoded an empty `TransferFromGuest` to `""`, which passed the size and
   echo checks, so `phone_text = Some("")` and every later guest paste got
   answered with empty bytes until the phone genuinely copied something.
   Task 5 deferred this as handled downstream, which was true before a
   later ruling added the `phone_text` write on this exact path and
   reintroduced it. Fixed with an early `if text.is_empty() { return
   SyncAction::Nothing; }`, mirroring the Kotlin side's own `text.isEmpty()`
   check. Unit test added:
   `an_empty_guest_transfer_is_dropped_and_does_not_become_phone_text`.
5. **A resume could never re-forward a value the guest had already
   received, even long after the phone's clipboard cycled back to it.**
   `ClipboardBridge.onLocalClipboardResume` returns null whenever the text
   equals `lastRemote`, and nothing ever cleared `lastRemote` except another
   remote push. So a value the daemon once pushed here could never again be
   sent phone→guest via resume — the path the spec calls the one most real
   transfers take — even after an intervening genuine local copy had moved
   the daemon's own `phone_text` on past it, at which point a guest paste
   should get the newer value again, not the stale one the resume path
   silently refused to re-send. Fixed: `onLocalClipboard` now clears
   `lastRemote` whenever it decides a genuine send (not an echo, not an
   unchanged resend) — the phone's clipboard has, by definition, moved past
   whatever the daemon last pushed once that decision is made. Unit test
   added: `a resume forwards a remote value again once an intervening send
   has moved past it`.

**Re-verification, on device, not by reasoning.** Rebuilt and reinstalled
both binaries, restarted `navetted` fresh (`find ... -newer` empty), ran the
exact same repro that failed 3/3 before any of this session's fixes existed:

- **3/3 pass, and confirmed content-free logcat.** Three fresh markers
  (`Wave5Verify1/2/3`), same copy-in-Settings-then-switch-back flow. All
  three logged `dropped SetClipboard: no socket` — no text, matching
  finding 1's fix — and all three then arrived correctly in the guest,
  confirmed by pasting into Firefox's find-in-page bar and reading back the
  exact marker each time. A full logcat scan for any of the three marker
  strings turned up nothing from the app (`adbd`'s own log of the `adb
  shell input text` commands that typed them is the only match — an
  artifact of the test tooling, not a leak).
- **Echo-loop re-checked again, with the full fix wave in place.** Same
  temporary content-free instrumentation as both earlier rounds (added,
  used, fully reverted — `git diff crates/navetted/src/bridge.rs` is empty
  at every checkpoint in this log). Guest → phone: still exactly one
  `PushToPhone` despite Firefox's double `SelectionOffered` — unaffected,
  since none of these five fixes touch that direction. Phone → guest, with
  a fresh marker (`EchoRecheckW5`) run through the same drop-then-retry
  path finding 2 changed: **exactly one** `OfferToGuest` reached the
  daemon, and the marker arrived correctly. Deferring `markSent` to
  confirmed delivery did not introduce a duplicate send.
- **Both size bands re-measured precisely** (finding 3) — see the corrected
  check 5 above.
- Full suites green: `cargo test --workspace` 213 passed / 1 pre-existing
  ignored / 0 failed (+1 for the empty-transfer test). Android
  `testDebugUnitTest` 213 passed / 0 failed (210 before this wave, +3: the
  markSent-not-confirmed regression test, the lastRemote-cleared-by-a-send
  test, and one of the two rewritten size-cap tests that is net new rather
  than a rename).

**Carried follow-ups (from prior task reviews, recorded here for the
permanent record):**
- `RequestDataTransfer(DataSource::Primary, ..)` is still unanswered — the
  same hang shape as the `Selection` path fixed in this branch, but
  pre-existing and deliberately out of scope (`bridge.rs:2119` is the only
  reference to `DataSource::Primary`, and it is not routed through
  `ClipboardSync`).
- `navette-bridge/src/input.rs:553` still uses `Serializer::new_server`,
  which widens the process-wide umask around its bind. Harmless today, but
  the first unguarded `tempfile::tempdir()` added to that crate's tests will
  start flaking — measured in `navetted`: 5 failures in 10 runs before the
  workaround.
- `SessionScreen.kt` is now **1041 lines** against this project's 800-line
  ceiling (measured this session; it was ~1008 at last count). Extracting a
  `SessionController` remains deferred and is now overdue.
- The stale-echo-token window is real but bounded: exactly one legitimate
  same-text copy can be dropped, then the state self-heals on the next
  distinct value. No correct in-layer fix exists — it needs a correlation id
  wprs does not carry.
- Same-text equality is a heuristic throughout `ClipboardBridge` and
  `ClipboardSync`; a genuine resume whose local clipboard coincidentally
  equals the daemon's last push is suppressed. Unrelated to, and not
  fixed by, the resume-ordering bug found above — that bug drops the send
  before this heuristic even runs.

### Follow-ups surfaced by the final fix-wave re-review (2026-09-09)

Three observations from the last review pass. None blocks merge; all three were
verified as pre-existing or by-design, and none was introduced by the fix wave.

- **A parked clipboard retry can fire after newer text was already sent.**
  `SessionScreen.kt`'s immediate-success branch returns before
  `pendingClipboardResend?.cancel()`, so a retry parked for older text `A` can
  still land after newer text `B` went out — a stale revert on the guest side.
  That early return is byte-identical before and after the fix wave, so this
  predates it. Worth noting that the new shape is *more* recoverable than the
  old: ending with `lastSent = A` while the phone's clipboard holds `B` means
  the next resume resends `B`, where the old decision-time write left
  `lastSent = B` and suppressed exactly that correction.

- **`MAX_INPUT_MESSAGE_BYTES` is a hand-maintained cross-language mirror.**
  `ClipboardBridge.kt` mirrors `crates/navette-protocol/src/media.rs`'s
  `MAX_INPUT_MESSAGE` (16 KiB) with no test binding the two, so they can drift
  silently. This is the shape the size fix asked for — the client must know the
  daemon's graceful limit to stay under it — but a drift guard would be a
  separate change. If the Rust constant moves, this one must move with it.

- **The Rust `MAX_CLIPBOARD_BYTES` (1 MiB) validate path stays unreachable over
  this transport.** The WebSocket frame cap rejects first, so that arm never
  runs. Left in place deliberately: it is correct, cheap, and would become live
  again if the transport limit were ever raised. The behaviour that actually
  governs is documented under check 5 above — refusal between 16 and 32 KiB,
  teardown above 32 KiB, and a client-side guard that now keeps sends at or
  under 16 KiB so neither is reached.

## CRITICAL — any web page can drive the daemon, in the default configuration (2026-09-12)

**FIXED (commit `f82c7e9`, `feat/hardening`).** `navetted` now refuses any request
carrying an `Origin` header with 403, on every route including `/healthz`, closing
the hole described below. Independently E2E-verified against a live debug daemon:
`/healthz` and `/v1/sessions/{s}/media` both return 403 with an `Origin` header
present (empty `Origin` also 403) and behave normally without one. The reasoning
below is left in place — it's still why the fix takes the shape it does, and it's
the source of the "no loopback exemption" rule the token in the entries below
inherits.

Found while designing API-wide auth. **This is live in shipped code on master and
needs no flags to reach** — it is the documented default, not a misconfiguration.

`grep -rn "origin\|Origin\|cors\|Host"` across `crates/navetted/src/` returns
nothing. The router (`api.rs:77-80`) carries no middleware of any kind.
`media_websocket` (`api.rs:84`) validates a session name and nothing else;
`websocket` (`api.rs:252`) requires a WebSocket subprotocol, which is **not** a
defense because a browser sets one with `new WebSocket(url, [...])`.

Browsers do not apply CORS preflight to WebSocket handshakes — they open the
connection and leave rejection to the server. So while `navetted` runs on loopback,
any page the user visits can attach to `/v1/sessions/{s}/media`, receive the screen,
and inject input. Session names are user-chosen and guessable, and the control
socket enumerates sessions anyway. The blob routes in
`specs/2026-09-11-bulk-transport-design.md` would inherit this: a cross-origin POST
with a simple content type fires without preflight.

**Fix (item 0 of the hardening branch):** reject any request carrying an `Origin`
header outright — navette has no browser client, so a blanket rejection is correct
rather than a policy to tune — plus `Host` validation against the expected authority
to blunt DNS rebinding. Router-wide middleware, small, and independent of every auth
decision.

**Design consequence, recorded because it nearly shipped:** an "unauthenticated
loopback, token for remote" split was about to be proposed on the reasoning that a
loopback TCP port is equivalent to a 0700 Unix socket. It is not. A 0700 socket is
unreachable from a web page; a loopback port is not. **The token applies to every
route with no loopback exemption.**

## Hardening branch — shape agreed 2026-09-12

Decided to fold all three items into one reviewed branch rather than hotfix item 0
separately: one coherent security story, one review pass.

0. `Origin` rejection + `Host` validation (above).
1. API-wide bearer token, no loopback exemption. Its own 0600 file — **not**
   `registry.json`, which sets no explicit mode and so lands at umask default
   (typically 0644).
2. `uncompressed_size` ceilings, below — two call sites, two values.

**Update (2026-09-12): all three items are designed, implemented, and reviewed.**
Items 0 and 1 are landed on `feat/hardening`; item 2 is implemented and reviewed but
not yet buildable here — see "wprs allocation ceilings are implemented but
unpushed" further down for why. One deviation from this plan worth flagging: item 0
as actually built does **not** include `Host` validation. That was reversed during
design, not dropped by accident —
`docs/superpowers/specs/2026-09-12-hardening-design.md` §2 records why: a browser
always sends `Origin` on a WebSocket handshake, so the `Origin` rule alone already
closes DNS rebinding, and a `Host` allowlist would additionally break the ordinary
case of a client dialling a tailnet name the daemon has no way to recognise.

Scope note found during orientation and accepted: the Android app persists
**nothing** (no DataStore, no SharedPreferences anywhere under
`android/app/src/main/kotlin/`), and `ConnectScreen.kt:24` explicitly defers a
saved-host registry to M4. A token therefore means retyping it every launch unless
this branch adds a minimal single-host credential store. Agreed approach: add the
minimal store here, let M4 generalize it.

When auth lands, `specs/2026-09-11-bulk-transport-design.md` §7 goes stale — it
says API-wide auth is "tracked separately in docs/HANDOFF.md". Update it then.

## Two items surfaced while designing bulk transport (2026-09-11)

Both were found designing `docs/superpowers/specs/2026-09-11-bulk-transport-design.md`.
Neither is caused by that design, and neither is fixed by it — recorded here so the
spec does not have to pretend otherwise.

- **HIGH — a wire-declared size drives an unbounded allocation on every object
  message.** **RESOLVED IN THE WPRS FORK, NOT YET LANDED HERE (2026-09-12)** — the
  fix is implemented, tested, and reviewed, but exists only as unpushed local
  commits; this workspace still builds against the un-ceilinged pin today. See
  "wprs allocation ceilings are implemented but unpushed" further down for the full
  status. The finding below is unchanged and still the reason the fix takes the
  shape it does.

  `streaming_framed_decompress_with` reads `uncompressed_size` with
  `usize::framed_read` (`wprs/src/serialization/framing.rs:67-75`, which is a
  **u32** on the wire), and passes it to `decompress_impl`, which resizes its
  buffer to that value (`wprs/src/sharding_compression.rs:434-439`). Ceiling is
  4 GB, allocated before any content is validated. This governs **every**
  `MessageType::Object` navetted reads today — surface commits, input, all of it —
  not just clipboard data, and it predates all clipboard work. navetted links the
  bridge in-process, so an OOM here takes down every session, not one.

  Fix is surgical and upstreamable: bound `uncompressed_size` against a ceiling in
  our wprs pin. 256 MB leaves roughly 8x headroom over a 4K framebuffer (~33 MB
  uncompressed). It touches every message path, so it wants its own change and its
  own test, not a line item inside a feature spec.

  **Correction to an earlier claim:** commit `0a49236` and the first draft of the
  bulk-transport spec cited `Vec<u8>::framed_read`
  (`wprs/src/serialization/framing.rs:101-106`) as the unbounded path and scoped it
  to clipboard data transfers. Both were wrong — wrong function, and far too narrow
  a scope. The citation above is the verified one. Anyone chasing the old reference
  should stop and read this entry instead.

- **HIGH — the daemon API has no authentication at all, and blob endpoints do not
  change that either way.** **RESOLVED (2026-09-12, commit `f716def` and the
  clients that followed it — see "Hardening branch" entries above).** Every route
  now requires `Authorization: Bearer <token>`, no loopback exemption. The finding
  below is unchanged and is still the reasoning behind that shape; the remedy it
  calls for below is what landed.

  Any peer that can reach the API can attach to
  `/v1/sessions/{s}/media`, read the whole screen, and inject input. The tailnet is
  the only boundary; `main.rs:57-62` refuses a non-loopback bind without
  `--allow-remote`, and that is the entire defense.

  Consequence for design work: adding a token to any single route is theater while
  the media socket stays open. API-wide session authentication is the real remedy
  and needs its own spec. Until it exists, every new route should be justified by
  showing it does not widen the boundary, which is what the bulk-transport spec's
  §7 does, rather than by listing per-route mitigations.

## wprs allocation ceilings: landed (2026-09-13)

The fix for the "wire-declared size drives an unbounded allocation" HIGH item above
is now what `master` builds against. The fork branch
`bearyjd/wprs@navette/fix-sse2-alignment` carries `e5958ed` then `38c61fe` on top of
the previously pinned `5763d74`, and both `crates/navette-bridge/Cargo.toml` and
`crates/navetted/Cargo.toml` pin `38c61feb7b05ad196cab95f7c66c33dfa95c8eee`
(`Cargo.lock` resolves to that rev from the remote, not a local `[patch]`).

What the two wprs commits do, for the next person who touches the decompress path:

- `e5958ed` adds the ceilings the item above called for: **80 MB** for
  `MessageType::Object` (`streaming_framed_decompress_with`), **128 MB** for
  `MessageType::RawBuffer` (`streaming_framed_decompress_to_owned`) — 4 tests
  covering reject-over-ceiling and accept-at-exactly-ceiling at both call sites, the
  accept tests asserting full payload length rather than mere absence of error.
- Review (`review10-wprs`) found that patch closed only one of three doors on the
  same wire path: `AlignedVec::framed_read` (the indices blob,
  `framing.rs:116-122`) and `Vec<u8>::framed_read` (per shard, `framing.rs:101-106`)
  are both *also* wire-length-driven allocations, one read before the new check and
  one read after it and independent of `uncompressed_size` — a peer could declare
  `uncompressed_size=1` to clear the message-level check, then send a shard claiming
  4 GB. `38c61fe` closes both: a 1 MB indices bound (`MAX_INDICES_BLOB`,
  ~131K entries against a fixed small shard count) and a per-shard bound tied to the
  same message-kind ceiling, both placed at the `sharding_compression.rs` call
  sites rather than in generic `framing.rs`. The fix round's own test drives the
  exact attack — `uncompressed_size=1` plus an oversized shard — through the public
  `streaming_framed_decompress_to_owned` API and confirms it's now refused.
  Re-review confirmed both new bounds precede their allocations, both call sites are
  patched, `read_bounded_shard` mirrors `CompressedShard::framed_read`'s field order
  exactly (no wire desync), and the accept side is exercised at the boundary by the
  existing encode-path tests.
- A live loopback E2E (real `navetted`, `foot` as a session, `navette-viewer`
  attached over the real media WS, VAAPI encoder + viewer decoder matching at
  696×496) confirmed the `RawBuffer`/`Object` decompress path still works end to end
  under the new ceilings.
- Also recorded here since it belongs beside this entry, not buried in a spec: the
  design's original retention estimate for the `RawBuffer` ceiling was wrong. See
  "Known limitation: retained decompression buffer is ~256 MB, not 128 MB" below.

Two things are still true after the bump. The `wprsd` binary a running `navetted`
spawns is whatever `--wprsd` points at; a `wprsd` built before `38c61fe` is
un-ceilinged on its own receive side, so rebuild `../wprs` when you rebuild
`navetted`. And the earlier sections in this file that cite `5763d74` describe the
sessions they date from; they are history, not the current pin.

## `--runtime-dir` did not reach the spawned wprsd: fixed (2026-09-13)

Found while running the loopback E2E for the pin bump, cost three failed
attempts. `navetted --runtime-dir X` changes where the supervisor *waits* for a
session's Wayland socket (`supervisor.rs:271`, `resources.wayland_socket` is
built from the override) but the spawned `wprsd` still inherits the process
environment's `XDG_RUNTIME_DIR`, so smithay creates `navette-<name>` under the
real runtime dir and `wait_for_ready` times out with "wprsd did not create
session sockets before timeout". The `wprs.sock` path *does* honor the
override, which is why the flag looks half-working: one of the two sockets
lands where expected.

Only bit when the flag's value differed from the environment, which is
exactly the isolated-daemon case. Fixed the same day: the supervisor now puts
`XDG_RUNTIME_DIR=<runtime dir>` in the env of **both** children (wprsd needs
it to place the socket, the app needs it to find `WAYLAND_DISPLAY`), pinned by
`spawned_processes_receive_the_supervisor_runtime_dir`. Verified by rerunning
the exact invocation that timed out: session came up, `navette-<name>` landed
under the override, nothing leaked into the real runtime dir, viewer at ~82
FPS. Two more harness notes from the same runs: the scratchpad path is too long for a Unix socket (`SUN_LEN`, 108
bytes), so use a short dir under `/run/user/<uid>`; and `wprsd` execs
`xwayland-xdg-shell` from `PATH` by default, so prepend `../wprs/target/release`
or use a `wprsd.ron` with `enable_xwayland: false`.

## On-device pairing verification: not done (2026-09-12)

Everything up through Android unit tests, `assembleDebug`, and `adb install` on the
Pixel 10 Pro Fold (`57211FDCG0023C`) is done and reviewed (Task 9, commits
`7de3234`..`30bf424`). The actual scan-a-QR-and-connect, rotate-and-get-rejected,
re-pair, and media-reconnect checks were **not run** — they need a human driving the
phone and a decision to expose `navetted` on the tailnet (`--allow-remote`), both
outside agent authorization. Copied here verbatim from
`.superpowers/sdd/2026-09-12-hardening/task-9-report.md` before that scratch
directory is deleted, since this is the branch's single most important behavioural
property (no invisible reconnect loop on a rotated token) and it is currently
verified by unit tests and code review only, not on hardware.

**Before anything else: restart `navetted` from current HEAD.** A daemon built
before this branch's fixes has no bearer-token auth at all, so scenario 1 would
"work" for a reason that says nothing about this change, and scenario 2's
rotation/rejection check needs the Task 2/3 guard code live.

**Exposing that daemon so the phone can reach it over the tailnet is the user's
decision** (no `--allow-remote`, nothing bound to a non-loopback address, was done
by the implementer) — the whole checklist is blocked on that first.

Once a fresh `navetted` is reachable from the phone's tailnet:

1. **Fresh pairing via QR.** On the daemon host, run
   `navette token --qr --advertise-host <host>` (the host the phone can reach it
   at). On the phone, open Navette (fresh install or after clearing app data so no
   pairing is stored), tap "Scan pairing code", point the camera at the terminal's
   QR code. Expect: the scan UI closes on its own, a brief "Connecting..." spinner,
   then the drawer (app/session list). If Play Services is unavailable, fall back to
   "Enter manually": type the host and the token printed alongside the QR, tap
   "Pair" — same expected result.
2. **Rotated token is rejected, no spinner loop.** With the app still connected from
   step 1, on the daemon host run `navette token --rotate`, then restart
   `navetted`. On the phone, force-close and reopen the app (this exercises the
   init-block auto-reconnect using the *old* stored pairing). Expect: a brief
   "Connecting...", then "Pairing rejected. Scan a new code or enter one manually."
   — and it stays there. Watch 15-20 seconds: no repeating spinner, no
   Connecting/Failed flicker, no crash.
3. **Re-pairing after rejection.** From "Pairing rejected", run
   `navette token --qr --advertise-host <host>` again (prints the new, post-rotation
   token) and scan, or use "Enter manually" with the new token. Expect: normal
   connection, landing on the drawer, same as step 1.
4. **Media-channel reconnect during an active session is not a pairing failure.**
   From the drawer, attach to (or run) a session so the session screen is live.
   Disable Wi-Fi on the phone for a few seconds, then re-enable it. Expect: the
   ordinary "Reconnecting..." UI and a resumed picture — it must **not** show
   "pairing rejected" or drop back to the ConnectScreen. This exercises
   `MediaClient`/`ReconnectPolicy`, unchanged by this task, but is the regression
   this task must not introduce now that `SessionController` carries a real token
   instead of a placeholder that always 401'd.

Report back which of the four passed, and for any that didn't, what was on screen —
a screenshot is the fastest way to communicate that.

## Project hazard: `ConnectionState` has no compiler-enforced exhaustiveness (2026-09-12)

Found during Task 7 (Android 401 handling), and the plan's own prediction about it
was wrong, in the dangerous direction — worth recording as a standing hazard rather
than only a footnote on a fixed bug.

`ConnectionState` is a sealed interface, which normally means the Kotlin compiler
forces every `when` over it to handle every variant or fail to compile. It doesn't
here: every consumer (`NavetteApp.kt`, `SessionOverlay.kt`) uses a **subject-less**
`when { }` with an existing `else` branch, not `when (state) { }`. A subject-less
`when` is just a chain of boolean conditions to the compiler — sealed-class
exhaustiveness checking never applies to it. Adding `ConnectionState.Unauthorized` in
Task 7 produced **zero compile errors** at either site; had the implementer trusted
"the compiler will catch any place this needs handling" and skipped adding explicit
arms, `Unauthorized` would have silently fallen into the generic "connection lost"
branch, and the terminal-401 behaviour the hardening design's §7 calls for would not
exist anywhere a user could see it.

This is a standing hazard, not specific to `Unauthorized` or to this branch: **any**
future `ConnectionState` variant is silently unhandled at both sites unless someone
remembers to update the `else` chains by hand. Nothing enforces that they will.

Not fixed here — fixing it means switching both sites to `when (state) { }`, a real
(if small) behavior-preserving refactor outside this branch's scope. Worth doing
before the next `ConnectionState` variant is added, precisely because the compiler
will not remind anyone to do it then either.

## Known limitation: retained decompression buffer is ~256 MB, not 128 MB (2026-09-12)

`docs/superpowers/specs/2026-09-12-hardening-design.md` §6 originally claimed
worst-case per-connection retention after the wprs ceilings was "the larger
ceiling, 128 MB" — wrong, in the optimistic direction, and corrected at source
during Task 10's review.

`ShardingDecompressor` holds one buffer for the connection's whole lifetime and only
ever grows it. `decompress_to_owned` (the `RawBuffer`/framebuffer path, 128 MB
ceiling) uses `mem::replace`, which briefly holds **both** the new buffer and the
old one, and returns a `Vec` that is `truncate`d rather than shrunk — so the
returned `Vec`'s *capacity* stays at the declared length even though its *length*
drops. Worst case after one maximum-size `RawBuffer` message, retention is closer to
**256 MB per connection**, not 128 MB.

Still bounded, still four orders of magnitude better than the pre-fix 4 GB ceiling,
and the spec has been corrected to say so. **Not fixed** — reducing it would mean
changing the allocation strategy (an actual shrink path, or a fresh allocation
instead of `mem::replace` truncation), a larger change than this branch's scope.
Recorded here as the number to plan memory budgets against, not 128 MB.

## Deferred minor: `Bearer` prefix match is case-sensitive (2026-09-12)

`crates/navetted`'s auth middleware checks the `Authorization` header with
`strip_prefix("Bearer ")`, which is case-sensitive. RFC 7235 treats the
auth-scheme token as case-insensitive, so a client sending `bearer <token>` is
refused (401) rather than accepted. Found and deferred during Task 3's review.

Fails closed, and every client this project controls (CLI, viewer, Android) sends
`Bearer` with the RFC-conventional capitalization, so this has no live impact today.
Worth a one-line case-insensitive match if a third-party client is ever added.

## Deferred minor: no end-to-end test of the Android reconnect wiring (2026-09-12)

`SessionScreen.kt`'s composed retry effect — `isDropped` feeding `first{}` feeding
`shouldRetry` — has each link unit-tested in isolation, but nothing exercises the
composition end to end. Reverting how those three are wired together (not their
individual logic) would go uncaught by the existing suite. Found during Task 7's
review; matches this codebase's existing style (no Compose/instrumented tests
anywhere), and the on-device checklist above (scenario 2) covers it behaviourally
once someone runs it — but that's a manual check, not a suite that fails a future
regression automatically.

## Pattern worth naming: a guarded secret with an unguarded copy elsewhere (2026-09-12)

Recurred three times on this branch, and once on the branch before it — enough to
be a pattern rather than a coincidence, and worth a standing check rather than an
anecdote.

1. **`AuthToken` vs raw `String` copies (Task 6).** `AuthToken` got a hand-written
   `Debug` that redacts the value specifically so it can't reach a log by accident —
   but the CLI's own argument structs held the same secret as a raw `String`, which
   `Debug`-derives normally, undoing the guard the moment anyone added a debug
   trace. Fixed with a `SecretString` newtype that carries the same redacting
   `Debug`.
2. **Encrypted store vs `rememberSaveable` instance state (Task 9).** The Android
   pairing token has an `EncryptedSharedPreferences`-backed store built specifically
   to hold it — but the manual-entry screen's typed token used `rememberSaveable`,
   which is an OS-held `Bundle` outside that store, survives process death, and can
   reach disk. Fixed by dropping it to plain `remember`.
3. **`lastSent` cleared, `lastRemote` not (the clipboard branch, prior to this
   one).** Same shape: a guard applied to one of a pair and not its sibling.

**The check to run, not just the anecdote to remember:** when you add a guard around
a secret (a redacting `Debug`, an encrypted store, a clearing-on-use rule), grep for
every *other* place the same value is held or constructed, and confirm each one is
covered too. The bug is never in the guarded copy — it's the copy nobody thought to
check.

## Log gap: PRs #23–#32 (2026-09-14 → 2026-09-16)

This file was not updated for the Codex-driven sessions between the hardening
branch and file transfer. The merged work in that window, for orientation only
(each PR body is the record; nothing here was re-derived from the code):

- #23 fix(android): stop HUD work after media disconnect
- #24 feat(android): redesign remote workbench launcher
- #25 refactor(android): extract session controller
- #28 fix(android): supersede stale clipboard retries
- #29 test(android): cover reconnect wiring
- #30 feat(android): scroll guest at zoom pan edges
- #31 feat(android): add multi-host pairing registry (Phase 2 "multi-host registry")
- #32 feat: add session image clipboard transport (Phase 2 "image clipboard";
  established the per-session bulk blob transport that #33 builds on)

## File transfer: landed, four review rounds deep (2026-09-19)

PR #33 (`b56f33b` feature + `2bfd942` hardening, merged as `f343aa2`) closes the
Phase 2 "file transfer" item: a phone or the CLI delivers a file into a running guest
session, bounded per session and per file. This entry records the shape, the
defects the reviews found (so nobody reintroduces them), and what was deliberately
left for later.

### Shape

- **Daemon** (`crates/navetted/src/file_transfers.rs`): `FileTransferStore`, one
  drop root under `XDG_DATA_HOME/navette/drops`. Guest-visible `root/<session>` is
  exported as `NAVETTE_DROP_DIR`; daemon-private staging is `root/.staging/<session>/<id>.part`.
  Lifecycle `awaiting_upload → queued → materializing → delivered`, terminals
  `failed`/`cancelled`. Limits: 64 MiB/file, 256 MiB and 64 objects per session,
  15 min preflight expiry, 64 retained terminal records. A per-session **epoch
  lease** makes a same-name replacement session invisible to the old incarnation's
  uploads, status reads, and materialization workers.
- **API** (`crates/navetted/src/api.rs`): `POST /v1/sessions/{s}/files` (preflight →
  id + upload URL), `PUT …/{id}/content` (exact `Content-Length`, existing upload
  slots and timeouts), `GET`/`DELETE …/{id}`. Auth/origin middleware applies. The
  PUT's 202 schedules materialization on a `BridgeManager` thread, off the render loop.
- **Supervisor** (`crates/navetted/src/supervisor.rs`): resets `root/<session>`
  *before* spawning the guest, then the API calls `FileTransferStore::activate_prepared`,
  which only records the new epoch. The old `activate` (reset after spawn) is
  `#[cfg(test)]` — see "Defects" for why it must stay that way.
- **Protocol/CLI**: `crates/navette-protocol/src/media.rs` separates generic safe-MIME
  blob descriptors from the image-only clipboard ones. `navette cp SESSION SOURCE
  [--name] [--no-wait]` streams, polls, cleans up on Ctrl-C, and refuses any
  server-provided upload URL that changes authority.
- **Android** (`ui/session/FileTransferCoordinator.kt`, 759 lines): document picker →
  re-openable `ContentResolver` source (bytes are never buffered) → streaming
  `HttpURLConnection` PUT → polling coordinator. UI states include a new
  `Unconfirmed(name, message)` — shown when the daemon's state could not be
  confirmed; it offers no Retry.

### How it got here

The feature commit came out of Codex. Codex's own review found four lifecycle
defects, an executor patched them, a second review found three more, and then the
Codex runner died (exit 139 / `ETXTBSY` on `/bin/echo`) with the patch uncommitted.
The work moved to Claude Code, which fixed the three, ran two more independent
review rounds (code + security) that found a further HIGH and several MEDIUMs, and
fixed those too. Final verdicts: code-reviewer APPROVE (round 4), security-reviewer
APPROVE. Every Android guard added in the hardening commit was mutation-checked
against its own test (remove the guard → exactly that test fails).

### Defects found and closed — the reasons behind non-obvious code

1. **Guest spawned before its drop directory was reset.** `activate` removed and
   recreated `root/<session>` after `Supervisor::start` had already exported the path
   to a running guest. A fast guest could see the previous incarnation's files or
   lose files it created in the gap. Fix: reset in the supervisor pre-spawn;
   `activate_prepared` never touches the directory. Test:
   `supervisor::tests::resets_drop_directory_before_spawning_the_guest`.
2. **Symlink escape via a guest-precreated `drop/<id>`.** `create_dir_all` followed a
   planted symlink out of the tree. Fix: `deliver_staged_file` opens the drop dir
   `O_DIRECTORY|O_NOFOLLOW`, `mkdirat`s the id (EEXIST fails the transfer), `openat`s
   it, and `renameat`s into it. The 32-hex id is unguessable, so this is
   defense-in-depth behind a random token.
3. **Path-based chmod of the staged file (TOCTOU).** `symlink_metadata` then
   `set_permissions(path)` — the latter follows symlinks. Fix: `openat(O_NOFOLLOW)` →
   `fstat` (regular, expected size) → `fchmod` on the same fd → `renameat`. The
   remaining open→rename window lets a guest sabotage only its own delivery. Test:
   `materialization_never_follows_a_swapped_staged_symlink`.
4. **Materialization worker spawn failure stranded quota.** Thread spawn error after
   a 202 left the entry `queued` forever with its reservation held. Fix:
   `fail_queued_materialization` (bridge.rs failure branch) → `failed`, staging
   removed, quota released.
5. **`reset_private_directory` used a recursive mkdir for the leaf**, which returns
   Ok over a planted symlink-to-dir. Fix: non-recursive leaf creation fails closed.
   Note: a symlink planted *before* start is simply unlinked by `remove_dir_all`, so
   the integration test can't discriminate this; the unit test on the helper does.
6. **Android treated DELETE→409 as "materializing".** The daemon returns 409 for three
   distinct states: PUT body still tearing down (`entry.uploading`), materializing/
   delivered, or already terminal. After Cancel the first is the common case (the
   client's socket disconnect races the DELETE), and the coordinator showed
   "Delivering…" for 30 s then "timed out". Fix: on 409 read `status()` and branch —
   AwaitingUpload/Queued → retry DELETE; Materializing/Delivered → resume polling;
   terminal → Cancelled. 404 → `Gone` → confirmed immediately.
7. **"Cancelled" rendered without server confirmation** when the network was down.
   Fix: after the DELETE budget, one status read decides; unknown → `Unconfirmed`.
8. **Duplicate delivery via Retry.** `fail()` released the reservation with
   `cancelRemote` and ignored the verdict; a transient poll failure on a file the
   daemon already held produced "Failed [Retry]", and Retry re-uploads under a new
   id. Fix: a transfer is `committed` at the 202; from then on every failure path
   follows the file to its end (`waitForDelivery`) or ends `Unconfirmed` — never
   Retry. Three consecutive dropped polls are tolerated before failing at all.
9. **Back-out-of-session leaked the reservation for 15 minutes.** `close()` ran in
   the Compose scope being disposed in the same pass; its `launch` died before the
   first dispatch. Fix: `close()`/replacement/`cancel()` release with
   `launch(UNDISPATCHED) { withContext(NonCancellable) { withTimeoutOrNull(5 s) … } }`
   around only the DELETE; the verdict handling stays cancellable.
   `RELEASE_TIMEOUT_MS` is a soft bound — it cannot interrupt a blocking connect.
10. **Resumed polling left the cancellation latch set and `work` pointing at a dead
    Job**, so a later replace/cancel never released anything and couldn't stop the
    poll. Fix: the resume path resets `remoteCancellationStarted` and re-points
    `work`. `settleCancellation` handles the one other latch producer (Cancel tapped
    while `fail()`'s own release loop is mid-retry) by reading status and spending a
    fresh DELETE budget if the entry is still cancellable.
11. Smaller: stale `onProgress` from a still-draining upload can't republish over a
    resumed delivery (guarded by the upload Job's liveness); a late verdict for a
    superseded attempt can't overwrite its replacement (`latest`); terminal publishes
    clear `active`.

### Known limitation — record it wherever this is next discussed

The guest is spawned as the daemon's uid with no sandbox (no bwrap/flatpak/unshare
anywhere in `crates/`). Items 2, 3 and 5 are therefore **defense-in-depth, not a
privilege boundary**: a guest that can write `root/.staging` — which the current
spawn permits — can still make the delivered inode differ from the validated one,
because `renameat` re-resolves `<id>.part` by name. The doc comment on
`deliver_staged_file` reads as though a boundary exists; it becomes one the moment
the guest is confined to `NAVETTE_DROP_DIR`. Do not remove the hardening because it
is "moot today" — it is the part that will matter.

### Deliberately deferred

- ~~`recover()` and `begin_upload()` still create/chmod by path~~ **done in
  PR #34 (2026-09-19)** — `open_private_directory` helper; `.staging` is now 0700;
  `recover` does its directory I/O before marking the session live and the API
  logs recovery failures instead of swallowing them.
- ~~No self-heal if the guest deletes its own drop directory~~ **done in PR #34** —
  `deliver_staged_file` recreates it (0700) on the next delivery.
- ~~Server side: let DELETE win over an in-flight PUT~~ **done in PR #34** —
  `cancel()` is authoritative while `uploading`; `FileTransferError::Cancelled`
  (409) surfaces on the PUT via `ensure_upload_open`/`complete_upload`/`finish`.
  The Android 409+`AwaitingUpload` retry branch is kept for older daemons but new
  daemons answer the first DELETE with 204. Accepted residual: at most one more
  chunk can land in the orphaned inode before the handler observes the cancel.
- Split `HttpFileTransferTransport` out of the 759-line coordinator.
- `ActiveTransfer.finished` is a structural guard with no test that detects its
  removal — defensive, not observed behaviour.
- ~~On-device verification of the Android flow has not been done.~~ **Done
  2026-09-19** on the Pixel 10 Pro Fold over the tailnet (`tower` 100.111.143.67,
  daemon built from `f343aa2`, isolated `XDG_DATA_HOME`/`--runtime-dir`): picker →
  2 MiB upload → "Delivered" in <1 s, SHA-256 match, file `0600` in a `0700`
  per-transfer dir, staging empty, visible under the guest's `NAVETTE_DROP_DIR`;
  Cancel tapped 0.6 s into a 60 MiB upload → "Cancelled" immediately and stable, no
  staging or quota left behind, next send succeeds; Kill+Run of the same name →
  empty drop dir at spawn, and an upload that straddled the respawn landed only in
  the new incarnation's dir; daemon restart mid-session preserved the delivered file
  (`recover`). Also `navette cp` 3 MiB → 0.26 s, hash match. Trap for the next run:
  the phone's home Wi-Fi ("crayon-mesh") isolates clients at L2, so a LAN bind is
  unreachable from the phone — use the tailnet; the app rejects loopback, so `adb
  reverse` is not an option either.

### Verification at merge

`cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`,
`cargo test --workspace` (315), Android `testDebugUnitTest` (315) + `assembleDebug` +
`lintDebug`, `git diff --check` — all clean locally; CI `rust`, `android`, and
`viewer-display` (the Xvfb gate that cannot run on this machine) all passed on the PR.
