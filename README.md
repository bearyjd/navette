# Navette

Navette (Fr. *shuttle*) is a "tmux for GUI apps" orchestration layer built
on top of [wprs](https://github.com/wayland-transpositor/wprs). Apps stay
put on a host as named, persistent sessions; the Navette shuttle carries
your window to whatever device you're holding — a Linux desktop as a real
native Wayland window, a phone over a decoded video stream.

Org: Grepon Labs LLC · License: AGPL-3.0-only · Status: experimental — M1
host daemon and Linux CLI, M2 encoder bridge, M3 Android client.

## Project layout

- `crates/navetted` — orchestration daemon
- `crates/navette-cli` — CLI / desktop client
- `crates/navette-bridge` — in-process wprs client role (encoder bridge)
- `crates/navette-protocol` — shared Navette API types
- `crates/navette-viewer` — Linux viewer client
- `android/` — Navette Android client (Compose; drawer, session screen, perf HUD)

## Docs

- [`docs/CODEMAPS/`](docs/CODEMAPS/) — architecture maps, read these first
- [`docs/CONTRIBUTING.md`](docs/CONTRIBUTING.md) — setup, commands, test gates
- [`docs/RUNBOOK.md`](docs/RUNBOOK.md) — running, diagnosing, rolling back
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — feature roadmap
- [`docs/HANDOFF.md`](docs/HANDOFF.md) — running engineering log
- [`docs/prp/PRP-plan.md`](docs/prp/PRP-plan.md) — product requirements & plan
- [`docs/superpowers/specs/`](docs/superpowers/specs/) — design specs
- [`docs/superpowers/plans/`](docs/superpowers/plans/) — implementation plans

## Building

```bash
cargo build --workspace
```

The M1 daemon discovers XDG applications, supervises named sessions through
stock `wprsd`, and exposes a loopback WebSocket API. The CLI can list, run,
attach, detach, and kill sessions. See the
[M1 operator guide](docs/operators/m1.md) for setup and safety boundaries.
