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
        }
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
        let tool_ctx: Arc<dyn ToolContext> = Arc::new(SubagentCtxAdapter {
            inner: self.outbound.clone(),
            outbox_root: self.outbox_root.clone(),
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
        // Subagents do not own an originating-inbound concept (they
        // run on behalf of the parent turn), so we pass an empty
        // routing and let the parent's own emit-outbound handle the
        // user-facing reply.
        let origin = OriginatingRouting::default();
        apply_effect(conn, &outbox, effect, &origin).map_err(to_tool_error)
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

#[allow(clippy::needless_pass_by_value)]
fn apply_send_message(
    conn: &mut Connection,
    spec: SendMessageSpec,
    origin: &OriginatingRouting,
) -> Result<ToolEffectAck, ToolApplyError> {
    let SendMessageSpec { to, text } = spec;
    let routed = resolve_outbound_routing(to, origin);
    let mut body = serde_json::Map::new();
    body.insert("text".into(), serde_json::Value::String(text));
    if let Some(r) = &routed.body_to {
        body.insert("to".into(), serde_json::to_value(r).unwrap_or_default());
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
    safe_attachment_name(&filename)
        .map_err(|e| ToolApplyError::Validation(e.to_string()))?;
    let msg_id = MessageId::new();
    let msg_id_str = msg_id.as_uuid().to_string();
    let target_dir = outbox_root.join(&msg_id_str);
    std::fs::create_dir_all(&target_dir)?;
    let target = target_dir.join(&filename);
    std::fs::write(&target, &data)?;

    let routed = resolve_outbound_routing(to, origin);
    let mut body = serde_json::Map::new();
    if let Some(t) = text {
        body.insert("text".into(), serde_json::Value::String(t));
    }
    if let Some(r) = &routed.body_to {
        body.insert("to".into(), serde_json::to_value(r).unwrap_or_default());
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
/// 1. Caller passed `to: Recipient::Agent { session_id }` → kind Agent.
/// 2. Caller passed any other explicit `to:` → kind Chat with the
///    explicit recipient preserved in the body; the channel routing
///    still falls back to the originating inbound (the delivery loop
///    doesn't yet route arbitrary channel ids on its own).
/// 3. Caller passed no `to:` AND the session has a `source_session_id`
///    (i.e. this is a child agent) → kind Agent, synthesise the
///    recipient pointing at the parent. **This is the headline routing
///    change in `docs/plans/agent-to-agent-routing.md` Phase 2.**
/// 4. Otherwise → kind Chat, channel routing from the inbound. The
///    root-session "reply to user" default.
fn resolve_outbound_routing(
    to: Option<Recipient>,
    origin: &OriginatingRouting,
) -> OutboundRouting {
    use ironclaw_mcp::Recipient as R;
    let kind = match &to {
        Some(R::Agent { .. }) => MessageKind::Agent,
        Some(_) => MessageKind::Chat,
        None => {
            if origin.source_session_id.is_some() {
                MessageKind::Agent
            } else {
                MessageKind::Chat
            }
        }
    };
    let body_to = match to {
        Some(r) => Some(r),
        None => origin
            .source_session_id
            .map(|sid| R::Agent { session_id: sid.as_uuid().to_string() }),
    };
    OutboundRouting {
        kind,
        body_to,
        channel_type: origin.channel_type.clone(),
        platform_id: origin.platform_id.clone(),
        thread_id: origin.thread_id.clone(),
        in_reply_to: origin.in_reply_to,
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
        // runner has a `source_session_id` (i.e. this is a child agent
        // spawned via `create_agent`), `send_message(to: None)` should
        // emit a MessageKind::Agent row whose body points at the
        // parent — NOT a chat-kind row dumped into the inherited
        // user channel.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let parent_session = SessionId::new();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(parent_session);
        // Simulate a parent inbound that the child is processing —
        // inheriting the user's channel routing.
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
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
            "child default routing must produce an Agent-kind row, not Chat",
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
