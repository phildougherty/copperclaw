//! Slack [`ChannelAdapter`] implementation.

use crate::api::{CompleteUploadEntry, SlackApi};
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, OutboundFile, OutboundMessage};
use serde_json::Value;
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Slack channel adapter. See module-level docs.
pub struct SlackAdapter {
    channel_type: ChannelType,
    api: SlackApi,
    server_handle: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for SlackAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl SlackAdapter {
    /// Construct with an already-built API client. Used by the factory and
    /// by tests that drive the adapter directly.
    #[must_use]
    pub fn new(channel_type: ChannelType, api: SlackApi) -> Self {
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
            .expect("slack adapter server handle mutex poisoned");
        *guard = Some(handle);
    }

    /// Abort the background events server (if any). Idempotent.
    pub fn shutdown_server(&self) {
        let mut guard = self
            .server_handle
            .lock()
            .expect("slack adapter server handle mutex poisoned");
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &SlackApi {
        &self.api
    }

    /// Upload any attachments and return the Slack file ids. Used by
    /// `deliver` for outbound messages that carry files.
    async fn upload_files(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        files: &[OutboundFile],
    ) -> Result<(), AdapterError> {
        if files.is_empty() {
            return Ok(());
        }
        let mut entries: Vec<CompleteUploadEntry> = Vec::with_capacity(files.len());
        for file in files {
            let url = self
                .api
                .files_get_upload_url_external(&file.filename, file.data.len())
                .await?;
            self.api
                .files_upload_to_url(&url.upload_url, file.data.clone())
                .await?;
            entries.push(CompleteUploadEntry {
                id: url.file_id,
                title: Some(file.filename.clone()),
            });
        }
        self.api
            .files_complete_upload_external(&entries, Some(channel), thread_ts)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ChannelAdapter for SlackAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        true
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let Some(thread) = thread_id else {
            // Slack only exposes a typing status for assistants threads.
            return Ok(());
        };
        // Best effort — `assistant.threads.setStatus` returns an error
        // outside an Assistants context. Swallow `not_in_channel`-style
        // bad-request responses so typing remains a soft no-op.
        match self
            .api
            .set_assistant_status(platform_id, thread, "is typing...")
            .await
        {
            Ok(()) | Err(AdapterError::BadRequest(_)) => Ok(()),
            Err(other) => Err(other),
        }
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let blocks = message.content.get("blocks").cloned();
        let ephemeral_to = message
            .content
            .get("ephemeral_to")
            .and_then(Value::as_str);

        let ts = if let Some(user) = ephemeral_to {
            self.api
                .post_ephemeral(platform_id, user, thread_id, &text)
                .await?
                .ts
        } else {
            self.api
                .post_message(platform_id, thread_id, &text, blocks.as_ref())
                .await?
                .ts
        };

        if !message.files.is_empty() {
            self.upload_files(platform_id, thread_id, &message.files)
                .await?;
        }

        Ok(ts)
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Slack DMs are addressed by user id directly via `chat.postMessage`,
        // so we don't need a pre-opened conversations.open handle. Returning
        // `None` matches the channel-trait default for adapters that don't
        // pre-resolve DMs.
        Ok(None)
    }

    /// Slack-specific plain-text fallback used by the delivery loop when
    /// `chat.postMessage` rejected the original payload with a block-kit
    /// validation error. Drops the `blocks` field — Slack then renders just
    /// the `text` fallback string — and prepends `"[reduced formatting] "`
    /// so the recipient knows the layout was simplified. Returns `None`
    /// when there are no `blocks` to strip (nothing to fall back to).
    fn plain_text_fallback(&self, msg: &OutboundMessage) -> Option<OutboundMessage> {
        let obj = msg.content.as_object()?;
        if !obj.contains_key("blocks") {
            return None;
        }
        let text = obj
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut new_obj = obj.clone();
        new_obj.remove("blocks");
        new_obj.insert(
            "text".to_owned(),
            Value::String(format!("[reduced formatting] {text}")),
        );
        Some(OutboundMessage {
            kind: msg.kind,
            content: Value::Object(new_obj),
            files: msg.files.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::MessageKind;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> SlackAdapter {
        SlackAdapter::new(
            ChannelType::new("slack"),
            SlackApi::new(server.uri(), "xoxb-test"),
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
        assert_eq!(adapter.channel_type().as_str(), "slack");
        assert!(adapter.supports_threads());
    }

    #[tokio::test]
    async fn deliver_post_message_roundtrip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(header("authorization", "Bearer xoxb-test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "100.000"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("C1", None, &text("hello"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("100.000"));
    }

    #[tokio::test]
    async fn deliver_uses_thread_ts_when_provided() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "200.001"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let id = adapter
            .deliver("C1", Some("199.000"), &text("threaded"))
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("200.001"));
    }

    #[tokio::test]
    async fn deliver_uses_ephemeral_when_field_present() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postEphemeral"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "message_ts": "300.0"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "secret", "ephemeral_to": "U1"}),
            files: vec![],
        };
        let id = adapter.deliver("C1", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("300.0"));
    }

    #[tokio::test]
    async fn deliver_surfaces_invalid_auth_as_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rate_limited_via_http_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "5"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(5)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rate_limited_via_ok_false_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "ratelimited"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Rate { .. }) => {}
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_other_slack_error_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "channel_not_found"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::BadRequest(s)) => assert_eq!(s, "channel_not_found"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_uploads_files_after_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "1.0"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.getUploadURLExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "upload_url": format!("{}/upload-target", server.uri()),
                "file_id": "F100"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/upload-target"))
            .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.completeUploadExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "see file"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: b"hello".to_vec(),
            }],
        };
        let ts = adapter.deliver("C1", None, &msg).await.unwrap();
        assert_eq!(ts.as_deref(), Some("1.0"));
    }

    #[tokio::test]
    async fn set_typing_no_thread_is_noop_ok() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        adapter.set_typing("C1", None).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_swallows_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.threads.setStatus"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "not_in_channel"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        adapter.set_typing("C1", Some("99.0")).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_propagates_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.threads.setStatus"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.set_typing("C1", Some("99.0")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_typing_propagates_rate_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/assistant.threads.setStatus"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.set_typing("C1", Some("99.0")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(1)),
            other => panic!("expected Rate, got {other:?}"),
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
        adapter.subscribe("C1", None).await.unwrap();
        adapter.subscribe("C1", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn api_accessor_returns_inner() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        assert!(format!("{:?}", adapter.api()).contains("xoxb-test"));
    }

    #[tokio::test]
    async fn server_handle_shutdown_is_idempotent() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        // Set a handle (use a never-resolving future so abort actually matters).
        let task = tokio::spawn(async move {
            std::future::pending::<()>().await;
        });
        adapter.set_server_handle(task);
        adapter.shutdown_server();
        adapter.shutdown_server();
    }

    #[tokio::test]
    async fn chat_update_roundtrip_via_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.update"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "5.0"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let resp = adapter
            .api()
            .chat_update("C1", "1.0", "edited")
            .await
            .unwrap();
        assert_eq!(resp.ts.as_deref(), Some("5.0"));
    }

    #[tokio::test]
    async fn reactions_add_via_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/reactions.add"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        adapter
            .api()
            .reactions_add("C1", "1.0", "thumbsup")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn auth_test_via_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": true, "user_id": "UBOT123"})),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let r = adapter.api().auth_test().await.unwrap();
        assert_eq!(r.user_id, "UBOT123");
    }

    #[tokio::test]
    async fn deliver_transport_error_on_non_200_non_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        match adapter.deliver("C1", None, &text("x")).await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("503")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_to_url_failure_surfaces_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "ts": "1.0"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/files.getUploadURLExternal"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "upload_url": format!("{}/bad-upload", server.uri()),
                "file_id": "F1"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/bad-upload"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: "x.txt".into(),
                data: vec![1, 2],
            }],
        };
        match adapter.deliver("C1", None, &msg).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plain_text_fallback_strips_blocks_for_slack() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({
                "text": "hello",
                "blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "*hi*"}}]
            }),
            files: vec![],
        };
        let fallback = adapter
            .plain_text_fallback(&msg)
            .expect("slack fallback");
        // blocks must be removed entirely.
        assert!(fallback.content.get("blocks").is_none());
        // text is preserved and prepended with the reduced-formatting marker.
        assert_eq!(
            fallback.content["text"].as_str().unwrap(),
            "[reduced formatting] hello"
        );
        assert_eq!(fallback.kind, MessageKind::Chat);
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server);
        let s = format!("{adapter:?}");
        assert!(s.contains("SlackAdapter"));
        assert!(s.contains("slack"));
    }
}
