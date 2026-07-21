//! Host-level resource health: "can this box support a container right now?"
//!
//! # Why this exists
//!
//! On 2026-07-18/19 the host's disk filled. The runner containers could
//! not write, their heartbeats went stale, and the reconcile loop did the
//! only thing it knew how to do: classify each dead container as a
//! *per-session* crash and defer that *one* session's respawn on the
//! per-session backoff curve. The log escalated from 1 to 56 to 179
//! `heartbeat stale; running -> stopped (will respawn)` lines a day, with
//! 235 `crash-restart recorded; respawn deferred by backoff ...
//! cause="generic"` alongside them.
//!
//! Per-session backoff structurally cannot fix a whole-box fault. The
//! fault is *shared*: N sessions each independently walk their curve to
//! the 300s cap and then retry forever against the same full disk, and an
//! operator reading 235 `cause="generic"` lines has no thread back to the
//! actual cause. The host had no notion of "the environment is broken."
//!
//! This module is that notion, in three pieces:
//!
//! 1. [`DiskProbe`] — a cached `statvfs` of the data dir. The spawn path
//!    consults it *before* burning a container spawn; below
//!    [`copperclaw_cclaw::disk::DISK_FAIL_BYTES`] the spawn is refused and
//!    the session stays `Stopped` with its inbound still pending. Refusing
//!    is strictly better than spawning: a doomed container still writes
//!    image layers and a session DB on the way to failing.
//! 2. [`EnvFaultBreaker`] — a global circuit breaker keyed on *distinct
//!    sessions*. When [`ENV_FAULT_SESSION_THRESHOLD`] different sessions
//!    crash-restart inside [`ENV_FAULT_WINDOW`], that is one environment
//!    fault, not K session faults. It trips once, fires ONE operator
//!    alert (not one per session), and stops respawns until the disk probe
//!    comes back clean.
//! 3. The thresholds themselves are *not* redefined here — they are
//!    [`copperclaw_cclaw::disk`]'s, the same ones `cclaw doctor` renders,
//!    so the CLI and the runtime can never disagree about how full is too
//!    full.
//!
//! Both the probe and the breaker are in-memory and reset on host
//! restart, matching the posture of `crash_loop`.

use copperclaw_cclaw::disk::{self, DiskLevel};
use copperclaw_types::SessionId;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

/// How many *distinct* sessions must crash-restart inside
/// [`ENV_FAULT_WINDOW`] before the manager calls it an environment fault
/// rather than K coincidental session bugs.
///
/// Three is deliberately low. Independent sessions crashing within two
/// minutes of each other is already unusual — they run different agents,
/// different models, different work. What makes them crash together is
/// almost always something they share: the disk, the Docker daemon, the
/// box. Set higher and a small install (2-3 active sessions) could never
/// trip it, which is exactly the install where the storm is loudest.
pub const ENV_FAULT_SESSION_THRESHOLD: usize = 3;

/// Rolling window the distinct-session count is measured over. Two
/// minutes comfortably spans several reconcile ticks (1s) and the first
/// few backoff steps (5s/15s/60s), so a genuine shared fault trips it on
/// the first round of crashes rather than after everyone has climbed to
/// the 300s cap.
pub const ENV_FAULT_WINDOW: Duration = Duration::from_secs(120);

/// How long a [`DiskProbe`] reading stays usable before the next read
/// re-`statvfs`es. The reconcile loop refreshes explicitly once per tick
/// (see `ContainerManager::refresh_host_resources`), so this TTL only
/// bounds the off-tick callers — it exists so the preflight can never
/// degenerate into one `statvfs` per session per pass, which is the exact
/// shape of I/O you do not want to add to a box that is already sick.
pub const DISK_PROBE_TTL: Duration = Duration::from_millis(super::spawn::POLL_INTERVAL_MS);

/// A single free-space reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    /// Bytes available to unprivileged writers (`statvfs` `f_bavail`).
    pub free_bytes: u64,
    /// Total bytes on the filesystem.
    pub total_bytes: u64,
}

impl DiskSpace {
    /// The shared [`DiskLevel`] band for this reading.
    #[must_use]
    pub fn level(self) -> DiskLevel {
        disk::level(self.free_bytes, self.total_bytes)
    }

    /// Whether this reading is bad enough to refuse a container spawn.
    #[must_use]
    pub fn is_exhausted(self) -> bool {
        matches!(self.level(), DiskLevel::Fail)
    }

    /// Operator-facing one-liner (`4.2 GiB free of 100.0 GiB (4% free)`).
    #[must_use]
    pub fn summary(self) -> String {
        format!(
            "{} free of {} ({}% free)",
            disk::human_bytes(self.free_bytes),
            disk::human_bytes(self.total_bytes),
            disk::free_pct(self.free_bytes, self.total_bytes),
        )
    }
}

// Test seam, mirroring the `DISK_OVERRIDE` thread-local `cclaw`'s doctor
// tests use: when set on the current thread, [`probe_path`] skips the real
// `statvfs` and reports these synthetic `(free, total)` bytes. Thread-local
// so parallel tests cannot race each other, and only ever compiled in test
// builds — the production probe has no branch for it.
#[cfg(test)]
thread_local! {
    static DISK_OVERRIDE: std::cell::Cell<Option<(u64, u64)>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only: force every subsequent probe on this thread to report
/// `(free_bytes, total_bytes)`.
#[cfg(test)]
pub(crate) fn set_disk_override(free_bytes: u64, total_bytes: u64) {
    DISK_OVERRIDE.with(|c| c.set(Some((free_bytes, total_bytes))));
}

/// Test-only: drop the override so probes hit the real filesystem again.
#[cfg(test)]
pub(crate) fn clear_disk_override() {
    DISK_OVERRIDE.with(|c| c.set(None));
}

/// Walk up from `path` to the nearest ancestor that exists. A fresh
/// install's data dir may not have been created yet, and the mount is the
/// same either way, so stat'ing the live ancestor answers the question.
#[must_use]
fn nearest_existing_ancestor(path: &Path) -> std::path::PathBuf {
    let mut stat_path = path.to_path_buf();
    while !stat_path.exists() {
        match stat_path.parent() {
            Some(parent) => stat_path = parent.to_path_buf(),
            None => break,
        }
    }
    stat_path
}

/// `statvfs` `path` (or its nearest existing ancestor).
///
/// Returns `None` when the filesystem cannot be stat'd at all. Callers
/// must treat `None` as *unknown*, never as *full*: a stat failure is not
/// evidence of exhaustion, and failing closed here would turn a bad path
/// into a fleet-wide spawn freeze.
///
/// In test builds this consults the [`set_disk_override`] thread-local
/// seam, defaulting to a healthy synthetic filesystem — the same shape
/// `cclaw`'s doctor tests use. Without that default, every unrelated
/// spawn test in this crate would start failing on a developer's box that
/// merely happens to be low on disk.
#[must_use]
pub fn probe_path(path: &Path) -> Option<DiskSpace> {
    #[cfg(test)]
    {
        let _ = path;
        let (free_bytes, total_bytes) = DISK_OVERRIDE
            .with(std::cell::Cell::get)
            .unwrap_or((500 * disk::GIB, 1024 * disk::GIB));
        Some(DiskSpace {
            free_bytes,
            total_bytes,
        })
    }
    #[cfg(not(test))]
    {
        probe_path_uncached(path)
    }
}

/// The real `statvfs` half of [`probe_path`], with no test seam — so the
/// path-walking and error-degradation behaviour stays directly testable.
#[must_use]
pub fn probe_path_uncached(path: &Path) -> Option<DiskSpace> {
    let stat_path = nearest_existing_ancestor(path);
    match disk::statvfs_bytes(&stat_path) {
        Ok((free_bytes, total_bytes)) => Some(DiskSpace {
            free_bytes,
            total_bytes,
        }),
        Err(err) => {
            tracing::debug!(
                path = %stat_path.display(),
                ?err,
                "could not statvfs the data dir; treating free space as unknown"
            );
            None
        }
    }
}

/// TTL-cached free-space reading for one path. Cheap to read from the
/// per-session spawn path; refreshed once per reconcile tick.
#[derive(Debug, Default)]
pub struct DiskProbe {
    inner: Mutex<Option<(Instant, Option<DiskSpace>)>>,
}

impl DiskProbe {
    /// Fresh probe with no cached reading.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force a `statvfs` now, replacing the cached reading.
    pub fn refresh(&self, path: &Path, now: Instant) -> Option<DiskSpace> {
        let reading = probe_path(path);
        *self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((now, reading));
        reading
    }

    /// The cached reading when it is younger than [`DISK_PROBE_TTL`],
    /// otherwise a fresh `statvfs` (which is then cached). `None` means
    /// "free space is unknown", not "full".
    pub fn get(&self, path: &Path, now: Instant) -> Option<DiskSpace> {
        {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((taken, reading)) = *guard {
                if now.duration_since(taken) < DISK_PROBE_TTL {
                    return reading;
                }
            }
        }
        self.refresh(path, now)
    }
}

/// Global circuit breaker for whole-box faults.
///
/// Per-session backoff answers "this session keeps crashing"; this
/// answers "several *different* sessions just crashed, so the thing they
/// share is broken." Tracks the most recent crash instant per session,
/// prunes anything older than [`ENV_FAULT_WINDOW`], and trips when the
/// surviving set reaches [`ENV_FAULT_SESSION_THRESHOLD`].
///
/// Tripping is edge-triggered: [`Self::record_crash`] returns `true`
/// exactly once per trip, so the caller fires exactly ONE operator alert
/// no matter how many sessions pile in behind it. [`Self::reset`] re-arms
/// it (the disk recovered, or an operator cleared it).
#[derive(Default)]
pub struct EnvFaultBreaker {
    inner: Mutex<BreakerInner>,
}

#[derive(Default)]
struct BreakerInner {
    /// Most recent crash instant per session, pruned to the window.
    recent: HashMap<SessionId, Instant>,
    /// When the breaker last opened, if it is open. Used to enforce a
    /// minimum hold before it can re-arm — see [`EnvFaultBreaker::maybe_rearm`].
    tripped_at: Option<Instant>,
}

impl std::fmt::Debug for EnvFaultBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvFaultBreaker").finish_non_exhaustive()
    }
}

impl EnvFaultBreaker {
    /// Fresh, closed (healthy) breaker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one session's crash-restart. Returns `true` only on the
    /// transition from closed to open, so the caller's alert + degrade
    /// side effects run exactly once per episode.
    pub fn record_crash(&self, session: SessionId, now: Instant) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .recent
            .retain(|_, at| now.duration_since(*at) < ENV_FAULT_WINDOW);
        guard.recent.insert(session, now);
        if guard.tripped_at.is_some() || guard.recent.len() < ENV_FAULT_SESSION_THRESHOLD {
            return false;
        }
        guard.tripped_at = Some(now);
        true
    }

    /// Close the breaker again once it has been open for at least
    /// [`ENV_FAULT_WINDOW`]. Returns `true` when this call re-armed it.
    ///
    /// The minimum hold matters. Without it, a breaker tripped by a
    /// non-disk fault (a sick Docker daemon, a wedged mount) would be
    /// re-armed by the very next tick — the disk looks fine, after all —
    /// and the respawn storm would resume immediately. With it, the host
    /// pauses for one window, then genuinely re-evaluates: if the fault
    /// persists, the sessions crash again and the breaker re-trips; if it
    /// cleared, work resumes with nothing lost (the inbound rows stayed
    /// pending the whole time).
    pub fn maybe_rearm(&self, now: Instant) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(at) = guard.tripped_at else {
            return false;
        };
        if now.duration_since(at) < ENV_FAULT_WINDOW {
            return false;
        }
        guard.recent.clear();
        guard.tripped_at = None;
        true
    }

    /// How many distinct sessions have crashed inside the window, as of
    /// `now`. Read-only — pruning is applied to the returned count, not
    /// to the stored map.
    #[must_use]
    pub fn distinct_sessions(&self, now: Instant) -> usize {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .recent
            .values()
            .filter(|at| now.duration_since(**at) < ENV_FAULT_WINDOW)
            .count()
    }

    /// Whether the breaker is currently open (respawns blocked).
    #[must_use]
    pub fn is_tripped(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tripped_at
            .is_some()
    }

    /// Re-arm unconditionally: forget the recorded crashes and close the
    /// breaker. The operator-initiated escape hatch (the time-gated
    /// automatic path is [`Self::maybe_rearm`]).
    pub fn reset(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.recent.clear();
        guard.tripped_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_cclaw::disk::GIB;

    #[test]
    fn disk_space_bands_delegate_to_the_shared_classifier() {
        let healthy = DiskSpace {
            free_bytes: 500 * GIB,
            total_bytes: 1024 * GIB,
        };
        assert!(!healthy.is_exhausted());
        // The 2026-07-18 shape: essentially nothing left.
        let full = DiskSpace {
            free_bytes: 64 * 1024,
            total_bytes: 100 * GIB,
        };
        assert!(full.is_exhausted());
        assert!(full.summary().contains("free of"));
        // Low-but-not-critical must NOT block spawns — refusing at WARN
        // would be more disruptive than the risk it avoids.
        let low = DiskSpace {
            free_bytes: 15 * GIB,
            total_bytes: 30 * GIB,
        };
        assert_eq!(low.level(), DiskLevel::Warn);
        assert!(!low.is_exhausted());
    }

    #[test]
    fn probe_walks_up_to_an_existing_ancestor() {
        // A data dir that does not exist yet (fresh install) must still
        // resolve — the mount is the same as its nearest live ancestor.
        // Exercised against the real `statvfs` (no test seam) so the walk
        // and the degradation behaviour are genuinely covered: an
        // unstattable path yields `None` (unknown), never "full". Failing
        // closed there would freeze every spawn on the box over a bad path.
        let missing = Path::new("/copperclaw/definitely/not/here");
        assert_eq!(nearest_existing_ancestor(missing), Path::new("/"));
        assert!(
            probe_path_uncached(missing).is_some(),
            "resolves via the nearest existing ancestor"
        );
    }

    #[test]
    fn probe_cache_serves_within_ttl_and_refreshes_after() {
        set_disk_override(4 * GIB, 100 * GIB);
        let probe = DiskProbe::new();
        let t0 = Instant::now();
        let first = probe.get(Path::new("/"), t0).expect("reading");
        assert!(first.is_exhausted());
        // Free space "recovers", but a read inside the TTL still serves
        // the cached (stale) value — that is the point of the cache.
        set_disk_override(500 * GIB, 1024 * GIB);
        let cached = probe
            .get(Path::new("/"), t0 + DISK_PROBE_TTL / 2)
            .expect("reading");
        assert_eq!(cached, first, "served from cache inside the TTL");
        // Past the TTL it re-probes and sees the recovery.
        let fresh = probe
            .get(Path::new("/"), t0 + DISK_PROBE_TTL * 2)
            .expect("reading");
        assert!(!fresh.is_exhausted());
        clear_disk_override();
    }

    #[test]
    fn breaker_trips_once_on_distinct_sessions_and_re_arms_on_reset() {
        let breaker = EnvFaultBreaker::new();
        let now = Instant::now();
        let sessions: Vec<SessionId> = (0..ENV_FAULT_SESSION_THRESHOLD)
            .map(|_| SessionId::new())
            .collect();
        for (i, s) in sessions.iter().enumerate() {
            let tripped = breaker.record_crash(*s, now);
            assert_eq!(
                tripped,
                i + 1 == ENV_FAULT_SESSION_THRESHOLD,
                "trip fires exactly on the crossing (session {})",
                i + 1
            );
        }
        assert!(breaker.is_tripped());
        // Every session that piles in afterwards must NOT re-fire the
        // alert — one environment fault, one alert.
        assert!(!breaker.record_crash(SessionId::new(), now));
        breaker.reset();
        assert!(!breaker.is_tripped());
        assert_eq!(breaker.distinct_sessions(now), 0);
    }

    #[test]
    fn breaker_holds_open_for_a_full_window_before_it_can_re_arm() {
        let breaker = EnvFaultBreaker::new();
        let now = Instant::now();
        for _ in 0..ENV_FAULT_SESSION_THRESHOLD {
            breaker.record_crash(SessionId::new(), now);
        }
        assert!(breaker.is_tripped());
        // Mid-window the breaker must stay open even though the caller is
        // asking every tick. Re-arming immediately would resume the storm.
        assert!(!breaker.maybe_rearm(now + ENV_FAULT_WINDOW / 2));
        assert!(breaker.is_tripped());
        // Past the hold, it re-arms and forgets the episode so a future
        // shared fault can trip it cleanly.
        assert!(breaker.maybe_rearm(now + ENV_FAULT_WINDOW));
        assert!(!breaker.is_tripped());
        assert_eq!(breaker.distinct_sessions(now + ENV_FAULT_WINDOW), 0);
        // Idempotent: a second call on a closed breaker is a no-op.
        assert!(!breaker.maybe_rearm(now + ENV_FAULT_WINDOW * 2));
    }

    #[test]
    fn one_session_crashing_repeatedly_never_trips_the_breaker() {
        // The whole point of the distinct-session key: a single wedged
        // session is a per-session bug and belongs to the per-session
        // backoff, not to the global breaker.
        let breaker = EnvFaultBreaker::new();
        let s = SessionId::new();
        let mut when = Instant::now();
        for _ in 0..(ENV_FAULT_SESSION_THRESHOLD * 4) {
            assert!(!breaker.record_crash(s, when));
            when += Duration::from_secs(5);
        }
        assert!(!breaker.is_tripped());
        assert_eq!(breaker.distinct_sessions(when), 1);
    }

    #[test]
    fn crashes_spread_past_the_window_do_not_trip() {
        // Distinct sessions crashing hours apart are unrelated events.
        let breaker = EnvFaultBreaker::new();
        let mut when = Instant::now();
        let mut last = when;
        for _ in 0..(ENV_FAULT_SESSION_THRESHOLD + 2) {
            assert!(!breaker.record_crash(SessionId::new(), when));
            last = when;
            when += ENV_FAULT_WINDOW + Duration::from_secs(1);
        }
        assert!(!breaker.is_tripped());
        assert_eq!(
            breaker.distinct_sessions(last),
            1,
            "only the most recent crash is inside the window"
        );
    }
}
