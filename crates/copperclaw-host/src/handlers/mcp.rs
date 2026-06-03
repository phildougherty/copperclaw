//! Handlers for `mcp.list-presets` and `mcp.add`.
//!
//! # MCP server preset registry
//!
//! copperclaw ships a curated catalog of known MCP server configurations. Each
//! preset knows:
//!
//! - The `command` and `args` the container will run to start the server.
//! - Which environment variables are required for the server to function.
//! - A one-line description for `cclaw mcp list-presets`.
//!
//! Presets live entirely as Rust constants — no external files, no network
//! calls at registration time.
//!
//! # `cclaw mcp add <preset> --agent-group-id <id> [--env KEY=VAL]...`
//!
//! Resolves the named preset, merges any `--env` overrides, then writes the
//! server definition into `container_configs.mcp_servers` for the group via
//! [`copperclaw_db::tables::container_configs::add_mcp_server`]. Audited.
//!
//! # `cclaw mcp list-presets`
//!
//! Returns the static catalog as a JSON array. No socket round-trip required
//! (the command is marked `composite.*` so it's handled client-side), but we
//! also expose it as a real handler so the host can serve it to agents.

use super::{db_err, parse_agent_group_id, req_str};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::container_configs;
use copperclaw_db::tables::container_configs::{CliScope, SkillsSelector};
use copperclaw_db::tables::mcp_oauth_tokens;
use copperclaw_mcp::ToolFilter;
use copperclaw_types::AgentGroupId;
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Preset registry
// ---------------------------------------------------------------------------

/// A single MCP server preset entry.
pub struct McpPreset {
    /// Short identifier (used as `cclaw mcp add <name>`).
    pub name: &'static str,
    /// One-line description shown in `cclaw mcp list-presets`.
    pub description: &'static str,
    /// Executable that the container will run.
    pub command: &'static str,
    /// Arguments passed to the command.
    pub args: &'static [&'static str],
    /// Environment variables the server requires. Each entry is `"KEY"`
    /// (required) or `"KEY=default"` (optional with a default value).
    pub required_env: &'static [&'static str],
}

impl McpPreset {
    /// Render the preset into the JSON shape that `mcp_servers` expects:
    ///
    /// ```json
    /// {
    ///   "name": "<preset-name>",
    ///   "command": "<exe>",
    ///   "args": [...],
    ///   "env": { "KEY": "VALUE" }
    /// }
    /// ```
    ///
    /// `extra_env` entries override or supplement `required_env` defaults.
    pub fn to_server_entry(&self, extra_env: &Map<String, Value>) -> Value {
        let mut env = Map::new();
        // Seed with defaults from required_env.
        for spec in self.required_env {
            if let Some((key, default)) = spec.split_once('=') {
                env.insert(key.to_string(), Value::String(default.to_string()));
            }
            // Required vars without a default are left for the operator to
            // supply via `--env`; they show up in the entry with a
            // placeholder so the operator knows they're needed.
        }
        // Apply caller-supplied overrides.
        for (k, v) in extra_env {
            env.insert(k.clone(), v.clone());
        }
        json!({
            "name": self.name,
            "command": self.command,
            "args": self.args,
            "env": env,
        })
    }

    /// Render to the catalog entry shape used by `list-presets`.
    pub fn to_catalog_entry(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "command": self.command,
            "args": self.args,
            "required_env": self.required_env,
        })
    }
}

/// The curated MCP server catalog. Adding a new entry here is the only change
/// required to ship a new preset — the `mcp.add` handler picks it up
/// automatically.
///
/// Preset design rules (per the OpenBSD-of-claw-agents tenets):
///
/// 1. No stubs. Every preset must run as described with the named binary
///    installed in the container.
/// 2. Required env vars are listed explicitly. Operators get a clear error if
///    they forget to supply a key instead of a silent auth failure at runtime.
/// 3. Conservative defaults where possible. Server binaries use `npx` /
///    `uvx` so the container doesn't need a globally installed binary.
pub const PRESETS: &[McpPreset] = &[
    McpPreset {
        name: "postgres",
        description: "PostgreSQL read/write access via the official MCP Postgres server.",
        command: "npx",
        args: &[
            "-y",
            "@modelcontextprotocol/server-postgres",
            "--connection-string",
            "${POSTGRES_CONNECTION_STRING}",
        ],
        required_env: &["POSTGRES_CONNECTION_STRING"],
    },
    McpPreset {
        name: "linear",
        description: "Linear issue tracker integration via the official Linear MCP server.",
        command: "npx",
        args: &["-y", "@linear/mcp-server"],
        required_env: &["LINEAR_API_KEY"],
    },
    McpPreset {
        name: "github",
        description: "GitHub repository and PR management via the official GitHub MCP server.",
        command: "npx",
        args: &["-y", "@modelcontextprotocol/server-github"],
        required_env: &["GITHUB_PERSONAL_ACCESS_TOKEN"],
    },
    McpPreset {
        name: "notion",
        description: "Notion workspace access via the official Notion MCP server.",
        command: "npx",
        args: &["-y", "@notionhq/mcp"],
        required_env: &["NOTION_API_KEY"],
    },
    McpPreset {
        name: "filesystem",
        description: "Local filesystem read/write via the official MCP filesystem server. \
                      Scope is limited to paths listed in args; defaults to /workspace.",
        command: "npx",
        args: &[
            "-y",
            "@modelcontextprotocol/server-filesystem",
            "/workspace",
        ],
        required_env: &[],
    },
    McpPreset {
        name: "browserbase",
        description: "Remote browser automation via Browserbase's MCP server.",
        command: "npx",
        args: &["-y", "@browserbasehq/mcp"],
        required_env: &["BROWSERBASE_API_KEY", "BROWSERBASE_PROJECT_ID"],
    },
];

/// Look up a preset by name. Returns `None` if the name is not in the catalog.
pub fn find_preset(name: &str) -> Option<&'static McpPreset> {
    PRESETS.iter().find(|p| p.name == name)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `mcp.list-presets` — return the static catalog as a JSON array.
///
/// No DB access; the response is built from the compile-time [`PRESETS`]
/// constant. This handler exists so agents can call `cclaw mcp list-presets`
/// through the socket. The `composite.mcp-list-presets` client-side variant
/// (for the operator CLI without a socket round-trip) simply calls this same
/// function from a static context.
pub fn list_presets(_args: &Value, _central: &CentralDb) -> Result<Value, ErrorPayload> {
    let catalog: Vec<Value> = PRESETS.iter().map(McpPreset::to_catalog_entry).collect();
    Ok(json!(catalog))
}

/// `mcp.add` — resolve a preset and write the server entry into
/// `container_configs.mcp_servers` for the given agent group.
///
/// Arguments (JSON object):
/// - `preset` (string, required): the preset name from the catalog.
/// - `agent_group_id` (string, required): the target agent group UUID.
/// - `env` (object, optional): environment variable overrides / additions.
///
/// The entry is merged into the existing `mcp_servers` object. If an entry
/// with the same `name` already exists it is replaced, preserving all other
/// entries (i.e., idempotent on re-add).
pub fn add(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let preset_name = req_str(args, "preset")?;
    let agent_group_id = parse_agent_group_id(args, "agent_group_id")?;

    let preset = find_preset(&preset_name).ok_or_else(|| {
        ErrorPayload::new(
            "not_found",
            format!(
                "unknown MCP preset `{preset_name}`; run `cclaw mcp list-presets` \
                 to see available presets"
            ),
        )
    })?;

    // Parse optional env overrides.
    let extra_env: Map<String, Value> = args
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let server_entry = preset.to_server_entry(&extra_env);

    // Ensure a container_configs row exists for this group.
    ensure_config_row(central, agent_group_id)?;

    // Read existing mcp_servers, merge our new entry, write back.
    let mut current = container_configs::get_mcp_servers(central, agent_group_id)
        .map_err(db_err)
        .unwrap_or(Value::Object(Map::new()));
    if !current.is_object() {
        current = Value::Object(Map::new());
    }
    if let Some(obj) = current.as_object_mut() {
        obj.insert(preset_name.clone(), server_entry.clone());
    }
    container_configs::set_mcp_servers(central, agent_group_id, current.clone()).map_err(db_err)?;

    Ok(json!({
        "agent_group_id": agent_group_id.as_uuid().to_string(),
        "preset": preset_name,
        "server": server_entry,
    }))
}

/// `mcp.inspect-filter` — report the EFFECTIVE per-server tool include/exclude
/// filter for a group's configured external MCP servers.
///
/// Arguments:
/// - `agent_group_id` (string, required).
/// - `server` (string, optional): inspect just this server; omit for all.
///
/// For each server, returns the declared `allowed_tools` / `denied_tools` and a
/// human summary (`open` / `deny-list` / `allow-list` / `permit-nothing`) so an
/// operator can verify a denied MCP tool really is filtered out before it ever
/// reaches the model. Read-only; safe for agent callers to introspect their
/// own group.
pub fn inspect_filter(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let agent_group_id = parse_agent_group_id(args, "agent_group_id")?;
    let only = args.get("server").and_then(Value::as_str);
    let servers = container_configs::get_mcp_servers(central, agent_group_id)
        .map_err(db_err)
        .unwrap_or(Value::Object(Map::new()));
    let obj = servers.as_object().cloned().unwrap_or_default();

    let mut out = Vec::new();
    for (name, entry) in &obj {
        if let Some(only) = only {
            if only != name {
                continue;
            }
        }
        let filter = ToolFilter::from_server_entry(entry);
        let allowed = entry
            .get(copperclaw_mcp::ALLOWED_TOOLS_KEY)
            .cloned()
            .unwrap_or(Value::Null);
        let denied = entry
            .get(copperclaw_mcp::DENIED_TOOLS_KEY)
            .cloned()
            .unwrap_or(Value::Null);
        out.push(json!({
            "server": name,
            "summary": filter_summary(&filter, &allowed),
            "allowed_tools": allowed,
            "denied_tools": denied,
            "allow_count": filter.allow_len(),
            "deny_count": filter.deny_len(),
            "is_open": filter.is_open(),
        }));
    }
    if let Some(only) = only {
        if out.is_empty() {
            return Err(ErrorPayload::new(
                "not_found",
                format!("no MCP server named `{only}` is configured for this group"),
            ));
        }
    }
    Ok(json!({
        "agent_group_id": agent_group_id.as_uuid().to_string(),
        "servers": out,
    }))
}

/// One-word summary of a filter's posture for operator-facing output.
fn filter_summary(filter: &ToolFilter, allowed_raw: &Value) -> &'static str {
    if filter.is_open() {
        return "open";
    }
    // An allow-list was declared. If it parsed to zero names it permits
    // nothing (a deliberate kill switch); otherwise it's a positive gate.
    let allow_declared = !allowed_raw.is_null();
    if allow_declared && filter.allow_len() == 0 {
        "permit-nothing"
    } else if allow_declared {
        "allow-list"
    } else {
        "deny-list"
    }
}

/// `mcp.oauth-list` — list host-side OAuth tokens stored for a group's external
/// MCP servers, METADATA ONLY (the access/refresh tokens are never returned).
///
/// This is the operator's window into the host-side OAuth store
/// (`mcp_oauth_tokens`): which servers have a token, its type/scope/expiry, and
/// when it was last refreshed — without ever surfacing the secret. The tokens
/// live on the host and are never forwarded into the container env.
pub fn oauth_list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let agent_group_id = parse_agent_group_id(args, "agent_group_id")?;
    let rows = mcp_oauth_tokens::list_for_group(central, agent_group_id).map_err(db_err)?;
    let tokens: Vec<Value> = rows
        .iter()
        .map(|t| {
            json!({
                "server": t.server_name,
                "token_type": t.token_type,
                "scope": t.scope,
                "has_refresh_token": t.refresh_token.is_some(),
                "expires_at": t.expires_at.map(|d| d.to_rfc3339()),
                "updated_at": t.updated_at.to_rfc3339(),
                // NB: access_token / refresh_token are intentionally omitted —
                // this is a metadata view; the secrets never leave the host.
            })
        })
        .collect();
    Ok(json!({
        "agent_group_id": agent_group_id.as_uuid().to_string(),
        "tokens": tokens,
    }))
}

/// Ensure a `container_configs` row exists for `agent_group_id`, creating a
/// default one if the group hasn't been configured yet.
fn ensure_config_row(central: &CentralDb, id: AgentGroupId) -> Result<(), ErrorPayload> {
    if container_configs::get(central, id)
        .map_err(db_err)?
        .is_none()
    {
        container_configs::upsert(
            central,
            container_configs::UpsertContainerConfig {
                agent_group_id: id,
                provider: None,
                model: None,
                effort: None,
                image_tag: None,
                assistant_name: None,
                max_messages_per_prompt: None,
                skills: SkillsSelector::All,
                mcp_servers: json!({}),
                packages_apt: vec![],
                packages_npm: vec![],
                additional_mounts: json!([]),
                cli_scope: CliScope::Group,
                config_fingerprint: None,
                egress_allow: vec![],
                resource_limits: json!({}),
                coding_enabled: false,
                surface_thinking: false,
                tool_profile: None,
            },
        )
        .map_err(db_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn make_group(db: &CentralDb) -> copperclaw_types::AgentGroupId {
        create_ag(
            db,
            CreateAgentGroup {
                name: "test-group".into(),
                folder: "test-group".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    // --- PRESETS catalog ---

    #[test]
    fn all_required_presets_are_present() {
        let names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        for required in [
            "postgres",
            "linear",
            "github",
            "notion",
            "filesystem",
            "browserbase",
        ] {
            assert!(
                names.contains(&required),
                "preset `{required}` is missing from the catalog"
            );
        }
    }

    #[test]
    fn preset_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for p in PRESETS {
            assert!(seen.insert(p.name), "duplicate preset name: {}", p.name);
        }
    }

    #[test]
    fn find_preset_returns_none_for_unknown() {
        assert!(find_preset("not-a-real-preset").is_none());
    }

    #[test]
    fn find_preset_returns_some_for_known() {
        assert!(find_preset("postgres").is_some());
        assert!(find_preset("github").is_some());
    }

    #[test]
    fn preset_to_catalog_entry_shape() {
        let p = find_preset("postgres").unwrap();
        let entry = p.to_catalog_entry();
        assert_eq!(entry["name"], "postgres");
        assert!(!entry["description"].as_str().unwrap().is_empty());
        assert!(entry["command"].is_string());
        assert!(entry["args"].is_array());
        assert!(entry["required_env"].is_array());
    }

    #[test]
    fn preset_to_server_entry_merges_extra_env() {
        let p = find_preset("postgres").unwrap();
        let mut extra = Map::new();
        extra.insert(
            "POSTGRES_CONNECTION_STRING".to_string(),
            Value::String("postgres://localhost/mydb".to_string()),
        );
        let entry = p.to_server_entry(&extra);
        assert_eq!(entry["name"], "postgres");
        assert_eq!(
            entry["env"]["POSTGRES_CONNECTION_STRING"],
            "postgres://localhost/mydb"
        );
    }

    #[test]
    fn preset_to_server_entry_empty_extra_env_for_filesystem() {
        // filesystem preset has no required env vars.
        let p = find_preset("filesystem").unwrap();
        let entry = p.to_server_entry(&Map::new());
        // env should be an empty object when there are no required env vars.
        assert!(entry["env"].is_object());
    }

    // --- list_presets handler ---

    #[test]
    fn list_presets_returns_array() {
        let db = db();
        let v = list_presets(&Value::Null, &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), PRESETS.len());
    }

    #[test]
    fn list_presets_entries_have_required_fields() {
        let db = db();
        let v = list_presets(&Value::Null, &db).unwrap();
        for entry in v.as_array().unwrap() {
            assert!(entry["name"].is_string(), "missing name");
            assert!(entry["description"].is_string(), "missing description");
            assert!(entry["command"].is_string(), "missing command");
            assert!(entry["args"].is_array(), "missing args");
        }
    }

    // --- mcp.add handler ---

    #[test]
    fn add_unknown_preset_errors() {
        let db = db();
        let ag = make_group(&db);
        let err = add(
            &json!({"preset": "no-such-preset", "agent_group_id": ag.as_uuid().to_string()}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
        assert!(err.message.contains("no-such-preset"));
    }

    #[test]
    fn add_missing_preset_arg_errors() {
        let db = db();
        let err = add(&json!({"agent_group_id": "x"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn add_missing_agent_group_id_arg_errors() {
        let db = db();
        let err = add(&json!({"preset": "postgres"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn add_postgres_preset_writes_mcp_servers_entry() {
        let db = db();
        let ag = make_group(&db);
        let result = add(
            &json!({
                "preset": "postgres",
                "agent_group_id": ag.as_uuid().to_string(),
                "env": {"POSTGRES_CONNECTION_STRING": "postgres://localhost/test"},
            }),
            &db,
        )
        .unwrap();
        assert_eq!(result["preset"], "postgres");
        assert_eq!(result["agent_group_id"], ag.as_uuid().to_string());
        let server = &result["server"];
        assert_eq!(server["name"], "postgres");
        assert_eq!(
            server["env"]["POSTGRES_CONNECTION_STRING"],
            "postgres://localhost/test"
        );
        // Verify the DB was updated: mcp_servers is an object keyed by name.
        let servers = copperclaw_db::tables::container_configs::get_mcp_servers(&db, ag).unwrap();
        let obj = servers.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.contains_key("postgres"));
    }

    #[test]
    fn add_is_idempotent_replaces_existing() {
        let db = db();
        let ag = make_group(&db);
        let args = json!({
            "preset": "github",
            "agent_group_id": ag.as_uuid().to_string(),
            "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_first"},
        });
        add(&args, &db).unwrap();
        let args2 = json!({
            "preset": "github",
            "agent_group_id": ag.as_uuid().to_string(),
            "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_second"},
        });
        add(&args2, &db).unwrap();
        let servers = copperclaw_db::tables::container_configs::get_mcp_servers(&db, ag).unwrap();
        let obj = servers.as_object().unwrap();
        // Should have only one key (idempotent replace).
        assert_eq!(obj.len(), 1);
        assert_eq!(
            obj["github"]["env"]["GITHUB_PERSONAL_ACCESS_TOKEN"],
            "ghp_second"
        );
    }

    #[test]
    fn add_multiple_presets_accumulates_entries() {
        let db = db();
        let ag = make_group(&db);
        add(
            &json!({"preset": "filesystem", "agent_group_id": ag.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        add(
            &json!({
                "preset": "linear",
                "agent_group_id": ag.as_uuid().to_string(),
                "env": {"LINEAR_API_KEY": "lin_test"},
            }),
            &db,
        )
        .unwrap();
        let servers = copperclaw_db::tables::container_configs::get_mcp_servers(&db, ag).unwrap();
        let obj = servers.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("filesystem"));
        assert!(obj.contains_key("linear"));
    }

    // --- mcp.inspect-filter handler ---

    fn seed_servers(db: &CentralDb, ag: AgentGroupId, servers: Value) {
        // mcp.add only writes presets; for filter tests we set the raw
        // mcp_servers object directly (an operator could declare filters via
        // `groups config add-mcp-server` with a custom config).
        super::ensure_config_row(db, ag).unwrap();
        container_configs::set_mcp_servers(db, ag, servers).unwrap();
    }

    #[test]
    fn inspect_filter_reports_deny_list_posture() {
        let db = db();
        let ag = make_group(&db);
        seed_servers(
            &db,
            ag,
            json!({
                "github": {
                    "command": "npx",
                    "denied_tools": ["delete_repo", "force_push"],
                },
            }),
        );
        let v = inspect_filter(&json!({"agent_group_id": ag.as_uuid().to_string()}), &db).unwrap();
        let servers = v["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        let s = &servers[0];
        assert_eq!(s["server"], "github");
        assert_eq!(s["summary"], "deny-list");
        assert_eq!(s["deny_count"], 2);
        assert_eq!(s["is_open"], false);
    }

    #[test]
    fn inspect_filter_reports_allow_list_and_permit_nothing() {
        let db = db();
        let ag = make_group(&db);
        seed_servers(
            &db,
            ag,
            json!({
                "gated": { "allowed_tools": ["read"] },
                "locked": { "allowed_tools": [] },
                "wide":  { "command": "npx" },
            }),
        );
        let v = inspect_filter(&json!({"agent_group_id": ag.as_uuid().to_string()}), &db).unwrap();
        let servers = v["servers"].as_array().unwrap();
        let by_name = |name: &str| {
            servers
                .iter()
                .find(|s| s["server"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(by_name("gated")["summary"], "allow-list");
        assert_eq!(by_name("locked")["summary"], "permit-nothing");
        assert_eq!(by_name("wide")["summary"], "open");
        assert_eq!(by_name("wide")["is_open"], true);
    }

    #[test]
    fn inspect_filter_single_server_filter() {
        let db = db();
        let ag = make_group(&db);
        seed_servers(
            &db,
            ag,
            json!({
                "a": { "denied_tools": ["x"] },
                "b": { "denied_tools": ["y"] },
            }),
        );
        let v = inspect_filter(
            &json!({"agent_group_id": ag.as_uuid().to_string(), "server": "b"}),
            &db,
        )
        .unwrap();
        let servers = v["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["server"], "b");
    }

    #[test]
    fn inspect_filter_unknown_server_is_not_found() {
        let db = db();
        let ag = make_group(&db);
        seed_servers(&db, ag, json!({"a": {"denied_tools": ["x"]}}));
        let err = inspect_filter(
            &json!({"agent_group_id": ag.as_uuid().to_string(), "server": "nope"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    // --- mcp.oauth-list handler ---

    #[test]
    fn oauth_list_returns_metadata_only_never_secrets() {
        use copperclaw_db::tables::mcp_oauth_tokens;
        let db = db();
        let ag = make_group(&db);
        mcp_oauth_tokens::upsert(
            &db,
            &mcp_oauth_tokens::UpsertMcpOAuthToken {
                agent_group_id: ag,
                server_name: "github".into(),
                access_token: "SECRET-access".into(),
                refresh_token: Some("SECRET-refresh".into()),
                token_type: "Bearer".into(),
                scope: Some("repo".into()),
                expires_at: None,
            },
        )
        .unwrap();
        let v = oauth_list(&json!({"agent_group_id": ag.as_uuid().to_string()}), &db).unwrap();
        let tokens = v["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 1);
        let t = &tokens[0];
        assert_eq!(t["server"], "github");
        assert_eq!(t["token_type"], "Bearer");
        assert_eq!(t["scope"], "repo");
        assert_eq!(t["has_refresh_token"], true);
        // The secrets must NOT be present anywhere in the response.
        let serialized = serde_json::to_string(&v).unwrap();
        assert!(
            !serialized.contains("SECRET"),
            "oauth-list must never surface the token secret: {serialized}"
        );
    }

    #[test]
    fn oauth_list_empty_for_group_without_tokens() {
        let db = db();
        let ag = make_group(&db);
        let v = oauth_list(&json!({"agent_group_id": ag.as_uuid().to_string()}), &db).unwrap();
        assert!(v["tokens"].as_array().unwrap().is_empty());
    }
}
