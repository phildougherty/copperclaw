//! [`ChannelFactory`] for the `WhatsApp` Cloud channel.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`WhatsappCloudConfig`].
//! 2. Build the [`WhatsappCloudApi`] REST client.
//! 3. Construct the webhook router and bind it to the configured `host:port`.
//! 4. Return the adapter with the server's `JoinHandle` attached for clean
//!    shutdown.

use crate::adapter::WhatsappCloudAdapter;
use crate::api::WhatsappCloudApi;
use crate::config::WhatsappCloudConfig;
use crate::events::router::{WhatsappCloudEventsState, build_events_router};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use copperclaw_types::ChannelType;
use std::net::SocketAddr;
use std::sync::Arc;

/// Channel-type string used by this channel (`"whatsapp-cloud"`).
pub const CHANNEL_TYPE_STR: &str = "whatsapp-cloud";

/// Factory for [`WhatsappCloudAdapter`].
#[derive(Debug, Default)]
pub struct WhatsappCloudFactory;

impl WhatsappCloudFactory {
    /// New empty factory.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for WhatsappCloudFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = WhatsappCloudConfig::from_value(&setup.config)?;
        let api = WhatsappCloudApi::new(&cfg.graph_base, &cfg.access_token);
        let ct = ChannelType::new(CHANNEL_TYPE_STR);
        let state = WhatsappCloudEventsState::new(
            cfg.app_secret.clone(),
            cfg.verify_token.clone(),
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
                tracing::warn!(error=%err, "whatsapp-cloud webhook server exited");
            }
        });
        let adapter = WhatsappCloudAdapter::new(ct, api, cfg.default_phone_number_id.clone());
        adapter.set_server_handle(server);
        Ok(Arc::new(adapter))
    }
}

/// Register this factory with a [`ChannelRegistry`]. Errors if the
/// channel type is already registered.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(WhatsappCloudFactory::new()))
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
            "access_token": "EAAG-init",
            "app_secret": "secret",
            "verify_token": "v-token",
            "graph_base": "https://example.test/v18.0",
            "webhook": {"host": "127.0.0.1", "port": port, "path": "/whatsapp-cloud/webhook"}
        })
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = WhatsappCloudFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = WhatsappCloudFactory;
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_binds_webhook_server() {
        let f = WhatsappCloudFactory::new();
        let port = pick_free_port();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(config_value(port), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_bind_host() {
        let f = WhatsappCloudFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let bad_cfg = json!({
            "access_token":"t","app_secret":"s","verify_token":"v",
            "webhook":{"host":"not a host","port": 1, "path":"/x"}
        });
        let setup = ChannelSetup::new(bad_cfg, tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("invalid webhook")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_access_token() {
        let f = WhatsappCloudFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"app_secret":"s","verify_token":"v"}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_app_secret() {
        let f = WhatsappCloudFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"access_token":"t","verify_token":"v"}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_verify_token() {
        let f = WhatsappCloudFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"access_token":"t","app_secret":"s"}), tx, "/tmp");
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
    fn channel_type_constant_is_whatsapp_cloud() {
        assert_eq!(CHANNEL_TYPE_STR, "whatsapp-cloud");
    }

    #[test]
    fn factory_default_container_contribution_is_empty() {
        let f = WhatsappCloudFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[test]
    fn factory_debug_format_present() {
        let f = WhatsappCloudFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("WhatsappCloudFactory"));
    }
}
