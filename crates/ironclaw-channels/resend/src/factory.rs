//! [`ChannelFactory`] for the Resend adapter.
//!
//! Build steps:
//! 1. Parse `setup.config` into [`ResendConfig`].
//! 2. Construct a [`ResendApi`] client.
//! 3. Return the adapter. No server is started — Resend is send-only.

use crate::adapter::ResendAdapter;
use crate::api::ResendApi;
use crate::config::ResendConfig;
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use ironclaw_types::ChannelType;
use std::sync::Arc;

/// Channel-type string used by this channel (`"resend"`).
pub const CHANNEL_TYPE_STR: &str = "resend";

/// Factory for [`ResendAdapter`].
#[derive(Debug, Default)]
pub struct ResendFactory;

impl ResendFactory {
    /// Construct a new factory (equivalent to [`ResendFactory::default`]).
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for ResendFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = ResendConfig::from_value(&setup.config)?;
        let api = ResendApi::new(cfg.api_base, cfg.api_key);
        let adapter = ResendAdapter::new(
            ChannelType::new(CHANNEL_TYPE_STR),
            api,
            cfg.from,
            cfg.default_subject,
        );
        Ok(Arc::new(adapter))
    }

    fn container_contribution(&self) -> ContainerContribution {
        // Resend talks to the outside world only via the host — agents need
        // nothing inside the container.
        ContainerContribution::default()
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(ResendFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn full_config() -> serde_json::Value {
        json!({
            "api_key": "re_init",
            "from": "agent@example.test"
        })
    }

    #[test]
    fn channel_type_constant_is_resend() {
        assert_eq!(CHANNEL_TYPE_STR, "resend");
    }

    #[tokio::test]
    async fn factory_reports_channel_type() {
        let f = ResendFactory::new();
        assert_eq!(f.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_default_container_contribution_is_empty() {
        let f = ResendFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f: ResendFactory = <ResendFactory as Default>::default();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_constructs_adapter() {
        let f = ResendFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(full_config(), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), CHANNEL_TYPE_STR);
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_api_key() {
        let f = ResendFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"from": "a@b.test"}), tx, "/tmp");
        let err = f.init(setup).await.err().expect("expected error");
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("api_key")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_missing_from() {
        let f = ResendFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!({"api_key": "x"}), tx, "/tmp");
        let err = f.init(setup).await.err().expect("expected error");
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("from")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_non_object_config() {
        let f = ResendFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup = ChannelSetup::new(json!("not an object"), tx, "/tmp");
        let err = f.init(setup).await.err().expect("expected error");
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn factory_init_does_not_bind_any_server() {
        // Smoke test: two inits back-to-back must both succeed since the
        // factory doesn't try to bind a port.
        let f = ResendFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let setup_a = ChannelSetup::new(full_config(), tx.clone(), "/tmp");
        let setup_b = ChannelSetup::new(full_config(), tx, "/tmp");
        let a = f.init(setup_a).await.unwrap();
        let b = f.init(setup_b).await.unwrap();
        assert_eq!(a.channel_type(), b.channel_type());
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
    fn factory_default_impl_works() {
        let _f: ResendFactory = <ResendFactory as Default>::default();
    }

    #[test]
    fn factory_debug_format_present() {
        let f = ResendFactory::new();
        let s = format!("{f:?}");
        assert!(s.contains("ResendFactory"));
    }
}
