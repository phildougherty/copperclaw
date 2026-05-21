//! [`MatrixAdapter`] — the [`ChannelAdapter`] implementation.
//!
//! Owns a shared [`MatrixApi`] client, an `RwLock<HashSet>` of subscribed
//! rooms (used by the `/sync` filter), the per-instance access token, and
//! the cancellation token used to stop the background `/sync` task.

use crate::api::MatrixApi;
use crate::config::MatrixConfig;
use crate::factory::CHANNEL_TYPE_STR;
use crate::sync::{NEXT_BATCH_FILENAME, run_sync_loop};
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, InboundEvent, OutboundMessage};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Matrix channel adapter.
pub struct MatrixAdapter {
    channel_type: ChannelType,
    api: Arc<MatrixApi>,
    bot_user_id: String,
    config: MatrixConfig,
    rooms: Arc<RwLock<HashSet<String>>>,
    /// In-memory cache of alias → room id resolutions.
    alias_cache: Mutex<HashMap<String, String>>,
    sync_handle: Mutex<Option<JoinHandle<()>>>,
    cancel: CancellationToken,
}

impl MatrixAdapter {
    /// Build an adapter and start its `/sync` background task.
    ///
    /// `data_dir` is the per-channel directory the host provides; the
    /// adapter persists its `next_batch` token in `data_dir/next_batch.txt`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn start(
        config: MatrixConfig,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let api = Arc::new(MatrixApi::with_client(
            reqwest::Client::new(),
            config.homeserver_url.clone(),
            config.access_token.clone(),
            config.txn_prefix.clone(),
        ));
        Self::start_with_api(api, config, inbound_tx, data_dir)
    }

    /// As [`start`] but with a caller-supplied [`MatrixApi`] (for tests).
    #[allow(clippy::needless_pass_by_value)]
    pub fn start_with_api(
        api: Arc<MatrixApi>,
        config: MatrixConfig,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Result<Arc<Self>, AdapterError> {
        let rooms: HashSet<String> = config.rooms.iter().cloned().collect();
        let rooms = Arc::new(RwLock::new(rooms));
        let cancel = CancellationToken::new();
        let state_path = data_dir.join(NEXT_BATCH_FILENAME);
        let handle = tokio::spawn(run_sync_loop(
            api.clone(),
            state_path,
            inbound_tx,
            config.user_id.clone(),
            rooms.clone(),
            config.sync_timeout_ms,
            cancel.clone(),
        ));
        Ok(Arc::new(Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            bot_user_id: config.user_id.clone(),
            api,
            config,
            rooms,
            alias_cache: Mutex::new(HashMap::new()),
            sync_handle: Mutex::new(Some(handle)),
            cancel,
        }))
    }

    /// Configured Matrix config.
    pub fn config(&self) -> &MatrixConfig {
        &self.config
    }

    /// Shared HTTP client.
    pub fn api(&self) -> &Arc<MatrixApi> {
        &self.api
    }

    /// Bot's Matrix user id.
    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Set of rooms the `/sync` filter is currently configured for. Clones
    /// the inner set; primarily useful for tests.
    pub async fn subscribed_rooms(&self) -> HashSet<String> {
        self.rooms.read().await.clone()
    }

    /// Stop the `/sync` task and wait for it to finish. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.sync_handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    /// Resolve an alias to a canonical room id, caching the result.
    async fn resolve_room(&self, identifier: &str) -> Result<String, AdapterError> {
        if identifier.starts_with('!') {
            return Ok(identifier.to_owned());
        }
        if let Some(cached) = self.alias_cache.lock().await.get(identifier).cloned() {
            return Ok(cached);
        }
        let resolved = self.api.resolve_alias(identifier).await?;
        self.alias_cache
            .lock()
            .await
            .insert(identifier.to_owned(), resolved.room_id.clone());
        Ok(resolved.room_id)
    }
}

impl std::fmt::Debug for MatrixAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixAdapter")
            .field("channel_type", &self.channel_type)
            .field("bot_user_id", &self.bot_user_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChannelAdapter for MatrixAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    async fn subscribe(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        self.rooms.write().await.insert(room_id);
        Ok(())
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;
        self.api.typing(&room_id, &self.bot_user_id, true).await
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let room_id = self.resolve_room(platform_id).await?;

        // System-action shape: { "action": "edit"|"reaction", ... }
        if let Some(action) = extract_action(&message.content) {
            return self
                .deliver_action(&room_id, action, &message.content)
                .await;
        }

        let text = extract_text(&message.content);
        let html = extract_html(&message.content);

        // Files first; the first file picks up `text` as its body (if any),
        // and subsequent files use their filename.
        if !message.files.is_empty() {
            let mut last_event_id: Option<String> = None;
            for (i, file) in message.files.iter().enumerate() {
                let mime = guess_mime(&file.filename);
                let upload = self
                    .api
                    .upload_media(&file.filename, &mime, file.data.clone())
                    .await?;
                let msgtype = msgtype_for_mime(&mime);
                let body = if i == 0 && !text.is_empty() {
                    text.as_str()
                } else {
                    file.filename.as_str()
                };
                let sent = self
                    .api
                    .send_media_message(
                        &room_id,
                        msgtype,
                        body,
                        &upload.content_uri,
                        &mime,
                        file.data.len(),
                        thread_id,
                    )
                    .await?;
                last_event_id = Some(sent.event_id);
            }
            return Ok(last_event_id);
        }

        if text.is_empty() && html.is_none() {
            return Err(AdapterError::BadRequest(
                "matrix deliver requires `text`, `html`, or files".into(),
            ));
        }

        let sent = if let Some(html) = html {
            // If the user supplied HTML the plain-text fallback is either
            // explicit `text` or the html stripped down. Use `text` if
            // present, otherwise the same html.
            let plain = if text.is_empty() { html.as_str() } else { text.as_str() };
            self.api.send_html(&room_id, plain, &html).await?
        } else if let Some(thread) = thread_id {
            self.api.send_threaded(&room_id, thread, &text).await?
        } else {
            self.api.send_text(&room_id, &text).await?
        };
        Ok(Some(sent.event_id))
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Matrix has no separate DM concept at the protocol level; callers
        // must configure the room and pass it as `platform_id`.
        Ok(None)
    }
}

impl MatrixAdapter {
    async fn deliver_action(
        &self,
        room_id: &str,
        action: &str,
        content: &Value,
    ) -> Result<Option<String>, AdapterError> {
        match action {
            "edit" => {
                let target = required_str(content, "target_event_id").or_else(|_| {
                    required_str(content, "target_platform_id")
                })?;
                let new_text = required_str(content, "text")?;
                let resp = self.api.edit_message(room_id, target, new_text).await?;
                Ok(Some(resp.event_id))
            }
            "reaction" => {
                let target = required_str(content, "target_event_id").or_else(|_| {
                    required_str(content, "target_platform_id")
                })?;
                let key = required_str(content, "emoji")
                    .or_else(|_| required_str(content, "key"))?;
                let resp = self.api.send_reaction(room_id, target, key).await?;
                Ok(Some(resp.event_id))
            }
            other => Err(AdapterError::Unsupported(format!(
                "matrix action `{other}` is not supported"
            ))),
        }
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

fn extract_html(value: &Value) -> Option<String> {
    value
        .get("html")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, AdapterError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::BadRequest(format!("missing `{key}` in matrix action")))
}

fn guess_mime(filename: &str) -> String {
    let lower = filename.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or_default();
    match ext {
        "png" => "image/png".into(),
        "jpg" | "jpeg" => "image/jpeg".into(),
        "gif" => "image/gif".into(),
        "webp" => "image/webp".into(),
        "mp4" => "video/mp4".into(),
        "webm" => "video/webm".into(),
        "mp3" => "audio/mpeg".into(),
        "ogg" => "audio/ogg".into(),
        "wav" => "audio/wav".into(),
        "txt" => "text/plain".into(),
        "json" => "application/json".into(),
        "pdf" => "application/pdf".into(),
        _ => "application/octet-stream".into(),
    }
}

fn msgtype_for_mime(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "m.image"
    } else if mime.starts_with("audio/") {
        "m.audio"
    } else if mime.starts_with("video/") {
        "m.video"
    } else {
        "m.file"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_TXN_PREFIX;
    use ironclaw_types::{MessageKind, OutboundFile};
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn build_adapter(
        server_url: &str,
    ) -> (Arc<MatrixAdapter>, TempDir, mpsc::Receiver<InboundEvent>) {
        let dir = TempDir::new().unwrap();
        let api = Arc::new(MatrixApi::with_client(
            reqwest::Client::new(),
            server_url.to_owned(),
            "tok".to_owned(),
            DEFAULT_TXN_PREFIX.to_owned(),
        ));
        let config = MatrixConfig {
            homeserver_url: server_url.to_owned(),
            access_token: "tok".to_owned(),
            user_id: "@bot:m.org".to_owned(),
            rooms: vec![],
            sync_timeout_ms: 0,
            txn_prefix: DEFAULT_TXN_PREFIX.to_owned(),
        };
        let (tx, rx) = mpsc::channel::<InboundEvent>(8);
        let adapter = MatrixAdapter::start_with_api(api, config, tx, dir.path().to_path_buf())
            .unwrap();
        (adapter, dir, rx)
    }

    async fn mount_empty_sync(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "n", "rooms": { "join": {} }
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn channel_type_and_supports_threads() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert_eq!(adapter.channel_type().as_str(), "matrix");
        assert!(adapter.supports_threads());
        assert_eq!(adapter.bot_user_id(), "@bot:m.org");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_routes_to_send_text() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$x:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "hi" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$x:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_thread_id_routes_to_send_threaded() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.thread\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$t:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                Some("$root:m.org"),
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "in-thread" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$t:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_html_routes_to_send_html() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"formatted_body\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$h:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "hi", "html": "<b>hi</b>" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$h:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_html_without_text_uses_html_as_plain() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"format\":\"org.matrix.custom.html\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$h2:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "html": "<b>hi</b>" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$h2:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_action_uses_m_replace() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.replace\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$e:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "edit",
                        "target_event_id": "$tgt:m.org",
                        "text": "updated"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$e:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_action_uses_m_annotation() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.reaction/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.annotation\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$r:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_event_id": "$tgt:m.org",
                        "emoji": "\u{1F44D}"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$r:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_action_missing_target_errors() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "edit", "text": "x"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "shrug"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_files_uploads_then_sends_media() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("POST"))
            .and(path("/_matrix/media/v3/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content_uri": "mxc://m.org/abc"
            })))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$file:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "caption"}),
                    files: vec![OutboundFile {
                        filename: "doc.txt".into(),
                        data: vec![1, 2, 3],
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$file:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_image_uses_m_image() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("POST"))
            .and(path("/_matrix/media/v3/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content_uri": "mxc://m.org/p"
            })))
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/send/m\.room\.message/.+"))
            .and(wiremock::matchers::body_string_contains("\"m.image\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": "$img:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let id = adapter
            .deliver(
                "!a:m.org",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({}),
                    files: vec![OutboundFile {
                        filename: "pic.png".into(),
                        data: vec![0; 4],
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("$img:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_no_text_no_files_errors() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let err = adapter
            .deliver(
                "!a:m.org",
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
    async fn set_typing_calls_typing_endpoint() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.set_typing("!a:m.org", None).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_adds_room_to_live_set() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert!(adapter.subscribed_rooms().await.is_empty());
        adapter.subscribe("!a:m.org", None).await.unwrap();
        let set = adapter.subscribed_rooms().await;
        assert!(set.contains("!a:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_resolves_alias_via_api() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/directory/room/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "room_id": "!real:m.org"
            })))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.subscribe("#alias:m.org", None).await.unwrap();
        let set = adapter.subscribed_rooms().await;
        assert!(set.contains("!real:m.org"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn alias_cache_avoids_second_lookup() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = counter.clone();
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/v3/directory/room/.+"))
            .respond_with(move |_req: &wiremock::Request| {
                cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "room_id": "!real:m.org"
                }))
            })
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/v3/rooms/.+/typing/.+"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&s)
            .await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.set_typing("#alias:m.org", None).await.unwrap();
        adapter.set_typing("#alias:m.org", None).await.unwrap();
        // First call resolves; second call should hit the cache.
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert!(adapter.open_dm("@u:m.org").await.unwrap().is_none());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        adapter.shutdown().await;
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn debug_format_renders() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        let s = format!("{adapter:?}");
        assert!(s.contains("MatrixAdapter"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn config_and_api_accessors() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let (adapter, _dir, _rx) = build_adapter(&s.uri());
        assert_eq!(adapter.config().user_id, "@bot:m.org");
        assert_eq!(adapter.api().access_token(), "tok");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn start_initialises_configured_rooms() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let dir = TempDir::new().unwrap();
        let cfg = MatrixConfig {
            homeserver_url: s.uri(),
            access_token: "tok".into(),
            user_id: "@bot:m.org".into(),
            rooms: vec!["!a:m.org".into(), "!b:m.org".into()],
            sync_timeout_ms: 0,
            txn_prefix: DEFAULT_TXN_PREFIX.into(),
        };
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let adapter = MatrixAdapter::start(cfg, tx, dir.path().to_path_buf()).unwrap();
        let set = adapter.subscribed_rooms().await;
        assert!(set.contains("!a:m.org"));
        assert!(set.contains("!b:m.org"));
        adapter.shutdown().await;
    }

    #[test]
    fn guess_mime_for_common_extensions() {
        assert_eq!(guess_mime("a.png"), "image/png");
        assert_eq!(guess_mime("a.jpg"), "image/jpeg");
        assert_eq!(guess_mime("a.JPEG"), "image/jpeg");
        assert_eq!(guess_mime("a.gif"), "image/gif");
        assert_eq!(guess_mime("a.webp"), "image/webp");
        assert_eq!(guess_mime("a.mp4"), "video/mp4");
        assert_eq!(guess_mime("a.webm"), "video/webm");
        assert_eq!(guess_mime("a.mp3"), "audio/mpeg");
        assert_eq!(guess_mime("a.ogg"), "audio/ogg");
        assert_eq!(guess_mime("a.wav"), "audio/wav");
        assert_eq!(guess_mime("a.txt"), "text/plain");
        assert_eq!(guess_mime("a.json"), "application/json");
        assert_eq!(guess_mime("a.pdf"), "application/pdf");
        assert_eq!(guess_mime("blob"), "application/octet-stream");
        assert_eq!(guess_mime("blob.weird"), "application/octet-stream");
    }

    #[test]
    fn msgtype_for_mime_branches() {
        assert_eq!(msgtype_for_mime("image/png"), "m.image");
        assert_eq!(msgtype_for_mime("audio/mpeg"), "m.audio");
        assert_eq!(msgtype_for_mime("video/mp4"), "m.video");
        assert_eq!(msgtype_for_mime("application/pdf"), "m.file");
    }

    #[test]
    fn extract_text_and_html_and_action() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({"text": "x"})), "x");
        assert_eq!(extract_html(&json!({})), None);
        assert_eq!(extract_html(&json!({"html":"<b>x</b>"})).as_deref(), Some("<b>x</b>"));
        assert_eq!(extract_action(&json!({"action": "edit"})), Some("edit"));
        assert_eq!(extract_action(&json!({})), None);
    }

    #[test]
    fn required_str_errors_when_missing() {
        let err = required_str(&json!({}), "x").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }
}
