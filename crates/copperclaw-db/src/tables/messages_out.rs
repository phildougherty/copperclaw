//! Reads and writes against per-session `outbound.db::messages_out`.
//!
//! Container is the writer; host reads. The host's delivery loop uses
//! `list_due` to pick the next batch.

use crate::DbError;
use chrono::{DateTime, Utc};
use copperclaw_types::{ChannelType, MessageId, MessageKind, MessageOutRow};
use rusqlite::{Connection, OptionalExtension, Row, params};

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

/// Persisted delivery retry state for one outbound row (M21 S3).
///
/// Mirrors the host delivery loop's in-memory retry cache onto the row
/// itself so attempt counters and backoff windows survive a host restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedRetryState {
    pub id: MessageId,
    /// Delivery attempts already made by the host (>= 0).
    pub tries: u32,
    /// Wall-clock time before which the host must not retry the row.
    /// `None` = no backoff window pending.
    pub not_before: Option<DateTime<Utc>>,
}

/// Write-through of the host's in-memory retry state for `id` (M21 S3).
///
/// A missing row is a silent no-op (0 rows updated) — the caller treats
/// this persistence as best-effort bookkeeping, never load-bearing for
/// the current host lifetime.
pub fn set_retry_state(
    conn: &Connection,
    id: MessageId,
    tries: u32,
    not_before: Option<DateTime<Utc>>,
) -> Result<(), DbError> {
    conn.execute(
        "UPDATE messages_out SET tries = ?2, not_before = ?3 WHERE id = ?1",
        params![
            id.as_uuid().to_string(),
            tries,
            not_before.map(|t| t.to_rfc3339()),
        ],
    )?;
    Ok(())
}

/// All rows carrying persisted retry state (`tries > 0` or a pending
/// `not_before` window). Used by the host delivery loop to prime its
/// in-memory retry cache on the first poll of a session after boot.
pub fn list_retry_state(conn: &Connection) -> Result<Vec<PersistedRetryState>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT id, tries, not_before FROM messages_out
         WHERE tries > 0 OR not_before IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        let id_str: String = row.get("id")?;
        let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let tries: i64 = row.get("tries")?;
        // Same empty-string-as-Some pitfall as `deliver_after` above:
        // coalesce '' to None instead of tripping chrono's `TooShort`.
        let not_before: Option<String> = row.get("not_before")?;
        let not_before = match not_before.as_deref() {
            None | Some("") => None,
            Some(ts) => Some(
                DateTime::parse_from_rfc3339(ts)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
            ),
        };
        Ok(PersistedRetryState {
            id: MessageId(id),
            tries: u32::try_from(tries).unwrap_or(0),
            not_before,
        })
    })?;
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
    let kind = MessageKind::parse_str(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown kind {kind}").into(),
        )
    })?;

    let timestamp_str: String = row.get("timestamp")?;
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;

    // Same empty-string-as-Some pitfall as messages_in::parse_dt_opt:
    // a row written with `deliver_after=''` would crash the parser with
    // chrono's `TooShort`. Coalesce to None.
    let deliver_after: Option<String> = row.get("deliver_after")?;
    let deliver_after = match deliver_after.as_deref() {
        None | Some("") => None,
        Some(ts) => Some(
            DateTime::parse_from_rfc3339(ts)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
        ),
    };

    let content_str: String = row.get("content")?;
    let content: serde_json::Value = serde_json::from_str(&content_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
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
    use crate::session::{SessionPaths, open_outbound};
    use copperclaw_types::{AgentGroupId, SessionId};
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

    #[test]
    fn retry_state_round_trips() {
        // M21 S3 acceptance: counters round-trip through the row.
        let (_tmp, conn) = fresh_outbound();
        let msg = make_msg();
        let id = msg.id;
        insert(&conn, &msg).unwrap();

        let not_before = Utc::now() + chrono::Duration::seconds(30);
        set_retry_state(&conn, id, 2, Some(not_before)).unwrap();

        let listed = list_retry_state(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].tries, 2);
        // RFC3339 round-trip preserves the instant (sub-second included).
        assert_eq!(
            listed[0].not_before.map(|t| t.to_rfc3339()),
            Some(not_before.to_rfc3339())
        );
    }

    #[test]
    fn retry_state_not_before_clears_to_null() {
        // The final bump (exhaustion) persists `tries` with no window; the
        // NULL must actually replace an earlier window, not linger.
        let (_tmp, conn) = fresh_outbound();
        let msg = make_msg();
        let id = msg.id;
        insert(&conn, &msg).unwrap();

        set_retry_state(&conn, id, 1, Some(Utc::now())).unwrap();
        set_retry_state(&conn, id, 3, None).unwrap();

        let listed = list_retry_state(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].tries, 3);
        assert_eq!(listed[0].not_before, None);
    }

    #[test]
    fn list_retry_state_skips_untouched_rows() {
        // Rows the delivery loop never had to retry carry the column
        // defaults (tries=0, not_before NULL) and must not be primed.
        let (_tmp, conn) = fresh_outbound();
        insert(&conn, &make_msg()).unwrap();
        let touched = make_msg();
        let touched_id = touched.id;
        insert(&conn, &touched).unwrap();
        set_retry_state(&conn, touched_id, 1, None).unwrap();

        let listed = list_retry_state(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, touched_id);
    }

    #[test]
    fn set_retry_state_on_missing_row_is_noop() {
        let (_tmp, conn) = fresh_outbound();
        set_retry_state(&conn, MessageId::new(), 1, None).unwrap();
        assert!(list_retry_state(&conn).unwrap().is_empty());
    }
}
