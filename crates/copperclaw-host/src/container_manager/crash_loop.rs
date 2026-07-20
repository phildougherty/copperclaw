//! Crash-loop bookkeeping — M21 S4, architecture decision (e).
//!
//! Before this module, `ReconcileAction::CrashRestart` had no backoff: a
//! session whose container died on every boot (an OOM loop being the
//! canonical case) was torn down and respawned once per reconcile tick,
//! forever — a tight kill loop with no user signal beyond the generic
//! restart apology each time. This module owns the two pieces decision
//! (e) adds:
//!
//! - **Per-session restart backoff** on the same curve the S1 loop
//!   supervisor uses ([`crate::supervisor::BACKOFF_STEPS`]: 5s -> 15s ->
//!   60s -> 300s cap), with the streak resetting after
//!   [`crate::supervisor::HEALTHY_RESET_WINDOW`] (10 minutes) without a
//!   crash. The crash-restart teardown (log capture, container removal,
//!   apology) still happens immediately — only the respawn is deferred.
//! - **OOM episode tracking**: each crash carries a [`CrashCause`]
//!   classified from the container's exit status (Docker `State.OOMKilled`
//!   or exit 137). After [`OOM_CARD_THRESHOLD`] OOM kills within one
//!   episode the manager emits exactly ONE user-facing `ErrorCard`
//!   ("this task keeps running out of memory — an operator can raise
//!   `memory_mb`"); the dedup flag clears when the episode ends (the
//!   healthy-reset window elapses without a crash).
//!
//! State is in-memory on [`crate::container_manager::ContainerManager`]
//! and deliberately NOT persisted across host restarts — boot's recovery
//! path re-baselines every session anyway (decision (e), "don't
//! re-litigate").
//!
//! All timing uses `tokio::time::Instant` so tests drive the curve with a
//! paused clock — no real waits.

use crate::supervisor::{BACKOFF_STEPS, HEALTHY_RESET_WINDOW};
use copperclaw_container_rt::ContainerExitStatus;
use copperclaw_types::SessionId;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};

/// OOM kills within one episode before the user gets the single
/// "keeps running out of memory" `ErrorCard`.
pub const OOM_CARD_THRESHOLD: u32 = 3;

/// F2 safe-mode respawn: consecutive crashes in the current episode before
/// the host spawns the runner in recovery mode (forcing aggressive
/// history truncation at startup — see the runner's `recovery_mode`). Set
/// higher than the first couple of backoff steps so a transient double-crash
/// doesn't trip it, but low enough that a genuinely stuck session self-heals
/// within a few minutes of backoff rather than looping on a flat curve
/// forever (the 2026-07-18 incident, where the streak climbed past 15).
pub const RECOVERY_MODE_STREAK: u32 = 5;

/// Why a container crash-restarted, as classified from the runtime's
/// exit-status inspection at capture time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashCause {
    /// The kernel OOM killer terminated the container (Docker
    /// `State.OOMKilled`, or exit code 137 — SIGKILL, which for a
    /// memory-limited session container almost always means the OOM
    /// killer even when the flag is unset, e.g. cgroup-v2 child kills).
    OomKill,
    /// The *host* was out of resources when the container died — the disk
    /// probe reported free space below the failure floor at crash time.
    ///
    /// Added after 2026-07-18, where 235 crash-restarts were logged as
    /// `cause="generic"` while the real cause was a full disk sitting
    /// right there in a different log line (`StorageFull`, `SQLite` `disk
    /// I/O error`). An operator reading the crash lines had no thread back
    /// to the disk. This variant is that thread. It is never *guessed*:
    /// it is set only when the probe actually read a full filesystem.
    ResourceExhausted,
    /// Any other crash: stale heartbeat with a nonzero exit, an
    /// uninspectable/already-gone container, a runner panic, etc.
    Generic,
}

impl CrashCause {
    /// Classify a crash from the runtime's inspect surface. `None`
    /// (backend can't inspect, container already gone) is a generic
    /// crash — never guess OOM without evidence.
    #[must_use]
    pub fn from_exit_status(status: Option<&ContainerExitStatus>) -> Self {
        match status {
            Some(s) if s.oom_killed || s.exit_code == Some(137) => Self::OomKill,
            _ => Self::Generic,
        }
    }

    /// Refine this cause with what the host's disk probe saw at crash
    /// time. A [`Self::Generic`] crash on a box whose filesystem is below
    /// the failure floor is almost certainly dying *of* the full disk, so
    /// it is relabelled [`Self::ResourceExhausted`].
    ///
    /// [`Self::OomKill`] is deliberately NOT overridden: the memory
    /// ceiling is both more specific and more actionable (raise
    /// `memory_mb`), and a full disk does not make an OOM kill any less of
    /// an OOM kill. And an exhausted disk is never *guessed* — this only
    /// ever fires when a probe actually read a full filesystem.
    #[must_use]
    pub fn with_host_disk(self, disk_exhausted: bool) -> Self {
        if disk_exhausted && self == Self::Generic {
            Self::ResourceExhausted
        } else {
            self
        }
    }

    /// Stable lowercase token for logs (and the M1 metrics rider).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OomKill => "oom_kill",
            Self::ResourceExhausted => "resource_exhausted",
            Self::Generic => "generic",
        }
    }
}

/// What [`CrashLoopTracker::record_crash`] decided for this crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashRecord {
    /// How long the respawn is deferred (the backoff step for this
    /// streak position).
    pub delay: Duration,
    /// Crashes since the episode began (1-based; indexes the curve).
    pub streak: u32,
    /// OOM kills within this episode so far (including this one when
    /// the cause was [`CrashCause::OomKill`]).
    pub oom_count: u32,
    /// True exactly once per episode: this crash pushed the OOM count
    /// to [`OOM_CARD_THRESHOLD`] and no card has been emitted for the
    /// episode yet. The caller emits the `ErrorCard` when this is set.
    pub emit_oom_card: bool,
}

/// Per-session crash bookkeeping. One entry per session that has
/// crash-restarted at least once; entries reset lazily (a crash after
/// the healthy window starts a fresh episode) and the map is bounded by
/// the number of sessions that ever crash in one host lifetime.
struct Entry {
    /// Crashes in the current episode.
    streak: u32,
    /// OOM kills in the current episode.
    oom_count: u32,
    /// Whether this episode's single OOM card has been emitted.
    oom_card_emitted: bool,
    /// When the most recent crash was recorded — the healthy-reset
    /// reference point.
    last_crash: Instant,
    /// Respawns are deferred until this instant.
    not_before: Instant,
}

/// Thread-safe per-session crash-loop tracker. Owned by the
/// `ContainerManager`; consulted by `classify` (spawn gating) and
/// updated by `restart_container` (crash and stuck-tool restarts both
/// record here — M21 S2 rides the same backoff so a repeatedly-stuck
/// session cannot hot-loop).
#[derive(Default)]
pub struct CrashLoopTracker {
    inner: Mutex<HashMap<SessionId, Entry>>,
}

impl std::fmt::Debug for CrashLoopTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrashLoopTracker").finish_non_exhaustive()
    }
}

impl CrashLoopTracker {
    /// Fresh tracker with no recorded crashes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a crash for `session` at `now` and compute the respawn
    /// backoff. If the previous crash is older than
    /// [`HEALTHY_RESET_WINDOW`], the episode resets first (streak and
    /// OOM count start over, the OOM-card dedup flag clears) — a
    /// session that ran healthy for 10 minutes earned a fresh curve.
    pub fn record_crash(&self, session: SessionId, cause: CrashCause, now: Instant) -> CrashRecord {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.entry(session).or_insert(Entry {
            streak: 0,
            oom_count: 0,
            oom_card_emitted: false,
            last_crash: now,
            not_before: now,
        });
        if now.duration_since(entry.last_crash) >= HEALTHY_RESET_WINDOW {
            entry.streak = 0;
            entry.oom_count = 0;
            entry.oom_card_emitted = false;
        }
        entry.streak = entry.streak.saturating_add(1);
        entry.last_crash = now;
        let step = (entry.streak as usize - 1).min(BACKOFF_STEPS.len() - 1);
        let delay = BACKOFF_STEPS[step];
        entry.not_before = now + delay;
        let mut emit_oom_card = false;
        if cause == CrashCause::OomKill {
            entry.oom_count = entry.oom_count.saturating_add(1);
            if entry.oom_count >= OOM_CARD_THRESHOLD && !entry.oom_card_emitted {
                entry.oom_card_emitted = true;
                emit_oom_card = true;
            }
        }
        CrashRecord {
            delay,
            streak: entry.streak,
            oom_count: entry.oom_count,
            emit_oom_card,
        }
    }

    /// The current consecutive-crash streak for `session`, reset-aware:
    /// returns `0` when the session has never crashed OR when the last crash
    /// is older than [`HEALTHY_RESET_WINDOW`] (the episode has ended and a
    /// fresh spawn earned its clean slate). Read at spawn time by the host's
    /// runner-config assembly to decide whether to flip `recovery_mode` on
    /// (F2 safe-mode respawn). Never mutates state — the actual reset is
    /// applied lazily by the next [`Self::record_crash`].
    #[must_use]
    pub fn current_streak(&self, session: SessionId, now: Instant) -> u32 {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.get(&session) {
            Some(entry) if now.duration_since(entry.last_crash) < HEALTHY_RESET_WINDOW => {
                entry.streak
            }
            _ => 0,
        }
    }

    /// How much longer `session`'s respawn is deferred, or `None` when
    /// a spawn is allowed (never crashed, or the backoff elapsed).
    #[must_use]
    pub fn spawn_delay_remaining(&self, session: SessionId, now: Instant) -> Option<Duration> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = guard.get(&session)?;
        if now < entry.not_before {
            Some(entry.not_before - now)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn backoff_walks_the_curve_and_caps() {
        let tracker = CrashLoopTracker::new();
        let s = SessionId::new();
        let now = t0();
        // Six crashes back-to-back: 5s, 15s, 60s, 300s, then pinned at
        // the 300s cap.
        let expected = [5u64, 15, 60, 300, 300, 300];
        let mut when = now;
        for (i, want_secs) in expected.iter().enumerate() {
            let rec = tracker.record_crash(s, CrashCause::Generic, when);
            assert_eq!(
                rec.delay,
                Duration::from_secs(*want_secs),
                "crash {} delay",
                i + 1
            );
            assert_eq!(rec.streak, u32::try_from(i).unwrap() + 1);
            // The next crash happens right when the backoff elapses —
            // well inside the healthy window, so the streak continues.
            when += rec.delay;
        }
    }

    #[test]
    fn spawn_delay_remaining_tracks_the_window() {
        let tracker = CrashLoopTracker::new();
        let s = SessionId::new();
        let now = t0();
        // Never crashed: no deferral.
        assert_eq!(tracker.spawn_delay_remaining(s, now), None);
        let rec = tracker.record_crash(s, CrashCause::Generic, now);
        assert_eq!(rec.delay, Duration::from_secs(5));
        // Mid-window: the remainder shrinks as time passes.
        assert_eq!(
            tracker.spawn_delay_remaining(s, now + Duration::from_secs(2)),
            Some(Duration::from_secs(3))
        );
        // At/after the boundary: allowed.
        assert_eq!(
            tracker.spawn_delay_remaining(s, now + Duration::from_secs(5)),
            None
        );
    }

    #[test]
    fn current_streak_tracks_record_crash_and_resets_with_window() {
        // F2: the recovery-mode decision reads this at spawn time.
        let tracker = CrashLoopTracker::new();
        let s = SessionId::new();
        let now = t0();
        // Never crashed: streak 0 → recovery mode stays off.
        assert_eq!(tracker.current_streak(s, now), 0);
        // Walk the streak up to the recovery threshold; each crash lands
        // right when the previous backoff elapsed (inside the healthy
        // window), so the streak accumulates.
        let mut when = now;
        for expect in 1..=RECOVERY_MODE_STREAK {
            let rec = tracker.record_crash(s, CrashCause::Generic, when);
            assert_eq!(tracker.current_streak(s, when), expect);
            assert_eq!(rec.streak, expect);
            when += rec.delay;
        }
        assert!(
            tracker.current_streak(s, when) >= RECOVERY_MODE_STREAK,
            "streak reached the recovery threshold"
        );
        // After the healthy window with no crash, the read reports 0 even
        // though the entry still holds the old streak (reset is lazy).
        let later = when + HEALTHY_RESET_WINDOW;
        assert_eq!(
            tracker.current_streak(s, later),
            0,
            "a healthy window ends the episode from the reader's view"
        );
    }

    #[test]
    fn healthy_window_resets_the_streak_and_episode() {
        let tracker = CrashLoopTracker::new();
        let s = SessionId::new();
        let now = t0();
        let _ = tracker.record_crash(s, CrashCause::OomKill, now);
        let r2 = tracker.record_crash(s, CrashCause::OomKill, now + Duration::from_secs(5));
        assert_eq!(r2.delay, Duration::from_secs(15));
        assert_eq!(r2.oom_count, 2);

        // 10 minutes without a crash: the next crash starts a fresh
        // episode on the FIRST backoff step with a zeroed OOM count.
        let later = now + Duration::from_secs(5) + HEALTHY_RESET_WINDOW;
        let r3 = tracker.record_crash(s, CrashCause::OomKill, later);
        assert_eq!(r3.delay, Duration::from_secs(5), "curve starts over");
        assert_eq!(r3.streak, 1);
        assert_eq!(r3.oom_count, 1, "OOM count is per-episode");
    }

    #[test]
    fn oom_threshold_emits_exactly_one_card_per_episode() {
        let tracker = CrashLoopTracker::new();
        let s = SessionId::new();
        let mut when = t0();
        // First two OOMs: below threshold, no card.
        for _ in 0..2 {
            let rec = tracker.record_crash(s, CrashCause::OomKill, when);
            assert!(!rec.emit_oom_card);
            when += rec.delay;
        }
        // Third OOM: the single card.
        let rec = tracker.record_crash(s, CrashCause::OomKill, when);
        assert!(rec.emit_oom_card, "third OOM in the episode emits the card");
        when += rec.delay;
        // Fourth and fifth: deduped.
        for _ in 0..2 {
            let rec = tracker.record_crash(s, CrashCause::OomKill, when);
            assert!(!rec.emit_oom_card, "one card per episode, ever");
            when += rec.delay;
        }
        // A fresh episode (healthy window elapsed) can earn a new card
        // after another three OOMs.
        when += HEALTHY_RESET_WINDOW;
        for i in 0..3 {
            let rec = tracker.record_crash(s, CrashCause::OomKill, when);
            assert_eq!(rec.emit_oom_card, i == 2, "new episode re-arms the card");
            when += rec.delay;
        }
    }

    #[test]
    fn generic_crashes_never_emit_the_oom_card() {
        let tracker = CrashLoopTracker::new();
        let s = SessionId::new();
        let mut when = t0();
        for _ in 0..6 {
            let rec = tracker.record_crash(s, CrashCause::Generic, when);
            assert_eq!(rec.oom_count, 0);
            assert!(!rec.emit_oom_card);
            when += rec.delay;
        }
    }

    #[test]
    fn mixed_causes_count_only_ooms_toward_the_card() {
        let tracker = CrashLoopTracker::new();
        let s = SessionId::new();
        let mut when = t0();
        // OOM, generic, OOM, generic, OOM: card fires on the third OOM
        // (fifth crash), not the third crash.
        let causes = [
            CrashCause::OomKill,
            CrashCause::Generic,
            CrashCause::OomKill,
            CrashCause::Generic,
            CrashCause::OomKill,
        ];
        for (i, cause) in causes.iter().enumerate() {
            let rec = tracker.record_crash(s, *cause, when);
            assert_eq!(rec.emit_oom_card, i == 4, "crash {}", i + 1);
            when += rec.delay;
        }
    }

    #[test]
    fn sessions_are_independent() {
        let tracker = CrashLoopTracker::new();
        let a = SessionId::new();
        let b = SessionId::new();
        let now = t0();
        let _ = tracker.record_crash(a, CrashCause::Generic, now);
        let _ = tracker.record_crash(a, CrashCause::Generic, now + Duration::from_secs(5));
        // Session B's first crash starts at the first step regardless.
        let rec = tracker.record_crash(b, CrashCause::Generic, now);
        assert_eq!(rec.delay, Duration::from_secs(5));
        assert_eq!(rec.streak, 1);
    }

    #[test]
    fn crash_cause_classification_matches_decision_e() {
        // Exit 137 alone is an OOM kill (cgroup-v2 child kills leave the
        // flag unset).
        let exit_137 = ContainerExitStatus {
            exit_code: Some(137),
            oom_killed: false,
        };
        assert_eq!(
            CrashCause::from_exit_status(Some(&exit_137)),
            CrashCause::OomKill
        );
        // The OOMKilled flag alone is an OOM kill regardless of code.
        let flagged = ContainerExitStatus {
            exit_code: Some(0),
            oom_killed: true,
        };
        assert_eq!(
            CrashCause::from_exit_status(Some(&flagged)),
            CrashCause::OomKill
        );
        // Any other exit is generic.
        let plain = ContainerExitStatus {
            exit_code: Some(1),
            oom_killed: false,
        };
        assert_eq!(
            CrashCause::from_exit_status(Some(&plain)),
            CrashCause::Generic
        );
        // Unknown status (no inspect surface / container gone) never
        // guesses OOM.
        assert_eq!(CrashCause::from_exit_status(None), CrashCause::Generic);
    }

    #[test]
    fn crash_cause_tokens_are_stable() {
        assert_eq!(CrashCause::OomKill.as_str(), "oom_kill");
        assert_eq!(CrashCause::ResourceExhausted.as_str(), "resource_exhausted");
        assert_eq!(CrashCause::Generic.as_str(), "generic");
    }

    #[test]
    fn a_full_disk_relabels_generic_crashes_but_not_oom_kills() {
        // The 2026-07-18 shape: 235 crashes logged `cause="generic"` while
        // the box was out of disk. A generic crash on a full filesystem
        // now names the disk.
        assert_eq!(
            CrashCause::Generic.with_host_disk(true),
            CrashCause::ResourceExhausted
        );
        // Healthy disk: untouched. Exhaustion is never guessed.
        assert_eq!(
            CrashCause::Generic.with_host_disk(false),
            CrashCause::Generic
        );
        // An OOM kill keeps its own (more specific, more actionable)
        // classification even when the disk is also full.
        assert_eq!(
            CrashCause::OomKill.with_host_disk(true),
            CrashCause::OomKill
        );
        // Idempotent — a second pass cannot re-label it into something else.
        assert_eq!(
            CrashCause::ResourceExhausted.with_host_disk(true),
            CrashCause::ResourceExhausted
        );
    }
}
