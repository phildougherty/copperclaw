//! [`TelegramAdapter`] — the [`ChannelAdapter`] implementation.
//!
//! Owns the [`TelegramApi`] client, the cancellation token used to stop
//! background tasks, and the resolved [`TelegramConfig`].

use crate::api::TelegramApi;
use crate::config::{IngressMode, TelegramConfig};
use crate::ingress::{IngressSettings, long_poll, webhook};
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
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

}
