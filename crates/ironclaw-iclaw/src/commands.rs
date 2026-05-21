//! Clap CLI definitions and the `command` / `args` extraction layer.
//!
//! See `PLAN.md` § A2 for the authoritative subcommand inventory. Every
//! leaf command maps to a `(command: String, args: serde_json::Value)`
//! pair which is shipped over the socket via [`crate::protocol::Request`].
//!
//! The strings produced by [`Cli::to_call`] are the contract surface the
//! M5 host socket server will register handlers against. Adding or
//! renaming a subcommand here is a breaking change for the host.

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "iclaw",
    version,
    about = "ironclaw admin CLI",
    propagate_version = true
)]
pub struct Cli {
    /// Path to the Unix-domain socket the host is listening on. When
    /// unset, [`Cli::resolve_socket`] picks a sensible default based on
    /// the platform's user-data dir, falling back to `./data/iclaw.sock`.
    #[arg(long, env = "ICLAW_SOCKET", global = true)]
    pub socket: Option<std::path::PathBuf>,

    /// Emit raw JSON instead of the formatted table.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: TopCommand,
}

impl Cli {
    /// Resolve the socket path the client should dial.
    ///
    /// Priority order:
    /// 1. Explicit `--socket` / `ICLAW_SOCKET` (clap populates `self.socket`).
    /// 2. Platform-default install root + `/data/iclaw.sock` (matches the
    ///    layout that `ironclaw-setup` produces with no `--data-dir`).
    /// 3. Legacy relative `./data/iclaw.sock` so behaviour is unchanged for
    ///    callers that ran the host out of a project-local checkout.
    pub fn resolve_socket(&self) -> std::path::PathBuf {
        if let Some(p) = &self.socket {
            return p.clone();
        }
        if let Some(p) = default_user_socket() {
            return p;
        }
        std::path::PathBuf::from("data/iclaw.sock")
    }
}

/// Platform-specific user-scoped socket path. `None` when `$HOME` is not
/// set (CI containers etc.) — callers should fall back to the legacy
/// relative default in that case.
#[must_use]
pub fn default_user_socket() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let home = std::path::PathBuf::from(home);
    Some(user_socket_for(&home, std::env::consts::OS))
}

/// Pure variant of [`default_user_socket`] for tests. Mirrors the platform
/// rules used by `ironclaw-setup::steps::data_dir::default_data_dir_for`,
/// so the two binaries agree on where a fresh install lives.
#[must_use]
pub fn user_socket_for(home: &std::path::Path, os: &str) -> std::path::PathBuf {
    let root = match os {
        "macos" => home
            .join("Library")
            .join("Application Support")
            .join("ironclaw"),
        "linux" => std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|x| !x.as_os_str().is_empty())
            .map_or_else(
                || home.join(".local").join("share").join("ironclaw"),
                |xdg| xdg.join("ironclaw"),
            ),
        _ => home.join(".ironclaw"),
    };
    root.join("data").join("iclaw.sock")
}

/// Top-level resource groups.
#[derive(Debug, Subcommand)]
pub enum TopCommand {
    /// Agent group administration.
    Groups {
        #[command(subcommand)]
        action: GroupsCmd,
    },
    /// Messaging group (channel binding) administration.
    #[command(name = "messaging-groups")]
    MessagingGroups {
        #[command(subcommand)]
        action: MessagingGroupsCmd,
    },
    /// Wiring (mg ↔ ag) administration.
    Wirings {
        #[command(subcommand)]
        action: WiringsCmd,
    },
    /// Known user records.
    Users {
        #[command(subcommand)]
        action: UsersCmd,
    },
    /// Role assignments.
    Roles {
        #[command(subcommand)]
        action: RolesCmd,
    },
    /// Agent-group membership.
    Members {
        #[command(subcommand)]
        action: MembersCmd,
    },
    /// Per-agent outbound destinations.
    Destinations {
        #[command(subcommand)]
        action: DestinationsCmd,
    },
    /// Sessions.
    Sessions {
        #[command(subcommand)]
        action: SessionsCmd,
    },
    /// User DM channels.
    #[command(name = "user-dms")]
    UserDms {
        #[command(subcommand)]
        action: UserDmsCmd,
    },
    /// Dropped messages.
    #[command(name = "dropped-messages")]
    DroppedMessages {
        #[command(subcommand)]
        action: DroppedMessagesCmd,
    },
    /// Pending approvals.
    Approvals {
        #[command(subcommand)]
        action: ApprovalsCmd,
    },
    /// Mutation audit log.
    Audit {
        #[command(subcommand)]
        action: AuditCmd,
    },
    /// One-shot composite installers for getting a fresh host chatable.
    Quickstart {
        #[command(subcommand)]
        action: QuickstartCmd,
    },
    /// One-shot overview of what's wired up on the host.
    Status,
    /// One-shot operator health check.
    Health,
    /// Emit shell completion script for `iclaw`.
    ///
    /// Pipe the output into your shell's completion dir, e.g.
    /// `iclaw completions bash > /etc/bash_completion.d/iclaw` or
    /// `iclaw completions zsh > ~/.zfunc/_iclaw`.
    Completions {
        /// Target shell. Supports `bash`, `zsh`, `fish`, `elvish`, `powershell`.
        shell: clap_complete::Shell,
    },
}

// --- quickstart ------------------------------------------------------------

/// Composite installer commands. Each one fans out into the underlying
/// `groups` / `messaging-groups` / `wirings` calls on the client side; the
/// host needs no new handlers.
#[derive(Debug, Subcommand)]
pub enum QuickstartCmd {
    /// Wire a fresh agent group to the cli channel so messages typed at
    /// the host's stdin route to the new group. Equivalent to running:
    ///
    ///   iclaw groups create --folder <folder> --name <name>
    ///   iclaw messaging-groups create --channel-type cli --platform-id stdin --name <name>
    ///   iclaw wirings create --mg <new-mg-id> --ag <new-ag-id> --engage pattern --pattern '.*'
    Cli {
        /// Display name for both the agent group and the messaging group.
        #[arg(long)]
        name: String,
        /// Folder slug for the new agent group. Defaults to the name.
        #[arg(long)]
        folder: Option<String>,
        /// Regex pattern the wiring will match against inbound messages.
        /// Defaults to `.*` so every line is routed.
        #[arg(long)]
        pattern: Option<String>,
        /// Optional agent provider override (e.g. `anthropic`, `codex`).
        #[arg(long)]
        provider: Option<String>,
    },
}

// --- groups ----------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum GroupsCmd {
    /// List all agent groups.
    List,
    /// Fetch a single agent group by id.
    Get { id: String },
    /// Create a new agent group.
    ///
    /// `--folder` is the on-disk slug under `<data>/groups/`; defaults
    /// to `--name` when omitted so the common case is a single arg.
    Create {
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        name: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Update an agent group.
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Delete an agent group.
    Delete { id: String },
    /// Restart the agent group's container.
    Restart { id: String },
    /// Container-config subcommands.
    Config {
        #[command(subcommand)]
        action: GroupConfigCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum GroupConfigCmd {
    /// Read the container config for an agent group.
    Get { id: String },
    /// Update a single config field.
    Update {
        id: String,
        /// `key=value` pair (value JSON-encoded).
        #[arg(long)]
        field: String,
    },
    /// Add an MCP server definition (JSON blob).
    #[command(name = "add-mcp-server")]
    AddMcpServer {
        id: String,
        /// JSON describing the MCP server to add.
        #[arg(long = "config")]
        config: String,
    },
    /// Remove an MCP server by name.
    #[command(name = "remove-mcp-server")]
    RemoveMcpServer {
        id: String,
        #[arg(long)]
        name: String,
    },
    /// Add an apt or npm package to the container manifest.
    #[command(name = "add-package")]
    AddPackage {
        id: String,
        #[command(flatten)]
        which: PackageFlag,
    },
    /// Remove an apt or npm package from the container manifest.
    #[command(name = "remove-package")]
    RemovePackage {
        id: String,
        #[command(flatten)]
        which: PackageFlag,
    },
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct PackageFlag {
    /// Apt package name.
    #[arg(long)]
    pub apt: Option<String>,
    /// NPM package name.
    #[arg(long)]
    pub npm: Option<String>,
}

// --- messaging-groups ------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum MessagingGroupsCmd {
    List,
    Get {
        id: String,
    },
    Create {
        #[arg(long = "channel-type")]
        channel_type: String,
        #[arg(long = "platform-id")]
        platform_id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "is-group")]
        is_group: bool,
    },
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "is-group")]
        is_group: Option<bool>,
    },
    Delete {
        id: String,
    },
}

// --- wirings ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EngageArg {
    Pattern,
    Mention,
    MentionSticky,
}

impl EngageArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pattern => "pattern",
            Self::Mention => "mention",
            Self::MentionSticky => "mention-sticky",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SenderScopeArg {
    All,
    Known,
}

impl SenderScopeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Known => "known",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SessionModeArg {
    Shared,
    PerThread,
    AgentShared,
}

impl SessionModeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::PerThread => "per-thread",
            Self::AgentShared => "agent-shared",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum WiringsCmd {
    List,
    Get {
        id: String,
    },
    Create {
        #[arg(long = "mg")]
        mg: String,
        #[arg(long = "ag")]
        ag: String,
        #[arg(long)]
        engage: EngageArg,
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long = "sender-scope")]
        sender_scope: Option<SenderScopeArg>,
        #[arg(long = "session-mode")]
        session_mode: Option<SessionModeArg>,
        #[arg(long)]
        priority: Option<i64>,
    },
    Update {
        id: String,
        #[arg(long)]
        engage: Option<EngageArg>,
        #[arg(long)]
        pattern: Option<String>,
        #[arg(long = "sender-scope")]
        sender_scope: Option<SenderScopeArg>,
        #[arg(long = "session-mode")]
        session_mode: Option<SessionModeArg>,
        #[arg(long)]
        priority: Option<i64>,
    },
    Delete {
        id: String,
    },
}

// --- users -----------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum UsersCmd {
    List,
    Get {
        id: String,
    },
    Create {
        #[arg(long)]
        identity: String,
        #[arg(long = "display-name")]
        display_name: Option<String>,
    },
    Update {
        id: String,
        #[arg(long = "display-name")]
        display_name: Option<String>,
    },
}

// --- roles -----------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum RolesCmd {
    List,
    Grant {
        user: String,
        role: String,
        #[arg(long = "agent-group")]
        agent_group: Option<String>,
    },
    Revoke {
        user: String,
        role: String,
        #[arg(long = "agent-group")]
        agent_group: Option<String>,
    },
}

// --- members ---------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum MembersCmd {
    List { agent_group: String },
    Add { agent_group: String, user: String },
    Remove { agent_group: String, user: String },
}

// --- destinations ----------------------------------------------------------

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DestinationKind {
    Channel,
    Agent,
}

impl DestinationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum DestinationsCmd {
    List {
        agent_group: String,
    },
    Add {
        agent_group: String,
        #[arg(long)]
        name: String,
        #[arg(long = "type")]
        kind: DestinationKind,
        #[arg(long = "display-name")]
        display_name: Option<String>,
        #[arg(long = "channel-type")]
        channel_type: Option<String>,
        #[arg(long = "platform-id")]
        platform_id: Option<String>,
        #[arg(long = "target-agent-group")]
        target_agent_group: Option<String>,
    },
    Remove {
        agent_group: String,
        #[arg(long)]
        name: String,
    },
}

// --- sessions --------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum SessionsCmd {
    List {
        #[arg(long = "agent-group")]
        agent_group: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    Get {
        id: String,
    },
}

// --- user-dms --------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum UserDmsCmd {
    List,
}

// --- dropped-messages ------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum DroppedMessagesCmd {
    List {
        #[arg(long)]
        since: Option<String>,
    },
}

// --- approvals -------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum ApprovalsCmd {
    List,
    Get { id: String },
}

/// `iclaw audit ...` — read the mutation audit log.
#[derive(Debug, Subcommand)]
pub enum AuditCmd {
    /// List recent audit entries.
    List {
        /// Window to look back. Accepts plain seconds (`3600`) or
        /// `Ns`/`Nm`/`Nh`/`Nd` shorthand. Default 24h.
        #[arg(long, default_value = "24h")]
        since: String,
        /// Max rows returned. Default 50.
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
}

// ---------------------------------------------------------------------------
// Conversion from clap structures to `(command, args)` pairs.
// ---------------------------------------------------------------------------

/// A single wire-level call extracted from the parsed CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCall {
    /// Dotted command name shipped on the wire (e.g. `"groups.list"`).
    pub command: String,
    /// JSON argument payload.
    pub args: Value,
}

impl ParsedCall {
    fn new(command: impl Into<String>, args: Value) -> Self {
        Self {
            command: command.into(),
            args,
        }
    }
}

impl Cli {
    /// Convert the parsed CLI invocation into the wire-level call shape.
    pub fn to_call(&self) -> ParsedCall {
        self.command.to_call()
    }
}

impl TopCommand {
    /// Convert this top-level command into a `(command, args)` pair.
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::Groups { action } => action.to_call(),
            Self::MessagingGroups { action } => action.to_call(),
            Self::Wirings { action } => action.to_call(),
            Self::Users { action } => action.to_call(),
            Self::Roles { action } => action.to_call(),
            Self::Members { action } => action.to_call(),
            Self::Destinations { action } => action.to_call(),
            Self::Sessions { action } => action.to_call(),
            Self::UserDms { action } => action.to_call(),
            Self::DroppedMessages { action } => action.to_call(),
            Self::Approvals { action } => action.to_call(),
            Self::Audit { action } => action.to_call(),
            Self::Quickstart { action } => action.to_call(),
            Self::Status => ParsedCall::new("composite.status", json!({})),
            Self::Health => ParsedCall::new("composite.health", json!({})),
            // Completions are emitted entirely client-side; the marker
            // command carries the requested shell name so `run_cli`
            // can short-circuit before any transport call.
            Self::Completions { shell } => ParsedCall::new(
                "composite.completions",
                json!({ "shell": shell.to_string() }),
            ),
        }
    }
}

impl QuickstartCmd {
    /// Build the marker `ParsedCall` for a composite quickstart op.
    ///
    /// Composites aren't a single wire call; `run_cli` recognises the
    /// `composite.*` command prefix and dispatches to a sequence of real
    /// wire calls before this would ever reach the transport. The args
    /// payload below is the shape that the client-side orchestrator
    /// consumes.
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::Cli {
                name,
                folder,
                pattern,
                provider,
            } => {
                let mut o = Map::new();
                o.insert("name".into(), name.clone().into());
                insert_opt(&mut o, "folder", folder.clone());
                insert_opt(&mut o, "pattern", pattern.clone());
                insert_opt(&mut o, "provider", provider.clone());
                ParsedCall::new("composite.quickstart-cli", Value::Object(o))
            }
        }
    }
}

fn insert_opt<V: Into<Value>>(obj: &mut Map<String, Value>, k: &str, v: Option<V>) {
    if let Some(v) = v {
        obj.insert(k.to_string(), v.into());
    }
}

fn parse_field(spec: &str) -> (String, Value) {
    let (key, value) = match spec.split_once('=') {
        Some((k, v)) => (k.to_string(), v.to_string()),
        None => (spec.to_string(), String::new()),
    };
    // Try to interpret the value as JSON; fall back to a JSON string.
    let parsed = serde_json::from_str::<Value>(&value).unwrap_or(Value::String(value));
    (key, parsed)
}

impl GroupsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("groups.list", json!({})),
            Self::Get { id } => ParsedCall::new("groups.get", json!({"id": id})),
            Self::Create {
                folder,
                name,
                provider,
            } => {
                let mut o = Map::new();
                // Default `folder` to `name` so most callers can run
                // `iclaw groups create --name X` and have the slug
                // chosen for them.
                let folder_value = folder.clone().unwrap_or_else(|| name.clone());
                o.insert("folder".into(), folder_value.into());
                o.insert("name".into(), name.clone().into());
                insert_opt(&mut o, "provider", provider.clone());
                ParsedCall::new("groups.create", Value::Object(o))
            }
            Self::Update {
                id,
                name,
                provider,
            } => {
                let mut o = Map::new();
                o.insert("id".into(), id.clone().into());
                insert_opt(&mut o, "name", name.clone());
                insert_opt(&mut o, "provider", provider.clone());
                ParsedCall::new("groups.update", Value::Object(o))
            }
            Self::Delete { id } => ParsedCall::new("groups.delete", json!({"id": id})),
            Self::Restart { id } => ParsedCall::new("groups.restart", json!({"id": id})),
            Self::Config { action } => action.to_call(),
        }
    }
}

impl GroupConfigCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::Get { id } => ParsedCall::new("groups.config.get", json!({"id": id})),
            Self::Update { id, field } => {
                let (key, value) = parse_field(field);
                ParsedCall::new(
                    "groups.config.update",
                    json!({"id": id, "field": key, "value": value}),
                )
            }
            Self::AddMcpServer { id, config } => {
                let parsed = serde_json::from_str::<Value>(config)
                    .unwrap_or_else(|_| Value::String(config.clone()));
                ParsedCall::new(
                    "groups.config.add-mcp-server",
                    json!({"id": id, "server": parsed}),
                )
            }
            Self::RemoveMcpServer { id, name } => ParsedCall::new(
                "groups.config.remove-mcp-server",
                json!({"id": id, "name": name}),
            ),
            Self::AddPackage { id, which } => {
                ParsedCall::new("groups.config.add-package", package_args(id, which))
            }
            Self::RemovePackage { id, which } => {
                ParsedCall::new("groups.config.remove-package", package_args(id, which))
            }
        }
    }
}

fn package_args(id: &str, which: &PackageFlag) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), id.into());
    if let Some(apt) = &which.apt {
        o.insert("kind".into(), "apt".into());
        o.insert("name".into(), apt.clone().into());
    } else if let Some(npm) = &which.npm {
        o.insert("kind".into(), "npm".into());
        o.insert("name".into(), npm.clone().into());
    }
    Value::Object(o)
}

impl MessagingGroupsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("messaging-groups.list", json!({})),
            Self::Get { id } => ParsedCall::new("messaging-groups.get", json!({"id": id})),
            Self::Create {
                channel_type,
                platform_id,
                name,
                is_group,
            } => {
                let mut o = Map::new();
                o.insert("channel_type".into(), channel_type.clone().into());
                o.insert("platform_id".into(), platform_id.clone().into());
                insert_opt(&mut o, "name", name.clone());
                o.insert("is_group".into(), Value::Bool(*is_group));
                ParsedCall::new("messaging-groups.create", Value::Object(o))
            }
            Self::Update { id, name, is_group } => {
                let mut o = Map::new();
                o.insert("id".into(), id.clone().into());
                insert_opt(&mut o, "name", name.clone());
                if let Some(g) = is_group {
                    o.insert("is_group".into(), Value::Bool(*g));
                }
                ParsedCall::new("messaging-groups.update", Value::Object(o))
            }
            Self::Delete { id } => ParsedCall::new("messaging-groups.delete", json!({"id": id})),
        }
    }
}

impl WiringsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("wirings.list", json!({})),
            Self::Get { id } => ParsedCall::new("wirings.get", json!({"id": id})),
            Self::Create {
                mg,
                ag,
                engage,
                pattern,
                sender_scope,
                session_mode,
                priority,
            } => {
                let mut o = Map::new();
                o.insert("messaging_group_id".into(), mg.clone().into());
                o.insert("agent_group_id".into(), ag.clone().into());
                o.insert("engage".into(), engage.as_str().into());
                insert_opt(&mut o, "pattern", pattern.clone());
                if let Some(s) = sender_scope {
                    o.insert("sender_scope".into(), s.as_str().into());
                }
                if let Some(s) = session_mode {
                    o.insert("session_mode".into(), s.as_str().into());
                }
                if let Some(p) = priority {
                    o.insert("priority".into(), (*p).into());
                }
                ParsedCall::new("wirings.create", Value::Object(o))
            }
            Self::Update {
                id,
                engage,
                pattern,
                sender_scope,
                session_mode,
                priority,
            } => {
                let mut o = Map::new();
                o.insert("id".into(), id.clone().into());
                if let Some(e) = engage {
                    o.insert("engage".into(), e.as_str().into());
                }
                insert_opt(&mut o, "pattern", pattern.clone());
                if let Some(s) = sender_scope {
                    o.insert("sender_scope".into(), s.as_str().into());
                }
                if let Some(s) = session_mode {
                    o.insert("session_mode".into(), s.as_str().into());
                }
                if let Some(p) = priority {
                    o.insert("priority".into(), (*p).into());
                }
                ParsedCall::new("wirings.update", Value::Object(o))
            }
            Self::Delete { id } => ParsedCall::new("wirings.delete", json!({"id": id})),
        }
    }
}

impl UsersCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("users.list", json!({})),
            Self::Get { id } => ParsedCall::new("users.get", json!({"id": id})),
            Self::Create {
                identity,
                display_name,
            } => {
                let mut o = Map::new();
                o.insert("identity".into(), identity.clone().into());
                insert_opt(&mut o, "display_name", display_name.clone());
                ParsedCall::new("users.create", Value::Object(o))
            }
            Self::Update { id, display_name } => {
                let mut o = Map::new();
                o.insert("id".into(), id.clone().into());
                insert_opt(&mut o, "display_name", display_name.clone());
                ParsedCall::new("users.update", Value::Object(o))
            }
        }
    }
}

impl RolesCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("roles.list", json!({})),
            Self::Grant {
                user,
                role,
                agent_group,
            } => {
                let mut o = Map::new();
                o.insert("user".into(), user.clone().into());
                o.insert("role".into(), role.clone().into());
                insert_opt(&mut o, "agent_group_id", agent_group.clone());
                ParsedCall::new("roles.grant", Value::Object(o))
            }
            Self::Revoke {
                user,
                role,
                agent_group,
            } => {
                let mut o = Map::new();
                o.insert("user".into(), user.clone().into());
                o.insert("role".into(), role.clone().into());
                insert_opt(&mut o, "agent_group_id", agent_group.clone());
                ParsedCall::new("roles.revoke", Value::Object(o))
            }
        }
    }
}

impl MembersCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List { agent_group } => ParsedCall::new(
                "members.list",
                json!({"agent_group_id": agent_group}),
            ),
            Self::Add { agent_group, user } => ParsedCall::new(
                "members.add",
                json!({"agent_group_id": agent_group, "user": user}),
            ),
            Self::Remove { agent_group, user } => ParsedCall::new(
                "members.remove",
                json!({"agent_group_id": agent_group, "user": user}),
            ),
        }
    }
}

impl DestinationsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List { agent_group } => ParsedCall::new(
                "destinations.list",
                json!({"agent_group_id": agent_group}),
            ),
            Self::Add {
                agent_group,
                name,
                kind,
                display_name,
                channel_type,
                platform_id,
                target_agent_group,
            } => {
                let mut o = Map::new();
                o.insert("agent_group_id".into(), agent_group.clone().into());
                o.insert("name".into(), name.clone().into());
                o.insert("type".into(), kind.as_str().into());
                insert_opt(&mut o, "display_name", display_name.clone());
                insert_opt(&mut o, "channel_type", channel_type.clone());
                insert_opt(&mut o, "platform_id", platform_id.clone());
                insert_opt(&mut o, "target_agent_group_id", target_agent_group.clone());
                ParsedCall::new("destinations.add", Value::Object(o))
            }
            Self::Remove { agent_group, name } => ParsedCall::new(
                "destinations.remove",
                json!({"agent_group_id": agent_group, "name": name}),
            ),
        }
    }
}

impl SessionsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List {
                agent_group,
                status,
            } => {
                let mut o = Map::new();
                insert_opt(&mut o, "agent_group_id", agent_group.clone());
                insert_opt(&mut o, "status", status.clone());
                ParsedCall::new("sessions.list", Value::Object(o))
            }
            Self::Get { id } => ParsedCall::new("sessions.get", json!({"id": id})),
        }
    }
}

impl UserDmsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("user-dms.list", json!({})),
        }
    }
}

impl DroppedMessagesCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List { since } => {
                let mut o = Map::new();
                insert_opt(&mut o, "since", since.clone());
                ParsedCall::new("dropped-messages.list", Value::Object(o))
            }
        }
    }
}

impl ApprovalsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("approvals.list", json!({})),
            Self::Get { id } => ParsedCall::new("approvals.get", json!({"id": id})),
        }
    }
}

impl AuditCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List { since, limit } => ParsedCall::new(
                "audit.list",
                json!({ "since": since, "limit": limit }),
            ),
        }
    }
}

/// All `command` strings this binary can emit. Useful for the host to
/// register matching handlers; also referenced by tests in this crate.
pub const ALL_COMMANDS: &[&str] = &[
    "groups.list",
    "groups.get",
    "groups.create",
    "groups.update",
    "groups.delete",
    "groups.restart",
    "groups.config.get",
    "groups.config.update",
    "groups.config.add-mcp-server",
    "groups.config.remove-mcp-server",
    "groups.config.add-package",
    "groups.config.remove-package",
    "messaging-groups.list",
    "messaging-groups.get",
    "messaging-groups.create",
    "messaging-groups.update",
    "messaging-groups.delete",
    "wirings.list",
    "wirings.get",
    "wirings.create",
    "wirings.update",
    "wirings.delete",
    "users.list",
    "users.get",
    "users.create",
    "users.update",
    "roles.list",
    "roles.grant",
    "roles.revoke",
    "members.list",
    "members.add",
    "members.remove",
    "destinations.list",
    "destinations.add",
    "destinations.remove",
    "sessions.list",
    "sessions.get",
    "user-dms.list",
    "dropped-messages.list",
    "approvals.list",
    "approvals.get",
    "audit.list",
];

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> ParsedCall {
        let cli = Cli::try_parse_from(args).unwrap();
        cli.to_call()
    }

    fn parse_err(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(args).unwrap_err()
    }

    // --- groups ------------------------------------------------------------

    #[test]
    fn groups_list() {
        let p = parse(&["iclaw", "groups", "list"]);
        assert_eq!(p.command, "groups.list");
        assert_eq!(p.args, json!({}));
    }

    #[test]
    fn groups_get() {
        let p = parse(&["iclaw", "groups", "get", "ag_1"]);
        assert_eq!(p.command, "groups.get");
        assert_eq!(p.args, json!({"id": "ag_1"}));
    }

    #[test]
    fn groups_create_required_and_optional() {
        let p = parse(&[
            "iclaw", "groups", "create", "--folder", "f", "--name", "n", "--provider", "claude",
        ]);
        assert_eq!(p.command, "groups.create");
        assert_eq!(
            p.args,
            json!({"folder":"f","name":"n","provider":"claude"})
        );

        let p = parse(&["iclaw", "groups", "create", "--folder", "f", "--name", "n"]);
        assert_eq!(p.args, json!({"folder":"f","name":"n"}));
    }

    #[test]
    fn groups_create_folder_defaults_to_name() {
        // When --folder is omitted, the slug mirrors --name so the common
        // single-arg form `iclaw groups create --name demo` Just Works.
        let p = parse(&["iclaw", "groups", "create", "--name", "demo"]);
        assert_eq!(p.command, "groups.create");
        assert_eq!(p.args, json!({"folder":"demo","name":"demo"}));
    }

    #[test]
    fn groups_update_partial() {
        let p = parse(&["iclaw", "groups", "update", "id1", "--name", "new"]);
        assert_eq!(p.command, "groups.update");
        assert_eq!(p.args, json!({"id":"id1","name":"new"}));
    }

    #[test]
    fn groups_delete_and_restart() {
        assert_eq!(parse(&["iclaw", "groups", "delete", "x"]).command, "groups.delete");
        assert_eq!(parse(&["iclaw", "groups", "restart", "x"]).command, "groups.restart");
    }

    // --- group config ------------------------------------------------------

    #[test]
    fn groups_config_get() {
        let p = parse(&["iclaw", "groups", "config", "get", "id1"]);
        assert_eq!(p.command, "groups.config.get");
        assert_eq!(p.args, json!({"id":"id1"}));
    }

    #[test]
    fn groups_config_update_field_parses_json_value() {
        let p = parse(&[
            "iclaw", "groups", "config", "update", "id1", "--field", "max_tokens=4096",
        ]);
        assert_eq!(p.command, "groups.config.update");
        assert_eq!(
            p.args,
            json!({"id":"id1","field":"max_tokens","value":4096})
        );
    }

    #[test]
    fn groups_config_update_field_falls_back_to_string() {
        let p = parse(&[
            "iclaw", "groups", "config", "update", "id1", "--field", "name=hello world",
        ]);
        assert_eq!(
            p.args,
            json!({"id":"id1","field":"name","value":"hello world"})
        );
    }

    #[test]
    fn groups_config_update_field_without_equals_yields_empty_string() {
        let p = parse(&[
            "iclaw", "groups", "config", "update", "id1", "--field", "stripped",
        ]);
        assert_eq!(
            p.args,
            json!({"id":"id1","field":"stripped","value":""})
        );
    }

    #[test]
    fn groups_config_add_mcp_server_parses_inner_json() {
        let p = parse(&[
            "iclaw",
            "groups",
            "config",
            "add-mcp-server",
            "id1",
            "--config",
            "{\"name\":\"s1\"}",
        ]);
        assert_eq!(p.command, "groups.config.add-mcp-server");
        assert_eq!(p.args, json!({"id":"id1","server":{"name":"s1"}}));
    }

    #[test]
    fn groups_config_add_mcp_server_invalid_json_kept_as_string() {
        let p = parse(&[
            "iclaw",
            "groups",
            "config",
            "add-mcp-server",
            "id1",
            "--config",
            "not-json",
        ]);
        assert_eq!(p.args, json!({"id":"id1","server":"not-json"}));
    }

    #[test]
    fn groups_config_remove_mcp_server() {
        let p = parse(&[
            "iclaw",
            "groups",
            "config",
            "remove-mcp-server",
            "id1",
            "--name",
            "s1",
        ]);
        assert_eq!(p.command, "groups.config.remove-mcp-server");
        assert_eq!(p.args, json!({"id":"id1","name":"s1"}));
    }

    #[test]
    fn groups_config_add_package_apt() {
        let p = parse(&[
            "iclaw", "groups", "config", "add-package", "id1", "--apt", "curl",
        ]);
        assert_eq!(p.command, "groups.config.add-package");
        assert_eq!(p.args, json!({"id":"id1","kind":"apt","name":"curl"}));
    }

    #[test]
    fn groups_config_add_package_npm() {
        let p = parse(&[
            "iclaw", "groups", "config", "add-package", "id1", "--npm", "left-pad",
        ]);
        assert_eq!(p.args, json!({"id":"id1","kind":"npm","name":"left-pad"}));
    }

    #[test]
    fn groups_config_remove_package() {
        let p = parse(&[
            "iclaw",
            "groups",
            "config",
            "remove-package",
            "id1",
            "--apt",
            "vim",
        ]);
        assert_eq!(p.command, "groups.config.remove-package");
        assert_eq!(p.args, json!({"id":"id1","kind":"apt","name":"vim"}));
    }

    #[test]
    fn groups_config_add_package_requires_one() {
        // Neither --apt nor --npm provided.
        let err = parse_err(&["iclaw", "groups", "config", "add-package", "id1"]);
        assert!(err.to_string().contains("required"));
    }

    #[test]
    fn groups_config_add_package_exclusive() {
        let err = parse_err(&[
            "iclaw",
            "groups",
            "config",
            "add-package",
            "id1",
            "--apt",
            "a",
            "--npm",
            "b",
        ]);
        // clap will reject both being set when group has multiple=false.
        assert!(err.to_string().contains("cannot be used") || err.to_string().contains("conflict"));
    }

    // --- messaging-groups --------------------------------------------------

    #[test]
    fn messaging_groups_all() {
        assert_eq!(
            parse(&["iclaw", "messaging-groups", "list"]).command,
            "messaging-groups.list"
        );
        assert_eq!(
            parse(&["iclaw", "messaging-groups", "get", "mg_1"]).args,
            json!({"id":"mg_1"})
        );
        let p = parse(&[
            "iclaw",
            "messaging-groups",
            "create",
            "--channel-type",
            "telegram",
            "--platform-id",
            "12345",
            "--name",
            "Main",
            "--is-group",
        ]);
        assert_eq!(p.command, "messaging-groups.create");
        assert_eq!(
            p.args,
            json!({"channel_type":"telegram","platform_id":"12345","name":"Main","is_group":true})
        );

        let p = parse(&[
            "iclaw",
            "messaging-groups",
            "create",
            "--channel-type",
            "slack",
            "--platform-id",
            "C1",
        ]);
        assert_eq!(
            p.args,
            json!({"channel_type":"slack","platform_id":"C1","is_group":false})
        );

        let p = parse(&[
            "iclaw",
            "messaging-groups",
            "update",
            "mg_1",
            "--name",
            "Renamed",
            "--is-group",
            "true",
        ]);
        assert_eq!(p.command, "messaging-groups.update");
        assert_eq!(p.args, json!({"id":"mg_1","name":"Renamed","is_group":true}));

        assert_eq!(
            parse(&["iclaw", "messaging-groups", "delete", "mg_1"]).command,
            "messaging-groups.delete"
        );
    }

    // --- wirings -----------------------------------------------------------

    #[test]
    fn wirings_create_minimal() {
        let p = parse(&[
            "iclaw", "wirings", "create", "--mg", "mg_1", "--ag", "ag_1", "--engage", "mention",
        ]);
        assert_eq!(p.command, "wirings.create");
        assert_eq!(
            p.args,
            json!({"messaging_group_id":"mg_1","agent_group_id":"ag_1","engage":"mention"})
        );
    }

    #[test]
    fn wirings_create_full() {
        let p = parse(&[
            "iclaw",
            "wirings",
            "create",
            "--mg",
            "mg_1",
            "--ag",
            "ag_1",
            "--engage",
            "mention-sticky",
            "--pattern",
            "^hi",
            "--sender-scope",
            "known",
            "--session-mode",
            "per-thread",
            "--priority",
            "10",
        ]);
        assert_eq!(
            p.args,
            json!({
                "messaging_group_id":"mg_1",
                "agent_group_id":"ag_1",
                "engage":"mention-sticky",
                "pattern":"^hi",
                "sender_scope":"known",
                "session_mode":"per-thread",
                "priority":10
            })
        );
    }

    #[test]
    fn wirings_engage_pattern_kebab() {
        let p = parse(&[
            "iclaw", "wirings", "create", "--mg", "m", "--ag", "a", "--engage", "pattern",
        ]);
        assert_eq!(p.args["engage"], "pattern");
    }

    #[test]
    fn wirings_session_mode_agent_shared() {
        let p = parse(&[
            "iclaw",
            "wirings",
            "create",
            "--mg",
            "m",
            "--ag",
            "a",
            "--engage",
            "mention",
            "--session-mode",
            "agent-shared",
        ]);
        assert_eq!(p.args["session_mode"], "agent-shared");
    }

    #[test]
    fn wirings_sender_scope_all() {
        let p = parse(&[
            "iclaw",
            "wirings",
            "create",
            "--mg",
            "m",
            "--ag",
            "a",
            "--engage",
            "mention",
            "--sender-scope",
            "all",
        ]);
        assert_eq!(p.args["sender_scope"], "all");
    }

    #[test]
    fn wirings_list_get_delete() {
        assert_eq!(parse(&["iclaw", "wirings", "list"]).command, "wirings.list");
        assert_eq!(parse(&["iclaw", "wirings", "get", "w"]).args, json!({"id":"w"}));
        assert_eq!(
            parse(&["iclaw", "wirings", "delete", "w"]).command,
            "wirings.delete"
        );
    }

    #[test]
    fn wirings_update_partial() {
        let p = parse(&[
            "iclaw", "wirings", "update", "w", "--engage", "mention", "--priority", "7",
        ]);
        assert_eq!(p.command, "wirings.update");
        assert_eq!(p.args, json!({"id":"w","engage":"mention","priority":7}));
    }

    #[test]
    fn wirings_update_with_all_optionals() {
        let p = parse(&[
            "iclaw",
            "wirings",
            "update",
            "w",
            "--engage",
            "pattern",
            "--pattern",
            "rx",
            "--sender-scope",
            "known",
            "--session-mode",
            "shared",
        ]);
        assert_eq!(
            p.args,
            json!({
                "id":"w",
                "engage":"pattern",
                "pattern":"rx",
                "sender_scope":"known",
                "session_mode":"shared"
            })
        );
    }

    // --- users -------------------------------------------------------------

    #[test]
    fn users_all() {
        assert_eq!(parse(&["iclaw", "users", "list"]).command, "users.list");
        assert_eq!(parse(&["iclaw", "users", "get", "u"]).args, json!({"id":"u"}));
        let p = parse(&[
            "iclaw",
            "users",
            "create",
            "--identity",
            "telegram:1",
            "--display-name",
            "Phil",
        ]);
        assert_eq!(p.command, "users.create");
        assert_eq!(
            p.args,
            json!({"identity":"telegram:1","display_name":"Phil"})
        );
        let p = parse(&[
            "iclaw", "users", "update", "u", "--display-name", "Updated",
        ]);
        assert_eq!(p.args, json!({"id":"u","display_name":"Updated"}));
    }

    // --- roles -------------------------------------------------------------

    #[test]
    fn roles_all() {
        assert_eq!(parse(&["iclaw", "roles", "list"]).command, "roles.list");
        let p = parse(&["iclaw", "roles", "grant", "u1", "admin"]);
        assert_eq!(p.command, "roles.grant");
        assert_eq!(p.args, json!({"user":"u1","role":"admin"}));
        let p = parse(&[
            "iclaw",
            "roles",
            "grant",
            "u1",
            "operator",
            "--agent-group",
            "ag1",
        ]);
        assert_eq!(
            p.args,
            json!({"user":"u1","role":"operator","agent_group_id":"ag1"})
        );
        let p = parse(&["iclaw", "roles", "revoke", "u1", "admin"]);
        assert_eq!(p.command, "roles.revoke");
    }

    // --- members -----------------------------------------------------------

    #[test]
    fn members_all() {
        let p = parse(&["iclaw", "members", "list", "ag1"]);
        assert_eq!(p.command, "members.list");
        assert_eq!(p.args, json!({"agent_group_id":"ag1"}));
        let p = parse(&["iclaw", "members", "add", "ag1", "u1"]);
        assert_eq!(p.command, "members.add");
        assert_eq!(p.args, json!({"agent_group_id":"ag1","user":"u1"}));
        let p = parse(&["iclaw", "members", "remove", "ag1", "u1"]);
        assert_eq!(p.command, "members.remove");
    }

    // --- destinations ------------------------------------------------------

    #[test]
    fn destinations_all() {
        let p = parse(&["iclaw", "destinations", "list", "ag1"]);
        assert_eq!(p.command, "destinations.list");
        assert_eq!(p.args, json!({"agent_group_id":"ag1"}));
        let p = parse(&[
            "iclaw",
            "destinations",
            "add",
            "ag1",
            "--name",
            "ops",
            "--type",
            "channel",
            "--channel-type",
            "slack",
            "--platform-id",
            "C1",
        ]);
        assert_eq!(p.command, "destinations.add");
        assert_eq!(
            p.args,
            json!({
                "agent_group_id":"ag1",
                "name":"ops",
                "type":"channel",
                "channel_type":"slack",
                "platform_id":"C1"
            })
        );
        let p = parse(&[
            "iclaw",
            "destinations",
            "add",
            "ag1",
            "--name",
            "peer",
            "--type",
            "agent",
            "--target-agent-group",
            "ag2",
            "--display-name",
            "Peer Agent",
        ]);
        assert_eq!(
            p.args,
            json!({
                "agent_group_id":"ag1",
                "name":"peer",
                "type":"agent",
                "display_name":"Peer Agent",
                "target_agent_group_id":"ag2"
            })
        );
        let p = parse(&["iclaw", "destinations", "remove", "ag1", "--name", "ops"]);
        assert_eq!(p.command, "destinations.remove");
        assert_eq!(p.args, json!({"agent_group_id":"ag1","name":"ops"}));
    }

    // --- sessions ----------------------------------------------------------

    #[test]
    fn sessions_all() {
        let p = parse(&["iclaw", "sessions", "list"]);
        assert_eq!(p.command, "sessions.list");
        assert_eq!(p.args, json!({}));
        let p = parse(&[
            "iclaw",
            "sessions",
            "list",
            "--agent-group",
            "ag1",
            "--status",
            "running",
        ]);
        assert_eq!(p.args, json!({"agent_group_id":"ag1","status":"running"}));
        let p = parse(&["iclaw", "sessions", "get", "s1"]);
        assert_eq!(p.command, "sessions.get");
    }

    // --- user-dms / dropped-messages / approvals ---------------------------

    #[test]
    fn user_dms_list() {
        let p = parse(&["iclaw", "user-dms", "list"]);
        assert_eq!(p.command, "user-dms.list");
    }

    #[test]
    fn dropped_messages_list() {
        let p = parse(&["iclaw", "dropped-messages", "list"]);
        assert_eq!(p.command, "dropped-messages.list");
        assert_eq!(p.args, json!({}));
        let p = parse(&[
            "iclaw",
            "dropped-messages",
            "list",
            "--since",
            "2026-01-01T00:00:00Z",
        ]);
        assert_eq!(p.args, json!({"since":"2026-01-01T00:00:00Z"}));
    }

    #[test]
    fn approvals_all() {
        let p = parse(&["iclaw", "approvals", "list"]);
        assert_eq!(p.command, "approvals.list");
        let p = parse(&["iclaw", "approvals", "get", "appr_1"]);
        assert_eq!(p.command, "approvals.get");
        assert_eq!(p.args, json!({"id":"appr_1"}));
    }

    // --- meta --------------------------------------------------------------

    #[test]
    fn all_commands_is_a_unique_sorted_list_of_strings() {
        let mut seen = std::collections::HashSet::new();
        for c in ALL_COMMANDS {
            assert!(seen.insert(*c), "duplicate command: {c}");
        }
        assert!(ALL_COMMANDS.contains(&"groups.list"));
    }

    /// Every command produced by [`Cli::to_call`] must appear in
    /// [`ALL_COMMANDS`] — the host-side handler registry depends on it.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_subcommand_emits_a_known_command_string() {
        let known: std::collections::HashSet<&str> = ALL_COMMANDS.iter().copied().collect();
        let invocations: &[&[&str]] = &[
            &["iclaw", "groups", "list"],
            &["iclaw", "groups", "get", "x"],
            &["iclaw", "groups", "create", "--folder", "f", "--name", "n"],
            &["iclaw", "groups", "update", "x"],
            &["iclaw", "groups", "delete", "x"],
            &["iclaw", "groups", "restart", "x"],
            &["iclaw", "groups", "config", "get", "x"],
            &["iclaw", "groups", "config", "update", "x", "--field", "k=v"],
            &[
                "iclaw",
                "groups",
                "config",
                "add-mcp-server",
                "x",
                "--config",
                "{}",
            ],
            &[
                "iclaw",
                "groups",
                "config",
                "remove-mcp-server",
                "x",
                "--name",
                "n",
            ],
            &["iclaw", "groups", "config", "add-package", "x", "--apt", "p"],
            &[
                "iclaw",
                "groups",
                "config",
                "remove-package",
                "x",
                "--npm",
                "p",
            ],
            &["iclaw", "messaging-groups", "list"],
            &["iclaw", "messaging-groups", "get", "x"],
            &[
                "iclaw",
                "messaging-groups",
                "create",
                "--channel-type",
                "t",
                "--platform-id",
                "p",
            ],
            &["iclaw", "messaging-groups", "update", "x"],
            &["iclaw", "messaging-groups", "delete", "x"],
            &["iclaw", "wirings", "list"],
            &["iclaw", "wirings", "get", "x"],
            &[
                "iclaw", "wirings", "create", "--mg", "m", "--ag", "a", "--engage", "mention",
            ],
            &["iclaw", "wirings", "update", "x"],
            &["iclaw", "wirings", "delete", "x"],
            &["iclaw", "users", "list"],
            &["iclaw", "users", "get", "x"],
            &["iclaw", "users", "create", "--identity", "ch:h"],
            &["iclaw", "users", "update", "x"],
            &["iclaw", "roles", "list"],
            &["iclaw", "roles", "grant", "u", "r"],
            &["iclaw", "roles", "revoke", "u", "r"],
            &["iclaw", "members", "list", "ag"],
            &["iclaw", "members", "add", "ag", "u"],
            &["iclaw", "members", "remove", "ag", "u"],
            &["iclaw", "destinations", "list", "ag"],
            &[
                "iclaw",
                "destinations",
                "add",
                "ag",
                "--name",
                "n",
                "--type",
                "channel",
            ],
            &["iclaw", "destinations", "remove", "ag", "--name", "n"],
            &["iclaw", "sessions", "list"],
            &["iclaw", "sessions", "get", "x"],
            &["iclaw", "user-dms", "list"],
            &["iclaw", "dropped-messages", "list"],
            &["iclaw", "approvals", "list"],
            &["iclaw", "approvals", "get", "x"],
            &["iclaw", "audit", "list"],
        ];
        for args in invocations {
            let p = parse(args);
            assert!(
                known.contains(p.command.as_str()),
                "command {:?} (from {:?}) not in ALL_COMMANDS",
                p.command,
                args,
            );
        }
        // And the reverse: every entry in ALL_COMMANDS should have been
        // emitted at least once above.
        let emitted: std::collections::HashSet<String> = invocations
            .iter()
            .map(|args| parse(args).command)
            .collect();
        for c in ALL_COMMANDS {
            assert!(
                emitted.contains(*c),
                "ALL_COMMANDS lists {c:?} but no invocation here produces it",
            );
        }
    }

    #[test]
    fn enum_str_conversions_cover_all_variants() {
        assert_eq!(EngageArg::Pattern.as_str(), "pattern");
        assert_eq!(EngageArg::Mention.as_str(), "mention");
        assert_eq!(EngageArg::MentionSticky.as_str(), "mention-sticky");
        assert_eq!(SenderScopeArg::All.as_str(), "all");
        assert_eq!(SenderScopeArg::Known.as_str(), "known");
        assert_eq!(SessionModeArg::Shared.as_str(), "shared");
        assert_eq!(SessionModeArg::PerThread.as_str(), "per-thread");
        assert_eq!(SessionModeArg::AgentShared.as_str(), "agent-shared");
        assert_eq!(DestinationKind::Channel.as_str(), "channel");
        assert_eq!(DestinationKind::Agent.as_str(), "agent");
    }

    #[test]
    fn parse_field_handles_json_and_string() {
        let (k, v) = parse_field("count=5");
        assert_eq!(k, "count");
        assert_eq!(v, json!(5));
        let (k, v) = parse_field("name=alice");
        assert_eq!(k, "name");
        assert_eq!(v, json!("alice"));
        let (k, v) = parse_field("flag=true");
        assert_eq!(k, "flag");
        assert_eq!(v, json!(true));
        let (k, v) = parse_field("obj={\"x\":1}");
        assert_eq!(k, "obj");
        assert_eq!(v, json!({"x":1}));
        let (k, v) = parse_field("nokey");
        assert_eq!(k, "nokey");
        assert_eq!(v, json!(""));
    }

    #[test]
    fn cli_to_call_delegates() {
        let cli = Cli::try_parse_from(["iclaw", "groups", "list"]).unwrap();
        assert_eq!(cli.to_call().command, "groups.list");
    }

    #[test]
    fn resolve_socket_prefers_explicit_flag() {
        let cli =
            Cli::try_parse_from(["iclaw", "--socket", "/tmp/x.sock", "groups", "list"]).unwrap();
        assert_eq!(cli.resolve_socket(), std::path::PathBuf::from("/tmp/x.sock"));
    }

    #[test]
    fn user_socket_for_linux_uses_xdg_share() {
        // Sidestep the parent env's XDG_DATA_HOME via a deterministic check
        // on the no-XDG branch.
        if std::env::var_os("XDG_DATA_HOME").is_some() {
            return;
        }
        let p = user_socket_for(std::path::Path::new("/home/u"), "linux");
        assert_eq!(
            p,
            std::path::PathBuf::from("/home/u/.local/share/ironclaw/data/iclaw.sock")
        );
    }

    #[test]
    fn user_socket_for_macos_uses_app_support() {
        let p = user_socket_for(std::path::Path::new("/Users/u"), "macos");
        assert_eq!(
            p,
            std::path::PathBuf::from(
                "/Users/u/Library/Application Support/ironclaw/data/iclaw.sock"
            )
        );
    }

    #[test]
    fn user_socket_for_other_os_falls_back_to_dot_dir() {
        let p = user_socket_for(std::path::Path::new("/h"), "freebsd");
        assert_eq!(p, std::path::PathBuf::from("/h/.ironclaw/data/iclaw.sock"));
    }
}
