//! Snapshot the central DB's `tasks` table for a session into a JSON
//! file the runner can read from inside its container. The runner can't
//! reach the central DB directly (it lives outside the bind mount), so
//! this snapshot is the host-side half of the `list_tasks` MCP tool.

use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::tasks::{self, Task, TaskStatus};
use copperclaw_types::SessionId;
use serde::Serialize;
use std::path::Path;
use tracing::warn;

/// Filename written into `<session_dir>/`. The runner's
/// `RunnerToolCtx::list_tasks` reads from this path inside the container
/// (`/data/tasks.json`).
pub const TASKS_SNAPSHOT_FILENAME: &str = "tasks.json";

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
pub fn write_tasks_snapshot(central: &CentralDb, session_id: SessionId, session_root: &Path) {
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
}
