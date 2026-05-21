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
    let outbound = root.outbound_pool(agent_group_id, session_id)?;
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
        let new_tries = reset_message_in(inbound.conn_mut(), message_id)?;
        // Best-effort delete; rusqlite returns NotFound but we already
        // observed the row in `claims`.
        let _ = processing_ack::delete(outbound.conn(), message_id);
        out.push(MessageReset {
            session_id: *session_id,
            message_id,
            new_tries,
        });
    }
    Ok(out)
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
}
