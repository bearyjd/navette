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

#[derive(Clone, Debug)]
pub struct BlobStore {
    root: Arc<PathBuf>,
    active: Arc<Mutex<HashMap<String, u64>>>,
    max_blob_bytes: u64,
    max_session_bytes: u64,
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
    #[error("blob I/O failed: {0}")]
    Io(#[from] io::Error),
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Arc::new(root.into()),
            active: Arc::new(Mutex::new(HashMap::new())),
            max_blob_bytes: MAX_BLOB_BYTES as u64,
            max_session_bytes: MAX_SESSION_BLOB_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limits(
        root: impl Into<PathBuf>,
        max_blob_bytes: u64,
        max_session_bytes: u64,
    ) -> Self {
        Self {
            root: Arc::new(root.into()),
            active: Arc::new(Mutex::new(HashMap::new())),
            max_blob_bytes,
            max_session_bytes,
        }
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
        let directory = self.root.join(session);
        fs::create_dir_all(&directory)?;
        set_private_directory(&directory)?;

        let id = random_id()?;
        let part_path = directory.join(format!("{}.part", id));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)?;
        Ok(BlobWriter {
            store: self.clone(),
            session: session.to_owned(),
            mime: mime.to_owned(),
            id,
            part_path,
            file: Some(file),
            reserved: 0,
        })
    }

    pub fn read(&self, session: &str, blob: &BlobDescriptor) -> Result<Vec<u8>, BlobStoreError> {
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
        let mut file = File::open(path).map_err(not_found)?;
        let mut output = Vec::with_capacity(usize::try_from(blob.size).unwrap_or(0));
        file.read_to_end(&mut output)?;
        Ok(output)
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

    fn reserve(&self, session: &str, additional: u64) -> Result<(), BlobStoreError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| io::Error::other("blob lock poisoned"))?;
        let current = stable_usage(&self.root.join(session))?;
        let reserved = active.get(session).copied().unwrap_or(0);
        if current
            .checked_add(reserved)
            .and_then(|used| used.checked_add(additional))
            .is_none_or(|used| used > self.max_session_bytes)
        {
            return Err(BlobStoreError::SessionBudgetExceeded);
        }
        *active.entry(session.to_owned()).or_default() += additional;
        Ok(())
    }

    fn release(&self, session: &str, bytes: u64) {
        if let Ok(mut active) = self.active.lock()
            && let Some(reserved) = active.get_mut(session)
        {
            *reserved = reserved.saturating_sub(bytes);
            if *reserved == 0 {
                active.remove(session);
            }
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
}

impl BlobWriter {
    pub fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), BlobStoreError> {
        let additional = u64::try_from(chunk.len()).map_err(|_| BlobStoreError::BlobTooLarge)?;
        if self
            .reserved
            .checked_add(additional)
            .is_none_or(|size| size > self.store.max_blob_bytes)
        {
            return Err(BlobStoreError::BlobTooLarge);
        }
        self.store.reserve(&self.session, additional)?;
        if let Some(file) = self.file.as_mut()
            && let Err(error) = file.write_all(chunk)
        {
            self.store.release(&self.session, additional);
            return Err(BlobStoreError::Io(error));
        }
        self.reserved += additional;
        Ok(())
    }

    pub fn finish(mut self) -> Result<BlobDescriptor, BlobStoreError> {
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
        self.store.release(&self.session, self.reserved);
        self.reserved = 0;
        Ok(descriptor)
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.part_path);
            self.store.release(&self.session, self.reserved);
        }
    }
}

fn is_supported_mime(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/webp")
}

fn stable_usage(directory: &Path) -> io::Result<u64> {
    let mut total = 0_u64;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_file() && !name.ends_with(".part") && !name.ends_with(".meta") {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
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
        BlobStore::new(temp.path().join("blobs"))
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
        let store = BlobStore::with_limits(temp.path().join("blobs"), 4, 6);

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
}
