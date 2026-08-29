# PR Review: #6 — perf(bridge): O(1) scene ancestor resolution + CI display coverage

**Reviewed**: 2026-08-26
**Author**: bearyjd
**Branch**: m2/scene-followups → master
**Decision**: APPROVE

## Summary

Closes the two items deliberately deferred at the end of PR #5's review:
an O(N)-scan-per-hop scene-graph ancestor lookup, replaced with an
incrementally-maintained `SurfaceNode::parent` back-pointer; and CI
wiring for the `#[ignore]`d display-dependent smoke test added in #5.
Small, focused diff (2 files, +265/-23). No CRITICAL, no HIGH.

## Findings

### CRITICAL
None.

### HIGH
None.

### MEDIUM
None.

### LOW
1. Whether the new `viewer-display` CI job is configured as a *required*
   branch-protection check (vs. just running and reporting status) is a
   repo-settings concern outside this PR's diff — worth confirming
   separately if the intent is for it to actually gate merges, not just
   run informationally.

## Correctness — where this review spent most of its attention

This diff replaces a lazily-derived value (recomputed fresh from a full
scan on every read) with an eagerly-maintained one (written at commit
time, read cheaply thereafter) in already-fragile scene-graph code. That
class of change is exactly where subtle bugs hide, so this got a line-by
-line adversarial pass rather than a skim:

- **Semantic equivalence with the old scan, checked explicitly for the
  orphaning case**: when a child is removed from its only parent's
  `children` list and not re-added elsewhere, the old O(N) scan would
  find no candidate on its next lazy lookup and return `None`. The new
  code's removal branch (`scene.rs` `sync_child_back_pointers`) produces
  the identical `None` by explicitly clearing the back-pointer — not just
  "a reasonable new behavior" but a proven behavioral match, and it's the
  one the `removing_a_child_from_its_only_parents_list_orphans_it` test
  pins.
- **The ordering bug the first draft actually had, and how it surfaced**:
  a parent can legitimately commit before the child it lists has ever
  committed (the pre-existing `resolves_children_to_their_toplevel_ancestor`
  test exercises this deliberately). An eager write-on-parent-commit
  design misses this, since the child doesn't exist yet to receive the
  stamp — and that test caught it immediately (failed) before the
  `pending_parents` deferred-resolution map was added to consume the
  assignment on the child's own first commit. Good evidence the existing
  test suite is doing real work here, not just passing along for the ride.
- **Self-referencing surface (a surface listing itself as its own
  child)**: traced this degenerate case by hand — `sync_child_back_pointers`
  would set a surface's `parent` to itself, but `toplevel_ancestor`'s
  pre-existing `visited: BTreeSet` cycle guard (unchanged by this PR)
  already returns `None` rather than looping, so this resolves safely to
  "no ancestor" rather than hanging or panicking. Not a new risk.
- **Destroy-path hygiene**: `remove_surface` now also clears any
  `pending_parents` entry for the destroyed key (a child could be listed
  by a parent and then destroyed before ever committing), and
  `ClientDisconnected`'s bulk path retains `pending_parents` by
  `client_id` alongside `surfaces`, for parity. Both have dedicated tests.

## Validation Results

| Check | Result |
|---|---|
| Build (`cargo build --workspace --all-targets`) | Pass |
| Tests (`cargo test --workspace`) | Pass — 145/145 (1 correctly ignored, no display in this environment; the new CI job is the first place it actually runs) |
| Lint (`cargo clippy --workspace --all-targets -- -D warnings`) | Pass |
| Format (`cargo fmt --all -- --check`) | Pass |
| YAML (`yamllint .github/workflows/ci.yml`) | Pass — no new warnings on the added lines; pre-existing warnings on the untouched `rust` job (missing `---` doc-start, `on:` truthy ambiguity, one long line) predate this PR |
| CI (GitHub Actions) | Pending at review time — see PR checks |

Test-discrimination discipline: 3 of the 4 new tests were independently
verified by temporarily reverting the specific behavior each one names
and confirming it fails, then restoring it (`removing_a_child_from_its_only_parents_list_orphans_it`,
`a_childs_own_repaint_preserves_its_parent_without_relisting`,
`a_child_destroyed_before_its_first_commit_drops_its_pending_assignment`).
The fourth, `reparenting_a_child_updates_its_toplevel_ancestor`, is a
correctness integration check rather than a regression test — the old
O(N) scan was already correct for straightforward reparenting, so this
test would pass under either implementation; it's included for direct
coverage of the "add" path, not as a discriminator.

## Files Reviewed

- `crates/navette-bridge/src/scene.rs` — Modified (back-pointer field, `sync_child_back_pointers`, `pending_parents` deferred-resolution map, 4 new tests; production code ~606 of 1230 lines, well under the repo's 800-line convention)
- `.github/workflows/ci.yml` — Modified (new `viewer-display` job)
