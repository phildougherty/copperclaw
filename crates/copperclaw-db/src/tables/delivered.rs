//! Writes and reads against per-session `inbound.db::delivered`.
//!
//! Host-owned writer; the container reads via `open_inbound_ro_no_mmap` to
//! learn which outbound messages have already been delivered (so it can
//! prune them from its outbound queue / state).

use crate::DbError;
use chrono::{DateTime, Utc};
use copperclaw_types::MessageId;
use rusqlite::{params, Connection, Row};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    pub message_out_id: MessageId,
    pub platform_message_id: Option<String>,
    pub status: String,
    pub delivered_at: DateTime<Utc>,
}

/// Record a delivery outcome for an outbound message. `delivered_at` is set
/// to `Utc::now()` at the moment of insertion.
pub fn insert(
    conn: &Connection,
    message_out_id: MessageId,
    platform_message_id: Option<&str>,
    status: &str,
) -> Result<(), DbError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO delivered (message_out_id, platform_message_id, status, delivered_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            message_out_id.as_uuid().to_string(),
            platform_message_id,
            status,
            now,
        ],
    )?;
    Ok(())
}

/// Return just the ids of delivered messages. Hot path on every container
/// poll, so we keep the projection narrow.
pub fn get_delivered_ids(conn: &Connection) -> Result<HashSet<MessageId>, DbError> {
    let mut stmt = conn.prepare("SELECT message_out_id FROM delivered")?;
    let rows = stmt.query_map([], |row| {
        let id_str: String = row.get(0)?;
        let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(MessageId(id))
    })?;
    let mut out = HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// List every delivered row in insertion order (`delivered_at` ascending).
pub fn list(conn: &Connection) -> Result<Vec<Delivered>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT message_out_id, platform_message_id, status, delivered_at
         FROM delivered
         ORDER BY delivered_at",
    )?;
    let rows = stmt.query_map([], row_to_delivered)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn row_to_delivered(row: &Row<'_>) -> rusqlite::Result<Delivered> {
    let id_str: String = row.get("message_out_id")?;
    let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let delivered_at_str: String = row.get("delivered_at")?;
    let delivered_at = DateTime::parse_from_rfc3339(&delivered_at_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(Delivered {
        message_out_id: MessageId(id),
        platform_message_id: row.get("platform_message_id")?,
        status: row.get("status")?,
        delivered_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open_inbound, SessionPaths};
    use copperclaw_types::{AgentGroupId, SessionId};

    fn fresh_inbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        (tmp, conn)
    }

    #[test]
    fn insert_then_list_roundtrips() {
        let (_tmp, conn) = fresh_inbound();
        let id = MessageId::new();
        insert(&conn, id, Some("p-1"), "ok").unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_out_id, id);
        assert_eq!(rows[0].platform_message_id.as_deref(), Some("p-1"));
        assert_eq!(rows[0].status, "ok");
    }

    #[test]
    fn insert_accepts_null_platform_id() {
        let (_tmp, conn) = fresh_inbound();
        let id = MessageId::new();
        insert(&conn, id, None, "ok").unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].platform_message_id.is_none());
    }

    #[test]
    fn duplicate_message_out_id_is_error() {
        let (_tmp, conn) = fresh_inbound();
        let id = MessageId::new();
        insert(&conn, id, None, "ok").unwrap();
        let err = insert(&conn, id, None, "ok").unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn get_delivered_ids_returns_inserted() {
        let (_tmp, conn) = fresh_inbound();
        let a = MessageId::new();
        let b = MessageId::new();
        insert(&conn, a, None, "ok").unwrap();
        insert(&conn, b, None, "failed").unwrap();
        let ids = get_delivered_ids(&conn).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
    }

    #[test]
    fn get_delivered_ids_empty_when_no_rows() {
        let (_tmp, conn) = fresh_inbound();
        assert!(get_delivered_ids(&conn).unwrap().is_empty());
    }

    #[test]
    fn list_is_ordered_by_delivered_at() {
        let (_tmp, conn) = fresh_inbound();
        let a = MessageId::new();
        insert(&conn, a, None, "ok").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = MessageId::new();
        insert(&conn, b, None, "ok").unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message_out_id, a);
        assert_eq!(rows[1].message_out_id, b);
    }

    #[test]
    fn list_empty_when_no_rows() {
        let (_tmp, conn) = fresh_inbound();
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn row_decode_rejects_bad_uuid() {
        let (_tmp, conn) = fresh_inbound();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO delivered (message_out_id, platform_message_id, status, delivered_at)
             VALUES (?1, NULL, 'ok', ?2)",
            params!["not-a-uuid", now],
        )
        .unwrap();
        let err = list(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn row_decode_rejects_bad_timestamp() {
        let (_tmp, conn) = fresh_inbound();
        let id = MessageId::new();
        conn.execute(
            "INSERT INTO delivered (message_out_id, platform_message_id, status, delivered_at)
             VALUES (?1, NULL, 'ok', 'not-a-timestamp')",
            params![id.as_uuid().to_string()],
        )
        .unwrap();
        let err = list(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
