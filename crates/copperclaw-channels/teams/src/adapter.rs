//! Microsoft Teams [`ChannelAdapter`] implementation.
//!
//! See the crate-level docs in [`crate`] for the contract overview.
//!
//! The adapter is constructed by [`crate::factory::TeamsFactory`] but is
//! also publicly visible so downstream tests can drive it directly without
//! booting the full webhook server.

use crate::api::{
    TeamsApi, build_adaptive_breadcrumb, build_adaptive_card, build_adaptive_collapsible,
    build_adaptive_diff, build_adaptive_error, build_adaptive_thinking, build_adaptive_todo_list,
};
use crate::emoji::shortcode_to_reaction_type;
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, Card, ChannelAdapter, DiffCard, DmHandle, ErrorCard, ThinkingBlock,
    TodoList,
};
use copperclaw_types::{ChannelType, MessageKind, OutboundMessage};
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

        // Files are supported for channel posts (uploaded into the
        // channel's SharePoint files folder, then attached by reference).
        // 1:1 / group chats route via OneDrive which requires delegated
        // user auth — the bot's app-only token can't reach there. Reject
        // up-front so the failure mode is visible to the caller.
        if !message.files.is_empty() && matches!(target, TeamsTarget::Chat { .. }) {
            return Err(AdapterError::Unsupported(
                "teams chat (1:1 / group) attachments need delegated OneDrive auth; \
                 bot app-only auth cannot upload there. Use a channel target."
                    .into(),
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
            } => {
                // Channel attachments: resolve the channel's files
                // folder once, upload each file into it, then attach
                // by reference on the message.
                let attachments = if message.files.is_empty() {
                    Vec::new()
                } else {
                    let folder = self
                        .api
                        .get_channel_files_folder(team_id, channel_id)
                        .await?;
                    let mut out = Vec::with_capacity(message.files.len());
                    for f in &message.files {
                        out.push(
                            self.api
                                .upload_channel_file(&folder, &f.filename, f.data.clone())
                                .await?,
                        );
                    }
                    out
                };

                if attachments.is_empty() {
                    match thread_id {
                        Some(parent) => {
                            self.api
                                .post_channel_reply(
                                    team_id,
                                    channel_id,
                                    parent,
                                    content,
                                    content_type,
                                )
                                .await?
                                .id
                        }
                        None => {
                            self.api
                                .post_channel_message(
                                    team_id,
                                    channel_id,
                                    content,
                                    content_type,
                                )
                                .await?
                                .id
                        }
                    }
                } else {
                    // Reply-in-thread + attachments isn't supported by
                    // the Graph endpoint we use; warn and post as a
                    // top-level channel message instead. Threading
                    // attachments is a known gap in the Graph reply
                    // surface (post_channel_reply only takes a body,
                    // not attachments).
                    if thread_id.is_some() {
                        tracing::warn!(
                            "teams: outbound has both thread_id and files; \
                             attachments posted as top-level channel message \
                             (Graph reply endpoint does not accept attachments)"
                        );
                    }
                    self.api
                        .post_channel_message_with_attachments(
                            team_id,
                            channel_id,
                            content,
                            content_type,
                            &attachments,
                        )
                        .await?
                        .id
                }
            }
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

    /// MS Graph `chatMessage` body content is documented as supporting up
    /// to 28 KB. We treat that as a char cap (the host's splitter is char-
    /// based; 28 KB is a safe under-approximation in any UTF-8 input).
    fn max_message_chars(&self) -> Option<usize> {
        Some(28_000)
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

    /// Render and deliver a [`Card`] natively as a Microsoft Teams
    /// Adaptive Card attachment (`application/vnd.microsoft.card.adaptive`).
    ///
    /// The card JSON is produced by [`build_adaptive_card`] and attached
    /// to a Graph `chatMessage` via [`build_adaptive_message_body`]; the
    /// fallback notification text uses [`Card::to_text_fallback`].
    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        _to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        let card_json = build_adaptive_card(card);
        let fallback = card.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`Breadcrumb`] chip as an Adaptive Card.
    ///
    /// When `existing_message_id` is provided we PATCH the Graph
    /// `chatMessage` in place so the user sees `running → done /
    /// failed` evolve on the same chip; otherwise we POST a new one.
    /// Graph supports message edits via `PATCH .../messages/{id}` —
    /// this is the Teams answer to the Slack `chat.update` /
    /// Telegram `editMessageText` edit-in-place pattern.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        let card_json = build_adaptive_breadcrumb(breadcrumb);
        let fallback = breadcrumb.to_text_fallback();
        if let Some(existing) = existing_message_id {
            self.patch_adaptive(&target, existing, &card_json, &fallback)
                .await?;
            return Ok(Some(existing.to_owned()));
        }
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`DiffCard`] as an Adaptive Card —
    /// monospace `TextBlock` per hunk for the diff body (Adaptive
    /// Cards 1.4 has no `CodeBlock` element; the 1.5 `CodeBlock` would
    /// add syntax-aware highlighting but isn't broadly client-supported
    /// yet, so we standardise on the universally-rendered monospace
    /// fallback).
    async fn deliver_diff(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        let card_json = build_adaptive_diff(diff);
        let fallback = diff.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver an [`ErrorCard`] as an Adaptive Card with an
    /// `attention`-styled top container and a red-bold header — the
    /// Teams answer to Slack's `attachments.color: "danger"`.
    async fn deliver_error(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        let card_json = build_adaptive_error(err);
        let fallback = err.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`TodoList`] as an Adaptive Card. When
    /// `existing_message_id` is set we PATCH the chip in place so the
    /// user sees items tick through; otherwise we POST a fresh one.
    /// `pin_hint` is ignored — Microsoft Graph has no documented
    /// "pin a chatMessage to a channel" API for bots, so the chip
    /// surface relies on conventional placement (the message stays in
    /// the channel feed); this matches the trait contract that
    /// adapters without a pin API SHOULD silently no-op.
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        list: &TodoList,
        existing_message_id: Option<&str>,
        _pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        let card_json = build_adaptive_todo_list(list);
        let fallback = list.to_text_fallback();
        if let Some(existing) = existing_message_id {
            self.patch_adaptive(&target, existing, &card_json, &fallback)
                .await?;
            return Ok(Some(existing.to_owned()));
        }
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a long-output expander surface as an
    /// Adaptive Card with `Action.ToggleVisibility` flipping the
    /// hidden `Container` holding the full body — Teams's idiomatic
    /// disclosure-widget primitive.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        let card_json = build_adaptive_collapsible(text, summary, preview_lines);
        // The fallback `text` field is the *summary* only — pushing the
        // entire body into a notification preview would defeat the
        // collapsible's purpose.
        let id = self
            .post_adaptive(&target, thread_id, &card_json, summary)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`ThinkingBlock`] as an Adaptive Card with
    /// a collapsed `Container` (hidden via `isVisible: false`, toggled
    /// by an `Action.ToggleVisibility` button) so reasoning stays out
    /// of the chat flow until the user opts to expand it. Redacted
    /// blocks emit the placeholder — the raw blob never reaches the
    /// wire even in the fallback `text` field.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        let target = TeamsTarget::parse(platform_id)?;
        let card_json = build_adaptive_thinking(thinking);
        let fallback = thinking.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }
}

impl TeamsAdapter {
    /// Shared "post an adaptive card to either a channel (optionally
    /// inside a thread) or a 1:1/group chat" helper. Returns the
    /// platform-side message id.
    async fn post_adaptive(
        &self,
        target: &TeamsTarget,
        thread_id: Option<&str>,
        card_json: &Value,
        fallback_text: &str,
    ) -> Result<String, AdapterError> {
        let id = match target {
            TeamsTarget::Channel { team_id, channel_id } => match thread_id {
                Some(parent) => self
                    .api
                    .post_channel_adaptive_card_reply(
                        team_id,
                        channel_id,
                        parent,
                        card_json,
                        fallback_text,
                    )
                    .await?
                    .id,
                None => self
                    .api
                    .post_channel_adaptive_card(team_id, channel_id, card_json, fallback_text)
                    .await?
                    .id,
            },
            TeamsTarget::Chat { chat_id } => self
                .api
                .post_chat_adaptive_card(chat_id, card_json, fallback_text)
                .await?
                .id,
        };
        Ok(id)
    }

    /// Shared "patch an existing adaptive card in place" helper. Used
    /// by `deliver_breadcrumb` / `deliver_todo_list` edit-in-place
    /// paths. Graph PATCH on `/chatMessages/{id}` accepts a fresh
    /// `body` + `attachments[]` payload — we ship the new adaptive
    /// card with the same wire shape `post_adaptive` produces.
    async fn patch_adaptive(
        &self,
        target: &TeamsTarget,
        message_id: &str,
        card_json: &Value,
        fallback_text: &str,
    ) -> Result<(), AdapterError> {
        match target {
            TeamsTarget::Channel { team_id, channel_id } => {
                self.api
                    .edit_channel_adaptive_card(
                        team_id,
                        channel_id,
                        message_id,
                        card_json,
                        fallback_text,
                    )
                    .await
            }
            TeamsTarget::Chat { chat_id } => {
                self.api
                    .edit_chat_adaptive_card(chat_id, message_id, card_json, fallback_text)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::MessageKind;
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
    async fn deliver_channel_files_upload_and_attach_by_reference() {
        let server = MockServer::start().await;
        // 1. filesFolder lookup returns drive + folder ids.
        Mock::given(method("GET"))
            .and(path("/teams/T1/channels/C1/filesFolder"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "FOLDER1",
                "parentReference": { "driveId": "DRV1" },
            })))
            .mount(&server)
            .await;
        // 2. upload returns a driveItem id + webUrl.
        Mock::given(method("PUT"))
            .and(path("/drives/DRV1/items/FOLDER1:/a.txt:/content"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "DI1",
                "webUrl": "https://contoso.sharepoint.com/sites/team/Shared%20Documents/General/a.txt",
            })))
            .mount(&server)
            .await;
        // 3. message post with attachments returns the chat-message id.
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "M-WITH-ATT"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "see attached"}),
            files: vec![copperclaw_types::OutboundFile {
                filename: "a.txt".into(),
                data: vec![1, 2, 3],
            }],
        };
        let id = adapter
            .deliver("team/T1/channel/C1", None, &msg)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("M-WITH-ATT"));
    }

    #[tokio::test]
    async fn deliver_chat_files_returns_unsupported_with_explanation() {
        // 1:1 / group chat attachments need delegated OneDrive auth;
        // app-only bot auth can't reach a user's OneDrive. The
        // adapter rejects up-front rather than failing mid-flight at
        // upload.
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "to chat"}),
            files: vec![copperclaw_types::OutboundFile {
                filename: "a.txt".into(),
                data: vec![1, 2, 3],
            }],
        };
        match adapter.deliver("chat/CHAT1", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => {
                assert!(m.contains("OneDrive") || m.contains("delegated"));
            }
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

    // -----------------------------------------------------------------
    // Adaptive-card overrides (slice 4 — native cards / breadcrumbs /
    // diffs / errors / todo lists / collapsible / thinking).
    //
    // The tests below assert: (a) the right Graph endpoint was called;
    // (b) the request body carried an `application/vnd.microsoft.card.adaptive`
    // attachment with a stringified card JSON; (c) the card content
    // matches the expected Adaptive Card shape (TextBlock / FactSet /
    // Container / Action.* primitives).
    // -----------------------------------------------------------------

    use copperclaw_channels_core::{
        Breadcrumb, BreadcrumbStatus, Card, CardButton, CardField, DiffCard, DiffHunk, DiffLine,
        DiffLineKind, ErrorCard, ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
        TodoListItem,
    };

    /// Extract the (parsed) adaptive-card JSON from the most-recent
    /// POST/PATCH against the mock server. Helper avoids repetition
    /// across the per-method tests.
    async fn last_adaptive_card(server: &MockServer) -> serde_json::Value {
        let reqs = server.received_requests().await.expect("requests");
        let last = reqs.last().expect("at least one request");
        let body: serde_json::Value =
            serde_json::from_slice(&last.body).expect("json body");
        let att = body["attachments"][0].clone();
        assert_eq!(
            att["contentType"], "application/vnd.microsoft.card.adaptive",
            "expected adaptive-card content type"
        );
        let content_str = att["content"].as_str().expect("card content stringified");
        serde_json::from_str(content_str).expect("card content parses as JSON")
    }

    #[tokio::test]
    async fn deliver_card_posts_adaptive_card_attachment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "M-CARD"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Confirm".into()),
            body: Some("Ready?".into()),
            fields: vec![CardField {
                label: "Item".into(),
                value: "Espresso".into(),
                inline: false,
            }],
            buttons: vec![
                CardButton {
                    label: "Go".into(),
                    value: Some("go".into()),
                    url: None,
                    style: Some("primary".into()),
                },
                CardButton {
                    label: "More".into(),
                    value: None,
                    url: Some("https://example.com".into()),
                    style: None,
                },
            ],
            image_url: None,
        };
        let id = adapter
            .deliver_card("team/T1/channel/C1", None, &card, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("M-CARD"));
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["type"], "AdaptiveCard");
        // Title TextBlock + body TextBlock + FactSet = at least 3 body elements.
        let body = card_json["body"].as_array().expect("body array");
        assert!(body.len() >= 3);
        assert_eq!(body[0]["type"], "TextBlock");
        assert_eq!(body[0]["text"], "Confirm");
        assert_eq!(body[0]["weight"], "Bolder");
        // FactSet entry should carry the Item fact.
        let factset = body.iter().find(|b| b["type"] == "FactSet").expect("factset");
        assert_eq!(factset["facts"][0]["title"], "Item");
        assert_eq!(factset["facts"][0]["value"], "Espresso");
        // Actions: Submit + OpenUrl.
        let actions = card_json["actions"].as_array().expect("actions");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["type"], "Action.Submit");
        assert_eq!(actions[0]["title"], "Go");
        assert_eq!(actions[0]["style"], "positive");
        assert_eq!(actions[0]["data"]["value"], "go");
        assert_eq!(actions[1]["type"], "Action.OpenUrl");
        assert_eq!(actions[1]["url"], "https://example.com");
    }

    #[tokio::test]
    async fn deliver_card_to_chat_posts_to_chat_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chats/CHAT1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "M-CHAT-CARD"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        let id = adapter
            .deliver_card("chat/CHAT1", None, &card, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("M-CHAT-CARD"));
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["body"][0]["text"], "Hi");
    }

    #[tokio::test]
    async fn deliver_card_in_thread_uses_replies_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages/PARENT/replies"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "RID-CARD"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Reply".into()),
            ..Card::default()
        };
        let id = adapter
            .deliver_card("team/T1/channel/C1", Some("PARENT"), &card, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("RID-CARD"));
    }

    #[tokio::test]
    async fn deliver_breadcrumb_running_posts_new_card() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "BC-1"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = Breadcrumb::running("shell").with_detail("cargo check");
        let id = adapter
            .deliver_breadcrumb("team/T1/channel/C1", None, &bc, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("BC-1"));
        let card_json = last_adaptive_card(&server).await;
        let text = card_json["body"][0]["text"].as_str().unwrap();
        assert!(text.contains("`shell`"));
        assert!(text.contains("cargo check"));
        // Running breadcrumbs render with the `Accent` colour.
        assert_eq!(card_json["body"][0]["color"], "Accent");
    }

    #[tokio::test]
    async fn deliver_breadcrumb_with_existing_id_patches_in_place() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/teams/T1/channels/C1/messages/BC-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = Breadcrumb {
            tool_name: "shell".into(),
            detail: Some("cargo test".into()),
            status: BreadcrumbStatus::Done,
            summary: Some("passed (0.4s)".into()),
        };
        let id = adapter
            .deliver_breadcrumb("team/T1/channel/C1", None, &bc, Some("BC-1"))
            .await
            .unwrap();
        // Edit-in-place returns the original id so the host keeps editing the same chip.
        assert_eq!(id.as_deref(), Some("BC-1"));
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["body"][0]["color"], "Good");
        let text = card_json["body"][0]["text"].as_str().unwrap();
        assert!(text.contains("passed (0.4s)"));
    }

    #[tokio::test]
    async fn deliver_breadcrumb_failed_uses_attention_color() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "BC-FAIL"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = Breadcrumb::running("shell")
            .with_detail("bad command")
            .finished(false, Some("exit 1".into()));
        adapter
            .deliver_breadcrumb("team/T1/channel/C1", None, &bc, None)
            .await
            .unwrap();
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["body"][0]["color"], "Attention");
        let text = card_json["body"][0]["text"].as_str().unwrap();
        assert!(text.contains("failed: exit 1"));
    }

    #[tokio::test]
    async fn deliver_diff_posts_adaptive_card_with_monospace_hunks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "DIFF-1"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let diff = DiffCard {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Remove,
                        text: "fn old() {}".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Add,
                        text: "fn new() {}".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let id = adapter
            .deliver_diff("team/T1/channel/C1", None, &diff)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("DIFF-1"));
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        // Header + one hunk = 2 blocks.
        assert_eq!(body.len(), 2);
        assert!(body[0]["text"].as_str().unwrap().contains("src/main.rs"));
        assert!(body[0]["text"].as_str().unwrap().contains("+1 / -1"));
        assert_eq!(body[1]["fontType"], "Monospace");
        let hunk_text = body[1]["text"].as_str().unwrap();
        assert!(hunk_text.contains("@@ -1,1 +1,1 @@"));
        assert!(hunk_text.contains("-fn old() {}"));
        assert!(hunk_text.contains("+fn new() {}"));
    }

    #[tokio::test]
    async fn deliver_diff_truncated_marker_appears_in_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "DIFF-T"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let diff = DiffCard {
            path: "huge.rs".into(),
            language: None,
            hunks: vec![],
            added: 50,
            removed: 100,
            truncated: true,
        };
        adapter
            .deliver_diff("team/T1/channel/C1", None, &diff)
            .await
            .unwrap();
        let card_json = last_adaptive_card(&server).await;
        assert!(card_json["body"][0]["text"]
            .as_str()
            .unwrap()
            .contains("truncated"));
    }

    #[tokio::test]
    async fn deliver_error_posts_attention_container() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "ERR-1"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let err = ErrorCard::new(ErrorCardKind::Internal, "shell timed out")
            .with_details("stderr: ENOENT");
        let id = adapter
            .deliver_error("team/T1/channel/C1", None, &err)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("ERR-1"));
        let card_json = last_adaptive_card(&server).await;
        let outer = &card_json["body"][0];
        assert_eq!(outer["type"], "Container");
        assert_eq!(outer["style"], "attention");
        let inner = outer["items"].as_array().unwrap();
        assert_eq!(inner[0]["color"], "Attention");
        assert!(inner[0]["text"]
            .as_str()
            .unwrap()
            .contains("[ERROR: tool]"));
        assert_eq!(inner[1]["text"], "shell timed out");
        assert_eq!(inner[2]["fontType"], "Monospace");
        assert!(inner[2]["text"].as_str().unwrap().contains("ENOENT"));
    }

    #[tokio::test]
    async fn deliver_error_retryable_appends_retry_footer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "ERR-R"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let err = ErrorCard::new(ErrorCardKind::Delivery, "graph returned 502").retryable();
        adapter
            .deliver_error("team/T1/channel/C1", None, &err)
            .await
            .unwrap();
        let card_json = last_adaptive_card(&server).await;
        let inner = card_json["body"][0]["items"].as_array().unwrap();
        let last = inner.last().unwrap();
        assert!(last["text"]
            .as_str()
            .unwrap()
            .contains("will retry automatically"));
    }

    #[tokio::test]
    async fn deliver_todo_list_first_emit_posts_card() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "TL-1"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let list = TodoList {
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "first".into(),
                    status: TodoItemStatus::Completed,
                },
                TodoListItem {
                    id: 2,
                    text: "second".into(),
                    status: TodoItemStatus::InProgress,
                },
                TodoListItem {
                    id: 3,
                    text: "third".into(),
                    status: TodoItemStatus::Pending,
                },
            ],
            title: Some("Plan A".into()),
        };
        let id = adapter
            .deliver_todo_list("team/T1/channel/C1", None, &list, None, true)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("TL-1"));
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        // Header + 3 items + footer.
        assert_eq!(body.len(), 5);
        assert!(body[0]["text"].as_str().unwrap().contains("Plan A (1/3)"));
        assert!(body[1]["text"].as_str().unwrap().starts_with("[x]"));
        assert_eq!(body[1]["color"], "Good");
        assert!(body[2]["text"].as_str().unwrap().starts_with("[~]"));
        assert_eq!(body[2]["color"], "Warning");
        assert!(body[3]["text"].as_str().unwrap().starts_with("[ ]"));
        // Footer summarises counts.
        assert!(body[4]["text"]
            .as_str()
            .unwrap()
            .contains("1/3 done"));
    }

    #[tokio::test]
    async fn deliver_todo_list_with_existing_id_patches_in_place() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/teams/T1/channels/C1/messages/TL-1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let list = TodoList {
            items: vec![TodoListItem {
                id: 1,
                text: "first".into(),
                status: TodoItemStatus::Completed,
            }],
            title: None,
        };
        let id = adapter
            .deliver_todo_list("team/T1/channel/C1", None, &list, Some("TL-1"), false)
            .await
            .unwrap();
        // The edit returns the original message id so subsequent edits target the same chip.
        assert_eq!(id.as_deref(), Some("TL-1"));
    }

    #[tokio::test]
    async fn deliver_collapsible_posts_with_toggle_action() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "EXP-1"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver_collapsible(
                "team/T1/channel/C1",
                None,
                "line1\nline2\nline3\nline4",
                "shell produced 4 lines",
                &["line1".into(), "line2".into()],
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("EXP-1"));
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        // Summary + preview + collapsed container.
        assert_eq!(body.len(), 3);
        assert_eq!(body[0]["text"], "shell produced 4 lines");
        assert_eq!(body[1]["fontType"], "Monospace");
        assert_eq!(body[2]["type"], "Container");
        assert_eq!(body[2]["id"], "expander_full");
        assert_eq!(body[2]["isVisible"], false);
        let actions = card_json["actions"].as_array().unwrap();
        assert_eq!(actions[0]["type"], "Action.ToggleVisibility");
        assert_eq!(actions[0]["targetElements"][0], "expander_full");
    }

    #[tokio::test]
    async fn deliver_collapsible_without_preview_still_renders_container() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "EXP-NP"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        adapter
            .deliver_collapsible(
                "team/T1/channel/C1",
                None,
                "x".repeat(8000).as_str(),
                "shell 8000 chars",
                &[],
            )
            .await
            .unwrap();
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        // Summary + collapsed container only (no preview block).
        assert_eq!(body.len(), 2);
        assert_eq!(body[1]["type"], "Container");
    }

    #[tokio::test]
    async fn deliver_thinking_posts_collapsed_container_with_toggle() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "TH-1"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let t = ThinkingBlock::visible("Let me think carefully.").with_model("claude-opus-4-7");
        let id = adapter
            .deliver_thinking("team/T1/channel/C1", None, &t)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("TH-1"));
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        // Header + collapsed container.
        assert_eq!(body.len(), 2);
        assert!(body[0]["text"]
            .as_str()
            .unwrap()
            .contains("claude-opus-4-7"));
        assert_eq!(body[1]["type"], "Container");
        assert_eq!(body[1]["isVisible"], false);
        assert_eq!(
            body[1]["items"][0]["text"],
            "Let me think carefully."
        );
        let actions = card_json["actions"].as_array().unwrap();
        assert_eq!(actions[0]["type"], "Action.ToggleVisibility");
    }

    #[tokio::test]
    async fn deliver_thinking_redacted_emits_placeholder_only() {
        // Redacted blocks MUST NOT leak the raw blob on the wire — not
        // in the card body and not in the fallback text field.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "TH-RED"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let t = ThinkingBlock::redacted("opaque-blob-secret");
        adapter
            .deliver_thinking("team/T1/channel/C1", None, &t)
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body_bytes = &reqs.last().unwrap().body;
        let body_str = std::str::from_utf8(body_bytes).unwrap();
        assert!(
            !body_str.contains("opaque-blob-secret"),
            "raw redacted blob must never reach the wire"
        );
        assert!(body_str.contains("(redacted reasoning)"));
    }

    #[tokio::test]
    async fn deliver_card_propagates_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        match adapter
            .deliver_card("team/T1/channel/C1", None, &card, None)
            .await
        {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_breadcrumb_patch_propagates_rate_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/teams/T1/channels/C1/messages/BC-1"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let bc = Breadcrumb::running("shell");
        match adapter
            .deliver_breadcrumb("team/T1/channel/C1", None, &bc, Some("BC-1"))
            .await
        {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(3)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_card_malformed_platform_id_is_bad_request() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        match adapter.deliver_card("nope", None, &card, None).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_card_includes_html_attachment_placeholder() {
        // The body's HTML must reference the attachment id so Teams
        // renders the card inline rather than treating the attachment
        // as orphaned.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "M"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        adapter
            .deliver_card("team/T1/channel/C1", None, &card, None)
            .await
            .unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs.last().unwrap().body).unwrap();
        let html = body["body"]["content"].as_str().unwrap();
        let id = body["attachments"][0]["id"].as_str().unwrap();
        assert!(
            html.contains(&format!("<attachment id=\"{id}\">")),
            "html body must reference the attachment id: {html}"
        );
        assert_eq!(body["body"]["contentType"], "html");
    }

    #[tokio::test]
    async fn deliver_card_image_only_emits_image_block() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "IMG"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            image_url: Some("https://example.com/x.png".into()),
            ..Card::default()
        };
        adapter
            .deliver_card("team/T1/channel/C1", None, &card, None)
            .await
            .unwrap();
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        assert_eq!(body[0]["type"], "Image");
        assert_eq!(body[0]["url"], "https://example.com/x.png");
    }

    #[tokio::test]
    async fn deliver_card_dropped_button_has_neither_value_nor_url() {
        // Buttons missing both `value` and `url` are silently
        // filtered out (the card validator rejects them upstream;
        // we belt-and-braces in the renderer).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/teams/T1/channels/C1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "BTN"})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            buttons: vec![CardButton {
                label: "Orphan".into(),
                value: None,
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        adapter
            .deliver_card("team/T1/channel/C1", None, &card, None)
            .await
            .unwrap();
        let card_json = last_adaptive_card(&server).await;
        // Card serialised without any actions (skipped via the filter).
        assert!(card_json.get("actions").is_none());
    }
}
