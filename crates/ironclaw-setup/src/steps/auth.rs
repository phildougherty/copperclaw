//! Step 6 — Anthropic API key capture.
//!
//! Reads `ANTHROPIC_API_KEY` from the environment or prompts for it, then
//! writes a `.env` file inside the data directory with mode `0o600`.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use std::path::Path;

/// Step implementation.
#[derive(Debug, Default)]
pub struct AuthStep;

impl Step for AuthStep {
    fn name(&self) -> &'static str {
        "auth"
    }

    fn description(&self) -> &'static str {
        "Capture and persist the Anthropic API key"
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
        let key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => prompt.secret("ANTHROPIC_API_KEY", "Anthropic API key")?,
        };
        let env_path = cfg.data_dir.join(".env");
        write_env_file(&env_path, &key)?;
        cfg.env_file.clone_from(&env_path);
        Ok(StepResult::ok(format!(
            "wrote {} (0600)",
            env_path.display()
        )))
    }
}

/// Write `key=value` lines into a `.env` file at `path` and chmod it to
/// `0o600` on Unix.
pub fn write_env_file(path: &Path, anthropic_api_key: &str) -> Result<(), StepError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = render_env_file(anthropic_api_key);
    std::fs::write(path, contents)?;
    restrict_permissions(path)?;
    Ok(())
}

/// Render the `.env` body for `key`.
#[must_use]
pub fn render_env_file(anthropic_api_key: &str) -> String {
    format!("ANTHROPIC_API_KEY={anthropic_api_key}\n")
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), StepError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<(), StepError> {
    // Non-Unix platforms don't have the same mode bits; treat as a no-op.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    #[test]
    fn render_env_file_includes_key() {
        let s = render_env_file("sk-abc");
        assert_eq!(s, "ANTHROPIC_API_KEY=sk-abc\n");
    }

    #[test]
    fn write_env_file_creates_with_restricted_perms() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".env");
        write_env_file(&path, "sk-xyz").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("ANTHROPIC_API_KEY=sk-xyz"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn write_env_file_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("nested/.env");
        write_env_file(&nested, "sk-1").unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn step_prompts_when_env_var_missing() {
        // The harness's parent env shouldn't define ANTHROPIC_API_KEY for
        // this test; if it does, exit early without flagging a failure.
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            return;
        }
        let dir = tempdir().unwrap();
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("ANTHROPIC_API_KEY", "sk-from-prompt");
        let res = AuthStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        let written = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(written.contains("sk-from-prompt"));
    }

    #[test]
    fn step_metadata() {
        let s = AuthStep;
        assert_eq!(s.name(), "auth");
        assert!(!s.description().is_empty());
        assert!(!s.is_skippable());
    }
}
