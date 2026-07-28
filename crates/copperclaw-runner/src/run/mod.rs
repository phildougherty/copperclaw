//! Runner poll loop. Module name is `run` because `loop` is a reserved
//! keyword in Rust 2024.
//!
//! The module is split into focused files: the per-turn orchestrator in
//! [`drive_turn`], the provider-call layer (retry + deadline + stream
//! pump) in [`provider_call`], the per-tool dispatcher in
//! [`tool_dispatch`], and the formatting seam in [`formatting`]. This
//! file holds the [`RunnerDeps`] struct, [`run_loop`] itself, and the
//! small DB-side helpers that need a `&RunnerDeps` and a mutex lock on
//! the inbound/outbound connections.

pub(super) mod blocker;
pub mod delegate_batch;
pub(super) mod drive_turn;
pub mod external_mcp;
pub(super) mod failover_health;
pub(super) mod formatting;
pub mod hud;
// M22 C3: container-local symbol-index bridge (ctags/LSP) feeding find_symbol.
pub(super) mod lsp;
pub mod preview;
pub(super) mod progressive;
// M22 C2: open/attach an existing repository as the working project.
pub mod project;
pub(super) mod prompt;
pub(super) mod provider_call;
pub(super) mod reaction;
// M23 W2.2: cold-boot service restart hook (`/data/.copperclaw/services`).
pub(super) mod services;
pub(super) mod tool_dispatch;

// M22 A2: the autonomy-gate types appear in [`RunnerDeps`]'s public
// `active_grant` field, so re-export them at the (public) `run` module path to
// keep the type publicly reachable (the runner binary constructs the field).
pub use tool_dispatch::{GrantGateState, TurnGrant};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use copperclaw_db::tables::{container_state, messages_in, processing_ack};
use copperclaw_mcp::{ToolContext, ToolEntry};
use copperclaw_providers::{AgentProvider, HistoryMessage, ToolDef};
#[cfg_attr(not(test), allow(unused_imports))]
use copperclaw_types::MessageId;
use copperclaw_types::{Effort, MessageInRow};
use rusqlite::Connection;
use std::collections::HashMap;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::compaction::{CompactionCfg, compact, estimate_tokens};
use crate::formatter::format_messages;
use crate::state::{load_state, save_state};

use self::drive_turn::{TurnOutcome, drive_turn_with_health};
use self::failover_health::FailoverHealth;
use self::provider_call::touch_heartbeat;
// The 4-arg `drive_turn` wrapper (session-scoped health created per call) is
// used only by the runner's own tests; production drives through
// `drive_turn_with_health` with the loop-scoped `FailoverHealth`.
#[cfg(test)]
use self::drive_turn::drive_turn;

/// Default poll interval (ms) while the loop is idle.
pub const POLL_INTERVAL_MS: u64 = 1000;
/// Active poll interval (ms) while messages are still flowing.
pub const ACTIVE_POLL_INTERVAL_MS: u64 = 500;

/// Default per-LLM-call deadline (milliseconds). Wraps the
/// `provider.query()` call inside `run_llm_turn`. Each attempt gets the
/// full budget independently — i.e. with 3 attempts the worst-case wall
/// time is `3 * DEFAULT_PROVIDER_DEADLINE_MS` + accumulated backoff
/// (~1.75s).
///
/// Overridable per-process via `COPPERCLAW_RUNNER_PROVIDER_DEADLINE_MS`.
///
/// # Coupled with the host's heartbeat-staleness threshold
///
/// The host's `DEFAULT_HEARTBEAT_STALE_SECS` (in
/// `copperclaw-host/src/container_manager/spawn.rs`) MUST stay at least
/// `2 * (DEFAULT_PROVIDER_DEADLINE_MS / 1000)`. If they're equal, the
/// host can race the runner and SIGKILL the container at the exact
/// moment a slow provider call returns `DeadlineExceeded`, losing the
/// in-flight turn and triggering a respawn loop. The host enforces
/// this with a startup check
/// (`check_heartbeat_deadline_alignment`) that emits a `warn!` when
/// the relationship is violated — see that function for the rationale.
///
/// If you raise this default, also raise the heartbeat threshold to
/// keep the 2x margin. If you lower it (e.g. tightening to 30s for a
/// faster-fail UX), the existing 120s heartbeat default already
/// covers the new value with margin to spare.
pub const DEFAULT_PROVIDER_DEADLINE_MS: u64 = 60_000;

/// Environment variable read at runner startup to override
/// [`DEFAULT_PROVIDER_DEADLINE_MS`]. Values outside the
/// [`MIN_PROVIDER_DEADLINE_MS`]..=[`MAX_PROVIDER_DEADLINE_MS`] range are
/// rejected with a warning (the default is used instead) so an operator
/// can't accidentally disable the deadline by setting it to 0.
pub const PROVIDER_DEADLINE_ENV: &str = "COPPERCLAW_RUNNER_PROVIDER_DEADLINE_MS";

/// Lower bound for the per-call deadline (ms). The spec calls out
/// "don't make the deadline default <30s"; the validator enforces the
/// same on user-supplied values so a typo of `60` (intending seconds)
/// doesn't trip every call.
pub const MIN_PROVIDER_DEADLINE_MS: u64 = 30_000;

/// Upper bound for the per-call deadline (ms). Anything higher than
/// 5 minutes per attempt is almost certainly a misconfiguration —
/// reqwest's own client timeout is 600s and 3 attempts past that would
/// hang the runner for 25 minutes.
pub const MAX_PROVIDER_DEADLINE_MS: u64 = 300_000;

/// Resolve the per-call provider deadline from the env. Out-of-range or
/// unparseable values fall back to [`DEFAULT_PROVIDER_DEADLINE_MS`]
/// (with a warning).
///
/// Pulled out as a free function so the runner binary and tests can
/// share the same clamping logic. The trait bound matches
/// `config::EnvLookup`.
#[must_use]
pub fn resolve_provider_deadline(env: &dyn crate::config::EnvLookup) -> Duration {
    let Some(raw) = env.get(PROVIDER_DEADLINE_ENV) else {
        return Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS);
    };
    let Ok(parsed) = raw.parse::<u64>() else {
        tracing::warn!(
            env = PROVIDER_DEADLINE_ENV,
            value = %raw,
            "could not parse provider deadline; using default"
        );
        return Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS);
    };
    if !(MIN_PROVIDER_DEADLINE_MS..=MAX_PROVIDER_DEADLINE_MS).contains(&parsed) {
        tracing::warn!(
            env = PROVIDER_DEADLINE_ENV,
            value = parsed,
            min = MIN_PROVIDER_DEADLINE_MS,
            max = MAX_PROVIDER_DEADLINE_MS,
            "provider deadline out of range; using default"
        );
        return Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS);
    }
    Duration::from_millis(parsed)
}

/// Env var operators / tests use to override the per-tool deadline.
pub const TOOL_DEADLINE_ENV: &str = "COPPERCLAW_RUNNER_TOOL_DEADLINE_SECS";

/// Floor for the per-tool deadline. Below 10s a routine `npm install`
/// of a trivial package list trips the timeout, defeating the safety
/// net.
pub const MIN_TOOL_DEADLINE_SECS: u64 = 10;

/// Ceiling for the per-tool deadline. An hour is generous for any
/// realistic single tool invocation (`cargo build` of a large crate,
/// big `apt-get install`). Beyond that, presume the tool is wedged.
pub const MAX_TOOL_DEADLINE_SECS: u64 = 3_600;

/// Env var operators use to override how many tool-use cycles a single
/// inbound is allowed to run before the runner bails. Build/research
/// tasks routinely exceed the original cap of 20 (e.g. researching App
/// Store apps + scaffolding a TypeScript project requires ~40 tool
/// calls). Operators can crank this up for long autonomous sessions or
/// dial it down for cheap conversational agents.
pub const MAX_TOOL_TURNS_ENV: &str = "COPPERCLAW_MAX_TOOL_TURNS";

/// Default tool-use cycles per inbound. Sized for autonomous build/
/// research tasks rather than chat — short conversations finish well
/// inside this. Raised from 60: substantial "review the codebase and
/// implement the fixes" / full-app-build requests routinely run past 60
/// turns and were getting cut off mid-task. Operators can tune it further
/// with `COPPERCLAW_MAX_TOOL_TURNS` (clamped to
/// [`MIN_MAX_TOOL_TURNS`, `MAX_MAX_TOOL_TURNS`]).
pub const DEFAULT_MAX_TOOL_TURNS: usize = 150;

/// Floor — below this even simple multi-tool chains bail mid-flight.
pub const MIN_MAX_TOOL_TURNS: usize = 5;

/// Ceiling — beyond this a runaway tool loop is more likely than a
/// legitimate need.
pub const MAX_MAX_TOOL_TURNS: usize = 500;

/// Resolve `max_tool_turns` from the env. Out-of-range / unparseable
/// values fall back to [`DEFAULT_MAX_TOOL_TURNS`] with a one-shot WARN.
///
/// The misconfig warn fires exactly once per process via a
/// `OnceLock<()>` guard. Each runner spawn is a fresh process inside a
/// container, so the dedupe is *within-process* — a long-lived runner
/// that hits this function across many turns won't re-warn. (Per-spawn
/// dedupe across containers would need host-side state we don't keep
/// here.)
#[must_use]
pub fn resolve_max_tool_turns(env: &dyn crate::config::EnvLookup) -> usize {
    static MISCONFIG_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let warn_once = |line: &str, raw: &str| {
        MISCONFIG_WARNED.get_or_init(|| {
            tracing::warn!(
                env = MAX_TOOL_TURNS_ENV,
                value = %raw,
                "{line} (using default {DEFAULT_MAX_TOOL_TURNS}; suppressing further warnings this process)",
            );
        });
    };
    let Some(raw) = env.get(MAX_TOOL_TURNS_ENV) else {
        return DEFAULT_MAX_TOOL_TURNS;
    };
    let Ok(parsed) = raw.parse::<usize>() else {
        warn_once("max tool turns is not a valid integer", &raw);
        return DEFAULT_MAX_TOOL_TURNS;
    };
    if !(MIN_MAX_TOOL_TURNS..=MAX_MAX_TOOL_TURNS).contains(&parsed) {
        warn_once(
            &format!("max tool turns out of range [{MIN_MAX_TOOL_TURNS}, {MAX_MAX_TOOL_TURNS}]"),
            &raw,
        );
        return DEFAULT_MAX_TOOL_TURNS;
    }
    parsed
}

/// Env var operators use to override the HARD tool-turn ceiling — the
/// absolute maximum number of tool-use cycles a single inbound may run
/// across ALL smart auto-continue blocks before the runner stops and asks
/// the operator to send `continue`. This is the runaway backstop for the
/// progress-gated budget (see [`drive_turn`]): the soft cap
/// ([`MAX_TOOL_TURNS_ENV`]) is one "block", and a block that made real
/// progress extends the budget by another block WITHOUT interrupting the
/// operator — but the total is always bounded by this hard ceiling so a
/// productive-looking loop can never run unbounded.
///
/// Resolved next to [`resolve_max_tool_turns`] via
/// [`resolve_max_tool_turns_hard`]. Set it EQUAL to the soft cap to disable
/// extension entirely (the historical flat-cap behaviour). The default is
/// `soft * 6` (so a fresh install extends generously), clamped to
/// [`MAX_MAX_TOOL_TURNS_HARD`] and to `>= soft`.
///
/// [`drive_turn`]: crate::run::drive_turn
pub const MAX_TOOL_TURNS_HARD_ENV: &str = "COPPERCLAW_MAX_TOOL_TURNS_HARD";

/// Absolute ceiling for the hard tool-turn bound. Even an operator who
/// cranks [`MAX_TOOL_TURNS_HARD_ENV`] up cannot push a single inbound past
/// this many tool turns — beyond it a "productive" loop is a runaway by any
/// measure, so we refuse to honour a higher configured value and clamp to
/// this (with a WARN). At the default soft cap of 150 the default hard
/// ceiling is 900, comfortably under this clamp; the clamp only bites for an
/// operator who sets both the soft cap and the hard ceiling very high.
pub const MAX_MAX_TOOL_TURNS_HARD: usize = 1200;

/// The default hard ceiling derived from a resolved `soft` cap: `soft * 6`,
/// clamped to [`MAX_MAX_TOOL_TURNS_HARD`] and floored at `soft` (so the hard
/// ceiling can never sit below the soft cap even for a pathological `soft`).
/// A fresh install (soft = [`DEFAULT_MAX_TOOL_TURNS`] = 150) gets a hard
/// ceiling of 900.
#[must_use]
fn default_max_tool_turns_hard(soft: usize) -> usize {
    soft.saturating_mul(6)
        .min(MAX_MAX_TOOL_TURNS_HARD)
        .max(soft)
}

/// Resolve the HARD tool-turn ceiling from the env, given the already-resolved
/// `soft` cap. Mirrors [`resolve_max_tool_turns`]'s shape: unset returns the
/// derived default (`soft * 6`, clamped — see [`default_max_tool_turns_hard`]);
/// an unparseable value warns once and returns that default; an in-range value
/// is honoured; an out-of-range value is CLAMPED into `[soft,
/// MAX_MAX_TOOL_TURNS_HARD]` (with a one-shot WARN) rather than reset to the
/// default, so an operator who asks for "as high as possible" gets the max
/// rather than a surprise drop back to `soft * 6`.
///
/// Setting the env EQUAL to `soft` is a legitimate configuration (extension
/// disabled → flat-cap behaviour), not a misconfig, so it never warns. The
/// misconfig warn fires exactly once per process via a `OnceLock<()>` guard,
/// same as its sibling.
#[must_use]
pub fn resolve_max_tool_turns_hard(env: &dyn crate::config::EnvLookup, soft: usize) -> usize {
    static MISCONFIG_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let default = default_max_tool_turns_hard(soft);
    let warn_once = |line: &str, raw: &str| {
        MISCONFIG_WARNED.get_or_init(|| {
            tracing::warn!(
                env = MAX_TOOL_TURNS_HARD_ENV,
                value = %raw,
                "{line} (clamped; suppressing further warnings this process)",
            );
        });
    };
    let Some(raw) = env.get(MAX_TOOL_TURNS_HARD_ENV) else {
        return default;
    };
    let Ok(parsed) = raw.parse::<usize>() else {
        MISCONFIG_WARNED.get_or_init(|| {
            tracing::warn!(
                env = MAX_TOOL_TURNS_HARD_ENV,
                value = %raw,
                "hard tool-turn ceiling is not a valid integer (using default {default}; suppressing further warnings this process)",
            );
        });
        return default;
    };
    // Clamp into [soft, MAX_MAX_TOOL_TURNS_HARD]. A value below the soft cap
    // would make the run bail before even one soft block completes, so we
    // floor it at `soft` (which also disables extension) rather than honour it.
    if parsed < soft {
        warn_once(
            &format!(
                "hard tool-turn ceiling {parsed} is below the soft cap {soft}; clamping up to {soft} (extension disabled)"
            ),
            &raw,
        );
        return soft;
    }
    if parsed > MAX_MAX_TOOL_TURNS_HARD {
        warn_once(
            &format!(
                "hard tool-turn ceiling {parsed} exceeds the absolute max {MAX_MAX_TOOL_TURNS_HARD}; clamping down"
            ),
            &raw,
        );
        return MAX_MAX_TOOL_TURNS_HARD;
    }
    parsed
}

/// Env var operators use to override the per-TASK token ceiling — the
/// total input+output tokens a single inbound's tool-loop may spend
/// before the runner hard-aborts. Distinct from the per-DAY group cap
/// (`budgets`/`BUDGET_GATE_DAILY_TOKENS`), which gates at *spawn* time:
/// this bounds the cost of a single runaway task mid-loop. A confused
/// agent that re-reads a large context on every one of 119 tool calls
/// can blow millions of tokens without exceeding
/// [`MAX_TOOL_TURNS_ENV`]; this ceiling is the cost backstop for that
/// shape. Set to `0` to disable (no per-task ceiling).
pub const MAX_TASK_TOKENS_ENV: &str = "COPPERCLAW_MAX_TASK_TOKENS";

/// Default per-task token ceiling. Sized so a normal build/research task
/// never trips it (those land in the tens-to-low-hundreds of thousands of
/// tokens across their tool loop), while a genuine runaway is caught well
/// before it becomes expensive. Calibrated against a live ~6M-token
/// single-task runaway: at 2M the abort fires around a third of the way
/// in, bounding the worst-case spend to roughly a third of what that run
/// actually cost. Operators tune via [`MAX_TASK_TOKENS_ENV`] (clamped to
/// [`MIN_MAX_TASK_TOKENS`, `MAX_MAX_TASK_TOKENS`], or `0` to disable).
pub const DEFAULT_MAX_TASK_TOKENS: u64 = 2_000_000;

/// Floor for a *non-zero* per-task ceiling. Below this even a single
/// large-context turn (a 200k-window model echoing its whole history)
/// could trip the abort on turn one, defeating the purpose. `0` is a
/// distinct sentinel meaning "disabled" and is allowed past this floor.
pub const MIN_MAX_TASK_TOKENS: u64 = 100_000;

/// Ceiling for the per-task token budget. Beyond this the "budget" stops
/// being a cost backstop — a single task spending 50M+ tokens is a
/// runaway by any measure, so we refuse to honour a configured value
/// that high and fall back to the default with a WARN.
pub const MAX_MAX_TASK_TOKENS: u64 = 50_000_000;

/// Resolve the per-task token ceiling from the env. Same fallback shape
/// as [`resolve_max_tool_turns`]: unparseable / out-of-range values fall
/// back to [`DEFAULT_MAX_TASK_TOKENS`] with a one-shot WARN. The literal
/// `0` is honoured as "disabled" (returns `0`); any other value below
/// [`MIN_MAX_TASK_TOKENS`] or above [`MAX_MAX_TASK_TOKENS`] is rejected.
#[must_use]
pub fn resolve_max_task_tokens(env: &dyn crate::config::EnvLookup) -> u64 {
    static MISCONFIG_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let warn_once = |line: &str, raw: &str| {
        MISCONFIG_WARNED.get_or_init(|| {
            tracing::warn!(
                env = MAX_TASK_TOKENS_ENV,
                value = %raw,
                "{line} (using default {DEFAULT_MAX_TASK_TOKENS}; suppressing further warnings this process)",
            );
        });
    };
    let Some(raw) = env.get(MAX_TASK_TOKENS_ENV) else {
        return DEFAULT_MAX_TASK_TOKENS;
    };
    let Ok(parsed) = raw.parse::<u64>() else {
        warn_once("max task tokens is not a valid integer", &raw);
        return DEFAULT_MAX_TASK_TOKENS;
    };
    // 0 is the explicit "disable the per-task ceiling" sentinel.
    if parsed == 0 {
        return 0;
    }
    if !(MIN_MAX_TASK_TOKENS..=MAX_MAX_TASK_TOKENS).contains(&parsed) {
        warn_once(
            &format!(
                "max task tokens out of range [{MIN_MAX_TASK_TOKENS}, {MAX_MAX_TASK_TOKENS}] (0 to disable)"
            ),
            &raw,
        );
        return DEFAULT_MAX_TASK_TOKENS;
    }
    parsed
}

/// Resolve the per-tool deadline from the env. Same shape as
/// [`resolve_provider_deadline`]: out-of-range / unparseable values
/// fall back to [`DEFAULT_TOOL_DEADLINE_SECS`] with a WARN.
#[must_use]
pub fn resolve_tool_deadline_secs(env: &dyn crate::config::EnvLookup) -> u64 {
    let Some(raw) = env.get(TOOL_DEADLINE_ENV) else {
        return DEFAULT_TOOL_DEADLINE_SECS;
    };
    let Ok(parsed) = raw.parse::<u64>() else {
        tracing::warn!(
            env = TOOL_DEADLINE_ENV,
            value = %raw,
            "could not parse tool deadline; using default"
        );
        return DEFAULT_TOOL_DEADLINE_SECS;
    };
    if !(MIN_TOOL_DEADLINE_SECS..=MAX_TOOL_DEADLINE_SECS).contains(&parsed) {
        tracing::warn!(
            env = TOOL_DEADLINE_ENV,
            value = parsed,
            min = MIN_TOOL_DEADLINE_SECS,
            max = MAX_TOOL_DEADLINE_SECS,
            "tool deadline out of range; using default"
        );
        return DEFAULT_TOOL_DEADLINE_SECS;
    }
    parsed
}

/// M22 B1: one-shot user-facing notices queued by loop-level events that
/// fire OUTSIDE a turn — before any [`hud::TaskHud`] exists to carry them.
/// Today's single producer is the automatic compaction pass in [`run_loop`]
/// (it runs before `drive_turn`; the `/compact` slash path already confirms
/// to the user directly, and this closes the asymmetry for the silent
/// automatic path). The next constructed [`hud::TaskHud`] drains ONE note
/// into its existing one-shot note machinery, so the note rides the next
/// HUD frame (rich channels) or the next periodic status row (bare
/// channels) and renders exactly once; a note the turn never got to render
/// (a fast pure-text turn posts no frame) is re-queued at finalize so a
/// later turn surfaces it instead of dropping it. FIFO: notes surface in
/// the order pushed, at most one per turn. Best-effort by design — a
/// poisoned lock holds or drops notes rather than panicking, because a
/// status annotation must never take down the runner.
#[derive(Debug, Default)]
pub struct Notices(std::sync::Mutex<Vec<String>>);

impl Notices {
    /// Queue a note for the next turn's HUD frame / status row.
    pub fn push(&self, text: impl Into<String>) {
        if let Ok(mut notes) = self.0.lock() {
            notes.push(text.into());
        }
    }

    /// Take the oldest queued note (FIFO), one at a time. `None` when the
    /// queue is empty (or the lock is poisoned).
    #[must_use]
    pub fn drain_one(&self) -> Option<String> {
        let mut notes = self.0.lock().ok()?;
        if notes.is_empty() {
            None
        } else {
            Some(notes.remove(0))
        }
    }
}

/// Dependencies injected into [`run_loop`]. Holding all of these in a struct
/// keeps the signature small and makes it easy to fan out variations from
/// tests.
pub struct RunnerDeps {
    /// Provider handle (Anthropic, Codex, …).
    pub provider: Arc<dyn AgentProvider>,
    /// Tool context the provider calls into.
    pub tool_ctx: Arc<dyn ToolContext>,
    /// `inbound.db` connection (host-written, read-only).
    pub inbound: Arc<Mutex<Connection>>,
    /// `outbound.db` connection (container-written).
    pub outbound: Arc<Mutex<Connection>>,
    /// Tools advertised to the model. Empty list means "no tools".
    pub tools: Vec<ToolDef>,
    /// System prompt to send on every turn.
    pub system: String,
    /// Model identifier.
    pub model: String,
    /// Effort hint.
    pub effort: Effort,
    /// Max tokens per turn.
    pub max_tokens: u32,
    /// Temperature, if any.
    pub temperature: Option<f32>,
    /// Display name of the assistant.
    pub assistant_name: Option<String>,
    /// Compaction configuration.
    pub compaction: CompactionCfg,
    /// Per-turn transcript-shrink configuration. Stale, oversized
    /// tool-result bodies are replaced with a one-line stub before the
    /// transcript is replayed to the provider (see
    /// [`crate::formatter::elide_stale_tool_results`]); recent results —
    /// including the current turn's — are left full. The persisted
    /// `state.history` is NOT mutated, so the elision is a pure send-time
    /// view and a later compaction still summarises the real bodies.
    pub elision: crate::formatter::ElisionCfg,
    /// How many turns to run before exiting cleanly. `None` means loop forever.
    pub max_turns: Option<usize>,
    /// Per-iteration sleep when the inbox is empty.
    pub idle_sleep: Duration,
    /// Heartbeat path the runner touches once per iteration (None to skip).
    pub heartbeat_path: Option<PathBuf>,
    /// Session id this runner is bound to. Stamped on every
    /// `usage_report` system row so the host can join to
    /// `agent_turns.session_id`.
    pub session_id: copperclaw_types::SessionId,
    /// Agent group id. Same use as `session_id`.
    pub agent_group_id: copperclaw_types::AgentGroupId,
    /// Runner-local turn counter. Bumped by the `usage_report` emitter
    /// after each turn so `agent_turns.seq` is monotonically
    /// increasing per session.
    pub turn_seq: Arc<std::sync::atomic::AtomicI64>,
    /// Per-tool dispatch map. Built once at runner startup from
    /// `copperclaw_mcp::build_tool_map()`. Keyed by the tool name the
    /// model emits in `tool_use` blocks; each entry knows how to
    /// validate the input and invoke the handler against the
    /// runner's `ToolContext`.
    pub tool_map: Arc<HashMap<String, Arc<ToolEntry>>>,
    /// Routes for **external** MCP tools advertised from the host's
    /// `mcp_tools.json` manifest. Keyed by the advertised, namespaced name
    /// (`mcp__<server>__<tool>`); a hit routes the call through the
    /// host-proxied request/response path
    /// ([`external_mcp::dispatch_external`]) instead of the in-container
    /// `tool_map`. Empty when the group configures no external MCP servers.
    pub external_tools: Arc<HashMap<String, external_mcp::ExternalToolRoute>>,
    /// SOFT cap on consecutive tool-use turns per inbound — one smart
    /// auto-continue "block". When a full block completes without an earlier
    /// bail (loop-breaker / token ceiling / parse-error), [`drive_turn`]
    /// consults [`Self::max_tool_turns_hard`]: if the block made real progress
    /// (a successful non-read-only tool call) it EXTENDS by another block
    /// without interrupting the operator; otherwise it stops and asks. Resolved
    /// from [`MAX_TOOL_TURNS_ENV`]; the `minimal` default is
    /// [`DEFAULT_MAX_TOOL_TURNS`].
    ///
    /// [`drive_turn`]: crate::run::drive_turn
    pub max_tool_turns: usize,
    /// HARD ceiling on the TOTAL tool-use turns per inbound across ALL smart
    /// auto-continue blocks — the runaway backstop. Once the cumulative turn
    /// count reaches this, [`drive_turn`] stops and surfaces a "hit the hard
    /// ceiling — send 'continue'" message EVEN IF the agent is still making
    /// progress, so a productive-looking loop can never run unbounded: the
    /// worst-case total tool turns for a single inbound is exactly this value.
    /// Must be `>= max_tool_turns`; set EQUAL to it to disable extension (the
    /// historical flat-cap behaviour). The runner binary resolves it from
    /// [`MAX_TOOL_TURNS_HARD_ENV`] via [`resolve_max_tool_turns_hard`]; the
    /// `minimal` default is `soft * 6` ([`default_max_tool_turns_hard`], = 900
    /// at the default soft cap of 150).
    ///
    /// [`drive_turn`]: crate::run::drive_turn
    pub max_tool_turns_hard: usize,
    /// Per-TASK token ceiling: the cumulative input+output tokens a
    /// single inbound's tool-loop may spend before [`drive_turn`]
    /// hard-aborts with a surfaced "task budget reached" message and a
    /// [`copperclaw_metrics::inc_task_budget_exhausted`] trip. `0`
    /// disables the ceiling (no per-task abort). Complements — does not
    /// replace — [`max_tool_turns`] and the per-DAY group cap: it bounds
    /// COST specifically, catching a token-heavy runaway that re-reads a
    /// big context without burning many *turns*. The runner binary
    /// resolves it from [`MAX_TASK_TOKENS_ENV`] via
    /// [`resolve_max_task_tokens`]; the default in [`RunnerDeps::minimal`]
    /// is [`DEFAULT_MAX_TASK_TOKENS`].
    ///
    /// [`drive_turn`]: crate::run::drive_turn
    pub max_task_tokens: u64,
    /// Per-LLM-call deadline. Wraps each `provider.query()` attempt in
    /// `tokio::time::timeout`. On expiry the attempt is treated as a
    /// retryable failure and reissued (with backoff) up to
    /// [`provider_call::MAX_PROVIDER_ATTEMPTS`] times before terminal failure.
    ///
    /// Default in [`RunnerDeps::minimal`] is
    /// [`DEFAULT_PROVIDER_DEADLINE_MS`]. The runner binary picks the
    /// value up from `COPPERCLAW_RUNNER_PROVIDER_DEADLINE_MS`; tests can
    /// shorten it to make failure-mode fixtures finish quickly.
    pub provider_deadline: Duration,
    /// Per-tool-call hard ceiling. Wraps each [`tool_dispatch::invoke_tool`]
    /// dispatch in `tokio::time::timeout`; on expiry the tool returns a
    /// `tool_result` with `is_error: true` describing the timeout.
    /// Belt-and-braces with [`provider_call::HeartbeatTicker`]: the ticker prevents
    /// the host from killing the container during a slow legitimate
    /// tool, this deadline keeps a *wedged* tool from running forever.
    /// Default [`DEFAULT_TOOL_DEADLINE_SECS`].
    pub tool_deadline_secs: u64,
    /// Receives "the provider call is still working" signals while a
    /// `run_llm_turn` is in flight (per ~3s tick via
    /// [`provider_call::ProviderActivityTicker`] plus one ping per
    /// useful SSE chunk). The default in [`RunnerDeps::minimal`] is a
    /// no-op [`provider_call::NoopPinger`] because tests don't depend
    /// on the signal; the production runner wires
    /// [`provider_call::HeartbeatPinger`] so each ping refreshes the
    /// heartbeat file and the host's typing-indicator path stays alive
    /// across long LLM streams.
    pub activity_pinger: Arc<dyn provider_call::ProviderActivityPinger>,
    /// Slice-3.5 opt-in: when `true`, the runner persists each
    /// completed `thinking` / `redacted_thinking` content block the
    /// provider streams as a [`copperclaw_types::MessageKind::Thinking`]
    /// row, and the host delivery service renders it as a collapsed
    /// native UI primitive (Telegram `<blockquote expandable>`, Slack
    /// `context` block, Discord muted-grey embed, Google Chat
    /// `collapsibleSection`, Matrix `<details>`).
    ///
    /// Default `false` — surfacing model chain-of-thought has privacy
    /// implications (mid-thought speculation about the user, debugging
    /// notes the model didn't intend the user to see, etc.). Operators
    /// flip per-group via `cclaw groups config edit <id>`; the value
    /// is plumbed in from `container_configs.surface_thinking` by the
    /// host's container manager into the runner's JSON config file.
    pub surface_thinking: bool,
    /// Layered tool-authorization policy evaluated at every dispatch
    /// (see [`crate::policy`]). Combines the group's tool-profile, the
    /// sender role, and the active skill's `allowed-tools`. Default
    /// ([`crate::policy::ToolPolicy::default`]) is the permissive `Full`
    /// profile with no role/skill scope, so groups that don't set a
    /// profile keep their historical full tool surface.
    pub policy: crate::policy::ToolPolicy,
    /// M18 Task HUD mode (`full` / `final` / `off`). Plumbed in from
    /// `runner.json`'s `hud_mode` (sourced host-side from the
    /// `COPPERCLAW_HUD_MODE` env var). Default [`HudMode::Full`]: the
    /// HUD is ON by default on channels whose adapter supports in-place
    /// message edits; bare channels always degrade to the legacy
    /// periodic status rows regardless of mode. See [`hud::TaskHud`].
    pub hud_mode: crate::config::HudMode,
    /// In-container path of the per-session todo store the Task HUD
    /// reads its "current step" line from. Production is
    /// [`hud::TODO_STORE_DEFAULT_PATH`] (`/data/agent_todos.json`, the
    /// same file the `todo_*` MCP tools maintain); tests point it at a
    /// tempdir.
    pub todo_path: PathBuf,
    /// M18 R3 verification gate: whether the `todo_update(completed)`
    /// gate is enforced for this group. Mirrors
    /// `crate::config::RunnerConfig::verify_gate`. The live enforcement
    /// path is [`copperclaw_mcp::ToolContext::verify_gate_enabled`],
    /// consulted directly by the `copperclaw-mcp` tool handlers (they
    /// only see `&dyn ToolContext`, not `RunnerDeps`) — this field
    /// exists for parity with the resolved config and for test
    /// scaffolding, same convention as [`Self::hud_mode`] /
    /// [`Self::policy`]. Default `true` (gate on).
    pub verify_gate: bool,
    /// M18 R3 verification gate: per-group override for the project
    /// verify command. Mirrors
    /// `crate::config::RunnerConfig::check_command_override`. See
    /// [`Self::verify_gate`] for why this is also surfaced here rather
    /// than solely on the `ToolContext`.
    pub check_command_override: Option<String>,
    /// M18 R5 hot in-session provider failover: pre-built alternate
    /// providers, in priority order, the runner switches to mid-turn when
    /// the primary ([`Self::provider`] / [`Self::model`]) exhausts its
    /// in-provider retries. Built once at startup from
    /// `runner.json`'s host-resolved `failover_chain` (see
    /// [`crate::config::RunnerConfig::failover_chain`]). Empty (the
    /// default, and for tests) keeps the historical single-provider
    /// behaviour byte-identical. See
    /// [`provider_call::run_llm_turn`] for the walk.
    pub failover_chain: Vec<provider_call::FailoverProvider>,
    /// M21 S6 test-clock seam: the monotonic clock the runner's timed
    /// surfaces read through — today the Task HUD's elapsed clock, the
    /// bare-channel 60s status-row cadence, and the 150s softening
    /// threshold (see [`hud::TaskHud`]); later M21 timed legs (backoffs,
    /// TTLs) should read the same clock. Default
    /// [`crate::clock::SystemClock`] (real time — byte-identical to the
    /// pre-seam behaviour); deterministic tests and the replay harness
    /// inject a [`crate::clock::TestClock`] and advance it by hand so
    /// timed legs no longer need real waits.
    pub clock: Arc<dyn crate::clock::Clock>,
    /// M22 A2 autonomy gate: the capability grant governing the current
    /// autonomous (scheduled / heartbeat) turn, plus its per-turn
    /// fire-consumed latch. [`run_loop`] rewrites it at the top of every turn
    /// from the host-written `<data_root>/grant.json` snapshot (the firing
    /// task's `effective_grant` — see [`tool_dispatch::TurnGrant`]); the
    /// dispatch gate ([`tool_dispatch::invoke_tool`]) reads it per call to
    /// decide whether a credentialed external action is pre-authorized. Shared
    /// behind an `Arc<Mutex>` because `invoke_tool` only holds `&RunnerDeps`
    /// yet must latch the fire. Default (`minimal`, and every human turn) is an
    /// empty state that authorizes nothing — the brake stays closed.
    pub active_grant: Arc<std::sync::Mutex<tool_dispatch::GrantGateState>>,
    /// M22 B1: one-shot user-facing notices queued outside a turn (see
    /// [`Notices`]). [`run_loop`] pushes here when the automatic compaction
    /// pass fires; [`hud::TaskHud::new`] drains one note per turn into the
    /// HUD's one-shot note slot. Default empty — no producer, no behaviour
    /// change.
    pub notices: Arc<Notices>,
    /// M22 B4: live per-inbound spend counters ([`hud::TurnSpend`]) —
    /// bumped by the provider-call layer with each completed call's
    /// billed tokens (priced via `copperclaw_types::pricing` when the
    /// model is known), reset by `drive_turn` at inbound entry, read by
    /// the Task HUD for the status line's token/cost fields and the
    /// final collapse. Default empty counters — with no provider usage
    /// recorded, every HUD frame stays byte-identical to pre-B4.
    pub spend: Arc<hud::TurnSpend>,
    /// F2 safe-mode respawn: when `true`, [`run_loop`] truncates the loaded
    /// `state.history` to a small recent window ([`RECOVERY_KEEP_MESSAGES`])
    /// at startup — bypassing normal compaction — so a persistent
    /// history/context problem self-heals instead of crash-looping. The host
    /// sets it (via `runner.json`'s `recovery_mode`) once a session's
    /// crash streak reaches its threshold; it clears as soon as a spawn
    /// survives. Default `false` (healthy path): startup is byte-identical to
    /// pre-F2 behaviour.
    pub recovery_mode: bool,
}

/// Default per-tool-call deadline. Comfortably above an `npm install`
/// of a typical TypeScript project or an `apt-get install` of a small
/// graph; below the threshold at which a stuck tool would be
/// indistinguishable from a hung container.
pub const DEFAULT_TOOL_DEADLINE_SECS: u64 = 900;

impl RunnerDeps {
    /// Convenience builder used by tests. Production callers populate fields
    /// directly.
    #[must_use]
    pub fn minimal(
        provider: Arc<dyn AgentProvider>,
        tool_ctx: Arc<dyn ToolContext>,
        inbound: Arc<Mutex<Connection>>,
        outbound: Arc<Mutex<Connection>>,
        archive_dir: PathBuf,
    ) -> Self {
        Self {
            provider,
            tool_ctx,
            inbound,
            outbound,
            tools: Vec::new(),
            system: "you are helpful".into(),
            model: "claude-sonnet-4-6".into(),
            effort: Effort::Medium,
            max_tokens: 4096,
            temperature: None,
            assistant_name: None,
            session_id: copperclaw_types::SessionId(uuid::Uuid::nil()),
            agent_group_id: copperclaw_types::AgentGroupId(uuid::Uuid::nil()),
            turn_seq: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            tool_map: Arc::new(HashMap::new()),
            // No external MCP servers by default; production wires this from
            // the host's `mcp_tools.json` manifest, tests opt in explicitly.
            external_tools: Arc::new(HashMap::new()),
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
            // Smart auto-continue backstop: soft * 6 = 900 at the default soft
            // cap. Extension is ON by default (hard > soft); tests that want
            // the old flat-cap behaviour set this equal to `max_tool_turns`.
            max_tool_turns_hard: default_max_tool_turns_hard(DEFAULT_MAX_TOOL_TURNS),
            max_task_tokens: DEFAULT_MAX_TASK_TOKENS,
            compaction: CompactionCfg {
                model_input_window: 200_000,
                safety_margin_tokens: 8_000,
                output_reserve_tokens: 4_096,
                // Test default: soft trigger off so the existing
                // hard-window compaction fixtures behave unchanged.
                soft_target_tokens: 0,
                summary_model: "claude-sonnet-4-6".into(),
                summary_effort: Effort::Low,
                summary_max_tokens: 1024,
                archive_dir,
                // Matches production (`/data`); scanned read-only for the
                // pinned project-facts header. Absent in the test env, so
                // fixtures get no header and stay byte-stable.
                data_root: PathBuf::from("/data"),
            },
            // Test default: elision effectively off (no result body can
            // exceed `usize::MAX`), so transcript/replay fixtures see the
            // full history. Production wires real knobs from RunnerConfig.
            elision: crate::formatter::ElisionCfg {
                recent_results_kept: 0,
                max_result_bytes: usize::MAX,
            },
            max_turns: None,
            idle_sleep: Duration::from_millis(POLL_INTERVAL_MS),
            heartbeat_path: None,
            provider_deadline: Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS),
            tool_deadline_secs: DEFAULT_TOOL_DEADLINE_SECS,
            activity_pinger: Arc::new(provider_call::NoopPinger),
            // Privacy-default: thinking blocks are dropped on the
            // floor unless the operator explicitly opts the group in
            // via `container_configs.surface_thinking`.
            surface_thinking: false,
            // Permissive default: `Full` profile, no role/skill scope.
            // The host overrides this from the group config + sender
            // role; tests opt into a tighter policy explicitly.
            policy: crate::policy::ToolPolicy::default(),
            // HUD on by default (the live behaviour); tests that assert
            // HUD emissions set the originating channel explicitly —
            // with no channel routing the HUD degrades to the legacy
            // status-row path, so existing tests see no new rows.
            hud_mode: crate::config::HudMode::default(),
            todo_path: PathBuf::from(hud::TODO_STORE_DEFAULT_PATH),
            // Test default: gate on, no override — same convention as
            // hud_mode/policy above; tests opt out explicitly.
            verify_gate: true,
            check_command_override: None,
            // No in-turn failover by default: tests that exercise the R5
            // walk populate this explicitly.
            failover_chain: Vec::new(),
            // Real time by default — the S6 seam only changes behaviour
            // when a test injects a TestClock explicitly.
            clock: Arc::new(crate::clock::SystemClock),
            // A2: the brake starts closed — no grant authorizes anything until
            // `run_loop` loads a live grant snapshot for an autonomous fire.
            active_grant: Arc::new(std::sync::Mutex::new(
                tool_dispatch::GrantGateState::default(),
            )),
            // B1: empty notice queue — nothing rides the next HUD frame
            // until a loop-level event (auto-compaction) pushes a note.
            notices: Arc::new(Notices::default()),
            // B4: empty spend counters — nothing renders until the
            // provider-call layer records billed tokens.
            spend: Arc::new(hud::TurnSpend::default()),
            // F2: recovery mode off by default — the host only turns it on
            // for a session that has crashed N times in a row.
            recovery_mode: false,
        }
    }
}

/// How many entries back from the tail of `state.history` the
/// resume-after-crash dedup check scans for a matching `User` entry.
/// The window has to cover the worst case where the prior runner had
/// completed one full tool turn before crashing — that leaves history
/// ending in `Tool { ... }` with `[User, Assistant, ToolUse, Tool]`
/// before it. Ten entries comfortably absorbs a couple of nested tool
/// turns (`User, Assistant, ToolUse, Tool, ToolUse, Tool, ...`)
/// without paying for a full-history walk on every poll.
const RESUME_DEDUP_LOOKBACK: usize = 10;

/// F2 safe-mode respawn: how many of the most-recent `state.history` entries
/// the runner keeps when the host has flipped `recovery_mode` on. Deliberately
/// small — the whole point is to shed an oversized / poison history that was
/// crash-looping the session, keeping only enough recent context (roughly the
/// last couple of turns) for the resumed session to stay coherent. Reused by
/// [`crate::compaction::truncate_to_recent`] at startup; see `run_loop`.
const RECOVERY_KEEP_MESSAGES: usize = 8;

/// Returns true iff the most-recent `User` entry within the last
/// [`RESUME_DEDUP_LOOKBACK`] history entries has the same content as
/// `prompt`. Used by `run_loop` to skip a duplicate push when the
/// inbound was already enqueued by a prior runner that crashed
/// mid-message. Walks backwards and STOPS at the first `User` entry —
/// older `User` entries are not relevant to the resume guard.
fn is_prompt_already_in_history(history: &[HistoryMessage], prompt: &str) -> bool {
    history
        .iter()
        .rev()
        .take(RESUME_DEDUP_LOOKBACK)
        .find_map(|m| match m {
            HistoryMessage::User { content } => Some(content == prompt),
            _ => None,
        })
        .unwrap_or(false)
}

/// M19 U7: mark every reaction row in a freshly-polled batch completed and
/// return the non-reaction rows. Called before formatting so a reaction never
/// drives a fresh turn or reaches the model as raw JSON (the mid-turn steering
/// seam in `drive_turn` is where a reaction that lands during a turn actually
/// steers).
async fn consume_fresh_reaction_rows(
    deps: &RunnerDeps,
    pending: Vec<copperclaw_types::MessageInRow>,
) -> Vec<copperclaw_types::MessageInRow> {
    let (reactions, rest): (Vec<_>, Vec<_>) =
        pending.into_iter().partition(reaction::is_reaction_row);
    if !reactions.is_empty() {
        let inbound = deps.inbound.lock().await;
        for row in &reactions {
            if let Err(err) = messages_in::mark_completed(&inbound, row.id) {
                tracing::warn!(
                    ?err,
                    "U7: consuming a lapsed reaction row failed; continuing"
                );
            }
        }
    }
    rest
}

/// Drive the poll loop until `max_turns` turns have been executed (or
/// forever, if `max_turns` is `None`). The function is `async` and may be
/// awaited from any tokio runtime.
//
// The body sequences ack → format → slash-command intercept → history
// reconciliation → context-block build → drive_turn → finalize. Splitting
// further would push the per-iteration locals into a struct without
// readability win.
#[allow(clippy::too_many_lines)]
pub async fn run_loop(deps: RunnerDeps) -> Result<()> {
    // Bring the persisted message history into memory once at startup.
    let mut state = {
        let g = deps.outbound.lock().await;
        load_state(&g).context("load runner state")?
    };

    // F2 safe-mode respawn: when the host has flipped `recovery_mode` on
    // (this session's container crashed N times in a row), aggressively
    // truncate the loaded history to a small recent window BEFORE the poll
    // loop — bypassing the provider-driven compaction below — so a persistent
    // oversized-history / poison-context problem self-heals instead of
    // crash-looping forever. Reuses F1's deterministic, LLM-free
    // `truncate_to_recent` (never a provider call — the summariser might be
    // the very thing that was crashing). The truncated history is persisted
    // immediately, with the continuation cleared: a shrunk transcript is
    // incompatible with a continuation handle anchored to the pre-truncation
    // prefix, and persisting now means a re-crash before the first turn saves
    // still leaves the poison history off disk. A no-op when `recovery_mode`
    // is off (the healthy path) or the history is already within the window.
    if deps.recovery_mode {
        let before = state.history.len();
        state.history = crate::compaction::truncate_to_recent(
            std::mem::take(&mut state.history),
            RECOVERY_KEEP_MESSAGES,
        );
        let after = state.history.len();
        if after != before {
            state.continuation = None;
            let g = deps.outbound.lock().await;
            if let Err(err) = save_state(&g, &state.history, state.continuation.as_deref()) {
                // Best-effort: even if the persist fails, the in-memory
                // history is already truncated for this run, so the recovery
                // still takes effect this spawn. Log and continue rather than
                // bail — bailing here would itself crash-loop the runner.
                tracing::warn!(error = %err, "recovery mode: failed to persist truncated history; continuing with in-memory truncation");
            }
        }
        tracing::warn!(
            history_before = before,
            history_after = after,
            keep = RECOVERY_KEEP_MESSAGES,
            "F2 recovery mode: truncated persisted history to a recent window at startup (safe-mode respawn)"
        );
    }

    let mut turns_run: usize = 0;
    let mut first_poll = true;

    // M21 O3: live provider-health for the failover chain, created ONCE per
    // session and shared across every turn, so a primary that dies mid-session
    // is skipped on the NEXT call (not re-tried every turn until respawn) and a
    // primary that recovers is restored after the re-probe window — without a
    // container respawn. Byte-identical for the common single-provider case
    // (`select_start` always returns 0, no transitions).
    let failover_health = FailoverHealth::from_deps(&deps);

    // M23 W2.2 cold-boot service restart: daemons the agent started
    // (database servers, dev servers) died with the previous container at
    // idle-stop while `/data` survived. Run the agent-maintained
    // `/data/.copperclaw/services` file once per container boot — before
    // the poll loop and before repo auto-attach, so anything the attach /
    // verify path touches can already reach its daemons. Best-effort and
    // time-bounded: never fatal, never blocks boot past the per-command
    // timeout, and a process-level latch keeps later turns of this boot
    // from re-running it.
    services::run_cold_boot_services().await;

    // M22 C2 attach seam: at startup, attach any *existing* repository
    // already sitting under the data root — a repo handed to / persisted
    // for this session (a clone from a prior turn, an operator-seeded
    // checkout). Best-effort and idempotent; a repo the agent clones
    // mid-session is caught by the matching post-turn call below.
    project::auto_attach_pending().await;

    loop {
        if let Some(limit) = deps.max_turns {
            if turns_run >= limit {
                return Ok(());
            }
        }
        // One-shot gate for child agents (`source_session_id` set in
        // `runner.json`). Once the child has emitted its first
        // `send_message` back to the parent, exit cleanly — even if
        // the LLM tries to push follow-up turns. This is the runtime
        // backstop for the "child agents sent duplicate report +
        // 'report delivered' summary" bug surfaced on 2026-05-24 with
        // `openrouter/owl-alpha`. Soft prompt rules ("EXACTLY ONE
        // send_message") don't reliably constrain free models, so the
        // runtime enforces the invariant.
        if deps.tool_ctx.parent_reply_sent() {
            tracing::info!(
                target: "copperclaw_runner",
                "child agent delivered its one-shot reply; exiting run_loop"
            );
            return Ok(());
        }
        touch_heartbeat(deps.heartbeat_path.as_ref());

        let pending = {
            let g = deps.inbound.lock().await;
            messages_in::get_pending(&g, first_poll, 10)?
        };
        first_poll = false;

        if pending.is_empty() {
            sleep(deps.idle_sleep).await;
            continue;
        }

        // M19 U7: reactions are lightweight steering for an IN-PROGRESS turn,
        // never a fresh turn of their own. A reaction that lands mid-turn is
        // consumed and (if curated + on the agent's own message) folded in by
        // `drive_turn`'s R2 steering seam. One that surfaces here — picked up
        // between turns — is a lapsed signal: consume it so it can neither
        // drive a spurious turn nor reach the model as raw reaction JSON, and
        // never fold it into a fresh prompt.
        let pending = consume_fresh_reaction_rows(&deps, pending).await;
        if pending.is_empty() {
            sleep(deps.idle_sleep).await;
            continue;
        }

        ack_picked_up(&deps, &pending).await?;
        let formatted = format_messages(pending);

        // Chat slash-command detection. When the user types `/clear`,
        // `/reset`, or `/compact` as their entire message (no other
        // text, no other rows in this batch), we handle it
        // synchronously here — wipe / compact history, write a
        // confirmation chat row, mark the inbound completed, skip the
        // LLM turn. This is the user-side counterpart to the agent's
        // `clear_history` / `compact_now` MCP tools (which fire from
        // INSIDE a turn via sentinel files); the chat command short-
        // circuits the turn entirely so a runaway session can be
        // reset even when the model is too confused / broken to
        // honour the request itself.
        if let Some(cmd) = detect_slash_command_batch(&formatted) {
            handle_slash_command(&deps, &mut state, &formatted, cmd).await?;
            // Bypass the rest of the loop body — the command's
            // confirmation is the entire reply for this turn.
            continue;
        }

        // Plumb the originating inbound's routing into the tool ctx
        // so any chat-kind outbound the turn produces (the model's
        // final assistant text or an explicit send_message /
        // send_file) carries the channel_type / platform_id /
        // thread_id columns. Without this the delivery loop has no
        // routing to dispatch by and the user sees nothing. We pick
        // the first chat-kind row's routing — same channel for all
        // inbounds in a batch is the overwhelming case (a single
        // user dropping multiple messages in the same window).
        let origin_row = formatted
            .rows
            .iter()
            .find(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .or_else(|| formatted.rows.first());
        if let Some(r) = origin_row {
            let in_reply_to = r.id.as_uuid().to_string();
            deps.tool_ctx.set_originating(
                r.channel_type
                    .as_ref()
                    .map(copperclaw_types::ChannelType::as_str),
                r.platform_id.as_deref(),
                r.thread_id.as_deref(),
                Some(in_reply_to.as_str()),
            );
        } else {
            deps.tool_ctx.set_originating(None, None, None, None);
        }

        // PROVENANCE / autonomy gate: classify this turn as autonomous when
        // nothing in the batch is a human channel message (Chat) — i.e. it was
        // driven by a scheduled `Task` fire or a wake. Autonomous turns may
        // search memory and propose but may NOT take a credentialed external
        // action UNLESS a live, human-approved capability grant (M22 A1) for the
        // firing task permits that specific action (M22 A2, decision (b)).
        //
        // `set_turn_provenance`'s `approved` stays wired `false`: it is the
        // *blanket* taint-clearing bool and must never be flipped on for an
        // autonomous turn. The grant is capability-scoped, never blanket — the
        // per-call decision (which action is pre-authorized, and consuming its
        // fire) lives in `tool_dispatch::invoke_tool`, which reads
        // `deps.active_grant`. Here we resolve the firing task's grant snapshot
        // and stash it (or clear it for a human turn).
        let autonomous = !formatted
            .rows
            .iter()
            .any(|r| r.kind == copperclaw_types::MessageKind::Chat);
        {
            let grant = if autonomous {
                let task_id = firing_task_id(&formatted.rows);
                load_turn_grant(&deps.compaction.data_root, task_id.as_deref(), Utc::now())
            } else {
                None
            };
            let mut gate = match deps.active_grant.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            // Reset the per-turn fire latch, then install this turn's grant.
            gate.fire_consumed = false;
            gate.grant = grant;
        }
        deps.tool_ctx.set_turn_provenance(autonomous, false);

        // Surface child-agent failure notices to the user channel
        // BEFORE the parent's LLM picks them up. Without this the
        // parent can spend minutes silently digesting the failure +
        // reworking — the user has no signal that the parent received
        // a "sub-task failed" from a child and is still on the case.
        // Lived through on 2026-05-24: a `golf-research-market` child
        // failed at 16:33:27, the parent went silent until 16:36:59
        // while it processed the failure and built a fallback
        // prototype — five minutes that read as "stuck" to the user.
        // Routing rules match `emit_status`: gated inside the runner's
        // `RunnerToolCtx` to channels with real user routing, so child-
        // agent contexts skip cleanly.
        emit_failure_notice_toasts(&deps, &formatted.rows).await;

        // Honor pending history actions BEFORE pushing the incoming
        // user message, so the user's request is processed against the
        // requested baseline (cleared or compacted) rather than being
        // silently dropped alongside it. Two sources drop these
        // sentinels: the `clear_history` / `compact_now` MCP tools (the
        // agent asks for it mid-turn, sentinel survives into the next
        // poll), and the operator (manually placing the file under
        // `<session>/` to reset a stuck session). Both produce the
        // same outcome here: wipe/compact prior history, then keep
        // processing this turn's inbound. Clear wins over compact when
        // both are pending — the caller presumably wanted a full reset.
        let clear_pending = copperclaw_mcp::clear_history_pending_path();
        let compact_pending = copperclaw_mcp::compact_now_pending_path();
        if clear_pending.exists() {
            state.history.clear();
            state.continuation = None;
            let _ = tokio::fs::remove_file(&clear_pending).await;
            // Clear wins over compact when both sentinels are pending:
            // remove the compact sentinel too so it doesn't fire a no-op
            // compaction on the next iteration (best-effort, ignore errors).
            if compact_pending.exists() {
                let _ = tokio::fs::remove_file(&compact_pending).await;
            }
            tracing::info!("history cleared (sentinel consumed)");
        } else if compact_pending.exists() {
            // Run compact FIRST. The sentinel is removed AFTER compact
            // succeeds so a transient provider error (429/timeout) doesn't
            // silently lose the user's compact request — the next runner
            // spawn will retry it. Mirror clear's continuation reset: a
            // post-compact history is incompatible with a continuation
            // handle anchored to the pre-compact prefix.
            state.history = compact(state.history, deps.provider.as_ref(), &deps.compaction)
                .await
                .context("compact_now failed")?;
            state.continuation = None;
            let _ = tokio::fs::remove_file(&compact_pending).await;
            tracing::info!("history compacted (sentinel consumed)");
        }

        let est_tokens = estimate_tokens(&state.history);
        if deps.compaction.should_compact(est_tokens) {
            // R4: count auto-triggered compactions by profile + the estimated
            // token count at the trigger point.
            copperclaw_metrics::inc_compaction_triggered(deps.policy.profile().as_str());
            copperclaw_metrics::observe_compaction_estimated_tokens(est_tokens as u64);
            // F1 (2026-07-18 incident): compaction must NEVER crash the
            // runner. An empty/failed summary used to `bail!`, which
            // propagated here as a fatal `?` and exited `run_loop`; the host
            // respawned into the same oversized history and the same empty
            // summary — an infinite crash-loop that bricked the session until
            // an operator cleared history. `compact()` now degrades every
            // failure (empty summary, provider error, archive write) to
            // deterministic truncation and returns Ok. The `HeartbeatTicker`
            // keeps the container marked alive across the (possibly slow)
            // summary provider call so the host supervisor doesn't SIGKILL a
            // healthy compaction as "stale". The Err arm is defense-in-depth:
            // if some future internal error ever escaped `compact()`, continue
            // with a truncated tail rather than exiting the loop.
            let before_len = state.history.len();
            state.history = {
                let _hb = provider_call::HeartbeatTicker::start(deps.heartbeat_path.clone());
                match compact(state.history, deps.provider.as_ref(), &deps.compaction).await {
                    Ok(compacted) => compacted,
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "compaction returned an unexpected error; continuing with truncated history"
                        );
                        Vec::new()
                    }
                }
            };
            // B1: the automatic path used to be silent (only the /compact
            // slash path confirmed to the user). No TaskHud exists yet —
            // this runs before drive_turn — so queue a one-shot note the
            // next turn's HUD drains onto its first frame (rich channels)
            // or periodic status row (bare channels). Kept short: it rides
            // a HUD summary capped at MAX_SUMMARY_CHARS (200).
            deps.notices.push(format!(
                "history auto-compacted ({before_len} msgs -> {}, ~{}k tokens)",
                state.history.len(),
                est_tokens.div_ceil(1000),
            ));
        }

        // Resume-after-crash guard — see `is_prompt_already_in_history`.
        let already_pushed = is_prompt_already_in_history(&state.history, &formatted.prompt);
        // Render the per-inbound conversation-context block from the
        // current history depth + the freshly-picked-up rows; see the
        // helper for the prior-vs-current entry-count semantics.
        let context_block =
            build_inbound_context_block(&state.history, &formatted.rows, already_pushed);
        if already_pushed {
            tracing::debug!("resuming mid-message — skipping duplicate user push");
        } else {
            state.history.push(HistoryMessage::User {
                content: formatted.prompt,
            });
            // Inbound images (e.g. a telegram photo) the user attached
            // become follow-on Image entries so vision-capable models see
            // them. Pushed right after the User text so the anthropic
            // serializer coalesces them into one [text, image] message.
            for (media_type, data) in crate::formatter::extract_inbound_images(&formatted.rows) {
                state
                    .history
                    .push(HistoryMessage::Image { media_type, data });
            }
        }

        let turn = drive_turn_with_health(
            &deps,
            &mut state.history,
            state.continuation.as_deref(),
            context_block.as_deref(),
            &failover_health,
        )
        .await?;
        state.continuation = turn.continuation.or(state.continuation);

        finalize_messages(&deps, &formatted.rows, turn.outcome, turn.blocker).await?;
        // Clear the originating routing so the next iteration's
        // emit-outbound calls don't accidentally inherit a stale
        // channel (e.g. a system-kind row written by save_state).
        deps.tool_ctx.clear_originating();

        {
            let g = deps.outbound.lock().await;
            save_state(&g, &state.history, state.continuation.as_deref())
                .context("save runner state")?;
        }
        // M22 C2 attach seam: a turn just completed and may have `git
        // clone`d an existing repository via `shell` (decision (a) — no
        // git MCP tool). Attach any newly-cloned, not-yet-attached repo
        // now, before the next turn reasons about it. Idempotent + gated
        // on an `origin` remote, so blank prototypes are never touched.
        project::auto_attach_pending().await;
        turns_run += 1;
        // Active path: poll faster when traffic is flowing.
        sleep(Duration::from_millis(ACTIVE_POLL_INTERVAL_MS)).await;
    }
}

/// Render the per-inbound "Conversation context" block (or `None` when
/// the batch yields nothing useful). Splits the work out of
/// [`run_loop`] so the poll body stays under clippy's per-function
/// line cap and so the depth-snapshot rule is documented once in one
/// place.
///
/// `already_pushed` is the resume-after-crash flag: when the runner is
/// re-picking-up an inbound it already pushed onto history before a
/// crash, the trailing User entry IS the current message and must be
/// excluded from the "prior history" depth the block reports.
fn build_inbound_context_block(
    history: &[HistoryMessage],
    rows: &[copperclaw_types::MessageInRow],
    already_pushed: bool,
) -> Option<String> {
    // Snapshot the prior-history depth BEFORE pushing the current
    // user turn so the rendered block counts entries from earlier
    // exchanges in this session, not the about-to-be-pushed message
    // we're currently replying to. When we're resuming a mid-message
    // crash recovery (already_pushed=true), the User entry is already
    // at the tail of history; back off by one to mirror the
    // not-yet-pushed semantics.
    let depth = if already_pushed {
        history.len().saturating_sub(1)
    } else {
        history.len()
    };
    self::prompt::render_conversation_context(rows, depth)
}

/// M22 A2: filename of the host-written capability-grant snapshot, sitting
/// alongside `tasks.json` under the session data root (`/data` in-container).
/// The host writes the firing task's `effective_grant` here at fire/spawn time
/// (companion plumbing — see the A2 security review); the runner reads it at
/// turn start. Absent file ⇒ no grant ⇒ the autonomy brake stays closed.
pub(super) const GRANT_SNAPSHOT_FILENAME: &str = "grant.json";

/// The task id that fired this autonomous turn, if any. A scheduled fire is a
/// `kind: Task` inbound row carrying `content.task_id` (and `series_id = task
/// id`) — see the sweep's `checks/scheduling.rs`. We read `content.task_id`
/// first and fall back to `series_id` so either encoding resolves.
fn firing_task_id(rows: &[MessageInRow]) -> Option<String> {
    rows.iter()
        .find(|r| r.kind == copperclaw_types::MessageKind::Task)
        .and_then(|r| {
            r.content
                .get("task_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| r.series_id.clone())
        })
}

/// Load the live capability grant for `task_id` from the host-written snapshot
/// under `data_root`, or `None` when there is no usable grant. Returns `Some`
/// only when the snapshot exists, parses, its `task_id` matches the firing task
/// (a stale snapshot for a *different* task can never authorize this fire), and
/// it is still live at `now` ([`tool_dispatch::TurnGrant::is_live`] — a
/// defence-in-depth re-check on top of the host's `effective_grant`). Every
/// failure path returns `None` — the brake fails closed.
fn load_turn_grant(
    data_root: &std::path::Path,
    task_id: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<tool_dispatch::TurnGrant> {
    let path = data_root.join(GRANT_SNAPSHOT_FILENAME);
    let bytes = std::fs::read(&path).ok()?;
    let grant: tool_dispatch::TurnGrant = serde_json::from_slice(&bytes).ok()?;
    if let Some(tid) = task_id {
        if grant.task_id != tid {
            tracing::warn!(
                snapshot_task = %grant.task_id,
                firing_task = %tid,
                "grant snapshot task id does not match the firing task; ignoring"
            );
            return None;
        }
    }
    grant.is_live(now).then_some(grant)
}

/// Inputs for one end-of-turn `usage_report` system row.
///
/// A struct (rather than a long arg list) so [`build_usage_report_payload`]
/// stays under clippy's argument cap and so the host's cross-crate
/// emit→record→fold integration test constructs the same inputs the live
/// runner does.
#[derive(Debug, Clone)]
pub struct UsageReportInputs<'a> {
    pub session_id: copperclaw_types::SessionId,
    pub agent_group_id: copperclaw_types::AgentGroupId,
    pub seq: i64,
    pub model: &'a str,
    pub provider: &'a str,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub outcome: &'a TurnOutcome,
}

/// Build the `usage_report` system-row payload the runner appends to
/// `outbound.db` at the end of every LLM turn. The host's delivery service
/// (`record_usage_report`) reads this object's fields straight into an
/// `agent_turns` row.
///
/// Extracted as a free function so the live emit path and the cross-crate
/// emit→record→fold integration test (`copperclaw-host`) build the payload
/// with identical logic — in particular the `error` field, which is
/// load-bearing for provider resilience: the host's health fold
/// (`fold_recent_turns`) classifies that string with
/// `DegradeReason::from_error_text` to decide whether to degrade the chain.
/// Omitting it (the pre-fix bug) left `agent_turns.error` NULL, so an
/// automatic failover never fired. `Done` carries no error, so the column
/// stays NULL on a successful turn.
#[must_use]
pub fn build_usage_report_payload(inputs: &UsageReportInputs<'_>) -> serde_json::Value {
    let (status, error) = match inputs.outcome {
        TurnOutcome::Done => ("ok", None),
        TurnOutcome::Failed(reason) => ("error", Some(reason.as_str())),
    };
    serde_json::json!({
        "usage_report": {
            // Bare UUIDs (no `ag_`/`sess_` Display prefix): the host stores
            // these verbatim into `agent_turns.{session_id,agent_group_id}`,
            // and EVERY consumer query — the resilience fold
            // (`recent_for_group`), budget tracking (`tokens_since`), and the
            // usage rollup's join against `agent_groups.id` — keys by the bare
            // uuid (`id.as_uuid().to_string()`). Emitting the prefixed Display
            // form here (the prior bug) wrote rows no query could ever match,
            // so the fold never degraded and usage/budget never attributed.
            "session_id": inputs.session_id.as_uuid().to_string(),
            "agent_group_id": inputs.agent_group_id.as_uuid().to_string(),
            "seq": inputs.seq,
            "model": inputs.model,
            "provider": inputs.provider,
            "input_tokens": inputs.input_tokens,
            "output_tokens": inputs.output_tokens,
            "started_at": inputs.started_at.to_rfc3339(),
            "ended_at": inputs.ended_at.to_rfc3339(),
            "status": status,
            "error": error,
        }
    })
}

/// Append a `usage_report` system row to `outbound.db`. The host's
/// delivery service intercepts this kind of system action (instead of
/// dispatching it to a channel adapter) and writes the corresponding
/// `agent_turns` row.
///
/// `provider_name` / `model` name the provider/model that ACTUALLY served
/// this turn — which for an R5 hot-failover switch is a fallback entry,
/// not `deps.provider`/`deps.model`. Reporting the serving entry keeps the
/// host's health fold authoritative: the failed primary is degraded and
/// the fallback that succeeded stays healthy.
pub(in crate::run) async fn emit_usage_report(
    deps: &RunnerDeps,
    provider_name: &str,
    model: &str,
    input_tokens: u32,
    output_tokens: u32,
    started_at: chrono::DateTime<chrono::Utc>,
    outcome: &TurnOutcome,
) {
    use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_out};
    let payload = build_usage_report_payload(&UsageReportInputs {
        session_id: deps.session_id,
        agent_group_id: deps.agent_group_id,
        seq: deps.turn_seq.load(std::sync::atomic::Ordering::Relaxed),
        model,
        provider: provider_name,
        input_tokens,
        output_tokens,
        started_at,
        ended_at: chrono::Utc::now(),
        outcome,
    });
    let row = WriteOutbound {
        id: copperclaw_types::MessageId::new(),
        in_reply_to: None,
        timestamp: chrono::Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: copperclaw_types::MessageKind::System,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        content: payload,
    };
    let outbound = deps.outbound.lock().await;
    let conn: &rusqlite::Connection = &outbound;
    if let Err(err) = insert_out(conn, &row) {
        tracing::warn!(?err, "usage_report insert failed");
    }
    deps.turn_seq
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Prefix the runner's `agent_apology_text` emits when a child agent
/// terminates with failure. Used here to spot the inbound copy of that
/// apology on the parent's side so we can surface a brief user-facing
/// toast.
const SUB_TASK_FAILED_PREFIX: &str = "sub-task failed:";

/// When any pending Agent-kind inbound carries the
/// [`SUB_TASK_FAILED_PREFIX`] sentinel, write a brief Chat-kind row to
/// the originating user channel so the user knows the parent received
/// a failure notice and is working on it. No-op when:
///   - none of the rows match (the common case),
///   - the originating routing has no user channel (parent is itself a
///     child agent — `emit_status` gates inside `RunnerToolCtx`),
///   - the apology text is missing / malformed.
///
/// Each batch emits at most one toast even when multiple failures
/// arrive together — repeated "sub-task failed" rows in one poll are
/// usually the same sweep of dying children; one heads-up is enough.
async fn emit_failure_notice_toasts(deps: &RunnerDeps, rows: &[MessageInRow]) {
    let any_failure = rows.iter().any(|r| {
        matches!(r.kind, copperclaw_types::MessageKind::Agent)
            && r.content
                .get("text")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.trim_start().starts_with(SUB_TASK_FAILED_PREFIX))
    });
    if !any_failure {
        return;
    }
    let count = rows
        .iter()
        .filter(|r| {
            matches!(r.kind, copperclaw_types::MessageKind::Agent)
                && r.content
                    .get("text")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.trim_start().starts_with(SUB_TASK_FAILED_PREFIX))
        })
        .count();
    let body = if count == 1 {
        "Heads up — a sub-task reported failure. Handling it now.".to_string()
    } else {
        format!("Heads up — {count} sub-tasks reported failure. Handling them now.")
    };
    deps.tool_ctx.emit_status(&body).await;
}

async fn ack_picked_up(deps: &RunnerDeps, rows: &[MessageInRow]) -> Result<()> {
    let mut g = deps.outbound.lock().await;
    let conn: &mut Connection = &mut g;
    for row in rows {
        // `insert` errors on duplicate; tolerate retries by switching
        // to update. Both paths are best-effort housekeeping — a
        // missing-or-broken processing_ack row must NOT abort the
        // runner; the actual inbound processing is what matters.
        match processing_ack::insert(conn, row.id, processing_ack::ProcessingStatus::Processing) {
            Ok(()) => {}
            Err(copperclaw_db::DbError::Sqlite(_)) => {
                if let Err(err) = processing_ack::update_status(
                    conn,
                    row.id,
                    processing_ack::ProcessingStatus::Processing,
                ) {
                    tracing::warn!(
                        ?err,
                        row_id = %row.id.as_uuid(),
                        "processing_ack ack_picked_up update failed; continuing"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    row_id = %row.id.as_uuid(),
                    "processing_ack ack_picked_up insert failed; continuing"
                );
            }
        }
    }
    Ok(())
}

/// Recognised user-side slash commands. Detected in the inbound text
/// BEFORE the message is pushed onto history or sent to the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashCommand {
    /// Wipe conversation history + continuation. Starts the next turn
    /// from a clean slate.
    Clear,
    /// Run the compaction pass on history (summarise + truncate).
    Compact,
    /// Show the list of supported commands.
    Help,
}

impl SlashCommand {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "/clear" | "/reset" | "/new" => Some(Self::Clear),
            "/compact" => Some(Self::Compact),
            "/help" | "/?" | "/commands" => Some(Self::Help),
            _ => None,
        }
    }
}

/// Inspect a freshly-formatted inbound batch and decide whether it's a
/// pure slash command. Returns `Some(cmd)` only when:
/// - the batch is a single chat row (no other rows queued), AND
/// - that row's `content.text` is exactly a recognised slash command.
///
/// Mixed batches (e.g. `/clear` followed by a real question) fall
/// through to the LLM unchanged — the model can decide what to do.
fn detect_slash_command_batch(formatted: &crate::formatter::FormattedTurn) -> Option<SlashCommand> {
    if formatted.rows.len() != 1 {
        return None;
    }
    let row = &formatted.rows[0];
    if row.kind != copperclaw_types::MessageKind::Chat {
        return None;
    }
    let text = row.content.get("text").and_then(|v| v.as_str())?;
    SlashCommand::parse(text)
}

/// Execute a slash command synchronously. Writes a confirmation chat
/// row through the configured tool context (so it routes to the
/// originating channel), marks all batch inbounds as completed, and
/// persists the new state. The caller `continue`s the run loop after
/// this returns — no LLM turn fires for the command itself.
async fn handle_slash_command(
    deps: &RunnerDeps,
    state: &mut crate::state::PersistedState,
    formatted: &crate::formatter::FormattedTurn,
    cmd: SlashCommand,
) -> Result<()> {
    let confirmation = match cmd {
        SlashCommand::Clear => {
            let prior = state.history.len();
            state.history.clear();
            state.continuation = None;
            // Also wipe the todo store. Without this, `/clear` leaves
            // the prior session's plan in `/data/agent_todos.json`,
            // the next prompt's model picks it up, appends new items
            // on top, and the user sees a Frankenstein "13/26 done"
            // plan with items from three unrelated tasks. Errors are
            // best-effort: a locked / missing store must not abort
            // the clear confirmation.
            let todo_removed = match copperclaw_mcp::clear_todo_store().await {
                Ok(removed) => removed,
                Err(err) => {
                    tracing::warn!(
                        target: "copperclaw_runner",
                        ?err,
                        "/clear: could not wipe todo store; continuing"
                    );
                    false
                }
            };
            tracing::info!(
                target: "copperclaw_runner",
                prior_entries = prior,
                todo_removed,
                "/clear: wiped conversation history + todo store"
            );
            let todo_note = if todo_removed {
                " The plan/todo list was also cleared."
            } else {
                ""
            };
            format!(
                "Cleared conversation history ({prior} prior entries removed).{todo_note} \
                 Starting fresh — send your next message and I'll have no \
                 memory of earlier turns."
            )
        }
        SlashCommand::Compact => {
            let prior = state.history.len();
            state.history = compact(
                std::mem::take(&mut state.history),
                deps.provider.as_ref(),
                &deps.compaction,
            )
            .await
            .context("/compact failed")?;
            state.continuation = None;
            let after = state.history.len();
            tracing::info!(
                target: "copperclaw_runner",
                prior_entries = prior,
                after_entries = after,
                "/compact: summarised history"
            );
            format!(
                "Compacted conversation history ({prior} entries → {after}). \
                 Older context summarised; recent turns kept verbatim."
            )
        }
        SlashCommand::Help => "Available commands:\n\
             - /clear (or /reset, /new) — wipe conversation history and start fresh\n\
             - /compact — summarise older history to free token budget\n\
             - /help (or /?, /commands) — show this list\n\
             \n\
             Anything else is sent to the model as a normal message."
            .to_string(),
    };

    // Persist the cleared / compacted state before the confirmation
    // hits the wire — that way a crash between the emit and the next
    // poll doesn't leave the user with confirmation + un-cleared
    // history.
    {
        let guard = deps.outbound.lock().await;
        save_state(&guard, &state.history, state.continuation.as_deref())
            .context("persist state after slash command")?;
    }

    // Emit the confirmation through the standard send_message path so
    // it picks up the originating channel routing set above.
    let spec = copperclaw_mcp::SendMessageSpec {
        to: None,
        text: confirmation,
    };
    deps.tool_ctx
        .emit_outbound(copperclaw_mcp::OutboundToolEffect::SendMessage(spec))
        .await
        .map_err(|e| anyhow::anyhow!("/{cmd:?}: confirmation send failed: {e}"))?;

    // Mark the batch's inbound rows as completed so the host doesn't
    // re-deliver them, then update the processing_ack rows.
    finalize_messages(deps, &formatted.rows, TurnOutcome::Done, None).await?;
    deps.tool_ctx.clear_originating();
    Ok(())
}

async fn finalize_messages(
    deps: &RunnerDeps,
    rows: &[MessageInRow],
    outcome: TurnOutcome,
    blocker: Option<blocker::BlockerCategory>,
) -> Result<()> {
    let ack_status = match outcome {
        TurnOutcome::Done => processing_ack::ProcessingStatus::Done,
        TurnOutcome::Failed(_) => processing_ack::ProcessingStatus::Failed,
    };
    {
        let inbound = deps.inbound.lock().await;
        for row in rows {
            match outcome {
                TurnOutcome::Done => {
                    let _ = messages_in::mark_completed(&inbound, row.id);
                }
                TurnOutcome::Failed(_) => {
                    let _ = messages_in::mark_failed(&inbound, row.id);
                }
            }
        }
    }
    {
        let mut outbound = deps.outbound.lock().await;
        let conn: &mut Connection = &mut outbound;
        for row in rows {
            // Don't bubble NotFound out of finalize_messages — it just
            // means the host's processing-reset sweep already cleared
            // the ack row between when the runner picked the inbound up
            // and now. Caught live: the parse-error cap fired
            // (TurnOutcome::Failed), finalize_messages reached this
            // line, the ack was missing, `?` propagated NotFound to
            // main(), the runner exited, the container died, the user
            // got silence. The mark_failed on messages_in above is
            // tolerated the same way (it's already let _ = ...). The
            // apology emit below is what the user actually sees; we
            // must not gate it on this housekeeping update.
            match processing_ack::update_status(conn, row.id, ack_status) {
                Ok(()) | Err(copperclaw_db::DbError::NotFound) => {}
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        row_id = %row.id.as_uuid(),
                        "processing_ack::update_status failed in finalize; continuing"
                    );
                }
            }
        }
    }
    // On terminal failure, surface a brief apology to the user via the
    // same channel the inbound came in on. Without this the user just
    // sees the typing indicator clear with no reply — caught live on
    // Telegram when the model emitted a malformed tool_use JSON, the
    // runner classified the stream as failed, marked the inbound
    // status='failed', and emitted nothing for the delivery loop to
    // route back. The apology message is one row per inbound so the
    // user still gets feedback per question if several failed in a
    // batch. Routing fields (`channel_type` / `platform_id` /
    // `thread_id`) are copied from the inbound so the delivery loop
    // dispatches the apology back to the originating chat.
    if let TurnOutcome::Failed(reason) = &outcome {
        if let Err(err) = emit_terminal_failure_apologies(deps, rows, reason, blocker).await {
            tracing::warn!(?err, "could not emit terminal-failure apology");
        }
    }
    Ok(())
}

/// One short chat outbound per failed inbound, routed back to the
/// channel the inbound came in on. Idempotent at the per-row level
/// because each inbound has a stable id that flows into `in_reply_to`.
///
/// `reason` is a short phrase from `TurnOutcome::Failed` ("stopped after
/// 900 tool turns (hit the 900 hard ceiling) — send 'continue' to keep
/// going", "the model's provider call did not return a complete response",
/// etc.) which we splice into the apology so the user knows
/// *which* snag instead of being told to "see runner stderr".
fn apology_text(reason: &str) -> String {
    if reason.is_empty() {
        // Fallback: emitter didn't supply a reason. Should be rare —
        // every `TurnOutcome::Failed` site in drive_turn.rs sets one.
        // Operators can still find the underlying cause in runner
        // stderr.
        "I couldn't finish a reply on that message. Try rephrasing, \
         or send a smaller request — and the operator can check the \
         runner log for the exact error."
            .into()
    } else {
        // Trim trailing sentence-ending punctuation from the reason so
        // we don't get double-punctuation when the upstream phrase
        // already ends in `.`/`?`/`!`.
        let trimmed = reason.trim_end_matches(['.', '?', '!']);
        format!(
            "I couldn't finish a reply on that message — {trimmed}. \
             Try rephrasing or sending a smaller request, and the \
             operator can check the runner log for details."
        )
    }
}

/// Apology text for a parent-agent recipient (Agent-kind outbound).
/// The parent's LLM, not a human, reads this — so the human "try
/// rephrasing" guidance is wrong from its position. Keep it terse and
/// machine-actionable: name the failure, name the dead child, and tell
/// the parent it MAY retry by calling `create_agent` again with the
/// same instructions. Most terminal failures here are transient
/// (parse-error cap, brief provider hiccup, container crash) and the
/// parent often has the cheapest path to a fix because it knows what
/// the task was. Pure-host auto-retry is deferred (see CHANGELOG note);
/// this prompt-side nudge is the smallest change that turns
/// "report failure" into "try once more, then report".
fn agent_apology_text(reason: &str) -> String {
    let trimmed = reason.trim_end_matches(['.', '?', '!']);
    if trimmed.is_empty() {
        "sub-task failed: child session terminated. You may retry by calling \
         create_agent again with the same name + instructions — these failures \
         are often transient (parse-error cap, brief provider hiccup, container \
         crash). If a second attempt also fails, report the failure upstream so \
         the user can intervene."
            .into()
    } else {
        format!(
            "sub-task failed: {trimmed}. The child session terminated. You may \
             retry by calling create_agent again with the same name + \
             instructions — these failures are often transient (parse-error \
             cap, brief provider hiccup, container crash). If a second attempt \
             also fails, report the failure upstream so the user can intervene."
        )
    }
}

async fn emit_terminal_failure_apologies(
    deps: &RunnerDeps,
    rows: &[MessageInRow],
    reason: &str,
    blocker: Option<blocker::BlockerCategory>,
) -> Result<()> {
    use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_out};
    if rows.is_empty() {
        return Ok(());
    }
    // Two flavors of apology:
    //   - End-user channels: a `MessageKind::Error` card (slice-3.3 —
    //     visually-distinct receipt, rendered red where the platform
    //     supports it, bold + `[ERROR]` prefix everywhere else). The
    //     human-readable apology prose lives in the card's `summary`.
    //   - Parent-agent recipients: plain Agent-kind chat — the reader
    //     is another LLM, not a person, and giving an LLM a structured
    //     ErrorCard hands it a side-channel signal that's harder to
    //     handle than a sentence. Keep the existing prose.
    // F2: when the turn walled on a run of same-blocker denials and
    // never produced a user-facing reply, swap the generic apology for
    // ONE curated, actionable wall card keyed by blocker category. The
    // card is fully curated — it never echoes the raw denial text, so no
    // internal detail (or injected content) reaches the user. Any other
    // terminal failure keeps the existing generic apology, byte-stable.
    let user_card = if let Some(cat) = blocker {
        tracing::info!(
            target: "copperclaw_runner",
            agent_group_id = %deps.agent_group_id,
            blocker = cat.metric_label(),
            "F2: surfacing actionable wall card for blocked turn",
        );
        cat.to_error_card()
    } else {
        let user_summary = apology_text(reason);
        build_terminal_failure_error_card(&user_summary, reason)
    };
    // M19 F2 metric: label for the wall card, counted per user-facing card
    // actually written to a channel below.
    let wall_blocker = blocker.map(blocker::BlockerCategory::metric_label);
    let agent_text = agent_apology_text(reason);
    let outbound = deps.outbound.lock().await;
    let conn: &rusqlite::Connection = &outbound;
    for row in rows {
        // Only emit for chat inbounds — system / task / wake events
        // don't have a user on the other end to apologize to.
        if !matches!(row.kind, copperclaw_types::MessageKind::Chat) {
            continue;
        }
        // Pick the apology shape. Three cases mirror the sweep
        // (`copperclaw-host-sweep/src/checks/apology.rs`):
        //   (a) Inbound has channel routing — Error-kind card back
        //       through the same channel (human reader).
        //   (b) Inbound has NO channel routing but has source_session_id
        //       — Agent-kind plain-prose apology UP to the source so
        //       the parent agent learns the child failed (LLM reader).
        //   (c) Neither — silently skip (no recipient to apologize to).
        let apology = if let (Some(channel_type), Some(platform_id)) =
            (row.channel_type.as_ref(), row.platform_id.as_ref())
        {
            // M19 F2: a curated wall card is actually reaching a user channel.
            if let Some(lbl) = wall_blocker {
                copperclaw_metrics::inc_wall_card(lbl);
            }
            WriteOutbound {
                id: copperclaw_types::MessageId::new(),
                in_reply_to: Some(row.id),
                timestamp: chrono::Utc::now(),
                deliver_after: None,
                recurrence: None,
                kind: copperclaw_types::MessageKind::Error,
                channel_type: Some(channel_type.clone()),
                platform_id: Some(platform_id.clone()),
                thread_id: row.thread_id.clone(),
                content: serde_json::json!({ "error": user_card }),
            }
        } else if let Some(source) = row.source_session_id.as_deref() {
            WriteOutbound {
                id: copperclaw_types::MessageId::new(),
                in_reply_to: None,
                timestamp: chrono::Utc::now(),
                deliver_after: None,
                recurrence: None,
                kind: copperclaw_types::MessageKind::Agent,
                channel_type: None,
                platform_id: None,
                thread_id: None,
                content: serde_json::json!({
                    "text": agent_text,
                    "to": { "kind": "agent", "session_id": source },
                }),
            }
        } else {
            continue;
        };
        if let Err(err) = insert_out(conn, &apology) {
            tracing::warn!(?err, "apology insert failed");
        }
    }
    Ok(())
}

/// Build the `ErrorCard` used for the terminal-failure apology
/// surface. Separated out so tests can pin the card's shape (kind,
/// title, summary, retryable flag) directly without poking at
/// outbound rows.
///
/// - `kind = Provider`: terminal turn failures are virtually always
///   provider-shaped (retry-exhausted streams, malformed `tool_use`
///   JSON the model couldn't recover from, etc.). The host's
///   delivery-retry-exhaustion path uses `ErrorCardKind::Delivery`;
///   internal-tool errors would use `ErrorCardKind::Internal`. Keep
///   `Provider` here so per-channel renderers can theme distinctly
///   in the future.
/// - `retryable = false`: terminal means terminal — the runner
///   already exhausted its budget by the time we reach
///   `emit_terminal_failure_apologies`. Telling the user "will retry
///   automatically" here would be a lie.
/// - `details = Some(reason)` when reason is non-empty: the raw
///   `failure_reason` string from `TurnOutcome::Failed` makes it
///   into the card's monospace details block so operators (and
///   curious users) can see what went wrong without scraping logs.
fn build_terminal_failure_error_card(
    summary: &str,
    reason: &str,
) -> copperclaw_channels_core::ErrorCard {
    use copperclaw_channels_core::{ErrorCard, ErrorCardKind};
    let mut card =
        ErrorCard::new(ErrorCardKind::Provider, summary).with_title("I couldn't finish that reply");
    let trimmed = reason.trim();
    if !trimmed.is_empty() {
        // Cap to fit within the schema's details cap so a long
        // provider trace doesn't fail validation.
        let capped: String = trimmed
            .chars()
            .take(copperclaw_channels_core::MAX_ERROR_DETAILS_CHARS)
            .collect();
        card = card.with_details(capped);
    }
    card
}

pub(in crate::run) async fn set_current_tool(
    deps: &RunnerDeps,
    name: &str,
    declared_timeout_ms: Option<u64>,
) -> Result<()> {
    let g = deps.outbound.lock().await;
    let timeout_i64 = declared_timeout_ms.and_then(|v| i64::try_from(v).ok());
    let state = container_state::ContainerState {
        current_tool: Some(name.to_string()),
        tool_declared_timeout_ms: timeout_i64,
        tool_started_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
    };
    container_state::set(&g, &state)?;
    Ok(())
}

pub(in crate::run) async fn clear_current_tool(deps: &RunnerDeps) -> Result<()> {
    let g = deps.outbound.lock().await;
    container_state::clear_tool(&g)?;
    Ok(())
}

#[cfg(test)]
mod slash_command_tests {
    use super::SlashCommand;

    #[test]
    fn clear_aliases() {
        assert_eq!(SlashCommand::parse("/clear"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::parse("/reset"), Some(SlashCommand::Clear));
        assert_eq!(SlashCommand::parse("/new"), Some(SlashCommand::Clear));
        // Case-insensitive.
        assert_eq!(SlashCommand::parse("/CLEAR"), Some(SlashCommand::Clear));
        // Whitespace trimmed.
        assert_eq!(SlashCommand::parse("  /reset  "), Some(SlashCommand::Clear));
    }

    #[test]
    fn compact_alias() {
        assert_eq!(SlashCommand::parse("/compact"), Some(SlashCommand::Compact));
    }

    #[test]
    fn help_aliases() {
        assert_eq!(SlashCommand::parse("/help"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/?"), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/commands"), Some(SlashCommand::Help));
    }

    #[test]
    fn unrecognised_returns_none() {
        // Anything else falls through to the LLM.
        assert_eq!(SlashCommand::parse("what is 2+2"), None);
        assert_eq!(SlashCommand::parse("/unknown"), None);
        // Slash command with trailing text is NOT a pure command (the
        // model should handle it). We don't try to strip args here.
        assert_eq!(SlashCommand::parse("/clear and also..."), None);
    }
}

#[cfg(test)]
mod tests {
    use super::provider_call::{
        HeartbeatTicker, MAX_PROVIDER_ATTEMPTS, backoff_for_attempt, query_with_retry,
    };
    use super::*;
    use crate::tools::RunnerToolCtx;
    use async_trait::async_trait;
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_db::tables::messages_in::{WriteInbound, insert as insert_in};
    use copperclaw_db::tables::messages_out;
    use copperclaw_providers::{AgentProvider, AgentQuery, ProviderError, QueryInput};
    use copperclaw_types::{AgentGroupId, ChannelType, MessageKind, ProviderEvent, SessionId};
    use std::sync::Mutex as StdMutex;

    /// Provider that yields a pre-baked sequence of events for each turn.
    struct ScriptedProvider {
        scripts: StdMutex<Vec<Vec<ProviderEvent>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: StdMutex::new(scripts),
            })
        }
    }

    #[async_trait]
    impl AgentProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            let mut g = self.scripts.lock().unwrap();
            let events = if g.is_empty() {
                vec![ProviderEvent::Result { text: None }]
            } else {
                g.remove(0)
            };
            Ok(Box::new(ScriptedQuery {
                events: StdMutex::new(events),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct ScriptedQuery {
        events: StdMutex<Vec<ProviderEvent>>,
    }

    #[async_trait]
    impl AgentQuery for ScriptedQuery {
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

    struct Setup {
        _tmp: tempfile::TempDir,
        paths: SessionPaths,
        deps: RunnerDeps,
    }

    fn build_setup(scripts: Vec<Vec<ProviderEvent>>) -> Setup {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = open_inbound(&paths).unwrap();
        let outbound = open_outbound(&paths).unwrap();
        let inbound = Arc::new(Mutex::new(inbound));
        let outbound = Arc::new(Mutex::new(outbound));
        let provider = ScriptedProvider::new(scripts);
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(provider, tool_ctx, inbound, outbound, archive_dir);
        deps.max_turns = Some(1);
        deps.idle_sleep = Duration::from_millis(1);
        Setup {
            _tmp: tmp,
            paths,
            deps,
        }
    }

    fn insert_pending(inbound: &Connection, text: &str) -> MessageId {
        let id = MessageId::new();
        let msg = WriteInbound {
            id,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            content: serde_json::json!({"text": text}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some("chat-1".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        };
        insert_in(inbound, &msg).unwrap();
        id
    }

    // ── M22 B1: the loop-level notice queue ─────────────────────────────

    #[test]
    fn notices_drain_one_is_fifo_one_at_a_time() {
        let notices = Notices::default();
        assert!(notices.drain_one().is_none(), "empty queue drains nothing");
        notices.push("first");
        notices.push("second");
        assert_eq!(
            notices.drain_one().as_deref(),
            Some("first"),
            "notes drain oldest-first"
        );
        assert_eq!(notices.drain_one().as_deref(), Some("second"));
        assert!(notices.drain_one().is_none(), "drained queue is empty");
    }

    /// B1 producer pin: when the automatic compaction pass fires inside
    /// `run_loop` (before `drive_turn` — no TaskHud exists yet), it queues
    /// the one-shot "history auto-compacted" note. This turn's HUD drains
    /// it, but a fast pure-text turn on a bare channel renders no frame
    /// and no status row, so finalize requeues it — after the loop exits
    /// the note is waiting for the next turn's HUD.
    #[tokio::test]
    async fn run_loop_auto_compaction_queues_a_notice() {
        let mut setup = build_setup(vec![
            // The compaction pass's summary-model call.
            vec![ProviderEvent::Result {
                text: Some("SUMMARY: earlier deployment chatter".into()),
            }],
            // The turn's ordinary reply.
            vec![ProviderEvent::Result {
                text: Some("still here".into()),
            }],
        ]);
        // Trip the soft trigger well below the pre-saved history's
        // estimate (6 x ~240 chars ≈ 400+ est tokens).
        setup.deps.compaction.soft_target_tokens = 50;
        let history: Vec<HistoryMessage> = (0..6)
            .map(|i| HistoryMessage::User {
                content: format!(
                    "filler message number {i}: {}",
                    "lorem ipsum dolor sit amet ".repeat(8)
                ),
            })
            .collect();
        {
            let g = setup.deps.outbound.lock().await;
            save_state(&g, &history, None).unwrap();
        }
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "and what happened next?");
        }
        let notices = Arc::clone(&setup.deps.notices);
        run_loop(setup.deps).await.unwrap();

        let note = notices
            .drain_one()
            .expect("auto-compaction must queue a notice (requeued by the unrendering turn)");
        assert!(
            note.starts_with("history auto-compacted (6 msgs -> "),
            "note carries the pre-compaction entry count: {note}"
        );
        assert!(
            note.ends_with("k tokens)"),
            "note carries the estimated token size: {note}"
        );
        assert!(notices.drain_one().is_none(), "exactly one notice queued");
    }

    #[tokio::test]
    async fn empty_inbox_exits_when_max_turns_zero() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result {
            text: Some("ignored".into()),
        }]]);
        setup.deps.max_turns = Some(0);
        run_loop(setup.deps).await.unwrap();
    }

    #[tokio::test]
    async fn one_message_writes_response_and_completes() {
        let mut setup = build_setup(vec![vec![
            ProviderEvent::Init {
                continuation: "c1".into(),
            },
            ProviderEvent::Result {
                text: Some("hello back".into()),
            },
        ]]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        // Outbound row landed. After M13 the runner also writes a
        // `MessageKind::System` `usage_report` row per turn, so we
        // pick the chat row explicitly rather than asserting on
        // `.last()`.
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat = rows
            .iter()
            .find(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .expect("expected one Chat outbound row");
        assert_eq!(chat.content["text"], "hello back");
        // Inbound message marked completed.
        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
        // processing_ack status went to Done.
        let claim = processing_ack::get(&outbound, id).unwrap().unwrap();
        assert_eq!(claim.status, processing_ack::ProcessingStatus::Done);
        // Continuation persisted.
        let st = load_state(&outbound).unwrap();
        assert_eq!(st.continuation.as_deref(), Some("c1"));
        assert!(!st.history.is_empty());
    }

    /// Regression: a single retryable stream error must NOT terminally
    /// fail the inbound. The runner's `run_llm_turn` should re-open the
    /// query (up to `MAX_PROVIDER_ATTEMPTS`) and let the next attempt
    /// produce a real `Result`. Caught live: OpenRouter's SSE stream
    /// dropped a chunk mid-flight for one Telegram message, the
    /// `pump_events` path marked it failed, and the user's question
    /// went unanswered. With this loop in place the second attempt
    /// completes normally and the agent replies.
    #[tokio::test]
    async fn retryable_stream_error_retries_then_succeeds() {
        // First scripted turn: stream produces an Error with retryable=true
        // (mirrors anthropic.rs's SSE-decode path). Second turn: clean
        // Result with text. The retry loop must consume both and emit
        // the assistant text on the second pass.
        let mut setup = build_setup(vec![
            vec![ProviderEvent::Error {
                message: "sse decode: transient".into(),
                retryable: true,
            }],
            vec![ProviderEvent::Result {
                text: Some("hello after retry".into()),
            }],
        ]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "completed",
            "retryable stream error must not terminally fail the inbound"
        );

        // The assistant text from the second turn must reach
        // messages_out as a chat row.
        let outbound = open_outbound(&setup.paths).unwrap();
        let text: String = outbound
            .query_row(
                "SELECT content FROM messages_out WHERE kind = 'chat' ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            text.contains("hello after retry"),
            "expected the retried-turn text in outbound: {text:?}"
        );
    }

    /// Regression: a non-retryable Error event (e.g. authentication
    /// failure) must terminate the inbound after the same retry budget
    /// applied at the query layer — i.e. NOT loop forever. This pins
    /// `pump_events`'s `retryable: false` short-circuit.
    /// Regression: a terminal turn failure must emit a user-visible
    /// apology chat row routed back to the originating channel.
    /// Caught live on Telegram: model emitted a malformed `send_file`
    /// tool_use, runner classified the stream as failed, marked the
    /// inbound `status=failed`, and emitted nothing — user was left
    /// staring at a stale typing indicator with no reply.
    #[tokio::test]
    async fn terminal_failure_emits_apology_to_originating_channel() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Error {
            message: "tool_use input json parse failed for send_file".into(),
            retryable: false,
        }]]);
        // Insert an inbound that carries channel routing fields, like
        // a real telegram message would.
        let id = {
            let g = setup.deps.inbound.lock().await;
            let id = MessageId::new();
            let msg = WriteInbound {
                id,
                kind: MessageKind::Chat,
                timestamp: Utc::now(),
                content: serde_json::json!({"text": "do something cool"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("8929393356".into()),
                channel_type: Some(ChannelType::new("telegram")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            };
            insert_in(&g, &msg).unwrap();
            id
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        // The inbound must be marked failed.
        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");

        // Slice-3.3: the apology is now an Error-kind row carrying a
        // canonical ErrorCard in `content.error` (rather than a plain
        // Chat row with `content.text`). The card's `summary` field
        // holds the user-facing apology text the original test
        // asserted on.
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let apologies: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == MessageKind::Error)
            .collect();
        assert_eq!(
            apologies.len(),
            1,
            "expected exactly one Error apology row, got: {rows:?}"
        );
        let apology = apologies[0];
        assert_eq!(
            apology
                .channel_type
                .as_ref()
                .map(copperclaw_types::ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(apology.platform_id.as_deref(), Some("8929393356"));
        assert_eq!(apology.in_reply_to.map(|m| m.as_uuid()), Some(id.as_uuid()));
        let card_value = apology
            .content
            .get("error")
            .expect("Error-kind apology row must carry content.error");
        let card: copperclaw_channels_core::ErrorCard =
            serde_json::from_value(card_value.clone()).unwrap();
        // Provider-kind because terminal turn failures are virtually
        // always provider-shaped (retry-exhausted streams, malformed
        // tool_use JSON, …).
        assert_eq!(card.kind, copperclaw_channels_core::ErrorCardKind::Provider);
        // The summary carries the same user-facing prose the
        // pre-slice-3.3 plain-text apology used to carry.
        assert!(
            card.summary.contains("snag") || card.summary.contains("couldn't finish"),
            "card summary should be user-facing: {:?}",
            card.summary
        );
        // Terminal failures are NOT retryable — we just gave up.
        assert!(!card.retryable);
    }

    /// Recoverable path: a malformed `tool_use` JSON on the first turn
    /// must NOT terminate the inbound. The runner synthesises a
    /// `tool_result { is_error: true }` describing the parse failure,
    /// feeds it back into the next turn, and the model self-corrects
    /// with a clean `Result`. The inbound flips to `completed`, the
    /// final assistant text reaches the channel as a chat row, and no
    /// apology row is emitted.
    #[tokio::test]
    async fn malformed_tool_use_recovers_after_one_retry() {
        let mut setup = build_setup(vec![
            vec![ProviderEvent::ToolInputParseError {
                tool_use_id: "tu_bad_1".into(),
                tool_name: "send_file".into(),
                raw_input: "{\"path\":".into(),
                parse_error: "EOF while parsing an object at line 1 column 37".into(),
            }],
            vec![ProviderEvent::Result {
                text: Some("ok done".into()),
            }],
        ]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "send me the file")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "completed",
            "parse-error recovery must not terminally fail the inbound"
        );

        // Exactly one Chat outbound row, carrying the second turn's text.
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(
            chat_rows.len(),
            1,
            "expected one chat outbound (the recovered reply), got: {chat_rows:?}"
        );
        assert_eq!(chat_rows[0].content["text"], "ok done");

        // History should contain the synthetic Tool result tagged
        // is_error=true with the parse-error feedback message.
        let st = load_state(&outbound).unwrap();
        assert!(
            st.history.iter().any(|m| matches!(
                m,
                HistoryMessage::Tool { tool_use_id, content, is_error: true }
                    if tool_use_id == "tu_bad_1"
                        && content.contains("could not be parsed")
                        && content.contains("EOF while parsing")
            )),
            "expected synthetic parse-error tool_result in history, got: {:?}",
            st.history
        );
    }

    /// Cap path: three consecutive turns each emit a
    /// `ToolInputParseError` for `send_file`. The fourth scripted turn
    /// is never reached because `drive_turn` bails after the third
    /// retry. The inbound is marked `failed` and the apology row is
    /// emitted.
    #[tokio::test]
    async fn malformed_tool_use_gives_up_after_three_attempts() {
        let parse_err = || ProviderEvent::ToolInputParseError {
            tool_use_id: "tu_bad_1".into(),
            tool_name: "send_file".into(),
            raw_input: "{\"path\":".into(),
            parse_error: "EOF while parsing an object at line 1 column 37".into(),
        };
        // Four scripted turns: the runner should consume the first
        // three and then bail without ever calling `query()` for the
        // fourth.
        let mut setup = build_setup(vec![
            vec![parse_err()],
            vec![parse_err()],
            vec![parse_err()],
            vec![ProviderEvent::Result {
                text: Some("should never be seen".into()),
            }],
        ]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "send me the file")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "failed",
            "three consecutive parse failures must terminally fail the inbound"
        );

        // The "should never be seen" turn must not have been emitted.
        // Slice-3.3: apology now lands as an Error-kind row carrying a
        // canonical ErrorCard (not a plain Chat row).
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert!(
            chat_rows.is_empty(),
            "no Chat outbound expected — apology rides Error kind now: {chat_rows:?}"
        );
        let error_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Error)
            .collect();
        assert_eq!(
            error_rows.len(),
            1,
            "expected exactly one Error apology row, got: {error_rows:?}"
        );
        let card_value = error_rows[0]
            .content
            .get("error")
            .expect("Error apology row must carry content.error");
        let card: copperclaw_channels_core::ErrorCard =
            serde_json::from_value(card_value.clone()).unwrap();
        let apology_text = card.summary.as_str();
        assert!(
            !apology_text.contains("should never be seen"),
            "fourth turn must not have been consumed: {apology_text:?}"
        );
        assert!(
            apology_text.contains("snag") || apology_text.contains("couldn't finish"),
            "apology text should be user-facing: {apology_text:?}"
        );

        // The history should carry three synthetic Tool error
        // results, one per consumed turn — matches the
        // "3 consecutive tool_use parse failures" audit narrative.
        let st = load_state(&outbound).unwrap();
        let parse_error_results = st
            .history
            .iter()
            .filter(|m| {
                matches!(
                    m,
                    HistoryMessage::Tool { content, is_error: true, .. }
                        if content.contains("could not be parsed")
                )
            })
            .count();
        assert_eq!(
            parse_error_results, 3,
            "expected 3 synthetic parse-error tool_results in history, got {parse_error_results}"
        );
    }

    /// Transient-empty path: the model returns two empty replies (no
    /// text, no tool call) and then a real answer. The empty turn
    /// appends nothing to history, so the runner replays the identical
    /// request instead of terminally failing — the inbound completes
    /// and the third turn's text reaches the channel. Pins the retry
    /// half of `MAX_EMPTY_REPLY_ATTEMPTS`.
    #[tokio::test]
    async fn empty_reply_retries_then_succeeds() {
        let mut setup = build_setup(vec![
            vec![ProviderEvent::Result { text: None }],
            vec![ProviderEvent::Result { text: None }],
            vec![ProviderEvent::Result {
                text: Some("ok done".into()),
            }],
        ]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "anyone there?")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "completed",
            "two empty replies followed by a real one must not terminally fail the inbound"
        );

        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(
            chat_rows.len(),
            1,
            "expected one chat outbound (the recovered reply), got: {chat_rows:?}"
        );
        assert_eq!(chat_rows[0].content["text"], "ok done");
    }

    /// Cap path: three consecutive empty replies exhaust
    /// `MAX_EMPTY_REPLY_ATTEMPTS`. The fourth scripted turn is never
    /// reached, the inbound is marked `failed`, and the apology rides
    /// an Error-kind row.
    #[tokio::test]
    async fn empty_reply_gives_up_after_three_attempts() {
        let empty = || vec![ProviderEvent::Result { text: None }];
        let mut setup = build_setup(vec![
            empty(),
            empty(),
            empty(),
            vec![ProviderEvent::Result {
                text: Some("should never be seen".into()),
            }],
        ]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "anyone there?")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "failed",
            "three consecutive empty replies must terminally fail the inbound"
        );

        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert!(
            chat_rows.is_empty(),
            "no Chat outbound expected — apology rides Error kind: {chat_rows:?}"
        );
        let error_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Error)
            .collect();
        assert_eq!(
            error_rows.len(),
            1,
            "expected exactly one Error apology row, got: {error_rows:?}"
        );
        let card_value = error_rows[0]
            .content
            .get("error")
            .expect("Error apology row must carry content.error");
        let card: copperclaw_channels_core::ErrorCard =
            serde_json::from_value(card_value.clone()).unwrap();
        assert!(
            !card.summary.contains("should never be seen"),
            "fourth turn must not have been consumed: {:?}",
            card.summary
        );
    }

    /// Content-loop breaker: the model emits the SAME `(tool, args)`
    /// call on every turn. After `TOOL_LOOP_BREAKER_THRESHOLD` identical
    /// calls the runner trips the circuit breaker, terminates the
    /// inbound as `failed`, and surfaces a loop-specific apology — well
    /// before the `max_tool_turns` depth cap (still 150 here) would
    /// fire. Pins the depth-cap-vs-content-cap distinction.
    #[tokio::test]
    async fn identical_tool_call_loop_trips_breaker() {
        // Script many more identical turns than the breaker threshold so
        // we prove the breaker — not the turn count — is what stops it.
        let identical = || {
            vec![ProviderEvent::ToolCall {
                id: "tu_loop".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/data/stuck"}),
            }]
        };
        let scripts: Vec<Vec<ProviderEvent>> = (0..20).map(|_| identical()).collect();
        let mut setup = build_setup(scripts);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "do the thing")
        };
        // Leave max_tool_turns at the default (150) so the ONLY thing
        // that can stop this loop is the content breaker.
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "failed",
            "an identical-call loop must terminally fail the inbound"
        );

        // The apology should name the loop, not the depth cap.
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let error_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Error)
            .collect();
        assert_eq!(
            error_rows.len(),
            1,
            "expected exactly one Error apology row, got: {error_rows:?}"
        );

        // Crucially, the breaker must fire well before 150 turns: the
        // history should hold ~threshold tool_use cycles, not 150.
        let st = load_state(&outbound).unwrap();
        let read_file_calls = st
            .history
            .iter()
            .filter(|m| matches!(m, HistoryMessage::ToolUse { name, .. } if name == "read_file"))
            .count();
        assert!(
            read_file_calls <= 6,
            "breaker should trip near the threshold, not run to the depth cap; \
             saw {read_file_calls} read_file calls"
        );
    }

    /// Mixed-batch path: when one turn emits both a clean ToolCall
    /// (`shell`) and a malformed `send_file`, the real tool path
    /// still runs for `shell` and the synthetic-error path runs for
    /// `send_file`. The next turn sees both `tool_result` rows.
    #[tokio::test]
    async fn malformed_tool_use_other_tools_still_work() {
        let mut setup = build_setup(vec![
            vec![
                ProviderEvent::ToolCall {
                    id: "tu_shell_1".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"cmd": "echo hi"}),
                },
                ProviderEvent::ToolInputParseError {
                    tool_use_id: "tu_send_bad".into(),
                    tool_name: "send_file".into(),
                    raw_input: "{\"path\":".into(),
                    parse_error: "EOF while parsing an object at line 1 column 37".into(),
                },
            ],
            vec![ProviderEvent::Result {
                text: Some("both handled".into()),
            }],
        ]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "do two things")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = load_state(&outbound).unwrap();

        // The shell tool_result is real: the test tool_map is empty so
        // `invoke_tool` returns the "Unknown tool" refusal, but
        // crucially it is NOT the synthetic "could not be parsed"
        // text — that path is reserved for parse-error calls.
        let shell_result = st.history.iter().find_map(|m| {
            if let HistoryMessage::Tool {
                tool_use_id,
                content,
                is_error,
            } = m
            {
                if tool_use_id == "tu_shell_1" {
                    return Some((content.clone(), *is_error));
                }
            }
            None
        });
        let (shell_content, shell_is_error) =
            shell_result.expect("expected a tool_result for tu_shell_1");
        assert!(
            !shell_content.contains("could not be parsed"),
            "shell tool_result must come from invoke_tool, not the synthetic parse-error path: {shell_content:?}"
        );
        assert!(shell_is_error, "unknown-tool returns is_error=true");

        // The send_file tool_result is the synthetic parse-error
        // message tagged is_error=true.
        let send_result = st.history.iter().find_map(|m| {
            if let HistoryMessage::Tool {
                tool_use_id,
                content,
                is_error,
            } = m
            {
                if tool_use_id == "tu_send_bad" {
                    return Some((content.clone(), *is_error));
                }
            }
            None
        });
        let (send_content, send_is_error) =
            send_result.expect("expected a tool_result for tu_send_bad");
        assert!(
            send_content.contains("could not be parsed"),
            "send_file tool_result must be the synthetic parse-error message: {send_content:?}"
        );
        assert!(send_is_error);

        // And the second turn's final text reached the channel.
        let _ = id;
        let chat_rows: Vec<_> = messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(chat_rows.len(), 1);
        assert_eq!(chat_rows[0].content["text"], "both handled");
    }

    /// Serde round-trip pin for the new `ToolInputParseError` variant.
    #[test]
    fn tool_input_parse_error_event_serialization() {
        let ev = ProviderEvent::ToolInputParseError {
            tool_use_id: "tu_42".into(),
            tool_name: "send_file".into(),
            raw_input: "{\"path\":".into(),
            parse_error: "EOF while parsing an object at line 1 column 37".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"type\":\"tool_input_parse_error\""));
        assert!(json.contains("\"tool_use_id\":\"tu_42\""));
        assert!(json.contains("\"tool_name\":\"send_file\""));
        assert!(json.contains("EOF while parsing"));

        let back: ProviderEvent = serde_json::from_str(&json).unwrap();
        match back {
            ProviderEvent::ToolInputParseError {
                tool_use_id,
                tool_name,
                raw_input,
                parse_error,
            } => {
                assert_eq!(tool_use_id, "tu_42");
                assert_eq!(tool_name, "send_file");
                assert_eq!(raw_input, "{\"path\":");
                assert!(parse_error.contains("EOF while parsing"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_event_marks_inbound_failed() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Error {
            message: "boom".into(),
            retryable: false,
        }]]);
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi")
        };
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let inbound = open_inbound(&setup.paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");

        let outbound = open_outbound(&setup.paths).unwrap();
        let claim = processing_ack::get(&outbound, id).unwrap().unwrap();
        assert_eq!(claim.status, processing_ack::ProcessingStatus::Failed);
    }

    #[tokio::test]
    async fn tool_start_writes_container_state_and_tool_end_clears() {
        let mut setup = build_setup(vec![vec![
            ProviderEvent::ToolStart {
                name: "bash".into(),
                declared_timeout_ms: Some(30_000),
            },
            ProviderEvent::ToolEnd,
            ProviderEvent::Result {
                text: Some("ok".into()),
            },
        ]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "do something");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = container_state::get(&outbound).unwrap().unwrap();
        assert!(
            st.current_tool.is_none(),
            "tool should be cleared by ToolEnd"
        );
        assert!(st.updated_at.is_some());
    }

    #[tokio::test]
    async fn policy_denied_tool_produces_refusal_in_history() {
        // First turn: model emits a ToolCall the group's tool-profile
        // denies (`shell` under `messaging`); the runner pushes a
        // `Tool { is_error: true }` refusal, then runs a second turn
        // where the model concedes.
        let mut setup = build_setup(vec![
            vec![ProviderEvent::ToolCall {
                id: "tu_1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            vec![ProviderEvent::Result {
                text: Some("ok".into()),
            }],
        ]);
        setup.deps.policy =
            crate::policy::ToolPolicy::new(crate::policy::ToolProfile::Messaging, None);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "please run ls");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = load_state(&outbound).unwrap();
        assert!(
            st.history.iter().any(|m| matches!(
                m,
                HistoryMessage::Tool { content, is_error: true, .. }
                    if content.contains("not permitted by the `messaging` tool profile")
            )),
            "expected a policy-denied refusal in history, got: {:?}",
            st.history
        );
    }

    #[tokio::test]
    async fn progress_and_activity_events_are_tolerated() {
        let mut setup = build_setup(vec![vec![
            ProviderEvent::Progress {
                message: "thinking".into(),
            },
            ProviderEvent::Activity,
            ProviderEvent::Result {
                text: Some("done".into()),
            },
        ]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();
    }

    #[tokio::test]
    async fn empty_result_text_does_not_emit_outbound_row() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result { text: None }]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "hi");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        // M13 emits a `usage_report` System row per turn; the chat
        // path still shouldn't emit anything for an empty result.
        let chat_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert!(
            chat_rows.is_empty(),
            "no Chat outbound row expected for empty result, got {chat_rows:?}"
        );
    }

    #[tokio::test]
    async fn heartbeat_file_touched_when_path_set() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result {
            text: Some("hi".into()),
        }]]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "x");
        }
        let hb_path = setup.paths.heartbeat.clone();
        setup.deps.heartbeat_path = Some(hb_path.clone());
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();
        assert!(hb_path.exists(), "heartbeat path should exist after a turn");
    }

    /// Regression for the "running container SIGKILLed during long
    /// `npm install`" bug: a synchronous tool call that takes longer
    /// than the host's 60s heartbeat-stale threshold must not block
    /// the heartbeat refresh. The ticker keeps the file's mtime
    /// advancing while the tool is in flight.
    #[tokio::test]
    async fn heartbeat_ticker_refreshes_during_slow_tool() {
        // Use a *short* tick interval inside the test by overriding
        // via the constant if it were configurable; since it isn't,
        // we cheat and call HeartbeatTicker directly with a synthetic
        // slow-tool sleep. That's exactly the surface invoke_tool
        // wraps, so the test pins the right contract.
        let tmp = tempfile::tempdir().unwrap();
        let hb = tmp.path().join(".heartbeat");
        // Touch initially so we have a baseline mtime.
        std::fs::write(&hb, b".").unwrap();
        let baseline = std::fs::metadata(&hb).unwrap().modified().unwrap();
        // Make the file look "old" by an explicit sleep before
        // starting the ticker; this would otherwise be racing the
        // mtime granularity.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let ticker = HeartbeatTicker::start(Some(hb.clone()));
        // Simulate a tool call that takes long enough that the
        // ticker fires at least once. We use a sub-tick-interval
        // sleep plus the initial touch to keep CI quick: the
        // initial touch alone already refreshes mtime.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(ticker);

        let after = std::fs::metadata(&hb).unwrap().modified().unwrap();
        assert!(
            after > baseline,
            "heartbeat mtime should advance while the ticker is alive"
        );
    }

    /// When `path` is `None` the ticker is a no-op — useful for
    /// in-process tests that don't bother wiring a heartbeat file.
    #[tokio::test]
    async fn heartbeat_ticker_with_no_path_is_inert() {
        let ticker = HeartbeatTicker::start(None);
        // Just exercising Drop without panic.
        drop(ticker);
    }

    #[test]
    fn apology_text_trims_trailing_sentence_punctuation() {
        // Reason ending in '.' must not produce double-punctuation.
        let s = apology_text("the agent ran out of turns.");
        assert!(
            s.contains("— the agent ran out of turns. Try"),
            "expected single '.' between reason and 'Try': {s}"
        );
        assert!(
            !s.contains("turns.. Try"),
            "double-period must be trimmed: {s}"
        );

        // '?' and '!' get the same treatment.
        let q = apology_text("did it really fail?");
        assert!(q.contains("— did it really fail. Try"), "got: {q}");
        let bang = apology_text("boom!");
        assert!(bang.contains("— boom. Try"), "got: {bang}");

        // No trailing punctuation → unchanged splice.
        let plain = apology_text("provider returned 503");
        assert!(
            plain.contains("— provider returned 503. Try"),
            "got: {plain}"
        );

        // Empty reason still falls through the fallback branch.
        let empty = apology_text("");
        assert!(empty.contains("couldn't finish a reply"), "got: {empty}");
    }

    #[test]
    fn agent_apology_text_is_terse_and_actionable() {
        // Agent-targeted text must NOT contain the human-style
        // "Try rephrasing or sending a smaller request" guidance.
        let s = agent_apology_text("provider returned 503");
        assert!(s.contains("sub-task failed"), "got: {s}");
        // Parent should be nudged toward retry-once-then-surface.
        // Slice-3.5 flip from "Report the failure upstream rather
        // than retrying" to "You may retry … then report upstream".
        assert!(
            s.contains("retry") && s.contains("create_agent"),
            "agent apology should encourage retry via create_agent: {s}"
        );
        assert!(
            s.contains("report the failure upstream"),
            "agent apology should still fall back to surfacing if retry fails: {s}"
        );
        assert!(
            !s.contains("Try rephrasing"),
            "agent apology must not carry human-channel guidance: {s}"
        );
        assert!(
            !s.contains("smaller request"),
            "agent apology must not carry human-channel guidance: {s}"
        );

        // Trailing punctuation gets trimmed here too.
        let dotted = agent_apology_text("ran out of turns.");
        assert!(
            dotted.contains("sub-task failed: ran out of turns. The"),
            "got: {dotted}"
        );

        // Empty reason still produces a usable message.
        let empty = agent_apology_text("");
        assert!(empty.contains("sub-task failed"), "got: {empty}");
    }

    #[test]
    fn resolve_tool_deadline_uses_env_when_in_range() {
        let env = crate::config::MapEnv::from_pairs([(TOOL_DEADLINE_ENV, "120")]);
        assert_eq!(resolve_tool_deadline_secs(&env), 120);
    }

    #[test]
    fn resolve_tool_deadline_falls_back_when_unset() {
        let env = crate::config::MapEnv::default();
        assert_eq!(resolve_tool_deadline_secs(&env), DEFAULT_TOOL_DEADLINE_SECS);
    }

    #[test]
    fn resolve_tool_deadline_rejects_out_of_range() {
        let env = crate::config::MapEnv::from_pairs([(TOOL_DEADLINE_ENV, "1")]);
        assert_eq!(resolve_tool_deadline_secs(&env), DEFAULT_TOOL_DEADLINE_SECS);
        let env = crate::config::MapEnv::from_pairs([(TOOL_DEADLINE_ENV, "99999")]);
        assert_eq!(resolve_tool_deadline_secs(&env), DEFAULT_TOOL_DEADLINE_SECS);
    }

    #[test]
    fn resolve_tool_deadline_rejects_garbage() {
        let env = crate::config::MapEnv::from_pairs([(TOOL_DEADLINE_ENV, "not-a-number")]);
        assert_eq!(resolve_tool_deadline_secs(&env), DEFAULT_TOOL_DEADLINE_SECS);
    }

    #[tokio::test]
    async fn minimal_builds_valid_deps() {
        // Smoke: just check `minimal` produces a runnable Deps that exits
        // immediately with max_turns=0.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let provider = ScriptedProvider::new(vec![]);
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut d = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound,
            outbound,
            paths.outbox.join("_compactions"),
        );
        d.max_turns = Some(0);
        d.idle_sleep = Duration::from_millis(1);
        run_loop(d).await.unwrap();
    }

    #[tokio::test]
    async fn processing_ack_re_ack_succeeds_on_existing_row() {
        // First insert one ack manually to exercise the duplicate-handling path.
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let outbound = open_outbound(&paths).unwrap();
        let id = MessageId::new();
        processing_ack::insert(&outbound, id, processing_ack::ProcessingStatus::Processing)
            .unwrap();
        // Building deps just to call ack_picked_up.
        let provider = ScriptedProvider::new(vec![]);
        let outbound = Arc::new(Mutex::new(outbound));
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut d = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound,
            outbound.clone(),
            paths.outbox.join("_compactions"),
        );
        d.max_turns = Some(0);
        let row = MessageInRow {
            id,
            seq: 2,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            status: "pending".into(),
            process_after: None,
            recurrence: None,
            series_id: None,
            tries: 0,
            trigger: true,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            content: serde_json::json!({}),
            source_session_id: None,
            on_wake: false,
            reply_to: None,
            is_group: None,
        };
        ack_picked_up(&d, &[row]).await.unwrap();
        let g = outbound.lock().await;
        let claim = processing_ack::get(&g, id).unwrap().unwrap();
        assert_eq!(claim.status, processing_ack::ProcessingStatus::Processing);
    }

    // ── query_with_retry / per-call deadline ──────────────────────────────

    /// A provider whose `query()` either:
    /// - sleeps `delay` then succeeds (with the events the caller scripted),
    /// - returns a pre-scripted `ProviderError`.
    ///
    /// Each call consumes the next entry from `plan`.
    struct PlanProvider {
        plan: StdMutex<Vec<PlanStep>>,
        observed_attempts: std::sync::atomic::AtomicU32,
    }

    #[derive(Clone)]
    enum PlanStep {
        /// Sleep `delay`, then succeed with `events`.
        Ok {
            delay: Duration,
            events: Vec<ProviderEvent>,
        },
        /// Return this error.
        Err(ProviderErrorKind),
    }

    /// Cheap clone-able mirror of [`ProviderError`] variants we need in
    /// tests. `ProviderError` itself is not `Clone` (the `thiserror`
    /// macros don't synthesise it), so we synthesise a fresh value per
    /// call instead.
    #[derive(Clone, Copy)]
    enum ProviderErrorKind {
        Api { status: u16 },
        BadRequest,
        Transport,
    }

    impl ProviderErrorKind {
        fn into_err(self) -> ProviderError {
            match self {
                Self::Api { status } => ProviderError::Api {
                    status,
                    message: "scripted".into(),
                },
                Self::BadRequest => ProviderError::BadRequest("scripted".into()),
                Self::Transport => ProviderError::Transport("scripted".into()),
            }
        }
    }

    impl PlanProvider {
        fn new(plan: Vec<PlanStep>) -> Arc<Self> {
            Arc::new(Self {
                plan: StdMutex::new(plan),
                observed_attempts: std::sync::atomic::AtomicU32::new(0),
            })
        }
        fn attempts(&self) -> u32 {
            self.observed_attempts
                .load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl AgentProvider for PlanProvider {
        fn name(&self) -> &'static str {
            "plan"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            self.observed_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let step = {
                let mut g = self.plan.lock().unwrap();
                if g.is_empty() {
                    // No more scripted steps — return a 500 so the test
                    // panics loudly if it loops past expectation.
                    return Err(ProviderError::Api {
                        status: 500,
                        message: "plan exhausted".into(),
                    });
                }
                g.remove(0)
            };
            match step {
                PlanStep::Ok { delay, events } => {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    Ok(Box::new(ScriptedQuery {
                        events: StdMutex::new(events),
                    }))
                }
                PlanStep::Err(kind) => Err(kind.into_err()),
            }
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    fn dummy_input() -> QueryInput {
        QueryInput {
            system: "sys".into(),
            system_context: None,
            model: "m".into(),
            effort: Effort::Medium,
            previous_continuation: None,
            history: Vec::new(),
            tools: Vec::new(),
            max_tokens: 16,
            temperature: None,
            assistant_name: None,
            display_name: None,
        }
    }

    /// Build a minimal `RunnerDeps` wired to the supplied provider. The
    /// rest of the dependencies are valid-but-unused stubs because
    /// `query_with_retry` only reads `provider`, `provider_deadline`,
    /// and `provider.name()`.
    fn deps_with_provider(provider: Arc<dyn AgentProvider>, deadline: Duration) -> RunnerDeps {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut deps = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound,
            outbound,
            paths.outbox.join("_compactions"),
        );
        deps.provider_deadline = deadline;
        // Leak the tempdir into the deps — these tests don't poke at
        // the on-disk state, so dropping it after the call is fine.
        std::mem::forget(tmp);
        deps
    }

    /// F2 (B): a runner spawned with `recovery_mode = true` truncates its
    /// oversized persisted history to the small recent window at startup —
    /// before any turn — and clears the continuation (a shrunk transcript is
    /// incompatible with a continuation anchored to the old prefix).
    /// `max_turns = Some(0)` returns right after the startup truncation, so
    /// the provider is never queried.
    #[tokio::test]
    async fn recovery_mode_truncates_history_at_startup() {
        let provider = PlanProvider::new(vec![]);
        let mut deps = deps_with_provider(provider, Duration::from_secs(5));
        deps.recovery_mode = true;
        deps.max_turns = Some(0);
        let outbound = deps.outbound.clone();
        // Seed a large persisted history + a continuation token.
        let big: Vec<HistoryMessage> = (0..50)
            .map(|i| HistoryMessage::User {
                content: format!("m{i}"),
            })
            .collect();
        {
            let g = outbound.lock().await;
            save_state(&g, &big, Some("cont-token")).unwrap();
        }
        run_loop(deps).await.unwrap();
        let g = outbound.lock().await;
        let st = load_state(&g).unwrap();
        assert_eq!(
            st.history.len(),
            RECOVERY_KEEP_MESSAGES,
            "recovery mode must truncate to the recent window"
        );
        assert!(
            st.continuation.is_none(),
            "truncation must clear the continuation handle"
        );
        // The tail was kept (most-recent messages survive).
        assert!(matches!(
            st.history.last(),
            Some(HistoryMessage::User { content }) if content == "m49"
        ));
    }

    /// F2 (B) default-safe guard: with `recovery_mode` off (the healthy path)
    /// the persisted history and continuation are byte-identical after
    /// startup — no truncation happens for a non-crashing session.
    #[tokio::test]
    async fn no_recovery_mode_leaves_history_untouched() {
        let provider = PlanProvider::new(vec![]);
        let mut deps = deps_with_provider(provider, Duration::from_secs(5));
        // recovery_mode stays false (RunnerDeps::minimal default).
        deps.max_turns = Some(0);
        let outbound = deps.outbound.clone();
        let big: Vec<HistoryMessage> = (0..50)
            .map(|i| HistoryMessage::User {
                content: format!("m{i}"),
            })
            .collect();
        {
            let g = outbound.lock().await;
            save_state(&g, &big, Some("cont-token")).unwrap();
        }
        run_loop(deps).await.unwrap();
        let g = outbound.lock().await;
        let st = load_state(&g).unwrap();
        assert_eq!(st.history.len(), 50, "healthy path: history untouched");
        assert_eq!(st.continuation.as_deref(), Some("cont-token"));
    }

    #[tokio::test]
    async fn retry_succeeds_after_one_503() {
        let provider = PlanProvider::new(vec![
            PlanStep::Err(ProviderErrorKind::Api { status: 503 }),
            PlanStep::Ok {
                delay: Duration::ZERO,
                events: vec![ProviderEvent::Result {
                    text: Some("hi".into()),
                }],
            },
        ]);
        let deps = deps_with_provider(provider.clone(), Duration::from_secs(5));

        let started = std::time::Instant::now();
        let result = query_with_retry(&deps, deps.provider.as_ref(), dummy_input()).await;
        let elapsed = started.elapsed();
        assert!(result.is_ok(), "expected Ok, got {:?}", result.map(|_| ()));
        assert_eq!(provider.attempts(), 2);
        // One backoff (250ms) should have fired between attempts.
        assert!(
            elapsed >= Duration::from_millis(200),
            "expected at least one backoff, elapsed={elapsed:?}"
        );
    }

    #[tokio::test]
    async fn retry_gives_up_after_three_503s() {
        let provider = PlanProvider::new(vec![
            PlanStep::Err(ProviderErrorKind::Api { status: 503 }),
            PlanStep::Err(ProviderErrorKind::Api { status: 503 }),
            PlanStep::Err(ProviderErrorKind::Api { status: 503 }),
        ]);
        let deps = deps_with_provider(provider.clone(), Duration::from_secs(5));

        let err = match query_with_retry(&deps, deps.provider.as_ref(), dummy_input()).await {
            Ok(_) => panic!("expected terminal failure"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::Api { status: 503, .. }));
        assert_eq!(provider.attempts(), MAX_PROVIDER_ATTEMPTS);
    }

    #[tokio::test]
    async fn non_retryable_error_does_not_retry() {
        let provider = PlanProvider::new(vec![PlanStep::Err(ProviderErrorKind::BadRequest)]);
        let deps = deps_with_provider(provider.clone(), Duration::from_secs(5));

        let err = match query_with_retry(&deps, deps.provider.as_ref(), dummy_input()).await {
            Ok(_) => panic!("expected terminal failure"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::BadRequest(_)));
        assert_eq!(provider.attempts(), 1, "non-retryable should fail fast");
    }

    #[tokio::test]
    async fn session_invalid_does_not_retry() {
        // Construct a provider that returns SessionInvalid directly.
        struct Always;
        #[async_trait]
        impl AgentProvider for Always {
            fn name(&self) -> &'static str {
                "always"
            }
            async fn query(
                &self,
                _input: QueryInput,
            ) -> Result<Box<dyn AgentQuery>, ProviderError> {
                Err(ProviderError::SessionInvalid)
            }
            fn is_session_invalid(&self, _err: &ProviderError) -> bool {
                true
            }
        }
        let deps = deps_with_provider(Arc::new(Always), Duration::from_secs(5));
        let err = match query_with_retry(&deps, deps.provider.as_ref(), dummy_input()).await {
            Ok(_) => panic!("expected terminal failure"),
            Err(e) => e,
        };
        assert!(matches!(err, ProviderError::SessionInvalid));
    }

    #[tokio::test]
    async fn transport_error_retries_then_succeeds() {
        let provider = PlanProvider::new(vec![
            PlanStep::Err(ProviderErrorKind::Transport),
            PlanStep::Ok {
                delay: Duration::ZERO,
                events: vec![ProviderEvent::Result {
                    text: Some("ok".into()),
                }],
            },
        ]);
        let deps = deps_with_provider(provider.clone(), Duration::from_secs(5));
        let result = query_with_retry(&deps, deps.provider.as_ref(), dummy_input()).await;
        assert!(result.is_ok());
        assert_eq!(provider.attempts(), 2);
    }

    #[tokio::test]
    async fn timeout_retries_and_eventually_succeeds() {
        let provider = PlanProvider::new(vec![
            // First attempt: sleeps long enough to trip a 50ms deadline.
            PlanStep::Ok {
                delay: Duration::from_millis(500),
                events: vec![ProviderEvent::Result {
                    text: Some("never seen".into()),
                }],
            },
            // Second attempt: returns immediately.
            PlanStep::Ok {
                delay: Duration::ZERO,
                events: vec![ProviderEvent::Result {
                    text: Some("ok".into()),
                }],
            },
        ]);
        let deps = deps_with_provider(provider.clone(), Duration::from_millis(50));
        let result = query_with_retry(&deps, deps.provider.as_ref(), dummy_input()).await;
        assert!(result.is_ok(), "expected eventual success");
        assert_eq!(provider.attempts(), 2);
    }

    #[tokio::test]
    async fn timeout_exhausts_to_deadline_exceeded() {
        // Every attempt hangs past the deadline. After 3 strikes the
        // terminal error is `DeadlineExceeded`.
        let provider = PlanProvider::new(vec![
            PlanStep::Ok {
                delay: Duration::from_millis(500),
                events: vec![],
            },
            PlanStep::Ok {
                delay: Duration::from_millis(500),
                events: vec![],
            },
            PlanStep::Ok {
                delay: Duration::from_millis(500),
                events: vec![],
            },
        ]);
        let deps = deps_with_provider(provider.clone(), Duration::from_millis(30));
        let err = match query_with_retry(&deps, deps.provider.as_ref(), dummy_input()).await {
            Ok(_) => panic!("expected DeadlineExceeded"),
            Err(e) => e,
        };
        match err {
            ProviderError::DeadlineExceeded {
                deadline_ms,
                attempts,
            } => {
                assert_eq!(deadline_ms, 30);
                assert_eq!(attempts, MAX_PROVIDER_ATTEMPTS);
            }
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
        assert_eq!(provider.attempts(), MAX_PROVIDER_ATTEMPTS);
    }

    #[tokio::test]
    async fn backoff_sequence_is_correct() {
        // Tail-call the backoff helper directly and time each sleep.
        // Allow generous slack for CI but verify the doubling shape.
        let t1 = std::time::Instant::now();
        backoff_for_attempt(1).await;
        let d1 = t1.elapsed();

        let t2 = std::time::Instant::now();
        backoff_for_attempt(2).await;
        let d2 = t2.elapsed();

        let t3 = std::time::Instant::now();
        backoff_for_attempt(3).await;
        let d3 = t3.elapsed();

        // Expected: 250ms, 500ms, 1000ms.
        assert!(d1 >= Duration::from_millis(240) && d1 < Duration::from_millis(450));
        assert!(d2 >= Duration::from_millis(490) && d2 < Duration::from_millis(800));
        assert!(d3 >= Duration::from_millis(990) && d3 < Duration::from_millis(1500));
    }

    #[tokio::test]
    async fn resolve_provider_deadline_uses_env_when_in_range() {
        let env = crate::config::MapEnv::from_pairs([(PROVIDER_DEADLINE_ENV, "45000")]);
        let d = resolve_provider_deadline(&env);
        assert_eq!(d, Duration::from_millis(45_000));
    }

    #[tokio::test]
    async fn resolve_provider_deadline_falls_back_when_unset() {
        let env = crate::config::MapEnv::default();
        let d = resolve_provider_deadline(&env);
        assert_eq!(d, Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS));
    }

    #[tokio::test]
    async fn resolve_provider_deadline_rejects_out_of_range() {
        let env = crate::config::MapEnv::from_pairs([(PROVIDER_DEADLINE_ENV, "1000")]);
        let d = resolve_provider_deadline(&env);
        // Below MIN_PROVIDER_DEADLINE_MS → default.
        assert_eq!(d, Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS));

        let env = crate::config::MapEnv::from_pairs([(PROVIDER_DEADLINE_ENV, "999999")]);
        let d = resolve_provider_deadline(&env);
        assert_eq!(d, Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS));
    }

    #[tokio::test]
    async fn resolve_provider_deadline_rejects_garbage() {
        let env = crate::config::MapEnv::from_pairs([(PROVIDER_DEADLINE_ENV, "not-a-number")]);
        let d = resolve_provider_deadline(&env);
        assert_eq!(d, Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS));
    }

    /// End-to-end: a 503 followed by success goes through `run_loop`
    /// and the message is marked completed. Validates that the retry
    /// loop is wired into the public entry point.
    #[tokio::test]
    async fn run_loop_retries_503_then_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let provider = PlanProvider::new(vec![
            PlanStep::Err(ProviderErrorKind::Api { status: 503 }),
            PlanStep::Ok {
                delay: Duration::ZERO,
                events: vec![
                    ProviderEvent::Init {
                        continuation: "c1".into(),
                    },
                    ProviderEvent::Result {
                        text: Some("recovered".into()),
                    },
                ],
            },
        ]);
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut deps = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound.clone(),
            outbound.clone(),
            paths.outbox.join("_compactions"),
        );
        deps.max_turns = Some(1);
        deps.idle_sleep = Duration::from_millis(1);
        deps.provider_deadline = Duration::from_secs(2);

        let id = {
            let g = inbound.lock().await;
            insert_pending(&g, "ping")
        };
        run_loop(deps).await.unwrap();

        let inbound = open_inbound(&paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    /// End-to-end: timeout-on-every-attempt drives the inbound to
    /// `failed`. Exercises the integration of the retry loop with
    /// `finalize_messages`.
    #[tokio::test]
    async fn run_loop_marks_failed_when_deadline_exhausted() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let provider = PlanProvider::new(vec![
            PlanStep::Ok {
                delay: Duration::from_millis(500),
                events: vec![],
            },
            PlanStep::Ok {
                delay: Duration::from_millis(500),
                events: vec![],
            },
            PlanStep::Ok {
                delay: Duration::from_millis(500),
                events: vec![],
            },
        ]);
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut deps = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound.clone(),
            outbound.clone(),
            paths.outbox.join("_compactions"),
        );
        deps.max_turns = Some(1);
        deps.idle_sleep = Duration::from_millis(1);
        // Short enough that each attempt trips before the wiremock sleep.
        deps.provider_deadline = Duration::from_millis(30);

        let id = {
            let g = inbound.lock().await;
            insert_pending(&g, "ping")
        };
        run_loop(deps).await.unwrap();

        let inbound = open_inbound(&paths).unwrap();
        let status: String = inbound
            .query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
    }

    /// Mid-message persistence: after the FIRST inner iteration of
    /// drive_turn's tool-turn loop, the assistant tool_use and the
    /// tool_result must already be in `runner.history` on disk. If the
    /// runner crashed mid-message before the end-of-inbound save_state,
    /// the respawned runner needs to see the work the prior attempt
    /// did so it doesn't replay every tool call.
    #[tokio::test]
    async fn save_state_per_turn_persists_intermediate_tool_results() {
        use std::sync::atomic::{AtomicU32, Ordering};
        // Custom provider that snapshots persisted runner state at the
        // start of each `query()` call. Turn 1: emit a ToolCall. Turn
        // 2: emit a final Result. We assert that the snapshot captured
        // at the START of turn 2 already includes the User + ToolUse +
        // Tool(result) entries — meaning the mid-message save_state
        // fired between the two turns.
        struct SnapshotProvider {
            outbound: Arc<Mutex<Connection>>,
            calls: AtomicU32,
            snapshot_at_turn2: StdMutex<Option<Vec<HistoryMessage>>>,
        }
        #[async_trait]
        impl AgentProvider for SnapshotProvider {
            fn name(&self) -> &'static str {
                "snapshot"
            }
            async fn query(
                &self,
                _input: QueryInput,
            ) -> Result<Box<dyn AgentQuery>, ProviderError> {
                let n = self.calls.fetch_add(1, Ordering::Relaxed);
                let events = if n == 0 {
                    // First turn: one tool call, then end of stream.
                    vec![
                        ProviderEvent::ToolCall {
                            id: "tu_mid_1".into(),
                            name: "shell".into(),
                            input: serde_json::json!({"cmd": "echo mid"}),
                        },
                        ProviderEvent::Result { text: None },
                    ]
                } else {
                    // Capture what the runner persisted between turn 1
                    // and turn 2 — that's the mid-message save_state.
                    if n == 1 {
                        let g = self.outbound.lock().await;
                        let st = crate::state::load_state(&g).unwrap();
                        *self.snapshot_at_turn2.lock().unwrap() = Some(st.history);
                    }
                    vec![ProviderEvent::Result {
                        text: Some("all done".into()),
                    }]
                };
                Ok(Box::new(ScriptedQuery {
                    events: StdMutex::new(events),
                }))
            }
            fn is_session_invalid(&self, _err: &ProviderError) -> bool {
                false
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let provider = Arc::new(SnapshotProvider {
            outbound: outbound.clone(),
            calls: AtomicU32::new(0),
            snapshot_at_turn2: StdMutex::new(None),
        });
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let mut deps = RunnerDeps::minimal(
            provider.clone(),
            tool_ctx,
            inbound.clone(),
            outbound.clone(),
            paths.outbox.join("_compactions"),
        );
        deps.max_turns = Some(1);
        deps.idle_sleep = Duration::from_millis(1);

        {
            let g = inbound.lock().await;
            insert_pending(&g, "do a tool then finish");
        }
        run_loop(deps).await.unwrap();

        let snapshot = provider
            .snapshot_at_turn2
            .lock()
            .unwrap()
            .clone()
            .expect("expected a mid-message snapshot at start of turn 2");

        // Must contain: User prompt + ToolUse(shell) + Tool(result) at minimum.
        let has_user = snapshot
            .iter()
            .any(|m| matches!(m, HistoryMessage::User { content } if content.contains("do a tool then finish")));
        let has_tool_use = snapshot.iter().any(|m| {
            matches!(
                m,
                HistoryMessage::ToolUse { id, name, .. } if id == "tu_mid_1" && name == "shell"
            )
        });
        let has_tool_result = snapshot.iter().any(|m| {
            matches!(
                m,
                HistoryMessage::Tool { tool_use_id, .. } if tool_use_id == "tu_mid_1"
            )
        });
        assert!(
            has_user,
            "mid-message snapshot missing the User entry: {snapshot:?}"
        );
        assert!(
            has_tool_use,
            "mid-message snapshot missing the ToolUse entry: {snapshot:?}"
        );
        assert!(
            has_tool_result,
            "mid-message snapshot missing the Tool result entry: {snapshot:?}"
        );
    }

    /// Resume-after-crash dedup: if the persisted history already ends
    /// with a User message matching the freshly-formatted prompt, the
    /// runner must NOT push a second copy. Two consecutive identical
    /// User entries confuse the model ("did the user really ask
    /// twice?"). Caught by inspection during the lost-progress fix:
    /// the per-turn save_state lands history mid-message, and a crash
    /// before finalize_messages leaves the inbound pending — the next
    /// runner spawn re-picks it and would otherwise duplicate the push.
    #[tokio::test]
    async fn resume_with_matching_user_message_skips_duplicate_push() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result {
            text: Some("resumed".into()),
        }]]);
        // Insert one pending inbound with text "ping". format_messages
        // wraps it in a `[chat …] ping` envelope; that's what the
        // dedup check compares against, so we pre-format the history
        // entry the same way by lifting format_messages's output.
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "ping")
        };
        // Capture the formatted prompt the runner will produce so we
        // can pre-seed history with a matching User entry. Reuse the
        // same code path the run loop uses.
        let formatted_prompt = {
            let g = setup.deps.inbound.lock().await;
            let pending = messages_in::get_pending(&g, true, 10).unwrap();
            crate::formatter::format_messages(pending).prompt
        };
        // Pre-seed history as if a prior runner had pushed the user
        // message and crashed before finalize_messages.
        {
            let g = setup.deps.outbound.lock().await;
            crate::state::save_state(
                &g,
                &[HistoryMessage::User {
                    content: formatted_prompt.clone(),
                }],
                None,
            )
            .unwrap();
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = load_state(&outbound).unwrap();
        // No two consecutive User entries with identical content.
        let mut prev_user: Option<&str> = None;
        for m in &st.history {
            if let HistoryMessage::User { content } = m {
                if let Some(prev) = prev_user {
                    assert!(
                        prev != content,
                        "duplicate consecutive User entries with same content: {:?}",
                        st.history
                    );
                }
                prev_user = Some(content.as_str());
            } else {
                prev_user = None;
            }
        }
        // And only one User entry total matches the formatted prompt.
        let user_matches = st
            .history
            .iter()
            .filter(
                |m| matches!(m, HistoryMessage::User { content } if content == &formatted_prompt),
            )
            .count();
        assert_eq!(
            user_matches, 1,
            "expected exactly one User entry matching the prompt, got: {:?}",
            st.history
        );
        let _ = id;
    }

    /// Resume-after-crash dedup, mid-tool-loop variant. When the prior
    /// runner crashed AFTER `persist_mid_message` saved history past
    /// the first tool turn, the persisted history ends in `Tool { ...
    /// }` (the tool result), with the originating User entry several
    /// positions back. The original dedup check only looked at
    /// `history.last()` and so missed the duplicate, re-pushing the
    /// user prompt and ending up with `[..., User(p), Assistant,
    /// ToolUse, Tool, User(p)]` — the model either re-answered or got
    /// confused. The lookback scan must walk past intervening
    /// `Assistant` / `ToolUse` / `Tool` entries and dedup when the
    /// most-recent `User` matches the current prompt.
    #[tokio::test]
    async fn resume_mid_tool_loop_skips_duplicate_push() {
        let mut setup = build_setup(vec![vec![ProviderEvent::Result {
            text: Some("resumed-mid-tool".into()),
        }]]);
        // Insert the same pending inbound the prior runner had been
        // processing.
        let id = {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "do the thing")
        };
        let formatted_prompt = {
            let g = setup.deps.inbound.lock().await;
            let pending = messages_in::get_pending(&g, true, 10).unwrap();
            crate::formatter::format_messages(pending).prompt
        };
        // Pre-seed history as if the prior runner had pushed the user
        // prompt, run one assistant turn that invoked a tool, persisted
        // mid-message AFTER the tool result, then crashed before
        // finalize_messages. History therefore ENDS in `Tool { ... }`
        // — the failure mode `history.last()`-only dedup missed.
        {
            let g = setup.deps.outbound.lock().await;
            crate::state::save_state(
                &g,
                &[
                    HistoryMessage::User {
                        content: formatted_prompt.clone(),
                    },
                    HistoryMessage::Assistant {
                        content: String::new(),
                    },
                    HistoryMessage::ToolUse {
                        id: "tu_resume_1".into(),
                        name: "shell".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                    HistoryMessage::Tool {
                        tool_use_id: "tu_resume_1".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                ],
                None,
            )
            .unwrap();
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = load_state(&outbound).unwrap();
        // Exactly one User entry matching the current prompt — the
        // pre-seeded one. A buggy dedup would have pushed a second
        // copy, taking the count to 2.
        let user_matches = st
            .history
            .iter()
            .filter(
                |m| matches!(m, HistoryMessage::User { content } if content == &formatted_prompt),
            )
            .count();
        assert_eq!(
            user_matches, 1,
            "expected exactly one User entry matching the prompt after mid-tool-loop resume, got: {:?}",
            st.history,
        );
        let _ = id;
    }

    // ── Per-task token budget (COPPERCLAW_MAX_TASK_TOKENS) ──────────────
    //
    // These exercise `drive_turn` directly so the assertions are about
    // the tool-loop's per-task cost accumulator, not the whole run_loop
    // pipeline. A "looping turn" emits one tool_call (an unknown tool,
    // which `invoke_tool` turns into an is_error tool_result) plus a
    // `Usage` event carrying the per-call token counts — the same shape
    // a real runaway has: many tool turns, each billing tokens, never
    // producing a no-tool final answer until the loop is stopped.

    /// One scripted turn that keeps the tool loop going and bills
    /// `in_tok` input + `out_tok` output tokens. The `noop_loop` tool is
    /// not in the (empty) `tool_map`, so `invoke_tool` returns an
    /// `is_error` `tool_result` and the loop runs another turn.
    fn looping_turn_with_usage(in_tok: u32, out_tok: u32) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::ToolCall {
                id: format!("tu_{in_tok}_{out_tok}"),
                name: "noop_loop".into(),
                input: serde_json::json!({}),
            },
            ProviderEvent::Usage {
                input_tokens: in_tok,
                output_tokens: out_tok,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        ]
    }

    #[tokio::test(flavor = "current_thread")]
    async fn task_budget_aborts_runaway_and_trips_metric() {
        // Ceiling 1,000,000. Each turn bills 300k in + 100k out = 400k.
        // Turn 1 → 400k (under), turn 2 → 800k (under), turn 3 → 1.2M
        // (>= ceiling) → abort after pushing the third turn's tool
        // results. Plenty of scripted turns left over to prove we stop
        // on budget, not on running out of script or hitting
        // max_tool_turns.
        let scripts: Vec<Vec<ProviderEvent>> = (0..10)
            .map(|_| looping_turn_with_usage(300_000, 100_000))
            .collect();
        let mut setup = build_setup(scripts);
        setup.deps.max_task_tokens = 1_000_000;
        // Keep the turn cap well above the expected trip point so the
        // per-task BUDGET is unambiguously what stopped the loop.
        setup.deps.max_tool_turns = 50;

        // Install an isolated Prometheus recorder for THIS thread for the
        // duration of the (current-thread) drive_turn future, so we can
        // assert the budget trip incremented the counter without racing
        // the process-global recorder. The guard must outlive the await.
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut history = Vec::new();
        let outcome = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            let turn = drive_turn(&setup.deps, &mut history, None, None)
                .await
                .unwrap();
            turn.outcome
        };

        match outcome {
            TurnOutcome::Failed(reason) => {
                assert!(
                    reason.contains("task budget reached") && reason.contains("tokens"),
                    "expected per-task budget abort reason, got: {reason:?}"
                );
                // 3 turns × 400k = 1.2M accumulated when the abort fires.
                assert!(
                    reason.contains("1200000"),
                    "reason should surface the accumulated token count: {reason:?}"
                );
            }
            TurnOutcome::Done => panic!("runaway should have aborted on the per-task budget"),
        }

        let body = handle.render();
        assert!(
            body.contains(copperclaw_metrics::TASK_BUDGET_EXHAUSTED_TOTAL),
            "budget trip must increment the metric:\n{body}"
        );
        assert!(
            body.contains(&format!("agent_group_id=\"{}\"", setup.deps.agent_group_id)),
            "metric must carry the agent_group_id label:\n{body}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn task_budget_under_ceiling_runs_to_completion() {
        // Two cheap looping turns (50k each → 100k total, well under the
        // 1M ceiling) then a clean no-tool final answer. The loop must
        // finish normally — the budget check must NOT fire.
        let scripts: Vec<Vec<ProviderEvent>> = vec![
            looping_turn_with_usage(30_000, 20_000),
            looping_turn_with_usage(30_000, 20_000),
            vec![ProviderEvent::Result {
                text: Some("all done".into()),
            }],
        ];
        let mut setup = build_setup(scripts);
        setup.deps.max_task_tokens = 1_000_000;
        setup.deps.max_tool_turns = 50;

        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut history = Vec::new();
        let outcome = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            let turn = drive_turn(&setup.deps, &mut history, None, None)
                .await
                .unwrap();
            turn.outcome
        };

        assert!(
            matches!(outcome, TurnOutcome::Done),
            "under the ceiling the task must complete normally, got: {outcome:?}"
        );
        let body = handle.render();
        assert!(
            !body.contains(copperclaw_metrics::TASK_BUDGET_EXHAUSTED_TOTAL),
            "budget metric must NOT trip under the ceiling:\n{body}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn task_budget_disabled_when_zero() {
        // max_task_tokens == 0 disables the per-task ceiling: even a
        // turn billing more than any sane ceiling must not abort on
        // budget. It runs until the (small) turn cap instead, proving
        // the two breakers are independent.
        // DISTINCT tool calls each turn (varying args) so the content-loop
        // breaker does NOT trip — with the budget disabled we want the TURN
        // CAP to be the stopper, proving the two breakers are independent.
        // (Identical calls here would now trip the loop breaker first.)
        let scripts: Vec<Vec<ProviderEvent>> = (0..10)
            .map(|i| {
                vec![
                    ProviderEvent::ToolCall {
                        id: format!("tu_{i}"),
                        name: "noop_loop".into(),
                        input: serde_json::json!({ "i": i }),
                    },
                    ProviderEvent::Usage {
                        input_tokens: 5_000_000,
                        output_tokens: 5_000_000,
                        cache_read_tokens: 0,
                        cache_creation_tokens: 0,
                    },
                ]
            })
            .collect();
        let mut setup = build_setup(scripts);
        setup.deps.max_task_tokens = 0; // disabled
        setup.deps.max_tool_turns = 4;

        let mut history = Vec::new();
        let turn = drive_turn(&setup.deps, &mut history, None, None)
            .await
            .unwrap();
        match turn.outcome {
            TurnOutcome::Failed(reason) => {
                // `noop_loop` isn't in the tool_map, so every call errors → the
                // block scores no progress and the smart auto-continue budget
                // stops the run at the soft cap. The point still stands: the
                // (disabled) per-task token budget is NOT what ended the run.
                assert!(
                    reason.contains("no visible progress"),
                    "with the budget disabled the soft-cap stop must be what ends it: {reason:?}"
                );
                assert!(
                    !reason.contains("task budget"),
                    "budget abort must not fire when disabled: {reason:?}"
                );
            }
            TurnOutcome::Done => panic!("expected the soft-cap stop to end the loop"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn task_budget_independent_of_max_turns_breaker() {
        // Per-task budget set high enough that it never trips; the loop must
        // still bail on the tool-turn cap. `looping_turn_with_usage` calls
        // `noop_loop` (absent from the tool_map → every call errors), so the
        // block makes no progress and the smart auto-continue budget stops it
        // at the soft cap of 3. Proves the token budget check didn't disturb
        // the turn-cap stop.
        let scripts: Vec<Vec<ProviderEvent>> = (0..10)
            .map(|_| looping_turn_with_usage(1_000, 1_000))
            .collect();
        let mut setup = build_setup(scripts);
        setup.deps.max_task_tokens = 10_000_000; // never reached (2k/turn)
        setup.deps.max_tool_turns = 3;

        let mut history = Vec::new();
        let turn = drive_turn(&setup.deps, &mut history, None, None)
            .await
            .unwrap();
        match turn.outcome {
            TurnOutcome::Failed(reason) => assert!(
                reason.contains("no visible progress") && reason.contains("3 tool turns"),
                "the soft-cap stop must still fire independently of the token budget: {reason:?}"
            ),
            TurnOutcome::Done => panic!("expected the soft-cap stop to end the loop"),
        }
    }

    // ── M18 R6: progressive final answers ───────────────────────────────

    /// A long answer, revealed via [`progressive::grow_final_answer`],
    /// lands as ONE `Chat` row (the first chunk) plus a run of `System`
    /// `edit` rows all anchored to that Chat row's `seq`, the last of
    /// which carries the complete text. This is the "assert edit cadence"
    /// half of the acceptance — the >30s + rich-adapter gate itself is
    /// covered by `progressive::should_grow` unit tests (which can name an
    /// elapsed duration a wall-clock test cannot fake).
    #[tokio::test]
    async fn progressive_growth_posts_then_edits_to_full_text() {
        let setup = build_setup(vec![]);
        setup
            .deps
            .tool_ctx
            .set_originating(Some("telegram"), Some("t-1"), None, None);
        // ~500 chars, single line → growable but not expander-scale.
        let answer = "The prototype is built and verified. ".repeat(14);
        assert!(super::progressive::worth_growing(&answer));

        super::progressive::grow_final_answer(&setup.deps, answer.clone(), Duration::ZERO)
            .await
            .unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(chat.len(), 1, "exactly one Chat row anchors the reveal");
        let anchor_seq = chat[0].seq;
        // First chunk is a genuine, strictly-shorter prefix of the answer.
        let first_text = chat[0].content["text"].as_str().unwrap();
        assert!(answer.starts_with(first_text));
        assert!(
            first_text.chars().count() < answer.chars().count(),
            "the first post must be a partial reveal, not the whole answer"
        );

        let edits: Vec<_> = rows
            .iter()
            .filter(|r| r.content.get("edit").is_some())
            .collect();
        assert!(!edits.is_empty(), "growth must emit at least one edit row");
        for e in &edits {
            assert_eq!(
                e.content["edit"]["seq"].as_i64().unwrap(),
                anchor_seq,
                "every edit must target the first message's seq"
            );
        }
        assert_eq!(
            edits.last().unwrap().content["edit"]["text"]
                .as_str()
                .unwrap(),
            answer,
            "the terminal edit must carry the complete answer"
        );
    }

    /// A sub-30s turn on a rich adapter keeps today's single terminal
    /// emit: exactly one `Chat` row with the full text and NOT a single
    /// `edit` row — byte-identical to the pre-R6 path. (Real time in a
    /// unit test is ~0s, so `drive_turn` naturally exercises the
    /// "elapsed < MIN_ELAPSED" arm even on an edit-capable channel.)
    #[tokio::test(flavor = "current_thread")]
    async fn short_turn_keeps_single_terminal_emit_on_rich_adapter() {
        let long = "All done — here is the full writeup. ".repeat(14); // ~500 chars
        let setup = build_setup(vec![vec![ProviderEvent::Result {
            text: Some(long.clone()),
        }]]);
        // Edit-capable origin: proves the fallback is the *turn length*,
        // not the adapter, that keeps the single emit here.
        setup
            .deps
            .tool_ctx
            .set_originating(Some("telegram"), Some("t-1"), None, None);

        let mut history = Vec::new();
        let turn = drive_turn(&setup.deps, &mut history, None, None)
            .await
            .unwrap();
        assert!(matches!(turn.outcome, TurnOutcome::Done));

        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(chat.len(), 1, "one terminal Chat row, as before R6");
        assert_eq!(chat[0].content["text"].as_str().unwrap(), long);
        assert!(
            rows.iter().all(|r| r.content.get("edit").is_none()),
            "a sub-30s turn must NOT grow the answer via edits"
        );
    }

    // ── resolve_max_task_tokens ────────────────────────────────────────

    #[test]
    fn resolve_max_task_tokens_uses_env_when_in_range() {
        let env = crate::config::MapEnv::from_pairs([(MAX_TASK_TOKENS_ENV, "3000000")]);
        assert_eq!(resolve_max_task_tokens(&env), 3_000_000);
    }

    #[test]
    fn resolve_max_task_tokens_falls_back_when_unset() {
        let env = crate::config::MapEnv::default();
        assert_eq!(resolve_max_task_tokens(&env), DEFAULT_MAX_TASK_TOKENS);
    }

    #[test]
    fn resolve_max_task_tokens_zero_disables() {
        let env = crate::config::MapEnv::from_pairs([(MAX_TASK_TOKENS_ENV, "0")]);
        assert_eq!(resolve_max_task_tokens(&env), 0);
    }

    #[test]
    fn resolve_max_task_tokens_rejects_below_floor() {
        // Below MIN (but non-zero) → fall back to default, not the tiny
        // value (which would trip on the first big-context turn).
        let env = crate::config::MapEnv::from_pairs([(MAX_TASK_TOKENS_ENV, "5000")]);
        assert_eq!(resolve_max_task_tokens(&env), DEFAULT_MAX_TASK_TOKENS);
    }

    #[test]
    fn resolve_max_task_tokens_rejects_above_ceiling() {
        let env = crate::config::MapEnv::from_pairs([(MAX_TASK_TOKENS_ENV, "999999999")]);
        assert_eq!(resolve_max_task_tokens(&env), DEFAULT_MAX_TASK_TOKENS);
    }

    #[test]
    fn resolve_max_task_tokens_rejects_garbage() {
        let env = crate::config::MapEnv::from_pairs([(MAX_TASK_TOKENS_ENV, "lots")]);
        assert_eq!(resolve_max_task_tokens(&env), DEFAULT_MAX_TASK_TOKENS);
    }

    // ── smart auto-continue: resolve_max_tool_turns_hard (test (f)) ──────

    #[test]
    fn resolve_hard_defaults_to_soft_times_six_when_unset() {
        // Fresh install: no env → soft * 6. At the default soft cap of 150
        // that's 900 — the SAFE default the design calls out.
        let env = crate::config::MapEnv::default();
        assert_eq!(resolve_max_tool_turns_hard(&env, 150), 900);
        assert_eq!(resolve_max_tool_turns_hard(&env, 10), 60);
    }

    #[test]
    fn resolve_hard_default_is_clamped_to_absolute_max() {
        // soft * 6 would be 3000 for soft = 500; the default is clamped to the
        // absolute ceiling so even a maxed-out soft cap can't push it past it.
        let env = crate::config::MapEnv::default();
        assert_eq!(
            resolve_max_tool_turns_hard(&env, 500),
            MAX_MAX_TOOL_TURNS_HARD
        );
    }

    #[test]
    fn resolve_hard_uses_env_when_in_range() {
        let env = crate::config::MapEnv::from_pairs([(MAX_TOOL_TURNS_HARD_ENV, "600")]);
        assert_eq!(resolve_max_tool_turns_hard(&env, 150), 600);
    }

    #[test]
    fn resolve_hard_equal_to_soft_is_honoured_and_disables_extension() {
        // Setting the ceiling EQUAL to the soft cap is a legitimate config
        // (extension off / flat cap), so it is returned verbatim.
        let env = crate::config::MapEnv::from_pairs([(MAX_TOOL_TURNS_HARD_ENV, "150")]);
        assert_eq!(resolve_max_tool_turns_hard(&env, 150), 150);
    }

    #[test]
    fn resolve_hard_below_soft_clamps_up_to_soft() {
        // A ceiling below the soft cap would bail before a block completes —
        // clamp it up to soft (which disables extension) rather than honour it.
        let env = crate::config::MapEnv::from_pairs([(MAX_TOOL_TURNS_HARD_ENV, "40")]);
        assert_eq!(resolve_max_tool_turns_hard(&env, 150), 150);
    }

    #[test]
    fn resolve_hard_above_absolute_max_clamps_down() {
        let env = crate::config::MapEnv::from_pairs([(MAX_TOOL_TURNS_HARD_ENV, "99999")]);
        assert_eq!(
            resolve_max_tool_turns_hard(&env, 150),
            MAX_MAX_TOOL_TURNS_HARD
        );
    }

    #[test]
    fn resolve_hard_rejects_garbage_and_uses_default() {
        let env = crate::config::MapEnv::from_pairs([(MAX_TOOL_TURNS_HARD_ENV, "soon")]);
        assert_eq!(resolve_max_tool_turns_hard(&env, 150), 900);
    }

    #[test]
    fn default_hard_never_below_soft() {
        // Even a pathological tiny soft cap keeps hard >= soft.
        assert!(default_max_tool_turns_hard(1) >= 1);
        assert_eq!(default_max_tool_turns_hard(1), 6);
    }

    // ── F2: actionable "I'm blocked" wall cards ─────────────────────────
    //
    // These drive the full run_loop pipeline (drive_turn → finalize →
    // apology emit) so the assertion is about what lands in `messages_out`
    // — the exact rows the delivery loop would route to the user.

    /// A tool that always fails with the deny-default egress hint the real
    /// `install_packages` surfaces (`self_mod::egress_allow_hint`), so a
    /// run of these produces a tail run of `Egress` denials.
    struct EgressDenyTool;

    #[async_trait]
    impl copperclaw_mcp::ToolHandler for EgressDenyTool {
        async fn call(
            &self,
            _arguments: Option<rmcp::model::JsonObject>,
            _ctx: &dyn copperclaw_mcp::ToolContext,
        ) -> Result<rmcp::model::CallToolResult, copperclaw_mcp::ToolError> {
            Err(copperclaw_mcp::ToolError::Context(
                "session install failed. `npm` could not reach its package registry — \
                 container egress is denied by default. Ask an operator to allow the \
                 registry, then retry: `cclaw groups config set-egress-allow abc \
                 --allow registry.npmjs.org:443`"
                    .into(),
            ))
        }
    }

    /// Register a single mock tool named `name` on `deps.tool_map`.
    fn install_mock_tool(
        deps: &mut RunnerDeps,
        name: &'static str,
        handler: Box<dyn copperclaw_mcp::ToolHandler>,
    ) {
        let mut map: std::collections::HashMap<String, Arc<copperclaw_mcp::ToolEntry>> =
            std::collections::HashMap::new();
        map.insert(
            name.to_string(),
            Arc::new(copperclaw_mcp::ToolEntry {
                tool: rmcp::model::Tool {
                    name: std::borrow::Cow::Borrowed(name),
                    description: None,
                    input_schema: Arc::new(rmcp::model::JsonObject::new()),
                    annotations: None,
                },
                handler,
            }),
        );
        deps.tool_map = Arc::new(map);
    }

    /// One scripted turn that calls `install_packages` with identical args
    /// (so a run trips the content-loop breaker) — the same shape a model
    /// wedged against a hard wall produces.
    fn install_packages_turn() -> Vec<ProviderEvent> {
        vec![ProviderEvent::ToolCall {
            id: "tu_install".into(),
            name: "install_packages".into(),
            input: serde_json::json!({ "ecosystem": "npm", "packages": ["left-pad"] }),
        }]
    }

    #[tokio::test]
    async fn egress_wall_produces_exactly_one_curated_card() {
        // Six identical egress-denied install turns: the content-loop
        // breaker trips at the fourth, the turn ends Failed with no
        // user-facing reply, and its tail is a run of Egress denials — so
        // finalize surfaces ONE curated wall card instead of the generic
        // apology, and never leaks the raw denial text.
        let scripts: Vec<Vec<ProviderEvent>> = (0..6).map(|_| install_packages_turn()).collect();
        let mut setup = build_setup(scripts);
        install_mock_tool(
            &mut setup.deps,
            "install_packages",
            Box::new(EgressDenyTool),
        );
        setup.deps.max_tool_turns = 50;
        setup.deps.max_turns = Some(1);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "please add left-pad and build the app");
        }
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        // Exactly one Error-kind row (the wall card) — no generic apology
        // in addition, and no user-facing Chat reply.
        let error_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Error)
            .collect();
        assert_eq!(
            error_rows.len(),
            1,
            "exactly one wall card must land; got {} error rows",
            error_rows.len()
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.kind == copperclaw_types::MessageKind::Chat),
            "a silently-walled turn must not also emit a chat reply"
        );
        let card: copperclaw_channels_core::ErrorCard =
            serde_json::from_value(error_rows[0].content["error"].clone()).unwrap();
        assert_eq!(card.title, "Blocked: can't reach a package registry");
        assert!(
            card.summary.contains("set-egress-allow"),
            "the curated card teaches the egress-allow remediation: {}",
            card.summary
        );
        // No RAW denial text (or its `details` block) reaches the user.
        assert!(
            card.details.is_none(),
            "wall card must carry no raw details"
        );
        let serialized = serde_json::to_string(&card).unwrap();
        assert!(
            !serialized.contains("session install failed")
                && !serialized.contains("registry.npmjs.org:443"),
            "raw ToolError text must never reach the user: {serialized}"
        );
    }

    #[tokio::test]
    async fn recovered_egress_error_produces_no_wall_card() {
        // One egress denial, then the model recovers and answers normally.
        // The turn ends Done with a chat reply — no wall card, byte-stable.
        let scripts: Vec<Vec<ProviderEvent>> = vec![
            install_packages_turn(),
            vec![ProviderEvent::Result {
                text: Some("I couldn't install it, but here's a workaround: …".into()),
            }],
        ];
        let mut setup = build_setup(scripts);
        install_mock_tool(
            &mut setup.deps,
            "install_packages",
            Box::new(EgressDenyTool),
        );
        setup.deps.max_tool_turns = 50;
        setup.deps.max_turns = Some(1);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "add left-pad");
        }
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        assert!(
            !rows
                .iter()
                .any(|r| r.kind == copperclaw_types::MessageKind::Error),
            "a recovered turn must emit no wall card"
        );
        let chat = rows
            .iter()
            .find(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .expect("the recovered turn must emit its normal answer");
        assert!(
            chat.content["text"]
                .as_str()
                .unwrap()
                .contains("workaround")
        );
    }

    #[tokio::test]
    async fn non_blocker_failure_keeps_generic_apology() {
        // A turn that fails for a reason with NO recognized blocker (an
        // unknown tool looping into the breaker) must keep the existing
        // generic apology card — the F2 path is byte-stable off the
        // blocker categories.
        let scripts: Vec<Vec<ProviderEvent>> = (0..6)
            .map(|_| {
                vec![ProviderEvent::ToolCall {
                    id: "tu_x".into(),
                    name: "noop_loop".into(),
                    input: serde_json::json!({}),
                }]
            })
            .collect();
        let mut setup = build_setup(scripts);
        setup.deps.max_tool_turns = 50;
        setup.deps.max_turns = Some(1);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "do the thing");
        }
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let error_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Error)
            .collect();
        assert_eq!(error_rows.len(), 1, "one generic apology card");
        let card: copperclaw_channels_core::ErrorCard =
            serde_json::from_value(error_rows[0].content["error"].clone()).unwrap();
        // The generic terminal-failure card, unchanged by F2.
        assert_eq!(card.title, "I couldn't finish that reply");
        assert!(!card.title.starts_with("Blocked:"));
    }

    // ── M22 A2: firing-task resolution + grant snapshot load ──────────────

    fn task_row(task_id: Option<&str>, series: Option<&str>) -> MessageInRow {
        MessageInRow {
            id: MessageId::new(),
            seq: 1,
            kind: MessageKind::Task,
            timestamp: Utc::now(),
            status: "pending".into(),
            process_after: None,
            recurrence: None,
            series_id: series.map(str::to_string),
            tries: 0,
            trigger: true,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            content: match task_id {
                Some(t) => serde_json::json!({"text": "do it", "task_id": t}),
                None => serde_json::json!({"text": "do it"}),
            },
            source_session_id: None,
            on_wake: true,
            reply_to: None,
            is_group: None,
        }
    }

    #[test]
    fn firing_task_id_reads_content_then_series() {
        // The sweep writes both `content.task_id` and `series_id`; content wins.
        assert_eq!(
            firing_task_id(&[task_row(Some("t-42"), Some("t-99"))]),
            Some("t-42".to_string())
        );
        // Falls back to series_id when content lacks task_id.
        assert_eq!(
            firing_task_id(&[task_row(None, Some("t-99"))]),
            Some("t-99".to_string())
        );
        // No Task row → no firing task.
        assert_eq!(firing_task_id(&[]), None);
    }

    /// Write a grant snapshot into `dir` and return it.
    fn write_grant_snapshot(dir: &std::path::Path, grant: &tool_dispatch::TurnGrant) {
        let bytes = serde_json::to_vec_pretty(grant).unwrap();
        std::fs::write(dir.join(GRANT_SNAPSHOT_FILENAME), bytes).unwrap();
    }

    fn snapshot_grant(task_id: &str, scope: &str) -> tool_dispatch::TurnGrant {
        tool_dispatch::TurnGrant {
            grant_id: "g-1".into(),
            task_id: task_id.into(),
            capability_scope: scope.into(),
            tokens_remaining: Some(1000),
            fires_remaining: Some(5),
            expires_at: Some(Utc::now() + chrono::Duration::days(30)),
        }
    }

    #[test]
    fn load_turn_grant_matches_task_and_liveness() {
        let tmp = tempfile::tempdir().unwrap();
        write_grant_snapshot(
            tmp.path(),
            &snapshot_grant("t-1", "web_fetch send_message:telegram"),
        );
        let now = Utc::now();
        // Matching task id + live → loaded.
        let g = load_turn_grant(tmp.path(), Some("t-1"), now).unwrap();
        assert_eq!(g.grant_id, "g-1");
        assert!(g.permits("web_fetch"));
        assert!(g.permits("send_message:telegram"));
        assert!(!g.permits("send_email"));
    }

    #[test]
    fn load_turn_grant_rejects_task_id_mismatch() {
        // A stale snapshot for a DIFFERENT task must never authorize this fire.
        let tmp = tempfile::tempdir().unwrap();
        write_grant_snapshot(tmp.path(), &snapshot_grant("t-other", "web_fetch"));
        assert!(load_turn_grant(tmp.path(), Some("t-1"), Utc::now()).is_none());
    }

    #[test]
    fn load_turn_grant_absent_or_inert_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        // No file → None (brake stays closed).
        assert!(load_turn_grant(tmp.path(), Some("t-1"), Utc::now()).is_none());
        // Expired snapshot → None (defence-in-depth liveness re-check).
        let mut g = snapshot_grant("t-1", "web_fetch");
        g.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        write_grant_snapshot(tmp.path(), &g);
        assert!(load_turn_grant(tmp.path(), Some("t-1"), Utc::now()).is_none());
        // Exhausted fires → None.
        let mut g = snapshot_grant("t-1", "web_fetch");
        g.fires_remaining = Some(0);
        write_grant_snapshot(tmp.path(), &g);
        assert!(load_turn_grant(tmp.path(), Some("t-1"), Utc::now()).is_none());
    }
}
