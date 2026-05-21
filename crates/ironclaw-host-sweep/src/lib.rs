//! 60-second host maintenance loop: stuck detection, recurrence fanout,
//! processing-ack reset, due-message wake, heartbeat sync.
//!
//! See `PLAN.md` § 6 (T3). This crate is consumed by `ironclaw-host`; it
//! never touches a container runtime directly. Each pass produces a
//! [`SweepReport`] describing sessions that need attention so the host
//! can translate the report into container operations.

pub mod checks;
pub mod clock;
pub mod error;
pub mod service;

#[cfg(test)]
mod test_support;

pub use clock::{Clock, SystemClock};
pub use error::SweepError;
pub use service::{
    MessageReset, SeriesFanout, SessionPool, SessionRoot, SweepReport, SweepService,
};

/// How often `run_loop` calls `run_once`.
pub const SWEEP_POLL_MS: u64 = 60_000;

/// A `processing_ack` in `picked_up` (or `processing`) state older than this
/// threshold is considered abandoned. Likewise, a `current_tool` whose
/// declared timeout is shorter than this is rounded up.
pub const CLAIM_STUCK_MS: u64 = 60_000;

/// Hard cap on any tool runtime regardless of declared timeout. A tool
/// running longer than this is considered stuck even if it declared a
/// longer timeout.
pub const ABSOLUTE_CEILING_MS: u64 = 1_800_000;

/// Heartbeat-file mtime older than this marks the session for restart.
pub const HEARTBEAT_STALE_MS: u64 = 90_000;

/// Hard cap on inbound message retries. A message whose `tries` exceeds
/// this is dropped by the host rather than re-queued.
pub const MAX_TRIES: u32 = 5;

#[cfg(test)]
mod constant_tests {
    use super::*;

    #[test]
    fn sweep_poll_matches_one_minute() {
        assert_eq!(SWEEP_POLL_MS, 60_000);
    }

    #[test]
    fn claim_stuck_matches_one_minute() {
        assert_eq!(CLAIM_STUCK_MS, 60_000);
    }

    #[test]
    fn absolute_ceiling_matches_thirty_minutes() {
        assert_eq!(ABSOLUTE_CEILING_MS, 1_800_000);
    }

    #[test]
    fn heartbeat_stale_matches_ninety_seconds() {
        assert_eq!(HEARTBEAT_STALE_MS, 90_000);
    }

    #[test]
    fn max_tries_is_five() {
        assert_eq!(MAX_TRIES, 5);
    }
}
