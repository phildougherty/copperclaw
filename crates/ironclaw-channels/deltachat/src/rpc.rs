//! JSON-RPC 2.0 transport for the `deltachat-rpc-server` subprocess.
//!
//! [`RpcTransport`] is the abstraction the adapter uses for every call so
//! tests can substitute [`MockTransport`] in place of a real subprocess.
//!
//! The real [`SubprocessTransport`] spawns `deltachat-rpc-server` with its
//! stdin/stdout pipes, serializes requests as newline-delimited JSON, and
//! routes responses back to the caller by matching the `id` field.
//!
//! ### Error mapping
//!
//! JSON-RPC errors carry an integer code. The transport maps:
//!
//! - code `2` (auth-like) or any message containing `"auth"` / `"login"`
//!   -> [`AdapterError::Auth`].
//! - any message containing `"rate"` and `"limit"` -> [`AdapterError::Rate`]
//!   with `retry_after = None`.
//! - everything else -> [`AdapterError::BadRequest`] (with the original
//!   server message preserved).
//!
//! I/O failures (broken pipe, killed subprocess, malformed JSON) surface
//! as [`AdapterError::Transport`].

use async_trait::async_trait;
use ironclaw_channels_core::AdapterError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Sender end of a pending RPC response delivery.
type ResponseSender = oneshot::Sender<Result<Value, AdapterError>>;
/// Pending-response map keyed by RPC id.
type PendingMap = Arc<Mutex<HashMap<u64, ResponseSender>>>;

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize)]
pub struct RpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Caller-assigned id; the response's `id` is matched against this.
    pub id: u64,
    /// Method name (e.g. `"send_msg"`, `"get_next_event"`).
    pub method: String,
    /// Positional parameter array.
    pub params: Value,
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse {
    /// Server-assigned id mirroring [`RpcRequest::id`].
    pub id: u64,
    /// Result payload when the call succeeded.
    #[serde(default)]
    pub result: Option<Value>,
    /// Error payload when the call failed.
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// JSON-RPC error payload.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcError {
    /// Server-defined error code.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured data.
    #[serde(default)]
    pub data: Option<Value>,
}

/// Abstract JSON-RPC transport used by the Delta Chat adapter.
///
/// `call` issues a single request and waits for the matching response.
/// `next_event` polls `get_next_event` and returns the next event payload.
#[async_trait]
pub trait RpcTransport: Send + Sync {
    /// Issue an RPC call and await its response.
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError>;

    /// Block until the next `Event` arrives. Returns the raw event payload.
    async fn next_event(&self) -> Result<Value, AdapterError>;
}

/// Real subprocess transport.
///
/// Spawns `deltachat-rpc-server` (or whatever the config points at), wires
/// up a writer task draining outgoing requests into the subprocess stdin,
/// and a reader task parsing newline-delimited JSON responses off stdout.
///
/// Responses are keyed by `id` and delivered to the pending caller via a
/// oneshot channel. Unsolicited notifications (server lines whose `id`
/// field is missing or `0`) are forwarded into [`Self::notifications`]
/// for consumers that prefer push-style consumption; the standard path
/// is to call `get_next_event` explicitly via [`RpcTransport::next_event`].
pub struct SubprocessTransport {
    next_id: AtomicU64,
    pending: PendingMap,
    writer_tx: mpsc::Sender<String>,
    notifications: Mutex<mpsc::Receiver<Value>>,
    child: Mutex<Option<Child>>,
    shutdown: CancellationToken,
    writer_handle: Mutex<Option<JoinHandle<()>>>,
    reader_handle: Mutex<Option<JoinHandle<()>>>,
}

impl SubprocessTransport {
    /// Spawn `bin` with `args` and wire up the I/O pumps.
    ///
    /// Returns an error if the subprocess fails to spawn (binary missing,
    /// permission denied, etc.).
    pub fn spawn(bin: &str, args: &[String]) -> Result<Arc<Self>, AdapterError> {
        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                AdapterError::Transport(format!("failed to spawn {bin}: {e}"))
            })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            AdapterError::Transport("subprocess stdin was not captured".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AdapterError::Transport("subprocess stdout was not captured".into())
        })?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (writer_tx, mut writer_rx) = mpsc::channel::<String>(32);
        let (notification_tx, notification_rx) = mpsc::channel::<Value>(32);
        let shutdown = CancellationToken::new();

        let writer_shutdown = shutdown.clone();
        let writer_handle = tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                tokio::select! {
                    () = writer_shutdown.cancelled() => break,
                    maybe = writer_rx.recv() => {
                        let Some(line) = maybe else { break };
                        if let Err(err) = stdin.write_all(line.as_bytes()).await {
                            tracing::warn!(?err, "deltachat: stdin write failed");
                            break;
                        }
                        if let Err(err) = stdin.write_all(b"\n").await {
                            tracing::warn!(?err, "deltachat: stdin newline write failed");
                            break;
                        }
                        if let Err(err) = stdin.flush().await {
                            tracing::warn!(?err, "deltachat: stdin flush failed");
                            break;
                        }
                    }
                }
            }
        });

        let pending_for_reader = pending.clone();
        let reader_shutdown = shutdown.clone();
        let reader_handle = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    () = reader_shutdown.cancelled() => break,
                    next = lines.next_line() => {
                        match next {
                            Ok(Some(line)) => {
                                if line.trim().is_empty() {
                                    continue;
                                }
                                handle_line(&line, &pending_for_reader, &notification_tx).await;
                            }
                            Ok(None) => break,
                            Err(err) => {
                                tracing::warn!(?err, "deltachat: stdout read failed");
                                break;
                            }
                        }
                    }
                }
            }
            // Notify any callers still waiting that the transport is dead.
            let mut guard = pending_for_reader.lock().await;
            for (_, tx) in guard.drain() {
                let _ = tx.send(Err(AdapterError::Transport(
                    "deltachat subprocess terminated".into(),
                )));
            }
        });

        Ok(Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending,
            writer_tx,
            notifications: Mutex::new(notification_rx),
            child: Mutex::new(Some(child)),
            shutdown,
            writer_handle: Mutex::new(Some(writer_handle)),
            reader_handle: Mutex::new(Some(reader_handle)),
        }))
    }

    /// Receive the next unsolicited notification line from the server, if
    /// any has been buffered. Returns `None` when the channel is closed
    /// (subprocess exited).
    ///
    /// Most callers should use [`RpcTransport::next_event`] (which issues
    /// `get_next_event`) instead — this method exists for tests and for
    /// future use should `deltachat-rpc-server` ever surface push-style
    /// events.
    pub async fn try_recv_notification(&self) -> Option<Value> {
        self.notifications.lock().await.recv().await
    }

    /// Stop background tasks and kill the subprocess. Idempotent.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        if let Some(h) = self.writer_handle.lock().await.take() {
            let _ = h.await;
        }
        if let Some(h) = self.reader_handle.lock().await.take() {
            let _ = h.await;
        }
    }
}

impl std::fmt::Debug for SubprocessTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubprocessTransport").finish_non_exhaustive()
    }
}

async fn handle_line(
    line: &str,
    pending: &PendingMap,
    event_tx: &mpsc::Sender<Value>,
) {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(?err, raw = %line, "deltachat: invalid JSON from server");
            return;
        }
    };
    // An RPC response always has a numeric id. A notification (unsolicited
    // event) either omits id or has id == 0.
    let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
    if id == 0 {
        let payload = value.get("params").cloned().unwrap_or(value);
        if event_tx.send(payload).await.is_err() {
            tracing::debug!("deltachat: event channel closed");
        }
        return;
    }
    let response: RpcResponse = match serde_json::from_value(value.clone()) {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(?err, raw = %line, "deltachat: response decode failed");
            return;
        }
    };
    let sender = pending.lock().await.remove(&id);
    let Some(sender) = sender else {
        tracing::debug!(id, "deltachat: unmatched response id");
        return;
    };
    let outcome = if let Some(err) = response.error {
        Err(map_rpc_error(&err))
    } else {
        Ok(response.result.unwrap_or(Value::Null))
    };
    let _ = sender.send(outcome);
}

/// Map a JSON-RPC `error` payload to the channel-level [`AdapterError`].
pub fn map_rpc_error(err: &RpcError) -> AdapterError {
    let msg_lower = err.message.to_ascii_lowercase();
    if err.code == 2
        || msg_lower.contains("auth")
        || msg_lower.contains("login")
        || msg_lower.contains("unauthorized")
    {
        return AdapterError::Auth(err.message.clone());
    }
    if msg_lower.contains("rate") && msg_lower.contains("limit") {
        return AdapterError::Rate { retry_after: None };
    }
    AdapterError::BadRequest(err.message.clone())
}

#[async_trait]
impl RpcTransport for SubprocessTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let req = RpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_owned(),
            params,
        };
        let line = serde_json::to_string(&req).map_err(|e| {
            AdapterError::BadRequest(format!("failed to serialise rpc request: {e}"))
        })?;
        if self.writer_tx.send(line).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(AdapterError::Transport(
                "deltachat subprocess writer closed".into(),
            ));
        }
        if let Ok(outcome) = rx.await {
            outcome
        } else {
            self.pending.lock().await.remove(&id);
            Err(AdapterError::Transport(
                "deltachat subprocess dropped response".into(),
            ))
        }
    }

    async fn next_event(&self) -> Result<Value, AdapterError> {
        // The deltachat-rpc-server's `get_next_event` blocks server-side;
        // we invoke it explicitly and return whatever payload comes back.
        self.call("get_next_event", Value::Array(vec![])).await
    }
}

/// Canned response queued in [`MockTransport`].
#[derive(Debug)]
pub struct MockResponse {
    /// Method this response matches against (informational; the mock asserts
    /// against this when [`MockTransport::strict`] is on).
    pub method: String,
    /// `Ok` payload returned by `call`, or `Err` to surface a typed error.
    pub outcome: Result<Value, AdapterError>,
}

impl MockResponse {
    /// Build an `Ok` response.
    pub fn ok(method: impl Into<String>, value: Value) -> Self {
        Self {
            method: method.into(),
            outcome: Ok(value),
        }
    }

    /// Build an `Err` response.
    pub fn err(method: impl Into<String>, error: AdapterError) -> Self {
        Self {
            method: method.into(),
            outcome: Err(error),
        }
    }
}

/// Observed call recorded by [`MockTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedCall {
    /// Method name passed to `call`.
    pub method: String,
    /// Params passed to `call`.
    pub params: Value,
}

/// Deterministic transport used in tests.
///
/// Construct with a queue of canned [`MockResponse`]s and a queue of canned
/// events; `call` and `next_event` pop from the corresponding queues in
/// arrival order.
pub struct MockTransport {
    responses: Mutex<VecDeque<MockResponse>>,
    events: Mutex<VecDeque<Result<Value, AdapterError>>>,
    observed: Mutex<Vec<ObservedCall>>,
    strict: bool,
}

impl MockTransport {
    /// Empty mock.
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            events: Mutex::new(VecDeque::new()),
            observed: Mutex::new(Vec::new()),
            strict: false,
        }
    }

    /// When `strict` is on each `call` asserts that the next queued response
    /// targets the same method name. Useful for tests that want to lock in
    /// the call order.
    #[must_use]
    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Append a canned response to the queue.
    pub async fn push_response(&self, response: MockResponse) {
        self.responses.lock().await.push_back(response);
    }

    /// Append several canned responses.
    pub async fn push_responses(&self, responses: impl IntoIterator<Item = MockResponse>) {
        let mut guard = self.responses.lock().await;
        for r in responses {
            guard.push_back(r);
        }
    }

    /// Append an `Ok` event payload to the event queue.
    pub async fn push_event(&self, event: Value) {
        self.events.lock().await.push_back(Ok(event));
    }

    /// Append an `Err` to the event queue.
    pub async fn push_event_error(&self, err: AdapterError) {
        self.events.lock().await.push_back(Err(err));
    }

    /// Snapshot the observed calls so far.
    pub async fn observed(&self) -> Vec<ObservedCall> {
        self.observed.lock().await.clone()
    }

    /// Number of remaining queued responses.
    pub async fn pending_responses(&self) -> usize {
        self.responses.lock().await.len()
    }

    /// Number of remaining queued events.
    pub async fn pending_events(&self) -> usize {
        self.events.lock().await.len()
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RpcTransport for MockTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        self.observed.lock().await.push(ObservedCall {
            method: method.to_owned(),
            params: params.clone(),
        });
        let next = self.responses.lock().await.pop_front();
        match next {
            Some(resp) => {
                if self.strict && resp.method != method {
                    return Err(AdapterError::BadRequest(format!(
                        "mock: expected `{}`, got `{}`",
                        resp.method, method
                    )));
                }
                resp.outcome
            }
            None => Err(AdapterError::BadRequest(format!(
                "mock transport has no queued response for `{method}`"
            ))),
        }
    }

    async fn next_event(&self) -> Result<Value, AdapterError> {
        // Block briefly so a forwarder loop that polls in a tight retry
        // doesn't spin the CPU when the queue is empty.
        loop {
            if let Some(next) = self.events.lock().await.pop_front() {
                return next;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_rpc_error_auth_by_code() {
        let e = map_rpc_error(&RpcError {
            code: 2,
            message: "boom".into(),
            data: None,
        });
        assert!(matches!(e, AdapterError::Auth(_)));
    }

    #[test]
    fn map_rpc_error_auth_by_message_keyword() {
        let e = map_rpc_error(&RpcError {
            code: 1,
            message: "Authentication failed".into(),
            data: None,
        });
        assert!(matches!(e, AdapterError::Auth(_)));
    }

    #[test]
    fn map_rpc_error_login_by_message_keyword() {
        let e = map_rpc_error(&RpcError {
            code: 1,
            message: "login required".into(),
            data: None,
        });
        assert!(matches!(e, AdapterError::Auth(_)));
    }

    #[test]
    fn map_rpc_error_rate_limit_by_message() {
        let e = map_rpc_error(&RpcError {
            code: 1,
            message: "rate limit exceeded".into(),
            data: None,
        });
        assert!(matches!(e, AdapterError::Rate { retry_after: None }));
    }

    #[test]
    fn map_rpc_error_otherwise_bad_request() {
        let e = map_rpc_error(&RpcError {
            code: 5,
            message: "chat 1 not found".into(),
            data: None,
        });
        match e {
            AdapterError::BadRequest(m) => assert!(m.contains("chat 1")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_rpc_error_unauthorized_message() {
        let e = map_rpc_error(&RpcError {
            code: 1,
            message: "unauthorized".into(),
            data: None,
        });
        assert!(matches!(e, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn mock_transport_call_returns_queued_response() {
        let m = MockTransport::new();
        m.push_response(MockResponse::ok("send_msg", json!(7))).await;
        let v = m.call("send_msg", json!([1, 2, {"text": "hi"}])).await.unwrap();
        assert_eq!(v, json!(7));
    }

    #[tokio::test]
    async fn mock_transport_call_with_no_queued_response_errors() {
        let m = MockTransport::new();
        let err = m.call("send_msg", json!([])).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn mock_transport_call_propagates_error_outcome() {
        let m = MockTransport::new();
        m.push_response(MockResponse::err(
            "send_msg",
            AdapterError::Rate { retry_after: None },
        ))
        .await;
        let err = m.call("send_msg", json!([])).await.unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: None }));
    }

    #[tokio::test]
    async fn mock_transport_records_observed_calls_in_order() {
        let m = MockTransport::new();
        m.push_responses([
            MockResponse::ok("a", json!(1)),
            MockResponse::ok("b", json!(2)),
        ])
        .await;
        m.call("a", json!([1])).await.unwrap();
        m.call("b", json!([2])).await.unwrap();
        let calls = m.observed().await;
        assert_eq!(
            calls,
            vec![
                ObservedCall {
                    method: "a".into(),
                    params: json!([1])
                },
                ObservedCall {
                    method: "b".into(),
                    params: json!([2])
                }
            ]
        );
    }

    #[tokio::test]
    async fn mock_transport_strict_errors_on_wrong_method() {
        let m = MockTransport::new().strict(true);
        m.push_response(MockResponse::ok("a", json!(1))).await;
        let err = m.call("b", json!([])).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("expected `a`")));
    }

    #[tokio::test]
    async fn mock_transport_next_event_pops_queue() {
        let m = MockTransport::new();
        m.push_event(json!({"kind": "Info", "msg": "hi"})).await;
        let evt = m.next_event().await.unwrap();
        assert_eq!(evt["kind"], "Info");
    }

    #[tokio::test]
    async fn mock_transport_next_event_propagates_error() {
        let m = MockTransport::new();
        m.push_event_error(AdapterError::Transport("dead".into())).await;
        let err = m.next_event().await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn mock_transport_next_event_waits_until_pushed() {
        let m = Arc::new(MockTransport::new());
        let m2 = m.clone();
        let handle = tokio::spawn(async move { m2.next_event().await });
        // Give the spawned task a chance to enter its wait loop.
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        m.push_event(json!({"kind": "Info", "msg": "x"})).await;
        let v = handle.await.unwrap().unwrap();
        assert_eq!(v["kind"], "Info");
    }

    #[tokio::test]
    async fn mock_transport_pending_counts_track_queue_size() {
        let m = MockTransport::new();
        assert_eq!(m.pending_responses().await, 0);
        assert_eq!(m.pending_events().await, 0);
        m.push_response(MockResponse::ok("a", json!(1))).await;
        m.push_event(json!({"kind": "Info"})).await;
        assert_eq!(m.pending_responses().await, 1);
        assert_eq!(m.pending_events().await, 1);
        m.call("a", json!([])).await.unwrap();
        m.next_event().await.unwrap();
        assert_eq!(m.pending_responses().await, 0);
        assert_eq!(m.pending_events().await, 0);
    }

    #[tokio::test]
    async fn mock_transport_default_is_empty() {
        let m: MockTransport = MockTransport::default();
        assert_eq!(m.pending_responses().await, 0);
        assert_eq!(m.pending_events().await, 0);
    }

    #[tokio::test]
    async fn mock_transport_keys_responses_by_arrival_order() {
        // Two callers fire in parallel; each gets the next queued response.
        let m = Arc::new(MockTransport::new());
        m.push_responses([
            MockResponse::ok("first", json!("a")),
            MockResponse::ok("second", json!("b")),
        ])
        .await;
        let m1 = m.clone();
        let m2 = m.clone();
        let h1 = tokio::spawn(async move { m1.call("first", json!([])).await });
        // Ensure ordering of observed calls — first task pushes before second.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let h2 = tokio::spawn(async move { m2.call("second", json!([])).await });
        let r1 = h1.await.unwrap().unwrap();
        let r2 = h2.await.unwrap().unwrap();
        assert_eq!(r1, json!("a"));
        assert_eq!(r2, json!("b"));
    }

    #[test]
    fn rpc_request_serialises_with_jsonrpc_field() {
        let req = RpcRequest {
            jsonrpc: "2.0",
            id: 7,
            method: "foo".into(),
            params: json!([1, 2]),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 7);
        assert_eq!(json["method"], "foo");
        assert_eq!(json["params"], json!([1, 2]));
    }

    #[test]
    fn rpc_response_deserialises_ok_shape() {
        let raw = json!({ "id": 1, "result": 42 });
        let r: RpcResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(r.id, 1);
        assert_eq!(r.result, Some(json!(42)));
        assert!(r.error.is_none());
    }

    #[test]
    fn rpc_response_deserialises_err_shape() {
        let raw = json!({
            "id": 9,
            "error": { "code": 1, "message": "boom" }
        });
        let r: RpcResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(r.id, 9);
        assert!(r.result.is_none());
        let err = r.error.unwrap();
        assert_eq!(err.code, 1);
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn rpc_error_data_optional_clone_serde() {
        let e = RpcError {
            code: 1,
            message: "x".into(),
            data: Some(json!({"k": "v"})),
        };
        let copy = e.clone();
        let json = serde_json::to_value(copy).unwrap();
        assert_eq!(json["data"]["k"], "v");
    }

    #[tokio::test]
    async fn handle_line_routes_response_to_pending_caller() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(4);
        let (resp_tx, resp_rx) = oneshot::channel();
        pending.lock().await.insert(3, resp_tx);
        handle_line(
            &serde_json::to_string(&json!({"id": 3, "result": "ok"})).unwrap(),
            &pending,
            &event_tx,
        )
        .await;
        let res = resp_rx.await.unwrap().unwrap();
        assert_eq!(res, json!("ok"));
        // No event delivered.
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_line_emits_event_on_zero_id() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(4);
        handle_line(
            r#"{"id": 0, "params": {"kind": "Info", "msg": "hi"}}"#,
            &pending,
            &event_tx,
        )
        .await;
        let v = event_rx.recv().await.unwrap();
        assert_eq!(v["kind"], "Info");
    }

    #[tokio::test]
    async fn handle_line_routes_error_outcome() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, _event_rx) = mpsc::channel::<Value>(4);
        let (resp_tx, resp_rx) = oneshot::channel();
        pending.lock().await.insert(11, resp_tx);
        handle_line(
            r#"{"id": 11, "error": {"code": 5, "message": "bad chat"}}"#,
            &pending,
            &event_tx,
        )
        .await;
        let err = resp_rx.await.unwrap().unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("bad chat")));
    }

    #[tokio::test]
    async fn handle_line_ignores_invalid_json() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(4);
        handle_line("not json", &pending, &event_tx).await;
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_line_unmatched_id_is_dropped() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, mut event_rx) = mpsc::channel::<Value>(4);
        handle_line(r#"{"id": 99, "result": 1}"#, &pending, &event_tx).await;
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn subprocess_transport_missing_binary_errors() {
        // /no/such/path will not exist; spawn() should error out.
        let res = SubprocessTransport::spawn("/no/such/deltachat-rpc-server", &[]);
        match res {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subprocess_transport_against_cat_routes_call() {
        // We use /bin/cat as a stand-in: it echoes whatever is written to
        // stdin. Build a request whose own id matches what we'll write so
        // the reader matches it against the pending caller.
        let path = std::path::Path::new("/bin/cat");
        if !path.exists() {
            // Skip on systems without /bin/cat.
            return;
        }
        let transport = SubprocessTransport::spawn("/bin/cat", &[]).unwrap();
        // The Subprocess implementation picks the id internally; cat will
        // echo the same line back so the response id matches.
        let res = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            transport.call("ping", json!([])),
        )
        .await;
        // cat won't produce a `result` field, so the response payload is
        // Null after decoding. We just confirm we got *some* value back
        // without timeout — proving the writer + reader plumbing works.
        match res {
            Ok(Ok(v)) => assert!(v.is_null() || v.is_object() || v.is_array() || v.is_string()),
            Ok(Err(err)) => panic!("call errored: {err:?}"),
            Err(elapsed) => panic!("call timed out: {elapsed}"),
        }
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn subprocess_transport_shutdown_is_idempotent() {
        let path = std::path::Path::new("/bin/cat");
        if !path.exists() {
            return;
        }
        let transport = SubprocessTransport::spawn("/bin/cat", &[]).unwrap();
        transport.shutdown().await;
        transport.shutdown().await;
    }

    #[tokio::test]
    async fn subprocess_transport_after_shutdown_call_errors() {
        let path = std::path::Path::new("/bin/cat");
        if !path.exists() {
            return;
        }
        let transport = SubprocessTransport::spawn("/bin/cat", &[]).unwrap();
        transport.shutdown().await;
        let err = transport.call("x", json!([])).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }
}
