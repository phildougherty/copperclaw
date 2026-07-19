//! Background reaper for orphaned `create_agent` child sessions.
//!
//! `create_agent` spawns a CHILD agent — its own `agent_group` + session —
//! to run a sub-task and report back to its parent. When a delegation is
//! botched, or the child finishes / errors and is then abandoned, that
//! child's group + session persists as an idle-but-warm, cost-burning
//! agent: a leaked container making provider calls with nobody reading the
//! output. One flaky delegation once leaked seven such orphans overnight.
//!
//! This module periodically finds children that are DONE-and-abandoned and
//! reaps them — deleting the session (central rows + on-disk `/data` tree)
//! and its `agent_group`, reclaiming the container / DB / disk.
//!
//! # Safety (this DELETES sessions)
//!
//! A wrong delete destroys a user's project, so the criteria are
//! deliberately conservative. A child is reaped only when **all** hold:
//!
//! 1. `source_session_id IS NOT NULL` — it is a `create_agent` child.
//!    **A top-level / user session (NULL `source_session_id`) is NEVER
//!    reaped. This is the paramount invariant** and is enforced in SQL by
//!    [`sessions::list_reapable_children`], which this loop is the only
//!    caller of.
//! 2. `container_status == Stopped` — never touch a running child (let it
//!    finish; a running container would also make `sessions::delete`
//!    refuse without `force`, and we never force).
//! 3. `last_active` older than the TTL grace ([`idle_secs`], default 900s)
//!    — a just-finished or briefly-paused child isn't reaped prematurely.
//! 4. No pending inbound — the child has nothing queued to process
//!    (checked here because it needs the per-session `inbound.db`).
//!
//! Everything is best-effort: a failure reaping one child logs a `warn!`
//! and moves to the next; the loop never panics.
//!
//! [`idle_secs`]: ChildReaper::idle_secs

use copperclaw_db::central::CentralDb;
use copperclaw_db::session::SessionPaths;
use copperclaw_db::tables::{agent_groups, sessions};
use copperclaw_types::{AgentGroupId, SessionId};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// How often the reaper scans for orphaned children.
pub const TICK_INTERVAL: Duration = Duration::from_secs(120);

/// Env var: master on/off switch. Reaper is ON by default; set to a
/// falsey value (`0` / `false` / `no` / `off`) to disable it entirely.
pub const ENABLE_ENV_VAR: &str = "COPPERCLAW_CHILD_REAP";

/// Env var: TTL grace in seconds. A child must be idle at least this long
/// before it is eligible. Default [`DEFAULT_IDLE_SECS`], floored at
/// [`MIN_IDLE_SECS`] so an operator can't set an aggressively short grace
/// that reaps children mid-handoff.
pub const IDLE_SECS_ENV_VAR: &str = "COPPERCLAW_CHILD_REAP_IDLE_SECS";

/// Default idle grace: 15 minutes.
pub const DEFAULT_IDLE_SECS: u64 = 900;

/// Lower clamp on the idle grace — never reap a child idle for less than
/// this, regardless of env override.
pub const MIN_IDLE_SECS: u64 = 60;

/// Periodic reaper of orphaned `create_agent` children. One per host.
pub struct ChildReaper {
    central: CentralDb,
    data_dir: PathBuf,
    idle_secs: u64,
    enabled: bool,
    interval: Duration,
}

impl ChildReaper {
    /// Build a reaper reading its config from the process environment
    /// (see [`ENABLE_ENV_VAR`] / [`IDLE_SECS_ENV_VAR`]).
    pub fn new(central: CentralDb, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            central,
            data_dir: data_dir.into(),
            idle_secs: idle_secs_from_env(),
            enabled: enabled_from_env(),
            interval: TICK_INTERVAL,
        }
    }

    /// The effective idle grace, in seconds.
    #[must_use]
    pub fn idle_secs(&self) -> u64 {
        self.idle_secs
    }

    /// Whether the reaper is enabled.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    #[must_use]
    fn with_config(mut self, enabled: bool, idle_secs: u64, interval: Duration) -> Self {
        self.enabled = enabled;
        self.idle_secs = idle_secs;
        self.interval = interval;
        self
    }

    /// One reap pass. Finds every reapable child, skips any with pending
    /// inbound work, and best-effort reaps the rest. Returns the number
    /// of children actually reaped this tick.
    pub fn tick(&self) -> usize {
        let idle_before =
            chrono::Utc::now() - chrono::Duration::seconds(idle_secs_i64(self.idle_secs));
        let candidates = match sessions::list_reapable_children(&self.central, idle_before) {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    ?err,
                    "child_reaper: list_reapable_children failed; skipping pass"
                );
                return 0;
            }
        };
        let mut reaped = 0usize;
        for child in candidates {
            // Pending-inbound guard: never drop queued work. A missing or
            // unreadable inbound.db is treated as "no pending" per design.
            if self.has_pending_inbound(child.session_id, child.agent_group_id) {
                debug!(
                    session = %child.session_id.as_uuid(),
                    "child_reaper: skipping child with pending inbound work",
                );
                continue;
            }
            if self.reap_child(child.session_id, child.agent_group_id) {
                reaped += 1;
            }
        }
        if reaped > 0 {
            // metric wish: increment a `copperclaw_child_reaper_reaped_total`
            // counter by `reaped` (recorded in copperclaw-metrics separately —
            // this crate does not touch the metrics hotspot).
            info!(
                reaped,
                "child_reaper: reclaimed orphaned create_agent children"
            );
        }
        reaped
    }

    /// Best-effort reap of one child: delete the session rows, remove the
    /// on-disk session dir, then delete the agent group. FK-safe order
    /// (session first, then group). Any single failure logs a `warn!` and
    /// returns `false` without touching the next child. Returns `true`
    /// only when the session-row delete succeeded (the disk + group steps
    /// are cleanup that never fails the reap).
    fn reap_child(&self, session_id: SessionId, agent_group_id: AgentGroupId) -> bool {
        // 1. Session rows (central DB). This is the load-bearing step.
        if let Err(err) = sessions::delete(&self.central, session_id) {
            warn!(
                ?err,
                session = %session_id.as_uuid(),
                "child_reaper: sessions::delete failed; leaving child in place",
            );
            return false;
        }
        // 2. On-disk session tree (best-effort — rows are already gone).
        remove_session_dir(&self.data_dir, agent_group_id, session_id);
        // 3. Agent group (best-effort — the session, which FK-referenced it
        //    via source/agent, is already gone).
        match agent_groups::delete(&self.central, agent_group_id) {
            // Deleted, or already gone (e.g. raced) — both are success.
            Ok(()) | Err(copperclaw_db::DbError::NotFound) => {}
            Err(err) => {
                warn!(
                    ?err,
                    agent_group = %agent_group_id.as_uuid(),
                    session = %session_id.as_uuid(),
                    "child_reaper: agent_groups::delete failed; session already reaped, \
                     group left behind (will retry next tick if still orphaned)",
                );
            }
        }
        info!(
            session = %session_id.as_uuid(),
            agent_group = %agent_group_id.as_uuid(),
            "child_reaper: reaped orphaned create_agent child",
        );
        true
    }

    /// Whether the child has any `status='pending'` row in its per-session
    /// `inbound.db`. A missing or unreadable DB is treated as **no pending**
    /// (so the child is still reapable) — matching the design note.
    fn has_pending_inbound(&self, session_id: SessionId, agent_group_id: AgentGroupId) -> bool {
        let paths = SessionPaths::new(&self.data_dir, agent_group_id, session_id);
        pending_inbound_count(&paths.inbound_db) > 0
    }

    /// Loop until shutdown. When the reaper is disabled it must still PARK
    /// on the shutdown token rather than return — this future is a
    /// supervised loop, and the supervisor treats any return while the host
    /// is running (`is_cancelled() == false`) as an unexpected exit and
    /// restart-storms it (surfacing as `child_reaper (dead)` in
    /// `cclaw doctor`). Parking makes a disabled loop a well-behaved idle
    /// task that exits only on real shutdown.
    pub async fn run_loop(self: Arc<Self>, shutdown: CancellationToken) {
        if !self.enabled {
            debug!("child_reaper: {ENABLE_ENV_VAR}=0 (disabled); idling until shutdown");
            shutdown.cancelled().await;
            return;
        }
        debug!(
            idle_secs = self.idle_secs,
            interval_secs = self.interval.as_secs(),
            "child_reaper: enabled",
        );
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.interval) => {
                    let _ = self.tick();
                }
            }
        }
    }
}

/// Count `status='pending'` rows in an `inbound.db`. Returns 0 when the
/// file is absent, can't be opened, or the query fails — the reaper reads
/// this as "no queued work", which is the conservative-for-cost but
/// design-mandated behaviour for an unreadable child DB.
fn pending_inbound_count(inbound_db: &Path) -> i64 {
    if !inbound_db.exists() {
        return 0;
    }
    let Some(conn) = open_inbound_readonly(inbound_db) else {
        return 0;
    };
    conn.query_row(
        "SELECT COUNT(*) FROM messages_in WHERE status = 'pending'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// Open an `inbound.db` strictly for reading. `inbound.db` is
/// `journal_mode=DELETE` (WAL is unsafe across the container bind-mount),
/// so a plain read-only handle is fine; fall back to read-write-no-create
/// defensively. No writes are ever issued.
fn open_inbound_readonly(path: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .or_else(|_| Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE))
        .ok()?;
    conn.execute_batch("PRAGMA mmap_size=0; PRAGMA busy_timeout=5000;")
        .ok()?;
    Some(conn)
}

/// Best-effort removal of the on-disk session tree. Mirrors the
/// `sessions.delete` handler: warn-logs a removal failure but never
/// propagates it — the central rows are already gone.
fn remove_session_dir(data_dir: &Path, agent: AgentGroupId, session: SessionId) {
    let paths = SessionPaths::new(data_dir, agent, session);
    if !paths.root.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&paths.root) {
        warn!(
            error = %e,
            path = %paths.root.display(),
            "child_reaper: failed to remove on-disk session directory; \
             central-DB rows are already gone",
        );
    }
}

fn enabled_from_env() -> bool {
    // Default ON. Only an explicit falsey value disables it.
    !matches!(
        std::env::var(ENABLE_ENV_VAR).ok().as_deref().map(str::trim),
        Some("0" | "false" | "no" | "off" | "FALSE" | "NO" | "OFF")
    )
}

fn idle_secs_from_env() -> u64 {
    std::env::var(IDLE_SECS_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_IDLE_SECS)
        .max(MIN_IDLE_SECS)
}

/// Saturating `u64 -> i64` for the chrono duration (idle grace is small in
/// practice; clamp defends against an absurd env override).
fn idle_secs_i64(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messages_in;
    use copperclaw_db::tables::sessions::{
        CreateSession, create as create_session, mark_container_stopped,
    };
    use copperclaw_types::{MessageId, MessageKind};
    use rusqlite::params;

    /// Build a reaper over an in-memory central DB + a temp data dir, with
    /// explicit config (bypassing env so tests are deterministic).
    fn reaper(central: CentralDb, data_dir: &Path) -> ChildReaper {
        ChildReaper::new(central, data_dir).with_config(true, 900, Duration::from_millis(10))
    }

    fn make_group(db: &CentralDb, folder: &str) -> AgentGroupId {
        create_ag(
            db,
            CreateAgentGroup {
                name: folder.into(),
                folder: folder.into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    /// Insert a stopped, long-idle child session (`source_session_id` set)
    /// under its own agent group. Materialises the on-disk session dir.
    fn seed_child(
        db: &CentralDb,
        data_dir: &Path,
        folder: &str,
        parent: SessionId,
    ) -> (SessionId, AgentGroupId) {
        let ag = make_group(db, folder);
        let s = create_session(
            db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: Some(parent),
            },
        )
        .unwrap();
        mark_container_stopped(db, s.id).unwrap();
        set_last_active_past(db, s.id);
        SessionPaths::new(data_dir, ag, s.id).ensure_dirs().unwrap();
        (s.id, ag)
    }

    fn set_last_active_past(db: &CentralDb, id: SessionId) {
        let conn = db.conn().unwrap();
        conn.execute(
            "UPDATE sessions SET last_active = ?1 WHERE id = ?2",
            params![
                (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
                id.as_uuid().to_string()
            ],
        )
        .unwrap();
    }

    fn write_pending_inbound(db_path: &Path) {
        // Open (create) a real inbound.db and insert one pending row.
        let paths_root = db_path.parent().unwrap();
        std::fs::create_dir_all(paths_root).unwrap();
        // Reuse the crate's inbound opener via a SessionPaths pointing here.
        // Simpler: open a fresh SessionPaths — but we only have the db path.
        // Open the DB directly through the db crate's helper by faking paths.
        let conn = copperclaw_db::session::open_inbound(&faux_paths(db_path)).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: MessageId::new(),
                kind: MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "queued work"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: None,
                channel_type: None,
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
    }

    /// `SessionPaths` whose `inbound_db` is exactly `db_path` (the rest of
    /// the fields are unused by `open_inbound`).
    fn faux_paths(db_path: &Path) -> SessionPaths {
        let root = db_path.parent().unwrap().to_path_buf();
        SessionPaths {
            inbound_db: db_path.to_path_buf(),
            outbound_db: root.join("outbound.db"),
            heartbeat: root.join(".heartbeat"),
            inbox: root.join("inbox"),
            outbox: root.join("outbox"),
            root,
        }
    }

    fn top_level(db: &CentralDb, ag: AgentGroupId) -> SessionId {
        create_session(
            db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn tick_reaps_child_rows_dir_and_group() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let parent_group = make_group(&db, "parent");
        let parent = top_level(&db, parent_group);
        let (child, child_group) = seed_child(&db, tmp.path(), "child", parent);
        let child_dir = SessionPaths::new(tmp.path(), child_group, child).root;
        assert!(child_dir.exists());

        let reaped = reaper(db.clone(), tmp.path()).tick();
        assert_eq!(reaped, 1);
        // Session row gone.
        assert!(matches!(
            sessions::get(&db, child),
            Err(copperclaw_db::DbError::NotFound)
        ));
        // On-disk dir gone.
        assert!(!child_dir.exists(), "child session dir should be removed");
        // Agent group gone.
        assert!(matches!(
            agent_groups::get(&db, child_group),
            Err(copperclaw_db::DbError::NotFound)
        ));
    }

    #[test]
    fn tick_never_reaps_top_level_session() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root_group = make_group(&db, "top");
        let root_session = top_level(&db, root_group);
        // Give the top-level session the exact same reap-tempting profile:
        // stopped + ancient last_active. It must survive.
        mark_container_stopped(&db, root_session).unwrap();
        set_last_active_past(&db, root_session);
        SessionPaths::new(tmp.path(), root_group, root_session)
            .ensure_dirs()
            .unwrap();
        let root_dir = SessionPaths::new(tmp.path(), root_group, root_session).root;

        let reaped = reaper(db.clone(), tmp.path()).tick();
        assert_eq!(reaped, 0, "no child present → nothing reaped");
        // Top-level session, its group, and its dir are ALL untouched.
        assert!(sessions::get(&db, root_session).is_ok());
        assert!(agent_groups::get(&db, root_group).is_ok());
        assert!(root_dir.exists());
    }

    #[test]
    fn tick_reaps_child_but_spares_top_level_with_same_idle_profile() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root_group = make_group(&db, "root_session");
        let root_session = top_level(&db, root_group);
        mark_container_stopped(&db, root_session).unwrap();
        set_last_active_past(&db, root_session);
        let (child, child_group) = seed_child(&db, tmp.path(), "child", root_session);

        let reaped = reaper(db.clone(), tmp.path()).tick();
        assert_eq!(reaped, 1);
        // Child gone.
        assert!(matches!(
            sessions::get(&db, child),
            Err(copperclaw_db::DbError::NotFound)
        ));
        assert!(matches!(
            agent_groups::get(&db, child_group),
            Err(copperclaw_db::DbError::NotFound)
        ));
        // Top-level parent untouched.
        assert!(sessions::get(&db, root_session).is_ok());
        assert!(agent_groups::get(&db, root_group).is_ok());
    }

    #[test]
    fn tick_skips_child_with_pending_inbound() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let parent_group = make_group(&db, "parent");
        let parent = top_level(&db, parent_group);

        // Child A has a pending inbound row → must be skipped.
        let (child_a, group_a) = seed_child(&db, tmp.path(), "child_a", parent);
        let inbound_a = SessionPaths::new(tmp.path(), group_a, child_a).inbound_db;
        write_pending_inbound(&inbound_a);

        // Child B has no pending work → must be reaped.
        let (child_b, group_b) = seed_child(&db, tmp.path(), "child_b", parent);

        let reaped = reaper(db.clone(), tmp.path()).tick();
        assert_eq!(reaped, 1, "only the child without pending work is reaped");
        // A survived.
        assert!(sessions::get(&db, child_a).is_ok());
        // B reaped.
        assert!(matches!(
            sessions::get(&db, child_b),
            Err(copperclaw_db::DbError::NotFound)
        ));
        assert!(matches!(
            agent_groups::get(&db, group_b),
            Err(copperclaw_db::DbError::NotFound)
        ));
    }

    #[test]
    fn pending_inbound_count_absent_db_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope").join("inbound.db");
        assert_eq!(pending_inbound_count(&missing), 0);
    }

    #[test]
    fn enabled_default_on_and_idle_clamps() {
        // These read the process env; with the vars unset we get defaults.
        // (Tests run without these vars set.)
        assert!(enabled_from_env());
        assert_eq!(idle_secs_from_env(), DEFAULT_IDLE_SECS);
    }

    /// A disabled reaper must PARK on the shutdown token, never return
    /// while the host runs — else the supervisor classifies the return as
    /// an unexpected exit and restart-storms it. Mirrors the todo_watcher
    /// regression guard.
    #[tokio::test]
    async fn disabled_reaper_parks_until_shutdown_instead_of_returning() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let reaper = Arc::new(ChildReaper::new(db, tmp.path()).with_config(
            false,
            900,
            Duration::from_millis(10),
        ));
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(Arc::clone(&reaper).run_loop(shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !handle.is_finished(),
            "disabled reaper returned instead of parking — supervisor will restart-storm it"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("reaper returns promptly on shutdown")
            .expect("run_loop task did not panic");
    }
}
