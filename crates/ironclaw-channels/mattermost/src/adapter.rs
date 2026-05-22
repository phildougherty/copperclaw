//! [`ChannelAdapter`] for Mattermost.
//!
//! Egress translates an [`OutboundMessage`] into one of three REST
//! calls keyed off the `action` field of `content`:
//!
//! - missing or `"post"` → `POST /api/v4/posts` (returns the new
//!   post's id).
//! - `"edit"` with `target_id` + `text` → `PUT
//!   /api/v4/posts/{target_id}/patch`.
//! - `"reaction"` with `target_id` + `emoji_name` → `POST
//!   /api/v4/reactions`. Requires the configured `bot_user_id`,
//!   because Mattermost binds reactions to a user.
//!
//! `subscribe`, `set_typing`, and `open_dm` use the trait defaults:
//! Mattermost has no "subscribe to channel" call for bots (the
//! outgoing webhook decides what flows in), typing indicators
//! require WebSocket auth we don't bother with, and DMs are just a
//! private channel id the caller already has.
//!
//! File attachments aren't yet implemented; an outbound with files
//! returns [`AdapterError::Unsupported`] explicitly rather than
//! silently dropping the files.

use crate::api::MattermostApi;
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter};
use ironclaw_types::{ChannelType, OutboundMessage};
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Mattermost adapter. Holds the REST client and the join handle for
/// the outgoing-webhook server so it can be aborted on drop.
pub struct MattermostAdapter {
    channel_type: ChannelType,
    api: MattermostApi,
    bot_user_id: Option<String>,
    server: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for MattermostAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let server_state = self.server.lock().map_or("poisoned", |g| {
            g.as_ref().map_or("stopped", |_| "running")
        });
        // `api` skipped on purpose — it holds the bearer token and we
        // don't want it in logs/error chains.
        f.debug_struct("MattermostAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &"<MattermostApi>")
            .field("bot_user_id", &self.bot_user_id)
            .field("server", &server_state)
            .finish()
    }
}

impl MattermostAdapter {
    /// Build a new adapter. The factory calls
    /// [`Self::set_server_handle`] right after spawning.
    #[must_use]
    pub fn new(
        channel_type: ChannelType,
        api: MattermostApi,
        bot_user_id: Option<String>,
    ) -> Self {
        Self {
            channel_type,
            api,
            bot_user_id,
            server: Mutex::new(None),
        }
    }

    /// Attach the outgoing-webhook server's join handle.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        if let Ok(mut slot) = self.server.lock() {
            *slot = Some(handle);
        }
    }

    /// Abort the background webhook server; idempotent.
    pub fn abort_server(&self) {
        if let Ok(mut slot) = self.server.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }
    }
}

impl Drop for MattermostAdapter {
    fn drop(&mut self) {
        self.abort_server();
    }
}

#[async_trait]
impl ChannelAdapter for MattermostAdapter {
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
        let content = &message.content;
        let action = content
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("post");
        // Reject files on actions that don't make sense for them
        // (edit / reaction) up front; only the post path supports
        // attachments.
        if !message.files.is_empty() && action != "post" {
            return Err(AdapterError::BadRequest(format!(
                "mattermost action `{action}` does not accept file attachments"
            )));
        }
        match action {
            "post" => {
                let text = content
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("missing `text` in outbound content".into())
                    })?;
                // Two-step upload: upload each file to /api/v4/files
                // against the destination channel, collect ids, then
                // POST the message with `file_ids` attached. Each
                // upload is independent; one failure cancels the post.
                let mut file_ids: Vec<String> = Vec::with_capacity(message.files.len());
                for f in &message.files {
                    let id = self
                        .api
                        .upload_file(platform_id, &f.filename, f.data.clone())
                        .await?;
                    file_ids.push(id);
                }
                let id = self
                    .api
                    .create_post_with_files(platform_id, text, thread_id, &file_ids)
                    .await?;
                Ok(Some(id))
            }
            "edit" => {
                let target = content
                    .get("target_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("edit requires `target_id`".into())
                    })?;
                let text = content
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("edit requires `text`".into())
                    })?;
                self.api.update_post(target, text).await?;
                Ok(Some(target.to_string()))
            }
            "reaction" => {
                let target = content
                    .get("target_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("reaction requires `target_id`".into())
                    })?;
                let emoji = content
                    .get("emoji_name")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("reaction requires `emoji_name`".into())
                    })?;
                let user = self.bot_user_id.as_deref().ok_or_else(|| {
                    AdapterError::Unsupported(
                        "reaction requires bot_user_id in mattermost config".into(),
                    )
                })?;
                self.api.add_reaction(user, target, emoji).await?;
                Ok(Some(target.to_string()))
            }
            other => Err(AdapterError::BadRequest(format!(
                "unsupported mattermost action: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::MessageKind;
    use serde_json::json;
    use tokio::time::{sleep, Duration};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn outbound(text: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": text}),
            files: vec![],
        }
    }

    fn make(server: &MockServer, bot: Option<&str>) -> MattermostAdapter {
        let api = MattermostApi::new(&server.uri(), "tok");
        MattermostAdapter::new(
            ChannelType::new("mattermost"),
            api,
            bot.map(str::to_string),
        )
    }

    #[tokio::test]
    async fn deliver_post_succeeds_and_returns_id() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "p-7"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let id = a.deliver("c1", None, &outbound("hi")).await.unwrap();
        assert_eq!(id.as_deref(), Some("p-7"));
    }

    #[tokio::test]
    async fn deliver_post_propagates_thread_as_root() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "p2"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let id = a
            .deliver("c1", Some("root-1"), &outbound("hi"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("p2"));
    }

    #[tokio::test]
    async fn deliver_edit_calls_patch() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/posts/p1/patch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"p1"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"action":"edit","target_id":"p1","text":"edited"}),
            files: vec![],
        };
        let id = a.deliver("c1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("p1"));
    }

    #[tokio::test]
    async fn deliver_edit_without_target_is_bad_request() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"action":"edit","text":"x"}),
            files: vec![],
        };
        assert!(matches!(
            a.deliver("c", None, &msg).await.unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn deliver_reaction_succeeds_with_bot() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/reactions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"emoji_name":"+1"})))
            .mount(&mock)
            .await;
        let a = make(&mock, Some("bot"));
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"action":"reaction","target_id":"p","emoji_name":"+1"}),
            files: vec![],
        };
        let id = a.deliver("c", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("p"));
    }

    #[tokio::test]
    async fn deliver_reaction_without_bot_is_unsupported() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"action":"reaction","target_id":"p","emoji_name":"+1"}),
            files: vec![],
        };
        match a.deliver("c", None, &msg).await.unwrap_err() {
            AdapterError::Unsupported(m) => assert!(m.contains("bot_user_id")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_post_without_text_is_bad_request() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        assert!(matches!(
            a.deliver("c", None, &msg).await.unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn deliver_post_with_files_uploads_then_attaches_ids() {
        let mock = MockServer::start().await;
        // First the multipart upload returns a file id.
        Mock::given(method("POST"))
            .and(path("/api/v4/files"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({ "file_infos": [{ "id": "f-1" }] })),
            )
            .mount(&mock)
            .await;
        // Then the post body must carry that file_id.
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "p-with-file"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "see attached"}),
            files: vec![ironclaw_types::OutboundFile {
                filename: "a.txt".into(),
                data: vec![1, 2, 3],
            }],
        };
        let id = a.deliver("c", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("p-with-file"));
    }

    #[tokio::test]
    async fn deliver_edit_with_files_is_bad_request() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"action":"edit","target_id":"p1","text":"x"}),
            files: vec![ironclaw_types::OutboundFile {
                filename: "a.txt".into(),
                data: vec![0; 1],
            }],
        };
        match a.deliver("c", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("edit") && m.contains("file")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_bad_request() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"action":"who-knows"}),
            files: vec![],
        };
        assert!(matches!(
            a.deliver("c", None, &msg).await.unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn supports_threads_is_true() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        assert!(a.supports_threads());
    }

    #[tokio::test]
    async fn defaults_are_no_ops() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        a.subscribe("c", None).await.unwrap();
        a.set_typing("c", None).await.unwrap();
        assert!(a.open_dm("u").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn drop_cancels_running_server() {
        let mock = MockServer::start().await;
        let handle = tokio::spawn(async {
            sleep(Duration::from_secs(60)).await;
        });
        let aborted_marker = handle.abort_handle();
        let a = make(&mock, None);
        a.set_server_handle(handle);
        drop(a);
        for _ in 0..50 {
            if aborted_marker.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(aborted_marker.is_finished());
    }

    #[test]
    fn debug_format_renders() {
        // No tokio runtime needed for the debug branch.
        let api = MattermostApi::new("https://chat.example", "t");
        let a = MattermostAdapter::new(ChannelType::new("mattermost"), api, None);
        let s = format!("{a:?}");
        assert!(s.contains("MattermostAdapter"));
    }
}
