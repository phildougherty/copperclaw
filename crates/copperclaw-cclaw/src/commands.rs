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
    name = "cclaw",
    version,
    about = "copperclaw admin CLI",
    propagate_version = true
)]
pub struct Cli {
    /// Path to the Unix-domain socket the host is listening on. When
    /// unset, [`Cli::resolve_socket`] picks a sensible default based on
    /// the platform's user-data dir, falling back to `./data/cclaw.sock`.
    #[arg(long, env = "CCLAW_SOCKET", global = true)]
    pub socket: Option<std::path::PathBuf>,

    /// Emit raw JSON instead of the formatted table.
    #[arg(long, global = true)]
    pub json: bool,

    /// Top-level subcommand. When omitted, `cclaw` emits a one-shot
    /// operator dashboard via the `composite.dashboard` marker. The
    /// dashboard contract is in `crate::run_dashboard`.
    #[command(subcommand)]
    pub command: Option<TopCommand>,
}

impl Cli {
    /// Resolve the socket path the client should dial.
    ///
    /// Priority order:
    /// 1. Explicit `--socket` / `CCLAW_SOCKET` (clap populates `self.socket`).
    /// 2. Platform-default install root + `/data/cclaw.sock` (matches the
    ///    layout that `copperclaw-setup` produces with no `--data-dir`).
    /// 3. Legacy relative `./data/cclaw.sock` so behaviour is unchanged for
    ///    callers that ran the host out of a project-local checkout.
    pub fn resolve_socket(&self) -> std::path::PathBuf {
        if let Some(p) = &self.socket {
            return p.clone();
        }
        if let Some(p) = default_user_socket() {
            return p;
        }
        std::path::PathBuf::from("data/cclaw.sock")
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
/// rules used by `copperclaw-setup::steps::data_dir::default_data_dir_for`,
/// so the two binaries agree on where a fresh install lives.
#[must_use]
pub fn user_socket_for(home: &std::path::Path, os: &str) -> std::path::PathBuf {
    let root = match os {
        "macos" => home
            .join("Library")
            .join("Application Support")
            .join("copperclaw"),
        "linux" => std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|x| !x.as_os_str().is_empty())
            .map_or_else(
                || home.join(".local").join("share").join("copperclaw"),
                |xdg| xdg.join("copperclaw"),
            ),
        _ => home.join(".copperclaw"),
    };
    root.join("data").join("cclaw.sock")
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
    /// First-run diagnostic. Walks the install end-to-end and reports
    /// what's wired, what's missing, and what command to run to fix
    /// each missing piece. Designed so a fresh user can paste the
    /// output into the README's troubleshooting section.
    ///
    /// Returns a non-zero exit when any check is in FAIL state so CI
    /// and pre-flight scripts can branch on it.
    Doctor,
    /// Per-group daily budget caps.
    Budgets {
        #[command(subcommand)]
        action: BudgetsCmd,
    },
    /// Per-group token usage rollup.
    Usage {
        /// Look-back window. Same format as `audit list --since`.
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Interactive REPL: read lines from this terminal, write them
    /// into the host's chat fifo, tail `chat.log` for replies.
    ///
    /// The host must already be running with a `cli` channel wired
    /// (see `cclaw quickstart cli`). Path defaults to the standard
    /// install layout; override with `--fifo` / `--log` for tests
    /// or non-default installs.
    Chat {
        /// FIFO the host reads its stdin from. Defaults to
        /// `<install_root>/chat.fifo`.
        #[arg(long)]
        fifo: Option<std::path::PathBuf>,
        /// Log the host writes its stdout to. Defaults to
        /// `<install_root>/chat.log`.
        #[arg(long)]
        log: Option<std::path::PathBuf>,
        /// Refuse to auto-start the host if it isn't already running.
        /// By default `cclaw chat` will run `copperclaw start` for you
        /// when the host's FIFO is missing.
        #[arg(long)]
        no_autostart: bool,
    },
    /// Emit shell completion script for `cclaw`.
    ///
    /// Pipe the output into your shell's completion dir, e.g.
    /// `cclaw completions bash > /etc/bash_completion.d/cclaw` or
    /// `cclaw completions zsh > ~/.zfunc/_cclaw`.
    Completions {
        /// Target shell. Supports `bash`, `zsh`, `fish`, `elvish`, `powershell`.
        shell: clap_complete::Shell,
    },
    /// Central database backup and restore.
    ///
    /// Backup copies the central `copperclaw.db` file after running a WAL
    /// checkpoint. Restore always refuses while the host is running and tells
    /// the operator to stop the host first.
    Db {
        #[command(subcommand)]
        action: DbCmd,
    },
    /// MCP server preset registry.
    ///
    /// `cclaw mcp list-presets` shows the built-in catalog of curated MCP
    /// server configurations. `cclaw mcp add <preset>` writes the chosen
    /// preset into an agent group's container config so it starts at the
    /// next container spawn.
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// Print the central DB schema version summary.
    ///
    /// Prints a JSON object `{ "expected": N, "applied": M, "status": "ok|pending|future" }`
    /// where:
    /// - `expected` is the number of migrations compiled into this binary.
    /// - `applied` is the number of migrations already recorded in the DB.
    /// - `status` is `"ok"` when equal, `"pending"` when applied < expected,
    ///   `"future"` when applied > expected (downgrade detected).
    #[command(name = "schema-version")]
    SchemaVersion,
}

// --- db --------------------------------------------------------------------

/// `cclaw db ...` — central database backup / restore.
#[derive(Debug, Subcommand)]
pub enum DbCmd {
    /// Backup the central DB to `<path>`. Runs a WAL checkpoint first, then
    /// atomically copies the `SQLite` file. The backup is a valid standalone
    /// `SQLite` database that can be opened immediately.
    ///
    /// The host does not need to be stopped for a backup; the WAL checkpoint
    /// ensures the copy is consistent. A non-zero `wal_pages_remaining` in
    /// the response means a write transaction was open during the checkpoint
    /// and some WAL data was included — this is safe and expected under load.
    Backup {
        /// Destination file path. Parent directories are created automatically.
        path: String,
    },
    /// Refuse to restore while the host is running.
    ///
    /// Restoring requires an exclusive lock on the `SQLite` file; the host
    /// holds an open WAL connection. Stop the host first, then copy the
    /// backup file over `<data_dir>/copperclaw.db` manually and restart.
    Restore {
        /// Backup file to restore from.
        path: String,
    },
}

// --- mcp -------------------------------------------------------------------

/// `cclaw mcp ...` — MCP server preset registry.
#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// List all built-in MCP server presets. No socket round-trip required.
    #[command(name = "list-presets")]
    ListPresets,
    /// Add a preset MCP server to an agent group's container config.
    ///
    /// The preset entry is written into `container_configs.mcp_servers` and
    /// takes effect at the next container spawn for that group. If a server
    /// with the same name already exists it is replaced (idempotent).
    ///
    /// Example:
    ///   cclaw mcp add postgres --agent-group-id <id> \\
    ///       --env `POSTGRES_CONNECTION_STRING=postgres://localhost/mydb`
    Add {
        /// Preset name from `cclaw mcp list-presets`.
        preset: String,
        /// Agent group to configure.
        #[arg(long = "agent-group-id")]
        agent_group_id: String,
        /// Environment variable overrides in `KEY=VALUE` form.
        /// May be specified multiple times.
        #[arg(long)]
        env: Vec<String>,
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
    ///   cclaw groups create --folder <folder> --name <name>
    ///   cclaw messaging-groups create --channel-type cli --platform-id stdin --name <name>
    ///   cclaw wirings create --mg <new-mg-id> --ag <new-ag-id> --engage pattern --pattern '.*'
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
    /// Enable the bundled coding skills (`coding-task`, `git-commit`,
    /// `code-review`, `testing`) for this group.
    #[command(name = "enable-coding")]
    EnableCoding { id: String },
    /// Disable the bundled coding skills for this group (the default
    /// for new groups).
    #[command(name = "disable-coding")]
    DisableCoding { id: String },
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
    /// Set (or replace) the egress allow-list for this group.
    ///
    /// Each entry must be a `host:port` pair (e.g. `api.example.com:443`).
    /// Passing no `--allow` arguments clears the list, restoring the
    /// default allow-all policy.
    #[command(name = "set-egress-allow")]
    SetEgressAllow {
        id: String,
        /// Host:port entries to allow. May be repeated. Pass no `--allow`
        /// flags to clear the list.
        #[arg(long = "allow", num_args = 0..)]
        allow: Vec<String>,
    },
    /// Set (or replace) the per-group resource caps.
    ///
    /// All flags are optional. To clear an existing cap, omit its flag.
    /// Docker runtime: --cpus / --memory / --pids-limit applied at spawn.
    /// Apple Container runtime: returns an error if any limit is set.
    #[command(name = "set-resource-limits")]
    SetResourceLimits {
        id: String,
        /// CPU quota as a fraction of one CPU (e.g. `1.5` for 1.5 CPUs).
        #[arg(long)]
        cpus: Option<String>,
        /// Memory cap in mebibytes (e.g. `512` for 512 MiB).
        #[arg(long)]
        memory_mb: Option<u64>,
        /// Maximum number of processes the container may create.
        #[arg(long)]
        pids_limit: Option<u64>,
    },
    /// Open the container config as TOML in `$EDITOR` (falls back to
    /// `$VISUAL`, then `vi`). On save the diff is converted into one
    /// `groups.config.update` (or dedicated mcp/package) call per
    /// changed field. Read-only fields (`agent_group_id`, `updated_at`)
    /// are rendered as comments and ignored if edited. Pass `--dry-run`
    /// to print the diff without committing anything.
    Edit {
        id: String,
        /// Skip the update step; just print what would change.
        #[arg(long = "dry-run")]
        dry_run: bool,
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
    /// List configured messaging groups (channel + platform-id pairs).
    List,
    /// Show one messaging group by id.
    Get {
        /// Messaging-group id (UUID).
        id: String,
    },
    /// Create a new messaging group — the channel + platform-id row that
    /// `wirings` then attach to an agent group.
    Create {
        /// Channel kind: `cli`, `telegram`, `slack`, `discord`, `matrix`, ...
        #[arg(long = "channel-type")]
        channel_type: String,
        /// Channel-native id for the conversation (Slack channel id, Discord
        /// guild id, Telegram chat id, ...).
        #[arg(long = "platform-id")]
        platform_id: String,
        /// Display name for the dashboard (`cclaw messaging-groups list`).
        #[arg(long)]
        name: Option<String>,
        /// Mark as a group conversation (vs. a DM) for sender-scope logic.
        #[arg(long = "is-group")]
        is_group: bool,
    },
    /// Update a messaging group's name or group/DM flag.
    Update {
        /// Messaging-group id.
        id: String,
        /// New display name.
        #[arg(long)]
        name: Option<String>,
        /// New group/DM flag.
        #[arg(long = "is-group")]
        is_group: Option<bool>,
    },
    /// Delete a messaging group. Wirings that reference it are deleted too.
    Delete {
        /// Messaging-group id.
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
    /// List all wirings (messaging-group → agent-group bindings).
    List,
    /// Show one wiring by id.
    Get {
        /// Wiring id.
        id: String,
    },
    /// Create a wiring: route messages from a messaging group to an agent
    /// group, with rules for when to engage and how to session messages.
    Create {
        /// Messaging-group id (the source side).
        #[arg(long = "mg")]
        mg: String,
        /// Agent-group id (the destination side).
        #[arg(long = "ag")]
        ag: String,
        /// Engage rule: `all` / `mention` / `mention-sticky` / `pattern`.
        #[arg(long)]
        engage: EngageArg,
        /// Regex applied to message text when `engage=pattern`.
        #[arg(long)]
        pattern: Option<String>,
        /// Sender scope: `all` (default — everyone) or `known` (registered users only).
        #[arg(long = "sender-scope")]
        sender_scope: Option<SenderScopeArg>,
        /// Session mode: `shared` (default), `per-thread`, or `agent-shared`.
        #[arg(long = "session-mode")]
        session_mode: Option<SessionModeArg>,
        /// Sort priority when multiple wirings match (higher fires first).
        #[arg(long)]
        priority: Option<i64>,
    },
    /// Update one or more fields on an existing wiring.
    Update {
        /// Wiring id.
        id: String,
        /// New engage rule.
        #[arg(long)]
        engage: Option<EngageArg>,
        /// New pattern (only valid when engage=pattern).
        #[arg(long)]
        pattern: Option<String>,
        /// New sender scope.
        #[arg(long = "sender-scope")]
        sender_scope: Option<SenderScopeArg>,
        /// New session mode.
        #[arg(long = "session-mode")]
        session_mode: Option<SessionModeArg>,
        /// New sort priority.
        #[arg(long)]
        priority: Option<i64>,
    },
    /// Delete a wiring. Sessions already routed through it stay; no new
    /// messages will be routed via this wiring after deletion.
    Delete {
        /// Wiring id.
        id: String,
    },
}

// --- users -----------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum UsersCmd {
    /// List known user records (approved senders + admin-created users).
    List,
    /// Show one user by id.
    Get {
        /// User id (UUID).
        id: String,
    },
    /// Create a user record directly (skips the sender-approval flow).
    Create {
        /// Identity string — `<channel_type>:<platform_id>`, e.g.
        /// `telegram:12345` or `slack:U01ABCDEF`.
        #[arg(long)]
        identity: String,
        /// Optional display name shown in dashboards.
        #[arg(long = "display-name")]
        display_name: Option<String>,
    },
    /// Update a user's display name.
    Update {
        /// User id.
        id: String,
        /// New display name.
        #[arg(long = "display-name")]
        display_name: Option<String>,
    },
}

// --- roles -----------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum RolesCmd {
    /// List role assignments across all users / agent groups.
    List,
    /// Grant a role to a user, optionally scoped to one agent group.
    Grant {
        /// User id.
        user: String,
        /// Role name (e.g. `admin`, `member`).
        role: String,
        /// Scope the grant to one agent group; omit for an install-wide grant.
        #[arg(long = "agent-group")]
        agent_group: Option<String>,
    },
    /// Revoke a previously-granted role.
    Revoke {
        /// User id.
        user: String,
        /// Role name to revoke.
        role: String,
        /// Agent group the grant was scoped to (must match the grant).
        #[arg(long = "agent-group")]
        agent_group: Option<String>,
    },
}

// --- members ---------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum MembersCmd {
    /// List users who are members of an agent group.
    List {
        /// Agent-group id.
        agent_group: String,
    },
    /// Add a user as a member of an agent group.
    Add {
        /// Agent-group id.
        agent_group: String,
        /// User id.
        user: String,
    },
    /// Remove a user from an agent group's membership.
    Remove {
        /// Agent-group id.
        agent_group: String,
        /// User id.
        user: String,
    },
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
    /// List the named outbound destinations configured for an agent group.
    List {
        /// Agent-group id.
        agent_group: String,
    },
    /// Add a named outbound destination that the agent can `send_message` to
    /// by name (vs. having to know the channel/platform-id pair).
    Add {
        /// Agent-group id that owns this destination.
        agent_group: String,
        /// Friendly name the agent uses (e.g. `releases`, `boss-dm`).
        #[arg(long)]
        name: String,
        /// Destination kind: `channel` (external chat) or `agent` (sibling agent).
        #[arg(long = "type")]
        kind: DestinationKind,
        /// Optional display name for dashboards.
        #[arg(long = "display-name")]
        display_name: Option<String>,
        /// For `--type channel`: the channel kind (e.g. `slack`, `discord`).
        #[arg(long = "channel-type")]
        channel_type: Option<String>,
        /// For `--type channel`: the channel-native platform id.
        #[arg(long = "platform-id")]
        platform_id: Option<String>,
        /// For `--type agent`: the destination agent-group id.
        #[arg(long = "target-agent-group")]
        target_agent_group: Option<String>,
    },
    /// Remove a named destination by name.
    Remove {
        /// Agent-group id that owns the destination.
        agent_group: String,
        /// Destination name to remove.
        #[arg(long)]
        name: String,
    },
}

// --- sessions --------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum SessionsCmd {
    /// List sessions, optionally filtered by agent group and / or status.
    List {
        /// Agent-group id to filter by.
        #[arg(long = "agent-group")]
        agent_group: Option<String>,
        /// Status filter: `active`, `idle`, `stopped`, `crashed`.
        #[arg(long)]
        status: Option<String>,
    },
    /// Show one session by id (state, last-active, container status,
    /// last few inbound / outbound rows).
    Get {
        /// Session id (UUID).
        id: String,
    },
    /// Delete a session: remove its central-DB row plus every per-session
    /// row that references it (`agent_turns`, `tasks`, `pending_questions`,
    /// `pending_approvals`) and its on-disk directory under
    /// `<data_dir>/sessions/<agent_group>/<session>/`.
    ///
    /// Refuses by default when the session's container is still Running;
    /// pass `--force` to delete anyway (the container will be orphaned
    /// until the next restart of the host).
    Delete {
        /// Session id (UUID).
        id: String,
        /// Delete even if the session's container is not Stopped.
        #[arg(long)]
        force: bool,
    },
}

// --- user-dms --------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum UserDmsCmd {
    /// List opened DM channels keyed by `(user, channel_type)`.
    List,
}

// --- dropped-messages ------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum DroppedMessagesCmd {
    /// List inbound messages the router dropped (no messaging group found,
    /// unknown sender, etc.).
    List {
        #[arg(long)]
        since: Option<String>,
    },
    /// List outbound messages the delivery loop could not deliver after all
    /// retries were exhausted. These rows are candidates for replay.
    #[command(name = "outbound-list")]
    OutboundList {
        /// Look-back window. ISO-8601 timestamp or relative shorthand
        /// (`1h`, `24h`, `7d`). Omit to list all.
        #[arg(long)]
        since: Option<String>,
        /// Maximum number of rows to return (default: 50).
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Re-queue an outbound dead-letter row. The row is re-inserted into the
    /// originating session's `messages_out` table with a fresh `deliver_after`
    /// so the delivery loop picks it up on its next sweep.
    Replay {
        /// Dead-letter row id returned by `cclaw dropped-messages outbound-list`.
        id: String,
    },
}

// --- approvals -------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum ApprovalsCmd {
    /// List pending approvals across all families (sender, channel,
    /// `install_packages`, `add_mcp_server`).
    List,
    /// Show one approval by id.
    Get {
        /// Approval id.
        id: String,
    },
    /// Approve a pending row by id. Dispatches per family based on the
    /// row's `action` column: `sender` / `approve_sender` upserts into
    /// `users`; `channel` creates a `messaging_groups` row (no auto-wire);
    /// `install_packages` merges into `container_configs.packages_apt` /
    /// `packages_npm`; `add_mcp_server` inserts into
    /// `container_configs.mcp_servers`. Re-approving an already-resolved
    /// row is a no-op that returns the row unchanged.
    ///
    /// The package / MCP families do NOT auto-rebuild the container; the
    /// response includes a hint instructing the operator to run
    /// `cclaw groups restart <id>` when convenient.
    #[command(name = "approve-id")]
    ApproveById {
        /// Pending-approval row id (UUID).
        id: String,
    },
    /// Deny a pending row by id. Marks the row `status = 'denied'`
    /// without applying any side effects. Idempotent.
    #[command(name = "deny")]
    Deny {
        /// Pending-approval row id (UUID).
        id: String,
    },
    /// Approve a sender by `(channel_type, identity)`. Persists a
    /// `users` row keyed on that pair; the host's sender-scope
    /// gate consults `users` on every inbound, so the approval
    /// takes effect on the next message without a host restart.
    /// (Use `cclaw approvals approve-id <id>` for non-sender families.)
    Approve {
        /// Channel kind the sender uses (e.g. `telegram`, `slack`).
        #[arg(long)]
        channel: String,
        /// Sender's platform-native id.
        #[arg(long)]
        identity: String,
        /// Optional display name to attach to the new `users` row.
        #[arg(long)]
        display_name: Option<String>,
    },
}

/// `cclaw budgets ...` — per-agent-group daily and rate-limit caps.
#[derive(Debug, Subcommand)]
pub enum BudgetsCmd {
    /// List all configured budgets (daily token cap + rate caps).
    List,
    /// Set or update a group's caps.
    ///
    /// `--daily-tokens 0` or `--clear` removes the daily token cap.
    /// `--turns-per-minute 0` removes the per-minute rate cap.
    /// `--turns-per-hour 0` removes the per-hour rate cap.
    Set {
        #[arg(long)]
        agent_group_id: String,
        #[arg(long)]
        daily_tokens: Option<i64>,
        /// Max LLM calls per trailing 60-second window. 0 = remove cap.
        #[arg(long)]
        turns_per_minute: Option<i64>,
        /// Max LLM calls per trailing 3600-second window. 0 = remove cap.
        #[arg(long)]
        turns_per_hour: Option<i64>,
        #[arg(long)]
        clear: bool,
    },
}

/// `cclaw audit ...` — read the mutation audit log.
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
    ///
    /// When no subcommand is supplied, emits the `composite.dashboard`
    /// marker so [`crate::run_cli`] can dispatch the no-args dashboard.
    pub fn to_call(&self) -> ParsedCall {
        match &self.command {
            Some(cmd) => cmd.to_call(),
            None => ParsedCall::new("composite.dashboard", json!({})),
        }
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
            Self::Budgets { action } => action.to_call(),
            Self::Quickstart { action } => action.to_call(),
            Self::Db { action } => action.to_call(),
            Self::Mcp { action } => action.to_call(),
            Self::Status => ParsedCall::new("composite.status", json!({})),
            Self::Health => ParsedCall::new("composite.health", json!({})),
            Self::Doctor => ParsedCall::new("composite.doctor", json!({})),
            Self::Usage { since } => ParsedCall::new("usage.rollup", json!({"since": since})),
            Self::Chat {
                fifo,
                log,
                no_autostart,
            } => {
                let mut o = Map::new();
                if let Some(p) = fifo {
                    o.insert("fifo".into(), p.to_string_lossy().into_owned().into());
                }
                if let Some(p) = log {
                    o.insert("log".into(), p.to_string_lossy().into_owned().into());
                }
                if *no_autostart {
                    o.insert("no_autostart".into(), Value::Bool(true));
                }
                ParsedCall::new("composite.chat", Value::Object(o))
            }
            // Completions are emitted entirely client-side; the marker
            // command carries the requested shell name so `run_cli`
            // can short-circuit before any transport call.
            Self::Completions { shell } => ParsedCall::new(
                "composite.completions",
                json!({ "shell": shell.to_string() }),
            ),
            Self::SchemaVersion => ParsedCall::new("schema.version", json!({})),
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

impl DbCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::Backup { path } => ParsedCall::new("db.backup", json!({"path": path})),
            Self::Restore { path } => ParsedCall::new("db.restore", json!({"path": path})),
        }
    }
}

impl McpCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::ListPresets => ParsedCall::new("mcp.list-presets", json!({})),
            Self::Add {
                preset,
                agent_group_id,
                env,
            } => {
                let mut env_obj = serde_json::Map::new();
                for kv in env {
                    if let Some((k, v)) = kv.split_once('=') {
                        env_obj.insert(k.to_string(), Value::String(v.to_string()));
                    }
                }
                ParsedCall::new(
                    "mcp.add",
                    json!({
                        "preset": preset,
                        "agent_group_id": agent_group_id,
                        "env": env_obj,
                    }),
                )
            }
        }
    }
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
                // `cclaw groups create --name X` and have the slug
                // chosen for them.
                let folder_value = folder.clone().unwrap_or_else(|| name.clone());
                o.insert("folder".into(), folder_value.into());
                o.insert("name".into(), name.clone().into());
                insert_opt(&mut o, "provider", provider.clone());
                ParsedCall::new("groups.create", Value::Object(o))
            }
            Self::Update { id, name, provider } => {
                let mut o = Map::new();
                o.insert("id".into(), id.clone().into());
                insert_opt(&mut o, "name", name.clone());
                insert_opt(&mut o, "provider", provider.clone());
                ParsedCall::new("groups.update", Value::Object(o))
            }
            Self::Delete { id } => ParsedCall::new("groups.delete", json!({"id": id})),
            Self::Restart { id } => ParsedCall::new("groups.restart", json!({"id": id})),
            Self::EnableCoding { id } => ParsedCall::new(
                "groups.config.set-coding-enabled",
                json!({"id": id, "enabled": true}),
            ),
            Self::DisableCoding { id } => ParsedCall::new(
                "groups.config.set-coding-enabled",
                json!({"id": id, "enabled": false}),
            ),
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
            Self::SetEgressAllow { id, allow } => {
                let allow_json: Vec<Value> =
                    allow.iter().map(|s| Value::String(s.clone())).collect();
                ParsedCall::new(
                    "groups.config.set-egress-allow",
                    json!({"id": id, "allow": Value::Array(allow_json)}),
                )
            }
            Self::SetResourceLimits {
                id,
                cpus,
                memory_mb,
                pids_limit,
            } => {
                let mut limits = Map::new();
                if let Some(c) = cpus {
                    limits.insert("cpus".into(), c.clone().into());
                }
                if let Some(m) = memory_mb {
                    limits.insert("memory_mb".into(), (*m).into());
                }
                if let Some(p) = pids_limit {
                    limits.insert("pids_limit".into(), (*p).into());
                }
                ParsedCall::new(
                    "groups.config.set-resource-limits",
                    json!({"id": id, "limits": Value::Object(limits)}),
                )
            }
            Self::Edit { id, dry_run } => ParsedCall::new(
                "composite.groups.config-edit",
                json!({"id": id, "dry_run": *dry_run}),
            ),
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
            Self::List { agent_group } => {
                ParsedCall::new("members.list", json!({"agent_group_id": agent_group}))
            }
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
            Self::List { agent_group } => {
                ParsedCall::new("destinations.list", json!({"agent_group_id": agent_group}))
            }
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
            Self::Delete { id, force } => {
                ParsedCall::new("sessions.delete", json!({"id": id, "force": *force}))
            }
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
            Self::OutboundList { since, limit } => {
                let mut o = Map::new();
                insert_opt(&mut o, "since", since.clone());
                o.insert("limit".into(), (*limit).into());
                ParsedCall::new("dropped-messages.outbound-list", Value::Object(o))
            }
            Self::Replay { id } => ParsedCall::new("dropped-messages.replay", json!({"id": id})),
        }
    }
}

impl ApprovalsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("approvals.list", json!({})),
            Self::Get { id } => ParsedCall::new("approvals.get", json!({"id": id})),
            Self::ApproveById { id } => ParsedCall::new("approvals.approve", json!({"id": id})),
            Self::Deny { id } => ParsedCall::new("approvals.deny", json!({"id": id})),
            Self::Approve {
                channel,
                identity,
                display_name,
            } => {
                let mut o = Map::new();
                o.insert("channel_type".into(), channel.clone().into());
                o.insert("identity".into(), identity.clone().into());
                insert_opt(&mut o, "display_name", display_name.clone());
                ParsedCall::new("approvals.approve_sender", Value::Object(o))
            }
        }
    }
}

impl AuditCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List { since, limit } => {
                ParsedCall::new("audit.list", json!({ "since": since, "limit": limit }))
            }
        }
    }
}

impl BudgetsCmd {
    pub fn to_call(&self) -> ParsedCall {
        match self {
            Self::List => ParsedCall::new("budgets.list", json!({})),
            Self::Set {
                agent_group_id,
                daily_tokens,
                turns_per_minute,
                turns_per_hour,
                clear,
            } => {
                let mut o = Map::new();
                o.insert("agent_group_id".into(), agent_group_id.clone().into());
                if *clear {
                    o.insert("daily_tokens".into(), Value::Null);
                } else if let Some(n) = daily_tokens {
                    o.insert("daily_tokens".into(), (*n).into());
                }
                // 0 means "remove the cap" (normalised to null by the host).
                if let Some(n) = turns_per_minute {
                    o.insert("turns_per_minute".into(), (*n).into());
                }
                if let Some(n) = turns_per_hour {
                    o.insert("turns_per_hour".into(), (*n).into());
                }
                ParsedCall::new("budgets.set", Value::Object(o))
            }
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
    "groups.config.set-egress-allow",
    "groups.config.set-resource-limits",
    "groups.config.set-coding-enabled",
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
    "sessions.delete",
    "user-dms.list",
    "dropped-messages.list",
    "dropped-messages.outbound-list",
    "dropped-messages.replay",
    "db.backup",
    "db.restore",
    "mcp.list-presets",
    "mcp.add",
    "approvals.list",
    "approvals.get",
    "approvals.approve_sender",
    "approvals.approve",
    "approvals.deny",
    "audit.list",
    "budgets.list",
    "budgets.set",
    "usage.rollup",
    "schema.version",
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
        let p = parse(&["cclaw", "groups", "list"]);
        assert_eq!(p.command, "groups.list");
        assert_eq!(p.args, json!({}));
    }

    #[test]
    fn groups_get() {
        let p = parse(&["cclaw", "groups", "get", "ag_1"]);
        assert_eq!(p.command, "groups.get");
        assert_eq!(p.args, json!({"id": "ag_1"}));
    }

    #[test]
    fn groups_create_required_and_optional() {
        let p = parse(&[
            "cclaw",
            "groups",
            "create",
            "--folder",
            "f",
            "--name",
            "n",
            "--provider",
            "claude",
        ]);
        assert_eq!(p.command, "groups.create");
        assert_eq!(p.args, json!({"folder":"f","name":"n","provider":"claude"}));

        let p = parse(&["cclaw", "groups", "create", "--folder", "f", "--name", "n"]);
        assert_eq!(p.args, json!({"folder":"f","name":"n"}));
    }

    #[test]
    fn groups_create_folder_defaults_to_name() {
        // When --folder is omitted, the slug mirrors --name so the common
        // single-arg form `cclaw groups create --name demo` Just Works.
        let p = parse(&["cclaw", "groups", "create", "--name", "demo"]);
        assert_eq!(p.command, "groups.create");
        assert_eq!(p.args, json!({"folder":"demo","name":"demo"}));
    }

    #[test]
    fn groups_update_partial() {
        let p = parse(&["cclaw", "groups", "update", "id1", "--name", "new"]);
        assert_eq!(p.command, "groups.update");
        assert_eq!(p.args, json!({"id":"id1","name":"new"}));
    }

    #[test]
    fn groups_delete_and_restart() {
        assert_eq!(
            parse(&["cclaw", "groups", "delete", "x"]).command,
            "groups.delete"
        );
        assert_eq!(
            parse(&["cclaw", "groups", "restart", "x"]).command,
            "groups.restart"
        );
    }

    // --- group config ------------------------------------------------------

    #[test]
    fn groups_config_get() {
        let p = parse(&["cclaw", "groups", "config", "get", "id1"]);
        assert_eq!(p.command, "groups.config.get");
        assert_eq!(p.args, json!({"id":"id1"}));
    }

    #[test]
    fn groups_config_update_field_parses_json_value() {
        let p = parse(&[
            "cclaw",
            "groups",
            "config",
            "update",
            "id1",
            "--field",
            "max_tokens=4096",
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
            "cclaw",
            "groups",
            "config",
            "update",
            "id1",
            "--field",
            "name=hello world",
        ]);
        assert_eq!(
            p.args,
            json!({"id":"id1","field":"name","value":"hello world"})
        );
    }

    #[test]
    fn groups_config_update_field_without_equals_yields_empty_string() {
        let p = parse(&[
            "cclaw", "groups", "config", "update", "id1", "--field", "stripped",
        ]);
        assert_eq!(p.args, json!({"id":"id1","field":"stripped","value":""}));
    }

    #[test]
    fn groups_config_add_mcp_server_parses_inner_json() {
        let p = parse(&[
            "cclaw",
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
            "cclaw",
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
    fn groups_config_edit_emits_composite() {
        let p = parse(&["cclaw", "groups", "config", "edit", "id1"]);
        assert_eq!(p.command, "composite.groups.config-edit");
        assert_eq!(p.args, json!({"id": "id1", "dry_run": false}));

        let p = parse(&["cclaw", "groups", "config", "edit", "id1", "--dry-run"]);
        assert_eq!(p.args, json!({"id": "id1", "dry_run": true}));
    }

    #[test]
    fn no_subcommand_emits_dashboard_composite() {
        let cli = Cli::try_parse_from(["cclaw"]).unwrap();
        let p = cli.to_call();
        assert_eq!(p.command, "composite.dashboard");
    }

    #[test]
    fn groups_config_remove_mcp_server() {
        let p = parse(&[
            "cclaw",
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
            "cclaw",
            "groups",
            "config",
            "add-package",
            "id1",
            "--apt",
            "curl",
        ]);
        assert_eq!(p.command, "groups.config.add-package");
        assert_eq!(p.args, json!({"id":"id1","kind":"apt","name":"curl"}));
    }

    #[test]
    fn groups_config_add_package_npm() {
        let p = parse(&[
            "cclaw",
            "groups",
            "config",
            "add-package",
            "id1",
            "--npm",
            "left-pad",
        ]);
        assert_eq!(p.args, json!({"id":"id1","kind":"npm","name":"left-pad"}));
    }

    #[test]
    fn groups_config_remove_package() {
        let p = parse(&[
            "cclaw",
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
        let err = parse_err(&["cclaw", "groups", "config", "add-package", "id1"]);
        assert!(err.to_string().contains("required"));
    }

    #[test]
    fn groups_config_add_package_exclusive() {
        let err = parse_err(&[
            "cclaw",
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

    // --- groups config set-egress-allow ------------------------------------

    #[test]
    fn groups_config_set_egress_allow_with_entries() {
        let p = parse(&[
            "cclaw",
            "groups",
            "config",
            "set-egress-allow",
            "id1",
            "--allow",
            "api.example.com:443",
            "--allow",
            "db.local:5432",
        ]);
        assert_eq!(p.command, "groups.config.set-egress-allow");
        assert_eq!(
            p.args,
            json!({"id": "id1", "allow": ["api.example.com:443", "db.local:5432"]})
        );
    }

    #[test]
    fn groups_config_set_egress_allow_empty_clears() {
        let p = parse(&["cclaw", "groups", "config", "set-egress-allow", "id1"]);
        assert_eq!(p.command, "groups.config.set-egress-allow");
        assert_eq!(p.args, json!({"id": "id1", "allow": []}));
    }

    // --- groups config set-resource-limits ---------------------------------

    #[test]
    fn groups_config_set_resource_limits_full() {
        let p = parse(&[
            "cclaw",
            "groups",
            "config",
            "set-resource-limits",
            "id1",
            "--cpus",
            "1.5",
            "--memory-mb",
            "512",
            "--pids-limit",
            "256",
        ]);
        assert_eq!(p.command, "groups.config.set-resource-limits");
        assert_eq!(
            p.args,
            json!({
                "id": "id1",
                "limits": {"cpus": "1.5", "memory_mb": 512u64, "pids_limit": 256u64}
            })
        );
    }

    #[test]
    fn groups_config_set_resource_limits_partial_cpus_only() {
        let p = parse(&[
            "cclaw",
            "groups",
            "config",
            "set-resource-limits",
            "id1",
            "--cpus",
            "2.0",
        ]);
        assert_eq!(p.command, "groups.config.set-resource-limits");
        assert_eq!(p.args, json!({"id": "id1", "limits": {"cpus": "2.0"}}));
    }

    #[test]
    fn groups_config_set_resource_limits_empty_clears() {
        let p = parse(&["cclaw", "groups", "config", "set-resource-limits", "id1"]);
        assert_eq!(p.command, "groups.config.set-resource-limits");
        assert_eq!(p.args, json!({"id": "id1", "limits": {}}));
    }

    // --- groups enable-coding / disable-coding -----------------------------

    #[test]
    fn groups_enable_coding_dispatches_set_coding_enabled_true() {
        let p = parse(&["cclaw", "groups", "enable-coding", "ag_1"]);
        assert_eq!(p.command, "groups.config.set-coding-enabled");
        assert_eq!(p.args, json!({"id": "ag_1", "enabled": true}));
    }

    #[test]
    fn groups_disable_coding_dispatches_set_coding_enabled_false() {
        let p = parse(&["cclaw", "groups", "disable-coding", "ag_1"]);
        assert_eq!(p.command, "groups.config.set-coding-enabled");
        assert_eq!(p.args, json!({"id": "ag_1", "enabled": false}));
    }

    // --- messaging-groups --------------------------------------------------

    #[test]
    fn messaging_groups_all() {
        assert_eq!(
            parse(&["cclaw", "messaging-groups", "list"]).command,
            "messaging-groups.list"
        );
        assert_eq!(
            parse(&["cclaw", "messaging-groups", "get", "mg_1"]).args,
            json!({"id":"mg_1"})
        );
        let p = parse(&[
            "cclaw",
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
            "cclaw",
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
            "cclaw",
            "messaging-groups",
            "update",
            "mg_1",
            "--name",
            "Renamed",
            "--is-group",
            "true",
        ]);
        assert_eq!(p.command, "messaging-groups.update");
        assert_eq!(
            p.args,
            json!({"id":"mg_1","name":"Renamed","is_group":true})
        );

        assert_eq!(
            parse(&["cclaw", "messaging-groups", "delete", "mg_1"]).command,
            "messaging-groups.delete"
        );
    }

    // --- wirings -----------------------------------------------------------

    #[test]
    fn wirings_create_minimal() {
        let p = parse(&[
            "cclaw", "wirings", "create", "--mg", "mg_1", "--ag", "ag_1", "--engage", "mention",
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
            "cclaw",
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
            "cclaw", "wirings", "create", "--mg", "m", "--ag", "a", "--engage", "pattern",
        ]);
        assert_eq!(p.args["engage"], "pattern");
    }

    #[test]
    fn wirings_session_mode_agent_shared() {
        let p = parse(&[
            "cclaw",
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
            "cclaw",
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
        assert_eq!(parse(&["cclaw", "wirings", "list"]).command, "wirings.list");
        assert_eq!(
            parse(&["cclaw", "wirings", "get", "w"]).args,
            json!({"id":"w"})
        );
        assert_eq!(
            parse(&["cclaw", "wirings", "delete", "w"]).command,
            "wirings.delete"
        );
    }

    #[test]
    fn wirings_update_partial() {
        let p = parse(&[
            "cclaw",
            "wirings",
            "update",
            "w",
            "--engage",
            "mention",
            "--priority",
            "7",
        ]);
        assert_eq!(p.command, "wirings.update");
        assert_eq!(p.args, json!({"id":"w","engage":"mention","priority":7}));
    }

    #[test]
    fn wirings_update_with_all_optionals() {
        let p = parse(&[
            "cclaw",
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
        assert_eq!(parse(&["cclaw", "users", "list"]).command, "users.list");
        assert_eq!(
            parse(&["cclaw", "users", "get", "u"]).args,
            json!({"id":"u"})
        );
        let p = parse(&[
            "cclaw",
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
        let p = parse(&["cclaw", "users", "update", "u", "--display-name", "Updated"]);
        assert_eq!(p.args, json!({"id":"u","display_name":"Updated"}));
    }

    // --- roles -------------------------------------------------------------

    #[test]
    fn roles_all() {
        assert_eq!(parse(&["cclaw", "roles", "list"]).command, "roles.list");
        let p = parse(&["cclaw", "roles", "grant", "u1", "admin"]);
        assert_eq!(p.command, "roles.grant");
        assert_eq!(p.args, json!({"user":"u1","role":"admin"}));
        let p = parse(&[
            "cclaw",
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
        let p = parse(&["cclaw", "roles", "revoke", "u1", "admin"]);
        assert_eq!(p.command, "roles.revoke");
    }

    // --- members -----------------------------------------------------------

    #[test]
    fn members_all() {
        let p = parse(&["cclaw", "members", "list", "ag1"]);
        assert_eq!(p.command, "members.list");
        assert_eq!(p.args, json!({"agent_group_id":"ag1"}));
        let p = parse(&["cclaw", "members", "add", "ag1", "u1"]);
        assert_eq!(p.command, "members.add");
        assert_eq!(p.args, json!({"agent_group_id":"ag1","user":"u1"}));
        let p = parse(&["cclaw", "members", "remove", "ag1", "u1"]);
        assert_eq!(p.command, "members.remove");
    }

    // --- destinations ------------------------------------------------------

    #[test]
    fn destinations_all() {
        let p = parse(&["cclaw", "destinations", "list", "ag1"]);
        assert_eq!(p.command, "destinations.list");
        assert_eq!(p.args, json!({"agent_group_id":"ag1"}));
        let p = parse(&[
            "cclaw",
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
            "cclaw",
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
        let p = parse(&["cclaw", "destinations", "remove", "ag1", "--name", "ops"]);
        assert_eq!(p.command, "destinations.remove");
        assert_eq!(p.args, json!({"agent_group_id":"ag1","name":"ops"}));
    }

    // --- sessions ----------------------------------------------------------

    #[test]
    fn sessions_all() {
        let p = parse(&["cclaw", "sessions", "list"]);
        assert_eq!(p.command, "sessions.list");
        assert_eq!(p.args, json!({}));
        let p = parse(&[
            "cclaw",
            "sessions",
            "list",
            "--agent-group",
            "ag1",
            "--status",
            "running",
        ]);
        assert_eq!(p.args, json!({"agent_group_id":"ag1","status":"running"}));
        let p = parse(&["cclaw", "sessions", "get", "s1"]);
        assert_eq!(p.command, "sessions.get");
    }

    #[test]
    fn sessions_delete_default_force_false() {
        let p = parse(&["cclaw", "sessions", "delete", "s1"]);
        assert_eq!(p.command, "sessions.delete");
        assert_eq!(p.args, json!({"id": "s1", "force": false}));
    }

    #[test]
    fn sessions_delete_with_force_flag() {
        let p = parse(&["cclaw", "sessions", "delete", "s1", "--force"]);
        assert_eq!(p.command, "sessions.delete");
        assert_eq!(p.args, json!({"id": "s1", "force": true}));
    }

    // --- user-dms / dropped-messages / approvals ---------------------------

    #[test]
    fn user_dms_list() {
        let p = parse(&["cclaw", "user-dms", "list"]);
        assert_eq!(p.command, "user-dms.list");
    }

    #[test]
    fn dropped_messages_list() {
        let p = parse(&["cclaw", "dropped-messages", "list"]);
        assert_eq!(p.command, "dropped-messages.list");
        assert_eq!(p.args, json!({}));
        let p = parse(&[
            "cclaw",
            "dropped-messages",
            "list",
            "--since",
            "2026-01-01T00:00:00Z",
        ]);
        assert_eq!(p.args, json!({"since":"2026-01-01T00:00:00Z"}));
    }

    #[test]
    fn approvals_all() {
        let p = parse(&["cclaw", "approvals", "list"]);
        assert_eq!(p.command, "approvals.list");
        let p = parse(&["cclaw", "approvals", "get", "appr_1"]);
        assert_eq!(p.command, "approvals.get");
        assert_eq!(p.args, json!({"id":"appr_1"}));
    }

    #[test]
    fn approvals_generic_approve_emits_approve_with_id() {
        let p = parse(&["cclaw", "approvals", "approve-id", "appr_2"]);
        assert_eq!(p.command, "approvals.approve");
        assert_eq!(p.args, json!({"id": "appr_2"}));
    }

    #[test]
    fn approvals_deny_emits_deny_with_id() {
        let p = parse(&["cclaw", "approvals", "deny", "appr_3"]);
        assert_eq!(p.command, "approvals.deny");
        assert_eq!(p.args, json!({"id": "appr_3"}));
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
            &["cclaw", "groups", "list"],
            &["cclaw", "groups", "get", "x"],
            &["cclaw", "groups", "create", "--folder", "f", "--name", "n"],
            &["cclaw", "groups", "update", "x"],
            &["cclaw", "groups", "delete", "x"],
            &["cclaw", "groups", "restart", "x"],
            &["cclaw", "groups", "config", "get", "x"],
            &["cclaw", "groups", "config", "update", "x", "--field", "k=v"],
            &[
                "cclaw",
                "groups",
                "config",
                "add-mcp-server",
                "x",
                "--config",
                "{}",
            ],
            &[
                "cclaw",
                "groups",
                "config",
                "remove-mcp-server",
                "x",
                "--name",
                "n",
            ],
            &[
                "cclaw",
                "groups",
                "config",
                "add-package",
                "x",
                "--apt",
                "p",
            ],
            &[
                "cclaw",
                "groups",
                "config",
                "remove-package",
                "x",
                "--npm",
                "p",
            ],
            &["cclaw", "groups", "config", "set-egress-allow", "x"],
            &["cclaw", "groups", "config", "set-resource-limits", "x"],
            &["cclaw", "groups", "enable-coding", "x"],
            &["cclaw", "groups", "disable-coding", "x"],
            &["cclaw", "messaging-groups", "list"],
            &["cclaw", "messaging-groups", "get", "x"],
            &[
                "cclaw",
                "messaging-groups",
                "create",
                "--channel-type",
                "t",
                "--platform-id",
                "p",
            ],
            &["cclaw", "messaging-groups", "update", "x"],
            &["cclaw", "messaging-groups", "delete", "x"],
            &["cclaw", "wirings", "list"],
            &["cclaw", "wirings", "get", "x"],
            &[
                "cclaw", "wirings", "create", "--mg", "m", "--ag", "a", "--engage", "mention",
            ],
            &["cclaw", "wirings", "update", "x"],
            &["cclaw", "wirings", "delete", "x"],
            &["cclaw", "users", "list"],
            &["cclaw", "users", "get", "x"],
            &["cclaw", "users", "create", "--identity", "ch:h"],
            &["cclaw", "users", "update", "x"],
            &["cclaw", "roles", "list"],
            &["cclaw", "roles", "grant", "u", "r"],
            &["cclaw", "roles", "revoke", "u", "r"],
            &["cclaw", "members", "list", "ag"],
            &["cclaw", "members", "add", "ag", "u"],
            &["cclaw", "members", "remove", "ag", "u"],
            &["cclaw", "destinations", "list", "ag"],
            &[
                "cclaw",
                "destinations",
                "add",
                "ag",
                "--name",
                "n",
                "--type",
                "channel",
            ],
            &["cclaw", "destinations", "remove", "ag", "--name", "n"],
            &["cclaw", "sessions", "list"],
            &["cclaw", "sessions", "get", "x"],
            &["cclaw", "sessions", "delete", "x"],
            &["cclaw", "user-dms", "list"],
            &["cclaw", "dropped-messages", "list"],
            &["cclaw", "dropped-messages", "outbound-list"],
            &[
                "cclaw",
                "dropped-messages",
                "replay",
                "00000000-0000-0000-0000-000000000000",
            ],
            &["cclaw", "db", "backup", "/tmp/copperclaw.db.bak"],
            &["cclaw", "db", "restore", "/tmp/copperclaw.db.bak"],
            &["cclaw", "mcp", "list-presets"],
            &[
                "cclaw",
                "mcp",
                "add",
                "postgres",
                "--agent-group-id",
                "00000000-0000-0000-0000-000000000000",
            ],
            &["cclaw", "approvals", "list"],
            &["cclaw", "approvals", "get", "x"],
            &[
                "cclaw",
                "approvals",
                "approve",
                "--channel",
                "telegram",
                "--identity",
                "user-42",
            ],
            &["cclaw", "approvals", "approve-id", "appr-1"],
            &["cclaw", "approvals", "deny", "appr-1"],
            &["cclaw", "audit", "list"],
            &["cclaw", "budgets", "list"],
            &[
                "cclaw",
                "budgets",
                "set",
                "--agent-group-id",
                "ag-1",
                "--daily-tokens",
                "10000",
                "--turns-per-minute",
                "5",
                "--turns-per-hour",
                "60",
            ],
            &["cclaw", "usage"],
            &["cclaw", "schema-version"],
            // Note: composite-only commands (`cclaw status`,
            // `cclaw health`, `cclaw quickstart`, `cclaw chat`,
            // `cclaw completions`) intentionally produce
            // `composite.*` marker commands that aren't in
            // ALL_COMMANDS. They're client-side fan-outs, not
            // wire calls, so they don't belong in the ALL_COMMANDS
            // contract.
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
        let emitted: std::collections::HashSet<String> =
            invocations.iter().map(|args| parse(args).command).collect();
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
        let cli = Cli::try_parse_from(["cclaw", "groups", "list"]).unwrap();
        assert_eq!(cli.to_call().command, "groups.list");
    }

    #[test]
    fn resolve_socket_prefers_explicit_flag() {
        let cli =
            Cli::try_parse_from(["cclaw", "--socket", "/tmp/x.sock", "groups", "list"]).unwrap();
        assert_eq!(
            cli.resolve_socket(),
            std::path::PathBuf::from("/tmp/x.sock")
        );
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
            std::path::PathBuf::from("/home/u/.local/share/copperclaw/data/cclaw.sock")
        );
    }

    #[test]
    fn user_socket_for_macos_uses_app_support() {
        let p = user_socket_for(std::path::Path::new("/Users/u"), "macos");
        assert_eq!(
            p,
            std::path::PathBuf::from(
                "/Users/u/Library/Application Support/copperclaw/data/cclaw.sock"
            )
        );
    }

    #[test]
    fn user_socket_for_other_os_falls_back_to_dot_dir() {
        let p = user_socket_for(std::path::Path::new("/h"), "freebsd");
        assert_eq!(
            p,
            std::path::PathBuf::from("/h/.copperclaw/data/cclaw.sock")
        );
    }

    // --- schema-version -------------------------------------------------------

    #[test]
    fn schema_version_produces_correct_call() {
        let p = parse(&["cclaw", "schema-version"]);
        assert_eq!(p.command, "schema.version");
        assert_eq!(p.args, json!({}));
    }

    #[test]
    fn schema_version_is_in_all_commands() {
        assert!(ALL_COMMANDS.contains(&"schema.version"));
    }

    // --- budgets rate-limit flags ------------------------------------------

    #[test]
    fn budgets_set_accepts_turns_per_minute() {
        let p = parse(&[
            "cclaw",
            "budgets",
            "set",
            "--agent-group-id",
            "ag-1",
            "--turns-per-minute",
            "10",
        ]);
        assert_eq!(p.command, "budgets.set");
        assert_eq!(p.args["agent_group_id"], "ag-1");
        assert_eq!(p.args["turns_per_minute"], 10);
        assert!(p.args.get("daily_tokens").is_none());
    }

    #[test]
    fn budgets_set_accepts_turns_per_hour() {
        let p = parse(&[
            "cclaw",
            "budgets",
            "set",
            "--agent-group-id",
            "ag-1",
            "--turns-per-hour",
            "120",
        ]);
        assert_eq!(p.command, "budgets.set");
        assert_eq!(p.args["turns_per_hour"], 120);
    }

    #[test]
    fn budgets_set_accepts_all_caps_together() {
        let p = parse(&[
            "cclaw",
            "budgets",
            "set",
            "--agent-group-id",
            "ag-1",
            "--daily-tokens",
            "50000",
            "--turns-per-minute",
            "5",
            "--turns-per-hour",
            "60",
        ]);
        assert_eq!(p.args["daily_tokens"], 50000);
        assert_eq!(p.args["turns_per_minute"], 5);
        assert_eq!(p.args["turns_per_hour"], 60);
    }

    #[test]
    fn budgets_set_zero_turns_per_minute_emits_zero() {
        let p = parse(&[
            "cclaw",
            "budgets",
            "set",
            "--agent-group-id",
            "ag-1",
            "--turns-per-minute",
            "0",
        ]);
        assert_eq!(p.args["turns_per_minute"], 0);
    }

    // --- chat autostart flag ----------------------------------------------

    #[test]
    fn chat_default_has_no_autostart_flag() {
        let p = parse(&["cclaw", "chat"]);
        assert_eq!(p.command, "composite.chat");
        // Without the flag the marker payload omits `no_autostart` so
        // the default (run-it) wins in `run_chat`.
        assert!(p.args.get("no_autostart").is_none());
    }

    #[test]
    fn chat_no_autostart_flag_propagates() {
        let p = parse(&["cclaw", "chat", "--no-autostart"]);
        assert_eq!(p.command, "composite.chat");
        assert_eq!(p.args["no_autostart"], json!(true));
    }

    #[test]
    fn chat_explicit_paths_carry_through() {
        let p = parse(&[
            "cclaw",
            "chat",
            "--fifo",
            "/tmp/foo.fifo",
            "--log",
            "/tmp/foo.log",
        ]);
        assert_eq!(p.args["fifo"], json!("/tmp/foo.fifo"));
        assert_eq!(p.args["log"], json!("/tmp/foo.log"));
    }
}
