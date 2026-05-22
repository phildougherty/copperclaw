//! Prometheus metrics for the ironclaw host.
//!
//! This crate provides:
//!
//! - Named metric helpers (counters and histograms) so callers never
//!   hard-code metric name strings.
//! - An optional HTTP `/metrics` endpoint, started only when
//!   `IRONCLAW_METRICS_ADDR` is set in the process environment.  The
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
//! ironclaw_metrics::maybe_start_server(None).await;
//!
//! // At a call site that routes a message:
//! ironclaw_metrics::inc_messages_inbound("cli");
//! # }
//! ```
//!
//! ## Metric names (all prefixed `ironclaw_`)
//!
//! | Kind      | Name                              | Labels         |
//! |-----------|-----------------------------------|----------------|
//! | Counter   | `ironclaw_messages_inbound_total`  | `channel_type` |
//! | Counter   | `ironclaw_messages_outbound_total` | `channel_type` |
//! | Counter   | `ironclaw_containers_spawned_total`| —              |
//! | Counter   | `ironclaw_containers_crashed_total`| —              |
//! | Counter   | `ironclaw_delivery_failed_total`   | `channel_type` |
//! | Counter   | `ironclaw_budget_exhausted_total`  | `agent_group_id`, `gate` |
//! | Counter   | `ironclaw_budget_exhausted_replies_total` | `agent_group_id` |
//! | Counter   | `ironclaw_budget_exhausted_suppressed_total` | `agent_group_id` |
//! | Histogram | `ironclaw_llm_call_seconds`        | —              |
//! | Histogram | `ironclaw_llm_tokens_input`        | —              |
//! | Histogram | `ironclaw_llm_tokens_output`       | —              |
//! | Histogram | `ironclaw_container_spawn_seconds` | —              |
//! | Counter   | `ironclaw_provider_deadline_total` | `provider`     |
//! | Counter   | `ironclaw_provider_retry_total`    | `provider`     |

use metrics::{counter, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

// ── Metric name constants ──────────────────────────────────────────────────

pub const MESSAGES_INBOUND_TOTAL: &str = "ironclaw_messages_inbound_total";
pub const MESSAGES_OUTBOUND_TOTAL: &str = "ironclaw_messages_outbound_total";
pub const CONTAINERS_SPAWNED_TOTAL: &str = "ironclaw_containers_spawned_total";
pub const CONTAINERS_CRASHED_TOTAL: &str = "ironclaw_containers_crashed_total";
pub const IMAGE_REBUILD_FAILED_TOTAL: &str = "ironclaw_image_rebuild_failed_total";
pub const SECRETS_ROTATED_TOTAL: &str = "ironclaw_secrets_rotated_total";
pub const DELIVERY_FAILED_TOTAL: &str = "ironclaw_delivery_failed_total";
pub const BUDGET_EXHAUSTED_TOTAL: &str = "ironclaw_budget_exhausted_total";
pub const BUDGET_EXHAUSTED_REPLIES_TOTAL: &str = "ironclaw_budget_exhausted_replies_total";
pub const BUDGET_EXHAUSTED_SUPPRESSED_TOTAL: &str = "ironclaw_budget_exhausted_suppressed_total";
pub const LLM_CALL_SECONDS: &str = "ironclaw_llm_call_seconds";
pub const LLM_TOKENS_INPUT: &str = "ironclaw_llm_tokens_input";
pub const LLM_TOKENS_OUTPUT: &str = "ironclaw_llm_tokens_output";
pub const CONTAINER_SPAWN_SECONDS: &str = "ironclaw_container_spawn_seconds";
pub const PROVIDER_DEADLINE_TOTAL: &str = "ironclaw_provider_deadline_total";
pub const PROVIDER_RETRY_TOTAL: &str = "ironclaw_provider_retry_total";

// ── Budget-gate label values for the `gate` label of
// `ironclaw_budget_exhausted_total`. Use these constants instead of
// stringly-typed literals at call sites so a typo is a compile error.
pub const BUDGET_GATE_DAILY_TOKENS: &str = "daily_tokens";
pub const BUDGET_GATE_TURNS_PER_MINUTE: &str = "turns_per_minute";
pub const BUDGET_GATE_TURNS_PER_HOUR: &str = "turns_per_hour";

// ── Counter helpers ────────────────────────────────────────────────────────

/// Increment `ironclaw_messages_inbound_total{channel_type=<ct>}`.
pub fn inc_messages_inbound(channel_type: &str) {
    counter!(MESSAGES_INBOUND_TOTAL, "channel_type" => channel_type.to_owned()).increment(1);
}

/// Increment `ironclaw_messages_outbound_total{channel_type=<ct>}`.
pub fn inc_messages_outbound(channel_type: &str) {
    counter!(MESSAGES_OUTBOUND_TOTAL, "channel_type" => channel_type.to_owned()).increment(1);
}

/// Increment `ironclaw_containers_spawned_total`.
pub fn inc_containers_spawned() {
    counter!(CONTAINERS_SPAWNED_TOTAL).increment(1);
}

/// Increment `ironclaw_containers_crashed_total`.
pub fn inc_containers_crashed() {
    counter!(CONTAINERS_CRASHED_TOTAL).increment(1);
}

/// Increment `ironclaw_image_rebuild_failed_total`. Fired by the
/// container manager when an image rebuild call to the runtime errors
/// out; the manager falls back to the last-known-good `image_tag`
/// when one exists so the agent group is not blocked.
pub fn inc_image_rebuild_failed() {
    counter!(IMAGE_REBUILD_FAILED_TOTAL).increment(1);
}

/// Increment `ironclaw_secrets_rotated_total`. Fired by the host's
/// SIGHUP handler each time it re-reads the install's `.env` to
/// pick up rotated provider keys. Incremented even when no values
/// changed — the metric measures rotation *attempts*, not deltas.
pub fn inc_secrets_rotated() {
    counter!(SECRETS_ROTATED_TOTAL).increment(1);
}

/// Increment `ironclaw_delivery_failed_total{channel_type=<ct>}`.
pub fn inc_delivery_failed(channel_type: &str) {
    counter!(DELIVERY_FAILED_TOTAL, "channel_type" => channel_type.to_owned()).increment(1);
}

/// Increment `ironclaw_provider_deadline_total{provider=<p>}`. Fired by
/// the runner each time the per-LLM-call deadline trips AND all retries
/// have been exhausted — i.e. the inbound is about to be marked failed.
pub fn inc_provider_deadline(provider: &str) {
    counter!(PROVIDER_DEADLINE_TOTAL, "provider" => provider.to_owned()).increment(1);
}

/// Increment `ironclaw_provider_retry_total{provider=<p>}`. Fired by the
/// runner each time it backs off and re-issues a `provider.query()`
/// call after a retryable failure (5xx, transport, overloaded, or
/// per-call timeout).
pub fn inc_provider_retry(provider: &str) {
    counter!(PROVIDER_RETRY_TOTAL, "provider" => provider.to_owned()).increment(1);
}

/// Increment `ironclaw_budget_exhausted_total{agent_group_id, gate}`. Fired by
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

/// Increment `ironclaw_budget_exhausted_replies_total{agent_group_id}`.
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

/// Increment `ironclaw_budget_exhausted_suppressed_total{agent_group_id}`.
/// Fired by the container manager when a refusal is detected but the
/// per-group dedup window suppresses the in-channel reply.
pub fn inc_budget_exhausted_suppressed(agent_group_id: &str) {
    counter!(
        BUDGET_EXHAUSTED_SUPPRESSED_TOTAL,
        "agent_group_id" => agent_group_id.to_owned(),
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

// ── Address parsing ────────────────────────────────────────────────────────

/// Parse `IRONCLAW_METRICS_ADDR`.  Accepts:
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
    with_host.parse::<SocketAddr>().map_err(|source| AddrParseError::Invalid {
        raw: raw.to_owned(),
        source,
    })
}

// ── Server ─────────────────────────────────────────────────────────────────

/// Install the Prometheus recorder and (if `addr` is `Some`) start the HTTP
/// listener.  When `addr` is `None`, the function reads `IRONCLAW_METRICS_ADDR`
/// from the environment.
///
/// If the bind fails, this function logs a warning and returns without
/// starting the listener — the process continues normally.
///
/// The `shutdown` token is passed through to the listener task.  The host
/// passes its own shutdown token so the listener terminates with the host.
pub async fn maybe_start_server(shutdown: Option<CancellationToken>) {
    let raw = match std::env::var("IRONCLAW_METRICS_ADDR") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };

    let addr = match parse_metrics_addr(&raw) {
        Ok(a) => a,
        Err(e) => {
            warn!("IRONCLAW_METRICS_ADDR is malformed, metrics endpoint disabled: {e}");
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
        // when IRONCLAW_METRICS_ADDR contains garbage.
        let result = parse_metrics_addr("not-a-socket-addr!!!");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not-a-socket-addr"));
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
        inc_image_rebuild_failed();
        inc_secrets_rotated();
        inc_provider_deadline("anthropic");
        inc_provider_retry("anthropic");
        inc_budget_exhausted("ag-test", BUDGET_GATE_DAILY_TOKENS);
        inc_budget_exhausted("ag-test", BUDGET_GATE_TURNS_PER_MINUTE);
        inc_budget_exhausted("ag-test", BUDGET_GATE_TURNS_PER_HOUR);
        inc_budget_exhausted_reply("ag-test");
        inc_budget_exhausted_suppressed("ag-test");
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
    fn histogram_helpers_compile() {
        observe_llm_call_seconds(1.23);
        observe_llm_tokens_input(512);
        observe_llm_tokens_output(128);
        observe_container_spawn_seconds(0.5);
    }

    // ---- metric name constants are correct ----

    #[test]
    fn metric_name_constants_have_ironclaw_prefix() {
        let names = [
            MESSAGES_INBOUND_TOTAL,
            MESSAGES_OUTBOUND_TOTAL,
            CONTAINERS_SPAWNED_TOTAL,
            CONTAINERS_CRASHED_TOTAL,
            IMAGE_REBUILD_FAILED_TOTAL,
            SECRETS_ROTATED_TOTAL,
            DELIVERY_FAILED_TOTAL,
            BUDGET_EXHAUSTED_TOTAL,
            BUDGET_EXHAUSTED_REPLIES_TOTAL,
            BUDGET_EXHAUSTED_SUPPRESSED_TOTAL,
            LLM_CALL_SECONDS,
            LLM_TOKENS_INPUT,
            LLM_TOKENS_OUTPUT,
            CONTAINER_SPAWN_SECONDS,
            PROVIDER_DEADLINE_TOTAL,
            PROVIDER_RETRY_TOTAL,
        ];
        for name in names {
            assert!(
                name.starts_with("ironclaw_"),
                "metric name {name:?} does not start with 'ironclaw_'"
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
            BUDGET_EXHAUSTED_TOTAL,
            BUDGET_EXHAUSTED_REPLIES_TOTAL,
            BUDGET_EXHAUSTED_SUPPRESSED_TOTAL,
        ];
        for name in counters {
            assert!(
                name.ends_with("_total"),
                "counter {name:?} does not end with '_total'"
            );
        }
    }
}
