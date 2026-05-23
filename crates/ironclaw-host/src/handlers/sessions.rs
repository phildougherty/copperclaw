//! Handlers for `sessions.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::sessions;
use ironclaw_iclaw::ErrorPayload;
use ironclaw_types::{AgentGroupId, Session, SessionId};
use serde_json::{json, Value};

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
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_db::tables::sessions::{create as create_session, CreateSession, mark_container_running};

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
}
