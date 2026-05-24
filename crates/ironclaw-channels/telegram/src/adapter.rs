//! [`TelegramAdapter`] — the [`ChannelAdapter`] implementation.
//!
//! Owns the [`TelegramApi`] client, the cancellation token used to stop
//! background tasks, and the resolved [`TelegramConfig`].

use crate::api::{InlineKeyboardButton, TelegramApi, escape_markdown_v2};
use crate::config::{IngressMode, TelegramConfig};
use crate::ingress::{IngressSettings, long_poll, webhook};
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, Card, ChannelAdapter, DmHandle};
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
/// payload. Empty string means "no parse mode" — plain text passes through
/// unmodified. The agent can opt into a specific mode by setting
/// `content.parse_mode = "MarkdownV2"` (or `Markdown` / `HTML`) on the
/// outbound row; that path requires the agent to escape Telegram's reserved
/// characters itself.
///
/// Why plain text by default: the agent generates natural-language replies
/// that contain bare `!`, `.`, `-`, `(`, `)`, `[`, `]` etc.  In `MarkdownV2`
/// every one of those is reserved and Telegram rejects the send with HTTP
/// 400 ("can't parse entities") unless the agent backslash-escapes them.
/// Plain text round-trips literally and lets the agent be Telegram-agnostic.
pub const DEFAULT_PARSE_MODE: &str = "";

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
        // DEFAULT_PARSE_MODE — empty string means "send as plain text" so
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
        let m = self
            .api
            .send_message(platform_id, thread_id, &text, mode_for_api)
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
        // Empty string = plain text (no parse_mode field on the API call).
        // The agent opts into MarkdownV2 / HTML / Markdown by setting
        // `content.parse_mode` on the outbound row.
        assert_eq!(DEFAULT_PARSE_MODE, "");
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
    async fn deliver_text_omits_parse_mode_by_default() {
        // Regression: telegram bot API rejects un-escaped `!` / `.` /
        // `-` / `(` / `)` / `[` / `]` etc. when parse_mode=MarkdownV2.
        // The default must NOT set parse_mode so the agent's natural-
        // language replies round-trip literally.
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
                    content: json!({ "text": "hey! yes!" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("777"));
        // Belt and braces: scan the recorded requests for an absent
        // parse_mode field. wiremock's body_string_contains alone is
        // necessary-but-not-sufficient — confirm the field is missing.
        let reqs = s.received_requests().await.unwrap();
        let send_msg_req = reqs
            .iter()
            .find(|r| r.url.path().ends_with("/sendMessage"))
            .expect("sendMessage request");
        let body = String::from_utf8_lossy(&send_msg_req.body);
        assert!(
            !body.contains("parse_mode"),
            "default deliver must omit parse_mode but body was: {body:?}"
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
}
