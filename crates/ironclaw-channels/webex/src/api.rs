//! Webex REST API client.
//!
//! Wraps the slice of Webex endpoints the adapter needs. Webex uses standard
//! HTTP status codes:
//!
//! - `200`/`204` — success.
//! - `401`/`403` — [`AdapterError::Auth`].
//! - `404` — [`AdapterError::BadRequest`] (we treat "not found" as
//!   client-side input that the host should not blindly retry).
//! - `429` — [`AdapterError::Rate`], honouring `Retry-After` when present.
//! - other `4xx` — [`AdapterError::BadRequest`].
//! - `5xx` and connection failure — [`AdapterError::Transport`].
//!
//! Error response bodies follow `{"message": "...", "errors": [...]}` and the
//! `message` text is surfaced in the error string.

use ironclaw_channels_core::{
    AdapterError, Breadcrumb, BreadcrumbStatus, Card, CardButton, DiffCard, ErrorCard,
    ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
};
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

/// Webex `GET /people/me` response (only the fields we care about).
#[derive(Debug, Clone, Deserialize)]
pub struct PersonMe {
    /// The bot's personId.
    pub id: String,
    /// Optional display name.
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
}

/// Webex `GET /messages/{id}` response (only the fields we care about).
#[derive(Debug, Clone, Deserialize)]
pub struct MessageView {
    /// Message id.
    pub id: String,
    /// Room id this message belongs to.
    #[serde(rename = "roomId")]
    pub room_id: String,
    /// Room type: `direct`, `group`, or `team`.
    #[serde(default, rename = "roomType")]
    pub room_type: Option<String>,
    /// Author personId.
    #[serde(default, rename = "personId")]
    pub person_id: Option<String>,
    /// Author email.
    #[serde(default, rename = "personEmail")]
    pub person_email: Option<String>,
    /// Plain text body.
    #[serde(default)]
    pub text: Option<String>,
    /// Markdown body (if the sender used markdown).
    #[serde(default)]
    pub markdown: Option<String>,
    /// Parent message id for threaded replies.
    #[serde(default, rename = "parentId")]
    pub parent_id: Option<String>,
    /// People mentioned by personId in this message (best-effort).
    #[serde(default, rename = "mentionedPeople")]
    pub mentioned_people: Vec<String>,
    /// ISO-8601 timestamp string.
    #[serde(default)]
    pub created: Option<String>,
}

/// Webex `GET /attachment/actions/{id}` response.
#[derive(Debug, Clone, Deserialize)]
pub struct AttachmentAction {
    /// Action id.
    pub id: String,
    /// `submit` or `share`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Parent message id this action was attached to.
    #[serde(rename = "messageId")]
    pub message_id: String,
    /// Person who triggered the action.
    #[serde(default, rename = "personId")]
    pub person_id: Option<String>,
    /// Room the action took place in.
    #[serde(default, rename = "roomId")]
    pub room_id: Option<String>,
    /// User-supplied inputs (free-form JSON).
    #[serde(default)]
    pub inputs: Option<Value>,
}

/// Successful response from `POST /messages` and `PUT /messages/{id}`.
#[derive(Debug, Clone, Deserialize)]
pub struct PostMessageResponse {
    /// Webex's message id.
    pub id: String,
    /// Room id the message was posted into.
    #[serde(default, rename = "roomId")]
    pub room_id: Option<String>,
    /// Parent message id when this is a threaded reply.
    #[serde(default, rename = "parentId")]
    pub parent_id: Option<String>,
}

/// Minimal Webex REST client used by the adapter.
#[derive(Debug, Clone)]
pub struct WebexApi {
    client: Client,
    api_base: String,
    bot_token: String,
}

impl WebexApi {
    /// Construct using a default [`reqwest::Client`].
    #[must_use]
    pub fn new(api_base: impl Into<String>, bot_token: impl Into<String>) -> Self {
        Self::with_client(Client::new(), api_base, bot_token)
    }

    /// Construct with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(
        client: Client,
        api_base: impl Into<String>,
        bot_token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            bot_token: bot_token.into(),
        }
    }

    /// Build a fully-qualified URL for the given relative path (e.g.
    /// `"messages"`).
    fn url(&self, rel: &str) -> String {
        format!("{}/{}", self.api_base, rel.trim_start_matches('/'))
    }

    /// Borrow the configured API base URL (for tests and tracing).
    #[must_use]
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// `GET /people/me`. Used at init to discover the bot's personId for
    /// mention detection and DM addressing.
    pub async fn me(&self) -> Result<PersonMe, AdapterError> {
        let resp = self
            .client
            .get(self.url("people/me"))
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_json(resp).await?;
        serde_json::from_value::<PersonMe>(value)
            .map_err(|e| AdapterError::Transport(format!("people/me decode: {e}")))
    }

    /// `GET /messages/{id}`. Webhooks omit body text, so we fetch it on demand.
    pub async fn get_message(&self, id: &str) -> Result<MessageView, AdapterError> {
        let resp = self
            .client
            .get(self.url(&format!("messages/{id}")))
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_json(resp).await?;
        serde_json::from_value::<MessageView>(value)
            .map_err(|e| AdapterError::Transport(format!("messages/{{id}} decode: {e}")))
    }

    /// `GET /attachment/actions/{id}` — fetch the inputs an adaptive-card
    /// submitter chose.
    pub async fn get_attachment_action(
        &self,
        id: &str,
    ) -> Result<AttachmentAction, AdapterError> {
        let resp = self
            .client
            .get(self.url(&format!("attachment/actions/{id}")))
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_json(resp).await?;
        serde_json::from_value::<AttachmentAction>(value)
            .map_err(|e| AdapterError::Transport(format!("attachment/actions decode: {e}")))
    }

    /// `POST /messages` — send a message to a room.
    ///
    /// `text` is preferred over `markdown` (caller supplies whichever is set).
    /// Callers must pass exactly one of `text` / `markdown` non-empty. If
    /// `card` is `Some`, an Adaptive Card attachment is added.
    pub async fn post_message(
        &self,
        room_id: &str,
        parent_id: Option<&str>,
        text: Option<&str>,
        markdown: Option<&str>,
        card: Option<&Value>,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut body = json!({"roomId": room_id});
        attach_body_fields(&mut body, parent_id, text, markdown, card);
        let resp = self
            .client
            .post(self.url("messages"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_json(resp).await?;
        serde_json::from_value::<PostMessageResponse>(value)
            .map_err(|e| AdapterError::Transport(format!("messages decode: {e}")))
    }

    /// `POST /messages` addressed to a person rather than a room.
    pub async fn post_dm_to_person(
        &self,
        person_id: &str,
        parent_id: Option<&str>,
        text: Option<&str>,
        markdown: Option<&str>,
        card: Option<&Value>,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut body = json!({"toPersonId": person_id});
        attach_body_fields(&mut body, parent_id, text, markdown, card);
        let resp = self
            .client
            .post(self.url("messages"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_json(resp).await?;
        serde_json::from_value::<PostMessageResponse>(value)
            .map_err(|e| AdapterError::Transport(format!("messages decode: {e}")))
    }

    /// `POST /messages` with a single multipart file attachment.
    pub async fn upload_file_message(
        &self,
        room_id: &str,
        parent_id: Option<&str>,
        text: Option<&str>,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut form = Form::new().text("roomId", room_id.to_owned());
        if let Some(parent) = parent_id {
            form = form.text("parentId", parent.to_owned());
        }
        if let Some(t) = text {
            form = form.text("text", t.to_owned());
        }
        let part = Part::bytes(bytes).file_name(filename.to_owned());
        form = form.part("files", part);

        let resp = self
            .client
            .post(self.url("messages"))
            .bearer_auth(&self.bot_token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_json(resp).await?;
        serde_json::from_value::<PostMessageResponse>(value)
            .map_err(|e| AdapterError::Transport(format!("messages decode: {e}")))
    }

    /// `PUT /messages/{id}` — edit a previously-posted message. Webex
    /// requires `roomId` in the body even on edit.
    pub async fn edit_message(
        &self,
        message_id: &str,
        room_id: &str,
        text: Option<&str>,
        markdown: Option<&str>,
    ) -> Result<PostMessageResponse, AdapterError> {
        let mut body = json!({"roomId": room_id});
        if let Some(t) = text {
            body["text"] = Value::String(t.to_owned());
        }
        if let Some(m) = markdown {
            body["markdown"] = Value::String(m.to_owned());
        }
        let resp = self
            .client
            .put(self.url(&format!("messages/{message_id}")))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let value = read_json(resp).await?;
        serde_json::from_value::<PostMessageResponse>(value)
            .map_err(|e| AdapterError::Transport(format!("messages PUT decode: {e}")))
    }

    /// `DELETE /messages/{id}`.
    pub async fn delete_message(&self, message_id: &str) -> Result<(), AdapterError> {
        let resp = self
            .client
            .delete(self.url(&format!("messages/{message_id}")))
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        read_empty(resp).await
    }

    /// `POST /reactions` — best-effort. Beta endpoint that some Webex
    /// deployments do not enable. `404`/`501` map to
    /// [`AdapterError::Unsupported`] so the host stops retrying.
    pub async fn post_reaction(
        &self,
        message_id: &str,
        reaction: &str,
    ) -> Result<(), AdapterError> {
        let body = json!({"messageId": message_id, "reaction": reaction});
        let resp = self
            .client
            .post(self.url("reactions"))
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| map_send_err(&e))?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND || status == StatusCode::NOT_IMPLEMENTED {
            return Err(AdapterError::Unsupported(format!(
                "webex reactions endpoint returned {status}"
            )));
        }
        read_empty(resp).await
    }
}

// ---------------------------------------------------------------------------
// Adaptive Cards (slice-4 native renderers).
//
// Webex uses the Microsoft Adaptive Cards schema for inline rich
// content — attached to `POST /messages` as
// `application/vnd.microsoft.card.adaptive`. The seven canonical
// surfaces (Card / Breadcrumb / Diff / Error / TodoList / Collapsible /
// Thinking) are mapped via the builders below.
//
// IMPORTANT: Webex's adaptive-card subset rejects a few elements the
// full Teams spec accepts. Notable gaps as of 2024:
//
//   - `CodeBlock` (Adaptive Cards 1.5) is NOT rendered — we fall back
//     to monospace `TextBlock` (`fontType: "Monospace"`).
//   - `Action.ToggleVisibility` is supported but the rendered chevron
//     differs from Teams's; we still emit it for Collapsible / Thinking.
//
// EDIT-IN-PLACE LIMITATION: Webex `PUT /messages/{id}` does NOT support
// editing the `attachments[]` field — only `text` / `markdown`. So
// Breadcrumb + TodoList update paths fall back to emitting a FRESH
// adaptive card; the host's delivery service will need to track the
// new id. This is a platform constraint documented in
// `WebexAdapter::deliver_breadcrumb` / `deliver_todo_list`.
// ---------------------------------------------------------------------------

/// Adaptive Cards schema URL.
pub(crate) const ADAPTIVE_CARD_SCHEMA: &str = "http://adaptivecards.io/schemas/adaptive-card.json";
/// Adaptive Cards schema version Webex renders. 1.2 is the safe
/// floor across the Webex client matrix; we use 1.2 features only
/// (`TextBlock`, `FactSet`, `Image`, `Container`, `Action.Submit`,
/// `Action.OpenUrl`, `Action.ToggleVisibility`).
pub(crate) const ADAPTIVE_CARD_VERSION: &str = "1.2";

/// Render a canonical [`Card`] into an Adaptive Card JSON Value.
///
/// Mapping (identical to the Teams renderer; Adaptive Cards is the
/// shared schema):
///
/// - `card.title` → bold `TextBlock` (size: Large).
/// - `card.body` → wrapping `TextBlock`.
/// - `card.image_url` → `Image` (size: Medium).
/// - `card.fields` → `FactSet`.
/// - `card.buttons` (`value`) → `Action.Submit` carrying `{value}`.
/// - `card.buttons` (`url`) → `Action.OpenUrl`.
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

/// Render a [`Breadcrumb`] chip as an Adaptive Card.
///
/// Webex doesn't support message edits with attachment changes; the
/// adapter's `deliver_breadcrumb` always POSTS a fresh card and the
/// host gets a new id for each update. Renderer is otherwise identical
/// to the Teams chip.
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

/// Render a [`DiffCard`] as an Adaptive Card. Webex's subset doesn't
/// render `CodeBlock` — we use monospace `TextBlock` per hunk.
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

/// Render an [`ErrorCard`] as an Adaptive Card with an
/// `attention`-styled top container + red header.
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

/// Render a [`TodoList`] as an Adaptive Card. Webex's no-message-edit
/// limitation means every list mutation emits a fresh card; the host
/// tracks the new id but the user sees a new chip per update.
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

/// Render a long-output expander as an Adaptive Card with
/// `Action.ToggleVisibility` flipping a hidden container.
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

/// Render a [`ThinkingBlock`] as an Adaptive Card with a collapsed
/// container + toggle action. Redacted blocks emit the placeholder
/// — the raw blob NEVER reaches the wire.
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

fn attach_body_fields(
    body: &mut Value,
    parent_id: Option<&str>,
    text: Option<&str>,
    markdown: Option<&str>,
    card: Option<&Value>,
) {
    if let Some(parent) = parent_id {
        body["parentId"] = Value::String(parent.to_owned());
    }
    if let Some(t) = text {
        body["text"] = Value::String(t.to_owned());
    }
    if let Some(m) = markdown {
        body["markdown"] = Value::String(m.to_owned());
    }
    if let Some(card_content) = card {
        body["attachments"] = json!([{
            "contentType": "application/vnd.microsoft.card.adaptive",
            "content": card_content,
        }]);
    }
}

fn map_send_err(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(format!("webex http error: {err}"))
}

async fn read_json(resp: Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after = parse_retry_after(&resp);
    if status.is_success() {
        return resp
            .json::<Value>()
            .await
            .map_err(|e| AdapterError::Transport(format!("webex response not JSON: {e}")));
    }
    let body = resp.text().await.unwrap_or_default();
    Err(map_status_error(status, &body, retry_after))
}

async fn read_empty(resp: Response) -> Result<(), AdapterError> {
    let status = resp.status();
    let retry_after = parse_retry_after(&resp);
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    Err(map_status_error(status, &body, retry_after))
}

fn parse_retry_after(resp: &Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

fn extract_error_message(body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.to_owned())
}

fn map_status_error(status: StatusCode, body: &str, retry_after: Option<u64>) -> AdapterError {
    let message = extract_error_message(body);
    let detail = if message.is_empty() {
        format!("webex returned {status}")
    } else {
        format!("webex returned {status}: {message}")
    };
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => AdapterError::Auth(detail),
        StatusCode::NOT_FOUND => AdapterError::BadRequest(detail),
        StatusCode::TOO_MANY_REQUESTS => AdapterError::Rate { retry_after },
        s if s.is_server_error() => AdapterError::Transport(detail),
        s if s.is_client_error() => AdapterError::BadRequest(detail),
        _ => AdapterError::Transport(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(server: &MockServer) -> WebexApi {
        WebexApi::new(server.uri(), "tok-test")
    }

    #[tokio::test]
    async fn url_trims_trailing_slash_and_leading_slash() {
        let api = WebexApi::new("https://x/api/", "tok");
        assert_eq!(api.url("messages"), "https://x/api/messages");
        let api = WebexApi::new("https://x/api", "tok");
        assert_eq!(api.url("/messages"), "https://x/api/messages");
    }

    #[tokio::test]
    async fn api_clone_and_debug() {
        let api = WebexApi::new("https://x", "tok-xyz");
        let _ = api.clone();
        assert!(format!("{api:?}").contains("tok-xyz"));
    }

    #[tokio::test]
    async fn api_base_accessor() {
        let api = WebexApi::new("https://x/api/", "tok");
        assert_eq!(api.api_base(), "https://x/api");
    }

    #[tokio::test]
    async fn me_returns_person() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me"))
            .and(header("authorization", "Bearer tok-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "PME", "displayName": "Bot"
            })))
            .mount(&server)
            .await;
        let r = api(&server).me().await.unwrap();
        assert_eq!(r.id, "PME");
        assert_eq!(r.display_name.as_deref(), Some("Bot"));
    }

    #[tokio::test]
    async fn me_401_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "message":"invalid token"
            })))
            .mount(&server)
            .await;
        match api(&server).me().await.unwrap_err() {
            AdapterError::Auth(m) => assert!(m.contains("invalid token")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn me_403_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/people/me"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message":"forbidden"
            })))
            .mount(&server)
            .await;
        match api(&server).me().await.unwrap_err() {
            AdapterError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_message_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"abc",
                "roomId":"R1",
                "roomType":"group",
                "personId":"P1",
                "personEmail":"p@example.com",
                "text":"hello",
                "parentId":"PAR1",
                "mentionedPeople":["PBOT"],
                "created":"2024-01-01T00:00:00.000Z"
            })))
            .mount(&server)
            .await;
        let m = api(&server).get_message("abc").await.unwrap();
        assert_eq!(m.id, "abc");
        assert_eq!(m.room_id, "R1");
        assert_eq!(m.room_type.as_deref(), Some("group"));
        assert_eq!(m.text.as_deref(), Some("hello"));
        assert_eq!(m.parent_id.as_deref(), Some("PAR1"));
        assert_eq!(m.mentioned_people, vec!["PBOT".to_string()]);
        assert_eq!(m.person_email.as_deref(), Some("p@example.com"));
    }

    #[tokio::test]
    async fn get_message_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/x"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message":"Message not found"
            })))
            .mount(&server)
            .await;
        match api(&server).get_message("x").await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("Message not found")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_message_decode_error_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/messages/x"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        match api(&server).get_message("x").await.unwrap_err() {
            AdapterError::Transport(_) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_attachment_action_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/attachment/actions/A1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"A1",
                "type":"submit",
                "messageId":"M1",
                "personId":"P1",
                "roomId":"R1",
                "inputs": {"name":"x"}
            })))
            .mount(&server)
            .await;
        let a = api(&server).get_attachment_action("A1").await.unwrap();
        assert_eq!(a.id, "A1");
        assert_eq!(a.kind, "submit");
        assert_eq!(a.message_id, "M1");
        assert_eq!(a.inputs.unwrap()["name"], "x");
    }

    #[tokio::test]
    async fn post_message_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap();
        assert_eq!(r.id, "M1");
        assert_eq!(r.room_id.as_deref(), Some("R1"));
    }

    #[tokio::test]
    async fn post_message_markdown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .post_message("R1", None, None, Some("**hi**"), None)
            .await
            .unwrap();
        assert_eq!(r.id, "M1");
    }

    #[tokio::test]
    async fn post_message_with_parent_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M2","roomId":"R1","parentId":"P1"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .post_message("R1", Some("P1"), Some("re"), None, None)
            .await
            .unwrap();
        assert_eq!(r.parent_id.as_deref(), Some("P1"));
    }

    #[tokio::test]
    async fn post_message_with_card_attachment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M3","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let card = json!({"$schema":"x"});
        let r = api(&server)
            .post_message("R1", None, Some("see card"), None, Some(&card))
            .await
            .unwrap();
        assert_eq!(r.id, "M3");
    }

    #[tokio::test]
    async fn post_dm_to_person_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M9"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .post_dm_to_person("PERSON", None, Some("hi"), None, None)
            .await
            .unwrap();
        assert_eq!(r.id, "M9");
    }

    #[tokio::test]
    async fn post_message_400_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "message":"missing roomId"
            })))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::BadRequest(m) => assert!(m.contains("missing roomId")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_401_is_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "message":"bad token"
            })))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_403_is_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message":"forbidden"
            })))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "13")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(13)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_429_without_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(429).set_body_string(""))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::Rate { retry_after } => assert!(retry_after.is_none()),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_500_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message":"boom"
            })))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::Transport(m) => assert!(m.contains("boom")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_redirect_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(304).set_body_string(""))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::Transport(_) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_other_4xx_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(418).set_body_string("teapot"))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::BadRequest(m) => assert!(m.contains("teapot")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_network_failure_is_transport() {
        let api = WebexApi::new("http://127.0.0.1:1", "tok");
        match api
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::Transport(_) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn edit_message_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .edit_message("M1", "R1", Some("new"), None)
            .await
            .unwrap();
        assert_eq!(r.id, "M1");
    }

    #[tokio::test]
    async fn edit_message_markdown() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M1","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .edit_message("M1", "R1", None, Some("**new**"))
            .await
            .unwrap();
        assert_eq!(r.id, "M1");
    }

    #[tokio::test]
    async fn edit_message_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message":"not found"
            })))
            .mount(&server)
            .await;
        match api(&server)
            .edit_message("M1", "R1", Some("new"), None)
            .await
            .unwrap_err()
        {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_message_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(204).set_body_string(""))
            .mount(&server)
            .await;
        api(&server).delete_message("M1").await.unwrap();
    }

    #[tokio::test]
    async fn delete_message_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "message":"not found"
            })))
            .mount(&server)
            .await;
        match api(&server).delete_message("M1").await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_message_401_is_auth() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(401).set_body_string(""))
            .mount(&server)
            .await;
        match api(&server).delete_message("M1").await.unwrap_err() {
            AdapterError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_file_message_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M-FILE","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .upload_file_message("R1", None, Some("see file"), "x.txt", b"data".to_vec())
            .await
            .unwrap();
        assert_eq!(r.id, "M-FILE");
    }

    #[tokio::test]
    async fn upload_file_message_with_parent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"M-FILE","roomId":"R1"
            })))
            .mount(&server)
            .await;
        let r = api(&server)
            .upload_file_message("R1", Some("PAR1"), None, "x.txt", b"d".to_vec())
            .await
            .unwrap();
        assert_eq!(r.id, "M-FILE");
    }

    #[tokio::test]
    async fn upload_file_message_400_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "message":"bad form"
            })))
            .mount(&server)
            .await;
        match api(&server)
            .upload_file_message("R1", None, None, "x.txt", b"d".to_vec())
            .await
            .unwrap_err()
        {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_reaction_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions"))
            .respond_with(ResponseTemplate::new(204).set_body_string(""))
            .mount(&server)
            .await;
        api(&server).post_reaction("M1", "thumbsup").await.unwrap();
    }

    #[tokio::test]
    async fn post_reaction_404_is_unsupported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions"))
            .respond_with(ResponseTemplate::new(404).set_body_string(""))
            .mount(&server)
            .await;
        match api(&server).post_reaction("M1", "thumbsup").await.unwrap_err() {
            AdapterError::Unsupported(m) => assert!(m.contains("404")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_reaction_501_is_unsupported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions"))
            .respond_with(ResponseTemplate::new(501).set_body_string(""))
            .mount(&server)
            .await;
        match api(&server).post_reaction("M1", "thumbsup").await.unwrap_err() {
            AdapterError::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_reaction_401_is_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "message":"bad token"
            })))
            .mount(&server)
            .await;
        match api(&server).post_reaction("M1", "thumbsup").await.unwrap_err() {
            AdapterError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_reaction_500_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("err"))
            .mount(&server)
            .await;
        match api(&server).post_reaction("M1", "thumbsup").await.unwrap_err() {
            AdapterError::Transport(_) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn extract_error_message_falls_back_to_body_on_unstructured() {
        // Plain-text response body becomes the detail string directly.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string("totally not json"))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::BadRequest(m) => assert!(m.contains("totally not json")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn extract_error_message_handles_empty_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_string(""))
            .mount(&server)
            .await;
        match api(&server)
            .post_message("R1", None, Some("hi"), None, None)
            .await
            .unwrap_err()
        {
            AdapterError::BadRequest(m) => assert!(m.contains("400")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn map_status_error_table_for_each_variant() {
        let err = map_status_error(StatusCode::UNAUTHORIZED, "{\"message\":\"x\"}", None);
        assert!(matches!(err, AdapterError::Auth(_)));
        let err = map_status_error(StatusCode::FORBIDDEN, "{\"message\":\"x\"}", None);
        assert!(matches!(err, AdapterError::Auth(_)));
        let err = map_status_error(StatusCode::NOT_FOUND, "{\"message\":\"x\"}", None);
        assert!(matches!(err, AdapterError::BadRequest(_)));
        let err = map_status_error(StatusCode::TOO_MANY_REQUESTS, "", Some(2));
        assert!(matches!(err, AdapterError::Rate { retry_after: Some(2) }));
        let err = map_status_error(StatusCode::BAD_REQUEST, "{\"message\":\"x\"}", None);
        assert!(matches!(err, AdapterError::BadRequest(_)));
        let err = map_status_error(StatusCode::INTERNAL_SERVER_ERROR, "", None);
        assert!(matches!(err, AdapterError::Transport(_)));
        let err = map_status_error(StatusCode::MOVED_PERMANENTLY, "", None);
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn extract_error_message_prefers_json_message_field() {
        let m = extract_error_message("{\"message\":\"x\"}");
        assert_eq!(m, "x");
    }

    #[test]
    fn extract_error_message_falls_back_to_body_when_no_message_key() {
        let m = extract_error_message("{\"err\":\"x\"}");
        assert_eq!(m, "{\"err\":\"x\"}");
    }

    #[test]
    fn person_me_default_display_name_is_none() {
        let p: PersonMe = serde_json::from_value(json!({"id":"X"})).unwrap();
        assert!(p.display_name.is_none());
    }

    #[test]
    fn message_view_defaults_when_optional_fields_omitted() {
        let m: MessageView = serde_json::from_value(json!({
            "id":"abc","roomId":"R1"
        }))
        .unwrap();
        assert!(m.text.is_none());
        assert!(m.markdown.is_none());
        assert!(m.parent_id.is_none());
        assert!(m.room_type.is_none());
        assert!(m.person_id.is_none());
        assert!(m.person_email.is_none());
        assert!(m.mentioned_people.is_empty());
        assert!(m.created.is_none());
    }

    #[test]
    fn attachment_action_minimal() {
        let a: AttachmentAction = serde_json::from_value(json!({
            "id":"A1","type":"submit","messageId":"M1"
        }))
        .unwrap();
        assert_eq!(a.id, "A1");
        assert_eq!(a.kind, "submit");
        assert_eq!(a.message_id, "M1");
        assert!(a.inputs.is_none());
    }
}
