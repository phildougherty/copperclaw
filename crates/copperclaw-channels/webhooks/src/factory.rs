//! [`ChannelFactory`] for the webhooks channel.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`WebhooksConfig`].
//! 2. Build the axum router via [`crate::router::build_router`].
//! 3. Bind a TCP listener on the configured `host:port`. A `port = 0`
//!    falls through to the kernel-assigned port — useful for tests.
//! 4. Spawn the server task and stash the handle on the adapter.
//! 5. Return the adapter.

use crate::adapter::WebhooksAdapter;
use crate::config::{ChannelConfigError, WebhooksConfig};
use crate::router::{WebhooksRouterState, build_router};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use copperclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this channel (`"webhooks"`).
pub const CHANNEL_TYPE_STR: &str = "webhooks";

/// Factory for [`WebhooksAdapter`].
#[derive(Debug, Default)]
pub struct WebhooksFactory;

impl WebhooksFactory {
    /// Construct a new factory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl From<ChannelConfigError> for AdapterError {
    fn from(value: ChannelConfigError) -> Self {
        AdapterError::BadRequest(value.to_string())
    }
}

#[async_trait]
impl ChannelFactory for WebhooksFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = WebhooksConfig::from_value(&setup.config)?;
        let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse().map_err(
            |e: std::net::AddrParseError| {
                AdapterError::BadRequest(format!("invalid webhook bind address: {e}"))
            },
        )?;
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = WebhooksRouterState::new(ct.clone(), cfg.clone(), setup.inbound_tx);
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        // Resolve the actual bound port for tracing — `port = 0` becomes
        // a real OS-assigned port after `bind`.
        let bound = listener.local_addr().ok();
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                tracing::warn!(error=%err, "webhooks server exited");
            }
        });
        tracing::info!(
            host = %cfg.host,
            port = bound.map_or(cfg.port, |a| a.port()),
            path = %cfg.path,
            signed = cfg.secret.is_some(),
            "webhooks channel listening"
        );
        let adapter = WebhooksAdapter::new(ct);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(WebhooksFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = WebhooksFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = WebhooksFactory;
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_starts_server_with_minimum_config() {
        let f = WebhooksFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"port": port}), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let f = WebhooksFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"host": "not a host", "port": 9}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook bind")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_config_json() {
        let f = WebhooksFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"path": "no-leading-slash"}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[test]
    fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new(CHANNEL_TYPE_STR)).is_some());
        // Duplicate registration is an error.
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn channel_type_constant_is_webhooks() {
        assert_eq!(CHANNEL_TYPE_STR, "webhooks");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = WebhooksFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("WebhooksFactory"));
    }

    #[tokio::test]
    async fn factory_container_contribution_is_empty() {
        let f = WebhooksFactory::new();
        assert!(f.container_contribution().is_empty());
    }
}
