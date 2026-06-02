//! Axum router for the LINE Messaging API webhook.
//!
//! LINE POSTs a JSON payload to the configured URL. The whole body
//! gets HMAC-signed with the channel secret; we verify before
//! parsing, then walk `events[]` and emit one `InboundEvent` per
//! supported event.
//!
//! Currently we map:
//!
//! - `type: "message"` with `message.type: "text"` → text inbound
//!   with `platform_id = source.{userId|groupId|roomId}` and
//!   `thread_id = None` (LINE has no threads).
//! - everything else → ack with 200, no inbound emitted.
//!
//! Each inbound event stashes the `replyToken` on the
//! [`crate::adapter::LineAdapter`]'s reply-token cache keyed by
//! `source` so a subsequent `deliver` can use the cheap reply path.

use crate::adapter::ReplyTokenCache;
use crate::config::LineConfig;
use crate::signature::{SignatureOutcome, verify};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use chrono::Utc;
use copperclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

/// Shared state plumbed through the router handlers.
#[derive(Clone)]
pub struct RouterState {
    pub channel_type: ChannelType,
    pub config: LineConfig,
    pub inbound_tx: Sender<InboundEvent>,
    pub reply_tokens: Arc<ReplyTokenCache>,
}

impl RouterState {
    #[must_use]
    pub fn new(
        channel_type: ChannelType,
        config: LineConfig,
        inbound_tx: Sender<InboundEvent>,
        reply_tokens: Arc<ReplyTokenCache>,
    ) -> Self {
        Self {
            channel_type,
            config,
            inbound_tx,
            reply_tokens,
        }
    }
}

/// Build the axum router with the LINE webhook handler mounted at
/// the configured path.
pub fn build_router(state: RouterState) -> Router {
    let path = state.config.webhook.path.clone();
    Router::new().route(&path, post(handle)).with_state(state)
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    events: Vec<Event>,
}

#[derive(Debug, Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "replyToken")]
    reply_token: Option<String>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    source: Option<Source>,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Source {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "userId")]
    user_id: Option<String>,
    #[serde(default, rename = "groupId")]
    group_id: Option<String>,
    #[serde(default, rename = "roomId")]
    room_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

async fn handle(
    State(state): State<RouterState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    let sig = headers
        .get("x-line-signature")
        .and_then(|v| v.to_str().ok());
    match verify(&body, &state.config.channel_secret, sig) {
        SignatureOutcome::Ok => {}
        SignatureOutcome::HeaderMissing => {
            return (StatusCode::UNAUTHORIZED, "signature header missing");
        }
        SignatureOutcome::Mismatch(reason) => {
            tracing::warn!(reason, "line signature rejected");
            return (StatusCode::UNAUTHORIZED, "signature invalid");
        }
    }

    let envelope: Envelope = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "line webhook json parse failed");
            return (StatusCode::BAD_REQUEST, "invalid json");
        }
    };

    for event in envelope.events {
        if let Err(reason) = dispatch_event(&state, event).await {
            tracing::warn!(reason, "line event dropped");
        }
    }
    (StatusCode::OK, "ok")
}

async fn dispatch_event(state: &RouterState, event: Event) -> Result<(), &'static str> {
    if event.kind != "message" {
        // Future: handle follow / unfollow / postback. For now ack.
        return Ok(());
    }
    let message = event.message.ok_or("message event without message field")?;
    if message.kind != "text" {
        return Ok(());
    }
    let text = message.text.ok_or("text message without text body")?;
    let source = event.source.ok_or("event without source")?;
    let (platform_id, is_group) = source_to_platform_id(&source);
    let platform_id = platform_id.ok_or("source had no usable id")?;

    if let Some(rt) = event.reply_token.as_deref() {
        state.reply_tokens.put(platform_id.clone(), rt.to_string());
    }

    let timestamp = event
        .timestamp
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .unwrap_or_else(Utc::now);
    let inbound_id = message.id.unwrap_or_else(|| Uuid::new_v4().to_string());

    let sender = source.user_id.clone().map(|uid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: uid,
        display_name: None,
    });

    let event = InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id,
        thread_id: None,
        message: InboundMessage {
            id: inbound_id,
            kind: MessageKind::Chat,
            content: serde_json::json!({"text": text}),
            timestamp,
            is_mention: None,
            is_group: Some(is_group),
        },
        reply_to: None,
        sender,
    };
    state
        .inbound_tx
        .send(event)
        .await
        .map_err(|_| "inbound channel closed")?;
    Ok(())
}

/// Translate a LINE source object into `(platform_id, is_group)`.
///
/// LINE's three source types map cleanly:
///
/// - `"user"`  → 1:1 chat, `platform_id` = `userId`.
/// - `"group"` → group chat, `platform_id` = `groupId`.
/// - `"room"`  → multi-user chat (no admin), `platform_id` = `roomId`.
fn source_to_platform_id(source: &Source) -> (Option<String>, bool) {
    match source.kind.as_str() {
        "user" => (source.user_id.clone(), false),
        "group" => (source.group_id.clone(), true),
        "room" => (source.room_id.clone(), true),
        _ => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebhookBind;
    use crate::signature::compute_base64;
    use axum::body::Body;
    use axum::http::Request;
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

    fn cfg() -> LineConfig {
        LineConfig {
            channel_secret: "secret".into(),
            channel_access_token: "tok".into(),
            api_base: "https://api.line.me".into(),
            webhook: WebhookBind {
                host: "127.0.0.1".into(),
                port: 0,
                path: "/line/webhook".into(),
            },
        }
    }

    fn router(tx: Sender<InboundEvent>) -> (Router, Arc<ReplyTokenCache>) {
        let cache = Arc::new(ReplyTokenCache::new());
        let state = RouterState::new(ChannelType::new("line"), cfg(), tx, cache.clone());
        (build_router(state), cache)
    }

    async fn post_signed(
        router: Router,
        path: &str,
        secret: Option<&str>,
        body: &[u8],
    ) -> axum::http::Response<axum::body::Body> {
        let mut req = Request::builder().method("POST").uri(path);
        if let Some(s) = secret {
            let sig = compute_base64(s, body);
            req = req.header("x-line-signature", sig);
        }
        let req = req
            .header("content-type", "application/json")
            .body(Body::from(body.to_vec()))
            .unwrap();
        router.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn signed_text_message_event_emits_inbound() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, cache) = router(tx);
        let body = br#"{
            "events": [{
                "type": "message",
                "replyToken": "rt-1",
                "timestamp": 1700000000000,
                "source": {"type": "user", "userId": "U1"},
                "message": {"type": "text", "id": "m1", "text": "hello"}
            }]
        }"#;
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.platform_id, "U1");
        assert_eq!(event.message.content["text"], "hello");
        assert_eq!(event.message.is_group, Some(false));
        assert_eq!(event.sender.as_ref().unwrap().identity, "U1");
        assert_eq!(cache.take(&"U1".to_string()).as_deref(), Some("rt-1"));
    }

    #[tokio::test]
    async fn group_source_marks_is_group_true() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = br#"{"events":[{
            "type":"message",
            "source":{"type":"group","groupId":"G42"},
            "message":{"type":"text","text":"hi"}
        }]}"#;
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.platform_id, "G42");
        assert_eq!(event.message.is_group, Some(true));
    }

    #[tokio::test]
    async fn room_source_marks_is_group_true() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = br#"{"events":[{
            "type":"message",
            "source":{"type":"room","roomId":"R42"},
            "message":{"type":"text","text":"hi"}
        }]}"#;
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.platform_id, "R42");
        assert_eq!(event.message.is_group, Some(true));
    }

    #[tokio::test]
    async fn missing_signature_returns_401() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = b"{}";
        let resp = post_signed(router, "/line/webhook", None, body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn bad_signature_returns_401() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = b"{}";
        let resp = post_signed(router, "/line/webhook", Some("wrong"), body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = b"not-json";
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn non_message_event_is_acked_and_dropped() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = br#"{"events":[{"type":"follow","source":{"type":"user","userId":"U"}}]}"#;
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_text_message_is_acked_and_dropped() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = br#"{"events":[{
            "type":"message",
            "source":{"type":"user","userId":"U"},
            "message":{"type":"image","id":"m"}
        }]}"#;
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn batched_events_emit_in_order() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = br#"{"events":[
            {"type":"message","source":{"type":"user","userId":"A"},"message":{"type":"text","text":"first"}},
            {"type":"message","source":{"type":"user","userId":"B"},"message":{"type":"text","text":"second"}}
        ]}"#;
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let a = rx.try_recv().unwrap();
        let b = rx.try_recv().unwrap();
        assert_eq!(a.platform_id, "A");
        assert_eq!(b.platform_id, "B");
    }

    #[tokio::test]
    async fn empty_events_array_returns_200() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (router, _cache) = router(tx);
        let body = br#"{"events":[]}"#;
        let resp = post_signed(router, "/line/webhook", Some("secret"), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err());
    }
}
