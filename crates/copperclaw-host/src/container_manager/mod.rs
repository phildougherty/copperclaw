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
//!      file on boot (its `COPPERCLAW_RUNNER_CONFIG` env var points at
//!      it).
//!    - Build a `ContainerSpec` that bind-mounts the session dir into
//!      `/data`, propagates `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL`,
//!      sets labels for orphan cleanup, and exec's
//!      `/usr/local/bin/copperclaw-runner --config /data/runner.json`.
//!    - Call `runtime.spawn(spec)` and persist
//!      `sessions.container_status = running`.
//!
//! Crash detection and idle-stop are explicit out-of-scope for this
//! slice — they belong in a follow-up that needs richer state tracking
//! than the table currently exposes. The runner writes a heartbeat
//! file under the session dir so a future sweep can read it.

pub mod budgets;
pub mod classify;
pub mod config;
pub mod egress;
pub mod mount_guard;
pub mod prompt;
pub mod runner_config;
pub mod spawn;
pub mod tasks_snapshot;

pub use config::{ManagerConfig, ROTATABLE_ENV_KEYS, RotatableConfig, SkillsMode};
pub use egress::{
    DNSMASQ_CONF_FILENAME, DnsFilterPlan, RESOLV_CONF_FILENAME, build_dns_filter_plan,
    filter_upstreams, model_base_url_for_provider, model_endpoint_entry, nft_table_name,
    parse_egress_mode, parse_resolv_conf_upstreams, resolve_allow_list,
    resolve_allow_list_for_provider,
};
pub use prompt::{
    BASE_PREAMBLE, MEMORY_UNAVAILABLE_FILENAME, PROJECT_BRIEFING_FILENAME,
    SKILLS_CATALOGUE_FILENAME,
};
pub use spawn::{
    CODING_SKILL_NAMES, CONTAINER_RUNNER_PATH, CONTAINER_SESSION_DIR, DEFAULT_HEARTBEAT_STALE_SECS,
    DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS, POLL_INTERVAL_MS, RUNNER_CONFIG_FILENAME,
    RebuildBackoff, resolve_rebuild_base,
};
pub use tasks_snapshot::TASKS_SNAPSHOT_FILENAME;

pub use classify::ReconcileAction;

use self::config::read_env_file;
use copperclaw_container_rt::{ContainerRuntime, RtError};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::sessions;
use copperclaw_host_sweep::SpawnAttemptTracker;
use copperclaw_modules::{MountHostContext, MountSecurityModule};
use copperclaw_types::{AgentGroupId, SessionStatus};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Errors raised by the manager's poll loop.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// DB read/write failure.
    #[error("db: {0}")]
    Db(#[from] copperclaw_db::DbError),
    /// JSON serialization failed building the runner config.
    #[error("json: {0}")]
    Json(serde_json::Error),
    /// Local-FS failure writing the runner config or ensuring dirs.
    #[error("io: {0}")]
    Io(std::io::Error),
    /// Container runtime spawn failed.
    #[error("spawn: {0}")]
    Spawn(#[source] RtError),
    /// A host-controlled bind-mount source failed validation at spawn
    /// time — e.g. a path component became a symlink between the host
    /// computing the path and the mount (a TOCTOU swap), or the source
    /// escaped its session root. The spawn is refused rather than handing
    /// dockerd a source that resolves outside the agent's data dir.
    #[error("unsafe mount source: {0}")]
    UnsafeMount(#[source] copperclaw_modules::MountError),
    /// The host entered degraded mode at boot (e.g. session image is
    /// missing or stale). Sessions cannot be spawned until the
    /// operator runs `./rebuild.sh` and restarts the host.
    #[error(
        "host degraded; refusing to spawn sessions until the operator restarts after `./rebuild.sh`"
    )]
    HostDegraded,
}

/// Manager service. Cheap to clone via `Arc`.
pub struct ContainerManager {
    pub(crate) central: CentralDb,
    pub(crate) runtime: Arc<dyn ContainerRuntime>,
    pub(crate) cfg: ManagerConfig,
    /// Per-agent-group timestamps of the last in-channel "budget
    /// exhausted" notice we emitted. Used to dedup so a user who
    /// sends ten messages while over the cap gets one explanation,
    /// not ten. Process-local — a host restart re-notifies once,
    /// which is acceptable.
    pub(crate) last_budget_notice:
        std::sync::Mutex<std::collections::HashMap<AgentGroupId, chrono::DateTime<chrono::Utc>>>,
    /// Same shape as `last_budget_notice` but for per-minute /
    /// per-hour LLM rate-limit notifications. Keyed by
    /// `AgentGroupId`; value is the UTC time of the last
    /// notification sent (minute OR hour cap, whichever fired).
    pub(crate) rate_limit_notified:
        std::sync::Mutex<std::collections::HashMap<AgentGroupId, chrono::DateTime<chrono::Utc>>>,
    /// Hot-swappable subset of the config (provider API keys + base
    /// URL + forwarded provider keys). Initialized from `cfg` at
    /// construction; updated by [`Self::reload_env`] on SIGHUP. Reads
    /// during `build_spec` / `runner_config_for` take a short-lived
    /// read-lock so the spawn path stays fast.
    pub(crate) rotatable: Arc<RwLock<RotatableConfig>>,
    /// Per-session counter of consecutive failed `runtime.spawn`
    /// calls. Shared with the host's sweep service so its apology
    /// check can detect "container never came up" and emit a
    /// user-visible note. A successful spawn resets the counter.
    /// Defaults to an empty tracker so test code that calls
    /// [`Self::new`] without wiring sweep still works.
    pub(crate) spawn_tracker: Arc<SpawnAttemptTracker>,
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
    pub(crate) rebuild_backoff: Arc<RebuildBackoff>,
    /// Set by [`Self::set_degraded`] when the boot-time image health
    /// check fails. When `true`, every call to [`Self::maybe_spawn`]
    /// short-circuits with [`ManagerError::HostDegraded`] — the host
    /// keeps running (so `cclaw doctor` still works) but no new
    /// containers are launched until the operator runs
    /// `./rebuild.sh` to refresh the session image and restart.
    /// Stored as an `AtomicBool` so the read-side on the spawn hot
    /// path is lock-free.
    pub(crate) degraded: AtomicBool,
    /// Built with a LIVE host root (`<data_dir>/sessions`) — the dir all
    /// per-session bind sources live under. Two roles: (1) it is the
    /// enumerable [`MountSecurityModule`] the host registers (with the same
    /// live root) so `cclaw modules list` reflects a real root rather than
    /// the `host: None` placeholder it shipped with; (2) it is the canonical
    /// source of [`Self::sessions_root`], which the spawn-time mount guard
    /// ([`mount_guard::validate_source`]) validates each host-controlled
    /// bind source against immediately before mounting (toctou redux). The
    /// guard canonicalizes the source and refuses it if a component was
    /// swapped for a symlink that escapes the sessions root. Residual:
    /// dockerd re-resolves the source path in its own process when it
    /// performs the bind, so this closes the host-side TOCTOU window but
    /// cannot eliminate a swap that races dockerd's own resolution.
    pub(crate) mount_security: MountSecurityModule,
}

impl ContainerManager {
    /// Build a new manager.
    #[must_use]
    pub fn new(central: CentralDb, runtime: Arc<dyn ContainerRuntime>, cfg: ManagerConfig) -> Self {
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
            // LIVE host root: the sessions dir all per-session bind sources
            // (session root, parent worktree, shared `.git`) live under.
            mount_security: MountSecurityModule::with_host(MountHostContext {
                session_root: cfg.data_dir.join("sessions"),
            }),
            cfg,
        }
    }

    /// Wire a shared spawn-attempt tracker (typically owned by
    /// [`copperclaw_host_sweep::SweepService`]) into the manager.
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

    /// The live sessions root all per-session bind sources live under
    /// (`<data_dir>/sessions`). Taken from the registered
    /// [`MountSecurityModule`]'s host context so the spawn-time mount guard
    /// and the enumerable module never drift. Falls back to recomputing from
    /// `cfg.data_dir` if the module was somehow registered without a host.
    pub(crate) fn sessions_root(&self) -> &Path {
        self.mount_security
            .host()
            .map_or(self.cfg.data_dir.as_path(), |h| h.session_root.as_path())
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
    /// `copperclaw_secrets_rotated_total` metric counter.
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
        copperclaw_metrics::inc_secrets_rotated();
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
            // Refresh the tasks snapshot for any session whose container
            // is up so a long-running agent calling `list_tasks` sees the
            // current state of the scheduler. Cheap (a single SELECT +
            // ~1 KB JSON write per running session per tick); only fires
            // on Running because Stopped/Idle sessions don't have a
            // runner that could observe the file.
            if matches!(
                session.container_status,
                copperclaw_types::ContainerStatus::Running
            ) {
                let paths = copperclaw_db::session::SessionPaths::new(
                    &self.cfg.data_dir,
                    session.agent_group_id,
                    session.id,
                );
                tasks_snapshot::write_tasks_snapshot(&self.central, session.id, &paths.root);
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
}
