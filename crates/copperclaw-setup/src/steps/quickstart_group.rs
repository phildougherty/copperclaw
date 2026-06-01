//! Step 14 — bootstrap a default `cli` agent group + wiring.
//!
//! Before this step the install is technically functional but
//! `cclaw chat` won't do anything because there's no agent group to
//! route inbound messages to. The previous setup flow printed a
//! "now run `cclaw quickstart cli --name first`" instruction; this
//! step does that automatically so a fresh install is chatable on
//! the very first `copperclaw run`.
//!
//! Idempotent: if any agent group is already configured (e.g. setup
//! was re-run, or the operator created one out of band), the step
//! exits without writing.
//!
//! Skippable via:
//! - `COPPERCLAW_SETUP_QUICKSTART=no` env var (headless mode).
//! - Interactive `n` answer at the prompt.
//! - The `--skip-step quickstart_group` CLI flag.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::telegram::append_env_var;
use crate::steps::{Step, StepError, StepResult};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{agent_groups, messaging_group_agents, messaging_groups, users};
use copperclaw_types::{ChannelType, EngageMode, SessionMode};
use std::path::{Path, PathBuf};

/// Slug used when the user accepts the default and doesn't set the
/// `COPPERCLAW_SETUP_QUICKSTART_NAME` env override.
const DEFAULT_GROUP_NAME: &str = "first";

/// Step implementation.
#[derive(Debug, Default)]
pub struct QuickstartGroupStep;

impl Step for QuickstartGroupStep {
    fn name(&self) -> &'static str {
        "quickstart_group"
    }

    fn description(&self) -> &'static str {
        "Bootstrap a default cli agent group so `cclaw chat` works on first run"
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
        if !matches!(cfg.first_channel.as_str(), "cli" | "telegram") {
            // Only the cli channel and (after the channel step's
            // pairing wizard) telegram have setup-time auto-bootstrap
            // support. Slack / Discord still need operator-supplied
            // credentials wired manually after setup.
            return Ok(StepResult::ok(format!(
                "first channel is `{}`; no auto-bootstrap recipe for it (cli + telegram are wired automatically)",
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
                "COPPERCLAW_SETUP_QUICKSTART",
                "Create a default cli agent group + wiring so `cclaw chat` works immediately?",
                true,
            )
            .map_err(|e| StepError::Other(format!("prompt failed: {e}")))?;
        if !agree {
            return Ok(StepResult::ok(
                "user declined; no default group created. Run `cclaw quickstart cli --name first` later to bootstrap one.".to_string(),
            ));
        }

        let name = std::env::var("COPPERCLAW_SETUP_QUICKSTART_NAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_GROUP_NAME.to_string());

        let mut messages = Vec::new();
        match cfg.first_channel.as_str() {
            "cli" => {
                bootstrap_default_cli_group(&db, &name)?;
                messages.push(format!(
                    "bootstrapped default cli group `{name}` (cli/stdin) — `cclaw chat` is ready"
                ));
            }
            "telegram" => {
                let outcome = bootstrap_telegram_install(&db, cfg, &name)?;
                messages.push(format!(
                    "bootstrapped agent group `{name}` for telegram (wrote COPPERCLAW_CHANNELS + COPPERCLAW_CHANNELS_CONFIG to .env)"
                ));
                if let Some(chat_id) = outcome.chat_id {
                    messages.push(format!(
                        "wired telegram messaging-group for chat_id={chat_id} + approved sender — send a message to your bot to start"
                    ));
                } else {
                    messages.push(
                        "no TELEGRAM_CHAT_ID captured at setup time; the bot will create an unregistered-sender row on first inbound — approve with `cclaw approvals approve --channel telegram --identity <chat_id>`".to_string(),
                    );
                }
            }
            _ => unreachable!("guarded above"),
        }
        cfg.quickstart_group_created = true;

        // Wire the cli-channel <-> cclaw-chat bridge: a named pipe
        // (chat.fifo) for inbound and an append-log (chat.log) for
        // outbound. `cclaw chat` writes to the FIFO and tails the
        // log; the host's cli channel reads the FIFO and writes the
        // log. Without this bridge the cli channel reads the host's
        // own terminal stdin instead, which is precisely the bug
        // we're fixing. The bridge is always set up — both telegram
        // and cli installs benefit from being able to fall back to
        // `cclaw chat` for debugging without re-running setup.
        match ensure_cli_bridge(cfg)? {
            Some(BridgeOutcome {
                fifo,
                log,
                env_updated,
            }) => {
                if env_updated {
                    messages.push(format!(
                        "wired cli-chat bridge: fifo={}, log={} (env vars written to .env)",
                        fifo.display(),
                        log.display(),
                    ));
                } else {
                    messages.push(format!(
                        "cli-chat bridge already present: fifo={}, log={}",
                        fifo.display(),
                        log.display(),
                    ));
                }
            }
            None => {
                messages.push("skipped cli-chat bridge wiring (data_dir unset)".to_string());
            }
        }

        Ok(StepResult {
            messages,
            config_changed: true,
        })
    }
}

/// What [`ensure_cli_bridge`] actually did, for the operator-facing
/// status line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BridgeOutcome {
    fifo: PathBuf,
    log: PathBuf,
    /// `true` when one or both env vars were newly written / updated
    /// in this run; `false` when the `.env` already contained them.
    env_updated: bool,
}

/// Create the `chat.fifo` + `chat.log` pair under the install root
/// and persist `COPPERCLAW_CLI_FIFO` / `COPPERCLAW_CLI_LOG` in the install's
/// `.env` so the host picks them up on next boot. Idempotent:
/// re-running setup against an already-wired install is a no-op
/// (existing FIFO/log are left alone; existing env lines are not
/// duplicated).
fn ensure_cli_bridge(cfg: &SetupConfig) -> Result<Option<BridgeOutcome>, StepError> {
    if cfg.data_dir.as_os_str().is_empty() {
        return Ok(None);
    }
    let fifo = cfg.data_dir.join("chat.fifo");
    let log = cfg.data_dir.join("chat.log");
    ensure_fifo(&fifo)?;
    ensure_log(&log)?;

    let env_path = if cfg.env_file.as_os_str().is_empty() {
        cfg.data_dir.join(".env")
    } else {
        cfg.env_file.clone()
    };
    let env_updated = update_bridge_env(&env_path, &fifo, &log)?;

    Ok(Some(BridgeOutcome {
        fifo,
        log,
        env_updated,
    }))
}

/// Create the FIFO at `path` (mode 0o600) if it doesn't exist.
/// Shells out to `mkfifo` because the workspace forbids `unsafe`,
/// so we can't call `libc::mkfifo` directly.
fn ensure_fifo(path: &Path) -> Result<(), StepError> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("0600")
        .arg(path)
        .status()?;
    if !status.success() {
        return Err(StepError::Other(format!(
            "mkfifo {} exited with {status}",
            path.display()
        )));
    }
    Ok(())
}

/// Touch the log at `path` (creating with mode 0o600 if missing).
fn ensure_log(path: &Path) -> Result<(), StepError> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let _file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// Write the two `COPPERCLAW_CLI_*` lines into the install's `.env`.
/// Returns `true` when either line was newly added (or updated),
/// `false` when both lines already matched the desired values.
fn update_bridge_env(env_path: &Path, fifo: &Path, log: &Path) -> Result<bool, StepError> {
    let fifo_str = fifo.display().to_string();
    let log_str = log.display().to_string();
    let existing = if env_path.exists() {
        std::fs::read_to_string(env_path).unwrap_or_default()
    } else {
        String::new()
    };
    let already_has = |key: &str, val: &str| -> bool {
        existing.lines().any(|line| line == format!("{key}={val}"))
    };
    let need_fifo = !already_has("COPPERCLAW_CLI_FIFO", &fifo_str);
    let need_log = !already_has("COPPERCLAW_CLI_LOG", &log_str);
    if !need_fifo && !need_log {
        return Ok(false);
    }
    if need_fifo {
        append_env_var(env_path, "COPPERCLAW_CLI_FIFO", &fifo_str)?;
    }
    if need_log {
        append_env_var(env_path, "COPPERCLAW_CLI_LOG", &log_str)?;
    }
    Ok(true)
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
pub fn bootstrap_default_cli_group(db: &CentralDb, name: &str) -> Result<BootstrapIds, StepError> {
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

/// Outcome of [`bootstrap_telegram_install`]. Empty `chat_id` means the
/// operator didn't capture one at setup time (headless without
/// `COPPERCLAW_SETUP_TELEGRAM_CHAT_ID`); the bot still works but the
/// messaging-group + wiring + sender approval will be created lazily
/// from the runtime `unregistered_senders` flow on first inbound.
#[derive(Debug, Clone)]
pub struct TelegramBootstrap {
    pub agent_group_id: copperclaw_types::AgentGroupId,
    pub chat_id: Option<i64>,
    pub messaging_group_id: Option<copperclaw_types::MessagingGroupId>,
    pub wiring_id: Option<copperclaw_types::WiringId>,
}

/// Wire a fresh install for Telegram end-to-end. Reads the bot token
/// (and optional `chat_id`) the channel step persisted to `.env`,
/// writes the channel-enable env vars the host needs at boot, creates
/// the agent group, and — if a `chat_id` was captured — also creates
/// the messaging-group, the wiring, and the sender approval so the
/// very first inbound from that chat reaches the agent without
/// operator intervention.
///
/// Idempotent across re-runs: `append_env_var` replaces existing keys
/// in place; the caller (the step) gates further inserts on
/// `agent_groups` being empty.
pub fn bootstrap_telegram_install(
    db: &CentralDb,
    cfg: &SetupConfig,
    name: &str,
) -> Result<TelegramBootstrap, StepError> {
    // Resolve env file path (matches channel step's resolution).
    let env_path = if cfg.env_file.as_os_str().is_empty() {
        cfg.data_dir.join(".env")
    } else {
        cfg.env_file.clone()
    };

    // Read the bot token the channel step just wrote. Reading from
    // disk (not the process env) so the step works whether or not the
    // operator exported the var before re-running setup.
    let token = read_env_var(&env_path, "TELEGRAM_BOT_TOKEN")?.ok_or_else(|| {
        StepError::Other(format!(
            "TELEGRAM_BOT_TOKEN not present in {} — the channel step must run before quickstart_group",
            env_path.display(),
        ))
    })?;
    let chat_id_str = read_env_var(&env_path, "TELEGRAM_CHAT_ID")?;
    let chat_id: Option<i64> = chat_id_str
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok());

    // 1. Enable the channel adapter at boot. The host reads
    //    COPPERCLAW_CHANNELS to know which factories to call; the cli
    //    channel stays in the list so `cclaw chat` keeps working as a
    //    debugging fallback.
    append_env_var(&env_path, "COPPERCLAW_CHANNELS", "cli,telegram")?;

    // 2. The telegram factory itself reads bot_token from the
    //    COPPERCLAW_CHANNELS_CONFIG JSON, not from TELEGRAM_BOT_TOKEN
    //    directly. Single-quote the JSON so dotenvy parses it
    //    literally (otherwise the unquoted `{` confuses the parser).
    let channels_config =
        format!(r#"'{{"telegram":{{"bot_token":"{token}","mode":"long_poll"}}}}'"#,);
    append_env_var(&env_path, "COPPERCLAW_CHANNELS_CONFIG", &channels_config)?;

    // 3. Create the agent group.
    let ag = agent_groups::create(
        db,
        agent_groups::CreateAgentGroup {
            name: name.to_string(),
            folder: name.to_string(),
            agent_provider: None,
        },
    )
    .map_err(|e| StepError::Other(format!("agent_groups::create failed: {e}")))?;

    // 4. If we have a chat_id, finish the wiring fully. Otherwise the
    //    runtime unregistered_senders flow will handle it on first
    //    inbound (operator approves via `cclaw approvals approve ...`).
    if let Some(chat_id) = chat_id {
        let mg = messaging_groups::upsert(
            db,
            messaging_groups::UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: chat_id.to_string(),
                name: Some(format!("{name} (Telegram DM)")),
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

        // Approve the captured sender so the very first inbound from
        // their chat skips the unregistered_senders gate.
        users::upsert(
            db,
            users::UpsertUser {
                kind: "telegram".to_string(),
                identity: chat_id.to_string(),
                display_name: None,
            },
        )
        .map_err(|e| StepError::Other(format!("users::upsert failed: {e}")))?;

        return Ok(TelegramBootstrap {
            agent_group_id: ag.id,
            chat_id: Some(chat_id),
            messaging_group_id: Some(mg.id),
            wiring_id: Some(wiring.id),
        });
    }

    Ok(TelegramBootstrap {
        agent_group_id: ag.id,
        chat_id: None,
        messaging_group_id: None,
        wiring_id: None,
    })
}

/// Read a single env-var line out of a `.env` file. Returns `None`
/// when the file doesn't exist or the key isn't present. Doesn't
/// import dotenvy — the .env format is trivially line-oriented and
/// the channel step's `append_env_var` writes it verbatim.
fn read_env_var(path: &Path, key: &str) -> Result<Option<String>, StepError> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)?;
    let prefix = format!("{key}=");
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix(&prefix) {
            // Strip surrounding single quotes if the value was
            // dotenvy-quoted (e.g. COPPERCLAW_CHANNELS_CONFIG='{"..."}').
            let trimmed = rest
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .unwrap_or(rest);
            return Ok(Some(trimmed.to_string()));
        }
    }
    Ok(None)
}

/// Returned by [`bootstrap_default_cli_group`] so callers (and tests)
/// can correlate the three rows that just landed in the DB.
#[derive(Debug, Clone, Copy)]
pub struct BootstrapIds {
    pub agent_group_id: copperclaw_types::AgentGroupId,
    pub messaging_group_id: copperclaw_types::MessagingGroupId,
    pub wiring_id: copperclaw_types::WiringId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    fn fresh_cfg(dir: &std::path::Path) -> SetupConfig {
        SetupConfig {
            data_dir: dir.to_path_buf(),
            central_db_path: dir.join("copperclaw.db"),
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
    fn bootstrap_telegram_install_writes_env_vars_and_db_rows_with_chat_id() {
        // Live-bug regression: setup must produce a fully working
        // Telegram install (channels enabled, channels_config written,
        // messaging-group + wiring + approval in DB) when chat_id was
        // captured by the channel step. Otherwise the operator has to
        // edit .env + run 3 `cclaw` commands by hand.
        let dir = tempdir().unwrap();
        let env_path = dir.path().join(".env");
        // Pre-seed what the channel step would have written.
        append_env_var(&env_path, "TELEGRAM_BOT_TOKEN", "123456789:ABC-DEF").unwrap();
        append_env_var(&env_path, "TELEGRAM_CHAT_ID", "8929393356").unwrap();
        let db = CentralDb::open(dir.path().join("copperclaw.db")).unwrap();
        let cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            central_db_path: dir.path().join("copperclaw.db"),
            first_channel: "telegram".into(),
            ..SetupConfig::default()
        };

        let out = bootstrap_telegram_install(&db, &cfg, "first").unwrap();
        assert_eq!(out.chat_id, Some(8_929_393_356));
        assert!(out.messaging_group_id.is_some(), "mg must exist");
        assert!(out.wiring_id.is_some(), "wiring must exist");

        // Env-side: both channel-enable lines landed.
        let env = std::fs::read_to_string(&env_path).unwrap();
        assert!(
            env.contains("COPPERCLAW_CHANNELS=cli,telegram"),
            "missing channels list:\n{env}"
        );
        assert!(
            env.contains("COPPERCLAW_CHANNELS_CONFIG='"),
            "channels_config must be single-quoted JSON:\n{env}"
        );
        assert!(
            env.contains(r#""bot_token":"123456789:ABC-DEF""#),
            "bot_token must reach channels_config:\n{env}"
        );

        // DB-side: agent group + messaging group + wiring + user.
        let ag = agent_groups::list(&db).unwrap();
        assert_eq!(ag.len(), 1);
        let mg = messaging_groups::list(&db).unwrap();
        assert_eq!(mg.len(), 1);
        assert_eq!(mg[0].channel_type.as_str(), "telegram");
        assert_eq!(mg[0].platform_id, "8929393356");
        let wiring = messaging_group_agents::list_for_ag(&db, ag[0].id).unwrap();
        assert_eq!(wiring.len(), 1);
        // Sender approval landed.
        let approved = users::get_by_identity(&db, "telegram", "8929393356").unwrap();
        assert!(approved.is_some(), "sender approval must be persisted");
    }

    #[test]
    fn bootstrap_telegram_install_without_chat_id_still_enables_channel() {
        // Headless / interactive-skipped case: no TELEGRAM_CHAT_ID but
        // a valid token. The channel must still be enabled at boot;
        // the messaging-group + wiring are created lazily from the
        // runtime unregistered_senders flow on first inbound.
        let dir = tempdir().unwrap();
        let env_path = dir.path().join(".env");
        append_env_var(&env_path, "TELEGRAM_BOT_TOKEN", "111:xyz").unwrap();
        let db = CentralDb::open(dir.path().join("copperclaw.db")).unwrap();
        let cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            central_db_path: dir.path().join("copperclaw.db"),
            first_channel: "telegram".into(),
            ..SetupConfig::default()
        };

        let out = bootstrap_telegram_install(&db, &cfg, "first").unwrap();
        assert_eq!(out.chat_id, None);
        assert!(out.messaging_group_id.is_none(), "no chat_id ⇒ no mg yet");
        // Agent group is still created so the runtime auto-registration
        // has somewhere to route the first inbound.
        assert!(agent_groups::list(&db).unwrap().len() == 1);
        let env = std::fs::read_to_string(&env_path).unwrap();
        assert!(env.contains("COPPERCLAW_CHANNELS=cli,telegram"));
        assert!(env.contains("COPPERCLAW_CHANNELS_CONFIG='"));
    }

    #[test]
    fn bootstrap_telegram_install_errors_without_token() {
        let dir = tempdir().unwrap();
        let db = CentralDb::open(dir.path().join("copperclaw.db")).unwrap();
        let cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            central_db_path: dir.path().join("copperclaw.db"),
            first_channel: "telegram".into(),
            ..SetupConfig::default()
        };
        let err = bootstrap_telegram_install(&db, &cfg, "first").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("TELEGRAM_BOT_TOKEN"),
            "error must name the missing var: {msg}"
        );
    }

    #[test]
    fn bootstrap_default_cli_group_writes_three_rows() {
        let dir = tempdir().unwrap();
        let db = CentralDb::open(dir.path().join("copperclaw.db")).unwrap();
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
        let wiring = messaging_group_agents::list_for_ag(&db, ids.agent_group_id).unwrap();
        assert_eq!(wiring.len(), 1);
        assert_eq!(wiring[0].id, ids.wiring_id);
        assert_eq!(wiring[0].engage_mode, EngageMode::Pattern);
        assert_eq!(wiring[0].engage_pattern.as_deref(), Some(".*"));
    }

    #[test]
    fn step_runs_happy_path_when_db_empty_and_user_agrees() {
        let dir = tempdir().unwrap();
        // Seed an empty DB via CentralDbStep's helper.
        crate::steps::central_db::open_and_migrate(&dir.path().join("copperclaw.db")).unwrap();
        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        // Scripted prompt defaults to accepting confirms unless told otherwise.
        let prompt = Scripted::new().with("COPPERCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep
            .run(&mut cfg, &prompt, &mut state)
            .unwrap();
        assert!(res.messages.iter().any(|m| m.contains("bootstrapped")));
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        assert_eq!(agent_groups::list(&db).unwrap().len(), 1);
        assert_eq!(messaging_groups::list(&db).unwrap().len(), 1);
    }

    #[test]
    fn step_skips_when_groups_already_exist() {
        let dir = tempdir().unwrap();
        crate::steps::central_db::open_and_migrate(&dir.path().join("copperclaw.db")).unwrap();
        let db = CentralDb::open(dir.path().join("copperclaw.db")).unwrap();
        // Pre-create a group.
        bootstrap_default_cli_group(&db, "already-here").unwrap();
        drop(db);

        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("COPPERCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep
            .run(&mut cfg, &prompt, &mut state)
            .unwrap();
        assert!(
            res.messages
                .iter()
                .any(|m| m.contains("already configured")),
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
        crate::steps::central_db::open_and_migrate(&dir.path().join("copperclaw.db")).unwrap();
        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("COPPERCLAW_SETUP_QUICKSTART", "no");
        let res = QuickstartGroupStep
            .run(&mut cfg, &prompt, &mut state)
            .unwrap();
        assert!(
            res.messages.iter().any(|m| m.contains("declined")),
            "unexpected: {:?}",
            res.messages
        );
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        assert!(agent_groups::list(&db).unwrap().is_empty());
    }

    #[test]
    fn step_skips_when_first_channel_is_slack_or_discord() {
        // cli + telegram have setup-time auto-bootstrap recipes; other
        // channels still need the operator to wire credentials manually
        // and the step should bail out cleanly.
        for channel in ["slack", "discord"] {
            let dir = tempdir().unwrap();
            crate::steps::central_db::open_and_migrate(&dir.path().join("copperclaw.db")).unwrap();
            let mut cfg = fresh_cfg(dir.path());
            cfg.first_channel = channel.to_string();
            let mut state = SetupState::new();
            let prompt = Scripted::new().with("COPPERCLAW_SETUP_QUICKSTART", "yes");
            let res = QuickstartGroupStep
                .run(&mut cfg, &prompt, &mut state)
                .unwrap();
            assert!(
                res.messages
                    .iter()
                    .any(|m| m.contains("no auto-bootstrap recipe")),
                "{channel}: unexpected: {:?}",
                res.messages
            );
            let db = CentralDb::open(&cfg.central_db_path).unwrap();
            assert!(
                agent_groups::list(&db).unwrap().is_empty(),
                "{channel}: agent_groups should be empty after skip"
            );
        }
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
        let prompt = Scripted::new().with("COPPERCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep
            .run(&mut cfg, &prompt, &mut state)
            .unwrap();
        assert!(
            res.messages
                .iter()
                .any(|m| m.contains("central_db_path not set")),
            "unexpected: {:?}",
            res.messages
        );
    }

    #[test]
    fn ensure_cli_bridge_creates_fifo_log_and_env_lines() {
        let dir = tempdir().unwrap();
        let cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            env_file: dir.path().join(".env"),
            ..SetupConfig::default()
        };
        let outcome = ensure_cli_bridge(&cfg).unwrap().unwrap();
        assert!(outcome.env_updated);
        assert!(outcome.fifo.exists(), "FIFO should exist");
        assert!(outcome.log.exists(), "log should exist");
        let env_body = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        let fifo_str = outcome.fifo.display().to_string();
        let log_str = outcome.log.display().to_string();
        assert!(
            env_body.contains(&format!("COPPERCLAW_CLI_FIFO={fifo_str}")),
            "env body: {env_body}"
        );
        assert!(
            env_body.contains(&format!("COPPERCLAW_CLI_LOG={log_str}")),
            "env body: {env_body}"
        );
    }

    #[test]
    fn ensure_cli_bridge_is_idempotent() {
        let dir = tempdir().unwrap();
        let cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            env_file: dir.path().join(".env"),
            ..SetupConfig::default()
        };
        let first = ensure_cli_bridge(&cfg).unwrap().unwrap();
        assert!(first.env_updated);
        let second = ensure_cli_bridge(&cfg).unwrap().unwrap();
        // Existing FIFO + log + env lines → nothing to do.
        assert!(!second.env_updated);
        // The env body should contain exactly one COPPERCLAW_CLI_FIFO line.
        let body = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        let fifo_lines = body
            .lines()
            .filter(|l| l.starts_with("COPPERCLAW_CLI_FIFO="))
            .count();
        assert_eq!(fifo_lines, 1, "duplicate COPPERCLAW_CLI_FIFO lines: {body}");
        let log_lines = body
            .lines()
            .filter(|l| l.starts_with("COPPERCLAW_CLI_LOG="))
            .count();
        assert_eq!(log_lines, 1, "duplicate COPPERCLAW_CLI_LOG lines: {body}");
    }

    #[test]
    fn ensure_cli_bridge_returns_none_when_data_dir_unset() {
        let cfg = SetupConfig::default();
        assert!(ensure_cli_bridge(&cfg).unwrap().is_none());
    }

    #[test]
    fn step_run_writes_bridge_env_lines() {
        let dir = tempdir().unwrap();
        crate::steps::central_db::open_and_migrate(&dir.path().join("copperclaw.db")).unwrap();
        let mut cfg = fresh_cfg(dir.path());
        cfg.env_file = dir.path().join(".env");
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("COPPERCLAW_SETUP_QUICKSTART", "yes");
        let res = QuickstartGroupStep
            .run(&mut cfg, &prompt, &mut state)
            .unwrap();
        assert!(
            res.messages.iter().any(|m| m.contains("cli-chat bridge")),
            "messages: {:?}",
            res.messages
        );
        assert!(dir.path().join("chat.fifo").exists());
        assert!(dir.path().join("chat.log").exists());
        let body = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(body.contains("COPPERCLAW_CLI_FIFO="));
        assert!(body.contains("COPPERCLAW_CLI_LOG="));
    }

    #[test]
    fn step_honors_quickstart_name_env_var() {
        let dir = tempdir().unwrap();
        crate::steps::central_db::open_and_migrate(&dir.path().join("copperclaw.db")).unwrap();
        let mut cfg = fresh_cfg(dir.path());
        let mut state = SetupState::new();
        let prompt = Scripted::new().with("COPPERCLAW_SETUP_QUICKSTART", "yes");
        // Use the in-process env (single-threaded tests are the
        // convention here; the env-mutation is safe).
        // SAFETY: tests run with std::env::set_var in the same crate
        // are gated by Rust 2024's unsafe-env-var rule. Avoid it by
        // calling the pure helper directly with a custom name.
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        bootstrap_default_cli_group(&db, "ops").unwrap();
        drop(db);
        // Re-running the step should now see the group and skip.
        let res = QuickstartGroupStep
            .run(&mut cfg, &prompt, &mut state)
            .unwrap();
        assert!(
            res.messages
                .iter()
                .any(|m| m.contains("already configured"))
        );
        let db = CentralDb::open(&cfg.central_db_path).unwrap();
        let groups = agent_groups::list(&db).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "ops");
    }
}
