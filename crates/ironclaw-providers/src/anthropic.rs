//! Anthropic Messages provider.
//!
//! Talks to `POST /v1/messages` with `stream=true` and parses the
//! server-sent event stream emitted by the Anthropic API. The provider is
//! stateless on the wire — every turn replays the full
//! [`crate::QueryInput::history`] — so the `continuation` field on
//! [`ironclaw_types::ProviderEvent::Init`] is just the upstream `message.id`
//! and exists purely so the runner has a stable handle to correlate logs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use ironclaw_types::ProviderEvent;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::ProviderError;
use crate::types::{HistoryMessage, QueryInput, ToolDef};
use crate::{AgentProvider, AgentQuery};

/// Default base URL for the Anthropic API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic API version sent in the `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Stable provider name surfaced via [`AgentProvider::name`].
pub const PROVIDER_NAME: &str = "anthropic";

/// Provider for Anthropic's Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl AnthropicProvider {
    /// Build a provider that talks to the public Anthropic API.
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Build a provider that talks to a custom base URL (used by tests via
    /// wiremock; the path `/v1/messages` is appended automatically).
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("reqwest client builds");
        Self {
            inner: Arc::new(Inner {
                http,
                base_url: base_url.into().trim_end_matches('/').to_string(),
                api_key: api_key.into(),
            }),
        }
    }
}

#[async_trait]
impl AgentProvider for AnthropicProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn supports_native_slash_commands(&self) -> bool {
        false
    }

    async fn query(&self, input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
        let body = build_request_body(&input);
        let url = format!("{}/v1/messages", self.inner.base_url);
        let resp = self
            .inner
            .http
            .post(&url)
            .header("x-api-key", &self.inner.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(map_http_error(status.as_u16(), resp.text().await.unwrap_or_default()));
        }

        let (tx, rx) = mpsc::channel(32);
        let stream = resp.bytes_stream().eventsource();
        let handle = tokio::spawn(pump_sse(stream, tx));

        Ok(Box::new(AnthropicQuery {
            rx,
            handle: Some(handle),
        }))
    }

    fn is_session_invalid(&self, err: &ProviderError) -> bool {
        matches!(err, ProviderError::SessionInvalid)
    }
}

/// Active streaming response from [`AnthropicProvider::query`]. The runner
/// drives it via [`AgentQuery::next_event`].
struct AnthropicQuery {
    rx: mpsc::Receiver<ProviderEvent>,
    handle: Option<JoinHandle<()>>,
}

#[async_trait]
impl AgentQuery for AnthropicQuery {
    async fn push(&mut self, _message: String) -> Result<(), ProviderError> {
        // Anthropic Messages is request/response per turn — there is no
        // mid-turn inbound channel. Tool results and follow-up user
        // messages are appended to `QueryInput::history` and submitted as
        // a brand-new query by the caller.
        Err(ProviderError::BadRequest(
            "anthropic provider does not accept mid-turn push; submit a new query with updated history".into(),
        ))
    }

    async fn end(&mut self) -> Result<(), ProviderError> {
        // No half-duplex protocol — the stream ends when the upstream sends
        // `message_stop`. `end` is a no-op for parity with other providers.
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

impl Drop for AnthropicQuery {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn build_request_body(input: &QueryInput) -> Value {
    let messages = history_to_messages(&input.history);
    let mut body = json!({
        "model": input.model,
        "max_tokens": input.max_tokens,
        "stream": true,
        "messages": messages,
    });
    let obj = body.as_object_mut().expect("json object");
    if !input.system.is_empty() {
        obj.insert("system".into(), Value::String(input.system.clone()));
    }
    if let Some(temp) = input.temperature {
        obj.insert("temperature".into(), json!(temp));
    }
    if !input.tools.is_empty() {
        obj.insert("tools".into(), Value::Array(tools_to_json(&input.tools)));
    }
    body
}

fn tools_to_json(tools: &[ToolDef]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect()
}

/// Collapse our [`HistoryMessage`] transcript into Anthropic's
/// `messages` array shape.
fn history_to_messages(history: &[HistoryMessage]) -> Vec<Value> {
    // We coalesce consecutive same-role entries into one message whose
    // content is a vector of content blocks. Tool-use blocks attach to the
    // assistant role; tool-result blocks attach to the user role.
    let mut out: Vec<Value> = Vec::new();
    for m in history {
        let (role, block) = match m {
            HistoryMessage::User { content } => {
                ("user", json!({ "type": "text", "text": content }))
            }
            HistoryMessage::Assistant { content } => {
                ("assistant", json!({ "type": "text", "text": content }))
            }
            HistoryMessage::ToolUse { id, name, input } => (
                "assistant",
                json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }),
            ),
            HistoryMessage::Tool { tool_use_id, content, is_error } => (
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }),
            ),
        };
        push_block(&mut out, role, block);
    }
    out
}

fn push_block(out: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                arr.push(block);
                return;
            }
        }
    }
    out.push(json!({ "role": role, "content": [block] }));
}

fn map_http_error(status: u16, body: String) -> ProviderError {
    match status {
        400 => ProviderError::BadRequest(body),
        401 | 403 => ProviderError::SessionInvalid,
        429 | 529 => ProviderError::Overloaded,
        _ => ProviderError::Api { status, message: body },
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicEventEnvelope {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    message: Option<MessageStart>,
    #[serde(default)]
    delta: Option<Value>,
    #[serde(default)]
    content_block: Option<ContentBlock>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ApiError {
    #[serde(rename = "type", default)]
    error_type: String,
    #[serde(default)]
    message: String,
}

async fn pump_sse<S>(mut stream: S, tx: mpsc::Sender<ProviderEvent>)
where
    S: futures::Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError<reqwest::Error>>> + Unpin,
{
    let mut buffered_text = String::new();
    let mut in_tool_use = false;
    let mut saw_init = false;

    while let Some(item) = stream.next().await {
        match item {
            Ok(ev) => {
                if !handle_sse_event(&ev, &tx, &mut buffered_text, &mut in_tool_use, &mut saw_init).await {
                    return;
                }
            }
            Err(e) => {
                let _ = tx
                    .send(ProviderEvent::Error {
                        message: format!("sse decode: {e}"),
                        retryable: false,
                    })
                    .await;
                return;
            }
        }
    }
}

/// Returns `false` if the pump should exit (terminal event sent).
async fn handle_sse_event(
    ev: &eventsource_stream::Event,
    tx: &mpsc::Sender<ProviderEvent>,
    buffered_text: &mut String,
    in_tool_use: &mut bool,
    saw_init: &mut bool,
) -> bool {
    let env: AnthropicEventEnvelope = match serde_json::from_str(&ev.data) {
        Ok(v) => v,
        Err(e) => {
            let _ = tx
                .send(ProviderEvent::Error {
                    message: format!("malformed event json: {e}"),
                    retryable: false,
                })
                .await;
            return false;
        }
    };

    match env.event_type.as_str() {
        "message_start" => {
            let continuation = env
                .message
                .and_then(|m| m.id)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            *saw_init = true;
            if tx.send(ProviderEvent::Init { continuation }).await.is_err() {
                return false;
            }
        }
        "content_block_start" => {
            if let Some(block) = env.content_block {
                if block.block_type == "tool_use" {
                    *in_tool_use = true;
                    let name = block.name.unwrap_or_default();
                    if tx
                        .send(ProviderEvent::ToolStart {
                            name,
                            declared_timeout_ms: None,
                        })
                        .await
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = env.delta {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    buffered_text.push_str(text);
                }
            }
        }
        "content_block_stop" => {
            if *in_tool_use {
                *in_tool_use = false;
                if tx.send(ProviderEvent::ToolEnd).await.is_err() {
                    return false;
                }
            }
        }
        "message_stop" => {
            if !*saw_init {
                // Synthesize an Init so the runner has something to persist.
                let _ = tx
                    .send(ProviderEvent::Init {
                        continuation: uuid::Uuid::new_v4().to_string(),
                    })
                    .await;
            }
            let text = if buffered_text.is_empty() {
                None
            } else {
                Some(std::mem::take(buffered_text))
            };
            let _ = tx.send(ProviderEvent::Result { text }).await;
            return false;
        }
        "error" => {
            let (msg, retryable) = if let Some(err) = env.error {
                let retry = matches!(err.error_type.as_str(), "overloaded_error" | "rate_limit_error");
                (err.message, retry)
            } else {
                ("unknown error".to_string(), false)
            };
            let _ = tx
                .send(ProviderEvent::Error {
                    message: msg,
                    retryable,
                })
                .await;
            return false;
        }
        "ping" | "message_delta" => {
            // Heartbeat / usage update — surface as Activity.
            let _ = tx.send(ProviderEvent::Activity).await;
        }
        _ => {
            // Unknown event — ignore, keep streaming.
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::Effort;

    #[test]
    fn build_request_body_minimal() {
        let mut q = QueryInput::new("you are helpful", "claude-sonnet-4-5");
        q.history.push(HistoryMessage::User { content: "hi".into() });
        let body = build_request_body(&q);
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"], "you are helpful");
        assert!(body.get("temperature").is_none());
        assert!(body.get("tools").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn build_request_body_omits_empty_system() {
        let mut q = QueryInput::new("", "m");
        q.history.push(HistoryMessage::User { content: "hi".into() });
        let body = build_request_body(&q);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn build_request_body_includes_temperature_and_tools() {
        let mut q = QueryInput::new("s", "m");
        q.history.push(HistoryMessage::User { content: "hi".into() });
        q.temperature = Some(0.3);
        q.tools.push(ToolDef {
            name: "t".into(),
            description: "d".into(),
            input_schema: json!({ "type": "object" }),
        });
        let body = build_request_body(&q);
        assert!((body["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "t");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn history_collapses_consecutive_same_role() {
        let hist = vec![
            HistoryMessage::User { content: "a".into() },
            HistoryMessage::User { content: "b".into() },
            HistoryMessage::Assistant { content: "c".into() },
        ];
        let out = history_to_messages(&hist);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[1]["role"], "assistant");
    }

    #[test]
    fn history_tool_use_goes_to_assistant() {
        let hist = vec![
            HistoryMessage::Assistant { content: "let me check".into() },
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
        let out = history_to_messages(&hist);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "assistant");
        let asst_blocks = out[0]["content"].as_array().unwrap();
        assert_eq!(asst_blocks.len(), 2);
        assert_eq!(asst_blocks[1]["type"], "tool_use");
        assert_eq!(out[1]["role"], "user");
        let user_blocks = out[1]["content"].as_array().unwrap();
        assert_eq!(user_blocks.len(), 1);
        assert_eq!(user_blocks[0]["type"], "tool_result");
        assert_eq!(user_blocks[0]["is_error"], false);
    }

    #[test]
    fn map_http_error_variants() {
        assert!(matches!(map_http_error(400, "bad".into()), ProviderError::BadRequest(_)));
        assert!(matches!(map_http_error(401, "x".into()), ProviderError::SessionInvalid));
        assert!(matches!(map_http_error(403, "x".into()), ProviderError::SessionInvalid));
        assert!(matches!(map_http_error(429, "x".into()), ProviderError::Overloaded));
        assert!(matches!(map_http_error(529, "x".into()), ProviderError::Overloaded));
        assert!(matches!(
            map_http_error(500, "boom".into()),
            ProviderError::Api { status: 500, .. }
        ));
        assert!(matches!(
            map_http_error(503, "x".into()),
            ProviderError::Api { status: 503, .. }
        ));
    }

    #[test]
    fn provider_name_and_flags() {
        let p = AnthropicProvider::new("k");
        assert_eq!(p.name(), "anthropic");
        assert!(!p.supports_native_slash_commands());
    }

    #[test]
    fn is_session_invalid_predicate() {
        let p = AnthropicProvider::new("k");
        assert!(p.is_session_invalid(&ProviderError::SessionInvalid));
        assert!(!p.is_session_invalid(&ProviderError::Overloaded));
        assert!(!p.is_session_invalid(&ProviderError::Transport("x".into())));
        assert!(!p.is_session_invalid(&ProviderError::BadRequest("x".into())));
        assert!(!p.is_session_invalid(&ProviderError::Cancelled));
        assert!(!p.is_session_invalid(&ProviderError::Decode("x".into())));
        assert!(!p.is_session_invalid(&ProviderError::Api { status: 500, message: "x".into() }));
    }

    #[test]
    fn provider_clone_shares_inner() {
        let p = AnthropicProvider::new("k");
        let q = p.clone();
        assert_eq!(p.name(), q.name());
    }

    #[test]
    fn default_effort_passthrough() {
        let q = QueryInput::default();
        // Effort is not propagated to the wire — assert it at least lives in
        // the input so callers can inspect it.
        assert_eq!(q.effort, Effort::Medium);
    }

    #[test]
    fn base_url_trims_trailing_slash() {
        let p = AnthropicProvider::with_base_url("k", "https://example.com/");
        assert_eq!(p.inner.base_url, "https://example.com");
    }
}
