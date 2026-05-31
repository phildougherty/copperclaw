//! Writes and reads against per-session `outbound.db::container_state`.
//!
//! Single-row table (`id = 1`). The container writes a snapshot of what it's
//! currently doing (active tool + declared timeout) so the host can detect
//! stuck sessions without parsing logs.

use crate::DbError;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerState {
    pub current_tool: Option<String>,
    pub tool_declared_timeout_ms: Option<i64>,
    pub tool_started_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Read the current state row, or `None` if it has never been written.
pub fn get(conn: &Connection) -> Result<Option<ContainerState>, DbError> {
    let row = conn
        .query_row(
            "SELECT current_tool, tool_declared_timeout_ms, tool_started_at, updated_at
             FROM container_state WHERE id = 1",
            [],
            row_to_state,
        )
        .optional()?;
    Ok(row)
}

/// Upsert the state row with the fixed `id = 1`.
pub fn set(conn: &Connection, state: &ContainerState) -> Result<(), DbError> {
    conn.execute(
        "INSERT OR REPLACE INTO container_state
           (id, current_tool, tool_declared_timeout_ms, tool_started_at, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4)",
        params![
            state.current_tool,
            state.tool_declared_timeout_ms,
            state.tool_started_at.map(|t| t.to_rfc3339()),
            state.updated_at.map(|t| t.to_rfc3339()),
        ],
    )?;
    Ok(())
}

/// Clear the active tool fields, leaving `updated_at` set to `Utc::now()`.
/// If no row exists yet, one is created with `id = 1` and nulled tool fields.
pub fn clear_tool(conn: &Connection) -> Result<(), DbError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO container_state
           (id, current_tool, tool_declared_timeout_ms, tool_started_at, updated_at)
         VALUES (1, NULL, NULL, NULL, ?1)
         ON CONFLICT(id) DO UPDATE SET
           current_tool = NULL,
           tool_declared_timeout_ms = NULL,
           tool_started_at = NULL,
           updated_at = excluded.updated_at",
        params![now],
    )?;
    Ok(())
}

/// Empty-string-tolerant `RFC3339` timestamp parse. `SQLite` columns
/// that should be NULL sometimes end up as the empty string when an
/// adapter writes `Some("")` instead of `None`. Real failure mode
/// observed live: a stuck `tool_started_at = ''` crashed the session
/// reconciler in a hot-loop with chrono's `ParseError(TooShort)`.
/// Treat empty as missing rather than propagating the error — by the
/// time it surfaces, the only visible symptom is a wedged session.
fn parse_dt_opt_empty_is_none(s: Option<&str>) -> rusqlite::Result<Option<DateTime<Utc>>> {
    match s {
        None | Some("") => Ok(None),
        Some(ts) => DateTime::parse_from_rfc3339(ts)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            }),
    }
}

fn row_to_state(row: &Row<'_>) -> rusqlite::Result<ContainerState> {
    let tool_started_at_str: Option<String> = row.get("tool_started_at")?;
    let tool_started_at = parse_dt_opt_empty_is_none(tool_started_at_str.as_deref())?;
    let updated_at_str: Option<String> = row.get("updated_at")?;
    let updated_at = parse_dt_opt_empty_is_none(updated_at_str.as_deref())?;
    let current_tool: Option<String> = row.get("current_tool")?;
    // Collapse the same empty-string-as-None mistake on `current_tool`:
    // observed sitting next to a NULL `tool_started_at` after the
    // runner crashed mid-tool. Nothing downstream wants `Some("")`.
    let current_tool = current_tool.filter(|s| !s.is_empty());
    Ok(ContainerState {
        current_tool,
        tool_declared_timeout_ms: row.get("tool_declared_timeout_ms")?,
        tool_started_at,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open_outbound, SessionPaths};
    use copperclaw_types::{AgentGroupId, SessionId};

    fn fresh_outbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        (tmp, conn)
    }

    #[test]
    fn get_returns_none_when_empty() {
        let (_tmp, conn) = fresh_outbound();
        assert!(get(&conn).unwrap().is_none());
    }

    #[test]
    fn set_then_get_roundtrips() {
        let (_tmp, conn) = fresh_outbound();
        let now = Utc::now();
        let state = ContainerState {
            current_tool: Some("bash".into()),
            tool_declared_timeout_ms: Some(30_000),
            tool_started_at: Some(now),
            updated_at: Some(now),
        };
        set(&conn, &state).unwrap();
        let got = get(&conn).unwrap().unwrap();
        assert_eq!(got.current_tool.as_deref(), Some("bash"));
        assert_eq!(got.tool_declared_timeout_ms, Some(30_000));
        assert!(got.tool_started_at.is_some());
        assert!(got.updated_at.is_some());
    }

    #[test]
    fn set_with_all_nulls_works() {
        let (_tmp, conn) = fresh_outbound();
        let state = ContainerState::default();
        set(&conn, &state).unwrap();
        let got = get(&conn).unwrap().unwrap();
        assert_eq!(got, ContainerState::default());
    }

    #[test]
    fn set_replaces_existing_row() {
        let (_tmp, conn) = fresh_outbound();
        let first = ContainerState {
            current_tool: Some("first".into()),
            tool_declared_timeout_ms: Some(1),
            tool_started_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        };
        set(&conn, &first).unwrap();
        let second = ContainerState {
            current_tool: Some("second".into()),
            tool_declared_timeout_ms: Some(2),
            tool_started_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        };
        set(&conn, &second).unwrap();
        let got = get(&conn).unwrap().unwrap();
        assert_eq!(got.current_tool.as_deref(), Some("second"));
        assert_eq!(got.tool_declared_timeout_ms, Some(2));
    }

    #[test]
    fn clear_tool_creates_row_when_absent() {
        let (_tmp, conn) = fresh_outbound();
        assert!(get(&conn).unwrap().is_none());
        clear_tool(&conn).unwrap();
        let got = get(&conn).unwrap().unwrap();
        assert!(got.current_tool.is_none());
        assert!(got.tool_declared_timeout_ms.is_none());
        assert!(got.tool_started_at.is_none());
        assert!(got.updated_at.is_some());
    }

    #[test]
    fn clear_tool_nulls_tool_fields_but_refreshes_updated_at() {
        let (_tmp, conn) = fresh_outbound();
        let state = ContainerState {
            current_tool: Some("bash".into()),
            tool_declared_timeout_ms: Some(30_000),
            tool_started_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        };
        set(&conn, &state).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        clear_tool(&conn).unwrap();
        let got = get(&conn).unwrap().unwrap();
        assert!(got.current_tool.is_none());
        assert!(got.tool_declared_timeout_ms.is_none());
        assert!(got.tool_started_at.is_none());
        assert!(got.updated_at.unwrap() > state.updated_at.unwrap());
    }

    #[test]
    fn manual_insert_with_wrong_id_is_rejected() {
        let (_tmp, conn) = fresh_outbound();
        let err = conn
            .execute(
                "INSERT INTO container_state (id, current_tool) VALUES (2, NULL)",
                [],
            )
            .unwrap_err();
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
    }

    #[test]
    fn row_decode_rejects_bad_tool_started_at() {
        let (_tmp, conn) = fresh_outbound();
        conn.execute(
            "INSERT INTO container_state
               (id, current_tool, tool_declared_timeout_ms, tool_started_at, updated_at)
             VALUES (1, 'bash', 0, 'nope', NULL)",
            [],
        )
        .unwrap();
        let err = get(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn row_decode_rejects_bad_updated_at() {
        let (_tmp, conn) = fresh_outbound();
        conn.execute(
            "INSERT INTO container_state
               (id, current_tool, tool_declared_timeout_ms, tool_started_at, updated_at)
             VALUES (1, NULL, NULL, NULL, 'nope')",
            [],
        )
        .unwrap();
        let err = get(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
