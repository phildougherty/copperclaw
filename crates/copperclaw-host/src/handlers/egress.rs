//! Handler for `egress.status` — the authoritative egress posture report.
//!
//! Surfaces the host-wide egress mode (from `COPPERCLAW_EGRESS_MODE`) plus,
//! per agent group, the operator-configured allow-list AND the *effective*
//! resolved list (configured entries unioned with the auto-injected model
//! endpoint derived from `ANTHROPIC_BASE_URL`). `cclaw doctor` renders this
//! so the operator can see, in one place, exactly what each group's container
//! is allowed to reach under deny-default — and that the model endpoint is
//! always present so deny-default can't blackhole model traffic.
//!
//! The resolution reuses [`crate::container_manager::egress`] (the same code
//! the spawn path runs) so the report can never drift from what is actually
//! stamped onto the spec.

use super::db_err;
use crate::container_manager::egress::{
    model_endpoint_entry, parse_egress_mode, resolve_allow_list,
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
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    build_report(central, mode, base_url.as_deref())
}

/// Env-free core of [`status`]: shape the egress report given an already-
/// resolved `mode` and optional model `base_url`.
fn build_report(
    central: &CentralDb,
    mode: EgressMode,
    base_url: Option<&str>,
) -> Result<Value, ErrorPayload> {
    let groups = agent_groups::list(central).map_err(db_err)?;
    let mut group_reports = Vec::with_capacity(groups.len());
    for g in &groups {
        let configured = container_configs::get(central, g.id)
            .map_err(db_err)?
            .map(|c| c.egress_allow)
            .unwrap_or_default();
        let effective = resolve_allow_list(&configured, base_url);
        group_reports.push(json!({
            "agent_group_id": g.id.as_uuid().to_string(),
            "name": g.name,
            "configured_allow": configured,
            "effective_allow": effective,
        }));
    }

    Ok(json!({
        "mode": mode.as_str(),
        // The endpoint the host auto-injects so deny-default never blackholes
        // model traffic. `null` when ANTHROPIC_BASE_URL is unset in the
        // host's env (rare — setup writes it).
        "model_endpoint": base_url.and_then(model_endpoint_entry),
        "groups": group_reports,
    }))
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
    fn build_report_allow_all_with_no_base_url() {
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
        let v = build_report(&db, EgressMode::AllowAll, None).unwrap();
        assert_eq!(v["mode"], "allow-all");
        assert!(v["model_endpoint"].is_null());
        // No base URL + no per-group config → empty effective list.
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0]["effective_allow"].as_array().unwrap().is_empty());
    }
}
