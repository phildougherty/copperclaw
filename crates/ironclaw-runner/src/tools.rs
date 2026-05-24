//! `ToolContext` implementation that writes effects to `outbound.db`.
//!
//! The runner's [`RunnerToolCtx`] implements [`ironclaw_mcp::ToolContext`]
//! against the per-session `outbound.db` plus the session's `outbox/` dir
//! on disk. Each effect maps onto an insertion into `messages_out` (kind
//! `chat` for end-user messages, `system` for everything host-handled).
//!
//! # JSON shapes
//!
//! Host parsers can match on the following structure of system-kind outbound
//! message `content` blobs:
//!
//! ```json
//! { "edit":          { "seq": 7, "text": "..." } }
//! { "reaction":      { "seq": 7, "emoji": "..." } }
//! { "ask_user_question": { "id": "q_<uuid>", "title": "...", "options": [...], "to": {...} } }
//! { "send_card":          { "to": {...}, "card": {...} } }
//! { "create_agent":  { "name": "...", "instructions": "...", "channel": "..." } }
//! { "install_packages": { "apt": [...], "npm": [...], "reason": "..." } }
//! { "add_mcp_server":   { "name": "...", "transport": {...}, "reason": "..." } }
//! { "schedule":      { "op": "create" | "cancel" | "pause" | "resume" | "update", "payload": {...} } }
//! ```
//!
//! `SendFile` writes the bytes to `outbox/<msg_id>/<filename>` and emits a
//! `chat`-kind row whose `content` includes a `files` array pointing at the
//! filename.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Path inside the container where the host writes the per-session tasks
/// snapshot for the `list_tasks` MCP tool. Mirrors
/// `ironclaw_host::container_manager::TASKS_SNAPSHOT_FILENAME` under the
/// `/data` bind. Test-overridable via the env var below.
const TASKS_SNAPSHOT_DEFAULT_PATH: &str = "/data/tasks.json";
const TASKS_SNAPSHOT_ENV_OVERRIDE: &str = "IRONCLAW_TASKS_SNAPSHOT_FILE";

fn tasks_snapshot_path() -> PathBuf {
    std::env::var_os(TASKS_SNAPSHOT_ENV_OVERRIDE)
        .map_or_else(|| PathBuf::from(TASKS_SNAPSHOT_DEFAULT_PATH), PathBuf::from)
}

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_db::attachments::safe_attachment_name;
use ironclaw_db::tables::messages_out::{self, WriteOutbound};
use ironclaw_db::DbError;
use ironclaw_mcp::{
    AddMcpServerSpec, AddReactionSpec, AskUserQuestionSpec, CreateAgentSpec, EditMessageSpec,
    InstallSpec, OutboundToolEffect, Recipient, ScheduleSpec, SendCardSpec, SendFileSpec,
    SendMessageSpec, SubagentRequest, SubagentResult, TaskSummary, ToolContext, ToolEffectAck,
    ToolEntry, ToolError, UpdateTaskSpec,
};
use ironclaw_providers::AgentProvider;
use ironclaw_types::{Effort, MessageId, MessageKind};
use rusqlite::Connection;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::subagent::{run_inner_loop, SubagentDeps, SubagentInputs};

/// Shared, async-safe handle to the runner's `outbound.db` connection.
pub type SharedOutbound = Arc<Mutex<Connection>>;

/// Bundle of dependencies the [`RunnerToolCtx::spawn_subagent`] impl
/// needs in addition to the outbound-write set. Populated at runner
/// startup from [`crate::RunnerDeps`] and handed to the ctx via
/// [`RunnerToolCtx::with_subagent`].
///
/// Kept as a separate struct rather than inlined into `RunnerToolCtx`
/// so existing callers (most of the runner's tests) can construct a
/// context without any subagent wiring — `spawn_subagent` then falls
/// back to the trait default ("subagent not supported in this
/// context").
#[derive(Clone)]
pub struct SubagentRunnerDeps {
    /// Provider handle, shared with the parent's loop.
    pub provider: Arc<dyn AgentProvider>,
    /// Full in-process tool inventory. The subagent loop filters this
    /// down to the caller's allowlist; tools not on the allowlist
    /// surface as `is_error=true` `tool_result` blocks.
    pub tool_map: Arc<HashMap<String, Arc<ToolEntry>>>,
    /// Base system prompt (skills already inlined by the runner).
    pub system: String,
    /// Provider-native model id.
    pub model: String,
    /// Effort hint.
    pub effort: Effort,
    /// Per-turn `max_tokens` for the provider call.
    pub per_turn_max_tokens: u32,
    /// Sampling temperature, if any.
    pub temperature: Option<f32>,
    /// Display name for the assistant, if any.
    pub assistant_name: Option<String>,
    /// Per-LLM-call deadline applied around `provider.query()`.
    pub provider_deadline: Duration,
}

/// `ToolContext` implementation backed by the per-session `outbound.db` plus
/// the session's `outbox/` directory.
///
/// `list_tasks` always returns an empty `Vec` — task state lives on the host
/// side, so the runner can't enumerate it directly. Schedulers tooling is
/// surfaced to the host via `system`-kind outbound rows (see
/// [`OutboundToolEffect::ScheduleTask`] et al.) and the host writes any
/// resulting state into its own central DB.
///
/// `spawn_subagent` is wired through [`SubagentRunnerDeps`] when the
/// runner binary builds the ctx in `main.rs`; tests / minimal builds
/// can skip the wiring and rely on the trait default (returns
/// `ToolError::Context("subagent not supported in this context")`).
/// Routing copied from the inbound message that triggered the current
/// turn. The runner's main loop sets this on [`RunnerToolCtx`] before
/// driving each turn so that `send_message` / `send_file` calls with
/// `to: None` ("reply on the originating channel") actually carry the
/// channel routing on the resulting `messages_out` row. Without it the
/// row's `channel_type` / `platform_id` columns end up empty and the
/// host's delivery loop has nowhere to send the reply — the bug live-
/// caught when the model replied normally but the user saw silence.
#[derive(Debug, Clone, Default)]
pub struct OriginatingRouting {
    pub channel_type: Option<String>,
    pub platform_id: Option<String>,
    pub thread_id: Option<String>,
    pub in_reply_to: Option<MessageId>,
    /// Parent session id when this runner is a spawned child (host
    /// writes this through from `RunnerConfig::source_session_id`).
    /// When set, `apply_send_message` / `apply_send_file` default
    /// `to: None` to "report up to the parent" instead of the
    /// inherited messaging-group channel.
    pub source_session_id: Option<ironclaw_types::SessionId>,
}

pub struct RunnerToolCtx {
    outbound: SharedOutbound,
    outbox_root: PathBuf,
    subagent: Option<SubagentRunnerDeps>,
    /// Re-entrancy guard so a subagent calling `explore` is detected
    /// and refused, even before the explore tool's own nested-check
    /// fires. Bumped on entry to `spawn_subagent` and checked on
    /// re-entry.
    in_subagent: Arc<std::sync::atomic::AtomicBool>,
    /// Routing of the inbound currently being processed. Set by
    /// `run_loop` before `drive_turn`, cleared after. Read by
    /// `apply_send_message` / `apply_send_file` to fill the outbound
    /// row's `channel_type` / `platform_id` columns when the model
    /// didn't pass an explicit `to` recipient.
    originating: Arc<std::sync::Mutex<OriginatingRouting>>,
    /// Parent session id, for child agents created via `create_agent`.
    /// Immutable for the lifetime of the runner (it's a property of
    /// the session, not the inbound). When set, `apply_send_message`
    /// routes `to: None` outbound rows up to the parent's inbound
    /// instead of dumping into the inherited messaging-group channel.
    source_session_id: Option<ironclaw_types::SessionId>,
    /// When true, the runner emits a brief `[tool] tool_name` chat
    /// message to the originating channel at the start of every
    /// "visible" tool call (`shell` / `web_search` / `write_file` /
    /// `explore` / etc.). Off by default; enabled via the
    /// `IRONCLAW_TOOL_BREADCRUMBS` env var.
    breadcrumbs_enabled: bool,
}

impl RunnerToolCtx {
    /// Build a fresh context around the given outbound DB handle and outbox
    /// directory. The resulting context cannot spawn subagents — call
    /// [`Self::with_subagent`] to wire that in.
    pub fn new(outbound: SharedOutbound, outbox_root: impl Into<PathBuf>) -> Self {
        Self {
            outbound,
            outbox_root: outbox_root.into(),
            subagent: None,
            in_subagent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            originating: Arc::new(std::sync::Mutex::new(OriginatingRouting::default())),
            source_session_id: None,
            breadcrumbs_enabled: false,
        }
    }

    /// Enable `[tool] name` chat breadcrumbs for visible tools. Reads
    /// the `IRONCLAW_TOOL_BREADCRUMBS` env var (1/true/yes/on = on);
    /// any other value keeps it disabled. Called at runner startup
    /// from `main.rs`.
    #[must_use]
    pub fn with_breadcrumbs_from_env(mut self) -> Self {
        self.breadcrumbs_enabled = matches!(
            std::env::var("IRONCLAW_TOOL_BREADCRUMBS").ok().as_deref(),
            Some("1" | "true" | "yes" | "on")
        );
        self
    }

    /// Emit a brief `[tool_name]` chat message via the originating
    /// channel routing so the user sees what the agent is working on.
    /// No-op when breadcrumbs are disabled, the tool isn't on the
    /// "visible" allowlist, or there's no channel routing to send to
    /// (e.g. agent-to-agent rows where the destination is the parent).
    ///
    /// Errors are swallowed: breadcrumbs are best-effort UX
    /// observability, NOT load-bearing — a failed write shouldn't
    /// abort the turn or surface to the user.
    pub async fn emit_breadcrumb(&self, tool_name: &str, input: Option<&serde_json::Value>) {
        if !self.breadcrumbs_enabled {
            return;
        }
        if !is_visible_breadcrumb_tool(tool_name) {
            return;
        }
        let origin = self.current_originating();
        // Only emit when there's a real user channel to send to.
        // Child agents that route to parent via Agent-kind won't
        // benefit from a breadcrumb (the parent doesn't need to be
        // told what its own children are doing).
        if origin.channel_type.is_none() || origin.platform_id.is_none() {
            return;
        }
        // Add per-tool detail when we have the input — shell command,
        // search query, file path, etc. — so the user can see at a
        // glance what the agent is doing, not just which tool it's
        // running. Format: `[name] detail` when we have detail,
        // `[name]` when we don't (e.g. detail extraction failed or
        // the tool has no useful arg to surface).
        let detail = input.and_then(|v| breadcrumb_detail(tool_name, v));
        let text = match detail {
            Some(d) => format!("[{tool_name}] {d}"),
            None => format!("[{tool_name}]"),
        };
        let body = serde_json::json!({ "text": text });
        let routed = OutboundRouting {
            kind: MessageKind::Chat,
            body_to: None,
            channel_type: origin.channel_type.clone(),
            platform_id: origin.platform_id.clone(),
            thread_id: origin.thread_id.clone(),
            in_reply_to: None,
        };
        let mut guard = self.outbound.lock().await;
        let conn: &mut Connection = &mut guard;
        let _ = insert_outbound_row(conn, MessageId::new(), body, &routed);
    }

    /// Stash the parent session id on the context so `apply_send_message`
    /// can route the child's default `to: None` calls to the parent's
    /// inbound instead of the inherited messaging-group channel.
    #[must_use]
    pub fn with_source_session_id(mut self, parent: ironclaw_types::SessionId) -> Self {
        self.source_session_id = Some(parent);
        self
    }

    /// Accessor for the parent session id (None when this is a root session).
    #[must_use]
    pub fn source_session_id(&self) -> Option<ironclaw_types::SessionId> {
        self.source_session_id
    }

    /// Set the originating-inbound routing so subsequent
    /// `send_message` / `send_file` effects on this ctx auto-fill
    /// the outbound row's channel columns. Called by `run_loop`
    /// at the start of each turn.
    pub fn set_originating(&self, routing: OriginatingRouting) {
        if let Ok(mut guard) = self.originating.lock() {
            *guard = routing;
        }
    }

    /// Clear the originating-inbound routing. Called by `run_loop`
    /// after the turn finalises so a subsequent emit (e.g. the
    /// terminal-failure apology written by the host-side
    /// `emit_terminal_failure_apologies`) doesn't accidentally
    /// pick up routing from a previous turn.
    pub fn clear_originating(&self) {
        if let Ok(mut guard) = self.originating.lock() {
            *guard = OriginatingRouting::default();
        }
    }

    fn current_originating(&self) -> OriginatingRouting {
        self.originating
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    /// Attach the subagent dependencies so the `explore` tool's
    /// `spawn_subagent` path can run.
    #[must_use]
    pub fn with_subagent(mut self, deps: SubagentRunnerDeps) -> Self {
        self.subagent = Some(deps);
        self
    }

    /// Snapshot accessor for tests.
    pub fn outbox_root(&self) -> &Path {
        &self.outbox_root
    }
}

#[async_trait]
impl ToolContext for RunnerToolCtx {
    async fn emit_outbound(
        &self,
        effect: OutboundToolEffect,
    ) -> Result<ToolEffectAck, ToolError> {
        let outbox = self.outbox_root.clone();
        let origin = self.current_originating();
        // Take the lock for the entire DB write.
        let mut guard = self.outbound.lock().await;
        let conn: &mut Connection = &mut guard;
        let ack = apply_effect(conn, &outbox, effect, &origin).map_err(to_tool_error)?;
        Ok(ack)
    }

    async fn list_tasks(&self) -> Result<Vec<TaskSummary>, ToolError> {
        // Task state lives on the host; the runner can't reach the
        // central DB from inside its container. Read the snapshot the
        // host writes into the session dir on every spawn + tick. NotFound
        // means the host hasn't written one yet (fresh deployment / first
        // tick) — return an empty list rather than erroring so the agent
        // sees "no tasks" rather than a tool failure.
        let path = tasks_snapshot_path();
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(ToolError::Internal(format!(
                    "list_tasks: read {}: {err}",
                    path.display()
                )));
            }
        };
        let tasks: Vec<TaskSummary> = serde_json::from_slice(&bytes).map_err(|err| {
            ToolError::Internal(format!(
                "list_tasks: parse {}: {err}",
                path.display()
            ))
        })?;
        Ok(tasks)
    }

    fn set_originating(
        &self,
        channel_type: Option<&str>,
        platform_id: Option<&str>,
        thread_id: Option<&str>,
        in_reply_to: Option<&str>,
    ) {
        let routing = OriginatingRouting {
            channel_type: channel_type.map(str::to_string),
            platform_id: platform_id.map(str::to_string),
            thread_id: thread_id.map(str::to_string),
            in_reply_to: in_reply_to.and_then(|s| {
                Uuid::parse_str(s).ok().map(MessageId::from)
            }),
            source_session_id: self.source_session_id,
        };
        Self::set_originating(self, routing);
    }

    fn clear_originating(&self) {
        Self::clear_originating(self);
    }

    async fn emit_breadcrumb(&self, tool_name: &str, input: Option<&serde_json::Value>) {
        Self::emit_breadcrumb(self, tool_name, input).await;
    }

    async fn spawn_subagent(
        &self,
        req: SubagentRequest,
    ) -> Result<SubagentResult, ToolError> {
        let Some(deps) = self.subagent.as_ref() else {
            return Err(ToolError::Context(
                "subagent deps not wired into RunnerToolCtx".into(),
            ));
        };
        // Hard-deny nested calls (subagent → subagent). The flag is
        // also threaded into the inner explore tool by re-entering
        // the context with `nested = true` set on the request, but
        // doing the check here too means we still refuse even if a
        // future caller forgets to set it.
        if req.nested
            || self
                .in_subagent
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(ToolError::Validation(
                "nested `explore` calls are not allowed".into(),
            ));
        }
        self.in_subagent
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Build a fresh ctx for the subagent. We hand it `self` so
        // the model can still emit outbound effects (if the
        // allowlist permits any) without recursing into another
        // subagent — the `in_subagent` flag above blocks that
        // explicitly.
        //
        // The subagent's `ToolContext` is currently the same
        // `RunnerToolCtx` the parent uses. That means a subagent
        // tool call to `send_message` would actually post a message;
        // the `tools_allowed` allowlist (defaults to read-only) is
        // what prevents that. The two are intentionally redundant.
        let provider = deps.provider.clone();
        // The subagent inherits the parent's originating routing AND
        // `source_session_id` so that any allowlisted message-emitting
        // tool (operator-widened tools_allowed) lands its row through
        // the same routing rules as a tool call from the parent's main
        // turn — same channel for root sessions, same parent for
        // child agents. Without this the subagent would emit rows with
        // empty channel routing and `MessageKind::Chat`, which the
        // delivery loop rejects with `NoRoute`.
        let inherited_origin = self.current_originating();
        let tool_ctx: Arc<dyn ToolContext> = Arc::new(SubagentCtxAdapter {
            inner: self.outbound.clone(),
            outbox_root: self.outbox_root.clone(),
            origin: inherited_origin,
        });
        let sub_deps = SubagentDeps {
            provider: &provider,
            tool_ctx: &tool_ctx,
            tool_map: &deps.tool_map,
            system: &deps.system,
            model: &deps.model,
            effort: deps.effort,
            per_turn_max_tokens: deps.per_turn_max_tokens,
            temperature: deps.temperature,
            assistant_name: deps.assistant_name.as_deref(),
            provider_deadline: deps.provider_deadline,
        };
        let inputs = SubagentInputs {
            task: req.task,
            tools_allowed: req.tools_allowed,
            max_turns: req.max_turns,
            max_input_tokens: req.max_tokens,
            wall_clock: Duration::from_secs(ironclaw_mcp::SUBAGENT_WALL_CLOCK_SECS),
        };
        let result = run_inner_loop(&sub_deps, inputs).await;
        self.in_subagent
            .store(false, std::sync::atomic::Ordering::Relaxed);
        result.map_err(|err| ToolError::Context(format!("subagent provider error: {err}")))
    }
}

/// A `ToolContext` adapter the subagent loop hands to its tool
/// handlers. Forwards outbound effects to the parent's outbound DB but
/// blocks `spawn_subagent` (no nested explore) and pretends task
/// listing is empty (same as the runner's main path).
///
/// We don't reuse `RunnerToolCtx` here because that would pull the
/// `subagent` deps along — letting a subagent's tool emit
/// `OutboundToolEffect` rows into the same DB is fine; letting it
/// recurse into another full subagent loop is the footgun we are
/// preventing. Distinct type, distinct trait impl, no surprise.
struct SubagentCtxAdapter {
    inner: SharedOutbound,
    outbox_root: PathBuf,
    /// Snapshot of the parent's routing at the time the subagent was
    /// spawned. Inherited so subagent-emitted rows route through the
    /// same MG / parent path as the parent's own emissions. Without
    /// this every subagent emission landed with empty channel columns
    /// and hit `NoRoute`.
    origin: OriginatingRouting,
}

#[async_trait]
impl ToolContext for SubagentCtxAdapter {
    async fn emit_outbound(
        &self,
        effect: OutboundToolEffect,
    ) -> Result<ToolEffectAck, ToolError> {
        let outbox = self.outbox_root.clone();
        let mut guard = self.inner.lock().await;
        let conn: &mut Connection = &mut guard;
        apply_effect(conn, &outbox, effect, &self.origin).map_err(to_tool_error)
    }
    async fn list_tasks(&self) -> Result<Vec<TaskSummary>, ToolError> {
        Ok(Vec::new())
    }
    async fn spawn_subagent(
        &self,
        _req: SubagentRequest,
    ) -> Result<SubagentResult, ToolError> {
        Err(ToolError::Validation(
            "nested `explore` calls are not allowed".into(),
        ))
    }
}

fn to_tool_error(err: ToolApplyError) -> ToolError {
    match err {
        ToolApplyError::Db(e) => ToolError::Context(e.to_string()),
        ToolApplyError::Io(e) => ToolError::Context(e.to_string()),
        ToolApplyError::Validation(s) => ToolError::Validation(s),
    }
}

/// Internal error type used while applying a tool effect to the outbound DB.
#[derive(Debug)]
enum ToolApplyError {
    Db(DbError),
    Io(std::io::Error),
    Validation(String),
}

impl From<DbError> for ToolApplyError {
    fn from(value: DbError) -> Self {
        Self::Db(value)
    }
}

impl From<std::io::Error> for ToolApplyError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn apply_effect(
    conn: &mut Connection,
    outbox_root: &Path,
    effect: OutboundToolEffect,
    origin: &OriginatingRouting,
) -> Result<ToolEffectAck, ToolApplyError> {
    match effect {
        OutboundToolEffect::SendMessage(spec) => apply_send_message(conn, spec, origin),
        OutboundToolEffect::SendFile(spec) => apply_send_file(conn, outbox_root, spec, origin),
        OutboundToolEffect::EditMessage(spec) => apply_edit_message(conn, spec),
        OutboundToolEffect::AddReaction(spec) => apply_add_reaction(conn, spec),
        OutboundToolEffect::AskUserQuestion(spec) => apply_ask_question(conn, spec),
        OutboundToolEffect::SendCard(spec) => apply_send_card(conn, spec),
        OutboundToolEffect::CreateAgent(spec) => apply_create_agent(conn, spec),
        OutboundToolEffect::InstallPackages(spec) => apply_install_packages(conn, spec),
        OutboundToolEffect::AddMcpServer(spec) => apply_add_mcp_server(conn, spec),
        OutboundToolEffect::ScheduleTask(spec) => apply_schedule_create(conn, spec),
        OutboundToolEffect::ListTasks => apply_schedule_list(conn),
        OutboundToolEffect::CancelTask { id } => apply_schedule_simple(conn, "cancel", &id),
        OutboundToolEffect::PauseTask { id } => apply_schedule_simple(conn, "pause", &id),
        OutboundToolEffect::ResumeTask { id } => apply_schedule_simple(conn, "resume", &id),
        OutboundToolEffect::UpdateTask(spec) => apply_schedule_update(conn, spec),
    }
}

/// Strip model-emitted reasoning blocks from outbound chat text.
///
/// Some models (notably Haiku 4.5 in our live testing) ignore the
/// "private reasoning" convention of the Anthropic API's
/// `thinking` content blocks and instead emit literal
/// `<thinking>...</thinking>` markup as part of regular text output.
/// That markup leaks to end users — they see the model "talking to
/// itself" in the chat. Strip it here, before the row hits
/// `messages_out`, so no future-channel-adapter has to worry about
/// the same payload.
///
/// Conservative: only strip closed `<thinking>...</thinking>` pairs.
/// An unterminated `<thinking>` without a closing tag is preserved
/// verbatim so we never accidentally swallow large chunks of
/// legitimate prose. Case-insensitive open/close tags, multi-line
/// content, leaves the surrounding text intact.
/// Allow-list of tool names that get a chat breadcrumb when
/// `IRONCLAW_TOOL_BREADCRUMBS=1`. Limited to tools that (a) take
/// long enough that the user wants to know about them and (b)
/// aren't already user-visible via their own outbound emission.
fn is_visible_breadcrumb_tool(name: &str) -> bool {
    matches!(
        name,
        "shell"
            | "web_search"
            | "web_fetch"
            | "explore"
            | "read_file"
            | "write_file"
            | "edit_file"
            | "grep"
            | "glob"
            | "create_agent"
            | "install_packages"
            | "add_mcp_server"
    )
}

/// Per-tool detail extractor: given a tool name and the model's input
/// JSON, return a short string (≤80 chars) for the user-visible
/// breadcrumb. Returns `None` when the tool has no useful arg to
/// surface or the input doesn't match the expected shape — caller
/// falls back to just `[tool_name]`.
///
/// Caps are deliberately tight: the breadcrumb is a UX cue, not the
/// full request. Long commands / paths / queries get truncated with
/// an ellipsis so the chat line stays readable on mobile.
fn breadcrumb_detail(name: &str, input: &serde_json::Value) -> Option<String> {
    const MAX_LEN: usize = 80;
    let field = |key: &str| -> Option<String> {
        input.get(key).and_then(|v| v.as_str()).map(str::to_owned)
    };
    let raw = match name {
        "shell" => field("command"),
        "web_search" | "explore" => field("query"),
        "web_fetch" => field("url"),
        "read_file" | "write_file" | "edit_file" => field("path"),
        "grep" | "glob" => field("pattern"),
        "create_agent" => field("name").or_else(|| field("prompt")),
        "add_mcp_server" => field("name"),
        "install_packages" => {
            // Two parallel arrays in this tool's input. Show the names
            // joined with commas (apt first, then npm).
            let mut parts: Vec<String> = Vec::new();
            for key in ["apt", "npm"] {
                if let Some(arr) = input.get(key).and_then(|v| v.as_array()) {
                    for v in arr {
                        if let Some(s) = v.as_str() {
                            parts.push(s.to_owned());
                        }
                    }
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(", "))
            }
        }
        _ => None,
    }?;
    // Collapse any internal newlines so the breadcrumb is one line;
    // the model may emit multi-line shell commands and the chat
    // adapter treats `\n` as message boundaries on some channels.
    let collapsed = raw.replace(['\n', '\r'], " ");
    if collapsed.chars().count() <= MAX_LEN {
        Some(collapsed)
    } else {
        let truncated: String = collapsed.chars().take(MAX_LEN.saturating_sub(1)).collect();
        Some(format!("{truncated}…"))
    }
}

fn strip_reasoning_blocks(text: &str) -> String {
    // Hand-rolled scan instead of pulling in `regex` just for this.
    // Looking for `<thinking>` followed by anything up to `</thinking>`,
    // case-insensitive on the tag names only.
    const OPEN: &str = "<thinking>";
    const CLOSE: &str = "</thinking>";
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let lower = text.to_ascii_lowercase();
    while cursor < text.len() {
        let Some(open_off) = lower[cursor..].find(OPEN) else {
            out.push_str(&text[cursor..]);
            break;
        };
        let open_abs = cursor + open_off;
        let after_open = open_abs + OPEN.len();
        let Some(close_off) = lower[after_open..].find(CLOSE) else {
            // No closing tag — preserve the rest as-is.
            out.push_str(&text[cursor..]);
            break;
        };
        let close_abs = after_open + close_off;
        let after_close = close_abs + CLOSE.len();
        // Emit everything before the open tag; skip the open..close span.
        out.push_str(&text[cursor..open_abs]);
        cursor = after_close;
    }
    // Collapse blank-line runs left behind by the strip so we don't
    // ship messages that start with multiple empty lines where the
    // reasoning used to be.
    out.trim_start_matches([' ', '\n', '\r'])
        .replace("\n\n\n", "\n\n")
}

#[allow(clippy::needless_pass_by_value)]
fn apply_send_message(
    conn: &mut Connection,
    spec: SendMessageSpec,
    origin: &OriginatingRouting,
) -> Result<ToolEffectAck, ToolApplyError> {
    let SendMessageSpec { to, text } = spec;
    let text = strip_reasoning_blocks(&text);
    let routed = resolve_outbound_routing(to, origin);
    let mut body = serde_json::Map::new();
    body.insert("text".into(), serde_json::Value::String(text));
    if let Some(r) = &routed.body_to {
        // `Recipient` is a tagged enum with simple owned fields — its
        // `Serialize` impl cannot fail. `.expect` makes the assumption
        // explicit so a future regression on `Recipient` surfaces
        // loudly instead of silently producing `to: null`.
        body.insert(
            "to".into(),
            serde_json::to_value(r).expect("Recipient is always serialisable"),
        );
    }
    // Agent-kind rows go to `agent_dispatch` which writes into the
    // target session's inbound. Carry the inbound's `thread_id` into
    // the body so the dispatcher can preserve thread context on the
    // parent's inbound row.
    if matches!(routed.kind, MessageKind::Agent) {
        if let Some(thread) = &routed.thread_id {
            body.insert("thread_id".into(), serde_json::Value::String(thread.clone()));
        }
    }
    let seq = insert_outbound_row(conn, MessageId::new(), serde_json::Value::Object(body), &routed)?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_send_file(
    conn: &mut Connection,
    outbox_root: &Path,
    spec: SendFileSpec,
    origin: &OriginatingRouting,
) -> Result<ToolEffectAck, ToolApplyError> {
    let SendFileSpec {
        to,
        filename,
        data,
        text,
    } = spec;
    let text = text.map(|t| strip_reasoning_blocks(&t));
    safe_attachment_name(&filename)
        .map_err(|e| ToolApplyError::Validation(e.to_string()))?;
    let msg_id = MessageId::new();
    let msg_id_str = msg_id.as_uuid().to_string();
    let target_dir = outbox_root.join(&msg_id_str);
    std::fs::create_dir_all(&target_dir)?;
    let target = target_dir.join(&filename);
    std::fs::write(&target, &data)?;

    let mut routed = resolve_outbound_routing(to, origin);
    // `send_file` cannot use the Agent-kind dispatch path: the
    // `agent_dispatch` handler only forwards the body's text into the
    // parent's inbound — it has no mechanism to copy the file bytes
    // from the child's outbox to the parent's session. If routing
    // resolved to Agent (e.g. a child agent's default `send_file(to:
    // None)`), force-fall-back to Chat with the inherited channel
    // routing so the bytes actually reach the user's channel. The
    // tradeoff: files always go through the inherited MG, never up to
    // the parent's inbound. Document via the changelog; long-term
    // answer is a real cross-session attachment-relay in `agent_dispatch`.
    if matches!(routed.kind, MessageKind::Agent) {
        routed.kind = MessageKind::Chat;
        // Drop the synthesised parent-as-recipient — there's no
        // useful body.to for a Chat-kind row that goes through the
        // inherited MG.
        routed.body_to = None;
        // Restore in_reply_to for the chat path (resolve_outbound_routing
        // elided it for Agent-kind rows).
        routed.in_reply_to = origin.in_reply_to;
    }
    let mut body = serde_json::Map::new();
    if let Some(t) = text {
        body.insert("text".into(), serde_json::Value::String(t));
    }
    if let Some(r) = &routed.body_to {
        body.insert(
            "to".into(),
            serde_json::to_value(r).expect("Recipient is always serialisable"),
        );
    }
    body.insert(
        "files".into(),
        serde_json::json!([{ "filename": filename }]),
    );
    let seq = insert_outbound_row(
        conn,
        msg_id,
        serde_json::Value::Object(body),
        &routed,
    )?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_edit_message(
    conn: &mut Connection,
    spec: EditMessageSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "edit": { "seq": spec.message_seq, "text": spec.text }
    });
    let seq = insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_add_reaction(
    conn: &mut Connection,
    spec: AddReactionSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "reaction": { "seq": spec.message_seq, "emoji": spec.emoji }
    });
    let seq = insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_ask_question(
    conn: &mut Connection,
    spec: AskUserQuestionSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let qid = format!("q_{}", Uuid::new_v4());
    let mut q = serde_json::Map::new();
    q.insert("id".into(), serde_json::Value::String(qid.clone()));
    q.insert("title".into(), serde_json::Value::String(spec.title));
    q.insert(
        "options".into(),
        serde_json::Value::Array(
            spec.options
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    if let Some(t) = spec.to {
        q.insert("to".into(), serde_json::to_value(t).unwrap_or_default());
    }
    // Action name must match what `InteractiveModule::install` registers
    // (`ask_user_question`). Until this round it was `"ask_question"`,
    // which fell through to "no handler; skipping" — the user never saw
    // the question card.
    let payload = serde_json::json!({ "ask_user_question": q });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Question { id: qid })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_send_card(
    conn: &mut Connection,
    spec: SendCardSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let mut body = serde_json::Map::new();
    if let Some(t) = spec.to {
        body.insert("to".into(), serde_json::to_value(t).unwrap_or_default());
    }
    body.insert("card".into(), spec.card);
    // Action name must match what `InteractiveModule::install` registers
    // (`send_card`). Previously emitted as `"card"` which fell through.
    let payload = serde_json::json!({ "send_card": body });
    let seq = insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Message { seq })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_create_agent(
    conn: &mut Connection,
    spec: CreateAgentSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "create_agent": {
            "name": spec.name,
            "instructions": spec.instructions,
            "channel": spec.channel,
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    // The host assigns the session id; we surface a synthetic placeholder so
    // the calling agent knows the request was queued.
    Ok(ToolEffectAck::Accepted)
}

#[allow(clippy::needless_pass_by_value)]
fn apply_install_packages(
    conn: &mut Connection,
    spec: InstallSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "install_packages": {
            "apt": spec.apt,
            "npm": spec.npm,
            "reason": spec.reason,
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

#[allow(clippy::needless_pass_by_value)]
fn apply_add_mcp_server(
    conn: &mut Connection,
    spec: AddMcpServerSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "add_mcp_server": {
            "name": spec.name,
            "transport": spec.transport,
            "reason": spec.reason,
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

#[allow(clippy::needless_pass_by_value)]
fn apply_schedule_create(
    conn: &mut Connection,
    spec: ScheduleSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": {
            "op": "create",
            "payload": {
                "name": spec.name,
                "when": spec.when,
                "prompt": spec.prompt,
                "recurrence": spec.recurrence,
            }
        }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

fn apply_schedule_list(conn: &mut Connection) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": { "op": "list", "payload": {} }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Accepted)
}

fn apply_schedule_simple(
    conn: &mut Connection,
    op: &str,
    id: &str,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": { "op": op, "payload": { "id": id } }
    });
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Task { id: id.to_string() })
}

#[allow(clippy::needless_pass_by_value)]
fn apply_schedule_update(
    conn: &mut Connection,
    spec: UpdateTaskSpec,
) -> Result<ToolEffectAck, ToolApplyError> {
    let payload = serde_json::json!({
        "schedule": {
            "op": "update",
            "payload": {
                "id": spec.id,
                "prompt": spec.prompt,
                "when": spec.when,
                "recurrence": spec.recurrence,
            }
        }
    });
    let id_for_ack = spec.id.clone();
    insert_row(conn, MessageKind::System, payload)?;
    Ok(ToolEffectAck::Task { id: id_for_ack })
}

fn insert_row(
    conn: &mut Connection,
    kind: MessageKind,
    content: serde_json::Value,
) -> Result<i64, ToolApplyError> {
    let id = MessageId::new();
    insert_row_with_id(conn, id, kind, content)
}

fn insert_row_with_id(
    conn: &mut Connection,
    id: MessageId,
    kind: MessageKind,
    content: serde_json::Value,
) -> Result<i64, ToolApplyError> {
    let row = WriteOutbound {
        id,
        in_reply_to: None,
        timestamp: Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        content,
    };
    let seq = messages_out::insert(conn, &row)?;
    Ok(seq)
}

/// Routing decision for an outbound `send_message` / `send_file` row.
/// Combines the explicit `to:` recipient (if any) with the originating
/// inbound's channel routing and the runner's `source_session_id`
/// (parent agent for child sessions) to pick the right `MessageKind`
/// and the right columns on the outbound row.
struct OutboundRouting {
    kind: MessageKind,
    /// The `Recipient` form that gets serialised into the row body's
    /// `to` field. Some when the caller passed an explicit `to:` or
    /// when we synthesised one from `source_session_id`. None when
    /// routing is implicit (no parent + no explicit `to`) — the
    /// delivery loop falls back to the channel columns.
    body_to: Option<Recipient>,
    /// Channel routing for `MessageKind::Chat` rows. Pulled from the
    /// originating inbound. Ignored for `MessageKind::Agent` rows.
    channel_type: Option<String>,
    platform_id: Option<String>,
    thread_id: Option<String>,
    in_reply_to: Option<MessageId>,
}

/// Decide where this outbound row should land.
///
/// Rules (in order):
/// 1. Caller passed `to: Recipient::Agent { session_id }` → kind Agent
///    addressed to that session. Channel columns on the row are elided
///    (Agent-kind dispatches via the body's `to`, not the row's
///    channel routing).
/// 2. Caller passed `to: Recipient::Channel { .. }` → kind Chat. The
///    explicit recipient is preserved in the body for any downstream
///    consumer that wants to inspect the override. Channel routing on
///    the row stays inherited from the originating inbound: today's
///    delivery loop doesn't parse arbitrary channel id strings on its
///    own, so the row inherits routing for now.
/// 3. Caller passed no `to:` AND the runner has a `source_session_id`
///    AND the originating inbound had no channel routing (i.e. this is
///    truly an internal child→parent reply, not a direct user message
///    to a child session via inherited MG) → kind Agent, synthesise
///    the recipient pointing at the parent.
/// 4. Otherwise → kind Chat, channel routing from the inbound. Covers
///    root-session "reply to user" AND the edge case where the child
///    session has been wired directly to a user channel and a user
///    message landed on it (per-thread wirings, operator-added wiring
///    pointing at the child's `agent_group`). In those cases the user
///    expects a reply, not silent siphoning up to the parent.
fn resolve_outbound_routing(
    to: Option<Recipient>,
    origin: &OriginatingRouting,
) -> OutboundRouting {
    use ironclaw_mcp::Recipient as R;
    let inbound_came_from_parent = origin.channel_type.is_none()
        && origin.platform_id.is_none()
        && origin.source_session_id.is_some();
    let kind = match &to {
        Some(R::Agent { .. }) => MessageKind::Agent,
        Some(_) => MessageKind::Chat,
        None => {
            if inbound_came_from_parent {
                MessageKind::Agent
            } else {
                MessageKind::Chat
            }
        }
    };
    let body_to = match to {
        Some(r) => Some(r),
        None if inbound_came_from_parent => origin
            .source_session_id
            .map(|sid| R::Agent { session_id: sid.as_uuid().to_string() }),
        None => None,
    };
    // For Agent-kind rows, the dispatcher receives channel routing
    // through the body's `to` payload (and we propagate thread_id into
    // the body so the parent's inbound row can carry it). For Chat-kind
    // rows the row's channel columns drive delivery.
    let inherit_thread = matches!(kind, MessageKind::Agent | MessageKind::Chat);
    let _ = inherit_thread; // suppressed; reads as documentation here.
    OutboundRouting {
        kind,
        body_to,
        channel_type: origin.channel_type.clone(),
        platform_id: origin.platform_id.clone(),
        thread_id: origin.thread_id.clone(),
        // Agent-kind rows MUST NOT carry an `in_reply_to` pointing at
        // the originating session's `messages_in.id` — that id is
        // session-local and would be a dangling reference inside the
        // target's session. Chat-kind rows keep the back-reference for
        // the apology-lookup / telemetry threading paths.
        in_reply_to: if matches!(kind, MessageKind::Agent) {
            None
        } else {
            origin.in_reply_to
        },
    }
}

/// Insert an outbound chat / agent row honouring [`OutboundRouting`].
/// Replaces the older `insert_chat_row` which only knew about chat-kind
/// rows backed by the inbound channel routing.
fn insert_outbound_row(
    conn: &mut Connection,
    id: MessageId,
    content: serde_json::Value,
    routed: &OutboundRouting,
) -> Result<i64, ToolApplyError> {
    let row = WriteOutbound {
        id,
        in_reply_to: routed.in_reply_to,
        timestamp: Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: routed.kind,
        // Agent-kind rows don't need channel routing on the row
        // itself — the delivery loop dispatches via the `Recipient`
        // in the body. Keeping them None makes that intent explicit.
        platform_id: if matches!(routed.kind, MessageKind::Agent) {
            None
        } else {
            routed.platform_id.clone()
        },
        channel_type: if matches!(routed.kind, MessageKind::Agent) {
            None
        } else {
            routed
                .channel_type
                .as_ref()
                .map(|s| ironclaw_types::ChannelType::new(s.as_str()))
        },
        thread_id: if matches!(routed.kind, MessageKind::Agent) {
            None
        } else {
            routed.thread_id.clone()
        },
        content,
    };
    let seq = messages_out::insert(conn, &row)?;
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::session::{open_outbound, SessionPaths};
    use ironclaw_mcp::Recipient;
    use ironclaw_types::{AgentGroupId, SessionId};

    #[test]
    fn breadcrumb_detail_shell_includes_command() {
        let input = serde_json::json!({"command": "cargo test --workspace"});
        assert_eq!(
            breadcrumb_detail("shell", &input).as_deref(),
            Some("cargo test --workspace")
        );
    }

    #[test]
    fn breadcrumb_detail_web_search_includes_query() {
        let input = serde_json::json!({"query": "rust async runtime comparison"});
        assert_eq!(
            breadcrumb_detail("web_search", &input).as_deref(),
            Some("rust async runtime comparison")
        );
    }

    #[test]
    fn breadcrumb_detail_write_file_includes_path() {
        let input = serde_json::json!({"path": "/data/src/main.rs", "content": "fn main(){}"});
        assert_eq!(
            breadcrumb_detail("write_file", &input).as_deref(),
            Some("/data/src/main.rs")
        );
    }

    #[test]
    fn breadcrumb_detail_truncates_long_commands_with_ellipsis() {
        let long_cmd = "a".repeat(200);
        let input = serde_json::json!({"command": long_cmd});
        let got = breadcrumb_detail("shell", &input).unwrap();
        // Cap is 80; truncated form is 79 chars + '…'.
        assert_eq!(got.chars().count(), 80);
        assert!(got.ends_with('…'));
    }

    #[test]
    fn breadcrumb_detail_collapses_newlines_in_multiline_commands() {
        let input = serde_json::json!({"command": "set -e\necho hi\nls -la"});
        let got = breadcrumb_detail("shell", &input).unwrap();
        assert!(!got.contains('\n'), "newlines must be collapsed: {got:?}");
        assert_eq!(got, "set -e echo hi ls -la");
    }

    #[test]
    fn breadcrumb_detail_install_packages_joins_apt_and_npm() {
        let input = serde_json::json!({
            "apt": ["jq", "ripgrep"],
            "npm": ["typescript"],
        });
        let got = breadcrumb_detail("install_packages", &input).unwrap();
        assert_eq!(got, "jq, ripgrep, typescript");
    }

    #[test]
    fn breadcrumb_detail_returns_none_for_unknown_tool() {
        let input = serde_json::json!({"anything": "here"});
        assert!(breadcrumb_detail("not_a_known_tool", &input).is_none());
    }

    #[test]
    fn breadcrumb_detail_returns_none_when_expected_field_missing() {
        let input = serde_json::json!({"wrong_field": "value"});
        assert!(breadcrumb_detail("shell", &input).is_none());
    }

    fn fresh_ctx() -> (tempfile::TempDir, RunnerToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone());
        (tmp, ctx)
    }

    async fn last_row(ctx: &RunnerToolCtx) -> ironclaw_types::MessageOutRow {
        let guard = ctx.outbound.lock().await;
        let rows = ironclaw_db::tables::messages_out::list_due(&guard).unwrap();
        rows.into_iter().next_back().unwrap()
    }

    #[test]
    fn strip_reasoning_drops_thinking_blocks() {
        let raw = "<thinking>\nThe user wants X.\nI should do Y.\n</thinking>\n\nHere is the answer.";
        assert_eq!(strip_reasoning_blocks(raw), "Here is the answer.");
    }

    #[test]
    fn strip_reasoning_handles_multiple_blocks() {
        let raw = "<thinking>step 1</thinking>\nFirst.\n<thinking>step 2</thinking>\nSecond.";
        let out = strip_reasoning_blocks(raw);
        assert!(!out.contains("step"), "got: {out:?}");
        assert!(out.contains("First.") && out.contains("Second."));
    }

    #[test]
    fn strip_reasoning_preserves_unterminated_tag() {
        // If the model emits an open tag with no close, leave the text
        // alone — we'd rather ship a tagged message than silently
        // swallow the entire reply.
        let raw = "<thinking>\nThis never closes — keep the body.";
        assert_eq!(strip_reasoning_blocks(raw), raw);
    }

    #[test]
    fn strip_reasoning_is_case_insensitive_on_tag() {
        let raw = "<Thinking>reason</THINKING>\nReply.";
        assert_eq!(strip_reasoning_blocks(raw), "Reply.");
    }

    #[test]
    fn strip_reasoning_leaves_plain_text_alone() {
        let raw = "Plain reply with no reasoning tags.";
        assert_eq!(strip_reasoning_blocks(raw), raw);
    }

    #[tokio::test]
    async fn send_message_strips_thinking_block_from_chat_text() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
            to: Some(Recipient::Channel {
                id: "telegram:chat-1".into(),
            }),
            text: "<thinking>\nThe user said hi.\nI should greet them.\n</thinking>\n\nHi there!"
                .into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        let text = row.content["text"].as_str().unwrap();
        assert!(
            !text.contains("<thinking>") && !text.contains("</thinking>"),
            "thinking tags leaked into outbound text: {text:?}"
        );
        assert!(text.contains("Hi there!"));
        assert!(
            !text.contains("I should greet"),
            "reasoning content leaked: {text:?}"
        );
    }

    #[tokio::test]
    async fn send_message_writes_chat_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
                to: Some(Recipient::Channel {
                    id: "telegram:chat-1".into(),
                }),
                text: "hello".into(),
            }))
            .await
            .unwrap();
        let ToolEffectAck::Message { seq } = ack else {
            panic!("unexpected ack")
        };
        assert!(seq > 0 && seq % 2 == 1);
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Chat);
        assert_eq!(row.content["text"], "hello");
        assert!(row.content.get("to").is_some());
    }

    #[tokio::test]
    async fn child_send_message_with_no_to_routes_to_parent() {
        // Phase 2 of docs/plans/agent-to-agent-routing.md: when the
        // runner has a `source_session_id` AND the inbound was a
        // child-routed (no channel columns) row written by the
        // `agent_dispatch` handler, `send_message(to: None)` should
        // route UP to the parent — emit a MessageKind::Agent row.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let parent_session = SessionId::new();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(parent_session);
        // Simulate the inbound being processed: kickoff from the
        // parent, channel columns empty (agent_dispatch elides them).
        ctx.set_originating(OriginatingRouting {
            channel_type: None,
            platform_id: None,
            thread_id: None,
            in_reply_to: None,
            source_session_id: Some(parent_session),
        });
        ctx.emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
            to: None,
            text: "ran scout".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(
            row.kind,
            MessageKind::Agent,
            "child default routing on a parent-routed inbound must produce \
             an Agent-kind row, not Chat",
        );
        assert!(
            row.channel_type.is_none() && row.platform_id.is_none(),
            "Agent-kind rows must not carry channel routing — that's how the \
             delivery loop knows to dispatch via agent_dispatch instead of a \
             channel adapter",
        );
        assert_eq!(
            row.content["to"]["kind"], "agent",
            "body.to must be the tagged Agent recipient form",
        );
        assert_eq!(
            row.content["to"]["session_id"],
            parent_session.as_uuid().to_string(),
            "child must address its reply to the parent session",
        );
    }

    #[tokio::test]
    async fn child_send_message_to_none_with_user_channel_inbound_replies_to_user() {
        // The new guard: if the inbound being processed CAME FROM a
        // user channel (channel_type/platform_id Some) — e.g. a
        // per-thread wiring landed it directly on the child session —
        // then `send_message(to: None)` should reply back to the user,
        // NOT siphon up to the parent. Otherwise children wired to
        // user channels would silently swallow user messages.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let parent_session = SessionId::new();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(parent_session);
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: Some(parent_session),
        });
        ctx.emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
            to: None,
            text: "hi user".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(
            row.kind,
            MessageKind::Chat,
            "user-channel inbound on a child session must reply via channel, \
             not siphon to the parent",
        );
        assert_eq!(row.platform_id.as_deref(), Some("chat-1"));
    }

    #[tokio::test]
    async fn child_send_file_to_none_routes_via_channel_not_parent() {
        // The `agent_dispatch` handler can't relay file bytes between
        // sessions, so `send_file(to: None)` from a child must fall
        // back to the inherited channel routing (Chat-kind). Bytes go
        // to the user; the alternative is silent file loss.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(SessionId::new());
        ctx.set_originating(OriginatingRouting {
            channel_type: None,
            platform_id: None,
            thread_id: None,
            in_reply_to: None,
            source_session_id: ctx.source_session_id(),
        });
        ctx.emit_outbound(OutboundToolEffect::SendFile(SendFileSpec {
            to: None,
            filename: "report.pdf".into(),
            data: b"bytes".to_vec(),
            text: Some("see attached".into()),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(
            row.kind,
            MessageKind::Chat,
            "send_file must NOT use Agent-kind dispatch (it can't carry bytes \
             across session boundaries) — fall back to Chat so the inherited \
             channel actually delivers the file",
        );
    }

    #[tokio::test]
    async fn child_send_message_no_to_propagates_thread_id_to_body() {
        // Thread context must reach the parent's inbound row. The
        // runner copies origin.thread_id into the Agent body so
        // `agent_dispatch` can write it onto the parent's inbound.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let parent_session = SessionId::new();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(parent_session);
        ctx.set_originating(OriginatingRouting {
            channel_type: None,
            platform_id: None,
            thread_id: Some("user-thread-1".into()),
            in_reply_to: None,
            source_session_id: Some(parent_session),
        });
        ctx.emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
            to: None,
            text: "in-thread reply".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Agent);
        assert_eq!(
            row.content["thread_id"], "user-thread-1",
            "thread_id must be preserved on the Agent-kind body so \
             agent_dispatch can carry it onto the parent's inbound",
        );
    }

    #[tokio::test]
    async fn child_send_message_with_explicit_channel_still_works() {
        // The new routing only flips the default. If the child agent
        // explicitly addresses a channel (e.g. crossposting to a sibling
        // chat), that should still produce a Chat-kind row.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(SessionId::new());
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: ctx.source_session_id(),
        });
        ctx.emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
            to: Some(Recipient::Channel {
                id: "telegram:other".into(),
            }),
            text: "cross-post".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Chat);
    }

    #[tokio::test]
    async fn send_file_writes_to_disk_and_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::SendFile(SendFileSpec {
            to: None,
            filename: "doc.txt".into(),
            data: b"data!".to_vec(),
            text: Some("see attached".into()),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Chat);
        let files = row.content.get("files").and_then(|v| v.as_array()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["filename"], "doc.txt");
        // Bytes are on disk under the outbox at the row's id.
        let id_str = row.id.as_uuid().to_string();
        let file_path = ctx.outbox_root().join(&id_str).join("doc.txt");
        let bytes = std::fs::read(&file_path).unwrap();
        assert_eq!(bytes, b"data!");
    }

    #[tokio::test]
    async fn send_file_rejects_dangerous_filename() {
        let (_tmp, ctx) = fresh_ctx();
        let err = ctx
            .emit_outbound(OutboundToolEffect::SendFile(SendFileSpec {
                to: None,
                filename: "../escape.txt".into(),
                data: b"x".to_vec(),
                text: None,
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn edit_message_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::EditMessage(EditMessageSpec {
            message_seq: 7,
            text: "edited".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["edit"]["seq"], 7);
        assert_eq!(row.content["edit"]["text"], "edited");
    }

    #[tokio::test]
    async fn add_reaction_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::AddReaction(AddReactionSpec {
            message_seq: 7,
            emoji: "thumbsup".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["reaction"]["emoji"], "thumbsup");
    }

    #[tokio::test]
    async fn ask_question_returns_question_ack() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::AskUserQuestion(AskUserQuestionSpec {
                title: "ok?".into(),
                options: vec!["yes".into(), "no".into()],
                to: None,
            }))
            .await
            .unwrap();
        let qid = match ack {
            ToolEffectAck::Question { id } => id,
            other => panic!("unexpected: {other:?}"),
        };
        assert!(qid.starts_with("q_"));
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["ask_user_question"]["id"], qid);
        assert_eq!(row.content["ask_user_question"]["title"], "ok?");
    }

    #[tokio::test]
    async fn send_card_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::SendCard(SendCardSpec {
            to: None,
            card: serde_json::json!({"hi": 1}),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        assert_eq!(row.content["send_card"]["card"]["hi"], 1);
    }

    #[tokio::test]
    async fn create_agent_writes_system_row_and_accepts() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::CreateAgent(CreateAgentSpec {
                name: "n".into(),
                instructions: "i".into(),
                channel: Some("cli".into()),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["create_agent"]["name"], "n");
        assert_eq!(row.content["create_agent"]["channel"], "cli");
    }

    #[tokio::test]
    async fn install_packages_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::InstallPackages(InstallSpec {
                apt: vec!["jq".into()],
                npm: vec!["zod".into()],
                reason: "needed for x".into(),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["install_packages"]["apt"], serde_json::json!(["jq"]));
        assert_eq!(
            row.content["install_packages"]["npm"],
            serde_json::json!(["zod"])
        );
    }

    #[tokio::test]
    async fn add_mcp_server_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        ctx.emit_outbound(OutboundToolEffect::AddMcpServer(AddMcpServerSpec {
            name: "n".into(),
            transport: serde_json::json!({"kind": "stdio"}),
            reason: "r".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.content["add_mcp_server"]["name"], "n");
        assert_eq!(row.content["add_mcp_server"]["transport"]["kind"], "stdio");
    }

    #[tokio::test]
    async fn schedule_create_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::ScheduleTask(ScheduleSpec {
                name: "t".into(),
                when: None,
                prompt: "p".into(),
                recurrence: Some("0 * * * *".into()),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["schedule"]["op"], "create");
        assert_eq!(row.content["schedule"]["payload"]["name"], "t");
    }

    #[tokio::test]
    async fn schedule_list_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::ListTasks)
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        let row = last_row(&ctx).await;
        assert_eq!(row.content["schedule"]["op"], "list");
    }

    #[tokio::test]
    async fn schedule_cancel_pause_resume_each_write_rows() {
        let (_tmp, ctx) = fresh_ctx();
        for (effect, op) in [
            (
                OutboundToolEffect::CancelTask {
                    id: "task_1".into(),
                },
                "cancel",
            ),
            (
                OutboundToolEffect::PauseTask {
                    id: "task_2".into(),
                },
                "pause",
            ),
            (
                OutboundToolEffect::ResumeTask {
                    id: "task_3".into(),
                },
                "resume",
            ),
        ] {
            let ack = ctx.emit_outbound(effect).await.unwrap();
            assert!(matches!(ack, ToolEffectAck::Task { .. }));
            let row = last_row(&ctx).await;
            assert_eq!(row.content["schedule"]["op"], op);
        }
    }

    #[tokio::test]
    async fn schedule_update_writes_system_row() {
        let (_tmp, ctx) = fresh_ctx();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::UpdateTask(UpdateTaskSpec {
                id: "task_9".into(),
                prompt: Some("new prompt".into()),
                when: Some(None),
                recurrence: Some(Some("@daily".into())),
            }))
            .await
            .unwrap();
        match ack {
            ToolEffectAck::Task { id } => assert_eq!(id, "task_9"),
            other => panic!("unexpected: {other:?}"),
        }
        let row = last_row(&ctx).await;
        assert_eq!(row.content["schedule"]["op"], "update");
        assert_eq!(row.content["schedule"]["payload"]["prompt"], "new prompt");
    }

    #[tokio::test]
    async fn list_tasks_returns_empty_vec() {
        let (_tmp, ctx) = fresh_ctx();
        let tasks = ctx.list_tasks().await.unwrap();
        assert!(tasks.is_empty());
    }

    // ── spawn_subagent / explore integration ────────────────────────

    use async_trait::async_trait;
    use ironclaw_providers::{AgentProvider, AgentQuery, ProviderError, QueryInput};
    use ironclaw_types::ProviderEvent;
    use std::sync::Mutex as StdMutex;

    struct OneShotProvider {
        events: StdMutex<Option<Vec<ProviderEvent>>>,
    }

    impl OneShotProvider {
        fn new(events: Vec<ProviderEvent>) -> Arc<Self> {
            Arc::new(Self {
                events: StdMutex::new(Some(events)),
            })
        }
    }

    #[async_trait]
    impl AgentProvider for OneShotProvider {
        fn name(&self) -> &'static str {
            "oneshot"
        }
        async fn query(
            &self,
            _input: QueryInput,
        ) -> Result<Box<dyn AgentQuery>, ProviderError> {
            let events = self.events.lock().unwrap().take().unwrap_or_default();
            Ok(Box::new(OneShotQuery {
                events: StdMutex::new(events),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct OneShotQuery {
        events: StdMutex<Vec<ProviderEvent>>,
    }

    #[async_trait]
    impl AgentQuery for OneShotQuery {
        async fn push(&mut self, _: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            let mut g = self.events.lock().unwrap();
            if g.is_empty() {
                None
            } else {
                Some(g.remove(0))
            }
        }
        async fn abort(&mut self) {}
    }

    fn ctx_with_subagent(
        provider_events: Vec<ProviderEvent>,
    ) -> (tempfile::TempDir, RunnerToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(
            tmp.path(),
            AgentGroupId::new(),
            SessionId::new(),
        );
        let conn = open_outbound(&paths).unwrap();
        let outbound = Arc::new(Mutex::new(conn));
        let provider: Arc<dyn AgentProvider> = OneShotProvider::new(provider_events);
        let tool_map = Arc::new(HashMap::new());
        let deps = SubagentRunnerDeps {
            provider,
            tool_map,
            system: "parent system".into(),
            model: "test-model".into(),
            effort: Effort::Low,
            per_turn_max_tokens: 4096,
            temperature: None,
            assistant_name: None,
            provider_deadline: Duration::from_secs(5),
        };
        let ctx = RunnerToolCtx::new(outbound, paths.outbox.clone())
            .with_subagent(deps);
        (tmp, ctx)
    }

    #[tokio::test]
    async fn spawn_subagent_happy_returns_summary() {
        let (_tmp, ctx) = ctx_with_subagent(vec![ProviderEvent::Result {
            text: Some("from subagent".into()),
        }]);
        let req = SubagentRequest {
            task: "look at thing".into(),
            max_turns: 3,
            max_tokens: 10_000,
            tools_allowed: vec!["read_file".into()],
            nested: false,
        };
        let out = ctx.spawn_subagent(req).await.unwrap();
        assert_eq!(out.summary, "from subagent");
        assert_eq!(out.turns_used, 1);
    }

    #[tokio::test]
    async fn spawn_subagent_refuses_nested() {
        let (_tmp, ctx) = ctx_with_subagent(vec![ProviderEvent::Result {
            text: Some("never".into()),
        }]);
        let req = SubagentRequest {
            task: "look".into(),
            max_turns: 1,
            max_tokens: 1000,
            tools_allowed: vec![],
            nested: true,
        };
        let err = ctx.spawn_subagent(req).await.unwrap_err();
        assert!(matches!(err, ToolError::Validation(s) if s.contains("nested")));
    }

    #[tokio::test]
    async fn spawn_subagent_without_deps_returns_context_error() {
        let (_tmp, ctx) = fresh_ctx();
        let req = SubagentRequest {
            task: "x".into(),
            max_turns: 1,
            max_tokens: 1000,
            tools_allowed: vec![],
            nested: false,
        };
        let err = ctx.spawn_subagent(req).await.unwrap_err();
        assert!(matches!(err, ToolError::Context(s) if s.contains("subagent deps not wired")));
    }
}
