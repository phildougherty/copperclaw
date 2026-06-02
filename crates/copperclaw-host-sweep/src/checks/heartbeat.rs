//! Heartbeat-staleness check.
//!
//! Stats `<session_root>/.heartbeat`. If the file does not exist OR its
//! mtime is older than [`crate::HEARTBEAT_STALE_MS`], the session is
//! marked for restart.

use crate::HEARTBEAT_STALE_MS;
use crate::error::SweepError;
use crate::service::SessionRoot;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, SessionId};
use std::time::SystemTime;

/// Returns `Ok(true)` if the heartbeat is stale (or missing). Errors
/// propagate only when `std::fs::metadata` fails for a reason other than
/// `NotFound`.
pub fn check(
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Result<bool, SweepError> {
    let path = root.heartbeat_path(agent_group_id, session_id);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Missing file is treated as stale so the host knows to (re)start.
            return Ok(true);
        }
        Err(e) => return Err(e.into()),
    };
    let mtime = meta.modified()?;
    let stale_threshold = SystemTime::from(now)
        .checked_sub(std::time::Duration::from_millis(HEARTBEAT_STALE_MS))
        .unwrap_or_else(SystemTime::now);
    Ok(mtime < stale_threshold)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemSessionRoot;
    use std::time::Duration as StdDuration;

    fn root_with_session() -> (MemSessionRoot, AgentGroupId, SessionId) {
        let root = MemSessionRoot::new();
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let _ = root.outbound_pool(&ag, &sess).unwrap();
        (root, ag, sess)
    }

    #[test]
    fn returns_true_when_heartbeat_file_missing() {
        let (root, ag, sess) = root_with_session();
        let now = Utc::now();
        assert!(check(&root, &ag, &sess, now).unwrap());
    }

    #[test]
    fn returns_false_when_heartbeat_is_fresh() {
        let (root, ag, sess) = root_with_session();
        root.write_heartbeat(&ag, &sess, SystemTime::now());
        let now = Utc::now();
        assert!(!check(&root, &ag, &sess, now).unwrap());
    }

    #[test]
    fn returns_true_when_heartbeat_is_stale() {
        let (root, ag, sess) = root_with_session();
        let stale = SystemTime::now() - StdDuration::from_millis(HEARTBEAT_STALE_MS + 5_000);
        root.write_heartbeat(&ag, &sess, stale);
        let now = Utc::now();
        assert!(check(&root, &ag, &sess, now).unwrap());
    }

    #[test]
    fn returns_false_exactly_at_threshold() {
        let (root, ag, sess) = root_with_session();
        // 1 second younger than the threshold — should be fresh.
        let young = SystemTime::now() - StdDuration::from_millis(HEARTBEAT_STALE_MS - 1_000);
        root.write_heartbeat(&ag, &sess, young);
        let now = Utc::now();
        assert!(!check(&root, &ag, &sess, now).unwrap());
    }
}
