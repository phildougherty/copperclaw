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
use copperclaw_types::AgentGroupId;
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq)]
pub struct GroupBudget {
    pub agent_group_id: AgentGroupId,
    pub daily_token_cap: Option<i64>,
    pub daily_cost_cap: Option<f64>,
    /// Max LLM calls in any trailing 60-second window. NULL = no cap.
    pub agent_turns_per_minute_cap: Option<i64>,
    /// Max LLM calls in any trailing 3600-second window. NULL = no cap.
    pub agent_turns_per_hour_cap: Option<i64>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpsertGroupBudget {
    pub agent_group_id: AgentGroupId,
    pub daily_token_cap: Option<i64>,
    pub daily_cost_cap: Option<f64>,
    /// Max LLM calls in any trailing 60-second window. NULL = no cap.
    pub agent_turns_per_minute_cap: Option<i64>,
    /// Max LLM calls in any trailing 3600-second window. NULL = no cap.
    pub agent_turns_per_hour_cap: Option<i64>,
}

pub fn get(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
) -> Result<Option<GroupBudget>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT agent_group_id, daily_token_cap, daily_cost_cap,
                    agent_turns_per_minute_cap, agent_turns_per_hour_cap,
                    updated_at
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
           (agent_group_id, daily_token_cap, daily_cost_cap,
            agent_turns_per_minute_cap, agent_turns_per_hour_cap,
            updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_group_id) DO UPDATE SET
           daily_token_cap             = excluded.daily_token_cap,
           daily_cost_cap              = excluded.daily_cost_cap,
           agent_turns_per_minute_cap  = excluded.agent_turns_per_minute_cap,
           agent_turns_per_hour_cap    = excluded.agent_turns_per_hour_cap,
           updated_at                  = excluded.updated_at",
        params![
            req.agent_group_id.as_uuid().to_string(),
            req.daily_token_cap,
            req.daily_cost_cap,
            req.agent_turns_per_minute_cap,
            req.agent_turns_per_hour_cap,
            now.to_rfc3339(),
        ],
    )?;
    Ok(GroupBudget {
        agent_group_id: req.agent_group_id,
        daily_token_cap: req.daily_token_cap,
        daily_cost_cap: req.daily_cost_cap,
        agent_turns_per_minute_cap: req.agent_turns_per_minute_cap,
        agent_turns_per_hour_cap: req.agent_turns_per_hour_cap,
        updated_at: now,
    })
}

pub fn list(db: &CentralDb) -> Result<Vec<GroupBudget>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id, daily_token_cap, daily_cost_cap,
                agent_turns_per_minute_cap, agent_turns_per_hour_cap,
                updated_at
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
    // Column indices: 0=agent_group_id, 1=daily_token_cap, 2=daily_cost_cap,
    //                 3=agent_turns_per_minute_cap, 4=agent_turns_per_hour_cap,
    //                 5=updated_at
    let ts_str: String = row.get(5)?;
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
        agent_turns_per_minute_cap: row.get(3)?,
        agent_turns_per_hour_cap: row.get(4)?,
        updated_at: ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_upsert(db: &CentralDb, ag: AgentGroupId) -> GroupBudget {
        upsert(
            db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: Some(10_000),
                daily_cost_cap: None,
                agent_turns_per_minute_cap: None,
                agent_turns_per_hour_cap: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn upsert_then_get_round_trip() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        basic_upsert(&db, ag);
        let got = get(&db, ag).unwrap().unwrap();
        assert_eq!(got.daily_token_cap, Some(10_000));
        assert_eq!(got.daily_cost_cap, None);
        assert_eq!(got.agent_turns_per_minute_cap, None);
        assert_eq!(got.agent_turns_per_hour_cap, None);
    }

    #[test]
    fn upsert_updates_existing_row() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        basic_upsert(&db, ag);
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: Some(2_000),
                daily_cost_cap: Some(1.5),
                agent_turns_per_minute_cap: Some(5),
                agent_turns_per_hour_cap: Some(60),
            },
        )
        .unwrap();
        let got = get(&db, ag).unwrap().unwrap();
        assert_eq!(got.daily_token_cap, Some(2_000));
        assert_eq!(got.daily_cost_cap, Some(1.5));
        assert_eq!(got.agent_turns_per_minute_cap, Some(5));
        assert_eq!(got.agent_turns_per_hour_cap, Some(60));
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
        basic_upsert(&db, a);
        std::thread::sleep(std::time::Duration::from_millis(10));
        basic_upsert(&db, b);
        let rows = list(&db).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent_group_id, b);
        assert_eq!(rows[1].agent_group_id, a);
    }

    #[test]
    fn rate_limit_caps_round_trip() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: None,
                daily_cost_cap: None,
                agent_turns_per_minute_cap: Some(10),
                agent_turns_per_hour_cap: Some(120),
            },
        )
        .unwrap();
        let got = get(&db, ag).unwrap().unwrap();
        assert_eq!(got.agent_turns_per_minute_cap, Some(10));
        assert_eq!(got.agent_turns_per_hour_cap, Some(120));
        assert_eq!(got.daily_token_cap, None);
    }

    #[test]
    fn rate_limit_caps_can_be_cleared() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: None,
                daily_cost_cap: None,
                agent_turns_per_minute_cap: Some(5),
                agent_turns_per_hour_cap: Some(50),
            },
        )
        .unwrap();
        upsert(
            &db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: None,
                daily_cost_cap: None,
                agent_turns_per_minute_cap: None,
                agent_turns_per_hour_cap: None,
            },
        )
        .unwrap();
        let got = get(&db, ag).unwrap().unwrap();
        assert_eq!(got.agent_turns_per_minute_cap, None);
        assert_eq!(got.agent_turns_per_hour_cap, None);
    }
}
