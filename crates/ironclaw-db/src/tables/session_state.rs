//! Writes and reads against per-session `outbound.db::session_state`.
//!
//! Container-owned KV store for state that should survive across container
//! wakes within a single session (e.g. cached tool results, paged-through
//! cursors). The host reads it only for debugging.

use crate::DbError;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

/// Fetch the value for `key`. Returns `Ok(None)` if no row exists.
pub fn get(conn: &Connection, key: &str) -> Result<Option<String>, DbError> {
    Ok(conn
        .query_row(
            "SELECT value FROM session_state WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// Upsert a value. Uses `INSERT OR REPLACE`, so existing rows are overwritten
/// and `updated_at` is refreshed to `Utc::now()`.
pub fn set(conn: &Connection, key: &str, value: &str) -> Result<(), DbError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO session_state (key, value, updated_at)
         VALUES (?1, ?2, ?3)",
        params![key, value, now],
    )?;
    Ok(())
}

/// Delete a row. Returns [`DbError::NotFound`] if no row exists.
pub fn delete(conn: &Connection, key: &str) -> Result<(), DbError> {
    let n = conn.execute("DELETE FROM session_state WHERE key = ?1", params![key])?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// List every `(key, value)` pair, ordered by `key`.
pub fn list(conn: &Connection) -> Result<Vec<(String, String)>, DbError> {
    let mut stmt = conn.prepare("SELECT key, value FROM session_state ORDER BY key")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open_outbound, SessionPaths};
    use ironclaw_types::{AgentGroupId, SessionId};

    fn fresh_outbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        (tmp, conn)
    }

    #[test]
    fn get_missing_returns_none() {
        let (_tmp, conn) = fresh_outbound();
        assert!(get(&conn, "absent").unwrap().is_none());
    }

    #[test]
    fn set_then_get_roundtrips() {
        let (_tmp, conn) = fresh_outbound();
        set(&conn, "k", "v").unwrap();
        assert_eq!(get(&conn, "k").unwrap(), Some("v".to_string()));
    }

    #[test]
    fn set_overwrites_existing_value_and_refreshes_updated_at() {
        let (_tmp, conn) = fresh_outbound();
        set(&conn, "k", "old").unwrap();
        let first: String = conn
            .query_row(
                "SELECT updated_at FROM session_state WHERE key = ?1",
                params!["k"],
                |row| row.get(0),
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        set(&conn, "k", "new").unwrap();
        let second: String = conn
            .query_row(
                "SELECT updated_at FROM session_state WHERE key = ?1",
                params!["k"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(get(&conn, "k").unwrap(), Some("new".to_string()));
        assert_ne!(first, second);
    }

    #[test]
    fn delete_removes_row() {
        let (_tmp, conn) = fresh_outbound();
        set(&conn, "k", "v").unwrap();
        delete(&conn, "k").unwrap();
        assert!(get(&conn, "k").unwrap().is_none());
    }

    #[test]
    fn delete_missing_is_not_found() {
        let (_tmp, conn) = fresh_outbound();
        let err = delete(&conn, "absent").unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn list_orders_by_key() {
        let (_tmp, conn) = fresh_outbound();
        set(&conn, "c", "3").unwrap();
        set(&conn, "a", "1").unwrap();
        set(&conn, "b", "2").unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn list_empty_when_no_rows() {
        let (_tmp, conn) = fresh_outbound();
        assert!(list(&conn).unwrap().is_empty());
    }
}
