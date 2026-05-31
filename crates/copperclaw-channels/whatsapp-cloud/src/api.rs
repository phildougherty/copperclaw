//! Minimal Graph API client for the `WhatsApp` Cloud channel.
//!
//! Wraps the endpoints the adapter needs:
//!
//! - `POST /{pnid}/messages` for text, replies (via `context`), document,
//!   read receipts, and reactions.
//! - `POST /{pnid}/media` (multipart) for uploading attachment bytes.
//!
//! ## Error mapping
//!
//! - HTTP `401` / `403` -> [`AdapterError::Auth`].
//! - HTTP `429` -> [`AdapterError::Rate`] (honoring `Retry-After`).
//! - HTTP `4xx` other than `429` -> [`AdapterError::BadRequest`].
//! - HTTP `5xx` / network failure -> [`AdapterError::Transport`].
//!
//! Meta returns errors with a top-level `error` object:
//!
//! ```json
//! {"error": {"message": "...", "type": "...", "code": 190, "fbtrace_id": "..."}}
//! ```
//!
//! `code` `190` and the `200..=299` range are treated as Auth regardless of
//! the HTTP status. Codes `4`, `80004`, and `130429` are treated as Rate.

use copperclaw_channels_core::AdapterError;
use reqwest::multipart::{Form, Part};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

/// Default Graph API base used when callers don't override it.
pub const DEFAULT_GRAPH_BASE: &str = "https://graph.facebook.com/v18.0";

/// `messages[0].id` returned from a successful send.
#[derive(Debug, Clone, Deserialize)]
struct MessageSendResponse {
    #[serde(default)]
    messages: Vec<SentMessageId>,
}

#[derive(Debug, Clone, Deserialize)]
struct SentMessageId {
    id: String,
}

/// Response body of a successful media upload.
#[derive(Debug, Clone, Deserialize)]
struct MediaUploadResponse {
    id: String,
}

/// Minimal Graph API client used by the adapter.
#[derive(Debug, Clone)]
pub struct WhatsappCloudApi {
    client: Client,
    graph_base: String,
    access_token: String,
}

impl WhatsappCloudApi {
    /// Construct a client. Uses a fresh `reqwest::Client`.
    #[must_use]
    pub fn new(graph_base: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self::with_client(Client::new(), graph_base, access_token)
    }

    /// Construct with a caller-supplied `reqwest::Client`.
    #[must_use]
    pub fn with_client(
        client: Client,
        graph_base: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            graph_base: graph_base.into().trim_end_matches('/').to_owned(),
            access_token: access_token.into(),
        }
    }

    fn messages_url(&self, phone_number_id: &str) -> String {
        format!("{}/{phone_number_id}/messages", self.graph_base)
    }

    fn media_url(&self, phone_number_id: &str) -> String {
        format!("{}/{phone_number_id}/media", self.graph_base)
    }

    /// Send a plain text message. Returns the platform-side `wamid`.
    pub async fn send_text(
        &self,
        phone_number_id: &str,
        recipient: &str,
        text: &str,
    ) -> Result<String, AdapterError> {
        let body = json!({
            "messaging_product": "whatsapp",
            "to": recipient,
            "type": "text",
            "text": {"body": text},
        });
        self.post_message(phone_number_id, &body).await
    }

    /// Send a text reply to a previous message. `WhatsApp` threads are flat
    /// replies addressed by `context.message_id`.
    pub async fn send_text_reply(
        &self,
        phone_number_id: &str,
        recipient: &str,
        text: &str,
        in_reply_to: &str,
    ) -> Result<String, AdapterError> {
        let body = json!({
            "messaging_product": "whatsapp",
            "to": recipient,
            "type": "text",
            "text": {"body": text},
            "context": {"message_id": in_reply_to},
        });
        self.post_message(phone_number_id, &body).await
    }

    /// Send a document referenced by a public URL.
    pub async fn send_document_by_link(
        &self,
        phone_number_id: &str,
        recipient: &str,
        link: &str,
        filename: Option<&str>,
        in_reply_to: Option<&str>,
    ) -> Result<String, AdapterError> {
        let mut document = json!({"link": link});
        if let Some(name) = filename {
            document["filename"] = Value::String(name.to_owned());
        }
        let mut body = json!({
            "messaging_product": "whatsapp",
            "to": recipient,
            "type": "document",
            "document": document,
        });
        if let Some(reply) = in_reply_to {
            body["context"] = json!({"message_id": reply});
        }
        self.post_message(phone_number_id, &body).await
    }

    /// Send a document referenced by a media id (after a prior upload).
    pub async fn send_document_by_id(
        &self,
        phone_number_id: &str,
        recipient: &str,
        media_id: &str,
        filename: Option<&str>,
        in_reply_to: Option<&str>,
    ) -> Result<String, AdapterError> {
        let mut document = json!({"id": media_id});
        if let Some(name) = filename {
            document["filename"] = Value::String(name.to_owned());
        }
        let mut body = json!({
            "messaging_product": "whatsapp",
            "to": recipient,
            "type": "document",
            "document": document,
        });
        if let Some(reply) = in_reply_to {
            body["context"] = json!({"message_id": reply});
        }
        self.post_message(phone_number_id, &body).await
    }

    /// Upload media bytes and return the media id.
    pub async fn upload_media(
        &self,
        phone_number_id: &str,
        filename: &str,
        mime_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String, AdapterError> {
        let part = Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime_type)
            .map_err(|e| AdapterError::BadRequest(format!("invalid mime type {mime_type}: {e}")))?;
        let form = Form::new()
            .text("messaging_product", "whatsapp")
            .text("type", mime_type.to_owned())
            .part("file", part);
        let resp = self
            .client
            .post(self.media_url(phone_number_id))
            .bearer_auth(&self.access_token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_graph_json(resp).await?;
        let parsed: MediaUploadResponse = serde_json::from_value(value).map_err(|e| {
            AdapterError::Transport(format!("whatsapp-cloud media upload decode: {e}"))
        })?;
        Ok(parsed.id)
    }

    /// Mark a previously-received message as read. Used as the closest
    /// reasonable approximation of "typing" since `WhatsApp` Cloud has no
    /// real typing API.
    pub async fn mark_read(
        &self,
        phone_number_id: &str,
        message_id: &str,
    ) -> Result<(), AdapterError> {
        let body = json!({
            "messaging_product": "whatsapp",
            "status": "read",
            "message_id": message_id,
        });
        let resp = self
            .client
            .post(self.messages_url(phone_number_id))
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_graph_json(resp).await?;
        Ok(())
    }

    /// Send a reaction. An empty `emoji` removes the existing reaction on
    /// the target message.
    pub async fn send_reaction(
        &self,
        phone_number_id: &str,
        recipient: &str,
        target_message_id: &str,
        emoji: &str,
    ) -> Result<String, AdapterError> {
        let body = json!({
            "messaging_product": "whatsapp",
            "to": recipient,
            "type": "reaction",
            "reaction": {
                "message_id": target_message_id,
                "emoji": emoji,
            },
        });
        self.post_message(phone_number_id, &body).await
    }

    async fn post_message(
        &self,
        phone_number_id: &str,
        body: &Value,
    ) -> Result<String, AdapterError> {
        let resp = self
            .client
            .post(self.messages_url(phone_number_id))
            .bearer_auth(&self.access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_graph_json(resp).await?;
        let parsed: MessageSendResponse = serde_json::from_value(value).map_err(|e| {
            AdapterError::Transport(format!("whatsapp-cloud message send decode: {e}"))
        })?;
        parsed
            .messages
            .into_iter()
            .next()
            .map(|m| m.id)
            .ok_or_else(|| {
                AdapterError::Transport(
                    "whatsapp-cloud message send returned no message id".into(),
                )
            })
    }
}

fn transport(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(err.to_string())
}

async fn read_graph_json(resp: reqwest::Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body_text = resp.text().await.unwrap_or_default();
    classify_response(status, retry_after, &body_text)
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

    // Pull error sub-object code, if any.
    let (err_code, err_message) = parsed
        .as_ref()
        .and_then(|v| v.get("error"))
        .map_or((None, None), |err| {
            let code = err.get("code").and_then(Value::as_i64);
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (code, msg)
        });

    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AdapterError::Rate { retry_after });
    }
    if matches!(err_code, Some(c) if is_rate_code(c)) {
        return Err(AdapterError::Rate { retry_after });
    }
    if status == StatusCode::UNAUTHORIZED
        || status == StatusCode::FORBIDDEN
        || matches!(err_code, Some(c) if is_auth_code(c))
    {
        let msg = err_message.unwrap_or_else(|| format!("http {status}"));
        return Err(AdapterError::Auth(msg));
    }
    if status.is_client_error() {
        let msg = err_message.unwrap_or_else(|| format!("http {status}"));
        return Err(AdapterError::BadRequest(msg));
    }
    if status.is_server_error() {
        let msg = err_message.unwrap_or_else(|| format!("http {status}"));
        return Err(AdapterError::Transport(msg));
    }
    if !status.is_success() {
        // Defensive: 3xx / unexpected.
        return Err(AdapterError::Transport(format!(
            "unexpected status {status}"
        )));
    }

    // Success path.
    parsed.ok_or_else(|| {
        AdapterError::Transport("whatsapp-cloud response was empty or not JSON".into())
    })
}

fn is_auth_code(code: i64) -> bool {
    // 190 (expired token) and the 200..=299 OAuth-error band Meta uses.
    code == 190 || (200..=299).contains(&code)
}

fn is_rate_code(code: i64) -> bool {
    // 4 = app rate limit, 80004 = business account rate limit,
    // 130429 = `WhatsApp` message rate hit.
    matches!(code, 4 | 80_004 | 130_429)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_for(server: &MockServer) -> WhatsappCloudApi {
        WhatsappCloudApi::new(server.uri(), "EAAG-test")
    }

    #[test]
    fn is_auth_code_recognises_190() {
        assert!(is_auth_code(190));
    }

    #[test]
    fn is_auth_code_recognises_oauth_band() {
        assert!(is_auth_code(200));
        assert!(is_auth_code(250));
        assert!(is_auth_code(299));
        assert!(!is_auth_code(300));
        assert!(!is_auth_code(199));
    }

    #[test]
    fn is_rate_code_recognises_known_codes() {
        assert!(is_rate_code(4));
        assert!(is_rate_code(80_004));
        assert!(is_rate_code(130_429));
        assert!(!is_rate_code(5));
        assert!(!is_rate_code(100));
    }

    #[test]
    fn classify_returns_value_on_2xx() {
        let v = classify_response(
            StatusCode::OK,
            None,
            r#"{"messages":[{"id":"wamid.1"}]}"#,
        )
        .unwrap();
        assert_eq!(v["messages"][0]["id"], "wamid.1");
    }

    #[test]
    fn classify_429_returns_rate_with_retry_after() {
        let err = classify_response(StatusCode::TOO_MANY_REQUESTS, Some(7), "").unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(7)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn classify_429_without_retry_after() {
        let err = classify_response(StatusCode::TOO_MANY_REQUESTS, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: None }));
    }

    #[test]
    fn classify_401_returns_auth() {
        let body = r#"{"error":{"message":"bad token","code":190}}"#;
        let err = classify_response(StatusCode::UNAUTHORIZED, None, body).unwrap_err();
        match err {
            AdapterError::Auth(m) => assert!(m.contains("bad token")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn classify_403_returns_auth() {
        let err = classify_response(StatusCode::FORBIDDEN, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_2xx_with_auth_code_returns_auth() {
        // Edge case: rare 200 OK with an error envelope — still surface it.
        // We model this as the body containing a 190 code under HTTP 400.
        let body = r#"{"error":{"message":"oauth","code":190}}"#;
        let err = classify_response(StatusCode::BAD_REQUEST, None, body).unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_rate_code_in_body_returns_rate() {
        let body = r#"{"error":{"message":"too fast","code":4}}"#;
        let err = classify_response(StatusCode::BAD_REQUEST, Some(2), body).unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(2)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn classify_400_returns_bad_request_with_message() {
        let body = r#"{"error":{"message":"bad shape","code":100}}"#;
        let err = classify_response(StatusCode::BAD_REQUEST, None, body).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert_eq!(m, "bad shape"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_404_returns_bad_request() {
        let err = classify_response(StatusCode::NOT_FOUND, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn classify_500_returns_transport() {
        let body = r#"{"error":{"message":"broken"}}"#;
        let err = classify_response(StatusCode::INTERNAL_SERVER_ERROR, None, body).unwrap_err();
        match err {
            AdapterError::Transport(m) => assert_eq!(m, "broken"),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[test]
    fn classify_503_returns_transport() {
        let err = classify_response(StatusCode::SERVICE_UNAVAILABLE, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_redirect_returns_transport() {
        let err = classify_response(StatusCode::MOVED_PERMANENTLY, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_success_with_empty_body_is_transport() {
        let err = classify_response(StatusCode::OK, None, "").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_success_with_invalid_json_is_transport() {
        let err = classify_response(StatusCode::OK, None, "not json").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn send_text_returns_wamid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(header("authorization", "Bearer EAAG-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "messages": [{"id": "wamid.ABC"}]
                })),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api.send_text("PNID", "+15551234", "hi").await.unwrap();
        assert_eq!(id, "wamid.ABC");
    }

    #[tokio::test]
    async fn send_text_reply_includes_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "context": {"message_id": "wamid.PARENT"},
                "type": "text",
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "messages": [{"id": "wamid.REPLY"}]
                })),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_text_reply("PNID", "+15551234", "hi", "wamid.PARENT")
            .await
            .unwrap();
        assert_eq!(id, "wamid.REPLY");
    }

    #[tokio::test]
    async fn send_document_by_link_includes_link_and_filename() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "type": "document",
                "document": {"link": "https://example.test/x.pdf", "filename": "x.pdf"}
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"messages":[{"id":"wamid.DOC"}]})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_document_by_link(
                "PNID",
                "+15551234",
                "https://example.test/x.pdf",
                Some("x.pdf"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(id, "wamid.DOC");
    }

    #[tokio::test]
    async fn send_document_by_link_with_reply_attaches_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "context": {"message_id": "wamid.PARENT"}
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"messages":[{"id":"wamid.DOC2"}]})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_document_by_link(
                "PNID",
                "+15551234",
                "https://example.test/x.pdf",
                None,
                Some("wamid.PARENT"),
            )
            .await
            .unwrap();
        assert_eq!(id, "wamid.DOC2");
    }

    #[tokio::test]
    async fn send_document_by_id_returns_wamid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "document": {"id": "MEDIA1", "filename": "x.pdf"}
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"messages":[{"id":"wamid.DOC3"}]})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_document_by_id("PNID", "+15551234", "MEDIA1", Some("x.pdf"), None)
            .await
            .unwrap();
        assert_eq!(id, "wamid.DOC3");
    }

    #[tokio::test]
    async fn send_document_by_id_with_reply_attaches_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "context": {"message_id": "wamid.PARENT"},
                "document": {"id": "MEDIA1"}
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"messages":[{"id":"wamid.DOC4"}]})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_document_by_id("PNID", "+15551234", "MEDIA1", None, Some("wamid.PARENT"))
            .await
            .unwrap();
        assert_eq!(id, "wamid.DOC4");
    }

    #[tokio::test]
    async fn upload_media_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/media"))
            .and(header("authorization", "Bearer EAAG-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "MEDIA-42"})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .upload_media(
                "PNID",
                "report.pdf",
                "application/pdf",
                b"PDF-bytes".to_vec(),
            )
            .await
            .unwrap();
        assert_eq!(id, "MEDIA-42");
    }

    #[tokio::test]
    async fn upload_media_bad_mime_rejected() {
        let server = MockServer::start().await;
        let api = api_for(&server);
        let err = api
            .upload_media("PNID", "x", "this is not a mime", b"x".to_vec())
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn mark_read_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "status":"read",
                "message_id":"wamid.READ"
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"success": true})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.mark_read("PNID", "wamid.READ").await.unwrap();
    }

    #[tokio::test]
    async fn send_reaction_with_emoji() {
        let server = MockServer::start().await;
        // Use unicode codepoint composed at runtime (project rule: no inline emojis).
        let emoji_codepoint: u32 = 0x1F44D;
        let emoji = char::from_u32(emoji_codepoint).unwrap().to_string();
        let emoji_for_match = emoji.clone();
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "type": "reaction",
                "reaction": {
                    "message_id": "wamid.TARGET",
                    "emoji": emoji_for_match
                }
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"messages":[{"id":"wamid.REACT"}]})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_reaction("PNID", "+15551234", "wamid.TARGET", &emoji)
            .await
            .unwrap();
        assert_eq!(id, "wamid.REACT");
    }

    #[tokio::test]
    async fn send_reaction_with_empty_emoji_removes_reaction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(wiremock::matchers::body_partial_json(json!({
                "reaction": {"message_id": "wamid.TARGET", "emoji": ""}
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"messages":[{"id":"wamid.UNREACT"}]})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let id = api
            .send_reaction("PNID", "+15551234", "wamid.TARGET", "")
            .await
            .unwrap();
        assert_eq!(id, "wamid.UNREACT");
    }

    #[tokio::test]
    async fn send_text_surfaces_401_as_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error":{"message":"invalid token","code":190}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.send_text("PNID", "+1", "x").await {
            Err(AdapterError::Auth(m)) => assert!(m.contains("invalid token")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_surfaces_403_as_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let api = api_for(&server);
        assert!(matches!(
            api.send_text("PNID", "+1", "x").await,
            Err(AdapterError::Auth(_))
        ));
    }

    #[tokio::test]
    async fn send_text_surfaces_429_as_rate_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "9"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.send_text("PNID", "+1", "x").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(9)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_5xx_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.send_text("PNID", "+1", "x").await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("503") || !m.is_empty()),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_400_with_message_surfaces_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error":{"message":"recipient invalid","code":100}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.send_text("PNID", "+1", "x").await {
            Err(AdapterError::BadRequest(m)) => assert_eq!(m, "recipient invalid"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_missing_messages_array_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"messages": []})))
            .mount(&server)
            .await;
        let api = api_for(&server);
        assert!(matches!(
            api.send_text("PNID", "+1", "x").await,
            Err(AdapterError::Transport(_))
        ));
    }

    #[test]
    fn url_helpers_trim_trailing_slash() {
        let a = WhatsappCloudApi::new("https://example.test/v18.0/", "t");
        assert_eq!(a.messages_url("PNID"), "https://example.test/v18.0/PNID/messages");
        assert_eq!(a.media_url("PNID"), "https://example.test/v18.0/PNID/media");
    }

    #[test]
    fn clone_and_debug_present() {
        let a = WhatsappCloudApi::new("https://x.test", "EAAG");
        let _ = a.clone();
        assert!(format!("{a:?}").contains("EAAG"));
    }

    #[test]
    fn default_graph_base_constant() {
        assert_eq!(DEFAULT_GRAPH_BASE, "https://graph.facebook.com/v18.0");
    }
}
