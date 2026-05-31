//! Axum router for the `WhatsApp` Cloud webhook.
//!
//! Two endpoints share the configured `path`:
//!
//! - `GET`  — verification handshake. Returns the literal `hub.challenge`
//!   value with status `200` when `hub.verify_token` matches the configured
//!   token, otherwise `403`. Missing required query parameters return `400`.
//! - `POST` — signed notifications. Body is HMAC-SHA256 verified against
//!   the configured app secret; mismatch returns `401`. Valid bodies are
//!   parsed into [`InboundEvent`] values and forwarded on the inbound
//!   sender. Duplicate `messages[].id` values are suppressed via an
//!   LRU-ish ring of capacity [`DEDUP_CAPACITY`].
//!
//! Inbound message types handled:
//!
//! | `WhatsApp` `type` | Result |
//! |---|---|
//! | `text` | `MessageKind::Chat`, `content = {"text": "..."}`. |
//! | `image` / `document` / `audio` / `video` | `MessageKind::Chat`, content with `attachment` metadata. |
//! | `button` | `MessageKind::Chat`, content `{"button","payload"}`. |
//! | `interactive` | `MessageKind::Chat`, content `{"interactive": ...}`. |
//! | `reaction` | accepted (200) but no inbound event. |
//!
//! `statuses[]` items (delivery/read receipts) are accepted with no
//! inbound event. Top-level body shapes that fail to parse return `400`.

use crate::signature::verify_signature;
use axum::{
    Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{TimeZone, Utc};
use copperclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, ReplyTo, SenderIdentity};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};

/// Maximum number of message ids we remember for duplicate suppression.
pub const DEDUP_CAPACITY: usize = 256;

/// Header `WhatsApp` uses for the body signature.
pub const SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// LRU-ish ring of recently-seen platform message ids.
#[derive(Debug, Default)]
pub struct EventDedup {
    seen: Mutex<VecDeque<String>>,
}

impl EventDedup {
    /// Empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True on first sight; false on a duplicate.
    pub async fn observe(&self, id: &str) -> bool {
        let mut guard = self.seen.lock().await;
        if guard.iter().any(|s| s == id) {
            return false;
        }
        if guard.len() == DEDUP_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(id.to_owned());
        true
    }
}

/// Shared state for the webhook handler.
#[derive(Clone)]
pub struct WhatsappCloudEventsState {
    /// App secret used for HMAC body verification.
    pub app_secret: Arc<String>,
    /// Verify token compared to `hub.verify_token` on GET.
    pub verify_token: Arc<String>,
    /// Inbound sender — the host owns the receiver.
    pub inbound_tx: Sender<InboundEvent>,
    /// Channel-type label attached to emitted events.
    pub channel_type: ChannelType,
    /// LRU of seen message ids.
    pub dedup: Arc<EventDedup>,
}

impl WhatsappCloudEventsState {
    /// Construct from constituent parts.
    #[must_use]
    pub fn new(
        app_secret: impl Into<String>,
        verify_token: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        channel_type: ChannelType,
    ) -> Self {
        Self {
            app_secret: Arc::new(app_secret.into()),
            verify_token: Arc::new(verify_token.into()),
            inbound_tx,
            channel_type,
            dedup: Arc::new(EventDedup::new()),
        }
    }
}

/// Query parameters Meta sends on the GET verification call.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyQuery {
    /// Expected to be `"subscribe"`.
    #[serde(rename = "hub.mode", default)]
    pub mode: Option<String>,
    /// Verify token the admin configured on the dashboard.
    #[serde(rename = "hub.verify_token", default)]
    pub verify_token: Option<String>,
    /// Random nonce we must echo back as the body when the token matches.
    #[serde(rename = "hub.challenge", default)]
    pub challenge: Option<String>,
}

/// Build the axum router. GET and POST both target `path`.
pub fn build_events_router(path: &str, state: WhatsappCloudEventsState) -> Router {
    Router::new()
        .route(path, get(handle_verify).post(handle_notification))
        .with_state(state)
}

async fn handle_verify(
    State(state): State<WhatsappCloudEventsState>,
    Query(q): Query<VerifyQuery>,
) -> Response {
    let (Some(mode), Some(token), Some(challenge)) = (q.mode, q.verify_token, q.challenge) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if mode != "subscribe" {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Constant-time compare on the verify token. Meta's docs treat the
    // verify token as a shared secret; a plain `!=` would leak the token
    // one char at a time via response timing.
    {
        use subtle::ConstantTimeEq;
        if !bool::from(
            token
                .as_bytes()
                .ct_eq(state.verify_token.as_bytes()),
        ) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    (StatusCode::OK, challenge).into_response()
}

async fn handle_notification(
    State(state): State<WhatsappCloudEventsState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sig = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok());
    if verify_signature(&state.app_secret, sig, &body).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let envelope: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    let Some(entries) = envelope.get("entry").and_then(Value::as_array) else {
        // Acknowledge unknown / unexpected shapes so Meta stops retrying.
        return StatusCode::OK.into_response();
    };
    for entry in entries {
        let Some(changes) = entry.get("changes").and_then(Value::as_array) else {
            continue;
        };
        for change in changes {
            let Some(value) = change.get("value") else {
                continue;
            };
            process_value(&state, value).await;
        }
    }
    StatusCode::OK.into_response()
}

async fn process_value(state: &WhatsappCloudEventsState, value: &Value) {
    let phone_number_id = value
        .get("metadata")
        .and_then(|m| m.get("phone_number_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let contacts: Vec<&Value> = value
        .get("contacts")
        .and_then(Value::as_array)
        .map(|v| v.iter().collect())
        .unwrap_or_default();

    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        // Could be statuses[]: ignore in v1.
        return;
    };

    for msg in messages {
        if let Some(event) =
            convert_message(state, &phone_number_id, &contacts, msg).await
        {
            if let Err(err) = state.inbound_tx.send(event).await {
                tracing::warn!(error=%err, "whatsapp-cloud inbound channel closed");
            }
        }
    }
}

async fn convert_message(
    state: &WhatsappCloudEventsState,
    phone_number_id: &str,
    contacts: &[&Value],
    msg: &Value,
) -> Option<InboundEvent> {
    let id = msg.get("id").and_then(Value::as_str)?.to_owned();
    if !state.dedup.observe(&id).await {
        return None;
    }
    let from = msg.get("from").and_then(Value::as_str)?.to_owned();
    let msg_type = msg.get("type").and_then(Value::as_str).unwrap_or("");
    let ts = msg
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|s| Utc.timestamp_opt(s, 0).single())
        .unwrap_or_else(Utc::now);

    let content = match msg_type {
        "text" => json!({
            "text": msg.get("text").and_then(|t| t.get("body")).and_then(Value::as_str).unwrap_or("")
        }),
        "image" | "document" | "audio" | "video" => {
            let inner = msg.get(msg_type).cloned().unwrap_or(json!({}));
            let attachment_id = inner.get("id").cloned().unwrap_or(Value::Null);
            let mime = inner.get("mime_type").cloned().unwrap_or(Value::Null);
            let filename = inner.get("filename").cloned().unwrap_or(Value::Null);
            json!({
                "text": "",
                "attachment": {
                    "id": attachment_id,
                    "mime_type": mime,
                    "filename": filename,
                    "kind": msg_type,
                }
            })
        }
        "button" => {
            let btn = msg.get("button").cloned().unwrap_or(json!({}));
            json!({
                "button": btn.get("text").cloned().unwrap_or(Value::Null),
                "payload": btn.get("payload").cloned().unwrap_or(Value::Null),
            })
        }
        "interactive" => {
            let interactive = msg.get("interactive").cloned().unwrap_or(json!({}));
            json!({"interactive": interactive})
        }
        // `reaction` (user-reacted-to-our-message) and any other unrecognised
        // type are acknowledged at the HTTP layer but produce no inbound
        // event in v1.
        _ => return None,
    };

    let platform_id = format!("{phone_number_id}:{from}");
    let sender = lookup_sender(state, contacts, &from);

    // WhatsApp Cloud surfaces "this message replies to <parent>" via
    // `context.message_id`. The parent lives in the same chat, so we route
    // the reply back through the same `platform_id` we just built.
    let reply_to = msg
        .get("context")
        .and_then(|c| c.get("message_id"))
        .and_then(Value::as_str)
        .map(|parent_id| ReplyTo {
            channel_type: state.channel_type.clone(),
            platform_id: platform_id.clone(),
            thread_id: Some(parent_id.to_owned()),
        });

    Some(InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id,
        thread_id: None,
        message: InboundMessage {
            id,
            kind: MessageKind::Chat,
            content,
            timestamp: ts,
            is_mention: None,
            is_group: Some(false),
        },
        reply_to,
        sender: Some(sender),
    })
}

fn lookup_sender(
    state: &WhatsappCloudEventsState,
    contacts: &[&Value],
    wa_id: &str,
) -> SenderIdentity {
    let display_name = contacts
        .iter()
        .find(|c| c.get("wa_id").and_then(Value::as_str) == Some(wa_id))
        .and_then(|c| c.get("profile"))
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: wa_id.to_owned(),
        display_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::compute_signature;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    const SECRET: &str = "app-secret";
    const VERIFY: &str = "v-token";

    fn make_state() -> (WhatsappCloudEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let state = WhatsappCloudEventsState::new(
            SECRET,
            VERIFY,
            tx,
            ChannelType::new("whatsapp-cloud"),
        );
        (state, rx)
    }

    fn signed_post(
        state: &WhatsappCloudEventsState,
        path_str: &str,
        body: &[u8],
    ) -> Request<Body> {
        let sig = compute_signature(&state.app_secret, body);
        Request::builder()
            .method("POST")
            .uri(path_str)
            .header(SIGNATURE_HEADER, sig)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn verify_matching_token_returns_challenge_body() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/wh?hub.mode=subscribe&hub.verify_token=v-token&hub.challenge=NONCE")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"NONCE");
    }

    #[tokio::test]
    async fn verify_mismatched_token_returns_403() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/wh?hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=NONCE")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn verify_mismatched_mode_returns_403() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/wh?hub.mode=other&hub.verify_token=v-token&hub.challenge=NONCE")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn verify_missing_params_returns_400() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/wh?hub.mode=subscribe&hub.verify_token=v-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn verify_no_query_returns_400() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/wh")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_bad_signature_returns_401() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let body = serde_json::to_vec(&json!({"object":"x","entry":[]})).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/wh")
            .header(SIGNATURE_HEADER, format!("sha256={}", "00".repeat(32)))
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_missing_signature_returns_401() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let body = serde_json::to_vec(&json!({"object":"x","entry":[]})).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/wh")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_malformed_body_returns_400() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let body = b"not json".to_vec();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    fn make_envelope(messages: &Value, contacts: &Value, pnid: &str) -> Value {
        json!({
            "object":"whatsapp_business_account",
            "entry":[{
                "id":"WABA",
                "changes":[{
                    "value":{
                        "messaging_product":"whatsapp",
                        "metadata":{"phone_number_id": pnid},
                        "contacts": contacts,
                        "messages": messages
                    },
                    "field":"messages"
                }]
            }]
        })
    }

    #[tokio::test]
    async fn text_message_emits_inbound() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.A",
                "timestamp":"1700000001",
                "type":"text",
                "text":{"body":"hello"}
            }]),
            &json!([{"profile":{"name":"Alice"},"wa_id":"15551234"}]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.channel_type.as_str(), "whatsapp-cloud");
        assert_eq!(evt.platform_id, "PNID:15551234");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.id, "wamid.A");
        assert_eq!(evt.message.content["text"], "hello");
        assert_eq!(evt.message.timestamp.timestamp(), 1_700_000_001);
        assert_eq!(evt.message.is_group, Some(false));
        assert!(evt.message.is_mention.is_none());
        let sender = evt.sender.expect("sender present");
        assert_eq!(sender.identity, "15551234");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
        assert_eq!(sender.channel_type.as_str(), "whatsapp-cloud");
    }

    #[tokio::test]
    async fn image_message_emits_attachment_metadata() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.IMG",
                "timestamp":"1700000002",
                "type":"image",
                "image":{"id":"MEDIA1","mime_type":"image/jpeg"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["text"], "");
        assert_eq!(evt.message.content["attachment"]["id"], "MEDIA1");
        assert_eq!(evt.message.content["attachment"]["mime_type"], "image/jpeg");
        assert_eq!(evt.message.content["attachment"]["kind"], "image");
    }

    #[tokio::test]
    async fn document_message_carries_filename() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.DOC",
                "timestamp":"1700000003",
                "type":"document",
                "document":{
                    "id":"MEDIA2",
                    "mime_type":"application/pdf",
                    "filename":"report.pdf"
                }
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["attachment"]["filename"], "report.pdf");
        assert_eq!(evt.message.content["attachment"]["kind"], "document");
    }

    #[tokio::test]
    async fn audio_message_emits_attachment() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.AUD",
                "timestamp":"1700000004",
                "type":"audio",
                "audio":{"id":"MEDIA3","mime_type":"audio/ogg"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["attachment"]["kind"], "audio");
    }

    #[tokio::test]
    async fn video_message_emits_attachment() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.VID",
                "timestamp":"1700000005",
                "type":"video",
                "video":{"id":"MEDIA4","mime_type":"video/mp4"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["attachment"]["kind"], "video");
    }

    #[tokio::test]
    async fn button_message_emits_button_payload() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.BTN",
                "timestamp":"1700000006",
                "type":"button",
                "button":{"text":"Confirm","payload":"YES"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["button"], "Confirm");
        assert_eq!(evt.message.content["payload"], "YES");
    }

    #[tokio::test]
    async fn interactive_message_emits_inner_object() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.INT",
                "timestamp":"1700000007",
                "type":"interactive",
                "interactive":{"type":"list_reply","list_reply":{"id":"opt1","title":"Option 1"}}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["interactive"]["type"], "list_reply");
        assert_eq!(
            evt.message.content["interactive"]["list_reply"]["id"],
            "opt1"
        );
    }

    #[tokio::test]
    async fn reaction_inbound_acked_no_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.REACT",
                "timestamp":"1700000008",
                "type":"reaction",
                "reaction":{"message_id":"wamid.X","emoji":""}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn statuses_only_payload_acked_no_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = json!({
            "object":"whatsapp_business_account",
            "entry":[{
                "id":"WABA",
                "changes":[{
                    "value":{
                        "messaging_product":"whatsapp",
                        "metadata":{"phone_number_id":"PNID"},
                        "statuses":[{"id":"wamid.S","status":"delivered"}]
                    },
                    "field":"messages"
                }]
            }]
        });
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn duplicate_message_id_suppressed() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.DUPE",
                "timestamp":"1700000009",
                "type":"text",
                "text":{"body":"once"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn missing_id_field_is_silently_skipped() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "timestamp":"1700000010",
                "type":"text",
                "text":{"body":"hi"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn missing_from_field_skipped() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "id":"wamid.NOFROM",
                "timestamp":"1700000011",
                "type":"text",
                "text":{"body":"hi"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_type_skipped() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.UNK",
                "timestamp":"1700000012",
                "type":"location",
                "location":{"latitude":0.0,"longitude":0.0}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn message_without_contacts_has_sender_no_display_name() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.NC",
                "timestamp":"1700000013",
                "type":"text",
                "text":{"body":"x"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        let sender = evt.sender.expect("sender present");
        assert_eq!(sender.identity, "15551234");
        assert!(sender.display_name.is_none());
    }

    #[tokio::test]
    async fn no_entry_array_returns_200_no_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let body = serde_json::to_vec(&json!({"object":"x"})).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn entries_without_changes_acked_no_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let body = serde_json::to_vec(&json!({
            "object":"x",
            "entry":[{"id":"X"}]
        }))
        .unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn entries_with_change_missing_value_acked() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let body = serde_json::to_vec(&json!({
            "object":"x",
            "entry":[{"id":"X","changes":[{"field":"messages"}]}]
        }))
        .unwrap();
        let req = signed_post(&state, "/wh", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn message_with_unparseable_timestamp_falls_back_to_now() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.BADTS",
                "timestamp":"not-a-number",
                "type":"text",
                "text":{"body":"x"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        // Just check that it parses to some recent timestamp.
        let now = Utc::now().timestamp();
        assert!((evt.message.timestamp.timestamp() - now).abs() <= 5);
    }

    #[tokio::test]
    async fn event_dedup_capacity_drops_oldest() {
        let dedup = EventDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("m{i}")).await);
        }
        assert!(!dedup.observe("m0").await);
        // Insert a new id → drops m0.
        assert!(dedup.observe("m9999").await);
        // m0 is admitted again because the ring evicted it.
        assert!(dedup.observe("m0").await);
    }

    #[test]
    fn signature_header_constant() {
        assert_eq!(SIGNATURE_HEADER, "X-Hub-Signature-256");
    }

    #[tokio::test]
    async fn message_with_context_populates_reply_to() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.REPLY",
                "timestamp":"1700000020",
                "type":"text",
                "text":{"body":"yeah, that one"},
                "context":{"message_id":"wamid.PARENT","from":"15559999"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        let rt = evt.reply_to.expect("reply_to populated from context.message_id");
        assert_eq!(rt.channel_type.as_str(), "whatsapp-cloud");
        assert_eq!(rt.platform_id, "PNID:15551234");
        assert_eq!(rt.thread_id.as_deref(), Some("wamid.PARENT"));
    }

    #[tokio::test]
    async fn message_without_context_leaves_reply_to_none() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wh", state.clone());
        let env = make_envelope(
            &json!([{
                "from":"15551234",
                "id":"wamid.PLAIN",
                "timestamp":"1700000021",
                "type":"text",
                "text":{"body":"fresh thought"}
            }]),
            &json!([]),
            "PNID",
        );
        let body = serde_json::to_vec(&env).unwrap();
        let req = signed_post(&state, "/wh", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.reply_to.is_none());
    }

    #[tokio::test]
    async fn state_constructor_populates_fields() {
        let (state, _rx) = make_state();
        assert_eq!(state.app_secret.as_str(), SECRET);
        assert_eq!(state.verify_token.as_str(), VERIFY);
        assert_eq!(state.channel_type.as_str(), "whatsapp-cloud");
    }
}
