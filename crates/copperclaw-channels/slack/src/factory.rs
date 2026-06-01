//! [`ChannelFactory`] for the Slack adapter.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`SlackConfig`].
//! 2. Construct a [`SlackApi`] client.
//! 3. Resolve the bot user id with `auth.test` (best effort — on failure we
//!    leave it unset and still start; mention detection from text falls back
//!    to disabled, but `app_mention` events keep working).
//! 4. Spawn the Events API server bound to the configured `host:port`.
//! 5. Return the adapter, with the server task handle attached.

use crate::adapter::SlackAdapter;
use crate::api::SlackApi;
use crate::config::SlackConfig;
use crate::events::router::{SlackEventsState, build_events_router};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use copperclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this channel (`"slack"`).
pub const CHANNEL_TYPE_STR: &str = "slack";

/// Factory for [`SlackAdapter`].
#[derive(Debug, Default)]
pub struct SlackFactory;

impl SlackFactory {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for SlackFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = SlackConfig::from_value(&setup.config)?;
        let api = SlackApi::new(&cfg.api_base, &cfg.bot_token);
        // Best-effort bot id discovery.
        let bot_user_id = match api.auth_test().await {
            Ok(resp) => Some(resp.user_id),
            Err(err) => {
                tracing::warn!(error=%err, "slack auth.test failed at init; mention detection will be limited");
                None
            }
        };
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = SlackEventsState::new(
            cfg.signing_secret.clone(),
            setup.inbound_tx,
            bot_user_id,
            ct.clone(),
        );
        let router = build_events_router(&cfg.webhook.path, state);
        let addr: SocketAddr = format!("{}:{}", cfg.webhook.host, cfg.webhook.port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                AdapterError::BadRequest(format!("invalid webhook bind address: {e}"))
            })?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                tracing::warn!(error=%err, "slack events server exited");
            }
        });
        let adapter = SlackAdapter::new(ct, api);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
///
/// Follows the same `register(&mut reg)` pattern used by the other channels.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(SlackFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_value(server_uri: &str, port: u16) -> serde_json::Value {
        json!({
            "bot_token": "xoxb-init",
            "signing_secret": "secret",
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/slack/events"},
            "api_base": server_uri
        })
    }

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = SlackFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = SlackFactory;
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_starts_server_and_auth_test() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "user_id": "UBOT"})),
            )
            .mount(&mock)
            .await;
        let f = SlackFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(&mock.uri(), port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_tolerates_auth_test_failure() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
            )
            .mount(&mock)
            .await;
        let f = SlackFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(&mock.uri(), port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth.test"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"ok": true, "user_id": "UBOT"})),
            )
            .mount(&mock)
            .await;
        let f = SlackFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let bad_cfg = json!({
            "bot_token": "x",
            "signing_secret": "s",
            "webhook": {"host": "not a host", "port": 9, "path": "/x"},
            "api_base": mock.uri()
        });
        let setup = ChannelSetup::new(bad_cfg, tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok(_)"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_secret() {
        let f = SlackFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"bot_token":"x"}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok(_)"),
        }
    }

    #[test]
    fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new(CHANNEL_TYPE_STR)).is_some());
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn channel_type_constant_is_slack() {
        assert_eq!(CHANNEL_TYPE_STR, "slack");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = SlackFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("SlackFactory"));
    }
}
