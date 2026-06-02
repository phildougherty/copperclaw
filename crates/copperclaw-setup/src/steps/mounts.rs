//! Step 7 — extra mount paths.
//!
//! Operators sometimes want extra host directories bind-mounted into agent
//! containers (e.g. shared data, models). This step takes a comma- or
//! newline-separated list of absolute paths, validates that each exists,
//! and stores them on the setup config.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use std::path::{Path, PathBuf};

/// Step implementation.
#[derive(Debug, Default)]
pub struct MountsStep;

impl Step for MountsStep {
    fn name(&self) -> &'static str {
        "mounts"
    }

    fn description(&self) -> &'static str {
        "Capture extra host paths to bind into agent containers"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let answer = prompt.input(
            "MOUNTS",
            "Extra host paths to mount (comma-separated, blank for none)",
            Some(""),
        )?;
        let paths = parse_mount_list(&answer);
        for p in &paths {
            if !p.exists() {
                return Err(StepError::Other(format!(
                    "mount path does not exist: {}",
                    p.display()
                )));
            }
        }
        cfg.mount_paths.clone_from(&paths);
        let message = if paths.is_empty() {
            "no extra mounts configured".to_string()
        } else {
            format!(
                "configured {} mount(s): {}",
                paths.len(),
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        Ok(StepResult::ok(message))
    }
}

/// Parse a comma- or newline-separated list of mount paths.
#[must_use]
pub fn parse_mount_list(raw: &str) -> Vec<PathBuf> {
    raw.split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Convenience predicate used by tests.
#[must_use]
pub fn all_exist(paths: &[PathBuf]) -> bool {
    paths.iter().all(|p: &PathBuf| Path::new(p).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    #[test]
    fn parse_mount_list_empty() {
        assert!(parse_mount_list("").is_empty());
    }

    #[test]
    fn parse_mount_list_comma() {
        let v = parse_mount_list("/a , /b,/c");
        assert_eq!(
            v,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c")
            ]
        );
    }

    #[test]
    fn parse_mount_list_newline() {
        let v = parse_mount_list("/a\n/b\n");
        assert_eq!(v, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn parse_mount_list_mixed() {
        let v = parse_mount_list("/a,\n/b , /c\n,/d");
        assert_eq!(
            v,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
                PathBuf::from("/d"),
            ]
        );
    }

    #[test]
    fn all_exist_true_when_all_present() {
        let dir = tempdir().unwrap();
        let sub = dir.path().join("x");
        std::fs::create_dir(&sub).unwrap();
        assert!(all_exist(&[sub]));
    }

    #[test]
    fn all_exist_false_when_any_missing() {
        let dir = tempdir().unwrap();
        let present = dir.path().join("present");
        std::fs::create_dir(&present).unwrap();
        assert!(!all_exist(&[
            present,
            PathBuf::from("/definitely/missing/xyz")
        ]));
    }

    #[test]
    fn step_blank_input_records_empty() {
        let s = MountsStep;
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("MOUNTS", "");
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        assert!(cfg.mount_paths.is_empty());
    }

    #[test]
    fn step_records_existing_paths() {
        let s = MountsStep;
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a");
        let p2 = dir.path().join("b");
        std::fs::create_dir(&p1).unwrap();
        std::fs::create_dir(&p2).unwrap();
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("MOUNTS", format!("{},{}", p1.display(), p2.display()));
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        assert_eq!(cfg.mount_paths, vec![p1, p2]);
    }

    #[test]
    fn step_errors_when_path_missing() {
        let s = MountsStep;
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("MOUNTS", "/definitely/missing/xyz");
        let err = s.run(&mut cfg, &prompt, &mut state).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    #[test]
    fn step_metadata() {
        let s = MountsStep;
        assert_eq!(s.name(), "mounts");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
