//! Prometheus metrics for the copperclaw host.
//!
//! This crate provides:
//!
//! - Named metric helpers (counters and histograms) so callers never
//!   hard-code metric name strings.
//! - An optional HTTP `/metrics` endpoint, started only when
//!   `COPPERCLAW_METRICS_ADDR` is set in the process environment.  The
//!   endpoint binds `127.0.0.1` when the operator supplies only a port
//!   number or an address without an explicit host, keeping the
//!   "secure-by-default" tenet.
//! - A bind-failure policy of warn-and-continue: a misconfigured address
//!   writes a `tracing::warn!` but does not kill the host.
//!
//! ## Usage
//!
//! ```rust,no_run
//! # #[tokio::main]
//! # async fn main() {
//! // In boot.rs, after reading the environment:
//! copperclaw_metrics::maybe_start_server(None).await;
//!
//! // At a call site that routes a message:
//! copperclaw_metrics::inc_messages_inbound("cli");
//! # }
//! ```
//!
//! ## Metric names (all prefixed `copperclaw_`)
//!
//! | Kind      | Name                              | Labels         |
//! |-----------|-----------------------------------|----------------|
//! | Counter   | `copperclaw_messages_inbound_total`  | `channel_type` |
//! | Counter   | `copperclaw_messages_outbound_total` | `channel_type` |
//! | Counter   | `copperclaw_containers_spawned_total`| —              |
//! | Counter   | `copperclaw_containers_crashed_total`| —              |
//! | Counter   | `copperclaw_delivery_failed_total`   | `channel_type` |
//! | Counter   | `copperclaw_delivery_formatting_fallback_total` | `channel_type` |
//! | Counter   | `copperclaw_self_mod_failed_total`   | `action`       |
//! | Counter   | `copperclaw_self_mod_succeeded_total`| `action`       |
//! | Counter   | `copperclaw_budget_exhausted_total`  | `agent_group_id`, `gate` |
//! | Counter   | `copperclaw_budget_exhausted_replies_total` | `agent_group_id` |
//! | Counter   | `copperclaw_budget_exhausted_suppressed_total` | `agent_group_id` |
//! | Counter   | `copperclaw_task_budget_exhausted_total` | `agent_group_id` |
//! | Histogram | `copperclaw_llm_call_seconds`        | —              |
//! | Histogram | `copperclaw_llm_tokens_input`        | —              |
//! | Histogram | `copperclaw_llm_tokens_output`       | —              |
//! | Histogram | `copperclaw_container_spawn_seconds` | —              |
//! | Counter   | `copperclaw_provider_deadline_total` | `provider`     |
//! | Counter   | `copperclaw_provider_retry_total`    | `provider`     |
//! | Counter   | `copperclaw_stuck_inbound_apology_total` | `agent_group_id`, `reason` |
//! | Counter   | `copperclaw_tool_loop_breaker_total` | `agent_group_id`, `pattern` |
//! | Counter   | `copperclaw_broker_requests_total`   | `agent_group_id`, `outcome` |
//! | Counter   | `copperclaw_broker_egress_bytes_total` | `agent_group_id` |
//! | Gauge     | `copperclaw_degraded_state`          | `reason`       |

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

// ── Metric name constants ──────────────────────────────────────────────────

pub const MESSAGES_INBOUND_TOTAL: &str = "copperclaw_messages_inbound_total";
pub const MESSAGES_OUTBOUND_TOTAL: &str = "copperclaw_messages_outbound_total";
pub const CONTAINERS_SPAWNED_TOTAL: &str = "copperclaw_containers_spawned_total";
pub const CONTAINERS_CRASHED_TOTAL: &str = "copperclaw_containers_crashed_total";
pub const IMAGE_REBUILD_FAILED_TOTAL: &str = "copperclaw_image_rebuild_failed_total";
pub const SECRETS_ROTATED_TOTAL: &str = "copperclaw_secrets_rotated_total";
pub const DELIVERY_FAILED_TOTAL: &str = "copperclaw_delivery_failed_total";
pub const DELIVERY_FORMATTING_FALLBACK_TOTAL: &str =
    "copperclaw_delivery_formatting_fallback_total";
pub const DELIVERY_CHAT_SPLIT_TOTAL: &str = "copperclaw_delivery_chat_split_total";
pub const SELF_MOD_FAILED_TOTAL: &str = "copperclaw_self_mod_failed_total";
pub const SELF_MOD_SUCCEEDED_TOTAL: &str = "copperclaw_self_mod_succeeded_total";
pub const BUDGET_EXHAUSTED_TOTAL: &str = "copperclaw_budget_exhausted_total";
pub const BUDGET_EXHAUSTED_REPLIES_TOTAL: &str = "copperclaw_budget_exhausted_replies_total";
pub const BUDGET_EXHAUSTED_SUPPRESSED_TOTAL: &str = "copperclaw_budget_exhausted_suppressed_total";
pub const TASK_BUDGET_EXHAUSTED_TOTAL: &str = "copperclaw_task_budget_exhausted_total";
pub const LLM_CALL_SECONDS: &str = "copperclaw_llm_call_seconds";
pub const LLM_TOKENS_INPUT: &str = "copperclaw_llm_tokens_input";
pub const LLM_TOKENS_OUTPUT: &str = "copperclaw_llm_tokens_output";
pub const CONTAINER_SPAWN_SECONDS: &str = "copperclaw_container_spawn_seconds";
pub const PROVIDER_DEADLINE_TOTAL: &str = "copperclaw_provider_deadline_total";
pub const PROVIDER_RETRY_TOTAL: &str = "copperclaw_provider_retry_total";
pub const STUCK_INBOUND_APOLOGY_TOTAL: &str = "copperclaw_stuck_inbound_apology_total";
pub const TOOL_LOOP_BREAKER_TOTAL: &str = "copperclaw_tool_loop_breaker_total";
pub const DEGRADED_STATE: &str = "copperclaw_degraded_state";

// ── Reason label values for the `reason` label of
// `copperclaw_stuck_inbound_apology_total`. Use these constants instead of
// stringly-typed literals at call sites so a typo is a compile error.

/// The inbound message sat in `messages_in.status='pending'` longer
/// than the apology threshold (default 5 min) without progress.
pub const STUCK_REASON_PENDING_TOO_LONG: &str = "pending_too_long";

/// The session's `container_status='stopped'` with a pending inbound
/// and the container manager has exhausted its spawn-retry budget.
pub const STUCK_REASON_CONTAINER_SPAWN_FAILED: &str = "container_spawn_failed";

// ── Pattern label values for the `pattern` label of
// `copperclaw_tool_loop_breaker_total`. Use these constants instead of
// stringly-typed literals at call sites so a typo is a compile error.

/// The runner observed N consecutive tool calls with the same tool name
/// AND identical arguments — the model is stuck re-issuing one call.
pub const LOOP_PATTERN_IDENTICAL: &str = "identical";

/// The runner observed an A,B,A,B alternation between two distinct tool
/// calls — the model is oscillating between two states without progress.
pub const LOOP_PATTERN_PING_PONG: &str = "ping_pong";

// ── Degraded-state reason label values for `copperclaw_degraded_state`.
// Use these constants instead of stringly-typed literals at call sites
// so a typo is a compile error.
pub const DEGRADED_REASON_IMAGE_NOT_FOUND: &str = "image_not_found";
pub const DEGRADED_REASON_RUNNER_BINARY_MISSING: &str = "runner_binary_missing";
pub const DEGRADED_REASON_RUNNER_BINARY_NOT_EXECUTABLE: &str = "runner_binary_not_executable";
pub const DEGRADED_REASON_HEALTH_CHECK_TIMEOUT: &str = "health_check_timeout";
pub const DEGRADED_REASON_HEALTH_CHECK_FAILED: &str = "health_check_failed";

// ── Budget-gate label values for the `gate` label of
// `copperclaw_budget_exhausted_total`. Use these constants instead of
// stringly-typed literals at call sites so a typo is a compile error.
pub const BUDGET_GATE_DAILY_TOKENS: &str = "daily_tokens";
pub const BUDGET_GATE_TURNS_PER_MINUTE: &str = "turns_per_minute";
pub const BUDGET_GATE_TURNS_PER_HOUR: &str = "turns_per_hour";

// ── Credential-broker metrics (Phase 0b). The host-side model proxy that
// holds the real provider key and forwards model calls per session, so the
// long-lived key never enters the container. See
// `copperclaw-host/src/container_manager/broker.rs`.
pub const BROKER_REQUESTS_TOTAL: &str = "copperclaw_broker_requests_total";
pub const BROKER_EGRESS_BYTES_TOTAL: &str = "copperclaw_broker_egress_bytes_total";

// ── Outcome label values for the `outcome` label of
// `copperclaw_broker_requests_total`. Use these constants instead of
// stringly-typed literals at call sites so a typo is a compile error.
/// Token validated, budget clear: the request was forwarded upstream.
pub const BROKER_OUTCOME_FORWARDED: &str = "forwarded";
/// Token failed validation (bad signature, malformed, expired, revoked).
pub const BROKER_OUTCOME_UNAUTHORIZED: &str = "unauthorized";
/// Token valid but the group is over its budget — request refused.
pub const BROKER_OUTCOME_OVER_BUDGET: &str = "over_budget";

// ── Counter helpers ────────────────────────────────────────────────────────

/// Increment `copperclaw_messages_inbound_total{channel_type=<ct>}`.
pub fn inc_messages_inbound(channel_type: &str) {
    counter!(MESSAGES_INBOUND_TOTAL, "channel_type" => channel_type.to_owned()).increment(1);
}

/// Increment `copperclaw_messages_outbound_total{channel_type=<ct>}`.
pub fn inc_messages_outbound(channel_type: &str) {
    counter!(MESSAGES_OUTBOUND_TOTAL, "channel_type" => channel_type.to_owned()).increment(1);
}

/// Increment `copperclaw_containers_spawned_total`.
pub fn inc_containers_spawned() {
    counter!(CONTAINERS_SPAWNED_TOTAL).increment(1);
}

/// Increment `copperclaw_containers_crashed_total`.
pub fn inc_containers_crashed() {
    counter!(CONTAINERS_CRASHED_TOTAL).increment(1);
}

/// Increment `copperclaw_image_rebuild_failed_total`. Fired by the
/// container manager when an image rebuild call to the runtime errors
/// out; the manager falls back to the last-known-good `image_tag`
/// when one exists so the agent group is not blocked.
pub fn inc_image_rebuild_failed() {
    counter!(IMAGE_REBUILD_FAILED_TOTAL).increment(1);
}

/// Increment `copperclaw_secrets_rotated_total`. Fired by the host's
/// SIGHUP handler each time it re-reads the install's `.env` to
/// pick up rotated provider keys. Incremented even when no values
/// changed — the metric measures rotation *attempts*, not deltas.
pub fn inc_secrets_rotated() {
    counter!(SECRETS_ROTATED_TOTAL).increment(1);
}

/// Increment `copperclaw_broker_requests_total{agent_group_id, outcome}`.
/// Fired by the credential broker for every model request it handles.
/// `outcome` should be one of [`BROKER_OUTCOME_FORWARDED`],
/// [`BROKER_OUTCOME_UNAUTHORIZED`], or [`BROKER_OUTCOME_OVER_BUDGET`].
/// The `agent_group_id` is `"unknown"` when the token failed to validate
/// (so the request could not be attributed to a group).
pub fn inc_broker_request(agent_group_id: &str, outcome: &str) {
    counter!(
        BROKER_REQUESTS_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Add `bytes` to `copperclaw_broker_egress_bytes_total{agent_group_id}`.
/// Meters the volume of request bodies the broker forwarded upstream on
/// behalf of a group, so an operator can see egress volume per agent group
/// alongside the token-based budget.
pub fn add_broker_egress_bytes(agent_group_id: &str, bytes: u64) {
    counter!(
        BROKER_EGRESS_BYTES_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
    )
    .increment(bytes);
}

/// Increment `copperclaw_delivery_failed_total{channel_type=<ct>}`.
pub fn inc_delivery_failed(channel_type: &str) {
    counter!(DELIVERY_FAILED_TOTAL, "channel_type" => channel_type.to_owned()).increment(1);
}

/// Increment `copperclaw_self_mod_failed_total{action=<action>}`. Fired by
/// the delivery loop when a self-modifying system action
/// (`install_packages` / `add_mcp_server`) fails to apply to
/// `container_configs`. Pairs with [`inc_self_mod_succeeded`].
pub fn inc_self_mod_failed(action: &str) {
    counter!(SELF_MOD_FAILED_TOTAL, "action" => action.to_owned()).increment(1);
}

/// Increment `copperclaw_self_mod_succeeded_total{action=<action>}`. Fired by
/// the delivery loop when a self-modifying system action
/// (`install_packages` / `add_mcp_server`) successfully applies to
/// `container_configs`. Pairs with [`inc_self_mod_failed`].
pub fn inc_self_mod_succeeded(action: &str) {
    counter!(SELF_MOD_SUCCEEDED_TOTAL, "action" => action.to_owned()).increment(1);
}

/// Increment `copperclaw_delivery_formatting_fallback_total{channel_type=<ct>}`.
/// Fired by the delivery loop when an adapter rejected a delivery with a
/// formatting-related `BadRequest` (e.g. Telegram's "can't parse entities")
/// AND the channel's `plain_text_fallback` retry then succeeded. Measures
/// downgraded deliveries — the user got the message but in a less rich shape.
pub fn inc_delivery_formatting_fallback(channel_type: &str) {
    counter!(
        DELIVERY_FORMATTING_FALLBACK_TOTAL,
        "channel_type" => channel_type.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_delivery_chat_split_total{channel_type=<ct>}`. Fired
/// by the delivery loop when an outbound chat row's text exceeded the
/// adapter's `max_message_chars()` cap and was split into multiple parts
/// before send. One increment per split row (not per resulting part).
pub fn inc_delivery_chat_split(channel_type: &str) {
    counter!(
        DELIVERY_CHAT_SPLIT_TOTAL,
        "channel_type" => channel_type.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_provider_deadline_total{provider=<p>}`. Fired by
/// the runner each time the per-LLM-call deadline trips AND all retries
/// have been exhausted — i.e. the inbound is about to be marked failed.
pub fn inc_provider_deadline(provider: &str) {
    counter!(PROVIDER_DEADLINE_TOTAL, "provider" => provider.to_owned()).increment(1);
}

/// Increment `copperclaw_provider_retry_total{provider=<p>}`. Fired by the
/// runner each time it backs off and re-issues a `provider.query()`
/// call after a retryable failure (5xx, transport, overloaded, or
/// per-call timeout).
pub fn inc_provider_retry(provider: &str) {
    counter!(PROVIDER_RETRY_TOTAL, "provider" => provider.to_owned()).increment(1);
}

/// Increment `copperclaw_budget_exhausted_total{agent_group_id, gate}`. Fired by
/// the container manager every time a budget gate refuses to spawn — once
/// per refusal regardless of whether the in-channel reply is then deduped.
/// `gate` should be one of [`BUDGET_GATE_DAILY_TOKENS`],
/// [`BUDGET_GATE_TURNS_PER_MINUTE`], or [`BUDGET_GATE_TURNS_PER_HOUR`].
pub fn inc_budget_exhausted(agent_group_id: &str, gate: &str) {
    counter!(
        BUDGET_EXHAUSTED_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
        "gate" => gate.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_budget_exhausted_replies_total{agent_group_id}`.
/// Fired by the container manager when a budget-exhausted (or rate-limit)
/// reply is *actually* written to outbound — i.e. AFTER the dedup window
/// check. Pairs with [`inc_budget_exhausted_suppressed`] which fires when
/// the reply is suppressed by the dedup window instead.
pub fn inc_budget_exhausted_reply(agent_group_id: &str) {
    counter!(
        BUDGET_EXHAUSTED_REPLIES_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_budget_exhausted_suppressed_total{agent_group_id}`.
/// Fired by the container manager when a refusal is detected but the
/// per-group dedup window suppresses the in-channel reply.
pub fn inc_budget_exhausted_suppressed(agent_group_id: &str) {
    counter!(
        BUDGET_EXHAUSTED_SUPPRESSED_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_task_budget_exhausted_total{agent_group_id}`. Fired by
/// the runner's tool loop ([`drive_turn`]) when the cumulative input+output
/// tokens spent on a SINGLE inbound's tool-loop crosses the configured
/// per-task ceiling (`COPPERCLAW_MAX_TASK_TOKENS`). This is distinct from the
/// per-DAY group cap surfaced via [`inc_budget_exhausted`] with
/// [`BUDGET_GATE_DAILY_TOKENS`]: the daily gate refuses to *spawn*, this one
/// hard-aborts a runaway *mid-loop* so a single confused task cannot blow the
/// token bill even when it stays inside `COPPERCLAW_MAX_TOOL_TURNS`.
///
/// [`drive_turn`]: https://docs.rs/copperclaw-runner
pub fn inc_task_budget_exhausted(agent_group_id: &str) {
    counter!(
        TASK_BUDGET_EXHAUSTED_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_stuck_inbound_apology_total{agent_group_id, reason}`.
/// Fired by the host sweep loop each time it emits a user-visible apology
/// for an inbound that's been sitting in `pending` longer than the apology
/// threshold (or whose session can't even spawn a container).
///
/// `reason` should be one of [`STUCK_REASON_PENDING_TOO_LONG`] or
/// [`STUCK_REASON_CONTAINER_SPAWN_FAILED`]. Pairs with the inbound's
/// `tries=99` dedupe marker — exactly one apology per stuck inbound, so
/// this counter is also the per-stuck-message-emit count.
pub fn inc_stuck_inbound_apology(agent_group_id: &str, reason: &str) {
    counter!(
        STUCK_INBOUND_APOLOGY_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_tool_loop_breaker_total{agent_group_id, pattern}`.
/// Fired by the runner's `drive_turn` circuit breaker when it detects a
/// content-level tool-call loop (the model re-issuing the same call, or
/// oscillating between two) and terminates the inbound to stop the spin.
/// Complements the consecutive-turn depth cap and the token budget — those
/// bound *how long* a loop runs; this bounds a loop that never advances.
///
/// `pattern` should be one of [`LOOP_PATTERN_IDENTICAL`] or
/// [`LOOP_PATTERN_PING_PONG`].
pub fn inc_tool_loop_breaker(agent_group_id: &str, pattern: &str) {
    counter!(
        TOOL_LOOP_BREAKER_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
        "pattern" => pattern.to_owned(),
    )
    .increment(1);
}

// ── Histogram helpers ──────────────────────────────────────────────────────

/// Record one LLM call duration (seconds).
pub fn observe_llm_call_seconds(secs: f64) {
    histogram!(LLM_CALL_SECONDS).record(secs);
}

/// Record input token count for one LLM call.
pub fn observe_llm_tokens_input(tokens: u32) {
    histogram!(LLM_TOKENS_INPUT).record(f64::from(tokens));
}

/// Record output token count for one LLM call.
pub fn observe_llm_tokens_output(tokens: u32) {
    histogram!(LLM_TOKENS_OUTPUT).record(f64::from(tokens));
}

/// Record container spawn duration (seconds).
pub fn observe_container_spawn_seconds(secs: f64) {
    histogram!(CONTAINER_SPAWN_SECONDS).record(secs);
}

// ── Gauge helpers ──────────────────────────────────────────────────────────

/// Set `copperclaw_degraded_state{reason=<reason>}` to 1. Fired by the
/// boot-time image health check when it detects the session image is
/// missing or stale; the host continues to run (so the admin socket
/// stays reachable) but the container manager refuses to spawn new
/// sessions until the operator runs `./rebuild.sh` to refresh the
/// image and restart the host.
pub fn set_degraded_state(reason: &str) {
    gauge!(DEGRADED_STATE, "reason" => reason.to_owned()).set(1.0);
}

/// Clear `copperclaw_degraded_state{reason=<reason>}` (i.e. set to 0).
/// Used by tests; production code does not transition out of degraded
/// without a host restart, so this is mostly for symmetry / hygiene.
pub fn clear_degraded_state(reason: &str) {
    gauge!(DEGRADED_STATE, "reason" => reason.to_owned()).set(0.0);
}

// ════════════════════════════════════════════════════════════════════════════
// M18 metrics rider (card M1) — one sweep of the metric "wishes" recorded in the
// merged M18 PRs (#24-#54). Names/labels mirror each PR's wish verbatim; the
// emission call sites live in the crate each wish named. Grouped by source card.
// ════════════════════════════════════════════════════════════════════════════

// ── C1 (#24) — Slack typing / HUD-capability decisions ─────────────────────
pub const SLACK_TYPING_SKIPPED_TOTAL: &str = "copperclaw_slack_typing_skipped_total";
pub const SLACK_TYPING_SET_STATUS_TOTAL: &str = "copperclaw_slack_typing_set_status_total";
pub const SLACK_HUD_DECISION_TOTAL: &str = "copperclaw_slack_hud_decision_total";

/// Increment `copperclaw_slack_typing_skipped_total{reason}` — the slack adapter
/// declined to set a typing indicator (e.g. `reason="non_assistant_surface"`).
pub fn inc_slack_typing_skipped(reason: &str) {
    counter!(SLACK_TYPING_SKIPPED_TOTAL, "reason" => reason.to_owned()).increment(1);
}

/// Increment `copperclaw_slack_typing_set_status_total{result}` — outcome of a
/// slack assistant set-status ("is typing…") call (`ok|bad_request|error`).
pub fn inc_slack_typing_set_status(result: &str) {
    counter!(SLACK_TYPING_SET_STATUS_TOTAL, "result" => result.to_owned()).increment(1);
}

/// Increment `copperclaw_slack_hud_decision_total{typing_indicator_visible}` —
/// how the HUD typing-capability predicate resolved for a surface.
pub fn inc_slack_hud_decision(typing_indicator_visible: bool) {
    counter!(
        SLACK_HUD_DECISION_TOTAL,
        "typing_indicator_visible" => if typing_indicator_visible { "true" } else { "false" },
    )
    .increment(1);
}

// ── T1 (#25) — shell truncation / read_file windowing ──────────────────────
pub const SHELL_TRUNCATED_TOTAL: &str = "copperclaw_shell_truncated_total";
pub const SHELL_TRUNCATED_BYTES: &str = "copperclaw_shell_truncated_bytes";
pub const READ_FILE_LINES_MODE_TOTAL: &str = "copperclaw_read_file_lines_mode_total";
pub const READ_FILE_PAGES: &str = "copperclaw_read_file_pages";

/// Increment `copperclaw_shell_truncated_total{mode}` — a shell tool stream was
/// capped; `mode` is `head` or `tail`.
pub fn inc_shell_truncated(mode: &str) {
    counter!(SHELL_TRUNCATED_TOTAL, "mode" => mode.to_owned()).increment(1);
}

/// Record `copperclaw_shell_truncated_bytes` — the pre-cap byte size of a shell
/// stream that was truncated (tunes the shell output cap).
pub fn observe_shell_truncated_bytes(bytes: usize) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(SHELL_TRUNCATED_BYTES).record(bytes as f64);
}

/// Increment `copperclaw_read_file_lines_mode_total` — a `read_file` call used
/// lines mode (vs bytes mode).
pub fn inc_read_file_lines_mode() {
    counter!(READ_FILE_LINES_MODE_TOTAL).increment(1);
}

/// Record `copperclaw_read_file_pages` — how many windowed reads it would take
/// to read the whole file at the requested line limit (pages-per-file).
pub fn observe_read_file_pages(pages: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(READ_FILE_PAGES).record(pages as f64);
}

// ── P1 (#26) — spawn tool-profile / skill loading ──────────────────────────
pub const SESSIONS_SPAWNED_PROFILE_TOTAL: &str = "copperclaw_sessions_spawned_profile_total";
pub const SYSTEM_PROMPT_BYTES: &str = "copperclaw_system_prompt_bytes";
pub const LOAD_SKILL_TOTAL: &str = "copperclaw_load_skill_total";

/// Increment `copperclaw_sessions_spawned_profile_total{profile}` — one session
/// spawn, attributed to its resolved tool profile.
pub fn inc_session_spawned_profile(profile: &str) {
    counter!(SESSIONS_SPAWNED_PROFILE_TOTAL, "profile" => profile.to_owned()).increment(1);
}

/// Record `copperclaw_system_prompt_bytes{profile}` — assembled system-prompt
/// byte size for a spawn, labelled by tool profile.
pub fn observe_system_prompt_bytes(profile: &str, bytes: usize) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(SYSTEM_PROMPT_BYTES, "profile" => profile.to_owned()).record(bytes as f64);
}

/// Increment `copperclaw_load_skill_total{skill, mode}` — a `load_skill` call;
/// `mode` is `inline` (host in inline-skills mode) or `callable` (catalogue).
pub fn inc_load_skill(skill: &str, mode: &str) {
    counter!(
        LOAD_SKILL_TOTAL,
        "skill" => skill.to_owned(),
        "mode" => mode.to_owned(),
    )
    .increment(1);
}

// ── R0 (#27) — policy denials / unknown tools ──────────────────────────────
pub const POLICY_DENIED_TOTAL: &str = "copperclaw_policy_denied_total";
pub const UNKNOWN_TOOL_TOTAL: &str = "copperclaw_unknown_tool_total";

/// Increment `copperclaw_policy_denied_total{layer, tool}` — a tool call refused
/// by a policy layer (`role|skill|profile|provenance|deny_list|allow_list`).
pub fn inc_policy_denied(layer: &str, tool: &str) {
    counter!(
        POLICY_DENIED_TOTAL,
        "layer" => layer.to_owned(),
        "tool" => tool.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_unknown_tool_total{tool}` — the MCP dispatch table had
/// no entry for the requested tool name.
pub fn inc_unknown_tool(tool: &str) {
    counter!(UNKNOWN_TOOL_TOTAL, "tool" => tool.to_owned()).increment(1);
}

// ── C2 (#28) / C5b (#53) — fence-aware chunk splitting ─────────────────────
pub const DELIVERY_FENCE_SPLIT_TOTAL: &str = "copperclaw_delivery_fence_split_total";
pub const DELIVERY_FENCE_UNBALANCED_INPUT_TOTAL: &str =
    "copperclaw_delivery_fence_unbalanced_input_total";

/// Increment `copperclaw_delivery_fence_split_total{channel_type, kind}` — the
/// markdown chunk splitter closed and reopened a code fence across a boundary;
/// `kind` is the fence kind (`backtick|pre`).
pub fn inc_delivery_fence_split(channel_type: &str, kind: &str) {
    counter!(
        DELIVERY_FENCE_SPLIT_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "kind" => kind.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_delivery_fence_unbalanced_input_total{channel_type}` —
/// the splitter saw an unbalanced (never-closed) fence in its input.
pub fn inc_delivery_fence_unbalanced_input(channel_type: &str) {
    counter!(
        DELIVERY_FENCE_UNBALANCED_INPUT_TOTAL,
        "channel_type" => channel_type.to_owned(),
    )
    .increment(1);
}

// ── C5b (#53) — shared markdown renderer ───────────────────────────────────
pub const MARKDOWN_RENDER_TOTAL: &str = "copperclaw_markdown_render_total";
pub const MARKDOWN_UNBALANCED_MARKER_TOTAL: &str = "copperclaw_markdown_unbalanced_marker_total";

/// Increment `copperclaw_markdown_render_total{flavor}` — one render through the
/// shared per-platform markdown renderer.
pub fn inc_markdown_render(flavor: &str) {
    counter!(MARKDOWN_RENDER_TOTAL, "flavor" => flavor.to_owned()).increment(1);
}

/// Increment `copperclaw_markdown_unbalanced_marker_total{flavor}` — the render
/// hit the forgiving path for an unbalanced inline marker (emitted literally).
pub fn inc_markdown_unbalanced_marker(flavor: &str) {
    counter!(MARKDOWN_UNBALANCED_MARKER_TOTAL, "flavor" => flavor.to_owned()).increment(1);
}

// ── R1 (#29) — slash commands / control rows / status timing ───────────────
pub const SLASH_COMMANDS_TOTAL: &str = "copperclaw_slash_commands_total";
pub const CONTROL_ROWS_WRITTEN_TOTAL: &str = "copperclaw_control_rows_written_total";
pub const STATUS_ANSWER_SECONDS: &str = "copperclaw_status_answer_seconds";

/// Increment `copperclaw_slash_commands_total{command, channel_type}` — a slash
/// command (`stop|status|compact|clear`) was detected on an inbound.
pub fn inc_slash_command(command: &str, channel_type: &str) {
    counter!(
        SLASH_COMMANDS_TOTAL,
        "command" => command.to_owned(),
        "channel_type" => channel_type.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_control_rows_written_total{command}` — a control row
/// (e.g. a `/stop`) was persisted to the inbound queue for the runner to
/// consume. (Adapted from the wished `control_rows_pending` gauge: the consumer
/// is a separate process, so there is no in-registry decrement to make a gauge
/// meaningful — this counts control rows written.)
pub fn inc_control_rows_written(command: &str) {
    counter!(CONTROL_ROWS_WRITTEN_TOTAL, "command" => command.to_owned()).increment(1);
}

/// Record `copperclaw_status_answer_seconds` — wall-clock to synthesize a
/// host-side `/status` reply.
pub fn observe_status_answer_seconds(secs: f64) {
    histogram!(STATUS_ANSWER_SECONDS).record(secs);
}

// ── H1 (#30) — Task HUD lifecycle ──────────────────────────────────────────
pub const HUD_POSTS_TOTAL: &str = "copperclaw_hud_posts_total";
pub const HUD_EDITS_TOTAL: &str = "copperclaw_hud_edits_total";
pub const HUD_DEGRADED_TOTAL: &str = "copperclaw_hud_degraded_total";
pub const HUD_FINALIZE_SECONDS: &str = "copperclaw_hud_finalize_seconds";

/// Increment `copperclaw_hud_posts_total{agent_group}` — the HUD posted its
/// first (or final-only) message for a turn.
pub fn inc_hud_post(agent_group: &str) {
    counter!(HUD_POSTS_TOTAL, "agent_group" => agent_group.to_owned()).increment(1);
}

/// Increment `copperclaw_hud_edits_total{agent_group, trigger}` — an in-place
/// HUD edit fired; `trigger` is `batch_start|batch_end|ticker|finalize`.
pub fn inc_hud_edits(agent_group: &str, trigger: &str) {
    counter!(
        HUD_EDITS_TOTAL,
        "agent_group" => agent_group.to_owned(),
        "trigger" => trigger.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_hud_degraded_total{channel_type, reason}` — the HUD
/// fell back to status-rows instead of a live self-editing message.
pub fn inc_hud_degraded(channel_type: &str, reason: &str) {
    counter!(
        HUD_DEGRADED_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

/// Record `copperclaw_hud_finalize_seconds` — turn duration at HUD finalize.
pub fn observe_hud_finalize_seconds(secs: f64) {
    histogram!(HUD_FINALIZE_SECONDS).record(secs);
}

// ── R2 (#31) — mid-turn steering ───────────────────────────────────────────
pub const MIDTURN_CONTROL_TOTAL: &str = "copperclaw_midturn_control_total";

/// Increment `copperclaw_midturn_control_total{agent_group, kind}` — a mid-turn
/// `/stop` was honored (`kind="stop"`) or interjections were consumed
/// (`kind="interjection"`).
pub fn inc_midturn_control(agent_group: &str, kind: &str) {
    counter!(
        MIDTURN_CONTROL_TOTAL,
        "agent_group" => agent_group.to_owned(),
        "kind" => kind.to_owned(),
    )
    .increment(1);
}

// ── R3 (#32) / X2 (#48) — verify gate ──────────────────────────────────────
pub const VERIFY_GATE_COMPLETION_TOTAL: &str = "copperclaw_verify_gate_completion_total";
pub const VERIFY_GATE_FIX_CYCLES: &str = "copperclaw_verify_gate_fix_cycles";
pub const VERIFY_RUN_TOTAL: &str = "copperclaw_verify_run_total";

/// Increment `copperclaw_verify_gate_completion_total{outcome}` — a todo
/// completion crossed the verify gate; `outcome` is
/// `refused_dirty|blocked_cycle_cap|passed`.
pub fn inc_verify_gate_completion(outcome: &str) {
    counter!(VERIFY_GATE_COMPLETION_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Record `copperclaw_verify_gate_fix_cycles` — the fix-cycle count at the
/// moment a project's dirty marker is cleared by a passing verify run.
pub fn observe_verify_gate_fix_cycles(cycles: u32) {
    histogram!(VERIFY_GATE_FIX_CYCLES).record(f64::from(cycles));
}

/// Increment `copperclaw_verify_run_total{result}` — a matched verify command
/// ran; `result` is `pass` or `fail`.
pub fn inc_verify_run(result: &str) {
    counter!(VERIFY_RUN_TOTAL, "result" => result.to_owned()).increment(1);
}

// ── C3/C4 (#33/#37/#38) — inbound attachment materialization ───────────────
pub const INBOUND_FILES_TOTAL: &str = "copperclaw_inbound_files_total";
pub const INBOUND_FILE_BYTES: &str = "copperclaw_inbound_file_bytes";

/// Increment `copperclaw_inbound_files_total{channel, outcome}` — an inbound
/// attachment was materialized; `outcome` is `ok|too_large|download_failed`.
pub fn inc_inbound_file(channel: &str, outcome: &str) {
    counter!(
        INBOUND_FILES_TOTAL,
        "channel" => channel.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Record `copperclaw_inbound_file_bytes{channel}` — downloaded attachment size
/// (tunes `max_attachment_bytes` defaults).
pub fn observe_inbound_file_bytes(channel: &str, bytes: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(INBOUND_FILE_BYTES, "channel" => channel.to_owned()).record(bytes as f64);
}

// ── X1 (#34) — preview-expose serve/timeout ────────────────────────────────
pub const PREVIEW_EXPOSE_TOTAL: &str = "copperclaw_preview_expose_total";

/// Increment `copperclaw_preview_expose_total{outcome}` — a preview-expose MCP
/// call was `served` or timed out (`timeout`).
pub fn inc_preview_expose(outcome: &str) {
    counter!(PREVIEW_EXPOSE_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── R4 (#39) — compaction ──────────────────────────────────────────────────
pub const COMPACTION_TRIGGERED_TOTAL: &str = "copperclaw_compaction_triggered_total";
pub const COMPACTION_ESTIMATED_TOKENS: &str = "copperclaw_compaction_estimated_tokens";
pub const COMPACTION_FACTS_HEADER_BYTES: &str = "copperclaw_compaction_facts_header_bytes";

/// Increment `copperclaw_compaction_triggered_total{profile}` — the auto
/// token-threshold compaction fired.
pub fn inc_compaction_triggered(profile: &str) {
    counter!(COMPACTION_TRIGGERED_TOTAL, "profile" => profile.to_owned()).increment(1);
}

/// Record `copperclaw_compaction_estimated_tokens` — the estimated history token
/// count at the moment compaction triggered.
pub fn observe_compaction_estimated_tokens(tokens: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(COMPACTION_ESTIMATED_TOKENS).record(tokens as f64);
}

/// Record `copperclaw_compaction_facts_header_bytes` — byte size of the project
/// facts header carried across a compaction (only when one is present).
pub fn observe_compaction_facts_header_bytes(bytes: usize) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(COMPACTION_FACTS_HEADER_BYTES).record(bytes as f64);
}

// ── V1 (#41) — preview proxy WebSocket bridge ──────────────────────────────
pub const PREVIEW_WS_UPGRADES_TOTAL: &str = "copperclaw_preview_ws_upgrades_total";
pub const PREVIEW_WS_ACTIVE: &str = "copperclaw_preview_ws_active";
pub const PREVIEW_WS_FRAMES_TOTAL: &str = "copperclaw_preview_ws_frames_total";
pub const PREVIEW_WS_BYTES_TOTAL: &str = "copperclaw_preview_ws_bytes_total";
pub const PREVIEW_WS_SESSION_SECONDS: &str = "copperclaw_preview_ws_session_seconds";

/// Increment `copperclaw_preview_ws_upgrades_total{result}` — a WebSocket
/// upgrade through the preview proxy; `result` is `ok|refused|upstream_502`.
pub fn inc_preview_ws_upgrade(result: &str) {
    counter!(PREVIEW_WS_UPGRADES_TOTAL, "result" => result.to_owned()).increment(1);
}

/// Adjust `copperclaw_preview_ws_active` — currently-open preview WS bridges.
pub fn inc_preview_ws_active() {
    gauge!(PREVIEW_WS_ACTIVE).increment(1.0);
}

/// Adjust `copperclaw_preview_ws_active` down when a bridge closes.
pub fn dec_preview_ws_active() {
    gauge!(PREVIEW_WS_ACTIVE).decrement(1.0);
}

/// Increment `copperclaw_preview_ws_frames_total{direction}` — one bridged
/// frame; `direction` is `browser_to_container|container_to_browser`.
pub fn inc_preview_ws_frame(direction: &str) {
    counter!(PREVIEW_WS_FRAMES_TOTAL, "direction" => direction.to_owned()).increment(1);
}

/// Add to `copperclaw_preview_ws_bytes_total{direction}` — bridged payload bytes.
pub fn add_preview_ws_bytes(direction: &str, bytes: u64) {
    counter!(PREVIEW_WS_BYTES_TOTAL, "direction" => direction.to_owned()).increment(bytes);
}

/// Record `copperclaw_preview_ws_session_seconds` — bridge lifetime.
pub fn observe_preview_ws_session_seconds(secs: f64) {
    histogram!(PREVIEW_WS_SESSION_SECONDS).record(secs);
}

// ── V3 (#42) — headless-browser render ─────────────────────────────────────
pub const BROWSER_RENDER_TOTAL: &str = "copperclaw_browser_render_total";
pub const BROWSER_CHILD_SPAWN_TOTAL: &str = "copperclaw_browser_child_spawn_total";
pub const BROWSER_CHILD_TEARDOWN_TOTAL: &str = "copperclaw_browser_child_teardown_total";
pub const BROWSER_RENDER_DURATION_SECONDS: &str = "copperclaw_browser_render_duration_seconds";
pub const BROWSER_CDP_CONNECT_FAILURES_TOTAL: &str =
    "copperclaw_browser_cdp_connect_failures_total";
pub const BROWSER_SSRF_BLOCK_TOTAL: &str = "copperclaw_browser_ssrf_block_total";

/// Increment `copperclaw_browser_render_total{mode, outcome}` — a browser render;
/// `mode` is `screenshot`/`dom_text`/`aria`, `outcome` is
/// `ok|blocked|driver_error|unavailable`.
pub fn inc_browser_render(mode: &str, outcome: &str) {
    counter!(
        BROWSER_RENDER_TOTAL,
        "mode" => mode.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_browser_child_spawn_total{result}` — browser child
/// container spawn (`ok|error`).
pub fn inc_browser_child_spawn(result: &str) {
    counter!(BROWSER_CHILD_SPAWN_TOTAL, "result" => result.to_owned()).increment(1);
}

/// Increment `copperclaw_browser_child_teardown_total{result}` — browser child
/// container teardown (`ok|error`); errors surface leaked children.
pub fn inc_browser_child_teardown(result: &str) {
    counter!(BROWSER_CHILD_TEARDOWN_TOTAL, "result" => result.to_owned()).increment(1);
}

/// Record `copperclaw_browser_render_duration_seconds` — navigate→artifact span.
pub fn observe_browser_render_duration_seconds(secs: f64) {
    histogram!(BROWSER_RENDER_DURATION_SECONDS).record(secs);
}

/// Increment `copperclaw_browser_cdp_connect_failures_total` — CDP connect to
/// the browser child failed (image/port misconfiguration signal).
pub fn inc_browser_cdp_connect_failure() {
    counter!(BROWSER_CDP_CONNECT_FAILURES_TOTAL).increment(1);
}

/// Increment `copperclaw_browser_ssrf_block_total{stage}` — an SSRF guard
/// refused a target; `stage` is `target_preflight|redirect_hop`.
pub fn inc_browser_ssrf_block(stage: &str) {
    counter!(BROWSER_SSRF_BLOCK_TOTAL, "stage" => stage.to_owned()).increment(1);
}

// ── V4 (#47) — screenshot-the-preview ──────────────────────────────────────
pub const BROWSER_RENDER_SCREENSHOTS_TOTAL: &str = "copperclaw_browser_render_screenshots_total";
pub const BROWSER_RENDER_PREVIEW_ALLOW_INJECTED_TOTAL: &str =
    "copperclaw_browser_render_preview_allow_injected_total";
pub const BROWSER_SCREENSHOT_DURATION_SECONDS: &str =
    "copperclaw_browser_screenshot_duration_seconds";

/// Increment `copperclaw_browser_render_screenshots_total{result}` — a
/// screenshot-mode render (`ok|blocked|driver_error`).
pub fn inc_browser_render_screenshot(result: &str) {
    counter!(BROWSER_RENDER_SCREENSHOTS_TOTAL, "result" => result.to_owned()).increment(1);
}

/// Increment `copperclaw_browser_render_preview_allow_injected_total` — the
/// preview host:port egress-allow injection fired (vs target-only).
pub fn inc_browser_render_preview_allow_injected() {
    counter!(BROWSER_RENDER_PREVIEW_ALLOW_INJECTED_TOTAL).increment(1);
}

/// Record `copperclaw_browser_screenshot_duration_seconds` — spawn→PNG-on-disk.
pub fn observe_browser_screenshot_duration_seconds(secs: f64) {
    histogram!(BROWSER_SCREENSHOT_DURATION_SECONDS).record(secs);
}

// ── V2 (#51) — preview enablement + tombstone recovery ─────────────────────
pub const PREVIEW_ENABLE_CARD_TOTAL: &str = "copperclaw_preview_enable_card_total";
pub const PREVIEW_TOMBSTONE_RECOVERY_TOTAL: &str = "copperclaw_preview_tombstone_recovery_total";
pub const PREVIEW_TOMBSTONED: &str = "copperclaw_preview_tombstoned";

/// Increment `copperclaw_preview_enable_card_total{outcome}` — a one-tap
/// enable-preview approval card lifecycle event; `outcome` is
/// `raised|skipped_already_pending|skipped_no_dispatcher|skipped_no_messaging_group|approved|denied`.
pub fn inc_preview_enable_card(outcome: &str) {
    counter!(PREVIEW_ENABLE_CARD_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Increment `copperclaw_preview_tombstone_recovery_total{outcome}` — a
/// tombstoned preview recovery attempt; `outcome` is
/// `recovered|terminal_spent|terminal_container_gone`.
pub fn inc_preview_tombstone_recovery(outcome: &str) {
    counter!(PREVIEW_TOMBSTONE_RECOVERY_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Adjust `copperclaw_preview_tombstoned` — currently-tombstoned previews.
pub fn inc_preview_tombstoned() {
    gauge!(PREVIEW_TOMBSTONED).increment(1.0);
}

/// Adjust `copperclaw_preview_tombstoned` down on recovery or teardown.
pub fn dec_preview_tombstoned() {
    gauge!(PREVIEW_TOMBSTONED).decrement(1.0);
}

// ── E2 (#43) — image profile / prototyping bakes ───────────────────────────
pub const IMAGE_REBUILD_TOTAL: &str = "copperclaw_image_rebuild_total";
pub const GROUP_IMAGE_PROFILE: &str = "copperclaw_group_image_profile";

/// Increment `copperclaw_image_rebuild_total{image_profile, result}` — a session
/// image rebuild attributed by profile (`minimal|prototyping`); `result` is
/// `ok|failed`. (The legacy unlabeled `copperclaw_image_rebuild_failed_total`
/// via [`inc_image_rebuild_failed`] is retained alongside.)
pub fn inc_image_rebuild(image_profile: &str, result: &str) {
    counter!(
        IMAGE_REBUILD_TOTAL,
        "image_profile" => image_profile.to_owned(),
        "result" => result.to_owned(),
    )
    .increment(1);
}

/// Set `copperclaw_group_image_profile{agent_group_id, image_profile}` to 1 —
/// fleet visibility on which groups run which image profile.
pub fn set_group_image_profile(agent_group_id: &str, image_profile: &str) {
    gauge!(
        GROUP_IMAGE_PROFILE,
        "agent_group_id" => agent_group_id.to_owned(),
        "image_profile" => image_profile.to_owned(),
    )
    .set(1.0);
}

// ── R5 (#44) — hot provider failover ───────────────────────────────────────
pub const PROVIDER_FAILOVER_TOTAL: &str = "copperclaw_provider_failover_total";
pub const PROVIDER_FAILOVER_CHAIN_EXHAUSTED_TOTAL: &str =
    "copperclaw_provider_failover_chain_exhausted_total";

/// Increment `copperclaw_provider_failover_total{from, to}` — a mid-turn hot
/// failover switched from a failed provider to the next serving one.
pub fn inc_provider_failover(from: &str, to: &str) {
    counter!(
        PROVIDER_FAILOVER_TOTAL,
        "from" => from.to_owned(),
        "to" => to.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_provider_failover_chain_exhausted_total{provider}` —
/// the whole failover chain was exhausted and the turn hit the apology.
pub fn inc_provider_failover_chain_exhausted(provider: &str) {
    counter!(
        PROVIDER_FAILOVER_CHAIN_EXHAUSTED_TOTAL,
        "provider" => provider.to_owned(),
    )
    .increment(1);
}

// ── G1 (#45) — in-chat approval taps ───────────────────────────────────────
pub const APPROVAL_TAPS_TOTAL: &str = "copperclaw_approval_taps_total";

/// Increment `copperclaw_approval_taps_total{outcome}` — an in-chat approval
/// tap resolution; `outcome` is `approved|denied|unauthorized|race_noop`.
pub fn inc_approval_tap(outcome: &str) {
    counter!(APPROVAL_TAPS_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── E1 (#49) — session-local package installs ──────────────────────────────
pub const SESSION_INSTALL_TOTAL: &str = "copperclaw_session_install_total";
pub const SESSION_INSTALL_SECONDS: &str = "copperclaw_session_install_seconds";
pub const SESSION_INSTALL_IMAGE_SCOPE_REJECTED_TOTAL: &str =
    "copperclaw_session_install_image_scope_rejected_total";
pub const SESSION_INSTALL_EGRESS_HINT_TOTAL: &str = "copperclaw_session_install_egress_hint_total";

/// Increment `copperclaw_session_install_total{ecosystem, outcome}` — a
/// session-scope install; `ecosystem` is `pip|npm`, `outcome` is
/// `ok|egress_blocked|toolchain_missing|other`.
pub fn inc_session_install(ecosystem: &str, outcome: &str) {
    counter!(
        SESSION_INSTALL_TOTAL,
        "ecosystem" => ecosystem.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Record `copperclaw_session_install_seconds` — session install wall-clock
/// (registry latency signal).
pub fn observe_session_install_seconds(secs: f64) {
    histogram!(SESSION_INSTALL_SECONDS).record(secs);
}

/// Increment `copperclaw_session_install_image_scope_rejected_total` — a
/// `scope:"image"` install carrying pip packages was rejected (a prompt/skill
/// teaching gap signal).
pub fn inc_session_install_image_scope_rejected() {
    counter!(SESSION_INSTALL_IMAGE_SCOPE_REJECTED_TOTAL).increment(1);
}

/// Increment `copperclaw_session_install_egress_hint_total{ecosystem}` — a
/// session install failed with an egress-denial hint surfaced to the model.
pub fn inc_session_install_egress_hint(ecosystem: &str) {
    counter!(
        SESSION_INSTALL_EGRESS_HINT_TOTAL,
        "ecosystem" => ecosystem.to_owned(),
    )
    .increment(1);
}

// ── R6 (#50) — progressive final answers ───────────────────────────────────
pub const PROGRESSIVE_FINAL_TOTAL: &str = "copperclaw_progressive_final_total";
pub const PROGRESSIVE_FINAL_STEPS: &str = "copperclaw_progressive_final_steps";
pub const PROGRESSIVE_FINAL_SKIPPED_TOTAL: &str = "copperclaw_progressive_final_skipped_total";
pub const PROGRESSIVE_FINAL_ANSWER_CHARS: &str = "copperclaw_progressive_final_answer_chars";

/// Increment `copperclaw_progressive_final_total{agent_group, outcome}` — the
/// final-answer reveal path fired (`grown`) or fell through (`single_emit`).
pub fn inc_progressive_final(agent_group: &str, outcome: &str) {
    counter!(
        PROGRESSIVE_FINAL_TOTAL,
        "agent_group" => agent_group.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Record `copperclaw_progressive_final_steps` — edit steps per grown answer.
pub fn observe_progressive_final_steps(steps: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(PROGRESSIVE_FINAL_STEPS).record(steps as f64);
}

/// Increment `copperclaw_progressive_final_skipped_total{reason}` — which gate
/// arm declined growth (`bare_adapter|short_turn|short_answer|expander_scale`).
pub fn inc_progressive_final_skipped(reason: &str) {
    counter!(PROGRESSIVE_FINAL_SKIPPED_TOTAL, "reason" => reason.to_owned()).increment(1);
}

/// Record `copperclaw_progressive_final_answer_chars` — grown-answer length.
pub fn observe_progressive_final_answer_chars(chars: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(PROGRESSIVE_FINAL_ANSWER_CHARS).record(chars as f64);
}

// ── C5 (#52) — adapter rich-render / HUD self-edit ─────────────────────────
pub const ADAPTER_RICH_RENDER_TOTAL: &str = "copperclaw_adapter_rich_render_total";
pub const HUD_EDIT_TOTAL: &str = "copperclaw_hud_edit_total";
pub const ADAPTER_EDIT_MESSAGE_TOTAL: &str = "copperclaw_adapter_edit_message_total";

/// Increment `copperclaw_adapter_rich_render_total{channel_type, surface}` — a
/// channel rendered a native rich surface; `surface` is
/// `card|diff|todo|thinking|error|collapsible`.
pub fn inc_adapter_rich_render(channel_type: &str, surface: &str) {
    counter!(
        ADAPTER_RICH_RENDER_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "surface" => surface.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_hud_edit_total{channel_type, result}` — an adapter-level
/// HUD self-edit attempt; `result` is `ok|error|unsupported_fallthrough`.
pub fn inc_hud_edit(channel_type: &str, result: &str) {
    counter!(
        HUD_EDIT_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "result" => result.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_adapter_edit_message_total{channel_type, result}` — the
/// low-level adapter edit API call (`ok|error`).
pub fn inc_adapter_edit_message(channel_type: &str, result: &str) {
    counter!(
        ADAPTER_EDIT_MESSAGE_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "result" => result.to_owned(),
    )
    .increment(1);
}

// ── R7 (#54) — delegate spawns ─────────────────────────────────────────────
pub const DELEGATE_SPAWN_TOTAL: &str = "copperclaw_delegate_spawn_total";
pub const DELEGATE_DEPTH_REJECTIONS_TOTAL: &str = "copperclaw_delegate_depth_rejections_total";
pub const DELEGATE_WORKTREE_PROVISION_SECONDS: &str =
    "copperclaw_delegate_worktree_provision_seconds";

/// Increment `copperclaw_delegate_spawn_total{tier, outcome}` — a `create_agent` /
/// delegate spawn gate outcome; `tier` is `create_agent|delegate`, `outcome` is
/// `created|denied|rejected|invalid`.
pub fn inc_delegate_spawn(tier: &str, outcome: &str) {
    counter!(
        DELEGATE_SPAWN_TOTAL,
        "tier" => tier.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_delegate_depth_rejections_total{tier}` — a spawn
/// rejected by the nesting depth cap.
pub fn inc_delegate_depth_rejection(tier: &str) {
    counter!(DELEGATE_DEPTH_REJECTIONS_TOTAL, "tier" => tier.to_owned()).increment(1);
}

/// Record `copperclaw_delegate_worktree_provision_seconds` — `git worktree add`
/// latency when a delegate's writable parent-repo worktree is provisioned.
pub fn observe_delegate_worktree_provision_seconds(secs: f64) {
    histogram!(DELEGATE_WORKTREE_PROVISION_SECONDS).record(secs);
}

// ════════════════════════════════════════════════════════════════════════════
// M19 metrics rider (card M1) — one sweep of the metric "wishes" the merged M19
// cards (F1–F5, U1–U7, A1–A6) recorded in their PR descriptions. No other M19
// card touches this crate; names/labels mirror each card's wish. The emit call
// sites live in the crate each wish named (noted per helper). Grouped by card.
// ════════════════════════════════════════════════════════════════════════════

// ── F1 — edit-drift fallthrough ────────────────────────────────────────────
pub const EDIT_DRIFT_FALLTHROUGH_TOTAL: &str = "copperclaw_edit_drift_fallthrough_total";

/// Increment `copperclaw_edit_drift_fallthrough_total{channel_type}` — a HUD /
/// approval edit hit the trait DEFAULT `edit_message` (the "not edit-capable"
/// fallthrough) instead of a real per-adapter override. A dedicated drift alarm
/// distinct from the generic `inc_hud_edit(_, "unsupported_fallthrough")` label:
/// a non-zero rate on a channel that is (or is expected to be) in
/// `EDIT_CAPABLE_CHANNELS` means an adapter lost its override. Emitted from the
/// core trait default (`copperclaw-channels/core/src/adapter.rs`).
pub fn inc_edit_drift_fallthrough(channel_type: &str) {
    counter!(EDIT_DRIFT_FALLTHROUGH_TOTAL, "channel_type" => channel_type.to_owned()).increment(1);
}

// ── F2 — user-facing "wall" cards by blocker category ──────────────────────
pub const WALL_CARD_TOTAL: &str = "copperclaw_wall_card_total";

/// Increment `copperclaw_wall_card_total{blocker}` — a curated, user-facing
/// "I'm blocked" wall card was emitted for a terminally-walled turn; `blocker`
/// is the `BlockerCategory::metric_label()` value
/// (`egress|verify_gate|provenance|autonomous|policy`). Emitted from the runner's
/// `emit_terminal_failure_apologies` (`copperclaw-runner/src/run/mod.rs`) when a
/// wall card is actually written back to a user channel.
pub fn inc_wall_card(blocker: &str) {
    counter!(WALL_CARD_TOTAL, "blocker" => blocker.to_owned()).increment(1);
}

// ── F3 — approval-card lifecycle outcomes ──────────────────────────────────
pub const APPROVAL_CARD_OUTCOME_TOTAL: &str = "copperclaw_approval_card_outcome_total";

/// Increment `copperclaw_approval_card_outcome_total{outcome}` — an approval
/// *card* lifecycle event, distinct from the tap-resolution
/// [`inc_approval_tap`] (`approved|denied|unauthorized|race_noop`). `outcome` is
/// one of `resolved_edit` (card stamped terminal in place), `resolved_fallback_reply`
/// (no editable anchor — resolution posted as a follow-up), `conflict_notified`
/// (a losing tapper was told who settled it / that it lapsed), or `expired_card`
/// (the TTL sweep stamped a delivered card expired). Emitted from
/// `copperclaw-host/src/approval_intercept.rs` and `.../handlers/approvals.rs`.
pub fn inc_approval_card_outcome(outcome: &str) {
    counter!(APPROVAL_CARD_OUTCOME_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── F4 — blocked-todo chips rendered ───────────────────────────────────────
pub const BLOCKED_TODO_RENDER_TOTAL: &str = "copperclaw_blocked_todo_render_total";

/// Increment `copperclaw_blocked_todo_render_total{channel_type, has_reason}` —
/// a delivered todo checklist carried at least one `blocked` item; `has_reason`
/// records whether any blocked item surfaced a reason string. Emitted once per
/// successful todo delivery (`copperclaw-host-delivery/src/service.rs`
/// `dispatch_todo_list`) when the combined list's `blocked_count() > 0`.
pub fn inc_blocked_todo_render(channel_type: &str, has_reason: bool) {
    counter!(
        BLOCKED_TODO_RENDER_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "has_reason" => if has_reason { "true" } else { "false" },
    )
    .increment(1);
}

// ── F5 — pre-first-tool "thinking…" HUD frames ─────────────────────────────
pub const HUD_THINKING_FRAME_TOTAL: &str = "copperclaw_hud_thinking_frame_total";

/// Increment `copperclaw_hud_thinking_frame_total{agent_group}` — the HUD posted
/// its pre-first-tool "thinking…" frame (the armed background task's initial
/// post, fired after `THINKING_THRESHOLD` on a turn that has not yet called a
/// tool). Distinct from the tool-triggered first post counted by
/// [`inc_hud_post`]; both share the `agent_group` label so an operator can see
/// how often a turn surfaced a pure-reasoning wait. Emitted from
/// `copperclaw-runner/src/run/hud.rs` (`TaskHud::arm`).
pub fn inc_hud_thinking_frame(agent_group: &str) {
    counter!(HUD_THINKING_FRAME_TOTAL, "agent_group" => agent_group.to_owned()).increment(1);
}

// ── U1/U2 — rich-surface in-place-edit vs create ───────────────────────────
pub const ADAPTER_SURFACE_WRITE_TOTAL: &str = "copperclaw_adapter_surface_write_total";

/// Increment `copperclaw_adapter_surface_write_total{channel_type, mode}` — a
/// pinned rich surface (the rolled-up todo card / HUD anchor) was written;
/// `mode` is `edit` when a prior anchor existed (edit-in-place intent) or
/// `create` when a fresh card was posted. Lets an operator see the edit-vs-create
/// ratio the U1/U2 rich-adapter upgrades exercise. Emitted from
/// `copperclaw-host-delivery/src/service.rs` (`dispatch_todo_list`).
pub fn inc_adapter_surface_write(channel_type: &str, mode: &str) {
    counter!(
        ADAPTER_SURFACE_WRITE_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "mode" => mode.to_owned(),
    )
    .increment(1);
}

// ── U3 — typing indicators + outbound reactions ────────────────────────────
pub const ADAPTER_TYPING_TOTAL: &str = "copperclaw_adapter_typing_total";
pub const ADAPTER_REACTION_TOTAL: &str = "copperclaw_adapter_reaction_total";

/// Increment `copperclaw_adapter_typing_total{channel_type, result}` — an
/// adapter typing-indicator send resolved; `result` is `ok|rate_limited|unsupported|error`.
/// Emitted from the host dispatcher's central `set_typing` path
/// (`copperclaw-host-delivery/src/dispatch.rs`), so it covers every adapter.
pub fn inc_adapter_typing(channel_type: &str, result: &str) {
    counter!(
        ADAPTER_TYPING_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "result" => result.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_adapter_reaction_total{channel_type, result}` — an
/// outbound emoji-reaction send resolved; `result` is `ok|unsupported|error`.
/// Emitted from the host delivery's central `reaction` system-action path
/// (`copperclaw-host-delivery/src/service.rs`), so it covers every adapter.
pub fn inc_adapter_reaction(channel_type: &str, result: &str) {
    counter!(
        ADAPTER_REACTION_TOTAL,
        "channel_type" => channel_type.to_owned(),
        "result" => result.to_owned(),
    )
    .increment(1);
}

// ── U6 — shared-renderer (`core::markdown::render`) adoption coverage ───────
pub const SHARED_RENDERER_ADOPTION: &str = "copperclaw_shared_renderer_adoption";

/// The adapters whose plain-text outbound path routes through the shared
/// `copperclaw_channels_core::markdown::render` renderer (M19 U6). Kept here as
/// the single source of truth the adoption gauge reflects; growing the shared
/// renderer to a new adapter adds a name here. (gchat is deliberately absent —
/// its text dialect matches no existing `Flavor`; see the U6 CHANGELOG entry.)
pub const SHARED_RENDERER_ADOPTED_ADAPTERS: &[&str] = &[
    "slack",
    "discord",
    "matrix",
    "mattermost",
    "whatsapp-cloud",
    "telegram",
];

/// Set `copperclaw_shared_renderer_adoption{channel_type}` to 1 for every adapter
/// in [`SHARED_RENDERER_ADOPTED_ADAPTERS`]. A static coverage signal (sum the
/// series for the adopted-adapter count). Called once from [`maybe_start_server`]
/// after the recorder is installed, so it needs no external call site.
pub fn set_shared_renderer_adoption() {
    for ct in SHARED_RENDERER_ADOPTED_ADAPTERS {
        gauge!(SHARED_RENDERER_ADOPTION, "channel_type" => (*ct).to_owned()).set(1.0);
    }
}

// ── U7 — inbound reactions by curated signal / outcome ─────────────────────
pub const INBOUND_REACTION_TOTAL: &str = "copperclaw_inbound_reaction_total";

/// Increment `copperclaw_inbound_reaction_total{signal, outcome}` — an inbound
/// reaction reached the runner's mid-turn steering seam. `signal` is the curated
/// meaning (`affirmative|looking|negative|uncurated`) and `outcome` is `folded`
/// (steered the run as a one-line interjection on the agent's own last message)
/// or `ignored` (uncurated emoji, or a reaction on an unrelated message). Distinct
/// from the generic `inc_midturn_control(_, "reaction")`. Emitted from
/// `copperclaw-runner/src/run/drive_turn.rs` (`check_mid_turn_steering`).
pub fn inc_inbound_reaction(signal: &str, outcome: &str) {
    counter!(
        INBOUND_REACTION_TOTAL,
        "signal" => signal.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

// ── A1 — `delegate_batch` parallel fan-out ─────────────────────────────────
pub const DELEGATE_BATCH_WIDTH: &str = "copperclaw_delegate_batch_width";
pub const DELEGATE_BATCH_WORKER_TOTAL: &str = "copperclaw_delegate_batch_worker_total";
pub const DELEGATE_BATCH_REFUSED_TOTAL: &str = "copperclaw_delegate_batch_refused_total";

/// Record `copperclaw_delegate_batch_width` — the number of workers a single
/// `delegate_batch` call fanned out (1..=`MAX_DELEGATE_BATCH_WIDTH`). Emitted from
/// the runner join (`copperclaw-runner/src/run/delegate_batch.rs`).
pub fn observe_delegate_batch_width(workers: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(DELEGATE_BATCH_WIDTH).record(workers as f64);
}

/// Increment `copperclaw_delegate_batch_worker_total{outcome}` — one joined
/// `delegate_batch` worker's terminal status; `outcome` is `ok` (reported),
/// `timeout` (spawned but silent past the join budget), or `spawn_failed`
/// (never spawned — depth/permission gate). Emitted from `finalize_worker`
/// (`copperclaw-runner/src/run/delegate_batch.rs`).
pub fn inc_delegate_batch_worker(outcome: &str) {
    counter!(DELEGATE_BATCH_WORKER_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Increment `copperclaw_delegate_batch_refused_total` — a whole `delegate_batch`
/// was refused (every worker failed to spawn, e.g. a batch from a max-depth
/// child), surfaced to the model as one tool error rather than a partial
/// aggregate. Emitted from the pure handler (`copperclaw-mcp/src/tools/agents.rs`).
pub fn inc_delegate_batch_refused() {
    counter!(DELEGATE_BATCH_REFUSED_TOTAL).increment(1);
}

// ── A2 — interactive browser actions ───────────────────────────────────────
pub const BROWSER_INTERACTIVE_ACTIONS_TOTAL: &str = "copperclaw_browser_interactive_actions_total";

/// Increment `copperclaw_browser_interactive_actions_total{action, outcome}` — one
/// scripted interactive-browser action ran; `action` is
/// `click|type|scroll|wait_for_selector` and `outcome` is `ok`, `blocked` (an
/// SSRF re-guard refused the resulting navigation), or `driver_error`. Emitted
/// per action from `copperclaw-browser/src/interactive.rs`. (The interactive
/// SSRF *stage* labels — `interactive_target_preflight` / `interactive_post_nav`
/// / `interactive_redirect_hop` — are already emitted via [`inc_browser_ssrf_block`].)
pub fn inc_browser_interactive_action(action: &str, outcome: &str) {
    counter!(
        BROWSER_INTERACTIVE_ACTIONS_TOTAL,
        "action" => action.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

// ── A3 — public-tunnel exposures ───────────────────────────────────────────
pub const PUBLIC_TUNNEL_TOTAL: &str = "copperclaw_public_tunnel_total";

/// Increment `copperclaw_public_tunnel_total{outcome, reason}` — a public-tunnel
/// (V5 `make_preview_public`) lifecycle event. `outcome` is `opened` (a tunnel
/// stood up on an approved grant), `approval_raised` (a pending-approval card was
/// raised), `denied` (refused — `reason` `not_enabled` for the opt-in gate or
/// `approval_denied` for an operator deny), or `torn_down` (`reason` the teardown
/// cause, e.g. `preview-closed` / `session-stop`). `reason` is empty for
/// `opened` / `approval_raised`. Emitted from `copperclaw-modules/src/tunnel.rs`.
pub fn inc_public_tunnel(outcome: &str, reason: &str) {
    counter!(
        PUBLIC_TUNNEL_TOTAL,
        "outcome" => outcome.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

// ── A4 — agent-authored skills saved ───────────────────────────────────────
pub const SKILLS_SAVED_TOTAL: &str = "copperclaw_skills_saved_total";

/// Increment `copperclaw_skills_saved_total{outcome}` — an approved `save_skill`
/// write reached the disk; `outcome` is `saved` (the `SKILL.md` was written) or
/// `rejected` (re-validation at the security boundary failed: bad frontmatter,
/// name mismatch, containment escape). Dedicated counter replacing the reuse of
/// `inc_self_mod_*("save_skill")` for the actual save outcome. Emitted from
/// `copperclaw-host/src/handlers/approvals.rs` (`apply_save_skill`).
pub fn inc_skills_saved(outcome: &str) {
    counter!(SKILLS_SAVED_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── A5 — agent memory writes by provenance ─────────────────────────────────
pub const MEMORY_WRITES_TOTAL: &str = "copperclaw_memory_writes_total";
pub const MEMORY_WRITE_RATE_CAPPED_TOTAL: &str = "copperclaw_memory_write_rate_capped_total";

/// Increment `copperclaw_memory_writes_total{provenance}` — an agent-initiated
/// `memory_save` committed to the per-group store; `provenance` is `trusted` or
/// `untrusted` (a turn tainted by untrusted-provenance content is honestly
/// downgraded). The provenance is decided store-side from the runner's own taint
/// flag, never the caller. Emitted from `copperclaw-runner/src/tools.rs`.
pub fn inc_memory_write(provenance: &str) {
    counter!(MEMORY_WRITES_TOTAL, "provenance" => provenance.to_owned()).increment(1);
}

/// Increment `copperclaw_memory_write_rate_capped_total` — a `memory_save` was
/// refused because the per-session write ceiling (`MAX_SAVES_PER_SESSION`) was
/// reached. Emitted from `copperclaw-runner/src/tools.rs`.
pub fn inc_memory_write_rate_capped() {
    counter!(MEMORY_WRITE_RATE_CAPPED_TOTAL).increment(1);
}

// ── A6 — scheduled-task fire lifecycle ─────────────────────────────────────
pub const SCHEDULED_TASK_FIRES_TOTAL: &str = "copperclaw_scheduled_task_fires_total";
pub const SCHEDULED_TASKS_ACTIVE: &str = "copperclaw_scheduled_tasks_active";
pub const SCHEDULED_TASK_FIRE_LATENCY_SECONDS: &str =
    "copperclaw_scheduled_task_fire_latency_seconds";

/// Increment `copperclaw_scheduled_task_fires_total{kind}` — a durable scheduled
/// task fired; `kind` is `recurring_rearm` (a recurring task re-armed its next
/// occurrence) or `one_shot_complete` (a one-shot task transitioned to
/// completed). Emitted from the sweep's due-task fan-out
/// (`copperclaw-host-sweep/src/checks/scheduling.rs`).
pub fn inc_scheduled_task_fire(kind: &str) {
    counter!(SCHEDULED_TASK_FIRES_TOTAL, "kind" => kind.to_owned()).increment(1);
}

/// Set `copperclaw_scheduled_tasks_active` — the count of `active` scheduled
/// tasks in the central `tasks` table at the last sweep pass. Emitted from
/// `copperclaw-host-sweep/src/checks/scheduling.rs`.
pub fn set_scheduled_tasks_active(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    gauge!(SCHEDULED_TASKS_ACTIVE).set(count as f64);
}

/// Record `copperclaw_scheduled_task_fire_latency_seconds` — how late a task
/// fired relative to its scheduled `next_fire` (the sweep's coarse 60s cadence
/// bounds the resolution). Emitted from
/// `copperclaw-host-sweep/src/checks/scheduling.rs`.
pub fn observe_scheduled_task_fire_latency_seconds(secs: f64) {
    histogram!(SCHEDULED_TASK_FIRE_LATENCY_SECONDS).record(secs);
}

// ── Address parsing ────────────────────────────────────────────────────────

/// Parse `COPPERCLAW_METRICS_ADDR`.  Accepts:
/// - `127.0.0.1:9090`  — used verbatim.
/// - `0.0.0.0:9090`   — used verbatim.
/// - `9090`            — prepended with `127.0.0.1:`.
/// - Empty string / unset → returns `None`.
///
/// Any other form that doesn't parse as a `SocketAddr` returns `Err`.
#[derive(Debug, thiserror::Error)]
pub enum AddrParseError {
    #[error("could not parse '{raw}' as a socket address: {source}")]
    Invalid {
        raw: String,
        #[source]
        source: std::net::AddrParseError,
    },
}

pub fn parse_metrics_addr(raw: &str) -> Result<SocketAddr, AddrParseError> {
    let raw = raw.trim();
    // Try as-is first.
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok(addr);
    }
    // Try as a bare port number -> bind to loopback.
    let with_host = format!("127.0.0.1:{raw}");
    with_host
        .parse::<SocketAddr>()
        .map_err(|source| AddrParseError::Invalid {
            raw: raw.to_owned(),
            source,
        })
}

// ── Server ─────────────────────────────────────────────────────────────────

/// Install the Prometheus recorder and (if `addr` is `Some`) start the HTTP
/// listener.  When `addr` is `None`, the function reads `COPPERCLAW_METRICS_ADDR`
/// from the environment.
///
/// If the bind fails, this function logs a warning and returns without
/// starting the listener — the process continues normally.
///
/// The `shutdown` token is passed through to the listener task.  The host
/// passes its own shutdown token so the listener terminates with the host.
pub async fn maybe_start_server(shutdown: Option<CancellationToken>) {
    let raw = match std::env::var("COPPERCLAW_METRICS_ADDR") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };

    let addr = match parse_metrics_addr(&raw) {
        Ok(a) => a,
        Err(e) => {
            warn!("COPPERCLAW_METRICS_ADDR is malformed, metrics endpoint disabled: {e}");
            return;
        }
    };

    // Install the global prometheus recorder.  A second call after the
    // recorder is already set returns an error; in that case reuse the
    // existing handle via a standalone recorder (the data is shared via
    // the global metrics facade).
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .unwrap_or_else(|_| PrometheusBuilder::new().build_recorder().handle());

    // M19 U6: publish the static shared-renderer adoption gauge now that a
    // recorder is installed, so `/metrics` exposes per-adapter coverage.
    set_shared_renderer_adoption();

    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!("could not bind metrics endpoint at {addr}: {e}; metrics endpoint disabled");
            return;
        }
    };
    info!("metrics endpoint listening on http://{addr}/metrics");

    let token = shutdown.unwrap_or_default();
    tokio::spawn(async move {
        run_server(listener, handle, token).await;
    });
}

/// Start the metrics server on a pre-bound listener.  Exposed for tests that
/// need to bind the socket themselves and verify the HTTP response.
///
/// Spawns the accept loop as a background task.  The task exits when
/// `shutdown` is cancelled.
pub fn start_on_listener(listener: TcpListener, shutdown: CancellationToken) {
    let handle = match PrometheusBuilder::new().install_recorder() {
        Ok(h) => h,
        Err(_) => {
            // Already installed — get the current handle via a standalone recorder.
            PrometheusBuilder::new().build_recorder().handle()
        }
    };
    tokio::spawn(async move {
        run_server(listener, handle, shutdown).await;
    });
}

/// Minimal HTTP/1.1 server that serves `GET /metrics` and nothing else.
/// Uses a hand-rolled accept loop over `tokio::net::TcpListener` — no axum
/// or warp dependency.
async fn run_server(
    listener: TcpListener,
    handle: metrics_exporter_prometheus::PrometheusHandle,
    shutdown: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => break,
            res = listener.accept() => res,
        };
        let (mut stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                warn!("metrics accept error: {e}");
                continue;
            }
        };
        let body = handle.render();
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain; version=0.0.4\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            body.len(),
            body
        );
        // Read the request header (we don't validate it — only path is
        // `/metrics` but a scraper that speaks HTTP/1.0 or omits the
        // Host header should still get the data).
        let mut buf = [0u8; 4096];
        tokio::select! {
            () = shutdown.cancelled() => break,
            result = stream.read(&mut buf) => {
                if result.is_err() {
                    continue;
                }
            }
        }
        if let Err(e) = stream.write_all(response.as_bytes()).await {
            warn!(peer = %peer, "metrics write error: {e}");
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    // ---- parse_metrics_addr ----

    #[test]
    fn parse_full_addr() {
        let addr = parse_metrics_addr("127.0.0.1:9090").unwrap();
        assert_eq!(addr, "127.0.0.1:9090".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn parse_bare_port_defaults_to_loopback() {
        let addr = parse_metrics_addr("9090").unwrap();
        assert_eq!(addr.port(), 9090);
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn parse_all_interfaces() {
        let addr = parse_metrics_addr("0.0.0.0:8080").unwrap();
        assert_eq!(addr.port(), 8080);
        assert_eq!(addr.ip().to_string(), "0.0.0.0");
    }

    #[test]
    fn parse_garbage_returns_error() {
        let err = parse_metrics_addr("not::an::addr").unwrap_err();
        assert!(err.to_string().contains("not::an::addr"));
    }

    #[test]
    fn parse_whitespace_only_fails() {
        // A string of only whitespace should fail to parse as a SocketAddr
        // or a port number.
        let result = parse_metrics_addr("   ");
        assert!(result.is_err(), "expected error for whitespace-only input");
    }

    #[test]
    fn parse_error_display_includes_raw() {
        let err = parse_metrics_addr("bad-input").unwrap_err();
        let s = err.to_string();
        assert!(s.contains("bad-input"), "display: {s}");
    }

    // ---- maybe_start_server: env unset path (no panic) ----
    // We test the individual internal functions rather than going through the
    // env-var path to avoid the unsafe set_var / remove_var that Rust 2024
    // edition forbids without an explicit unsafe block.

    #[tokio::test]
    async fn start_on_listener_serves_prometheus_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let token = CancellationToken::new();

        // Find a free port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let token_clone = token.clone();
        start_on_listener(listener, token_clone);

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();

        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "expected 200, got: {response}"
        );
        assert!(
            response.contains("text/plain"),
            "expected text/plain content-type"
        );
        // Prometheus text format body is either empty (no metrics registered
        // yet in this test run's recorder) or begins with '#'.
        let body_start = response.find("\r\n\r\n").map_or(0, |i| i + 4);
        let body = &response[body_start..];
        assert!(
            body.is_empty() || body.starts_with('#'),
            "unexpected body: {body:?}"
        );

        token.cancel();
    }

    // ---- bind failure: port 1 requires root on Linux ----

    #[tokio::test]
    async fn bind_failure_warns_and_does_not_panic() {
        // Attempting to bind port 1 will fail for unprivileged processes.
        // The important property is that it doesn't panic.
        let addr = "127.0.0.1:1".parse::<SocketAddr>().unwrap();
        // We exercise the internal bind path directly instead of going
        // through the env-var path.
        let result = TcpListener::bind(addr).await;
        // Either it fails (expected) or it succeeds (running as root, fine).
        if let Err(e) = result {
            // Confirm the error is "permission denied" or similar.
            assert!(
                e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.kind() == std::io::ErrorKind::AddrInUse
                    || e.raw_os_error().is_some(),
                "unexpected error kind: {e}"
            );
        }
        // Ok(_) → running as root — acceptable.
    }

    // ---- malformed address: parse-level error path ----

    #[test]
    fn malformed_addr_returns_parse_error() {
        // This exercises the exact code path that `maybe_start_server` hits
        // when COPPERCLAW_METRICS_ADDR contains garbage.
        let result = parse_metrics_addr("not-a-socket-addr!!!");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not-a-socket-addr")
        );
    }

    // ---- counter helpers compile and don't panic ----

    #[test]
    fn counter_helpers_compile() {
        // These will no-op when no recorder is installed; that's fine.
        inc_messages_inbound("cli");
        inc_messages_outbound("telegram");
        inc_containers_spawned();
        inc_containers_crashed();
        inc_delivery_failed("slack");
        inc_delivery_formatting_fallback("telegram");
        inc_self_mod_failed("install_packages");
        inc_self_mod_succeeded("add_mcp_server");
        inc_image_rebuild_failed();
        inc_secrets_rotated();
        inc_provider_deadline("anthropic");
        inc_provider_retry("anthropic");
        inc_budget_exhausted("ag-test", BUDGET_GATE_DAILY_TOKENS);
        inc_budget_exhausted("ag-test", BUDGET_GATE_TURNS_PER_MINUTE);
        inc_budget_exhausted("ag-test", BUDGET_GATE_TURNS_PER_HOUR);
        inc_budget_exhausted_reply("ag-test");
        inc_budget_exhausted_suppressed("ag-test");
        inc_task_budget_exhausted("ag-test");
        inc_stuck_inbound_apology("ag-test", STUCK_REASON_PENDING_TOO_LONG);
        inc_stuck_inbound_apology("ag-test", STUCK_REASON_CONTAINER_SPAWN_FAILED);
        inc_tool_loop_breaker("ag-test", LOOP_PATTERN_IDENTICAL);
        inc_tool_loop_breaker("ag-test", LOOP_PATTERN_PING_PONG);
    }

    #[test]
    fn budget_gate_label_constants_are_snake_case() {
        for label in [
            BUDGET_GATE_DAILY_TOKENS,
            BUDGET_GATE_TURNS_PER_MINUTE,
            BUDGET_GATE_TURNS_PER_HOUR,
        ] {
            assert!(
                !label.contains("__"),
                "label {label:?} must not contain double underscores"
            );
            assert!(
                label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "label {label:?} must be snake_case ASCII"
            );
        }
    }

    #[test]
    fn budget_exhausted_counter_renders_with_labels() {
        // Install an isolated recorder via build_recorder() so this test
        // doesn't depend on (or interfere with) other tests' state.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            inc_budget_exhausted("ag-abc-123", BUDGET_GATE_DAILY_TOKENS);
            inc_budget_exhausted("ag-abc-123", BUDGET_GATE_DAILY_TOKENS);
            inc_budget_exhausted("ag-abc-123", BUDGET_GATE_TURNS_PER_MINUTE);
            inc_budget_exhausted_reply("ag-abc-123");
            inc_budget_exhausted_suppressed("ag-abc-123");
        });
        let body = handle.render();
        // The Prometheus text exposition format renders counters as
        // `<name>{<labels>} <value>`. We don't care about whitespace,
        // just that the right name + label + value triple shows up.
        assert!(
            body.contains(BUDGET_EXHAUSTED_TOTAL),
            "missing exhausted counter:\n{body}"
        );
        assert!(
            body.contains("gate=\"daily_tokens\""),
            "missing gate label:\n{body}"
        );
        assert!(
            body.contains("gate=\"turns_per_minute\""),
            "missing per-minute gate label:\n{body}"
        );
        assert!(
            body.contains("agent_group_id=\"ag-abc-123\""),
            "missing agent_group_id label:\n{body}"
        );
        assert!(
            body.contains(BUDGET_EXHAUSTED_REPLIES_TOTAL),
            "missing replies counter:\n{body}"
        );
        assert!(
            body.contains(BUDGET_EXHAUSTED_SUPPRESSED_TOTAL),
            "missing suppressed counter:\n{body}"
        );
    }

    #[test]
    fn task_budget_exhausted_counter_renders_with_labels() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            inc_task_budget_exhausted("ag-runaway-7");
            inc_task_budget_exhausted("ag-runaway-7");
        });
        let body = handle.render();
        assert!(
            body.contains(TASK_BUDGET_EXHAUSTED_TOTAL),
            "missing task-budget counter:\n{body}"
        );
        assert!(
            body.contains("agent_group_id=\"ag-runaway-7\""),
            "missing agent_group_id label:\n{body}"
        );
    }

    #[test]
    fn histogram_helpers_compile() {
        observe_llm_call_seconds(1.23);
        observe_llm_tokens_input(512);
        observe_llm_tokens_output(128);
        observe_container_spawn_seconds(0.5);
    }

    // ---- metric name constants are correct ----

    #[test]
    fn metric_name_constants_have_copperclaw_prefix() {
        let names = [
            MESSAGES_INBOUND_TOTAL,
            MESSAGES_OUTBOUND_TOTAL,
            CONTAINERS_SPAWNED_TOTAL,
            CONTAINERS_CRASHED_TOTAL,
            IMAGE_REBUILD_FAILED_TOTAL,
            SECRETS_ROTATED_TOTAL,
            DELIVERY_FAILED_TOTAL,
            DELIVERY_FORMATTING_FALLBACK_TOTAL,
            SELF_MOD_FAILED_TOTAL,
            SELF_MOD_SUCCEEDED_TOTAL,
            BUDGET_EXHAUSTED_TOTAL,
            BUDGET_EXHAUSTED_REPLIES_TOTAL,
            BUDGET_EXHAUSTED_SUPPRESSED_TOTAL,
            TASK_BUDGET_EXHAUSTED_TOTAL,
            LLM_CALL_SECONDS,
            LLM_TOKENS_INPUT,
            LLM_TOKENS_OUTPUT,
            CONTAINER_SPAWN_SECONDS,
            PROVIDER_DEADLINE_TOTAL,
            PROVIDER_RETRY_TOTAL,
            STUCK_INBOUND_APOLOGY_TOTAL,
            TOOL_LOOP_BREAKER_TOTAL,
            DEGRADED_STATE,
        ];
        for name in names {
            assert!(
                name.starts_with("copperclaw_"),
                "metric name {name:?} does not start with 'copperclaw_'"
            );
            assert!(
                !name.contains("__"),
                "metric name {name:?} must not contain double underscores"
            );
        }
    }

    #[test]
    fn counter_metric_names_end_with_total() {
        let counters = [
            MESSAGES_INBOUND_TOTAL,
            MESSAGES_OUTBOUND_TOTAL,
            CONTAINERS_SPAWNED_TOTAL,
            CONTAINERS_CRASHED_TOTAL,
            IMAGE_REBUILD_FAILED_TOTAL,
            SECRETS_ROTATED_TOTAL,
            DELIVERY_FAILED_TOTAL,
            DELIVERY_FORMATTING_FALLBACK_TOTAL,
            SELF_MOD_FAILED_TOTAL,
            SELF_MOD_SUCCEEDED_TOTAL,
            BUDGET_EXHAUSTED_TOTAL,
            BUDGET_EXHAUSTED_REPLIES_TOTAL,
            BUDGET_EXHAUSTED_SUPPRESSED_TOTAL,
            TASK_BUDGET_EXHAUSTED_TOTAL,
            STUCK_INBOUND_APOLOGY_TOTAL,
            TOOL_LOOP_BREAKER_TOTAL,
        ];
        for name in counters {
            assert!(
                name.ends_with("_total"),
                "counter {name:?} does not end with '_total'"
            );
        }
    }

    #[test]
    fn stuck_inbound_apology_counter_renders_with_labels() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            inc_stuck_inbound_apology("ag-stuck-1", STUCK_REASON_PENDING_TOO_LONG);
            inc_stuck_inbound_apology("ag-stuck-1", STUCK_REASON_CONTAINER_SPAWN_FAILED);
            inc_stuck_inbound_apology("ag-stuck-1", STUCK_REASON_PENDING_TOO_LONG);
        });
        let body = handle.render();
        assert!(
            body.contains(STUCK_INBOUND_APOLOGY_TOTAL),
            "missing stuck-apology counter:\n{body}"
        );
        assert!(
            body.contains("reason=\"pending_too_long\""),
            "missing pending_too_long reason label:\n{body}"
        );
        assert!(
            body.contains("reason=\"container_spawn_failed\""),
            "missing container_spawn_failed reason label:\n{body}"
        );
        assert!(
            body.contains("agent_group_id=\"ag-stuck-1\""),
            "missing agent_group_id label:\n{body}"
        );
    }

    #[test]
    fn tool_loop_breaker_counter_renders_with_labels() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            inc_tool_loop_breaker("ag-loop-1", LOOP_PATTERN_IDENTICAL);
            inc_tool_loop_breaker("ag-loop-1", LOOP_PATTERN_PING_PONG);
            inc_tool_loop_breaker("ag-loop-1", LOOP_PATTERN_IDENTICAL);
        });
        let body = handle.render();
        assert!(
            body.contains(TOOL_LOOP_BREAKER_TOTAL),
            "missing loop-breaker counter:\n{body}"
        );
        assert!(
            body.contains("pattern=\"identical\""),
            "missing identical pattern label:\n{body}"
        );
        assert!(
            body.contains("pattern=\"ping_pong\""),
            "missing ping_pong pattern label:\n{body}"
        );
        assert!(
            body.contains("agent_group_id=\"ag-loop-1\""),
            "missing agent_group_id label:\n{body}"
        );
    }

    #[test]
    fn loop_pattern_label_constants_are_snake_case() {
        for label in [LOOP_PATTERN_IDENTICAL, LOOP_PATTERN_PING_PONG] {
            assert!(
                !label.contains("__"),
                "label {label:?} must not contain double underscores"
            );
            assert!(
                label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                "label {label:?} must be snake_case ASCII"
            );
        }
    }

    // ── M18 metrics-rider (card M1) coverage ───────────────────────────────

    /// Every new M18 metric name const, so the prefix / double-underscore
    /// invariants extend to the rider's additions.
    const M18_METRIC_NAMES: &[&str] = &[
        SLACK_TYPING_SKIPPED_TOTAL,
        SLACK_TYPING_SET_STATUS_TOTAL,
        SLACK_HUD_DECISION_TOTAL,
        SHELL_TRUNCATED_TOTAL,
        SHELL_TRUNCATED_BYTES,
        READ_FILE_LINES_MODE_TOTAL,
        READ_FILE_PAGES,
        SESSIONS_SPAWNED_PROFILE_TOTAL,
        SYSTEM_PROMPT_BYTES,
        LOAD_SKILL_TOTAL,
        POLICY_DENIED_TOTAL,
        UNKNOWN_TOOL_TOTAL,
        DELIVERY_FENCE_SPLIT_TOTAL,
        DELIVERY_FENCE_UNBALANCED_INPUT_TOTAL,
        MARKDOWN_RENDER_TOTAL,
        MARKDOWN_UNBALANCED_MARKER_TOTAL,
        SLASH_COMMANDS_TOTAL,
        CONTROL_ROWS_WRITTEN_TOTAL,
        STATUS_ANSWER_SECONDS,
        HUD_POSTS_TOTAL,
        HUD_EDITS_TOTAL,
        HUD_DEGRADED_TOTAL,
        HUD_FINALIZE_SECONDS,
        MIDTURN_CONTROL_TOTAL,
        VERIFY_GATE_COMPLETION_TOTAL,
        VERIFY_GATE_FIX_CYCLES,
        VERIFY_RUN_TOTAL,
        INBOUND_FILES_TOTAL,
        INBOUND_FILE_BYTES,
        PREVIEW_EXPOSE_TOTAL,
        COMPACTION_TRIGGERED_TOTAL,
        COMPACTION_ESTIMATED_TOKENS,
        COMPACTION_FACTS_HEADER_BYTES,
        PREVIEW_WS_UPGRADES_TOTAL,
        PREVIEW_WS_ACTIVE,
        PREVIEW_WS_FRAMES_TOTAL,
        PREVIEW_WS_BYTES_TOTAL,
        PREVIEW_WS_SESSION_SECONDS,
        BROWSER_RENDER_TOTAL,
        BROWSER_CHILD_SPAWN_TOTAL,
        BROWSER_CHILD_TEARDOWN_TOTAL,
        BROWSER_RENDER_DURATION_SECONDS,
        BROWSER_CDP_CONNECT_FAILURES_TOTAL,
        BROWSER_SSRF_BLOCK_TOTAL,
        BROWSER_RENDER_SCREENSHOTS_TOTAL,
        BROWSER_RENDER_PREVIEW_ALLOW_INJECTED_TOTAL,
        BROWSER_SCREENSHOT_DURATION_SECONDS,
        PREVIEW_ENABLE_CARD_TOTAL,
        PREVIEW_TOMBSTONE_RECOVERY_TOTAL,
        PREVIEW_TOMBSTONED,
        IMAGE_REBUILD_TOTAL,
        GROUP_IMAGE_PROFILE,
        PROVIDER_FAILOVER_TOTAL,
        PROVIDER_FAILOVER_CHAIN_EXHAUSTED_TOTAL,
        APPROVAL_TAPS_TOTAL,
        SESSION_INSTALL_TOTAL,
        SESSION_INSTALL_SECONDS,
        SESSION_INSTALL_IMAGE_SCOPE_REJECTED_TOTAL,
        SESSION_INSTALL_EGRESS_HINT_TOTAL,
        PROGRESSIVE_FINAL_TOTAL,
        PROGRESSIVE_FINAL_STEPS,
        PROGRESSIVE_FINAL_SKIPPED_TOTAL,
        PROGRESSIVE_FINAL_ANSWER_CHARS,
        ADAPTER_RICH_RENDER_TOTAL,
        HUD_EDIT_TOTAL,
        ADAPTER_EDIT_MESSAGE_TOTAL,
        DELEGATE_SPAWN_TOTAL,
        DELEGATE_DEPTH_REJECTIONS_TOTAL,
        DELEGATE_WORKTREE_PROVISION_SECONDS,
    ];

    #[test]
    fn m18_metric_names_have_copperclaw_prefix_no_double_underscore() {
        for name in M18_METRIC_NAMES {
            assert!(
                name.starts_with("copperclaw_"),
                "metric name {name:?} does not start with 'copperclaw_'"
            );
            assert!(
                !name.contains("__"),
                "metric name {name:?} must not contain double underscores"
            );
        }
    }

    #[test]
    fn m18_counter_names_end_with_total() {
        for name in M18_METRIC_NAMES {
            if name.contains("_seconds")
                || name.contains("_bytes")
                || name.contains("_tokens")
                || name.contains("_chars")
                || name.contains("_steps")
                || name.contains("_pages")
                || name.contains("_fix_cycles")
                || name.ends_with("_active")
                || name.ends_with("_tombstoned")
                || name == &GROUP_IMAGE_PROFILE
            {
                // histograms / gauges: exempt from the `_total` suffix rule.
                continue;
            }
            assert!(
                name.ends_with("_total"),
                "counter {name:?} does not end with '_total'"
            );
        }
    }

    #[test]
    fn m18_helpers_compile_and_do_not_panic() {
        // No recorder installed → all of these no-op; this is a smoke test that
        // every rider helper is callable with its intended argument shape.
        inc_slack_typing_skipped("non_assistant_surface");
        inc_slack_typing_set_status("ok");
        inc_slack_hud_decision(true);
        inc_shell_truncated("head");
        observe_shell_truncated_bytes(4096);
        inc_read_file_lines_mode();
        observe_read_file_pages(3);
        inc_session_spawned_profile("coding");
        observe_system_prompt_bytes("coding", 12_345);
        inc_load_skill("coding-task", "inline");
        inc_policy_denied("profile", "shell");
        inc_unknown_tool("frobnicate");
        inc_delivery_fence_split("telegram", "backtick");
        inc_delivery_fence_unbalanced_input("telegram");
        inc_markdown_render("slack");
        inc_markdown_unbalanced_marker("slack");
        inc_slash_command("stop", "cli");
        inc_control_rows_written("stop");
        observe_status_answer_seconds(0.01);
        inc_hud_post("ag-1");
        inc_hud_edits("ag-1", "ticker");
        inc_hud_degraded("signal", "no_message_edit");
        observe_hud_finalize_seconds(12.5);
        inc_midturn_control("ag-1", "stop");
        inc_verify_gate_completion("passed");
        observe_verify_gate_fix_cycles(2);
        inc_verify_run("pass");
        inc_inbound_file("slack", "ok");
        observe_inbound_file_bytes("slack", 1024);
        inc_preview_expose("served");
        inc_compaction_triggered("coding");
        observe_compaction_estimated_tokens(120_000);
        observe_compaction_facts_header_bytes(2048);
        inc_preview_ws_upgrade("ok");
        inc_preview_ws_active();
        dec_preview_ws_active();
        inc_preview_ws_frame("browser_to_container");
        add_preview_ws_bytes("container_to_browser", 512);
        observe_preview_ws_session_seconds(30.0);
        inc_browser_render("screenshot", "ok");
        inc_browser_child_spawn("ok");
        inc_browser_child_teardown("error");
        observe_browser_render_duration_seconds(1.5);
        inc_browser_cdp_connect_failure();
        inc_browser_ssrf_block("target_preflight");
        inc_browser_render_screenshot("ok");
        inc_browser_render_preview_allow_injected();
        observe_browser_screenshot_duration_seconds(2.0);
        inc_preview_enable_card("raised");
        inc_preview_tombstone_recovery("recovered");
        inc_preview_tombstoned();
        dec_preview_tombstoned();
        inc_image_rebuild("prototyping", "ok");
        set_group_image_profile("ag-1", "prototyping");
        inc_provider_failover("anthropic", "openai");
        inc_provider_failover_chain_exhausted("openai");
        inc_approval_tap("approved");
        inc_session_install("pip", "ok");
        observe_session_install_seconds(4.2);
        inc_session_install_image_scope_rejected();
        inc_session_install_egress_hint("npm");
        inc_progressive_final("ag-1", "grown");
        observe_progressive_final_steps(6);
        inc_progressive_final_skipped("short_answer");
        observe_progressive_final_answer_chars(800);
        inc_adapter_rich_render("mattermost", "card");
        inc_hud_edit("signal", "ok");
        inc_adapter_edit_message("signal", "ok");
        inc_delegate_spawn("delegate", "created");
        inc_delegate_depth_rejection("delegate");
        observe_delegate_worktree_provision_seconds(0.3);
    }

    #[test]
    fn m18_labeled_counter_renders() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            inc_delegate_spawn("delegate", "created");
            inc_delegate_spawn("create_agent", "denied");
            inc_approval_tap("unauthorized");
        });
        let body = handle.render();
        assert!(
            body.contains(DELEGATE_SPAWN_TOTAL),
            "missing delegate:\n{body}"
        );
        assert!(
            body.contains("tier=\"delegate\""),
            "missing tier label:\n{body}"
        );
        assert!(
            body.contains("outcome=\"created\""),
            "missing outcome label:\n{body}"
        );
        assert!(
            body.contains(APPROVAL_TAPS_TOTAL),
            "missing approval:\n{body}"
        );
        assert!(
            body.contains("outcome=\"unauthorized\""),
            "missing approval outcome:\n{body}"
        );
    }

    // ── M19 metrics-rider (card M1) coverage ───────────────────────────────

    /// Every new M19 metric name const, so the prefix / double-underscore
    /// invariants extend to the rider's additions.
    const M19_METRIC_NAMES: &[&str] = &[
        EDIT_DRIFT_FALLTHROUGH_TOTAL,
        WALL_CARD_TOTAL,
        APPROVAL_CARD_OUTCOME_TOTAL,
        BLOCKED_TODO_RENDER_TOTAL,
        HUD_THINKING_FRAME_TOTAL,
        ADAPTER_SURFACE_WRITE_TOTAL,
        ADAPTER_TYPING_TOTAL,
        ADAPTER_REACTION_TOTAL,
        SHARED_RENDERER_ADOPTION,
        INBOUND_REACTION_TOTAL,
        DELEGATE_BATCH_WIDTH,
        DELEGATE_BATCH_WORKER_TOTAL,
        DELEGATE_BATCH_REFUSED_TOTAL,
        BROWSER_INTERACTIVE_ACTIONS_TOTAL,
        PUBLIC_TUNNEL_TOTAL,
        SKILLS_SAVED_TOTAL,
        MEMORY_WRITES_TOTAL,
        MEMORY_WRITE_RATE_CAPPED_TOTAL,
        SCHEDULED_TASK_FIRES_TOTAL,
        SCHEDULED_TASKS_ACTIVE,
        SCHEDULED_TASK_FIRE_LATENCY_SECONDS,
    ];

    #[test]
    fn m19_metric_names_have_copperclaw_prefix_no_double_underscore() {
        for name in M19_METRIC_NAMES {
            assert!(
                name.starts_with("copperclaw_"),
                "metric name {name:?} does not start with 'copperclaw_'"
            );
            assert!(
                !name.contains("__"),
                "metric name {name:?} must not contain double underscores"
            );
        }
    }

    #[test]
    fn m19_counter_names_end_with_total() {
        for name in M19_METRIC_NAMES {
            if name.contains("_seconds")
                || name == &DELEGATE_BATCH_WIDTH
                || name == &SHARED_RENDERER_ADOPTION
                || name == &SCHEDULED_TASKS_ACTIVE
            {
                // histograms / gauges: exempt from the `_total` suffix rule.
                continue;
            }
            assert!(
                name.ends_with("_total"),
                "counter {name:?} does not end with '_total'"
            );
        }
    }

    #[test]
    fn m19_helpers_compile_and_do_not_panic() {
        // No recorder installed → all of these no-op; smoke test that every
        // rider helper is callable with its intended argument shape.
        inc_edit_drift_fallthrough("webex");
        inc_wall_card("egress");
        inc_approval_card_outcome("resolved_edit");
        inc_approval_card_outcome("conflict_notified");
        inc_blocked_todo_render("telegram", true);
        inc_blocked_todo_render("signal", false);
        inc_hud_thinking_frame("ag-1");
        inc_adapter_surface_write("mattermost", "edit");
        inc_adapter_surface_write("deltachat", "create");
        inc_adapter_typing("discord", "ok");
        inc_adapter_typing("discord", "rate_limited");
        inc_adapter_reaction("teams", "unsupported");
        set_shared_renderer_adoption();
        inc_inbound_reaction("affirmative", "folded");
        inc_inbound_reaction("uncurated", "ignored");
        observe_delegate_batch_width(3);
        inc_delegate_batch_worker("ok");
        inc_delegate_batch_worker("timeout");
        inc_delegate_batch_worker("spawn_failed");
        inc_delegate_batch_refused();
        inc_browser_interactive_action("click", "ok");
        inc_browser_interactive_action("type", "blocked");
        inc_public_tunnel("opened", "");
        inc_public_tunnel("torn_down", "session-stop");
        inc_skills_saved("saved");
        inc_skills_saved("rejected");
        inc_memory_write("trusted");
        inc_memory_write("untrusted");
        inc_memory_write_rate_capped();
        inc_scheduled_task_fire("recurring_rearm");
        inc_scheduled_task_fire("one_shot_complete");
        set_scheduled_tasks_active(7);
        observe_scheduled_task_fire_latency_seconds(1.5);
    }

    #[test]
    fn m19_labeled_counter_renders() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            inc_wall_card("egress");
            inc_approval_card_outcome("expired_card");
            inc_public_tunnel("denied", "not_enabled");
            inc_scheduled_task_fire("recurring_rearm");
            set_shared_renderer_adoption();
        });
        let body = handle.render();
        assert!(body.contains(WALL_CARD_TOTAL), "missing wall card:\n{body}");
        assert!(
            body.contains("blocker=\"egress\""),
            "missing blocker label:\n{body}"
        );
        assert!(
            body.contains(APPROVAL_CARD_OUTCOME_TOTAL),
            "missing approval card outcome:\n{body}"
        );
        assert!(
            body.contains("outcome=\"expired_card\""),
            "missing expired_card outcome:\n{body}"
        );
        assert!(
            body.contains(PUBLIC_TUNNEL_TOTAL) && body.contains("reason=\"not_enabled\""),
            "missing public tunnel reason:\n{body}"
        );
        assert!(
            body.contains(SHARED_RENDERER_ADOPTION) && body.contains("channel_type=\"slack\""),
            "missing shared-renderer adoption gauge:\n{body}"
        );
    }
}
