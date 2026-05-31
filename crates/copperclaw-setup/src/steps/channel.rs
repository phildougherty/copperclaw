//! Step 11 — first channel selection.
//!
//! Picks a channel kind to use immediately after setup. The CLI channel is
//! the safe default and works without external credentials. When the
//! operator picks `telegram` the step also runs the interactive
//! [`super::telegram`] pairing wizard (`BotFather` walkthrough,
//! `getMe` verification, optional chat-id capture, `.env` persistence).
//! Other non-`cli` channels still defer to manual `cclaw channel ...`
//! configuration.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::telegram::{
    self, PairingOutcome, TELEGRAM_BOT_TOKEN_ENV, TELEGRAM_CHAT_ID_ENV,
};
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

        match channel.as_str() {
            "telegram" => {
                run_telegram_pairing(cfg, prompt, &mut messages)?;
            }
            "cli" => {}
            // TODO(team-d): generalize for other channels — Slack /
            // Discord pairing wizards plug in here.
            _ => {
                messages.push(
                    "configure additional credentials via `cclaw channel ...` after setup"
                        .to_string(),
                );
            }
        }

        Ok(StepResult {
            messages,
            config_changed: true,
        })
    }
}

/// Drive the Telegram pairing wizard and persist its outputs.
fn run_telegram_pairing(
    cfg: &SetupConfig,
    prompt: &dyn Prompt,
    messages: &mut Vec<String>,
) -> Result<Option<PairingOutcome>, StepError> {
    let outcome = telegram::run_pairing(prompt, telegram::DEFAULT_API_BASE, messages)?;
    let Some(outcome) = outcome else {
        // Operator typed `skip` — leave the .env untouched. Tell them
        // they can wire it later via `cclaw channel ...`.
        return Ok(None);
    };

    let env_path = if cfg.env_file.as_os_str().is_empty() {
        cfg.data_dir.join(".env")
    } else {
        cfg.env_file.clone()
    };
    telegram::append_env_var(&env_path, TELEGRAM_BOT_TOKEN_ENV, &outcome.token)?;
    messages.push(format!(
        "wrote {TELEGRAM_BOT_TOKEN_ENV} to {} (redacted: {})",
        env_path.display(),
        telegram::redact_token(&outcome.token)
    ));
    if let Some(chat_id) = outcome.chat_id {
        telegram::append_env_var(&env_path, TELEGRAM_CHAT_ID_ENV, &chat_id.to_string())?;
        messages.push(format!("wrote {TELEGRAM_CHAT_ID_ENV}={chat_id}"));
    }
    Ok(Some(outcome))
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
    use std::path::PathBuf;
    use tempfile::tempdir;

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
        let prompt = Scripted::new().with("FIRST_CHANNEL", "slack");
        let res = ChannelStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert_eq!(cfg.first_channel, "slack");
        assert!(res.messages.iter().any(|m| m.contains("cclaw channel")));
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

    #[test]
    fn step_telegram_skip_does_not_touch_env_file() {
        // Pick telegram, then immediately skip the pairing wizard. The
        // .env file should not be created / modified.
        let dir = tempdir().unwrap();
        let env_path = dir.path().join(".env");
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            env_file: PathBuf::new(),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new()
            .with("FIRST_CHANNEL", "telegram")
            .with(super::telegram::HEADLESS_TOKEN_KEY, "skip");
        let res = ChannelStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert_eq!(cfg.first_channel, "telegram");
        assert!(res.messages.iter().any(|m| m.contains("skipped")));
        assert!(!env_path.exists());
    }

    // run_telegram_pairing happy path requires HTTP — covered in
    // telegram.rs tests. Here we only verify the step calls the wizard
    // when channel=telegram and the user opts to skip.
}
