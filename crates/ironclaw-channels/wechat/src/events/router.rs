//! Axum router for the `WeChat` Work webhook.
//!
//! Two endpoints share the configured `path`:
//!
//! - `GET` — URL verification handshake. Work Weixin sends
//!   `msg_signature`, `timestamp`, `nonce`, and `echostr` (an AES-encrypted
//!   nonce) as query parameters. We verify the signature against
//!   `[token, timestamp, nonce, echostr]`, decrypt `echostr`, and return
//!   the plaintext (the inner random nonce, not the XML payload) with
//!   status 200. Anything else returns 401 / 400.
//!
//! - `POST` — encrypted XML push. Query params carry the signature, body
//!   is `<xml>...<Encrypt>...</Encrypt></xml>`. We extract `Encrypt`,
//!   verify the signature against `[token, timestamp, nonce, encrypt]`,
//!   decrypt to inner XML, parse, deduplicate by `MsgId`, and forward to
//!   the inbound channel.
//!
//! Inbound message types handled:
//!
//! | `MsgType` | Result |
//! |---|---|
//! | `text` | `MessageKind::Chat`, `content = {"text": "..."}`. |
//! | `image` / `voice` / `video` / `file` | `MessageKind::Chat`, content with `attachment` metadata. |
//! | `event` | `MessageKind::System`, content `{ "event": ..., "event_key": ... }`. |
//! | anything else | acknowledged at the HTTP layer; no inbound event. |
//!
//! Duplicate `MsgId` values are suppressed via an LRU-ish ring of capacity
//! [`DEDUP_CAPACITY`].

use crate::parse::{MsgType, parse_inbound_xml};
use crate::signature::{decrypt_payload, verify_msg_signature};
use axum::{
    Router,
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{TimeZone, Utc};
use ironclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};

/// Maximum number of `MsgId` values we remember for duplicate suppression.
pub const DEDUP_CAPACITY: usize = 256;

/// LRU-ish ring of recently-seen `MsgId`s.
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
pub struct WeChatEventsState {
    /// Plain-text token used for signature verification.
    pub token: Arc<String>,
    /// `encoding_aes_key` (43-char base64).
    pub encoding_aes_key: Arc<String>,
    /// Expected receiver `corp_id` (the company tenant id this app belongs to).
    pub corp_id: Arc<String>,
    /// Inbound sender — the host owns the receiver.
    pub inbound_tx: Sender<InboundEvent>,
    /// Channel-type label attached to emitted events.
    pub channel_type: ChannelType,
    /// LRU of seen `MsgId`s.
    pub dedup: Arc<EventDedup>,
}

impl WeChatEventsState {
    /// Construct from constituent parts.
    #[must_use]
    pub fn new(
        token: impl Into<String>,
        encoding_aes_key: impl Into<String>,
        corp_id: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        channel_type: ChannelType,
    ) -> Self {
        Self {
            token: Arc::new(token.into()),
            encoding_aes_key: Arc::new(encoding_aes_key.into()),
            corp_id: Arc::new(corp_id.into()),
            inbound_tx,
            channel_type,
            dedup: Arc::new(EventDedup::new()),
        }
    }
}

/// Query parameters Work Weixin sends on every callback (GET and POST).
#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    /// SHA1 signature over the sorted concatenation.
    #[serde(default)]
    pub msg_signature: Option<String>,
    /// Unix-second timestamp.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// Random nonce.
    #[serde(default)]
    pub nonce: Option<String>,
    /// AES-encrypted echo string. Present only on the GET URL-verify call.
    #[serde(default)]
    pub echostr: Option<String>,
}

/// Build the axum router. GET and POST both target `path`.
pub fn build_events_router(path: &str, state: WeChatEventsState) -> Router {
    Router::new()
        .route(path, get(handle_verify).post(handle_notification))
        .with_state(state)
}

async fn handle_verify(
    State(state): State<WeChatEventsState>,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let (Some(sig), Some(ts), Some(nonce), Some(echostr)) =
        (q.msg_signature, q.timestamp, q.nonce, q.echostr)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if verify_msg_signature(&state.token, &ts, &nonce, &echostr, Some(&sig)).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(plain) = decrypt_payload(&state.encoding_aes_key, &echostr, &state.corp_id) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    (StatusCode::OK, plain).into_response()
}

async fn handle_notification(
    State(state): State<WeChatEventsState>,
    Query(q): Query<CallbackQuery>,
    body: Bytes,
) -> Response {
    let (Some(sig), Some(ts), Some(nonce)) = (q.msg_signature, q.timestamp, q.nonce) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // The body is XML carrying an `<Encrypt>` field. Extract it the same
    // way `parse_inbound_xml` extracts inner fields — we share the helper
    // via a local re-implementation here to avoid making it public, since
    // the parse module is for *decrypted* payloads.
    let Ok(body_str) = std::str::from_utf8(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(encrypt) = extract_encrypt(body_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if verify_msg_signature(&state.token, &ts, &nonce, &encrypt, Some(&sig)).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(plain) = decrypt_payload(&state.encoding_aes_key, &encrypt, &state.corp_id) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(parsed) = parse_inbound_xml(&plain) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    if let Some(msg_id) = parsed.msg_id.as_ref() {
        if !state.dedup.observe(msg_id).await {
            return StatusCode::OK.into_response();
        }
    }

    if let Some(event) = build_inbound_event(&state, &parsed) {
        if let Err(err) = state.inbound_tx.send(event).await {
            tracing::warn!(error=%err, "wechat inbound channel closed");
        }
    }
    StatusCode::OK.into_response()
}

/// Extract the inner text of an `<Encrypt>...</Encrypt>` element from a
/// shallow XML body. CDATA wrappers are stripped.
fn extract_encrypt(body: &str) -> Option<String> {
    let start = body.find("<Encrypt>")?;
    let after_open = start + "<Encrypt>".len();
    let end = body[after_open..].find("</Encrypt>")?;
    let raw = &body[after_open..after_open + end].trim();
    let cdata_open = "<![CDATA[";
    let cdata_close = "]]>";
    if let Some(rest) = raw.strip_prefix(cdata_open) {
        if let Some(inner) = rest.strip_suffix(cdata_close) {
            return Some(inner.to_owned());
        }
    }
    Some((*raw).to_owned())
}

fn build_inbound_event(
    state: &WeChatEventsState,
    parsed: &crate::parse::InboundXml,
) -> Option<InboundEvent> {
    let ts = parsed
        .create_time
        .parse::<i64>()
        .ok()
        .and_then(|s| Utc.timestamp_opt(s, 0).single())
        .unwrap_or_else(Utc::now);
    let platform_id = format!("user:{}", parsed.from_user_name);
    let id = parsed
        .msg_id
        .clone()
        .unwrap_or_else(|| format!("evt-{}", parsed.create_time));

    let (kind, content) = match parsed.msg_type {
        MsgType::Text => (MessageKind::Chat, json!({"text": parsed.content})),
        MsgType::Image | MsgType::Voice | MsgType::Video | MsgType::File => (
            MessageKind::Chat,
            json!({
                "text": "",
                "attachment": {
                    "kind": parsed.msg_type.as_str(),
                    "media_id": parsed.media_id.clone().unwrap_or_default(),
                    "filename": parsed.file_name.clone(),
                    "format": parsed.format.clone(),
                }
            }),
        ),
        MsgType::Event => (
            MessageKind::System,
            json!({
                "event": parsed.event.clone().unwrap_or_default(),
                "event_key": parsed.event_key.clone(),
            }),
        ),
        MsgType::Other => return None,
    };

    let sender = SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: parsed.from_user_name.clone(),
        display_name: None,
    };

    Some(InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id,
        thread_id: None,
        message: InboundMessage {
            id,
            kind,
            content,
            timestamp: ts,
            is_mention: None,
            is_group: Some(false),
        },
        reply_to: None,
        sender: Some(sender),
    })
}

/// Re-export of the standard `Value` import to silence unused-warnings on
/// `serde_json::Value` (used only inside one of the conditional arms).
#[allow(dead_code)]
fn _value_alias() -> Value {
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::compute_msg_signature;
    use aes::Aes256;
    use axum::body::Body;
    use axum::http::Request;
    use base64::Engine;
    use cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    type Aes256CbcEnc = cbc::Encryptor<Aes256>;

    const TOKEN: &str = "tok";
    const CORP_ID: &str = "wx-corp";

    fn good_aes_key() -> String {
        let raw = [3u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        encoded.trim_end_matches('=').to_owned()
    }

    fn make_state() -> (WeChatEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let state = WeChatEventsState::new(
            TOKEN,
            good_aes_key(),
            CORP_ID,
            tx,
            ChannelType::new("wechat"),
        );
        (state, rx)
    }

    fn encrypt(plain: &[u8], corp_id: &str) -> String {
        let key_str = good_aes_key();
        let key_bytes = crate::signature::decode_aes_key(&key_str).unwrap();
        let iv: [u8; 16] = key_bytes[..16].try_into().unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0xAB; 16]);
        let len = u32::try_from(plain.len()).unwrap().to_be_bytes();
        buf.extend_from_slice(&len);
        buf.extend_from_slice(plain);
        buf.extend_from_slice(corp_id.as_bytes());
        let enc = Aes256CbcEnc::new_from_slices(&key_bytes, &iv).unwrap();
        let cipher = enc.encrypt_padded_vec::<Pkcs7>(&buf);
        base64::engine::general_purpose::STANDARD.encode(cipher)
    }

    fn xml_text_msg(from: &str, content: &str, msg_id: &str) -> String {
        format!(
            r"<xml><ToUserName><![CDATA[wx-corp]]></ToUserName><FromUserName><![CDATA[{from}]]></FromUserName><CreateTime>1700000000</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[{content}]]></Content><MsgId>{msg_id}</MsgId></xml>"
        )
    }

    fn make_post_request(
        path_str: &str,
        token: &str,
        encrypt_b64: &str,
        body_xml: &str,
    ) -> Request<Body> {
        let ts = "1700000001";
        let nonce = "n";
        let sig = compute_msg_signature(token, ts, nonce, encrypt_b64);
        let uri = format!("{path_str}?msg_signature={sig}&timestamp={ts}&nonce={nonce}");
        Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::from(body_xml.to_owned()))
            .unwrap()
    }

    fn envelope(encrypted: &str) -> String {
        format!(
            r"<xml><ToUserName><![CDATA[wx-corp]]></ToUserName><Encrypt><![CDATA[{encrypted}]]></Encrypt><AgentID>1</AgentID></xml>"
        )
    }

    #[tokio::test]
    async fn verify_returns_decrypted_echostr() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let echo = encrypt(b"random-nonce", CORP_ID);
        // The signature is computed over the *decoded* echostr (what axum
        // hands to the handler). The wire form percent-encodes any base64
        // characters that aren't URL-safe.
        let ts = "1700000002";
        let nonce = "n";
        let sig = compute_msg_signature(TOKEN, ts, nonce, &echo);
        let echo_q = url_encode(&echo);
        let uri =
            format!("/wc?msg_signature={sig}&timestamp={ts}&nonce={nonce}&echostr={echo_q}");
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 256).await.unwrap();
        assert_eq!(&body[..], b"random-nonce");
    }

    fn url_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
                _ => {
                    let mut buf = [0u8; 4];
                    for b in ch.encode_utf8(&mut buf).bytes() {
                        out.push_str(&format!("%{b:02X}"));
                    }
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn verify_missing_params_returns_400() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state);
        let req = Request::builder()
            .method("GET")
            .uri("/wc")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn verify_bad_signature_returns_401() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state);
        let echo = "ZZZZ";
        let bad_sig = "0".repeat(40);
        let uri =
            format!("/wc?msg_signature={bad_sig}&timestamp=1&nonce=n&echostr={echo}");
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn verify_bad_aes_payload_returns_401() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let echo = "not-valid-aes-bytes";
        let ts = "1";
        let nonce = "n";
        let sig = compute_msg_signature(&state.token, ts, nonce, echo);
        let uri = format!("/wc?msg_signature={sig}&timestamp={ts}&nonce={nonce}&echostr={echo}");
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_text_emits_inbound_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = xml_text_msg("alice", "hello", "M1");
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.channel_type.as_str(), "wechat");
        assert_eq!(evt.platform_id, "user:alice");
        assert_eq!(evt.message.id, "M1");
        assert_eq!(evt.message.content["text"], "hello");
        let sender = evt.sender.expect("sender present");
        assert_eq!(sender.identity, "alice");
        assert_eq!(sender.channel_type.as_str(), "wechat");
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[tokio::test]
    async fn post_image_emits_attachment_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><MsgType><![CDATA[image]]></MsgType><MediaId><![CDATA[MID-IMG]]></MediaId><MsgId>2</MsgId></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["text"], "");
        assert_eq!(evt.message.content["attachment"]["kind"], "image");
        assert_eq!(evt.message.content["attachment"]["media_id"], "MID-IMG");
    }

    #[tokio::test]
    async fn post_voice_emits_attachment_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><MsgType><![CDATA[voice]]></MsgType><MediaId><![CDATA[MID-V]]></MediaId><Format><![CDATA[amr]]></Format><MsgId>3</MsgId></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["attachment"]["kind"], "voice");
        assert_eq!(evt.message.content["attachment"]["format"], "amr");
    }

    #[tokio::test]
    async fn post_video_emits_attachment_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><MsgType><![CDATA[video]]></MsgType><MediaId><![CDATA[MID-VID]]></MediaId><MsgId>4</MsgId></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["attachment"]["kind"], "video");
    }

    #[tokio::test]
    async fn post_file_emits_attachment_with_filename() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><MsgType><![CDATA[file]]></MsgType><MediaId><![CDATA[MID-F]]></MediaId><FileName><![CDATA[r.pdf]]></FileName><MsgId>5</MsgId></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["attachment"]["filename"], "r.pdf");
    }

    #[tokio::test]
    async fn post_event_emits_system_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><MsgType><![CDATA[event]]></MsgType><Event><![CDATA[subscribe]]></Event></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["event"], "subscribe");
    }

    #[tokio::test]
    async fn post_event_with_event_key_propagates() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><MsgType><![CDATA[event]]></MsgType><Event><![CDATA[click]]></Event><EventKey><![CDATA[K42]]></EventKey></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.content["event_key"], "K42");
    }

    #[tokio::test]
    async fn post_other_msg_type_is_acked_no_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><MsgType><![CDATA[location]]></MsgType><MsgId>9</MsgId></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn post_duplicate_msg_id_suppressed() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = xml_text_msg("alice", "once", "DUPE");
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req1 = make_post_request("/wc", &state.token, &enc, &body);
        let req2 = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.clone().oneshot(req1).await.unwrap();
        let _ = app.oneshot(req2).await.unwrap();
        let _ = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn post_missing_query_returns_400() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state);
        let req = Request::builder()
            .method("POST")
            .uri("/wc")
            .body(Body::from("<xml></xml>"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_missing_encrypt_returns_400() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let body = r"<xml><ToUserName><![CDATA[x]]></ToUserName></xml>";
        let ts = "1";
        let nonce = "n";
        let sig = compute_msg_signature(&state.token, ts, nonce, "x");
        let uri = format!("/wc?msg_signature={sig}&timestamp={ts}&nonce={nonce}");
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_bad_signature_returns_401() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = xml_text_msg("alice", "hi", "M1");
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let bad_sig = "0".repeat(40);
        let uri = format!("/wc?msg_signature={bad_sig}&timestamp=1&nonce=n");
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_corrupted_ciphertext_returns_401() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        // Decode a valid-shape ciphertext, flip a byte, re-encode.
        let inner = xml_text_msg("alice", "hi", "M1");
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(encrypt(inner.as_bytes(), CORP_ID))
            .unwrap();
        bytes[20] ^= 0x01;
        let enc = base64::engine::general_purpose::STANDARD.encode(bytes);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_wrong_corpid_returns_401() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = xml_text_msg("alice", "hi", "M1");
        // Encrypt with the wrong corp id — verifies the corpid check inside
        // decrypt_payload.
        let enc = encrypt(inner.as_bytes(), "other-corp");
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_inner_xml_invalid_returns_400() {
        let (state, _rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = b"not xml";
        let enc = encrypt(inner, CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_with_empty_msg_id_still_emits_event() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        // No MsgId → use create_time fallback.
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><CreateTime>1700000000</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[hi]]></Content></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.message.id.starts_with("evt-"));
    }

    #[test]
    fn extract_encrypt_with_cdata() {
        let body = r"<xml><Encrypt><![CDATA[ABC]]></Encrypt></xml>";
        assert_eq!(extract_encrypt(body).as_deref(), Some("ABC"));
    }

    #[test]
    fn extract_encrypt_without_cdata() {
        let body = r"<xml><Encrypt>ABC</Encrypt></xml>";
        assert_eq!(extract_encrypt(body).as_deref(), Some("ABC"));
    }

    #[test]
    fn extract_encrypt_missing_returns_none() {
        assert!(extract_encrypt("<xml></xml>").is_none());
    }

    #[tokio::test]
    async fn event_dedup_capacity_drops_oldest() {
        let dedup = EventDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("m{i}")).await);
        }
        assert!(!dedup.observe("m0").await);
        assert!(dedup.observe("m9999").await);
        assert!(dedup.observe("m0").await);
    }

    #[tokio::test]
    async fn event_dedup_default_constructor() {
        let dedup = EventDedup::default();
        assert!(dedup.observe("x").await);
        assert!(!dedup.observe("x").await);
    }

    #[tokio::test]
    async fn state_constructor_populates_fields() {
        let (state, _rx) = make_state();
        assert_eq!(state.token.as_str(), TOKEN);
        assert_eq!(state.encoding_aes_key.as_str(), &good_aes_key());
        assert_eq!(state.corp_id.as_str(), CORP_ID);
        assert_eq!(state.channel_type.as_str(), "wechat");
    }

    #[tokio::test]
    async fn build_inbound_event_returns_none_for_other_msg_type() {
        let (state, _rx) = make_state();
        let parsed = crate::parse::InboundXml {
            to_user_name: "x".into(),
            from_user_name: "u".into(),
            create_time: "1700000000".into(),
            msg_type: MsgType::Other,
            msg_type_raw: "weird".into(),
            content: String::new(),
            msg_id: None,
            media_id: None,
            event: None,
            event_key: None,
            agent_id: None,
            format: None,
            file_name: None,
        };
        assert!(build_inbound_event(&state, &parsed).is_none());
    }

    #[tokio::test]
    async fn build_inbound_event_text_message_round_trip() {
        let (state, _rx) = make_state();
        let parsed = crate::parse::InboundXml {
            to_user_name: "x".into(),
            from_user_name: "alice".into(),
            create_time: "1700000000".into(),
            msg_type: MsgType::Text,
            msg_type_raw: "text".into(),
            content: "hello".into(),
            msg_id: Some("M1".into()),
            media_id: None,
            event: None,
            event_key: None,
            agent_id: None,
            format: None,
            file_name: None,
        };
        let evt = build_inbound_event(&state, &parsed).expect("event");
        assert_eq!(evt.platform_id, "user:alice");
        assert_eq!(evt.message.content["text"], "hello");
    }

    #[tokio::test]
    async fn unparseable_create_time_falls_back_to_now() {
        let (state, mut rx) = make_state();
        let app = build_events_router("/wc", state.clone());
        let inner = r"<xml><FromUserName><![CDATA[alice]]></FromUserName><CreateTime>not-a-number</CreateTime><MsgType><![CDATA[text]]></MsgType><Content><![CDATA[x]]></Content><MsgId>BADTS</MsgId></xml>";
        let enc = encrypt(inner.as_bytes(), CORP_ID);
        let body = envelope(&enc);
        let req = make_post_request("/wc", &state.token, &enc, &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        let now = Utc::now().timestamp();
        assert!((evt.message.timestamp.timestamp() - now).abs() <= 5);
    }
}
