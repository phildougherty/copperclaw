//! Integration tests for `OllamaProvider` in *shim* mode — i.e. against an
//! Anthropic-compatible proxy in front of Ollama (`LiteLLM`, etc.). These
//! tests exist to keep the legacy facade behaviour pinned. The native
//! `/api/chat` path is covered in `ollama_conformance.rs`.

use copperclaw_providers::{
    AgentProvider, HistoryMessage, OllamaProvider, ProviderError, QueryInput,
};
use copperclaw_types::ProviderEvent;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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

fn basic_input() -> QueryInput {
    let mut q = QueryInput::new("you are helpful", "");
    q.history.push(HistoryMessage::User { content: "hi".into() });
    q
}

fn expect_query_err<T>(r: Result<T, ProviderError>) -> ProviderError {
    match r {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn ollama_happy_path() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"oll_01"}}"#,
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
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ollama"}}"#,
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

    let p = OllamaProvider::shim(server.uri(), Some("qwen2:7b".into()));
    let mut q = p.query(basic_input()).await.expect("query starts");
    let first = q.next_event().await.expect("init");
    match first {
        ProviderEvent::Init { continuation } => assert_eq!(continuation, "oll_01"),
        other => panic!("expected init, got {other:?}"),
    }
    let mut got = None;
    while let Some(ev) = q.next_event().await {
        if let ProviderEvent::Result { text } = ev {
            got = Some(text);
            break;
        }
    }
    assert_eq!(got.unwrap().as_deref(), Some("hello ollama"));
}

#[tokio::test]
async fn ollama_default_model_substituted_on_empty() {
    let server = MockServer::start().await;
    // Capture the request body so we can confirm the model was rewritten.
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"oll_02"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(|req: &Request| {
            let v: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
            v.get("model").and_then(|m| m.as_str()) == Some("llama3.1:8b")
        })
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = OllamaProvider::shim(server.uri(), None);
    let mut q = p.query(basic_input()).await.expect("query starts");
    let _init = q.next_event().await.unwrap();
    let result = q.next_event().await.unwrap();
    assert!(matches!(result, ProviderEvent::Result { .. }));
}

#[tokio::test]
async fn ollama_explicit_model_passed_through() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"oll_03"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(|req: &Request| {
            let v: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
            v.get("model").and_then(|m| m.as_str()) == Some("mistral:7b")
        })
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let p = OllamaProvider::shim(server.uri(), Some("custom:override".into()));
    let mut q = QueryInput::new("s", "mistral:7b");
    q.history.push(HistoryMessage::User { content: "hi".into() });
    let mut handle = p.query(q).await.expect("query starts");
    let _ = handle.next_event().await.unwrap();
    let result = handle.next_event().await.unwrap();
    assert!(matches!(result, ProviderEvent::Result { .. }));
}

#[tokio::test]
async fn ollama_error_mapping_is_inherited() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("backend dead"))
        .mount(&server)
        .await;
    let p = OllamaProvider::shim(server.uri(), None);
    let err = expect_query_err(p.query(basic_input()).await);
    match err {
        ProviderError::Api { status, message } => {
            assert_eq!(status, 500);
            assert_eq!(message, "backend dead");
        }
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn ollama_provider_name() {
    let server = MockServer::start().await;
    let p = OllamaProvider::shim(server.uri(), None);
    assert_eq!(p.name(), "ollama");
    assert!(!p.supports_native_slash_commands());
}
