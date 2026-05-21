//! Per-agent-group daily caps.
//!
//! One row per group with optional token / cost limits. The container
//! manager queries [`get`] before spawning and refuses to start a
//! container when today's usage exceeds the cap.
//
// `AgentGroupId` is `Copy`, so the by-value parameter style matches
// the rest of the table CRUD layer (`sessions::*`, `agent_groups::*`,
// etc.). Clippy's `needless_pass_by_value` lint would have us
// rewrite all of them; allowed module-wide instead.
#![allow(clippy::needless_pass_by_value)]

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::AgentGroupId;
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq)]
pub struct GroupBudget {
    pub agent_group_id: AgentGroupId,
    pub daily_token_cap: Option<i64>,
    pub daily_cost_cap: Option<f64>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpsertGroupBudget {
    pub agent_group_id: AgentGroupId,
    pub daily_token_cap: Option<i64>,
    pub daily_cost_cap: Option<f64>,
}

pub fn get(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
) -> Result<Option<GroupBudget>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT agent_group_id, daily_token_cap, daily_cost_cap, updated_at
             FROM group_budgets
             WHERE agent_group_id = ?1",
            params![agent_group_id.as_uuid().to_string()],
            row_to_budget,
        )
        .optional()?)
}

pub fn upsert(db: &CentralDb, req: UpsertGroupBudget) -> Result<GroupBudget, DbError> {
    let conn = db.conn()?;
    let now = Utc::now();
    conn.execute(
        "INSERT INTO group_budgets
           (agent_group_id, daily_token_cap, daily_cost_cap, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_group_id) DO UPDATE SET
           daily_token_cap = excluded.daily_token_cap,
           daily_cost_cap  = excluded.daily_cost_cap,
           updated_at      = excluded.updated_at",
        params![
            req.agent_group_id.as_uuid().to_string(),
            req.daily_token_cap,
            req.daily_cost_cap,
            now.to_rfc3339(),
        ],
    )?;
    Ok(GroupBudget {
        agent_group_id: req.agent_group_id,
        daily_token_cap: req.daily_token_cap,
        daily_cost_cap: req.daily_cost_cap,
        updated_at: now,
    })
}

pub fn list(db: &CentralDb) -> Result<Vec<GroupBudget>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id, daily_token_cap, daily_cost_cap, updated_at
         FROM group_budgets
         ORDER BY updated_at DESC",
    )?;
    let rows = stmt
        .query_map([], row_to_budget)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_budget(row: &Row<'_>) -> rusqlite::Result<GroupBudget> {
    let id_str: String = row.get(0)?;
    let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let ts_str: String = row.get(3)?;
    let ts = DateTime::parse_from_rfc3339(&ts_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?
        .with_timezone(&Utc);
    Ok(GroupBudget {
        agent_group_id: AgentGroupId(id),
        daily_token_cap: row.get(1)?,
        daily_cost_cap: row.get(2)?,
        updated_at: ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_get_round_trip() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: Some(10_000),
                daily_cost_cap: None,
            },
        )
        .unwrap();
        let got = get(&db, ag).unwrap().unwrap();
        assert_eq!(got.daily_token_cap, Some(10_000));
        assert_eq!(got.daily_cost_cap, None);
    }

    #[test]
    fn upsert_updates_existing_row() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: Some(1_000),
                daily_cost_cap: None,
            },
        )
        .unwrap();
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: Some(2_000),
                daily_cost_cap: Some(1.5),
            },
        )
        .unwrap();
        let got = get(&db, ag).unwrap().unwrap();
        assert_eq!(got.daily_token_cap, Some(2_000));
        assert_eq!(got.daily_cost_cap, Some(1.5));
    }

    #[test]
    fn missing_row_returns_none() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        assert!(get(&db, ag).unwrap().is_none());
    }

    #[test]
    fn list_returns_rows_newest_first() {
        let db = CentralDb::open_in_memory().unwrap();
        let a = AgentGroupId::new();
        let b = AgentGroupId::new();
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: a,
                daily_token_cap: Some(1),
                daily_cost_cap: None,
            },
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: b,
                daily_token_cap: Some(2),
                daily_cost_cap: None,
            },
        )
        .unwrap();
        let rows = list(&db).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent_group_id, b);
        assert_eq!(rows[1].agent_group_id, a);
    }
}
