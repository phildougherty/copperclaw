//! Provider-call layer: retry + deadline wrappers around `AgentProvider::query`,
//! the streamed-event pump, and the heartbeat ticker that keeps the host happy
//! during a slow provider attempt.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use copperclaw_providers::{AgentProvider, AgentQuery, DegradeReason, ProviderError, QueryInput};
use copperclaw_types::ProviderEvent;
use tokio::time::{sleep, timeout};

use super::drive_turn::{LlmTurnOutput, PendingToolCall, TurnOutcome};
use super::failover_health::FailoverHealth;
use super::hud::TaskHud;
use super::{RunnerDeps, clear_current_tool, emit_usage_report, set_current_tool};

/// One pre-built alternate provider for M18 R5 hot in-session failover.
/// Constructed once at runner startup from `runner.json`'s host-resolved
/// `failover_chain` (see [`crate::config::RunnerConfig::failover_chain`]).
/// The runner walks these in priority order in [`run_llm_turn`] when the
/// primary provider exhausts its in-provider retries mid-turn.
pub struct FailoverProvider {
    /// The alternate provider handle (already carrying its credential /
    /// endpoint — the host only ships entries the container can already
    /// reach, so building it needs no new secret).
    pub provider: Arc<dyn AgentProvider>,
    /// Provider-native model id to request from this entry.
    pub model: String,
    /// Canonical provider-kind name recorded on the `usage_report` for
    /// this entry and shown in the HUD "switched to <name>" note. Kept
    /// alongside the handle because a built provider's `.name()` can
    /// normalise differently (e.g. the ollama-shim reports `ollama`).
    pub provider_name: String,
}

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
    // Scrub key/token shapes out of the provider error body before it
    // reaches the usage_report (audit log) and the user-visible apology —
    // some gateways echo the offending `Authorization` value in a 401 body.
    let raw = crate::redact::redact_secrets(&format!(
        "provider rejected the query before streaming started ({err})"
    ));
    // Trim at a char boundary, append `…` if we truncated. Cheaper
    // than pulling unicode-segmentation; correct because `chars()`
    // returns codepoints.
    if raw.chars().count() <= FAILURE_REASON_CAP {
        return raw;
    }
    let prefix: String = raw
        .chars()
        .take(FAILURE_REASON_CAP.saturating_sub(1))
        .collect();
    format!("{prefix}…")
}

/// Make one LLM turn, with M18 R5 hot in-session provider failover.
///
/// The runner walks a candidate sequence — the primary
/// (`deps.provider` / `deps.model`) followed by each host-resolved healthy
/// fallback in `deps.failover_chain`, in priority order. Each candidate
/// gets the same two-layer in-provider retry (see
/// [`run_one_provider_attempt`]); on a terminal failure the runner records
/// that failure against the failing entry (so the host's health fold
/// degrades exactly it), notes the switch on the Task HUD, and retries the
/// SAME call against the next entry. Only when the WHOLE chain is exhausted
/// does it fall through with a failed [`LlmTurnOutput`] → the user-visible
/// apology. An empty `failover_chain` (the default) is exactly the
/// historical single-provider path — byte-identical behaviour.
///
/// `context_block` is the per-inbound "Conversation context" paragraph
/// (see `super::prompt::render_conversation_context`) appended to the
/// static `deps.system` for this single turn. `None` keeps the
/// historical behaviour (model sees only the static system prompt) so
/// callers and tests that don't care about channel context aren't
/// forced to populate the field. `hud` is the inbound's Task HUD, used
/// to surface the "switched to <provider>" note on a mid-turn failover.
///
/// M21 O3: `health` is the session's live [`FailoverHealth`]. The runner
/// re-consults it here, at every provider-call construction, to decide which
/// candidate to START on — a primary that died on an earlier call (this turn
/// or an earlier inbound) is skipped until its cooldown lapses, and a
/// recovered primary is restored — instead of blindly re-trying the primary
/// every call. Terminal failures/successes are recorded back into `health`
/// so the NEXT call routes correctly. For a single-provider chain
/// `select_start` is always 0 and no transition ever fires, so the
/// no-failover path is byte-identical.
pub(super) async fn run_llm_turn(
    deps: &RunnerDeps,
    history: &[copperclaw_providers::HistoryMessage],
    previous_continuation: Option<&str>,
    context_block: Option<&str>,
    hud: &TaskHud,
    health: &FailoverHealth,
) -> Result<LlmTurnOutput> {
    // Shrink the replayed transcript: stub stale, oversized tool-result
    // bodies (old file reads, command stdout, diffs) so they aren't
    // re-sent verbatim every turn. Recent results — including this turn's
    // — stay full, and the tool_use/tool_result pairing is preserved. The
    // persisted `state.history` is untouched (this is a send-time view).
    let history = crate::formatter::elide_stale_tool_results(history.to_vec(), &deps.elision);
    // Hand the static system prompt and the volatile per-inbound context to
    // the provider AS SEPARATE PARTS. The Anthropic provider places a cache
    // breakpoint after the stable `system` and emits the volatile
    // `system_context` only AFTER the transcript-tail breakpoint, so the
    // cached prefix stays byte-stable across inbounds (a caching HIT instead
    // of a per-turn MISS/rewrite). Non-caching providers flatten the two
    // back via `QueryInput::combined_system`, so their request bytes are
    // unchanged (same `\n\n` join the runner used to do inline here).
    let system_context = context_block.filter(|b| !b.is_empty()).map(str::to_string);

    // Candidate sequence: primary at index 0, then the fallback chain.
    let total_candidates = 1 + deps.failover_chain.len();
    // M21 O3: consult live chain health to decide which candidate this call
    // STARTS on. `select_start` returns 0 (the primary) for a healthy or
    // single-provider chain — byte-identical to the historical walk — but a
    // primary degraded by an earlier call is skipped here until its cooldown
    // lapses, and once it does the primary is selected again (restored). We
    // still walk forward through the remaining candidates on a mid-call
    // failure, exactly as before.
    let now = chrono::Utc::now();
    let start_idx = health.select_start(now).min(total_candidates - 1);
    for idx in start_idx..total_candidates {
        let (provider, model, provider_name): (&dyn AgentProvider, &str, &str) = if idx == 0 {
            (
                deps.provider.as_ref(),
                deps.model.as_str(),
                deps.provider.name(),
            )
        } else {
            let e = &deps.failover_chain[idx - 1];
            (
                e.provider.as_ref(),
                e.model.as_str(),
                e.provider_name.as_str(),
            )
        };

        // M21 O3: announce a provider transition — a mid-call failover OR a
        // cross-call switch that health moved the start point to — with the
        // EXISTING "switched to <provider>" HUD note and the failover metric.
        // `enter_candidate` returns None on the session's first candidate and
        // whenever the serving provider is unchanged, so a steady chain never
        // spams notes.
        // M21 O3 (M1 rider): gauge the currently-serving chain position on
        // every candidate entry (0 = primary), so it tracks reality even when
        // there is no transition to announce.
        copperclaw_metrics::set_provider_failover_active_position(idx);
        if let Some(t) = health.enter_candidate(idx) {
            hud.add_note(&format!("switched to {}", t.to));
            // Reuse the existing transition counter (M18 R5) AND the M21 O3
            // dedicated live-failover counter that distinguishes a *degrade*
            // (moving to a higher-index fallback) from a *restore* (moving
            // back toward the primary after re-probe), so operators can see
            // live failover activity separately from spawn-time selection.
            copperclaw_metrics::inc_provider_failover(&t.from, &t.to);
            let direction = if t.degrade {
                copperclaw_metrics::FAILOVER_DIRECTION_DEGRADE
            } else {
                copperclaw_metrics::FAILOVER_DIRECTION_RESTORE
            };
            copperclaw_metrics::inc_provider_failover_transition(direction, &t.from, &t.to);
        }

        let input = QueryInput {
            system: deps.system.clone(),
            system_context: system_context.clone(),
            model: model.to_string(),
            effort: deps.effort,
            previous_continuation: previous_continuation.map(str::to_string),
            history: history.clone(),
            tools: deps.tools.clone(),
            max_tokens: deps.max_tokens,
            temperature: deps.temperature,
            assistant_name: deps.assistant_name.clone(),
            display_name: None,
        };
        let turn_started_at = chrono::Utc::now();
        let attempt = run_one_provider_attempt(deps, provider, input).await?;

        // Per-call Prometheus metrics: observe only when the stream pump
        // ran (success or stream-error), matching the pre-R5 placement — a
        // query-time terminal failure observed nothing.
        if attempt.reached_stream {
            let elapsed_ms = (chrono::Utc::now() - turn_started_at)
                .num_milliseconds()
                .max(0);
            // i64 -> f64 loses precision above ~2^53 ms (~285 years);
            // acceptable for a call-duration measurement in seconds.
            #[allow(clippy::cast_precision_loss)]
            let elapsed_secs = elapsed_ms as f64 / 1000.0;
            copperclaw_metrics::observe_llm_call_seconds(elapsed_secs.max(0.0));
            if attempt.input_tokens > 0 {
                copperclaw_metrics::observe_llm_tokens_input(attempt.input_tokens);
            }
            if attempt.output_tokens > 0 {
                copperclaw_metrics::observe_llm_tokens_output(attempt.output_tokens);
            }
        }

        let is_last = idx + 1 == total_candidates;
        if attempt.out.failed {
            // Record the failure against THIS provider/model (not
            // deps.provider) so the host's degrade fold hits exactly the
            // entry that failed. `usage_fail_reason` preserves the
            // historical audit shapes (query-time = specific provider
            // error; stream-time = generic "stream ended" sentinel).
            let usage_reason = attempt
                .usage_fail_reason
                .clone()
                .unwrap_or_else(|| "provider stream ended with an error event".to_string());
            // M21 O3: degrade this candidate in live health so the NEXT
            // call's `select_start` routes around it for the cooldown window.
            // Classify with the SAME `DegradeReason::from_error_text` the host
            // uses on `agent_turns.error`, so only resilience-relevant
            // failures degrade the chain — a 4xx bad-request never does. When
            // the classifier returns None we still fail over within this call
            // (unchanged), we just don't record a cooldown.
            if let Some(reason) = DegradeReason::from_error_text(&usage_reason) {
                health.record_failure(idx, reason, turn_started_at);
            }
            emit_usage_report(
                deps,
                provider_name,
                model,
                attempt.input_tokens,
                attempt.output_tokens,
                turn_started_at,
                &TurnOutcome::Failed(usage_reason),
            )
            .await;

            if is_last {
                // Whole chain exhausted: surface the last entry's
                // user-visible failure reason (drive_turn maps it into the
                // apology). Byte-stable with the pre-R5 single-provider
                // path when `failover_chain` is empty.
                if !deps.failover_chain.is_empty() {
                    // R5: a real multi-entry failover chain was exhausted (a
                    // plain single-provider failure has an empty chain).
                    copperclaw_metrics::inc_provider_failover_chain_exhausted(provider_name);
                }
                return Ok(attempt.out);
            }

            // Fail over to the next candidate and retry the SAME call. The
            // "switched to <provider>" HUD note + failover metric are emitted
            // by `enter_candidate` at the top of the next iteration (M21 O3
            // unified both the mid-call and cross-call transition surfaces
            // there), so we only log here.
            let next = &deps.failover_chain[idx];
            tracing::warn!(
                failed_provider = provider_name,
                failed_model = model,
                next_provider = %next.provider_name,
                next_model = %next.model,
                "provider exhausted its retries; failing over to next healthy entry"
            );
            continue;
        }

        // Success against this candidate. Restore it to healthy in live
        // health (the "restore on recovery" half — a no-op when it was
        // already healthy) so a re-probed primary is preferred again.
        health.record_success(idx);
        // Surface the per-call token counts on the returned output so
        // `drive_turn` can accumulate them across the tool loop for the
        // per-task ceiling.
        let mut out = attempt.out;
        out.input_tokens = attempt.input_tokens;
        out.output_tokens = attempt.output_tokens;
        emit_usage_report(
            deps,
            provider_name,
            model,
            attempt.input_tokens,
            attempt.output_tokens,
            turn_started_at,
            &TurnOutcome::Done,
        )
        .await;
        return Ok(out);
    }

    // `total_candidates >= 1`, so the loop always returns above.
    unreachable!("failover candidate loop must return on the final entry")
}

/// Result of one full (query + stream-pump) attempt against a single
/// provider, after its own two-layer in-provider retry budget is spent.
struct AttemptResult {
    /// Accumulated turn output. `out.failed` distinguishes success from a
    /// terminal failure against this provider; `out.failure_reason` carries
    /// the user-visible apology text on failure.
    out: LlmTurnOutput,
    input_tokens: u32,
    output_tokens: u32,
    /// Reason to record on the `usage_report` when this attempt failed;
    /// `None` on success. Kept distinct from `out.failure_reason` to
    /// preserve the historical audit shapes (query-time = the specific
    /// provider error, stream-time = the generic sentinel).
    usage_fail_reason: Option<String>,
    /// True when the stream pump ran (success or stream-error); false when
    /// the provider rejected the query before streaming. Gates the per-call
    /// histogram observes to match the pre-R5 placement.
    reached_stream: bool,
}

/// Run the two-layer in-provider retry against ONE provider and return the
/// resulting [`AttemptResult`]. Does NOT emit metrics or a `usage_report` —
/// the caller ([`run_llm_turn`]) owns those so it can attribute them to the
/// entry that actually served (or failed) the turn.
///
/// Two layers of retry surround the stream, both capped at the same
/// `MAX_PROVIDER_ATTEMPTS` budget:
///
/// 1. `query_with_retry` retries the initial HTTP call when it fails
///    before the stream starts (connect / TLS / 5xx / timeout).
/// 2. The loop below retries the WHOLE (query + pump) pair when the stream
///    itself errors mid-way and the provider tagged the
///    `ProviderEvent::Error` as retryable (transient SSE-decode /
///    dropped-connection cases the initial query can't see behind an
///    HTTP 200).
async fn run_one_provider_attempt(
    deps: &RunnerDeps,
    provider: &dyn AgentProvider,
    input: QueryInput,
) -> Result<AttemptResult> {
    let mut stream_attempts: u32 = 0;
    let (out, input_tokens, output_tokens) = loop {
        stream_attempts += 1;
        // Keep the typing-indicator path (and the heartbeat-stale
        // supervisor) refreshed for the entire `query + pump_events`
        // cycle, not just the initial HTTP call. Without this, a 30s
        // silent stream would let the typing bubble fade out on channels
        // with a ~5s indicator window. Overridable via
        // RunnerDeps::activity_pinger so tests can count pings.
        let _activity = ProviderActivityTicker::start(Arc::clone(&deps.activity_pinger));
        let mut query = match query_with_retry(deps, provider, input.clone()).await {
            Ok(q) => q,
            Err(err) => {
                // All retries exhausted (or non-retryable) for THIS
                // provider. Return a terminal-failure attempt; the caller
                // decides whether to fail over or surface the apology. Do
                // NOT bubble — the runner must stay up. Redact the error
                // body (some gateways echo the offending key on a 4xx).
                tracing::error!(
                    error = %crate::redact::redact_secrets(&err.to_string()),
                    provider = provider.name(),
                    "provider query failed terminally"
                );
                // Include the underlying provider error in the reason so
                // the apology says WHY ("api error 400: prompt too long"
                // rather than a bare "rejected the query"), capped so a
                // giant 4xx body can't overflow it.
                let reason = format_provider_failure_reason(&err);
                let out = LlmTurnOutput {
                    failed: true,
                    failure_reason: reason.clone(),
                    ..LlmTurnOutput::default()
                };
                return Ok(AttemptResult {
                    out,
                    input_tokens: 0,
                    output_tokens: 0,
                    usage_fail_reason: Some(reason),
                    reached_stream: false,
                });
            }
        };

        let pumped = pump_events(deps, provider, query.as_mut()).await?;
        query.abort().await;

        // Retry only if the failure was tagged retryable AND we have
        // budget left, using the same backoff schedule as query_with_retry.
        if pumped.0.failed && pumped.0.retryable_failure && stream_attempts < MAX_PROVIDER_ATTEMPTS
        {
            tracing::warn!(
                attempt = stream_attempts,
                max = MAX_PROVIDER_ATTEMPTS,
                provider = provider.name(),
                "retryable stream failure; backing off and retrying"
            );
            copperclaw_metrics::inc_provider_retry(provider.name());
            backoff_for_attempt(stream_attempts).await;
            continue;
        }
        break pumped;
    };
    // Stream-time terminal failure records the generic sentinel (matching
    // the historical usage_report shape); `out.failure_reason` still
    // carries the spliced provider body for the apology.
    let usage_fail_reason = out
        .failed
        .then(|| "provider stream ended with an error event".to_string());
    Ok(AttemptResult {
        out,
        input_tokens,
        output_tokens,
        usage_fail_reason,
        reached_stream: true,
    })
}

/// Pump events off a live [`AgentQuery`] until the stream ends or
/// emits a terminal event ([`ProviderEvent::Result`] /
/// [`ProviderEvent::Error`]). Returns the accumulated turn output plus
/// the latest seen `(input_tokens, output_tokens)` counts.
#[allow(clippy::too_many_lines)]
pub(super) async fn pump_events(
    deps: &RunnerDeps,
    provider: &dyn AgentProvider,
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
                cache_read_tokens: cr,
                cache_creation_tokens: cc,
            } => {
                if it > 0 {
                    input_tokens = it;
                }
                if ot > 0 {
                    output_tokens = ot;
                }
                // Surface cache hits/writes for cost observability. A
                // non-zero `cr` means the prompt-caching prefix HIT this
                // turn (cached input billed at ~10% of the base rate);
                // `cc` is the premium-billed write that primes later hits.
                // Logged at debug so a single `RUST_LOG=…=debug` run can
                // confirm the breakpoints are landing without a schema
                // change.
                if cr > 0 || cc > 0 {
                    tracing::debug!(
                        cache_read_tokens = cr,
                        cache_creation_tokens = cc,
                        "prompt cache usage"
                    );
                }
            }
            ProviderEvent::Result { text } => {
                if let Some(t) = text {
                    out.text = t;
                }
                break;
            }
            ProviderEvent::Error { message, retryable } => {
                // Scrub key/token shapes out of the streamed error body
                // before it reaches stdout / the transcript / the apology —
                // the `message` is whatever the provider streamed back and
                // can carry an echoed `Authorization` value on some gateways.
                let message = crate::redact::redact_secrets(&message);
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
                    out.failure_reason =
                        format!("provider stream ended with an error event ({trimmed}{suffix})");
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
                // Authorization is enforced authoritatively in
                // `invoke_tool` (via `deps.policy.evaluate`), which
                // synthesises the model-facing refusal. We always push
                // the PendingToolCall here so the model sees a matching
                // `tool_result` on the next turn even when it's denied.
                // User-visible progress for the batch is surfaced by
                // the Task HUD in `drive_turn` (one self-editing status
                // message per inbound) once the batch executes — the
                // old per-tool breadcrumb chip emit that lived here was
                // removed with the M18 H1 HUD work.
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
                // R5 label fix: attribute the retry to the ACTIVE candidate
                // (which may be a failover entry), not the primary
                // `deps.provider`.
                copperclaw_metrics::inc_provider_retry(provider.name());
                out.tool_calls.push(PendingToolCall {
                    id: tool_use_id,
                    name: tool_name,
                    // Empty object, not Null: this tool_use is recorded into
                    // history and replayed every turn. A null `input`
                    // serializes as a tool call with null arguments, which
                    // strict OpenAI-compatible gateways (OpenRouter -> MiniMax)
                    // reject. The real (unparseable) args are surfaced to the
                    // model via the is_error tool_result instead.
                    input: serde_json::Value::Object(serde_json::Map::new()),
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
/// All retries fire a `copperclaw_provider_retry_total` counter so the
/// operator dashboard can spot flapping upstreams. Timeout-final fires
/// `copperclaw_provider_deadline_total`.
pub(super) async fn query_with_retry(
    deps: &RunnerDeps,
    provider: &dyn AgentProvider,
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
        let result = timeout(deps.provider_deadline, provider.query(input.clone())).await;

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
                    provider = provider.name(),
                    "provider query deadline exceeded"
                );
                if attempt >= MAX_PROVIDER_ATTEMPTS {
                    copperclaw_metrics::inc_provider_deadline(provider.name());
                    tracing::error!(
                        attempt,
                        max = MAX_PROVIDER_ATTEMPTS,
                        deadline_ms = u64_from_dur(deps.provider_deadline),
                        provider = provider.name(),
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
                copperclaw_metrics::inc_provider_retry(provider.name());
                backoff_for_attempt(attempt).await;
                continue;
            }
        };

        // We have a ProviderError. Decide whether to retry. Redact the
        // error body once up-front — some gateways echo the offending
        // key/`Authorization` value in a 4xx body, and `err` is logged at
        // every branch below.
        let redacted_err = crate::redact::redact_secrets(&err.to_string());
        if err.is_retryable() && attempt < MAX_PROVIDER_ATTEMPTS {
            tracing::warn!(
                attempt,
                max = MAX_PROVIDER_ATTEMPTS,
                provider = provider.name(),
                error = %redacted_err,
                "provider query failed; retrying after backoff"
            );
            copperclaw_metrics::inc_provider_retry(provider.name());
            backoff_for_attempt(attempt).await;
            continue;
        }

        // Terminal: either non-retryable, or we've exhausted attempts.
        if err.is_retryable() {
            tracing::error!(
                attempt,
                max = MAX_PROVIDER_ATTEMPTS,
                provider = provider.name(),
                error = %redacted_err,
                "provider query failed; retry budget exhausted"
            );
        } else {
            tracing::error!(
                attempt,
                provider = provider.name(),
                error = %redacted_err,
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
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(HEARTBEAT_TICK_INTERVAL_MS));
            // We already touched once; skip the immediate tick.
            interval.tick().await;
            loop {
                interval.tick().await;
                touch_heartbeat(Some(&path));
            }
        });
        Self {
            handle: Some(handle),
        }
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
        Self {
            handle: Some(handle),
        }
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
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_db::tables::messages_in::{WriteInbound, insert as insert_in};
    use copperclaw_providers::{AgentProvider, AgentQuery, ProviderError, QueryInput};
    use copperclaw_types::{
        AgentGroupId, ChannelType, MessageId, MessageKind, ProviderEvent, SessionId,
    };
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    use crate::run::hud::TaskHud;
    use crate::run::{RunnerDeps, run_loop};
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
    fn build_setup(
        scripts: Vec<Vec<ProviderEvent>>,
    ) -> (
        RunnerDeps,
        Arc<CountingPinger>,
        tempfile::TempDir,
        SessionPaths,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = open_inbound(&paths).unwrap();
        let outbound = open_outbound(&paths).unwrap();
        let inbound = Arc::new(Mutex::new(inbound));
        let outbound = Arc::new(Mutex::new(outbound));
        let provider = ScriptedProvider::new(scripts);
        let tool_ctx: Arc<dyn copperclaw_mcp::ToolContext> =
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
            ProviderEvent::Init {
                continuation: "c1".into(),
            },
            ProviderEvent::Activity,
            ProviderEvent::Progress {
                message: "thinking".into(),
            },
            ProviderEvent::Usage {
                input_tokens: 5,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            ProviderEvent::Result {
                text: Some("hello".into()),
            },
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
            ProviderEvent::Init {
                continuation: "c1".into(),
            },
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
        let rows = copperclaw_db::tables::messages_out::list_due(&outbound).unwrap();
        let chat = rows
            .iter()
            .find(|r| r.kind == MessageKind::Chat)
            .expect("expected the assistant Chat row to land despite container_state errors");
        assert_eq!(chat.content["text"], "survived the DB error");
    }

    // ---- M18 R5: hot in-session provider failover ---------------------------

    /// Provider whose `query()` always fails terminally (non-retryable, so
    /// the in-provider retry budget is spent on attempt 1 and the test is
    /// fast). Stands in for a gateway that's hiccuping mid-build.
    struct FailingProvider {
        name: &'static str,
    }

    #[async_trait]
    impl AgentProvider for FailingProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn query(
            &self,
            _input: QueryInput,
        ) -> std::result::Result<Box<dyn AgentQuery>, ProviderError> {
            Err(ProviderError::BadRequest("primary gateway is down".into()))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    /// Assemble a bare `RunnerDeps` (single-turn) with an explicit primary
    /// provider + model, plus the outbound path so tests can read the
    /// emitted `usage_report` rows.
    fn failover_deps(
        primary: Arc<dyn AgentProvider>,
        model: &str,
    ) -> (RunnerDeps, tempfile::TempDir, SessionPaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let tool_ctx: Arc<dyn copperclaw_mcp::ToolContext> =
            Arc::new(RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()));
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(primary, tool_ctx, inbound, outbound, archive_dir);
        deps.model = model.to_string();
        (deps, tmp, paths)
    }

    /// Read every `usage_report` system row as `(provider, model, status)`.
    fn usage_reports(paths: &SessionPaths) -> Vec<(String, String, String)> {
        let outbound = open_outbound(paths).unwrap();
        copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter_map(|r| {
                let u = r.content.get("usage_report")?;
                Some((
                    u.get("provider")?.as_str()?.to_string(),
                    u.get("model")?.as_str()?.to_string(),
                    u.get("status")?.as_str()?.to_string(),
                ))
            })
            .collect()
    }

    /// The core R5 acceptance: a mid-turn failure on provider A retries the
    /// SAME call against provider B and completes the turn — instead of
    /// failing the inbound with an apology. The HUD note fires on the
    /// switch, and each entry is reported back through its own
    /// `usage_report` (A error, B ok) so host-side health stays authoritative.
    #[tokio::test]
    async fn failover_switches_to_next_entry_and_completes() {
        let primary: Arc<dyn AgentProvider> = Arc::new(FailingProvider { name: "anthropic" });
        let (mut deps, _tmp, paths) = failover_deps(primary, "model-a");
        let fallback: Arc<dyn AgentProvider> =
            ScriptedProvider::new(vec![vec![ProviderEvent::Result {
                text: Some("built by the fallback".into()),
            }]]);
        deps.failover_chain = vec![FailoverProvider {
            provider: fallback,
            model: "model-b".into(),
            provider_name: "ollama".into(),
        }];
        let hud = TaskHud::new(&deps);
        let health = FailoverHealth::from_deps(&deps);

        let out = run_llm_turn(&deps, &[], None, None, &hud, &health)
            .await
            .unwrap();
        assert!(!out.failed, "the fallback served the turn, not a failure");
        assert_eq!(out.text, "built by the fallback");

        // The HUD note surfaced the switch for the user.
        assert_eq!(hud.note_for_test().as_deref(), Some("switched to ollama"));

        // Both entries reported back: primary errored, fallback ok.
        let reports = usage_reports(&paths);
        assert!(
            reports
                .iter()
                .any(|(p, m, s)| p == "anthropic" && m == "model-a" && s == "error"),
            "primary failure must be reported against its own entry: {reports:?}"
        );
        assert!(
            reports
                .iter()
                .any(|(p, m, s)| p == "ollama" && m == "model-b" && s == "ok"),
            "the serving fallback must be reported ok against its own entry: {reports:?}"
        );
    }

    /// Whole-chain exhaustion still produces the terminal failure (→ the
    /// user-visible apology in `drive_turn`): every candidate fails, so the
    /// runner surfaces the last entry's reason and fires NO switch note past
    /// the final entry.
    #[tokio::test]
    async fn whole_chain_exhaustion_still_fails() {
        let primary: Arc<dyn AgentProvider> = Arc::new(FailingProvider { name: "anthropic" });
        let (mut deps, _tmp, _paths) = failover_deps(primary, "model-a");
        deps.failover_chain = vec![FailoverProvider {
            provider: Arc::new(FailingProvider { name: "ollama" }),
            model: "model-b".into(),
            provider_name: "ollama".into(),
        }];
        let hud = TaskHud::new(&deps);
        let health = FailoverHealth::from_deps(&deps);

        let out = run_llm_turn(&deps, &[], None, None, &hud, &health)
            .await
            .unwrap();
        assert!(out.failed, "whole chain exhausted → terminal failure");
        assert!(
            out.failure_reason
                .contains("provider rejected the query before streaming started"),
            "apology carries the last entry's provider reason: {}",
            out.failure_reason
        );
    }

    /// Byte-stable single-provider path: an empty `failover_chain` is
    /// exactly the pre-R5 behaviour — a failing provider yields the same
    /// terminal-failure output, and NO switch note is set.
    #[tokio::test]
    async fn empty_chain_is_single_provider_behaviour() {
        let primary: Arc<dyn AgentProvider> = Arc::new(FailingProvider { name: "anthropic" });
        let (deps, _tmp, _paths) = failover_deps(primary, "model-a");
        assert!(deps.failover_chain.is_empty());
        let hud = TaskHud::new(&deps);
        let health = FailoverHealth::from_deps(&deps);

        let out = run_llm_turn(&deps, &[], None, None, &hud, &health)
            .await
            .unwrap();
        assert!(out.failed);
        assert!(
            hud.note_for_test().is_none(),
            "no failover note without a chain"
        );
    }

    /// A provider that fails its query with a resilience-relevant error
    /// (server 5xx) so `DegradeReason::from_error_text` classifies it and the
    /// live health machinery degrades the entry. Counts invocations so a test
    /// can prove a dead candidate was SKIPPED on a later call.
    struct CountingServerErrorProvider {
        name: &'static str,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl AgentProvider for CountingServerErrorProvider {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn query(
            &self,
            _input: QueryInput,
        ) -> std::result::Result<Box<dyn AgentQuery>, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(ProviderError::Api {
                status: 503,
                message: "upstream down".into(),
            })
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    /// M21 O3 acceptance: once the primary has died mid-session (a recorded
    /// resilience failure), the NEXT call skips it entirely and serves off the
    /// fallback — WITHOUT any container respawn. Proven by the primary's query
    /// never being invoked on the second call.
    #[tokio::test]
    async fn dead_primary_is_skipped_on_next_call() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let primary: Arc<dyn AgentProvider> = Arc::new(CountingServerErrorProvider {
            name: "anthropic",
            calls: calls.clone(),
        });
        let (mut deps, _tmp, _paths) = failover_deps(primary, "model-a");
        deps.failover_chain = vec![FailoverProvider {
            provider: ScriptedProvider::new(vec![vec![ProviderEvent::Result {
                text: Some("served by the fallback".into()),
            }]]),
            model: "model-b".into(),
            provider_name: "ollama".into(),
        }];
        let health = FailoverHealth::from_deps(&deps);

        // Pre-degrade the primary as if it died on an earlier call this
        // session (cooldown_until is 2 minutes in the future, so it is not
        // yet re-probe-eligible).
        health.record_failure(0, DegradeReason::ServerError, chrono::Utc::now());

        let hud = TaskHud::new(&deps);
        let out = run_llm_turn(&deps, &[], None, None, &hud, &health)
            .await
            .unwrap();

        assert!(!out.failed, "the healthy fallback served the turn");
        assert_eq!(out.text, "served by the fallback");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the dead primary must not be queried at all on the next call"
        );
    }

    /// M21 O3 acceptance: after the re-probe window elapses, the primary is
    /// restored — the call starts on it again and (on success) it is promoted
    /// back to healthy, with a "switched to" note announcing the restore.
    #[tokio::test]
    async fn recovered_primary_is_restored_after_reprobe_window() {
        let primary: Arc<dyn AgentProvider> =
            ScriptedProvider::new(vec![vec![ProviderEvent::Result {
                text: Some("primary is back".into()),
            }]]);
        let (mut deps, _tmp, _paths) = failover_deps(primary, "model-a");
        deps.failover_chain = vec![FailoverProvider {
            provider: Arc::new(FailingProvider { name: "ollama" }),
            model: "model-b".into(),
            provider_name: "ollama".into(),
        }];
        let health = FailoverHealth::from_deps(&deps);

        // Simulate the earlier degraded state: the session was serving the
        // fallback, and the primary's failure is now OLDER than the re-probe
        // window, so it is eligible again.
        assert_eq!(
            health.enter_candidate(1),
            None,
            "seed the last-served provider to the fallback"
        );
        health.record_failure(
            0,
            DegradeReason::ServerError,
            chrono::Utc::now() - chrono::Duration::minutes(3),
        );

        let hud = TaskHud::new(&deps);
        let out = run_llm_turn(&deps, &[], None, None, &hud, &health)
            .await
            .unwrap();

        assert!(!out.failed, "the restored primary served the turn");
        assert_eq!(out.text, "primary is back");
        // The primary here is a `ScriptedProvider` (name "scripted"); the
        // restore is announced with the existing switch note.
        assert_eq!(hud.note_for_test().as_deref(), Some("switched to scripted"),);
    }
}
