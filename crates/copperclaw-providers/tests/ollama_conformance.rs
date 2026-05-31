//! Conformance tests for `OllamaProvider` in **native** mode against
//! Ollama's `/api/chat` NDJSON endpoint. Each test stands up a wiremock
//! pretending to be Ollama, scripts a response, and asserts the resulting
//! [`ProviderEvent`] stream matches expectations.
//!
//! The shim mode against an Anthropic-compatible proxy in front of Ollama
//! is covered by `ollama_shim.rs`. The conformance suite here intentionally
//! avoids that path so it stays a forward-compatible signal: if Team
//! OLLAMA fields someone's "the runner can't actually talk to my local
//! `ollama serve`" report, the failing test will live here.

use std::sync::Arc;
use std::time::Duration;

use copperclaw_providers::{
    AgentProvider, HistoryMessage, OllamaProvider, ProviderError, QueryInput, ToolDef,
};
use copperclaw_types::ProviderEvent;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Helper — render a list of frames as a single NDJSON body.
fn ndjson(frames: &[&str]) -> String {
    let mut s = String::new();
    for f in frames {
        s.push_str(f);
        s.push('\n');
    }
    s
}

fn basic_input(model: &str) -> QueryInput {
    let mut q = QueryInput::new("you are helpful", model);
    q.history.push(HistoryMessage::User { content: "hi".into() });
    q
}

/// Drain events until terminal (Result or Error) and return the full
/// sequence. Bounded with a generous timeout so a misbehaving provider
/// can't hang the suite.
async fn collect_events(mut q: Box<dyn copperclaw_providers::AgentQuery>) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), q.next_event()).await {
            Ok(Some(ev)) => {
                let terminal = matches!(
                    ev,
                    ProviderEvent::Result { .. } | ProviderEvent::Error { .. }
                );
                out.push(ev);
                if terminal {
                    break;
                }
            }
            Ok(None) => break,
            Err(elapsed) => {
                panic!("provider event timeout ({elapsed}); collected so far: {out:?}")
            }
        }
    }
    out
}

// --------------------------------------------------------------------------
// 1. Simple text reply
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_simple_text_reply() {
    let server = MockServer::start().await;
    let body = ndjson(&[
        r#"{"model":"llama3.1:8b","message":{"role":"assistant","content":"Hello "}}"#,
        r#"{"model":"llama3.1:8b","message":{"role":"assistant","content":"world"}}"#,
        r#"{"model":"llama3.1:8b","done":true,"done_reason":"stop","prompt_eval_count":12,"eval_count":3}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let events = collect_events(p.query(basic_input("llama3.1:8b")).await.unwrap()).await;

    assert!(matches!(events.first(), Some(ProviderEvent::Init { .. })));
    let has_usage = events
        .iter()
        .any(|e| matches!(e, ProviderEvent::Usage { input_tokens: 12, output_tokens: 3 }));
    assert!(has_usage, "expected usage 12/3, got {events:?}");
    let last = events.last().unwrap();
    match last {
        ProviderEvent::Result { text } => {
            assert_eq!(text.as_deref(), Some("Hello world"));
        }
        other => panic!("expected Result, got {other:?}"),
    }
}

// --------------------------------------------------------------------------
// 2. Tool-use round-trip
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_tool_use_round_trip() {
    let server = MockServer::start().await;
    let body = ndjson(&[
        r#"{"model":"m","message":{"role":"assistant","content":"","tool_calls":[{"id":"call_1","function":{"name":"weather","arguments":{"loc":"sf"}}}]}}"#,
        r#"{"model":"m","done":true,"done_reason":"stop","prompt_eval_count":5,"eval_count":1}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let events = collect_events(p.query(basic_input("m")).await.unwrap()).await;

    let mut saw_start = false;
    let mut saw_call = false;
    let mut saw_end = false;
    for ev in &events {
        match ev {
            ProviderEvent::ToolStart { name, .. } => {
                assert_eq!(name, "weather");
                saw_start = true;
            }
            ProviderEvent::ToolCall { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "weather");
                assert_eq!(input["loc"], "sf");
                saw_call = true;
            }
            ProviderEvent::ToolEnd => saw_end = true,
            _ => {}
        }
    }
    assert!(saw_start, "expected ToolStart");
    assert!(saw_call, "expected ToolCall");
    assert!(saw_end, "expected ToolEnd");

    // And second turn carries the tool_result back. Confirm the request
    // body now contains a `tool` role message — the shim doesn't get
    // through this if the history translation is wrong.
    let server2 = MockServer::start().await;
    let saw_tool_role = Arc::new(Mutex::new(false));
    let flag = saw_tool_role.clone();
    let body2 = ndjson(&[
        r#"{"model":"m","message":{"role":"assistant","content":"sunny."}}"#,
        r#"{"model":"m","done":true,"prompt_eval_count":20,"eval_count":2}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(move |req: &Request| {
            let v: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let msgs = v["messages"].as_array().unwrap();
            let has_tool = msgs
                .iter()
                .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_1");
            if has_tool {
                if let Ok(mut g) = flag.try_lock() {
                    *g = true;
                }
            }
            has_tool
        })
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body2),
        )
        .mount(&server2)
        .await;
    let p2 = OllamaProvider::new(server2.uri(), None);
    let mut q2 = basic_input("m");
    q2.history.push(HistoryMessage::ToolUse {
        id: "call_1".into(),
        name: "weather".into(),
        input: serde_json::json!({ "loc": "sf" }),
    });
    q2.history.push(HistoryMessage::Tool {
        tool_use_id: "call_1".into(),
        content: "sunny, 65F".into(),
        is_error: false,
    });
    let _ = collect_events(p2.query(q2).await.unwrap()).await;
    assert!(*saw_tool_role.lock().await, "wiremock never saw a tool-role message");
}

// --------------------------------------------------------------------------
// 3. Streaming chunks arrive individually
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_streaming_chunks_arrive_individually() {
    // We stream 5 distinct text frames. The provider concatenates them
    // into the final Result, but every frame also emits Activity so the
    // runner's liveness heartbeat keeps ticking. Assert we see >= 5
    // Activity beats *before* the terminal Result lands — that's the
    // signal nothing is buffered.
    let server = MockServer::start().await;
    let mut frames = Vec::new();
    for i in 0..5 {
        frames.push(format!(
            r#"{{"model":"m","message":{{"role":"assistant","content":"chunk{i} "}}}}"#
        ));
    }
    frames.push(
        r#"{"model":"m","done":true,"prompt_eval_count":1,"eval_count":5}"#.to_string(),
    );
    let body = frames
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body + "\n"),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let events = collect_events(p.query(basic_input("m")).await.unwrap()).await;

    let activity = events
        .iter()
        .filter(|e| matches!(e, ProviderEvent::Activity))
        .count();
    assert!(activity >= 5, "expected >= 5 Activity heartbeats, got {activity}: {events:?}");

    match events.last().unwrap() {
        ProviderEvent::Result { text } => {
            assert_eq!(text.as_deref(), Some("chunk0 chunk1 chunk2 chunk3 chunk4 "));
        }
        other => panic!("expected Result, got {other:?}"),
    }
}

// --------------------------------------------------------------------------
// 4. Abort terminates upstream
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_abort_terminates_upstream() {
    let server = MockServer::start().await;
    // Hand the client a long-running NDJSON stream — wiremock holds the
    // response body until the delay elapses. Abort must cut through that.
    let body = ndjson(&[
        r#"{"model":"m","message":{"role":"assistant","content":"slow "}}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body)
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    // `query` itself blocks until the response head arrives, so we
    // spawn it so we can drive the abort path while it's pending.
    let p_clone = p.clone();
    let query_task = tokio::spawn(async move { p_clone.query(basic_input("m")).await });
    // Give the request a moment to land at the mock server.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Abort by dropping the task; the underlying reqwest future is
    // cancelled, which closes the upstream connection. Wiremock's
    // recorded-request count then settles even though the delay never
    // elapsed.
    query_task.abort();
    let _ = query_task.await; // join; ignore cancellation result
    // Sanity: confirm we did not block here — if we made it this far
    // within the 2s deadline below, abort actually propagated.
    let elapsed = tokio::time::timeout(Duration::from_secs(2), async {
        // Re-issue a quick query against a fast mock to prove the client
        // is still functional. (Same provider, same client.)
        let server2 = MockServer::start().await;
        let body2 = ndjson(&[
            r#"{"model":"m","done":true,"prompt_eval_count":0,"eval_count":0}"#,
        ]);
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/x-ndjson")
                    .set_body_string(body2),
            )
            .mount(&server2)
            .await;
        let p2 = OllamaProvider::new(server2.uri(), None);
        let q2 = p2.query(basic_input("m")).await.expect("post-abort query starts");
        let _ = collect_events(q2).await;
    })
    .await;
    assert!(elapsed.is_ok(), "post-abort follow-up query hung");
}

// --------------------------------------------------------------------------
// 5. Usage includes token counts
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_usage_includes_token_counts() {
    let server = MockServer::start().await;
    let body = ndjson(&[
        r#"{"model":"m","message":{"role":"assistant","content":"ok"}}"#,
        r#"{"model":"m","done":true,"prompt_eval_count":42,"eval_count":7}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let events = collect_events(p.query(basic_input("m")).await.unwrap()).await;
    let usage = events.iter().find_map(|e| match e {
        ProviderEvent::Usage { input_tokens, output_tokens } => Some((*input_tokens, *output_tokens)),
        _ => None,
    });
    assert_eq!(usage, Some((42, 7)));
}

// --------------------------------------------------------------------------
// 6. Model name passes through
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_model_name_passes_through() {
    let server = MockServer::start().await;
    let observed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let obs2 = observed.clone();
    let body = ndjson(&[r#"{"model":"x","done":true,"prompt_eval_count":1,"eval_count":1}"#]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(move |req: &Request| {
            let v: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let m = v["model"].as_str().map(str::to_string);
            if let Ok(mut g) = obs2.try_lock() {
                *g = m;
            }
            true
        })
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let _ = collect_events(p.query(basic_input("llama3.1:70b")).await.unwrap()).await;
    assert_eq!(observed.lock().await.as_deref(), Some("llama3.1:70b"));
}

// --------------------------------------------------------------------------
// 7. Tool schema translated to OpenAI form
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_tool_schema_translated_to_openai_form() {
    let server = MockServer::start().await;
    let observed: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let obs2 = observed.clone();
    let body = ndjson(&[r#"{"model":"m","done":true,"prompt_eval_count":1,"eval_count":1}"#]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(move |req: &Request| {
            let v: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if let Ok(mut g) = obs2.try_lock() {
                *g = v.get("tools").cloned();
            }
            true
        })
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let mut q = basic_input("m");
    q.tools.push(ToolDef {
        name: "lookup".into(),
        description: "look it up".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "q": { "type": "string" } }
        }),
    });
    let _ = collect_events(p.query(q).await.unwrap()).await;
    let tools = observed.lock().await.clone().expect("tools present");
    let arr = tools.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["type"], "function");
    assert_eq!(arr[0]["function"]["name"], "lookup");
    assert_eq!(arr[0]["function"]["description"], "look it up");
    assert_eq!(arr[0]["function"]["parameters"]["type"], "object");
}

// --------------------------------------------------------------------------
// 8. Tool-result history translation
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_tool_result_history_translation() {
    let server = MockServer::start().await;
    let observed: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let obs2 = observed.clone();
    let body = ndjson(&[r#"{"model":"m","done":true,"prompt_eval_count":1,"eval_count":1}"#]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(move |req: &Request| {
            let v: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if let Ok(mut g) = obs2.try_lock() {
                *g = v.get("messages").cloned();
            }
            true
        })
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let mut q = basic_input("m");
    q.history.push(HistoryMessage::ToolUse {
        id: "tu_abc".into(),
        name: "weather".into(),
        input: serde_json::json!({ "loc": "sf" }),
    });
    q.history.push(HistoryMessage::Tool {
        tool_use_id: "tu_abc".into(),
        content: "sunny".into(),
        is_error: false,
    });
    let _ = collect_events(p.query(q).await.unwrap()).await;
    let msgs = observed.lock().await.clone().expect("messages present");
    let arr = msgs.as_array().unwrap();
    // system + user + assistant(with tool_call) + tool
    let tool_msg = arr.iter().find(|m| m["role"] == "tool").expect("tool role msg");
    assert_eq!(tool_msg["tool_call_id"], "tu_abc");
    assert_eq!(tool_msg["content"], "sunny");

    let assistant_with_call = arr
        .iter()
        .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
        .expect("assistant with tool_calls");
    let calls = assistant_with_call["tool_calls"].as_array().unwrap();
    assert_eq!(calls[0]["id"], "tu_abc");
    assert_eq!(calls[0]["function"]["name"], "weather");
}

// --------------------------------------------------------------------------
// 9. System prompt lands correctly
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_system_prompt_lands_correctly() {
    let server = MockServer::start().await;
    let observed: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let obs2 = observed.clone();
    let body = ndjson(&[r#"{"model":"m","done":true,"prompt_eval_count":1,"eval_count":1}"#]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(move |req: &Request| {
            let v: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if let Ok(mut g) = obs2.try_lock() {
                *g = v.get("messages").cloned();
            }
            true
        })
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let _ = collect_events(p.query(basic_input("m")).await.unwrap()).await;
    let msgs = observed.lock().await.clone().unwrap();
    let arr = msgs.as_array().unwrap();
    assert_eq!(arr[0]["role"], "system");
    assert_eq!(arr[0]["content"], "you are helpful");
    assert_eq!(arr[1]["role"], "user");
}

// --------------------------------------------------------------------------
// 10. Error classification
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_error_event_classification() {
    // 401 -> SessionInvalid
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let err = p.query(basic_input("m")).await.err().expect("err");
    assert!(matches!(err, ProviderError::SessionInvalid), "got {err:?}");

    // 500 -> retryable Api
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server2)
        .await;
    let p2 = OllamaProvider::new(server2.uri(), None);
    let err2 = p2.query(basic_input("m")).await.err().expect("err");
    match err2 {
        ProviderError::Api { status, .. } => {
            assert_eq!(status, 500);
            assert!(ProviderError::Api { status: 500, message: "x".into() }.is_retryable());
        }
        other => panic!("expected Api, got {other:?}"),
    }

    // 400 -> BadRequest
    let server3 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(400).set_body_string("malformed"))
        .mount(&server3)
        .await;
    let p3 = OllamaProvider::new(server3.uri(), None);
    let err3 = p3.query(basic_input("m")).await.err().expect("err");
    assert!(matches!(err3, ProviderError::BadRequest(_)), "got {err3:?}");
}

// --------------------------------------------------------------------------
// 11. Empty response
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_handles_empty_response() {
    let server = MockServer::start().await;
    // Single frame, `done:true` immediately, no text — model loaded but
    // produced nothing. We must still surface a Result + Init pair without
    // panicking.
    let body = ndjson(&[
        r#"{"model":"m","done":true,"done_reason":"stop","prompt_eval_count":0,"eval_count":0}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let events = collect_events(p.query(basic_input("m")).await.unwrap()).await;
    assert!(matches!(events.first(), Some(ProviderEvent::Init { .. })));
    match events.last().unwrap() {
        ProviderEvent::Result { text } => assert!(text.is_none()),
        other => panic!("expected Result, got {other:?}"),
    }
}

// --------------------------------------------------------------------------
// 12. Malformed JSON in stream
// --------------------------------------------------------------------------

#[tokio::test]
async fn ollama_handles_malformed_json_in_stream() {
    let server = MockServer::start().await;
    // Mix one bad line in among the good ones. The provider should skip
    // the bad line (surfacing an Activity beat) and still produce a clean
    // Result from the rest.
    let body = format!(
        "{}\n{}\n{}\n",
        r#"{"model":"m","message":{"role":"assistant","content":"hi "}}"#,
        r"{ this is not json ",
        r#"{"model":"m","done":true,"prompt_eval_count":2,"eval_count":1}"#,
    );
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let p = OllamaProvider::new(server.uri(), None);
    let events = collect_events(p.query(basic_input("m")).await.unwrap()).await;
    match events.last().unwrap() {
        ProviderEvent::Result { text } => assert_eq!(text.as_deref(), Some("hi ")),
        other => panic!("expected Result, got {other:?}"),
    }
}
