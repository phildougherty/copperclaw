//! Step 1 — environment check.
//!
//! Probes `PATH` for the toolchain copperclaw expects: a container runtime
//! (`docker` or `container`), `git`, optionally `gh`, and the `cclaw` CLI.

use crate::config::{EnvReport, SetupConfig};
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult, binary_on_path};

/// Step implementation.
#[derive(Debug, Default)]
pub struct EnvCheckStep;

impl Step for EnvCheckStep {
    fn name(&self) -> &'static str {
        "env_check"
    }

    fn description(&self) -> &'static str {
        "Verify expected toolchain is on PATH"
    }

    fn is_skippable(&self) -> bool {
        false
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        _prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let report = detect();
        let mut messages = vec![format_report(&report)];
        if !report.has_container_runtime() {
            messages.push(
                "WARNING: no container runtime detected. Install docker or Apple container."
                    .to_string(),
            );
        }
        if !report.has_git {
            messages.push("WARNING: git is not on PATH.".to_string());
        }
        cfg.env_report = report;
        Ok(StepResult {
            messages,
            config_changed: true,
        })
    }
}

/// Probe each expected binary and build an [`EnvReport`].
#[must_use]
pub fn detect() -> EnvReport {
    EnvReport {
        has_docker: binary_on_path("docker"),
        has_apple_container: binary_on_path("container"),
        has_git: binary_on_path("git"),
        has_gh: binary_on_path("gh"),
        has_ncl: binary_on_path("cclaw"),
    }
}

/// Format a human report.
#[must_use]
pub fn format_report(r: &EnvReport) -> String {
    let mark = |b: bool| if b { "yes" } else { "no" };
    format!(
        "environment check:\n  docker: {}\n  container (apple): {}\n  git: {}\n  gh: {}\n  cclaw: {}",
        mark(r.has_docker),
        mark(r.has_apple_container),
        mark(r.has_git),
        mark(r.has_gh),
        mark(r.has_ncl),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;

    #[test]
    fn detect_returns_struct() {
        let _r = detect();
    }

    #[test]
    fn format_report_lists_all_tools() {
        let r = EnvReport {
            has_docker: true,
            has_apple_container: false,
            has_git: true,
            has_gh: false,
            has_ncl: true,
        };
        let s = format_report(&r);
        assert!(s.contains("docker: yes"));
        assert!(s.contains("container (apple): no"));
        assert!(s.contains("git: yes"));
        assert!(s.contains("gh: no"));
        assert!(s.contains("cclaw: yes"));
    }

    #[test]
    fn step_name_and_description() {
        let s = EnvCheckStep;
        assert_eq!(s.name(), "env_check");
        assert!(!s.description().is_empty());
        assert!(!s.is_skippable());
    }

    #[test]
    fn step_run_records_report() {
        let s = EnvCheckStep;
        let mut cfg = SetupConfig::default();
        let mut st = SetupState::new();
        let prompt = Scripted::new();
        let res = s.run(&mut cfg, &prompt, &mut st).unwrap();
        assert!(res.config_changed);
        assert!(!res.messages.is_empty());
    }
}
