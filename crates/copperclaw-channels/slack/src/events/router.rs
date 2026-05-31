//! Axum router for the Slack Events API webhook.
//!
//! The router serves two payload shapes on the same path so operators can
//! point Slack's "Request URL" (events) and "Interactivity Request URL"
//! (block-kit actions) at the same endpoint:
//!
//! - JSON `event_callback` envelopes (the canonical Events API),
//!   demultiplexed by [`SlackEventEnvelope`].
//! - Form-encoded `payload=<urlencoded-json>` (interactive components — what
//!   Block Kit `button` taps land as). The JSON inside is a `block_actions`
//!   payload; we parse it via [`parse_block_actions`] and synthesise an
//!   inbound chat event whose text is the tapped button's `value` so the
//!   agent sees the tap as if the user typed the value, mirroring the
//!   Telegram `callback_query` pattern.

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
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, ReplyTo, SenderIdentity,
};
use serde_json::{Value, json};
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

    // Interactivity payloads (block_actions) arrive form-encoded as
    // `payload=<urlencoded-json>`. Detect by the Content-Type header (Slack
    // always sets `application/x-www-form-urlencoded` for these) so we don't
    // misroute JSON envelopes that happen to start with `payload=`.
    if is_form_urlencoded(&headers) {
        return handle_interactive(&state, &body).await;
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

/// Handle an interactive payload (`block_actions` / `interactive_message`).
///
/// Slack expects an empty `200 OK` within 3 s — the tap spinner clears as
/// soon as we respond, so we ACK immediately and ship the synthesised
/// inbound event to the host channel without round-tripping any further
/// Slack API calls.
async fn handle_interactive(state: &SlackEventsState, body: &[u8]) -> Response {
    // Body is `payload=<urlencoded-json>`. Strip the prefix, then percent-
    // decode (form encoding == percent encoding with `+` standing in for a
    // space; we use a tiny inline decoder so we don't pull a new crate just
    // for this).
    let Ok(body_str) = std::str::from_utf8(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(("payload", encoded)) = body_str.split_once('=') else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(json_text) = form_decode(encoded) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(payload): Result<Value, _> = serde_json::from_str(&json_text) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    if let Some(evt) = parse_block_actions(state, &payload) {
        // Best-effort send; the ACK still goes out either way so the user's
        // client clears its spinner. Slack will retry the interactive
        // payload if we send a non-2xx, so failing the channel send by
        // returning an error here would create user-visible duplicates.
        if let Err(err) = state.inbound_tx.send(evt).await {
            tracing::warn!(error=%err, "slack inbound channel closed (interactive)");
        }
    }
    // Empty 200 OK — Slack's "do nothing else" ACK.
    StatusCode::OK.into_response()
}

/// Map a `block_actions` JSON payload to a chat-shaped `InboundEvent`.
///
/// Returns `None` when the payload isn't a `block_actions` shape, no
/// `actions[]` entry was usable (no `value`), or required routing fields
/// are missing — in which case the caller still ACKs to clear the spinner.
///
/// The synthesised event mimics a regular chat message so the agent's
/// existing branching code Just Works:
///
/// - `kind = MessageKind::Chat`,
/// - `content.text = action.value` (the tapped button's canonical value),
/// - `content.callback` carries the platform metadata an agent might want
///   (`action_id`, `block_id`, `trigger_id`, container) without inflating
///   the primary `text` field.
pub fn parse_block_actions(state: &SlackEventsState, payload: &Value) -> Option<InboundEvent> {
    let kind = payload.get("type").and_then(Value::as_str)?;
    if kind != "block_actions" {
        return None;
    }

    let action = payload
        .get("actions")
        .and_then(Value::as_array)
        .and_then(|a| a.first())?;
    let value = action.get("value").and_then(Value::as_str)?;
    let action_id = action.get("action_id").and_then(Value::as_str);
    let block_id = action.get("block_id").and_then(Value::as_str);

    // Slack puts the channel + message + ts inside `container` for block
    // taps in a regular channel. For DMs the same fields land under
    // `channel`. Try `container` first, then fall back to `channel`.
    let container = payload.get("container");
    let channel_id = container
        .and_then(|c| c.get("channel_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("channel")
                .and_then(|c| c.get("id"))
                .and_then(Value::as_str)
        })?;

    let message_ts = container
        .and_then(|c| c.get("message_ts"))
        .and_then(Value::as_str);
    // Slack threading: when the original card lives inside a thread, the
    // interactive payload carries `container.thread_ts`. Otherwise the tap
    // is at the channel root and we leave thread_id None.
    let thread_id = container
        .and_then(|c| c.get("thread_ts"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let user = payload.get("user");
    let user_id = user.and_then(|u| u.get("id")).and_then(Value::as_str);
    let display_name = user
        .and_then(|u| u.get("username"))
        .and_then(Value::as_str)
        .or_else(|| {
            user.and_then(|u| u.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned);

    let trigger_id = payload.get("trigger_id").and_then(Value::as_str);
    let response_url = payload.get("response_url").and_then(Value::as_str);

    let sender = user_id.map(|uid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: uid.to_owned(),
        display_name,
    });

    // The interactive payload doesn't carry the message ts in the way a
    // regular `message` event does, so use the action_id + trigger_id (or
    // the original message_ts when present) as a unique-enough id for
    // dedup on the host side. This matches Telegram's strategy of using
    // the callback_query id as the inbound message id.
    let message_id = if let Some(a) = action_id {
        if let Some(ts) = message_ts {
            format!("{a}:{ts}")
        } else if let Some(t) = trigger_id {
            format!("{a}:{t}")
        } else {
            a.to_owned()
        }
    } else {
        // Falling back to the value keeps the event stable across
        // retries when no other id is present.
        value.to_owned()
    };

    let mut callback = json!({
        "value": value,
    });
    if let Some(a) = action_id {
        callback["action_id"] = Value::String(a.to_owned());
    }
    if let Some(b) = block_id {
        callback["block_id"] = Value::String(b.to_owned());
    }
    if let Some(ts) = message_ts {
        callback["message_ts"] = Value::String(ts.to_owned());
    }
    if let Some(t) = trigger_id {
        callback["trigger_id"] = Value::String(t.to_owned());
    }
    if let Some(r) = response_url {
        callback["response_url"] = Value::String(r.to_owned());
    }

    let content = json!({
        "text": value,
        "callback": callback,
    });

    let is_group = Some(channel_id.starts_with('C') || channel_id.starts_with('G'));

    Some(InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: channel_id.to_owned(),
        thread_id,
        message: InboundMessage {
            id: message_id,
            kind: MessageKind::Chat,
            content,
            timestamp: Utc::now(),
            is_mention: None,
            is_group,
        },
        reply_to: None,
        sender,
    })
}

fn is_form_urlencoded(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            // Header may include `; charset=...` — match the prefix.
            s.trim()
                .to_ascii_lowercase()
                .starts_with("application/x-www-form-urlencoded")
        })
}

/// Inline form-decoder. Replaces `+` with space and percent-decodes the rest.
/// Returns an error only when a percent escape is malformed.
fn form_decode(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(());
                }
                let hi = hex_digit(bytes[i + 1])?;
                let lo = hex_digit(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn hex_digit(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
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
    // Slack uses `thread_ts` for two distinct things:
    //   * On the thread root, `thread_ts == ts`.
    //   * On a reply within a thread, `thread_ts` points at the root
    //     (which IS the message being replied to from Slack's POV).
    // Only the latter is a real reply, so we surface `reply_to` only when
    // the two timestamps differ.
    let reply_to = m
        .thread_ts
        .as_deref()
        .filter(|parent| *parent != m.ts.as_str())
        .map(|parent| ReplyTo {
            channel_type: state.channel_type.clone(),
            platform_id: m.channel.clone(),
            thread_id: Some(parent.to_owned()),
        });
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
        reply_to,
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

    #[tokio::test]
    async fn thread_reply_populates_reply_to() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        // thread_ts != ts → this is a reply inside an existing thread.
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev-Reply",
            "event":{
                "type":"message",
                "ts":"1700000020.000010",
                "channel":"C-CHAN",
                "user":"U-REPLIER",
                "text":"+1",
                "thread_ts":"1700000010.000001"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        let rt = evt.reply_to.expect("reply_to populated for in-thread reply");
        assert_eq!(rt.channel_type.as_str(), "slack");
        assert_eq!(rt.platform_id, "C-CHAN");
        assert_eq!(rt.thread_id.as_deref(), Some("1700000010.000001"));
    }

    #[tokio::test]
    async fn thread_root_does_not_populate_reply_to() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        // thread_ts == ts → the message IS the thread root, not a reply.
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev-Root",
            "event":{
                "type":"message",
                "ts":"1700000030.000001",
                "channel":"C-CHAN",
                "user":"U-AUTHOR",
                "text":"starting a thread",
                "thread_ts":"1700000030.000001"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.reply_to.is_none(),
            "thread root carries thread_ts==ts; that should NOT be a reply_to");
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

    /// Build a Slack `block_actions` payload, percent-encode it, and POST
    /// it as `payload=<encoded>` so the interactive branch exercises the
    /// same shape Slack actually sends.
    fn signed_interactive_request(
        state: &SlackEventsState,
        path: &str,
        payload_json: &str,
    ) -> Request<Body> {
        let encoded = percent_encode_form(payload_json);
        let body = format!("payload={encoded}");
        let body_bytes = body.into_bytes();
        let sig = compute_signature(&state.signing_secret, TS, &body_bytes);
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body_bytes))
            .unwrap()
    }

    /// Minimal `application/x-www-form-urlencoded` value encoder — only the
    /// reserved set so test strings round-trip predictably.
    fn percent_encode_form(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for &b in s.as_bytes() {
            let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
            if unreserved {
                out.push(b as char);
            } else if b == b' ' {
                out.push('+');
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    #[tokio::test]
    async fn interactive_block_actions_emits_inbound_with_value_text() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U42", "username": "alice"},
            "trigger_id": "trig-1",
            "response_url": "https://hooks.slack.test/r",
            "container": {
                "type": "message",
                "channel_id": "C100",
                "message_ts": "1700000050.000100"
            },
            "actions": [{
                "type": "button",
                "action_id": "card_btn_0",
                "block_id": "card_actions",
                "value": "deploy:yes"
            }]
        });
        let req = signed_interactive_request(
            &state,
            "/slack/events",
            &payload.to_string(),
        );
        let resp = app.oneshot(req).await.unwrap();
        // Slack requires a 2xx ACK so the spinner clears.
        assert_eq!(resp.status(), StatusCode::OK);

        let evt = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("event delivered within timeout")
            .expect("inbound channel open");
        assert_eq!(evt.channel_type.as_str(), "slack");
        assert_eq!(evt.platform_id, "C100");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.kind, MessageKind::Chat);
        // Text mimics what the user "typed" — the button value.
        assert_eq!(evt.message.content["text"], "deploy:yes");
        // Callback metadata is preserved for agents that want to branch on it.
        assert_eq!(evt.message.content["callback"]["action_id"], "card_btn_0");
        assert_eq!(evt.message.content["callback"]["value"], "deploy:yes");
        assert_eq!(evt.message.content["callback"]["message_ts"], "1700000050.000100");
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "U42");
        assert_eq!(sender.display_name.as_deref(), Some("alice"));
        // is_group derived from the `C` channel prefix.
        assert_eq!(evt.message.is_group, Some(true));
    }

    #[tokio::test]
    async fn interactive_dm_routes_to_channel_id_under_channel_field() {
        // Slack puts the channel under `channel.id` instead of
        // `container.channel_id` for some legacy / DM interactive shapes.
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U7", "name": "bob"},
            "channel": {"id": "D1", "name": "directmessage"},
            "actions": [{
                "type": "button",
                "action_id": "card_btn_1",
                "value": "approve"
            }]
        });
        let req = signed_interactive_request(
            &state,
            "/slack/events",
            &payload.to_string(),
        );
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "D1");
        assert_eq!(evt.message.is_group, Some(false));
        assert_eq!(evt.message.content["text"], "approve");
    }

    #[tokio::test]
    async fn interactive_preserves_thread_ts_when_card_was_in_thread() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U7"},
            "container": {
                "channel_id": "C1",
                "message_ts": "1700000060.000010",
                "thread_ts": "1700000050.000001"
            },
            "actions": [{
                "type": "button",
                "action_id": "card_btn_0",
                "value": "ack"
            }]
        });
        let req = signed_interactive_request(
            &state,
            "/slack/events",
            &payload.to_string(),
        );
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.thread_id.as_deref(), Some("1700000050.000001"));
    }

    #[tokio::test]
    async fn interactive_url_button_taps_have_no_value_so_no_event() {
        // Slack doesn't fire block_actions for `url` buttons — but we still
        // defend the parser against payloads with a missing `value`.
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U7"},
            "container": {"channel_id": "C1", "message_ts": "1.0"},
            "actions": [{
                "type": "button",
                "action_id": "card_btn_0",
                "url": "https://example.com"
            }]
        });
        let req = signed_interactive_request(
            &state,
            "/slack/events",
            &payload.to_string(),
        );
        let resp = app.oneshot(req).await.unwrap();
        // ACK still 200.
        assert_eq!(resp.status(), StatusCode::OK);
        // But no event.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn interactive_unsigned_request_returns_401() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = b"payload=%7B%7D".to_vec();
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("content-type", "application/x-www-form-urlencoded")
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
    async fn interactive_malformed_form_body_returns_400_no_panic() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        // No `payload=` prefix.
        let body = b"foo=bar".to_vec();
        let sig = compute_signature(&state.signing_secret, TS, &body);
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn form_decode_handles_plus_and_percent_escapes() {
        assert_eq!(form_decode("hello+world").unwrap(), "hello world");
        assert_eq!(form_decode("a%20b").unwrap(), "a b");
        assert_eq!(form_decode("%7B%22a%22%3A1%7D").unwrap(), "{\"a\":1}");
        // Truncated escape — should error rather than panic.
        assert!(form_decode("%2").is_err());
        // Non-hex digit.
        assert!(form_decode("%ZZ").is_err());
    }

    #[test]
    fn parse_block_actions_returns_none_for_other_payload_types() {
        let (state, _rx) = make_state(None);
        let payload = json!({"type": "view_submission"});
        assert!(parse_block_actions(&state, &payload).is_none());
    }

    #[test]
    fn is_form_urlencoded_handles_charset_suffix() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8"
                .parse()
                .unwrap(),
        );
        assert!(is_form_urlencoded(&h));

        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        assert!(!is_form_urlencoded(&h));
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
