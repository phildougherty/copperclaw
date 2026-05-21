//! Google Chat REST API client.
//!
//! Wraps the subset of `chat.googleapis.com` endpoints the adapter needs.
//! Authentication is a service-account-derived `OAuth2` bearer token supplied
//! verbatim via configuration — this client never refreshes it. Token
//! rotation is the operator's responsibility.

use ironclaw_channels_core::AdapterError;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

/// Response from a successful `spaces.messages.create` (and `update`) call.
///
/// Google Chat returns more fields than this; we only deserialize the bits
/// we use as the platform-side message id.
#[derive(Debug, Clone, Deserialize)]
pub struct MessageResponse {
    /// Full message resource name, e.g. `spaces/AAQ.../messages/123.456`.
    /// We surface this verbatim as `platform_message_id` because it
    /// includes the parent space and is what the edit / delete / reaction
    /// endpoints take.
    pub name: String,
}

/// Minimal Google Chat REST client.
#[derive(Debug, Clone)]
pub struct GchatApi {
    client: Client,
    api_base: String,
    bot_token: String,
}

impl GchatApi {
    /// Build a client using the configured token and base URL.
    #[must_use]
    pub fn new(api_base: impl Into<String>, bot_token: impl Into<String>) -> Self {
        Self::with_client(Client::new(), api_base, bot_token)
    }

    /// Construct with a caller-supplied `reqwest::Client` (useful for tests
    /// that want a shared connection pool or custom timeouts).
    #[must_use]
    pub fn with_client(
        client: Client,
        api_base: impl Into<String>,
        bot_token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_base: api_base.into(),
            bot_token: bot_token.into(),
        }
    }

    /// Compose a URL for a relative API path.
    ///
    /// `path` must begin with `/` and is appended after stripping any
    /// trailing slash from `api_base`.
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api_base.trim_end_matches('/'), path)
    }

    /// Send a plain text message to a space.
    ///
    /// `space` is the bare id portion of `spaces/<id>` (the resource path).
    /// Returns the response containing the full `name` of the created
    /// message.
    pub async fn send_text(
        &self,
        space: &str,
        text: &str,
    ) -> Result<MessageResponse, AdapterError> {
        let body = json!({ "text": text });
        let url = self.url(&format!("/v1/spaces/{space}/messages"));
        self.do_create(&url, &body, None).await
    }

    /// Send a threaded reply.
    ///
    /// Pairs `messageReplyOption=REPLY_MESSAGE_OR_FAIL` with a `thread.name`
    /// field in the body. `thread_name` must be a full thread resource
    /// path (`spaces/<id>/threads/<id>`).
    pub async fn send_threaded_text(
        &self,
        space: &str,
        thread_name: &str,
        text: &str,
    ) -> Result<MessageResponse, AdapterError> {
        let body = json!({
            "text": text,
            "thread": { "name": thread_name },
        });
        let url = self.url(&format!("/v1/spaces/{space}/messages"));
        self.do_create(&url, &body, Some(("messageReplyOption", "REPLY_MESSAGE_OR_FAIL")))
            .await
    }

    /// Send a card v2 message.
    ///
    /// `card` is the inner card object (everything that goes under
    /// `cardsV2[0].card`). `card_id` is the operator-chosen identifier
    /// (Google Chat requires one).
    pub async fn send_card(
        &self,
        space: &str,
        card_id: &str,
        card: &Value,
    ) -> Result<MessageResponse, AdapterError> {
        let body = json!({
            "cardsV2": [{
                "cardId": card_id,
                "card": card,
            }]
        });
        let url = self.url(&format!("/v1/spaces/{space}/messages"));
        self.do_create(&url, &body, None).await
    }

    /// Edit a previously-sent message's text.
    ///
    /// `message_name` is the full resource path returned by `send_text` /
    /// `send_card` (e.g. `spaces/AAQ.../messages/123`). Updates only the
    /// `text` field via the `updateMask` query parameter.
    pub async fn edit_text(
        &self,
        message_name: &str,
        text: &str,
    ) -> Result<MessageResponse, AdapterError> {
        let body = json!({ "text": text });
        let url = self.url(&format!("/v1/{message_name}"));
        let resp = self
            .client
            .put(url)
            .bearer_auth(&self.bot_token)
            .query(&[("updateMask", "text")])
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_gchat_json(resp).await?;
        let parsed: MessageResponse = serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("update message decode: {e}")))?;
        Ok(parsed)
    }

    /// Delete a message by full resource name.
    pub async fn delete_message(&self, message_name: &str) -> Result<(), AdapterError> {
        let url = self.url(&format!("/v1/{message_name}"));
        let resp = self
            .client
            .delete(url)
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_gchat_json_optional(resp).await?;
        Ok(())
    }

    /// Add a reaction (unicode emoji) to a message.
    ///
    /// `unicode_char` is the literal character (from
    /// [`crate::emoji::emoji_codepoint`]). Google Chat expects the bytes of
    /// that character; we let `serde_json` encode them.
    pub async fn create_reaction(
        &self,
        message_name: &str,
        unicode_char: char,
    ) -> Result<(), AdapterError> {
        let body = json!({
            "emoji": { "unicode": unicode_char.to_string() }
        });
        let url = self.url(&format!("/v1/{message_name}/reactions"));
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_gchat_json_optional(resp).await?;
        Ok(())
    }

    async fn do_create(
        &self,
        url: &str,
        body: &Value,
        extra_query: Option<(&str, &str)>,
    ) -> Result<MessageResponse, AdapterError> {
        let mut req = self
            .client
            .post(url)
            .bearer_auth(&self.bot_token)
            .json(body);
        if let Some((k, v)) = extra_query {
            req = req.query(&[(k, v)]);
        }
        let resp = req.send().await.map_err(|e| transport(&e))?;
        let value = read_gchat_json(resp).await?;
        let parsed: MessageResponse = serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("create message decode: {e}")))?;
        Ok(parsed)
    }
}

fn transport(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(err.to_string())
}

/// Read a Google Chat JSON response, classifying transport/auth/rate errors.
///
/// Successful 2xx responses must be valid JSON; non-2xx responses are
/// lifted into typed `AdapterError`s.
async fn read_gchat_json(resp: reqwest::Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after = retry_after_seconds(&resp);
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AdapterError::Rate { retry_after });
    }
    if !status.is_success() {
        return Err(map_http_error(status, resp).await);
    }
    let value: Value = resp
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("gchat response not JSON: {e}")))?;
    Ok(value)
}

/// Like [`read_gchat_json`] but tolerates an empty body on success
/// (e.g. `DELETE` returns no body). Returns `Value::Null` on empty body.
async fn read_gchat_json_optional(resp: reqwest::Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after = retry_after_seconds(&resp);
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AdapterError::Rate { retry_after });
    }
    if !status.is_success() {
        return Err(map_http_error(status, resp).await);
    }
    let text = resp.text().await.unwrap_or_default();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|e| AdapterError::Transport(format!("gchat response not JSON: {e}")))
}

fn retry_after_seconds(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// Map a non-2xx HTTP response to an [`AdapterError`].
///
/// Inspects the response body for Google's `{"error": {"message": ...}}`
/// shape so the message reaches the caller. Public so the adapter tests can
/// exercise it directly.
pub(crate) async fn map_http_error(
    status: StatusCode,
    resp: reqwest::Response,
) -> AdapterError {
    let body = resp.text().await.unwrap_or_default();
    let message = extract_error_message(&body);
    classify_http_status(status, &message, &body)
}

/// Pull the human-readable error message from a Google error envelope, or
/// fall back to the raw body.
pub(crate) fn extract_error_message(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(body) {
        Ok(v) => v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map_or_else(|| body.to_owned(), str::to_owned),
        Err(_) => body.to_owned(),
    }
}

/// Map an HTTP status code to the appropriate adapter error variant.
///
/// 401/403 → `Auth`, 404 → `BadRequest`, 400 → `BadRequest`, 5xx → `Transport`.
/// Anything else falls through to Transport.
pub(crate) fn classify_http_status(
    status: StatusCode,
    message: &str,
    raw_body: &str,
) -> AdapterError {
    let surface = if message.is_empty() { raw_body } else { message };
    match status.as_u16() {
        401 | 403 => AdapterError::Auth(format!("{status}: {surface}")),
        400 | 404 | 422 => AdapterError::BadRequest(format!("{status}: {surface}")),
        s if (500..=599).contains(&s) => AdapterError::Transport(format!("{status}: {surface}")),
        _ => AdapterError::Transport(format!("{status}: {surface}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(server: &MockServer) -> GchatApi {
        GchatApi::new(server.uri(), "tok-test")
    }

    #[test]
    fn url_handles_trailing_slash() {
        let a = GchatApi::new("https://example.test/", "tok");
        assert_eq!(a.url("/v1/x"), "https://example.test/v1/x");
        let a = GchatApi::new("https://example.test", "tok");
        assert_eq!(a.url("/v1/x"), "https://example.test/v1/x");
    }

    #[test]
    fn extract_error_message_with_google_envelope() {
        let body = r#"{"error":{"code":400,"message":"bad thread","status":"INVALID_ARGUMENT"}}"#;
        assert_eq!(extract_error_message(body), "bad thread");
    }

    #[test]
    fn extract_error_message_falls_back_to_body() {
        assert_eq!(extract_error_message("not json"), "not json");
        assert_eq!(extract_error_message(""), "");
        assert_eq!(extract_error_message("{}"), "{}");
    }

    #[test]
    fn classify_http_status_codes() {
        assert!(matches!(
            classify_http_status(StatusCode::UNAUTHORIZED, "nope", ""),
            AdapterError::Auth(_)
        ));
        assert!(matches!(
            classify_http_status(StatusCode::FORBIDDEN, "nope", ""),
            AdapterError::Auth(_)
        ));
        assert!(matches!(
            classify_http_status(StatusCode::BAD_REQUEST, "x", ""),
            AdapterError::BadRequest(_)
        ));
        assert!(matches!(
            classify_http_status(StatusCode::NOT_FOUND, "x", ""),
            AdapterError::BadRequest(_)
        ));
        assert!(matches!(
            classify_http_status(StatusCode::UNPROCESSABLE_ENTITY, "x", ""),
            AdapterError::BadRequest(_)
        ));
        assert!(matches!(
            classify_http_status(StatusCode::INTERNAL_SERVER_ERROR, "x", ""),
            AdapterError::Transport(_)
        ));
        assert!(matches!(
            classify_http_status(StatusCode::BAD_GATEWAY, "x", ""),
            AdapterError::Transport(_)
        ));
        assert!(matches!(
            classify_http_status(StatusCode::IM_A_TEAPOT, "x", ""),
            AdapterError::Transport(_)
        ));
    }

    #[test]
    fn classify_uses_raw_body_when_message_empty() {
        match classify_http_status(StatusCode::BAD_REQUEST, "", "raw body") {
            AdapterError::BadRequest(m) => assert!(m.contains("raw body")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .and(header("authorization", "Bearer tok-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/ABC/messages/100.200"}),
            ))
            .mount(&server)
            .await;
        let resp = api(&server).send_text("ABC", "hi").await.unwrap();
        assert_eq!(resp.name, "spaces/ABC/messages/100.200");
    }

    #[tokio::test]
    async fn send_threaded_text_uses_reply_query_and_thread_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .and(query_param("messageReplyOption", "REPLY_MESSAGE_OR_FAIL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/ABC/messages/200.300"}),
            ))
            .mount(&server)
            .await;
        let resp = api(&server)
            .send_threaded_text("ABC", "spaces/ABC/threads/T1", "hello")
            .await
            .unwrap();
        assert_eq!(resp.name, "spaces/ABC/messages/200.300");
    }

    #[tokio::test]
    async fn send_card_routes_to_cards_v2() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/ABC/messages/CARD"}),
            ))
            .mount(&server)
            .await;
        let resp = api(&server)
            .send_card("ABC", "card-1", &serde_json::json!({"header":{"title":"x"}}))
            .await
            .unwrap();
        assert_eq!(resp.name, "spaces/ABC/messages/CARD");
    }

    #[tokio::test]
    async fn edit_text_uses_update_mask() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/spaces/ABC/messages/100"))
            .and(query_param("updateMask", "text"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"name": "spaces/ABC/messages/100"}),
            ))
            .mount(&server)
            .await;
        let resp = api(&server)
            .edit_text("spaces/ABC/messages/100", "edited")
            .await
            .unwrap();
        assert_eq!(resp.name, "spaces/ABC/messages/100");
    }

    #[tokio::test]
    async fn delete_message_succeeds_with_empty_body() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/spaces/ABC/messages/100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        api(&server)
            .delete_message("spaces/ABC/messages/100")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_reaction_posts_to_reactions_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages/100/reactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let c = char::from_u32(0x1F44D).unwrap();
        api(&server)
            .create_reaction("spaces/ABC/messages/100", c)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn send_text_401_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(
                serde_json::json!({"error":{"code":401,"message":"bad token","status":"UNAUTHENTICATED"}}),
            ))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::Auth(m)) => assert!(m.contains("bad token")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_403_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::Auth(m)) => assert!(m.contains("denied")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_429_is_rate_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(7)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_429_without_retry_after_is_rate_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, None),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_500_is_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("boom")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(404).set_body_json(
                serde_json::json!({"error":{"code":404,"message":"space not found","status":"NOT_FOUND"}}),
            ))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("space not found")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_400_is_bad_request_with_surfaced_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(
                serde_json::json!({"error":{"code":400,"message":"missing text","status":"INVALID_ARGUMENT"}}),
            ))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("missing text")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_decode_failure_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("not JSON")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_missing_name_field_decode_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        match api(&server).send_text("ABC", "x").await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("decode")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_429_is_rate() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/spaces/ABC/messages/1"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "2"))
            .mount(&server)
            .await;
        match api(&server).delete_message("spaces/ABC/messages/1").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(2)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_500_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/spaces/ABC/messages/1"))
            .respond_with(ResponseTemplate::new(500).set_body_string("err"))
            .mount(&server)
            .await;
        match api(&server).delete_message("spaces/ABC/messages/1").await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reaction_401_is_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/ABC/messages/1/reactions"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let c = char::from_u32(0x1F44D).unwrap();
        match api(&server)
            .create_reaction("spaces/ABC/messages/1", c)
            .await
        {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn edit_text_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/spaces/ABC/messages/1"))
            .respond_with(ResponseTemplate::new(404).set_body_json(
                serde_json::json!({"error":{"message":"gone"}}),
            ))
            .mount(&server)
            .await;
        match api(&server)
            .edit_text("spaces/ABC/messages/1", "new")
            .await
        {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("gone")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn edit_text_decode_failure_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/spaces/ABC/messages/1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        match api(&server)
            .edit_text("spaces/ABC/messages/1", "new")
            .await
        {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_error_on_connection_failure() {
        // Build an API pointing at a closed port to provoke a reqwest error.
        let api = GchatApi::new("http://127.0.0.1:1", "tok");
        match api.send_text("ABC", "x").await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[test]
    fn api_clone_and_debug() {
        let a = GchatApi::new("https://example.test", "tok");
        let _ = a.clone();
        assert!(format!("{a:?}").contains("tok"));
    }

    #[test]
    fn message_response_decode() {
        let v = serde_json::json!({"name": "spaces/X/messages/42"});
        let parsed: MessageResponse = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.name, "spaces/X/messages/42");
    }
}
