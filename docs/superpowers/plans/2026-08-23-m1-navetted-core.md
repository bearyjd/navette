# Navette M1 Core Implementation Plan

Status: Complete · 2026-08-23

Goal: implement the approved M1 design as small buildable commits.

## Task 1 — Protocol contract

- Define versioned request/response, app, session, attach, error, and clipboard
  types in `navette-protocol`.
- Add JSON fixture tests for every request and response family.
- Verify formatting, linting, and workspace tests.
- Commit: `feat(protocol): define Navette v1 control API`

## Task 2 — XDG application index

- Convert `navetted` to a library plus thin binary.
- Add an `app_index` module using `freedesktop-desktop-entry`.
- Implement filtering, priority, localization, and executable argv extraction.
- Cover fixtures for hidden/no-display/duplicate/invalid entries and field
  codes.
- Commit: `feat(navetted): add XDG application index`

## Task 3 — Persistent registry

- Add registry storage under the XDG state directory.
- Implement validated session names, atomic JSON persistence, list/get/insert,
  update, and remove/state transitions.
- Add round-trip, corrupt-file, duplicate, and validation tests.
- Commit: `feat(navetted): add persistent session registry`

## Task 4 — Supervisor

- Add runtime-path allocation and process-runner abstraction.
- Implement bounded `wprsd` readiness, app launch, rollback, reconciliation,
  and kill.
- Test behavior with a fake runner; do not require wprs in automated tests.
- Commit: `feat(navetted): supervise named wprs sessions`

## Task 5 — WebSocket API

- Add `/healthz` and `/v1/ws` on loopback by default.
- Enforce `navette.v1`, text-frame size limits, and structured errors.
- Wire app index, registry/supervisor, attach metadata, and clipboard requests.
- Add ephemeral-listener integration tests.
- Commit: `feat(navetted): expose versioned WebSocket API`

## Task 6 — CLI client

- Add daemon URL and optional SSH target configuration.
- Replace stub handlers with protocol calls for all five existing commands.
- Implement local `wprsc` attach and tracked detach behavior; add SSH Unix-socket
  forwarding behind explicit options.
- Preserve parser tests and add response/rendering/error tests.
- Commit: `feat(cli): connect commands to navetted API`

## Task 7 — Operator integration

- Add a sample systemd user service and configuration documentation.
- Run a local smoke test with the M0-built behavior when wprs is available;
  automated tests continue to use fakes.
- Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo test --workspace`, and `cargo build --workspace`.
- Commit: `docs: add M1 operator workflow`

## Done

- `navetted` can discover apps, persist and supervise named sessions, and serve
  the v1 API.
- `navette` can list, run, attach, detach, and kill through that API.
- Two sessions receive distinct displays/sockets by construction.
- Default networking is loopback-only and non-loopback binds require explicit
  acknowledgement.
- No wprs Rust dependency or M2 orchestration is introduced.
