//! Resend [`ChannelAdapter`] implementation.
//!
//! Send-only adapter. `subscribe`, `set_typing`, and `open_dm` use the
//! trait defaults; `deliver` translates an [`OutboundMessage`] into a
//! single `POST /emails` call.

use crate::api::{Attachment, Header, ResendApi, SendEmailRequest};
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter};
use ironclaw_types::{ChannelType, MessageKind, OutboundFile, OutboundMessage};
use serde_json::Value;

const MAX_ATTACHMENT_NAME_LEN: usize = 255;

/// Resend channel adapter. See module-level docs.
#[derive(Debug)]
pub struct ResendAdapter {
    channel_type: ChannelType,
    api: ResendApi,
    from: String,
    default_subject: String,
}

impl ResendAdapter {
    /// Construct with an already-built API client. Used by the factory and
    /// by tests that drive the adapter directly.
    #[must_use]
    pub fn new(
        channel_type: ChannelType,
        api: ResendApi,
        from: impl Into<String>,
        default_subject: impl Into<String>,
    ) -> Self {
        Self {
            channel_type,
            api,
            from: from.into(),
            default_subject: default_subject.into(),
        }
    }

    /// Borrow the underlying API client (mostly useful for tests).
    #[must_use]
    pub fn api(&self) -> &ResendApi {
        &self.api
    }

    /// Borrow the configured `from` address.
    #[must_use]
    pub fn from_address(&self) -> &str {
        &self.from
    }

    /// Borrow the configured default subject.
    #[must_use]
    pub fn default_subject(&self) -> &str {
        &self.default_subject
    }

    fn build_request(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<SendEmailRequest, AdapterError> {
        let to = parse_recipients(platform_id)?;
        let (subject, text, html) = parse_content(&message.content, &self.default_subject)?;
        let attachments = build_attachments(&message.files)?;
        let headers = thread_id
            .map(|tid| {
                vec![
                    Header {
                        name: "In-Reply-To".into(),
                        value: tid.to_owned(),
                    },
                    Header {
                        name: "References".into(),
                        value: tid.to_owned(),
                    },
                ]
            })
            .unwrap_or_default();
        Ok(SendEmailRequest {
            from: self.from.clone(),
            to,
            subject,
            text,
            html,
            attachments,
            headers,
        })
    }
}

fn parse_recipients(platform_id: &str) -> Result<Vec<String>, AdapterError> {
    let trimmed = platform_id.trim();
    if trimmed.is_empty() {
        return Err(AdapterError::BadRequest(
            "resend platform_id (recipient list) is empty".into(),
        ));
    }
    let mut out: Vec<String> = Vec::new();
    for piece in trimmed.split(',') {
        let addr = piece.trim();
        if addr.is_empty() {
            return Err(AdapterError::BadRequest(
                "resend platform_id contains an empty recipient".into(),
            ));
        }
        out.push(addr.to_owned());
    }
    Ok(out)
}

fn parse_content(
    content: &Value,
    default_subject: &str,
) -> Result<(String, Option<String>, Option<String>), AdapterError> {
    let obj = content.as_object().ok_or_else(|| {
        AdapterError::BadRequest("resend message content must be a JSON object".into())
    })?;
    let subject = match obj.get("subject") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => default_subject.to_owned(),
        Some(_) => {
            return Err(AdapterError::BadRequest(
                "resend `subject` must be a string".into(),
            ));
        }
    };
    let text = match obj.get("text") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(AdapterError::BadRequest(
                "resend `text` must be a string".into(),
            ));
        }
    };
    let html = match obj.get("html") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(AdapterError::BadRequest(
                "resend `html` must be a string".into(),
            ));
        }
    };
    if text.is_none() && html.is_none() {
        return Err(AdapterError::BadRequest(
            "resend message must carry `text` or `html`".into(),
        ));
    }
    Ok((subject, text, html))
}

fn build_attachments(files: &[OutboundFile]) -> Result<Vec<Attachment>, AdapterError> {
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        validate_attachment_name(&f.filename)?;
        out.push(Attachment::from_bytes(f.filename.clone(), &f.data));
    }
    Ok(out)
}

/// Reject attachment names that contain path separators, `..`, leading
/// dots, NUL, or are oversized.
fn validate_attachment_name(name: &str) -> Result<(), AdapterError> {
    if name.is_empty() {
        return Err(AdapterError::BadRequest(
            "attachment filename is empty".into(),
        ));
    }
    if name.len() > MAX_ATTACHMENT_NAME_LEN {
        return Err(AdapterError::BadRequest(
            "attachment filename exceeds 255 bytes".into(),
        ));
    }
    if name.starts_with('.') {
        return Err(AdapterError::BadRequest(
            "attachment filename starts with '.'".into(),
        ));
    }
    if name.contains("..") {
        return Err(AdapterError::BadRequest(
            "attachment filename contains '..'".into(),
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(AdapterError::BadRequest(
            "attachment filename has path separator or NUL".into(),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(AdapterError::BadRequest(
            "attachment filename has control character".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl ChannelAdapter for ResendAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        // Resend has no native thread concept; we surface a header pair
        // when `thread_id` is provided but do not treat it as first-class.
        false
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        if matches!(message.kind, MessageKind::System) {
            if let Some(action) = message.content.get("action").and_then(Value::as_str) {
                return Err(AdapterError::Unsupported(format!(
                    "resend does not support system action `{action}`"
                )));
            }
        }
        let req = self.build_request(platform_id, thread_id, message)?;
        tracing::debug!(
            recipients = req.to.len(),
            has_text = req.text.is_some(),
            has_html = req.html.is_some(),
            attachments = req.attachments.len(),
            "resend deliver"
        );
        let resp = self.api.send_email(&req).await?;
        Ok(Some(resp.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::OutboundFile;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn adapter_for(server: &MockServer) -> ResendAdapter {
        ResendAdapter::new(
            ChannelType::new(crate::CHANNEL_TYPE_STR),
            ResendApi::new(server.uri(), "re_test"),
            "agent@example.test",
            "(no subject)",
        )
    }

    fn text_msg(t: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": t}),
            files: vec![],
        }
    }

    async fn install_send_ok(server: &MockServer, id: &str) {
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": id.to_owned()})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn channel_type_returns_resend() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        assert_eq!(a.channel_type().as_str(), "resend");
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn deliver_text_returns_resend_id() {
        let server = MockServer::start().await;
        install_send_ok(&server, "msg_xyz").await;
        let a = adapter_for(&server);
        let id = a.deliver("alice@e.test", None, &text_msg("hi")).await.unwrap();
        assert_eq!(id.as_deref(), Some("msg_xyz"));
    }

    #[tokio::test]
    async fn deliver_html_variant() {
        let server = MockServer::start().await;
        install_send_ok(&server, "id-h").await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"html": "<p>hi</p>"}),
            files: vec![],
        };
        let id = a.deliver("alice@e.test", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("id-h"));
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["html"], "<p>hi</p>");
        assert!(body.get("text").is_none());
    }

    #[tokio::test]
    async fn deliver_with_explicit_subject() {
        let server = MockServer::start().await;
        install_send_ok(&server, "id-s").await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"subject": "Custom", "text": "body"}),
            files: vec![],
        };
        a.deliver("alice@e.test", None, &msg).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["subject"], "Custom");
        assert_eq!(body["text"], "body");
    }

    #[tokio::test]
    async fn deliver_uses_default_subject_when_omitted() {
        let server = MockServer::start().await;
        install_send_ok(&server, "id-d").await;
        let a = adapter_for(&server);
        a.deliver("alice@e.test", None, &text_msg("body")).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["subject"], "(no subject)");
    }

    #[tokio::test]
    async fn deliver_multi_recipient_platform_id_splits_on_comma() {
        let server = MockServer::start().await;
        install_send_ok(&server, "id-m").await;
        let a = adapter_for(&server);
        a.deliver("x@e.test,  y@e.test , z@e.test", None, &text_msg("b"))
            .await
            .unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["to"][0], "x@e.test");
        assert_eq!(body["to"][1], "y@e.test");
        assert_eq!(body["to"][2], "z@e.test");
    }

    #[tokio::test]
    async fn deliver_rejects_empty_platform_id() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        match a.deliver("", None, &text_msg("hi")).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_whitespace_only_platform_id() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        match a.deliver("   ", None, &text_msg("hi")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_trailing_comma_recipient() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        match a.deliver("a@e.test,", None, &text_msg("hi")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_double_comma_recipient() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        match a.deliver("a@e.test,,b@e.test", None, &text_msg("hi")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_non_object_content() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!("just a string"),
            files: vec![],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("object")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_missing_body() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"subject": "only"}),
            files: vec![],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("text") || m.contains("html")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_non_string_subject() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"subject": 7, "text": "b"}),
            files: vec![],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("subject")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_non_string_text() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": 7}),
            files: vec![],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("text")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_non_string_html() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"html": [1,2,3]}),
            files: vec![],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("html")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_null_subject_falls_back_to_default() {
        let server = MockServer::start().await;
        install_send_ok(&server, "ok").await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"subject": null, "text": "b"}),
            files: vec![],
        };
        a.deliver("a@e.test", None, &msg).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["subject"], "(no subject)");
    }

    #[tokio::test]
    async fn deliver_with_thread_id_sets_reply_headers() {
        let server = MockServer::start().await;
        install_send_ok(&server, "ok").await;
        let a = adapter_for(&server);
        a.deliver("a@e.test", Some("<thread@x>"), &text_msg("hi"))
            .await
            .unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        let headers = body["headers"].as_array().unwrap();
        let names: Vec<&str> = headers
            .iter()
            .map(|h| h["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"In-Reply-To"));
        assert!(names.contains(&"References"));
        for h in headers {
            assert_eq!(h["value"], "<thread@x>");
        }
    }

    #[tokio::test]
    async fn deliver_without_thread_id_omits_headers() {
        let server = MockServer::start().await;
        install_send_ok(&server, "ok").await;
        let a = adapter_for(&server);
        a.deliver("a@e.test", None, &text_msg("hi")).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert!(body.get("headers").is_none());
    }

    #[tokio::test]
    async fn deliver_with_attachments_encodes_them() {
        let server = MockServer::start().await;
        install_send_ok(&server, "ok").await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "see file"}),
            files: vec![OutboundFile {
                filename: "doc.txt".into(),
                data: b"hello".to_vec(),
            }],
        };
        a.deliver("a@e.test", None, &msg).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["attachments"][0]["filename"], "doc.txt");
        assert_eq!(body["attachments"][0]["content"], "aGVsbG8=");
    }

    #[tokio::test]
    async fn deliver_rejects_attachment_with_path_separator() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: "a/b.txt".into(),
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("path separator")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_attachment_with_backslash() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: "a\\b.txt".into(),
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_attachment_with_double_dot() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: "x..txt".into(),
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("..")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_attachment_with_leading_dot() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: ".secret".into(),
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains('.')),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_empty_attachment_filename() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: String::new(),
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("empty")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_oversize_attachment_filename() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let name: String = "a".repeat(MAX_ATTACHMENT_NAME_LEN + 1);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: name,
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("255")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_attachment_with_nul() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: "a\0b".into(),
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_rejects_attachment_with_control_char() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: "a\tb".into(),
                data: vec![],
            }],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("control")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_max_length_attachment_name_is_ok() {
        let server = MockServer::start().await;
        install_send_ok(&server, "ok").await;
        let a = adapter_for(&server);
        let name: String = "a".repeat(MAX_ATTACHMENT_NAME_LEN);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "x"}),
            files: vec![OutboundFile {
                filename: name,
                data: b"data".to_vec(),
            }],
        };
        a.deliver("a@e.test", None, &msg).await.unwrap();
    }

    #[tokio::test]
    async fn deliver_system_edit_action_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "edit", "target_seq": 1, "text": "x"}),
            files: vec![],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("edit")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_reaction_action_is_unsupported() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"action": "reaction", "target_seq": 1, "emoji": "thumbsup"}),
            files: vec![],
        };
        match a.deliver("a@e.test", None, &msg).await {
            Err(AdapterError::Unsupported(m)) => assert!(m.contains("reaction")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_without_action_falls_through_to_normal_send() {
        let server = MockServer::start().await;
        install_send_ok(&server, "sys").await;
        let a = adapter_for(&server);
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({"text": "system body"}),
            files: vec![],
        };
        // No `action` key — we don't reject; treat as a normal send.
        let id = a.deliver("a@e.test", None, &msg).await.unwrap();
        assert_eq!(id.as_deref(), Some("sys"));
    }

    #[tokio::test]
    async fn deliver_propagates_auth_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(401).set_body_string("no"))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("a@e.test", None, &text_msg("x")).await {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_rate_error_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("a@e.test", None, &text_msg("x")).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(7)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_bad_request_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(422))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("a@e.test", None, &text_msg("x")).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_propagates_transport_from_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let a = adapter_for(&server);
        match a.deliver("a@e.test", None, &text_msg("x")).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_typing_is_default_noop() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        a.set_typing("a@e.test", None).await.unwrap();
        a.set_typing("a@e.test", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_is_default_noop() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        a.subscribe("a@e.test", None).await.unwrap();
        a.subscribe("a@e.test", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn open_dm_returns_none_default() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        assert!(a.open_dm("u").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accessors_return_configured_values() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        assert_eq!(a.from_address(), "agent@example.test");
        assert_eq!(a.default_subject(), "(no subject)");
        // api() borrows back the underlying client; debug should show key.
        assert!(format!("{:?}", a.api()).contains("re_test"));
    }

    #[tokio::test]
    async fn debug_format_includes_channel_type() {
        let server = MockServer::start().await;
        let a = adapter_for(&server);
        let s = format!("{a:?}");
        assert!(s.contains("ResendAdapter"));
        assert!(s.contains("resend"));
    }

    #[test]
    fn parse_recipients_single() {
        let r = parse_recipients("a@b.test").unwrap();
        assert_eq!(r, vec!["a@b.test".to_owned()]);
    }

    #[test]
    fn parse_recipients_trims() {
        let r = parse_recipients("  a@b.test  ").unwrap();
        assert_eq!(r, vec!["a@b.test".to_owned()]);
    }

    #[test]
    fn parse_recipients_multi() {
        let r = parse_recipients("a@b.test, c@d.test").unwrap();
        assert_eq!(r, vec!["a@b.test".to_owned(), "c@d.test".to_owned()]);
    }

    #[test]
    fn validate_attachment_name_accepts_simple() {
        validate_attachment_name("file.txt").unwrap();
        validate_attachment_name("a-b_c.tar.gz").unwrap();
    }

    #[test]
    fn parse_content_uses_default_subject_when_absent() {
        let v = json!({"text": "hi"});
        let (subj, text, html) = parse_content(&v, "fallback").unwrap();
        assert_eq!(subj, "fallback");
        assert_eq!(text.as_deref(), Some("hi"));
        assert!(html.is_none());
    }

    #[test]
    fn parse_content_prefers_explicit_subject() {
        let v = json!({"subject": "S", "html": "<p/>"});
        let (subj, text, html) = parse_content(&v, "fallback").unwrap();
        assert_eq!(subj, "S");
        assert!(text.is_none());
        assert_eq!(html.as_deref(), Some("<p/>"));
    }

    #[test]
    fn parse_content_accepts_both_text_and_html() {
        let v = json!({"text": "t", "html": "<h/>"});
        let (_, text, html) = parse_content(&v, "x").unwrap();
        assert_eq!(text.as_deref(), Some("t"));
        assert_eq!(html.as_deref(), Some("<h/>"));
    }

    #[test]
    fn build_attachments_empty_returns_empty() {
        let r = build_attachments(&[]).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn build_attachments_propagates_validation() {
        let res = build_attachments(&[OutboundFile {
            filename: "../etc/passwd".into(),
            data: vec![],
        }]);
        assert!(matches!(res, Err(AdapterError::BadRequest(_))));
    }

}
