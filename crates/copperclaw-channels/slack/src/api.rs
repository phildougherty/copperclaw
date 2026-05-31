//! Slack Web API client.
//!
//! Wraps the small slice of Slack endpoints the adapter needs. Slack returns
//! HTTP 200 even for logical failures (with `{"ok": false, "error": "..."}`),
//! so the client lifts those into [`AdapterError`].

use copperclaw_channels_core::{AdapterError, Card, CardButton};
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

/// Render a canonical [`Card`] into a Slack Block Kit `blocks` array.
///
/// Mapping:
///
/// - `card.title` -> `header` block (Slack caps header text at 150 chars; we
///   pre-trim defensively so a long agent-generated title doesn't trip the
///   400-validation path).
/// - `card.body` -> `section` block with `mrkdwn` text. If the card carries an
///   `image_url` it rides as the section's `accessory` (a small thumbnail to
///   the right of the text) so the body and image stay visually paired.
/// - `card.image_url` standalone (no body) -> dedicated `image` block.
/// - `card.fields` -> a single `section` block with a `fields` array, two
///   columns per row in Slack's UI; each entry is a `mrkdwn` `*label*\nvalue`
///   string. Slack caps `fields` per section at 10 — we split into multiple
///   section blocks when we exceed that.
/// - `card.buttons` -> one `actions` block with one `button` element per
///   button. `value` buttons set `value` and (when `style == "primary" |
///   "danger"`) the matching `style` field; `url` buttons set `url`. Slack
///   allows up to 25 elements per actions block, well above the card's hard
///   limit of 8 buttons, so we never need to chunk on the Slack side.
///
/// Every `action_id` is set to `card_btn_<index>` so the events router can
/// trace the tap back to the position-in-card without us round-tripping the
/// original card payload.
#[must_use]
pub fn build_card_blocks(card: &Card) -> Value {
    /// Slack's per-`header` text cap (`plain_text`, max 150 chars).
    const HEADER_TEXT_CAP: usize = 150;
    /// Slack's per-`section.fields` cap.
    const FIELDS_PER_SECTION_CAP: usize = 10;

    let mut blocks: Vec<Value> = Vec::new();

    if let Some(title) = card.title.as_deref() {
        let t = title.trim();
        if !t.is_empty() {
            let trimmed: String = t.chars().take(HEADER_TEXT_CAP).collect();
            blocks.push(json!({
                "type": "header",
                "text": { "type": "plain_text", "text": trimmed, "emoji": true }
            }));
        }
    }

    // Body section. Carries the image as an `accessory` when both are present
    // so the message keeps visual cohesion; if there's an image but no body,
    // the image gets its own `image` block lower down.
    if let Some(body) = card.body.as_deref() {
        let b = body.trim();
        if !b.is_empty() {
            let mut section = json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": b }
            });
            if let Some(img) = card.image_url.as_deref() {
                let img = img.trim();
                if !img.is_empty() {
                    section["accessory"] = json!({
                        "type": "image",
                        "image_url": img,
                        "alt_text": "card image"
                    });
                }
            }
            blocks.push(section);
        }
    }

    // Image-only card (no body): standalone image block.
    let body_carries_image = card
        .body
        .as_deref()
        .is_some_and(|b| !b.trim().is_empty());
    if !body_carries_image {
        if let Some(img) = card.image_url.as_deref() {
            let img = img.trim();
            if !img.is_empty() {
                blocks.push(json!({
                    "type": "image",
                    "image_url": img,
                    "alt_text": "card image"
                }));
            }
        }
    }

    if !card.fields.is_empty() {
        for chunk in card.fields.chunks(FIELDS_PER_SECTION_CAP) {
            let fields: Vec<Value> = chunk
                .iter()
                .map(|f| {
                    json!({
                        "type": "mrkdwn",
                        "text": format!("*{}*\n{}", f.label, f.value)
                    })
                })
                .collect();
            blocks.push(json!({
                "type": "section",
                "fields": fields
            }));
        }
    }

    if !card.buttons.is_empty() {
        let elements: Vec<Value> = card
            .buttons
            .iter()
            .enumerate()
            .filter_map(|(i, b)| button_to_element(i, b))
            .collect();
        if !elements.is_empty() {
            blocks.push(json!({
                "type": "actions",
                "block_id": "card_actions",
                "elements": elements,
            }));
        }
    }

    Value::Array(blocks)
}

/// Map one canonical [`CardButton`] to a Slack Block Kit button element.
/// `index` is the button's position in the card; we encode it into the
/// `action_id` (`card_btn_<index>`) so the interactive-payload handler can
/// thread the tap back to the button without round-tripping the whole card.
fn button_to_element(index: usize, btn: &CardButton) -> Option<Value> {
    let text = json!({"type": "plain_text", "text": btn.label.clone(), "emoji": true});
    let action_id = format!("card_btn_{index}");
    let mut out = json!({
        "type": "button",
        "action_id": action_id,
        "text": text,
    });
    match (btn.value.as_deref(), btn.url.as_deref()) {
        (Some(v), None) => {
            out["value"] = Value::String(v.to_owned());
            // Slack only accepts the exact strings "primary" or "danger";
            // anything else (the default secondary look) must omit `style`.
            if let Some(style) = btn.style.as_deref() {
                if matches!(style, "primary" | "danger") {
                    out["style"] = Value::String(style.to_owned());
                }
            }
            Some(out)
        }
        (None, Some(u)) => {
            out["url"] = Value::String(u.to_owned());
            Some(out)
        }
        // The card validator rejects these shapes — fall through silently if
        // a malformed card slips past.
        (Some(_), Some(_)) | (None, None) => None,
    }
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

    /// `chat.postMessage` — text + (optional) attachments. The
    /// secondary-message-attachment API is the way to drive a coloured
    /// left bar (`attachments[].color = "danger"` for red, etc.) which
    /// Block Kit's primary blocks API can't produce on its own. Used
    /// by `deliver_error` to render an error card with the red bar
    /// affordance.
    pub async fn post_message_with_attachments(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        text: &str,
        attachments: &Value,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut body = json!({
            "channel": channel,
            "text": text,
            "attachments": attachments.clone(),
        });
        if let Some(thread) = thread_ts {
            body["thread_ts"] = Value::String(thread.to_owned());
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
        let parsed: PostMessageResponse = serde_json::from_value(value).map_err(|e| {
            AdapterError::Transport(format!(
                "chat.postMessage (attachments) decode: {e}"
            ))
        })?;
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
        self.chat_update_with_blocks(channel, ts, text, None).await
    }

    /// As [`chat_update`] but lets the caller pass an optional `blocks`
    /// array so chip-style edits (Block Kit `context` blocks used by
    /// `deliver_breadcrumb`) keep their structured rendering on update.
    pub async fn chat_update_with_blocks(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
        blocks: Option<&Value>,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut body = json!({"channel": channel, "ts": ts, "text": text});
        if let Some(blocks) = blocks {
            body["blocks"] = blocks.clone();
        }
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

    /// `pins.add` — pin a message to a channel. Used by
    /// [`crate::adapter::SlackAdapter::deliver_todo_list`] on the
    /// first emit so the todo chip stays surfaced in the channel
    /// pins. Best-effort: the bot may lack the `pins:write` scope
    /// (legacy workspaces), in which case Slack returns
    /// `not_authed` and the caller swallows.
    pub async fn pins_add(
        &self,
        channel: &str,
        timestamp: &str,
    ) -> Result<(), AdapterError> {
        let body = json!({"channel": channel, "timestamp": timestamp});
        let resp = self
            .client
            .post(self.url("pins.add"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_slack_json(resp).await?;
        Ok(())
    }

    /// `pins.remove` — unpin a previously-pinned message. Called
    /// when a [`TodoList`](copperclaw_channels_core::TodoList) transitions
    /// to fully-completed so the channel pin list doesn't fill with
    /// stale finished plans.
    pub async fn pins_remove(
        &self,
        channel: &str,
        timestamp: &str,
    ) -> Result<(), AdapterError> {
        let body = json!({"channel": channel, "timestamp": timestamp});
        let resp = self
            .client
            .post(self.url("pins.remove"))
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
    fn build_card_blocks_full_card_shape() {
        use copperclaw_channels_core::{CardButton, CardField};
        let card = Card {
            title: Some("Approve deploy?".into()),
            body: Some("Push green to prod-canary?".into()),
            fields: vec![
                CardField {
                    label: "Branch".into(),
                    value: "main".into(),
                    inline: true,
                },
                CardField {
                    label: "Commit".into(),
                    value: "a1b2c3d".into(),
                    inline: true,
                },
            ],
            buttons: vec![
                CardButton {
                    label: "Yes".into(),
                    value: Some("deploy:yes".into()),
                    url: None,
                    style: Some("primary".into()),
                },
                CardButton {
                    label: "Docs".into(),
                    value: None,
                    url: Some("https://example.com/docs".into()),
                    style: None,
                },
            ],
            image_url: Some("https://example.com/x.png".into()),
        };
        let blocks = build_card_blocks(&card);
        let arr = blocks.as_array().expect("array");
        // header / section(body+accessory) / section(fields) / actions
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["type"], "header");
        assert_eq!(arr[0]["text"]["text"], "Approve deploy?");
        assert_eq!(arr[1]["type"], "section");
        assert_eq!(arr[1]["text"]["text"], "Push green to prod-canary?");
        assert_eq!(arr[1]["accessory"]["type"], "image");
        assert_eq!(arr[1]["accessory"]["image_url"], "https://example.com/x.png");
        assert_eq!(arr[2]["type"], "section");
        assert_eq!(arr[2]["fields"].as_array().unwrap().len(), 2);
        assert_eq!(arr[3]["type"], "actions");
        assert_eq!(arr[3]["block_id"], "card_actions");
        let elements = arr[3]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0]["action_id"], "card_btn_0");
        assert_eq!(elements[0]["value"], "deploy:yes");
        assert_eq!(elements[0]["style"], "primary");
        assert_eq!(elements[1]["action_id"], "card_btn_1");
        assert_eq!(elements[1]["url"], "https://example.com/docs");
        assert!(elements[1].get("value").is_none());
        assert!(elements[1].get("style").is_none());
    }

    #[test]
    fn build_card_blocks_image_only_emits_image_block() {
        let card = Card {
            title: Some("Snap".into()),
            image_url: Some("https://example.com/p.png".into()),
            ..Card::default()
        };
        let blocks = build_card_blocks(&card);
        let arr = blocks.as_array().unwrap();
        // header + standalone image (no body to attach accessory to).
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[1]["type"], "image");
        assert_eq!(arr[1]["image_url"], "https://example.com/p.png");
    }

    #[test]
    fn build_card_blocks_unsupported_button_style_is_omitted() {
        let card = Card {
            title: Some("hi".into()),
            buttons: vec![CardButton {
                label: "Maybe".into(),
                value: Some("m".into()),
                url: None,
                style: Some("secondary".into()),
            }],
            ..Card::default()
        };
        let arr = build_card_blocks(&card)
            .as_array()
            .cloned()
            .unwrap();
        let btn = &arr.last().unwrap()["elements"][0];
        assert!(btn.get("style").is_none());
    }

    #[test]
    fn build_card_blocks_long_header_truncated() {
        let long = "a".repeat(500);
        let card = Card {
            title: Some(long),
            body: Some("x".into()),
            ..Card::default()
        };
        let blocks = build_card_blocks(&card);
        let header_text = blocks[0]["text"]["text"].as_str().unwrap();
        assert_eq!(header_text.chars().count(), 150);
    }

    #[test]
    fn build_card_blocks_field_chunking_at_ten() {
        let fields = (0..15)
            .map(|i| copperclaw_channels_core::CardField {
                label: format!("L{i}"),
                value: format!("V{i}"),
                inline: false,
            })
            .collect();
        // Note: card validator caps fields at 25 — 15 is legal and forces a
        // chunk split (10 + 5) because Slack section.fields caps at 10.
        let card = Card {
            title: Some("hi".into()),
            fields,
            ..Card::default()
        };
        let blocks = build_card_blocks(&card);
        let arr = blocks.as_array().unwrap();
        // header + 2 field-sections = 3
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[1]["fields"].as_array().unwrap().len(), 10);
        assert_eq!(arr[2]["fields"].as_array().unwrap().len(), 5);
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
