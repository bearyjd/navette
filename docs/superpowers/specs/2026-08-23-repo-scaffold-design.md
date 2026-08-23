# Navette — Repo Scaffold Design

Status: Approved · Date: 2026-08-23

## Purpose

Turn the empty `navette/` repo (currently just `docs/ROADMAP.md` and
`docs/prp/startup.md`, and a fresh `git init` with no commits) into a
buildable Rust workspace skeleton, so subsequent milestone work (starting
with M0/M1 from the PRP) has a project to build inside instead of starting
from nothing.

This is scaffolding only: no wprs integration, no orchestration logic, no
protocol design. Those are separate, later pieces of work gated on PRP
milestones (notably M0's adopt-vs-fork gate, PRP §6, and D1).

## Decisions

1. **Repo layout** — single Cargo workspace at the repo root, resolving
   PRP open question #5. Rust crates live under `crates/`; an `android/`
   placeholder directory is included now (empty except a short README
   noting the real Kotlin/Compose client lands at M3, PRP §4.3) so the
   top-level layout matches the PRP's architecture diagram from day one.
2. **No wprs dependency yet** — PRP D1 ("adopt vs fork") is explicitly
   gated on M0's exit criteria, which hasn't run. Wiring wprs into
   `Cargo.toml` now would commit to D1/D5 before that gate. All crates
   compile dependency-free (aside from `clap`, `tokio`, `tracing`,
   `serde` — general-purpose, not wprs-specific).
3. **Four crates**, matching PRP §4 plus one addition:
   - `navetted` (bin) — orchestration daemon (PRP §4.1).
   - `navette-cli` (bin) — CLI/desktop client (PRP §4.2).
   - `navette-bridge` (lib) — in-process wprs client role / encoder
     bridge (PRP §4.1, D5). Empty until M0 validates wprs adoption.
   - `navette-protocol` (lib, **new**, not named in the PRP) — shared
     WS/JSON control-channel types consumed by `navetted`, `navette-cli`,
     and eventually Android via codegen (PRP §4.3, D4, D6). Added now
     because the coding-style rule favors small, high-cohesion crates
     over a later extraction refactor, and because D4 already commits to
     a stable API being a first-class concern.
4. **Tooling**: Rust edition 2024, no `rust-toolchain.toml` pin
   (single-operator project; add a pin later only if a version-skew
   problem actually appears), `Cargo.lock` committed (workspace produces
   binaries), no CI workflow yet (deliberately deferred, not forgotten).

## Crate contents

### `navetted`
Minimal `main.rs`: tokio runtime bootstrap, `tracing` subscriber init,
prints "navetted: not yet implemented" and exits. No module scaffolding
for supervisor/registry/app-index yet — those get built when M1 work
actually starts, per YAGNI (empty module files with no behavior are
premature structure).

### `navette-cli`
`clap`-derive binary wiring the five subcommands named explicitly in the
PRP (`navette ls`, `navette <host> run <app>`, `attach`, `detach`,
`kill`). Each subcommand is a stub that prints "not yet implemented" and
exits non-zero. One test asserts `clap`'s derived parser accepts each
subcommand's expected argument shape — this is real, testable behavior
(arg parsing), unlike the other crates which have no behavior yet.

### `navette-bridge`
Empty lib. `lib.rs` carries a doc comment describing its future role
(D5: in-process wprs client, VA-API H.264 encode). No dependencies beyond
what the workspace needs to compile.

### `navette-protocol`
Empty lib except `serde` as a dependency. A placeholder module doc notes
that control-channel request/response types land here once the Navette
API is actually designed (M1).

## Testing

Only `navette-cli` gets a test in this scaffold (arg-parsing), per the
project's TDD/coverage rules — the other three crates have no behavior
yet, so there is nothing correctness-bearing to test. Coverage
requirements apply to real logic as it's added in later milestone work,
not to inert scaffolding.

## Out of scope (explicitly deferred)

- wprs integration (gated on M0)
- Navette API design (M1)
- Supervisor/registry/app-index implementation (M1)
- CI workflow
- `rust-toolchain.toml` version pin
- Android client implementation (M3)

## Next step

Implementation plan via the `writing-plans` skill, then execute: create
the workspace files, verify `cargo build` and `cargo test` succeed, and
make the first commit.
