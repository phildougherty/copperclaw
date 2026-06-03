//! Handler for `egress.status` — the authoritative egress posture report.
//!
//! Surfaces the host-wide egress mode (from `COPPERCLAW_EGRESS_MODE`) plus,
//! per agent group, the operator-configured allow-list AND the *effective*
//! resolved list (configured entries unioned with the auto-injected REAL
//! model endpoint — provider-aware, from `ANTHROPIC_BASE_URL` or
//! `OLLAMA_BASE_URL`, each with its real default applied when unset).
//! `cclaw doctor` renders this so the operator can see, in one place,
//! exactly what each group's container is allowed to reach under
//! deny-default — and that the model endpoint is always present so
//! deny-default can't blackhole model traffic.
//!
//! The resolution reuses [`crate::container_manager::egress`] (the same code
//! the spawn path runs) so the report can never drift from what is actually
//! stamped onto the spec.

use super::db_err;
use crate::container_manager::egress::{
    DEFAULT_ANTHROPIC_BASE_URL, model_endpoint_entry, parse_egress_mode,
    resolve_allow_list_for_provider,
};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_container_rt::EgressMode;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{agent_groups, container_configs};
use serde_json::{Value, json};

/// `egress.status` — host egress mode + per-group effective allow-lists.
///
/// Reads the posture from the host's process env (the same source the spawn
/// path reads at boot) and delegates the report shaping to [`build_report`]
/// so the env-free core stays unit-testable under the workspace's
/// `forbid(unsafe_code)` (which makes `std::env::set_var` unavailable).
pub fn status(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let mode = parse_egress_mode(std::env::var("COPPERCLAW_EGRESS_MODE").ok().as_deref());
    let anthropic_base_url = std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    let ollama_base_url = std::env::var("OLLAMA_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    let default_provider = std::env::var("COPPERCLAW_DEFAULT_PROVIDER")
        .ok()
        .filter(|s| !s.is_empty());
    build_report(
        central,
        mode,
        anthropic_base_url.as_deref(),
        ollama_base_url.as_deref(),
        default_provider.as_deref(),
    )
}

/// Env-free core of [`status`]: shape the egress report given an already-
/// resolved `mode` and the host's model-endpoint env (the Anthropic + ollama
/// base URLs and the host default provider). The per-group effective list is
/// resolved provider-aware so it matches exactly what the spawn path stamps.
fn build_report(
    central: &CentralDb,
    mode: EgressMode,
    anthropic_base_url: Option<&str>,
    ollama_base_url: Option<&str>,
    default_provider: Option<&str>,
) -> Result<Value, ErrorPayload> {
    let groups = agent_groups::list(central).map_err(db_err)?;
    let mut group_reports = Vec::with_capacity(groups.len());
    for g in &groups {
        let config = container_configs::get(central, g.id).map_err(db_err)?;
        let configured = config
            .as_ref()
            .map(|c| c.egress_allow.clone())
            .unwrap_or_default();
        // Mirror the spawn path's provider precedence: per-group config
        // provider (the report has no session, so `agent_provider` doesn't
        // apply here) → host default provider. The `"claude"` alias and
        // empty-string normalisation match `resolved_provider`.
        let provider = resolve_report_provider(
            config.as_ref().and_then(|c| c.provider.as_deref()),
            default_provider,
        );
        let effective = resolve_allow_list_for_provider(
            &configured,
            provider.as_deref(),
            anthropic_base_url,
            ollama_base_url,
        );
        group_reports.push(json!({
            "agent_group_id": g.id.as_uuid().to_string(),
            "name": g.name,
            "configured_allow": configured,
            "effective_allow": effective,
        }));
    }

    Ok(json!({
        "mode": mode.as_str(),
        // The endpoint the host auto-injects for the *default* provider so
        // deny-default never blackholes model traffic. The Anthropic
        // endpoint defaults to api.anthropic.com when ANTHROPIC_BASE_URL is
        // unset, so this is non-null for the common deployment. Per-group
        // endpoints (e.g. ollama groups) are reflected in each group's
        // `effective_allow`.
        "model_endpoint": model_endpoint_entry(
            anthropic_base_url.unwrap_or(DEFAULT_ANTHROPIC_BASE_URL)
        ),
        "groups": group_reports,
    }))
}

/// Resolve a report-time provider from the per-group `container_config`
/// provider and the host default, applying the same alias normalisation as
/// the spawn path's `resolved_provider`.
fn resolve_report_provider(
    config_provider: Option<&str>,
    default_provider: Option<&str>,
) -> Option<String> {
    let raw = config_provider.or(default_provider).unwrap_or("");
    match raw {
        "" => None,
        "claude" => Some("anthropic".to_string()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_types::AgentGroupId;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    /// Seed a minimal `container_config` row so `set_egress_allow` (an UPDATE)
    /// has something to write to.
    fn seed_config(db: &CentralDb, id: AgentGroupId) {
        container_configs::upsert(
            db,
            container_configs::UpsertContainerConfig {
                agent_group_id: id,
                provider: None,
                model: None,
                effort: None,
                image_tag: None,
                assistant_name: None,
                max_messages_per_prompt: None,
                skills: container_configs::SkillsSelector::All,
                mcp_servers: serde_json::json!({}),
                packages_apt: vec![],
                packages_npm: vec![],
                additional_mounts: serde_json::json!([]),
                cli_scope: container_configs::CliScope::Group,
                config_fingerprint: None,
                egress_allow: vec![],
                resource_limits: serde_json::json!({}),
                coding_enabled: false,
                surface_thinking: false,
                tool_profile: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn build_report_deny_default_injects_model_and_unions_configured() {
        let db = db();
        let g = create_ag(
            &db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        seed_config(&db, g.id);
        container_configs::set_egress_allow(&db, g.id, &["db.local:5432".to_string()]).unwrap();

        let v = build_report(
            &db,
            EgressMode::DenyDefault,
            Some("https://api.anthropic.com"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(v["mode"], "deny-default");
        assert_eq!(v["model_endpoint"], "api.anthropic.com:443");
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        let eff = groups[0]["effective_allow"].as_array().unwrap();
        // Model endpoint auto-injected first, then the configured entry.
        assert_eq!(eff[0], "api.anthropic.com:443");
        assert_eq!(eff[1], "db.local:5432");
    }

    #[test]
    fn build_report_allow_all_defaults_anthropic_when_base_url_unset() {
        let db = db();
        create_ag(
            &db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        // No ANTHROPIC_BASE_URL set → the report now reflects the REAL
        // default Anthropic endpoint (not null), and every group's
        // effective list reaches it. This is the migration-safety fix: the
        // common no-override deployment must not look black-holed.
        let v = build_report(&db, EgressMode::AllowAll, None, None, None).unwrap();
        assert_eq!(v["mode"], "allow-all");
        assert_eq!(v["model_endpoint"], "api.anthropic.com:443");
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        let eff = groups[0]["effective_allow"].as_array().unwrap();
        assert_eq!(eff[0], "api.anthropic.com:443");
    }

    #[test]
    fn build_report_ollama_group_injects_ollama_endpoint() {
        let db = db();
        let g = create_ag(
            &db,
            CreateAgentGroup {
                name: "olla".into(),
                folder: "olla".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        // Seed a per-group config pinned to provider=ollama.
        container_configs::upsert(
            &db,
            container_configs::UpsertContainerConfig {
                agent_group_id: g.id,
                provider: Some("ollama".into()),
                model: None,
                effort: None,
                image_tag: None,
                assistant_name: None,
                max_messages_per_prompt: None,
                skills: container_configs::SkillsSelector::All,
                mcp_servers: serde_json::json!({}),
                packages_apt: vec![],
                packages_npm: vec![],
                additional_mounts: serde_json::json!([]),
                cli_scope: container_configs::CliScope::Group,
                config_fingerprint: None,
                egress_allow: vec![],
                resource_limits: serde_json::json!({}),
                coding_enabled: false,
                surface_thinking: false,
            },
        )
        .unwrap();

        // Anthropic base set (irrelevant to an ollama group) + a real
        // OLLAMA_BASE_URL → the ollama group's effective list reaches the
        // ollama host, NOT api.anthropic.com.
        let v = build_report(
            &db,
            EgressMode::DenyDefault,
            Some("https://api.anthropic.com"),
            Some("http://172.17.0.1:11434"),
            None,
        )
        .unwrap();
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        let eff = groups[0]["effective_allow"].as_array().unwrap();
        assert_eq!(eff[0], "172.17.0.1:11434");
    }

    #[test]
    fn build_report_default_provider_ollama_applies_to_unconfigured_group() {
        let db = db();
        create_ag(
            &db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        // No per-group config at all, host default provider = ollama, no
        // OLLAMA_BASE_URL set → falls back to the ollama localhost default.
        let v = build_report(&db, EgressMode::DenyDefault, None, None, Some("ollama")).unwrap();
        let groups = v["groups"].as_array().unwrap();
        let eff = groups[0]["effective_allow"].as_array().unwrap();
        assert_eq!(eff[0], "localhost:11434");
    }
}
