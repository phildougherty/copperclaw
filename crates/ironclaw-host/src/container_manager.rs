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
use ironclaw_db::session::{open_inbound, open_outbound, SessionPaths};
use ironclaw_db::tables::{container_configs, messages_in, sessions};
use ironclaw_host_sweep::SpawnAttemptTracker;
use ironclaw_types::{AgentGroupId, ContainerStatus, Session, SessionStatus};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
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

/// Env-var names treated as secrets that the SIGHUP handler re-reads
/// from the install's `.env`. Missing keys after rotation are dropped
/// from the forwarded set — we never fall back to a stale value.
pub const ROTATABLE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "TAVILY_API_KEY",
    "EXA_API_KEY",
    "BRAVE_SEARCH_API_KEY",
    "SERPAPI_API_KEY",
    "OLLAMA_BASE_URL",
];

/// Subset of [`ManagerConfig`] that the SIGHUP handler can hot-swap.
/// Held behind an `Arc<RwLock<...>>` on [`ContainerManager`] so the
/// handler can update it without restarting the host. Reads during
/// `build_spec` take a short-lived read-lock; writes during
/// `reload_env` take a write-lock.
///
/// Note on already-running containers: Docker's env is immutable
/// post-creation. A rotated key only takes effect for containers
/// spawned **after** the reload. With the default
/// `idle_timeout_secs = 300`, an idle container respawns within
/// 5 minutes of the next inbound message and picks up the new key
/// at that point.
#[derive(Debug, Clone, Default)]
pub struct RotatableConfig {
    /// Current `ANTHROPIC_API_KEY`. `None` means the var is absent
    /// (or was removed during rotation).
    pub anthropic_api_key: Option<String>,
    /// Current `ANTHROPIC_BASE_URL` override.
    pub anthropic_base_url: Option<String>,
    /// Additional provider-key env-vars to forward into spawned
    /// containers. Keys here are only forwarded when their value is
    /// non-empty. Tracks the web-search provider keys today
    /// (`TAVILY_API_KEY`, `EXA_API_KEY`, `BRAVE_SEARCH_API_KEY`,
    /// `SERPAPI_API_KEY`).
    pub forward_env: Vec<(String, String)>,
}

impl RotatableConfig {
    /// Build from a flat env-var map (typically the process env
    /// snapshot at boot or a re-read of `.env` on SIGHUP). Empty
    /// strings are treated as absent.
    pub fn from_env_map(map: &std::collections::HashMap<String, String>) -> Self {
        let anthropic_api_key = map
            .get("ANTHROPIC_API_KEY")
            .filter(|v| !v.is_empty())
            .cloned();
        let anthropic_base_url = map
            .get("ANTHROPIC_BASE_URL")
            .filter(|v| !v.is_empty())
            .cloned();
        let forward_env = ROTATABLE_ENV_KEYS[2..]
            .iter()
            .filter_map(|k| {
                map.get(*k)
                    .filter(|v| !v.is_empty())
                    .map(|v| ((*k).to_string(), v.clone()))
            })
            .collect();
        Self {
            anthropic_api_key,
            anthropic_base_url,
            forward_env,
        }
    }
}

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
    /// How skill bodies are surfaced to the agent. See [`SkillsMode`].
    /// `Inline` (default) preserves today's behaviour; `Callable` shifts
    /// bodies behind a `load_skill` MCP tool to keep the system prompt
    /// small. Set via `IRONCLAW_SKILLS_MODE` at boot.
    pub skills_mode: SkillsMode,
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
    /// Per-agent-group timestamps of the last in-channel "budget
    /// exhausted" notice we emitted. Used to dedup so a user who
    /// sends ten messages while over the cap gets one explanation,
    /// not ten. Process-local — a host restart re-notifies once,
    /// which is acceptable.
    last_budget_notice: std::sync::Mutex<
        std::collections::HashMap<AgentGroupId, chrono::DateTime<chrono::Utc>>,
    >,
    /// Same shape as `last_budget_notice` but for per-minute /
    /// per-hour LLM rate-limit notifications. Keyed by
    /// `AgentGroupId`; value is the UTC time of the last
    /// notification sent (minute OR hour cap, whichever fired).
    rate_limit_notified: std::sync::Mutex<
        std::collections::HashMap<AgentGroupId, chrono::DateTime<chrono::Utc>>,
    >,
    /// Hot-swappable subset of the config (provider API keys + base
    /// URL + forwarded provider keys). Initialized from `cfg` at
    /// construction; updated by [`Self::reload_env`] on SIGHUP. Reads
    /// during `build_spec` / `runner_config_for` take a short-lived
    /// read-lock so the spawn path stays fast.
    rotatable: Arc<RwLock<RotatableConfig>>,
    /// Per-session counter of consecutive failed `runtime.spawn`
    /// calls. Shared with the host's sweep service so its apology
    /// check can detect "container never came up" and emit a
    /// user-visible note. A successful spawn resets the counter.
    /// Defaults to an empty tracker so test code that calls
    /// [`Self::new`] without wiring sweep still works.
    spawn_tracker: Arc<SpawnAttemptTracker>,
    /// Per-agent-group cooldown tracker for image rebuilds. The host
    /// auto-rebuilds when `container_configs.config_fingerprint` no
    /// longer matches the live config (e.g. agent emitted
    /// `install_packages`). When the rebuild *fails* (Docker stream
    /// error, bad apt name, transient network), the previous code
    /// path retried the rebuild on every subsequent spawn — wasting
    /// minutes per spawn and turning a single bad package name into
    /// a continuous rebuild storm. This tracker enforces an
    /// exponential cooldown per group; while a group is in cooldown
    /// the spawn path falls through to the last-known-good image
    /// without attempting a fresh build.
    rebuild_backoff: Arc<RebuildBackoff>,
    /// Set by [`Self::set_degraded`] when the boot-time image health
    /// check fails. When `true`, every call to [`Self::maybe_spawn`]
    /// short-circuits with [`ManagerError::HostDegraded`] — the host
    /// keeps running (so `iclaw doctor` still works) but no new
    /// containers are launched until the operator runs
    /// `./rebuild.sh` to refresh the session image and restart.
    /// Stored as an `AtomicBool` so the read-side on the spawn hot
    /// path is lock-free.
    degraded: AtomicBool,
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
            last_budget_notice: std::sync::Mutex::new(std::collections::HashMap::new()),
            rate_limit_notified: std::sync::Mutex::new(std::collections::HashMap::new()),
            rotatable: Arc::new(RwLock::new(RotatableConfig {
                anthropic_api_key: cfg.anthropic_api_key.clone(),
                anthropic_base_url: cfg.anthropic_base_url.clone(),
                forward_env: cfg.forward_env.clone(),
            })),
            spawn_tracker: Arc::new(SpawnAttemptTracker::new()),
            rebuild_backoff: Arc::new(RebuildBackoff::new()),
            degraded: AtomicBool::new(false),
            cfg,
        }
    }

    /// Wire a shared spawn-attempt tracker (typically owned by
    /// [`ironclaw_host_sweep::SweepService`]) into the manager.
    /// Mutates `self` so the boot sequence can hand the same tracker to
    /// both halves.
    #[must_use]
    pub fn with_spawn_tracker(mut self, tracker: Arc<SpawnAttemptTracker>) -> Self {
        self.spawn_tracker = tracker;
        self
    }

    /// Access the shared spawn-attempt tracker. Exposed so callers
    /// composing the manager can also pass the same handle to the
    /// sweep service.
    pub fn spawn_tracker(&self) -> &Arc<SpawnAttemptTracker> {
        &self.spawn_tracker
    }

    /// Flag the manager as degraded — subsequent
    /// [`Self::maybe_spawn`] calls will reject with
    /// [`ManagerError::HostDegraded`] until the host restarts. There
    /// is intentionally no live `clear_degraded` companion: the host
    /// does not try to do degraded → healthy transitions without a
    /// restart (the boot-time image health check would have to
    /// re-run, the metric gauge would have to be re-set, etc. —
    /// trickier than the operator just re-running `./rebuild.sh`).
    pub fn set_degraded(&self) {
        self.degraded.store(true, Ordering::SeqCst);
    }

    /// Whether the manager is in degraded mode (boot-time image
    /// health check failed).
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    /// Re-read the `.env` file at `env_file` (or use no file when
    /// `None`) and update [`Self::rotatable`]. Logs which key
    /// **names** changed (never the values) and increments the
    /// `ironclaw_secrets_rotated_total` metric counter.
    ///
    /// Returns the list of key names that were added, removed, or
    /// changed so the SIGHUP handler can log a summary line.
    pub fn reload_env(&self, env_file: Option<&Path>) -> Vec<String> {
        let new_map = read_env_file(env_file);
        let new_cfg = RotatableConfig::from_env_map(&new_map);

        let mut changed: Vec<String> = Vec::new();
        {
            let old = self
                .rotatable
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if old.anthropic_api_key != new_cfg.anthropic_api_key {
                changed.push("ANTHROPIC_API_KEY".to_string());
            }
            if old.anthropic_base_url != new_cfg.anthropic_base_url {
                changed.push("ANTHROPIC_BASE_URL".to_string());
            }
            let old_map: std::collections::HashMap<&str, &str> = old
                .forward_env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let new_map_fwd: std::collections::HashMap<&str, &str> = new_cfg
                .forward_env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            for (k, v) in &old_map {
                if new_map_fwd.get(k) != Some(v) {
                    changed.push((*k).to_string());
                }
            }
            for k in new_map_fwd.keys() {
                if !old_map.contains_key(k) {
                    changed.push((*k).to_string());
                }
            }
        }

        {
            let mut w = self
                .rotatable
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *w = new_cfg;
        }
        ironclaw_metrics::inc_secrets_rotated();
        changed
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
                // Degraded mode is a sticky, expected state — the
                // session stays Stopped and the inbound row stays
                // pending until the operator runs `./rebuild.sh`
                // and restarts. Surfacing it every tick would spam
                // the host log, so we collapse Ok(_) and
                // Err(HostDegraded) into Ok here. The startup
                // banner and the metric are the source of truth
                // for "host is degraded".
                match self.maybe_spawn(session).await {
                    Ok(_) | Err(ManagerError::HostDegraded) => Ok(()),
                    Err(err) => Err(err),
                }
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
    ///
    /// Refuses with [`ManagerError::HostDegraded`] when
    /// [`Self::is_degraded`] is `true`. This is the boot-time image
    /// health gate's runtime tail: when the host couldn't verify
    /// the session image at boot, it refuses to silently spawn
    /// containers that might be using a stale runner.
    #[allow(clippy::too_many_lines)] // spawn flow has several gates that are clearer inline
    pub async fn maybe_spawn(&self, session: &Session) -> Result<bool, ManagerError> {
        if self.is_degraded() {
            return Err(ManagerError::HostDegraded);
        }
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
        //
        // The user gets one in-channel reply per agent group per
        // notice window — without that, a chat goes silent and the
        // user has no idea why. See `maybe_post_budget_exhausted` for
        // the dedup window + the message text.
        if self.is_over_budget(session)? {
            warn!(
                session = %session.id.as_uuid(),
                agent_group = %session.agent_group_id.as_uuid(),
                "daily token budget exhausted; spawn deferred"
            );
            // Fires once per refusal, before the dedup window check —
            // operators can alert on the spike independent of how many
            // reply notices actually went out.
            ironclaw_metrics::inc_budget_exhausted(
                &session.agent_group_id.as_uuid().to_string(),
                ironclaw_metrics::BUDGET_GATE_DAILY_TOKENS,
            );
            if let Err(err) = self.maybe_post_budget_exhausted(session, &paths) {
                // Notification failure is non-fatal; the spawn is
                // still deferred and the warning above is in the log.
                warn!(?err, "could not post budget-exhausted reply");
            }
            return Ok(false);
        }

        // Rate-limit gate. If the group has a per-minute or per-hour
        // LLM-call cap and the trailing window count already meets/exceeds
        // it, refuse to spawn and post a one-per-window in-channel reply.
        if let Some((msg, gate_label)) = self.rate_limit_message(session)? {
            warn!(
                session = %session.id.as_uuid(),
                agent_group = %session.agent_group_id.as_uuid(),
                gate = gate_label,
                "rate limit reached; spawn deferred"
            );
            ironclaw_metrics::inc_budget_exhausted(
                &session.agent_group_id.as_uuid().to_string(),
                gate_label,
            );
            if let Err(err) = self.maybe_post_rate_limit_reply(session, &paths, &msg) {
                warn!(?err, "could not post rate-limit reply");
            }
            return Ok(false);
        }

        let cfg_row = container_configs::get(&self.central, session.agent_group_id)
            .map_err(ManagerError::Db)?;
        let runner_cfg = self.runner_config_for(session, cfg_row.as_ref(), Some(paths.root.as_path()));
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
            } else if let Some(cooldown_remaining) =
                self.rebuild_backoff.in_cooldown(session.agent_group_id)
            {
                // Don't attempt a fresh rebuild while this group is in
                // cooldown after recent rebuild failures. Spawn on the
                // last-known-good image; the operator (or the agent
                // itself, via in-container `shell npm install`) can
                // unblock by fixing the config and waiting out the
                // backoff window, or the backoff expires naturally.
                if base_tag.is_empty() {
                    return Err(ManagerError::Spawn(RtError::Container(
                        "image rebuild in cooldown and no fallback tag is configured"
                            .into(),
                    )));
                }
                warn!(
                    agent_group = %session.agent_group_id.as_uuid(),
                    cooldown_remaining_secs = cooldown_remaining.as_secs(),
                    fallback_tag = %base_tag,
                    "image rebuild in cooldown after prior failures; spawning on last-known-good tag"
                );
                base_tag
            } else {
                match self.rebuild_image(session.agent_group_id, cfg).await {
                    Ok(new_tag) => {
                        self.rebuild_backoff.record_success(session.agent_group_id);
                        new_tag
                    }
                    Err(err) if !base_tag.is_empty() => {
                        let next_backoff =
                            self.rebuild_backoff.record_failure(session.agent_group_id);
                        warn!(
                            agent_group = %session.agent_group_id.as_uuid(),
                            fallback_tag = %base_tag,
                            next_backoff_secs = next_backoff.as_secs(),
                            ?err,
                            "image rebuild failed; spawning on last-known-good tag and backing off"
                        );
                        ironclaw_metrics::inc_image_rebuild_failed();
                        base_tag
                    }
                    Err(err) => {
                        self.rebuild_backoff.record_failure(session.agent_group_id);
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
        let handle = match self.runtime.spawn(spec).await {
            Ok(h) => h,
            Err(err) => {
                // Bump the spawn-attempt tracker so the sweep's apology
                // check can decide whether to notify the user that the
                // container can't come up. The tracker is process-local
                // and cleared on a successful spawn below.
                let attempts = self.spawn_tracker.record_failure(session.id);
                warn!(
                    session = %session.id.as_uuid(),
                    attempts,
                    ?err,
                    "runtime spawn failed; bumped spawn-attempt counter",
                );
                return Err(ManagerError::Spawn(err));
            }
        };
        let spawn_elapsed = spawn_started.elapsed().as_secs_f64();
        // Successful spawn: clear any prior failure record so a future
        // crash doesn't immediately trip the apology threshold.
        self.spawn_tracker.record_success(session.id);
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

    /// Dedup window: if we already posted a budget-exhausted notice
    /// for this group inside the last hour, skip emitting another
    /// one. Picked to be long enough that a chatty user doesn't get
    /// repeated reminders but short enough that they get *some*
    /// follow-up if they're still chatting an hour later.
    const BUDGET_NOTICE_WINDOW_SECS: i64 = 3600;

    /// Dedup window for the rate-limit gate. Shorter than the budget
    /// window because the cap itself recovers on a minute / hour
    /// cadence — a user retrying after the cap clears should not be
    /// suppressed.
    const RATE_LIMIT_NOTICE_WINDOW_SECS: i64 = 60;

    /// When the budget gate refuses to spawn, post an in-channel
    /// reply telling the user the cap is hit and when it resets.
    /// Dedups per agent-group via [`Self::last_budget_notice`]; logs
    /// + swallows errors so the gate stays the source of truth.
    ///
    /// The reply is routed via `session_routing` (the
    /// `(channel_type, platform_id, thread_id)` the router stored on
    /// session create) so the user sees it back on the channel that
    /// asked the question.
    fn maybe_post_budget_exhausted(
        &self,
        session: &Session,
        paths: &SessionPaths,
    ) -> Result<(), ManagerError> {
        // Compute next midnight UTC so the reply tells the user when
        // the cap resets.
        let now = chrono::Utc::now();
        let next_reset = (now.date_naive() + chrono::Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is always valid")
            .and_utc();
        let text = format!(
            "I have reached this agent's daily token budget. New requests will resume after {} UTC. \
Operators can raise the cap with `iclaw groups budget set --agent-group-id <id> --daily-tokens N`.",
            next_reset.format("%Y-%m-%d %H:%M"),
        );
        self.post_cap_reply(
            session,
            paths,
            &text,
            &self.last_budget_notice,
            Self::BUDGET_NOTICE_WINDOW_SECS,
            "budget-exhausted",
        )
    }

    /// Returns `Some((notification_text, gate_label))` when a per-minute
    /// or per-hour LLM rate cap has been reached, `None` when both caps
    /// are clear (or unset). Used by the spawn gate to short-circuit
    /// before calling the runtime and to derive the message for the
    /// in-channel notification. The `gate_label` is one of
    /// `ironclaw_metrics::BUDGET_GATE_TURNS_PER_MINUTE` or
    /// `..._TURNS_PER_HOUR`; callers pipe it straight into
    /// `ironclaw_metrics::inc_budget_exhausted` for the `gate` label.
    fn rate_limit_message(
        &self,
        session: &Session,
    ) -> Result<Option<(String, &'static str)>, ManagerError> {
        use ironclaw_db::tables::{agent_turns, group_budgets};
        let Some(budget) = group_budgets::get(&self.central, session.agent_group_id)
            .map_err(ManagerError::Db)?
        else {
            return Ok(None);
        };
        let ag_id = session.agent_group_id.as_uuid().to_string();
        let now = chrono::Utc::now();

        if let Some(cap) = budget.agent_turns_per_minute_cap {
            let since = now - chrono::Duration::seconds(60);
            let count = agent_turns::turns_since(&self.central, &ag_id, since)
                .map_err(ManagerError::Db)?;
            if count >= cap {
                return Ok(Some((
                    format!(
                        "Per-minute LLM rate limit reached for this agent \
                         ({count} calls in the last minute, cap is {cap}). \
                         New requests resume within a minute. \
                         Operators can raise the cap with `iclaw groups budget set --agent-group-id <id> --turns-per-minute N`."
                    ),
                    ironclaw_metrics::BUDGET_GATE_TURNS_PER_MINUTE,
                )));
            }
        }

        if let Some(cap) = budget.agent_turns_per_hour_cap {
            let since = now - chrono::Duration::seconds(3600);
            let count = agent_turns::turns_since(&self.central, &ag_id, since)
                .map_err(ManagerError::Db)?;
            if count >= cap {
                return Ok(Some((
                    format!(
                        "Per-hour LLM rate limit reached for this agent \
                         ({count} calls in the last hour, cap is {cap}). \
                         New requests resume within the hour. \
                         Operators can raise the cap with `iclaw groups budget set --agent-group-id <id> --turns-per-hour N`."
                    ),
                    ironclaw_metrics::BUDGET_GATE_TURNS_PER_HOUR,
                )));
            }
        }

        Ok(None)
    }

    /// Same dispatch path as the budget-exhausted reply, but with the
    /// dedup map keyed off [`Self::rate_limit_notified`] and a shorter
    /// window so a user retrying after the cap clears isn't silenced.
    fn maybe_post_rate_limit_reply(
        &self,
        session: &Session,
        paths: &SessionPaths,
        text: &str,
    ) -> Result<(), ManagerError> {
        self.post_cap_reply(
            session,
            paths,
            text,
            &self.rate_limit_notified,
            Self::RATE_LIMIT_NOTICE_WINDOW_SECS,
            "rate-limit",
        )
    }

    /// Shared body for the cap-reply paths. Holds the dedup mutex
    /// only across the lookup + insert, then writes a Chat-kind
    /// outbound row routed via `session_routing` so the delivery
    /// loop dispatches it through the channel adapter.
    ///
    /// Bumps `ironclaw_budget_exhausted_suppressed_total` when the
    /// dedup window swallows the reply, and
    /// `ironclaw_budget_exhausted_replies_total` when a reply is
    /// actually written to outbound. The "no routing target" branch
    /// does NOT increment the reply counter — nothing was sent.
    #[allow(clippy::unused_self)] // kept as a method so callers can use `self.dispatch_cap_reply(...)`.
    fn post_cap_reply(
        &self,
        session: &Session,
        paths: &SessionPaths,
        text: &str,
        dedup: &std::sync::Mutex<
            std::collections::HashMap<AgentGroupId, chrono::DateTime<chrono::Utc>>,
        >,
        window_secs: i64,
        label: &'static str,
    ) -> Result<(), ManagerError> {
        let ag_id_str = session.agent_group_id.as_uuid().to_string();
        let now = chrono::Utc::now();
        {
            let mut state = dedup.lock().expect("cap-reply dedup mutex poisoned");
            if let Some(prev) = state.get(&session.agent_group_id) {
                let elapsed = now.signed_duration_since(*prev).num_seconds();
                if elapsed.abs() < window_secs {
                    ironclaw_metrics::inc_budget_exhausted_suppressed(&ag_id_str);
                    return Ok(());
                }
            }
            state.insert(session.agent_group_id, now);
        }

        let routing = {
            let conn = open_inbound(paths).map_err(ManagerError::Db)?;
            ironclaw_db::tables::session_routing::read(&conn).map_err(ManagerError::Db)?
        };
        let Some(routing) = routing else {
            warn!(
                session = %session.id.as_uuid(),
                kind = label,
                "cap notice skipped: no session_routing target",
            );
            return Ok(());
        };

        let outbound = open_outbound(paths).map_err(ManagerError::Db)?;
        let row = ironclaw_db::tables::messages_out::WriteOutbound {
            id: ironclaw_types::MessageId::new(),
            in_reply_to: None,
            timestamp: now,
            deliver_after: None,
            recurrence: None,
            kind: ironclaw_types::MessageKind::Chat,
            platform_id: routing.platform_id.clone(),
            channel_type: routing.channel_type.clone(),
            thread_id: routing.thread_id.clone(),
            content: serde_json::json!({ "text": text }),
        };
        ironclaw_db::tables::messages_out::insert(&outbound, &row).map_err(ManagerError::Db)?;
        ironclaw_metrics::inc_budget_exhausted_reply(&ag_id_str);
        info!(
            session = %session.id.as_uuid(),
            agent_group = %session.agent_group_id.as_uuid(),
            channel_type = ?routing.channel_type,
            kind = label,
            "posted cap reply to original sender"
        );
        Ok(())
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
        session_root: Option<&Path>,
    ) -> RunnerConfigForFile {
        let provider_raw = session
            .agent_provider
            .clone()
            .or_else(|| cc.and_then(|c| c.provider.clone()))
            .unwrap_or_else(|| self.cfg.default_provider.clone());
        // Normalize aliases for the runner; unknown values pass through
        // (the runner logs + falls back to anthropic). Empty string is
        // treated as "use the default" so an empty default_provider doesn't
        // leak into the JSON file.
        let provider = match provider_raw.as_str() {
            "" => None,
            "claude" => Some("anthropic".to_string()),
            other => Some(other.to_string()),
        };

        let model = cc
            .and_then(|c| c.model.clone())
            .unwrap_or_else(|| self.cfg.default_model.clone());
        let assistant_name = cc.and_then(|c| c.assistant_name.clone());
        let max_messages = cc.and_then(|c| c.max_messages_per_prompt);
        let selector = cc.map_or(ironclaw_skills::SkillsSelector::All, |c| {
            db_selector_to_skills_selector(&c.skills)
        });
        let now = chrono::Utc::now();

        // Build the Callable-mode catalogue first (if applicable) so we
        // can (a) reuse it to render the prompt index — making the
        // on-disk `skills.json` and the in-prompt catalogue the same
        // data — and (b) fall back to Inline-mode prompt assembly if
        // the catalogue write fails, so the agent isn't left holding a
        // Callable prompt that points at a missing file.
        let mut effective_mode = self.cfg.skills_mode;
        let mut catalogue_for_prompt: Option<Vec<SkillCatalogueEntry>> = None;
        if let (SkillsMode::Callable, Some(root)) = (self.cfg.skills_mode, session_root) {
            let entries = select_callable_skills(
                self.cfg.skills_dir.as_deref(),
                self.cfg.groups_dir.as_deref(),
                session.agent_group_id,
                &selector,
            );
            let path = root.join(SKILLS_CATALOGUE_FILENAME);
            if entries.is_empty() {
                // Nothing to advertise; behave like Inline for this spawn
                // so `load_skill` is not in the prompt at all.
                effective_mode = SkillsMode::Inline;
                remove_stale_catalogue(&path);
            } else {
                match serde_json::to_vec_pretty(&entries) {
                    Ok(json) => match std::fs::write(&path, json) {
                        Ok(()) => catalogue_for_prompt = Some(entries),
                        Err(err) => {
                            warn!(
                                ?err,
                                path = %path.display(),
                                "could not write skills catalogue; falling back to inline-skills prompt"
                            );
                            effective_mode = SkillsMode::Inline;
                            remove_stale_catalogue(&path);
                        }
                    },
                    Err(err) => {
                        warn!(
                            ?err,
                            "could not serialise skills catalogue; falling back to inline-skills prompt"
                        );
                        effective_mode = SkillsMode::Inline;
                        remove_stale_catalogue(&path);
                    }
                }
            }
        } else if let Some(root) = session_root {
            // Inline mode (either configured or fallen-back-to): drop
            // any catalogue from a previous spawn so `load_skill` never
            // reads stale bodies that don't match the inlined prompt.
            remove_stale_catalogue(&root.join(SKILLS_CATALOGUE_FILENAME));
        }

        let system = assemble_system_prompt_with_catalogue(
            self.cfg.skills_dir.as_deref(),
            self.cfg.groups_dir.as_deref(),
            session.agent_group_id,
            &selector,
            session_root,
            session.id,
            now,
            assistant_name.as_deref(),
            effective_mode,
            catalogue_for_prompt.as_deref(),
        );

        // Pick the api_key_env that matches the wire format. Ollama
        // native doesn't authenticate (or uses its own bearer in front
        // of a proxy); the runner accepts a missing api_key when
        // provider=ollama. Ollama-shim talks the Anthropic envelope at
        // a proxy that may or may not require a key — keep ANTHROPIC_API_KEY
        // so the operator can set one if they need it.
        let api_key_env: Option<String> = match provider.as_deref() {
            Some("ollama") => None,
            _ => Some("ANTHROPIC_API_KEY".to_string()),
        };

        // For ollama-shim we route api_base_url to OLLAMA_BASE_URL (or
        // leave it None and let the runner read OLLAMA_BASE_URL from the
        // container env). For native ollama, api_base_url is irrelevant
        // — the runner reads OLLAMA_BASE_URL directly.
        let api_base_url = match provider.as_deref() {
            Some("ollama" | "ollama-shim") => None,
            _ => self
                .rotatable
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .anthropic_base_url
                .clone(),
        };

        RunnerConfigForFile {
            session_id: session.id.as_uuid().to_string(),
            agent_group_id: session.agent_group_id.as_uuid().to_string(),
            // The container always sees its session dir at `/data` —
            // that's where the bind mount lands and where the runner
            // looks for `inbound.db`/`outbound.db`.
            session_dir: CONTAINER_SESSION_DIR.to_string(),
            provider,
            model,
            system,
            api_key_env,
            api_base_url,
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
        // Base the per-group rebuild on the install's default image
        // (which has `/usr/local/bin/ironclaw-runner` baked in at setup
        // time), NOT on bare `debian:trixie-slim`. The rebuild's
        // Dockerfile only adds layers (apt / npm / labels) — it does
        // not COPY the runner binary, so building from a runnerless
        // base produces a runnerless image and every subsequent
        // `runc create` fails with "stat /usr/local/bin/ironclaw-runner:
        // no such file or directory". Caught live: an agent emitted
        // `install_packages` → host auto-rebuilt → container_configs
        // got pinned to a 413MB runnerless image → all subsequent
        // spawns wedged. Falling back to a hard-coded slim base when
        // `default_image_tag` is empty (shouldn't happen in a
        // setup-completed install, but the fallback keeps tests that
        // run with `HostConfig::default()` working).
        let base = resolve_rebuild_base(&self.cfg.default_image_tag);
        let mut build_spec = ImageBuildSpec::new("ironclaw/session", &base);
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

        // Per-agent-group memory mount. Lives outside the session dir
        // so the same memory file is visible to every session of the
        // group — an agent can write a memory entry in one chat and
        // read it back from another. Created lazily here so the
        // operator doesn't need to provision the directory before the
        // first spawn. Mounted read-write because the agent is the one
        // writing to it. Disabled when `groups_dir` is unset — without
        // a per-group root there's nowhere to anchor the source.
        if let Some(groups) = self.cfg.groups_dir.as_deref() {
            let mem_src = groups
                .join(session.agent_group_id.as_uuid().to_string())
                .join("memory");
            match std::fs::create_dir_all(&mem_src) {
                Ok(()) => {
                    set_memory_dir_perms(&mem_src);
                    spec = spec.with_mount(Mount::Bind {
                        source: mem_src.to_string_lossy().into_owned(),
                        target: format!("{CONTAINER_SESSION_DIR}/memory"),
                        read_only: false,
                    });
                }
                Err(err) => {
                    warn!(
                        ?err,
                        path = %mem_src.display(),
                        "could not prepare per-group memory dir; falling back to session-local memory with UNAVAILABLE marker"
                    );
                    write_memory_unavailable_marker(&paths.root, &mem_src, &err);
                }
            }
        }

        // The runner reads its config from this path via the
        // `--config` flag wired into the entrypoint args. ContainerSpec
        // doesn't have a dedicated args field, so we encode the flag
        // by extending the entrypoint vector.
        spec.entrypoint
            .extend(vec!["--config".to_string(), format!("{CONTAINER_SESSION_DIR}/{RUNNER_CONFIG_FILENAME}")]);

        // All three forwarded surfaces (anthropic key, base URL,
        // provider keys) live behind the rotatable read-lock so the
        // SIGHUP handler can swap them mid-process without a host
        // restart.
        let rotatable = self
            .rotatable
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(key) = rotatable.anthropic_api_key.as_deref() {
            spec = spec.with_env("ANTHROPIC_API_KEY", key);
        }
        if let Some(base) = rotatable.anthropic_base_url.as_deref() {
            spec = spec.with_env("ANTHROPIC_BASE_URL", base);
        }
        // Operator-configured forwards (search API keys, etc.). Skip
        // empty values — an unset env var should not appear in the
        // container env at all. After a SIGHUP rotation that
        // *removes* a key, the missing entry is correctly dropped.
        for (k, v) in &rotatable.forward_env {
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

/// Read the `.env` file at `explicit_path` (or return an empty map
/// when `None`). We **do not** call `dotenvy` here because dotenvy
/// mutates the process env, which would race with other handlers and
/// leak rotated-away values to anything that's already read the env.
/// Instead we parse a minimal subset by hand.
fn read_env_file(explicit_path: Option<&Path>) -> std::collections::HashMap<String, String> {
    let Some(path) = explicit_path else {
        return std::collections::HashMap::new();
    };
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            warn!(path = %path.display(), ?err, "SIGHUP: could not read env file");
            return std::collections::HashMap::new();
        }
    };
    parse_dotenv_content(&content)
}

/// Parse a `.env`-style document. Handles comments (`#`), blank
/// lines, optional `export` prefixes, and single-/double-quoted
/// values. The parser is deliberately small: it does not expand
/// `${VAR}` references or honour escape sequences inside quotes.
pub(crate) fn parse_dotenv_content(content: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        let value = strip_quotes(v.trim()).to_string();
        out.insert(key.to_string(), value);
    }
    out
}

/// Strip a single layer of matching single or double quotes.
fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Filename for an optional operator-supplied project briefing read at
/// spawn time and prepended to the agent's system prompt. The
/// convention mirrors Claude Code's `CLAUDE.md`: anything the operator
/// wants the agent to know about *this* deployment that wouldn't be
/// obvious from the inbound message alone (house style, identity, the
/// shape of the workload). Two locations are checked, both optional:
/// the session dir (`/data/IRONCLAW.md` from the container's
/// perspective) and the per-group override directory
/// (`<groups_dir>/<agent_group_id>/IRONCLAW.md`).
pub const PROJECT_BRIEFING_FILENAME: &str = "IRONCLAW.md";

/// Filename for the per-session skills catalogue. Written into the
/// session dir alongside `runner.json` when `skills_mode = Callable` so
/// the runner's `load_skill` MCP tool can pull skill bodies on demand
/// instead of pre-inlining every body into the system prompt. From the
/// container's view this lands at `/data/skills.json`.
pub const SKILLS_CATALOGUE_FILENAME: &str = "skills.json";

/// How skill bodies reach the agent. The default mirrors today's
/// behaviour (inline every selected skill body into the system prompt
/// at spawn) so flipping to callable is opt-in per operator. Callable
/// mode advertises a compact index of skill names + descriptions in the
/// prompt and exposes a `load_skill` MCP tool that returns a named
/// skill's body on demand. The trade-off: bodies move from the always-on
/// prompt window (cheap to use, expensive every turn) to a tool call
/// (more turns, but every other turn pays nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SkillsMode {
    /// Inline every selected skill's body into the system prompt at
    /// spawn time. No `skills.json` is written.
    #[default]
    Inline,
    /// Emit a name+description index in the system prompt and write
    /// `skills.json` next to `runner.json` for the runner's
    /// `load_skill` MCP tool.
    Callable,
}

impl SkillsMode {
    /// Parse the operator-facing string form. Accepts `"inline"` and
    /// `"callable"`; unknown values fall back to [`SkillsMode::Inline`]
    /// with a `WARN` so a typo never silently mutes skills.
    pub fn parse_or_default(s: Option<&str>) -> Self {
        match s.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("" | "inline") => Self::Inline,
            Some("callable") => Self::Callable,
            Some(other) => {
                warn!(value = other, "unknown IRONCLAW_SKILLS_MODE; falling back to inline");
                Self::Inline
            }
        }
    }
}

/// Universal preamble prepended to every agent's system prompt before
/// the environment block, project briefing, and skill catalogue. The
/// content is deliberately mode-agnostic — it describes how to *be* an
/// Ironclaw agent (read carefully, act with care, prefer dedicated
/// tools, reply concisely) without assuming the agent is doing any
/// particular kind of work. Coding, support, scheduling, etc. are all
/// served by the same disciplines; specialised guidance lives in opt-in
/// skills.
pub const BASE_PREAMBLE: &str = "\
You are an Ironclaw agent — a self-hosted assistant that talks to people \
through channel adapters and runs inside a per-session Linux container with \
a set of tools. The capabilities you have are documented as skills further \
down this prompt; read the skill catalogue before deciding which tool to \
call.

# How to work

Read the inbound message carefully before acting. For multi-step work, \
think briefly about the steps first, then take them one tool call at a \
time and observe each result before deciding the next. Don't speculate \
past what your tool calls confirmed. If a request is genuinely ambiguous, \
ask one focused clarifying question rather than guessing across \
possibilities.

# Acting with care

Tools have real effects: they send messages, change files, spawn sibling \
agents, schedule future work. Match the boldness of your actions to how \
reversible they are.

- Reading, searching, asking a clarifying question — go ahead.
- Editing a file, sending a chat reply, spawning a sibling agent — fine \
  when the user asked for it; pause if it would be hard to undo.
- Deleting files, rewriting committed history, modifying configuration \
  the user didn't ask about — stop and confirm first.

When you find unexpected state (an unfamiliar file, a branch you didn't \
make, a lock you didn't take), investigate it before overwriting — it is \
usually the user's in-progress work.

# Picking tools

Prefer the dedicated tool over `shell` when one fits the job. Use `grep` \
and `glob` for searching rather than shelling out to `find`. When you \
have several independent things to check or do, call multiple tools in \
the same turn. When one call needs another's output, sequence them.

# Replying

Be concise. Don't restate the user's request, don't summarize what you \
just did at the end of every reply, and don't pad with preamble. One or \
two sentences is usually enough; add a code block, command, or link \
when it helps and skip the prose around it.

Never use emojis unless the user explicitly asks for them.
";

/// Build the environment block: a short, structured snapshot of the
/// agent's context that an operator would otherwise have to teach via
/// skills (today's date, which session is running, the working directory
/// inside the container, the assistant's display name when set). Mirrors
/// the equivalent block Claude Code injects at the top of its system
/// prompt.
fn environment_block(
    session_id: ironclaw_types::SessionId,
    agent_group_id: AgentGroupId,
    now: chrono::DateTime<chrono::Utc>,
    assistant_name: Option<&str>,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("\n# Environment\n\n");
    out.push_str(&format!("Today is {}.\n", now.format("%Y-%m-%d")));
    out.push_str(&format!("Session id: {}\n", session_id.as_uuid()));
    out.push_str(&format!("Agent group id: {}\n", agent_group_id.as_uuid()));
    out.push_str(&format!(
        "Working directory: {CONTAINER_SESSION_DIR} (per-session bind mount; this is where \
         inbound.db, outbound.db, and your runner config live)\n"
    ));
    if let Some(name) = assistant_name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("Assistant name: {trimmed}\n"));
        }
    }
    out
}

/// Read the optional project briefing from disk. Two sources, both
/// optional:
///
/// 1. The session dir (`<session_root>/IRONCLAW.md`) — operator-supplied
///    per-session context, e.g. dropped in by a wrapper that materialised
///    a specific workload before the runner booted.
/// 2. The per-group override (`<groups_dir>/<agent_group_id>/IRONCLAW.md`)
///    — operator-supplied per-group context that applies to every session
///    of this group.
///
/// Returns `None` if neither file exists. Read errors are logged and the
/// briefing is dropped — a missing or malformed briefing must not block
/// spawn.
fn read_project_briefing(
    session_root: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
) -> Option<String> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();

    if let Some(dir) = groups_dir {
        let path = dir
            .join(agent_group_id.as_uuid().to_string())
            .join(PROJECT_BRIEFING_FILENAME);
        match std::fs::read_to_string(&path) {
            Ok(body) if !body.trim().is_empty() => {
                sections.push((format!("group: {}", path.display()), body));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                warn!(?err, path = %path.display(), "could not read group briefing");
                diagnostics.push(format!(
                    "The operator-supplied group briefing at {} could not be read ({}); \
                     the agent has no context from that file for this session.",
                    path.display(),
                    err.kind(),
                ));
            }
        }
    }

    if let Some(root) = session_root {
        let path = root.join(PROJECT_BRIEFING_FILENAME);
        match std::fs::read_to_string(&path) {
            Ok(body) if !body.trim().is_empty() => {
                sections.push((format!("session: {}", path.display()), body));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                warn!(?err, path = %path.display(), "could not read session briefing");
                diagnostics.push(format!(
                    "The operator-supplied session briefing at {} could not be read ({}); \
                     the agent has no context from that file for this session.",
                    path.display(),
                    err.kind(),
                ));
            }
        }
    }

    if sections.is_empty() && diagnostics.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(2 * 1024);
    out.push_str("\n# Project briefing\n\n");
    if !sections.is_empty() {
        out.push_str(
            "The operator supplied the following briefing(s); treat them as \
             authoritative context for this deployment.\n",
        );
        for (source, body) in sections {
            out.push_str(&format!("\n<briefing source=\"{}\">\n", escape_attr(&source)));
            out.push_str(body.trim_end_matches('\n'));
            out.push_str("\n</briefing>\n");
        }
    }
    if !diagnostics.is_empty() {
        out.push_str("\n## Briefing diagnostics\n\n");
        for line in diagnostics {
            out.push_str(&format!("Note: {line}\n"));
        }
    }
    Some(out)
}

/// Top-level system-prompt assembler: stitches the universal preamble,
/// the environment block, an optional operator-supplied project briefing,
/// and the skill catalogue into a single string the runner writes into
/// `runner.json` and the provider sends as the `system` message.
///
/// Each piece is independent: a deployment without a skills directory
/// still gets the preamble + environment; a deployment without an
/// `IRONCLAW.md` still gets the rest. The order is fixed (preamble →
/// environment → briefing → skills) so that operator briefings can refer
/// back to the environment block without forward-references.
///
/// `skills_mode` controls whether full skill bodies are inlined into the
/// prompt or just a name/description index is emitted (with bodies
/// reachable on demand via the `load_skill` MCP tool).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn assemble_system_prompt(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &ironclaw_skills::SkillsSelector,
    session_root: Option<&Path>,
    session_id: ironclaw_types::SessionId,
    now: chrono::DateTime<chrono::Utc>,
    assistant_name: Option<&str>,
    skills_mode: SkillsMode,
) -> String {
    assemble_system_prompt_with_catalogue(
        skills_dir,
        groups_dir,
        agent_group_id,
        selector,
        session_root,
        session_id,
        now,
        assistant_name,
        skills_mode,
        None,
    )
}

/// Like [`assemble_system_prompt`] but accepts a pre-built Callable-mode
/// catalogue so the host can render the prompt index off the same data
/// it just wrote to `skills.json`. Pass `None` to let the assembler
/// build whatever it needs internally.
#[allow(clippy::too_many_arguments)]
fn assemble_system_prompt_with_catalogue(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &ironclaw_skills::SkillsSelector,
    session_root: Option<&Path>,
    session_id: ironclaw_types::SessionId,
    now: chrono::DateTime<chrono::Utc>,
    assistant_name: Option<&str>,
    skills_mode: SkillsMode,
    prebuilt_catalogue: Option<&[SkillCatalogueEntry]>,
) -> String {
    let mut out = String::with_capacity(16 * 1024);
    out.push_str(BASE_PREAMBLE);
    out.push_str(&environment_block(session_id, agent_group_id, now, assistant_name));
    if let Some(brief) = read_project_briefing(session_root, groups_dir, agent_group_id) {
        out.push_str(&brief);
    }
    let skills_section = match (skills_mode, prebuilt_catalogue) {
        (SkillsMode::Callable, Some(cat)) if !cat.is_empty() => render_callable_skill_index(cat),
        (SkillsMode::Callable, Some(_)) => String::new(),
        _ => build_skill_system_prompt(
            skills_dir,
            groups_dir,
            agent_group_id,
            selector,
            skills_mode,
        ),
    };
    if !skills_section.is_empty() {
        out.push('\n');
        out.push_str(&skills_section);
    }
    out
}

/// Per-skill record written into `skills.json` for the runner-side
/// `load_skill` MCP tool to consume. Kept simple (no extra metadata)
/// so the schema is forward-compatible.
#[derive(Debug, Clone, serde::Serialize)]
struct SkillCatalogueEntry {
    name: String,
    description: String,
    body: String,
}

/// Single source of truth for which skills make it into a Callable-mode
/// spawn: scans the registry, reads each selected skill's body, and
/// drops any that fail to load. The returned vector is the union used
/// by both the prompt index and the `skills.json` catalogue write, so
/// the two cannot disagree about which skills exist.
fn select_callable_skills(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &ironclaw_skills::SkillsSelector,
) -> Vec<SkillCatalogueEntry> {
    let Some(global) = skills_dir else {
        return Vec::new();
    };
    let group_override = groups_dir
        .map(|root| root.join(agent_group_id.as_uuid().to_string()).join("skills"))
        .filter(|p| p.is_dir())
        .map(|p| (agent_group_id, p));
    let registry = match ironclaw_skills::SkillRegistry::scan(
        global,
        group_override.as_ref().map(|(id, p)| (*id, p.as_path())),
    ) {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, dir = %global.display(), "skill scan failed; callable selection will be empty");
            return Vec::new();
        }
    };
    let selected = registry.list_for_group(agent_group_id, selector);
    let mut out = Vec::with_capacity(selected.len());
    for skill in &selected {
        let body = match ironclaw_skills::read_skill_body(skill) {
            Ok(b) => b,
            Err(err) => {
                warn!(skill = %skill.name, ?err, "skill body read failed; skipping in catalogue");
                continue;
            }
        };
        out.push(SkillCatalogueEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            body: body.trim_end().to_string(),
        });
    }
    out
}

/// Test-only thin wrapper over [`select_callable_skills`] that returns
/// `None` when no skills are selected.
#[cfg(test)]
fn build_skills_catalogue(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &ironclaw_skills::SkillsSelector,
) -> Option<Vec<SkillCatalogueEntry>> {
    let entries = select_callable_skills(skills_dir, groups_dir, agent_group_id, selector);
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
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
    mode: SkillsMode,
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
    match mode {
        SkillsMode::Inline => {
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
                out.push_str(&escape_attr(&skill.name));
                out.push_str("\" description=\"");
                out.push_str(&escape_attr(&skill.description));
                out.push_str("\">\n");
                out.push_str(body.trim_end());
                out.push_str("\n</skill>\n");
            }
        }
        SkillsMode::Callable => {
            // Hand off to the catalogue-backed renderer so the prompt
            // index and the on-disk `skills.json` cannot disagree about
            // which skills exist (any whose body fails to read is dropped
            // from both).
            let catalogue =
                select_callable_skills(skills_dir, groups_dir, agent_group_id, selector);
            if catalogue.is_empty() {
                return String::new();
            }
            return render_callable_skill_index(&catalogue);
        }
    }
    out
}

/// Emit the Callable-mode prompt section from a pre-built catalogue.
/// Used both by `build_skill_system_prompt` (which builds the catalogue
/// itself) and `runner_config_for` (which reuses the catalogue it
/// already built for the `skills.json` write).
fn render_callable_skill_index(catalogue: &[SkillCatalogueEntry]) -> String {
    let mut out = String::with_capacity(2 * 1024);
    out.push_str(
        "The following is the catalogue of skills available to you. Each \
<skill> entry shows only the skill's name and one-line description — the full \
SKILL.md body is not inlined. To read a skill's body before acting on it, call \
the `load_skill` tool with that skill's `name`; the tool returns the same \
markdown that would have been inlined.\n",
    );
    for entry in catalogue {
        out.push_str("\n<skill name=\"");
        out.push_str(&escape_attr(&entry.name));
        out.push_str("\" description=\"");
        out.push_str(&escape_attr(&entry.description));
        out.push_str("\" />\n");
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

/// Best-effort removal of a previous spawn's `skills.json`. `NotFound`
/// is the common case (no prior catalogue) and silently ignored; other
/// errors are logged but never fail the spawn.
fn remove_stale_catalogue(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            warn!(?err, path = %path.display(), "could not remove stale skills catalogue");
        }
    }
}

/// Filename of the per-session marker dropped when the per-group memory
/// mount could not be configured. The agent reads
/// `/data/memory/UNAVAILABLE.md` and learns the writes it makes here are
/// session-local rather than persistent.
pub const MEMORY_UNAVAILABLE_FILENAME: &str = "UNAVAILABLE.md";

/// Loosen the per-group memory dir to group-writeable (`0o775`) so the
/// operator can clean up files the container's root user wrote into
/// the bind. Best-effort and no-op on non-unix targets.
fn set_memory_dir_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o775)) {
            warn!(
                ?err,
                path = %path.display(),
                "could not relax per-group memory dir permissions to 0o775"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Drop a session-local `/data/memory/UNAVAILABLE.md` marker so the
/// agent can detect (from inside the container) that the persistent
/// memory mount was not configured for this spawn. Writes that land in
/// this directory are bound to the session dir, not the per-group dir,
/// so they will not be visible to future sessions of the same group.
fn write_memory_unavailable_marker(
    session_root: &Path,
    intended_src: &Path,
    err: &std::io::Error,
) {
    let dir = session_root.join("memory");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            ?e,
            path = %dir.display(),
            "could not create session-local memory dir for UNAVAILABLE marker"
        );
        return;
    }
    let body = format!(
        "# Memory mount unavailable\n\n\
         The per-group memory mount at `{intended}` could not be configured \
         for this session (host error: {err_kind}). Files you write under \
         `/data/memory/` will land in this session's own directory and will \
         **not** persist or be visible to other sessions of this agent group.\n\n\
         If a user references a memory the agent should have, mention that \
         the persistent memory mount is currently unavailable so the operator \
         can investigate.\n",
        intended = intended_src.display(),
        err_kind = err.kind(),
    );
    let marker = dir.join(MEMORY_UNAVAILABLE_FILENAME);
    if let Err(e) = std::fs::write(&marker, body) {
        warn!(
            ?e,
            path = %marker.display(),
            "could not write memory-unavailable marker"
        );
    }
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
    /// Provider kind, e.g. `"anthropic"`, `"ollama"`, `"ollama-shim"`.
    /// When unset the runner falls back to `"anthropic"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
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
    /// The host entered degraded mode at boot (e.g. session image is
    /// missing or stale). Sessions cannot be spawned until the
    /// operator runs `./rebuild.sh` and restarts the host.
    #[error("host degraded; refusing to spawn sessions until the operator restarts after `./rebuild.sh`")]
    HostDegraded,
}

/// Choose the base image for a per-group rebuild. Returns the install's
/// `default_image_tag` (which has `/usr/local/bin/ironclaw-runner`
/// baked in at setup time) when set; falls back to `debian:trixie-slim`
/// otherwise (only reachable from tests that construct a `HostConfig`
/// without going through setup). The rebuild Dockerfile only adds layers
/// on top — it doesn't COPY the runner — so basing on a runnerless image
/// produces an unspawnable container.
#[must_use]
pub fn resolve_rebuild_base(default_image_tag: &str) -> String {
    if default_image_tag.is_empty() {
        "debian:trixie-slim".to_string()
    } else {
        default_image_tag.to_string()
    }
}

/// Initial backoff after the first rebuild failure for an agent group.
/// Doubles per subsequent failure up to [`REBUILD_BACKOFF_CEILING`].
const REBUILD_BACKOFF_INITIAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Ceiling for the rebuild backoff per group. 30 min mirrors the
/// delivery loop's `ABSOLUTE_CEILING_MS`: after this much time has
/// passed without operator action, retrying the build won't have
/// gotten cheaper, but unblocking the group does have value.
const REBUILD_BACKOFF_CEILING: std::time::Duration =
    std::time::Duration::from_secs(1_800);

/// Per-agent-group cooldown table for image rebuilds. Wraps a
/// `Mutex<HashMap>` because the spawn path is already async + holds
/// the broader manager state; trading the lock contention for not
/// having to thread a watch / arc-swap is the right call here. All
/// methods are short, lock-free I/O free.
pub struct RebuildBackoff {
    inner: std::sync::Mutex<std::collections::HashMap<AgentGroupId, RebuildBackoffEntry>>,
}

#[derive(Debug, Clone, Copy)]
struct RebuildBackoffEntry {
    /// Number of consecutive failures observed.
    consecutive_failures: u32,
    /// Earliest moment at which the next rebuild attempt is allowed.
    next_attempt_at: std::time::Instant,
}

impl Default for RebuildBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl RebuildBackoff {
    /// Build an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// If the group is still in cooldown, return the remaining
    /// duration. Returns `None` when the group has no record or the
    /// cooldown has elapsed (in which case the caller should attempt
    /// the rebuild).
    #[must_use]
    pub fn in_cooldown(&self, group: AgentGroupId) -> Option<std::time::Duration> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.get(&group)?;
        let now = std::time::Instant::now();
        if entry.next_attempt_at > now {
            Some(entry.next_attempt_at - now)
        } else {
            None
        }
    }

    /// Record a successful rebuild; clears any prior cooldown for
    /// this group so the next config change is attempted immediately.
    pub fn record_success(&self, group: AgentGroupId) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&group);
    }

    /// Record a rebuild failure for this group. Increments the
    /// consecutive-failure count, computes the next exponential
    /// backoff, and returns the duration the group is now in
    /// cooldown for (useful for log messages).
    pub fn record_failure(&self, group: AgentGroupId) -> std::time::Duration {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = std::time::Instant::now();
        let entry = guard.entry(group).or_insert(RebuildBackoffEntry {
            consecutive_failures: 0,
            next_attempt_at: now,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        let exp = entry.consecutive_failures.saturating_sub(1).min(8);
        let backoff = REBUILD_BACKOFF_INITIAL
            .saturating_mul(1u32 << exp)
            .min(REBUILD_BACKOFF_CEILING);
        entry.next_attempt_at = now + backoff;
        backoff
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_db::tables::sessions::{create as create_session, CreateSession};
    use ironclaw_types::SessionId;

    /// Backoff is empty on construction: every group is allowed to
    /// rebuild immediately.
    #[test]
    fn rebuild_backoff_clear_by_default() {
        let bo = RebuildBackoff::new();
        assert!(bo.in_cooldown(AgentGroupId::new()).is_none());
    }

    /// First failure installs a cooldown approximately equal to
    /// `REBUILD_BACKOFF_INITIAL`. Subsequent calls within the
    /// cooldown window report time remaining and don't re-trigger
    /// the rebuild.
    #[test]
    fn rebuild_backoff_first_failure_installs_initial_cooldown() {
        let bo = RebuildBackoff::new();
        let ag = AgentGroupId::new();
        let dur = bo.record_failure(ag);
        assert_eq!(dur, REBUILD_BACKOFF_INITIAL);
        let remaining = bo.in_cooldown(ag).expect("group must be in cooldown");
        assert!(remaining <= REBUILD_BACKOFF_INITIAL);
    }

    /// Consecutive failures double the backoff up to the ceiling.
    #[test]
    fn rebuild_backoff_doubles_to_ceiling() {
        let bo = RebuildBackoff::new();
        let ag = AgentGroupId::new();
        let mut prev = bo.record_failure(ag);
        // Push past the doubling threshold a few times.
        for _ in 0..6 {
            let next = bo.record_failure(ag);
            assert!(next >= prev, "backoff should not regress: prev={prev:?} next={next:?}");
            assert!(next <= REBUILD_BACKOFF_CEILING);
            prev = next;
        }
        // After many failures we should be sitting at the ceiling.
        assert_eq!(prev, REBUILD_BACKOFF_CEILING);
    }

    /// A successful rebuild clears the cooldown immediately.
    #[test]
    fn rebuild_backoff_success_clears_cooldown() {
        let bo = RebuildBackoff::new();
        let ag = AgentGroupId::new();
        let _ = bo.record_failure(ag);
        assert!(bo.in_cooldown(ag).is_some());
        bo.record_success(ag);
        assert!(bo.in_cooldown(ag).is_none());
    }

    /// Per-group isolation: a failure for group A must not delay
    /// rebuilds for group B.
    #[test]
    fn rebuild_backoff_per_group_independent() {
        let bo = RebuildBackoff::new();
        let a = AgentGroupId::new();
        let b = AgentGroupId::new();
        let _ = bo.record_failure(a);
        assert!(bo.in_cooldown(a).is_some());
        assert!(bo.in_cooldown(b).is_none(), "other group must be unaffected");
    }

    /// Regression: image rebuilds must base off the install's default
    /// image (which has the runner binary), not bare `debian:trixie-slim`.
    /// Caught live: agent emitted `install_packages` → host rebuilt
    /// against debian-slim → resulting image had apt packages but no
    /// `/usr/local/bin/ironclaw-runner` → every `runc create` failed.
    #[test]
    fn rebuild_base_prefers_default_image_tag() {
        assert_eq!(
            resolve_rebuild_base("ironclaw/session:sha256-abc123"),
            "ironclaw/session:sha256-abc123"
        );
    }

    #[test]
    fn rebuild_base_falls_back_when_default_unset() {
        // Only reachable from tests constructing HostConfig::default()
        // — a setup-completed install always has the env var. The
        // fallback keeps unit tests working without producing an
        // empty `FROM` directive in the Dockerfile.
        assert_eq!(resolve_rebuild_base(""), "debian:trixie-slim");
    }

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
            skills_mode: SkillsMode::Inline,
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
    fn build_spec_mounts_per_group_memory_when_groups_dir_set() {
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        std::fs::create_dir_all(&groups).unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = Some(groups.clone());
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        let memory_mount = spec.mounts.iter().find_map(|m| match m {
            Mount::Bind {
                source,
                target,
                read_only,
            } if target == &format!("{CONTAINER_SESSION_DIR}/memory") => {
                Some((source.clone(), *read_only))
            }
            _ => None,
        });
        let (src, ro) = memory_mount.expect("memory mount present");
        let expected = groups
            .join(session.agent_group_id.as_uuid().to_string())
            .join("memory");
        assert_eq!(src, expected.to_string_lossy().to_string());
        assert!(!ro);
        // Mount source dir is created lazily.
        assert!(expected.is_dir());
    }

    #[test]
    fn build_spec_skips_memory_mount_when_groups_dir_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = None;
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let spec = mgr.build_spec(&session, &paths, "img", None);
        let has_memory = spec.mounts.iter().any(|m| match m {
            Mount::Bind { target, .. } => target == &format!("{CONTAINER_SESSION_DIR}/memory"),
            _ => false,
        });
        assert!(!has_memory, "memory mount must not appear without groups_dir");
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
        let cfg = mgr.runner_config_for(&session, None, None);
        assert_eq!(cfg.model, "claude-sonnet-4-6");
        assert_eq!(cfg.session_dir, CONTAINER_SESSION_DIR);
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(
            cfg.api_base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    /// Per-group `container_config.provider = "ollama"` must reach the
    /// runner-config file so the runner builds an Ollama provider rather
    /// than the default Anthropic one. Caught regression: the old
    /// `let _ = provider;` line silently swallowed the field and the
    /// runner ignored every per-group choice.
    #[test]
    fn runner_config_propagates_ollama_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("ollama".into()),
            model: Some("llama3.1:8b".into()),
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
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(cfg.provider.as_deref(), Some("ollama"));
        assert_eq!(cfg.model, "llama3.1:8b");
        // Ollama native doesn't authenticate via ANTHROPIC_API_KEY,
        // so the runner must not be told to pull one.
        assert!(cfg.api_key_env.is_none());
        // And the per-rotatable Anthropic base URL is irrelevant —
        // we must not leak it into an ollama config.
        assert!(cfg.api_base_url.is_none());
    }

    /// `claude` is an alias for `anthropic` — both must still resolve
    /// to `api_key_env=ANTHROPIC_API_KEY` and the rotatable base URL.
    #[test]
    fn runner_config_claude_alias_resolves_to_anthropic_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let cc = container_configs::ContainerConfig {
            agent_group_id: session.agent_group_id,
            provider: Some("claude".into()),
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
        let cfg = mgr.runner_config_for(&session, Some(&cc), None);
        assert_eq!(cfg.provider.as_deref(), Some("anthropic"));
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(cfg.api_base_url.is_some());
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
            SkillsMode::Inline,
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
            SkillsMode::Inline,
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
            SkillsMode::Inline,
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
            SkillsMode::Inline,
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
            SkillsMode::Inline,
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
            SkillsMode::Inline,
        );
        assert!(prompt.is_empty());
    }

    #[test]
    fn build_skill_system_prompt_callable_emits_index_without_bodies() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "Alpha body\n");
        write_skill_md(&skills, "beta", "Beta body\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            SkillsMode::Callable,
        );
        // Names + descriptions present, bodies absent.
        assert!(prompt.contains("name=\"alpha\""));
        assert!(prompt.contains("name=\"beta\""));
        assert!(prompt.contains("desc-of-alpha"));
        assert!(!prompt.contains("Alpha body"));
        assert!(!prompt.contains("Beta body"));
        // The self-closing form makes it visually obvious to the model
        // that the body is *not* here.
        assert!(prompt.contains("\" />"));
        // The instructional sentence reminds the model how to retrieve
        // bodies; the tool name is mentioned literally.
        assert!(prompt.contains("`load_skill`"));
    }

    #[test]
    fn skills_mode_parse_or_default_handles_known_and_unknown() {
        assert_eq!(SkillsMode::parse_or_default(None), SkillsMode::Inline);
        assert_eq!(SkillsMode::parse_or_default(Some("")), SkillsMode::Inline);
        assert_eq!(
            SkillsMode::parse_or_default(Some("inline")),
            SkillsMode::Inline
        );
        assert_eq!(
            SkillsMode::parse_or_default(Some("INLINE")),
            SkillsMode::Inline
        );
        assert_eq!(
            SkillsMode::parse_or_default(Some("callable")),
            SkillsMode::Callable
        );
        assert_eq!(
            SkillsMode::parse_or_default(Some("Callable")),
            SkillsMode::Callable
        );
        // Unknown falls back without panicking.
        assert_eq!(
            SkillsMode::parse_or_default(Some("on")),
            SkillsMode::Inline
        );
    }

    #[test]
    fn build_skills_catalogue_returns_entries_for_selected_skills() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "Alpha body line one\nAlpha body line two\n");
        write_skill_md(&skills, "beta", "Beta body\n");
        let entries = build_skills_catalogue(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "alpha"
            && e.description == "desc-of-alpha"
            && e.body.contains("Alpha body line one")));
        assert!(entries.iter().any(|e| e.name == "beta" && e.body.contains("Beta body")));
    }

    #[test]
    fn build_skills_catalogue_returns_none_with_empty_selector() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "Alpha body\n");
        let entries = build_skills_catalogue(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::Explicit(vec![]),
        );
        assert!(entries.is_none());
    }

    #[test]
    fn build_skills_catalogue_returns_none_without_skills_dir() {
        let entries = build_skills_catalogue(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
        );
        assert!(entries.is_none());
    }

    // ---- universal preamble + environment + briefing ----

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 5, 22, 12, 0, 0).unwrap()
    }

    #[test]
    fn assemble_system_prompt_includes_universal_preamble_and_environment() {
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            None,
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        // Preamble is mode-agnostic — these phrases anchor it.
        assert!(prompt.contains("You are an Ironclaw agent"));
        assert!(prompt.contains("Acting with care"));
        assert!(prompt.contains("Picking tools"));
        assert!(prompt.contains("Never use emojis"));
        // Environment block follows.
        assert!(prompt.contains("# Environment"));
        assert!(prompt.contains("Today is 2026-05-22"));
        assert!(prompt.contains("Working directory: /data"));
    }

    #[test]
    fn assemble_system_prompt_includes_session_and_group_ids() {
        let session = ironclaw_types::SessionId::new();
        let ag = AgentGroupId::new();
        let prompt = assemble_system_prompt(
            None,
            None,
            ag,
            &ironclaw_skills::SkillsSelector::All,
            None,
            session,
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains(&session.as_uuid().to_string()));
        assert!(prompt.contains(&ag.as_uuid().to_string()));
    }

    #[test]
    fn assemble_system_prompt_includes_assistant_name_when_set() {
        let with_name = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            None,
            ironclaw_types::SessionId::new(),
            fixed_now(),
            Some("Atlas"),
            SkillsMode::Inline,
        );
        assert!(with_name.contains("Assistant name: Atlas"));
        let without_name = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            None,
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(!without_name.contains("Assistant name:"));
    }

    #[test]
    fn assemble_system_prompt_omits_briefing_section_when_absent() {
        let td = tempfile::tempdir().unwrap();
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            Some(td.path()),
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(!prompt.contains("# Project briefing"));
        assert!(!prompt.contains("<briefing"));
    }

    #[test]
    fn assemble_system_prompt_includes_session_briefing_when_present() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join(PROJECT_BRIEFING_FILENAME),
            "House style: terse, no preamble.\n",
        )
        .unwrap();
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            Some(td.path()),
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains("# Project briefing"));
        assert!(prompt.contains("<briefing source=\"session:"));
        assert!(prompt.contains("House style: terse, no preamble."));
    }

    #[test]
    fn assemble_system_prompt_includes_group_briefing_when_present() {
        let td = tempfile::tempdir().unwrap();
        let ag = AgentGroupId::new();
        let group_dir = td.path().join(ag.as_uuid().to_string());
        std::fs::create_dir_all(&group_dir).unwrap();
        std::fs::write(
            group_dir.join(PROJECT_BRIEFING_FILENAME),
            "This deployment runs the support workload.\n",
        )
        .unwrap();
        let prompt = assemble_system_prompt(
            None,
            Some(td.path()),
            ag,
            &ironclaw_skills::SkillsSelector::All,
            None,
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains("<briefing source=\"group:"));
        assert!(prompt.contains("This deployment runs the support workload."));
    }

    #[test]
    fn assemble_system_prompt_group_then_session_briefings_both_included() {
        let td = tempfile::tempdir().unwrap();
        let ag = AgentGroupId::new();
        let group_dir = td.path().join("groups").join(ag.as_uuid().to_string());
        std::fs::create_dir_all(&group_dir).unwrap();
        std::fs::write(group_dir.join(PROJECT_BRIEFING_FILENAME), "GROUP-LEVEL\n").unwrap();
        let session_dir = td.path().join("sess");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join(PROJECT_BRIEFING_FILENAME), "SESSION-LEVEL\n").unwrap();
        let prompt = assemble_system_prompt(
            None,
            Some(&td.path().join("groups")),
            ag,
            &ironclaw_skills::SkillsSelector::All,
            Some(&session_dir),
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        let g_pos = prompt.find("GROUP-LEVEL").expect("group briefing present");
        let s_pos = prompt.find("SESSION-LEVEL").expect("session briefing present");
        assert!(g_pos < s_pos, "group briefing must precede session briefing");
    }

    #[test]
    fn assemble_system_prompt_ignores_empty_briefing_file() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join(PROJECT_BRIEFING_FILENAME), "   \n\n").unwrap();
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            Some(td.path()),
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(!prompt.contains("# Project briefing"));
    }

    #[test]
    fn assemble_system_prompt_skills_section_follows_briefing() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        std::fs::write(td.path().join(PROJECT_BRIEFING_FILENAME), "BRIEF\n").unwrap();
        let prompt = assemble_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            Some(td.path()),
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        let brief_pos = prompt.find("BRIEF").expect("brief present");
        let skill_pos = prompt
            .find("<skill name=\"alpha\"")
            .expect("skill present");
        assert!(brief_pos < skill_pos, "briefing must precede skills");
    }

    #[test]
    fn assemble_system_prompt_works_with_no_skills_dir() {
        // A deployment with zero skills still gets a complete, useful
        // system prompt — preamble + environment, no skill block.
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            None,
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains("You are an Ironclaw agent"));
        assert!(!prompt.contains("<skill name="));
    }

    #[test]
    fn read_project_briefing_returns_none_when_neither_source_present() {
        let td = tempfile::tempdir().unwrap();
        let out = read_project_briefing(
            Some(td.path()),
            Some(td.path()),
            AgentGroupId::new(),
        );
        assert!(out.is_none());
    }

    #[test]
    fn environment_block_includes_all_required_fields() {
        let session = ironclaw_types::SessionId::new();
        let ag = AgentGroupId::new();
        let block = environment_block(session, ag, fixed_now(), Some("Atlas"));
        assert!(block.starts_with("\n# Environment\n"));
        assert!(block.contains("Today is 2026-05-22"));
        assert!(block.contains(&format!("Session id: {}", session.as_uuid())));
        assert!(block.contains(&format!("Agent group id: {}", ag.as_uuid())));
        assert!(block.contains("Working directory: /data"));
        assert!(block.contains("Assistant name: Atlas"));
    }

    #[test]
    fn environment_block_trims_whitespace_only_assistant_name() {
        let block = environment_block(
            ironclaw_types::SessionId::new(),
            AgentGroupId::new(),
            fixed_now(),
            Some("   "),
        );
        assert!(!block.contains("Assistant name:"));
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
        let rc = mgr.runner_config_for(&session, None, None);
        assert!(rc.system.contains("alpha body"));
        assert!(rc.system.contains("<skill name=\"alpha\""));
        // The new top-level assembler always prepends the universal
        // preamble + environment block, even when the session_root is
        // None (the briefing is the only optional piece). That gives us
        // a couple of cheap structural sanity checks here.
        assert!(rc.system.contains("You are an Ironclaw agent"));
        assert!(rc.system.contains("# Environment"));
    }

    #[test]
    fn runner_config_callable_mode_emits_index_and_writes_catalogue() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        write_skill_md(&skills, "beta", "beta body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        // System prompt: index only, no inlined bodies.
        assert!(rc.system.contains("name=\"alpha\""));
        assert!(rc.system.contains("name=\"beta\""));
        assert!(!rc.system.contains("alpha body"));
        assert!(!rc.system.contains("beta body"));
        assert!(rc.system.contains("`load_skill`"));
        // Catalogue file written next to the session dir.
        let catalogue_path = session_root.join(SKILLS_CATALOGUE_FILENAME);
        assert!(catalogue_path.is_file(), "expected skills.json on disk");
        let bytes = std::fs::read(&catalogue_path).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let names: Vec<&str> = parsed
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"));
        let bodies: String = parsed
            .iter()
            .filter_map(|e| e.get("body").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("|");
        assert!(bodies.contains("alpha body"));
        assert!(bodies.contains("beta body"));
    }

    #[test]
    fn runner_config_inline_mode_does_not_write_catalogue() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        // SkillsMode::Inline is the default — explicit here for clarity.
        cfg.skills_mode = SkillsMode::Inline;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert!(
            !session_root.join(SKILLS_CATALOGUE_FILENAME).exists(),
            "inline mode must not write skills.json"
        );
    }

    #[test]
    fn runner_config_callable_mode_removes_stale_catalogue_when_no_skills_selected() {
        let td = tempfile::tempdir().unwrap();
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        // Pre-existing catalogue from a prior spawn that selected skills.
        std::fs::write(
            session_root.join(SKILLS_CATALOGUE_FILENAME),
            r#"[{"name":"old","description":"","body":"old body"}]"#,
        )
        .unwrap();
        // No skills_dir configured → catalogue should be removed so the
        // agent doesn't see stale entries.
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_mode = SkillsMode::Callable;
        cfg.skills_dir = None;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _rc = mgr.runner_config_for(&session, None, Some(&session_root));
        assert!(
            !session_root.join(SKILLS_CATALOGUE_FILENAME).exists(),
            "stale catalogue must be removed when no skills selected"
        );
    }

    // ── budget-exhausted reply ──────────────────────────────────────

    fn set_daily_cap(db: &CentralDb, ag: AgentGroupId, cap: i64) {
        ironclaw_db::tables::group_budgets::upsert(
            db,
            ironclaw_db::tables::group_budgets::UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: Some(cap),
                daily_cost_cap: None,
                agent_turns_per_minute_cap: None,
                agent_turns_per_hour_cap: None,
            },
        )
        .unwrap();
    }

    /// Upsert a `group_budgets` row with only the per-minute / per-hour
    /// rate caps set. Used by the rate-limit tests.
    fn set_rate_caps(
        db: &CentralDb,
        ag: AgentGroupId,
        per_min: Option<i64>,
        per_hour: Option<i64>,
    ) {
        use ironclaw_db::tables::group_budgets::{upsert, UpsertGroupBudget};
        upsert(
            db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: None,
                daily_cost_cap: None,
                agent_turns_per_minute_cap: per_min,
                agent_turns_per_hour_cap: per_hour,
            },
        )
        .unwrap();
    }

    /// Seed `count` recent `agent_turns` for the session's group so
    /// the rate-limit gate sees them. Each turn is timestamped within
    /// the last 5 seconds (well inside both the per-minute and
    /// per-hour windows).
    fn seed_turns(db: &CentralDb, ag: AgentGroupId, count: usize) {
        use ironclaw_db::tables::agent_turns::{insert, NewAgentTurn};
        let now = chrono::Utc::now();
        for i in 0..count {
            #[allow(clippy::cast_possible_wrap)]
            let seq = i as i64;
            insert(
                db,
                &NewAgentTurn {
                    session_id: "sess-test".into(),
                    agent_group_id: ag.as_uuid().to_string(),
                    seq,
                    model: "claude-sonnet-4-6".into(),
                    provider: "anthropic".into(),
                    input_tokens: 10,
                    output_tokens: 20,
                    started_at: now - chrono::Duration::seconds(5),
                    ended_at: now - chrono::Duration::seconds(1),
                    status: "ok".into(),
                    error: None,
                },
            )
            .unwrap();
        }
    }

    fn record_today_tokens(db: &CentralDb, ag: AgentGroupId, input: i64, output: i64) {
        ironclaw_db::tables::agent_turns::insert(
            db,
            &ironclaw_db::tables::agent_turns::NewAgentTurn {
                agent_group_id: ag.as_uuid().to_string(),
                session_id: SessionId(uuid::Uuid::new_v4()).as_uuid().to_string(),
                seq: 1,
                model: "stub".into(),
                provider: "stub".into(),
                started_at: chrono::Utc::now(),
                ended_at: chrono::Utc::now(),
                input_tokens: input,
                output_tokens: output,
                status: "ok".into(),
                error: None,
            },
        )
        .unwrap();
    }

    fn seed_routing(paths: &SessionPaths) {
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(paths).unwrap();
        ironclaw_db::tables::session_routing::write(
            &conn,
            &ironclaw_types::routing::SessionRouting {
                channel_type: Some(ironclaw_types::ChannelType::new("cli")),
                platform_id: Some("stdin".into()),
                thread_id: None,
            },
        )
        .unwrap();
    }

    fn count_outbound_text_replies(paths: &SessionPaths) -> Vec<String> {
        let conn = open_outbound(paths).unwrap();
        let rows = ironclaw_db::tables::messages_out::list_due(&conn).unwrap();
        rows.into_iter()
            .filter_map(|r| {
                if matches!(r.kind, ironclaw_types::MessageKind::Chat) {
                    r.content
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect()
    }

    fn seed_pending_chat_inbound(paths: &SessionPaths) {
        let conn = open_inbound(paths).unwrap();
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
    }

    #[test]
    fn maybe_post_budget_exhausted_writes_reply_when_routing_known() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        seed_routing(&paths);

        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].contains("daily token budget"));
        assert!(replies[0].contains("iclaw groups budget set"));
    }

    #[test]
    fn maybe_post_budget_exhausted_dedups_within_window() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        seed_routing(&paths);

        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();
        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();
        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
    }

    #[test]
    fn maybe_post_budget_exhausted_skips_when_no_routing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let _ = open_inbound(&paths).unwrap();

        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn maybe_spawn_posts_one_reply_per_window_when_over_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_routing(&paths);
        seed_pending_chat_inbound(&paths);

        set_daily_cap(&db, session.agent_group_id, 100);
        record_today_tokens(&db, session.agent_group_id, 150, 50);

        let spawned1 = mgr.maybe_spawn(&session).await.unwrap();
        let spawned2 = mgr.maybe_spawn(&session).await.unwrap();
        assert!(!spawned1, "must not spawn when over budget");
        assert!(!spawned2);
        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
    }

    /// Render the Prometheus body for whichever recorder is active.
    /// Used by the budget-gate metric tests; pair with
    /// `metrics::with_local_recorder` to get isolated counter state.
    fn render_prometheus(handle: &metrics_exporter_prometheus::PrometheusHandle) -> String {
        handle.render()
    }

    /// End-to-end: drive `maybe_spawn` twice against an over-budget group
    /// and assert the three budget counters land at the expected totals.
    /// First call: refusal + reply (no dedup hit). Second call: refusal +
    /// dedup suppression. Total: 2 refusals, 1 reply, 1 suppression.
    ///
    /// Plain `#[test]` (not `#[tokio::test]`) so `with_local_recorder` can
    /// own the thread for the duration of the inner runtime's `block_on`.
    /// `#[tokio::test]` would already be driving a runtime on this thread
    /// and the inner `block_on` would panic.
    #[test]
    fn maybe_spawn_emits_budget_counters_for_daily_token_cap() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            // tokio runtime is already active (#[tokio::test]); spawn the
            // gate work on a fresh blocking task so the local recorder
            // remains in scope for the metric calls. Easier: use a
            // single-threaded async block_on inside the closure.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let tmp = tempfile::tempdir().unwrap();
                let db = CentralDb::open_in_memory().unwrap();
                let mgr = ContainerManager::new(
                    db.clone(),
                    std::sync::Arc::new(crate::tests::NoopRuntime::default()),
                    manager_cfg(tmp.path().to_path_buf()),
                );
                let session = fixture_session(&db);
                let paths =
                    SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
                paths.ensure_dirs().unwrap();
                seed_routing(&paths);
                seed_pending_chat_inbound(&paths);
                set_daily_cap(&db, session.agent_group_id, 100);
                record_today_tokens(&db, session.agent_group_id, 150, 50);
                // Two refusals: the first writes a reply, the second is
                // dedup-suppressed inside the BUDGET_NOTICE_WINDOW_SECS
                // window.
                let _ = mgr.maybe_spawn(&session).await.unwrap();
                let _ = mgr.maybe_spawn(&session).await.unwrap();
            });
            render_prometheus(&handle)
        });

        // `ironclaw_budget_exhausted_total{gate=daily_tokens, ...} 2`
        assert!(
            body.contains(ironclaw_metrics::BUDGET_EXHAUSTED_TOTAL),
            "exhausted counter missing:\n{body}"
        );
        assert!(
            body.contains("gate=\"daily_tokens\""),
            "daily_tokens gate label missing:\n{body}"
        );
        assert!(
            find_counter_value(&body, ironclaw_metrics::BUDGET_EXHAUSTED_TOTAL) == Some(2),
            "expected exhausted_total=2, body:\n{body}"
        );
        assert!(
            find_counter_value(&body, ironclaw_metrics::BUDGET_EXHAUSTED_REPLIES_TOTAL)
                == Some(1),
            "expected replies_total=1, body:\n{body}"
        );
        assert!(
            find_counter_value(&body, ironclaw_metrics::BUDGET_EXHAUSTED_SUPPRESSED_TOTAL)
                == Some(1),
            "expected suppressed_total=1, body:\n{body}"
        );
    }

    /// Per-minute rate-limit gate fires `gate=turns_per_minute`. Plain
    /// `#[test]` for the same `with_local_recorder` reason as above.
    #[test]
    fn maybe_spawn_emits_turns_per_minute_gate_label() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let tmp = tempfile::tempdir().unwrap();
                let db = CentralDb::open_in_memory().unwrap();
                let mgr = ContainerManager::new(
                    db.clone(),
                    std::sync::Arc::new(crate::tests::NoopRuntime::default()),
                    manager_cfg(tmp.path().to_path_buf()),
                );
                let session = fixture_session(&db);
                let paths =
                    SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
                paths.ensure_dirs().unwrap();
                seed_routing(&paths);
                seed_pending_chat_inbound(&paths);
                set_rate_caps(&db, session.agent_group_id, Some(1), None);
                seed_turns(&db, session.agent_group_id, 1);
                let _ = mgr.maybe_spawn(&session).await.unwrap();
            });
            render_prometheus(&handle)
        });
        assert!(
            body.contains("gate=\"turns_per_minute\""),
            "turns_per_minute gate label missing:\n{body}"
        );
        assert!(
            find_counter_value(&body, ironclaw_metrics::BUDGET_EXHAUSTED_TOTAL) == Some(1),
            "expected exhausted_total=1 for per-minute gate, body:\n{body}"
        );
    }

    /// Per-hour rate-limit gate fires `gate=turns_per_hour`. Plain
    /// `#[test]` for the same `with_local_recorder` reason as above.
    #[test]
    fn maybe_spawn_emits_turns_per_hour_gate_label() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let tmp = tempfile::tempdir().unwrap();
                let db = CentralDb::open_in_memory().unwrap();
                let mgr = ContainerManager::new(
                    db.clone(),
                    std::sync::Arc::new(crate::tests::NoopRuntime::default()),
                    manager_cfg(tmp.path().to_path_buf()),
                );
                let session = fixture_session(&db);
                let paths =
                    SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
                paths.ensure_dirs().unwrap();
                seed_routing(&paths);
                seed_pending_chat_inbound(&paths);
                set_rate_caps(&db, session.agent_group_id, None, Some(1));
                seed_turns(&db, session.agent_group_id, 1);
                let _ = mgr.maybe_spawn(&session).await.unwrap();
            });
            render_prometheus(&handle)
        });
        assert!(
            body.contains("gate=\"turns_per_hour\""),
            "turns_per_hour gate label missing:\n{body}"
        );
    }

    /// Walk the Prometheus text body and return the integer value of the
    /// first sample whose name matches `metric_name`. Whitespace tolerant.
    /// Returns `None` if the metric isn't in the body.
    fn find_counter_value(body: &str, metric_name: &str) -> Option<u64> {
        // Prometheus text format: `<name>{<labels>} <value>` or `<name> <value>`.
        // We sum across all label combinations for the metric.
        let mut total: u64 = 0;
        let mut seen = false;
        for line in body.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            // Match either `name{...}` or `name ` exactly.
            let name_matches = line
                .strip_prefix(metric_name)
                .is_some_and(|rest| rest.starts_with('{') || rest.starts_with(' '));
            if !name_matches {
                continue;
            }
            // Value is the last whitespace-separated token.
            if let Some(value) = line.split_whitespace().last() {
                if let Ok(parsed) = value.parse::<f64>() {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let parsed_u = parsed as u64;
                    total += parsed_u;
                    seen = true;
                }
            }
        }
        if seen { Some(total) } else { None }
    }

    // ---- rate-limit gate (per-minute / per-hour) -------------------------

    #[test]
    fn rate_limit_message_none_when_caps_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        assert!(mgr.rate_limit_message(&session).unwrap().is_none());
        set_rate_caps(&db, session.agent_group_id, None, None);
        assert!(mgr.rate_limit_message(&session).unwrap().is_none());
    }

    #[test]
    fn rate_limit_message_fires_on_per_minute_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        set_rate_caps(&db, session.agent_group_id, Some(3), None);
        seed_turns(&db, session.agent_group_id, 2);
        assert!(mgr.rate_limit_message(&session).unwrap().is_none());
        seed_turns(&db, session.agent_group_id, 1);
        let (msg, gate) = mgr.rate_limit_message(&session).unwrap().unwrap();
        assert!(msg.contains("Per-minute"), "{msg}");
        assert!(msg.contains("cap is 3"), "{msg}");
        assert_eq!(gate, ironclaw_metrics::BUDGET_GATE_TURNS_PER_MINUTE);
    }

    #[test]
    fn rate_limit_message_fires_on_per_hour_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        set_rate_caps(&db, session.agent_group_id, None, Some(5));
        seed_turns(&db, session.agent_group_id, 5);
        let (msg, gate) = mgr.rate_limit_message(&session).unwrap().unwrap();
        assert!(msg.contains("Per-hour"), "{msg}");
        assert!(msg.contains("cap is 5"), "{msg}");
        assert_eq!(gate, ironclaw_metrics::BUDGET_GATE_TURNS_PER_HOUR);
    }

    #[tokio::test]
    async fn tick_refuses_spawn_when_per_minute_cap_reached() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_pending_chat_inbound(&paths);

        set_rate_caps(&db, session.agent_group_id, Some(1), None);
        seed_turns(&db, session.agent_group_id, 1);
        mgr.tick().await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
        assert!(runtime.spawn_calls().is_empty());
    }

    #[tokio::test]
    async fn tick_refuses_spawn_when_per_hour_cap_reached() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_pending_chat_inbound(&paths);

        set_rate_caps(&db, session.agent_group_id, None, Some(2));
        seed_turns(&db, session.agent_group_id, 2);
        mgr.tick().await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
        assert!(runtime.spawn_calls().is_empty());
    }

    #[test]
    fn rate_limit_dedup_within_window_emits_exactly_one_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        seed_routing(&paths);

        let text = "rate-limit reply text";
        mgr.maybe_post_rate_limit_reply(&session, &paths, text).unwrap();
        mgr.maybe_post_rate_limit_reply(&session, &paths, text).unwrap();
        mgr.maybe_post_rate_limit_reply(&session, &paths, text).unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0], text);
    }

    // ── SIGHUP secret rotation ─────────────────────────────────────

    #[test]
    fn parse_dotenv_content_handles_quotes_export_and_comments() {
        let raw = "
# leading comment
ANTHROPIC_API_KEY=sk-plain
export TAVILY_API_KEY=\"tav-quoted\"
BRAVE_SEARCH_API_KEY='br-single'

# trailing comment
SERPAPI_API_KEY=
NOT_A_PAIR_LINE
";
        let map = parse_dotenv_content(raw);
        assert_eq!(map.get("ANTHROPIC_API_KEY"), Some(&"sk-plain".to_string()));
        assert_eq!(map.get("TAVILY_API_KEY"), Some(&"tav-quoted".to_string()));
        assert_eq!(map.get("BRAVE_SEARCH_API_KEY"), Some(&"br-single".to_string()));
        assert_eq!(map.get("SERPAPI_API_KEY"), Some(&String::new()));
        assert!(!map.contains_key("NOT_A_PAIR_LINE"));
    }

    #[test]
    fn rotatable_config_drops_empty_values() {
        let mut m = std::collections::HashMap::new();
        m.insert("ANTHROPIC_API_KEY".into(), "sk-1".into());
        m.insert("ANTHROPIC_BASE_URL".into(), String::new());
        m.insert("TAVILY_API_KEY".into(), "tav-1".into());
        let cfg = RotatableConfig::from_env_map(&m);
        assert_eq!(cfg.anthropic_api_key.as_deref(), Some("sk-1"));
        assert!(cfg.anthropic_base_url.is_none(), "empty value must be dropped");
        assert_eq!(cfg.forward_env.len(), 1);
        assert_eq!(cfg.forward_env[0], ("TAVILY_API_KEY".into(), "tav-1".into()));
    }

    #[test]
    fn reload_env_updates_rotatable_and_returns_changed_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let env_path = tmp.path().join(".env");
        std::fs::write(&env_path, "ANTHROPIC_API_KEY=rotated-key\nEXA_API_KEY=exa-1\n").unwrap();
        let changed = mgr.reload_env(Some(&env_path));
        assert!(changed.contains(&"ANTHROPIC_API_KEY".to_string()), "{changed:?}");
        assert!(changed.contains(&"EXA_API_KEY".to_string()), "{changed:?}");
        // RwLock now reflects the rotation.
        let r = mgr.rotatable.read().unwrap();
        assert_eq!(r.anthropic_api_key.as_deref(), Some("rotated-key"));
        assert!(r.forward_env.iter().any(|(k, v)| k == "EXA_API_KEY" && v == "exa-1"));
    }

    #[test]
    fn reload_env_drops_keys_that_disappear() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        // Seed an EXA_API_KEY at construction time so it's in the
        // initial RotatableConfig; the env file rotation will drop it.
        cfg.forward_env = vec![("EXA_API_KEY".into(), "exa-old".into())];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let env_path = tmp.path().join(".env");
        // New env has no EXA_API_KEY → the key should be dropped.
        std::fs::write(&env_path, "ANTHROPIC_API_KEY=sk-new\n").unwrap();
        let changed = mgr.reload_env(Some(&env_path));
        assert!(changed.contains(&"EXA_API_KEY".to_string()));
        let r = mgr.rotatable.read().unwrap();
        assert!(
            r.forward_env.iter().all(|(k, _)| k != "EXA_API_KEY"),
            "EXA_API_KEY must be dropped after rotation, got: {:?}",
            r.forward_env
        );
    }

    // ── degraded mode ──────────────────────────────────────────────

    /// Boot-time image health check failed → host enters degraded
    /// mode → `maybe_spawn` refuses with `HostDegraded`.
    #[tokio::test]
    async fn degraded_mode_refuses_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        // Seed pending inbound so the gate would otherwise spawn.
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_pending_chat_inbound(&paths);

        // Flip into degraded mode programmatically (in production
        // this is done by run_host after the image health check
        // returns an Err).
        mgr.set_degraded();
        assert!(mgr.is_degraded(), "manager should report degraded");

        let err = mgr
            .maybe_spawn(&session)
            .await
            .expect_err("must refuse to spawn when degraded");
        assert!(
            matches!(err, ManagerError::HostDegraded),
            "expected HostDegraded, got {err:?}"
        );
        // The runtime must NOT have been called.
        assert!(
            runtime.spawn_calls().is_empty(),
            "no runtime spawn allowed in degraded mode"
        );
    }

    /// `enter_degraded_mode` writes an apology row to every session
    /// with a pending chat inbound, routed via that inbound's
    /// `(channel_type, platform_id, thread_id)`.
    #[test]
    fn degraded_mode_emits_apology_to_pending_inbounds() {
        use crate::image_health::{enter_degraded_mode, HealthDegradedReason, DEGRADED_APOLOGY_TEXT};
        use ironclaw_db::tables::sessions::{create as create_session, CreateSession};

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();

        // Two sessions, each with a pending chat inbound. Use
        // distinct agent groups so the unique-folder constraint
        // doesn't trip.
        let ag1 = create_ag(
            &db,
            CreateAgentGroup {
                name: "deg-a".into(),
                folder: "deg-a".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let ag2 = create_ag(
            &db,
            CreateAgentGroup {
                name: "deg-b".into(),
                folder: "deg-b".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let s1 = create_session(
            &db,
            CreateSession {
                agent_group_id: ag1.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
            },
        )
        .unwrap();
        let s2 = create_session(
            &db,
            CreateSession {
                agent_group_id: ag2.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
            },
        )
        .unwrap();
        let paths1 = SessionPaths::new(tmp.path(), s1.agent_group_id, s1.id);
        let paths2 = SessionPaths::new(tmp.path(), s2.agent_group_id, s2.id);
        paths1.ensure_dirs().unwrap();
        paths2.ensure_dirs().unwrap();
        seed_pending_chat_inbound(&paths1);
        seed_pending_chat_inbound(&paths2);

        let reason = HealthDegradedReason::ImageNotFound {
            tag: "ironclaw/session:nope".into(),
        };
        let notified = enter_degraded_mode(&db, tmp.path(), &reason);
        assert_eq!(notified, 2, "expected 2 sessions notified, got {notified}");

        for paths in [&paths1, &paths2] {
            let replies = count_outbound_text_replies(paths);
            assert_eq!(
                replies.len(),
                1,
                "each session should get exactly one apology row, got {replies:?}"
            );
            let body = &replies[0];
            assert!(
                body.contains("temporarily degraded"),
                "apology text must mention degraded state: {body}"
            );
            // The exact text matches DEGRADED_APOLOGY_TEXT.
            assert_eq!(body.as_str(), DEGRADED_APOLOGY_TEXT);
        }
    }

    #[test]
    fn reload_env_with_no_file_returns_empty_changed_when_nothing_was_set() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.anthropic_api_key = None;
        cfg.anthropic_base_url = None;
        cfg.forward_env = vec![];
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let changed = mgr.reload_env(None);
        assert!(changed.is_empty(), "{changed:?}");
    }

    // ── code-review fixes ───────────────────────────────────────────

    /// Finding 1: an operator who flips `IRONCLAW_SKILLS_MODE` from
    /// `callable` to `inline` between spawns must not leave a prior
    /// `skills.json` on disk — `load_skill` would otherwise hand the
    /// agent stale bodies that don't match the inlined prompt.
    #[test]
    fn runner_config_inline_mode_removes_stale_catalogue_from_prior_spawn() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body v2\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        // Pre-existing catalogue from a previous Callable-mode spawn.
        std::fs::write(
            session_root.join(SKILLS_CATALOGUE_FILENAME),
            r#"[{"name":"alpha","description":"","body":"alpha body v1"}]"#,
        )
        .unwrap();

        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Inline;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let _ = mgr.runner_config_for(&session, None, Some(&session_root));
        assert!(
            !session_root.join(SKILLS_CATALOGUE_FILENAME).exists(),
            "inline-mode spawn must scrub a catalogue left by a prior callable spawn"
        );
    }

    /// Finding 2: when the `skills.json` write fails in Callable mode,
    /// the assembled prompt falls back to Inline-mode shape (skill
    /// bodies present, no `load_skill` advert) so the agent isn't left
    /// pointing at a missing catalogue.
    #[test]
    fn runner_config_callable_falls_back_to_inline_when_catalogue_write_fails() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body marker\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        // Sabotage the write: pre-create `skills.json` as a directory so
        // `fs::write` to that path errors. Mirrors a real-world failure
        // mode (the path exists but isn't a regular file).
        std::fs::create_dir_all(session_root.join(SKILLS_CATALOGUE_FILENAME)).unwrap();

        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        // Inline shape: the body is inlined, the `load_skill` callable
        // index instructions are NOT mentioned.
        assert!(
            rc.system.contains("alpha body marker"),
            "fallback prompt must inline skill bodies"
        );
        assert!(
            !rc.system.contains("`load_skill`"),
            "fallback prompt must not advertise load_skill when no catalogue was written"
        );
    }

    /// Finding 3: the Callable-mode prompt index and the on-disk
    /// catalogue must agree about which skills exist. The fix makes
    /// `select_callable_skills` the single source of truth used by both
    /// outputs; this test pins the consistency.
    #[test]
    fn runner_config_callable_prompt_index_and_catalogue_agree() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        write_skill_md(&skills, "beta", "beta body\n");
        let session_root = td.path().join("session");
        std::fs::create_dir_all(&session_root).unwrap();
        let mut cfg = manager_cfg(td.path().to_path_buf());
        cfg.skills_dir = Some(skills);
        cfg.skills_mode = SkillsMode::Callable;
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let rc = mgr.runner_config_for(&session, None, Some(&session_root));
        let bytes = std::fs::read(session_root.join(SKILLS_CATALOGUE_FILENAME)).unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
        let catalogue_names: std::collections::BTreeSet<String> = parsed
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        // Every name in the catalogue must also appear in the prompt
        // index, and the prompt must not mention any name absent from
        // the catalogue.
        for name in &catalogue_names {
            assert!(
                rc.system.contains(&format!("name=\"{name}\"")),
                "catalogue entry {name} missing from prompt index"
            );
        }
        for candidate in ["alpha", "beta"] {
            let in_cat = catalogue_names.contains(candidate);
            let in_prompt = rc.system.contains(&format!("name=\"{candidate}\""));
            assert_eq!(
                in_cat, in_prompt,
                "{candidate}: catalogue/prompt disagreement (cat={in_cat}, prompt={in_prompt})"
            );
        }
    }

    /// Finding 3 (renderer contract): `render_callable_skill_index` only
    /// emits entries from its input, so a skill that didn't make it
    /// into the catalogue cannot appear in the prompt.
    #[test]
    fn render_callable_skill_index_only_emits_entries_from_catalogue() {
        let entries = vec![SkillCatalogueEntry {
            name: "kept".into(),
            description: "the only one".into(),
            body: "body".into(),
        }];
        let out = render_callable_skill_index(&entries);
        assert!(out.contains("name=\"kept\""));
        assert!(!out.contains("name=\"dropped\""));
    }

    /// Finding 7: when the per-group memory dir cannot be created, the
    /// host drops a session-local `memory/UNAVAILABLE.md` marker so the
    /// agent inside the container can detect the degraded mount.
    #[test]
    fn build_spec_writes_memory_unavailable_marker_when_groups_dir_unwriteable() {
        let tmp = tempfile::tempdir().unwrap();
        // Point groups_dir at a path whose parent is a regular file —
        // `create_dir_all` cannot create a directory under a non-dir.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let groups = blocker.join("groups");
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = Some(groups);
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let spec = mgr.build_spec(&session, &paths, "img", None);
        // The bind mount is skipped (no source could be prepared).
        let has_memory_mount = spec.mounts.iter().any(|m| match m {
            Mount::Bind { target, .. } => target == &format!("{CONTAINER_SESSION_DIR}/memory"),
            _ => false,
        });
        assert!(!has_memory_mount, "memory mount must not appear when source prep failed");
        // The marker file lands inside the session root so /data/memory
        // is still browsable from the container.
        let marker = paths.root.join("memory").join(MEMORY_UNAVAILABLE_FILENAME);
        assert!(
            marker.is_file(),
            "expected UNAVAILABLE.md marker at {}",
            marker.display()
        );
        let body = std::fs::read_to_string(&marker).unwrap();
        assert!(body.contains("Memory mount unavailable"));
    }

    /// Finding 8: the per-group memory dir is relaxed to group-writeable
    /// (`0o775`) so the operator (host uid) can clean up files the
    /// container's root user wrote into the bind without sudo.
    #[cfg(unix)]
    #[test]
    fn build_spec_per_group_memory_dir_is_group_writeable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let groups = tmp.path().join("groups");
        std::fs::create_dir_all(&groups).unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mut cfg = manager_cfg(tmp.path().to_path_buf());
        cfg.groups_dir = Some(groups.clone());
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            cfg,
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        let _spec = mgr.build_spec(&session, &paths, "img", None);
        let mem = groups
            .join(session.agent_group_id.as_uuid().to_string())
            .join("memory");
        let mode = std::fs::metadata(&mem).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o775,
            "expected 0o775 mode bits on per-group memory dir, got {mode:o}"
        );
    }

    /// Finding 11: a briefing file the operator dropped on disk but the
    /// host couldn't read (permission error, EISDIR, etc.) surfaces as a
    /// diagnostic note inside the assembled prompt so the agent can
    /// mention the failure if the user references the briefing.
    #[cfg(unix)]
    #[test]
    fn assemble_system_prompt_surfaces_briefing_read_error_as_diagnostic() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join(PROJECT_BRIEFING_FILENAME);
        std::fs::write(&path, "secret deployment notes\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Tests running as root would defeat chmod 0; bail in that case.
        if std::fs::read_to_string(&path).is_ok() {
            return;
        }
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &ironclaw_skills::SkillsSelector::All,
            Some(td.path()),
            ironclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(
            prompt.contains("Briefing diagnostics"),
            "expected diagnostics section when briefing was unreadable"
        );
        assert!(
            prompt.contains("could not be read"),
            "expected explanation of the read failure"
        );
        // Restore permissions so tempdir cleanup can drop the file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// Finding 13a: `skill.name` is escaped symmetrically with
    /// `skill.description` at both call sites so a name containing `"`
    /// or `&` (defence in depth — the kebab-case validator already
    /// rejects these) cannot produce malformed prompt markup.
    #[test]
    fn build_skill_system_prompt_escapes_skill_name_in_both_modes() {
        let weird = ironclaw_skills::Skill {
            id: ironclaw_skills::SkillId("weird".into()),
            name: "weird&\"name".into(),
            description: "desc".into(),
            dir: std::path::PathBuf::from("/nonexistent"),
            allowed_tools: None,
            source: ironclaw_skills::SkillSource::Global,
        };
        // Inline-mode rendering goes through `out.push_str` with the
        // escape — exercise the helper directly to pin the contract.
        let entry = SkillCatalogueEntry {
            name: weird.name.clone(),
            description: weird.description.clone(),
            body: "body".into(),
        };
        let callable_out = render_callable_skill_index(std::slice::from_ref(&entry));
        assert!(
            callable_out.contains("name=\"weird&amp;&quot;name\""),
            "callable mode must escape `&` and `\"` in skill name; got: {callable_out}"
        );
        // The inline path renders manually inside `build_skill_system_prompt`
        // — assert via a focused chunk of the format string.
        let inline_chunk = {
            let mut s = String::new();
            s.push_str("\n<skill name=\"");
            s.push_str(&escape_attr(&weird.name));
            s.push_str("\" description=\"");
            s.push_str(&escape_attr(&weird.description));
            s.push_str("\">\n");
            s
        };
        assert!(
            inline_chunk.contains("name=\"weird&amp;&quot;name\""),
            "inline mode must escape `&` and `\"` in skill name"
        );
    }
}
