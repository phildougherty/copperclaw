//! Debounce + in-flight tracking for the router.
//!
//! Two dashmaps keyed by tuples; the router uses them to drop duplicate
//! deliveries that arrive within `DEBOUNCE_WINDOW` and to prevent two
//! concurrent fanouts of the same `(messaging_group, thread)` from
//! interleaving.

use copperclaw_types::ChannelType;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Duplicate-suppression window for inbound events that report the same
/// `(channel_type, platform_id, thread_id, message.id)`. The window is
/// deliberately short — 500ms is enough to absorb retries from chatty
/// platforms (Slack, Telegram long-poll bounces) without hiding legitimate
/// rapid follow-ups.
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(500);

/// Key used to identify a single inbound event for debouncing.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct DebounceKey {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub message_id: String,
}

/// Key used to track in-flight routing fans-out for a target session.
/// Combined with the session id rather than the messaging group so the
/// re-entry guard can see "this exact session is already processing".
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct InflightKey {
    pub session_id: String,
}

/// Wrapper around the debounce map. Time advances via `Instant::now()` so the
/// fast path doesn't allocate.
#[derive(Clone, Default)]
pub struct Debouncer {
    inner: Arc<DashMap<DebounceKey, Instant>>,
}

impl Debouncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the raw map (used by `Router` to keep the field exposed).
    pub fn inner(&self) -> &Arc<DashMap<DebounceKey, Instant>> {
        &self.inner
    }

    /// Check whether `key` has been observed within the debounce window.
    /// If not, record `now` and return `false`. The returned bool is
    /// "should drop?"; `true` means the event is a duplicate.
    pub fn check_and_record(&self, key: DebounceKey, now: Instant) -> bool {
        // Take a snapshot of the previous timestamp and drop the dashmap
        // reference before doing any further work; holding the read ref
        // and then calling `insert` on the same shard deadlocks.
        let previous = self.inner.get(&key).map(|r| *r);
        if let Some(prev) = previous {
            if now.duration_since(prev) < DEBOUNCE_WINDOW {
                return true;
            }
        }
        self.inner.insert(key, now);
        false
    }

    /// Drop entries whose timestamp is older than `DEBOUNCE_WINDOW * 4`.
    /// Called opportunistically; never required for correctness.
    pub fn purge_expired(&self, now: Instant) {
        let cutoff = DEBOUNCE_WINDOW * 4;
        self.inner.retain(|_, ts| now.duration_since(*ts) < cutoff);
    }
}

impl std::fmt::Debug for Debouncer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Debouncer")
            .field("entries", &self.inner.len())
            .finish()
    }
}

/// Tracks sessions that are currently being routed-to so the re-entry guard
/// can refuse to recurse into a session that's already on the call stack
/// (an agent sending to itself via a wired-self destination, for instance).
#[derive(Clone, Default)]
pub struct InflightSet {
    inner: Arc<DashMap<InflightKey, ()>>,
}

impl InflightSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the raw map (used by `Router` to keep the field exposed).
    pub fn inner(&self) -> &Arc<DashMap<InflightKey, ()>> {
        &self.inner
    }

    /// True if `key` is currently registered as in-flight.
    pub fn contains(&self, key: &InflightKey) -> bool {
        self.inner.contains_key(key)
    }

    /// Insert `key` and return a guard that removes the entry on drop.
    /// Returns `None` if the key was already present.
    pub fn enter(&self, key: InflightKey) -> Option<InflightGuard<'_>> {
        if self.inner.contains_key(&key) {
            return None;
        }
        self.inner.insert(key.clone(), ());
        Some(InflightGuard { set: self, key })
    }
}

impl std::fmt::Debug for InflightSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InflightSet")
            .field("entries", &self.inner.len())
            .finish()
    }
}

/// RAII handle returned by [`InflightSet::enter`]. Dropping releases the
/// reservation. Tests may keep the guard alive to assert re-entry is
/// blocked.
pub struct InflightGuard<'a> {
    set: &'a InflightSet,
    key: InflightKey,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.set.inner.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(message_id: &str) -> DebounceKey {
        DebounceKey {
            channel_type: ChannelType::new("cli"),
            platform_id: "p1".into(),
            thread_id: None,
            message_id: message_id.into(),
        }
    }

    #[test]
    fn first_record_does_not_drop() {
        let d = Debouncer::new();
        assert!(!d.check_and_record(key("m1"), Instant::now()));
    }

    #[test]
    fn second_record_within_window_drops() {
        let d = Debouncer::new();
        let now = Instant::now();
        assert!(!d.check_and_record(key("m1"), now));
        assert!(d.check_and_record(key("m1"), now));
    }

    #[test]
    fn record_after_window_does_not_drop() {
        let d = Debouncer::new();
        let now = Instant::now();
        assert!(!d.check_and_record(key("m1"), now));
        let later = now + DEBOUNCE_WINDOW + Duration::from_millis(10);
        assert!(!d.check_and_record(key("m1"), later));
    }

    #[test]
    fn different_keys_do_not_collide() {
        let d = Debouncer::new();
        let now = Instant::now();
        assert!(!d.check_and_record(key("m1"), now));
        assert!(!d.check_and_record(key("m2"), now));
    }

    #[test]
    fn purge_expired_drops_stale_entries() {
        let d = Debouncer::new();
        let now = Instant::now();
        d.check_and_record(key("m1"), now);
        assert_eq!(d.inner.len(), 1);
        let later = now + DEBOUNCE_WINDOW * 5;
        d.purge_expired(later);
        assert_eq!(d.inner.len(), 0);
    }

    #[test]
    fn inflight_enter_blocks_re_entry() {
        let s = InflightSet::new();
        let k = InflightKey {
            session_id: "sess-1".into(),
        };
        let g = s.enter(k.clone()).unwrap();
        assert!(s.contains(&k));
        assert!(s.enter(k.clone()).is_none());
        drop(g);
        assert!(!s.contains(&k));
    }

    #[test]
    fn inflight_guard_releases_on_drop() {
        let s = InflightSet::new();
        let k = InflightKey {
            session_id: "x".into(),
        };
        {
            let _g = s.enter(k.clone()).unwrap();
            assert_eq!(s.inner.len(), 1);
        }
        assert_eq!(s.inner.len(), 0);
    }

    #[test]
    fn debug_impls_render() {
        let d = Debouncer::new();
        let s = InflightSet::new();
        assert!(format!("{d:?}").contains("Debouncer"));
        assert!(format!("{s:?}").contains("InflightSet"));
    }
}
