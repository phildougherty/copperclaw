//! Reads and writes against per-session `outbound.db::messages_out`.
//!
//! Container is the writer; host reads. The host's delivery loop uses
//! `list_due` to pick the next batch.

use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{ChannelType, MessageId, MessageKind, MessageOutRow};
use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone)]
pub struct WriteOutbound {
    pub id: MessageId,
    pub in_reply_to: Option<MessageId>,
    pub timestamp: DateTime<Utc>,
    pub deliver_after: Option<DateTime<Utc>>,
    pub recurrence: Option<String>,
    pub kind: MessageKind,
    pub platform_id: Option<String>,
    pub channel_type: Option<ChannelType>,
    pub thread_id: Option<String>,
    pub content: serde_json::Value,
}

/// Insert with the next odd seq (container parity).
pub fn insert(conn: &Connection, msg: &WriteOutbound) -> Result<i64, DbError> {
    let seq = next_odd_seq(conn)?;
    conn.execute(
        "INSERT INTO messages_out
           (id, seq, in_reply_to, timestamp, deliver_after, recurrence, kind,
            platform_id, channel_type, thread_id, content)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            msg.id.as_uuid().to_string(),
            seq,
            msg.in_reply_to.map(|i| i.as_uuid().to_string()),
            msg.timestamp.to_rfc3339(),
            msg.deliver_after.map(|t| t.to_rfc3339()),
            &msg.recurrence,
            msg.kind.as_str(),
            &msg.platform_id,
            msg.channel_type.as_ref().map(ChannelType::as_str),
            &msg.thread_id,
            msg.content.to_string(),
        ],
    )?;
    Ok(seq)
}

fn next_odd_seq(conn: &Connection) -> Result<i64, DbError> {
    let max: Option<i64> = conn
        .query_row("SELECT MAX(seq) FROM messages_out", [], |r| r.get(0))
        .optional()?
        .flatten();
    let mut next = max.unwrap_or(0) + 1;
    if next % 2 == 0 {
        next += 1;
    }
    Ok(next)
}

pub fn list_due(conn: &Connection) -> Result<Vec<MessageOutRow>, DbError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, seq, in_reply_to, timestamp, deliver_after, recurrence, kind,
                platform_id, channel_type, thread_id, content
         FROM messages_out
         WHERE deliver_after IS NULL OR deliver_after <= ?1
         ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![now], row_to_message_out)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(conn: &Connection, id: MessageId) -> Result<MessageOutRow, DbError> {
    conn.query_row(
        "SELECT id, seq, in_reply_to, timestamp, deliver_after, recurrence, kind,
                platform_id, channel_type, thread_id, content
         FROM messages_out WHERE id = ?1",
        params![id.as_uuid().to_string()],
        row_to_message_out,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

fn row_to_message_out(row: &Row<'_>) -> rusqlite::Result<MessageOutRow> {
    let id_str: String = row.get("id")?;
    let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let in_reply_to: Option<String> = row.get("in_reply_to")?;
    let in_reply_to = in_reply_to
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .map(MessageId);

    let kind: String = row.get("kind")?;
    let kind = match kind.as_str() {
        "chat" => MessageKind::Chat,
        "task" => MessageKind::Task,
        "webhook" => MessageKind::Webhook,
        "system" => MessageKind::System,
        "agent" => MessageKind::Agent,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown kind {other}").into(),
            ))
        }
    };

    let timestamp_str: String = row.get("timestamp")?;
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;

    let deliver_after: Option<String> = row.get("deliver_after")?;
    let deliver_after = deliver_after
        .as_deref()
        .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;

    let content_str: String = row.get("content")?;
    let content: serde_json::Value = serde_json::from_str(&content_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let channel_type: Option<String> = row.get("channel_type")?;

    Ok(MessageOutRow {
        id: MessageId(id),
        seq: row.get("seq")?,
        in_reply_to,
        timestamp,
        deliver_after,
        recurrence: row.get("recurrence")?,
        kind,
        platform_id: row.get("platform_id")?,
        channel_type: channel_type.map(ChannelType::from),
        thread_id: row.get("thread_id")?,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open_outbound, SessionPaths};
    use ironclaw_types::{AgentGroupId, SessionId};
    use serde_json::json;

    fn fresh_outbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        (tmp, conn)
    }

    fn make_msg() -> WriteOutbound {
        WriteOutbound {
            id: MessageId::new(),
            in_reply_to: None,
            timestamp: Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind: MessageKind::Chat,
            platform_id: Some("chat-1".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            content: json!({"text":"hi"}),
        }
    }

    #[test]
    fn insert_returns_odd_seq() {
        let (_tmp, conn) = fresh_outbound();
        let seq1 = insert(&conn, &make_msg()).unwrap();
        let seq2 = insert(&conn, &make_msg()).unwrap();
        assert_eq!(seq1 % 2, 1, "expected odd, got {seq1}");
        assert_eq!(seq2 % 2, 1, "expected odd, got {seq2}");
        assert!(seq2 > seq1);
    }

    #[test]
    fn list_due_returns_immediate() {
        let (_tmp, conn) = fresh_outbound();
        insert(&conn, &make_msg()).unwrap();
        let rows = list_due(&conn).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn list_due_respects_deliver_after() {
        let (_tmp, conn) = fresh_outbound();
        let mut m = make_msg();
        m.deliver_after = Some(Utc::now() + chrono::Duration::seconds(60));
        insert(&conn, &m).unwrap();
        let rows = list_due(&conn).unwrap();
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn get_by_id_works() {
        let (_tmp, conn) = fresh_outbound();
        let msg = make_msg();
        let id = msg.id;
        insert(&conn, &msg).unwrap();
        let row = get(&conn, id).unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.kind, MessageKind::Chat);
    }

    #[test]
    fn get_missing_is_not_found() {
        let (_tmp, conn) = fresh_outbound();
        let err = get(&conn, MessageId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }
}
