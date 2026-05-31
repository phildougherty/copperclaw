//! [`EmacsFactory`] — registers the Emacs channel with the host.

use crate::adapter::{CHANNEL_TYPE_STR, EmacsAdapter};
use crate::client::EmacsClientCli;
use crate::config::EmacsConfig;
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use copperclaw_types::ChannelType;
use std::sync::Arc;

/// Factory for [`EmacsAdapter`].
///
/// Default `init` parses the JSON config, builds the [`EmacsClientCli`]
/// subprocess client, and starts the adapter (which spawns its poll task).
#[derive(Debug, Default)]
pub struct EmacsFactory;

impl EmacsFactory {
    /// Construct an empty factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for EmacsFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = EmacsConfig::from_value(&setup.config)?;
        let client = Arc::new(EmacsClientCli::from_config(&cfg));
        let adapter = EmacsAdapter::start(client, cfg, setup.inbound_tx);
        Ok(adapter as Arc<dyn ChannelAdapter>)
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        // Emacs talks to the host's emacsclient binary; the agent container
        // itself needs nothing.
        ContainerContribution::default()
    }
}

/// Register [`EmacsFactory`] with a [`ChannelRegistry`].
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(EmacsFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::InboundEvent;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn factory_channel_type_is_emacs() {
        let f = EmacsFactory::new();
        assert_eq!(f.channel_type().as_str(), "emacs");
    }

    #[tokio::test]
    async fn factory_container_contribution_is_empty() {
        let f = EmacsFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_default_constructible() {
        // Default impl is derived on a unit struct; this confirms the trait
        // implementation is wired up. clippy's lint that flags Default on
        // unit structs is silenced because we want to exercise that surface.
        #[allow(clippy::default_constructed_unit_structs)]
        let _f: EmacsFactory = <EmacsFactory as Default>::default();
    }

    #[tokio::test]
    async fn factory_init_with_default_config() {
        let f = EmacsFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        // Use a long poll interval + a non-existent binary so the poll task
        // sits idle; we don't actually need it to talk to anyone.
        let setup = ChannelSetup::new(
            json!({
                "client_bin": "/does/not/exist/emacsclient",
                "poll_interval_ms": 60000
            }),
            tx,
            "/tmp",
        );
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "emacs");
    }

    #[tokio::test]
    async fn factory_init_with_null_config_uses_defaults() {
        let f = EmacsFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(Value::Null, tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "emacs");
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_config() {
        let f = EmacsFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({ "unknown": true }), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn factory_shutdown_is_ok() {
        let f = EmacsFactory::new();
        f.shutdown().await.unwrap();
    }

    #[test]
    fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("emacs")).is_some());
    }

    #[test]
    fn duplicate_register_errors() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }
}
