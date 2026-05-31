//! Processing-ack reset.
//!
//! For each `processing_ack` row in `outbound.db`:
//!
//! * status `processing` (the container has picked the message up)
//! * `status_changed > CLAIM_STUCK_MS` ago
//! * no `messages_out` row exists with `in_reply_to == message_id` (the
//!   container never produced a reply / result / error event)
//!
//! we treat the in-flight message as abandoned: bump `tries` in
//! `messages_in` and reset its `status` to `pending`. The container-side
//! `processing_ack` row is deleted so the next poll can re-acquire.

use crate::error::SweepError;
use crate::service::{MessageReset, SessionRoot};
use crate::CLAIM_STUCK_MS;
use chrono::{DateTime, Utc};
use ironclaw_db::tables::processing_ack;
use ironclaw_db::tables::processing_ack::ProcessingStatus;
use ironclaw_types::{AgentGroupId, SessionId};
use rusqlite::{params, Connection};

/// Walk every claim on this session's `processing_ack`. Returns one
/// [`MessageReset`] per row that was reset.
pub fn check(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Result<Vec<MessageReset>, SweepError> {
    check_with_hook(root, agent_group_id, session_id, now, &mut |_| {})
}

/// Test-only seam: same as [`check`] but invokes `before_reset` after
/// the initial scan identifies a candidate but BEFORE the reset's
/// transactional re-check. Tests use this to inject a concurrent
/// "runner just finished the turn" state mutation so we can prove the
/// re-check observes it and skips the reset (TOCTOU regression).
pub fn check_with_hook(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
    now: DateTime<Utc>,
    before_reset: &mut dyn FnMut(ironclaw_types::MessageId),
) -> Result<Vec<MessageReset>, SweepError> {
    let mut outbound = root.outbound_pool(agent_group_id, session_id)?;
    let claims = processing_ack::get_all(outbound.conn())?;

    let threshold_ms = i64::try_from(CLAIM_STUCK_MS).unwrap_or(i64::MAX);
    let mut to_reset = Vec::new();
    for claim in claims {
        if claim.status != ProcessingStatus::Processing {
            continue;
        }
        let age_ms = (now - claim.status_changed).num_milliseconds();
        if age_ms < threshold_ms {
            continue;
        }
        if has_reply_in_outbound(outbound.conn(), claim.message_id)? {
            continue;
        }
        to_reset.push(claim.message_id);
    }

    if to_reset.is_empty() {
        return Ok(Vec::new());
    }

    // Reset the inbound side. Open the inbound pool only when we actually
    // have work to do so the empty-claims fast path costs one query.
    let mut inbound = root.inbound_pool(agent_group_id, session_id)?;
    let mut out = Vec::with_capacity(to_reset.len());
    for message_id in to_reset {
        before_reset(message_id);
        // TOCTOU guard: between the initial SELECT above and this
        // point, the runner may have finished the turn — writing its
        // reply to `messages_out` and flipping `processing_ack.status`
        // from Processing to Done. Without a re-check we'd happily
        // reset the inbound to `pending`, the runner would re-pick it
        // up, and the user would get a duplicate reply. We re-acquire
        // the claim atomically (BEGIN IMMEDIATE) against the SAME
        // outbound DB the runner writes to, re-verify it is still
        // Processing AND that no reply landed, and ONLY then delete
        // the claim. If the delete succeeds we proceed to bump the
        // inbound row; if the re-check loses the race we skip
        // entirely so the runner's successful turn is honoured.
        //
        // The cross-DB inbound bump that follows is now safe: the
        // ack has been deleted, so a fresh runner pickup would
        // require both this UPDATE and a new INSERT into
        // processing_ack — and no path in the system synthesises that
        // for an inbound whose status is still Processing.
        let deleted = atomic_reclaim_claim(
            outbound.conn_mut(),
            message_id,
            now,
            threshold_ms,
        )?;
        if !deleted {
            tracing::debug!(
                target: "ironclaw_host_sweep::processing",
                message = %message_id.as_uuid(),
                "skipped stale-claim reset: runner finished the turn between scan and reset",
            );
            continue;
        }
        let new_tries = reset_message_in(inbound.conn_mut(), message_id)?;
        out.push(MessageReset {
            session_id: *session_id,
            message_id,
            new_tries,
        });
    }
    Ok(out)
}

/// Re-acquire the candidate claim transactionally and delete it iff:
/// (a) the row still exists, (b) its status is still `Processing`, (c)
/// it is still older than `threshold_ms`, and (d) no reply landed in
/// `messages_out` between the initial scan and now. Returns `true` if
/// we deleted (caller proceeds with the inbound reset) or `false` if
/// any guard failed (caller skips so we don't trigger a duplicate
/// reply). Runs entirely against the outbound DB — `messages_out` and
/// `processing_ack` are colocated there, so a single IMMEDIATE
/// transaction is enough.
fn atomic_reclaim_claim(
    conn: &mut Connection,
    message_id: ironclaw_types::MessageId,
    now: DateTime<Utc>,
    threshold_ms: i64,
) -> Result<bool, SweepError> {
    let id_str = message_id.as_uuid().to_string();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Re-read the claim under the immediate lock.
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT status, status_changed FROM processing_ack WHERE message_id = ?1",
            params![id_str],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((status_str, changed_str)) = row else {
        tx.commit()?;
        return Ok(false);
    };
    if ProcessingStatus::parse(&status_str) != Some(ProcessingStatus::Processing) {
        tx.commit()?;
        return Ok(false);
    }
    let status_changed = if let Ok(dt) = DateTime::parse_from_rfc3339(&changed_str) {
        dt.with_timezone(&Utc)
    } else {
        tx.commit()?;
        return Ok(false);
    };
    if (now - status_changed).num_milliseconds() < threshold_ms {
        tx.commit()?;
        return Ok(false);
    }
    // Re-check for a reply under the same transaction so a runner
    // write of (reply + ack=Done) that races the scan can't slip
    // through.
    let reply_count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages_out WHERE in_reply_to = ?1",
        params![id_str],
        |r| r.get(0),
    )?;
    if reply_count > 0 {
        tx.commit()?;
        return Ok(false);
    }
    let n = tx.execute(
        "DELETE FROM processing_ack WHERE message_id = ?1",
        params![id_str],
    )?;
    tx.commit()?;
    Ok(n > 0)
}

/// Returns true if the container produced any `messages_out` row whose
/// `in_reply_to` equals the given inbound `message_id`. We treat any reply
/// (result, error, side-effect message) as evidence the container actually
/// processed the inbound message and we shouldn't reset it.
fn has_reply_in_outbound(
    conn: &Connection,
    message_id: ironclaw_types::MessageId,
) -> Result<bool, SweepError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages_out WHERE in_reply_to = ?1",
        params![message_id.as_uuid().to_string()],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Bump `tries` and reset `status` to `pending`. Returns the new `tries`
/// value. If the row is missing we silently return 0.
fn reset_message_in(
    conn: &mut Connection,
    message_id: ironclaw_types::MessageId,
) -> Result<i64, SweepError> {
    let tx = conn.transaction()?;
    let id_str = message_id.as_uuid().to_string();
    let n = tx.execute(
        "UPDATE messages_in
         SET tries = tries + 1, status = 'pending'
         WHERE id = ?1",
        params![id_str],
    )?;
    if n == 0 {
        tx.commit()?;
        return Ok(0);
    }
    let new_tries: i64 = tx.query_row(
        "SELECT tries FROM messages_in WHERE id = ?1",
        params![id_str],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(new_tries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        insert_inbound_message, insert_outbound_reply, seed_running_session, MemSessionRoot,
    };
    use chrono::{Duration as ChDuration, TimeZone};
    use ironclaw_db::central::CentralDb;
    use ironclaw_types::MessageId;

    fn fixture() -> (MemSessionRoot, ironclaw_types::Session, DateTime<Utc>) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        (root, session, now)
    }

    fn write_claim(
        root: &MemSessionRoot,
        sess: &ironclaw_types::Session,
        id: MessageId,
        status: ProcessingStatus,
        when: DateTime<Utc>,
    ) {
        let mut pool = root
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let conn = pool.conn_mut();
        conn.execute(
            "INSERT INTO processing_ack (message_id, status, status_changed) VALUES (?1, ?2, ?3)",
            params![id.as_uuid().to_string(), status.as_str(), when.to_rfc3339()],
        )
        .unwrap();
    }

    #[test]
    fn empty_processing_ack_returns_empty() {
        let (root, sess, now) = fixture();
        let _ = root
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn stale_processing_claim_with_no_reply_is_reset() {
        let (root, sess, now) = fixture();
        let msg = insert_inbound_message(&root, &sess);
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Processing,
            now - ChDuration::minutes(5),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].message_id, msg);
        assert_eq!(r[0].new_tries, 1);
        // Second pass: row is now status='pending' but tries was bumped;
        // the claim was deleted so nothing to do.
        let r2 = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r2.is_empty());
    }

    #[test]
    fn fresh_processing_claim_is_not_reset() {
        let (root, sess, now) = fixture();
        let msg = insert_inbound_message(&root, &sess);
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Processing,
            now - ChDuration::seconds(5),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn done_status_is_skipped() {
        let (root, sess, now) = fixture();
        let msg = insert_inbound_message(&root, &sess);
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Done,
            now - ChDuration::minutes(5),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn failed_status_is_skipped() {
        let (root, sess, now) = fixture();
        let msg = insert_inbound_message(&root, &sess);
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Failed,
            now - ChDuration::minutes(5),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn processing_with_existing_reply_is_skipped() {
        let (root, sess, now) = fixture();
        let msg = insert_inbound_message(&root, &sess);
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Processing,
            now - ChDuration::minutes(5),
        );
        insert_outbound_reply(&root, &sess, msg);
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn reset_increments_tries_each_time() {
        let (root, sess, now) = fixture();
        let msg = insert_inbound_message(&root, &sess);
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Processing,
            now - ChDuration::minutes(5),
        );
        let r1 = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r1[0].new_tries, 1);

        // Simulate a second stuck pickup of the same message.
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Processing,
            now - ChDuration::minutes(5),
        );
        let r2 = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r2[0].new_tries, 2);
    }

    #[test]
    fn reset_for_missing_message_in_returns_zero() {
        let (root, sess, now) = fixture();
        let orphan = MessageId::new();
        write_claim(
            &root,
            &sess,
            orphan,
            ProcessingStatus::Processing,
            now - ChDuration::minutes(5),
        );
        let r = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].new_tries, 0);
    }

    /// TOCTOU regression: the original `check` did SELECT (claim is
    /// Processing-stale, no reply) -> UPDATE (reset inbound to
    /// pending) without a re-check. If the runner finishes the turn
    /// in that window — writing its reply to `messages_out` and
    /// flipping the claim to `Done` — the reset would still fire,
    /// the runner would re-pick the inbound, and the user would
    /// receive a duplicate reply. The `check_with_hook` seam lets us
    /// inject exactly that concurrent state mutation between the
    /// initial scan and the reset; the fix's transactional re-check
    /// must observe the new state and skip the reset.
    #[test]
    fn runner_finishes_turn_between_scan_and_reset_is_not_reset() {
        let (root, sess, now) = fixture();
        let msg = insert_inbound_message(&root, &sess);
        write_claim(
            &root,
            &sess,
            msg,
            ProcessingStatus::Processing,
            now - ChDuration::minutes(5),
        );
        // Capture the initial messages_in status so we can prove the
        // reset did not fire.
        let initial_status = {
            let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
            let s: String = pool
                .conn_mut()
                .query_row(
                    "SELECT status FROM messages_in WHERE id = ?1",
                    params![msg.as_uuid().to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            s
        };

        // Hook fires after the initial scan tags `msg` for reset but
        // before atomic_reclaim_claim re-checks. Simulate the runner
        // racing past us: write the reply and flip the ack to Done.
        let mut fired = false;
        let r = check_with_hook(
            &root,
            &sess.agent_group_id,
            &sess.id,
            now,
            &mut |hooked_id| {
                fired = true;
                assert_eq!(hooked_id, msg);
                insert_outbound_reply(&root, &sess, msg);
                let pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
                ironclaw_db::tables::processing_ack::update_status(
                    pool.conn(),
                    msg,
                    ProcessingStatus::Done,
                )
                .unwrap();
            },
        )
        .unwrap();
        assert!(fired, "before_reset hook must fire for the candidate");

        // The TOCTOU re-check must have observed both the new reply
        // and the Done flip, so no reset was performed.
        assert!(
            r.is_empty(),
            "expected zero resets after the runner finished the turn mid-flight, got {r:?}",
        );

        // messages_in is untouched (no tries bump, no status flip
        // back to pending).
        let (final_status, final_tries) = {
            let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
            let row: (String, i64) = pool
                .conn_mut()
                .query_row(
                    "SELECT status, tries FROM messages_in WHERE id = ?1",
                    params![msg.as_uuid().to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            row
        };
        assert_eq!(
            final_status, initial_status,
            "inbound status was unexpectedly mutated by the TOCTOU-skipped reset",
        );
        assert_eq!(
            final_tries, 0,
            "inbound tries was unexpectedly bumped by the TOCTOU-skipped reset",
        );
        // The runner-set ack is preserved at Done — the sweep must
        // not have deleted it.
        let claim = ironclaw_db::tables::processing_ack::get(
            root.outbound_pool(&sess.agent_group_id, &sess.id)
                .unwrap()
                .conn(),
            msg,
        )
        .unwrap()
        .expect("processing_ack row should still exist");
        assert_eq!(claim.status, ProcessingStatus::Done);
    }
}
