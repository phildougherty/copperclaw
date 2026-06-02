//! Step 9 — verify the `cclaw` CLI agent is on `PATH`.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult, binary_on_path};

/// Step implementation.
#[derive(Debug, Default)]
pub struct CliAgentStep;

impl Step for CliAgentStep {
    fn name(&self) -> &'static str {
        "cli_agent"
    }

    fn description(&self) -> &'static str {
        "Verify the cclaw CLI agent is on PATH"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        _prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let present = binary_on_path("cclaw");
        cfg.env_report.has_ncl = present;
        let message = if present {
            "cclaw: found on PATH".to_string()
        } else {
            "cclaw: NOT found on PATH (the CLI channel will be unavailable until installed)"
                .to_string()
        };
        Ok(StepResult {
            messages: vec![message],
            config_changed: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;

    #[test]
    fn step_records_presence() {
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new();
        let res = CliAgentStep.run(&mut cfg, &prompt, &mut state).unwrap();
        // We don't assert true/false here — depends on host PATH. We just
        // verify the result is well-formed.
        assert!(res.config_changed);
        assert_eq!(cfg.env_report.has_ncl, binary_on_path("cclaw"));
    }

    #[test]
    fn step_metadata() {
        let s = CliAgentStep;
        assert_eq!(s.name(), "cli_agent");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
