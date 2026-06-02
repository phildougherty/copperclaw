//! HTTP transport for the `OneCLI` Agent Vault service.
//!
//! See `PLAN.md` § 6 (T11). [`OneCliClient`] is a thin wrapper around a
//! [`reqwest::Client`] that knows the small set of endpoints copperclaw needs
//! and centralizes status-to-error mapping in [`map_status`].

use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderValue, RETRY_AFTER};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::OneCliError;
use crate::types::{AgentSummary, ContainerProvisioning, PendingApproval};

/// Maximum number of characters retained from a response body when surfacing
/// it inside [`OneCliError::Server`] or [`OneCliError::Api`].
const MAX_BODY_CHARS: usize = 4096;

/// HTTP client for the `OneCLI` Agent Vault service.
///
/// Construct with [`OneCliClient::new`] for default transport, or
/// [`OneCliClient::with_http_client`] when a caller needs to supply a
/// pre-configured [`reqwest::Client`] (timeouts, proxies, fake transports in
/// tests).
#[derive(Debug, Clone)]
pub struct OneCliClient {
    http: Client,
    base_url: Url,
    bearer_token: String,
}

impl OneCliClient {
    /// Build a client against `base_url` using the default [`reqwest::Client`].
    ///
    /// `base_url` must be a syntactically valid absolute URL; otherwise a
    /// [`OneCliError::Transport`] is returned describing the parse failure.
    /// The trailing slash is normalized so that relative joins below work
    /// regardless of how the caller wrote the prefix.
    pub fn new(
        base_url: impl AsRef<str>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, OneCliError> {
        Self::with_http_client(base_url, bearer_token, Client::new())
    }

    /// Build a client against `base_url` using the supplied [`reqwest::Client`].
    ///
    /// Same URL-validation contract as [`Self::new`].
    pub fn with_http_client(
        base_url: impl AsRef<str>,
        bearer_token: impl Into<String>,
        http: Client,
    ) -> Result<Self, OneCliError> {
        let raw = base_url.as_ref();
        let normalized = if raw.ends_with('/') {
            raw.to_owned()
        } else {
            format!("{raw}/")
        };
        let base_url = Url::parse(&normalized)
            .map_err(|e| OneCliError::Transport(format!("invalid base URL: {e}")))?;
        Ok(Self {
            http,
            base_url,
            bearer_token: bearer_token.into(),
        })
    }

    /// Ensure an agent record exists for `slug` and return its summary.
    ///
    /// Idempotent: POSTs to `/v1/agents` with `{slug, display_name?}`. A `200`
    /// or `201` response carries the [`AgentSummary`] directly. A `409` means
    /// the slug already exists; in that case the client performs a follow-up
    /// `GET /v1/agents/by-slug/{slug}` to return the existing record so the
    /// caller can treat the operation as pure upsert.
    pub async fn ensure_agent(
        &self,
        slug: &str,
        display_name: Option<&str>,
    ) -> Result<AgentSummary, OneCliError> {
        let url = self.endpoint("v1/agents")?;
        let body = CreateAgentRequest { slug, display_name };
        let resp = self
            .http
            .post(url)
            .header(AUTHORIZATION, self.auth_value())
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(transport_err)?;

        let status = resp.status();
        if status == StatusCode::OK || status == StatusCode::CREATED {
            return parse_json::<AgentSummary>(resp).await;
        }
        if status == StatusCode::CONFLICT {
            return self.fetch_agent_by_slug(slug).await;
        }
        Err(self.error_for_response(resp).await)
    }

    async fn fetch_agent_by_slug(&self, slug: &str) -> Result<AgentSummary, OneCliError> {
        let path = format!("v1/agents/by-slug/{}", urlencoding(slug));
        let url = self.endpoint(&path)?;
        let resp = self
            .http
            .get(url)
            .header(AUTHORIZATION, self.auth_value())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(transport_err)?;
        if resp.status().is_success() {
            return parse_json::<AgentSummary>(resp).await;
        }
        Err(self.error_for_response(resp).await)
    }

    /// Push provisioning state for `agent_id`.
    ///
    /// PUTs to `/v1/agents/{agent_id}/provisioning`; a `204` response means
    /// the vault accepted the config. Any non-2xx is mapped via
    /// [`map_status`].
    pub async fn apply_container_config(
        &self,
        agent_id: &str,
        config: &ContainerProvisioning,
    ) -> Result<(), OneCliError> {
        let path = format!("v1/agents/{}/provisioning", urlencoding(agent_id));
        let url = self.endpoint(&path)?;
        let resp = self
            .http
            .put(url)
            .header(AUTHORIZATION, self.auth_value())
            .header(ACCEPT, "application/json")
            .json(config)
            .send()
            .await
            .map_err(transport_err)?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(self.error_for_response(resp).await)
    }

    /// List approvals for `agent_id` that are currently pending operator action.
    ///
    /// GETs `/v1/agents/{agent_id}/approvals?status=pending`. The vault
    /// returns `{ "items": [...] }`; this method returns the unwrapped vector.
    pub async fn list_pending_approvals(
        &self,
        agent_id: &str,
    ) -> Result<Vec<PendingApproval>, OneCliError> {
        let path = format!("v1/agents/{}/approvals", urlencoding(agent_id));
        let mut url = self.endpoint(&path)?;
        url.query_pairs_mut().append_pair("status", "pending");
        let resp = self
            .http
            .get(url)
            .header(AUTHORIZATION, self.auth_value())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(transport_err)?;
        if resp.status().is_success() {
            let envelope: ApprovalListResponse = parse_json(resp).await?;
            return Ok(envelope.items);
        }
        Err(self.error_for_response(resp).await)
    }

    /// Approve a pending approval by id. POSTs `/v1/approvals/{id}/approve`;
    /// success is `204 No Content`.
    pub async fn approve(&self, approval_id: &str) -> Result<(), OneCliError> {
        let path = format!("v1/approvals/{}/approve", urlencoding(approval_id));
        let url = self.endpoint(&path)?;
        let resp = self
            .http
            .post(url)
            .header(AUTHORIZATION, self.auth_value())
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(transport_err)?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(self.error_for_response(resp).await)
    }

    /// Deny a pending approval by id with an optional human reason. POSTs
    /// `/v1/approvals/{id}/deny` with `{ reason?: string }`; success is `204
    /// No Content`.
    pub async fn deny(&self, approval_id: &str, reason: Option<&str>) -> Result<(), OneCliError> {
        let path = format!("v1/approvals/{}/deny", urlencoding(approval_id));
        let url = self.endpoint(&path)?;
        let body = DenyRequest { reason };
        let resp = self
            .http
            .post(url)
            .header(AUTHORIZATION, self.auth_value())
            .header(ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(transport_err)?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(self.error_for_response(resp).await)
    }

    fn endpoint(&self, path: &str) -> Result<Url, OneCliError> {
        self.base_url
            .join(path)
            .map_err(|e| OneCliError::Transport(format!("invalid endpoint path: {e}")))
    }

    fn auth_value(&self) -> String {
        format!("Bearer {}", self.bearer_token)
    }

    async fn error_for_response(&self, resp: reqwest::Response) -> OneCliError {
        let status = resp.status();
        let retry_after = resp.headers().get(RETRY_AFTER).cloned();
        let body = resp.text().await.unwrap_or_default();
        map_status(status, &body, retry_after.as_ref())
    }
}

#[derive(Serialize)]
struct CreateAgentRequest<'a> {
    slug: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<&'a str>,
}

#[derive(Serialize)]
struct DenyRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

#[derive(Deserialize)]
struct ApprovalListResponse {
    items: Vec<PendingApproval>,
}

async fn parse_json<T: for<'de> Deserialize<'de>>(
    resp: reqwest::Response,
) -> Result<T, OneCliError> {
    let bytes = resp.bytes().await.map_err(transport_err)?;
    serde_json::from_slice(&bytes).map_err(|e| OneCliError::Decode(format!("{e}")))
}

#[allow(clippy::needless_pass_by_value)]
fn transport_err(e: reqwest::Error) -> OneCliError {
    OneCliError::Transport(format!("{e}"))
}

fn truncate_body(body: &str) -> String {
    body.chars().take(MAX_BODY_CHARS).collect()
}

/// Map an HTTP `status` + body + optional `Retry-After` header into the
/// closest matching [`OneCliError`] variant. Centralized so every endpoint
/// call site is a one-line `Err(self.error_for_response(resp).await)`.
fn map_status(status: StatusCode, body: &str, retry_after: Option<&HeaderValue>) -> OneCliError {
    match status.as_u16() {
        401 => OneCliError::Unauthorized,
        404 => OneCliError::NotFound,
        409 => OneCliError::Conflict {
            message: truncate_body(body),
        },
        429 => OneCliError::RateLimited {
            retry_after: retry_after.and_then(parse_retry_after),
        },
        500..=599 => OneCliError::Server(truncate_body(body)),
        other => OneCliError::Api {
            status: other,
            message: truncate_body(body),
        },
    }
}

/// Parse a `Retry-After` header value into whole seconds.
///
/// Accepts either an integer seconds count or an HTTP-date; for the latter we
/// return the delta from "now" clamped to non-negative. Returns `None` for
/// values that match neither form.
fn parse_retry_after(value: &HeaderValue) -> Option<u64> {
    let s = value.to_str().ok()?.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    let parsed = DateTime::parse_from_rfc2822(s)
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .ok()?;
    let when: DateTime<Utc> = parsed.with_timezone(&Utc);
    let now = Utc::now();
    let delta = when.signed_duration_since(now).num_seconds();
    if delta < 0 {
        Some(0)
    } else {
        Some(u64::try_from(delta).unwrap_or(0))
    }
}

/// Minimal percent-encoder for path segments. Reqwest's URL is built via
/// `Url::join`, so we encode forbidden characters before substitution.
fn urlencoding(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EnvVarSpec, NetworkPolicy, SecretMountSpec};
    use chrono::Duration;
    use reqwest::header::HeaderValue;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "test-token";

    fn client_for(server: &MockServer) -> OneCliClient {
        OneCliClient::new(server.uri(), TOKEN).expect("client builds")
    }

    #[test]
    fn new_rejects_malformed_url() {
        let err = OneCliClient::new("not a url", TOKEN).unwrap_err();
        match err {
            OneCliError::Transport(msg) => assert!(msg.contains("invalid base URL")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn with_http_client_rejects_malformed_url() {
        let err = OneCliClient::with_http_client("::::", TOKEN, Client::new()).unwrap_err();
        assert!(matches!(err, OneCliError::Transport(_)));
    }

    #[test]
    fn new_normalizes_trailing_slash() {
        // Both forms must produce a client whose joins resolve correctly.
        let a = OneCliClient::new("https://vault.example.com", TOKEN).unwrap();
        let b = OneCliClient::new("https://vault.example.com/", TOKEN).unwrap();
        assert_eq!(a.base_url.as_str(), b.base_url.as_str());
    }

    #[test]
    fn truncate_body_caps_at_4096_chars() {
        let huge = "x".repeat(5000);
        let out = truncate_body(&huge);
        assert_eq!(out.chars().count(), 4096);
    }

    #[test]
    fn urlencoding_escapes_special_chars() {
        assert_eq!(urlencoding("safe-id_1.2~"), "safe-id_1.2~");
        assert_eq!(urlencoding("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn map_status_401_is_unauthorized() {
        let e = map_status(StatusCode::UNAUTHORIZED, "no", None);
        assert!(matches!(e, OneCliError::Unauthorized));
    }

    #[test]
    fn map_status_404_is_not_found() {
        let e = map_status(StatusCode::NOT_FOUND, "missing", None);
        assert!(matches!(e, OneCliError::NotFound));
    }

    #[test]
    fn map_status_409_carries_message() {
        let e = map_status(StatusCode::CONFLICT, "slug-taken", None);
        match e {
            OneCliError::Conflict { message } => assert_eq!(message, "slug-taken"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_status_429_parses_integer_retry_after() {
        let header = HeaderValue::from_static("42");
        let e = map_status(StatusCode::TOO_MANY_REQUESTS, "", Some(&header));
        match e {
            OneCliError::RateLimited { retry_after } => assert_eq!(retry_after, Some(42)),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_status_429_without_header() {
        let e = map_status(StatusCode::TOO_MANY_REQUESTS, "", None);
        assert!(matches!(e, OneCliError::RateLimited { retry_after: None }));
    }

    #[test]
    fn parse_retry_after_rfc2822_date() {
        let future = Utc::now() + Duration::seconds(120);
        let formatted = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let header = HeaderValue::from_str(&formatted).unwrap();
        let secs = parse_retry_after(&header).expect("parsed");
        // Allow a small window because clock advances between calls.
        assert!((100..=120).contains(&secs), "got {secs}");
    }

    #[test]
    fn parse_retry_after_rfc3339_date() {
        let future = Utc::now() + Duration::seconds(60);
        let s = future.to_rfc3339();
        let header = HeaderValue::from_str(&s).unwrap();
        let secs = parse_retry_after(&header).expect("parsed");
        assert!((40..=60).contains(&secs), "got {secs}");
    }

    #[test]
    fn parse_retry_after_past_date_clamps_to_zero() {
        let past = Utc::now() - Duration::seconds(120);
        let s = past.to_rfc3339();
        let header = HeaderValue::from_str(&s).unwrap();
        assert_eq!(parse_retry_after(&header), Some(0));
    }

    #[test]
    fn parse_retry_after_garbage_is_none() {
        let header = HeaderValue::from_static("not-a-date");
        assert!(parse_retry_after(&header).is_none());
    }

    #[test]
    fn map_status_500_is_server() {
        let e = map_status(StatusCode::INTERNAL_SERVER_ERROR, "boom", None);
        match e {
            OneCliError::Server(msg) => assert_eq!(msg, "boom"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn map_status_503_is_server() {
        let e = map_status(StatusCode::SERVICE_UNAVAILABLE, "down", None);
        assert!(matches!(e, OneCliError::Server(_)));
    }

    #[test]
    fn map_status_other_4xx_is_api() {
        let e = map_status(StatusCode::IM_A_TEAPOT, "tea", None);
        match e {
            OneCliError::Api { status, message } => {
                assert_eq!(status, 418);
                assert_eq!(message, "tea");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- ensure_agent ----

    #[tokio::test]
    async fn ensure_agent_happy_201() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .and(header("authorization", "Bearer test-token"))
            .and(header("accept", "application/json"))
            .and(body_json(
                json!({"slug": "demo", "display_name": "Demo Agent"}),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "ag_1",
                "slug": "demo",
                "display_name": "Demo Agent",
            })))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let out = c.ensure_agent("demo", Some("Demo Agent")).await.unwrap();
        assert_eq!(out.id, "ag_1");
        assert_eq!(out.slug, "demo");
        assert_eq!(out.display_name.as_deref(), Some("Demo Agent"));
    }

    #[tokio::test]
    async fn ensure_agent_happy_200_without_display_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .and(body_json(json!({"slug": "demo"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "ag_1",
                "slug": "demo",
            })))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let out = c.ensure_agent("demo", None).await.unwrap();
        assert_eq!(out.id, "ag_1");
        assert!(out.display_name.is_none());
    }

    #[tokio::test]
    async fn ensure_agent_409_triggers_by_slug_lookup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(ResponseTemplate::new(409).set_body_string("slug exists"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/by-slug/demo"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "ag_existing",
                "slug": "demo",
                "display_name": "Existing",
            })))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let out = c.ensure_agent("demo", Some("New")).await.unwrap();
        assert_eq!(out.id, "ag_existing");
    }

    #[tokio::test]
    async fn ensure_agent_409_then_by_slug_404_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/by-slug/demo"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        assert!(matches!(err, OneCliError::NotFound));
    }

    #[tokio::test]
    async fn ensure_agent_401_is_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        assert!(matches!(err, OneCliError::Unauthorized));
    }

    #[tokio::test]
    async fn ensure_agent_500_is_server() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(ResponseTemplate::new(500).set_body_string("kaboom"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        match err {
            OneCliError::Server(msg) => assert_eq!(msg, "kaboom"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ensure_agent_418_is_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(ResponseTemplate::new(418).set_body_string("tea"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        match err {
            OneCliError::Api { status, message } => {
                assert_eq!(status, 418);
                assert_eq!(message, "tea");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ensure_agent_429_with_retry_after_seconds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        assert!(matches!(
            err,
            OneCliError::RateLimited {
                retry_after: Some(30)
            }
        ));
    }

    #[tokio::test]
    async fn ensure_agent_429_with_http_date() {
        let server = MockServer::start().await;
        let future = Utc::now() + Duration::seconds(75);
        let formatted = future.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(
                ResponseTemplate::new(429).insert_header("retry-after", formatted.as_str()),
            )
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        match err {
            OneCliError::RateLimited {
                retry_after: Some(secs),
            } => {
                assert!((55..=75).contains(&secs), "got {secs}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn ensure_agent_bad_json_is_decode_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        assert!(matches!(err, OneCliError::Decode(_)));
    }

    #[tokio::test]
    async fn ensure_agent_transport_error_on_bad_host() {
        // 127.0.0.1:1 should refuse connection synchronously.
        let c = OneCliClient::new("http://127.0.0.1:1", TOKEN).unwrap();
        let err = c.ensure_agent("demo", None).await.unwrap_err();
        assert!(matches!(err, OneCliError::Transport(_)));
    }

    // ---- apply_container_config ----

    fn sample_config() -> ContainerProvisioning {
        ContainerProvisioning {
            env: vec![EnvVarSpec {
                name: "K".into(),
                secret_ref: "vault://k".into(),
            }],
            mounts: vec![SecretMountSpec {
                mount_path: "/m".into(),
                secret_ref: "vault://m".into(),
                mode: Some(0o400),
            }],
            network_policy: NetworkPolicy::Allowlist,
            allowed_egress: vec!["api.example.com".into()],
        }
    }

    #[tokio::test]
    async fn apply_container_config_happy_204() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/agents/ag_1/provisioning"))
            .and(header("authorization", "Bearer test-token"))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let c = client_for(&server);
        c.apply_container_config("ag_1", &sample_config())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn apply_container_config_404() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/agents/ag_1/provisioning"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c
            .apply_container_config("ag_1", &sample_config())
            .await
            .unwrap_err();
        assert!(matches!(err, OneCliError::NotFound));
    }

    #[tokio::test]
    async fn apply_container_config_401() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/agents/ag_1/provisioning"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c
            .apply_container_config("ag_1", &sample_config())
            .await
            .unwrap_err();
        assert!(matches!(err, OneCliError::Unauthorized));
    }

    #[tokio::test]
    async fn apply_container_config_500() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/agents/ag_1/provisioning"))
            .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c
            .apply_container_config("ag_1", &sample_config())
            .await
            .unwrap_err();
        match err {
            OneCliError::Server(b) => assert_eq!(b, "nope"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_container_config_418_is_api() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/agents/ag_1/provisioning"))
            .respond_with(ResponseTemplate::new(418).set_body_string("teapot"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c
            .apply_container_config("ag_1", &sample_config())
            .await
            .unwrap_err();
        match err {
            OneCliError::Api { status, message } => {
                assert_eq!(status, 418);
                assert_eq!(message, "teapot");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_container_config_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/v1/agents/ag_1/provisioning"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "12"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c
            .apply_container_config("ag_1", &sample_config())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            OneCliError::RateLimited {
                retry_after: Some(12)
            }
        ));
    }

    // ---- list_pending_approvals ----

    #[tokio::test]
    async fn list_pending_approvals_happy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/ag_1/approvals"))
            .and(query_param("status", "pending"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {
                        "id": "apr_1",
                        "agent_id": "ag_1",
                        "action": "read_secret",
                        "requested_at": "2026-05-20T12:00:00Z",
                    },
                    {
                        "id": "apr_2",
                        "agent_id": "ag_1",
                        "action": "exec",
                        "reason": "debug",
                        "requested_at": "2026-05-20T12:01:00Z",
                    }
                ]
            })))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let items = c.list_pending_approvals("ag_1").await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "apr_1");
        assert_eq!(items[1].reason.as_deref(), Some("debug"));
    }

    #[tokio::test]
    async fn list_pending_approvals_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/ag_1/approvals"))
            .and(query_param("status", "pending"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": []})))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(c.list_pending_approvals("ag_1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_pending_approvals_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/ag_1/approvals"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.list_pending_approvals("ag_1").await.unwrap_err();
        assert!(matches!(err, OneCliError::NotFound));
    }

    #[tokio::test]
    async fn list_pending_approvals_401() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/ag_1/approvals"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.list_pending_approvals("ag_1").await.unwrap_err();
        assert!(matches!(err, OneCliError::Unauthorized));
    }

    #[tokio::test]
    async fn list_pending_approvals_500() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/ag_1/approvals"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oh no"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.list_pending_approvals("ag_1").await.unwrap_err();
        assert!(matches!(err, OneCliError::Server(_)));
    }

    #[tokio::test]
    async fn list_pending_approvals_bad_json_is_decode() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/ag_1/approvals"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{not json"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.list_pending_approvals("ag_1").await.unwrap_err();
        assert!(matches!(err, OneCliError::Decode(_)));
    }

    #[tokio::test]
    async fn list_pending_approvals_418_is_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/ag_1/approvals"))
            .respond_with(ResponseTemplate::new(418).set_body_string("tea"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        let err = c.list_pending_approvals("ag_1").await.unwrap_err();
        match err {
            OneCliError::Api { status, message } => {
                assert_eq!(status, 418);
                assert_eq!(message, "tea");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- approve ----

    #[tokio::test]
    async fn approve_happy_204() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/approve"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let c = client_for(&server);
        c.approve("apr_1").await.unwrap();
    }

    #[tokio::test]
    async fn approve_404() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/approve"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.approve("apr_1").await.unwrap_err(),
            OneCliError::NotFound
        ));
    }

    #[tokio::test]
    async fn approve_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/approve"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.approve("apr_1").await.unwrap_err(),
            OneCliError::Unauthorized
        ));
    }

    #[tokio::test]
    async fn approve_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/approve"))
            .respond_with(ResponseTemplate::new(500).set_body_string("x"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.approve("apr_1").await.unwrap_err(),
            OneCliError::Server(_)
        ));
    }

    #[tokio::test]
    async fn approve_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/approve"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "5"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.approve("apr_1").await.unwrap_err(),
            OneCliError::RateLimited {
                retry_after: Some(5)
            }
        ));
    }

    #[tokio::test]
    async fn approve_418_is_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/approve"))
            .respond_with(ResponseTemplate::new(418).set_body_string("tea"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        match c.approve("apr_1").await.unwrap_err() {
            OneCliError::Api { status, message } => {
                assert_eq!(status, 418);
                assert_eq!(message, "tea");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- deny ----

    #[tokio::test]
    async fn deny_happy_with_reason() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/deny"))
            .and(header("authorization", "Bearer test-token"))
            .and(body_json(json!({"reason": "looks unsafe"})))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let c = client_for(&server);
        c.deny("apr_1", Some("looks unsafe")).await.unwrap();
    }

    #[tokio::test]
    async fn deny_happy_without_reason() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/deny"))
            .and(body_json(json!({})))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let c = client_for(&server);
        c.deny("apr_1", None).await.unwrap();
    }

    #[tokio::test]
    async fn deny_404() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/deny"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.deny("apr_1", None).await.unwrap_err(),
            OneCliError::NotFound
        ));
    }

    #[tokio::test]
    async fn deny_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/deny"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.deny("apr_1", None).await.unwrap_err(),
            OneCliError::Unauthorized
        ));
    }

    #[tokio::test]
    async fn deny_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/deny"))
            .respond_with(ResponseTemplate::new(500).set_body_string("x"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.deny("apr_1", None).await.unwrap_err(),
            OneCliError::Server(_)
        ));
    }

    #[tokio::test]
    async fn deny_429_no_header_yields_none_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/deny"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let c = client_for(&server);
        assert!(matches!(
            c.deny("apr_1", None).await.unwrap_err(),
            OneCliError::RateLimited { retry_after: None }
        ));
    }

    #[tokio::test]
    async fn deny_418_is_api() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/approvals/apr_1/deny"))
            .respond_with(ResponseTemplate::new(418).set_body_string("tea"))
            .mount(&server)
            .await;
        let c = client_for(&server);
        match c.deny("apr_1", None).await.unwrap_err() {
            OneCliError::Api { status, message } => {
                assert_eq!(status, 418);
                assert_eq!(message, "tea");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
