//! Linear [`ChannelAdapter`] implementation.

use crate::api::{CommentCreateInput, CommentUpdateInput, LinearApi, ReactionCreateInput};
use async_trait::async_trait;
use copperclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use copperclaw_types::{ChannelType, MessageKind, OutboundMessage};
use serde_json::Value;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Linear channel adapter. See module-level docs.
pub struct LinearAdapter {
    channel_type: ChannelType,
    api: LinearApi,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for LinearAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl LinearAdapter {
    /// Construct with an already-built API client. Used by the factory and
    /// by tests that drive the adapter directly.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: LinearApi) -> Self {
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
            .expect("linear adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background webhook server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("linear adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &LinearApi {
        &self.api
    }
}

/// Allowed character set for emoji shortcodes routed through
/// `reactionCreate`. Linear accepts arbitrary shortcodes; we restrict to
/// `[a-z0-9_+-]+` so we never round-trip whitespace or control chars.
fn is_valid_shortcode(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '+' | '-'))
}

#[async_trait]
impl ChannelAdapter for LinearAdapter {
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
        // System actions: edit + reaction.
        if matches!(message.kind, MessageKind::System) {
            if let Some(action) = message
                .content
                .get("action")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                return self.deliver_system_action(&action, &message.content).await;
            }
        }

        // Regular text path.
        if !message.files.is_empty() {
            return Err(AdapterError::Unsupported(
                "linear comments do not support file attachments".into(),
            ));
        }
        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if text.trim().is_empty() {
            return Err(AdapterError::BadRequest(
                "linear deliver requires non-empty text".into(),
            ));
        }
        let input = CommentCreateInput {
            issue_id: platform_id.to_owned(),
            body: text,
            parent_id: thread_id.map(str::to_owned),
        };
        let r = self.api.create_comment(&input).await?;
        Ok(Some(r.id))
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Linear has no DM concept in the messaging sense — every comment
        // exists on a team/workspace issue. Returning `None` matches the
        // trait default for adapters that don't pre-resolve DMs.
        Ok(None)
    }
}

impl LinearAdapter {
    async fn deliver_system_action(
        &self,
        action: &str,
        content: &Value,
    ) -> Result<Option<String>, AdapterError> {
        match action {
            "edit" => {
                let target = content
                    .get("target_platform_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "linear edit action requires `target_platform_id` (comment uuid)".into(),
                        )
                    })?;
                let text = content
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if text.trim().is_empty() {
                    return Err(AdapterError::BadRequest(
                        "linear edit action requires non-empty text".into(),
                    ));
                }
                let r = self
                    .api
                    .update_comment(target, &CommentUpdateInput { body: text })
                    .await?;
                Ok(Some(r.id))
            }
            "reaction" => {
                let target = content
                    .get("target_platform_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "linear reaction action requires `target_platform_id` (comment uuid)"
                                .into(),
                        )
                    })?;
                let emoji = content
                    .get("emoji")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if !is_valid_shortcode(&emoji) {
                    return Err(AdapterError::BadRequest(format!(
                        "linear reaction emoji must match [a-z0-9_+-]+; got `{emoji}`"
                    )));
                }
                self.api
                    .create_reaction(&ReactionCreateInput {
                        comment_id: target.to_owned(),
                        emoji,
                    })
                    .await?;
                Ok(None)
            }
            other => Err(AdapterError::Unsupported(format!(
                "linear adapter does not support system action `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::OutboundFile;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> LinearAdapter {
        LinearAdapter::new(
            ChannelType::new("linear"),
            LinearApi::new(format!("{}/graphql", server.uri()), "lin_api_test"),
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
        assert_eq!(adapter.channel_type().as_str(), "linear");
        assert!(adapter.supports_threads());
    }

    #[tokio::test]
    async fn deliver_text_creates_comment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "lin_api_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"success": true, "comment": {"id": "c-100"}}}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("issue-uuid-1", None, &text("hi linear"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("c-100"));
    }

    #[tokio::test]
    async fn deliver_with_thread_id_sets_parent_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(wiremock::matchers::body_string_contains("parentId"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"success": true, "comment": {"id": "c-101"}}}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("issue-1", Some("parent-comment-1"), &text("threaded"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("c-101"));
    }

    #[tokio::test]
    async fn deliver_empty_text_rejects_with_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        match adapter.deliver("i", None, &text("")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("non-empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_whitespace_only_text_rejects_with_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        match adapter.deliver("i", None, &text("   \t\n")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_files_returns_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "with file"}),
            files: vec![OutboundFile {
                filename: "a.txt".into(),
                data: vec![1, 2, 3],
            }],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_auth_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("i", None, &text("hi")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_action_routes_to_update_comment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(wiremock::matchers::body_string_contains("commentUpdate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentUpdate": {"success": true, "comment": {"id": "c-200"}}}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "edit",
                "target_platform_id": "c-200",
                "text": "edited body"
            }),
            files: vec![],
        };
        let id = adapter.deliver("i", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("c-200"));
    }

    #[tokio::test]
    async fn deliver_edit_missing_target_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "edit", "text": "new"}),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("target_platform_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_empty_text_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "edit",
                "target_platform_id": "c-1",
                "text": ""
            }),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_action_routes_to_create_reaction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(wiremock::matchers::body_string_contains("reactionCreate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"reactionCreate": {"success": true, "reaction": {"id": "r-1"}}}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "reaction",
                "target_platform_id": "c-1",
                "emoji": "thumbsup"
            }),
            files: vec![],
        };
        let id = adapter.deliver("i", None, &msg).await.unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn deliver_reaction_missing_target_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "reaction", "emoji": "thumbsup"}),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("target_platform_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_empty_emoji_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "reaction",
                "target_platform_id": "c-1",
                "emoji": ""
            }),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_whitespace_emoji_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "reaction",
                "target_platform_id": "c-1",
                "emoji": "   "
            }),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_invalid_chars_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action": "reaction",
                "target_platform_id": "c-1",
                "emoji": "with space"
            }),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("[a-z0-9_+-]")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_accepts_underscore_plus_minus_digits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"reactionCreate": {"success": true, "reaction": {"id": "r"}}}
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        for emoji in ["thumbsup", "tada", "plus-one", "u_score", "100", "ok+"] {
            let msg = OutboundMessage {
                kind: MessageKind::System,
                content: json!({
                    "action": "reaction",
                    "target_platform_id": "c",
                    "emoji": emoji
                }),
                files: vec![],
            };
            adapter.deliver("i", None, &msg).await.unwrap();
        }
    }

    #[tokio::test]
    async fn deliver_unknown_system_action_returns_unsupported() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "vanish", "target_platform_id": "c"}),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("vanish")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_without_action_falls_through_to_text_path() {
        // System message without an `action` falls back to comment-creating
        // text path; if the text is empty we get BadRequest.
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"text": ""}),
            files: vec![],
        };
        match adapter.deliver("i", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_rate_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "9"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("i", None, &text("hi")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(9)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("i", None, &text("hi")).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_bad_request_from_graphql_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "Issue not found"}]
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("nope", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("Issue not found")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_uses_default_impl() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.subscribe("i", None).await.unwrap();
        adapter.subscribe("i", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_is_default_noop() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.set_typing("i", None).await.unwrap();
        adapter.set_typing("i", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(adapter.open_dm("u").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(format!("{:?}", adapter.api()).contains("lin_api_test"));
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
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let s = format!("{adapter:?}");
        assert!(s.contains("LinearAdapter"));
        assert!(s.contains("linear"));
    }

    #[test]
    fn is_valid_shortcode_accepts_canonical_examples() {
        for s in ["thumbsup", "tada", "plus-one", "u_score", "100", "ok+", "a-b_c+1"] {
            assert!(is_valid_shortcode(s), "rejected `{s}` unexpectedly");
        }
    }

    #[test]
    fn is_valid_shortcode_rejects_empty_and_uppercase_and_punctuation() {
        for s in ["", "Thumbsup", "with space", "smile!", ":wink:", "🙂"] {
            assert!(!is_valid_shortcode(s), "accepted `{s}` unexpectedly");
        }
    }
}
