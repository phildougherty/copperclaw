//! Subprocess-bridge provider.
//!
//! Several upstream agent CLIs (e.g. `codex`, `opencode`) expose a
//! stdin/stdout JSON-Lines protocol rather than an HTTP API. This module
//! captures the common driver — spawn child, pipe JSON requests in,
//! parse a stream of `ProviderEvent`-equivalent envelopes out — and lets
//! the per-binary providers be near-trivial wrappers.
//!
//! # Bridge protocol
//!
//! The child receives **one** JSON object per line on stdin:
//!
//! ```json
//! {"type":"query","system":"...","history":[...],"tools":[...],
//!  "model":"...","max_tokens":4096,"temperature":null,
//!  "previous_continuation":null}
//! ```
//!
//! Subsequent calls to [`crate::AgentQuery::push`] produce additional
//! lines of the form:
//!
//! ```json
//! {"type":"push","content":"another user message"}
//! ```
//!
//! The child writes one JSON object per line on stdout. The driver maps
//! each line onto a [`ProviderEvent`]:
//!
//! | input envelope                                      | mapped event             |
//! |-----------------------------------------------------|--------------------------|
//! | `{"type":"init","continuation":"..."}`              | [`ProviderEvent::Init`]      |
//! | `{"type":"progress","message":"..."}`               | [`ProviderEvent::Progress`]  |
//! | `{"type":"activity"}`                               | [`ProviderEvent::Activity`]  |
//! | `{"type":"tool_start","name":"...","declared_timeout_ms":n}` | [`ProviderEvent::ToolStart`] |
//! | `{"type":"tool_end"}`                               | [`ProviderEvent::ToolEnd`]   |
//! | `{"type":"result","text":"..."}` (text optional)    | [`ProviderEvent::Result`]    |
//! | `{"type":"error","message":"...","retryable":bool}` | [`ProviderEvent::Error`]     |

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use copperclaw_types::ProviderEvent;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::ProviderError;
use crate::types::{HistoryMessage, QueryInput, ToolDef};
use crate::{AgentProvider, AgentQuery};

/// Configuration for a [`SubprocessProvider`]. Built via the public
/// constructors / setters below; consumed by [`SubprocessProvider::new`].
///
/// A config is cheap to clone — the heavy lifting (spawning the child) only
/// happens inside [`AgentProvider::query`].
#[derive(Debug, Clone)]
pub struct SubprocessConfig {
    /// Stable provider name reported via [`AgentProvider::name`] (e.g.
    /// `"codex"`).
    pub name: String,
    /// Path to the executable to spawn.
    pub binary: PathBuf,
    /// Extra command-line arguments passed verbatim after `binary`.
    pub args: Vec<String>,
    /// How [`AgentQuery::push`] is interpreted. See [`PushPolicy`].
    pub push_policy: PushPolicy,
}

/// How a subprocess provider should treat [`AgentQuery::push`].
///
/// Different upstream CLIs handle mid-turn input differently. Codex spawns
/// per-query and exits when finished, so a push is meaningless; opencode
/// keeps an interactive session and accepts follow-up turns.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PushPolicy {
    /// `push` writes an additional `{"type":"push","content":"..."}` line
    /// to the child's stdin. The child is expected to interpret this as a
    /// follow-up user turn.
    Accept,
    /// `push` is rejected with [`ProviderError::BadRequest`]. Used when the
    /// upstream is single-turn-per-spawn.
    Reject,
}

impl SubprocessConfig {
    /// Minimal constructor. Defaults to [`PushPolicy::Reject`] and no extra
    /// args.
    #[must_use]
    pub fn new(name: impl Into<String>, binary: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            binary: binary.into(),
            args: Vec::new(),
            push_policy: PushPolicy::Reject,
        }
    }

    /// Append extra command-line arguments. Chains.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Choose how [`AgentQuery::push`] is treated.
    #[must_use]
    pub fn with_push_policy(mut self, policy: PushPolicy) -> Self {
        self.push_policy = policy;
        self
    }

    /// Accessor — the configured binary path.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Accessor — extra arguments forwarded to the child.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Accessor — the configured push policy.
    #[must_use]
    pub fn push_policy(&self) -> PushPolicy {
        self.push_policy
    }

    /// Accessor — the provider name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Generic subprocess provider driven by a [`SubprocessConfig`].
///
/// Used directly by [`crate::CodexProvider`] and [`crate::OpenCodeProvider`].
#[derive(Debug, Clone)]
pub struct SubprocessProvider {
    inner: Arc<SubprocessConfig>,
}

impl SubprocessProvider {
    /// Build a provider from a fully-formed config.
    #[must_use]
    pub fn new(cfg: SubprocessConfig) -> Self {
        Self { inner: Arc::new(cfg) }
    }

    /// Accessor — the inner config.
    #[must_use]
    pub fn config(&self) -> &SubprocessConfig {
        &self.inner
    }
}

#[async_trait]
impl AgentProvider for SubprocessProvider {
    fn name(&self) -> &str {
        &self.inner.name
    }

    fn supports_native_slash_commands(&self) -> bool {
        // Subprocess bridges defer slash-command interpretation to the
        // child binary if it wants them; from our perspective the runner
        // still owns them, so report false.
        false
    }

    async fn query(&self, input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
        let mut cmd = Command::new(self.inner.binary.as_os_str());
        for arg in &self.inner.args {
            cmd.arg::<&OsStr>(arg.as_ref());
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Transport(format!("spawn {}: {e}", self.inner.binary.display())))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderError::Transport("child stdin not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderError::Transport("child stdout not piped".to_string()))?;

        // Send the initial query line.
        let req = build_query_line(&input);
        let mut stdin = stdin;
        write_line(&mut stdin, &req)
            .await
            .map_err(|e| ProviderError::Transport(format!("write query: {e}")))?;

        let (tx, rx) = mpsc::channel(32);
        let pump_handle = tokio::spawn(pump_lines(stdout, tx));

        Ok(Box::new(SubprocessQuery {
            child: Some(child),
            stdin: Some(Arc::new(Mutex::new(stdin))),
            rx,
            pump: Some(pump_handle),
            push_policy: self.inner.push_policy,
        }))
    }

    fn is_session_invalid(&self, err: &ProviderError) -> bool {
        matches!(err, ProviderError::SessionInvalid)
    }
}

/// Active turn for a [`SubprocessProvider`].
pub struct SubprocessQuery {
    child: Option<Child>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    rx: mpsc::Receiver<ProviderEvent>,
    pump: Option<JoinHandle<()>>,
    push_policy: PushPolicy,
}

#[async_trait]
impl AgentQuery for SubprocessQuery {
    async fn push(&mut self, message: String) -> Result<(), ProviderError> {
        match self.push_policy {
            PushPolicy::Reject => Err(ProviderError::BadRequest(
                "subprocess provider does not accept mid-turn push".to_string(),
            )),
            PushPolicy::Accept => {
                let Some(stdin) = self.stdin.as_ref() else {
                    return Err(ProviderError::BadRequest("stdin already closed".to_string()));
                };
                let line = json!({ "type": "push", "content": message });
                let mut guard = stdin.lock().await;
                write_line(&mut *guard, &line)
                    .await
                    .map_err(|e| ProviderError::Transport(format!("write push: {e}")))
            }
        }
    }

    async fn end(&mut self) -> Result<(), ProviderError> {
        // Dropping the stdin handle closes the pipe, which the child should
        // interpret as EOF on its input stream.
        self.stdin = None;
        Ok(())
    }

    async fn next_event(&mut self) -> Option<ProviderEvent> {
        self.rx.recv().await
    }

    async fn abort(&mut self) {
        if let Some(handle) = self.pump.take() {
            handle.abort();
        }
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            // Best-effort kill; the child may already have exited.
            let _ = child.start_kill();
            // Reap to avoid zombies; tokio's Drop also takes care of this,
            // but waiting here makes the abort observable in tests.
            let _ = child.wait().await;
        }
        self.rx.close();
    }
}

impl Drop for SubprocessQuery {
    fn drop(&mut self) {
        if let Some(handle) = self.pump.take() {
            handle.abort();
        }
        // `kill_on_drop(true)` plus dropping the `Child` reaps the process.
    }
}

/// Build the initial JSON line sent to the child on stdin.
fn build_query_line(input: &QueryInput) -> Value {
    let history: Vec<Value> = input.history.iter().map(history_message_to_json).collect();
    let tools: Vec<Value> = input.tools.iter().map(tool_to_json).collect();
    let mut obj = json!({
        "type": "query",
        "system": input.system,
        "history": history,
        "tools": tools,
        "model": input.model,
        "max_tokens": input.max_tokens,
        "previous_continuation": input.previous_continuation,
    });
    if let Some(t) = input.temperature {
        obj["temperature"] = json!(t);
    }
    obj
}

fn history_message_to_json(m: &HistoryMessage) -> Value {
    match m {
        HistoryMessage::User { content } => json!({ "role": "user", "content": content }),
        HistoryMessage::Assistant { content } => json!({ "role": "assistant", "content": content }),
        HistoryMessage::ToolUse { id, name, input } => json!({
            "role": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        HistoryMessage::Tool { tool_use_id, content, is_error } => json!({
            "role": "tool",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        HistoryMessage::Image { media_type, data } => json!({
            "role": "image",
            "media_type": media_type,
            "data": data,
        }),
    }
}

fn tool_to_json(t: &ToolDef) -> Value {
    json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.input_schema,
    })
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, line: &Value) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(line).map_err(std::io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf).await?;
    w.flush().await
}

/// Wire envelope coming back on the child's stdout.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    Init { continuation: String },
    Progress { message: String },
    Activity,
    ToolStart {
        name: String,
        #[serde(default)]
        declared_timeout_ms: Option<u64>,
    },
    ToolEnd,
    Result {
        #[serde(default)]
        text: Option<String>,
    },
    Error { message: String, retryable: bool },
}

impl WireEvent {
    fn into_provider_event(self) -> ProviderEvent {
        match self {
            Self::Init { continuation } => ProviderEvent::Init { continuation },
            Self::Progress { message } => ProviderEvent::Progress { message },
            Self::Activity => ProviderEvent::Activity,
            Self::ToolStart { name, declared_timeout_ms } => ProviderEvent::ToolStart {
                name,
                declared_timeout_ms,
            },
            Self::ToolEnd => ProviderEvent::ToolEnd,
            Self::Result { text } => ProviderEvent::Result { text },
            Self::Error { message, retryable } => ProviderEvent::Error { message, retryable },
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Result { .. } | Self::Error { .. })
    }
}

async fn pump_lines(stdout: ChildStdout, tx: mpsc::Sender<ProviderEvent>) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<WireEvent>(trimmed) {
                    Ok(ev) => {
                        let terminal = ev.is_terminal();
                        let pe = ev.into_provider_event();
                        if tx.send(pe).await.is_err() || terminal {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(ProviderEvent::Error {
                                message: format!("decode: {e}"),
                                retryable: false,
                            })
                            .await;
                        return;
                    }
                }
            }
            Ok(None) => {
                // EOF before a terminal event.
                let _ = tx
                    .send(ProviderEvent::Error {
                        message: "subprocess stdout closed before result".to_string(),
                        retryable: true,
                    })
                    .await;
                return;
            }
            Err(e) => {
                let _ = tx
                    .send(ProviderEvent::Error {
                        message: format!("subprocess stdout read error: {e}"),
                        retryable: true,
                    })
                    .await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HistoryMessage, ToolDef};

    fn sh_provider(script: &str) -> SubprocessProvider {
        // Run the canned script via /bin/sh. The script reads its stdin (so
        // it sees the query line and any pushes) and emits a JSON-Lines
        // response on its stdout.
        SubprocessProvider::new(
            SubprocessConfig::new("sh-test", PathBuf::from("/bin/sh"))
                .with_args(["-c", script]),
        )
    }

    #[test]
    fn config_builder_round_trip() {
        let cfg = SubprocessConfig::new("codex", "/usr/bin/codex")
            .with_args(["--json"])
            .with_push_policy(PushPolicy::Accept);
        assert_eq!(cfg.name(), "codex");
        assert_eq!(cfg.binary(), Path::new("/usr/bin/codex"));
        assert_eq!(cfg.args(), &["--json".to_string()]);
        assert_eq!(cfg.push_policy(), PushPolicy::Accept);

        let cloned = cfg.clone();
        assert_eq!(cloned.name(), "codex");
    }

    #[test]
    fn config_default_policy_is_reject() {
        let cfg = SubprocessConfig::new("x", "/bin/true");
        assert_eq!(cfg.push_policy(), PushPolicy::Reject);
        assert!(cfg.args().is_empty());
    }

    #[test]
    fn build_query_line_includes_history_tools_temperature() {
        let mut q = QueryInput::new("sys", "m");
        q.history.push(HistoryMessage::User { content: "hi".into() });
        q.history.push(HistoryMessage::Assistant { content: "ok".into() });
        q.history.push(HistoryMessage::ToolUse {
            id: "tu_1".into(),
            name: "weather".into(),
            input: json!({ "loc": "sf" }),
        });
        q.history.push(HistoryMessage::Tool {
            tool_use_id: "tu_1".into(),
            content: "sunny".into(),
            is_error: false,
        });
        q.tools.push(ToolDef {
            name: "t".into(),
            description: "d".into(),
            input_schema: json!({ "type": "object" }),
        });
        q.temperature = Some(0.25);
        q.previous_continuation = Some("c1".into());

        let line = build_query_line(&q);
        assert_eq!(line["type"], "query");
        assert_eq!(line["system"], "sys");
        assert_eq!(line["model"], "m");
        assert_eq!(line["max_tokens"], 4096);
        assert_eq!(line["previous_continuation"], "c1");
        assert!((line["temperature"].as_f64().unwrap() - 0.25).abs() < 1e-6);

        let history = line["history"].as_array().unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[1]["role"], "assistant");
        assert_eq!(history[2]["role"], "tool_use");
        assert_eq!(history[2]["name"], "weather");
        assert_eq!(history[3]["role"], "tool");
        assert_eq!(history[3]["tool_use_id"], "tu_1");
        assert_eq!(history[3]["is_error"], false);

        let tools = line["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "t");
    }

    #[test]
    fn build_query_line_omits_temperature_when_none() {
        let q = QueryInput::new("s", "m");
        let line = build_query_line(&q);
        assert!(line.get("temperature").is_none());
        assert_eq!(line["previous_continuation"], Value::Null);
    }

    #[test]
    fn wire_event_terminal_classification() {
        assert!(WireEvent::Result { text: None }.is_terminal());
        assert!(WireEvent::Error { message: "x".into(), retryable: false }.is_terminal());
        assert!(!WireEvent::Activity.is_terminal());
        assert!(!WireEvent::Init { continuation: "c".into() }.is_terminal());
        assert!(!WireEvent::Progress { message: "p".into() }.is_terminal());
        assert!(!WireEvent::ToolStart {
            name: "t".into(),
            declared_timeout_ms: Some(1)
        }
        .is_terminal());
        assert!(!WireEvent::ToolEnd.is_terminal());
    }

    #[test]
    fn wire_event_into_provider_event_maps_every_variant() {
        // Init.
        match (WireEvent::Init { continuation: "c".into() }).into_provider_event() {
            ProviderEvent::Init { continuation } => assert_eq!(continuation, "c"),
            other => panic!("expected init, got {other:?}"),
        }
        // Progress.
        match (WireEvent::Progress { message: "p".into() }).into_provider_event() {
            ProviderEvent::Progress { message } => assert_eq!(message, "p"),
            other => panic!("expected progress, got {other:?}"),
        }
        // Activity.
        assert!(matches!(
            WireEvent::Activity.into_provider_event(),
            ProviderEvent::Activity
        ));
        // ToolStart.
        match (WireEvent::ToolStart {
            name: "t".into(),
            declared_timeout_ms: Some(10),
        })
        .into_provider_event()
        {
            ProviderEvent::ToolStart { name, declared_timeout_ms } => {
                assert_eq!(name, "t");
                assert_eq!(declared_timeout_ms, Some(10));
            }
            other => panic!("expected tool_start, got {other:?}"),
        }
        // ToolEnd.
        assert!(matches!(
            WireEvent::ToolEnd.into_provider_event(),
            ProviderEvent::ToolEnd
        ));
        // Result.
        match (WireEvent::Result { text: Some("ok".into()) }).into_provider_event() {
            ProviderEvent::Result { text } => assert_eq!(text.as_deref(), Some("ok")),
            other => panic!("expected result, got {other:?}"),
        }
        // Error.
        match (WireEvent::Error {
            message: "boom".into(),
            retryable: true,
        })
        .into_provider_event()
        {
            ProviderEvent::Error { message, retryable } => {
                assert_eq!(message, "boom");
                assert!(retryable);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn happy_path_init_progress_result() {
        // Read all stdin (so the parent's write doesn't EPIPE), then emit a
        // few canned lines.
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_1\"}'; \
             printf '%s\\n' '{\"type\":\"progress\",\"message\":\"thinking\"}'; \
             printf '%s\\n' '{\"type\":\"result\",\"text\":\"hello world\"}'";
        let p = sh_provider(script);
        assert_eq!(p.name(), "sh-test");
        let mut q = p.query(QueryInput::new("sys", "m")).await.unwrap();
        // Drop stdin so `cat > /dev/null` can finish.
        q.end().await.unwrap();
        let first = q.next_event().await.unwrap();
        match first {
            ProviderEvent::Init { continuation } => assert_eq!(continuation, "cx_1"),
            other => panic!("expected init, got {other:?}"),
        }
        let mut got_result = None;
        while let Some(ev) = q.next_event().await {
            match ev {
                ProviderEvent::Progress { message } => assert_eq!(message, "thinking"),
                ProviderEvent::Result { text } => {
                    got_result = Some(text);
                    break;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(got_result.unwrap().as_deref(), Some("hello world"));
        // Stream ended.
        assert!(q.next_event().await.is_none());
    }

    #[tokio::test]
    async fn tool_use_round_trip() {
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_2\"}'; \
             printf '%s\\n' '{\"type\":\"tool_start\",\"name\":\"weather\",\"declared_timeout_ms\":5000}'; \
             printf '%s\\n' '{\"type\":\"tool_end\"}'; \
             printf '%s\\n' '{\"type\":\"result\",\"text\":\"sunny\"}'";
        let p = sh_provider(script);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        let mut saw_start = false;
        let mut saw_end = false;
        let mut saw_result = false;
        while let Some(ev) = q.next_event().await {
            match ev {
                ProviderEvent::Init { .. } => {}
                ProviderEvent::ToolStart {
                    name,
                    declared_timeout_ms,
                } => {
                    assert_eq!(name, "weather");
                    assert_eq!(declared_timeout_ms, Some(5000));
                    saw_start = true;
                }
                ProviderEvent::ToolEnd => saw_end = true,
                ProviderEvent::Result { text } => {
                    assert_eq!(text.as_deref(), Some("sunny"));
                    saw_result = true;
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(saw_start && saw_end && saw_result);
    }

    #[tokio::test]
    async fn error_event_is_forwarded() {
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_3\"}'; \
             printf '%s\\n' '{\"type\":\"error\",\"message\":\"upstream sad\",\"retryable\":true}'";
        let p = sh_provider(script);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        let _ = q.next_event().await.unwrap();
        let next = q.next_event().await.unwrap();
        match next {
            ProviderEvent::Error { message, retryable } => {
                assert_eq!(message, "upstream sad");
                assert!(retryable);
            }
            other => panic!("expected error, got {other:?}"),
        }
        assert!(q.next_event().await.is_none());
    }

    #[tokio::test]
    async fn malformed_json_line_emits_decode_error() {
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_4\"}'; \
             printf '%s\\n' 'not-json'";
        let p = sh_provider(script);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        let _ = q.next_event().await.unwrap();
        let next = q.next_event().await.unwrap();
        match next {
            ProviderEvent::Error { message, retryable } => {
                assert!(message.starts_with("decode: "), "got: {message}");
                assert!(!retryable);
            }
            other => panic!("expected decode error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subprocess_crash_mid_stream() {
        // Init, then exit with non-zero status. The pump should observe
        // stdout EOF and emit a transport-level error.
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_5\"}'; \
             exit 1";
        let p = sh_provider(script);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        let _ = q.next_event().await.unwrap();
        let next = q.next_event().await.unwrap();
        match next {
            ProviderEvent::Error { message, retryable } => {
                assert!(message.contains("closed"), "got: {message}");
                assert!(retryable);
            }
            other => panic!("expected close error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_failure_is_transport_error() {
        let p = SubprocessProvider::new(SubprocessConfig::new(
            "missing",
            "/nonexistent/path/definitely-not-here",
        ));
        let r = p.query(QueryInput::new("s", "m")).await;
        match r {
            Err(ProviderError::Transport(msg)) => {
                assert!(msg.starts_with("spawn "), "got: {msg}");
            }
            Ok(_) => panic!("expected transport err"),
            Err(other) => panic!("expected transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_rejected_under_reject_policy() {
        let script = "sleep 5";
        let p = sh_provider(script);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        let r = q.push("hi".into()).await;
        match r {
            Err(ProviderError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
        q.abort().await;
    }

    #[tokio::test]
    async fn push_accepted_under_accept_policy() {
        // The script consumes one query line, then waits for a push line,
        // echoes it as a Result.
        let script = "read query; read push; \
             echo \"$push\" | grep -q '\"type\":\"push\"' || { echo '{\"type\":\"error\",\"message\":\"no push\",\"retryable\":false}'; exit 0; }; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_p\"}'; \
             printf '%s\\n' '{\"type\":\"result\",\"text\":\"got push\"}'";
        let cfg = SubprocessConfig::new("sh-push", PathBuf::from("/bin/sh"))
            .with_args(["-c", script])
            .with_push_policy(PushPolicy::Accept);
        let p = SubprocessProvider::new(cfg);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.push("hi there".into()).await.unwrap();
        q.end().await.unwrap();

        let mut saw_result = false;
        while let Some(ev) = q.next_event().await {
            if let ProviderEvent::Result { text } = ev {
                assert_eq!(text.as_deref(), Some("got push"));
                saw_result = true;
                break;
            }
        }
        assert!(saw_result);
    }

    #[tokio::test]
    async fn push_after_end_is_rejected() {
        let script = "cat > /dev/null; \
             printf '%s\\n' '{\"type\":\"init\",\"continuation\":\"cx_e\"}'; \
             printf '%s\\n' '{\"type\":\"result\",\"text\":\"ok\"}'";
        let cfg = SubprocessConfig::new("sh-end", PathBuf::from("/bin/sh"))
            .with_args(["-c", script])
            .with_push_policy(PushPolicy::Accept);
        let p = SubprocessProvider::new(cfg);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        let r = q.push("hi".into()).await;
        match r {
            Err(ProviderError::BadRequest(msg)) => {
                assert!(msg.contains("closed"));
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abort_kills_child() {
        // Child reads stdin forever; only abort can stop it.
        let script = "cat";
        let p = sh_provider(script);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.abort().await;
        // After abort, the channel is closed; next_event yields None.
        assert!(q.next_event().await.is_none());
    }

    #[tokio::test]
    async fn provider_name_and_flags() {
        let p = sh_provider("true");
        assert_eq!(p.name(), "sh-test");
        assert!(!p.supports_native_slash_commands());
        assert!(p.is_session_invalid(&ProviderError::SessionInvalid));
        assert!(!p.is_session_invalid(&ProviderError::Cancelled));
    }

    #[tokio::test]
    async fn provider_clone_shares_config() {
        let p = sh_provider("true");
        let cloned = p.clone();
        assert_eq!(cloned.name(), p.name());
        assert_eq!(cloned.config().binary(), p.config().binary());
    }

    #[tokio::test]
    async fn end_is_idempotent() {
        let script = "cat > /dev/null; printf '%s\\n' '{\"type\":\"result\"}'";
        let p = sh_provider(script);
        let mut q = p.query(QueryInput::new("s", "m")).await.unwrap();
        q.end().await.unwrap();
        // Second end is a no-op.
        q.end().await.unwrap();
    }
}
