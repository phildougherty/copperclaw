//! Axum router for the Webex webhook endpoint.
//!
//! Webex POSTs a JSON envelope of the form:
//!
//! ```json
//! {
//!   "id": "<webhook-event-id>",
//!   "name": "<webhook-friendly-name>",
//!   "resource": "messages" | "memberships" | "attachmentActions" | ...,
//!   "event": "created" | "updated" | "deleted",
//!   "actorId": "<personId>",
//!   "data": { ... }
//! }
//! ```
//!
//! Webhook payloads do **not** contain message text (for security). We fetch
//! the full message body via `GET /messages/{id}` when needed. Adaptive-card
//! action submits arrive as `resource=attachmentActions, event=created` and
//! we enrich with `GET /attachment/actions/{id}`.

use crate::api::{MessageView, WebexApi};
use crate::signature::{SignatureAlgo, verify_signature};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{DateTime, Utc};
use copperclaw_channels_core::AdapterError;
use copperclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};

/// Maximum number of recent webhook event ids to keep for duplicate detection.
pub const DEDUP_CAPACITY: usize = 256;

/// In-memory FIFO ring of webhook ids we have already processed.
#[derive(Debug, Default)]
pub struct EventDedup {
    seen: Mutex<VecDeque<String>>,
}

impl EventDedup {
    /// Empty dedup buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if `id` was not previously observed.
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

/// Shared state for the webhook HTTP handler.
#[derive(Clone)]
pub struct WebexEventsState {
    /// Webhook HMAC secret.
    pub webhook_secret: Arc<Vec<u8>>,
    /// Algorithm used to verify `X-Spark-Signature`.
    pub algo: SignatureAlgo,
    /// Sender into the host's inbound channel.
    pub inbound_tx: Sender<InboundEvent>,
    /// FIFO ring for duplicate suppression.
    pub dedup: Arc<EventDedup>,
    /// Bot's personId, if known. Used both to filter self-loops and to
    /// detect mentions in fetched message bodies.
    pub bot_person_id: Arc<Option<String>>,
    /// Channel-type label attached to emitted events.
    pub channel_type: ChannelType,
    /// REST API client used to fetch full message bodies and action inputs.
    pub api: WebexApi,
}

impl WebexEventsState {
    /// Construct a fresh state object.
    #[must_use]
    pub fn new(
        webhook_secret: Vec<u8>,
        algo: SignatureAlgo,
        inbound_tx: Sender<InboundEvent>,
        bot_person_id: Option<String>,
        channel_type: ChannelType,
        api: WebexApi,
    ) -> Self {
        Self {
            webhook_secret: Arc::new(webhook_secret),
            algo,
            inbound_tx,
            dedup: Arc::new(EventDedup::new()),
            bot_person_id: Arc::new(bot_person_id),
            channel_type,
            api,
        }
    }
}

/// Webex webhook envelope (the parts we need).
#[derive(Debug, Clone, Deserialize)]
pub struct WebexWebhookEnvelope {
    /// Webhook event id. Used for dedup.
    pub id: String,
    /// `messages`, `memberships`, `attachmentActions`, …
    pub resource: String,
    /// `created`, `updated`, `deleted`.
    pub event: String,
    /// Person who caused the event (best-effort).
    #[serde(default, rename = "actorId")]
    pub actor_id: Option<String>,
    /// Payload specific to the resource.
    pub data: Value,
}

/// Build the webhook router. Mounts the handler at `path`.
pub fn build_events_router(path: &str, state: WebexEventsState) -> Router {
    Router::new()
        .route(path, post(handle_webhook))
        .with_state(state)
}

async fn handle_webhook(
    State(state): State<WebexEventsState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sig = headers
        .get("x-spark-signature")
        .and_then(|v| v.to_str().ok());
    if verify_signature(state.algo, &state.webhook_secret, &body, sig).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let envelope: WebexWebhookEnvelope = match serde_json::from_slice(&body) {
        Ok(env) => env,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if !state.dedup.observe(&envelope.id).await {
        // Already handled — Webex will retry on non-2xx, so we ack.
        return StatusCode::OK.into_response();
    }
    match dispatch_envelope(&state, &envelope).await {
        Ok(Some(event)) => {
            if let Err(err) = state.inbound_tx.send(event).await {
                tracing::warn!(error=%err, "webex inbound channel closed");
            }
            StatusCode::OK.into_response()
        }
        Ok(None) => StatusCode::OK.into_response(),
        Err(err) => {
            tracing::warn!(error=%err, "webex webhook dispatch failed");
            StatusCode::OK.into_response()
        }
    }
}

async fn dispatch_envelope(
    state: &WebexEventsState,
    env: &WebexWebhookEnvelope,
) -> Result<Option<InboundEvent>, AdapterError> {
    match (env.resource.as_str(), env.event.as_str()) {
        ("messages", "created") => handle_message_created(state, &env.data).await,
        ("attachmentActions", "created") => handle_attachment_action(state, &env.data).await,
        // Any other resource/event combination is ignored (200, no inbound).
        _ => Ok(None),
    }
}

async fn handle_message_created(
    state: &WebexEventsState,
    data: &Value,
) -> Result<Option<InboundEvent>, AdapterError> {
    let message_id = data
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::BadRequest("messages.created missing data.id".into()))?
        .to_owned();
    let actor_person = data
        .get("personId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let (Some(actor), Some(bot)) = (
        actor_person.as_deref(),
        state.bot_person_id.as_ref().as_deref(),
    ) {
        if actor == bot {
            // The bot's own message — skip to avoid feedback loops.
            return Ok(None);
        }
    }
    let view = state.api.get_message(&message_id).await?;
    // If we now know the author and it matches the bot, drop it.
    if let (Some(person), Some(bot)) = (
        view.person_id.as_deref(),
        state.bot_person_id.as_ref().as_deref(),
    ) {
        if person == bot {
            return Ok(None);
        }
    }
    Ok(Some(build_message_event(state, &view)))
}

async fn handle_attachment_action(
    state: &WebexEventsState,
    data: &Value,
) -> Result<Option<InboundEvent>, AdapterError> {
    let action_id = data
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AdapterError::BadRequest("attachmentActions.created missing data.id".into())
        })?
        .to_owned();
    let action = state.api.get_attachment_action(&action_id).await?;
    if let (Some(person), Some(bot)) = (
        action.person_id.as_deref(),
        state.bot_person_id.as_ref().as_deref(),
    ) {
        if person == bot {
            return Ok(None);
        }
    }
    // We need a roomId for routing; fetch the parent message if the action's
    // own envelope didn't include one.
    let room_id = if let Some(r) = action.room_id.clone() {
        r
    } else {
        state.api.get_message(&action.message_id).await?.room_id
    };
    let sender = action.person_id.as_ref().map(|pid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: pid.clone(),
        display_name: None,
    });
    let inputs = action.inputs.clone().unwrap_or_else(|| json!({}));
    let content = json!({
        "action": action.kind,
        "inputs": inputs,
    });
    Ok(Some(InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: room_id,
        thread_id: Some(action.message_id.clone()),
        message: InboundMessage {
            id: action.id.clone(),
            kind: MessageKind::Chat,
            content,
            timestamp: Utc::now(),
            is_mention: None,
            is_group: None,
        },
        reply_to: None,
        sender,
    }))
}

fn build_message_event(state: &WebexEventsState, view: &MessageView) -> InboundEvent {
    let text = view
        .text
        .clone()
        .or_else(|| view.markdown.clone())
        .unwrap_or_default();
    let content = json!({"text": text});
    let is_group = view
        .room_type
        .as_deref()
        .map(|t| matches!(t, "group" | "team"));
    let is_mention = state.bot_person_id.as_ref().as_deref().map(|bot| {
        view.mentioned_people.iter().any(|p| p == bot) || text.contains(&format!("<@{bot}>"))
    });
    let sender = view.person_id.as_ref().map(|pid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: pid.clone(),
        display_name: view.person_email.clone(),
    });
    let timestamp = parse_iso8601(view.created.as_deref()).unwrap_or_else(Utc::now);
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: view.room_id.clone(),
        thread_id: view.parent_id.clone(),
        message: InboundMessage {
            id: view.id.clone(),
            kind: MessageKind::Chat,
            content,
            timestamp,
            is_mention,
            is_group,
        },
        reply_to: None,
        sender,
    }
}

fn parse_iso8601(s: Option<&str>) -> Option<DateTime<Utc>> {
    let s = s?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use crate::signature::compute_signature;
    use axum::body::Body;
    use axum::http::Request;
    use tokio::sync::mpsc;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &[u8] = b"hook-secret";

    fn make_state(
        api: WebexApi,
        bot: Option<String>,
        algo: SignatureAlgo,
    ) -> (WebexEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let state = WebexEventsState::new(
            SECRET.to_vec(),
            algo,
            tx,
            bot,
            ChannelType::new("webex"),
            api,
        );
        (state, rx)
    }

    fn signed_request(algo: SignatureAlgo, path_str: &str, body: &[u8]) -> Request<Body> {
        let sig = compute_signature(algo, SECRET, body);
        Request::builder()
            .method("POST")
            .uri(path_str)
            .header("x-spark-signature", sig)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn messages_created_fetches_body_and_emits_inbound() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1",
                "roomId":"R1",
                "roomType":"group",
                "personId":"U1",
                "personEmail":"u1@example.com",
                "text":"hello world",
                "created":"2024-01-01T00:00:00.000Z"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK1",
            "resource":"messages",
            "event":"created",
            "actorId":"U1",
            "data":{"id":"M1","personId":"U1","roomId":"R1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "R1");
        assert_eq!(evt.message.id, "M1");
        assert_eq!(evt.message.content["text"], "hello world");
        assert_eq!(evt.message.is_group, Some(true));
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "U1");
        assert_eq!(sender.display_name.as_deref(), Some("u1@example.com"));
    }

    #[tokio::test]
    async fn messages_created_filters_self_via_actor() {
        let server = MockServer::start().await;
        // We do NOT mount a GET /messages mock — the handler must NOT call
        // it because the actor matches the bot id.
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, Some("PBOT".into()), SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_SELF",
            "resource":"messages",
            "event":"created",
            "data":{"id":"M1","personId":"PBOT","roomId":"R1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // No inbound emitted.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn messages_created_filters_self_via_fetched_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"PBOT","text":"loop"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, Some("PBOT".into()), SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        // Actor id is missing from the webhook envelope; only the fetched
        // body reveals the author.
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_LATE",
            "resource":"messages",
            "event":"created",
            "data":{"id":"M1","roomId":"R1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn attachment_actions_created_emits_action_event() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/attachment/actions/A1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"A1",
                "type":"submit",
                "messageId":"M1",
                "personId":"U2",
                "roomId":"R9",
                "inputs":{"choice":"yes"}
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_A",
            "resource":"attachmentActions",
            "event":"created",
            "data":{"id":"A1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "R9");
        assert_eq!(evt.thread_id.as_deref(), Some("M1"));
        assert_eq!(evt.message.content["action"], "submit");
        assert_eq!(evt.message.content["inputs"]["choice"], "yes");
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "U2");
    }

    #[tokio::test]
    async fn attachment_actions_falls_back_to_parent_message_for_room() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/attachment/actions/A2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"A2",
                "type":"submit",
                "messageId":"M2",
                "personId":"U2"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/messages/M2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M2","roomId":"R-FROM-PARENT"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_A2",
            "resource":"attachmentActions",
            "event":"created",
            "data":{"id":"A2"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "R-FROM-PARENT");
    }

    #[tokio::test]
    async fn attachment_actions_skips_when_actor_is_bot() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/attachment/actions/A3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"A3","type":"submit","messageId":"M3","personId":"PBOT","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, Some("PBOT".into()), SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_A3",
            "resource":"attachmentActions",
            "event":"created",
            "data":{"id":"A3"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn messages_updated_is_ignored() {
        let server = MockServer::start().await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_U",
            "resource":"messages",
            "event":"updated",
            "data":{"id":"M1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn messages_deleted_is_ignored() {
        let server = MockServer::start().await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_D",
            "resource":"messages",
            "event":"deleted",
            "data":{"id":"M1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_resource_is_ignored() {
        let server = MockServer::start().await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_X",
            "resource":"memberships",
            "event":"created",
            "data":{}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn duplicate_event_id_is_not_redelivered() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"U1","text":"once"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"DUPE",
            "resource":"messages",
            "event":"created",
            "data":{"id":"M1","personId":"U1","roomId":"R1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let req2 = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req2).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn bad_signature_returns_401() {
        let server = MockServer::start().await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, _rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"X","resource":"messages","event":"created","data":{}
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/webex/webhook")
            .header("x-spark-signature", "0".repeat(40))
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_signature_returns_401() {
        let server = MockServer::start().await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, _rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"X","resource":"messages","event":"created","data":{}
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/webex/webhook")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn sha256_signature_path_works() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"U1","text":"hi"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha256);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_S256",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = rx.recv().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let server = MockServer::start().await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, _rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = b"not json".to_vec();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn message_event_without_text_falls_back_to_markdown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"U1","markdown":"**hello**"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_MD",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["text"], "**hello**");
    }

    #[tokio::test]
    async fn direct_room_is_not_group() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","roomType":"direct","personId":"U1","text":"hi"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_DM",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[tokio::test]
    async fn team_room_is_group() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","roomType":"team","personId":"U1","text":"hi"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_TEAM",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_group, Some(true));
    }

    #[tokio::test]
    async fn mention_detected_via_mentioned_people_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"U1","text":"hi bot",
                "mentionedPeople":["PBOT"]
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, Some("PBOT".into()), SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_MENTION",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn mention_detected_via_text_marker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"U1","text":"hi <@PBOT> there"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, Some("PBOT".into()), SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_TEXT_MENTION",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn no_bot_id_makes_is_mention_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"U1","text":"hi"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_NOBOT",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.message.is_mention.is_none());
    }

    #[tokio::test]
    async fn parent_id_propagates_to_thread_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1","personId":"U1","text":"x","parentId":"PAR9"
            })))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_PARENT",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.thread_id.as_deref(), Some("PAR9"));
    }

    #[tokio::test]
    async fn data_missing_id_yields_no_inbound() {
        let server = MockServer::start().await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_BAD","resource":"messages","event":"created","data":{}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        // We swallow internal errors and ack 200; no inbound emitted.
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fetch_failure_does_not_panic() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let api = WebexApi::new(server.uri(), "tok");
        let (state, mut rx) = make_state(api, None, SignatureAlgo::Sha1);
        let app = build_events_router("/webex/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "id":"WHOOK_500",
            "resource":"messages","event":"created",
            "data":{"id":"M1","personId":"U1"}
        }))
        .unwrap();
        let req = signed_request(state.algo, "/webex/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn event_dedup_capacity_drops_oldest() {
        let dedup = EventDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("e{i}")).await);
        }
        // Re-observe one of the existing ids — false.
        assert!(!dedup.observe("e0").await);
        // Insert one more — drops "e0" from the ring.
        assert!(dedup.observe("e256").await);
        // "e0" can be observed again.
        assert!(dedup.observe("e0").await);
    }

    #[test]
    fn parse_iso8601_round_trips() {
        let dt = parse_iso8601(Some("2024-01-01T00:00:00.000Z")).unwrap();
        assert_eq!(dt.timestamp(), 1_704_067_200);
        assert!(parse_iso8601(None).is_none());
        assert!(parse_iso8601(Some("not a date")).is_none());
    }

    #[test]
    fn dedup_default_is_empty() {
        let _ = EventDedup::default();
    }
}
