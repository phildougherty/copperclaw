//! [`ChannelFactory`] for the Linear adapter.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`LinearConfig`].
//! 2. Construct a [`LinearApi`] client.
//! 3. Spawn the webhook server bound to the configured `host:port`.
//! 4. Return the adapter with the server task handle attached.

use crate::adapter::LinearAdapter;
use crate::api::LinearApi;
use crate::config::LinearConfig;
use crate::events::router::{LinearEventsState, build_events_router};
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use ironclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this channel (`"linear"`).
pub const CHANNEL_TYPE_STR: &str = "linear";

/// Factory for [`LinearAdapter`].
#[derive(Debug, Default)]
pub struct LinearFactory;

impl LinearFactory {
    /// Convenience constructor.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for LinearFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = LinearConfig::from_value(&setup.config)?;
        let api = LinearApi::new(&cfg.api_base, &cfg.api_key);
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = LinearEventsState::new(
            cfg.webhook_secret.clone(),
            setup.inbound_tx,
            cfg.bot_user_id.clone(),
            cfg.bot_username.clone(),
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
                tracing::warn!(error=%err, "linear webhook server exited");
            }
        });
        let adapter = LinearAdapter::new(ct, api);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
///
/// Follows the same `register(&mut reg)` pattern used by the other channels.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(LinearFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn config_value(port: u16) -> serde_json::Value {
        json!({
            "api_key": "lin_api_init",
            "webhook_secret": "ws",
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/linear/webhook"},
            "api_base": "https://example.test/graphql"
        })
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = LinearFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_container_contribution_is_empty() {
        let f = LinearFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = LinearFactory;
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_binds_server_and_returns_adapter() {
        let f = LinearFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let f = LinearFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let bad_cfg = json!({
            "api_key": "k",
            "webhook_secret": "s",
            "webhook": {"host": "not a host", "port": 9, "path": "/x"}
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
        let f = LinearFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"api_key": "k"}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok(_)"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_api_key() {
        let f = LinearFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"webhook_secret": "s"}), tx, "/tmp");
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
    }

    #[test]
    fn register_duplicate_errors() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn channel_type_constant_is_linear() {
        assert_eq!(CHANNEL_TYPE_STR, "linear");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = LinearFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("LinearFactory"));
    }

    #[test]
    fn factory_default_constructs() {
        let _ = LinearFactory;
        let _ = LinearFactory::new();
    }
}
