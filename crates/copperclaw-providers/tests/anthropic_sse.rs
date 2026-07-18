//! HTTP-level integration tests for `AnthropicProvider` using wiremock.

use copperclaw_providers::{
    AgentProvider, AnthropicProvider, HistoryMessage, ProviderError, QueryInput,
};
use copperclaw_types::ProviderEvent;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse_body(events: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (event, data) in events {
        out.push_str("event: ");
        out.push_str(event);
        out.push('\n');
        out.push_str("data: ");
        out.push_str(data);
        out.push_str("\n\n");
    }
    out
}

fn provider(server: &MockServer) -> AnthropicProvider {
    AnthropicProvider::with_base_url("test-key", server.uri())
}

/// `unwrap_err` requires `Debug` on the `Ok` arm, but `Box<dyn AgentQuery>`
/// is not `Debug` — this helper avoids that constraint.
fn expect_query_err<T>(r: Result<T, ProviderError>) -> ProviderError {
    match r {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    }
}

fn basic_input() -> QueryInput {
    let mut q = QueryInput::new("you are helpful", "claude-sonnet-4-6");
    q.history.push(HistoryMessage::User {
        content: "hi".into(),
    });
    q
}

#[tokio::test]
async fn happy_path_single_text_turn() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_01"}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = p.query(basic_input()).await.expect("query starts");

    let first = q.next_event().await.expect("init event");
    match first {
        ProviderEvent::Init { continuation } => assert_eq!(continuation, "msg_01"),
        other => panic!("expected Init, got {other:?}"),
    }

    let mut got_result = None;
    while let Some(ev) = q.next_event().await {
        if let ProviderEvent::Result { text } = ev {
            got_result = Some(text);
            break;
        }
    }
    assert_eq!(got_result.unwrap().as_deref(), Some("hello world"));
}

#[tokio::test]
async fn happy_path_empty_text_yields_none() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_02"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();
    let _init = q.next_event().await.unwrap();
    let result = q.next_event().await.unwrap();
    match result {
        ProviderEvent::Result { text } => assert!(text.is_none()),
        other => panic!("expected Result, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_use_emits_tool_start_and_end() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_03"}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"weather","input":{}}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();

    let mut saw_start = false;
    let mut saw_end = false;
    let mut saw_result = false;
    while let Some(ev) = q.next_event().await {
        match ev {
            ProviderEvent::ToolStart {
                name,
                declared_timeout_ms,
            } => {
                assert_eq!(name, "weather");
                assert!(declared_timeout_ms.is_none());
                saw_start = true;
            }
            ProviderEvent::ToolEnd => saw_end = true,
            ProviderEvent::Result { .. } => {
                saw_result = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_start && saw_end && saw_result);
}

#[tokio::test]
async fn rate_limit_is_overloaded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "5")
                .set_body_string(
                    "{\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}",
                ),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = expect_query_err(p.query(basic_input()).await);
    assert!(matches!(err, ProviderError::Overloaded));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn overload_529_is_overloaded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_string("overloaded"))
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = expect_query_err(p.query(basic_input()).await);
    assert!(matches!(err, ProviderError::Overloaded));
}

#[tokio::test]
async fn unauthorized_is_session_invalid() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("auth failed"))
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = expect_query_err(p.query(basic_input()).await);
    assert!(matches!(err, ProviderError::SessionInvalid));
    assert!(p.is_session_invalid(&err));
}

#[tokio::test]
async fn server_error_is_api_with_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("kaboom"))
        .mount(&server)
        .await;

    let p = provider(&server);
    let err = expect_query_err(p.query(basic_input()).await);
    match err {
        ProviderError::Api { status, message } => {
            assert_eq!(status, 500);
            assert_eq!(message, "kaboom");
        }
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_request_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("missing model"))
        .mount(&server)
        .await;
    let p = provider(&server);
    let err = expect_query_err(p.query(basic_input()).await);
    assert!(matches!(err, ProviderError::BadRequest(_)));
}

#[tokio::test]
async fn malformed_sse_event_surfaces_decode_error() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_04"}}"#,
        ),
        ("content_block_delta", "not-json"),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();
    let _init = q.next_event().await.unwrap();
    let next = q.next_event().await.unwrap();
    match next {
        ProviderEvent::Error { message, retryable } => {
            assert!(message.contains("malformed event json"));
            assert!(!retryable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn upstream_error_event_is_forwarded() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_05"}}"#,
        ),
        (
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"slow"}}"#,
        ),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();
    let _init = q.next_event().await.unwrap();
    let next = q.next_event().await.unwrap();
    match next {
        ProviderEvent::Error { message, retryable } => {
            assert_eq!(message, "slow");
            assert!(retryable);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn ping_event_surfaces_activity() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_06"}}"#,
        ),
        ("ping", r#"{"type":"ping"}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();
    let _ = q.next_event().await.unwrap();
    let mut saw_activity = false;
    while let Some(ev) = q.next_event().await {
        if matches!(ev, ProviderEvent::Activity) {
            saw_activity = true;
        }
        if matches!(ev, ProviderEvent::Result { .. }) {
            break;
        }
    }
    assert!(saw_activity);
}

#[tokio::test]
async fn push_is_rejected_for_anthropic() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_07"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();
    let err = q.push("hi".into()).await.unwrap_err();
    assert!(matches!(err, ProviderError::BadRequest(_)));
    q.end().await.unwrap();
}

#[tokio::test]
async fn abort_drops_pending_stream() {
    let server = MockServer::start().await;
    // Slow stream — only an init, then nothing for a long time.
    let body = sse_body(&[(
        "message_start",
        r#"{"type":"message_start","message":{"id":"msg_08"}}"#,
    )]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body)
                .set_delay(std::time::Duration::from_millis(0)),
        )
        .mount(&server)
        .await;
    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();
    let _init = q.next_event().await.unwrap();
    q.abort().await;
    // After abort the channel is closed; further events return None.
    assert!(q.next_event().await.is_none());
}

#[tokio::test]
async fn caching_request_body_carries_cache_control_for_anthropic_model() {
    // Wiremock captures the outbound request so we can assert the serialized
    // JSON body carries `cache_control` on the system prompt and the tools
    // tail for a Claude-family model.
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_c1"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = QueryInput::new("a stable system prompt", "claude-sonnet-4-6");
    q.tools.push(copperclaw_providers::ToolDef {
        name: "t".into(),
        description: "d".into(),
        input_schema: serde_json::json!({ "type": "object" }),
    });
    q.history.push(HistoryMessage::User {
        content: "hi".into(),
    });
    let mut query = p.query(q).await.expect("query starts");
    while query.next_event().await.is_some() {}

    // Inspect the recorded request body.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["system"][0]["cache_control"]["type"], "ephemeral");
    let tools = sent["tools"].as_array().unwrap();
    assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");
    let messages = sent["messages"].as_array().unwrap();
    let last = messages.last().unwrap();
    let blocks = last["content"].as_array().unwrap();
    assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral");
}

#[tokio::test]
async fn caching_request_body_splits_static_system_from_volatile_context() {
    // The HTTP-level proof of the prompt-caching fix: when a volatile
    // `system_context` is set (the per-inbound "Conversation context"
    // paragraph), the wire body must keep the STATIC system prompt as the
    // sole cached system block, and carry the volatile context as a trailing
    // block on the last user message AFTER the transcript-tail breakpoint —
    // so the cached prefix stays byte-stable across inbounds.
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_c3"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = QueryInput::new("a stable system prompt", "claude-sonnet-4-6");
    q.system_context = Some("Conversation context: 7 prior entries in session history.".into());
    q.history.push(HistoryMessage::User {
        content: "the actual question".into(),
    });
    let mut query = p.query(q).await.expect("query starts");
    while query.next_event().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();

    // System: exactly one block — the STATIC prompt — carrying the
    // breakpoint, with NO volatile context leaked in.
    let system = sent["system"].as_array().expect("system is a block array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["text"], "a stable system prompt");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert!(
        !system[0]["text"]
            .as_str()
            .unwrap()
            .contains("Conversation context"),
        "volatile context must not be in the cached system block"
    );

    // Last user message: the real question carries the transcript-tail
    // breakpoint, and the volatile context is a SUBSEQUENT, UNMARKED block.
    let last = sent["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    let blocks = last["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["text"], "the actual question");
    assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        blocks[1]["text"],
        "Conversation context: 7 prior entries in session history."
    );
    assert!(
        blocks[1].get("cache_control").is_none(),
        "the volatile context block must NOT carry a breakpoint"
    );
}

#[tokio::test]
async fn no_cache_control_in_request_body_for_non_anthropic_model() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_c2"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = QueryInput::new("a stable system prompt", "deepseek/deepseek-r1");
    q.tools.push(copperclaw_providers::ToolDef {
        name: "t".into(),
        description: "d".into(),
        input_schema: serde_json::json!({ "type": "object" }),
    });
    q.history.push(HistoryMessage::User {
        content: "hi".into(),
    });
    let mut query = p.query(q).await.expect("query starts");
    while query.next_event().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    let raw = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(
        !raw.contains("cache_control"),
        "non-Anthropic model must send NO cache_control; body was: {raw}"
    );
    let sent: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // System stays a plain string in the pre-caching shape.
    assert!(sent["system"].is_string());
}

#[tokio::test]
async fn cache_usage_tokens_surface_from_usage_event() {
    // The `message_start.usage` block carries Anthropic's prompt-caching
    // counters; the provider must forward them on `ProviderEvent::Usage`.
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_c3","usage":{"input_tokens":12,"output_tokens":0,"cache_read_input_tokens":4000,"cache_creation_input_tokens":50}}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = p.query(basic_input()).await.unwrap();
    let mut found = None;
    while let Some(ev) = q.next_event().await {
        if let ProviderEvent::Usage {
            input_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            ..
        } = ev
        {
            found = Some((input_tokens, cache_read_tokens, cache_creation_tokens));
        }
    }
    assert_eq!(found, Some((12, 4000, 50)));
}

#[tokio::test]
async fn image_input_404_strips_screenshots_and_retries() {
    // A text-only model behind an OpenRouter-style gateway rejects a transcript
    // carrying an image with a 404 "No endpoints found that support image
    // input". The provider must strip the image to a placeholder and retry
    // once, so a screenshot the agent took can't wedge every turn.
    let server = MockServer::start().await;
    let ok_body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_v1"}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);

    // Higher-priority mock: any request whose body carries an image block is
    // rejected exactly like a text-only OpenRouter model.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_string_contains("\"type\":\"image\""))
        .respond_with(ResponseTemplate::new(404).set_body_string(
            r#"{"error":{"message":"No endpoints found that support image input","code":404}}"#,
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    // Fallback: a request with NO image (the stripped retry) streams success.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(ok_body),
        )
        .with_priority(5)
        .mount(&server)
        .await;

    let p = provider(&server);
    let mut q = QueryInput::new("sys", "z-ai/glm-5.2");
    q.history.push(HistoryMessage::User {
        content: "build the UI".into(),
    });
    q.history.push(HistoryMessage::Image {
        media_type: "image/png".into(),
        data: "QUJD".into(),
    });

    let mut query = p
        .query(q)
        .await
        .expect("query should succeed after stripping images and retrying");
    let mut result = None;
    while let Some(ev) = query.next_event().await {
        if let ProviderEvent::Result { text } = ev {
            result = Some(text);
            break;
        }
    }
    assert_eq!(result.unwrap().as_deref(), Some("done"));

    // Two requests: the image attempt (404) then the stripped retry (200).
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected one image attempt + one retry");
    let first = String::from_utf8(requests[0].body.clone()).unwrap();
    let second = String::from_utf8(requests[1].body.clone()).unwrap();
    assert!(
        first.contains("\"type\":\"image\""),
        "first attempt must carry the image block"
    );
    assert!(
        !second.contains("\"type\":\"image\""),
        "the retry must not carry any image block; body was: {second}"
    );
    assert!(
        second.contains("does not support image input"),
        "the retry must carry the text placeholder"
    );
}

#[tokio::test]
async fn transport_error_when_server_down() {
    // Bind a port, drop the listener -> connection refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let url = format!("http://{addr}");
    let p = AnthropicProvider::with_base_url("k", url);
    let err = expect_query_err(p.query(basic_input()).await);
    assert!(matches!(err, ProviderError::Transport(_)));
    assert!(err.is_retryable());
}
