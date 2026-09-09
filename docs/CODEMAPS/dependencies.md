<!-- Generated: 2026-09-08 | Files scanned: 6 Cargo.toml + gradle | Token estimate: ~700 -->

# Dependencies

## Pinned git dependencies — read before upgrading

| Crate | Source | Pin |
|---|---|---|
| `wprs` | `github.com/bearyjd/wprs` | rev `5763d746` |
| `minifb` | `github.com/emoon/rust_minifb` | rev `3a711add` |

**`wprs` is a fork pin.** Local checkout for reading:
`~/.cargo/git/checkouts/wprs-a1177b03fe300706/2481a23`.

**`minifb` is an upstream pin, not a fork** — the Wayland keysym fix was merged
upstream, so the fork was dropped. The viewer still crashes on non-XKB keymaps.

## Rust crates

| Crate | Key deps |
|---|---|
| `navette-protocol` | serde only — **no wprs**, so clients never link it |
| `navette-bridge` | `wprs`, encoder backends |
| `navetted` | `axum` 0.8 (ws), `tokio` 1, `wprs`, `tokio-tungstenite` 0.30 |
| `navette-viewer` | `minifb`, `tokio`, `tokio-tungstenite`, `axum` |
| `navette-cli` | `tokio`, `tokio-tungstenite`, `clap` |

Rust edition **2024**, workspace resolver **3**. License AGPL-3.0-only,
`publish = false`.

## External processes

| Process | Role | Lifecycle |
|---|---|---|
| `wprsd` | upstream wprs server; runs the guest's Wayland session | spawned and reaped per session by `supervisor.rs` |
| `wprsc` | upstream wprs client | used by `navette-cli attach`, not by the daemon |
| guest apps | unmodified Wayland clients | children of `wprsd` |

`navetted` does **not** spawn `navette-bridge` — it links it as a library and
runs the calloop loop in-process.

## Encoders

```
EncoderBackend = Vaapi { device } | Libx264 | Fake
```

- **VAAPI** — hardware, needs a render node (`/dev/dri/renderD*`)
- **libx264** — software fallback, no GPU required
- **Fake** — tests only

Both real backends run `-bf 0`: no B-frames, so decode order equals
presentation order. Client decoders assume this.

## Android

| Dependency | Purpose |
|---|---|
| Jetpack Compose | UI |
| kotlinx.serialization | JSON wire messages |
| OkHttp | WebSocket |
| `MediaCodec` (platform) | H.264 decode, async on a HandlerThread |
| Turbine | Flow testing |
| JUnit | unit tests |

**Debug builds skip JS-style bundling pitfalls but have their own trap:** set
`debuggableVariants = []` and clear `/tmp/metro-cache` for sideloaded APKs.

## Network

No cloud services, no third-party APIs, no telemetry, no auth provider.
Transport is a **tailnet** — the daemon binds a tailnet address and the phone
connects directly. There is no TLS termination or gateway in the path.

## CI

Three GitHub Actions checks: `rust`, `android`, `viewer-display`.

## Upgrade hazards

- **wprs rev bump** — the scene, input and data-device types are used
  structurally. Cargo's cache can hide struct-shape breakage; run
  `cargo clean && cargo test` before trusting a green run.
- **Adding a `MediaInput` variant** — breaks exhaustive matches in
  `navette-bridge/src/input.rs` and `navette-viewer/src/session.rs`.
  `cargo build --workspace` will **not** catch the latter (it is `#[cfg(test)]`
  code); use `cargo clippy --workspace --all-targets -- -D warnings`.
- **Adding a `MediaKind`** — breaking for old Android clients, which throw on
  unknown kinds. Server→client additions should travel as JSON text instead.
