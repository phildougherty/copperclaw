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
/// default destination" (the parent agent for spawned child sessions, the
/// originating channel otherwise); the runner is responsible for
/// materialising that default. When the caller supplies `to`, it is one of
/// the variants below.
///
/// [`Recipient::Parent`] and [`Recipient::RootUser`] are *virtual*
/// recipients (phase 2 of `docs/plans/agent-to-agent-routing.md`): the
/// runner resolves them into a concrete route before any outbound row is
/// written, so they never appear on the wire — `Parent` becomes an
/// `Agent { session_id }` addressed at the session's `source_session_id`,
/// and `RootUser` becomes a chat-kind row delivered on the root
/// conversation's human channel.
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
    /// The agent that spawned this session (`to: "agent:parent"`).
    /// Resolved by the runner against the session's `source_session_id`;
    /// a session with no parent gets a validation error.
    Parent,
    /// The human in the ROOT conversation of the spawn chain
    /// (`to: "user"`). Resolved by the runner to the originating human
    /// channel: the triggering inbound's channel routing when present,
    /// otherwise the session's host-written `session_routing` — which
    /// the host copies down the spawn chain from the root session, so a
    /// grandchild's `to: "user"` reaches the human, not a middle agent.
    RootUser,
}

impl Recipient {
    /// Parse the shorthand string forms of a `to` argument.
    ///
    /// - `"user"` -> [`Recipient::RootUser`] (the human at the root of
    ///   the spawn chain).
    /// - `"agent:parent"` -> [`Recipient::Parent`] (the spawning agent).
    /// - anything else -> [`Recipient::Channel`] with the string as the
    ///   channel id (the historical bare-string behaviour).
    #[must_use]
    pub fn from_to_string(s: &str) -> Self {
        match s.trim() {
            "user" => Self::RootUser,
            "agent:parent" => Self::Parent,
            other => Self::Channel {
                id: other.to_string(),
            },
        }
    }
}

/// Where an `install_packages` request applies.
///
/// `Image` (the historical default) records the packages into the group's
/// pending `container_configs` so the *next* container spawn bakes them into
/// the image. `Session` additionally runs an ecosystem-appropriate LOCAL
/// install into the session's persistent `/data` **now** (a venv for pip, a
/// prefixed global for npm) so the package is usable in the *current* session
/// without waiting for a rebuild — the "works now, permanent later" default
/// the agent actually wants. Bakeable ecosystems (apt/npm) are still recorded
/// for the next image even under `Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InstallScope {
    /// Record into the pending image config; applies at the next spawn.
    #[default]
    Image,
    /// Install into the session's `/data` now AND record bakeables for the
    /// next image.
    Session,
}

impl InstallScope {
    /// Stable token for the wire payload / logging.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Session => "session",
        }
    }
}

/// A single self-mod install request. The runner translates this into an
/// approval request and ultimately a privileged action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InstallSpec {
    /// `apt` package names.
    pub apt: Vec<String>,
    /// `npm` package names.
    pub npm: Vec<String>,
    /// Human-readable reason for the install (required, audited).
    pub reason: String,
    /// Where the request applies (see [`InstallScope`]). Defaults to
    /// [`InstallScope::Image`] for backward compatibility.
    #[serde(default)]
    pub scope: InstallScope,
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

/// Spec for `save_skill` (M19 A4). The agent proposes a reusable skill; the
/// runner records it and the host raises an approval. On approval the host
/// validates and writes the `SKILL.md` into the group's per-group skills
/// override directory, where the next container spawn discovers it. This is a
/// per-group capability, never cross-group sharing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveSkillSpec {
    /// Kebab-case skill name; becomes the on-disk directory slug and must
    /// equal the frontmatter `name`.
    pub name: String,
    /// The full `SKILL.md` text including its YAML frontmatter. Validated by
    /// the tool before emission and again host-side before the write.
    pub content: String,
    /// Human-readable reason for saving the skill (audited on the approval).
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

/// Spec for `delegate` — the middle-tier write-capable build worker.
///
/// A `delegate` is lighter and more contained than a `create_agent`
/// sibling: it is NOT wired into any channel and it reports ONLY back to
/// the spawning parent (never into the user's chat), but — unlike the
/// read-only `explore` subagent — it gets a WRITABLE git worktree of the
/// parent's repo (same mechanics as `create_agent`) so it can build and
/// commit in isolation. It carries no `channel` field precisely because
/// it is never user-facing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateSpec {
    /// Display name for the worker (also the folder slug seed).
    pub name: String,
    /// Build instructions / task for the worker.
    pub instructions: String,
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

/// Spec for a task capability grant authored alongside `schedule_task`
/// (M22 A1). The agent proposes a bounded authorization for the task it is
/// scheduling; the runner records it and the host raises an approval card
/// (reusing the `save_skill` round-trip, decision (c)). ONLY on operator
/// approval does a `task_grants` row persist — so a pending grant never
/// authorizes anything.
///
/// The grant references the task by `task_name` because the concrete task id is
/// assigned host-side and is not known when the tool runs; the host resolves
/// the id when it raises the approval. Grants are bounded (decision (b)):
/// `expires_at` is required (no standing grants) and at least one of a token or
/// fire bound should be set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorTaskGrantSpec {
    /// Name of the task being scheduled in the same call; the host binds the
    /// grant to that task's id.
    pub task_name: String,
    /// Space-separated set of capability-scope tokens (`class` or
    /// `class:resource`) the grant authorizes. See
    /// `copperclaw_db::tables::task_grants` for the matching grammar.
    pub capability_scope: String,
    /// Max tokens the grant may spend across its fires; `None` = no token bound.
    pub token_budget: Option<i64>,
    /// Max autonomous fires the grant authorizes; `None` = no fire bound.
    pub max_fires: Option<i64>,
    /// Absolute expiry instant (required by the tool — decision (b): no
    /// non-expiring grants).
    pub expires_at: DateTime<Utc>,
    /// Human-readable reason shown on the approval card.
    pub reason: String,
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

/// Spec for `create_goal` (M22 A3). The agent declares a durable long-running
/// objective the sweep will drive check-ins against. Persisted host-side into
/// the central `goals` table (decision (d): the goal INDEXES over the todo /
/// memory stores, it does not replace them). Not approval-gated — a goal is
/// internal state, it authorizes nothing on its own (any autonomous ACTION a
/// check-in wake takes is still gated by A2's grant machinery).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGoalSpec {
    /// The durable objective text (what the goal is for).
    pub objective: String,
    /// Optional cron (croner) the sweep re-arms the next check-in from.
    pub checkin_recurrence: Option<String>,
    /// Optional first check-in instant. When omitted and `checkin_recurrence`
    /// is set, the host computes the first fire from the recurrence.
    pub first_checkin: Option<DateTime<Utc>>,
    /// Optional prompt injected on a check-in wake; `None` = the sweep
    /// synthesises one from the objective.
    pub checkin_prompt: Option<String>,
    /// The goal's own cumulative token cap (used only when no grant is linked).
    pub token_budget: Option<i64>,
}

/// Spec for `update_goal` (M22 A3). Records progress and/or moves the goal's
/// lifecycle state. At least one of `status` / `progress` / `objective` is set
/// (validated by the tool). `status` is one of `active`/`paused`/`completed`/
/// `abandoned`; illegal transitions are rejected host-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateGoalSpec {
    /// Goal id (assigned host-side; the agent learns it from a check-in wake).
    pub id: String,
    /// New lifecycle status, if transitioning.
    pub status: Option<String>,
    /// A progress note to append to the goal's log, if reporting progress.
    pub progress: Option<String>,
    /// Tokens attributed to this progress report (accrued cumulatively), if any.
    pub progress_tokens: Option<i64>,
    /// A revised objective, if refining the goal.
    pub objective: Option<String>,
}

/// A description of one goal as surfaced by `list_goals` (M22 A3). Runner-facing
/// view; the host owns the full row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSummary {
    /// Goal id.
    pub id: String,
    /// Objective text.
    pub objective: String,
    /// Lifecycle status (`active`/`paused`/`completed`/`abandoned`).
    pub status: String,
    /// Next scheduled check-in, if any.
    pub next_checkin: Option<DateTime<Utc>>,
    /// Count of check-in fires so far.
    pub checkin_count: i64,
    /// Cumulative tokens accrued against the goal.
    pub tokens_consumed: i64,
}

/// Spec for `register_condition` (M22 A4). Registers — or, when `remove` is
/// set, deregisters — a durable HEARTBEAT-style condition the sweep fires a
/// `kind:task` check-in wake for on the RISING edge of its predicate. Persisted
/// host-side into the central `conditions` table (mirroring the `create_goal`
/// path). A condition is internal tracking state — it authorizes nothing on its
/// own; any autonomous ACTION the woken turn takes stays gated by A2's grant
/// machinery at fire time (`grant_id` records the optional linkage).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterConditionSpec {
    /// Agent-chosen stable key. Re-registering the same id replaces it.
    pub id: String,
    /// `pending_inbound` | `idle` | `flag` — the observable tested.
    pub kind: String,
    /// `min` pending count (`pending_inbound`) or idle-seconds floor (`idle`);
    /// `None` for `flag`.
    pub threshold: Option<i64>,
    /// Watched flag name for `flag`; `None` otherwise.
    pub flag: Option<String>,
    /// Prompt delivered to the woken agent on a fire (required for register).
    pub prompt: Option<String>,
    /// Optional A2 fire-time grant linkage.
    pub grant_id: Option<String>,
    /// When true, deregister the condition `id` instead of registering.
    pub remove: bool,
}

/// Spec for `set_condition_flag` (M22 A4). Sets or clears a named per-session
/// latch the `flag` condition kind watches — the settable signal that lets a
/// `flag` condition fire. Persisted host-side into `condition_flags`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetConditionFlagSpec {
    /// The flag name to set or clear.
    pub flag: String,
    /// `true` = raise the flag (set), `false` = lower it (clear).
    pub value: bool,
}

/// One memory hit surfaced to the `memory_search` / `memory_get` tools.
///
/// `provenance` is the wire form (`"trusted"` / `"untrusted"`) of the stored
/// entry's tag (see `copperclaw_db::memory::Provenance`). The runner marks the
/// turn tainted when any returned hit is untrusted, so the coarse approval gate
/// blocks credentialed external actions until fresh approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHitView {
    /// Logical key of the entry.
    pub key: String,
    /// Stored text body.
    pub body: String,
    /// `"trusted"` | `"untrusted"`.
    pub provenance: String,
    /// Optional source label (e.g. `"web_fetch:https://..."`).
    pub source: Option<String>,
    /// Blended relevance score (FTS5 + cosine), higher is better. `None` for a
    /// direct `memory_get` (no ranking).
    pub score: Option<f64>,
    /// RFC3339 last-updated timestamp.
    pub updated_at: String,
}

/// Spec for the `memory_search` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySearchSpec {
    /// Free-text query matched against the FTS5 index.
    pub query: String,
    /// Maximum hits to return (the context clamps to a sane ceiling).
    pub limit: Option<usize>,
}

/// Spec for the `memory_save` tool: an agent-initiated write into the group
/// memory store. Deliberately carries NO provenance field — the agent cannot
/// request a provenance. The provenance is decided by the side-effecting
/// context from the current turn's taint state (see
/// [`resolve_save_provenance`]) so an untrusted turn can never launder content
/// into `trusted` memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySaveSpec {
    /// Logical key to upsert under (overwrites an existing entry of the same
    /// key). Trimmed; must be non-empty and within the key-length cap.
    pub key: String,
    /// The text body to remember. Must be non-empty and within the body-size
    /// cap.
    pub body: String,
    /// Optional short source label recorded with the entry (e.g. `"agent"` or
    /// a note about where the fact came from).
    pub source: Option<String>,
}

/// Outcome of a `memory_save`, returned to the agent so it can see the honest
/// provenance the store recorded (which may differ from `trusted` when the
/// turn was tainted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySaveOutcome {
    /// The key that was written.
    pub key: String,
    /// The effective provenance recorded: `"trusted"` or `"untrusted"`.
    pub provenance: String,
    /// True when the requested `trusted` write was forced down to `untrusted`
    /// because the current turn was tainted by untrusted-provenance content.
    pub downgraded: bool,
}

/// Resolve the honest provenance for an agent-initiated memory write given the
/// current turn's taint state. A tainted turn can NEVER write `trusted`
/// memory: the entry is forced to `untrusted` (a downgrade) so external
/// content pulled into the turn can't be laundered into trusted memory.
///
/// Returns `(provenance_wire, downgraded)`. This is the single reference for
/// the taint→provenance rule; both the runner's `ToolContext` impl and the
/// unit-test mock call it so the two can't drift.
#[must_use]
pub fn resolve_save_provenance(tainted: bool) -> (&'static str, bool) {
    if tainted {
        ("untrusted", true)
    } else {
        ("trusted", false)
    }
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
/// `copperclaw-channels-core`. The runner serialises this directly into a
/// `MessageKind::Card` outbound row; the delivery service deserialises it
/// back into [`copperclaw_channels_core::Card`] and hands it to the
/// adapter's `deliver_card` hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendCardSpec {
    /// Recipient; `None` means "reply on origin channel".
    pub to: Option<Recipient>,
    /// Canonical card. Validated against the schema at construction time
    /// by the `send_card` MCP tool — anything that reaches the runner is
    /// guaranteed to pass [`copperclaw_channels_core::Card::validate`].
    pub card: copperclaw_channels_core::Card,
}

/// Spec for the host-side `TodoList` emit triggered by every
/// `todo_add` / `todo_update` / `todo_delete` MCP tool handler. The
/// runner serialises this into a `MessageKind::TodoList` outbound
/// row (routed to the originating inbound channel); the delivery
/// service deserialises it back into
/// [`copperclaw_channels_core::TodoList`] and hands it to the adapter's
/// `deliver_todo_list` hook for native rendering and (where supported)
/// pinning. The agent never sees this spec — there's no MCP tool
/// named `emit_todo_list`; the runner emits it implicitly after each
/// mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmitTodoListSpec {
    /// Canonical todo list. Validated against the schema at
    /// construction time by the `todo_*` MCP tool handlers — anything
    /// that reaches the runner is guaranteed to pass
    /// [`copperclaw_channels_core::TodoList::validate`].
    pub list: copperclaw_channels_core::TodoList,
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
    /// Host-side `MessageKind::TodoList` emit triggered by every
    /// mutating `todo_*` MCP-tool handler. NOT a model-facing tool
    /// (the model continues to call `todo_add` / `todo_update` /
    /// `todo_delete` unchanged) — the handler builds the full
    /// post-mutation list and routes it through this effect so the
    /// delivery service can render it natively via
    /// `deliver_todo_list` and pin it on platforms that support it.
    EmitTodoList(EmitTodoListSpec),
    /// `create_agent`.
    CreateAgent(CreateAgentSpec),
    /// `delegate` — spawn a write-capable, parent-only build worker.
    Delegate(DelegateSpec),
    /// `install_packages`.
    InstallPackages(InstallSpec),
    /// `add_mcp_server`.
    AddMcpServer(AddMcpServerSpec),
    /// `save_skill` — persist an agent-authored skill into the group's
    /// per-group skills override (approval-gated host-side).
    SaveSkill(SaveSkillSpec),
    /// `schedule_task`.
    ScheduleTask(ScheduleSpec),
    /// Grant authoring emitted alongside `schedule_task` when the caller
    /// supplies a `grant` (M22 A1). Approval-gated host-side: raises a card and
    /// persists a `task_grants` row only on operator approval.
    AuthorTaskGrant(AuthorTaskGrantSpec),
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
    /// `create_goal` (M22 A3) — persist a durable long-running goal into the
    /// central `goals` table host-side.
    CreateGoal(CreateGoalSpec),
    /// `update_goal` (M22 A3) — record progress and/or transition a goal's
    /// lifecycle state host-side.
    UpdateGoal(UpdateGoalSpec),
    /// `register_condition` (M22 A4) — persist / deregister a durable
    /// HEARTBEAT-style condition in the central `conditions` table.
    RegisterCondition(RegisterConditionSpec),
    /// `set_condition_flag` (M22 A4) — set / clear a per-session flag latch the
    /// `flag` condition kind watches.
    SetConditionFlag(SetConditionFlagSpec),
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
            Self::EmitTodoList(_) => "emit_todo_list",
            Self::CreateAgent(_) => "create_agent",
            Self::Delegate(_) => "delegate",
            Self::InstallPackages(_) => "install_packages",
            Self::AddMcpServer(_) => "add_mcp_server",
            Self::SaveSkill(_) => "save_skill",
            Self::ScheduleTask(_) => "schedule_task",
            Self::AuthorTaskGrant(_) => "author_task_grant",
            Self::ListTasks => "list_tasks",
            Self::CancelTask { .. } => "cancel_task",
            Self::PauseTask { .. } => "pause_task",
            Self::ResumeTask { .. } => "resume_task",
            Self::UpdateTask(_) => "update_task",
            Self::CreateGoal(_) => "create_goal",
            Self::UpdateGoal(_) => "update_goal",
            Self::RegisterCondition(_) => "register_condition",
            Self::SetConditionFlag(_) => "set_condition_flag",
        }
    }
}

/// A description of a single task as surfaced by `list_tasks`.
///
/// This is a runner-facing view, intentionally simpler than
/// `copperclaw_types::ScheduledTask` so the context impl can build it from
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

/// Hard cap on the number of workers a single `delegate_batch` may fan
/// out. A batch is a *bounded* join: each worker is a full container with
/// its own writable `sib/<id>` worktree, and the parent blocks on all of
/// them and folds every report into its context — so an unbounded width
/// would swamp the host and blow the parent's context on join. The looser,
/// unbounded async fan-out (`delegate` called N times, results arriving
/// asynchronously) is still available for wider spreads.
pub const MAX_DELEGATE_BATCH_WIDTH: usize = 6;

/// Default wall-clock budget for a `delegate_batch` join when the caller
/// omits `timeout_secs`. Bounded well under the runner's per-tool deadline
/// so the join returns a partial aggregate (per-worker timeout errors)
/// rather than being hard-aborted mid-call.
pub const DEFAULT_DELEGATE_BATCH_TIMEOUT_SECS: u64 = 300;

/// Hard ceiling on the `delegate_batch` join budget. A caller may not ask
/// to block longer than this; still comfortably under the runner's
/// per-tool deadline (`DEFAULT_TOOL_DEADLINE_SECS`).
pub const MAX_DELEGATE_BATCH_TIMEOUT_SECS: u64 = 600;

/// One worker in a [`DelegateBatchRequest`]. Same two fields as a single
/// `delegate` (no channel — a delegate is never user-facing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateBatchWorker {
    /// Display name / folder-slug seed for the worker.
    pub name: String,
    /// Build instructions for the worker (point it at `/workspace`).
    pub instructions: String,
}

/// Request handed to [`ToolContext::run_delegate_batch`] by the
/// `delegate_batch` tool. Widths and budgets are already validated /
/// clamped by the tool handler before this is constructed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateBatchRequest {
    /// The workers to fan out (1..=[`MAX_DELEGATE_BATCH_WIDTH`]).
    pub workers: Vec<DelegateBatchWorker>,
    /// Wall-clock join budget in seconds (clamped to
    /// [`MAX_DELEGATE_BATCH_TIMEOUT_SECS`]).
    pub timeout_secs: u64,
}

/// Terminal status of one worker in a joined [`DelegateBatchOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    /// The worker spawned, ran, and reported back within budget.
    Ok,
    /// The host refused to spawn the worker (depth cap, permission gate,
    /// or an invalid spawn) — the worker never ran.
    SpawnFailed,
    /// The worker spawned but did not report within the join budget.
    Timeout,
}

/// One worker's outcome in a joined [`DelegateBatchOutcome`], in request
/// order. A failed worker surfaces as an `error` here rather than losing
/// the whole batch turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerOutcome {
    /// The worker name the caller supplied.
    pub name: String,
    /// Terminal status.
    pub status: WorkerStatus,
    /// The worker's consolidated report (its single `send_message` back to
    /// the parent). `Some` iff `status == Ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// A short reason when the worker did not produce a report
    /// (spawn-gate refusal or timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The spawned child session id, when one was assigned (for the
    /// caller to `git diff main..sib/<id>` / merge the worker's branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// The aggregated result of a `delegate_batch` join: one [`WorkerOutcome`]
/// per requested worker, in request order. Returned as ONE tool response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateBatchOutcome {
    /// Per-worker outcomes, in request order.
    pub workers: Vec<WorkerOutcome>,
}

impl DelegateBatchOutcome {
    /// True when EVERY worker failed to spawn (e.g. the whole batch was
    /// refused by the subagent depth cap). The `delegate_batch` tool
    /// surfaces this as a tool error — a clean refusal — rather than a
    /// partial aggregate.
    #[must_use]
    pub fn all_spawn_failed(&self) -> bool {
        !self.workers.is_empty()
            && self
                .workers
                .iter()
                .all(|w| w.status == WorkerStatus::SpawnFailed)
    }

    /// Count of workers that reported successfully.
    #[must_use]
    pub fn completed(&self) -> usize {
        self.workers
            .iter()
            .filter(|w| w.status == WorkerStatus::Ok)
            .count()
    }
}

/// Channel routing of the inbound currently being processed, as exposed
/// through [`ToolContext::originating_channel`]. A read-only snapshot of
/// what [`ToolContext::set_originating`] stashed — consumers (the Task
/// HUD) key channel-capability decisions off it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginatingChannel {
    /// Channel type string (`"telegram"`, `"slack"`, `"cli"`, …).
    pub channel_type: String,
    /// Platform-native conversation id, when the inbound carried one.
    pub platform_id: Option<String>,
    /// Platform-native thread id, when the inbound carried one.
    pub thread_id: Option<String>,
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
    async fn emit_outbound(&self, effect: OutboundToolEffect) -> Result<ToolEffectAck, ToolError>;

    /// Convenience hook so `list_tasks` does not need a separate request/ack
    /// pathway. Implementors may delegate to whatever scheduler state they
    /// own; the mock keeps a synthetic table.
    async fn list_tasks(&self) -> Result<Vec<TaskSummary>, ToolError>;

    /// Read hook for the `list_goals` MCP tool (M22 A3). Goal state lives on the
    /// host (central `goals` table), not the container, so the runner's context
    /// returns an empty `Vec` (exactly like `list_tasks`); the mock returns
    /// whatever `set_goals` seeded. Default `Ok(Vec::new())` so contexts that
    /// don't track goals (subagent adapters) compile unchanged.
    async fn list_goals(&self) -> Result<Vec<GoalSummary>, ToolError> {
        Ok(Vec::new())
    }

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
    async fn spawn_subagent(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError> {
        let _ = req;
        Err(ToolError::Context(
            "subagent not supported in this context".into(),
        ))
    }

    /// Spawn a bounded batch of `delegate` workers in isolated `sib/<id>`
    /// worktrees and BLOCK until all report (or the join budget elapses),
    /// returning one [`WorkerOutcome`] per worker in request order.
    ///
    /// This is the runner-side JOIN SEAM behind the `delegate_batch` tool
    /// (M19 A1). It reuses the exact single-`delegate` spawn machinery —
    /// same depth/permission caps, same containment (each worker lands
    /// with a NULL messaging group and reports ONLY back to the parent,
    /// never into the user's chat) — but adds a synchronous gather so a
    /// parent can fan out N parallel builds and assemble their joined
    /// output in ONE turn, rather than the N-separate-`delegate` calls
    /// whose results arrive asynchronously on later turns.
    ///
    /// The implementation block-polls the parent's own `inbound.db` for
    /// each worker's spawn result and its final report, exactly analogous
    /// to how an external-MCP call block-polls the host's response
    /// (`run::external_mcp`).
    ///
    /// Default impl returns `ToolError::Context` so mock / subagent
    /// contexts (which cannot poll the parent's inbound.db) compile
    /// unchanged — only the runner's `RunnerToolCtx` overrides it.
    async fn run_delegate_batch(
        &self,
        req: DelegateBatchRequest,
    ) -> Result<DelegateBatchOutcome, ToolError> {
        let _ = req;
        Err(ToolError::Context(
            "delegate_batch not supported in this context".into(),
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
    fn set_originating(
        &self,
        channel_type: Option<&str>,
        platform_id: Option<&str>,
        thread_id: Option<&str>,
        in_reply_to: Option<&str>,
    ) {
        let _ = (channel_type, platform_id, thread_id, in_reply_to);
    }

    /// Clear the originating-routing stash. Called by the runner
    /// after the turn completes so a subsequent emit on this ctx
    /// (e.g. a host-side apology write) doesn't inherit stale
    /// routing.
    fn clear_originating(&self) {}

    /// Record the `allowed-tools` of the skill the agent just loaded via
    /// the `load_skill` tool (the FUEL for the runner's
    /// `ToolPolicy::with_active_skill` layer). `Some(names)` narrows the
    /// live dispatch policy so a skill declaring `allowed-tools: [Read]`
    /// blocks `shell`; `None` (a skill with no declared `allowed-tools`)
    /// leaves the policy unscoped. Names are already normalized to
    /// copperclaw MCP tool names by the host's catalogue writer.
    ///
    /// Default no-op so contexts that don't gate tools (mocks, subagent
    /// adapters) compile unchanged. The runner's `RunnerToolCtx`
    /// overrides this to stash the value for [`Self::active_skill_allowed_tools`].
    fn set_active_skill_allowed_tools(&self, allowed: Option<Vec<String>>) {
        let _ = allowed;
    }

    /// The active skill's `allowed-tools` (set by the most recent
    /// `load_skill` that declared one), or `None` when no tool-scoping
    /// skill is active. The runner reads this at every dispatch and feeds
    /// it into `ToolPolicy::with_active_skill` so the loaded skill's
    /// scope narrows the live policy.
    ///
    /// Default `None` so non-gating contexts compile unchanged.
    fn active_skill_allowed_tools(&self) -> Option<Vec<String>> {
        None
    }

    /// Hybrid search of this group's searchable memory store (FTS5 +
    /// cosine over stored embeddings). Returns ranked hits.
    ///
    /// Default impl returns `ToolError::Context("memory store not configured
    /// in this context")` so mock / subagent contexts compile unchanged — only
    /// the runner's `RunnerToolCtx` (which knows the per-group `memory.db`
    /// path) overrides it. The runner-side impl ALSO marks the current turn
    /// tainted when any returned hit is untrusted, wiring the coarse
    /// provenance gate.
    async fn memory_search(&self, spec: MemorySearchSpec) -> Result<Vec<MemoryHitView>, ToolError> {
        let _ = spec;
        Err(ToolError::Context(
            "memory store not configured in this context".into(),
        ))
    }

    /// Fetch one memory entry by its logical key. `Ok(None)` when absent.
    /// Same default + taint semantics as [`Self::memory_search`].
    async fn memory_get(&self, key: &str) -> Result<Option<MemoryHitView>, ToolError> {
        let _ = key;
        Err(ToolError::Context(
            "memory store not configured in this context".into(),
        ))
    }

    /// Persist a fact into this group's memory store on the agent's behalf
    /// (the write half of [`Self::memory_search`] / [`Self::memory_get`]).
    ///
    /// PROVENANCE (security-critical): the implementation decides the recorded
    /// provenance from the current turn's taint state via
    /// [`resolve_save_provenance`] — the agent cannot request a provenance and
    /// a tainted turn is forced to `untrusted` (downgrade), so nothing lets an
    /// untrusted turn launder content into trusted memory. The impl also owns
    /// the per-session rate cap and embedding generation (deferred today — the
    /// runner writes text-only, exactly as `memory_search` reads text-only).
    ///
    /// Default impl returns `ToolError::Context` so mock / subagent contexts
    /// compile unchanged — only the runner's `RunnerToolCtx` (which knows the
    /// per-group `memory.db` path) overrides it.
    async fn memory_save(&self, spec: MemorySaveSpec) -> Result<MemorySaveOutcome, ToolError> {
        let _ = spec;
        Err(ToolError::Context(
            "memory store not configured in this context".into(),
        ))
    }

    /// Mark the current turn's context as carrying **untrusted-provenance**
    /// content, e.g. the body of a `web_fetch` or a third-party tool output.
    /// `source` is a short label for audit/logging (e.g. the fetched URL).
    ///
    /// The runner's `ToolContext` impl flips a per-turn taint flag that its
    /// dispatch gate then consults: once a turn is tainted, credentialed
    /// external actions are blocked until fresh approval (the coarse
    /// provenance gate). Default no-op so mock / subagent contexts compile
    /// unchanged — they simply don't enforce the gate.
    ///
    /// This is necessarily COARSE: taint cannot propagate through the model,
    /// so the runner treats *any* untrusted content in the turn as tainting
    /// the *whole* turn rather than attempting per-value tracking.
    fn mark_untrusted_context(&self, source: &str) {
        let _ = source;
    }

    /// Whether the current turn's context has been tainted by
    /// untrusted-provenance content (see [`Self::mark_untrusted_context`]).
    /// The runner's dispatch gate reads this to build the per-call provenance
    /// gate. Default `false` (no taint tracking) so non-gating contexts
    /// compile unchanged.
    fn is_context_tainted(&self) -> bool {
        false
    }

    /// Whether the current turn is an autonomous one (heartbeat / scheduled
    /// wake) with no triggering human message. Autonomous turns may search
    /// memory and propose, but may NOT take a credentialed external action
    /// (read-then-propose). Default `false` so non-gating contexts treat every
    /// turn as human-driven (no extra restriction).
    fn is_autonomous_turn(&self) -> bool {
        false
    }

    /// Whether a fresh operator approval has cleared the provenance taint for
    /// credentialed external actions on this turn. Default `false`: absent an
    /// explicit grant, a tainted turn stays blocked.
    fn external_action_approved(&self) -> bool {
        false
    }

    /// Set the per-turn provenance signals (`autonomous`, `approved`) the
    /// dispatch gate consults. Called by the runner's main loop before driving
    /// each turn. Default no-op so mock / subagent contexts compile unchanged.
    fn set_turn_provenance(&self, autonomous: bool, approved: bool) {
        let _ = (autonomous, approved);
    }

    /// True when this context represents a child agent session that
    /// has already emitted its first `send_message` to the parent.
    /// The runner's main loop checks this after every turn and exits
    /// cleanly when set — enforcing one-shot semantics for child
    /// agents at runtime instead of relying on the LLM to follow
    /// soft prompt rules like "EXACTLY ONE send_message."
    ///
    /// Default `false` so test / mock contexts compile unchanged.
    /// Root sessions also return `false` (the gate is only meaningful
    /// for child sessions spawned via `create_agent`).
    fn parent_reply_sent(&self) -> bool {
        false
    }

    /// Mark the start of a new per-inbound turn. Called by the runner at
    /// the top of each `drive_turn`; implementations reset any per-turn
    /// state (notably the coarse-provenance taint flag — see
    /// [`Self::mark_untrusted_context`]). Default no-op so mock /
    /// subagent contexts compile unchanged.
    fn begin_activity(&self) {}

    /// Channel routing of the inbound currently being processed, when it
    /// targets a real user channel. The Task HUD reads this to decide
    /// whether the originating channel can edit messages in place
    /// (`copperclaw_channels_core::capabilities`) and whether its typing
    /// indicator is visible on this surface. Returns `None` for contexts
    /// with no user-facing channel (mocks, subagent adapters, child-agent
    /// sessions whose recipient is another LLM) — the HUD then stays
    /// inactive. Default `None`.
    fn originating_channel(&self) -> Option<OriginatingChannel> {
        None
    }

    /// M18 Task HUD emit hook: one self-editing status message per
    /// inbound task. `first: true` posts the HUD (the runner writes a
    /// `MessageKind::Breadcrumb` row the delivery loop renders as a
    /// chip); `first: false` updates it in place (a `MessageKind::System`
    /// `update_breadcrumb` row the delivery loop resolves to an
    /// `edit_message` on adapters with an edit API). The breadcrumb's
    /// `tool_name` is the stable HUD anchor so every update edits the
    /// same platform message. Default no-op so mock / subagent contexts
    /// compile unchanged; best-effort in the runner impl (a failed write
    /// never aborts the turn).
    async fn emit_task_hud(&self, breadcrumb: &copperclaw_channels_core::Breadcrumb, first: bool) {
        let _ = (breadcrumb, first);
    }

    /// Slice-3.5 opt-in: emit a structured `MessageKind::Thinking` row
    /// carrying the model's just-completed reasoning block so the host
    /// delivery service can render it as a collapsed native UI
    /// primitive (Telegram `<blockquote expandable>`, Slack `context`
    /// block, Discord muted-grey embed, Google Chat
    /// `collapsibleSection`, Matrix `<details>`).
    ///
    /// `text` is the accumulated thinking prose (empty for redacted
    /// blocks); `redacted` mirrors the upstream `redacted_thinking`
    /// flag — renderers MUST substitute a placeholder for redacted
    /// blocks rather than display the raw blob. `model` is optional
    /// provenance (e.g. `"claude-opus-4-7"`) so the user can
    /// disambiguate which model produced the reasoning when their
    /// group fans out across several.
    ///
    /// The opt-in gate (per-group `surface_thinking`) is enforced by
    /// the runner BEFORE calling this method — implementations
    /// receive only events the operator has consented to surface.
    /// Default no-op so contexts that don't speak to a user channel
    /// (mocks, subagent adapters) compile unchanged.
    async fn emit_thinking(&self, text: &str, redacted: bool, model: Option<&str>) {
        let _ = (text, redacted, model);
    }

    /// Periodic "still working" status hook. The runner calls this
    /// from `drive_turn` after each tool turn when more than
    /// `STATUS_INTERVAL_SECS` of wall-clock has elapsed without a chat
    /// row going to the user, so a long tool-heavy stretch (or a long
    /// silent reasoning pass) doesn't look like the agent has hung.
    /// `text` is the pre-rendered status sentence — implementations
    /// route it as-is.
    ///
    /// Default no-op so test / mock contexts compile unchanged. The
    /// runner's `RunnerToolCtx` writes a `MessageKind::Chat` row to
    /// `outbound.db` against the originating channel — but ONLY when
    /// real user channel routing exists. Child-agent sessions (no
    /// channel routing, just a `source_session_id`) skip the emit
    /// because the recipient is another LLM and "still working"
    /// chatter would just bloat its history.
    async fn emit_status(&self, text: &str) {
        let _ = text;
    }

    /// Optional UX-observability hook: emit a structured file-edit
    /// diff card to the originating channel after a successful
    /// `edit_file` / `multi_edit` / `apply_patch` / `write_file` write.
    /// The `diff` is the canonical [`DiffCard`](
    /// copperclaw_channels_core::DiffCard) computed by the tool handler
    /// from the pre-edit snapshot vs the post-edit content (the
    /// handler runs the diff once and hands us the structured result —
    /// reserialising into unified-diff text and re-parsing in every
    /// renderer is wasted work).
    ///
    /// Default no-op so contexts that don't need diff cards (mocks,
    /// subagent adapters) continue to compile unchanged. The runner's
    /// `RunnerToolCtx` overrides this with a real implementation that
    /// writes a `MessageKind::Diff` row to `outbound.db`; the host's
    /// delivery service then routes it through the adapter's
    /// `deliver_diff` hook for native rendering.
    ///
    /// Errors are swallowed: diff cards are best-effort UX, NOT
    /// load-bearing — a failed write must not abort the file edit.
    async fn emit_diff(&self, diff: copperclaw_channels_core::DiffCard) {
        let _ = diff;
    }

    /// M18 R3 verification gate: whether the `todo_update(completed)`
    /// gate is enforced for this session. `false` restores byte-
    /// identical pre-R3 behaviour (evidence-only anti-fabrication
    /// check, no dirty-tracking). Default `true` (gate on) so contexts
    /// that don't opt out (mocks, subagent adapters) get the stricter,
    /// safer default; the runner's `RunnerToolCtx` overrides this from
    /// `container_configs.verify_gate` (via `RunnerDeps`/`RunnerConfig`).
    fn verify_gate_enabled(&self) -> bool {
        true
    }

    /// M18 R3 verification gate: per-group override for the shell
    /// command that verifies a project, winning over whatever the
    /// agent itself recorded at `<project>/.copperclaw/verify`.
    /// `None` means "no override — use the agent-recorded command."
    /// Default `None` so contexts that don't configure one (mocks,
    /// subagent adapters) compile unchanged; the runner's
    /// `RunnerToolCtx` overrides this from `container_configs.check_command`.
    fn check_command_override(&self) -> Option<String> {
        None
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
    /// Pre-seeded list of goals returned by `list_goals` (M22 A3).
    goal_summaries: Vec<GoalSummary>,
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
    /// Diff cards recorded in order — populated by `emit_diff` (the
    /// MCP slice-3.1 surface). Lets file-edit tool tests assert the
    /// runner-side diff card emit fired with the expected payload.
    diff_calls: Vec<copperclaw_channels_core::DiffCard>,
    /// Last value passed to `set_active_skill_allowed_tools`. Lets the
    /// `load_skill` tool tests assert the active-skill policy hook fired
    /// with the catalogue's normalized `allowed-tools`. The three states
    /// are all meaningful, so the double `Option` is deliberate: `None` =
    /// the hook was never called; `Some(None)` = called with no scope (a
    /// skill that declared no `allowed-tools`); `Some(Some(_))` = called
    /// with an explicit scope.
    #[allow(clippy::option_option)]
    active_skill_allowed: Option<Option<Vec<String>>>,
    /// Test override for `verify_gate_enabled`. `None` falls through
    /// to the trait default (`true`).
    verify_gate_enabled: Option<bool>,
    /// Test override for `check_command_override`. Same double-`Option`
    /// shape as `active_skill_allowed`: `None` = not overridden (falls
    /// through to the trait default of `None`), `Some(inner)` = the
    /// override value tests want returned.
    #[allow(clippy::option_option)]
    check_command_override: Option<Option<String>>,
    /// `delegate_batch` requests recorded in order.
    delegate_batch_calls: Vec<DelegateBatchRequest>,
    /// Pre-seeded outcome returned by the next `run_delegate_batch`. When
    /// `None`, the mock returns a canned all-`ok` outcome (one report per
    /// worker) so happy-path tool tests work without wiring.
    next_delegate_batch_outcome: Option<DelegateBatchOutcome>,
    /// If set, the next `run_delegate_batch` returns this Err instead.
    next_delegate_batch_err: Option<ToolError>,
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

    /// Seed the goal list returned by `list_goals` (M22 A3).
    pub fn set_goals(&self, goals: Vec<GoalSummary>) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .goal_summaries = goals;
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

    /// Snapshot of diff cards recorded via `emit_diff`.
    pub fn diff_calls(&self) -> Vec<copperclaw_channels_core::DiffCard> {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .diff_calls
            .clone()
    }

    /// The last value passed to `set_active_skill_allowed_tools`, or
    /// `None` if the hook was never called. `Some(None)` means it was
    /// called with no tool scope (a skill that declared no `allowed-tools`).
    #[allow(clippy::option_option)]
    pub fn active_skill_allowed_recorded(&self) -> Option<Option<Vec<String>>> {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .active_skill_allowed
            .clone()
    }

    /// Override the value `verify_gate_enabled()` returns. Tests use
    /// this to exercise the `verify_gate=off` byte-stable path.
    pub fn set_verify_gate_enabled(&self, enabled: bool) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .verify_gate_enabled = Some(enabled);
    }

    /// Override the value `check_command_override()` returns.
    pub fn set_check_command_override(&self, cmd: Option<String>) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .check_command_override = Some(cmd);
    }

    /// Snapshot of `delegate_batch` requests recorded so far.
    pub fn delegate_batch_calls(&self) -> Vec<DelegateBatchRequest> {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .delegate_batch_calls
            .clone()
    }

    /// Override the outcome returned by the next `run_delegate_batch`.
    pub fn set_next_delegate_batch_outcome(&self, outcome: DelegateBatchOutcome) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .next_delegate_batch_outcome = Some(outcome);
    }

    /// Cause the *next* `run_delegate_batch` to fail.
    pub fn fail_next_delegate_batch(&self, err: ToolError) {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .next_delegate_batch_err = Some(err);
    }
}

#[async_trait]
impl ToolContext for MockToolContext {
    async fn emit_outbound(&self, effect: OutboundToolEffect) -> Result<ToolEffectAck, ToolError> {
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

    async fn list_goals(&self) -> Result<Vec<GoalSummary>, ToolError> {
        let mut g = self.inner.lock().expect("MockToolContext mutex poisoned");
        if let Some(err) = g.next_list_err.take() {
            return Err(err);
        }
        Ok(g.goal_summaries.clone())
    }

    async fn spawn_subagent(&self, req: SubagentRequest) -> Result<SubagentResult, ToolError> {
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

    async fn run_delegate_batch(
        &self,
        req: DelegateBatchRequest,
    ) -> Result<DelegateBatchOutcome, ToolError> {
        let mut g = self.inner.lock().expect("MockToolContext mutex poisoned");
        if let Some(err) = g.next_delegate_batch_err.take() {
            return Err(err);
        }
        let canned = DelegateBatchOutcome {
            workers: req
                .workers
                .iter()
                .map(|w| WorkerOutcome {
                    name: w.name.clone(),
                    status: WorkerStatus::Ok,
                    report: Some(format!("mock report from {}", w.name)),
                    error: None,
                    session_id: None,
                })
                .collect(),
        };
        let outcome = g.next_delegate_batch_outcome.take().unwrap_or(canned);
        g.delegate_batch_calls.push(req);
        Ok(outcome)
    }

    async fn emit_diff(&self, diff: copperclaw_channels_core::DiffCard) {
        let mut g = self.inner.lock().expect("MockToolContext mutex poisoned");
        g.diff_calls.push(diff);
    }

    fn set_active_skill_allowed_tools(&self, allowed: Option<Vec<String>>) {
        let mut g = self.inner.lock().expect("MockToolContext mutex poisoned");
        g.active_skill_allowed = Some(allowed);
    }

    fn active_skill_allowed_tools(&self) -> Option<Vec<String>> {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .active_skill_allowed
            .clone()
            .flatten()
    }

    fn verify_gate_enabled(&self) -> bool {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .verify_gate_enabled
            .unwrap_or(true)
    }

    fn check_command_override(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("MockToolContext mutex poisoned")
            .check_command_override
            .clone()
            .flatten()
    }
}

/// Internal base64 helper used by `SendFileSpec`. The runner can produce
/// these bytes from `OutboundFile`, and the input JSON wire format accepts
/// standard base64.
pub(crate) mod bytes_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// Encode bytes as standard base64 (with `=` padding, no line breaks).
    /// Shared by the serde `serialize` below and by tools that need a
    /// plain `&[u8] -> String` encode (e.g. `view_image`).
    pub(crate) fn encode(bytes: &[u8]) -> String {
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
        out
    }

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(bytes))
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

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
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
                    card: copperclaw_channels_core::Card {
                        title: Some("t".into()),
                        ..copperclaw_channels_core::Card::default()
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
                    scope: InstallScope::Image,
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
                OutboundToolEffect::SaveSkill(SaveSkillSpec {
                    name: "my-skill".into(),
                    content: "---\nname: my-skill\ndescription: d\n---\nb\n".into(),
                    reason: "r".into(),
                }),
                "save_skill",
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
            (
                OutboundToolEffect::AuthorTaskGrant(AuthorTaskGrantSpec {
                    task_name: "t".into(),
                    capability_scope: "send_message:telegram".into(),
                    token_budget: Some(1000),
                    max_fires: Some(30),
                    expires_at: chrono::Utc::now(),
                    reason: "r".into(),
                }),
                "author_task_grant",
            ),
            (OutboundToolEffect::ListTasks, "list_tasks"),
            (
                OutboundToolEffect::CancelTask {
                    id: "task_1".into(),
                },
                "cancel_task",
            ),
            (
                OutboundToolEffect::PauseTask {
                    id: "task_1".into(),
                },
                "pause_task",
            ),
            (
                OutboundToolEffect::ResumeTask {
                    id: "task_1".into(),
                },
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
            ToolEffectAck::Task {
                id: "task_1".into(),
            },
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
            Recipient::Channel {
                id: "telegram:1".into(),
            },
            Recipient::Agent {
                session_id: "sess_1".into(),
            },
            Recipient::User { id: "u_1".into() },
            Recipient::Parent,
            Recipient::RootUser,
        ];
        for r in recipients {
            let s = serde_json::to_string(&r).unwrap();
            let back: Recipient = serde_json::from_str(&s).unwrap();
            assert_eq!(r, back);
        }
    }

    #[test]
    fn recipient_from_to_string_parses_special_forms() {
        assert_eq!(Recipient::from_to_string("user"), Recipient::RootUser);
        assert_eq!(Recipient::from_to_string(" user "), Recipient::RootUser);
        assert_eq!(Recipient::from_to_string("agent:parent"), Recipient::Parent);
        // Anything else keeps the historical bare-string channel form —
        // including `agent:<name>` slugs, which the host's agent_to_agent
        // module resolves by name.
        assert_eq!(
            Recipient::from_to_string("telegram:chat-9"),
            Recipient::Channel {
                id: "telegram:chat-9".into()
            }
        );
        assert_eq!(
            Recipient::from_to_string("agent:scout-a"),
            Recipient::Channel {
                id: "agent:scout-a".into()
            }
        );
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
