//! HTTP-level integration tests for `AnthropicProvider` using wiremock.

use copperclaw_providers::{
    AgentProvider, AnthropicProvider, HistoryMessage, ProviderError, QueryInput,
};
use copperclaw_types::ProviderEvent;
use wiremock::matchers::{header, method, path};
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
