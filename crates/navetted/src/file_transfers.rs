//! Session-scoped, one-way file delivery from an authenticated client to a
//! guest. Client input never chooses a destination path: it supplies metadata
//! for a daemon-generated transfer id, and materialization owns every path.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    /// Starts a fresh per-session drop namespace. A session name is not
    /// reusable until its old guest process has been stopped, so stale files
    /// must never become visible to a new guest with the same name.
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
        let source = self.staging_path(session, id);
        let drop_dir = self.drop_dir(session);
        let destination_dir = drop_dir.join(id);
        let outcome = (|| -> Result<(), io::Error> {
            fs::create_dir_all(&drop_dir)?;
            set_private_directory(&drop_dir)?;
            fs::create_dir_all(&destination_dir)?;
            set_private_directory(&destination_dir)?;
            let metadata = fs::symlink_metadata(&source)?;
            if !metadata.file_type().is_file() || metadata.len() != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "staged file is invalid",
                ));
            }
            set_private_file(&source)?;
            let destination = destination_dir.join(name);
            fs::rename(source, &destination)?;
            Ok(())
        })();
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
            let _ = fs::remove_dir_all(&destination_dir);
        }
        drop(lifecycle);
        outcome.map_err(FileTransferError::Io)?;
        Ok(result)
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

#[cfg(unix)]
fn set_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> io::Result<()> {
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

        // This models Kill followed by Run using the same session name while
        // an old HTTP request is still streaming. `activate` advances the
        // incarnation and removes the old transfer registry atomically.
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
}
