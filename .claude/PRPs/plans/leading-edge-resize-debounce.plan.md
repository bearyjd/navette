# Plan: Leading-Edge Resize Debounce

## Summary
Right now a viewport resize only starts propagating to the server 100ms
*after the user stops dragging* (pure trailing-edge debounce), and the real
cost after that — Firefox's own relayout/repaint plus an encoder restart —
measured at ~770-830ms end to end on a real tailnet path. Since applying a
resize is itself cheap (two fire-and-forget sends), we can fire the *first*
resize in a drag immediately and keep debouncing the rest, so the expensive
repaint chain starts while the user is still dragging instead of after they
let go. This plan adds that leading edge, keeping the existing trailing edge
so the final settled size still always wins.

## User Story
As someone driving a Navette session through `navette-viewer`, I want the
remote window to start catching up to a resize as soon as I begin dragging,
so that by the time I let go of the window edge, the content is already
close to right instead of visibly lagging for another second.

## Problem → Solution
**Current**: `ViewportResize` commands only ever get applied to the wprs
transport 100ms after the *last* resize event in a burst — i.e., only once
the user has finished dragging. The ~700ms of real work after that
(Firefox relayout/repaint, encoder restart, re-encode, re-publish) all
happens *after* the user's hand is already off the mouse.
**Desired**: The first resize event in a burst applies immediately,
kicking off that same ~700ms chain while the user may still be actively
dragging (often itself several hundred ms), so a meaningful fraction of the
perceived lag is hidden behind the gesture instead of tacked onto the end
of it. The trailing-edge behavior (final value, 100ms after the burst
quiets) is unchanged, so correctness — the window always ends up at the
size the user actually released it at — is unaffected.

## Metadata
- **Complexity**: Small
- **Source PRD**: N/A
- **PRD Phase**: N/A
- **Estimated Files**: 1 source file (`crates/navetted/src/bridge.rs`), same file's test module

---

## UX Design

### Before
```
user drags edge ──┐
                   │ (100ms of silence needed before anything happens)
user releases ─────┤
                   │◄── 100ms trailing debounce ──►│
                                                     │◄── ~700ms: Firefox relayout+repaint,
                                                     │    encoder restart, re-encode/publish ──►│
                                                                                                  ▼
                                                                                     window catches up
Total felt lag after release: ~770-830ms (measured on a real tailnet path, see Notes)
```

### After
```
user starts dragging ──┐
                        ├──► first resize applied IMMEDIATELY
                        │    (Firefox relayout/repaint + encoder restart start NOW,
                        │     overlapping with the rest of the drag gesture)
user keeps dragging ────┤    ...intermediate sizes debounce as before...
user releases ──────────┤
                        │◄── 100ms trailing debounce ──►│
                                                          │◄── same ~700ms chain,
                                                          │    but for however much of it
                                                          │    didn't already overlap the drag ──►│
                                                                                                     ▼
                                                                                        window catches up sooner
Felt lag after release: reduced by however long the drag itself lasted (0 in the
worst case of an instantaneous single-step resize, up to the full ~700ms saved
for a drag that lasts that long)
```

### Interaction Changes
| Touchpoint | Before | After | Notes |
|---|---|---|---|
| Start of a drag-resize | Nothing happens server-side until the user stops | Server-side relayout chain starts immediately | The one behavior change this plan makes |
| Mid-drag | Every intermediate size debounced, none applied | Same — only the *first* event in a burst is exempted from debouncing | Prevents encoder-restart thrash during a fast drag |
| End of a drag | 100ms debounce, then the ~700ms chain, from a cold start | 100ms debounce, then the ~700ms chain, but the chain's *server-relay* leg may already be in flight/settled from the leading-edge apply | Best case: the whole thing already resolved before the trailing edge even fires |
| A single instantaneous resize (no real "drag", e.g. a scripted `ViewportResize`) | One apply, 100ms late | One apply, immediately (leading edge fires, trailing edge is a no-op re-apply of the same value) | Strictly faster, never slower |

---

## Mandatory Reading

| Priority | File | Lines | Why |
|---|---|---|---|
| P0 | `crates/navetted/src/bridge.rs` | 147-233 (`run_bridge`) | The exact debounce state machine this plan changes |
| P0 | `crates/navetted/src/bridge.rs` | 351-356 (`force_keyframe_on_all_streams`) | Already-established helper this plan's leading-edge apply reuses |
| P1 | `crates/navette-bridge/src/input.rs` | 182-201 (`InputState::apply`'s `ViewportResize` arm) | What "applying" a resize actually costs — two fire-and-forget `transport.send`/`update_output` calls, confirms the leading-edge apply is cheap |
| P1 | `crates/navette-bridge/src/encoder.rs` | 57-60, 213-221 (`FfmpegEncoder::reconfigure`, `restart`) | `reconfigure` is a full process restart (kill + respawn ffmpeg), not a cheap in-place resize — this is the real risk of firing resize eagerly: a leading-edge size that Firefox actually repaints to before the trailing edge fires costs a second restart |
| P2 | `crates/navetted/src/bridge.rs` | 592-660ish (`BridgeManager` tests) | Existing pattern for testing this module: spin up a fake wprs socket pair and assert on what gets sent, rather than mocking `WprsTransport` |

## External Documentation
No external research needed — this is a debounce/throttle pattern applied to
existing internal machinery, not a new library or protocol.

---

## Patterns to Mirror

### DEBOUNCE_STATE
// SOURCE: crates/navetted/src/bridge.rs:169, 214-231
```rust
let mut resize: Option<(Instant, u32, u32)> = None;
// ...
if let Some((requested, width, height)) = resize
    && requested.elapsed() >= RESIZE_DEBOUNCE
{
    worker
        .input
        .apply(0, MediaInput::ViewportResize { width, height }, &worker.scene, &transport)
        .ok();
    force_keyframe_on_all_streams(&mut worker.streams);
    resize = None;
}
```
This is the trailing edge to keep unchanged. The leading edge is a new branch
at the point `resize` gets *set* (currently line 187), not where it gets consumed.

### PURE_HELPER_EXTRACTION (mirrors this session's `navette-viewer` fix)
// SOURCE: crates/navette-viewer/src/native.rs (this session's `rescale_to_content`)
The equivalent codebase convention for "logic embedded in an untestable
event loop": pull the *decision* (should this resize apply now, given its
own prior state?) into a small pure function/struct that takes-and-returns
plain values, and unit-test that directly. `run_bridge` itself stays
integration-tested (it already is, via `BridgeManager` fakes), but the
debounce *decision* doesn't have to be.

### DOC_COMMENT_STYLE
// SOURCE: crates/navette-viewer/src/native.rs (this session's `content_size` field doc)
This codebase's comments consistently explain *why*, cite the concrete
measured numbers or observed behavior that motivated the code, and reference
the specific upstream/library behavior being worked around — not just what
the code does. Follow that for the new debounce logic's doc comment.

---

## Files to Change

| File | Action | Justification |
|---|---|---|
| `crates/navetted/src/bridge.rs` | UPDATE | Add leading-edge apply; extract the debounce decision into a small testable unit |

## NOT Building
- **Client-side visual treatment during the gap** (letterboxing instead of
  distorted stretch, a "resizing…" overlay, cross-fade on the new frame).
  This is a real, separate UX lever — it reduces how *broken* the
  transition looks regardless of raw latency — but it's a `navette-viewer`
  change with its own design questions (does a throwaway validation client
  deserve this polish, or does it belong in a future real client?). Keep
  this plan scoped to the one clear, low-risk, high-leverage server-side
  win; revisit the visual treatment separately if the numbers below show
  the gap is still felt after this change.
- **Encoder-level incremental resize** (VA-API dynamic resolution change
  instead of a full ffmpeg process restart on every reconfigure). Real
  potential win given `restart()` is a full process respawn, but it's a
  bigger, separate change to `FfmpegEncoder` with its own risk profile
  (VA-API surface pool invalidation, etc.). Out of scope here; noted as a
  risk of *this* plan below since firing resize eagerly can cause one
  extra restart in the case described in the Risks table.
- **Changing `RESIZE_DEBOUNCE` itself** (currently 100ms). It's a small
  fraction of the measured ~800ms and isn't the lever this plan pulls.

---

## Step-by-Step Tasks

### Task 1: Extract the debounce decision into a pure, testable unit
- **ACTION**: Add a small struct (e.g. `ResizeDebounce`) that owns exactly
  the state the current `resize: Option<(Instant, u32, u32)>` local holds,
  with two methods: `observe(&mut self, now: Instant, width: u32, height:
  u32) -> Option<(u32, u32)>` (called on every incoming `ViewportResize`;
  returns `Some(size)` when this is the *first* event since the last
  settle — the leading edge — else `None`) and `poll(&mut self, now:
  Instant) -> Option<(u32, u32)>` (called every loop iteration; returns
  `Some(size)` and clears state once `RESIZE_DEBOUNCE` has elapsed since
  the last `observe` — the existing trailing edge, unchanged in behavior).
- **IMPLEMENT**: Internally this is still just `Option<(Instant, u32,
  u32)>` plus the one-bit "was this the first observe since the last
  settle" question, which falls straight out of "was the option `None`
  before this call."
- **MIRROR**: `PURE_HELPER_EXTRACTION` above — same shape as
  `rescale_to_content`: a plain function/struct taking/returning values,
  no `Instant::now()` hidden inside it that a test can't control (accept
  `now` as a parameter, exactly like the existing code already threads
  `Instant::now()` in from the caller at line 187).
- **IMPORTS**: `std::time::{Duration, Instant}` (already imported in this file)
- **GOTCHA**: Keep `RESIZE_DEBOUNCE` as a parameter or associated const on
  the struct, not hardcoded twice — the existing top-of-file constant
  (`crates/navetted/src/bridge.rs:22`) is the single source of truth.
- **VALIDATE**: `cargo test -p navetted resize_debounce` (new tests from Task 3) passes.

### Task 2: Wire the leading edge into `run_bridge`
- **ACTION**: Replace the current `resize = Some((Instant::now(), width,
  height));` assignment (bridge.rs:187) with a call to the new struct's
  `observe(...)`. When it returns `Some(size)`, immediately do exactly what
  the trailing-edge branch already does — `worker.input.apply(0,
  MediaInput::ViewportResize { width, height }, &worker.scene,
  &transport).ok();` followed by `force_keyframe_on_all_streams(&mut
  worker.streams);` — for that leading-edge size. Replace the existing
  trailing-edge `if let Some((requested, width, height)) = resize &&
  requested.elapsed() >= RESIZE_DEBOUNCE` block (bridge.rs:214-231) with a
  call to `poll(...)`, applying the same two calls when it returns `Some`.
- **IMPLEMENT**: The apply-and-force-keyframe sequence is identical in both
  branches — pull it into a tiny local closure or helper function inside
  `run_bridge` (e.g. `let apply_resize = |worker: &mut WorkerState, width,
  height| { ... };`) so Task 2 doesn't duplicate those two lines.
- **MIRROR**: `DEBOUNCE_STATE` above for the exact calls being replaced.
- **IMPORTS**: none new
- **GOTCHA**: The leading-edge apply and the trailing-edge apply must
  **both** still run even when they'd send the same size twice (e.g. a
  single non-dragged `ViewportResize` command with no follow-up) — that's
  already how it works today in spirit (one apply after one debounce
  period) and this plan should not skip the trailing edge just because a
  leading edge already fired, because the trailing edge is what guarantees
  the *final* size always gets committed even if it differs from the
  leading one.
- **VALIDATE**: `cargo build -p navetted` and re-run the manual repro: on
  the live desktop, `navette run <app>`, drag-resize the `navette-viewer`
  window, release, and confirm the window visibly starts adjusting sooner
  (subjective, but should be obviously different from the pre-fix feel
  already confirmed as "laggy" this session).

### Task 3: Unit-test the debounce state machine
- **ACTION**: Add a `#[cfg(test)] mod tests` (or extend the existing one in
  this file, alongside `BridgeManager`'s tests) covering: (a) a single
  `observe` returns the leading-edge size immediately; (b) a second
  `observe` within `RESIZE_DEBOUNCE` of the first returns `None` (no
  leading edge for it) and does not reset when the trailing edge will fire;
  (c) `poll` returns `None` before `RESIZE_DEBOUNCE` has elapsed since the
  last `observe`, and `Some(latest_size)` after; (d) after a `poll` fires
  (settles), a fresh `observe` produces a new leading edge again — the
  debounce doesn't stay "used up".
- **IMPLEMENT**: Table/case-style tests are fine given the small state
  space; follow this file's existing test naming style (descriptive
  snake_case sentences, e.g. `a_second_resize_within_the_debounce_window_is_not_a_leading_edge`).
- **MIRROR**: `TEST_STRUCTURE` — see the naming convention already used for
  `crates/navette-viewer/src/native.rs`'s new tests this session
  (`pointer_position_is_identity_when_window_matches_content`, etc.) and
  `crates/navette-bridge/src/scene.rs`'s existing test names — both favor
  a full sentence describing the scenario over `test_resize_1`-style names.
- **IMPORTS**: none new beyond what the file already has
- **GOTCHA**: Because `observe`/`poll` take `now: Instant` as a parameter
  (Task 1's design), these tests need no real sleeping — construct
  `Instant`s via `Instant::now() + Duration::from_millis(n)` arithmetic (or
  simplest: call the real `Instant::now()` once at test start and compute
  offsets from it) rather than `std::thread::sleep`, keeping the suite fast.
- **VALIDATE**: `cargo test -p navetted --lib bridge::` — all new tests
  pass, no `#[ignore]` needed (no real time or display involved).

### Task 4: Re-measure the real round trip and record the before/after
- **ACTION**: This session already built a repeatable, disposable
  measurement tool for exactly this number: a dependency-free WebSocket
  probe that sends `viewport_resize` and times the round trip to the
  resulting `stream_config` (see Notes — it lived in the session's
  scratchpad, not the repo, since it's a one-off measurement script rather
  than project code). Before merging, re-run the same style of probe
  against a live session with this change, and record the new number next
  to the pre-fix baseline (768-832ms, avg ~796ms, 7 samples) in the PR
  description or a follow-up to `docs/HANDOFF.md`.
- **IMPLEMENT**: N/A — this is a measurement task, not a code change.
- **MIRROR**: N/A
- **IMPORTS**: N/A
- **GOTCHA**: The original probe measured *scripted, instantaneous*
  resizes (no real drag gesture), which is exactly the case this plan's
  leading edge helps least (a single-step resize already gets its leading
  edge applied immediately, so the *total* wall-clock time to settle
  shouldn't change much for that case — what improves is a real drag,
  which a scripted probe can't easily simulate). Note that distinction
  when reporting the number: an unchanged scripted-probe number is
  *expected*, not a sign the fix didn't work. If a truly convincing
  before/after is wanted, it would need to simulate a drag (a burst of several
  `viewport_resize` sends a few ms apart, ending in a settle) rather than one
  instantaneous send.
- **VALIDATE**: A written-down number (or explicit "unchanged as expected
  for the scripted case, see GOTCHA") exists before this ships.

---

## Testing Strategy

### Unit Tests
| Test | Input | Expected Output | Edge Case? |
|---|---|---|---|
| Leading edge fires on first observe | `observe(t0, 1280, 720)` on fresh state | `Some((1280, 720))` | — |
| No leading edge mid-burst | `observe(t0, ...)` then `observe(t0+50ms, ...)` | Second call returns `None` | Debounce window |
| Trailing edge waits | `poll(t0+50ms)` after one `observe(t0, ...)` | `None` (< 100ms elapsed) | Boundary |
| Trailing edge fires | `poll(t0+100ms)` after one `observe(t0, ...)` | `Some(size)`, state clears | Boundary (>=, matching existing `elapsed() >= RESIZE_DEBOUNCE`) |
| Fresh leading edge after settle | `observe` again after a `poll` returned `Some` | `Some(new_size)` | Debounce doesn't stay "used up" |

### Edge Cases Checklist
- [x] Single instantaneous resize (no drag) — leading + trailing both fire, same size, no regression
- [x] Rapid multi-event burst — only one leading edge, one trailing edge
- [x] Two separate bursts back-to-back — each gets its own leading edge
- [ ] Concurrent access — N/A, `run_bridge` is single-threaded per session
- [ ] Network failure — unrelated to this change (unaffected error paths)

---

## Validation Commands

### Static Analysis
```bash
cargo clippy -p navetted --all-targets -- -D warnings
```
EXPECT: Zero warnings

### Unit Tests
```bash
cargo test -p navetted --lib
```
EXPECT: All tests pass, including the new debounce tests from Task 3

### Full Test Suite
```bash
cargo test --workspace && cargo fmt --all -- --check
```
EXPECT: No regressions (156 total tests going in — 149 currently + this
plan's ~5 new debounce tests — 1 ignored, matching the existing display-only
exception)

### Manual Validation
- [ ] Fresh `navetted` + `navette run <app>` + `navette-viewer <session>` on
      a real display (per this session's established repro)
- [ ] Drag-resize the window and confirm the content visibly starts
      catching up sooner than before this change, particularly for a
      slower/longer drag gesture
- [ ] Confirm a click made shortly after releasing a resize still lands
      correctly (this session's separate `native.rs` fix) — the two fixes
      are complementary, not redundant: that one fixes *where clicks land*
      during the gap, this one *shrinks* the gap
- [ ] Re-run the endurance-style resize loop (this session's
      `endurance.py`/`latency_client.py` pattern) for a few minutes and
      confirm no new errors, stream-ends, or encoder-restart storms appear

---

## Acceptance Criteria
- [ ] All tasks completed
- [ ] All validation commands pass
- [ ] Tests written and passing
- [ ] No clippy warnings
- [ ] No fmt diffs
- [ ] Matches UX design above (leading edge measurably starts the repaint
      chain before the user releases the drag)

## Completion Checklist
- [ ] Code follows discovered patterns (pure-function extraction, this
      file's existing test-naming style)
- [ ] Error handling matches codebase style (`.ok()` on the resize apply,
      matching the existing trailing-edge call exactly)
- [ ] Logging follows codebase conventions (no new logging needed — this
      is timing-only, not a new failure mode)
- [ ] Tests follow test patterns
- [ ] No hardcoded values (debounce duration stays the single existing
      `RESIZE_DEBOUNCE` constant)
- [ ] Documentation updated — `docs/HANDOFF.md` gets the before/after
      latency number from Task 4
- [ ] No unnecessary scope additions (client-side visual treatment and
      encoder-level incremental resize both stay out, per "NOT Building")
- [ ] Self-contained — no questions needed during implementation

## Risks
| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| A leading-edge size that Firefox actually repaints to before the trailing edge fires costs one extra encoder restart (`FfmpegEncoder::reconfigure` → full process respawn, `crates/navette-bridge/src/encoder.rs:213-221`) | Medium — depends on how long a typical drag lasts relative to Firefox's repaint cadence | Low-Medium — an extra process restart is bounded (tens of ms class, not the ~700ms class), and only happens for drags slow enough that Firefox keeps up with intermediate sizes | Accept for this plan (the whole point is to trade a small, bounded extra cost for hiding a much larger one); revisit only if Task 4's measurement shows it regressing common cases |
| Very rapid resize churn (e.g. a user aggressively wiggling the edge) could produce more leading edges than today if the debounce window keeps getting reset before it settles | Low — the leading edge only fires once per burst by construction (Task 1's design), not once per event | Low | Covered by Task 3's "no leading edge mid-burst" test |
| This plan's measurement task (Task 4) can't cleanly reproduce a real drag gesture with the existing scripted probe | High (it's a known limitation, not a surprise) | Low — doesn't block shipping, just limits how convincing the "after" number is | Documented explicitly in Task 4's GOTCHA; manual validation (dragging by hand) is the real acceptance signal for this plan, not the scripted number |

## Notes
This plan was scoped directly from a live debugging session on this same
branch: a human tester found that clicking right after a resize landed in
the wrong place (fixed separately, see `crates/navette-viewer/src/native.rs`'s
`content_size`/`rescale_to_content` change and the "also on a resize it is
pretty laggy" follow-up report that prompted this plan). The ~770-830ms
number cited throughout this plan (avg ~796ms, 7 samples, tight cluster) was
measured against a real second machine over Tailscale (a DigitalOcean droplet)
via a dependency-free WebSocket probe timing `viewport_resize` →
`stream_config` round trips — see `docs/HANDOFF.md`'s "Tailnet latency
measurement" section for the full methodology and numbers. That probe script
itself was disposable (scratchpad, not committed); Task 4 either reuses that
same approach ad hoc or, if this kind of measurement turns out to be needed
repeatedly, promoting it into a real `navette-viewer` dev-tool would be a
reasonable follow-up — but is explicitly out of scope for this plan.
