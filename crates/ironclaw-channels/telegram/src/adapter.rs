//! [`TelegramAdapter`] — the [`ChannelAdapter`] implementation.
//!
//! Owns the [`TelegramApi`] client, the cancellation token used to stop
//! background tasks, and the resolved [`TelegramConfig`].

use crate::api::{InlineKeyboardButton, TelegramApi, escape_markdown_v2};
use crate::config::{IngressMode, TelegramConfig};
use crate::ingress::{IngressSettings, long_poll, webhook};
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, Breadcrumb, BreadcrumbStatus, Card, ChannelAdapter, DiffCard, DmHandle,
    ErrorCard, ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
};
use ironclaw_types::{ChannelType, InboundEvent, OutboundMessage};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// `ChannelType` string used by this channel.
pub const CHANNEL_TYPE_STR: &str = "telegram";

/// `parse_mode` used by `sendMessage` when none is supplied in the outbound
/// payload. We default to `HTML` and pre-convert a small subset of common
/// markdown syntax (`**bold**`, `*italic*` / `_italic_`, `` `code` ``,
/// ``` ```block``` ```) to HTML tags in [`markdown_to_html`] before send.
/// All other text is HTML-escaped so unbalanced angle brackets / ampersands
/// in agent prose don't break the parse.
///
/// Why HTML rather than `MarkdownV2`: `MarkdownV2` reserves `!`, `.`, `-`,
/// `(`, `)`, `[`, `]`, `=`, `|`, `{`, `}`, `~`, `>`, `#`, `+`, `_`, `*`
/// — every one of those needs backslash escaping or Telegram returns
/// HTTP 400 ("can't parse entities"). Natural-language agent prose
/// contains plenty of them. HTML only reserves `<`, `>`, `&` — three
/// characters we can deterministically escape inside the adapter.
///
/// Agents that want a different mode (or already-formatted text) can
/// still override by setting `content.parse_mode = "MarkdownV2"` /
/// `"Markdown"` / `"HTML"` / `""` (empty = plain text) on the outbound
/// row.
pub const DEFAULT_PARSE_MODE: &str = "HTML";

/// Telegram channel adapter.
pub struct TelegramAdapter {
    channel_type: ChannelType,
    api: TelegramApi,
    cancel: CancellationToken,
    /// Resolved bot username from `getMe`, used for mention detection.
    bot_username: Option<String>,
    /// Stored config (kept for `Debug` and potential introspection).
    config: TelegramConfig,
    /// Per-channel data directory. Inbound attachments are written under
    /// `<data_dir>/inbox/<msg_id>/<filename>`.
    data_dir: PathBuf,
    /// Background task handle (long-poll or webhook driver).
    ingress_handle: Mutex<Option<JoinHandle<()>>>,
    /// Optional bound webhook address (None for long-poll mode).
    webhook_addr: Option<SocketAddr>,
}

impl TelegramAdapter {
    /// Build an adapter, starting the configured ingress mode.
    pub async fn start(
        config: TelegramConfig,
        bot_username: Option<String>,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let api = TelegramApi::new(config.api_base.clone(), config.bot_token.clone());
        Self::start_with_api(api, config, bot_username, inbound_tx, data_dir).await
    }

    /// As [`start`] but with a caller-supplied [`TelegramApi`] (used by
    /// tests that share an HTTP client and a wiremock instance).
    pub async fn start_with_api(
        api: TelegramApi,
        config: TelegramConfig,
        bot_username: Option<String>,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let cancel = CancellationToken::new();
        let settings = IngressSettings {
            attachment_download: config.attachment_download,
            max_attachment_bytes: config.max_attachment_bytes,
            bot_username: bot_username.clone(),
            data_dir: data_dir.clone(),
        };
        let (handle, webhook_addr) = match config.mode.clone() {
            IngressMode::LongPoll(lp) => {
                let join = tokio::spawn(long_poll::run_long_poll(
                    api.clone(),
                    lp,
                    settings,
                    inbound_tx,
                    cancel.clone(),
                ));
                (Some(join), None)
            }
            IngressMode::Webhook(wh) => {
                let addr = webhook::spawn_server(
                    wh,
                    inbound_tx,
                    api.clone(),
                    settings,
                    cancel.clone(),
                )
                .await
                .map_err(|e| AdapterError::Transport(format!("telegram webhook bind: {e}")))?;
                (None, Some(addr))
            }
        };

        Ok(Arc::new(Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            api,
            cancel,
            bot_username,
            config,
            data_dir,
            ingress_handle: Mutex::new(handle),
            webhook_addr,
        }))
    }

    /// Per-channel data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Resolved bot username (when known from `getMe`).
    pub fn bot_username(&self) -> Option<&str> {
        self.bot_username.as_deref()
    }

    /// Bound webhook address when running in webhook mode.
    pub fn webhook_addr(&self) -> Option<SocketAddr> {
        self.webhook_addr
    }

    /// Resolved [`TelegramConfig`].
    pub fn config(&self) -> &TelegramConfig {
        &self.config
    }

    /// Stop background tasks and wait for the ingress handle (long-poll).
    /// Idempotent: subsequent calls are no-ops.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.ingress_handle.lock().await.take() {
            // Best-effort: ignore JoinError so shutdown is idempotent.
            let _ = handle.await;
        }
    }
}

impl std::fmt::Debug for TelegramAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramAdapter")
            .field("channel_type", &self.channel_type)
            .field("bot_username", &self.bot_username)
            .field("webhook_addr", &self.webhook_addr)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChannelAdapter for TelegramAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    /// Telegram `sendMessage` rejects bodies over 4096 chars. Lets the
    /// host's delivery splitter break long agent replies into a sequence
    /// of messages before they hit the API.
    fn max_message_chars(&self) -> Option<usize> {
        Some(4096)
    }

    async fn subscribe(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // Telegram bots are auto-subscribed to chats they are members of;
        // we only validate that the bot token is still good.
        let _ = self.api.get_me().await?;
        Ok(())
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        self.api.send_chat_action(platform_id, thread_id, "typing").await
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let text = extract_text(&message.content);
        let parse_mode = extract_parse_mode(&message.content);

        // Files first (with the first file carrying the caption, if any),
        // then the text-only message if no files were sent and text exists.
        if !message.files.is_empty() {
            let mut last_id: Option<i64> = None;
            for (i, file) in message.files.iter().enumerate() {
                let caption = if i == 0 && !text.is_empty() {
                    Some(text.as_str())
                } else {
                    None
                };
                let m = self
                    .api
                    .send_document(
                        platform_id,
                        thread_id,
                        &file.filename,
                        file.data.clone(),
                        caption,
                    )
                    .await?;
                last_id = Some(m.message_id);
            }
            return Ok(last_id.map(|id| id.to_string()));
        }

        if text.is_empty() {
            return Err(AdapterError::BadRequest(
                "telegram deliver requires `text` or files".into(),
            ));
        }
        // Use the agent-supplied parse_mode if any; otherwise fall back to
        // DEFAULT_PARSE_MODE. Empty string means "send as plain text" so
        // we omit the field entirely from the API call rather than passing
        // an empty `parse_mode` that Telegram would reject.
        let effective_mode = parse_mode
            .as_deref()
            .unwrap_or(DEFAULT_PARSE_MODE);
        let mode_for_api = if effective_mode.is_empty() {
            None
        } else {
            Some(effective_mode)
        };
        // When the agent didn't pre-format (no explicit `parse_mode`)
        // AND the default is HTML, run a small markdown→HTML pass so
        // `**bold**` / `*italic*` / `` `code` `` etc. in agent prose
        // render properly. The pass HTML-escapes everything else so
        // unbalanced `<` / `>` / `&` don't break the parse. Agents that
        // supplied their own `parse_mode` get their text passed verbatim.
        let body: String = if parse_mode.is_none() && effective_mode == "HTML" {
            markdown_to_html(&text)
        } else {
            text.clone()
        };
        let m = self
            .api
            .send_message(platform_id, thread_id, &body, mode_for_api)
            .await?;
        Ok(Some(m.message_id.to_string()))
    }

    async fn edit_message(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        external_id: &str,
        new_text: &str,
    ) -> Result<(), AdapterError> {
        // Telegram's `editMessageText` is addressed by (chat_id, message_id);
        // thread context is implicit in the message_id so we discard it.
        self.api
            .edit_message_text(platform_id, external_id, new_text)
            .await
    }

    /// Render a [`Card`] natively as a MarkdownV2 message with an
    /// `inline_keyboard` reply_markup. The card's `image_url` is attached
    /// via `sendPhoto`; if the photo caption would exceed Telegram's
    /// `MAX_PHOTO_CAPTION_CHARS` budget, the body is sent as a follow-up
    /// `sendMessage` (still carrying the keyboard) — see
    /// [`MAX_PHOTO_CAPTION_CHARS`].
    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        _to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let body = render_card_markdown_v2(card);
        let keyboard = build_inline_keyboard(card);
        let parse_mode = Some("MarkdownV2");

        match card.image_url.as_deref() {
            Some(image_url) => {
                let caption_fits = body.chars().count() <= MAX_PHOTO_CAPTION_CHARS;
                if caption_fits {
                    // Photo + caption + keyboard all in one message.
                    let caption = if body.is_empty() { None } else { Some(body.as_str()) };
                    let keyboard_ref = if keyboard.is_empty() {
                        None
                    } else {
                        Some(keyboard.as_slice())
                    };
                    let m = self
                        .api
                        .send_photo_with_caption_and_keyboard(
                            platform_id,
                            thread_id,
                            image_url,
                            caption,
                            if caption.is_some() { parse_mode } else { None },
                            keyboard_ref,
                        )
                        .await?;
                    Ok(Some(m.message_id.to_string()))
                } else {
                    // Caption too long: send a truncated caption with the
                    // photo, then a follow-up message carrying the full
                    // body and the keyboard.
                    let truncated = truncate_for_photo_caption(&body);
                    let truncated_for_send = if truncated.is_empty() {
                        None
                    } else {
                        Some(truncated.as_str())
                    };
                    let photo = self
                        .api
                        .send_photo_with_caption_and_keyboard(
                            platform_id,
                            thread_id,
                            image_url,
                            truncated_for_send,
                            if truncated_for_send.is_some() {
                                parse_mode
                            } else {
                                None
                            },
                            None,
                        )
                        .await?;
                    let _follow = self
                        .api
                        .send_message_with_inline_keyboard(
                            platform_id,
                            thread_id,
                            &body,
                            parse_mode,
                            &keyboard,
                        )
                        .await?;
                    // Return the photo's id — it's the first message in
                    // the pair the user sees and the most useful anchor
                    // for downstream replies/edits.
                    Ok(Some(photo.message_id.to_string()))
                }
            }
            None => {
                // Text-only card. If there are buttons, use the
                // inline-keyboard sender; otherwise fall through to plain
                // `sendMessage` so we don't put an empty `reply_markup`
                // on the wire.
                if keyboard.is_empty() {
                    let m = self
                        .api
                        .send_message(platform_id, thread_id, &body, parse_mode)
                        .await?;
                    Ok(Some(m.message_id.to_string()))
                } else {
                    let m = self
                        .api
                        .send_message_with_inline_keyboard(
                            platform_id,
                            thread_id,
                            &body,
                            parse_mode,
                            &keyboard,
                        )
                        .await?;
                    Ok(Some(m.message_id.to_string()))
                }
            }
        }
    }

    async fn add_reaction(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        external_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        self.api
            .set_message_reaction(platform_id, external_id, emoji)
            .await
    }

    /// Native breadcrumb chip — renders the tool name as
    /// `<code>…</code>` (Telegram HTML mode) so the chat client
    /// shows a monospace inline-code badge rather than a regular
    /// chat line. When `existing_message_id` is provided we drive
    /// `editMessageText` to update the original chip in place;
    /// otherwise we emit a fresh `sendMessage`.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let body = render_breadcrumb_html(breadcrumb);
        let parse_mode = Some("HTML");
        if let Some(existing) = existing_message_id {
            // editMessageText doesn't propagate thread_id — Telegram
            // resolves the thread from the original message_id.
            self.api
                .edit_message_text_with_mode(platform_id, existing, &body, parse_mode)
                .await?;
            return Ok(Some(existing.to_owned()));
        }
        let m = self
            .api
            .send_message(platform_id, thread_id, &body, parse_mode)
            .await?;
        Ok(Some(m.message_id.to_string()))
    }

    /// Native diff card — wraps the unified-diff body in a
    /// MarkdownV2 ` ```diff … ``` ` fenced block. Mobile Telegram
    /// clients colourise `diff` syntax inside the fence so the user
    /// sees `+`/`-` gutters and per-line highlighting; desktop
    /// clients render the same fenced block in monospace.
    ///
    /// MarkdownV2 inside a fenced code block is special: backticks
    /// and backslashes are the only chars that need escaping (we
    /// guard against backticks by replacing them with a similar
    /// glyph; the diff body is unlikely to contain backticks but
    /// untrusted source code might). Bodies above MarkdownV2's
    /// per-message budget fall back to plain HTML `<pre>` via the
    /// trait-default if `sendMessage` rejects the fenced shape.
    async fn deliver_diff(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        let body = render_diff_markdown_v2(diff);
        let m = self
            .api
            .send_message(platform_id, thread_id, &body, Some("MarkdownV2"))
            .await?;
        Ok(Some(m.message_id.to_string()))
    }

    /// Native long-output expander (slice 3.4) — wraps the full body
    /// in a Telegram HTML `<blockquote expandable>` (Bot API 7.6+,
    /// available on every officially supported client as of 2024-Q4).
    /// The summary rides outside the blockquote as the visible first
    /// line so the user can read it without expanding. Clients that
    /// don't recognise `expandable` fall back to a fully-rendered
    /// blockquote — still readable, just not collapsed; graceful.
    ///
    /// HTML chosen over MarkdownV2 because MarkdownV2 reserves so
    /// many characters that escaping arbitrary tool output (shell
    /// stdout, source code, log lines) would dominate the body. HTML
    /// only requires `<`, `>`, `&` escaping.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        text: &str,
        summary: &str,
        _preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        let body = render_collapsible_html(text, summary);
        let parse_mode = Some("HTML");
        let m = self
            .api
            .send_message(platform_id, thread_id, &body, parse_mode)
            .await?;
        Ok(Some(m.message_id.to_string()))
    }

    /// Native todo-list checklist — rendered as MarkdownV2 with a
    /// bold title, one line per item carrying a status glyph
    /// (`☑` / `▶` / `☐`) and the item text, plus a small footer
    /// showing `done/total`. On first emit we `sendMessage` and then
    /// `pinChatMessage` so the chip stays visible above the timeline;
    /// on subsequent mutations we `editMessageText` so the user sees
    /// the same chip ticking through. When every item is completed
    /// we unpin the chip so the chat header isn't permanently
    /// occupied by a finished plan.
    ///
    /// Pin / unpin failures are swallowed and logged at `debug` —
    /// the chip is the load-bearing UX, pinning is decoration.
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        list: &TodoList,
        existing_message_id: Option<&str>,
        pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        let body = render_todo_list_markdown(list);
        let parse_mode = Some("MarkdownV2");
        let message_id = if let Some(existing) = existing_message_id {
            // editMessageText doesn't propagate thread_id — Telegram
            // resolves it from the original message_id.
            self.api
                .edit_message_text_with_mode(platform_id, existing, &body, parse_mode)
                .await?;
            existing.to_owned()
        } else {
            let m = self
                .api
                .send_message(platform_id, thread_id, &body, parse_mode)
                .await?;
            let new_id = m.message_id.to_string();
            // First emit: pin the chip best-effort.
            if pin_hint {
                if let Err(err) = self.api.pin_chat_message(platform_id, &new_id).await {
                    tracing::debug!(
                        ?err,
                        message_id = %new_id,
                        "telegram pin_chat_message failed (ignored)"
                    );
                }
            }
            new_id
        };
        // Unpin when fully completed (pin_hint is also set on this
        // transition by the host's dispatch_todo_list).
        if pin_hint && list.is_fully_completed() && existing_message_id.is_some() {
            if let Err(err) = self
                .api
                .unpin_chat_message(platform_id, &message_id)
                .await
            {
                tracing::debug!(
                    ?err,
                    message_id = %message_id,
                    "telegram unpin_chat_message failed (ignored)"
                );
            }
        }
        Ok(Some(message_id))
    }

    /// Native error card — rendered as Telegram HTML with a bold
    /// `<b>Error:</b>` prefix (Telegram has no color affordance so
    /// weight + monospace details + the `[error]` text prefix from
    /// the canonical card carry the severity signal) and the details
    /// block (when present) inside a `<pre>` monospace fence so
    /// stderr / tracebacks render legibly on mobile.
    ///
    /// No `existing_message_id` argument here — error receipts are
    /// immutable.
    async fn deliver_error(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        let body = render_error_html(err);
        let parse_mode = Some("HTML");
        let m = self
            .api
            .send_message(platform_id, thread_id, &body, parse_mode)
            .await?;
        Ok(Some(m.message_id.to_string()))
    }

    /// Native thinking block (slice 3.5) — wraps the reasoning text in
    /// a Telegram HTML `<blockquote expandable>` (Bot API 7.6+, same
    /// primitive surface 4 uses for long-output collapsibles). A small
    /// `<i>reasoning</i>` italic prefix rides outside the blockquote
    /// so the user can tell the block apart from the agent's reply
    /// without expanding. Older clients that ignore `expandable` see
    /// the fully-rendered blockquote — readable, just not collapsed.
    /// Redacted blocks render the placeholder line via
    /// `to_text_fallback` so the raw blob never reaches the wire.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        let body = render_thinking_html(thinking);
        let parse_mode = Some("HTML");
        let m = self
            .api
            .send_message(platform_id, thread_id, &body, parse_mode)
            .await?;
        Ok(Some(m.message_id.to_string()))
    }

    async fn open_dm(&self, user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Telegram requires the user to message the bot first. Once that's
        // happened the platform_id of the DM equals the user id, so we can
        // surface the handle without a round-trip.
        Ok(Some(DmHandle {
            user_id: user_id.to_owned(),
            platform_id: user_id.to_owned(),
            channel_type: self.channel_type.clone(),
        }))
    }

    /// Telegram-specific plain-text fallback used by the delivery loop when
    /// `sendMessage` rejected the original payload with a "can't parse
    /// entities" error (or similar formatting validation). Removes any
    /// `parse_mode` field — the adapter then sends the text without
    /// MarkdownV2 / HTML / Markdown escaping requirements — and prepends
    /// `"[reduced formatting] "` so the recipient knows the message arrived
    /// in a downgraded shape. Returns `None` if there is no `text` AND no
    /// `parse_mode` to strip (nothing to fall back to).
    fn plain_text_fallback(&self, msg: &OutboundMessage) -> Option<OutboundMessage> {
        let obj = msg.content.as_object()?;
        let text = obj
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let has_parse_mode = obj.contains_key("parse_mode");
        if !has_parse_mode {
            return None;
        }
        let mut new_obj = obj.clone();
        new_obj.remove("parse_mode");
        new_obj.insert(
            "text".to_owned(),
            serde_json::Value::String(format!("[reduced formatting] {text}")),
        );
        Some(OutboundMessage {
            kind: msg.kind,
            content: serde_json::Value::Object(new_obj),
            files: msg.files.clone(),
        })
    }
}

/// Maximum characters Telegram accepts in a `sendPhoto` caption.
/// Spec is 1024; we leave headroom for the "see message below" suffix
/// we append when the body is too long for a single caption.
pub const MAX_PHOTO_CAPTION_CHARS: usize = 900;

/// Maximum inline-keyboard buttons per row before we wrap to a new row.
/// Telegram allows up to 8 per row, but readability on phones drops fast
/// past 3 — see the canonical-card schema notes at
/// [`ironclaw_channels_core::Card`].
pub const MAX_BUTTONS_PER_ROW: usize = 3;

/// Suffix appended to a truncated photo caption when the full body is
/// being sent as a separate follow-up message.
const CAPTION_OVERFLOW_SUFFIX: &str = "(see message below)";

/// Render a [`Card`] into the `MarkdownV2` body Telegram expects from
/// `sendMessage(parse_mode="MarkdownV2")`. The shape is:
///
/// ```text
/// *Title*
///
/// Body text
///
/// *Label:* value
/// *Other:* value
/// ```
///
/// Every user-supplied substring is run through
/// [`escape_markdown_v2`] so reserved punctuation cannot break the
/// parser. Markdown markers we add ourselves (`*` for bold) are emitted
/// outside the escaped segments so they remain functional.
fn render_card_markdown_v2(card: &Card) -> String {
    let mut out = String::new();

    if let Some(title) = card.title.as_deref() {
        let t = title.trim();
        if !t.is_empty() {
            out.push('*');
            out.push_str(&escape_markdown_v2(t));
            out.push('*');
            out.push('\n');
        }
    }

    if let Some(body) = card.body.as_deref() {
        let b = body.trim();
        if !b.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&escape_markdown_v2(b));
            out.push('\n');
        }
    }

    if !card.fields.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        for f in &card.fields {
            out.push('*');
            out.push_str(&escape_markdown_v2(&f.label));
            // The colon is reserved-adjacent but `:` itself is not in
            // the MarkdownV2 reserved set; we still escape the label
            // contents to be safe.
            out.push_str(":*");
            out.push(' ');
            out.push_str(&escape_markdown_v2(&f.value));
            out.push('\n');
        }
    }

    // Trim trailing newline so the renderer composes cleanly with a
    // photo caption (where trailing whitespace is ugly).
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Render a [`Breadcrumb`] as a compact Telegram HTML chip. Format:
///
/// - Running: `[~] <code>shell</code> · cargo check`
/// - Done:    `[ok] <code>shell</code> · cargo check — passed (0.4s)`
/// - Failed:  `[x] <code>shell</code> · cargo check — failed: timeout`
///
/// Tool name is wrapped in `<code>` so Telegram renders it as a
/// monospace badge (the "chip" affordance). Detail / summary strings
/// are HTML-escaped — they're free-form text from the agent's tool
/// inputs so we cannot trust them to be HTML-clean.
/// Render a [`DiffCard`] as a Telegram `MarkdownV2` message wrapping
/// the unified-diff body in a ` ```diff … ``` ` fenced block. Mobile
/// clients colourise `diff`-tagged fences, so the user sees `+`/`-`
/// gutter colouring natively.
///
/// `MarkdownV2` escaping rules inside a fenced code block: only `` ` ``
/// and `\` need escaping. Untrusted source code rarely contains
/// either, but we still replace bare backticks with a similar glyph
/// to avoid corrupting the fence — losing a backtick in a diff is
/// far less surprising than the message failing to send.
pub(crate) fn render_diff_markdown_v2(diff: &DiffCard) -> String {
    // Header: path + totals. Path goes outside the fence (so it can
    // bold-link if we ever add that), totals after so the user has a
    // one-glance "+N / -M" summary.
    let mut out = String::with_capacity(128 + diff.hunks.len() * 64);
    out.push('*');
    out.push_str(&escape_markdown_v2(&diff.path));
    out.push_str("*  \\(");
    out.push_str(&format!("\\+{} / \\-{}", diff.added, diff.removed));
    if diff.truncated {
        out.push_str(" · truncated");
    }
    out.push_str("\\)\n");
    out.push_str("```diff\n");
    for h in &diff.hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        ));
        for line in &h.lines {
            out.push(line.kind.unified_prefix());
            // Inside a fenced block MarkdownV2 only treats `` ` `` and
            // `\` specially. Replace backticks with a similar glyph
            // (U+2032 PRIME) so the closing fence is unambiguous.
            for c in line.text.chars() {
                match c {
                    '`' => out.push('\u{2032}'),
                    '\\' => {
                        out.push('\\');
                        out.push('\\');
                    }
                    _ => out.push(c),
                }
            }
            out.push('\n');
        }
    }
    out.push_str("```");
    out
}

pub(crate) fn render_breadcrumb_html(b: &Breadcrumb) -> String {
    // ASCII-only status markers per the project's no-emoji rule.
    // Even non-emoji Unicode symbols (U+23F3 hourglass, U+2713 check,
    // U+2717 cross) get rendered as colourful emoji on iOS Telegram —
    // user-visible emoji either way. Plain ASCII squarely sidesteps that.
    let glyph = match b.status {
        BreadcrumbStatus::Running => "[~]",
        BreadcrumbStatus::Done => "[ok]",
        BreadcrumbStatus::Failed => "[x]",
    };
    let mut out = String::with_capacity(64);
    out.push_str(glyph);
    out.push(' ');
    out.push_str("<code>");
    out.push_str(&escape_html(&b.tool_name));
    out.push_str("</code>");
    if let Some(d) = b.detail.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            out.push_str(" · ");
            out.push_str(&escape_html(d));
        }
    }
    if let Some(s) = b.summary.as_deref() {
        let s = s.trim();
        if !s.is_empty() {
            if b.status == BreadcrumbStatus::Failed {
                out.push_str(" — failed: ");
            } else {
                out.push_str(" — ");
            }
            out.push_str(&escape_html(s));
        }
    }
    out
}

/// Render an [`ErrorCard`] as Telegram HTML. Format:
///
/// ```text
/// <b>Error:</b> <b>Title</b>
/// summary line wrapped
/// <pre>details monospace block (when present)</pre>
/// <i>will retry automatically</i>   (when retryable)
/// ```
///
/// Telegram has no native color affordance, so weight + monospace
/// carry the severity signal. The `Error:` prefix label varies by
/// `ErrorCardKind` (Internal → `Tool error`, Provider → `Provider
/// error`, Delivery → `Delivery error`) so the user sees which
/// stage of the pipeline gave up.
pub(crate) fn render_error_html(err: &ErrorCard) -> String {
    let label = match err.kind {
        ErrorCardKind::Internal => "Tool error",
        ErrorCardKind::Provider => "Provider error",
        ErrorCardKind::Delivery => "Delivery error",
    };
    let mut out = String::with_capacity(128 + err.summary.len());
    out.push_str("<b>");
    out.push_str(label);
    out.push_str(":</b> <b>");
    out.push_str(&escape_html(err.title.trim()));
    out.push_str("</b>\n");
    out.push_str(&escape_html(err.summary.trim()));
    if let Some(d) = err.details.as_deref() {
        let d = d.trim();
        if !d.is_empty() {
            out.push_str("\n<pre>");
            out.push_str(&escape_html(d));
            out.push_str("</pre>");
        }
    }
    if err.retryable {
        out.push_str("\n<i>will retry automatically</i>");
    }
    out
}

/// Render a long-output expander as Telegram HTML:
///
/// ```text
/// <i>{escaped summary}</i>
/// <blockquote expandable>{escaped full text}</blockquote>
/// ```
///
/// `<blockquote expandable>` is a Bot API 7.6+ primitive that
/// collapses the body behind a disclosure widget on every
/// officially-supported client. Clients that don't recognise
/// `expandable` fall through to a fully-rendered blockquote — still
/// readable, just not collapsed (graceful).
///
/// HTML (vs `MarkdownV2`) keeps escaping to the five XML entities so
/// tool stdout / source / log lines round-trip without surprising
/// backslashing.
pub(crate) fn render_collapsible_html(text: &str, summary: &str) -> String {
    let mut out = String::with_capacity(text.len() + summary.len() + 64);
    let trimmed_summary = summary.trim();
    if !trimmed_summary.is_empty() {
        out.push_str("<i>");
        out.push_str(&escape_html(trimmed_summary));
        out.push_str("</i>\n");
    }
    out.push_str("<blockquote expandable>");
    out.push_str(&escape_html(text));
    out.push_str("</blockquote>");
    out
}

/// Render a [`ThinkingBlock`] for Telegram HTML. Format:
///
/// ```text
/// <i>reasoning</i>
/// <blockquote expandable>The user asked about X. I should…</blockquote>
/// ```
///
/// With a provenance tag the label becomes
/// `<i>reasoning (claude-opus-4-7)</i>`; redacted blocks emit a
/// placeholder body so the raw blob is never put on the wire even on a
/// channel as private as a 1:1 DM.
///
/// Same `<blockquote expandable>` primitive as surface 4 (long-output)
/// — Bot API 7.6+, every mainstream client renders it collapsed by
/// default with a "Show more" disclosure widget. Pre-7.6 clients fall
/// back to a fully-rendered blockquote — readable, just not collapsed
/// (graceful).
pub(crate) fn render_thinking_html(t: &ThinkingBlock) -> String {
    let mut out = String::with_capacity(t.text.len() + 64);
    out.push_str("<i>reasoning");
    if let Some(m) = t.model.as_deref() {
        let m = m.trim();
        if !m.is_empty() {
            out.push_str(" (");
            out.push_str(&escape_html(m));
            out.push(')');
        }
    }
    out.push_str("</i>\n");
    out.push_str("<blockquote expandable>");
    if t.redacted {
        // Privacy contract: never emit the raw redacted blob — even on
        // channels with strong encryption / 1:1 DMs. The user sees a
        // labelled placeholder so they know reasoning happened but the
        // model didn't authorise its display.
        out.push_str("(redacted reasoning)");
    } else {
        out.push_str(&escape_html(&t.text));
    }
    out.push_str("</blockquote>");
    out
}

/// Render a [`TodoList`] for Telegram `MarkdownV2`. Format:
///
/// ```text
/// *Plan*
/// ☑ Wash dishes
/// ▶ Dry dishes
/// ☐ Put dishes away
/// _1/3 done_
/// ```
///
/// Glyphs are picked so they're visually distinct without depending
/// on color: `☑` for completed (filled check), `▶` for in-progress
/// (forward-pointing arrow), `☐` for pending (empty box). All static
/// markup characters are emitted verbatim; only user-supplied text
/// (title, item text) goes through [`escape_markdown_v2`] so the
/// `parse_mode` doesn't reject the body on a bare `.` / `-` / `!` in
/// the agent's plan. Completed items are wrapped in `~strikethrough~`
/// so the eye can scan the unchecked work at a glance.
pub(crate) fn render_todo_list_markdown(list: &TodoList) -> String {
    let mut out = String::with_capacity(64 + list.items.len() * 48);
    out.push('*');
    out.push_str(&escape_markdown_v2(list.title_or_default()));
    out.push_str("*\n");
    for item in &list.items {
        // ASCII-only glyphs per the project's no-emoji rule. iOS
        // Telegram renders the symbol forms (☑ / ▶ / ☐) as colourful
        // emoji even though they're technically symbols.
        //
        // MarkdownV2 reserves `[`, `]`, `(`, `)`, `~`, `_`, `*`, `>`,
        // `#`, `+`, `-`, `=`, `|`, `{`, `}`, `.`, `!` — they MUST be
        // backslash-escaped or Telegram rejects the message with
        // `Bad Request: can't parse entities`. The raw markers below
        // pass through `escape_markdown_v2` so `[`/`]` survive the
        // parse. Caught live on 2026-05-24: every TodoList edit
        // returned 400, the delivery loop marked failed, the agent
        // saw it and cascaded into endless retries.
        let glyph_raw = match item.status {
            TodoItemStatus::Completed => "[x]",
            TodoItemStatus::InProgress => "[~]",
            TodoItemStatus::Pending => "[ ]",
        };
        out.push_str(&escape_markdown_v2(glyph_raw));
        out.push(' ');
        if item.status == TodoItemStatus::Completed {
            out.push('~');
            out.push_str(&escape_markdown_v2(item.text.trim()));
            out.push('~');
        } else {
            out.push_str(&escape_markdown_v2(item.text.trim()));
        }
        out.push('\n');
    }
    let done = list.completed_count();
    let total = list.items.len();
    out.push('_');
    out.push_str(&escape_markdown_v2(&format!("{done}/{total} done")));
    out.push('_');
    out
}

/// Convert a small subset of markdown syntax to Telegram-compatible HTML.
///
/// Handles, in order:
/// - Fenced code blocks ``` ```lang\n...\n``` ``` → `<pre><code class="language-lang">...</code></pre>`
/// - Inline code `` `x` `` → `<code>x</code>`
/// - Bold `**x**` → `<b>x</b>` (also `__x__`)
/// - Italic `*x*` / `_x_` → `<i>x</i>` (only when surrounded by word boundaries
///   to avoid matching inside identifiers like `foo_bar_baz`)
/// - Strikethrough `~~x~~` → `<s>x</s>`
/// - Inline links `[text](url)` → `<a href="url">text</a>`
///
/// Everything else is HTML-escaped (`<`, `>`, `&`, `"`, `'`). The output is
/// safe to send with `parse_mode=HTML` — Telegram's tolerant HTML parser
/// only cares about the five XML escapes and a small allow-list of tags.
///
/// Designed for natural-language agent prose: stays correct under
/// unbalanced markers (e.g. a stray `*` becomes a literal asterisk), and
/// preserves leading whitespace + newlines so list-style replies still
/// look like lists.
pub(crate) fn markdown_to_html(input: &str) -> String {
    // Pass 1: extract fenced code blocks and inline code into placeholder
    // tokens so the bold/italic passes don't run inside them. Token
    // format: `\x00<idx>\x00` (NULs never appear in agent output).
    //
    // Byte-indexing on `&str` is safe for ASCII markers (`` ` ``, `\n`)
    // because every byte of a multi-byte UTF-8 sequence has the
    // high bit set — none of them match the ASCII markers we look for.
    // We only push to the output via `&str` slicing (`&input[a..b]`)
    // which preserves UTF-8 boundaries; we never coerce a single byte
    // back to a `char`.
    let mut frozen: Vec<String> = Vec::new();
    let mut text = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut chunk_start = 0;
    while i < bytes.len() {
        // Fenced code block: ```lang?\n...\n```
        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"```" {
            if let Some(close) = find_fence_close(bytes, i + 3) {
                // Flush any pending text before the fence.
                if chunk_start < i {
                    text.push_str(&input[chunk_start..i]);
                }
                let header_end = bytes[i + 3..close]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(close, |p| i + 3 + p);
                let lang = input[i + 3..header_end].trim();
                let body_start = if header_end < close { header_end + 1 } else { header_end };
                let body = &input[body_start..close];
                let body_escaped = escape_html(body.trim_end_matches('\n'));
                let token = if lang.is_empty() {
                    format!("<pre><code>{body_escaped}</code></pre>")
                } else {
                    let lang_safe = escape_html(lang);
                    format!("<pre><code class=\"language-{lang_safe}\">{body_escaped}</code></pre>")
                };
                frozen.push(token);
                text.push('\0');
                text.push_str(&(frozen.len() - 1).to_string());
                text.push('\0');
                i = close + 3;
                chunk_start = i;
                continue;
            }
        }
        // Inline code: `x`
        if bytes[i] == b'`' {
            if let Some(rel) = bytes[i + 1..].iter().position(|&b| b == b'`') {
                let inner = &input[i + 1..i + 1 + rel];
                if !inner.is_empty() && !inner.contains('\n') {
                    // Flush any pending text before the backtick.
                    if chunk_start < i {
                        text.push_str(&input[chunk_start..i]);
                    }
                    frozen.push(format!("<code>{}</code>", escape_html(inner)));
                    text.push('\0');
                    text.push_str(&(frozen.len() - 1).to_string());
                    text.push('\0');
                    i += rel + 2;
                    chunk_start = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    // Flush trailing text.
    if chunk_start < bytes.len() {
        text.push_str(&input[chunk_start..]);
    }

    // Pass 2: escape HTML in the body (NUL tokens carry through the escape).
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(c),
        }
    }
    text = escaped;

    // Pass 3: bold (**x**), strikethrough (~~x~~), italic (*x*/_x_),
    // inline links [text](url). Order matters — bold before italic so
    // `**foo**` doesn't get partially consumed.
    text = replace_paired(&text, "**", "<b>", "</b>");
    text = replace_paired(&text, "__", "<b>", "</b>");
    text = replace_paired(&text, "~~", "<s>", "</s>");
    text = replace_italic_paired(&text, '*');
    text = replace_italic_paired(&text, '_');
    text = replace_inline_links(&text);

    // Pass 4: substitute placeholder tokens back with their HTML.
    let mut out = String::with_capacity(text.len());
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\0' {
            out.push(c);
            continue;
        }
        let mut digits = String::new();
        while let Some(&d) = it.peek() {
            if d.is_ascii_digit() {
                digits.push(d);
                it.next();
            } else {
                break;
            }
        }
        // Consume closing NUL.
        if it.peek() == Some(&'\0') {
            it.next();
        }
        if let Ok(idx) = digits.parse::<usize>() {
            if let Some(token) = frozen.get(idx) {
                out.push_str(token);
            }
        }
    }
    out
}

/// Find the byte offset of the closing fence (the leading backticks of
/// the closing ```` ``` ````). Returns `None` if unbalanced.
fn find_fence_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 2 < bytes.len() {
        if &bytes[j..j + 3] == b"```" {
            // Must be at line start (or string start) — protects against
            // mid-line backtick triplets.
            if j == 0 || bytes[j - 1] == b'\n' {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// Replace `delim ... delim` with `open ... close`. `delim` must be a
/// multi-char marker like `**`. Non-greedy.
fn replace_paired(input: &str, delim: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(delim) {
        out.push_str(&rest[..start]);
        let after = &rest[start + delim.len()..];
        if let Some(end) = after.find(delim) {
            out.push_str(open);
            out.push_str(&after[..end]);
            out.push_str(close);
            rest = &after[end + delim.len()..];
        } else {
            // No close — leave the marker literal.
            out.push_str(delim);
            rest = after;
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Italic replacement for `*x*` / `_x_`. Only matches when the marker
/// isn't doubled (which would already be bold) and the content is
/// non-whitespace. Conservative: skips word-internal `_` like `foo_bar`.
fn replace_italic_paired(input: &str, delim: char) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == delim && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            // Find closing delim at a word boundary too.
            let mut j = i + 1;
            let mut found_end = None;
            while j < chars.len() {
                if chars[j] == delim
                    && (j + 1 == chars.len() || !chars[j + 1].is_alphanumeric())
                {
                    found_end = Some(j);
                    break;
                }
                if chars[j] == '\n' {
                    break;
                }
                j += 1;
            }
            if let Some(end) = found_end {
                let inner: String = chars[i + 1..end].iter().collect();
                if !inner.trim().is_empty() {
                    out.push_str("<i>");
                    out.push_str(&inner);
                    out.push_str("</i>");
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `[text](url)` → `<a href="url">text</a>`. URL is left as-is (already
/// HTML-escaped from pass 2); text is also already escaped.
fn replace_inline_links(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        if let Some(close_bracket) = after_open.find(']') {
            let after_bracket = &after_open[close_bracket + 1..];
            if after_bracket.starts_with('(') {
                if let Some(close_paren) = after_bracket.find(')') {
                    let text = &after_open[..close_bracket];
                    let url = &after_bracket[1..close_paren];
                    out.push_str("<a href=\"");
                    out.push_str(url);
                    out.push_str("\">");
                    out.push_str(text);
                    out.push_str("</a>");
                    rest = &after_bracket[close_paren + 1..];
                    continue;
                }
            }
        }
        // No valid link — leave the bracket literal.
        out.push('[');
        rest = after_open;
    }
    out.push_str(rest);
    out
}

/// Telegram's HTML parser only requires the five XML escapes; everything
/// else is rendered literally. Mirrors what every mainstream HTML
/// escape util does so we don't pull in a dependency for one function.
fn escape_html(s: &str) -> String {
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

/// Build the row-major [`InlineKeyboardButton`] layout Telegram expects
/// for the `reply_markup.inline_keyboard` field. Buttons with `value`
/// become `callback_data` buttons; buttons with `url` become URL
/// buttons. Rows wrap at [`MAX_BUTTONS_PER_ROW`].
fn build_inline_keyboard(card: &Card) -> Vec<Vec<InlineKeyboardButton>> {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut current: Vec<InlineKeyboardButton> = Vec::new();
    for btn in &card.buttons {
        let row_button = match (btn.value.as_deref(), btn.url.as_deref()) {
            (Some(value), _) => InlineKeyboardButton::callback(btn.label.clone(), value),
            (None, Some(url)) => InlineKeyboardButton::url(btn.label.clone(), url),
            // The card validator rejects buttons missing both — but we
            // defensively skip rather than panic if a bad card slips
            // through.
            (None, None) => continue,
        };
        current.push(row_button);
        if current.len() >= MAX_BUTTONS_PER_ROW {
            rows.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    rows
}

/// Truncate `body` so it fits in [`MAX_PHOTO_CAPTION_CHARS`] characters
/// once the "(see message below)" suffix is appended. The suffix is
/// `MarkdownV2`-escaped to survive `parse_mode=MarkdownV2`.
fn truncate_for_photo_caption(body: &str) -> String {
    let escaped_suffix = escape_markdown_v2(CAPTION_OVERFLOW_SUFFIX);
    // We measure in chars, not bytes — MarkdownV2 supports multi-byte
    // codepoints and Telegram counts them as 1.
    let suffix_chars = escaped_suffix.chars().count();
    // Leave room for the suffix plus a separator newline.
    let budget = MAX_PHOTO_CAPTION_CHARS.saturating_sub(suffix_chars + 1);
    let mut out: String = body.chars().take(budget).collect();
    // Avoid leaving a dangling backslash that would escape a
    // non-existent next character and confuse the MarkdownV2 parser.
    while out.ends_with('\\') {
        out.pop();
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&escaped_suffix);
    out
}

fn extract_text(value: &serde_json::Value) -> String {
    value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn extract_parse_mode(value: &serde_json::Value) -> Option<String> {
    value
        .get("parse_mode")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LongPollConfig, TelegramConfig, WebhookConfig};
    use ironclaw_types::{MessageKind, OutboundFile};
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── markdown_to_html ──────────────────────────────────────────

    #[test]
    fn markdown_html_bold_and_italic() {
        assert_eq!(markdown_to_html("**hi**"), "<b>hi</b>");
        assert_eq!(markdown_to_html("__hi__"), "<b>hi</b>");
        assert_eq!(markdown_to_html("*hi*"), "<i>hi</i>");
        assert_eq!(markdown_to_html("_hi_"), "<i>hi</i>");
    }

    #[test]
    fn markdown_html_inline_code_blocks_protected_from_bold_pass() {
        // Asterisks inside inline code must NOT be interpreted as bold.
        assert_eq!(
            markdown_to_html("Use `**not bold**` literally"),
            "Use <code>**not bold**</code> literally"
        );
    }

    #[test]
    fn markdown_html_fenced_block_with_language_tag() {
        let out = markdown_to_html("```rust\nfn main() {}\n```");
        assert!(out.contains("<pre><code class=\"language-rust\">fn main() {}</code></pre>"), "got: {out}");
    }

    #[test]
    fn markdown_html_escapes_special_chars_outside_code() {
        // Bare `<`, `>`, `&`, `"`, `'` outside code must be HTML-escaped.
        let out = markdown_to_html("a < b & c > d \"e\" 'f'");
        assert!(out.contains("&lt;"), "got: {out}");
        assert!(out.contains("&gt;"), "got: {out}");
        assert!(out.contains("&amp;"), "got: {out}");
        assert!(out.contains("&quot;"), "got: {out}");
        assert!(out.contains("&#39;"), "got: {out}");
    }

    #[test]
    fn markdown_html_underscore_within_identifier_not_italicised() {
        // `foo_bar_baz` should NOT become `foo<i>bar</i>baz`.
        let out = markdown_to_html("foo_bar_baz");
        assert_eq!(out, "foo_bar_baz");
    }

    #[test]
    fn markdown_html_inline_link() {
        assert_eq!(
            markdown_to_html("see [docs](https://example.com)"),
            "see <a href=\"https://example.com\">docs</a>"
        );
    }

    #[test]
    fn markdown_html_unbalanced_marker_left_literal() {
        // A stray `*` with no closer must stay literal, not break the parser.
        let out = markdown_to_html("rate is 5 * 3 = 15");
        // The single `*` is left as-is (not wrapped in <i>); equal sign + digits all escape-safe.
        assert!(!out.contains("<i>"), "got: {out}");
    }

    #[test]
    fn markdown_html_strikethrough() {
        assert_eq!(markdown_to_html("~~old~~"), "<s>old</s>");
    }

    #[test]
    fn markdown_html_preserves_newlines_and_lists() {
        let out = markdown_to_html("- one\n- two\n- three");
        assert!(out.contains("- one\n- two\n- three"), "got: {out}");
    }

    #[test]
    fn markdown_html_preserves_multi_byte_utf8_chars() {
        // Regression: an earlier version iterated `bytes[i] as char` which
        // corrupted multi-byte UTF-8 sequences (em-dash, é, CJK, emoji)
        // into Latin-1 codepoints — em-dash became `â`.
        let out = markdown_to_html("hello — world. café. 日本語. 🦀");
        assert!(out.contains("—"), "em-dash preserved: {out}");
        assert!(out.contains("café"), "accent preserved: {out}");
        assert!(out.contains("日本語"), "CJK preserved: {out}");
        assert!(out.contains("🦀"), "emoji preserved: {out}");
        assert!(!out.contains("â"), "no Latin-1 corruption: {out}");
    }

    #[test]
    fn markdown_html_multi_byte_inside_bold() {
        // Multi-byte chars inside markdown markers must round-trip.
        let out = markdown_to_html("**café — résumé**");
        assert!(out.contains("<b>café — résumé</b>"), "got: {out}");
    }

    fn temp_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    fn lp_config(api_base: &str) -> TelegramConfig {
        TelegramConfig {
            bot_token: "tok".into(),
            api_base: api_base.into(),
            mode: IngressMode::LongPoll(LongPollConfig {
                timeout_secs: 0,
                limit: 100,
                allowed_updates: vec![],
            }),
            attachment_download: true,
            max_attachment_bytes: crate::config::DEFAULT_MAX_ATTACHMENT_BYTES,
        }
    }

    fn wh_config(api_base: &str) -> TelegramConfig {
        TelegramConfig {
            bot_token: "tok".into(),
            api_base: api_base.into(),
            mode: IngressMode::Webhook(WebhookConfig {
                host: "127.0.0.1".into(),
                port: 0,
                path: "/hook".into(),
                secret_token: None,
            }),
            attachment_download: true,
            max_attachment_bytes: crate::config::DEFAULT_MAX_ATTACHMENT_BYTES,
        }
    }

    async fn empty_get_updates(s: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": []
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn channel_type_and_thread_support() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cfg = lp_config(&s.uri());
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            cfg,
            Some("ironbot".into()),
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        assert_eq!(adapter.channel_type().as_str(), "telegram");
        assert!(adapter.supports_threads());
        assert_eq!(adapter.bot_username(), Some("ironbot"));
        assert!(adapter.webhook_addr().is_none());
        assert_eq!(adapter.config().bot_token, "tok");
        assert_eq!(adapter.data_dir(), dir.path());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn debug_format_renders() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let dbg = format!("{adapter:?}");
        assert!(dbg.contains("TelegramAdapter"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_sends_send_message_and_returns_id() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": 555 }
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let id = adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "hello" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("555"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_uses_explicit_parse_mode_when_provided() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": 1 }
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "*hi*", "parse_mode": "Markdown" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_files_uses_send_document_returns_last_id() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        // We return increasing ids for two consecutive document uploads.
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(700));
        let c = counter.clone();
        Mock::given(method("POST"))
            .and(path("/bottok/sendDocument"))
            .respond_with(move |_req: &wiremock::Request| {
                let n = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "ok": true, "result": { "message_id": n }
                }))
            })
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let id = adapter
            .deliver(
                "100",
                Some("9"),
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "caption" }),
                    files: vec![
                        OutboundFile {
                            filename: "a.txt".into(),
                            data: vec![1, 2, 3],
                        },
                        OutboundFile {
                            filename: "b.bin".into(),
                            data: vec![],
                        },
                    ],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("701"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_no_text_and_no_files_errors() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let err = adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_files_no_text_works() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendDocument"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": 12 }
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let id = adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({}),
                    files: vec![OutboundFile {
                        filename: "x.bin".into(),
                        data: vec![1],
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("12"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn set_typing_sends_chat_action() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendChatAction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": true
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        adapter.set_typing("100", None).await.unwrap();
        adapter.set_typing("100", Some("9")).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_calls_get_me() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "id": 1, "is_bot": true }
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        adapter.subscribe("100", None).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_returns_handle_with_matching_ids() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let handle = adapter.open_dm("user-123").await.unwrap().expect("handle");
        assert_eq!(handle.user_id, "user-123");
        assert_eq!(handle.platform_id, "user-123");
        assert_eq!(handle.channel_type.as_str(), "telegram");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn webhook_mode_records_bound_addr() {
        let s = MockServer::start().await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            wh_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let addr = adapter.webhook_addr().expect("addr");
        assert!(addr.port() > 0);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        adapter.shutdown().await;
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn start_with_api_uses_supplied_client() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let api = TelegramApi::new(s.uri(), "tok");
        let dir = temp_dir();
        let adapter = TelegramAdapter::start_with_api(
            api,
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        assert_eq!(adapter.channel_type().as_str(), "telegram");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn webhook_bind_failure_returns_transport_error() {
        let bad = TelegramConfig {
            bot_token: "tok".into(),
            api_base: "http://localhost".into(),
            mode: IngressMode::Webhook(WebhookConfig {
                host: "::not-an-ip::".into(),
                port: 0,
                path: "/hook".into(),
                secret_token: None,
            }),
            attachment_download: true,
            max_attachment_bytes: crate::config::DEFAULT_MAX_ATTACHMENT_BYTES,
        };
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let err = TelegramAdapter::start(bad, None, tx, dir.path().to_path_buf())
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn plain_text_fallback_strips_parse_mode_for_telegram() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();

        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({
                "text": "Hey! Yes, I'm here!",
                "parse_mode": "MarkdownV2"
            }),
            files: vec![],
        };
        let fallback = adapter
            .plain_text_fallback(&msg)
            .expect("telegram fallback");
        // parse_mode must be removed entirely.
        assert!(fallback.content.get("parse_mode").is_none());
        // text is prepended with the reduced-formatting marker.
        assert_eq!(
            fallback.content["text"].as_str().unwrap(),
            "[reduced formatting] Hey! Yes, I'm here!"
        );
        // Kind and files are preserved.
        assert_eq!(fallback.kind, MessageKind::Chat);
        assert!(fallback.files.is_empty());

        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn plain_text_fallback_returns_none_when_already_plain() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();

        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": "Hello" }),
            files: vec![],
        };
        assert!(adapter.plain_text_fallback(&msg).is_none());
        adapter.shutdown().await;
    }

    #[test]
    fn extract_text_handles_missing_field() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({"text": "x"})), "x");
        assert_eq!(extract_text(&json!({"text": 1})), "");
    }

    #[test]
    fn extract_parse_mode_handles_field() {
        assert_eq!(extract_parse_mode(&json!({})), None);
        assert_eq!(extract_parse_mode(&json!({"parse_mode": "Markdown"})), Some("Markdown".into()));
        assert_eq!(extract_parse_mode(&json!({"parse_mode": 1})), None);
    }

    #[test]
    fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "telegram");
        // HTML is the default — the deliver path pre-converts a small
        // subset of markdown to HTML tags (see `markdown_to_html`) and
        // HTML-escapes the rest. Agents can override via
        // `content.parse_mode = "MarkdownV2"` / `"Markdown"` / `""`.
        assert_eq!(DEFAULT_PARSE_MODE, "HTML");
    }

    #[tokio::test]
    async fn telegram_edit_message_calls_api() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/editMessageText"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": 7 }
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        adapter
            .edit_message("100", Some("thread-1"), "7", "edited text")
            .await
            .unwrap();
        let reqs = s.received_requests().await.unwrap();
        let edit_req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/editMessageText"))
            .expect("editMessageText request");
        let body: serde_json::Value =
            serde_json::from_slice(&edit_req.body).expect("json body");
        assert_eq!(body["chat_id"], "100");
        assert_eq!(body["message_id"], 7);
        assert_eq!(body["text"], "edited text");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn telegram_add_reaction_calls_api() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/setMessageReaction"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": true
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        adapter
            .add_reaction("100", None, "7", "\u{1F44D}")
            .await
            .unwrap();
        let reqs = s.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/setMessageReaction"))
            .expect("setMessageReaction request");
        let body: serde_json::Value =
            serde_json::from_slice(&req.body).expect("json body");
        assert_eq!(body["chat_id"], "100");
        assert_eq!(body["message_id"], 7);
        assert_eq!(body["reaction"][0]["type"], "emoji");
        assert_eq!(body["reaction"][0]["emoji"], "\u{1F44D}");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_uses_html_parse_mode_and_escapes_specials() {
        // After the HTML-default switch: natural-language replies with
        // `!`, `.`, etc. round-trip safely because HTML mode only
        // reserves `<`, `>`, `&`, `"`, `'` — all five are escaped by
        // `markdown_to_html`. parse_mode MUST be present and the body
        // MUST be HTML-escaped (no raw `<` etc. would survive).
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": 777 }
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let id = adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "**hey!** <yes>" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("777"));
        let reqs = s.received_requests().await.unwrap();
        let send_msg_req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body = String::from_utf8_lossy(&send_msg_req.body);
        assert!(
            body.contains("\"parse_mode\":\"HTML\""),
            "default deliver must set parse_mode=HTML but body was: {body:?}"
        );
        assert!(
            body.contains("<b>hey!</b>"),
            "markdown bold should be converted to HTML <b>: {body:?}"
        );
        assert!(
            body.contains("&lt;yes&gt;"),
            "raw angle brackets in agent text must be HTML-escaped: {body:?}"
        );
        adapter.shutdown().await;
    }

    // -----------------------------------------------------------------
    // Team CHN audit additions: adapter-level edge cases for rate-limit
    // and malformed-response paths. The api-level tests cover the same
    // shapes; these confirm the adapter surfaces the error variant
    // unchanged through the trait boundary.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn deliver_propagates_rate_limit_with_retry_after() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "42")
                    .set_body_json(json!({
                        "ok": false,
                        "error_code": 429,
                        "description": "Too Many Requests: retry after 42",
                        "parameters": { "retry_after": 42 }
                    })),
            )
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let err = adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => {
                assert_eq!(retry_after, Some(42));
            }
            other => panic!("expected Rate, got {other:?}"),
        }
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_malformed_response_body_is_transport() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json at all"))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let err = adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, AdapterError::Transport(_)),
            "expected Transport, got {err:?}"
        );
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_non_object_content_with_no_text_errors() {
        // content as a bare JSON array — no `text` key reachable.
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        let err = adapter
            .deliver(
                "100",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!([1, 2, 3]),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    // ---------------------------------------------------------------
    // Wave 2b: `deliver_card` native rendering + callback_query inbound.
    // ---------------------------------------------------------------

    use ironclaw_channels_core::{Card, CardButton, CardField};

    /// Mount a `sendMessage` mock that records the body and replies with
    /// `message_id = 1001`. Returns the server so the test can inspect
    /// `received_requests` for the captured payload.
    async fn mount_send_message(server: &MockServer, id: i64) {
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": id }
            })))
            .mount(server)
            .await;
    }

    async fn mount_send_photo(server: &MockServer, id: i64) {
        Mock::given(method("POST"))
            .and(path("/bottok/sendPhoto"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": id }
            })))
            .mount(server)
            .await;
    }

    async fn build_adapter(s: &MockServer) -> Arc<TelegramAdapter> {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();
        // Leak the tempdir so it lives as long as the adapter — the
        // returned Arc owns the path reference indirectly via
        // `data_dir`.
        std::mem::forget(dir);
        adapter
    }

    fn confirm_card_with_buttons() -> Card {
        Card {
            title: Some("Order #42".into()),
            body: Some("Ready to confirm?".into()),
            fields: vec![
                CardField {
                    label: "Item".into(),
                    value: "Espresso".into(),
                    inline: true,
                },
                CardField {
                    label: "Price".into(),
                    value: "$4.50".into(),
                    inline: true,
                },
            ],
            buttons: vec![
                CardButton {
                    label: "Confirm".into(),
                    value: Some("confirm:42".into()),
                    url: None,
                    style: Some("primary".into()),
                },
                CardButton {
                    label: "Cancel".into(),
                    value: Some("cancel:42".into()),
                    url: None,
                    style: None,
                },
                CardButton {
                    label: "Details".into(),
                    value: None,
                    url: Some("https://example.com/o/42".into()),
                    style: None,
                },
            ],
            image_url: None,
        }
    }

    #[tokio::test]
    async fn deliver_card_with_buttons_sends_inline_keyboard() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        mount_send_message(&s, 5001).await;
        let adapter = build_adapter(&s).await;

        let card = confirm_card_with_buttons();
        let id = adapter
            .deliver_card("100", None, &card, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("5001"));

        let reqs = s.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["chat_id"], "100");
        // MarkdownV2 parse_mode is set.
        assert_eq!(body["parse_mode"], "MarkdownV2");
        // Body contains the bold title (with `#` escaped) and the body.
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("*Order \\#42*"), "got `{text}`");
        assert!(text.contains("Ready to confirm"), "got `{text}`");
        // Fields use `*Label:*` MarkdownV2 bold around the label.
        assert!(text.contains("*Item:*"), "got `{text}`");
        assert!(text.contains("Espresso"), "got `{text}`");

        // Inline keyboard: three buttons → 1 row of 3 (fits within MAX_BUTTONS_PER_ROW=3).
        let kb = body["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap();
        assert_eq!(kb.len(), 1);
        let row = kb[0].as_array().unwrap();
        assert_eq!(row.len(), 3);
        assert_eq!(row[0]["text"], "Confirm");
        assert_eq!(row[0]["callback_data"], "confirm:42");
        assert!(row[0].get("url").is_none() || row[0]["url"].is_null());
        assert_eq!(row[2]["text"], "Details");
        assert_eq!(row[2]["url"], "https://example.com/o/42");
        assert!(row[2].get("callback_data").is_none() || row[2]["callback_data"].is_null());

        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_card_text_only_falls_through_to_send_message() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        mount_send_message(&s, 99).await;
        let adapter = build_adapter(&s).await;

        let card = Card {
            title: Some("Just a heading".into()),
            body: Some("Nothing to click here.".into()),
            ..Card::default()
        };
        let id = adapter
            .deliver_card("c-1", None, &card, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("99"));

        let reqs = s.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        // No buttons → no reply_markup field.
        assert!(
            body.get("reply_markup").is_none(),
            "expected no reply_markup, got `{body}`"
        );
        assert_eq!(body["parse_mode"], "MarkdownV2");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_card_with_image_sends_photo_with_caption_and_keyboard() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        mount_send_photo(&s, 222).await;
        let adapter = build_adapter(&s).await;

        let card = Card {
            title: Some("Latte".into()),
            body: Some("Want one?".into()),
            buttons: vec![CardButton {
                label: "Yes".into(),
                value: Some("yes".into()),
                url: None,
                style: None,
            }],
            image_url: Some("https://example.com/latte.png".into()),
            ..Card::default()
        };
        let id = adapter.deliver_card("c-1", None, &card, None).await.unwrap();
        assert_eq!(id.as_deref(), Some("222"));

        let reqs = s.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendPhoto"))
            .expect("sendPhoto request");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["photo"], "https://example.com/latte.png");
        assert!(body["caption"].as_str().unwrap().contains("*Latte*"));
        assert_eq!(body["parse_mode"], "MarkdownV2");
        // Keyboard rides on the photo message in the single-call path.
        let kb = body["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap();
        assert_eq!(kb.len(), 1);
        assert_eq!(kb[0][0]["callback_data"], "yes");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_card_long_caption_splits_to_photo_plus_message() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        mount_send_photo(&s, 1).await;
        mount_send_message(&s, 2).await;
        let adapter = build_adapter(&s).await;

        // 2000 chars of body → after escaping (no reserved chars in `a`)
        // still 2000 chars, well above MAX_PHOTO_CAPTION_CHARS=900.
        let big_body = "a".repeat(2000);
        let card = Card {
            body: Some(big_body),
            buttons: vec![CardButton {
                label: "Ok".into(),
                value: Some("ok".into()),
                url: None,
                style: None,
            }],
            image_url: Some("https://example.com/x.png".into()),
            ..Card::default()
        };
        let id = adapter.deliver_card("c-1", None, &card, None).await.unwrap();
        assert_eq!(id.as_deref(), Some("1")); // photo id returned, not follow-up

        let reqs = s.received_requests().await.unwrap();
        let photo_req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendPhoto"))
            .expect("sendPhoto request");
        let photo_body: serde_json::Value =
            serde_json::from_slice(&photo_req.body).unwrap();
        // Caption is truncated and ends with the (escaped) overflow suffix.
        let caption = photo_body["caption"].as_str().unwrap();
        assert!(caption.chars().count() <= MAX_PHOTO_CAPTION_CHARS);
        assert!(caption.contains("see message below"), "got `{caption}`");
        // No reply_markup on the photo when the body splits.
        assert!(
            photo_body.get("reply_markup").is_none(),
            "expected no reply_markup on photo when body split"
        );

        let msg_req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("follow-up sendMessage request");
        let msg_body: serde_json::Value = serde_json::from_slice(&msg_req.body).unwrap();
        // Full body lands in the follow-up message and carries the keyboard.
        assert!(msg_body["text"].as_str().unwrap().contains("aaaa"));
        let kb = msg_body["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap();
        assert_eq!(kb[0][0]["callback_data"], "ok");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_card_many_buttons_wrap_into_rows() {
        // 8 buttons → 3 + 3 + 2 layout.
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        mount_send_message(&s, 1).await;
        let adapter = build_adapter(&s).await;

        let buttons: Vec<CardButton> = (0..8)
            .map(|i| CardButton {
                label: format!("B{i}"),
                value: Some(format!("v{i}")),
                url: None,
                style: None,
            })
            .collect();
        let card = Card {
            title: Some("Pick one".into()),
            buttons,
            ..Card::default()
        };
        adapter.deliver_card("c", None, &card, None).await.unwrap();

        let reqs = s.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        let kb = body["reply_markup"]["inline_keyboard"]
            .as_array()
            .unwrap();
        assert_eq!(kb.len(), 3, "expected 3 rows, got {}", kb.len());
        assert_eq!(kb[0].as_array().unwrap().len(), 3);
        assert_eq!(kb[1].as_array().unwrap().len(), 3);
        assert_eq!(kb[2].as_array().unwrap().len(), 2);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_card_propagates_thread_id_to_send_message() {
        let s = MockServer::start().await;
        empty_get_updates(&s).await;
        mount_send_message(&s, 1).await;
        let adapter = build_adapter(&s).await;

        let card = Card {
            title: Some("Hi".into()),
            buttons: vec![CardButton {
                label: "Ok".into(),
                value: Some("ok".into()),
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        adapter
            .deliver_card("c-1", Some("17"), &card, None)
            .await
            .unwrap();
        let reqs = s.received_requests().await.unwrap();
        let req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["message_thread_id"], 17);
        adapter.shutdown().await;
    }

    #[test]
    fn markdown_v2_escapes_special_chars() {
        // Every reserved char per the Bot API spec must be backslash-prefixed.
        let escaped = escape_markdown_v2("Title v1.2 (beta)!");
        assert_eq!(escaped, "Title v1\\.2 \\(beta\\)\\!");

        // Underscores + stars + brackets all escaped.
        let escaped = escape_markdown_v2("a_b *c* [d] `e` >f #g +h -i =j |k {l} ~m");
        assert!(escaped.contains("a\\_b"));
        assert!(escaped.contains("\\*c\\*"));
        assert!(escaped.contains("\\[d\\]"));
        assert!(escaped.contains("\\`e\\`"));
        assert!(escaped.contains("\\>f"));
        assert!(escaped.contains("\\#g"));
        assert!(escaped.contains("\\+h"));
        assert!(escaped.contains("\\-i"));
        assert!(escaped.contains("\\=j"));
        assert!(escaped.contains("\\|k"));
        assert!(escaped.contains("\\{l\\}"));
        assert!(escaped.contains("\\~m"));

        // Non-reserved chars passed through unchanged.
        assert_eq!(escape_markdown_v2("hello world"), "hello world");
        assert_eq!(escape_markdown_v2(""), "");

        // Backslashes themselves are escaped so they cannot smuggle
        // through additional escape semantics.
        assert_eq!(escape_markdown_v2("\\foo"), "\\\\foo");
    }

    #[test]
    fn render_card_markdown_v2_full_card_layout() {
        let c = confirm_card_with_buttons();
        let out = render_card_markdown_v2(&c);
        // Title bolded with `#` and `.` escaped.
        assert!(out.starts_with("*Order \\#42*"), "got `{out}`");
        // Body present (unescaped chars survive).
        assert!(out.contains("Ready to confirm"));
        // Fields rendered as *Label:* value.
        assert!(out.contains("*Item:* Espresso"));
        // `$` is not reserved in MarkdownV2; `.` and `4.50` is escaped.
        assert!(out.contains("$4\\.50"), "got `{out}`");
        // No trailing newline.
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn build_inline_keyboard_wraps_at_three() {
        let mut card = Card {
            title: Some("t".into()),
            ..Card::default()
        };
        card.buttons = (0..5)
            .map(|i| CardButton {
                label: format!("B{i}"),
                value: Some(format!("v{i}")),
                url: None,
                style: None,
            })
            .collect();
        let rows = build_inline_keyboard(&card);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 3);
        assert_eq!(rows[1].len(), 2);
        assert_eq!(rows[0][0].callback_data.as_deref(), Some("v0"));
        assert!(rows[0][0].url.is_none());
    }

    #[test]
    fn build_inline_keyboard_skips_buttons_with_neither_value_nor_url() {
        // Validator catches these at the runner, but the adapter must
        // not panic if a bad card sneaks through (defence in depth).
        let card = Card {
            title: Some("t".into()),
            buttons: vec![CardButton {
                label: "B".into(),
                value: None,
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        assert!(build_inline_keyboard(&card).is_empty());
    }

    #[test]
    fn render_card_markdown_v2_title_only() {
        let c = Card {
            title: Some("hi".into()),
            ..Card::default()
        };
        assert_eq!(render_card_markdown_v2(&c), "*hi*");
    }

    #[test]
    fn render_card_markdown_v2_omits_blank_sections() {
        // Whitespace-only title / body must not produce empty headings.
        let c = Card {
            title: Some("  ".into()),
            body: Some("real body".into()),
            ..Card::default()
        };
        let out = render_card_markdown_v2(&c);
        assert_eq!(out, "real body");
    }

    #[test]
    fn truncate_for_photo_caption_appends_suffix_under_budget() {
        let body = "a".repeat(2000);
        let out = truncate_for_photo_caption(&body);
        assert!(out.chars().count() <= MAX_PHOTO_CAPTION_CHARS);
        assert!(out.ends_with("\\(see message below\\)"), "got `{out}`");
    }

    #[test]
    fn truncate_for_photo_caption_drops_dangling_backslash() {
        // Construct an escaped body whose truncation lands mid-escape.
        let body: String = "\\.".repeat(1000); // each pair is 2 chars
        let out = truncate_for_photo_caption(&body);
        // The truncation point must not leave a stray `\` immediately
        // before the suffix newline.
        let pre_suffix: String = out
            .chars()
            .take(out.chars().count() - "\\(see message below\\)".chars().count() - 1)
            .collect();
        assert!(!pre_suffix.ends_with('\\'), "stray backslash in `{pre_suffix}`");
    }

    // ---------------------------------------------------------------
    // Inbound: callback_query path.
    // ---------------------------------------------------------------

    /// Mount the long-poll endpoint so it returns a single `callback_query`
    /// update on the first call and then idle empty responses.
    async fn mount_callback_query_long_poll(server: &MockServer, data: &str) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = counter.clone();
        let data = data.to_owned();
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(move |_req: &wiremock::Request| {
                let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "ok": true,
                        "result": [{
                            "update_id": 11,
                            "callback_query": {
                                "id": "cb-xyz",
                                "from": { "id": 200, "is_bot": false, "username": "alice" },
                                "message": {
                                    "message_id": 99,
                                    "date": 1_700_000_000,
                                    "chat": { "id": 100, "type": "private" }
                                },
                                "data": data,
                            }
                        }]
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []}))
                }
            })
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn callback_query_inbound_becomes_chat_event() {
        let s = MockServer::start().await;
        mount_callback_query_long_poll(&s, "confirm:42").await;
        // answerCallbackQuery is called eagerly — mount so it does not fail.
        Mock::given(method("POST"))
            .and(path("/bottok/answerCallbackQuery"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": true
            })))
            .mount(&s)
            .await;

        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();

        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.channel_type.as_str(), "telegram");
        assert_eq!(evt.platform_id, "100");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "confirm:42");
        // Tagged so handlers can distinguish a callback from a typed message.
        assert_eq!(evt.message.content["callback"]["id"], "cb-xyz");
        assert_eq!(evt.message.content["callback"]["data"], "confirm:42");
        assert_eq!(evt.message.content["callback"]["original_message_id"], 99);
        let sender = evt.sender.as_ref().expect("sender");
        assert_eq!(sender.identity, "200");
        assert_eq!(sender.display_name.as_deref(), Some("alice"));
        assert_eq!(sender.channel_type.as_str(), "telegram");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn callback_query_acks_the_query() {
        let s = MockServer::start().await;
        mount_callback_query_long_poll(&s, "noop").await;
        Mock::given(method("POST"))
            .and(path("/bottok/answerCallbackQuery"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": true
            })))
            .mount(&s)
            .await;

        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let dir = temp_dir();
        let adapter = TelegramAdapter::start(
            lp_config(&s.uri()),
            None,
            tx,
            dir.path().to_path_buf(),
        )
        .await
        .unwrap();

        // Wait for the inbound event so we know the loop has processed
        // the callback and (per our ordering) already called the ack.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        // Give the long-poll a moment to finish flushing the ack.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let reqs = s.received_requests().await.unwrap();
        let ack = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/answerCallbackQuery"))
            .expect("answerCallbackQuery request");
        let body: serde_json::Value = serde_json::from_slice(&ack.body).unwrap();
        assert_eq!(body["callback_query_id"], "cb-xyz");
        // We pass `None` text so no toast pops up.
        assert!(body.get("text").is_none(), "expected no text, got `{body}`");
        adapter.shutdown().await;
    }

    // ── Breadcrumb chip rendering ──────────────────────────────────

    #[test]
    fn render_breadcrumb_running_uses_code_chip_and_ascii_marker() {
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let html = super::render_breadcrumb_html(&bc);
        assert!(html.starts_with("[~]"), "running ASCII marker: {html}");
        assert!(html.contains("<code>shell</code>"));
        assert!(html.contains("cargo check"));
    }

    #[test]
    fn render_breadcrumb_done_includes_marker_and_summary() {
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let html = super::render_breadcrumb_html(&bc);
        assert!(html.starts_with("[ok]"), "got: {html}");
        assert!(html.contains("passed (0.4s)"));
    }

    #[test]
    fn render_breadcrumb_failed_prefixes_failed_summary() {
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(false, Some("timeout".into()));
        let html = super::render_breadcrumb_html(&bc);
        assert!(html.starts_with("[x]"), "got: {html}");
        assert!(html.contains("failed: timeout"));
    }

    #[test]
    fn render_breadcrumb_escapes_html_in_detail() {
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("echo <hi> & bye");
        let html = super::render_breadcrumb_html(&bc);
        assert!(html.contains("echo &lt;hi&gt; &amp; bye"), "got: {html}");
    }

    #[test]
    fn render_diff_markdown_v2_wraps_body_in_diff_fence_with_header_totals() {
        let card = ironclaw_channels_core::DiffCard {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            hunks: vec![ironclaw_channels_core::DiffHunk {
                old_start: 10,
                old_lines: 1,
                new_start: 10,
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
        let out = super::render_diff_markdown_v2(&card);
        assert!(out.contains("*src/lib\\.rs*"), "path bolded + escaped: {out}");
        assert!(out.contains("\\+1 / \\-1"), "totals header: {out}");
        assert!(out.contains("```diff\n"));
        assert!(out.contains("@@ -10,1 +10,1 @@"));
        assert!(out.contains("-let x = 1;"));
        assert!(out.contains("+let x = 2;"));
        assert!(out.trim_end().ends_with("```"), "must close fence: {out}");
    }

    #[test]
    fn render_diff_markdown_v2_replaces_backticks_inside_fenced_block() {
        let card = ironclaw_channels_core::DiffCard {
            path: "x.rs".into(),
            language: None,
            hunks: vec![ironclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                lines: vec![ironclaw_channels_core::DiffLine {
                    kind: ironclaw_channels_core::DiffLineKind::Add,
                    text: "let x = `raw`;".into(),
                }],
            }],
            added: 1,
            removed: 0,
            truncated: false,
        };
        let out = super::render_diff_markdown_v2(&card);
        // The opening + closing fences are the only literal backticks
        // that should remain — three at the start of the line and three
        // at the very end.
        let inside_backticks = out.matches('`').count();
        assert_eq!(inside_backticks, 6, "exactly two fences: {out}");
        // The body's backticks must have been swapped for the prime
        // glyph (U+2032).
        assert!(out.contains("let x = \u{2032}raw\u{2032};"), "got: {out}");
    }

    #[tokio::test]
    async fn deliver_diff_sends_markdownv2_fenced_block_via_send_message() {
        // The native diff renderer routes through sendMessage with
        // parse_mode=MarkdownV2 and a ` ```diff … ``` ` body.
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "message_id": 9999, "chat": { "id": 1, "type": "private" }, "date": 0 }
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let card = ironclaw_channels_core::DiffCard {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            hunks: vec![ironclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    ironclaw_channels_core::DiffLine {
                        kind: ironclaw_channels_core::DiffLineKind::Remove,
                        text: "fn old() {}".into(),
                    },
                    ironclaw_channels_core::DiffLine {
                        kind: ironclaw_channels_core::DiffLineKind::Add,
                        text: "fn new() {}".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let id = adapter.deliver_diff("chat-1", None, &card).await.unwrap();
        assert_eq!(id.as_deref(), Some("9999"));
        let reqs = s.received_requests().await.unwrap();
        let sm = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage");
        let body: serde_json::Value = serde_json::from_slice(&sm.body).unwrap();
        assert_eq!(body["parse_mode"], "MarkdownV2");
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("```diff\n"), "got: {text}");
        assert!(text.contains("-fn old() {}"));
        assert!(text.contains("+fn new() {}"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_breadcrumb_running_sends_html_chip_via_send_message() {
        // First emit (existing_message_id = None) should hit sendMessage
        // with parse_mode=HTML and the `<code>` chip body.
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "message_id": 4242, "chat": { "id": 1, "type": "private" }, "date": 0 }
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let id = adapter
            .deliver_breadcrumb("chat-1", None, &bc, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("4242"));
        let reqs = s.received_requests().await.unwrap();
        let sm = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body: serde_json::Value = serde_json::from_slice(&sm.body).unwrap();
        assert_eq!(body["parse_mode"], "HTML");
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("<code>shell</code>"), "got: {text}");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_breadcrumb_with_existing_message_id_edits_in_place() {
        // existing_message_id = Some(..) → editMessageText with the
        // finished chip body, preserving the platform message id.
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/editMessageText"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "message_id": 4242, "chat": { "id": 1, "type": "private" }, "date": 0 }
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let bc = ironclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let id = adapter
            .deliver_breadcrumb("chat-1", None, &bc, Some("4242"))
            .await
            .unwrap();
        // Returns the existing id so subsequent edits keep targeting
        // the same chip.
        assert_eq!(id.as_deref(), Some("4242"));
        let reqs = s.received_requests().await.unwrap();
        let edit = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/editMessageText"))
            .expect("editMessageText request");
        let body: serde_json::Value = serde_json::from_slice(&edit.body).unwrap();
        assert_eq!(body["message_id"], 4242);
        assert_eq!(body["parse_mode"], "HTML");
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("passed (0.4s)"));
        assert!(text.starts_with("[ok]"), "got: {text}");
        adapter.shutdown().await;
    }

    // ── Error card rendering ───────────────────────────────────────

    #[test]
    fn render_error_html_uses_bold_label_for_each_kind() {
        // The kind label varies — internal/provider/delivery each get
        // a distinct human-readable prefix so the user can tell which
        // pipeline stage failed.
        for (kind, expected_label) in [
            (
                ironclaw_channels_core::ErrorCardKind::Internal,
                "Tool error",
            ),
            (
                ironclaw_channels_core::ErrorCardKind::Provider,
                "Provider error",
            ),
            (
                ironclaw_channels_core::ErrorCardKind::Delivery,
                "Delivery error",
            ),
        ] {
            let card = ironclaw_channels_core::ErrorCard::new(kind, "boom");
            let html = super::render_error_html(&card);
            assert!(
                html.starts_with(&format!("<b>{expected_label}:</b>")),
                "{kind:?} → {html}"
            );
        }
    }

    #[test]
    fn render_error_html_wraps_details_in_pre_block() {
        // Stderr / traceback goes in `<pre>…</pre>` so monospace
        // survives Telegram's HTML pipeline.
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Internal,
            "the shell tool timed out",
        )
        .with_details("stderr: SIGKILL\nexit code 137");
        let html = super::render_error_html(&card);
        assert!(html.contains("<pre>"), "expected <pre> block: {html}");
        assert!(html.contains("SIGKILL"));
        assert!(html.contains("</pre>"));
    }

    #[test]
    fn render_error_html_appends_retry_footer_when_retryable() {
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Delivery,
            "telegram returned 502 — backing off",
        )
        .retryable();
        let html = super::render_error_html(&card);
        assert!(
            html.contains("<i>will retry automatically</i>"),
            "retryable footer missing: {html}"
        );
    }

    #[test]
    fn render_error_html_escapes_user_content() {
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Internal,
            "bad input: <script>alert(1)</script>",
        )
        .with_title("attack? & escape")
        .with_details("payload = <evil>");
        let html = super::render_error_html(&card);
        // Bare `<script>` would survive Telegram's HTML parse and
        // confuse the renderer; we have to escape every angle bracket.
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("attack? &amp; escape"));
        assert!(html.contains("&lt;evil&gt;"));
    }

    #[tokio::test]
    async fn deliver_error_sends_html_card_via_send_message() {
        // End-to-end: deliver_error must POST to /sendMessage with
        // parse_mode=HTML and the rendered card body. No call to
        // editMessageText — error receipts are immutable.
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "message_id": 9001, "chat": { "id": 1, "type": "private" }, "date": 0 }
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let card = ironclaw_channels_core::ErrorCard::new(
            ironclaw_channels_core::ErrorCardKind::Provider,
            "the model failed to produce a complete response after 3 retries",
        )
        .with_details("anthropic 502 bad gateway");
        let id = adapter
            .deliver_error("chat-1", None, &card)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("9001"));
        let reqs = s.received_requests().await.unwrap();
        let sm = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body: serde_json::Value = serde_json::from_slice(&sm.body).unwrap();
        assert_eq!(body["parse_mode"], "HTML");
        let text = body["text"].as_str().unwrap();
        assert!(text.starts_with("<b>Provider error:</b>"), "got: {text}");
        assert!(text.contains("<pre>"), "expected details pre block: {text}");
        adapter.shutdown().await;
    }

    // ── Long-output expander (slice 3.4) rendering ──────────────────

    #[test]
    fn render_collapsible_html_wraps_body_in_expandable_blockquote() {
        // The native Telegram primitive for this surface is
        // `<blockquote expandable>` (Bot API 7.6+). The summary
        // rides outside the blockquote so the user can see what
        // they'd be expanding before they tap.
        let body = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let html = super::render_collapsible_html(&body, "shell 40 lines (250 B)");
        // Summary shows up in `<i>…</i>` at the top.
        assert!(html.starts_with("<i>shell 40 lines (250 B)</i>\n"), "got: {html}");
        // Full body lives inside the expandable blockquote.
        assert!(html.contains("<blockquote expandable>"), "got: {html}");
        assert!(html.ends_with("</blockquote>"), "got: {html}");
        assert!(html.contains("line 0"));
        assert!(html.contains("line 39"));
    }

    #[test]
    fn render_collapsible_html_escapes_html_in_body() {
        // Tool output (especially `web_fetch`, `read_file` on HTML
        // sources) can contain literal `<` and `&` — these must be
        // entity-encoded or Telegram rejects the parse.
        let body = "<script>alert(\"xss\")</script>\n& <div>";
        let html = super::render_collapsible_html(body, "fetched 1 page");
        assert!(!html.contains("<script>"), "raw <script> not escaped: {html}");
        assert!(html.contains("&lt;script&gt;"), "got: {html}");
        assert!(html.contains("&amp;"), "got: {html}");
    }

    #[test]
    fn render_collapsible_html_omits_summary_when_blank() {
        // Defensive: an empty summary shouldn't leak an empty `<i>`
        // tag at the top of the message.
        let html = super::render_collapsible_html("body text", "");
        assert!(!html.starts_with("<i>"));
        assert!(html.starts_with("<blockquote expandable>"));
    }

    #[tokio::test]
    async fn deliver_collapsible_sends_blockquote_via_send_message() {
        // End-to-end against the wiremock server: the adapter's
        // `deliver_collapsible` must hit `sendMessage` with HTML
        // parse_mode and the `<blockquote expandable>` body.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"ok": true, "result": {"message_id": 42}})
            ))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let body = (0..35)
            .map(|i| format!("out line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview: Vec<String> = (0..3).map(|i| format!("out line {i}")).collect();
        let id = adapter
            .deliver_collapsible("100", None, &body, "shell 35 lines", &preview)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("42"));
        let reqs = s.received_requests().await.unwrap();
        let sm = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let payload: serde_json::Value = serde_json::from_slice(&sm.body).unwrap();
        assert_eq!(payload["parse_mode"], "HTML");
        let text = payload["text"].as_str().unwrap();
        assert!(text.starts_with("<i>shell 35 lines</i>"), "got: {text}");
        assert!(text.contains("<blockquote expandable>"), "got: {text}");
        assert!(text.contains("out line 0"));
        assert!(text.contains("out line 34"));
        adapter.shutdown().await;
    }

    // ── Thinking block (slice 3.5) rendering ────────────────────────

    #[test]
    fn render_thinking_html_wraps_body_in_expandable_blockquote() {
        // Native Telegram primitive for this surface is the same
        // `<blockquote expandable>` surface 4 uses for long output —
        // every mainstream client renders it collapsed by default with
        // a "Show more" widget. The `<i>reasoning</i>` italic prefix
        // rides outside the blockquote so the user can identify the
        // block without expanding it.
        let t = ThinkingBlock::visible("The user is asking about X. I should…");
        let html = super::render_thinking_html(&t);
        assert!(html.starts_with("<i>reasoning</i>\n"), "got: {html}");
        assert!(html.contains("<blockquote expandable>"), "got: {html}");
        assert!(html.ends_with("</blockquote>"), "got: {html}");
        assert!(html.contains("The user is asking about X. I should…"));
    }

    #[test]
    fn render_thinking_html_includes_model_provenance_in_label() {
        // When a model tag is attached, it shows up parenthesised in
        // the italic prefix so the user can disambiguate which model
        // produced the reasoning (groups can fan out across several).
        let t = ThinkingBlock::visible("reasoning text").with_model("claude-opus-4-7");
        let html = super::render_thinking_html(&t);
        assert!(
            html.starts_with("<i>reasoning (claude-opus-4-7)</i>\n"),
            "got: {html}"
        );
    }

    #[test]
    fn render_thinking_html_escapes_html_in_body() {
        // Thinking text is free-form model prose — it can absolutely
        // contain literal `<`, `&`, etc. The renderer MUST entity-
        // encode them or Telegram rejects the parse.
        let t = ThinkingBlock::visible("<script>alert(\"x\")</script>");
        let html = super::render_thinking_html(&t);
        assert!(!html.contains("<script>"), "raw <script> not escaped: {html}");
        assert!(html.contains("&lt;script&gt;"), "got: {html}");
    }

    #[test]
    fn render_thinking_html_redacted_substitutes_placeholder() {
        // Privacy contract: redacted blocks must never put the raw
        // blob on the wire — even on Telegram, even in 1:1 DMs.
        let t = ThinkingBlock::redacted("opaque-secret-blob");
        let html = super::render_thinking_html(&t);
        assert!(
            html.contains("(redacted reasoning)"),
            "expected placeholder body, got: {html}"
        );
        assert!(
            !html.contains("opaque-secret-blob"),
            "raw redacted blob leaked: {html}"
        );
    }

    #[tokio::test]
    async fn deliver_thinking_sends_blockquote_via_send_message() {
        // End-to-end against the wiremock server: the adapter's
        // `deliver_thinking` must hit `sendMessage` with HTML
        // parse_mode and the `<blockquote expandable>` body.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"ok": true, "result": {"message_id": 99}}),
            ))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let t = ThinkingBlock::visible("a chain of thought").with_model("claude-opus-4-7");
        let id = adapter
            .deliver_thinking("100", None, &t)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("99"));
        let reqs = s.received_requests().await.unwrap();
        let sm = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let payload: serde_json::Value = serde_json::from_slice(&sm.body).unwrap();
        assert_eq!(payload["parse_mode"], "HTML");
        let text = payload["text"].as_str().unwrap();
        assert!(text.starts_with("<i>reasoning (claude-opus-4-7)</i>"), "got: {text}");
        assert!(text.contains("<blockquote expandable>"), "got: {text}");
        assert!(text.contains("a chain of thought"));
        adapter.shutdown().await;
    }

    // ── TodoList chip rendering ────────────────────────────────────

    fn todo_list_sample() -> ironclaw_channels_core::TodoList {
        ironclaw_channels_core::TodoList {
            items: vec![
                ironclaw_channels_core::TodoListItem {
                    id: 1,
                    text: "Wash dishes".into(),
                    status: ironclaw_channels_core::TodoItemStatus::Completed,
                },
                ironclaw_channels_core::TodoListItem {
                    id: 2,
                    text: "Dry dishes".into(),
                    status: ironclaw_channels_core::TodoItemStatus::InProgress,
                },
                ironclaw_channels_core::TodoListItem {
                    id: 3,
                    text: "Put dishes away".into(),
                    status: ironclaw_channels_core::TodoItemStatus::Pending,
                },
            ],
            title: Some("Kitchen".into()),
        }
    }

    #[test]
    fn render_todo_list_markdown_includes_title_glyphs_and_footer() {
        let body = super::render_todo_list_markdown(&todo_list_sample());
        assert!(body.starts_with("*Kitchen*\n"), "title bold: {body}");
        // The `[` and `]` in the ASCII markers MUST be backslash-escaped
        // — MarkdownV2 reserves both brackets. Regression caught live
        // 2026-05-24: unescaped `[x]` returned 400 from Telegram and
        // cascaded into endless agent retries.
        assert!(body.contains(r"\[x\] ~Wash dishes~"), "done strikethrough: {body}");
        // `~` is also MarkdownV2-reserved (strikethrough marker), so
        // the `[~]` glyph becomes `\[\~\]` after escaping.
        assert!(body.contains(r"\[\~\] Dry dishes"), "in-progress: {body}");
        assert!(body.contains(r"\[ \] Put dishes away"), "pending: {body}");
        assert!(body.ends_with("done_"), "footer: {body}");
        assert!(body.contains("1/3 done"));
    }

    #[test]
    fn render_todo_list_markdown_escapes_user_text() {
        let list = ironclaw_channels_core::TodoList {
            items: vec![ironclaw_channels_core::TodoListItem {
                id: 1,
                text: "ship it!".into(),
                status: ironclaw_channels_core::TodoItemStatus::Pending,
            }],
            title: None,
        };
        let body = super::render_todo_list_markdown(&list);
        // MarkdownV2 reserves `!` — must be backslash-escaped.
        assert!(body.contains("ship it\\!"), "escape: {body}");
    }

    #[tokio::test]
    async fn deliver_todo_list_first_emit_posts_and_pins() {
        let s = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "message_id": 555 }
            })))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/bottok/pinChatMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": true
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let id = adapter
            .deliver_todo_list("100", None, &todo_list_sample(), None, true)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("555"));
        let reqs = s.received_requests().await.unwrap();
        let pin = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/pinChatMessage"))
            .expect("expected pin call on first emit");
        let body: serde_json::Value = serde_json::from_slice(&pin.body).unwrap();
        assert_eq!(body["chat_id"], "100");
        assert_eq!(body["message_id"], 555);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_todo_list_with_existing_id_edits_in_place() {
        let s = wiremock::MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/editMessageText"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "message_id": 555 }
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let cfg = lp_config(&s.uri());
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let dir = temp_dir();
        let adapter =
            TelegramAdapter::start_with_api(api, cfg, None, tx, dir.path().to_path_buf())
                .await
                .unwrap();
        let id = adapter
            .deliver_todo_list("100", None, &todo_list_sample(), Some("555"), false)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("555"));
        let reqs = s.received_requests().await.unwrap();
        // Edit was called; no new sendMessage / pinChatMessage.
        assert!(reqs.iter().any(|r| r.url.path().ends_with("/editMessageText")));
        assert!(!reqs.iter().any(|r| r.url.path().ends_with("/sendMessage")));
        assert!(!reqs.iter().any(|r| r.url.path().ends_with("/pinChatMessage")));
        adapter.shutdown().await;
    }
}
