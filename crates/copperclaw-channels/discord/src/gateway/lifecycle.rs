//! Pure-function lifecycle helpers for the Discord gateway.
//!
//! - `heartbeat_delay` — compute the first heartbeat delay using the
//!   `interval * jitter` formula Discord prescribes.
//! - `next_backoff` — exponential reconnect delay with a cap.
//! - `decide_resume_or_identify` — the "do I IDENTIFY or RESUME?" decision
//!   tree given the in-memory session state.
//! - `is_fatal_close` — gateway close codes that must abort reconnect.

use std::time::Duration;

/// In-memory state carried across gateway reconnects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    pub session_id: Option<String>,
    pub resume_gateway_url: Option<String>,
    pub last_sequence: Option<u64>,
}

impl SessionState {
    /// True when we have enough state to attempt a `RESUME`.
    pub fn can_resume(&self) -> bool {
        self.session_id.is_some() && self.last_sequence.is_some()
    }

    /// Clear all resume state. Called on `INVALID_SESSION { resumable: false }`.
    pub fn reset(&mut self) {
        self.session_id = None;
        self.resume_gateway_url = None;
        self.last_sequence = None;
    }

    /// Record a `READY` payload's session metadata.
    pub fn record_ready(&mut self, session_id: String, resume_gateway_url: Option<String>) {
        self.session_id = Some(session_id);
        self.resume_gateway_url = resume_gateway_url;
    }

    /// Update the running sequence number from a dispatch.
    pub fn record_sequence(&mut self, s: u64) {
        self.last_sequence = Some(s);
    }
}

/// What to do after connecting to the gateway.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NextAction {
    Identify,
    Resume,
}

/// Decide whether to send `IDENTIFY` or `RESUME` after a (re)connect.
pub fn decide_resume_or_identify(state: &SessionState) -> NextAction {
    if state.can_resume() {
        NextAction::Resume
    } else {
        NextAction::Identify
    }
}

/// Compute the first heartbeat delay: `interval_ms * jitter`, where `jitter`
/// is in `[0.0, 1.0]`. Discord recommends a random jitter for the first
/// heartbeat to spread bot load.
pub fn heartbeat_delay(interval_ms: u64, jitter: f64) -> Duration {
    let j = jitter.clamp(0.0, 1.0);
    #[allow(clippy::cast_precision_loss)]
    let scaled = interval_ms as f64 * j;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = scaled.floor() as u64;
    Duration::from_millis(ms)
}

/// Compute the next reconnect backoff. `attempt = 0` returns `base`, each
/// subsequent attempt doubles up to `cap`.
pub fn next_backoff(attempt: u32, base: Duration, cap: Duration) -> Duration {
    // `2^attempt` saturating at u32::MAX so we don't overflow even on absurd
    // attempt counts; the `min(cap)` at the end keeps us honest anyway.
    let mult = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let secs = base.as_millis().saturating_mul(u128::from(mult));
    let cap_ms = cap.as_millis();
    let clamped = secs.min(cap_ms);
    Duration::from_millis(u64::try_from(clamped).unwrap_or(u64::MAX))
}

/// Gateway close codes that must abort reconnect attempts.
///
/// - `4004` — authentication failed.
/// - `4011` — sharding required (not applicable to small bots, but we still
///   surface it as fatal rather than thrashing the gateway).
/// - `4014` — disallowed intents (privileged intent not enabled in the dev
///   portal).
pub const FATAL_CLOSE_CODES: &[u16] = &[4004, 4011, 4014];

/// True if a close code should stop the reconnect loop permanently.
pub fn is_fatal_close(code: u16) -> bool {
    FATAL_CLOSE_CODES.contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_default_cannot_resume() {
        let s = SessionState::default();
        assert!(!s.can_resume());
        assert_eq!(decide_resume_or_identify(&s), NextAction::Identify);
    }

    #[test]
    fn session_state_with_id_and_seq_can_resume() {
        let mut s = SessionState::default();
        s.record_ready("sess".into(), Some("wss://x".into()));
        s.record_sequence(7);
        assert!(s.can_resume());
        assert_eq!(decide_resume_or_identify(&s), NextAction::Resume);
        assert_eq!(s.resume_gateway_url.as_deref(), Some("wss://x"));
    }

    #[test]
    fn session_state_without_sequence_cannot_resume() {
        let mut s = SessionState::default();
        s.record_ready("sess".into(), None);
        // No sequence yet.
        assert!(!s.can_resume());
    }

    #[test]
    fn session_state_reset_clears_everything() {
        let mut s = SessionState::default();
        s.record_ready("sess".into(), Some("wss://x".into()));
        s.record_sequence(7);
        s.reset();
        assert_eq!(s, SessionState::default());
    }

    #[test]
    fn heartbeat_delay_with_jitter_zero_is_zero() {
        assert_eq!(heartbeat_delay(40_000, 0.0), Duration::ZERO);
    }

    #[test]
    fn heartbeat_delay_with_jitter_one_is_interval() {
        assert_eq!(heartbeat_delay(40_000, 1.0), Duration::from_millis(40_000));
    }

    #[test]
    fn heartbeat_delay_with_half_jitter() {
        assert_eq!(heartbeat_delay(40_000, 0.5), Duration::from_millis(20_000));
    }

    #[test]
    fn heartbeat_delay_clamps_out_of_range_jitter() {
        // Negative -> 0, > 1.0 -> 1.0.
        assert_eq!(heartbeat_delay(1000, -1.0), Duration::ZERO);
        assert_eq!(heartbeat_delay(1000, 5.0), Duration::from_millis(1000));
    }

    #[test]
    fn backoff_first_attempt_is_base() {
        assert_eq!(
            next_backoff(0, Duration::from_secs(1), Duration::from_secs(30)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn backoff_doubles_each_step() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        assert_eq!(next_backoff(1, base, cap), Duration::from_secs(2));
        assert_eq!(next_backoff(2, base, cap), Duration::from_secs(4));
        assert_eq!(next_backoff(3, base, cap), Duration::from_secs(8));
        assert_eq!(next_backoff(4, base, cap), Duration::from_secs(16));
    }

    #[test]
    fn backoff_clamps_to_cap() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        assert_eq!(next_backoff(5, base, cap), Duration::from_secs(30));
        assert_eq!(next_backoff(20, base, cap), Duration::from_secs(30));
    }

    #[test]
    fn backoff_extreme_attempt_does_not_panic() {
        let cap = Duration::from_secs(30);
        let d = next_backoff(u32::MAX, Duration::from_secs(1), cap);
        assert_eq!(d, cap);
    }

    #[test]
    fn fatal_close_codes_are_fatal() {
        for c in FATAL_CLOSE_CODES {
            assert!(is_fatal_close(*c), "code {c} should be fatal");
        }
    }

    #[test]
    fn benign_close_codes_are_not_fatal() {
        for c in [1000_u16, 1006, 4000, 4007, 4009] {
            assert!(!is_fatal_close(c), "code {c} must not be fatal");
        }
    }

    #[test]
    fn next_action_eq_and_debug() {
        assert_eq!(NextAction::Identify, NextAction::Identify);
        let _ = format!("{:?}", NextAction::Resume);
    }
}
