//! Google Chat [`ChannelAdapter`] implementation.

use crate::api::GchatApi;
use crate::emoji::emoji_codepoint;
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, BreadcrumbStatus, ChannelAdapter, DiffCard, DmHandle, ErrorCard,
    ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
};
use copperclaw_types::{ChannelType, OutboundMessage};
use serde_json::{json, Value};
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Google Chat channel adapter. See module-level docs.
pub struct GchatAdapter {
    channel_type: ChannelType,
    api: GchatApi,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for GchatAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GchatAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl GchatAdapter {
    /// Construct with an already-built API client. Used by the factory and
    /// by tests that drive the adapter directly.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: GchatApi) -> Self {
        Self {
            channel_type,
            api,
            server_handle: Mutex::new(None),
        }
    }

    /// Attach the join handle of the spawned axum server so the adapter
    /// can abort it on shutdown.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("gchat adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background events server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("gchat adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &GchatApi {
        &self.api
    }

    /// Pull the bare `<space>` segment out of a `spaces/<space>` platform id.
    ///
    /// Google Chat's `platform_id` from inbound events is the full `spaces/X`
    /// path; the create-message URL needs only the `X` part because the
    /// path itself is `/v1/spaces/X/messages`.
    fn space_segment(platform_id: &str) -> Result<&str, AdapterError> {
        platform_id.strip_prefix("spaces/").ok_or_else(|| {
            AdapterError::BadRequest(format!(
                "gchat platform_id must start with `spaces/`, got `{platform_id}`"
            ))
        })
    }
}

#[async_trait]
impl ChannelAdapter for GchatAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    /// Google Chat `spaces.messages.create` text field caps at 4 096 chars.
    fn max_message_chars(&self) -> Option<usize> {
        Some(4096)
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        // System actions (edit / reaction) are dispatched from the
        // `content` payload regardless of message kind, mirroring the
        // contract in `docs/adding-a-channel.md` § 4. Reject files on
        // those actions (they have no attachment semantics).
        if let Some(action) = message.content.get("action").and_then(Value::as_str) {
            if !message.files.is_empty() {
                return Err(AdapterError::BadRequest(format!(
                    "gchat action `{action}` does not accept file attachments"
                )));
            }
            return self.dispatch_action(action, &message.content).await;
        }

        let space = Self::space_segment(platform_id)?;
        let card = message.content.get("card");
        let card_id = message
            .content
            .get("card_id")
            .and_then(Value::as_str)
            .unwrap_or("default");

        if let Some(card) = card {
            // Card messages don't support attachments either.
            if !message.files.is_empty() {
                return Err(AdapterError::BadRequest(
                    "gchat cards do not accept file attachments".into(),
                ));
            }
            let resp = self.api.send_card(space, card_id, card).await?;
            return Ok(Some(resp.name));
        }

        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        // Two-step attachment flow: upload each file via
        // attachments:upload (returns an opaque resourceName), then
        // include those names in the `attachment[]` array on the
        // outgoing message. Google Chat's `messages.create` with
        // attachments doesn't accept `messageReplyOption`, so a
        // threaded reply with attachments will be posted top-level
        // with a warning — symmetric to the Teams behaviour and the
        // current Graph reply limitation.
        if !message.files.is_empty() {
            if thread_id.is_some() {
                tracing::warn!(
                    "gchat: outbound has both thread_id and files; \
                     attachments posted as top-level space message \
                     (Chat attachments + REPLY_MESSAGE_OR_FAIL not supported)"
                );
            }
            let mut refs = Vec::with_capacity(message.files.len());
            for f in &message.files {
                refs.push(
                    self.api
                        .upload_attachment(space, &f.filename, f.data.clone())
                        .await?,
                );
            }
            let resp = self
                .api
                .send_text_with_attachments(space, &text, &refs)
                .await?;
            return Ok(Some(resp.name));
        }

        let resp = if let Some(thread) = thread_id {
            self.api.send_threaded_text(space, thread, &text).await?
        } else {
            self.api.send_text(space, &text).await?
        };
        Ok(Some(resp.name))
    }

    /// Native breadcrumb chip — rendered as a Google Chat cards v2
    /// single-section card. We use `decoratedText` with a small icon
    /// and a `<font color="..."><b>shell</b></font> · cargo check`
    /// body so the chat client renders a compact one-line card
    /// (Google Chat's closest affordance to a "chip"). When
    /// `existing_message_id` is provided we PATCH the original card
    /// via `spaces.messages.patch` with `updateMask=cardsV2`;
    /// otherwise we POST a fresh card via `spaces.messages.create`.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let card = build_breadcrumb_card(breadcrumb);
        if let Some(message_name) = existing_message_id {
            let resp = self
                .api
                .edit_card(message_name, "breadcrumb", &card)
                .await?;
            return Ok(Some(resp.name));
        }
        let space = Self::space_segment(platform_id)?;
        let resp = self.api.send_card(space, "breadcrumb", &card).await?;
        Ok(Some(resp.name))
    }

    /// Native diff card — rendered as a Google Chat cards v2 card.
    /// The header carries `*<path>* (+N / -M)`; the body has one
    /// `decoratedText` widget per hunk with the hunk header as the
    /// `topLabel` and the body lines wrapped in `<font
    /// face="monospace">` so leading `+`/`-` characters stay aligned.
    ///
    /// Google Chat's cards v2 doesn't offer per-line gutter colour
    /// (no `+`/`-` highlight primitive), so the gutter colour is
    /// purely the leading char. Mobile renders the same monospace
    /// block as desktop — degraded but readable.
    async fn deliver_diff(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        let card = build_diff_card(diff);
        let space = Self::space_segment(platform_id)?;
        let resp = self.api.send_card(space, "diff", &card).await?;
        Ok(Some(resp.name))
    }

    /// Native long-output expander (slice 3.4) — rendered as a
    /// Google Chat cards v2 card with a single
    /// `collapsibleSection`. Google Chat's `collapsibleSection`
    /// primitive natively renders a disclosure widget with a
    /// header (the summary) and a body block that collapses behind
    /// a "Show more" / "Show less" toggle. This is the cleanest
    /// match for the slice-3.4 surface on any platform.
    ///
    /// Layout:
    /// - `sections[0]` is the collapsible section.
    /// - `sections[0].header` is the summary string.
    /// - `sections[0].collapsible = true`,
    ///   `uncollapsibleWidgetsCount = N` with N = preview-line count
    ///   so the preview always shows even when collapsed.
    /// - The first N widgets are `textParagraph`s for each preview
    ///   line (rendered above the disclosure fold).
    /// - The final widget is a `textParagraph` with the full body
    ///   inside `<font face="monospace">` so log output stays
    ///   aligned.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        let card = build_collapsible_card(text, summary, preview_lines);
        let space = Self::space_segment(platform_id)?;
        let resp = self.api.send_card(space, "long_output", &card).await?;
        Ok(Some(resp.name))
    }

    /// Native todo-list checklist — rendered as a Google Chat cards
    /// v2 single-section card whose header carries the title +
    /// `done/total` counter and whose section contains one
    /// `decoratedText` widget per item. Each item widget uses a
    /// material icon as `startIcon` to indicate status (`CHECK_CIRCLE`
    /// for completed, `CIRCLE` for in-progress, `STAR` for pending —
    /// the icon set is what's reliably available and visually
    /// distinct on Google Chat's web + mobile clients).
    ///
    /// First emit: POST via `spaces.messages.create`. Mutations:
    /// PATCH via `spaces.messages.patch` with `updateMask=cardsV2`
    /// so the user sees the same card tick through.
    ///
    /// Google Chat has NO public bot API for pinning messages — the
    /// `pin_hint` is silently accepted and ignored. The chip itself
    /// is the load-bearing UX; we accept the platform limitation.
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        list: &TodoList,
        existing_message_id: Option<&str>,
        _pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        let card = build_todo_list_card(list);
        if let Some(message_name) = existing_message_id {
            let resp = self.api.edit_card(message_name, "todo_list", &card).await?;
            return Ok(Some(resp.name));
        }
        let space = Self::space_segment(platform_id)?;
        let resp = self.api.send_card(space, "todo_list", &card).await?;
        Ok(Some(resp.name))
    }

    /// Native error card — rendered as a Google Chat cards v2 single
    /// section. Google Chat has no native colour-theming primitive on
    /// `cardsV2`, so we lean on:
    ///
    /// 1. A `decoratedText` widget at the top with a `startIcon` set
    ///    to a high-contrast Material known-icon (`BOOKMARK` —
    ///    closest visual analog to a "warning marker" Google Chat
    ///    universally renders).
    /// 2. The card's `header.title` carries the canonical
    ///    `ErrorCard::title`; `header.subtitle` carries the user-
    ///    facing summary. Bold + the "Error" prefix in the title
    ///    carries the severity signal.
    /// 3. Optional `textParagraph` widget with the details block in
    ///    monospace formatting (Google Chat renders `<font
    ///    face="monospace">` natively).
    /// 4. Retryable footer rides as a final `textParagraph` widget
    ///    italicised.
    async fn deliver_error(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        let card = build_error_card(err);
        let space = Self::space_segment(platform_id)?;
        let resp = self.api.send_card(space, "error", &card).await?;
        Ok(Some(resp.name))
    }

    /// Native thinking block (slice 3.5) — rendered as a Google Chat
    /// Cards v2 card with a `collapsibleSection` (the platform's
    /// native disclosure-widget primitive — same one surface 4 uses
    /// for long-output expanders). The collapsed header reads
    /// `reasoning (model)`; tap to expand reveals the reasoning text
    /// in a `textParagraph` widget. Redacted blocks render the
    /// placeholder body — the raw blob never reaches the wire.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        let card = build_thinking_card(thinking);
        let space = Self::space_segment(platform_id)?;
        let resp = self.api.send_card(space, "thinking", &card).await?;
        Ok(Some(resp.name))
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Google Chat DMs have to be opened via a separate `spaces.create`
        // flow we don't model in v1; deliveries address the existing DM
        // space directly via its `spaces/X` id.
        Ok(None)
    }
}

/// Build a Google Chat cards v2 card representing a [`DiffCard`].
///
/// Shape:
///
/// ```json
/// {
///   "header": {
///     "title": "<path>",
///     "subtitle": "+N / -M",
///     "imageType": "CIRCLE"
///   },
///   "sections": [{
///     "widgets": [
///       {"decoratedText": {
///         "topLabel": "@@ -old,len +new,len @@",
///         "text": "<font face=\"monospace\">…hunk body…</font>"
///       }},
///       …one widget per hunk…
///     ]
///   }]
/// }
/// ```
///
/// Google Chat's cards v2 doesn't expose per-line gutter colour;
/// the leading `+`/`-` characters in monospace carry the diff
/// semantics. Truncated cards get a footer note.
pub(crate) fn build_diff_card(diff: &DiffCard) -> Value {
    let subtitle = if diff.truncated {
        format!("+{} / -{} (truncated)", diff.added, diff.removed)
    } else {
        format!("+{} / -{}", diff.added, diff.removed)
    };
    let mut widgets: Vec<Value> = Vec::with_capacity(diff.hunks.len());
    for h in &diff.hunks {
        let header = format!(
            "@@ -{},{} +{},{} @@",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        );
        let mut body = String::with_capacity(h.lines.len() * 64);
        body.push_str("<font face=\"monospace\">");
        for line in &h.lines {
            body.push(line.kind.unified_prefix());
            // Lightweight HTML-escape — Google Chat renders cardsV2
            // text widgets with a small HTML subset, so `<` / `>` /
            // `&` from source code must be neutralised or they'd be
            // interpreted as tags.
            for c in line.text.chars() {
                match c {
                    '<' => body.push_str("&lt;"),
                    '>' => body.push_str("&gt;"),
                    '&' => body.push_str("&amp;"),
                    _ => body.push(c),
                }
            }
            body.push_str("<br>");
        }
        body.push_str("</font>");
        widgets.push(json!({
            "decoratedText": {
                "topLabel": header,
                "text": body,
                "wrapText": true,
            }
        }));
    }
    json!({
        "header": {
            "title": diff.path.clone(),
            "subtitle": subtitle,
            "imageType": "CIRCLE",
        },
        "sections": [{ "widgets": widgets }]
    })
}

/// Build a Google Chat cards v2 single-section card representing the
/// breadcrumb. The shape mirrors what Chat clients render as a compact
/// inline card (one section, one decoratedText widget) so the chip
/// stays one line on desktop and two lines on mobile.
pub(crate) fn build_breadcrumb_card(b: &Breadcrumb) -> Value {
    // ASCII-only status marker per the project's no-emoji rule.
    // Earlier versions used Cards v2 `knownIcon` (CLOCK / STAR /
    // BOOKMARK) but those render as colourful inline pictograms —
    // visually indistinguishable from emoji to the user — so they're
    // dropped in favour of a plain-text prefix on `topLabel`.
    let marker = match b.status {
        BreadcrumbStatus::Running => "[~]",
        BreadcrumbStatus::Done => "[ok]",
        BreadcrumbStatus::Failed => "[x]",
    };
    let top_label = format!("{marker} {}", b.tool_name);
    let mut body = String::new();
    if let Some(d) = b.detail.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            body.push_str(d);
        }
    }
    if let Some(s) = b.summary.as_deref() {
        let s = s.trim();
        if !s.is_empty() {
            if !body.is_empty() {
                body.push_str(" — ");
            }
            if b.status == BreadcrumbStatus::Failed {
                body.push_str("failed: ");
            }
            body.push_str(s);
        }
    }
    if body.is_empty() {
        b.status.as_str().clone_into(&mut body);
    }
    json!({
        "sections": [{
            "widgets": [{
                "decoratedText": {
                    "topLabel": top_label,
                    "text": body,
                }
            }]
        }]
    })
}

/// Build a Google Chat cards v2 card representing an [`ErrorCard`].
///
/// Shape:
///
/// ```json
/// {
///   "header": {
///     "title": "Error: <ErrorCard::title>",
///     "subtitle": "<ErrorCard::summary>",
///     "imageType": "CIRCLE"
///   },
///   "sections": [{
///     "widgets": [
///       {"decoratedText": {"startIcon": {"knownIcon": "BOOKMARK"},
///                          "text": "<bold severity label>"}},
///       {"textParagraph": {"text": "<details monospace>"}},      // optional
///       {"textParagraph": {"text": "<i>will retry…</i>"}}        // optional
///     ]
///   }]
/// }
/// ```
///
/// Google Chat has no native colour-theming on `cardsV2`, so the
/// severity carry rides on:
/// - The `Error:` prefix in `header.title`.
/// - The high-contrast `BOOKMARK` icon (universally available).
/// - Bold severity label inside the decoratedText widget.
pub(crate) fn build_error_card(err: &ErrorCard) -> Value {
    let label = match err.kind {
        ErrorCardKind::Internal => "Tool error",
        ErrorCardKind::Provider => "Provider error",
        ErrorCardKind::Delivery => "Delivery error",
    };
    let mut widgets: Vec<Value> = Vec::with_capacity(3);
    // No `startIcon` — Cards v2 `knownIcon` renders as a colourful
    // pictogram which violates the project's no-emoji rule. The text
    // already carries an "[!]" prefix + bold label.
    widgets.push(json!({
        "decoratedText": {
            "text": format!("[!] <b>{}</b>", escape_html_gchat(label)),
        }
    }));
    if let Some(d) = err.details.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            // Google Chat renders `<font face="monospace">` natively
            // inside text paragraphs; the `<br>` keeps line breaks.
            let mono = escape_html_gchat(d).replace('\n', "<br>");
            widgets.push(json!({
                "textParagraph": {
                    "text": format!("<font face=\"monospace\">{mono}</font>"),
                }
            }));
        }
    }
    if err.retryable {
        widgets.push(json!({
            "textParagraph": {
                "text": "<i>will retry automatically</i>",
            }
        }));
    }
    json!({
        "header": {
            "title": format!("Error: {}", err.title.trim()),
            "subtitle": err.summary.trim(),
            "imageType": "CIRCLE",
        },
        "sections": [{ "widgets": widgets }]
    })
}

/// Build a Google Chat cards v2 card for the slice-3.5 thinking-block
/// surface. Uses the native `collapsibleSection` primitive — same
/// disclosure widget surface 4 uses for long-output expanders — so the
/// reasoning collapses by default with a "Show more" affordance.
///
/// Shape:
///
/// ```json
/// {
///   "sections": [{
///     "header": "reasoning (claude-opus-4-7)",
///     "collapsible": true,
///     "uncollapsibleWidgetsCount": 0,
///     "widgets": [
///       { "textParagraph": { "text": "<i>thinking text…</i>" } }
///     ]
///   }]
/// }
/// ```
///
/// `uncollapsibleWidgetsCount: 0` keeps the whole body behind the
/// disclosure widget so the chat stays uncluttered. The body wraps
/// in `<i>` italic so it visually reads as muted metadata distinct
/// from the agent's chat reply.
///
/// Redacted blocks emit a single placeholder paragraph — the raw
/// blob never reaches the wire.
pub(crate) fn build_thinking_card(t: &ThinkingBlock) -> Value {
    let header = match t.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => format!("reasoning ({})", escape_html_gchat(m)),
        _ => "reasoning".to_string(),
    };
    let body_html = if t.redacted {
        "<i>(redacted reasoning)</i>".to_string()
    } else {
        let escaped = escape_html_gchat(&t.text).replace('\n', "<br>");
        format!("<i>{escaped}</i>")
    };
    json!({
        "sections": [{
            "header": header,
            "collapsible": true,
            "uncollapsibleWidgetsCount": 0,
            "widgets": [{
                "textParagraph": { "text": body_html },
            }],
        }]
    })
}

/// Build a Google Chat cards v2 card for the slice-3.4 long-output
/// expander surface. Uses the native `collapsibleSection` widget so
/// the user sees the summary + preview by default and clicks "Show
/// more" to expand the full body.
///
/// Shape:
///
/// ```json
/// {
///   "sections": [{
///     "header": "<summary>",
///     "collapsible": true,
///     "uncollapsibleWidgetsCount": <preview count>,
///     "widgets": [
///       { "textParagraph": { "text": "<preview line 1>" } },
///       …,
///       { "textParagraph": { "text": "<font face=\"monospace\">full body…</font>" } }
///     ]
///   }]
/// }
/// ```
///
/// `uncollapsibleWidgetsCount` set to the preview-line count keeps
/// the preview always visible (above the disclosure fold) while
/// the full body collapses underneath. The full body wraps in
/// `<font face="monospace">` so log / source output stays aligned.
pub(crate) fn build_collapsible_card(
    text: &str,
    summary: &str,
    preview_lines: &[String],
) -> Value {
    let mut widgets: Vec<Value> = Vec::with_capacity(preview_lines.len() + 1);
    for line in preview_lines {
        widgets.push(json!({
            "textParagraph": {
                "text": escape_html_gchat(line),
            }
        }));
    }
    let body_mono = escape_html_gchat(text).replace('\n', "<br>");
    widgets.push(json!({
        "textParagraph": {
            "text": format!("<font face=\"monospace\">{body_mono}</font>"),
        }
    }));
    json!({
        "sections": [{
            "header": summary.trim(),
            "collapsible": true,
            "uncollapsibleWidgetsCount": preview_lines.len(),
            "widgets": widgets,
        }]
    })
}

/// Build a Google Chat cards v2 card carrying the canonical
/// [`TodoList`] payload. Shape: header with title +
/// `(done/total)`, one section with one `decoratedText` widget per
/// item. Each item widget uses a known material icon as `startIcon`:
///
/// - `CHECK_CIRCLE` (completed)
/// - `CIRCLE` (in-progress)
/// - `STAR` (pending; closest to "outstanding marker" in the known-
///   icon set)
///
/// Item text goes through [`escape_html_gchat`]; completed items
/// wrap the text in `<font color="#808080">...<s>...</s></font>` so
/// the user sees finished work in muted strikethrough.
pub(crate) fn build_todo_list_card(list: &TodoList) -> Value {
    let done = list.completed_count();
    let total = list.items.len();
    let widgets: Vec<Value> = list
        .items
        .iter()
        .map(|item| {
            // ASCII-only status marker per the project's no-emoji rule.
            // `knownIcon` renders as a colourful Material pictogram —
            // visually indistinguishable from emoji to the user.
            let marker = match item.status {
                TodoItemStatus::Completed => "[x]",
                TodoItemStatus::InProgress => "[~]",
                TodoItemStatus::Pending => "[ ]",
            };
            let escaped = escape_html_gchat(item.text.trim());
            let body = if item.status == TodoItemStatus::Completed {
                // <s> = strikethrough; muted grey reinforces "done".
                format!("{marker} <font color=\"#808080\"><s>{escaped}</s></font>")
            } else {
                format!("{marker} {escaped}")
            };
            json!({
                "decoratedText": {
                    "text": body,
                }
            })
        })
        .collect();
    json!({
        "header": {
            "title": format!("{} ({done}/{total})", list.title_or_default()),
            "imageType": "CIRCLE",
        },
        "sections": [{ "widgets": widgets }]
    })
}

/// Google Chat's text-paragraph HTML subset uses the same five XML
/// escapes as Telegram; mirrors that pattern so user-supplied stderr /
/// tracebacks can't break the parser.
fn escape_html_gchat(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

impl GchatAdapter {
    /// Handle a `system`-action outbound message (edit, delete, reaction).
    async fn dispatch_action(
        &self,
        action: &str,
        content: &Value,
    ) -> Result<Option<String>, AdapterError> {
        match action {
            "edit" => {
                let message_name = content
                    .get("message_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "edit action requires `message_name` (spaces/.../messages/...)".into(),
                        )
                    })?;
                let text = content
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("edit action requires `text` field".into())
                    })?;
                let resp = self.api.edit_text(message_name, text).await?;
                Ok(Some(resp.name))
            }
            "delete" => {
                let message_name = content
                    .get("message_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "delete action requires `message_name`".into(),
                        )
                    })?;
                self.api.delete_message(message_name).await?;
                Ok(Some(message_name.to_owned()))
            }
            "reaction" => {
                let message_name = content
                    .get("message_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "reaction action requires `message_name`".into(),
                        )
                    })?;
                let shortcode = content
                    .get("emoji")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "reaction action requires `emoji` shortcode".into(),
                        )
                    })?;
                let ch = emoji_codepoint(shortcode).ok_or_else(|| {
                    AdapterError::Unsupported(format!(
                        "unknown gchat emoji shortcode `{shortcode}`"
                    ))
                })?;
                self.api.create_reaction(message_name, ch).await?;
                Ok(None)
            }
            other => Err(AdapterError::Unsupported(format!(
                "gchat action `{other}` is not supported"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::{MessageKind, OutboundFile};
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> GchatAdapter {
        GchatAdapter::new(
            ChannelType::new("gchat"),
            GchatApi::new(server.uri(), "tok-test"),
        )
    }

    fn text(msg: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": msg}),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn channel_type_and_thread_support() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert_eq!(adapter.channel_type().as_str(), "gchat");
        assert!(adapter.supports_threads());
    }

    #[tokio::test]
    async fn deliver_text_calls_send_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .and(header("authorization", "Bearer tok-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/100"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("spaces/AAA", None, &text("hello"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/100"));
    }

    #[tokio::test]
    async fn deliver_threaded_calls_send_threaded_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .and(query_param("messageReplyOption", "REPLY_MESSAGE_OR_FAIL"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/200"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver(
                "spaces/AAA",
                Some("spaces/AAA/threads/T1"),
                &text("threaded"),
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/200"));
    }

    #[tokio::test]
    async fn deliver_card_calls_send_card() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/CARD"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"card": {"header": {"title": "x"}}, "card_id": "c1"}),
            files: vec![],
        };
        let id = adapter.deliver("spaces/AAA", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/CARD"));
    }

    #[tokio::test]
    async fn deliver_card_defaults_card_id_when_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/CARD2"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"card": {"header": {"title": "x"}}}),
            files: vec![],
        };
        let id = adapter.deliver("spaces/AAA", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/CARD2"));
    }

    #[tokio::test]
    async fn deliver_edit_action_routes_to_put_with_update_mask() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/spaces/AAA/messages/100"))
            .and(query_param("updateMask", "text"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/100"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "edit",
                "message_name": "spaces/AAA/messages/100",
                "text": "edited"
            }),
            files: vec![],
        };
        let id = adapter.deliver("spaces/AAA", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/100"));
    }

    #[tokio::test]
    async fn deliver_edit_action_missing_message_name_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "edit", "text": "x"}),
            files: vec![],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("message_name")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_action_missing_text_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "edit", "message_name": "spaces/AAA/messages/100"}),
            files: vec![],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("text")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_delete_action_routes_to_delete() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/spaces/AAA/messages/100"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "delete",
                "message_name": "spaces/AAA/messages/100"
            }),
            files: vec![],
        };
        let id = adapter.deliver("spaces/AAA", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/100"));
    }

    #[tokio::test]
    async fn deliver_delete_missing_message_name_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "delete"}),
            files: vec![],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_action_uses_mapped_codepoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages/100/reactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "reaction",
                "message_name": "spaces/AAA/messages/100",
                "emoji": "thumbsup"
            }),
            files: vec![],
        };
        let id = adapter.deliver("spaces/AAA", None, &msg).await.unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn deliver_reaction_with_unknown_emoji_is_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "reaction",
                "message_name": "spaces/AAA/messages/100",
                "emoji": "not-a-real-emoji"
            }),
            files: vec![],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("not-a-real-emoji")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_missing_message_name_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "reaction", "emoji": "thumbsup"}),
            files: vec![],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("message_name")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_missing_emoji_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "reaction",
                "message_name": "spaces/AAA/messages/100"
            }),
            files: vec![],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("emoji")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "frobnicate"}),
            files: vec![],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("frobnicate")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_files_uploads_then_attaches_resource_names() {
        let server = MockServer::start().await;
        // attachments:upload returns a resourceName.
        Mock::given(method("POST"))
            .and(path("/upload/v1/spaces/AAA/attachments:upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "attachmentDataRef": { "resourceName": "spaces/AAA/attachments/abc" },
            })))
            .mount(&server)
            .await;
        // messages.create includes the attachment[] array.
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/M-with-att",
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "see attached"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"hi".to_vec(),
            }],
        };
        let id = adapter
            .deliver("spaces/AAA", None, &msg)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/M-with-att"));
    }

    #[tokio::test]
    async fn deliver_with_files_and_card_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({
                "card": { "header": { "title": "x" } }
            }),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"hi".to_vec(),
            }],
        };
        match adapter.deliver("spaces/AAA", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("card")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_bad_platform_id_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = text("oops");
        match adapter.deliver("invalid-id", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("spaces/")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_surfaces_auth_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": {"code": 401, "message": "no creds", "status": "UNAUTHENTICATED"}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("spaces/AAA", None, &text("x")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_surfaces_rate_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "9"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("spaces/AAA", None, &text("x")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(9)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_surfaces_transport_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("spaces/AAA", None, &text("x")).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_surfaces_bad_request_404() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": {"message": "no such space"}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("spaces/AAA", None, &text("x")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("no such space")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(adapter.open_dm("users/1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn subscribe_uses_default_impl() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.subscribe("spaces/A", None).await.unwrap();
        adapter.subscribe("spaces/A", Some("spaces/A/threads/X")).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_default_impl_is_ok() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.set_typing("spaces/A", None).await.unwrap();
        adapter
            .set_typing("spaces/A", Some("spaces/A/threads/X"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(format!("{:?}", adapter.api()).contains("tok-test"));
    }

    #[tokio::test]
    async fn server_handle_shutdown_is_idempotent() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        adapter.set_server_handle(task);
        adapter.shutdown_server();
        adapter.shutdown_server();
    }

    #[tokio::test]
    async fn deliver_empty_text_works() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/EMPTY"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        let id = adapter.deliver("spaces/AAA", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/EMPTY"));
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let s = format!("{adapter:?}");
        assert!(s.contains("GchatAdapter"));
        assert!(s.contains("gchat"));
    }

    #[test]
    fn space_segment_strips_prefix() {
        assert_eq!(
            GchatAdapter::space_segment("spaces/AAA").unwrap(),
            "AAA"
        );
        assert!(GchatAdapter::space_segment("AAA").is_err());
        assert!(GchatAdapter::space_segment("").is_err());
    }

    // ── Breadcrumb chip rendering ──────────────────────────────────

    #[test]
    fn build_breadcrumb_card_running_uses_ascii_marker_in_top_label() {
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let card = super::build_breadcrumb_card(&bc);
        let section = &card["sections"][0]["widgets"][0]["decoratedText"];
        assert_eq!(section["topLabel"], "[~] shell");
        assert_eq!(section["text"], "cargo check");
        // `startIcon` is intentionally absent — Material `knownIcon`
        // renders as a colourful pictogram violating the no-emoji rule.
        assert!(section.get("startIcon").is_none());
    }

    #[test]
    fn build_breadcrumb_card_done_with_summary() {
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let card = super::build_breadcrumb_card(&bc);
        let section = &card["sections"][0]["widgets"][0]["decoratedText"];
        assert_eq!(section["topLabel"], "[ok] shell");
        assert!(section["text"]
            .as_str()
            .unwrap()
            .contains("passed (0.4s)"));
    }

    #[test]
    fn build_breadcrumb_card_failed_prefixes_failed() {
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(false, Some("timeout".into()));
        let card = super::build_breadcrumb_card(&bc);
        let section = &card["sections"][0]["widgets"][0]["decoratedText"];
        assert_eq!(section["topLabel"], "[x] shell");
        assert!(section["text"]
            .as_str()
            .unwrap()
            .contains("failed: timeout"));
    }

    #[tokio::test]
    async fn deliver_breadcrumb_running_posts_card_to_create() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/M1"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let id = adapter
            .deliver_breadcrumb("spaces/AAA", None, &bc, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/M1"));
        let req_body: serde_json::Value = serde_json::from_slice(
            &server.received_requests().await.unwrap().last().unwrap().body,
        )
        .unwrap();
        // cardsV2 array with our chip card.
        let card = &req_body["cardsV2"][0];
        assert_eq!(card["cardId"], "breadcrumb");
        let text = card["card"]["sections"][0]["widgets"][0]["decoratedText"]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text, "cargo check");
    }

    #[tokio::test]
    async fn deliver_breadcrumb_with_existing_id_patches_card() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/spaces/AAA/messages/M1"))
            .and(query_param("updateMask", "cardsV2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/M1"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let id = adapter
            .deliver_breadcrumb(
                "spaces/AAA",
                None,
                &bc,
                Some("spaces/AAA/messages/M1"),
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/M1"));
    }

    // ── Diff card rendering ────────────────────────────────────────

    #[test]
    fn build_diff_card_carries_path_in_header_and_widget_per_hunk() {
        let card = copperclaw_channels_core::DiffCard {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 10,
                old_lines: 1,
                new_start: 10,
                new_lines: 1,
                lines: vec![
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Remove,
                        text: "let x = 1;".into(),
                    },
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Add,
                        text: "let x = 2;".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let card_json = super::build_diff_card(&card);
        assert_eq!(card_json["header"]["title"], "src/lib.rs");
        assert_eq!(card_json["header"]["subtitle"], "+1 / -1");
        let widgets = card_json["sections"][0]["widgets"].as_array().unwrap();
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0]["decoratedText"]["topLabel"], "@@ -10,1 +10,1 @@");
        let body = widgets[0]["decoratedText"]["text"].as_str().unwrap();
        assert!(body.contains("-let x = 1;"));
        assert!(body.contains("+let x = 2;"));
        assert!(body.contains("monospace"));
    }

    #[test]
    fn build_diff_card_html_escapes_source_lines() {
        let card = copperclaw_channels_core::DiffCard {
            path: "x.rs".into(),
            language: None,
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                lines: vec![copperclaw_channels_core::DiffLine {
                    kind: copperclaw_channels_core::DiffLineKind::Add,
                    text: "fn f<T>() -> &str { \"a&b\" }".into(),
                }],
            }],
            added: 1,
            removed: 0,
            truncated: false,
        };
        let card_json = super::build_diff_card(&card);
        let body = card_json["sections"][0]["widgets"][0]["decoratedText"]["text"]
            .as_str()
            .unwrap();
        assert!(body.contains("&lt;T&gt;"), "html-escapes < > : {body}");
        assert!(body.contains("&amp;"), "html-escapes &");
    }

    #[tokio::test]
    async fn deliver_diff_posts_card_via_messages_create() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/M9"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = copperclaw_channels_core::DiffCard {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![copperclaw_channels_core::DiffLine {
                    kind: copperclaw_channels_core::DiffLineKind::Add,
                    text: "fn main() {}".into(),
                }],
            }],
            added: 1,
            removed: 0,
            truncated: false,
        };
        let id = adapter
            .deliver_diff("spaces/AAA", None, &card)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/M9"));
        let req_body: serde_json::Value = serde_json::from_slice(
            &server.received_requests().await.unwrap().last().unwrap().body,
        )
        .unwrap();
        assert_eq!(req_body["cardsV2"][0]["cardId"], "diff");
    }

    // ── Error card rendering ────────────────────────────────────────

    #[test]
    fn build_error_card_header_prefixes_with_error_label() {
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Internal,
            "shell tool exited 137",
        )
        .with_title("Tool failed");
        let built = super::build_error_card(&card);
        assert_eq!(built["header"]["title"], "Error: Tool failed");
        assert_eq!(built["header"]["subtitle"], "shell tool exited 137");
    }

    #[test]
    fn build_error_card_kind_label_varies() {
        for (kind, expected) in [
            (
                copperclaw_channels_core::ErrorCardKind::Internal,
                "Tool error",
            ),
            (
                copperclaw_channels_core::ErrorCardKind::Provider,
                "Provider error",
            ),
            (
                copperclaw_channels_core::ErrorCardKind::Delivery,
                "Delivery error",
            ),
        ] {
            let card = copperclaw_channels_core::ErrorCard::new(kind, "x");
            let built = super::build_error_card(&card);
            let widgets = built["sections"][0]["widgets"].as_array().unwrap();
            let label_text = widgets[0]["decoratedText"]["text"].as_str().unwrap();
            assert!(
                label_text.contains(expected),
                "{kind:?} → label_text={label_text:?}"
            );
        }
    }

    #[test]
    fn build_error_card_details_lands_in_monospace_text_paragraph() {
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Internal,
            "tool timed out",
        )
        .with_details("stderr: SIGKILL\nexit 137");
        let built = super::build_error_card(&card);
        let widgets = built["sections"][0]["widgets"].as_array().unwrap();
        // Index 1: details textParagraph; index 0 is the severity-label decoratedText.
        let mono = widgets[1]["textParagraph"]["text"].as_str().unwrap();
        assert!(mono.contains("monospace"), "expected font face: {mono}");
        assert!(mono.contains("SIGKILL"));
        // Newlines become <br> so paragraph breaks survive.
        assert!(mono.contains("<br>"));
    }

    #[test]
    fn build_error_card_retryable_appends_italic_footer() {
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Delivery,
            "telegram 502",
        )
        .retryable();
        let built = super::build_error_card(&card);
        let widgets = built["sections"][0]["widgets"].as_array().unwrap();
        let last = widgets.last().unwrap();
        let text = last["textParagraph"]["text"].as_str().unwrap();
        assert!(text.contains("<i>"));
        assert!(text.contains("will retry automatically"));
    }

    #[tokio::test]
    async fn deliver_error_posts_card_to_spaces_messages() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "spaces/AAA/messages/E1"
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Provider,
            "model 502 after retry exhaustion",
        )
        .with_title("Provider failed");
        let id = adapter
            .deliver_error("spaces/AAA", None, &card)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/E1"));
        let req_body: serde_json::Value = serde_json::from_slice(
            &server.received_requests().await.unwrap().last().unwrap().body,
        )
        .unwrap();
        // Sent via cardsV2 with a sensible card id.
        let card_v2 = &req_body["cardsV2"][0];
        assert_eq!(card_v2["cardId"], "error");
        assert_eq!(
            card_v2["card"]["header"]["title"],
            "Error: Provider failed"
        );
    }

    // ── Long-output expander (slice 3.4) rendering ────────────────

    #[test]
    fn build_collapsible_card_uses_native_collapsible_section() {
        // Native Google Chat primitive — `collapsibleSection`. The
        // summary rides as the section header, preview lines are
        // first (uncollapsibly visible), full body rides as a
        // monospace paragraph that collapses underneath.
        let body = "alpha\nbeta\ngamma";
        let preview = vec!["alpha".to_owned(), "beta".to_owned()];
        let card = super::build_collapsible_card(body, "shell 3 lines (15 B)", &preview);
        let section = &card["sections"][0];
        assert_eq!(section["header"], "shell 3 lines (15 B)");
        assert_eq!(section["collapsible"], true);
        assert_eq!(section["uncollapsibleWidgetsCount"], 2);
        let widgets = section["widgets"].as_array().unwrap();
        assert_eq!(widgets.len(), 3, "two preview + one body");
        // Preview widgets are plain text paragraphs.
        assert_eq!(widgets[0]["textParagraph"]["text"], "alpha");
        assert_eq!(widgets[1]["textParagraph"]["text"], "beta");
        // Body uses monospace font with `<br>` line breaks.
        let body_text = widgets[2]["textParagraph"]["text"].as_str().unwrap();
        assert!(body_text.contains("face=\"monospace\""));
        assert!(body_text.contains("alpha<br>beta<br>gamma"));
    }

    #[test]
    fn build_collapsible_card_empty_preview_keeps_section_collapsible() {
        // With no preview the section still uses
        // `collapsible: true` (so the user gets the disclosure
        // affordance) but `uncollapsibleWidgetsCount = 0`.
        let card = super::build_collapsible_card("body", "summary", &[]);
        let section = &card["sections"][0];
        assert_eq!(section["collapsible"], true);
        assert_eq!(section["uncollapsibleWidgetsCount"], 0);
        let widgets = section["widgets"].as_array().unwrap();
        assert_eq!(widgets.len(), 1, "body only");
    }

    #[test]
    fn build_collapsible_card_escapes_html_in_preview_and_body() {
        // Tool output may contain literal `<` / `>` / `&`; these
        // must be entity-encoded or Google Chat strips them.
        let body = "<script>alert(1)</script> & <div>";
        let preview = vec!["<warn>".to_owned()];
        let card = super::build_collapsible_card(body, "summary", &preview);
        let widgets = card["sections"][0]["widgets"].as_array().unwrap();
        assert_eq!(widgets[0]["textParagraph"]["text"], "&lt;warn&gt;");
        let body_text = widgets[1]["textParagraph"]["text"].as_str().unwrap();
        assert!(body_text.contains("&lt;script&gt;"));
        assert!(body_text.contains("&amp;"));
    }

    // ── Thinking block (slice 3.5) rendering ────────────────────────

    #[test]
    fn build_thinking_card_uses_collapsible_section_with_reasoning_header() {
        // Native Google Chat primitive for this surface is the same
        // `collapsibleSection` widget surface 4 uses for long-output —
        // the user sees the header collapsed by default and clicks to
        // expand the reasoning body.
        let t = ThinkingBlock::visible("Let me work through the question.");
        let card = super::build_thinking_card(&t);
        let section = &card["sections"][0];
        assert_eq!(section["collapsible"], true);
        assert_eq!(section["uncollapsibleWidgetsCount"], 0);
        assert_eq!(section["header"], "reasoning");
        let widgets = section["widgets"].as_array().unwrap();
        let body = widgets[0]["textParagraph"]["text"].as_str().unwrap();
        assert!(body.contains("Let me work through the question."), "got: {body}");
        assert!(body.contains("<i>"), "expected italic wrap, got: {body}");
    }

    #[test]
    fn build_thinking_card_header_includes_model_provenance() {
        let t = ThinkingBlock::visible("ok").with_model("claude-opus-4-7");
        let card = super::build_thinking_card(&t);
        assert_eq!(card["sections"][0]["header"], "reasoning (claude-opus-4-7)");
    }

    #[test]
    fn build_thinking_card_redacted_emits_placeholder_only() {
        // Privacy contract: redacted blocks must never put the raw
        // blob on the wire.
        let t = ThinkingBlock::redacted("opaque-secret-blob");
        let card = super::build_thinking_card(&t);
        let raw = serde_json::to_string(&card).unwrap();
        assert!(!raw.contains("opaque-secret-blob"), "leak: {raw}");
        assert!(raw.contains("(redacted reasoning)"));
    }

    #[test]
    fn build_thinking_card_escapes_html_in_body() {
        let t = ThinkingBlock::visible("<script>alert(1)</script>");
        let card = super::build_thinking_card(&t);
        let body = card["sections"][0]["widgets"][0]["textParagraph"]["text"]
            .as_str()
            .unwrap();
        assert!(!body.contains("<script>"), "got: {body}");
        assert!(body.contains("&lt;script&gt;"));
    }

    // ── TodoList chip rendering ────────────────────────────────────

    fn gchat_todo_list_sample() -> copperclaw_channels_core::TodoList {
        copperclaw_channels_core::TodoList {
            items: vec![
                copperclaw_channels_core::TodoListItem {
                    id: 1,
                    text: "Wash dishes".into(),
                    status: copperclaw_channels_core::TodoItemStatus::Completed,
                },
                copperclaw_channels_core::TodoListItem {
                    id: 2,
                    text: "Dry dishes".into(),
                    status: copperclaw_channels_core::TodoItemStatus::InProgress,
                },
            ],
            title: Some("Kitchen".into()),
        }
    }

    #[test]
    fn build_todo_list_card_has_header_and_widget_per_item() {
        let card = super::build_todo_list_card(&gchat_todo_list_sample());
        let header_title = card["header"]["title"].as_str().unwrap();
        assert!(header_title.starts_with("Kitchen"));
        assert!(header_title.contains("(1/2)"));
        let widgets = card["sections"][0]["widgets"].as_array().unwrap();
        assert_eq!(widgets.len(), 2);
        // No `startIcon` — Material `knownIcon` violates the no-emoji
        // rule. The status marker rides as an ASCII prefix in `text`.
        assert!(widgets[0]["decoratedText"].get("startIcon").is_none());
        let done_body = widgets[0]["decoratedText"]["text"].as_str().unwrap();
        assert!(done_body.starts_with("[x]"), "got: {done_body}");
        assert!(done_body.contains("<s>Wash dishes</s>"));
        assert!(widgets[1]["decoratedText"].get("startIcon").is_none());
        let prog_body = widgets[1]["decoratedText"]["text"].as_str().unwrap();
        assert!(prog_body.starts_with("[~]"), "got: {prog_body}");
    }

    #[tokio::test]
    async fn deliver_todo_list_first_emit_posts_to_create() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/spaces/AAA/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "name": "spaces/AAA/messages/MSG1" })),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver_todo_list("spaces/AAA", None, &gchat_todo_list_sample(), None, true)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/MSG1"));
    }

    #[tokio::test]
    async fn deliver_todo_list_with_existing_id_patches_card() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/spaces/AAA/messages/MSG1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "name": "spaces/AAA/messages/MSG1" })),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver_todo_list(
                "spaces/AAA",
                None,
                &gchat_todo_list_sample(),
                Some("spaces/AAA/messages/MSG1"),
                false,
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("spaces/AAA/messages/MSG1"));
    }
}
