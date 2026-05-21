//! Twitter / X v2 REST client (plus the v1.1 media upload endpoint).
//!
//! Wraps the small slice of the platform the DM-focused adapter needs.
//! Every failure shape is lifted to a typed [`AdapterError`]:
//!
//! - `200` / `201` -> `Ok`
//! - `401` / `403` -> [`AdapterError::Auth`]
//! - `404` -> [`AdapterError::BadRequest`]
//! - `429` -> [`AdapterError::Rate`]; prefers the `x-rate-limit-reset`
//!   epoch-seconds header (converted to seconds-from-now), then
//!   `Retry-After`, then a 60-second fallback.
//! - `5xx` / network failures -> [`AdapterError::Transport`]
//! - `4xx` other -> [`AdapterError::BadRequest`]
//!
//! The X v2 error body shape is `{"errors":[{"message":"...","code":<int>,
//! "type":"..."}]}` or `{"title":"...","type":"...","detail":"..."}`; both
//! are accepted by [`classify_error`].

use ironclaw_channels_core::AdapterError;
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

/// Default fallback used when X returns 429 without a useful hint.
pub const DEFAULT_RATE_RETRY_SECONDS: u64 = 60;

/// Result of a successful DM send (either to a user or to a conversation).
#[derive(Debug, Clone, Deserialize)]
pub struct SendDmResponse {
    /// The conversation id the message was posted in.
    pub dm_conversation_id: String,
    /// The platform-side id of the resulting DM event.
    pub dm_event_id: String,
}

/// Result of a media upload.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadMediaResponse {
    /// Stringified media id to embed in a DM `attachments` array.
    pub media_id_string: String,
}

/// Minimal X v2 REST client.
#[derive(Debug, Clone)]
pub struct XApi {
    http: Client,
    api_base: String,
    media_base: String,
    bearer_token: String,
}

impl XApi {
    /// Build a client using the configured bases and bearer token.
    pub fn new(
        api_base: impl Into<String>,
        media_base: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Self {
        Self::with_client(Client::new(), api_base, media_base, bearer_token)
    }

    /// Build with a caller-supplied [`reqwest::Client`]. Useful for tests.
    pub fn with_client(
        http: Client,
        api_base: impl Into<String>,
        media_base: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Self {
        Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            media_base: media_base.into().trim_end_matches('/').to_owned(),
            bearer_token: bearer_token.into(),
        }
    }

    /// Configured v2 REST base URL.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Configured v1.1 media base URL.
    pub fn media_base(&self) -> &str {
        &self.media_base
    }

    /// Configured bearer token.
    pub fn bearer_token(&self) -> &str {
        &self.bearer_token
    }

    fn api_url(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.api_base)
    }

    fn media_url(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.media_base)
    }

    /// `POST /2/dm_conversations/with/:participant_id/messages` — send a DM
    /// to a single user (the conversation is created or reused implicitly).
    pub async fn dm_send_to_user(
        &self,
        participant_id: &str,
        text: &str,
        media_ids: &[String],
    ) -> Result<SendDmResponse, AdapterError> {
        let url = self.api_url(&format!(
            "/2/dm_conversations/with/{}/messages",
            urlencoding(participant_id)
        ));
        let body = build_message_body(text, media_ids);
        self.post_dm(&url, &body).await
    }

    /// `POST /2/dm_conversations/:dm_conversation_id/messages` — send a DM
    /// into a previously known conversation (1:1 or group).
    pub async fn dm_send_to_conversation(
        &self,
        dm_conversation_id: &str,
        text: &str,
        media_ids: &[String],
    ) -> Result<SendDmResponse, AdapterError> {
        let url = self.api_url(&format!(
            "/2/dm_conversations/{}/messages",
            urlencoding(dm_conversation_id)
        ));
        let body = build_message_body(text, media_ids);
        self.post_dm(&url, &body).await
    }

    /// `POST /2/dm_conversations` — create a group DM seeded with `text`.
    pub async fn dm_create_group(
        &self,
        participant_ids: &[String],
        text: &str,
    ) -> Result<SendDmResponse, AdapterError> {
        let url = self.api_url("/2/dm_conversations");
        let body = json!({
            "conversation_type": "Group",
            "participant_ids": participant_ids,
            "message": { "text": text }
        });
        self.post_dm(&url, &body).await
    }

    /// `POST https://upload.twitter.com/1.1/media/upload.json` — upload a
    /// single image / gif / video using base64 `media_data`.
    ///
    /// `media_category` is typically `"dm_image"`, `"dm_gif"`, or `"dm_video"`.
    pub async fn upload_media(
        &self,
        bytes: &[u8],
        media_category: &str,
    ) -> Result<UploadMediaResponse, AdapterError> {
        let url = self.media_url("/1.1/media/upload.json");
        let media_data = base64_encode(bytes);
        let form = [
            ("media_data", media_data.as_str()),
            ("media_category", media_category),
        ];
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.bearer_token)
            .form(&form)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_x_json(resp).await?;
        serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("x media upload decode: {e}")))
    }

    /// `GET /2/dm_events?dm_event_types=MessageCreate&...` — fetch the latest
    /// DM events for the authenticated user.
    ///
    /// `since_id` is the last seen `dm_event_id`; when present only newer
    /// events are returned.
    pub async fn dm_events_page(
        &self,
        since_id: Option<&str>,
        max_results: u32,
    ) -> Result<Value, AdapterError> {
        let url = self.api_url("/2/dm_events");
        let max = max_results.to_string();
        let mut query: Vec<(&str, String)> = vec![
            ("dm_event_types", "MessageCreate".to_owned()),
            ("max_results", max),
            (
                "expansions",
                "sender_id,referenced_tweets.id,attachments.media_keys".to_owned(),
            ),
            ("user.fields", "name,username".to_owned()),
            (
                "dm_event.fields",
                "id,text,sender_id,dm_conversation_id,created_at,event_type".to_owned(),
            ),
        ];
        if let Some(since) = since_id {
            query.push(("since_id", since.to_owned()));
        }
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .query(&query)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        read_x_json(resp).await
    }

    async fn post_dm(&self, url: &str, body: &Value) -> Result<SendDmResponse, AdapterError> {
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.bearer_token)
            .json(body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_x_json(resp).await?;
        let data = value.get("data").cloned().ok_or_else(|| {
            AdapterError::Transport("x dm response missing `data`".into())
        })?;
        serde_json::from_value(data)
            .map_err(|e| AdapterError::Transport(format!("x dm send decode: {e}")))
    }
}

/// Build the `text`-plus-optional-`attachments` body shared by every send.
fn build_message_body(text: &str, media_ids: &[String]) -> Value {
    let mut body = json!({ "text": text });
    if !media_ids.is_empty() {
        body["attachments"] = Value::Array(
            media_ids
                .iter()
                .map(|id| json!({ "media_id": id }))
                .collect(),
        );
    }
    body
}

fn urlencoding(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn map_send_err(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(format!("x http error: {err}"))
}

async fn read_x_json(resp: Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after_header = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let rate_limit_reset = resp
        .headers()
        .get("x-rate-limit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let body = resp
        .text()
        .await
        .map_err(|e| AdapterError::Transport(format!("x body read failed: {e}")))?;
    let parsed: Option<Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_str(&body).ok()
    };

    if status.is_success() {
        return parsed.ok_or_else(|| {
            AdapterError::Transport(format!("x response not JSON: {body}"))
        });
    }

    Err(classify_error(
        status,
        parsed.as_ref(),
        retry_after_header,
        rate_limit_reset,
        now_unix_seconds(),
    ))
}

/// Translate an unsuccessful HTTP response into a typed [`AdapterError`].
///
/// `now_secs` is the current unix epoch in seconds — supplied as an
/// argument so the function stays pure and testable.
pub(crate) fn classify_error(
    status: StatusCode,
    body: Option<&Value>,
    retry_after_header: Option<u64>,
    rate_limit_reset: Option<i64>,
    now_secs: i64,
) -> AdapterError {
    let description = describe_error(body, status);

    match status.as_u16() {
        401 | 403 => AdapterError::Auth(description),
        404 => AdapterError::BadRequest(description),
        429 => AdapterError::Rate {
            retry_after: Some(rate_retry_seconds(
                rate_limit_reset,
                retry_after_header,
                now_secs,
            )),
        },
        s if (500..600).contains(&s) => AdapterError::Transport(description),
        s if (400..500).contains(&s) => AdapterError::BadRequest(description),
        _ => AdapterError::Transport(description),
    }
}

/// Compute the `retry_after` seconds value for a 429 response.
///
/// Preference order: `x-rate-limit-reset` (epoch seconds, converted to
/// seconds-from-now), then the standard `Retry-After` header, then the
/// 60-second fallback.
pub(crate) fn rate_retry_seconds(
    rate_limit_reset: Option<i64>,
    retry_after_header: Option<u64>,
    now_secs: i64,
) -> u64 {
    if let Some(reset) = rate_limit_reset {
        let delta = reset - now_secs;
        if delta > 0 {
            return u64::try_from(delta).unwrap_or(DEFAULT_RATE_RETRY_SECONDS);
        }
        return 0;
    }
    if let Some(h) = retry_after_header {
        return h;
    }
    DEFAULT_RATE_RETRY_SECONDS
}

/// Pull a human-readable description out of an X error body, falling back
/// to the bare status if no useful field is present.
pub(crate) fn describe_error(body: Option<&Value>, status: StatusCode) -> String {
    if let Some(value) = body {
        if let Some(errors) = value.get("errors").and_then(Value::as_array) {
            if let Some(first) = errors.first() {
                if let Some(msg) = first.get("message").and_then(Value::as_str) {
                    return msg.to_owned();
                }
                if let Some(detail) = first.get("detail").and_then(Value::as_str) {
                    return detail.to_owned();
                }
            }
        }
        if let Some(detail) = value.get("detail").and_then(Value::as_str) {
            return detail.to_owned();
        }
        if let Some(title) = value.get("title").and_then(Value::as_str) {
            return title.to_owned();
        }
    }
    format!("x http {status}")
}

fn now_unix_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Minimal base64 encoder; the workspace does not depend on the `base64`
/// crate.
fn base64_encode(bytes: &[u8]) -> String {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() / 3 + 1) * 4);
    let mut buf = [0u8; 3];
    for chunk in bytes.chunks(3) {
        for (i, byte) in chunk.iter().enumerate() {
            buf[i] = *byte;
        }
        let b0 = buf[0];
        let b1 = if chunk.len() > 1 { buf[1] } else { 0 };
        let b2 = if chunk.len() > 2 { buf[2] } else { 0 };
        let n: u32 = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(alphabet[((n >> 18) & 63) as usize] as char);
        out.push(alphabet[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(alphabet[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(alphabet[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(server_url: &str) -> XApi {
        XApi::with_client(Client::new(), server_url, server_url, "tok")
    }

    fn split_api(api_url: &str, media_url: &str) -> XApi {
        XApi::with_client(Client::new(), api_url, media_url, "tok")
    }

    #[tokio::test]
    async fn dm_send_to_user_returns_event_id() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(
                r"^/2/dm_conversations/with/.+/messages$",
            ))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": {
                    "dm_conversation_id": "111-222",
                    "dm_event_id": "evt-1"
                }
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .dm_send_to_user("222", "hi", &[])
            .await
            .unwrap();
        assert_eq!(r.dm_conversation_id, "111-222");
        assert_eq!(r.dm_event_id, "evt-1");
    }

    #[tokio::test]
    async fn dm_send_to_user_includes_attachments_when_media_present() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(
                r"^/2/dm_conversations/with/.+/messages$",
            ))
            .and(wiremock::matchers::body_string_contains("\"media_id\":\"mid1\""))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "dm_conversation_id": "c", "dm_event_id": "e" }
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .dm_send_to_user("222", "img", &["mid1".into()])
            .await
            .unwrap();
        assert_eq!(r.dm_event_id, "e");
    }

    #[tokio::test]
    async fn dm_send_to_conversation_returns_event_id() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2/dm_conversations/abc/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "dm_conversation_id": "abc", "dm_event_id": "e2" }
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .dm_send_to_conversation("abc", "hi", &[])
            .await
            .unwrap();
        assert_eq!(r.dm_conversation_id, "abc");
        assert_eq!(r.dm_event_id, "e2");
    }

    #[tokio::test]
    async fn dm_create_group_returns_event_id() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2/dm_conversations"))
            .and(wiremock::matchers::body_string_contains("\"Group\""))
            .and(wiremock::matchers::body_string_contains("\"participant_ids\""))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "dm_conversation_id": "grp-1", "dm_event_id": "e3" }
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .dm_create_group(&["1".into(), "2".into()], "hi")
            .await
            .unwrap();
        assert_eq!(r.dm_conversation_id, "grp-1");
        assert_eq!(r.dm_event_id, "e3");
    }

    #[tokio::test]
    async fn upload_media_returns_media_id_string() {
        let api_srv = MockServer::start().await;
        let media_srv = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/1.1/media/upload.json"))
            .and(wiremock::matchers::body_string_contains("media_data="))
            .and(wiremock::matchers::body_string_contains("media_category=dm_image"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "media_id_string": "mid-7"
            })))
            .mount(&media_srv)
            .await;
        let r = split_api(&api_srv.uri(), &media_srv.uri())
            .upload_media(&[1, 2, 3], "dm_image")
            .await
            .unwrap();
        assert_eq!(r.media_id_string, "mid-7");
    }

    #[tokio::test]
    async fn dm_events_page_applies_since_id() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .and(query_param("dm_event_types", "MessageCreate"))
            .and(query_param("max_results", "100"))
            .and(query_param("since_id", "evt-prev"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [],
                "meta": {}
            })))
            .mount(&s)
            .await;
        let v = api(&s.uri())
            .dm_events_page(Some("evt-prev"), 100)
            .await
            .unwrap();
        assert!(v.get("data").is_some());
    }

    #[tokio::test]
    async fn dm_events_page_without_since_omits_param() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .and(query_param("dm_event_types", "MessageCreate"))
            .and(query_param("max_results", "50"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [],
                "meta": {}
            })))
            .mount(&s)
            .await;
        let v = api(&s.uri()).dm_events_page(None, 50).await.unwrap();
        assert!(v.get("data").is_some());
    }

    #[tokio::test]
    async fn dm_send_401_is_auth() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "errors": [{ "message": "bad token", "code": 32 }]
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn dm_send_403_is_auth() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "errors": [{ "message": "cannot dm user" }]
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn dm_send_404_is_bad_request() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "title": "Not Found"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn dm_send_429_with_rate_limit_reset_is_rate() {
        let s = MockServer::start().await;
        let reset = (chrono::Utc::now().timestamp() + 42).to_string();
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("x-rate-limit-reset", reset.as_str())
                    .set_body_json(json!({ "errors": [{"message": "rl"}] })),
            )
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        match err {
            AdapterError::Rate { retry_after: Some(secs) } => {
                assert!(secs > 0 && secs <= 60);
            }
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dm_send_429_with_retry_after_is_rate() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "11")
                    .set_body_string(""),
            )
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: Some(11) }));
    }

    #[tokio::test]
    async fn dm_send_429_without_hint_uses_default() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(429).set_body_string(""))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            AdapterError::Rate {
                retry_after: Some(DEFAULT_RATE_RETRY_SECONDS)
            }
        ));
    }

    #[tokio::test]
    async fn dm_send_5xx_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn dm_send_other_4xx_is_bad_request() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(418).set_body_string("teapot"))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn dm_send_3xx_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(304).set_body_string(""))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn dm_send_malformed_success_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn dm_send_success_missing_data_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        let err = api(&s.uri())
            .dm_send_to_user("u", "x", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn network_failure_is_transport() {
        // 127.0.0.1:1 is reserved; the connect must fail fast.
        let api = XApi::new("http://127.0.0.1:1", "http://127.0.0.1:1", "tok");
        let err = api.dm_send_to_user("u", "x", &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn build_message_body_no_attachments() {
        let body = build_message_body("hi", &[]);
        assert_eq!(body["text"], "hi");
        assert!(body.get("attachments").is_none());
    }

    #[test]
    fn build_message_body_with_attachments() {
        let body = build_message_body("img", &["m1".into(), "m2".into()]);
        assert_eq!(body["text"], "img");
        let attachments = body["attachments"].as_array().unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0]["media_id"], "m1");
        assert_eq!(attachments[1]["media_id"], "m2");
    }

    #[test]
    fn classify_error_401() {
        let err = classify_error(
            StatusCode::UNAUTHORIZED,
            Some(&json!({"errors": [{"message": "no"}]})),
            None,
            None,
            0,
        );
        assert!(matches!(err, AdapterError::Auth(m) if m == "no"));
    }

    #[test]
    fn classify_error_403() {
        let err = classify_error(StatusCode::FORBIDDEN, None, None, None, 0);
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_error_404() {
        let err = classify_error(StatusCode::NOT_FOUND, None, None, None, 0);
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn classify_error_429_uses_rate_limit_reset_when_in_future() {
        let err = classify_error(
            StatusCode::TOO_MANY_REQUESTS,
            None,
            None,
            Some(110),
            100,
        );
        assert!(matches!(err, AdapterError::Rate { retry_after: Some(10) }));
    }

    #[test]
    fn classify_error_429_uses_retry_after_header_when_no_reset() {
        let err = classify_error(
            StatusCode::TOO_MANY_REQUESTS,
            None,
            Some(7),
            None,
            0,
        );
        assert!(matches!(err, AdapterError::Rate { retry_after: Some(7) }));
    }

    #[test]
    fn classify_error_429_fallback_default() {
        let err = classify_error(StatusCode::TOO_MANY_REQUESTS, None, None, None, 0);
        assert!(matches!(
            err,
            AdapterError::Rate {
                retry_after: Some(DEFAULT_RATE_RETRY_SECONDS)
            }
        ));
    }

    #[test]
    fn classify_error_5xx_is_transport() {
        for code in [500u16, 502, 503, 504] {
            let err = classify_error(StatusCode::from_u16(code).unwrap(), None, None, None, 0);
            assert!(matches!(err, AdapterError::Transport(_)));
        }
    }

    #[test]
    fn classify_error_other_4xx_is_bad_request() {
        let err = classify_error(StatusCode::from_u16(418).unwrap(), None, None, None, 0);
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn classify_error_unknown_status_is_transport() {
        let err = classify_error(StatusCode::from_u16(304).unwrap(), None, None, None, 0);
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn rate_retry_seconds_prefers_reset_when_future() {
        assert_eq!(rate_retry_seconds(Some(110), Some(7), 100), 10);
    }

    #[test]
    fn rate_retry_seconds_returns_zero_when_reset_in_past() {
        assert_eq!(rate_retry_seconds(Some(50), Some(7), 100), 0);
    }

    #[test]
    fn rate_retry_seconds_uses_header_when_no_reset() {
        assert_eq!(rate_retry_seconds(None, Some(7), 100), 7);
    }

    #[test]
    fn rate_retry_seconds_default_when_no_hint() {
        assert_eq!(rate_retry_seconds(None, None, 100), DEFAULT_RATE_RETRY_SECONDS);
    }

    #[test]
    fn describe_error_from_errors_message() {
        let v = json!({"errors": [{"message": "boom"}]});
        assert_eq!(describe_error(Some(&v), StatusCode::BAD_REQUEST), "boom");
    }

    #[test]
    fn describe_error_from_errors_detail() {
        let v = json!({"errors": [{"detail": "boomy"}]});
        assert_eq!(describe_error(Some(&v), StatusCode::BAD_REQUEST), "boomy");
    }

    #[test]
    fn describe_error_from_problem_detail() {
        let v = json!({"detail": "problem"});
        assert_eq!(describe_error(Some(&v), StatusCode::BAD_REQUEST), "problem");
    }

    #[test]
    fn describe_error_from_problem_title() {
        let v = json!({"title": "Bad"});
        assert_eq!(describe_error(Some(&v), StatusCode::BAD_REQUEST), "Bad");
    }

    #[test]
    fn describe_error_fallback_to_status() {
        let s = describe_error(None, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(s.contains("500"));
    }

    #[test]
    fn base64_encode_empty() {
        assert_eq!(base64_encode(&[]), "");
    }

    #[test]
    fn base64_encode_one_byte() {
        assert_eq!(base64_encode(b"A"), "QQ==");
    }

    #[test]
    fn base64_encode_two_bytes() {
        assert_eq!(base64_encode(b"AB"), "QUI=");
    }

    #[test]
    fn base64_encode_three_bytes() {
        assert_eq!(base64_encode(b"ABC"), "QUJD");
    }

    #[test]
    fn base64_encode_many_bytes() {
        // "hello world" -> "aGVsbG8gd29ybGQ="
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn urlencoding_percent_escapes() {
        assert_eq!(urlencoding("a/b"), "a%2Fb");
        assert_eq!(urlencoding("user:42"), "user%3A42");
    }

    #[test]
    fn xapi_builders_trim_trailing_slash_and_expose_accessors() {
        let api = XApi::new("https://x.test/", "https://up.test/", "tok");
        assert_eq!(api.api_base(), "https://x.test");
        assert_eq!(api.media_base(), "https://up.test");
        assert_eq!(api.bearer_token(), "tok");
        let _ = api.clone();
        let _ = format!("{api:?}");
    }
}
