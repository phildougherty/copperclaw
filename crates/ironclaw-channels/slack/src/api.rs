//! Slack Web API client.
//!
//! Wraps the small slice of Slack endpoints the adapter needs. Slack returns
//! HTTP 200 even for logical failures (with `{"ok": false, "error": "..."}`),
//! so the client lifts those into [`AdapterError`].

use ironclaw_channels_core::AdapterError;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Response from `auth.test`. Only the fields we need.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthTestResponse {
    pub user_id: String,
}

/// Response from `chat.postMessage` / `chat.postEphemeral` / `chat.update`.
#[derive(Debug, Clone, Deserialize)]
pub struct PostMessageResponse {
    /// Slack's `ts` field — used as the platform-side message id.
    /// `chat.postEphemeral` returns `message_ts` instead.
    #[serde(default, alias = "message_ts")]
    pub ts: Option<String>,
}

/// Response from `files.getUploadURLExternal`.
#[derive(Debug, Clone, Deserialize)]
pub struct GetUploadUrlResponse {
    pub upload_url: String,
    pub file_id: String,
}

/// One entry in the `files.completeUploadExternal` `files` array.
#[derive(Debug, Clone, Serialize)]
pub struct CompleteUploadEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Minimal Slack Web API client.
#[derive(Debug, Clone)]
pub struct SlackApi {
    client: Client,
    api_base: String,
    bot_token: String,
}

impl SlackApi {
    /// Build a client using the configured token and base URL.
    ///
    /// Uses [`reqwest::Client::new`] for default settings.
    #[must_use]
    pub fn new(api_base: impl Into<String>, bot_token: impl Into<String>) -> Self {
        Self::with_client(Client::new(), api_base, bot_token)
    }

    /// Construct with a caller-supplied `reqwest::Client`. Useful for tests
    /// that want a shared connection pool or custom timeouts.
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

    fn url(&self, method: &str) -> String {
        format!("{}/{method}", self.api_base.trim_end_matches('/'))
    }

    /// `auth.test` — used at init to discover the bot's user id so we can
    /// detect `<@bot>` mentions in inbound messages.
    pub async fn auth_test(&self) -> Result<AuthTestResponse, AdapterError> {
        let resp = self
            .client
            .post(self.url("auth.test"))
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_slack_json(resp).await?;
        let parsed: AuthTestResponse = serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("auth.test decode: {e}")))?;
        Ok(parsed)
    }

    /// `chat.postMessage` — text + (optional) blocks.
    pub async fn post_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        blocks: Option<&Value>,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut body = json!({"channel": channel, "text": text});
        if let Some(thread) = thread_ts {
            body["thread_ts"] = Value::String(thread.to_owned());
        }
        if let Some(blocks) = blocks {
            body["blocks"] = blocks.clone();
        }
        let resp = self
            .client
            .post(self.url("chat.postMessage"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_slack_json(resp).await?;
        let parsed: PostMessageResponse = serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("chat.postMessage decode: {e}")))?;
        Ok(parsed)
    }

    /// `chat.postEphemeral` — text visible to a single user only.
    pub async fn post_ephemeral(
        &self,
        channel: &str,
        user: &str,
        thread_ts: Option<&str>,
        text: &str,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut body = json!({"channel": channel, "user": user, "text": text});
        if let Some(thread) = thread_ts {
            body["thread_ts"] = Value::String(thread.to_owned());
        }
        let resp = self
            .client
            .post(self.url("chat.postEphemeral"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_slack_json(resp).await?;
        let parsed: PostMessageResponse = serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("chat.postEphemeral decode: {e}")))?;
        Ok(parsed)
    }

    /// `chat.update` — edit a previously-posted message.
    pub async fn chat_update(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<PostMessageResponse, AdapterError> {
        let body = json!({"channel": channel, "ts": ts, "text": text});
        let resp = self
            .client
            .post(self.url("chat.update"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_slack_json(resp).await?;
        let parsed: PostMessageResponse = serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("chat.update decode: {e}")))?;
        Ok(parsed)
    }

    /// `reactions.add` — add an emoji reaction to a message.
    pub async fn reactions_add(
        &self,
        channel: &str,
        timestamp: &str,
        name: &str,
    ) -> Result<(), AdapterError> {
        let body = json!({"channel": channel, "timestamp": timestamp, "name": name});
        let resp = self
            .client
            .post(self.url("reactions.add"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_slack_json(resp).await?;
        Ok(())
    }

    /// `assistant.threads.setStatus` — best-effort typing indicator. Only
    /// effective inside an Assistants context; otherwise Slack returns a
    /// benign error which we surface as [`AdapterError::BadRequest`].
    pub async fn set_assistant_status(
        &self,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> Result<(), AdapterError> {
        let body = json!({
            "channel_id": channel,
            "thread_ts": thread_ts,
            "status": status
        });
        let resp = self
            .client
            .post(self.url("assistant.threads.setStatus"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_slack_json(resp).await?;
        Ok(())
    }

    /// Step 1 of file upload v2: get an external upload URL + file id.
    pub async fn files_get_upload_url_external(
        &self,
        filename: &str,
        length: usize,
    ) -> Result<GetUploadUrlResponse, AdapterError> {
        let body = json!({
            "filename": filename,
            "length": length,
        });
        let resp = self
            .client
            .post(self.url("files.getUploadURLExternal"))
            .bearer_auth(&self.bot_token)
            .form(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_slack_json(resp).await?;
        let parsed: GetUploadUrlResponse = serde_json::from_value(value).map_err(|e| {
            AdapterError::Transport(format!("files.getUploadURLExternal decode: {e}"))
        })?;
        Ok(parsed)
    }

    /// Step 2 of file upload v2: PUT bytes to the supplied upload URL.
    ///
    /// (Slack accepts POST too, but we use POST to match their reference
    /// flow.) Returns the body for tests; only the 2xx status matters.
    pub async fn files_upload_to_url(
        &self,
        upload_url: &str,
        bytes: Vec<u8>,
    ) -> Result<(), AdapterError> {
        let resp = self
            .client
            .post(upload_url)
            .body(bytes)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AdapterError::Transport(format!(
                "file upload to slack returned {status}"
            )));
        }
        Ok(())
    }

    /// Step 3 of file upload v2: finalize the upload(s) and (optionally)
    /// share into the given channel.
    pub async fn files_complete_upload_external(
        &self,
        files: &[CompleteUploadEntry],
        channel: Option<&str>,
        thread_ts: Option<&str>,
    ) -> Result<(), AdapterError> {
        let mut body = json!({"files": files});
        if let Some(channel) = channel {
            body["channel_id"] = Value::String(channel.to_owned());
        }
        if let Some(thread) = thread_ts {
            body["thread_ts"] = Value::String(thread.to_owned());
        }
        let resp = self
            .client
            .post(self.url("files.completeUploadExternal"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_slack_json(resp).await?;
        Ok(())
    }
}

fn transport(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(err.to_string())
}

async fn read_slack_json(resp: reqwest::Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AdapterError::Rate { retry_after });
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::Transport(format!(
            "slack returned {status}: {body}"
        )));
    }
    let value: Value = resp
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("slack response not JSON: {e}")))?;
    classify_slack_payload(value, retry_after)
}

/// Slack returns 200 OK with `{"ok": false, "error": "..."}` for logical
/// errors. Lift those into typed `AdapterError`s. Public so tests in the
/// adapter layer can poke it without round-tripping HTTP.
pub(crate) fn classify_slack_payload(
    value: Value,
    retry_after: Option<u64>,
) -> Result<Value, AdapterError> {
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if ok {
        return Ok(value);
    }
    let err = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown_error")
        .to_owned();
    Err(map_slack_error(&err, retry_after))
}

pub(crate) fn map_slack_error(code: &str, retry_after: Option<u64>) -> AdapterError {
    match code {
        "not_authed" | "invalid_auth" | "token_revoked" | "account_inactive" => {
            AdapterError::Auth(code.to_owned())
        }
        "ratelimited" | "rate_limited" => AdapterError::Rate { retry_after },
        other => AdapterError::BadRequest(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_slack_error_auth_codes() {
        for code in ["not_authed", "invalid_auth", "token_revoked", "account_inactive"] {
            match map_slack_error(code, None) {
                AdapterError::Auth(c) => assert_eq!(c, code),
                other => panic!("expected Auth for {code}, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_slack_error_rate_limited_uses_retry_after() {
        for code in ["ratelimited", "rate_limited"] {
            match map_slack_error(code, Some(42)) {
                AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(42)),
                other => panic!("expected Rate, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_slack_error_other_is_bad_request() {
        match map_slack_error("channel_not_found", None) {
            AdapterError::BadRequest(c) => assert_eq!(c, "channel_not_found"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_returns_value_when_ok() {
        let v = json!({"ok": true, "ts": "1.2"});
        let got = classify_slack_payload(v.clone(), None).unwrap();
        assert_eq!(got["ts"], "1.2");
    }

    #[test]
    fn classify_lifts_error_to_auth() {
        let v = json!({"ok": false, "error": "invalid_auth"});
        match classify_slack_payload(v, None).unwrap_err() {
            AdapterError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[test]
    fn classify_lifts_unknown_error() {
        let v = json!({"ok": false});
        match classify_slack_payload(v, None).unwrap_err() {
            AdapterError::BadRequest(s) => assert_eq!(s, "unknown_error"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn slack_api_builds_url_correctly() {
        let api = SlackApi::new("https://example.test/api/", "xoxb-x");
        assert_eq!(api.url("chat.postMessage"), "https://example.test/api/chat.postMessage");
        let api = SlackApi::new("https://example.test/api", "xoxb-x");
        assert_eq!(api.url("chat.postMessage"), "https://example.test/api/chat.postMessage");
    }

    #[test]
    fn slack_api_clone_and_debug() {
        let api = SlackApi::new("https://example.test/api", "xoxb-x");
        let _ = api.clone();
        assert!(format!("{api:?}").contains("xoxb-x"));
    }

    #[test]
    fn complete_upload_entry_skips_none_title() {
        let entry = CompleteUploadEntry {
            id: "F1".into(),
            title: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, "{\"id\":\"F1\"}");
        let entry = CompleteUploadEntry {
            id: "F1".into(),
            title: Some("hi".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"title\":\"hi\""));
    }

    #[test]
    fn post_message_response_accepts_message_ts_alias() {
        let v = json!({"ts":"123"});
        let parsed: PostMessageResponse = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.ts.as_deref(), Some("123"));
        let v = json!({"message_ts":"456"});
        let parsed: PostMessageResponse = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.ts.as_deref(), Some("456"));
    }
}
