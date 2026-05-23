//! Per-inbound orchestrator: loops `run_llm_turn` → execute tools →
//! `run_llm_turn` until the model produces a no-tool turn or we hit the
//! per-inbound cap.

use anyhow::Result;
use ironclaw_providers::HistoryMessage;

use super::provider_call::run_llm_turn;
use super::tool_dispatch::invoke_tool;
use super::RunnerDeps;

#[derive(Debug, Clone)]
pub(super) struct TurnResult {
    pub(super) continuation: Option<String>,
    pub(super) outcome: TurnOutcome,
}

#[derive(Debug, Clone)]
pub(super) enum TurnOutcome {
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
pub(super) async fn drive_turn(
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
                outcome: TurnOutcome::Failed(format!(
                    "model produced malformed tool-call JSON {consecutive_parse_error_turns} turns in a row"
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
