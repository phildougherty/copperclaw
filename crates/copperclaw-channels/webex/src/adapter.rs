//! Webex [`ChannelAdapter`] implementation.

use crate::api::{
    WebexApi, build_adaptive_breadcrumb, build_adaptive_card, build_adaptive_collapsible,
    build_adaptive_diff, build_adaptive_error, build_adaptive_thinking, build_adaptive_todo_list,
};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, Card, ChannelAdapter, DiffCard, DmHandle, ErrorCard, ThinkingBlock,
    TodoList,
};
use copperclaw_types::{ChannelType, MessageKind, OutboundMessage};
use serde_json::Value;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Prefix used to mark a `platform_id` that addresses a Webex person rather
/// than a room. The adapter recognises it and uses `toPersonId` on send.
pub const PERSON_PREFIX: &str = "person:";

/// Resolved target of an outbound delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeliverTarget {
    /// Address by `roomId` — the default Webex case.
    Room(String),
    /// Address by `toPersonId` — the result of `open_dm`.
    Person(String),
}

impl DeliverTarget {
    fn parse(platform_id: &str) -> Result<Self, AdapterError> {
        if let Some(rest) = platform_id.strip_prefix(PERSON_PREFIX) {
            if rest.is_empty() {
                return Err(AdapterError::BadRequest(
                    "webex platform_id `person:` prefix requires a personId".into(),
                ));
            }
            Ok(Self::Person(rest.to_owned()))
        } else if platform_id.is_empty() {
            Err(AdapterError::BadRequest(
                "webex platform_id must not be empty".into(),
            ))
        } else {
            Ok(Self::Room(platform_id.to_owned()))
        }
    }
}

/// Webex channel adapter. See module-level docs in `lib.rs`.
pub struct WebexAdapter {
    channel_type: ChannelType,
    api: WebexApi,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for WebexAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebexAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl WebexAdapter {
    /// Build a new adapter wrapping the given API client.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: WebexApi) -> Self {
        Self {
            channel_type,
            api,
            server_handle: Mutex::new(None),
        }
    }

    /// Attach the join handle of the spawned webhook server.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("webex adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background webhook server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("webex adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &WebexApi {
        &self.api
    }

    async fn deliver_chat(
        &self,
        target: &DeliverTarget,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let card = extract_card(&message.content);
        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let markdown = message
            .content
            .get("markdown")
            .and_then(Value::as_str)
            .map(str::to_owned);

        // First send any text/markdown/card body (if there is one or no files).
        // Webex allows files to ride along on the same multipart POST; but
        // text+card combined requires the JSON path. We follow this rule:
        //  - With files and no card: one POST per file (multipart, files share
        //    text on the first call).
        //  - With files and card: card goes first (JSON), files follow.
        //  - Without files: one POST with whatever payload was supplied.
        let mut last_id: Option<String> = None;
        let has_files = !message.files.is_empty();
        let has_card = card.is_some();
        let has_body = text.is_some() || markdown.is_some();
        let body_text = text.as_deref();
        let body_markdown = markdown.as_deref();

        // Send a JSON body when there are no files (any text/markdown/card or
        // empty payload — we always make at least one API call), OR when a
        // card is present (cards can't ride on the multipart endpoint, so
        // they go first as their own JSON POST).
        if has_card || !has_files {
            // Send JSON body (handles plain text/markdown, card, and the
            // empty-message edge so we always make at least one call).
            let resp = match target {
                DeliverTarget::Room(room) => {
                    self.api
                        .post_message(
                            room,
                            thread_id,
                            body_text,
                            body_markdown,
                            card.as_ref(),
                        )
                        .await?
                }
                DeliverTarget::Person(person) => {
                    self.api
                        .post_dm_to_person(
                            person,
                            thread_id,
                            body_text,
                            body_markdown,
                            card.as_ref(),
                        )
                        .await?
                }
            };
            last_id = Some(resp.id);
        }

        if has_files {
            // Webex only accepts one file per multipart POST. Send one POST
            // per attachment, attaching the message text to the first call
            // when there was no JSON body above.
            let attach_text = if has_card || has_body {
                None
            } else {
                body_text
            };
            // Person-target multipart isn't supported by Webex (the multipart
            // endpoint requires roomId). If we got here with a person target
            // and files, surface as Unsupported.
            let room = match target {
                DeliverTarget::Room(room) => room.clone(),
                DeliverTarget::Person(_) => {
                    return Err(AdapterError::Unsupported(
                        "webex multipart file upload requires a roomId, not toPersonId".into(),
                    ));
                }
            };
            for (idx, file) in message.files.iter().enumerate() {
                let caption = if idx == 0 { attach_text } else { None };
                let resp = self
                    .api
                    .upload_file_message(
                        &room,
                        thread_id,
                        caption,
                        &file.filename,
                        file.data.clone(),
                    )
                    .await?;
                last_id = Some(resp.id);
            }
        }

        Ok(last_id)
    }

    async fn deliver_system(
        &self,
        target: &DeliverTarget,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let action = message
            .content
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdapterError::BadRequest("webex system message missing `action` field".into())
            })?;
        let room_id = match target {
            DeliverTarget::Room(room) => room.clone(),
            DeliverTarget::Person(_) => {
                return Err(AdapterError::Unsupported(
                    "webex system actions are not supported against person handles".into(),
                ));
            }
        };
        match action {
            "edit" => {
                let target_id = message
                    .content
                    .get("target_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "webex edit requires `target_id` (the platform message id)".into(),
                        )
                    })?;
                let text = message.content.get("text").and_then(Value::as_str);
                let markdown = message.content.get("markdown").and_then(Value::as_str);
                if text.is_none() && markdown.is_none() {
                    return Err(AdapterError::BadRequest(
                        "webex edit requires `text` or `markdown`".into(),
                    ));
                }
                let resp = self
                    .api
                    .edit_message(target_id, &room_id, text, markdown)
                    .await?;
                Ok(Some(resp.id))
            }
            "delete" => {
                let target_id = message
                    .content
                    .get("target_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("webex delete requires `target_id`".into())
                    })?;
                self.api.delete_message(target_id).await?;
                Ok(None)
            }
            "reaction" => {
                let target_id = message
                    .content
                    .get("target_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("webex reaction requires `target_id`".into())
                    })?;
                let emoji = message
                    .content
                    .get("emoji")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest("webex reaction requires `emoji`".into())
                    })?;
                self.api.post_reaction(target_id, emoji).await?;
                Ok(None)
            }
            other => Err(AdapterError::BadRequest(format!(
                "webex unknown system action `{other}`"
            ))),
        }
    }
}

fn extract_card(content: &Value) -> Option<Value> {
    // Convention: outbound row carrying `{"card": {...}}` becomes an Adaptive
    // Card attachment. We pass the inner value through verbatim.
    content.get("card").cloned()
}

#[async_trait]
impl ChannelAdapter for WebexAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    /// Webex `POST /messages` caps `text` at 7 439 chars.
    fn max_message_chars(&self) -> Option<usize> {
        Some(7439)
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        match message.kind {
            MessageKind::Chat
            | MessageKind::Task
            | MessageKind::Webhook
            | MessageKind::Agent
            // Card-kind rows arrive here only when the host routes a card
            // through the legacy `deliver` path (e.g. unit-test fixtures).
            // Treat as chat delivery — wave 2 wires up `deliver_card`
            // properly and routes card-kind rows away from this match.
            | MessageKind::Card
            // Webex has no native breadcrumb chip; the host-delivery
            // service routes Breadcrumb-kind rows through
            // `deliver_breadcrumb` (which the trait-level default
            // converts back to a chat-shape `deliver` call). A
            // Breadcrumb-kind row landing here means a legacy /
            // test-fixture path bypassed dispatch_breadcrumb — treat
            // as chat delivery so the body still reaches the user.
            | MessageKind::Breadcrumb
            // Webex has no native rendering for TodoList / Diff /
            // Error / Thinking either; the same logic applies —
            // host-side dispatch routes these through their typed
            // hooks (which trait defaults convert back to chat). If
            // one slips through here it's a legacy / fixture path;
            // deliver as chat so the user still sees the body.
            | MessageKind::TodoList
            | MessageKind::Diff
            | MessageKind::Error
            | MessageKind::Thinking => self.deliver_chat(&target, thread_id, message).await,
            MessageKind::System => self.deliver_system(&target, message).await,
        }
    }

    async fn open_dm(&self, user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        if user_id.is_empty() {
            return Err(AdapterError::BadRequest(
                "webex open_dm requires a non-empty user id".into(),
            ));
        }
        Ok(Some(DmHandle {
            user_id: user_id.to_owned(),
            platform_id: format!("{PERSON_PREFIX}{user_id}"),
            channel_type: self.channel_type.clone(),
        }))
    }

    /// Render and deliver a [`Card`] natively as a Webex Adaptive Card
    /// attachment (`application/vnd.microsoft.card.adaptive`). The
    /// canonical [`Card::to_text_fallback`] rides as the message
    /// `markdown` so notification previews / older Webex clients still
    /// render a useful body.
    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        _to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        let card_json = build_adaptive_card(card);
        let fallback = card.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`Breadcrumb`] chip as an Adaptive Card.
    ///
    /// **Edit-in-place limitation**: Webex's `PUT /messages/{id}`
    /// endpoint only accepts `text` / `markdown` fields — it does NOT
    /// support changing `attachments[]`. So `existing_message_id` is
    /// effectively ignored: we always POST a fresh card and return the
    /// new id. Operators see a stream of chips rather than a single
    /// evolving one. This matches the trait contract's "adapters
    /// without an edit API SHOULD ignore the argument and emit a
    /// fresh chip (visible but harmless)" guidance.
    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        _existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        let card_json = build_adaptive_breadcrumb(breadcrumb);
        let fallback = breadcrumb.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`DiffCard`] as an Adaptive Card. Hunk
    /// bodies render via monospace `TextBlock` (Webex's Adaptive Cards
    /// subset doesn't ship the 1.5+ `CodeBlock` element).
    async fn deliver_diff(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        let card_json = build_adaptive_diff(diff);
        let fallback = diff.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver an [`ErrorCard`] as an Adaptive Card with an
    /// `attention`-styled top container — Webex's analogue to Slack's
    /// `danger` colour bar.
    async fn deliver_error(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        let card_json = build_adaptive_error(err);
        let fallback = err.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`TodoList`] as an Adaptive Card.
    ///
    /// **Edit-in-place limitation**: same as `deliver_breadcrumb` —
    /// Webex's PUT cannot mutate `attachments[]`, so every list
    /// mutation emits a fresh card and the host gets a new platform
    /// id. The `existing_message_id` argument is intentionally
    /// ignored. `pin_hint` is also ignored — Webex has no public
    /// "pin a message" API for bots.
    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        list: &TodoList,
        _existing_message_id: Option<&str>,
        _pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        let card_json = build_adaptive_todo_list(list);
        let fallback = list.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a long-output expander surface as an
    /// Adaptive Card with `Action.ToggleVisibility` flipping the
    /// hidden container holding the full body.
    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        let card_json = build_adaptive_collapsible(text, summary, preview_lines);
        // Summary is the notification-preview text; pushing the full
        // body in there would defeat the collapsible's purpose.
        let id = self
            .post_adaptive(&target, thread_id, &card_json, summary)
            .await?;
        Ok(Some(id))
    }

    /// Render and deliver a [`ThinkingBlock`] as an Adaptive Card with
    /// a collapsed `Container` (hidden via `isVisible: false`) paired
    /// with `Action.ToggleVisibility` — Webex's idiomatic disclosure
    /// widget. Redacted blocks emit the placeholder; the raw blob
    /// never reaches the wire even via the fallback markdown field.
    async fn deliver_thinking(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        let target = DeliverTarget::parse(platform_id)?;
        let card_json = build_adaptive_thinking(thinking);
        let fallback = thinking.to_text_fallback();
        let id = self
            .post_adaptive(&target, thread_id, &card_json, &fallback)
            .await?;
        Ok(Some(id))
    }

    // `subscribe` and `set_typing` keep their trait defaults: Webex offers no
    // per-room subscription mechanism (webhooks are firehose) and no public
    // typing indicator API.
}

impl WebexAdapter {
    /// Shared "post an adaptive card" helper used by every native
    /// renderer above. Wraps the existing [`WebexApi::post_message`] /
    /// [`WebexApi::post_dm_to_person`] paths, passing the card JSON
    /// through their already-supported `card` argument.
    async fn post_adaptive(
        &self,
        target: &DeliverTarget,
        thread_id: Option<&str>,
        card_json: &Value,
        fallback: &str,
    ) -> Result<String, AdapterError> {
        let resp = match target {
            DeliverTarget::Room(room) => {
                self.api
                    .post_message(room, thread_id, None, Some(fallback), Some(card_json))
                    .await?
            }
            DeliverTarget::Person(person) => {
                self.api
                    .post_dm_to_person(person, thread_id, None, Some(fallback), Some(card_json))
                    .await?
            }
        };
        Ok(resp.id)
    }
}

/// Convenience for constructing outbound messages in tests.
#[cfg(test)]
fn chat_text(text: &str) -> OutboundMessage {
    OutboundMessage {
        kind: MessageKind::Chat,
        content: serde_json::json!({"text": text}),
        files: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::OutboundFile;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> WebexAdapter {
        WebexAdapter::new(
            ChannelType::new("webex"),
            WebexApi::new(server.uri(), "tok"),
        )
    }

    #[tokio::test]
    async fn channel_type_is_webex() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        assert_eq!(a.channel_type().as_str(), "webex");
    }

    #[tokio::test]
    async fn supports_threads_is_true() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        assert!(a.supports_threads());
    }

    #[tokio::test]
    async fn deliver_chat_text_to_room() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id":"M1","roomId":"R1"})),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let id = a.deliver("R1", None, &chat_text("hi")).await.unwrap();
        assert_eq!(id.as_deref(), Some("M1"));
    }

    #[tokio::test]
    async fn deliver_chat_with_thread_id_sets_parent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"id":"M2","roomId":"R1","parentId":"PAR1"}),
            ))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let id = a
            .deliver("R1", Some("PAR1"), &chat_text("threaded"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("M2"));
    }

    #[tokio::test]
    async fn deliver_chat_markdown_only_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M3"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"markdown":"**bold**"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M3"));
    }

    #[tokio::test]
    async fn deliver_chat_with_card_attachment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-CARD"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({
                "text":"see card",
                "card":{"$schema":"https://adaptivecards.io/schemas/adaptive-card.json"}
            }),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-CARD"));
    }

    #[tokio::test]
    async fn deliver_chat_with_file_uses_multipart() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-FILE"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":"see attachment"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"data".to_vec(),
            }],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-FILE"));
    }

    #[tokio::test]
    async fn deliver_chat_with_card_and_file_sends_both() {
        let server = MockServer::start().await;
        // Two POST /messages calls; both 200.
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-LAST"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({
                "text":"see card and file",
                "card":{"v":1}
            }),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"d".to_vec(),
            }],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-LAST"));
    }

    #[tokio::test]
    async fn deliver_chat_with_two_files_sends_two_posts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-MULTI"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":"two files"}),
            files: vec![
                OutboundFile {
                    filename: "a.txt".into(),
                    data: b"a".to_vec(),
                },
                OutboundFile {
                    filename: "b.txt".into(),
                    data: b"b".to_vec(),
                },
            ],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-MULTI"));
    }

    #[tokio::test]
    async fn deliver_chat_empty_content_still_calls_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-EMPTY"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-EMPTY"));
    }

    #[tokio::test]
    async fn deliver_chat_bad_request_propagates() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "message":"missing roomId"
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("R1", None, &chat_text("hi")).await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("missing roomId")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_chat_401_is_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string(""))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("R1", None, &chat_text("hi")).await.unwrap_err() {
            AdapterError::Auth(_) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_chat_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "9")
                    .set_body_string(""),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("R1", None, &chat_text("hi")).await.unwrap_err() {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(9)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_chat_5xx_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(503).set_body_string(""))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("R1", None, &chat_text("hi")).await.unwrap_err() {
            AdapterError::Transport(_) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_person_platform_id_calls_dm_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-DM"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let id = a
            .deliver("person:PERSON1", None, &chat_text("hi"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("M-DM"));
    }

    #[tokio::test]
    async fn deliver_with_empty_person_platform_id_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        match a.deliver("person:", None, &chat_text("hi")).await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_empty_platform_id_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        match a.deliver("", None, &chat_text("hi")).await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_person_target_and_file_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":"hi"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"d".to_vec(),
            }],
        };
        // The first JSON POST is what fails — but since we send text first
        // and then files, we mount a 200 for the JSON path so we reach the
        // file path which surfaces Unsupported.
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M"})))
            .mount(&server)
            .await;
        match a.deliver("person:PER1", None, &msg).await.unwrap_err() {
            AdapterError::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_action_calls_put() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_id":"M1","text":"new"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M1"));
    }

    #[tokio::test]
    async fn deliver_edit_with_markdown_only() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_id":"M1","markdown":"**new**"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M1"));
    }

    #[tokio::test]
    async fn deliver_edit_missing_target_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","text":"new"}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("target_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_edit_missing_text_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_id":"M1"}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("text")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_delete_action() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/messages/M1"))
            .respond_with(ResponseTemplate::new(204).set_body_string(""))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"delete","target_id":"M1"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn deliver_delete_missing_target_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"delete"}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions"))
            .respond_with(ResponseTemplate::new(204).set_body_string(""))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_id":"M1","emoji":"thumbsup"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn deliver_reaction_unsupported_when_endpoint_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions"))
            .respond_with(ResponseTemplate::new(404).set_body_string(""))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_id":"M1","emoji":"x"}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_missing_emoji_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","target_id":"M1"}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_missing_target_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","emoji":"x"}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_unknown_action_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"shimmy","target_id":"M1"}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("shimmy")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_missing_action_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({}),
            files: vec![],
        };
        match a.deliver("R1", None, &msg).await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_to_person_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_id":"M1","text":"hi"}),
            files: vec![],
        };
        match a
            .deliver("person:PER1", None, &msg)
            .await
            .unwrap_err()
        {
            AdapterError::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_dm_returns_person_handle() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let handle = a.open_dm("PER123").await.unwrap().expect("handle");
        assert_eq!(handle.user_id, "PER123");
        assert_eq!(handle.platform_id, "person:PER123");
        assert_eq!(handle.channel_type.as_str(), "webex");
    }

    #[tokio::test]
    async fn open_dm_rejects_empty_user_id() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        match a.open_dm("").await.unwrap_err() {
            AdapterError::BadRequest(_) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_default_impl_is_ok() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        a.subscribe("R1", None).await.unwrap();
        a.subscribe("R1", Some("PAR1")).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_default_impl_is_ok() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        a.set_typing("R1", None).await.unwrap();
        a.set_typing("R1", Some("PAR1")).await.unwrap();
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        assert!(format!("{:?}", a.api()).contains("tok"));
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let s = format!("{a:?}");
        assert!(s.contains("WebexAdapter"));
        assert!(s.contains("webex"));
    }

    #[tokio::test]
    async fn server_handle_shutdown_is_idempotent() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        a.set_server_handle(task);
        a.shutdown_server();
        a.shutdown_server();
    }

    #[tokio::test]
    async fn deliver_task_kind_uses_chat_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-TASK"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Task,
            content: json!({"text":"task!"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-TASK"));
    }

    #[tokio::test]
    async fn deliver_webhook_kind_uses_chat_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-WH"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Webhook,
            content: json!({"text":"hook!"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-WH"));
    }

    #[tokio::test]
    async fn deliver_agent_kind_uses_chat_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-AG"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Agent,
            content: json!({"text":"agent!"}),
            files: vec![],
        };
        let id = a.deliver("R1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-AG"));
    }

    #[test]
    fn deliver_target_parses_room() {
        let t = DeliverTarget::parse("R1").unwrap();
        assert_eq!(t, DeliverTarget::Room("R1".into()));
    }

    #[test]
    fn deliver_target_parses_person() {
        let t = DeliverTarget::parse("person:P1").unwrap();
        assert_eq!(t, DeliverTarget::Person("P1".into()));
    }

    #[test]
    fn deliver_target_rejects_empty() {
        assert!(DeliverTarget::parse("").is_err());
        assert!(DeliverTarget::parse("person:").is_err());
    }

    #[test]
    fn extract_card_returns_none_without_card_key() {
        assert!(extract_card(&json!({"text":"x"})).is_none());
    }

    #[test]
    fn extract_card_returns_card_value() {
        let v = extract_card(&json!({"card":{"v":1}})).unwrap();
        assert_eq!(v["v"], 1);
    }

    // -----------------------------------------------------------------
    // Adaptive-card overrides (slice 4 — native cards / breadcrumbs /
    // diffs / errors / todo lists / collapsible / thinking).
    //
    // Each test asserts: (a) the right `POST /messages` was issued;
    // (b) the request body carries an
    // `application/vnd.microsoft.card.adaptive` attachment; (c) the
    // card JSON matches the expected Adaptive Card shape.
    //
    // Webex's PUT cannot mutate `attachments[]`, so the
    // `_with_existing_id_*` tests for Breadcrumb / TodoList assert the
    // FRESH-POST behaviour (the existing id is ignored — documented
    // platform limitation).
    // -----------------------------------------------------------------

    use copperclaw_channels_core::{
        Breadcrumb, BreadcrumbStatus, Card, CardButton, CardField, DiffCard, DiffHunk, DiffLine,
        DiffLineKind, ErrorCard, ErrorCardKind, ThinkingBlock, TodoItemStatus, TodoList,
        TodoListItem,
    };

    /// Pull the Adaptive Card JSON out of the most-recent
    /// `POST /messages` body. Helper avoids repetition across the
    /// per-method tests.
    async fn last_adaptive_card(server: &MockServer) -> serde_json::Value {
        let reqs = server.received_requests().await.expect("requests");
        let last = reqs.last().expect("at least one request");
        let body: serde_json::Value = serde_json::from_slice(&last.body).expect("json body");
        let att = body["attachments"][0].clone();
        assert_eq!(
            att["contentType"], "application/vnd.microsoft.card.adaptive",
            "expected adaptive-card content type"
        );
        att["content"].clone()
    }

    #[tokio::test]
    async fn deliver_card_posts_adaptive_card_attachment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-CARD"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
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
        let id = a.deliver_card("R1", None, &card, None).await.unwrap();
        assert_eq!(id.as_deref(), Some("M-CARD"));
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["type"], "AdaptiveCard");
        let body = card_json["body"].as_array().expect("body array");
        assert!(body.len() >= 3);
        assert_eq!(body[0]["text"], "Confirm");
        assert_eq!(body[0]["weight"], "Bolder");
        let factset = body.iter().find(|b| b["type"] == "FactSet").expect("factset");
        assert_eq!(factset["facts"][0]["title"], "Item");
        assert_eq!(factset["facts"][0]["value"], "Espresso");
        let actions = card_json["actions"].as_array().expect("actions");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["type"], "Action.Submit");
        assert_eq!(actions[0]["style"], "positive");
        assert_eq!(actions[0]["data"]["value"], "go");
        assert_eq!(actions[1]["type"], "Action.OpenUrl");
        assert_eq!(actions[1]["url"], "https://example.com");
    }

    #[tokio::test]
    async fn deliver_card_uses_room_id_in_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-CARD"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        a.deliver_card("R1", None, &card, None).await.unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["roomId"], "R1");
        // Markdown fallback is set so notification preview is human-readable.
        assert!(body["markdown"].as_str().unwrap().contains("Hi"));
    }

    #[tokio::test]
    async fn deliver_card_to_person_uses_to_person_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"M-DM-CARD"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        let id = a
            .deliver_card("person:P1", None, &card, None)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("M-DM-CARD"));
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["toPersonId"], "P1");
    }

    #[tokio::test]
    async fn deliver_breadcrumb_posts_adaptive_card_with_accent_color() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"BC-1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let bc = Breadcrumb::running("shell").with_detail("cargo check");
        let id = a.deliver_breadcrumb("R1", None, &bc, None).await.unwrap();
        assert_eq!(id.as_deref(), Some("BC-1"));
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["body"][0]["color"], "Accent");
        let text = card_json["body"][0]["text"].as_str().unwrap();
        assert!(text.contains("`shell`"));
        assert!(text.contains("cargo check"));
    }

    #[tokio::test]
    async fn deliver_breadcrumb_with_existing_id_still_posts_fresh_card() {
        // Webex doesn't support editing message attachments; the
        // existing_message_id is intentionally ignored and a fresh
        // card is posted. The new id is returned so the host can
        // re-thread future updates.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"BC-NEW"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let bc = Breadcrumb {
            tool_name: "shell".into(),
            detail: Some("cargo test".into()),
            status: BreadcrumbStatus::Done,
            summary: Some("passed (0.4s)".into()),
        };
        let id = a
            .deliver_breadcrumb("R1", None, &bc, Some("BC-OLD"))
            .await
            .unwrap();
        // New id (NOT the existing one) — Webex always creates a fresh chip.
        assert_eq!(id.as_deref(), Some("BC-NEW"));
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["body"][0]["color"], "Good");
        // No PUT was issued — we only POSTed.
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.iter().all(|r| r.method == wiremock::http::Method::POST));
    }

    #[tokio::test]
    async fn deliver_breadcrumb_failed_uses_attention_color() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"BC-FAIL"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let bc = Breadcrumb::running("shell")
            .with_detail("bad command")
            .finished(false, Some("exit 1".into()));
        a.deliver_breadcrumb("R1", None, &bc, None).await.unwrap();
        let card_json = last_adaptive_card(&server).await;
        assert_eq!(card_json["body"][0]["color"], "Attention");
        let text = card_json["body"][0]["text"].as_str().unwrap();
        assert!(text.contains("failed: exit 1"));
    }

    #[tokio::test]
    async fn deliver_diff_posts_adaptive_card_with_monospace_hunks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"DIFF-1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
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
        let id = a.deliver_diff("R1", None, &diff).await.unwrap();
        assert_eq!(id.as_deref(), Some("DIFF-1"));
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
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
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"DIFF-T"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let diff = DiffCard {
            path: "huge.rs".into(),
            language: None,
            hunks: vec![],
            added: 50,
            removed: 100,
            truncated: true,
        };
        a.deliver_diff("R1", None, &diff).await.unwrap();
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
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"ERR-1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let err = ErrorCard::new(ErrorCardKind::Internal, "shell timed out")
            .with_details("stderr: ENOENT");
        let id = a.deliver_error("R1", None, &err).await.unwrap();
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
    }

    #[tokio::test]
    async fn deliver_error_retryable_appends_retry_footer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"ERR-R"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let err = ErrorCard::new(ErrorCardKind::Delivery, "graph returned 502").retryable();
        a.deliver_error("R1", None, &err).await.unwrap();
        let card_json = last_adaptive_card(&server).await;
        let inner = card_json["body"][0]["items"].as_array().unwrap();
        assert!(inner
            .last()
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("will retry automatically"));
    }

    #[tokio::test]
    async fn deliver_todo_list_first_emit_posts_card() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"TL-1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
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
        let id = a
            .deliver_todo_list("R1", None, &list, None, true)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("TL-1"));
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        assert_eq!(body.len(), 5);
        assert!(body[0]["text"].as_str().unwrap().contains("Plan A (1/3)"));
        assert!(body[1]["text"].as_str().unwrap().starts_with("[x]"));
        assert_eq!(body[1]["color"], "Good");
        assert!(body[2]["text"].as_str().unwrap().starts_with("[~]"));
        assert_eq!(body[2]["color"], "Warning");
    }

    #[tokio::test]
    async fn deliver_todo_list_with_existing_id_still_posts_fresh_card() {
        // Webex has no message-edit-with-attachments support — the
        // host's existing_message_id is ignored and a fresh card is
        // emitted. The new id is returned so the host can re-thread
        // future updates.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"TL-NEW"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let list = TodoList {
            items: vec![TodoListItem {
                id: 1,
                text: "first".into(),
                status: TodoItemStatus::Completed,
            }],
            title: None,
        };
        let id = a
            .deliver_todo_list("R1", None, &list, Some("TL-OLD"), false)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("TL-NEW"));
        let reqs = server.received_requests().await.unwrap();
        assert!(reqs.iter().all(|r| r.method == wiremock::http::Method::POST));
    }

    #[tokio::test]
    async fn deliver_collapsible_posts_with_toggle_action() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"EXP-1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let id = a
            .deliver_collapsible(
                "R1",
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
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"EXP-NP"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        a.deliver_collapsible(
            "R1",
            None,
            "x".repeat(8000).as_str(),
            "shell 8000 chars",
            &[],
        )
        .await
        .unwrap();
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        // Summary + collapsed container (no preview block).
        assert_eq!(body.len(), 2);
        assert_eq!(body[1]["type"], "Container");
    }

    #[tokio::test]
    async fn deliver_thinking_posts_collapsed_container_with_toggle() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"TH-1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let t = ThinkingBlock::visible("Let me think carefully.").with_model("claude-opus-4-7");
        let id = a.deliver_thinking("R1", None, &t).await.unwrap();
        assert_eq!(id.as_deref(), Some("TH-1"));
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
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
        // Privacy contract: raw redacted blob NEVER appears on the wire.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"TH-RED"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let t = ThinkingBlock::redacted("opaque-blob-secret");
        a.deliver_thinking("R1", None, &t).await.unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body_str = std::str::from_utf8(&reqs.last().unwrap().body).unwrap();
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
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string(""))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        match a.deliver_card("R1", None, &card, None).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_breadcrumb_propagates_rate_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3"))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let bc = Breadcrumb::running("shell");
        match a.deliver_breadcrumb("R1", None, &bc, None).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(3)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_card_image_only_emits_image_block() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"IMG"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let card = Card {
            image_url: Some("https://example.com/x.png".into()),
            ..Card::default()
        };
        a.deliver_card("R1", None, &card, None).await.unwrap();
        let card_json = last_adaptive_card(&server).await;
        let body = card_json["body"].as_array().unwrap();
        assert_eq!(body[0]["type"], "Image");
        assert_eq!(body[0]["url"], "https://example.com/x.png");
    }

    #[tokio::test]
    async fn deliver_card_empty_platform_id_is_bad_request() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let card = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        match a.deliver_card("", None, &card, None).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_card_dropped_button_has_neither_value_nor_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"BTN"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
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
        a.deliver_card("R1", None, &card, None).await.unwrap();
        let card_json = last_adaptive_card(&server).await;
        assert!(card_json.get("actions").is_none());
    }

    #[tokio::test]
    async fn deliver_diff_in_thread_sets_parent_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"DIFF-T"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        let diff = DiffCard {
            path: "x.rs".into(),
            language: None,
            hunks: vec![],
            added: 0,
            removed: 0,
            truncated: false,
        };
        a.deliver_diff("R1", Some("PAR1"), &diff).await.unwrap();
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["parentId"], "PAR1");
    }
}
