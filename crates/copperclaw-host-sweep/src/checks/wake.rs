//! Due-message wake.
//!
//! If a session is currently `idle` (or `stopped`) but its `inbound.db`
//! contains at least one row with `status='pending'` and
//! `process_after <= now`, transition the session `container_status` to
//! `running` so the host's container manager spins it back up to process
//! the message.

use crate::error::SweepError;
use crate::service::SessionRoot;
use chrono::{DateTime, Utc};
use copperclaw_db::tables::sessions as sessions_tbl;
use copperclaw_db::central::CentralDb;
use copperclaw_types::{ContainerStatus, Session};
use rusqlite::params;

/// Returns `Ok(true)` if this session was woken (its `container_status`
/// changed from `idle`/`stopped` to `running`).
pub fn check(
    central: &CentralDb,
    root: &dyn SessionRoot,
    session: &Session,
    now: DateTime<Utc>,
) -> Result<bool, SweepError> {
    if matches!(session.container_status, ContainerStatus::Running) {
        return Ok(false);
    }
    let inbound = root.inbound_pool(&session.agent_group_id, &session.id)?;
    let count: i64 = inbound.conn().query_row(
        "SELECT COUNT(*) FROM messages_in
         WHERE status = 'pending'
           AND (process_after IS NULL OR process_after <= ?1)",
        params![now.to_rfc3339()],
        |r| r.get(0),
    )?;
    if count == 0 {
        return Ok(false);
    }
    sessions_tbl::mark_container_running(central, session.id)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        insert_inbound_message_with_process_after, seed_running_session, MemSessionRoot,
    };
    use chrono::{Duration as ChDuration, TimeZone};

    fn fixture() -> (CentralDb, MemSessionRoot, Session, DateTime<Utc>) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        (central, root, session, now)
    }

    #[test]
    fn idle_session_with_due_message_is_woken() {
        let (central, root, mut session, now) = fixture();
        sessions_tbl::mark_container_idle(&central, session.id).unwrap();
        session = sessions_tbl::get(&central, session.id).unwrap();
        insert_inbound_message_with_process_after(
            &root,
            &session,
            Some(now - ChDuration::seconds(1)),
        );
        let woken = check(&central, &root, &session, now).unwrap();
        assert!(woken);
        assert_eq!(
            sessions_tbl::get(&central, session.id).unwrap().container_status,
            ContainerStatus::Running,
        );
    }

    #[test]
    fn idle_session_with_null_process_after_is_woken() {
        let (central, root, mut session, now) = fixture();
        sessions_tbl::mark_container_idle(&central, session.id).unwrap();
        session = sessions_tbl::get(&central, session.id).unwrap();
        insert_inbound_message_with_process_after(&root, &session, None);
        assert!(check(&central, &root, &session, now).unwrap());
    }

    #[test]
    fn idle_session_with_future_process_after_is_not_woken() {
        let (central, root, mut session, now) = fixture();
        sessions_tbl::mark_container_idle(&central, session.id).unwrap();
        session = sessions_tbl::get(&central, session.id).unwrap();
        insert_inbound_message_with_process_after(
            &root,
            &session,
            Some(now + ChDuration::hours(1)),
        );
        assert!(!check(&central, &root, &session, now).unwrap());
    }

    #[test]
    fn idle_session_with_no_pending_messages_is_not_woken() {
        let (central, root, mut session, now) = fixture();
        sessions_tbl::mark_container_idle(&central, session.id).unwrap();
        session = sessions_tbl::get(&central, session.id).unwrap();
        let _ = root.inbound_pool(&session.agent_group_id, &session.id).unwrap();
        assert!(!check(&central, &root, &session, now).unwrap());
    }

    #[test]
    fn running_session_short_circuits() {
        // No inbound pool ever opened — if check tried to open one, it
        // would return Err (strict root). We confirm running sessions are
        // short-circuited before any per-session DB work.
        let central = CentralDb::open_in_memory().unwrap();
        let session = seed_running_session(&central);
        sessions_tbl::mark_container_running(&central, session.id).unwrap();
        let session = sessions_tbl::get(&central, session.id).unwrap();
        let root = MemSessionRoot::new_strict_unknown();
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        assert!(!check(&central, &root, &session, now).unwrap());
    }

    #[test]
    fn stopped_session_with_due_message_is_woken() {
        let (central, root, mut session, now) = fixture();
        sessions_tbl::mark_container_stopped(&central, session.id).unwrap();
        session = sessions_tbl::get(&central, session.id).unwrap();
        insert_inbound_message_with_process_after(
            &root,
            &session,
            Some(now - ChDuration::seconds(5)),
        );
        assert!(check(&central, &root, &session, now).unwrap());
    }
}
