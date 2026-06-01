//! Axum router that accepts inbound webhook POSTs and emits
//! [`InboundEvent`]s.
//!
//! Routing rule: a POST to `<base_path>/<suffix>` becomes an event with
//! `platform_id = "<suffix>"`. A POST to the bare `<base_path>` (no
//! suffix) becomes `platform_id = "default"`. Sub-segments are joined
//! with `/` so `<base>/stripe/invoices` yields `platform_id =
//! "stripe/invoices"` — that lets users wire one copperclaw install to
//! many sources by namespacing the URL.
//!
//! The body MUST be JSON: copperclaw is structured-message-first, and
//! every realistic webhook source (GitHub, Stripe, Shopify, Sentry,
//! Grafana, Vercel, …) sends JSON. Non-JSON bodies are rejected with
//! 415; this is loud-by-design — quietly accepting form-encoded or
//! binary bodies would force every downstream consumer to write
//! decoders defensively.
//!
//! See [`crate::signature`] for the HMAC verification path.

use crate::config::WebhooksConfig;
use crate::signature::{SignatureOutcome, verify};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use chrono::Utc;
use copperclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

/// Shared state plumbed through every axum handler.
#[derive(Clone)]
pub struct WebhooksRouterState {
    pub channel_type: ChannelType,
    pub config: WebhooksConfig,
    pub inbound_tx: Sender<InboundEvent>,
}

impl WebhooksRouterState {
    /// Build a state struct.
    #[must_use]
    pub fn new(
        channel_type: ChannelType,
        config: WebhooksConfig,
        inbound_tx: Sender<InboundEvent>,
    ) -> Self {
        Self {
            channel_type,
            config,
            inbound_tx,
        }
    }
}

/// Build an axum router that mounts the webhook accept endpoint at the
/// configured base path with both an exact match (no suffix) and a
/// wildcard child (`<base>/*rest`). axum 0.7 uses matchit's `*name`
/// syntax for catch-all segments, not the `{*name}` form 0.8 ships.
pub fn build_router(state: WebhooksRouterState) -> Router {
    let base = state.config.path.clone();
    let child_path = if base == "/" {
        "/*rest".to_string()
    } else {
        format!("{base}/*rest")
    };
    Router::new()
        .route(&base, post(handle))
        .route(&child_path, post(handle_with_rest))
        .with_state(state)
}

/// Handler for the exact base path (no suffix).
async fn handle(
    State(state): State<WebhooksRouterState>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    process(&state, "default".to_string(), &headers, body).await
}

/// Handler for a path with a wildcard suffix.
async fn handle_with_rest(
    State(state): State<WebhooksRouterState>,
    AxumPath(rest): AxumPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    let platform_id = if rest.is_empty() {
        "default".to_string()
    } else {
        rest
    };
    process(&state, platform_id, &headers, body).await
}

/// Shared body of both handlers. Returns the HTTP status + a short
/// reason string the caller can include in the response body for
/// diagnosability.
async fn process(
    state: &WebhooksRouterState,
    platform_id: String,
    headers: &HeaderMap,
    body: Bytes,
) -> (StatusCode, &'static str) {
    if let Some(secret) = state.config.secret.as_deref() {
        let header_value = headers
            .get(&state.config.signature_header)
            .and_then(|v| v.to_str().ok());
        match verify(&body, secret, &state.config.signature_prefix, header_value) {
            SignatureOutcome::Ok => {}
            SignatureOutcome::HeaderMissing => {
                return (StatusCode::UNAUTHORIZED, "signature header missing");
            }
            SignatureOutcome::Mismatch(reason) => {
                tracing::warn!(reason, "webhook signature rejected");
                return (StatusCode::UNAUTHORIZED, "signature invalid");
            }
        }
    }

    let content: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, "webhook body is not JSON");
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "body must be JSON");
        }
    };

    let event = InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id,
        thread_id: None,
        message: InboundMessage {
            id: Uuid::new_v4().to_string(),
            kind: MessageKind::Webhook,
            content,
            timestamp: Utc::now(),
            is_mention: None,
            is_group: None,
        },
        reply_to: None,
        sender: None,
    };

    if state.inbound_tx.send(event).await.is_err() {
        tracing::warn!("inbound channel closed; dropping webhook event");
        return (StatusCode::SERVICE_UNAVAILABLE, "host shutting down");
    }
    (StatusCode::OK, "ok")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::compute_hex;
    use axum::body::Body;
    use axum::http::Request;
    use tokio::sync::mpsc;
    use tower::ServiceExt as _;

    fn cfg(secret: Option<&str>, path: &str) -> WebhooksConfig {
        WebhooksConfig {
            host: "127.0.0.1".into(),
            port: 0,
            path: path.into(),
            secret: secret.map(str::to_string),
            signature_header: "X-Webhook-Signature".into(),
            signature_prefix: String::new(),
        }
    }

    async fn post_json(
        router: Router,
        path: &str,
        sig: Option<&str>,
        body: &[u8],
    ) -> axum::http::Response<axum::body::Body> {
        let mut req = Request::builder().method("POST").uri(path);
        if let Some(s) = sig {
            req = req.header("X-Webhook-Signature", s);
        }
        let req = req
            .header("content-type", "application/json")
            .body(Body::from(body.to_vec()))
            .unwrap();
        router.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn unsigned_post_is_accepted_when_no_secret_configured() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(ChannelType::new("webhooks"), cfg(None, "/hooks"), tx);
        let router = build_router(state);
        let resp = post_json(router, "/hooks", None, br#"{"hello":"world"}"#).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.platform_id, "default");
        assert_eq!(event.message.content["hello"], "world");
        assert!(matches!(event.message.kind, MessageKind::Webhook));
    }

    #[tokio::test]
    async fn suffix_path_becomes_platform_id() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(ChannelType::new("webhooks"), cfg(None, "/hooks"), tx);
        let router = build_router(state);
        let resp = post_json(router, "/hooks/stripe/invoices", None, b"{}").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.platform_id, "stripe/invoices");
    }

    #[tokio::test]
    async fn signature_required_when_secret_set() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(
            ChannelType::new("webhooks"),
            cfg(Some("topsecret"), "/hooks"),
            tx,
        );
        let router = build_router(state);
        let resp = post_json(router, "/hooks", None, b"{}").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_signature_accepted() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(
            ChannelType::new("webhooks"),
            cfg(Some("topsecret"), "/hooks"),
            tx,
        );
        let router = build_router(state);
        let body = br#"{"x":1}"#;
        let sig = compute_hex("topsecret", body);
        let resp = post_json(router, "/hooks", Some(&sig), body).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn bad_signature_rejected() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(
            ChannelType::new("webhooks"),
            cfg(Some("topsecret"), "/hooks"),
            tx,
        );
        let router = build_router(state);
        let body = br#"{"x":1}"#;
        let wrong_sig = compute_hex("wrong-key", body);
        let resp = post_json(router, "/hooks", Some(&wrong_sig), body).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_json_rejected_with_415() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(ChannelType::new("webhooks"), cfg(None, "/hooks"), tx);
        let router = build_router(state);
        let resp = post_json(router, "/hooks", None, b"not json").await;
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn closed_inbound_returns_503() {
        let (tx, rx) = mpsc::channel::<InboundEvent>(8);
        drop(rx);
        let state = WebhooksRouterState::new(ChannelType::new("webhooks"), cfg(None, "/hooks"), tx);
        let router = build_router(state);
        let resp = post_json(router, "/hooks", None, b"{}").await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn root_path_works() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(ChannelType::new("webhooks"), cfg(None, "/"), tx);
        let router = build_router(state);
        let resp = post_json(router, "/", None, b"{}").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.platform_id, "default");
    }

    #[tokio::test]
    async fn root_path_suffix_works() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let state = WebhooksRouterState::new(ChannelType::new("webhooks"), cfg(None, "/"), tx);
        let router = build_router(state);
        let resp = post_json(router, "/grafana", None, b"{}").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.platform_id, "grafana");
    }
}
