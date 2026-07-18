//! Snapshot the central DB's `tasks` table for a session into a JSON
//! file the runner can read from inside its container. The runner can't
//! reach the central DB directly (it lives outside the bind mount), so
//! this snapshot is the host-side half of the `list_tasks` MCP tool.

use chrono::{DateTime, Utc};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::task_grants;
use copperclaw_db::tables::tasks::{self, Task, TaskStatus};
use copperclaw_types::{MessageKind, SessionId};
use serde::Serialize;
use std::path::Path;
use tracing::warn;

/// Filename written into `<session_dir>/`. The runner's
/// `RunnerToolCtx::list_tasks` reads from this path inside the container
/// (`/data/tasks.json`).
pub const TASKS_SNAPSHOT_FILENAME: &str = "tasks.json";

/// Filename written into `<session_dir>/`. The runner's autonomy gate reads the
/// firing task's live capability grant from this path inside the container
/// (`/data/grant.json`) via `copperclaw_runner::run::load_turn_grant` (M22 A2).
///
/// The on-disk shape MUST stay byte-compatible with
/// `copperclaw_runner::run::tool_dispatch::TurnGrant` (see [`GrantSnapshotRow`]).
/// **Secure-by-default:** this file exists ONLY when the firing task has a live
/// `effective_grant`; on any other outcome (no firing task, no/revoked/expired/
/// exhausted grant, or a read error) it is removed so the runner's gate stays
/// CLOSED — the brake fails closed.
pub const GRANT_SNAPSHOT_FILENAME: &str = "grant.json";

/// On-disk row shape. Matches `copperclaw_mcp::context::TaskSummary` so the
/// runner can deserialize directly into that type. We don't import
/// `TaskSummary` here because the host crate doesn't depend on
/// `copperclaw-mcp` (same circular-dep rationale as `RunnerConfigForFile`).
#[derive(Debug, Serialize)]
struct TaskSnapshotRow {
    id: String,
    name: String,
    status: String,
    when: Option<chrono::DateTime<chrono::Utc>>,
    recurrence: Option<String>,
}

impl From<&Task> for TaskSnapshotRow {
    fn from(t: &Task) -> Self {
        Self {
            id: t.id.clone(),
            name: t.name.clone().unwrap_or_default(),
            status: status_str(t.status).to_string(),
            when: t.next_fire,
            recurrence: t.recurrence.clone(),
        }
    }
}

fn status_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Active => "active",
        TaskStatus::Paused => "paused",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Completed => "completed",
    }
}

/// Write the current set of tasks for `session_id` to
/// `<session_root>/tasks.json`. Errors are logged and dropped — a stale
/// or missing snapshot is much better than a failed spawn.
///
/// Also refreshes the autonomy grant snapshot (`grant.json`) for the same
/// session in the same breath — both are host→runner snapshots written at
/// exactly the moments the runner needs them fresh (container spawn via
/// `runner_config_for`, and the running-session refresh in the manager tick).
/// Folding the grant write in here is what makes the FIRST scheduled fire see
/// its grant at spawn time without the spawn caller needing to know about it.
pub fn write_tasks_snapshot(central: &CentralDb, session_id: SessionId, session_root: &Path) {
    // Refresh the grant snapshot first so a tasks-write failure (below) never
    // skips the autonomy gate's input.
    write_grant_snapshot(central, session_root, Utc::now());

    let rows: Vec<TaskSnapshotRow> = match tasks::list_for_session(central, session_id) {
        Ok(ts) => ts.iter().map(TaskSnapshotRow::from).collect(),
        Err(err) => {
            warn!(?err, session = %session_id.as_uuid(), "tasks_snapshot: list_for_session failed");
            return;
        }
    };
    let path = session_root.join(TASKS_SNAPSHOT_FILENAME);
    let bytes = match serde_json::to_vec_pretty(&rows) {
        Ok(b) => b,
        Err(err) => {
            warn!(?err, "tasks_snapshot: serialise failed");
            return;
        }
    };
    if let Err(err) = std::fs::write(&path, bytes) {
        warn!(?err, path = %path.display(), "tasks_snapshot: write failed");
    }
}

/// On-disk grant shape. MUST match `copperclaw_runner::run::tool_dispatch::TurnGrant`
/// field-for-field so the runner deserializes it directly. We don't import that
/// type because the host crate doesn't depend on `copperclaw-runner` (same
/// circular-dep rationale as [`TaskSnapshotRow`]). The `Option` fields are
/// omitted when `None` (the runner reads them as `#[serde(default)]`), so an
/// absent field reads as "unbounded / no expiry".
#[derive(Debug, Serialize)]
struct GrantSnapshotRow {
    grant_id: String,
    task_id: String,
    capability_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens_remaining: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fires_remaining: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<DateTime<Utc>>,
}

/// Write `<session_root>/grant.json` with the firing task's live capability
/// grant (M22 A2 companion writer), or REMOVE any stale snapshot when there is
/// nothing live to authorize.
///
/// The firing task is resolved exactly as the runner resolves it
/// (`copperclaw_runner::run::firing_task_id`): the newest pending `kind:task`
/// inbound row's `content.task_id`, falling back to its `series_id`. The runner
/// re-verifies that the snapshot's `task_id` matches its own firing task, so a
/// stale grant for a *different* task can never authorize a fire.
///
/// **Secure-by-default (rule 4):** a `grant.json` is written ONLY when
/// [`task_grants::effective_grant`] returns `Some` (approved, not revoked, not
/// expired, budget + fires remaining). No firing task, an inert grant, or a DB
/// read error all remove any existing snapshot and write nothing — the runner's
/// gate stays CLOSED (`load_turn_grant` returns `None` → read-then-propose).
pub fn write_grant_snapshot(central: &CentralDb, session_root: &Path, now: DateTime<Utc>) {
    let path = session_root.join(GRANT_SNAPSHOT_FILENAME);

    let Some(task_id) = firing_task_id_from_inbound(session_root) else {
        // No scheduled/autonomous fire pending → no grant to authorize.
        remove_stale_grant(&path);
        return;
    };

    let effective = match task_grants::effective_grant(central, &task_id, now) {
        Ok(e) => e,
        Err(err) => {
            // Fail closed: a read error means we cannot PROVE a live grant, so
            // remove any stale snapshot and leave the gate closed.
            warn!(?err, task_id = %task_id, "grant_snapshot: effective_grant read failed; failing closed");
            remove_stale_grant(&path);
            return;
        }
    };
    let Some(e) = effective else {
        // No live grant (none / revoked / expired / exhausted).
        remove_stale_grant(&path);
        return;
    };

    // Compute the derived budgets before moving the string fields out of `e`.
    let tokens_remaining = e.tokens_remaining();
    let fires_remaining = e.fires_remaining();
    let snap = GrantSnapshotRow {
        grant_id: e.id,
        task_id: e.task_id,
        capability_scope: e.capability_scope,
        tokens_remaining,
        fires_remaining,
        expires_at: e.expires_at,
    };
    let bytes = match serde_json::to_vec_pretty(&snap) {
        Ok(b) => b,
        Err(err) => {
            warn!(?err, "grant_snapshot: serialise failed");
            return;
        }
    };
    if let Err(err) = std::fs::write(&path, bytes) {
        warn!(?err, path = %path.display(), "grant_snapshot: write failed");
    }
}

/// Remove a stale `grant.json`. A missing file is the success case (the gate is
/// already closed); any other IO error is logged and dropped.
fn remove_stale_grant(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => warn!(?err, path = %path.display(), "grant_snapshot: stale removal failed"),
    }
}

/// The task id that fired this spawn's autonomous turn, read from the session's
/// pending inbound. Mirrors `copperclaw_runner::run::firing_task_id`: the first
/// pending `kind:task` row (which carries `content.task_id` and `series_id =
/// task id` — see the sweep's `checks/scheduling.rs`), preferring
/// `content.task_id` and falling back to `series_id`. `None` when there is no
/// inbound db, no pending task fire, or the read fails (all fail closed).
fn firing_task_id_from_inbound(session_root: &Path) -> Option<String> {
    use copperclaw_db::tables::messages_in;

    let inbound_db = session_root.join("inbound.db");
    if !inbound_db.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &inbound_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));

    // `first_poll = true` so the wake-only `kind:task` rows (on_wake = 1) are
    // included; a modest limit keeps this cheap while covering any realistic
    // pending batch.
    let rows = messages_in::get_pending(&conn, true, 64).ok()?;
    rows.iter()
        .find(|r| r.kind == MessageKind::Task)
        .and_then(|r| {
            r.content
                .get("task_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| r.series_id.clone())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::{
        agent_groups::{self, CreateAgentGroup},
        sessions::{self, CreateSession},
        tasks::{self as tasks_tbl, NewTask, TaskStatus},
    };
    use copperclaw_types::AgentGroupId;

    fn seed_agent_and_session(db: &CentralDb) -> (AgentGroupId, SessionId) {
        let ag = agent_groups::create(
            db,
            CreateAgentGroup {
                name: "test".into(),
                folder: "test".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let s = sessions::create(
            db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        (ag.id, s.id)
    }

    #[test]
    fn snapshot_writes_empty_array_for_session_with_no_tasks() {
        let db = CentralDb::open_in_memory().unwrap();
        let (_ag, session_id) = seed_agent_and_session(&db);
        let tmp = tempfile::tempdir().unwrap();
        write_tasks_snapshot(&db, session_id, tmp.path());
        let bytes = std::fs::read(tmp.path().join(TASKS_SNAPSHOT_FILENAME)).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, serde_json::json!([]));
    }

    #[test]
    fn snapshot_round_trips_active_task_with_when_and_recurrence() {
        let db = CentralDb::open_in_memory().unwrap();
        let (ag, session_id) = seed_agent_and_session(&db);
        let when = chrono::Utc::now() + chrono::Duration::hours(1);
        tasks_tbl::insert(
            &db,
            NewTask {
                id: "task_abc".into(),
                agent_group_id: ag,
                session_id,
                name: Some("digest".into()),
                prompt: "do stuff".into(),
                when_spec: when.to_rfc3339(),
                recurrence: Some("0 8 * * *".into()),
                next_fire: Some(when),
            },
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        write_tasks_snapshot(&db, session_id, tmp.path());
        let bytes = std::fs::read(tmp.path().join(TASKS_SNAPSHOT_FILENAME)).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "task_abc");
        assert_eq!(arr[0]["name"], "digest");
        assert_eq!(arr[0]["status"], "active");
        assert_eq!(arr[0]["recurrence"], "0 8 * * *");
        assert!(arr[0]["when"].is_string());
    }

    #[test]
    fn snapshot_status_str_covers_all_variants() {
        assert_eq!(status_str(TaskStatus::Active), "active");
        assert_eq!(status_str(TaskStatus::Paused), "paused");
        assert_eq!(status_str(TaskStatus::Cancelled), "cancelled");
        assert_eq!(status_str(TaskStatus::Completed), "completed");
    }

    // --- M22 A2H: grant.json writer -------------------------------------

    use copperclaw_db::session::{SessionPaths, open_inbound};
    use copperclaw_db::tables::messages_in::{self, WriteInbound};
    use copperclaw_db::tables::task_grants::{self as grants_tbl, NewTaskGrant};
    use copperclaw_types::MessageId;

    /// A `SessionPaths` rooted directly at `root` (rather than the deep
    /// `sessions/<ag>/<sess>` layout), so a test can put `inbound.db` at the
    /// exact `session_root` the writer reads.
    fn session_paths_at(root: &Path) -> SessionPaths {
        SessionPaths {
            inbound_db: root.join("inbound.db"),
            outbound_db: root.join("outbound.db"),
            heartbeat: root.join(".heartbeat"),
            inbox: root.join("inbox"),
            outbox: root.join("outbox"),
            root: root.to_path_buf(),
        }
    }

    fn seed_task(db: &CentralDb, ag: AgentGroupId, session_id: SessionId, task_id: &str) {
        tasks_tbl::insert(
            db,
            NewTask {
                id: task_id.into(),
                agent_group_id: ag,
                session_id,
                name: Some("digest".into()),
                prompt: "do stuff".into(),
                when_spec: chrono::Utc::now().to_rfc3339(),
                recurrence: None,
                next_fire: None,
            },
        )
        .unwrap();
    }

    /// Write a pending `kind:task` inbound row into `<root>/inbound.db` so the
    /// writer resolves `task_id` as the firing task (as the sweep's fan-out does).
    fn write_pending_task_inbound(root: &Path, task_id: &str) {
        let paths = session_paths_at(root);
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &WriteInbound {
                id: MessageId::new(),
                kind: MessageKind::Task,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({ "task_id": task_id, "text": "go" }),
                trigger: true,
                on_wake: true,
                process_after: None,
                recurrence: None,
                series_id: Some(task_id.to_string()),
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

    #[test]
    fn grant_snapshot_writes_live_grant_for_firing_task() {
        let db = CentralDb::open_in_memory().unwrap();
        let (ag, session_id) = seed_agent_and_session(&db);
        seed_task(&db, ag, session_id, "task_g");
        grants_tbl::insert_approved(
            &db,
            NewTaskGrant {
                id: "grant_1".into(),
                task_id: "task_g".into(),
                capability_scope: "web_fetch send_message:telegram".into(),
                token_budget: Some(1000),
                max_fires: Some(5),
                expires_at: None,
                granted_by: Some("op".into()),
            },
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        write_pending_task_inbound(tmp.path(), "task_g");

        write_grant_snapshot(&db, tmp.path(), chrono::Utc::now());

        let bytes = std::fs::read(tmp.path().join(GRANT_SNAPSHOT_FILENAME)).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["grant_id"], "grant_1");
        assert_eq!(parsed["task_id"], "task_g");
        assert_eq!(
            parsed["capability_scope"],
            "web_fetch send_message:telegram"
        );
        assert_eq!(parsed["tokens_remaining"], 1000);
        assert_eq!(parsed["fires_remaining"], 5);
        // `None` expiry is OMITTED (the runner reads it as `#[serde(default)]`).
        assert!(parsed.get("expires_at").is_none());
    }

    #[test]
    fn grant_snapshot_absent_when_task_has_no_grant() {
        // Secure-by-default: a firing task with NO grant must never produce a
        // grant.json, so the runner's autonomy gate stays closed.
        let db = CentralDb::open_in_memory().unwrap();
        let (ag, session_id) = seed_agent_and_session(&db);
        seed_task(&db, ag, session_id, "task_g");
        let tmp = tempfile::tempdir().unwrap();
        write_pending_task_inbound(tmp.path(), "task_g");

        write_grant_snapshot(&db, tmp.path(), chrono::Utc::now());

        assert!(!tmp.path().join(GRANT_SNAPSHOT_FILENAME).exists());
    }

    #[test]
    fn grant_snapshot_removed_when_grant_revoked() {
        let db = CentralDb::open_in_memory().unwrap();
        let (ag, session_id) = seed_agent_and_session(&db);
        seed_task(&db, ag, session_id, "task_g");
        grants_tbl::insert_approved(
            &db,
            NewTaskGrant {
                id: "grant_1".into(),
                task_id: "task_g".into(),
                capability_scope: "web_fetch".into(),
                token_budget: None,
                max_fires: None,
                expires_at: None,
                granted_by: None,
            },
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        write_pending_task_inbound(tmp.path(), "task_g");

        // Live grant → file written.
        write_grant_snapshot(&db, tmp.path(), chrono::Utc::now());
        assert!(tmp.path().join(GRANT_SNAPSHOT_FILENAME).exists());

        // Revoke → the next write must REMOVE the now-stale snapshot.
        grants_tbl::revoke(&db, "grant_1", chrono::Utc::now()).unwrap();
        write_grant_snapshot(&db, tmp.path(), chrono::Utc::now());
        assert!(!tmp.path().join(GRANT_SNAPSHOT_FILENAME).exists());
    }

    #[test]
    fn grant_snapshot_absent_when_fires_exhausted() {
        // Exhaustion reads inert (this is what the delivery `grant_consume`
        // handler produces once fires are spent): effective_grant → None → no
        // grant.json.
        let db = CentralDb::open_in_memory().unwrap();
        let (ag, session_id) = seed_agent_and_session(&db);
        seed_task(&db, ag, session_id, "task_g");
        grants_tbl::insert_approved(
            &db,
            NewTaskGrant {
                id: "grant_1".into(),
                task_id: "task_g".into(),
                capability_scope: "web_fetch".into(),
                token_budget: None,
                max_fires: Some(1),
                expires_at: None,
                granted_by: None,
            },
        )
        .unwrap();
        // Spend the one and only fire → grant reads inert.
        grants_tbl::consume_fire(&db, "grant_1", chrono::Utc::now()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        write_pending_task_inbound(tmp.path(), "task_g");

        write_grant_snapshot(&db, tmp.path(), chrono::Utc::now());
        assert!(!tmp.path().join(GRANT_SNAPSHOT_FILENAME).exists());
    }

    #[test]
    fn grant_snapshot_removed_when_no_firing_task() {
        // No pending task fire at all → any pre-existing (stale) grant.json is
        // removed so it can never authorize a non-autonomous or unrelated turn.
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(GRANT_SNAPSHOT_FILENAME),
            b"{\"stale\":true}",
        )
        .unwrap();

        write_grant_snapshot(&db, tmp.path(), chrono::Utc::now());
        assert!(!tmp.path().join(GRANT_SNAPSHOT_FILENAME).exists());
    }
}
