//! Stuck-tool detection.
//!
//! Reads `container_state` from `outbound.db`. A session is "stuck" if
//! `current_tool` is set and the wall-clock delta from `tool_started_at` to
//! `now` exceeds `max(declared_timeout_ms, CLAIM_STUCK_MS)`. The
//! [`crate::ABSOLUTE_CEILING_MS`] hard cap also forces the session into
//! the stuck list regardless of declared timeout.

use crate::error::SweepError;
use crate::service::SessionRoot;
use crate::{ABSOLUTE_CEILING_MS, CLAIM_STUCK_MS};
use chrono::{DateTime, Utc};
use copperclaw_db::tables::container_state;
use copperclaw_types::{AgentGroupId, SessionId};

/// Returns `Ok(true)` if the session has a stuck tool, `Ok(false)`
/// otherwise. Returns `Err` only on DB / IO failure.
pub fn check(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Result<bool, SweepError> {
    let pool = root.outbound_pool(agent_group_id, session_id)?;
    let state = container_state::get(pool.conn())?;
    let Some(state) = state else {
        return Ok(false);
    };
    let (Some(_current_tool), Some(started_at)) =
        (state.current_tool.as_deref(), state.tool_started_at)
    else {
        return Ok(false);
    };

    let elapsed_ms = (now - started_at).num_milliseconds();
    if elapsed_ms < 0 {
        return Ok(false);
    }
    let elapsed_ms = u64::try_from(elapsed_ms).unwrap_or(u64::MAX);

    // Absolute ceiling — no tool may run longer than this.
    if elapsed_ms >= ABSOLUTE_CEILING_MS {
        return Ok(true);
    }

    let declared = state
        .tool_declared_timeout_ms
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(0);
    let threshold = declared.max(CLAIM_STUCK_MS);
    Ok(elapsed_ms >= threshold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemSessionRoot, seed_running_session};
    use chrono::{Duration as ChDuration, TimeZone};
    use copperclaw_db::central::CentralDb;

    fn fixture() -> (
        CentralDb,
        MemSessionRoot,
        copperclaw_types::Session,
        DateTime<Utc>,
    ) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        (central, root, session, now)
    }

    #[test]
    fn returns_false_when_no_state_row() {
        let (_c, root, sess, now) = fixture();
        // Touch the outbound pool so the row table exists.
        let _ = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let stuck = check(&root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(!stuck);
    }

    #[test]
    fn returns_false_when_current_tool_is_none() {
        let (_c, root, sess, now) = fixture();
        let mut pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        container_state::set(
            pool.conn_mut(),
            &container_state::ContainerState {
                current_tool: None,
                tool_declared_timeout_ms: Some(60_000),
                tool_started_at: Some(now - ChDuration::minutes(5)),
                updated_at: Some(now),
            },
        )
        .unwrap();
        assert!(!check(&root, &sess.agent_group_id, &sess.id, now).unwrap());
    }

    #[test]
    fn returns_false_when_tool_started_at_is_none() {
        let (_c, root, sess, now) = fixture();
        let mut pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        container_state::set(
            pool.conn_mut(),
            &container_state::ContainerState {
                current_tool: Some("bash".into()),
                tool_declared_timeout_ms: Some(60_000),
                tool_started_at: None,
                updated_at: Some(now),
            },
        )
        .unwrap();
        assert!(!check(&root, &sess.agent_group_id, &sess.id, now).unwrap());
    }

    #[test]
    fn returns_false_when_started_at_in_future() {
        let (_c, root, sess, now) = fixture();
        let mut pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        container_state::set(
            pool.conn_mut(),
            &container_state::ContainerState {
                current_tool: Some("bash".into()),
                tool_declared_timeout_ms: Some(60_000),
                tool_started_at: Some(now + ChDuration::minutes(5)),
                updated_at: Some(now),
            },
        )
        .unwrap();
        assert!(!check(&root, &sess.agent_group_id, &sess.id, now).unwrap());
    }

    #[test]
    fn returns_true_when_elapsed_exceeds_claim_stuck() {
        let (_c, root, sess, now) = fixture();
        let mut pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        container_state::set(
            pool.conn_mut(),
            &container_state::ContainerState {
                current_tool: Some("bash".into()),
                tool_declared_timeout_ms: None,
                tool_started_at: Some(now - ChDuration::seconds(120)),
                updated_at: Some(now),
            },
        )
        .unwrap();
        assert!(check(&root, &sess.agent_group_id, &sess.id, now).unwrap());
    }

    #[test]
    fn declared_timeout_overrides_claim_stuck() {
        let (_c, root, sess, now) = fixture();
        let mut pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        // Declared 5 minutes, elapsed 90 seconds — under the declared
        // threshold so NOT stuck (declared wins over CLAIM_STUCK_MS).
        container_state::set(
            pool.conn_mut(),
            &container_state::ContainerState {
                current_tool: Some("long_running".into()),
                tool_declared_timeout_ms: Some(300_000),
                tool_started_at: Some(now - ChDuration::seconds(90)),
                updated_at: Some(now),
            },
        )
        .unwrap();
        assert!(!check(&root, &sess.agent_group_id, &sess.id, now).unwrap());
    }

    #[test]
    fn absolute_ceiling_marks_long_runs_stuck() {
        let (_c, root, sess, now) = fixture();
        let mut pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        // Declared 1 hour but tool has been running for >30 minutes — the
        // ABSOLUTE_CEILING_MS overrides the declared timeout.
        container_state::set(
            pool.conn_mut(),
            &container_state::ContainerState {
                current_tool: Some("bash".into()),
                tool_declared_timeout_ms: Some(3_600_000),
                tool_started_at: Some(now - ChDuration::minutes(31)),
                updated_at: Some(now),
            },
        )
        .unwrap();
        assert!(check(&root, &sess.agent_group_id, &sess.id, now).unwrap());
    }

    #[test]
    fn returns_false_when_under_threshold() {
        let (_c, root, sess, now) = fixture();
        let mut pool = root.outbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        container_state::set(
            pool.conn_mut(),
            &container_state::ContainerState {
                current_tool: Some("bash".into()),
                tool_declared_timeout_ms: None,
                tool_started_at: Some(now - ChDuration::seconds(10)),
                updated_at: Some(now),
            },
        )
        .unwrap();
        assert!(!check(&root, &sess.agent_group_id, &sess.id, now).unwrap());
    }

    #[test]
    fn missing_outbound_pool_propagates_error() {
        let root = MemSessionRoot::new_strict_unknown();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let now = Utc::now();
        assert!(check(&root, &ag, &sess, now).is_err());
    }
}
