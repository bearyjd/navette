use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
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
    #[error(
        "token file {path} exists but is not a valid token; refusing to overwrite it — inspect it, or run `navette token --rotate` to replace it deliberately"
    )]
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
            .map(|chunk| {
                std::str::from_utf8(chunk)
                    .expect("alphabet is ASCII")
                    .to_owned()
            })
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

#[cfg(test)]
impl AuthToken {
    /// Test-only: builds a token from known bytes so the codec can be
    /// checked against a fixed vector, not just against itself. Kept
    /// `cfg(test)` rather than widening the public API.
    fn from_bytes(bytes: [u8; TOKEN_BYTES]) -> Self {
        Self(bytes)
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

/// Resolves the bearer token a client should send for `url`.
///
/// An explicit token (`--token` / `NAVETTE_TOKEN`) always wins. Otherwise the
/// local token file is used, but only for a loopback `url`: reading this
/// host's token and sending it to some other machine would hand our
/// credential to whatever is listening there, which may not be our daemon at
/// all.
pub fn resolve_token(
    url: &str,
    explicit: Option<&str>,
    token_file: Option<&Path>,
) -> Result<String> {
    if let Some(token) = explicit {
        return Ok(token.to_owned());
    }
    let parsed = url::Url::parse(url).context("could not parse --url")?;
    let host = parsed.host_str().unwrap_or("");
    let is_local = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    if !is_local {
        bail!(
            "--url points at a remote daemon; pass --token or set NAVETTE_TOKEN (the local token file belongs to this host and must not be sent elsewhere)"
        );
    }
    let path = match token_file {
        Some(path) => path.to_path_buf(),
        None => default_token_path().context("cannot determine a token path")?,
    };
    Ok(AuthToken::load_or_create(&path)?.render())
}

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
            assert!(
                !rendered.contains(excluded),
                "{excluded} is ambiguous and excluded"
            );
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
        assert!(
            !debug.contains(&token.render()),
            "Debug leaked the token: {debug}"
        );
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn load_or_create_writes_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("token");
        let token = AuthToken::load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "token file must not be readable by others"
        );
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

    #[test]
    fn rotate_preserves_private_permissions() {
        // `rotate` writes to a staging file and renames it over the target.
        // `rename` carries the source file's mode, but that's the property
        // we depend on for the token to stay unreadable by others, so pin
        // it rather than assume it.
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("token");
        AuthToken::load_or_create(&path).unwrap();
        AuthToken::rotate(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "rotated token file must still be private after the rename"
        );
    }

    #[test]
    fn renders_a_known_byte_sequence_to_the_expected_string() {
        // The round-trip tests above only check self-consistency: a codec
        // that is wrong the same way in both `render` and `parse` (e.g.
        // consistently reversed bit order within each 5-bit group) would
        // still pass them. This pins `render` against a value computed
        // independently of the implementation, so that class of bug fails
        // loudly instead of producing tokens that intermittently fail to
        // authenticate.
        let token = AuthToken::from_bytes([
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E,
        ]);
        assert_eq!(token.render(), "000G40R40M30E209185GR38E");
    }

    #[test]
    fn resolves_the_local_token_file_for_a_loopback_url() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("token");
        let token = AuthToken::load_or_create(&path).unwrap();
        let resolved = resolve_token("ws://127.0.0.1:9417/v1/ws", None, Some(&path)).unwrap();
        assert_eq!(resolved, token.render());
    }

    #[test]
    fn refuses_to_send_the_local_token_to_a_remote_daemon() {
        // Reading the local host's token and sending it to some other machine
        // would hand our credential to whatever is listening there.
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("token");
        AuthToken::load_or_create(&path).unwrap();
        let error = resolve_token("ws://tower:9417/v1/ws", None, Some(&path)).unwrap_err();
        assert!(error.to_string().contains("--token"));
    }

    #[test]
    fn an_explicit_token_is_used_for_any_url() {
        let resolved = resolve_token("ws://tower:9417/v1/ws", Some("EXPLICIT"), None).unwrap();
        assert_eq!(resolved, "EXPLICIT");
    }
}
