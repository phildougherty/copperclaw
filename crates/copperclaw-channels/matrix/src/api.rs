//! HTTP client for the Matrix Client-Server API.
//!
//! Each public method performs one HTTP call and maps platform failures to
//! [`AdapterError`] variants:
//!
//! - `200` -> `Ok`
//! - `401` / `403` -> [`AdapterError::Auth`]
//! - `404` -> [`AdapterError::BadRequest`]
//! - `429` -> [`AdapterError::Rate`]; prefers the JSON `retry_after_ms`
//!   field (converted to seconds, rounded up) and falls back to the
//!   `Retry-After` HTTP header.
//! - `5xx` / network failures -> [`AdapterError::Transport`]
//! - `4xx` JSON with `errcode` (e.g. `M_FORBIDDEN`) -> classified per
//!   [`map_errcode`].

use copperclaw_channels_core::AdapterError;
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::DEFAULT_TXN_PREFIX;

/// Response from `PUT .../send/{event_type}/{txn}` and friends.
#[derive(Debug, Clone, Deserialize)]
pub struct SendResponse {
    /// `$<id>` event id returned by the homeserver.
    pub event_id: String,
}

/// Response from `POST /_matrix/media/v3/upload`.
#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    /// `mxc://...` URI for the uploaded blob.
    pub content_uri: String,
}

/// Response from `GET /_matrix/client/v3/profile/{userId}`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileResponse {
    /// Display name, when known.
    #[serde(default)]
    pub displayname: Option<String>,
    /// Avatar mxc URI, when known.
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// Response from `GET /_matrix/client/v3/directory/room/{alias}`.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolveAliasResponse {
    /// Canonical `!roomid:server`.
    pub room_id: String,
}

/// Minimal Matrix Client-Server API client.
#[derive(Debug, Clone)]
pub struct MatrixApi {
    http: Client,
    homeserver_url: String,
    access_token: String,
    txn_prefix: String,
}

impl MatrixApi {
    /// Build a client.
    pub fn new(homeserver_url: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self::with_client(
            Client::new(),
            homeserver_url,
            access_token,
            DEFAULT_TXN_PREFIX,
        )
    }

    /// Build with a caller-supplied [`reqwest::Client`] and txn prefix.
    pub fn with_client(
        http: Client,
        homeserver_url: impl Into<String>,
        access_token: impl Into<String>,
        txn_prefix: impl Into<String>,
    ) -> Self {
        Self {
            http,
            homeserver_url: homeserver_url.into().trim_end_matches('/').to_owned(),
            access_token: access_token.into(),
            txn_prefix: txn_prefix.into(),
        }
    }

    /// Configured homeserver base URL.
    pub fn homeserver_url(&self) -> &str {
        &self.homeserver_url
    }

    /// Configured access token.
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    fn endpoint(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.homeserver_url)
    }

    fn next_txn_id(&self) -> String {
        format!("{}-{}", self.txn_prefix, uuid::Uuid::new_v4())
    }

    /// `PUT /_matrix/client/v3/rooms/{roomId}/send/m.room.message/{txnId}`
    /// with a `m.text` body. Returns the new event id.
    pub async fn send_text(&self, room_id: &str, text: &str) -> Result<SendResponse, AdapterError> {
        let body = json!({ "msgtype": "m.text", "body": text });
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// As [`send_text`] but also includes `format` / `formatted_body` for
    /// HTML rendering on Matrix clients that support it.
    pub async fn send_html(
        &self,
        room_id: &str,
        plain: &str,
        html: &str,
    ) -> Result<SendResponse, AdapterError> {
        let body = json!({
            "msgtype": "m.text",
            "body": plain,
            "format": "org.matrix.custom.html",
            "formatted_body": html,
        });
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// Send an `m.notice` HTML message — Matrix's idiomatic shape for
    /// bot-generated "metadata" content (typing notices, build statuses,
    /// breadcrumb chips). Clients render `m.notice` in a less
    /// prominent visual style than `m.text`, which is exactly the
    /// chip aesthetic the breadcrumb path needs.
    pub async fn send_notice_html(
        &self,
        room_id: &str,
        plain: &str,
        html: &str,
    ) -> Result<SendResponse, AdapterError> {
        let body = json!({
            "msgtype": "m.notice",
            "body": plain,
            "format": "org.matrix.custom.html",
            "formatted_body": html,
        });
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// Edit a previously-sent message with an `m.notice` HTML body
    /// (mirrors [`edit_message`](Self::edit_message) but produces a
    /// notice rather than a text replacement). Used by the
    /// breadcrumb adapter to update a chip in place via Matrix's
    /// `m.replace` relation.
    pub async fn edit_message_notice_html(
        &self,
        room_id: &str,
        target_event_id: &str,
        plain: &str,
        html: &str,
    ) -> Result<SendResponse, AdapterError> {
        let body = json!({
            "msgtype": "m.notice",
            "body": format!("* {plain}"),
            "format": "org.matrix.custom.html",
            "formatted_body": format!("* {html}"),
            "m.new_content": {
                "msgtype": "m.notice",
                "body": plain,
                "format": "org.matrix.custom.html",
                "formatted_body": html,
            },
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": target_event_id,
            }
        });
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// As [`Self::edit_message_notice_html`] but for `m.text` events
    /// (so Element raises the same notification a fresh message
    /// would). Used by `deliver_todo_list` to update an HTML
    /// checklist in place via Matrix's `m.replace` relation.
    pub async fn edit_message_html(
        &self,
        room_id: &str,
        target_event_id: &str,
        plain: &str,
        html: &str,
    ) -> Result<SendResponse, AdapterError> {
        let body = json!({
            "msgtype": "m.text",
            "body": format!("* {plain}"),
            "format": "org.matrix.custom.html",
            "formatted_body": format!("* {html}"),
            "m.new_content": {
                "msgtype": "m.text",
                "body": plain,
                "format": "org.matrix.custom.html",
                "formatted_body": html,
            },
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": target_event_id,
            }
        });
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// Send a text message into an existing Matrix thread (per MSC3440).
    pub async fn send_threaded(
        &self,
        room_id: &str,
        thread_event_id: &str,
        text: &str,
    ) -> Result<SendResponse, AdapterError> {
        let body = json!({
            "msgtype": "m.text",
            "body": text,
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": thread_event_id,
            }
        });
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// Edit a previously-sent message by sending an `m.replace` event.
    pub async fn edit_message(
        &self,
        room_id: &str,
        target_event_id: &str,
        new_text: &str,
    ) -> Result<SendResponse, AdapterError> {
        let body = json!({
            "msgtype": "m.text",
            "body": format!("* {new_text}"),
            "m.new_content": {
                "msgtype": "m.text",
                "body": new_text,
            },
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": target_event_id,
            }
        });
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// Add a reaction (`m.annotation`) to a message.
    pub async fn send_reaction(
        &self,
        room_id: &str,
        target_event_id: &str,
        key: &str,
    ) -> Result<SendResponse, AdapterError> {
        let body = json!({
            "m.relates_to": {
                "rel_type": "m.annotation",
                "event_id": target_event_id,
                "key": key,
            }
        });
        self.send_room_event(room_id, "m.reaction", body).await
    }

    /// Upload bytes to Matrix media; returns the `mxc://...` content URI.
    pub async fn upload_media(
        &self,
        filename: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<UploadResponse, AdapterError> {
        let url = self.endpoint("/_matrix/media/v3/upload");
        let resp = self
            .http
            .post(&url)
            .query(&[("filename", filename)])
            .bearer_auth(&self.access_token)
            .header(reqwest::header::CONTENT_TYPE, mime)
            .body(bytes)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_matrix_json(resp).await?;
        serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("matrix upload decode: {e}")))
    }

    /// Send a `m.file` (or `m.image`) room message referencing an uploaded
    /// `mxc://` URI. `msgtype` is typically `"m.file"` or `"m.image"`.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_media_message(
        &self,
        room_id: &str,
        msgtype: &str,
        filename: &str,
        mxc_url: &str,
        mime: &str,
        size: usize,
        thread_event_id: Option<&str>,
    ) -> Result<SendResponse, AdapterError> {
        let mut body = json!({
            "msgtype": msgtype,
            "body": filename,
            "filename": filename,
            "info": {
                "mimetype": mime,
                "size": size,
            },
            "url": mxc_url,
        });
        if let Some(thread) = thread_event_id {
            body["m.relates_to"] = json!({
                "rel_type": "m.thread",
                "event_id": thread,
            });
        }
        self.send_room_event(room_id, "m.room.message", body).await
    }

    /// `PUT .../typing/{userId}` — typing indicator. `typing=true` with a
    /// 4-second timeout; `typing=false` cancels.
    pub async fn typing(
        &self,
        room_id: &str,
        user_id: &str,
        typing: bool,
    ) -> Result<(), AdapterError> {
        let url = self.endpoint(&format!(
            "/_matrix/client/v3/rooms/{}/typing/{}",
            urlencoding(room_id),
            urlencoding(user_id),
        ));
        let body = if typing {
            json!({ "typing": true, "timeout": 4000 })
        } else {
            json!({ "typing": false })
        };
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let _ = read_matrix_json(resp).await?;
        Ok(())
    }

    /// `GET /_matrix/client/v3/profile/{userId}` — best-effort profile lookup.
    pub async fn get_profile(&self, user_id: &str) -> Result<ProfileResponse, AdapterError> {
        let url = self.endpoint(&format!(
            "/_matrix/client/v3/profile/{}",
            urlencoding(user_id)
        ));
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_matrix_json(resp).await?;
        serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("matrix profile decode: {e}")))
    }

    /// `GET /_matrix/client/v3/directory/room/{alias}` — resolve an alias
    /// (`#name:server`) to its canonical room id.
    pub async fn resolve_alias(&self, alias: &str) -> Result<ResolveAliasResponse, AdapterError> {
        let url = self.endpoint(&format!(
            "/_matrix/client/v3/directory/room/{}",
            urlencoding(alias)
        ));
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_matrix_json(resp).await?;
        serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("matrix alias decode: {e}")))
    }

    /// `GET /_matrix/client/v3/sync` — long-poll for new events.
    ///
    /// `since` may be `None` for the first call. `timeout_ms` is forwarded
    /// to the homeserver. `filter`, when present, is the inline filter JSON
    /// (or a saved filter id — we always inline).
    pub async fn sync(
        &self,
        since: Option<&str>,
        timeout_ms: u64,
        filter: Option<&Value>,
    ) -> Result<Value, AdapterError> {
        let url = self.endpoint("/_matrix/client/v3/sync");
        let timeout_str = timeout_ms.to_string();
        let mut query: Vec<(&str, String)> = vec![("timeout", timeout_str)];
        if let Some(since) = since {
            query.push(("since", since.to_owned()));
        }
        if let Some(filter) = filter {
            query.push(("filter", filter.to_string()));
        }
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.access_token)
            .query(&query)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        read_matrix_json(resp).await
    }

    async fn send_room_event(
        &self,
        room_id: &str,
        event_type: &str,
        body: Value,
    ) -> Result<SendResponse, AdapterError> {
        let txn = self.next_txn_id();
        let url = self.endpoint(&format!(
            "/_matrix/client/v3/rooms/{}/send/{event_type}/{}",
            urlencoding(room_id),
            urlencoding(&txn),
        ));
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_matrix_json(resp).await?;
        serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("matrix send decode: {e}")))
    }
}

fn urlencoding(s: &str) -> String {
    // Matrix path segments must be percent-encoded; we use `url` for this.
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn map_send_err(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(format!("matrix http error: {err}"))
}

async fn read_matrix_json(resp: Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after_header = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp
        .text()
        .await
        .map_err(|e| AdapterError::Transport(format!("matrix body read failed: {e}")))?;

    let parsed: Option<Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_str(&body).ok()
    };

    if status.is_success() {
        return parsed
            .ok_or_else(|| AdapterError::Transport(format!("matrix response not JSON: {body}")));
    }

    Err(classify_error(status, parsed.as_ref(), retry_after_header))
}

/// Build an [`AdapterError`] from an unsuccessful HTTP response.
pub(crate) fn classify_error(
    status: StatusCode,
    body: Option<&Value>,
    retry_after_header: Option<u64>,
) -> AdapterError {
    let errcode = body.and_then(|v| v.get("errcode")).and_then(Value::as_str);
    let description = body
        .and_then(|v| v.get("error"))
        .and_then(Value::as_str)
        .map_or_else(|| format!("matrix http {status}"), str::to_owned);

    if let Some(code) = errcode {
        if matches!(code, "M_LIMIT_EXCEEDED") {
            let retry_after_ms = body
                .and_then(|v| v.get("retry_after_ms"))
                .and_then(Value::as_u64);
            return AdapterError::Rate {
                retry_after: retry_after_ms.map(ms_to_secs_ceil).or(retry_after_header),
            };
        }
        return map_errcode(code, description, retry_after_header);
    }

    match status.as_u16() {
        401 | 403 => AdapterError::Auth(description),
        404 => AdapterError::BadRequest(description),
        429 => AdapterError::Rate {
            retry_after: body
                .and_then(|v| v.get("retry_after_ms"))
                .and_then(Value::as_u64)
                .map(ms_to_secs_ceil)
                .or(retry_after_header),
        },
        s if (500..600).contains(&s) => AdapterError::Transport(description),
        s if (400..500).contains(&s) => AdapterError::BadRequest(description),
        _ => AdapterError::Transport(description),
    }
}

/// Map a Matrix `M_*` errcode string to a typed [`AdapterError`].
pub(crate) fn map_errcode(
    code: &str,
    description: String,
    retry_after_header: Option<u64>,
) -> AdapterError {
    match code {
        "M_UNKNOWN_TOKEN" | "M_MISSING_TOKEN" | "M_FORBIDDEN" => AdapterError::Auth(description),
        "M_LIMIT_EXCEEDED" => AdapterError::Rate {
            retry_after: retry_after_header,
        },
        _ => AdapterError::BadRequest(description),
    }
}

fn ms_to_secs_ceil(ms: u64) -> u64 {
    ms.div_ceil(1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(server_url: &str) -> MatrixApi {
        MatrixApi::with_client(Client::new(), server_url, "tok", DEFAULT_TXN_PREFIX)
    }

    #[tokio::test]
    async fn send_text_returns_event_id() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$evt:m.org"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri()).send_text("!a:m.org", "hi").await.unwrap();
        assert_eq!(r.event_id, "$evt:m.org");
    }

    #[tokio::test]
    async fn send_html_returns_event_id() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$h:m.org"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .send_html("!a:m.org", "plain", "<p>plain</p>")
            .await
            .unwrap();
        assert_eq!(r.event_id, "$h:m.org");
    }

    #[tokio::test]
    async fn send_threaded_includes_relates_to_thread() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .and(wiremock::matchers::body_string_contains("\"m.thread\""))
            .and(wiremock::matchers::body_string_contains("\"$root:m.org\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$t:m.org"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .send_threaded("!a:m.org", "$root:m.org", "reply")
            .await
            .unwrap();
        assert_eq!(r.event_id, "$t:m.org");
    }

    #[tokio::test]
    async fn edit_message_uses_m_replace() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .and(wiremock::matchers::body_string_contains("\"m.replace\""))
            .and(wiremock::matchers::body_string_contains(
                "\"m.new_content\"",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$e:m.org"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .edit_message("!a:m.org", "$tgt:m.org", "updated")
            .await
            .unwrap();
        assert_eq!(r.event_id, "$e:m.org");
    }

    #[tokio::test]
    async fn reaction_uses_m_annotation_event_type() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.reaction/.+",
            ))
            .and(wiremock::matchers::body_string_contains("\"m.annotation\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$r:m.org"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .send_reaction("!a:m.org", "$tgt:m.org", "\u{1F44D}")
            .await
            .unwrap();
        assert_eq!(r.event_id, "$r:m.org");
    }

    #[tokio::test]
    async fn upload_returns_content_uri() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/_matrix/media/v3/upload"))
            .and(query_param("filename", "doc.txt"))
            .and(header("content-type", "text/plain"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content_uri": "mxc://m.org/abc"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .upload_media("doc.txt", "text/plain", vec![1, 2, 3])
            .await
            .unwrap();
        assert_eq!(r.content_uri, "mxc://m.org/abc");
    }

    #[tokio::test]
    async fn send_media_message_references_mxc_uri() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .and(wiremock::matchers::body_string_contains("mxc://m.org/abc"))
            .and(wiremock::matchers::body_string_contains("\"m.file\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$f:m.org"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri())
            .send_media_message(
                "!a:m.org",
                "m.file",
                "doc.txt",
                "mxc://m.org/abc",
                "text/plain",
                3,
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.event_id, "$f:m.org");
    }

    #[tokio::test]
    async fn send_media_message_with_thread() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .and(wiremock::matchers::body_string_contains("\"m.thread\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$f2:m.org"
            })))
            .mount(&s)
            .await;
        let _ = api(&s.uri())
            .send_media_message(
                "!a:m.org",
                "m.image",
                "p.png",
                "mxc://m.org/p",
                "image/png",
                10,
                Some("$root:m.org"),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn typing_true_succeeds() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+"))
            .and(wiremock::matchers::body_string_contains("\"typing\":true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        api(&s.uri())
            .typing("!a:m.org", "@b:m.org", true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn typing_false_succeeds() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+"))
            .and(wiremock::matchers::body_string_contains("\"typing\":false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        api(&s.uri())
            .typing("!a:m.org", "@b:m.org", false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn get_profile_returns_displayname() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/profile/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "displayname": "Alice"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri()).get_profile("@a:m.org").await.unwrap();
        assert_eq!(r.displayname.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn get_profile_missing_displayname_is_none() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/profile/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        let r = api(&s.uri()).get_profile("@a:m.org").await.unwrap();
        assert!(r.displayname.is_none());
    }

    #[tokio::test]
    async fn resolve_alias_returns_room_id() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/directory/room/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "room_id": "!real:m.org"
            })))
            .mount(&s)
            .await;
        let r = api(&s.uri()).resolve_alias("#alias:m.org").await.unwrap();
        assert_eq!(r.room_id, "!real:m.org");
    }

    #[tokio::test]
    async fn sync_passes_since_and_timeout() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .and(query_param("timeout", "100"))
            .and(query_param("since", "abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "next"
            })))
            .mount(&s)
            .await;
        let v = api(&s.uri()).sync(Some("abc"), 100, None).await.unwrap();
        assert_eq!(v["next_batch"], "next");
    }

    #[tokio::test]
    async fn sync_without_since_omits_param() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .and(query_param("timeout", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "first"
            })))
            .mount(&s)
            .await;
        let v = api(&s.uri()).sync(None, 0, None).await.unwrap();
        assert_eq!(v["next_batch"], "first");
    }

    #[tokio::test]
    async fn sync_with_filter_serialises_filter() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .and(query_param("filter", r#"{"x":1}"#))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "x"
            })))
            .mount(&s)
            .await;
        let v = api(&s.uri())
            .sync(None, 0, Some(&json!({"x": 1})))
            .await
            .unwrap();
        assert_eq!(v["next_batch"], "x");
    }

    #[tokio::test]
    async fn send_text_auth_error_401() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "errcode": "M_MISSING_TOKEN", "error": "missing"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn send_text_m_forbidden_is_auth() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "errcode": "M_FORBIDDEN", "error": "no"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn send_text_unknown_token_is_auth() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "errcode": "M_UNKNOWN_TOKEN", "error": "expired"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn send_text_rate_limit_uses_json_retry_after_ms() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "errcode": "M_LIMIT_EXCEEDED",
                "error": "slow down",
                "retry_after_ms": 2500
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(3)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_text_rate_limit_falls_back_to_retry_after_header() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "5")
                    .set_body_string(""),
            )
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(
            err,
            AdapterError::Rate {
                retry_after: Some(5)
            }
        ));
    }

    #[tokio::test]
    async fn send_text_rate_limit_with_no_hint() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(429).set_body_string(""))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: None }));
    }

    #[tokio::test]
    async fn send_text_404_is_bad_request() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "errcode": "M_NOT_FOUND", "error": "no such room"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn send_text_5xx_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn send_text_3xx_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(304).set_body_string(""))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn send_text_other_4xx_no_errcode_is_bad_request() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(418).set_body_string("teapot"))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn send_text_other_errcode_is_bad_request() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "errcode": "M_INVALID_PARAM", "error": "bad"
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn malformed_success_body_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn success_body_missing_event_id_is_transport() {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn network_failure_maps_to_transport() {
        let api = MatrixApi::new("http://127.0.0.1:1", "tok");
        let err = api.send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn rate_limit_via_429_with_retry_after_ms_no_errcode() {
        // 429 status code without an M_LIMIT_EXCEEDED errcode.
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path_regex(
                r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+",
            ))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "retry_after_ms": 1000
            })))
            .mount(&s)
            .await;
        let err = api(&s.uri()).send_text("!a:m.org", "x").await.unwrap_err();
        assert!(matches!(
            err,
            AdapterError::Rate {
                retry_after: Some(1)
            }
        ));
    }

    #[test]
    fn map_errcode_auth_codes() {
        for code in ["M_UNKNOWN_TOKEN", "M_MISSING_TOKEN", "M_FORBIDDEN"] {
            assert!(matches!(
                map_errcode(code, "x".into(), None),
                AdapterError::Auth(_)
            ));
        }
    }

    #[test]
    fn map_errcode_rate() {
        assert!(matches!(
            map_errcode("M_LIMIT_EXCEEDED", "x".into(), Some(7)),
            AdapterError::Rate {
                retry_after: Some(7)
            }
        ));
    }

    #[test]
    fn map_errcode_other_is_bad_request() {
        assert!(matches!(
            map_errcode("M_OTHER", "x".into(), None),
            AdapterError::BadRequest(_)
        ));
    }

    #[test]
    fn ms_to_secs_ceil_rounds_up() {
        assert_eq!(ms_to_secs_ceil(0), 0);
        assert_eq!(ms_to_secs_ceil(1), 1);
        assert_eq!(ms_to_secs_ceil(1000), 1);
        assert_eq!(ms_to_secs_ceil(1001), 2);
        assert_eq!(ms_to_secs_ceil(2500), 3);
    }

    #[test]
    fn classify_error_500_is_transport() {
        let err = classify_error(StatusCode::from_u16(500).unwrap(), None, None);
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_error_unknown_2xx_falls_back_transport() {
        // 2xx never reaches classify_error in practice but guard the arm.
        let err = classify_error(StatusCode::from_u16(204).unwrap(), None, None);
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn urlencoding_basic() {
        assert_eq!(urlencoding("!a:m.org"), "%21a%3Am.org");
        assert_eq!(urlencoding("@b:m.org"), "%40b%3Am.org");
    }

    #[test]
    fn next_txn_id_uses_prefix_and_unique_suffix() {
        let a = MatrixApi::with_client(Client::new(), "http://x", "tok", "p");
        let id1 = a.next_txn_id();
        let id2 = a.next_txn_id();
        assert!(id1.starts_with("p-"));
        assert!(id2.starts_with("p-"));
        assert_ne!(id1, id2);
    }

    #[test]
    fn debug_and_clone() {
        let a = MatrixApi::new("http://x", "tok");
        let _ = a.clone();
        assert!(format!("{a:?}").contains("MatrixApi"));
        assert_eq!(a.homeserver_url(), "http://x");
        assert_eq!(a.access_token(), "tok");
    }

    #[test]
    fn homeserver_url_trims_trailing_slash() {
        let a = MatrixApi::new("http://x/", "tok");
        assert_eq!(a.homeserver_url(), "http://x");
    }
}
