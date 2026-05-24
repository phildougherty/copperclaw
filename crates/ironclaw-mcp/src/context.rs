//! `ToolContext`: the trait every tool handler depends on for side effects.
//!
//! Handlers themselves are pure (validate input, build an `OutboundToolEffect`,
//! call the context). The runner crate implements `ToolContext` by writing
//! effects to `outbound.db` and mutating the scheduler. Tests use
//! `MockToolContext` (below) to record calls without touching any I/O.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use crate::error::ToolError;

/// Acknowledgement returned by the runner when an effect was accepted.
///
/// Tools that emit a message return the assigned message id; tools that
/// create a task return the assigned task id; tools that have no natural
/// identifier return [`ToolEffectAck::Accepted`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolEffectAck {
    /// Generic ack with no payload.
    Accepted,
    /// A message was queued; the runner returns the assigned numeric sequence.
    Message {
        /// The `seq` field of the outbound message row.
        seq: i64,
    },
    /// A scheduled task was created.
    Task {
        /// The task id assigned by the scheduler.
        id: String,
    },
    /// A question was asked; the runner returns the assigned question id.
    Question {
        /// The question id assigned by the host.
        id: String,
    },
    /// A new agent was created.
    Agent {
        /// The session id of the newly created agent.
        session_id: String,
    },
}

/// Reference to a recipient for outbound delivery.
///
/// The `to` parameter on most tools is optional and means "reply on the
/// originating channel"; the runner is responsible for materialising that
/// default. When the caller supplies `to`, it is one of the variants below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Recipient {
    /// Send to a specific channel id, e.g. `"telegram:chat-123"`.
    Channel {
        /// Fully-qualified channel id understood by the channel router.
        id: String,
    },
    /// Send to another agent by its session id.
    Agent {
        /// Session id of the destination agent.
        session_id: String,
    },
    /// Send to a user by their user id (the host resolves the route).
    User {
        /// User id (string form of `UserId`).
        id: String,
    },
}

/// A single self-mod install request. The runner translates this into an
/// approval request and ultimately a privileged action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallSpec {
    /// `apt` package names.
    pub apt: Vec<String>,
    /// `npm` package names.
    pub npm: Vec<String>,
    /// Human-readable reason for the install (required, audited).
    pub reason: String,
}

/// A request to register a new MCP server with the host for this agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddMcpServerSpec {
    /// Unique name for the server within this agent's scope.
    pub name: String,
    /// Transport configuration; opaque to this crate (validated by the host).
    pub transport: serde_json::Value,
    /// Human-readable reason.
    pub reason: String,
}

/// Spec for `create_agent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAgentSpec {
    /// Display name.
    pub name: String,
    /// System instructions / prompt.
    pub instructions: String,
    /// Optional channel binding for the new agent.
    pub channel: Option<String>,
}

/// Spec for `schedule_task`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleSpec {
    /// Caller-supplied task name (also used for display).
    pub name: String,
    /// Absolute target time (UTC). If `None`, `recurrence` must be set.
    pub when: Option<DateTime<Utc>>,
    /// Prompt to inject when the task fires.
    pub prompt: String,
    /// Optional cron-style recurrence (croner syntax).
    pub recurrence: Option<String>,
}

/// Spec for `update_task`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateTaskSpec {
    /// Task id (string form of `TaskId`).
    pub id: String,
    /// New prompt, if changing.
    pub prompt: Option<String>,
    /// New `when`, if changing. Pass `Some(None)` to clear.
    pub when: Option<Option<DateTime<Utc>>>,
    /// New recurrence, if changing. Pass `Some(None)` to clear.
    pub recurrence: Option<Option<String>>,
}

/// Spec for `send_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageSpec {
    /// Recipient; `None` means "reply on origin channel".
    pub to: Option<Recipient>,
    /// Message text.
    pub text: String,
}

/// Spec for `send_file`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendFileSpec {
    /// Recipient; `None` means "reply on origin channel".
    pub to: Option<Recipient>,
    /// File name to present to the recipient.
    pub filename: String,
    /// Raw file bytes (the JSON wire transport carries this base64-encoded).
    #[serde(with = "crate::context::bytes_b64")]
    pub data: Vec<u8>,
    /// Optional accompanying caption.
    pub text: Option<String>,
}

/// Spec for `edit_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditMessageSpec {
    /// Sequence number of the outbound message to edit.
    pub message_seq: i64,
    /// Replacement text.
    pub text: String,
}

/// Spec for `add_reaction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddReactionSpec {
    /// Sequence number of the target message.
    pub message_seq: i64,
    /// Emoji or platform-specific reaction shortcode.
    pub emoji: String,
}

/// Spec for `ask_user_question`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestionSpec {
    /// Question title shown to the user.
    pub title: String,
    /// Allowed answer options (1..=N).
    pub options: Vec<String>,
    /// Recipient; `None` means "ask on the origin channel".
    pub to: Option<Recipient>,
}

/// Spec for `send_card` — the canonical portable card schema defined in
/// `ironclaw-channels-core`. The runner serialises this directly into a
/// `MessageKind::Card` outbound row; the delivery service deserialises it
/// back into [`ironclaw_channels_core::Card`] and hands it to the
/// adapter's `deliver_card` hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendCardSpec {
    /// Recipient; `None` means "reply on origin channel".
    pub to: Option<Recipient>,
    /// Canonical card. Validated against the schema at construction time
    /// by the `send_card` MCP tool — anything that reaches the runner is
    /// guaranteed to pass [`ironclaw_channels_core::Card::validate`].
    pub card: ironclaw_channels_core::Card,
}

/// The sum type of every side effect a tool may emit.
///
/// The runner's `ToolContext` impl pattern-matches on this to write the
/// appropriate row(s) into `outbound.db` (or, for scheduling effects, to
/// mutate the scheduler state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum OutboundToolEffect {
    /// `send_message`.
    SendMessage(SendMessageSpec),
    /// `send_file`.
    SendFile(SendFileSpec),
    /// `edit_message`.
    EditMessage(EditMessageSpec),
    /// `add_reaction`.
    AddReaction(AddReactionSpec),
    /// `ask_user_question`.
    AskUserQuestion(AskUserQuestionSpec),
    /// `send_card`.
    SendCard(SendCardSpec),
    /// `create_agent`.
    CreateAgent(CreateAgentSpec),
    /// `install_packages`.
    InstallPackages(InstallSpec),
    /// `add_mcp_server`.
    AddMcpServer(AddMcpServerSpec),
    /// `schedule_task`.
    ScheduleTask(ScheduleSpec),
    /// `list_tasks`.
    ListTasks,
    /// `cancel_task`.
    CancelTask {
        /// Task id (string form of `TaskId`).
        id: String,
    },
    /// `pause_task`.
    PauseTask {
        /// Task id (string form of `TaskId`).
        id: String,
    },
    /// `resume_task`.
    ResumeTask {
        /// Task id (string form of `TaskId`).
        id: String,
    },
    /// `update_task`.
    UpdateTask(UpdateTaskSpec),
}

impl OutboundToolEffect {
    /// Stable name suitable for logging and metrics.
    pub fn tool_name(&self) -> &'static str {
        match self {
            Self::SendMessage(_) => "send_message",
            Self::SendFile(_) => "send_file",
            Self::EditMessage(_) => "edit_message",
            Self::AddReaction(_) => "add_reaction",
            Self::AskUserQuestion(_) => "ask_user_question",
            Self::SendCard(_) => "send_card",
            Self::CreateAgent(_) => "create_agent",
            Self::InstallPackages(_) => "install_packages",
            Self::AddMcpServer(_) => "add_mcp_server",
            Self::ScheduleTask(_) => "schedule_task",
            Self::ListTasks => "list_tasks",
            Self::CancelTask { .. } => "cancel_task",
            Self::PauseTask { .. } => "pause_task",
            Self::ResumeTask { .. } => "resume_task",
            Self::UpdateTask(_) => "update_task",
        }
    }
}

/// A description of a single task as surfaced by `list_tasks`.
///
/// This is a runner-facing view, intentionally simpler than
/// `ironclaw_types::ScheduledTask` so the context impl can build it from
/// whatever scheduler-internal shape it likes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    /// Task id (string form of `TaskId`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Current scheduler status (`pending`/`active`/`paused`/...).
    pub status: String,
    /// Next fire time if scheduled.
    pub when: Option<DateTime<Utc>>,
    /// Cron-style recurrence if scheduled.
    pub recurrence: Option<String>,
}

/// Request handed to [`ToolContext::spawn_subagent`] by the `explore` tool.
///
/// The fields are deliberately small: the runner reuses its own
/// provider, model, and base system prompt — the caller just supplies the
/// task, the allowlisted tools, and the bounded budgets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRequest {
    /// Free-text task description. Becomes the subagent's user message.
    pub task: String,
    /// Maximum LLM turns. Hard-capped by the host to
    /// [`SUBAGENT_MAX_TURNS_LIMIT`].
    pub max_turns: u32,
    /// Maximum cumulative input tokens across all turns. Hard-capped
    /// by the host to [`SUBAGENT_MAX_TOKENS_LIMIT`].
    pub max_tokens: u32,
    /// Tool name allowlist. Anything outside this list is refused
    /// with a synthetic tool-result error.
    pub tools_allowed: Vec<String>,
    /// True when this request is itself originating from inside a
    /// subagent. The runner refuses nested explore calls; the mock
    /// records-then-refuses.
    pub nested: bool,
}

/// Hard cap on the `max_turns` field of [`SubagentRequest`].
pub const SUBAGENT_MAX_TURNS_LIMIT: u32 = 10;
/// Hard cap on the `max_tokens` field of [`SubagentRequest`]. This is
/// an *input* budget — the subagent's cumulative `input_tokens` across
/// turns must stay under this. Output tokens are accounted but not
/// budgeted (the model's own `max_tokens` per turn bounds them).
pub const SUBAGENT_MAX_TOKENS_LIMIT: u32 = 200_000;
/// Hard wall-clock cap on a single subagent invocation. Enforced as a
/// `tokio::time::timeout` around the whole loop.
pub const SUBAGENT_WALL_CLOCK_SECS: u64 = 60;

/// One tool call observed during a subagent run. Surfaced verbatim in
/// the `explore` tool's response so the parent agent can audit what
/// the subagent actually did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentToolCall {
    /// Tool name the subagent invoked (or attempted to invoke — entries
    /// for refused calls are kept too so the audit trail is complete).
    pub name: String,
    /// Verbatim input the subagent passed. Truncated by callers if it
    /// would explode the parent's context; this struct itself does not
    /// elide.
    pub input: serde_json::Value,
}

/// Result returned by [`ToolContext::spawn_subagent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentResult {
    /// Final assistant text. Empty when the loop exited without a
    /// final text turn (e.g. cap hit on `tool_use`); the caller is
    /// expected to surface `summary` as-is.
    pub summary: String,
    /// How many LLM turns actually fired. Always `<= max_turns`.
    pub turns_used: u32,
    /// Cumulative input + output tokens across the run. The runner
    /// charges these against the parent's daily budget.
    pub tokens_used: u32,
    /// Tool calls the subagent made, in order.
    pub tools_called: Vec<SubagentToolCall>,
}

/// The contract that every tool handler relies on for side effects.
///
/// Implementations: the runner (writes to outbound.db, mutates scheduler);
/// [`MockToolContext`] (records calls in-memory for tests).
#[async_trait]
pub trait ToolContext: Send + Sync {
    /// Emit a side effect. The runner converts this into an outbound DB row
    /// (or a scheduler mutation) and returns an ack the tool surfaces back
    /// to the caller as a `CallToolResult`.
    async fn emit_outbound(
        &self,
        effect: OutboundToolEffect,
    ) -> Result<ToolEffectAck, ToolError>;

    /// Convenience hook so `list_tasks` does not need a separate request/ack
    /// pathway. Implementors may delegate to whatever scheduler state they
    /// own; the mock keeps a synthetic table.
    async fn list_tasks(&self) -> Result<Vec<TaskSummary>, ToolError>;

    /// Open an in-process LLM subagent loop. Default impl returns
    /// `ToolError::Context("subagent not supported in this context")` so
    /// existing test contexts work without modification — only the
    /// runner's [`crate::ToolContext`] impl and contexts that opt in
    /// (notably `MockToolContext` in tests) override this.
    ///
    /// The `explore` tool calls into this method wrapped in a 60s
    /// wall-clock timeout enforced inside the tool. Implementations
    /// may add their own deadlines.
    ///
    /// Nested calls (a subagent itself calling `explore`) are refused
    /// at the tool layer by checking `req.nested == true` and
    /// returning `ToolError::Validation`.
    async fn spawn_subagent(
        &self,
        req: SubagentRequest,
    ) -> Result<SubagentResult, ToolError> {
        let _ = req;
        Err(ToolError::Context(
            "subagent not supported in this context".into(),
        ))
    }

    /// Stash channel-routing fields copied from the inbound that
    /// triggered the current turn. Implementations that write
    /// `messages_out` rows should populate `channel_type` /
    /// `platform_id` / `thread_id` from this when the tool caller
    /// didn't pass an explicit `to`.
    ///
    /// Default no-op so contexts that don't need routing (mocks,
    /// subagent adapters) continue to compile unchanged. The
    /// runner's `RunnerToolCtx` overrides this with the real
    /// implementation.
    fn set_originating(&self, channel_type: Option<&str>, platform_id: Option<&str>, thread_id: Option<&str>, in_reply_to: Option<&str>) {
        let _ = (channel_type, platform_id, thread_id, in_reply_to);
    }

    /// Clear the originating-routing stash. Called by the runner
    /// after the turn completes so a subsequent emit on this ctx
    /// (e.g. a host-side apology write) doesn't inherit stale
    /// routing.
    fn clear_originating(&self) {}

    /// Optional UX-observability hook: emit a brief
    /// `[tool_name detail]` chat message to the originating channel
    /// right before a tool call fires, so users see what the agent
    /// is working on during long turns. `input` is the model's full
    /// tool-call argument JSON (when available) — the runner-side
    /// implementation pulls per-tool detail strings out of it
    /// (command for `shell`, query for `web_search`, path for
    /// `write_file`, etc.) and appends them. Default no-op so most
    /// contexts opt-out. The runner's `RunnerToolCtx` enables this
    /// when `IRONCLAW_TOOL_BREADCRUMBS` is set; the implementation
    /// filters by a hard-coded allowlist of "visible" tools and
    /// only emits when there's real channel routing.
    async fn emit_breadcrumb(&self, tool_name: &str, input: Option<&serde_json::Value>) {
        let _ = (tool_name, input);
    }
}

/// In-memory recording implementation used by tests.
///
/// All calls land in `calls` in order; `list_tasks` returns whatever
/// `task_summaries` was seeded with.
#[derive(Debug, Default)]
pub struct MockToolContext {
    inner: Mutex<MockInner>,
}

#[derive(Debug, Default)]
struct MockInner {
    calls: Vec<OutboundToolEffect>,
    /// If set, `emit_outbound` returns this Err next.
    next_emit_err: Option<ToolError>,
    /// If set, `list_tasks` returns this Err next.
    next_list_err: Option<ToolError>,
    /// Pre-seeded list of tasks.
    task_summaries: Vec<TaskSummary>,
    /// Override ack returned by the next `emit_outbound`.
    next_ack: Option<ToolEffectAck>,
    /// Subagent requests recorded in order.
    subagent_calls: Vec<SubagentRequest>,
    /// Pre-seeded subagent result returned by the next
    /// `spawn_subagent`. When `None`, the mock returns a canned
    /// `SubagentResult { summary: "mock subagent: <task>", ... }`.
    next_subagent_result: Option<SubagentResult>,
    /// If set, the next `spawn_subagent` returns this Err instead.
    next_subagent_err: Option<ToolError>,
}

impl MockToolContext {
    /// Build a fresh mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of effects recorded so far.
    pub fn calls(&self) -> Vec<OutboundToolEffect> {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .calls
            .clone()
    }

    /// Number of calls recorded so far.
    pub fn call_count(&self) -> usize {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .calls
            .len()
    }

    /// Cause the *next* `emit_outbound` to fail.
    pub fn fail_next_emit(&self, err: ToolError) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .next_emit_err = Some(err);
    }

    /// Cause the *next* `list_tasks` to fail.
    pub fn fail_next_list(&self, err: ToolError) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .next_list_err = Some(err);
    }

    /// Override the ack returned by the next `emit_outbound`.
    pub fn set_next_ack(&self, ack: ToolEffectAck) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .next_ack = Some(ack);
    }

    /// Seed the task list returned by `list_tasks`.
    pub fn set_tasks(&self, tasks: Vec<TaskSummary>) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .task_summaries = tasks;
    }

    /// Snapshot of subagent requests recorded so far.
    pub fn subagent_calls(&self) -> Vec<SubagentRequest> {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .subagent_calls
            .clone()
    }

    /// Override the result returned by the next `spawn_subagent`.
    pub fn set_next_subagent_result(&self, result: SubagentResult) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .next_subagent_result = Some(result);
    }

    /// Cause the *next* `spawn_subagent` to fail.
    pub fn fail_next_subagent(&self, err: ToolError) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .next_subagent_err = Some(err);
    }
}

#[async_trait]
impl ToolContext for MockToolContext {
    async fn emit_outbound(
        &self,
        effect: OutboundToolEffect,
    ) -> Result<ToolEffectAck, ToolError> {
        let mut g = self.inner.lock().expect("MockToolContext mutex poisoned");
        if let Some(err) = g.next_emit_err.take() {
            return Err(err);
        }
        let ack = g.next_ack.take().unwrap_or(ToolEffectAck::Accepted);
        g.calls.push(effect);
        Ok(ack)
    }

    async fn list_tasks(&self) -> Result<Vec<TaskSummary>, ToolError> {
        let mut g = self.inner.lock().expect("MockToolContext mutex poisoned");
        if let Some(err) = g.next_list_err.take() {
            return Err(err);
        }
        Ok(g.task_summaries.clone())
    }

    async fn spawn_subagent(
        &self,
        req: SubagentRequest,
    ) -> Result<SubagentResult, ToolError> {
        let mut g = self.inner.lock().expect("MockToolContext mutex poisoned");
        if let Some(err) = g.next_subagent_err.take() {
            return Err(err);
        }
        let canned = SubagentResult {
            summary: format!("mock subagent: {}", req.task),
            turns_used: 1,
            tokens_used: 0,
            tools_called: Vec::new(),
        };
        let result = g.next_subagent_result.take().unwrap_or(canned);
        g.subagent_calls.push(req);
        Ok(result)
    }
}

/// Internal base64 helper used by `SendFileSpec`. The runner can produce
/// these bytes from `OutboundFile`, and the input JSON wire format accepts
/// standard base64.
pub(crate) mod bytes_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity((bytes.len() / 3 + 1) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
            let n: u32 = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        let bytes = s.trim().as_bytes();
        if bytes.len() % 4 != 0 {
            return Err(serde::de::Error::custom("base64 length not multiple of 4"));
        }
        let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
        let val = |c: u8| -> Result<u8, &'static str> {
            Ok(match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => 0,
                _ => return Err("invalid base64 char"),
            })
        };
        for chunk in bytes.chunks(4) {
            let v0 = val(chunk[0]).map_err(serde::de::Error::custom)?;
            let v1 = val(chunk[1]).map_err(serde::de::Error::custom)?;
            let v2 = val(chunk[2]).map_err(serde::de::Error::custom)?;
            let v3 = val(chunk[3]).map_err(serde::de::Error::custom)?;
            let n: u32 = (u32::from(v0) << 18)
                | (u32::from(v1) << 12)
                | (u32::from(v2) << 6)
                | u32::from(v3);
            out.push(((n >> 16) & 0xFF) as u8);
            if chunk[2] != b'=' {
                out.push(((n >> 8) & 0xFF) as u8);
            }
            if chunk[3] != b'=' {
                out.push((n & 0xFF) as u8);
            }
        }
        Ok(out)
    }
}

/// Same as `bytes_b64`, but for `Option<Vec<u8>>` fields where the
/// model may omit the data entirely (e.g. `send_file` when using
/// `path` instead). Delegates to `bytes_b64` for the actual decode
/// when present.
pub(crate) mod bytes_b64_optional {
    use super::bytes_b64;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        match opt {
            None => Ok(None),
            Some(s) if s.is_empty() => Ok(None),
            Some(s) => {
                // Funnel through bytes_b64's deserializer via a small
                // adapter so we don't duplicate the alphabet table.
                let de = serde::de::value::StrDeserializer::<D::Error>::new(&s);
                bytes_b64::deserialize(de).map(Some)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn tool_name_for_each_variant() {
        let cases: Vec<(OutboundToolEffect, &str)> = vec![
            (
                OutboundToolEffect::SendMessage(SendMessageSpec {
                    to: None,
                    text: "hi".into(),
                }),
                "send_message",
            ),
            (
                OutboundToolEffect::SendFile(SendFileSpec {
                    to: None,
                    filename: "f".into(),
                    data: vec![],
                    text: None,
                }),
                "send_file",
            ),
            (
                OutboundToolEffect::EditMessage(EditMessageSpec {
                    message_seq: 1,
                    text: "x".into(),
                }),
                "edit_message",
            ),
            (
                OutboundToolEffect::AddReaction(AddReactionSpec {
                    message_seq: 1,
                    emoji: ":x:".into(),
                }),
                "add_reaction",
            ),
            (
                OutboundToolEffect::AskUserQuestion(AskUserQuestionSpec {
                    title: "?".into(),
                    options: vec!["a".into()],
                    to: None,
                }),
                "ask_user_question",
            ),
            (
                OutboundToolEffect::SendCard(SendCardSpec {
                    to: None,
                    card: ironclaw_channels_core::Card {
                        title: Some("t".into()),
                        ..ironclaw_channels_core::Card::default()
                    },
                }),
                "send_card",
            ),
            (
                OutboundToolEffect::CreateAgent(CreateAgentSpec {
                    name: "a".into(),
                    instructions: "i".into(),
                    channel: None,
                }),
                "create_agent",
            ),
            (
                OutboundToolEffect::InstallPackages(InstallSpec {
                    apt: vec![],
                    npm: vec![],
                    reason: "r".into(),
                }),
                "install_packages",
            ),
            (
                OutboundToolEffect::AddMcpServer(AddMcpServerSpec {
                    name: "n".into(),
                    transport: serde_json::json!({}),
                    reason: "r".into(),
                }),
                "add_mcp_server",
            ),
            (
                OutboundToolEffect::ScheduleTask(ScheduleSpec {
                    name: "t".into(),
                    when: None,
                    prompt: "p".into(),
                    recurrence: Some("0 * * * *".into()),
                }),
                "schedule_task",
            ),
            (OutboundToolEffect::ListTasks, "list_tasks"),
            (
                OutboundToolEffect::CancelTask { id: "task_1".into() },
                "cancel_task",
            ),
            (
                OutboundToolEffect::PauseTask { id: "task_1".into() },
                "pause_task",
            ),
            (
                OutboundToolEffect::ResumeTask { id: "task_1".into() },
                "resume_task",
            ),
            (
                OutboundToolEffect::UpdateTask(UpdateTaskSpec {
                    id: "task_1".into(),
                    prompt: None,
                    when: None,
                    recurrence: None,
                }),
                "update_task",
            ),
        ];
        for (effect, expected) in cases {
            assert_eq!(effect.tool_name(), expected);
        }
    }

    #[tokio::test]
    async fn mock_records_calls() {
        let ctx = MockToolContext::new();
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
                to: None,
                text: "hi".into(),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Accepted);
        assert_eq!(ctx.call_count(), 1);
        assert!(matches!(
            &ctx.calls()[0],
            OutboundToolEffect::SendMessage(s) if s.text == "hi"
        ));
    }

    #[tokio::test]
    async fn mock_can_override_ack() {
        let ctx = MockToolContext::new();
        ctx.set_next_ack(ToolEffectAck::Message { seq: 42 });
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
                to: None,
                text: "hi".into(),
            }))
            .await
            .unwrap();
        assert_eq!(ack, ToolEffectAck::Message { seq: 42 });
    }

    #[tokio::test]
    async fn mock_can_fail_emit() {
        let ctx = MockToolContext::new();
        ctx.fail_next_emit(ToolError::Context("nope".into()));
        let err = ctx
            .emit_outbound(OutboundToolEffect::ListTasks)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Context(_)));
        // recorded nothing
        assert_eq!(ctx.call_count(), 0);
    }

    #[tokio::test]
    async fn mock_list_tasks_seeded() {
        let ctx = MockToolContext::new();
        ctx.set_tasks(vec![TaskSummary {
            id: "task_1".into(),
            name: "a".into(),
            status: "active".into(),
            when: None,
            recurrence: Some("0 * * * *".into()),
        }]);
        let v = ctx.list_tasks().await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "task_1");
    }

    #[tokio::test]
    async fn mock_list_tasks_can_fail() {
        let ctx = MockToolContext::new();
        ctx.fail_next_list(ToolError::Internal("x".into()));
        let err = ctx.list_tasks().await.unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[test]
    fn ack_serde_roundtrip() {
        let acks = vec![
            ToolEffectAck::Accepted,
            ToolEffectAck::Message { seq: 7 },
            ToolEffectAck::Task { id: "task_1".into() },
            ToolEffectAck::Question { id: "q_1".into() },
            ToolEffectAck::Agent {
                session_id: "sess_1".into(),
            },
        ];
        for a in acks {
            let s = serde_json::to_string(&a).unwrap();
            let back: ToolEffectAck = serde_json::from_str(&s).unwrap();
            assert_eq!(a, back);
        }
    }

    #[test]
    fn recipient_serde_roundtrip() {
        let recipients = vec![
            Recipient::Channel { id: "telegram:1".into() },
            Recipient::Agent { session_id: "sess_1".into() },
            Recipient::User { id: "u_1".into() },
        ];
        for r in recipients {
            let s = serde_json::to_string(&r).unwrap();
            let back: Recipient = serde_json::from_str(&s).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn send_file_bytes_b64_roundtrip() {
        let spec = SendFileSpec {
            to: None,
            filename: "x.bin".into(),
            data: (0u8..=40).collect(),
            text: None,
        };
        let s = serde_json::to_string(&spec).unwrap();
        let back: SendFileSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(spec.data, back.data);
    }

    #[test]
    fn outbound_effect_serde_tag() {
        let e = OutboundToolEffect::ListTasks;
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains("\"tool\""), "tag should be `tool`: {s}");
        let back: OutboundToolEffect = serde_json::from_str(&s).unwrap();
        assert_eq!(e, back);
    }
}
