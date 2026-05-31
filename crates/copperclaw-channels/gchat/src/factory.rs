//! [`ChannelFactory`] for the Google Chat adapter.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`crate::GchatConfig`].
//! 2. Construct a [`crate::GchatApi`] client.
//! 3. Spawn the events webhook server bound to the configured `host:port`.
//! 4. Return the adapter with the server task handle attached.

use crate::adapter::GchatAdapter;
use crate::api::GchatApi;
use crate::config::GchatConfig;
use crate::events::router::{build_events_router, GchatEventsState};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use copperclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this channel (`"gchat"`).
pub const CHANNEL_TYPE_STR: &str = "gchat";

/// Factory for [`GchatAdapter`].
#[derive(Debug, Default)]
pub struct GchatFactory;

impl GchatFactory {
    /// Convenience constructor.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for GchatFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = GchatConfig::from_value(&setup.config)?;
        let api = GchatApi::new(&cfg.api_base, &cfg.bot_token);
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = GchatEventsState::new(
            cfg.client_token.clone(),
            setup.inbound_tx,
            cfg.bot_user_id.clone(),
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
                tracing::warn!(error=%err, "gchat events server exited");
            }
        });
        let adapter = GchatAdapter::new(ct, api);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
///
/// Follows the same `register(&mut reg)` pattern used by the other channels.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(GchatFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn config_value(port: u16) -> serde_json::Value {
        json!({
            "bot_token": "tok",
            "client_token": "shared",
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/gchat/webhook"},
            "api_base": "https://example.test"
        })
    }

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = GchatFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = GchatFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_binds_server() {
        let f = GchatFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_required_field() {
        let f = GchatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"bot_token": "x"}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("client_token")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let f = GchatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let bad_cfg = json!({
            "bot_token": "x",
            "client_token": "y",
            "webhook": {"host": "not a host", "port": 9, "path": "/x"}
        });
        let setup = ChannelSetup::new(bad_cfg, tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_propagates_bind_failure() {
        // Bind a port first, then try to bind it again -> EADDRINUSE -> Io error.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let f = GchatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let res = f.init(setup).await;
        // Either Io (address already in use) is fine; just assert it's an error.
        assert!(res.is_err(), "expected error, got Ok");
        drop(listener);
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
    fn channel_type_constant_is_gchat() {
        assert_eq!(CHANNEL_TYPE_STR, "gchat");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = GchatFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("GchatFactory"));
    }

    #[test]
    fn factory_container_contribution_is_empty() {
        let f = GchatFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[test]
    fn factory_default_equivalent_to_new() {
        let f = GchatFactory;
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }
}
