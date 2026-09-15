use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter};
use std::path::{Path, PathBuf};

use navette_protocol::{Session, SessionStatus};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

const REGISTRY_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("invalid session name: {0}")]
    InvalidName(String),
    #[error("session already exists: {0}")]
    AlreadyExists(String),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("failed to read registry {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode registry {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported registry version {0}")]
    UnsupportedVersion(u16),
    #[error("failed to persist registry {path}: {source}")]
    Persist {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Clone, Debug)]
pub struct Registry {
    path: PathBuf,
    sessions: BTreeMap<String, Session>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RegistryFile {
    version: u16,
    sessions: Vec<Session>,
}

impl Registry {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                sessions: BTreeMap::new(),
            });
        }

        let file = File::open(&path).map_err(|source| RegistryError::Read {
            path: path.clone(),
            source,
        })?;
        let stored: RegistryFile =
            serde_json::from_reader(BufReader::new(file)).map_err(|source| {
                RegistryError::Decode {
                    path: path.clone(),
                    source,
                }
            })?;
        if stored.version != REGISTRY_VERSION {
            return Err(RegistryError::UnsupportedVersion(stored.version));
        }

        let mut sessions = BTreeMap::new();
        for session in stored.sessions {
            validate_session_name(&session.name)?;
            sessions.insert(session.name.clone(), session);
        }
        Ok(Self { path, sessions })
    }

    pub fn open_default() -> Result<Self, RegistryError> {
        Self::open(default_registry_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Vec<Session> {
        self.sessions.values().cloned().collect()
    }

    pub fn get(&self, name: &str) -> Option<&Session> {
        self.sessions.get(name)
    }

    pub fn insert(&mut self, session: Session) -> Result<(), RegistryError> {
        validate_session_name(&session.name)?;
        if self.sessions.contains_key(&session.name) {
            return Err(RegistryError::AlreadyExists(session.name));
        }
        let name = session.name.clone();
        self.sessions.insert(name.clone(), session);
        if let Err(error) = self.save() {
            self.sessions.remove(&name);
            return Err(error);
        }
        Ok(())
    }

    pub fn replace(&mut self, session: Session) -> Result<(), RegistryError> {
        validate_session_name(&session.name)?;
        if !self.sessions.contains_key(&session.name) {
            return Err(RegistryError::NotFound(session.name));
        }
        let name = session.name.clone();
        let previous = self
            .sessions
            .insert(name.clone(), session)
            .expect("existence checked");
        if let Err(error) = self.save() {
            self.sessions.insert(name, previous);
            return Err(error);
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<Session, RegistryError> {
        let session = self
            .sessions
            .remove(name)
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        if let Err(error) = self.save() {
            self.sessions.insert(name.to_string(), session);
            return Err(error);
        }
        Ok(session)
    }

    pub fn mark_attached(&mut self, name: &str, now_ms: u64) -> Result<Session, RegistryError> {
        let previous = self
            .sessions
            .get(name)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        let result = {
            let session = self.sessions.get_mut(name).expect("existence checked");
            session.last_attached_at_ms = Some(now_ms);
            session.client_count = session.client_count.saturating_add(1);
            session.clone()
        };
        if let Err(error) = self.save() {
            self.sessions.insert(name.to_string(), previous);
            return Err(error);
        }
        Ok(result)
    }

    pub fn mark_detached(&mut self, name: &str) -> Result<Session, RegistryError> {
        let previous = self
            .sessions
            .get(name)
            .cloned()
            .ok_or_else(|| RegistryError::NotFound(name.to_string()))?;
        let result = {
            let session = self.sessions.get_mut(name).expect("existence checked");
            session.client_count = session.client_count.saturating_sub(1);
            session.clone()
        };
        if let Err(error) = self.save() {
            self.sessions.insert(name.to_string(), previous);
            return Err(error);
        }
        Ok(result)
    }

    pub fn reconcile<F>(&mut self, mut is_alive: F) -> Result<bool, RegistryError>
    where
        F: FnMut(u32) -> bool,
    {
        let previous = self.sessions.clone();
        let mut changed = false;
        for session in self.sessions.values_mut() {
            if matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running
            ) && (!is_alive(session.app_pid) || !is_alive(session.daemon_pid))
            {
                session.status = SessionStatus::Stopped;
                session.client_count = 0;
                changed = true;
            }
        }
        if changed && let Err(error) = self.save() {
            self.sessions = previous;
            return Err(error);
        }
        Ok(changed)
    }

    pub fn save(&self) -> Result<(), RegistryError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| RegistryError::Persist {
            path: self.path.clone(),
            source,
        })?;

        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|source| RegistryError::Persist {
                path: self.path.clone(),
                source,
            })?;
        {
            let stored = RegistryFile {
                version: REGISTRY_VERSION,
                sessions: self.list(),
            };
            let mut writer = BufWriter::new(temporary.as_file_mut());
            serde_json::to_writer_pretty(&mut writer, &stored).map_err(|source| {
                RegistryError::Persist {
                    path: self.path.clone(),
                    source: io::Error::other(source),
                }
            })?;
            use std::io::Write;
            writer
                .write_all(b"\n")
                .map_err(|source| RegistryError::Persist {
                    path: self.path.clone(),
                    source,
                })?;
            writer.flush().map_err(|source| RegistryError::Persist {
                path: self.path.clone(),
                source,
            })?;
        }
        temporary
            .as_file()
            .sync_all()
            .map_err(|source| RegistryError::Persist {
                path: self.path.clone(),
                source,
            })?;
        temporary
            .persist(&self.path)
            .map_err(|error| RegistryError::Persist {
                path: self.path.clone(),
                source: error.error,
            })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| RegistryError::Persist {
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }
}

pub fn validate_session_name(name: &str) -> Result<(), RegistryError> {
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(RegistryError::InvalidName(name.to_string()))
    }
}

pub fn default_session_name(app_id: &str) -> String {
    let mut result = String::with_capacity(app_id.len().min(64));
    let mut last_was_separator = false;
    for character in app_id.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            last_was_separator = false;
        } else if !result.is_empty() && !last_was_separator {
            result.push('-');
            last_was_separator = true;
        }
        if result.len() == 64 {
            break;
        }
    }
    while result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "session".to_string()
    } else {
        result
    }
}

pub fn default_registry_path() -> PathBuf {
    let state_home = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    state_home
        .unwrap_or_else(|| env::temp_dir().join(format!("navette-{}", std::process::id())))
        .join("navette/registry.json")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn session(name: &str) -> Session {
        Session {
            name: name.into(),
            app_id: "firefox".into(),
            app_pid: 10,
            daemon_pid: 11,
            wayland_display: format!("navette-{name}"),
            socket_path: format!("/run/navette/{name}/wprs.sock"),
            created_at_ms: 100,
            last_attached_at_ms: None,
            client_count: 0,
            status: SessionStatus::Running,
        }
    }

    #[test]
    fn persists_and_reopens_registry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("nested/registry.json");
        let mut registry = Registry::open(&path).unwrap();
        registry.insert(session("work")).unwrap();

        let reopened = Registry::open(&path).unwrap();
        assert_eq!(reopened.get("work"), Some(&session("work")));
        assert_eq!(reopened.path(), path);
    }

    #[test]
    fn duplicate_insert_is_rejected_without_changing_disk() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("registry.json");
        let mut registry = Registry::open(&path).unwrap();
        registry.insert(session("work")).unwrap();
        let before = fs::read(&path).unwrap();

        let error = registry.insert(session("work")).unwrap_err();
        assert!(matches!(error, RegistryError::AlreadyExists(_)));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn failed_persist_rolls_back_in_memory_insert() {
        let temp = TempDir::new().unwrap();
        let blocker = temp.path().join("not-a-directory");
        fs::write(&blocker, b"blocker").unwrap();
        let mut registry = Registry::open(blocker.join("registry.json")).unwrap();

        assert!(matches!(
            registry.insert(session("work")),
            Err(RegistryError::Persist { .. })
        ));
        assert!(registry.list().is_empty());
    }

    #[test]
    fn invalid_names_are_rejected() {
        for invalid in [
            "",
            "Upper",
            "-leading",
            "has space",
            "dot.name",
            &"a".repeat(65),
        ] {
            assert!(validate_session_name(invalid).is_err(), "{invalid}");
        }
        for valid in ["a", "work-browser", "work_2", "2fa"] {
            assert!(validate_session_name(valid).is_ok(), "{valid}");
        }
    }

    #[test]
    fn derives_safe_default_names() {
        assert_eq!(default_session_name("Firefox.desktop"), "firefox-desktop");
        assert_eq!(
            default_session_name("org.gnome.Builder"),
            "org-gnome-builder"
        );
        assert_eq!(default_session_name("---"), "session");
    }

    #[test]
    fn attach_and_detach_counts_are_saturating() {
        let temp = TempDir::new().unwrap();
        let mut registry = Registry::open(temp.path().join("registry.json")).unwrap();
        registry.insert(session("work")).unwrap();

        assert_eq!(registry.mark_attached("work", 200).unwrap().client_count, 1);
        let detached = registry.mark_detached("work").unwrap();
        assert_eq!(detached.client_count, 0);
        assert_eq!(detached.last_attached_at_ms, Some(200));
        assert_eq!(registry.mark_detached("work").unwrap().client_count, 0);
    }

    #[test]
    fn reconcile_marks_dead_sessions_stopped() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("registry.json");
        let mut registry = Registry::open(&path).unwrap();
        registry.insert(session("work")).unwrap();

        assert!(registry.reconcile(|pid| pid == 10).unwrap());
        assert_eq!(registry.get("work").unwrap().status, SessionStatus::Stopped);
        assert!(!Registry::open(path).unwrap().reconcile(|_| false).unwrap());
    }

    #[test]
    fn corrupt_registry_returns_decode_error() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("registry.json");
        fs::write(&path, b"not json").unwrap();
        assert!(matches!(
            Registry::open(path),
            Err(RegistryError::Decode { .. })
        ));
    }

    #[test]
    fn reopening_refuses_a_persisted_session_name_that_escapes_runtime_storage() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("registry.json");
        fs::write(
            &path,
            serde_json::to_vec(&RegistryFile {
                version: REGISTRY_VERSION,
                sessions: vec![session("../outside")],
            })
            .unwrap(),
        )
        .unwrap();

        assert!(matches!(
            Registry::open(path),
            Err(RegistryError::InvalidName(name)) if name == "../outside"
        ));
    }

    #[test]
    fn remove_and_replace_require_existing_session() {
        let temp = TempDir::new().unwrap();
        let mut registry = Registry::open(temp.path().join("registry.json")).unwrap();
        assert!(matches!(
            registry.remove("missing"),
            Err(RegistryError::NotFound(_))
        ));
        assert!(matches!(
            registry.replace(session("missing")),
            Err(RegistryError::NotFound(_))
        ));

        registry.insert(session("work")).unwrap();
        let mut updated = session("work");
        updated.status = SessionStatus::Failed;
        registry.replace(updated.clone()).unwrap();
        assert_eq!(registry.get("work"), Some(&updated));
        assert_eq!(registry.remove("work").unwrap(), updated);
        assert!(registry.is_empty());
    }

    impl Registry {
        fn is_empty(&self) -> bool {
            self.sessions.is_empty()
        }
    }
}
