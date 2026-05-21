//! Work Weixin REST API client.
//!
//! Wraps the endpoints the adapter needs:
//!
//! - `GET /cgi-bin/gettoken?corpid={corp_id}&corpsecret={corp_secret}` —
//!   obtain a cached access token.
//! - `POST /cgi-bin/message/send?access_token={token}` — send text /
//!   image / file / `template_card`.
//! - `POST /cgi-bin/media/upload?access_token={token}&type=image|file|...`
//!   — multipart upload returning a `media_id`.
//!
//! Token caching is automatic via [`TokenStore`] — on a fresh request we
//! check `expires_at`, refresh if needed, and reuse the same token until
//! it's stale. On `errcode == 42001` (token expired) the request is
//! retried once after a forced refresh.
//!
//! ## Error mapping
//!
//! Work Weixin returns errors in the response body, not the HTTP status:
//!
//! ```json
//! {"errcode": 40014, "errmsg": "invalid access_token"}
//! ```
//!
//! Mapping policy:
//!
//! | `errcode` family | Result |
//! |---|---|
//! | `0` | success |
//! | `40014` / `40001` / `40082` / `42001` / `42007` / `42009` | `Auth` |
//! | `45009` / `45033` | `Rate { retry_after: None }` |
//! | other `>= 40000` | `BadRequest` |
//!
//! HTTP-layer mapping:
//!
//! | HTTP | Result |
//! |---|---|
//! | `401` / `403` | `Auth` |
//! | `429` | `Rate` (honoring `Retry-After`) |
//! | other `4xx` | `BadRequest` |
//! | `5xx` | `Transport` |

use chrono::{DateTime, Utc};
use ironclaw_channels_core::AdapterError;
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Default Work Weixin REST base.
pub const DEFAULT_API_BASE: &str = "https://qyapi.weixin.qq.com";

/// Cached `access_token` and the moment it stops being valid.
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

/// Thread-safe access-token cache with on-demand refresh.
///
/// Public so tests / advanced callers can seed a token directly.
#[derive(Debug, Clone, Default)]
pub struct TokenStore {
    inner: Arc<Mutex<Option<CachedToken>>>,
}

impl TokenStore {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inject a token + expiry directly (test/util).
    pub async fn put(&self, token: impl Into<String>, expires_at: DateTime<Utc>) {
        *self.inner.lock().await = Some(CachedToken {
            token: token.into(),
            expires_at,
        });
    }

    /// Read the current cached token if it's still valid for at least
    /// `slack_secs` more seconds.
    pub async fn get(&self) -> Option<String> {
        let guard = self.inner.lock().await;
        let cached = guard.as_ref()?;
        // 30s headroom: refresh ahead of expiry to dodge clock skew /
        // in-flight races.
        if cached.expires_at - Utc::now() < chrono::Duration::seconds(30) {
            return None;
        }
        Some(cached.token.clone())
    }

    /// Invalidate the cached token. Used after a `42001` response.
    pub async fn invalidate(&self) {
        *self.inner.lock().await = None;
    }
}

/// Minimal Work Weixin REST client.
#[derive(Debug, Clone)]
pub struct WeChatApi {
    client: Client,
    api_base: String,
    corp_id: String,
    corp_secret: String,
    tokens: TokenStore,
}

impl WeChatApi {
    /// Construct a client with a fresh `reqwest::Client`.
    #[must_use]
    pub fn new(
        api_base: impl Into<String>,
        corp_id: impl Into<String>,
        corp_secret: impl Into<String>,
    ) -> Self {
        Self::with_client(Client::new(), api_base, corp_id, corp_secret)
    }

    /// Construct with a caller-supplied `reqwest::Client`.
    #[must_use]
    pub fn with_client(
        client: Client,
        api_base: impl Into<String>,
        corp_id: impl Into<String>,
        corp_secret: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            corp_id: corp_id.into(),
            corp_secret: corp_secret.into(),
            tokens: TokenStore::new(),
        }
    }

    /// Borrow the token store (test convenience).
    #[must_use]
    pub fn tokens(&self) -> &TokenStore {
        &self.tokens
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api_base)
    }

    /// Return a cached access token, refreshing if necessary.
    pub async fn access_token(&self) -> Result<String, AdapterError> {
        if let Some(tok) = self.tokens.get().await {
            return Ok(tok);
        }
        self.refresh_token().await
    }

    /// Force-refresh the access token bypassing the cache.
    pub async fn refresh_token(&self) -> Result<String, AdapterError> {
        let url = self.url("/cgi-bin/gettoken");
        let resp = self
            .client
            .get(url)
            .query(&[("corpid", &self.corp_id), ("corpsecret", &self.corp_secret)])
            .send()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        let value = read_wechat_json(resp).await?;
        let parsed: GetTokenResponse = serde_json::from_value(value).map_err(|e| {
            AdapterError::Transport(format!("wechat gettoken decode: {e}"))
        })?;
        let expires_at = Utc::now() + chrono::Duration::seconds(parsed.expires_in.max(60));
        self.tokens.put(parsed.access_token.clone(), expires_at).await;
        Ok(parsed.access_token)
    }

    /// Send a text message addressed by one of `touser` / `toparty` / `totag`.
    ///
    /// Returns Work Weixin's `msgid` when the platform supplies one.
    pub async fn send_text(
        &self,
        agent_id: i64,
        target: MessageTarget<'_>,
        text: &str,
    ) -> Result<Option<String>, AdapterError> {
        let mut body = json!({
            "msgtype": "text",
            "agentid": agent_id,
            "text": {"content": text},
        });
        target.apply(&mut body);
        self.post_message(&body).await
    }

    /// Send an image addressed by an already-uploaded `media_id`.
    pub async fn send_image_by_id(
        &self,
        agent_id: i64,
        target: MessageTarget<'_>,
        media_id: &str,
    ) -> Result<Option<String>, AdapterError> {
        let mut body = json!({
            "msgtype": "image",
            "agentid": agent_id,
            "image": {"media_id": media_id},
        });
        target.apply(&mut body);
        self.post_message(&body).await
    }

    /// Send a file addressed by an already-uploaded `media_id`.
    pub async fn send_file_by_id(
        &self,
        agent_id: i64,
        target: MessageTarget<'_>,
        media_id: &str,
    ) -> Result<Option<String>, AdapterError> {
        let mut body = json!({
            "msgtype": "file",
            "agentid": agent_id,
            "file": {"media_id": media_id},
        });
        target.apply(&mut body);
        self.post_message(&body).await
    }

    /// Send a `template_card` payload verbatim. The card structure is
    /// defined by the platform; we pass it through.
    pub async fn send_template_card(
        &self,
        agent_id: i64,
        target: MessageTarget<'_>,
        card: &Value,
    ) -> Result<Option<String>, AdapterError> {
        let mut body = json!({
            "msgtype": "template_card",
            "agentid": agent_id,
            "template_card": card,
        });
        target.apply(&mut body);
        self.post_message(&body).await
    }

    /// Upload media bytes and return the `media_id`. Valid `media_type`s
    /// are `image`, `voice`, `video`, `file`.
    pub async fn upload_media(
        &self,
        media_type: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<String, AdapterError> {
        let part = Part::bytes(bytes).file_name(filename.to_owned());
        let form = Form::new().part("media", part);
        let token = self.access_token().await?;
        let resp = self
            .client
            .post(self.url("/cgi-bin/media/upload"))
            .query(&[("access_token", token.as_str()), ("type", media_type)])
            .multipart(form)
            .send()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        let value = read_wechat_json(resp).await?;
        let parsed: UploadResponse = serde_json::from_value(value).map_err(|e| {
            AdapterError::Transport(format!("wechat media upload decode: {e}"))
        })?;
        Ok(parsed.media_id)
    }

    async fn post_message(&self, body: &Value) -> Result<Option<String>, AdapterError> {
        // One-shot retry on token-expired errors.
        let mut attempts = 0;
        loop {
            let token = self.access_token().await?;
            let resp = self
                .client
                .post(self.url("/cgi-bin/message/send"))
                .query(&[("access_token", token.as_str())])
                .json(body)
                .send()
                .await
                .map_err(|e| AdapterError::Transport(e.to_string()))?;
            let value = read_wechat_json(resp).await;
            match value {
                Ok(v) => {
                    let parsed: SendResponse =
                        serde_json::from_value(v).unwrap_or(SendResponse { msgid: None });
                    return Ok(parsed.msgid);
                }
                Err(AdapterError::Auth(msg))
                    if attempts == 0 && msg.contains("42001") =>
                {
                    // Force a refresh and try again exactly once.
                    self.tokens.invalidate().await;
                    attempts += 1;
                    continue;
                }
                Err(other) => return Err(other),
            }
        }
    }
}

/// Outbound addressing — one of `touser`, `toparty`, or `totag`.
#[derive(Debug, Clone, Copy)]
pub enum MessageTarget<'a> {
    /// `touser` field — pipe-delimited userids.
    User(&'a str),
    /// `toparty` field — pipe-delimited party (department) ids.
    Party(&'a str),
    /// `totag` field — pipe-delimited tag ids.
    Tag(&'a str),
}

impl MessageTarget<'_> {
    fn apply(self, body: &mut Value) {
        let (field, value) = match self {
            Self::User(s) => ("touser", s),
            Self::Party(s) => ("toparty", s),
            Self::Tag(s) => ("totag", s),
        };
        body[field] = Value::String(value.to_owned());
    }
}

#[derive(Debug, Deserialize)]
struct GetTokenResponse {
    access_token: String,
    expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct UploadResponse {
    media_id: String,
}

#[derive(Debug, Deserialize)]
struct SendResponse {
    #[serde(default)]
    msgid: Option<String>,
}

async fn read_wechat_json(resp: reqwest::Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.text().await.unwrap_or_default();
    classify_response(status, retry_after, &body)
}

/// Public for unit tests so the classification can be exercised without
/// going through reqwest.
pub(crate) fn classify_response(
    status: StatusCode,
    retry_after: Option<u64>,
    body_text: &str,
) -> Result<Value, AdapterError> {
    let parsed: Option<Value> = if body_text.is_empty() {
        None
    } else {
        serde_json::from_str(body_text).ok()
    };

    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AdapterError::Rate { retry_after });
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(AdapterError::Auth(format!("http {status}")));
    }
    if status.is_client_error() {
        return Err(AdapterError::BadRequest(format!("http {status}")));
    }
    if status.is_server_error() {
        return Err(AdapterError::Transport(format!("http {status}")));
    }
    if !status.is_success() {
        return Err(AdapterError::Transport(format!("unexpected status {status}")));
    }

    // Success path — but Work Weixin folds errors into the body.
    let value = parsed.ok_or_else(|| {
        AdapterError::Transport("wechat response was empty or not JSON".into())
    })?;
    if let Some(errcode) = value.get("errcode").and_then(Value::as_i64) {
        if errcode == 0 {
            return Ok(value);
        }
        let errmsg = value
            .get("errmsg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        return Err(classify_errcode(errcode, &errmsg, retry_after));
    }
    Ok(value)
}

/// Translate a Work Weixin `errcode` into an `AdapterError`.
fn classify_errcode(errcode: i64, errmsg: &str, retry_after: Option<u64>) -> AdapterError {
    let detail = if errmsg.is_empty() {
        format!("errcode {errcode}")
    } else {
        format!("{errcode} {errmsg}")
    };
    match errcode {
        // Token / auth-related
        40001 | 40014 | 40082 | 42001 | 42007 | 42009 => AdapterError::Auth(detail),
        // Rate
        45009 | 45033 => AdapterError::Rate { retry_after },
        // Permission / config errors that still map best to BadRequest:
        // 40036 = invalid agentid, 40068 = invalid signature.
        _ if errcode >= 40000 => AdapterError::BadRequest(detail),
        // Anything else gets surfaced as Transport so retries can happen.
        _ => AdapterError::Transport(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_for(server: &MockServer) -> WeChatApi {
        WeChatApi::new(server.uri(), "wx-corp", "secret")
    }

    #[test]
    fn classify_returns_value_on_zero_errcode() {
        let v = classify_response(StatusCode::OK, None, r#"{"errcode":0,"data":42}"#).unwrap();
        assert_eq!(v["data"], 42);
    }

    #[test]
    fn classify_returns_value_with_no_errcode_field() {
        let v =
            classify_response(StatusCode::OK, None, r#"{"access_token":"x","expires_in":7200}"#)
                .unwrap();
        assert_eq!(v["access_token"], "x");
    }

    #[test]
    fn classify_401_returns_auth() {
        let err = classify_response(StatusCode::UNAUTHORIZED, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_403_returns_auth() {
        let err = classify_response(StatusCode::FORBIDDEN, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_429_returns_rate_with_retry_after() {
        let err = classify_response(StatusCode::TOO_MANY_REQUESTS, Some(5), "").unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(5)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn classify_400_returns_bad_request() {
        let err = classify_response(StatusCode::BAD_REQUEST, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn classify_500_returns_transport() {
        let err = classify_response(StatusCode::INTERNAL_SERVER_ERROR, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_503_returns_transport() {
        let err = classify_response(StatusCode::SERVICE_UNAVAILABLE, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_3xx_returns_transport() {
        let err = classify_response(StatusCode::MOVED_PERMANENTLY, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_empty_body_2xx_is_transport() {
        let err = classify_response(StatusCode::OK, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_invalid_json_2xx_is_transport() {
        let err = classify_response(StatusCode::OK, None, "not json").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_errcode_40014_is_auth() {
        let err = classify_response(
            StatusCode::OK,
            None,
            r#"{"errcode":40014,"errmsg":"invalid token"}"#,
        )
        .unwrap_err();
        match err {
            AdapterError::Auth(m) => {
                assert!(m.contains("40014"));
                assert!(m.contains("invalid token"));
            }
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn classify_errcode_40001_is_auth() {
        let err = classify_response(
            StatusCode::OK,
            None,
            r#"{"errcode":40001,"errmsg":"bad secret"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_errcode_42001_is_auth() {
        let err = classify_response(
            StatusCode::OK,
            None,
            r#"{"errcode":42001,"errmsg":"access_token expired"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_errcode_45009_is_rate() {
        let err = classify_response(
            StatusCode::OK,
            None,
            r#"{"errcode":45009,"errmsg":"api freq"}"#,
        )
        .unwrap_err();
        match err {
            AdapterError::Rate { .. } => {}
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn classify_errcode_45033_is_rate() {
        let err = classify_response(
            StatusCode::OK,
            None,
            r#"{"errcode":45033,"errmsg":"send too fast"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::Rate { .. }));
    }

    #[test]
    fn classify_unknown_high_errcode_is_bad_request() {
        let err = classify_response(
            StatusCode::OK,
            None,
            r#"{"errcode":40036,"errmsg":"invalid agentid"}"#,
        )
        .unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("40036")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_low_errcode_is_transport() {
        let err = classify_response(StatusCode::OK, None, r#"{"errcode":-1,"errmsg":"x"}"#)
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_errmsg_empty_still_renders_errcode() {
        let err = classify_response(StatusCode::OK, None, r#"{"errcode":40014}"#).unwrap_err();
        match err {
            AdapterError::Auth(m) => assert!(m.contains("40014")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn token_store_returns_none_when_empty() {
        let s = TokenStore::new();
        assert!(s.get().await.is_none());
    }

    #[tokio::test]
    async fn token_store_returns_some_when_fresh() {
        let s = TokenStore::new();
        s.put("tok", Utc::now() + chrono::Duration::hours(1)).await;
        assert_eq!(s.get().await.as_deref(), Some("tok"));
    }

    #[tokio::test]
    async fn token_store_returns_none_when_close_to_expiry() {
        let s = TokenStore::new();
        s.put("tok", Utc::now() + chrono::Duration::seconds(5)).await;
        assert!(s.get().await.is_none());
    }

    #[tokio::test]
    async fn token_store_invalidate_clears_state() {
        let s = TokenStore::new();
        s.put("tok", Utc::now() + chrono::Duration::hours(1)).await;
        s.invalidate().await;
        assert!(s.get().await.is_none());
    }

    #[tokio::test]
    async fn access_token_refreshes_on_first_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .and(query_param("corpid", "wx-corp"))
            .and(query_param("corpsecret", "secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"TOKEN-1","expires_in":7200
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let tok = api.access_token().await.unwrap();
        assert_eq!(tok, "TOKEN-1");
        // Second call returns from cache.
        let tok = api.access_token().await.unwrap();
        assert_eq!(tok, "TOKEN-1");
    }

    #[tokio::test]
    async fn access_token_surfaces_auth_errcode() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":40014,"errmsg":"bad"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.access_token().await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn access_token_returns_transport_on_http_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.access_token().await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_addresses_touser() {
        let server = MockServer::start().await;
        // First the token, then the send.
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(query_param("access_token", "T"))
            .and(wiremock::matchers::body_partial_json(json!({
                "touser":"alice","msgtype":"text","agentid":1,
                "text":{"content":"hi"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"errmsg":"ok","msgid":"MSG-1"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_text(1, MessageTarget::User("alice"), "hi")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("MSG-1"));
    }

    #[tokio::test]
    async fn send_text_addresses_toparty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "toparty":"99","msgtype":"text","agentid":1
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"MSG-P"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_text(1, MessageTarget::Party("99"), "hi")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("MSG-P"));
    }

    #[tokio::test]
    async fn send_text_addresses_totag() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "totag":"7","msgtype":"text","agentid":1
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"MSG-T"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_text(1, MessageTarget::Tag("7"), "hi")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("MSG-T"));
    }

    #[tokio::test]
    async fn send_text_no_msgid_returns_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"errcode":0})))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_text(1, MessageTarget::User("a"), "hi")
            .await
            .unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn send_text_surfaces_auth_errcode() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        // For non-42001 auth errors we should NOT retry — single mount.
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":40014,"errmsg":"bad token"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.send_text(1, MessageTarget::User("a"), "hi").await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_retries_once_on_42001() {
        let server = MockServer::start().await;
        // Two token fetches expected: initial + post-invalidation.
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts_clone = attempts.clone();
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .respond_with(move |_: &wiremock::Request| {
                let n = attempts_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "errcode":42001,"errmsg":"access_token expired"
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "errcode":0,"msgid":"AFTER-RETRY"
                    }))
                }
            })
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_text(1, MessageTarget::User("a"), "hi")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("AFTER-RETRY"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn send_image_by_id_uses_image_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "msgtype":"image",
                "image":{"media_id":"MID"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"IMG-OK"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_image_by_id(1, MessageTarget::User("a"), "MID")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("IMG-OK"));
    }

    #[tokio::test]
    async fn send_file_by_id_uses_file_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "msgtype":"file",
                "file":{"media_id":"FID"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"FILE-OK"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_file_by_id(1, MessageTarget::User("a"), "FID")
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("FILE-OK"));
    }

    #[tokio::test]
    async fn send_template_card_uses_template_card_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "msgtype":"template_card",
                "template_card":{"card_type":"text_notice"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"CARD-OK"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_template_card(
                1,
                MessageTarget::User("a"),
                &json!({"card_type":"text_notice"}),
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("CARD-OK"));
    }

    #[tokio::test]
    async fn upload_media_returns_media_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/media/upload"))
            .and(query_param("access_token", "T"))
            .and(query_param("type", "file"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"type":"file","media_id":"MID-1","created_at":"1700000000"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .upload_media("file", "x.pdf", b"PDF".to_vec())
            .await
            .unwrap();
        assert_eq!(id, "MID-1");
    }

    #[tokio::test]
    async fn upload_media_image_type() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/media/upload"))
            .and(query_param("type", "image"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"media_id":"MID-IMG"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .upload_media("image", "x.png", b"PNG".to_vec())
            .await
            .unwrap();
        assert_eq!(id, "MID-IMG");
    }

    #[tokio::test]
    async fn upload_media_surfaces_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/media/upload"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.upload_media("file", "x", b"".to_vec()).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_media_5xx_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/media/upload"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.upload_media("file", "x", b"".to_vec()).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[test]
    fn message_target_apply_user() {
        let mut body = json!({});
        MessageTarget::User("alice").apply(&mut body);
        assert_eq!(body["touser"], "alice");
    }

    #[test]
    fn message_target_apply_party() {
        let mut body = json!({});
        MessageTarget::Party("99").apply(&mut body);
        assert_eq!(body["toparty"], "99");
    }

    #[test]
    fn message_target_apply_tag() {
        let mut body = json!({});
        MessageTarget::Tag("7").apply(&mut body);
        assert_eq!(body["totag"], "7");
    }

    #[test]
    fn url_helpers_trim_trailing_slash() {
        let a = WeChatApi::new("https://example.test/", "c", "s");
        assert_eq!(a.url("/x"), "https://example.test/x");
    }

    #[test]
    fn tokens_accessor_returns_inner() {
        let a = WeChatApi::new("https://x", "c", "s");
        let _ = a.tokens();
    }

    #[test]
    fn clone_and_debug_present() {
        let a = WeChatApi::new("https://x", "c", "s");
        let _ = a.clone();
        assert!(format!("{a:?}").contains("WeChatApi"));
    }

    #[test]
    fn default_api_base_constant() {
        assert_eq!(DEFAULT_API_BASE, "https://qyapi.weixin.qq.com");
    }
}
