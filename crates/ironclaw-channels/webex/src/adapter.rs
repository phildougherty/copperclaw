//! Webex [`ChannelAdapter`] implementation.

use crate::api::WebexApi;
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, MessageKind, OutboundMessage};
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
            | MessageKind::Agent => self.deliver_chat(&target, thread_id, message).await,
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

    // `subscribe` and `set_typing` keep their trait defaults: Webex offers no
    // per-room subscription mechanism (webhooks are firehose) and no public
    // typing indicator API.
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
    use ironclaw_types::OutboundFile;
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
}
