//! Linear GraphQL client.
//!
//! Wraps the slice of mutations the adapter needs (create / update comment,
//! add reaction). Linear returns HTTP 200 even for logical errors (with a
//! non-empty `errors[]` array), so the client lifts those into
//! [`AdapterError`] variants.

use crate::queries;
use copperclaw_channels_core::AdapterError;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Input to [`LinearApi::create_comment`].
#[derive(Debug, Clone, Serialize)]
pub struct CommentCreateInput {
    /// Linear issue UUID the comment is created against.
    #[serde(rename = "issueId")]
    pub issue_id: String,
    /// Comment markdown body.
    pub body: String,
    /// Parent comment UUID for thread replies. Skipped if `None`.
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Input to [`LinearApi::update_comment`].
#[derive(Debug, Clone, Serialize)]
pub struct CommentUpdateInput {
    /// New markdown body of the comment.
    pub body: String,
}

/// Input to [`LinearApi::create_reaction`].
#[derive(Debug, Clone, Serialize)]
pub struct ReactionCreateInput {
    /// Linear comment UUID to react to.
    #[serde(rename = "commentId")]
    pub comment_id: String,
    /// Emoji shortcode (`thumbsup`, `tada`, `eyes`, …).
    pub emoji: String,
}

/// Comment id returned by Linear's `commentCreate` / `commentUpdate`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CommentRef {
    /// Linear comment UUID.
    pub id: String,
}

/// Minimal Linear GraphQL client.
#[derive(Debug, Clone)]
pub struct LinearApi {
    client: Client,
    api_base: String,
    api_key: String,
}

impl LinearApi {
    /// Build a client using the configured API key and base URL.
    #[must_use]
    pub fn new(api_base: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_client(Client::new(), api_base, api_key)
    }

    /// Construct with a caller-supplied `reqwest::Client`. Useful for tests
    /// that want a shared connection pool or custom timeouts.
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

    /// POST a GraphQL query and pluck a typed value out of `data.<field>`.
    ///
    /// Maps HTTP / GraphQL failures to [`AdapterError`]. Sets the
    /// `Authorization` header to the raw API key — Linear accepts either a
    /// raw `lin_api_...` token or an OAuth Bearer token there; the client
    /// passes the value through verbatim.
    pub async fn post_graphql(
        &self,
        query: &str,
        variables: Value,
        field: &str,
    ) -> Result<Value, AdapterError> {
        let body = json!({"query": query, "variables": variables});
        let resp = self
            .client
            .post(&self.api_base)
            .header("Authorization", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let value = read_linear_json(resp).await?;
        let data = value.get("data").cloned().ok_or_else(|| {
            AdapterError::BadRequest(format!(
                "linear response missing `data` for `{field}`: {value}"
            ))
        })?;
        let inner = data.get(field).cloned().ok_or_else(|| {
            AdapterError::BadRequest(format!(
                "linear response `data` missing field `{field}`: {data}"
            ))
        })?;
        Ok(inner)
    }

    /// `commentCreate` — post a comment on an issue (or reply to a parent
    /// comment when `parent_id` is set). Returns the new comment's UUID.
    pub async fn create_comment(
        &self,
        input: &CommentCreateInput,
    ) -> Result<CommentRef, AdapterError> {
        let value = self
            .post_graphql(
                queries::CREATE_COMMENT,
                json!({"input": input}),
                "commentCreate",
            )
            .await?;
        decode_comment_ref(&value, "commentCreate")
    }

    /// `commentUpdate` — edit an existing comment. Returns the comment's
    /// UUID (same as `id`).
    pub async fn update_comment(
        &self,
        id: &str,
        input: &CommentUpdateInput,
    ) -> Result<CommentRef, AdapterError> {
        let value = self
            .post_graphql(
                queries::UPDATE_COMMENT,
                json!({"id": id, "input": input}),
                "commentUpdate",
            )
            .await?;
        decode_comment_ref(&value, "commentUpdate")
    }

    /// `reactionCreate` — add an emoji reaction to a comment. Returns `()`.
    pub async fn create_reaction(&self, input: &ReactionCreateInput) -> Result<(), AdapterError> {
        let value = self
            .post_graphql(
                queries::CREATE_REACTION,
                json!({"input": input}),
                "reactionCreate",
            )
            .await?;
        let success = value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !success {
            return Err(AdapterError::BadRequest(format!(
                "linear reactionCreate did not return success: {value}"
            )));
        }
        Ok(())
    }
}

fn decode_comment_ref(value: &Value, op: &str) -> Result<CommentRef, AdapterError> {
    let success = value
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !success {
        return Err(AdapterError::BadRequest(format!(
            "linear {op} did not return success: {value}"
        )));
    }
    let comment = value.get("comment").ok_or_else(|| {
        AdapterError::BadRequest(format!("linear {op} response missing `comment`: {value}"))
    })?;
    let parsed: CommentRef = serde_json::from_value(comment.clone())
        .map_err(|e| AdapterError::Transport(format!("linear {op} comment decode failed: {e}")))?;
    Ok(parsed)
}

fn transport(err: &reqwest::Error) -> AdapterError {
    AdapterError::Transport(err.to_string())
}

/// Read a GraphQL response off the wire and map HTTP / errors[] failures to
/// `AdapterError`. On success, returns the full JSON value (so callers can
/// pluck `data.<field>` themselves).
async fn read_linear_json(resp: reqwest::Response) -> Result<Value, AdapterError> {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(AdapterError::Rate { retry_after });
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::Auth(format!(
            "linear returned {status}: {body}"
        )));
    }
    if status.is_server_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::Transport(format!(
            "linear returned {status}: {body}"
        )));
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AdapterError::BadRequest(format!(
            "linear returned {status}: {body}"
        )));
    }
    let value: Value = resp
        .json()
        .await
        .map_err(|e| AdapterError::Transport(format!("linear response not JSON: {e}")))?;
    classify_linear_payload(value, retry_after)
}

/// Linear returns 200 OK with `{"errors": [...], "data": null}` for logical
/// errors. Lift those into typed [`AdapterError`]s.
pub(crate) fn classify_linear_payload(
    value: Value,
    retry_after: Option<u64>,
) -> Result<Value, AdapterError> {
    let errors = value
        .get("errors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if errors.is_empty() {
        return Ok(value);
    }
    let messages: Vec<String> = errors
        .iter()
        .filter_map(|e| e.get("message").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let joined = if messages.is_empty() {
        "linear returned unknown error".to_owned()
    } else {
        messages.join("; ")
    };
    Err(map_linear_error(&joined, retry_after))
}

/// Choose the right `AdapterError` variant for a Linear `errors[]` message.
pub(crate) fn map_linear_error(message: &str, retry_after: Option<u64>) -> AdapterError {
    let lc = message.to_ascii_lowercase();
    if lc.contains("rate limit") || lc.contains("ratelimit") || lc.contains("too many requests") {
        return AdapterError::Rate { retry_after };
    }
    if lc.contains("authentication")
        || lc.contains("unauthorized")
        || lc.contains("invalid api key")
        || lc.contains("invalid token")
        || lc.contains("forbidden")
    {
        return AdapterError::Auth(message.to_owned());
    }
    AdapterError::BadRequest(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api_for(server: &MockServer) -> LinearApi {
        LinearApi::new(format!("{}/graphql", server.uri()), "lin_api_test")
    }

    #[test]
    fn map_linear_error_rate_limit_messages() {
        for msg in ["rate limit exceeded", "Too Many Requests", "RateLimit hit"] {
            match map_linear_error(msg, Some(7)) {
                AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(7)),
                other => panic!("expected Rate for {msg}, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_linear_error_auth_messages() {
        for msg in [
            "Authentication required",
            "Invalid API key",
            "Forbidden — admin only",
            "unauthorized request",
            "Invalid token presented",
        ] {
            match map_linear_error(msg, None) {
                AdapterError::Auth(_) => {}
                other => panic!("expected Auth for {msg}, got {other:?}"),
            }
        }
    }

    #[test]
    fn map_linear_error_other_is_bad_request() {
        match map_linear_error("Issue not found", None) {
            AdapterError::BadRequest(m) => assert_eq!(m, "Issue not found"),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_payload_returns_when_no_errors() {
        let v = json!({"data": {"commentCreate": {"success": true}}});
        let got = classify_linear_payload(v.clone(), None).unwrap();
        assert_eq!(got, v);
    }

    #[test]
    fn classify_payload_lifts_errors_message_into_bad_request() {
        let v = json!({"errors": [{"message": "Issue not found"}], "data": null});
        let err = classify_linear_payload(v, None).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("Issue not found")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_payload_lifts_rate_limit_to_rate() {
        let v = json!({"errors": [{"message": "rate limit exceeded"}]});
        let err = classify_linear_payload(v, Some(3)).unwrap_err();
        match err {
            AdapterError::Rate { retry_after } => assert_eq!(retry_after, Some(3)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[test]
    fn classify_payload_lifts_auth_message_to_auth() {
        let v = json!({"errors": [{"message": "Authentication required"}]});
        let err = classify_linear_payload(v, None).unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_payload_with_empty_errors_array_is_success() {
        let v = json!({"data": {"x": 1}, "errors": []});
        let got = classify_linear_payload(v.clone(), None).unwrap();
        assert_eq!(got, v);
    }

    #[test]
    fn classify_payload_with_unmessaged_errors_falls_back() {
        let v = json!({"errors": [{"code": "x"}]});
        let err = classify_linear_payload(v, None).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("unknown")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn classify_payload_joins_multiple_error_messages() {
        let v = json!({"errors": [{"message": "first"}, {"message": "second"}]});
        let err = classify_linear_payload(v, None).unwrap_err();
        match err {
            AdapterError::BadRequest(m) => {
                assert!(m.contains("first"));
                assert!(m.contains("second"));
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_success_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "lin_api_test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"success": true, "comment": {"id": "c-1"}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let r = api
            .create_comment(&CommentCreateInput {
                issue_id: "i-1".into(),
                body: "hello".into(),
                parent_id: None,
            })
            .await
            .unwrap();
        assert_eq!(r.id, "c-1");
    }

    #[tokio::test]
    async fn create_comment_with_parent_serializes_parent_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(wiremock::matchers::body_string_contains("parentId"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"success": true, "comment": {"id": "c-2"}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let r = api
            .create_comment(&CommentCreateInput {
                issue_id: "i-1".into(),
                body: "reply".into(),
                parent_id: Some("c-parent".into()),
            })
            .await
            .unwrap();
        assert_eq!(r.id, "c-2");
    }

    #[tokio::test]
    async fn create_comment_without_parent_omits_parent_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"success": true, "comment": {"id": "c-3"}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        // Direct serialization check to be sure `parentId` is skipped.
        let serialized = serde_json::to_string(&CommentCreateInput {
            issue_id: "i-1".into(),
            body: "hi".into(),
            parent_id: None,
        })
        .unwrap();
        assert!(!serialized.contains("parentId"));
        let _ = api
            .create_comment(&CommentCreateInput {
                issue_id: "i-1".into(),
                body: "hi".into(),
                parent_id: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_comment_no_success_field_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"comment": {"id": "c-9"}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i-1".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_missing_comment_field_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"success": true}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i-1".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("comment")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_missing_data_field_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "extensions": {"x": 1}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("data")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_missing_field_in_data_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"other": {"x": 1}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("commentCreate")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_401_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_403_is_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_429_with_retry_after_is_rate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "11"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Rate { retry_after }) => assert_eq!(retry_after, Some(11)),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_429_without_retry_after_is_rate_with_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Rate { retry_after }) => assert!(retry_after.is_none()),
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_5xx_is_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Transport(m)) => assert!(m.contains("503")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_400_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("400")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_graphql_errors_become_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "Issue not found"}],
                "data": null
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "nope".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("Issue not found")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_graphql_auth_error_is_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "Authentication required"}]
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Auth(_)) => {}
            other => panic!("expected Auth, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_comment_graphql_rate_limit_is_rate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "Rate limit reached"}]
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Rate { .. }) => {}
            other => panic!("expected Rate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_comment_success_returns_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentUpdate": {"success": true, "comment": {"id": "c-5"}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let r = api
            .update_comment(
                "c-5",
                &CommentUpdateInput {
                    body: "edited".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(r.id, "c-5");
    }

    #[tokio::test]
    async fn update_comment_graphql_error_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "Comment not found"}]
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .update_comment(
                "missing",
                &CommentUpdateInput {
                    body: "edited".into(),
                },
            )
            .await
        {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("Comment not found")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_comment_success_false_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentUpdate": {"success": false, "comment": {"id": "x"}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .update_comment("x", &CommentUpdateInput { body: "x".into() })
            .await
        {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_reaction_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"reactionCreate": {"success": true, "reaction": {"id": "r-1"}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        api.create_reaction(&ReactionCreateInput {
            comment_id: "c-1".into(),
            emoji: "thumbsup".into(),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn create_reaction_success_false_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"reactionCreate": {"success": false}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_reaction(&ReactionCreateInput {
                comment_id: "c-1".into(),
                emoji: "thumbsup".into(),
            })
            .await
        {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_reaction_graphql_error_is_bad_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{"message": "comment id invalid"}]
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_reaction(&ReactionCreateInput {
                comment_id: "c-1".into(),
                emoji: "tada".into(),
            })
            .await
        {
            Err(AdapterError::BadRequest(_)) => {}
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_json_response_is_transport_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Transport(m)) => assert!(m.contains("not JSON")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn comment_ref_decode_failure_surfaces_transport_error() {
        let server = MockServer::start().await;
        // comment field present but malformed (id is a number, not a string).
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"commentCreate": {"success": true, "comment": {"id": 7}}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        match api
            .create_comment(&CommentCreateInput {
                issue_id: "i".into(),
                body: "x".into(),
                parent_id: None,
            })
            .await
        {
            Err(AdapterError::Transport(m)) => assert!(m.contains("decode")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_graphql_is_used_directly_for_arbitrary_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"viewer": {"id": "u-1"}}
            })))
            .mount(&server)
            .await;
        let api = api_for(&server);
        let got = api
            .post_graphql("query { viewer { id } }", json!({}), "viewer")
            .await
            .unwrap();
        assert_eq!(got["id"], "u-1");
    }

    #[test]
    fn comment_ref_clone_and_eq() {
        let r = CommentRef { id: "c-1".into() };
        let r2 = r.clone();
        assert_eq!(r, r2);
        assert!(format!("{r:?}").contains("c-1"));
    }

    #[test]
    fn input_types_clone_and_debug() {
        let c = CommentCreateInput {
            issue_id: "i".into(),
            body: "b".into(),
            parent_id: Some("p".into()),
        };
        let _ = c.clone();
        let u = CommentUpdateInput { body: "b".into() };
        let _ = u.clone();
        let r = ReactionCreateInput {
            comment_id: "c".into(),
            emoji: "thumbsup".into(),
        };
        let _ = r.clone();
        assert!(format!("{c:?}").contains("issue_id"));
        assert!(format!("{u:?}").contains("body"));
        assert!(format!("{r:?}").contains("comment_id"));
    }

    #[test]
    fn linear_api_clone_and_debug() {
        let api = LinearApi::new("https://api.linear.app/graphql", "lin_api_x");
        let _ = api.clone();
        assert!(format!("{api:?}").contains("api_key"));
    }
}
