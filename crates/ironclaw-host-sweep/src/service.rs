//! [`SweepService`] orchestrates the five maintenance checks once per
//! [`crate::SWEEP_POLL_MS`] tick and produces a [`SweepReport`] describing
//! sessions that need host attention.

use crate::checks::{apology, heartbeat, processing, recurrence, stuck, wake};
use crate::checks::apology::ApologyEmit;
use crate::clock::{Clock, SystemClock};
use crate::error::SweepError;
use crate::spawn_tracker::SpawnAttemptTracker;
use chrono::{DateTime, Utc};
use ironclaw_db::central::CentralDb;
use ironclaw_types::{AgentGroupId, MessageId, SessionId};
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
}

/// Production [`SessionRoot`] backed by `ironclaw_db::session`. Each call
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
        let paths =
            ironclaw_db::session::SessionPaths::new(&self.data_root, *agent_group_id, *session_id);
        let conn = ironclaw_db::session::open_outbound(&paths)?;
        Ok(SessionPool::new(conn))
    }

    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, SweepError> {
        let paths =
            ironclaw_db::session::SessionPaths::new(&self.data_root, *agent_group_id, *session_id);
        let conn = ironclaw_db::session::open_inbound(&paths)?;
        Ok(SessionPool::new(conn))
    }

    fn heartbeat_path(&self, agent_group_id: &AgentGroupId, session_id: &SessionId) -> PathBuf {
        ironclaw_db::session::SessionPaths::new(&self.data_root, *agent_group_id, *session_id)
            .heartbeat
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
    pub recurrences_fired: Vec<SeriesFanout>,
    pub processing_acks_reset: Vec<MessageReset>,
    pub woken_sessions: Vec<SessionId>,
    pub heartbeat_stale: Vec<SessionId>,
    /// One entry per stuck-inbound apology written by the apology
    /// check during this pass. Empty in the common case where every
    /// session is healthy.
    pub apologies_emitted: Vec<ApologyEmit>,
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
    }

    /// Total number of items across all check categories.
    pub fn total(&self) -> usize {
        self.stuck_sessions.len()
            + self.recurrences_fired.len()
            + self.processing_acks_reset.len()
            + self.woken_sessions.len()
            + self.heartbeat_stale.len()
            + self.apologies_emitted.len()
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
}

impl SweepService {
    /// Build a service with the default [`SystemClock`].
    pub fn new(central: CentralDb, session_paths: Arc<dyn SessionRoot>) -> Self {
        Self {
            central,
            session_paths,
            clock: Arc::new(SystemClock),
            spawn_tracker: Arc::new(SpawnAttemptTracker::new()),
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

    /// Tick `run_once` every [`crate::SWEEP_POLL_MS`] until `shutdown` is
    /// cancelled. Errors are logged but do not abort the loop.
    pub async fn run_loop(self: Arc<Self>, shutdown: CancellationToken) {
        let interval = Duration::from_millis(crate::SWEEP_POLL_MS);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(interval) => {
                    match self.run_once() {
                        Ok(report) => {
                            if !report.is_empty() {
                                tracing::info!(
                                    target: "ironclaw_host_sweep",
                                    stuck = report.stuck_sessions.len(),
                                    recurrences = report.recurrences_fired.len(),
                                    acks_reset = report.processing_acks_reset.len(),
                                    woken = report.woken_sessions.len(),
                                    heartbeat_stale = report.heartbeat_stale.len(),
                                    apologies = report.apologies_emitted.len(),
                                    "sweep pass produced report",
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "ironclaw_host_sweep",
                                error = %e,
                                "sweep pass failed",
                            );
                        }
                    }
                }
            }
        }
    }

    /// Run a single sweep pass and return a populated [`SweepReport`].
    ///
    /// Errors during one session's check are logged and skipped — the pass
    /// continues with the next session — except for failures reading the
    /// central session list, which abort the pass with `Err`.
    pub fn run_once(&self) -> Result<SweepReport, SweepError> {
        let now = self.clock.now();
        let sessions = ironclaw_db::tables::sessions::list_active(&self.central)?;

        let mut report = SweepReport::default();

        for session in sessions {
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
                    target: "ironclaw_host_sweep",
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
                Ok(true) => report.stuck_sessions.push(session.id),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    target: "ironclaw_host_sweep",
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
                    target: "ironclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "processing-ack check failed",
                ),
            }

            match recurrence::check(
                self.session_paths.as_ref(),
                &session.agent_group_id,
                &session.id,
                now,
            ) {
                Ok(mut fan) => report.recurrences_fired.append(&mut fan),
                Err(e) => tracing::warn!(
                    target: "ironclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "recurrence-fanout check failed",
                ),
            }

            match wake::check(
                &self.central,
                self.session_paths.as_ref(),
                &session,
                now,
            ) {
                Ok(true) => report.woken_sessions.push(session.id),
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    target: "ironclaw_host_sweep",
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
            ) {
                Ok(mut emits) => report.apologies_emitted.append(&mut emits),
                Err(e) => tracing::warn!(
                    target: "ironclaw_host_sweep",
                    session = %session.id,
                    error = %e,
                    "stuck-inbound apology check failed",
                ),
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockClock;
    use crate::test_support::{
        seed_due_message, seed_recurrence, seed_running_session, seed_stale_heartbeat,
        seed_stuck_tool, seed_stuck_processing_ack, MemSessionRoot,
    };
    use chrono::{Duration as ChDuration, TimeZone};
    use ironclaw_db::tables::sessions as sessions_tbl;

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
        assert!(
            report.heartbeat_stale.contains(&stale_hb.id),
            "heartbeat branch",
        );
        assert_eq!(report.processing_acks_reset.len(), 1, "ack branch");
        assert_eq!(report.processing_acks_reset[0].session_id, ack.id);
        assert!(report.woken_sessions.contains(&due.id), "wake branch");
        assert_eq!(report.recurrences_fired.len(), 1, "recurrence branch");
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
        let svc = SweepService::with_clock(central, root, clock.clone());
        assert_eq!(svc.clock().now(), t);
        // Round-trip accessor coverage.
        let _ = svc.session_root();
        let _ = svc.central();
    }
}
