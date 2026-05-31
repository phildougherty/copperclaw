//! Axum router for Mattermost outgoing-webhook ingress.
//!
//! Mattermost outgoing webhooks POST `application/x-www-form-urlencoded`
//! by default but most modern deployments use the JSON Content-Type
//! variant (preferred). We accept both: requests with
//! `application/json` are parsed as JSON; everything else is parsed as
//! form-encoded. Either way the upstream fields are the same — see
//! `https://docs.mattermost.com/developer/webhooks-outgoing.html`.
//!
//! Authentication is by shared `token`: Mattermost sends the
//! configured token in the body and we constant-time-compare it
//! against [`crate::config::MattermostConfig::webhook_token`]. Token
//! mismatch → 401. Missing fields → 400. Everything else → 200 with
//! an empty body (we don't speak the response-back-from-webhook
//! protocol; replies come via the egress API).

use crate::config::MattermostConfig;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use chrono::Utc;
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity,
};
use serde::Deserialize;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

/// Shared state for the router handlers.
#[derive(Clone)]
pub struct RouterState {
    pub channel_type: ChannelType,
    pub config: MattermostConfig,
    pub inbound_tx: Sender<InboundEvent>,
}

impl RouterState {
    #[must_use]
    pub fn new(
        channel_type: ChannelType,
        config: MattermostConfig,
        inbound_tx: Sender<InboundEvent>,
    ) -> Self {
        Self {
            channel_type,
            config,
            inbound_tx,
        }
    }
}

/// Build the axum router with the outgoing-webhook handler mounted at
/// the configured path.
pub fn build_router(state: RouterState) -> Router {
    let path = state.config.webhook.path.clone();
    Router::new()
        .route(&path, post(handle))
        .with_state(state)
}

/// Fields Mattermost sends on every outgoing-webhook POST. See the
/// outgoing-webhooks doc page for the canonical list — we use the
/// subset that's stable across versions.
#[derive(Debug, Default, Deserialize)]
struct OutgoingPayload {
    token: Option<String>,
    team_id: Option<String>,
    team_domain: Option<String>,
    channel_id: Option<String>,
    channel_name: Option<String>,
    timestamp: Option<i64>,
    user_id: Option<String>,
    user_name: Option<String>,
    post_id: Option<String>,
    text: Option<String>,
    trigger_word: Option<String>,
    file_ids: Option<String>,
}

async fn handle(
    State(state): State<RouterState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    let payload = match parse(&headers, &body) {
        Ok(p) => p,
        Err(reason) => {
            tracing::warn!(reason, "mattermost webhook parse failed");
            return (StatusCode::BAD_REQUEST, reason);
        }
    };

    let provided = payload.token.as_deref().unwrap_or("");
    if !constant_eq(provided.as_bytes(), state.config.webhook_token.as_bytes()) {
        tracing::warn!("mattermost webhook token mismatch");
        return (StatusCode::UNAUTHORIZED, "token mismatch");
    }

    // Drop messages from the bot itself so the agent doesn't reply to
    // its own posts.
    if let (Some(bot_id), Some(uid)) =
        (state.config.bot_user_id.as_deref(), payload.user_id.as_deref())
    {
        if bot_id == uid {
            return (StatusCode::OK, "ignored: bot message");
        }
    }

    let Some(channel_id) = payload.channel_id.as_deref() else {
        return (StatusCode::BAD_REQUEST, "missing channel_id");
    };

    let text = payload.text.clone().unwrap_or_default();
    let trigger = payload.trigger_word.clone();
    let post_id = payload.post_id.clone();
    let user_id = payload.user_id.clone();
    let user_name = payload.user_name.clone();

    let mut content = serde_json::Map::new();
    content.insert("text".into(), serde_json::Value::String(text));
    if let Some(t) = trigger {
        content.insert("trigger_word".into(), serde_json::Value::String(t));
    }
    if let Some(team) = payload.team_domain.clone() {
        content.insert("team_domain".into(), serde_json::Value::String(team));
    }
    if let Some(name) = payload.channel_name.clone() {
        content.insert("channel_name".into(), serde_json::Value::String(name));
    }

    let inbound_id = post_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let timestamp = payload
        .timestamp
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .unwrap_or_else(Utc::now);

    let sender = user_id.map(|uid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: uid,
        display_name: user_name,
    });

    // TODO(channel-ux): Mattermost outgoing-webhook payload does NOT
    // expose channel type (direct / group-direct / open / private), so
    // we cannot populate `is_group` from the wire fields alone. The
    // documented payload (token / team_id / channel_id / channel_name /
    // user_id / text / trigger_word / file_ids — see
    // https://developers.mattermost.com/integrate/webhooks/outgoing/) is
    // type-agnostic. To populate this we'd need a REST follow-up to
    // `GET /api/v4/channels/{channel_id}` and map `type` (`D`/`G` →
    // group=false / DM, `O`/`P` → group=true). That requires an
    // `MattermostApi` handle on the router state and a cache (channels
    // rarely change type) so we don't add a synchronous lookup per
    // inbound. Left as `None` until that follow-up lands.
    let event = InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: channel_id.to_string(),
        thread_id: post_id,
        message: InboundMessage {
            id: inbound_id,
            kind: MessageKind::Chat,
            content: serde_json::Value::Object(content),
            timestamp,
            is_mention: None,
            is_group: None,
        },
        reply_to: None,
        sender,
    };
    let _ = payload.file_ids; // currently unused; kept for forward-compat.
    let _ = payload.team_id;

    if state.inbound_tx.send(event).await.is_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, "host shutting down");
    }
    (StatusCode::OK, "ok")
}

/// Parse the request body according to the Content-Type. Returns a
/// short reason on parse failure that's safe to surface to the
/// upstream Mattermost server.
fn parse(headers: &HeaderMap, body: &[u8]) -> Result<OutgoingPayload, &'static str> {
    let is_json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("application/json"));
    if is_json {
        serde_json::from_slice(body).map_err(|_| "invalid json body")
    } else {
        parse_form(body)
    }
}

fn parse_form(body: &[u8]) -> Result<OutgoingPayload, &'static str> {
    // Mattermost form bodies use the standard
    // `application/x-www-form-urlencoded` shape. We deserialize via
    // serde_urlencoded by way of a manual conversion to JSON so the
    // `serde(default)` semantics line up with the JSON path.
    let s = std::str::from_utf8(body).map_err(|_| "non-utf8 body")?;
    let mut map = serde_json::Map::new();
    for pair in s.split('&').filter(|p| !p.is_empty()) {
        let mut iter = pair.splitn(2, '=');
        let key = decode_form(iter.next().unwrap_or(""))?;
        let val = decode_form(iter.next().unwrap_or(""))?;
        if key.is_empty() {
            continue;
        }
        // `timestamp` is an integer in the wire format — keep its
        // semantics by parsing eagerly when it looks numeric.
        let parsed_val = if key == "timestamp" {
            val.parse::<i64>()
                .map(serde_json::Value::from)
                .unwrap_or(serde_json::Value::String(val))
        } else {
            serde_json::Value::String(val)
        };
        map.insert(key, parsed_val);
    }
    serde_json::from_value(serde_json::Value::Object(map))
        .map_err(|_| "form body did not match expected fields")
}

fn decode_form(s: &str) -> Result<String, &'static str> {
    // Lightweight percent-decode + plus-to-space — avoid pulling in
    // another dependency for what is a tiny well-defined transform.
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
                    return Err("malformed percent escape");
                }
                let hi = from_hex(bytes[i + 1])?;
                let lo = from_hex(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "form value was not utf8")
}

fn from_hex(b: u8) -> Result<u8, &'static str> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err("invalid hex digit"),
    }
}

/// Branch-free byte-wise equality. We only use this for token compare
/// where leaking timing on token length is acceptable but we want to
/// avoid early-exit on first mismatch.
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MattermostConfig, WebhookBind};
    use axum::body::Body;
    use axum::http::Request;
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

    fn cfg(bot: Option<&str>) -> MattermostConfig {
        MattermostConfig {
            server_url: "https://chat.example".into(),
            access_token: "tok".into(),
            webhook_token: "wh-secret".into(),
            webhook: WebhookBind {
                host: "127.0.0.1".into(),
                port: 0,
                path: "/mattermost/webhook".into(),
            },
            bot_user_id: bot.map(str::to_string),
        }
    }

    async fn post(
        router: Router,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> axum::http::Response<axum::body::Body> {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
        router.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn json_post_with_valid_token_emits_event() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let router = build_router(RouterState::new(
            ChannelType::new("mattermost"),
            cfg(None),
            tx,
        ));
        let payload = serde_json::json!({
            "token": "wh-secret",
            "channel_id": "c1",
            "user_id": "u1",
            "user_name": "alice",
            "post_id": "p9",
            "text": "hello",
            "trigger_word": "hey",
            "timestamp": 1_700_000_000_000_i64,
        });
        let resp = post(
            router,
            "/mattermost/webhook",
            "application/json",
            serde_json::to_vec(&payload).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.platform_id, "c1");
        assert_eq!(event.thread_id.as_deref(), Some("p9"));
        assert_eq!(event.message.id, "p9");
        assert_eq!(event.message.content["text"], "hello");
        assert_eq!(event.message.content["trigger_word"], "hey");
        let sender = event.sender.unwrap();
        assert_eq!(sender.identity, "u1");
        assert_eq!(sender.display_name.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn form_post_with_valid_token_emits_event() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let router = build_router(RouterState::new(
            ChannelType::new("mattermost"),
            cfg(None),
            tx,
        ));
        let body = "token=wh-secret&channel_id=c2&text=hi+there&post_id=p1";
        let resp = post(
            router,
            "/mattermost/webhook",
            "application/x-www-form-urlencoded",
            body.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.platform_id, "c2");
        assert_eq!(event.message.content["text"], "hi there");
    }

    #[tokio::test]
    async fn token_mismatch_returns_401() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let router = build_router(RouterState::new(
            ChannelType::new("mattermost"),
            cfg(None),
            tx,
        ));
        let payload =
            serde_json::json!({"token": "wrong", "channel_id": "c1", "text": "hi"});
        let resp = post(
            router,
            "/mattermost/webhook",
            "application/json",
            serde_json::to_vec(&payload).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn missing_channel_id_returns_400() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let router = build_router(RouterState::new(
            ChannelType::new("mattermost"),
            cfg(None),
            tx,
        ));
        let payload = serde_json::json!({"token": "wh-secret", "text": "no channel"});
        let resp = post(
            router,
            "/mattermost/webhook",
            "application/json",
            serde_json::to_vec(&payload).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_json_body_returns_400() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let router = build_router(RouterState::new(
            ChannelType::new("mattermost"),
            cfg(None),
            tx,
        ));
        let resp = post(
            router,
            "/mattermost/webhook",
            "application/json",
            b"not-json".to_vec(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn bot_self_message_is_dropped() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let router = build_router(RouterState::new(
            ChannelType::new("mattermost"),
            cfg(Some("bot-9")),
            tx,
        ));
        let payload = serde_json::json!({
            "token": "wh-secret",
            "channel_id": "c1",
            "user_id": "bot-9",
            "text": "echo",
        });
        let resp = post(
            router,
            "/mattermost/webhook",
            "application/json",
            serde_json::to_vec(&payload).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn closed_inbound_returns_503() {
        let (tx, rx) = mpsc::channel::<InboundEvent>(8);
        drop(rx);
        let router = build_router(RouterState::new(
            ChannelType::new("mattermost"),
            cfg(None),
            tx,
        ));
        let payload =
            serde_json::json!({"token": "wh-secret", "channel_id": "c", "text": "x"});
        let resp = post(
            router,
            "/mattermost/webhook",
            "application/json",
            serde_json::to_vec(&payload).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn constant_eq_handles_length_mismatch() {
        assert!(!constant_eq(b"abc", b"ab"));
        assert!(!constant_eq(b"ab", b"abc"));
        assert!(constant_eq(b"abcd", b"abcd"));
        assert!(!constant_eq(b"abcd", b"abce"));
    }

    #[test]
    fn decode_form_handles_percent_and_plus() {
        assert_eq!(decode_form("hello+world").unwrap(), "hello world");
        assert_eq!(decode_form("a%20b").unwrap(), "a b");
        assert!(decode_form("a%2").is_err());
        assert!(decode_form("a%zz").is_err());
    }

    #[test]
    fn parse_form_recognises_numeric_timestamp() {
        let body = b"timestamp=1700000000000&channel_id=c";
        let parsed = parse_form(body).unwrap();
        assert_eq!(parsed.timestamp, Some(1_700_000_000_000));
        assert_eq!(parsed.channel_id.as_deref(), Some("c"));
    }
}
