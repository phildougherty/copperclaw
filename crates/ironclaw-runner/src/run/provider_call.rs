//! Provider-call layer: retry + deadline wrappers around `AgentProvider::query`,
//! the streamed-event pump, and the heartbeat ticker that keeps the host happy
//! during a slow provider attempt.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use ironclaw_providers::{AgentQuery, ProviderError, QueryInput};
use ironclaw_types::ProviderEvent;
use tokio::time::{sleep, timeout};

use crate::disallowed::is_disallowed;

use super::drive_turn::{LlmTurnOutput, PendingToolCall, TurnOutcome};
use super::prompt::system_with_context;
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

/// Cap on the `failure_reason` we splice into the user-visible apology.
/// The apology format is `"I couldn't finish a reply on that message — {reason}. Try ..."`,
/// so a 200-char reason keeps the whole message under ~300 chars even
/// on the cli channel.
const FAILURE_REASON_CAP: usize = 200;

/// Build a concise, user-visible failure reason from a `ProviderError`.
/// Includes the underlying error message (which carries the HTTP
/// status and the API's response body for the `Api` variant, the
/// transport detail for `Transport`, etc.) so the apology actually
/// tells the user what went wrong instead of a bare "rejected the
/// query".
fn format_provider_failure_reason(err: &ProviderError) -> String {
    let raw = format!("provider rejected the query before streaming started ({err})");
    // Trim at a char boundary, append `…` if we truncated. Cheaper
    // than pulling unicode-segmentation; correct because `chars()`
    // returns codepoints.
    if raw.chars().count() <= FAILURE_REASON_CAP {
        return raw;
    }
    let prefix: String = raw.chars().take(FAILURE_REASON_CAP.saturating_sub(1)).collect();
    format!("{prefix}…")
}

/// Make one LLM call. Pumps the streamed provider events into
/// per-turn buffers and returns when the stream ends.
///
/// `context_block` is the per-inbound "Conversation context" paragraph
/// (see `super::prompt::render_conversation_context`) appended to the
/// static `deps.system` for this single turn. `None` keeps the
/// historical behaviour (model sees only the static system prompt) so
/// callers and tests that don't care about channel context aren't
/// forced to populate the field.
pub(super) async fn run_llm_turn(
    deps: &RunnerDeps,
    history: &[ironclaw_providers::HistoryMessage],
    previous_continuation: Option<&str>,
    context_block: Option<&str>,
) -> Result<LlmTurnOutput> {
    let system = system_with_context(&deps.system, context_block);
    let input = QueryInput {
        system,
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
        // Keep the typing-indicator path (and the heartbeat-stale
        // supervisor) refreshed for the entire `query + pump_events`
        // cycle, not just the initial HTTP call that `query_with_retry`
        // covers with its own HeartbeatTicker. Without this, a 30s
        // LLM stream that doesn't emit `Progress` / `Activity` events
        // between chunks would let the typing bubble fade out on
        // channels with a ~5s indicator window (Telegram, Slack,
        // Discord). The pinger defaults to refreshing the heartbeat
        // file (HeartbeatPinger), but is overridable via
        // RunnerDeps::activity_pinger so tests can count pings.
        let _activity = ProviderActivityTicker::start(Arc::clone(&deps.activity_pinger));
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
                // Include the underlying provider error in the
                // failure_reason so the user-visible apology actually
                // says WHY (e.g. "api error 400: prompt is too long:
                // 250000 tokens, max 200000" instead of the bare
                // "provider rejected the query"). Cap at 200 chars so
                // a giant 4xx body doesn't overflow the apology.
                // `ProviderError` derives Display via thiserror so
                // `to_string()` gives a structured message ("api
                // error 400: …", "transport error: …", etc.).
                let reason = format_provider_failure_reason(&err);
                let out = LlmTurnOutput {
                    failed: true,
                    failure_reason: reason.clone(),
                    ..LlmTurnOutput::default()
                };
                // Same SPECIFIC reason on the usage_report so the
                // failure mode is greppable in the audit log instead
                // of being conflated with the post-stream failure
                // path below. `drive_turn` will preserve this reason
                // verbatim when wrapping the per-turn output for the
                // run loop (see #12 in code-review notes).
                emit_usage_report(
                    deps,
                    0,
                    0,
                    turn_started_at,
                    &TurnOutcome::Failed(reason),
                )
                .await;
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
        // Specific reason for the usage report so the audit log shows
        // *where* the failure happened (stream-time vs query-time —
        // see the matching reason in the query_with_retry branch
        // above). `drive_turn` overwrites this with a higher-level
        // reason for the user-visible apology unless empty.
        TurnOutcome::Failed("provider stream ended with an error event".into())
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
#[allow(clippy::too_many_lines)]
pub(super) async fn pump_events(
    deps: &RunnerDeps,
    query: &mut dyn AgentQuery,
) -> Result<(LlmTurnOutput, u32, u32)> {
    let mut out = LlmTurnOutput::default();
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;

    while let Some(event) = query.next_event().await {
        // Per-chunk activity ping. The background ProviderActivityTicker
        // covers the long-silence case (10s between SSE chunks); this
        // covers the opposite case (high-frequency token-by-token
        // streaming) so each useful chunk also refreshes the typing
        // signal without waiting for the next 3s tick.
        deps.activity_pinger.ping();
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
                // Site-specific reason so drive_turn can preserve it
                // instead of falling back to the generic "did not
                // return a complete response" wording. Empty-string
                // sentinel is reserved for "no reason captured".
                if out.failure_reason.is_empty() {
                    // Splice the actual provider message in (capped)
                    // so the apology says what happened, not just
                    // "ended with an error event". The `message`
                    // field on ProviderEvent::Error carries whatever
                    // the provider streamed back as the error body.
                    let trimmed: String = message
                        .chars()
                        .take(FAILURE_REASON_CAP.saturating_sub(60))
                        .collect();
                    let suffix = if message.chars().count() > FAILURE_REASON_CAP - 60 {
                        "…"
                    } else {
                        ""
                    };
                    out.failure_reason = format!(
                        "provider stream ended with an error event ({trimmed}{suffix})"
                    );
                }
                break;
            }
            ProviderEvent::ToolStart {
                name,
                declared_timeout_ms,
            } => {
                // Best-effort container_state housekeeping: a transient
                // SQLite lock contention (or any other write error) here
                // must NOT abort the stream pump. The stuck-tool
                // detector consumes this row to time out wedged tools,
                // but a single missed write only means one tool's
                // started_at is briefly stale — preferable to losing
                // every mid-stream tool_use event by propagating the
                // error and crashing the runner. Matches the
                // let-the-write-fail convention used elsewhere in the
                // runner for non-load-bearing DB writes (inbound status
                // updates etc.).
                if let Err(e) = set_current_tool(deps, &name, declared_timeout_ms).await {
                    tracing::warn!(
                        tool = %name,
                        error = %e,
                        "set_current_tool failed; continuing stream pump",
                    );
                }
                // Breadcrumb emit moved to ToolCall below — by that
                // point we have the full input JSON and can include
                // the command / query / path in the breadcrumb. The
                // ToolStart timing was the wrong place: the input
                // hasn't been reassembled from streaming deltas yet.
            }
            ProviderEvent::ToolCall { id, name, input } => {
                // `is_disallowed` is checked here AND inside
                // `invoke_tool`; the second check is the one that
                // synthesises the refusal text. We still push the
                // PendingToolCall either way so the model sees a
                // matching `tool_result` on the next turn.
                let _ = is_disallowed(&name);
                // Optional `[tool detail]` chat breadcrumb for
                // user-visible observability during long agent turns.
                // Default no-op via the trait; the RunnerToolCtx impl
                // gates on `IRONCLAW_TOOL_BREADCRUMBS=1`, the tool
                // allowlist, and extracts a per-tool detail (command
                // for shell, query for web_search, path for
                // write_file, etc.) from the input.
                deps.tool_ctx.emit_breadcrumb(&name, Some(&input)).await;
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
                // Best-effort housekeeping; see the ToolStart arm above
                // for the rationale. Propagating this error would crash
                // the runner mid-stream and lose any subsequent events
                // (final assistant text, additional tool_use blocks).
                if let Err(e) = clear_current_tool(deps).await {
                    tracing::warn!(
                        error = %e,
                        "clear_current_tool failed; continuing stream pump",
                    );
                }
            }
            ProviderEvent::Progress { message } => {
                tracing::debug!(message = %message, "provider progress");
                touch_heartbeat(deps.heartbeat_path.as_ref());
            }
            ProviderEvent::Activity => {
                touch_heartbeat(deps.heartbeat_path.as_ref());
            }
            ProviderEvent::Thinking { text, redacted } => {
                // Slice-3.5 opt-in pipeline. The Anthropic provider
                // emits one of these at every `thinking` /
                // `redacted_thinking` content_block_stop boundary so the
                // runner can surface the reasoning to the user as a
                // collapsed native UI primitive (Telegram `<blockquote
                // expandable>`, Slack `context`, Discord muted embed,
                // Google Chat `collapsibleSection`, Matrix `<details>`).
                //
                // The privacy gate lives HERE (canonical opt-in check):
                // unless the operator has flipped the per-group
                // `surface_thinking` flag (default false), we drop the
                // event on the floor — matching historical behaviour.
                // The orthogonal `strip_reasoning_blocks` sanitiser
                // (which scrubs inline `<thinking>` markup from Chat
                // rows in `apply_send_message`) is unchanged: that path
                // protects against prose contamination in the chat
                // reply, this path optionally surfaces structured
                // reasoning as its own row.
                if !deps.surface_thinking {
                    continue;
                }
                deps.tool_ctx
                    .emit_thinking(&text, redacted, Some(&deps.model))
                    .await;
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

/// Interval at which a [`ProviderActivityTicker`] pings the configured
/// [`ProviderActivityPinger`] while a provider call is in flight. 3s is
/// well below the host's typing-indicator fade-out window (~5s on
/// Telegram / Slack / Discord) so the bubble never gets a chance to
/// vanish between provider chunks. It is also short enough that even a
/// 30s LLM stream produces ~10 pings, which is plenty of headroom for
/// the typing module to refresh `set_typing` at its own ~4s cadence
/// without ever missing a beat.
pub(crate) const PROVIDER_ACTIVITY_TICK_INTERVAL_MS: u64 = 3_000;

/// Trait the runner uses to surface "the LLM call is still working"
/// signals so the host's typing-indicator path stays alive during a
/// slow provider stream. The default production implementation is a
/// thin wrapper around [`touch_heartbeat`] — the host's typing-ticker
/// is keyed off container liveness and pending-inbound rows, and
/// refreshing the heartbeat keeps the container marked Running so the
/// indicator never goes silent because the supervisor presumed the
/// runner was wedged.
///
/// Tests construct a counting mock and assert that long provider
/// streams produce many pings (one per ~3s of stream-time plus one
/// per useful chunk in the SSE pump).
pub trait ProviderActivityPinger: Send + Sync {
    /// Called once on each [`PROVIDER_ACTIVITY_TICK_INTERVAL_MS`] tick
    /// while a provider call is in flight, and once per useful chunk
    /// observed by [`pump_events`] (`Init` / `Usage` / `Progress` /
    /// `Activity` / `Result` / `ToolStart` / `ToolCall` / `ToolEnd`).
    /// Cheap operations only — this fires from a hot spawned-task loop
    /// and the stream-pump path.
    fn ping(&self);
}

/// Default production [`ProviderActivityPinger`]: refresh the
/// heartbeat file. The host watches the file's mtime to decide whether
/// the container is alive, and the typing-ticker only fires for
/// `container_status=Running` sessions — so a stale heartbeat would
/// otherwise let the indicator wink out during a long stream even
/// while the runner is happily consuming chunks.
pub struct HeartbeatPinger {
    pub(crate) path: Option<PathBuf>,
}

impl HeartbeatPinger {
    #[must_use]
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path }
    }
}

impl ProviderActivityPinger for HeartbeatPinger {
    fn ping(&self) {
        touch_heartbeat(self.path.as_ref());
    }
}

/// No-op pinger used by tests and any caller that doesn't want
/// provider-activity signals (e.g. an offline subagent run where no
/// human is watching for a typing indicator).
pub struct NoopPinger;

impl ProviderActivityPinger for NoopPinger {
    fn ping(&self) {}
}

/// RAII guard that calls [`ProviderActivityPinger::ping`] every
/// [`PROVIDER_ACTIVITY_TICK_INTERVAL_MS`] while alive.
///
/// Wrap the whole `query + pump_events` cycle (not just the initial
/// `query()` call — that's [`HeartbeatTicker`]'s job) so the
/// downstream typing-indicator path keeps getting "still working"
/// signals across long SSE streams. Dropped at the end of one
/// `run_llm_turn` attempt; backoff sleeps between attempts are short
/// enough (≤1s) that they don't need their own coverage.
pub(crate) struct ProviderActivityTicker {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl ProviderActivityTicker {
    /// Spawn a ticker. The first ping fires immediately so a
    /// sub-tick-interval stream still gets a signal; subsequent pings
    /// fire every [`PROVIDER_ACTIVITY_TICK_INTERVAL_MS`] until the
    /// guard is dropped.
    pub(crate) fn start(pinger: Arc<dyn ProviderActivityPinger>) -> Self {
        Self::start_with_interval(
            pinger,
            Duration::from_millis(PROVIDER_ACTIVITY_TICK_INTERVAL_MS),
        )
    }

    /// Same as [`start`], but the caller picks the tick interval.
    /// Test-only seam — production callers should use
    /// [`PROVIDER_ACTIVITY_TICK_INTERVAL_MS`] via [`start`].
    pub(crate) fn start_with_interval(
        pinger: Arc<dyn ProviderActivityPinger>,
        interval: Duration,
    ) -> Self {
        // Touch once up-front.
        pinger.ping();
        let handle = tokio::spawn(async move {
            let mut iv = tokio::time::interval(interval);
            // The first tick fires immediately; we already pinged before
            // spawning, so skip it to keep the cadence honest.
            iv.tick().await;
            loop {
                iv.tick().await;
                pinger.ping();
            }
        });
        Self { handle: Some(handle) }
    }
}

impl Drop for ProviderActivityTicker {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ironclaw_db::session::{open_inbound, open_outbound, SessionPaths};
    use ironclaw_db::tables::messages_in::{insert as insert_in, WriteInbound};
    use ironclaw_providers::{AgentProvider, AgentQuery, ProviderError, QueryInput};
    use ironclaw_types::{
        AgentGroupId, ChannelType, MessageId, MessageKind, ProviderEvent, SessionId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex;

    use crate::run::{run_loop, RunnerDeps};
    use crate::tools::RunnerToolCtx;

    /// Counting pinger: every call to [`ProviderActivityPinger::ping`]
    /// bumps an atomic so tests can assert on the total number of
    /// activity signals a turn produced.
    #[derive(Default)]
    struct CountingPinger {
        count: AtomicUsize,
    }

    impl CountingPinger {
        fn snapshot(&self) -> usize {
            self.count.load(Ordering::Relaxed)
        }
    }

    impl ProviderActivityPinger for CountingPinger {
        fn ping(&self) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Provider that yields a pre-baked sequence of events for each
    /// turn — slimmed copy of the one in `super::tests`, kept local
    /// so we don't depend on a sibling module's visibility.
    struct ScriptedProvider {
        scripts: StdMutex<Vec<Vec<ProviderEvent>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self { scripts: StdMutex::new(scripts) })
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
        ) -> std::result::Result<Box<dyn AgentQuery>, ProviderError> {
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
        async fn push(&mut self, _: String) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> std::result::Result<(), ProviderError> {
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

    /// Slimmed setup mirroring `super::tests::build_setup` but with a
    /// counting pinger so the typing-keepalive path can be observed.
    fn build_setup(scripts: Vec<Vec<ProviderEvent>>) -> (RunnerDeps, Arc<CountingPinger>, tempfile::TempDir, SessionPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = open_inbound(&paths).unwrap();
        let outbound = open_outbound(&paths).unwrap();
        let inbound = Arc::new(Mutex::new(inbound));
        let outbound = Arc::new(Mutex::new(outbound));
        let provider = ScriptedProvider::new(scripts);
        let tool_ctx: Arc<dyn ironclaw_mcp::ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(provider, tool_ctx, inbound, outbound, archive_dir);
        let pinger = Arc::new(CountingPinger::default());
        deps.activity_pinger = pinger.clone();
        deps.max_turns = Some(1);
        deps.idle_sleep = Duration::from_millis(1);
        (deps, pinger, tmp, paths)
    }

    fn insert_pending(inbound: &rusqlite::Connection, text: &str) -> MessageId {
        let id = MessageId::new();
        let msg = WriteInbound {
            id,
            kind: MessageKind::Chat,
            timestamp: chrono::Utc::now(),
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

    /// Each useful streamed chunk fires one activity ping plus the
    /// one-shot tick-zero ping at ticker startup. With 5 events plus
    /// one start-up tick we expect at least 6 pings before the stream
    /// finishes — proves the per-chunk path runs.
    #[tokio::test]
    async fn provider_stream_chunks_fire_activity_pings() {
        let (mut deps, pinger, _tmp, _paths) = build_setup(vec![vec![
            ProviderEvent::Init { continuation: "c1".into() },
            ProviderEvent::Activity,
            ProviderEvent::Progress { message: "thinking".into() },
            ProviderEvent::Usage { input_tokens: 5, output_tokens: 0 },
            ProviderEvent::Result { text: Some("hello".into()) },
        ]]);
        {
            let g = deps.inbound.lock().await;
            insert_pending(&g, "hi");
        }
        deps.max_turns = Some(1);
        run_loop(deps).await.unwrap();
        let n = pinger.snapshot();
        // 1 from ticker start + 5 from each event = 6 minimum. Allow >=6
        // because the background ticker may slip in extra ticks if the
        // test scheduler delays the stream.
        assert!(
            n >= 6,
            "expected at least 6 activity pings (1 start-up + 5 chunks), got {n}",
        );
    }

    /// Background ticker path: a long-lived provider call must
    /// accumulate at least one ping per tick interval. Uses the
    /// test-only `start_with_interval` seam to compress 3s ticks to
    /// 10ms so the test finishes in well under a second of real
    /// wall-clock time. The key signal is "ping count climbs
    /// monotonically while the ticker is alive, then stops after
    /// drop".
    #[tokio::test]
    async fn long_provider_stream_accumulates_periodic_pings() {
        let pinger = Arc::new(CountingPinger::default());
        let pinger_dyn: Arc<dyn ProviderActivityPinger> = pinger.clone();
        let interval = Duration::from_millis(10);
        let ticker = ProviderActivityTicker::start_with_interval(pinger_dyn, interval);
        // Start-up ping (== 1) lands synchronously inside `start`.
        assert!(
            pinger.snapshot() >= 1,
            "ticker must fire one ping on start, got {}",
            pinger.snapshot(),
        );

        // ~6 intervals worth of wall time -> expect >= 5 ticks past
        // the start-up ping. Generous floor of 4 absorbs any tokio-
        // scheduler jitter on a busy CI worker.
        tokio::time::sleep(interval * 6).await;
        let mid = pinger.snapshot();
        assert!(
            mid >= 4,
            "expected at least 4 pings after ~6 intervals, got {mid}",
        );

        drop(ticker);
        let after_drop = pinger.snapshot();
        // After drop the spawned task aborts; further sleeps must not
        // bump the counter. Allow one extra ping for an in-flight tick
        // that already raced past the abort point.
        tokio::time::sleep(interval * 5).await;
        let later = pinger.snapshot();
        assert!(
            later <= after_drop + 1,
            "ticker must stop pinging after drop: was {after_drop}, now {later}",
        );
    }

    /// Default `NoopPinger` swallows all pings — no panics, no
    /// observable side effects. Belt-and-braces for the "tests don't
    /// care about activity signals" path.
    #[test]
    fn noop_pinger_ping_does_nothing() {
        let p = NoopPinger;
        for _ in 0..1024 {
            p.ping();
        }
    }

    /// Regression for the silent-runner-crash on transient
    /// `container_state` write failures: a `ToolStart` / `ToolEnd`
    /// event whose downstream `set_current_tool` / `clear_current_tool`
    /// call errors must NOT abort the stream pump. Prior behaviour
    /// propagated the DbError up through `?`, which crashed the runner
    /// mid-stream — every subsequent event (including the final
    /// assistant `Result`) was discarded and the inbound never got an
    /// outbound reply. Simulated by dropping the `container_state`
    /// table before the run so every write returns "no such table".
    /// Pass condition: the assistant text from the trailing `Result`
    /// still lands in `messages_out`.
    #[tokio::test]
    async fn pump_completes_when_container_state_writes_fail() {
        let (mut deps, _pinger, _tmp, paths) = build_setup(vec![vec![
            ProviderEvent::Init { continuation: "c1".into() },
            ProviderEvent::ToolStart {
                name: "shell".into(),
                declared_timeout_ms: Some(5_000),
            },
            ProviderEvent::ToolEnd,
            ProviderEvent::Result {
                text: Some("survived the DB error".into()),
            },
        ]]);
        {
            let g = deps.inbound.lock().await;
            insert_pending(&g, "hi");
        }
        // Sabotage every set_current_tool / clear_current_tool write
        // by removing the table they target.
        {
            let g = deps.outbound.lock().await;
            g.execute("DROP TABLE container_state", []).unwrap();
        }
        deps.max_turns = Some(1);
        run_loop(deps).await.unwrap();

        // The pump survived: a Chat outbound row exists with the
        // post-ToolEnd assistant text.
        let outbound = open_outbound(&paths).unwrap();
        let rows = ironclaw_db::tables::messages_out::list_due(&outbound).unwrap();
        let chat = rows
            .iter()
            .find(|r| r.kind == MessageKind::Chat)
            .expect("expected the assistant Chat row to land despite container_state errors");
        assert_eq!(chat.content["text"], "survived the DB error");
    }
}
