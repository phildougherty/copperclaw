//! Writes and reads against per-session `outbound.db::processing_ack`.
//!
//! The container writes here to acknowledge that it has picked up an inbound
//! message and to report progress. The host reads to decide when to stop
//! resending and to surface "stuck" sessions.

use crate::DbError;
use chrono::{DateTime, Utc};
use copperclaw_types::MessageId;
use rusqlite::{params, Connection, OptionalExtension, Row};

/// Lifecycle states the container reports back to the host. The string form
/// is what lands in the `status` column.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProcessingStatus {
    Processing,
    Done,
    Failed,
}

impl ProcessingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Processing => "processing",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "processing" => Some(Self::Processing),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessingClaim {
    pub message_id: MessageId,
    pub status: ProcessingStatus,
    pub status_changed: DateTime<Utc>,
}

/// Insert a new claim row. Returns a sqlite error if the `message_id` already
/// has a claim (callers should use [`update_status`] instead).
pub fn insert(
    conn: &Connection,
    message_id: MessageId,
    status: ProcessingStatus,
) -> Result<(), DbError> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO processing_ack (message_id, status, status_changed)
         VALUES (?1, ?2, ?3)",
        params![message_id.as_uuid().to_string(), status.as_str(), now],
    )?;
    Ok(())
}

/// Update the status of an existing claim. Returns [`DbError::NotFound`] if
/// no row exists for the given `message_id`.
pub fn update_status(
    conn: &Connection,
    message_id: MessageId,
    status: ProcessingStatus,
) -> Result<(), DbError> {
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE processing_ack SET status = ?1, status_changed = ?2 WHERE message_id = ?3",
        params![status.as_str(), now, message_id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// List every claim in `status_changed` ascending order.
pub fn get_all(conn: &Connection) -> Result<Vec<ProcessingClaim>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT message_id, status, status_changed
         FROM processing_ack
         ORDER BY status_changed",
    )?;
    let rows = stmt.query_map([], row_to_claim)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Fetch a single claim. Returns `Ok(None)` if no row exists.
pub fn get(conn: &Connection, message_id: MessageId) -> Result<Option<ProcessingClaim>, DbError> {
    Ok(conn
        .query_row(
            "SELECT message_id, status, status_changed
             FROM processing_ack WHERE message_id = ?1",
            params![message_id.as_uuid().to_string()],
            row_to_claim,
        )
        .optional()?)
}

/// Delete a claim. Returns [`DbError::NotFound`] if no row exists.
pub fn delete(conn: &Connection, message_id: MessageId) -> Result<(), DbError> {
    let n = conn.execute(
        "DELETE FROM processing_ack WHERE message_id = ?1",
        params![message_id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

fn row_to_claim(row: &Row<'_>) -> rusqlite::Result<ProcessingClaim> {
    let id_str: String = row.get("message_id")?;
    let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status_str: String = row.get("status")?;
    let status = ProcessingStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown processing status {status_str}").into(),
        )
    })?;
    let changed_str: String = row.get("status_changed")?;
    let status_changed = DateTime::parse_from_rfc3339(&changed_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(ProcessingClaim {
        message_id: MessageId(id),
        status,
        status_changed,
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
    fn status_as_str_round_trips() {
        for s in [
            ProcessingStatus::Processing,
            ProcessingStatus::Done,
            ProcessingStatus::Failed,
        ] {
            assert_eq!(ProcessingStatus::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn status_parse_unknown_returns_none() {
        assert!(ProcessingStatus::parse("bogus").is_none());
    }

    #[test]
    fn insert_then_get() {
        let (_tmp, conn) = fresh_outbound();
        let id = MessageId::new();
        insert(&conn, id, ProcessingStatus::Processing).unwrap();
        let row = get(&conn, id).unwrap().unwrap();
        assert_eq!(row.message_id, id);
        assert_eq!(row.status, ProcessingStatus::Processing);
    }

    #[test]
    fn insert_duplicate_is_error() {
        let (_tmp, conn) = fresh_outbound();
        let id = MessageId::new();
        insert(&conn, id, ProcessingStatus::Processing).unwrap();
        let err = insert(&conn, id, ProcessingStatus::Processing).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn update_status_changes_value_and_timestamp() {
        let (_tmp, conn) = fresh_outbound();
        let id = MessageId::new();
        insert(&conn, id, ProcessingStatus::Processing).unwrap();
        let first = get(&conn, id).unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        update_status(&conn, id, ProcessingStatus::Done).unwrap();
        let second = get(&conn, id).unwrap().unwrap();
        assert_eq!(second.status, ProcessingStatus::Done);
        assert!(second.status_changed > first.status_changed);
    }

    #[test]
    fn update_status_missing_is_not_found() {
        let (_tmp, conn) = fresh_outbound();
        let err = update_status(&conn, MessageId::new(), ProcessingStatus::Done).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn get_missing_returns_none() {
        let (_tmp, conn) = fresh_outbound();
        assert!(get(&conn, MessageId::new()).unwrap().is_none());
    }

    #[test]
    fn get_all_orders_by_status_changed() {
        let (_tmp, conn) = fresh_outbound();
        let a = MessageId::new();
        let b = MessageId::new();
        insert(&conn, a, ProcessingStatus::Processing).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        insert(&conn, b, ProcessingStatus::Processing).unwrap();
        let rows = get_all(&conn).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].message_id, a);
        assert_eq!(rows[1].message_id, b);
    }

    #[test]
    fn get_all_empty_when_no_rows() {
        let (_tmp, conn) = fresh_outbound();
        assert!(get_all(&conn).unwrap().is_empty());
    }

    #[test]
    fn delete_removes_row() {
        let (_tmp, conn) = fresh_outbound();
        let id = MessageId::new();
        insert(&conn, id, ProcessingStatus::Processing).unwrap();
        delete(&conn, id).unwrap();
        assert!(get(&conn, id).unwrap().is_none());
    }

    #[test]
    fn delete_missing_is_not_found() {
        let (_tmp, conn) = fresh_outbound();
        let err = delete(&conn, MessageId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn row_decode_rejects_bad_uuid() {
        let (_tmp, conn) = fresh_outbound();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO processing_ack (message_id, status, status_changed)
             VALUES (?1, 'processing', ?2)",
            params!["not-a-uuid", now],
        )
        .unwrap();
        let err = get_all(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn row_decode_rejects_unknown_status() {
        let (_tmp, conn) = fresh_outbound();
        let id = MessageId::new();
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO processing_ack (message_id, status, status_changed)
             VALUES (?1, 'bogus', ?2)",
            params![id.as_uuid().to_string(), now],
        )
        .unwrap();
        let err = get_all(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn row_decode_rejects_bad_timestamp() {
        let (_tmp, conn) = fresh_outbound();
        let id = MessageId::new();
        conn.execute(
            "INSERT INTO processing_ack (message_id, status, status_changed)
             VALUES (?1, 'processing', 'nope')",
            params![id.as_uuid().to_string()],
        )
        .unwrap();
        let err = get_all(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
