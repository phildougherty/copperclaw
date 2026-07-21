//! [`SweepService`] orchestrates the five maintenance checks once per
//! [`crate::SWEEP_POLL_MS`] tick and produces a [`SweepReport`] describing
//! sessions that need host attention.

use crate::actuator::StuckActuator;
use crate::alert_sink::OperatorAlertSink;
use crate::checks::apology::ApologyEmit;
use crate::checks::condition_checkin::{self, ConditionContext, ConditionStore};
use crate::checks::integrity::{INTEGRITY_ROTATION_SLOTS, IntegrityFinding};
use crate::checks::questions::QuestionExpiryEmit;
use crate::checks::stuck::StuckSeverity;
use crate::checks::{
    apology, goals, heartbeat, integrity, processing, questions, recurrence, scheduling, stuck,
    wake,
};
use crate::clock::{Clock, SystemClock};
use crate::error::SweepError;
use crate::spawn_tracker::SpawnAttemptTracker;
use chrono::{DateTime, Utc};
use copperclaw_db::central::CentralDb;
use copperclaw_types::{AgentGroupId, MessageId, SessionId};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A thin newtype around a raw per-session `rusqlite::Connection`. Each
/// sweep pass requests a fresh connection per session via the
/// [`SessionRoot`] trait, so there is no internal pool — but we leave room
/// in the abstraction for one to be added without changing callers.
pub struct SessionPool {
    conn: Connection,
}

impl SessionPool {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn into_conn(self) -> Connection {
        self.conn
    }
}

impl std::fmt::Debug for SessionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPool").finish_non_exhaustive()
    }
}

/// Translates `(agent_group_id, session_id)` pairs into open per-session
/// resources. The default implementation, [`FilesystemSessionRoot`], opens
/// the `inbound.db` / `outbound.db` files under a data root. Tests inject a
/// mock that returns connections to in-memory databases.
pub trait SessionRoot: Send + Sync {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, SweepError>;
    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, SweepError>;
    fn heartbeat_path(&self, agent_group_id: &AgentGroupId, session_id: &SessionId) -> PathBuf;
    /// Full filesystem layout for a session: the on-disk DB paths and the
    /// session root directory. The M21 O2 integrity check reads the DB
    /// paths (read-only `quick_check`) and writes its quarantine sidecar
    /// into the root dir — see [`crate::checks::integrity`].
    fn session_paths(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> copperclaw_db::session::SessionPaths;
}

/// Production [`SessionRoot`] backed by `copperclaw_db::session`. Each call
/// opens a fresh `Connection`; per-session DB files are tiny so this is
/// cheaper than maintaining a pool inside the sweep loop.
pub struct FilesystemSessionRoot {
    data_root: PathBuf,
}

impl FilesystemSessionRoot {
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }
}

impl SessionRoot for FilesystemSessionRoot {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, SweepError> {
        let paths = copperclaw_db::session::SessionPaths::new(
            &self.data_root,
            *agent_group_id,
            *session_id,
        );
        let conn = copperclaw_db::session::open_outbound(&paths)?;
        Ok(SessionPool::new(conn))
    }

    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, SweepError> {
        let paths = copperclaw_db::session::SessionPaths::new(
            &self.data_root,
            *agent_group_id,
            *session_id,
        );
        let conn = copperclaw_db::session::open_inbound(&paths)?;
        Ok(SessionPool::new(conn))
    }

    fn heartbeat_path(&self, agent_group_id: &AgentGroupId, session_id: &SessionId) -> PathBuf {
        copperclaw_db::session::SessionPaths::new(&self.data_root, *agent_group_id, *session_id)
            .heartbeat
    }

    fn session_paths(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> copperclaw_db::session::SessionPaths {
        copperclaw_db::session::SessionPaths::new(&self.data_root, *agent_group_id, *session_id)
    }
}

/// One inbound message whose `processing_ack` was reset back to `pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageReset {
    pub session_id: SessionId,
    pub message_id: MessageId,
    pub new_tries: i64,
}

/// One recurrence-fanout outcome. `series_id` matches the parent's
/// `series_id` (or is the parent's `message_id` if the parent had no
/// `series_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesFanout {
    pub series_id: String,
    pub new_message_id: MessageId,
    pub next_fire: DateTime<Utc>,
}

/// Aggregated outcome of one [`SweepService::run_once`] pass. The host
/// consumes this and translates each list into a container operation
/// (restart, ack-resend, etc.) — the sweep itself never touches container
/// runtimes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub stuck_sessions: Vec<SessionId>,
    /// M21 S2: the subset of [`Self::stuck_sessions`] whose active tool
    /// ran past [`crate::ABSOLUTE_CEILING_MS`]. These are the sessions
    /// the injected [`StuckActuator`] restarts; claim-threshold
    /// detections stay in `stuck_sessions` only (observe-only). Being a
    /// strict subset, this field is deliberately excluded from
    /// [`Self::is_empty`] / [`Self::total`] so entries are not counted
    /// twice.
    pub stuck_past_ceiling: Vec<SessionId>,
    pub recurrences_fired: Vec<SeriesFanout>,
    pub processing_acks_reset: Vec<MessageReset>,
    pub woken_sessions: Vec<SessionId>,
    pub heartbeat_stale: Vec<SessionId>,
    /// One [`SeriesFanout`] per condition-check-in that fired this pass
    /// (a stored HEARTBEAT-style condition that just became true). The
    /// series id is the condition id. Empty in the common case where no
    /// conditions are registered or none transitioned to true.
    pub condition_checkins_fired: Vec<SeriesFanout>,
    /// One entry per stuck-inbound apology written by the apology
    /// check during this pass. Empty in the common case where every
    /// session is healthy.
    pub apologies_emitted: Vec<ApologyEmit>,
    /// M21 F2: one entry per `ask_user_question` whose TTL lapsed this
    /// pass (expiry note + synthetic no-answer result written, or the
    /// lapse resolved silently by a later reply). Empty unless a
    /// question store is wired via [`SweepService::set_question_store`].
    pub questions_expired: Vec<QuestionExpiryEmit>,
    /// M21 O2: sessions newly quarantined this pass — a rotating
    /// `quick_check` found their per-session DB corrupt, a
    /// `.quarantined` sidecar was written, and the session is now
    /// excluded from sweeps. A real, escalating event: counted in
    /// [`Self::is_empty`] / [`Self::total`].
    pub integrity_quarantined: Vec<IntegrityFinding>,
    /// M21 O2: sessions whose per-session DBs were `quick_check`ed this
    /// pass (this pass's rotation slot) and found healthy. Purely
    /// observational — deliberately excluded from [`Self::is_empty`] /
    /// [`Self::total`] so a routine integrity rotation does not make
    /// every pass "non-empty".
    pub integrity_checked: Vec<SessionId>,
    /// M21 O2: already-quarantined sessions skipped this pass (their
    /// sidecar predates this pass). Observational; excluded from
    /// [`Self::is_empty`] / [`Self::total`].
    pub integrity_excluded: Vec<SessionId>,
    /// M22 A3: one [`SeriesFanout`] per long-running goal whose check-in fired
    /// this pass (a `kind:task` wake synthesised into the goal's session). The
    /// series id is the goal id. Empty when no goal was due a check-in.
    pub goal_checkins_fired: Vec<SeriesFanout>,
    /// M22 A3: goal ids paused this pass because their (grant-backed) budget was
    /// exhausted — the sweep stops spending on a goal whose authority is spent.
    /// A real state change: counted in [`Self::is_empty`] / [`Self::total`].
    pub goals_budget_paused: Vec<String>,
    /// M21 O2: true if the central DB was `quick_check`ed this pass (at
    /// boot, then daily). Observational.
    pub central_integrity_checked: bool,
    /// M21 O2: true if a central-DB `quick_check` this pass found
    /// corruption. Serious and unrecoverable by quarantine (the whole
    /// host depends on the central DB) — logged at ERROR and counted in
    /// [`Self::is_empty`].
    pub central_integrity_corrupt: bool,
}

impl SweepReport {
    /// True if every list is empty.
    pub fn is_empty(&self) -> bool {
        self.stuck_sessions.is_empty()
            && self.recurrences_fired.is_empty()
            && self.processing_acks_reset.is_empty()
            && self.woken_sessions.is_empty()
            && self.heartbeat_stale.is_empty()
            && self.apologies_emitted.is_empty()
            && self.condition_checkins_fired.is_empty()
            && self.questions_expired.is_empty()
            && self.integrity_quarantined.is_empty()
            && self.goal_checkins_fired.is_empty()
            && self.goals_budget_paused.is_empty()
            && !self.central_integrity_corrupt
    }

    /// Total number of items across all check categories.
    pub fn total(&self) -> usize {
        self.stuck_sessions.len()
            + self.recurrences_fired.len()
            + self.processing_acks_reset.len()
            + self.woken_sessions.len()
            + self.heartbeat_stale.len()
            + self.apologies_emitted.len()
            + self.condition_checkins_fired.len()
            + self.questions_expired.len()
            + self.integrity_quarantined.len()
            + self.goal_checkins_fired.len()
            + self.goals_budget_paused.len()
    }
}

/// Periodic maintenance task. Cheap to clone (everything is behind `Arc`).
pub struct SweepService {
    central: CentralDb,
    session_paths: Arc<dyn SessionRoot>,
    clock: Arc<dyn Clock>,
    /// Shared with the host's container manager (when wired through
    /// `with_spawn_tracker`). The manager bumps this on every failed
    /// `runtime.spawn` call; the apology check reads it to gate the
    /// `container_spawn_failed` reason. A default-empty tracker keeps
    /// the test path simple.
    spawn_tracker: Arc<SpawnAttemptTracker>,
    /// Registry of HEARTBEAT-style conditions evaluated each tick.
    /// Shared with the host (which registers conditions). Default-empty,
    /// so the condition-check-in is a strict no-op until populated and
    /// the sweep's existing behaviour is unchanged.
    condition_store: Arc<ConditionStore>,
    /// M21 S2 (decision (a)): restart authority for sessions whose
    /// active tool ran past [`crate::ABSOLUTE_CEILING_MS`]. Injected
    /// once at boot via [`Self::set_stuck_actuator`] (the host's
    /// container manager is built after the sweep, so this is a
    /// set-once slot rather than a constructor argument). Unset — the
    /// default, and every pre-S2 test — keeps ceiling detections
    /// observe-only, exactly the old behaviour.
    stuck_actuator: std::sync::OnceLock<Arc<dyn StuckActuator>>,
    /// M21 F2: shared handle onto the host's [`InteractiveModule`]
    /// (clones share pending-question state), injected once at boot via
    /// [`Self::set_question_store`] after module install. The
    /// question-expiry check reads lapsed questions through it. Unset —
    /// the default, and every pre-F2 test — the check is a no-op and
    /// unanswered questions keep the old (silent-evaporation) behaviour
    /// only in hosts that never wire the store.
    question_store: std::sync::OnceLock<copperclaw_modules::InteractiveModule>,
    /// M21 O2: monotonically-increasing sweep-pass counter. Selects the
    /// per-session integrity rotation slot for the pass (`pass %
    /// INTEGRITY_ROTATION_SLOTS`) so a healthy DB pays one `quick_check`
    /// per full rotation, not one per pass. Pass 0 is boot.
    integrity_pass: std::sync::atomic::AtomicU64,
    /// M21 O2: instant of the last central-DB `quick_check`, or `None`
    /// before the first (boot) check. Gates the daily central cadence.
    central_integrity_last: std::sync::Mutex<Option<DateTime<Utc>>>,
    /// M21 O4 (decision (d)): opt-in operator-alert push, injected once at
    /// boot via [`Self::set_operator_alerts`] (the host builds its
    /// `OperatorAlerts` after the sweep, so this is a set-once slot).
    /// Unset — the default, and every test — makes the quarantine alert a
    /// no-op and keeps the apology copy on the S2 honest text.
    operator_alerts: std::sync::OnceLock<Arc<dyn OperatorAlertSink>>,
}

impl SweepService {
    /// Build a service with the default [`SystemClock`].
    pub fn new(central: CentralDb, session_paths: Arc<dyn SessionRoot>) -> Self {
        Self {
            central,
            session_paths,
            clock: Arc::new(SystemClock),
            spawn_tracker: Arc::new(SpawnAttemptTracker::new()),
            condition_store: Arc::new(ConditionStore::new()),
            stuck_actuator: std::sync::OnceLock::new(),
            question_store: std::sync::OnceLock::new(),
            integrity_pass: std::sync::atomic::AtomicU64::new(0),
            central_integrity_last: std::sync::Mutex::new(None),
            operator_alerts: std::sync::OnceLock::new(),
        }
    }

    /// Build a service with a caller-supplied [`Clock`]. Used by tests and
    /// by host integration tests that want deterministic time.
    pub fn with_clock(
        central: CentralDb,
        session_paths: Arc<dyn SessionRoot>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            central,
            session_paths,
            clock,
            spawn_tracker: Arc::new(SpawnAttemptTracker::new()),
            condition_store: Arc::new(ConditionStore::new()),
            stuck_actuator: std::sync::OnceLock::new(),
            question_store: std::sync::OnceLock::new(),
            integrity_pass: std::sync::atomic::AtomicU64::new(0),
            central_integrity_last: std::sync::Mutex::new(None),
            operator_alerts: std::sync::OnceLock::new(),
        }
    }

    /// Replace the in-memory spawn-attempt tracker with one shared
    /// between the sweep and the container manager. Builders return
    /// `self` so this composes with [`Self::new`] / [`Self::with_clock`].
    #[must_use]
    pub fn with_spawn_tracker(mut self, tracker: Arc<SpawnAttemptTracker>) -> Self {
        self.spawn_tracker = tracker;
        self
    }

    /// Replace the in-memory condition store with one shared between the
    /// sweep and the host (the host registers conditions; the sweep
    /// evaluates + fires). Composes with the constructors.
    #[must_use]
    pub fn with_condition_store(mut self, store: Arc<ConditionStore>) -> Self {
        self.condition_store = store;
        self
    }

    /// Access the shared condition store. The host calls `register` /
    /// `remove` on this; the sweep's condition check reads it.
    pub fn condition_store(&self) -> &Arc<ConditionStore> {
        &self.condition_store
    }

    /// Access the configured clock (mostly for tests).
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Access the configured session root.
    pub fn session_root(&self) -> &Arc<dyn SessionRoot> {
        &self.session_paths
    }

    /// Access the central DB handle.
    pub fn central(&self) -> &CentralDb {
        &self.central
    }

    /// Access the shared spawn-attempt tracker. The host's container
    /// manager calls `record_failure` / `record_success` on this; the
    /// sweep's apology check reads it.
    pub fn spawn_tracker(&self) -> &Arc<SpawnAttemptTracker> {
        &self.spawn_tracker
    }

    /// Inject the stuck-tool restart actuator (M21 S2, decision (a)).
    /// Called once at boot after the host builds its container manager;
    /// a second call is ignored with a warning (the actuator is a
    /// boot-time wiring decision, not a runtime toggle). Takes `&self`
    /// because the service is already behind an `Arc` by the time the
    /// manager exists — the sweep loop's first pass fires a full
    /// [`crate::SWEEP_POLL_MS`] after boot, long after this returns.
    pub fn set_stuck_actuator(&self, actuator: Arc<dyn StuckActuator>) {
        if self.stuck_actuator.set(actuator).is_err() {
            tracing::warn!(
                target: "copperclaw_host_sweep",
                "stuck actuator already wired; ignoring second injection",
            );
        }
    }

    /// Inject the shared question store (M21 F2). Called once at boot
    /// after `install_modules` builds the host's `InteractiveModule`;
    /// the handle is a cheap clone sharing the module's live
    /// pending-question state. Mirrors [`Self::set_stuck_actuator`]:
    /// set-once, second injections are ignored with a warning, and the
    /// unset default keeps the question-expiry check a strict no-op.
    pub fn set_question_store(&self, store: copperclaw_modules::InteractiveModule) {
        if self.question_store.set(store).is_err() {
            tracing::warn!(
                target: "copperclaw_host_sweep",
                "question store already wired; ignoring second injection",
            );
        }
    }

    /// Inject the opt-in operator-alert sink (M21 O4, decision (d)).
    /// Called once at boot after the host builds its `OperatorAlerts`;
    /// mirrors [`Self::set_stuck_actuator`] (set-once, a second injection
    /// is ignored with a warning). Unset — the default, and every test —
    /// keeps the quarantine alert a no-op and the apology copy on the S2
    /// honest text.
    pub fn set_operator_alerts(&self, alerts: Arc<dyn OperatorAlertSink>) {
        if self.operator_alerts.set(alerts).is_err() {
            tracing::warn!(
                target: "copperclaw_host_sweep",
                "operator-alert sink already wired; ignoring second injection",
            );
        }
    }

    /// Whether an operator-alert destination is wired and enabled. Gates
    /// the apology copy's truthful "the operator has been notified" line
    /// (M21 O4). False with no sink wired — the pre-O4 default.
    pub fn operator_alerts_enabled(&self) -> bool {
        self.operator_alerts
            .get()
            .is_some_and(|sink| sink.is_enabled())
    }

    /// Fire one operator alert per session newly quarantined this pass
    /// (M21 O4 quarantine call site). No sink wired ⇒ a no-op; the host
    /// side dedups on the `quarantine:<session>` key so a session stays
    /// quarantined without re-alerting every pass. Called from
    /// [`Self::run_once_actuated`] — the pure `run_once` stays
    /// side-effect-free for tests and the replay harness.
    fn alert_quarantines(&self, report: &SweepReport) {
        let Some(sink) = self.operator_alerts.get() else {
            return;
        };
        for finding in &report.integrity_quarantined {
            sink.fire(
                "critical",
                &format!("quarantine:{}", finding.session_id),
                "A per-session database was quarantined for corruption; that session is \
                 excluded from sweeps until an operator intervenes. See `cclaw doctor`.",
            );
        }
    }

    /// Drive the injected [`StuckActuator`] for every session the pass
    /// found past the absolute ceiling. Returns the number of restarts
    /// the actuator accepted. With no actuator wired (tests, a host
    /// booted without a container manager) this is a no-op beyond a
    /// debug log — detections stay observe-only, the pre-S2 behaviour.
    ///
    /// Failures are logged and skipped rather than aborting the batch:
    /// the stuck detection re-fires on the next pass until the session's
    /// tool state clears, so a failed restart retries naturally.
    pub async fn actuate_stuck(&self, report: &SweepReport) -> usize {
        if report.stuck_past_ceiling.is_empty() {
            return 0;
        }
        let Some(actuator) = self.stuck_actuator.get() else {
            tracing::debug!(
                target: "copperclaw_host_sweep",
                sessions = report.stuck_past_ceiling.len(),
                "stuck sessions past absolute ceiling but no actuator wired; observe-only",
            );
            return 0;
        };
        let mut restarted = 0;
        for session_id in &report.stuck_past_ceiling {
            match actuator.restart_stuck(*session_id).await {
                Ok(()) => {
                    restarted += 1;
                    tracing::info!(
                        target: "copperclaw_host_sweep",
                        session = %session_id,
                        "stuck-tool restart requested (tool past absolute ceiling)",
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        target: "copperclaw_host_sweep",
                        session = %session_id,
                        error = %err,
                        "stuck-tool restart failed; will retry next sweep pass",
                    );
                }
            }
        }
        restarted
    }

    /// [`Self::run_once`] followed by [`Self::actuate_stuck`] — the
    /// full sweep pass as the production loop runs it. Split so tests
    /// (and the replay harness) can still call the pure `run_once`
    /// without triggering restarts.
    pub async fn run_once_actuated(&self) -> Result<SweepReport, SweepError> {
        let report = self.run_once()?;
        self.alert_quarantines(&report);
        self.actuate_stuck(&report).await;
        Ok(report)
    }

    /// Tick `run_once` every [`crate::SWEEP_POLL_MS`] until `shutdown` is
    /// cancelled. Errors are logged but do not abort the loop.
    pub async fn run_loop(self: Arc<Self>, shutdown: CancellationToken) {
        let interval = Duration::from_millis(crate::SWEEP_POLL_MS);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(interval) => {
                    match self.run_once_actuated().await {
                        Ok(report) => {
                            // M22 A3 metrics: consume the sweep report's goal
                            // fan-out counts once here (one site covers both
                            // counters). No-ops when the pass fired nothing.
                            copperclaw_metrics::add_goal_checkins_fired(
                                report.goal_checkins_fired.len() as u64,
                            );
                            copperclaw_metrics::add_goals_budget_paused(
                                report.goals_budget_paused.len() as u64,
                            );
                            if !report.is_empty() {
                                tracing::info!(
                                    target: "copperclaw_host_sweep",
                                    stuck = report.stuck_sessions.len(),
                                    stuck_past_ceiling = report.stuck_past_ceiling.len(),
                                    recurrences = report.recurrences_fired.len(),
                                    acks_reset = report.processing_acks_reset.len(),
                                    woken = report.woken_sessions.len(),
                                    heartbeat_stale = report.heartbeat_stale.len(),
                                    apologies = report.apologies_emitted.len(),
                                    condition_checkins = report.condition_checkins_fired.len(),
                                    questions_expired = report.questions_expired.len(),
                                    integrity_quarantined = report.integrity_quarantined.len(),
                                    goal_checkins = report.goal_checkins_fired.len(),
                                    goals_budget_paused = report.goals_budget_paused.len(),
                                    central_integrity_corrupt = report.central_integrity_corrupt,
                                    "sweep pass produced report",
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "copperclaw_host_sweep",
                                error = %e,
                                "sweep pass failed",
                            );
                        }
                    }
                }
            }
        }
    }

    /// Sample the observable [`ConditionContext`] for one session — the input
    /// all three [`crate::checks::condition_checkin::ConditionKind`]s evaluate
    /// against. A failure to read any one signal degrades that signal to its
    /// quiet default rather than aborting the pass — a condition simply will
    /// not fire for a signal we cannot sample, the safe default.
    ///
    /// M22 A4 populates the two signals the revived condition check needs
    /// beyond `pending_inbound`:
    ///   * `idle_secs` — seconds since the session's `last_active` (drives
    ///     [`IdleForAtLeastSecs`](crate::checks::condition_checkin::ConditionKind::IdleForAtLeastSecs));
    ///   * `flags_set` — the session's currently-set `condition_flags`
    ///     latches (drives
    ///     [`FlagSet`](crate::checks::condition_checkin::ConditionKind::FlagSet)).
    fn sample_condition_context(
        &self,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> ConditionContext {
        // We need the agent_group_id to open the per-session DB. The
        // condition store carries it, so look it up there.
        let Some(cond) = self
            .condition_store
            .for_session(session_id)
            .into_iter()
            .next()
        else {
            return ConditionContext::quiet();
        };
        let pending = self
            .session_paths
            .inbound_pool(&cond.agent_group_id, session_id)
            .ok()
            .and_then(|mut pool| {
                copperclaw_db::tables::messages_in::count_pending_for_typing(pool.conn_mut()).ok()
            })
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        // M22 A4: idle duration since the session's last recorded activity.
        // A session with `last_active` in the future (clock skew) reads as 0
        // idle, never negative.
        let idle_secs = copperclaw_db::tables::sessions::get(&self.central, *session_id)
            .ok()
            .map(|s| {
                let secs = now.signed_duration_since(s.last_active).num_seconds();
                u64::try_from(secs.max(0)).unwrap_or(0)
            });
        // M22 A4: operator/agent-set flag latches for this session.
        let flags_set =
            copperclaw_db::tables::conditions::list_flags_for_session(&self.central, *session_id)
                .unwrap_or_default();
        ConditionContext {
            pending_inbound: pending,
            idle_secs,
            flags_set,
        }
    }

    /// M21 O2: run a central-DB `quick_check` if due — at boot (the first
    /// pass) and once per day thereafter. Returns `(checked, corrupt)`.
    /// Central corruption cannot be quarantined (the whole host depends on
    /// the central DB), so it is logged at ERROR and surfaced in the
    /// report for `cclaw doctor`; recovery is an operator action.
    fn check_central_integrity(&self, now: DateTime<Utc>) -> (bool, bool) {
        let day = chrono::Duration::days(1);
        {
            let last = self
                .central_integrity_last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(prev) = *last {
                if now.signed_duration_since(prev) < day {
                    return (false, false);
                }
            }
        }
        // Due: run the check. Record the attempt time regardless of
        // outcome so a corrupt central DB is not re-probed every pass.
        *self
            .central_integrity_last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(now);

        let conn = match self.central.conn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "copperclaw_host_sweep",
                    error = %e,
                    "central integrity check skipped: could not borrow central connection",
                );
                return (false, false);
            }
        };
        // M21 O2 (M1 rider): central-DB integrity quick-check outcome.
        let central_outcome = copperclaw_db::integrity::quick_check_conn(&conn);
        copperclaw_metrics::inc_integrity_quick_check(
            copperclaw_metrics::INTEGRITY_SCOPE_CENTRAL,
            match &central_outcome {
                copperclaw_db::integrity::QuickCheckOutcome::Healthy => "healthy",
                copperclaw_db::integrity::QuickCheckOutcome::Missing => "missing",
                copperclaw_db::integrity::QuickCheckOutcome::Corrupt(_) => "corrupt",
            },
        );
        match central_outcome {
            copperclaw_db::integrity::QuickCheckOutcome::Healthy
            | copperclaw_db::integrity::QuickCheckOutcome::Missing => (true, false),
            copperclaw_db::integrity::QuickCheckOutcome::Corrupt(detail) => {
                tracing::error!(
                    target: "copperclaw_host_sweep",
                    detail = %detail,
                    "central DB failed quick_check — this is not recoverable by quarantine; \
                     an operator must restore from backup (cclaw doctor will surface it)",
                );
                (true, true)
            }
        }
    }

    /// Run a single sweep pass and return a populated [`SweepReport`].
    ///
    /// Errors during one session's check are logged and skipped — the pass
    /// continues with the next session — except for failures reading the
    /// central session list, which abort the pass with `Err`.
    #[allow(clippy::too_many_lines)]
    pub fn run_once(&self) -> Result<SweepReport, SweepError> {
        let now = self.clock.now();
        let sessions = copperclaw_db::tables::sessions::list_active(&self.central)?;

        let mut report = SweepReport::default();

        // M21 O2: which pass is this, and thus which per-session integrity
        // rotation slot does it check? `fetch_add` returns the pre-increment
        // value, so the first pass is 0 (boot).
        let pass = self
            .integrity_pass
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let integrity_slot = pass % INTEGRITY_ROTATION_SLOTS;

        // Global: central-DB integrity (boot + daily). Independent of the
        // per-session rotation below.
        let (central_checked, central_corrupt) = self.check_central_integrity(now);
        report.central_integrity_checked = central_checked;
        report.central_integrity_corrupt = central_corrupt;

        // Global: scan the central `tasks` table for due scheduled
        // tasks, synthesise wake inbounds, and re-arm / mark completed.
        // Errors here are logged and swallowed so per-session checks
        // still run; a single sqlite hiccup must not abort the pass.
        match scheduling::check(&self.central, self.session_paths.as_ref(), now) {
            Ok(mut fan) => report.recurrences_fired.append(&mut fan),
            Err(e) => tracing::warn!(
                target: "copperclaw_host_sweep",
                error = %e,
                "scheduled-task fan-out failed",
            ),
        }

        // M22 A3: goal check-in fan-out. Like the scheduled-task scan above,
        // this is a global central-DB scan (`goals` table) that synthesises a
        // `kind:task` wake per due goal and pauses goals whose grant-backed
        // budget is exhausted. Additive + self-contained so later lane-W cards
        // (A4/A5) rebase alongside it. Errors are logged and swallowed so a
        // sqlite hiccup here never aborts the per-session checks below.
        match goals::check(&self.central, self.session_paths.as_ref(), now) {
            Ok(mut goal_report) => {
                report.goal_checkins_fired.append(&mut goal_report.fired);
                report
                    .goals_budget_paused
                    .append(&mut goal_report.budget_paused);
            }
            Err(e) => tracing::warn!(
                target: "copperclaw_host_sweep",
                error = %e,
                "goal check-in fan-out failed",
            ),
        }

        // M22 A4: reload the durable `conditions` table into the shared
        // in-memory `ConditionStore` before evaluating. This is what revives
        // the previously-dormant check — a registered condition survives a
        // restart (the store is rebuilt from the DB each pass) and a
        // deregistered one stops firing. `reconcile` preserves the rising-edge
        // latch of an unchanged condition so a still-true condition does not
        // re-fire every pass. Errors are logged and swallowed (an empty reload
        // leaves the store as-is) so a sqlite hiccup never aborts the pass.
        match copperclaw_db::tables::conditions::list_active(&self.central) {
            Ok(rows) => {
                let desired = rows
                    .into_iter()
                    .filter_map(condition_checkin::Condition::from_stored)
                    .collect();
                self.condition_store.reconcile(desired);
            }
            Err(e) => tracing::warn!(
                target: "copperclaw_host_sweep",
                error = %e,
                "condition reload from central DB failed; keeping prior store",
            ),
        }

        // Global: HEARTBEAT-style condition check-ins. Distinct from the
        // time-based scheduling above — these fire when a *stored
        // condition currently holds*, not when a clock deadline elapses.
        // The sampler builds each session's observable context (pending
        // inbound count, idle duration, set flags — M22 A4) from the central
        // + per-session DBs; the check fires a wake inbound only on a
        // condition's rising edge and audits each fire. Empty store => no-op.
        let sampler = |sid: &SessionId| self.sample_condition_context(sid, now);
        match condition_checkin::check(
            self.condition_store.as_ref(),
            &self.central,
            self.session_paths.as_ref(),
            &sampler,
            now,
        ) {
            Ok(mut fan) => report.condition_checkins_fired.append(&mut fan),
            Err(e) => tracing::warn!(
                target: "copperclaw_host_sweep",
                error = %e,
                "condition-check-in fan-out failed",
            ),
        }

        // Global: question expiry (M21 F2). The store carries each
        // question's origin (session + routing captured at ask time),
        // so this runs once per pass rather than per session. No store
        // wired (tests, hosts without module install) => strict no-op.
        if let Some(store) = self.question_store.get() {
            let mut expired = questions::check(self.session_paths.as_ref(), store, now);
            report.questions_expired.append(&mut expired);
        }

        for session in sessions {
            // M21 O2: a session already quarantined (its `.quarantined`
            // sidecar predates this pass, surviving any host restart) is
            // excluded from ALL sweep work. This replaces today's silent
            // per-pass log-and-swallow in every downstream check: the loud
            // escalation happened once, at quarantine time.
            if integrity::is_quarantined(
                self.session_paths.as_ref(),
                &session.agent_group_id,
                &session.id,
            ) {
                // M21 O2 (M1 rider): the excluded population feeds the
                // integrity_quarantined_sessions gauge, set once at the end of
                // the pass from the report.
                report.integrity_excluded.push(session.id);
                continue;
            }

            // M21 O2: rotating integrity probe. Only sessions in this
            // pass's slot pay the `quick_check` cost, so a healthy DB is
            // probed once per full rotation, not once per pass. A newly
            // corrupt DB is quarantined here and excluded from the rest of
            // this pass's checks (so a corrupt DB never reaches the checks
            // that would otherwise fail-and-swallow it).
            if integrity::rotation_slot(&session.id, INTEGRITY_ROTATION_SLOTS) == integrity_slot {
                match integrity::check_and_quarantine(
                    self.session_paths.as_ref(),
                    &session.agent_group_id,
                    &session.id,
                    now,
                ) {
                    Ok(Some(finding)) => {
                        // ONE escalating log line per quarantine (decision
                        // (f)) — replaces the old silent per-pass skip.
                        tracing::error!(
                            target: "copperclaw_host_sweep",
                            session = %session.id,
                            db = finding.db,
                            detail = %finding.detail,
                            "session DB failed quick_check — quarantined and excluded from sweeps \
                             (cclaw doctor will surface it)",
                        );
                        report.integrity_quarantined.push(finding);
                        continue;
                    }
                    Ok(None) => report.integrity_checked.push(session.id),
                    Err(e) => tracing::warn!(
                        target: "copperclaw_host_sweep",
                        session = %session.id,
                        error = %e,
                        "integrity check could not write quarantine sidecar; will retry",
                    ),
                }
            }

            // Heartbeat is checked even for non-running containers because
            // a session whose container died silently won't have a fresh
            // heartbeat.
            match heartbeat::check(
                self.session_paths.as_ref(),
                &session.agent_group_id,
                &session.id,
                now,
            ) {
                Ok(true) => report.heartbeat_stale.push(session.id),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    target: "copperclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "heartbeat check failed",
                ),
            }

            match stuck::check(
                self.session_paths.as_ref(),
                &session.agent_group_id,
                &session.id,
                now,
            ) {
                Ok(Some(severity)) => {
                    report.stuck_sessions.push(session.id);
                    if severity == StuckSeverity::AbsoluteCeiling {
                        report.stuck_past_ceiling.push(session.id);
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    target: "copperclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "stuck-tool check failed",
                ),
            }

            match processing::check(
                self.session_paths.as_ref(),
                &session.agent_group_id,
                &session.id,
                now,
            ) {
                Ok(mut resets) => report.processing_acks_reset.append(&mut resets),
                Err(e) => tracing::warn!(
                    target: "copperclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "processing-ack check failed",
                ),
            }

            // M22 A5: the per-session recurrence engine is now a deprecation
            // shim that forwards each legacy recurring series into the central
            // `tasks` scheduler (and neutralises the source rows), so all
            // recurrence gains list/pause/resume/cancel. It therefore needs the
            // central DB handle to create/inspect the target task rows.
            match recurrence::check(
                &self.central,
                self.session_paths.as_ref(),
                &session.agent_group_id,
                &session.id,
                now,
            ) {
                Ok(mut fan) => report.recurrences_fired.append(&mut fan),
                Err(e) => tracing::warn!(
                    target: "copperclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "recurrence-consolidation check failed",
                ),
            }

            match wake::check(&self.central, self.session_paths.as_ref(), &session, now) {
                Ok(true) => report.woken_sessions.push(session.id),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    target: "copperclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "due-message wake check failed",
                ),
            }

            match apology::check(
                self.session_paths.as_ref(),
                self.spawn_tracker.as_ref(),
                &session,
                now,
                self.operator_alerts_enabled(),
            ) {
                Ok(mut emits) => report.apologies_emitted.append(&mut emits),
                Err(e) => tracing::warn!(
                    target: "copperclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "stuck-inbound apology check failed",
                ),
            }
        }

        // M21 M1 rider: pass-level metrics driven by the assembled report.
        // Quarantined-sessions gauge = already-excluded + newly quarantined
        // this pass (the live sweep-excluded population).
        copperclaw_metrics::set_integrity_quarantined_sessions(
            (report.integrity_excluded.len() + report.integrity_quarantined.len()) as u64,
        );
        // F2: question expiries by outcome.
        for q in &report.questions_expired {
            copperclaw_metrics::inc_question_expiry(if q.resolved_by_reply {
                "resolved_by_reply"
            } else {
                "surfaced"
            });
        }
        // Long-wished: last successful sweep-pass wall clock, for a
        // "sweep is wedged" alert (`time() - <this>`).
        copperclaw_metrics::set_sweep_last_run_timestamp(now.timestamp());

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use crate::test_support::{
        MemSessionRoot, seed_due_message, seed_recurrence, seed_running_session,
        seed_stale_heartbeat, seed_stuck_processing_ack, seed_stuck_tool,
    };
    use chrono::{Duration as ChDuration, TimeZone};
    use copperclaw_db::tables::sessions as sessions_tbl;

    fn fresh_central() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[tokio::test]
    async fn run_once_empty_central_returns_empty_report() {
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let svc = SweepService::new(central, root);
        let report = svc.run_once().unwrap();
        assert!(report.is_empty());
        assert_eq!(report.total(), 0);
    }

    /// Test double for the M21 O4 operator-alert sink: records every
    /// `fire` and reports itself enabled.
    struct RecordingSink {
        fired: std::sync::Mutex<Vec<(String, String, String)>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                fired: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl OperatorAlertSink for RecordingSink {
        fn fire(&self, severity: &str, dedup_key: &str, message: &str) {
            self.fired.lock().unwrap().push((
                severity.to_string(),
                dedup_key.to_string(),
                message.to_string(),
            ));
        }
        fn is_enabled(&self) -> bool {
            true
        }
    }

    /// M21 O4 quarantine call site: a session newly quarantined this pass
    /// fires exactly one critical alert keyed on `quarantine:<session>`.
    #[test]
    fn quarantine_fires_one_operator_alert_per_finding() {
        let svc = SweepService::new(fresh_central(), Arc::new(MemSessionRoot::new()));
        let sink = Arc::new(RecordingSink::new());
        svc.set_operator_alerts(Arc::clone(&sink) as Arc<dyn OperatorAlertSink>);
        let session_id = SessionId::new();
        let report = SweepReport {
            integrity_quarantined: vec![IntegrityFinding {
                session_id,
                db: "outbound.db",
                detail: "database disk image is malformed".to_string(),
            }],
            ..Default::default()
        };
        svc.alert_quarantines(&report);
        let fired = sink.fired.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, "critical");
        assert_eq!(fired[0].1, format!("quarantine:{session_id}"));
        assert!(fired[0].2.contains("quarantined"));
    }

    /// No sink wired ⇒ the quarantine alert is a silent no-op (pre-O4
    /// behaviour) and `operator_alerts_enabled` is false.
    #[test]
    fn quarantine_without_sink_is_a_noop() {
        let svc = SweepService::new(fresh_central(), Arc::new(MemSessionRoot::new()));
        assert!(!svc.operator_alerts_enabled());
        let report = SweepReport {
            integrity_quarantined: vec![IntegrityFinding {
                session_id: SessionId::new(),
                db: "inbound.db",
                detail: "corrupt".to_string(),
            }],
            ..Default::default()
        };
        // Must not panic with no sink wired.
        svc.alert_quarantines(&report);
    }

    /// Test double for the M21 S2 actuator: records every requested
    /// restart; optionally fails for one session to prove a failed
    /// restart doesn't abort the batch.
    struct RecordingActuator {
        calls: std::sync::Mutex<Vec<SessionId>>,
        fail_for: Option<SessionId>,
    }

    impl RecordingActuator {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_for: None,
            }
        }

        fn failing_for(session: SessionId) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                fail_for: Some(session),
            }
        }

        fn calls(&self) -> Vec<SessionId> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::actuator::StuckActuator for RecordingActuator {
        async fn restart_stuck(
            &self,
            session_id: SessionId,
        ) -> Result<(), crate::actuator::ActuatorError> {
            self.calls.lock().unwrap().push(session_id);
            if self.fail_for == Some(session_id) {
                return Err("injected restart failure".into());
            }
            Ok(())
        }
    }

    /// The S2 acceptance split: a tool past the 30-minute absolute
    /// ceiling is actuated; a tool merely past the 60s claim threshold
    /// is reported but NOT actuated (observe-only).
    #[tokio::test]
    async fn actuator_fires_only_past_the_ceiling() {
        let central = fresh_central();
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(now));
        let root = Arc::new(MemSessionRoot::new());

        // Past the claim threshold (5 min) but under the ceiling.
        let floor_stuck = seed_running_session(&central);
        seed_stuck_tool(&root, &floor_stuck, now - ChDuration::minutes(5));

        // Past the 30-minute absolute ceiling.
        let ceiling_stuck = seed_running_session(&central);
        seed_stuck_tool(&root, &ceiling_stuck, now - ChDuration::minutes(31));

        let svc = SweepService::with_clock(central, root, clock);
        let actuator = Arc::new(RecordingActuator::new());
        svc.set_stuck_actuator(Arc::clone(&actuator) as Arc<dyn crate::actuator::StuckActuator>);

        let report = svc.run_once_actuated().await.unwrap();

        // Both are stuck; only the ceiling one is in the actuator list.
        assert!(report.stuck_sessions.contains(&floor_stuck.id));
        assert!(report.stuck_sessions.contains(&ceiling_stuck.id));
        assert_eq!(report.stuck_past_ceiling, vec![ceiling_stuck.id]);

        // And the actuator saw exactly the ceiling session.
        assert_eq!(actuator.calls(), vec![ceiling_stuck.id]);
    }

    /// With no actuator wired (the default, and every pre-S2 caller),
    /// ceiling detections stay observe-only — no panic, no restart.
    #[tokio::test]
    async fn ceiling_detection_without_actuator_is_observe_only() {
        let central = fresh_central();
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(now));
        let root = Arc::new(MemSessionRoot::new());
        let stuck = seed_running_session(&central);
        seed_stuck_tool(&root, &stuck, now - ChDuration::minutes(31));

        let svc = SweepService::with_clock(central, root, clock);
        let report = svc.run_once_actuated().await.unwrap();
        assert_eq!(report.stuck_past_ceiling, vec![stuck.id]);
        assert_eq!(svc.actuate_stuck(&report).await, 0);
    }

    /// One failing restart must not abort the rest of the batch; the
    /// return value counts only the accepted restarts.
    #[tokio::test]
    async fn actuator_failure_does_not_abort_the_batch() {
        let central = fresh_central();
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(now));
        let root = Arc::new(MemSessionRoot::new());
        let a = seed_running_session(&central);
        seed_stuck_tool(&root, &a, now - ChDuration::minutes(31));
        let b = seed_running_session(&central);
        seed_stuck_tool(&root, &b, now - ChDuration::minutes(45));

        let svc = SweepService::with_clock(central, root, clock);
        let actuator = Arc::new(RecordingActuator::failing_for(a.id));
        svc.set_stuck_actuator(Arc::clone(&actuator) as Arc<dyn crate::actuator::StuckActuator>);

        let report = svc.run_once().unwrap();
        assert_eq!(report.stuck_past_ceiling.len(), 2);
        let restarted = svc.actuate_stuck(&report).await;
        assert_eq!(restarted, 1, "only the non-failing session counts");
        assert_eq!(actuator.calls().len(), 2, "both sessions were attempted");
    }

    /// The actuator slot is set-once: a second injection is ignored so
    /// a misbehaving caller can't swap restart authority at runtime.
    #[tokio::test]
    async fn second_actuator_injection_is_ignored() {
        let central = fresh_central();
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(now));
        let root = Arc::new(MemSessionRoot::new());
        let stuck = seed_running_session(&central);
        seed_stuck_tool(&root, &stuck, now - ChDuration::minutes(31));

        let svc = SweepService::with_clock(central, root, clock);
        let first = Arc::new(RecordingActuator::new());
        let second = Arc::new(RecordingActuator::new());
        svc.set_stuck_actuator(Arc::clone(&first) as Arc<dyn crate::actuator::StuckActuator>);
        svc.set_stuck_actuator(Arc::clone(&second) as Arc<dyn crate::actuator::StuckActuator>);

        let _ = svc.run_once_actuated().await.unwrap();
        assert_eq!(first.calls(), vec![stuck.id], "first injection wins");
        assert!(second.calls().is_empty(), "second injection is inert");
    }

    #[tokio::test]
    async fn run_once_populates_each_branch() {
        let central = fresh_central();
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(now));
        let root = Arc::new(MemSessionRoot::new());

        let stuck = seed_running_session(&central);
        seed_stuck_tool(&root, &stuck, now - ChDuration::minutes(5));

        let stale_hb = seed_running_session(&central);
        seed_stale_heartbeat(&root, &stale_hb, now - ChDuration::minutes(5));

        let ack = seed_running_session(&central);
        let _ack_msg = seed_stuck_processing_ack(&root, &ack, now - ChDuration::minutes(5));

        let due = seed_running_session(&central);
        // Mark it idle so wake branch fires.
        sessions_tbl::mark_container_idle(&central, due.id).unwrap();
        seed_due_message(&root, &due, now - ChDuration::seconds(1));

        let recur = seed_running_session(&central);
        seed_recurrence(&root, &recur, "0 */2 * * *", now - ChDuration::days(1));

        let svc = SweepService::with_clock(central, root, clock);
        let report = svc.run_once().unwrap();

        assert!(report.stuck_sessions.contains(&stuck.id), "stuck branch");
        // 5 minutes is past the claim threshold but well under the
        // 30-minute absolute ceiling: observe-only, never actuated.
        assert!(
            report.stuck_past_ceiling.is_empty(),
            "claim-threshold detections must stay out of the actuator list",
        );
        assert!(
            report.heartbeat_stale.contains(&stale_hb.id),
            "heartbeat branch",
        );
        assert_eq!(report.processing_acks_reset.len(), 1, "ack branch");
        assert_eq!(report.processing_acks_reset[0].session_id, ack.id);
        assert!(report.woken_sessions.contains(&due.id), "wake branch");
        assert_eq!(report.recurrences_fired.len(), 1, "recurrence branch");
    }

    /// M21 F2 at the service level: with a wired question store, a
    /// question inside its TTL is untouched by one pass and surfaced by
    /// a later pass once the (mock) clock crosses the deadline. Zero
    /// real waits — the sweep clock is the seam.
    #[tokio::test]
    async fn run_once_surfaces_expired_questions_when_store_wired() {
        use copperclaw_modules::{InteractiveModule, QuestionId, QuestionOrigin};

        let central = fresh_central();
        let t0 = chrono::Utc.with_ymd_and_hms(2026, 7, 17, 12, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(t0 + ChDuration::hours(23)));
        let root = Arc::new(MemSessionRoot::new());
        let session = seed_running_session(&central);

        let store = InteractiveModule::default(); // 24h TTL
        store.ask(
            QuestionId::new("q_svc"),
            "Deploy now?".into(),
            vec!["yes".into(), "no".into()],
            QuestionOrigin {
                session_id: Some(session.id),
                agent_group_id: Some(session.agent_group_id),
                message_out_id: None,
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                platform_id: Some("stdin".into()),
                thread_id: None,
            },
            t0,
        );
        // Materialise the per-session DBs so the check can read/write.
        let _ = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        let _ = root
            .outbound_pool(&session.agent_group_id, &session.id)
            .unwrap();

        let svc = SweepService::with_clock(central, root, clock.clone());
        svc.set_question_store(store.clone());

        // 23h in: inside the TTL, untouched.
        let report = svc.run_once().unwrap();
        assert!(report.questions_expired.is_empty(), "inside TTL: no-op");
        assert_eq!(store.pending().len(), 1);

        // 25h in: expired, surfaced exactly once, terminal in state.
        clock.advance(ChDuration::hours(2));
        let report = svc.run_once().unwrap();
        assert_eq!(report.questions_expired.len(), 1);
        assert_eq!(report.questions_expired[0].question_id, "q_svc");
        assert!(report.questions_expired[0].note_emitted);
        assert!(!report.is_empty());
        // total() counts the expiry (the session's missing heartbeat
        // file also reports stale here — incidental to this test).
        assert!(report.total() >= 1);
        assert!(store.pending().is_empty());

        // Next pass is quiet again.
        let report = svc.run_once().unwrap();
        assert!(report.questions_expired.is_empty());
    }

    /// No store wired (the default and every pre-F2 caller): the check
    /// never runs and the report field stays empty.
    #[tokio::test]
    async fn run_once_without_question_store_reports_no_question_expiries() {
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let _session = seed_running_session(&central);
        let svc = SweepService::new(central, root);
        let report = svc.run_once().unwrap();
        assert!(report.questions_expired.is_empty());
    }

    /// The store slot is set-once, mirroring the actuator slot.
    #[tokio::test]
    async fn second_question_store_injection_is_ignored() {
        use copperclaw_modules::{InteractiveModule, QuestionId, QuestionOrigin};

        let central = fresh_central();
        let t0 = chrono::Utc.with_ymd_and_hms(2026, 7, 17, 12, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(t0 + ChDuration::hours(25)));
        let root = Arc::new(MemSessionRoot::new());
        let session = seed_running_session(&central);

        let first = InteractiveModule::default();
        first.ask(
            QuestionId::new("q_first"),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin {
                session_id: Some(session.id),
                agent_group_id: Some(session.agent_group_id),
                message_out_id: None,
                channel_type: None,
                platform_id: None,
                thread_id: None,
            },
            t0,
        );
        let second = InteractiveModule::default();
        second.ask(
            QuestionId::new("q_second"),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0,
        );
        let _ = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();

        let svc = SweepService::with_clock(central, root, clock);
        svc.set_question_store(first);
        svc.set_question_store(second.clone());

        let report = svc.run_once().unwrap();
        assert_eq!(report.questions_expired.len(), 1);
        assert_eq!(
            report.questions_expired[0].question_id, "q_first",
            "first injection wins",
        );
        assert_eq!(second.pending().len(), 1, "second injection is inert");
    }

    /// M21 O2 acceptance: over one full rotation of
    /// `INTEGRITY_ROTATION_SLOTS` passes, every healthy session's DB is
    /// `quick_check`ed exactly once — one probe per rotation slot, NOT one
    /// per pass.
    #[tokio::test]
    async fn integrity_rotation_checks_each_healthy_session_once_per_rotation() {
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        // Seed a handful of sessions with materialised (healthy) DBs.
        let mut ids = Vec::new();
        for _ in 0..8 {
            let s = seed_running_session(&central);
            let _ = root.outbound_pool(&s.agent_group_id, &s.id).unwrap();
            let _ = root.inbound_pool(&s.agent_group_id, &s.id).unwrap();
            ids.push(s.id);
        }
        let svc = SweepService::new(central, root);

        let mut checks: std::collections::HashMap<SessionId, usize> =
            ids.iter().map(|id| (*id, 0usize)).collect();
        for _ in 0..INTEGRITY_ROTATION_SLOTS {
            let report = svc.run_once().unwrap();
            for id in report.integrity_checked {
                *checks.get_mut(&id).unwrap() += 1;
            }
            assert!(
                report.integrity_quarantined.is_empty(),
                "healthy: no quarantine"
            );
        }
        for id in &ids {
            assert_eq!(
                checks[id], 1,
                "each healthy session is quick_checked exactly once per rotation, not per pass",
            );
        }
    }

    /// M21 O2 acceptance: a deliberately-corrupted per-session DB is
    /// detected on its rotation slot, quarantined (sidecar written), and
    /// excluded from subsequent sweeps.
    #[tokio::test]
    async fn corrupt_session_is_detected_quarantined_and_excluded() {
        use crate::test_support::corrupt_session_db;
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let session = seed_running_session(&central);
        corrupt_session_db(&root, &session, "outbound.db");

        let dyn_root: Arc<dyn SessionRoot> = root.clone();
        let svc = SweepService::new(central, dyn_root);

        // Run a full rotation; the session's slot is hit exactly once, and
        // that pass quarantines it.
        let mut quarantined_passes = 0;
        for _ in 0..INTEGRITY_ROTATION_SLOTS {
            let report = svc.run_once().unwrap();
            if report
                .integrity_quarantined
                .iter()
                .any(|f| f.session_id == session.id)
            {
                quarantined_passes += 1;
            }
        }
        assert_eq!(quarantined_passes, 1, "quarantined exactly once");
        assert!(crate::checks::integrity::is_quarantined(
            root.as_ref(),
            &session.agent_group_id,
            &session.id,
        ));

        // Every subsequent pass excludes it: never checked, never
        // quarantined again, never reaching the heartbeat/stuck/etc checks.
        let report = svc.run_once().unwrap();
        assert!(report.integrity_excluded.contains(&session.id));
        assert!(!report.integrity_checked.contains(&session.id));
        assert!(report.integrity_quarantined.is_empty());
        assert!(
            !report.heartbeat_stale.contains(&session.id),
            "excluded sessions must not reach downstream checks",
        );
    }

    /// M21 O2 acceptance: quarantine is a file, so it survives a host
    /// restart. A brand-new `SweepService` (fresh in-memory rotation
    /// state) built over the same on-disk root still excludes the
    /// previously-quarantined session from pass one.
    #[tokio::test]
    async fn quarantine_survives_a_restart() {
        use crate::test_support::corrupt_session_db;
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let session = seed_running_session(&central);
        corrupt_session_db(&root, &session, "inbound.db");

        // First host lifetime: quarantine it.
        let dyn_root: Arc<dyn SessionRoot> = root.clone();
        let svc1 = SweepService::new(central.clone(), dyn_root);
        for _ in 0..INTEGRITY_ROTATION_SLOTS {
            let _ = svc1.run_once().unwrap();
        }
        assert!(crate::checks::integrity::is_quarantined(
            root.as_ref(),
            &session.agent_group_id,
            &session.id,
        ));
        drop(svc1);

        // "Restart": a fresh service over the same on-disk root. Its very
        // first pass (pass 0) must already exclude the session — the
        // sidecar file, not in-memory state, is the source of truth.
        let dyn_root2: Arc<dyn SessionRoot> = root;
        let svc2 = SweepService::new(central, dyn_root2);
        let report = svc2.run_once().unwrap();
        assert!(report.integrity_excluded.contains(&session.id));
        assert!(!report.integrity_checked.contains(&session.id));
    }

    /// M21 O2: the central DB is `quick_check`ed at boot (pass 0) and then
    /// only once per day, not every pass.
    #[tokio::test]
    async fn central_integrity_runs_at_boot_then_daily() {
        let central = fresh_central();
        let t0 = chrono::Utc.with_ymd_and_hms(2026, 7, 17, 0, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(t0));
        let root = Arc::new(MemSessionRoot::new());
        let svc = SweepService::with_clock(central, root, clock.clone());

        // Pass 0 (boot): checked, healthy.
        let r = svc.run_once().unwrap();
        assert!(r.central_integrity_checked, "boot check runs");
        assert!(!r.central_integrity_corrupt);

        // A few minutes later: not due again.
        clock.advance(ChDuration::minutes(5));
        let r = svc.run_once().unwrap();
        assert!(!r.central_integrity_checked, "not due within a day");

        // A day later: due again.
        clock.advance(ChDuration::hours(24));
        let r = svc.run_once().unwrap();
        assert!(r.central_integrity_checked, "daily cadence re-fires");
    }

    /// M22 A4 acceptance (unit): the condition sampler populates ALL THREE
    /// observable signals — pending-inbound count, idle duration, and the set
    /// flag latches — from the central + per-session DBs, so every
    /// `ConditionKind` has real input to evaluate against.
    #[test]
    fn sample_condition_context_populates_all_three_kinds() {
        use crate::checks::condition_checkin::{Condition, ConditionKind};
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let sess = seed_running_session(&central);

        // pending_inbound: two pending inbound rows.
        crate::test_support::insert_inbound_message(&root, &sess);
        crate::test_support::insert_inbound_message(&root, &sess);
        // flags_set: one set latch for the session.
        copperclaw_db::tables::conditions::set_flag(
            &central,
            sess.agent_group_id,
            sess.id,
            "deploying",
            Utc::now(),
        )
        .unwrap();

        let now = chrono::Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        let svc = SweepService::new(central, root);
        // The sampler resolves the session's agent_group_id via a registered
        // condition, so register one for this session first.
        svc.condition_store().register(Condition {
            id: "c".into(),
            agent_group_id: sess.agent_group_id,
            session_id: sess.id,
            kind: ConditionKind::FlagSet {
                flag: "deploying".into(),
            },
            prompt: "x".into(),
        });
        // idle_secs: force last_active one hour before `now`.
        svc.central()
            .conn()
            .unwrap()
            .execute(
                "UPDATE sessions SET last_active = ?1 WHERE id = ?2",
                rusqlite::params![
                    (now - ChDuration::hours(1)).to_rfc3339(),
                    sess.id.as_uuid().to_string()
                ],
            )
            .unwrap();

        let ctx = svc.sample_condition_context(&sess.id, now);
        assert_eq!(ctx.pending_inbound, 2, "pending_inbound populated");
        assert_eq!(ctx.idle_secs, Some(3600), "idle_secs populated");
        assert_eq!(
            ctx.flags_set,
            vec!["deploying".to_string()],
            "flags_set populated"
        );
    }

    /// M22 A4 acceptance (integration): a durable idle condition persisted in
    /// the central `conditions` table fires a `kind:task` check-in wake once the
    /// session has been idle past its floor — driven entirely through
    /// `run_once` (reload → sample → rising-edge fire), reusing the sweep's
    /// test-clock seam for timing rather than any real wait.
    #[tokio::test]
    async fn run_once_fires_persisted_idle_condition_wake() {
        use copperclaw_db::tables::conditions::{self, NewCondition};
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let sess = seed_running_session(&central);
        // Materialise the inbound DB so the fire can write into it.
        let _ = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();

        // Register a durable idle condition: fire when idle >= 300s.
        conditions::upsert(
            &central,
            NewCondition {
                id: "idle-watchdog".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                kind: crate::checks::condition_checkin::KIND_IDLE.into(),
                threshold: Some(300),
                flag: None,
                prompt: "you've gone quiet — check in".into(),
                grant_id: None,
            },
        )
        .unwrap();

        // Clock: two hours after last_active (set at seed time) → well idle.
        let now = Utc::now() + ChDuration::hours(2);
        let clock = Arc::new(MockClock::new(now));
        let svc = SweepService::with_clock(central, root.clone(), clock);

        // First pass: reload from DB → rising edge → one fire.
        let report = svc.run_once().unwrap();
        assert_eq!(report.condition_checkins_fired.len(), 1, "idle wake fires");
        assert_eq!(
            report.condition_checkins_fired[0].series_id,
            "idle-watchdog"
        );
        let count: i64 = root
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap()
            .conn_mut()
            .query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "a wake inbound landed in the session");

        // Second pass: still idle (holding) → no re-fire (edge-triggered).
        let report = svc.run_once().unwrap();
        assert!(
            report.condition_checkins_fired.is_empty(),
            "holding condition fires once, not every pass",
        );
    }

    /// A soft-removed (deregistered) condition stops firing on the next pass —
    /// `run_once`'s reconcile drops it from the store.
    #[tokio::test]
    async fn run_once_stops_firing_after_deregister() {
        use copperclaw_db::tables::conditions::{self, NewCondition};
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let sess = seed_running_session(&central);
        let _ = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        conditions::set_flag(&central, sess.agent_group_id, sess.id, "go", Utc::now()).unwrap();
        conditions::upsert(
            &central,
            NewCondition {
                id: "flag-cond".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                kind: crate::checks::condition_checkin::KIND_FLAG.into(),
                threshold: None,
                flag: Some("go".into()),
                prompt: "flag raised".into(),
                grant_id: None,
            },
        )
        .unwrap();

        let clock = Arc::new(MockClock::new(Utc::now()));
        let svc = SweepService::with_clock(central.clone(), root, clock);
        assert_eq!(svc.run_once().unwrap().condition_checkins_fired.len(), 1);

        // Deregister → next pass reloads without it → clear the flag would also
        // stop it, but here we prove the reconcile-driven removal path.
        conditions::soft_remove(&central, "flag-cond", Utc::now()).unwrap();
        let report = svc.run_once().unwrap();
        assert!(report.condition_checkins_fired.is_empty());
        assert!(
            svc.condition_store().all().is_empty(),
            "reconcile dropped it"
        );
    }

    #[tokio::test]
    async fn run_loop_stops_when_cancelled() {
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let svc = Arc::new(SweepService::new(central, root));
        let token = CancellationToken::new();
        let task_token = token.clone();
        let h = tokio::spawn(async move { svc.run_loop(task_token).await });
        token.cancel();
        // Should finish quickly.
        tokio::time::timeout(std::time::Duration::from_secs(1), h)
            .await
            .expect("run_loop did not honor cancellation")
            .unwrap();
    }

    #[tokio::test]
    async fn run_once_continues_when_one_session_open_fails() {
        // Build a central with one session whose per-session pools cannot
        // be opened — every DB-touching check should log and swallow the
        // error rather than aborting the pass. The heartbeat check is the
        // only one that succeeds (a missing heartbeat counts as stale),
        // so the report contains exactly one heartbeat entry.
        let central = fresh_central();
        let session = seed_running_session(&central);
        let root = Arc::new(MemSessionRoot::new_strict_unknown());
        let svc = SweepService::new(central, root);
        let report = svc.run_once().unwrap();
        assert!(report.stuck_sessions.is_empty());
        assert!(report.processing_acks_reset.is_empty());
        assert!(report.recurrences_fired.is_empty());
        assert!(report.woken_sessions.is_empty());
        // Heartbeat is computed from a path lookup so it still works.
        assert_eq!(report.heartbeat_stale, vec![session.id]);
    }

    #[test]
    fn session_pool_exposes_conn_and_conn_mut() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let mut pool = SessionPool::new(conn);
        // Both accessors work.
        let _: &rusqlite::Connection = pool.conn();
        let _: &mut rusqlite::Connection = pool.conn_mut();
        // Debug impl exists.
        assert!(format!("{pool:?}").contains("SessionPool"));
        // into_conn yields the underlying handle.
        let _: rusqlite::Connection = pool.into_conn();
    }

    #[test]
    fn report_is_empty_and_total() {
        let mut r = SweepReport::default();
        assert!(r.is_empty());
        assert_eq!(r.total(), 0);
        r.stuck_sessions.push(SessionId::new());
        r.woken_sessions.push(SessionId::new());
        assert!(!r.is_empty());
        assert_eq!(r.total(), 2);
    }

    #[test]
    fn filesystem_session_root_returns_paths_under_data_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FilesystemSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let path = root.heartbeat_path(&ag, &sess);
        assert!(path.starts_with(tmp.path()));
        assert_eq!(path.file_name().unwrap(), ".heartbeat");
    }

    #[test]
    fn filesystem_session_root_opens_inbound_and_outbound() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FilesystemSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let _outbound = root.outbound_pool(&ag, &sess).unwrap();
        let _inbound = root.inbound_pool(&ag, &sess).unwrap();
    }

    #[tokio::test]
    async fn with_clock_uses_injected_clock() {
        let t = chrono::Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        let clock = Arc::new(MockClock::new(t));
        let central = fresh_central();
        let root = Arc::new(MemSessionRoot::new());
        let svc = SweepService::with_clock(central, root, clock);
        assert_eq!(svc.clock().now(), t);
        // Round-trip accessor coverage.
        let _ = svc.session_root();
        let _ = svc.central();
    }
}
