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
`Cargo.toml`) against this machine's live KDE Plasma Wayland session, and
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
per input and nothing else. `apply_us` is still ~10ms p50 and untouched, and
`compose_us` p50 is still ~21ms — one composite per iteration is bounded, but
not free. Neither is currently causing a symptom. The harness still aborts
roughly a third of runs with `no window`.

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
   product moment. Everything so far has been proving the plumbing works.

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
