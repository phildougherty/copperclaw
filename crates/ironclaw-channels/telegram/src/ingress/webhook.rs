//! axum HTTP server that accepts Telegram webhook callbacks.
//!
//! Telegram POSTs an [`Update`] JSON body to a publicly reachable URL
//! configured by `setWebhook`. The adapter exposes a single route at the
//! configured `path`; if `secret_token` is set, the server validates the
//! `X-Telegram-Bot-Api-Secret-Token` header.

use crate::api::TelegramApi;
use crate::config::WebhookConfig;
use crate::ingress::{IngressSettings, updates_to_events};
use crate::types::Update;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use ironclaw_types::InboundEvent;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// Telegram header carrying the configured shared secret.
pub const SECRET_TOKEN_HEADER: &str = "x-telegram-bot-api-secret-token";

#[derive(Clone)]
struct AppState {
    inbound_tx: Sender<InboundEvent>,
    secret_token: Option<String>,
    api: TelegramApi,
    settings: IngressSettings,
}

/// Build the axum [`Router`] used by the webhook server.
///
/// Exposed so tests can hit it without binding a real socket.
pub fn build_router(
    path: &str,
    inbound_tx: Sender<InboundEvent>,
    secret_token: Option<String>,
    api: TelegramApi,
    settings: IngressSettings,
) -> Router {
    let state = AppState {
        inbound_tx,
        secret_token,
        api,
        settings,
    };
    Router::new()
        .route(path, post(handle_update))
        .with_state(Arc::new(state))
}

async fn handle_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(update): Json<Update>,
) -> StatusCode {
    if let Some(expected) = state.secret_token.as_deref() {
        let provided = headers
            .get(SECRET_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok());
        // Constant-time compare so a network attacker can't probe the
        // secret one char at a time via the response-time side channel.
        let ok = match provided {
            Some(p) => {
                use subtle::ConstantTimeEq;
                p.as_bytes().ct_eq(expected.as_bytes()).into()
            }
            None => false,
        };
        if !ok {
            tracing::warn!("telegram webhook rejected: secret token mismatch");
            return StatusCode::UNAUTHORIZED;
        }
    }

    let events = updates_to_events(&update, &state.api, &state.settings).await;
    for event in events {
        if state.inbound_tx.send(event).await.is_err() {
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    }
    StatusCode::OK
}

/// Spawn the axum server. Returns the bound [`SocketAddr`] so callers can
/// observe the actual address (useful when `port = 0`).
///
/// The server stops when `cancel` is triggered.
pub async fn spawn_server(
    cfg: WebhookConfig,
    inbound_tx: Sender<InboundEvent>,
    api: TelegramApi,
    settings: IngressSettings,
    cancel: CancellationToken,
) -> Result<SocketAddr, std::io::Error> {
    let router = build_router(&cfg.path, inbound_tx, cfg.secret_token, api, settings);
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("telegram webhook bind address invalid: {e}"),
        )
    })?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let shutdown = async move { cancel.cancelled().await };

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::warn!(error = %err, "telegram webhook server exited");
        }
    });

    Ok(local_addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_MAX_ATTACHMENT_BYTES;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path as wpath};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn update_body() -> serde_json::Value {
        json!({
            "update_id": 9,
            "message": {
                "message_id": 1,
                "date": 1,
                "chat": { "id": 100, "type": "private" },
                "text": "hello webhook"
            }
        })
    }

    fn make_settings(dir: &Path) -> IngressSettings {
        IngressSettings {
            attachment_download: true,
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            bot_username: None,
            data_dir: dir.to_path_buf(),
        }
    }

    async fn dummy_api() -> (TelegramApi, MockServer) {
        let s = MockServer::start().await;
        let api = TelegramApi::new(s.uri(), "tok");
        (api, s)
    }

    #[tokio::test]
    async fn post_update_produces_inbound_event() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(1);
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let router = build_router("/hook", tx, None, api, make_settings(dir.path()));
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&update_body()).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let evt = rx.recv().await.expect("event");
        assert_eq!(evt.platform_id, "100");
        assert_eq!(evt.message.content["text"], "hello webhook");
    }

    #[tokio::test]
    async fn post_with_correct_secret_token_passes() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(1);
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let router = build_router(
            "/hook",
            tx,
            Some("shh".into()),
            api,
            make_settings(dir.path()),
        );
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .header(SECRET_TOKEN_HEADER, "shh")
            .body(axum::body::Body::from(serde_json::to_vec(&update_body()).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = rx.recv().await.expect("event");
    }

    #[tokio::test]
    async fn post_with_missing_secret_token_rejected() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(1);
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let router = build_router(
            "/hook",
            tx,
            Some("shh".into()),
            api,
            make_settings(dir.path()),
        );
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&update_body()).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // No event delivered.
        let timed = tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await;
        assert!(timed.is_err() || timed.unwrap().is_none());
    }

    #[tokio::test]
    async fn post_with_wrong_secret_token_rejected() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let router = build_router(
            "/hook",
            tx,
            Some("shh".into()),
            api,
            make_settings(dir.path()),
        );
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .header(SECRET_TOKEN_HEADER, "wrong")
            .body(axum::body::Body::from(serde_json::to_vec(&update_body()).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn post_returns_503_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel::<InboundEvent>(1);
        drop(rx);
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let router = build_router("/hook", tx, None, api, make_settings(dir.path()));
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&update_body()).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn post_with_empty_update_returns_ok_no_event() {
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(1);
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let router = build_router("/hook", tx, None, api, make_settings(dir.path()));
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&json!({"update_id": 7})).unwrap(),
            ))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let timed = tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await;
        assert!(timed.is_err() || timed.unwrap().is_none());
    }

    #[tokio::test]
    async fn malformed_body_returns_4xx() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let router = build_router("/hook", tx, None, api, make_settings(dir.path()));
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("not json".as_bytes().to_vec()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn spawn_server_binds_and_shuts_down_cleanly() {
        let cfg = WebhookConfig {
            host: "127.0.0.1".into(),
            port: 0,
            path: "/hook".into(),
            secret_token: None,
        };
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let addr = spawn_server(cfg, tx, api, make_settings(dir.path()), cancel.clone())
            .await
            .unwrap();
        assert!(addr.port() > 0);

        // Hit the endpoint to be sure it's actually running.
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/hook");
        let resp = client.post(&url).json(&update_body()).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        cancel.cancel();
        // Allow background task to wind down.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn spawn_server_invalid_bind_addr_errors() {
        let cfg = WebhookConfig {
            host: "::not-an-ip::".into(),
            port: 0,
            path: "/hook".into(),
            secret_token: None,
        };
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let (api, _s) = dummy_api().await;
        let dir = TempDir::new().unwrap();
        let res = spawn_server(cfg, tx, api, make_settings(dir.path()), cancel).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn webhook_downloads_attachment_when_enabled() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wpath("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "file_id": "F", "file_unique_id": "U", "file_size": 5, "file_path": "documents/a.txt" }
            })))
            .mount(&upstream)
            .await;
        Mock::given(method("GET"))
            .and(wpath("/file/bottok/documents/a.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&upstream)
            .await;

        let (tx, mut rx) = mpsc::channel::<InboundEvent>(1);
        let api = TelegramApi::new(upstream.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let router = build_router("/hook", tx, None, api, make_settings(dir.path()));
        let body = json!({
            "update_id": 1,
            "message": {
                "message_id": 9, "date": 1,
                "chat": { "id": 5, "type": "private" },
                "document": {
                    "file_id": "F", "file_unique_id": "U",
                    "file_name": "a.txt", "mime_type": "text/plain", "file_size": 5
                }
            }
        });
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/hook")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.expect("event");
        assert_eq!(evt.message.kind, ironclaw_types::MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["kind"], "telegram.document");
    }

    #[tokio::test]
    async fn secret_token_header_constant() {
        assert_eq!(SECRET_TOKEN_HEADER, "x-telegram-bot-api-secret-token");
    }
}
