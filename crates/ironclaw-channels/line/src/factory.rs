//! [`ChannelFactory`] for the LINE channel.
//!
//! Init sequence:
//! 1. Parse `setup.config` into [`LineConfig`].
//! 2. Build the [`LineApi`] egress client.
//! 3. Construct a shared [`ReplyTokenCache`] and hand a clone to both
//!    the webhook router and the adapter so reply tokens cached during
//!    ingress are visible to subsequent egress calls.
//! 4. Bind the webhook listener and spawn the axum task.

use crate::adapter::{LineAdapter, ReplyTokenCache};
use crate::api::LineApi;
use crate::config::{ConfigError, LineConfig};
use crate::router::{build_router, RouterState};
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use ironclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string for this channel (`"line"`).
pub const CHANNEL_TYPE_STR: &str = "line";

/// Factory for [`LineAdapter`].
#[derive(Debug, Default)]
pub struct LineFactory;

impl LineFactory {
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
impl ChannelFactory for LineFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = LineConfig::from_value(&setup.config)?;
        let addr: SocketAddr = format!("{}:{}", cfg.webhook.host, cfg.webhook.port)
            .parse()
            .map_err(|e: std::net::AddrParseError| {
                AdapterError::BadRequest(format!("invalid webhook bind address: {e}"))
            })?;
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let api = LineApi::new(&cfg.api_base, &cfg.channel_access_token);
        let reply_tokens = Arc::new(ReplyTokenCache::new());
        let router_state = RouterState::new(
            ct.clone(),
            cfg.clone(),
            setup.inbound_tx,
            reply_tokens.clone(),
        );
        let router = build_router(router_state);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr().ok();
        let server = tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, router).await {
                tracing::warn!(error=%err, "line webhook server exited");
            }
        });
        tracing::info!(
            host = %cfg.webhook.host,
            port = bound.map_or(cfg.webhook.port, |a| a.port()),
            path = %cfg.webhook.path,
            api_base = %cfg.api_base,
            "line channel listening"
        );
        let adapter = LineAdapter::new(ct, api, reply_tokens);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(LineFactory::new()))
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
            "channel_secret": "secret",
            "channel_access_token": "tok",
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/line"},
        })
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = LineFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = LineFactory;
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_starts_server() {
        let f = LineFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let f = LineFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(
            json!({
                "channel_secret": "s",
                "channel_access_token": "t",
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
        let f = LineFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"channel_secret": "only-secret"}), tx, "/tmp");
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
    fn channel_type_constant_is_line() {
        assert_eq!(CHANNEL_TYPE_STR, "line");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = LineFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("LineFactory"));
    }

    #[tokio::test]
    async fn factory_container_contribution_is_empty() {
        let f = LineFactory::new();
        assert!(f.container_contribution().is_empty());
    }
}
