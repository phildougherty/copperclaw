//! Per-inbound orchestrator: loops `run_llm_turn` → execute tools →
//! `run_llm_turn` until the model produces a no-tool turn or we hit the
//! per-inbound cap.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use copperclaw_providers::HistoryMessage;

use super::RunnerDeps;
use super::provider_call::{HeartbeatTicker, run_llm_turn};
use super::tool_dispatch::{ToolImage, invoke_tool};
use crate::state::save_state;

/// Wall-clock budget the runner gives itself between user-facing emits
/// before it surfaces a "still working" status row. Sized to cover one
/// or two long tool calls (npm install, large grep, file build) without
/// chattering, while still putting a heartbeat in chat before the user
/// concludes the agent has hung. The user-reported live failure on
/// 2026-05-24 saw a 5+ minute silent stretch after a child sub-task
/// failed and the parent went off building a prototype — at 60s
/// cadence that stretch would have produced ~5 status rows.
const STATUS_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub(super) struct TurnResult {
    pub(super) continuation: Option<String>,
    pub(super) outcome: TurnOutcome,
}

/// Outcome of one per-inbound drive. Public because the host's
/// provider-resilience integration test (`copperclaw-host`) drives the real
/// emit→record→fold path and needs to construct the same `Failed(reason)` the
/// runner reports, so the `build_usage_report_payload` mapping is exercised
/// end-to-end rather than re-implemented in the test.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// Model produced a final response.
    Done,
    /// Turn could not complete. The wrapped string is a short
    /// human-readable reason ("provider error: …", "exceeded
    /// 60-turn cap", "model emitted malformed JSON 3 turns in a
    /// row") that the apology emitter surfaces to the user instead
    /// of the old generic "I hit a snag". Keep it under ~80 chars —
    /// it's spliced into a single chat sentence.
    Failed(String),
}

/// One pending tool call extracted from a streamed turn.
#[derive(Debug, Clone)]
pub(super) struct PendingToolCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) input: serde_json::Value,
    /// `Some` when the provider couldn't parse the model's `tool_use`
    /// input JSON. The runner skips real tool invocation for this
    /// call and instead feeds the parse error back to the model as a
    /// `tool_result { is_error: true }` so it can self-correct on the
    /// next turn. The `input` field is `Value::Null` in this case.
    pub(super) parse_error: Option<String>,
}

/// What one LLM round-trip produced.
#[derive(Debug, Clone, Default)]
pub(super) struct LlmTurnOutput {
    pub(super) continuation: Option<String>,
    /// Final assistant text accumulated during the stream. May be
    /// empty when the model produced only `tool_use` blocks.
    pub(super) text: String,
    /// Tool calls the model requested. When non-empty the caller
    /// must execute them and run another LLM turn before treating
    /// the message as answered.
    pub(super) tool_calls: Vec<PendingToolCall>,
    /// True if the provider emitted a terminal Error event.
    pub(super) failed: bool,
    /// When `failed` is true and the provider's `Error` event carried
    /// `retryable: true`, this is set so `run_llm_turn` can re-issue
    /// the whole query rather than terminating the inbound. Surfaces the
    /// transport/SSE-decode classification the provider already does.
    pub(super) retryable_failure: bool,
    /// Short, site-specific reason describing why this turn failed
    /// ("provider rejected the query before streaming started",
    /// "provider stream ended with an error event"). When non-empty
    /// `drive_turn` preserves it on the resulting
    /// `TurnOutcome::Failed`; empty falls back to the generic
    /// "did not return a complete response" wording. Decouples
    /// failure-site identification from the empty-string sentinel
    /// the old code used (#12 in code-review notes).
    pub(super) failure_reason: String,
    /// Input tokens billed for THIS LLM round-trip, as reported by the
    /// provider's `Usage` event (0 when the provider didn't surface a
    /// count or the turn failed before streaming). Surfaced up to
    /// `drive_turn` so the tool loop can accumulate per-task cost and
    /// enforce the per-task token ceiling (`COPPERCLAW_MAX_TASK_TOKENS`).
    /// Independent of the Prometheus histogram observe in `run_llm_turn`
    /// — that records every call; this drives the abort decision.
    pub(super) input_tokens: u32,
    /// Output tokens billed for THIS LLM round-trip. See `input_tokens`.
    pub(super) output_tokens: u32,
}

/// Persist `history` + `continuation` to `outbound` mid-message so a
/// crash between tool turns doesn't lose the prior work. Errors are
/// logged at WARN and swallowed — the next iteration (or the
/// end-of-message save in `run_loop`) will retry, and we'd rather make
/// forward progress than abort the turn on a transient `SQLite` hiccup.
async fn persist_mid_message(
    deps: &RunnerDeps,
    history: &[HistoryMessage],
    continuation: Option<&str>,
    tool_turn: usize,
) {
    let g = deps.outbound.lock().await;
    if let Err(err) = save_state(&g, history, continuation) {
        tracing::warn!(
            ?err,
            tool_turn,
            "mid-message save_state failed; continuing (next turn will retry)"
        );
    }
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

/// Content-loop circuit-breaker threshold. Once the model has emitted
/// this many tool calls in a degenerate pattern — either the same
/// `(tool, args)` call N times in a row, or an A,B,A,B oscillation
/// between two distinct calls — the runner concludes it is spinning
/// without making progress and bails. Four is deliberately low: a
/// legitimate workflow that genuinely needs to call the same tool with
/// identical args four times in a row is vanishingly rare (a re-read of
/// the same file with no edit between, say), whereas a wedged local
/// model loops on the same call dozens of times.
///
/// This is a *content* breaker that complements — and does not replace —
/// the consecutive-turn DEPTH cap (`max_tool_turns`) and the token
/// budget. The depth cap and budget bound how *long* the agent runs;
/// this bounds a loop that never advances regardless of how many turns
/// remain. See `identical_tool_call_loop_trips_breaker`.
const TOOL_LOOP_BREAKER_THRESHOLD: usize = 4;

/// Which degenerate tool-call pattern tripped the breaker. Carries the
/// `copperclaw-metrics` `pattern` label value so the call site can't
/// drift from the recorded label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopPattern {
    /// `TOOL_LOOP_BREAKER_THRESHOLD` consecutive identical calls
    /// (same name + identical args).
    Identical,
    /// A,B,A,B alternation between two distinct calls long enough that
    /// the last `TOOL_LOOP_BREAKER_THRESHOLD` calls form the pattern.
    PingPong,
}

impl LoopPattern {
    /// Metric label value (see `copperclaw_metrics::LOOP_PATTERN_*`).
    fn metric_label(self) -> &'static str {
        match self {
            LoopPattern::Identical => copperclaw_metrics::LOOP_PATTERN_IDENTICAL,
            LoopPattern::PingPong => copperclaw_metrics::LOOP_PATTERN_PING_PONG,
        }
    }
}

/// Recursively rewrite `v` so every object's keys are in a stable
/// (sorted) order. The workspace builds `serde_json` with
/// `preserve_order`, so `Value`'s map is insertion-ordered and
/// `to_string()` is key-order-sensitive by default — without this a
/// model that re-emits the same arguments with shuffled keys would
/// fingerprint differently each turn and slip the breaker. Sorting
/// makes `{"a":1,"b":2}` and `{"b":2,"a":1}` serialise identically.
fn canonicalize_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::collections::BTreeMap::new();
            for (k, val) in map {
                sorted.insert(k.clone(), canonicalize_json(val));
            }
            serde_json::to_value(sorted).unwrap_or_else(|_| v.clone())
        }
        serde_json::Value::Array(items) => {
            // Array element order is semantically meaningful (it's not a
            // set), so preserve it — only canonicalise each element.
            serde_json::Value::Array(items.iter().map(canonicalize_json).collect())
        }
        other => other.clone(),
    }
}

/// Rolling fingerprint history for the content-loop breaker. A
/// fingerprint is `name` + canonicalised args JSON (see
/// [`canonicalize_json`]), so two calls match iff the model asked for
/// the same tool with the same arguments regardless of object key order
/// — `{"a":1,"b":2}` and `{"b":2,"a":1}` fingerprint identically.
///
/// Only the trailing window needed to decide either pattern is retained
/// (`TOOL_LOOP_BREAKER_THRESHOLD` entries), so memory is O(threshold)
/// regardless of how many tool calls a long-running turn makes.
#[derive(Debug, Default)]
struct ToolLoopGuard {
    recent: Vec<String>,
}

impl ToolLoopGuard {
    /// Fingerprint one model-requested call. `args` is canonicalised so
    /// semantically identical inputs collapse to one string.
    fn fingerprint(name: &str, args: &serde_json::Value) -> String {
        format!("{name}\u{1f}{}", canonicalize_json(args))
    }

    /// Record one tool call and report the pattern if the trailing
    /// window has degenerated into a loop. `None` means "keep going".
    ///
    /// Detection runs on the model-requested `(name, args)` pairs — it
    /// is intentionally independent of whether each tool *succeeded*, so
    /// a model that re-issues a failing call (unknown tool, repeated
    /// error result) is caught just as a model re-issuing a succeeding
    /// no-op would be.
    fn observe(&mut self, name: &str, args: &serde_json::Value) -> Option<LoopPattern> {
        self.recent.push(Self::fingerprint(name, args));
        // Keep only the window we need to evaluate either pattern.
        let window = TOOL_LOOP_BREAKER_THRESHOLD;
        if self.recent.len() > window {
            let excess = self.recent.len() - window;
            self.recent.drain(0..excess);
        }
        if self.recent.len() < window {
            return None;
        }

        // (a) N identical consecutive calls: every entry in the window
        // is the same fingerprint.
        let first = &self.recent[0];
        if self.recent.iter().all(|fp| fp == first) {
            return Some(LoopPattern::Identical);
        }

        // (b) Ping-pong A,B,A,B: exactly two distinct fingerprints,
        // strictly alternating across the whole window, and not all the
        // same (already excluded above). Requires window >= 4 to be a
        // genuine A,B,A,B rather than a single A,B pair.
        if window >= 4 {
            let a = &self.recent[0];
            let b = &self.recent[1];
            if a != b
                && self
                    .recent
                    .iter()
                    .enumerate()
                    .all(|(i, fp)| fp == if i % 2 == 0 { a } else { b })
            {
                return Some(LoopPattern::PingPong);
            }
        }

        None
    }
}

/// Drive one inbound through to a final assistant response. Loops
/// LLM-turn → execute-tools → LLM-turn until the model produces a
/// turn with no `tool_use` blocks (or we hit `max_tool_turns`).
///
/// `context_block` is the per-inbound "Conversation context"
/// paragraph (rendered once in [`crate::run::run_loop`] and reused
/// across every tool-loop turn within this inbound) that the
/// provider-call layer splices onto the static system prompt. `None`
/// keeps the historical behaviour where the model sees only the
/// pre-baked system prompt — tests that don't care about channel
/// shape can leave it unset.
// `drive_turn` is the central tool-loop orchestrator; its length is
// intrinsic to the state machine (one branch per `TurnOutcome` shape
// times the parse-error vs invoke-tool fork). Splitting further would
// just push the locals into a struct with no readability win.
#[allow(clippy::too_many_lines)]
pub(super) async fn drive_turn(
    deps: &RunnerDeps,
    history: &mut Vec<HistoryMessage>,
    previous_continuation: Option<&str>,
    context_block: Option<&str>,
) -> Result<TurnResult> {
    let mut continuation: Option<String> = previous_continuation.map(str::to_string);
    // Start a fresh rolling-activity chip for this turn (no-op unless
    // COPPERCLAW_BREADCRUMB_STYLE=rolling). Subsequent tool starts/finishes
    // accumulate into the one aggregate chip until the next drive_turn.
    deps.tool_ctx.begin_activity();
    // Counts consecutive turns whose output included any
    // parse-error-tagged tool call. Reset when a turn produces a
    // parse-error-free output. Bounded by
    // `MAX_TOOL_PARSE_ERROR_ATTEMPTS` so a stuck model can't loop us
    // forever.
    let mut consecutive_parse_error_turns: u32 = 0;
    // Wall-clock anchor for the "still working" status heartbeat.
    // Started at drive_turn entry (effectively when the runner picked
    // up the inbound); bumped after each emit so cadence stays at
    // `STATUS_INTERVAL` regardless of how long individual tool calls
    // take. Cumulative `tool_runs` is reported in the status text so
    // the user can see real progress, not just a clock tick.
    let drive_turn_started_at = Instant::now();
    let mut last_status_emit_at = drive_turn_started_at;
    let mut cumulative_tool_runs: usize = 0;
    let mut last_tool_name: Option<String> = None;
    // Cumulative input+output tokens spent across THIS inbound's tool
    // loop. Drives the per-task cost ceiling (`deps.max_task_tokens`),
    // which hard-aborts a runaway mid-loop independently of
    // `max_tool_turns` and the per-day group cap. A token-heavy task
    // that re-reads a large context on each of many tool calls can blow
    // millions of tokens without exceeding the turn cap; this bounds the
    // worst-case spend to the configured ceiling. u64 because a single
    // big-context task can sum past u32::MAX (~4.3B) over a long loop.
    let mut task_tokens_spent: u64 = 0;
    // Content-loop circuit breaker (complements the depth cap below and
    // the token budget). Tracks the trailing window of model-requested
    // `(tool, args)` fingerprints and trips on N-identical or A,B,A,B
    // patterns — see `ToolLoopGuard`.
    let mut loop_guard = ToolLoopGuard::default();

    for tool_turn in 0..deps.max_tool_turns.max(1) {
        let output = run_llm_turn(deps, history, continuation.as_deref(), context_block).await?;
        continuation = output.continuation.or(continuation);
        // Accumulate this round-trip's billed tokens before any
        // early-return below so the per-task total reflects every call
        // the provider charged for (including the turn that trips the
        // ceiling). `failed` turns report 0 tokens, so this is a no-op
        // on the error path.
        task_tokens_spent = task_tokens_spent
            .saturating_add(u64::from(output.input_tokens) + u64::from(output.output_tokens));

        if output.failed {
            // Preserve the site-specific reason from provider_call if
            // it filled one in; otherwise fall back to the generic
            // wording. Only the generic path reaches the user-visible
            // apology when no inner detail was available.
            let reason = if output.failure_reason.is_empty() {
                "the model's provider call did not return a complete response".into()
            } else {
                output.failure_reason
            };
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Failed(reason),
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
            // Empty-reply guard: model returned NO tool calls AND NO
            // text. Without this guard the runner exits silently and
            // the user sees no response at all — caught live on
            // 2026-05-24 with `deepseek/deepseek-v4-flash` + effort=high,
            // where the model returned an HTTP 200 with zero output
            // tokens. Flip to Failed so `emit_terminal_failure_apologies`
            // surfaces an ErrorCard to the originating channel instead.
            if output.text.is_empty() {
                tracing::warn!(
                    target: "copperclaw_runner",
                    provider = %deps.provider.name(),
                    model = %deps.model,
                    "model returned empty reply (no text, no tool call); \
                     surfacing as terminal failure"
                );
                return Ok(TurnResult {
                    continuation,
                    outcome: TurnOutcome::Failed(
                        "the model returned an empty reply — no text and no \
                         tool call. This usually means the model id is \
                         wrong, the provider's response was malformed, or a \
                         reasoning model produced only thinking tokens. Try \
                         a different model or lower the reasoning effort."
                            .to_string(),
                    ),
                });
            }
            let spec = copperclaw_mcp::SendMessageSpec {
                to: None,
                text: output.text,
            };
            let _ack = deps
                .tool_ctx
                .emit_outbound(copperclaw_mcp::OutboundToolEffect::SendMessage(spec))
                .await
                .map_err(|e| anyhow::anyhow!("send_message failed: {e}"))?;
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
        let turn_had_parse_error = output.tool_calls.iter().any(|c| c.parse_error.is_some());
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
        // Content-loop guard runs against the whole batch BEFORE spawning
        // — it is a purely sequential, ordering-sensitive check on the
        // model-requested `(name, args)` fingerprints and must not race
        // with tool execution. Preserve the original semantics exactly:
        // observe each non-parse-error call in original order and keep the
        // FIRST pattern that trips; stop observing once tripped so the
        // trailing window isn't polluted by calls past the trip point.
        // Parse-error synthetic calls (input is Null) are skipped here and
        // bounded instead by the parse-error cap below.
        //
        // Set when the content-loop breaker trips: we still execute +
        // persist the current batch (so the audit history is complete)
        // before bailing.
        let mut tripped_loop: Option<LoopPattern> = None;
        for call in &output.tool_calls {
            if call.parse_error.is_none() && tripped_loop.is_none() {
                tripped_loop = loop_guard.observe(&call.name, &call.input);
            }
        }

        // Execute the batch concurrently, then append results to history
        // in the ORIGINAL call order. Independent calls (e.g. N read_file)
        // finish in ~max(latency) instead of ~sum. Ordering-preserving
        // append keeps transcripts deterministic and tool_use/tool_result
        // pairing intact regardless of completion order. `shell` calls and
        // same-path edit-family calls are serialised inside the batch (see
        // `execute_tool_batch`) because they mutate shared state.
        let batch = execute_tool_batch(deps, &output.tool_calls).await;
        for (call, (content, images, is_error)) in output.tool_calls.iter().zip(batch) {
            cumulative_tool_runs += 1;
            last_tool_name = Some(call.name.clone());
            history.push(HistoryMessage::Tool {
                tool_use_id: call.id.clone(),
                content,
                is_error,
            });
            // A tool that returned image content (e.g. `view_image`)
            // surfaces it as follow-on Image entries so vision models see
            // the pixels. The anthropic serializer puts each in its own
            // user message, so it never mixes with the tool_result block.
            for (media_type, data) in images {
                history.push(HistoryMessage::Image { media_type, data });
            }
        }

        // Persisted mid-message so a crash here (OOM, panic, container
        // kill) doesn't lose the prior tool turns: without this the
        // respawned runner would re-pick the same inbound and start
        // from the pre-message history, repeating every tool call.
        // Failure to save is warn-and-continue — the next iteration or
        // the end-of-message save_state in run_loop will retry.
        persist_mid_message(deps, history, continuation.as_deref(), tool_turn).await;

        // Content-loop circuit breaker. After persisting this turn's
        // tool_results (so the audit history shows the full degenerate
        // run), bail if the model has spun on the same call or
        // oscillated between two. Emits a metric + an audit-grade
        // tracing::error! and surfaces a clear apology reason.
        if let Some(pattern) = tripped_loop {
            copperclaw_metrics::inc_tool_loop_breaker(
                &deps.agent_group_id.as_uuid().to_string(),
                pattern.metric_label(),
            );
            tracing::error!(
                target: "copperclaw_runner",
                agent_group_id = %deps.agent_group_id,
                session_id = %deps.session_id,
                pattern = pattern.metric_label(),
                threshold = TOOL_LOOP_BREAKER_THRESHOLD,
                last_tool = last_tool_name.as_deref().unwrap_or("?"),
                "tool-call loop breaker tripped; terminating inbound",
            );
            let reason = match pattern {
                LoopPattern::Identical => format!(
                    "the agent got stuck repeating the same `{}` tool call {TOOL_LOOP_BREAKER_THRESHOLD} times in a row without making progress",
                    last_tool_name.as_deref().unwrap_or("tool"),
                ),
                LoopPattern::PingPong => format!(
                    "the agent got stuck alternating between two tool calls {TOOL_LOOP_BREAKER_THRESHOLD} times without making progress"
                ),
            };
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Failed(reason),
            });
        }

        // "Still working" heartbeat. Emitted only when the runner is
        // about to loop again (we have tool calls + we're past the
        // parse-error cap check below) AND the user-facing channel has
        // been silent for `STATUS_INTERVAL`. The status emit goes
        // direct to outbound as a Chat row via `emit_status` — it
        // does NOT push into `state.history`, so the model's view of
        // its own turn isn't contaminated by runner-generated chatter.
        // Child-agent sessions (no channel routing) skip inside
        // `RunnerToolCtx::emit_status`, so this is a no-op for them.
        if last_status_emit_at.elapsed() >= STATUS_INTERVAL {
            let elapsed_secs = drive_turn_started_at.elapsed().as_secs();
            let last = last_tool_name.as_deref().unwrap_or("thinking");
            let plural = if cumulative_tool_runs == 1 { "" } else { "s" };
            let status = format!(
                "Still working on this — {elapsed_secs}s in, \
                 {cumulative_tool_runs} tool call{plural} so far (latest: {last}). \
                 I'll keep going."
            );
            deps.tool_ctx.emit_status(&status).await;
            last_status_emit_at = Instant::now();
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
                outcome: TurnOutcome::Failed(format!(
                    "model produced malformed tool-call JSON {consecutive_parse_error_turns} turns in a row"
                )),
            });
        }

        // Per-task cost ceiling. We only reach here when the turn
        // produced tool calls and is about to loop again, so the check
        // sits on the runaway path: a token-heavy loop that re-reads a
        // big context burns the budget here even when it would never hit
        // `max_tool_turns`. `0` disables the ceiling. Fires a metrics
        // trip + ERROR log (the audit narrative) and falls through to
        // the existing terminal-failure path so `finalize_messages`
        // marks the inbound failed and the user sees the surfaced
        // "task budget reached" reason on the apology row. Independent
        // of the per-day group cap (spawn-time gate) and the parse-error
        // / max-turns breakers above.
        if deps.max_task_tokens > 0 && task_tokens_spent >= deps.max_task_tokens {
            tracing::error!(
                agent_group_id = %deps.agent_group_id,
                session_id = %deps.session_id,
                tokens_spent = task_tokens_spent,
                ceiling = deps.max_task_tokens,
                tool_turn,
                "per-task token budget reached; aborting tool loop"
            );
            copperclaw_metrics::inc_task_budget_exhausted(&deps.agent_group_id.to_string());
            return Ok(TurnResult {
                continuation,
                outcome: TurnOutcome::Failed(format!(
                    "task budget reached: {task_tokens_spent} tokens; stopping"
                )),
            });
        }
    }

    // Exhausted the cap. Push a synthetic system message so the
    // model can see what happened on the next inbound, return
    // Failed so finalize_messages marks the inbound that way too.
    let cap = deps.max_tool_turns;
    tracing::warn!(max = cap, "tool-use cycle exceeded max turns; bailing");
    Ok(TurnResult {
        continuation,
        outcome: TurnOutcome::Failed(format!(
            "the agent ran out of turns after {cap} tool calls without finishing the task"
        )),
    })
}

/// One executed tool call's rendering: `(content, images, is_error)` —
/// the text for the `HistoryMessage::Tool` row, any image blocks, and
/// whether the call errored. Mirrors `invoke_tool`'s return shape.
type ToolCallOutput = (String, Vec<ToolImage>, bool);

/// Edit-family tools that mutate a file identified by a `path` input.
/// Two calls in one batch that target the same file both read-modify-
/// write it, so [`execute_tool_batch`] groups them into one serial chain
/// keyed by path. Names mirror the tool-map registrations.
const EDIT_FAMILY_TOOLS: [&str; 4] = ["edit_file", "multi_edit", "apply_patch", "write_file"];

/// Serialisation key for an edit-family call: the target file `path`, so
/// two edits to the *same* file run in order while edits to *different*
/// files stay concurrent. Non-edit tools return `None` (fully
/// concurrent). An edit-family call whose `path` can't be resolved from
/// its input collapses to one shared sentinel key so unknown-target edits
/// never race — conservative, and vanishingly rare in practice.
fn edit_family_path(name: &str, input: &serde_json::Value) -> Option<String> {
    if !EDIT_FAMILY_TOOLS.contains(&name) {
        return None;
    }
    match input.get("path").and_then(serde_json::Value::as_str) {
        Some(path) => Some(path.to_string()),
        None => Some("\u{0}edit-family-unresolved-path\u{0}".to_string()),
    }
}

/// Execute one batch of model-requested tool calls concurrently and
/// return their `(content, images, is_error)` results in the SAME order
/// as `calls`. Independent calls run in parallel (so N slow `read_file`s
/// finish in ~max latency, not ~sum); two classes are serialised because
/// they mutate shared state that would corrupt under interleaving:
///
/// - **`shell`**: persists cwd/env to `/data/.shell_state`, so every
///   `shell` call in the batch runs in its original relative order
///   (still concurrent with the non-shell calls).
/// - **edit family targeting the same path** (`edit_file` / `multi_edit`
///   / `apply_patch` / `write_file`): grouped by `path` and serialised
///   within a group so two writes to one file don't race; different paths
///   stay concurrent.
///
/// A batch-scoped [`HeartbeatTicker`] is held across the whole join so the
/// heartbeat stays fresh even in the micro-gap between two calls in a
/// serial chain (each `invoke_tool` also starts its own per-call ticker).
async fn execute_tool_batch(deps: &RunnerDeps, calls: &[PendingToolCall]) -> Vec<ToolCallOutput> {
    let _batch_hb = HeartbeatTicker::start(deps.heartbeat_path.clone());

    // Assign each call to a serialisation chain. Distinct chains run
    // concurrently; calls within a chain run sequentially in call order.
    let mut chain_of: Vec<usize> = Vec::with_capacity(calls.len());
    let mut next_chain = 0usize;
    let mut shell_chain: Option<usize> = None;
    let mut edit_chains: HashMap<String, usize> = HashMap::new();
    for call in calls {
        let chain = if call.name == "shell" {
            *shell_chain.get_or_insert_with(|| {
                let c = next_chain;
                next_chain += 1;
                c
            })
        } else if let Some(path) = edit_family_path(&call.name, &call.input) {
            *edit_chains.entry(path).or_insert_with(|| {
                let c = next_chain;
                next_chain += 1;
                c
            })
        } else {
            let c = next_chain;
            next_chain += 1;
            c
        };
        chain_of.push(chain);
    }

    // Bucket the original call indices into their chains (call order
    // preserved within each bucket).
    let mut chains: Vec<Vec<usize>> = vec![Vec::new(); next_chain];
    for (idx, &chain) in chain_of.iter().enumerate() {
        chains[chain].push(idx);
    }

    // One future per chain: awaits its calls in order. Chains are polled
    // concurrently by `join_all`, so cross-chain calls overlap.
    let chain_futures = chains.into_iter().map(|indices| async move {
        let mut out: Vec<(usize, ToolCallOutput)> = Vec::with_capacity(indices.len());
        for idx in indices {
            let result = run_one_call(deps, &calls[idx]).await;
            out.push((idx, result));
        }
        out
    });

    // Flatten and restore original call order for a deterministic
    // transcript (tool_use/tool_result pairing must not depend on which
    // call finished first).
    let mut flat: Vec<(usize, ToolCallOutput)> = futures::future::join_all(chain_futures)
        .await
        .into_iter()
        .flatten()
        .collect();
    flat.sort_by_key(|(idx, _)| *idx);
    flat.into_iter().map(|(_, result)| result).collect()
}

/// Run one model-requested tool call to a `(content, images, is_error)`
/// result and flip its breadcrumb chip. Parse-error synthetic calls
/// (input is Null) never dispatch — they hand the model a `tool_result`
/// describing the JSON parse failure so it can re-emit valid input next
/// turn (bounded by the parse-error cap in `drive_turn`).
async fn run_one_call(deps: &RunnerDeps, call: &PendingToolCall) -> ToolCallOutput {
    let (content, images, is_error) = if let Some(parse_err) = call.parse_error.as_deref() {
        (
            format!(
                "Your tool_use input JSON could not be parsed: {parse_err}. Please re-issue this exact tool call with valid JSON.",
            ),
            Vec::new(),
            true,
        )
    } else {
        invoke_tool(deps, call).await
    };
    finish_tool_breadcrumb(deps, call, &content, is_error).await;
    (content, images, is_error)
}

/// Flip the in-place breadcrumb chip from "Running" to "Done"/"Failed"
/// once a tool call returns. Summary is the first non-empty line of the
/// tool result, char-truncated to 200 to fit the breadcrumb schema's
/// `MAX_SUMMARY_CHARS`.
async fn finish_tool_breadcrumb(
    deps: &RunnerDeps,
    call: &PendingToolCall,
    content: &str,
    is_error: bool,
) {
    let summary = first_line_truncated(content, 200);
    deps.tool_ctx
        .emit_breadcrumb_finish(&call.name, Some(&call.input), !is_error, summary.as_deref())
        .await;
}

/// First useful line of `s`, char-truncated to `max_chars` with an
/// ellipsis when truncated. Returns `None` when `s` has no non-whitespace
/// content (so the breadcrumb chip shows status-only instead of an empty
/// summary).
///
/// "Useful" excludes pretty-printed-JSON / array open-brackets (`{` or
/// `[` alone on the first line) — without this filter, a tool that
/// returns `{\n  "wrote_bytes": 1234\n}` would render a breadcrumb chip
/// ending in `— {` instead of `— wrote_bytes: 1234`. The chip is meant
/// to be a quick "what did this produce?" signal; an opening brace
/// signals nothing.
///
/// When the first non-empty line is a bare structural opener, the
/// helper scans forward and concatenates subsequent meaningful lines
/// (trimmed, joined by " · ") until the char budget is consumed — so
/// `{\n  "wrote_bytes": 1234,\n  "path": "/data/x.py"\n}` becomes
/// `"wrote_bytes": 1234, · "path": "/data/x.py" ·` (truncated). The
/// trailing closing brace `}` / `]` is dropped for the same reason.
fn first_line_truncated(s: &str, max_chars: usize) -> Option<String> {
    // First pass: find the first non-empty line.
    let mut lines = s.lines();
    let first_raw = lines.find_map(|l| {
        let t = l.trim();
        if t.is_empty() { None } else { Some(t) }
    })?;

    // If it's a meaningful line (not a bare structural opener), use it
    // as-is. Multi-line tool output like `cargo test` stdout reads
    // best as just the first real line — no join.
    let is_bare_opener = matches!(first_raw, "{" | "}" | "[" | "]" | "{}" | "[]");
    if !is_bare_opener {
        return Some(truncate_with_ellipsis(first_raw, max_chars));
    }

    // Bare-opener case (pretty-printed JSON `{`, `[`): scan forward
    // and join meaningful subsequent lines with " · " so the chip
    // shows the actual fields instead of the useless brace. Caught
    // live on 2026-05-24 when `write_file` returned `{\n  "wrote_bytes":
    // 1234\n}` and the chip rendered `— {`.
    let mut out = String::new();
    for line in lines {
        let t = line.trim();
        if t.is_empty() || matches!(t, "{" | "}" | "[" | "]") {
            continue;
        }
        // Strip a trailing comma — JSON field lines like
        // `"wrote_bytes": 1234,` read cleaner without the comma.
        let t = t.trim_end_matches(',');
        if out.is_empty() {
            out.push_str(t);
        } else {
            if out.chars().count() + 3 + t.chars().count() > max_chars {
                break;
            }
            out.push_str(" · ");
            out.push_str(t);
        }
        if out.chars().count() >= max_chars {
            break;
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(truncate_with_ellipsis(&out, max_chars))
}

/// Char-truncate `s` to `max_chars`, appending `…` when truncated.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod first_line_truncated_tests {
    use super::first_line_truncated;

    #[test]
    fn empty_input_returns_none() {
        assert_eq!(first_line_truncated("", 200), None);
        assert_eq!(first_line_truncated("   \n\t\n", 200), None);
    }

    #[test]
    fn returns_first_non_empty_line() {
        assert_eq!(
            first_line_truncated("first\nsecond", 200),
            Some("first".into())
        );
        assert_eq!(
            first_line_truncated("\n\n  hello \n", 200),
            Some("hello".into())
        );
    }

    #[test]
    fn truncates_with_ellipsis_when_too_long() {
        let s = "a".repeat(250);
        let out = first_line_truncated(&s, 200).unwrap();
        assert_eq!(out.chars().count(), 200);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn skips_bare_open_brace_in_pretty_printed_json() {
        // Regression for the 2026-05-24 live test: write_file tool
        // returned pretty-printed JSON `{\n  "wrote_bytes": 1234\n}`
        // and the breadcrumb chip rendered `— {` (useless). The helper
        // should skip the bare opener and report the next meaningful
        // line.
        let s = "{\n  \"wrote_bytes\": 1234\n}";
        let out = first_line_truncated(s, 200).unwrap();
        assert!(out.contains("wrote_bytes"), "got: {out}");
        assert!(!out.starts_with('{'), "should not start with `{{`: {out}");
    }

    #[test]
    fn joins_multiple_meaningful_lines_with_separator() {
        let s = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}";
        let out = first_line_truncated(s, 200).unwrap();
        // All three field lines should appear, joined by " · ".
        assert!(out.contains("\"a\": 1"), "got: {out}");
        assert!(out.contains("\"b\": 2"), "got: {out}");
        assert!(out.contains("\"c\": 3"), "got: {out}");
        assert!(out.contains(" · "), "uses ` · ` joiner: {out}");
    }

    #[test]
    fn bare_braces_only_returns_none() {
        // A tool result that is JUST `{}` carries no information; the
        // breadcrumb chip should fall back to status-only.
        assert_eq!(first_line_truncated("{}", 200), None);
        assert_eq!(first_line_truncated("{\n}", 200), None);
    }
}

#[cfg(test)]
mod execute_tool_batch_tests {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_mcp::{ToolContext, ToolEntry, ToolError, ToolHandler};
    use copperclaw_providers::{
        AgentProvider, AgentQuery, HistoryMessage, ProviderError, QueryInput,
    };
    use copperclaw_types::{AgentGroupId, ProviderEvent, SessionId};
    use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
    use tokio::sync::Mutex;

    use super::{PendingToolCall, TurnOutcome, drive_turn, execute_tool_batch};
    use crate::run::RunnerDeps;
    use crate::tools::RunnerToolCtx;

    /// Shared high-water mark of concurrently-executing mock tool bodies.
    /// `peak() >= 2` proves two calls genuinely overlapped; `peak() == 1`
    /// proves strict serialisation.
    #[derive(Default)]
    struct ConcurrencyTracker {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl ConcurrencyTracker {
        fn enter(&self) {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
        }
        fn exit(&self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        fn peak(&self) -> usize {
            self.max_active.load(Ordering::SeqCst)
        }
    }

    fn arg_str(arguments: Option<&JsonObject>, key: &str) -> String {
        arguments
            .and_then(|a| a.get(key))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_string()
    }

    fn arg_u64(arguments: Option<&JsonObject>, key: &str) -> u64 {
        arguments
            .and_then(|a| a.get(key))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    }

    /// Mock tool: sleeps `delay_ms` (from args), then echoes its `id`
    /// argument as `done:<id>`. Records start/end events + concurrency.
    struct SlowEcho {
        tracker: Arc<ConcurrencyTracker>,
        log: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait]
    impl ToolHandler for SlowEcho {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            _ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            let id = arg_str(arguments.as_ref(), "id");
            let delay_ms = arg_u64(arguments.as_ref(), "delay_ms");
            self.log.lock().unwrap().push(format!("start:{id}"));
            self.tracker.enter();
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            self.tracker.exit();
            self.log.lock().unwrap().push(format!("end:{id}"));
            Ok(CallToolResult::success(vec![Content::text(format!(
                "done:{id}"
            ))]))
        }
    }

    /// Mock `shell`: emulates the real tool's persisted-cwd semantics
    /// (`/data/.shell_state`). `cmd: "cd <path>"` sleeps 100ms and THEN
    /// records the new cwd — so a concurrently-running `pwd` would read
    /// the stale value; a serialised one observes the change.
    struct MockShell {
        cwd: Arc<StdMutex<String>>,
        tracker: Arc<ConcurrencyTracker>,
    }

    #[async_trait]
    impl ToolHandler for MockShell {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            _ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            let cmd = arg_str(arguments.as_ref(), "cmd");
            self.tracker.enter();
            let out = if let Some(path) = cmd.strip_prefix("cd ") {
                // Sleep BEFORE mutating so an interleaved reader sees
                // the old cwd — this is what makes lost-ordering visible.
                tokio::time::sleep(Duration::from_millis(100)).await;
                *self.cwd.lock().unwrap() = path.to_string();
                String::new()
            } else {
                // pwd
                self.cwd.lock().unwrap().clone()
            };
            self.tracker.exit();
            Ok(CallToolResult::success(vec![Content::text(out)]))
        }
    }

    /// Mock `write_file`: read-modify-write against a shared per-path
    /// map with a sleep in the middle, so two concurrent appends to the
    /// same path lose an update while serialised appends compose.
    struct MockWriteFile {
        files: Arc<StdMutex<HashMap<String, String>>>,
        tracker: Arc<ConcurrencyTracker>,
    }

    #[async_trait]
    impl ToolHandler for MockWriteFile {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            _ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            let path = arg_str(arguments.as_ref(), "path");
            let append = arg_str(arguments.as_ref(), "append");
            self.tracker.enter();
            let current = self
                .files
                .lock()
                .unwrap()
                .get(&path)
                .cloned()
                .unwrap_or_default();
            tokio::time::sleep(Duration::from_millis(50)).await;
            let merged = format!("{current}{append}");
            self.files.lock().unwrap().insert(path, merged.clone());
            self.tracker.exit();
            Ok(CallToolResult::success(vec![Content::text(merged)]))
        }
    }

    /// Provider stub for the batch-execution tests (never queried) and
    /// scripted variant for the `drive_turn` loop-guard test.
    struct ScriptedProvider {
        scripts: StdMutex<Vec<Vec<ProviderEvent>>>,
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

    /// Build `RunnerDeps` whose `tool_map` holds exactly the supplied mock
    /// handlers (keyed by tool name) and whose provider replays `scripts`.
    fn deps_with_mocks(
        mocks: Vec<(&'static str, Box<dyn ToolHandler>)>,
        scripts: Vec<Vec<ProviderEvent>>,
    ) -> (tempfile::TempDir, RunnerDeps) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let tool_ctx: Arc<dyn ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let provider: Arc<dyn AgentProvider> = Arc::new(ScriptedProvider {
            scripts: StdMutex::new(scripts),
        });
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(provider, tool_ctx, inbound, outbound, archive_dir);
        let mut map: HashMap<String, Arc<ToolEntry>> = HashMap::new();
        for (name, handler) in mocks {
            map.insert(
                name.to_string(),
                Arc::new(ToolEntry {
                    tool: Tool {
                        name: Cow::Borrowed(name),
                        description: None,
                        input_schema: Arc::new(JsonObject::new()),
                        annotations: None,
                    },
                    handler,
                }),
            );
        }
        deps.tool_map = Arc::new(map);
        (tmp, deps)
    }

    fn call(name: &str, id: &str, input: serde_json::Value) -> PendingToolCall {
        PendingToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
            parse_error: None,
        }
    }

    /// N independent slow calls complete in ~max latency, not ~sum. The
    /// concurrency high-water mark is the primary (deterministic)
    /// assertion; the wall-clock bound is a generous secondary check
    /// (sum would be 4 x 100ms = 400ms).
    #[tokio::test]
    async fn independent_calls_run_concurrently() {
        let tracker = Arc::new(ConcurrencyTracker::default());
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (_tmp, deps) = deps_with_mocks(
            vec![(
                "mock_read",
                Box::new(SlowEcho {
                    tracker: tracker.clone(),
                    log,
                }),
            )],
            vec![],
        );
        let calls: Vec<PendingToolCall> = (0..4)
            .map(|i| {
                call(
                    "mock_read",
                    &format!("tu_{i}"),
                    serde_json::json!({"id": format!("c{i}"), "delay_ms": 100}),
                )
            })
            .collect();
        let started = Instant::now();
        let results = execute_tool_batch(&deps, &calls).await;
        let elapsed = started.elapsed();
        assert_eq!(results.len(), 4);
        assert_eq!(
            tracker.peak(),
            4,
            "all four independent calls must be in flight at once"
        );
        assert!(
            elapsed < Duration::from_millis(300),
            "4 x 100ms independent calls must finish in ~max, not ~sum; took {elapsed:?}"
        );
    }

    /// Results come back in the ORIGINAL call order even when the first
    /// call is the slowest (completion order is reversed).
    #[tokio::test]
    async fn results_keep_original_call_order() {
        let tracker = Arc::new(ConcurrencyTracker::default());
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (_tmp, deps) = deps_with_mocks(
            vec![(
                "mock_read",
                Box::new(SlowEcho {
                    tracker,
                    log: log.clone(),
                }),
            )],
            vec![],
        );
        let calls = vec![
            call(
                "mock_read",
                "tu_a",
                serde_json::json!({"id": "a", "delay_ms": 150}),
            ),
            call(
                "mock_read",
                "tu_b",
                serde_json::json!({"id": "b", "delay_ms": 50}),
            ),
            call(
                "mock_read",
                "tu_c",
                serde_json::json!({"id": "c", "delay_ms": 0}),
            ),
        ];
        let results = execute_tool_batch(&deps, &calls).await;
        let contents: Vec<&str> = results.iter().map(|(c, _, _)| c.as_str()).collect();
        assert_eq!(
            contents,
            vec!["done:a", "done:b", "done:c"],
            "results must be ordered by call position, not completion time"
        );
        // Sanity: completion order genuinely differed from call order.
        let events = log.lock().unwrap().clone();
        let end_c = events.iter().position(|e| e == "end:c").unwrap();
        let end_a = events.iter().position(|e| e == "end:a").unwrap();
        assert!(
            end_c < end_a,
            "the fast call must have finished first: {events:?}"
        );
    }

    /// Two `shell` calls in one batch run in order relative to each
    /// other — the second observes the first's cwd change — while a
    /// non-shell call still overlaps them.
    #[tokio::test]
    async fn shell_calls_serialize_and_observe_cwd_in_order() {
        let tracker = Arc::new(ConcurrencyTracker::default());
        let log = Arc::new(StdMutex::new(Vec::new()));
        let cwd = Arc::new(StdMutex::new("/".to_string()));
        let (_tmp, deps) = deps_with_mocks(
            vec![
                (
                    "shell",
                    Box::new(MockShell {
                        cwd,
                        tracker: tracker.clone(),
                    }),
                ),
                (
                    "mock_read",
                    Box::new(SlowEcho {
                        tracker: tracker.clone(),
                        log,
                    }),
                ),
            ],
            vec![],
        );
        let calls = vec![
            call("shell", "tu_1", serde_json::json!({"cmd": "cd /work"})),
            call("shell", "tu_2", serde_json::json!({"cmd": "pwd"})),
            call(
                "mock_read",
                "tu_3",
                serde_json::json!({"id": "r", "delay_ms": 100}),
            ),
        ];
        let results = execute_tool_batch(&deps, &calls).await;
        // The second shell call must see the first one's cwd change: had
        // they run concurrently, `pwd` (no sleep) would have returned "/".
        assert_eq!(
            results[1].0, "/work",
            "second shell call must observe the first's cwd change"
        );
        // The non-shell call still overlapped the shell chain.
        assert!(
            tracker.peak() >= 2,
            "non-shell call must run concurrently with the shell chain; peak={}",
            tracker.peak()
        );
    }

    /// Edit-family calls targeting the SAME path serialize (no lost
    /// update); a call to a DIFFERENT path overlaps them.
    #[tokio::test]
    async fn edit_family_same_path_serializes_different_paths_overlap() {
        let tracker = Arc::new(ConcurrencyTracker::default());
        let files = Arc::new(StdMutex::new(HashMap::new()));
        let (_tmp, deps) = deps_with_mocks(
            vec![(
                "write_file",
                Box::new(MockWriteFile {
                    files: files.clone(),
                    tracker: tracker.clone(),
                }),
            )],
            vec![],
        );
        let calls = vec![
            call(
                "write_file",
                "tu_1",
                serde_json::json!({"path": "/data/f", "append": "a"}),
            ),
            call(
                "write_file",
                "tu_2",
                serde_json::json!({"path": "/data/f", "append": "b"}),
            ),
            call(
                "write_file",
                "tu_3",
                serde_json::json!({"path": "/data/g", "append": "x"}),
            ),
        ];
        let results = execute_tool_batch(&deps, &calls).await;
        // Serialised same-path appends compose; a concurrent interleave
        // would lose the first append ("b" instead of "ab").
        assert_eq!(
            results[1].0, "ab",
            "second same-path edit must observe the first's write"
        );
        assert_eq!(
            files.lock().unwrap().get("/data/f").map(String::as_str),
            Some("ab")
        );
        // The different-path edit overlapped the same-path chain.
        assert!(
            tracker.peak() >= 2,
            "different-path edit must run concurrently; peak={}",
            tracker.peak()
        );
    }

    /// A single batch whose calls are duplicate-heavy (four identical
    /// `(tool, args)` pairs) trips the content-loop breaker: the batch
    /// still executes fully (audit history complete) but the turn ends
    /// `Failed` with the loop-specific reason.
    #[tokio::test]
    async fn loop_guard_trips_on_duplicate_heavy_batch() {
        let tracker = Arc::new(ConcurrencyTracker::default());
        let log = Arc::new(StdMutex::new(Vec::new()));
        let identical = || ProviderEvent::ToolCall {
            id: "tu_dup".into(),
            name: "mock_read".into(),
            input: serde_json::json!({"id": "same", "delay_ms": 0}),
        };
        let (_tmp, deps) = deps_with_mocks(
            vec![(
                "mock_read",
                Box::new(SlowEcho {
                    tracker,
                    log: log.clone(),
                }),
            )],
            // One scripted turn: a duplicate-heavy batch. A second turn
            // must never be consumed — the breaker ends the inbound.
            vec![
                vec![identical(), identical(), identical(), identical()],
                vec![ProviderEvent::Result {
                    text: Some("must never be reached".into()),
                }],
            ],
        );
        let mut history: Vec<HistoryMessage> = Vec::new();
        let result = drive_turn(&deps, &mut history, None, None).await.unwrap();
        let TurnOutcome::Failed(reason) = result.outcome else {
            panic!("duplicate-heavy batch must fail the turn: {result:?}");
        };
        assert!(
            reason.contains("repeating the same"),
            "reason must name the loop, got: {reason}"
        );
        // The whole batch still executed and landed in history (audit
        // trail stays complete even when the breaker trips).
        let tool_results = history
            .iter()
            .filter(|m| matches!(m, HistoryMessage::Tool { .. }))
            .count();
        assert_eq!(tool_results, 4, "all four batch calls must be in history");
        assert_eq!(log.lock().unwrap().len(), 8, "4 starts + 4 ends");
    }

    /// A single-tool batch behaves exactly like the old sequential path:
    /// one result, correct pairing, no reordering.
    #[tokio::test]
    async fn single_call_batch_is_equivalent_to_sequential() {
        let tracker = Arc::new(ConcurrencyTracker::default());
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (_tmp, deps) = deps_with_mocks(
            vec![("mock_read", Box::new(SlowEcho { tracker, log }))],
            vec![],
        );
        let calls = vec![call(
            "mock_read",
            "tu_only",
            serde_json::json!({"id": "solo", "delay_ms": 0}),
        )];
        let results = execute_tool_batch(&deps, &calls).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "done:solo");
        assert!(!results[0].2, "successful call must not be an error");
    }

    /// Parse-error synthetic calls never dispatch — they produce the
    /// canned feedback result even inside a concurrent batch, and real
    /// calls around them still run.
    #[tokio::test]
    async fn parse_error_calls_synthesize_feedback_in_batch() {
        let tracker = Arc::new(ConcurrencyTracker::default());
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (_tmp, deps) = deps_with_mocks(
            vec![(
                "mock_read",
                Box::new(SlowEcho {
                    tracker,
                    log: log.clone(),
                }),
            )],
            vec![],
        );
        let calls = vec![
            PendingToolCall {
                id: "tu_bad".into(),
                name: "mock_read".into(),
                input: serde_json::Value::Null,
                parse_error: Some("EOF while parsing".into()),
            },
            call(
                "mock_read",
                "tu_ok",
                serde_json::json!({"id": "ok", "delay_ms": 0}),
            ),
        ];
        let results = execute_tool_batch(&deps, &calls).await;
        assert!(results[0].2, "parse-error call must be an error result");
        assert!(
            results[0].0.contains("could not be parsed"),
            "got: {}",
            results[0].0
        );
        assert_eq!(results[1].0, "done:ok");
        // Only the real call dispatched.
        assert_eq!(log.lock().unwrap().len(), 2, "1 start + 1 end");
    }
}

#[cfg(test)]
mod edit_family_path_tests {
    use super::edit_family_path;
    use serde_json::json;

    #[test]
    fn edit_family_tools_key_on_path() {
        for name in ["edit_file", "multi_edit", "apply_patch", "write_file"] {
            assert_eq!(
                edit_family_path(name, &json!({"path": "/data/x"})),
                Some("/data/x".to_string()),
                "{name} must serialize by path"
            );
        }
    }

    #[test]
    fn non_edit_tools_are_unkeyed() {
        assert_eq!(
            edit_family_path("read_file", &json!({"path": "/data/x"})),
            None
        );
        assert_eq!(edit_family_path("shell", &json!({"cmd": "ls"})), None);
        assert_eq!(edit_family_path("grep", &json!({"pattern": "x"})), None);
    }

    #[test]
    fn missing_path_collapses_to_shared_sentinel() {
        // Unresolvable targets all share one chain so they never race.
        let a = edit_family_path("write_file", &json!({}));
        let b = edit_family_path("edit_file", &serde_json::Value::Null);
        assert!(a.is_some());
        assert_eq!(a, b, "unresolved edit targets must share one chain");
    }
}

#[cfg(test)]
mod tool_loop_guard_tests {
    use super::{LoopPattern, TOOL_LOOP_BREAKER_THRESHOLD, ToolLoopGuard};
    use serde_json::json;

    /// The threshold this suite is written against. If someone retunes
    /// the constant the alternation/identity arithmetic below needs a
    /// fresh look, so pin it.
    #[test]
    fn threshold_is_four() {
        assert_eq!(TOOL_LOOP_BREAKER_THRESHOLD, 4);
    }

    #[test]
    fn identical_calls_trip_at_threshold() {
        let mut g = ToolLoopGuard::default();
        let args = json!({"path": "/data/x"});
        // First THRESHOLD-1 observations must not trip.
        for _ in 0..TOOL_LOOP_BREAKER_THRESHOLD - 1 {
            assert_eq!(g.observe("read_file", &args), None);
        }
        // The THRESHOLD-th identical call trips the breaker.
        assert_eq!(g.observe("read_file", &args), Some(LoopPattern::Identical));
    }

    #[test]
    fn arg_key_order_does_not_matter() {
        // Two JSON objects with the same fields in different key order
        // must fingerprint identically (serde_json normalises key order
        // on a map), so a model that re-emits the same call with shuffled
        // keys still counts as identical.
        let mut g = ToolLoopGuard::default();
        let a = json!({"a": 1, "b": 2});
        let b = json!({"b": 2, "a": 1});
        assert_eq!(g.observe("t", &a), None);
        assert_eq!(g.observe("t", &b), None);
        assert_eq!(g.observe("t", &a), None);
        assert_eq!(g.observe("t", &b), Some(LoopPattern::Identical));
    }

    #[test]
    fn different_args_do_not_trip_identical() {
        // Same tool, marching args — legitimate progress, never trips.
        let mut g = ToolLoopGuard::default();
        for i in 0..(TOOL_LOOP_BREAKER_THRESHOLD * 3) {
            assert_eq!(
                g.observe("read_file", &json!({"line": i})),
                None,
                "distinct args must not trip at i={i}"
            );
        }
    }

    #[test]
    fn ping_pong_trips() {
        // A,B,A,B alternation between two distinct calls.
        let mut g = ToolLoopGuard::default();
        let a = json!({"cmd": "ls"});
        let b = json!({"cmd": "pwd"});
        assert_eq!(g.observe("shell", &a), None); // A
        assert_eq!(g.observe("shell", &b), None); // A,B
        assert_eq!(g.observe("shell", &a), None); // A,B,A
        assert_eq!(g.observe("shell", &b), Some(LoopPattern::PingPong)); // A,B,A,B
    }

    #[test]
    fn ping_pong_distinguishes_tool_name() {
        // Same args but alternating tool *names* is still a ping-pong.
        let mut g = ToolLoopGuard::default();
        let args = json!({});
        assert_eq!(g.observe("alpha", &args), None);
        assert_eq!(g.observe("beta", &args), None);
        assert_eq!(g.observe("alpha", &args), None);
        assert_eq!(g.observe("beta", &args), Some(LoopPattern::PingPong));
    }

    #[test]
    fn three_way_rotation_does_not_trip() {
        // A,B,C,A,B,C is genuine multi-step work, not a 2-state loop.
        let mut g = ToolLoopGuard::default();
        let names = ["a", "b", "c"];
        for i in 0..12 {
            let res = g.observe(names[i % 3], &json!({}));
            assert_eq!(res, None, "3-way rotation tripped at i={i}");
        }
    }

    #[test]
    fn identical_takes_precedence_over_ping_pong() {
        // Once a run goes fully identical it reports Identical, not
        // PingPong (the all-equal check runs first).
        let mut g = ToolLoopGuard::default();
        let a = json!({"x": 1});
        for _ in 0..TOOL_LOOP_BREAKER_THRESHOLD {
            g.observe("t", &a);
        }
        // Window is now all-A; the most recent observation reported
        // Identical (verified in `identical_calls_trip_at_threshold`).
        assert_eq!(g.observe("t", &a), Some(LoopPattern::Identical));
    }
}
