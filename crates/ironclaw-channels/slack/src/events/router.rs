//! Axum router for the Slack Events API webhook.

use crate::events::types::{MessageEvent, SlackEvent, SlackEventEnvelope};
use crate::signature::verify_signature;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::post,
};
use chrono::{TimeZone, Utc};
use ironclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity,
};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};

/// Maximum number of recent `event_id`s to keep for duplicate suppression.
pub const DEDUP_CAPACITY: usize = 256;

/// In-memory LRU-ish ring of `event_id`s seen so we can suppress retries.
#[derive(Debug, Default)]
pub struct EventDedup {
    seen: Mutex<VecDeque<String>>,
}

impl EventDedup {
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

/// Shared state for the Slack events HTTP handler.
#[derive(Clone)]
pub struct SlackEventsState {
    pub signing_secret: Arc<String>,
    pub inbound_tx: Sender<InboundEvent>,
    pub dedup: Arc<EventDedup>,
    /// Bot user id (resolved from `auth.test`) used to detect mentions in
    /// text bodies. `None` disables mention detection from text (the
    /// `app_mention` event itself still sets `is_mention`).
    pub bot_user_id: Arc<Option<String>>,
    /// Channel-type label attached to emitted events. Lives in state rather
    /// than a constant so tests can override it.
    pub channel_type: ChannelType,
    /// Override for "now" in seconds — only used by tests for deterministic
    /// signature drift checks.
    pub now_secs_override: Option<i64>,
}

impl SlackEventsState {
    #[must_use]
    pub fn new(
        signing_secret: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        bot_user_id: Option<String>,
        channel_type: ChannelType,
    ) -> Self {
        Self {
            signing_secret: Arc::new(signing_secret.into()),
            inbound_tx,
            dedup: Arc::new(EventDedup::new()),
            bot_user_id: Arc::new(bot_user_id),
            channel_type,
            now_secs_override: None,
        }
    }

    fn now_secs(&self) -> i64 {
        self.now_secs_override
            .unwrap_or_else(|| Utc::now().timestamp())
    }
}

/// Build the Slack events router. Mounts the handler at the given `path`.
pub fn build_events_router(path: &str, state: SlackEventsState) -> Router {
    Router::new()
        .route(path, post(handle_events))
        .with_state(state)
}

async fn handle_events(
    State(state): State<SlackEventsState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ts = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok());
    let sig = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok());
    if verify_signature(&state.signing_secret, ts, sig, &body, state.now_secs()).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let envelope: SlackEventEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match envelope {
        SlackEventEnvelope::UrlVerification { challenge, .. } => {
            (StatusCode::OK, Json(json!({"challenge": challenge}))).into_response()
        }
        SlackEventEnvelope::EventCallback(cb) => {
            if !state.dedup.observe(&cb.event_id).await {
                // Already-seen — respond 200 OK so Slack stops retrying.
                return StatusCode::OK.into_response();
            }
            let event = match cb.event {
                SlackEvent::Message(m) => convert_message(&state, &m, false),
                SlackEvent::AppMention(m) => convert_message(&state, &m, true),
                SlackEvent::Other => return StatusCode::OK.into_response(),
            };
            if let Err(err) = state.inbound_tx.send(event).await {
                tracing::warn!(error=%err, "slack inbound channel closed");
            }
            StatusCode::OK.into_response()
        }
    }
}

fn convert_message(
    state: &SlackEventsState,
    m: &MessageEvent,
    is_app_mention: bool,
) -> InboundEvent {
    let mut content = json!({"text": m.text.clone().unwrap_or_default()});
    if let Some(blocks) = m.blocks.clone() {
        content["blocks"] = blocks;
    }
    let is_mention = if is_app_mention {
        Some(true)
    } else {
        state
            .bot_user_id
            .as_ref()
            .as_ref()
            .map(|bot| m.mentions_user(bot))
    };
    let is_group = Some(m.is_group_channel());
    let sender = m.user.as_ref().map(|uid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: uid.clone(),
        display_name: None,
    });
    let timestamp = parse_slack_ts(&m.ts);
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: m.channel.clone(),
        thread_id: m.thread_ts.clone(),
        message: InboundMessage {
            id: m.ts.clone(),
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

/// Convert Slack's `ts` (`<seconds>.<microseconds>`) into a `DateTime<Utc>`.
/// Falls back to the current time if parsing fails.
pub(crate) fn parse_slack_ts(ts: &str) -> chrono::DateTime<Utc> {
    let trimmed = ts.split('.').next().unwrap_or(ts);
    if let Ok(secs) = trimmed.parse::<i64>() {
        if let Some(dt) = Utc.timestamp_opt(secs, 0).single() {
            return dt;
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
    use serde_json::Value;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    const SECRET: &str = "test-secret";
    const TS: &str = "1700000000";

    fn make_state(
        bot: Option<String>,
    ) -> (SlackEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let mut s = SlackEventsState::new(SECRET, tx, bot, ChannelType::new("slack"));
        s.now_secs_override = Some(TS.parse().unwrap());
        (s, rx)
    }

    fn signed_request(state: &SlackEventsState, path: &str, body: &[u8]) -> Request<Body> {
        let sig = compute_signature(&state.signing_secret, TS, body);
        Request::builder()
            .method("POST")
            .uri(path)
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn url_verification_returns_challenge() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"url_verification","challenge":"abc"
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["challenge"], "abc");
    }

    #[tokio::test]
    async fn rejects_bad_signature() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"url_verification","challenge":"abc"
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("x-slack-request-timestamp", TS)
            .header(
                "x-slack-signature",
                format!("v0={}", "00".repeat(32)),
            )
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_stale_timestamp() {
        let (mut state, _rx) = make_state(None);
        state.now_secs_override = Some(10_000_000_000);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"url_verification","challenge":"abc"
        }))
        .unwrap();
        // Sign with the (stale) TS so the only thing wrong is drift.
        let sig = compute_signature(&state.signing_secret, TS, &body);
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = b"not json".to_vec();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn message_event_emits_inbound() {
        let (state, mut rx) = make_state(Some("UBOT".into()));
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev1",
            "event":{
                "type":"message",
                "ts":"1700000001.000001",
                "channel":"C1",
                "user":"U1",
                "text":"hi <@UBOT>",
                "channel_type":"channel",
                "blocks":[{"type":"rt"}]
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.channel_type.as_str(), "slack");
        assert_eq!(evt.platform_id, "C1");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.content["text"], "hi <@UBOT>");
        assert!(evt.message.content["blocks"].is_array());
        assert_eq!(evt.message.id, "1700000001.000001");
        assert_eq!(evt.message.is_mention, Some(true));
        assert_eq!(evt.message.is_group, Some(true));
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "U1");
        assert_eq!(sender.channel_type.as_str(), "slack");
    }

    #[tokio::test]
    async fn app_mention_sets_is_mention_true() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev2",
            "event":{
                "type":"app_mention",
                "ts":"1700000002.0",
                "channel":"C9",
                "user":"U9",
                "text":"hey",
                "thread_ts":"1700000001.0"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
        assert_eq!(evt.thread_id.as_deref(), Some("1700000001.0"));
    }

    #[tokio::test]
    async fn dm_channel_is_not_group() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev3",
            "event":{
                "type":"message",
                "ts":"1700000003.0",
                "channel":"D1",
                "user":"U1",
                "text":"dm"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[tokio::test]
    async fn duplicate_event_id_is_suppressed() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type":"event_callback",
            "event_id":"DUPE",
            "event":{
                "type":"message",
                "ts":"1700000010.0",
                "channel":"C1",
                "user":"U1",
                "text":"once"
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Resend.
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Only one event delivered.
        let _first = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_event_type_is_acked_without_emit() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev4",
            "event":{"type":"reaction_added"}
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn no_bot_user_id_yields_none_is_mention_for_plain_messages() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev5",
            "event":{
                "type":"message",
                "ts":"1700000005.0",
                "channel":"C1",
                "user":"U1",
                "text":"hi <@UBOT>"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.message.is_mention.is_none());
    }

    #[tokio::test]
    async fn bot_user_id_without_mention_marks_false() {
        let (state, mut rx) = make_state(Some("UBOT".into()));
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev6",
            "event":{
                "type":"message",
                "ts":"1700000006.0",
                "channel":"C1",
                "user":"U1",
                "text":"just chatting"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(false));
    }

    #[tokio::test]
    async fn missing_user_yields_no_sender() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev7",
            "event":{
                "type":"message",
                "ts":"1700000007.0",
                "channel":"C1",
                "text":"ghost"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.sender.is_none());
    }

    #[tokio::test]
    async fn event_dedup_capacity_drops_oldest() {
        let dedup = EventDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("e{i}")).await);
        }
        // Re-observe one of the existing ids → false.
        assert!(!dedup.observe("e0").await);
        // Add one more → drops "e0" from the ring (but it stays denied because
        // we just re-checked it ABOVE; instead let's overflow then observe a new one
        // followed by the original).
        assert!(dedup.observe("e256").await);
        // The earliest id ("e1" now) should no longer be in ring once one is dropped
        // after a fresh insert. But since we already inserted DEDUP_CAPACITY and then
        // one more above ("e256"), the oldest dropped was "e1" if e0 was bumped to
        // back via re-observe? No — re-observe does NOT bump in our impl.
        // So inserting "e256" dropped "e0" from a full ring. Re-observing "e0" should
        // succeed again now.
        assert!(dedup.observe("e0").await);
    }

    #[test]
    fn parse_slack_ts_returns_unix_timestamp() {
        let dt = parse_slack_ts("1700000000.000001");
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }

    #[test]
    fn parse_slack_ts_bad_input_falls_back_to_now() {
        // Just ensure it doesn't panic — value compared with a recent range.
        let before = Utc::now().timestamp() - 5;
        let dt = parse_slack_ts("not-a-ts");
        assert!(dt.timestamp() >= before);
    }

    #[test]
    fn now_secs_falls_back_to_real_clock_without_override() {
        let (tx, _rx) = mpsc::channel(1);
        let s = SlackEventsState::new("s", tx, None, ChannelType::new("slack"));
        let now = s.now_secs();
        let real = Utc::now().timestamp();
        assert!((now - real).abs() <= 5);
    }
}
