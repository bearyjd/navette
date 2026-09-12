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

    pub fn rotate(path: &Path) -> Result<Self, AuthError> {
        let token = Self::generate();
        let _ = fs::remove_file(path);
        token.write_private(path)?;
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
}
