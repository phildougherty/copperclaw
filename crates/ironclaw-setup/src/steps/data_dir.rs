//! Step 2 — data directory init.
//!
//! Picks the default directory per platform (or uses the `--data-dir`
//! override applied to the [`SetupConfig`] by the caller) and creates the
//! required subdirectories.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use std::path::{Path, PathBuf};

/// Standard subdirectories created under the data root.
pub const SUBDIRS: &[&str] = &["data", "data/sessions", "groups", "skills", "logs"];

/// Step implementation.
#[derive(Debug, Default)]
pub struct DataDirStep;

impl Step for DataDirStep {
    fn name(&self) -> &'static str {
        "data_dir"
    }

    fn description(&self) -> &'static str {
        "Create the ironclaw data directory layout"
    }

    fn is_skippable(&self) -> bool {
        false
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        if cfg.data_dir.as_os_str().is_empty() {
            let default = default_data_dir().unwrap_or_else(|| PathBuf::from("./ironclaw"));
            let answer = prompt.input(
                "DATA_DIR",
                "Data directory",
                Some(&default.display().to_string()),
            )?;
            cfg.data_dir = PathBuf::from(answer);
        }
        ensure_layout(&cfg.data_dir)?;
        cfg.central_db_path = cfg.data_dir.join("data").join("ironclaw.db");
        Ok(StepResult::ok(format!(
            "data directory ready at {}",
            cfg.data_dir.display()
        )))
    }
}

/// Default data directory per platform.
///
/// - Linux: `$XDG_DATA_HOME/ironclaw` (falling back to `~/.local/share/ironclaw`).
/// - macOS: `~/Library/Application Support/ironclaw`.
/// - Other targets: `~/.ironclaw`.
///
/// Returns `None` when neither `$HOME` nor a platform-specific override is
/// available.
#[must_use]
pub fn default_data_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(default_data_dir_for(&home, std::env::consts::OS))
}

/// Pure version of [`default_data_dir`] for unit tests.
#[must_use]
pub fn default_data_dir_for(home: &Path, os: &str) -> PathBuf {
    match os {
        "macos" => home.join("Library").join("Application Support").join("ironclaw"),
        "linux" => {
            if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
                let xdg = PathBuf::from(xdg);
                if !xdg.as_os_str().is_empty() {
                    return xdg.join("ironclaw");
                }
            }
            home.join(".local").join("share").join("ironclaw")
        }
        _ => home.join(".ironclaw"),
    }
}

/// Create each required subdirectory under `root`.
pub fn ensure_layout(root: &Path) -> Result<(), StepError> {
    std::fs::create_dir_all(root)?;
    for sub in SUBDIRS {
        std::fs::create_dir_all(root.join(sub))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    #[test]
    fn default_data_dir_macos() {
        let p = default_data_dir_for(Path::new("/home/u"), "macos");
        assert_eq!(
            p,
            PathBuf::from("/home/u/Library/Application Support/ironclaw")
        );
    }

    #[test]
    fn default_data_dir_linux_fallback() {
        // Tests inherit the parent env; guarantee XDG_DATA_HOME is unset
        // by reading and short-circuiting when it's set.
        if std::env::var_os("XDG_DATA_HOME").is_some() {
            // Skip the fallback test rather than touch global env.
            return;
        }
        let p = default_data_dir_for(Path::new("/home/u"), "linux");
        assert_eq!(p, PathBuf::from("/home/u/.local/share/ironclaw"));
    }

    #[test]
    fn default_data_dir_other_os() {
        let p = default_data_dir_for(Path::new("/h"), "freebsd");
        assert_eq!(p, PathBuf::from("/h/.ironclaw"));
    }

    #[test]
    fn ensure_layout_creates_each_subdir() {
        let dir = tempdir().unwrap();
        ensure_layout(dir.path()).unwrap();
        for sub in SUBDIRS {
            assert!(dir.path().join(sub).is_dir(), "missing {sub}");
        }
    }

    #[test]
    fn ensure_layout_is_idempotent() {
        let dir = tempdir().unwrap();
        ensure_layout(dir.path()).unwrap();
        ensure_layout(dir.path()).unwrap();
    }

    #[test]
    fn step_run_uses_default_when_empty() {
        let dir = tempdir().unwrap();
        let prompt = Scripted::new().with("DATA_DIR", dir.path().to_string_lossy());
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let res = DataDirStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        assert_eq!(cfg.data_dir, dir.path());
        assert_eq!(cfg.central_db_path, dir.path().join("data/ironclaw.db"));
    }

    #[test]
    fn step_run_keeps_existing_data_dir() {
        let dir = tempdir().unwrap();
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new();
        let _ = DataDirStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert_eq!(cfg.data_dir, dir.path());
    }

    #[test]
    fn step_metadata() {
        let s = DataDirStep;
        assert_eq!(s.name(), "data_dir");
        assert!(!s.description().is_empty());
        assert!(!s.is_skippable());
    }

    #[test]
    fn subdirs_const_lists_expected_entries() {
        assert!(SUBDIRS.contains(&"data"));
        assert!(SUBDIRS.contains(&"data/sessions"));
        assert!(SUBDIRS.contains(&"groups"));
        assert!(SUBDIRS.contains(&"skills"));
        assert!(SUBDIRS.contains(&"logs"));
    }
}
