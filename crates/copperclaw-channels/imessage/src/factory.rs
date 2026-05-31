//! [`IMessageFactory`] — registers the iMessage channel with the host.

use crate::adapter::IMessageAdapter;
use crate::bridge::IMessageBridge;
use crate::bridge_osascript::OsaScriptBridge;
use crate::config::IMessageConfig;
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use copperclaw_types::ChannelType;
use std::sync::Arc;

/// `ChannelType` string used by this channel.
pub const CHANNEL_TYPE_STR: &str = "imessage";

/// Factory for [`IMessageAdapter`].
///
/// Default `init` parses the JSON config, builds an [`OsaScriptBridge`]
/// (the real `osascript` + `sqlite3` subprocess client), and starts the
/// adapter (which spawns its poll task unless polling is disabled).
#[derive(Debug, Default)]
pub struct IMessageFactory;

impl IMessageFactory {
    /// Construct a fresh factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for IMessageFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = IMessageConfig::from_value(&setup.config)?;
        let bridge: Arc<dyn IMessageBridge> = Arc::new(OsaScriptBridge::from_config(&cfg));
        let adapter = IMessageAdapter::start_with_bridge(
            bridge,
            cfg,
            setup.inbound_tx,
            setup.data_dir,
        );
        Ok(adapter as Arc<dyn ChannelAdapter>)
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        // iMessage runs on the host; the agent container needs nothing.
        ContainerContribution::default()
    }
}

/// Register [`IMessageFactory`] with the supplied registry.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(IMessageFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::InboundEvent;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn factory_channel_type_is_imessage() {
        let f = IMessageFactory::new();
        assert_eq!(f.channel_type().as_str(), "imessage");
    }

    #[tokio::test]
    async fn factory_container_contribution_is_empty() {
        let f = IMessageFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_shutdown_is_ok() {
        let f = IMessageFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn factory_default_constructible() {
        #[allow(clippy::default_constructed_unit_structs)]
        let _f: IMessageFactory = <IMessageFactory as Default>::default();
    }

    #[tokio::test]
    async fn factory_init_with_default_config_returns_adapter() {
        let f = IMessageFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let dir = TempDir::new().unwrap();
        // Long poll interval + polling off so the test doesn't hammer the
        // (nonexistent) sqlite3 binary on a Linux runner.
        let setup = ChannelSetup::new(
            json!({
                "poll_interval_ms": 60000,
                "enable_polling": false,
                "since_rowid_file": "rowid.txt"
            }),
            tx,
            dir.path(),
        );
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "imessage");
    }

    #[tokio::test]
    async fn factory_init_with_null_config_succeeds() {
        let f = IMessageFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let dir = TempDir::new().unwrap();
        let setup = ChannelSetup::new(Value::Null, tx, dir.path());
        // Defaults turn polling on; the real bridge will fail at spawn,
        // which the poll loop logs and rides out. We just want init() to
        // succeed and return the adapter.
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "imessage");
    }

    #[tokio::test]
    async fn factory_init_rejects_bad_config() {
        let f = IMessageFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let dir = TempDir::new().unwrap();
        let setup = ChannelSetup::new(json!({ "unknown": true }), tx, dir.path());
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected BadRequest, got Ok"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_string_config() {
        let f = IMessageFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let dir = TempDir::new().unwrap();
        let setup = ChannelSetup::new(json!("nope"), tx, dir.path());
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
        assert!(reg.get(&ChannelType::new("imessage")).is_some());
    }

    #[test]
    fn duplicate_register_returns_bad_request() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "imessage");
    }

    #[test]
    fn factory_debug_renders_name() {
        let f = IMessageFactory::new();
        assert!(format!("{f:?}").contains("IMessageFactory"));
    }
}
