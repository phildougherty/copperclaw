//! Step 11 — first channel selection.
//!
//! Picks a channel kind to use immediately after setup. The CLI channel is
//! the safe default and works without external credentials. Operators
//! configure other channels via `iclaw` after first start.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};

/// Channels surfaced to the operator at setup time.
pub const KNOWN_CHANNELS: &[&str] = &["cli", "telegram", "slack", "discord"];

/// Default first channel.
pub const DEFAULT_CHANNEL: &str = "cli";

/// Step implementation.
#[derive(Debug, Default)]
pub struct ChannelStep;

impl Step for ChannelStep {
    fn name(&self) -> &'static str {
        "channel"
    }

    fn description(&self) -> &'static str {
        "Pick the first channel to configure"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let answer = prompt.input(
            "FIRST_CHANNEL",
            "First channel (cli|telegram|slack|discord)",
            Some(DEFAULT_CHANNEL),
        )?;
        let channel = answer.trim().to_ascii_lowercase();
        if !is_known_channel(&channel) {
            return Err(StepError::Other(format!(
                "unknown channel `{channel}`; expected one of cli|telegram|slack|discord"
            )));
        }
        cfg.first_channel.clone_from(&channel);
        let mut messages = vec![format!("first channel: {channel}")];
        if channel != "cli" {
            messages.push(
                "configure additional credentials via `iclaw channel ...` after setup".to_string(),
            );
        }
        Ok(StepResult {
            messages,
            config_changed: true,
        })
    }
}

/// Whether `name` is among the channel kinds setup knows about.
#[must_use]
pub fn is_known_channel(name: &str) -> bool {
    KNOWN_CHANNELS.iter().any(|k| *k == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;

    #[test]
    fn known_channels_contains_each() {
        for name in ["cli", "telegram", "slack", "discord"] {
            assert!(is_known_channel(name), "missing {name}");
        }
    }

    #[test]
    fn unknown_channel_rejected() {
        assert!(!is_known_channel("matrix"));
    }

    #[test]
    fn step_default_is_cli() {
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new();
        let res = ChannelStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        assert_eq!(cfg.first_channel, "cli");
    }

    #[test]
    fn step_accepts_non_cli_with_followup_message() {
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("FIRST_CHANNEL", "telegram");
        let res = ChannelStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert_eq!(cfg.first_channel, "telegram");
        assert!(res.messages.iter().any(|m| m.contains("iclaw channel")));
    }

    #[test]
    fn step_rejects_unknown_channel() {
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("FIRST_CHANNEL", "matrix");
        let err = ChannelStep.run(&mut cfg, &prompt, &mut state).unwrap_err();
        match err {
            StepError::Other(msg) => assert!(msg.contains("matrix")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn step_case_insensitive() {
        let mut cfg = SetupConfig::default();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("FIRST_CHANNEL", "Slack");
        ChannelStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert_eq!(cfg.first_channel, "slack");
    }

    #[test]
    fn step_metadata() {
        let s = ChannelStep;
        assert_eq!(s.name(), "channel");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
