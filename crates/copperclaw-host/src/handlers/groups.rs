//! Handlers for the `groups.*` and `groups.config.*` commands.

use super::{db_err, opt_str, parse_agent_group_id, req_str};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{agent_groups, container_configs, sessions};
use copperclaw_types::AgentGroupId;
use copperclaw_types::ContainerStatus;
use serde_json::{Value, json};

/// `groups.list`
pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let rows = agent_groups::list(central).map_err(db_err)?;
    Ok(json!(rows.iter().map(group_to_json).collect::<Vec<_>>()))
}

/// `groups.get`
pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let row = agent_groups::get(central, id).map_err(db_err)?;
    Ok(group_to_json(&row))
}

/// `groups.create`
pub fn create(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let folder = req_str(args, "folder")?;
    let name = req_str(args, "name")?;
    let provider = opt_str(args, "provider");
    let row = agent_groups::create(
        central,
        agent_groups::CreateAgentGroup {
            name,
            folder,
            agent_provider: provider,
        },
    )
    .map_err(db_err)?;
    Ok(group_to_json(&row))
}

/// `groups.update`
pub fn update(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    // Distinguish "field absent" from "field present and null" for provider.
    let provider_patch = if args.get("provider").is_some() {
        Some(opt_str(args, "provider"))
    } else {
        None
    };
    let row = agent_groups::update(
        central,
        id,
        agent_groups::UpdateAgentGroup {
            name: opt_str(args, "name"),
            agent_provider: provider_patch,
        },
    )
    .map_err(db_err)?;
    Ok(group_to_json(&row))
}

/// `groups.delete`
pub fn delete(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    agent_groups::delete(central, id).map_err(db_err)?;
    Ok(json!({"deleted": id.as_uuid().to_string()}))
}

/// `groups.restart` — marks every session belonging to the agent group as
/// `container_status = stopped` so the host's poll loop will spawn a fresh
/// container on the next inbound message. Sessions already in `stopped`
/// state are counted as "no change". The actual container kill is owned by
/// the runtime: the next time the runtime checks status against the DB it
/// will see the stop and tear the existing container down.
///
/// Returns `{"agent_group_id", "sessions_marked_stopped": N, "sessions": [ids]}`.
pub fn restart(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    // Existence check — surface a NotFound rather than silently no-op.
    agent_groups::get(central, id).map_err(db_err)?;
    let sessions = sessions::list_for_agent_group(central, id).map_err(db_err)?;
    let mut marked = 0usize;
    let mut ids = Vec::with_capacity(sessions.len());
    for s in &sessions {
        ids.push(s.id.as_uuid().to_string());
        if matches!(s.container_status, ContainerStatus::Stopped) {
            continue;
        }
        sessions::mark_container_stopped(central, s.id).map_err(db_err)?;
        marked += 1;
    }
    Ok(json!({
        "agent_group_id": id.as_uuid().to_string(),
        "sessions_marked_stopped": marked,
        "sessions": ids,
    }))
}

/// `groups.config.get`
pub fn config_get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let row = container_configs::get(central, id).map_err(db_err)?;
    match row {
        Some(cfg) => Ok(container_config_to_json(&cfg)),
        None => Ok(Value::Null),
    }
}

/// `groups.config.update` — narrow free-form field update.
///
/// Only the fields documented below are accepted. Anything else returns
/// `bad_request`.
pub fn config_update(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let field = req_str(args, "field")?;
    let value = args
        .get("value")
        .cloned()
        .ok_or_else(|| ErrorPayload::new("bad_request", "missing `value`"))?;
    let mut existing = container_configs::get(central, id)
        .map_err(db_err)?
        .unwrap_or_else(|| default_config(id));
    match field.as_str() {
        "provider" => existing.provider = value.as_str().map(str::to_owned),
        "model" => existing.model = value.as_str().map(str::to_owned),
        "image_tag" => existing.image_tag = value.as_str().map(str::to_owned),
        "assistant_name" => existing.assistant_name = value.as_str().map(str::to_owned),
        "max_messages_per_prompt" => {
            existing.max_messages_per_prompt = match value {
                Value::Null => None,
                Value::Number(n) => n.as_u64().and_then(|x| u32::try_from(x).ok()),
                _ => {
                    return Err(ErrorPayload::new(
                        "bad_request",
                        "`max_messages_per_prompt` must be a non-negative integer or null",
                    ));
                }
            };
        }
        // Group tool-profile: the FUEL for the runner's layered ToolPolicy.
        // Validate against the known profile names so a typo can't reach
        // the runner (which would silently fall back to `full`); `null`
        // clears the override (→ runner default `full`).
        "tool_profile" => {
            existing.tool_profile = match &value {
                Value::Null => None,
                Value::String(s) => {
                    if copperclaw_modules::permissions::ToolProfile::parse(s).is_none() {
                        return Err(ErrorPayload::new(
                            "bad_request",
                            format!(
                                "unknown tool_profile `{s}` (expected one of: minimal, messaging, coding, full, or null to clear)"
                            ),
                        ));
                    }
                    Some(s.clone())
                }
                _ => {
                    return Err(ErrorPayload::new(
                        "bad_request",
                        "`tool_profile` must be a profile name string or null",
                    ));
                }
            };
        }
        other => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("cannot update unknown field `{other}`"),
            ));
        }
    }
    let row = container_configs::upsert(
        central,
        container_configs::UpsertContainerConfig {
            agent_group_id: existing.agent_group_id,
            provider: existing.provider,
            model: existing.model,
            effort: existing.effort,
            image_tag: existing.image_tag,
            assistant_name: existing.assistant_name,
            max_messages_per_prompt: existing.max_messages_per_prompt,
            skills: existing.skills,
            mcp_servers: existing.mcp_servers,
            packages_apt: existing.packages_apt,
            packages_npm: existing.packages_npm,
            additional_mounts: existing.additional_mounts,
            cli_scope: existing.cli_scope,
            config_fingerprint: existing.config_fingerprint,
            egress_allow: existing.egress_allow,
            resource_limits: existing.resource_limits,
            coding_enabled: existing.coding_enabled,
            surface_thinking: existing.surface_thinking,
            tool_profile: existing.tool_profile,
        },
    )
    .map_err(db_err)?;
    Ok(container_config_to_json(&row))
}

/// `groups.config.add-mcp-server`
pub fn config_add_mcp_server(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let server = args
        .get("server")
        .cloned()
        .ok_or_else(|| ErrorPayload::new("bad_request", "missing `server`"))?;
    let name = server.get("name").and_then(Value::as_str).ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            "`server.name` is required and must be a string",
        )
    })?;
    ensure_config_row(central, id)?;
    let mut current = container_configs::get_mcp_servers(central, id)
        .map_err(db_err)
        .unwrap_or(Value::Object(serde_json::Map::new()));
    if !current.is_object() {
        current = Value::Object(serde_json::Map::new());
    }
    if let Some(obj) = current.as_object_mut() {
        obj.insert(name.to_string(), server.clone());
    }
    container_configs::set_mcp_servers(central, id, current.clone()).map_err(db_err)?;
    Ok(current)
}

/// `groups.config.remove-mcp-server`
pub fn config_remove_mcp_server(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let name = req_str(args, "name")?;
    ensure_config_row(central, id)?;
    let mut current = container_configs::get_mcp_servers(central, id)
        .map_err(db_err)
        .unwrap_or(Value::Object(serde_json::Map::new()));
    if let Some(obj) = current.as_object_mut() {
        obj.remove(&name);
    }
    container_configs::set_mcp_servers(central, id, current.clone()).map_err(db_err)?;
    Ok(current)
}

/// `groups.config.add-package`
pub fn config_add_package(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let kind = req_str(args, "kind")?;
    let name = req_str(args, "name")?;
    ensure_config_row(central, id)?;
    match kind.as_str() {
        "apt" => container_configs::add_package_apt(central, id, name.clone()).map_err(db_err)?,
        "npm" => container_configs::add_package_npm(central, id, name.clone()).map_err(db_err)?,
        other => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("unknown package kind `{other}`"),
            ));
        }
    }
    Ok(json!({"added": {"kind": kind, "name": name}}))
}

/// `groups.config.remove-package`
pub fn config_remove_package(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let kind = req_str(args, "kind")?;
    let name = req_str(args, "name")?;
    ensure_config_row(central, id)?;
    match kind.as_str() {
        "apt" => container_configs::remove_package_apt(central, id, &name).map_err(db_err)?,
        "npm" => container_configs::remove_package_npm(central, id, &name).map_err(db_err)?,
        other => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("unknown package kind `{other}`"),
            ));
        }
    }
    Ok(json!({"removed": {"kind": kind, "name": name}}))
}

/// `groups.config.set-egress-allow`
pub fn config_set_egress_allow(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let allow_val = args
        .get("allow")
        .cloned()
        .ok_or_else(|| ErrorPayload::new("bad_request", "missing `allow` array"))?;
    let allow: Vec<String> = allow_val
        .as_array()
        .ok_or_else(|| ErrorPayload::new("bad_request", "`allow` must be a JSON array"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| ErrorPayload::new("bad_request", "`allow` entries must be strings"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Validate each entry looks like "host:port".
    for entry in &allow {
        validate_egress_entry(entry)?;
    }
    ensure_config_row(central, id)?;
    container_configs::set_egress_allow(central, id, &allow).map_err(db_err)?;
    Ok(serde_json::json!({"egress_allow": allow}))
}

/// `groups.config.set-coding-enabled` — narrow toggle for the coding
/// skills bundle.
pub fn config_set_coding_enabled(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let enabled = args
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ErrorPayload::new("bad_request", "missing boolean `enabled`"))?;
    ensure_config_row(central, id)?;
    container_configs::set_coding_enabled(central, id, enabled).map_err(db_err)?;
    Ok(serde_json::json!({"coding_enabled": enabled}))
}

/// `groups.config.set-resource-limits`
pub fn config_set_resource_limits(
    args: &Value,
    central: &CentralDb,
) -> Result<Value, ErrorPayload> {
    let id = parse_agent_group_id(args, "id")?;
    let limits = args
        .get("limits")
        .cloned()
        .ok_or_else(|| ErrorPayload::new("bad_request", "missing `limits` object"))?;
    if !limits.is_object() {
        return Err(ErrorPayload::new(
            "bad_request",
            "`limits` must be a JSON object",
        ));
    }
    // Validate via ResourceLimits::from_json to catch bad types eagerly.
    copperclaw_container_rt::ResourceLimits::from_json(&limits)
        .map_err(|e| ErrorPayload::new("bad_request", e))?;
    ensure_config_row(central, id)?;
    container_configs::set_resource_limits(central, id, &limits).map_err(db_err)?;
    Ok(serde_json::json!({"resource_limits": limits}))
}

/// Validate that an egress entry is a `"host:port"` pair where port is a
/// valid TCP/UDP port number (1–65535).
fn validate_egress_entry(entry: &str) -> Result<(), ErrorPayload> {
    let (_, port_str) = entry.rsplit_once(':').ok_or_else(|| {
        ErrorPayload::new(
            "bad_request",
            format!("egress entry `{entry}` must be in `host:port` format"),
        )
    })?;
    let port: u32 = port_str.parse().map_err(|_| {
        ErrorPayload::new(
            "bad_request",
            format!("egress entry `{entry}` has an invalid port `{port_str}`"),
        )
    })?;
    if port == 0 || port > 65535 {
        return Err(ErrorPayload::new(
            "bad_request",
            format!("egress entry `{entry}` port must be 1–65535"),
        ));
    }
    Ok(())
}

fn default_config(id: AgentGroupId) -> container_configs::ContainerConfig {
    container_configs::ContainerConfig {
        agent_group_id: id,
        provider: None,
        model: None,
        effort: None,
        image_tag: None,
        assistant_name: None,
        max_messages_per_prompt: None,
        skills: container_configs::SkillsSelector::All,
        mcp_servers: Value::Object(serde_json::Map::new()),
        packages_apt: Vec::new(),
        packages_npm: Vec::new(),
        additional_mounts: Value::Object(serde_json::Map::new()),
        cli_scope: container_configs::CliScope::Disabled,
        config_fingerprint: None,
        egress_allow: Vec::new(),
        resource_limits: Value::Object(serde_json::Map::new()),
        coding_enabled: false,
        surface_thinking: false,
        tool_profile: None,
        updated_at: chrono::Utc::now(),
    }
}

fn ensure_config_row(central: &CentralDb, id: AgentGroupId) -> Result<(), ErrorPayload> {
    if container_configs::get(central, id)
        .map_err(db_err)?
        .is_none()
    {
        let row = default_config(id);
        container_configs::upsert(
            central,
            container_configs::UpsertContainerConfig {
                agent_group_id: row.agent_group_id,
                provider: row.provider,
                model: row.model,
                effort: row.effort,
                image_tag: row.image_tag,
                assistant_name: row.assistant_name,
                max_messages_per_prompt: row.max_messages_per_prompt,
                skills: row.skills,
                mcp_servers: row.mcp_servers,
                packages_apt: row.packages_apt,
                packages_npm: row.packages_npm,
                additional_mounts: row.additional_mounts,
                cli_scope: row.cli_scope,
                config_fingerprint: row.config_fingerprint,
                egress_allow: row.egress_allow,
                resource_limits: row.resource_limits,
                coding_enabled: row.coding_enabled,
                surface_thinking: row.surface_thinking,
                tool_profile: row.tool_profile,
            },
        )
        .map_err(db_err)?;
    }
    Ok(())
}

fn group_to_json(g: &agent_groups::AgentGroup) -> Value {
    json!({
        "id": g.id.as_uuid().to_string(),
        "name": g.name,
        "folder": g.folder,
        "agent_provider": g.agent_provider,
        "created_at": g.created_at.to_rfc3339(),
    })
}

fn container_config_to_json(c: &container_configs::ContainerConfig) -> Value {
    json!({
        "agent_group_id": c.agent_group_id.as_uuid().to_string(),
        "provider": c.provider,
        "model": c.model,
        "image_tag": c.image_tag,
        "assistant_name": c.assistant_name,
        "max_messages_per_prompt": c.max_messages_per_prompt,
        "skills": c.skills,
        "mcp_servers": c.mcp_servers,
        "packages_apt": c.packages_apt,
        "packages_npm": c.packages_npm,
        "additional_mounts": c.additional_mounts,
        "cli_scope": c.cli_scope,
        "config_fingerprint": c.config_fingerprint,
        "egress_allow": c.egress_allow,
        "resource_limits": c.resource_limits,
        "coding_enabled": c.coding_enabled,
        "tool_profile": c.tool_profile,
        "updated_at": c.updated_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn make_group(central: &CentralDb, folder: &str) -> agent_groups::AgentGroup {
        agent_groups::create(
            central,
            agent_groups::CreateAgentGroup {
                name: folder.into(),
                folder: folder.into(),
                agent_provider: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn list_empty() {
        let db = db();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    #[test]
    fn create_and_get_roundtrip() {
        let db = db();
        let v = create(
            &json!({"folder": "g", "name": "Greeter", "provider": "claude"}),
            &db,
        )
        .unwrap();
        let id = v["id"].as_str().unwrap().to_owned();
        let got = get(&json!({"id": id}), &db).unwrap();
        assert_eq!(got["folder"], "g");
        assert_eq!(got["agent_provider"], "claude");
    }

    #[test]
    fn create_missing_required_errors() {
        let db = db();
        let err = create(&json!({"folder": "g"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn update_sets_name() {
        let db = db();
        let g = make_group(&db, "g");
        let v = update(
            &json!({"id": g.id.as_uuid().to_string(), "name": "renamed"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["name"], "renamed");
    }

    #[test]
    fn update_can_clear_provider() {
        let db = db();
        let g = make_group(&db, "g");
        let v = update(
            &json!({"id": g.id.as_uuid().to_string(), "provider": Value::Null}),
            &db,
        )
        .unwrap();
        assert!(v["agent_provider"].is_null());
    }

    #[test]
    fn delete_and_then_get_yields_not_found() {
        let db = db();
        let g = make_group(&db, "g");
        delete(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap();
        let err = get(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn restart_with_no_sessions_marks_zero() {
        let db = db();
        let g = make_group(&db, "g");
        let v = restart(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["sessions_marked_stopped"], 0);
        assert_eq!(v["agent_group_id"], g.id.as_uuid().to_string());
        assert_eq!(v["sessions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn restart_stops_running_sessions_but_leaves_already_stopped() {
        use copperclaw_db::tables::sessions::{self as s, CreateSession};
        let db = db();
        let g = make_group(&db, "g");
        // Two sessions: one running, one already stopped.
        let a = s::create(
            &db,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: None,
                thread_id: Some("t1".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let b = s::create(
            &db,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: None,
                thread_id: Some("t2".into()),
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        s::mark_container_running(&db, a.id).unwrap();
        s::mark_container_stopped(&db, b.id).unwrap();

        let v = restart(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["sessions_marked_stopped"], 1, "only a transitioned");
        assert_eq!(v["sessions"].as_array().unwrap().len(), 2);

        let after_a = s::get(&db, a.id).unwrap();
        assert_eq!(after_a.container_status, ContainerStatus::Stopped);
        let after_b = s::get(&db, b.id).unwrap();
        assert_eq!(after_b.container_status, ContainerStatus::Stopped);
    }

    #[test]
    fn restart_unknown_group_is_not_found() {
        let db = db();
        let fake = copperclaw_types::AgentGroupId::new();
        let err = restart(&json!({"id": fake.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn restart_does_not_touch_other_groups() {
        use copperclaw_db::tables::sessions::{self as s, CreateSession};
        let db = db();
        let g1 = make_group(&db, "g1");
        let g2 = make_group(&db, "g2");
        let s1 = s::create(
            &db,
            CreateSession {
                agent_group_id: g1.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let s2 = s::create(
            &db,
            CreateSession {
                agent_group_id: g2.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        s::mark_container_running(&db, s1.id).unwrap();
        s::mark_container_running(&db, s2.id).unwrap();
        restart(&json!({"id": g1.id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(
            s::get(&db, s1.id).unwrap().container_status,
            ContainerStatus::Stopped
        );
        assert_eq!(
            s::get(&db, s2.id).unwrap().container_status,
            ContainerStatus::Running
        );
    }

    #[test]
    fn config_get_missing_is_null() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_get(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap();
        assert!(v.is_null());
    }

    #[test]
    fn config_update_creates_default_then_sets_field() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "model", "value": "claude-3"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["model"], "claude-3");
    }

    #[test]
    fn config_update_unknown_field_errors() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "ghost", "value": "x"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_update_max_messages_parses_int() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "max_messages_per_prompt", "value": 32}),
            &db,
        )
        .unwrap();
        assert_eq!(v["max_messages_per_prompt"], 32);
    }

    #[test]
    fn config_update_sets_tool_profile() {
        // FUEL exposure: `cclaw groups config update --field
        // 'tool_profile="messaging"'` lands the profile and surfaces it in
        // the config JSON the assembler reads.
        let db = db();
        let g = make_group(&db, "g");
        let v = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "tool_profile", "value": "messaging"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["tool_profile"], "messaging");
        let stored = container_configs::get(&db, g.id).unwrap().unwrap();
        assert_eq!(stored.tool_profile.as_deref(), Some("messaging"));
    }

    #[test]
    fn config_update_tool_profile_null_clears() {
        let db = db();
        let g = make_group(&db, "g");
        config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "tool_profile", "value": "coding"}),
            &db,
        )
        .unwrap();
        let v = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "tool_profile", "value": null}),
            &db,
        )
        .unwrap();
        assert!(v["tool_profile"].is_null());
    }

    #[test]
    fn config_update_tool_profile_rejects_unknown_name() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "tool_profile", "value": "wizard"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
        assert!(err.message.contains("unknown tool_profile"));
    }

    #[test]
    fn config_update_tool_profile_rejects_non_string() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "tool_profile", "value": 7}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_update_max_messages_rejects_bad_type() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "max_messages_per_prompt", "value": "lots"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_update_max_messages_accepts_null() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "max_messages_per_prompt", "value": Value::Null}),
            &db,
        )
        .unwrap();
        assert!(v["max_messages_per_prompt"].is_null());
    }

    #[test]
    fn config_update_missing_value_errors() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "model"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_add_and_remove_mcp_server() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_add_mcp_server(
            &json!({
                "id": g.id.as_uuid().to_string(),
                "server": {"name": "alpha", "transport": "stdio"},
            }),
            &db,
        )
        .unwrap();
        assert!(v.get("alpha").is_some());

        let v = config_remove_mcp_server(
            &json!({"id": g.id.as_uuid().to_string(), "name": "alpha"}),
            &db,
        )
        .unwrap();
        assert!(v.get("alpha").is_none());
    }

    #[test]
    fn config_add_mcp_server_requires_name() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_add_mcp_server(
            &json!({"id": g.id.as_uuid().to_string(), "server": {}}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_add_mcp_server_requires_server_field() {
        let db = db();
        let g = make_group(&db, "g");
        let err =
            config_add_mcp_server(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_add_and_remove_package_apt() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_add_package(
            &json!({"id": g.id.as_uuid().to_string(), "kind": "apt", "name": "curl"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["added"]["name"], "curl");

        let v = config_remove_package(
            &json!({"id": g.id.as_uuid().to_string(), "kind": "apt", "name": "curl"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["removed"]["name"], "curl");
    }

    #[test]
    fn config_add_package_npm() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_add_package(
            &json!({"id": g.id.as_uuid().to_string(), "kind": "npm", "name": "left-pad"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["added"]["kind"], "npm");
    }

    #[test]
    fn config_add_package_rejects_unknown_kind() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_add_package(
            &json!({"id": g.id.as_uuid().to_string(), "kind": "snap", "name": "x"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_remove_package_rejects_unknown_kind() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_remove_package(
            &json!({"id": g.id.as_uuid().to_string(), "kind": "snap", "name": "x"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_egress_allow_valid_entries() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_set_egress_allow(
            &json!({
                "id": g.id.as_uuid().to_string(),
                "allow": ["api.example.com:443", "db.local:5432"]
            }),
            &db,
        )
        .unwrap();
        let allow = v["egress_allow"].as_array().unwrap();
        assert_eq!(allow.len(), 2);
        assert_eq!(allow[0], "api.example.com:443");
    }

    #[test]
    fn config_set_egress_allow_empty_clears_list() {
        let db = db();
        let g = make_group(&db, "g");
        // First set something.
        config_set_egress_allow(
            &json!({"id": g.id.as_uuid().to_string(), "allow": ["x.local:80"]}),
            &db,
        )
        .unwrap();
        // Then clear.
        let v =
            config_set_egress_allow(&json!({"id": g.id.as_uuid().to_string(), "allow": []}), &db)
                .unwrap();
        assert_eq!(v["egress_allow"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn config_set_egress_allow_rejects_missing_port() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_set_egress_allow(
            &json!({"id": g.id.as_uuid().to_string(), "allow": ["no-port"]}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_egress_allow_rejects_port_zero() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_set_egress_allow(
            &json!({"id": g.id.as_uuid().to_string(), "allow": ["host:0"]}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_egress_allow_rejects_port_too_large() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_set_egress_allow(
            &json!({"id": g.id.as_uuid().to_string(), "allow": ["host:99999"]}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_egress_allow_requires_allow_key() {
        let db = db();
        let g = make_group(&db, "g");
        let err =
            config_set_egress_allow(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_resource_limits_full() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_set_resource_limits(
            &json!({
                "id": g.id.as_uuid().to_string(),
                "limits": {"cpus": "1.5", "memory_mb": 512u64, "pids_limit": 256u64}
            }),
            &db,
        )
        .unwrap();
        let lim = &v["resource_limits"];
        assert_eq!(lim["cpus"], "1.5");
        assert_eq!(lim["memory_mb"], 512);
        assert_eq!(lim["pids_limit"], 256);
    }

    #[test]
    fn config_set_resource_limits_empty_clears() {
        let db = db();
        let g = make_group(&db, "g");
        config_set_resource_limits(
            &json!({"id": g.id.as_uuid().to_string(), "limits": {"memory_mb": 256u64}}),
            &db,
        )
        .unwrap();
        let v = config_set_resource_limits(
            &json!({"id": g.id.as_uuid().to_string(), "limits": {}}),
            &db,
        )
        .unwrap();
        assert_eq!(v["resource_limits"], json!({}));
    }

    #[test]
    fn config_set_resource_limits_rejects_bad_cpus_type() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_set_resource_limits(
            &json!({"id": g.id.as_uuid().to_string(), "limits": {"cpus": 1}}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_resource_limits_rejects_non_object_limits() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_set_resource_limits(
            &json!({"id": g.id.as_uuid().to_string(), "limits": "big"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_resource_limits_requires_limits_key() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_set_resource_limits(&json!({"id": g.id.as_uuid().to_string()}), &db)
            .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_coding_enabled_true_then_false_roundtrip() {
        let db = db();
        let g = make_group(&db, "g");
        let v = config_set_coding_enabled(
            &json!({"id": g.id.as_uuid().to_string(), "enabled": true}),
            &db,
        )
        .unwrap();
        assert_eq!(v["coding_enabled"], true);
        let cfg = config_get(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(cfg["coding_enabled"], true);

        let v = config_set_coding_enabled(
            &json!({"id": g.id.as_uuid().to_string(), "enabled": false}),
            &db,
        )
        .unwrap();
        assert_eq!(v["coding_enabled"], false);
        let cfg = config_get(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(cfg["coding_enabled"], false);
    }

    #[test]
    fn config_set_coding_enabled_requires_enabled_key() {
        let db = db();
        let g = make_group(&db, "g");
        let err =
            config_set_coding_enabled(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_set_coding_enabled_rejects_non_bool_enabled() {
        let db = db();
        let g = make_group(&db, "g");
        let err = config_set_coding_enabled(
            &json!({"id": g.id.as_uuid().to_string(), "enabled": "yes"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn config_get_after_update_returns_full_struct() {
        let db = db();
        let g = make_group(&db, "g");
        config_update(
            &json!({"id": g.id.as_uuid().to_string(), "field": "provider", "value": "claude"}),
            &db,
        )
        .unwrap();
        let v = config_get(&json!({"id": g.id.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v["provider"], "claude");
    }

    #[test]
    fn parse_agent_group_id_helper_used() {
        // Exercise the helper through a constructed args object.
        let id = AgentGroupId::new();
        let v = json!({"id": id.as_uuid().to_string()});
        assert_eq!(parse_agent_group_id(&v, "id").unwrap(), id);
    }
}
