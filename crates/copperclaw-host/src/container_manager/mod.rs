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

pub mod broker;
pub mod broker_server;
pub mod budgets;
pub mod classify;
pub mod cold_start;
pub mod config;
pub mod crash_loop;
pub mod egress;
pub mod host_resources;
pub mod mcp_tools;
pub mod mount_guard;
pub mod prompt;
pub mod provider_failover;
pub mod runner_config;
pub mod spawn;
pub mod stuck_actuator;
pub mod tasks_snapshot;

pub use broker::{
    AuthScheme, AuthzDecision, BrokerConfig, BrokerKeyring, BrokerState, BudgetVerdict,
    DEFAULT_TOKEN_TTL_SECS, Revocations, TokenClaims, TokenError, UpstreamRequest,
    auth_scheme_for_provider, authorize,
};
pub use config::{ManagerConfig, ROTATABLE_ENV_KEYS, RotatableConfig, SkillsMode};
pub use egress::{
    DNSMASQ_CONF_FILENAME, DnsFilterPlan, RESOLV_CONF_FILENAME, build_dns_filter_plan,
    filter_upstreams, model_base_url_for_provider, model_endpoint_entry, nft_table_name,
    parse_egress_mode, parse_resolv_conf_upstreams, resolve_allow_list,
    resolve_allow_list_for_provider,
};
pub use prompt::{
    BASE_PREAMBLE, CODING_PREAMBLE, MEMORY_UNAVAILABLE_FILENAME, PROJECT_BRIEFING_FILENAME,
    SKILLS_CATALOGUE_FILENAME,
};
pub use spawn::{
    CODING_SKILL_NAMES, CONTAINER_RUNNER_PATH, CONTAINER_SESSION_DIR, DEFAULT_HEARTBEAT_STALE_SECS,
    DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS, POLL_INTERVAL_MS, RUNNER_CONFIG_FILENAME,
    RebuildBackoff, SpawnFailureReason, classify_spawn_failure, resolve_rebuild_base,
};
pub use tasks_snapshot::TASKS_SNAPSHOT_FILENAME;

pub use classify::ReconcileAction;
pub use cold_start::{SLOW_SPAWN_NOTICE_AFTER, SLOW_SPAWN_NOTICE_TEXT, SpawnActivity};
pub use crash_loop::{CrashCause, CrashLoopTracker, OOM_CARD_THRESHOLD};
pub use host_resources::{
    DiskProbe, DiskSpace, ENV_FAULT_SESSION_THRESHOLD, ENV_FAULT_WINDOW, EnvFaultBreaker,
};

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
    /// The *environment* cannot support a container right now — the box
    /// is out of disk, or enough distinct sessions crash-restarted in one
    /// window that the fault is clearly shared rather than per-session
    /// (see [`host_resources`]).
    ///
    /// Distinct from [`Self::HostDegraded`] in one important way: this is
    /// self-clearing. `HostDegraded` is a boot-time verdict that needs an
    /// operator to run `./rebuild.sh` and restart; this one clears itself
    /// the moment the disk probe comes back clean. Both are collapsed to
    /// `Ok` by the reconcile loop's `Spawn` arm — the session stays
    /// `Stopped` with its inbound pending, which is precisely the point:
    /// no doomed container spawn, no lost message.
    #[error("host resources exhausted; refusing to spawn sessions until the environment recovers")]
    HostResourceExhausted,
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
    /// Credential broker (Phase 0b). `None` (the default) keeps the legacy
    /// spawn path: the real `ANTHROPIC_API_KEY` is forwarded into the
    /// container env. When `Some`, the broker is enabled: `build_spec` stops
    /// forwarding the master key and instead mints a per-session capability
    /// token (in the `ANTHROPIC_API_KEY` slot) and points `ANTHROPIC_BASE_URL`
    /// at [`Self::broker_base_url`]. Set via [`Self::with_broker`] at boot
    /// when `COPPERCLAW_CREDENTIAL_BROKER` is enabled and a real key exists.
    pub(crate) broker: Option<Arc<broker::BrokerState>>,
    /// The loopback base URL the container reaches the broker at (e.g.
    /// `http://127.0.0.1:NNNNN`). Only meaningful when [`Self::broker`] is
    /// `Some`. The container's reachability of this loopback address is the
    /// deployment's responsibility (host networking / docker bridge); see the
    /// module docs.
    pub(crate) broker_base_url: Option<String>,
    /// M17 session-preview manager. When wired (via [`Self::with_preview`]),
    /// the idle-stop and crash-restart paths tear down every preview the
    /// session had open — a stopped/removed container's bridge IP is dead (or
    /// worse, may be reassigned), so its proxies must not outlive it. `None`
    /// (the default, and every test constructor) is a no-op.
    pub(crate) preview: Option<Arc<crate::preview::PreviewManager>>,
    /// M21 F1 cold-start feedback (decision (c)): registry of spawn
    /// attempts currently in flight, shared with the typing ticker so
    /// mid-spawn sessions with pending inbound pulse the typing
    /// indicator from message one; also carries the per-episode
    /// slow-spawn-notice dedup. In-memory only — a host restart resets
    /// it, same posture as `crash_loop`.
    pub(crate) spawn_activity: Arc<cold_start::SpawnActivity>,
    /// M21 S4 crash-loop bookkeeping (decision (e)): per-session
    /// exponential respawn backoff for crash restarts plus per-episode
    /// OOM counting/dedup for the single "keeps running out of memory"
    /// `ErrorCard`. In-memory only — a host restart resets it (boot's
    /// recovery path re-baselines everything anyway).
    pub(crate) crash_loop: crash_loop::CrashLoopTracker,
    /// Event-driven wake accelerator. When wired (via
    /// [`Self::with_wake_notify`], from the router's
    /// `inbound_wake` handle), [`Self::run_loop`] awaits it alongside the
    /// poll timer and runs an immediate [`Self::tick`] on signal — so a
    /// message to an idle/stopped session spawns a container within ~one
    /// tick instead of waiting out the poll interval. `Notify` coalesces
    /// bursts into a single stored permit, and `classify()` stays the single
    /// decision point, so there are no spawn storms. `None` (the default)
    /// keeps the pure polling loop — the poll cadence remains the crash-safe
    /// fallback either way.
    pub(crate) wake: Option<Arc<tokio::sync::Notify>>,
    /// M21 O4 (decision (d)): opt-in operator-alert enqueuer. `None` (the
    /// default, and every test constructor that doesn't wire it) means the
    /// crash-loop/OOM and spawn-failure-streak call sites fire nothing beyond
    /// their existing log + metric — zero behavior change. When wired (via
    /// [`Self::with_operator_alerts`], from `boot.rs`) AND a destination is
    /// configured, those thresholds enqueue one deduped, rate-limited alert
    /// row through the existing delivery pipeline.
    pub(crate) operator_alerts: Option<Arc<crate::operator_alerts::OperatorAlerts>>,
    /// Cached free-space reading for `cfg.data_dir`, refreshed once per
    /// reconcile tick by [`Self::refresh_host_resources`] and read by the
    /// spawn preflight. Cached deliberately: probing per session per pass
    /// would add `statvfs` traffic to a box that is already sick.
    pub(crate) disk_probe: host_resources::DiskProbe,
    /// Global circuit breaker for whole-box faults — trips when
    /// [`ENV_FAULT_SESSION_THRESHOLD`] distinct sessions crash-restart
    /// inside [`ENV_FAULT_WINDOW`]. See [`host_resources`] for why
    /// per-session backoff cannot substitute for this.
    pub(crate) env_breaker: host_resources::EnvFaultBreaker,
    /// Whether the environment is currently considered unable to host a
    /// container (disk below the FAIL threshold, or the breaker open).
    /// Separate from [`Self::degraded`] because this one self-clears:
    /// `degraded` is a sticky boot verdict, this is a live condition.
    /// `AtomicBool` so the spawn hot path reads it lock-free.
    pub(crate) env_exhausted: AtomicBool,
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
            broker: None,
            broker_base_url: None,
            preview: None,
            spawn_activity: Arc::new(cold_start::SpawnActivity::new()),
            crash_loop: crash_loop::CrashLoopTracker::new(),
            wake: None,
            operator_alerts: None,
            disk_probe: host_resources::DiskProbe::new(),
            env_breaker: host_resources::EnvFaultBreaker::new(),
            env_exhausted: AtomicBool::new(false),
            cfg,
        }
    }

    /// Wire the opt-in operator-alert enqueuer (M21 O4) into the manager. The
    /// boot sequence hands the same `Arc<OperatorAlerts>` it registers the
    /// supervisor degraded-watch loop with, so the crash-loop/OOM and
    /// spawn-failure-streak thresholds can push a deduped, rate-limited alert
    /// to the configured operator destination. Mutates `self` so boot can
    /// attach it after building the manager. `None` (the default) is a no-op:
    /// the thresholds keep their pre-O4 log + metric behaviour.
    #[must_use]
    pub fn with_operator_alerts(
        mut self,
        alerts: Arc<crate::operator_alerts::OperatorAlerts>,
    ) -> Self {
        self.operator_alerts = Some(alerts);
        self
    }

    /// Wire the M17 session-preview manager so container stop / removal tears
    /// down the session's previews. Mutates `self` so the boot sequence can
    /// attach it after building the manager.
    #[must_use]
    pub fn with_preview(mut self, preview: Arc<crate::preview::PreviewManager>) -> Self {
        self.preview = Some(preview);
        self
    }

    /// Wire an event-driven wake signal into the manager. The host passes
    /// the router's `inbound_wake` handle so a `messages_in` insert wakes
    /// the reconcile loop immediately (see the field docs on [`Self::wake`]
    /// for the coalescing / fallback contract). Mutates `self` so the boot
    /// sequence can attach it after building the manager.
    #[must_use]
    pub fn with_wake_notify(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Wire an enabled credential broker into the manager. Stores the broker
    /// state (used to mint per-session tokens at spawn) and the loopback base
    /// URL the container reaches it at. Mutates `self` so the boot sequence
    /// can attach it after building the manager. With this set, the spawn path
    /// stops forwarding the real provider key — see [`spawn::ContainerManager`]
    /// `build_spec`.
    #[must_use]
    pub fn with_broker(mut self, broker: Arc<broker::BrokerState>, base_url: String) -> Self {
        self.broker = Some(broker);
        self.broker_base_url = Some(base_url);
        self
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

    /// Wire a shared cold-start spawn-activity registry (M21 F1,
    /// decision (c)) into the manager. The boot sequence hands the
    /// same handle to the typing ticker so mid-spawn sessions pulse
    /// the typing indicator. Mutates `self` so boot can attach it
    /// after building the manager.
    #[must_use]
    pub fn with_spawn_activity(mut self, activity: Arc<cold_start::SpawnActivity>) -> Self {
        self.spawn_activity = activity;
        self
    }

    /// Access the shared cold-start spawn-activity registry.
    pub fn spawn_activity(&self) -> &Arc<cold_start::SpawnActivity> {
        &self.spawn_activity
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

    /// Whether the *environment* is currently unable to host a container.
    /// Unlike [`Self::is_degraded`] this is a live condition, not a boot
    /// verdict — it clears on its own as soon as the disk recovers.
    #[must_use]
    pub fn is_env_exhausted(&self) -> bool {
        self.env_exhausted.load(Ordering::SeqCst)
    }

    /// The current (cached) free-space reading for the data dir, or
    /// `None` when the filesystem could not be stat'd. `None` is
    /// *unknown*, never *full*.
    #[must_use]
    pub fn current_disk(&self) -> Option<host_resources::DiskSpace> {
        self.disk_probe
            .get(&self.cfg.data_dir, tokio::time::Instant::now())
    }

    /// Re-`statvfs` the data dir and reconcile the environment-fault
    /// state. Called once at the top of every [`Self::tick`] — once per
    /// *tick*, not once per session, which is what keeps the preflight
    /// from adding N `statvfs` calls per pass to a sick box.
    ///
    /// Transitions are edge-logged so a full disk produces one WARN and
    /// one recovery INFO, not one line per tick per session (the 2026-07-18
    /// incident's log volume was itself part of the problem).
    pub(crate) fn refresh_host_resources(&self) {
        let now = tokio::time::Instant::now();
        let reading = self.disk_probe.refresh(&self.cfg.data_dir, now);
        // Unknown free space is not evidence of exhaustion: keep whatever
        // the breaker already decided rather than inventing a fault.
        let disk_full = reading.is_some_and(host_resources::DiskSpace::is_exhausted);
        // A clean disk is the signal that lets the breaker re-arm — but
        // only after it has held for one full window, so a non-disk fault
        // (sick daemon, wedged mount) still gets a real pause instead of
        // being waved through on the very next tick.
        if !disk_full {
            self.env_breaker.maybe_rearm(now);
        }
        let exhausted = disk_full || self.env_breaker.is_tripped();
        let was = self.env_exhausted.swap(exhausted, Ordering::SeqCst);
        if exhausted && !was {
            warn!(
                disk = reading
                    .map_or_else(|| "unknown".to_string(), host_resources::DiskSpace::summary),
                breaker_tripped = self.env_breaker.is_tripped(),
                "host resources exhausted; refusing container spawns until the environment recovers"
            );
        } else if !exhausted && was {
            // Recovered. The deferred sessions spawn on this very pass —
            // their inbound rows stayed pending throughout, so nothing was
            // lost to the outage.
            tracing::info!(
                disk = reading
                    .map_or_else(|| "unknown".to_string(), host_resources::DiskSpace::summary),
                "host resources recovered; resuming container spawns"
            );
        }
    }

    /// Clear the environment-fault state by operator request (the
    /// self-clearing path is [`Self::refresh_host_resources`] noticing the
    /// disk recovered). Re-arms the breaker; the next tick re-evaluates
    /// the disk, so this cannot paper over a filesystem that is still full.
    pub fn clear_env_exhausted(&self) {
        self.env_breaker.reset();
        self.env_exhausted.store(false, Ordering::SeqCst);
    }

    /// Record one session's crash-restart against the global breaker. On
    /// the trip edge — and ONLY on the trip edge — flip the manager into
    /// environment-degraded mode and fire exactly one operator alert.
    ///
    /// This is the piece that actually caps the storm. Before it, N
    /// sessions sharing one broken disk produced N independent per-session
    /// backoff curves that all bottomed out at the 300s cap and retried
    /// forever; the remedy could not work because the fault was not
    /// per-session.
    pub(crate) fn note_crash_for_env_breaker(&self, session: &copperclaw_types::Session) {
        if !self
            .env_breaker
            .record_crash(session.id, tokio::time::Instant::now())
        {
            return;
        }
        self.env_exhausted.store(true, Ordering::SeqCst);
        let disk = self
            .current_disk()
            .map_or_else(|| "unknown".to_string(), host_resources::DiskSpace::summary);
        warn!(
            distinct_sessions = ENV_FAULT_SESSION_THRESHOLD,
            window_secs = ENV_FAULT_WINDOW.as_secs(),
            disk = disk,
            "environment fault: multiple distinct sessions crash-restarted in one window; \
             respawns paused globally (this is NOT a per-session bug)"
        );
        let Some(alerts) = self.operator_alerts.as_ref() else {
            return;
        };
        // One dedup key for the whole fault (not per session): the entire
        // point is that the operator gets ONE page, not one per session.
        alerts.fire(
            crate::operator_alerts::AlertSeverity::Critical,
            "env_fault",
            &format!(
                "{ENV_FAULT_SESSION_THRESHOLD}+ distinct agent sessions crash-restarted within \
                 {}s — this looks like a host-wide fault, not a per-session bug. Container \
                 spawns are paused until it clears. Disk: {disk}. {}",
                ENV_FAULT_WINDOW.as_secs(),
                copperclaw_cclaw::disk::DISK_FIX,
            ),
        );
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

    /// M21 O4: fire one operator alert when a session's spawn-failure streak
    /// crosses [`SpawnAttemptTracker`]'s threshold. Fired on the exact
    /// crossing (`attempts == SPAWN_FAIL_THRESHOLD`) so it is once-per-streak:
    /// a successful spawn clears the tracker, so the next streak re-crosses and
    /// re-arms. `OperatorAlerts::fire`'s own dedup is belt-and-suspenders. A
    /// no-op when alerts are unwired or their destination is unconfigured.
    pub(crate) fn alert_spawn_failure(
        &self,
        session: &copperclaw_types::Session,
        attempts: u32,
        reason: spawn::SpawnFailureReason,
    ) {
        if attempts != copperclaw_host_sweep::SPAWN_FAIL_THRESHOLD {
            return;
        }
        let Some(alerts) = self.operator_alerts.as_ref() else {
            return;
        };
        alerts.fire(
            crate::operator_alerts::AlertSeverity::Warning,
            &format!("spawn_fail:{}", session.id.as_uuid()),
            &format!(
                "A session container has failed to spawn {attempts} times in a row \
                 (reason: {}); the session cannot come up. Agent group {}. \
                 See `cclaw doctor`.",
                reason.as_str(),
                session.agent_group_id.as_uuid()
            ),
        );
    }

    /// Poll loop. Returns when `shutdown` is cancelled.
    ///
    /// When a wake signal is wired ([`Self::with_wake_notify`]), the loop
    /// also runs an immediate global [`Self::tick`] whenever the router
    /// signals a fresh `messages_in` insert. The timer arm keeps firing
    /// regardless — polling is the crash-safe fallback, the notify is only
    /// an accelerator. `Notify` stores at most one permit, so a burst of
    /// inserts arriving while a tick is in flight coalesces into exactly one
    /// follow-up tick; `classify()` remains the single decision point, so an
    /// extra tick against an already-reconciled session is a no-op.
    pub async fn run_loop(self: Arc<Self>, shutdown: CancellationToken) {
        let interval = Duration::from_millis(POLL_INTERVAL_MS);
        // When no wake signal is wired, park the wake arm on a private
        // Notify nobody ever signals — the select then degenerates to the
        // original timer-only loop.
        let wake = self
            .wake
            .clone()
            .unwrap_or_else(|| Arc::new(tokio::sync::Notify::new()));
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(interval) => {
                    if let Err(err) = self.tick().await {
                        warn!(?err, "container_manager tick failed");
                    }
                }
                () = wake.notified() => {
                    if let Err(err) = self.tick().await {
                        warn!(?err, "container_manager wake tick failed");
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
        // One `statvfs` per tick (NOT per session) — the spawn preflight
        // below reads the cached result. This also drives the
        // self-clearing half of the environment-fault state: when the disk
        // recovers, the breaker re-arms here and the deferred sessions
        // spawn on this very pass.
        self.refresh_host_resources();
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

#[cfg(test)]
mod wake_tests {
    //! Event-driven wake (M17 C3): the run loop's `Notify` arm and its
    //! coalescing behaviour. Spawn-path details are covered in
    //! `spawn.rs` / `classify.rs`; here we only assert the wiring between
    //! the wake handle and `tick()`.

    use super::config::{ManagerConfig, SkillsMode};
    use super::spawn::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use copperclaw_db::session::{SessionPaths, open_inbound};
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messages_in;
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use copperclaw_types::{ContainerStatus, Session};
    use std::path::PathBuf;
    use tokio::sync::Notify;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: None,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: SkillsMode::Inline,
            gpu_passthrough: false,
            forward_env: Vec::new(),
            egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
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
                source_session_id: None,
            },
        )
        .unwrap()
    }

    fn seed_pending_inbound(data_dir: &Path, session: &Session) {
        let paths = SessionPaths::new(data_dir, session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
    }

    /// A burst of wake signals against a stopped session with pending
    /// inbound produces exactly ONE spawn, and it lands well before the
    /// poll timer would have fired — the wake is an accelerator, the
    /// coalescing is the no-spawn-storm guarantee.
    #[tokio::test]
    async fn wake_burst_spawns_once_before_poll_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = Arc::new(crate::tests::NoopRuntime::default());
        let wake = Arc::new(Notify::new());
        let mgr = Arc::new(
            ContainerManager::new(db.clone(), runtime.clone(), manager_cfg(tmp.path().into()))
                .with_wake_notify(Arc::clone(&wake)),
        );
        let session = fixture_session(&db);
        seed_pending_inbound(tmp.path(), &session);

        let shutdown = CancellationToken::new();
        let task = tokio::spawn(Arc::clone(&mgr).run_loop(shutdown.clone()));
        // Let the loop park on its select before signalling.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let start = std::time::Instant::now();
        for _ in 0..25 {
            wake.notify_one();
        }

        // Poll for the spawn; the timer arm can't have fired before
        // POLL_INTERVAL_MS (1000ms), so a spawn observed inside this
        // 800ms budget was wake-driven.
        let budget = Duration::from_millis(800);
        loop {
            if !runtime.spawn_calls().is_empty() {
                break;
            }
            assert!(
                start.elapsed() < budget,
                "wake did not trigger a spawn within {budget:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Give any (incorrectly) queued extra wake ticks a chance to run,
        // then assert the burst coalesced into a single spawn.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            runtime.spawn_calls().len(),
            1,
            "burst of 25 wake signals must coalesce into one spawn"
        );

        shutdown.cancel();
        let _ = task.await;
    }

    /// `WakeFromIdle` is a two-tick transition (idle → stopped, then
    /// spawn). With the wake wired, the first tick chains the second by
    /// self-signalling instead of waiting out another poll interval.
    #[tokio::test]
    async fn wake_from_idle_tick_self_signals_for_the_spawn_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = Arc::new(crate::tests::NoopRuntime::default());
        let wake = Arc::new(Notify::new());
        let mgr = ContainerManager::new(db.clone(), runtime, manager_cfg(tmp.path().into()))
            .with_wake_notify(Arc::clone(&wake));
        let session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        seed_pending_inbound(tmp.path(), &session);

        mgr.tick().await.unwrap();

        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
        // The idle → stopped transition stored a permit so the next loop
        // iteration spawns immediately.
        tokio::time::timeout(Duration::from_secs(1), wake.notified())
            .await
            .expect("WakeFromIdle must self-signal the follow-up tick");
    }

    /// Without a wired wake handle the manager behaves exactly as before:
    /// no permit is stored anywhere and `tick()` still reconciles.
    #[tokio::test]
    async fn tick_without_wake_handle_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(db.clone(), runtime, manager_cfg(tmp.path().into()));
        assert!(mgr.wake.is_none(), "wake defaults to None");
        let session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        seed_pending_inbound(tmp.path(), &session);
        mgr.tick().await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }
}
