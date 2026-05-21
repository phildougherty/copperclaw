//! [`XAdapter`] — [`ChannelAdapter`] implementation for Twitter / X DMs.
//!
//! The adapter:
//!
//! - Owns a shared [`XApi`] HTTP client and the bot's numeric user id.
//! - Runs a background `dm_events` poll task (see [`crate::poll`]) that
//!   pushes inbound events into the host-supplied mpsc channel.
//! - Routes `deliver` based on the `platform_id` prefix:
//!     - `"user:<id>"` posts to `/2/dm_conversations/with/<id>/messages`,
//!     - `"conversation:<id>"` posts to `/2/dm_conversations/<id>/messages`.
//! - Translates `OutboundMessage.files` into v1.1 media uploads followed by
//!   DM sends (one DM per file, with `text` on the first only).
//! - Returns [`AdapterError::Unsupported`] for `edit` and `reaction`
//!   actions: X DMs do not expose either via API.
//! - Implements `set_typing` as a no-op — X v2 has no public typing API for
//!   DMs.

use crate::api::XApi;
use crate::config::XConfig;
use crate::factory::CHANNEL_TYPE_STR;
use crate::poll::run_poll_loop;
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, InboundEvent, OutboundFile, OutboundMessage};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Parsed shape of [`OutboundMessage::content`] addressing a single
/// platform endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    /// `platform_id = "user:<participant_id>"`.
    User(&'a str),
    /// `platform_id = "conversation:<dm_conversation_id>"`.
    Conversation(&'a str),
}

impl<'a> Target<'a> {
    /// Parse a `platform_id` into a [`Target`].
    pub fn parse(platform_id: &'a str) -> Result<Self, AdapterError> {
        if let Some(rest) = platform_id.strip_prefix("user:") {
            if rest.is_empty() {
                return Err(AdapterError::BadRequest(
                    "x platform_id `user:` requires a participant id".into(),
                ));
            }
            return Ok(Self::User(rest));
        }
        if let Some(rest) = platform_id.strip_prefix("conversation:") {
            if rest.is_empty() {
                return Err(AdapterError::BadRequest(
                    "x platform_id `conversation:` requires a conversation id".into(),
                ));
            }
            return Ok(Self::Conversation(rest));
        }
        Err(AdapterError::BadRequest(format!(
            "x platform_id must start with `user:` or `conversation:`, got `{platform_id}`"
        )))
    }
}

/// Twitter / X DM channel adapter.
pub struct XAdapter {
    channel_type: ChannelType,
    api: Arc<XApi>,
    bot_user_id: String,
    config: XConfig,
    poll_handle: Mutex<Option<JoinHandle<()>>>,
    cancel: CancellationToken,
}

impl XAdapter {
    /// Build an adapter and start its `dm_events` polling task.
    #[allow(clippy::needless_pass_by_value)]
    pub fn start(
        config: XConfig,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let api = Arc::new(XApi::with_client(
            reqwest::Client::new(),
            config.api_base.clone(),
            config.media_base.clone(),
            config.bearer_token.clone(),
        ));
        Self::start_with_api(api, config, inbound_tx, data_dir)
    }

    /// Build an adapter with a caller-supplied [`XApi`] (for tests).
    #[allow(clippy::needless_pass_by_value)]
    pub fn start_with_api(
        api: Arc<XApi>,
        config: XConfig,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let state_path = data_dir.join(&config.since_id_filename);
        let cancel = CancellationToken::new();
        let bot_user_id = config.user_id.clone();
        let handle = tokio::spawn(run_poll_loop(
            api.clone(),
            state_path,
            inbound_tx,
            bot_user_id.clone(),
            config.poll_interval_ms,
            cancel.clone(),
        ));
        Ok(Arc::new(Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            bot_user_id,
            api,
            config,
            poll_handle: Mutex::new(Some(handle)),
            cancel,
        }))
    }

    /// Configured X config.
    pub fn config(&self) -> &XConfig {
        &self.config
    }

    /// Shared HTTP client.
    pub fn api(&self) -> &Arc<XApi> {
        &self.api
    }

    /// Bot's numeric user id.
    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Stop the poll task and wait for it to finish. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.poll_handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    async fn upload_files(&self, files: &[OutboundFile]) -> Result<Vec<String>, AdapterError> {
        let mut ids = Vec::with_capacity(files.len());
        for file in files {
            let category = media_category_for(&file.filename);
            let resp = self.api.upload_media(&file.data, category).await?;
            ids.push(resp.media_id_string);
        }
        Ok(ids)
    }

    async fn send_one(
        &self,
        target: &Target<'_>,
        text: &str,
        media_ids: &[String],
    ) -> Result<String, AdapterError> {
        let resp = match target {
            Target::User(id) => self.api.dm_send_to_user(id, text, media_ids).await?,
            Target::Conversation(id) => {
                self.api.dm_send_to_conversation(id, text, media_ids).await?
            }
        };
        Ok(resp.dm_event_id)
    }
}

impl std::fmt::Debug for XAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XAdapter")
            .field("channel_type", &self.channel_type)
            .field("bot_user_id", &self.bot_user_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChannelAdapter for XAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        false
    }

    async fn set_typing(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // X v2 has no public DM typing-indicator API; intentional no-op.
        Ok(())
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let target = Target::parse(platform_id)?;

        if let Some(action) = extract_action(&message.content) {
            return Err(AdapterError::Unsupported(format!(
                "x DMs do not support `{action}` actions"
            )));
        }

        let text = extract_text(&message.content);

        if message.files.is_empty() {
            if text.is_empty() {
                return Err(AdapterError::BadRequest(
                    "x deliver requires `text` or at least one file".into(),
                ));
            }
            return Ok(Some(self.send_one(&target, &text, &[]).await?));
        }

        // One DM per file. The first carries `text`; subsequent DMs use the
        // filename as a placeholder body since X requires a non-empty
        // `text` field on every send.
        let mut last_id: Option<String> = None;
        for (idx, file) in message.files.iter().enumerate() {
            let media_ids = self.upload_files(std::slice::from_ref(file)).await?;
            let body = if idx == 0 && !text.is_empty() {
                text.clone()
            } else {
                file.filename.clone()
            };
            let event_id = self.send_one(&target, &body, &media_ids).await?;
            last_id = Some(event_id);
        }
        Ok(last_id)
    }

    async fn open_dm(&self, user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        if user_id.is_empty() {
            return Err(AdapterError::BadRequest(
                "x open_dm requires a non-empty user id".into(),
            ));
        }
        Ok(Some(DmHandle {
            user_id: user_id.to_owned(),
            platform_id: format!("user:{user_id}"),
            channel_type: self.channel_type.clone(),
        }))
    }
}

fn extract_action(content: &Value) -> Option<&str> {
    content.get("action").and_then(Value::as_str)
}

fn extract_text(value: &Value) -> String {
    value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Pick a `media_category` value for v1.1 media upload based on the
/// filename's extension. We default to `"dm_image"` which is the most
/// permissive bucket; videos / GIFs are bucketed appropriately.
fn media_category_for(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or_default();
    match ext {
        "gif" => "dm_gif",
        "mp4" | "mov" | "webm" => "dm_video",
        _ => "dm_image",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::{MessageKind, OutboundFile};
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build_adapter(
        server_url: &str,
    ) -> (Arc<XAdapter>, TempDir, mpsc::Receiver<InboundEvent>) {
        let dir = TempDir::new().unwrap();
        let api = Arc::new(XApi::with_client(
            reqwest::Client::new(),
            server_url.to_owned(),
            server_url.to_owned(),
            "tok".to_owned(),
        ));
        let config = XConfig {
            bearer_token: "tok".into(),
            user_id: "bot".into(),
            api_base: server_url.to_owned(),
            media_base: server_url.to_owned(),
            since_id_filename: "x_dm_since_id.txt".into(),
            poll_interval_ms: 50_000,
        };
        let (tx, rx) = mpsc::channel::<InboundEvent>(8);
        let adapter =
            XAdapter::start_with_api(api, config, tx, dir.path().to_path_buf()).unwrap();
        (adapter, dir, rx)
    }

    async fn mount_empty_poll(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "meta": {}
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn channel_type_is_x_and_threads_are_not_supported() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert_eq!(adapter.channel_type().as_str(), "x");
        assert!(!adapter.supports_threads());
        assert_eq!(adapter.bot_user_id(), "bot");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_user_prefix_posts_to_with_endpoint() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        Mock::given(method("POST"))
            .and(path("/2/dm_conversations/with/222/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "dm_conversation_id": "c", "dm_event_id": "ev1" }
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "user:222",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "hello" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("ev1"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_conversation_prefix_posts_to_conversation_endpoint() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        Mock::given(method("POST"))
            .and(path("/2/dm_conversations/abc/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "dm_conversation_id": "abc", "dm_event_id": "ev2" }
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "conversation:abc",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "in conv" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("ev2"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_malformed_platform_id_is_bad_request() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "bogus",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_empty_user_id_is_bad_request() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "user:",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_empty_conversation_id_is_bad_request() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "conversation:",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_no_text_and_no_files_is_bad_request() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "user:1",
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
    async fn deliver_single_file_uploads_then_sends() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        Mock::given(method("POST"))
            .and(path("/1.1/media/upload.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "media_id_string": "mid-1"
            })))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/2/dm_conversations/with/222/messages"))
            .and(wiremock::matchers::body_string_contains("\"mid-1\""))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "dm_conversation_id": "c", "dm_event_id": "ev-f" }
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "user:222",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "caption" }),
                    files: vec![OutboundFile {
                        filename: "pic.png".into(),
                        data: vec![1, 2, 3],
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("ev-f"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_multi_file_sends_one_dm_per_file_and_returns_last_id() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        Mock::given(method("POST"))
            .and(path("/1.1/media/upload.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "media_id_string": "mid-x"
            })))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/2/dm_conversations/with/.+/messages$"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": { "dm_conversation_id": "c", "dm_event_id": "ev-last" }
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "user:222",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "with two attachments" }),
                    files: vec![
                        OutboundFile {
                            filename: "a.png".into(),
                            data: vec![1],
                        },
                        OutboundFile {
                            filename: "b.png".into(),
                            data: vec![2],
                        },
                    ],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("ev-last"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_action_is_unsupported() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "user:1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "edit", "target_seq": 1, "text": "new"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(m) if m.contains("edit")));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_action_is_unsupported() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "user:1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "reaction", "target_seq": 1, "emoji": "+1"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(m) if m.contains("reaction")));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn set_typing_is_a_noop() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.set_typing("user:1", None).await.unwrap();
        adapter.set_typing("conversation:c", Some("t")).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_returns_user_prefixed_handle() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let handle = adapter.open_dm("12345").await.unwrap().unwrap();
        assert_eq!(handle.platform_id, "user:12345");
        assert_eq!(handle.channel_type.as_str(), "x");
        assert_eq!(handle.user_id, "12345");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_empty_user_id_is_bad_request() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter.open_dm("").await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.shutdown().await;
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn debug_format_renders() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let s = format!("{adapter:?}");
        assert!(s.contains("XAdapter"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn config_and_api_accessors() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert_eq!(adapter.config().user_id, "bot");
        assert_eq!(adapter.api().bearer_token(), "tok");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_default_returns_ok() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.subscribe("user:1", None).await.unwrap();
        adapter.shutdown().await;
    }

    #[test]
    fn target_parse_user() {
        assert_eq!(Target::parse("user:42").unwrap(), Target::User("42"));
    }

    #[test]
    fn target_parse_conversation() {
        assert_eq!(
            Target::parse("conversation:abc").unwrap(),
            Target::Conversation("abc")
        );
    }

    #[test]
    fn target_parse_bare_string_is_bad_request() {
        let err = Target::parse("just-a-string").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn target_parse_empty_user_is_bad_request() {
        let err = Target::parse("user:").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn target_parse_empty_conversation_is_bad_request() {
        let err = Target::parse("conversation:").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn media_category_for_known_extensions() {
        assert_eq!(media_category_for("a.gif"), "dm_gif");
        assert_eq!(media_category_for("a.GIF"), "dm_gif");
        assert_eq!(media_category_for("a.mp4"), "dm_video");
        assert_eq!(media_category_for("a.MOV"), "dm_video");
        assert_eq!(media_category_for("a.webm"), "dm_video");
        assert_eq!(media_category_for("a.png"), "dm_image");
        assert_eq!(media_category_for("a.jpg"), "dm_image");
        assert_eq!(media_category_for("noext"), "dm_image");
    }

    #[test]
    fn extract_text_and_action() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({"text": "x"})), "x");
        assert_eq!(extract_action(&json!({"action": "edit"})), Some("edit"));
        assert_eq!(extract_action(&json!({})), None);
    }
}
