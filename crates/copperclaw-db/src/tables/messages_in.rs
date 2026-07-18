//! Writes and reads against per-session `inbound.db::messages_in`.
//!
//! Host-owned writer; the container reads through `open_inbound_ro_no_mmap`.

use crate::DbError;
use chrono::{DateTime, Utc};
use copperclaw_types::{ChannelType, MessageId, MessageInRow, MessageKind};
use rusqlite::{Connection, OptionalExtension, Row, params};

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
    /// Platform-side parent message id when the wire event was a reply.
    /// `None` when the originating message is a top-level send or the
    /// channel doesn't carry a reply link. See `MessageInRow::reply_to`.
    pub reply_to: Option<String>,
    /// Whether the originating venue is a group chat (vs. a 1-on-1 DM).
    /// `None` when the channel doesn't distinguish. See
    /// `MessageInRow::is_group`.
    pub is_group: Option<bool>,
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
            content, source_session_id, on_wake, reply_to, is_group)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
    } else {
        "INSERT INTO messages_in
           (id, seq, kind, timestamp, status, process_after, recurrence,
            series_id, tries, trigger, platform_id, channel_type, thread_id,
            content, source_session_id, on_wake, reply_to, is_group)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
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
            &msg.reply_to,
            msg.is_group.map(i32::from),
        ],
    )?;
    if rows == 0 { Ok(0) } else { Ok(seq) }
}

fn next_even_seq(conn: &Connection) -> Result<i64, DbError> {
    let max: Option<i64> = conn
        .query_row("SELECT MAX(seq) FROM messages_in", [], |r| r.get(0))
        .optional()?
        .flatten();
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

/// Increment the per-message crash counter for poison-message quarantine
/// (F2) and return the new value. Called by the host's crash-restart path
/// once per in-flight message per crash: a message that has been in flight
/// during `crash_attempts >= K` crashes is quarantined (marked terminally
/// `status='failed'` via [`mark_failed`]) so a poison inbound that reliably
/// crashes the runner stops being re-claimed on every respawn.
///
/// Deliberately distinct from `tries` (which the host-sweep apology path
/// overloads with the `APOLOGY_TRIES_MARKER = 99` sentinel): the crash
/// counter must increment cleanly from 0 without colliding with that magic
/// value. Returns [`DbError::NotFound`] if the row is gone.
pub fn increment_crash_attempts(conn: &Connection, id: MessageId) -> Result<i64, DbError> {
    let n = conn.execute(
        "UPDATE messages_in SET crash_attempts = crash_attempts + 1 WHERE id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    let count: i64 = conn.query_row(
        "SELECT crash_attempts FROM messages_in WHERE id = ?1",
        params![id.as_uuid().to_string()],
        |r| r.get(0),
    )?;
    Ok(count)
}

/// Highest `seq` currently in the table, or `0` if empty. Used by the
/// M18 R2 mid-turn steering check: the runner snapshots this once at
/// `drive_turn` entry as the "known at turn start" ceiling, then any row
/// peeked with `seq` above it arrived strictly after the turn began.
pub fn max_seq(conn: &Connection) -> Result<i64, DbError> {
    let max: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM messages_in", [], |r| {
        r.get(0)
    })?;
    Ok(max)
}

/// Pending rows inserted strictly after `min_seq`, oldest first. The M18
/// R2 mid-turn steering check calls this between tool batches to detect a
/// `/stop` control row or a human interjection that arrived while the
/// turn was already running — same due-now / non-wake filter as
/// [`get_pending`], but scoped by sequence instead of a row limit so the
/// caller sees every new arrival, not just the newest N.
pub fn get_new_since(conn: &Connection, min_seq: i64) -> Result<Vec<MessageInRow>, DbError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, seq, kind, timestamp, status, process_after, recurrence,
                series_id, tries, trigger, platform_id, channel_type, thread_id,
                content, source_session_id, on_wake, reply_to, is_group
         FROM messages_in
         WHERE status = 'pending'
           AND seq > ?1
           AND on_wake = 0
           AND (process_after IS NULL OR process_after <= ?2)
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(params![min_seq, now], row_to_message_in)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_pending(
    conn: &Connection,
    first_poll: bool,
    limit: i64,
) -> Result<Vec<MessageInRow>, DbError> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, seq, kind, timestamp, status, process_after, recurrence,
                series_id, tries, trigger, platform_id, channel_type, thread_id,
                content, source_session_id, on_wake, reply_to, is_group
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
    let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let kind: String = row.get("kind")?;
    let kind = MessageKind::parse_str(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown kind {kind}").into(),
        )
    })?;
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
        reply_to: {
            // Empty-string-as-Some defence (parallel to source_session_id
            // above): a legacy / adapter-misbehaving row with `reply_to = ''`
            // would propagate into the runner's context-block as an empty
            // " in reply to ''" fragment. Coalesce to None at the boundary.
            let v: Option<String> = row.get("reply_to")?;
            v.filter(|s| !s.is_empty())
        },
        is_group: {
            // SQLite stores the value as INTEGER 0/1, but the column is
            // nullable: `None` (channel doesn't distinguish) is distinct
            // from `Some(false)` (DM) and `Some(true)` (group).
            let v: Option<i64> = row.get("is_group")?;
            v.map(|n| n != 0)
        },
    })
}

fn parse_dt(row: &Row<'_>, col: &str) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = row.get(col)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
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
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionPaths, open_inbound};
    use copperclaw_types::{AgentGroupId, SessionId};
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
            reply_to: None,
            is_group: None,
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
    fn crash_attempts_starts_at_zero_and_increments() {
        // F2: the per-message crash counter defaults to 0 (healthy path)
        // and increments by 1 per call, returning the new value. Quarantine
        // (marking the row failed) is the host's decision at threshold K.
        let (_tmp, conn) = fresh_inbound();
        let msg = make_msg();
        let id = msg.id;
        insert(&conn, &msg).unwrap();
        // Fresh row: counter is 0 (never crashed).
        let start: i64 = conn
            .query_row(
                "SELECT crash_attempts FROM messages_in WHERE id = ?1",
                params![id.as_uuid().to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(start, 0);
        assert_eq!(increment_crash_attempts(&conn, id).unwrap(), 1);
        assert_eq!(increment_crash_attempts(&conn, id).unwrap(), 2);
        assert_eq!(increment_crash_attempts(&conn, id).unwrap(), 3);
    }

    #[test]
    fn increment_crash_attempts_missing_row_is_not_found() {
        let (_tmp, conn) = fresh_inbound();
        let err = increment_crash_attempts(&conn, MessageId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
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
    fn insert_round_trips_reply_to_and_is_group() {
        // The runner's "Conversation context" block reads these fields
        // off `MessageInRow`; persisting them and reading them back is
        // the gate that lets the block say "in a group chat" / "in
        // reply to the user's earlier message" instead of degrading to
        // the venue-shape-only phrasing.
        let (_tmp, conn) = fresh_inbound();
        let mut msg = make_msg();
        msg.reply_to = Some("parent-msg-42".into());
        msg.is_group = Some(true);
        insert(&conn, &msg).unwrap();

        let rows = get_pending(&conn, true, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reply_to.as_deref(), Some("parent-msg-42"));
        assert_eq!(rows[0].is_group, Some(true));

        // The Some(false) case is distinct from None — DMs explicitly
        // set is_group=false, channels that don't distinguish leave it
        // None.
        let mut dm = make_msg();
        dm.is_group = Some(false);
        insert(&conn, &dm).unwrap();
        let rows = get_pending(&conn, true, 10).unwrap();
        let dm_row = rows.iter().find(|r| r.id == dm.id).expect("dm row");
        assert_eq!(dm_row.is_group, Some(false));
        assert_eq!(dm_row.reply_to, None);
    }

    #[test]
    fn row_to_message_in_coalesces_empty_reply_to() {
        // Parallel to the source_session_id branch: an adapter that
        // writes `Some("")` instead of `None` must read back as `None`
        // so the runner's context-block doesn't render "in reply to ''".
        let (_tmp, conn) = fresh_inbound();
        let msg = make_msg();
        insert(&conn, &msg).unwrap();
        conn.execute(
            "UPDATE messages_in SET reply_to = '' WHERE id = ?1",
            params![msg.id.as_uuid().to_string()],
        )
        .unwrap();
        let rows = get_pending(&conn, true, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reply_to, None);
    }

    #[test]
    fn max_seq_is_zero_when_empty_and_tracks_the_high_water_mark() {
        let (_tmp, conn) = fresh_inbound();
        assert_eq!(max_seq(&conn).unwrap(), 0);
        let seq1 = insert(&conn, &make_msg()).unwrap();
        assert_eq!(max_seq(&conn).unwrap(), seq1);
        let second = make_msg();
        let second_id = second.id;
        let seq2 = insert(&conn, &second).unwrap();
        assert_eq!(max_seq(&conn).unwrap(), seq2);
        // Completing a row doesn't move the high-water mark backward.
        mark_completed(&conn, second_id).unwrap();
        assert_eq!(max_seq(&conn).unwrap(), seq2);
    }

    #[test]
    fn get_new_since_returns_only_rows_past_the_ceiling() {
        let (_tmp, conn) = fresh_inbound();
        let before = insert(&conn, &make_msg()).unwrap();
        let ceiling = max_seq(&conn).unwrap();
        assert_eq!(ceiling, before);
        assert!(get_new_since(&conn, ceiling).unwrap().is_empty());

        let mut after_msg = make_msg();
        after_msg.content = json!({"text": "arrived after"});
        let after_id = after_msg.id;
        insert(&conn, &after_msg).unwrap();

        let new_rows = get_new_since(&conn, ceiling).unwrap();
        assert_eq!(new_rows.len(), 1);
        assert_eq!(new_rows[0].id, after_id);
    }

    #[test]
    fn get_new_since_excludes_completed_and_on_wake_and_future_rows() {
        let (_tmp, conn) = fresh_inbound();
        let ceiling = max_seq(&conn).unwrap();

        let completed = make_msg();
        let completed_id = completed.id;
        insert(&conn, &completed).unwrap();
        mark_completed(&conn, completed_id).unwrap();

        let mut wake_only = make_msg();
        wake_only.on_wake = true;
        insert(&conn, &wake_only).unwrap();

        let mut future = make_msg();
        future.process_after = Some(Utc::now() + chrono::Duration::seconds(60));
        insert(&conn, &future).unwrap();

        let mut due = make_msg();
        due.content = json!({"text": "this one counts"});
        let due_id = due.id;
        insert(&conn, &due).unwrap();

        let new_rows = get_new_since(&conn, ceiling).unwrap();
        assert_eq!(new_rows.len(), 1, "only the due, non-wake, pending row");
        assert_eq!(new_rows[0].id, due_id);
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
