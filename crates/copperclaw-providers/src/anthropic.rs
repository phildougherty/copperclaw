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

/// Beta header value enabling the prompt-caching API on the direct
/// Anthropic endpoint. `OpenRouter` (and any Anthropic-compatible gateway)
/// forwards `cache_control` natively and does not require — and may not
/// recognize — this header, so it is only sent when we are talking to the
/// canonical Anthropic host.
pub const PROMPT_CACHING_BETA: &str = "prompt-caching-2024-07-31";

/// Returns `true` for model identifiers in the Anthropic / Claude family,
/// across both the direct API (`claude-sonnet-4-6`) and `OpenRouter`'s
/// vendor-prefixed slugs (`anthropic/claude-3.7-sonnet`). Used to gate the
/// `cache_control` breakpoints: a non-Anthropic `OpenRouter` model
/// (`deepseek/deepseek-r1`, `minimax/minimax-m3`, `google/gemini-…`) would
/// reject the unknown `cache_control` field, so for those we emit the
/// pre-caching request shape verbatim.
///
/// Matching is deliberately broad-but-anchored: any slug whose final
/// path segment starts with `claude` counts (covers `anthropic/claude*`
/// and bare `claude*`), and so does any explicit `anthropic/` vendor
/// prefix. Comparison is case-insensitive.
#[must_use]
pub fn is_anthropic_family_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return false;
    }
    // Vendor-prefixed OpenRouter slug, e.g. `anthropic/claude-3.7-sonnet`.
    if m.starts_with("anthropic/") {
        return true;
    }
    // The bare model name or the final path segment of a slug.
    let leaf = m.rsplit('/').next().unwrap_or(&m);
    leaf.starts_with("claude")
}

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
    /// `true` when `base_url` points at the canonical Anthropic API host
    /// (`api.anthropic.com`). Gates the prompt-caching beta header: the
    /// direct API requires it, gateways (`OpenRouter`) forward
    /// `cache_control` without it.
    is_anthropic_host: bool,
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
        let is_anthropic_host = normalized.contains("api.anthropic.com");
        Self {
            inner: Arc::new(Inner {
                http,
                base_url: normalized,
                api_key: api_key.into(),
                is_anthropic_host,
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
        let caching = is_anthropic_family_model(&input.model);
        let body = build_request_body(&input, caching);
        let url = format!("{}/v1/messages", self.inner.base_url);
        let mut req = self
            .inner
            .http
            .post(&url)
            .header("x-api-key", &self.inner.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        // Prompt-caching beta header: only on the direct Anthropic host
        // (gateways forward `cache_control` natively and may not recognize
        // the header) and only when the model is in the Claude family
        // (a non-Anthropic model never carries `cache_control` blocks, so
        // the header would be a no-op at best and a rejection at worst).
        if caching && self.inner.is_anthropic_host {
            req = req.header("anthropic-beta", PROMPT_CACHING_BETA);
        }
        let resp = req
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

/// A `cache_control` breakpoint marking the preceding prefix as cacheable.
/// Anthropic's only supported value today is `{"type":"ephemeral"}` (a
/// ~5-minute TTL that refreshes on every hit).
fn ephemeral_cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

/// Stamp a `cache_control` breakpoint on the LAST content block of the
/// LAST message in `messages`. This caches the entire transcript prefix up
/// to and including that block, so on the next turn (which appends new
/// messages after it) the whole prior transcript is a cache hit.
///
/// Why the last block of the last message and not the last *message*: the
/// breakpoint attaches to a content block, and a message's `content` is an
/// array of blocks. We pick the final block so the cached span is maximal.
fn mark_messages_tail(messages: &mut [Value]) {
    if let Some(last_msg) = messages.last_mut() {
        if let Some(blocks) = last_msg.get_mut("content").and_then(Value::as_array_mut) {
            if let Some(last_block) = blocks.last_mut() {
                if let Some(obj) = last_block.as_object_mut() {
                    obj.insert("cache_control".into(), ephemeral_cache_control());
                }
            }
        }
    }
}

/// Build the exact request-body JSON [`AnthropicProvider::query`] would
/// POST for `input`, with the prompt-caching gate derived from the model
/// the same way `query` derives it ([`is_anthropic_family_model`]). Public
/// so callers (and cross-crate tests) can inspect the wire shape — in
/// particular to verify the cached prefix is byte-stable across turns —
/// without standing up an HTTP mock. Does NOT send anything.
#[must_use]
pub fn build_request_body_for_model(input: &QueryInput) -> Value {
    build_request_body(input, is_anthropic_family_model(&input.model))
}

/// Build the request body. `caching` is the gate computed by the caller
/// from [`is_anthropic_family_model`]: when `false` the body is byte-for-byte
/// the pre-caching shape (no `cache_control` anywhere, system stays a plain
/// string) so non-Anthropic gateways never see an unknown field.
fn build_request_body(input: &QueryInput, caching: bool) -> Value {
    let mut messages = history_to_messages(&input.history);
    // Breakpoint 1 (when caching): the transcript tail. Placed on the last
    // block of the last message so the entire growing prefix caches across
    // turns. The new content each turn lands AFTER this breakpoint, keeping
    // the cached span byte-stable turn over turn.
    //
    // ORDER MATTERS: stamp the breakpoint on the STABLE transcript tail
    // FIRST, then append the volatile conversation-context AFTER it. The
    // context block thus lands one position past the breakpoint and never
    // enters any cached prefix — that is the whole point of holding it out
    // of the system block. (Non-caching path: the context is folded into
    // the plain-string `system` by `build_system`/`combined_system`, so we
    // skip the message-append entirely to keep those bytes unchanged.)
    if caching {
        mark_messages_tail(&mut messages);
        if let Some(ctx) = input.system_context.as_deref().filter(|c| !c.is_empty()) {
            append_context_to_last_user_message(&mut messages, ctx);
        }
    }
    let mut body = json!({
        "model": input.model,
        "max_tokens": input.max_tokens,
        "stream": true,
        "messages": messages,
    });
    let obj = body.as_object_mut().expect("json object");
    if let Some(system) = build_system(input, caching) {
        obj.insert("system".into(), system);
    }
    if let Some(temp) = input.temperature {
        obj.insert("temperature".into(), json!(temp));
    }
    if !input.tools.is_empty() {
        // Breakpoint 3 (when caching): the tools array tail. The tool
        // catalogue is stable across turns; the breakpoint on the final
        // tool caches the whole tools block. `tools_to_json` stamps it.
        obj.insert(
            "tools".into(),
            Value::Array(tools_to_json(&input.tools, caching)),
        );
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

/// Build the `system` field of the request body, returning `None` when
/// there is nothing to send.
///
/// Non-caching path: emit a single plain string — the static system and
/// the volatile conversation-context flattened by
/// [`QueryInput::combined_system`]. This is byte-identical to the
/// pre-split shape (the runner used to hand us the already-concatenated
/// string), so non-Anthropic gateways see exactly the same bytes.
///
/// Caching path: emit Anthropic's system-ARRAY form carrying ONLY the
/// STATIC system prompt as a single block with a trailing
/// `cache_control` breakpoint. The volatile conversation-context does
/// NOT go here — it is appended to the last user message AFTER the
/// transcript-tail breakpoint (see [`build_request_body`]). Keeping the
/// system block byte-stable is what makes BOTH the system breakpoint AND
/// the transcript-tail breakpoint (whose cached prefix spans
/// `tools + system + prior messages`) actually hit across turns: if any
/// volatile text lived in `system`, the messages-prefix that the
/// transcript breakpoint caches would shift every inbound and miss.
fn build_system(input: &QueryInput, caching: bool) -> Option<Value> {
    if !caching {
        let combined = input.combined_system();
        if combined.is_empty() {
            return None;
        }
        return Some(Value::String(combined));
    }

    if input.system.is_empty() {
        return None;
    }
    Some(json!([{
        "type": "text",
        "text": input.system,
        "cache_control": ephemeral_cache_control(),
    }]))
}

/// Append the volatile conversation-context as a trailing `text` block on
/// the LAST user message, placed strictly AFTER the transcript-tail
/// `cache_control` breakpoint so it never perturbs the cached prefix.
///
/// Ordering contract (caching path): [`build_request_body`] calls this
/// only AFTER [`mark_messages_tail`] has stamped the breakpoint on the
/// last block of the last message — so the freshly-pushed context block
/// becomes the new final block, sitting one position past the breakpoint.
/// The cached span (`tools + system + every block up to and including the
/// breakpoint`) is therefore byte-stable turn-over-turn even though this
/// context paragraph changes on (almost) every inbound.
///
/// When the transcript ends on an assistant turn, or on a user message
/// that already holds `tool_result` blocks (mixing text with those would
/// trip the same strict-gateway rule `push_block` guards against), the
/// context goes into a FRESH trailing user message instead — still after
/// the breakpoint, still outside the cached span.
fn append_context_to_last_user_message(messages: &mut Vec<Value>, context: &str) {
    let block = json!({ "type": "text", "text": context });
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("user") {
            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                // Don't mix a text block into a tool_result-bearing user
                // message — keep them in separate user messages exactly as
                // `push_block` does for the transcript proper.
                if !would_mix_tool_result_and_text(arr, &block) {
                    arr.push(block);
                    return;
                }
            }
        }
    }
    // The transcript ends on an assistant turn (or a tool_result user
    // message): append a fresh user message carrying just the context so
    // it still lands after the breakpoint and outside the cached prefix.
    messages.push(json!({ "role": "user", "content": [block] }));
}

fn tools_to_json(tools: &[ToolDef], caching: bool) -> Vec<Value> {
    let mut out: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect();
    // Stamp the breakpoint on the LAST tool: it caches the whole tools
    // array (the breakpoint marks everything up to and including itself).
    if caching {
        if let Some(last) = out.last_mut() {
            if let Some(obj) = last.as_object_mut() {
                obj.insert("cache_control".into(), ephemeral_cache_control());
            }
        }
    }
    out
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
///
/// `cache_read_input_tokens` / `cache_creation_input_tokens` are the
/// prompt-caching usage counters: a non-zero read count means a cache
/// breakpoint *hit* this turn (the bulk of the cost win); the creation
/// count is the premium-billed write that primes the cache for later
/// hits. Absent on providers that don't support caching (left as `None`).
//
// The `_tokens` / `_input_tokens` field suffixes mirror Anthropic's wire
// JSON keys verbatim (no serde rename), so the shared-postfix lint is a
// false positive here — renaming would silently break deserialization.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Deserialize, Default, Clone, Copy)]
struct UsageEvent {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
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
                        cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0),
                        cache_creation_tokens: u.cache_creation_input_tokens.unwrap_or(0),
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
                        cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0),
                        cache_creation_tokens: u.cache_creation_input_tokens.unwrap_or(0),
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
        let body = build_request_body(&q, false);
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
        let body = build_request_body(&q, false);
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
        let body = build_request_body(&q, false);
        assert!((body["temperature"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "t");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    // ----- prompt caching -------------------------------------------------

    /// Build a representative multi-turn input: system + tools + a couple
    /// of transcript turns. `model` selects the caching gate. Carries a
    /// volatile `system_context` so the tests exercise the static/volatile
    /// split that keeps the cached prefix stable.
    fn caching_fixture(model: &str) -> QueryInput {
        let mut q = QueryInput::new("you are a large stable system prompt", model);
        q.system_context = Some(
            "Conversation context: this turn is in a direct conversation on cli; \
                  3 prior entries in session history."
                .into(),
        );
        q.tools.push(ToolDef {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: json!({ "type": "object" }),
        });
        q.tools.push(ToolDef {
            name: "write_file".into(),
            description: "write a file".into(),
            input_schema: json!({ "type": "object" }),
        });
        q.history.push(HistoryMessage::User {
            content: "first question".into(),
        });
        q.history.push(HistoryMessage::Assistant {
            content: "first answer".into(),
        });
        q.history.push(HistoryMessage::User {
            content: "second question".into(),
        });
        q
    }

    /// Count every `cache_control` breakpoint anywhere in the body — the
    /// Anthropic limit is 4.
    fn count_cache_control(body: &Value) -> usize {
        fn walk(v: &Value, n: &mut usize) {
            match v {
                Value::Object(map) => {
                    if map.contains_key("cache_control") {
                        *n += 1;
                    }
                    for child in map.values() {
                        walk(child, n);
                    }
                }
                Value::Array(arr) => {
                    for child in arr {
                        walk(child, n);
                    }
                }
                _ => {}
            }
        }
        let mut n = 0;
        walk(body, &mut n);
        n
    }

    #[test]
    fn anthropic_family_detection_matrix() {
        // Direct-API bare names.
        assert!(is_anthropic_family_model("claude-sonnet-4-6"));
        assert!(is_anthropic_family_model("claude-3-5-haiku-latest"));
        assert!(is_anthropic_family_model("CLAUDE-OPUS")); // case-insensitive
        // OpenRouter vendor-prefixed Claude slugs.
        assert!(is_anthropic_family_model("anthropic/claude-3.7-sonnet"));
        assert!(is_anthropic_family_model("anthropic/claude-opus-4"));
        // Non-Anthropic OpenRouter slugs must NOT match.
        assert!(!is_anthropic_family_model("deepseek/deepseek-r1"));
        assert!(!is_anthropic_family_model("minimax/minimax-m3"));
        assert!(!is_anthropic_family_model("google/gemini-2.5-pro"));
        assert!(!is_anthropic_family_model("openrouter/owl-alpha"));
        assert!(!is_anthropic_family_model("qwen3.6:27b"));
        assert!(!is_anthropic_family_model(""));
        // A slug whose leaf merely contains "claude" mid-string must not
        // false-positive (anchor is "starts_with").
        assert!(!is_anthropic_family_model("vendor/notclaude-1"));
    }

    #[test]
    fn caching_marks_system_tools_and_transcript_tail_for_anthropic_model() {
        let q = caching_fixture("claude-sonnet-4-6");
        let body = build_request_body(&q, true);

        // System prompt becomes a single STATIC block with a trailing
        // breakpoint. The volatile conversation-context is NOT here — it
        // moves into the last user message after the transcript breakpoint
        // (asserted below) so it can never perturb the cached prefix.
        let system = body["system"].as_array().expect("system is a block array");
        assert_eq!(system.len(), 1, "only the static system block lives here");
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "you are a large stable system prompt");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        // The volatile context must not have leaked into the system block.
        assert!(
            !system[0]["text"]
                .as_str()
                .unwrap()
                .contains("Conversation context"),
            "volatile context must not be in the cached system block"
        );

        // Tools: only the LAST tool carries the breakpoint.
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");

        // Transcript tail: the original last user message ("second
        // question") carries the breakpoint, and the volatile context was
        // appended as a SUBSEQUENT block on the same user message WITHOUT a
        // breakpoint — so the breakpoint marks the stable block and the
        // volatile text lands strictly after it.
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        let blocks = last["content"].as_array().unwrap();
        assert_eq!(
            blocks.len(),
            2,
            "stable question block + volatile context block"
        );
        assert_eq!(blocks[0]["text"], "second question");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["text"], q.system_context.as_deref().unwrap());
        assert!(
            blocks[1].get("cache_control").is_none(),
            "volatile context block must NOT carry a breakpoint"
        );
        // An earlier message must NOT carry a breakpoint.
        let first = &messages[0];
        let first_blocks = first["content"].as_array().unwrap();
        assert!(first_blocks[0].get("cache_control").is_none());

        // Total breakpoints: system + tools-tail + transcript-tail = 3 <= 4.
        assert_eq!(count_cache_control(&body), 3);
        assert!(count_cache_control(&body) <= 4);
    }

    #[test]
    fn no_cache_control_for_non_anthropic_model() {
        // Same fixture, caching gate OFF (a non-Anthropic OpenRouter model).
        let q = caching_fixture("deepseek/deepseek-r1");
        let body = build_request_body(&q, false);

        // System stays a plain string (pre-caching shape, no unknown field):
        // the static system and the volatile context flattened back into one
        // string exactly as the pre-split runner produced it.
        assert!(body["system"].is_string());
        assert_eq!(body["system"], q.combined_system());
        assert_eq!(
            body["system"],
            "you are a large stable system prompt\n\n\
             Conversation context: this turn is in a direct conversation on cli; \
             3 prior entries in session history."
        );
        // No cache_control anywhere — a strict gateway would reject it.
        assert_eq!(count_cache_control(&body), 0);
        // The volatile context is NOT appended as a separate message block on
        // the non-caching path (it lives in the flat system string instead),
        // so the last user message still has exactly its original one block.
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        assert_eq!(last["content"].as_array().unwrap().len(), 1);
        assert_eq!(last["content"][0]["text"], "second question");
        // Tools are present but unmarked.
        let tools = body["tools"].as_array().unwrap();
        assert!(tools.iter().all(|t| t.get("cache_control").is_none()));
    }

    #[test]
    fn cached_prefix_bytes_are_stable_across_turns_even_when_context_differs() {
        // Two successive turns. Turn 2 grows the transcript AND carries a
        // DIFFERENT volatile conversation-context (its history depth and
        // batch shape changed — the realistic live case). The cached prefix
        // — the static system block, the tools array, and every transcript
        // block up to and including turn-1's tail breakpoint — must be
        // byte-identical so the cache HITS, despite the context differing.
        let turn1 = caching_fixture("claude-sonnet-4-6");
        let body1 = build_request_body(&turn1, true);

        // Turn 2: the prior transcript plus one appended exchange, AND a
        // changed volatile context (different history-depth phrasing).
        let mut turn2 = turn1.clone();
        turn2.system_context = Some(
            "Conversation context: this turn is in a direct conversation on cli; \
             5 prior entries in session history."
                .into(),
        );
        turn2.history.push(HistoryMessage::Assistant {
            content: "second answer".into(),
        });
        turn2.history.push(HistoryMessage::User {
            content: "third question".into(),
        });
        let body2 = build_request_body(&turn2, true);

        // Sanity: the volatile context really did differ between the turns.
        assert_ne!(turn1.system_context, turn2.system_context);

        // System + tools are stable verbatim across turns (the volatile
        // context lives in the messages, never the system block).
        assert_eq!(
            serde_json::to_vec(&body1["system"]).unwrap(),
            serde_json::to_vec(&body2["system"]).unwrap(),
            "static system block (incl. its breakpoint) must be byte-stable"
        );
        assert_eq!(
            serde_json::to_vec(&body1["tools"]).unwrap(),
            serde_json::to_vec(&body2["tools"]).unwrap(),
            "tools array (incl. its tail breakpoint) must be byte-stable"
        );

        // Turn 1 WROTE a cache spanning everything up to and including its
        // tail breakpoint (the "second question" block) — excluding the
        // volatile context block, which lands AFTER the breakpoint and so
        // is outside the cached span. For the cache to HIT on turn 2, that
        // exact byte sequence must be a PREFIX of turn 2's serialized
        // message blocks. Anthropic matches the longest common prefix, so
        // "prefix of turn 2" is the precise correctness condition.
        let cached1 = cached_message_prefix(&body1);
        let turn2_blocks = flatten_message_blocks(&body2);
        assert!(
            turn2_blocks.starts_with(&cached1),
            "turn 1's cached message prefix must be a byte-stable prefix of \
             turn 2's messages so the cache hits; cached1.len()={}, \
             turn2.len()={}",
            cached1.len(),
            turn2_blocks.len(),
        );
        assert!(
            !cached1.is_empty(),
            "the cached prefix must be non-empty (the cache must actually span something)"
        );

        // And the volatile context block itself DID change between turns —
        // proving the variation is real and that it lives strictly past the
        // breakpoint (i.e. it never entered the cached prefix asserted above).
        let ctx1 = volatile_context_block(&body1);
        let ctx2 = volatile_context_block(&body2);
        assert_ne!(
            ctx1, ctx2,
            "the volatile context block must differ across the two turns"
        );
    }

    /// Flatten a body's `messages` into the sequence of (role, block) pairs
    /// the way Anthropic concatenates them for cache-prefix matching, with
    /// every `cache_control` marker stripped so two turns can be compared on
    /// their stable content alone. Each element is `[role, block]`.
    fn flatten_message_blocks(body: &Value) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        for msg in body["messages"].as_array().unwrap() {
            let role = msg["role"].clone();
            for block in msg["content"].as_array().unwrap() {
                let mut b = block.clone();
                strip_all_cache_control(std::slice::from_mut(&mut b));
                out.push(json!([role.clone(), b]));
            }
        }
        out
    }

    /// Turn 1's cached message prefix: the flattened (role, block) sequence
    /// up to AND INCLUDING the block carrying the transcript-tail
    /// `cache_control`. Everything after it (the volatile context block) is
    /// dropped — it is outside the cached span.
    fn cached_message_prefix(body: &Value) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        'outer: for msg in body["messages"].as_array().unwrap() {
            let role = msg["role"].clone();
            for block in msg["content"].as_array().unwrap() {
                let has_bp = block.get("cache_control").is_some();
                let mut b = block.clone();
                strip_all_cache_control(std::slice::from_mut(&mut b));
                out.push(json!([role.clone(), b]));
                if has_bp {
                    break 'outer; // nothing past the breakpoint is cached
                }
            }
        }
        out
    }

    /// The volatile conversation-context block: the trailing text block on
    /// the last user message that carries no `cache_control`.
    fn volatile_context_block(body: &Value) -> Value {
        let last = body["messages"].as_array().unwrap().last().unwrap();
        last["content"].as_array().unwrap().last().unwrap().clone()
    }

    /// Recursively drop every `cache_control` key so two bodies can be
    /// compared on their stable content alone.
    fn strip_all_cache_control(msgs: &mut [Value]) {
        fn walk(v: &mut Value) {
            match v {
                Value::Object(map) => {
                    map.remove("cache_control");
                    for child in map.values_mut() {
                        walk(child);
                    }
                }
                Value::Array(arr) => {
                    for child in arr.iter_mut() {
                        walk(child);
                    }
                }
                _ => {}
            }
        }
        for m in msgs {
            walk(m);
        }
    }

    #[test]
    fn caching_breakpoint_total_stays_within_anthropic_limit() {
        // Even with a long history and many tools, we never exceed 4
        // breakpoints (we place exactly 3: system, tools-tail, txn-tail).
        let mut q = caching_fixture("anthropic/claude-3.7-sonnet");
        for i in 0..20 {
            q.history.push(HistoryMessage::User {
                content: format!("msg {i}"),
            });
        }
        for i in 0..30 {
            q.tools.push(ToolDef {
                name: format!("tool_{i}"),
                description: "d".into(),
                input_schema: json!({ "type": "object" }),
            });
        }
        let body = build_request_body(&q, true);
        assert!(
            count_cache_control(&body) <= 4,
            "must not exceed Anthropic's 4-breakpoint cap, got {}",
            count_cache_control(&body)
        );
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
    fn direct_anthropic_host_gates_the_caching_beta_header() {
        // `new()` and an explicit api.anthropic.com base both flag the
        // direct host (beta header sent). A gateway URL (OpenRouter, a
        // proxy, or a wiremock test server) does NOT — those forward
        // cache_control natively and may not recognize the beta header.
        assert!(AnthropicProvider::new("k").inner.is_anthropic_host);
        assert!(
            AnthropicProvider::with_base_url("k", "https://api.anthropic.com/v1")
                .inner
                .is_anthropic_host
        );
        assert!(
            !AnthropicProvider::with_base_url("k", "https://openrouter.ai/api/v1")
                .inner
                .is_anthropic_host
        );
        assert!(
            !AnthropicProvider::with_base_url("k", "http://127.0.0.1:8080")
                .inner
                .is_anthropic_host
        );
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
