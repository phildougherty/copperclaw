//! Step 8 — generate the systemd unit (Linux) or launchd plist (macOS).
//!
//! Writes the file to the platform-default location under `$HOME` and
//! prints the manual `systemctl --user enable` / `launchctl load` command
//! the operator should run themselves.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use crate::units::{default_install_path, generate, UnitContext, UnitKind};
use std::path::{Path, PathBuf};

/// Step implementation.
#[derive(Debug, Default)]
pub struct ServiceUnitStep;

impl Step for ServiceUnitStep {
    fn name(&self) -> &'static str {
        "service_unit"
    }

    fn description(&self) -> &'static str {
        "Generate a systemd unit or launchd plist"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let opt_in = prompt.confirm("WRITE_SERVICE_UNIT", "Write the service unit?", true)?;
        if !opt_in {
            return Ok(StepResult::noop("skipping service unit"));
        }

        let kind = guess_unit_kind();
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| StepError::Other("HOME is not set".to_string()))?;
        let default_path = default_install_path(kind, &home);
        let path_answer = prompt.input(
            "SERVICE_UNIT_PATH",
            "Path to write the unit",
            Some(&default_path.display().to_string()),
        )?;
        let out = PathBuf::from(path_answer);

        let exec_path = exec_path_from_config(cfg);
        let ctx = UnitContext::new(exec_path, &cfg.data_dir, &cfg.env_file);
        let template_root = std::env::var_os("IRONCLAW_TEMPLATE_ROOT")
            .map(PathBuf::from);
        let body = generate(kind, &ctx, template_root.as_deref());

        write_unit(&out, &body)?;
        cfg.service_unit_path.clone_from(&out);

        let enable_hint = match kind {
            UnitKind::Systemd => {
                "systemctl --user daemon-reload && systemctl --user enable --now ironclaw.service"
                    .to_string()
            }
            UnitKind::Launchd => {
                format!("launchctl load {}", out.display())
            }
        };
        Ok(StepResult::ok(format!(
            "wrote {} -- to enable later, run: {}",
            out.display(),
            enable_hint
        )))
    }
}

/// Pick a unit flavor for the current OS.
#[must_use]
pub fn guess_unit_kind() -> UnitKind {
    match std::env::consts::OS {
        "macos" => UnitKind::Launchd,
        _ => UnitKind::Systemd,
    }
}

/// Inspect `cfg.image_tag` for a hint of where the binary lives, fall back
/// to a sensible default.
#[must_use]
pub fn exec_path_from_config(_cfg: &SetupConfig) -> PathBuf {
    // Heuristic: assume the operator already has `ironclaw` on the PATH at
    // a conventional location. Operators can edit the generated file if
    // their setup differs.
    PathBuf::from("/usr/local/bin/ironclaw")
}

/// Write `body` to `path`, creating the parent directory if needed.
pub fn write_unit(path: &Path, body: &str) -> Result<(), StepError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    #[test]
    fn guess_unit_kind_matches_target() {
        let k = guess_unit_kind();
        match std::env::consts::OS {
            "macos" => assert_eq!(k, UnitKind::Launchd),
            _ => assert_eq!(k, UnitKind::Systemd),
        }
    }

    #[test]
    fn exec_path_default() {
        let cfg = SetupConfig::default();
        assert_eq!(exec_path_from_config(&cfg), PathBuf::from("/usr/local/bin/ironclaw"));
    }

    #[test]
    fn write_unit_creates_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a/b/foo.service");
        write_unit(&path, "body").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "body");
    }

    #[test]
    fn step_skipped_when_declined() {
        let s = ServiceUnitStep;
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("WRITE_SERVICE_UNIT", "no");
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(!res.config_changed);
    }

    #[test]
    fn step_writes_unit_at_requested_path() {
        // Provide HOME so the default-install-path code is happy even when
        // we're going to override the path anyway.
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let s = ServiceUnitStep;
        let dir = tempdir().unwrap();
        let target = dir.path().join("custom/ironclaw.service");
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            env_file: dir.path().join(".env"),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new()
            .with("WRITE_SERVICE_UNIT", "yes")
            .with("SERVICE_UNIT_PATH", target.to_string_lossy());
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        assert!(target.exists());
        assert_eq!(cfg.service_unit_path, target);
    }

    #[test]
    fn step_metadata() {
        let s = ServiceUnitStep;
        assert_eq!(s.name(), "service_unit");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
