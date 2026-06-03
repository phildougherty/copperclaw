//! Host-side bridge for provider resilience (M16 Phase 4).
//!
//! The pure fallback/rotation/pin logic lives in
//! [`copperclaw_providers::failover`]; the persisted chain + health live in
//! the central DB (`provider_profiles` / `provider_health`, migration
//! `020_provider_profiles`). This module is the glue between them and the
//! runner-config assembler:
//!
//! 1. [`load_chain`] parses the per-group JSON chain + pin map into a typed
//!    [`FallbackChain`]. A group with no profile row yields `None`, and the
//!    assembler keeps its historical single-provider selection untouched.
//! 2. [`hydrate_health`] reads the group's `provider_health` rows into an
//!    in-memory [`HealthMap`].
//! 3. [`ContainerManager::resolve_provider_selection`] ties it together:
//!    resolves the triggering channel (for per-channel pinning), selects the
//!    active entry/key as of `now`, persists the chosen entry's health row
//!    (so the entry/key is tracked even before its first failure), and audits
//!    a `provider.degrade` event when the chosen entry is not the primary.
//!
//! The "degrade on failure" + "restore on recovery" half is
//! [`ContainerManager::fold_recent_turns`], run at the top of
//! `resolve_provider_selection`: it reads the runner's recently-reported
//! `agent_turns` outcomes (the host cannot see the in-container
//! `ProviderError` directly) and degrades a `(provider, model, key)` triple on
//! a classified failure, or restores it on a later success (the re-probe).

use super::ContainerManager;
use copperclaw_db::session::{SessionPaths, open_inbound};
use copperclaw_db::tables::{audit_log, messages_in, provider_profiles};
use copperclaw_providers::failover::{
    ChainEntry, DegradeReason, FallbackChain, HealthEntry, HealthKey, HealthMap, HealthStatus,
    Selection,
};
use copperclaw_types::{AgentGroupId, Session};
use tracing::warn;

/// Parse the per-group provider profile (chain + pin map) into a typed
/// [`FallbackChain`]. Returns `None` when the group has no profile row or its
/// chain is empty — the caller keeps single-provider behaviour. A malformed
/// chain blob logs and reads as `None` (fail safe: never wedge a group dark
/// on a parse error).
pub fn load_chain(
    central: &copperclaw_db::central::CentralDb,
    id: AgentGroupId,
) -> Option<FallbackChain> {
    let profile = match provider_profiles::get_chain(central, id) {
        Ok(Some(p)) => p,
        Ok(None) => return None,
        Err(err) => {
            warn!(agent_group = %id.as_uuid(), ?err, "provider failover: chain read failed");
            return None;
        }
    };
    let entries: Vec<ChainEntry> = if profile.chain.is_null() {
        Vec::new()
    } else {
        match serde_json::from_value(profile.chain.clone()) {
            Ok(e) => e,
            Err(err) => {
                warn!(agent_group = %id.as_uuid(), ?err, "provider failover: malformed chain JSON; ignoring");
                return None;
            }
        }
    };
    if entries.is_empty() {
        return None;
    }
    let model_by_channel = if profile.model_by_channel.is_null() {
        std::collections::HashMap::new()
    } else {
        serde_json::from_value(profile.model_by_channel.clone()).unwrap_or_else(|err| {
            warn!(agent_group = %id.as_uuid(), ?err, "provider failover: malformed model_by_channel; ignoring pins");
            std::collections::HashMap::new()
        })
    };
    Some(FallbackChain {
        entries,
        model_by_channel,
        reprobe_after_secs: profile.reprobe_after_secs,
    })
}

/// Read every `provider_health` row for a group into an in-memory
/// [`HealthMap`]. Failures log and yield an empty map (everything reads as
/// healthy — fail safe).
pub fn hydrate_health(central: &copperclaw_db::central::CentralDb, id: AgentGroupId) -> HealthMap {
    let rows = match provider_profiles::list_health(central, id) {
        Ok(r) => r,
        Err(err) => {
            warn!(agent_group = %id.as_uuid(), ?err, "provider failover: health read failed");
            return HealthMap::new();
        }
    };
    rows.into_iter()
        .map(|r| {
            let key = HealthKey::new(r.provider, r.model, r.key_id);
            let entry = HealthEntry {
                status: HealthStatus::parse(&r.status),
                cooldown_until: r.cooldown_until,
                last_failure_at: r.last_failure_at,
                last_reason: r.last_reason,
                failure_count: r.failure_count,
            };
            (key, entry)
        })
        .collect()
}

/// Persist one health entry back to the DB. Best-effort: a write failure logs
/// but does not block the spawn.
fn persist_health(
    central: &copperclaw_db::central::CentralDb,
    id: AgentGroupId,
    provider: &str,
    model: &str,
    key_id: &str,
    entry: &HealthEntry,
    now: chrono::DateTime<chrono::Utc>,
) {
    let req = provider_profiles::UpsertHealth {
        agent_group_id: id,
        provider: provider.to_string(),
        model: model.to_string(),
        key_id: key_id.to_string(),
        status: entry.status.as_str().to_string(),
        cooldown_until: entry.cooldown_until,
        last_failure_at: entry.last_failure_at,
        last_reason: entry.last_reason.clone(),
        failure_count: entry.failure_count,
    };
    if let Err(err) = provider_profiles::upsert_health(central, &req, now) {
        warn!(agent_group = %id.as_uuid(), ?err, "provider failover: health write failed");
    }
}

impl ContainerManager {
    /// Resolve the triggering channel type for a session from its newest
    /// pending inbound, for per-channel model pinning. `None` for
    /// agent-to-agent traffic (no channel sender) or when the inbound DB
    /// can't be read.
    fn triggering_channel(&self, session: &Session) -> Option<String> {
        let paths = SessionPaths::new(&self.cfg.data_dir, session.agent_group_id, session.id);
        let conn = open_inbound(&paths).ok()?;
        let pending = messages_in::get_pending(&conn, true, 1).ok()?;
        let row = pending.first()?;
        row.channel_type.as_ref().map(|c| c.as_str().to_string())
    }

    /// Resolve the active provider/model/key for a spawn against the group's
    /// configured fallback chain + live health. Returns `None` when no chain
    /// is configured (the assembler then keeps its single-provider path).
    ///
    /// Side effects (all best-effort, none block the spawn):
    /// * persists the chosen entry/key's health row so it is tracked even
    ///   before its first failure;
    /// * audits a `provider.degrade` event when the chosen entry is not the
    ///   primary (the group is running degraded).
    pub(crate) fn resolve_provider_selection(
        &self,
        session: &Session,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<Selection> {
        let id = session.agent_group_id;
        let chain = load_chain(&self.central, id)?;
        // Fold the runner's recently-reported turn outcomes into health before
        // selecting: a turn that failed with a rate-limit / 5xx / overload
        // degrades its `(provider, model, key_id)` triple; a successful turn on
        // a degraded triple restores it (the re-probe-and-restore half). This
        // is how an automatic degrade happens — the host reads the outcomes the
        // runner persisted to `agent_turns` (it cannot see the in-container
        // `ProviderError` directly).
        self.fold_recent_turns(&chain, id, now);
        let health = hydrate_health(&self.central, id);
        let channel = self.triggering_channel(session);
        let sel = chain.select(channel.as_deref(), &health, now)?;

        // Track the chosen entry/key's health up-front so `cclaw groups
        // provider status` shows every selected triple, and so a later
        // failure/recovery updates an existing row rather than racing to
        // create one. Only when no row exists yet (don't clobber an active
        // cool-down with a fresh "healthy").
        if provider_profiles::get_health(&self.central, id, &sel.provider, &sel.model, &sel.key_id)
            .ok()
            .flatten()
            .is_none()
        {
            persist_health(
                &self.central,
                id,
                &sel.provider,
                &sel.model,
                &sel.key_id,
                &HealthEntry::default(),
                now,
            );
        }

        if sel.degraded {
            self.audit_provider_degrade(session, &sel);
        }
        Some(sel)
    }

    /// Audit a degrade: the spawn selected a non-primary chain entry because
    /// the primary is unhealthy. Best-effort (audit failures only log).
    fn audit_provider_degrade(&self, session: &Session, sel: &Selection) {
        let args = serde_json::json!({
            "session_id": session.id.as_uuid().to_string(),
            "entry_index": sel.entry_index,
            "provider": sel.provider,
            "model": sel.model,
            "key_id": sel.key_id,
            "pinned_model": sel.pinned_model,
        })
        .to_string();
        let entry = audit_log::AuditEntry {
            ts: chrono::Utc::now(),
            caller_kind: "host".to_string(),
            caller_session: Some(session.id.as_uuid().to_string()),
            caller_agent_group: Some(session.agent_group_id.as_uuid().to_string()),
            command: "provider.degrade".to_string(),
            args,
            result: "ok".to_string(),
            error_code: None,
            error_message: None,
            latency_ms: 0,
        };
        if let Err(err) = audit_log::insert(&self.central, &entry) {
            warn!(agent_group = %session.agent_group_id.as_uuid(), ?err, "provider failover: degrade audit failed");
        }
    }

    /// Fold the group's recently-reported `agent_turns` outcomes into its
    /// persisted health. Drives the *automatic* degrade / restore:
    ///
    /// * A turn with `status = "error"` whose error text classifies as a
    ///   resilience failure ([`DegradeReason::from_error_text`]) degrades its
    ///   `(provider, model, key_id)` triple — but only when the turn is newer
    ///   than the triple's recorded `last_failure_at`, so re-reading the same
    ///   error doesn't keep extending the cool-down (idempotent).
    /// * A successful turn newer than the triple's `last_failure_at` restores
    ///   it to healthy (a real re-probe success after a degrade).
    ///
    /// The runner records turns with the *active* key only implicitly (the
    /// `agent_turns` row carries provider + model, not the key id), so a
    /// failure is attributed to the entry's currently-selected key — the one
    /// the previous spawn wrote into `runner.json`. We resolve that key by
    /// re-selecting against the pre-fold health; that's the key that was live
    /// when the failing turn ran.
    fn fold_recent_turns(
        &self,
        chain: &FallbackChain,
        id: AgentGroupId,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        // Bound the scan: a turn older than two cool-down windows can't change
        // the current decision (its cool-down has long since lapsed).
        let window = chain.reprobe_after() * 2 + chrono::Duration::minutes(5);
        let since = now - window;
        let turns = match copperclaw_db::tables::agent_turns::recent_for_group(
            &self.central,
            &id.as_uuid().to_string(),
            since,
            64,
        ) {
            Ok(t) => t,
            Err(err) => {
                warn!(agent_group = %id.as_uuid(), ?err, "provider failover: agent_turns read failed");
                return;
            }
        };
        if turns.is_empty() {
            return;
        }
        let mut health = hydrate_health(&self.central, id);
        // Oldest-first so successive failures/recoveries apply in order.
        for turn in turns.into_iter().rev() {
            // Find the chain entry this turn ran on (by provider + model). A
            // turn for a provider/model not in the chain (e.g. an old config)
            // is ignored.
            let Some(entry) = chain
                .entries
                .iter()
                .find(|e| e.provider == turn.provider && e.model == turn.model)
            else {
                continue;
            };
            let is_error = turn.status == "error";
            if is_error {
                let Some(reason) = turn
                    .error
                    .as_deref()
                    .and_then(DegradeReason::from_error_text)
                else {
                    continue; // not a resilience failure (bad request, etc.)
                };
                // Attribute the failure to the key that was live when the turn
                // ran: the key the pre-fold selection would have picked for
                // this entry. Falls back to the entry's first key id.
                let key_id = chain
                    .select(None, &health, turn.ended_at)
                    .filter(|s| s.provider == turn.provider && s.model == turn.model)
                    .map_or_else(
                        || entry.key_ids().into_iter().next().unwrap_or_default(),
                        |s| s.key_id,
                    );
                let hk = HealthKey::new(&turn.provider, &turn.model, &key_id);
                let last_failure = health.get(&hk).and_then(|h| h.last_failure_at);
                // Only degrade on a turn newer than the last recorded failure
                // (idempotent: re-reading the same turn is a no-op).
                if last_failure.is_some_and(|prev| turn.ended_at <= prev) {
                    continue;
                }
                chain.record_failure(
                    &mut health,
                    &turn.provider,
                    &turn.model,
                    &key_id,
                    reason,
                    turn.ended_at,
                );
                if let Some(h) = health.get(&hk) {
                    persist_health(
                        &self.central,
                        id,
                        &turn.provider,
                        &turn.model,
                        &key_id,
                        h,
                        now,
                    );
                }
            } else {
                // A successful turn restores every key of this entry that is
                // currently degraded with a failure older than the success —
                // the re-probe-and-restore half. (A success carries no key id,
                // so it heals whichever of the entry's keys had degraded.)
                for key_id in entry.key_ids() {
                    let hk = HealthKey::new(&turn.provider, &turn.model, &key_id);
                    let recovered = health
                        .get(&hk)
                        .and_then(|h| h.last_failure_at)
                        .is_some_and(|prev| turn.ended_at > prev);
                    if !recovered {
                        continue;
                    }
                    chain.record_success(&mut health, &turn.provider, &turn.model, &key_id);
                    if let Some(h) = health.get(&hk) {
                        persist_health(
                            &self.central,
                            id,
                            &turn.provider,
                            &turn.model,
                            &key_id,
                            h,
                            now,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::spawn::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use copperclaw_providers::ProviderError;
    use serde_json::json;
    use std::path::PathBuf;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: SkillsMode::Inline,
            gpu_passthrough: false,
            forward_env: Vec::new(),
            egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
        }
    }

    fn mgr() -> ContainerManager {
        let central = CentralDb::open_in_memory().unwrap();
        // A throwaway data dir: the failover path never opens the inbound DB
        // for these sessions (no pending inbound), so per-channel pin resolves
        // to `None` and the entry's default model is used.
        let tmp = std::env::temp_dir().join(format!("cc-failover-{}", uuid::Uuid::new_v4()));
        ContainerManager::new(
            central,
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp),
        )
    }

    fn group(m: &ContainerManager) -> AgentGroupId {
        create_ag(
            &m.central,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    fn session(m: &ContainerManager, id: AgentGroupId) -> Session {
        create_session(
            &m.central,
            CreateSession {
                agent_group_id: id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
    }

    fn set_chain(
        m: &ContainerManager,
        id: AgentGroupId,
        chain: &serde_json::Value,
        mbc: &serde_json::Value,
    ) {
        provider_profiles::set_chain(&m.central, id, chain, mbc, None, chrono::Utc::now()).unwrap();
    }

    /// Insert a real `agent_turns` row so the spawn-time fold sees the
    /// runner's reported outcome (this is exactly what `record_usage_report`
    /// writes on the live path). `error` is the `ProviderError::Display` text.
    fn record_turn(
        m: &ContainerManager,
        s: &Session,
        provider: &str,
        model: &str,
        status: &str,
        error: Option<&str>,
        ended_at: chrono::DateTime<chrono::Utc>,
    ) {
        use copperclaw_db::tables::agent_turns::{NewAgentTurn, insert};
        insert(
            &m.central,
            &NewAgentTurn {
                session_id: s.id.as_uuid().to_string(),
                agent_group_id: s.agent_group_id.as_uuid().to_string(),
                seq: 0,
                model: model.into(),
                provider: provider.into(),
                input_tokens: 1,
                output_tokens: 1,
                started_at: ended_at,
                ended_at,
                status: status.into(),
                error: error.map(str::to_string),
            },
        )
        .unwrap();
    }

    #[test]
    fn no_chain_yields_no_selection() {
        let m = mgr();
        let g = group(&m);
        let s = session(&m, g);
        assert!(
            m.resolve_provider_selection(&s, chrono::Utc::now())
                .is_none()
        );
    }

    #[test]
    fn healthy_chain_selects_primary() {
        let m = mgr();
        let g = group(&m);
        let s = session(&m, g);
        set_chain(
            &m,
            g,
            &json!([
                {"provider": "anthropic", "model": "claude-sonnet-4-6",
                 "keys": [{"id": "primary", "api_key_env": "ANTHROPIC_API_KEY"}]},
                {"provider": "ollama", "model": "qwen3.6:27b"}
            ]),
            &json!({}),
        );
        let sel = m
            .resolve_provider_selection(&s, chrono::Utc::now())
            .unwrap();
        assert_eq!(sel.provider, "anthropic");
        assert_eq!(sel.key_id, "primary");
        assert!(!sel.degraded);
    }

    #[test]
    fn rate_limit_degrades_to_fallback_and_audits() {
        let m = mgr();
        let g = group(&m);
        let s = session(&m, g);
        set_chain(
            &m,
            g,
            &json!([
                {"provider": "anthropic", "model": "claude-sonnet-4-6",
                 "keys": [{"id": "k1", "api_key_env": "ANTHROPIC_API_KEY"}]},
                {"provider": "ollama", "model": "qwen3.6:27b"}
            ]),
            &json!({}),
        );
        let now = chrono::Utc::now();
        // The runner reports a rate-limited turn against the primary entry.
        record_turn(
            &m,
            &s,
            "anthropic",
            "claude-sonnet-4-6",
            "error",
            Some(
                &ProviderError::Api {
                    status: 429,
                    message: "slow down".into(),
                }
                .to_string(),
            ),
            now - chrono::Duration::seconds(1),
        );
        // Next selection folds that outcome, degrades to ollama, and audits.
        let before = audit_log::count(&m.central).unwrap();
        let sel = m.resolve_provider_selection(&s, now).unwrap();
        assert_eq!(sel.provider, "ollama");
        assert!(sel.degraded);
        let after = audit_log::count(&m.central).unwrap();
        assert_eq!(after, before + 1, "degrade audited");
        let recent =
            audit_log::list_recent(&m.central, now - chrono::Duration::minutes(1), 10).unwrap();
        assert!(recent.iter().any(|e| e.command == "provider.degrade"));
    }

    #[test]
    fn primary_restored_after_recovery() {
        let m = mgr();
        let g = group(&m);
        let s = session(&m, g);
        set_chain(
            &m,
            g,
            &json!([
                {"provider": "anthropic", "model": "m1",
                 "keys": [{"id": "k1"}]},
                {"provider": "ollama", "model": "m2"}
            ]),
            &json!({}),
        );
        let t0 = chrono::Utc::now() - chrono::Duration::seconds(10);
        // The runner reports a 5xx failure on the primary.
        record_turn(
            &m,
            &s,
            "anthropic",
            "m1",
            "error",
            Some(
                &ProviderError::Api {
                    status: 503,
                    message: "down".into(),
                }
                .to_string(),
            ),
            t0,
        );
        assert!(
            m.resolve_provider_selection(&s, t0 + chrono::Duration::seconds(1))
                .unwrap()
                .degraded
        );
        // A later successful turn (re-probe of the primary) restores it.
        record_turn(
            &m,
            &s,
            "anthropic",
            "m1",
            "ok",
            None,
            t0 + chrono::Duration::seconds(5),
        );
        let sel = m
            .resolve_provider_selection(&s, t0 + chrono::Duration::seconds(6))
            .unwrap();
        assert_eq!(sel.provider, "anthropic");
        assert!(!sel.degraded);
    }

    #[test]
    fn unhealthy_key_rotated_out() {
        let m = mgr();
        let g = group(&m);
        let s = session(&m, g);
        set_chain(
            &m,
            g,
            &json!([
                {"provider": "anthropic", "model": "m1",
                 "keys": [
                    {"id": "k1", "api_key_env": "KEY_A"},
                    {"id": "k2", "api_key_env": "KEY_B"}
                 ]}
            ]),
            &json!({}),
        );
        let t0 = chrono::Utc::now() - chrono::Duration::seconds(5);
        // The runner reports a rate-limited turn. The fold attributes it to
        // the live key (k1, the first healthy key the previous spawn picked).
        record_turn(
            &m,
            &s,
            "anthropic",
            "m1",
            "error",
            Some(
                &ProviderError::Api {
                    status: 429,
                    message: "rl".into(),
                }
                .to_string(),
            ),
            t0,
        );
        let sel = m
            .resolve_provider_selection(&s, t0 + chrono::Duration::seconds(1))
            .unwrap();
        // Same entry, rotated off the rate-limited k1 onto healthy k2.
        assert_eq!(sel.provider, "anthropic");
        assert_eq!(sel.key_id, "k2");
        assert_eq!(sel.api_key_env.as_deref(), Some("KEY_B"));
        assert!(!sel.degraded);
    }

    #[test]
    fn non_resilience_error_does_not_degrade() {
        let m = mgr();
        let g = group(&m);
        let s = session(&m, g);
        set_chain(
            &m,
            g,
            &json!([
                {"provider": "anthropic", "model": "m1", "keys": [{"id": "k1"}]},
                {"provider": "ollama", "model": "m2"}
            ]),
            &json!({}),
        );
        let now = chrono::Utc::now();
        // A bad-request error must NOT degrade the chain.
        record_turn(
            &m,
            &s,
            "anthropic",
            "m1",
            "error",
            Some(&ProviderError::BadRequest("nope".into()).to_string()),
            now - chrono::Duration::seconds(1),
        );
        let sel = m.resolve_provider_selection(&s, now).unwrap();
        assert_eq!(sel.provider, "anthropic", "bad request keeps primary");
        assert!(!sel.degraded);
    }

    #[test]
    fn per_channel_pin_honored_on_selection() {
        // A telegram-pinned model overrides the active entry's default model
        // when the triggering inbound is on the telegram channel.
        use copperclaw_db::session::{SessionPaths, open_inbound};
        let tmp = tempfile::tempdir().unwrap();
        let central = CentralDb::open_in_memory().unwrap();
        let m = ContainerManager::new(
            central,
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let g = group(&m);
        let s = session(&m, g);
        set_chain(
            &m,
            g,
            &json!([{"provider": "anthropic", "model": "claude-sonnet-4-6",
                    "keys": [{"id": "k1"}]}]),
            &json!({"telegram": "claude-opus-4-1"}),
        );
        // Seed a telegram-channel pending inbound so triggering_channel
        // resolves to "telegram".
        let paths = SessionPaths::new(tmp.path(), s.agent_group_id, s.id);
        let conn = open_inbound(&paths).unwrap();
        copperclaw_db::tables::messages_in::insert(
            &conn,
            &copperclaw_db::tables::messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("tg-123".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("telegram")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        drop(conn);
        let sel = m
            .resolve_provider_selection(&s, chrono::Utc::now())
            .unwrap();
        assert_eq!(sel.provider, "anthropic");
        assert_eq!(sel.model, "claude-opus-4-1", "telegram pin overrides model");
        assert!(sel.pinned_model);
    }

    #[test]
    fn fold_is_idempotent_across_repeated_spawns() {
        // Re-reading the same failing turn across multiple spawns must not
        // keep extending the cool-down (failure_count stays 1).
        let m = mgr();
        let g = group(&m);
        let s = session(&m, g);
        set_chain(
            &m,
            g,
            &json!([
                {"provider": "anthropic", "model": "m1", "keys": [{"id": "k1"}]},
                {"provider": "ollama", "model": "m2"}
            ]),
            &json!({}),
        );
        let t0 = chrono::Utc::now() - chrono::Duration::seconds(5);
        record_turn(
            &m,
            &s,
            "anthropic",
            "m1",
            "error",
            Some(
                &ProviderError::Api {
                    status: 503,
                    message: "down".into(),
                }
                .to_string(),
            ),
            t0,
        );
        // Three spawns in a row.
        for _ in 0..3 {
            let _ = m.resolve_provider_selection(&s, t0 + chrono::Duration::seconds(1));
        }
        let h = provider_profiles::get_health(&m.central, g, "anthropic", "m1", "k1")
            .unwrap()
            .unwrap();
        assert_eq!(h.failure_count, 1, "same turn folded once, not thrice");
    }
}
