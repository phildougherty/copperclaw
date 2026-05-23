//! Handlers for `sessions.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use ironclaw_db::central::CentralDb;
use ironclaw_db::session::SessionPaths;
use ironclaw_db::tables::sessions;
use ironclaw_iclaw::ErrorPayload;
use ironclaw_types::{AgentGroupId, ContainerStatus, Session, SessionId};
use serde_json::{json, Value};
use std::path::Path;
use tracing::warn;

pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let status = opt_str(args, "status");
    let mut rows = match status.as_deref() {
        Some("running") => sessions::list_running(central).map_err(db_err)?,
        // Treat both "active" and missing as "list_active".
        _ => sessions::list_active(central).map_err(db_err)?,
    };
    if let Some(ag) = opt_str(args, "agent_group_id") {
        let ag_id = AgentGroupId(parse_uuid(&ag)?);
        rows.retain(|s| s.agent_group_id == ag_id);
    }
    Ok(json!(rows.iter().map(session_to_json).collect::<Vec<_>>()))
}

pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = SessionId(parse_uuid(&req_str(args, "id")?)?);
    let row = sessions::get(central, id).map_err(db_err)?;
    Ok(session_to_json(&row))
}

/// `sessions.delete` — drop the central DB row, cascade per-session
/// rows that don't have FK cascade today (`agent_turns` + `tasks`) and
/// rows the FK would otherwise refuse to leave behind
/// (`pending_questions`, `pending_approvals`), then rmtree the on-disk
/// session directory.
///
/// Refuses by default when the session's container is not Stopped so
/// the operator gets a chance to call `groups.restart` first; pass
/// `force: true` to delete anyway.
///
/// Filesystem removal is best-effort: a failure there logs a `warn!`
/// but doesn't fail the request, because the central-DB rows are
/// already gone and re-running the command would just `NotFound`.
pub fn delete(args: &Value, ctx: &crate::socket::HandlerCtx) -> Result<Value, ErrorPayload> {
    let id = SessionId(parse_uuid(&req_str(args, "id")?)?);
    let force = args
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let session = sessions::get(&ctx.central, id).map_err(db_err)?;
    if !matches!(session.container_status, ContainerStatus::Stopped) && !force {
        return Err(ErrorPayload::new(
            "container_not_stopped",
            format!(
                "session {} container_status is {:?}; restart the agent group first \
                 (`iclaw groups restart <id>`) or pass --force",
                id.as_uuid(),
                session.container_status
            ),
        ));
    }
    sessions::delete(&ctx.central, id).map_err(db_err)?;
    let removed_dir = remove_session_dir(&ctx.data_dir, session.agent_group_id, id);
    Ok(json!({
        "deleted": id.as_uuid().to_string(),
        "agent_group_id": session.agent_group_id.as_uuid().to_string(),
        "directory_removed": removed_dir,
    }))
}

/// Best-effort removal of the on-disk session tree. Returns the boolean
/// `true` if the directory existed and was removed, `false` if it was
/// missing or removal failed (warn-logged in the latter case). Never
/// fails the parent request.
fn remove_session_dir(
    data_dir: &Path,
    agent: AgentGroupId,
    session: SessionId,
) -> bool {
    let paths = SessionPaths::new(data_dir, agent, session);
    if !paths.root.exists() {
        return false;
    }
    match std::fs::remove_dir_all(&paths.root) {
        Ok(()) => true,
        Err(e) => {
            warn!(
                error = %e,
                path = %paths.root.display(),
                "sessions.delete: failed to remove on-disk session directory; \
                 central-DB row is already gone so the command still succeeded",
            );
            false
        }
    }
}

fn session_to_json(s: &Session) -> Value {
    json!({
        "id": s.id.as_uuid().to_string(),
        "agent_group_id": s.agent_group_id.as_uuid().to_string(),
        "messaging_group_id": s.messaging_group_id.map(|m| m.as_uuid().to_string()),
        "thread_id": s.thread_id,
        "agent_provider": s.agent_provider,
        "status": session_status_str(s.status),
        "container_status": s.container_status.as_str(),
        "last_active": s.last_active.to_rfc3339(),
        "created_at": s.created_at.to_rfc3339(),
    })
}

fn session_status_str(s: ironclaw_types::SessionStatus) -> &'static str {
    match s {
        ironclaw_types::SessionStatus::Active => "active",
        ironclaw_types::SessionStatus::Archived => "archived",
        ironclaw_types::SessionStatus::Stopped => "stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::HandlerCtx;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_db::tables::sessions::{create as create_session, CreateSession, mark_container_running};

    fn ctx_with(central: CentralDb, data_dir: std::path::PathBuf) -> HandlerCtx {
        HandlerCtx::with_data_dir(central, data_dir)
    }

    fn db_with_session() -> (CentralDb, SessionId, AgentGroupId) {
        let db = CentralDb::open_in_memory().unwrap();
        let g = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let s = create_session(
            &db,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        (db, s.id, g.id)
    }

    #[test]
    fn list_returns_active_sessions() {
        let (db, _s, _g) = db_with_session();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn list_filtered_by_agent_group() {
        let (db, _s, g) = db_with_session();
        let v = list(
            &json!({"agent_group_id": g.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        let other = AgentGroupId::new();
        let v = list(
            &json!({"agent_group_id": other.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_running_filter_returns_only_running() {
        let (db, s, _g) = db_with_session();
        let v = list(&json!({"status": "running"}), &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
        mark_container_running(&db, s).unwrap();
        let v = list(&json!({"status": "running"}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn get_by_id() {
        let (db, s, _g) = db_with_session();
        let v = get(&json!({"id": s.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["id"].as_str().unwrap(), s.as_uuid().to_string());
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = get(
            &json!({"id": uuid::Uuid::now_v7().to_string()}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn session_status_str_covers_variants() {
        assert_eq!(session_status_str(ironclaw_types::SessionStatus::Active), "active");
        assert_eq!(session_status_str(ironclaw_types::SessionStatus::Archived), "archived");
        assert_eq!(session_status_str(ironclaw_types::SessionStatus::Stopped), "stopped");
    }

    #[test]
    fn delete_removes_stopped_session_row() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = delete(
            &json!({"id": s.as_uuid().to_string()}),
            &ctx,
        )
        .unwrap();
        assert_eq!(v["deleted"], s.as_uuid().to_string());
        assert_eq!(v["agent_group_id"], ag.as_uuid().to_string());
        // Session is gone from the central DB.
        let err = get(
            &json!({"id": s.as_uuid().to_string()}),
            &ctx.central,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_refuses_running_session_without_force() {
        let (db, s, _ag) = db_with_session();
        mark_container_running(&db, s).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let err = delete(
            &json!({"id": s.as_uuid().to_string()}),
            &ctx,
        )
        .unwrap_err();
        assert_eq!(err.code, "container_not_stopped");
        // Session still present.
        assert!(get(
            &json!({"id": s.as_uuid().to_string()}),
            &ctx.central
        )
        .is_ok());
    }

    #[test]
    fn delete_running_session_with_force_succeeds() {
        let (db, s, _ag) = db_with_session();
        mark_container_running(&db, s).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        delete(
            &json!({"id": s.as_uuid().to_string(), "force": true}),
            &ctx,
        )
        .unwrap();
        let err = get(
            &json!({"id": s.as_uuid().to_string()}),
            &ctx.central,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_missing_session_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let err = delete(
            &json!({"id": SessionId::new().as_uuid().to_string()}),
            &ctx,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_removes_on_disk_session_directory() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        // Materialise the session dir before deletion.
        let paths = SessionPaths::new(tmp.path(), ag, s);
        paths.ensure_dirs().unwrap();
        assert!(paths.root.exists());
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = delete(
            &json!({"id": s.as_uuid().to_string()}),
            &ctx,
        )
        .unwrap();
        assert_eq!(v["directory_removed"], true);
        assert!(!paths.root.exists(), "session dir should be gone");
    }

    #[test]
    fn delete_succeeds_when_session_dir_missing() {
        let (db, s, _ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = delete(
            &json!({"id": s.as_uuid().to_string()}),
            &ctx,
        )
        .unwrap();
        // No directory pre-existed.
        assert_eq!(v["directory_removed"], false);
    }
}
