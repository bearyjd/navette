# Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close a live browser-origin vulnerability, put authentication on every API route, and bound an unbounded allocation in the wprs read path.

**Architecture:** One axum middleware performs both gate checks in order (`Origin` first, then bearer token) so ordering is explicit rather than emergent from layer nesting. The token is a value type with no I/O, persisted separately, and carried in `ApiState`. The phone pairs by scanning a QR the CLI renders, storing host and token in a single-slot encrypted store. The wprs ceilings are a patch to our own fork plus a rev bump.

**Tech Stack:** Rust (axum 0.8, clap, `subtle`, `rand`, `qrcode`), Kotlin (OkHttp, Compose, `play-services-code-scanner`, `androidx.security:security-crypto`).

**Spec:** `docs/superpowers/specs/2026-09-12-hardening-design.md`

## Global Constraints

- The token value is **never logged at any level, in either language**. Startup logs that a token was loaded, never its value.
- Token comparison is **constant-time**, via `subtle::ConstantTimeEq`. Never `==` on the secret.
- The token file is created `0600` via `OpenOptions::new().create_new(true).mode(0o600)`.
- The token applies to **every route with no loopback exemption**, `/healthz` included.
- Any request carrying an `Origin` header is refused **403**, on every route.
- Authentication failure is a bare **401** with no detail about why.
- `Host` validation is deliberately **not** implemented. See spec §2.
- A token file that exists but cannot be read or parsed is a **fatal startup error**, never a trigger to regenerate.
- Rust gate before every commit: `cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo fmt --check`. `--all-targets` is required: `cargo build --workspace` does not compile test targets and has previously let a break through.
- Android gate: `./gradlew :app:testDebugUnitTest`.
- **Daemon enforcement and Android pairing must land in the same branch.** Once Task 4 lands, any client that cannot present a token is locked out, and the phone cannot read the token file. Do not merge a partial branch.

## Deliberate deviations from the spec

Recorded here so a reviewer does not read them as defects:

1. **`--advertise-host` lives on `navette token`, not on `navetted`.** Spec §3 put it on the daemon. The daemon never needs its own advertised name — only the command rendering the QR does, and that is the CLI. The *rule* is unchanged: use the URL host when it is a specific non-loopback address, otherwise require the flag rather than guessing.
2. **`navette token` is documented as a local admin command.** It reads the daemon's token file directly, so it only works on the daemon's own host. This is inherent, not a limitation to fix.

## File Structure

| File | Responsibility |
|---|---|
| `crates/navette-auth/src/lib.rs` (new) | `AuthToken` value type: generation, Crockford base32 render/parse, constant-time compare, file persistence. No axum types. |
| `crates/navetted/src/guard.rs` (new) | The axum middleware: `Origin` rejection then bearer verification. No token internals. |
| `crates/navetted/src/api.rs` | `ApiState` gains `auth`; `router()` applies the guard. Signature of `router()` unchanged. |
| `crates/navetted/src/main.rs` | Loads or creates the token at startup; updated `--allow-remote` help. |
| `crates/navette-cli/src/main.rs` | `token` subcommand (`--qr`, `--rotate`); global `--token`/`NAVETTE_TOKEN`. |
| `crates/navette-cli/src/lib.rs`, `crates/navette-viewer/src/client.rs` | Send `Authorization` on the handshake. |
| `android/.../net/PairingStore.kt` (new) | Single-slot credential store behind an interface, so consumers are unit-testable with a fake. |
| `android/.../net/PairingUri.kt` (new) | Pure parser for `navette://pair?…`. Unit-testable with no framework. |
| `android/.../net/{NavetteClient,MediaClient}.kt` | Send `Authorization`; map a 401 to `ConnectionState.Unauthorized`. |
| `android/.../ui/session/ReconnectPolicy.kt` | 401 is terminal. |

---

### Task 1: `AuthToken` — value type and persistence

**Files:**
- Create: `crates/navette-auth/src/lib.rs`, `crates/navette-auth/Cargo.toml`
- Modify: `Cargo.toml` (workspace members), `crates/navetted/Cargo.toml`

**Why its own crate.** `navette-cli` and `navette-viewer` both need `AuthToken`,
and neither depends on `navetted` — they depend on `navette-protocol`. Making the
CLI depend on `navetted` would drag axum, the supervisor and wprs into a client
binary. Putting file I/O into `navette-protocol` would give the wire-format crate
filesystem concerns it has no business holding. A small dedicated crate is the
boundary that costs least.

**Interfaces:**
- Produces: `AuthToken::generate() -> AuthToken`, `AuthToken::render(&self) -> String` (24 chars, ungrouped), `AuthToken::render_grouped(&self) -> String` (`XXXX-XXXX-…`), `AuthToken::parse(&str) -> Result<AuthToken, AuthError>`, `AuthToken::matches(&self, presented: &str) -> bool`, `AuthToken::load_or_create(path: &Path) -> Result<AuthToken, AuthError>`, `AuthToken::rotate(path: &Path) -> Result<AuthToken, AuthError>`, `default_token_path() -> Option<PathBuf>`.

- [ ] **Step 1: Create the crate**

`crates/navette-auth/Cargo.toml`:

```toml
[package]
name = "navette-auth"
version = "0.1.0"
edition = "2024"

[dependencies]
rand = "0.8"
subtle = "2.6"
thiserror = "2"
url = "2"

[dev-dependencies]
tempfile = "3"
```

Copy `edition` from `crates/navette-protocol/Cargo.toml` rather than trusting the
value above — the workspace must stay on one edition.

Add `"crates/navette-auth"` to the workspace `members` list in the root
`Cargo.toml`, and add `navette-auth = { path = "../navette-auth" }` to the
`[dependencies]` of `crates/navetted/Cargo.toml`. Tasks 5 and 6 add the same line
to `navette-cli` and `navette-viewer`; do not add it for them here.

- [ ] **Step 2: Write the failing tests**

Create `crates/navette-auth/src/lib.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_twenty_four_crockford_characters() {
        let token = AuthToken::generate();
        let rendered = token.render();
        assert_eq!(rendered.len(), 24);
        assert!(
            rendered.bytes().all(|b| ALPHABET.contains(&b)),
            "rendered token must use only the Crockford alphabet: {rendered}"
        );
        for excluded in ['I', 'L', 'O', 'U'] {
            assert!(!rendered.contains(excluded), "{excluded} is ambiguous and excluded");
        }
    }

    #[test]
    fn round_trips_through_parse() {
        let token = AuthToken::generate();
        let parsed = AuthToken::parse(&token.render()).unwrap();
        assert!(parsed.matches(&token.render()));
    }

    #[test]
    fn parse_accepts_grouped_and_lowercase_input() {
        // Grouping is a display convenience; a human retyping it must not be
        // punished for the dashes we printed or for their shift key.
        let token = AuthToken::generate();
        let grouped = token.render_grouped().to_lowercase();
        let parsed = AuthToken::parse(&grouped).unwrap();
        assert!(parsed.matches(&token.render()));
    }

    #[test]
    fn parse_rejects_wrong_length_and_bad_characters() {
        assert!(AuthToken::parse("").is_err());
        assert!(AuthToken::parse("ABC").is_err());
        assert!(AuthToken::parse(&"A".repeat(25)).is_err());
        // 'I' is excluded from the alphabet, so a 24-char string using it is invalid.
        assert!(AuthToken::parse(&"I".repeat(24)).is_err());
    }

    #[test]
    fn does_not_match_a_different_token() {
        let a = AuthToken::generate();
        let b = AuthToken::generate();
        assert!(!a.matches(&b.render()));
        assert!(!a.matches("not a token"));
    }

    #[test]
    fn debug_never_reveals_the_value() {
        // A secret that lands in a tracing field or a panic message via Debug
        // is the exact leak the global constraint forbids, so the type must
        // not be able to print itself even by accident.
        let token = AuthToken::generate();
        let debug = format!("{token:?}");
        assert!(!debug.contains(&token.render()), "Debug leaked the token: {debug}");
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn load_or_create_writes_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("token");
        let token = AuthToken::load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "token file must not be readable by others");
        // Reopening returns the same value rather than minting a new one.
        let again = AuthToken::load_or_create(&path).unwrap();
        assert!(again.matches(&token.render()));
    }

    #[test]
    fn load_or_create_fails_loudly_on_a_corrupt_file() {
        // Regenerating here would silently invalidate every paired client the
        // first time a truncated write happened, and the operator would see
        // only that every device stopped working.
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("token");
        std::fs::write(&path, b"not-a-valid-token").unwrap();
        assert!(AuthToken::load_or_create(&path).is_err());
    }

    #[test]
    fn rotate_replaces_the_stored_value() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("token");
        let first = AuthToken::load_or_create(&path).unwrap();
        let second = AuthToken::rotate(&path).unwrap();
        assert!(!second.matches(&first.render()));
        let reloaded = AuthToken::load_or_create(&path).unwrap();
        assert!(reloaded.matches(&second.render()));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p navette-auth`
Expected: FAIL to compile — `AuthToken` not found.

- [ ] **Step 4: Implement the module**

Write above the test module in `crates/navette-auth/src/lib.rs`:

```rust
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rand::RngCore;
use subtle::ConstantTimeEq;
use thiserror::Error;

/// Crockford base32 minus the ambiguous letters I, L, O and U, so a token read
/// off a screen and retyped cannot become a different valid token.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 120 bits: divides evenly by 5, so the encoding needs no padding, and 24
/// characters is short enough to retype as the QR fallback.
const TOKEN_BYTES: usize = 15;
const TOKEN_CHARS: usize = 24;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("token must be {TOKEN_CHARS} characters from the Crockford alphabet")]
    Malformed,
    #[error("failed to read token file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write token file {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("token file {path} exists but is not a valid token; refusing to overwrite it — inspect it, or run `navette token --rotate` to replace it deliberately")]
    CorruptFile { path: PathBuf },
    #[error("cannot determine a token path: set XDG_STATE_HOME or HOME, or pass --token-file")]
    NoPath,
}

#[derive(Clone)]
pub struct AuthToken([u8; TOKEN_BYTES]);

/// Hand-written so the secret cannot reach a log line, a panic message, or a
/// `tracing` field through a derived `Debug`.
impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(redacted)")
    }
}

impl AuthToken {
    pub fn generate() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(TOKEN_CHARS);
        let mut acc: u32 = 0;
        let mut bits = 0u32;
        for &byte in &self.0 {
            acc = (acc << 8) | u32::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(char::from(ALPHABET[((acc >> bits) & 0x1f) as usize]));
            }
        }
        out
    }

    pub fn render_grouped(&self) -> String {
        let raw = self.render();
        raw.as_bytes()
            .chunks(4)
            .map(|chunk| std::str::from_utf8(chunk).expect("alphabet is ASCII").to_owned())
            .collect::<Vec<_>>()
            .join("-")
    }

    pub fn parse(input: &str) -> Result<Self, AuthError> {
        let cleaned: Vec<u8> = input
            .bytes()
            .filter(|b| !matches!(b, b'-' | b' '))
            .map(|b| b.to_ascii_uppercase())
            .collect();
        if cleaned.len() != TOKEN_CHARS {
            return Err(AuthError::Malformed);
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        let mut acc: u32 = 0;
        let mut bits = 0u32;
        let mut index = 0;
        for character in cleaned {
            let value = ALPHABET
                .iter()
                .position(|&candidate| candidate == character)
                .ok_or(AuthError::Malformed)? as u32;
            acc = (acc << 5) | value;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                bytes[index] = ((acc >> bits) & 0xff) as u8;
                index += 1;
            }
        }
        Ok(Self(bytes))
    }

    /// Constant-time. An early-exit comparison on a secret hands out a timing
    /// oracle that lets an attacker recover it byte by byte.
    pub fn matches(&self, presented: &str) -> bool {
        let Ok(other) = Self::parse(presented) else {
            return false;
        };
        self.0.ct_eq(&other.0).into()
    }

    pub fn load_or_create(path: &Path) -> Result<Self, AuthError> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse(contents.trim()).map_err(|_| AuthError::CorruptFile {
                path: path.to_path_buf(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let token = Self::generate();
                token.write_private(path)?;
                Ok(token)
            }
            Err(source) => Err(AuthError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Writes to a temporary file and renames over the old one. `rename` is
    /// atomic within a filesystem, so there is no window where the old token is
    /// gone and the new one has not landed. Unlinking first and then writing
    /// would leave no token file at all if the write failed, and the next
    /// startup would silently mint a third value.
    pub fn rotate(path: &Path) -> Result<Self, AuthError> {
        let token = Self::generate();
        let staging = path.with_extension("next");
        let _ = fs::remove_file(&staging);
        token.write_private(&staging)?;
        fs::rename(&staging, path).map_err(|source| AuthError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(token)
    }

    fn write_private(&self, path: &Path) -> Result<(), AuthError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| AuthError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|source| AuthError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        writeln!(file, "{}", self.render()).map_err(|source| AuthError::Write {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Mirrors `registry.rs`'s `default_registry_path` resolution so both pieces of
/// daemon state land in the same place.
pub fn default_token_path() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|base| base.join("navette/token"))
}
```


- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p navette-auth`
Expected: PASS, 9 tests.

- [ ] **Step 6: Run the full gate and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo fmt --check
git add crates/navette-auth Cargo.toml crates/navetted/Cargo.toml Cargo.lock
git commit -m "feat(auth): add the AuthToken value type and its private persistence"
```

---

### Task 2: The `Origin` guard (closes the live hole)

This task is deliberately shippable on its own: it closes the vulnerability without depending on any token work.

**Files:**
- Create: `crates/navetted/src/guard.rs`
- Modify: `crates/navetted/src/api.rs:76-82` (`router`), `crates/navetted/src/lib.rs`

**Interfaces:**
- Produces: `guard::reject_browser_origin(request: Request, next: Next) -> Response`.

- [ ] **Step 1: Add the test dependency**

`oneshot` comes from tower's `ServiceExt`, which navetted does not currently
depend on. Add to `crates/navetted/Cargo.toml` under `[dev-dependencies]`:

```toml
tower = { version = "0.5", features = ["util"] }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/navetted/src/guard.rs` with the test module. These are HTTP-level tests against the real router:

```rust
#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::api::tests_support::test_router;

    #[tokio::test]
    async fn rejects_any_request_carrying_an_origin_header() {
        // Browsers do not preflight WebSocket handshakes: they open the socket
        // and leave rejection to us. Without this, any visited page can attach
        // to a session and read the screen.
        for path in ["/healthz", "/v1/ws", "/v1/sessions/work/media"] {
            let router = test_router();
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("Origin", "https://evil.example")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{path} must refuse a request with an Origin header"
            );
        }
    }

    #[tokio::test]
    async fn allows_a_request_with_no_origin_header() {
        let router = test_router();
        let response = router
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_an_empty_origin_header() {
        // "null" and "" are what a sandboxed iframe and some privacy tools
        // send. Presence is the rule, not the value.
        let router = test_router();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .header("Origin", "")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
```

- [ ] **Step 2: Expose a test router from `api.rs`**

`test_state` is currently private to `api.rs`'s own test module. Add a small support module in `crates/navetted/src/api.rs` so `guard.rs` tests can build a real router:

```rust
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use tempfile::TempDir;

    /// Leaks a TempDir so the returned router owns a live registry path for the
    /// duration of the test. Acceptable in tests; never do this in production
    /// code.
    pub(crate) fn test_router() -> Router {
        let temp = Box::leak(Box::new(TempDir::new().unwrap()));
        router(super::tests::test_state(temp))
    }
}
```

Mark `fn test_state` in `api.rs`'s test module as `pub(crate) fn test_state`.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p navetted guard::`
Expected: FAIL — `reject_browser_origin` does not exist; and once wired, the Origin tests return 200/400 rather than 403.

- [ ] **Step 4: Implement the guard**

Above the test module in `crates/navetted/src/guard.rs`:

```rust
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Refuses any request a browser originated.
///
/// navette has no browser client, so this is a blanket rejection rather than a
/// policy with an allowlist to maintain. It is load-bearing: browsers do not
/// apply CORS preflight to WebSocket handshakes -- they open the connection and
/// leave rejection to the server -- so without this, any page the user visits
/// can attach to `/v1/sessions/{s}/media`, receive the screen, and inject input
/// while navetted runs on loopback.
///
/// Presence is the test, not the value: `null` and the empty string are what a
/// sandboxed iframe and some privacy tools send, and all of them are browsers.
pub async fn reject_browser_origin(request: Request, next: Next) -> Response {
    if request.headers().contains_key(axum::http::header::ORIGIN) {
        return (StatusCode::FORBIDDEN, "requests from browsers are not accepted").into_response();
    }
    next.run(request).await
}
```

Add `pub mod guard;` to `crates/navetted/src/lib.rs`.

- [ ] **Step 5: Wire it into the router**

In `crates/navetted/src/api.rs`, replace `router`:

```rust
pub fn router<R: ProcessRunner>(state: ApiState<R>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/ws", any(websocket::<R>))
        .route("/v1/sessions/{session}/media", any(media_websocket::<R>))
        .layer(axum::middleware::from_fn(crate::guard::reject_browser_origin))
        .with_state(state)
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p navetted guard::`
Expected: PASS, 3 tests.

- [ ] **Step 7: Prove our own clients send no `Origin`**

Add to `crates/navetted/src/api.rs`'s test module. The rule is worthless if our own traffic trips it, and "tungstenite does not set Origin" is currently an assumption:

```rust
#[tokio::test]
async fn our_own_websocket_client_sends_no_origin_header() {
    let temp = TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(test_state(&temp))).await.unwrap();
    });
    let mut request = format!("ws://{address}/v1/ws")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        WEBSOCKET_SUBPROTOCOL.parse().unwrap(),
    );
    assert!(
        !request.headers().contains_key("Origin"),
        "our client must not send Origin, or the guard would lock us out"
    );
    assert!(connect_async(request).await.is_ok());
}
```

- [ ] **Step 8: Run the full gate and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo fmt --check
git add crates/navetted/src/guard.rs crates/navetted/src/api.rs crates/navetted/src/lib.rs
git commit -m "fix(api): refuse browser-originated requests on every route

Browsers do not preflight WebSocket handshakes, so while navetted ran on
loopback -- the documented default -- any visited page could attach to a
session's media socket, read the screen, and inject input. The /v1/ws
subprotocol check was not a defense: JS sets subprotocols."
```

---

### Task 3: Bearer verification

**Files:**
- Modify: `crates/navetted/src/guard.rs`, `crates/navetted/src/api.rs:30-57` (`ApiState`), `crates/navetted/src/main.rs:90`, `crates/navette-viewer/tests/media_endpoint.rs:67`

**Interfaces:**
- Consumes: `AuthToken` from Task 1.
- Produces: `ApiState { auth: Arc<AuthToken>, .. }`, `ApiState::new(apps, supervisor, auth)`, `guard::authenticate`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/navetted/src/guard.rs`'s test module:

```rust
#[tokio::test]
async fn rejects_a_request_with_no_authorization_header() {
    for path in ["/healthz", "/v1/ws", "/v1/sessions/work/media"] {
        let router = test_router();
        let response = router
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path} must require a token");
    }
}

#[tokio::test]
async fn rejects_a_wrong_token() {
    let router = test_router();
    let wrong = navette_auth::AuthToken::generate().render();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("Authorization", format!("Bearer {wrong}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_the_configured_token() {
    let (router, token) = crate::api::tests_support::test_router_with_token();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("Authorization", format!("Bearer {}", token.render()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn origin_is_refused_before_the_token_is_checked() {
    // A browser presenting a stolen token must still get 403, not 401: the
    // origin rule is the outer gate and its failure must not be maskable by
    // supplying a valid credential.
    let (router, token) = crate::api::tests_support::test_router_with_token();
    let response = router
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("Origin", "https://evil.example")
                .header("Authorization", format!("Bearer {}", token.render()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p navetted guard::`
Expected: FAIL — `test_router_with_token` missing; unauthenticated requests return 200.

- [ ] **Step 3: Add `auth` to `ApiState`**

In `crates/navetted/src/api.rs`, add the field, clone it in `Clone`, and take it in `new`:

```rust
pub struct ApiState<R: ProcessRunner> {
    pub apps: Arc<AppIndex>,
    pub supervisor: Arc<Supervisor<R>>,
    pub media: MediaHub,
    pub bridges: BridgeManager,
    pub auth: Arc<navette_auth::AuthToken>,
}
```

Add `auth: Arc::clone(&self.auth)` to the `Clone` impl, and change the constructor:

```rust
pub fn new(
    apps: Arc<AppIndex>,
    supervisor: Arc<Supervisor<R>>,
    auth: Arc<navette_auth::AuthToken>,
) -> Self {
```

- [ ] **Step 4: Update all three call sites**

There are exactly three. Do not use a catch-all; update each explicitly:

- `crates/navetted/src/api.rs:518` (in `test_state`): `ApiState::new(Arc::new(apps), Arc::new(supervisor), Arc::new(AuthToken::generate()))`
- `crates/navette-viewer/tests/media_endpoint.rs:67`: same shape. This test also connects to the media socket, so it must now send the header — see Step 7.
- `crates/navetted/src/main.rs:90`: Task 4 supplies the real token.

Also add to `tests_support`:

```rust
pub(crate) fn test_router_with_token() -> (Router, Arc<AuthToken>) {
    let temp = Box::leak(Box::new(TempDir::new().unwrap()));
    let state = super::tests::test_state(temp);
    let token = Arc::clone(&state.auth);
    (router(state), token)
}
```

and make `test_router()` reuse it: `test_router_with_token().0`.

- [ ] **Step 5: Implement the authenticate layer**

In `crates/navetted/src/guard.rs`:

```rust
use std::sync::Arc;

use axum::extract::State;

use navette_auth::AuthToken;

/// Requires `Authorization: Bearer <token>` on every route.
///
/// There is no loopback exemption. An earlier design proposed one, reasoning
/// that a loopback TCP port is equivalent to a 0700 Unix socket -- it is not. A
/// 0700 socket is unreachable from a web page; a loopback port is not.
pub async fn authenticate(
    State(token): State<Arc<AuthToken>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    // A bare 401 with no detail: saying *why* it failed tells an attacker
    // whether a token was recognised at all.
    match presented {
        Some(presented) if token.matches(presented) => next.run(request).await,
        _ => (StatusCode::UNAUTHORIZED, "").into_response(),
    }
}
```

- [ ] **Step 6: Wire both layers in order**

In `api.rs`'s `router`. The last `.layer()` is the outermost, so `reject_browser_origin` must be applied *after* `authenticate` for Origin to be checked first:

```rust
pub fn router<R: ProcessRunner>(state: ApiState<R>) -> Router {
    let auth = Arc::clone(&state.auth);
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/ws", any(websocket::<R>))
        .route("/v1/sessions/{session}/media", any(media_websocket::<R>))
        .layer(axum::middleware::from_fn_with_state(auth, crate::guard::authenticate))
        .layer(axum::middleware::from_fn(crate::guard::reject_browser_origin))
        .with_state(state)
}
```

**Which test actually pins this ordering** — verified empirically by swapping the
layers and observing which tests fail:

- `rejects_any_request_carrying_an_origin_header` and `rejects_an_empty_origin_header`
  **are** the pin. They send `Origin` with **no** `Authorization`, so the correct
  order returns 403 and the swapped order returns 401.
- `origin_is_refused_before_the_token_is_checked` is **not** a pin, despite its name.
  It sends a *valid* token, and a valid token passes the auth layer either way, so
  both orderings return 403. It still asserts a real security property — a browser
  holding a stolen but valid token is refused — so keep it; just do not rely on it
  to catch a reordering.

- [ ] **Step 7: Update the existing WebSocket tests to send the header**

Every test in `api.rs` and `crates/navette-viewer/tests/media_endpoint.rs` that opens a socket now needs the header. For each `into_client_request()` site (`api.rs:595`, `:641`, `:668`, `:730`, `:785` and the viewer test), add:

```rust
request.headers_mut().insert(
    "Authorization",
    format!("Bearer {}", state.auth.render()).parse().unwrap(),
);
```

- [ ] **Step 8: Run the full gate and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo fmt --check
git add -A
git commit -m "feat(api): require a bearer token on every route, no loopback exemption"
```

---

### Task 4: Daemon startup wiring

**Files:**
- Modify: `crates/navetted/src/main.rs:15-35` (`Arguments`), `:54-90`

- [ ] **Step 1: Add the `--token-file` argument and update stale help**

In `Arguments`:

```rust
/// Override the API token file path.
#[arg(long)]
token_file: Option<PathBuf>,
```

And correct the now-false `--allow-remote` help, which claims the API is unauthenticated:

```rust
/// Acknowledge that binding the API beyond loopback exposes it to the whole
/// network, not only the tailnet. The API requires a token, but the transport
/// is plaintext.
#[arg(long)]
allow_remote: bool,
```

- [ ] **Step 2: Load the token before serving**

After the registry is opened in `main`:

```rust
let token_path = match arguments.token_file {
    Some(path) => path,
    None => navette_auth::default_token_path().context("cannot determine a token path")?,
};
let auth = Arc::new(
    navette_auth::AuthToken::load_or_create(&token_path)
        .context("failed to load the API token")?,
);
// Never log the value itself.
tracing::info!(path = %token_path.display(), "API token loaded");
```

Pass `Arc::clone(&auth)` into `ApiState::new` at `:90`.

- [ ] **Step 3: Verify by hand**

```bash
cargo run -p navetted -- --bind 127.0.0.1:9417 &
sleep 1
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9417/healthz          # expect 401
curl -s -o /dev/null -w '%{http_code}\n' -H "Origin: https://evil.example" \
     http://127.0.0.1:9417/healthz                                              # expect 403
TOKEN=$(cat "${XDG_STATE_HOME:-$HOME/.local/state}/navette/token")
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $TOKEN" \
     http://127.0.0.1:9417/healthz                                              # expect 200
stat -c '%a' "${XDG_STATE_HOME:-$HOME/.local/state}/navette/token"              # expect 600
kill %1
```

- [ ] **Step 4: Confirm the token reaches no log line**

```bash
RUST_LOG=debug cargo run -p navetted -- --bind 127.0.0.1:9417 2>&1 | tee /tmp/navetted.log &
sleep 2; kill %1
TOKEN=$(cat "${XDG_STATE_HOME:-$HOME/.local/state}/navette/token")
! grep -qF "$TOKEN" /tmp/navetted.log && echo "clean" || echo "LEAK"
```

Expected: `clean`.

- [ ] **Step 5: Commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo fmt --check
git add crates/navetted/src/main.rs
git commit -m "feat(navetted): load or create the API token at startup"
```

---

### Task 5: `navette token` and QR rendering

**Files:**
- Modify: `crates/navette-cli/src/main.rs:18-40` (`Cli`), the `Command` enum, `crates/navette-cli/Cargo.toml`

**Interfaces:**
- Produces: `pairing_uri(host: &str, port: u16, token: &str) -> String`, `resolve_advertise_host(url: &str, advertise: Option<&str>) -> Result<String>`.

- [ ] **Step 1: Add the dependency**

`crates/navette-cli/Cargo.toml`:

```toml
qrcode = { version = "0.14", default-features = false }
url = "2"
navette-auth = { path = "../navette-auth" }
```

- [ ] **Step 2: Write the failing tests**

In `crates/navette-cli/src/main.rs`'s test module:

```rust
#[test]
fn builds_a_pairing_uri() {
    assert_eq!(
        pairing_uri("tower.example.ts.net", 9417, "ABCD1234ABCD1234ABCD1234"),
        "navette://pair?host=tower.example.ts.net&port=9417&token=ABCD1234ABCD1234ABCD1234"
    );
}

#[test]
fn uses_a_specific_non_loopback_url_host_when_no_flag_is_given() {
    // Reachable by construction: the client is already talking to it.
    let host = resolve_advertise_host("ws://100.64.0.3:9417/v1/ws", None).unwrap();
    assert_eq!(host, "100.64.0.3");
}

#[test]
fn requires_the_flag_when_the_url_host_is_loopback() {
    // A QR saying 127.0.0.1 scans cleanly and then fails to connect, which
    // presents as an auth bug. Refuse rather than guess.
    let error = resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", None).unwrap_err();
    assert!(error.to_string().contains("--advertise-host"));
}

#[test]
fn the_flag_always_wins() {
    let host = resolve_advertise_host("ws://127.0.0.1:9417/v1/ws", Some("tower.ts.net")).unwrap();
    assert_eq!(host, "tower.ts.net");
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p navette-cli`
Expected: FAIL — functions not defined.

- [ ] **Step 4: Implement**

```rust
fn pairing_uri(host: &str, port: u16, token: &str) -> String {
    format!("navette://pair?host={host}&port={port}&token={token}")
}

/// The daemon cannot derive its own reachable name: the phone connects over the
/// tailnet, where that is a MagicDNS name or tailnet IP, not `uname -n`, and
/// there is no Tailscale integration to ask. So use the URL host when it is
/// already a specific non-loopback address, and otherwise refuse.
fn resolve_advertise_host(url: &str, advertise: Option<&str>) -> Result<String> {
    let host = match advertise {
        Some(host) => host.to_owned(),
        None => {
            let parsed = url::Url::parse(url).context("could not parse --url")?;
            let host = parsed.host_str().context("--url has no host")?;
            let is_loopback = host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .map(|address| address.is_loopback())
                    .unwrap_or(false);
            if is_loopback {
                bail!(
                    "--url points at {host}, which the phone cannot reach; pass --advertise-host with the name or address the phone should dial"
                );
            }
            host.to_owned()
        }
    };

    // Validated once, at the single exit, because the property that matters is
    // what reaches `pairing_uri` — not which branch produced it. Validating only
    // the explicit flag leaves the --url path open: `&` is NOT a forbidden host
    // code point in the WHATWG URL spec the `url` crate implements, so
    // `--url ws://tower&evil.example:9417/v1/ws` survives `host_str()` intact.
    //
    // Validate rather than percent-encode: `pairing_uri` interpolates this
    // straight into a query string, so `&`, `#`, `?` or `/` would yield a URI
    // the Android parser reads differently than intended. Encoding would force
    // the phone side to share a decoding convention; rejecting needs no
    // agreement between them. No real hostname or IP contains these characters
    // — brackets and colons are allowed for IPv6 literals.
    let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']');
    if host.is_empty() || !host.chars().all(legal) {
        bail!("{host:?} is not a valid hostname or address");
    }
    Ok(host)
}
```

Add the subcommand:

```rust
/// Show the API token, optionally as a QR code for phone pairing.
///
/// Local admin command: it reads the daemon's token file directly, so it
/// only works on the host running navetted.
Token {
    /// Render a QR code carrying host, port and token.
    #[arg(long)]
    qr: bool,
    /// Generate a new token, invalidating every paired client.
    #[arg(long)]
    rotate: bool,
    /// The name or address the phone should dial.
    #[arg(long)]
    advertise_host: Option<String>,
},
```

And the handler:

```rust
Command::Token { qr, rotate, advertise_host } => {
    let path = navette_auth::default_token_path().context("cannot determine a token path")?;

    // Resolve the advertised host BEFORE touching the token file. Resolving
    // after rotation means `token --rotate --qr` against a default loopback
    // --url invalidates every paired client, prints the rotation notice, and
    // only then errors on the missing --advertise-host -- leaving the operator
    // with no working pairings and no sight of the replacement token.
    let advertised = if qr {
        let parsed = url::Url::parse(&cli.url)?;
        Some((
            resolve_advertise_host(&cli.url, advertise_host.as_deref())?,
            parsed.port().unwrap_or(9417),
        ))
    } else {
        None
    };

    let token = if rotate {
        let token = navette_auth::AuthToken::rotate(&path)?;
        eprintln!("Token rotated. Every paired client must pair again.");
        eprintln!("Restart navetted for this to take effect: it holds the value in memory.");
        token
    } else {
        navette_auth::AuthToken::load_or_create(&path)?
    };

    // The token is printed BEFORE anything else that can fail. Host resolution
    // already happens above the rotate, but `QrCode::new` is fallible too, and
    // leaving it between the rotation and the print reopens the same hole one
    // call later: the operator loses every pairing and never sees the
    // replacement. The QR cannot be built earlier -- its payload contains the
    // token -- so the ordering is the fix.
    println!("{}", token.render_grouped());

    if let Some((host, port)) = advertised {
        let uri = pairing_uri(&host, port, &token.render());
        match qrcode::QrCode::new(uri.as_bytes()) {
            Ok(code) => {
                println!("{}", code.render::<qrcode::render::unicode::Dense1x2>().build());
            }
            // Degrade rather than fail: the token above is what pairing needs,
            // and the QR is a convenience for typing it.
            Err(error) => {
                eprintln!("could not render a QR code ({error}); pair with the token above");
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p navette-cli`
Expected: PASS, 4 new tests.

- [ ] **Step 6: Verify the QR scans**

Run `cargo run -p navette-cli -- token --qr --advertise-host tower.ts.net` and scan the output with any phone QR reader. Expected: it reads back the exact `navette://pair?...` URI. A QR that renders but does not scan is the failure this step exists to catch.

- [ ] **Step 7: Commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo fmt --check
git add -A
git commit -m "feat(cli): add navette token with QR pairing output"
```

---

### Task 6: CLI and viewer authenticate

**Files:**
- Modify: `crates/navette-cli/src/main.rs` (global args), `crates/navette-cli/src/lib.rs` (`Client`), `crates/navette-viewer/src/main.rs:28`, `crates/navette-viewer/src/client.rs:75`

- [ ] **Step 1: Write the failing test**

In `crates/navette-auth/src/lib.rs`'s test module. **`resolve_token` lives in
`navette-auth`, not in `navette-cli`**: the viewer needs it too and does not depend
on the CLI.

```rust
#[test]
fn resolves_the_local_token_file_for_a_loopback_url() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("token");
    let token = navette_auth::AuthToken::load_or_create(&path).unwrap();
    let resolved = resolve_token("ws://127.0.0.1:9417/v1/ws", None, Some(&path)).unwrap();
    assert_eq!(resolved, token.render());
}

#[test]
fn refuses_to_send_the_local_token_to_a_remote_daemon() {
    // Reading the local host's token and sending it to some other machine
    // would hand our credential to whatever is listening there.
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join("token");
    navette_auth::AuthToken::load_or_create(&path).unwrap();
    let error = resolve_token("ws://tower:9417/v1/ws", None, Some(&path)).unwrap_err();
    assert!(error.to_string().contains("--token"));
}

#[test]
fn an_explicit_token_is_used_for_any_url() {
    let resolved = resolve_token("ws://tower:9417/v1/ws", Some("EXPLICIT"), None).unwrap();
    assert_eq!(resolved, "EXPLICIT");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p navette-auth resolve_token`
Expected: FAIL — `resolve_token` not defined.

- [ ] **Step 3: Implement `resolve_token` in `navette-auth` and send the header**

Add `navette-auth = { path = "../navette-auth" }` to `crates/navette-viewer/Cargo.toml`
(Task 5 already added it to `navette-cli`).

```rust
pub fn resolve_token(
    url: &str,
    explicit: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> Result<String> {
    if let Some(token) = explicit {
        return Ok(token.to_owned());
    }
    let parsed = url::Url::parse(url).context("could not parse --url")?;
    // Match on `url::Host`, not on `host_str()`. For an IPv6 URL `host_str()`
    // returns the *bracketed* form `[::1]`, which `IpAddr::parse` rejects — so a
    // string-parsing check silently classifies a genuinely local `ws://[::1]:9417`
    // as remote and demands an explicit --token. `host()` hands back the parsed
    // address with no brackets to strip.
    let is_local = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !is_local {
        bail!("--url points at a remote daemon; pass --token or set NAVETTE_TOKEN (the local token file belongs to this host and must not be sent elsewhere)");
    }
    let path = match token_file {
        Some(path) => path.to_path_buf(),
        None => navette_auth::default_token_path().context("cannot determine a token path")?,
    };
    Ok(navette_auth::AuthToken::load_or_create(&path)?.render())
}
```

Add the global arg to `Cli`:

```rust
/// API token. Defaults to the local token file for a loopback --url.
#[arg(long, env = "NAVETTE_TOKEN", global = true)]
token: Option<String>,
```

In both `crates/navette-cli/src/lib.rs`'s `Client` and `crates/navette-viewer/src/client.rs`, add the header to the handshake request alongside the existing subprotocol header:

```rust
request.headers_mut().insert(
    "Authorization",
    format!("Bearer {token}").parse().context("token is not a valid header value")?,
);
```

Add `--token` / `NAVETTE_TOKEN` to the viewer's `Arguments` the same way.

- [ ] **Step 4: Run to verify pass, then end-to-end**

```bash
cargo test --workspace
cargo run -p navetted -- --bind 127.0.0.1:9417 &
sleep 1
cargo run -p navette-cli -- ls          # expect a session list, not a 401
kill %1
```

- [ ] **Step 5: Commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo fmt --check
git add -A
git commit -m "feat(clients): send the API token from the CLI and viewer"
```

---

### Task 7: Android — `Unauthorized` state and terminal 401

**Files:**
- Modify: `android/.../net/NavetteClient.kt:24-32` (`ConnectionState`), `:403` area, `android/.../net/MediaClient.kt:403-406`, `android/.../ui/session/ReconnectPolicy.kt`, `android/.../ui/NavetteApp.kt`, `android/.../ui/session/SessionOverlay.kt`

**Interfaces:**
- Produces: `ConnectionState.Unauthorized`.

**Blast radius — and a correction, because the obvious assumption is wrong here.**

An earlier draft of this plan said the compiler would find the sites that need a new
`ConnectionState` arm, because a sealed type forces exhaustive `when`. **That is not
true in this codebase.** Both consumer sites use a *subject-less* `when { ... }` with
boolean `is` checks and an existing `else`, so adding a variant produces **zero
compile errors anywhere**. Verified by grep across the whole app.

The consequence is the opposite of reassuring: there is no compiler-enforced safety
net for this sealed type at all, and a new variant silently falls into whichever
`else` it meets. For `SessionOverlay` that means a rejected pairing would display as
a generic "connection lost".

So these two sites must be updated **by hand and on purpose**:
- `android/.../ui/NavetteApp.kt`
- `android/.../ui/session/SessionOverlay.kt`

Do not add an `else` to "be safe" — an `else` is what created this situation.

- [ ] **Step 1: Write the failing tests**

In `android/app/src/test/kotlin/com/greponlabs/navette/ui/session/ReconnectPolicyTest.kt`:

```kotlin
@Test
fun `does not retry after an authentication failure`() {
    // A rotated token otherwise produces an invisible infinite reconnect loop:
    // the phone spins forever while the daemon refuses every attempt.
    assertFalse(ReconnectPolicy.shouldRetry(attempt = 0, streamEnded = false, decodeError = null, unauthorized = true))
}

@Test
fun `still retries an ordinary dropped connection`() {
    // streamEnded = false: `shouldRetry` returns `!streamEnded && ...`, so passing
    // true here would assert that a *terminated* stream retries, which contradicts
    // the rule this test is named for.
    assertTrue(ReconnectPolicy.shouldRetry(attempt = 0, streamEnded = false, decodeError = null, unauthorized = false))
}
```

In `MediaClientTest.kt`:

```kotlin
@Test
fun `a 401 handshake becomes Unauthorized, not a generic failure`() = runTest {
    val server = MockWebServer()
    server.enqueue(MockResponse().setResponseCode(401))
    server.start()
    val client = MediaClient(server.url("/v1/sessions/work/media").toString().replace("http", "ws"), token = "WRONG")
    client.open()
    val state = client.connectionState.first { it !is ConnectionState.Connecting }
    assertEquals(ConnectionState.Unauthorized, state)
    server.shutdown()
}
```

- [ ] **Step 2: Run to verify failure**

Run: `./gradlew :app:testDebugUnitTest --tests '*ReconnectPolicyTest*' --tests '*MediaClientTest*'`
Expected: FAIL to compile — `Unauthorized` and the `unauthorized` parameter do not exist.

- [ ] **Step 3: Add the variant**

In `NavetteClient.kt`:

```kotlin
sealed interface ConnectionState {
    data object Disconnected : ConnectionState

    data object Connecting : ConnectionState

    data object Connected : ConnectionState

    data class Failed(val reason: String) : ConnectionState

    /**
     * The daemon refused our token. Terminal: retrying cannot succeed until the
     * user pairs again, and retrying anyway produces a silent infinite loop
     * that looks to the user like a network problem.
     */
    data object Unauthorized : ConnectionState
}
```

- [ ] **Step 4: Add the token constructor parameter to both clients**

Task 7's tests construct `MediaClient(url, token = "...")`, so the parameter lands
here rather than in Task 9. Both `MediaClient` and `NavetteClient` take
`private val token: String` and add the header beside the existing subprotocol
header:

```kotlin
Request.Builder()
    .url(webSocketUrl)
    .addHeader("Sec-WebSocket-Protocol", MEDIA_WEBSOCKET_SUBPROTOCOL)
    .addHeader("Authorization", "Bearer $token")
    .build()
```

Every existing construction site in `SessionScreen.kt`, `AppViewModel.kt` and the
test suites must pass a token. Until Task 9 wires the store, pass a literal
placeholder at the production call sites and let Task 9 replace it.

- [ ] **Step 5: Map 401 in both clients**

In `MediaClient.kt:403` and the matching handler in `NavetteClient.kt`:

```kotlin
override fun onFailure(webSocket: WebSocket, t: Throwable, response: OkHttpResponse?) {
    _connectionState.value =
        if (response?.code == 401) {
            ConnectionState.Unauthorized
        } else {
            ConnectionState.Failed(t.message ?: "connection failed")
        }
    endStream()
}
```

`NavetteClient` has no reconnect loop to stop (`NavetteClient.kt:62` defers reconnection), so for the control socket this is purely about surfacing: the user must see "pairing rejected", not a generic connection failure they will blame on the network. **When reconnection is added there, it inherits the terminal rule.**

- [ ] **Step 6: Make 401 terminal in the policy**

`isDropped` must also count `Unauthorized` as dropped. Without that, `SessionScreen`'s
retry effect never observes the transition on a pure auth failure, so the
`shouldRetry` call below is unreachable and the overlay never appears.

In `ReconnectPolicy.kt`, add the parameter and the early return:

```kotlin
fun shouldRetry(
    attempt: Int,
    streamEnded: Boolean,
    decodeError: String?,
    unauthorized: Boolean,
): Boolean {
    if (unauthorized) return false
    // ... existing logic unchanged
}
```

Update the two call sites in `SessionScreen.kt:268` and `:278` to pass
`state.connection is ConnectionState.Unauthorized`.

- [ ] **Step 7: Update the two exhaustive `when` sites**

In `NavetteApp.kt` and `SessionOverlay.kt`, add an explicit `ConnectionState.Unauthorized ->` arm. In `SessionOverlay.kt` the copy is `"Pairing rejected — scan the QR code again"` with the re-pair action. Do not add `else`.

- [ ] **Step 8: Run to verify pass**

Run: `./gradlew :app:testDebugUnitTest`
Expected: PASS, all existing tests plus the new ones.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat(android): send the bearer token and treat a 401 as terminal"
```

---

### Task 8: Android — pairing store and URI parser

**Files:**
- Create: `android/.../net/PairingStore.kt`, `android/.../net/PairingUri.kt`
- Create: `android/app/src/test/kotlin/com/greponlabs/navette/net/PairingUriTest.kt`
- Modify: `android/gradle/libs.versions.toml`, `android/app/build.gradle.kts`

**Interfaces:**
- Produces: `data class Pairing(val host: String, val port: Int, val token: String)`, `parsePairingUri(raw: String): Pairing?`, `interface PairingStore { fun load(): Pairing?; fun save(pairing: Pairing); fun clear() }`.

- [ ] **Step 1: Write the failing parser tests**

The parser is pure, so it is a plain JVM test with no framework:

```kotlin
class PairingUriTest {
    @Test
    fun `parses a well formed pairing uri`() {
        val pairing = parsePairingUri("navette://pair?host=tower.ts.net&port=9417&token=ABCD1234ABCD1234ABCD1234")
        assertEquals(Pairing("tower.ts.net", 9417, "ABCD1234ABCD1234ABCD1234"), pairing)
    }

    @Test
    fun `rejects a uri that is not a navette pairing uri`() {
        // The scanner will happily read any QR code in the room.
        assertNull(parsePairingUri("https://example.com"))
        assertNull(parsePairingUri("navette://other?host=a&port=1&token=b"))
        assertNull(parsePairingUri("not a uri at all"))
    }

    @Test
    fun `rejects a uri missing any required field`() {
        assertNull(parsePairingUri("navette://pair?port=9417&token=ABCD1234ABCD1234ABCD1234"))
        assertNull(parsePairingUri("navette://pair?host=tower.ts.net&token=ABCD1234ABCD1234ABCD1234"))
        assertNull(parsePairingUri("navette://pair?host=tower.ts.net&port=9417"))
    }

    @Test
    fun `rejects a non numeric or out of range port`() {
        assertNull(parsePairingUri("navette://pair?host=t&port=abc&token=ABCD1234ABCD1234ABCD1234"))
        assertNull(parsePairingUri("navette://pair?host=t&port=99999&token=ABCD1234ABCD1234ABCD1234"))
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `./gradlew :app:testDebugUnitTest --tests '*PairingUriTest*'`
Expected: FAIL to compile.

- [ ] **Step 3: Implement the parser**

`PairingUri.kt` — deliberately uses no Android framework class, so it stays a plain JVM test:

```kotlin
package com.greponlabs.navette.net

import java.net.URI

data class Pairing(val host: String, val port: Int, val token: String)

/**
 * Parses the `navette://pair?host=&port=&token=` URI the daemon renders as a QR
 * code. Returns null for anything else: the scanner reads whatever QR code is
 * in front of it, so this is a validation boundary, not a convenience.
 */
fun parsePairingUri(raw: String): Pairing? {
    val uri = runCatching { URI(raw) }.getOrNull() ?: return null
    // rawAuthority/rawQuery, NOT authority/query: the getters without `raw`
    // silently percent-decode, and the Rust producer deliberately does not
    // percent-encode -- it validates the host charset instead, so neither side
    // needs a shared decoding convention. Decoding here would corrupt any
    // literal `%` the moment that charset grew.
    if (uri.scheme != "navette" || uri.rawAuthority != "pair") return null
    val pairs = (uri.rawQuery ?: return null)
        .split('&')
        .map { part ->
            val index = part.indexOf('=').takeIf { it > 0 } ?: return null
            part.substring(0, index) to part.substring(index + 1)
        }
    // A repeated key is ambiguous, and ambiguous input from a camera is input
    // we refuse rather than guess at. `.toMap()` alone would silently keep the
    // last occurrence and hand back a Pairing that looks well-formed.
    if (pairs.distinctBy { it.first }.size != pairs.size) return null
    val fields = pairs.toMap()
    val host = fields["host"]?.takeIf { it.isNotBlank() } ?: return null
    val port = fields["port"]?.toIntOrNull()?.takeIf { it in 1..65535 } ?: return null
    val token = fields["token"]?.takeIf { it.isNotBlank() } ?: return null
    return Pairing(host, port, token)
}
```

- [ ] **Step 4: Add the store behind an interface**

`PairingStore.kt`. The interface exists so consumers are unit-testable with a fake — `EncryptedSharedPreferences` needs the Android framework and cannot run in a JVM test:

```kotlin
package com.greponlabs.navette.net

import android.content.Context
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey

interface PairingStore {
    fun load(): Pairing?
    fun save(pairing: Pairing)
    fun clear()
}

/**
 * Single-slot: saving replaces whatever was paired, host and token together.
 * That is the honest behaviour for one slot, and the alternative a user might
 * expect -- accumulating hosts -- is what M4 adds and this does not.
 */
class EncryptedPairingStore(context: Context) : PairingStore {
    private val prefs by lazy {
        val key = MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build()
        EncryptedSharedPreferences.create(
            context,
            "navette_pairing",
            key,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    override fun load(): Pairing? {
        val host = prefs.getString("host", null) ?: return null
        val token = prefs.getString("token", null) ?: return null
        val port = prefs.getInt("port", -1).takeIf { it > 0 } ?: return null
        return Pairing(host, port, token)
    }

    override fun save(pairing: Pairing) {
        prefs.edit()
            .putString("host", pairing.host)
            .putInt("port", pairing.port)
            .putString("token", pairing.token)
            .apply()
    }

    override fun clear() = prefs.edit().clear().apply()
}
```

Add to `libs.versions.toml`:

```toml
security-crypto = "1.1.0-alpha06"
```
```toml
security-crypto = { module = "androidx.security:security-crypto", version.ref = "security-crypto" }
```

and `implementation(libs.security.crypto)` to `app/build.gradle.kts`.

**Note for the reviewer:** `androidx.security:security-crypto` has sat at alpha for years and is effectively in maintenance. It is the house rule (`~/.claude/rules/kotlin/security.md`) and the standard answer, and the token is rotatable, so this follows it rather than hand-rolling Keystore code. The interface means swapping it later touches one class.

- [ ] **Step 5: Run to verify pass**

Run: `./gradlew :app:testDebugUnitTest --tests '*PairingUriTest*'`
Expected: PASS, 4 tests.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(android): add the pairing URI parser and an encrypted single-slot store"
```

---

### Task 9: Android — QR scanning and pairing UI

**Files:**
- Modify: `android/.../ui/connect/ConnectScreen.kt`, `android/.../ui/AppViewModel.kt`, `android/.../net/{NavetteClient,MediaClient}.kt`, `android/gradle/libs.versions.toml`, `android/app/build.gradle.kts`

- [ ] **Step 1: Add the dependency**

```toml
play-services-code-scanner = "16.1.0"
```
```toml
code-scanner = { module = "com.google.android.gms:play-services-code-scanner", version.ref = "play-services-code-scanner" }
```

`implementation(libs.code.scanner)`.

This needs **no camera permission** — scanning runs in Play Services' own UI — so no manifest permission and no runtime permission flow. It does require Play Services, which is why manual entry stays.

- [ ] **Step 2: Replace the placeholder token with the stored one**

Task 7 already added the `token` constructor parameter and the `Authorization`
header to both clients. This step only replaces the placeholder passed at the
production construction sites with `PairingStore.load()?.token`, and routes the
host and port from the same `Pairing` rather than from the typed host field.

- [ ] **Step 3: Add a test that the header is sent**

In `MediaClientTest.kt`, using the existing `MockWebServer`:

```kotlin
@Test
fun `sends the bearer token on the handshake`() = runTest {
    val server = MockWebServer()
    server.enqueue(MockResponse().setResponseCode(101))
    server.start()
    val client = MediaClient(server.url("/v1/sessions/work/media").toString().replace("http", "ws"), token = "TESTTOKEN")
    client.open()
    val request = server.takeRequest()
    assertEquals("Bearer TESTTOKEN", request.getHeader("Authorization"))
    // The guard refuses any request carrying Origin; OkHttp must not add one.
    assertNull(request.getHeader("Origin"))
    server.shutdown()
}
```

- [ ] **Step 4: Wire the scanner into `ConnectScreen`**

Add a "Scan pairing code" button calling:

```kotlin
GmsBarcodeScanning.getClient(context)
    .startScan()
    .addOnSuccessListener { barcode ->
        val pairing = barcode.rawValue?.let(::parsePairingUri)
        if (pairing == null) onScanError("That QR code is not a navette pairing code")
        else onPaired(pairing)
    }
    .addOnFailureListener { onScanError(it.message ?: "Scan failed") }
```

Keep the existing host field and add a token field as the manual fallback, shown behind a "Enter manually" disclosure. Both paths end at the same `onPaired(Pairing)`.

- [ ] **Step 5: Run the suite**

Run: `./gradlew :app:testDebugUnitTest`
Expected: PASS.

- [ ] **Step 6: On-device verification**

No unit suite substitutes for this, and the last two branches each ended with a device-only bug:

1. Fresh install, scan the QR from `navette token --qr --advertise-host <host>`. Expect connection.
2. `navette token --rotate`, restart navetted, reopen the app. Expect "pairing rejected" **and no reconnect spinner loop** — this is the failure mode that is invisible in tests.
3. Re-scan the new QR. Expect connection.
4. Kill wifi mid-session. Expect ordinary reconnect behaviour, *not* "pairing rejected".

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(android): pair by scanning the daemon's QR code"
```

---

### Task 10: wprs allocation ceilings

**Files:**
- Modify (in the `bearyjd/wprs` fork): `src/sharding_compression.rs:141-163`, `:166-180`
- Modify: `crates/navette-bridge/Cargo.toml:15`, `crates/navetted/Cargo.toml:25`

**Context:** `wprs` is already our own fork (`https://github.com/bearyjd/wprs.git`) pinned by rev, so the patch is a commit there plus a rev bump in two manifests.

- [ ] **Step 1: Add the ceilings in the fork**

In `src/sharding_compression.rs`:

```rust
/// `uncompressed_size` arrives off the wire as a u32 (see `Framed for usize` in
/// `serialization/framing.rs`) and drives the buffer allocation in
/// `decompress_impl`, so without a ceiling a peer can demand a 4 GB allocation
/// before any content is validated.
///
/// Two values because the two message kinds are not alike: Object carries
/// metadata and clipboard transfers, RawBuffer carries framebuffers (~33 MB for
/// 4K at 32bpp).
pub const MAX_UNCOMPRESSED_OBJECT: usize = 80 * 1024 * 1024;
pub const MAX_UNCOMPRESSED_RAW_BUFFER: usize = 128 * 1024 * 1024;
```

In `streaming_framed_decompress_with`, immediately after reading `uncompressed_size` at `:154`:

```rust
if uncompressed_size > MAX_UNCOMPRESSED_OBJECT {
    bail!("object message declares {uncompressed_size} bytes, over the {MAX_UNCOMPRESSED_OBJECT} ceiling");
}
```

and the matching check with `MAX_UNCOMPRESSED_RAW_BUFFER` in `streaming_framed_decompress_to_owned` after `:175`.

- [ ] **Step 2: Add tests in the fork**

```rust
#[test]
fn refuses_an_object_declaring_more_than_the_ceiling() {
    let mut stream = std::io::Cursor::new(encode_header_declaring(MAX_UNCOMPRESSED_OBJECT + 1));
    let mut decompressor = ShardingDecompressor::new(NonZeroUsize::new(1).unwrap()).unwrap();
    let result = CompressedShards::streaming_framed_decompress_with(&mut stream, &mut decompressor, |_| Ok(()));
    assert!(result.is_err());
}

#[test]
fn accepts_an_object_at_exactly_the_ceiling() {
    // An off-by-one here would reject legitimate maximum-size traffic, which
    // would look like a mysterious intermittent disconnect.
    let mut stream = std::io::Cursor::new(encode_header_declaring(MAX_UNCOMPRESSED_OBJECT));
    let mut decompressor = ShardingDecompressor::new(NonZeroUsize::new(1).unwrap()).unwrap();
    let result = CompressedShards::streaming_framed_decompress_with(&mut stream, &mut decompressor, |_| Ok(()));
    assert!(!matches!(result, Err(e) if e.to_string().contains("ceiling")));
}
```

- [ ] **Step 3: Push the fork and bump the rev**

Commit and push to `bearyjd/wprs`, then set the new rev in **both** `crates/navette-bridge/Cargo.toml:15` and `crates/navetted/Cargo.toml:25`. They must match, or cargo builds two copies of wprs and the type mismatch is confusing.

- [ ] **Step 4: Verify**

```bash
cargo update -p wprs
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Then a live session end to end: start navetted, `navette run` an app, attach the viewer, confirm frames still arrive. The RawBuffer ceiling is on the framebuffer path, so a wrong value here shows up as a blank or broken screen rather than a test failure.

- [ ] **Step 5: Commit**

```bash
git add crates/navette-bridge/Cargo.toml crates/navetted/Cargo.toml Cargo.lock
git commit -m "fix(wprs): bound the wire-declared decompression size

uncompressed_size arrives as a u32 and drives decompress_impl's buffer
allocation, so any peer could demand 4 GB before content was validated.
This governs every object message navetted reads, not just clipboard
data. Two ceilings because Object and RawBuffer carry different traffic."
```

---

### Task 11: Documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-09-11-bulk-transport-design.md` §7, `docs/RUNBOOK.md`, `docs/CONTRIBUTING.md`, `docs/HANDOFF.md`

- [ ] **Step 1: Update the bulk transport spec**

Its §7 says API-wide auth is "tracked separately in `docs/HANDOFF.md`". That is now stale: auth exists. Rewrite that paragraph to state that the blob routes inherit the guard and the bearer requirement automatically, since they join the same router, and that the equivalence argument is no longer load-bearing.

- [ ] **Step 2: Add a pairing section to the runbook**

Cover: where the token lives and its mode; `navette token`, `--qr`, `--advertise-host`; that rotation requires a daemon restart; that every paired client must re-pair after a rotation; and that `navette token` is a local admin command.

- [ ] **Step 3: Record the outcome in HANDOFF**

Note that the CRITICAL browser-origin entry is now fixed and in which commit, and move the two hardening entries from open to resolved, leaving the reasoning in place.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "docs: pairing runbook, and close out the hardening findings"
```

---

## Self-Review

**Spec coverage.** §2 Origin → Task 2. §3 token generation, storage, presentation → Tasks 1, 4, 5. §4 verification → Task 3. §5 clients → Tasks 6, 8, 9. §6 ceilings → Task 10. §7 failure modes → Task 7. §8 testing → distributed through every task. §9 exclusions → nothing to build. The spec's "must land together" constraint is in Global Constraints.

**Two spec requirements needed their own steps rather than riding along**, and were added during review: the `--allow-remote` help text is factually wrong once auth exists (Task 4 Step 1), and `navette-viewer/tests/media_endpoint.rs` is a cross-crate integration test that breaks on the `ApiState` change (Task 3 Step 4).

**Type consistency.** `AuthToken` methods are used with the same names in Tasks 3–6. `Pairing` is produced in Task 8 and consumed in Task 9. `ConnectionState.Unauthorized` is defined in Task 7 and consumed in Task 9's tests. `resolve_advertise_host` and `resolve_token` are distinct functions with distinct jobs — the first picks a host to advertise, the second picks a token to send — and are not interchangeable despite the similar shape.

**One ordering hazard worth stating.** Task 3 locks every client out until Tasks 6 and 9 land. Between them the tree is not runnable end to end, though the test suites pass throughout. This is inherent in the spec's "must land together" constraint, not a plan defect.
