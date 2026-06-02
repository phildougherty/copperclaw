//! Anthropic Messages provider.
//!
//! Talks to `POST /v1/messages` with `stream=true` and parses the
//! server-sent event stream emitted by the Anthropic API. The provider is
//! stateless on the wire — every turn replays the full
//! [`crate::QueryInput::history`] — so the `continuation` field on
//! [`copperclaw_types::ProviderEvent::Init`] is just the upstream `message.id`
//! and exists purely so the runner has a stable handle to correlate logs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use copperclaw_types::ProviderEvent;
use eventsource_stream::Eventsource;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    ///
    /// Users sometimes pass base URLs that already end in `/v1` —
    /// e.g. `OpenRouter`'s Anthropic-compatible endpoint at
    /// `https://openrouter.ai/api/v1`. To make those Just Work without
    /// a footgun double-`/v1` path, the constructor strips a trailing
    /// `/v1` segment off the supplied base before the suffix is
    /// appended at call time.
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .expect("reqwest client builds");
        let raw = base_url.into();
        let trimmed = raw.trim_end_matches('/');
        let normalized = trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string();
        Self {
            inner: Arc::new(Inner {
                http,
                base_url: normalized,
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
            return Err(map_http_error(
                status.as_u16(),
                resp.text().await.unwrap_or_default(),
            ));
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
    // Reasoning-effort hint. Only emitted for low / high — Medium is
    // the implicit default for every provider that supports a tier,
    // so omitting it keeps the request body bit-identical to the
    // pre-effort shape for the common case. OpenRouter's unified API
    // forwards `reasoning.effort` to any underlying reasoning-capable
    // model (DeepSeek R1, OpenAI o-series, Anthropic extended-thinking
    // shim, etc.); providers that don't understand it ignore it.
    match input.effort {
        copperclaw_types::Effort::Low => {
            obj.insert("reasoning".into(), json!({ "effort": "low" }));
        }
        copperclaw_types::Effort::High => {
            obj.insert("reasoning".into(), json!({ "effort": "high" }));
        }
        copperclaw_types::Effort::Medium => {}
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
                    // A tool call whose argument JSON failed to parse (e.g. the
                    // model truncated it at the token limit) is recorded with a
                    // null input. Anthropic tolerates that, but strict
                    // OpenAI-compatible gateways reject a tool call with null
                    // arguments — coerce to an empty object.
                    "input": if input.is_null() { json!({}) } else { input.clone() },
                }),
            ),
            HistoryMessage::Tool {
                tool_use_id,
                content,
                is_error,
            } => (
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }),
            ),
            HistoryMessage::Image { media_type, data } => (
                "user",
                json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    },
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
                // Anthropic tolerates a single user message that mixes
                // `tool_result` and `text` blocks (it happens when a turn ends
                // on a tool call without a final reply and a new inbound user
                // message arrives). Strict OpenAI-compatible gateways
                // (OpenRouter -> MiniMax, etc.) reject it midstream with
                // "tool call result does not follow tool call". Keep
                // tool_result and text in separate user messages; the Anthropic
                // API recombines consecutive same-role turns server-side, so
                // this stays valid for the native backend too.
                if !would_mix_tool_result_and_text(arr, &block) {
                    arr.push(block);
                    return;
                }
            }
        }
    }
    out.push(json!({ "role": role, "content": [block] }));
}

/// True when appending `block` to `existing` would put a `tool_result` block
/// in the same message as a non-`tool_result` (text) block, or vice versa.
fn would_mix_tool_result_and_text(existing: &[Value], block: &Value) -> bool {
    fn is_tool_result(v: &Value) -> bool {
        v.get("type").and_then(Value::as_str) == Some("tool_result")
    }
    let block_is_tr = is_tool_result(block);
    let has_tr = existing.iter().any(is_tool_result);
    let has_other = existing.iter().any(|b| !is_tool_result(b));
    (block_is_tr && has_other) || (!block_is_tr && has_tr)
}

fn map_http_error(status: u16, body: String) -> ProviderError {
    match status {
        400 => ProviderError::BadRequest(body),
        401 | 403 => ProviderError::SessionInvalid,
        429 | 529 => ProviderError::Overloaded,
        _ => ProviderError::Api {
            status,
            message: body,
        },
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
    /// `message_delta` events carry incremental usage updates; the
    /// final values are what we record in `agent_turns`.
    #[serde(default)]
    usage: Option<UsageEvent>,
}

#[derive(Debug, Deserialize)]
struct MessageStart {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    usage: Option<UsageEvent>,
}

/// Slice of Anthropic's `usage` block. Multiple SSE events can
/// repeat fields; the latest non-None value wins.
#[derive(Debug, Deserialize, Default, Clone, Copy)]
struct UsageEvent {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    name: Option<String>,
    /// Present on `tool_use` blocks. Echoes back as
    /// `tool_result.tool_use_id` on the follow-up turn.
    #[serde(default)]
    id: Option<String>,
}

/// Tracks an in-flight `tool_use` content block. Anthropic streams
/// the JSON input as `input_json_delta` chunks; we accumulate them
/// here and parse at `content_block_stop`.
#[derive(Debug, Default)]
struct ToolUseAccumulator {
    id: String,
    name: String,
    input_json: String,
}

/// Tracks an in-flight `thinking` (or `redacted_thinking`) content block.
/// Reasoning models (Anthropic extended thinking, `Kimi K2.6`, `Qwen QwQ`,
/// `DeepSeek R1`, etc.) stream chain-of-thought as `thinking_delta` events
/// interleaved with `signature_delta` events that authenticate the block
/// for re-submission. We accumulate the text and the signature, and on
/// `content_block_stop` emit a [`ProviderEvent::Thinking`] event carrying
/// the completed prose; the runner decides whether to surface it to the
/// user (gated on per-group `surface_thinking`).
///
/// The signature is captured for future use (history re-submission
/// requires it for some providers); presently dropped at stop.
#[derive(Debug, Default)]
struct ThinkingAccumulator {
    text: String,
    #[allow(dead_code)] // captured for future history-passthrough; see TODO above.
    signature: String,
    /// `true` when the upstream block was a `redacted_thinking` rather
    /// than a regular `thinking` block. Renderers MUST substitute a
    /// placeholder for redacted blocks instead of displaying any text.
    redacted: bool,
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
    S: futures::Stream<
            Item = Result<
                eventsource_stream::Event,
                eventsource_stream::EventStreamError<reqwest::Error>,
            >,
        > + Unpin,
{
    let mut state = SseState::default();

    while let Some(item) = stream.next().await {
        match item {
            Ok(ev) => {
                if !handle_sse_event(&ev, &tx, &mut state).await {
                    return;
                }
            }
            Err(e) => {
                // SSE transport/decode failures are almost always
                // transient: the upstream connection got reset mid-stream
                // or a chunk arrived malformed. Mark retryable so the
                // runner's `drive_turn` loop re-opens the query rather
                // than terminally failing the inbound. Verified live
                // against OpenRouter: a single dropped SSE chunk would
                // otherwise lose the user's message permanently.
                let _ = tx
                    .send(ProviderEvent::Error {
                        message: format!("sse decode: {e}"),
                        retryable: true,
                    })
                    .await;
                return;
            }
        }
    }
}

/// SSE-pump scratch state. Lives across `handle_sse_event` calls for
/// one streaming response.
#[derive(Debug, Default)]
struct SseState {
    buffered_text: String,
    /// Set while we're inside a `tool_use` content block. `None`
    /// otherwise. Anthropic streams a tool's JSON input across
    /// multiple `input_json_delta` events under a single
    /// `content_block_start` → `content_block_stop` envelope.
    tool_use: Option<ToolUseAccumulator>,
    /// Set while we're inside a `thinking` or `redacted_thinking`
    /// content block. `None` otherwise. Reasoning models emit one or
    /// more thinking blocks before any visible text; we absorb them
    /// silently and keep the user-facing reply unaffected.
    thinking: Option<ThinkingAccumulator>,
    saw_init: bool,
}

/// Returns `false` if the pump should exit (terminal event sent).
///
/// Long-but-flat: the body is a single match on the Anthropic SSE
/// event-type string, so splitting it just to satisfy a line-count
/// cap would hurt readability more than it helps.
#[allow(clippy::too_many_lines)]
async fn handle_sse_event(
    ev: &eventsource_stream::Event,
    tx: &mpsc::Sender<ProviderEvent>,
    state: &mut SseState,
) -> bool {
    let buffered_text = &mut state.buffered_text;
    let saw_init = &mut state.saw_init;
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
            let message = env.message;
            let usage = message.as_ref().and_then(|m| m.usage);
            let continuation = message
                .and_then(|m| m.id)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            *saw_init = true;
            if tx.send(ProviderEvent::Init { continuation }).await.is_err() {
                return false;
            }
            if let Some(u) = usage {
                let _ = tx
                    .send(ProviderEvent::Usage {
                        input_tokens: u.input_tokens.unwrap_or(0),
                        output_tokens: u.output_tokens.unwrap_or(0),
                    })
                    .await;
            }
        }
        "content_block_start" => {
            if let Some(block) = env.content_block {
                match block.block_type.as_str() {
                    "tool_use" => {
                        let name = block.name.unwrap_or_default();
                        let id = block.id.unwrap_or_default();
                        state.tool_use = Some(ToolUseAccumulator {
                            id,
                            name: name.clone(),
                            input_json: String::new(),
                        });
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
                    "thinking" | "redacted_thinking" => {
                        // Reasoning model is entering its chain-of-thought.
                        // Surface Activity so the runner's drive loop sees
                        // liveness — thinking phases can run 30-90s before
                        // any user-visible text appears.
                        //
                        // Track which variant we're in so the
                        // `content_block_stop` handler can emit a
                        // `ProviderEvent::Thinking { redacted: … }` event
                        // with the right flag.
                        let redacted = block.block_type == "redacted_thinking";
                        state.thinking = Some(ThinkingAccumulator {
                            text: String::new(),
                            signature: String::new(),
                            redacted,
                        });
                        let _ = tx.send(ProviderEvent::Activity).await;
                    }
                    _ => {
                        // Unknown content-block type — keep streaming;
                        // future-proof against new variants.
                    }
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = env.delta {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    buffered_text.push_str(text);
                }
                // `input_json_delta` carries one chunk of a tool_use
                // block's JSON input. Anthropic guarantees the
                // concatenation parses as JSON at content_block_stop.
                if let Some(chunk) = delta.get("partial_json").and_then(Value::as_str) {
                    if let Some(acc) = state.tool_use.as_mut() {
                        acc.input_json.push_str(chunk);
                    }
                }
                // `thinking_delta` carries one chunk of the model's
                // reasoning. Accumulate but don't expose to the agent's
                // reply. Emit Activity so the runner sees progress.
                if let Some(thought) = delta.get("thinking").and_then(Value::as_str) {
                    if let Some(acc) = state.thinking.as_mut() {
                        acc.text.push_str(thought);
                    }
                    let _ = tx.send(ProviderEvent::Activity).await;
                }
                // `signature_delta` authenticates a thinking block for
                // re-submission. Captured for future history-passthrough;
                // dropped on `content_block_stop` for now since we don't
                // yet re-submit thinking content across turns.
                if let Some(sig) = delta.get("signature").and_then(Value::as_str) {
                    if let Some(acc) = state.thinking.as_mut() {
                        acc.signature.push_str(sig);
                    }
                }
            }
        }
        "content_block_stop" => {
            // Close any in-flight thinking block first — thinking and
            // tool_use are mutually exclusive within a single index, but
            // closing it here keeps the state machine simple.
            if let Some(acc) = state.thinking.take() {
                let _ = tx.send(ProviderEvent::Activity).await;
                // Emit the structured Thinking event so the runner can
                // optionally surface it to the user (gated on per-group
                // `surface_thinking`). The signature is dropped here —
                // we don't yet re-submit thinking content across turns.
                if tx
                    .send(ProviderEvent::Thinking {
                        text: acc.text,
                        redacted: acc.redacted,
                    })
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            if let Some(acc) = state.tool_use.take() {
                // Empty input is the typical zero-arg call.
                let input = if acc.input_json.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    match serde_json::from_str::<Value>(&acc.input_json) {
                        Ok(v) => v,
                        Err(e) => {
                            // Surface this as a recoverable event rather
                            // than a terminal Error: the runner will hand
                            // the parse error back to the model as a
                            // tool_result with is_error=true so it can
                            // self-correct on the next turn. See
                            // `copperclaw-runner::run::pump_events` for the
                            // matching recovery path.
                            let _ = tx
                                .send(ProviderEvent::ToolInputParseError {
                                    tool_use_id: acc.id,
                                    tool_name: acc.name,
                                    raw_input: acc.input_json,
                                    parse_error: e.to_string(),
                                })
                                .await;
                            let _ = tx.send(ProviderEvent::ToolEnd).await;
                            return false;
                        }
                    }
                };
                if tx
                    .send(ProviderEvent::ToolCall {
                        id: acc.id,
                        name: acc.name,
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
                let retry = matches!(
                    err.error_type.as_str(),
                    "overloaded_error" | "rate_limit_error"
                );
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
        "ping" => {
            let _ = tx.send(ProviderEvent::Activity).await;
        }
        "message_delta" => {
            // Heartbeat + usage update. Always surface Activity for
            // liveness; emit Usage when the event carries one.
            let _ = tx.send(ProviderEvent::Activity).await;
            if let Some(u) = env.usage {
                let _ = tx
                    .send(ProviderEvent::Usage {
                        input_tokens: u.input_tokens.unwrap_or(0),
                        output_tokens: u.output_tokens.unwrap_or(0),
                    })
                    .await;
            }
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
    use copperclaw_types::Effort;

    #[test]
    fn build_request_body_minimal() {
        let mut q = QueryInput::new("you are helpful", "claude-sonnet-4-6");
        q.history.push(HistoryMessage::User {
            content: "hi".into(),
        });
        let body = build_request_body(&q);
        assert_eq!(body["model"], "claude-sonnet-4-6");
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
        q.history.push(HistoryMessage::User {
            content: "hi".into(),
        });
        let body = build_request_body(&q);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn build_request_body_includes_temperature_and_tools() {
        let mut q = QueryInput::new("s", "m");
        q.history.push(HistoryMessage::User {
            content: "hi".into(),
        });
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
            HistoryMessage::User {
                content: "a".into(),
            },
            HistoryMessage::User {
                content: "b".into(),
            },
            HistoryMessage::Assistant {
                content: "c".into(),
            },
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
            HistoryMessage::Assistant {
                content: "let me check".into(),
            },
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
    fn tool_result_and_user_text_split_into_separate_messages() {
        // A turn that ended on a tool call without a final reply, then a fresh
        // inbound user message. The tool_result and the user text must NOT share
        // one user message — strict OpenAI-compatible gateways (MiniMax) reject
        // that with "tool call result does not follow tool call".
        let hist = vec![
            HistoryMessage::ToolUse {
                id: "tu_1".into(),
                name: "write_file".into(),
                input: json!({ "path": "/x" }),
            },
            HistoryMessage::Tool {
                tool_use_id: "tu_1".into(),
                content: "boom".into(),
                is_error: true,
            },
            HistoryMessage::User {
                content: "what happened?".into(),
            },
        ];
        let out = history_to_messages(&hist);
        // assistant[tool_use], user[tool_result], user[text]
        assert_eq!(out.len(), 3);
        assert_eq!(out[1]["role"], "user");
        let res = out[1]["content"].as_array().unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0]["type"], "tool_result");
        assert_eq!(out[2]["role"], "user");
        let txt = out[2]["content"].as_array().unwrap();
        assert_eq!(txt.len(), 1);
        assert_eq!(txt[0]["type"], "text");
    }

    #[test]
    fn parallel_tool_results_still_coalesce() {
        // Multiple tool_results from one parallel batch DO still share a user
        // message (valid everywhere); only the tool_result/text mix splits.
        let hist = vec![
            HistoryMessage::ToolUse {
                id: "a".into(),
                name: "t".into(),
                input: json!({}),
            },
            HistoryMessage::ToolUse {
                id: "b".into(),
                name: "t".into(),
                input: json!({}),
            },
            HistoryMessage::Tool {
                tool_use_id: "a".into(),
                content: "1".into(),
                is_error: false,
            },
            HistoryMessage::Tool {
                tool_use_id: "b".into(),
                content: "2".into(),
                is_error: false,
            },
        ];
        let out = history_to_messages(&hist);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2); // two tool_use
        assert_eq!(out[1]["content"].as_array().unwrap().len(), 2); // two tool_result
    }

    #[test]
    fn image_serializes_as_base64_image_block_on_user_role() {
        // The exact shape verified live against minimax-m3 via OpenRouter.
        let hist = vec![
            HistoryMessage::User {
                content: "what is this?".into(),
            },
            HistoryMessage::Image {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            },
        ];
        let out = history_to_messages(&hist);
        // User text + image coalesce into one user message [text, image].
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
        let blocks = out[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "QUJD");
    }

    #[test]
    fn image_after_tool_result_splits_into_its_own_message() {
        // A tool (view_image) returns an image: the tool_result and the
        // image must NOT share a user message (minimax rejects the mix).
        let hist = vec![
            HistoryMessage::ToolUse {
                id: "tu_1".into(),
                name: "view_image".into(),
                input: json!({ "path": "/data/x.png" }),
            },
            HistoryMessage::Tool {
                tool_use_id: "tu_1".into(),
                content: "loaded".into(),
                is_error: false,
            },
            HistoryMessage::Image {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            },
        ];
        let out = history_to_messages(&hist);
        // assistant[tool_use], user[tool_result], user[image]
        assert_eq!(out.len(), 3);
        assert_eq!(out[1]["content"][0]["type"], "tool_result");
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["type"], "image");
    }

    #[test]
    fn null_tool_use_input_serializes_as_empty_object() {
        // Truncated/unparseable argument JSON is recorded as a null input;
        // it must serialize as `{}`, never `null`, for OpenAI-compatible gateways.
        let hist = vec![HistoryMessage::ToolUse {
            id: "tu_1".into(),
            name: "write_file".into(),
            input: Value::Null,
        }];
        let out = history_to_messages(&hist);
        assert_eq!(out[0]["content"][0]["input"], json!({}));
    }

    /// Build an `eventsource_stream::Event` from a JSON payload for
    /// the SSE-pump unit tests. The `event` name field is not used by
    /// the pump (we read `data.type`) so it stays default.
    fn sse(data: &str) -> eventsource_stream::Event {
        eventsource_stream::Event {
            data: data.to_string(),
            ..Default::default()
        }
    }

    fn drain<T>(rx: &mut mpsc::Receiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn thinking_blocks_emit_activity_and_do_not_pollute_text() {
        // Reproduces a Kimi K2.6 / Claude-extended-thinking stream: the
        // model opens a `thinking` content block, streams thinking_delta
        // and signature_delta events, closes it, then opens a normal
        // `text` content block with the user-facing reply. The bug this
        // pins: thinking content must NOT leak into buffered_text, and
        // the runner must see Activity events while the thinking phase
        // is in flight (so it doesn't perceive a silent hang).
        let (tx, mut rx) = mpsc::channel(64);
        let mut state = SseState::default();

        for ev in [
            sse(
                r#"{"type":"message_start","message":{"id":"gen_1","usage":{"input_tokens":10,"output_tokens":0}}}"#,
            ),
            sse(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The user said hi."}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" I should reply briefly."}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-xyz"}}"#,
            ),
            sse(r#"{"type":"content_block_stop","index":0}"#),
            sse(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi!"}}"#,
            ),
            sse(r#"{"type":"content_block_stop","index":1}"#),
            sse(r#"{"type":"message_stop"}"#),
        ] {
            handle_sse_event(&ev, &tx, &mut state).await;
        }

        let events = drain(&mut rx);
        let names: Vec<&'static str> = events
            .iter()
            .map(|e| match e {
                ProviderEvent::Init { .. } => "init",
                ProviderEvent::Activity => "activity",
                ProviderEvent::Result { .. } => "result",
                ProviderEvent::Usage { .. } => "usage",
                ProviderEvent::ToolStart { .. } => "tool_start",
                ProviderEvent::ToolEnd => "tool_end",
                ProviderEvent::ToolCall { .. } => "tool_call",
                ProviderEvent::Thinking { .. } => "thinking",
                _ => "other",
            })
            .collect();
        // Must contain init, at least one activity (thinking liveness),
        // and a terminal result.
        assert!(names.contains(&"init"), "got: {names:?}");
        assert!(
            names.iter().filter(|n| **n == "activity").count() >= 2,
            "expected ≥2 activity events during thinking, got: {names:?}"
        );
        let result_text: Option<String> = events.iter().find_map(|e| match e {
            ProviderEvent::Result { text } => Some(text.clone().unwrap_or_default()),
            _ => None,
        });
        assert_eq!(
            result_text.as_deref(),
            Some("Hi!"),
            "user-facing text must equal the text block; thinking deltas must not leak in"
        );
    }

    #[tokio::test]
    async fn redacted_thinking_block_does_not_hang_or_leak() {
        // `redacted_thinking` is the model's encoded reasoning that the
        // user cannot see but that providers require be acknowledged.
        // Treat it like a `thinking` block: enter, emit Activity, exit,
        // never let it pollute the text buffer.
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = SseState::default();
        for ev in [
            sse(r#"{"type":"message_start","message":{"id":"gen_2"}}"#),
            sse(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque-blob"}}"#,
            ),
            sse(r#"{"type":"content_block_stop","index":0}"#),
            sse(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            ),
            sse(r#"{"type":"content_block_stop","index":1}"#),
            sse(r#"{"type":"message_stop"}"#),
        ] {
            handle_sse_event(&ev, &tx, &mut state).await;
        }
        let events = drain(&mut rx);
        let final_text = events.iter().find_map(|e| match e {
            ProviderEvent::Result { text } => text.clone(),
            _ => None,
        });
        assert_eq!(final_text.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn thinking_block_stop_emits_provider_event_thinking_with_full_text() {
        // Slice-3.5 contract: when the upstream closes a `thinking` block
        // we surface a structured `ProviderEvent::Thinking` carrying the
        // accumulated text. The runner consumes this (gated on per-group
        // `surface_thinking`) and persists a `MessageKind::Thinking` row
        // so the user can optionally see the reasoning as a collapsed
        // native primitive.
        let (tx, mut rx) = mpsc::channel(64);
        let mut state = SseState::default();
        for ev in [
            sse(r#"{"type":"message_start","message":{"id":"gen_t1"}}"#),
            sse(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"The user is asking"}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":" a simple greeting."}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-abc"}}"#,
            ),
            sse(r#"{"type":"content_block_stop","index":0}"#),
            sse(
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            ),
            sse(
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi!"}}"#,
            ),
            sse(r#"{"type":"content_block_stop","index":1}"#),
            sse(r#"{"type":"message_stop"}"#),
        ] {
            handle_sse_event(&ev, &tx, &mut state).await;
        }
        let events = drain(&mut rx);
        let thinking = events.iter().find_map(|e| match e {
            ProviderEvent::Thinking { text, redacted } => Some((text.clone(), *redacted)),
            _ => None,
        });
        let (text, redacted) = thinking.expect("expected ProviderEvent::Thinking to be emitted");
        assert_eq!(text, "The user is asking a simple greeting.");
        assert!(
            !redacted,
            "regular `thinking` block must have redacted=false"
        );
    }

    #[tokio::test]
    async fn redacted_thinking_block_emits_provider_event_thinking_with_redacted_flag() {
        // `redacted_thinking` blocks must still surface a Thinking event
        // (so the runner can record an audit row / render a placeholder)
        // — but with `redacted=true` so downstream code knows to
        // substitute a placeholder rather than display any text.
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = SseState::default();
        for ev in [
            sse(r#"{"type":"message_start","message":{"id":"gen_r1"}}"#),
            sse(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque-secret-blob"}}"#,
            ),
            sse(r#"{"type":"content_block_stop","index":0}"#),
            sse(r#"{"type":"message_stop"}"#),
        ] {
            handle_sse_event(&ev, &tx, &mut state).await;
        }
        let events = drain(&mut rx);
        let thinking = events.iter().find_map(|e| match e {
            ProviderEvent::Thinking { text, redacted } => Some((text.clone(), *redacted)),
            _ => None,
        });
        let (text, redacted) =
            thinking.expect("expected ProviderEvent::Thinking even for redacted blocks");
        assert!(redacted, "redacted_thinking must have redacted=true");
        // The accumulator doesn't capture the `data` field — we never
        // surface the opaque blob on the wire, even via the Thinking
        // event. Text stays empty.
        assert!(
            text.is_empty(),
            "redacted blocks must not capture the opaque data blob; got {text:?}"
        );
    }

    #[test]
    fn map_http_error_variants() {
        assert!(matches!(
            map_http_error(400, "bad".into()),
            ProviderError::BadRequest(_)
        ));
        assert!(matches!(
            map_http_error(401, "x".into()),
            ProviderError::SessionInvalid
        ));
        assert!(matches!(
            map_http_error(403, "x".into()),
            ProviderError::SessionInvalid
        ));
        assert!(matches!(
            map_http_error(429, "x".into()),
            ProviderError::Overloaded
        ));
        assert!(matches!(
            map_http_error(529, "x".into()),
            ProviderError::Overloaded
        ));
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
        assert!(!p.is_session_invalid(&ProviderError::Api {
            status: 500,
            message: "x".into()
        }));
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

    #[test]
    fn base_url_strips_trailing_v1_so_users_can_paste_openrouter_url_verbatim() {
        // `https://openrouter.ai/api/v1` is the canonical form
        // OpenRouter docs hand operators. The provider appends
        // `/v1/messages` later, so the trailing `/v1` would yield a
        // double-`/v1` 404 if we naively concatenated.
        let p = AnthropicProvider::with_base_url("k", "https://openrouter.ai/api/v1");
        assert_eq!(p.inner.base_url, "https://openrouter.ai/api");
    }

    #[test]
    fn base_url_strips_trailing_v1_even_with_trailing_slash() {
        let p = AnthropicProvider::with_base_url("k", "https://openrouter.ai/api/v1/");
        assert_eq!(p.inner.base_url, "https://openrouter.ai/api");
    }

    #[test]
    fn base_url_without_v1_suffix_unchanged() {
        let p = AnthropicProvider::with_base_url("k", "https://api.anthropic.com");
        assert_eq!(p.inner.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn base_url_does_not_strip_v1_in_the_middle_of_the_path() {
        // We only strip a trailing `/v1`, not an embedded one — a
        // versioned proxy path like `/v1/projects/123/anthropic`
        // must round-trip untouched.
        let p = AnthropicProvider::with_base_url("k", "https://proxy.example/v1/anthropic");
        assert_eq!(p.inner.base_url, "https://proxy.example/v1/anthropic");
    }
}
