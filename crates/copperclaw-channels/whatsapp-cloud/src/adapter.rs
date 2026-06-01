//! [`ChannelAdapter`] implementation for the `WhatsApp` Cloud channel.
//!
//! See the crate-level docs for the platform-id format
//! (`<phone_number_id>:<wa_id>`) and the system-action semantics
//! (`edit` → `Unsupported`, `reaction` → working, `set_typing` →
//! `mark_read` of the last known message id).

use crate::api::WhatsappCloudApi;
use async_trait::async_trait;
use copperclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use copperclaw_types::{ChannelType, OutboundFile, OutboundMessage};
use serde_json::Value;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// The `WhatsApp` Cloud channel adapter.
pub struct WhatsappCloudAdapter {
    channel_type: ChannelType,
    api: WhatsappCloudApi,
    default_phone_number_id: Option<String>,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for WhatsappCloudAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhatsappCloudAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .field("default_phone_number_id", &self.default_phone_number_id)
            .finish_non_exhaustive()
    }
}

impl WhatsappCloudAdapter {
    /// Construct with an already-built API client and the (optional)
    /// `default_phone_number_id` used when an outbound `platform_id` does
    /// not contain a `<pnid>:` prefix.
    #[must_use]
    pub fn new(
        channel_type: ChannelType,
        api: WhatsappCloudApi,
        default_phone_number_id: Option<String>,
    ) -> Self {
        Self {
            channel_type,
            api,
            default_phone_number_id,
            server_handle: Mutex::new(None),
        }
    }

    /// Borrow the underlying API client (test-only convenience).
    #[must_use]
    pub fn api(&self) -> &WhatsappCloudApi {
        &self.api
    }

    /// Attach the join handle of the spawned axum server so the adapter
    /// can abort it on shutdown.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("whatsapp-cloud adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background webhook server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("whatsapp-cloud adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    fn split_platform_id<'a>(
        &'a self,
        platform_id: &'a str,
    ) -> Result<(String, &'a str), AdapterError> {
        if let Some((pnid, recipient)) = platform_id.split_once(':') {
            if pnid.is_empty() || recipient.is_empty() {
                return Err(AdapterError::BadRequest(format!(
                    "whatsapp-cloud platform_id `{platform_id}` is malformed (expected `<pnid>:<recipient>`)"
                )));
            }
            return Ok((pnid.to_owned(), recipient));
        }
        // No prefix — fall back to the configured default.
        let pnid = self.default_phone_number_id.clone().ok_or_else(|| {
            AdapterError::BadRequest(format!(
                "whatsapp-cloud platform_id `{platform_id}` has no `<pnid>:` prefix and no default_phone_number_id is configured"
            ))
        })?;
        Ok((pnid, platform_id))
    }

    async fn deliver_files(
        &self,
        phone_number_id: &str,
        recipient: &str,
        files: &[OutboundFile],
        in_reply_to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let mut last_id: Option<String> = None;
        for file in files {
            let mime = infer_mime(&file.filename);
            let media_id = self
                .api
                .upload_media(phone_number_id, &file.filename, &mime, file.data.clone())
                .await?;
            let id = self
                .api
                .send_document_by_id(
                    phone_number_id,
                    recipient,
                    &media_id,
                    Some(&file.filename),
                    in_reply_to,
                )
                .await?;
            last_id = Some(id);
        }
        Ok(last_id)
    }
}

fn infer_mime(filename: &str) -> String {
    let lower = filename.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "mp4" => "video/mp4",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_owned()
}

#[async_trait]
impl ChannelAdapter for WhatsappCloudAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        // WhatsApp threads are flat replies addressed by message id, not
        // a separate thread object; surface that as "no thread support" so
        // the host doesn't try to mount per-thread state for us.
        false
    }

    /// Cloud API text messages cap at 4096 chars per send.
    fn max_message_chars(&self) -> Option<usize> {
        Some(4096)
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // WhatsApp Cloud has no real typing indicator. The closest
        // reasonable approximation is marking the user's last message as
        // read. We surface that here only when the caller passes the
        // last-message id in `thread_id` — otherwise this is a no-op so
        // we don't issue stray reads on every typing tick.
        let Some(message_id) = thread_id else {
            return Ok(());
        };
        let (pnid, _recipient) = self.split_platform_id(platform_id)?;
        self.api.mark_read(&pnid, message_id).await
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let (pnid, recipient) = self.split_platform_id(platform_id)?;

        // System action handling (edit / reaction) — recognised regardless
        // of MessageKind because the content carries the action discriminator.
        if let Some(action) = message.content.get("action").and_then(Value::as_str) {
            return self
                .handle_system_action(&pnid, recipient, action, &message.content)
                .await;
        }

        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");

        // Files-first: upload + send by media id, threading via `in_reply_to`.
        if !message.files.is_empty() {
            // If there's also a text body, send it first so the order makes
            // sense in the chat.
            let mut first_id: Option<String> = None;
            if !text.is_empty() {
                let id = if let Some(reply) = thread_id {
                    self.api
                        .send_text_reply(&pnid, recipient, text, reply)
                        .await?
                } else {
                    self.api.send_text(&pnid, recipient, text).await?
                };
                first_id = Some(id);
            }
            let last = self
                .deliver_files(&pnid, recipient, &message.files, thread_id)
                .await?;
            return Ok(last.or(first_id));
        }

        // Pure text.
        let id = if let Some(reply) = thread_id {
            self.api
                .send_text_reply(&pnid, recipient, text, reply)
                .await?
        } else {
            self.api.send_text(&pnid, recipient, text).await?
        };
        Ok(Some(id))
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // WhatsApp messages are addressed by phone number directly; there is
        // no separate "open DM" handshake. Return `None`.
        Ok(None)
    }
}

impl WhatsappCloudAdapter {
    async fn handle_system_action(
        &self,
        phone_number_id: &str,
        recipient: &str,
        action: &str,
        content: &Value,
    ) -> Result<Option<String>, AdapterError> {
        match action {
            "edit" => Err(AdapterError::Unsupported(
                "whatsapp-cloud does not support editing previously sent messages".into(),
            )),
            "reaction" => {
                let target = content
                    .get("target_message_id")
                    .or_else(|| content.get("message_id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::BadRequest(
                            "reaction action requires `target_message_id` or `message_id`".into(),
                        )
                    })?;
                let emoji = content.get("emoji").and_then(Value::as_str).unwrap_or("");
                let id = self
                    .api
                    .send_reaction(phone_number_id, recipient, target, emoji)
                    .await?;
                Ok(Some(id))
            }
            other => Err(AdapterError::Unsupported(format!(
                "whatsapp-cloud does not support system action `{other}`"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::MessageKind;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer, default_pnid: Option<&str>) -> WhatsappCloudAdapter {
        WhatsappCloudAdapter::new(
            ChannelType::new("whatsapp-cloud"),
            WhatsappCloudApi::new(server.uri(), "EAAG-test"),
            default_pnid.map(str::to_owned),
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
    async fn channel_type_is_whatsapp_cloud() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        assert_eq!(a.channel_type().as_str(), "whatsapp-cloud");
    }

    #[tokio::test]
    async fn supports_threads_is_false() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn subscribe_default_is_ok() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        a.subscribe("PNID:+1", None).await.unwrap();
    }

    #[tokio::test]
    async fn deliver_text_uses_parsed_phone_number_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(body_partial_json(json!({
                "to":"+15551234",
                "type":"text",
                "text":{"body":"hello"}
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"wamid.X"}]})),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        let id = a
            .deliver("PNID:+15551234", None, &text("hello"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("wamid.X"));
    }

    #[tokio::test]
    async fn deliver_text_reply_includes_context() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(body_partial_json(json!({
                "context":{"message_id":"wamid.PARENT"}
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"wamid.R"}]})),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        let id = a
            .deliver("PNID:+15551234", Some("wamid.PARENT"), &text("threaded"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("wamid.R"));
    }

    #[tokio::test]
    async fn deliver_with_default_pnid_when_no_prefix() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/DEFAULT/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"wamid.D"}]})),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server, Some("DEFAULT"));
        let id = a.deliver("+15551234", None, &text("hi")).await.unwrap();
        assert_eq!(id.as_deref(), Some("wamid.D"));
    }

    #[tokio::test]
    async fn deliver_without_prefix_or_default_errors() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        match a.deliver("+15551234", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("default_phone_number_id")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_malformed_prefix_errors() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        match a.deliver(":just-recipient", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("malformed")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
        match a.deliver("pnid:", None, &text("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("malformed")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_with_files_uploads_then_sends() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/media"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"MEDIA1"})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(body_partial_json(json!({
                "type":"document",
                "document":{"id":"MEDIA1","filename":"x.pdf"}
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"wamid.F"}]})),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": ""}),
            files: vec![OutboundFile {
                filename: "x.pdf".into(),
                data: b"PDF-data".to_vec(),
            }],
        };
        let id = a.deliver("PNID:+1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("wamid.F"));
    }

    #[tokio::test]
    async fn deliver_with_text_and_files_sends_both() {
        let server = MockServer::start().await;
        // Both text and document hit /PNID/messages — we register one mock
        // and assert at least one wamid is returned.
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"wamid.T"}]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/PNID/media"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"MEDIA1"})))
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":"caption"}),
            files: vec![OutboundFile {
                filename: "x.pdf".into(),
                data: b"PDF".to_vec(),
            }],
        };
        let id = a.deliver("PNID:+1", None, &msg).await.unwrap();
        assert!(id.is_some());
    }

    #[tokio::test]
    async fn deliver_edit_action_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"edit","target_seq":7,"text":"new"}),
            files: vec![],
        };
        match a.deliver("PNID:+1", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("edit")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_reaction_action_calls_send_reaction() {
        let server = MockServer::start().await;
        // Compose emoji at runtime — project rule: no inline emoji literals.
        let emoji_codepoint: u32 = 0x1F44D;
        let emoji = char::from_u32(emoji_codepoint).unwrap().to_string();
        let emoji_for_match = emoji.clone();
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(body_partial_json(json!({
                "type":"reaction",
                "reaction":{"message_id":"wamid.TGT","emoji": emoji_for_match}
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"wamid.RXN"}]})),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({
                "action":"reaction",
                "target_message_id":"wamid.TGT",
                "emoji": emoji
            }),
            files: vec![],
        };
        let id = a.deliver("PNID:+1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("wamid.RXN"));
    }

    #[tokio::test]
    async fn deliver_reaction_action_with_legacy_message_id_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"wamid.RXN2"}]})),
            )
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","message_id":"wamid.TGT","emoji":""}),
            files: vec![],
        };
        let id = a.deliver("PNID:+1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("wamid.RXN2"));
    }

    #[tokio::test]
    async fn deliver_reaction_action_without_target_errors() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"reaction","emoji":""}),
            files: vec![],
        };
        match a.deliver("PNID:+1", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => {
                assert!(m.contains("target_message_id") || m.contains("message_id"));
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action":"forward","target":"x"}),
            files: vec![],
        };
        match a.deliver("PNID:+1", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("forward")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_typing_without_thread_id_is_noop() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        a.set_typing("PNID:+1", None).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_with_thread_id_marks_read() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .and(body_partial_json(json!({
                "status":"read",
                "message_id":"wamid.LAST"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true})))
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        a.set_typing("PNID:+1", Some("wamid.LAST")).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_propagates_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        match a.set_typing("PNID:+1", Some("wamid.X")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_typing_with_malformed_platform_id_errors() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        match a.set_typing(":bad", Some("wamid.X")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_text_surfaces_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error":{"message":"bad token","code":190}
            })))
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        match a.deliver("PNID:+1", None, &text("hi")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_text_surfaces_rate_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/PNID/messages"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3"))
            .mount(&server)
            .await;
        let a = adapter_for(&server, None);
        match a.deliver("PNID:+1", None, &text("hi")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(3)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
        assert!(a.open_dm("anyone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn server_handle_shutdown_is_idempotent() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, None);
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
        let a = adapter_for(&server, None);
        assert!(format!("{:?}", a.api()).contains("EAAG-test"));
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let a = adapter_for(&server, Some("DEFAULT"));
        let s = format!("{a:?}");
        assert!(s.contains("WhatsappCloudAdapter"));
        assert!(s.contains("whatsapp-cloud"));
        assert!(s.contains("DEFAULT"));
    }

    #[test]
    fn infer_mime_recognises_common_extensions() {
        assert_eq!(infer_mime("a.PDF"), "application/pdf");
        assert_eq!(infer_mime("a.png"), "image/png");
        assert_eq!(infer_mime("a.jpg"), "image/jpeg");
        assert_eq!(infer_mime("a.JPEG"), "image/jpeg");
        assert_eq!(infer_mime("a.gif"), "image/gif");
        assert_eq!(infer_mime("a.mp3"), "audio/mpeg");
        assert_eq!(infer_mime("a.ogg"), "audio/ogg");
        assert_eq!(infer_mime("a.mp4"), "video/mp4");
        assert_eq!(infer_mime("a.txt"), "text/plain");
        assert_eq!(infer_mime("noext"), "application/octet-stream");
        assert_eq!(infer_mime("a.weird"), "application/octet-stream");
    }
}
