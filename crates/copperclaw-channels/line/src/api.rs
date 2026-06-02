//! LINE Messaging API client (egress).
//!
//! Two calls are implemented — the ones the channel adapter actually
//! uses:
//!
//! - `reply` → `POST /v2/bot/message/reply`. Costs nothing on the
//!   LINE side but only works once per `reply_token`, and the token
//!   expires ~30 seconds after delivery.
//! - `push` → `POST /v2/bot/message/push`. Charged per message; used
//!   when the reply window has elapsed or when the agent
//!   self-initiates.
//!
//! The adapter prefers `reply` when it has a still-fresh reply token
//! and falls back to `push` otherwise.
//!
//! HTTP-error → `AdapterError` mapping follows
//! `docs/adding-a-channel.md` § 5.

use copperclaw_channels_core::AdapterError;
use reqwest::Client;
use serde::Serialize;

/// LINE REST client. One per adapter; cheap to clone.
#[derive(Clone, Debug)]
pub struct LineApi {
    base_url: String,
    token: String,
    client: Client,
}

impl LineApi {
    /// Build a client targeting `base_url` (no trailing slash) with
    /// the supplied Channel Access Token.
    #[must_use]
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            client: Client::new(),
        }
    }

    /// `POST /v2/bot/message/reply`. Returns Ok(()) on success — LINE
    /// does not surface a message id for replies.
    pub async fn reply(&self, reply_token: &str, text: &str) -> Result<(), AdapterError> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(rename = "replyToken")]
            reply_token: &'a str,
            messages: Vec<TextMessage<'a>>,
        }
        let body = Body {
            reply_token,
            messages: vec![TextMessage::new(text)],
        };
        self.post("/v2/bot/message/reply", &body).await
    }

    /// `POST /v2/bot/message/push`. Used when no reply token is
    /// available (or it's stale).
    pub async fn push(&self, to: &str, text: &str) -> Result<(), AdapterError> {
        #[derive(Serialize)]
        struct Body<'a> {
            to: &'a str,
            messages: Vec<TextMessage<'a>>,
        }
        let body = Body {
            to,
            messages: vec![TextMessage::new(text)],
        };
        self.post("/v2/bot/message/push", &body).await
    }

    async fn post<B: Serialize>(&self, path: &str, body: &B) -> Result<(), AdapterError> {
        let res = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| AdapterError::Transport(e.to_string()))?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        Err(map_error(status, res).await)
    }
}

#[derive(Serialize)]
struct TextMessage<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

impl<'a> TextMessage<'a> {
    fn new(text: &'a str) -> Self {
        Self { kind: "text", text }
    }
}

async fn map_error(status: reqwest::StatusCode, res: reqwest::Response) -> AdapterError {
    let body = res
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable body>".to_string());
    let snippet: String = body.chars().take(256).collect();
    match status.as_u16() {
        401 | 403 => AdapterError::Auth(snippet),
        400 | 404 | 422 => AdapterError::BadRequest(format!("{status}: {snippet}")),
        429 => AdapterError::Rate { retry_after: None },
        500..=599 => AdapterError::Transport(format!("server {status}: {snippet}")),
        _ => AdapterError::Transport(format!("unexpected {status}: {snippet}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn server() -> MockServer {
        MockServer::start().await
    }

    #[tokio::test]
    async fn reply_succeeds_on_200() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&mock)
            .await;
        let api = LineApi::new(&mock.uri(), "test-token");
        api.reply("rt-1", "hi").await.unwrap();
    }

    #[tokio::test]
    async fn push_succeeds_on_200() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&mock)
            .await;
        let api = LineApi::new(&mock.uri(), "t");
        api.push("U123", "hi").await.unwrap();
    }

    #[tokio::test]
    async fn auth_failure_maps_to_auth_error() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad token"))
            .mount(&mock)
            .await;
        let api = LineApi::new(&mock.uri(), "wrong");
        assert!(matches!(
            api.push("U", "x").await.unwrap_err(),
            AdapterError::Auth(_)
        ));
    }

    #[tokio::test]
    async fn bad_request_maps_to_bad_request() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid reply token"))
            .mount(&mock)
            .await;
        let api = LineApi::new(&mock.uri(), "t");
        match api.reply("stale", "x").await.unwrap_err() {
            AdapterError::BadRequest(m) => assert!(m.contains("invalid reply token")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rate_limit_maps_to_rate() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow"))
            .mount(&mock)
            .await;
        let api = LineApi::new(&mock.uri(), "t");
        assert!(matches!(
            api.push("U", "x").await.unwrap_err(),
            AdapterError::Rate { .. }
        ));
    }

    #[tokio::test]
    async fn server_error_maps_to_transport() {
        let mock = server().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .respond_with(ResponseTemplate::new(503).set_body_string("backend down"))
            .mount(&mock)
            .await;
        let api = LineApi::new(&mock.uri(), "t");
        assert!(matches!(
            api.reply("rt", "x").await.unwrap_err(),
            AdapterError::Transport(_)
        ));
    }

    #[test]
    fn new_strips_trailing_slash_from_base() {
        let a = LineApi::new("https://api.line.me/", "t");
        assert!(!a.base_url.ends_with('/'));
    }
}
