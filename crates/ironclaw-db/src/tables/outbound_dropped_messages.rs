//! CRUD for `outbound_dropped_messages`.
//!
//! When the delivery loop exhausts all retries for an outbound message it
//! writes a row here so an operator can later inspect failures via
//! `iclaw dropped-messages list` and replay them via
//! `iclaw dropped-messages replay <id>`.
//!
//! The table lives in the **central** DB so an operator can query it without
//! knowing which session the failure came from.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, ChannelType, MessageId, MessageKind, SessionId};
use rusqlite::{params, OptionalExtension, Row};
use uuid::Uuid;

/// A single outbound dead-letter record.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundDroppedMessage {
    /// Surrogate id for this dead-letter row (not the original `message_out_id`).
    pub id: Uuid,
    /// Session that originally owned the outbound message.
    pub session_id: SessionId,
    /// Agent group the session belongs to.
    pub agent_group_id: AgentGroupId,
    /// Original `messages_out.id` — preserved so the operator can cross-reference.
    pub message_out_id: MessageId,
    /// Channel the delivery was attempted to, if known.
    pub channel_type: Option<ChannelType>,
    /// Platform-specific destination id, if known.
    pub platform_id: Option<String>,
    /// Thread id, if any.
    pub thread_id: Option<String>,
    /// Message kind (chat, task, …).
    pub kind: MessageKind,
    /// Raw JSON content of the outbound message.
    pub content: serde_json::Value,
    /// Description of the last error that caused the failure.
    pub last_error: String,
    /// When this row was inserted.
    pub dropped_at: DateTime<Utc>,
}

/// Parameters for inserting a dead-letter row.
#[derive(Debug, Clone)]
pub struct InsertOutboundDropped {
    pub session_id: SessionId,
    pub agent_group_id: AgentGroupId,
    pub message_out_id: MessageId,
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub thread_id: Option<String>,
    pub kind: MessageKind,
    pub content: serde_json::Value,
    pub last_error: String,
}

fn row_to_record(row: &Row<'_>) -> rusqlite::Result<OutboundDroppedMessage> {
    let id_str: String = row.get("id")?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let session_str: String = row.get("session_id")?;
    let session_id = SessionId(Uuid::parse_str(&session_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?);
    let ag_str: String = row.get("agent_group_id")?;
    let agent_group_id = AgentGroupId(Uuid::parse_str(&ag_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?);
    let msg_str: String = row.get("message_out_id")?;
    let message_out_id = MessageId(Uuid::parse_str(&msg_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?);
    let channel_type: Option<String> = row.get("channel_type")?;
    let kind_str: String = row.get("kind")?;
    let kind = match kind_str.as_str() {
        "chat" => MessageKind::Chat,
        "task" => MessageKind::Task,
        "webhook" => MessageKind::Webhook,
        "system" => MessageKind::System,
        "agent" => MessageKind::Agent,
        "card" => MessageKind::Card,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown message kind: {other}").into(),
            ));
        }
    };
    let content_str: String = row.get("content")?;
    let content: serde_json::Value = serde_json::from_str(&content_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let dropped_at_str: String = row.get("dropped_at")?;
    let dropped_at = DateTime::parse_from_rfc3339(&dropped_at_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;
    Ok(OutboundDroppedMessage {
        id,
        session_id,
        agent_group_id,
        message_out_id,
        channel_type: channel_type.map(ChannelType::from),
        platform_id: row.get("platform_id")?,
        thread_id: row.get("thread_id")?,
        kind,
        content,
        last_error: row.get("last_error")?,
        dropped_at,
    })
}

/// Insert a new outbound dead-letter row. Returns the inserted record with
/// its generated id and timestamp.
pub fn insert(db: &CentralDb, req: InsertOutboundDropped) -> Result<OutboundDroppedMessage, DbError> {
    let id = Uuid::now_v7();
    let now = Utc::now();
    let content_json = serde_json::to_string(&req.content)?;
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO outbound_dropped_messages
           (id, session_id, agent_group_id, message_out_id, channel_type,
            platform_id, thread_id, kind, content, last_error, dropped_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id.to_string(),
            req.session_id.as_uuid().to_string(),
            req.agent_group_id.as_uuid().to_string(),
            req.message_out_id.as_uuid().to_string(),
            req.channel_type.as_ref().map(ChannelType::as_str),
            req.platform_id,
            req.thread_id,
            req.kind.as_str(),
            content_json,
            req.last_error,
            now.to_rfc3339(),
        ],
    )?;
    Ok(OutboundDroppedMessage {
        id,
        session_id: req.session_id,
        agent_group_id: req.agent_group_id,
        message_out_id: req.message_out_id,
        channel_type: req.channel_type,
        platform_id: req.platform_id,
        thread_id: req.thread_id,
        kind: req.kind,
        content: req.content,
        last_error: req.last_error,
        dropped_at: now,
    })
}

/// Fetch a single record by its dead-letter id. Returns `Err(NotFound)` if
/// no row with that id exists.
pub fn get(db: &CentralDb, id: Uuid) -> Result<OutboundDroppedMessage, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT id, session_id, agent_group_id, message_out_id, channel_type,
                platform_id, thread_id, kind, content, last_error, dropped_at
         FROM outbound_dropped_messages WHERE id = ?1",
        params![id.to_string()],
        row_to_record,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

/// List dead-letter rows. Optionally filtered to rows dropped on or after
/// `since` and capped to `limit` rows (most-recent first).
pub fn list(
    db: &CentralDb,
    since: Option<DateTime<Utc>>,
    limit: Option<i64>,
) -> Result<Vec<OutboundDroppedMessage>, DbError> {
    let conn = db.conn()?;
    let limit_val = limit.unwrap_or(i64::MAX);
    let rows: Vec<OutboundDroppedMessage> = if let Some(ts) = since {
        let mut stmt = conn.prepare(
            "SELECT id, session_id, agent_group_id, message_out_id, channel_type,
                    platform_id, thread_id, kind, content, last_error, dropped_at
             FROM outbound_dropped_messages
             WHERE dropped_at >= ?1
             ORDER BY dropped_at DESC
             LIMIT ?2",
        )?;
        let mapped = stmt.query_map(params![ts.to_rfc3339(), limit_val], row_to_record)?;
        let out: rusqlite::Result<Vec<_>> = mapped.collect();
        out?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, session_id, agent_group_id, message_out_id, channel_type,
                    platform_id, thread_id, kind, content, last_error, dropped_at
             FROM outbound_dropped_messages
             ORDER BY dropped_at DESC
             LIMIT ?1",
        )?;
        let mapped = stmt.query_map(params![limit_val], row_to_record)?;
        let out: rusqlite::Result<Vec<_>> = mapped.collect();
        out?
    };
    Ok(rows)
}

/// Delete a dead-letter row by id (called after a successful replay so the
/// row doesn't keep appearing in `iclaw dropped-messages list`).
pub fn delete(db: &CentralDb, id: Uuid) -> Result<bool, DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM outbound_dropped_messages WHERE id = ?1",
        params![id.to_string()],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::AgentGroupId;
    use serde_json::json;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn sample() -> InsertOutboundDropped {
        InsertOutboundDropped {
            session_id: SessionId::new(),
            agent_group_id: AgentGroupId::new(),
            message_out_id: MessageId::new(),
            channel_type: Some(ChannelType::new("cli")),
            platform_id: Some("stdin".into()),
            thread_id: None,
            kind: MessageKind::Chat,
            content: json!({"text": "hello"}),
            last_error: "adapter returned Err".into(),
        }
    }

    #[test]
    fn insert_returns_populated_record() {
        let db = db();
        let rec = insert(&db, sample()).unwrap();
        assert_eq!(rec.kind, MessageKind::Chat);
        assert_eq!(rec.last_error, "adapter returned Err");
        assert_eq!(rec.channel_type, Some(ChannelType::new("cli")));
        assert_eq!(rec.platform_id.as_deref(), Some("stdin"));
    }

    #[test]
    fn get_by_id_roundtrips() {
        let db = db();
        let rec = insert(&db, sample()).unwrap();
        let fetched = get(&db, rec.id).unwrap();
        assert_eq!(fetched.id, rec.id);
        assert_eq!(fetched.last_error, rec.last_error);
        assert_eq!(fetched.content, rec.content);
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = db();
        let err = get(&db, Uuid::now_v7()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn list_empty_when_no_rows() {
        let db = db();
        assert!(list(&db, None, None).unwrap().is_empty());
    }

    #[test]
    fn list_returns_inserted_rows() {
        let db = db();
        insert(&db, sample()).unwrap();
        insert(&db, sample()).unwrap();
        let rows = list(&db, None, None).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn list_limit_caps_results() {
        let db = db();
        for _ in 0..5 {
            insert(&db, sample()).unwrap();
        }
        let rows = list(&db, None, Some(3)).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn list_since_filter_works() {
        let db = db();
        insert(&db, sample()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let cutoff = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let rec2 = insert(&db, sample()).unwrap();
        let rows = list(&db, Some(cutoff), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, rec2.id);
    }

    #[test]
    fn delete_removes_row() {
        let db = db();
        let rec = insert(&db, sample()).unwrap();
        assert!(delete(&db, rec.id).unwrap());
        let err = get(&db, rec.id).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn delete_missing_returns_false() {
        let db = db();
        assert!(!delete(&db, Uuid::now_v7()).unwrap());
    }

    #[test]
    fn insert_unique_ids_per_row() {
        let db = db();
        let a = insert(&db, sample()).unwrap();
        let b = insert(&db, sample()).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn null_channel_fields_roundtrip() {
        let db = db();
        let mut row = sample();
        row.channel_type = None;
        row.platform_id = None;
        row.thread_id = None;
        let inserted = insert(&db, row).unwrap();
        let fetched = get(&db, inserted.id).unwrap();
        assert!(fetched.channel_type.is_none());
        assert!(fetched.platform_id.is_none());
        assert!(fetched.thread_id.is_none());
    }
}
