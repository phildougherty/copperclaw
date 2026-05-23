//! CRUD for `sessions`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, ContainerStatus, MessagingGroupId, Session, SessionId, SessionStatus};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, Default)]
pub struct CreateSession {
    pub agent_group_id: AgentGroupId,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub thread_id: Option<String>,
    pub agent_provider: Option<String>,
    /// Session id of the parent agent that spawned this one (via
    /// `create_agent`). `None` for root sessions kicked off by a real
    /// user channel.
    pub source_session_id: Option<SessionId>,
}

/// `SELECT` column list used by every `row_to_session` callsite. Kept
/// as a constant so the column order stays in sync across the half-
/// dozen reads in this file.
const SESSION_SELECT_COLS: &str =
    "id, agent_group_id, messaging_group_id, thread_id, agent_provider,
     status, container_status, last_active, created_at, source_session_id";

fn parse_status(s: &str) -> SessionStatus {
    match s {
        "archived" => SessionStatus::Archived,
        "stopped" => SessionStatus::Stopped,
        // "active" or unknown legacy value — default to Active.
        _ => SessionStatus::Active,
    }
}

fn parse_container_status(s: &str) -> ContainerStatus {
    match s {
        "running" => ContainerStatus::Running,
        "idle" => ContainerStatus::Idle,
        // "stopped" or unknown legacy value — default to Stopped.
        _ => ContainerStatus::Stopped,
    }
}

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<Session> {
    let id_str: String = row.get("id")?;
    let id = uuid::Uuid::parse_str(&id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let ag_str: String = row.get("agent_group_id")?;
    let ag = uuid::Uuid::parse_str(&ag_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let mg_opt: Option<String> = row.get("messaging_group_id")?;
    let mg = mg_opt
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .map(MessagingGroupId);

    let status: String = row.get("status")?;
    let container_status: String = row.get("container_status")?;
    let last_active: Option<String> = row.get("last_active")?;
    let created_at: String = row.get("created_at")?;
    let last_active_parsed = last_active
        .as_deref()
        .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .unwrap_or_else(Utc::now);
    let created_at_parsed = DateTime::parse_from_rfc3339(&created_at)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);

    let source_session_opt: Option<String> = row.get("source_session_id")?;
    let source_session_id = source_session_opt
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .map(SessionId);

    Ok(Session {
        id: SessionId(id),
        agent_group_id: AgentGroupId(ag),
        messaging_group_id: mg,
        thread_id: row.get("thread_id")?,
        agent_provider: row.get("agent_provider")?,
        status: parse_status(&status),
        container_status: parse_container_status(&container_status),
        last_active: last_active_parsed,
        created_at: created_at_parsed,
        source_session_id,
    })
}

pub fn create(db: &CentralDb, req: CreateSession) -> Result<Session, DbError> {
    let id = SessionId::new();
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO sessions
           (id, agent_group_id, messaging_group_id, thread_id, agent_provider,
            status, container_status, last_active, created_at, source_session_id)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', 'stopped', ?6, ?6, ?7)",
        params![
            id.as_uuid().to_string(),
            req.agent_group_id.as_uuid().to_string(),
            req.messaging_group_id.map(|m| m.as_uuid().to_string()),
            req.thread_id,
            req.agent_provider,
            now.to_rfc3339(),
            req.source_session_id.map(|s| s.as_uuid().to_string()),
        ],
    )?;
    Ok(Session {
        id,
        agent_group_id: req.agent_group_id,
        messaging_group_id: req.messaging_group_id,
        thread_id: req.thread_id,
        agent_provider: req.agent_provider,
        status: SessionStatus::Active,
        container_status: ContainerStatus::Stopped,
        last_active: now,
        created_at: now,
        source_session_id: req.source_session_id,
    })
}

pub fn get(db: &CentralDb, id: SessionId) -> Result<Session, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        &format!("SELECT {SESSION_SELECT_COLS} FROM sessions WHERE id = ?1"),
        params![id.as_uuid().to_string()],
        row_to_session,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

pub fn find_for_agent(
    db: &CentralDb,
    agent: AgentGroupId,
    mg: Option<MessagingGroupId>,
    thread: Option<&str>,
) -> Result<Option<Session>, DbError> {
    let conn = db.conn()?;
    let mg_str = mg.map(|m| m.as_uuid().to_string());

    // SQLite treats NULL != NULL, so we use IS comparison.
    let row = conn
        .query_row(
            &format!(
                "SELECT {SESSION_SELECT_COLS}
                 FROM sessions
                 WHERE agent_group_id = ?1
                   AND messaging_group_id IS ?2
                   AND thread_id IS ?3
                 ORDER BY created_at DESC
                 LIMIT 1"
            ),
            params![agent.as_uuid().to_string(), mg_str, thread],
            row_to_session,
        )
        .optional()?;
    Ok(row)
}

pub fn find_by_agent_group(db: &CentralDb, agent: AgentGroupId) -> Result<Option<Session>, DbError> {
    let conn = db.conn()?;
    let row = conn
        .query_row(
            &format!(
                "SELECT {SESSION_SELECT_COLS}
                 FROM sessions
                 WHERE agent_group_id = ?1
                 ORDER BY created_at DESC
                 LIMIT 1"
            ),
            params![agent.as_uuid().to_string()],
            row_to_session,
        )
        .optional()?;
    Ok(row)
}

pub fn mark_container_running(db: &CentralDb, id: SessionId) -> Result<(), DbError> {
    set_container_status(db, id, ContainerStatus::Running)
}

pub fn mark_container_idle(db: &CentralDb, id: SessionId) -> Result<(), DbError> {
    set_container_status(db, id, ContainerStatus::Idle)
}

pub fn mark_container_stopped(db: &CentralDb, id: SessionId) -> Result<(), DbError> {
    set_container_status(db, id, ContainerStatus::Stopped)
}

/// Set the session's lifecycle `status` (Active / Archived / Stopped).
/// Used by retire / cleanup flows, and by the `agent_dispatch` test
/// that pins the "don't dead-letter into archived parents" behaviour.
pub fn set_status(
    db: &CentralDb,
    id: SessionId,
    status: ironclaw_types::SessionStatus,
) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE sessions SET status = ?1 WHERE id = ?2",
        params![status.as_str(), id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

fn set_container_status(db: &CentralDb, id: SessionId, status: ContainerStatus) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE sessions SET container_status = ?1, last_active = ?2 WHERE id = ?3",
        params![status.as_str(), Utc::now().to_rfc3339(), id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn touch_last_active(db: &CentralDb, id: SessionId) -> Result<(), DbError> {
    let conn = db.conn()?;
    conn.execute(
        "UPDATE sessions SET last_active = ?1 WHERE id = ?2",
        params![Utc::now().to_rfc3339(), id.as_uuid().to_string()],
    )?;
    Ok(())
}

pub fn list_active(db: &CentralDb) -> Result<Vec<Session>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        &format!(
            "SELECT {SESSION_SELECT_COLS}
             FROM sessions
             WHERE status = 'active'
             ORDER BY last_active DESC"
        ),
    )?;
    let rows = stmt.query_map([], row_to_session)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn list_for_agent_group(
    db: &CentralDb,
    agent: AgentGroupId,
) -> Result<Vec<Session>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        &format!(
            "SELECT {SESSION_SELECT_COLS}
             FROM sessions
             WHERE agent_group_id = ?1
             ORDER BY last_active DESC"
        ),
    )?;
    let rows = stmt.query_map(
        params![agent.as_uuid().to_string()],
        row_to_session,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn list_running(db: &CentralDb) -> Result<Vec<Session>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        &format!(
            "SELECT {SESSION_SELECT_COLS}
             FROM sessions
             WHERE container_status = 'running'"
        ),
    )?;
    let rows = stmt.query_map([], row_to_session)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};

    fn db_with_agent() -> (CentralDb, AgentGroupId) {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "greeter".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        (db, ag.id)
    }

    #[test]
    fn create_then_get() {
        let (db, ag) = db_with_agent();
        let s = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: Some("claude".into()),
                source_session_id: None,
            },
        )
        .unwrap();
        let back = get(&db, s.id).unwrap();
        assert_eq!(s.id, back.id);
        assert_eq!(back.container_status, ContainerStatus::Stopped);
        assert_eq!(back.status, SessionStatus::Active);
    }

    #[test]
    fn find_for_agent_null_match() {
        let (db, ag) = db_with_agent();
        let s = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let found = find_for_agent(&db, ag, None, None).unwrap();
        assert_eq!(found.map(|f| f.id), Some(s.id));
    }

    #[test]
    fn find_for_agent_thread_match() {
        let (db, ag) = db_with_agent();
        let _a = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: Some("t1".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let b = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: Some("t2".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let found = find_for_agent(&db, ag, None, Some("t2")).unwrap();
        assert_eq!(found.unwrap().id, b.id);
    }

    #[test]
    fn mark_container_running_transitions() {
        let (db, ag) = db_with_agent();
        let s = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        mark_container_running(&db, s.id).unwrap();
        assert_eq!(get(&db, s.id).unwrap().container_status, ContainerStatus::Running);
        mark_container_idle(&db, s.id).unwrap();
        assert_eq!(get(&db, s.id).unwrap().container_status, ContainerStatus::Idle);
        mark_container_stopped(&db, s.id).unwrap();
        assert_eq!(get(&db, s.id).unwrap().container_status, ContainerStatus::Stopped);
    }

    #[test]
    fn list_running_filters() {
        let (db, ag) = db_with_agent();
        let a = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: Some("t1".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let _b = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: Some("t2".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        mark_container_running(&db, a.id).unwrap();
        let running = list_running(&db).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, a.id);
    }

    #[test]
    fn list_active_excludes_stopped() {
        let (db, ag) = db_with_agent();
        let s = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        // Status defaults to 'active' so this should appear.
        let active = list_active(&db).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, s.id);
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = get(&db, SessionId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn list_for_agent_group_returns_every_session_for_that_group() {
        let (db, ag1) = db_with_agent();
        let ag2 = create_ag(
            &db,
            CreateAgentGroup {
                name: "second".into(),
                folder: "g2".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id;
        let a = create(
            &db,
            CreateSession {
                agent_group_id: ag1,
                messaging_group_id: None,
                thread_id: Some("t1".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let b = create(
            &db,
            CreateSession {
                agent_group_id: ag1,
                messaging_group_id: None,
                thread_id: Some("t2".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let _ = create(
            &db,
            CreateSession {
                agent_group_id: ag2,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let mut ids: Vec<_> = list_for_agent_group(&db, ag1)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        ids.sort_by_key(|i| i.as_uuid().as_bytes().to_vec());
        let mut want = vec![a.id, b.id];
        want.sort_by_key(|i| i.as_uuid().as_bytes().to_vec());
        assert_eq!(ids, want);
    }

    #[test]
    fn list_for_agent_group_empty_when_no_sessions() {
        let (db, ag) = db_with_agent();
        let v = list_for_agent_group(&db, ag).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn touch_last_active_updates_time() {
        let (db, ag) = db_with_agent();
        let s = create(
            &db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let before = get(&db, s.id).unwrap().last_active;
        std::thread::sleep(std::time::Duration::from_millis(10));
        touch_last_active(&db, s.id).unwrap();
        let after = get(&db, s.id).unwrap().last_active;
        assert!(after > before, "{after} should be > {before}");
    }
}
