//! [`ChannelFactory`] for the GitHub adapter.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`GithubConfig`].
//! 2. Construct a [`GithubApi`] client.
//! 3. Spawn the webhook server bound to the configured `host:port`.
//! 4. Return the adapter, with the server task handle attached.

use crate::adapter::GithubAdapter;
use crate::api::GithubApi;
use crate::config::GithubConfig;
use crate::events::router::{GithubEventsState, build_events_router};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use copperclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this channel (`"github"`).
pub const CHANNEL_TYPE_STR: &str = "github";

/// Factory for [`GithubAdapter`].
#[derive(Debug, Default)]
pub struct GithubFactory;

impl GithubFactory {
    /// Construct a new factory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for GithubFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = GithubConfig::from_value(&setup.config)?;
        let api = GithubApi::new(&cfg.api_base, &cfg.token);
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = GithubEventsState::new(
            cfg.webhook_secret.clone(),
            setup.inbound_tx,
            cfg.bot_login.clone(),
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
                tracing::warn!(error=%err, "github webhook server exited");
            }
        });
        let adapter = GithubAdapter::new(ct, api);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
///
/// Follows the same `register(&mut reg)` pattern used by the other channels.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(GithubFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn config_value(port: u16) -> serde_json::Value {
        json!({
            "token": "ghp-init",
            "webhook_secret": "secret",
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/github/webhook"},
            "api_base": "https://api.test/example"
        })
    }

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = GithubFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = GithubFactory;
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_starts_server() {
        let f = GithubFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let f = GithubFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let bad_cfg = json!({
            "token":"x",
            "webhook_secret":"s",
            "webhook":{"host":"not a host", "port": 9, "path":"/x"}
        });
        let setup = ChannelSetup::new(bad_cfg, tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_secret() {
        let f = GithubFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"token":"x"}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_token() {
        let f = GithubFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"webhook_secret":"s"}), tx, "/tmp");
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
    fn channel_type_constant_is_github() {
        assert_eq!(CHANNEL_TYPE_STR, "github");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = GithubFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("GithubFactory"));
    }

    #[test]
    fn factory_default_constructs() {
        // Cover both no-arg construction patterns to ensure Default is wired.
        let f1 = GithubFactory;
        let f2 = GithubFactory::new();
        assert_eq!(f1.channel_type(), f2.channel_type());
    }

    #[tokio::test]
    async fn factory_container_contribution_is_empty() {
        let f = GithubFactory::new();
        assert!(f.container_contribution().is_empty());
    }
}
