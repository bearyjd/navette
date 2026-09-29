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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::RangeInclusive;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal;
use nix::unistd::{Pid, geteuid};

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
    pub(crate) fn with_claim_ttl_for_test(self, ttl: Duration) -> Self {
        self.with_claim_ttl(ttl)
    }

    #[cfg(test)]
    fn with_claim_ttl(mut self, ttl: Duration) -> Self {
        self.claim_ttl = ttl;
        self
    }

    /// The lowest display number that no X server holds, no recent claim
    /// reserved and no live session recorded in [taken], reserved for the
    /// caller; `None` when the range is exhausted.
    pub fn claim(&self, taken: &BTreeSet<u32>) -> Option<u32> {
        let mut claims = self
            .claims
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        claims.retain(|_, claimed_at| now.duration_since(*claimed_at) < self.claim_ttl);
        let free = self
            .range
            .clone()
            .find(|n| !claims.contains_key(n) && !taken.contains(n) && !self.in_use(*n))?;
        claims.insert(free, now);
        Some(free)
    }

    /// Whether display `n` is served by a server running as us: a real
    /// listening socket and a regular lock file, both ours, neither a symlink.
    /// Only then may a guest be told `DISPLAY=:n` -- any other state means the
    /// number could belong to someone else.
    pub fn owned(&self, n: u32) -> bool {
        let uid = geteuid().as_raw();
        let ours = |path: &Path, is_kind: fn(&fs::FileType) -> bool| {
            fs::symlink_metadata(path)
                .is_ok_and(|meta| is_kind(&meta.file_type()) && meta.uid() == uid)
        };
        ours(&self.socket(n), |kind| kind.is_socket()) && ours(&self.lock(n), |kind| kind.is_file())
    }

    /// Any entry at the socket or lock path -- a dangling symlink included --
    /// takes the number, except our own lock left by a server that is gone,
    /// which smithay reclaims itself and so may we.
    fn in_use(&self, n: u32) -> bool {
        let present = |path: &Path| fs::symlink_metadata(path).is_ok();
        if !present(&self.socket(n)) && !present(&self.lock(n)) {
            return false;
        }
        !self.stale_and_ours(n)
    }

    fn stale_and_ours(&self, n: u32) -> bool {
        let uid = geteuid().as_raw();
        let ours_or_absent = |path: &Path| match fs::symlink_metadata(path) {
            Ok(meta) => meta.uid() == uid && !meta.file_type().is_symlink(),
            Err(_) => true,
        };
        let Ok(lock) = fs::symlink_metadata(self.lock(n)) else {
            return false;
        };
        if !lock.file_type().is_file() || lock.uid() != uid || !ours_or_absent(&self.socket(n)) {
            return false;
        }
        let pid = fs::read_to_string(self.lock(n))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok());
        pid.is_some_and(|pid| signal::kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH))
    }

    fn socket(&self, n: u32) -> PathBuf {
        self.x11_unix_dir.join(format!("X{n}"))
    }

    fn lock(&self, n: u32) -> PathBuf {
        self.lock_dir.join(format!(".X{n}-lock"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;

    use tempfile::TempDir;

    use super::*;

    fn displays(temp: &TempDir) -> XDisplays {
        let sockets = temp.path().join(".X11-unix");
        fs::create_dir_all(&sockets).unwrap();
        XDisplays::with_dirs(sockets, temp.path())
    }

    fn none() -> BTreeSet<u32> {
        BTreeSet::new()
    }

    /// What a running X server on display `n` leaves behind, owned by us.
    fn serve(temp: &TempDir, n: u32, pid: u32) {
        drop(UnixListener::bind(temp.path().join(format!(".X11-unix/X{n}"))).unwrap());
        fs::write(
            temp.path().join(format!(".X{n}-lock")),
            format!("{pid:>10}\n"),
        )
        .unwrap();
    }

    #[test]
    fn claims_the_first_free_display() {
        let temp = TempDir::new().unwrap();
        assert_eq!(displays(&temp).claim(&none()), Some(100));
    }

    #[test]
    fn skips_a_display_with_a_live_socket_or_a_lock_file() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp);
        fs::write(temp.path().join(".X11-unix/X100"), b"").unwrap();
        fs::write(
            temp.path().join(".X101-lock"),
            format!("{:>10}\n", std::process::id()),
        )
        .unwrap();
        assert_eq!(allocator.claim(&none()), Some(102));
    }

    #[test]
    fn a_dangling_symlink_is_taken_not_free() {
        // exists() follows the link and says "free"; another user could plant
        // one to steer the next session onto a display they then serve.
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp);
        symlink(temp.path().join("nowhere"), temp.path().join(".X100-lock")).unwrap();
        symlink(
            temp.path().join("nowhere"),
            temp.path().join(".X11-unix/X101"),
        )
        .unwrap();
        assert_eq!(allocator.claim(&none()), Some(102));
    }

    #[test]
    fn our_own_stale_lock_from_a_dead_server_is_reclaimed() {
        // A SIGKILLed Xwayland leaves its lock; smithay reclaims such a lock
        // itself, so skipping it forever would only leak display numbers.
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp);
        serve(&temp, 100, i32::MAX as u32);
        assert_eq!(allocator.claim(&none()), Some(100));
    }

    #[test]
    fn a_number_a_live_session_recorded_is_taken_even_without_a_server() {
        let temp = TempDir::new().unwrap();
        assert_eq!(
            displays(&temp).claim(&BTreeSet::from([100, 101])),
            Some(102)
        );
    }

    #[test]
    fn a_claim_reserves_its_number_before_any_server_takes_it() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp);
        assert_eq!(allocator.claim(&none()), Some(100));
        assert_eq!(allocator.claim(&none()), Some(101));
    }

    #[test]
    fn an_expired_claim_frees_its_number() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp).with_claim_ttl(Duration::ZERO);
        assert_eq!(allocator.claim(&none()), Some(100));
        assert_eq!(allocator.claim(&none()), Some(100));
    }

    #[test]
    fn an_exhausted_range_claims_nothing() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp).with_range(100..=101);
        fs::write(temp.path().join(".X11-unix/X100"), b"").unwrap();
        assert_eq!(allocator.claim(&none()), Some(101));
        assert_eq!(allocator.claim(&none()), None);
    }

    #[test]
    fn a_display_is_owned_only_by_our_live_socket_and_lock() {
        let temp = TempDir::new().unwrap();
        let allocator = displays(&temp);
        assert!(!allocator.owned(100), "nothing there yet");
        fs::write(temp.path().join(".X11-unix/X100"), b"").unwrap();
        fs::write(temp.path().join(".X100-lock"), b"").unwrap();
        assert!(
            !allocator.owned(100),
            "a regular file is not a listening socket"
        );
        serve(&temp, 101, std::process::id());
        assert!(allocator.owned(101));
        symlink(
            temp.path().join(".X11-unix/X101"),
            temp.path().join(".X11-unix/X102"),
        )
        .unwrap();
        fs::write(temp.path().join(".X102-lock"), b"").unwrap();
        assert!(
            !allocator.owned(102),
            "a symlink to a socket is not the socket"
        );
    }
}
