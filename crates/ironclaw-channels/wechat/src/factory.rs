//! [`ChannelFactory`] for the `WeChat` Work channel.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`WeChatConfig`].
//! 2. Build the [`WeChatApi`] REST client.
//! 3. Construct the webhook router and bind it to the configured `host:port`.
//! 4. Return the adapter with the server's `JoinHandle` attached for clean
//!    shutdown.

use crate::adapter::WeChatAdapter;
use crate::api::WeChatApi;
use crate::config::WeChatConfig;
use crate::events::router::{WeChatEventsState, build_events_router};
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use ironclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this crate (`"wechat"`).
pub const CHANNEL_TYPE_STR: &str = "wechat";

/// Factory producing [`WeChatAdapter`] instances.
#[derive(Debug, Default)]
pub struct WeChatFactory;

impl WeChatFactory {
    /// New empty factory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for WeChatFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = WeChatConfig::from_value(&setup.config)?;
        let api = WeChatApi::new(&cfg.api_base, &cfg.corp_id, &cfg.corp_secret);
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = WeChatEventsState::new(
            cfg.token.clone(),
            cfg.encoding_aes_key.clone(),
            cfg.corp_id.clone(),
            setup.inbound_tx,
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
                tracing::warn!(error=%err, "wechat webhook server exited");
            }
        });
        let adapter = WeChatAdapter::new(ct, api, cfg.agent_id);
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }

    fn container_contribution(&self) -> ContainerContribution {
        // WeChat talks via the host; nothing extra is needed inside the
        // agent container.
        ContainerContribution::default()
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(WeChatFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn good_aes_key() -> String {
        let raw = [3u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        encoded.trim_end_matches('=').to_owned()
    }

    fn pick_free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    fn config_value(port: u16) -> serde_json::Value {
        json!({
            "corp_id":"wx-corp",
            "corp_secret":"secret",
            "agent_id": 1,
            "token":"tok",
            "encoding_aes_key": good_aes_key(),
            "api_base":"https://example.test",
            "webhook": {"host":"127.0.0.1","port":port,"path":"/wechat/webhook"}
        })
    }

    #[tokio::test]
    async fn channel_type_is_wechat() {
        let f = WeChatFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn container_contribution_is_empty() {
        let f = WeChatFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn default_shutdown_is_ok() {
        let f = WeChatFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn init_binds_webhook_server() {
        let f = WeChatFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn init_rejects_bad_bind_host() {
        let f = WeChatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let bad_cfg = json!({
            "corp_id":"c","corp_secret":"s","agent_id":1,"token":"t",
            "encoding_aes_key": good_aes_key(),
            "webhook":{"host":"not a host","port":1,"path":"/x"}
        });
        let setup = ChannelSetup::new(bad_cfg, tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn init_rejects_missing_corp_id() {
        let f = WeChatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(
            json!({"corp_secret":"s","agent_id":1,"token":"t",
                   "encoding_aes_key": good_aes_key()}),
            tx,
            "/tmp",
        );
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn init_rejects_missing_aes_key() {
        let f = WeChatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(
            json!({"corp_id":"c","corp_secret":"s","agent_id":1,"token":"t"}),
            tx,
            "/tmp",
        );
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("encoding_aes_key")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn init_rejects_missing_agent_id() {
        let f = WeChatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(
            json!({"corp_id":"c","corp_secret":"s","token":"t",
                   "encoding_aes_key": good_aes_key()}),
            tx,
            "/tmp",
        );
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn init_rejects_bad_root_config() {
        let f = WeChatFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!("bad"), tx, "/tmp");
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
    fn channel_type_constant_is_wechat() {
        assert_eq!(CHANNEL_TYPE_STR, "wechat");
    }

    #[test]
    fn factory_debug_format_present() {
        let f = WeChatFactory::new();
        assert!(format!("{f:?}").contains("WeChatFactory"));
    }

    #[test]
    fn factory_default_equals_new() {
        let a = WeChatFactory::new();
        let b = WeChatFactory;
        assert_eq!(a.channel_type(), b.channel_type());
    }
}
