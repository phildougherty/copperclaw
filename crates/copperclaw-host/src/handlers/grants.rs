//! Handlers for `grants.*` commands — the operator surface over the M22 A1
//! task capability grants (M24 S3).
//!
//! `grants.list` is the read side: every grant (any status) joined with its
//! owning task, optionally narrowed to one agent group. `grants.revoke` is the
//! operator kill switch the A1 security verdict depends on: it flips the
//! central `task_grants` row to `revoked` (so `effective_grant` reads the task
//! inert immediately) and then refreshes the session's `grant.json` snapshot
//! in the same call, so the runner's autonomy gate input is withdrawn without
//! waiting for the next container-manager tick.
//!
//! ## When a revocation takes effect
//!
//! - **Central DB: immediately.** `task_grants::revoke` commits before the
//!   handler returns; every subsequent `effective_grant` read (grant snapshot
//!   writes at spawn and each manager tick, sweep budget gates) sees the grant
//!   as inert.
//! - **Runner gate: no later than the next fire attempt.** The handler
//!   eagerly removes the session's `grant.json` (via
//!   [`crate::container_manager::tasks_snapshot::write_grant_snapshot`], which
//!   fails closed), so the next autonomous turn's `load_turn_grant` finds no
//!   grant and stays in read-then-propose. Even if the eager refresh cannot
//!   run (session dir gone), the snapshot regenerates from the revoked row at
//!   the next container spawn and on every manager tick for running sessions.
//! - **A turn already in flight** loaded its `TurnGrant` at turn start and may
//!   finish the action it already authorized; its fire/token consumption still
//!   debits the (now revoked) row and can never re-open the gate.

use super::{db_err, opt_str, parse_uuid, req_str};
use crate::container_manager::tasks_snapshot::write_grant_snapshot;
use chrono::Utc;
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::SessionPaths;
use copperclaw_db::tables::task_grants::{self, GrantStatus, TaskGrant};
use copperclaw_db::tables::tasks;
use serde_json::{Value, json};
use std::path::Path;

/// Serialize one grant row. `status` is the *effective* liveness at `now`
/// (`live|revoked|expired|exhausted`); the raw stored status is recoverable
/// from `revoked_at` (set iff stored-revoked).
fn grant_to_json(g: &TaskGrant, now: chrono::DateTime<chrono::Utc>) -> Value {
    json!({
        "id": g.id,
        "task_id": g.task_id,
        "capability_scope": g.capability_scope,
        "token_budget": g.token_budget,
        "tokens_consumed": g.tokens_consumed,
        "max_fires": g.max_fires,
        "fires_consumed": g.fires_consumed,
        "expires_at": g.expires_at.map(|t| t.to_rfc3339()),
        "granted_by": g.granted_by,
        "status": g.effective_status(now),
        "approved_at": g.approved_at.map(|t| t.to_rfc3339()),
        "revoked_at": g.revoked_at.map(|t| t.to_rfc3339()),
        "created_at": g.created_at.to_rfc3339(),
    })
}

/// `grants.list` — every task capability grant joined with its owning task,
/// newest first. Optional `agent_group_id` narrows to one group.
pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    // Normalise an optional group filter through the uuid parser so both the
    // bare uuid and an `ag_`-prefixed form work (and garbage is rejected
    // rather than silently matching nothing).
    let group_filter = match opt_str(args, "agent_group_id") {
        Some(s) => Some(parse_uuid(&s)?.to_string()),
        None => None,
    };
    let now = Utc::now();
    let rows = task_grants::list_with_tasks(central, group_filter.as_deref()).map_err(db_err)?;
    Ok(json!(
        rows.iter()
            .map(|r| {
                let mut v = grant_to_json(&r.grant, now);
                let obj = v.as_object_mut().expect("grant_to_json is an object");
                obj.insert("task_name".into(), json!(r.task_name));
                obj.insert("agent_group_id".into(), json!(r.agent_group_id));
                obj.insert("session_id".into(), json!(r.session_id));
                v
            })
            .collect::<Vec<_>>()
    ))
}

/// `grants.revoke` — revoke one grant by id (host-only, audited via the
/// dispatch layer like every other mutation). See the module docs for the
/// exact effect timeline. Returns the revoked row plus `snapshot_refreshed`
/// (whether the owning session's `grant.json` was eagerly re-evaluated).
pub fn revoke(args: &Value, central: &CentralDb, data_dir: &Path) -> Result<Value, ErrorPayload> {
    let id = req_str(args, "id")?;
    let existing = task_grants::get(central, &id)
        .map_err(db_err)?
        .ok_or_else(|| ErrorPayload::new("not_found", format!("no grant `{id}`")))?;

    let now = Utc::now();
    task_grants::revoke(central, &id, now).map_err(db_err)?;
    // Count only genuine Approved -> Revoked transitions; re-revoking an
    // already-revoked grant re-stamps the timestamp but is not a new
    // lifecycle event.
    if existing.status == GrantStatus::Approved {
        copperclaw_metrics::inc_task_grant("revoked");
    }

    // Eagerly withdraw the runner-facing snapshot: the revoked row now reads
    // inert, so `write_grant_snapshot` removes any live `grant.json` (or
    // rewrites it from a newer grant, if one exists). Best-effort — a missing
    // session dir just means the spawn-time snapshot will regenerate from the
    // already-revoked row.
    let mut snapshot_refreshed = false;
    match tasks::get(central, &existing.task_id) {
        Ok(Some(task)) => {
            let paths = SessionPaths::new(data_dir, task.agent_group_id, task.session_id);
            if paths.root.is_dir() {
                write_grant_snapshot(central, &paths.root, now);
                snapshot_refreshed = true;
            }
        }
        Ok(None) => {}
        Err(err) => {
            // The revoke itself already committed; the snapshot self-heals on
            // the next tick/spawn, so a task lookup failure is non-fatal.
            tracing::warn!(?err, grant = %id, "grants.revoke: task lookup for snapshot refresh failed");
        }
    }

    let row = task_grants::get(central, &id)
        .map_err(db_err)?
        .ok_or_else(|| ErrorPayload::new("not_found", format!("no grant `{id}`")))?;
    let mut v = grant_to_json(&row, now);
    v.as_object_mut()
        .expect("grant_to_json is an object")
        .insert("snapshot_refreshed".into(), json!(snapshot_refreshed));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_manager::tasks_snapshot::GRANT_SNAPSHOT_FILENAME;
    use copperclaw_db::session::open_inbound;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messages_in::{self, WriteInbound};
    use copperclaw_db::tables::task_grants::NewTaskGrant;
    use copperclaw_db::tables::tasks::NewTask;
    use copperclaw_types::{AgentGroupId, MessageId, MessageKind, SessionId};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn seed_group(db: &CentralDb) -> AgentGroupId {
        // Unique folder per call — agent_groups.folder is UNIQUE.
        let tag = uuid::Uuid::now_v7().to_string();
        create_ag(
            db,
            CreateAgentGroup {
                name: format!("g-{tag}"),
                folder: format!("g-{tag}"),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    fn seed_task(db: &CentralDb, ag: AgentGroupId, session: SessionId, task_id: &str) {
        tasks::insert(
            db,
            NewTask {
                id: task_id.into(),
                agent_group_id: ag,
                session_id: session,
                name: Some("standup".into()),
                prompt: "post standup".into(),
                when_spec: "daily at 09:00".into(),
                recurrence: Some("0 9 * * *".into()),
                next_fire: Some(Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .unwrap();
    }

    fn seed_grant(db: &CentralDb, id: &str, task_id: &str) {
        task_grants::insert_approved(
            db,
            NewTaskGrant {
                id: id.into(),
                task_id: task_id.into(),
                capability_scope: "send_message:telegram".into(),
                token_budget: Some(1000),
                max_fires: Some(5),
                expires_at: None,
                granted_by: Some("operator".into()),
            },
        )
        .unwrap();
    }

    #[test]
    fn list_empty_is_empty_array() {
        let db = db();
        let v = list(&json!({}), &db).unwrap();
        assert_eq!(v, json!([]));
    }

    #[test]
    fn list_shows_grant_with_task_identity_and_live_status() {
        let db = db();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        seed_task(&db, ag, sess, "t-1");
        seed_grant(&db, "g-1", "t-1");

        let v = list(&json!({}), &db).unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "g-1");
        assert_eq!(rows[0]["task_id"], "t-1");
        assert_eq!(rows[0]["task_name"], "standup");
        assert_eq!(rows[0]["agent_group_id"], ag.as_uuid().to_string());
        assert_eq!(rows[0]["capability_scope"], "send_message:telegram");
        assert_eq!(rows[0]["token_budget"], 1000);
        assert_eq!(rows[0]["max_fires"], 5);
        assert_eq!(rows[0]["fires_consumed"], 0);
        assert_eq!(rows[0]["status"], "live");
    }

    #[test]
    fn list_filters_by_agent_group() {
        let db = db();
        let ag1 = seed_group(&db);
        let ag2 = seed_group(&db);
        seed_task(&db, ag1, SessionId::new(), "t-1");
        seed_task(&db, ag2, SessionId::new(), "t-2");
        seed_grant(&db, "g-1", "t-1");
        seed_grant(&db, "g-2", "t-2");

        let v = list(&json!({"agent_group_id": ag1.as_uuid().to_string()}), &db).unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "g-1");

        // Prefixed form works too.
        let v = list(
            &json!({"agent_group_id": format!("ag_{}", ag1.as_uuid())}),
            &db,
        )
        .unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn list_rejects_garbage_group_filter() {
        let db = db();
        let err = list(&json!({"agent_group_id": "not-a-uuid"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn revoke_flips_row_and_reads_inert() {
        let db = db();
        let ag = seed_group(&db);
        seed_task(&db, ag, SessionId::new(), "t-1");
        seed_grant(&db, "g-1", "t-1");
        let tmp = tempfile::tempdir().unwrap();

        let v = revoke(&json!({"id": "g-1"}), &db, tmp.path()).unwrap();
        assert_eq!(v["id"], "g-1");
        assert_eq!(v["status"], "revoked");
        assert!(v["revoked_at"].is_string());
        // No session dir under the temp data_dir → no eager snapshot refresh.
        assert_eq!(v["snapshot_refreshed"], false);

        // The A2 read contract sees the task as inert immediately.
        assert!(
            task_grants::effective_grant(&db, "t-1", Utc::now())
                .unwrap()
                .is_none()
        );
        // And the list surface reports it revoked.
        let rows = list(&json!({}), &db).unwrap();
        assert_eq!(rows.as_array().unwrap()[0]["status"], "revoked");
    }

    #[test]
    fn revoke_unknown_id_is_not_found() {
        let db = db();
        let tmp = tempfile::tempdir().unwrap();
        let err = revoke(&json!({"id": "ghost"}), &db, tmp.path()).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn revoke_missing_id_is_bad_request() {
        let db = db();
        let tmp = tempfile::tempdir().unwrap();
        let err = revoke(&json!({}), &db, tmp.path()).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn revoke_is_idempotent() {
        let db = db();
        let ag = seed_group(&db);
        seed_task(&db, ag, SessionId::new(), "t-1");
        seed_grant(&db, "g-1", "t-1");
        let tmp = tempfile::tempdir().unwrap();
        revoke(&json!({"id": "g-1"}), &db, tmp.path()).unwrap();
        let v = revoke(&json!({"id": "g-1"}), &db, tmp.path()).unwrap();
        assert_eq!(v["status"], "revoked");
    }

    /// The load-bearing S3 property: revoking through the handler withdraws
    /// the runner-facing `grant.json` in the same call, so the autonomy
    /// gate's input (the verdict path's `TurnGrant`) is gone before the next
    /// fire attempt.
    #[test]
    fn revoke_removes_grant_snapshot_for_pending_fire() {
        let db = db();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        seed_task(&db, ag, sess, "t-1");
        seed_grant(&db, "g-1", "t-1");

        // Real session layout under a data_dir, with a pending `kind:task`
        // inbound row so the snapshot writer resolves t-1 as the firing task.
        let data_dir = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(data_dir.path(), ag, sess);
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &WriteInbound {
                id: MessageId::new(),
                kind: MessageKind::Task,
                timestamp: Utc::now(),
                content: json!({ "task_id": "t-1", "text": "go" }),
                trigger: true,
                on_wake: true,
                process_after: None,
                recurrence: None,
                series_id: Some("t-1".into()),
                platform_id: None,
                channel_type: None,
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();

        // Live grant → snapshot written (the gate MAY open next fire).
        write_grant_snapshot(&db, &paths.root, Utc::now());
        let snap = paths.root.join(GRANT_SNAPSHOT_FILENAME);
        assert!(snap.exists());

        // Revoke through the handler → snapshot withdrawn in the same call.
        let v = revoke(&json!({"id": "g-1"}), &db, data_dir.path()).unwrap();
        assert_eq!(v["snapshot_refreshed"], true);
        assert!(!snap.exists());
    }
}
