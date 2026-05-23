//! Provider-call layer: retry + deadline wrappers around `AgentProvider::query`,
//! the streamed-event pump, and the heartbeat ticker that keeps the host happy
//! during a slow provider attempt.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ironclaw_providers::{AgentQuery, ProviderError, QueryInput};
use ironclaw_types::ProviderEvent;
use tokio::time::{sleep, timeout};

use crate::disallowed::is_disallowed;

use super::drive_turn::{LlmTurnOutput, PendingToolCall, TurnOutcome};
use super::{emit_usage_report, set_current_tool, clear_current_tool, RunnerDeps};

/// Maximum number of `provider.query()` attempts (including the first)
/// before the runner gives up and marks the inbound failed. Hard-coded
/// rather than configurable: 3 strikes is the standard SRE default for
/// idempotent retries and we don't want to give operators a footgun for
/// "infinite retry on a flapping API".
pub(super) const MAX_PROVIDER_ATTEMPTS: u32 = 3;

/// Initial backoff between retries; doubles each attempt:
/// 250ms → 500ms → 1s.
const INITIAL_PROVIDER_BACKOFF: Duration = Duration::from_millis(250);

/// Make one LLM call. Pumps the streamed provider events into
/// per-turn buffers and returns when the stream ends.
pub(super) async fn run_llm_turn(
    deps: &RunnerDeps,
    history: &[ironclaw_providers::HistoryMessage],
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
pub(super) async fn pump_events(
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
pub(super) async fn query_with_retry(
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
pub(super) async fn backoff_for_attempt(attempt: u32) {
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

/// Refresh the heartbeat file's mtime so the host's container
/// manager knows the runner is alive. Just opening the file is *not*
/// enough — Linux only updates mtime on actual writes, so the file
/// would look frozen at first-create. Truncate to 0 then write one
/// byte; that's the minimum change that bumps mtime portably.
pub(super) fn touch_heartbeat(path: Option<&PathBuf>) {
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
