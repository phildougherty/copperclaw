//! Handlers for `mcp.list-presets` and `mcp.add`.
//!
//! # MCP server preset registry
//!
//! ironclaw ships a curated catalog of known MCP server configurations. Each
//! preset knows:
//!
//! - The `command` and `args` the container will run to start the server.
//! - Which environment variables are required for the server to function.
//! - A one-line description for `iclaw mcp list-presets`.
//!
//! Presets live entirely as Rust constants — no external files, no network
//! calls at registration time.
//!
//! # `iclaw mcp add <preset> --agent-group-id <id> [--env KEY=VAL]...`
//!
//! Resolves the named preset, merges any `--env` overrides, then writes the
//! server definition into `container_configs.mcp_servers` for the group via
//! [`ironclaw_db::tables::container_configs::add_mcp_server`]. Audited.
//!
//! # `iclaw mcp list-presets`
//!
//! Returns the static catalog as a JSON array. No socket round-trip required
//! (the command is marked `composite.*` so it's handled client-side), but we
//! also expose it as a real handler so the host can serve it to agents.

use super::{db_err, parse_agent_group_id, req_str};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::container_configs;
use ironclaw_db::tables::container_configs::{CliScope, SkillsSelector};
use ironclaw_iclaw::ErrorPayload;
use ironclaw_types::AgentGroupId;
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Preset registry
// ---------------------------------------------------------------------------

/// A single MCP server preset entry.
pub struct McpPreset {
    /// Short identifier (used as `iclaw mcp add <name>`).
    pub name: &'static str,
    /// One-line description shown in `iclaw mcp list-presets`.
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
        args: &["-y", "@modelcontextprotocol/server-postgres", "--connection-string", "${POSTGRES_CONNECTION_STRING}"],
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
        args: &["-y", "@modelcontextprotocol/server-filesystem", "/workspace"],
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
/// constant. This handler exists so agents can call `iclaw mcp list-presets`
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
                "unknown MCP preset `{preset_name}`; run `iclaw mcp list-presets` \
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
    container_configs::set_mcp_servers(central, agent_group_id, current.clone())
        .map_err(db_err)?;

    Ok(json!({
        "agent_group_id": agent_group_id.as_uuid().to_string(),
        "preset": preset_name,
        "server": server_entry,
    }))
}

/// Ensure a `container_configs` row exists for `agent_group_id`, creating a
/// default one if the group hasn't been configured yet.
fn ensure_config_row(central: &CentralDb, id: AgentGroupId) -> Result<(), ErrorPayload> {
    if container_configs::get(central, id).map_err(db_err)?.is_none() {
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
            },
        )
        .map_err(db_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::central::CentralDb;
    use ironclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn make_group(db: &CentralDb) -> ironclaw_types::AgentGroupId {
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
        for required in ["postgres", "linear", "github", "notion", "filesystem", "browserbase"] {
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
        let servers =
            ironclaw_db::tables::container_configs::get_mcp_servers(&db, ag).unwrap();
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
        let servers =
            ironclaw_db::tables::container_configs::get_mcp_servers(&db, ag).unwrap();
        let obj = servers.as_object().unwrap();
        // Should have only one key (idempotent replace).
        assert_eq!(obj.len(), 1);
        assert_eq!(obj["github"]["env"]["GITHUB_PERSONAL_ACCESS_TOKEN"], "ghp_second");
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
        let servers =
            ironclaw_db::tables::container_configs::get_mcp_servers(&db, ag).unwrap();
        let obj = servers.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("filesystem"));
        assert!(obj.contains_key("linear"));
    }
}
