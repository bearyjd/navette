# PR Review: #5 — M2 PR5: viewer, input, resize

**Reviewed**: 2026-08-25
**Author**: bearyjd
**Branch**: m2/bridge-runtime → master
**Decision**: APPROVE

## Summary

Completes M2's PR5: a new `navette-viewer` crate (protocol client, H.264
decoder, per-toplevel windows, scoped input, resize, HUD) plus the
bridge-side hardening and PR3 media-routing amendment it depends on. This
PR already went through an unusually thorough process before this review —
per-task implementer/reviewer/fix-loop cycles, a whole-branch review that
caught two integration-level bugs (input focus conflation, orphaned
decoder subprocesses / unbounded re-encoding), a fix wave for those, and a
devil's-advocate pass that found three more real bugs (a destroy-path
regression, stale input focus, a misleading timestamp fallback) which were
fixed and verified in the final commit (`c599854`) reviewed here. Nothing
outstanding blocks merge.

## Findings

### CRITICAL
None.

### HIGH
None. Two items are deliberately deferred, not missed — both are
documented in the PR body and the final commit message, with a stated fix
direction, and neither is a correctness defect in what's shipped:
- `Scene::parent_of`'s O(N) linear scan per ancestor-resolution hop (a
  cost introduced when re-encoding was narrowed to just the committed
  toplevel) — real, but bounded by realistic scene sizes; needs its own
  reviewed task since the fix (an incrementally-maintained parent
  back-pointer) is non-trivial given `children` is fully rebuilt on every
  commit, not incrementally mutated.
- CI wiring for the new `#[ignore]`d Xvfb smoke test in `native.rs` —
  correctly scoped out of a code PR (shared CI infra change).

### MEDIUM
1. **PR description is now stale** — the "Known follow-up (not blocking)"
   section describes the ghost-popup regression as still open; it was
   fixed in `c599854`, the commit this review covers. Should be updated
   before merge so the PR record matches what actually shipped (see
   Recommendation below).
2. `session.rs` at 1161 lines exceeds this repo's 800-line file-size
   convention — already identified and ruled on twice earlier in this
   branch's history (task review, whole-branch review): production code
   is ~380 lines with one clearly coupled responsibility; the excess is
   an inline test module that can't cleanly move to `tests/` because
   several tests reach private items. Not new, not blocking, still
   tracked.

### LOW
- `crates/navette-bridge/src/scene.rs`, `input.rs`, `crates/navette-viewer/src/router.rs`,
  `crates/navetted/src/bridge.rs` are all >800 lines in total but <540
  lines of production code — the same test-module-inflation pattern as
  `session.rs` above, not a fresh issue.
- ~30 further Minor findings from the per-task and whole-branch reviews
  (rate-limited logging, a `ffmpeg_available()` exit-code check, a couple
  of dead branches, doubled key auto-repeat, etc.) remain documented in
  this session's history but aren't re-litigated here — none change the
  merge decision.

## Validation Results

| Check | Result |
|---|---|
| Build (`cargo build --workspace --all-targets`, from clean) | Pass |
| Tests (`cargo test --workspace`) | Pass — 141/141 (1 correctly ignored: the Xvfb-gated smoke test, no display in this environment) |
| Lint (`cargo clippy --workspace --all-targets -- -D warnings`) | Pass |
| Format (`cargo fmt --all -- --check`) | Pass |
| CI (GitHub Actions, latest push) | Pass — Format, Lint, Test, Build all green |

## Fresh Review Scope

This branch's earlier commits were already reviewed exhaustively (per-task
reviews, a whole-branch review, fix-wave re-reviews). This pass focused
verification on what hadn't been reviewed by anyone else yet: the final
commit (`c599854`, 5 files, +400/-14), covering the devil's-advocate
findings fixed this session. Checked against all 7 categories
(correctness, type safety, pattern compliance, security, performance,
completeness, maintainability); a repo-wide sweep for TODO/FIXME,
`println!`/`dbg!`, and `unwrap()`/`expect()` outside test modules found
none introduced. Each of the three behavioral fixes in that commit was
independently verified discriminating (temporarily reverted, confirmed
its regression test fails, restored, confirmed green) before this review.

## Files Reviewed

All 27 files changed on this branch (`master...HEAD`), with focused fresh
review on the 5 touched in the final commit:

- `crates/navette-bridge/src/scene.rs` — Modified (this session: `remove_surface` fix + 2 tests)
- `crates/navette-bridge/src/input.rs` — Modified (this session: focus-reset methods + 2 tests)
- `crates/navetted/src/bridge.rs` — Modified (this session: call sites for focus reset)
- `crates/navette-viewer/src/router.rs` — Modified (this session: `last_timestamp_us` fix + 1 test)
- `crates/navette-viewer/src/native.rs` — Modified (this session: ignored Xvfb smoke test)
- `crates/navette-bridge/src/transport.rs`, `crates/navette-bridge/src/lib.rs`, `crates/navette-protocol/src/media.rs`, `crates/navette-viewer/{Cargo.toml,src/client.rs,src/decoder.rs,src/hud.rs,src/lib.rs,src/main.rs,src/overlay.rs,src/session.rs,src/window.rs,tests/media_endpoint.rs}`, `crates/navetted/{Cargo.toml,src/api.rs,src/lib.rs,src/main.rs,src/media.rs}`, `crates/navette-bridge/Cargo.toml`, `.gitignore`, `Cargo.toml`, `Cargo.lock` — Added/Modified earlier this session, already reviewed per-task and whole-branch
