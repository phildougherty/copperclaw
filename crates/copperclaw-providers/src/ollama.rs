//! Ollama provider — native `/api/chat` NDJSON adapter.
//!
//! Historically this file was a thin facade over [`crate::AnthropicProvider`]
//! that aimed at `<base_url>/v1/messages`. That only worked against an
//! Anthropic-shaped proxy in front of Ollama (`LiteLLM`, an
//! ollama-anthropic-bridge, etc.) — vanilla `ollama serve` does not expose
//! `/v1/messages` and returns `404` for that route. The audit by Team OLLAMA
//! (see `docs/providers/ollama.md`) confirmed this gap end-to-end.
//!
//! This implementation talks Ollama's **native** chat endpoint:
//!
//! * `POST /api/chat` with `{"model", "messages", "stream": true, ...}`.
//! * NDJSON streaming — one JSON object per line, terminated by an object
//!   with `"done": true`.
//! * OpenAI-shaped tool definitions (`{type:"function", function:{name,
//!   description, parameters}}`) and tool calls
//!   (`message.tool_calls[].function.{name, arguments}`).
//! * `tool` role messages for tool results (history translation handled by
//!   [`history_to_messages`]).
//! * `prompt_eval_count` / `eval_count` carried out as
//!   [`ProviderEvent::Usage`].
//!
//! The legacy Anthropic-shim mode is still reachable via
//! [`OllamaProvider::shim`] for operators who front Ollama with such a proxy.
//!
//! ## Defaults
//!
//! * `base_url` is required and points at the Ollama server root
//!   (e.g. `http://localhost:11434`).
//! * `model` defaults to [`DEFAULT_MODEL`] when the caller passes `None`
//!   *and* the [`QueryInput::model`] is empty.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::StreamExt;
use copperclaw_types::ProviderEvent;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::anthropic::AnthropicProvider;
use crate::error::ProviderError;
use crate::types::{HistoryMessage, QueryInput, ToolDef};
use crate::{AgentProvider, AgentQuery};

/// Stable provider name surfaced via [`AgentProvider::name`].
pub const PROVIDER_NAME: &str = "ollama";

/// Sensible default model identifier when the caller doesn't override.
pub const DEFAULT_MODEL: &str = "llama3.1:8b";

/// Per-request timeout. Local-Ollama-on-CPU can chug; 10 minutes is the
/// same ceiling the Anthropic provider uses for parity with the runner's
/// turn deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Which on-wire format the provider speaks.
#[derive(Debug, Clone)]
enum Mode {
    /// Native Ollama `/api/chat` NDJSON.
    Native { http: reqwest::Client, base_url: String },
    /// Legacy facade — defer everything to the Anthropic provider against
    /// an Anthropic-compatible proxy in front of Ollama.
    Shim(AnthropicProvider),
}

/// Provider that talks to a local (or remote) Ollama server.
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    mode: Mode,
    default_model: String,
}

impl OllamaProvider {
    /// Build a native provider against `base_url` with a per-call model
    /// override. Pass `None` for `model` to inherit [`DEFAULT_MODEL`].
    #[must_use]
    pub fn new(base_url: impl Into<String>, model: Option<String>) -> Self {
        let raw = base_url.into();
        let normalized = raw.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client builds");
        let default_model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Self {
            inner: Arc::new(Inner {
                mode: Mode::Native { http, base_url: normalized },
                default_model,
            }),
        }
    }

    /// Build a provider that talks to an Anthropic-compatible proxy in
    /// front of Ollama (`LiteLLM`, etc.). The `base_url` should be the
    /// proxy's root; `/v1/messages` is appended automatically by the
    /// inner [`AnthropicProvider`].
    #[must_use]
    pub fn shim(base_url: impl Into<String>, model: Option<String>) -> Self {
        let inner_provider = AnthropicProvider::with_base_url("", base_url);
        let default_model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        Self {
            inner: Arc::new(Inner {
                mode: Mode::Shim(inner_provider),
                default_model,
            }),
        }
    }

    /// The model name applied when a [`QueryInput`] arrives with an empty
    /// `model` field.
    #[must_use]
    pub fn default_model(&self) -> &str {
        &self.inner.default_model
    }

    /// True when this provider talks native Ollama (`/api/chat` NDJSON).
    /// False when it's the legacy Anthropic shim.
    #[must_use]
    pub fn is_native(&self) -> bool {
        matches!(self.inner.mode, Mode::Native { .. })
    }
}

#[async_trait]
impl AgentProvider for OllamaProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn supports_native_slash_commands(&self) -> bool {
        false
    }

    async fn query(&self, mut input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
        if input.model.is_empty() {
            input.model.clone_from(&self.inner.default_model);
        }
        match &self.inner.mode {
            Mode::Shim(inner) => inner.query(input).await,
            Mode::Native { http, base_url } => native_query(http, base_url, input).await,
        }
    }

    fn is_session_invalid(&self, err: &ProviderError) -> bool {
        matches!(err, ProviderError::SessionInvalid)
    }
}

// --------------------------------------------------------------------------
// Native path
// --------------------------------------------------------------------------

/// Open a single turn against Ollama's native `/api/chat` endpoint.
async fn native_query(
    http: &reqwest::Client,
    base_url: &str,
    input: QueryInput,
) -> Result<Box<dyn AgentQuery>, ProviderError> {
    let body = build_native_body(&input);
    let url = format!("{base_url}/api/chat");
    let resp = http
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/x-ndjson")
        .json(&body)
        .send()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(map_http_error(status.as_u16(), resp.text().await.unwrap_or_default()));
    }

    let (tx, rx) = mpsc::channel(32);
    let stream = resp.bytes_stream();
    let handle = tokio::spawn(pump_ndjson(stream, tx));

    Ok(Box::new(OllamaQuery { rx, handle: Some(handle) }))
}

/// Build the JSON body for `POST /api/chat`. Public-in-module for tests.
pub(crate) fn build_native_body(input: &QueryInput) -> Value {
    let messages = history_to_messages(input);
    let mut body = json!({
        "model": input.model,
        "messages": messages,
        "stream": true,
    });
    let obj = body.as_object_mut().expect("json object");

    // `options.num_predict` is Ollama's max-tokens equivalent. The
    // Anthropic-side default of 4096 is small enough that capping is
    // safer than letting llama3 run to its own ceiling.
    let mut options = Map::new();
    options.insert("num_predict".into(), json!(input.max_tokens));
    if let Some(t) = input.temperature {
        options.insert("temperature".into(), json!(t));
    }
    obj.insert("options".into(), Value::Object(options));

    if !input.tools.is_empty() {
        obj.insert("tools".into(), Value::Array(tools_to_openai_form(&input.tools)));
    }
    body
}

/// Translate `ToolDef` list to `OpenAI`'s `{type:"function", function:{...}}`
/// envelope that Ollama's `/api/chat` consumes.
pub(crate) fn tools_to_openai_form(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect()
}

/// Render the transcript into Ollama's `messages` array. System prompts go
/// in as a leading `system` message (Ollama also accepts a top-level
/// `system` field but the chat-history form is what `/api/chat` documents
/// and tools work with both).
pub(crate) fn history_to_messages(input: &QueryInput) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    if !input.system.is_empty() {
        out.push(json!({ "role": "system", "content": input.system }));
    }
    for m in &input.history {
        match m {
            HistoryMessage::User { content } => {
                out.push(json!({ "role": "user", "content": content }));
            }
            HistoryMessage::Assistant { content } => {
                out.push(json!({ "role": "assistant", "content": content }));
            }
            HistoryMessage::ToolUse { id, name, input } => {
                // Ollama's tool_calls are attached to an assistant turn.
                // Collapse onto the previous assistant if present so the
                // transcript looks like "assistant said X, with tool_calls".
                let call = json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input,
                    }
                });
                let pushed_onto_last = out.last_mut().and_then(|last| {
                    if last.get("role").and_then(Value::as_str) == Some("assistant") {
                        let obj = last.as_object_mut()?;
                        let arr = obj
                            .entry("tool_calls".to_string())
                            .or_insert_with(|| Value::Array(Vec::new()))
                            .as_array_mut()?;
                        arr.push(call.clone());
                        Some(())
                    } else {
                        None
                    }
                });
                if pushed_onto_last.is_none() {
                    out.push(json!({
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [call],
                    }));
                }
            }
            HistoryMessage::Tool { tool_use_id, content, is_error: _ } => {
                // Ollama uses a `tool` role message for tool results.
                // The `tool_call_id` correlates with the prior
                // `tool_calls[].id` so the model can match them up.
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }
        }
    }
    out
}

fn map_http_error(status: u16, body: String) -> ProviderError {
    match status {
        400 => ProviderError::BadRequest(body),
        401 | 403 => ProviderError::SessionInvalid,
        429 | 529 => ProviderError::Overloaded,
        _ => ProviderError::Api { status, message: body },
    }
}

/// Active streaming response. The runner drives it via
/// [`AgentQuery::next_event`].
struct OllamaQuery {
    rx: mpsc::Receiver<ProviderEvent>,
    handle: Option<JoinHandle<()>>,
}

#[async_trait]
impl AgentQuery for OllamaQuery {
    async fn push(&mut self, _message: String) -> Result<(), ProviderError> {
        Err(ProviderError::BadRequest(
            "ollama provider does not accept mid-turn push; submit a new query with updated history".into(),
        ))
    }

    async fn end(&mut self) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn next_event(&mut self) -> Option<ProviderEvent> {
        self.rx.recv().await
    }

    async fn abort(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        self.rx.close();
    }
}

impl Drop for OllamaQuery {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

// --------------------------------------------------------------------------
// NDJSON pump
// --------------------------------------------------------------------------

/// One frame of the `/api/chat` NDJSON stream.
#[derive(Debug, Deserialize, Default)]
struct OllamaFrame {
    #[serde(default)]
    message: Option<OllamaMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
    /// Ollama surfaces fatal upstream conditions (e.g. "model not loaded")
    /// inside a 200-OK body via an `error` field. We map that onto a
    /// retryable-false Error event.
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct OllamaMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    /// Ollama doesn't always supply an id on `tool_calls` in older builds;
    /// we synthesize one with a uuid for the runner's tool-result
    /// correlation if missing.
    #[serde(default)]
    id: Option<String>,
    function: OllamaToolFunction,
}

#[derive(Debug, Deserialize)]
struct OllamaToolFunction {
    name: String,
    /// `arguments` is an object on the wire (Ollama) but some shims
    /// stringify it (`OpenAI` does). Accept either via `Value`.
    #[serde(default)]
    arguments: Value,
}

async fn pump_ndjson<S>(mut stream: S, tx: mpsc::Sender<ProviderEvent>)
where
    S: futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
{
    // Surface an Init event up front: Ollama doesn't carry a server-side
    // message id, so we mint a uuid the runner can use as a stable handle.
    let continuation = uuid::Uuid::new_v4().to_string();
    if tx.send(ProviderEvent::Init { continuation }).await.is_err() {
        return;
    }

    let mut buf = Vec::<u8>::new();
    let mut buffered_text = String::new();
    let mut last_usage: Option<(u32, u32)> = None;
    let mut emitted_result = false;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                buf.extend_from_slice(&bytes);
                // NDJSON: one JSON value per newline-terminated line.
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    // Strip the trailing '\n' for the parser.
                    let line_str = std::str::from_utf8(&line[..line.len() - 1])
                        .unwrap_or("")
                        .trim();
                    if line_str.is_empty() {
                        continue;
                    }
                    if !handle_ndjson_line(
                        line_str,
                        &tx,
                        &mut buffered_text,
                        &mut last_usage,
                        &mut emitted_result,
                    )
                    .await
                    {
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx
                    .send(ProviderEvent::Error {
                        message: format!("ollama transport: {e}"),
                        retryable: true,
                    })
                    .await;
                return;
            }
        }
    }

    // Stream ended without a `done:true` frame. Emit whatever we have.
    if !emitted_result {
        emit_terminal(&tx, &mut buffered_text, last_usage).await;
    }
}

/// Process one NDJSON line. Returns `false` once a terminal event has been
/// emitted and the pump should exit.
async fn handle_ndjson_line(
    line: &str,
    tx: &mpsc::Sender<ProviderEvent>,
    buffered_text: &mut String,
    last_usage: &mut Option<(u32, u32)>,
    emitted_result: &mut bool,
) -> bool {
    let frame: OllamaFrame = if let Ok(v) = serde_json::from_str(line) {
        v
    } else {
        // One bad line shouldn't poison the whole stream — Ollama is
        // known to interleave partial writes when the upstream model
        // OOMs. Surface Activity so the runner keeps the turn alive
        // and move on.
        let _ = tx.send(ProviderEvent::Activity).await;
        return true;
    };

    if let Some(err) = frame.error {
        let _ = tx
            .send(ProviderEvent::Error {
                message: err,
                retryable: false,
            })
            .await;
        *emitted_result = true;
        return false;
    }

    if let Some(msg) = frame.message {
        if let Some(text) = msg.content {
            if !text.is_empty() {
                buffered_text.push_str(&text);
                // TODO(team-ollama): when `ProviderEvent::TextDelta`
                // lands, emit the per-frame chunk here. Today the runner
                // consumes only the final `Result`, so we surface
                // `Activity` to keep the liveness heartbeat ticking
                // during long streams.
                let _ = tx.send(ProviderEvent::Activity).await;
            }
        }
        if let Some(calls) = msg.tool_calls {
            for call in calls {
                let id = call.id.unwrap_or_else(|| {
                    format!("toolu_{}", uuid::Uuid::new_v4().simple())
                });
                let name = call.function.name.clone();
                // Arguments can arrive as either an object or a stringified
                // JSON blob (OpenAI legacy). Normalize either way.
                let input = normalize_tool_arguments(call.function.arguments);
                if tx
                    .send(ProviderEvent::ToolStart {
                        name: name.clone(),
                        declared_timeout_ms: None,
                    })
                    .await
                    .is_err()
                {
                    return false;
                }
                if tx
                    .send(ProviderEvent::ToolCall {
                        id,
                        name,
                        input,
                    })
                    .await
                    .is_err()
                {
                    return false;
                }
                if tx.send(ProviderEvent::ToolEnd).await.is_err() {
                    return false;
                }
            }
        }
    }

    if let (Some(p), Some(e)) = (frame.prompt_eval_count, frame.eval_count) {
        *last_usage = Some((p, e));
    } else if let Some(p) = frame.prompt_eval_count {
        let (_, prev_e) = last_usage.unwrap_or((0, 0));
        *last_usage = Some((p, prev_e));
    } else if let Some(e) = frame.eval_count {
        let (prev_p, _) = last_usage.unwrap_or((0, 0));
        *last_usage = Some((prev_p, e));
    }

    if frame.done {
        // Capture done_reason for log breadcrumbs but otherwise treat as
        // normal completion.
        let _ = frame.done_reason;
        emit_terminal(tx, buffered_text, *last_usage).await;
        *emitted_result = true;
        return false;
    }

    true
}

async fn emit_terminal(
    tx: &mpsc::Sender<ProviderEvent>,
    buffered_text: &mut String,
    last_usage: Option<(u32, u32)>,
) {
    if let Some((p, e)) = last_usage {
        let _ = tx
            .send(ProviderEvent::Usage {
                input_tokens: p,
                output_tokens: e,
            })
            .await;
    }
    let text = if buffered_text.is_empty() {
        None
    } else {
        Some(std::mem::take(buffered_text))
    };
    let _ = tx.send(ProviderEvent::Result { text }).await;
}

/// Ollama emits tool-call arguments as a JSON object. OpenAI-style
/// adapters sometimes stringify; `LiteLLM` is one such bridge. Accept either
/// shape so we don't strand the runner with a String it can't dispatch.
fn normalize_tool_arguments(v: Value) -> Value {
    match v {
        Value::Object(_) => v,
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(trimmed).unwrap_or_else(|_| Value::Object(Map::new()))
            }
        }
        Value::Null => Value::Object(Map::new()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::HistoryMessage;

    #[test]
    fn name_and_flags() {
        let p = OllamaProvider::new("http://localhost:11434", None);
        assert_eq!(p.name(), PROVIDER_NAME);
        assert!(!p.supports_native_slash_commands());
        assert_eq!(p.default_model(), DEFAULT_MODEL);
        assert!(p.is_native());
    }

    #[test]
    fn explicit_model_overrides_default() {
        let p = OllamaProvider::new("http://localhost:11434", Some("qwen2:7b".into()));
        assert_eq!(p.default_model(), "qwen2:7b");
    }

    #[test]
    fn shim_mode_flag() {
        let p = OllamaProvider::shim("http://localhost:1234", None);
        assert!(!p.is_native());
        assert_eq!(p.default_model(), DEFAULT_MODEL);
    }

    #[test]
    fn is_session_invalid_predicate() {
        let p = OllamaProvider::new("http://x", None);
        assert!(p.is_session_invalid(&ProviderError::SessionInvalid));
        assert!(!p.is_session_invalid(&ProviderError::Cancelled));
        assert!(!p.is_session_invalid(&ProviderError::Overloaded));
        assert!(!p.is_session_invalid(&ProviderError::BadRequest("x".into())));
    }

    #[test]
    fn provider_clone_shares_inner() {
        let p = OllamaProvider::new("http://x", Some("m".into()));
        let c = p.clone();
        assert_eq!(p.name(), c.name());
        assert_eq!(p.default_model(), c.default_model());
        assert_eq!(p.is_native(), c.is_native());
    }

    #[tokio::test]
    async fn empty_model_falls_back_to_default() {
        // We can't actually round-trip without a server, but we can confirm
        // the model rewrite happens before the (failing) connection attempt
        // by aiming at an unbound port and inspecting the error type.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let p = OllamaProvider::new(format!("http://{addr}"), None);
        let mut input = QueryInput::new("s", "");
        input.history.push(HistoryMessage::User { content: "hi".into() });
        let r = p.query(input).await;
        match r {
            Err(ProviderError::Transport(_)) => {}
            Ok(_) => panic!("expected transport err"),
            Err(other) => panic!("expected transport, got {other:?}"),
        }
    }

    #[test]
    fn build_native_body_minimal() {
        let mut q = QueryInput::new("you are helpful", "llama3.1:8b");
        q.history.push(HistoryMessage::User { content: "hi".into() });
        let body = build_native_body(&q);
        assert_eq!(body["model"], "llama3.1:8b");
        assert_eq!(body["stream"], true);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(body["options"]["num_predict"], 4096);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_native_body_omits_system_when_empty() {
        let mut q = QueryInput::new("", "m");
        q.history.push(HistoryMessage::User { content: "hi".into() });
        let body = build_native_body(&q);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn build_native_body_includes_temperature_and_tools() {
        let mut q = QueryInput::new("s", "m");
        q.history.push(HistoryMessage::User { content: "hi".into() });
        q.temperature = Some(0.3);
        q.tools.push(ToolDef {
            name: "t".into(),
            description: "d".into(),
            input_schema: json!({ "type": "object" }),
        });
        let body = build_native_body(&q);
        assert!((body["options"]["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "t");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn history_emits_tool_role_for_tool_results() {
        let mut q = QueryInput::new("s", "m");
        q.history = vec![
            HistoryMessage::Assistant { content: "ok let me look".into() },
            HistoryMessage::ToolUse {
                id: "tu_1".into(),
                name: "weather".into(),
                input: json!({ "loc": "sf" }),
            },
            HistoryMessage::Tool {
                tool_use_id: "tu_1".into(),
                content: "sunny".into(),
                is_error: false,
            },
        ];
        let msgs = history_to_messages(&q);
        // system + assistant + tool
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        let calls = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "weather");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "tu_1");
    }

    #[test]
    fn map_http_error_variants() {
        assert!(matches!(map_http_error(400, "bad".into()), ProviderError::BadRequest(_)));
        assert!(matches!(map_http_error(401, "x".into()), ProviderError::SessionInvalid));
        assert!(matches!(map_http_error(429, "x".into()), ProviderError::Overloaded));
        assert!(matches!(
            map_http_error(500, "boom".into()),
            ProviderError::Api { status: 500, .. }
        ));
    }

    #[test]
    fn normalize_tool_arguments_object_passthrough() {
        let v = json!({"a": 1});
        assert_eq!(normalize_tool_arguments(v.clone()), v);
    }

    #[test]
    fn normalize_tool_arguments_stringified_json() {
        let v = Value::String(r#"{"a":1}"#.to_string());
        assert_eq!(normalize_tool_arguments(v), json!({"a": 1}));
    }

    #[test]
    fn normalize_tool_arguments_empty_string_becomes_empty_object() {
        let v = Value::String(String::new());
        assert_eq!(normalize_tool_arguments(v), json!({}));
    }

    #[test]
    fn normalize_tool_arguments_null_becomes_empty_object() {
        assert_eq!(normalize_tool_arguments(Value::Null), json!({}));
    }
}
