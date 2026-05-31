//! Step 5 — optional `OneCLI` Agent Vault wiring.
//!
//! Prompts for a base URL + bearer token and runs
//! [`OneCliClient::ensure_agent`] as a connectivity test. The user may
//! decline and the step records the empty config.

use crate::config::{OneCliConfig, SetupConfig};
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use copperclaw_onecli::OneCliClient;

/// Default agent slug used to verify the vault connection.
pub const DEFAULT_PROBE_SLUG: &str = "copperclaw-host";

/// Step implementation.
#[derive(Debug, Default)]
pub struct OneCliStep;

impl Step for OneCliStep {
    fn name(&self) -> &'static str {
        "onecli"
    }

    fn description(&self) -> &'static str {
        "Configure the OneCLI vault (optional)"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let enabled = prompt.confirm("USE_ONECLI", "Wire up the OneCLI vault now?", false)?;
        if !enabled {
            cfg.onecli = None;
            return Ok(StepResult::noop("OneCLI step skipped"));
        }
        let base_url = prompt.input("ONECLI_BASE_URL", "OneCLI base URL", None)?;
        let token = prompt.secret("ONECLI_TOKEN", "OneCLI bearer token")?;
        let slug = prompt.input(
            "ONECLI_SLUG",
            "Agent slug to probe",
            Some(DEFAULT_PROBE_SLUG),
        )?;

        let probe = run_probe(&base_url, &token, &slug);
        let mut messages = Vec::new();
        match probe {
            Ok(()) => messages.push(format!("OneCLI reachable at {base_url}")),
            Err(e) => messages.push(format!("OneCLI probe failed: {e}")),
        }
        cfg.onecli = Some(OneCliConfig {
            base_url,
            bearer_token: token,
            probe_slug: slug,
        });
        Ok(StepResult {
            messages,
            config_changed: true,
        })
    }
}

/// Issue a `ensure_agent` against the supplied vault as a connectivity
/// check. Drives the async call on the current Tokio runtime.
pub fn run_probe(base_url: &str, token: &str, slug: &str) -> Result<(), StepError> {
    let client = OneCliClient::new(base_url, token)
        .map_err(|e| StepError::Other(format!("onecli client build: {e}")))?;
    let slug = slug.to_string();
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| StepError::Other("no Tokio runtime available for OneCLI probe".into()))?;
    tokio::task::block_in_place(|| {
        handle.block_on(async move {
            client
                .ensure_agent(&slug, None)
                .await
                .map_err(|e| StepError::Other(format!("ensure_agent: {e}")))
        })
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;

    #[test]
    fn step_skipped_when_declined() {
        let s = OneCliStep;
        let prompt = Scripted::new().with("USE_ONECLI", "no");
        let mut cfg = SetupConfig {
            onecli: Some(OneCliConfig {
                base_url: "old".into(),
                bearer_token: "old".into(),
                probe_slug: "old".into(),
            }),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(!res.config_changed);
        assert!(cfg.onecli.is_none());
    }

    #[test]
    fn step_records_config_even_when_probe_fails() {
        let s = OneCliStep;
        let prompt = Scripted::new()
            .with("USE_ONECLI", "yes")
            .with("ONECLI_BASE_URL", "http://127.0.0.1:1")
            .with("ONECLI_TOKEN", "tok")
            .with("ONECLI_SLUG", "host");
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let res = s.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        let stored = cfg.onecli.as_ref().unwrap();
        assert_eq!(stored.base_url, "http://127.0.0.1:1");
        assert_eq!(stored.bearer_token, "tok");
        assert_eq!(stored.probe_slug, "host");
        assert!(res.messages.iter().any(|m| m.contains("probe failed")));
    }

    #[test]
    fn run_probe_without_runtime_errors() {
        let err = run_probe("http://127.0.0.1:1", "tok", "host").unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    #[test]
    fn run_probe_invalid_url_errors() {
        let err = run_probe("not a url", "tok", "host").unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    #[test]
    fn default_probe_slug_constant() {
        assert_eq!(DEFAULT_PROBE_SLUG, "copperclaw-host");
    }

    #[test]
    fn step_metadata() {
        let s = OneCliStep;
        assert_eq!(s.name(), "onecli");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
