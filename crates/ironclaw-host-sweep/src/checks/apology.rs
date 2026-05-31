//! Stuck-inbound apology emit.
//!
//! Detects chat inbounds that have been sitting in `messages_in.status =
//! 'pending'` longer than [`crate::APOLOGY_AFTER_SECS`] without progress,
//! and emits a single user-visible "I'm having trouble" chat row to the
//! session's `outbound.db` so the user knows the bot isn't dead. The
//! inbound's `tries` column is bumped to [`APOLOGY_TRIES_MARKER`] as a
//! dedupe marker so the same inbound never gets a second apology — see
//! the module-level note on dedupe below.
//!
//! ## Dedupe mechanism (no new column)
//!
//! Per the task scope, we explicitly do NOT add a new column to
//! `messages_in`. Instead we reuse the existing `tries` column with a
//! magic sentinel value [`APOLOGY_TRIES_MARKER`] (= 99). The host's own
//! retry path never sets `tries` that high (the hard cap is
//! `MAX_TRIES = 5`), so the value is safe to overload.
//!
//! ## Two failure paths, one apology
//!
//! - `reason=pending_too_long` — the inbound has aged past the 5-min
//!   threshold (covers heartbeat-stale, panicked runner, jammed loop,
//!   etc).
//! - `reason=container_spawn_failed` — the session's
//!   `container_status=stopped` AND the container manager has burned at
//!   least [`crate::spawn_tracker::SPAWN_FAIL_THRESHOLD`] failed
//!   `runtime.spawn(...)` attempts. The pending inbound can be any age;
//!   if the container can't come up at all, the user deserves an apology
//!   sooner than the 5-min mark.
//!
//! Both paths emit exactly one apology row per inbound and stamp
//! `tries=99` so subsequent sweep passes skip the row.

use crate::error::SweepError;
use crate::service::SessionRoot;
use crate::spawn_tracker::SpawnAttemptTracker;
use crate::APOLOGY_AFTER_SECS;
use chrono::{DateTime, Utc};
use ironclaw_db::tables::messages_out::{insert as insert_out, WriteOutbound};
use ironclaw_channels_core::{ErrorCard, ErrorCardKind};
use ironclaw_types::{ChannelType, ContainerStatus, MessageId, MessageKind, Session};
use rusqlite::params;
#[cfg(test)]
use rusqlite::OptionalExtension;

/// Dedupe sentinel written back into `messages_in.tries`. The runner's
/// retry path tops out at `MAX_TRIES=5`, so 99 is safely out-of-band.
/// Documented here so a future reader doesn't add a new column "for
/// safety".
pub const APOLOGY_TRIES_MARKER: i64 = 99;

/// Hard cap on the number of stuck inbounds the apology check scans
/// per session in one pass. A user could in theory accumulate dozens
/// of pending rows during an outage; emitting an apology for each is
/// still bounded so we don't choke on a very large backlog.
const APOLOGY_SCAN_LIMIT: i64 = 50;

/// User-facing text. Kept verbatim per the task brief — no operator
/// jargon ("OCI runtime error", "heartbeat stale") leaks into the user
/// view. The operator-facing detail lives in the log line + metric.
pub(crate) const APOLOGY_TEXT: &str =
    "I'm having trouble processing your message right now (the agent's container isn't responding). \
     The operator has been notified. Please try again in a few minutes.";

/// One row visible to the apology check inside `messages_in`. Mirrors
/// the subset of [`ironclaw_types::MessageInRow`] we actually look at.
#[derive(Debug, Clone)]
struct StuckInboundRow {
    id: MessageId,
    age_secs: i64,
    channel_type: Option<ChannelType>,
    platform_id: Option<String>,
    thread_id: Option<String>,
    /// Set when the inbound was written by `agent_dispatch` (a child
    /// reporting up to its parent). When channel routing is absent,
    /// the apology cascade walks this UUID via an Agent-kind outbound
    /// so the parent agent learns the child failed.
    source_session_id: Option<String>,
}

/// Result of one apology emit. Returned so the sweep can count them
/// per pass and put them in tracing fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApologyEmit {
    pub message_id: MessageId,
    pub reason: ApologyReason,
}

/// Which failure mode triggered the apology. Maps 1:1 to the `reason`
/// label on the `ironclaw_stuck_inbound_apology_total` metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApologyReason {
    /// Inbound sat at `status=pending` longer than
    /// [`crate::APOLOGY_AFTER_SECS`].
    PendingTooLong,
    /// Session's container has failed to spawn enough times to be
    /// declared stuck (`container_status=stopped` plus
    /// [`crate::spawn_tracker::SPAWN_FAIL_THRESHOLD`] attempts).
    ContainerSpawnFailed,
}

impl ApologyReason {
    /// The Prometheus label value matching this reason.
    pub const fn metric_label(self) -> &'static str {
        match self {
            Self::PendingTooLong => ironclaw_metrics::STUCK_REASON_PENDING_TOO_LONG,
            Self::ContainerSpawnFailed => ironclaw_metrics::STUCK_REASON_CONTAINER_SPAWN_FAILED,
        }
    }
}

/// Run the apology check against one session.
///
/// Returns the list of inbounds for which a fresh apology row was written.
/// An empty result is the normal case — the bulk of sweeps find nothing.
///
/// The check is split into two sub-rules:
///
/// 1. For every stuck chat inbound (age > `APOLOGY_AFTER_SECS`,
///    `tries < APOLOGY_TRIES_MARKER`): emit, mark, count.
/// 2. If the session is also in `container_status=Stopped` AND the
///    spawn tracker has exhausted its budget, reason → `ContainerSpawnFailed`;
///    otherwise reason → `PendingTooLong`.
//
// `check` carries the three-branch apology builder (channel-routed
// ErrorCard, agent-routed plain text, give-up marker) inline. Splitting
// to silence `too_many_lines` would just push the per-row locals into
// a struct for no readability win.
#[allow(clippy::too_many_lines)]
pub fn check(
    root: &dyn SessionRoot,
    spawn_tracker: &SpawnAttemptTracker,
    session: &Session,
    now: DateTime<Utc>,
) -> Result<Vec<ApologyEmit>, SweepError> {
    let mut emits = Vec::new();
    let spawn_failed = matches!(session.container_status, ContainerStatus::Stopped)
        && spawn_tracker.is_exhausted(session.id);

    // For the spawn-failed branch we still respect the time threshold
    // because emitting an apology the instant a session lands in
    // `stopped` would be premature — the manager's tick has to actually
    // try three spawns first. The tracker exhaustion gives us that
    // signal. The `pending_too_long` branch is separately gated on
    // age > APOLOGY_AFTER_SECS.
    //
    // We let the spawn-failed branch fire at any age (no minimum) only
    // when the tracker is already exhausted — that means the manager
    // has burned 3+ ticks (3+ minutes) trying. By construction the
    // tracker can't reach SPAWN_FAIL_THRESHOLD faster than that.

    let rows = scan_stuck_inbounds(root, session, now)?;
    if rows.is_empty() {
        return Ok(emits);
    }

    let mut inbound_pool =
        root.inbound_pool(&session.agent_group_id, &session.id)?;
    let mut outbound_pool =
        root.outbound_pool(&session.agent_group_id, &session.id)?;
    let agent_group_id_str = session.agent_group_id.as_uuid().to_string();

    let container_is_running = matches!(session.container_status, ContainerStatus::Running);

    for row in rows {
        let reason = if spawn_failed {
            ApologyReason::ContainerSpawnFailed
        } else if row.age_secs >= i64::from(APOLOGY_AFTER_SECS) {
            ApologyReason::PendingTooLong
        } else {
            // Not stuck long enough yet and the container isn't even
            // declared spawn-failed — skip.
            continue;
        };

        // Liveness gate. The pending-too-long branch fires on message
        // AGE alone, which previously produced false positives during
        // legitimate long work: a model spending 6 minutes building a
        // prototype writes a fresh heartbeat each tick and bumps a
        // `processing_ack` row to `processing`, but the inbound stays
        // `status=pending` until finalize_messages flips it at the
        // very end. The age-only check therefore saw a "stuck" row
        // even though the runner was actively making forward
        // progress. Lived through on 2026-05-24 with a Telegram
        // session that got a false "I'm having trouble" toast 5
        // minutes into a multi-file build (heartbeat 1s old, golfflow
        // dir modified just now, ~30 rapid usage_report rows in
        // outbound).
        //
        // The gate: if the container is Running AND the row's
        // processing_ack is `processing`, the runner is on it.
        // Skip the apology. The spawn-failed branch is exempt
        // because by definition the container is Stopped there.
        if matches!(reason, ApologyReason::PendingTooLong)
            && container_is_running
            && inbound_is_being_processed(outbound_pool.conn(), row.id)?
        {
            tracing::debug!(
                target: "ironclaw_host_sweep",
                session = %session.id,
                message = %row.id,
                age_secs = row.age_secs,
                "stuck-inbound apology suppressed: runner is actively processing",
            );
            continue;
        }

        // Pick the apology shape. Three cases:
        //   (a) Inbound has channel_type + platform_id — chat-kind
        //       apology back through the same channel (root sessions
        //       and child sessions wired directly to a user channel).
        //   (b) Inbound has NO channel routing but has `source_session_id`
        //       — Agent-kind apology back UP the chain, so the parent
        //       (or whoever owns the source session) sees a system
        //       message that we failed. Lets the parent surface the
        //       failure to the user instead of silently swallowing it.
        //   (c) Neither — give up, mark tries=99 to stop re-evaluating.
        let apology = if let (Some(channel_type), Some(platform_id)) =
            (row.channel_type.clone(), row.platform_id.clone())
        {
            // Mirror the runner's terminal-failure apology shape
            // (slice-3.3 ErrorCard) so a user whose container went
            // unresponsive sees the same red error chip as one whose
            // provider call timed out. The two failure modes are
            // operationally distinct but user-cosmetically identical.
            let card = ErrorCard {
                title: "I couldn't process your message".to_string(),
                summary: APOLOGY_TEXT.to_string(),
                kind: ErrorCardKind::Internal,
                details: Some(
                    "agent container unresponsive (heartbeat stale or never spawned); \
                     operator notified via log + metrics"
                        .to_string(),
                ),
                retryable: true,
            };
            WriteOutbound {
                id: MessageId::new(),
                in_reply_to: Some(row.id),
                timestamp: now,
                deliver_after: None,
                recurrence: None,
                kind: MessageKind::Error,
                channel_type: Some(channel_type),
                platform_id: Some(platform_id),
                thread_id: row.thread_id,
                content: serde_json::json!({
                    "error": serde_json::to_value(&card)
                        .expect("ErrorCard is always serialisable"),
                }),
            }
        } else if let Some(source) = row.source_session_id.as_deref() {
            // Build an Agent-kind apology targeted at the source
            // session. The host's `agent_dispatch` handler writes it
            // into that session's inbound on the next delivery sweep.
            WriteOutbound {
                id: MessageId::new(),
                in_reply_to: None,
                timestamp: now,
                deliver_after: None,
                recurrence: None,
                kind: MessageKind::Agent,
                channel_type: None,
                platform_id: None,
                thread_id: None,
                content: serde_json::json!({
                    "text": APOLOGY_TEXT,
                    "to": { "kind": "agent", "session_id": source },
                }),
            }
        } else {
            mark_tries_apology_sent(inbound_pool.conn_mut(), row.id)?;
            tracing::debug!(
                target: "ironclaw_host_sweep",
                session = %session.id,
                message = %row.id,
                "apology skipped: inbound has no channel routing and no source_session_id",
            );
            continue;
        };

        if let Err(err) = insert_out(outbound_pool.conn_mut(), &apology) {
            tracing::warn!(
                target: "ironclaw_host_sweep",
                session = %session.id,
                message = %row.id,
                error = %err,
                "could not write apology outbound row",
            );
            continue;
        }

        mark_tries_apology_sent(inbound_pool.conn_mut(), row.id)?;
        ironclaw_metrics::inc_stuck_inbound_apology(&agent_group_id_str, reason.metric_label());
        tracing::info!(
            target: "ironclaw_host_sweep",
            session = %session.id,
            agent_group = %session.agent_group_id,
            message = %row.id,
            reason = ?reason,
            "emitted stuck-inbound apology",
        );

        emits.push(ApologyEmit {
            message_id: row.id,
            reason,
        });
    }

    Ok(emits)
}

/// Read the per-session inbound DB for chat rows that look stuck:
/// `status=pending`, `kind=chat`, `tries < APOLOGY_TRIES_MARKER`,
/// `(now - timestamp) > APOLOGY_AFTER_SECS - some slack`.
///
/// We deliberately fetch even rows that are *just barely* under the
/// time threshold because the caller still needs them when the session
/// is in the spawn-failed branch (which doesn't require age > threshold).
/// The caller filters again at apology-emit time.
fn scan_stuck_inbounds(
    root: &dyn SessionRoot,
    session: &Session,
    now: DateTime<Utc>,
) -> Result<Vec<StuckInboundRow>, SweepError> {
    let pool = root.inbound_pool(&session.agent_group_id, &session.id)?;
    let mut stmt = pool.conn().prepare(
        "SELECT id, timestamp, channel_type, platform_id, thread_id, source_session_id
         FROM messages_in
         WHERE status = 'pending'
           AND kind = 'chat'
           AND tries < ?1
         ORDER BY timestamp ASC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![APOLOGY_TRIES_MARKER, APOLOGY_SCAN_LIMIT], |row| {
            let id_str: String = row.get("id")?;
            let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let ts_str: String = row.get("timestamp")?;
            let ts = DateTime::parse_from_rfc3339(&ts_str)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&Utc);
            let channel_type: Option<String> = row.get("channel_type")?;
            Ok(StuckInboundRow {
                id: MessageId(id),
                age_secs: (now - ts).num_seconds(),
                channel_type: channel_type.map(ChannelType::from),
                platform_id: row.get("platform_id")?,
                thread_id: row.get("thread_id")?,
                source_session_id: row.get("source_session_id")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Liveness gate for the pending-too-long apology branch. Returns
/// `true` when the runner has acked the inbound and is still working
/// on it (its `processing_ack` row carries `status='processing'`).
/// The sweep uses this to suppress the false-positive "I'm having
/// trouble" toast that the age-only check produced during legitimate
/// long work (multi-minute model turns + tool sequences).
///
/// `None` from `processing_ack::get` means the runner hasn't seen
/// the row yet (truly unacknowledged) — that IS suspicious past the
/// time threshold, so we fall through to the normal apology path.
/// Done/Failed ack states also fall through — by then the runner is
/// off the row entirely and the inbound shouldn't still be
/// `status=pending`; if it is, something else is wrong and the
/// apology is appropriate.
///
/// DB errors are surfaced as `SweepError::Db` rather than swallowed —
/// a broken outbound DB is a real problem and squashing it here would
/// hide it. The cost of bubbling is one extra `Result` return at this
/// site; the upside is the caller's `?` matches the rest of the
/// scan/emit chain.
fn inbound_is_being_processed(
    outbound_conn: &rusqlite::Connection,
    message_id: MessageId,
) -> Result<bool, SweepError> {
    use ironclaw_db::tables::processing_ack;
    match processing_ack::get(outbound_conn, message_id) {
        Ok(Some(claim)) => Ok(matches!(
            claim.status,
            processing_ack::ProcessingStatus::Processing,
        )),
        Ok(None) => Ok(false),
        Err(err) => Err(SweepError::from(err)),
    }
}

/// Stamp `tries = APOLOGY_TRIES_MARKER` on a single inbound to dedupe
/// subsequent apology emits. The row is left at `status=pending` so the
/// runner can still pick it up if the container ever recovers — the
/// host doesn't decide for the runner that the inbound is dead.
fn mark_tries_apology_sent(
    conn: &mut rusqlite::Connection,
    id: MessageId,
) -> Result<(), SweepError> {
    conn.execute(
        "UPDATE messages_in SET tries = ?1 WHERE id = ?2",
        params![APOLOGY_TRIES_MARKER, id.as_uuid().to_string()],
    )?;
    Ok(())
}

/// Defensive helper: confirms the marker has been set on the given row.
/// Useful for tests that want to assert the dedupe path fired without
/// re-querying the inbound DB in three places.
#[cfg(test)]
fn apology_marker_present(
    conn: &rusqlite::Connection,
    id: MessageId,
) -> Result<bool, rusqlite::Error> {
    let tries: Option<i64> = conn
        .query_row(
            "SELECT tries FROM messages_in WHERE id = ?1",
            params![id.as_uuid().to_string()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(tries == Some(APOLOGY_TRIES_MARKER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_running_session, MemSessionRoot};
    use crate::APOLOGY_AFTER_SECS;
    use chrono::{Duration as ChDuration, TimeZone};
    use ironclaw_db::central::CentralDb;
    use ironclaw_db::tables::messages_in::{insert as insert_in, WriteInbound};
    use ironclaw_db::tables::messages_out;
    use ironclaw_db::tables::sessions as sessions_tbl;
    use ironclaw_types::{ChannelType, MessageKind, Session};

    fn fixture() -> (
        CentralDb,
        MemSessionRoot,
        Session,
        DateTime<Utc>,
        SpawnAttemptTracker,
    ) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let tracker = SpawnAttemptTracker::new();
        (central, root, session, now, tracker)
    }

    fn insert_stuck_chat(
        root: &MemSessionRoot,
        session: &Session,
        age: ChDuration,
        platform_id: &str,
        channel_type: &str,
        thread_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> MessageId {
        let id = MessageId::new();
        let msg = WriteInbound {
            id,
            kind: MessageKind::Chat,
            timestamp: now - age,
            content: serde_json::json!({"text":"please respond"}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some(platform_id.into()),
            channel_type: Some(ChannelType::new(channel_type)),
            thread_id: thread_id.map(str::to_string),
            source_session_id: None,
            reply_to: None,
            is_group: None,
        };
        let mut pool = root.inbound_pool(&session.agent_group_id, &session.id).unwrap();
        insert_in(pool.conn_mut(), &msg).unwrap();
        id
    }

    #[test]
    fn no_pending_rows_no_emits() {
        let (_c, root, sess, now, tracker) = fixture();
        // Touch the inbound pool so the table exists for the SELECT.
        let _ = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert!(emits.is_empty());
    }

    /// Spec test #1: an inbound at `timestamp = now - 6min` with
    /// `status=pending` produces exactly one apology chat row in
    /// `outbound.db` after one sweep pass.
    #[test]
    fn stuck_inbound_apology_emits_after_5min() {
        let (_c, root, sess, now, tracker) = fixture();
        let id = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::minutes(6),
            "tg-123",
            "telegram",
            None,
            now,
        );

        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].message_id, id);
        assert_eq!(emits[0].reason, ApologyReason::PendingTooLong);

        // Outbound row landed with the right routing.
        let outbound = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let rows = messages_out::list_due(outbound.conn()).unwrap();
        let apology = rows
            .iter()
            .find(|r| r.kind == MessageKind::Error)
            .expect("expected an apology error row");
        assert_eq!(
            apology.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(apology.platform_id.as_deref(), Some("tg-123"));
        assert_eq!(apology.in_reply_to, Some(id));

        // User-facing text lives on the ErrorCard's `summary` field.
        let summary = apology
            .content
            .get("error")
            .and_then(|e| e.get("summary"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert!(
            summary.contains("trouble") && summary.contains("operator"),
            "apology summary should be user-facing: {summary:?}"
        );

        // Dedupe marker present so subsequent passes skip the row.
        let inbound = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        assert!(apology_marker_present(inbound.conn(), id).unwrap());
    }

    /// Spec test #2: an inbound at `now - 2min` produces no apology.
    #[test]
    fn apology_not_emitted_below_threshold() {
        let (_c, root, sess, now, tracker) = fixture();
        let _ = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::minutes(2),
            "tg-1",
            "telegram",
            None,
            now,
        );

        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert!(emits.is_empty(), "no apology expected below 5 min");

        // And no error apology row landed in outbound.
        let outbound = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let rows = messages_out::list_due(outbound.conn()).unwrap();
        let err_rows: Vec<_> = rows.iter().filter(|r| r.kind == MessageKind::Error).collect();
        assert!(err_rows.is_empty(), "no error outbound expected, got {err_rows:?}");
    }

    /// Spec test #3: a stuck inbound across two consecutive sweep
    /// passes still yields exactly one apology row (dedupe).
    #[test]
    fn apology_only_emitted_once() {
        let (_c, root, sess, now, tracker) = fixture();
        let _id = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::minutes(7),
            "tg-1",
            "telegram",
            None,
            now,
        );

        let first = check(&root, &tracker, &sess, now).unwrap();
        assert_eq!(first.len(), 1, "first sweep should emit one apology");

        let second = check(&root, &tracker, &sess, now).unwrap();
        assert!(second.is_empty(), "second sweep should emit nothing");

        let outbound = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let rows = messages_out::list_due(outbound.conn()).unwrap();
        let apologies: Vec<_> = rows.iter().filter(|r| r.kind == MessageKind::Error).collect();
        assert_eq!(apologies.len(), 1, "expected exactly one apology row");
    }

    /// Spec test #5: an apology routed at `(telegram, 12345)` lands
    /// with those exact channel-routing fields on the outbound row,
    /// so the delivery loop dispatches the reply back to the right chat.
    #[test]
    fn apology_routing_preserves_channel_fields() {
        let (_c, root, sess, now, tracker) = fixture();
        let _id = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::minutes(6),
            "12345",
            "telegram",
            Some("thread-77"),
            now,
        );

        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert_eq!(emits.len(), 1);

        let outbound = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let rows = messages_out::list_due(outbound.conn()).unwrap();
        let apology = rows
            .iter()
            .find(|r| r.kind == MessageKind::Error)
            .expect("expected an apology error row");
        assert_eq!(
            apology.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram"),
            "channel_type must be preserved",
        );
        assert_eq!(
            apology.platform_id.as_deref(),
            Some("12345"),
            "platform_id must be preserved",
        );
        assert_eq!(
            apology.thread_id.as_deref(),
            Some("thread-77"),
            "thread_id must be preserved",
        );
    }

    #[test]
    fn non_chat_kinds_are_skipped() {
        let (_c, root, sess, now, tracker) = fixture();
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let msg = WriteInbound {
            id: MessageId::new(),
            kind: MessageKind::Task,
            timestamp: now - ChDuration::seconds(i64::from(APOLOGY_AFTER_SECS) + 60),
            content: serde_json::json!({"text":"cron"}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some("p".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        };
        insert_in(pool.conn_mut(), &msg).unwrap();
        drop(pool);
        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert!(emits.is_empty());
    }

    /// Spec test #4: a session with `container_status='stopped'`, a
    /// pending inbound, and `spawn_attempts >= 3` produces an apology
    /// tagged `reason=container_spawn_failed`. Note no age threshold
    /// applies here — once the spawn budget is exhausted the user
    /// deserves an apology sooner than the 5-min mark.
    #[test]
    fn container_spawn_failure_emits_apology() {
        let (central, root, mut sess, now, tracker) = fixture();
        sessions_tbl::mark_container_stopped(&central, sess.id).unwrap();
        sess = sessions_tbl::get(&central, sess.id).unwrap();
        let id = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::seconds(2),
            "p-spawn",
            "cli",
            None,
            now,
        );
        // Simulate three failed spawn attempts (matching the container
        // manager's spawn_tracker.record_failure on each error).
        for _ in 0..crate::spawn_tracker::SPAWN_FAIL_THRESHOLD {
            tracker.record_failure(sess.id);
        }
        assert!(tracker.is_exhausted(sess.id));

        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].message_id, id);
        assert_eq!(emits[0].reason, ApologyReason::ContainerSpawnFailed);
        assert_eq!(
            emits[0].reason.metric_label(),
            ironclaw_metrics::STUCK_REASON_CONTAINER_SPAWN_FAILED
        );

        // Dedupe marker set.
        let inbound = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        assert!(apology_marker_present(inbound.conn(), id).unwrap());
    }

    /// Below the spawn-failure threshold the check does NOT fire the
    /// `container_spawn_failed` branch — without enough attempts the
    /// manager may still recover.
    #[test]
    fn container_spawn_not_failed_until_threshold_reached() {
        let (central, root, mut sess, now, tracker) = fixture();
        sessions_tbl::mark_container_stopped(&central, sess.id).unwrap();
        sess = sessions_tbl::get(&central, sess.id).unwrap();
        let _ = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::seconds(2),
            "p-spawn",
            "cli",
            None,
            now,
        );
        // Only two failures — under the threshold of 3.
        tracker.record_failure(sess.id);
        tracker.record_failure(sess.id);
        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert!(
            emits.is_empty(),
            "no apology until spawn tracker is exhausted",
        );
    }

    #[test]
    fn missing_routing_marks_dedupe_without_emitting() {
        let (_c, root, sess, now, tracker) = fixture();
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let id = MessageId::new();
        // Insert a chat row with no channel routing — the runner-side
        // apology path skips these too. The sweep marks tries=99 so we
        // don't scan it again, but emits nothing.
        let msg = WriteInbound {
            id,
            kind: MessageKind::Chat,
            timestamp: now - ChDuration::seconds(i64::from(APOLOGY_AFTER_SECS) + 60),
            content: serde_json::json!({"text":"hi"}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        };
        insert_in(pool.conn_mut(), &msg).unwrap();
        drop(pool);

        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert!(emits.is_empty());
        let inbound = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        assert!(apology_marker_present(inbound.conn(), id).unwrap());
    }

    // ── Liveness gate (false-positive apology suppression) ────────

    /// A long-running model turn writes a fresh heartbeat and acks
    /// the inbound as `processing` while the inbound's `status`
    /// column stays `pending` (the runner only flips that at
    /// finalize). The age-only branch used to fire here; the gate
    /// added on 2026-05-24 must suppress the apology so the user
    /// doesn't get a false "I'm having trouble" toast during
    /// legitimate work.
    #[test]
    fn apology_suppressed_when_runner_actively_processing() {
        use ironclaw_db::tables::processing_ack;

        let (_c, root, sess, now, tracker) = fixture();
        let id = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::minutes(7),
            "tg-1",
            "telegram",
            None,
            now,
        );

        // Runner has acked the inbound and is still chewing on it.
        let mut outbound_pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        processing_ack::insert(
            outbound_pool.conn_mut(),
            id,
            processing_ack::ProcessingStatus::Processing,
        )
        .unwrap();
        drop(outbound_pool);

        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert!(
            emits.is_empty(),
            "apology should be suppressed when ack=processing + container Running, got: {emits:?}",
        );

        let outbound = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let rows = messages_out::list_due(outbound.conn()).unwrap();
        assert!(
            rows.iter().all(|r| r.kind != MessageKind::Error),
            "no Error apology should land in outbound when suppressed",
        );

        // The dedupe marker must NOT be stamped — the row may still
        // genuinely become stuck later (e.g. the runner crashes
        // after the gate's snapshot), and we want the next sweep
        // pass to re-evaluate from scratch.
        let inbound = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        assert!(
            !apology_marker_present(inbound.conn(), id).unwrap(),
            "tries marker must not be stamped on suppressed rows",
        );
    }

    /// Container is Stopped (crashed mid-process). Even with
    /// `processing_ack=processing` on the row, the gate must not
    /// suppress — the runner is dead and the user deserves the
    /// apology. The age-only branch should fire normally.
    #[test]
    fn apology_still_fires_when_ack_processing_but_container_stopped() {
        use ironclaw_db::tables::processing_ack;

        let (central, root, mut sess, now, tracker) = fixture();
        let id = insert_stuck_chat(
            &root,
            &sess,
            ChDuration::minutes(7),
            "tg-1",
            "telegram",
            None,
            now,
        );

        // Ack is stale-processing — runner picked it up then crashed.
        let mut outbound_pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        processing_ack::insert(
            outbound_pool.conn_mut(),
            id,
            processing_ack::ProcessingStatus::Processing,
        )
        .unwrap();
        drop(outbound_pool);

        // Now mark the container stopped.
        sessions_tbl::mark_container_stopped(&central, sess.id).unwrap();
        sess = sessions_tbl::get(&central, sess.id).unwrap();

        let emits = check(&root, &tracker, &sess, now).unwrap();
        assert_eq!(emits.len(), 1, "apology must fire when container is stopped");
        assert_eq!(emits[0].message_id, id);
        assert_eq!(emits[0].reason, ApologyReason::PendingTooLong);
    }

    /// Done/Failed acks do NOT suppress: the runner is off the row,
    /// and if the inbound is still `status=pending` something else is
    /// wrong (the runner exited without flipping it). Better to
    /// surface the apology than silently swallow.
    #[test]
    fn apology_fires_when_ack_is_done_or_failed() {
        use ironclaw_db::tables::processing_ack;

        for terminal in [
            processing_ack::ProcessingStatus::Done,
            processing_ack::ProcessingStatus::Failed,
        ] {
            let (_c, root, sess, now, tracker) = fixture();
            let id = insert_stuck_chat(
                &root,
                &sess,
                ChDuration::minutes(7),
                "tg-1",
                "telegram",
                None,
                now,
            );

            let mut outbound_pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
            processing_ack::insert(outbound_pool.conn_mut(), id, terminal).unwrap();
            drop(outbound_pool);

            let emits = check(&root, &tracker, &sess, now).unwrap();
            assert_eq!(
                emits.len(),
                1,
                "ack={terminal:?} should NOT suppress apology",
            );
        }
    }

    /// Direct unit coverage on the helper — sanity that the truth
    /// table matches: only `Some(Processing)` returns true.
    #[test]
    fn inbound_is_being_processed_truth_table() {
        use ironclaw_db::tables::processing_ack;

        let (_c, root, sess, _now, _tracker) = fixture();
        let id_unacked = MessageId::new();
        let id_running = MessageId::new();
        let id_finished = MessageId::new();
        let id_errored = MessageId::new();

        let mut outbound_pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        processing_ack::insert(
            outbound_pool.conn_mut(),
            id_running,
            processing_ack::ProcessingStatus::Processing,
        )
        .unwrap();
        processing_ack::insert(
            outbound_pool.conn_mut(),
            id_finished,
            processing_ack::ProcessingStatus::Done,
        )
        .unwrap();
        processing_ack::insert(
            outbound_pool.conn_mut(),
            id_errored,
            processing_ack::ProcessingStatus::Failed,
        )
        .unwrap();

        assert!(!inbound_is_being_processed(outbound_pool.conn(), id_unacked).unwrap());
        assert!(inbound_is_being_processed(outbound_pool.conn(), id_running).unwrap());
        assert!(!inbound_is_being_processed(outbound_pool.conn(), id_finished).unwrap());
        assert!(!inbound_is_being_processed(outbound_pool.conn(), id_errored).unwrap());
    }
}
