//! Runner poll loop. Module name is `run` because `loop` is a reserved
//! keyword in Rust 2024.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use ironclaw_db::tables::{
    container_state, messages_in, processing_ack,
};
use ironclaw_mcp::{ToolContext, ToolEntry};
use ironclaw_providers::{AgentProvider, AgentQuery, HistoryMessage, ProviderError, QueryInput, ToolDef};
#[cfg_attr(not(test), allow(unused_imports))]
use ironclaw_types::MessageId;
use ironclaw_types::{Effort, MessageInRow, ProviderEvent};
use std::collections::HashMap;
use rusqlite::Connection;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};

use crate::compaction::{compact, estimate_tokens, CompactionCfg};
use crate::disallowed::is_disallowed;
use crate::formatter::format_messages;
use crate::state::{load_state, save_state};

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
/// Overridable per-process via `IRONCLAW_RUNNER_PROVIDER_DEADLINE_MS`.
pub const DEFAULT_PROVIDER_DEADLINE_MS: u64 = 60_000;

/// Environment variable read at runner startup to override
/// [`DEFAULT_PROVIDER_DEADLINE_MS`]. Values outside the
/// [`MIN_PROVIDER_DEADLINE_MS`]..=[`MAX_PROVIDER_DEADLINE_MS`] range are
/// rejected with a warning (the default is used instead) so an operator
/// can't accidentally disable the deadline by setting it to 0.
pub const PROVIDER_DEADLINE_ENV: &str = "IRONCLAW_RUNNER_PROVIDER_DEADLINE_MS";

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

/// Maximum number of `provider.query()` attempts (including the first)
/// before the runner gives up and marks the inbound failed. Hard-coded
/// rather than configurable: 3 strikes is the standard SRE default for
/// idempotent retries and we don't want to give operators a footgun for
/// "infinite retry on a flapping API".
const MAX_PROVIDER_ATTEMPTS: u32 = 3;

/// Initial backoff between retries; doubles each attempt:
/// 250ms → 500ms → 1s.
const INITIAL_PROVIDER_BACKOFF: Duration = Duration::from_millis(250);

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
pub const TOOL_DEADLINE_ENV: &str = "IRONCLAW_RUNNER_TOOL_DEADLINE_SECS";

/// Floor for the per-tool deadline. Below 10s a routine `npm install`
/// of a trivial package list trips the timeout, defeating the safety
/// net.
pub const MIN_TOOL_DEADLINE_SECS: u64 = 10;

/// Ceiling for the per-tool deadline. An hour is generous for any
/// realistic single tool invocation (`cargo build` of a large crate,
/// big `apt-get install`). Beyond that, presume the tool is wedged.
pub const MAX_TOOL_DEADLINE_SECS: u64 = 3_600;

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
    /// How many turns to run before exiting cleanly. `None` means loop forever.
    pub max_turns: Option<usize>,
    /// Per-iteration sleep when the inbox is empty.
    pub idle_sleep: Duration,
    /// Heartbeat path the runner touches once per iteration (None to skip).
    pub heartbeat_path: Option<PathBuf>,
    /// Session id this runner is bound to. Stamped on every
    /// `usage_report` system row so the host can join to
    /// `agent_turns.session_id`.
    pub session_id: ironclaw_types::SessionId,
    /// Agent group id. Same use as `session_id`.
    pub agent_group_id: ironclaw_types::AgentGroupId,
    /// Runner-local turn counter. Bumped by the `usage_report` emitter
    /// after each turn so `agent_turns.seq` is monotonically
    /// increasing per session.
    pub turn_seq: Arc<std::sync::atomic::AtomicI64>,
    /// Per-tool dispatch map. Built once at runner startup from
    /// `ironclaw_mcp::build_tool_map()`. Keyed by the tool name the
    /// model emits in `tool_use` blocks; each entry knows how to
    /// validate the input and invoke the handler against the
    /// runner's `ToolContext`.
    pub tool_map: Arc<HashMap<String, Arc<ToolEntry>>>,
    /// Hard cap on consecutive tool-use turns per inbound. Stops a
    /// confused model from looping forever. Default 20.
    pub max_tool_turns: usize,
    /// Per-LLM-call deadline. Wraps each `provider.query()` attempt in
    /// `tokio::time::timeout`. On expiry the attempt is treated as a
    /// retryable failure and reissued (with backoff) up to
    /// [`MAX_PROVIDER_ATTEMPTS`] times before terminal failure.
    ///
    /// Default in [`RunnerDeps::minimal`] is
    /// [`DEFAULT_PROVIDER_DEADLINE_MS`]. The runner binary picks the
    /// value up from `IRONCLAW_RUNNER_PROVIDER_DEADLINE_MS`; tests can
    /// shorten it to make failure-mode fixtures finish quickly.
    pub provider_deadline: Duration,
    /// Per-tool-call hard ceiling. Wraps each [`invoke_tool`] dispatch
    /// in `tokio::time::timeout`; on expiry the tool returns a
    /// `tool_result` with `is_error: true` describing the timeout.
    /// Belt-and-braces with [`HeartbeatTicker`]: the ticker prevents
    /// the host from killing the container during a slow legitimate
    /// tool, this deadline keeps a *wedged* tool from running forever.
    /// Default [`DEFAULT_TOOL_DEADLINE_SECS`].
    pub tool_deadline_secs: u64,
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
            session_id: ironclaw_types::SessionId(uuid::Uuid::nil()),
            agent_group_id: ironclaw_types::AgentGroupId(uuid::Uuid::nil()),
            turn_seq: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            tool_map: Arc::new(HashMap::new()),
            max_tool_turns: 20,
            compaction: CompactionCfg {
                model_input_window: 200_000,
                safety_margin_tokens: 8_000,
                output_reserve_tokens: 4_096,
                summary_model: "claude-sonnet-4-6".into(),
                summary_effort: Effort::Low,
                summary_max_tokens: 1024,
                archive_dir,
            },
            max_turns: None,
            idle_sleep: Duration::from_millis(POLL_INTERVAL_MS),
            heartbeat_path: None,
            provider_deadline: Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS),
            tool_deadline_secs: DEFAULT_TOOL_DEADLINE_SECS,
        }
    }
}

/// Drive the poll loop until `max_turns` turns have been executed (or
/// forever, if `max_turns` is `None`). The function is `async` and may be
/// awaited from any tokio runtime.
pub async fn run_loop(deps: RunnerDeps) -> Result<()> {
    // Bring the persisted message history into memory once at startup.
    let mut state = {
        let g = deps.outbound.lock().await;
        load_state(&g).context("load runner state")?
    };
    let mut turns_run: usize = 0;
    let mut first_poll = true;

    loop {
        if let Some(limit) = deps.max_turns {
            if turns_run >= limit {
                return Ok(());
            }
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

        ack_picked_up(&deps, &pending).await?;
        let formatted = format_messages(pending);

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
            .find(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .or_else(|| formatted.rows.first());
        if let Some(r) = origin_row {
            let in_reply_to = r.id.as_uuid().to_string();
            deps.tool_ctx.set_originating(
                r.channel_type.as_ref().map(ironclaw_types::ChannelType::as_str),
                r.platform_id.as_deref(),
                r.thread_id.as_deref(),
                Some(in_reply_to.as_str()),
            );
        } else {
            deps.tool_ctx.set_originating(None, None, None, None);
        }

        state
            .history
            .push(HistoryMessage::User { content: formatted.prompt });

        if deps.compaction.should_compact(estimate_tokens(&state.history)) {
            state.history = compact(state.history, deps.provider.as_ref(), &deps.compaction)
                .await
                .context("compaction failed")?;
        }

        let turn = drive_turn(&deps, &mut state.history, state.continuation.as_deref()).await?;
        state.continuation = turn.continuation.or(state.continuation);

        finalize_messages(&deps, &formatted.rows, turn.outcome).await?;
        // Clear the originating routing so the next iteration's
        // emit-outbound calls don't accidentally inherit a stale
        // channel (e.g. a system-kind row written by save_state).
        deps.tool_ctx.clear_originating();

        {
            let g = deps.outbound.lock().await;
            save_state(&g, &state.history, state.continuation.as_deref())
                .context("save runner state")?;
        }
        turns_run += 1;
        // Active path: poll faster when traffic is flowing.
        sleep(Duration::from_millis(ACTIVE_POLL_INTERVAL_MS)).await;
    }
}

#[derive(Debug, Clone)]
struct TurnResult {
    continuation: Option<String>,
    outcome: TurnOutcome,
}

#[derive(Debug, Clone)]
enum TurnOutcome {
    /// Model produced a final response.
    Done,
    /// Provider returned an error event.
    Failed,
}

/// One pending tool call extracted from a streamed turn.
#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
    /// `Some` when the provider couldn't parse the model's `tool_use`
    /// input JSON. The runner skips real tool invocation for this
    /// call and instead feeds the parse error back to the model as a
    /// `tool_result { is_error: true }` so it can self-correct on the
    /// next turn. The `input` field is `Value::Null` in this case.
    parse_error: Option<String>,
}

/// What one LLM round-trip produced.
#[derive(Debug, Clone, Default)]
struct LlmTurnOutput {
    continuation: Option<String>,
    /// Final assistant text accumulated during the stream. May be
    /// empty when the model produced only `tool_use` blocks.
    text: String,
    /// Tool calls the model requested. When non-empty the caller
    /// must execute them and run another LLM turn before treating
    /// the message as answered.
    tool_calls: Vec<PendingToolCall>,
    /// True if the provider emitted a terminal Error event.
    failed: bool,
    /// When `failed` is true and the provider's `Error` event carried
    /// `retryable: true`, this is set so `run_llm_turn` can re-issue
    /// the whole query rather than terminating the inbound. Surfaces the
    /// transport/SSE-decode classification the provider already does.
    retryable_failure: bool,
}

/// Hard cap on consecutive turns where the model emitted at least one
/// `tool_use` block whose input JSON failed to parse. The runner feeds
/// the parse error back as a `tool_result { is_error: true }` so the
/// model can self-correct, but if it can't fix it after this many
/// attempts we fall through to the existing terminal-failure path so
/// the user at least sees the apology row. See
/// `malformed_tool_use_gives_up_after_three_attempts` for the
/// regression pin.
const MAX_TOOL_PARSE_ERROR_ATTEMPTS: u32 = 3;

/// Drive one inbound through to a final assistant response. Loops
/// LLM-turn → execute-tools → LLM-turn until the model produces a
/// turn with no `tool_use` blocks (or we hit `max_tool_turns`).
async fn drive_turn(
    deps: &RunnerDeps,
    history: &mut Vec<HistoryMessage>,
    previous_continuation: Option<&str>,
) -> Result<TurnResult> {
    let mut continuation: Option<String> = previous_continuation.map(str::to_string);
    // Counts consecutive turns whose output included any
    // parse-error-tagged tool call. Reset when a turn produces a
    // parse-error-free output. Bounded by
    // `MAX_TOOL_PARSE_ERROR_ATTEMPTS` so a stuck model can't loop us
    // forever.
    let mut consecutive_parse_error_turns: u32 = 0;

    for tool_turn in 0..deps.max_tool_turns.max(1) {
        let output = run_llm_turn(deps, history, continuation.as_deref()).await?;
        continuation = output.continuation.or(continuation);

        if output.failed {
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Failed,
            });
        }

        // Append the model's assistant turn (text + tool_use blocks)
        // to history before deciding what to do next. Anthropic's
        // serializer coalesces consecutive same-role entries, so
        // Assistant{text} + ToolUse{...} round-trip as one
        // multi-block assistant message.
        if !output.text.is_empty() {
            history.push(HistoryMessage::Assistant {
                content: output.text.clone(),
            });
        }
        for call in &output.tool_calls {
            history.push(HistoryMessage::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }

        // No tools requested → this is the final answer for the
        // inbound. Surface the text to the channel and return.
        if output.tool_calls.is_empty() {
            if !output.text.is_empty() {
                let spec = ironclaw_mcp::SendMessageSpec {
                    to: None,
                    text: output.text,
                };
                let _ack = deps
                    .tool_ctx
                    .emit_outbound(ironclaw_mcp::OutboundToolEffect::SendMessage(spec))
                    .await
                    .map_err(|e| anyhow::anyhow!("send_message failed: {e}"))?;
            }
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Done,
            });
        }

        // Track whether THIS turn included any synthetic
        // parse-error tool calls. We bump the counter now but defer
        // the cap check until after pushing tool_results into history
        // so the audit trail captures all attempts (the model never
        // sees the third turn's results, but the persisted history
        // shows three full parse-error cycles for ops review).
        let turn_had_parse_error = output
            .tool_calls
            .iter()
            .any(|c| c.parse_error.is_some());
        if turn_had_parse_error {
            consecutive_parse_error_turns += 1;
        } else {
            consecutive_parse_error_turns = 0;
        }

        // Tools requested → execute each, push the result as a
        // user-role tool_result history entry, and loop into
        // another LLM turn.
        tracing::info!(
            tool_turn,
            n = output.tool_calls.len(),
            "executing tool calls"
        );
        for call in &output.tool_calls {
            let (content, is_error) = if let Some(parse_err) = call.parse_error.as_deref() {
                // Synthetic call from `ProviderEvent::ToolInputParseError`:
                // we never actually invoke the tool — instead we hand the
                // model a tool_result describing what went wrong so it
                // can re-emit the call with valid JSON next turn.
                (
                    format!(
                        "Your tool_use input JSON could not be parsed: {parse_err}. Please re-issue this exact tool call with valid JSON.",
                    ),
                    true,
                )
            } else {
                invoke_tool(deps, call).await
            };
            history.push(HistoryMessage::Tool {
                tool_use_id: call.id.clone(),
                content,
                is_error,
            });
        }

        // After pushing the tool_results, enforce the parse-error cap.
        // Three consecutive turns of malformed tool_use JSON means
        // the model is stuck — fall through to the existing terminal
        // failure path so the user sees the apology row.
        if consecutive_parse_error_turns >= MAX_TOOL_PARSE_ERROR_ATTEMPTS {
            tracing::error!(
                attempts = consecutive_parse_error_turns,
                "{MAX_TOOL_PARSE_ERROR_ATTEMPTS} consecutive tool_use parse failures; bailing",
            );
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Failed,
            });
        }
    }

    // Exhausted the cap. Push a synthetic system message so the
    // model can see what happened on the next inbound, return
    // Failed so finalize_messages marks the inbound that way too.
    tracing::warn!(
        max = deps.max_tool_turns,
        "tool-use cycle exceeded max turns; bailing"
    );
    Ok(TurnResult {
        continuation,
        outcome: TurnOutcome::Failed,
    })
}

/// Make one LLM call. Pumps the streamed provider events into
/// per-turn buffers and returns when the stream ends.
async fn run_llm_turn(
    deps: &RunnerDeps,
    history: &[HistoryMessage],
    previous_continuation: Option<&str>,
) -> Result<LlmTurnOutput> {
    let input = QueryInput {
        system: deps.system.clone(),
        model: deps.model.clone(),
        effort: deps.effort,
        previous_continuation: previous_continuation.map(str::to_string),
        history: history.to_vec(),
        tools: deps.tools.clone(),
        max_tokens: deps.max_tokens,
        temperature: deps.temperature,
        assistant_name: deps.assistant_name.clone(),
        display_name: None,
    };
    let turn_started_at = chrono::Utc::now();

    // Two layers of retry surround the stream:
    //   1. `query_with_retry` retries the initial HTTP call when it
    //      fails before the stream starts (covers connect / TLS / 5xx /
    //      timeout on the request).
    //   2. The loop below retries the WHOLE (query + pump) pair when the
    //      stream itself errors mid-way and the provider tagged the
    //      `ProviderEvent::Error` as retryable. This catches transient
    //      SSE-decode / dropped-connection cases that the initial query
    //      can't see because the HTTP response already returned 200.
    // Both layers cap at the same `MAX_PROVIDER_ATTEMPTS` budget so the
    // worst-case wall time stays bounded.
    let mut stream_attempts: u32 = 0;
    let (out, input_tokens, output_tokens) = loop {
        stream_attempts += 1;
        let mut query = match query_with_retry(deps, input.clone()).await {
            Ok(q) => q,
            Err(err) => {
                // All retries exhausted (or the failure was non-retryable).
                // Mark this turn as a terminal failure so the caller flips
                // the inbound to `failed` and emits a usage_report with
                // status=error. Do NOT bubble — the runner must stay up.
                tracing::error!(
                    error = %err,
                    provider = deps.provider.name(),
                    "provider query failed terminally; marking turn as failed"
                );
                let out = LlmTurnOutput {
                    failed: true,
                    ..LlmTurnOutput::default()
                };
                emit_usage_report(deps, 0, 0, turn_started_at, &TurnOutcome::Failed).await;
                return Ok(out);
            }
        };

        let pumped = pump_events(deps, query.as_mut()).await?;
        query.abort().await;

        // Retry only if the failure was tagged retryable AND we have
        // budget left. Use the same backoff schedule as query_with_retry
        // for consistency.
        if pumped.0.failed && pumped.0.retryable_failure && stream_attempts < MAX_PROVIDER_ATTEMPTS {
            tracing::warn!(
                attempt = stream_attempts,
                max = MAX_PROVIDER_ATTEMPTS,
                provider = deps.provider.name(),
                "retryable stream failure; backing off and retrying"
            );
            ironclaw_metrics::inc_provider_retry(deps.provider.name());
            backoff_for_attempt(stream_attempts).await;
            continue;
        }
        break pumped;
    };
    let outcome = if out.failed {
        TurnOutcome::Failed
    } else {
        TurnOutcome::Done
    };

    // Emit Prometheus metrics for this LLM call.
    let elapsed_ms = (chrono::Utc::now() - turn_started_at)
        .num_milliseconds()
        .max(0);
    // i64 -> f64 loses precision for large values (> 2^53 ms = ~285 years);
    // acceptable here since we're measuring LLM call durations in seconds.
    #[allow(clippy::cast_precision_loss)]
    let elapsed_secs = elapsed_ms as f64 / 1000.0;
    ironclaw_metrics::observe_llm_call_seconds(elapsed_secs.max(0.0));
    if input_tokens > 0 {
        ironclaw_metrics::observe_llm_tokens_input(input_tokens);
    }
    if output_tokens > 0 {
        ironclaw_metrics::observe_llm_tokens_output(output_tokens);
    }

    emit_usage_report(deps, input_tokens, output_tokens, turn_started_at, &outcome).await;
    Ok(out)
}

/// Pump events off a live [`AgentQuery`] until the stream ends or
/// emits a terminal event ([`ProviderEvent::Result`] /
/// [`ProviderEvent::Error`]). Returns the accumulated turn output plus
/// the latest seen `(input_tokens, output_tokens)` counts.
async fn pump_events(
    deps: &RunnerDeps,
    query: &mut dyn AgentQuery,
) -> Result<(LlmTurnOutput, u32, u32)> {
    let mut out = LlmTurnOutput::default();
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;

    while let Some(event) = query.next_event().await {
        match event {
            ProviderEvent::Init { continuation: c } => {
                out.continuation = Some(c);
            }
            ProviderEvent::Usage {
                input_tokens: it,
                output_tokens: ot,
            } => {
                if it > 0 {
                    input_tokens = it;
                }
                if ot > 0 {
                    output_tokens = ot;
                }
            }
            ProviderEvent::Result { text } => {
                if let Some(t) = text {
                    out.text = t;
                }
                break;
            }
            ProviderEvent::Error { message, retryable } => {
                tracing::warn!(
                    error = %message,
                    retryable,
                    "provider returned an error event"
                );
                out.failed = true;
                out.retryable_failure = retryable;
                break;
            }
            ProviderEvent::ToolStart {
                name,
                declared_timeout_ms,
            } => {
                set_current_tool(deps, &name, declared_timeout_ms).await?;
            }
            ProviderEvent::ToolCall { id, name, input } => {
                // `is_disallowed` is checked here AND inside
                // `invoke_tool`; the second check is the one that
                // synthesises the refusal text. We still push the
                // PendingToolCall either way so the model sees a
                // matching `tool_result` on the next turn.
                let _ = is_disallowed(&name);
                out.tool_calls.push(PendingToolCall {
                    id,
                    name,
                    input,
                    parse_error: None,
                });
            }
            ProviderEvent::ToolInputParseError {
                tool_use_id,
                tool_name,
                raw_input,
                parse_error,
            } => {
                // The provider couldn't parse the tool_use input JSON
                // the model emitted. Rather than terminating the turn
                // (which would leave the user with no reply, only the
                // generic apology), we synthesise a PendingToolCall
                // tagged with `parse_error`. `drive_turn` recognises
                // these and feeds a `tool_result { is_error: true }`
                // back into the next turn so the model self-corrects.
                tracing::warn!(
                    tool_use_id = %tool_use_id,
                    tool_name = %tool_name,
                    raw_input_bytes = raw_input.len(),
                    parse_error = %parse_error,
                    "tool_use input JSON did not parse; feeding error back to model"
                );
                ironclaw_metrics::inc_provider_retry(deps.provider.name());
                out.tool_calls.push(PendingToolCall {
                    id: tool_use_id,
                    name: tool_name,
                    input: serde_json::Value::Null,
                    parse_error: Some(parse_error),
                });
            }
            ProviderEvent::ToolEnd => {
                clear_current_tool(deps).await?;
            }
            ProviderEvent::Progress { message } => {
                tracing::debug!(message = %message, "provider progress");
                touch_heartbeat(deps.heartbeat_path.as_ref());
            }
            ProviderEvent::Activity => {
                touch_heartbeat(deps.heartbeat_path.as_ref());
            }
        }
    }
    Ok((out, input_tokens, output_tokens))
}

/// Call `provider.query()` with a per-attempt deadline and exponential
/// backoff. Returns the live [`AgentQuery`] once a call succeeds, or a
/// terminal [`ProviderError`] once retries are exhausted (or the failure
/// was non-retryable).
///
/// Behaviour:
/// - Each attempt is wrapped in [`tokio::time::timeout`] with
///   `deps.provider_deadline`.
/// - A timeout is treated as a retryable failure — counts toward the
///   attempt cap just like a 5xx.
/// - Retryable [`ProviderError`]s (`is_retryable() == true`) trigger
///   exponential backoff (250ms → 500ms → 1s) and another attempt.
/// - Non-retryable errors fail-fast on attempt 1.
/// - Final attempt's timeout is converted to
///   [`ProviderError::DeadlineExceeded`].
///
/// All retries fire a `ironclaw_provider_retry_total` counter so the
/// operator dashboard can spot flapping upstreams. Timeout-final fires
/// `ironclaw_provider_deadline_total`.
async fn query_with_retry(
    deps: &RunnerDeps,
    input: QueryInput,
) -> std::result::Result<Box<dyn AgentQuery>, ProviderError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let attempt_started = std::time::Instant::now();
        // Clone the input for this attempt; the previous attempt may
        // have consumed it on a successful call but we never reach
        // here once query() returns Ok, so the borrow checker is fine
        // with a fresh clone per loop iteration.
        //
        // Heartbeat coverage: local-model providers (Ollama) can take
        // 60-180s of prefill before the first token streams back; the
        // host's heartbeat-stale threshold (default 60s) would otherwise
        // kill the container mid-prefill. Holding a HeartbeatTicker for
        // the duration of each provider attempt keeps the file fresh
        // while we wait. The Ticker is RAII-dropped at the end of the
        // attempt — backoff sleeps between attempts are short enough
        // (≤1s) that they don't need their own coverage.
        let _hb = HeartbeatTicker::start(deps.heartbeat_path.clone());
        let result = timeout(deps.provider_deadline, deps.provider.query(input.clone())).await;

        let err: ProviderError = match result {
            Ok(Ok(query)) => return Ok(query),
            Ok(Err(err)) => err,
            Err(_elapsed) => {
                // Per-call deadline tripped.
                tracing::warn!(
                    attempt,
                    max = MAX_PROVIDER_ATTEMPTS,
                    deadline_ms = u64_from_dur(deps.provider_deadline),
                    elapsed_ms = u64_from_dur(attempt_started.elapsed()),
                    provider = deps.provider.name(),
                    "provider query deadline exceeded"
                );
                if attempt >= MAX_PROVIDER_ATTEMPTS {
                    ironclaw_metrics::inc_provider_deadline(deps.provider.name());
                    tracing::error!(
                        attempt,
                        max = MAX_PROVIDER_ATTEMPTS,
                        deadline_ms = u64_from_dur(deps.provider_deadline),
                        provider = deps.provider.name(),
                        "provider deadline exceeded after {ms}ms (attempt {attempt}/{max})",
                        ms = u64_from_dur(deps.provider_deadline),
                        attempt = attempt,
                        max = MAX_PROVIDER_ATTEMPTS,
                    );
                    return Err(ProviderError::DeadlineExceeded {
                        deadline_ms: u64_from_dur(deps.provider_deadline),
                        attempts: attempt,
                    });
                }
                // Treat as retryable; fall through to backoff.
                ironclaw_metrics::inc_provider_retry(deps.provider.name());
                backoff_for_attempt(attempt).await;
                continue;
            }
        };

        // We have a ProviderError. Decide whether to retry.
        if err.is_retryable() && attempt < MAX_PROVIDER_ATTEMPTS {
            tracing::warn!(
                attempt,
                max = MAX_PROVIDER_ATTEMPTS,
                provider = deps.provider.name(),
                error = %err,
                "provider query failed; retrying after backoff"
            );
            ironclaw_metrics::inc_provider_retry(deps.provider.name());
            backoff_for_attempt(attempt).await;
            continue;
        }

        // Terminal: either non-retryable, or we've exhausted attempts.
        if err.is_retryable() {
            tracing::error!(
                attempt,
                max = MAX_PROVIDER_ATTEMPTS,
                provider = deps.provider.name(),
                error = %err,
                "provider query failed; retry budget exhausted"
            );
        } else {
            tracing::error!(
                attempt,
                provider = deps.provider.name(),
                error = %err,
                "provider query failed with non-retryable error"
            );
        }
        return Err(err);
    }
}

/// Compute the backoff delay for the *next* attempt given the current
/// attempt number. With [`INITIAL_PROVIDER_BACKOFF`] = 250ms:
/// - after attempt 1 → 250ms (before attempt 2)
/// - after attempt 2 → 500ms (before attempt 3)
async fn backoff_for_attempt(attempt: u32) {
    let exp = attempt.saturating_sub(1).min(16); // cap shift just in case
    let delay = INITIAL_PROVIDER_BACKOFF
        .checked_mul(1u32 << exp)
        .unwrap_or(Duration::from_secs(60));
    sleep(delay).await;
}

/// Saturating `Duration::as_millis()` → `u64`. The standard
/// `as_millis()` returns `u128`; for tracing fields and the
/// `DeadlineExceeded` variant we want a plain `u64`.
fn u64_from_dur(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Execute one tool call against the runner's tool map. Returns the
/// `(content, is_error)` pair for the `HistoryMessage::Tool` row the
/// model sees on the next turn.
async fn invoke_tool(
    deps: &RunnerDeps,
    call: &PendingToolCall,
) -> (String, bool) {
    if is_disallowed(&call.name) {
        return (
            format!(
                "Tool `{}` is disallowed inside the ironclaw container.",
                call.name
            ),
            true,
        );
    }
    let Some(entry) = deps.tool_map.get(&call.name) else {
        return (
            format!("Unknown tool `{}` — no handler registered.", call.name),
            true,
        );
    };
    // ToolHandler::call wants `Option<JsonObject>`; convert from the
    // Value we got off the wire.
    let arguments = match &call.input {
        serde_json::Value::Object(map) => Some(map.clone()),
        serde_json::Value::Null => None,
        _ => {
            return (
                format!(
                    "Tool `{}` input must be a JSON object, got {}",
                    call.name,
                    short_type(&call.input)
                ),
                true,
            );
        }
    };
    // Keep the heartbeat file fresh for the duration of the tool
    // call. Without this a `shell { cmd: "npm install …" }` (~60-90s
    // on a fresh image) drifts past the host's 60s staleness
    // threshold and the host SIGKILLs the container. Drops on
    // function return; the background task is aborted.
    let _hb = HeartbeatTicker::start(deps.heartbeat_path.clone());
    // Per-tool hard deadline so a wedged tool can't run forever.
    // The provider call has its own deadline (`provider_deadline`);
    // tool dispatch did not, until now. Defaulting to a generous
    // 15 min ceiling — `npm install`, `cargo build`, `apt-get
    // install gcc` are all the kinds of tools we want to permit;
    // anything past that is presumed wedged.
    let call_fut = entry.handler.call(arguments, deps.tool_ctx.as_ref());
    let timeout = std::time::Duration::from_secs(deps.tool_deadline_secs);
    match tokio::time::timeout(timeout, call_fut).await {
        Ok(Ok(result)) => (render_tool_result(&result), false),
        Ok(Err(err)) => (format!("Tool `{}` failed: {err}", call.name), true),
        Err(_) => (
            format!(
                "Tool `{}` did not return within {}s (per-tool deadline); the runner aborted it. Consider breaking the work into smaller steps.",
                call.name, deps.tool_deadline_secs
            ),
            true,
        ),
    }
}

/// Pluck the textual content out of a `CallToolResult`. Multiple
/// blocks get joined with double newlines; non-text blocks
/// (resources, images) are rendered as their type tag so the model
/// at least sees they happened.
fn render_tool_result(result: &rmcp::model::CallToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let raw = &block.raw;
        match raw {
            rmcp::model::RawContent::Text(t) => out.push_str(&t.text),
            rmcp::model::RawContent::Image(_) => out.push_str("<image>"),
            rmcp::model::RawContent::Audio(_) => out.push_str("<audio>"),
            rmcp::model::RawContent::Resource(_) => out.push_str("<resource>"),
        }
    }
    if out.is_empty() {
        "(tool produced no output)".to_string()
    } else {
        out
    }
}

fn short_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Append a `usage_report` system row to `outbound.db`. The host's
/// delivery service intercepts this kind of system action (instead of
/// dispatching it to a channel adapter) and writes the corresponding
/// `agent_turns` row.
async fn emit_usage_report(
    deps: &RunnerDeps,
    input_tokens: u32,
    output_tokens: u32,
    started_at: chrono::DateTime<chrono::Utc>,
    outcome: &TurnOutcome,
) {
    use ironclaw_db::tables::messages_out::{insert as insert_out, WriteOutbound};
    let payload = serde_json::json!({
        "usage_report": {
            "session_id": deps.session_id.to_string(),
            "agent_group_id": deps.agent_group_id.to_string(),
            "seq": deps.turn_seq.load(std::sync::atomic::Ordering::Relaxed),
            "model": deps.model,
            "provider": deps.provider.name(),
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "started_at": started_at.to_rfc3339(),
            "ended_at": chrono::Utc::now().to_rfc3339(),
            "status": match outcome {
                TurnOutcome::Done => "ok",
                TurnOutcome::Failed => "error",
            },
        }
    });
    let row = WriteOutbound {
        id: ironclaw_types::MessageId::new(),
        in_reply_to: None,
        timestamp: chrono::Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: ironclaw_types::MessageKind::System,
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
    deps.turn_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

async fn ack_picked_up(deps: &RunnerDeps, rows: &[MessageInRow]) -> Result<()> {
    let mut g = deps.outbound.lock().await;
    let conn: &mut Connection = &mut g;
    for row in rows {
        // `insert` errors on duplicate; tolerate retries by switching to update.
        match processing_ack::insert(conn, row.id, processing_ack::ProcessingStatus::Processing) {
            Ok(()) => {}
            Err(ironclaw_db::DbError::Sqlite(_)) => {
                processing_ack::update_status(
                    conn,
                    row.id,
                    processing_ack::ProcessingStatus::Processing,
                )?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

async fn finalize_messages(
    deps: &RunnerDeps,
    rows: &[MessageInRow],
    outcome: TurnOutcome,
) -> Result<()> {
    let (in_status, ack_status) = match outcome {
        TurnOutcome::Done => ("completed", processing_ack::ProcessingStatus::Done),
        TurnOutcome::Failed => ("failed", processing_ack::ProcessingStatus::Failed),
    };
    let _ = in_status;
    {
        let inbound = deps.inbound.lock().await;
        for row in rows {
            match outcome {
                TurnOutcome::Done => {
                    let _ = messages_in::mark_completed(&inbound, row.id);
                }
                TurnOutcome::Failed => {
                    let _ = messages_in::mark_failed(&inbound, row.id);
                }
            }
        }
    }
    {
        let mut outbound = deps.outbound.lock().await;
        let conn: &mut Connection = &mut outbound;
        for row in rows {
            processing_ack::update_status(conn, row.id, ack_status)?;
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
    if let TurnOutcome::Failed = outcome {
        if let Err(err) = emit_terminal_failure_apologies(deps, rows).await {
            tracing::warn!(?err, "could not emit terminal-failure apology");
        }
    }
    Ok(())
}

/// One short chat outbound per failed inbound, routed back to the
/// channel the inbound came in on. Idempotent at the per-row level
/// because each inbound has a stable id that flows into `in_reply_to`.
async fn emit_terminal_failure_apologies(
    deps: &RunnerDeps,
    rows: &[MessageInRow],
) -> Result<()> {
    use ironclaw_db::tables::messages_out::{insert as insert_out, WriteOutbound};
    if rows.is_empty() {
        return Ok(());
    }
    let outbound = deps.outbound.lock().await;
    let conn: &rusqlite::Connection = &outbound;
    for row in rows {
        // Only emit for chat inbounds — system / task / wake events
        // don't have a user on the other end to apologize to.
        if !matches!(row.kind, ironclaw_types::MessageKind::Chat) {
            continue;
        }
        // Skip if the inbound has no channel info (e.g. test fixtures
        // that route through a bare router). Without routing the
        // delivery loop can't dispatch the apology anyway.
        let Some(channel_type) = &row.channel_type else {
            continue;
        };
        let Some(platform_id) = &row.platform_id else {
            continue;
        };
        let apology = WriteOutbound {
            id: ironclaw_types::MessageId::new(),
            in_reply_to: Some(row.id),
            timestamp: chrono::Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind: ironclaw_types::MessageKind::Chat,
            channel_type: Some(channel_type.clone()),
            platform_id: Some(platform_id.clone()),
            thread_id: row.thread_id.clone(),
            content: serde_json::json!({
                "text": "I hit a snag processing your message and couldn't finish a reply. Could you try rephrasing or sending a smaller request? (Operator: see runner stderr for the exact error.)",
            }),
        };
        if let Err(err) = insert_out(conn, &apology) {
            tracing::warn!(?err, "apology insert failed");
        }
    }
    Ok(())
}

async fn set_current_tool(
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

async fn clear_current_tool(deps: &RunnerDeps) -> Result<()> {
    let g = deps.outbound.lock().await;
    container_state::clear_tool(&g)?;
    Ok(())
}

/// Refresh the heartbeat file's mtime so the host's container
/// manager knows the runner is alive. Just opening the file is *not*
/// enough — Linux only updates mtime on actual writes, so the file
/// would look frozen at first-create. Truncate to 0 then write one
/// byte; that's the minimum change that bumps mtime portably.
fn touch_heartbeat(path: Option<&PathBuf>) {
    use std::io::Write;
    if let Some(p) = path {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)
        {
            let _ = f.write_all(b".");
        }
    }
}

/// Interval at which a [`HeartbeatTicker`] refreshes the heartbeat
/// file. Picked well under the host's default 60s `heartbeat_stale_secs`
/// so a slow tool call (npm install, apt-get install, compile) can't
/// drift past the staleness threshold while the runner is blocked
/// awaiting `invoke_tool`.
pub(crate) const HEARTBEAT_TICK_INTERVAL_MS: u64 = 5_000;

/// RAII guard that refreshes the heartbeat file every
/// [`HEARTBEAT_TICK_INTERVAL_MS`] while alive.
///
/// The runner's main poll loop touches the heartbeat between turns
/// and when the provider streams `Progress` / `Activity`, but a
/// synchronous `invoke_tool().await` blocks all of that — and a long
/// tool call (npm install, cargo build) easily runs past the host's
/// 60s staleness threshold. The host then SIGKILLs the container
/// thinking the runner has hung. Wrap each tool dispatch with one of
/// these to keep the heartbeat fresh; drop the guard when the tool
/// returns to stop the background task.
pub(crate) struct HeartbeatTicker {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl HeartbeatTicker {
    /// Start a ticker that touches `path` immediately and then every
    /// [`HEARTBEAT_TICK_INTERVAL_MS`] until dropped. When `path` is
    /// `None` (test runners with no heartbeat configured), returns a
    /// no-op guard.
    pub(crate) fn start(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self { handle: None };
        };
        // Touch once up-front so a sub-tick-interval tool call still
        // sees its heartbeat refreshed.
        touch_heartbeat(Some(&path));
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_millis(HEARTBEAT_TICK_INTERVAL_MS),
            );
            // We already touched once; skip the immediate tick.
            interval.tick().await;
            loop {
                interval.tick().await;
                touch_heartbeat(Some(&path));
            }
        });
        Self { handle: Some(handle) }
    }
}

impl Drop for HeartbeatTicker {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::RunnerToolCtx;
    use async_trait::async_trait;
    use ironclaw_db::session::{open_inbound, open_outbound, SessionPaths};
    use ironclaw_db::tables::messages_in::{insert as insert_in, WriteInbound};
    use ironclaw_db::tables::messages_out;
    use ironclaw_providers::{AgentProvider, AgentQuery, ProviderError};
    use ironclaw_types::{AgentGroupId, ChannelType, MessageKind, SessionId};
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
        async fn query(
            &self,
            _input: QueryInput,
        ) -> Result<Box<dyn AgentQuery>, ProviderError> {
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
        };
        insert_in(inbound, &msg).unwrap();
        id
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
            .find(|r| r.kind == ironclaw_types::MessageKind::Chat)
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

        // The outbound must include exactly one Chat apology routed at
        // (telegram, 8929393356) with the inbound id in `in_reply_to`.
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let apologies: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == MessageKind::Chat)
            .collect();
        assert_eq!(
            apologies.len(),
            1,
            "expected exactly one apology row, got: {rows:?}"
        );
        let apology = apologies[0];
        assert_eq!(
            apology.channel_type.as_ref().map(ironclaw_types::ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(apology.platform_id.as_deref(), Some("8929393356"));
        assert_eq!(apology.in_reply_to.map(|m| m.as_uuid()), Some(id.as_uuid()));
        let text = apology
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert!(
            text.contains("snag") || text.contains("couldn't finish"),
            "apology text should be user-facing: {text:?}"
        );
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
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
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
        let outbound = open_outbound(&setup.paths).unwrap();
        let rows = messages_out::list_due(&outbound).unwrap();
        let chat_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .collect();
        // Exactly one chat row — the apology — and it must NOT contain
        // the never-reached fourth-turn text.
        assert_eq!(
            chat_rows.len(),
            1,
            "expected exactly one apology chat row, got: {chat_rows:?}"
        );
        let apology_text = chat_rows[0]
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
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
            if let HistoryMessage::Tool { tool_use_id, content, is_error } = m {
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
            if let HistoryMessage::Tool { tool_use_id, content, is_error } = m {
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
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
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
        assert!(st.current_tool.is_none(), "tool should be cleared by ToolEnd");
        assert!(st.updated_at.is_some());
    }

    #[tokio::test]
    async fn disallowed_tool_produces_refusal_in_history() {
        // First turn: model emits a ToolCall to a disallowed tool;
        // the runner pushes a `Tool { is_error: true }` refusal,
        // then runs a second turn where the model concedes.
        let mut setup = build_setup(vec![
            vec![
                ProviderEvent::ToolCall {
                    id: "tu_1".into(),
                    name: "CronCreate".into(),
                    input: serde_json::json!({}),
                },
            ],
            vec![ProviderEvent::Result {
                text: Some("ok".into()),
            }],
        ]);
        {
            let g = setup.deps.inbound.lock().await;
            insert_pending(&g, "please cron");
        }
        setup.deps.max_turns = Some(1);
        run_loop(setup.deps).await.unwrap();

        let outbound = open_outbound(&setup.paths).unwrap();
        let st = load_state(&outbound).unwrap();
        assert!(
            st.history.iter().any(|m| matches!(
                m,
                HistoryMessage::Tool { content, is_error: true, .. }
                    if content.contains("disallowed")
            )),
            "expected a disallowed-tool refusal in history, got: {:?}",
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
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
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
        async fn query(
            &self,
            _input: QueryInput,
        ) -> Result<Box<dyn AgentQuery>, ProviderError> {
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
        let result = query_with_retry(&deps, dummy_input()).await;
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

        let err = match query_with_retry(&deps, dummy_input()).await {
            Ok(_) => panic!("expected terminal failure"),
            Err(e) => e,
        };
        assert!(matches!(
            err,
            ProviderError::Api { status: 503, .. }
        ));
        assert_eq!(provider.attempts(), MAX_PROVIDER_ATTEMPTS);
    }

    #[tokio::test]
    async fn non_retryable_error_does_not_retry() {
        let provider = PlanProvider::new(vec![PlanStep::Err(ProviderErrorKind::BadRequest)]);
        let deps = deps_with_provider(provider.clone(), Duration::from_secs(5));

        let err = match query_with_retry(&deps, dummy_input()).await {
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
        let err = match query_with_retry(&deps, dummy_input()).await {
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
        let result = query_with_retry(&deps, dummy_input()).await;
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
        let result = query_with_retry(&deps, dummy_input()).await;
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
        let err = match query_with_retry(&deps, dummy_input()).await {
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
        let env = crate::config::MapEnv::from_pairs([(
            PROVIDER_DEADLINE_ENV,
            "45000",
        )]);
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
        let env =
            crate::config::MapEnv::from_pairs([(PROVIDER_DEADLINE_ENV, "1000")]);
        let d = resolve_provider_deadline(&env);
        // Below MIN_PROVIDER_DEADLINE_MS → default.
        assert_eq!(d, Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS));

        let env =
            crate::config::MapEnv::from_pairs([(PROVIDER_DEADLINE_ENV, "999999")]);
        let d = resolve_provider_deadline(&env);
        assert_eq!(d, Duration::from_millis(DEFAULT_PROVIDER_DEADLINE_MS));
    }

    #[tokio::test]
    async fn resolve_provider_deadline_rejects_garbage() {
        let env = crate::config::MapEnv::from_pairs([(
            PROVIDER_DEADLINE_ENV,
            "not-a-number",
        )]);
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
}
