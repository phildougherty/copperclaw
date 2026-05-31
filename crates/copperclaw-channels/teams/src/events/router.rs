//! Axum router for the Microsoft Graph change-notification webhook.
//!
//! Two flows are served on the same path:
//!
//! 1. **Validation handshake** — when a subscription is created, Graph sends
//!    a request containing the `validationToken` query parameter. We respond
//!    `200 OK` with `Content-Type: text/plain` and the token as the body.
//!    Validation requests are *not* signed; we always accept them.
//!
//! 2. **Notifications** — Graph POSTs JSON in the shape
//!    `{"value": [{...}, ...]}`. Each entry carries a `clientState` string
//!    matching the secret the subscriber set at create-time; the handler
//!    rejects the request (401) if any entry's `clientState` mismatches.
//!    For each surviving entry the handler fetches the referenced message
//!    via Microsoft Graph, converts the body to plain text, and pushes an
//!    [`InboundEvent`] onto the host-provided mpsc sender.

use crate::api::TeamsApi;
use crate::html::html_to_text;
use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use chrono::{DateTime, Utc};
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, ReplyTo, SenderIdentity,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, mpsc::Sender};

/// Maximum number of recent notification ids kept for duplicate suppression.
pub const DEDUP_CAPACITY: usize = 256;

/// In-memory LRU ring of `(subscriptionId, resourceData.id)` pairs.
#[derive(Debug, Default)]
pub struct NotificationDedup {
    seen: Mutex<VecDeque<String>>,
}

impl NotificationDedup {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true on first sight; false if the id was already in the ring.
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

/// Shared state for the Teams change-notification handler.
#[derive(Clone)]
pub struct TeamsWebhookState {
    /// Shared secret the subscription was created with.
    pub client_state_secret: Arc<String>,
    /// Sender into the host's inbound event queue.
    pub inbound_tx: Sender<InboundEvent>,
    /// Optional bot user id; matching `from.user.id` values are filtered.
    pub bot_user_id: Arc<Option<String>>,
    /// Channel-type label attached to emitted events.
    pub channel_type: ChannelType,
    /// Microsoft Graph client used to fetch full message bodies.
    pub api: TeamsApi,
    /// Dedup ring shared across requests.
    pub dedup: Arc<NotificationDedup>,
}

impl TeamsWebhookState {
    /// Construct the shared state.
    #[must_use]
    pub fn new(
        client_state_secret: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        bot_user_id: Option<String>,
        channel_type: ChannelType,
        api: TeamsApi,
    ) -> Self {
        Self {
            client_state_secret: Arc::new(client_state_secret.into()),
            inbound_tx,
            bot_user_id: Arc::new(bot_user_id),
            channel_type,
            api,
            dedup: Arc::new(NotificationDedup::new()),
        }
    }
}

/// Build the Teams webhook router. Mounts the handler at the given `path`.
///
/// The same path serves both the validation handshake (any method, with a
/// `validationToken` query parameter) and the notifications POST.
pub fn build_webhook_router(path: &str, state: TeamsWebhookState) -> Router {
    Router::new()
        .route(path, any(handle_webhook))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct NotificationEnvelope {
    #[serde(default)]
    value: Vec<NotificationEntry>,
}

#[derive(Debug, Deserialize, Clone)]
struct NotificationEntry {
    #[serde(default, rename = "subscriptionId")]
    subscription_id: Option<String>,
    #[serde(default, rename = "clientState")]
    client_state: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default, rename = "resourceData")]
    resource_data: Option<ResourceData>,
}

#[derive(Debug, Deserialize, Clone)]
struct ResourceData {
    #[serde(default)]
    id: Option<String>,
}

async fn handle_webhook(
    State(state): State<TeamsWebhookState>,
    Query(query): Query<HashMap<String, String>>,
    _headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Validation handshake — Graph sends an unsigned request with
    // `?validationToken=...`. We must echo it back as plain text.
    if let Some(token) = query.get("validationToken").cloned() {
        return validation_response(&token);
    }

    // Notification POST. Parse the envelope; reject 400 on malformed JSON.
    let envelope: NotificationEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(error=%err, "teams webhook received malformed JSON");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Any entry with a missing or mismatched clientState invalidates the
    // whole batch — Microsoft sends them as a unit and any forgery in the
    // batch is grounds for refusal.
    for entry in &envelope.value {
        let provided = entry.client_state.as_deref().unwrap_or("");
        let expected = state.client_state_secret.as_str();
        if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            tracing::warn!("teams webhook clientState mismatch — rejecting");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }

    // For each entry, derive a dedup key, fetch the message, and emit.
    for entry in envelope.value {
        let Some(resource_data) = entry.resource_data.as_ref() else {
            continue;
        };
        let Some(message_id) = resource_data.id.as_deref() else {
            continue;
        };
        let dedup_key = format!(
            "{}:{}",
            entry.subscription_id.as_deref().unwrap_or(""),
            message_id
        );
        if !state.dedup.observe(&dedup_key).await {
            tracing::debug!(key=%dedup_key, "teams duplicate notification suppressed");
            continue;
        }

        let Some(resource) = entry.resource.as_deref() else {
            continue;
        };
        let Some(parsed) = ResourceShape::parse(resource) else {
            tracing::warn!(resource=%resource, "teams webhook received unknown resource shape");
            continue;
        };

        match dispatch_entry(&state, parsed, message_id).await {
            Ok(Some(event)) => {
                if let Err(err) = state.inbound_tx.send(event).await {
                    tracing::warn!(error=%err, "teams inbound channel closed");
                }
            }
            Ok(None) => {
                // Filtered (e.g. our own message) — nothing to emit.
            }
            Err(err) => {
                tracing::warn!(error=%err, "teams failed to fetch message body");
            }
        }
    }

    StatusCode::ACCEPTED.into_response()
}

fn validation_response(token: &str) -> Response {
    let mut resp = (StatusCode::OK, token.to_owned()).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// Parsed resource path for inbound notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResourceShape {
    /// `teams/{teamId}/channels/{channelId}/messages` (optionally `/.../{id}`).
    Channel {
        team_id: String,
        channel_id: String,
    },
    /// `chats/{chatId}/messages` (optionally `/.../{id}`).
    Chat { chat_id: String },
}

impl ResourceShape {
    pub(crate) fn parse(resource: &str) -> Option<Self> {
        let trimmed = resource.trim().trim_start_matches('/');
        let parts: Vec<&str> = trimmed.split('/').filter(|p| !p.is_empty()).collect();
        match parts.as_slice() {
            ["teams", team, "channels", channel, "messages", ..]
                if !team.is_empty() && !channel.is_empty() =>
            {
                Some(Self::Channel {
                    team_id: (*team).to_owned(),
                    channel_id: (*channel).to_owned(),
                })
            }
            ["chats", chat, "messages", ..] if !chat.is_empty() => Some(Self::Chat {
                chat_id: (*chat).to_owned(),
            }),
            _ => None,
        }
    }
}

async fn dispatch_entry(
    state: &TeamsWebhookState,
    resource: ResourceShape,
    message_id: &str,
) -> Result<Option<InboundEvent>, copperclaw_channels_core::AdapterError> {
    match resource {
        ResourceShape::Channel {
            team_id,
            channel_id,
        } => {
            let raw = state
                .api
                .get_channel_message(&team_id, &channel_id, message_id)
                .await?;
            Ok(build_channel_event(state, &team_id, &channel_id, &raw))
        }
        ResourceShape::Chat { chat_id } => {
            let raw = state.api.get_chat_message(&chat_id, message_id).await?;
            // Decide is_group by querying the chat metadata.
            let chat_info = state.api.get_chat(&chat_id).await.ok();
            let chat_type = chat_info.and_then(|i| i.chat_type);
            Ok(build_chat_event(state, &chat_id, chat_type.as_deref(), &raw))
        }
    }
}

fn build_channel_event(
    state: &TeamsWebhookState,
    team_id: &str,
    channel_id: &str,
    raw: &Value,
) -> Option<InboundEvent> {
    if message_is_from_bot(raw, state.bot_user_id.as_ref().as_deref()) {
        return None;
    }
    let platform_id = format!("team/{team_id}/channel/{channel_id}");
    let thread_id = raw
        .get("replyToId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let is_mention = Some(detect_mention(
        raw,
        state.bot_user_id.as_ref().as_deref(),
    ));
    let is_group = Some(true);
    Some(build_event(
        state,
        raw,
        platform_id,
        thread_id,
        is_mention,
        is_group,
    ))
}

fn build_chat_event(
    state: &TeamsWebhookState,
    chat_id: &str,
    chat_type: Option<&str>,
    raw: &Value,
) -> Option<InboundEvent> {
    if message_is_from_bot(raw, state.bot_user_id.as_ref().as_deref()) {
        return None;
    }
    let platform_id = format!("chat/{chat_id}");
    let thread_id = raw
        .get("replyToId")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let is_mention = Some(detect_mention(
        raw,
        state.bot_user_id.as_ref().as_deref(),
    ));
    let is_group = Some(matches!(chat_type, Some(t) if t != "oneOnOne"));
    Some(build_event(
        state,
        raw,
        platform_id,
        thread_id,
        is_mention,
        is_group,
    ))
}

fn build_event(
    state: &TeamsWebhookState,
    raw: &Value,
    platform_id: String,
    thread_id: Option<String>,
    is_mention: Option<bool>,
    is_group: Option<bool>,
) -> InboundEvent {
    // `replyToId` on the Graph payload is the message this reply targets.
    // The historical `thread_id` mirror is kept (callers rely on it to
    // stitch a Teams "reply chain" together); `reply_to` is the cleaner
    // semantic so we surface it explicitly too.
    let reply_to = raw
        .get("replyToId")
        .and_then(Value::as_str)
        .map(|parent| ReplyTo {
            channel_type: state.channel_type.clone(),
            platform_id: platform_id.clone(),
            thread_id: Some(parent.to_owned()),
        });
    let html = raw
        .get("body")
        .and_then(|b| b.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let text = html_to_text(html);
    let content = serde_json::json!({
        "text": text,
        "html": html,
    });
    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let timestamp = parse_created_datetime(raw);
    let sender = raw
        .get("from")
        .and_then(|f| f.get("user"))
        .and_then(|u| {
            let user_id = u.get("id").and_then(Value::as_str)?;
            let display = u
                .get("displayName")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            Some(SenderIdentity {
                channel_type: state.channel_type.clone(),
                identity: user_id.to_owned(),
                display_name: display,
            })
        });
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id,
        thread_id,
        message: InboundMessage {
            id,
            kind: MessageKind::Chat,
            content,
            timestamp,
            is_mention,
            is_group,
        },
        reply_to,
        sender,
    }
}

fn parse_created_datetime(raw: &Value) -> DateTime<Utc> {
    let s = raw
        .get("createdDateTime")
        .and_then(Value::as_str)
        .unwrap_or("");
    DateTime::parse_from_rfc3339(s)
        .map_or_else(|_| Utc::now(), |d| d.with_timezone(&Utc))
}

fn message_is_from_bot(raw: &Value, bot_user_id: Option<&str>) -> bool {
    let Some(bot_id) = bot_user_id else {
        return false;
    };
    let from_id = raw
        .get("from")
        .and_then(|f| f.get("user"))
        .and_then(|u| u.get("id"))
        .and_then(Value::as_str);
    matches!(from_id, Some(id) if id == bot_id)
}

fn detect_mention(raw: &Value, bot_user_id: Option<&str>) -> bool {
    let Some(bot_id) = bot_user_id else {
        return false;
    };
    // Check the `mentions[]` array first — most reliable signal.
    if let Some(mentions) = raw.get("mentions").and_then(Value::as_array) {
        for m in mentions {
            if let Some(mid) = m
                .get("mentioned")
                .and_then(|x| x.get("user"))
                .and_then(|u| u.get("id"))
                .and_then(Value::as_str)
            {
                if mid == bot_id {
                    return true;
                }
            }
        }
    }
    // Fall back to looking at body content for `<at id="...">` referencing
    // the bot via index lookup in mentions[].id.
    if let Some(mentions) = raw.get("mentions").and_then(Value::as_array) {
        for m in mentions {
            let mention_id = m
                .get("mentioned")
                .and_then(|x| x.get("user"))
                .and_then(|u| u.get("id"))
                .and_then(Value::as_str);
            if mention_id != Some(bot_id) {
                continue;
            }
            let idx = m.get("id").and_then(Value::as_i64);
            if let Some(idx) = idx {
                let needle = format!("<at id=\"{idx}\">");
                if let Some(html) = raw
                    .get("body")
                    .and_then(|b| b.get("content"))
                    .and_then(Value::as_str)
                {
                    if html.contains(&needle) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_state(
        server: &MockServer,
        bot: Option<String>,
    ) -> (TeamsWebhookState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let api = TeamsApi::new(server.uri(), "tok");
        let state = TeamsWebhookState::new(
            "shared-secret",
            tx,
            bot,
            ChannelType::new("teams"),
            api,
        );
        (state, rx)
    }

    fn json_request(uri: &str, body: &[u8]) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn validation_handshake_returns_token_as_plain_text() {
        let server = MockServer::start().await;
        let (state, _rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let req = Request::builder()
            .method("POST")
            .uri("/teams/webhook?validationToken=hello-world")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        assert!(ct.starts_with("text/plain"), "content-type was {ct}");
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"hello-world");
    }

    #[tokio::test]
    async fn validation_handshake_works_on_get_method_too() {
        let server = MockServer::start().await;
        let (state, _rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let req = Request::builder()
            .method("GET")
            .uri("/teams/webhook?validationToken=abc")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"abc");
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let server = MockServer::start().await;
        let (state, _rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let req = json_request("/teams/webhook", b"not-json");
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mismatched_client_state_returns_401() {
        let server = MockServer::start().await;
        let (state, _rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "wrong",
                "resource": "teams/T1/channels/C1/messages/M1",
                "resourceData": {"id": "M1"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_client_state_returns_401() {
        let server = MockServer::start().await;
        let (state, _rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "resource": "teams/T1/channels/C1/messages/M1",
                "resourceData": {"id": "M1"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_value_array_returns_202() {
        let server = MockServer::start().await;
        let (state, _rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({"value": []})).unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn channel_notification_fetches_and_emits_event() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "M1",
                "createdDateTime": "2026-05-20T12:34:56Z",
                "body": {"contentType":"html","content":"<p>hello world</p>"},
                "from": {"user": {"id": "U-USER", "displayName":"Alice"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, Some("U-BOT".into()));
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/M1",
                "resourceData": {"id": "M1"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.channel_type.as_str(), "teams");
        assert_eq!(evt.platform_id, "team/T1/channel/C1");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.id, "M1");
        assert_eq!(evt.message.content["text"], "hello world");
        assert_eq!(evt.message.content["html"], "<p>hello world</p>");
        assert_eq!(evt.message.is_group, Some(true));
        assert_eq!(evt.message.is_mention, Some(false));
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "U-USER");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
        assert_eq!(evt.message.timestamp.timestamp(), 1_779_280_496);
    }

    #[tokio::test]
    async fn channel_notification_with_reply_to_sets_thread_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/M2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "M2",
                "replyToId": "PARENT-1",
                "body": {"contentType":"html","content":"reply"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/M2",
                "resourceData": {"id": "M2"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.thread_id.as_deref(), Some("PARENT-1"));
    }

    #[tokio::test]
    async fn chat_notification_oneonone_is_not_group() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT1/messages/M3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "M3",
                "body": {"contentType":"html","content":"dm"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"chatType": "oneOnOne"})),
            )
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "chats/CHAT1/messages/M3",
                "resourceData": {"id": "M3"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "chat/CHAT1");
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[tokio::test]
    async fn chat_notification_group_is_group_true() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT2/messages/M4"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "M4",
                "body": {"contentType":"html","content":"hey"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"chatType": "group"})),
            )
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "chats/CHAT2/messages/M4",
                "resourceData": {"id": "M4"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_group, Some(true));
    }

    #[tokio::test]
    async fn chat_notification_when_get_chat_fails_defaults_to_not_group() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT3/messages/M5"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "M5",
                "body": {"contentType":"html","content":"hi"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT3"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "chats/CHAT3/messages/M5",
                "resourceData": {"id": "M5"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[tokio::test]
    async fn message_from_bot_is_filtered() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/M6"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "M6",
                "body": {"contentType":"html","content":"self"},
                "from": {"user": {"id": "BOT"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, Some("BOT".into()));
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/M6",
                "resourceData": {"id": "M6"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        // No event delivered.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn duplicate_notification_id_suppressed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/M7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "M7",
                "body": {"contentType":"html","content":"once"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let payload = json!({
            "value": [{
                "subscriptionId": "S-DUPE",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/M7",
                "resourceData": {"id": "M7"}
            }]
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        // Only one event was delivered.
        let _ = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn entry_missing_resource_data_is_skipped() {
        let server = MockServer::start().await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/M9"
                // no resourceData
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn entry_with_unknown_resource_shape_is_skipped() {
        let server = MockServer::start().await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "users/U1/messages",
                "resourceData": {"id":"M9"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fetch_failure_does_not_break_handler() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MISS"))
            .respond_with(ResponseTemplate::new(500).set_body_string("err"))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MISS",
                "resourceData": {"id":"MISS"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn mention_via_mentions_array_marks_is_mention_true() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MM"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "MM",
                "body": {"contentType":"html","content":"<at id=\"0\">Bot</at> hi"},
                "mentions": [{
                    "id": 0,
                    "mentioned": {"user": {"id": "BOT"}}
                }],
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, Some("BOT".into()));
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MM",
                "resourceData": {"id":"MM"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn mention_without_bot_id_defaults_to_false() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MN"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "MN",
                "body": {"contentType":"html","content":"<at id=\"0\">Alice</at>"},
                "mentions": [{
                    "id": 0,
                    "mentioned": {"user": {"id": "U-OTHER"}}
                }],
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, Some("BOT".into()));
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MN",
                "resourceData": {"id":"MN"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(false));
    }

    #[tokio::test]
    async fn missing_from_yields_no_sender() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MS"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "MS",
                "body": {"contentType":"html","content":"ghost"}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MS",
                "resourceData": {"id":"MS"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.sender.is_none());
    }

    #[tokio::test]
    async fn bad_created_datetime_falls_back_to_now() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "MT",
                "createdDateTime": "not-a-date",
                "body": {"contentType":"html","content":"hi"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MT",
                "resourceData": {"id":"MT"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        let before = Utc::now().timestamp() - 10;
        assert!(evt.message.timestamp.timestamp() >= before);
    }

    #[test]
    fn resource_shape_parses_channel() {
        let r = ResourceShape::parse("teams/T1/channels/C1/messages/M1").unwrap();
        assert_eq!(
            r,
            ResourceShape::Channel {
                team_id: "T1".into(),
                channel_id: "C1".into()
            }
        );
    }

    #[test]
    fn resource_shape_parses_channel_without_message_id() {
        let r = ResourceShape::parse("teams/T1/channels/C1/messages").unwrap();
        assert_eq!(
            r,
            ResourceShape::Channel {
                team_id: "T1".into(),
                channel_id: "C1".into()
            }
        );
    }

    #[test]
    fn resource_shape_parses_chat() {
        let r = ResourceShape::parse("chats/CHAT1/messages/M1").unwrap();
        assert_eq!(
            r,
            ResourceShape::Chat {
                chat_id: "CHAT1".into()
            }
        );
    }

    #[test]
    fn resource_shape_handles_leading_slash() {
        let r = ResourceShape::parse("/teams/T1/channels/C1/messages/M1").unwrap();
        assert!(matches!(r, ResourceShape::Channel { .. }));
    }

    #[test]
    fn resource_shape_rejects_unknown() {
        assert!(ResourceShape::parse("users/U1/messages/M1").is_none());
        assert!(ResourceShape::parse("garbage").is_none());
        assert!(ResourceShape::parse("").is_none());
    }

    #[tokio::test]
    async fn dedup_lru_drops_oldest_when_full() {
        let dedup = NotificationDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("k{i}")).await);
        }
        // Re-observe — false (still in ring).
        assert!(!dedup.observe("k0").await);
        // Insert a new id — drops "k0".
        assert!(dedup.observe("k-new").await);
        // Now "k0" can be observed again as fresh.
        assert!(dedup.observe("k0").await);
    }

    #[test]
    fn constant_time_eq_handles_length_mismatch() {
        assert!(!constant_time_eq(&[1, 2, 3], &[1, 2]));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
    }

    #[tokio::test]
    async fn notification_routed_to_correct_path_with_custom_mount() {
        // Mount under a non-default path to exercise the route binding.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MX"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "MX",
                "body": {"contentType":"html","content":"x"},
                "from": {"user": {"id": "U"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/some/other/path", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MX",
                "resourceData": {"id":"MX"}
            }]
        }))
        .unwrap();
        let req = json_request("/some/other/path", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.id, "MX");
    }

    #[tokio::test]
    async fn reply_to_id_populates_reply_to_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MR"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "MR",
                "replyToId": "PARENT-77",
                "body": {"contentType":"html","content":"+1"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MR",
                "resourceData": {"id":"MR"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        let rt = evt.reply_to.expect("reply_to populated from replyToId");
        assert_eq!(rt.channel_type.as_str(), "teams");
        assert_eq!(rt.platform_id, "team/T1/channel/C1");
        assert_eq!(rt.thread_id.as_deref(), Some("PARENT-77"));
    }

    #[tokio::test]
    async fn message_without_reply_to_id_leaves_reply_to_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MR2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "MR2",
                "body": {"contentType":"html","content":"fresh"},
                "from": {"user": {"id": "U-USER"}}
            })))
            .mount(&server)
            .await;
        let (state, mut rx) = make_state(&server, None);
        let app = build_webhook_router("/teams/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "value": [{
                "subscriptionId": "S1",
                "clientState": "shared-secret",
                "resource": "teams/T1/channels/C1/messages/MR2",
                "resourceData": {"id":"MR2"}
            }]
        }))
        .unwrap();
        let req = json_request("/teams/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.reply_to.is_none());
    }

    #[tokio::test]
    async fn state_constructor_seeds_fields() {
        let server = MockServer::start().await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let api = TeamsApi::new(server.uri(), "tok");
        let s = TeamsWebhookState::new(
            "secret",
            tx,
            Some("BOT".into()),
            ChannelType::new("teams"),
            api,
        );
        assert_eq!(s.client_state_secret.as_str(), "secret");
        assert_eq!(s.channel_type.as_str(), "teams");
        assert_eq!(s.bot_user_id.as_ref().as_deref(), Some("BOT"));
    }
}
