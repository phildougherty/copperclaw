//! Time source abstraction so tests can advance time deterministically.

use chrono::{DateTime, Utc};
use std::sync::Mutex;

/// Source of the current `DateTime<Utc>`. Production uses [`SystemClock`];
/// tests inject a clock that returns a controlled instant.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Real-wall-clock implementation. Returns `chrono::Utc::now()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// In-memory clock with a manually advanceable cursor — used in tests and
/// available to host integration tests via the public API.
#[derive(Debug)]
pub struct MockClock {
    now: Mutex<DateTime<Utc>>,
}

impl MockClock {
    pub fn new(initial: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(initial),
        }
    }

    /// Advance the clock by the given duration. Negative durations move
    /// the clock backwards (useful for "this row was inserted N seconds
    /// ago" fixtures).
    pub fn advance(&self, by: chrono::Duration) {
        let mut g = self.now.lock().expect("mock clock mutex poisoned");
        *g += by;
    }

    /// Replace the clock's current instant.
    pub fn set(&self, t: DateTime<Utc>) {
        let mut g = self.now.lock().expect("mock clock mutex poisoned");
        *g = t;
    }
}

impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("mock clock mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_returns_recent_now() {
        let before = Utc::now();
        let c = SystemClock;
        let t = c.now();
        let after = Utc::now();
        assert!(t >= before);
        assert!(t <= after + chrono::Duration::seconds(1));
    }

    #[test]
    fn system_clock_default_works() {
        let c = SystemClock;
        let _ = c.now();
    }

    #[test]
    fn mock_clock_returns_initial() {
        let t = Utc::now();
        let c = MockClock::new(t);
        assert_eq!(c.now(), t);
    }

    #[test]
    fn mock_clock_advance_moves_forward() {
        let t0 = Utc::now();
        let c = MockClock::new(t0);
        c.advance(chrono::Duration::seconds(30));
        assert_eq!(c.now(), t0 + chrono::Duration::seconds(30));
    }

    #[test]
    fn mock_clock_set_overwrites() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::hours(2);
        let c = MockClock::new(t0);
        c.set(t1);
        assert_eq!(c.now(), t1);
    }

    #[test]
    fn mock_clock_advance_negative_moves_back() {
        let t0 = Utc::now();
        let c = MockClock::new(t0);
        c.advance(chrono::Duration::seconds(-10));
        assert_eq!(c.now(), t0 - chrono::Duration::seconds(10));
    }
}
