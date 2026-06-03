//! Handlers for the `groups.provider.*` commands (M16 Phase 4 — provider
//! resilience). Configure a group's fallback chain + per-channel model pins,
//! and inspect the configured chain alongside its live health state.
//!
//! The chain JSON is validated against the typed
//! [`copperclaw_providers::failover::FallbackChain`] shape before it is
//! persisted, so a malformed chain is rejected at the socket boundary rather
//! than silently ignored at spawn time.

use super::{db_err, parse_agent_group_id};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{agent_groups, provider_profiles};
use copperclaw_providers::failover::FallbackChain;
use copperclaw_types::AgentGroupId;
use serde_json::{Value, json};

/// `groups.provider.set-chain` — set the ordered fallback chain (and,
/// optionally, the per-channel pin map + re-probe override) for a group.
///
/// `chain` is a JSON array of `{provider, model, keys?}` entries (position 0
/// is the primary). `model_by_channel` is an optional `{channel: model}`
/// object; `reprobe_after_secs` an optional cool-down override. Validated
/// against [`FallbackChain`] before persisting.
pub fn set_chain(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    ensure_group(central, id)?;
    let chain_val = args
        .get("chain")
        .cloned()
        .ok_or_else(|| ErrorPayload::new("bad_request", "missing `chain` array"))?;
    if !chain_val.is_array() {
        return Err(ErrorPayload::new(
            "bad_request",
            "`chain` must be a JSON array",
        ));
    }
    let mbc = args
        .get("model_by_channel")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !mbc.is_null() && !mbc.is_object() {
        return Err(ErrorPayload::new(
            "bad_request",
            "`model_by_channel` must be a JSON object",
        ));
    }
    let reprobe = match args.get("reprobe_after_secs") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(n.as_u64().ok_or_else(|| {
            ErrorPayload::new(
                "bad_request",
                "`reprobe_after_secs` must be a non-negative integer",
            )
        })?),
        Some(_) => {
            return Err(ErrorPayload::new(
                "bad_request",
                "`reprobe_after_secs` must be an integer or null",
            ));
        }
    };

    // Validate the full shape (chain + pins + reprobe) by deserializing into
    // the typed FallbackChain. This rejects unknown provider entry shapes,
    // non-string pins, etc. before anything is written.
    let probe = json!({
        "entries": chain_val,
        "model_by_channel": if mbc.is_null() { json!({}) } else { mbc.clone() },
        "reprobe_after_secs": reprobe,
    });
    let parsed: FallbackChain = serde_json::from_value(probe)
        .map_err(|e| ErrorPayload::new("bad_request", format!("invalid provider chain: {e}")))?;
    if parsed.entries.is_empty() {
        return Err(ErrorPayload::new(
            "bad_request",
            "`chain` must have at least one entry (use groups.provider.clear to remove)",
        ));
    }
    for (i, entry) in parsed.entries.iter().enumerate() {
        if entry.provider.trim().is_empty() {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("chain entry {i} has an empty `provider`"),
            ));
        }
        if entry.model.trim().is_empty() {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("chain entry {i} has an empty `model`"),
            ));
        }
    }

    let stored =
        provider_profiles::set_chain(central, id, &chain_val, &mbc, reprobe, chrono::Utc::now())
            .map_err(db_err)?;
    Ok(profile_to_json(&stored))
}

/// `groups.provider.set-pins` — replace the per-channel model pin map without
/// touching the chain. `model_by_channel` is a `{channel: model}` object.
pub fn set_pins(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let mbc = args
        .get("model_by_channel")
        .cloned()
        .ok_or_else(|| ErrorPayload::new("bad_request", "missing `model_by_channel` object"))?;
    if !mbc.is_object() {
        return Err(ErrorPayload::new(
            "bad_request",
            "`model_by_channel` must be a JSON object of channel -> model",
        ));
    }
    for (k, v) in mbc.as_object().expect("checked object above") {
        if !v.is_string() {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("pin for channel `{k}` must be a model-id string"),
            ));
        }
    }
    let existing = provider_profiles::get_chain(central, id)
        .map_err(db_err)?
        .ok_or_else(|| {
            ErrorPayload::new(
                "bad_request",
                "no provider chain configured; set one with groups.provider.set-chain first",
            )
        })?;
    let stored = provider_profiles::set_chain(
        central,
        id,
        &existing.chain,
        &mbc,
        existing.reprobe_after_secs,
        chrono::Utc::now(),
    )
    .map_err(db_err)?;
    Ok(profile_to_json(&stored))
}

/// `groups.provider.clear` — remove the provider chain + pins + health for a
/// group (revert to single-provider behaviour). Idempotent.
pub fn clear(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    provider_profiles::clear_chain(central, id).map_err(db_err)?;
    Ok(json!({"cleared": id.as_uuid().to_string()}))
}

/// `groups.provider.get` — the configured chain + pins for a group, or `null`
/// when no chain is configured.
pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    match provider_profiles::get_chain(central, id).map_err(db_err)? {
        Some(p) => Ok(profile_to_json(&p)),
        None => Ok(Value::Null),
    }
}

/// `groups.provider.status` — the chain plus live per-`(provider, model,
/// key)` health (status, cool-down, last reason, failure count). Read-only;
/// surfaced by operator tooling.
pub fn status(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let chain = provider_profiles::get_chain(central, id).map_err(db_err)?;
    let health = provider_profiles::list_health(central, id).map_err(db_err)?;
    Ok(json!({
        "agent_group_id": id.as_uuid().to_string(),
        "chain": chain.as_ref().map_or(Value::Null, |p| p.chain.clone()),
        "model_by_channel": chain.as_ref().map_or(Value::Null, |p| p.model_by_channel.clone()),
        "reprobe_after_secs": chain.as_ref().and_then(|p| p.reprobe_after_secs),
        "health": health.iter().map(health_to_json).collect::<Vec<_>>(),
    }))
}

fn ensure_group(central: &CentralDb, id: AgentGroupId) -> Result<(), ErrorPayload> {
    agent_groups::get(central, id).map_err(db_err)?;
    Ok(())
}

fn profile_to_json(p: &provider_profiles::ProviderProfile) -> Value {
    json!({
        "agent_group_id": p.agent_group_id.as_uuid().to_string(),
        "chain": p.chain,
        "model_by_channel": p.model_by_channel,
        "reprobe_after_secs": p.reprobe_after_secs,
        "updated_at": p.updated_at.to_rfc3339(),
    })
}

fn health_to_json(h: &provider_profiles::HealthRow) -> Value {
    json!({
        "provider": h.provider,
        "model": h.model,
        "key_id": h.key_id,
        "status": h.status,
        "cooldown_until": h.cooldown_until.map(|d| d.to_rfc3339()),
        "last_failure_at": h.last_failure_at.map(|d| d.to_rfc3339()),
        "last_reason": h.last_reason,
        "failure_count": h.failure_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn make_group(central: &CentralDb) -> AgentGroupId {
        agent_groups::create(
            central,
            agent_groups::CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn set_chain_validates_and_persists() {
        let db = db();
        let g = make_group(&db);
        let v = set_chain(
            &json!({
                "id": g.id_str(),
                "chain": [
                    {"provider": "anthropic", "model": "claude-sonnet-4-6",
                     "keys": [{"id": "primary", "api_key_env": "ANTHROPIC_API_KEY"}]},
                    {"provider": "ollama", "model": "qwen3.6:27b"}
                ],
                "model_by_channel": {"telegram": "claude-opus-4-1"},
                "reprobe_after_secs": 120
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["chain"][0]["provider"], "anthropic");
        assert_eq!(v["model_by_channel"]["telegram"], "claude-opus-4-1");
        assert_eq!(v["reprobe_after_secs"], 120);
    }

    #[test]
    fn set_chain_rejects_empty_array() {
        let db = db();
        let g = make_group(&db);
        let err = set_chain(&json!({"id": g.id_str(), "chain": []}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn set_chain_rejects_non_array() {
        let db = db();
        let g = make_group(&db);
        let err = set_chain(&json!({"id": g.id_str(), "chain": "nope"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn set_chain_rejects_entry_missing_model() {
        let db = db();
        let g = make_group(&db);
        let err = set_chain(
            &json!({"id": g.id_str(), "chain": [{"provider": "anthropic", "model": ""}]}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn set_chain_rejects_malformed_entry_shape() {
        let db = db();
        let g = make_group(&db);
        // `provider` must be a string; an integer fails FallbackChain parse.
        let err = set_chain(
            &json!({"id": g.id_str(), "chain": [{"provider": 7, "model": "m"}]}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn get_returns_null_when_unset() {
        let db = db();
        let g = make_group(&db);
        assert!(get(&json!({"id": g.id_str()}), &db).unwrap().is_null());
    }

    #[test]
    fn set_pins_requires_existing_chain() {
        let db = db();
        let g = make_group(&db);
        let err = set_pins(
            &json!({"id": g.id_str(), "model_by_channel": {"cli": "m"}}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn set_pins_updates_without_touching_chain() {
        let db = db();
        let g = make_group(&db);
        set_chain(
            &json!({"id": g.id_str(),
                    "chain": [{"provider": "anthropic", "model": "m1"}]}),
            &db,
        )
        .unwrap();
        let v = set_pins(
            &json!({"id": g.id_str(), "model_by_channel": {"cli": "m-cli"}}),
            &db,
        )
        .unwrap();
        assert_eq!(v["model_by_channel"]["cli"], "m-cli");
        assert_eq!(v["chain"][0]["model"], "m1", "chain untouched");
    }

    #[test]
    fn set_pins_rejects_non_string_model() {
        let db = db();
        let g = make_group(&db);
        set_chain(
            &json!({"id": g.id_str(),
                    "chain": [{"provider": "anthropic", "model": "m1"}]}),
            &db,
        )
        .unwrap();
        let err = set_pins(
            &json!({"id": g.id_str(), "model_by_channel": {"cli": 9}}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn clear_is_idempotent() {
        let db = db();
        let g = make_group(&db);
        set_chain(
            &json!({"id": g.id_str(),
                    "chain": [{"provider": "anthropic", "model": "m1"}]}),
            &db,
        )
        .unwrap();
        clear(&json!({"id": g.id_str()}), &db).unwrap();
        // Second clear is still ok.
        clear(&json!({"id": g.id_str()}), &db).unwrap();
        assert!(get(&json!({"id": g.id_str()}), &db).unwrap().is_null());
    }

    #[test]
    fn status_reports_chain_and_health() {
        let db = db();
        let g = make_group(&db);
        set_chain(
            &json!({"id": g.id_str(),
                    "chain": [{"provider": "anthropic", "model": "m1", "keys": [{"id": "k1"}]}]}),
            &db,
        )
        .unwrap();
        // Seed a health row.
        provider_profiles::upsert_health(
            &db,
            &provider_profiles::UpsertHealth {
                agent_group_id: g,
                provider: "anthropic".into(),
                model: "m1".into(),
                key_id: "k1".into(),
                status: "rate_limited".into(),
                cooldown_until: Some(chrono::Utc::now() + chrono::Duration::minutes(2)),
                last_failure_at: Some(chrono::Utc::now()),
                last_reason: Some("rate_limit".into()),
                failure_count: 2,
            },
            chrono::Utc::now(),
        )
        .unwrap();
        let v = status(&json!({"id": g.id_str()}), &db).unwrap();
        assert_eq!(v["chain"][0]["provider"], "anthropic");
        let health = v["health"].as_array().unwrap();
        assert_eq!(health.len(), 1);
        assert_eq!(health[0]["status"], "rate_limited");
        assert_eq!(health[0]["failure_count"], 2);
    }

    // Small helper so the tests read cleanly.
    trait IdStr {
        fn id_str(&self) -> String;
    }
    impl IdStr for AgentGroupId {
        fn id_str(&self) -> String {
            self.as_uuid().to_string()
        }
    }
}
