//! [`ChannelAdapter`] implementation for the `WeChat` Work channel.
//!
//! See the crate-level docs for `platform_id` shapes and the system-action
//! semantics (`edit` / `reaction` → `Unsupported`).

use crate::api::{MessageTarget, WeChatApi};
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, OutboundFile, OutboundMessage};
use serde_json::Value;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// The `WeChat` Work channel adapter.
pub struct WeChatAdapter {
    channel_type: ChannelType,
    api: WeChatApi,
    agent_id: i64,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for WeChatAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeChatAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

impl WeChatAdapter {
    /// Construct with an already-built API client and the configured
    /// `agent_id`.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: WeChatApi, agent_id: i64) -> Self {
        Self {
            channel_type,
            api,
            agent_id,
            server_handle: Mutex::new(None),
        }
    }

    /// Borrow the underlying API client (test-only convenience).
    #[must_use]
    pub fn api(&self) -> &WeChatApi {
        &self.api
    }

    /// `agent_id` this adapter sends as.
    #[must_use]
    pub fn agent_id(&self) -> i64 {
        self.agent_id
    }

    /// Attach the join handle of the spawned axum server so the adapter
    /// can abort it on shutdown.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("wechat adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background webhook server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("wechat adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    fn parse_platform_id(platform_id: &str) -> Result<MessageTarget<'_>, AdapterError> {
        if let Some(rest) = platform_id.strip_prefix("user:") {
            if rest.is_empty() {
                return Err(AdapterError::BadRequest(
                    "wechat platform_id `user:` is empty".into(),
                ));
            }
            return Ok(MessageTarget::User(rest));
        }
        if let Some(rest) = platform_id.strip_prefix("party:") {
            if rest.is_empty() {
                return Err(AdapterError::BadRequest(
                    "wechat platform_id `party:` is empty".into(),
                ));
            }
            return Ok(MessageTarget::Party(rest));
        }
        if let Some(rest) = platform_id.strip_prefix("tag:") {
            if rest.is_empty() {
                return Err(AdapterError::BadRequest(
                    "wechat platform_id `tag:` is empty".into(),
                ));
            }
            return Ok(MessageTarget::Tag(rest));
        }
        Err(AdapterError::BadRequest(format!(
            "wechat platform_id `{platform_id}` must be `user:<id>`, `party:<id>`, or `tag:<id>`"
        )))
    }

    async fn deliver_files(
        &self,
        target: MessageTarget<'_>,
        files: &[OutboundFile],
    ) -> Result<Option<String>, AdapterError> {
        let mut last_id: Option<String> = None;
        for file in files {
            let kind = infer_media_kind(&file.filename);
            let media_id = self
                .api
                .upload_media(kind, &file.filename, file.data.clone())
                .await?;
            let id = if kind == "image" {
                self.api
                    .send_image_by_id(self.agent_id, target, &media_id)
                    .await?
            } else {
                self.api
                    .send_file_by_id(self.agent_id, target, &media_id)
                    .await?
            };
            if id.is_some() {
                last_id = id;
            }
        }
        Ok(last_id)
    }
}

/// Infer the Work Weixin media `type` for a filename.
///
/// Public so the host can pre-validate. Returns one of `"image"`, `"voice"`,
/// `"video"`, or `"file"`.
fn infer_media_kind(filename: &str) -> &'static str {
    let lower = filename.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => "image",
        "amr" | "mp3" | "wav" | "ogg" | "oga" => "voice",
        "mp4" | "mov" | "mkv" | "avi" | "webm" => "video",
        _ => "file",
    }
}

#[async_trait]
impl ChannelAdapter for WeChatAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        // Work Weixin DMs have no thread concept.
        false
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let target = Self::parse_platform_id(platform_id)?;

        // System action handling — `edit` and `reaction` are not supported by
        // the Work Weixin API.
        if let Some(action) = message.content.get("action").and_then(Value::as_str) {
            return Self::handle_system_action(action);
        }

        // `card` content with a `template_card` payload — passes through.
        if let Some(card) = message.content.get("template_card") {
            return self
                .api
                .send_template_card(self.agent_id, target, card)
                .await;
        }

        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");

        // Files-first: upload + send by media id.
        if !message.files.is_empty() {
            let mut first_id: Option<String> = None;
            if !text.is_empty() {
                let id = self.api.send_text(self.agent_id, target, text).await?;
                first_id = id;
            }
            let last = self.deliver_files(target, &message.files).await?;
            return Ok(last.or(first_id));
        }

        // Pure text.
        self.api.send_text(self.agent_id, target, text).await
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Work Weixin sends DMs by `touser` directly; there is no separate
        // open-DM handshake.
        Ok(None)
    }
}

impl WeChatAdapter {
    fn handle_system_action(action: &str) -> Result<Option<String>, AdapterError> {
        match action {
            "edit" => Err(AdapterError::Unsupported(
                "wechat does not support editing previously sent messages".into(),
            )),
            "reaction" => Err(AdapterError::Unsupported(
                "wechat does not support message reactions".into(),
            )),
            other => Err(AdapterError::Unsupported(format!(
                "wechat does not support system action `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::MessageKind;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn token_mount(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/cgi-bin/gettoken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token":"T","expires_in":7200
            })))
            .mount(server)
            .await;
    }

    fn adapter_for(server: &MockServer, agent_id: i64) -> WeChatAdapter {
        WeChatAdapter::new(
            ChannelType::new("wechat"),
            WeChatApi::new(server.uri(), "wx-corp", "secret"),
            agent_id,
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
    async fn channel_type_is_wechat() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        assert_eq!(a.channel_type().as_str(), "wechat");
    }

    #[tokio::test]
    async fn supports_threads_is_false() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn subscribe_default_is_ok() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        a.subscribe("user:alice", None).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_default_is_ok() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        a.set_typing("user:alice", None).await.unwrap();
    }

    #[tokio::test]
    async fn deliver_text_to_user() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "touser":"alice","msgtype":"text","agentid":1,
                "text":{"content":"hi"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"M-1"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        let id = a.deliver("user:alice", None, &text("hi")).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-1"));
    }

    #[tokio::test]
    async fn deliver_text_to_party() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "toparty":"99","msgtype":"text"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"M-P"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        let id = a.deliver("party:99", None, &text("hi")).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-P"));
    }

    #[tokio::test]
    async fn deliver_text_to_tag() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "totag":"7","msgtype":"text"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"M-T"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        let id = a.deliver("tag:7", None, &text("hi")).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-T"));
    }

    #[tokio::test]
    async fn deliver_rejects_unknown_prefix() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        match a.deliver("dept:99", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => {
                assert!(m.contains("user:") && m.contains("party:") && m.contains("tag:"));
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_bare_id_without_prefix() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        match a.deliver("alice", None, &text("hi")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_empty_user_id() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        match a.deliver("user:", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_empty_party_id() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        match a.deliver("party:", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_empty_tag_id() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        match a.deliver("tag:", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_file_only_uploads_then_sends() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/media/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"media_id":"MID-FILE"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "msgtype":"file",
                "file":{"media_id":"MID-FILE"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"FILE-OK"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":""}),
            files: vec![OutboundFile {
                filename: "report.pdf".into(),
                data: b"PDF".to_vec(),
            }],
        };
        let id = a.deliver("user:alice", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("FILE-OK"));
    }

    #[tokio::test]
    async fn deliver_with_image_only_uploads_as_image() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/media/upload"))
            .and(wiremock::matchers::query_param("type", "image"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"media_id":"MID-IMG"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "msgtype":"image",
                "image":{"media_id":"MID-IMG"}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"IMG-OK"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":""}),
            files: vec![OutboundFile {
                filename: "photo.JPG".into(),
                data: b"PIC".to_vec(),
            }],
        };
        let id = a.deliver("user:alice", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("IMG-OK"));
    }

    #[tokio::test]
    async fn deliver_with_text_and_files_sends_both() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/media/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"media_id":"MID-X"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"OK"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":"caption"}),
            files: vec![OutboundFile {
                filename: "x.pdf".into(),
                data: b"PDF".to_vec(),
            }],
        };
        let id = a.deliver("user:alice", None, &msg).await.unwrap();
        assert!(id.is_some());
    }

    #[tokio::test]
    async fn deliver_edit_action_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_seq":7,"text":"new"}),
            files: vec![],
        };
        match a.deliver("user:alice", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("edit")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_action_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_message_id":"X","emoji":""}),
            files: vec![],
        };
        match a.deliver("user:alice", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("reaction")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"forward","target":"x"}),
            files: vec![],
        };
        match a.deliver("user:alice", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("forward")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_template_card_passes_through() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .and(wiremock::matchers::body_partial_json(json!({
                "msgtype":"template_card",
                "template_card":{"card_type":"text_notice","source":{"desc":"Bot"}}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":0,"msgid":"CARD"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({
                "template_card":{
                    "card_type":"text_notice",
                    "source":{"desc":"Bot"}
                }
            }),
            files: vec![],
        };
        let id = a.deliver("user:alice", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("CARD"));
    }

    #[tokio::test]
    async fn deliver_surfaces_auth_error() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":40014,"errmsg":"bad token"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        match a.deliver("user:alice", None, &text("hi")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_surfaces_rate_error() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":45009,"errmsg":"too fast"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        match a.deliver("user:alice", None, &text("hi")).await {
            Err(AdapterError::Rate { .. }) => {}
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_surfaces_bad_request_error() {
        let server = MockServer::start().await;
        token_mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/cgi-bin/message/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errcode":40036,"errmsg":"invalid agentid"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, 1);
        match a.deliver("user:alice", None, &text("hi")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        assert!(a.open_dm("anyone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn server_handle_shutdown_is_idempotent() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        let task = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        a.set_server_handle(task);
        a.shutdown_server();
        a.shutdown_server();
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        let _ = a.api();
    }

    #[tokio::test]
    async fn agent_id_accessor_returns_value() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 42);
        assert_eq!(a.agent_id(), 42);
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, 1);
        let s = format!("{a:?}");
        assert!(s.contains("WeChatAdapter"));
        assert!(s.contains("wechat"));
    }

    #[test]
    fn infer_media_kind_image_extensions() {
        for ext in ["png", "jpg", "jpeg", "gif", "bmp", "webp"] {
            assert_eq!(infer_media_kind(&format!("f.{ext}")), "image");
            assert_eq!(infer_media_kind(&format!("f.{}", ext.to_uppercase())), "image");
        }
    }

    #[test]
    fn infer_media_kind_voice_extensions() {
        for ext in ["amr", "mp3", "wav", "ogg", "oga"] {
            assert_eq!(infer_media_kind(&format!("v.{ext}")), "voice");
        }
    }

    #[test]
    fn infer_media_kind_video_extensions() {
        for ext in ["mp4", "mov", "mkv", "avi", "webm"] {
            assert_eq!(infer_media_kind(&format!("v.{ext}")), "video");
        }
    }

    #[test]
    fn infer_media_kind_default_is_file() {
        assert_eq!(infer_media_kind("noext"), "file");
        assert_eq!(infer_media_kind("x.pdf"), "file");
        assert_eq!(infer_media_kind("x.bin"), "file");
    }
}
