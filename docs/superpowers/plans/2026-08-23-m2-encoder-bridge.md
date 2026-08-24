# Navette M2 Encoder Bridge Implementation Plan

Status: In progress · 2026-08-23

Goal: deliver the approved M2 bridge as reviewed, CI-gated pull requests.

## PR 1 — Architecture and CI

- Add the M2 design and implementation plan.
- Add required formatting, lint, test, and build CI.
- Verify locally, review the diff, merge only after GitHub CI passes.

## PR 2 — Recoverable wprs transport and scene capture

- Prepare the minimal wprs transport lifecycle change as its own reviewed PR.
- Pin the reviewed wprs commit in `navette-bridge`.
- Implement bounded connection lifecycle, raw-buffer pairing, scene state,
  surface/toplevel/popup creation and destruction, and compositing.
- Add fixture and live stock-`wprsd` capture tests.

## PR 3 — Media contract and bounded distribution

- Define and test the binary media header and client input messages.
- Add per-session media routing and bounded fan-out queues.
- Enforce size, identity, rate, and cross-session boundaries.
- Test slow clients, reconnect/keyframe, malformed frames, and disconnect
  cleanup.

## PR 4 — H.264 encoder

- Add the encoder abstraction and FFmpeg process lifecycle.
- Probe VA-API and select Intel hardware encoding when functional.
- Add libx264 fallback, Annex-B access-unit parsing, forced keyframes, resize
  reconfiguration, metrics, and deterministic fake-encoder tests.
- Measure synthetic and captured application frames.

## PR 5 — Viewer, input, and resize

- Add the throwaway Linux viewer with headless adapters.
- Decode/display H.264, forward scoped pointer/keyboard events, and debounce
  viewport resize.
- Add the performance HUD and input/resize adversarial tests.
- Exercise Firefox and Claude Desktop end to end.

## PR 6 — M2 gate report

- Run the 30-minute endurance and performance gate.
- Run LAN/tailnet measurements if another endpoint is available.
- Record pass/fail evidence, remaining limitations, exact dependency revisions,
  and cleanup.
- Run all workspace gates and merge the completion report.
