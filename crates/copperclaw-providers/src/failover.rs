//! Provider resilience: fallback chains, multi-key rotation, per-channel
//! model pinning, and the health state machine that drives automatic
//! degrade-on-failure + re-probe-and-restore.
//!
//! This module is the *pure logic core* of M16 Phase 4. It owns no I/O and
//! no clock: every decision is a function of the configured [`FallbackChain`],
//! the current [`HealthMap`], and an explicit `now: DateTime<Utc>`. The host
//! crate persists the chain + health to the central DB (migration
//! `020_provider_profiles`) and supplies `now`; the runner-config assembler
//! calls [`FallbackChain::select`] to pick the provider/model/key it writes
//! into `runner.json`.
//!
//! ## The model
//!
//! * A [`FallbackChain`] is an ordered list of [`ChainEntry`]. Position 0 is
//!   the *primary*; later entries are progressively-degraded fallbacks.
//! * Each [`ChainEntry`] names a `(provider, model)` and carries its own set
//!   of [`ProviderKey`] — the multi-key rotation set for that entry. An entry
//!   with no keys runs on the ambient credential (the historical single-key
//!   behaviour).
//! * Each `(provider, model, key_id)` triple has a [`HealthEntry`] recording
//!   whether it is [`HealthStatus::Healthy`], cooling down after a rate-limit
//!   ([`HealthStatus::RateLimited`]), or down after a server error
//!   ([`HealthStatus::Down`]). A degraded entry carries a `cooldown_until`
//!   deadline; once `now` passes it the entry is *re-probe-eligible* and the
//!   selector treats it as healthy again (restoring the primary).
//!
//! ## Default behaviour is unchanged
//!
//! An empty chain ([`FallbackChain::is_empty`]) means "no resilience
//! configured": the host keeps its existing single-provider selection
//! untouched. The whole module is inert until an operator configures a chain.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Sentinel `key_id` for a chain entry that declares no explicit keys: the
/// entry runs on the ambient credential. Kept as a stable string so the
/// health map's key is always well-formed.
pub const AMBIENT_KEY_ID: &str = "";

/// Default cool-down applied to a degraded entry/key before it becomes
/// re-probe-eligible, when the chain doesn't override it. Bounds the
/// recover interval: after this long the selector promotes the entry back
/// to eligible and the host re-probes the primary.
pub const DEFAULT_REPROBE_AFTER: Duration = Duration::minutes(2);

/// One credential/profile within a [`ChainEntry`]. The `api_key_env` names
/// the environment variable the runner reads for this key; rotation picks a
/// healthy `ProviderKey` and skips a rate-limited one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderKey {
    /// Stable identifier, unique within the entry's key list. Used as the
    /// health-map key and surfaced in audit/inspect output.
    pub id: String,
    /// Environment variable the runner reads to obtain this key's secret.
    /// `None` keeps the provider's default env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// One `(provider, model)` step in a [`FallbackChain`], with its multi-key
/// rotation set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainEntry {
    /// Provider kind, e.g. `"anthropic"`, `"ollama"`, `"codex"`.
    pub provider: String,
    /// Provider-native model id for this entry.
    pub model: String,
    /// Multi-key rotation set. Empty means "ambient credential" — the
    /// selector uses the [`AMBIENT_KEY_ID`] sentinel for health tracking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<ProviderKey>,
}

impl ChainEntry {
    /// The key ids this entry tracks health for: each declared key's id, or
    /// the single [`AMBIENT_KEY_ID`] sentinel when no keys are declared.
    #[must_use]
    pub fn key_ids(&self) -> Vec<String> {
        if self.keys.is_empty() {
            vec![AMBIENT_KEY_ID.to_string()]
        } else {
            self.keys.iter().map(|k| k.id.clone()).collect()
        }
    }
}

/// An ordered provider/model fallback chain plus the per-channel model pin
/// map. Persisted per agent group; an empty chain is "no resilience
/// configured".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackChain {
    /// Ordered entries; position 0 is the primary.
    #[serde(default)]
    pub entries: Vec<ChainEntry>,
    /// Per-channel model pin: `channel_type -> model id`. A pinned channel
    /// overrides the active entry's model (the provider stays the active
    /// entry's). Empty means "no pins".
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_by_channel: HashMap<String, String>,
    /// How long a degraded entry/key cools down before re-probe, in seconds.
    /// `None` uses [`DEFAULT_REPROBE_AFTER`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reprobe_after_secs: Option<u64>,
}

/// Health status of one `(provider, model, key_id)` triple.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Eligible for selection.
    Healthy,
    /// Cooling down after a rate-limit (HTTP 429) or overload (529).
    RateLimited,
    /// Cooling down after a server error (5xx) / transport failure.
    Down,
}

impl HealthStatus {
    /// Stable wire/string form for the DB and inspect output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HealthStatus::Healthy => "healthy",
            HealthStatus::RateLimited => "rate_limited",
            HealthStatus::Down => "down",
        }
    }

    /// Parse the DB/wire form. An unrecognised string reads as
    /// [`HealthStatus::Healthy`] so an unknown status never wedges a group
    /// dark.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "rate_limited" => HealthStatus::RateLimited,
            "down" => HealthStatus::Down,
            _ => HealthStatus::Healthy,
        }
    }
}

/// True when `haystack` contains the decimal `status` as a standalone token
/// (digit run not adjacent to other digits) — so `429` matches in
/// "http 429" but neither `4290` nor `81429` does. Used by
/// [`DegradeReason::from_error_text`] to spot a bare HTTP status in a
/// provider's free-form SSE error body without false-matching a longer
/// number that happens to embed the code.
fn contains_status(haystack: &str, status: u16) -> bool {
    let needle = status.to_string();
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(&needle) {
        let start = from + rel;
        let end = start + needle.len();
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_digit();
        let right_ok = end >= bytes.len() || !bytes[end].is_ascii_digit();
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// Why a chain entry/key was last degraded. Short, stable reason strings for
/// audit + inspect.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DegradeReason {
    /// HTTP 429.
    RateLimit,
    /// HTTP 529 / explicit overload signal.
    Overloaded,
    /// HTTP 5xx.
    ServerError,
    /// Transport-level failure (DNS/TCP/TLS/broken stream).
    Transport,
}

impl DegradeReason {
    /// Stable string form persisted to `provider_health.last_reason`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DegradeReason::RateLimit => "rate_limit",
            DegradeReason::Overloaded => "overloaded",
            DegradeReason::ServerError => "server_error",
            DegradeReason::Transport => "transport",
        }
    }

    /// The health status a failure of this kind degrades an entry to. A
    /// rate-limit / overload rotates *off the key* ([`HealthStatus::RateLimited`]);
    /// a server / transport failure marks the whole *entry*
    /// [`HealthStatus::Down`].
    #[must_use]
    pub fn status(self) -> HealthStatus {
        match self {
            DegradeReason::RateLimit | DegradeReason::Overloaded => HealthStatus::RateLimited,
            DegradeReason::ServerError | DegradeReason::Transport => HealthStatus::Down,
        }
    }

    /// Classify a recorded error *string* (as the runner persists it on an
    /// `agent_turns` row) into a degrade reason, or `None` when the text is
    /// not resilience-relevant.
    ///
    /// The host reads turn outcomes from the central DB (not live
    /// `ProviderError` values — those live inside the container), so it
    /// classifies by the stable error-message shape. Crucially the live
    /// reason is NOT a bare `ProviderError` Display: the runner wraps it
    /// (`provider_call::format_provider_failure_reason` emits "provider
    /// rejected the query before streaming started (<err>)" and the stream
    /// path emits "provider stream ended with an error event (<body>)"), so
    /// matching the bare Display by *prefix* misses every real failure. We
    /// therefore match by SUBSTRING and prefer the structured `ProviderError`
    /// markers, falling back to the common provider phrasing on the
    /// stream-body path:
    /// * embedded `"api error <status>: ..."` -> 429 ⇒ rate-limit,
    ///   5xx ⇒ server-error, other (4xx) ⇒ `None`;
    /// * `"transport error"` / connection-reset / timeout phrasing ⇒ transport;
    /// * `"overloaded"` / `529` ⇒ overloaded;
    /// * bare `429` / "rate limit" phrasing ⇒ rate-limit;
    /// * "service unavailable" / "bad gateway" / "internal server error" /
    ///   `500`..`599` phrasing ⇒ server-error.
    ///
    /// Non-resilience shapes (bad request / 4xx, decode, cancelled, session
    /// invalid, deadline) classify to `None` so they never degrade the chain.
    #[must_use]
    pub fn from_error_text(text: &str) -> Option<Self> {
        let t = text.trim().to_ascii_lowercase();
        if t.is_empty() {
            return None;
        }
        // 1. Authoritative: the structured `api error <status>: ...` shape the
        //    `ProviderError::Api` Display produces, found ANYWHERE in the
        //    string (the runner wraps it in a parenthesised prefix). A 4xx
        //    other than 429 is NOT a resilience failure, so return early on
        //    any `api error` marker rather than falling through to the looser
        //    substring heuristics below (which must not reclassify a 400/404).
        if let Some(idx) = t.find("api error ") {
            let rest = &t[idx + "api error ".len()..];
            let status: Option<u16> = rest
                .split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
                .and_then(|s| s.parse().ok());
            return match status {
                Some(429) => Some(DegradeReason::RateLimit),
                Some(s) if (500..600).contains(&s) => Some(DegradeReason::ServerError),
                _ => None,
            };
        }
        // 2. Transport: our own `transport error: ...` marker plus the
        //    connection-level phrasing a provider SSE body may carry.
        if t.contains("transport error")
            || t.contains("connection reset")
            || t.contains("connection refused")
            || t.contains("timed out")
            || t.contains("timeout")
        {
            return Some(DegradeReason::Transport);
        }
        // 3. Overload: our `overloaded` marker, the explicit Anthropic
        //    `overloaded_error`, or a bare 529.
        if t.contains("overload") || contains_status(&t, 529) {
            return Some(DegradeReason::Overloaded);
        }
        // 4. Rate-limit: provider phrasing or a bare 429.
        if t.contains("rate limit") || t.contains("rate-limit") || contains_status(&t, 429) {
            return Some(DegradeReason::RateLimit);
        }
        // 5. Server error: common 5xx phrasing or a bare 5xx status.
        if t.contains("service unavailable")
            || t.contains("bad gateway")
            || t.contains("internal server error")
            || t.contains("gateway timeout")
            || (500..600).any(|s| contains_status(&t, s))
        {
            return Some(DegradeReason::ServerError);
        }
        None
    }

    /// Classify a [`crate::ProviderError`] into a degrade reason, or `None`
    /// when the error is not a resilience-relevant failure (e.g. a bad
    /// request or a cancelled turn must NOT degrade the chain).
    #[must_use]
    pub fn from_error(err: &crate::ProviderError) -> Option<Self> {
        use crate::ProviderError as E;
        match err {
            E::Overloaded => Some(DegradeReason::Overloaded),
            E::Transport(_) => Some(DegradeReason::Transport),
            E::Api { status, .. } => match *status {
                429 => Some(DegradeReason::RateLimit),
                s if s >= 500 => Some(DegradeReason::ServerError),
                _ => None,
            },
            E::SessionInvalid
            | E::Decode(_)
            | E::Cancelled
            | E::BadRequest(_)
            | E::DeadlineExceeded { .. } => None,
        }
    }
}

/// Runtime health of one `(provider, model, key_id)` triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthEntry {
    pub status: HealthStatus,
    /// When set, the entry is cooling down until this instant; `now` past it
    /// means re-probe-eligible.
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub last_reason: Option<String>,
    pub failure_count: u32,
}

impl Default for HealthEntry {
    fn default() -> Self {
        Self {
            status: HealthStatus::Healthy,
            cooldown_until: None,
            last_failure_at: None,
            last_reason: None,
            failure_count: 0,
        }
    }
}

impl HealthEntry {
    /// True when this entry/key is eligible for selection as of `now`:
    /// either healthy, or degraded but past its cool-down deadline (so the
    /// host re-probes it).
    #[must_use]
    pub fn is_eligible(&self, now: DateTime<Utc>) -> bool {
        match self.status {
            HealthStatus::Healthy => true,
            HealthStatus::RateLimited | HealthStatus::Down => {
                self.cooldown_until.is_none_or(|until| now >= until)
            }
        }
    }
}

/// Composite key into [`HealthMap`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HealthKey {
    pub provider: String,
    pub model: String,
    pub key_id: String,
}

impl HealthKey {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        key_id: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            key_id: key_id.into(),
        }
    }
}

/// In-memory snapshot of health for every tracked triple. The host hydrates
/// this from `provider_health` before selection and writes back the deltas
/// after a failure / recovery.
pub type HealthMap = HashMap<HealthKey, HealthEntry>;

/// The provider/model/key the selector chose for one spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Index into [`FallbackChain::entries`] of the chosen entry.
    pub entry_index: usize,
    pub provider: String,
    /// The model to use: the per-channel pin when present, else the entry's
    /// default model.
    pub model: String,
    /// The chosen key id (or [`AMBIENT_KEY_ID`]).
    pub key_id: String,
    /// The chosen key's `api_key_env`, if the entry declared one.
    pub api_key_env: Option<String>,
    /// True when the chosen entry is NOT the primary (index > 0): the group
    /// is running degraded. The host audits this.
    pub degraded: bool,
    /// True when a per-channel pin overrode the entry's default model.
    pub pinned_model: bool,
}

impl FallbackChain {
    /// True when no entries are configured: the resilience layer is inert and
    /// the host keeps its single-provider behaviour.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The configured re-probe cool-down, or [`DEFAULT_REPROBE_AFTER`].
    #[must_use]
    pub fn reprobe_after(&self) -> Duration {
        self.reprobe_after_secs
            .and_then(|s| i64::try_from(s).ok())
            .map_or(DEFAULT_REPROBE_AFTER, Duration::seconds)
    }

    /// Look up a health entry, defaulting to [`HealthEntry::default`]
    /// (healthy) when the map has no row yet — an untracked triple is healthy.
    fn health<'a>(
        health: &'a HealthMap,
        provider: &str,
        model: &str,
        key_id: &str,
    ) -> std::borrow::Cow<'a, HealthEntry> {
        let key = HealthKey::new(provider, model, key_id);
        health.get(&key).map_or_else(
            || std::borrow::Cow::Owned(HealthEntry::default()),
            std::borrow::Cow::Borrowed,
        )
    }

    /// Pick the first eligible key for an entry as of `now`. Returns the key
    /// id + its `api_key_env`. Prefers a fully-healthy key over a
    /// re-probe-eligible (cooled-down) one so a recovered-but-untested key
    /// isn't chosen ahead of a known-good sibling. Returns `None` when every
    /// key is still cooling down.
    fn pick_key(
        entry: &ChainEntry,
        health: &HealthMap,
        now: DateTime<Utc>,
    ) -> Option<(String, Option<String>)> {
        // Two passes: strictly-healthy first, then re-probe-eligible. This
        // keeps a known-good key ahead of a key whose cooldown merely lapsed.
        if entry.keys.is_empty() {
            let h = Self::health(health, &entry.provider, &entry.model, AMBIENT_KEY_ID);
            return h
                .is_eligible(now)
                .then(|| (AMBIENT_KEY_ID.to_string(), None));
        }
        for want_healthy in [true, false] {
            for k in &entry.keys {
                let h = Self::health(health, &entry.provider, &entry.model, &k.id);
                let healthy = h.status == HealthStatus::Healthy;
                if (want_healthy && healthy) || (!want_healthy && h.is_eligible(now)) {
                    return Some((k.id.clone(), k.api_key_env.clone()));
                }
            }
        }
        None
    }

    /// True when the entry (provider+model) has *any* eligible key as of
    /// `now`.
    fn entry_eligible(entry: &ChainEntry, health: &HealthMap, now: DateTime<Utc>) -> bool {
        Self::pick_key(entry, health, now).is_some()
    }

    /// Select the active provider/model/key for a spawn.
    ///
    /// Walks the chain from the primary (index 0) down, returning the first
    /// entry that has an eligible key as of `now`. A per-channel pin
    /// (`channel_type` present in [`Self::model_by_channel`]) overrides the
    /// chosen entry's model. Returns `None` only for an empty chain — when a
    /// chain is configured but every entry is degraded the selector falls
    /// back to the *primary* (index 0) anyway, because going dark is worse
    /// than retrying the (cooling-down) primary; the runner's in-provider
    /// retry/backoff then applies.
    #[must_use]
    pub fn select(
        &self,
        channel_type: Option<&str>,
        health: &HealthMap,
        now: DateTime<Utc>,
    ) -> Option<Selection> {
        if self.entries.is_empty() {
            return None;
        }
        let pin = channel_type
            .and_then(|c| self.model_by_channel.get(c))
            .cloned();

        let chosen_index = self
            .entries
            .iter()
            .position(|e| Self::entry_eligible(e, health, now))
            // Every entry cooling down: fall back to the primary rather than
            // refusing to spawn.
            .unwrap_or(0);

        let entry = &self.entries[chosen_index];
        let (key_id, api_key_env) = Self::pick_key(entry, health, now)
            // The unwrap_or(0) branch may have picked a fully-degraded
            // primary; default to its first key (ambient if none).
            .unwrap_or_else(|| {
                entry.keys.first().map_or_else(
                    || (AMBIENT_KEY_ID.to_string(), None),
                    |k| (k.id.clone(), k.api_key_env.clone()),
                )
            });

        let pinned_model = pin.is_some();
        let model = pin.unwrap_or_else(|| entry.model.clone());

        Some(Selection {
            entry_index: chosen_index,
            provider: entry.provider.clone(),
            model,
            key_id,
            api_key_env,
            degraded: chosen_index > 0,
            pinned_model,
        })
    }

    /// Record a classified failure against a `(provider, model, key_id)`
    /// triple, mutating `health` in place. Sets the status per
    /// [`DegradeReason::status`], stamps a cool-down `reprobe_after` from
    /// `now`, bumps the failure count, and records the reason. Idempotent in
    /// the sense that repeated failures keep extending the cool-down.
    pub fn record_failure(
        &self,
        health: &mut HealthMap,
        provider: &str,
        model: &str,
        key_id: &str,
        reason: DegradeReason,
        now: DateTime<Utc>,
    ) {
        let key = HealthKey::new(provider, model, key_id);
        let entry = health.entry(key).or_default();
        entry.status = reason.status();
        entry.cooldown_until = Some(now + self.reprobe_after());
        entry.last_failure_at = Some(now);
        entry.last_reason = Some(reason.as_str().to_string());
        entry.failure_count = entry.failure_count.saturating_add(1);
    }

    /// Record a successful probe/turn against a triple: clears the degraded
    /// state back to healthy and resets the failure count. This is the
    /// "restore on recovery" half — once the host re-probes a cooled-down
    /// primary and it succeeds, calling this promotes it back to healthy so
    /// future selections prefer it again.
    pub fn record_success(
        &self,
        health: &mut HealthMap,
        provider: &str,
        model: &str,
        key_id: &str,
    ) {
        let key = HealthKey::new(provider, model, key_id);
        let entry = health.entry(key).or_default();
        entry.status = HealthStatus::Healthy;
        entry.cooldown_until = None;
        entry.last_reason = None;
        entry.failure_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;

    fn entry(provider: &str, model: &str, keys: &[&str]) -> ChainEntry {
        ChainEntry {
            provider: provider.into(),
            model: model.into(),
            keys: keys
                .iter()
                .map(|id| ProviderKey {
                    id: (*id).to_string(),
                    api_key_env: Some(format!("KEY_{id}")),
                })
                .collect(),
        }
    }

    fn chain() -> FallbackChain {
        FallbackChain {
            entries: vec![
                entry("anthropic", "claude-sonnet-4-6", &["primary", "backup"]),
                entry("ollama", "qwen3.6:27b", &[]),
            ],
            model_by_channel: HashMap::new(),
            reprobe_after_secs: None,
        }
    }

    #[test]
    fn empty_chain_is_inert() {
        let c = FallbackChain::default();
        assert!(c.is_empty());
        let health = HealthMap::new();
        assert!(c.select(None, &health, Utc::now()).is_none());
    }

    #[test]
    fn healthy_chain_picks_primary_entry_and_first_key() {
        let c = chain();
        let health = HealthMap::new();
        let sel = c.select(None, &health, Utc::now()).unwrap();
        assert_eq!(sel.entry_index, 0);
        assert_eq!(sel.provider, "anthropic");
        assert_eq!(sel.model, "claude-sonnet-4-6");
        assert_eq!(sel.key_id, "primary");
        assert_eq!(sel.api_key_env.as_deref(), Some("KEY_primary"));
        assert!(!sel.degraded);
        assert!(!sel.pinned_model);
    }

    #[test]
    fn rate_limited_key_rotates_to_healthy_sibling() {
        let c = chain();
        let mut health = HealthMap::new();
        let now = Utc::now();
        c.record_failure(
            &mut health,
            "anthropic",
            "claude-sonnet-4-6",
            "primary",
            DegradeReason::RateLimit,
            now,
        );
        // Still the primary entry, but rotated off the rate-limited key.
        let sel = c.select(None, &health, now).unwrap();
        assert_eq!(sel.entry_index, 0);
        assert_eq!(sel.key_id, "backup");
        assert!(!sel.degraded);
    }

    #[test]
    fn primary_down_degrades_to_fallback_entry() {
        let c = chain();
        let mut health = HealthMap::new();
        let now = Utc::now();
        // Both keys of the primary entry go down (5xx).
        for k in ["primary", "backup"] {
            c.record_failure(
                &mut health,
                "anthropic",
                "claude-sonnet-4-6",
                k,
                DegradeReason::ServerError,
                now,
            );
        }
        let sel = c.select(None, &health, now).unwrap();
        assert_eq!(sel.entry_index, 1);
        assert_eq!(sel.provider, "ollama");
        assert!(sel.degraded);
    }

    #[test]
    fn primary_restored_after_cooldown_elapses() {
        let c = chain();
        let mut health = HealthMap::new();
        let now = Utc::now();
        for k in ["primary", "backup"] {
            c.record_failure(
                &mut health,
                "anthropic",
                "claude-sonnet-4-6",
                k,
                DegradeReason::ServerError,
                now,
            );
        }
        // Immediately: degraded to ollama.
        assert_eq!(c.select(None, &health, now).unwrap().entry_index, 1);
        // After the cool-down window: primary is re-probe-eligible again.
        let later = now + DEFAULT_REPROBE_AFTER + Duration::seconds(1);
        let sel = c.select(None, &health, later).unwrap();
        assert_eq!(sel.entry_index, 0, "primary restored on re-probe");
        assert!(!sel.degraded);
    }

    #[test]
    fn record_success_promotes_back_to_healthy() {
        let c = chain();
        let mut health = HealthMap::new();
        let now = Utc::now();
        c.record_failure(
            &mut health,
            "anthropic",
            "claude-sonnet-4-6",
            "primary",
            DegradeReason::RateLimit,
            now,
        );
        c.record_success(&mut health, "anthropic", "claude-sonnet-4-6", "primary");
        let h = health
            .get(&HealthKey::new("anthropic", "claude-sonnet-4-6", "primary"))
            .unwrap();
        assert_eq!(h.status, HealthStatus::Healthy);
        assert_eq!(h.failure_count, 0);
        assert!(h.cooldown_until.is_none());
    }

    #[test]
    fn per_channel_pin_overrides_model_not_provider() {
        let mut c = chain();
        c.model_by_channel
            .insert("telegram".to_string(), "claude-opus-4-1".to_string());
        let health = HealthMap::new();
        // Pinned channel: model overridden.
        let sel = c.select(Some("telegram"), &health, Utc::now()).unwrap();
        assert_eq!(sel.provider, "anthropic");
        assert_eq!(sel.model, "claude-opus-4-1");
        assert!(sel.pinned_model);
        // Unpinned channel: entry's default model.
        let sel = c.select(Some("cli"), &health, Utc::now()).unwrap();
        assert_eq!(sel.model, "claude-sonnet-4-6");
        assert!(!sel.pinned_model);
    }

    #[test]
    fn all_entries_cooling_down_falls_back_to_primary() {
        let c = chain();
        let mut health = HealthMap::new();
        let now = Utc::now();
        // Degrade every key of every entry.
        for k in ["primary", "backup"] {
            c.record_failure(
                &mut health,
                "anthropic",
                "claude-sonnet-4-6",
                k,
                DegradeReason::ServerError,
                now,
            );
        }
        c.record_failure(
            &mut health,
            "ollama",
            "qwen3.6:27b",
            AMBIENT_KEY_ID,
            DegradeReason::ServerError,
            now,
        );
        // Nothing eligible — selector falls back to the primary rather than
        // returning None, so the group never goes dark.
        let sel = c.select(None, &health, now).unwrap();
        assert_eq!(sel.entry_index, 0);
    }

    #[test]
    fn degrade_reason_classifies_errors() {
        assert_eq!(
            DegradeReason::from_error(&ProviderError::Overloaded),
            Some(DegradeReason::Overloaded)
        );
        assert_eq!(
            DegradeReason::from_error(&ProviderError::Transport("x".into())),
            Some(DegradeReason::Transport)
        );
        assert_eq!(
            DegradeReason::from_error(&ProviderError::Api {
                status: 429,
                message: "slow down".into()
            }),
            Some(DegradeReason::RateLimit)
        );
        assert_eq!(
            DegradeReason::from_error(&ProviderError::Api {
                status: 503,
                message: "down".into()
            }),
            Some(DegradeReason::ServerError)
        );
        // Non-resilience errors must NOT degrade the chain.
        assert_eq!(
            DegradeReason::from_error(&ProviderError::BadRequest("nope".into())),
            None
        );
        assert_eq!(
            DegradeReason::from_error(&ProviderError::Api {
                status: 404,
                message: "x".into()
            }),
            None
        );
        assert_eq!(DegradeReason::from_error(&ProviderError::Cancelled), None);
    }

    #[test]
    fn degrade_reason_classifies_error_text() {
        // Round-trips the bare ProviderError Display shapes.
        assert_eq!(
            DegradeReason::from_error_text(&ProviderError::Overloaded.to_string()),
            Some(DegradeReason::Overloaded)
        );
        assert_eq!(
            DegradeReason::from_error_text(&ProviderError::Transport("reset".into()).to_string()),
            Some(DegradeReason::Transport)
        );
        assert_eq!(
            DegradeReason::from_error_text(
                &ProviderError::Api {
                    status: 429,
                    message: "slow down".into()
                }
                .to_string()
            ),
            Some(DegradeReason::RateLimit)
        );
        assert_eq!(
            DegradeReason::from_error_text(
                &ProviderError::Api {
                    status: 503,
                    message: "down".into()
                }
                .to_string()
            ),
            Some(DegradeReason::ServerError)
        );
        // Non-resilience shapes -> None.
        assert_eq!(
            DegradeReason::from_error_text(
                &ProviderError::Api {
                    status: 404,
                    message: "x".into()
                }
                .to_string()
            ),
            None
        );
        assert_eq!(
            DegradeReason::from_error_text(&ProviderError::BadRequest("nope".into()).to_string()),
            None
        );
        assert_eq!(DegradeReason::from_error_text(""), None);
        assert_eq!(DegradeReason::from_error_text("weird garbage"), None);
    }

    #[test]
    fn degrade_reason_classifies_real_wrapped_reason_strings() {
        // These are the EXACT shapes the live runner persists into
        // `agent_turns.error` (see `provider_call::format_provider_failure_reason`
        // and the stream-error path). The old prefix-matching classifier
        // missed every one of them, so the live failover never fired.

        // Query-time wrapper around `ProviderError::Api { 429 }`.
        let q429 =
            "provider rejected the query before streaming started (api error 429: rate limited)";
        assert_eq!(
            DegradeReason::from_error_text(q429),
            Some(DegradeReason::RateLimit)
        );
        // Query-time wrapper around a 5xx.
        let q503 = "provider rejected the query before streaming started \
                    (api error 503: upstream connect error)";
        assert_eq!(
            DegradeReason::from_error_text(q503),
            Some(DegradeReason::ServerError)
        );
        // Query-time wrapper around overloaded.
        let qover = "provider rejected the query before streaming started (overloaded)";
        assert_eq!(
            DegradeReason::from_error_text(qover),
            Some(DegradeReason::Overloaded)
        );
        // Query-time wrapper around a transport error.
        let qtrans = "provider rejected the query before streaming started \
                      (transport error: connection reset by peer)";
        assert_eq!(
            DegradeReason::from_error_text(qtrans),
            Some(DegradeReason::Transport)
        );
        // Query-time wrapper around a 4xx -> NOT a resilience failure.
        let q400 =
            "provider rejected the query before streaming started (api error 400: prompt too long)";
        assert_eq!(DegradeReason::from_error_text(q400), None);

        // Stream-time path: bare sentinel with no embedded marker stays inert
        // (we can't tell what failed, so we don't degrade on it alone).
        let bare = "provider stream ended with an error event";
        assert_eq!(DegradeReason::from_error_text(bare), None);
        // Stream-time path carrying the provider's own body: classify by the
        // common phrasing providers actually emit.
        let s_over = "provider stream ended with an error event (overloaded_error: \
                      Anthropic is temporarily overloaded)";
        assert_eq!(
            DegradeReason::from_error_text(s_over),
            Some(DegradeReason::Overloaded)
        );
        let s_rl = "provider stream ended with an error event (HTTP 429 Too Many Requests)";
        assert_eq!(
            DegradeReason::from_error_text(s_rl),
            Some(DegradeReason::RateLimit)
        );
        let s_5xx = "provider stream ended with an error event (503 Service Unavailable)";
        assert_eq!(
            DegradeReason::from_error_text(s_5xx),
            Some(DegradeReason::ServerError)
        );
        let s_timeout = "provider stream ended with an error event (upstream request timed out)";
        assert_eq!(
            DegradeReason::from_error_text(s_timeout),
            Some(DegradeReason::Transport)
        );
    }

    #[test]
    fn contains_status_matches_only_standalone_tokens() {
        assert!(contains_status("http 429 too many", 429));
        assert!(contains_status("(429)", 429));
        assert!(contains_status("429", 429));
        // Embedded in a longer number must NOT match.
        assert!(!contains_status("error 14290 occurred", 429));
        assert!(!contains_status("4295", 429));
        assert!(!contains_status("x4290", 429));
    }

    #[test]
    fn health_status_parse_is_forgiving() {
        assert_eq!(HealthStatus::parse("healthy"), HealthStatus::Healthy);
        assert_eq!(
            HealthStatus::parse("rate_limited"),
            HealthStatus::RateLimited
        );
        assert_eq!(HealthStatus::parse("down"), HealthStatus::Down);
        // Unknown -> healthy (never wedge dark).
        assert_eq!(HealthStatus::parse("garbage"), HealthStatus::Healthy);
    }

    #[test]
    fn chain_json_roundtrip() {
        let c = chain();
        let s = serde_json::to_string(&c).unwrap();
        let back: FallbackChain = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn reprobe_after_uses_override_then_default() {
        let mut c = FallbackChain::default();
        assert_eq!(c.reprobe_after(), DEFAULT_REPROBE_AFTER);
        c.reprobe_after_secs = Some(30);
        assert_eq!(c.reprobe_after(), Duration::seconds(30));
    }

    #[test]
    fn key_ids_uses_ambient_sentinel_when_no_keys() {
        let e = entry("ollama", "m", &[]);
        assert_eq!(e.key_ids(), vec![AMBIENT_KEY_ID.to_string()]);
        let e = entry("anthropic", "m", &["a", "b"]);
        assert_eq!(e.key_ids(), vec!["a".to_string(), "b".to_string()]);
    }
}
