//! Integration proof for the M21 F4 external-MCP connection cache against a
//! real stdio MCP server.
//!
//! The stub server is this very test binary re-executed with
//! `COPPERCLAW_MCP_STUB_SERVER=1`: in that mode `main` appends its PID to the
//! connection log named by `COPPERCLAW_MCP_STUB_LOG` (one line per
//! connection, i.e. per process spawn) and serves the crate's own
//! [`copperclaw_mcp::build_server`] over stdio with a
//! [`copperclaw_mcp::MockToolContext`]. Each line in the log therefore *is*
//! one real connection setup — which is exactly what the cache exists to
//! avoid.
//!
//! `harness = false` (see `Cargo.toml`) because libtest owns stdout in
//! harnessed tests and the MCP protocol needs the child's stdout raw.
//!
//! Scenarios (the F4 acceptance list):
//! - N sequential calls open exactly one connection.
//! - A server restart mid-session (SIGKILL of the child) degrades to
//!   reconnect-and-retry, not an error.
//! - First-call behavior is byte-identical to the uncached
//!   [`copperclaw_mcp::call_external_tool`], for success and for connect
//!   failure.
//!
//! The small real sleeps here wait on OS process death, not on cache
//! timing — idle-reap timing is covered by the paused-clock unit tests in
//! `src/external_cache.rs`.

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use copperclaw_mcp::{McpConnectionCache, MockToolContext, ToolContext, call_external_tool};
use serde_json::{Value, json};

const STUB_SERVER_ENV: &str = "COPPERCLAW_MCP_STUB_SERVER";
const STUB_LOG_ENV: &str = "COPPERCLAW_MCP_STUB_LOG";

fn main() {
    if std::env::var_os(STUB_SERVER_ENV).is_some() {
        stub_server_main();
        return;
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        n_sequential_calls_open_exactly_one_connection().await;
        server_restart_mid_session_reconnects_and_retries().await;
        first_call_success_is_byte_identical_to_uncached().await;
        first_call_connect_failure_is_byte_identical_to_uncached().await;
    });
    println!("external_cache_stub: all scenarios passed");
}

// ── stub server mode ─────────────────────────────────────────────────────

/// Serve the crate's own MCP tool surface on stdio, logging this process's
/// PID to the connection log first so the parent can count connections (and
/// kill this exact process to simulate a server crash).
fn stub_server_main() {
    let log_path = std::env::var(STUB_LOG_ENV).expect("stub server needs the connection log path");
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("open connection log");
    writeln!(log, "{}", std::process::id()).expect("record connection");
    drop(log);

    let rt = tokio::runtime::Runtime::new().expect("stub tokio runtime");
    rt.block_on(async {
        use rmcp::service::ServiceExt as _;
        let ctx: Arc<dyn ToolContext> = Arc::new(MockToolContext::new());
        let server = copperclaw_mcp::build_server(ctx);
        let running = server
            .serve((tokio::io::stdin(), tokio::io::stdout()))
            .await
            .expect("serve stub MCP server on stdio");
        // Run until the client hangs up (or we are killed).
        let _ = running.waiting().await;
    });
}

// ── scenario helpers ─────────────────────────────────────────────────────

/// Build an `mcp_servers`-style entry that spawns this test binary in stub
/// mode, logging connections to `log_path`.
fn stub_entry(log_path: &std::path::Path) -> Value {
    let exe = std::env::current_exe().expect("current exe");
    json!({
        "command": exe.to_str().expect("utf-8 exe path"),
        "args": [],
        "env": {
            STUB_SERVER_ENV: "1",
            STUB_LOG_ENV: log_path.to_str().expect("utf-8 log path"),
        },
    })
}

/// PIDs recorded in the connection log — one per connection the stub
/// accepted (i.e. per process spawn).
fn connections(log_path: &std::path::Path) -> Vec<u32> {
    match std::fs::read_to_string(log_path) {
        Ok(text) => text
            .lines()
            .map(|l| l.trim().parse().expect("pid line"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Assert a successful `list_tasks` result shape.
fn assert_ok_result(out: &Value) {
    assert_eq!(
        out.get("is_error").and_then(Value::as_bool),
        Some(false),
        "expected a non-error tool result, got: {out}"
    );
}

// ── scenarios ────────────────────────────────────────────────────────────

async fn n_sequential_calls_open_exactly_one_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("connections.log");
    let entry = stub_entry(&log);
    let cache = McpConnectionCache::new(Duration::from_secs(300));

    for i in 0..5 {
        let out = cache
            .call("sess-count", &entry, "list_tasks", json!({}))
            .await
            .unwrap_or_else(|e| panic!("call {i} failed: {e}"));
        assert_ok_result(&out);
    }
    let conns = connections(&log);
    assert_eq!(
        conns.len(),
        1,
        "5 sequential calls must reuse one connection, got {conns:?}"
    );
    println!("scenario ok: 5 sequential calls opened exactly one connection");
}

async fn server_restart_mid_session_reconnects_and_retries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("connections.log");
    let entry = stub_entry(&log);
    let cache = McpConnectionCache::new(Duration::from_secs(300));

    let out = cache
        .call("sess-restart", &entry, "list_tasks", json!({}))
        .await
        .expect("first call");
    assert_ok_result(&out);
    let conns = connections(&log);
    assert_eq!(conns.len(), 1);

    // Simulate a server crash/restart: SIGKILL the live stub process. The
    // kernel closes its pipes at death, so the cached connection is broken.
    let pid = conns[0].to_string();
    let status = std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -9 {pid} failed");
    // Wait for OS-level process death (not cache timing): the pipes close at
    // death, making the next write fail deterministically.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let out = cache
        .call("sess-restart", &entry, "list_tasks", json!({}))
        .await
        .expect("a killed server must degrade to reconnect-and-retry, not an error");
    assert_ok_result(&out);
    assert_eq!(
        connections(&log).len(),
        2,
        "the retry must have opened exactly one fresh connection"
    );
    println!("scenario ok: server restart mid-session reconnected and retried");
}

async fn first_call_success_is_byte_identical_to_uncached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("connections.log");
    let entry = stub_entry(&log);

    let uncached = call_external_tool(&entry, "list_tasks", json!({}))
        .await
        .expect("uncached call");
    let cache = McpConnectionCache::new(Duration::from_secs(300));
    let cached_first = cache
        .call("sess-identical", &entry, "list_tasks", json!({}))
        .await
        .expect("cached first call");
    assert_eq!(
        uncached, cached_first,
        "a cache-miss first call must return exactly what the uncached path returns"
    );
    println!("scenario ok: first-call success result is byte-identical to uncached");
}

async fn first_call_connect_failure_is_byte_identical_to_uncached() {
    let entry = json!({
        "command": "/path/that/does/not/exist/copperclaw-f4-stub",
        "args": [],
    });
    let uncached = call_external_tool(&entry, "list_tasks", json!({}))
        .await
        .expect_err("uncached connect must fail");
    let cache = McpConnectionCache::new(Duration::from_secs(300));
    let cached_first = cache
        .call("sess-fail", &entry, "list_tasks", json!({}))
        .await
        .expect_err("cached-miss connect must fail");
    assert_eq!(
        uncached, cached_first,
        "a cache-miss connect failure must be the exact uncached error"
    );
    println!("scenario ok: first-call connect failure is byte-identical to uncached");
}
