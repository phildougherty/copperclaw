//! GitHub REST API client.
//!
//! Wraps the small slice of the REST API the adapter needs:
//!
//! - `POST /repos/{owner}/{repo}/issues/{number}/comments` — post a comment.
//! - `PATCH /repos/{owner}/{repo}/issues/comments/{id}` — edit a comment.
//! - `POST /repos/{owner}/{repo}/issues/comments/{id}/reactions` — react to a
//!   comment.
//!
//! GitHub responses are classified into [`AdapterError`] variants:
//!
//! - HTTP 401 → [`AdapterError::Auth`].
//! - HTTP 403 with `X-RateLimit-Remaining: 0` → [`AdapterError::Rate`]
//!   (computing `retry_after` from `X-RateLimit-Reset`).
//! - HTTP 403 (otherwise) → [`AdapterError::Auth`].
//! - HTTP 404 / 422 → [`AdapterError::BadRequest`].
//! - HTTP 429 → [`AdapterError::Rate`] (honoring `Retry-After` when present).
//! - HTTP 5xx → [`AdapterError::Transport`].

use ironclaw_channels_core::AdapterError;
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

/// User-Agent string sent on every request. GitHub rejects requests without
/// one.
pub const USER_AGENT: &str = "ironclaw";

/// Subset of the issue-comment response we care about.
#[derive(Debug, Clone, Deserialize)]
pub struct CommentResponse {
    /// GitHub's numeric comment id. Used as the platform-side message id.
    pub id: i64,
    /// Public URL of the comment. Useful for log lines.
    #[serde(default)]
    pub html_url: Option<String>,
}

/// Minimal GitHub REST client.
#[derive(Debug, Clone)]
pub struct GithubApi {
    client: Client,
    api_base: String,
    token: String,
}

impl GithubApi {
    /// Build a client using the configured token and base URL.
    ///
    /// Uses [`reqwest::Client::new`] for default settings.
    #[must_use]
    pub fn new(api_base: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_client(Client::new(), api_base, token)
    }

    /// Construct with a caller-supplied `reqwest::Client`. Useful for tests
    /// that want a shared connection pool or custom timeouts.
    #[must_use]
    pub fn with_client(
        client: Client,
        api_base: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_base: api_base.into(),
            token: token.into(),
        }
    }

    fn url(&self, suffix: &str) -> String {
        format!(
            "{}/{}",
            self.api_base.trim_end_matches('/'),
            suffix.trim_start_matches('/')
        )
    }

    /// `POST /repos/{owner}/{repo}/issues/{number}/comments`. Returns the new
    /// comment's id.
    pub async fn post_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<CommentResponse, AdapterError> {
        let url = self.url(&format!("repos/{owner}/{repo}/issues/{number}/comments"));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", USER_AGENT)
            .json(&json!({"body": body}))
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_github_json(resp).await?;
        serde_json::from_value::<CommentResponse>(value)
            .map_err(|e| AdapterError::Transport(format!("post_comment decode: {e}")))
    }

    /// `PATCH /repos/{owner}/{repo}/issues/comments/{comment_id}`. Returns
    /// the updated comment.
    pub async fn edit_comment(
        &self,
        owner: &str,
        repo: &str,
        comment_id: i64,
        body: &str,
    ) -> Result<CommentResponse, AdapterError> {
        let url = self.url(&format!("repos/{owner}/{repo}/issues/comments/{comment_id}"));
        let resp = self
            .client
            .patch(&url)
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", USER_AGENT)
            .json(&json!({"body": body}))
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_github_json(resp).await?;
        serde_json::from_value::<CommentResponse>(value)
            .map_err(|e| AdapterError::Transport(format!("edit_comment decode: {e}")))
    }

    /// `POST /repos/{owner}/{repo}/issues/comments/{comment_id}/reactions`.
    /// `slug` must be one of the eight GitHub-accepted reaction strings.
    pub async fn add_reaction(
        &self,
        owner: &str,
        repo: &str,
        comment_id: i64,
        slug: &str,
    ) -> Result<(), AdapterError> {
        let url = self.url(&format!(
            "repos/{owner}/{repo}/issues/comments/{comment_id}/reactions"
        ));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", USER_AGENT)
            .json(&json!({"content": slug}))
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let _ = read_github_json(resp).await?;
        Ok(())
    }
}

fn transport(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(err.to_string())
}

/// Translate the raw HTTP response into an [`AdapterError`] or a JSON value.
async fn read_github_json(resp: Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after_header = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let rate_remaining = resp
        .headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let rate_reset = resp
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let now_secs = chrono::Utc::now().timestamp();

    if status == StatusCode::UNAUTHORIZED {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::Auth(format!("github 401: {body}")));
    }
    if status == StatusCode::FORBIDDEN {
        // 403 + rate-limit-remaining=0 → really a rate-limit failure.
        if rate_remaining == Some(0) {
            let retry_after = match (rate_reset, retry_after_header) {
                (Some(reset), _) => Some(u64::try_from((reset - now_secs).max(0)).unwrap_or(0)),
                (None, Some(ra)) => Some(ra),
                _ => None,
            };
            return Err(AdapterError::Rate { retry_after });
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::Auth(format!("github 403: {body}")));
    }
    if status == StatusCode::NOT_FOUND {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::BadRequest(format!("github 404: {body}")));
    }
    if status == StatusCode::UNPROCESSABLE_ENTITY {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::BadRequest(format!("github 422: {body}")));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AdapterError::Rate {
            retry_after: retry_after_header,
        });
    }
    if status.is_server_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::Transport(format!(
            "github {status}: {body}"
        )));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::BadRequest(format!(
            "github {status}: {body}"
        )));
    }

    // 2xx — body may be empty (e.g. some 201 responses without content).
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AdapterError::Transport(format!("github body read: {e}")))?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| AdapterError::Transport(format!("github response not JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_for(server: &MockServer) -> GithubApi {
        GithubApi::new(server.uri(), "ghp-test")
    }

    #[tokio::test]
    async fn url_handles_trailing_slash() {
        let api = GithubApi::new("https://api.example/", "t");
        assert_eq!(api.url("repos/o/r"), "https://api.example/repos/o/r");
        let api = GithubApi::new("https://api.example", "t");
        assert_eq!(api.url("/repos/o/r"), "https://api.example/repos/o/r");
    }

    #[tokio::test]
    async fn post_comment_returns_id_and_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .and(header("authorization", "Bearer ghp-test"))
            .and(header("user-agent", USER_AGENT))
            .and(header("accept", "application/vnd.github+json"))
            .and(body_json(json!({"body": "hi"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": 42, "html_url": "https://github.com/o/r/issues/7#issuecomment-42"
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let resp = api.post_comment("o", "r", 7, "hi").await.unwrap();
        assert_eq!(resp.id, 42);
        assert_eq!(
            resp.html_url.as_deref(),
            Some("https://github.com/o/r/issues/7#issuecomment-42")
        );
    }

    #[tokio::test]
    async fn edit_comment_uses_patch_and_returns_response() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/comments/42"))
            .and(body_json(json!({"body":"edited"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": 42, "html_url": "u"})),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        let resp = api.edit_comment("o", "r", 42, "edited").await.unwrap();
        assert_eq!(resp.id, 42);
        assert_eq!(resp.html_url.as_deref(), Some("u"));
    }

    #[tokio::test]
    async fn add_reaction_uses_post_with_slug_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/comments/42/reactions"))
            .and(body_json(json!({"content":"+1"})))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 99})))
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.add_reaction("o", "r", 42, "+1").await.unwrap();
    }

    #[tokio::test]
    async fn add_reaction_tolerates_empty_2xx_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/comments/42/reactions"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(Vec::<u8>::new()))
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.add_reaction("o", "r", 42, "heart").await.unwrap();
    }

    #[tokio::test]
    async fn unauthorized_response_maps_to_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Bad credentials"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Auth(m)) => assert!(m.contains("401")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forbidden_with_zero_remaining_maps_to_rate() {
        let server = MockServer::start().await;
        let reset = chrono::Utc::now().timestamp() + 30;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", reset.to_string().as_str())
                    .set_body_string("rate limited"),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Rate { retry_after }) => {
                assert!(retry_after.unwrap_or(0) <= 30);
                assert!(retry_after.unwrap_or(0) >= 25);
            }
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forbidden_with_zero_remaining_no_reset_uses_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("retry-after", "12")
                    .set_body_string("limit"),
            )
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(12)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn forbidden_without_zero_remaining_maps_to_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Auth(m)) => assert!(m.contains("403")),
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_found_maps_to_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(404).set_body_string("missing"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("404")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unprocessable_entity_maps_to_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(422).set_body_string("validation"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("422")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn too_many_requests_maps_to_rate_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "5"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(5)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn too_many_requests_without_retry_after_is_rate_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Rate { retry_after }) => assert!(retry_after.is_none()),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_error_maps_to_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("503")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unhandled_4xx_maps_to_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(418))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("418")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_json_2xx_maps_to_transport() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/7/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_string("not json"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Transport(m)) => assert!(m.contains("not JSON")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_error_on_unreachable_host() {
        // Reserved IP that does not exist; reqwest will return a connect error
        // synchronously without ever talking to wiremock.
        let api = GithubApi::new("http://127.0.0.1:1", "t");
        match api.post_comment("o", "r", 7, "x").await {
            Err(AdapterError::Transport(_)) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_clone_and_debug() {
        let api = GithubApi::new("https://api.example", "ghp-x");
        let cloned = api.clone();
        assert_eq!(cloned.api_base, "https://api.example");
        assert!(format!("{api:?}").contains("ghp-x"));
    }

    #[tokio::test]
    async fn with_client_keeps_supplied_client() {
        let client = Client::new();
        let api = GithubApi::with_client(client, "https://api.example", "t");
        assert_eq!(api.token, "t");
    }

    #[tokio::test]
    async fn edit_comment_404_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/o/r/issues/comments/9"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.edit_comment("o", "r", 9, "x").await {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_reaction_422_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/o/r/issues/comments/9/reactions"))
            .respond_with(ResponseTemplate::new(422).set_body_string("bad slug"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api.add_reaction("o", "r", 9, "rocket").await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("422")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn comment_response_accepts_missing_html_url() {
        let v = json!({"id": 1});
        let parsed: CommentResponse = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.id, 1);
        assert!(parsed.html_url.is_none());
    }

    #[test]
    fn comment_response_keeps_html_url() {
        let v = json!({"id": 2, "html_url": "u"});
        let parsed: CommentResponse = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.html_url.as_deref(), Some("u"));
    }
}
