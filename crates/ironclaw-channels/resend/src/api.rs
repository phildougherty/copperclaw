//! Resend HTTP API client.
//!
//! Wraps the small slice of the Resend API the channel uses (just `POST
//! /emails`). Returns the message id Resend assigns. Maps HTTP status
//! codes to typed [`AdapterError`] variants per the contract in
//! `docs/adding-a-channel.md` § 5.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use ironclaw_channels_core::AdapterError;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

/// Body for `POST /emails`. Only the fields we use; serializes with `null`
/// fields stripped via `skip_serializing_if`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SendEmailRequest {
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
}

/// Resend attachment shape — `content` is base64.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Attachment {
    pub filename: String,
    pub content: String,
}

impl Attachment {
    /// Build an attachment by base64-encoding `data`.
    #[must_use]
    pub fn from_bytes(filename: impl Into<String>, data: &[u8]) -> Self {
        Self {
            filename: filename.into(),
            content: BASE64_STANDARD.encode(data),
        }
    }
}

/// Email header tuple Resend accepts via the optional `headers` field.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Response body from `POST /emails`.
#[derive(Debug, Clone, Deserialize)]
pub struct SendEmailResponse {
    pub id: String,
}

/// Minimal Resend HTTP client.
#[derive(Debug, Clone)]
pub struct ResendApi {
    client: Client,
    api_base: String,
    api_key: String,
}

impl ResendApi {
    /// Build a client using the configured key and base URL.
    #[must_use]
    pub fn new(api_base: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_client(Client::new(), api_base, api_key)
    }

    /// Construct with a caller-supplied `reqwest::Client`.
    #[must_use]
    pub fn with_client(
        client: Client,
        api_base: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_base: api_base.into(),
            api_key: api_key.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.api_base.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    /// `POST /emails` — send a single message. Returns the Resend message id.
    pub async fn send_email(
        &self,
        req: &SendEmailRequest,
    ) -> Result<SendEmailResponse, AdapterError> {
        let resp = self
            .client
            .post(self.url("emails"))
            .bearer_auth(&self.api_key)
            .json(req)
            .send()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        let status = resp.status();
        let retry_after = parse_retry_after(&resp);
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(AdapterError::Rate { retry_after });
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            let body = resp.text().await.unwrap_or_default();
            return Err(AdapterError::Auth(format!("{status}: {body}")));
        }
        if status == StatusCode::BAD_REQUEST
            || status == StatusCode::UNPROCESSABLE_ENTITY
            || status == StatusCode::NOT_FOUND
        {
            let body = resp.text().await.unwrap_or_default();
            return Err(AdapterError::BadRequest(format!("{status}: {body}")));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AdapterError::Transport(format!("{status}: {body}")));
        }
        let parsed: SendEmailResponse = resp.json().await.map_err(|e| {
            AdapterError::Transport(format!("resend response not JSON: {e}"))
        })?;
        Ok(parsed)
    }
}

fn parse_retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn req() -> SendEmailRequest {
        SendEmailRequest {
            from: "agent@example.test".into(),
            to: vec!["alice@example.test".into()],
            subject: "Hi".into(),
            text: Some("body".into()),
            html: None,
            attachments: vec![],
            headers: vec![],
        }
    }

    #[test]
    fn attachment_from_bytes_base64_encodes_correctly() {
        let a = Attachment::from_bytes("x.bin", b"hello");
        assert_eq!(a.filename, "x.bin");
        // base64("hello") = "aGVsbG8="
        assert_eq!(a.content, "aGVsbG8=");
    }

    #[test]
    fn attachment_from_bytes_handles_empty_data() {
        let a = Attachment::from_bytes("x.bin", b"");
        assert_eq!(a.content, "");
    }

    #[test]
    fn attachment_from_bytes_round_trips_binary() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let a = Attachment::from_bytes("bin", &bytes);
        let decoded = BASE64_STANDARD.decode(a.content.as_bytes()).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn send_email_request_skips_empty_optional_fields() {
        let r = SendEmailRequest {
            from: "a@b.test".into(),
            to: vec!["c@d.test".into()],
            subject: "S".into(),
            text: Some("t".into()),
            html: None,
            attachments: vec![],
            headers: vec![],
        };
        let j = serde_json::to_value(&r).unwrap();
        assert!(j.get("text").is_some());
        assert!(j.get("html").is_none());
        assert!(j.get("attachments").is_none());
        assert!(j.get("headers").is_none());
    }

    #[test]
    fn send_email_request_includes_html_when_set() {
        let r = SendEmailRequest {
            from: "a@b.test".into(),
            to: vec!["c@d.test".into()],
            subject: "S".into(),
            text: None,
            html: Some("<p>hi</p>".into()),
            attachments: vec![],
            headers: vec![],
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["html"], "<p>hi</p>");
        assert!(j.get("text").is_none());
    }

    #[test]
    fn url_joins_with_and_without_trailing_slash() {
        let a = ResendApi::new("https://x.test/api/", "k");
        assert_eq!(a.url("emails"), "https://x.test/api/emails");
        let b = ResendApi::new("https://x.test/api", "k");
        assert_eq!(b.url("emails"), "https://x.test/api/emails");
        let c = ResendApi::new("https://x.test/api", "k");
        assert_eq!(c.url("/emails"), "https://x.test/api/emails");
    }

    #[test]
    fn api_clone_and_debug() {
        let api = ResendApi::new("https://x.test", "re_secret");
        let _ = api.clone();
        let s = format!("{api:?}");
        assert!(s.contains("re_secret"));
    }

    #[tokio::test]
    async fn send_email_success_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .and(header("authorization", "Bearer re_test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "msg_123"})),
            )
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        let resp = api.send_email(&req()).await.unwrap();
        assert_eq!(resp.id, "msg_123");
    }

    #[tokio::test]
    async fn send_email_success_201_also_ok() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"id": "msg_201"})),
            )
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        let resp = api.send_email(&req()).await.unwrap();
        assert_eq!(resp.id, "msg_201");
    }

    #[tokio::test]
    async fn send_email_401_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Auth(m)) => assert!(m.contains("401")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_403_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Auth(m)) => assert!(m.contains("403")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_400_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(400).set_body_string("oops"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("400")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_422_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(422).set_body_string("validation"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("422")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_429_with_retry_after_is_rate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "12"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(12)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_429_without_retry_after_is_rate_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, None),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_429_with_invalid_retry_after_is_rate_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "later"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, None),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_500_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("500")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_502_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_503_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_malformed_json_response_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("not JSON")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_response_missing_id_is_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"foo": "bar"})))
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_connection_error_is_transport() {
        // Point at a port that should be closed.
        let api = ResendApi::new("http://127.0.0.1:1", "re_test");
        match api.send_email(&req()).await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_email_sends_attachments_base64() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "id-1"})),
            )
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        let r = SendEmailRequest {
            from: "a@b.test".into(),
            to: vec!["c@d.test".into()],
            subject: "S".into(),
            text: Some("body".into()),
            html: None,
            attachments: vec![Attachment::from_bytes("hi.txt", b"hello")],
            headers: vec![],
        };
        let resp = api.send_email(&r).await.unwrap();
        assert_eq!(resp.id, "id-1");

        // Inspect the actually-sent body via wiremock's request log.
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["attachments"][0]["filename"], "hi.txt");
        assert_eq!(body["attachments"][0]["content"], "aGVsbG8=");
    }

    #[tokio::test]
    async fn send_email_sends_headers_when_present() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "id-h"})),
            )
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        let r = SendEmailRequest {
            from: "a@b.test".into(),
            to: vec!["c@d.test".into()],
            subject: "S".into(),
            text: Some("b".into()),
            html: None,
            attachments: vec![],
            headers: vec![Header {
                name: "In-Reply-To".into(),
                value: "<tid@example>".into(),
            }],
        };
        api.send_email(&r).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["headers"][0]["name"], "In-Reply-To");
        assert_eq!(body["headers"][0]["value"], "<tid@example>");
    }

    #[tokio::test]
    async fn send_email_sends_multi_recipient_to() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "id-m"})),
            )
            .mount(&server)
            .await;
        let api = ResendApi::new(server.uri(), "re_test");
        let r = SendEmailRequest {
            from: "a@b.test".into(),
            to: vec!["x@e.test".into(), "y@e.test".into()],
            subject: "S".into(),
            text: Some("b".into()),
            html: None,
            attachments: vec![],
            headers: vec![],
        };
        api.send_email(&r).await.unwrap();
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["to"][0], "x@e.test");
        assert_eq!(body["to"][1], "y@e.test");
    }

    #[tokio::test]
    async fn send_email_uses_custom_api_base() {
        let server = MockServer::start().await;
        // Mount under a subpath to confirm the configured base is honored.
        let custom_base = format!("{}/api/v2", server.uri());
        Mock::given(method("POST"))
            .and(path("/api/v2/emails"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "custom"})),
            )
            .mount(&server)
            .await;
        let api = ResendApi::new(custom_base, "re_test");
        let resp = api.send_email(&req()).await.unwrap();
        assert_eq!(resp.id, "custom");
    }

    #[test]
    fn header_eq_clone_debug() {
        let h = Header {
            name: "X-A".into(),
            value: "1".into(),
        };
        let h2 = h.clone();
        assert_eq!(h, h2);
        assert!(format!("{h:?}").contains("X-A"));
    }

    #[test]
    fn attachment_eq_clone_debug() {
        let a = Attachment {
            filename: "f".into(),
            content: "c".into(),
        };
        let a2 = a.clone();
        assert_eq!(a, a2);
        assert!(format!("{a:?}").contains('f'));
    }

    #[test]
    fn send_email_request_default_is_empty() {
        let r = SendEmailRequest::default();
        assert!(r.from.is_empty());
        assert!(r.to.is_empty());
        assert!(r.subject.is_empty());
        assert!(r.text.is_none());
        assert!(r.html.is_none());
        assert!(r.attachments.is_empty());
        assert!(r.headers.is_empty());
    }
}
