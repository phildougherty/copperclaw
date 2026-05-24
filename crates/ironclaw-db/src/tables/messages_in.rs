//! Writes and reads against per-session `inbound.db::messages_in`.
//!
//! Host-owned writer; the container reads through `open_inbound_ro_no_mmap`.

use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{ChannelType, MessageId, MessageInRow, MessageKind};
use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone)]
pub struct WriteInbound {
    pub id: MessageId,
    pub kind: MessageKind,
    pub timestamp: DateTime<Utc>,
    pub content: serde_json::Value,
    pub trigger: bool,
    pub on_wake: bool,
    pub process_after: Option<DateTime<Utc>>,
    pub recurrence: Option<String>,
    pub series_id: Option<String>,
    pub platform_id: Option<String>,
    pub channel_type: Option<ChannelType>,
    pub thread_id: Option<String>,
    pub source_session_id: Option<String>,
}

/// Insert a row using the next even sequence number (host parity).
///
/// Looking at the max seq across both files is the runner's job; the host
/// only needs to pick the next even value. Concurrent host writers are
/// disallowed by design, so we don't worry about contention.
pub fn insert(conn: &Connection, msg: &WriteInbound) -> Result<i64, DbError> {
    insert_impl(conn, msg, false)
}

/// Same as [`insert`], but uses `INSERT OR IGNORE` so a row with the same
/// `id` already present is a no-op rather than a constraint violation.
///
/// Used by `agent_dispatch` to make cross-session inbound writes idempotent
/// under delivery-loop retry: the parent's inbound row reuses the source
/// outbound row's [`MessageId`], so a retry of the dispatch (between the
/// handler succeeding and `delivered::insert` succeeding) is dedup'd at
/// the `SQLite` layer rather than duplicating the parent's inbound.
///
/// Returns `Ok(0)` when the insert was ignored (row already present),
/// `Ok(seq)` with the new sequence number when the row was inserted.
pub fn insert_idempotent(conn: &Connection, msg: &WriteInbound) -> Result<i64, DbError> {
    insert_impl(conn, msg, true)
}

fn insert_impl(conn: &Connection, msg: &WriteInbound, idempotent: bool) -> Result<i64, DbError> {
    let seq = next_even_seq(conn)?;
    let sql = if idempotent {
        "INSERT OR IGNORE INTO messages_in
           (id, seq, kind, timestamp, status, process_after, recurrence,
            series_id, tries, trigger, platform_id, channel_type, thread_id,
            content, source_session_id, on_wake)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
    } else {
        "INSERT INTO messages_in
           (id, seq, kind, timestamp, status, process_after, recurrence,
            series_id, tries, trigger, platform_id, channel_type, thread_id,
            content, source_session_id, on_wake)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
    };
    let rows = conn.execute(
        sql,
        params![
            msg.id.as_uuid().to_string(),
            seq,
            msg.kind.as_str(),
            msg.timestamp.to_rfc3339(),
            msg.process_after.map(|t| t.to_rfc3339()),
            &msg.recurrence,
            &msg.series_id,
            i32::from(msg.trigger),
            &msg.platform_id,
            msg.channel_type.as_ref().map(ChannelType::as_str),
            &msg.thread_id,
            msg.content.to_string(),
            &msg.source_session_id,
            i32::from(msg.on_wake),
        ],
    )?;
    if rows == 0 {
        Ok(0)
    } else {
        Ok(seq)
    }
}

fn next_even_seq(conn: &Connection) -> Result<i64, DbError> {
    let max: Option<i64> = conn.query_row("SELECT MAX(seq) FROM messages_in", [], |r| r.get(0)).optional()?.flatten();
    let mut next = max.unwrap_or(0) + 1;
    if next % 2 != 0 {
        next += 1;
    }
    Ok(next)
}

pub fn count_due(conn: &Connection) -> Result<i64, DbError> {
    let now = Utc::now().to_rfc3339();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_in
         WHERE status = 'pending'
           AND trigger = 1
           AND (process_after IS NULL OR process_after <= ?1)",
        params![now],
        |r| r.get(0),
    )?;
    Ok(count)
}

/// Count any pending row whose `process_after` is null or due, *without*
/// the `trigger = 1` filter that [`count_due`] applies.
///
/// Why this exists: the typing-ticker needs to decide whether to keep
/// pulsing the "agent is working" indicator. The runner's first-poll
/// pass picks up non-trigger rows (agent-to-agent dispatch, scheduled
/// Task/wake messages, system messages) too, so filtering them out
/// here would make the ticker conclude "no work" during those turns
/// and the typing bubble would fade. Keep [`count_due`] as-is for the
/// existing callers that rely on its precise trigger=1 semantics; this
/// function is the right answer for "is there *any* work the runner
/// will pick up on its next poll".
pub fn count_pending_for_typing(conn: &Connection) -> Result<i64, DbError> {
    let now = Utc::now().to_rfc3339();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_in
         WHERE status = 'pending'
           AND (process_after IS NULL OR process_after <= ?1)",
        params![now],
        |r| r.get(0),
    )?;
    Ok(count)
}

pub fn mark_completed(conn: &Connection, id: MessageId) -> Result<(), DbError> {
    let n = conn.execute(
        "UPDATE messages_in SET status = 'completed' WHERE id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn mark_failed(conn: &Connection, id: MessageId) -> Result<(), DbError> {
    let n = conn.execute(
        "UPDATE messages_in SET status = 'failed' WHERE id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn get_pending(conn: &Connection, first_poll: bool, limit: i64) -> Result<Vec<MessageInRow>, DbError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, seq, kind, timestamp, status, process_after, recurrence,
                series_id, tries, trigger, platform_id, channel_type, thread_id,
                content, source_session_id, on_wake
         FROM messages_in
         WHERE status = 'pending'
           AND (process_after IS NULL OR process_after <= ?1)
           AND (on_wake = 0 OR ?2 = 1)
         ORDER BY seq DESC
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![now, i32::from(first_poll), limit],
        row_to_message_in,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn row_to_message_in(row: &Row<'_>) -> rusqlite::Result<MessageInRow> {
    let id_str: String = row.get("id")?;
    let id = uuid::Uuid::parse_str(&id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let kind: String = row.get("kind")?;
    let kind = match kind.as_str() {
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
                format!("unknown kind {other}").into(),
            ))
        }
    };
    let timestamp = parse_dt(row, "timestamp")?;
    let process_after = parse_dt_opt(row, "process_after")?;
    let content_str: String = row.get("content")?;
    let content: serde_json::Value = serde_json::from_str(&content_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let channel_type: Option<String> = row.get("channel_type")?;
    // Empty-string-as-Some defence (parallel to the SessionId branch in
    // `sessions.rs::row_to_session`): a legacy row with
    // `source_session_id = ''` would read as `Some("")` and propagate
    // into downstream code — the runner's apology emitter consumes it
    // as the literal empty session_id on the Agent-kind branch and
    // silently drops the message. Coalesce the empty string to `None`
    // here so every reader sees the same well-formed value.
    let source_session_id: Option<String> = row.get("source_session_id")?;
    let source_session_id = source_session_id.filter(|s| !s.is_empty());

    Ok(MessageInRow {
        id: MessageId(id),
        seq: row.get("seq")?,
        kind,
        timestamp,
        status: row.get("status")?,
        process_after,
        recurrence: row.get("recurrence")?,
        series_id: row.get("series_id")?,
        tries: {
            let v: i64 = row.get("tries")?;
            u32::try_from(v).unwrap_or(0)
        },
        trigger: {
            let v: i64 = row.get("trigger")?;
            v != 0
        },
        platform_id: row.get("platform_id")?,
        channel_type: channel_type.map(ChannelType::from),
        thread_id: row.get("thread_id")?,
        content,
        source_session_id,
        on_wake: {
            let v: i64 = row.get("on_wake")?;
            v != 0
        },
    })
}

fn parse_dt(row: &Row<'_>, col: &str) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = row.get(col)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn parse_dt_opt(row: &Row<'_>, col: &str) -> rusqlite::Result<Option<DateTime<Utc>>> {
    // Empty string is treated as missing. Adapters occasionally write
    // `Some("")` instead of `None` for optional timestamp columns; the
    // chrono parser returns `ParseError(TooShort)` on `""`, which used
    // to wedge the host's session reconciler in a hot-loop. Coalesce
    // to None and move on. Real RFC3339 strings still parse normally.
    let s: Option<String> = row.get(col)?;
    match s.as_deref() {
        None | Some("") => Ok(None),
        Some(ts) => DateTime::parse_from_rfc3339(ts)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open_inbound, SessionPaths};
    use ironclaw_types::{AgentGroupId, SessionId};
    use serde_json::json;

    fn fresh_inbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        (tmp, conn)
    }

    fn make_msg() -> WriteInbound {
        WriteInbound {
            id: MessageId::new(),
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            content: json!({"text":"hi"}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some("chat-1".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            source_session_id: None,
        }
    }

    #[test]
    fn insert_returns_even_seq() {
        let (_tmp, conn) = fresh_inbound();
        let seq1 = insert(&conn, &make_msg()).unwrap();
        let seq2 = insert(&conn, &make_msg()).unwrap();
        assert_eq!(seq1 % 2, 0, "expected even, got {seq1}");
        assert_eq!(seq2 % 2, 0, "expected even, got {seq2}");
        assert!(seq2 > seq1);
    }

    #[test]
    fn count_due_excludes_non_trigger() {
        let (_tmp, conn) = fresh_inbound();
        let mut m = make_msg();
        m.trigger = false;
        insert(&conn, &m).unwrap();
        assert_eq!(count_due(&conn).unwrap(), 0);
        insert(&conn, &make_msg()).unwrap();
        assert_eq!(count_due(&conn).unwrap(), 1);
    }

    #[test]
    fn count_due_respects_process_after() {
        let (_tmp, conn) = fresh_inbound();
        let mut m = make_msg();
        m.process_after = Some(Utc::now() + chrono::Duration::seconds(60));
        insert(&conn, &m).unwrap();
        assert_eq!(count_due(&conn).unwrap(), 0);
    }

    #[test]
    fn get_pending_returns_inserted() {
        let (_tmp, conn) = fresh_inbound();
        let msg = make_msg();
        let expected_id = msg.id;
        insert(&conn, &msg).unwrap();
        let rows = get_pending(&conn, true, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, expected_id);
        assert_eq!(rows[0].kind, MessageKind::Chat);
    }

    #[test]
    fn get_pending_first_poll_includes_on_wake() {
        let (_tmp, conn) = fresh_inbound();
        let mut m = make_msg();
        m.on_wake = true;
        insert(&conn, &m).unwrap();
        assert_eq!(get_pending(&conn, false, 10).unwrap().len(), 0);
        assert_eq!(get_pending(&conn, true, 10).unwrap().len(), 1);
    }

    #[test]
    fn mark_completed_transitions() {
        let (_tmp, conn) = fresh_inbound();
        let msg = make_msg();
        let id = msg.id;
        insert(&conn, &msg).unwrap();
        mark_completed(&conn, id).unwrap();
        assert_eq!(count_due(&conn).unwrap(), 0);
        let pending = get_pending(&conn, true, 10).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn mark_failed_transitions() {
        let (_tmp, conn) = fresh_inbound();
        let msg = make_msg();
        let id = msg.id;
        insert(&conn, &msg).unwrap();
        mark_failed(&conn, id).unwrap();
        assert_eq!(count_due(&conn).unwrap(), 0);
    }

    #[test]
    fn mark_missing_is_not_found() {
        let (_tmp, conn) = fresh_inbound();
        let err = mark_completed(&conn, MessageId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn count_pending_for_typing_includes_non_trigger() {
        // The typing-ticker uses `count_pending_for_typing` instead of
        // `count_due` so non-trigger rows (agent-to-agent dispatch,
        // Task/wake from the scheduler, system messages) still keep
        // the typing indicator alive while the runner processes them.
        let (_tmp, conn) = fresh_inbound();
        let mut m = make_msg();
        m.trigger = false;
        insert(&conn, &m).unwrap();
        assert_eq!(
            count_due(&conn).unwrap(),
            0,
            "count_due must keep its trigger=1 semantics",
        );
        assert_eq!(
            count_pending_for_typing(&conn).unwrap(),
            1,
            "count_pending_for_typing must include trigger=0 rows",
        );
    }

    #[test]
    fn count_pending_for_typing_respects_process_after() {
        let (_tmp, conn) = fresh_inbound();
        let mut m = make_msg();
        m.process_after = Some(Utc::now() + chrono::Duration::seconds(60));
        insert(&conn, &m).unwrap();
        assert_eq!(count_pending_for_typing(&conn).unwrap(), 0);
    }

    #[test]
    fn row_to_message_in_coalesces_empty_source_session_id() {
        // A row written by a legacy code path with `source_session_id = ''`
        // (instead of NULL) must read back as `None` so the runner's
        // apology emitter on the Agent-kind branch doesn't consume the
        // empty string as a literal session_id and silently drop the
        // message.
        let (_tmp, conn) = fresh_inbound();
        let msg = make_msg();
        insert(&conn, &msg).unwrap();
        // Forcibly stamp the row's source_session_id to '' to simulate
        // the legacy shape.
        conn.execute(
            "UPDATE messages_in SET source_session_id = '' WHERE id = ?1",
            params![msg.id.as_uuid().to_string()],
        )
        .unwrap();
        let rows = get_pending(&conn, true, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].source_session_id, None,
            "Some(\"\") must coalesce to None at the row-parsing boundary",
        );
    }
}
