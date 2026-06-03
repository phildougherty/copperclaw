//! `ToolContext` implementation that writes effects to `outbound.db`.
//!
//! The runner's [`RunnerToolCtx`] implements [`copperclaw_mcp::ToolContext`]
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
//! { "create_agent":  { "name": "...", "instructions": "...", "channel": "..." } }
//! { "install_packages": { "apt": [...], "npm": [...], "reason": "..." } }
//! { "add_mcp_server":   { "name": "...", "transport": {...}, "reason": "..." } }
//! { "schedule":      { "op": "create" | "cancel" | "pause" | "resume" | "update", "payload": {...} } }
//! ```
//!
//! `SendFile` writes the bytes to `outbox/<msg_id>/<filename>` and emits a
//! `chat`-kind row whose `content` includes a `files` array pointing at the
//! filename.
//!
//! `SendCard` emits a `card`-kind row (NOT a system row) whose `content` is:
//!
//! ```json
//! { "card": { "title": "...", "body": "...", "fields": [...], "buttons": [...], "image_url": "..." },
//!   "to":   { "kind": "channel" | "agent" | "user", ... }  // optional, only when caller passed `to:`
//! }
//! ```
//!
//! The host-delivery service deserialises `content.card` back into
//! [`copperclaw_channels_core::Card`] and hands it to the adapter's
//! `deliver_card` hook. Channels with native card support render the
//! structure; the trait-level default converts to a text fallback.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Path inside the container where the host writes the per-session tasks
/// snapshot for the `list_tasks` MCP tool. Mirrors
/// `copperclaw_host::container_manager::TASKS_SNAPSHOT_FILENAME` under the
/// `/data` bind. Test-overridable via the env var below.
const TASKS_SNAPSHOT_DEFAULT_PATH: &str = "/data/tasks.json";
const TASKS_SNAPSHOT_ENV_OVERRIDE: &str = "COPPERCLAW_TASKS_SNAPSHOT_FILE";

fn tasks_snapshot_path() -> PathBuf {
    std::env::var_os(TASKS_SNAPSHOT_ENV_OVERRIDE)
        .map_or_else(|| PathBuf::from(TASKS_SNAPSHOT_DEFAULT_PATH), PathBuf::from)
}

use async_trait::async_trait;
use chrono::Utc;
use copperclaw_db::DbError;
use copperclaw_db::attachments::safe_attachment_name;
use copperclaw_db::tables::messages_out::{self, WriteOutbound};
use copperclaw_mcp::{
    AddMcpServerSpec, AddReactionSpec, AskUserQuestionSpec, CreateAgentSpec, EditMessageSpec,
    EmitTodoListSpec, InstallSpec, OutboundToolEffect, Recipient, ScheduleSpec, SendCardSpec,
    SendFileSpec, SendMessageSpec, SubagentRequest, SubagentResult, TaskSummary, ToolContext,
    ToolEffectAck, ToolEntry, ToolError, UpdateTaskSpec,
};
use copperclaw_providers::AgentProvider;
use copperclaw_types::{Effort, MessageId, MessageKind};
use rusqlite::Connection;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::subagent::{SubagentDeps, SubagentInputs, run_inner_loop};

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
    pub source_session_id: Option<copperclaw_types::SessionId>,
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
    source_session_id: Option<copperclaw_types::SessionId>,
    /// When true, the runner emits a brief `[tool] tool_name` chat
    /// message to the originating channel at the start of every
    /// "visible" tool call (`shell` / `web_search` / `write_file` /
    /// `explore` / etc.). Off by default; enabled via the
    /// `COPPERCLAW_TOOL_BREADCRUMBS` env var.
    breadcrumbs_enabled: bool,
    /// One-shot gate for child agents (sessions with
    /// [`source_session_id`] set). Flipped to `true` the first time the
    /// child emits a `send_message` effect. The runner's main loop
    /// checks this after each turn and exits cleanly when set —
    /// turning every child into a one-shot agent that delivers ONE
    /// reply and stops, even if the LLM tries to send follow-ups.
    ///
    /// Required because soft prompt rules ("EXACTLY ONE `send_message`")
    /// don't reliably constrain free / open-weight models. Lived
    /// through on 2026-05-24 with `openrouter/owl-alpha` sending a
    /// report + "report delivered" summary as two separate messages.
    ///
    /// Stays `false` forever for root sessions (no `source_session_id`),
    /// so the gate is a no-op for the top-level conversation.
    parent_reply_sent: Arc<std::sync::atomic::AtomicBool>,
    /// Chip presentation: `Chips` (one chip message per tool, legacy
    /// default) or `Rolling` (one aggregate "activity" chip per turn that
    /// accumulates every tool step into an expandable region). Set via
    /// `COPPERCLAW_BREADCRUMB_STYLE`.
    breadcrumb_style: BreadcrumbStyle,
    /// Rolling-mode accumulator for the current turn (the steps shown in
    /// the aggregate chip's expandable region, plus whether the chip has
    /// been emitted yet this turn). Reset by [`Self::begin_activity`].
    activity: Arc<std::sync::Mutex<ActivityState>>,
    /// Active skill's `allowed-tools` (set by the `load_skill` tool when
    /// the loaded skill declared an `allowed-tools` frontmatter list).
    /// `None` means no tool-scoping skill is active. The runner's
    /// dispatch gate reads this each call and feeds it into
    /// [`crate::policy::ToolPolicy::with_active_skill`] so a read-only
    /// skill narrows the live policy. `Arc<Mutex<...>>` so it survives
    /// `Arc<dyn ToolContext>` erasure and is mutable from the shared ref.
    active_skill_allowed: Arc<std::sync::Mutex<Option<Vec<String>>>>,
}

/// How tool-progress breadcrumbs are presented in chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BreadcrumbStyle {
    /// One chip message per tool call, edited Running → Done in place.
    #[default]
    Chips,
    /// One aggregate "activity" chip per turn: a collapsed summary line
    /// plus an expandable, per-step-styled list of every tool the turn
    /// ran. Far less chat churn for long, tool-heavy turns.
    Rolling,
}

/// Per-turn accumulator backing [`BreadcrumbStyle::Rolling`].
#[derive(Debug, Default)]
struct ActivityState {
    /// Tool steps for the current turn, in start order. Each is a
    /// `Breadcrumb` (tool/detail/status/summary) the renderer styles
    /// individually inside the expandable region.
    steps: Vec<copperclaw_channels_core::Breadcrumb>,
    /// True once the aggregate chip has been emitted this turn, so later
    /// events edit it (via `update_breadcrumb`) instead of opening a new
    /// chip. Reset to `false` at the start of each turn.
    chip_open: bool,
}

/// Stable pseudo tool-name the aggregate chip rides under so the host's
/// "most recent Breadcrumb row for this tool/origin" correlation edits
/// the same chip across a turn's tool events.
const ACTIVITY_CHIP_TOOL: &str = "activity";

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
            parent_reply_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            breadcrumb_style: BreadcrumbStyle::default(),
            activity: Arc::new(std::sync::Mutex::new(ActivityState::default())),
            active_skill_allowed: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// True when this session is a child of another session (was
    /// spawned via `create_agent`) AND has already emitted its first
    /// `send_message`. Used by the runner's main loop to exit cleanly
    /// after a child delivers its one-shot reply.
    pub fn parent_reply_sent(&self) -> bool {
        self.source_session_id.is_some()
            && self
                .parent_reply_sent
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Enable native tool-progress breadcrumb chips for visible tools.
    /// **Default: ON.** Reads the `COPPERCLAW_TOOL_BREADCRUMBS` env var
    /// to allow an operator to OPT OUT (`0`/`false`/`no`/`off`); any
    /// other value (including unset) leaves the default on. Called at
    /// runner startup from `main.rs`.
    ///
    /// The default flip (off → on) shipped alongside the slice-2
    /// `deliver_breadcrumb` native renderers — there's no UX payoff if
    /// the surface is gated behind an env var nobody knows to set.
    /// The opt-out exists for noisy / low-bandwidth deployments where
    /// the operator doesn't want chip churn in chat.
    #[must_use]
    pub fn with_breadcrumbs_from_env(mut self) -> Self {
        self.breadcrumbs_enabled = !matches!(
            std::env::var("COPPERCLAW_TOOL_BREADCRUMBS").ok().as_deref(),
            Some("0" | "false" | "no" | "off")
        );
        // `rolling` collapses a turn's tools into one expandable activity
        // chip; anything else (incl. unset) keeps the legacy per-tool chips.
        self.breadcrumb_style = match std::env::var("COPPERCLAW_BREADCRUMB_STYLE").ok().as_deref() {
            Some("rolling") => BreadcrumbStyle::Rolling,
            _ => BreadcrumbStyle::Chips,
        };
        self
    }

    /// Force-enable breadcrumbs. Used by tests so we don't depend on
    /// process-level env vars in concurrent test runs.
    #[must_use]
    pub fn with_breadcrumbs_enabled(mut self, enabled: bool) -> Self {
        self.breadcrumbs_enabled = enabled;
        self
    }

    /// Force the breadcrumb presentation style. Used by tests.
    #[must_use]
    pub fn with_breadcrumb_style(mut self, style: BreadcrumbStyle) -> Self {
        self.breadcrumb_style = style;
        self
    }

    /// Start a fresh rolling-activity turn: clear the accumulated steps so
    /// the next tool opens a new aggregate chip. Called via the
    /// [`copperclaw_mcp::ToolContext::begin_activity`] trait hook at the
    /// top of each `drive_turn`. No-op in `Chips` mode.
    fn reset_activity(&self) {
        if self.breadcrumb_style != BreadcrumbStyle::Rolling {
            return;
        }
        if let Ok(mut act) = self.activity.lock() {
            act.steps.clear();
            act.chip_open = false;
        }
    }

    /// Emit a [`MessageKind::Breadcrumb`] outbound row capturing the
    /// agent's tool invocation as a `Running` chip on the originating
    /// channel. The host's delivery loop hands the row to the
    /// adapter's `deliver_breadcrumb` hook, which renders a compact
    /// native chip (Telegram HTML `<code>`, Slack Block Kit `context`,
    /// Discord embed footer, Google Chat cards v2, Matrix `m.notice`
    /// with `<code>`) — or falls back to the legacy `[tool] detail`
    /// text line on adapters without a native renderer.
    ///
    /// No-op when breadcrumbs are disabled, the tool isn't on the
    /// "visible" allowlist, or there's no channel routing to send to
    /// (e.g. agent-to-agent rows where the destination is the parent).
    ///
    /// We also stash a row-id → seq mapping (via the
    /// `_breadcrumb_seq` field on the row body) so a subsequent
    /// [`Self::emit_breadcrumb_finish`] can reference the original
    /// chip when emitting the update — adapters with an in-place edit
    /// API use this to replace the chip's contents rather than emit a
    /// fresh row.
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
        // Skip for child sessions only (their recipient is another
        // LLM, not a user). Origin's channel routing may be None —
        // delivery's session_routing wiring fallback resolves it.
        // See [`should_skip_user_facing_emit`] for the 2026-05-24
        // incident that motivated this relaxation.
        if self.should_skip_user_facing_emit() {
            return;
        }
        let origin = self.current_originating();
        // Per-tool detail: shell command, search query, file path…
        // Falls back to None when extraction fails so the renderer
        // shows just `[tool_name]`.
        let detail = input.and_then(|v| breadcrumb_detail(tool_name, v));
        if self.breadcrumb_style == BreadcrumbStyle::Rolling {
            self.emit_rolling_start(tool_name, detail, &origin).await;
            return;
        }
        let breadcrumb = copperclaw_channels_core::Breadcrumb {
            tool_name: tool_name.to_owned(),
            detail,
            status: copperclaw_channels_core::BreadcrumbStatus::Running,
            summary: None,
            steps: Vec::new(),
        };
        // `breadcrumb.validate()` is best-effort: a too-long detail
        // would just cap the renderer's display. Skip validation here
        // (the runner already truncates detail strings to 80 chars).
        self.insert_breadcrumb_row(&breadcrumb, &origin).await;
    }

    /// Rolling-mode tool start: append a `Running` step and emit (first
    /// tool of the turn) or edit (subsequent) the single aggregate chip.
    async fn emit_rolling_start(
        &self,
        tool_name: &str,
        detail: Option<String>,
        origin: &OriginatingRouting,
    ) {
        let (aggregate, first) = {
            let Ok(mut act) = self.activity.lock() else {
                return;
            };
            act.steps.push(copperclaw_channels_core::Breadcrumb {
                tool_name: tool_name.to_owned(),
                detail,
                status: copperclaw_channels_core::BreadcrumbStatus::Running,
                summary: None,
                steps: Vec::new(),
            });
            let first = !act.chip_open;
            act.chip_open = true;
            (activity_aggregate(&act.steps), first)
        };
        if first {
            self.insert_breadcrumb_row(&aggregate, origin).await;
        } else {
            self.insert_update_breadcrumb_row(ACTIVITY_CHIP_TOOL, &aggregate, origin)
                .await;
        }
    }

    /// Insert a first-emit `MessageKind::Breadcrumb` row (a new chip).
    async fn insert_breadcrumb_row(
        &self,
        breadcrumb: &copperclaw_channels_core::Breadcrumb,
        origin: &OriginatingRouting,
    ) {
        let body = serde_json::json!({ "breadcrumb": breadcrumb });
        let routed = OutboundRouting {
            kind: MessageKind::Breadcrumb,
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

    /// Insert an `update_breadcrumb` System row that edits the prior chip
    /// for `tool_name` in place (on adapters with an edit API).
    async fn insert_update_breadcrumb_row(
        &self,
        tool_name: &str,
        breadcrumb: &copperclaw_channels_core::Breadcrumb,
        origin: &OriginatingRouting,
    ) {
        let payload = serde_json::json!({
            "update_breadcrumb": {
                "tool_name": tool_name,
                "breadcrumb": breadcrumb,
            }
        });
        let routed = OutboundRouting {
            kind: MessageKind::System,
            body_to: None,
            channel_type: origin.channel_type.clone(),
            platform_id: origin.platform_id.clone(),
            thread_id: origin.thread_id.clone(),
            in_reply_to: None,
        };
        let mut guard = self.outbound.lock().await;
        let conn: &mut Connection = &mut guard;
        let _ = insert_outbound_row(conn, MessageId::new(), payload, &routed);
    }

    /// Finalisation half of [`Self::emit_breadcrumb`]. Writes a
    /// `MessageKind::System` row carrying an `update_breadcrumb`
    /// action; the host's delivery loop looks up the most recent
    /// Breadcrumb-kind row for this tool/origin and, on adapters with
    /// an edit API, edits the original chip in place. Adapters
    /// without an edit API surface a fresh chip with the completion
    /// blurb (visible but harmless).
    ///
    /// Best-effort: swallows errors and is a no-op when breadcrumbs
    /// are disabled, the tool isn't on the visible allowlist, or the
    /// originating routing was cleared.
    pub async fn emit_breadcrumb_finish(
        &self,
        tool_name: &str,
        input: Option<&serde_json::Value>,
        ok: bool,
        summary: Option<&str>,
    ) {
        if !self.breadcrumbs_enabled {
            return;
        }
        if !is_visible_breadcrumb_tool(tool_name) {
            return;
        }
        // Mirror `emit_breadcrumb`'s relaxation — skip only for child
        // sessions, not when origin lacks channel routing.
        if self.should_skip_user_facing_emit() {
            return;
        }
        let origin = self.current_originating();
        let detail = input.and_then(|v| breadcrumb_detail(tool_name, v));
        if self.breadcrumb_style == BreadcrumbStyle::Rolling {
            self.emit_rolling_finish(tool_name, detail, ok, summary, &origin)
                .await;
            return;
        }
        let status = if ok {
            copperclaw_channels_core::BreadcrumbStatus::Done
        } else {
            copperclaw_channels_core::BreadcrumbStatus::Failed
        };
        let breadcrumb = copperclaw_channels_core::Breadcrumb {
            tool_name: tool_name.to_owned(),
            detail,
            status,
            summary: summary.map(str::to_owned),
            steps: Vec::new(),
        };
        // Update rows ride the System path so the delivery loop's
        // existing system-action dispatch handles them. The row inherits
        // the originating inbound's channel routing so the adapter can
        // look up the prior chip's platform message id.
        self.insert_update_breadcrumb_row(tool_name, &breadcrumb, &origin)
            .await;
    }

    /// Rolling-mode tool finish: flip the matching `Running` step to
    /// `Done`/`Failed` and edit the aggregate chip in place.
    async fn emit_rolling_finish(
        &self,
        tool_name: &str,
        detail: Option<String>,
        ok: bool,
        summary: Option<&str>,
        origin: &OriginatingRouting,
    ) {
        let status = if ok {
            copperclaw_channels_core::BreadcrumbStatus::Done
        } else {
            copperclaw_channels_core::BreadcrumbStatus::Failed
        };
        let aggregate = {
            let Ok(mut act) = self.activity.lock() else {
                return;
            };
            // Flip the earliest still-running step for this tool. Parallel
            // calls to the same tool finish in execution order, so the
            // earliest Running one is the right match.
            if let Some(step) = act.steps.iter_mut().find(|s| {
                s.tool_name == tool_name
                    && s.status == copperclaw_channels_core::BreadcrumbStatus::Running
            }) {
                step.status = status;
                step.summary = summary.map(str::to_owned);
                if step.detail.is_none() {
                    step.detail = detail;
                }
            } else {
                // A finish with no matching start (shouldn't happen, but be
                // safe): append it so the step is still surfaced.
                act.steps.push(copperclaw_channels_core::Breadcrumb {
                    tool_name: tool_name.to_owned(),
                    detail,
                    status,
                    summary: summary.map(str::to_owned),
                    steps: Vec::new(),
                });
                act.chip_open = true;
            }
            activity_aggregate(&act.steps)
        };
        self.insert_update_breadcrumb_row(ACTIVITY_CHIP_TOOL, &aggregate, origin)
            .await;
    }

    /// Slice-3.5 opt-in emit path. Persists a structured
    /// `MessageKind::Thinking` outbound row carrying the canonical
    /// `ThinkingBlock` payload so the host delivery service can render
    /// it as a collapsed native UI primitive (Telegram `<blockquote
    /// expandable>`, Slack `context` block, Discord muted-grey embed,
    /// Google Chat `collapsibleSection`, Matrix `<details>`).
    ///
    /// `text` is the accumulated thinking prose (empty for redacted
    /// blocks); `redacted == true` flags the upstream
    /// `redacted_thinking` variant — adapters render a placeholder
    /// rather than the raw blob. `model` is optional provenance.
    ///
    /// The opt-in privacy gate is enforced UPSTREAM in
    /// `run::provider_call::pump_events` — this method runs only after
    /// the runner has verified the per-group `surface_thinking` flag
    /// is on. Errors are swallowed; thinking is best-effort UX
    /// observability, not load-bearing.
    pub async fn emit_thinking(&self, text: &str, redacted: bool, model: Option<&str>) {
        // Mirror `emit_status`'s relaxation — see
        // [`should_skip_user_facing_emit`] for rationale.
        if self.should_skip_user_facing_emit() {
            return;
        }
        let origin = self.current_originating();
        // Cap `text` at the schema's max — Anthropic streams can produce
        // very long reasoning blocks for `effort=high`, and we don't
        // want to overflow the renderer's tight on-screen budget. The
        // canonical fallback wraps everything in quoted lines on
        // text-only channels so length matters.
        let mut capped_text = String::from(text);
        let max = copperclaw_channels_core::MAX_THINKING_CHARS;
        if capped_text.chars().count() > max {
            capped_text = capped_text.chars().take(max).collect();
        }
        let block = copperclaw_channels_core::ThinkingBlock {
            text: capped_text,
            redacted,
            model: model.map(|m| {
                let model_len = m.chars().count();
                let model_max = copperclaw_channels_core::MAX_THINKING_MODEL_CHARS;
                if model_len > model_max {
                    m.chars().take(model_max).collect()
                } else {
                    m.to_owned()
                }
            }),
        };
        let body = serde_json::json!({ "thinking": block });
        let routed = OutboundRouting {
            kind: MessageKind::Thinking,
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

    /// Slice-3.1 emit path. Persists a structured `MessageKind::Diff`
    /// outbound row carrying the canonical `DiffCard` payload so the
    /// host delivery service can render it as a native diff (Telegram
    /// `MarkdownV2` ` ```diff ``` `, Slack Block Kit
    /// `rich_text_preformatted`, Discord embed + color, Google Chat
    /// Cards v2 `decoratedText`, Matrix `<pre><code
    /// class="language-diff">…</code></pre>`).
    ///
    /// Emitted *alongside* the existing tool breadcrumb — breadcrumb
    /// says "what tool ran", diff card says "what changed". Routes to
    /// the originating inbound channel so the user sees the diff in
    /// the same conversation they triggered the edit from.
    ///
    /// Errors are swallowed: diff cards are best-effort UX, NOT
    /// load-bearing — a failed write must not abort the file edit.
    /// Same no-route guard as `emit_breadcrumb`: skip when there's no
    /// human channel on the receiving end (agent-to-agent rows).
    pub async fn emit_diff(&self, diff: copperclaw_channels_core::DiffCard) {
        // Mirror `emit_status`'s relaxation — see
        // [`should_skip_user_facing_emit`] for rationale.
        if self.should_skip_user_facing_emit() {
            return;
        }
        let origin = self.current_originating();
        let body = serde_json::json!({ "diff": diff });
        let routed = OutboundRouting {
            kind: MessageKind::Diff,
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

    /// Returns true when status / breadcrumb / diff / thinking emits
    /// should be skipped because the recipient isn't a human user.
    ///
    /// The ONLY skip condition is "this runner is a child session"
    /// (its [`source_session_id`] was wired in by the host's
    /// `create_agent` path). Child sessions report up to their parent
    /// LLM, not to a human channel, so UX-observability chatter would
    /// just bloat the parent's history.
    ///
    /// We DO NOT skip merely because the originating inbound lacks
    /// `channel_type` / `platform_id`. Agent-dispatched inbounds
    /// (child reports forwarded into a root session's inbound) carry
    /// NULL channel routing on the row but the messaging-group
    /// wiring's `session_routing` fallback fills in the user channel
    /// at delivery time. Lived through on 2026-05-24: a Telegram run
    /// spawned 3 children, the parent picked up the third child's
    /// forwarded report (NULL routing, `source_session_id` set on the
    /// row), spent 5+ minutes silently synthesising a prototype, and
    /// the user saw no heartbeat — the old gate over-strictly
    /// skipped because origin had no channel, even though normal
    /// `send_message` calls in the same scenario reach the user just
    /// fine via the wiring fallback.
    fn should_skip_user_facing_emit(&self) -> bool {
        self.source_session_id.is_some()
    }

    /// Emit a "still working" status row to the originating channel.
    /// Triggered from `drive_turn` after a configurable silent stretch
    /// (default 60s) so users see a heartbeat-shaped chat message when
    /// a tool-heavy turn (or a long silent reasoning pass) would
    /// otherwise look like the agent has hung.
    ///
    /// Routing rules:
    ///   - Skipped for child sessions
    ///     (see [`should_skip_user_facing_emit`] for rationale).
    ///   - Writes a `MessageKind::Chat` row direct to outbound,
    ///     bypassing the `emit_outbound` one-shot gate. Status
    ///     messages are not "the reply" and must not consume the
    ///     child-reply budget.
    ///   - Channel routing on the row may be `None` (agent-dispatched
    ///     inbounds carry NULL routing). Delivery's `session_routing`
    ///     fallback (`crates/copperclaw-host-delivery/src/service.rs`,
    ///     `resolve_target`) fills in the user channel from the
    ///     messaging-group wiring before dispatch.
    ///
    /// Errors are swallowed: status is best-effort UX, not load-bearing.
    pub async fn emit_status(&self, text: &str) {
        if self.should_skip_user_facing_emit() {
            return;
        }
        let origin = self.current_originating();
        let body = serde_json::json!({ "text": text });
        let routed = OutboundRouting {
            kind: MessageKind::Chat,
            body_to: None,
            channel_type: origin.channel_type.clone(),
            platform_id: origin.platform_id.clone(),
            thread_id: origin.thread_id.clone(),
            in_reply_to: origin.in_reply_to,
        };
        let mut guard = self.outbound.lock().await;
        let conn: &mut Connection = &mut guard;
        let _ = insert_outbound_row(conn, MessageId::new(), body, &routed);
    }

    /// Stash the parent session id on the context so `apply_send_message`
    /// can route the child's default `to: None` calls to the parent's
    /// inbound instead of the inherited messaging-group channel.
    #[must_use]
    pub fn with_source_session_id(mut self, parent: copperclaw_types::SessionId) -> Self {
        self.source_session_id = Some(parent);
        self
    }

    /// Accessor for the parent session id (None when this is a root session).
    #[must_use]
    pub fn source_session_id(&self) -> Option<copperclaw_types::SessionId> {
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
    async fn emit_outbound(&self, effect: OutboundToolEffect) -> Result<ToolEffectAck, ToolError> {
        let outbox = self.outbox_root.clone();
        let origin = self.current_originating();
        // One-shot gate for child agents: refuse second send_message.
        // The flag is only set for children (`source_session_id` set)
        // AFTER their first successful send_message — so root sessions
        // and pre-first-send children pass through untouched.
        if self.source_session_id.is_some()
            && matches!(effect, OutboundToolEffect::SendMessage(_))
            && self
                .parent_reply_sent
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(ToolError::Internal(
                "child agent has already delivered its one-shot reply; \
                 a child session may emit only one `send_message`. The \
                 prior send was accepted; this duplicate is refused."
                    .to_string(),
            ));
        }
        let is_child_send_message = self.source_session_id.is_some()
            && matches!(effect, OutboundToolEffect::SendMessage(_));
        // Take the lock for the entire DB write.
        let mut guard = self.outbound.lock().await;
        let conn: &mut Connection = &mut guard;
        let ack = apply_effect(conn, &outbox, effect, &origin).map_err(to_tool_error)?;
        // Flip the gate after the first successful child-send. The
        // runner's main loop checks `parent_reply_sent()` after each
        // turn and exits when set.
        if is_child_send_message {
            self.parent_reply_sent
                .store(true, std::sync::atomic::Ordering::Release);
        }
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
            ToolError::Internal(format!("list_tasks: parse {}: {err}", path.display()))
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
            in_reply_to: in_reply_to.and_then(|s| Uuid::parse_str(s).ok().map(MessageId::from)),
            source_session_id: self.source_session_id,
        };
        Self::set_originating(self, routing);
    }

    fn clear_originating(&self) {
        Self::clear_originating(self);
    }

    fn set_active_skill_allowed_tools(&self, allowed: Option<Vec<String>>) {
        if let Ok(mut guard) = self.active_skill_allowed.lock() {
            *guard = allowed;
        }
    }

    fn active_skill_allowed_tools(&self) -> Option<Vec<String>> {
        self.active_skill_allowed
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn parent_reply_sent(&self) -> bool {
        Self::parent_reply_sent(self)
    }

    async fn emit_breadcrumb(&self, tool_name: &str, input: Option<&serde_json::Value>) {
        Self::emit_breadcrumb(self, tool_name, input).await;
    }

    fn begin_activity(&self) {
        self.reset_activity();
    }

    async fn emit_breadcrumb_finish(
        &self,
        tool_name: &str,
        input: Option<&serde_json::Value>,
        ok: bool,
        summary: Option<&str>,
    ) {
        Self::emit_breadcrumb_finish(self, tool_name, input, ok, summary).await;
    }

    async fn emit_thinking(&self, text: &str, redacted: bool, model: Option<&str>) {
        Self::emit_thinking(self, text, redacted, model).await;
    }

    async fn emit_status(&self, text: &str) {
        Self::emit_status(self, text).await;
    }

    async fn emit_diff(&self, diff: copperclaw_channels_core::DiffCard) {
        Self::emit_diff(self, diff).await;
    }

    async fn spawn_subagent(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError> {
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
        if req.nested || self.in_subagent.load(std::sync::atomic::Ordering::Relaxed) {
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
            wall_clock: Duration::from_secs(copperclaw_mcp::SUBAGENT_WALL_CLOCK_SECS),
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
    async fn emit_outbound(&self, effect: OutboundToolEffect) -> Result<ToolEffectAck, ToolError> {
        let outbox = self.outbox_root.clone();
        let mut guard = self.inner.lock().await;
        let conn: &mut Connection = &mut guard;
        apply_effect(conn, &outbox, effect, &self.origin).map_err(to_tool_error)
    }
    async fn list_tasks(&self) -> Result<Vec<TaskSummary>, ToolError> {
        Ok(Vec::new())
    }
    async fn spawn_subagent(&self, _req: SubagentRequest) -> Result<SubagentResult, ToolError> {
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
        OutboundToolEffect::SendCard(spec) => apply_send_card(conn, spec, origin),
        OutboundToolEffect::EmitTodoList(spec) => apply_emit_todo_list(conn, spec, origin),
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
/// Build the collapsed aggregate chip from the current rolling steps.
/// The top-level fields are the one-line summary (current activity +
/// completed/total count) shown collapsed; `steps` carries the full
/// per-step list the renderer styles individually in the expandable
/// region. The aggregate's `tool_name` is the stable [`ACTIVITY_CHIP_TOOL`]
/// so the host correlation edits one chip across the turn.
fn activity_aggregate(
    steps: &[copperclaw_channels_core::Breadcrumb],
) -> copperclaw_channels_core::Breadcrumb {
    use copperclaw_channels_core::BreadcrumbStatus;
    let total = steps.len();
    let done = steps
        .iter()
        .filter(|s| s.status != BreadcrumbStatus::Running)
        .count();
    let any_running = steps.iter().any(|s| s.status == BreadcrumbStatus::Running);
    let any_failed = steps.iter().any(|s| s.status == BreadcrumbStatus::Failed);
    // Current activity = the latest still-running step, else the last step.
    let current = steps
        .iter()
        .rev()
        .find(|s| s.status == BreadcrumbStatus::Running)
        .or_else(|| steps.last());
    let detail = current.map(|s| match &s.detail {
        Some(d) if !d.is_empty() => format!("{} {}", s.tool_name, d),
        _ => s.tool_name.clone(),
    });
    let status = if any_running {
        BreadcrumbStatus::Running
    } else if any_failed {
        BreadcrumbStatus::Failed
    } else {
        BreadcrumbStatus::Done
    };
    let summary = Some(if total == 1 {
        "1 step".to_owned()
    } else {
        format!("{done}/{total} steps")
    });
    copperclaw_channels_core::Breadcrumb {
        tool_name: ACTIVITY_CHIP_TOOL.to_owned(),
        detail,
        status,
        summary,
        steps: steps.to_vec(),
    }
}

/// Allow-list of tool names that get a chat breadcrumb when
/// `COPPERCLAW_TOOL_BREADCRUMBS=1`. Limited to tools that (a) take
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
            | "multi_edit"
            | "apply_patch"
            | "copy_file"
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
        "read_file" | "write_file" | "edit_file" | "multi_edit" | "apply_patch" => field("path"),
        "copy_file" => {
            // Show "src → dst" so the user sees both ends of the copy
            // in one glance.
            let src = field("src");
            let dst = field("dst");
            match (src, dst) {
                (Some(s), Some(d)) => Some(format!("{s} → {d}")),
                (Some(s), None) | (None, Some(s)) => Some(s),
                _ => None,
            }
        }
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

/// Threshold for triggering the slice-3.4 long-output expander
/// decorator: lines count.
pub(crate) const EXPANDER_LINE_THRESHOLD: usize = 30;
/// Threshold for triggering the long-output expander decorator: byte
/// length of the body. Set well above the largest splitter
/// `max_message_chars()` (Slack's 40 000 is the ceiling among shipped
/// channels) so a single agent message that's "just over the platform
/// cap" goes through the splitter unwrapped, not folded into an
/// expander chip. Long *tool output* (the use case this surface is
/// for) — pages of shell stdout, a 30-line `cat` — easily exceeds
/// 64 KB.
pub(crate) const EXPANDER_BYTE_THRESHOLD: usize = 64 * 1024;
/// How many leading lines we capture into the preview ("teaser") when
/// attaching an expander decorator. Six lines fits the average mobile
/// viewport without scroll while still giving a meaningful look at
/// what the user will get when they expand.
pub(crate) const EXPANDER_PREVIEW_LINES: usize = 6;

/// Build the long-output expander decorator (slice 3.4) when `text`
/// exceeds the line OR byte threshold. Returns `None` if the text is
/// short enough that no decoration is warranted — the chat row goes
/// through the default `dispatch_chat` path unchanged.
///
/// The returned JSON has the shape:
///
/// ```json
/// {
///   "summary": "<short host-generated one-liner>",
///   "summary_kind": "lines" | "bytes",
///   "preview_lines": ["<line 1>", "<line 2>", …]
/// }
/// ```
///
/// `dispatch_chat` (host-delivery) checks for `content.expander` and
/// routes such rows to `ChannelAdapter::deliver_collapsible` instead
/// of `deliver`. The full body still rides as `content.text` — the
/// decorator only carries the metadata an adapter needs to render the
/// "summary + disclosure" treatment.
pub(crate) fn build_expander_decorator(text: &str) -> Option<serde_json::Value> {
    let bytes = text.len();
    let line_count = text.lines().count();
    let exceeds_lines = line_count > EXPANDER_LINE_THRESHOLD;
    let exceeds_bytes = bytes > EXPANDER_BYTE_THRESHOLD;
    if !exceeds_lines && !exceeds_bytes {
        return None;
    }
    // Prefer the line trigger when both fire — line count is the more
    // human-meaningful unit ("30 lines" is more legible than "8 KB"
    // when both are true of the same buffer).
    let summary_kind = if exceeds_lines { "lines" } else { "bytes" };
    let summary = if exceeds_lines {
        format!("long output: {line_count} lines ({bytes} B)")
    } else {
        format!("long output: {bytes} bytes ({line_count} lines)")
    };
    let preview_lines: Vec<String> = text
        .lines()
        .take(EXPANDER_PREVIEW_LINES)
        .map(str::to_owned)
        .collect();
    Some(serde_json::json!({
        "summary": summary,
        "summary_kind": summary_kind,
        "preview_lines": preview_lines,
    }))
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
    // Slice 3.4: attach the long-output expander decorator BEFORE we
    // move `text` into the body. Only attaches on Chat-kind rows
    // (Agent-kind rows are runner-to-runner traffic; the adapter
    // expander surface doesn't apply there). The decorator just adds
    // a `content.expander` sibling to `content.text`; the full text
    // still rides in the row for the renderer to use.
    let expander = if matches!(routed.kind, MessageKind::Chat) {
        build_expander_decorator(&text)
    } else {
        None
    };
    body.insert("text".into(), serde_json::Value::String(text));
    if let Some(e) = expander {
        body.insert("expander".into(), e);
    }
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
            body.insert(
                "thread_id".into(),
                serde_json::Value::String(thread.clone()),
            );
        }
    }
    let seq = insert_outbound_row(
        conn,
        MessageId::new(),
        serde_json::Value::Object(body),
        &routed,
    )?;
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
    safe_attachment_name(&filename).map_err(|e| ToolApplyError::Validation(e.to_string()))?;
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
    // Slice 3.4: same as `apply_send_message`, attach the long-output
    // expander decorator if the file's accompanying caption text is
    // long. Files themselves are rendered platform-native so the
    // decorator only governs the `text` body (the caption).
    let expander = match (&text, matches!(routed.kind, MessageKind::Chat)) {
        (Some(t), true) => build_expander_decorator(t),
        _ => None,
    };
    if let Some(t) = text {
        body.insert("text".into(), serde_json::Value::String(t));
    }
    if let Some(e) = expander {
        body.insert("expander".into(), e);
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
    let seq = insert_outbound_row(conn, msg_id, serde_json::Value::Object(body), &routed)?;
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
    origin: &OriginatingRouting,
) -> Result<ToolEffectAck, ToolApplyError> {
    let SendCardSpec { to, card } = spec;
    // Reuse the existing chat-routing resolver so Card-kind rows
    // inherit the originating inbound's channel / platform / thread
    // exactly the way Chat rows do. After it returns we override the
    // kind to Card — the Agent-routing branch is irrelevant for cards
    // (the model picks `send_message`/`send_file` for cross-agent
    // hand-offs, not `send_card`), but if `resolve_outbound_routing`
    // does flip to Agent (e.g. a child agent with no explicit `to`),
    // force the kind back to Card and drop the synthesised parent
    // recipient: a Card-kind row addressed at a parent session can't
    // be rendered by any adapter, so we fall back to Chat-style
    // delivery on the inherited channel routing. This mirrors the
    // belt-and-braces logic in `apply_send_file`.
    let mut routed = resolve_outbound_routing(to.clone(), origin);
    if matches!(routed.kind, MessageKind::Agent) {
        routed.body_to = None;
        routed.in_reply_to = origin.in_reply_to;
    }
    routed.kind = MessageKind::Card;

    let mut body = serde_json::Map::new();
    // The canonical Card serialises cleanly via its derived Serialize —
    // expect is justified because Card has no failable serialisation
    // paths and the MCP layer already validated the payload.
    body.insert(
        "card".into(),
        serde_json::to_value(&card).expect("Card is always serialisable"),
    );
    if let Some(r) = &routed.body_to {
        body.insert(
            "to".into(),
            serde_json::to_value(r).expect("Recipient is always serialisable"),
        );
    }
    let seq = insert_outbound_row(
        conn,
        MessageId::new(),
        serde_json::Value::Object(body),
        &routed,
    )?;
    Ok(ToolEffectAck::Message { seq })
}

/// Apply an [`EmitTodoListSpec`]: write a `MessageKind::TodoList`
/// outbound row carrying the canonical `TodoList` in `content.todo_list`,
/// routed to the originating inbound's channel exactly the way Card-kind
/// rows are. Mirrors `apply_send_card`'s body shape — just the typed key
/// differs.
///
/// Emitted by every mutating `todo_*` MCP-tool handler (`todo_add`,
/// `todo_update`, `todo_delete`) at the END of the mutation. The host
/// delivery service's `dispatch_todo_list` path looks up the prior
/// list's platform message id and threads it through to the adapter's
/// `deliver_todo_list` hook so the chip is edited in place rather than
/// emitted as a fresh row on every mutation. There is no model-facing
/// `emit_todo_list` tool — the spec is constructed implicitly by the
/// runner-side todo handlers; the model continues to call the existing
/// `todo_add` / `todo_update` / `todo_delete` tools unchanged.
#[allow(clippy::needless_pass_by_value)]
fn apply_emit_todo_list(
    conn: &mut Connection,
    spec: EmitTodoListSpec,
    origin: &OriginatingRouting,
) -> Result<ToolEffectAck, ToolApplyError> {
    let EmitTodoListSpec { list } = spec;
    // Inherit the originating inbound's channel routing the same way
    // Card / Chat rows do. We force the kind to TodoList AFTER the
    // resolver returns; the Agent-routing branch is irrelevant here
    // (a TodoList sent cross-agent makes no sense — the parent
    // doesn't care about its child's plan) but if the resolver does
    // flip to Agent (no explicit `to`, no inbound channel), force
    // the kind back and drop the body_to so we don't try to render
    // an inter-agent TodoList. Mirrors `apply_send_card`'s
    // belt-and-braces logic.
    let mut routed = resolve_outbound_routing(None, origin);
    if matches!(routed.kind, MessageKind::Agent) {
        routed.body_to = None;
        routed.in_reply_to = origin.in_reply_to;
    }
    routed.kind = MessageKind::TodoList;

    // Build the body via `Map::new()` + `.insert()` rather than the
    // `json!({...})` macro so the runner-emit-set coverage test (which
    // regex-greps for `json!({"<action>":`) doesn't mistake `"todo_list"`
    // for a System-action name. TodoList rows are a typed `MessageKind`
    // and go through `dispatch_todo_list`, not the System handler.
    // Mirrors the same dodge in `apply_send_card`.
    let mut body = serde_json::Map::new();
    body.insert(
        "todo_list".into(),
        serde_json::to_value(&list).expect("TodoList is always serialisable"),
    );
    let seq = insert_outbound_row(
        conn,
        MessageId::new(),
        serde_json::Value::Object(body),
        &routed,
    )?;
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
fn resolve_outbound_routing(to: Option<Recipient>, origin: &OriginatingRouting) -> OutboundRouting {
    use copperclaw_mcp::Recipient as R;
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
        None if inbound_came_from_parent => origin.source_session_id.map(|sid| R::Agent {
            session_id: sid.as_uuid().to_string(),
        }),
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
                .map(|s| copperclaw_types::ChannelType::new(s.as_str()))
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
    use copperclaw_db::session::{SessionPaths, open_outbound};
    use copperclaw_mcp::Recipient;
    use copperclaw_types::{AgentGroupId, SessionId};

    #[test]
    fn breadcrumb_detail_shell_includes_command() {
        let input = serde_json::json!({"command": "cargo test --workspace"});
        assert_eq!(
            breadcrumb_detail("shell", &input).as_deref(),
            Some("cargo test --workspace")
        );
    }

    #[test]
    fn activity_aggregate_summarises_steps_and_current() {
        use copperclaw_channels_core::{Breadcrumb, BreadcrumbStatus};
        let steps = vec![
            Breadcrumb::running("read_file")
                .with_detail("a.rs")
                .finished(true, Some("10 lines".into())),
            Breadcrumb::running("shell").with_detail("cargo build"),
        ];
        let agg = activity_aggregate(&steps);
        assert_eq!(agg.tool_name, ACTIVITY_CHIP_TOOL);
        // A still-running step → overall Running.
        assert_eq!(agg.status, BreadcrumbStatus::Running);
        assert_eq!(agg.summary.as_deref(), Some("1/2 steps"));
        // Current activity = the latest running step.
        assert_eq!(agg.detail.as_deref(), Some("shell cargo build"));
        assert_eq!(agg.steps.len(), 2);
    }

    #[test]
    fn activity_aggregate_status_rolls_up_done_and_failed() {
        use copperclaw_channels_core::{Breadcrumb, BreadcrumbStatus};
        let all_ok = vec![
            Breadcrumb::running("a").finished(true, None),
            Breadcrumb::running("b").finished(true, None),
        ];
        assert_eq!(activity_aggregate(&all_ok).status, BreadcrumbStatus::Done);
        let with_fail = vec![
            Breadcrumb::running("a").finished(true, None),
            Breadcrumb::running("b").finished(false, Some("boom".into())),
        ];
        assert_eq!(
            activity_aggregate(&with_fail).status,
            BreadcrumbStatus::Failed
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

    #[tokio::test]
    async fn emit_breadcrumb_writes_breadcrumb_kind_row() {
        // The runner used to write a Chat-kind row; this verifies the
        // switch to MessageKind::Breadcrumb with the canonical
        // `Breadcrumb` payload under `content.breadcrumb`. The
        // host's delivery loop dispatches on the kind so it can call
        // the adapter's `deliver_breadcrumb` hook (native chip
        // rendering) instead of plain `deliver`.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_breadcrumbs_enabled(true);
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        ctx.emit_breadcrumb(
            "shell",
            Some(&serde_json::json!({"command": "cargo check"})),
        )
        .await;
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Breadcrumb);
        let bc: copperclaw_channels_core::Breadcrumb =
            serde_json::from_value(row.content["breadcrumb"].clone()).unwrap();
        assert_eq!(bc.tool_name, "shell");
        assert_eq!(bc.detail.as_deref(), Some("cargo check"));
        assert_eq!(
            bc.status,
            copperclaw_channels_core::BreadcrumbStatus::Running
        );
        assert!(bc.summary.is_none());
        // Channel routing is inherited from the originating inbound so
        // the delivery loop has a target to dispatch to.
        assert_eq!(row.platform_id.as_deref(), Some("chat-1"));
    }

    #[tokio::test]
    async fn emit_breadcrumb_writes_even_without_channel_routing_for_root_session() {
        // 2026-05-24 fix: the gate used to skip when origin had NULL
        // channel routing. Lived through with a parent runner processing
        // an agent-dispatched inbound (child report up to parent): the
        // inbound row carries NULL channel_type/platform_id but
        // delivery's session_routing wiring fallback fills in the user
        // channel before dispatch. The old gate over-skipped, so the
        // user saw no breadcrumb chips during multi-minute synthesis.
        // The new gate skips ONLY for child sessions (source_session_id
        // set on the runner ctx); root sessions emit regardless.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_breadcrumbs_enabled(true);
        // No originating set → no channel info, but THIS IS A ROOT
        // session (no source_session_id) so the emit must fire.
        ctx.emit_breadcrumb("shell", Some(&serde_json::json!({"command": "ls"})))
            .await;
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "root-session breadcrumb must land even with NULL routing"
        );
        assert_eq!(rows[0].kind, MessageKind::Breadcrumb);
        // Routing fields on the row stay NULL; delivery's
        // session_routing fallback resolves them at dispatch time.
        assert!(rows[0].channel_type.is_none());
        assert!(rows[0].platform_id.is_none());
    }

    #[tokio::test]
    async fn emit_breadcrumb_skips_for_child_session() {
        // Child sessions (source_session_id set on the runner) report
        // up to a parent LLM, not a user — breadcrumb chatter would
        // just bloat the parent's history.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let parent_session = SessionId::new();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_breadcrumbs_enabled(true)
            .with_source_session_id(parent_session);
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: Some(parent_session),
        });
        ctx.emit_breadcrumb("shell", Some(&serde_json::json!({"command": "ls"})))
            .await;
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        assert!(rows.is_empty(), "child-session breadcrumb must skip");
    }

    #[tokio::test]
    async fn emit_breadcrumb_skips_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone());
        // breadcrumbs_enabled defaults to false.
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        ctx.emit_breadcrumb("shell", Some(&serde_json::json!({"command": "ls"})))
            .await;
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn emit_breadcrumb_finish_writes_update_system_row() {
        // The finish hook writes a MessageKind::System row carrying
        // an `update_breadcrumb` action. The host-delivery service's
        // system-action dispatcher (see `update_breadcrumb`
        // registration in copperclaw-host-delivery) translates this
        // into an in-place edit of the original chip.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_breadcrumbs_enabled(true);
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("slack".into()),
            platform_id: Some("C123".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        ctx.emit_breadcrumb_finish(
            "shell",
            Some(&serde_json::json!({"command": "cargo check"})),
            true,
            Some("passed (0.4s)"),
        )
        .await;
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::System);
        let bc: copperclaw_channels_core::Breadcrumb =
            serde_json::from_value(row.content["update_breadcrumb"]["breadcrumb"].clone()).unwrap();
        assert_eq!(bc.status, copperclaw_channels_core::BreadcrumbStatus::Done);
        assert_eq!(bc.summary.as_deref(), Some("passed (0.4s)"));
        assert_eq!(
            row.content["update_breadcrumb"]["tool_name"], "shell",
            "tool_name is duplicated at the action level so the host's \
             dispatcher can look up the prior chip without deserialising \
             the full Breadcrumb",
        );
    }

    #[tokio::test]
    async fn emit_breadcrumb_finish_marks_failed_when_not_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_breadcrumbs_enabled(true);
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("slack".into()),
            platform_id: Some("C123".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        ctx.emit_breadcrumb_finish("shell", None, false, Some("ENOENT"))
            .await;
        let row = last_row(&ctx).await;
        let bc: copperclaw_channels_core::Breadcrumb =
            serde_json::from_value(row.content["update_breadcrumb"]["breadcrumb"].clone()).unwrap();
        assert_eq!(
            bc.status,
            copperclaw_channels_core::BreadcrumbStatus::Failed
        );
        assert_eq!(bc.summary.as_deref(), Some("ENOENT"));
    }

    fn fresh_ctx() -> (tempfile::TempDir, RunnerToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone());
        (tmp, ctx)
    }

    #[tokio::test]
    async fn child_agent_first_send_message_flips_parent_reply_sent() {
        // The one-shot gate fires on the FIRST `send_message` emitted
        // by a child session (one with `source_session_id` set). After
        // that, `parent_reply_sent()` returns true and the runner's
        // main loop exits cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let parent = SessionId::new();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(parent);
        // Origin: inbound came from the parent (no channel routing,
        // source_session_id set). `send_message(to: None)` routes
        // back as `MessageKind::Agent` via `resolve_outbound_routing`.
        ctx.set_originating(OriginatingRouting {
            channel_type: None,
            platform_id: None,
            thread_id: None,
            in_reply_to: None,
            source_session_id: Some(parent),
        });
        assert!(!ctx.parent_reply_sent(), "fresh child has not replied yet");
        let spec = copperclaw_mcp::SendMessageSpec {
            to: None,
            text: "here is my one-shot report".into(),
        };
        ctx.emit_outbound(OutboundToolEffect::SendMessage(spec))
            .await
            .expect("first send_message accepted");
        assert!(ctx.parent_reply_sent(), "gate flips after first child-send");
    }

    #[tokio::test]
    async fn child_agent_second_send_message_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let parent = SessionId::new();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone())
            .with_source_session_id(parent);
        ctx.set_originating(OriginatingRouting {
            channel_type: None,
            platform_id: None,
            thread_id: None,
            in_reply_to: None,
            source_session_id: Some(parent),
        });
        // First send: accepted.
        ctx.emit_outbound(OutboundToolEffect::SendMessage(
            copperclaw_mcp::SendMessageSpec {
                to: None,
                text: "report".into(),
            },
        ))
        .await
        .expect("first send accepted");
        // Second send: refused with a clear error.
        let err = ctx
            .emit_outbound(OutboundToolEffect::SendMessage(
                copperclaw_mcp::SendMessageSpec {
                    to: None,
                    text: "follow-up the model wants to send anyway".into(),
                },
            ))
            .await
            .expect_err("second send refused");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("one-shot") || msg.contains("already delivered"),
            "error should explain the gate: {msg}"
        );
    }

    #[tokio::test]
    async fn root_session_send_message_does_not_flip_gate() {
        // Root sessions (no `source_session_id`) are unaffected by
        // the one-shot gate — they can send as many messages as the
        // model produces.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone());
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        for i in 0..3 {
            ctx.emit_outbound(OutboundToolEffect::SendMessage(
                copperclaw_mcp::SendMessageSpec {
                    to: None,
                    text: format!("msg {i}"),
                },
            ))
            .await
            .expect("root sessions can send multiple messages");
        }
        assert!(!ctx.parent_reply_sent(), "root never flips the gate");
    }

    #[tokio::test]
    async fn emit_diff_writes_diff_kind_row_with_canonical_payload() {
        // The runner persists a `MessageKind::Diff` outbound row with
        // the canonical `DiffCard` under `content.diff`. The
        // host-delivery service's `dispatch_diff` arm pulls it out and
        // hands it to the adapter's `deliver_diff` hook.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let ctx = RunnerToolCtx::new(Arc::new(Mutex::new(conn)), paths.outbox.clone());
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        let card = copperclaw_channels_core::DiffCard {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Remove,
                        text: "old".into(),
                    },
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Add,
                        text: "new".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        ctx.emit_diff(card.clone()).await;
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Diff);
        let back: copperclaw_channels_core::DiffCard =
            serde_json::from_value(row.content["diff"].clone()).unwrap();
        assert_eq!(back, card);
        // Channel routing inherits from the originating inbound so the
        // delivery loop has a target.
        assert_eq!(row.platform_id.as_deref(), Some("chat-1"));
        assert_eq!(
            row.channel_type.as_ref().map(|c| c.as_str().to_owned()),
            Some("telegram".into())
        );
    }

    #[tokio::test]
    async fn emit_diff_writes_even_without_channel_routing_for_root_session() {
        // Mirror of emit_breadcrumb's same-gate test — 2026-05-24 fix.
        // Root sessions emit even when origin lacks channel routing;
        // delivery's session_routing wiring fallback fills in the user
        // channel at dispatch time.
        let (_tmp, ctx) = fresh_ctx();
        let card = copperclaw_channels_core::DiffCard {
            path: "x.rs".into(),
            language: None,
            hunks: vec![],
            added: 0,
            removed: 0,
            truncated: false,
        };
        ctx.emit_diff(card).await;
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        assert_eq!(rows.len(), 1, "root-session diff must land");
        assert_eq!(rows[0].kind, MessageKind::Diff);
    }

    #[tokio::test]
    async fn emit_diff_skips_for_child_session() {
        // Child sessions report up to another LLM, not a user.
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
        let card = copperclaw_channels_core::DiffCard {
            path: "x.rs".into(),
            language: None,
            hunks: vec![],
            added: 0,
            removed: 0,
            truncated: false,
        };
        ctx.emit_diff(card).await;
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        assert!(rows.is_empty(), "child-session diff must skip");
    }

    #[tokio::test]
    async fn emit_status_writes_for_root_session_with_null_routing() {
        // Direct regression on the specific incident: parent runner
        // processing an agent-dispatched inbound (NULL channel routing
        // on the row) must still produce a heartbeat status row.
        let (_tmp, ctx) = fresh_ctx();
        ctx.set_originating(OriginatingRouting {
            channel_type: None,
            platform_id: None,
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        ctx.emit_status("Still working on this — 60s in").await;
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        assert_eq!(rows.len(), 1, "root-session status must land");
        assert_eq!(rows[0].kind, MessageKind::Chat);
        // Routing fields NULL — delivery's session_routing fallback
        // fills them at dispatch.
        assert!(rows[0].channel_type.is_none());
        assert!(rows[0].platform_id.is_none());
    }

    #[tokio::test]
    async fn emit_status_skips_for_child_session() {
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
        ctx.emit_status("Still working — should not fire for child")
            .await;
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        assert!(rows.is_empty(), "child-session status must skip");
    }

    async fn last_row(ctx: &RunnerToolCtx) -> copperclaw_types::MessageOutRow {
        let guard = ctx.outbound.lock().await;
        let rows = copperclaw_db::tables::messages_out::list_due(&guard).unwrap();
        rows.into_iter().next_back().unwrap()
    }

    // ---------------------------------------------------------------
    // Slice 3.4 — long-output expander decorator tests.
    //
    // `build_expander_decorator` is the runner-side hot path: every
    // `apply_send_message` / `apply_send_file` call passes the chat
    // body through it before writing the outbound row. We verify the
    // line-vs-byte trigger, the summary shape, the preview cap, and
    // the round-trip into a row's `content.expander`.
    // ---------------------------------------------------------------
    #[test]
    fn expander_short_text_returns_none() {
        // Plain ten-line reply is well under both thresholds; no
        // decorator should be attached.
        let text = "alpha\nbeta\ngamma\ndelta\nepsilon";
        assert!(build_expander_decorator(text).is_none());
    }

    #[test]
    fn expander_empty_text_returns_none() {
        assert!(build_expander_decorator("").is_none());
    }

    #[test]
    fn expander_triggers_on_line_count_alone() {
        // 31 short lines — under the byte cap but over the line cap.
        let text = (0..31)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.len() < EXPANDER_BYTE_THRESHOLD);
        let v = build_expander_decorator(&text).expect("31 lines must trigger");
        assert_eq!(v["summary_kind"], "lines");
        let summary = v["summary"].as_str().unwrap();
        assert!(summary.contains("31 lines"), "got: {summary}");
        let preview = v["preview_lines"].as_array().unwrap();
        // Preview cap is 6 lines; even if more are available we only
        // surface six.
        assert_eq!(preview.len(), EXPANDER_PREVIEW_LINES);
        assert_eq!(preview[0], "l0");
        assert_eq!(preview[5], "l5");
    }

    #[test]
    fn expander_triggers_on_byte_count_alone() {
        // One huge single line — under the line cap but well over the
        // byte cap.
        let text = "x".repeat(EXPANDER_BYTE_THRESHOLD + 1);
        let v = build_expander_decorator(&text).expect("oversized text must trigger");
        assert_eq!(v["summary_kind"], "bytes");
        let summary = v["summary"].as_str().unwrap();
        assert!(summary.contains("bytes"), "got: {summary}");
        // Preview captures the single line (it's all one line).
        let preview = v["preview_lines"].as_array().unwrap();
        assert_eq!(preview.len(), 1);
    }

    #[test]
    fn expander_prefers_lines_when_both_triggers_fire() {
        // Long enough lines and enough of them to exceed BOTH the byte
        // and line thresholds — tie-break is `lines` because it's the
        // more human-meaningful unit. Sized at 1000 × 80-char lines so
        // the byte total clears any future bump of `EXPANDER_BYTE_THRESHOLD`
        // up to ~80 KB.
        let line: String = "x".repeat(80);
        let text = (0..1000)
            .map(|_| line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.len() > EXPANDER_BYTE_THRESHOLD);
        assert!(text.lines().count() > EXPANDER_LINE_THRESHOLD);
        let v = build_expander_decorator(&text).expect("both triggers fire");
        assert_eq!(v["summary_kind"], "lines");
    }

    #[test]
    fn expander_below_threshold_byte_boundary() {
        // Single line, exactly at the byte threshold (not >). Must
        // NOT trigger — threshold is `> 4 KB`, not `>= 4 KB`.
        let text = "x".repeat(EXPANDER_BYTE_THRESHOLD);
        assert!(build_expander_decorator(&text).is_none());
    }

    #[test]
    fn expander_at_line_threshold_does_not_trigger() {
        // Exactly 30 lines must NOT trigger — threshold is `> 30`.
        let text = (0..EXPANDER_LINE_THRESHOLD)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(build_expander_decorator(&text).is_none());
    }

    #[test]
    fn expander_decorator_shape_validates() {
        // The decorator payload MUST have `summary` (string),
        // `summary_kind` (string), and `preview_lines` (array of
        // strings) so the host-delivery service can read it back
        // without a defensive decode loop.
        let text = (0..40)
            .map(|i| format!("L{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let v = build_expander_decorator(&text).unwrap();
        assert!(v.get("summary").and_then(|s| s.as_str()).is_some());
        assert!(v.get("summary_kind").and_then(|s| s.as_str()).is_some());
        let preview = v.get("preview_lines").and_then(|p| p.as_array()).unwrap();
        for entry in preview {
            assert!(entry.is_string());
        }
    }

    #[tokio::test]
    async fn send_message_attaches_expander_when_long() {
        // End-to-end: an `apply_send_message` call with a long body
        // writes a Chat-kind row with both `content.text` (the full
        // body) AND `content.expander` (the decorator). The decorator
        // is what tells the host-delivery service to route through
        // `deliver_collapsible`.
        let (_tmp, ctx) = fresh_ctx();
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        let long_text = (0..50)
            .map(|i| format!("output line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ctx.emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
            to: None,
            text: long_text.clone(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Chat);
        // Full body preserved on `content.text`.
        assert_eq!(row.content["text"].as_str().unwrap(), long_text);
        // Decorator attached on `content.expander`.
        let exp = &row.content["expander"];
        assert_eq!(exp["summary_kind"], "lines");
        assert!(exp["summary"].as_str().unwrap().contains("50 lines"));
        let preview = exp["preview_lines"].as_array().unwrap();
        assert_eq!(preview.len(), EXPANDER_PREVIEW_LINES);
    }

    #[tokio::test]
    async fn send_message_skips_expander_when_short() {
        // Short messages must NOT carry an `expander` field — keeps
        // the wire payload minimal and the host-delivery service's
        // hot path identical to today's behaviour for normal chats.
        let (_tmp, ctx) = fresh_ctx();
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        ctx.emit_outbound(OutboundToolEffect::SendMessage(SendMessageSpec {
            to: None,
            text: "short reply.".into(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert!(
            row.content.get("expander").is_none(),
            "got: {:?}",
            row.content
        );
    }

    #[tokio::test]
    async fn send_file_attaches_expander_when_caption_is_long() {
        // `send_file` may carry a caption alongside the bytes; we
        // mirror the message-side decoration so long captions still
        // get the disclosure treatment.
        let (_tmp, ctx) = fresh_ctx();
        ctx.set_originating(OriginatingRouting {
            channel_type: Some("telegram".into()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        let long_caption: String = (0..40)
            .map(|i| format!("notes line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ctx.emit_outbound(OutboundToolEffect::SendFile(SendFileSpec {
            to: None,
            filename: "report.txt".into(),
            data: b"placeholder".to_vec(),
            text: Some(long_caption.clone()),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Chat);
        assert!(row.content.get("expander").is_some());
        let exp = &row.content["expander"];
        assert!(exp["summary"].as_str().unwrap().contains("40 lines"));
    }

    #[test]
    fn strip_reasoning_drops_thinking_blocks() {
        let raw =
            "<thinking>\nThe user wants X.\nI should do Y.\n</thinking>\n\nHere is the answer.";
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
    async fn send_card_writes_card_kind_row() {
        // Wave 2 contract: `send_card` writes a `MessageKind::Card` row to
        // messages_out with the canonical Card serialised under
        // `content.card`. The host-delivery service deserialises it back
        // into `copperclaw_channels_core::Card` and hands it to the
        // adapter's `deliver_card` hook.
        let (_tmp, ctx) = fresh_ctx();
        let card = copperclaw_channels_core::Card {
            title: Some("Order #42".into()),
            body: Some("Confirm?".into()),
            fields: vec![copperclaw_channels_core::CardField {
                label: "Item".into(),
                value: "Espresso".into(),
                inline: false,
            }],
            buttons: vec![copperclaw_channels_core::CardButton {
                label: "Confirm".into(),
                value: Some("confirm:42".into()),
                url: None,
                style: Some("primary".into()),
            }],
            image_url: Some("https://example.com/x.png".into()),
        };
        ctx.emit_outbound(OutboundToolEffect::SendCard(SendCardSpec {
            to: None,
            card: card.clone(),
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        // Kind is Card (not System / not Chat).
        assert_eq!(row.kind, MessageKind::Card);
        // Content carries the canonical Card payload under `card` and
        // round-trips through serde back into the same struct.
        let parsed: copperclaw_channels_core::Card =
            serde_json::from_value(row.content["card"].clone()).unwrap();
        assert_eq!(parsed, card);
        // No `to` field on the row when the caller didn't pass one.
        assert!(row.content.get("to").is_none());
    }

    #[tokio::test]
    async fn send_card_carries_explicit_to_in_content() {
        // When the caller supplies an explicit `to:`, it is preserved in
        // `content.to` so sibling agents / DM-opening adapters can read
        // the routing override from the row.
        let (_tmp, ctx) = fresh_ctx();
        let card = copperclaw_channels_core::Card {
            title: Some("Hi".into()),
            ..Default::default()
        };
        ctx.emit_outbound(OutboundToolEffect::SendCard(SendCardSpec {
            to: Some(Recipient::User { id: "u-1".into() }),
            card,
        }))
        .await
        .unwrap();
        let row = last_row(&ctx).await;
        assert_eq!(row.kind, MessageKind::Card);
        assert_eq!(row.content["to"]["kind"], "user");
        assert_eq!(row.content["to"]["id"], "u-1");
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
        assert_eq!(
            row.content["install_packages"]["apt"],
            serde_json::json!(["jq"])
        );
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
    use copperclaw_providers::{AgentProvider, AgentQuery, ProviderError, QueryInput};
    use copperclaw_types::ProviderEvent;
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
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
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
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
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
        let ctx = RunnerToolCtx::new(outbound, paths.outbox.clone()).with_subagent(deps);
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
