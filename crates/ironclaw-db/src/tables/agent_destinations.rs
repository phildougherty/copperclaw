//! CRUD for `agent_destinations`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, DestinationKind};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDestination {
    pub agent_group_id: AgentGroupId,
    pub local_name: String,
    pub target_type: DestinationKind,
    pub target_id: String,
    pub created_at: DateTime<Utc>,
}

fn destination_kind_as_str(kind: DestinationKind) -> &'static str {
    match kind {
        DestinationKind::Channel => "channel",
        DestinationKind::Agent => "agent",
    }
}

fn parse_destination_kind(s: &str) -> rusqlite::Result<DestinationKind> {
    match s {
        "channel" => Ok(DestinationKind::Channel),
        "agent" => Ok(DestinationKind::Agent),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown target_type {other}").into(),
        )),
    }
}

fn row_to_agent_destination(row: &Row<'_>) -> rusqlite::Result<AgentDestination> {
    let agent_group_id_str: String = row.get("agent_group_id")?;
    let agent_group_uuid = uuid::Uuid::parse_str(&agent_group_id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let target_type_str: String = row.get("target_type")?;
    let target_type = parse_destination_kind(&target_type_str)?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    Ok(AgentDestination {
        agent_group_id: AgentGroupId(agent_group_uuid),
        local_name: row.get("local_name")?,
        target_type,
        target_id: row.get("target_id")?,
        created_at,
    })
}

pub fn list(db: &CentralDb, agent_group_id: AgentGroupId) -> Result<Vec<AgentDestination>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id, local_name, target_type, target_id, created_at
         FROM agent_destinations
         WHERE agent_group_id = ?1
         ORDER BY local_name",
    )?;
    let rows = stmt.query_map(
        params![agent_group_id.as_uuid().to_string()],
        row_to_agent_destination,
    )?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

pub fn get(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    local_name: &str,
) -> Result<Option<AgentDestination>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT agent_group_id, local_name, target_type, target_id, created_at
             FROM agent_destinations
             WHERE agent_group_id = ?1 AND local_name = ?2",
            params![agent_group_id.as_uuid().to_string(), local_name],
            row_to_agent_destination,
        )
        .optional()?)
}

pub fn add(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    local_name: String,
    target_type: DestinationKind,
    target_id: String,
) -> Result<AgentDestination, DbError> {
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO agent_destinations
           (agent_group_id, local_name, target_type, target_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            agent_group_id.as_uuid().to_string(),
            local_name,
            destination_kind_as_str(target_type),
            target_id,
            now.to_rfc3339(),
        ],
    )?;
    Ok(AgentDestination {
        agent_group_id,
        local_name,
        target_type,
        target_id,
        created_at: now,
    })
}

pub fn remove(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    local_name: &str,
) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM agent_destinations
         WHERE agent_group_id = ?1 AND local_name = ?2",
        params![agent_group_id.as_uuid().to_string(), local_name],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn lookup_by_target(
    db: &CentralDb,
    target_type: DestinationKind,
    target_id: &str,
) -> Result<Vec<AgentDestination>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id, local_name, target_type, target_id, created_at
         FROM agent_destinations
         WHERE target_type = ?1 AND target_id = ?2
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map(
        params![destination_kind_as_str(target_type), target_id],
        row_to_agent_destination,
    )?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn make_agent_group(db: &CentralDb, folder: &str) -> AgentGroupId {
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

    #[test]
    fn add_then_get() {
        let db = db();
        let ag = make_agent_group(&db, "g1");
        let d = add(
            &db,
            ag,
            "buddy".into(),
            DestinationKind::Agent,
            "ag-other".into(),
        )
        .unwrap();
        let fetched = get(&db, ag, "buddy").unwrap();
        assert_eq!(fetched, Some(d));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = db();
        let ag = make_agent_group(&db, "g1");
        assert!(get(&db, ag, "absent").unwrap().is_none());
    }

    #[test]
    fn add_duplicate_local_name_fails() {
        let db = db();
        let ag = make_agent_group(&db, "g1");
        add(&db, ag, "n".into(), DestinationKind::Channel, "mg-1".into()).unwrap();
        let err = add(&db, ag, "n".into(), DestinationKind::Agent, "ag-2".into()).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn add_with_unknown_agent_group_fails_fk() {
        let db = db();
        let err = add(
            &db,
            AgentGroupId::new(),
            "n".into(),
            DestinationKind::Agent,
            "x".into(),
        )
        .unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn list_returns_only_rows_for_group() {
        let db = db();
        let a = make_agent_group(&db, "a");
        let b = make_agent_group(&db, "b");
        add(&db, a, "x".into(), DestinationKind::Channel, "mg1".into()).unwrap();
        add(&db, a, "y".into(), DestinationKind::Agent, "ag1".into()).unwrap();
        add(&db, b, "z".into(), DestinationKind::Channel, "mg2".into()).unwrap();
        let rows = list(&db, a).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].local_name, "x");
        assert_eq!(rows[1].local_name, "y");
    }

    #[test]
    fn list_empty_when_no_destinations() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        assert!(list(&db, ag).unwrap().is_empty());
    }

    #[test]
    fn remove_deletes_row() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        add(&db, ag, "n".into(), DestinationKind::Channel, "mg".into()).unwrap();
        remove(&db, ag, "n").unwrap();
        assert!(get(&db, ag, "n").unwrap().is_none());
    }

    #[test]
    fn remove_missing_is_not_found() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let err = remove(&db, ag, "absent").unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn lookup_by_target_matches_kind_and_id() {
        let db = db();
        let a = make_agent_group(&db, "a");
        let b = make_agent_group(&db, "b");
        add(&db, a, "x".into(), DestinationKind::Channel, "mg-1".into()).unwrap();
        add(&db, b, "y".into(), DestinationKind::Channel, "mg-1".into()).unwrap();
        add(&db, a, "z".into(), DestinationKind::Agent, "mg-1".into()).unwrap();
        let rows = lookup_by_target(&db, DestinationKind::Channel, "mg-1").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.target_type == DestinationKind::Channel));
        assert!(rows.iter().all(|r| r.target_id == "mg-1"));
    }

    #[test]
    fn lookup_by_target_empty_when_no_match() {
        let db = db();
        let rows = lookup_by_target(&db, DestinationKind::Agent, "nope").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn destination_kind_roundtrips_through_text() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        add(&db, ag, "c".into(), DestinationKind::Channel, "mg".into()).unwrap();
        add(&db, ag, "a".into(), DestinationKind::Agent, "ag".into()).unwrap();
        let fetched_c = get(&db, ag, "c").unwrap().unwrap();
        let fetched_a = get(&db, ag, "a").unwrap().unwrap();
        assert_eq!(fetched_c.target_type, DestinationKind::Channel);
        assert_eq!(fetched_a.target_type, DestinationKind::Agent);
    }

    #[test]
    fn unknown_target_type_in_db_errors() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let conn = db.conn().unwrap();
        conn.execute(
            "INSERT INTO agent_destinations
               (agent_group_id, local_name, target_type, target_id, created_at)
             VALUES (?1, ?2, 'bogus', 'x', ?3)",
            params![ag.as_uuid().to_string(), "n", Utc::now().to_rfc3339()],
        )
        .unwrap();
        drop(conn);
        let err = get(&db, ag, "n").unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
