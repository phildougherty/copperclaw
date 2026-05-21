//! Persistent setup state.
//!
//! [`SetupState`] lives at `<data_dir>/setup-state.json` and records the
//! results of each completed step so a re-run can skip work that already
//! finished. The file is JSON for easy operator inspection.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::SetupConfig;

/// File name of the persisted state inside the data directory.
pub const STATE_FILENAME: &str = "setup-state.json";

/// On-disk shape persisted between setup runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupState {
    /// Schema version. Bump on breaking changes.
    #[serde(default = "default_version")]
    pub version: u32,
    /// The full setup config produced so far.
    pub config: SetupConfig,
    /// Step names that have already completed.
    #[serde(default)]
    pub completed_steps: Vec<String>,
}

fn default_version() -> u32 {
    1
}

/// Errors surfaced from state I/O.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Reading or writing the state file failed.
    #[error("state I/O at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// Parsing the state JSON failed (corrupted file).
    #[error("corrupted state at {path}: {source}")]
    Parse {
        /// Path involved.
        path: PathBuf,
        /// JSON parse error.
        #[source]
        source: serde_json::Error,
    },
}

impl SetupState {
    /// Create an empty state seeded with the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: default_version(),
            ..Self::default()
        }
    }

    /// Path to the state file inside `data_dir`.
    #[must_use]
    pub fn path_in(data_dir: &Path) -> PathBuf {
        data_dir.join(STATE_FILENAME)
    }

    /// Load state from `<data_dir>/setup-state.json`.
    ///
    /// Returns a fresh [`SetupState`] when the file does not exist.
    /// A corrupted (non-JSON) file is surfaced as [`StateError::Parse`].
    pub fn load(data_dir: &Path) -> Result<Self, StateError> {
        let path = Self::path_in(data_dir);
        match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| StateError::Parse { path, source })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::new()),
            Err(source) => Err(StateError::Io { path, source }),
        }
    }

    /// Persist state to `<data_dir>/setup-state.json`. Creates the
    /// directory if missing.
    pub fn save(&self, data_dir: &Path) -> Result<(), StateError> {
        let path = Self::path_in(data_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| StateError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self).expect("setup state always serializes");
        fs::write(&path, bytes).map_err(|source| StateError::Io { path, source })?;
        Ok(())
    }

    /// Mark `step` as completed (no-op when already present).
    pub fn mark_completed(&mut self, step: &str) {
        if !self.completed_steps.iter().any(|s| s == step) {
            self.completed_steps.push(step.to_string());
        }
    }

    /// Whether `step` is recorded as completed.
    #[must_use]
    pub fn is_completed(&self, step: &str) -> bool {
        self.completed_steps.iter().any(|s| s == step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn new_sets_version_and_empty_steps() {
        let s = SetupState::new();
        assert_eq!(s.version, 1);
        assert!(s.completed_steps.is_empty());
    }

    #[test]
    fn load_missing_returns_fresh() {
        let dir = tempdir().unwrap();
        let s = SetupState::load(dir.path()).unwrap();
        assert_eq!(s, SetupState::new());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let mut s = SetupState::new();
        s.mark_completed("env_check");
        s.config.image_tag = "img:tag".into();
        s.save(dir.path()).unwrap();
        let back = SetupState::load(dir.path()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn save_creates_missing_directory() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("nested/deep");
        let s = SetupState::new();
        s.save(&nested).unwrap();
        assert!(nested.join(STATE_FILENAME).exists());
    }

    #[test]
    fn corrupted_json_is_reported() {
        let dir = tempdir().unwrap();
        fs::write(SetupState::path_in(dir.path()), b"{not json").unwrap();
        let err = SetupState::load(dir.path()).unwrap_err();
        match err {
            StateError::Parse { path, .. } => {
                assert_eq!(path, SetupState::path_in(dir.path()));
            }
            StateError::Io { .. } => panic!("expected Parse, got Io"),
        }
    }

    #[test]
    fn mark_completed_is_idempotent() {
        let mut s = SetupState::new();
        s.mark_completed("data_dir");
        s.mark_completed("data_dir");
        assert_eq!(s.completed_steps, vec!["data_dir".to_string()]);
    }

    #[test]
    fn is_completed_reports_status() {
        let mut s = SetupState::new();
        assert!(!s.is_completed("auth"));
        s.mark_completed("auth");
        assert!(s.is_completed("auth"));
    }

    #[test]
    fn state_error_display() {
        let err = StateError::Io {
            path: PathBuf::from("/x"),
            source: io::Error::new(io::ErrorKind::Other, "boom"),
        };
        assert!(err.to_string().contains("/x"));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn state_error_parse_display() {
        let bad: serde_json::Error = serde_json::from_str::<SetupState>("{").unwrap_err();
        let err = StateError::Parse {
            path: PathBuf::from("/y"),
            source: bad,
        };
        assert!(err.to_string().contains("/y"));
    }

    #[test]
    fn path_in_returns_filename_under_dir() {
        let p = SetupState::path_in(Path::new("/tmp/x"));
        assert_eq!(p, PathBuf::from("/tmp/x/setup-state.json"));
    }

    #[test]
    fn default_version_helper_is_one() {
        assert_eq!(default_version(), 1);
    }

    #[test]
    fn load_io_error_for_unreadable_path() {
        // Pointing at a path the OS will return NotFound for parent is fine,
        // but here we want a non-NotFound error: pass a path that exists as
        // a directory rather than a file.
        let dir = tempdir().unwrap();
        let nested_dir = dir.path().join(STATE_FILENAME);
        fs::create_dir(&nested_dir).unwrap();
        let err = SetupState::load(dir.path()).unwrap_err();
        match err {
            StateError::Io { path, .. } => assert_eq!(path, nested_dir),
            StateError::Parse { .. } => panic!("expected Io, got Parse"),
        }
    }
}
