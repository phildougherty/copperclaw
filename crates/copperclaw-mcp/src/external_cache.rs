//! Per-session connection cache for external MCP servers (M21 F4 — the
//! twice-deferred M17 B1b wish).
//!
//! Before this module every host-proxied external MCP tool call paid a fresh
//! connection setup — child-process spawn + MCP handshake for stdio servers,
//! TCP + SSE handshake for HTTP servers — roughly 1-2 seconds per call. Users
//! felt it as unexplained per-step latency in exactly the long multi-tool
//! tasks where waiting already hurts.
//!
//! [`McpConnectionCache`] keeps one live [`FilteredMcpClient`] per
//! `(session scope, server config)` pair and reuses it across sequential tool
//! calls. Three invariants, in order of importance:
//!
//! 1. **First-call behavior is byte-identical to the uncached path.** A cache
//!    miss runs exactly what [`crate::external::call_external_tool`] runs —
//!    [`connect_filtered`] then `call_tool` — and every error propagates
//!    unchanged, with no retry. A connect failure or a call failure on a
//!    brand-new connection looks precisely like it did before this cache
//!    existed.
//! 2. **A dead cached connection retries once fresh before erroring.** Only a
//!    transport-level failure ([`McpError::Transport`] — broken pipe,
//!    transport closed) on a *cached* connection triggers the retry: the
//!    entry is evicted, one fresh connection is made, and the call runs once
//!    more. If the fresh connect or the retried call fails, that error
//!    surfaces — the same outcome a fresh-per-call caller would have seen.
//!    `Timeout`, `RemoteError`, `Protocol`, and `ToolFiltered` are never
//!    retried: the remote may already have executed the tool, and replaying a
//!    side-effecting call is worse than surfacing the error.
//! 3. **Idle connections are reaped.** Every cache access lazily evicts
//!    entries idle past the TTL, and [`McpConnectionCache::spawn_reaper`]
//!    adds a background sweep so the last connection of a quiet session does
//!    not outlive its usefulness. Eviction is drop-based: dropping the last
//!    `Arc` cancels the rmcp service (its `DropGuard`) which kills a stdio
//!    child process (`ChildWithCleanup`) — an in-flight call holding its own
//!    `Arc` keeps the connection alive until it finishes.
//!
//! The cache key's config half is a *canonical* JSON fingerprint of the
//! server entry (recursively key-sorted), because the workspace builds
//! `serde_json` with `preserve_order` — raw `Value::to_string()` would treat
//! two identical configs with different key order as different servers. The
//! fingerprint (and the entry itself) can contain credentials, so neither is
//! ever logged; log lines carry only the scope and tool name.
//!
//! Timing uses `tokio::time::Instant`, so idle-reap behavior is fully
//! testable under `tokio::time::pause()` with zero real waits.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;
use tracing::debug;

use crate::client::FilteredMcpClient;
use crate::error::McpError;
use crate::external::connect_filtered;

/// Default idle TTL for cached connections. Long enough to span the model
/// "thinking" gaps between tool calls in a multi-step task (minutes on small
/// local models), short enough that a finished session's stdio children do
/// not linger on the host.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(300);

/// Connection factory seam. The production implementation is
/// [`connect_filtered`]; unit tests inject counting fakes so cache keying,
/// idle reap, and retry-once semantics can be proven without a transport.
#[async_trait::async_trait]
trait McpConnector: Send + Sync {
    /// Connect the given server entry and wrap it in its per-server filter.
    async fn connect(&self, entry: &Value) -> Result<FilteredMcpClient, McpError>;
}

/// The production connector: one fresh, filter-wrapped connection per call,
/// exactly what the uncached path uses.
struct RealConnector;

#[async_trait::async_trait]
impl McpConnector for RealConnector {
    async fn connect(&self, entry: &Value) -> Result<FilteredMcpClient, McpError> {
        connect_filtered(entry).await
    }
}

/// Cache key: the caller-supplied scope (the session id on the host-proxied
/// call path) crossed with the canonical fingerprint of the server config.
/// Including the whole entry in the fingerprint means a config edit (URL,
/// env, filter lists) naturally keys a new connection; the stale one ages
/// out through the idle reap.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    scope: String,
    config: String,
}

impl CacheKey {
    fn new(scope: &str, entry: &Value) -> Self {
        Self {
            scope: scope.to_owned(),
            config: config_fingerprint(entry),
        }
    }
}

/// One cached live connection. `last_used` marks the most recent checkout
/// (cache hit or insert), not call completion — a call longer than the idle
/// TTL can therefore see its entry reaped mid-flight, which is harmless: the
/// caller's `Arc` keeps the connection alive until the call returns.
struct CachedConnection {
    client: Arc<FilteredMcpClient>,
    last_used: Instant,
}

/// Per-session external-MCP connection cache. See the module docs for the
/// three behavioral invariants (byte-identical first call, retry-once on a
/// dead cached connection, idle reap).
pub struct McpConnectionCache {
    idle_ttl: Duration,
    inner: Mutex<HashMap<CacheKey, CachedConnection>>,
}

impl std::fmt::Debug for McpConnectionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnectionCache")
            .field("idle_ttl", &self.idle_ttl)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl McpConnectionCache {
    /// Build an empty cache whose entries are evicted once idle for
    /// `idle_ttl`. No background task is spawned — see
    /// [`Self::spawn_reaper`].
    #[must_use]
    pub fn new(idle_ttl: Duration) -> Self {
        Self {
            idle_ttl,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// The configured idle TTL.
    #[must_use]
    pub fn idle_ttl(&self) -> Duration {
        self.idle_ttl
    }

    /// Number of live cached connections.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the cache currently holds no connections.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Call `tool` on the external server described by `entry`, reusing the
    /// scope's cached connection when one is live. `scope` is the reuse
    /// boundary — the host-proxied call path passes the session id, so
    /// connections are shared within a session and never across sessions.
    pub async fn call(
        &self,
        scope: &str,
        entry: &Value,
        tool: &str,
        input: Value,
    ) -> Result<Value, McpError> {
        self.call_with(&RealConnector, scope, entry, tool, input)
            .await
    }

    /// [`Self::call`] with an injectable connector (the unit-test seam).
    async fn call_with(
        &self,
        connector: &dyn McpConnector,
        scope: &str,
        entry: &Value,
        tool: &str,
        input: Value,
    ) -> Result<Value, McpError> {
        let reaped = self.reap_idle();
        if reaped > 0 {
            // M21 F4 (M1 rider): reaped-connection count.
            copperclaw_metrics::add_mcp_connections_reaped(reaped as u64);
            debug!(reaped, "reaped idle external MCP connections");
        }
        let key = CacheKey::new(scope, entry);
        let Some(client) = self.checkout(&key) else {
            // Miss: byte-identical to the uncached path — connect fresh, call
            // once, propagate any error unchanged.
            copperclaw_metrics::inc_mcp_connection_cache("miss");
            debug!(scope, tool, "external MCP connection cache miss");
            return self
                .connect_and_call(connector, key, entry, tool, input)
                .await;
        };
        copperclaw_metrics::inc_mcp_connection_cache("hit");
        debug!(scope, tool, "external MCP connection cache hit");
        match client.call_tool(tool, input.clone()).await {
            Err(err) if is_connection_dead(&err) => {
                // The cached connection died under us (server restart, child
                // exit, broken pipe). Evict it and retry exactly once on a
                // fresh connection; any failure from here surfaces as-is.
                copperclaw_metrics::inc_mcp_connection_cache("dead_retry");
                debug!(
                    scope,
                    tool,
                    %err,
                    "cached external MCP connection is dead; retrying once on a fresh connection"
                );
                self.evict_if_same(&key, &client);
                self.connect_and_call(connector, key, entry, tool, input)
                    .await
            }
            outcome => outcome,
        }
    }

    /// Connect fresh, run the call once, and cache the connection unless the
    /// call itself proved it dead. A healthy connection is cached even when
    /// the call returns a non-transport error (`RemoteError`, `Timeout`,
    /// `ToolFiltered`) — the transport is fine; only the call failed.
    async fn connect_and_call(
        &self,
        connector: &dyn McpConnector,
        key: CacheKey,
        entry: &Value,
        tool: &str,
        input: Value,
    ) -> Result<Value, McpError> {
        let client = Arc::new(connector.connect(entry).await?);
        let outcome = client.call_tool(tool, input).await;
        let dead = matches!(&outcome, Err(err) if is_connection_dead(err));
        if !dead {
            self.insert(key, client);
        }
        outcome
    }

    /// Evict every connection idle for at least the TTL. Returns the number
    /// evicted. Called lazily on each cache access and periodically by the
    /// background reaper. The M21 M1 rider meters the reaped-connection count
    /// via `copperclaw_mcp_connection_reaped_total` at the `call_with` call
    /// site (which has the count in hand).
    pub fn reap_idle(&self) -> usize {
        let now = Instant::now();
        let mut map = self.lock();
        let before = map.len();
        map.retain(|_, conn| now.duration_since(conn.last_used) < self.idle_ttl);
        before - map.len()
    }

    /// Spawn a detached background task that reaps idle connections every
    /// half TTL (minimum 1s), so the last connection of a quiet session is
    /// closed even when no further call ever touches the cache. The task
    /// holds only a `Weak` reference and exits when the cache is dropped.
    /// Must be called from within a tokio runtime.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let period = (self.idle_ttl / 2).max(Duration::from_secs(1));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(cache) = weak.upgrade() else { break };
                let reaped = cache.reap_idle();
                if reaped > 0 {
                    debug!(
                        reaped,
                        "background reaper closed idle external MCP connections"
                    );
                }
            }
        });
    }

    /// Fetch the scope's cached connection, refreshing its idle clock.
    fn checkout(&self, key: &CacheKey) -> Option<Arc<FilteredMcpClient>> {
        let mut map = self.lock();
        map.get_mut(key).map(|conn| {
            conn.last_used = Instant::now();
            Arc::clone(&conn.client)
        })
    }

    /// Insert (or refresh) the connection for `key`.
    fn insert(&self, key: CacheKey, client: Arc<FilteredMcpClient>) {
        self.lock().insert(
            key,
            CachedConnection {
                client,
                last_used: Instant::now(),
            },
        );
    }

    /// Evict `key` only if it still maps to the exact connection the caller
    /// found dead — a concurrent call may already have replaced it with a
    /// healthy one, which must not be discarded.
    fn evict_if_same(&self, key: &CacheKey, client: &Arc<FilteredMcpClient>) {
        let mut map = self.lock();
        if map
            .get(key)
            .is_some_and(|conn| Arc::ptr_eq(&conn.client, client))
        {
            map.remove(key);
        }
    }

    /// Lock the map, recovering from a poisoned mutex (the map holds no
    /// invariants a panicked holder could have half-applied).
    fn lock(&self) -> MutexGuard<'_, HashMap<CacheKey, CachedConnection>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Whether an error means the *connection* is dead (as opposed to the call
/// merely failing). Only [`McpError::Transport`] qualifies: rmcp maps a
/// broken pipe / closed transport there. `Timeout` deliberately does not —
/// the remote may still be executing a side-effecting tool.
fn is_connection_dead(err: &McpError) -> bool {
    matches!(err, McpError::Transport(_))
}

/// Canonical JSON fingerprint of a server entry: objects recursively
/// key-sorted so key order never splits the cache. Never log this value —
/// it can contain credentials (`env`, `headers`).
fn config_fingerprint(entry: &Value) -> String {
    canonicalize(entry).to_string()
}

/// Rebuild `value` with every object's keys in sorted order. Required
/// because the workspace enables `serde_json/preserve_order` (insertion
/// order), which would make serialization order config-authoring order.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), canonicalize(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// The process-wide cache behind [`call_external_tool_cached`]. Initialized
/// on first use (from async context, so the reaper can spawn); one instance
/// for the host process's lifetime.
fn global_cache() -> &'static Arc<McpConnectionCache> {
    static GLOBAL: OnceLock<Arc<McpConnectionCache>> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let cache = Arc::new(McpConnectionCache::new(DEFAULT_IDLE_TTL));
        if tokio::runtime::Handle::try_current().is_ok() {
            cache.spawn_reaper();
        }
        cache
    })
}

/// Cached variant of [`crate::external::call_external_tool`]: connect the
/// configured server (or reuse `scope`'s live connection to it), enforce the
/// per-server filter, and call `tool` with `input`.
///
/// `scope` is the reuse boundary — pass the session id so connections are
/// reused within a session and never shared across sessions. Failure
/// behavior matches the uncached call exactly, except that a dead cached
/// connection is retried once on a fresh one before erroring.
pub async fn call_external_tool_cached(
    scope: &str,
    entry: &Value,
    tool: &str,
    input: Value,
) -> Result<Value, McpError> {
    global_cache().call(scope, entry, tool, input).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::client::{McpToolTransport, RemoteTool};
    use crate::filter::ToolFilter;

    /// A fake live connection: answers `call_tool` with a payload naming its
    /// connection id, or a `Transport` error once its shared `broken` flag is
    /// set (simulating a broken pipe on an established connection).
    struct FakeTransport {
        id: usize,
        broken: Arc<AtomicBool>,
        remote_error: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl McpToolTransport for FakeTransport {
        async fn list_tools(&self) -> Result<Vec<RemoteTool>, McpError> {
            Ok(Vec::new())
        }

        async fn list_all_tools(&self) -> Result<Vec<RemoteTool>, McpError> {
            Ok(Vec::new())
        }

        async fn call_tool(&self, name: &str, _input: Value) -> Result<Value, McpError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.broken.load(Ordering::SeqCst) {
                return Err(McpError::Transport("broken pipe".into()));
            }
            if self.remote_error.load(Ordering::SeqCst) {
                return Err(McpError::RemoteError {
                    code: -32000,
                    message: "tool exploded".into(),
                });
            }
            Ok(serde_json::json!({
                "is_error": false,
                "content": [{"type": "text", "text": format!("conn-{}:{name}", self.id)}],
            }))
        }
    }

    /// Handle onto one fake connection so a test can break it after the
    /// cache has stored it.
    struct FakeConn {
        broken: Arc<AtomicBool>,
        remote_error: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    /// Counting connector: hands out [`FakeTransport`]s (filter parsed from
    /// the entry, exactly like the real connector) and records every connect.
    #[derive(Default)]
    struct CountingConnector {
        connects: AtomicUsize,
        fail_connect: AtomicBool,
        born_broken: AtomicBool,
        conns: Mutex<Vec<FakeConn>>,
    }

    impl CountingConnector {
        fn connects(&self) -> usize {
            self.connects.load(Ordering::SeqCst)
        }

        fn conn(&self, idx: usize) -> (Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicUsize>) {
            let conns = self.conns.lock().unwrap();
            let c = &conns[idx];
            (
                Arc::clone(&c.broken),
                Arc::clone(&c.remote_error),
                Arc::clone(&c.calls),
            )
        }
    }

    #[async_trait::async_trait]
    impl McpConnector for CountingConnector {
        async fn connect(&self, entry: &Value) -> Result<FilteredMcpClient, McpError> {
            if self.fail_connect.load(Ordering::SeqCst) {
                return Err(McpError::Transport("connect refused".into()));
            }
            let id = self.connects.fetch_add(1, Ordering::SeqCst);
            let broken = Arc::new(AtomicBool::new(self.born_broken.load(Ordering::SeqCst)));
            let remote_error = Arc::new(AtomicBool::new(false));
            let calls = Arc::new(AtomicUsize::new(0));
            self.conns.lock().unwrap().push(FakeConn {
                broken: Arc::clone(&broken),
                remote_error: Arc::clone(&remote_error),
                calls: Arc::clone(&calls),
            });
            Ok(FilteredMcpClient::from_transport(
                Box::new(FakeTransport {
                    id,
                    broken,
                    remote_error,
                    calls,
                }),
                ToolFilter::from_server_entry(entry),
            ))
        }
    }

    fn entry() -> Value {
        serde_json::json!({"command": "/srv/tool", "env": {"KEY": "v"}})
    }

    fn text_of(v: &Value) -> &str {
        v["content"][0]["text"].as_str().unwrap()
    }

    // ── cache keying ─────────────────────────────────────────────────────

    #[test]
    fn fingerprint_is_key_order_independent() {
        let plain = serde_json::json!({
            "command": "/srv/tool",
            "args": ["--x"],
            "env": {"B": "2", "A": "1"},
        });
        let reordered = serde_json::json!({
            "env": {"A": "1", "B": "2"},
            "args": ["--x"],
            "command": "/srv/tool",
        });
        assert_eq!(config_fingerprint(&plain), config_fingerprint(&reordered));
        // Any value difference — including a nested env var — splits the key.
        let other_env = serde_json::json!({
            "command": "/srv/tool",
            "args": ["--x"],
            "env": {"B": "2", "A": "other"},
        });
        assert_ne!(config_fingerprint(&plain), config_fingerprint(&other_env));
        // Array order is data, not authoring noise: it must NOT be normalised.
        let args_yx = serde_json::json!({"command": "/srv/tool", "args": ["--y", "--x"]});
        let args_xy = serde_json::json!({"command": "/srv/tool", "args": ["--x", "--y"]});
        assert_ne!(config_fingerprint(&args_yx), config_fingerprint(&args_xy));
    }

    #[tokio::test]
    async fn sequential_calls_reuse_one_connection() {
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        for _ in 0..5 {
            let out = cache
                .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(text_of(&out), "conn-0:echo");
        }
        assert_eq!(conn.connects(), 1);
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn scope_is_part_of_the_key() {
        // Two sessions with the byte-identical server config must NOT share
        // a connection — the scope is the per-session boundary.
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        let a = cache
            .call_with(&conn, "sess-a", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        let b = cache
            .call_with(&conn, "sess-b", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(text_of(&a), "conn-0:echo");
        assert_eq!(text_of(&b), "conn-1:echo");
        assert_eq!(conn.connects(), 2);
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn different_config_gets_its_own_connection() {
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        let other = serde_json::json!({"command": "/srv/other-tool"});
        cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        cache
            .call_with(&conn, "sess-1", &other, "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(conn.connects(), 2);
        // And re-calling the first config still hits its cached connection.
        let again = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(text_of(&again), "conn-0:echo");
        assert_eq!(conn.connects(), 2);
    }

    #[tokio::test]
    async fn reordered_config_hits_the_same_connection() {
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        let a = serde_json::json!({"command": "/srv/tool", "env": {"A": "1", "B": "2"}});
        let b = serde_json::json!({"env": {"B": "2", "A": "1"}, "command": "/srv/tool"});
        cache
            .call_with(&conn, "sess-1", &a, "echo", serde_json::json!({}))
            .await
            .unwrap();
        cache
            .call_with(&conn, "sess-1", &b, "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(conn.connects(), 1);
    }

    // ── idle reap ────────────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn idle_connection_is_reaped_after_ttl() {
        let cache = McpConnectionCache::new(Duration::from_secs(60));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(cache.len(), 1);

        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(cache.reap_idle(), 0, "not yet idle past the TTL");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(cache.reap_idle(), 1);
        assert!(cache.is_empty());

        // The next call transparently reconnects.
        let out = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(text_of(&out), "conn-1:echo");
        assert_eq!(conn.connects(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn recently_used_connection_survives_reap() {
        let cache = McpConnectionCache::new(Duration::from_secs(60));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-old", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(40)).await;
        // Touch a second scope now; the first keeps aging.
        cache
            .call_with(&conn, "sess-new", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(30)).await;
        // sess-old is 70s idle (past TTL); sess-new only 30s.
        assert_eq!(cache.reap_idle(), 1);
        assert_eq!(cache.len(), 1);
        let out = cache
            .call_with(&conn, "sess-new", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(text_of(&out), "conn-1:echo", "survivor is sess-new's");
        assert_eq!(conn.connects(), 2, "no reconnect for the survivor");
    }

    #[tokio::test(start_paused = true)]
    async fn lazy_reap_runs_on_call() {
        // No explicit reap_idle(): a later call on a different key must
        // itself evict the expired entry.
        let cache = McpConnectionCache::new(Duration::from_secs(60));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-stale", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(61)).await;
        cache
            .call_with(&conn, "sess-live", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(cache.len(), 1, "stale entry evicted by the call itself");
    }

    #[tokio::test(start_paused = true)]
    async fn background_reaper_evicts_without_further_calls() {
        let cache = Arc::new(McpConnectionCache::new(Duration::from_secs(60)));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        cache.spawn_reaper();
        // Let the reaper task start and take its immediate first tick.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(61)).await;
        // Give the reaper a chance to run its due ticks (paused clock: no
        // real waiting happens here).
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(cache.is_empty(), "reaper must evict with no cache traffic");
    }

    // ── broken-pipe retry-once ───────────────────────────────────────────

    #[tokio::test]
    async fn dead_cached_connection_retries_once_and_succeeds() {
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        // Kill the live connection under the cache (server restart).
        let (broken, _, _) = conn.conn(0);
        broken.store(true, Ordering::SeqCst);

        let out = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .expect("dead cached connection must degrade to reconnect, not error");
        assert_eq!(
            text_of(&out),
            "conn-1:echo",
            "answered by the fresh connection"
        );
        assert_eq!(conn.connects(), 2);
        // The fresh connection replaced the dead one in the cache.
        let again = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(text_of(&again), "conn-1:echo");
        assert_eq!(conn.connects(), 2);
    }

    #[tokio::test]
    async fn retry_is_once_not_a_loop() {
        // Cached connection dead AND every fresh connection born dead: the
        // second failure surfaces — exactly one reconnect attempt.
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        let (broken, _, _) = conn.conn(0);
        broken.store(true, Ordering::SeqCst);
        conn.born_broken.store(true, Ordering::SeqCst);

        let err = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
        assert_eq!(conn.connects(), 2, "initial + exactly one retry connect");
        assert!(
            cache.is_empty(),
            "a connection that died mid-call is not cached"
        );
    }

    #[tokio::test]
    async fn reconnect_failure_during_retry_propagates() {
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        let (broken, _, _) = conn.conn(0);
        broken.store(true, Ordering::SeqCst);
        conn.fail_connect.store(true, Ordering::SeqCst);

        let err = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err, McpError::Transport("connect refused".into()));
        assert!(cache.is_empty(), "the dead connection was evicted");
    }

    #[tokio::test]
    async fn remote_error_is_not_retried_and_keeps_the_connection() {
        // A tool-level failure is the call's outcome, not a dead connection:
        // no reconnect (the remote may have side-effected), entry stays.
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        let (_, remote_error, calls) = conn.conn(0);
        remote_error.store(true, Ordering::SeqCst);

        let err = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::RemoteError { .. }), "got {err:?}");
        assert_eq!(conn.connects(), 1, "no reconnect for a remote error");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.len(), 1, "connection stays cached");

        remote_error.store(false, Ordering::SeqCst);
        let out = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(text_of(&out), "conn-0:echo", "same connection recovered");
    }

    #[tokio::test]
    async fn filtered_tool_is_refused_and_never_retried() {
        // The per-server filter refuses before the transport is touched —
        // and a policy refusal must never trigger a reconnect.
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        let filtered = serde_json::json!({"command": "/srv/tool", "denied_tools": ["danger"]});
        let err = cache
            .call_with(&conn, "sess-1", &filtered, "danger", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ToolFiltered { .. }), "got {err:?}");
        // Matches the uncached path: the connect happens first, then the
        // filter refuses; the healthy connection is kept for permitted tools.
        assert_eq!(conn.connects(), 1);
        let (_, _, calls) = conn.conn(0);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "remote never contacted");
        assert_eq!(cache.len(), 1);

        let err = cache
            .call_with(&conn, "sess-1", &filtered, "danger", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ToolFiltered { .. }));
        assert_eq!(
            conn.connects(),
            1,
            "refusal on the cached path: no reconnect"
        );
    }

    #[tokio::test]
    async fn first_call_failure_is_not_retried() {
        // Miss-path semantics are byte-identical to the uncached call: a
        // transport failure on a brand-new connection surfaces immediately
        // (no retry — retry-once is only for previously-cached connections).
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        conn.born_broken.store(true, Ordering::SeqCst);
        let err = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
        assert_eq!(
            conn.connects(),
            1,
            "exactly one connect, like the uncached path"
        );
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn first_call_connect_failure_propagates_unchanged() {
        let cache = McpConnectionCache::new(Duration::from_secs(300));
        let conn = CountingConnector::default();
        conn.fail_connect.store(true, Ordering::SeqCst);
        let err = cache
            .call_with(&conn, "sess-1", &entry(), "echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err, McpError::Transport("connect refused".into()));
        assert!(cache.is_empty());
    }
}
