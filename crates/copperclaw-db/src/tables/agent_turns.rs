//! One row per LLM call, with usage as reported by the provider.
//!
//! Insert is host-owned for the moment because the runner runs inside
//! the container and only sees its own session's inbound/outbound DBs.
//! To get usage out we route it via the outbound channel: the runner
//! writes a `MessageKind::System` row with `action="usage"` and
//! `payload={input_tokens, output_tokens, model}`, the host's delivery
//! loop intercepts it and calls [`insert`] before any external
//! adapter is dispatched. This keeps the per-session writer model
//! (host writes central, container writes its own session DB) intact.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};

/// One LLM call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTurn {
    pub id: i64,
    pub session_id: String,
    pub agent_group_id: String,
    pub seq: i64,
    pub model: String,
    pub provider: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub status: String,
    pub error: Option<String>,
}

/// Append a new turn row. Returns the assigned `id`.
pub fn insert(db: &CentralDb, turn: &NewAgentTurn) -> Result<i64, DbError> {
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO agent_turns
           (session_id, agent_group_id, seq, model, provider,
            input_tokens, output_tokens, started_at, ended_at,
            status, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            turn.session_id,
            turn.agent_group_id,
            turn.seq,
            turn.model,
            turn.provider,
            turn.input_tokens,
            turn.output_tokens,
            turn.started_at.to_rfc3339(),
            turn.ended_at.to_rfc3339(),
            turn.status,
            turn.error,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Insert-time payload (sans the auto-assigned id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgentTurn {
    pub session_id: String,
    pub agent_group_id: String,
    pub seq: i64,
    pub model: String,
    pub provider: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub status: String,
    pub error: Option<String>,
}

/// Per-group rollup over a time window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRollup {
    pub agent_group_id: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub first_at: DateTime<Utc>,
    pub last_at: DateTime<Utc>,
}

/// Sum tokens + count turns per `agent_group_id` since `since`.
pub fn rollup_since(db: &CentralDb, since: DateTime<Utc>) -> Result<Vec<UsageRollup>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id,
                COUNT(*) AS turns,
                COALESCE(SUM(input_tokens), 0)  AS input_tokens,
                COALESCE(SUM(output_tokens), 0) AS output_tokens,
                MIN(started_at) AS first_at,
                MAX(ended_at)   AS last_at
         FROM agent_turns
         WHERE ended_at >= ?1
         GROUP BY agent_group_id
         ORDER BY output_tokens DESC",
    )?;
    let rows = stmt
        .query_map(params![since.to_rfc3339()], row_to_rollup)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Per-(group, model, provider) rollup over a time window.
///
/// Finer-grained companion to [`UsageRollup`]: cost is priced per model,
/// so the usage handler needs the token sums split by which model (and
/// which provider) produced them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsageRollup {
    pub agent_group_id: String,
    pub model: String,
    pub provider: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Sum tokens + count turns per `(agent_group_id, model, provider)` since
/// `since`. Pure read — the `idx_agent_turns_group_ended` index covers the
/// scan; no migration required. Rows are ordered by group then descending
/// output tokens so each group's heaviest model comes first.
pub fn rollup_by_model_since(
    db: &CentralDb,
    since: DateTime<Utc>,
) -> Result<Vec<ModelUsageRollup>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id,
                model,
                provider,
                COUNT(*) AS turns,
                COALESCE(SUM(input_tokens), 0)  AS input_tokens,
                COALESCE(SUM(output_tokens), 0) AS output_tokens
         FROM agent_turns
         WHERE ended_at >= ?1
         GROUP BY agent_group_id, model, provider
         ORDER BY agent_group_id, output_tokens DESC",
    )?;
    let rows = stmt
        .query_map(params![since.to_rfc3339()], row_to_model_rollup)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Count LLM-call rows for `agent_group_id` since `since`.
/// Used by the container manager to enforce per-minute and per-hour
/// spawn-rate caps without pulling the full token history.
pub fn turns_since(
    db: &CentralDb,
    agent_group_id: &str,
    since: DateTime<Utc>,
) -> Result<i64, DbError> {
    let conn = db.conn()?;
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM agent_turns
             WHERE agent_group_id = ?1 AND ended_at >= ?2",
            params![agent_group_id, since.to_rfc3339()],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(n)
}

/// Sum input + output tokens for `agent_group_id` since `since`.
/// Used by the container manager to enforce daily budgets without
/// pulling the full per-row history.
pub fn tokens_since(
    db: &CentralDb,
    agent_group_id: &str,
    since: DateTime<Utc>,
) -> Result<i64, DbError> {
    let conn = db.conn()?;
    let n: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(input_tokens + output_tokens), 0)
             FROM agent_turns
             WHERE agent_group_id = ?1 AND ended_at >= ?2",
            params![agent_group_id, since.to_rfc3339()],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(n)
}

/// Fetch the most recent turns for a group whose `ended_at` is at or after
/// `since`, newest first, capped at `limit`. Drives the provider-resilience
/// health fold (M16 Phase 4): the host reads the runner's reported turn
/// outcomes (`provider`, `model`, `status`, `error`) and degrades / restores
/// the matching chain entry. Cheap — bounded by `limit`.
pub fn recent_for_group(
    db: &CentralDb,
    agent_group_id: &str,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<AgentTurn>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, session_id, agent_group_id, seq, model, provider,
                input_tokens, output_tokens, started_at, ended_at, status, error
         FROM agent_turns
         WHERE agent_group_id = ?1 AND ended_at >= ?2
         ORDER BY ended_at DESC, id DESC
         LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![agent_group_id, since.to_rfc3339(), limit],
            row_to_turn,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_turn(row: &Row<'_>) -> rusqlite::Result<AgentTurn> {
    let started_str: String = row.get("started_at")?;
    let ended_str: String = row.get("ended_at")?;
    Ok(AgentTurn {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        agent_group_id: row.get("agent_group_id")?,
        seq: row.get("seq")?,
        model: row.get("model")?,
        provider: row.get("provider")?,
        input_tokens: row.get("input_tokens")?,
        output_tokens: row.get("output_tokens")?,
        started_at: parse_ts(&started_str)?,
        ended_at: parse_ts(&ended_str)?,
        status: row.get("status")?,
        error: row.get("error")?,
    })
}

/// Count rows in the table. Cheap diagnostic.
pub fn count(db: &CentralDb) -> Result<i64, DbError> {
    let conn = db.conn()?;
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_turns", [], |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    Ok(n)
}

fn row_to_rollup(row: &Row<'_>) -> rusqlite::Result<UsageRollup> {
    let agent_group_id: String = row.get(0)?;
    let turns: i64 = row.get(1)?;
    let input_tokens: i64 = row.get(2)?;
    let output_tokens: i64 = row.get(3)?;
    let first_str: String = row.get(4)?;
    let last_str: String = row.get(5)?;
    let first_at = parse_ts(&first_str)?;
    let last_at = parse_ts(&last_str)?;
    Ok(UsageRollup {
        agent_group_id,
        turns,
        input_tokens,
        output_tokens,
        first_at,
        last_at,
    })
}

fn row_to_model_rollup(row: &Row<'_>) -> rusqlite::Result<ModelUsageRollup> {
    let agent_group_id: String = row.get(0)?;
    let model: String = row.get(1)?;
    let provider: String = row.get(2)?;
    let turns: i64 = row.get(3)?;
    let input_tokens: i64 = row.get(4)?;
    let output_tokens: i64 = row.get(5)?;
    Ok(ModelUsageRollup {
        agent_group_id,
        model,
        provider,
        turns,
        input_tokens,
        output_tokens,
    })
}

fn parse_ts(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(ag: &str, input: i64, output: i64) -> NewAgentTurn {
        let now = Utc::now();
        NewAgentTurn {
            session_id: "sess-1".into(),
            agent_group_id: ag.into(),
            seq: 1,
            model: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            input_tokens: input,
            output_tokens: output,
            started_at: now - chrono::Duration::seconds(1),
            ended_at: now,
            status: "ok".into(),
            error: None,
        }
    }

    #[test]
    fn insert_and_count_roundtrip() {
        let db = CentralDb::open_in_memory().unwrap();
        assert_eq!(count(&db).unwrap(), 0);
        insert(&db, &turn("ag-1", 10, 20)).unwrap();
        insert(&db, &turn("ag-1", 30, 40)).unwrap();
        assert_eq!(count(&db).unwrap(), 2);
    }

    #[test]
    fn rollup_sums_per_group() {
        let db = CentralDb::open_in_memory().unwrap();
        insert(&db, &turn("ag-1", 100, 200)).unwrap();
        insert(&db, &turn("ag-1", 50, 75)).unwrap();
        insert(&db, &turn("ag-2", 10, 1)).unwrap();
        let rollups = rollup_since(&db, Utc::now() - chrono::Duration::hours(1)).unwrap();
        // Ordered DESC by output_tokens
        assert_eq!(rollups[0].agent_group_id, "ag-1");
        assert_eq!(rollups[0].turns, 2);
        assert_eq!(rollups[0].input_tokens, 150);
        assert_eq!(rollups[0].output_tokens, 275);
        assert_eq!(rollups[1].agent_group_id, "ag-2");
    }

    #[test]
    fn rollup_filters_by_window() {
        let db = CentralDb::open_in_memory().unwrap();
        let mut old = turn("ag-old", 999, 999);
        old.started_at = Utc::now() - chrono::Duration::hours(48);
        old.ended_at = Utc::now() - chrono::Duration::hours(48);
        let recent = turn("ag-recent", 5, 5);
        insert(&db, &old).unwrap();
        insert(&db, &recent).unwrap();
        let rollups = rollup_since(&db, Utc::now() - chrono::Duration::hours(24)).unwrap();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].agent_group_id, "ag-recent");
    }

    fn turn_with_model(
        ag: &str,
        model: &str,
        provider: &str,
        input: i64,
        output: i64,
    ) -> NewAgentTurn {
        let mut t = turn(ag, input, output);
        t.model = model.into();
        t.provider = provider.into();
        t
    }

    #[test]
    fn rollup_by_model_splits_groups_by_model() {
        let db = CentralDb::open_in_memory().unwrap();
        // ag-1 uses two models; ag-2 uses one.
        insert(
            &db,
            &turn_with_model("ag-1", "claude-sonnet-4-6", "anthropic", 100, 200),
        )
        .unwrap();
        insert(
            &db,
            &turn_with_model("ag-1", "claude-sonnet-4-6", "anthropic", 50, 75),
        )
        .unwrap();
        insert(
            &db,
            &turn_with_model("ag-1", "claude-haiku-4-5", "anthropic", 10, 20),
        )
        .unwrap();
        insert(&db, &turn_with_model("ag-2", "qwen3.6:27b", "ollama", 7, 9)).unwrap();
        let rows = rollup_by_model_since(&db, Utc::now() - chrono::Duration::hours(1)).unwrap();
        assert_eq!(rows.len(), 3);
        // Ordered by group, then output_tokens DESC within the group.
        assert_eq!(rows[0].agent_group_id, "ag-1");
        assert_eq!(rows[0].model, "claude-sonnet-4-6");
        assert_eq!(rows[0].provider, "anthropic");
        assert_eq!(rows[0].turns, 2);
        assert_eq!(rows[0].input_tokens, 150);
        assert_eq!(rows[0].output_tokens, 275);
        assert_eq!(rows[1].agent_group_id, "ag-1");
        assert_eq!(rows[1].model, "claude-haiku-4-5");
        assert_eq!(rows[1].turns, 1);
        assert_eq!(rows[1].input_tokens, 10);
        assert_eq!(rows[1].output_tokens, 20);
        assert_eq!(rows[2].agent_group_id, "ag-2");
        assert_eq!(rows[2].model, "qwen3.6:27b");
        assert_eq!(rows[2].provider, "ollama");
        assert_eq!(rows[2].turns, 1);
    }

    #[test]
    fn rollup_by_model_filters_by_window() {
        let db = CentralDb::open_in_memory().unwrap();
        let mut old = turn_with_model("ag-1", "claude-sonnet-4-6", "anthropic", 999, 999);
        old.started_at = Utc::now() - chrono::Duration::hours(48);
        old.ended_at = Utc::now() - chrono::Duration::hours(48);
        insert(&db, &old).unwrap();
        insert(
            &db,
            &turn_with_model("ag-1", "claude-sonnet-4-6", "anthropic", 5, 5),
        )
        .unwrap();
        let rows = rollup_by_model_since(&db, Utc::now() - chrono::Duration::hours(24)).unwrap();
        assert_eq!(rows.len(), 1);
        // The old row's tokens must not leak into the recent bucket.
        assert_eq!(rows[0].input_tokens, 5);
        assert_eq!(rows[0].output_tokens, 5);
        assert_eq!(rows[0].turns, 1);
    }

    #[test]
    fn turns_since_counts_correctly() {
        let db = CentralDb::open_in_memory().unwrap();
        // Two recent turns for ag-1, one old turn.
        insert(&db, &turn("ag-1", 10, 20)).unwrap();
        insert(&db, &turn("ag-1", 30, 40)).unwrap();
        let mut old = turn("ag-1", 5, 5);
        old.started_at = Utc::now() - chrono::Duration::hours(2);
        old.ended_at = Utc::now() - chrono::Duration::hours(2);
        insert(&db, &old).unwrap();
        // With a 1-hour window only the two recent turns count.
        let count = turns_since(&db, "ag-1", Utc::now() - chrono::Duration::hours(1)).unwrap();
        assert_eq!(count, 2);
        // Other group not counted.
        insert(&db, &turn("ag-2", 1, 1)).unwrap();
        let count2 = turns_since(&db, "ag-2", Utc::now() - chrono::Duration::hours(1)).unwrap();
        assert_eq!(count2, 1);
    }

    #[test]
    fn error_field_round_trips() {
        let db = CentralDb::open_in_memory().unwrap();
        let mut t = turn("ag-err", 0, 0);
        t.status = "error".into();
        t.error = Some("rate limited".into());
        insert(&db, &t).unwrap();
        let rollups = rollup_since(&db, Utc::now() - chrono::Duration::hours(1)).unwrap();
        assert_eq!(rollups.len(), 1);
        // Errors still count as turns even if tokens are zero.
        assert_eq!(rollups[0].turns, 1);
    }
}
