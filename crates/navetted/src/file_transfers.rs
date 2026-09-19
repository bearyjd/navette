//! Session-scoped, one-way file delivery from an authenticated client to a
//! guest. Client input never chooses a destination path: it supplies metadata
//! for a daemon-generated transfer id, and materialization owns every path.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use nix::fcntl::{OFlag, open, openat, renameat};
#[cfg(unix)]
use nix::sys::stat::{Mode, SFlag, fchmod, fstat, mkdirat};
#[cfg(unix)]
use nix::unistd::{UnlinkatFlags, unlinkat};

use navette_protocol::media::{BLOB_ID_LEN, MAX_BLOB_BYTES, is_valid_mime};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::registry::validate_session_name;

pub const MAX_FILE_BYTES: u64 = MAX_BLOB_BYTES as u64;
pub const MAX_SESSION_FILE_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_SESSION_FILE_OBJECTS: u64 = 64;
/// Terminal transfers remain queryable briefly so a client can observe its
/// cancellation/failure, but must not turn repeated preflight attempts into
/// an unbounded in-memory registry.
pub const MAX_TERMINAL_TRANSFERS_PER_SESSION: usize = 64;
pub const TRANSFER_EXPIRY: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Deserialize)]
pub struct FilePreflight {
    pub name: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct FileTransferStatus {
    pub transfer_id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub bytes_received: u64,
    pub state: FileTransferState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferState {
    AwaitingUpload,
    Queued,
    Materializing,
    Delivered,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FilePreflightResponse {
    pub transfer_id: String,
    pub upload_url: String,
    pub expires_at: u64,
}

/// Opaque proof that a request observed one particular live session
/// incarnation. It is intentionally not serializable or forgeable by callers.
#[derive(Clone, Debug)]
pub struct FileSessionLease {
    session: String,
    epoch: u64,
}

#[derive(Clone, Debug)]
pub struct FileTransferStore {
    root: Arc<PathBuf>,
    entries: Arc<Mutex<Entries>>,
    lifecycle: Arc<Mutex<FileLifecycle>>,
    max_file_bytes: u64,
    max_session_bytes: u64,
    max_session_objects: u64,
    expiry: Duration,
}

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    mime: String,
    size: u64,
    bytes_received: u64,
    state: FileTransferState,
    expires_at: SystemTime,
    uploading: bool,
    terminal_at: Option<SystemTime>,
    epoch: u64,
}

type Entries = HashMap<(String, String), Entry>;

#[derive(Debug, Default)]
struct FileLifecycle {
    next_epoch: u64,
    live: HashMap<String, u64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Usage {
    bytes: u64,
    objects: u64,
}

#[derive(Debug, Error)]
pub enum FileTransferError {
    #[error("invalid session")]
    InvalidSession,
    #[error("invalid file name")]
    InvalidName,
    #[error("invalid MIME type")]
    InvalidMime,
    #[error("file exceeds the per-file limit")]
    FileTooLarge,
    #[error("session file budget is exhausted")]
    SessionBudgetExceeded,
    #[error("transfer not found")]
    NotFound,
    #[error("session is not live")]
    SessionNotLive,
    #[error("transfer is already being uploaded")]
    UploadInProgress,
    #[error("transfer is not awaiting an upload")]
    InvalidState,
    #[error("transfer cannot be cancelled")]
    NotCancellable,
    #[error("upload length does not match its preflight")]
    SizeMismatch,
    #[error("file transfer I/O failed: {0}")]
    Io(#[from] io::Error),
}

impl FileTransferStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
            entries: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(FileLifecycle::default())),
            max_file_bytes: MAX_FILE_BYTES,
            max_session_bytes: MAX_SESSION_FILE_BYTES,
            max_session_objects: MAX_SESSION_FILE_OBJECTS,
            expiry: TRANSFER_EXPIRY,
        }
    }

    #[cfg(test)]
    fn with_limits(
        root: impl Into<PathBuf>,
        max_file_bytes: u64,
        max_session_bytes: u64,
        max_session_objects: u64,
        expiry: Duration,
    ) -> Self {
        Self {
            root: Arc::new(root.into()),
            entries: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(FileLifecycle::default())),
            max_file_bytes,
            max_session_bytes,
            max_session_objects,
            expiry,
        }
    }

    /// Test-only shortcut for the production sequence of a supervisor drop
    /// directory reset followed by [`Self::activate_prepared`]. Production
    /// never clears the drop directory from here: that must happen before
    /// the guest process is spawned, which only the supervisor can order.
    #[cfg(test)]
    pub fn activate(&self, session: &str) -> Result<(), FileTransferError> {
        validate(session)?;
        let mut lifecycle = self.lock_lifecycle()?;
        let epoch = next_epoch(&mut lifecycle);
        lifecycle.live.insert(session.to_owned(), epoch);
        let mut entries = self.lock_entries()?;
        entries.retain(|(entry_session, _), _| entry_session != session);
        drop(entries);
        let directory = self.drop_dir(session);
        remove_tree(&directory)?;
        remove_tree(&self.staging_dir(session))?;
        fs::create_dir_all(&directory)?;
        set_private_directory(&directory)?;
        drop(lifecycle);
        Ok(())
    }

    /// Records a new session incarnation after `Supervisor::start` has
    /// already reset the guest-visible directory. Keeping that reset before
    /// process spawn prevents a newly launched guest from observing old files
    /// (or losing files it creates in the small interval after spawn).
    pub fn activate_prepared(&self, session: &str) -> Result<(), FileTransferError> {
        validate(session)?;
        let mut lifecycle = self.lock_lifecycle()?;
        let epoch = next_epoch(&mut lifecycle);
        lifecycle.live.insert(session.to_owned(), epoch);
        let mut entries = self.lock_entries()?;
        entries.retain(|(entry_session, _), _| entry_session != session);
        drop(entries);
        remove_tree(&self.staging_dir(session))?;
        drop(lifecycle);
        Ok(())
    }

    /// Recreates only the directory needed for a session recovered after the
    /// daemon restarts. Delivered files belong to the still-running guest and
    /// must remain available; transfer metadata itself is intentionally
    /// in-memory and is not resurrected.
    pub fn recover(&self, session: &str) -> Result<(), FileTransferError> {
        validate(session)?;
        let mut lifecycle = self.lock_lifecycle()?;
        let epoch = next_epoch(&mut lifecycle);
        lifecycle.live.insert(session.to_owned(), epoch);
        let mut entries = self.lock_entries()?;
        entries.retain(|(entry_session, _), _| entry_session != session);
        drop(entries);
        let directory = self.drop_dir(session);
        fs::create_dir_all(&directory)?;
        set_private_directory(&directory)?;
        // The in-memory transfer registry is lost across a daemon restart, so
        // no partial upload can be resumed safely. Keep public delivered files
        // for the live guest, but discard their private staging siblings.
        remove_tree(&self.staging_dir(session))?;
        drop(lifecycle);
        Ok(())
    }

    pub fn deactivate(&self, session: &str) -> Result<(), FileTransferError> {
        validate(session)?;
        let mut lifecycle = self.lock_lifecycle()?;
        lifecycle.live.remove(session);
        let mut entries = self.lock_entries()?;
        entries.retain(|(entry_session, _), _| entry_session != session);
        drop(entries);
        remove_tree(&self.drop_dir(session))?;
        remove_tree(&self.staging_dir(session))?;
        drop(lifecycle);
        Ok(())
    }

    /// The only directory exposed to the guest via `NAVETTE_DROP_DIR`.
    pub fn drop_dir(&self, session: &str) -> PathBuf {
        self.root.join(session)
    }

    pub fn preflight(
        &self,
        session: &str,
        request: FilePreflight,
    ) -> Result<FilePreflightResponse, FileTransferError> {
        let lease = self.lease(session)?;
        self.preflight_with_lease(&lease, request)
    }

    /// Captures the live session incarnation before a caller samples related
    /// lifecycle state (such as the supervisor registry).
    pub fn lease(&self, session: &str) -> Result<FileSessionLease, FileTransferError> {
        validate(session)?;
        let lifecycle = self.lock_lifecycle()?;
        Ok(FileSessionLease {
            session: session.to_owned(),
            epoch: live_epoch(&lifecycle, session)?,
        })
    }

    /// Creates a reservation only if `lease` still names the exact session
    /// incarnation that produced the handler's liveness observation.
    pub fn preflight_with_lease(
        &self,
        lease: &FileSessionLease,
        request: FilePreflight,
    ) -> Result<FilePreflightResponse, FileTransferError> {
        let session = &lease.session;
        validate_name(&request.name)?;
        if !is_valid_mime(&request.mime) {
            return Err(FileTransferError::InvalidMime);
        }
        if request.size == 0 || request.size > self.max_file_bytes {
            return Err(FileTransferError::FileTooLarge);
        }
        let lifecycle = self.lock_lifecycle()?;
        if live_epoch(&lifecycle, session)? != lease.epoch {
            return Err(FileTransferError::SessionNotLive);
        }
        let mut entries = self.lock_entries()?;
        self.purge_expired_locked(&mut entries)?;
        let durable = durable_usage(&self.drop_dir(session))?;
        let reserved = reservation_usage(&entries, session);
        if durable
            .bytes
            .checked_add(reserved.bytes)
            .and_then(|total| total.checked_add(request.size))
            .is_none_or(|total| total > self.max_session_bytes)
            || durable
                .objects
                .checked_add(reserved.objects)
                .is_none_or(|total| total >= self.max_session_objects)
        {
            return Err(FileTransferError::SessionBudgetExceeded);
        }
        let id = self.new_id_locked(&entries)?;
        let expires_at = SystemTime::now() + self.expiry;
        entries.insert(
            (session.to_owned(), id.clone()),
            Entry {
                name: request.name,
                mime: request.mime,
                size: request.size,
                bytes_received: 0,
                state: FileTransferState::AwaitingUpload,
                expires_at,
                uploading: false,
                terminal_at: None,
                epoch: lease.epoch,
            },
        );
        let response = FilePreflightResponse {
            upload_url: format!("/v1/sessions/{session}/files/{id}/content"),
            transfer_id: id,
            expires_at: timestamp_ms(expires_at),
        };
        drop(entries);
        drop(lifecycle);
        Ok(response)
    }

    pub fn begin_upload(&self, session: &str, id: &str) -> Result<FileUpload, FileTransferError> {
        validate(session)?;
        validate_id(id)?;
        let lifecycle = self.lock_lifecycle()?;
        let epoch = live_epoch(&lifecycle, session)?;
        let mut entries = self.lock_entries()?;
        self.purge_expired_locked(&mut entries)?;
        let entry = entries
            .get_mut(&(session.to_owned(), id.to_owned()))
            .ok_or(FileTransferError::NotFound)?;
        if entry.epoch != epoch {
            return Err(FileTransferError::NotFound);
        }
        if entry.state != FileTransferState::AwaitingUpload {
            return Err(FileTransferError::InvalidState);
        }
        if entry.uploading {
            return Err(FileTransferError::UploadInProgress);
        }
        entry.uploading = true;
        let directory = self.staging_dir(session);
        if let Err(error) =
            fs::create_dir_all(&directory).and_then(|_| set_private_directory(&directory))
        {
            entry.uploading = false;
            return Err(FileTransferError::Io(error));
        }
        let path = directory.join(format!("{id}.part"));
        let file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) => {
                entry.uploading = false;
                return Err(FileTransferError::Io(error));
            }
        };
        let expected = entry.size;
        let upload = FileUpload {
            store: self.clone(),
            session: session.to_owned(),
            id: id.to_owned(),
            path,
            file: Some(file),
            written: 0,
            expected,
            epoch,
            finished: false,
        };
        drop(entries);
        drop(lifecycle);
        Ok(upload)
    }

    pub fn status(&self, session: &str, id: &str) -> Result<FileTransferStatus, FileTransferError> {
        validate(session)?;
        validate_id(id)?;
        let lifecycle = self.lock_lifecycle()?;
        let epoch = live_epoch(&lifecycle, session)?;
        let mut entries = self.lock_entries()?;
        self.purge_expired_locked(&mut entries)?;
        let entry = entries
            .get(&(session.to_owned(), id.to_owned()))
            .ok_or(FileTransferError::NotFound)?;
        if entry.epoch != epoch {
            return Err(FileTransferError::NotFound);
        }
        Ok(status(id, entry))
    }

    pub fn cancel(&self, session: &str, id: &str) -> Result<(), FileTransferError> {
        validate(session)?;
        validate_id(id)?;
        let lifecycle = self.lock_lifecycle()?;
        let epoch = live_epoch(&lifecycle, session)?;
        let mut entries = self.lock_entries()?;
        self.purge_expired_locked(&mut entries)?;
        let entry = entries
            .get_mut(&(session.to_owned(), id.to_owned()))
            .ok_or(FileTransferError::NotFound)?;
        if entry.epoch != epoch {
            return Err(FileTransferError::NotFound);
        }
        if !matches!(
            entry.state,
            FileTransferState::AwaitingUpload | FileTransferState::Queued
        ) || entry.uploading
        {
            return Err(FileTransferError::NotCancellable);
        }
        entry.state = FileTransferState::Cancelled;
        entry.uploading = false;
        entry.terminal_at = Some(SystemTime::now());
        self.prune_terminal_locked(&mut entries, session, Some(id));
        drop(entries);
        let result = remove_file(&self.staging_path(session, id));
        drop(lifecycle);
        result?;
        Ok(())
    }

    /// Runs outside the bridge render loop. The only client-derived filename
    /// here passed `validate_name`, and every parent is daemon-owned.
    pub fn materialize(
        &self,
        session: &str,
        id: &str,
    ) -> Result<FileTransferStatus, FileTransferError> {
        validate(session)?;
        validate_id(id)?;
        // Hold only across the short daemon-owned rename, never during HTTP
        // body streaming. This makes the transition atomic with deactivate /
        // activate for a reused session name.
        let lifecycle = self.lock_lifecycle()?;
        let epoch = live_epoch(&lifecycle, session)?;
        let (name, expected) = {
            let mut entries = self.lock_entries()?;
            let entry = entries
                .get_mut(&(session.to_owned(), id.to_owned()))
                .ok_or(FileTransferError::NotFound)?;
            if entry.epoch != epoch {
                return Err(FileTransferError::NotFound);
            }
            if entry.state != FileTransferState::Queued {
                return Err(FileTransferError::InvalidState);
            }
            entry.state = FileTransferState::Materializing;
            (entry.name.clone(), entry.size)
        };
        let outcome = deliver_staged_file(
            &self.drop_dir(session),
            &self.staging_dir(session),
            id,
            &name,
            expected,
        );
        let mut entries = self.lock_entries()?;
        {
            let entry = entries
                .get_mut(&(session.to_owned(), id.to_owned()))
                .ok_or(FileTransferError::NotFound)?;
            if entry.epoch != epoch {
                return Err(FileTransferError::NotFound);
            }
            entry.state = if outcome.is_ok() {
                FileTransferState::Delivered
            } else {
                entry.terminal_at = Some(SystemTime::now());
                FileTransferState::Failed
            };
        }
        self.prune_terminal_locked(&mut entries, session, Some(id));
        let result = entries
            .get(&(session.to_owned(), id.to_owned()))
            .map(|entry| status(id, entry))
            .ok_or(FileTransferError::NotFound)?;
        drop(entries);
        if outcome.is_err() {
            let _ = remove_file(&self.staging_path(session, id));
        }
        drop(lifecycle);
        outcome.map_err(FileTransferError::Io)?;
        Ok(result)
    }

    /// Marks a queued transfer terminal when its materialization worker could
    /// not even be launched. This frees the in-memory reservation immediately
    /// instead of leaving a permanently queued transfer after a 202 response.
    pub fn fail_queued_materialization(
        &self,
        session: &str,
        id: &str,
    ) -> Result<(), FileTransferError> {
        validate(session)?;
        validate_id(id)?;
        let lifecycle = self.lock_lifecycle()?;
        let epoch = live_epoch(&lifecycle, session)?;
        let mut entries = self.lock_entries()?;
        let entry = entries
            .get_mut(&(session.to_owned(), id.to_owned()))
            .ok_or(FileTransferError::NotFound)?;
        if entry.epoch != epoch {
            return Err(FileTransferError::NotFound);
        }
        if entry.state != FileTransferState::Queued {
            return Err(FileTransferError::InvalidState);
        }
        entry.state = FileTransferState::Failed;
        entry.terminal_at = Some(SystemTime::now());
        self.prune_terminal_locked(&mut entries, session, Some(id));
        drop(entries);
        let result = remove_file(&self.staging_path(session, id));
        drop(lifecycle);
        result?;
        Ok(())
    }

    fn complete_upload(
        &self,
        session: &str,
        id: &str,
        epoch: u64,
        written: u64,
    ) -> Result<FileTransferStatus, FileTransferError> {
        let lifecycle = self.lock_lifecycle()?;
        if live_epoch(&lifecycle, session)? != epoch {
            return Err(FileTransferError::SessionNotLive);
        }
        let mut entries = self.lock_entries()?;
        let entry = entries
            .get_mut(&(session.to_owned(), id.to_owned()))
            .ok_or(FileTransferError::NotFound)?;
        if entry.epoch != epoch {
            return Err(FileTransferError::NotFound);
        }
        entry.uploading = false;
        if written != entry.size {
            entry.state = FileTransferState::Failed;
            entry.terminal_at = Some(SystemTime::now());
            self.prune_terminal_locked(&mut entries, session, Some(id));
            return Err(FileTransferError::SizeMismatch);
        }
        entry.bytes_received = written;
        entry.state = FileTransferState::Queued;
        let status = status(id, entry);
        drop(entries);
        drop(lifecycle);
        Ok(status)
    }

    fn abort_upload(&self, session: &str, id: &str, epoch: u64, failed: bool) {
        if let Ok(lifecycle) = self.lock_lifecycle()
            && live_epoch(&lifecycle, session).is_ok_and(|current| current == epoch)
            && let Ok(mut entries) = self.lock_entries()
            && let Some(entry) = entries.get_mut(&(session.to_owned(), id.to_owned()))
            && entry.epoch == epoch
            && entry.state == FileTransferState::AwaitingUpload
        {
            entry.uploading = false;
            if failed {
                entry.state = FileTransferState::Failed;
                entry.terminal_at = Some(SystemTime::now());
                self.prune_terminal_locked(&mut entries, session, Some(id));
            }
        }
    }

    fn purge_expired_locked(
        &self,
        entries: &mut HashMap<(String, String), Entry>,
    ) -> Result<(), FileTransferError> {
        let now = SystemTime::now();
        let expired: Vec<_> = entries
            .iter()
            .filter(|(_, entry)| {
                entry.state == FileTransferState::AwaitingUpload
                    && !entry.uploading
                    && entry.expires_at <= now
            })
            .map(|((session, id), _)| (session.clone(), id.clone()))
            .collect();
        for (session, id) in expired {
            entries.remove(&(session.clone(), id.clone()));
            remove_file(&self.staging_path(&session, &id))?;
        }
        Ok(())
    }

    fn prune_terminal_locked(
        &self,
        entries: &mut Entries,
        session: &str,
        preserve_id: Option<&str>,
    ) {
        let mut terminal: Vec<_> = entries
            .iter()
            .filter_map(|((entry_session, id), entry)| {
                (entry_session == session)
                    .then_some(entry.terminal_at)
                    .flatten()
                    .map(|at| (id.clone(), at))
            })
            .collect();
        terminal.sort_unstable_by_key(|(_, at)| *at);
        let excess = terminal
            .len()
            .saturating_sub(MAX_TERMINAL_TRANSFERS_PER_SESSION);
        for (id, _) in terminal
            .into_iter()
            .filter(|(id, _)| Some(id.as_str()) != preserve_id)
            .take(excess)
        {
            entries.remove(&(session.to_owned(), id));
        }
    }

    fn lock_entries(&self) -> Result<std::sync::MutexGuard<'_, Entries>, FileTransferError> {
        self.entries
            .lock()
            .map_err(|_| FileTransferError::Io(io::Error::other("file transfer lock poisoned")))
    }

    fn lock_lifecycle(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, FileLifecycle>, FileTransferError> {
        self.lifecycle.lock().map_err(|_| {
            FileTransferError::Io(io::Error::other("file transfer lifecycle lock poisoned"))
        })
    }

    fn ensure_live(&self, session: &str, epoch: u64) -> Result<(), FileTransferError> {
        let lifecycle = self.lock_lifecycle()?;
        if live_epoch(&lifecycle, session)? == epoch {
            Ok(())
        } else {
            Err(FileTransferError::SessionNotLive)
        }
    }

    fn new_id_locked(
        &self,
        entries: &HashMap<(String, String), Entry>,
    ) -> Result<String, FileTransferError> {
        loop {
            let id = random_id()?;
            if !entries.keys().any(|(_, existing)| existing == &id) {
                return Ok(id);
            }
        }
    }

    fn staging_dir(&self, session: &str) -> PathBuf {
        self.root.join(".staging").join(session)
    }

    fn staging_path(&self, session: &str, id: &str) -> PathBuf {
        self.staging_dir(session).join(format!("{id}.part"))
    }
}

pub struct FileUpload {
    store: FileTransferStore,
    session: String,
    id: String,
    path: PathBuf,
    file: Option<File>,
    written: u64,
    expected: u64,
    epoch: u64,
    finished: bool,
}

impl FileUpload {
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), FileTransferError> {
        self.store.ensure_live(&self.session, self.epoch)?;
        let additional = u64::try_from(chunk.len()).map_err(|_| FileTransferError::FileTooLarge)?;
        if self
            .written
            .checked_add(additional)
            .is_none_or(|size| size > self.expected)
        {
            self.store
                .abort_upload(&self.session, &self.id, self.epoch, true);
            return Err(FileTransferError::SizeMismatch);
        }
        self.file
            .as_mut()
            .ok_or(FileTransferError::InvalidState)?
            .write_all(chunk)?;
        self.written += additional;
        Ok(())
    }

    pub fn finish(mut self) -> Result<FileTransferStatus, FileTransferError> {
        if self.written != self.expected {
            self.store
                .abort_upload(&self.session, &self.id, self.epoch, true);
            return Err(FileTransferError::SizeMismatch);
        }
        self.file
            .as_mut()
            .ok_or(FileTransferError::InvalidState)?
            .sync_all()?;
        self.file.take();
        let status =
            self.store
                .complete_upload(&self.session, &self.id, self.epoch, self.written)?;
        self.finished = true;
        Ok(status)
    }
}

impl Drop for FileUpload {
    fn drop(&mut self) {
        if !self.finished {
            self.store
                .abort_upload(&self.session, &self.id, self.epoch, false);
            let _ = remove_file(&self.path);
        }
    }
}

fn validate(session: &str) -> Result<(), FileTransferError> {
    validate_session_name(session).map_err(|_| FileTransferError::InvalidSession)
}

fn next_epoch(lifecycle: &mut FileLifecycle) -> u64 {
    lifecycle.next_epoch = lifecycle.next_epoch.wrapping_add(1).max(1);
    lifecycle.next_epoch
}

fn live_epoch(lifecycle: &FileLifecycle, session: &str) -> Result<u64, FileTransferError> {
    lifecycle
        .live
        .get(session)
        .copied()
        .ok_or(FileTransferError::SessionNotLive)
}

fn validate_name(name: &str) -> Result<(), FileTransferError> {
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        Err(FileTransferError::InvalidName)
    } else {
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), FileTransferError> {
    if id.len() == BLOB_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(FileTransferError::NotFound)
    }
}

/// Counts reservations that do not yet have a durable drop file. Delivered
/// entries are counted by `durable_usage`, avoiding a double count before or
/// after a daemon restart.
fn reservation_usage(entries: &Entries, session: &str) -> Usage {
    entries
        .iter()
        .filter(|((entry_session, _), entry)| {
            entry_session == session
                && !matches!(
                    entry.state,
                    FileTransferState::Delivered
                        | FileTransferState::Failed
                        | FileTransferState::Cancelled
                )
        })
        .fold(Usage::default(), |usage, (_, entry)| Usage {
            bytes: usage.bytes.saturating_add(entry.size),
            objects: usage.objects.saturating_add(1),
        })
}

/// Counts only the daemon's documented `drops/<session>/<transfer-id>/<name>`
/// shape. Symlinks, unknown ids, malformed names and nested directories are
/// never followed, so recovery cannot account files outside the drop root.
/// Allocated blocks, rather than logical length, match durable disk use.
fn durable_usage(directory: &Path) -> io::Result<Usage> {
    let mut usage = Usage::default();
    let transfers = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(usage),
        Err(error) => return Err(error),
    };
    for transfer in transfers {
        let transfer = transfer?;
        let Ok(id) = transfer.file_name().into_string() else {
            continue;
        };
        if validate_id(&id).is_err() || !transfer.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(transfer.path())? {
            let file = file?;
            let Ok(name) = file.file_name().into_string() else {
                continue;
            };
            if validate_name(&name).is_err() || !file.file_type()?.is_file() {
                continue;
            }
            usage.bytes = usage
                .bytes
                .saturating_add(allocated_bytes(&file.metadata()?));
            usage.objects = usage.objects.saturating_add(1);
        }
    }
    Ok(usage)
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}

fn status(id: &str, entry: &Entry) -> FileTransferStatus {
    FileTransferStatus {
        transfer_id: id.to_owned(),
        name: entry.name.clone(),
        mime: entry.mime.clone(),
        size: entry.size,
        bytes_received: entry.bytes_received,
        state: entry.state,
        expires_at: (entry.state == FileTransferState::AwaitingUpload)
            .then(|| timestamp_ms(entry.expires_at)),
    }
}

fn timestamp_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn random_id() -> Result<String, FileTransferError> {
    let mut bytes = [0_u8; BLOB_ID_LEN / 2];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut id = String::with_capacity(BLOB_ID_LEN);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(id)
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Validates, locks down, and delivers a staged upload entirely through
/// descriptors. The guest may write to the drop directory and can race the
/// daemon, so every check must apply to the inode that is then chmod'd and
/// moved: `openat(O_NOFOLLOW)` refuses a swapped-in symlink, `fstat` inspects
/// that same open file, and `fchmod` tightens that same open file rather
/// than whatever a path lookup would resolve to. The final `renameat` still
/// resolves `<id>.part` by name; a guest swapping that entry between open and
/// rename can only sabotage its own delivery into a fresh 0o700 directory the
/// daemon just created, so that residual window is acceptable. A fresh id
/// directory is required; a guest-created entry makes this transfer fail.
#[cfg(unix)]
fn deliver_staged_file(
    drop_dir: &Path,
    staging_dir: &Path,
    id: &str,
    name: &str,
    expected: u64,
) -> io::Result<()> {
    let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let file_flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let staging = open(staging_dir, dir_flags, Mode::empty()).map_err(nix_to_io)?;
    let staged_name = format!("{id}.part");
    let staged =
        openat(&staging, staged_name.as_str(), file_flags, Mode::empty()).map_err(nix_to_io)?;
    ensure_regular_file_of_size(&staged, expected)?;
    fchmod(&staged, Mode::from_bits_truncate(0o600)).map_err(nix_to_io)?;

    let drop = open(drop_dir, dir_flags, Mode::empty()).map_err(nix_to_io)?;
    mkdirat(&drop, id, Mode::from_bits_truncate(0o700)).map_err(nix_to_io)?;
    let delivered = openat(&drop, id, dir_flags, Mode::empty())
        .and_then(|destination| renameat(&staging, staged_name.as_str(), &destination, name));
    if delivered.is_err() {
        // The directory created above is still empty on this path, so remove
        // it by name, best effort. That name is resolved again here: if the
        // guest already replaced the entry with a symlink, a file, or a
        // populated directory, unlinkat fails and is ignored, and an empty
        // directory of theirs at that name lives inside their own drop
        // directory anyway. The delivery error is what gets reported.
        let _ = unlinkat(&drop, id, UnlinkatFlags::RemoveDir);
    }
    delivered.map_err(nix_to_io)
}

#[cfg(unix)]
fn ensure_regular_file_of_size(file: impl std::os::fd::AsFd, expected: u64) -> io::Result<()> {
    let stat = fstat(file).map_err(nix_to_io)?;
    let is_regular = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFREG;
    if !is_regular || !u64::try_from(stat.st_size).is_ok_and(|size| size == expected) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged file is invalid",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn nix_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

/// Best-effort mirror of the descriptor-based unix path. Without `openat`
/// semantics this cannot close the check-then-use window, and there is no
/// portable private mode to apply, so it validates by path and moves.
#[cfg(not(unix))]
fn deliver_staged_file(
    drop_dir: &Path,
    staging_dir: &Path,
    id: &str,
    name: &str,
    expected: u64,
) -> io::Result<()> {
    let source = staging_dir.join(format!("{id}.part"));
    let metadata = fs::symlink_metadata(&source)?;
    if !metadata.file_type().is_file() || metadata.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged file is invalid",
        ));
    }
    let destination_dir = drop_dir.join(id);
    fs::create_dir(&destination_dir)?;
    let delivered = fs::rename(&source, destination_dir.join(name));
    if delivered.is_err() {
        let _ = fs::remove_dir(&destination_dir);
    }
    delivered
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(temp: &TempDir) -> FileTransferStore {
        let store = FileTransferStore::with_limits(
            temp.path().join("drops"),
            8,
            12,
            2,
            Duration::from_millis(1),
        );
        store.activate("work").unwrap();
        store
    }

    fn request(name: &str, size: u64) -> FilePreflight {
        FilePreflight {
            name: name.into(),
            mime: "application/pdf".into(),
            size,
        }
    }

    #[test]
    fn reserves_exact_preflight_bytes_and_materializes_only_under_daemon_paths() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let preflight = store.preflight("work", request("report.pdf", 4)).unwrap();
        assert!(preflight.upload_url.ends_with("/content"));
        let mut upload = store.begin_upload("work", &preflight.transfer_id).unwrap();
        upload.write_chunk(b"data").unwrap();
        assert_eq!(upload.finish().unwrap().state, FileTransferState::Queued);
        let delivered = store.materialize("work", &preflight.transfer_id).unwrap();
        assert_eq!(delivered.state, FileTransferState::Delivered);
        assert_eq!(
            fs::read(
                store
                    .drop_dir("work")
                    .join(&preflight.transfer_id)
                    .join("report.pdf")
            )
            .unwrap(),
            b"data"
        );
    }

    #[test]
    fn rejects_paths_size_mismatches_and_over_budget_reservations() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        assert!(matches!(
            store.preflight("work", request("../escape", 1)),
            Err(FileTransferError::InvalidName)
        ));
        let first = store.preflight("work", request("one", 8)).unwrap();
        assert!(matches!(
            store.preflight("work", request("two", 5)),
            Err(FileTransferError::SessionBudgetExceeded)
        ));
        let mut upload = store.begin_upload("work", &first.transfer_id).unwrap();
        upload.write_chunk(b"short").unwrap();
        assert!(matches!(
            upload.finish(),
            Err(FileTransferError::SizeMismatch)
        ));
        assert_eq!(
            store.status("work", &first.transfer_id).unwrap().state,
            FileTransferState::Failed
        );
    }

    #[test]
    fn cancellation_and_expiry_release_a_reservation() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let first = store.preflight("work", request("one", 8)).unwrap();
        store.cancel("work", &first.transfer_id).unwrap();
        assert_eq!(
            store.status("work", &first.transfer_id).unwrap().state,
            FileTransferState::Cancelled
        );
        store.preflight("work", request("two", 8)).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let third = store.preflight("work", request("three", 8)).unwrap();
        assert!(store.status("work", &third.transfer_id).is_ok());
    }

    #[test]
    fn terminal_transfer_metadata_is_bounded_per_session() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        for number in 0..MAX_TERMINAL_TRANSFERS_PER_SESSION + 8 {
            let preflight = store
                .preflight("work", request(&format!("cancelled-{number}"), 1))
                .unwrap();
            store.cancel("work", &preflight.transfer_id).unwrap();
        }
        let entries = store.entries.lock().unwrap();
        assert_eq!(
            entries
                .values()
                .filter(|entry| entry.terminal_at.is_some())
                .count(),
            MAX_TERMINAL_TRANSFERS_PER_SESSION
        );
    }

    #[test]
    fn recovery_preserves_delivered_files_for_a_live_session() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let preflight = store.preflight("work", request("report.pdf", 4)).unwrap();
        let mut upload = store.begin_upload("work", &preflight.transfer_id).unwrap();
        upload.write_chunk(b"data").unwrap();
        upload.finish().unwrap();
        store.materialize("work", &preflight.transfer_id).unwrap();
        let path = store
            .drop_dir("work")
            .join(&preflight.transfer_id)
            .join("report.pdf");
        let staged = store.staging_dir("work").join("crashed.part");
        fs::create_dir_all(staged.parent().unwrap()).unwrap();
        fs::write(&staged, b"partial").unwrap();

        store.recover("work").unwrap();
        assert_eq!(fs::read(path).unwrap(), b"data");
        assert!(
            !staged.exists(),
            "recovery must not retain an unresumable partial upload"
        );
    }

    #[test]
    fn prepared_activation_keeps_the_supervisor_initialized_drop_directory() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("drops");
        let store = FileTransferStore::with_limits(&root, 8, 12, 2, Duration::from_secs(60));
        let drop_dir = store.drop_dir("work");
        fs::create_dir_all(&drop_dir).unwrap();
        fs::write(drop_dir.join("guest-created"), b"keep").unwrap();

        store.activate_prepared("work").unwrap();

        assert_eq!(fs::read(drop_dir.join("guest-created")).unwrap(), b"keep");
        assert!(store.preflight("work", request("report.pdf", 1)).is_ok());
    }

    #[test]
    fn recovery_accounts_for_durable_deliveries_before_accepting_new_preflights() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("drops");
        let initial =
            FileTransferStore::with_limits(&root, 8, 64 * 1024, 1, Duration::from_secs(60));
        initial.activate("work").unwrap();
        let first = initial.preflight("work", request("first.pdf", 1)).unwrap();
        let mut upload = initial.begin_upload("work", &first.transfer_id).unwrap();
        upload.write_chunk(b"x").unwrap();
        upload.finish().unwrap();
        initial.materialize("work", &first.transfer_id).unwrap();

        let recovered =
            FileTransferStore::with_limits(root, 8, 64 * 1024, 1, Duration::from_secs(60));
        recovered.recover("work").unwrap();
        assert!(matches!(
            recovered.preflight("work", request("second.pdf", 1)),
            Err(FileTransferError::SessionBudgetExceeded)
        ));
    }

    #[test]
    fn old_session_lease_cannot_upload_query_or_deliver_after_name_reuse() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);

        let uploading = store
            .preflight("work", request("uploading.pdf", 4))
            .unwrap();
        let mut upload = store.begin_upload("work", &uploading.transfer_id).unwrap();
        upload.write_chunk(b"data").unwrap();

        // Exercises the lease/epoch invariant directly through the test-only
        // `activate` shortcut, which advances the incarnation and drops the
        // old transfer registry atomically while an old HTTP request is
        // still streaming. The production Kill + Run order is covered by
        // queued_transfer_from_previous_incarnation_cannot_materialize_after_supervisor_reset.
        store.deactivate("work").unwrap();
        store.activate("work").unwrap();
        assert!(matches!(
            upload.finish(),
            Err(FileTransferError::SessionNotLive)
        ));
        assert!(matches!(
            store.status("work", &uploading.transfer_id),
            Err(FileTransferError::NotFound)
        ));

        let queued = store.preflight("work", request("queued.pdf", 4)).unwrap();
        let mut upload = store.begin_upload("work", &queued.transfer_id).unwrap();
        upload.write_chunk(b"data").unwrap();
        upload.finish().unwrap();
        store.deactivate("work").unwrap();
        store.activate("work").unwrap();
        assert!(matches!(
            store.materialize("work", &queued.transfer_id),
            Err(FileTransferError::NotFound)
        ));
        assert!(
            !store.drop_dir("work").join(&queued.transfer_id).exists(),
            "the old incarnation must not materialize into the reused drop directory"
        );
    }

    /// Drives the exact production name-reuse order: `Kill` deactivates the
    /// store, `Run` has the supervisor reset the drop directory before the
    /// guest spawns, then `activate_prepared` records the new incarnation.
    #[test]
    fn queued_transfer_from_previous_incarnation_cannot_materialize_after_supervisor_reset() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("drops");
        // A long expiry keeps the queued transfer from being purged by age;
        // only the incarnation change may make it unreachable.
        let store = FileTransferStore::with_limits(&root, 8, 64 * 1024, 4, Duration::from_secs(60));
        store.activate("work").unwrap();
        let queued = store.preflight("work", request("queued.pdf", 4)).unwrap();
        let mut upload = store.begin_upload("work", &queued.transfer_id).unwrap();
        upload.write_chunk(b"data").unwrap();
        assert_eq!(upload.finish().unwrap().state, FileTransferState::Queued);
        let staged = store.staging_path("work", &queued.transfer_id);
        assert!(
            staged.is_file(),
            "the queued transfer must be staged on disk"
        );

        store.deactivate("work").unwrap();
        let drop_dir = store.drop_dir("work");
        if let Err(error) = fs::remove_dir_all(&drop_dir) {
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
        }
        fs::create_dir_all(&drop_dir).unwrap();
        set_private_directory(&drop_dir).unwrap();
        store.activate_prepared("work").unwrap();

        assert!(matches!(
            store.materialize("work", &queued.transfer_id),
            Err(FileTransferError::NotFound)
        ));
        assert!(matches!(
            store.status("work", &queued.transfer_id),
            Err(FileTransferError::NotFound)
        ));
        assert!(drop_dir.is_dir());
        assert_eq!(fs::read_dir(&drop_dir).unwrap().count(), 0);
        assert!(!staged.exists());
        store
            .preflight("work", request("fresh.pdf", 4))
            .expect("the new incarnation starts with a clean quota");
    }

    #[cfg(unix)]
    #[test]
    fn materialization_rejects_a_guest_precreated_transfer_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let preflight = store.preflight("work", request("report.pdf", 4)).unwrap();
        let mut upload = store.begin_upload("work", &preflight.transfer_id).unwrap();
        upload.write_chunk(b"data").unwrap();
        upload.finish().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(
            &outside,
            store.drop_dir("work").join(&preflight.transfer_id),
        )
        .unwrap();

        assert!(matches!(
            store.materialize("work", &preflight.transfer_id),
            Err(FileTransferError::Io(_))
        ));
        assert_eq!(
            store.status("work", &preflight.transfer_id).unwrap().state,
            FileTransferState::Failed
        );
        assert!(
            !outside.join("report.pdf").exists(),
            "a guest-controlled transfer-id symlink must not redirect the destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialization_never_follows_a_swapped_staged_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let preflight = store.preflight("work", request("report.pdf", 4)).unwrap();
        let mut upload = store.begin_upload("work", &preflight.transfer_id).unwrap();
        upload.write_chunk(b"data").unwrap();
        upload.finish().unwrap();
        // Same size and type as the staged upload, so only a refusal to
        // follow the link (not a metadata mismatch) can make this fail.
        let outside = temp.path().join("outside.bin");
        fs::write(&outside, b"data").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
        let staged = store.staging_path("work", &preflight.transfer_id);
        fs::remove_file(&staged).unwrap();
        symlink(&outside, &staged).unwrap();

        assert!(matches!(
            store.materialize("work", &preflight.transfer_id),
            Err(FileTransferError::Io(_))
        ));
        assert_eq!(
            store.status("work", &preflight.transfer_id).unwrap().state,
            FileTransferState::Failed
        );
        let mode = fs::metadata(&outside).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "a swapped staged symlink must not be followed to chmod its target"
        );
        assert!(
            !store.drop_dir("work").join(&preflight.transfer_id).exists(),
            "validation fails before any transfer directory is created"
        );
    }

    #[test]
    fn failed_queued_materialization_releases_its_reservation() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let preflight = store.preflight("work", request("first.pdf", 8)).unwrap();
        let mut upload = store.begin_upload("work", &preflight.transfer_id).unwrap();
        upload.write_chunk(b"contents").unwrap();
        upload.finish().unwrap();

        store
            .fail_queued_materialization("work", &preflight.transfer_id)
            .unwrap();
        assert_eq!(
            store.status("work", &preflight.transfer_id).unwrap().state,
            FileTransferState::Failed
        );
        assert!(!store.staging_path("work", &preflight.transfer_id).exists());
        assert!(
            store.preflight("work", request("second.pdf", 8)).is_ok(),
            "a failed worker launch must not keep its queued quota reservation"
        );
    }
}
