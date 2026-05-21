//! Microsoft Teams [`ChannelAdapter`] implementation.
//!
//! See the crate-level docs in [`crate`] for the contract overview.
//!
//! The adapter is constructed by [`crate::factory::TeamsFactory`] but is
//! also publicly visible so downstream tests can drive it directly without
//! booting the full webhook server.

use crate::api::TeamsApi;
use crate::emoji::shortcode_to_reaction_type;
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, MessageKind, OutboundMessage};
use serde_json::Value;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// The two shapes a Teams `platform_id` can take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TeamsTarget {
    /// `team/{team_id}/channel/{channel_id}`.
    Channel {
        team_id: String,
        channel_id: String,
    },
    /// `chat/{chat_id}`.
    Chat { chat_id: String },
}

impl TeamsTarget {
    /// Parse a `platform_id` string into a typed target.
    pub(crate) fn parse(platform_id: &str) -> Result<Self, AdapterError> {
        let parts: Vec<&str> = platform_id.split('/').collect();
        match parts.as_slice() {
            ["team", team, "channel", channel] if !team.is_empty() && !channel.is_empty() => {
                Ok(Self::Channel {
                    team_id: (*team).to_owned(),
                    channel_id: (*channel).to_owned(),
                })
            }
            ["chat", chat] if !chat.is_empty() => Ok(Self::Chat {
                chat_id: (*chat).to_owned(),
            }),
            _ => Err(AdapterError::BadRequest(format!(
                "teams: unrecognized platform_id shape: {platform_id}"
            ))),
        }
    }
}

/// Microsoft Teams channel adapter. See module-level docs.
pub struct TeamsAdapter {
    channel_type: ChannelType,
    api: TeamsApi,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for TeamsAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamsAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl TeamsAdapter {
    /// Construct with an already-built API client. Used by the factory and
    /// by tests that drive the adapter directly.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: TeamsApi) -> Self {
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
            .expect("teams adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background webhook server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("teams adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &TeamsApi {
        &self.api
    }

    async fn deliver_chat(
        &self,
        target: &TeamsTarget,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        // For system actions (edit/reaction) we look at the structured fields.
        if matches!(message.kind, MessageKind::System) {
            return self.deliver_system_action(target, message).await;
        }

        if !message.files.is_empty() {
            return Err(AdapterError::Unsupported(
                "teams adapter does not yet support outbound file attachments".into(),
            ));
        }

        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let html = message.content.get("html").and_then(Value::as_str);
        let (content, content_type) = match html {
            Some(html) => (html, "html"),
            None => (text, "text"),
        };

        let id = match target {
            TeamsTarget::Channel {
                team_id,
                channel_id,
            } => match thread_id {
                Some(parent) => {
                    self.api
                        .post_channel_reply(team_id, channel_id, parent, content, content_type)
                        .await?
                        .id
                }
                None => {
                    self.api
                        .post_channel_message(team_id, channel_id, content, content_type)
                        .await?
                        .id
                }
            },
            TeamsTarget::Chat { chat_id } => {
                self.api
                    .post_chat_message(chat_id, content, content_type)
                    .await?
                    .id
            }
        };
        Ok(Some(id))
    }

    async fn deliver_system_action(
        &self,
        target: &TeamsTarget,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let action = message
            .content
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdapterError::BadRequest(
                    "teams system message missing `action` field".into(),
                )
            })?;
        match action {
            "edit" => {
                let target_id = message
                    .content
                    .get("target_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "teams edit action missing `target_id`".into(),
                        )
                    })?;
                let new_text = message
                    .content
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match target {
                    TeamsTarget::Channel {
                        team_id,
                        channel_id,
                    } => {
                        self.api
                            .edit_channel_message(team_id, channel_id, target_id, new_text)
                            .await?;
                    }
                    TeamsTarget::Chat { chat_id } => {
                        self.api
                            .edit_chat_message(chat_id, target_id, new_text)
                            .await?;
                    }
                }
                Ok(None)
            }
            "reaction" => {
                let target_id = message
                    .content
                    .get("target_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "teams reaction action missing `target_id`".into(),
                        )
                    })?;
                let emoji = message
                    .content
                    .get("emoji")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "teams reaction action missing `emoji`".into(),
                        )
                    })?;
                let reaction_type = shortcode_to_reaction_type(emoji).ok_or_else(|| {
                    AdapterError::Unsupported(format!(
                        "teams does not support reaction `{emoji}`"
                    ))
                })?;
                match target {
                    TeamsTarget::Channel {
                        team_id,
                        channel_id,
                    } => {
                        self.api
                            .set_channel_reaction(team_id, channel_id, target_id, reaction_type)
                            .await?;
                    }
                    TeamsTarget::Chat { chat_id } => {
                        self.api
                            .set_chat_reaction(chat_id, target_id, reaction_type)
                            .await?;
                    }
                }
                Ok(None)
            }
            other => Err(AdapterError::Unsupported(format!(
                "teams system action `{other}` is not supported"
            ))),
        }
    }
}

#[async_trait]
impl ChannelAdapter for TeamsAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        self.deliver_chat(&target, thread_id, message).await
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Microsoft Graph supports DMs via `/chats` but they require multiple
        // calls (create chat with members, then post). For v1 we surface
        // `None` and rely on the host wiring to supply an existing chat id.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::MessageKind;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> TeamsAdapter {
        TeamsAdapter::new(
            ChannelType::new("teams"),
            TeamsApi::new(server.uri(), "tok-test"),
        )
    }

    fn text(msg: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": msg}),
            files: vec![],
        }
    }

    #[test]
    fn target_parses_channel_shape() {
        let t = TeamsTarget::parse("team/T1/channel/C1").unwrap();
        assert_eq!(
            t,
            TeamsTarget::Channel {
                team_id: "T1".into(),
                channel_id: "C1".into()
            }
        );
    }

    #[test]
    fn target_parses_chat_shape() {
        let t = TeamsTarget::parse("chat/CHAT1").unwrap();
        assert_eq!(
            t,
            TeamsTarget::Chat {
                chat_id: "CHAT1".into()
            }
        );
    }

    #[test]
    fn target_rejects_unknown_shape() {
        let err = TeamsTarget::parse("foo/bar").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn target_rejects_empty_components() {
        assert!(TeamsTarget::parse("team//channel/C1").is_err());
        assert!(TeamsTarget::parse("team/T/channel/").is_err());
        assert!(TeamsTarget::parse("chat/").is_err());
        assert!(TeamsTarget::parse("").is_err());
    }

    #[tokio::test]
    async fn channel_type_and_thread_support() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert_eq!(adapter.channel_type().as_str(), "teams");
        assert!(adapter.supports_threads());
    }

    #[tokio::test]
    async fn deliver_channel_message_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .and(header("authorization", "Bearer tok-test"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "MID"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("team/T1/channel/C1", None, &text("hello"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("MID"));
    }

    #[tokio::test]
    async fn deliver_channel_with_thread_uses_replies_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages/PARENT/replies"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "RID"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("team/T1/channel/C1", Some("PARENT"), &text("yo"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("RID"));
    }

    #[tokio::test]
    async fn deliver_chat_uses_chats_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chats/CHAT1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "CMID"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("chat/CHAT1", None, &text("hello"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("CMID"));
    }

    #[tokio::test]
    async fn deliver_html_content_uses_html_content_type() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "HID"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "fallback", "html": "<p>html</p>"}),
            files: vec![],
        };
        let id = adapter
            .deliver("team/T1/channel/C1", None, &msg)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("HID"));
    }

    #[tokio::test]
    async fn deliver_files_returns_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "with file"}),
            files: vec![ironclaw_types::OutboundFile {
                filename: "a.txt".into(),
                data: vec![1, 2, 3],
            }],
        };
        match adapter.deliver("team/T1/channel/C1", None, &msg).await {
            Err(AdapterError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_malformed_platform_id_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        match adapter.deliver("nope", None, &text("x")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("platform_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_action_on_channel_patches_message() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/teams/T1/channels/C1/messages/MID"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_id":"MID","text":"new"}),
            files: vec![],
        };
        let id = adapter
            .deliver("team/T1/channel/C1", None, &msg)
            .await
            .unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn deliver_edit_action_on_chat_patches_chat_message() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/chats/CHAT1/messages/MID"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_id":"MID","text":"new"}),
            files: vec![],
        };
        adapter
            .deliver("chat/CHAT1", None, &msg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deliver_edit_missing_target_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","text":"new"}),
            files: vec![],
        };
        match adapter.deliver("team/T1/channel/C1", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_action_on_channel_sets_reaction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages/MID/setReaction"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_id":"MID","emoji":"thumbsup"}),
            files: vec![],
        };
        adapter
            .deliver("team/T1/channel/C1", None, &msg)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deliver_reaction_action_on_chat_sets_reaction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chats/CHAT1/messages/MID/setReaction"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_id":"MID","emoji":"heart"}),
            files: vec![],
        };
        adapter.deliver("chat/CHAT1", None, &msg).await.unwrap();
    }

    #[tokio::test]
    async fn deliver_reaction_with_unknown_emoji_is_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_id":"MID","emoji":"unicorn"}),
            files: vec![],
        };
        match adapter.deliver("team/T1/channel/C1", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("unicorn")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_missing_emoji_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_id":"MID"}),
            files: vec![],
        };
        match adapter.deliver("team/T1/channel/C1", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_missing_target_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","emoji":"like"}),
            files: vec![],
        };
        match adapter.deliver("team/T1/channel/C1", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"yeet","target_id":"MID"}),
            files: vec![],
        };
        match adapter.deliver("team/T1/channel/C1", None, &msg).await {
            Err(AdapterError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_without_action_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({}),
            files: vec![],
        };
        match adapter.deliver("team/T1/channel/C1", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_auth_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter
            .deliver("team/T1/channel/C1", None, &text("x"))
            .await
        {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_rate_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "9"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter
            .deliver("team/T1/channel/C1", None, &text("x"))
            .await
        {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(9)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_transport_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter
            .deliver("team/T1/channel/C1", None, &text("x"))
            .await
        {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
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
        adapter.subscribe("team/T1/channel/C1", None).await.unwrap();
        adapter
            .subscribe("team/T1/channel/C1", Some("PARENT"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn set_typing_uses_default_impl() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.set_typing("team/T1/channel/C1", None).await.unwrap();
    }

    #[tokio::test]
    async fn server_handle_shutdown_idempotent() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        adapter.set_server_handle(task);
        adapter.shutdown_server();
        adapter.shutdown_server();
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let s = format!("{adapter:?}");
        assert!(s.contains("TeamsAdapter"));
        assert!(s.contains("teams"));
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let dbg = format!("{:?}", adapter.api());
        assert!(dbg.contains("tok-test"));
    }
}
