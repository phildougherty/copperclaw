//! Axum router for the Google Chat HTTP push webhook.

use crate::events::types::{
    GchatEvent, GchatEventEnvelope, GchatMessage, GchatSpace, GchatUser,
};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use chrono::Utc;
use copperclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{mpsc::Sender, Mutex};

/// Maximum number of recent `message.name`s to keep for duplicate suppression.
pub const DEDUP_CAPACITY: usize = 256;

/// In-memory LRU ring of seen message resource names.
#[derive(Debug, Default)]
pub struct EventDedup {
    seen: Mutex<VecDeque<String>>,
}

impl EventDedup {
    /// Empty deduper.
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

/// Shared state for the Google Chat events HTTP handler.
#[derive(Clone)]
pub struct GchatEventsState {
    /// Shared secret the operator configured. The handler requires every
    /// request carry `?token=<client_token>` and matches it constant-time.
    pub client_token: Arc<String>,
    /// Channel for emitting inbound events.
    pub inbound_tx: Sender<InboundEvent>,
    /// Deduper to suppress retried push events.
    pub dedup: Arc<EventDedup>,
    /// Bot user resource id (e.g. `users/12345`). When set, messages from
    /// that user are dropped (defensive in case Google ever delivers an
    /// event without the BOT type marker).
    pub bot_user_id: Arc<Option<String>>,
    /// Channel-type label stamped onto emitted events. Lives in state so
    /// tests can override.
    pub channel_type: ChannelType,
}

impl GchatEventsState {
    /// Build state with a fresh deduper.
    #[must_use]
    pub fn new(
        client_token: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        bot_user_id: Option<String>,
        channel_type: ChannelType,
    ) -> Self {
        Self {
            client_token: Arc::new(client_token.into()),
            inbound_tx,
            dedup: Arc::new(EventDedup::new()),
            bot_user_id: Arc::new(bot_user_id),
            channel_type,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: Option<String>,
}

/// Build the Google Chat events router. Mounts the handler at the given
/// `path`.
pub fn build_events_router(path: &str, state: GchatEventsState) -> Router {
    Router::new()
        .route(path, post(handle_events))
        .with_state(state)
}

async fn handle_events(
    State(state): State<GchatEventsState>,
    Query(q): Query<TokenQuery>,
    body: Bytes,
) -> Response {
    if !token_matches(&state.client_token, q.token.as_deref()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let envelope: GchatEventEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    match envelope.event {
        GchatEvent::Message => {
            if let Some(evt) = convert_message_event(&state, &envelope, false).await {
                if let Err(err) = state.inbound_tx.send(evt).await {
                    tracing::warn!(error=%err, "gchat inbound channel closed");
                }
            }
            StatusCode::OK.into_response()
        }
        GchatEvent::CardClicked => {
            if let Some(evt) = convert_message_event(&state, &envelope, true).await {
                if let Err(err) = state.inbound_tx.send(evt).await {
                    tracing::warn!(error=%err, "gchat inbound channel closed");
                }
            }
            StatusCode::OK.into_response()
        }
        GchatEvent::AddedToSpace
        | GchatEvent::RemovedFromSpace
        | GchatEvent::Other => StatusCode::OK.into_response(),
    }
}

/// Constant-time comparison between configured and supplied tokens.
fn token_matches(expected: &str, supplied: Option<&str>) -> bool {
    use subtle::ConstantTimeEq;
    let Some(supplied) = supplied else {
        return false;
    };
    let a = expected.as_bytes();
    let b = supplied.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Convert a `MESSAGE` or `CARD_CLICKED` envelope into an `InboundEvent`.
///
/// Returns `None` when the envelope should be dropped (bot author, missing
/// message, or duplicate).
async fn convert_message_event(
    state: &GchatEventsState,
    env: &GchatEventEnvelope,
    is_card_click: bool,
) -> Option<InboundEvent> {
    let user = env.user.as_ref()?;
    if user.is_bot() {
        return None;
    }
    if let Some(bot) = state.bot_user_id.as_ref() {
        if &user.name == bot {
            return None;
        }
    }
    let message = env.message.as_ref()?;
    // Dedup on the full message resource name.
    if !state.dedup.observe(&message.name).await {
        return None;
    }
    Some(build_inbound(state, &env.space, user, message, is_card_click))
}

fn build_inbound(
    state: &GchatEventsState,
    space: &GchatSpace,
    user: &GchatUser,
    message: &GchatMessage,
    is_card_click: bool,
) -> InboundEvent {
    let content = if is_card_click {
        json!({
            "action": message.action.clone().unwrap_or(Value::Null),
            "parameters": message.parameters.clone().unwrap_or(Value::Null),
        })
    } else {
        json!({ "text": message.text.clone().unwrap_or_default() })
    };
    let is_group = Some(space.is_room());
    let is_mention = Some(message.was_mentioned());
    let thread_id = message.thread.as_ref().map(|t| t.name.clone());
    let sender = Some(SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: user.name.clone(),
        display_name: user.display_name.clone(),
    });
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: space.name.clone(),
        thread_id,
        message: InboundMessage {
            id: message.name.clone(),
            kind: MessageKind::Chat,
            content,
            timestamp: Utc::now(),
            is_mention,
            is_group,
        },
        reply_to: None,
        sender,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    const TOKEN: &str = "shared-test-secret";
    const WEBHOOK_PATH: &str = "/gchat/webhook";

    fn make_state(
        bot: Option<String>,
    ) -> (GchatEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let s = GchatEventsState::new(TOKEN, tx, bot, ChannelType::new("gchat"));
        (s, rx)
    }

    fn post_with_token(path_and_query: &str, body: Vec<u8>) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path_and_query)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    /// Assert that the inbound channel does NOT receive a real event within
    /// a short window. `recv()` will return `None` when the sender end is
    /// dropped (e.g. when the Router is consumed by `oneshot`), which we
    /// also treat as "no inbound was emitted".
    async fn assert_no_inbound(rx: &mut mpsc::Receiver<InboundEvent>) {
        match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
            // Timeout (receiver alive but nothing emitted) or channel
            // closed (sender dropped) — both mean "no event". Pass.
            Err(_) | Ok(None) => {}
            Ok(Some(evt)) => panic!("expected no inbound, got {evt:?}"),
        }
    }

    #[tokio::test]
    async fn valid_token_accepted() {
        let (state, _rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "ADDED_TO_SPACE",
            "space": {"name": "spaces/X"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_token_rejected_with_401() {
        let (state, _rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({"type":"MESSAGE","space":{"name":"x"}}))
            .unwrap();
        let req = post_with_token(WEBHOOK_PATH, body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_rejected_with_401() {
        let (state, _rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body =
            serde_json::to_vec(&json!({"type":"MESSAGE","space":{"name":"x"}})).unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token=other"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn different_length_token_rejected() {
        let (state, _rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body =
            serde_json::to_vec(&json!({"type":"MESSAGE","space":{"name":"x"}})).unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token=short"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn message_event_emits_inbound() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/AAA", "type": "ROOM"},
            "user": {"name": "users/1", "displayName": "Alice", "type": "HUMAN"},
            "message": {
                "name": "spaces/AAA/messages/M1",
                "text": "hi there",
                "thread": {"name": "spaces/AAA/threads/T1"}
            }
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.expect("inbound emitted");
        assert_eq!(evt.channel_type.as_str(), "gchat");
        assert_eq!(evt.platform_id, "spaces/AAA");
        assert_eq!(evt.thread_id.as_deref(), Some("spaces/AAA/threads/T1"));
        assert_eq!(evt.message.id, "spaces/AAA/messages/M1");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hi there");
        assert_eq!(evt.message.is_group, Some(true));
        assert_eq!(evt.message.is_mention, Some(false));
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "users/1");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
        assert_eq!(sender.channel_type.as_str(), "gchat");
    }

    #[tokio::test]
    async fn bot_user_is_filtered() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/X", "type": "ROOM"},
            "user": {"name": "users/9", "type": "BOT"},
            "message": {"name": "spaces/X/messages/M9", "text": "loop"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn bot_user_id_match_filtered() {
        let (state, mut rx) = make_state(Some("users/bot".into()));
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/X", "type": "ROOM"},
            "user": {"name": "users/bot", "type": "HUMAN"},
            "message": {"name": "spaces/X/messages/M9", "text": "self"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let _ = app.oneshot(req).await.unwrap();
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn added_to_space_ignored() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "ADDED_TO_SPACE",
            "space": {"name": "spaces/X"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn removed_from_space_ignored() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "REMOVED_FROM_SPACE",
            "space": {"name": "spaces/X"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn unknown_event_ignored() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "WIDGET_UPDATED",
            "space": {"name": "spaces/X"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn card_clicked_emits_action_and_parameters() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "CARD_CLICKED",
            "space": {"name": "spaces/X", "type": "ROOM"},
            "user": {"name": "users/2"},
            "message": {
                "name": "spaces/X/messages/M2",
                "action": {"actionMethodName": "submit"},
                "parameters": [{"key":"id","value":"42"}]
            }
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.expect("inbound emitted");
        assert_eq!(evt.platform_id, "spaces/X");
        assert_eq!(evt.message.id, "spaces/X/messages/M2");
        assert_eq!(evt.message.content["action"]["actionMethodName"], "submit");
        assert_eq!(evt.message.content["parameters"][0]["key"], "id");
    }

    #[tokio::test]
    async fn duplicate_message_name_suppressed() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let payload = json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/AAA", "type": "ROOM"},
            "user": {"name": "users/1"},
            "message": {"name": "spaces/AAA/messages/DUPE", "text": "once"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body.clone());
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Resend.
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _first = rx.recv().await.expect("first delivered");
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn malformed_body_returns_400() {
        let (state, _rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let req = post_with_token(
            &format!("{WEBHOOK_PATH}?token={TOKEN}"),
            b"not json".to_vec(),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn dm_space_is_not_group() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/DM1", "type": "DM"},
            "user": {"name": "users/1"},
            "message": {"name": "spaces/DM1/messages/M1", "text": "hey"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.expect("inbound emitted");
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[tokio::test]
    async fn room_space_is_group() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/R1", "type": "ROOM"},
            "user": {"name": "users/1"},
            "message": {"name": "spaces/R1/messages/M2", "text": "team"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.expect("inbound emitted");
        assert_eq!(evt.message.is_group, Some(true));
    }

    #[tokio::test]
    async fn argument_text_diff_marks_mention() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/R1", "type": "ROOM"},
            "user": {"name": "users/1"},
            "message": {
                "name": "spaces/R1/messages/M3",
                "text": "@bot do thing",
                "argumentText": " do thing"
            }
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.expect("inbound emitted");
        assert_eq!(evt.message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn message_without_user_is_dropped() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/R1", "type": "ROOM"},
            "message": {"name": "spaces/R1/messages/M3", "text": "ghost"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn message_without_message_payload_is_dropped() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router(WEBHOOK_PATH, state);
        let body = serde_json::to_vec(&json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/R1", "type": "ROOM"},
            "user": {"name": "users/1"}
        }))
        .unwrap();
        let req = post_with_token(&format!("{WEBHOOK_PATH}?token={TOKEN}"), body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_no_inbound(&mut rx).await;
    }

    #[tokio::test]
    async fn event_dedup_capacity_drops_oldest() {
        let dedup = EventDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("e{i}")).await);
        }
        // Re-observe existing id -> false.
        assert!(!dedup.observe("e0").await);
        // Overflow with a new one -> oldest ("e0") is dropped from a full ring.
        assert!(dedup.observe("e256").await);
        // "e0" should be observable again.
        assert!(dedup.observe("e0").await);
    }

    #[test]
    fn token_matches_constant_time() {
        assert!(token_matches("abc", Some("abc")));
        assert!(!token_matches("abc", Some("abd")));
        assert!(!token_matches("abc", Some("ab")));
        assert!(!token_matches("abc", None));
        assert!(!token_matches("abc", Some("")));
    }

    #[test]
    fn token_matches_empty_expected_only_matches_empty() {
        // (edge case) an empty expected token matches only an empty supplied
        // token. In practice config.rs rejects empty client_token, so this
        // path is just defensive.
        assert!(token_matches("", Some("")));
        assert!(!token_matches("", Some("x")));
        assert!(!token_matches("", None));
    }

    #[test]
    fn event_dedup_observe_idempotent_for_same_id() {
        // Single-thread non-async check is awkward here; the async tests
        // above cover the real behavior. This test exists to assert the
        // default constructor at least.
        let _ = EventDedup::new();
    }
}
