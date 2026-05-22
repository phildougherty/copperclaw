//! Per-session spawn-attempt counter shared between the container manager
//! and the sweep service.
//!
//! Why: the sweep's `apology` check (see [`crate::checks::apology`]) needs
//! to know how many spawn attempts the container manager has burned on a
//! session before deciding "the container will never come up, apologise to
//! the user". The manager doesn't persist this counter to disk — it's a
//! per-process signal — so we share it via an in-memory `Arc<DashMap>`.
//!
//! The map is process-local and intentionally bounded by the number of
//! active sessions: failures past the threshold do not grow the map, and
//! a successful spawn clears the counter. No background eviction is
//! needed because the host's session lifecycle handles cleanup.

use ironclaw_types::SessionId;
use std::collections::HashMap;
use std::sync::Mutex;

/// Threshold at which the sweep's apology check treats a session as
/// "spawn permanently failed". Matches the SRE convention of 3 strikes
/// before declaring an attempt path unhealthy.
pub const SPAWN_FAIL_THRESHOLD: u32 = 3;

/// Shared, thread-safe per-session spawn-attempt counter.
///
/// Bumped by the container manager every time `runtime.spawn(...)`
/// returns an error. Reset on a successful spawn. Read by the sweep
/// service to gate the `container_spawn_failed` apology emit.
#[derive(Debug, Default)]
pub struct SpawnAttemptTracker {
    counts: Mutex<HashMap<SessionId, u32>>,
}

impl SpawnAttemptTracker {
    /// Build a fresh tracker with no recorded failures.
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Bump the failure count for `session_id`. Returns the new count.
    /// Saturating add — won't roll over even on a pathological flap.
    pub fn record_failure(&self, session_id: SessionId) -> u32 {
        let mut g = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = g.entry(session_id).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Drop the recorded count (typically after a successful spawn).
    pub fn record_success(&self, session_id: SessionId) {
        let mut g = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.remove(&session_id);
    }

    /// Read the current failure count for `session_id`. Returns 0 when
    /// the session has never failed (or its counter was cleared).
    #[must_use]
    pub fn failure_count(&self, session_id: SessionId) -> u32 {
        let g = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.get(&session_id).copied().unwrap_or(0)
    }

    /// True if the session has reached the [`SPAWN_FAIL_THRESHOLD`].
    #[must_use]
    pub fn is_exhausted(&self, session_id: SessionId) -> bool {
        self.failure_count(session_id) >= SPAWN_FAIL_THRESHOLD
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_tracker_reports_zero() {
        let t = SpawnAttemptTracker::new();
        let s = SessionId::new();
        assert_eq!(t.failure_count(s), 0);
        assert!(!t.is_exhausted(s));
    }

    #[test]
    fn record_failure_increments_count() {
        let t = SpawnAttemptTracker::new();
        let s = SessionId::new();
        assert_eq!(t.record_failure(s), 1);
        assert_eq!(t.record_failure(s), 2);
        assert_eq!(t.failure_count(s), 2);
    }

    #[test]
    fn threshold_triggers_exhausted() {
        let t = SpawnAttemptTracker::new();
        let s = SessionId::new();
        for _ in 0..SPAWN_FAIL_THRESHOLD {
            t.record_failure(s);
        }
        assert!(t.is_exhausted(s));
    }

    #[test]
    fn record_success_clears_count() {
        let t = SpawnAttemptTracker::new();
        let s = SessionId::new();
        t.record_failure(s);
        t.record_failure(s);
        t.record_success(s);
        assert_eq!(t.failure_count(s), 0);
    }

    #[test]
    fn distinct_sessions_have_distinct_counts() {
        let t = SpawnAttemptTracker::new();
        let a = SessionId::new();
        let b = SessionId::new();
        t.record_failure(a);
        t.record_failure(a);
        t.record_failure(b);
        assert_eq!(t.failure_count(a), 2);
        assert_eq!(t.failure_count(b), 1);
    }

    #[test]
    fn saturating_add_does_not_overflow() {
        let t = SpawnAttemptTracker::new();
        let s = SessionId::new();
        {
            let mut g = t.counts.lock().unwrap();
            g.insert(s, u32::MAX);
        }
        assert_eq!(t.record_failure(s), u32::MAX);
    }
}
