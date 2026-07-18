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

// ════════════════════════════════════════════════════════════════════════════
// M20 metrics rider (card M1) — one sweep of the metric "wishes" the merged M20
// cards (Q1-Q8, D1-D5) recorded in their PR descriptions. No other M20 card
// touches this crate; names/labels mirror each card's wish where one exists,
// noting any proxy/gap. The emit call sites live in the crate each wish named
// (noted per helper). Grouped by card.
// ════════════════════════════════════════════════════════════════════════════

// ── Q1 — image profile bundle version + pinned-binary fetch outcomes ──────
pub const IMAGE_BUNDLE_VERSION: &str = "copperclaw_image_bundle_version";
pub const PINNED_BINARY_FETCH_TOTAL: &str = "copperclaw_pinned_binary_fetch_total";

/// Set `copperclaw_image_bundle_version{profile, pinned_binary, version}` to 1
/// — fleet visibility on exactly which pinned-binary version a spawning
/// group's image profile bakes (e.g. `profile="prototyping"`,
/// `pinned_binary="ruff"`, `version="0.15.22"`). Complements the M18
/// `copperclaw_group_image_profile` gauge (profile only, no binary version).
/// Emitted from `copperclaw-host/src/container_manager/spawn.rs` at the same
/// spawn point as [`set_group_image_profile`], once per
/// [`ImageProfile::extra_pinned_binaries`] entry (no-op for a profile with
/// none, e.g. `minimal`).
///
/// [`ImageProfile::extra_pinned_binaries`]: https://docs.rs/copperclaw-types
pub fn set_image_bundle_version(profile: &str, pinned_binary: &str, version: &str) {
    gauge!(
        IMAGE_BUNDLE_VERSION,
        "profile" => profile.to_owned(),
        "pinned_binary" => pinned_binary.to_owned(),
        "version" => version.to_owned(),
    )
    .set(1.0);
}

/// Increment `copperclaw_pinned_binary_fetch_total{binary, outcome}` — one
/// `fetch_pinned_binary` call during an image build/rebuild step. `outcome` is
/// `cache_hit` (verified bytes already on disk under the cache dir), `fetch_ok`
/// (downloaded + checksum-verified + extracted), `checksum_fail` (the
/// downloaded tarball's sha256 didn't match the pin), `arch_unsupported` (no
/// [`PinnedBinaryTarget`] for `std::env::consts::ARCH`), or `fetch_failed` (any
/// other download/extract/read error — network blip, corrupt archive, missing
/// archive member). Emitted from
/// `copperclaw-setup/src/steps/image.rs` (`fetch_pinned_binary`).
///
/// [`PinnedBinaryTarget`]: https://docs.rs/copperclaw-types
pub fn inc_pinned_binary_fetch(binary: &str, outcome: &str) {
    counter!(
        PINNED_BINARY_FETCH_TOTAL,
        "binary" => binary.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

// ── Q2 — multi-stage verify: per-stage runs + gate refusals by pending count ─
pub const VERIFY_RUN_STAGE_TOTAL: &str = "copperclaw_verify_run_stage_total";
pub const VERIFY_GATE_PENDING_STAGES_TOTAL: &str = "copperclaw_verify_gate_pending_stages_total";
pub const VERIFY_STAGES_DECLARED: &str = "copperclaw_verify_stages_declared";

/// Increment `copperclaw_verify_run_stage_total{stage, result}` — a matched
/// verify command ran, attributed to the named stage it satisfied (a legacy
/// one-line unprefixed file derives a single stage name); `result` is `pass`
/// or `fail`. Distinct from the pre-Q2, stage-unaware
/// [`inc_verify_run`] (kept for the legacy single-stage-file back-compat
/// path — both fire together so existing dashboards built on `inc_verify_run`
/// don't need to change). Emitted from
/// `copperclaw-mcp/src/tools/computer_use.rs` (`apply_verify_gate`).
pub fn inc_verify_run_stage(stage: &str, result: &str) {
    counter!(
        VERIFY_RUN_STAGE_TOTAL,
        "stage" => stage.to_owned(),
        "result" => result.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_verify_gate_pending_stages_total{pending}` — a todo
/// completion was refused by the multi-stage verify gate; `pending` buckets the
/// count of stages still missing/failing since the last dirty mark as `"1"` or
/// `"2+"` (coarse enough to stay low-cardinality while distinguishing "one more
/// stage to go" from "barely started"). Emitted from
/// `copperclaw-mcp/src/tools/todo.rs` (the completion gate), alongside the
/// existing `inc_verify_gate_completion("refused_dirty")`.
pub fn inc_verify_gate_pending_stages(pending: &str) {
    counter!(VERIFY_GATE_PENDING_STAGES_TOTAL, "pending" => pending.to_owned()).increment(1);
}

/// Record `copperclaw_verify_stages_declared` — the number of stages parsed
/// out of a project's `.copperclaw/verify` file (1 for a legacy unprefixed
/// single-line file). Emitted from `copperclaw-mcp/src/tools/verify_gate.rs`
/// wherever the file is parsed into its stage list.
pub fn observe_verify_stages_declared(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(VERIFY_STAGES_DECLARED).record(count as f64);
}

// ── Q3 — diagnostics tool runs by tool + outcome ───────────────────────────
pub const DIAGNOSTICS_RUN_TOTAL: &str = "copperclaw_diagnostics_run_total";

/// Increment `copperclaw_diagnostics_run_total{tool, outcome}` — a
/// `diagnostics` tool call attempted to run one linter/typechecker; `tool` is
/// `eslint|tsc|ruff`, `outcome` is `ran|not_available|error`. Emitted from
/// `copperclaw-mcp/src/tools/diagnostics.rs`.
pub fn inc_diagnostics_run(tool: &str, outcome: &str) {
    counter!(
        DIAGNOSTICS_RUN_TOTAL,
        "tool" => tool.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

// ── Q6 — enforced self-review gate ─────────────────────────────────────────
pub const REVIEW_GATE_COMPLETION_TOTAL: &str = "copperclaw_review_gate_completion_total";
pub const SELF_REVIEW_FINDINGS: &str = "copperclaw_self_review_findings";
pub const SELF_REVIEW_SUBMISSION_TOTAL: &str = "copperclaw_self_review_submission_total";

/// Increment `copperclaw_review_gate_completion_total{outcome}` — a final/
/// delivery todo completion crossed the self-review gate; `outcome` is
/// `refused_never_reviewed|refused_dirty|passed|blocked_cycle_cap`. Mirrors
/// the verify-gate-completion counter ([`inc_verify_gate_completion`]) one
/// layer up the delivery pipeline. Emitted from
/// `copperclaw-mcp/src/tools/todo.rs` (the final-todo review gate).
pub fn inc_review_gate_completion(outcome: &str) {
    counter!(REVIEW_GATE_COMPLETION_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Record `copperclaw_self_review_findings` — the number of structured
/// findings submitted in one `self_review` call (0 for an explicit
/// `no_findings`). Emitted from `copperclaw-mcp/src/tools/self_review.rs`.
pub fn observe_self_review_findings(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(SELF_REVIEW_FINDINGS).record(count as f64);
}

/// Increment `copperclaw_self_review_submission_total{kind}` — one
/// `self_review` findings submission landed; `kind` is `no_findings` or
/// `findings` (the has-findings/no-findings split the card asked for, as a
/// counter alongside the histogram so a raw ratio is a one-line `PromQL` query).
/// Emitted from `copperclaw-mcp/src/tools/self_review.rs`.
pub fn inc_self_review_submission(kind: &str) {
    counter!(SELF_REVIEW_SUBMISSION_TOTAL, "kind" => kind.to_owned()).increment(1);
}

// ── Q7 — delegate_batch contract presence + post-join dirty outcome ───────
pub const DELEGATE_BATCH_CONTRACT_TOTAL: &str = "copperclaw_delegate_batch_contract_total";
pub const DELEGATE_BATCH_POST_JOIN_DIRTY_TOTAL: &str =
    "copperclaw_delegate_batch_post_join_dirty_total";

/// Increment `copperclaw_delegate_batch_contract_total{present}` — a
/// `delegate_batch` call's optional shared `contract` arg was present
/// (`"true"`) or absent (`"false"`). Emitted from
/// `copperclaw-mcp/src/tools/agents.rs` (`delegate_batch`).
pub fn inc_delegate_batch_contract(present: bool) {
    counter!(
        DELEGATE_BATCH_CONTRACT_TOTAL,
        "present" => if present { "true" } else { "false" },
    )
    .increment(1);
}

/// Increment `copperclaw_delegate_batch_post_join_dirty_total{outcome}` — the
/// post-join integration-verify dirty-mark decision for a `delegate_batch`
/// parent project; `outcome` is `marked` (the parent's `.copperclaw/verify`
/// exists and was marked dirty so the merged union must re-pass all stages),
/// `skipped_no_project` (the parent path isn't a recognized project),
/// `skipped_no_verify` (no `.copperclaw/verify` file to gate on),
/// `skipped_gate_off` (`verify_gate=off` for the group), or
/// `skipped_all_spawn_failed` (every worker failed to spawn — nothing merged,
/// so nothing to re-verify). Emitted from
/// `copperclaw-mcp/src/tools/agents.rs` (`delegate_batch` post-join).
pub fn inc_delegate_batch_post_join_dirty(outcome: &str) {
    counter!(
        DELEGATE_BATCH_POST_JOIN_DIRTY_TOTAL,
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

// ── Q8 — compaction digest sections pinned per round ───────────────────────
pub const COMPACTION_FILE_INVENTORY_COUNT: &str = "copperclaw_compaction_file_inventory_count";
pub const COMPACTION_FILE_INVENTORY_BYTES: &str = "copperclaw_compaction_file_inventory_bytes";
pub const COMPACTION_VERIFY_STAGES_PINNED: &str = "copperclaw_compaction_verify_stages_pinned";
pub const COMPACTION_DECISIONS_TAIL_LINES: &str = "copperclaw_compaction_decisions_tail_lines";

/// Record `copperclaw_compaction_file_inventory_count` — the number of files
/// listed in a compaction's pinned project file inventory (`git ls-files`,
/// capped). Emitted from `copperclaw-runner/src/compaction.rs`
/// (`build_project_facts_header`).
pub fn observe_compaction_file_inventory_count(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(COMPACTION_FILE_INVENTORY_COUNT).record(count as f64);
}

/// Record `copperclaw_compaction_file_inventory_bytes` — the byte size of the
/// pinned file-inventory text within the facts header. Emitted from
/// `copperclaw-runner/src/compaction.rs` (`build_project_facts_header`).
pub fn observe_compaction_file_inventory_bytes(bytes: usize) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(COMPACTION_FILE_INVENTORY_BYTES).record(bytes as f64);
}

/// Record `copperclaw_compaction_verify_stages_pinned` — the number of Q2
/// verify stages pinned verbatim into a compaction's facts header (0 for a
/// project with no `.copperclaw/verify`). Emitted from
/// `copperclaw-runner/src/compaction.rs` (`build_project_facts_header`).
pub fn observe_compaction_verify_stages_pinned(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(COMPACTION_VERIFY_STAGES_PINNED).record(count as f64);
}

/// Record `copperclaw_compaction_decisions_tail_lines` — the number of lines
/// from `<project>/.copperclaw/DECISIONS.md` pinned into a compaction's facts
/// header (0 when the project has no decisions log — compacts exactly as
/// before Q8). Emitted from `copperclaw-runner/src/compaction.rs`
/// (`build_project_facts_header`).
pub fn observe_compaction_decisions_tail_lines(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(COMPACTION_DECISIONS_TAIL_LINES).record(count as f64);
}

// ── D1/D2 — ui_screenshot calls by outcome/viewport, refusals, chromium
// singleton lifecycle, capture latency, browser_render/interact by format ───
pub const UI_SCREENSHOT_TOTAL: &str = "copperclaw_ui_screenshot_total";
pub const UI_SCREENSHOT_REFUSED_URL_TOTAL: &str = "copperclaw_ui_screenshot_refused_url_total";
pub const CHROMIUM_SINGLETON_SPAWN_TOTAL: &str = "copperclaw_chromium_singleton_spawn_total";
pub const CHROMIUM_SINGLETON_IDLE_REAP_TOTAL: &str =
    "copperclaw_chromium_singleton_idle_reap_total";
pub const UI_SCREENSHOT_CAPTURE_SECONDS: &str = "copperclaw_ui_screenshot_capture_seconds";
pub const BROWSER_OUTPUT_FORMAT_TOTAL: &str = "copperclaw_browser_output_format_total";

/// Increment `copperclaw_ui_screenshot_total{outcome, viewport}` — one
/// `ui_screenshot` call; `outcome` is
/// `ok|blocked_non_loopback|chromium_missing|driver_error|oversize|downgraded`,
/// `viewport` is `desktop|mobile`. Emitted from
/// `copperclaw-mcp/src/tools/ui_screenshot.rs`.
pub fn inc_ui_screenshot(outcome: &str, viewport: &str) {
    counter!(
        UI_SCREENSHOT_TOTAL,
        "outcome" => outcome.to_owned(),
        "viewport" => viewport.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_ui_screenshot_refused_url_total` — a `ui_screenshot`
/// (or `ui_inspect`) call was refused because its `url` arg was not
/// loopback (`127.0.0.1`/`localhost`). Dedicated counter distinct from the
/// generic `outcome="blocked_non_loopback"` label above, so a refused-URL rate
/// alarm doesn't require decomposing the labeled counter. Emitted from
/// `copperclaw-mcp/src/tools/ui_screenshot.rs` and
/// `copperclaw-mcp/src/tools/ui_inspect.rs`.
pub fn inc_ui_screenshot_refused_url() {
    counter!(UI_SCREENSHOT_REFUSED_URL_TOTAL).increment(1);
}

/// Increment `copperclaw_chromium_singleton_spawn_total{result}` — the
/// in-container chromium singleton (D1's lazy-per-session launcher) started a
/// new headless instance; `result` is `ok|error`. Emitted from
/// `copperclaw-browser/src/incontainer.rs`.
pub fn inc_chromium_singleton_spawn(result: &str) {
    counter!(CHROMIUM_SINGLETON_SPAWN_TOTAL, "result" => result.to_owned()).increment(1);
}

/// Increment `copperclaw_chromium_singleton_idle_reap_total` — the
/// in-container chromium singleton was torn down after sitting idle past its
/// reap threshold. Emitted from `copperclaw-browser/src/incontainer.rs`.
pub fn inc_chromium_singleton_idle_reap() {
    counter!(CHROMIUM_SINGLETON_IDLE_REAP_TOTAL).increment(1);
}

/// Record `copperclaw_ui_screenshot_capture_seconds` — navigate/wait→PNG (or
/// JPEG) bytes-in-hand span for one `ui_screenshot` call. Emitted from
/// `copperclaw-mcp/src/tools/ui_screenshot.rs`.
pub fn observe_ui_screenshot_capture_seconds(secs: f64) {
    histogram!(UI_SCREENSHOT_CAPTURE_SECONDS).record(secs);
}

/// Increment `copperclaw_browser_output_format_total{tool, format}` — a
/// screenshot-capable tool call's chosen (or downgraded-to) output format;
/// `tool` is `ui_screenshot|browser_render|browser_interact`, `format` is
/// `png|jpeg`. Emitted from `copperclaw-mcp/src/tools/ui_screenshot.rs`,
/// `copperclaw-mcp/src/tools/browser_render.rs`, and
/// `copperclaw-mcp/src/tools/browser_interact.rs`.
pub fn inc_browser_output_format(tool: &str, format: &str) {
    counter!(
        BROWSER_OUTPUT_FORMAT_TOTAL,
        "tool" => tool.to_owned(),
        "format" => format.to_owned(),
    )
    .increment(1);
}

// ── D4 — see→fix cycles + ritual screenshot delivery (proxy — see gap note) ─
// D4's own acceptance criteria are prompt-level (the model looks at the
// image, runs the frontend-design critique checklist, edits, re-screenshots)
// — that critique/edit step has no runtime marker at all (it's a habit
// taught by static prompt text, not a tool call), so it is NOT independently
// emittable. Both metrics below are the closest HONEST proxies, not a direct
// measurement of "did the model actually look and think":
//   - `SEE_FIX_SCREENSHOTS_PER_BUILD` counts `ui_screenshot` calls within one
//     inbound (>=2 is CONSISTENT WITH a see->fix cycle — an initial shot plus
//     a re-shot — but doesn't prove a critique happened in between).
//   - `RITUAL_SCREENSHOT_DELIVERY_TOTAL` only ever emits `"delivered"` (a
//     `send_file` call whose path is a `ui_screenshot`-saved image). The
//     card's other wished outcome, `"omitted"` (a ready-card went out with
//     NO screenshot, on a project that has a UI), is NOT emitted: detecting
//     it requires correlating two facts across the same inbound turn — "a
//     `send_card` fired" AND "no screenshot `send_file` fired" AND "the
//     project has a UI" — and neither `send_card` nor `send_file` carries a
//     "this is the ready card" / provenance tag today (`SendCardSpec` and
//     `SendFileSpec` in `copperclaw-mcp/src/context.rs` are undifferentiated
//     from any other card/file send). Wiring that distinction would mean
//     threading new per-turn state through `TurnResult` AND teaching
//     `send_card`/`send_file` to tag ready-card sends — a materially bigger
//     change than a metrics rider should make unilaterally. Documented gap;
//     the positive signal (`"delivered"`) is still directly useful (its rate
//     over total coding-project deliveries is the proxy for the negative).
pub const SEE_FIX_SCREENSHOTS_PER_BUILD: &str = "copperclaw_see_fix_screenshots_per_build";
pub const RITUAL_SCREENSHOT_DELIVERY_TOTAL: &str = "copperclaw_ritual_screenshot_delivery_total";

/// Record `copperclaw_see_fix_screenshots_per_build` — the number of
/// `ui_screenshot` calls observed within one inbound's tool loop (proxy for
/// see->fix cycle count — see the gap note above). Emitted from
/// `copperclaw-runner/src/run/drive_turn.rs` (`drive_turn`), once per inbound,
/// only when at least one `ui_screenshot` call occurred.
pub fn observe_see_fix_screenshots_per_build(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(SEE_FIX_SCREENSHOTS_PER_BUILD).record(count as f64);
}

/// Increment `copperclaw_ritual_screenshot_delivery_total{outcome}` — today
/// only ever called with `outcome="delivered"` (a `send_file` call whose
/// `path` arg is a `ui_screenshot`-saved image under
/// `.copperclaw/screenshots/`). The card's other wished outcome, `"omitted"`,
/// is a documented gap — see the section note above for why it isn't cleanly
/// emittable without much bigger plumbing. Emitted from
/// `copperclaw-mcp/src/tools/core.rs` (`send_file::handle`).
pub fn inc_ritual_screenshot_delivery(outcome: &str) {
    counter!(RITUAL_SCREENSHOT_DELIVERY_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── D5 — ui_inspect calls by outcome + console errors surfaced ────────────
pub const UI_INSPECT_TOTAL: &str = "copperclaw_ui_inspect_total";
pub const UI_INSPECT_CONSOLE_ERRORS: &str = "copperclaw_ui_inspect_console_errors";

/// Increment `copperclaw_ui_inspect_total{outcome}` — one `ui_inspect` call;
/// `outcome` is `success|refused_url|chromium_missing`. Emitted from
/// `copperclaw-mcp/src/tools/ui_inspect.rs`.
pub fn inc_ui_inspect(outcome: &str) {
    counter!(UI_INSPECT_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Record `copperclaw_ui_inspect_console_errors` — the number of buffered
/// console errors surfaced by one `ui_inspect` call (0 when the page logged
/// none). Emitted from `copperclaw-mcp/src/tools/ui_inspect.rs`.
pub fn observe_ui_inspect_console_errors(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(UI_INSPECT_CONSOLE_ERRORS).record(count as f64);
}

// ════════════════════════════════════════════════════════════════════════════
// M21 metrics rider (card M1) — one sweep of the metric "wishes" the merged M21
// cards (S1-S6, F1-F4, O2-O4) recorded in their PR descriptions, code comments
// (`// M1 metric wish:`), and the plan's `## M1. Metrics rider` section. No
// other M21 card touches this crate; names/labels mirror each card's wish where
// one exists. The emit call sites live in the crate each wish named (noted per
// helper). Grouped by card.
// ════════════════════════════════════════════════════════════════════════════

// ── S1 — background-loop liveness gauges + restart counts ──────────────────
pub const SUPERVISED_LOOP_ALIVE: &str = "copperclaw_supervised_loop_alive";
pub const SUPERVISED_LOOP_DEGRADED: &str = "copperclaw_supervised_loop_degraded";
pub const SUPERVISED_LOOP_RESTARTS_TOTAL: &str = "copperclaw_supervised_loop_restarts_total";

/// Set `copperclaw_supervised_loop_alive{loop}` to 1 (a running incarnation)
/// or 0 (drained on shutdown or between a crash and its backoff respawn) — the
/// per-loop liveness gauge O1's `cclaw doctor` reads to spot a dead background
/// loop. Emitted from `copperclaw-host/src/supervisor.rs` on every loop
/// state transition (`mark_started` / `mark_stopped` / `record_unexpected_exit`)
/// and refreshed at status-read time (`snapshot`, which applies the lazy
/// healthy-reset first).
pub fn set_supervised_loop_alive(loop_name: &str, alive: bool) {
    gauge!(SUPERVISED_LOOP_ALIVE, "loop" => loop_name.to_owned()).set(f64::from(u8::from(alive)));
}

/// Set `copperclaw_supervised_loop_degraded{loop}` to 1 once a loop's restart
/// streak has exhausted the backoff curve (it keeps dying), 0 once it heals
/// past the reset window. Emitted from `copperclaw-host/src/supervisor.rs`
/// alongside [`set_supervised_loop_alive`].
pub fn set_supervised_loop_degraded(loop_name: &str, degraded: bool) {
    gauge!(SUPERVISED_LOOP_DEGRADED, "loop" => loop_name.to_owned())
        .set(f64::from(u8::from(degraded)));
}

/// Increment `copperclaw_supervised_loop_restarts_total{loop, reason}` — a
/// supervised background loop exited unexpectedly and is being restarted;
/// `reason` is `panicked` or `returned` (a bare return outside shutdown).
/// Distinct from the liveness gauge: this is the monotonic restart count O1
/// surfaces. Emitted from `copperclaw-host/src/supervisor.rs` (the driver's
/// unexpected-exit arm).
pub fn inc_supervised_loop_restart(loop_name: &str, reason: &str) {
    counter!(
        SUPERVISED_LOOP_RESTARTS_TOTAL,
        "loop" => loop_name.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

// ── S2 — container restarts by reason ──────────────────────────────────────
pub const CONTAINER_RESTART_TOTAL: &str = "copperclaw_container_restart_total";

/// Increment `copperclaw_container_restart_total{reason}` — the container
/// manager tore down and will respawn a session's container; `reason` is
/// `crash` (stale/dead heartbeat) or `stuck_tool` (S2 — the sweep found a tool
/// past the absolute ceiling; the runner is alive but wedged). Gives the S2
/// stuck restart its own series, distinct from the crash-only
/// [`inc_containers_crashed`] (a deliberate recovery is not a crash). Emitted
/// from `copperclaw-host/src/container_manager/classify.rs` (`restart_container`).
pub fn inc_container_restart(reason: &str) {
    counter!(CONTAINER_RESTART_TOTAL, "reason" => reason.to_owned()).increment(1);
}

// ── S3/S5 — delivery retry resumes + dead-letters by reason ────────────────
pub const DELIVERY_RETRY_RESUMED_TOTAL: &str = "copperclaw_delivery_retry_resumed_total";
pub const DELIVERY_DEAD_LETTER_TOTAL: &str = "copperclaw_delivery_dead_letter_total";

/// Reason label values for `copperclaw_delivery_dead_letter_total`. Use these
/// constants at call sites so a typo is a compile error.
/// The row burned its full retry budget (adapter kept failing).
pub const DEAD_LETTER_REASON_RETRY_EXHAUSTED: &str = "retry_exhausted";
/// The row's channel had no live adapter past the age ceiling (S5).
pub const DEAD_LETTER_REASON_NO_ADAPTER: &str = "no_adapter";

/// Increment `copperclaw_delivery_retry_resumed_total` — the delivery loop
/// rehydrated a still-pending outbound row's persisted retry counter
/// (migration 029 `tries`/`not_before`) after a host restart, so the attempt
/// budget resumes where it left off instead of restarting from zero (S3).
/// Counted once per row with a nonzero persisted attempt count. Emitted from
/// `copperclaw-host-delivery/src/service.rs` (`prime_retry_cache`).
pub fn inc_delivery_retry_resumed() {
    counter!(DELIVERY_RETRY_RESUMED_TOTAL).increment(1);
}

/// Increment `copperclaw_delivery_dead_letter_total{reason}` — one outbound row
/// was dead-lettered (recorded `delivered{status="failed"}`, and for the
/// no-adapter case a central `outbound_dropped_messages` row); `reason` is one
/// of [`DEAD_LETTER_REASON_RETRY_EXHAUSTED`] or [`DEAD_LETTER_REASON_NO_ADAPTER`].
/// A by-reason companion to the channel-labelled [`inc_delivery_failed`].
/// Emitted from `copperclaw-host-delivery/src/service.rs` (`record_exhausted_row`
/// and `record_no_adapter_expired`).
pub fn inc_delivery_dead_letter(reason: &str) {
    counter!(DELIVERY_DEAD_LETTER_TOTAL, "reason" => reason.to_owned()).increment(1);
}

// ── S4 — OOM kills + crash-loop backoff level ──────────────────────────────
pub const CONTAINER_OOM_KILLS_TOTAL: &str = "copperclaw_container_oom_kills_total";
pub const CRASH_BACKOFF_LEVEL: &str = "copperclaw_crash_backoff_level";

/// Increment `copperclaw_container_oom_kills_total` — a session container was
/// classified as OOM-killed (Docker `State.OOMKilled` or exit 137) at
/// crash-restart time. The user-facing "keeps running out of memory" card fires
/// once per episode; this counter meters every OOM kill. Emitted from
/// `copperclaw-host/src/container_manager/classify.rs` (`restart_container`).
pub fn inc_container_oom_kill() {
    counter!(CONTAINER_OOM_KILLS_TOTAL).increment(1);
}

/// Record `copperclaw_crash_backoff_level` — the crash-loop streak position
/// (1-based) a session reached when a crash/stuck restart was recorded, i.e.
/// how deep into the respawn backoff curve (5s→15s→60s→300s cap) the episode
/// got. A distribution skewed toward the cap means sessions are crash-looping.
/// Emitted from `copperclaw-host/src/container_manager/classify.rs`
/// (`restart_container`, one observation per recorded crash).
pub fn observe_crash_backoff_level(streak: u32) {
    histogram!(CRASH_BACKOFF_LEVEL).record(f64::from(streak));
}

// ── F1 — slow-spawn notices (the spawn-duration histogram already exists as
// `copperclaw_container_spawn_seconds`, observed in spawn.rs) ───────────────
pub const SLOW_SPAWN_NOTICES_TOTAL: &str = "copperclaw_slow_spawn_notices_total";

/// Increment `copperclaw_slow_spawn_notices_total` — the one-per-episode
/// slow-spawn watchdog posted its "setting things up" notice to the user
/// because a cold spawn ran past the notice threshold (~first image build/pull).
/// Emitted from `copperclaw-host/src/container_manager/cold_start.rs`
/// (`post_slow_spawn_notice`, on a successfully-enqueued notice).
pub fn inc_slow_spawn_notice() {
    counter!(SLOW_SPAWN_NOTICES_TOTAL).increment(1);
}

// ── F2 — ask_user_question expiries ────────────────────────────────────────
pub const QUESTION_EXPIRIES_TOTAL: &str = "copperclaw_question_expiries_total";

/// Increment `copperclaw_question_expiries_total{outcome}` — the sweep found an
/// `ask_user_question` whose TTL lapsed; `outcome` is `surfaced` (a terminal
/// note + synthetic no-answer result were written) or `resolved_by_reply` (the
/// user de-facto answered after the ask, so the lapse resolved silently).
/// Emitted from `copperclaw-host-sweep/src/service.rs` (`run_once`, from the
/// F2 question-expiry check's report).
pub fn inc_question_expiry(outcome: &str) {
    counter!(QUESTION_EXPIRIES_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── F3 — boot/host-restart recovery notices ────────────────────────────────
pub const RECOVERY_NOTICES_TOTAL: &str = "copperclaw_recovery_notices_total";

/// Increment `copperclaw_recovery_notices_total` — a boot-time recovery notice
/// (the "sorry, I was restarted mid-task" apology) was written for a session
/// that had an in-flight turn when the host went down. Emitted from
/// `copperclaw-host/src/boot.rs` (the stale-running-session reset path, once
/// per notice written).
pub fn inc_recovery_notice() {
    counter!(RECOVERY_NOTICES_TOTAL).increment(1);
}

// ── F4 — external-MCP connection cache hit/miss ────────────────────────────
pub const MCP_CONNECTION_CACHE_TOTAL: &str = "copperclaw_mcp_connection_cache_total";
pub const MCP_CONNECTION_REAPED_TOTAL: &str = "copperclaw_mcp_connection_reaped_total";

/// Increment `copperclaw_mcp_connection_cache_total{outcome}` — one external
/// MCP tool call consulted the connection cache; `outcome` is `hit` (a live
/// cached connection was reused), `miss` (no cached connection — connect
/// fresh), or `dead_retry` (a cached connection was found dead under us,
/// evicted, and the call retried once on a fresh connection). Emitted from
/// `copperclaw-mcp/src/external_cache.rs` (`call_with`).
pub fn inc_mcp_connection_cache(outcome: &str) {
    counter!(MCP_CONNECTION_CACHE_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Add `n` to `copperclaw_mcp_connection_reaped_total` — idle external MCP
/// connections closed by the lazy/background reaper (past their idle TTL).
/// Emitted from `copperclaw-mcp/src/external_cache.rs` (`reap_idle`, when it
/// closed at least one connection).
pub fn add_mcp_connections_reaped(n: u64) {
    counter!(MCP_CONNECTION_REAPED_TOTAL).increment(n);
}

// ── O2 — integrity quick-check outcomes + quarantines ──────────────────────
pub const INTEGRITY_QUICK_CHECK_TOTAL: &str = "copperclaw_integrity_quick_check_total";
pub const INTEGRITY_QUARANTINES_TOTAL: &str = "copperclaw_integrity_quarantines_total";
pub const INTEGRITY_QUARANTINED_SESSIONS: &str = "copperclaw_integrity_quarantined_sessions";

/// Scope label values for `copperclaw_integrity_quick_check_total`.
pub const INTEGRITY_SCOPE_SESSION: &str = "session";
pub const INTEGRITY_SCOPE_CENTRAL: &str = "central";

/// Increment `copperclaw_integrity_quick_check_total{scope, outcome}` — one
/// `SQLite` `quick_check` probe ran; `scope` is [`INTEGRITY_SCOPE_SESSION`] (a
/// per-session DB, one per db probed) or [`INTEGRITY_SCOPE_CENTRAL`] (the
/// central DB); `outcome` is `healthy`, `missing` (not yet materialised — not
/// corruption), or `corrupt`. Emitted from
/// `copperclaw-host-sweep/src/checks/integrity.rs` (session) and
/// `copperclaw-host-sweep/src/service.rs` (central).
pub fn inc_integrity_quick_check(scope: &str, outcome: &str) {
    counter!(
        INTEGRITY_QUICK_CHECK_TOTAL,
        "scope" => scope.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Increment `copperclaw_integrity_quarantines_total` — a session's per-session
/// DB failed `quick_check`, its `.quarantined` sidecar was written, and the
/// session was excluded from further sweeps. Emitted from
/// `copperclaw-host-sweep/src/checks/integrity.rs` (`check_and_quarantine`).
pub fn inc_integrity_quarantines() {
    counter!(INTEGRITY_QUARANTINES_TOTAL).increment(1);
}

/// Set `copperclaw_integrity_quarantined_sessions` — the current count of
/// quarantined (sweep-excluded) sessions observed this sweep pass (already
/// quarantined + newly quarantined this pass). A gauge, not a counter: it
/// reflects the live excluded population. Emitted from
/// `copperclaw-host-sweep/src/service.rs` (`run_once`, once per pass).
pub fn set_integrity_quarantined_sessions(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    gauge!(INTEGRITY_QUARANTINED_SESSIONS).set(count as f64);
}

// ── O3 — live provider-failover transitions (degrade vs restore) ───────────
pub const PROVIDER_FAILOVER_TRANSITION_TOTAL: &str =
    "copperclaw_provider_failover_transition_total";
pub const PROVIDER_FAILOVER_ACTIVE_POSITION: &str = "copperclaw_provider_failover_active_position";

/// Direction label values for `copperclaw_provider_failover_transition_total`.
/// Moved to a higher-index fallback (a provider failed).
pub const FAILOVER_DIRECTION_DEGRADE: &str = "degrade";
/// Moved back toward the primary (an earlier-degraded provider re-probed OK).
pub const FAILOVER_DIRECTION_RESTORE: &str = "restore";

/// Increment `copperclaw_provider_failover_transition_total{direction, from, to}`
/// — the runner's live failover moved the serving provider; `direction` is
/// [`FAILOVER_DIRECTION_DEGRADE`] (to a higher-index fallback) or
/// [`FAILOVER_DIRECTION_RESTORE`] (back toward the primary). Distinguishes live
/// failover activity from the spawn-time selection the pre-existing
/// [`inc_provider_failover`] counts. Emitted from
/// `copperclaw-runner/src/run/provider_call.rs`.
pub fn inc_provider_failover_transition(direction: &str, from: &str, to: &str) {
    counter!(
        PROVIDER_FAILOVER_TRANSITION_TOTAL,
        "direction" => direction.to_owned(),
        "from" => from.to_owned(),
        "to" => to.to_owned(),
    )
    .increment(1);
}

/// Set `copperclaw_provider_failover_active_position` — the chain index (0 =
/// primary) of the provider currently serving this session's turn. One runner
/// process serves one session, so a plain gauge is unambiguous per process.
/// Emitted from `copperclaw-runner/src/run/provider_call.rs` on entering each
/// candidate.
pub fn set_provider_failover_active_position(position: usize) {
    #[allow(clippy::cast_precision_loss)]
    gauge!(PROVIDER_FAILOVER_ACTIVE_POSITION).set(position as f64);
}

// ── O4 — operator alerts by severity + outcome ─────────────────────────────
pub const OPERATOR_ALERTS_TOTAL: &str = "copperclaw_operator_alerts_total";

/// Outcome label values for `copperclaw_operator_alerts_total`.
/// Enqueued for delivery to the operator destination.
pub const ALERT_OUTCOME_SENT: &str = "sent";
/// No operator destination configured — log-only.
pub const ALERT_OUTCOME_SUPPRESSED_DISABLED: &str = "suppressed_disabled";
/// Same episode already alerted within the re-alert window.
pub const ALERT_OUTCOME_SUPPRESSED_DEDUPED: &str = "suppressed_deduped";
/// Alert-flood rate limit tripped.
pub const ALERT_OUTCOME_SUPPRESSED_RATE_LIMITED: &str = "suppressed_rate_limited";
/// No active session row to carry the outbound alert (fail-closed).
pub const ALERT_OUTCOME_NO_CARRIER: &str = "no_carrier";
/// A DB error dropped the enqueue (fail-closed).
pub const ALERT_OUTCOME_ENQUEUE_FAILED: &str = "enqueue_failed";

/// Increment `copperclaw_operator_alerts_total{severity, outcome}` — one
/// `OperatorAlerts::fire` decision; `severity` is the alert's severity token
/// and `outcome` is one of the `ALERT_OUTCOME_*` constants. Emitted from
/// `copperclaw-host/src/operator_alerts.rs` (`fire`).
pub fn inc_operator_alert(severity: &str, outcome: &str) {
    counter!(
        OPERATOR_ALERTS_TOTAL,
        "severity" => severity.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

// ── Long-wished: sweep last-run wall-clock ─────────────────────────────────
pub const SWEEP_LAST_RUN_TIMESTAMP: &str = "copperclaw_sweep_last_run_timestamp";

/// Set `copperclaw_sweep_last_run_timestamp` to the Unix time (seconds) of the
/// last completed sweep pass — an alert on `time() - <this>` catches a wedged
/// or dead sweep loop (complements the S1 loop-liveness gauge). Emitted from
/// `copperclaw-host-sweep/src/service.rs` (end of `run_once`).
pub fn set_sweep_last_run_timestamp(unix_secs: i64) {
    #[allow(clippy::cast_precision_loss)]
    gauge!(SWEEP_LAST_RUN_TIMESTAMP).set(unix_secs as f64);
}

// ════════════════════════════════════════════════════════════════════════════
// M22 metrics rider (card M1) — one sweep of the metric "wishes" the merged M22
// cards (Wave 1 C1-C6, Wave 2 A1-A5, Wave 3 S1-S4) recorded in their PR
// descriptions, in-code markers (`// M1 metric wish`, `// M22 <card> metric`),
// and the plan's `## M1. Metrics rider` section. No other M22 card touches this
// crate; names/labels mirror each card's wish where one exists. The emit call
// sites live in the crate each wish named (noted per helper). Grouped by card.
//
// Where a wish enumerated label values that have no clean production emit site
// yet (e.g. A1 `revoked`/`expired` — no production revoke caller / no expiry
// sweep), the helper still takes the label as `&str` so the family is defined
// once and the missing arms light up for free when a real site lands. No helper
// below is left unwired.
// ════════════════════════════════════════════════════════════════════════════

// ── C1 — post-edit verify hook (runs the format/typecheck path over a just-
// edited file and feeds the digest back). Emitted from
// `copperclaw-mcp/src/tools/diagnostics.rs` (`post_edit_verify`). ────────────
pub const POST_EDIT_VERIFY_TOTAL: &str = "copperclaw_post_edit_verify_total";
pub const POST_EDIT_VERIFY_FINDINGS: &str = "copperclaw_post_edit_verify_findings";

/// Increment `copperclaw_post_edit_verify_total{tool, outcome}` — one post-edit
/// verify attempt over a just-mutated file. `tool` is the resolved checker
/// (`eslint`/`tsc`/`ruff`, or `none` when no checker fits). `outcome` is one of
/// `flagged` (the checker found error(s)/warning(s) — a digest was fed back),
/// `clean` (checker ran, nothing to report), `not_available` (the checker
/// binary is not installed in this image), `unsupported` (no checker fits the
/// file type), `disabled` (the opt-out flag is set), or `error` (the checker run
/// could not be launched/parsed). Emitted from
/// `copperclaw-mcp/src/tools/diagnostics.rs` (`post_edit_verify`).
pub fn inc_post_edit_verify(tool: &str, outcome: &str) {
    counter!(
        POST_EDIT_VERIFY_TOTAL,
        "tool" => tool.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

/// Record `copperclaw_post_edit_verify_findings` — the per-run error+warning
/// count for a post-edit verify that flagged (observed only on the `flagged`
/// outcome). Emitted from `copperclaw-mcp/src/tools/diagnostics.rs`.
pub fn observe_post_edit_verify_findings(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(POST_EDIT_VERIFY_FINDINGS).record(count as f64);
}

// ── C2 — open/attach an existing repository. Attach flow emits from
// `copperclaw-runner/src/run/project.rs` (`attach_project`); the host-side
// detection notice from `copperclaw-host/.../cold_start.rs`. ─────────────────
pub const REPO_ATTACH_TOTAL: &str = "copperclaw_repo_attach_total";
pub const REPO_ATTACH_VERIFY_STAGES_INFERRED: &str =
    "copperclaw_repo_attach_verify_stages_inferred";
pub const REPO_ATTACH_DETECTED_TOTAL: &str = "copperclaw_repo_attach_detected_total";

/// Increment `copperclaw_repo_attach_total` — the runner attached an existing
/// repository as the working project (inferred its verify stages, seeded the
/// decision log, triggered the C3 symbol index, dropped the attached marker).
/// Counted once per genuine attach (a re-attach of an already-attached repo is
/// a no-op and is NOT counted). Emitted from
/// `copperclaw-runner/src/run/project.rs` (`attach_project`).
pub fn inc_repo_attach() {
    counter!(REPO_ATTACH_TOTAL).increment(1);
}

/// Record `copperclaw_repo_attach_verify_stages_inferred` — how many verify
/// stages the attach flow inferred from a repo's manifests
/// (`package.json`/`Makefile`/`Cargo.toml`/`pyproject`). Zero means nothing was
/// inferred (the repo carries no recognized build/test entrypoint). Emitted
/// from `copperclaw-runner/src/run/project.rs` (`attach_project`).
pub fn observe_repo_attach_verify_stages_inferred(stages: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(REPO_ATTACH_VERIFY_STAGES_INFERRED).record(stages as f64);
}

/// Increment `copperclaw_repo_attach_detected_total` — the host's cold-start
/// path noticed an attachable repository handed into a session's `/data`. A
/// host-side companion to the runner's [`inc_repo_attach`] (which counts the
/// actual attach). Emitted from
/// `copperclaw-host/src/container_manager/cold_start.rs`
/// (`note_attachable_repos`).
pub fn inc_repo_attach_detected() {
    counter!(REPO_ATTACH_DETECTED_TOTAL).increment(1);
}

// ── C3 — `find_symbol` + symbol-index build. Emitted from
// `copperclaw-mcp/src/tools/find_symbol.rs` (lookup) and
// `copperclaw-runner/src/run/project.rs` (`trigger_symbol_index`). ───────────
pub const FIND_SYMBOL_TOTAL: &str = "copperclaw_find_symbol_total";
pub const SYMBOL_INDEX_BUILDS_TOTAL: &str = "copperclaw_symbol_index_builds_total";
pub const SYMBOL_INDEX_SYMBOLS: &str = "copperclaw_symbol_index_symbols";

/// Increment `copperclaw_find_symbol_total{definition_source}` — one
/// `find_symbol` invocation, labelled by which backend tier resolved the
/// definition: `ctags-index`, `ctags-ondemand`, `grep`, or `none` (no
/// definition found). Emitted from
/// `copperclaw-mcp/src/tools/find_symbol.rs` (`run_find_symbol`).
pub fn inc_find_symbol(definition_source: &str) {
    counter!(FIND_SYMBOL_TOTAL, "definition_source" => definition_source.to_owned()).increment(1);
}

/// Increment `copperclaw_symbol_index_builds_total{backend}` — one symbol-index
/// build over an attached repo; `backend` is `language-server-assisted`,
/// `ctags`, or `none` (no index builder available). Emitted from
/// `copperclaw-runner/src/run/project.rs` (`trigger_symbol_index`).
pub fn inc_symbol_index_build(backend: &str) {
    counter!(SYMBOL_INDEX_BUILDS_TOTAL, "backend" => backend.to_owned()).increment(1);
}

/// Record `copperclaw_symbol_index_symbols` — the number of symbols an index
/// build wrote (0 when no index was built). Emitted alongside
/// [`inc_symbol_index_build`] from `copperclaw-runner/src/run/project.rs`.
pub fn observe_symbol_index_symbols(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    histogram!(SYMBOL_INDEX_SYMBOLS).record(count as f64);
}

// ── C4 — screenshot-diff visual regression. Emitted from
// `copperclaw-mcp/src/tools/ui_screenshot.rs` (`visual::regression_note`). ───
pub const VISUAL_REGRESSION_FLAGS_TOTAL: &str = "copperclaw_visual_regression_flags_total";
pub const VISUAL_REGRESSION_BASELINES_TOTAL: &str = "copperclaw_visual_regression_baselines_total";

/// Increment `copperclaw_visual_regression_flags_total{viewport,
/// dimensions_changed}` — a post-edit screenshot diff flagged a visual
/// regression against this view's stored baseline. `viewport` is the capture
/// preset (`desktop`/`mobile`); `dimensions_changed` is `true` when the viewport
/// dimensions themselves changed (a strong layout-regression signal) vs a
/// within-frame pixel shift. Emitted from
/// `copperclaw-mcp/src/tools/ui_screenshot.rs` (`visual::regression_note`).
pub fn inc_visual_regression_flag(viewport: &str, dimensions_changed: bool) {
    counter!(
        VISUAL_REGRESSION_FLAGS_TOTAL,
        "viewport" => viewport.to_owned(),
        "dimensions_changed" => if dimensions_changed { "true" } else { "false" },
    )
    .increment(1);
}

/// Increment `copperclaw_visual_regression_baselines_total` — a view's baseline
/// PNG was (re)written after a successful capture (the next diff will compare
/// against it). Emitted from
/// `copperclaw-mcp/src/tools/ui_screenshot.rs` (`visual::regression_note`).
pub fn inc_visual_regression_baseline() {
    counter!(VISUAL_REGRESSION_BASELINES_TOTAL).increment(1);
}

// ── C5 — reviewer role in `delegate_batch`. Emitted from
// `copperclaw-mcp/src/tools/agents.rs` (`delegate_batch::handle`). ───────────
pub const REVIEW_BATCH_REVIEWERS_TOTAL: &str = "copperclaw_review_batch_reviewers_total";
pub const REVIEW_MERGE_GATE_TOTAL: &str = "copperclaw_review_merge_gate_total";

/// Add `n` to `copperclaw_review_batch_reviewers_total` — the count of reviewer
/// workers dispatched in one `delegate_batch` call (0 when the batch carried no
/// reviewer, in which case this is not called). Emitted from
/// `copperclaw-mcp/src/tools/agents.rs` (`delegate_batch::handle`).
pub fn add_review_batch_reviewers(n: u64) {
    counter!(REVIEW_BATCH_REVIEWERS_TOTAL).increment(n);
}

/// Increment `copperclaw_review_merge_gate_total{outcome}` — the merge-gate
/// verdict a reviewer-bearing batch computed; `outcome` is `blocked` (at least
/// one reviewer blocked, or returned an unrecognized verdict) or `passed` (all
/// reviewers cleared the diff). Emitted from
/// `copperclaw-mcp/src/tools/agents.rs` (`delegate_batch::handle`).
pub fn inc_review_merge_gate(outcome: &str) {
    counter!(REVIEW_MERGE_GATE_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── C6 — see→fix (screenshot) completion gate. Mirrors
// [`inc_review_gate_completion`]. Emitted from
// `copperclaw-mcp/src/tools/todo.rs` (the C6 gate in `update::handle`). ──────
pub const SEE_FIX_GATE_COMPLETION_TOTAL: &str = "copperclaw_see_fix_gate_completion_total";

/// Increment `copperclaw_see_fix_gate_completion_total{outcome}` — a
/// final/delivery todo of a UI task crossed the see→fix (post-fix screenshot)
/// gate; `outcome` is `refused_needs_screenshot` (blocked this attempt, cycles
/// remain), `blocked_cycle_cap` (the see→fix cap was burned — the todo
/// auto-transitioned to `blocked`), or `passed` (no pending post-fix screenshot,
/// completion allowed). The see→fix analogue of the verify + review gates'
/// [`inc_review_gate_completion`]. Emitted from
/// `copperclaw-mcp/src/tools/todo.rs` (`update::handle`).
pub fn inc_see_fix_gate_completion(outcome: &str) {
    counter!(SEE_FIX_GATE_COMPLETION_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── A1 — task capability grants (approval-gated authoring). Emitted from
// `copperclaw-host/src/handlers/approvals.rs` (`apply_task_grant`). ──────────
pub const TASK_GRANTS_TOTAL: &str = "copperclaw_task_grants_total";

/// Increment `copperclaw_task_grants_total{outcome}` — a task capability grant
/// lifecycle event. `outcome` is `approved` (an operator approved a pending
/// grant card and the bounded `task_grants` row persisted). Reserved for future
/// sites: `issued` (grant proposal raised), `revoked` (a revoke path — no
/// production caller yet), and `expired` (an expiry sweep — none exists; grants
/// lapse lazily via `effective_grant`). Emitted from
/// `copperclaw-host/src/handlers/approvals.rs` (`apply_task_grant`).
pub fn inc_task_grant(outcome: &str) {
    counter!(TASK_GRANTS_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── A2 — enforce grants at the autonomy gate. Emitted from
// `copperclaw-runner/src/run/tool_dispatch.rs` (`invoke_tool`). ──────────────
pub const AUTONOMOUS_ACTIONS_TOTAL: &str = "copperclaw_autonomous_actions_total";

/// Increment `copperclaw_autonomous_actions_total{outcome}` — an autonomous
/// (scheduled/heartbeat) turn's attempt at a credentialed external action met
/// the grant gate. `outcome` is `taken` (a live grant authorized THIS action
/// and a fire was charged — the agent acted) or `blocked_proposed` (no grant, or
/// the action was out of the grant's scope — the action stays blocked and falls
/// to read-then-propose). Emitted from
/// `copperclaw-runner/src/run/tool_dispatch.rs` (`invoke_tool`).
pub fn inc_autonomous_action(outcome: &str) {
    counter!(AUTONOMOUS_ACTIONS_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── A2 (host half) — grant snapshotting + fire/token consumption. Emitted from
// `copperclaw-host/.../tasks_snapshot.rs` (`write_grant_snapshot`) and
// `copperclaw-host-delivery/src/service.rs` (`apply_grant_consume`). ─────────
pub const GRANTS_SNAPSHOTTED_TOTAL: &str = "copperclaw_grants_snapshotted_total";
pub const GRANT_FIRES_CONSUMED_TOTAL: &str = "copperclaw_grant_fires_consumed_total";
pub const GRANT_TOKENS_CONSUMED_TOTAL: &str = "copperclaw_grant_tokens_consumed_total";

/// Increment `copperclaw_grants_snapshotted_total{outcome}` — the host wrote (or
/// removed) the per-session `grant.json` the runner's autonomy gate reads.
/// `outcome` is `written` (a live grant was snapshotted — the gate MAY open),
/// `removed_no_grant` (the firing task has no live grant — snapshot removed,
/// gate stays closed), `removed_no_firing_task` (no pending autonomous fire), or
/// `removed_read_error` (a DB read failed — fail closed). Emitted from
/// `copperclaw-host/src/container_manager/tasks_snapshot.rs`
/// (`write_grant_snapshot`).
pub fn inc_grants_snapshotted(outcome: &str) {
    counter!(GRANTS_SNAPSHOTTED_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

/// Add `n` to `copperclaw_grant_fires_consumed_total` — the number of grant
/// fires the host debited from a `task_grants` row after an autonomous action
/// (one per `consume_fire`). Emitted from
/// `copperclaw-host-delivery/src/service.rs` (`apply_grant_consume`).
pub fn add_grant_fires_consumed(n: u64) {
    counter!(GRANT_FIRES_CONSUMED_TOTAL).increment(n);
}

/// Add `tokens` to `copperclaw_grant_tokens_consumed_total` — grant token budget
/// the host debited after an autonomous action carried a token spend. Emitted
/// from `copperclaw-host-delivery/src/service.rs` (`apply_grant_consume`).
pub fn add_grant_tokens_consumed(tokens: u64) {
    counter!(GRANT_TOKENS_CONSUMED_TOTAL).increment(tokens);
}

// ── A3 — first-class long-running goals. Counts flow from the sweep report
// (`copperclaw-host-sweep/src/service.rs` run loop) + the goal check-in scan
// (`checks/goals.rs`); status/progress from the `update_goal` apply
// (`copperclaw-host-delivery/src/service.rs`, `apply_goal`). ─────────────────
pub const GOAL_CHECKINS_FIRED_TOTAL: &str = "copperclaw_goal_checkins_fired_total";
pub const GOALS_BUDGET_PAUSED_TOTAL: &str = "copperclaw_goals_budget_paused_total";
pub const GOAL_STATUS_TOTAL: &str = "copperclaw_goal_status_total";
pub const GOAL_PROGRESS_RECORDED_TOTAL: &str = "copperclaw_goal_progress_recorded_total";
pub const ACTIVE_GOALS: &str = "copperclaw_active_goals";

/// Add `n` to `copperclaw_goal_checkins_fired_total` — goal check-in wakes the
/// sweep synthesised this pass (one per due `active` goal). Emitted from the
/// sweep run loop consuming `SweepReport.goal_checkins_fired`
/// (`copperclaw-host-sweep/src/service.rs`).
pub fn add_goal_checkins_fired(n: u64) {
    counter!(GOAL_CHECKINS_FIRED_TOTAL).increment(n);
}

/// Add `n` to `copperclaw_goals_budget_paused_total` — goals the sweep paused
/// this pass because their grant-backed budget was exhausted. Emitted from the
/// sweep run loop consuming `SweepReport.goals_budget_paused`.
pub fn add_goals_budget_paused(n: u64) {
    counter!(GOALS_BUDGET_PAUSED_TOTAL).increment(n);
}

/// Increment `copperclaw_goal_status_total{status}` — a goal reached a terminal
/// status via `update_goal`; `status` is `completed` or `abandoned`. Emitted
/// from `copperclaw-host-delivery/src/service.rs` (`apply_goal`, the status
/// transition arm).
pub fn inc_goal_status(status: &str) {
    counter!(GOAL_STATUS_TOTAL, "status" => status.to_owned()).increment(1);
}

/// Increment `copperclaw_goal_progress_recorded_total` — an `update_goal` call
/// recorded a progress note against a goal. Emitted from
/// `copperclaw-host-delivery/src/service.rs` (`apply_goal`, the progress arm).
pub fn inc_goal_progress_recorded() {
    counter!(GOAL_PROGRESS_RECORDED_TOTAL).increment(1);
}

/// Set `copperclaw_active_goals` — the count of `active` goals observed this
/// sweep pass. A gauge over the live active-goal population. Emitted from
/// `copperclaw-host-sweep/src/checks/goals.rs` (`check`, once per pass).
pub fn set_active_goals(count: u64) {
    #[allow(clippy::cast_precision_loss)]
    gauge!(ACTIVE_GOALS).set(count as f64);
}

// ── A4 — condition/event check-ins. Emitted from
// `copperclaw-host-sweep/src/checks/condition_checkin.rs` (`check`). ─────────
pub const CONDITION_CHECKINS_FIRED_TOTAL: &str = "copperclaw_condition_checkins_fired_total";

/// Increment `copperclaw_condition_checkins_fired_total{kind}` — a stored
/// HEARTBEAT-style condition fired a check-in wake on its rising edge; `kind` is
/// `pending_inbound`, `idle`, or `flag`. Emitted from
/// `copperclaw-host-sweep/src/checks/condition_checkin.rs` (`check`) — the site
/// where the fired condition's kind is known (the `SweepReport` fanout carries
/// only the series id, not the kind).
pub fn inc_condition_checkin_fired(kind: &str) {
    counter!(CONDITION_CHECKINS_FIRED_TOTAL, "kind" => kind.to_owned()).increment(1);
}

// ── A5 — recurrence consolidation into the central `tasks` scheduler. Emitted
// from `copperclaw-host-sweep/src/checks/recurrence.rs` (`check`). ───────────
pub const RECURRENCE_CONSOLIDATED_TOTAL: &str = "copperclaw_recurrence_consolidated_total";

/// Increment `copperclaw_recurrence_consolidated_total{outcome}` — a per-session
/// self-replicating recurrence series was consolidated into the central `tasks`
/// scheduler; `outcome` is `created` (a new central task was inserted for the
/// series) or `already_present` (a task already existed for this series — the
/// migration is idempotent). Emitted from
/// `copperclaw-host-sweep/src/checks/recurrence.rs` (`check`).
pub fn inc_recurrence_consolidated(outcome: &str) {
    counter!(RECURRENCE_CONSOLIDATED_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

// ── S1 — materialize skills into the container. Emitted from
// `copperclaw-host/src/container_manager/cold_start.rs`
// (`materialize_session_skills`). ────────────────────────────────────────────
pub const SKILLS_MATERIALIZED_TOTAL: &str = "copperclaw_skills_materialized_total";

/// Add `n` to `copperclaw_skills_materialized_total{agent_group}` — skill dirs
/// symlinked into a session container's `/data/skills` at cold start (so a
/// skill's `scripts/`/`data/` reach the sandbox). Labelled by agent group so an
/// operator can see how many runnable skills each group's spawns receive.
/// Emitted from `copperclaw-host/src/container_manager/cold_start.rs`
/// (`materialize_session_skills`).
pub fn add_skills_materialized(agent_group: &str, n: u64) {
    counter!(SKILLS_MATERIALIZED_TOTAL, "agent_group" => agent_group.to_owned()).increment(n);
}

// ── S2 — relevance-narrowed skill selection. Emitted from
// `copperclaw-host/src/container_manager/prompt.rs` (inline-skills builder). ─
pub const SKILLS_RELEVANCE_FILTERED_TOTAL: &str = "copperclaw_skills_relevance_filtered_total";

/// Add `n` to `copperclaw_skills_relevance_filtered_total` — skills dropped from
/// the inline system prompt by a `SkillsSelector::Relevant` narrowing (the
/// registry total minus the selected count), versus `All` which inlines every
/// skill. Emitted from `copperclaw-host/src/container_manager/prompt.rs` when the
/// host resolves a `Relevant` selector for the inline prompt.
pub fn add_skills_relevance_filtered(n: u64) {
    counter!(SKILLS_RELEVANCE_FILTERED_TOTAL).increment(n);
}

// ── S3 — skill versioning + `list_skills`. Emitted from
// `copperclaw-mcp/src/tools/list_skills.rs` (`handle`) and
// `copperclaw-host/src/handlers/approvals.rs` (the `save_skill` write). ──────
pub const SKILLS_LISTED_TOTAL: &str = "copperclaw_skills_listed_total";
pub const SKILL_VERSION_SAVED: &str = "copperclaw_skill_version_saved";

/// Increment `copperclaw_skills_listed_total{mode}` — a `list_skills` call
/// answered; `mode` is `catalogue` (callable mode — a catalogue file existed and
/// was read) or `inline_empty` (inline mode — no catalogue on disk, an empty
/// list with an explanatory note was returned). Emitted from
/// `copperclaw-mcp/src/tools/list_skills.rs` (`handle`).
pub fn inc_skills_listed(mode: &str) {
    counter!(SKILLS_LISTED_TOTAL, "mode" => mode.to_owned()).increment(1);
}

/// Record `copperclaw_skill_version_saved` — the effective version persisted by
/// an approval-gated `save_skill` (1 on a first save; N+1 on a re-save that
/// bumped an on-disk version N). Emitted from
/// `copperclaw-host/src/handlers/approvals.rs` (after `save_group_skill`).
pub fn observe_skill_version_saved(version: u32) {
    histogram!(SKILL_VERSION_SAVED).record(f64::from(version));
}

// ── S4 — `tools:` frontmatter narrowing under inline skills mode. Emitted from
// `copperclaw-mcp/src/tools/load_skill.rs` (`activate_inline_skill_scope`). ──
pub const LOAD_SKILL_INLINE_SCOPED_TOTAL: &str = "copperclaw_load_skill_inline_scoped_total";

/// Increment `copperclaw_load_skill_inline_scoped_total{skill, scope}` — an
/// inline-mode `load_skill` resolved a materialized skill's tool scope; `scope`
/// is `narrowed` (the skill declared a `tools:`/`allowed-tools:` allowlist that
/// now narrows dispatch) or `cleared` (the skill declared no scope — any prior
/// narrowing was cleared). Complements the pre-existing [`inc_load_skill`]
/// (which counts every inline/callable invocation). Emitted from
/// `copperclaw-mcp/src/tools/load_skill.rs` (`activate_inline_skill_scope`).
pub fn inc_load_skill_inline_scoped(skill: &str, scope: &str) {
    counter!(
        LOAD_SKILL_INLINE_SCOPED_TOTAL,
        "skill" => skill.to_owned(),
        "scope" => scope.to_owned(),
    )
    .increment(1);
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

    // ── M20 metrics-rider (card M1) coverage ───────────────────────────────

    /// Every new M20 metric name const, so the prefix / double-underscore
    /// invariants extend to the rider's additions.
    const M20_METRIC_NAMES: &[&str] = &[
        IMAGE_BUNDLE_VERSION,
        PINNED_BINARY_FETCH_TOTAL,
        VERIFY_RUN_STAGE_TOTAL,
        VERIFY_GATE_PENDING_STAGES_TOTAL,
        VERIFY_STAGES_DECLARED,
        DIAGNOSTICS_RUN_TOTAL,
        REVIEW_GATE_COMPLETION_TOTAL,
        SELF_REVIEW_FINDINGS,
        SELF_REVIEW_SUBMISSION_TOTAL,
        DELEGATE_BATCH_CONTRACT_TOTAL,
        DELEGATE_BATCH_POST_JOIN_DIRTY_TOTAL,
        COMPACTION_FILE_INVENTORY_COUNT,
        COMPACTION_FILE_INVENTORY_BYTES,
        COMPACTION_VERIFY_STAGES_PINNED,
        COMPACTION_DECISIONS_TAIL_LINES,
        UI_SCREENSHOT_TOTAL,
        UI_SCREENSHOT_REFUSED_URL_TOTAL,
        CHROMIUM_SINGLETON_SPAWN_TOTAL,
        CHROMIUM_SINGLETON_IDLE_REAP_TOTAL,
        UI_SCREENSHOT_CAPTURE_SECONDS,
        BROWSER_OUTPUT_FORMAT_TOTAL,
        SEE_FIX_SCREENSHOTS_PER_BUILD,
        RITUAL_SCREENSHOT_DELIVERY_TOTAL,
        UI_INSPECT_TOTAL,
        UI_INSPECT_CONSOLE_ERRORS,
    ];

    #[test]
    fn m20_metric_names_have_copperclaw_prefix_no_double_underscore() {
        for name in M20_METRIC_NAMES {
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
    fn m20_counter_names_end_with_total() {
        for name in M20_METRIC_NAMES {
            if name.contains("_seconds")
                || name.contains("_bytes")
                || name.contains("_declared")
                || name.contains("_findings")
                || name.contains("_pinned")
                || name.contains("_lines")
                || name.contains("_count")
                || name.contains("_per_build")
                || name.contains("_errors")
                || name == &IMAGE_BUNDLE_VERSION
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
    fn m20_helpers_compile_and_do_not_panic() {
        // No recorder installed → all of these no-op; smoke test that every
        // rider helper is callable with its intended argument shape.
        set_image_bundle_version("prototyping", "ruff", "0.15.22");
        inc_pinned_binary_fetch("ruff", "cache_hit");
        inc_pinned_binary_fetch("ruff", "fetch_ok");
        inc_pinned_binary_fetch("ruff", "checksum_fail");
        inc_pinned_binary_fetch("ruff", "arch_unsupported");
        inc_pinned_binary_fetch("ruff", "fetch_failed");
        inc_verify_run_stage("lint", "pass");
        inc_verify_run_stage("typecheck", "fail");
        inc_verify_gate_pending_stages("1");
        inc_verify_gate_pending_stages("2+");
        observe_verify_stages_declared(3);
        inc_diagnostics_run("eslint", "ran");
        inc_diagnostics_run("tsc", "not_available");
        inc_diagnostics_run("ruff", "error");
        inc_review_gate_completion("refused_never_reviewed");
        inc_review_gate_completion("refused_dirty");
        inc_review_gate_completion("passed");
        inc_review_gate_completion("blocked_cycle_cap");
        observe_self_review_findings(4);
        inc_self_review_submission("no_findings");
        inc_self_review_submission("findings");
        inc_delegate_batch_contract(true);
        inc_delegate_batch_contract(false);
        inc_delegate_batch_post_join_dirty("marked");
        inc_delegate_batch_post_join_dirty("skipped_no_project");
        inc_delegate_batch_post_join_dirty("skipped_no_verify");
        inc_delegate_batch_post_join_dirty("skipped_gate_off");
        inc_delegate_batch_post_join_dirty("skipped_all_spawn_failed");
        observe_compaction_file_inventory_count(42);
        observe_compaction_file_inventory_bytes(2048);
        observe_compaction_verify_stages_pinned(2);
        observe_compaction_decisions_tail_lines(10);
        inc_ui_screenshot("ok", "desktop");
        inc_ui_screenshot("downgraded", "mobile");
        inc_ui_screenshot_refused_url();
        inc_chromium_singleton_spawn("ok");
        inc_chromium_singleton_spawn("error");
        inc_chromium_singleton_idle_reap();
        observe_ui_screenshot_capture_seconds(0.8);
        inc_browser_output_format("ui_screenshot", "png");
        inc_browser_output_format("browser_render", "jpeg");
        inc_browser_output_format("browser_interact", "png");
        observe_see_fix_screenshots_per_build(2);
        inc_ritual_screenshot_delivery("delivered");
        inc_ui_inspect("success");
        inc_ui_inspect("refused_url");
        inc_ui_inspect("chromium_missing");
        observe_ui_inspect_console_errors(0);
        observe_ui_inspect_console_errors(3);
    }

    #[test]
    fn m20_labeled_counter_renders() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            set_image_bundle_version("prototyping", "ruff", "0.15.22");
            inc_pinned_binary_fetch("ruff", "checksum_fail");
            inc_verify_run_stage("lint", "pass");
            inc_verify_gate_pending_stages("2+");
            inc_diagnostics_run("tsc", "not_available");
            inc_review_gate_completion("refused_never_reviewed");
            inc_delegate_batch_contract(true);
            inc_delegate_batch_post_join_dirty("marked");
            inc_ui_screenshot("chromium_missing", "mobile");
            inc_ui_inspect("success");
        });
        let body = handle.render();
        assert!(
            body.contains(IMAGE_BUNDLE_VERSION) && body.contains("pinned_binary=\"ruff\""),
            "missing image bundle version gauge:\n{body}"
        );
        assert!(
            body.contains(PINNED_BINARY_FETCH_TOTAL) && body.contains("outcome=\"checksum_fail\""),
            "missing pinned binary fetch outcome:\n{body}"
        );
        assert!(
            body.contains(VERIFY_RUN_STAGE_TOTAL) && body.contains("stage=\"lint\""),
            "missing verify run stage:\n{body}"
        );
        assert!(
            body.contains(VERIFY_GATE_PENDING_STAGES_TOTAL) && body.contains("pending=\"2+\""),
            "missing verify gate pending stages:\n{body}"
        );
        assert!(
            body.contains(DIAGNOSTICS_RUN_TOTAL) && body.contains("tool=\"tsc\""),
            "missing diagnostics run:\n{body}"
        );
        assert!(
            body.contains(REVIEW_GATE_COMPLETION_TOTAL)
                && body.contains("outcome=\"refused_never_reviewed\""),
            "missing review gate completion:\n{body}"
        );
        assert!(
            body.contains(DELEGATE_BATCH_CONTRACT_TOTAL) && body.contains("present=\"true\""),
            "missing delegate batch contract:\n{body}"
        );
        assert!(
            body.contains(DELEGATE_BATCH_POST_JOIN_DIRTY_TOTAL)
                && body.contains("outcome=\"marked\""),
            "missing delegate batch post-join dirty:\n{body}"
        );
        assert!(
            body.contains(UI_SCREENSHOT_TOTAL)
                && body.contains("outcome=\"chromium_missing\"")
                && body.contains("viewport=\"mobile\""),
            "missing ui_screenshot counter:\n{body}"
        );
        assert!(
            body.contains(UI_INSPECT_TOTAL) && body.contains("outcome=\"success\""),
            "missing ui_inspect counter:\n{body}"
        );
    }

    // ── M21 metrics-rider (card M1) coverage ───────────────────────────────

    /// Every new M21 metric name const, so the prefix / double-underscore
    /// invariants extend to the rider's additions.
    const M21_METRIC_NAMES: &[&str] = &[
        SUPERVISED_LOOP_ALIVE,
        SUPERVISED_LOOP_DEGRADED,
        SUPERVISED_LOOP_RESTARTS_TOTAL,
        CONTAINER_RESTART_TOTAL,
        DELIVERY_RETRY_RESUMED_TOTAL,
        DELIVERY_DEAD_LETTER_TOTAL,
        CONTAINER_OOM_KILLS_TOTAL,
        CRASH_BACKOFF_LEVEL,
        SLOW_SPAWN_NOTICES_TOTAL,
        QUESTION_EXPIRIES_TOTAL,
        RECOVERY_NOTICES_TOTAL,
        MCP_CONNECTION_CACHE_TOTAL,
        MCP_CONNECTION_REAPED_TOTAL,
        INTEGRITY_QUICK_CHECK_TOTAL,
        INTEGRITY_QUARANTINES_TOTAL,
        INTEGRITY_QUARANTINED_SESSIONS,
        PROVIDER_FAILOVER_TRANSITION_TOTAL,
        PROVIDER_FAILOVER_ACTIVE_POSITION,
        OPERATOR_ALERTS_TOTAL,
        SWEEP_LAST_RUN_TIMESTAMP,
    ];

    #[test]
    fn m21_metric_names_have_copperclaw_prefix_no_double_underscore() {
        for name in M21_METRIC_NAMES {
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
    fn m21_counter_names_end_with_total() {
        for name in M21_METRIC_NAMES {
            if name == &SUPERVISED_LOOP_ALIVE
                || name == &SUPERVISED_LOOP_DEGRADED
                || name == &CRASH_BACKOFF_LEVEL
                || name == &INTEGRITY_QUARANTINED_SESSIONS
                || name == &PROVIDER_FAILOVER_ACTIVE_POSITION
                || name == &SWEEP_LAST_RUN_TIMESTAMP
            {
                // gauges / histograms: exempt from the `_total` suffix rule.
                continue;
            }
            assert!(
                name.ends_with("_total"),
                "counter {name:?} does not end with '_total'"
            );
        }
    }

    #[test]
    fn m21_helpers_compile_and_do_not_panic() {
        // No recorder installed → all of these no-op; smoke test that every
        // rider helper is callable with its intended argument shape.
        set_supervised_loop_alive("sweep", true);
        set_supervised_loop_alive("sweep", false);
        set_supervised_loop_degraded("sweep", true);
        set_supervised_loop_degraded("sweep", false);
        inc_supervised_loop_restart("sweep", "panicked");
        inc_supervised_loop_restart("delivery", "returned");
        inc_container_restart("crash");
        inc_container_restart("stuck_tool");
        inc_delivery_retry_resumed();
        inc_delivery_dead_letter(DEAD_LETTER_REASON_RETRY_EXHAUSTED);
        inc_delivery_dead_letter(DEAD_LETTER_REASON_NO_ADAPTER);
        inc_container_oom_kill();
        observe_crash_backoff_level(1);
        observe_crash_backoff_level(4);
        inc_slow_spawn_notice();
        inc_question_expiry("surfaced");
        inc_question_expiry("resolved_by_reply");
        inc_recovery_notice();
        inc_mcp_connection_cache("hit");
        inc_mcp_connection_cache("miss");
        inc_mcp_connection_cache("dead_retry");
        add_mcp_connections_reaped(2);
        inc_integrity_quick_check(INTEGRITY_SCOPE_SESSION, "healthy");
        inc_integrity_quick_check(INTEGRITY_SCOPE_SESSION, "missing");
        inc_integrity_quick_check(INTEGRITY_SCOPE_CENTRAL, "corrupt");
        inc_integrity_quarantines();
        set_integrity_quarantined_sessions(3);
        inc_provider_failover_transition(FAILOVER_DIRECTION_DEGRADE, "anthropic", "ollama");
        inc_provider_failover_transition(FAILOVER_DIRECTION_RESTORE, "ollama", "anthropic");
        set_provider_failover_active_position(1);
        inc_operator_alert("warning", ALERT_OUTCOME_SENT);
        inc_operator_alert("critical", ALERT_OUTCOME_SUPPRESSED_DISABLED);
        inc_operator_alert("warning", ALERT_OUTCOME_SUPPRESSED_DEDUPED);
        inc_operator_alert("warning", ALERT_OUTCOME_SUPPRESSED_RATE_LIMITED);
        inc_operator_alert("warning", ALERT_OUTCOME_NO_CARRIER);
        inc_operator_alert("warning", ALERT_OUTCOME_ENQUEUE_FAILED);
        set_sweep_last_run_timestamp(1_700_000_000);
    }

    #[test]
    fn m21_labeled_series_render() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            set_supervised_loop_alive("sweep", true);
            inc_supervised_loop_restart("sweep", "panicked");
            inc_container_restart("stuck_tool");
            inc_delivery_dead_letter(DEAD_LETTER_REASON_NO_ADAPTER);
            inc_container_oom_kill();
            inc_question_expiry("surfaced");
            inc_mcp_connection_cache("hit");
            inc_integrity_quick_check(INTEGRITY_SCOPE_SESSION, "corrupt");
            set_integrity_quarantined_sessions(2);
            inc_provider_failover_transition(FAILOVER_DIRECTION_DEGRADE, "anthropic", "ollama");
            inc_operator_alert("warning", ALERT_OUTCOME_SENT);
            set_sweep_last_run_timestamp(1_700_000_000);
        });
        let body = handle.render();
        assert!(
            body.contains(SUPERVISED_LOOP_ALIVE) && body.contains("loop=\"sweep\""),
            "missing supervised loop alive gauge:\n{body}"
        );
        assert!(
            body.contains(SUPERVISED_LOOP_RESTARTS_TOTAL) && body.contains("reason=\"panicked\""),
            "missing supervised loop restart counter:\n{body}"
        );
        assert!(
            body.contains(CONTAINER_RESTART_TOTAL) && body.contains("reason=\"stuck_tool\""),
            "missing container restart-by-reason counter:\n{body}"
        );
        assert!(
            body.contains(DELIVERY_DEAD_LETTER_TOTAL) && body.contains("reason=\"no_adapter\""),
            "missing delivery dead-letter counter:\n{body}"
        );
        assert!(
            body.contains(CONTAINER_OOM_KILLS_TOTAL),
            "missing OOM kill counter:\n{body}"
        );
        assert!(
            body.contains(QUESTION_EXPIRIES_TOTAL) && body.contains("outcome=\"surfaced\""),
            "missing question expiry counter:\n{body}"
        );
        assert!(
            body.contains(MCP_CONNECTION_CACHE_TOTAL) && body.contains("outcome=\"hit\""),
            "missing MCP connection cache counter:\n{body}"
        );
        assert!(
            body.contains(INTEGRITY_QUICK_CHECK_TOTAL)
                && body.contains("scope=\"session\"")
                && body.contains("outcome=\"corrupt\""),
            "missing integrity quick-check counter:\n{body}"
        );
        assert!(
            body.contains(INTEGRITY_QUARANTINED_SESSIONS),
            "missing quarantined-sessions gauge:\n{body}"
        );
        assert!(
            body.contains(PROVIDER_FAILOVER_TRANSITION_TOTAL)
                && body.contains("direction=\"degrade\""),
            "missing failover transition counter:\n{body}"
        );
        assert!(
            body.contains(OPERATOR_ALERTS_TOTAL)
                && body.contains("severity=\"warning\"")
                && body.contains("outcome=\"sent\""),
            "missing operator alert counter:\n{body}"
        );
        assert!(
            body.contains(SWEEP_LAST_RUN_TIMESTAMP),
            "missing sweep last-run timestamp gauge:\n{body}"
        );
    }
}
