//! REST client for the Discord HTTP API.
//!
//! `DiscordRest` wraps a `reqwest::Client` with the bot token and base URL.
//! All public methods map response status codes to `AdapterError` variants
//! per the Discord rate-limit contract:
//!
//! - `200`/`201`/`204` -> `Ok`.
//! - `401` -> `AdapterError::Auth`.
//! - `429` -> `AdapterError::Rate { retry_after }`, reading either
//!   `Retry-After` or `X-RateLimit-Reset-After` (seconds, may be fractional).
//! - `400` / `404` / other `4xx` (except auth and rate) -> `AdapterError::BadRequest`.
//! - `5xx` -> `AdapterError::Transport`.
//! - Network failure -> `AdapterError::Transport`.

use ironclaw_channels_core::AdapterError;
use ironclaw_types::OutboundFile;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Response, StatusCode};
use serde_json::{Value, json};

/// HTTP client wrapper for the Discord REST API.
#[derive(Debug, Clone)]
pub struct DiscordRest {
    client: Client,
    api_base: String,
    bot_token: String,
}

impl DiscordRest {
    /// Build a new REST client with the given bot token and API base.
    pub fn new(client: Client, bot_token: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            client,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            bot_token: bot_token.into(),
        }
    }

    fn auth_headers(&self) -> Result<HeaderMap, AdapterError> {
        let mut h = HeaderMap::new();
        let val = HeaderValue::from_str(&format!("Bot {}", self.bot_token))
            .map_err(|e| AdapterError::BadRequest(format!("invalid bot token: {e}")))?;
        h.insert(AUTHORIZATION, val);
        Ok(h)
    }

    /// `POST /channels/{channel_id}/messages`.
    ///
    /// When `files` is non-empty, the request becomes `multipart/form-data`
    /// with a `payload_json` part plus one `files[i]` part per attachment.
    pub async fn post_message(
        &self,
        channel_id: &str,
        text: &str,
        files: &[OutboundFile],
    ) -> Result<String, AdapterError> {
        let url = format!("{}/channels/{}/messages", self.api_base, channel_id);
        let payload = json!({ "content": text });

        let req = if files.is_empty() {
            self.client
                .post(&url)
                .headers(self.auth_headers()?)
                .header(CONTENT_TYPE, "application/json")
                .body(payload.to_string())
        } else {
            let mut form = Form::new().text("payload_json", payload.to_string());
            for (idx, f) in files.iter().enumerate() {
                let part = Part::bytes(f.data.clone()).file_name(f.filename.clone());
                form = form.part(format!("files[{idx}]"), part);
            }
            self.client.post(&url).headers(self.auth_headers()?).multipart(form)
        };

        let resp = req
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("post_message: {e}")))?;
        let resp = check_response(resp).await?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| AdapterError::Transport(format!("post_message decode: {e}")))?;
        body.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| AdapterError::Transport("post_message: missing id in response".into()))
    }

    /// `POST /channels/{channel_id}/messages` with a structured Discord
    /// payload (`embeds`, `components`, optional `content`).
    ///
    /// Used by [`crate::adapter::DiscordAdapter::deliver_card`] to ship a
    /// canonical [`ironclaw_channels_core::Card`] as a Discord embed plus
    /// `ActionRow` components. The full payload is sent verbatim so the
    /// caller has full control over the JSON shape; we just wrap the
    /// HTTP / auth / status-mapping plumbing.
    ///
    /// Returns the platform message id (`id` field from the response).
    pub async fn post_message_payload(
        &self,
        channel_id: &str,
        payload: &Value,
    ) -> Result<String, AdapterError> {
        let url = format!("{}/channels/{}/messages", self.api_base, channel_id);
        let resp = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("post_message_payload: {e}")))?;
        let resp = check_response(resp).await?;
        let body: Value = resp.json().await.map_err(|e| {
            AdapterError::Transport(format!("post_message_payload decode: {e}"))
        })?;
        body.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                AdapterError::Transport("post_message_payload: missing id in response".into())
            })
    }

    /// `POST /interactions/{interaction_id}/{token}/callback`.
    ///
    /// ACK an inbound component interaction so the user's client clears
    /// the spinner on the tapped button. We always send type `6`
    /// (`DEFERRED_UPDATE_MESSAGE`) — Discord treats this as "I'm handling
    /// it; don't expect a new message, but the spinner goes away". The
    /// agent-side reply (if any) flows through the normal `deliver`
    /// outbound path so we don't need to use type `4` here.
    pub async fn create_interaction_response_ack(
        &self,
        interaction_id: &str,
        interaction_token: &str,
    ) -> Result<(), AdapterError> {
        let url = format!(
            "{}/interactions/{}/{}/callback",
            self.api_base, interaction_id, interaction_token
        );
        // Type 6 = DEFERRED_UPDATE_MESSAGE — clears the spinner without
        // forcing us to send a message body in the callback response.
        let payload = json!({ "type": 6 });
        // Note: this endpoint does NOT require the bot Authorization header
        // (the interaction token authenticates the response). We still pass
        // it for parity with the rest of the client; Discord ignores it
        // here.
        let resp = self
            .client
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await
            .map_err(|e| {
                AdapterError::Transport(format!("create_interaction_response: {e}"))
            })?;
        check_response(resp).await?;
        Ok(())
    }

    /// `PATCH /channels/{channel_id}/messages/{message_id}`.
    pub async fn patch_message(
        &self,
        channel_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), AdapterError> {
        let url = format!(
            "{}/channels/{}/messages/{}",
            self.api_base, channel_id, message_id
        );
        let resp = self
            .client
            .patch(&url)
            .headers(self.auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body(json!({ "content": text }).to_string())
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("patch_message: {e}")))?;
        check_response(resp).await?;
        Ok(())
    }

    /// `PATCH /channels/{channel_id}/messages/{message_id}` with an
    /// arbitrary JSON payload (embeds, components, etc.). Mirrors
    /// [`Self::post_message_payload`] for the edit path; used by
    /// `deliver_todo_list` to update the embed without losing it.
    pub async fn patch_message_payload(
        &self,
        channel_id: &str,
        message_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), AdapterError> {
        let url = format!(
            "{}/channels/{}/messages/{}",
            self.api_base, channel_id, message_id
        );
        let resp = self
            .client
            .patch(&url)
            .headers(self.auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body(payload.to_string())
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("patch_message_payload: {e}")))?;
        check_response(resp).await?;
        Ok(())
    }

    /// `PUT /channels/{channel_id}/pins/{message_id}`. Used by
    /// `deliver_todo_list` on first emit. Best-effort: bots often
    /// lack the `MANAGE_MESSAGES` permission required to pin, in
    /// which case Discord returns `50013` (missing permissions) and
    /// the caller swallows.
    pub async fn put_pin(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), AdapterError> {
        let url = format!(
            "{}/channels/{}/pins/{}",
            self.api_base, channel_id, message_id
        );
        let resp = self
            .client
            .put(&url)
            .headers(self.auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body("")
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("put_pin: {e}")))?;
        check_response(resp).await?;
        Ok(())
    }

    /// `DELETE /channels/{channel_id}/pins/{message_id}`. Called
    /// when a [`TodoList`](ironclaw_channels_core::TodoList) transitions
    /// to fully-completed. Same permission-failure caveat as
    /// [`Self::put_pin`].
    pub async fn delete_pin(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), AdapterError> {
        let url = format!(
            "{}/channels/{}/pins/{}",
            self.api_base, channel_id, message_id
        );
        let resp = self
            .client
            .delete(&url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("delete_pin: {e}")))?;
        check_response(resp).await?;
        Ok(())
    }

    /// `PUT /channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me`.
    pub async fn put_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        let encoded = urlencoding(emoji);
        let url = format!(
            "{}/channels/{}/messages/{}/reactions/{}/@me",
            self.api_base, channel_id, message_id, encoded
        );
        let resp = self
            .client
            .put(&url)
            .headers(self.auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body("")
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("put_reaction: {e}")))?;
        check_response(resp).await?;
        Ok(())
    }

    /// `POST /channels/{channel_id}/typing`.
    pub async fn post_typing(&self, channel_id: &str) -> Result<(), AdapterError> {
        let url = format!("{}/channels/{}/typing", self.api_base, channel_id);
        let resp = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .body("")
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("post_typing: {e}")))?;
        check_response(resp).await?;
        Ok(())
    }

    /// `POST /users/@me/channels { recipient_id }`.
    ///
    /// Returns the platform id (`id`) of the freshly opened DM channel.
    pub async fn open_dm(&self, recipient_id: &str) -> Result<String, AdapterError> {
        let url = format!("{}/users/@me/channels", self.api_base);
        let resp = self
            .client
            .post(&url)
            .headers(self.auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body(json!({ "recipient_id": recipient_id }).to_string())
            .send()
            .await
            .map_err(|e| AdapterError::Transport(format!("open_dm: {e}")))?;
        let resp = check_response(resp).await?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| AdapterError::Transport(format!("open_dm decode: {e}")))?;
        body.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| AdapterError::Transport("open_dm: missing id in response".into()))
    }
}

/// Map an HTTP response to an `AdapterError` based on status code, returning
/// the response unchanged on success (2xx).
async fn check_response(resp: Response) -> Result<Response, AdapterError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            let body = body_snippet(resp).await;
            Err(AdapterError::Auth(format!("discord auth failed: {body}")))
        }
        StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = retry_after_seconds(&resp);
            Err(AdapterError::Rate { retry_after })
        }
        s if s.is_client_error() => {
            let body = body_snippet(resp).await;
            Err(AdapterError::BadRequest(format!(
                "discord {status}: {body}"
            )))
        }
        s if s.is_server_error() => {
            let body = body_snippet(resp).await;
            Err(AdapterError::Transport(format!(
                "discord {status}: {body}"
            )))
        }
        _ => {
            let body = body_snippet(resp).await;
            Err(AdapterError::Transport(format!(
                "discord {status}: {body}"
            )))
        }
    }
}

/// Read the `Retry-After` header (preferred) or `X-RateLimit-Reset-After`,
/// converting any fractional seconds to a whole-second `u64`.
fn retry_after_seconds(resp: &Response) -> Option<u64> {
    let h = resp.headers();
    h.get("retry-after")
        .or_else(|| h.get("x-ratelimit-reset-after"))
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .map(seconds_ceil)
}

/// Round a fractional-seconds duration up to the next whole second, clamping
/// at 0 below and `u64::MAX` above. Pulled out for testability.
fn seconds_ceil(secs: f64) -> u64 {
    if !secs.is_finite() || secs <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_precision_loss)]
    let max_f = u64::MAX as f64;
    let ceiled = secs.ceil();
    if ceiled >= max_f {
        return u64::MAX;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = ceiled as u64;
    out
}

async fn body_snippet(resp: Response) -> String {
    match resp.text().await {
        Ok(t) => t.chars().take(256).collect(),
        Err(_) => String::new(),
    }
}

/// Minimal percent-encoder for path segments. Discord reaction emoji can be
/// non-ASCII (unicode emoji or `name:id` for custom). We escape everything
/// outside the unreserved-character set.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~' | b':');
        if unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> DiscordRest {
        DiscordRest::new(Client::new(), "test-token", server.uri())
    }

    #[tokio::test]
    async fn post_message_happy_path_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .and(header("authorization", "Bot test-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "id": "m9" })),
            )
            .mount(&server)
            .await;
        let r = client(&server);
        let id = r.post_message("c1", "hi", &[]).await.unwrap();
        assert_eq!(id, "m9");
    }

    #[tokio::test]
    async fn post_message_multipart_when_files_present() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "id": "m10" })),
            )
            .mount(&server)
            .await;
        let r = client(&server);
        let files = vec![OutboundFile {
            filename: "a.txt".into(),
            data: b"hello".to_vec(),
        }];
        let id = r.post_message("c1", "see attached", &files).await.unwrap();
        assert_eq!(id, "m10");
    }

    #[tokio::test]
    async fn post_message_missing_id_in_response_is_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn post_message_invalid_json_is_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn post_message_401_maps_to_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn post_message_403_maps_to_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn post_message_429_reads_retry_after_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(
                ResponseTemplate::new(429).insert_header("retry-after", "2.5"),
            )
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(3)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_429_falls_back_to_x_ratelimit_reset_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("x-ratelimit-reset-after", "7"),
            )
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(7)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_429_without_header_yields_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert!(retry_after.is_none()),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_400_maps_to_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn post_message_500_maps_to_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_message("c1", "hi", &[]).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn patch_message_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/channels/c1/messages/m9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let r = client(&server);
        r.patch_message("c1", "m9", "updated").await.unwrap();
    }

    #[tokio::test]
    async fn patch_message_propagates_errors() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/channels/c1/messages/m9"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.patch_message("c1", "m9", "updated").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn put_reaction_url_encodes_emoji() {
        let server = MockServer::start().await;
        // Smiley face emoji "\u{1F600}" -> %F0%9F%98%80
        Mock::given(method("PUT"))
            .and(path("/channels/c1/messages/m9/reactions/%F0%9F%98%80/@me"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let r = client(&server);
        r.put_reaction("c1", "m9", "\u{1F600}").await.unwrap();
    }

    #[tokio::test]
    async fn put_reaction_custom_name_id_form() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/channels/c1/messages/m9/reactions/thumbs:123/@me"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let r = client(&server);
        r.put_reaction("c1", "m9", "thumbs:123").await.unwrap();
    }

    #[tokio::test]
    async fn post_typing_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/typing"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let r = client(&server);
        r.post_typing("c1").await.unwrap();
    }

    #[tokio::test]
    async fn post_typing_5xx_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/typing"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.post_typing("c1").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn open_dm_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/@me/channels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "dm-1"})))
            .mount(&server)
            .await;
        let r = client(&server);
        let id = r.open_dm("u-7").await.unwrap();
        assert_eq!(id, "dm-1");
    }

    #[tokio::test]
    async fn open_dm_missing_id_is_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/@me/channels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.open_dm("u-7").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn open_dm_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/users/@me/channels"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r.open_dm("u-7").await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn transport_error_on_unreachable_host() {
        // Bind to a localhost address that nothing is listening on.
        let r = DiscordRest::new(Client::new(), "tok", "http://127.0.0.1:1");
        let err = r.post_typing("c1").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn urlencoding_passes_through_alphanumeric_and_colon() {
        assert_eq!(urlencoding("abc:123"), "abc:123");
    }

    #[test]
    fn urlencoding_escapes_slash_and_unicode() {
        let s = urlencoding("/a b");
        assert_eq!(s, "%2Fa%20b");
    }

    #[test]
    fn debug_and_clone_round_trip() {
        let r = DiscordRest::new(Client::new(), "t", "http://x");
        let r2 = r.clone();
        let dbg = format!("{r2:?}");
        assert!(dbg.contains("DiscordRest"));
    }

    #[test]
    fn trim_trailing_slash_on_base() {
        let r = DiscordRest::new(Client::new(), "t", "http://x/api/v10/");
        assert_eq!(r.api_base, "http://x/api/v10");
    }

    #[test]
    fn seconds_ceil_rounds_up_fractional() {
        assert_eq!(seconds_ceil(2.5), 3);
        assert_eq!(seconds_ceil(0.1), 1);
        assert_eq!(seconds_ceil(7.0), 7);
    }

    #[test]
    fn seconds_ceil_returns_zero_for_negative_or_nan() {
        assert_eq!(seconds_ceil(-1.0), 0);
        assert_eq!(seconds_ceil(0.0), 0);
        assert_eq!(seconds_ceil(f64::NAN), 0);
    }

    #[tokio::test]
    async fn post_message_payload_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .and(header("authorization", "Bot test-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "m-card-1"})),
            )
            .mount(&server)
            .await;
        let r = client(&server);
        let payload = json!({
            "embeds": [{"title": "T"}],
            "components": []
        });
        let id = r.post_message_payload("c1", &payload).await.unwrap();
        assert_eq!(id, "m-card-1");
    }

    #[tokio::test]
    async fn post_message_payload_400_maps_to_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid embed"))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r
            .post_message_payload("c1", &json!({"embeds": []}))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn create_interaction_response_ack_posts_type_six() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/interactions/int-1/tok-1/callback"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let r = client(&server);
        r.create_interaction_response_ack("int-1", "tok-1")
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path() == "/interactions/int-1/tok-1/callback")
            .expect("callback request");
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        // Type 6 = DEFERRED_UPDATE_MESSAGE.
        assert_eq!(body["type"], 6);
    }

    #[tokio::test]
    async fn create_interaction_response_ack_propagates_5xx_as_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/interactions/int-1/tok-1/callback"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let r = client(&server);
        let err = r
            .create_interaction_response_ack("int-1", "tok-1")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn seconds_ceil_clamps_at_u64_max() {
        assert_eq!(seconds_ceil(f64::INFINITY), 0);
        // Anything beyond u64::MAX as f64 saturates.
        let huge = 1.0e30;
        assert_eq!(seconds_ceil(huge), u64::MAX);
    }
}
