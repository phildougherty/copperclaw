//! Slack [`ChannelAdapter`] implementation.

use crate::api::{CompleteUploadEntry, SlackApi, build_card_blocks};
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, Breadcrumb, BreadcrumbStatus, Card, ChannelAdapter, DiffCard, DmHandle,
    ErrorCard, ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
};
use ironclaw_types::{ChannelType, OutboundFile, OutboundMessage};
use serde_json::{json, Value};
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Slack channel adapter. See module-level docs.
pub struct SlackAdapter {
    channel_type: ChannelType,
    api: SlackApi,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for SlackAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl SlackAdapter {
    /// Construct with an already-built API client. Used by the factory and
    /// by tests that drive the adapter directly.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: SlackApi) -> Self {
        Self {
            channel_type,
            api,
            server_handle: Mutex::new(None),
        }
    }

    /// Attach the join handle of the spawned axum server so the adapter can
    /// abort it on shutdown.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("slack adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background events server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("slack adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &SlackApi {
        &self.api
    }

    /// Upload any attachments and return the Slack file ids. Used by
    /// `deliver` for outbound messages that carry files.
    async fn upload_files(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        files: &[OutboundFile],
    ) -> Result<(), AdapterError> {
        if files.is_empty() {
            return Ok(());
        }
        let mut entries: Vec<CompleteUploadEntry> = Vec::with_capacity(files.len());
        for file in files {
            let url = self
                .api
                .files_get_upload_url_external(&file.filename, file.data.len())
                .await?;
            self.api
                .files_upload_to_url(&url.upload_url, file.data.clone())
                .await?;
            entries.push(CompleteUploadEntry {
                id: url.file_id,
                title: Some(file.filename.clone()),
            });
        }
        self.api
            .files_complete_upload_external(&entries, Some(channel), thread_ts)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ChannelAdapter for SlackAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    /// Slack's `chat.postMessage` hard-caps `text` at 40 000 chars.
    /// (The platform recommends ~3 000 for readability, but the host's
    /// job is to honour the wire limit; readability is a content concern.)
    fn max_message_chars(&self) -> Option<usize> {
        Some(40_000)
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let Some(thread) = thread_id else {
            // Slack only exposes a typing status for assistants threads.
            return Ok(());
        };
        // Best effort — `assistant.threads.setStatus` returns an error
        // outside an Assistants context. Swallow `not_in_channel`-style
        // bad-request responses so typing remains a soft no-op.
        match self
            .api
            .set_assistant_status(platform_id, thread, "is typing...")
            .await
        {
            Ok(()) | Err(AdapterError::BadRequest(_)) => Ok(()),
            Err(other) => Err(other),
        }
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let blocks = message.content.get("blocks").cloned();
        let ephemeral_to = message
            .content
            .get("ephemeral_to")
            .and_then(Value::as_str);

        let ts = if let Some(user) = ephemeral_to {
            self.api
                .post_ephemeral(platform_id, user, thread_id, &text)
                .await?
                .ts
        } else {
            self.api
                .post_message(platform_id, thread_id, &text, blocks.as_ref())
                .await?
                .ts
        };

        if !message.files.is_empty() {
            self.upload_files(platform_id, thread_id, &message.files)
                .await?;
        }

        Ok(ts)
    }

    async fn edit_message(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        external_id: &str,
        new_text: &str,
    ) -> Result<(), AdapterError> {
        // Slack's `chat.update` is addressed by (channel, ts); thread context
        // is implicit via the original ts so we discard the thread_id.
        self.api.chat_update(platform_id, external_id, new_text).await?;
        Ok(())
    }

    /// Render a [`Card`] natively via Slack Block Kit and post it to the
    /// channel.
    ///
    /// The blocks are built by [`build_card_blocks`]; the `text` field on
    /// `chat.postMessage` carries the canonical text-fallback rendering
    /// (from [`Card::to_text_fallback`]) so notification surfaces (mobile
    /// previews, email digests, screen readers) and any future block-render
    /// downgrade still show a human-readable card body.
    ///
    /// `to` is accepted for parity with the trait signature; Slack is
    /// addressed via `platform_id` (the channel id) and doesn't need a
    /// separate routing hint.
    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        _to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let blocks = build_card_blocks(card);
        let text = card.to_text_fallback();
        let resp = self
            .api
            .post_message(platform_id, thread_id, &text, Some(&blocks))
            .await?;
        Ok(resp.ts)
    }

    async fn add_reaction(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        external_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        // Slack reactions are addressed by name (no surrounding `:`); strip
        // any colons the agent might have left behind.
        let name = emoji.trim_matches(':');
        self.api.reactions_add(platform_id, external_id, name).await
    }

    /// Native breadcrumb chip — rendered as a Block Kit `context`
    /// block containing a status emoji and a monospace `mrkdwn`
    /// fragment so Slack draws a small chip rather than a chat row.
    /// When `existing_message_id` (the original chip's `ts`) is
    /// provided we drive `chat.update` to edit the chip in place;
    /// otherwise we post a fresh `chat.postMessage`.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let blocks = build_breadcrumb_blocks(breadcrumb);
        let fallback_text = breadcrumb.to_text_fallback();
        if let Some(ts) = existing_message_id {
            let resp = self
                .api
                .chat_update_with_blocks(platform_id, ts, &fallback_text, Some(&blocks))
                .await?;
            // Slack chat.update preserves the original `ts`; if the
            // response omits it (mock / fixture quirk) we fall back
            // to the existing id so subsequent updates keep
            // targeting the same chip.
            return Ok(resp.ts.or_else(|| Some(ts.to_owned())));
        }
        let resp = self
            .api
            .post_message(platform_id, thread_id, &fallback_text, Some(&blocks))
            .await?;
        Ok(resp.ts)
    }

    /// Native diff card — rendered as a Block Kit message with:
    ///
    /// 1. A `section` mrkdwn header carrying `*<path>* (+N / -M)` plus
    ///    a `· truncated` suffix when caps kicked in.
    /// 2. One `rich_text` block per hunk containing a
    ///    `rich_text_preformatted` element with the hunk's body
    ///    (`@@ -…@@` header line, then `+`/`-`/` `-prefixed lines).
    ///    Preformatted blocks honour the leading `+` / `-` chars
    ///    visually and dodge the 3 000-char per-`section` cap that
    ///    bites when a diff is stuffed into a single mrkdwn block.
    ///
    /// The fallback `text` field on `chat.postMessage` carries the
    /// unified-diff text rendering so notification surfaces (mobile
    /// push, email digest) still show a useful preview.
    async fn deliver_diff(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        let blocks = build_diff_blocks(diff);
        let fallback_text = diff.to_text_fallback();
        let resp = self
            .api
            .post_message(platform_id, thread_id, &fallback_text, Some(&blocks))
            .await?;
        Ok(resp.ts)
    }

    /// Native long-output expander (slice 3.4) — Slack has no
    /// native disclosure widget (`<details>` doesn't exist in mrkdwn,
    /// Block Kit has no collapsed-by-default element), so we
    /// approximate the surface by posting a Block Kit message with:
    ///
    /// 1. A `section` mrkdwn block carrying the summary (`shell
    ///    produced 312 lines (12 KB)`) and the preview lines in a
    ///    code fence — at-a-glance "what was emitted".
    /// 2. A `rich_text` block containing the FULL body in a
    ///    `rich_text_preformatted` element. Slack does collapse
    ///    very long preformatted blocks behind a "Show more" link
    ///    natively (the "Show more" affordance kicks in on bodies
    ///    over ~700 chars on desktop, ~400 chars on mobile) so the
    ///    full text still gets the disclosure treatment without us
    ///    having to mirror the runner's threshold logic.
    ///
    /// Falling back to the trait-default text impl here would lose
    /// the structured-block rendering and risk the body being split
    /// by the host's chat splitter into separate messages — the
    /// per-block 3000-char limit Slack enforces is finer-grained
    /// than the message-level splitter. Block Kit handles this for
    /// us: oversized `rich_text_preformatted` content gets the
    /// native "Show more" link.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        let blocks = build_collapsible_blocks(text, summary, preview_lines);
        // The fallback `text` field on `chat.postMessage` powers
        // notification previews (mobile push, email digest). Use the
        // summary alone — pushing the entire body into a notification
        // body would defeat the whole point of a collapsible.
        let resp = self
            .api
            .post_message(platform_id, thread_id, summary, Some(&blocks))
            .await?;
        Ok(resp.ts)
    }

    /// Native todo-list checklist — rendered as Block Kit `section`
    /// blocks (one per item) with an emoji prefix indicating each
    /// item's status (`:white_check_mark:` / `:arrow_forward:` /
    /// `:white_square:`), a header `section` carrying the title and
    /// `done/total` counter. On first emit we post via
    /// `chat.postMessage` then `pins.add` so the chip stays visible
    /// in the channel pins; on subsequent mutations we drive
    /// `chat.update` with the same `ts` so the user sees the same
    /// chip tick through. When every item completes we `pins.remove`
    /// so the pin list doesn't fill with finished plans.
    ///
    /// Pin / unpin failures are swallowed at `debug` — the bot may
    /// lack the `pins:write` scope on legacy workspaces and we
    /// don't want the chip to fail over a permissions blip.
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        list: &TodoList,
        existing_message_id: Option<&str>,
        pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        let blocks = build_todo_list_blocks(list);
        let fallback_text = list.to_text_fallback();
        let ts = if let Some(existing) = existing_message_id {
            let resp = self
                .api
                .chat_update_with_blocks(platform_id, existing, &fallback_text, Some(&blocks))
                .await?;
            resp.ts.or_else(|| Some(existing.to_owned())).unwrap_or_else(|| existing.to_owned())
        } else {
            let resp = self
                .api
                .post_message(platform_id, thread_id, &fallback_text, Some(&blocks))
                .await?;
            let Some(new_ts) = resp.ts else {
                return Ok(None);
            };
            if pin_hint {
                if let Err(err) = self.api.pins_add(platform_id, &new_ts).await {
                    tracing::debug!(
                        ?err,
                        ts = %new_ts,
                        "slack pins.add failed (ignored)"
                    );
                }
            }
            new_ts
        };
        if pin_hint && list.is_fully_completed() && existing_message_id.is_some() {
            if let Err(err) = self.api.pins_remove(platform_id, &ts).await {
                tracing::debug!(
                    ?err,
                    ts = %ts,
                    "slack pins.remove failed (ignored)"
                );
            }
        }
        Ok(Some(ts))
    }

    /// Native error card — rendered via Slack's secondary-message
    /// attachments API so we can drive the coloured left bar
    /// affordance Block Kit's primary blocks can't produce. Color
    /// keys off [`ErrorCardKind`]:
    ///
    /// - `Internal` / `Provider` / `Delivery` → `"danger"` (red bar)
    ///
    /// All three host-emit sites land as red — design doc treats
    /// retry-exhaustion as just as serious as a tool failing. (Future
    /// `Warning` / `RateLimit` etc. variants would map to `warning` /
    /// `#1264A3`; the schema doesn't have them today.)
    ///
    /// Body shape inside the attachment: a `section` mrkdwn block for
    /// the title + summary, optional `rich_text_preformatted` for
    /// details. The top-level `text` field carries the canonical text
    /// fallback so Slack's notification preview is still legible.
    async fn deliver_error(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        let attachments = build_error_attachments(err);
        let fallback_text = err.to_text_fallback();
        let resp = self
            .api
            .post_message_with_attachments(platform_id, thread_id, &fallback_text, &attachments)
            .await?;
        Ok(resp.ts)
    }

    /// Native thinking block (slice 3.5) — rendered as a Block Kit
    /// `context` block (the platform's idiomatic muted-metadata
    /// affordance — same primitive the breadcrumb chip uses) with a
    /// `:thought_balloon:` emoji + `reasoning` label, followed by one
    /// or more `mrkdwn` blocks carrying the chunked reasoning text.
    /// The mrkdwn fragments are capped at 3000 chars apiece (Slack's
    /// per-element limit) and spilled across multiple context blocks
    /// so a long block doesn't trip element-level validation.
    /// Redacted blocks render the placeholder via `to_text_fallback`
    /// so the raw blob never reaches the wire.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        let blocks = build_thinking_blocks(thinking);
        let fallback_text = thinking.to_text_fallback();
        let resp = self
            .api
            .post_message(platform_id, thread_id, &fallback_text, Some(&blocks))
            .await?;
        Ok(resp.ts)
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Slack DMs are addressed by user id directly via `chat.postMessage`,
        // so we don't need a pre-opened conversations.open handle. Returning
        // `None` matches the channel-trait default for adapters that don't
        // pre-resolve DMs.
        Ok(None)
    }

    /// Slack-specific plain-text fallback used by the delivery loop when
    /// `chat.postMessage` rejected the original payload with a block-kit
    /// validation error. Drops the `blocks` field — Slack then renders just
    /// the `text` fallback string — and prepends `"[reduced formatting] "`
    /// so the recipient knows the layout was simplified. Returns `None`
    /// when there are no `blocks` to strip (nothing to fall back to).
    fn plain_text_fallback(&self, msg: &OutboundMessage) -> Option<OutboundMessage> {
        let obj = msg.content.as_object()?;
        if !obj.contains_key("blocks") {
            return None;
        }
        let text = obj
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut new_obj = obj.clone();
        new_obj.remove("blocks");
        new_obj.insert(
            "text".to_owned(),
            Value::String(format!("[reduced formatting] {text}")),
        );
        Some(OutboundMessage {
            kind: msg.kind,
            content: Value::Object(new_obj),
            files: msg.files.clone(),
        })
    }
}

/// Build the Slack Block Kit block array for a [`DiffCard`].
///
/// Layout:
///
/// - `{"type": "section", "text": {"type": "mrkdwn", "text":
///   "*<path>* (+N / -M)"}}`
/// - one `{"type": "rich_text", "elements": [{"type":
///   "rich_text_preformatted", ...}]}` block per hunk
///
/// `rich_text_preformatted` is the Block Kit primitive Slack uses
/// for code blocks; oversized preformatted content gets a native
/// "Show more" disclosure on both desktop and mobile clients, which
/// dodges the 3 000-char per-`section` cap that bites when a long
/// diff is stuffed into a single mrkdwn block.
pub(crate) fn build_diff_blocks(diff: &DiffCard) -> Value {
    let totals = if diff.truncated {
        format!("(+{} / -{} · truncated)", diff.added, diff.removed)
    } else {
        format!("(+{} / -{})", diff.added, diff.removed)
    };
    let header_text = format!("*{}*  {totals}", escape_mrkdwn(&diff.path));
    let mut blocks: Vec<Value> = vec![json!({
        "type": "section",
        "text": { "type": "mrkdwn", "text": header_text },
    })];
    for h in &diff.hunks {
        let mut body = String::with_capacity(64 + h.lines.len() * 64);
        body.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        ));
        for line in &h.lines {
            body.push(line.kind.unified_prefix());
            body.push_str(&line.text);
            body.push('\n');
        }
        blocks.push(json!({
            "type": "rich_text",
            "elements": [{
                "type": "rich_text_preformatted",
                "elements": [{ "type": "text", "text": body }],
            }],
        }));
    }
    Value::Array(blocks)
}

/// Build a Slack Block Kit `context` block carrying the breadcrumb's
/// emoji + mrkdwn body. `context` blocks are Slack's idiomatic
/// "small metadata chip" affordance — they render in a compact
/// secondary style with the emoji and a short mrkdwn string, which
/// is exactly the Claude-Code-mobile chip aesthetic we want.
///
/// Format (one element per element index):
///
/// 1. `mrkdwn` element carrying an ASCII status marker (`[~]` running,
///    `[ok]` done, `[x]` failed) followed by `` `shell` `` (the tool
///    name as inline code) plus the detail / summary text.
///
/// We deliberately do NOT use Slack `emoji` elements for the status
/// indicator — every emoji shortcode (`:hourglass_flowing_sand:`,
/// `:white_check_mark:`, etc.) renders as a colourful emoji in the
/// Slack client, which violates the project's no-emoji rule.
pub(crate) fn build_breadcrumb_blocks(b: &Breadcrumb) -> Value {
    let marker = match b.status {
        BreadcrumbStatus::Running => "[~]",
        BreadcrumbStatus::Done => "[ok]",
        BreadcrumbStatus::Failed => "[x]",
    };
    // Inline-code the tool name (Slack mrkdwn uses backticks). Detail
    // text rides as plain mrkdwn so URLs/paths render normally.
    let mut text = format!("{marker} `{}`", escape_mrkdwn(&b.tool_name));
    if let Some(d) = b.detail.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            text.push_str(" · ");
            text.push_str(&escape_mrkdwn(d));
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
            text.push_str(&escape_mrkdwn(s));
        }
    }
    json!([
        {
            "type": "context",
            "elements": [
                { "type": "mrkdwn", "text": text },
            ]
        }
    ])
}

/// Build the Slack Block Kit block array for the slice-3.4
/// long-output expander surface. Layout:
///
/// 1. `section` mrkdwn block carrying the summary (`shell produced
///    312 lines (12 KB)`).
/// 2. (Optional) `rich_text` block with `rich_text_preformatted`
///    holding the preview lines — only emitted when the runner
///    actually attached a non-empty `preview_lines` array.
/// 3. `rich_text` block with `rich_text_preformatted` holding the
///    FULL body. Slack natively collapses oversized preformatted
///    blocks behind a "Show more" link on both desktop and mobile,
///    which is the closest native primitive Slack offers to a
///    disclosure widget.
///
/// We do NOT use an `actions` button + interaction callback for
/// expansion: that would require a round-trip through Slack's
/// `block_actions` events, which the slice-3.4 surface doesn't yet
/// have a routed handler for. The native "Show more" is functionally
/// equivalent for the read-then-expand UX.
pub(crate) fn build_collapsible_blocks(
    text: &str,
    summary: &str,
    preview_lines: &[String],
) -> Value {
    let mut blocks: Vec<Value> = Vec::with_capacity(3);
    blocks.push(json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": escape_mrkdwn(summary.trim()),
        }
    }));
    if !preview_lines.is_empty() {
        let preview_text = preview_lines.join("\n");
        blocks.push(json!({
            "type": "rich_text",
            "elements": [{
                "type": "rich_text_preformatted",
                "elements": [{
                    "type": "text",
                    "text": preview_text,
                }],
            }]
        }));
    }
    blocks.push(json!({
        "type": "rich_text",
        "elements": [{
            "type": "rich_text_preformatted",
            "elements": [{
                "type": "text",
                "text": text,
            }],
        }]
    }));
    Value::Array(blocks)
}

/// Build a Slack secondary-message attachment array carrying the
/// canonical [`ErrorCard`] payload. Shape:
///
/// ```json
/// [{
///   "color": "danger",
///   "blocks": [
///     {"type": "header", "text": {"type": "plain_text", "text": "<title>"}},
///     {"type": "section", "text": {"type": "mrkdwn", "text": "<summary>"}},
///     {"type": "rich_text", ...details preformatted...}   // optional
///   ],
///   "footer": "will retry automatically"   // optional
/// }]
/// ```
///
/// `color` keys off [`ErrorCardKind`]:
///
/// - all three current kinds (`Internal` / `Provider` / `Delivery`)
///   render as `"danger"` (red bar) — the design doc treats them as
///   equally serious failures from the user's perspective.
pub(crate) fn build_error_attachments(err: &ErrorCard) -> Value {
    let color = match err.kind {
        ErrorCardKind::Internal | ErrorCardKind::Provider | ErrorCardKind::Delivery => "danger",
    };
    let mut blocks: Vec<Value> = Vec::with_capacity(4);
    // Header for the title — Slack's `plain_text` header has stricter
    // rules than `mrkdwn` (no markdown, no escapes), so feed the raw
    // title trimmed of whitespace.
    blocks.push(json!({
        "type": "header",
        "text": {
            "type": "plain_text",
            "text": err.title.trim(),
            "emoji": false,
        }
    }));
    // Section for the user-facing summary — `mrkdwn` so URLs etc.
    // render, with the three Slack control characters escaped.
    blocks.push(json!({
        "type": "section",
        "text": {
            "type": "mrkdwn",
            "text": escape_mrkdwn(err.summary.trim()),
        }
    }));
    if let Some(d) = err.details.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            // rich_text_preformatted renders as a monospace block in
            // Slack's modern client; the legacy fallback shows it as
            // a `code block` in the surface_text.
            blocks.push(json!({
                "type": "rich_text",
                "elements": [{
                    "type": "rich_text_preformatted",
                    "elements": [{ "type": "text", "text": d }]
                }]
            }));
        }
    }
    let mut attachment = json!({
        "color": color,
        "blocks": blocks,
    });
    if err.retryable {
        attachment["footer"] = Value::String("will retry automatically".to_owned());
    }
    json!([attachment])
}

/// Slack's per-block-element char cap. `context` and `section` mrkdwn
/// elements must stay under this; longer reasoning text gets split
/// across multiple blocks.
const MAX_MRKDWN_ELEMENT_CHARS: usize = 3000;

/// Build a Slack Block Kit block array carrying the canonical
/// [`ThinkingBlock`] payload. Shape: one `context` block bearing a
/// `:thought_balloon:` + `reasoning` label (with optional provenance
/// suffix), followed by one or more `context` blocks chunking the
/// reasoning text under Slack's 3000-char element cap. `context` is
/// Slack's idiomatic "small muted metadata" affordance — same
/// primitive the breadcrumb chip uses — so the block reads as a quiet
/// receipt of model reasoning, distinct from the agent's chat reply.
///
/// Redacted blocks render the placeholder body and never put the raw
/// blob on the wire — even via the canonical-fallback text field.
pub(crate) fn build_thinking_blocks(t: &ThinkingBlock) -> Value {
    let mut blocks: Vec<Value> = Vec::with_capacity(4);
    let label_suffix = match t.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => format!(" ({})", escape_mrkdwn(m)),
        _ => String::new(),
    };
    blocks.push(json!({
        "type": "context",
        "elements": [
            { "type": "emoji", "name": "thought_balloon" },
            { "type": "mrkdwn", "text": format!("_reasoning{label_suffix}_") },
        ]
    }));
    if t.redacted {
        blocks.push(json!({
            "type": "context",
            "elements": [{
                "type": "mrkdwn",
                "text": "_(redacted reasoning)_",
            }]
        }));
        return Value::Array(blocks);
    }
    // Split long reasoning text across multiple context blocks so a
    // single chunk stays under Slack's per-element char cap. We do this
    // greedily on `char` indices (NOT bytes) so non-ASCII content
    // doesn't get cut mid-codepoint.
    let chunks = split_for_slack_mrkdwn(&t.text, MAX_MRKDWN_ELEMENT_CHARS);
    for chunk in chunks {
        blocks.push(json!({
            "type": "context",
            "elements": [{
                "type": "mrkdwn",
                "text": escape_mrkdwn(&chunk),
            }]
        }));
    }
    Value::Array(blocks)
}

/// Split `text` into a sequence of `max`-char chunks. Conservative —
/// always cuts on the first newline before the boundary, falling back
/// to a hard char cut. Operates on `chars()` indices, never bytes.
fn split_for_slack_mrkdwn(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return vec![text.to_owned()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let remaining = chars.len() - start;
        if remaining <= max {
            out.push(chars[start..].iter().collect());
            break;
        }
        let hi = start + max;
        // Try to back off to the last newline in the window to keep
        // line boundaries intact across blocks.
        let mut cut = hi;
        let mut i = hi.saturating_sub(1);
        while i > start {
            if chars[i] == '\n' {
                cut = i + 1;
                break;
            }
            i -= 1;
        }
        out.push(chars[start..cut].iter().collect());
        start = cut;
    }
    out
}

/// Build a Slack Block Kit block array carrying the canonical
/// [`TodoList`] payload. Shape: one `header` block with the title +
/// `done/total` counter, followed by one `section` block per item
/// whose mrkdwn body is the status emoji + the item text
/// (strikethrough for completed). Section blocks are Slack's
/// idiomatic "list of items with status" affordance — the user can
/// scan completion at a glance.
///
/// Item text is escaped through [`escape_mrkdwn`] so user-supplied
/// `<` / `>` / `&` don't trip the parser. The block array stays well
/// under Slack's 50-block limit because the canonical [`TodoList`] caps
/// at 50 items (header + 50 sections = 51, which Slack accepts with
/// a soft warning; we trim if needed but in practice no agent will
/// surface 50 items at once).
pub(crate) fn build_todo_list_blocks(list: &TodoList) -> Value {
    let done = list.completed_count();
    let total = list.items.len();
    let header_text = format!("{} ({done}/{total})", list.title_or_default());
    let mut blocks: Vec<Value> = Vec::with_capacity(list.items.len() + 1);
    blocks.push(json!({
        "type": "header",
        "text": {
            "type": "plain_text",
            "text": header_text,
            "emoji": false,
        }
    }));
    for item in list.items.iter().take(48) {
        // 48 leaves headroom for header + footer-style addenda.
        let emoji = match item.status {
            // ASCII-only glyphs per the project's no-emoji rule.
            // Slack emoji shortcodes (`:white_check_mark:` etc.)
            // render as colourful emoji in the client.
            TodoItemStatus::Completed => "[x]",
            TodoItemStatus::InProgress => "[~]",
            TodoItemStatus::Pending => "[ ]",
        };
        let escaped = escape_mrkdwn(item.text.trim());
        let body = if item.status == TodoItemStatus::Completed {
            // mrkdwn `~text~` renders as strikethrough.
            format!("{emoji} ~{escaped}~")
        } else {
            format!("{emoji} {escaped}")
        };
        blocks.push(json!({
            "type": "section",
            "text": { "type": "mrkdwn", "text": body },
        }));
    }
    if list.items.len() > 48 {
        let dropped = list.items.len() - 48;
        blocks.push(json!({
            "type": "context",
            "elements": [{ "type": "mrkdwn", "text": format!("(+{dropped} more)") }]
        }));
    }
    Value::Array(blocks)
}

/// Slack mrkdwn only treats `&`, `<`, `>` as control characters; the
/// usual markdown punctuation is rendered literally. We escape just
/// those three so user-supplied paths / URLs / queries don't trip the
/// parser.
fn escape_mrkdwn(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_channels_core::{TodoItemStatus, TodoList, TodoListItem};
    use ironclaw_types::MessageKind;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> SlackAdapter {
        SlackAdapter::new(
            ChannelType::new("slack"),
            SlackApi::new(server.uri(), "xoxb-test"),
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
        assert_eq!(adapter.channel_type().as_str(), "slack");
        assert!(adapter.supports_threads());
    }

    #[tokio::test]
    async fn deliver_post_message_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(header("authorization", "Bearer xoxb-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "100.000"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("C1", None, &text("hello"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("100.000"));
    }

    #[tokio::test]
    async fn deliver_uses_thread_ts_when_provided() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "200.001"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("C1", Some("199.000"), &text("threaded"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("200.001"));
    }

    #[tokio::test]
    async fn deliver_uses_ephemeral_when_field_present() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postEphemeral"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "message_ts": "300.0"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "secret", "ephemeral_to": "U1"}),
            files: vec![],
        };
        let id = adapter.deliver("C1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("300.0"));
    }

    #[tokio::test]
    async fn deliver_surfaces_invalid_auth_as_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rate_limited_via_http_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "5"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(5)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rate_limited_via_ok_false_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "ratelimited"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Rate { .. }) => {}
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_other_slack_error_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "channel_not_found"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::BadRequest(s)) => assert_eq!(s, "channel_not_found"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_uploads_files_after_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "1.0"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.getUploadURLExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "upload_url": format!("{}/upload-target", server.uri()),
                "file_id": "F100"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/upload-target"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.completeUploadExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "see file"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"hello".to_vec(),
            }],
        };
        let ts = adapter.deliver("C1", None, &msg).await.unwrap();
        assert_eq!(ts.as_deref(), Some("1.0"));
    }

    #[tokio::test]
    async fn set_typing_no_thread_is_noop_ok() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.set_typing("C1", None).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_swallows_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.threads.setStatus"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "not_in_channel"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        adapter.set_typing("C1", Some("99.0")).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_propagates_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.threads.setStatus"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.set_typing("C1", Some("99.0")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_typing_propagates_rate_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.threads.setStatus"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.set_typing("C1", Some("99.0")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(1)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(adapter.open_dm("U1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn subscribe_uses_default_impl() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.subscribe("C1", None).await.unwrap();
        adapter.subscribe("C1", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(format!("{:?}", adapter.api()).contains("xoxb-test"));
    }

    #[tokio::test]
    async fn server_handle_shutdown_is_idempotent() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        // Set a handle (use a never-resolving future so abort actually matters).
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        adapter.set_server_handle(task);
        adapter.shutdown_server();
        adapter.shutdown_server();
    }

    #[tokio::test]
    async fn chat_update_roundtrip_via_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "5.0"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let resp = adapter
            .api()
            .chat_update("C1", "1.0", "edited")
            .await
            .unwrap();
        assert_eq!(resp.ts.as_deref(), Some("5.0"));
    }

    #[tokio::test]
    async fn reactions_add_via_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions.add"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        adapter
            .api()
            .reactions_add("C1", "1.0", "thumbsup")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn auth_test_via_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "user_id": "UBOT123"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let r = adapter.api().auth_test().await.unwrap();
        assert_eq!(r.user_id, "UBOT123");
    }

    #[tokio::test]
    async fn deliver_transport_error_on_non_200_non_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("503")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_to_url_failure_surfaces_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "1.0"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.getUploadURLExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "upload_url": format!("{}/bad-upload", server.uri()),
                "file_id": "F1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/bad-upload"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: vec![1, 2],
            }],
        };
        match adapter.deliver("C1", None, &msg).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plain_text_fallback_strips_blocks_for_slack() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({
                "text": "hello",
                "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "*hi*"}}]
            }),
            files: vec![],
        };
        let fallback = adapter
            .plain_text_fallback(&msg)
            .expect("slack fallback");
        // blocks must be removed entirely.
        assert!(fallback.content.get("blocks").is_none());
        // text is preserved and prepended with the reduced-formatting marker.
        assert_eq!(
            fallback.content["text"].as_str().unwrap(),
            "[reduced formatting] hello"
        );
        assert_eq!(fallback.kind, MessageKind::Chat);
    }

    #[tokio::test]
    async fn slack_edit_message_calls_chat_update() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .and(header("authorization", "Bearer xoxb-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "5.0"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        adapter
            .edit_message("C1", Some("100.000"), "100.000", "updated body")
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/chat.update"))
            .expect("chat.update request");
        let body: serde_json::Value =
            serde_json::from_slice(&req.body).expect("json body");
        assert_eq!(body["channel"], "C1");
        assert_eq!(body["ts"], "100.000");
        assert_eq!(body["text"], "updated body");
    }

    #[tokio::test]
    async fn slack_add_reaction_calls_reactions_add() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions.add"))
            .and(header("authorization", "Bearer xoxb-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        // Verify colon-stripping: the agent may emit `:thumbsup:` or
        // `thumbsup`; Slack expects the bare name.
        adapter
            .add_reaction("C1", None, "100.000", ":thumbsup:")
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/reactions.add"))
            .expect("reactions.add request");
        let body: serde_json::Value =
            serde_json::from_slice(&req.body).expect("json body");
        assert_eq!(body["channel"], "C1");
        assert_eq!(body["timestamp"], "100.000");
        assert_eq!(body["name"], "thumbsup");
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let s = format!("{adapter:?}");
        assert!(s.contains("SlackAdapter"));
        assert!(s.contains("slack"));
    }

    // -----------------------------------------------------------------
    // Team CHN audit additions: adapter-level edge cases.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn deliver_empty_text_still_posts_message() {
        // Slack happily posts an empty `text` (the call succeeds and
        // the user sees nothing). We don't want to silently drop it on
        // our side — confirm the API does get called.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "1.0"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        let id = adapter.deliver("C1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("1.0"));
    }

    #[tokio::test]
    async fn deliver_non_object_content_treats_as_empty_text() {
        // content as a JSON array — no `text` key reachable; the adapter
        // falls back to empty text and still hits the API. Verify it
        // doesn't panic on the unexpected shape.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "2.0"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!([1, 2, 3]),
            files: vec![],
        };
        let id = adapter.deliver("C1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("2.0"));
    }

    #[tokio::test]
    async fn deliver_card_posts_blocks_and_text_fallback_returns_ts() {
        use ironclaw_channels_core::{Card, CardButton, CardField};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "ts": "999.111"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Approve deploy?".into()),
            body: Some("Push the green build to prod-canary?".into()),
            fields: vec![CardField {
                label: "Branch".into(),
                value: "main".into(),
                inline: true,
            }],
            buttons: vec![CardButton {
                label: "Yes".into(),
                value: Some("deploy:yes".into()),
                url: None,
                style: Some("primary".into()),
            }],
            image_url: None,
        };
        let id = adapter
            .deliver_card("C123", Some("100.1"), &card, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("999.111"));

        // Inspect the outbound request body — we want to be sure the override
        // hit, not the default trait fallback which would have sent plain
        // `text` with no `blocks`.
        let reqs = server.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/chat.postMessage"))
            .expect("postMessage request");
        let body: serde_json::Value =
            serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["channel"], "C123");
        assert_eq!(body["thread_ts"], "100.1");
        // Block Kit payload shape verified more thoroughly in api.rs tests;
        // here we just check the call carries blocks at all.
        let blocks = body["blocks"].as_array().expect("blocks array");
        assert!(!blocks.is_empty());
        assert_eq!(blocks[0]["type"], "header");
        // Text fallback should be present so notifications stay readable.
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("Approve deploy?"));
    }

    #[tokio::test]
    async fn deliver_card_surfaces_rate_limited_error() {
        use ironclaw_channels_core::Card;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "9"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("hi".into()),
            ..Card::default()
        };
        match adapter.deliver_card("C1", None, &card, None).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(9)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_429_with_retry_after_surfaces_rate_to_adapter_caller() {
        // Sanity-check: even when the api maps the rate-limit, the
        // adapter passes the Rate variant straight through.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "11")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let err = adapter.deliver("C1", None, &text("hi")).await.unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(11)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    // ── Diff card rendering ────────────────────────────────────────

    #[test]
    fn build_diff_blocks_header_section_carries_path_and_totals() {
        let card = ironclaw_channels_core::DiffCard {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            hunks: vec![ironclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    ironclaw_channels_core::DiffLine {
                        kind: ironclaw_channels_core::DiffLineKind::Remove,
                        text: "let x = 1;".into(),
                    },
                    ironclaw_channels_core::DiffLine {
                        kind: ironclaw_channels_core::DiffLineKind::Add,
                        text: "let x = 2;".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let blocks = super::build_diff_blocks(&card);
        let arr = blocks.as_array().unwrap();
        assert_eq!(arr.len(), 2, "header + one hunk block");
        assert_eq!(arr[0]["type"], "section");
        let header = arr[0]["text"]["text"].as_str().unwrap();
        assert!(header.contains("*src/lib.rs*"));
        assert!(header.contains("(+1 / -1)"));
        // Hunk body sits in a rich_text_preformatted element.
        assert_eq!(arr[1]["type"], "rich_text");
        let body = arr[1]["elements"][0]["elements"][0]["text"]
            .as_str()
            .unwrap();
        assert!(body.contains("@@ -1,1 +1,1 @@"));
        assert!(body.contains("-let x = 1;"));
        assert!(body.contains("+let x = 2;"));
    }

    #[test]
    fn build_diff_blocks_truncated_card_marks_header() {
        let mut card = ironclaw_channels_core::DiffCard {
            path: "x.rs".into(),
            language: None,
            hunks: vec![],
            added: 5,
            removed: 3,
            truncated: true,
        };
        // Single empty hunk for the array shape; the test focuses on
        // the header layout.
        card.hunks.push(ironclaw_channels_core::DiffHunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 0,
            lines: vec![],
        });
        let blocks = super::build_diff_blocks(&card);
        let header = blocks.as_array().unwrap()[0]["text"]["text"]
            .as_str()
            .unwrap();
        assert!(header.contains("truncated"), "got: {header}");
    }

    #[tokio::test]
    async fn deliver_diff_posts_block_kit_message() {
        // The native diff renderer posts via chat.postMessage with the
        // Block Kit blocks built by `build_diff_blocks`, plus a
        // fallback text field for notification surfaces.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "ok": true,
                    "ts": "9999.0001",
                    "channel": "C1",
                })),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = ironclaw_channels_core::DiffCard {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            hunks: vec![ironclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![ironclaw_channels_core::DiffLine {
                    kind: ironclaw_channels_core::DiffLineKind::Add,
                    text: "fn main() {}".into(),
                }],
            }],
            added: 1,
            removed: 0,
            truncated: false,
        };
        let id = adapter.deliver_diff("C1", None, &card).await.unwrap();
        assert_eq!(id.as_deref(), Some("9999.0001"));
        let reqs = server.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/chat.postMessage"))
            .expect("post");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        // Block array present.
        let blocks = body["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "section");
        // Fallback text is the unified-diff body.
        let fallback = body["text"].as_str().unwrap();
        assert!(fallback.contains("--- a/src/main.rs"));
    }

    // ── Breadcrumb chip rendering ──────────────────────────────────

    #[test]
    fn build_breadcrumb_blocks_running_uses_ascii_marker_and_code_chip() {
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let blocks = super::build_breadcrumb_blocks(&bc);
        let arr = blocks.as_array().expect("blocks must be a JSON array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "context");
        let elements = arr[0]["elements"].as_array().unwrap();
        // Only one element now: the mrkdwn carrying the ASCII status
        // marker + tool name + detail. The previous emoji element was
        // dropped per the no-emoji rule.
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0]["type"], "mrkdwn");
        let txt = elements[0]["text"].as_str().unwrap();
        assert!(txt.starts_with("[~] `shell`"), "got: {txt}");
        assert!(txt.contains("cargo check"));
    }

    #[test]
    fn build_breadcrumb_blocks_done_uses_ascii_marker_and_summary() {
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let blocks = super::build_breadcrumb_blocks(&bc);
        let elements = blocks[0]["elements"].as_array().unwrap();
        let txt = elements[0]["text"].as_str().unwrap();
        assert!(txt.starts_with("[ok]"), "got: {txt}");
        assert!(txt.contains("passed (0.4s)"));
    }

    #[test]
    fn build_breadcrumb_blocks_failed_uses_ascii_marker_and_failed_prefix() {
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .finished(false, Some("timeout".into()));
        let blocks = super::build_breadcrumb_blocks(&bc);
        let elements = blocks[0]["elements"].as_array().unwrap();
        let txt = elements[0]["text"].as_str().unwrap();
        assert!(txt.starts_with("[x]"), "got: {txt}");
        assert!(txt.contains("failed: timeout"));
    }

    #[tokio::test]
    async fn deliver_breadcrumb_running_calls_post_message_with_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "channel": "C1",
                "ts": "1700000000.000001",
                "message": {}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let id = adapter
            .deliver_breadcrumb("C1", None, &bc, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("1700000000.000001"));
        let reqs = server.received_requests().await.unwrap();
        let last = reqs.last().unwrap();
        let body: Value = serde_json::from_slice(&last.body).unwrap();
        // Block Kit `context` block lands on `blocks` and the
        // text-fallback rendering lands on `text` for notification
        // surfaces.
        assert_eq!(body["channel"], "C1");
        let blocks = body["blocks"].as_array().expect("blocks array");
        assert_eq!(blocks[0]["type"], "context");
    }

    #[tokio::test]
    async fn deliver_breadcrumb_with_existing_id_calls_chat_update() {
        // existing_message_id = Some(..) → chat.update with the
        // finished chip's blocks, preserving the channel + ts.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "channel": "C1",
                "ts": "1700000000.000001",
                "message": {}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let id = adapter
            .deliver_breadcrumb("C1", None, &bc, Some("1700000000.000001"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("1700000000.000001"));
        let reqs = server.received_requests().await.unwrap();
        let last = reqs.last().unwrap();
        let body: Value = serde_json::from_slice(&last.body).unwrap();
        assert_eq!(body["channel"], "C1");
        assert_eq!(body["ts"], "1700000000.000001");
        let blocks = body["blocks"].as_array().expect("blocks array");
        assert_eq!(blocks[0]["type"], "context");
        let elements = blocks[0]["elements"].as_array().unwrap();
        let txt = elements[0]["text"].as_str().unwrap();
        assert!(txt.starts_with("[ok]"), "got: {txt}");
    }

    // ── Error card rendering ────────────────────────────────────────

    #[test]
    fn build_error_attachments_uses_danger_color_for_each_kind() {
        // All three current ErrorCardKinds render as "danger" — red
        // bar. Future Warning/RateLimit variants would map to other
        // Slack colours; until they exist, every host-emit site is
        // equally serious from the user's POV.
        for kind in [
            ironclaw_channels_core::ErrorCardKind::Internal,
            ironclaw_channels_core::ErrorCardKind::Provider,
            ironclaw_channels_core::ErrorCardKind::Delivery,
        ] {
            let card = ironclaw_channels_core::ErrorCard::new(kind, "boom");
            let atts = super::build_error_attachments(&card);
            let arr = atts.as_array().unwrap();
            assert_eq!(arr.len(), 1);
            assert_eq!(arr[0]["color"], "danger", "{kind:?} should be danger");
        }
    }

    #[test]
    fn build_error_attachments_includes_header_and_section_blocks() {
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Internal,
            "the shell tool timed out after 30s",
        )
        .with_title("Tool failed");
        let atts = super::build_error_attachments(&card);
        let blocks = atts[0]["blocks"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "header");
        assert_eq!(blocks[0]["text"]["text"], "Tool failed");
        assert_eq!(blocks[1]["type"], "section");
        let summary = blocks[1]["text"]["text"].as_str().unwrap();
        assert!(summary.contains("timed out"));
    }

    #[test]
    fn build_error_attachments_emits_preformatted_block_for_details() {
        // Details should land in a rich_text_preformatted block so
        // they render monospace on Slack's modern client.
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Provider,
            "model 502",
        )
        .with_details("upstream: anthropic\nstatus: 502");
        let atts = super::build_error_attachments(&card);
        let blocks = atts[0]["blocks"].as_array().unwrap();
        let preformatted = blocks
            .iter()
            .find(|b| b["type"] == "rich_text")
            .expect("rich_text block missing for details");
        assert_eq!(
            preformatted["elements"][0]["type"],
            "rich_text_preformatted"
        );
    }

    #[test]
    fn build_error_attachments_retryable_adds_footer() {
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Delivery,
            "telegram 502",
        )
        .retryable();
        let atts = super::build_error_attachments(&card);
        assert_eq!(atts[0]["footer"], "will retry automatically");
    }

    #[test]
    fn build_error_attachments_omits_footer_when_not_retryable() {
        // Terminal failures must NOT carry "will retry automatically"
        // — promising a retry we already gave up on is worse than no
        // footer.
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Internal,
            "fatal",
        );
        let atts = super::build_error_attachments(&card);
        assert!(atts[0].get("footer").is_none());
    }

    #[tokio::test]
    async fn deliver_error_posts_attachments_with_danger_color() {
        // End-to-end: deliver_error must POST to /chat.postMessage
        // with a populated `attachments` array (and `text` for
        // notification fallback).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "channel": "C1",
                "ts": "1700000000.999999",
                "message": {}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Provider,
            "anthropic returned 502 after retry exhaustion",
        )
        .with_title("Provider failed");
        let id = adapter
            .deliver_error("C1", None, &card)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("1700000000.999999"));
        let reqs = server.received_requests().await.unwrap();
        let last = reqs.last().unwrap();
        let body: Value = serde_json::from_slice(&last.body).unwrap();
        assert_eq!(body["channel"], "C1");
        let atts = body["attachments"].as_array().expect("attachments array");
        assert_eq!(atts[0]["color"], "danger");
        // The text fallback must still be present so Slack's
        // notification preview is meaningful.
        let fallback = body["text"].as_str().unwrap();
        assert!(fallback.starts_with("[ERROR: provider]"));
    }

    // ── Long-output expander (slice 3.4) ──────────────────────────

    #[test]
    fn build_collapsible_blocks_includes_summary_and_full_body() {
        // Layout contract: summary on top in a `section` mrkdwn,
        // preview lines (when present) in their own
        // `rich_text_preformatted`, full body in a second
        // `rich_text_preformatted` — Slack natively collapses the
        // oversized preformatted block behind "Show more".
        let body = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let preview: Vec<String> = (0..4).map(|i| format!("line {i}")).collect();
        let blocks = super::build_collapsible_blocks(&body, "shell 30 lines", &preview);
        let arr = blocks.as_array().expect("blocks must be an array");
        assert_eq!(arr.len(), 3, "summary + preview + full body");
        assert_eq!(arr[0]["type"], "section");
        assert_eq!(
            arr[0]["text"]["text"].as_str().unwrap(),
            "shell 30 lines",
        );
        assert_eq!(arr[1]["type"], "rich_text");
        assert_eq!(arr[1]["elements"][0]["type"], "rich_text_preformatted");
        assert_eq!(arr[2]["type"], "rich_text");
        let full = arr[2]["elements"][0]["elements"][0]["text"].as_str().unwrap();
        assert_eq!(full, body);
    }

    #[test]
    fn build_collapsible_blocks_omits_preview_when_empty() {
        // When the runner doesn't extract a preview (e.g.
        // single-line-but-huge body) we skip the preview block to
        // avoid an empty/duplicate preformatted element.
        let blocks = super::build_collapsible_blocks("body", "summary", &[]);
        let arr = blocks.as_array().unwrap();
        assert_eq!(arr.len(), 2, "summary + full body, no preview");
        assert_eq!(arr[0]["type"], "section");
        assert_eq!(arr[1]["type"], "rich_text");
    }

    #[tokio::test]
    async fn deliver_collapsible_posts_via_chat_post_message_with_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "channel": "C1",
                "ts": "1700000000.999999",
                "message": {}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let body = (0..40).map(|i| format!("L{i}")).collect::<Vec<_>>().join("\n");
        let preview: Vec<String> = (0..3).map(|i| format!("L{i}")).collect();
        let id = adapter
            .deliver_collapsible("C1", None, &body, "shell 40 lines", &preview)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("1700000000.999999"));
        let reqs = server.received_requests().await.unwrap();
        let last = reqs.last().unwrap();
        let post: Value = serde_json::from_slice(&last.body).unwrap();
        // Notification text is the summary alone — the full body
        // doesn't leak into push notifications.
        assert_eq!(post["text"], "shell 40 lines");
        let blocks = post["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 3);
    }

    // ── Thinking block (slice 3.5) rendering ────────────────────────

    #[test]
    fn build_thinking_blocks_uses_context_with_thought_balloon_and_reasoning_label() {
        // Slack's `context` block is the platform's idiomatic muted
        // metadata affordance — same primitive the breadcrumb chip
        // uses. Header element pair is `:thought_balloon:` emoji +
        // italicised `reasoning` mrkdwn.
        let t = ThinkingBlock::visible("Let me work through the question.");
        let blocks = super::build_thinking_blocks(&t);
        let arr = blocks.as_array().expect("array");
        assert!(arr.len() >= 2, "got: {blocks}");
        let header = &arr[0];
        assert_eq!(header["type"], "context");
        let elems = header["elements"].as_array().unwrap();
        assert_eq!(elems[0]["type"], "emoji");
        assert_eq!(elems[0]["name"], "thought_balloon");
        let label = elems[1]["text"].as_str().unwrap();
        assert!(label.contains("reasoning"));
        // Body block carries the reasoning text.
        let body = &arr[1];
        assert_eq!(body["type"], "context");
        let body_text = body["elements"][0]["text"].as_str().unwrap();
        assert!(body_text.contains("Let me work through the question."));
    }

    #[test]
    fn build_thinking_blocks_includes_provenance_in_label() {
        let t = ThinkingBlock::visible("ok").with_model("claude-opus-4-7");
        let blocks = super::build_thinking_blocks(&t);
        let header = &blocks[0];
        let label = header["elements"][1]["text"].as_str().unwrap();
        assert!(label.contains("claude-opus-4-7"), "got: {label}");
    }

    #[test]
    fn build_thinking_blocks_redacted_emits_placeholder_only() {
        // Privacy contract: redacted blocks must never put the raw
        // blob on the wire — neither via the canonical fallback text
        // nor through the structured blocks.
        let t = ThinkingBlock::redacted("opaque-secret-blob");
        let blocks = super::build_thinking_blocks(&t);
        let raw = serde_json::to_string(&blocks).unwrap();
        assert!(
            !raw.contains("opaque-secret-blob"),
            "raw redacted blob leaked: {raw}"
        );
        assert!(raw.contains("redacted reasoning"), "got: {raw}");
    }

    #[test]
    fn build_thinking_blocks_splits_long_text_across_blocks() {
        // Slack caps mrkdwn elements at 3000 chars. A 7500-char block
        // must be spilled across multiple `context` blocks rather than
        // truncated or rejected.
        let big = "x".repeat(7500);
        let t = ThinkingBlock::visible(big);
        let blocks = super::build_thinking_blocks(&t);
        let arr = blocks.as_array().unwrap();
        // 1 header + ceil(7500/3000) = 3 body chunks → 4 blocks.
        assert_eq!(arr.len(), 4, "got: {blocks}");
        for body in arr.iter().skip(1) {
            let text = body["elements"][0]["text"].as_str().unwrap();
            assert!(
                text.chars().count() <= super::MAX_MRKDWN_ELEMENT_CHARS,
                "chunk over cap: {} chars",
                text.chars().count()
            );
        }
    }

    #[tokio::test]
    async fn deliver_thinking_calls_post_message_with_context_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "ts": "1700000000.111111",
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let t = ThinkingBlock::visible("a chain of thought").with_model("claude-opus-4-7");
        let id = adapter.deliver_thinking("C1", None, &t).await.unwrap();
        assert_eq!(id.as_deref(), Some("1700000000.111111"));
        let reqs = server.received_requests().await.unwrap();
        let last = reqs.last().unwrap();
        let post: Value = serde_json::from_slice(&last.body).unwrap();
        // The notification-text fallback is the canonical text rendering.
        let text = post["text"].as_str().unwrap();
        assert!(text.starts_with("[reasoning: claude-opus-4-7]"), "got: {text}");
        let blocks = post["blocks"].as_array().expect("blocks");
        assert_eq!(blocks[0]["type"], "context");
        assert_eq!(blocks[0]["elements"][0]["name"], "thought_balloon");
    }

    // ── TodoList chip rendering ────────────────────────────────────

    fn slack_todo_list_sample() -> TodoList {
        TodoList {
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "Wash dishes".into(),
                    status: TodoItemStatus::Completed,
                },
                TodoListItem {
                    id: 2,
                    text: "Dry dishes".into(),
                    status: TodoItemStatus::InProgress,
                },
            ],
            title: Some("Kitchen".into()),
        }
    }

    #[test]
    fn build_todo_list_blocks_has_header_and_per_item_sections() {
        let blocks = super::build_todo_list_blocks(&slack_todo_list_sample());
        let arr = blocks.as_array().expect("array");
        assert_eq!(arr[0]["type"], "header");
        let header_text = arr[0]["text"]["text"].as_str().unwrap();
        assert!(header_text.contains("Kitchen"));
        assert!(header_text.contains("(1/2)"));
        assert_eq!(arr[1]["type"], "section");
        let done_text = arr[1]["text"]["text"].as_str().unwrap();
        assert!(done_text.contains("[x]"));
        assert!(done_text.contains("~Wash dishes~"));
        let prog_text = arr[2]["text"]["text"].as_str().unwrap();
        assert!(prog_text.contains("[~]"));
        assert!(prog_text.contains("Dry dishes"));
    }

    #[tokio::test]
    async fn deliver_todo_list_first_emit_posts_and_pins() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "ts": "1700000000.000001"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/pins.add"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver_todo_list("C1", None, &slack_todo_list_sample(), None, true)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("1700000000.000001"));
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.iter().any(|r| r.url.path().ends_with("/pins.add")));
    }

    #[tokio::test]
    async fn deliver_todo_list_with_existing_id_edits_in_place_no_pin() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "ts": "1700000000.000001"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver_todo_list(
                "C1",
                None,
                &slack_todo_list_sample(),
                Some("1700000000.000001"),
                false,
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("1700000000.000001"));
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.iter().any(|r| r.url.path().ends_with("/chat.update")));
        assert!(!reqs.iter().any(|r| r.url.path().ends_with("/pins.add")));
        assert!(!reqs.iter().any(|r| r.url.path().ends_with("/chat.postMessage")));
    }
}
