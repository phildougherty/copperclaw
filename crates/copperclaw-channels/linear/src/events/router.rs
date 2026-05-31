//! Axum router for the Linear webhook.

use crate::signature::verify_signature;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{DateTime, Utc};
use copperclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};

/// Maximum number of recent `Linear-Delivery` ids retained for duplicate
/// suppression. Linear retries deliveries on non-200 responses; we ack 200
/// for re-deliveries seen here.
pub const DEDUP_CAPACITY: usize = 256;

/// In-memory LRU-ish ring of `Linear-Delivery` ids seen so we can suppress
/// retries. The deque is ordered insert-first; on overflow the oldest entry
/// is dropped.
#[derive(Debug, Default)]
pub struct EventDedup {
    seen: Mutex<VecDeque<String>>,
}

impl EventDedup {
    /// Construct an empty dedup ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` on first sight; `false` if the id was already in the
    /// ring. Inserts the id when first seen.
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

/// Shared state for the Linear webhook HTTP handler.
#[derive(Clone)]
pub struct LinearEventsState {
    /// Webhook signing secret used to validate `Linear-Signature`.
    pub webhook_secret: Arc<String>,
    /// Sender to push assembled `InboundEvent`s onto. The host owns the rx.
    pub inbound_tx: Sender<InboundEvent>,
    /// LRU of `Linear-Delivery` ids already processed.
    pub dedup: Arc<EventDedup>,
    /// Bot user id used to mark comments that explicitly @-mention the bot.
    pub bot_user_id: Arc<Option<String>>,
    /// Bot username used to mark comments whose body contains `@<username>`.
    pub bot_username: Arc<Option<String>>,
    /// Channel-type label attached to emitted events.
    pub channel_type: ChannelType,
}

impl LinearEventsState {
    /// Construct fresh state with an empty dedup ring.
    #[must_use]
    pub fn new(
        webhook_secret: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        bot_user_id: Option<String>,
        bot_username: Option<String>,
        channel_type: ChannelType,
    ) -> Self {
        Self {
            webhook_secret: Arc::new(webhook_secret.into()),
            inbound_tx,
            dedup: Arc::new(EventDedup::new()),
            bot_user_id: Arc::new(bot_user_id),
            bot_username: Arc::new(bot_username),
            channel_type,
        }
    }
}

/// Build the Linear webhook router. Mounts the handler at `path`.
pub fn build_events_router(path: &str, state: LinearEventsState) -> Router {
    Router::new()
        .route(path, post(handle_webhook))
        .with_state(state)
}

async fn handle_webhook(
    State(state): State<LinearEventsState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let sig = headers
        .get("linear-signature")
        .and_then(|v| v.to_str().ok());
    if verify_signature(&state.webhook_secret, sig, &body).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let delivery_id = headers
        .get("linear-delivery")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if let Some(id) = &delivery_id {
        if !state.dedup.observe(id).await {
            return StatusCode::OK.into_response();
        }
    }
    let Some(event) = parse_event(&state, &payload) else {
        // Unknown / ignored event — still ack so Linear stops retrying.
        return StatusCode::OK.into_response();
    };
    if let Err(err) = state.inbound_tx.send(event).await {
        tracing::warn!(error=%err, "linear inbound channel closed");
    }
    StatusCode::OK.into_response()
}

/// Build an `InboundEvent` from a Linear webhook payload, or `None` if the
/// event is not one we handle.
pub(crate) fn parse_event(state: &LinearEventsState, payload: &Value) -> Option<InboundEvent> {
    let event_type = payload.get("type").and_then(Value::as_str)?;
    let action = payload.get("action").and_then(Value::as_str)?;
    let data = payload.get("data")?;
    if action != "create" {
        return None;
    }
    match event_type {
        "Comment" => Some(comment_to_event(state, data)),
        "Issue" => Some(issue_to_event(state, data)),
        _ => None,
    }
}

fn comment_to_event(state: &LinearEventsState, data: &Value) -> InboundEvent {
    let body = data
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    // platform_id is the parent issue's UUID. Prefer the embedded
    // `data.issue.id`; fall back to `data.issueId` which Linear also emits.
    let platform_id = data
        .get("issue")
        .and_then(|i| i.get("id"))
        .and_then(Value::as_str)
        .or_else(|| data.get("issueId").and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();
    let thread_id = data
        .get("parentId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let user = data.get("user");
    let sender = user.and_then(|u| {
        u.get("id").and_then(Value::as_str).map(|uid| SenderIdentity {
            channel_type: state.channel_type.clone(),
            identity: uid.to_owned(),
            display_name: u
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    });
    let timestamp = parse_iso8601(data.get("createdAt").and_then(Value::as_str));
    let content = serde_json::json!({"text": body});
    let is_mention = Some(body_mentions_bot(
        &body,
        state.bot_user_id.as_ref().as_deref(),
        state.bot_username.as_ref().as_deref(),
    ));
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
            is_group: Some(true),
        },
        reply_to: None,
        sender,
    }
}

fn issue_to_event(state: &LinearEventsState, data: &Value) -> InboundEvent {
    let title = data
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let description = data
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let text = if description.is_empty() {
        format!("{title}\n\n")
    } else {
        format!("{title}\n\n{description}")
    };
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let sender = data.get("creator").and_then(|u| {
        u.get("id").and_then(Value::as_str).map(|uid| SenderIdentity {
            channel_type: state.channel_type.clone(),
            identity: uid.to_owned(),
            display_name: u
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    });
    let timestamp = parse_iso8601(data.get("createdAt").and_then(Value::as_str));
    let content = serde_json::json!({"text": text});
    let is_mention = Some(body_mentions_bot(
        &text,
        state.bot_user_id.as_ref().as_deref(),
        state.bot_username.as_ref().as_deref(),
    ));
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: id.clone(),
        thread_id: None,
        message: InboundMessage {
            id,
            kind: MessageKind::Chat,
            content,
            timestamp,
            is_mention,
            is_group: Some(true),
        },
        reply_to: None,
        sender,
    }
}

/// Decide whether a Linear comment body @-mentions the bot.
///
/// Returns `true` if either:
/// - the body contains `@<bot_username>` (when configured), or
/// - the body contains the configured `bot_user_id` (Linear surfaces
///   `@`-mentions inside a comment body as the user's UUID).
pub(crate) fn body_mentions_bot(
    body: &str,
    bot_user_id: Option<&str>,
    bot_username: Option<&str>,
) -> bool {
    if let Some(name) = bot_username {
        let needle = format!("@{name}");
        if body.contains(&needle) {
            return true;
        }
    }
    if let Some(uid) = bot_user_id {
        if body.contains(uid) {
            return true;
        }
    }
    false
}

/// Parse a Linear ISO8601 timestamp string. Falls back to `Utc::now()` if
/// the input is missing or unparseable.
pub(crate) fn parse_iso8601(value: Option<&str>) -> DateTime<Utc> {
    if let Some(s) = value {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return dt.with_timezone(&Utc);
        }
    }
    Utc::now()
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

    const SECRET: &str = "linear-test-secret";

    fn make_state(
        bot_user_id: Option<String>,
        bot_username: Option<String>,
    ) -> (LinearEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let s = LinearEventsState::new(
            SECRET,
            tx,
            bot_user_id,
            bot_username,
            ChannelType::new("linear"),
        );
        (s, rx)
    }

    fn signed_request(_state: &LinearEventsState, path: &str, body: &[u8]) -> Request<Body> {
        let sig = compute_signature(SECRET, body);
        Request::builder()
            .method("POST")
            .uri(path)
            .header("linear-signature", sig)
            .header("linear-delivery", "delivery-1")
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    fn signed_request_with_delivery(path: &str, body: &[u8], delivery: &str) -> Request<Body> {
        let sig = compute_signature(SECRET, body);
        Request::builder()
            .method("POST")
            .uri(path)
            .header("linear-signature", sig)
            .header("linear-delivery", delivery)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn comment_create_produces_inbound_event() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment",
            "action":"create",
            "data": {
                "id":"comment-1",
                "body":"hello world",
                "createdAt":"2026-01-02T03:04:05Z",
                "issue": {"id":"issue-uuid-1"},
                "user": {"id":"user-1","name":"Alice"}
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.channel_type.as_str(), "linear");
        assert_eq!(evt.platform_id, "issue-uuid-1");
        assert_eq!(evt.message.id, "comment-1");
        assert_eq!(evt.message.content["text"], "hello world");
        assert!(matches!(evt.message.kind, MessageKind::Chat));
        assert_eq!(evt.message.is_group, Some(true));
        assert_eq!(evt.message.is_mention, Some(false));
        assert_eq!(evt.message.timestamp.to_rfc3339(), "2026-01-02T03:04:05+00:00");
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "user-1");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn comment_create_falls_back_to_issue_id_field() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment",
            "action":"create",
            "data": {
                "id":"comment-2",
                "body":"flat issueId",
                "issueId":"issue-uuid-flat",
                "user": {"id":"user-2"}
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "issue-uuid-flat");
    }

    #[tokio::test]
    async fn comment_create_thread_id_uses_parent_id() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment",
            "action":"create",
            "data": {
                "id":"comment-3",
                "body":"threaded",
                "parentId":"parent-comment-uuid",
                "issue":{"id":"issue-uuid-2"}
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.thread_id.as_deref(), Some("parent-comment-uuid"));
    }

    #[tokio::test]
    async fn issue_create_produces_title_plus_description_event() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Issue",
            "action":"create",
            "data": {
                "id":"issue-uuid-99",
                "title":"Hello",
                "description":"a long description",
                "creator":{"id":"user-9","name":"Creator"}
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "issue-uuid-99");
        assert_eq!(evt.message.id, "issue-uuid-99");
        assert_eq!(
            evt.message.content["text"],
            "Hello\n\na long description"
        );
        assert!(evt.thread_id.is_none());
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "user-9");
    }

    #[tokio::test]
    async fn issue_create_without_description_uses_title_only() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Issue",
            "action":"create",
            "data": {
                "id":"issue-uuid-100",
                "title":"Bare title"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["text"], "Bare title\n\n");
    }

    #[tokio::test]
    async fn reaction_create_is_ignored_with_200() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Reaction",
            "action":"create",
            "data": {"id":"r-1","emoji":"thumbsup"}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(timeout.is_err());
    }

    #[tokio::test]
    async fn unknown_event_type_is_acked_without_emit() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Project",
            "action":"create",
            "data": {"id":"p-1"}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(timeout.is_err());
    }

    #[tokio::test]
    async fn update_action_is_ignored() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment",
            "action":"update",
            "data": {"id":"c","body":"edited","issue":{"id":"i"}}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(timeout.is_err());
    }

    #[tokio::test]
    async fn duplicate_delivery_id_is_suppressed() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment",
            "action":"create",
            "data": {"id":"c","body":"once","issue":{"id":"i"}}
        }))
        .unwrap();
        let req = signed_request_with_delivery("/linear/webhook", &body, "DUPE-1");
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Resend the same delivery id.
        let req = signed_request_with_delivery("/linear/webhook", &body, "DUPE-1");
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Only one event delivered.
        let _first = rx.recv().await.unwrap();
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(timeout.is_err());
    }

    #[tokio::test]
    async fn rejects_bad_signature_with_401() {
        let (state, _rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({"type":"Comment","action":"create","data":{}}))
            .unwrap();
        let bad_sig = hex::encode([0u8; 32]);
        let req = Request::builder()
            .method("POST")
            .uri("/linear/webhook")
            .header("linear-signature", bad_sig)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_missing_signature_header_with_401() {
        let (state, _rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({"type":"Comment","action":"create","data":{}}))
            .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/linear/webhook")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let (state, _rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = b"not json".to_vec();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn payload_missing_type_is_ignored() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body =
            serde_json::to_vec(&json!({"action":"create","data":{"id":"c"}})).unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(timeout.is_err());
    }

    #[tokio::test]
    async fn payload_missing_action_is_ignored() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body =
            serde_json::to_vec(&json!({"type":"Comment","data":{"id":"c"}})).unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(timeout.is_err());
    }

    #[tokio::test]
    async fn payload_missing_data_is_ignored() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({"type":"Comment","action":"create"})).unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(timeout.is_err());
    }

    #[tokio::test]
    async fn missing_user_yields_no_sender() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment","action":"create",
            "data":{"id":"c","body":"x","issue":{"id":"i"}}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.sender.is_none());
    }

    #[tokio::test]
    async fn comment_with_bot_username_mention_marks_true() {
        let (state, mut rx) = make_state(None, Some("copperclaw".into()));
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment","action":"create",
            "data":{"id":"c","body":"hey @copperclaw look","issue":{"id":"i"}}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn comment_with_bot_user_id_mention_marks_true() {
        let (state, mut rx) = make_state(Some("user-bot-uuid".into()), None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment","action":"create",
            "data":{"id":"c","body":"@user-bot-uuid help","issue":{"id":"i"}}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn comment_without_mention_marks_false() {
        let (state, mut rx) = make_state(None, Some("copperclaw".into()));
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment","action":"create",
            "data":{"id":"c","body":"no mention here","issue":{"id":"i"}}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(false));
    }

    #[tokio::test]
    async fn comment_without_body_has_empty_text() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment","action":"create",
            "data":{"id":"c","issue":{"id":"i"}}
        }))
        .unwrap();
        let req = signed_request(&state, "/linear/webhook", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["text"], "");
    }

    #[tokio::test]
    async fn comment_with_no_delivery_header_still_processes() {
        let (state, mut rx) = make_state(None, None);
        let app = build_events_router("/linear/webhook", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"Comment","action":"create",
            "data":{"id":"c","body":"x","issue":{"id":"i"}}
        }))
        .unwrap();
        let sig = compute_signature(SECRET, &body);
        let req = Request::builder()
            .method("POST")
            .uri("/linear/webhook")
            .header("linear-signature", sig)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = rx.recv().await.unwrap();
    }

    #[tokio::test]
    async fn event_dedup_capacity_drops_oldest() {
        let dedup = EventDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("d{i}")).await);
        }
        // Re-observe → false.
        assert!(!dedup.observe("d0").await);
        // Inserting a new id pushes the oldest out.
        assert!(dedup.observe("dN").await);
        // Original oldest "d0" was popped above; it's accepted again.
        assert!(dedup.observe("d0").await);
    }

    #[tokio::test]
    async fn event_dedup_observe_returns_false_on_repeat() {
        let dedup = EventDedup::new();
        assert!(dedup.observe("a").await);
        assert!(!dedup.observe("a").await);
        assert!(!dedup.observe("a").await);
    }

    #[test]
    fn body_mentions_bot_with_username() {
        assert!(body_mentions_bot("hi @bob", None, Some("bob")));
        assert!(!body_mentions_bot("hi bob", None, Some("bob")));
    }

    #[test]
    fn body_mentions_bot_with_user_id() {
        assert!(body_mentions_bot("uuid here user-x", Some("user-x"), None));
        assert!(!body_mentions_bot("nope", Some("user-x"), None));
    }

    #[test]
    fn body_mentions_bot_with_both() {
        assert!(body_mentions_bot("@bob hi", Some("u"), Some("bob")));
        assert!(body_mentions_bot("uuid u in body", Some("u"), Some("bob")));
        assert!(!body_mentions_bot("nothing", Some("u"), Some("bob")));
    }

    #[test]
    fn body_mentions_bot_with_neither_returns_false() {
        assert!(!body_mentions_bot("whatever", None, None));
    }

    #[test]
    fn parse_iso8601_handles_valid_input() {
        let dt = parse_iso8601(Some("2026-01-01T12:00:00Z"));
        assert_eq!(dt.to_rfc3339(), "2026-01-01T12:00:00+00:00");
    }

    #[test]
    fn parse_iso8601_bad_input_falls_back_to_now() {
        let before = Utc::now().timestamp() - 5;
        let dt = parse_iso8601(Some("not a timestamp"));
        assert!(dt.timestamp() >= before);
    }

    #[test]
    fn parse_iso8601_none_falls_back_to_now() {
        let before = Utc::now().timestamp() - 5;
        let dt = parse_iso8601(None);
        assert!(dt.timestamp() >= before);
    }

    #[test]
    fn linear_events_state_is_clone() {
        let (s, _rx) = make_state(Some("u".into()), Some("n".into()));
        let _ = s.clone();
    }

    #[test]
    fn event_dedup_debug_format_present() {
        let d = EventDedup::new();
        let s = format!("{d:?}");
        assert!(s.contains("EventDedup"));
    }
}
