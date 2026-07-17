//! M21 O3: live, session-scoped provider-health for the runner's failover
//! chain.
//!
//! Before M21, provider health was applied only at container spawn: the host
//! hydrated the health map, selected a provider, and wrote `runner.json`
//! once. In-session the runner walked its pre-built `failover_chain`
//! ([`super::provider_call::FailoverProvider`]) linearly from the primary on
//! *every* call, so a primary that died mid-session burned a per-turn retry
//! against it forever, and a primary that recovered was never restored until
//! the next respawn.
//!
//! This type closes that gap by re-consulting the chain's health state at
//! **every** provider-call construction. It reuses the pure cooldown /
//! re-probe machinery in [`copperclaw_providers::FallbackChain`] — the same
//! semantics the host applies at spawn — so behaviour is consistent across
//! the two brains:
//!
//! * [`Self::select_start`] asks the chain which candidate a call should
//!   START on given live health as of `now`: a cooled-down candidate is
//!   skipped, and once its cooldown lapses it becomes re-probe-eligible and
//!   the primary is selected again (restored).
//! * [`Self::record_failure`] degrades the candidate that just failed (for
//!   the configured cooldown window) so the NEXT call's `select_start`
//!   routes around it — but only for resilience-relevant failures; a 4xx
//!   bad-request never degrades the chain.
//! * [`Self::record_success`] promotes a candidate back to healthy, the
//!   clean "restore on recovery" half.
//! * [`Self::enter_candidate`] tracks which provider the user was last told
//!   is serving, so the caller can emit the existing "switched to
//!   <provider>" HUD note + failover metric on any transition — whether a
//!   mid-call failover or a cross-call switch driven by health.
//!
//! Byte-identical default: a single-candidate chain (the common
//! no-failover-configured case, `failover_chain` empty) always starts on
//! index 0, never produces a transition note, and records health that is
//! never consulted for a switch — so the single-provider path is unchanged.

use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use copperclaw_providers::{ChainEntry, DegradeReason, FallbackChain, HealthMap, ProviderKey};

use super::RunnerDeps;

/// A provider transition the caller should surface: the user was serving
/// `from` and is now switching to `to`. Drives the "switched to <provider>"
/// HUD note and the `provider_failover` metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub from: String,
    pub to: String,
    /// M21 O3 (M1 rider): `true` when the switch moved to a higher-index
    /// fallback (a degrade — a provider failed), `false` when it moved back
    /// toward the primary (a restore — an earlier-degraded provider re-probed
    /// OK). Drives the `direction` label on the live-failover metric.
    pub degrade: bool,
}

/// Session-scoped live health for the runner's provider failover chain.
/// Created once per `run_loop` and shared (by reference) across every
/// `drive_turn` / `run_llm_turn` of the session, so failures and recoveries
/// accumulate across turns and inbounds rather than resetting each call.
pub struct FailoverHealth {
    /// One [`ChainEntry`] per candidate, index 0 = primary. Each entry
    /// carries a single synthetic key (`cand{idx}`) so two candidates that
    /// happen to share a `(provider, model)` never collide in the health
    /// map — the health grain here is "candidate position", the finest the
    /// runner can observe (it holds already-resolved provider handles, not
    /// key ids).
    chain: FallbackChain,
    inner: Mutex<Inner>,
}

struct Inner {
    health: HealthMap,
    /// Provider name the user was last told is serving; `None` until the
    /// first candidate is entered. Drives the cross-call transition note.
    active: Option<String>,
    /// Chain index of the last-entered candidate; `None` until the first
    /// candidate is entered. Lets [`FailoverHealth::enter_candidate`] label a
    /// transition as a degrade (higher index) or restore (lower index).
    active_idx: Option<usize>,
}

impl FailoverHealth {
    /// Build from the ordered candidate `(provider_name, model)` list,
    /// index 0 = primary. Always has at least the primary.
    #[must_use]
    pub fn from_candidates(candidates: Vec<(String, String)>) -> Self {
        let entries = candidates
            .into_iter()
            .enumerate()
            .map(|(idx, (provider, model))| ChainEntry {
                provider,
                model,
                keys: vec![ProviderKey {
                    id: format!("cand{idx}"),
                    api_key_env: None,
                }],
            })
            .collect();
        Self {
            chain: FallbackChain {
                entries,
                ..FallbackChain::default()
            },
            inner: Mutex::new(Inner {
                health: HealthMap::new(),
                active: None,
                active_idx: None,
            }),
        }
    }

    /// Convenience: build the candidate list from a [`RunnerDeps`] — the
    /// primary (`deps.provider` / `deps.model`) followed by each pre-built
    /// [`super::provider_call::FailoverProvider`] in priority order.
    #[must_use]
    pub fn from_deps(deps: &RunnerDeps) -> Self {
        let mut candidates = Vec::with_capacity(1 + deps.failover_chain.len());
        candidates.push((deps.provider.name().to_string(), deps.model.clone()));
        for f in &deps.failover_chain {
            candidates.push((f.provider_name.clone(), f.model.clone()));
        }
        Self::from_candidates(candidates)
    }

    /// The candidate index the current call should START on, given live
    /// health as of `now`. A cooled-down candidate is skipped in favour of
    /// the first eligible one; once every candidate is cooling the chain
    /// falls back to the primary (index 0) rather than going dark. A
    /// single-candidate chain always returns 0.
    #[must_use]
    pub fn select_start(&self, now: DateTime<Utc>) -> usize {
        let g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        self.chain
            .select(None, &g.health, now)
            .map_or(0, |s| s.entry_index)
    }

    /// Mark that candidate `idx` is about to serve this turn. Returns
    /// `Some(Transition)` when its provider differs from the last-announced
    /// one (so the caller emits the "switched to" note + metric); `None` on
    /// the first candidate of the session or when the provider is unchanged.
    #[must_use]
    pub fn enter_candidate(&self, idx: usize) -> Option<Transition> {
        let name = self.chain.entries[idx].provider.clone();
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let prev = g.active.replace(name.clone());
        let prev_idx = g.active_idx.replace(idx);
        match prev {
            // `prev_idx` is `Some` whenever `prev` is (both are set together),
            // so `unwrap_or(0)` is only the unreachable first-candidate case.
            Some(from) if from != name => Some(Transition {
                from,
                to: name,
                degrade: idx > prev_idx.unwrap_or(0),
            }),
            _ => None,
        }
    }

    /// Record a classified terminal failure against candidate `idx`,
    /// degrading it for the chain's cooldown window so the next
    /// [`Self::select_start`] routes around it.
    pub fn record_failure(&self, idx: usize, reason: DegradeReason, now: DateTime<Utc>) {
        let entry = &self.chain.entries[idx];
        let provider = entry.provider.clone();
        let model = entry.model.clone();
        let key_id = entry.keys[0].id.clone();
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        self.chain
            .record_failure(&mut g.health, &provider, &model, &key_id, reason, now);
    }

    /// Record a success against candidate `idx`: promotes it back to healthy
    /// and clears its cooldown — the "restore on recovery" half.
    pub fn record_success(&self, idx: usize) {
        let entry = &self.chain.entries[idx];
        let provider = entry.provider.clone();
        let model = entry.model.clone();
        let key_id = entry.keys[0].id.clone();
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        self.chain
            .record_success(&mut g.health, &provider, &model, &key_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_candidate() -> FailoverHealth {
        FailoverHealth::from_candidates(vec![
            ("anthropic".to_string(), "model-a".to_string()),
            ("ollama".to_string(), "model-b".to_string()),
        ])
    }

    #[test]
    fn single_candidate_always_starts_at_zero() {
        let fh =
            FailoverHealth::from_candidates(vec![("anthropic".to_string(), "model-a".to_string())]);
        let now = Utc::now();
        // Even after a recorded failure a single-candidate chain can only
        // ever start at 0 (there is nowhere else to go) — byte-identical to
        // the historical single-provider path.
        assert_eq!(fh.select_start(now), 0);
        fh.record_failure(0, DegradeReason::ServerError, now);
        assert_eq!(fh.select_start(now), 0);
    }

    #[test]
    fn healthy_chain_starts_on_primary() {
        let fh = two_candidate();
        assert_eq!(fh.select_start(Utc::now()), 0);
    }

    #[test]
    fn dead_primary_moves_start_to_fallback() {
        let fh = two_candidate();
        let now = Utc::now();
        // Primary dies mid-session (a resilience-relevant failure).
        fh.record_failure(0, DegradeReason::ServerError, now);
        // The NEXT call starts on the fallback without any respawn.
        assert_eq!(fh.select_start(now), 1);
    }

    #[test]
    fn recovered_primary_is_restored_after_reprobe_window() {
        let fh = two_candidate();
        let now = Utc::now();
        fh.record_failure(0, DegradeReason::ServerError, now);
        assert_eq!(fh.select_start(now), 1, "degraded to fallback immediately");
        // Past the default 2-minute re-probe window the primary is
        // re-probe-eligible again and selection restores it.
        let later = now + chrono::Duration::minutes(2) + chrono::Duration::seconds(1);
        assert_eq!(fh.select_start(later), 0, "primary restored on re-probe");
    }

    #[test]
    fn record_success_restores_primary_immediately() {
        let fh = two_candidate();
        let now = Utc::now();
        fh.record_failure(0, DegradeReason::ServerError, now);
        assert_eq!(fh.select_start(now), 1);
        // An explicit success (the host re-probe half) promotes it back to
        // healthy right away, no cooldown wait.
        fh.record_success(0);
        assert_eq!(fh.select_start(now), 0);
    }

    #[test]
    fn enter_candidate_notes_only_real_transitions() {
        let fh = two_candidate();
        // First entry of the session: no note (the user expects the primary).
        assert_eq!(fh.enter_candidate(0), None);
        // Re-entering the same provider: no note.
        assert_eq!(fh.enter_candidate(0), None);
        // Switching to the fallback: a transition the caller surfaces (a
        // degrade — moving to a higher chain index).
        assert_eq!(
            fh.enter_candidate(1),
            Some(Transition {
                from: "anthropic".to_string(),
                to: "ollama".to_string(),
                degrade: true,
            })
        );
        // Switching back to the primary (recovery): the reverse transition (a
        // restore — moving to a lower chain index).
        assert_eq!(
            fh.enter_candidate(0),
            Some(Transition {
                from: "ollama".to_string(),
                to: "anthropic".to_string(),
                degrade: false,
            })
        );
    }

    #[test]
    fn same_provider_model_candidates_do_not_collide() {
        // Two candidates that share (provider, model) must track health
        // independently — the synthetic per-candidate key disambiguates.
        let fh = FailoverHealth::from_candidates(vec![
            ("anthropic".to_string(), "model-a".to_string()),
            ("anthropic".to_string(), "model-a".to_string()),
        ]);
        let now = Utc::now();
        fh.record_failure(0, DegradeReason::ServerError, now);
        // Only candidate 0 degraded → selection moves to candidate 1.
        assert_eq!(fh.select_start(now), 1);
    }
}
