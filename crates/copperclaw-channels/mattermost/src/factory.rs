//! [`ChannelFactory`] for the Mattermost channel.
//!
//! Init sequence:
//! 1. Parse `setup.config` into [`MattermostConfig`].
//! 2. Build a [`MattermostApi`] client targeting the configured server.
//! 3. Bind the outgoing-webhook listener on the configured host/port.
//! 4. Spawn the axum server task; stash the join handle on the adapter
//!    so it's cancelled on drop.

use crate::adapter::MattermostAdapter;
use crate::api::MattermostApi;
use crate::config::{ConfigError, MattermostConfig};
use crate::router::{RouterState, build_router};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use copperclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string for this channel (`"mattermost"`).
pub const CHANNEL_TYPE_STR: &str = "mattermost";

/// Factory for [`MattermostAdapter`].
#[derive(Debug, Default)]
pub struct MattermostFactory;

impl MattermostFactory {
    /// Construct a fresh factory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl From<ConfigError> for AdapterError {
    fn from(value: ConfigError) -> Self {
        AdapterError::BadRequest(value.to_string())
    }
}

#[async_trait]
impl ChannelFactory for MattermostFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = MattermostConfig::from_value(&setup.config)?;
        let addr: SocketAddr = format!("{}:{}", cfg.webhook.host, cfg.webhook.port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                AdapterError::BadRequest(format!("invalid webhook bind address: {e}"))
            })?;
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let api = MattermostApi::new(&cfg.server_url, &cfg.access_token);
        let state = RouterState::new(ct.clone(), cfg.clone(), setup.inbound_tx);
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr().ok();
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                tracing::warn!(error=%err, "mattermost webhook server exited");
            }
        });
        tracing::info!(
            host = %cfg.webhook.host,
            port = bound.map_or(cfg.webhook.port, |a| a.port()),
            path = %cfg.webhook.path,
            server_url = %cfg.server_url,
            "mattermost channel listening"
        );
        let adapter = MattermostAdapter::new(ct, api, cfg.bot_user_id.clone());
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(MattermostFactory::new()))
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

    fn config_value(port: u16) -> serde_json::Value {
        json!({
            "server_url": "https://chat.example.com",
            "access_token": "tok",
            "webhook_token": "wh-secret",
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/mm"},
        })
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = MattermostFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = MattermostFactory;
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_starts_server() {
        let f = MattermostFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let f = MattermostFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(
            json!({
                "server_url": "https://chat.example.com",
                "access_token": "t",
                "webhook_token": "w",
                "webhook": {"host": "not a host", "port": 9},
            }),
            tx,
            "/tmp",
        );
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook bind")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_fields() {
        let f = MattermostFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"server_url": "https://x.example"}), tx, "/tmp");
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
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn channel_type_constant_is_mattermost() {
        assert_eq!(CHANNEL_TYPE_STR, "mattermost");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = MattermostFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("MattermostFactory"));
    }

    #[tokio::test]
    async fn factory_container_contribution_is_empty() {
        let f = MattermostFactory::new();
        assert!(f.container_contribution().is_empty());
    }
}
