# Contributing

## Prerequisites

Linux with Wayland. The daemon supervises stock `wprsd`, so wprs must be
installed and on `PATH` (or pointed at with `--wprsd`).

<!-- AUTO-GENERATED: from .github/workflows/ci.yml -->

System packages CI installs, and the minimum you need locally:

```bash
sudo apt-get install --yes libwayland-dev libxkbcommon-dev
```

Android work additionally needs JDK 17 (temurin) and Android SDK
`platform-tools` + `platforms;android-36`.

<!-- END AUTO-GENERATED -->

Rust edition is **2024** with workspace resolver **3**, so a recent toolchain
is required.

## Commands

<!-- AUTO-GENERATED: from Cargo.toml, android/gradlew, .github/workflows/ci.yml -->

| Command | Description |
|---|---|
| `cargo build --workspace` | Build every crate |
| `cargo test --workspace` | Run the full Rust suite |
| `cargo fmt --all -- --check` | Formatting gate (CI runs exactly this) |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lint gate |
| `xvfb-run -a cargo test -p navette-viewer -- --ignored native::` | Viewer display tests (need an X server) |
| `cd android && ./gradlew assembleDebug` | Build the Android debug APK |
| `cd android && ./gradlew testDebugUnitTest --rerun-tasks` | Android unit tests |
| `cd android && ./gradlew lintDebug` | Android lint |
| `cd android && ./gradlew installDebug` | Install to a connected device |

Binaries produced: `navetted` (daemon), `navette` (CLI), `navette-viewer`
(Linux client).

<!-- END AUTO-GENERATED -->

### Two gate details that have bitten this project

**Use `--all-targets` on clippy, not `cargo build --workspace`.** `build` does
not compile `#[cfg(test)]` code, so it will happily pass while a test-only
exhaustive `match` is broken. Adding a `MediaInput` variant has broken matches
in `navette-bridge/src/input.rs` and `navette-viewer/src/session.rs`, and only
the clippy form catches the second.

**Read Android test counts from the JUnit XML, not the Gradle line.** A cached
"BUILD SUCCESSFUL in 2s" can run **zero** tests and is indistinguishable from a
real pass. Always pass `--rerun-tasks` and check:

```bash
android/app/build/test-results/testDebugUnitTest/*.xml
```

## Environment variables

There is no `.env.example` — the daemon and clients are configured by flags,
with a few variables as overrides.

<!-- AUTO-GENERATED: from clap `env =` attributes and env::var call sites -->

| Variable | Required | Description | Example |
|---|---|---|---|
| `NAVETTE_URL` | No | Daemon endpoint for `navette` and `navette-viewer`. Equivalent to `--url`. | `ws://127.0.0.1:9417/v1/ws` |
| `NAVETTE_SSH` | No | SSH host used to forward the wprs Unix socket for `attach`. Equivalent to `--ssh`. | `user@host` |
| `RUST_LOG` | No | Log filter. Defaults to `info`. | `debug`, `navetted=trace` |
| `XDG_STATE_HOME` | No | Base for the persistent session registry (`registry.rs:310`). | `~/.local/state` |
| `XDG_RUNTIME_DIR` | No | Session socket directory. Overridable with `--runtime-dir`. | `/run/user/1000` |
| `WAYLAND_DISPLAY` | No | Read when talking to the host compositor. | `wayland-0` |

<!-- END AUTO-GENERATED -->

## Flags

<!-- AUTO-GENERATED: from clap derive definitions -->

**`navetted`**

| Flag | Default | Purpose |
|---|---|---|
| `--bind` | `127.0.0.1:9417` | Control API address |
| `--allow-remote` | off | Acknowledge that binding the unauthenticated API beyond loopback is unsafe |
| `--state-file` | XDG state | Override the session registry path |
| `--runtime-dir` | `XDG_RUNTIME_DIR` | Override session socket directory |
| `--wprsd` | `wprsd` | wprsd executable to supervise |

**`navette`** — global: `--url`, `--ssh`, `--wprsc` (default `wprsc`).
Subcommands: `ls`, `run`, `attach`, `detach`, `kill`.

**`navette-viewer`** — `--url` (default `ws://127.0.0.1:9417`), `--ffmpeg`
(default `ffmpeg`).

<!-- END AUTO-GENERATED -->

## Testing

Tests live beside the code they cover, in `#[cfg(test)] mod tests`. Android
tests are under `android/app/src/test/kotlin/`.

Expectations for new tests:

- **Write the test first and watch it fail.** A test whose RED was inferred
  rather than observed is how a vacuous test reached this repo: it passed for
  an unrelated reason and would have passed under the very bug it existed to
  catch. If a test pins an invariant, prove it fails when you break that
  invariant.
- **Never leave a socket wait unbounded.** Wrap every `socket.next().await` in
  `tokio::time::timeout`. An unbounded wait once hung the suite for five
  minutes.
- **Supply time from the caller.** HUD and metric code takes `nowMs` as a
  parameter rather than reading a clock, which is what makes rolling windows
  assertable in a plain JVM test.
- Coverage target is 80%.

## Code style

- Immutability by default; return new values rather than mutating in place.
- Files 200–400 lines typical, **800 maximum**. `SessionScreen.kt` is
  currently over that ceiling and is a known, recorded exception.
- Functions under 50 lines, nesting under 4 levels.
- No hardcoded values — use constants.
- Handle errors explicitly; never swallow them silently.
- **Never log secrets or user content.** Clipboard text in particular must
  never reach a log line at any level: log lengths, MIME types, error kinds and
  counts instead. Note that `Debug` derives on clipboard types render payloads
  under `{:?}`, so never log `?action` or `?event`.

## Architecture orientation

Read `docs/CODEMAPS/` before a first substantial change — five short files
covering the system, the daemon, both clients, the wire formats, and the
dependency hazards.

The single most misread fact: **`navette-bridge` is a library, not a process.**
`navetted` links it and owns the calloop loop, so there is no IPC hop between
them.

## Pull requests

1. Branch off `master`.
2. Conventional commit format: `<type>: <description>` where type is one of
   `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `ci`.
3. All three CI checks must pass: `rust`, `android`, `viewer-display`.
4. Include a test plan, and mark unchecked boxes honestly — an unrun check is
   more useful stated than omitted.
5. Resolve conflicts and rebase before requesting review.
