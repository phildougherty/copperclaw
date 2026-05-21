//! Step 13 — print instructions for the first chat.
//!
//! Pure-output step: no I/O, no prompts. Reads from the [`SetupConfig`] to
//! produce a tailored set of next-step lines.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};

/// Step implementation.
#[derive(Debug, Default)]
pub struct FirstChatStep;

impl Step for FirstChatStep {
    fn name(&self) -> &'static str {
        "first_chat"
    }

    fn description(&self) -> &'static str {
        "Print first-chat instructions"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        _prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let messages = instructions(cfg);
        Ok(StepResult {
            messages,
            config_changed: false,
        })
    }
}

/// Build the instruction lines that should be shown after setup completes.
///
/// `quickstart_ran` reflects whether the `quickstart_group` step
/// successfully bootstrapped a default cli agent group + wiring. When
/// true, we tell the operator they can chat immediately; when false
/// we point them at `iclaw quickstart cli` as the manual fallback.
#[must_use]
pub fn instructions(cfg: &SetupConfig) -> Vec<String> {
    let mut out: Vec<String> = vec![
        "Setup is complete. To start the host:".to_string(),
        "  ironclaw run".to_string(),
        String::new(),
        "(`ironclaw run` auto-discovers the .env in this install root; the".to_string(),
        "companion `iclaw` resolves the same socket without flags.)".to_string(),
    ];
    match cfg.first_channel.as_str() {
        "cli" => {
            out.push(String::new());
            if cfg.quickstart_group_created {
                out.push("Then, in a second terminal:".to_string());
                out.push("  iclaw chat".to_string());
                out.push(String::new());
                out.push(
                    "A default cli agent group is already wired up — `iclaw chat`"
                        .to_string(),
                );
                out.push("will start a REPL against it.".to_string());
                out.push(String::new());
                out.push(
                    "First-run sanity check: `iclaw doctor` walks every check and"
                        .to_string(),
                );
                out.push("prints a `fix:` line on each non-OK row.".to_string());
            } else {
                out.push("Then, in a second terminal:".to_string());
                out.push("  iclaw quickstart cli --name first".to_string());
                out.push("  iclaw chat".to_string());
                out.push(String::new());
                out.push(
                    "(Or run `iclaw doctor` if anything looks off — it walks every"
                        .to_string(),
                );
                out.push("check and prints a `fix:` line on each non-OK row.)".to_string());
            }
        }
        other => {
            out.push(format!(
                "Then bind the {other} channel — see docs/adding-a-channel.md."
            ));
            out.push(String::new());
            out.push("Run `iclaw doctor` once configured to verify the install.".to_string());
        }
    }
    if !cfg.service_unit_path.as_os_str().is_empty() {
        out.push(format!(
            "Service unit written to {}.",
            cfg.service_unit_path.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use std::path::PathBuf;

    #[test]
    fn instructions_cli_channel() {
        let cfg = SetupConfig {
            data_dir: PathBuf::from("/srv/i"),
            first_channel: "cli".into(),
            ..SetupConfig::default()
        };
        let out = instructions(&cfg);
        // Points at the real binary (not a non-existent
        // `ironclaw run --data-dir` form). Auto-discovery means no
        // `--env-file` flag. When the quickstart_group step didn't
        // run, we expect a fallback `iclaw quickstart cli` line.
        assert!(out.iter().any(|m| m.trim() == "ironclaw run"));
        assert!(out.iter().any(|m| m.contains("iclaw quickstart cli")));
        // `iclaw doctor` is always referenced in the cli path so
        // first-run users find the diagnostic command without
        // searching docs.
        assert!(out.iter().any(|m| m.contains("iclaw doctor")));
    }

    #[test]
    fn instructions_after_quickstart_say_chat_directly() {
        let cfg = SetupConfig {
            first_channel: "cli".into(),
            quickstart_group_created: true,
            ..SetupConfig::default()
        };
        let out = instructions(&cfg);
        assert!(out.iter().any(|m| m.trim() == "ironclaw run"));
        assert!(out.iter().any(|m| m.trim() == "iclaw chat"));
        // When the default group is already in place we should NOT
        // tell the user to run quickstart again.
        assert!(!out.iter().any(|m| m.contains("iclaw quickstart cli")));
        assert!(out.iter().any(|m| m.contains("iclaw doctor")));
    }

    #[test]
    fn instructions_telegram_points_at_docs() {
        let cfg = SetupConfig {
            first_channel: "telegram".into(),
            ..SetupConfig::default()
        };
        let out = instructions(&cfg);
        assert!(out.iter().any(|m| m.contains("docs/adding-a-channel.md")));
    }

    #[test]
    fn instructions_mention_service_unit_when_present() {
        let cfg = SetupConfig {
            service_unit_path: PathBuf::from("/x/ironclaw.service"),
            first_channel: "cli".into(),
            ..SetupConfig::default()
        };
        let out = instructions(&cfg);
        assert!(out.iter().any(|m| m.contains("/x/ironclaw.service")));
    }

    #[test]
    fn step_run_returns_instruction_lines() {
        let mut cfg = SetupConfig {
            first_channel: "cli".into(),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new();
        let res = FirstChatStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(!res.config_changed);
        assert!(!res.messages.is_empty());
    }

    #[test]
    fn step_metadata() {
        let s = FirstChatStep;
        assert_eq!(s.name(), "first_chat");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
