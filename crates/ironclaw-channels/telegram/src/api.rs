//! Typed wrappers around the Telegram Bot API endpoints used by the adapter.
//!
//! Each function performs one HTTP call and maps platform-level failures
//! into [`AdapterError`] variants:
//!
//! - HTTP `401` -> `AdapterError::Auth`.
//! - HTTP `429` -> `AdapterError::Rate { retry_after }` (from
//!   `parameters.retry_after` if present, falling back to the
//!   `Retry-After` HTTP header).
//! - HTTP `4xx` -> `AdapterError::BadRequest` (using the `description` field).
//! - HTTP `5xx` or network failures -> `AdapterError::Transport`.

use crate::types::{ApiResponse, FileMeta, Message, SentMessage, Update, User};
use ironclaw_channels_core::AdapterError;
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;

/// Tiny client wrapping a [`reqwest::Client`] and the API base URL.
#[derive(Debug, Clone)]
pub struct TelegramApi {
    http: Client,
    api_base: String,
    bot_token: String,
}

impl TelegramApi {
    /// Construct a new client. Uses a fresh `reqwest::Client` if none is
    /// supplied.
    pub fn new(api_base: impl Into<String>, bot_token: impl Into<String>) -> Self {
        Self::with_client(Client::new(), api_base, bot_token)
    }

    /// Construct with a caller-supplied HTTP client (useful for tests).
    pub fn with_client(
        http: Client,
        api_base: impl Into<String>,
        bot_token: impl Into<String>,
    ) -> Self {
        Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            bot_token: bot_token.into(),
        }
    }

    fn endpoint(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base, self.bot_token, method)
    }

    /// File-download endpoint (different prefix from API methods).
    fn file_url(&self, file_path: &str) -> String {
        let trimmed = file_path.trim_start_matches('/');
        format!("{}/file/bot{}/{}", self.api_base, self.bot_token, trimmed)
    }

    /// Call `getMe`. Used by the factory to validate the bot token at init.
    pub async fn get_me(&self) -> Result<User, AdapterError> {
        let url = self.endpoint("getMe");
        let resp = self.http.get(&url).send().await.map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        decode_envelope::<User>(raw)
    }

    /// Resolve a `file_id` into a [`FileMeta`] via `getFile`.
    ///
    /// The returned `file_path` is the relative suffix to be appended onto
    /// `<api_base>/file/bot<token>/` for the binary download (see
    /// [`Self::download_file`]).
    pub async fn get_file(&self, file_id: &str) -> Result<FileMeta, AdapterError> {
        let url = self.endpoint("getFile");
        let body = serde_json::json!({ "file_id": file_id });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        decode_envelope::<FileMeta>(raw)
    }

    /// Download the bytes pointed at by `file_path` (as returned by
    /// `getFile`). Returns the raw bytes on success.
    ///
    /// Errors are mapped the same way as API method calls:
    /// - HTTP `401` -> [`AdapterError::Auth`].
    /// - HTTP `429` -> [`AdapterError::Rate`] (with `retry_after` from the
    ///   `Retry-After` header when present).
    /// - HTTP `4xx` -> [`AdapterError::BadRequest`].
    /// - HTTP `5xx` / network failures -> [`AdapterError::Transport`].
    pub async fn download_file(&self, file_path: &str) -> Result<Vec<u8>, AdapterError> {
        let url = self.file_url(file_path);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let status = resp.status();
        if status.is_success() {
            let bytes = resp.bytes().await.map_err(|e| {
                AdapterError::Transport(format!("telegram file body read failed: {e}"))
            })?;
            return Ok(bytes.to_vec());
        }
        let retry_after_header = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let body_text = resp.text().await.unwrap_or_default();
        Err(map_file_download_error(status, retry_after_header, &body_text))
    }

    /// Call `getUpdates` with the given parameters and parse the result.
    pub async fn get_updates(
        &self,
        offset: i64,
        timeout_secs: u64,
        limit: u32,
        allowed_updates: &[String],
    ) -> Result<Vec<Update>, AdapterError> {
        let url = self.endpoint("getUpdates");
        let mut body = serde_json::Map::new();
        body.insert("offset".into(), Value::from(offset));
        body.insert("timeout".into(), Value::from(timeout_secs));
        body.insert("limit".into(), Value::from(limit));
        if !allowed_updates.is_empty() {
            body.insert(
                "allowed_updates".into(),
                Value::from(allowed_updates.to_vec()),
            );
        }
        let resp = self
            .http
            .post(&url)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        decode_envelope::<Vec<Update>>(raw)
    }

    /// `sendMessage` — text only.
    pub async fn send_message(
        &self,
        chat_id: &str,
        message_thread_id: Option<&str>,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<Message, AdapterError> {
        let url = self.endpoint("sendMessage");
        let mut body = serde_json::Map::new();
        body.insert("chat_id".into(), Value::from(chat_id));
        if let Some(t) = message_thread_id {
            if let Ok(n) = t.parse::<i64>() {
                body.insert("message_thread_id".into(), Value::from(n));
            } else {
                body.insert("message_thread_id".into(), Value::from(t));
            }
        }
        body.insert("text".into(), Value::from(text));
        if let Some(mode) = parse_mode {
            body.insert("parse_mode".into(), Value::from(mode));
        }

        let resp = self
            .http
            .post(&url)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        let sent = decode_envelope::<SentMessage>(raw)?;
        Ok(Message {
            message_id: sent.message_id,
            message_thread_id: None,
            from: None,
            chat: crate::types::Chat {
                id: 0,
                kind: String::new(),
                title: None,
                username: None,
            },
            date: 0,
            text: None,
            caption: None,
            entities: vec![],
            document: None,
            photo: vec![],
            audio: None,
            video: None,
            voice: None,
            video_note: None,
            sticker: None,
        })
    }

    /// `sendDocument` — multipart upload of `bytes` as `filename`.
    pub async fn send_document(
        &self,
        chat_id: &str,
        message_thread_id: Option<&str>,
        filename: &str,
        bytes: Vec<u8>,
        caption: Option<&str>,
    ) -> Result<Message, AdapterError> {
        let url = self.endpoint("sendDocument");
        let mut form = Form::new().text("chat_id", chat_id.to_owned());
        if let Some(t) = message_thread_id {
            form = form.text("message_thread_id", t.to_owned());
        }
        if let Some(c) = caption {
            form = form.text("caption", c.to_owned());
        }
        let part = Part::bytes(bytes).file_name(filename.to_owned());
        form = form.part("document", part);

        let resp = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        let sent = decode_envelope::<SentMessage>(raw)?;
        Ok(Message {
            message_id: sent.message_id,
            message_thread_id: None,
            from: None,
            chat: crate::types::Chat {
                id: 0,
                kind: String::new(),
                title: None,
                username: None,
            },
            date: 0,
            text: None,
            caption: None,
            entities: vec![],
            document: None,
            photo: vec![],
            audio: None,
            video: None,
            voice: None,
            video_note: None,
            sticker: None,
        })
    }

    /// `editMessageText` — replace the text body of an existing message.
    ///
    /// Returns `()` on success; the Telegram API returns the edited message
    /// envelope but we don't surface it because the platform `message_id` is
    /// preserved and the host already has it.
    pub async fn edit_message_text(
        &self,
        chat_id: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), AdapterError> {
        let url = self.endpoint("editMessageText");
        let mut body = serde_json::Map::new();
        body.insert("chat_id".into(), Value::from(chat_id));
        // Telegram message_id is numeric; we try to parse, falling back to
        // the original string so non-numeric ids (shouldn't happen on
        // Telegram, but safe) still round-trip.
        if let Ok(n) = message_id.parse::<i64>() {
            body.insert("message_id".into(), Value::from(n));
        } else {
            body.insert("message_id".into(), Value::from(message_id));
        }
        body.insert("text".into(), Value::from(text));

        let resp = self
            .http
            .post(&url)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        decode_envelope::<Value>(raw).map(|_| ())
    }

    /// `setMessageReaction` — react to a message with a single emoji.
    pub async fn set_message_reaction(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        let url = self.endpoint("setMessageReaction");
        let mut body = serde_json::Map::new();
        body.insert("chat_id".into(), Value::from(chat_id));
        if let Ok(n) = message_id.parse::<i64>() {
            body.insert("message_id".into(), Value::from(n));
        } else {
            body.insert("message_id".into(), Value::from(message_id));
        }
        body.insert(
            "reaction".into(),
            Value::Array(vec![serde_json::json!({"type": "emoji", "emoji": emoji})]),
        );

        let resp = self
            .http
            .post(&url)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        decode_envelope::<Value>(raw).map(|_| ())
    }

    /// `sendChatAction` — fire-and-forget. Returns `()` on success.
    pub async fn send_chat_action(
        &self,
        chat_id: &str,
        message_thread_id: Option<&str>,
        action: &str,
    ) -> Result<(), AdapterError> {
        let url = self.endpoint("sendChatAction");
        let mut body = serde_json::Map::new();
        body.insert("chat_id".into(), Value::from(chat_id));
        if let Some(t) = message_thread_id {
            body.insert("message_thread_id".into(), Value::from(t));
        }
        body.insert("action".into(), Value::from(action));

        let resp = self
            .http
            .post(&url)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let raw = read_body(resp).await?;
        // Result type is `true` per Telegram docs; we only care about the envelope.
        decode_envelope::<Value>(raw).map(|_| ())
    }
}

fn map_send_err(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(format!("telegram http error: {err}"))
}

/// Map an HTTP failure response from the file-download endpoint into the
/// adapter's error taxonomy. The file endpoint does not use the JSON
/// envelope, so we work purely off the status code and `Retry-After`.
fn map_file_download_error(
    status: StatusCode,
    retry_after_header: Option<u64>,
    body: &str,
) -> AdapterError {
    let truncated = if body.len() > 256 { &body[..256] } else { body };
    let detail = if truncated.is_empty() {
        format!("telegram file http {status}")
    } else {
        format!("telegram file http {status}: {truncated}")
    };
    match status.as_u16() {
        401 => AdapterError::Auth(detail),
        429 => AdapterError::Rate {
            retry_after: retry_after_header,
        },
        s if (500..600).contains(&s) => AdapterError::Transport(detail),
        s if (400..500).contains(&s) => AdapterError::BadRequest(detail),
        _ => AdapterError::Transport(detail),
    }
}

/// `(status, retry_after_header, body)` parsed from a [`Response`].
struct RawResponse {
    status: StatusCode,
    retry_after_header: Option<u64>,
    body: String,
}

async fn read_body(resp: Response) -> Result<RawResponse, AdapterError> {
    let status = resp.status();
    // Capture Retry-After header before we move the body.
    let retry_after_header = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let body = resp
        .text()
        .await
        .map_err(|e| AdapterError::Transport(format!("telegram body read failed: {e}")))?;

    Ok(RawResponse {
        status,
        retry_after_header,
        body,
    })
}

fn decode_envelope<T: serde::de::DeserializeOwned>(raw: RawResponse) -> Result<T, AdapterError> {
    let RawResponse {
        status,
        retry_after_header,
        body,
    } = raw;

    if status.is_success() {
        let resp: ApiResponse<T> = serde_json::from_str(&body).map_err(|e| {
            AdapterError::Transport(format!("telegram response decode failed: {e}"))
        })?;
        if resp.ok {
            resp.result.ok_or_else(|| {
                AdapterError::Transport("telegram response missing `result`".into())
            })
        } else {
            Err(envelope_to_error(&resp))
        }
    } else {
        // Try parsing as the standard envelope; if that fails fall back to
        // status-based mapping.
        let envelope: Option<ApiResponse<T>> = if body.is_empty() {
            None
        } else {
            serde_json::from_str::<ApiResponse<T>>(&body).ok()
        };
        Err(map_status_error(status, envelope.as_ref(), retry_after_header))
    }
}

fn envelope_to_error<T>(resp: &ApiResponse<T>) -> AdapterError {
    let description = resp
        .description
        .clone()
        .unwrap_or_else(|| "telegram api error".into());
    let code = resp.error_code.unwrap_or(0);
    let retry_after = resp.parameters.as_ref().and_then(|p| p.retry_after);
    match code {
        401 => AdapterError::Auth(description),
        429 => AdapterError::Rate { retry_after },
        400 => AdapterError::BadRequest(description),
        c if (500..600).contains(&c) => AdapterError::Transport(description),
        _ => AdapterError::Transport(description),
    }
}

fn map_status_error<T>(
    status: StatusCode,
    envelope: Option<&ApiResponse<T>>,
    retry_after_header: Option<u64>,
) -> AdapterError {
    let description = envelope
        .and_then(|e| e.description.clone())
        .unwrap_or_else(|| format!("telegram http {status}"));
    let retry_after = envelope
        .and_then(|e| e.parameters.as_ref())
        .and_then(|p| p.retry_after)
        .or(retry_after_header);

    match status.as_u16() {
        401 => AdapterError::Auth(description),
        429 => AdapterError::Rate { retry_after },
        400 => AdapterError::BadRequest(description),
        s if (500..600).contains(&s) => AdapterError::Transport(description),
        s if (400..500).contains(&s) => AdapterError::BadRequest(description),
        _ => AdapterError::Transport(description),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server() -> MockServer {
        MockServer::start().await
    }

    fn api(server_url: &str, token: &str) -> TelegramApi {
        TelegramApi::new(server_url, token)
    }

    #[tokio::test]
    async fn get_me_success() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "id": 1, "is_bot": true, "username": "ironbot" }
            })))
            .mount(&s)
            .await;
        let api = api(&s.uri(), "tok");
        let u = api.get_me().await.unwrap();
        assert_eq!(u.id, 1);
        assert!(u.is_bot);
        assert_eq!(u.username.as_deref(), Some("ironbot"));
    }

    #[tokio::test]
    async fn get_me_auth_error_401() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false, "error_code": 401, "description": "Unauthorized"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok").get_me().await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(ref m) if m == "Unauthorized"));
    }

    #[tokio::test]
    async fn get_updates_success() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": [
                    {
                        "update_id": 9,
                        "message": {
                            "message_id": 1,
                            "date": 1,
                            "chat": { "id": 1, "type": "private" },
                            "text": "hello"
                        }
                    }
                ]
            })))
            .mount(&s)
            .await;
        let updates = api(&s.uri(), "tok")
            .get_updates(0, 1, 100, &[])
            .await
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 9);
    }

    #[tokio::test]
    async fn get_updates_with_allowed_updates() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": []
            })))
            .mount(&s)
            .await;
        let updates = api(&s.uri(), "tok")
            .get_updates(0, 1, 100, &["message".to_owned()])
            .await
            .unwrap();
        assert!(updates.is_empty());
    }

    #[tokio::test]
    async fn send_message_success_returns_message_id() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "message_id": 1234 }
            })))
            .mount(&s)
            .await;
        let m = api(&s.uri(), "tok")
            .send_message("100", None, "hello", Some("MarkdownV2"))
            .await
            .unwrap();
        assert_eq!(m.message_id, 1234);
    }

    #[tokio::test]
    async fn send_message_with_thread_numeric() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "message_id": 1 }
            })))
            .mount(&s)
            .await;
        let _ = api(&s.uri(), "tok")
            .send_message("c", Some("42"), "x", None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn send_message_with_thread_non_numeric_falls_back_to_string() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "message_id": 1 }
            })))
            .mount(&s)
            .await;
        let _ = api(&s.uri(), "tok")
            .send_message("c", Some("not-a-number"), "x", None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn send_message_rate_limited_with_retry_after_in_body() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "ok": false, "error_code": 429, "description": "Too Many Requests",
                "parameters": { "retry_after": 7 }
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(7)),
            other => panic!("expected rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_message_rate_limited_with_retry_after_header_only() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "5")
                    .set_body_string("rate"),
            )
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: Some(5) }));
    }

    #[tokio::test]
    async fn send_message_bad_request_400() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false, "error_code": 400, "description": "Bad Request: chat not found"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(ref m) if m.contains("chat not found")));
    }

    #[tokio::test]
    async fn send_message_500_is_transport() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn send_message_unknown_4xx_is_bad_request() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(418).set_body_string("teapot"))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn send_message_3xx_is_transport() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(304).set_body_string(""))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn send_message_envelope_ok_false_with_500_code() {
        // 200 from HTTP but ok=false with 5xx error code maps to Transport.
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false, "error_code": 500, "description": "internal"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn send_message_envelope_ok_false_with_no_code() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false, "description": "weird"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(ref m) if m == "weird"));
    }

    #[tokio::test]
    async fn send_message_envelope_ok_true_missing_result_errors() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_message("c", None, "x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn send_document_success() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendDocument"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "message_id": 42 }
            })))
            .mount(&s)
            .await;
        let m = api(&s.uri(), "tok")
            .send_document("c", Some("99"), "x.bin", vec![1, 2, 3], Some("see attached"))
            .await
            .unwrap();
        assert_eq!(m.message_id, 42);
    }

    #[tokio::test]
    async fn send_chat_action_success() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendChatAction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": true
            })))
            .mount(&s)
            .await;
        api(&s.uri(), "tok")
            .send_chat_action("c", Some("3"), "typing")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn send_chat_action_error() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendChatAction"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false, "error_code": 400, "description": "bad chat"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .send_chat_action("c", None, "typing")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn network_failure_is_transport() {
        // Point at a port nothing is listening on.
        let api = api("http://127.0.0.1:1", "tok");
        let err = api.get_me().await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn api_constructor_trims_trailing_slash() {
        let api = TelegramApi::new("http://x/", "tok");
        assert!(api.endpoint("getMe").ends_with("/bottok/getMe"));
        assert!(api.endpoint("getMe").starts_with("http://x/"));
    }

    #[tokio::test]
    async fn with_client_uses_supplied_client() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "id": 1, "is_bot": true }
            })))
            .mount(&s)
            .await;
        let client = Client::builder().build().unwrap();
        let api = TelegramApi::with_client(client, s.uri(), "tok");
        let u = api.get_me().await.unwrap();
        assert_eq!(u.id, 1);
    }

    #[tokio::test]
    async fn malformed_response_body_maps_to_transport() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok").get_me().await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn debug_format_of_api() {
        let api = TelegramApi::new("http://x", "tok");
        let s = format!("{api:?}");
        assert!(s.contains("TelegramApi"));
    }

    #[tokio::test]
    async fn clone_of_api_works() {
        let api = TelegramApi::new("http://x", "tok");
        let _ = api.clone();
    }

    #[tokio::test]
    async fn get_file_success_decodes_envelope() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "file_id": "F",
                    "file_unique_id": "U",
                    "file_size": 9,
                    "file_path": "documents/a.bin"
                }
            })))
            .mount(&s)
            .await;
        let meta = api(&s.uri(), "tok").get_file("F").await.unwrap();
        assert_eq!(meta.file_id, "F");
        assert_eq!(meta.file_unique_id.as_deref(), Some("U"));
        assert_eq!(meta.file_size, Some(9));
        assert_eq!(meta.file_path.as_deref(), Some("documents/a.bin"));
    }

    #[tokio::test]
    async fn get_file_401_maps_to_auth() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false, "error_code": 401, "description": "Unauthorized"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok").get_file("F").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn get_file_400_maps_to_bad_request() {
        let s = server().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "ok": false, "error_code": 400, "description": "file not found"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok").get_file("F").await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn download_file_success_returns_bytes() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/documents/a.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"payload".to_vec()))
            .mount(&s)
            .await;
        let bytes = api(&s.uri(), "tok")
            .download_file("documents/a.bin")
            .await
            .unwrap();
        assert_eq!(bytes, b"payload");
    }

    #[tokio::test]
    async fn download_file_strips_leading_slash() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/documents/a.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec()))
            .mount(&s)
            .await;
        let bytes = api(&s.uri(), "tok")
            .download_file("/documents/a.bin")
            .await
            .unwrap();
        assert_eq!(bytes, b"ok");
    }

    #[tokio::test]
    async fn download_file_401_is_auth() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/x"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok").download_file("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn download_file_429_carries_retry_after_header() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/x"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "9")
                    .set_body_string(""),
            )
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok").download_file("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: Some(9) }));
    }

    #[tokio::test]
    async fn download_file_5xx_is_transport() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/x"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok").download_file("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn download_file_404_is_bad_request() {
        let s = server().await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/missing"))
            .respond_with(ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&s)
            .await;
        let err = api(&s.uri(), "tok")
            .download_file("missing")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn download_file_network_failure_is_transport() {
        let api = api("http://127.0.0.1:1", "tok");
        let err = api.download_file("x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }
}
