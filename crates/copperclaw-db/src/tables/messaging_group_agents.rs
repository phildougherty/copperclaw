//! CRUD for `messaging_group_agents` — the wiring table that connects a
//! messaging group to an agent group.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, EngageMode, MessagingGroupId, SessionMode, WiringId};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingGroupAgent {
    pub id: WiringId,
    pub messaging_group_id: MessagingGroupId,
    pub agent_group_id: AgentGroupId,
    pub engage_mode: EngageMode,
    pub engage_pattern: Option<String>,
    pub sender_scope: String,
    pub ignored_message_policy: String,
    pub session_mode: SessionMode,
    pub priority: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UpsertWiring {
    pub messaging_group_id: MessagingGroupId,
    pub agent_group_id: AgentGroupId,
    pub engage_mode: EngageMode,
    pub engage_pattern: Option<String>,
    pub sender_scope: String,
    pub ignored_message_policy: String,
    pub session_mode: SessionMode,
    pub priority: i32,
}

fn engage_mode_to_str(m: EngageMode) -> &'static str {
    match m {
        EngageMode::Pattern => "pattern",
        EngageMode::Mention => "mention",
        EngageMode::MentionSticky => "mention-sticky",
    }
}

fn parse_engage_mode(s: &str) -> rusqlite::Result<EngageMode> {
    match s {
        "pattern" => Ok(EngageMode::Pattern),
        "mention" => Ok(EngageMode::Mention),
        "mention-sticky" => Ok(EngageMode::MentionSticky),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid engage_mode: {other}"),
            )),
        )),
    }
}

fn session_mode_to_str(m: SessionMode) -> &'static str {
    match m {
        SessionMode::Shared => "shared",
        SessionMode::PerThread => "per-thread",
        SessionMode::AgentShared => "agent-shared",
    }
}

fn parse_session_mode(s: &str) -> rusqlite::Result<SessionMode> {
    match s {
        "shared" => Ok(SessionMode::Shared),
        "per-thread" => Ok(SessionMode::PerThread),
        "agent-shared" => Ok(SessionMode::AgentShared),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid session_mode: {other}"),
            )),
        )),
    }
}

fn row_to_wiring(row: &Row<'_>) -> rusqlite::Result<MessagingGroupAgent> {
    let id_str: String = row.get("id")?;
    let id = uuid::Uuid::parse_str(&id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let mg_str: String = row.get("messaging_group_id")?;
    let mg = uuid::Uuid::parse_str(&mg_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let ag_str: String = row.get("agent_group_id")?;
    let ag = uuid::Uuid::parse_str(&ag_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let engage_mode_str: String = row.get("engage_mode")?;
    let session_mode_str: String = row.get("session_mode")?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    let priority: i64 = row.get("priority")?;
    let priority = i32::try_from(priority).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, Box::new(e))
    })?;

    Ok(MessagingGroupAgent {
        id: WiringId(id),
        messaging_group_id: MessagingGroupId(mg),
        agent_group_id: AgentGroupId(ag),
        engage_mode: parse_engage_mode(&engage_mode_str)?,
        engage_pattern: row.get("engage_pattern")?,
        sender_scope: row.get("sender_scope")?,
        ignored_message_policy: row.get("ignored_message_policy")?,
        session_mode: parse_session_mode(&session_mode_str)?,
        priority,
        created_at,
    })
}

pub fn list_for_mg(db: &CentralDb, mg: MessagingGroupId) -> Result<Vec<MessagingGroupAgent>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, messaging_group_id, agent_group_id, engage_mode, engage_pattern,
                sender_scope, ignored_message_policy, session_mode, priority, created_at
         FROM messaging_group_agents
         WHERE messaging_group_id = ?1
         ORDER BY priority DESC, created_at",
    )?;
    let rows = stmt.query_map(params![mg.as_uuid().to_string()], row_to_wiring)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn list_for_ag(db: &CentralDb, ag: AgentGroupId) -> Result<Vec<MessagingGroupAgent>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, messaging_group_id, agent_group_id, engage_mode, engage_pattern,
                sender_scope, ignored_message_policy, session_mode, priority, created_at
         FROM messaging_group_agents
         WHERE agent_group_id = ?1
         ORDER BY priority DESC, created_at",
    )?;
    let rows = stmt.query_map(params![ag.as_uuid().to_string()], row_to_wiring)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(db: &CentralDb, id: WiringId) -> Result<MessagingGroupAgent, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT id, messaging_group_id, agent_group_id, engage_mode, engage_pattern,
                sender_scope, ignored_message_policy, session_mode, priority, created_at
         FROM messaging_group_agents WHERE id = ?1",
        params![id.as_uuid().to_string()],
        row_to_wiring,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

pub fn upsert(db: &CentralDb, req: UpsertWiring) -> Result<MessagingGroupAgent, DbError> {
    let conn = db.conn()?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM messaging_group_agents
             WHERE messaging_group_id = ?1 AND agent_group_id = ?2",
            params![
                req.messaging_group_id.as_uuid().to_string(),
                req.agent_group_id.as_uuid().to_string(),
            ],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(id_str) = existing {
        conn.execute(
            "UPDATE messaging_group_agents
             SET engage_mode = ?1, engage_pattern = ?2, sender_scope = ?3,
                 ignored_message_policy = ?4, session_mode = ?5, priority = ?6
             WHERE id = ?7",
            params![
                engage_mode_to_str(req.engage_mode),
                req.engage_pattern,
                req.sender_scope,
                req.ignored_message_policy,
                session_mode_to_str(req.session_mode),
                i64::from(req.priority),
                id_str,
            ],
        )?;
        let id = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| DbError::Invariant(format!("invalid uuid in messaging_group_agents.id: {e}")))?;
        drop(conn);
        return get(db, WiringId(id));
    }

    let id = WiringId::new();
    let now = Utc::now();
    conn.execute(
        "INSERT INTO messaging_group_agents
           (id, messaging_group_id, agent_group_id, engage_mode, engage_pattern,
            sender_scope, ignored_message_policy, session_mode, priority, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id.as_uuid().to_string(),
            req.messaging_group_id.as_uuid().to_string(),
            req.agent_group_id.as_uuid().to_string(),
            engage_mode_to_str(req.engage_mode),
            req.engage_pattern,
            req.sender_scope,
            req.ignored_message_policy,
            session_mode_to_str(req.session_mode),
            i64::from(req.priority),
            now.to_rfc3339(),
        ],
    )?;
    Ok(MessagingGroupAgent {
        id,
        messaging_group_id: req.messaging_group_id,
        agent_group_id: req.agent_group_id,
        engage_mode: req.engage_mode,
        engage_pattern: req.engage_pattern,
        sender_scope: req.sender_scope,
        ignored_message_policy: req.ignored_message_policy,
        session_mode: req.session_mode,
        priority: req.priority,
        created_at: now,
    })
}

pub fn delete(db: &CentralDb, id: WiringId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM messaging_group_agents WHERE id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use crate::tables::messaging_groups::{upsert as upsert_mg, UpsertMessagingGroup};
    use copperclaw_types::ChannelType;

    fn setup() -> (CentralDb, MessagingGroupId, AgentGroupId) {
        let db = CentralDb::open_in_memory().unwrap();
        let mg = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "p1".into(),
                name: Some("Group".into()),
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "greeter".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        (db, mg.id, ag.id)
    }

    fn sample(mg: MessagingGroupId, ag: AgentGroupId) -> UpsertWiring {
        UpsertWiring {
            messaging_group_id: mg,
            agent_group_id: ag,
            engage_mode: EngageMode::Mention,
            engage_pattern: None,
            sender_scope: "all".into(),
            ignored_message_policy: "drop".into(),
            session_mode: SessionMode::Shared,
            priority: 0,
        }
    }

    #[test]
    fn upsert_then_get() {
        let (db, mg, ag) = setup();
        let w = upsert(&db, sample(mg, ag)).unwrap();
        let fetched = get(&db, w.id).unwrap();
        assert_eq!(w, fetched);
        assert_eq!(fetched.engage_mode, EngageMode::Mention);
        assert_eq!(fetched.session_mode, SessionMode::Shared);
        assert_eq!(fetched.sender_scope, "all");
        assert_eq!(fetched.ignored_message_policy, "drop");
        assert_eq!(fetched.priority, 0);
    }

    #[test]
    fn upsert_updates_existing_row() {
        let (db, mg, ag) = setup();
        let first = upsert(&db, sample(mg, ag)).unwrap();
        let mut req = sample(mg, ag);
        req.engage_mode = EngageMode::Pattern;
        req.engage_pattern = Some("hello.*".into());
        req.session_mode = SessionMode::PerThread;
        req.sender_scope = "known".into();
        req.ignored_message_policy = "accumulate".into();
        req.priority = 5;
        let second = upsert(&db, req).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.engage_mode, EngageMode::Pattern);
        assert_eq!(second.engage_pattern.as_deref(), Some("hello.*"));
        assert_eq!(second.session_mode, SessionMode::PerThread);
        assert_eq!(second.sender_scope, "known");
        assert_eq!(second.ignored_message_policy, "accumulate");
        assert_eq!(second.priority, 5);
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = get(&db, WiringId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn list_for_mg_orders_by_priority_desc() {
        let (db, mg, ag) = setup();
        // Create a second agent group so we can wire two rows to the same mg.
        let ag2 = create_ag(
            &db,
            CreateAgentGroup {
                name: "second".into(),
                folder: "s".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let low = upsert(&db, sample(mg, ag)).unwrap();
        let mut high_req = sample(mg, ag2.id);
        high_req.priority = 10;
        let high = upsert(&db, high_req).unwrap();
        let rows = list_for_mg(&db, mg).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, high.id);
        assert_eq!(rows[1].id, low.id);
    }

    #[test]
    fn list_for_mg_empty() {
        let db = CentralDb::open_in_memory().unwrap();
        let rows = list_for_mg(&db, MessagingGroupId::new()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn list_for_ag_filters_to_agent() {
        let (db, mg, ag) = setup();
        // Add a second messaging group + wiring for the same agent.
        let mg2 = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("slack"),
                platform_id: "p2".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let _w1 = upsert(&db, sample(mg, ag)).unwrap();
        let _w2 = upsert(&db, sample(mg2.id, ag)).unwrap();
        // Wiring to a different agent should not appear.
        let other_ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "other".into(),
                folder: "o".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let _w3 = upsert(&db, sample(mg, other_ag.id)).unwrap();
        let rows = list_for_ag(&db, ag).unwrap();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.agent_group_id, ag);
        }
    }

    #[test]
    fn list_for_ag_empty() {
        let db = CentralDb::open_in_memory().unwrap();
        let rows = list_for_ag(&db, AgentGroupId::new()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn delete_works() {
        let (db, mg, ag) = setup();
        let w = upsert(&db, sample(mg, ag)).unwrap();
        delete(&db, w.id).unwrap();
        assert!(matches!(get(&db, w.id).unwrap_err(), DbError::NotFound));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = delete(&db, WiringId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn engage_mode_round_trips_all_variants() {
        let (db, mg, ag) = setup();
        let modes = [EngageMode::Pattern, EngageMode::Mention, EngageMode::MentionSticky];
        for m in modes {
            let mut req = sample(mg, ag);
            req.engage_mode = m;
            let w = upsert(&db, req).unwrap();
            let back = get(&db, w.id).unwrap();
            assert_eq!(back.engage_mode, m);
        }
    }

    #[test]
    fn session_mode_round_trips_all_variants() {
        let (db, mg, ag) = setup();
        let modes = [SessionMode::Shared, SessionMode::PerThread, SessionMode::AgentShared];
        for m in modes {
            let mut req = sample(mg, ag);
            req.session_mode = m;
            let w = upsert(&db, req).unwrap();
            let back = get(&db, w.id).unwrap();
            assert_eq!(back.session_mode, m);
        }
    }

    #[test]
    fn invalid_engage_mode_in_db_surfaces_as_sqlite_error() {
        let (db, mg, ag) = setup();
        let conn = db.conn().unwrap();
        let id = WiringId::new();
        conn.execute(
            "INSERT INTO messaging_group_agents
               (id, messaging_group_id, agent_group_id, engage_mode,
                engage_pattern, sender_scope, ignored_message_policy,
                session_mode, priority, created_at)
             VALUES (?1, ?2, ?3, 'bogus', NULL, 'all', 'drop', 'shared', 0, ?4)",
            params![
                id.as_uuid().to_string(),
                mg.as_uuid().to_string(),
                ag.as_uuid().to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .unwrap();
        drop(conn);
        let err = get(&db, id).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn invalid_session_mode_in_db_surfaces_as_sqlite_error() {
        let (db, mg, ag) = setup();
        let conn = db.conn().unwrap();
        let id = WiringId::new();
        conn.execute(
            "INSERT INTO messaging_group_agents
               (id, messaging_group_id, agent_group_id, engage_mode,
                engage_pattern, sender_scope, ignored_message_policy,
                session_mode, priority, created_at)
             VALUES (?1, ?2, ?3, 'mention', NULL, 'all', 'drop', 'bogus', 0, ?4)",
            params![
                id.as_uuid().to_string(),
                mg.as_uuid().to_string(),
                ag.as_uuid().to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .unwrap();
        drop(conn);
        let err = get(&db, id).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
