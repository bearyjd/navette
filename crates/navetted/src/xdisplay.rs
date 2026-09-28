//! X11 display numbers for sessions.
//!
//! wprsd starts `xwayland-xdg-shell`, whose Xwayland listens on display `:100`
//! and a Wayland socket named `xwayland-xdg-shell-0` unless told otherwise.
//! Every session got those defaults, so every session after the first failed
//! to start Xwayland (`AddrInUse: Could not find a free socket for the
//! XServer`), and no session's app was pointed at its own display at all: the
//! guest inherited navetted's `DISPLAY`, so an X11 app launched from the phone
//! opened on the host's own screen. Each session now claims a free display
//! number here, passes it to wprsd, and gives the app `DISPLAY=:<n>`.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Well above the displays a desktop or a stock `Xvfb` picks, and where
/// xwayland-xdg-shell's own default already lived.
const DISPLAYS: RangeInclusive<u32> = 100..=999;

/// How long a claim holds a number that has no lock file yet. Once Xwayland is
/// up its own `/tmp/.X<n>-lock` protects the number, so a claim only has to
/// cover the few seconds between choosing it and Xwayland taking it.
const CLAIM_TTL: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub struct XDisplays {
    x11_unix_dir: PathBuf,
    lock_dir: PathBuf,
    range: RangeInclusive<u32>,
    claim_ttl: Duration,
    claims: Mutex<BTreeMap<u32, Instant>>,
}

impl Default for XDisplays {
    fn default() -> Self {
        Self::with_dirs("/tmp/.X11-unix", "/tmp")
    }
}

impl XDisplays {
    /// An allocator that treats `<x11_unix_dir>/X<n>` and `<lock_dir>/.X<n>-lock`
    /// as "display `n` is taken", the same two files an X server creates.
    pub fn with_dirs(x11_unix_dir: impl Into<PathBuf>, lock_dir: impl Into<PathBuf>) -> Self {
        Self {
            x11_unix_dir: x11_unix_dir.into(),
            lock_dir: lock_dir.into(),
            range: DISPLAYS,
            claim_ttl: CLAIM_TTL,
            claims: Mutex::new(BTreeMap::new()),
        }
    }

    #[cfg(test)]
    fn with_range(mut self, range: RangeInclusive<u32>) -> Self {
        self.range = range;
        self
    }

    #[cfg(test)]
    fn with_claim_ttl(mut self, ttl: Duration) -> Self {
        self.claim_ttl = ttl;
        self
    }

    /// The lowest display number that no X server holds and no recent claim
    /// reserved, reserved for the caller; `None` when the range is exhausted.
    pub fn claim(&self) -> Option<u32> {
        let mut claims = self
            .claims
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        claims.retain(|_, claimed_at| now.duration_since(*claimed_at) < self.claim_ttl);
        let free = self
            .range
            .clone()
            .find(|n| !claims.contains_key(n) && !self.in_use(*n))?;
        claims.insert(free, now);
        Some(free)
    }

    fn in_use(&self, n: u32) -> bool {
        self.x11_unix_dir.join(format!("X{n}")).exists()
            || self.lock_dir.join(format!(".X{n}-lock")).exists()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn displays(temp: &TempDir) -> XDisplays {
        let sockets = temp.path().join(".X11-unix");
        fs::create_dir_all(&sockets).unwrap();
        XDisplays::with_dirs(sockets, temp.path())
    }

    #[test]
    fn claims_the_first_free_display() {
        let temp = TempDir::new().unwrap();
        assert_eq!(displays(&temp).claim(), Some(100));
    }

    #[test]
    fn skips_a_display_with_a_live_socket_or_a_lock_file() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp);
        fs::write(temp.path().join(".X11-unix/X100"), b"").unwrap();
        fs::write(temp.path().join(".X101-lock"), b"").unwrap();
        assert_eq!(allocator.claim(), Some(102));
    }

    #[test]
    fn a_claim_reserves_its_number_before_any_server_takes_it() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp);
        assert_eq!(allocator.claim(), Some(100));
        assert_eq!(allocator.claim(), Some(101));
    }

    #[test]
    fn an_expired_claim_frees_its_number() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp).with_claim_ttl(Duration::ZERO);
        assert_eq!(allocator.claim(), Some(100));
        assert_eq!(allocator.claim(), Some(100));
    }

    #[test]
    fn an_exhausted_range_claims_nothing() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp).with_range(100..=101);
        fs::write(temp.path().join(".X11-unix/X100"), b"").unwrap();
        assert_eq!(allocator.claim(), Some(101));
        assert_eq!(allocator.claim(), None);
    }
}
