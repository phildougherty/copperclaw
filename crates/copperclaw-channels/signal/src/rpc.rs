//! JSON-RPC 2.0 transport for signal-cli's `daemon --json-rpc` mode.
//!
//! `signal-cli` reads JSON-RPC requests on stdin (one frame per line) and
//! writes responses and notifications on stdout (also one frame per line).
//!
//! This module defines:
//!
//! - [`RpcError`] — JSON-RPC error type returned by signal-cli; carries a
//!   numeric `code` and a human message.
//! - [`Notification`] — a server-initiated message (notably the `receive`
//!   stream of incoming chat events).
//! - [`RpcTransport`] — the trait the adapter speaks to. There are two
//!   implementations: [`JsonRpcClient`], which owns the real subprocess,
//!   and [`MockTransport`] (under `#[cfg(test)]`-friendly visibility), used
//!   by adapter and API tests so nothing spawns `signal-cli`.
//! - [`JsonRpcClient`] — the production transport.
//!
//! The client spawns a child process and three tasks:
//!
//! 1. A writer task draining a `Sender<JsonRpcRequest>` to `stdin`.
//! 2. A reader task parsing `stdout` lines into either request-response
//!    matching (`id` present) or notifications (`method` present), which are
//!    forwarded to a broadcast-style channel for the adapter to consume.
//! 3. A stderr-drain task that logs whatever the daemon writes to stderr.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use copperclaw_channels_core::AdapterError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

/// A JSON-RPC error as returned by signal-cli.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RpcError {
    /// Numeric error code (e.g. `-1` for `AuthorizationFailedException`,
    /// `-3` for `RateLimitException`).
    pub code: i64,
    /// Human-readable message.
    pub message: String,
    /// Optional structured data returned by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rpc error {}: {}", self.code, self.message)
    }
}

/// A server-initiated JSON-RPC notification.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Notification {
    /// The method name (e.g. `"receive"`).
    pub method: String,
    /// The full `params` value.
    pub params: Value,
}

/// A JSON-RPC request issued to signal-cli.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Per-call id used to correlate the response.
    pub id: u64,
    /// Method name.
    pub method: String,
    /// `params` value.
    pub params: Value,
}

impl JsonRpcRequest {
    /// Build a new request with the supplied `id`, `method`, and `params`.
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// Map a signal-cli [`RpcError`] to an [`AdapterError`] using the contract:
///
/// - `-1` (`AuthorizationFailedException`, `MissingTokenException`) -> `Auth`
/// - `-3` (`RateLimitException`) -> `Rate { retry_after: None }`
/// - any other code -> `BadRequest("signal: {code}: {message}")`
pub fn rpc_error_to_adapter(err: &RpcError) -> AdapterError {
    match err.code {
        -1 => AdapterError::Auth(err.message.clone()),
        -3 => AdapterError::Rate { retry_after: None },
        code => AdapterError::BadRequest(format!("signal: {code}: {}", err.message)),
    }
}

/// Abstract JSON-RPC transport. The adapter speaks to one of these; the
/// production implementation is [`JsonRpcClient`]; tests use
/// [`MockTransport`].
#[async_trait]
pub trait RpcTransport: Send + Sync {
    /// Issue a JSON-RPC `call`. The transport assigns the id, sends the
    /// frame, awaits the matching response, and returns either the `result`
    /// value or an [`AdapterError`] (rate, auth, transport, or
    /// bad-request).
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError>;

    /// Take a receiver for server-initiated notifications.
    ///
    /// Each call returns the receiver for a freshly-installed channel; in
    /// practice the adapter calls this exactly once during start-up. If the
    /// transport has no notification stream (`MockTransport` configured
    /// without one), the returned `Receiver` is closed immediately.
    async fn take_notifications(&self) -> mpsc::Receiver<Notification>;
}

/// Production JSON-RPC transport: owns a `signal-cli daemon --json-rpc`
/// subprocess and shuttles frames in and out of it.
pub struct JsonRpcClient {
    next_id: AtomicU64,
    requests_tx: mpsc::Sender<JsonRpcRequest>,
    pending: PendingMap,
    notif_rx: Mutex<Option<mpsc::Receiver<Notification>>>,
    #[allow(dead_code)] // owned for the lifetime of the client; cleanup on drop
    writer_task: JoinHandle<()>,
    #[allow(dead_code)]
    reader_task: JoinHandle<()>,
    #[allow(dead_code)]
    stderr_task: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    child: Mutex<Child>,
}

/// Shared map from JSON-RPC request id to the oneshot sender that will
/// resolve when the matching response arrives.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// Capacity of the request channel between the adapter and the writer task.
pub const REQUEST_CHANNEL_CAPACITY: usize = 64;

/// Capacity of the notification channel from the reader task to the adapter.
pub const NOTIFICATION_CHANNEL_CAPACITY: usize = 64;

impl std::fmt::Debug for JsonRpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonRpcClient")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl JsonRpcClient {
    /// True when both reader and writer tasks are still running. False
    /// when either one has finished, which on the production transport
    /// indicates the child process closed its stdio (i.e. died).
    /// Polled by [`SignalSupervisor`] to decide when to respawn.
    pub fn is_alive(&self) -> bool {
        !self.writer_task.is_finished() && !self.reader_task.is_finished()
    }

    /// Spawn `bin` with the supplied arguments and wire stdin/stdout/stderr
    /// through the request, response, and stderr tasks.
    ///
    /// Errors map to [`AdapterError::Transport`] when the subprocess cannot
    /// be spawned or its stdio handles cannot be opened.
    pub fn spawn(bin: &str, args: &[String]) -> Result<Arc<Self>, AdapterError> {
        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AdapterError::Transport(format!("signal: failed to spawn {bin}: {e}")))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            AdapterError::Transport("signal: subprocess stdin unavailable".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AdapterError::Transport("signal: subprocess stdout unavailable".into())
        })?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (requests_tx, requests_rx) = mpsc::channel::<JsonRpcRequest>(REQUEST_CHANNEL_CAPACITY);
        let (notif_tx, notif_rx) = mpsc::channel::<Notification>(NOTIFICATION_CHANNEL_CAPACITY);

        let writer_task = tokio::spawn(writer_loop(stdin, requests_rx));
        let reader_task = tokio::spawn(reader_loop(stdout, pending.clone(), notif_tx));
        let stderr_task = stderr.map(|s| tokio::spawn(stderr_loop(s)));

        Ok(Arc::new(Self {
            next_id: AtomicU64::new(1),
            requests_tx,
            pending,
            notif_rx: Mutex::new(Some(notif_rx)),
            writer_task,
            reader_task,
            stderr_task,
            child: Mutex::new(child),
        }))
    }
}

#[async_trait]
impl RpcTransport for JsonRpcClient {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }
        let req = JsonRpcRequest::new(id, method, params);
        if self.requests_tx.send(req).await.is_err() {
            // Writer task dropped the receiver — subprocess is gone.
            self.pending.lock().await.remove(&id);
            return Err(AdapterError::Transport(
                "signal: rpc writer channel closed".into(),
            ));
        }
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(rpc_error_to_adapter(&err)),
            Err(_) => Err(AdapterError::Transport(
                "signal: rpc response channel closed".into(),
            )),
        }
    }

    async fn take_notifications(&self) -> mpsc::Receiver<Notification> {
        let mut slot = self.notif_rx.lock().await;
        slot.take().unwrap_or_else(|| {
            // Already taken — hand back an immediately-closed channel.
            let (_, rx) = mpsc::channel::<Notification>(1);
            rx
        })
    }
}

/// Background task that drains the request channel and writes each frame
/// (followed by `\n`) to the subprocess stdin. Exits when the channel is
/// closed or any write fails.
async fn writer_loop(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<JsonRpcRequest>,
) {
    while let Some(req) = rx.recv().await {
        let mut bytes = match serde_json::to_vec(&req) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, "signal: failed to serialize request");
                continue;
            }
        };
        bytes.push(b'\n');
        if let Err(err) = stdin.write_all(&bytes).await {
            tracing::warn!(?err, "signal: stdin write failed; stopping writer");
            return;
        }
        if let Err(err) = stdin.flush().await {
            tracing::warn!(?err, "signal: stdin flush failed; stopping writer");
            return;
        }
    }
}

/// Background task that reads subprocess stdout line-by-line, decodes each
/// line as a JSON-RPC frame, and either:
///
/// - resolves the matching `oneshot::Sender` for a response (`id` present),
/// - or forwards a notification (`method` present) on the `notif_tx`
///   channel.
///
/// Unparseable lines are logged and skipped.
async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    notif_tx: mpsc::Sender<Notification>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!(error = %err, line = %line, "signal: unparseable stdout line");
                        continue;
                    }
                };
                dispatch_frame(value, &pending, &notif_tx).await;
            }
            Ok(None) => {
                tracing::debug!("signal: subprocess stdout EOF; reader exiting");
                return;
            }
            Err(err) => {
                tracing::warn!(?err, "signal: stdout read error; reader exiting");
                return;
            }
        }
    }
}

/// Dispatch a parsed JSON-RPC frame to either the response or notification
/// path. Frames missing both an `id` and a `method` are logged and dropped.
async fn dispatch_frame(value: Value, pending: &PendingMap, notif_tx: &mpsc::Sender<Notification>) {
    let id = value.get("id").and_then(Value::as_u64);
    let method = value.get("method").and_then(Value::as_str);

    if let Some(id) = id {
        // Response frame.
        let sender = {
            let mut guard = pending.lock().await;
            guard.remove(&id)
        };
        let Some(sender) = sender else {
            tracing::warn!(id, "signal: response for unknown id");
            return;
        };
        if let Some(err) = value.get("error") {
            match serde_json::from_value::<RpcError>(err.clone()) {
                Ok(rpc_err) => {
                    let _ = sender.send(Err(rpc_err));
                }
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        "signal: failed to parse error field; treating as bad request"
                    );
                    let _ = sender.send(Err(RpcError {
                        code: 0,
                        message: format!("malformed error field: {e}"),
                        data: None,
                    }));
                }
            }
            return;
        }
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        let _ = sender.send(Ok(result));
        return;
    }

    if let Some(method) = method {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let notif = Notification {
            method: method.to_owned(),
            params,
        };
        if let Err(err) = notif_tx.send(notif).await {
            tracing::debug!(?err, "signal: notification channel closed");
        }
        return;
    }

    tracing::warn!(?value, "signal: frame missing both id and method");
}

/// Background task that drains the subprocess stderr and logs each line at
/// `debug` level.
async fn stderr_loop(stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(target: "signal-cli", "{line}");
    }
}

/// Test-only mock transport. Wraps a script of canned `(method, response)`
/// pairs and a notification stream that tests can push into.
///
/// Construction:
///
/// ```ignore
/// let (mock, ctl) = MockTransport::new();
/// ctl.expect("send", json!({"timestamp": 1}));
/// ```
///
/// `MockTransport` is `cfg(test)` in spirit, but lives outside the cfg
/// guard so the [`api`](crate::api) and [`adapter`](crate::adapter) tests
/// (in their own modules) can use it.
pub struct MockTransport {
    inner: Arc<MockInner>,
}

struct MockInner {
    expected: Mutex<Vec<MockExpectation>>,
    calls: Mutex<Vec<(String, Value)>>,
    notif_rx: Mutex<Option<mpsc::Receiver<Notification>>>,
    notif_tx: mpsc::Sender<Notification>,
}

#[allow(dead_code)]
struct MockExpectation {
    method: String,
    response: Result<Value, RpcError>,
}

/// Test handle for [`MockTransport`]. Tests use it to queue responses and
/// inspect captured calls without exposing the transport's internals.
pub struct MockHandle {
    inner: Arc<MockInner>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new().0
    }
}

impl MockTransport {
    /// Construct a fresh mock transport. Returns the transport itself plus a
    /// handle the test uses to script behaviour.
    pub fn new() -> (Self, MockHandle) {
        let (notif_tx, notif_rx) = mpsc::channel::<Notification>(NOTIFICATION_CHANNEL_CAPACITY);
        let inner = Arc::new(MockInner {
            expected: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            notif_rx: Mutex::new(Some(notif_rx)),
            notif_tx,
        });
        (
            Self {
                inner: inner.clone(),
            },
            MockHandle { inner },
        )
    }
}

impl MockHandle {
    /// Queue a successful response for the next call to `method`.
    pub async fn expect_ok(&self, method: impl Into<String>, response: Value) {
        self.inner.expected.lock().await.push(MockExpectation {
            method: method.into(),
            response: Ok(response),
        });
    }

    /// Queue an error response for the next call to `method`.
    pub async fn expect_err(&self, method: impl Into<String>, err: RpcError) {
        self.inner.expected.lock().await.push(MockExpectation {
            method: method.into(),
            response: Err(err),
        });
    }

    /// Push a notification into the stream.
    pub async fn push_notification(&self, notif: Notification) {
        let _ = self.inner.notif_tx.send(notif).await;
    }

    /// Snapshot of the calls observed by the transport, in order.
    pub async fn calls(&self) -> Vec<(String, Value)> {
        self.inner.calls.lock().await.clone()
    }
}

#[async_trait]
impl RpcTransport for MockTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        self.inner
            .calls
            .lock()
            .await
            .push((method.to_owned(), params.clone()));
        let mut queue = self.inner.expected.lock().await;
        if queue.is_empty() {
            return Err(AdapterError::Transport(format!(
                "mock: no expectation for {method}"
            )));
        }
        let exp = queue.remove(0);
        if exp.method != method {
            return Err(AdapterError::BadRequest(format!(
                "mock: expected method `{}`, got `{}`",
                exp.method, method
            )));
        }
        match exp.response {
            Ok(v) => Ok(v),
            Err(err) => Err(rpc_error_to_adapter(&err)),
        }
    }

    async fn take_notifications(&self) -> mpsc::Receiver<Notification> {
        self.inner.notif_rx.lock().await.take().unwrap_or_else(|| {
            let (_, rx) = mpsc::channel::<Notification>(1);
            rx
        })
    }
}

// ----------------------------------------------------------------------
// SignalSupervisor — daemon respawn
// ----------------------------------------------------------------------

/// Initial respawn backoff. Doubles per failed spawn up to
/// [`RESPAWN_BACKOFF_CEILING`]. Short to keep transient daemon flaps
/// from causing visible message gaps; long enough to avoid hammering
/// the executable when it's permanently broken.
const RESPAWN_BACKOFF_INITIAL_MS: u64 = 500;
/// Ceiling for the respawn backoff. 30 s mirrors the host's
/// delivery-loop ceiling — the daemon will eventually catch up
/// without an operator restart.
const RESPAWN_BACKOFF_CEILING_MS: u64 = 30_000;
/// How often the watchdog polls the underlying client's liveness.
const WATCHDOG_POLL_INTERVAL_MS: u64 = 500;

/// Wraps a [`JsonRpcClient`] and respawns it when the child process
/// exits. The supervisor implements [`RpcTransport`] by delegating to
/// the currently-live client; from the adapter's point of view, a
/// daemon respawn is invisible.
///
/// Notifications from each successive child are forwarded through a
/// shared mpsc so the adapter's already-running notification loop
/// keeps receiving events without rewiring after a respawn.
pub struct SignalSupervisor {
    current: Mutex<Arc<JsonRpcClient>>,
    bin: String,
    args: Vec<String>,
    notif_tx: mpsc::Sender<Notification>,
    notif_rx_slot: Mutex<Option<mpsc::Receiver<Notification>>>,
}

impl std::fmt::Debug for SignalSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalSupervisor")
            .field("bin", &self.bin)
            .field("args", &self.args)
            .finish_non_exhaustive()
    }
}

impl SignalSupervisor {
    /// Spawn the initial signal-cli daemon, install the watchdog, and
    /// return the supervisor as an `Arc<dyn RpcTransport>`-ready value.
    pub async fn spawn(bin: &str, args: &[String]) -> Result<Arc<Self>, AdapterError> {
        let initial = JsonRpcClient::spawn(bin, args)?;
        let initial_notif = initial.take_notifications().await;
        let (notif_tx, notif_rx) = mpsc::channel::<Notification>(NOTIFICATION_CHANNEL_CAPACITY);
        // Forward the first child's notifications into the shared mpsc.
        tokio::spawn(forward_notifications(initial_notif, notif_tx.clone()));
        let me = Arc::new(Self {
            current: Mutex::new(initial),
            bin: bin.to_owned(),
            args: args.to_owned(),
            notif_tx,
            notif_rx_slot: Mutex::new(Some(notif_rx)),
        });
        tokio::spawn(watchdog_loop(Arc::clone(&me)));
        Ok(me)
    }
}

#[async_trait]
impl RpcTransport for SignalSupervisor {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        // Snapshot the current client outside the await so the
        // mutex isn't held while we wait for the daemon to reply.
        let client = { self.current.lock().await.clone() };
        client.call(method, params).await
    }

    async fn take_notifications(&self) -> mpsc::Receiver<Notification> {
        let mut slot = self.notif_rx_slot.lock().await;
        slot.take().unwrap_or_else(|| {
            let (_, rx) = mpsc::channel::<Notification>(1);
            rx
        })
    }
}

/// Forward every notification from `src` into `dst` until `src` closes.
/// One copy per child process — when the child dies, the source mpsc
/// closes and this task exits; the supervisor's watchdog spawns a fresh
/// forwarder against the next child.
async fn forward_notifications(
    mut src: mpsc::Receiver<Notification>,
    dst: mpsc::Sender<Notification>,
) {
    while let Some(n) = src.recv().await {
        if dst.send(n).await.is_err() {
            // Adapter dropped its receiver; supervisor going down.
            break;
        }
    }
}

/// Background loop that polls the supervised client's liveness; when
/// the child dies, respawns a fresh daemon with exponential backoff
/// and swaps it into the supervisor.
async fn watchdog_loop(supervisor: Arc<SignalSupervisor>) {
    loop {
        // Snapshot the current client and wait for it to die.
        let current = { supervisor.current.lock().await.clone() };
        while current.is_alive() {
            // The supervisor itself may be the only owner of `current`
            // — keep our copy alive until we observe death, then drop.
            tokio::time::sleep(std::time::Duration::from_millis(WATCHDOG_POLL_INTERVAL_MS)).await;
        }
        tracing::warn!("signal: signal-cli daemon exited; respawning with backoff");
        drop(current);

        // Respawn with exponential backoff. Tries forever — the
        // operator's only correct response to a permanently-broken
        // daemon binary is to fix the config and restart the host;
        // until then we just keep trying.
        let mut backoff_ms = RESPAWN_BACKOFF_INITIAL_MS;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            match JsonRpcClient::spawn(&supervisor.bin, &supervisor.args) {
                Ok(new_client) => {
                    let new_notif = new_client.take_notifications().await;
                    tokio::spawn(forward_notifications(
                        new_notif,
                        supervisor.notif_tx.clone(),
                    ));
                    *supervisor.current.lock().await = new_client;
                    tracing::info!("signal: daemon respawned");
                    break;
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        backoff_ms,
                        "signal: respawn attempt failed; backing off"
                    );
                    backoff_ms = (backoff_ms.saturating_mul(2)).min(RESPAWN_BACKOFF_CEILING_MS);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::time::{Duration, timeout};

    #[test]
    fn rpc_error_displays_with_code_and_message() {
        let e = RpcError {
            code: -3,
            message: "rate".into(),
            data: None,
        };
        assert_eq!(format!("{e}"), "rpc error -3: rate");
    }

    #[test]
    fn rpc_error_to_adapter_minus_one_is_auth() {
        let e = RpcError {
            code: -1,
            message: "AuthorizationFailedException".into(),
            data: None,
        };
        assert!(matches!(rpc_error_to_adapter(&e), AdapterError::Auth(_)));
    }

    #[test]
    fn rpc_error_to_adapter_minus_three_is_rate() {
        let e = RpcError {
            code: -3,
            message: "RateLimitException".into(),
            data: None,
        };
        assert!(matches!(
            rpc_error_to_adapter(&e),
            AdapterError::Rate { retry_after: None }
        ));
    }

    #[test]
    fn rpc_error_to_adapter_other_is_bad_request() {
        let e = RpcError {
            code: -32601,
            message: "Method not found".into(),
            data: None,
        };
        match rpc_error_to_adapter(&e) {
            AdapterError::BadRequest(m) => {
                assert!(m.contains("-32601"));
                assert!(m.contains("Method not found"));
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn json_rpc_request_serialises_with_jsonrpc_field() {
        let req = JsonRpcRequest::new(7, "send", json!({"x": 1}));
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"id\":7"));
        assert!(s.contains("\"method\":\"send\""));
    }

    #[tokio::test]
    async fn mock_returns_queued_response() {
        let (mock, ctl) = MockTransport::new();
        ctl.expect_ok("send", json!({"timestamp": 1700})).await;
        let v = mock
            .call("send", json!({"recipient": ["+1"]}))
            .await
            .unwrap();
        assert_eq!(v["timestamp"], 1700);
    }

    #[tokio::test]
    async fn mock_call_records_calls() {
        let (mock, ctl) = MockTransport::new();
        ctl.expect_ok("send", json!({})).await;
        mock.call("send", json!({"k": "v"})).await.unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "send");
        assert_eq!(calls[0].1["k"], "v");
    }

    #[tokio::test]
    async fn mock_returns_error_when_no_expectation() {
        let (mock, _ctl) = MockTransport::new();
        let err = mock.call("send", json!({})).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn mock_wrong_method_returns_bad_request() {
        let (mock, ctl) = MockTransport::new();
        ctl.expect_ok("send", json!({})).await;
        let err = mock.call("sendTyping", json!({})).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn mock_rate_error_propagates() {
        let (mock, ctl) = MockTransport::new();
        ctl.expect_err(
            "send",
            RpcError {
                code: -3,
                message: "rl".into(),
                data: None,
            },
        )
        .await;
        let err = mock.call("send", json!({})).await.unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: None }));
    }

    #[tokio::test]
    async fn mock_auth_error_propagates() {
        let (mock, ctl) = MockTransport::new();
        ctl.expect_err(
            "send",
            RpcError {
                code: -1,
                message: "auth".into(),
                data: None,
            },
        )
        .await;
        let err = mock.call("send", json!({})).await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn mock_other_error_is_bad_request() {
        let (mock, ctl) = MockTransport::new();
        ctl.expect_err(
            "send",
            RpcError {
                code: -2,
                message: "untrusted".into(),
                data: None,
            },
        )
        .await;
        let err = mock.call("send", json!({})).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn mock_take_notifications_then_push() {
        let (mock, ctl) = MockTransport::new();
        let mut rx = mock.take_notifications().await;
        ctl.push_notification(Notification {
            method: "receive".into(),
            params: json!({"hello": "world"}),
        })
        .await;
        let n = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n.method, "receive");
        assert_eq!(n.params["hello"], "world");
    }

    #[tokio::test]
    async fn mock_second_take_yields_closed_receiver() {
        let (mock, _ctl) = MockTransport::new();
        let _first = mock.take_notifications().await;
        let mut second = mock.take_notifications().await;
        let v = timeout(Duration::from_millis(50), second.recv()).await;
        // Either closed (Ok(None)) or timed out (Err) — both are acceptable
        // "no events" outcomes.
        match v {
            Ok(None) | Err(_) => {}
            Ok(Some(n)) => panic!("unexpected: {n:?}"),
        }
    }

    #[tokio::test]
    async fn mock_default_is_usable() {
        let mock: MockTransport = MockTransport::default();
        let mut rx = mock.take_notifications().await;
        let res = timeout(Duration::from_millis(20), rx.recv()).await;
        match res {
            Ok(None) | Err(_) => {}
            Ok(Some(_)) => panic!("unexpected notification"),
        }
    }

    #[tokio::test]
    async fn dispatch_frame_response_with_result_resolves_pending() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(1, tx);
        let (notif_tx, _notif_rx) = mpsc::channel::<Notification>(4);
        dispatch_frame(
            json!({"jsonrpc": "2.0", "id": 1, "result": {"timestamp": 7}}),
            &pending,
            &notif_tx,
        )
        .await;
        let val = rx.await.unwrap().unwrap();
        assert_eq!(val["timestamp"], 7);
    }

    #[tokio::test]
    async fn dispatch_frame_response_with_error_resolves_pending() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(2, tx);
        let (notif_tx, _notif_rx) = mpsc::channel::<Notification>(4);
        dispatch_frame(
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": {"code": -3, "message": "rl"}
            }),
            &pending,
            &notif_tx,
        )
        .await;
        let err = rx.await.unwrap().unwrap_err();
        assert_eq!(err.code, -3);
        assert_eq!(err.message, "rl");
    }

    #[tokio::test]
    async fn dispatch_frame_response_unknown_id_is_dropped() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, _rx) = mpsc::channel::<Notification>(4);
        // No pending entry for id=99: handler should log and drop.
        dispatch_frame(
            json!({"jsonrpc": "2.0", "id": 99, "result": {}}),
            &pending,
            &notif_tx,
        )
        .await;
        assert!(pending.lock().await.is_empty());
    }

    #[tokio::test]
    async fn dispatch_frame_notification_is_forwarded() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, mut notif_rx) = mpsc::channel::<Notification>(4);
        dispatch_frame(
            json!({
                "jsonrpc": "2.0",
                "method": "receive",
                "params": {"envelope": {"x": 1}}
            }),
            &pending,
            &notif_tx,
        )
        .await;
        let n = notif_rx.recv().await.unwrap();
        assert_eq!(n.method, "receive");
        assert_eq!(n.params["envelope"]["x"], 1);
    }

    #[tokio::test]
    async fn dispatch_frame_without_id_or_method_is_dropped() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, mut notif_rx) = mpsc::channel::<Notification>(4);
        dispatch_frame(json!({"jsonrpc": "2.0"}), &pending, &notif_tx).await;
        let v = timeout(Duration::from_millis(20), notif_rx.recv()).await;
        match v {
            Ok(None) | Err(_) => {}
            Ok(Some(n)) => panic!("unexpected forwarded notification: {n:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_frame_malformed_error_still_resolves_pending() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(3, tx);
        let (notif_tx, _rx) = mpsc::channel::<Notification>(4);
        // `error.code` is the wrong type (string) — the parser handles this
        // by producing a synthetic RpcError so the call still completes.
        dispatch_frame(
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "error": {"code": "weird", "message": "huh"}
            }),
            &pending,
            &notif_tx,
        )
        .await;
        let err = rx.await.unwrap().unwrap_err();
        assert_eq!(err.code, 0);
        assert!(err.message.contains("malformed"));
    }

    #[tokio::test]
    async fn spawn_with_missing_binary_returns_transport_error() {
        let err = JsonRpcClient::spawn(
            "definitely-not-on-path-signal-cli-binary-xyz",
            &["daemon".to_owned()],
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn supervisor_spawn_with_missing_binary_returns_transport_error() {
        // The supervisor delegates to JsonRpcClient::spawn for the
        // initial spawn; a missing binary surfaces as Transport, same
        // as the unwrapped path. The watchdog never starts because
        // we never get past the initial spawn.
        let err = SignalSupervisor::spawn(
            "definitely-not-on-path-signal-cli-binary-xyz",
            &["daemon".to_owned()],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn jsonrpc_client_is_alive_starts_true() {
        // A freshly-spawned mock daemon (using `/bin/cat` so the
        // child stays alive reading stdin) should be `is_alive`. We
        // can't easily simulate a real signal-cli daemon here, but
        // `cat` produces a long-lived child with stdin/stdout piped,
        // which is enough to exercise the liveness check.
        let client = JsonRpcClient::spawn("/bin/cat", &[]).unwrap();
        // The reader_loop may exit when EOF is reached on stdout — but
        // for `cat` stdout stays open as long as stdin does, and we
        // haven't closed stdin yet.
        assert!(client.is_alive());
    }

    #[test]
    fn notification_clone_and_debug() {
        let n = Notification {
            method: "receive".into(),
            params: json!({}),
        };
        let copy = n.clone();
        assert_eq!(n, copy);
        let s = format!("{n:?}");
        assert!(s.contains("Notification"));
    }

    #[test]
    fn rpc_error_clone_and_debug() {
        let e = RpcError {
            code: -1,
            message: "x".into(),
            data: Some(json!({"k": "v"})),
        };
        let copy = e.clone();
        assert_eq!(e, copy);
        let s = format!("{e:?}");
        assert!(s.contains("RpcError"));
    }

    #[test]
    fn channel_capacities_are_reasonable() {
        // Use runtime-bound locals to sidestep clippy::assertions_on_constants.
        let req_cap = REQUEST_CHANNEL_CAPACITY;
        let notif_cap = NOTIFICATION_CHANNEL_CAPACITY;
        assert!(req_cap >= 16);
        assert!(notif_cap >= 16);
    }
}
