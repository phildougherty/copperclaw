//! Google Chat [`ChannelAdapter`] implementation.

use crate::api::GchatApi;
use crate::emoji::emoji_codepoint;
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, OutboundMessage};
use serde_json::Value;
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

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Google Chat DMs have to be opened via a separate `spaces.create`
        // flow we don't model in v1; deliveries address the existing DM
        // space directly via its `spaces/X` id.
        Ok(None)
    }
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
    use ironclaw_types::{MessageKind, OutboundFile};
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
}
