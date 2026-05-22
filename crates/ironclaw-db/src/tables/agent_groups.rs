//! CRUD for `agent_groups`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::AgentGroupId;
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGroup {
    pub id: AgentGroupId,
    pub name: String,
    pub folder: String,
    pub agent_provider: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Persisted nesting depth for groups spawned via `create_agent`.
    /// 0 means "not a spawned subagent" (the root agent or any group
    /// created by setup / an operator).
    pub subagent_depth: u8,
}

#[derive(Debug, Clone, Default)]
pub struct CreateAgentGroup {
    pub name: String,
    pub folder: String,
    pub agent_provider: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateAgentGroup {
    pub name: Option<String>,
    pub agent_provider: Option<Option<String>>,
}

fn row_to_agent_group(row: &Row<'_>) -> rusqlite::Result<AgentGroup> {
    let id_str: String = row.get("id")?;
    let id = uuid::Uuid::parse_str(&id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    let depth_i64: i64 = row.get("subagent_depth")?;
    let subagent_depth = u8::try_from(depth_i64.clamp(0, i64::from(u8::MAX))).unwrap_or(0);
    Ok(AgentGroup {
        id: AgentGroupId(id),
        name: row.get("name")?,
        folder: row.get("folder")?,
        agent_provider: row.get("agent_provider")?,
        created_at,
        subagent_depth,
    })
}

pub fn create(db: &CentralDb, req: CreateAgentGroup) -> Result<AgentGroup, DbError> {
    let id = AgentGroupId::new();
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO agent_groups (id, name, folder, agent_provider, created_at, subagent_depth)
         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        params![
            id.as_uuid().to_string(),
            req.name,
            req.folder,
            req.agent_provider,
            now.to_rfc3339(),
        ],
    )?;
    Ok(AgentGroup {
        id,
        name: req.name,
        folder: req.folder,
        agent_provider: req.agent_provider,
        created_at: now,
        subagent_depth: 0,
    })
}

pub fn get(db: &CentralDb, id: AgentGroupId) -> Result<AgentGroup, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT id, name, folder, agent_provider, created_at, subagent_depth
         FROM agent_groups WHERE id = ?1",
        params![id.as_uuid().to_string()],
        row_to_agent_group,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

pub fn get_by_folder(db: &CentralDb, folder: &str) -> Result<Option<AgentGroup>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT id, name, folder, agent_provider, created_at, subagent_depth
             FROM agent_groups WHERE folder = ?1",
            params![folder],
            row_to_agent_group,
        )
        .optional()?)
}

pub fn list(db: &CentralDb) -> Result<Vec<AgentGroup>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, name, folder, agent_provider, created_at, subagent_depth
         FROM agent_groups
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], row_to_agent_group)?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

/// Set the persisted `subagent_depth` for an agent group. Called by
/// `create_agent` immediately after the row is inserted so the value
/// survives host restarts.
pub fn set_subagent_depth(db: &CentralDb, id: AgentGroupId, depth: u8) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE agent_groups SET subagent_depth = ?1 WHERE id = ?2",
        params![i64::from(depth), id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Fetch just the `subagent_depth` for an agent group. Returns
/// `Ok(None)` when the group doesn't exist (rather than `NotFound`) so
/// the depth-gate can treat a missing parent as "no recorded depth".
pub fn get_subagent_depth(db: &CentralDb, id: AgentGroupId) -> Result<Option<u8>, DbError> {
    let conn = db.conn()?;
    let row: Option<i64> = conn
        .query_row(
            "SELECT subagent_depth FROM agent_groups WHERE id = ?1",
            params![id.as_uuid().to_string()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.map(|n| u8::try_from(n.clamp(0, i64::from(u8::MAX))).unwrap_or(0)))
}

pub fn update(db: &CentralDb, id: AgentGroupId, patch: UpdateAgentGroup) -> Result<AgentGroup, DbError> {
    let conn = db.conn()?;
    if let Some(name) = patch.name {
        conn.execute(
            "UPDATE agent_groups SET name = ?1 WHERE id = ?2",
            params![name, id.as_uuid().to_string()],
        )?;
    }
    if let Some(provider) = patch.agent_provider {
        conn.execute(
            "UPDATE agent_groups SET agent_provider = ?1 WHERE id = ?2",
            params![provider, id.as_uuid().to_string()],
        )?;
    }
    drop(conn);
    get(db, id)
}

pub fn delete(db: &CentralDb, id: AgentGroupId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM agent_groups WHERE id = ?1",
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

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn create_then_get() {
        let db = db();
        let g = create(
            &db,
            CreateAgentGroup {
                name: "Greeter".into(),
                folder: "greeter".into(),
                agent_provider: Some("claude".into()),
            },
        )
        .unwrap();
        let fetched = get(&db, g.id).unwrap();
        assert_eq!(g, fetched);
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = db();
        let err = get(&db, AgentGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn get_by_folder_returns_option() {
        let db = db();
        assert!(get_by_folder(&db, "absent").unwrap().is_none());
        create(
            &db,
            CreateAgentGroup {
                name: "x".into(),
                folder: "present".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        assert!(get_by_folder(&db, "present").unwrap().is_some());
    }

    #[test]
    fn folder_is_unique() {
        let db = db();
        create(
            &db,
            CreateAgentGroup {
                name: "a".into(),
                folder: "f".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let err = create(
            &db,
            CreateAgentGroup {
                name: "b".into(),
                folder: "f".into(),
                agent_provider: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn list_is_ordered_by_created_at() {
        let db = db();
        for i in 0..3 {
            create(
                &db,
                CreateAgentGroup {
                    name: format!("agent-{i}"),
                    folder: format!("folder-{i}"),
                    agent_provider: None,
                },
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows = list(&db).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "agent-0");
        assert_eq!(rows[2].name, "agent-2");
    }

    #[test]
    fn update_replaces_name() {
        let db = db();
        let g = create(
            &db,
            CreateAgentGroup {
                name: "old".into(),
                folder: "f".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let updated = update(
            &db,
            g.id,
            UpdateAgentGroup {
                name: Some("new".into()),
                agent_provider: None,
            },
        )
        .unwrap();
        assert_eq!(updated.name, "new");
        assert_eq!(updated.agent_provider, None);
    }

    #[test]
    fn update_can_clear_provider() {
        let db = db();
        let g = create(
            &db,
            CreateAgentGroup {
                name: "x".into(),
                folder: "f".into(),
                agent_provider: Some("claude".into()),
            },
        )
        .unwrap();
        let updated = update(
            &db,
            g.id,
            UpdateAgentGroup {
                name: None,
                agent_provider: Some(None),
            },
        )
        .unwrap();
        assert_eq!(updated.agent_provider, None);
    }

    #[test]
    fn delete_works() {
        let db = db();
        let g = create(
            &db,
            CreateAgentGroup {
                name: "x".into(),
                folder: "f".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        delete(&db, g.id).unwrap();
        assert!(matches!(get(&db, g.id).unwrap_err(), DbError::NotFound));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let db = db();
        let err = delete(&db, AgentGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }
}
