//! Append-only audit log of mutating cclaw socket calls.
//!
//! See `migrations/004_audit_log.sql` for the schema. The host's
//! dispatch layer calls [`insert`] inside the request → response hot
//! path; we keep the function simple (one INSERT, no batching) so the
//! audit hook adds at most one statement of latency per mutation.
//!
//! Read-only commands are deliberately not logged — there are 100x as
//! many of them as mutations and they have no security relevance.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension, Row};

/// One row inserted per mutating command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub ts: DateTime<Utc>,
    /// `"host"` or `"agent"`. Matches the wire-level [`Caller`] enum.
    pub caller_kind: String,
    pub caller_session: Option<String>,
    pub caller_agent_group: Option<String>,
    pub command: String,
    /// JSON. Long payloads are truncated at the call site.
    pub args: String,
    /// `"ok"` or `"error"`.
    pub result: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub latency_ms: i64,
}

/// Append an audit row. Errors propagate but the dispatch path treats
/// them as best-effort: a failure here must not block the request.
pub fn insert(db: &CentralDb, entry: &AuditEntry) -> Result<i64, DbError> {
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO audit_log
           (ts, caller_kind, caller_session, caller_agent_group,
            command, args, result, error_code, error_message, latency_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            entry.ts.to_rfc3339(),
            entry.caller_kind,
            entry.caller_session,
            entry.caller_agent_group,
            entry.command,
            entry.args,
            entry.result,
            entry.error_code,
            entry.error_message,
            entry.latency_ms,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// List the `limit` most recent audit entries newer than `since`.
/// Returns newest first.
pub fn list_recent(
    db: &CentralDb,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<AuditEntry>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT ts, caller_kind, caller_session, caller_agent_group,
                command, args, result, error_code, error_message, latency_ms
         FROM audit_log
         WHERE ts >= ?1
         ORDER BY id DESC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![since.to_rfc3339(), limit], row_to_entry)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Count the rows currently in `audit_log`. Used by `cclaw health`
/// and tests that don't want to scroll the full table.
pub fn count(db: &CentralDb) -> Result<i64, DbError> {
    let conn = db.conn()?;
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    Ok(n)
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<AuditEntry> {
    let ts_str: String = row.get(0)?;
    let ts = DateTime::parse_from_rfc3339(&ts_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?
        .with_timezone(&Utc);
    Ok(AuditEntry {
        ts,
        caller_kind: row.get(1)?,
        caller_session: row.get(2)?,
        caller_agent_group: row.get(3)?,
        command: row.get(4)?,
        args: row.get(5)?,
        result: row.get(6)?,
        error_code: row.get(7)?,
        error_message: row.get(8)?,
        latency_ms: row.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(command: &str) -> AuditEntry {
        AuditEntry {
            ts: Utc::now(),
            caller_kind: "host".into(),
            caller_session: None,
            caller_agent_group: None,
            command: command.into(),
            args: "{}".into(),
            result: "ok".into(),
            error_code: None,
            error_message: None,
            latency_ms: 1,
        }
    }

    #[test]
    fn insert_and_count_roundtrip() {
        let db = CentralDb::open_in_memory().unwrap();
        assert_eq!(count(&db).unwrap(), 0);
        insert(&db, &entry("groups.create")).unwrap();
        insert(&db, &entry("wirings.create")).unwrap();
        assert_eq!(count(&db).unwrap(), 2);
    }

    #[test]
    fn list_recent_returns_newest_first() {
        let db = CentralDb::open_in_memory().unwrap();
        let mut first = entry("groups.create");
        first.ts = Utc::now() - chrono::Duration::seconds(60);
        let mut second = entry("wirings.create");
        second.ts = Utc::now();
        insert(&db, &first).unwrap();
        insert(&db, &second).unwrap();
        let rows = list_recent(&db, Utc::now() - chrono::Duration::hours(1), 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].command, "wirings.create");
        assert_eq!(rows[1].command, "groups.create");
    }

    #[test]
    fn list_recent_filters_by_since() {
        let db = CentralDb::open_in_memory().unwrap();
        let mut old = entry("groups.create");
        old.ts = Utc::now() - chrono::Duration::hours(2);
        let mut recent = entry("wirings.create");
        recent.ts = Utc::now();
        insert(&db, &old).unwrap();
        insert(&db, &recent).unwrap();
        let rows = list_recent(&db, Utc::now() - chrono::Duration::hours(1), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].command, "wirings.create");
    }

    #[test]
    fn list_recent_respects_limit() {
        let db = CentralDb::open_in_memory().unwrap();
        for i in 0..5 {
            insert(&db, &entry(&format!("cmd.{i}"))).unwrap();
        }
        let rows = list_recent(&db, Utc::now() - chrono::Duration::hours(1), 3).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn agent_caller_fields_round_trip() {
        let db = CentralDb::open_in_memory().unwrap();
        let mut e = entry("destinations.add");
        e.caller_kind = "agent".into();
        e.caller_session = Some("sess-1".into());
        e.caller_agent_group = Some("ag-1".into());
        insert(&db, &e).unwrap();
        let rows = list_recent(&db, Utc::now() - chrono::Duration::hours(1), 10).unwrap();
        assert_eq!(rows[0].caller_kind, "agent");
        assert_eq!(rows[0].caller_session.as_deref(), Some("sess-1"));
        assert_eq!(rows[0].caller_agent_group.as_deref(), Some("ag-1"));
    }

    #[test]
    fn error_fields_round_trip() {
        let db = CentralDb::open_in_memory().unwrap();
        let mut e = entry("groups.create");
        e.result = "error".into();
        e.error_code = Some("validation".into());
        e.error_message = Some("folder required".into());
        insert(&db, &e).unwrap();
        let rows = list_recent(&db, Utc::now() - chrono::Duration::hours(1), 10).unwrap();
        assert_eq!(rows[0].result, "error");
        assert_eq!(rows[0].error_code.as_deref(), Some("validation"));
        assert_eq!(rows[0].error_message.as_deref(), Some("folder required"));
    }
}
