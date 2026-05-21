//! Step trait + registry.
//!
//! Each public submodule defines one step. The driver in [`crate::cli`]
//! walks `all_steps()` in order, asks the [`Prompt`] for any inputs the
//! step needs, applies the resulting [`StepResult`] to a [`SetupConfig`],
//! and writes the updated [`SetupState`] back to disk.

use crate::config::SetupConfig;
use crate::prompt::{Prompt, PromptError};
use crate::state::SetupState;
use std::path::Path;

pub mod auth;
pub mod central_db;
pub mod channel;
pub mod cli_agent;
pub mod data_dir;
pub mod env_check;
pub mod first_chat;
pub mod image;
pub mod mounts;
pub mod onecli;
pub mod service_unit;
pub mod telegram;
pub mod timezone;
pub mod verify;

/// What a step reports back when it finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    /// Lines to print to the operator.
    pub messages: Vec<String>,
    /// Whether the step modified the persisted config (driver should save).
    pub config_changed: bool,
}

impl StepResult {
    /// Convenience: a single-message result that did update the config.
    #[must_use]
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            messages: vec![message.into()],
            config_changed: true,
        }
    }

    /// Convenience: noop result with a single status message.
    #[must_use]
    pub fn noop(message: impl Into<String>) -> Self {
        Self {
            messages: vec![message.into()],
            config_changed: false,
        }
    }
}

/// Errors a step can return. All step errors are wrapped here.
#[derive(Debug, thiserror::Error)]
pub enum StepError {
    /// Prompt subsystem failed (missing env var, etc.).
    #[error(transparent)]
    Prompt(#[from] PromptError),
    /// Filesystem error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization / deserialization error.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Anything the step considers fatal but doesn't fit elsewhere.
    #[error("{0}")]
    Other(String),
}

/// Lifecycle of a single setup step.
pub trait Step: Send + Sync {
    /// Stable name. Must be unique. Used as the persisted-step key.
    fn name(&self) -> &'static str;

    /// Human-readable label shown to the operator.
    fn description(&self) -> &'static str;

    /// Whether the step can be safely skipped via `--skip-step`.
    fn is_skippable(&self) -> bool {
        true
    }

    /// Execute the step.
    ///
    /// Steps receive a mutable [`SetupConfig`] so they can record their
    /// outputs, plus a [`Prompt`] for any required inputs.
    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        state: &mut SetupState,
    ) -> Result<StepResult, StepError>;
}

/// Box-helper used by the registry.
pub type BoxedStep = Box<dyn Step>;

/// All steps in the canonical order they run.
#[must_use]
pub fn all_steps() -> Vec<BoxedStep> {
    vec![
        Box::new(env_check::EnvCheckStep),
        Box::new(data_dir::DataDirStep),
        Box::new(central_db::CentralDbStep),
        Box::new(image::ImageBuildStep),
        Box::new(onecli::OneCliStep),
        Box::new(auth::AuthStep),
        Box::new(mounts::MountsStep),
        Box::new(service_unit::ServiceUnitStep),
        Box::new(cli_agent::CliAgentStep),
        Box::new(timezone::TimezoneStep),
        Box::new(channel::ChannelStep),
        Box::new(verify::VerifyStep),
        Box::new(first_chat::FirstChatStep),
    ]
}

/// Look up a step by name. Returns `None` when the name is unknown.
#[must_use]
pub fn step_by_name(name: &str) -> Option<BoxedStep> {
    all_steps().into_iter().find(|s| s.name() == name)
}

/// Check whether a tool is on `PATH` by scanning `$PATH` entries.
///
/// Pure-function (avoids spawning subprocesses); listed here so several
/// steps can share it.
#[must_use]
pub fn binary_on_path(name: &str) -> bool {
    let Some(path_env) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_env).any(|dir| has_executable(&dir, name))
}

fn has_executable(dir: &Path, name: &str) -> bool {
    let candidate = dir.join(name);
    candidate.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_result_ok_records_message() {
        let r = StepResult::ok("hi");
        assert_eq!(r.messages, vec!["hi".to_string()]);
        assert!(r.config_changed);
    }

    #[test]
    fn step_result_noop_marks_unchanged() {
        let r = StepResult::noop("skipped");
        assert_eq!(r.messages, vec!["skipped".to_string()]);
        assert!(!r.config_changed);
    }

    #[test]
    fn step_error_display() {
        let e = StepError::Other("boom".into());
        assert_eq!(e.to_string(), "boom");
    }

    #[test]
    fn step_error_from_io() {
        let e: StepError = std::io::Error::other("x").into();
        assert!(matches!(e, StepError::Io(_)));
    }

    #[test]
    fn step_error_from_prompt() {
        let e: StepError = PromptError::Missing("X".into()).into();
        assert!(matches!(e, StepError::Prompt(_)));
    }

    #[test]
    fn step_error_from_json() {
        let bad: serde_json::Error = serde_json::from_str::<u32>("not").unwrap_err();
        let e: StepError = bad.into();
        assert!(matches!(e, StepError::Json(_)));
    }

    #[test]
    fn all_steps_has_thirteen_in_order() {
        let names: Vec<&'static str> = all_steps().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "env_check",
                "data_dir",
                "central_db",
                "image",
                "onecli",
                "auth",
                "mounts",
                "service_unit",
                "cli_agent",
                "timezone",
                "channel",
                "verify",
                "first_chat",
            ]
        );
    }

    #[test]
    fn step_by_name_returns_some_for_known() {
        let s = step_by_name("env_check").unwrap();
        assert_eq!(s.name(), "env_check");
    }

    #[test]
    fn step_by_name_returns_none_for_unknown() {
        assert!(step_by_name("nope").is_none());
    }

    #[test]
    fn binary_on_path_finds_known_tool() {
        // `sh` should be on POSIX systems used in CI / dev hosts.
        assert!(binary_on_path("sh"));
    }

    #[test]
    fn binary_on_path_missing_returns_false() {
        assert!(!binary_on_path("definitely-not-on-path-xyz-1234567890"));
    }

    #[test]
    fn has_executable_returns_false_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_executable(dir.path(), "absent"));
    }

    #[test]
    fn has_executable_returns_true_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let exec_path = dir.path().join("tool");
        std::fs::write(&exec_path, b"").unwrap();
        assert!(has_executable(dir.path(), "tool"));
    }
}
