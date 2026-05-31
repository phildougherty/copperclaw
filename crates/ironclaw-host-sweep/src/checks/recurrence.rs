//! Recurrence fan-out.
//!
//! For each `messages_in` row with a non-null `recurrence`, find the most
//! recent member of the series (by `seq`). If that row's
//! `process_after <= now`, parse the cron string via
//! `ironclaw_modules::scheduling::parse_when` / `compute_next_fire` and
//! insert a fresh `messages_in` row with the computed `next_fire`.
//!
//! The new row's `series_id` matches the parent's `series_id` if present,
//! otherwise it is set to the parent's `message_id` string so the host can
//! still correlate fan-outs back to their source row. The new row inherits
//! the parent's kind, content, trigger / `on_wake` flags, recurrence,
//! platform metadata, and resets `tries` to 0 (a new fan-out is a fresh
//! attempt, not a retry of a stuck one).

use crate::error::SweepError;
use crate::service::{SeriesFanout, SessionRoot};
use chrono::{DateTime, Utc};
use ironclaw_db::tables::messages_in::{insert, WriteInbound};
use ironclaw_modules::scheduling::{compute_next_fire, parse_when};
use ironclaw_types::{AgentGroupId, ChannelType, MessageId, MessageKind, SessionId};
use rusqlite::Connection;

#[derive(Debug, Clone)]
struct RecurrenceParent {
    id: MessageId,
    kind: MessageKind,
    recurrence: String,
    series_id: Option<String>,
    trigger: bool,
    on_wake: bool,
    platform_id: Option<String>,
    channel_type: Option<ChannelType>,
    thread_id: Option<String>,
    content: serde_json::Value,
    source_session_id: Option<String>,
    // Carried onto each fan-out so the runner's per-turn context block
    // sees the same venue shape on every member of a recurring series.
    reply_to: Option<String>,
    is_group: Option<bool>,
}

/// Returns one [`SeriesFanout`] per series that fired during this pass.
pub fn check(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Result<Vec<SeriesFanout>, SweepError> {
    let mut inbound = root.inbound_pool(agent_group_id, session_id)?;
    let parents = newest_per_series(inbound.conn(), now)?;
    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let next_fire = match compute_next(&parent.recurrence, now) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(_e) => {
                // Skip rows with unparseable cron. We could push these
                // into a separate error list, but the host doesn't need
                // structured visibility on each malformed row.
                tracing::warn!(
                    target: "ironclaw_host_sweep::recurrence",
                    recurrence = %parent.recurrence,
                    "skipping series with unparseable recurrence",
                );
                continue;
            }
        };

        let new_id = MessageId::new();
        let series_id = parent
            .series_id
            .clone()
            .unwrap_or_else(|| parent.id.as_uuid().to_string());

        let write = WriteInbound {
            id: new_id,
            kind: parent.kind,
            timestamp: now,
            content: parent.content.clone(),
            trigger: parent.trigger,
            on_wake: parent.on_wake,
            process_after: Some(next_fire),
            recurrence: Some(parent.recurrence.clone()),
            series_id: Some(series_id.clone()),
            platform_id: parent.platform_id.clone(),
            channel_type: parent.channel_type.clone(),
            thread_id: parent.thread_id.clone(),
            source_session_id: parent.source_session_id.clone(),
            // Carry the parent's reply_to/is_group signal onto every
            // recurrence fan-out so the runner's context-block sees the
            // same venue shape across a recurring series.
            reply_to: parent.reply_to.clone(),
            is_group: parent.is_group,
        };
        insert(inbound.conn_mut(), &write)?;
        out.push(SeriesFanout {
            series_id,
            new_message_id: new_id,
            next_fire,
        });
    }
    Ok(out)
}

fn compute_next(
    recurrence: &str,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, SweepError> {
    let when = parse_when(recurrence).map_err(|e| SweepError::ScheduleParse(e.to_string()))?;
    Ok(compute_next_fire(&when, now, Some(recurrence)))
}

/// Find the newest member of each series whose `process_after` is at or
/// before `now`. A series is identified by `series_id` when present and by
/// the row's own `id` otherwise (so the very first member of a series is
/// still correlatable).
fn newest_per_series(
    conn: &Connection,
    now: DateTime<Utc>,
) -> Result<Vec<RecurrenceParent>, SweepError> {
    let now_str = now.to_rfc3339();
    // Pick the row with the largest `seq` per series_id-or-id key.
    let mut stmt = conn.prepare(
        "SELECT id, kind, process_after, recurrence, series_id,
                trigger, on_wake, platform_id, channel_type, thread_id,
                content, source_session_id, reply_to, is_group, seq,
                COALESCE(series_id, id) AS series_key
         FROM messages_in
         WHERE recurrence IS NOT NULL AND TRIM(recurrence) != ''
         ORDER BY series_key, seq DESC",
    )?;
    let mut rows = stmt.query([])?;

    let mut seen_keys = std::collections::HashSet::new();
    let mut parents = Vec::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get("series_key")?;
        if !seen_keys.insert(key) {
            continue;
        }
        let process_after_str: Option<String> = row.get("process_after")?;
        let process_after = match process_after_str.as_deref() {
            None => None,
            Some(s) => Some(
                DateTime::parse_from_rfc3339(s)
                    .map_err(|e| SweepError::ScheduleParse(e.to_string()))?
                    .with_timezone(&Utc),
            ),
        };
        // Only fan out the parent if it is at-or-past its own fire time.
        if let Some(pa) = process_after {
            if pa.to_rfc3339() > now_str {
                continue;
            }
        }

        let id_str: String = row.get("id")?;
        let id_uuid = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| SweepError::ScheduleParse(format!("bad uuid {id_str}: {e}")))?;
        let kind_str: String = row.get("kind")?;
        // Delegate to the canonical `MessageKind::parse_str` so this
        // call site does NOT need updating every time a new variant
        // lands (slice-3 added Breadcrumb / Diff / TodoList / Error /
        // Thinking on top of the original six). A drift here aborts
        // the whole session's recurrence sweep with an
        // `unknown kind` error — strictly worse than letting the row
        // ride for one pass.
        let kind = MessageKind::parse_str(&kind_str).ok_or_else(|| {
            SweepError::ScheduleParse(format!("unknown kind `{kind_str}`"))
        })?;
        let recurrence: String = row.get("recurrence")?;
        let series_id: Option<String> = row.get("series_id")?;
        let trigger_i: i64 = row.get("trigger")?;
        let on_wake_i: i64 = row.get("on_wake")?;
        let platform_id: Option<String> = row.get("platform_id")?;
        let channel_type_str: Option<String> = row.get("channel_type")?;
        let thread_id: Option<String> = row.get("thread_id")?;
        let content_str: String = row.get("content")?;
        let content: serde_json::Value = serde_json::from_str(&content_str)
            .map_err(|e| SweepError::ScheduleParse(format!("bad content json: {e}")))?;
        let source_session_id: Option<String> = row.get("source_session_id")?;
        let reply_to: Option<String> = row.get("reply_to")?;
        let is_group: Option<bool> = {
            let v: Option<i64> = row.get("is_group")?;
            v.map(|n| n != 0)
        };

        parents.push(RecurrenceParent {
            id: MessageId::from(id_uuid),
            kind,
            recurrence,
            series_id,
            trigger: trigger_i != 0,
            on_wake: on_wake_i != 0,
            platform_id,
            channel_type: channel_type_str.map(ChannelType::from),
            thread_id,
            content,
            source_session_id,
            reply_to,
            is_group,
        });
    }
    Ok(parents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        insert_recurring_inbound, seed_running_session, MemSessionRoot,
    };
    use chrono::{Duration as ChDuration, TimeZone};
    use ironclaw_db::central::CentralDb;

    fn fixture() -> (MemSessionRoot, ironclaw_types::Session, DateTime<Utc>) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        (root, sess, now)
    }

    #[test]
    fn empty_messages_in_returns_empty() {
        let (root, sess, now) = fixture();
        let _ = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parent_in_the_future_does_not_fan_out() {
        let (root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-a".into()),
            Some(now + ChDuration::hours(1)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parent_at_or_past_fire_time_fans_out() {
        let (root, sess, now) = fixture();
        let parent = insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-a".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].series_id, "series-a");
        assert_ne!(r[0].new_message_id, parent);
        // Next fire computed from cron should be in the future.
        assert!(r[0].next_fire > now);
    }

    #[test]
    fn null_series_id_defaults_to_parent_id() {
        let (root, sess, now) = fixture();
        let parent = insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            None,
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].series_id, parent.as_uuid().to_string());
    }

    #[test]
    fn only_newest_member_of_series_fires() {
        let (root, sess, now) = fixture();
        // Two members of the same series; only one fan-out expected.
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-b".into()),
            Some(now - ChDuration::hours(4)),
        );
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-b".into()),
            Some(now - ChDuration::minutes(1)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].series_id, "series-b");
    }

    #[test]
    fn empty_recurrence_string_is_ignored() {
        let (root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "",
            Some("series-empty".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn unparseable_recurrence_is_skipped_without_error() {
        let (root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "not a valid cron",
            Some("series-bad".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn fan_out_row_inherits_kind_and_content() {
        let (root, sess, now) = fixture();
        let _parent = insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-c".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let conn = pool.conn_mut();
        let (kind_str, content_str, series_str): (String, String, Option<String>) = conn
            .query_row(
                "SELECT kind, content, series_id FROM messages_in WHERE id = ?1",
                rusqlite::params![r[0].new_message_id.as_uuid().to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind_str, "task");
        assert!(content_str.contains("recurring"));
        assert_eq!(series_str.as_deref(), Some("series-c"));
    }

    #[test]
    fn fan_out_row_resets_tries_to_zero() {
        let (root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-tries".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let tries: i64 = pool
            .conn_mut()
            .query_row(
                "SELECT tries FROM messages_in WHERE id = ?1",
                rusqlite::params![r[0].new_message_id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tries, 0);
    }

    #[test]
    fn fan_out_with_no_process_after_still_fires() {
        // A recurring row that has never been fired (process_after IS NULL)
        // should produce a fan-out immediately.
        let (root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-null-pa".into()),
            None,
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
    }

    /// Regression: the original `parse_kind` hand-rolled only six
    /// variants (chat/task/webhook/system/agent/card) and aborted the
    /// whole session's recurrence sweep with `unknown kind` whenever a
    /// recurring row of a newer kind landed (slice-3 added breadcrumb,
    /// diff, `todo_list`, error, thinking). Now that we delegate to
    /// `MessageKind::parse_str`, every documented variant must round-
    /// trip cleanly. We seed one recurring row per variant and assert
    /// the check completes (no Err) and fans out the expected count.
    #[test]
    fn all_message_kinds_parse_without_error_in_recurrence_sweep() {
        for kind in [
            MessageKind::Chat,
            MessageKind::Task,
            MessageKind::Webhook,
            MessageKind::System,
            MessageKind::Agent,
            MessageKind::Card,
            MessageKind::Breadcrumb,
            MessageKind::Diff,
            MessageKind::TodoList,
            MessageKind::Error,
            MessageKind::Thinking,
        ] {
            let (root, sess, now) = fixture();
            // Hand-rolled insert so we can pick the kind directly —
            // the shared `insert_recurring_inbound` helper hard-codes
            // `Task`, which would defeat the test.
            let id = ironclaw_types::MessageId::new();
            let write = WriteInbound {
                id,
                kind,
                timestamp: now,
                content: serde_json::json!({"text": "recurring"}),
                trigger: true,
                on_wake: false,
                process_after: Some(now - ChDuration::minutes(5)),
                recurrence: Some("0 */2 * * *".into()),
                series_id: Some(format!("series-kind-{}", kind.as_str())),
                platform_id: None,
                channel_type: None,
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            };
            {
                let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
                insert(pool.conn_mut(), &write).unwrap();
            }
            let r = check(&root, &sess.agent_group_id, &sess.id, now)
                .unwrap_or_else(|e| panic!("kind `{}` aborted sweep: {e}", kind.as_str()));
            assert_eq!(
                r.len(),
                1,
                "expected exactly one fan-out for kind `{}`",
                kind.as_str(),
            );
        }
    }
}
