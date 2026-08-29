# PR Review: #7 — fix: root-cause and fix the resize + rapid-typing key repeat

**Reviewed**: 2026-08-28
**Author**: bearyjd
**Branch**: `fix/wayland-key-latch-under-load` → `master` (head `a3f90e7`)
**Decision**: **REQUEST CHANGES** — *gate addressed in `5e3a440`; see Resolution*

Reviewed in two independent passes: the authoring agent, and a separate
adversarial reviewer given no access to the author's reasoning. The HIGH
finding below came from the independent pass — the author missed it — and was
then confirmed against source. This is exactly why the passes are kept apart.

## Summary

The core change is correct, well-evidenced, and should land. The minifb
key-edge root cause is real, the two-phase fix is right (the PR even
demonstrates why the obvious one-line version is wrong), and the before/after
measurement is rigorous.

But the PR ships a **new instance of the very bug class it exists to fix**.
Its thesis is "a dropped release leaves the guest desynced with nothing to
correct it." That reasoning was applied to key and button releases, and not to
the `MediaInput` variants that carry *absolute state* — which are strictly
more latch-prone, because a lost absolute update is never superseded.

## Findings

### CRITICAL
None. No memory-safety, panic, or data-loss risk in the new code.
`rescale_to_content` is guarded against zero and uses `f64` throughout;
`release_all_held` has no borrow or panic risk; serial arithmetic is
`wrapping_add`. Nothing here regresses master — master has the latch this PR
removes.

### HIGH

**H1. Absolute-state inputs are excluded from redelivery, so dropping one
latches the guest permanently.**
`crates/navette-viewer/src/main.rs:135-141`

`must_redeliver` matches only `KeyboardKey{pressed:false}` and
`PointerButton{pressed:false}`. Its doc comment justifies this as "a dropped
press or motion is a missed input" — true for **edges**, false for **absolute
state**. Three variants carry absolute state, and two of them can latch:

| variant | kind | dropped ⇒ |
|---|---|---|
| `KeyboardKey` / `PointerButton` | edge | handled (retried) |
| `PointerMotion` | absolute, continuously re-sent | self-corrects |
| **`KeyboardModifiers`** | **absolute** | **latches (see below)** |
| **`ViewportResize`** | **absolute** | **latches** |

The mechanism is shared: each producer marks its state as sent at *emit* time,
then short-circuits on equality, so a send that fails afterwards is never
retried and never re-derived.

*Modifiers* — `native.rs:225-228`:
```rust
if modifiers == self.modifiers { return; }
self.modifiers = modifiers;                     // marked sent on PUSH
events.push(WindowEvent::Modifiers(modifiers));
```
*Viewport* — `session.rs:373-376`:
```rust
if self.last_sent == Some(pending.size) { return None; }
self.last_sent = Some(pending.size);            // marked sent on EMIT
```

Failure scenario (modifiers): the 64-slot input queue
(`client.rs:25`) saturates during a resize — which the measured poll cycles
make likely, since a 626ms cycle cannot drain. The user releases Shift.
`modifier_changes` computes `shift:false`, updates `self.modifiers`, and
pushes the event. The send returns `InputBackpressure`; `must_redeliver`
returns false; it is logged and dropped. The viewer now believes the guest
knows Shift is up. **`grep` confirms no resync path exists** anywhere — not on
focus change, reconnect, or stream reconfigure — so it will not re-emit until
the user happens to make another modifier transition by hand. Until then the
guest holds Shift and uppercases everything; with Ctrl, every keystroke
becomes a shortcut.

Failure scenario (viewport): the final `ViewportResize` of a drag is dropped.
The guest keeps the old viewport indefinitely. Because pointer coordinates are
clamped against frame dimensions, this reintroduces the pointer-misalignment
class this same PR fixes elsewhere. Lower probability — a drag emits many
resizes, so usually another follows — but the same shape.

**Suggested fix, and the obvious one is wrong.** Do *not* push these onto
`pending_redelivery`: a stale absolute state redelivered a tick later can
clobber a newer one. **Coalesce instead of queueing** — hold the latest known
value, and on a failed send retry *the current state* on the next tick, so a
newer value always supersedes an older one. Better still, have the producers
mark state as sent only on successful send, which removes the root cause
rather than compensating for it.

### MEDIUM

**M2. A press can overtake an older release of the same key; the new dedup
then converts a duplicate into a lost keystroke.**
`crates/navette-viewer/src/main.rs:79-102`

Reachable by construction: the viewer is `#[tokio::main]` multi-threaded and
the connection task drains `inputs` on another worker (`client.rs:72`), so a
permit can free between two `try_send` calls inside one tick.

1. `R1` (release K) fails with backpressure → pushed back (`main.rs:92`)
2. Later in the *same* batch `P2` (press K) is retried, a permit has freed, `P2` succeeds
3. Wire order becomes `P1, P2, … R1`
4. Bridge: `P2` is a redundant press → **silently dropped** by the new dedup (`input.rs:147-151, 173`)
5. `R1` then releases the key

The user typed two characters and got one. Not a latch, but the dedup turns
what used to be a harmless duplicate keydown into silent input loss.

Fix: on the first backpressure failure for a must-redeliver event, push the
failed event **and every remaining item in `due`**, so nothing overtakes a
queued earlier event.

**M3. `pending_redelivery` is unbounded and has no age limit.**
`crates/navette-viewer/src/main.rs:52`

A plain `VecDeque::new()` with no cap and no eviction. While the connection
stays saturated, every entry re-fails and is re-pushed, and new failures are
appended. Memory is not the real risk (`MediaInput` is small); staleness is —
a release queued seconds ago is still delivered verbatim. This also
contradicts the module's own stated design: `client.rs:34-36` documents a
bounded channel used specifically "instead of growing an unbounded backlog."

Fix: cap it, and drop or coalesce entries older than a short deadline.

**M4. `release_all_held` is skipped on the only exit path its own comment
describes — the fix does not fix.**
`crates/navetted/src/bridge.rs:174` (early return), `:241` (call site)

`event_loop.dispatch(...)?` sits inside the `while` loop, so an `Err`
propagates straight out of `run_bridge`, past the flush at line 241.

| exit path | reaches flush? | useful? |
|---|---|---|
| `stop == true` | yes | no — only from `RequestCommand::Kill`, which kills wprsd next line |
| `!transport.is_connected()` | yes | no — transport already gone, sends land nowhere (acknowledged in the comment) |
| `dispatch(...)?` returns `Err` | **no** | **this is the scenario the comment describes** — worker dies, navetted survives, a later `start` reaps the handle and can start a fresh worker against a live wprsd |

So the flush runs only where it is useless and is skipped where it would help.
Verified in source. Notably this is the *same shape* as minifb fork fix #2 in
this very PR — an early return skipping a call that must always run — applied
upstream and then reintroduced here.

Fix: `let result = (|| { ...loop... })(); worker.input.release_all_held(&transport); result`.

**M5. The dedup's invariant is per-attachment; the guest seat is global.**
`crates/navette-bridge/src/input.rs:147-151`

`pressed_keys` is keyed by `attachment_id`, but `KeyInner { serial, raw_code,
state }` carries no attachment identity — the guest sees one keyboard.
Attachment 10 presses keycode 30 (forwarded); attachment 20 presses keycode 30
(not redundant *for 20*, so also forwarded). The guest gets two keydowns with
no intervening release — precisely what the dedup's comment says must not
reach the wire. Attachment 10 then releases and the guest sees the key go up
while 20 still holds it. Multi-attachment is supported and tested
(`disconnect_releases_only_that_attachments_held_input` uses ids 10/20).

Fix: enforce on a seat-global set with per-attachment refcounts, or narrow the
comment to say the cross-attachment case is knowingly unhandled.

**M6. `KeyRepeat::No` removed the mechanism that made a dropped *press*
self-healing.**
`crates/navette-viewer/src/native.rs:331`

Under `KeyRepeat::Yes` a press lost to backpressure was followed by another
~250ms later and every ~50ms while held, so a held keystroke recovered on its
own. Under `No` the press is reported exactly once, `must_redeliver` returns
false for it, and it is gone permanently — while `held_keys` still contains
the key, so the eventual release reaches a bridge that never saw the press and
forwards a spurious Release.

The `KeyRepeat::No` change itself is correct and should stay. The finding is
that `must_redeliver`'s "a dropped press is a missed input" reasoning was
written against repeat behaviour **this same PR removed** — the clearest
single piece of evidence that the two halves were not reasoned about together.
(The new bridge dedup does *not* block this recovery, since the bridge never
saw the dropped press; `KeyRepeat::No` alone accounts for it.)

Worth recording in HANDOFF.md beside the existing "press and release inside a
single cycle are dropped" gap — same failure surface.

**M7. `main.rs` has zero tests.**

The new retry queue, `must_redeliver`, and `init_tracing` are entirely
uncovered. The branch adds 6 tests (4 for `rescale_to_content`, 1 dedup, 1
`release_all_held`) and those are good — real behaviour, meaningful
assertions — but none touch the logic most likely to be wrong, as H1, M2 and M4 all show. `must_redeliver` in particular is a pure function and trivially
testable; a test enumerating every `MediaInput` variant would have caught H1
directly.

**M8. The flush deliberately desyncs bridge from viewer, and degrades the
diagnostic this PR adds.**
`crates/navette-bridge/src/input.rs:239-265`

`release_all_held`/`disconnect` send Releases for keys the physical keyboard
may still hold and clear `pressed_keys`. `release_all_held` is explicitly about
a client that does *not* disconnect, so the viewer survives with its own
`held_keys` intact. Its eventual single Release then lands on the
untracked-release branch (`input.rs:157-172`) and forwards a second Release for
a key already up; a press arriving in that window is no longer redundant and
forwards fresh.

Nothing corrupts — a duplicate Release is inert to a Wayland client. The cost
is that the debug log added in *this same PR* (`input.rs:166-170`) now fires on
an expected path, so whoever reads it next will chase a non-bug. Add a line to
`release_all_held`'s comment saying the bridge's view is synthetic after a
flush and the next real Release will read as untracked.

### LOW

**L8.** `init_tracing` is duplicated verbatim in two binaries. Acceptable
(they are separate crates and the doc comment is worth having in both), but a
shared helper would prevent the two drifting.

**L9.** The manual `*serial = serial.wrapping_add(1).max(1)` in
`release_buttons`/`release_keys` duplicates `next_serial` (`input.rs:346-349`).
Verified *consistent and correct*; the duplication is forced by `mem::take`'s
borrow, so this is a note, not a defect. A `fn bump(serial: &mut u32)` shared
by both would remove the chance of them diverging.

**L10. `content_size` vs the server's clamp basis are different quantities.**
The independent pass filed this as "`content_size` is the *padded* frame size";
**that could not be confirmed** — there is no padding or even-alignment logic in
either `navette-bridge/src/encoder.rs` or `navette-viewer/src/decoder.rs`, and
the doc comment is accurate as written. The real point underneath it: the
viewer rescales pointer positions into decoded-frame space (`content_size`)
while the server clamps against `scene.surface_dimensions(key)`
(`input.rs:57-59`). Those are two different sources of truth assumed equal.
They agree in practice because Wayland surfaces are even-sized, but nothing
enforces it, and a divergence would reintroduce the misalignment class this PR
fixes. Worth a comment noting the coupling, not a code change.

**L11. The minifb pin comment is stale and omits this PR's own fix.**
`crates/navette-viewer/Cargo.toml:16-21` says the fork "fixes **two** real
Wayland keyboard bugs" and lists the shifted-keysym and present-failure fixes.
The pin is `0b54200`, which carries **three** — the missing one being the
key-edge two-phase fix that is the entire subject of this PR. Confirmed;
one-line fix.

## Validation Results

| Check | Result |
|---|---|
| Lint (`cargo clippy --all-targets -D warnings`) | **Pass** (CI + local) |
| Tests (`cargo test --workspace`) | **Pass** — 151 passed, 1 ignored |
| Build | **Pass** |
| CI `rust` | **Pass** (4m20s) |
| CI `viewer-display` (Xvfb, exercises repinned minifb) | **Pass** (2m41s) |

Not re-run locally during review: nothing changed since they passed.

## Files Reviewed

| File | Change |
|---|---|
| `crates/navette-viewer/src/main.rs` | Modified — retry queue, `must_redeliver`, `init_tracing` |
| `crates/navette-viewer/src/native.rs` | Modified — `content_size`/`rescale_to_content`, `KeyRepeat::No` |
| `crates/navette-bridge/src/input.rs` | Modified — press dedup, `release_all_held`, helpers |
| `crates/navetted/src/bridge.rs` | Modified — `release_all_held` on worker exit |
| `crates/navetted/src/main.rs` | Modified — `init_tracing` |
| `crates/navette-viewer/Cargo.toml` | Modified — minifb fork pin, `env-filter` |
| `crates/navette-bridge/Cargo.toml` | Modified — `tracing` dep |
| `crates/navetted/Cargo.toml` | Modified — `env-filter` |
| `Cargo.lock` | Modified |
| `docs/HANDOFF.md` | Added |
| `docs/ROADMAP.md` | Added |
| `docs/patches/minifb-two-phase-keyhandler.patch` | Added |

## Recommendation

**REQUEST CHANGES** — both passes independently reached this verdict.

The independent reviewer's proportionality note is worth preserving verbatim in
spirit: nothing here is a regression from master (master has the latch), the
two-phase `KeyHandler` diagnosis is correct, the 480-edge measurement is real
evidence, and the "moving `update()` wholesale is not a fix" note shows the
author tested the obvious wrong answer before committing. There is no CRITICAL.

The gate is four cheap items, none of which touches the thesis: **H1**
(coalesce modifier state), **M3** (cap/age-limit the queue), **M4** (hoist the
loop into a closure so `?` cannot skip the flush), and **L11** (the stale pin
comment). Accept as filed follow-ups, provided they are written down rather
than lost: M2, M5, M6, M8.

**Structural recommendation, worth more than any single fix:** move
`must_redeliver` and the pending queue out of `main.rs` into the library beside
`ViewerSession`. The retry queue is the only genuinely new *stateful* logic in
this PR, and it currently sits in a binary where H1, M2 and M3 are all
untestable. Relocated, all three become unit-testable against the existing
`RecordingWindow`, and the next person to touch backpressure inherits a
regression net instead of a comment. This also dissolves M7.

Original note, still true — H1 blocks on its own: It is a permanent guest-side latch, in a PR
whose entire purpose is removing a permanent guest-side latch, and it is
reachable through the same backpressure path the PR's own poll-cycle
measurement shows is more likely than assumed.

M2/M3 are worth fixing in the same pass since they share the queue, and M4 is
a one-line fix that should go with them. M7 (tests) should gate the work: whatever lands for H1 needs a test enumerating `MediaInput`
variants, or the next absolute-state variant added to the protocol will
reintroduce this silently.

None of this undermines the minifb work, which is the substance of the PR and
is sound.

---

## Resolution (`5e3a440`)

All four gate items fixed; CI green (`rust` 4m37s, `viewer-display` 2m54s).

| item | resolution |
|---|---|
| **H1** | `Recovery::Coalesce` for `KeyboardModifiers`/`ViewportResize` — newest value kept and re-sent, never queued, so a stale state cannot clobber a newer one |
| **M3** | queue capped at `MAX_PENDING_RELEASES` (128) and expired past `RELEASE_STALE_AFTER` (5s); both reported for logging |
| **M4** | `run_bridge`'s loop hoisted into a closure so `release_all_held` runs on every exit, including the dispatch-error path it was written for |
| **L11** | pin comment now lists all three fork fixes |

The policy moved from `main.rs` into `navette_viewer::relay`, per the
structural recommendation — it was the only new stateful logic in the branch
and it sat in a binary, which is why none of these had tests and why H1
shipped. It now has 10, including `every_variant_is_classified_deliberately`,
which forces a new absolute-state variant to be classified rather than falling
through to "drop" and latching the guest the way modifiers did.

**Coverage verified by mutation**, not just by passing: reverting the H1
classification fails 4 of the 10 new tests. Workspace 161 passed / 1 ignored
(up from 151); clippy and fmt clean.

Still open, deliberately out of scope for the gate: **M2**, **M5**, **M6**,
**M8**. M7 is dissolved by the relocation — the logic that had no tests now
has them, though `main.rs` itself remains test-free by design (it is now only
wiring).
