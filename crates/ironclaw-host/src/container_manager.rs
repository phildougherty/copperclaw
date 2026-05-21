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

use ironclaw_container_rt::{ContainerRuntime, ContainerSpec, ImageBuildSpec, Mount, ResourceLimits, RtError};
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

/// Default idle window before the manager stops a running container.
/// 300s (5 min) matches the OpenBSD-of-claw-agents "conservative
/// defaults" tenet — long enough to avoid thrashing on quiet groups,
/// short enough that an unattended host doesn't burn memory.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Default heartbeat-staleness threshold. The runner refreshes its
/// `.heartbeat` file's mtime every ~1s as part of the poll loop; if
/// the host hasn't seen a refresh for this long, the runner is
/// presumed dead and the container is reset for respawn.
pub const DEFAULT_HEARTBEAT_STALE_SECS: u64 = 60;

/// Grace period passed to `runtime.stop` on idle / crash transitions.
/// 5s is enough for the runner to flush an in-flight HTTP call.
pub const DEFAULT_STOP_GRACE_SECS: u64 = 5;

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
    /// Seconds without inbound activity before the manager stops the
    /// container and flips `container_status=idle`.
    pub idle_timeout_secs: u64,
    /// Seconds without heartbeat refresh before the manager
    /// considers the runner dead, stops the container (best effort),
    /// and resets `container_status=stopped` for respawn.
    pub heartbeat_stale_secs: u64,
    /// Grace period for `runtime.stop` calls — sent as SIGTERM
    /// timeout. The runtime sends SIGKILL after.
    pub stop_grace_secs: u64,
    /// Directory containing global `SKILL.md` bundles. When set, the
    /// manager loads each enabled skill's body into the runner's
    /// system prompt at spawn so the model knows what tools it has.
    /// `None` keeps the system prompt empty (legacy behaviour).
    pub skills_dir: Option<PathBuf>,
    /// Per-group override root. When set, `<groups_dir>/<ag_uuid>/skills/`
    /// is scanned alongside the global skills directory and skills
    /// with matching names shadow the global ones.
    pub groups_dir: Option<PathBuf>,
    /// Extra environment variables to forward into every spawned
    /// session container. Used to plumb operator-supplied API keys
    /// (Tavily / Exa / Brave / `SerpAPI` / etc.) and arbitrary
    /// `IRONCLAW_*` settings through to the runner. Keys with empty
    /// values are skipped so an unset operator env doesn't write
    /// `FOO=` lines into the container env.
    pub forward_env: Vec<(String, String)>,
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

    /// One iteration. Walks every active session and reconciles its
    /// `container_status` with reality:
    ///
    /// - `Stopped` + pending inbound → spawn → `Running`.
    /// - `Idle`    + pending inbound → reset to `Stopped` so the next
    ///   tick spawns; we don't try to start a container at the same
    ///   time we mark it stopped, because spawning needs the most
    ///   recent state.
    /// - `Running` + heartbeat stale → crash detected, stop best-effort,
    ///   reset to `Stopped` (manager will respawn next tick).
    /// - `Running` + `last_active` stale → idle, stop, mark `Idle`.
    /// - `Running` + alive + recently active → leave alone.
    pub async fn tick(&self) -> Result<(), ManagerError> {
        let sessions = sessions::list_active(&self.central).map_err(ManagerError::Db)?;
        for session in sessions {
            if !matches!(session.status, SessionStatus::Active) {
                continue;
            }
            let action = self.classify(&session);
            if let Err(err) = self.apply(&session, action).await {
                warn!(
                    session = %session.id.as_uuid(),
                    ?err,
                    "session reconcile failed; will retry on next tick"
                );
            }
        }
        Ok(())
    }

    /// Decide what to do with a single session based on its
    /// `container_status`, the inbound pending count, the heartbeat
    /// file's mtime, and the `last_active` timestamp. Pure: takes no
    /// async work and no DB writes so the state machine is unit-
    /// testable.
    fn classify(&self, session: &Session) -> ReconcileAction {
        let paths = SessionPaths::new(
            &self.cfg.data_dir,
            session.agent_group_id,
            session.id,
        );
        let pending = Self::has_pending_inbound(&paths).unwrap_or(false);
        match session.container_status {
            ContainerStatus::Stopped => {
                if pending {
                    ReconcileAction::Spawn
                } else {
                    ReconcileAction::Noop
                }
            }
            ContainerStatus::Idle => {
                if pending {
                    ReconcileAction::WakeFromIdle
                } else {
                    ReconcileAction::Noop
                }
            }
            ContainerStatus::Running => {
                if Self::heartbeat_stale(&paths, self.cfg.heartbeat_stale_secs)
                    .unwrap_or(false)
                {
                    ReconcileAction::CrashRestart
                } else if Self::session_idle(session, self.cfg.idle_timeout_secs) {
                    ReconcileAction::IdleStop
                } else {
                    ReconcileAction::Noop
                }
            }
        }
    }

    async fn apply(
        &self,
        session: &Session,
        action: ReconcileAction,
    ) -> Result<(), ManagerError> {
        match action {
            ReconcileAction::Noop => Ok(()),
            ReconcileAction::Spawn => {
                self.maybe_spawn(session).await?;
                Ok(())
            }
            ReconcileAction::WakeFromIdle => {
                sessions::mark_container_stopped(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                info!(session = %session.id.as_uuid(), "idle → stopped (pending inbound)");
                Ok(())
            }
            ReconcileAction::IdleStop => {
                let name = container_name(session.agent_group_id, session.id);
                let _ = self
                    .runtime
                    .stop(&name, Duration::from_secs(self.cfg.stop_grace_secs))
                    .await;
                sessions::mark_container_idle(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                info!(session = %session.id.as_uuid(), "running → idle (no activity)");
                Ok(())
            }
            ReconcileAction::CrashRestart => {
                // Remove (not just stop) so the next spawn doesn't
                // collide on the container name. `remove` is a
                // stop+rm that treats 404 as success, so it's safe
                // to call even when the container is already gone.
                let name = container_name(session.agent_group_id, session.id);
                let _ = self.runtime.remove(&name).await;
                sessions::mark_container_stopped(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                ironclaw_metrics::inc_containers_crashed();
                warn!(
                    session = %session.id.as_uuid(),
                    "heartbeat stale; running → stopped (will respawn)"
                );
                Ok(())
            }
        }
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

        // Budget gate. If the group has a daily_token_cap and today's
        // turns already meet/exceed it, refuse to spawn. The inbound
        // sits in the row until the cap resets at UTC midnight or the
        // operator raises it via `iclaw groups budget set`.
        if self.is_over_budget(session)? {
            warn!(
                session = %session.id.as_uuid(),
                agent_group = %session.agent_group_id.as_uuid(),
                "daily token budget exhausted; spawn deferred"
            );
            return Ok(false);
        }

        let cfg_row = container_configs::get(&self.central, session.agent_group_id)
            .map_err(ManagerError::Db)?;
        let runner_cfg = self.runner_config_for(session, cfg_row.as_ref());
        let runner_json = serde_json::to_vec_pretty(&runner_cfg).map_err(ManagerError::Json)?;
        std::fs::write(paths.root.join(RUNNER_CONFIG_FILENAME), runner_json)
            .map_err(ManagerError::Io)?;

        // Image rebuild gate (Top 10 #1 / M13).  Compute the fingerprint of
        // the rebuild-relevant config fields.  If the stored fingerprint
        // differs from the current config (or is absent), rebuild the image
        // before spawning.  The new tag + fingerprint are persisted back to
        // `container_configs` so subsequent spawns reuse the cached image.
        //
        // Failure handling: if the rebuild itself fails (bad apt name,
        // network blip during `apt-get update`, etc.) we fall back to the
        // last-known-good `image_tag` so the session still spawns. The
        // fingerprint is NOT updated on fallback, so the next spawn retries
        // the rebuild — the operator can fix the broken config without the
        // group going dark in the meantime. When there is no fallback tag
        // (first-ever build, no cached image), the error propagates and the
        // session stays Stopped for the next tick to retry.
        let image_tag = if let Some(ref cfg) = cfg_row {
            let live_fp = container_configs::compute_fingerprint(cfg);
            let stored_fp = cfg.config_fingerprint.as_deref().unwrap_or("");
            let base_tag = cfg.image_tag.clone().unwrap_or_else(|| self.cfg.default_image_tag.clone());
            if stored_fp == live_fp {
                base_tag
            } else {
                match self.rebuild_image(session.agent_group_id, cfg).await {
                    Ok(new_tag) => new_tag,
                    Err(err) if !base_tag.is_empty() => {
                        warn!(
                            agent_group = %session.agent_group_id.as_uuid(),
                            fallback_tag = %base_tag,
                            ?err,
                            "image rebuild failed; spawning on last-known-good tag (operator must fix config to pick up changes)"
                        );
                        ironclaw_metrics::inc_image_rebuild_failed();
                        base_tag
                    }
                    Err(err) => {
                        ironclaw_metrics::inc_image_rebuild_failed();
                        return Err(err);
                    }
                }
            }
        } else {
            self.cfg.default_image_tag.clone()
        };

        let spec = self.build_spec(session, &paths, &image_tag, cfg_row.as_ref());
        // Defensive: if a previous container with this name lingered
        // (e.g. host crashed mid-cycle, orphan cleanup missed it,
        // crash-restart's remove() raced the spawn) the create call
        // would 409. Best-effort remove is a no-op when nothing
        // matches, so it's cheap to always do.
        let _ = self.runtime.remove(&spec.name).await;
        // Reset the heartbeat file so the new container starts with
        // a clean slate. Without this, the previous container's
        // last (now-stale) mtime persists and the manager would
        // crash-restart the new spawn before its runner could write
        // its first heartbeat.
        let _ = std::fs::remove_file(&paths.heartbeat);
        let spawn_started = std::time::Instant::now();
        let handle = self
            .runtime
            .spawn(spec)
            .await
            .map_err(ManagerError::Spawn)?;
        let spawn_elapsed = spawn_started.elapsed().as_secs_f64();
        sessions::mark_container_running(&self.central, session.id).map_err(ManagerError::Db)?;
        ironclaw_metrics::inc_containers_spawned();
        ironclaw_metrics::observe_container_spawn_seconds(spawn_elapsed);
        info!(
            session = %session.id.as_uuid(),
            container = %handle.id,
            image = %image_tag,
            "spawned session container"
        );
        Ok(true)
    }

    /// Returns true when the group's `daily_token_cap` is set AND
    /// today's accumulated input + output tokens already meet or
    /// exceed it. Day boundary = UTC midnight, matching what an
    /// operator setting "daily" cap would naturally expect.
    fn is_over_budget(&self, session: &Session) -> Result<bool, ManagerError> {
        use ironclaw_db::tables::{agent_turns, group_budgets};
        let Some(budget) = group_budgets::get(&self.central, session.agent_group_id)
            .map_err(ManagerError::Db)?
        else {
            return Ok(false);
        };
        let Some(cap) = budget.daily_token_cap else {
            return Ok(false);
        };
        let midnight = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is a valid time")
            .and_utc();
        let used = agent_turns::tokens_since(
            &self.central,
            &session.agent_group_id.as_uuid().to_string(),
            midnight,
        )
        .map_err(ManagerError::Db)?;
        Ok(used >= cap)
    }

    fn has_pending_inbound(paths: &SessionPaths) -> Result<bool, ManagerError> {
        // Opening inbound here might create the DB file if it's
        // somehow missing; that's fine — `count_due` will return 0.
        let conn = open_inbound(paths).map_err(ManagerError::Db)?;
        let n = messages_in::count_due(&conn).map_err(ManagerError::Db)?;
        Ok(n > 0)
    }

    /// Whether the runner has stopped refreshing its `.heartbeat`
    /// file. Treats the file's mtime as the truth source; if the
    /// file doesn't exist yet, that's *not* stale — the runner may
    /// not have started writing it yet (containers take a moment to
    /// boot).
    fn heartbeat_stale(paths: &SessionPaths, threshold_secs: u64) -> Result<bool, ManagerError> {
        let mtime = paths.heartbeat_mtime().map_err(ManagerError::Io)?;
        let Some(mtime) = mtime else { return Ok(false) };
        let age = std::time::SystemTime::now()
            .duration_since(mtime)
            .unwrap_or(std::time::Duration::ZERO);
        Ok(age > std::time::Duration::from_secs(threshold_secs))
    }

    /// Whether `last_active` is older than the configured idle window.
    fn session_idle(session: &Session, idle_window_secs: u64) -> bool {
        let now = chrono::Utc::now();
        let elapsed = now.signed_duration_since(session.last_active);
        elapsed.num_seconds() > i64::try_from(idle_window_secs).unwrap_or(i64::MAX)
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
        let selector = cc.map_or(ironclaw_skills::SkillsSelector::All, |c| {
            db_selector_to_skills_selector(&c.skills)
        });
        let system = build_skill_system_prompt(
            self.cfg.skills_dir.as_deref(),
            self.cfg.groups_dir.as_deref(),
            session.agent_group_id,
            &selector,
        );

        RunnerConfigForFile {
            session_id: session.id.as_uuid().to_string(),
            agent_group_id: session.agent_group_id.as_uuid().to_string(),
            // The container always sees its session dir at `/data` —
            // that's where the bind mount lands and where the runner
            // looks for `inbound.db`/`outbound.db`.
            session_dir: CONTAINER_SESSION_DIR.to_string(),
            model,
            system,
            api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
            api_base_url: self.cfg.anthropic_base_url.clone(),
            assistant_name,
            max_messages_per_prompt: max_messages,
        }
    }

    /// Build the image for an agent group whose config has changed.
    ///
    /// Uses the `ImageBuildSpec` machinery to produce a sha256-tagged image
    /// from `packages_apt` / `packages_npm` (`mcp_servers` and skills are
    /// runtime config, not image config — they don't affect the Dockerfile).
    /// After a successful build the new tag + fingerprint are written back to
    /// `container_configs` so future spawns can reuse the cached image.
    async fn rebuild_image(
        &self,
        agent_group_id: AgentGroupId,
        cfg: &container_configs::ContainerConfig,
    ) -> Result<String, ManagerError> {
        let mut build_spec = ImageBuildSpec::new("ironclaw/session", "debian:trixie-slim");
        build_spec.apt_packages.clone_from(&cfg.packages_apt);
        build_spec.npm_packages.clone_from(&cfg.packages_npm);
        let tag = self.runtime.build_image(build_spec).await.map_err(ManagerError::Spawn)?;
        let live_fp = container_configs::compute_fingerprint(cfg);
        container_configs::set_image_tag_and_fingerprint(
            &self.central,
            agent_group_id,
            &tag,
            &live_fp,
        )
        .map_err(ManagerError::Db)?;
        info!(
            agent_group = %agent_group_id.as_uuid(),
            image = %tag,
            "rebuilt image for config change"
        );
        Ok(tag)
    }

    fn build_spec(
        &self,
        session: &Session,
        paths: &SessionPaths,
        image_tag: &str,
        cfg: Option<&container_configs::ContainerConfig>,
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
        // Operator-configured forwards (search API keys, etc.). Skip
        // empty values — an unset env var should not appear in the
        // container env at all.
        for (k, v) in &self.cfg.forward_env {
            if !v.is_empty() {
                spec = spec.with_env(k, v);
            }
        }

        // Per-group resource limits (Top 10 #8 / M13).
        if let Some(c) = cfg {
            match ResourceLimits::from_json(&c.resource_limits) {
                Ok(limits) if !limits.is_empty() => {
                    spec = spec.with_resource_limits(limits);
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        agent_group = %session.agent_group_id.as_uuid(),
                        error = %e,
                        "ignoring invalid resource_limits JSON; spawning without caps"
                    );
                }
            }
            // Egress allow-list (Top 10 #6 / M13).
            if !c.egress_allow.is_empty() {
                spec = spec.with_egress_allow(c.egress_allow.clone());
            }
        }

        spec
    }
}

/// Container name format. Uses the session id (which is a UUID) so
/// names are globally unique and DNS-safe.
fn container_name(_agent: AgentGroupId, session: ironclaw_types::SessionId) -> String {
    format!("ironclaw-{}", session.as_uuid())
}

/// Assemble the agent's system prompt from the global skills directory
/// (optional per-group override), filtered through the group's
/// `SkillsSelector`. Each skill's `SKILL.md` body is inlined as a
/// labelled `<skill>` block; the wrapper tags help the model treat
/// each one as a discrete unit while keeping the underlying markdown
/// intact.
///
/// Returns an empty string when no skills directory is configured or
/// when the selector resolves to zero skills. Read or parse failures
/// for individual skills are logged and that skill is dropped —
/// failing to load a single skill does not cause the whole spawn to
/// fail.
fn build_skill_system_prompt(
    skills_dir: Option<&std::path::Path>,
    groups_dir: Option<&std::path::Path>,
    agent_group_id: AgentGroupId,
    selector: &ironclaw_skills::SkillsSelector,
) -> String {
    let Some(global) = skills_dir else {
        return String::new();
    };
    let group_override = groups_dir
        .map(|root| {
            root.join(agent_group_id.as_uuid().to_string())
                .join("skills")
        })
        .filter(|p| p.is_dir())
        .map(|p| (agent_group_id, p));

    let registry = match ironclaw_skills::SkillRegistry::scan(
        global,
        group_override
            .as_ref()
            .map(|(id, p)| (*id, p.as_path())),
    ) {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, dir = %global.display(), "skill scan failed; system prompt will be empty");
            return String::new();
        }
    };

    let selected = registry.list_for_group(agent_group_id, selector);
    if selected.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(8 * 1024);
    out.push_str(
        "The following skills document the capabilities available to you. \
Each <skill> block is the rendered SKILL.md for one capability — read \
them all before deciding which tool to call.\n",
    );
    for skill in &selected {
        let body = match ironclaw_skills::read_skill_body(skill) {
            Ok(b) => b,
            Err(err) => {
                warn!(
                    skill = %skill.name,
                    ?err,
                    "skill body read failed; skipping"
                );
                continue;
            }
        };
        out.push_str("\n<skill name=\"");
        out.push_str(&skill.name);
        out.push_str("\" description=\"");
        out.push_str(&escape_attr(&skill.description));
        out.push_str("\">\n");
        out.push_str(body.trim_end());
        out.push_str("\n</skill>\n");
    }
    out
}

/// Minimal escape for a description embedded in an XML-like attribute
/// value. We only need to neutralise the quote and ampersand — the
/// agent doesn't parse this strictly, but unbalanced quotes would
/// confuse a casual reader.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// Translate the db crate's [`container_configs::SkillsSelector`] to
/// the skills crate's [`ironclaw_skills::SkillsSelector`]. They share
/// a JSON shape but are distinct types because the two crates don't
/// (and shouldn't) depend on each other.
fn db_selector_to_skills_selector(
    sel: &container_configs::SkillsSelector,
) -> ironclaw_skills::SkillsSelector {
    match sel {
        container_configs::SkillsSelector::All => ironclaw_skills::SkillsSelector::All,
        container_configs::SkillsSelector::Explicit(names) => {
            ironclaw_skills::SkillsSelector::Explicit(names.clone())
        }
    }
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

/// What the reconcile loop wants to do with a session this tick.
#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Nothing to do — session is in a healthy steady state.
    Noop,
    /// Spawn a fresh container for a `Stopped` session with pending
    /// inbound.
    Spawn,
    /// `Idle` session got new inbound; mark it `Stopped` so the next
    /// tick spawns. Two-step transition (rather than spawning here)
    /// because spawn needs to read the current `container_status`
    /// row and we don't want to race ourselves.
    WakeFromIdle,
    /// `Running` session has been quiet long enough — stop the
    /// container and mark `Idle`.
    IdleStop,
    /// `Running` session's heartbeat is stale — the runner has likely
    /// crashed. Stop best-effort and reset to `Stopped` for respawn.
    CrashRestart,
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
            default_model: "claude-sonnet-4-6".into(),
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            forward_env: Vec::new(),
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
        let spec = mgr.build_spec(&session, &paths, "ironclaw/session:abc", None);
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
    fn build_spec_forwards_extra_env_and_skips_empty_values() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.forward_env = vec![
            ("TAVILY_API_KEY".to_string(), "tav-secret".to_string()),
            ("EXA_API_KEY".to_string(), "exa-secret".to_string()),
            // Empty values must not appear in the container env.
            ("BRAVE_SEARCH_API_KEY".to_string(), String::new()),
        ];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        assert!(spec.env.iter().any(|(k, v)| k == "TAVILY_API_KEY" && v == "tav-secret"));
        assert!(spec.env.iter().any(|(k, v)| k == "EXA_API_KEY" && v == "exa-secret"));
        assert!(
            spec.env.iter().all(|(k, _)| k != "BRAVE_SEARCH_API_KEY"),
            "empty-valued forward must be skipped"
        );
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
        let spec = mgr.build_spec(&session, &paths, "img", None);
        assert!(spec.env.iter().all(|(k, _)| k != "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn build_spec_applies_resource_limits_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        // Build a minimal ContainerConfig with resource limits set.
        let cfg = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({"cpus": "1.5", "memory_mb": 512u64}),
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg));
        assert!(!spec.resource_limits.is_empty());
        assert!((spec.resource_limits.cpus.unwrap() - 1.5).abs() < f64::EPSILON);
        assert_eq!(spec.resource_limits.memory_mb, Some(512));
    }

    #[test]
    fn build_spec_applies_egress_allow_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let cfg = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec!["api.example.com:443".into(), "db.local:5432".into()],
            resource_limits: serde_json::json!({}),
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg));
        assert_eq!(spec.egress_allow, vec!["api.example.com:443", "db.local:5432"]);
    }

    #[test]
    fn build_spec_empty_egress_allow_stays_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let cfg = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            updated_at: chrono::Utc::now(),
        };
        let spec = mgr.build_spec(&session, &paths, "img", Some(&cfg));
        assert!(spec.egress_allow.is_empty());
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
        assert_eq!(cfg.model, "claude-sonnet-4-6");
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

    // ---- state machine classification ----

    fn make_mgr(tmp: &tempfile::TempDir) -> (ContainerManager, CentralDb) {
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        (mgr, db)
    }

    #[test]
    fn classify_stopped_without_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        // container_status defaults to Stopped per create_session.
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_stopped_with_pending_is_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
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
        assert_eq!(mgr.classify(&session), ReconcileAction::Spawn);
    }

    #[test]
    fn classify_running_with_fresh_heartbeat_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        session.last_active = chrono::Utc::now();
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_running_with_stale_heartbeat_is_crash_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // Backdate the heartbeat mtime to before the staleness window.
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        filetime::set_file_mtime(
            &paths.heartbeat,
            filetime::FileTime::from_system_time(old),
        )
        .unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::CrashRestart);
    }

    #[test]
    fn classify_running_with_quiet_session_is_idle_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        // last_active is set to "now" at session create; backdate it.
        session.last_active = chrono::Utc::now()
            - chrono::Duration::seconds(
                i64::try_from(DEFAULT_IDLE_TIMEOUT_SECS).unwrap() + 10,
            );
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::IdleStop);
    }

    #[test]
    fn classify_idle_without_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_idle_with_pending_is_wake_from_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
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
        assert_eq!(mgr.classify(&session), ReconcileAction::WakeFromIdle);
    }

    #[tokio::test]
    async fn apply_wake_from_idle_marks_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        mgr.apply(&session, ReconcileAction::WakeFromIdle).await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    #[tokio::test]
    async fn apply_idle_stop_marks_idle_and_calls_runtime_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        mgr.apply(&session, ReconcileAction::IdleStop).await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Idle));
    }

    #[tokio::test]
    async fn apply_crash_restart_marks_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    // ---- skill system prompt assembly ----

    fn write_skill_md(parent: &std::path::Path, name: &str, body: &str) {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!(
            "---\nname: {name}\ndescription: desc-of-{name}\n---\n\n{body}"
        );
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn build_skill_system_prompt_empty_when_no_dir() {
        let prompt = build_skill_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
        );
        assert!(prompt.is_empty());
    }

    #[test]
    fn build_skill_system_prompt_all_includes_each_skill_body() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "# alpha\nAlpha body\n");
        write_skill_md(&skills, "beta", "# beta\nBeta body\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
        );
        assert!(prompt.contains("<skill name=\"alpha\""));
        assert!(prompt.contains("Alpha body"));
        assert!(prompt.contains("<skill name=\"beta\""));
        assert!(prompt.contains("Beta body"));
        assert!(prompt.contains("desc-of-alpha"));
        // Frontmatter delimiters must not leak into the prompt.
        assert!(!prompt.contains("---\nname: alpha"));
    }

    #[test]
    fn build_skill_system_prompt_explicit_filters() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        write_skill_md(&skills, "beta", "beta body\n");
        write_skill_md(&skills, "gamma", "gamma body\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::Explicit(vec!["beta".into()]),
        );
        assert!(!prompt.contains("alpha body"));
        assert!(prompt.contains("beta body"));
        assert!(!prompt.contains("gamma body"));
    }

    #[test]
    fn build_skill_system_prompt_empty_when_no_skills_selected() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "a\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::Explicit(vec![]),
        );
        assert!(prompt.is_empty());
    }

    #[test]
    fn build_skill_system_prompt_group_override_shadows_global() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "send-message", "global body\n");

        let ag = AgentGroupId::new();
        let groups = td.path().join("groups");
        let group_skills = groups
            .join(ag.as_uuid().to_string())
            .join("skills");
        std::fs::create_dir_all(&group_skills).unwrap();
        write_skill_md(&group_skills, "send-message", "group override body\n");

        let prompt = build_skill_system_prompt(
            Some(&skills),
            Some(&groups),
            ag,
            &ironclaw_skills::SkillsSelector::All,
        );
        assert!(prompt.contains("group override body"));
        assert!(!prompt.contains("global body"));
    }

    #[test]
    fn build_skill_system_prompt_missing_dir_returns_empty() {
        let prompt = build_skill_system_prompt(
            Some(std::path::Path::new("/definitely/does/not/exist")),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
        );
        assert!(prompt.is_empty());
    }

    #[test]
    fn escape_attr_neutralises_quote_and_amp() {
        assert_eq!(escape_attr("plain"), "plain");
        assert_eq!(escape_attr("a&b"), "a&amp;b");
        assert_eq!(escape_attr("\"hi\""), "&quot;hi&quot;");
    }

    #[test]
    fn db_selector_conversion_roundtrips() {
        use ironclaw_db::tables::container_configs::SkillsSelector as DbSel;
        assert!(matches!(
            db_selector_to_skills_selector(&DbSel::All),
            ironclaw_skills::SkillsSelector::All
        ));
        let names = vec!["a".to_string(), "b".to_string()];
        let mapped = db_selector_to_skills_selector(&DbSel::Explicit(names.clone()));
        match mapped {
            ironclaw_skills::SkillsSelector::Explicit(out) => assert_eq!(out, names),
            ironclaw_skills::SkillsSelector::All => panic!("expected Explicit, got All"),
        }
    }

    #[test]
    fn runner_config_uses_skill_dir_when_configured() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None);
        assert!(rc.system.contains("alpha body"));
        assert!(rc.system.contains("<skill name=\"alpha\""));
    }
}
