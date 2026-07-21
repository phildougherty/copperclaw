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
//! `subscribe` and `open_dm` use the trait defaults: Mattermost has no
//! "subscribe to channel" call for bots (the outgoing webhook decides
//! what flows in), and DMs are just a private channel id the caller
//! already has.
//!
//! `set_typing` publishes the bot's "…is typing" indicator via `POST
//! /api/v4/users/me/typing` (the REST shortcut for the websocket
//! `user_typing` action — no persistent socket needed). `add_reaction`
//! (the host-driven trait hook) and `deliver_breadcrumb` (an in-place
//! edited tool-progress chip) are native overrides too.
//!
//! File attachments aren't yet implemented; an outbound with files
//! returns [`AdapterError::Unsupported`] explicitly rather than
//! silently dropping the files.

use crate::api::MattermostApi;
use crate::render;
use async_trait::async_trait;
use copperclaw_channels_core::markdown::{Flavor, render as render_markdown};
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, Card, ChannelAdapter, DiffCard, ErrorCard, ThinkingBlock, TodoList,
};
use copperclaw_types::{ChannelType, OutboundMessage};
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
        let server_state = self
            .server
            .lock()
            .map_or("poisoned", |g| g.as_ref().map_or("stopped", |_| "running"));
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
    pub fn new(channel_type: ChannelType, api: MattermostApi, bot_user_id: Option<String>) -> Self {
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

    /// Mattermost's server-side post limit is `MaxPostSize`, which defaults
    /// to `PostMessageMaxRunesV1 = 4000` (`server/public/model/post.go`).
    /// Installs that ran the v2 migration allow `PostMessageMaxRunesV2 =
    /// 16383`, but that is opt-in and admin-configurable, so we target the
    /// default every server accepts. The server counts *runes*, which is
    /// what this trait counts, so no byte/char mismatch here.
    fn max_message_chars(&self) -> Option<usize> {
        Some(4000)
    }

    /// Publish the bot's typing indicator via `POST
    /// /api/v4/users/me/typing`. The server fans the `user_typing` event
    /// out to connected clients, giving users an "agent is working"
    /// signal during a run. `thread_id`, when present, scopes the
    /// indicator to that thread root (`parent_id`); otherwise it targets
    /// the whole channel. Best-effort: a failure here is surfaced to the
    /// caller (the delivery loop treats typing as advisory).
    async fn set_typing(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        self.api.post_typing("me", platform_id, thread_id).await
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
                let raw_text = content
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("missing `text` in outbound content".into())
                    })?;
                // U6: route the agent's canonical Markdown through the shared
                // renderer ([`Flavor::Mattermost`] — full CommonMark) so
                // bullets normalise and the plain-text path shares one
                // formatter with every other channel.
                let text = render_markdown(raw_text, Flavor::Mattermost);
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
                    .create_post_with_files(platform_id, &text, thread_id, &file_ids)
                    .await?;
                Ok(Some(id))
            }
            "edit" => {
                let target = content
                    .get("target_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| AdapterError::BadRequest("edit requires `target_id`".into()))?;
                let text = content
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| AdapterError::BadRequest("edit requires `text`".into()))?;
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

    /// Edit a previously delivered post in place via
    /// `PUT /api/v4/posts/{id}/patch`. `external_id` is the post id from
    /// the original `deliver`. Enables the M18 Task HUD to self-edit one
    /// status post rather than spam a new message per tool batch.
    async fn edit_message(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
        external_id: &str,
        new_text: &str,
    ) -> Result<(), AdapterError> {
        let ct = self.channel_type().as_str();
        match self.api.update_post(external_id, new_text).await {
            Ok(()) => {
                copperclaw_metrics::inc_hud_edit(ct, "ok");
                copperclaw_metrics::inc_adapter_edit_message(ct, "ok");
                Ok(())
            }
            Err(e) => {
                copperclaw_metrics::inc_hud_edit(ct, "error");
                copperclaw_metrics::inc_adapter_edit_message(ct, "error");
                Err(e)
            }
        }
    }

    /// Native tool-progress breadcrumb — a compact Markdown chip
    /// (`[~] `shell` · cargo check`, see [`render::render_breadcrumb`]).
    /// When `existing_message_id` is known we edit the original chip in
    /// place via `PUT /api/v4/posts/{id}/patch` so the user sees
    /// `Running…` → `Done`; otherwise we post a fresh chip. `thread_id`
    /// threads the initial post as usual.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        copperclaw_metrics::inc_adapter_rich_render(self.channel_type().as_str(), "breadcrumb");
        let text = render::render_breadcrumb(breadcrumb);
        if let Some(existing) = existing_message_id {
            self.api.update_post(existing, &text).await?;
            return Ok(Some(existing.to_owned()));
        }
        let id = self.api.create_post(platform_id, &text, thread_id).await?;
        Ok(Some(id))
    }

    /// Host-driven reaction — the trait hook the delivery service calls
    /// (distinct from the `reaction` egress action on [`Self::deliver`]).
    /// Routes to `POST /api/v4/reactions` on behalf of the configured
    /// `bot_user_id` (Mattermost binds reactions to a user). Falls
    /// through to [`AdapterError::Unsupported`] when no bot id is
    /// configured so the host can post a fresh message instead.
    async fn add_reaction(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
        external_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        let user = self.bot_user_id.as_deref().ok_or_else(|| {
            AdapterError::Unsupported("reaction requires bot_user_id in mattermost config".into())
        })?;
        self.api.add_reaction(user, external_id, emoji).await
    }

    /// Native card — a Mattermost Markdown post (heading + body + field
    /// list + button list + inline image). See [`render::render_card`].
    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        _to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        copperclaw_metrics::inc_adapter_rich_render(self.channel_type().as_str(), "card");
        let text = render::render_card(card);
        let id = self.api.create_post(platform_id, &text, thread_id).await?;
        Ok(Some(id))
    }

    /// Native diff — a fenced ` ```diff ` block Mattermost colourises.
    async fn deliver_diff(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        copperclaw_metrics::inc_adapter_rich_render(self.channel_type().as_str(), "diff");
        let text = render::render_diff(diff);
        let id = self.api.create_post(platform_id, &text, thread_id).await?;
        Ok(Some(id))
    }

    /// Native long-output expander — bold summary + fenced preview.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        copperclaw_metrics::inc_adapter_rich_render(self.channel_type().as_str(), "collapsible");
        let body = render::render_collapsible(text, summary, preview_lines);
        let id = self.api.create_post(platform_id, &body, thread_id).await?;
        Ok(Some(id))
    }

    /// Native todo list — a Markdown task list, edited in place on
    /// mutation when the prior post id is known. Mattermost bots have no
    /// pin API we model, so `pin_hint` is a silent no-op.
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        list: &TodoList,
        existing_message_id: Option<&str>,
        _pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        copperclaw_metrics::inc_adapter_rich_render(self.channel_type().as_str(), "todo");
        let text = render::render_todo_list(list);
        if let Some(existing) = existing_message_id {
            self.api.update_post(existing, &text).await?;
            return Ok(Some(existing.to_owned()));
        }
        let id = self.api.create_post(platform_id, &text, thread_id).await?;
        Ok(Some(id))
    }

    /// Native thinking block — a Markdown blockquote. Redacted blocks
    /// emit the placeholder only.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        copperclaw_metrics::inc_adapter_rich_render(self.channel_type().as_str(), "thinking");
        let text = render::render_thinking(thinking);
        let id = self.api.create_post(platform_id, &text, thread_id).await?;
        Ok(Some(id))
    }

    /// Native error card — a bold `[ERROR: kind]` header + fenced detail.
    async fn deliver_error(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        copperclaw_metrics::inc_adapter_rich_render(self.channel_type().as_str(), "error");
        let text = render::render_error(err);
        let id = self.api.create_post(platform_id, &text, thread_id).await?;
        Ok(Some(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::MessageKind;
    use serde_json::json;
    use tokio::time::{Duration, sleep};
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
        MattermostAdapter::new(ChannelType::new("mattermost"), api, bot.map(str::to_string))
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
            files: vec![copperclaw_types::OutboundFile {
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
            files: vec![copperclaw_types::OutboundFile {
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

    /// 4000 is `PostMessageMaxRunesV1`, the default `MaxPostSize` on every
    /// Mattermost server. The v2 limit (16383) requires an opt-in
    /// migration, so we target the value that always works.
    #[tokio::test]
    async fn max_message_chars_is_the_default_max_post_size() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        assert_eq!(a.max_message_chars(), Some(4000));
    }

    #[tokio::test]
    async fn subscribe_and_open_dm_are_no_ops() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        a.subscribe("c", None).await.unwrap();
        assert!(a.open_dm("u").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_typing_posts_to_users_me_typing() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/users/me/typing"))
            .and(wiremock::matchers::body_string_contains(
                "\"channel_id\":\"c1\"",
            ))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        a.set_typing("c1", None).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_scopes_to_thread_parent() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/users/me/typing"))
            .and(wiremock::matchers::body_string_contains(
                "\"parent_id\":\"root-2\"",
            ))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        a.set_typing("c1", Some("root-2")).await.unwrap();
    }

    #[tokio::test]
    async fn add_reaction_trait_hook_hits_reactions_api_with_bot() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/reactions"))
            .and(wiremock::matchers::body_string_contains(
                "\"post_id\":\"p-9\"",
            ))
            .and(wiremock::matchers::body_string_contains(
                "\"user_id\":\"bot\"",
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"emoji_name":"+1"})))
            .mount(&mock)
            .await;
        let a = make(&mock, Some("bot"));
        a.add_reaction("c1", None, "p-9", "+1").await.unwrap();
    }

    #[tokio::test]
    async fn add_reaction_trait_hook_without_bot_is_unsupported() {
        let mock = MockServer::start().await;
        let a = make(&mock, None);
        match a.add_reaction("c1", None, "p-9", "+1").await.unwrap_err() {
            AdapterError::Unsupported(m) => assert!(m.contains("bot_user_id")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_breadcrumb_posts_chip_when_no_existing_id() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .and(wiremock::matchers::body_string_contains("[~] `shell`"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "bc-1"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let bc = Breadcrumb::running("shell").with_detail("cargo check");
        let id = a.deliver_breadcrumb("c1", None, &bc, None).await.unwrap();
        assert_eq!(id.as_deref(), Some("bc-1"));
    }

    #[tokio::test]
    async fn deliver_breadcrumb_edits_chip_in_place_when_id_known() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/posts/bc-7/patch"))
            .and(wiremock::matchers::body_string_contains("[ok] `shell`"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "bc-7"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let bc = Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed".into()));
        let id = a
            .deliver_breadcrumb("c1", None, &bc, Some("bc-7"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("bc-7"));
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

    #[tokio::test]
    async fn deliver_card_posts_markdown() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .and(wiremock::matchers::body_string_contains("### Order"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "card-1"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let card = copperclaw_channels_core::Card {
            title: Some("Order".into()),
            body: Some("Ready?".into()),
            ..Default::default()
        };
        let id = a.deliver_card("c1", None, &card, None).await.unwrap();
        assert_eq!(id.as_deref(), Some("card-1"));
    }

    #[tokio::test]
    async fn deliver_diff_posts_fenced_diff() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .and(wiremock::matchers::body_string_contains("```diff"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "diff-1"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let diff = copperclaw_channels_core::DiffCard {
            path: "a.rs".into(),
            language: None,
            hunks: vec![],
            added: 0,
            removed: 0,
            truncated: false,
        };
        let id = a.deliver_diff("c1", None, &diff).await.unwrap();
        assert_eq!(id.as_deref(), Some("diff-1"));
    }

    #[tokio::test]
    async fn edit_message_calls_patch_endpoint() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/posts/p-9/patch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "p-9"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        a.edit_message("c1", None, "p-9", "hud update")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deliver_todo_list_edits_in_place_when_id_known() {
        let mock = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v4/posts/list-7/patch"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "list-7"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let list = copperclaw_channels_core::TodoList {
            items: vec![copperclaw_channels_core::TodoListItem {
                id: 1,
                text: "task".into(),
                status: copperclaw_channels_core::TodoItemStatus::InProgress,
                blocked_reason: None,
            }],
            title: Some("Plan".into()),
        };
        let id = a
            .deliver_todo_list("c1", None, &list, Some("list-7"), false)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("list-7"));
    }

    #[tokio::test]
    async fn deliver_error_posts_error_header() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v4/posts"))
            .and(wiremock::matchers::body_string_contains("[ERROR: tool]"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "err-1"})))
            .mount(&mock)
            .await;
        let a = make(&mock, None);
        let err = copperclaw_channels_core::ErrorCard::new(
            copperclaw_channels_core::ErrorCardKind::Internal,
            "boom",
        );
        let id = a.deliver_error("c1", None, &err).await.unwrap();
        assert_eq!(id.as_deref(), Some("err-1"));
    }
}
