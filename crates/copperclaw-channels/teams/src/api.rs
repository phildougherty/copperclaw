//! Microsoft Graph API client for the slice of endpoints the Teams adapter
//! needs.
//!
//! All calls authenticate with `Authorization: Bearer <bot_token>`. HTTP
//! status codes are mapped onto [`AdapterError`] variants according to the
//! contract documented in `docs/adding-a-channel.md`:
//!
//! - 401 / 403 → [`AdapterError::Auth`].
//! - 404 → [`AdapterError::BadRequest`].
//! - 429 → [`AdapterError::Rate`] (honoring `Retry-After` when present).
//! - 5xx → [`AdapterError::Transport`].
//! - 400 / 422 → [`AdapterError::BadRequest`].

use copperclaw_channels_core::{
    AdapterError, Breadcrumb, BreadcrumbStatus, Card, CardButton, DiffCard, ErrorCard,
    ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
};
use reqwest::{Client, Method, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

/// Response from a message-create or message-edit call. Only the `id` field
/// (the Microsoft Graph chat-message id) is interesting to us.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessageResponse {
    /// Microsoft Graph chat-message id.
    pub id: String,
}

/// Resolved drive + drive-item pair for a channel's files folder. The
/// channel's `SharePoint` document library is opaque from the channel
/// id; Graph returns these once via `GET .../filesFolder` and the
/// caller passes them back on subsequent uploads.
#[derive(Debug, Clone)]
pub struct DriveItemRef {
    /// Graph drive id (`SharePoint` document library).
    pub drive_id: String,
    /// Drive-item id of the *folder* (not the upload target itself).
    pub item_id: String,
}

/// Reference-style attachment metadata produced by an upload. Suitable
/// for inclusion on a Graph chat-message body's `attachments` array.
#[derive(Debug, Clone)]
pub struct GraphAttachment {
    /// driveItem id of the uploaded file (also used as the
    /// attachment's `id`).
    pub id: String,
    /// `webUrl` of the uploaded driveItem; Teams resolves this back
    /// to the file inline.
    pub content_url: String,
    /// Display name for the attachment.
    pub name: String,
}

/// Response from `GET /chats/{id}`. We use it to decide `is_group`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatInfoResponse {
    /// `oneOnOne`, `group`, `meeting`, or `unknownFutureValue`.
    #[serde(default, rename = "chatType")]
    pub chat_type: Option<String>,
}

/// Minimal Microsoft Graph client.
#[derive(Debug, Clone)]
pub struct TeamsApi {
    client: Client,
    graph_base: String,
    bot_token: String,
}

impl TeamsApi {
    /// Build a client using the configured token and base URL.
    #[must_use]
    pub fn new(graph_base: impl Into<String>, bot_token: impl Into<String>) -> Self {
        Self::with_client(Client::new(), graph_base, bot_token)
    }

    /// Construct with a caller-supplied [`reqwest::Client`]. Useful for tests
    /// that want a shared connection pool or custom timeouts.
    #[must_use]
    pub fn with_client(
        client: Client,
        graph_base: impl Into<String>,
        bot_token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            graph_base: graph_base.into(),
            bot_token: bot_token.into(),
        }
    }

    /// Build an absolute URL by appending `suffix` to the configured base.
    pub(crate) fn url(&self, suffix: &str) -> String {
        let base = self.graph_base.trim_end_matches('/');
        let suffix = suffix.trim_start_matches('/');
        format!("{base}/{suffix}")
    }

    /// `POST /teams/{teamId}/channels/{channelId}/messages` — create a new
    /// channel post.
    pub async fn post_channel_message(
        &self,
        team_id: &str,
        channel_id: &str,
        content: &str,
        content_type: &str,
    ) -> Result<ChatMessageResponse, AdapterError> {
        self.post_channel_message_with_attachments(
            team_id,
            channel_id,
            content,
            content_type,
            &[],
        )
        .await
    }

    /// `POST /teams/{teamId}/channels/{channelId}/messages` with optional
    /// `attachments` (reference-style — produced by
    /// [`Self::upload_channel_file`]).
    pub async fn post_channel_message_with_attachments(
        &self,
        team_id: &str,
        channel_id: &str,
        content: &str,
        content_type: &str,
        attachments: &[GraphAttachment],
    ) -> Result<ChatMessageResponse, AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/messages"
        ));
        let body = build_message_body(content, content_type, attachments);
        let resp = self.send_json(Method::POST, &url, &body).await?;
        decode_message(resp).await
    }

    /// `GET /teams/{teamId}/channels/{channelId}/filesFolder` — resolve
    /// the drive + drive-item id of the channel's files folder. Required
    /// before uploading: the `SharePoint` document library a channel maps
    /// to isn't predictable from the channel id.
    pub async fn get_channel_files_folder(
        &self,
        team_id: &str,
        channel_id: &str,
    ) -> Result<DriveItemRef, AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/filesFolder"
        ));
        let resp = self.send_empty(Method::GET, &url).await?;
        let value = consume_json(resp).await?;
        let drive_id = value
            .pointer("/parentReference/driveId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdapterError::Transport(
                    "filesFolder response missing parentReference.driveId".into(),
                )
            })?
            .to_string();
        let item_id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdapterError::Transport("filesFolder response missing id".into())
            })?
            .to_string();
        Ok(DriveItemRef { drive_id, item_id })
    }

    /// `PUT /drives/{driveId}/items/{itemId}:/{filename}:/content` — upload
    /// `bytes` into the channel's files folder. Returns a reference
    /// suitable for [`Self::post_channel_message_with_attachments`].
    ///
    /// Path-style upload tops out at the Graph 4 MB ceiling; larger
    /// payloads need an upload session, which this helper does not yet
    /// open. Larger files surface as `BadRequest` from Graph's 4xx with the
    /// `RequestEntityTooLarge` code; the adapter propagates that.
    pub async fn upload_channel_file(
        &self,
        folder: &DriveItemRef,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<GraphAttachment, AdapterError> {
        if filename.is_empty() {
            return Err(AdapterError::BadRequest(
                "upload_channel_file: empty filename".into(),
            ));
        }
        // Graph path-style upload: `…/items/{itemId}:/{filename}:/content`.
        // The filename segment must be URL-encoded; spaces in particular
        // are tolerated as `%20`.
        let encoded = urlencode_segment(filename);
        let url = self.url(&format!(
            "drives/{}/items/{}:/{}:/content",
            folder.drive_id, folder.item_id, encoded
        ));
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.bot_token)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/octet-stream",
            )
            .body(bytes)
            .send()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        let value = consume_json(resp).await?;
        let drive_item_id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::Transport("upload response missing id".into()))?
            .to_string();
        let web_url = value
            .get("webUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| AdapterError::Transport("upload response missing webUrl".into()))?
            .to_string();
        Ok(GraphAttachment {
            id: drive_item_id,
            content_url: web_url,
            name: filename.to_string(),
        })
    }

    /// `POST /teams/{teamId}/channels/{channelId}/messages/{messageId}/replies`
    /// — reply within an existing channel thread.
    pub async fn post_channel_reply(
        &self,
        team_id: &str,
        channel_id: &str,
        parent_message_id: &str,
        content: &str,
        content_type: &str,
    ) -> Result<ChatMessageResponse, AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/messages/{parent_message_id}/replies"
        ));
        let body = json!({"body": {"contentType": content_type, "content": content}});
        let resp = self.send_json(Method::POST, &url, &body).await?;
        decode_message(resp).await
    }

    /// `POST /chats/{chatId}/messages` — post into a one-on-one or group chat.
    pub async fn post_chat_message(
        &self,
        chat_id: &str,
        content: &str,
        content_type: &str,
    ) -> Result<ChatMessageResponse, AdapterError> {
        let url = self.url(&format!("chats/{chat_id}/messages"));
        let body = json!({"body": {"contentType": content_type, "content": content}});
        let resp = self.send_json(Method::POST, &url, &body).await?;
        decode_message(resp).await
    }

    /// `PATCH /teams/{teamId}/channels/{channelId}/messages/{messageId}` —
    /// edit a previously-sent channel message body.
    pub async fn edit_channel_message(
        &self,
        team_id: &str,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/messages/{message_id}"
        ));
        let body = json!({"body": {"contentType": "html", "content": content}});
        let resp = self.send_json(Method::PATCH, &url, &body).await?;
        let _ = consume_ok(resp).await?;
        Ok(())
    }

    /// `PATCH /chats/{chatId}/messages/{messageId}` — edit a chat message body.
    pub async fn edit_chat_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<(), AdapterError> {
        let url = self.url(&format!("chats/{chat_id}/messages/{message_id}"));
        let body = json!({"body": {"contentType": "html", "content": content}});
        let resp = self.send_json(Method::PATCH, &url, &body).await?;
        let _ = consume_ok(resp).await?;
        Ok(())
    }

    /// `POST .../messages/{id}/setReaction` for a channel message.
    pub async fn set_channel_reaction(
        &self,
        team_id: &str,
        channel_id: &str,
        message_id: &str,
        reaction_type: &str,
    ) -> Result<(), AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/messages/{message_id}/setReaction"
        ));
        let body = json!({"reactionType": reaction_type});
        let resp = self.send_json(Method::POST, &url, &body).await?;
        let _ = consume_ok(resp).await?;
        Ok(())
    }

    /// `POST /chats/{id}/messages/{mid}/setReaction` — set a reaction on a
    /// chat message.
    pub async fn set_chat_reaction(
        &self,
        chat_id: &str,
        message_id: &str,
        reaction_type: &str,
    ) -> Result<(), AdapterError> {
        let url = self.url(&format!(
            "chats/{chat_id}/messages/{message_id}/setReaction"
        ));
        let body = json!({"reactionType": reaction_type});
        let resp = self.send_json(Method::POST, &url, &body).await?;
        let _ = consume_ok(resp).await?;
        Ok(())
    }

    /// `GET /teams/{teamId}/channels/{channelId}/messages/{messageId}` —
    /// fetch a channel message. Returns the raw JSON for the caller to mine.
    pub async fn get_channel_message(
        &self,
        team_id: &str,
        channel_id: &str,
        message_id: &str,
    ) -> Result<Value, AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/messages/{message_id}"
        ));
        let resp = self.send_empty(Method::GET, &url).await?;
        consume_json(resp).await
    }

    /// `GET /chats/{chatId}/messages/{messageId}` — fetch a chat message.
    pub async fn get_chat_message(
        &self,
        chat_id: &str,
        message_id: &str,
    ) -> Result<Value, AdapterError> {
        let url = self.url(&format!("chats/{chat_id}/messages/{message_id}"));
        let resp = self.send_empty(Method::GET, &url).await?;
        consume_json(resp).await
    }

    /// `GET /chats/{chatId}` — fetch chat metadata (used to compute
    /// `is_group` for inbound chat notifications).
    pub async fn get_chat(&self, chat_id: &str) -> Result<ChatInfoResponse, AdapterError> {
        let url = self.url(&format!("chats/{chat_id}"));
        let resp = self.send_empty(Method::GET, &url).await?;
        let value = consume_json(resp).await?;
        serde_json::from_value(value)
            .map_err(|e| AdapterError::Transport(format!("get chat decode: {e}")))
    }

    async fn send_json(
        &self,
        method: Method,
        url: &str,
        body: &Value,
    ) -> Result<Response, AdapterError> {
        self.client
            .request(method, url)
            .bearer_auth(&self.bot_token)
            .json(body)
            .send()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))
    }

    async fn send_empty(&self, method: Method, url: &str) -> Result<Response, AdapterError> {
        self.client
            .request(method, url)
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))
    }
}

/// Translate a Microsoft Graph HTTP response into an [`AdapterError`] or
/// return the raw response on success.
pub(crate) async fn consume_ok(resp: Response) -> Result<Response, AdapterError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let body = resp.text().await.unwrap_or_default();
    Err(map_status_error(status, &body, retry_after))
}

/// Like [`consume_ok`] but additionally decodes the body as JSON.
pub(crate) async fn consume_json(resp: Response) -> Result<Value, AdapterError> {
    let resp = consume_ok(resp).await?;
    resp.json::<Value>()
        .await
        .map_err(|e| AdapterError::Transport(format!("graph response not JSON: {e}")))
}

async fn decode_message(resp: Response) -> Result<ChatMessageResponse, AdapterError> {
    let value = consume_json(resp).await?;
    serde_json::from_value(value)
        .map_err(|e| AdapterError::Transport(format!("graph message decode: {e}")))
}

// ---------------------------------------------------------------------------
// Adaptive Cards (slice-4 native renderers).
//
// Microsoft Teams renders the seven canonical surfaces (Card, Breadcrumb,
// Diff, Error, TodoList, Collapsible, Thinking) via the Adaptive Cards
// schema attached to a `chatMessage` as
// `application/vnd.microsoft.card.adaptive`. The Graph wire shape for an
// adaptive card attachment is a JSON-string `content` field with the
// card itself stringified — NOT a nested object. The `body` of the
// owning message must reference the attachment via an
// `<attachment id="…"></attachment>` placeholder.
//
// The builders below produce the canonical (deserialized) card JSON; the
// `build_adaptive_message_body` helper attaches it as a Graph attachment
// and inserts the placeholder for you.
// ---------------------------------------------------------------------------

/// Adaptive Cards schema version we target. 1.4 is the highest spec MS
/// Teams currently renders fully on both desktop and mobile clients;
/// 1.5 has partial support. `CodeBlock` is 1.5-only — we feature-gate
/// it via the helper `supports_codeblock` so older clients fall back to
/// monospace `TextBlock`.
pub(crate) const ADAPTIVE_CARD_SCHEMA: &str = "http://adaptivecards.io/schemas/adaptive-card.json";
pub(crate) const ADAPTIVE_CARD_VERSION: &str = "1.4";

/// Render a canonical [`Card`] into an Adaptive Card JSON Value.
///
/// Mapping:
///
/// - `card.title` → first `TextBlock` (large, bold).
/// - `card.body` → `TextBlock` (default size, `wrap: true`).
/// - `card.image_url` → `Image` element (stretch: medium).
/// - `card.fields` → single `FactSet` with one `Fact` per row.
/// - `card.buttons` (`value`) → `Action.Submit` with `data: {value}`.
/// - `card.buttons` (`url`) → `Action.OpenUrl` with the URL.
/// - `card.buttons.style == "primary"` → `style: "positive"`.
/// - `card.buttons.style == "danger"` → `style: "destructive"`.
#[must_use]
pub fn build_adaptive_card(card: &Card) -> Value {
    let mut body: Vec<Value> = Vec::new();
    if let Some(t) = card.title.as_deref() {
        let t = t.trim();
        if !t.is_empty() {
            body.push(json!({
                "type": "TextBlock",
                "text": t,
                "weight": "Bolder",
                "size": "Large",
                "wrap": true,
            }));
        }
    }
    if let Some(b) = card.body.as_deref() {
        let b = b.trim();
        if !b.is_empty() {
            body.push(json!({
                "type": "TextBlock",
                "text": b,
                "wrap": true,
            }));
        }
    }
    if let Some(img) = card.image_url.as_deref() {
        let img = img.trim();
        if !img.is_empty() {
            body.push(json!({
                "type": "Image",
                "url": img,
                "size": "Medium",
                "altText": "card image",
            }));
        }
    }
    if !card.fields.is_empty() {
        let facts: Vec<Value> = card
            .fields
            .iter()
            .map(|f| json!({"title": f.label, "value": f.value}))
            .collect();
        body.push(json!({
            "type": "FactSet",
            "facts": facts,
        }));
    }
    let actions: Vec<Value> = card
        .buttons
        .iter()
        .filter_map(button_to_action)
        .collect();
    let mut out = json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": body,
    });
    if !actions.is_empty() {
        out["actions"] = Value::Array(actions);
    }
    out
}

/// Map a canonical [`CardButton`] to an Adaptive Card `Action`.
///
/// `value` buttons emit `Action.Submit` carrying `{"value": "<value>"}`
/// in their `data` field — Teams routes a submit back to the bot via
/// the `messages` activity with the data attached, which the host's
/// inbound router can pick up. `url` buttons emit `Action.OpenUrl`.
fn button_to_action(btn: &CardButton) -> Option<Value> {
    match (btn.value.as_deref(), btn.url.as_deref()) {
        (Some(v), None) => {
            let style = match btn.style.as_deref() {
                Some("primary") => "positive",
                Some("danger") => "destructive",
                _ => "default",
            };
            Some(json!({
                "type": "Action.Submit",
                "title": btn.label,
                "style": style,
                "data": { "value": v },
            }))
        }
        (None, Some(u)) => Some(json!({
            "type": "Action.OpenUrl",
            "title": btn.label,
            "url": u,
        })),
        _ => None,
    }
}

/// Render a [`Breadcrumb`] into a small accent-coloured Adaptive Card.
/// Used by both the initial emit and the edit-in-place PATCH.
#[must_use]
pub fn build_adaptive_breadcrumb(b: &Breadcrumb) -> Value {
    let color = match b.status {
        BreadcrumbStatus::Running => "Accent",
        BreadcrumbStatus::Done => "Good",
        BreadcrumbStatus::Failed => "Attention",
    };
    let mut text = format!("`{}`", b.tool_name);
    if let Some(d) = b.detail.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            text.push_str(" \u{00B7} ");
            text.push_str(d);
        }
    }
    if let Some(s) = b.summary.as_deref() {
        let s = s.trim();
        if !s.is_empty() {
            if b.status == BreadcrumbStatus::Failed {
                text.push_str(" — failed: ");
            } else {
                text.push_str(" — ");
            }
            text.push_str(s);
        }
    }
    json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": [{
            "type": "TextBlock",
            "text": text,
            "size": "Small",
            "color": color,
            "isSubtle": true,
            "wrap": true,
        }],
    })
}

/// Render a [`DiffCard`] into an Adaptive Card. The header `TextBlock`
/// carries `*<path>* (+N / -M)`; each hunk's body lands in a monospace
/// `TextBlock` (Adaptive Cards 1.4 doesn't ship a `CodeBlock` element
/// — that's 1.5+; we use `fontType: "Monospace"` for max compatibility).
#[must_use]
pub fn build_adaptive_diff(diff: &DiffCard) -> Value {
    let totals = if diff.truncated {
        format!("(+{} / -{} · truncated)", diff.added, diff.removed)
    } else {
        format!("(+{} / -{})", diff.added, diff.removed)
    };
    let mut body: Vec<Value> = Vec::with_capacity(1 + diff.hunks.len());
    body.push(json!({
        "type": "TextBlock",
        "text": format!("**{}**  {totals}", diff.path),
        "weight": "Bolder",
        "wrap": true,
    }));
    for h in &diff.hunks {
        let mut buf = String::with_capacity(64 + h.lines.len() * 64);
        buf.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        ));
        for line in &h.lines {
            buf.push(line.kind.unified_prefix());
            buf.push_str(&line.text);
            buf.push('\n');
        }
        // Trim trailing newline so the monospace block doesn't render an
        // empty bottom row.
        while buf.ends_with('\n') {
            buf.pop();
        }
        body.push(json!({
            "type": "TextBlock",
            "text": buf,
            "fontType": "Monospace",
            "wrap": true,
        }));
    }
    json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": body,
    })
}

/// Render an [`ErrorCard`] into an Adaptive Card with an
/// `attention`-styled top container, a bold red header, and an
/// optional monospace details block plus retry footer.
#[must_use]
pub fn build_adaptive_error(err: &ErrorCard) -> Value {
    let kind_label = match err.kind {
        ErrorCardKind::Internal => "tool",
        ErrorCardKind::Provider => "provider",
        ErrorCardKind::Delivery => "delivery",
    };
    let mut inner: Vec<Value> = Vec::with_capacity(4);
    inner.push(json!({
        "type": "TextBlock",
        "text": format!("[ERROR: {kind_label}] {}", err.title.trim()),
        "weight": "Bolder",
        "color": "Attention",
        "wrap": true,
    }));
    inner.push(json!({
        "type": "TextBlock",
        "text": err.summary.trim(),
        "wrap": true,
    }));
    if let Some(d) = err.details.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            inner.push(json!({
                "type": "TextBlock",
                "text": d,
                "fontType": "Monospace",
                "wrap": true,
                "isSubtle": true,
            }));
        }
    }
    if err.retryable {
        inner.push(json!({
            "type": "TextBlock",
            "text": "(will retry automatically)",
            "size": "Small",
            "isSubtle": true,
            "wrap": true,
        }));
    }
    json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": [{
            "type": "Container",
            "style": "attention",
            "items": inner,
        }],
    })
}

/// Render a [`TodoList`] into an Adaptive Card.
///
/// One row per item using `Input.Toggle` so each item displays as a
/// visual checkbox (read-only — Teams doesn't round-trip the toggle
/// state back to us in slice 4). A small `TextBlock` header carries
/// the title + `done/total` counter; a footer block summarises
/// in-progress / pending counts so the user can glance at the chip.
#[must_use]
pub fn build_adaptive_todo_list(list: &TodoList) -> Value {
    let done = list.completed_count();
    let total = list.items.len();
    let in_prog = list.in_progress_count();
    let pending = list.pending_count();
    let mut body: Vec<Value> = Vec::with_capacity(2 + list.items.len());
    body.push(json!({
        "type": "TextBlock",
        "text": format!("{} ({done}/{total})", list.title_or_default()),
        "weight": "Bolder",
        "wrap": true,
    }));
    for item in list.items.iter().take(48) {
        let (glyph, color) = match item.status {
            TodoItemStatus::Completed => ("[x]", "Good"),
            TodoItemStatus::InProgress => ("[~]", "Warning"),
            TodoItemStatus::Pending => ("[ ]", "Default"),
        };
        body.push(json!({
            "type": "TextBlock",
            "text": format!("{glyph} {}", item.text.trim()),
            "color": color,
            "wrap": true,
            "spacing": "Small",
        }));
    }
    body.push(json!({
        "type": "TextBlock",
        "text": format!("({done}/{total} done, {in_prog} in progress, {pending} pending)"),
        "size": "Small",
        "isSubtle": true,
        "spacing": "Medium",
        "wrap": true,
    }));
    json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": body,
    })
}

/// Render a long-output expander into an Adaptive Card.
///
/// Layout: visible summary `TextBlock` + (optional) preview lines, then a
/// `Container` holding the full body monospace `TextBlock` that defaults
/// to `isVisible: false`; an `Action.ToggleVisibility` button flips
/// the container on demand — the closest native disclosure-widget the
/// Adaptive Cards schema offers.
#[must_use]
pub fn build_adaptive_collapsible(text: &str, summary: &str, preview_lines: &[String]) -> Value {
    let mut body: Vec<Value> = Vec::with_capacity(4);
    body.push(json!({
        "type": "TextBlock",
        "text": summary.trim(),
        "weight": "Bolder",
        "wrap": true,
    }));
    if !preview_lines.is_empty() {
        body.push(json!({
            "type": "TextBlock",
            "text": preview_lines.join("\n"),
            "fontType": "Monospace",
            "wrap": true,
            "isSubtle": true,
        }));
    }
    body.push(json!({
        "type": "Container",
        "id": "expander_full",
        "isVisible": false,
        "items": [{
            "type": "TextBlock",
            "text": text,
            "fontType": "Monospace",
            "wrap": true,
        }],
    }));
    json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": body,
        "actions": [{
            "type": "Action.ToggleVisibility",
            "title": "Show full output",
            "targetElements": ["expander_full"],
        }],
    })
}

/// Render a [`ThinkingBlock`] into an Adaptive Card.
///
/// Header `TextBlock` (small, accent) carries a `_reasoning_` label
/// plus optional `(<model>)` provenance; the body lives inside a
/// `Container` with `isVisible: false`, paired with an
/// `Action.ToggleVisibility` button so reasoning stays out of the way
/// until the user asks for it. Redacted blocks emit the placeholder
/// inline (NEVER the raw blob) so the privacy contract is preserved.
#[must_use]
pub fn build_adaptive_thinking(t: &ThinkingBlock) -> Value {
    let label = match t.model.as_deref() {
        Some(m) if !m.trim().is_empty() => format!("_reasoning ({})_", m.trim()),
        _ => "_reasoning_".to_string(),
    };
    let mut body: Vec<Value> = Vec::with_capacity(2);
    body.push(json!({
        "type": "TextBlock",
        "text": label,
        "size": "Small",
        "color": "Accent",
        "isSubtle": true,
        "wrap": true,
    }));
    let inner_text = if t.redacted {
        "(redacted reasoning)".to_string()
    } else {
        t.text.clone()
    };
    body.push(json!({
        "type": "Container",
        "id": "thinking_full",
        "isVisible": false,
        "items": [{
            "type": "TextBlock",
            "text": inner_text,
            "wrap": true,
            "isSubtle": true,
        }],
    }));
    json!({
        "type": "AdaptiveCard",
        "$schema": ADAPTIVE_CARD_SCHEMA,
        "version": ADAPTIVE_CARD_VERSION,
        "body": body,
        "actions": [{
            "type": "Action.ToggleVisibility",
            "title": "Show reasoning",
            "targetElements": ["thinking_full"],
        }],
    })
}

/// Build a Graph chat-message body whose only attachment is the given
/// Adaptive Card. Returns a `body` carrying the
/// `<attachment id="…"></attachment>` placeholder + `attachments[]`
/// with the card under `application/vnd.microsoft.card.adaptive`.
///
/// `fallback_text` is used as a `text`-style fallback inside the
/// placeholder block so notification surfaces (mobile push, Outlook
/// activity feed digests) still render a human-readable preview when
/// the recipient's client can't load the adaptive card.
#[must_use]
pub fn build_adaptive_message_body(card_json: &Value, fallback_text: &str) -> Value {
    // Graph adaptive-card attachments require `content` to be a
    // **stringified** JSON blob, not a nested object — quirky historical
    // wire shape inherited from the Bot Framework cards spec.
    let content_str = serde_json::to_string(card_json).unwrap_or_else(|_| "{}".to_string());
    let attachment_id = adaptive_card_attachment_id(card_json);
    let escaped_fallback = html_escape(fallback_text);
    let body_html = format!(
        "<p>{escaped_fallback}</p><attachment id=\"{}\"></attachment>",
        xml_attr_escape(&attachment_id)
    );
    json!({
        "body": { "contentType": "html", "content": body_html },
        "attachments": [{
            "id": attachment_id,
            "contentType": "application/vnd.microsoft.card.adaptive",
            "contentUrl": null,
            "content": content_str,
            "name": null,
            "thumbnailUrl": null,
        }],
    })
}

/// Derive a stable, deterministic attachment id from a card's JSON. We
/// avoid pulling in a random UUID at call-time because (a) deterministic
/// ids make replay fixtures trivial to diff, and (b) Microsoft Graph
/// only requires uniqueness within the message — not globally.
fn adaptive_card_attachment_id(card_json: &Value) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    card_json.to_string().hash(&mut h);
    format!("card-{:016x}", h.finish())
}

impl TeamsApi {
    /// POST a Graph chat-message that carries an Adaptive Card
    /// attachment as its only body.
    ///
    /// `fallback_text` powers the notification-preview surface.
    pub async fn post_channel_adaptive_card(
        &self,
        team_id: &str,
        channel_id: &str,
        card_json: &Value,
        fallback_text: &str,
    ) -> Result<ChatMessageResponse, AdapterError> {
        let url = self.url(&format!("teams/{team_id}/channels/{channel_id}/messages"));
        let body = build_adaptive_message_body(card_json, fallback_text);
        let resp = self.send_json(Method::POST, &url, &body).await?;
        decode_message(resp).await
    }

    /// POST a Graph chat-message **as a reply** inside an existing
    /// channel thread, carrying an Adaptive Card attachment.
    pub async fn post_channel_adaptive_card_reply(
        &self,
        team_id: &str,
        channel_id: &str,
        parent_message_id: &str,
        card_json: &Value,
        fallback_text: &str,
    ) -> Result<ChatMessageResponse, AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/messages/{parent_message_id}/replies"
        ));
        let body = build_adaptive_message_body(card_json, fallback_text);
        let resp = self.send_json(Method::POST, &url, &body).await?;
        decode_message(resp).await
    }

    /// POST a Graph chat-message that carries an Adaptive Card
    /// attachment, addressed to a 1:1 / group chat.
    pub async fn post_chat_adaptive_card(
        &self,
        chat_id: &str,
        card_json: &Value,
        fallback_text: &str,
    ) -> Result<ChatMessageResponse, AdapterError> {
        let url = self.url(&format!("chats/{chat_id}/messages"));
        let body = build_adaptive_message_body(card_json, fallback_text);
        let resp = self.send_json(Method::POST, &url, &body).await?;
        decode_message(resp).await
    }

    /// PATCH a previously-posted channel adaptive-card message in
    /// place. The Graph PATCH on `/chatMessages/{id}` accepts a fresh
    /// `body` + `attachments[]` array — we send the new card with the
    /// same wire shape `post_channel_adaptive_card` produces.
    pub async fn edit_channel_adaptive_card(
        &self,
        team_id: &str,
        channel_id: &str,
        message_id: &str,
        card_json: &Value,
        fallback_text: &str,
    ) -> Result<(), AdapterError> {
        let url = self.url(&format!(
            "teams/{team_id}/channels/{channel_id}/messages/{message_id}"
        ));
        let body = build_adaptive_message_body(card_json, fallback_text);
        let resp = self.send_json(Method::PATCH, &url, &body).await?;
        let _ = consume_ok(resp).await?;
        Ok(())
    }

    /// PATCH a previously-posted 1:1/group chat adaptive-card message.
    pub async fn edit_chat_adaptive_card(
        &self,
        chat_id: &str,
        message_id: &str,
        card_json: &Value,
        fallback_text: &str,
    ) -> Result<(), AdapterError> {
        let url = self.url(&format!("chats/{chat_id}/messages/{message_id}"));
        let body = build_adaptive_message_body(card_json, fallback_text);
        let resp = self.send_json(Method::PATCH, &url, &body).await?;
        let _ = consume_ok(resp).await?;
        Ok(())
    }
}

/// Build the JSON request body for a `messages` create. When
/// `attachments` is non-empty, the corresponding inline references
/// (`<attachment id="…"></attachment>`) are appended to the HTML
/// content so Teams renders the file inline in the message card.
pub(crate) fn build_message_body(
    content: &str,
    content_type: &str,
    attachments: &[GraphAttachment],
) -> Value {
    if attachments.is_empty() {
        return json!({
            "body": { "contentType": content_type, "content": content }
        });
    }
    // Graph attachments require an `<attachment id="…">` placeholder
    // inside the message body (html). If the caller passed text we
    // upgrade to html so the placeholders render correctly.
    let mut body_html = if content_type == "html" {
        content.to_string()
    } else {
        // Escape for safety, then convert into html.
        html_escape(content)
    };
    for a in attachments {
        body_html.push_str(&format!(
            "<attachment id=\"{}\"></attachment>",
            xml_attr_escape(&a.id)
        ));
    }
    let attach_json: Vec<Value> = attachments
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "contentType": "reference",
                "contentUrl": a.content_url,
                "name": a.name,
            })
        })
        .collect();
    json!({
        "body": { "contentType": "html", "content": body_html },
        "attachments": attach_json,
    })
}

/// Percent-encode characters that aren't safe in a Graph path segment.
/// Keeps unreserved chars + `.` and `-`; everything else (including
/// spaces) becomes `%XX`. We don't pull in `urlencoding` for this — the
/// alphabet of Teams filenames is narrow enough that a hand-rolled
/// pass is shorter than a new dep.
pub(crate) fn urlencode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn xml_attr_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// Map an HTTP status (with optional body and retry-after) onto an
/// [`AdapterError`].
pub(crate) fn map_status_error(
    status: StatusCode,
    body: &str,
    retry_after: Option<u64>,
) -> AdapterError {
    let snippet = body.chars().take(256).collect::<String>();
    match status.as_u16() {
        401 | 403 => AdapterError::Auth(format!("{status}: {snippet}")),
        429 => AdapterError::Rate { retry_after },
        400 | 404 | 422 => AdapterError::BadRequest(format!("{status}: {snippet}")),
        _ => AdapterError::Transport(format!("{status}: {snippet}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_for(server: &MockServer) -> TeamsApi {
        TeamsApi::new(server.uri(), "tok-123")
    }

    #[test]
    fn url_builds_correctly_with_trailing_slash() {
        let api = TeamsApi::new("https://example.test/v1.0/", "tok");
        assert_eq!(api.url("teams/x"), "https://example.test/v1.0/teams/x");
        let api = TeamsApi::new("https://example.test/v1.0", "tok");
        assert_eq!(api.url("/teams/x"), "https://example.test/v1.0/teams/x");
    }

    #[test]
    fn map_status_auth_codes() {
        for code in [401u16, 403] {
            let s = StatusCode::from_u16(code).unwrap();
            match map_status_error(s, "denied", None) {
                AdapterError::Auth(_) => {}
                other => panic!("expected Auth for {code}, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_status_404_is_bad_request() {
        match map_status_error(StatusCode::NOT_FOUND, "missing", None) {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn map_status_429_is_rate_with_retry_after() {
        match map_status_error(StatusCode::TOO_MANY_REQUESTS, "slow down", Some(7)) {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(7)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn map_status_400_is_bad_request() {
        match map_status_error(StatusCode::BAD_REQUEST, "bad", None) {
            AdapterError::BadRequest(_) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn map_status_422_is_bad_request() {
        match map_status_error(StatusCode::UNPROCESSABLE_ENTITY, "bad", None) {
            AdapterError::BadRequest(_) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn map_status_5xx_is_transport() {
        match map_status_error(StatusCode::SERVICE_UNAVAILABLE, "down", None) {
            AdapterError::Transport(_) => {}
            other => panic!("got {other:?}"),
        }
        match map_status_error(StatusCode::INTERNAL_SERVER_ERROR, "boom", None) {
            AdapterError::Transport(_) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn map_status_other_is_transport() {
        // Status 418 (teapot) is not specially classified.
        match map_status_error(StatusCode::IM_A_TEAPOT, "no coffee", None) {
            AdapterError::Transport(_) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_channel_message_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .and(header("authorization", "Bearer tok-123"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"id": "1700000000000"})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let resp = api
            .post_channel_message("T1", "C1", "hi", "text")
            .await
            .unwrap();
        assert_eq!(resp.id, "1700000000000");
    }

    #[tokio::test]
    async fn post_channel_reply_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages/PARENT/replies"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "RID"})))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let resp = api
            .post_channel_reply("T1", "C1", "PARENT", "yo", "text")
            .await
            .unwrap();
        assert_eq!(resp.id, "RID");
    }

    #[tokio::test]
    async fn post_chat_message_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chats/CHAT1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "CMID"})))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let resp = api.post_chat_message("CHAT1", "yo", "text").await.unwrap();
        assert_eq!(resp.id, "CMID");
    }

    #[tokio::test]
    async fn edit_channel_message_ok() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/teams/T1/channels/C1/messages/MID"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.edit_channel_message("T1", "C1", "MID", "new").await.unwrap();
    }

    #[tokio::test]
    async fn edit_chat_message_ok() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/chats/CHAT1/messages/MID"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.edit_chat_message("CHAT1", "MID", "new").await.unwrap();
    }

    #[tokio::test]
    async fn set_channel_reaction_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages/MID/setReaction"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.set_channel_reaction("T1", "C1", "MID", "like")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn set_chat_reaction_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chats/CHAT1/messages/MID/setReaction"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.set_chat_reaction("CHAT1", "MID", "heart")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn get_channel_message_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/messages/MID"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"MID","body":{"content":"<p>hi</p>"}})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let v = api.get_channel_message("T1", "C1", "MID").await.unwrap();
        assert_eq!(v["body"]["content"], "<p>hi</p>");
    }

    #[tokio::test]
    async fn get_chat_message_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT1/messages/MID"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":"MID","body":{"content":"hi"}})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let v = api.get_chat_message("CHAT1", "MID").await.unwrap();
        assert_eq!(v["id"], "MID");
    }

    #[tokio::test]
    async fn get_chat_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/chats/CHAT1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"chatType": "oneOnOne"})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let info = api.get_chat("CHAT1").await.unwrap();
        assert_eq!(info.chat_type.as_deref(), Some("oneOnOne"));
    }

    #[tokio::test]
    async fn returns_auth_on_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string("Unauthorized"),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_auth_on_403() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_bad_request_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_bad_request_on_400() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_rate_on_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "13")
                    .set_body_string("too many"),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(13)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_rate_on_429_without_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_string("too many"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, None),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_transport_on_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_on_unreachable_host() {
        // 1 is a reserved port that will refuse connections, exercising the
        // reqwest::Error -> Transport mapping.
        let api = TeamsApi::new("http://127.0.0.1:1", "tok");
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_json_body_yields_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(
                ResponseTemplate::new(201)
                    .insert_header("content-type", "application/json")
                    .set_body_string("not-json"),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn message_id_missing_yields_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"foo":"bar"})))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_channel_message("T1", "C1", "x", "text").await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn api_clone_and_debug() {
        let api = TeamsApi::new("https://example.test/v1.0", "tok-1");
        let _ = api.clone();
        let s = format!("{api:?}");
        assert!(s.contains("tok-1"));
    }

    #[test]
    fn build_message_body_without_attachments_is_simple_body() {
        let v = build_message_body("hi", "text", &[]);
        assert_eq!(v["body"]["contentType"], "text");
        assert_eq!(v["body"]["content"], "hi");
        assert!(v.get("attachments").is_none());
    }

    #[test]
    fn build_message_body_with_attachments_promotes_text_to_html_and_inlines_refs() {
        let attachments = vec![GraphAttachment {
            id: "DI1".into(),
            content_url: "https://example/c.txt".into(),
            name: "c.txt".into(),
        }];
        let v = build_message_body("plain & body", "text", &attachments);
        assert_eq!(v["body"]["contentType"], "html");
        let html = v["body"]["content"].as_str().unwrap();
        assert!(html.contains("plain &amp; body"));
        assert!(html.contains("<attachment id=\"DI1\"></attachment>"));
        let arr = v["attachments"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "DI1");
        assert_eq!(arr[0]["contentType"], "reference");
        assert_eq!(arr[0]["contentUrl"], "https://example/c.txt");
        assert_eq!(arr[0]["name"], "c.txt");
    }

    #[test]
    fn urlencode_segment_keeps_safe_and_escapes_unsafe() {
        assert_eq!(urlencode_segment("a.txt"), "a.txt");
        assert_eq!(urlencode_segment("my file.pdf"), "my%20file.pdf");
        assert_eq!(urlencode_segment("hello/world"), "hello%2Fworld");
    }
}
