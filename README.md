# Navette

Navette (Fr. *shuttle*) is a "tmux for GUI apps" orchestration layer built
on top of [wprs](https://github.com/wayland-transpositor/wprs). Apps stay
put on a host as named, persistent sessions; the Navette shuttle carries
your window to whatever device you're holding — a Linux desktop as a real
native Wayland window, a phone over a decoded video stream.

Org: Grepon Labs LLC · License: AGPL-3.0-only · Status: early scaffolding,
no working software yet.

## Project layout

- `crates/navetted` — orchestration daemon
- `crates/navette-cli` — CLI / desktop client
- `crates/navette-bridge` — in-process wprs client role (encoder bridge)
- `crates/navette-protocol` — shared Navette API types
- `android/` — Navette Android client (not started; see M3 in the roadmap)

## Docs

- [`docs/prp/startup.md`](docs/prp/startup.md) — product requirements & plan
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — feature roadmap
- [`docs/superpowers/specs/`](docs/superpowers/specs/) — design specs
- [`docs/superpowers/plans/`](docs/superpowers/plans/) — implementation plans

## Building

```bash
cargo build --workspace
```
