//! Session-scoped, bounded storage for authenticated bulk clipboard blobs.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use navette_protocol::media::{BLOB_ID_LEN, BlobDescriptor, MAX_BLOB_BYTES};
use thiserror::Error;

use crate::registry::validate_session_name;

pub const MAX_SESSION_BLOB_BYTES: u64 = 256 * 1024 * 1024;
/// Limits the number of completed blobs in one live session, so many tiny
/// images cannot exhaust filesystem inodes while staying under the byte cap.
pub const MAX_SESSION_BLOB_OBJECTS: u64 = 1_024;

#[derive(Clone, Debug)]
pub struct BlobStore {
    root: Arc<PathBuf>,
    active: Arc<Mutex<HashMap<(String, u64), Reservation>>>,
    lifecycle: Arc<Mutex<BlobLifecycle>>,
    max_blob_bytes: u64,
    max_session_bytes: u64,
    max_session_objects: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Reservation {
    bytes: u64,
    objects: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Usage {
    bytes: u64,
    objects: u64,
}

#[derive(Debug, Default)]
struct BlobLifecycle {
    next_epoch: u64,
    live: HashMap<String, u64>,
}

#[derive(Debug, Error)]
pub enum BlobStoreError {
    #[error("invalid session")]
    InvalidSession,
    #[error("unsupported blob MIME type")]
    UnsupportedMime,
    #[error("invalid blob descriptor")]
    InvalidDescriptor,
    #[error("blob exceeds the per-blob limit")]
    BlobTooLarge,
    #[error("session blob budget is exhausted")]
    SessionBudgetExceeded,
    #[error("blob not found")]
    NotFound,
    #[error("session is not live")]
    SessionNotLive,
    #[error("blob I/O failed: {0}")]
    Io(#[from] io::Error),
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
            active: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(BlobLifecycle::default())),
            max_blob_bytes: MAX_BLOB_BYTES as u64,
            max_session_bytes: MAX_SESSION_BLOB_BYTES,
            max_session_objects: MAX_SESSION_BLOB_OBJECTS,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(
        root: impl Into<PathBuf>,
        max_blob_bytes: u64,
        max_session_bytes: u64,
        max_session_objects: u64,
    ) -> Self {
        Self {
            root: Arc::new(root.into()),
            active: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(BlobLifecycle::default())),
            max_blob_bytes,
            max_session_bytes,
            max_session_objects,
        }
    }

    /// Starts a fresh blob namespace for a newly running session. Holding the
    /// lifecycle lock across cleanup prevents a writer leased to a previous
    /// incarnation from publishing into a reused name.
    pub fn activate(&self, session: &str) -> Result<(), BlobStoreError> {
        if validate_session_name(session).is_err() {
            return Err(BlobStoreError::InvalidSession);
        }
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("blob lifecycle lock poisoned"))?;
        fs::remove_dir_all(self.root.join(session)).or_else(ignore_not_found)?;
        lifecycle.next_epoch = lifecycle.next_epoch.wrapping_add(1).max(1);
        let epoch = lifecycle.next_epoch;
        lifecycle.live.insert(session.to_owned(), epoch);
        Ok(())
    }

    /// Revokes every writer lease before deleting the session namespace.
    pub fn deactivate(&self, session: &str) -> Result<(), BlobStoreError> {
        if validate_session_name(session).is_err() {
            return Err(BlobStoreError::InvalidSession);
        }
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("blob lifecycle lock poisoned"))?;
        lifecycle.live.remove(session);
        fs::remove_dir_all(self.root.join(session)).or_else(ignore_not_found)?;
        Ok(())
    }

    /// Begins an atomic write. Dropping the writer removes the partial file
    /// and returns its reservation to the session budget.
    pub fn begin_write(&self, session: &str, mime: &str) -> Result<BlobWriter, BlobStoreError> {
        if validate_session_name(session).is_err() {
            return Err(BlobStoreError::InvalidSession);
        }
        if !is_supported_mime(mime) {
            return Err(BlobStoreError::UnsupportedMime);
        }
        let lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("blob lifecycle lock poisoned"))?;
        let Some(epoch) = lifecycle.live.get(session).copied() else {
            return Err(BlobStoreError::SessionNotLive);
        };
        let directory = self.root.join(session);
        fs::create_dir_all(&directory)?;
        set_private_directory(&directory)?;

        // Reserve an object before creating a partial file. This covers
        // concurrent writers as well as completed blobs; otherwise a burst
        // of tiny uploads can race the stable on-disk object count.
        self.reserve(session, epoch, 0, 1)?;

        let id = random_id()?;
        let part_path = directory.join(format!("{}.part", id));
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
        {
            Ok(file) => file,
            Err(error) => {
                self.release(session, epoch, 0, 1);
                return Err(BlobStoreError::Io(error));
            }
        };
        Ok(BlobWriter {
            store: self.clone(),
            session: session.to_owned(),
            mime: mime.to_owned(),
            id,
            part_path,
            file: Some(file),
            reserved: 0,
            reserved_objects: 1,
            epoch,
            revoked: false,
        })
    }

    pub fn read(&self, session: &str, blob: &BlobDescriptor) -> Result<Vec<u8>, BlobStoreError> {
        let path = self.read_path(session, blob)?;
        let mut file = File::open(path).map_err(not_found)?;
        let mut output = Vec::with_capacity(usize::try_from(blob.size).unwrap_or(0));
        file.read_to_end(&mut output)?;
        Ok(output)
    }

    /// Validates a blob and returns its regular-file path for a streaming
    /// response. The route opens it asynchronously, avoiding a full in-memory
    /// buffer for 64 MiB downloads.
    pub fn read_path(
        &self,
        session: &str,
        blob: &BlobDescriptor,
    ) -> Result<PathBuf, BlobStoreError> {
        if validate_session_name(session).is_err() {
            return Err(BlobStoreError::InvalidSession);
        }
        blob.validate()
            .map_err(|_| BlobStoreError::InvalidDescriptor)?;
        let path = self.root.join(session).join(&blob.id);
        let metadata = fs::symlink_metadata(&path).map_err(not_found)?;
        if !metadata.file_type().is_file() || metadata.len() != blob.size {
            return Err(BlobStoreError::NotFound);
        }
        Ok(path)
    }

    /// Removes a no-longer-current blob only when the stored descriptor still
    /// matches exactly. Callers cannot turn a stale or forged descriptor into
    /// deletion of another object in the session namespace.
    pub fn remove(&self, session: &str, blob: &BlobDescriptor) -> Result<(), BlobStoreError> {
        if validate_session_name(session).is_err() {
            return Err(BlobStoreError::InvalidSession);
        }
        blob.validate()
            .map_err(|_| BlobStoreError::InvalidDescriptor)?;
        if self.descriptor(session, &blob.id).as_ref() != Some(blob) {
            return Err(BlobStoreError::NotFound);
        }
        let directory = self.root.join(session);
        fs::remove_file(directory.join(&blob.id)).map_err(not_found)?;
        fs::remove_file(directory.join(format!("{}.meta", blob.id))).map_err(not_found)?;
        Ok(())
    }

    /// Looks up daemon-written metadata for an opaque ID. The route never
    /// accepts MIME or size from the caller.
    pub fn descriptor(&self, session: &str, id: &str) -> Option<BlobDescriptor> {
        if validate_session_name(session).is_err()
            || id.len() != BLOB_ID_LEN
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let metadata = fs::read(self.root.join(session).join(format!("{}.meta", id))).ok()?;
        let descriptor: BlobDescriptor = serde_json::from_slice(&metadata).ok()?;
        (descriptor.id == id && descriptor.validate().is_ok()).then_some(descriptor)
    }

    fn reserve(
        &self,
        session: &str,
        epoch: u64,
        additional_bytes: u64,
        additional_objects: u64,
    ) -> Result<(), BlobStoreError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| io::Error::other("blob lock poisoned"))?;
        let current = stable_usage(&self.root.join(session))?;
        let key = (session.to_owned(), epoch);
        let reserved = active.get(&key).copied().unwrap_or_default();
        if additional_bytes > 0
            && current
                .bytes
                .checked_add(reserved.bytes)
                .and_then(|used| used.checked_add(additional_bytes))
                .is_none_or(|used| used > self.max_session_bytes)
        {
            return Err(BlobStoreError::SessionBudgetExceeded);
        }
        if current
            .objects
            .checked_add(reserved.objects)
            .and_then(|used| used.checked_add(additional_objects))
            .is_none_or(|used| used > self.max_session_objects)
        {
            return Err(BlobStoreError::SessionBudgetExceeded);
        }
        let reservation = active.entry(key).or_default();
        reservation.bytes += additional_bytes;
        reservation.objects += additional_objects;
        Ok(())
    }

    fn release(&self, session: &str, epoch: u64, bytes: u64, objects: u64) {
        if let Ok(mut active) = self.active.lock()
            && let Some(reserved) = active.get_mut(&(session.to_owned(), epoch))
        {
            reserved.bytes = reserved.bytes.saturating_sub(bytes);
            reserved.objects = reserved.objects.saturating_sub(objects);
            if reserved.bytes == 0 && reserved.objects == 0 {
                active.remove(&(session.to_owned(), epoch));
            }
        }
    }

    fn lease(
        &self,
        session: &str,
        epoch: u64,
    ) -> Result<std::sync::MutexGuard<'_, BlobLifecycle>, BlobStoreError> {
        let lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| io::Error::other("blob lifecycle lock poisoned"))?;
        if lifecycle.live.get(session).copied() == Some(epoch) {
            Ok(lifecycle)
        } else {
            Err(BlobStoreError::SessionNotLive)
        }
    }
}

pub struct BlobWriter {
    store: BlobStore,
    session: String,
    mime: String,
    id: String,
    part_path: PathBuf,
    file: Option<File>,
    reserved: u64,
    reserved_objects: u64,
    epoch: u64,
    revoked: bool,
}

impl BlobWriter {
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), BlobStoreError> {
        let store = self.store.clone();
        let session = self.session.clone();
        let epoch = self.epoch;
        let lease = match store.lease(&session, epoch) {
            Ok(lease) => lease,
            Err(BlobStoreError::SessionNotLive) => {
                self.revoke();
                return Err(BlobStoreError::SessionNotLive);
            }
            Err(error) => return Err(error),
        };
        let additional = u64::try_from(chunk.len()).map_err(|_| BlobStoreError::BlobTooLarge)?;
        if self
            .reserved
            .checked_add(additional)
            .is_none_or(|size| size > self.store.max_blob_bytes)
        {
            return Err(BlobStoreError::BlobTooLarge);
        }
        self.store
            .reserve(&self.session, self.epoch, additional, 0)?;
        if let Some(file) = self.file.as_mut()
            && let Err(error) = file.write_all(chunk)
        {
            self.store.release(&self.session, self.epoch, additional, 0);
            return Err(BlobStoreError::Io(error));
        }
        self.reserved += additional;
        drop(lease);
        Ok(())
    }

    pub fn finish(mut self) -> Result<BlobDescriptor, BlobStoreError> {
        if self.revoked {
            return Err(BlobStoreError::SessionNotLive);
        }
        if self.reserved == 0 {
            return Err(BlobStoreError::InvalidDescriptor);
        }
        self.file
            .as_mut()
            .expect("writer not already finished")
            .sync_all()?;
        let final_path = self.part_path.with_extension("");
        let descriptor = BlobDescriptor {
            id: self.id.clone(),
            mime: self.mime.clone(),
            size: self.reserved,
        };
        descriptor
            .validate()
            .map_err(|_| BlobStoreError::InvalidDescriptor)?;
        let _lifecycle = self.store.lease(&self.session, self.epoch)?;
        let metadata_part = final_path.with_extension("meta.part");
        let metadata_path = final_path.with_extension("meta");
        fs::write(
            &metadata_part,
            serde_json::to_vec(&descriptor).expect("BlobDescriptor serializes"),
        )?;
        fs::rename(&self.part_path, &final_path)?;
        if let Err(error) = fs::rename(&metadata_part, &metadata_path) {
            let _ = fs::remove_file(&final_path);
            let _ = fs::remove_file(&metadata_part);
            return Err(BlobStoreError::Io(error));
        }
        self.file.take();
        self.store.release(
            &self.session,
            self.epoch,
            self.reserved,
            self.reserved_objects,
        );
        self.reserved = 0;
        self.reserved_objects = 0;
        Ok(descriptor)
    }

    fn revoke(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.part_path);
            self.store.release(
                &self.session,
                self.epoch,
                self.reserved,
                self.reserved_objects,
            );
        }
        self.reserved = 0;
        self.reserved_objects = 0;
        self.revoked = true;
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.part_path);
            self.store.release(
                &self.session,
                self.epoch,
                self.reserved,
                self.reserved_objects,
            );
        }
    }
}

fn is_supported_mime(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/webp")
}

fn stable_usage(directory: &Path) -> io::Result<Usage> {
    let mut usage = Usage::default();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Usage::default()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_file() && !name.ends_with(".part") {
            let metadata = entry.metadata()?;
            // Payload and descriptor metadata both consume disk blocks. The
            // object count intentionally tracks payload files only: every
            // completed blob has exactly one paired metadata file.
            usage.bytes = usage.bytes.saturating_add(allocated_bytes(&metadata));
            if !name.ends_with(".meta") {
                usage.objects = usage.objects.saturating_add(1);
            }
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

fn random_id() -> Result<String, BlobStoreError> {
    let mut bytes = [0_u8; BLOB_ID_LEN / 2];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut id = String::with_capacity(BLOB_ID_LEN);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(id)
}

fn not_found(error: io::Error) -> BlobStoreError {
    if error.kind() == io::ErrorKind::NotFound {
        BlobStoreError::NotFound
    } else {
        BlobStoreError::Io(error)
    }
}

fn ignore_not_found(error: io::Error) -> io::Result<()> {
    if error.kind() == io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
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

    fn store(temp: &TempDir) -> BlobStore {
        let store = BlobStore::new(temp.path().join("blobs"));
        store.activate("desktop").unwrap();
        store
    }

    #[test]
    fn stores_atomically_and_reads_by_opaque_descriptor() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let mut writer = store.begin_write("desktop", "image/png").unwrap();
        writer.write_chunk(b"hello").unwrap();
        let blob = writer.finish().unwrap();
        assert_eq!(store.read("desktop", &blob).unwrap(), b"hello");
        assert!(
            !temp
                .path()
                .join("blobs")
                .join("desktop")
                .join(format!("{}.part", blob.id))
                .exists()
        );
    }

    #[test]
    fn dropping_a_cancelled_upload_removes_the_partial_file_and_budget_reservation() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let part_path;
        {
            let mut writer = store.begin_write("desktop", "image/png").unwrap();
            part_path = writer.part_path.clone();
            writer.write_chunk(b"partial").unwrap();
        }
        assert!(!part_path.exists());
        let mut writer = store.begin_write("desktop", "image/png").unwrap();
        writer.write_chunk(b"replacement").unwrap();
        assert_eq!(
            store.read("desktop", &writer.finish().unwrap()).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn rejects_invalid_inputs_and_never_treats_paths_as_ids() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        assert!(matches!(
            store.begin_write("../desktop", "image/png"),
            Err(BlobStoreError::InvalidSession)
        ));
        assert!(matches!(
            store.begin_write("desktop", "image/svg+xml"),
            Err(BlobStoreError::UnsupportedMime)
        ));
        let bad = BlobDescriptor {
            id: "../not-a-blob".into(),
            mime: "image/png".into(),
            size: 1,
        };
        assert!(matches!(
            store.read("desktop", &bad),
            Err(BlobStoreError::InvalidDescriptor)
        ));
    }

    #[test]
    fn enforces_per_blob_and_per_session_budgets_without_publishing_partial_data() {
        let temp = TempDir::new().unwrap();
        let store = BlobStore::with_limits(temp.path().join("blobs"), 4, 6, 16);
        store.activate("desktop").unwrap();

        let mut too_big = store.begin_write("desktop", "image/png").unwrap();
        assert!(matches!(
            too_big.write_chunk(b"12345"),
            Err(BlobStoreError::BlobTooLarge)
        ));

        let mut first = store.begin_write("desktop", "image/png").unwrap();
        first.write_chunk(b"1234").unwrap();
        first.finish().unwrap();

        let mut over_session = store.begin_write("desktop", "image/png").unwrap();
        assert!(matches!(
            over_session.write_chunk(b"123"),
            Err(BlobStoreError::SessionBudgetExceeded)
        ));
    }

    #[test]
    fn limits_tiny_blob_objects_and_reclaims_a_superseded_descriptor() {
        let temp = TempDir::new().unwrap();
        let store = BlobStore::with_limits(temp.path().join("blobs"), 4, 64 * 1024, 1);
        store.activate("desktop").unwrap();

        let mut first_writer = store.begin_write("desktop", "image/png").unwrap();
        first_writer.write_chunk(b"a").unwrap();
        let first = first_writer.finish().unwrap();

        assert!(matches!(
            store.begin_write("desktop", "image/png"),
            Err(BlobStoreError::SessionBudgetExceeded)
        ));
        store.remove("desktop", &first).unwrap();
        assert!(
            !temp
                .path()
                .join("blobs/desktop")
                .join(format!("{}.meta", first.id))
                .exists(),
            "reclamation removes paired descriptor metadata too"
        );

        let mut replacement = store.begin_write("desktop", "image/png").unwrap();
        replacement.write_chunk(b"b").unwrap();
        assert_eq!(
            store
                .read("desktop", &replacement.finish().unwrap())
                .unwrap(),
            b"b"
        );
    }

    #[test]
    fn revoked_writer_cannot_publish_into_a_recreated_session_namespace() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("blobs");
        let stale = root.join("desktop/stale-blob");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, b"orphaned").unwrap();
        let store = BlobStore::new(&root);
        store.activate("desktop").unwrap();
        assert!(
            !stale.exists(),
            "a session start must clear an orphaned namespace before leasing it"
        );
        let mut writer = store.begin_write("desktop", "image/png").unwrap();
        writer.write_chunk(b"old").unwrap();

        store.deactivate("desktop").unwrap();
        store.activate("desktop").unwrap();

        assert!(matches!(
            writer.finish(),
            Err(BlobStoreError::SessionNotLive)
        ));
        assert!(
            !temp.path().join("blobs/desktop").exists(),
            "a writer leased before kill must not recreate the replacement namespace"
        );
    }

    #[test]
    fn old_epoch_reservations_do_not_exhaust_or_release_a_recreated_sessions_budget() {
        let temp = TempDir::new().unwrap();
        let store = BlobStore::with_limits(temp.path().join("blobs"), 4, 4, 16);
        store.activate("desktop").unwrap();
        let mut old = store.begin_write("desktop", "image/png").unwrap();
        old.write_chunk(b"old!").unwrap();

        store.deactivate("desktop").unwrap();
        store.activate("desktop").unwrap();
        let mut current = store.begin_write("desktop", "image/png").unwrap();
        current.write_chunk(b"new!").unwrap();

        drop(old);
        let mut another_current = store.begin_write("desktop", "image/png").unwrap();
        assert!(
            matches!(
                another_current.write_chunk(b"more"),
                Err(BlobStoreError::SessionBudgetExceeded)
            ),
            "dropping an old writer must not release the recreated session's reservation"
        );
        drop(another_current);
        drop(current);
    }

    #[test]
    fn revoked_writer_rejects_later_chunks_cleans_its_partial_and_cannot_finish() {
        let temp = TempDir::new().unwrap();
        let store = store(&temp);
        let mut writer = store.begin_write("desktop", "image/png").unwrap();
        writer.write_chunk(b"old").unwrap();
        let part_path = writer.part_path.clone();
        assert!(part_path.exists());

        store.deactivate("desktop").unwrap();

        assert!(matches!(
            writer.write_chunk(b"new"),
            Err(BlobStoreError::SessionNotLive)
        ));
        assert!(
            !part_path.exists(),
            "revocation must clean the stale partial immediately"
        );
        assert!(matches!(
            writer.finish(),
            Err(BlobStoreError::SessionNotLive)
        ));
    }
}
