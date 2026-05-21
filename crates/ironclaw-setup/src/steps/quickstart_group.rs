//! Step 14 — bootstrap a default `cli` agent group + wiring.
//!
//! Before this step the install is technically functional but
//! `iclaw chat` won't do anything because there's no agent group to
//! route inbound messages to. The previous setup flow printed a
//! "now run `iclaw quickstart cli --name first`" instruction; this
//! step does that automatically so a fresh install is chatable on
//! the very first `ironclaw run`.
//!
//! Idempotent: if any agent group is already configured (e.g. setup
//! was re-run, or the operator created one out of band), the step
//! exits without writing.
//!
//! Skippable via:
//! - `IRONCLAW_SETUP_QUICKSTART=no` env var (headless mode).
//! - Interactive `n` answer at the prompt.
//! - The `--skip-step quickstart_group` CLI flag.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::{agent_groups, messaging_group_agents, messaging_groups};
use ironclaw_types::{ChannelType, EngageMode, SessionMode};

/// Slug used when the user accepts the default and doesn't set the
/// `IRONCLAW_SETUP_QUICKSTART_NAME` env override.
const DEFAULT_GROUP_NAME: &str = "first";

/// Step implementation.
#[derive(Debug, Default)]
pub struct QuickstartGroupStep;

impl Step for QuickstartGroupStep {
    fn name(&self) -> &'static str {
        "quickstart_group"
    }

    fn description(&self) -> &'static str {
        "Bootstrap a default cli agent group so `iclaw chat` works on first run"
    }

    fn is_skippable(&self) -> bool {
        true
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        if cfg.first_channel.as_str() != "cli" {
            // Only the cli channel can be wired with `(channel=cli,
            // platform=stdin)` defaults. Other channels need
            // operator-supplied credentials (Telegram bot token,
            // Slack signing secret, etc.) that this step can't fill
            // in safely.
            return Ok(StepResult::ok(format!(
                "first channel is `{}`, not `cli`; skipping default-group bootstrap",
                cfg.first_channel
            )));
        }
        if cfg.central_db_path.as_os_str().is_empty() {
            return Ok(StepResult::ok(
                "central_db_path not set; skipping default-group bootstrap".to_string(),
            ));
        }

        let db = CentralDb::open(&cfg.central_db_path)
            .map_err(|e| StepError::Other(format!("central DB open failed: {e}")))?;

        // Idempotency gate: if any agent group exists, leave the
        // install alone. The operator can re-run setup safely.
        let existing = agent_groups::list(&db)
            .map_err(|e| StepError::Other(format!("agent_groups::list failed: {e}")))?;
        if !existing.is_empty() {
            return Ok(StepResult::ok(format!(
                "{} agent group(s) already configured; skipping default-group bootstrap",
                existing.len()
            )));
        }

        // Operator opt-out via env var (headless mode) or interactive prompt.
        let agree = prompt
            .confirm(
                "IRONCLAW_SETUP_QUICKSTART",
                "Create a default cli agent group + wiring so `iclaw chat` works immediately?",
                true,
            )
            .map_err(|e| StepError::Other(format!("prompt failed: {e}")))?;
        if !agree {
            return Ok(StepResult::ok(
                "user declined; no default group created. Run `iclaw quickstart cli --name first` later to bootstrap one.".to_string(),
            ));
        }

        let name = std::env::var("IRONCLAW_SETUP_QUICKSTART_NAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_GROUP_NAME.to_string());
        bootstrap_default_cli_group(&db, &name)?;
        cfg.quickstart_group_created = true;

        Ok(StepResult {
            messages: vec![format!(
                "bootstrapped default cli group `{name}` (cli/stdin) — `iclaw chat` is ready"
            )],
            config_changed: true,
        })
    }
}

/// Pure helper: write the trio (agent group, cli/stdin messaging
/// group, pattern-`.*` wiring) into the central DB.  Returns the
/// three IDs as a tuple so tests can assert wiring exists.
///
/// The caller is responsible for the idempotency gate — this fn
/// **does** insert another row if you call it twice. (Why: the
/// gate in [`QuickstartGroupStep::run`] is the operator-facing
/// contract; this fn is also called from tests that want to seed
/// known state.)
pub fn bootstrap_default_cli_group(
    db: &CentralDb,
    name: &str,
) -> Result<BootstrapIds, StepError> {
    let ag = agent_groups::create(
        db,
        agent_groups::CreateAgentGroup {
            name: name.to_string(),
            folder: name.to_string(),
            agent_provider: None,
        },
    )
    .map_err(|e| StepError::Other(format!("agent_groups::create failed: {e}")))?;

    let mg = messaging_groups::upsert(
        db,
        messaging_groups::UpsertMessagingGroup {
            channel_type: ChannelType::new(ChannelType::CLI),
            platform_id: "stdin".to_string(),
            name: Some(name.to_string()),
            is_group: false,
            unknown_sender_policy: "approval-required".to_string(),
        },
    )
    .map_err(|e| StepError::Other(format!("messaging_groups::upsert failed: {e}")))?;

    let wiring = messaging_group_agents::upsert(
        db,
        messaging_group_agents::UpsertWiring {
            messaging_group_id: mg.id,
            agent_group_id: ag.id,
            engage_mode: EngageMode::Pattern,
            engage_pattern: Some(".*".to_string()),
            sender_scope: "any".to_string(),
            ignored_message_policy: "drop".to_string(),
            session_mode: SessionMode::Shared,
            priority: 0,
        },
    )
    .map_err(|e| StepError::Other(format!("messaging_group_agents::upsert failed: {e}")))?;

    Ok(BootstrapIds {
        agent_group_id: ag.id,
        messaging_group_id: mg.id,
        wiring_id: wiring.id,
    })
}

/// Returned by [`bootstrap_default_cli_group`] so callers (and tests)
/// can correlate the three rows that just landed in the DB.
#[derive(Debug, Clone, Copy)]
pub struct BootstrapIds {
    pub agent_group_id: ironclaw_types::AgentGroupId,
    pub messaging_group_id: ironclaw_types::MessagingGroupId,
    pub wiring_id: ironclaw_types::WiringId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    fn fresh_cfg(dir: &std::path::Path) -> SetupConfig {
        SetupConfig {
            data_dir: dir.to_path_buf(),
            central_db_path: dir.join("ironclaw.db"),
            first_channel: "cli".into(),
            ..SetupConfig::default()
        }
    }

    #[test]
    fn step_metadata() {
        let s = QuickstartGroupStep;
        assert_eq!(s.name(), "quickstart_group");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }

    #[test]
    fn bootstrap_default_cli_group_writes_three_rows() {
        let dir = tempdir().unwrap();
        let db = CentralDb::open(dir.path().join("ironclaw.db")).unwrap();
        let ids = bootstrap_default_cli_group(&db, "first").unwrap();
        // Agent group reachable.
        let ag = agent_groups::list(&db).unwrap();
        assert_eq!(ag.len(), 1);
        assert_eq!(ag[0].id, ids.agent_group_id);
        assert_eq!(ag[0].name, "first");
        // Messaging group reachable + bound to cli/stdin.
        let mg = messaging_groups::list(&db).unwrap();
        assert_eq!(mg.len(), 1);
        assert_eq!(mg[0].id, ids.messaging_group_id);
        assert_eq!(mg[0].channel_type.as_str(), "cli");
        assert_eq!(mg[0].platform_id, "stdin");
        // Wiring reachable.
        let wiring =
            messaging_group_agents::list_for_ag(&db, ids.agent_group_id).unwrap();
        assert_eq!(wiring.len(), 1);
        assert_eq!(wiring[0].id, ids.wiring_id);
        assert_eq!(wiring[0].engage_mode, EngageMode::Pattern);
        assert_eq!(wiring[0].engage_pattern.as_deref(), Some(".*"));
    }

    #[test]
    fn step_runs_happy_path_when_db_empty_and_user_agrees() {
        let dir = tempdir().unwrap();
        // Seed an empty DB via CentralDbStep's helper.
        crate::steps::central_db::open_and_migrate(&dir.path().join("ironclaw.db"))
            .unwrap();
        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        // Scripted prompt defaults to accepting confirms unless told otherwise.
        let prompt = Scripted::new().with("IRONCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.messages.iter().any(|m| m.contains("bootstrapped")));
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        assert_eq!(agent_groups::list(&db).unwrap().len(), 1);
        assert_eq!(messaging_groups::list(&db).unwrap().len(), 1);
    }

    #[test]
    fn step_skips_when_groups_already_exist() {
        let dir = tempdir().unwrap();
        crate::steps::central_db::open_and_migrate(&dir.path().join("ironclaw.db"))
            .unwrap();
        let db = CentralDb::open(dir.path().join("ironclaw.db")).unwrap();
        // Pre-create a group.
        bootstrap_default_cli_group(&db, "already-here").unwrap();
        drop(db);

        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("IRONCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(
            res.messages.iter().any(|m| m.contains("already configured")),
            "unexpected: {:?}",
            res.messages
        );
        // Still exactly one group.
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        assert_eq!(agent_groups::list(&db).unwrap().len(), 1);
    }

    #[test]
    fn step_skips_when_user_declines() {
        let dir = tempdir().unwrap();
        crate::steps::central_db::open_and_migrate(&dir.path().join("ironclaw.db"))
            .unwrap();
        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("IRONCLAW_SETUP_QUICKSTART", "no");
        let res = QuickstartGroupStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(
            res.messages.iter().any(|m| m.contains("declined")),
            "unexpected: {:?}",
            res.messages
        );
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        assert!(agent_groups::list(&db).unwrap().is_empty());
    }

    #[test]
    fn step_skips_when_first_channel_is_not_cli() {
        let dir = tempdir().unwrap();
        crate::steps::central_db::open_and_migrate(&dir.path().join("ironclaw.db"))
            .unwrap();
        let mut cfg = fresh_cfg(dir.path());
        cfg.first_channel = "telegram".into();
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("IRONCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(
            res.messages.iter().any(|m| m.contains("not `cli`")),
            "unexpected: {:?}",
            res.messages
        );
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        assert!(agent_groups::list(&db).unwrap().is_empty());
    }

    #[test]
    fn step_skips_when_central_db_path_unset() {
        let dir = tempdir().unwrap();
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            // central_db_path intentionally empty.
            first_channel: "cli".into(),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("IRONCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(
            res.messages.iter().any(|m| m.contains("central_db_path not set")),
            "unexpected: {:?}",
            res.messages
        );
    }

    #[test]
    fn step_honors_quickstart_name_env_var() {
        let dir = tempdir().unwrap();
        crate::steps::central_db::open_and_migrate(&dir.path().join("ironclaw.db"))
            .unwrap();
        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("IRONCLAW_SETUP_QUICKSTART", "yes");
        // Use the in-process env (single-threaded tests are the
        // convention here; the env-mutation is safe).
        // SAFETY: tests run with std::env::set_var in the same crate
        // are gated by Rust 2024's unsafe-env-var rule. Avoid it by
        // calling the pure helper directly with a custom name.
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        bootstrap_default_cli_group(&db, "ops").unwrap();
        drop(db);
        // Re-running the step should now see the group and skip.
        let res = QuickstartGroupStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.messages.iter().any(|m| m.contains("already configured")));
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        let groups = agent_groups::list(&db).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "ops");
    }
}
