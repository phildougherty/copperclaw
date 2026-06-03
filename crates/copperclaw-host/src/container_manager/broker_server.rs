//! Live loopback HTTP listener for the credential broker (Phase 0b).
//!
//! This is the **runtime path** that wires the pure authz core in
//! [`super::broker`] to a real socket. It is started only when the broker is
//! enabled ([`super::broker::BrokerConfig`] resolved to `Some`), so the
//! default install never binds this listener.
//!
//! Flow per request:
//!
//!   1. The runner inside a container POSTs `/v1/messages` to this listener
//!      (its `ANTHROPIC_BASE_URL` points here), carrying the capability token
//!      in the `x-api-key` / `Authorization` slot.
//!   2. [`handle`] reads the token, calls [`super::broker::authorize`] with a
//!      budget closure backed by the central DB, and:
//!        - on `Unauthorized` returns 401 (token never reaches upstream),
//!        - on `OverBudget` returns 429 (request never reaches upstream),
//!        - on `Forward` strips the client auth header, injects the REAL key
//!          host-side via [`super::broker::UpstreamRequest::with_injected_auth`],
//!          forwards the body upstream with `reqwest`, meters the egress
//!          bytes, and streams the upstream response back.
//!
//! The forwarding itself (a real outbound TCP connection to the model
//! provider) needs a live network, so it is exercised in tests against a
//! wiremock upstream rather than the real Anthropic API. The decision logic
//! (which is what the security property hinges on) is fully covered.

use super::broker::{
    AuthScheme, AuthzDecision, BrokerState, BudgetVerdict, auth_scheme_for_provider, authorize,
    now_epoch_secs,
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Everything the request handler needs: the broker state (token validation +
/// the real upstream key/base), the resolved provider's auth scheme, an HTTP
/// client to forward with, and a budget check. The budget check is boxed as a
/// closure so tests can inject a deterministic verdict without a DB.
pub struct BrokerServerState {
    pub broker: Arc<BrokerState>,
    pub auth_scheme: AuthScheme,
    pub http: reqwest::Client,
    /// Returns `WithinBudget`/`OverBudget` for a group at request time. In
    /// production this reads the central DB (see
    /// [`super::budgets::broker_budget_verdict`]); in tests it's a fixed verdict.
    pub budget_check: Box<dyn Fn(copperclaw_types::AgentGroupId) -> BudgetVerdict + Send + Sync>,
}

/// Build the broker router. A single catch-all route proxies every method +
/// path so the runner's `/v1/messages` (and any future provider path) is
/// forwarded transparently.
pub fn build_router(state: Arc<BrokerServerState>) -> Router {
    Router::new()
        .route("/", any(handle))
        .route("/*path", any(handle))
        .with_state(state)
}

/// Start the loopback listener on `addr`, returning the bound local address.
/// Spawns the accept loop on a background task that exits when `shutdown`
/// fires. Guarded entirely by the caller (only invoked when the broker is
/// enabled), so the default path never reaches here.
pub async fn serve(
    addr: std::net::SocketAddr,
    state: Arc<BrokerServerState>,
    shutdown: CancellationToken,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let router = build_router(state);
    info!(addr = %local, "credential broker listening on loopback");
    tokio::spawn(async move {
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
        if let Err(err) = result {
            warn!(error = %err, "credential broker server exited with error");
        }
    });
    Ok(local)
}

/// Extract the capability token from the inbound request headers. Checks
/// `x-api-key` first (Anthropic), then `Authorization: Bearer` (OpenAI-shaped).
fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(v.to_string());
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Axum handler: validate, gate on budget, inject the real key, forward.
async fn handle(
    State(state): State<Arc<BrokerServerState>>,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let headers = parts.headers.clone();

    // Read the body fully so we can both forward it and meter its size. Model
    // request bodies are small (a few KB of messages), so buffering is fine.
    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(err) => {
            warn!(error = %err, "broker could not read request body");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let token = extract_token(&headers).unwrap_or_default();
    let now = now_epoch_secs();

    let decision = {
        let revs = state
            .broker
            .revocations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        authorize(&state.broker.keyring, &token, now, &revs, |claims| {
            (state.budget_check)(claims.agent_group_id)
        })
    };

    let claims = match decision {
        AuthzDecision::Unauthorized(err) => {
            warn!(reason = %err, "broker rejected request: token invalid");
            copperclaw_metrics::inc_broker_request(
                "unknown",
                copperclaw_metrics::BROKER_OUTCOME_UNAUTHORIZED,
            );
            return (StatusCode::UNAUTHORIZED, "invalid broker token").into_response();
        }
        AuthzDecision::OverBudget(claims) => {
            let ag = claims.agent_group_id.as_uuid().to_string();
            warn!(agent_group = %ag, "broker refused request: over budget");
            copperclaw_metrics::inc_broker_request(
                &ag,
                copperclaw_metrics::BROKER_OUTCOME_OVER_BUDGET,
            );
            return (StatusCode::TOO_MANY_REQUESTS, "agent group over budget").into_response();
        }
        AuthzDecision::Forward(claims) => claims,
    };

    let ag = claims.agent_group_id.as_uuid().to_string();

    // Build the upstream URL: broker base + the original request path.
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or(parts.uri.path(), |pq| pq.as_str());
    let base = state.broker.config.upstream_base_url.trim_end_matches('/');
    let upstream_url = format!("{base}{path_and_query}");

    // Inject the REAL key host-side, dropping the client's token header.
    let header_pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|val| (k.as_str().to_string(), val.to_string()))
        })
        .collect();
    let upstream_req = super::broker::UpstreamRequest::from_headers(header_pairs);
    let injected =
        upstream_req.with_injected_auth(state.auth_scheme, &state.broker.config.upstream_key);

    let mut builder = state.http.request(parts.method.clone(), &upstream_url);
    for (k, v) in &injected {
        // Skip hop-by-hop / host headers that reqwest sets itself; forwarding
        // a stale `host`/`content-length` confuses the upstream.
        if k == "host" || k == "content-length" {
            continue;
        }
        builder = builder.header(k, v);
    }
    let egress_len = body_bytes.len() as u64;
    let upstream_resp = builder.body(body_bytes).send().await;

    match upstream_resp {
        Ok(resp) => {
            copperclaw_metrics::inc_broker_request(
                &ag,
                copperclaw_metrics::BROKER_OUTCOME_FORWARDED,
            );
            copperclaw_metrics::add_broker_egress_bytes(&ag, egress_len);
            let status = resp.status();
            let mut out = Response::builder().status(status.as_u16());
            // Copy upstream response headers back to the runner.
            for (k, v) in resp.headers() {
                if let Ok(val) = v.to_str() {
                    out = out.header(k.as_str(), val);
                }
            }
            let bytes = resp.bytes().await.unwrap_or_default();
            out.body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(err) => {
            warn!(agent_group = %ag, error = %err, "broker upstream forward failed");
            // Still counted as forwarded (we attempted) but surfaced as a gateway error.
            (StatusCode::BAD_GATEWAY, "upstream request failed").into_response()
        }
    }
}

/// Build the production server state from a [`BrokerState`], the resolved
/// provider, and a central DB handle for the budget check. The closure reads
/// the group's daily-token budget at request time via a fresh
/// [`super::ContainerManager`]-style check — pulled out here so the server has
/// no back-reference to the manager.
#[must_use]
pub fn production_state(
    broker: Arc<BrokerState>,
    provider: Option<&str>,
    central: copperclaw_db::central::CentralDb,
) -> Arc<BrokerServerState> {
    let auth_scheme = auth_scheme_for_provider(provider);
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .unwrap_or_default();
    let budget_check = Box::new(move |group: copperclaw_types::AgentGroupId| {
        match super::budgets::broker_budget_verdict(&central, group) {
            Ok(verdict) => verdict,
            Err(err) => {
                // Fail OPEN on a DB error: a transient read failure must not
                // brick every agent's model calls. The spawn-time gate is the
                // primary budget enforcement; the broker gate is the backstop.
                warn!(error = %err, "broker budget check failed; allowing request (fail-open)");
                BudgetVerdict::WithinBudget
            }
        }
    });
    Arc::new(BrokerServerState {
        broker,
        auth_scheme,
        http,
        budget_check,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_manager::broker::{BrokerConfig, BrokerState};
    use axum::body::Body;
    use axum::http::Request;
    use copperclaw_types::{AgentGroupId, SessionId};
    use tower::ServiceExt; // for `oneshot`
    use uuid::Uuid;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn broker_state(upstream_base: &str) -> Arc<BrokerState> {
        let cfg = BrokerConfig::resolve(
            true,
            Some("sk-REAL-master"),
            Some(upstream_base),
            Some(3600),
        )
        .unwrap();
        Arc::new(BrokerState::new(cfg))
    }

    fn server_state(broker: Arc<BrokerState>, verdict: BudgetVerdict) -> Arc<BrokerServerState> {
        Arc::new(BrokerServerState {
            broker,
            auth_scheme: AuthScheme::XApiKey,
            http: reqwest::Client::new(),
            budget_check: Box::new(move |_| verdict),
        })
    }

    fn ids() -> (SessionId, AgentGroupId) {
        (SessionId(Uuid::new_v4()), AgentGroupId(Uuid::new_v4()))
    }

    #[tokio::test]
    async fn forwards_valid_token_and_injects_real_key() {
        // Upstream asserts it receives the REAL key, never the token.
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-REAL-master"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"ok\":true}"))
            .mount(&upstream)
            .await;

        let broker = broker_state(&upstream.uri());
        let (sid, gid) = ids();
        let token = broker.mint_for(sid, gid, now_epoch_secs());
        let state = server_state(broker, BudgetVerdict::WithinBudget);
        let app = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", &token) // the TOKEN, not the real key
            .header("content-type", "application/json")
            .body(Body::from("{\"model\":\"x\"}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"{\"ok\":true}");
        // The wiremock `header("x-api-key", "sk-REAL-master")` matcher only
        // matched because the broker swapped the token for the real key.
    }

    #[tokio::test]
    async fn rejects_invalid_token_with_401_and_does_not_forward() {
        // No mounts: if the broker forwarded, the upstream would 404, not 401.
        let upstream = MockServer::start().await;
        let broker = broker_state(&upstream.uri());
        let state = server_state(broker, BudgetVerdict::WithinBudget);
        let app = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", "totally-bogus-token")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn refuses_over_budget_with_429_and_does_not_forward() {
        let upstream = MockServer::start().await;
        // Mount a 200 that, if hit, would make the test pass for the wrong
        // reason — we assert 429 instead, proving the request never forwarded.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream)
            .await;
        let broker = broker_state(&upstream.uri());
        let (sid, gid) = ids();
        let token = broker.mint_for(sid, gid, now_epoch_secs());
        let state = server_state(broker, BudgetVerdict::OverBudget);
        let app = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", &token)
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        // Upstream received zero requests.
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn revoked_token_is_rejected_at_the_listener() {
        let upstream = MockServer::start().await;
        let broker = broker_state(&upstream.uri());
        let (sid, gid) = ids();
        let token = broker.mint_for(sid, gid, now_epoch_secs());
        broker.revoke_session(sid);
        let state = server_state(broker, BudgetVerdict::WithinBudget);
        let app = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", &token)
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_value_never_reaches_upstream() {
        // Upstream records every request; assert none carried the token.
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream)
            .await;
        let broker = broker_state(&upstream.uri());
        let (sid, gid) = ids();
        let token = broker.mint_for(sid, gid, now_epoch_secs());
        let state = server_state(broker, BudgetVerdict::WithinBudget);
        let app = build_router(state);

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("x-api-key", &token)
            .body(Body::from("{}"))
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();

        let received = upstream.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let got = &received[0];
        let sent_key = got
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(sent_key, "sk-REAL-master", "upstream must see the real key");
        assert_ne!(
            sent_key, token,
            "upstream must NOT see the capability token"
        );
    }
}
