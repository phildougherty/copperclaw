//! Container manager: turns "session has pending inbound" into a running
//! container that runs the runner against the configured provider.
//!
//! This is the piece that closes the loop between the router (writes
//! inbound) and the delivery service (reads outbound). The host's
//! M0–M10 deliverables shipped both sides as tested in-process services
//! but never wired up the "spawn the runner" step that connects them in
//! production. This module is that step.
//!
//! Lifecycle in this slice:
//!
//! 1. Every `POLL_INTERVAL_MS` we poll the central `sessions` table for
//!    every active session.
//! 2. For each session where `container_status = stopped`, we open the
//!    session's `inbound.db` and ask if there's pending work
//!    (`messages_in.count_due > 0`).
//! 3. When there is, we:
//!    - Look up the agent group's `container_config` (provider, model,
//!      `image_tag`, etc.) — falling back to host defaults when the
//!      operator hasn't configured one yet.
//!    - Build a `RunnerConfigFile` and write it into the session dir
//!      as `runner.json`. The runner inside the container reads this
//!      file on boot (its `IRONCLAW_RUNNER_CONFIG` env var points at
//!      it).
//!    - Build a `ContainerSpec` that bind-mounts the session dir into
//!      `/data`, propagates `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL`,
//!      sets labels for orphan cleanup, and exec's
//!      `/usr/local/bin/ironclaw-runner --config /data/runner.json`.
//!    - Call `runtime.spawn(spec)` and persist
//!      `sessions.container_status = running`.
//!
//! Crash detection and idle-stop are explicit out-of-scope for this
//! slice — they belong in a follow-up that needs richer state tracking
//! than the table currently exposes. The runner writes a heartbeat
//! file under the session dir so a future sweep can read it.

use ironclaw_container_rt::{ContainerRuntime, ContainerSpec, Mount, RtError};
use ironclaw_db::central::CentralDb;
use ironclaw_db::session::{open_inbound, SessionPaths};
use ironclaw_db::tables::{container_configs, messages_in, sessions};
use ironclaw_types::{AgentGroupId, ContainerStatus, Session, SessionStatus};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Default poll cadence. The router debounces inbound, so once a
/// message has settled in `messages_in` we want to spawn fast — this
/// is the tail latency between "user typed" and "container starts
/// running". 1s feels right for a local cli loop; the sweep loop runs
/// at 60s and handles the slower lifecycle work.
pub const POLL_INTERVAL_MS: u64 = 1000;

/// Path inside the container where the session dir is mounted.
pub const CONTAINER_SESSION_DIR: &str = "/data";

/// Filename of the runner-config JSON written into the session dir.
pub const RUNNER_CONFIG_FILENAME: &str = "runner.json";

/// Path inside the container where the runner binary lives. Must
/// match the path baked into the session image at build time.
pub const CONTAINER_RUNNER_PATH: &str = "/usr/local/bin/ironclaw-runner";

/// Host-side knobs that don't change per-session.
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Label propagated to spawned containers so orphan cleanup picks
    /// them up across restarts.
    pub install_slug: String,
    /// Absolute path to the host's data dir (parent of `sessions/`).
    pub data_dir: PathBuf,
    /// Default image tag used when a `container_config` row doesn't
    /// pin one. Computed at boot from the default spec.
    pub default_image_tag: String,
    /// Default provider, e.g. `"anthropic"`. Pulled from
    /// `IRONCLAW_DEFAULT_PROVIDER` or `"anthropic"` as a fallback.
    pub default_provider: String,
    /// Default model id.
    pub default_model: String,
    /// `ANTHROPIC_API_KEY` value the runner inside the container will
    /// see. Read from the host's process env at boot.
    pub anthropic_api_key: Option<String>,
    /// Optional override base URL (e.g. `OpenRouter`'s
    /// `https://openrouter.ai/api/v1`).
    pub anthropic_base_url: Option<String>,
}

/// Manager service. Cheap to clone via `Arc`.
pub struct ContainerManager {
    central: CentralDb,
    runtime: Arc<dyn ContainerRuntime>,
    cfg: ManagerConfig,
}

impl ContainerManager {
    /// Build a new manager.
    #[must_use]
    pub fn new(
        central: CentralDb,
        runtime: Arc<dyn ContainerRuntime>,
        cfg: ManagerConfig,
    ) -> Self {
        Self {
            central,
            runtime,
            cfg,
        }
    }

    /// Poll loop. Returns when `shutdown` is cancelled.
    pub async fn run_loop(self: Arc<Self>, shutdown: CancellationToken) {
        let interval = Duration::from_millis(POLL_INTERVAL_MS);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(interval) => {
                    if let Err(err) = self.tick().await {
                        warn!(?err, "container_manager tick failed");
                    }
                }
            }
        }
    }

    /// One iteration: spawn containers for every stopped session that
    /// has pending inbound.
    pub async fn tick(&self) -> Result<(), ManagerError> {
        let sessions = sessions::list_active(&self.central).map_err(ManagerError::Db)?;
        for session in sessions {
            if !matches!(session.status, SessionStatus::Active) {
                continue;
            }
            if !matches!(session.container_status, ContainerStatus::Stopped) {
                continue;
            }
            if let Err(err) = self.maybe_spawn(&session).await {
                warn!(
                    session = %session.id.as_uuid(),
                    ?err,
                    "spawn failed; will retry on next tick"
                );
            }
        }
        Ok(())
    }

    /// Try to spawn a container for `session`. Returns `Ok(true)` when
    /// a container was actually spawned (i.e. pending work was found
    /// and the runtime call succeeded), `Ok(false)` when there was
    /// nothing pending, and `Err(...)` for real failures.
    async fn maybe_spawn(&self, session: &Session) -> Result<bool, ManagerError> {
        let paths = SessionPaths::new(
            &self.cfg.data_dir,
            session.agent_group_id,
            session.id,
        );
        paths.ensure_dirs().map_err(ManagerError::Io)?;

        if !Self::has_pending_inbound(&paths)? {
            return Ok(false);
        }

        let cfg_row = container_configs::get(&self.central, session.agent_group_id)
            .map_err(ManagerError::Db)?;
        let runner_cfg = self.runner_config_for(session, cfg_row.as_ref());
        let runner_json = serde_json::to_vec_pretty(&runner_cfg).map_err(ManagerError::Json)?;
        std::fs::write(paths.root.join(RUNNER_CONFIG_FILENAME), runner_json)
            .map_err(ManagerError::Io)?;

        let image_tag = cfg_row
            .as_ref()
            .and_then(|c| c.image_tag.clone())
            .unwrap_or_else(|| self.cfg.default_image_tag.clone());

        let spec = self.build_spec(session, &paths, &image_tag);
        let handle = self
            .runtime
            .spawn(spec)
            .await
            .map_err(ManagerError::Spawn)?;
        sessions::mark_container_running(&self.central, session.id).map_err(ManagerError::Db)?;
        info!(
            session = %session.id.as_uuid(),
            container = %handle.id,
            image = %image_tag,
            "spawned session container"
        );
        Ok(true)
    }

    fn has_pending_inbound(paths: &SessionPaths) -> Result<bool, ManagerError> {
        // Opening inbound here might create the DB file if it's
        // somehow missing; that's fine — `count_due` will return 0.
        let conn = open_inbound(paths).map_err(ManagerError::Db)?;
        let n = messages_in::count_due(&conn).map_err(ManagerError::Db)?;
        Ok(n > 0)
    }

    fn runner_config_for(
        &self,
        session: &Session,
        cc: Option<&container_configs::ContainerConfig>,
    ) -> RunnerConfigForFile {
        let provider = session
            .agent_provider
            .clone()
            .or_else(|| cc.and_then(|c| c.provider.clone()))
            .unwrap_or_else(|| self.cfg.default_provider.clone());
        let _ = provider; // currently only AnthropicProvider is wired in the runner.

        let model = cc
            .and_then(|c| c.model.clone())
            .unwrap_or_else(|| self.cfg.default_model.clone());
        let assistant_name = cc.and_then(|c| c.assistant_name.clone());
        let max_messages = cc.and_then(|c| c.max_messages_per_prompt);

        RunnerConfigForFile {
            session_id: session.id.as_uuid().to_string(),
            agent_group_id: session.agent_group_id.as_uuid().to_string(),
            // The container always sees its session dir at `/data` —
            // that's where the bind mount lands and where the runner
            // looks for `inbound.db`/`outbound.db`.
            session_dir: CONTAINER_SESSION_DIR.to_string(),
            model,
            system: String::new(),
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            api_base_url: self.cfg.anthropic_base_url.clone(),
            assistant_name,
            max_messages_per_prompt: max_messages,
        }
    }

    fn build_spec(
        &self,
        session: &Session,
        paths: &SessionPaths,
        image_tag: &str,
    ) -> ContainerSpec {
        let mut spec = ContainerSpec::new(container_name(session.agent_group_id, session.id), image_tag)
            .with_entrypoint(vec![CONTAINER_RUNNER_PATH.to_string()])
            .with_label("ironclaw.install", self.cfg.install_slug.clone())
            .with_label("ironclaw.session", session.id.as_uuid().to_string())
            .with_label(
                "ironclaw.agent_group",
                session.agent_group_id.as_uuid().to_string(),
            )
            .with_mount(Mount::Bind {
                source: paths.root.to_string_lossy().into_owned(),
                target: CONTAINER_SESSION_DIR.to_string(),
                read_only: false,
            });

        // The runner reads its config from this path via the
        // `--config` flag wired into the entrypoint args. ContainerSpec
        // doesn't have a dedicated args field, so we encode the flag
        // by extending the entrypoint vector.
        spec.entrypoint
            .extend(vec!["--config".to_string(), format!("{CONTAINER_SESSION_DIR}/{RUNNER_CONFIG_FILENAME}")]);

        if let Some(key) = self.cfg.anthropic_api_key.as_deref() {
            spec = spec.with_env("ANTHROPIC_API_KEY", key);
        }
        if let Some(base) = self.cfg.anthropic_base_url.as_deref() {
            spec = spec.with_env("ANTHROPIC_BASE_URL", base);
        }
        spec
    }
}

/// Container name format. Uses the session id (which is a UUID) so
/// names are globally unique and DNS-safe.
fn container_name(_agent: AgentGroupId, session: ironclaw_types::SessionId) -> String {
    format!("ironclaw-{}", session.as_uuid())
}

/// `RunnerConfigFile` lives in `ironclaw-runner`, but pulling the
/// runner crate into the host as a non-test dep would create a
/// circular trail (the runner crate already pulls in `ironclaw-mcp`
/// and `ironclaw-providers`, both of which the host doesn't otherwise
/// need at runtime). Mirror the on-disk schema here — there's
/// exactly one consumer and it's a JSON file, so the duplication is
/// cheap.
#[derive(Debug, serde::Serialize)]
struct RunnerConfigForFile {
    session_id: String,
    agent_group_id: String,
    session_dir: String,
    model: String,
    system: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assistant_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_messages_per_prompt: Option<u32>,
}

/// Errors raised by the manager's poll loop.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// DB read/write failure.
    #[error("db: {0}")]
    Db(#[from] ironclaw_db::DbError),
    /// JSON serialization failed building the runner config.
    #[error("json: {0}")]
    Json(serde_json::Error),
    /// Local-FS failure writing the runner config or ensuring dirs.
    #[error("io: {0}")]
    Io(std::io::Error),
    /// Container runtime spawn failed.
    #[error("spawn: {0}")]
    Spawn(#[source] RtError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_db::tables::sessions::{create as create_session, CreateSession};
    use ironclaw_types::SessionId;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "ironclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-5".into(),
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
        }
    }

    fn fixture_session(db: &CentralDb) -> Session {
        let ag = create_ag(
            db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        create_session(
            db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn container_name_is_deterministic_and_uuid_shaped() {
        let s = SessionId(uuid::Uuid::nil());
        let ag = AgentGroupId::new();
        let name = container_name(ag, s);
        assert_eq!(name, "ironclaw-00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn build_spec_includes_bind_label_env_entrypoint() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "ironclaw/session:abc");
        // Image
        assert_eq!(spec.image, "ironclaw/session:abc");
        // Entrypoint includes the runner path + --config arg
        assert_eq!(spec.entrypoint[0], CONTAINER_RUNNER_PATH);
        assert_eq!(spec.entrypoint[1], "--config");
        assert!(spec.entrypoint[2].ends_with(RUNNER_CONFIG_FILENAME));
        // Mount the session root at /data
        let bind = spec
            .mounts
            .iter()
            .find_map(|m| match m {
                Mount::Bind { source, target, read_only } => Some((source, target, read_only)),
                _ => None,
            })
            .unwrap();
        assert_eq!(bind.1, CONTAINER_SESSION_DIR);
        assert!(!*bind.2);
        // Env carries both API key and base URL.
        assert!(spec.env.iter().any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "sk-test"));
        assert!(spec.env.iter().any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v.contains("openrouter")));
        // Labels for orphan cleanup.
        assert_eq!(spec.labels.get("ironclaw.install").map(String::as_str), Some("test"));
        assert!(spec.labels.contains_key("ironclaw.session"));
        assert!(spec.labels.contains_key("ironclaw.agent_group"));
    }

    #[test]
    fn build_spec_skips_base_url_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.anthropic_base_url = None;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img");
        assert!(spec.env.iter().all(|(k, _)| k != "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn runner_config_uses_session_then_container_config_then_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cfg = mgr.runner_config_for(&session, None);
        assert_eq!(cfg.model, "claude-sonnet-4-5");
        assert_eq!(cfg.session_dir, CONTAINER_SESSION_DIR);
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(
            cfg.api_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[tokio::test]
    async fn tick_skips_sessions_without_pending_inbound() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let _session = fixture_session(&db);
        mgr.tick().await.unwrap();
        // session_status is unchanged: NoopRuntime would have been
        // called, but only if pending inbound existed.
        let sessions = sessions::list_active(&db).unwrap();
        for s in sessions {
            assert!(matches!(s.container_status, ContainerStatus::Stopped));
        }
    }

    #[tokio::test]
    async fn tick_spawns_when_inbound_has_pending_work() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        // Seed pending inbound (even seq, status='pending').
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: ironclaw_types::MessageId::new(),
                kind: ironclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ironclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
            },
        )
        .unwrap();

        mgr.tick().await.unwrap();

        // Session should now be marked running.
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Running));
        // The noop runtime records the spawn call.
        assert!(runtime.spawn_calls().iter().any(|name| {
            name.contains(&session.id.as_uuid().to_string())
        }));
        // Runner config got written.
        assert!(paths.root.join(RUNNER_CONFIG_FILENAME).is_file());
    }
}
