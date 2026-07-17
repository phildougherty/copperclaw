//! M21 S6: the runner's test-clock seam.
//!
//! The Task HUD's timed surfaces (the bare-channel 60s status-row
//! cadence, the 150s softening threshold, the elapsed clock every frame
//! renders) read wall time through [`Clock`] instead of calling
//! [`Instant::now`] directly. Production injects [`SystemClock`] (real
//! time, byte-identical behaviour to the pre-seam code); deterministic
//! tests and the replay harness inject a [`TestClock`] they advance by
//! hand, so legs that used to need a real 60-second wait (the M18 X2
//! known gap) are now pinned without any wall-clock sleep.
//!
//! Scope note: this seam covers *elapsed-time measurement* — the
//! `Instant` reads that decide whether a timed leg is due. It does NOT
//! wrap `tokio::time::sleep`; the HUD's background ticker sleeps are
//! already controllable via tokio's paused test clock
//! (`#[tokio::test(start_paused = true)]` + `tokio::time::advance`),
//! which composes with this seam rather than duplicating it. Later M21
//! timed surfaces (backoffs, TTLs, spawn thresholds) should take a
//! `Arc<dyn Clock>` the same way instead of growing their own seams.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Monotonic-time source for the runner's timed surfaces.
///
/// Implementations must be cheap to call and monotonic (never move
/// backwards) — callers compare instants with
/// [`Instant::saturating_duration_since`], so a stalled clock degrades
/// to "no time has passed", never to a panic.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The production clock: a plain [`Instant::now`] passthrough. This is
/// the default everywhere ([`crate::RunnerDeps::minimal`] and the
/// production binary), so behaviour at default is byte-identical to the
/// pre-seam code.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A manually-advanced clock for deterministic tests.
///
/// Starts at the real "now" captured at construction and only moves
/// when [`TestClock::advance`] is called. Clones share the same offset
/// (it lives behind an `Arc`), so a test can keep one handle to drive
/// time while the code under test holds another as its `dyn Clock` —
/// the replay harness relies on exactly this to advance the runner's
/// HUD clock mid-turn from a wiremock responder.
#[derive(Debug, Clone)]
pub struct TestClock {
    /// Real instant captured at construction; all reported instants are
    /// `base + offset` so arithmetic against other instants stays valid.
    base: Instant,
    /// Total manual advancement so far. Shared across clones.
    offset: Arc<Mutex<Duration>>,
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl TestClock {
    /// A fresh clock pinned at the construction-time "now".
    #[must_use]
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    /// Advance the clock by `by`. Visible to every clone immediately.
    pub fn advance(&self, by: Duration) {
        if let Ok(mut offset) = self.offset.lock() {
            *offset = offset.saturating_add(by);
        }
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        let offset = self.offset.lock().map_or(Duration::ZERO, |offset| *offset);
        self.base + offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_tracks_real_time() {
        let clock = SystemClock;
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a, "system clock must be monotonic");
    }

    #[test]
    fn test_clock_only_moves_on_advance_and_shares_across_clones() {
        let clock = TestClock::new();
        let handle = clock.clone();
        let start = clock.now();
        assert_eq!(clock.now(), start, "no advance -> no movement");
        handle.advance(Duration::from_secs(61));
        assert_eq!(
            clock.now().saturating_duration_since(start),
            Duration::from_secs(61),
            "an advance on one handle is visible on every clone"
        );
        clock.advance(Duration::from_secs(90));
        assert_eq!(
            handle.now().saturating_duration_since(start),
            Duration::from_secs(151),
            "advances accumulate across handles"
        );
    }
}
