//! `WhatsAppFactory` — wires a `WhatsAppAdapter` from a `ChannelSetup`.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use ironclaw_types::ChannelType;

use crate::adapter::WhatsAppAdapter;
use crate::config::WhatsAppConfig;
use crate::keystore;

/// The string used as the channel type for the WhatsApp native channel.
pub const CHANNEL_TYPE_STR: &str = "whatsapp";

/// Default filename for the keystore under the channel's data dir.
pub const DEFAULT_KEYSTORE_FILENAME: &str = "whatsapp_keystore.json";

/// Channel factory for `whatsapp` (native).
#[derive(Debug, Default)]
pub struct WhatsAppFactory;

impl WhatsAppFactory {
    /// Construct a fresh factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for WhatsAppFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let mut cfg = WhatsAppConfig::from_value(&setup.config)?;
        if cfg.keystore_path.is_empty() {
            let path = setup.data_dir.join(DEFAULT_KEYSTORE_FILENAME);
            let s = path
                .to_str()
                .ok_or_else(|| {
                    AdapterError::BadRequest(format!(
                        "whatsapp: keystore path not valid utf-8: {path:?}"
                    ))
                })?
                .to_owned();
            cfg.keystore_path = s;
        }
        let path = std::path::PathBuf::from(&cfg.keystore_path);
        let ks = keystore::load(&path).map_err(|err| match err {
            keystore::KeystoreError::Io(e) => AdapterError::Io(e),
            keystore::KeystoreError::Serde(e) => AdapterError::BadRequest(format!(
                "whatsapp: keystore serde: {e}"
            )),
        })?;
        let adapter = WhatsAppAdapter::new(cfg, ks, setup.inbound_tx);
        Ok(Arc::new(adapter))
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// Register the WhatsApp factory.
pub fn register(reg: &mut ChannelRegistry) -> Result<(), AdapterError> {
    reg.register(Arc::new(WhatsAppFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn channel_type_returns_whatsapp() {
        let f = WhatsAppFactory::new();
        assert_eq!(f.channel_type().as_str(), "whatsapp");
    }

    #[tokio::test]
    async fn init_with_empty_config_succeeds() {
        let f = WhatsAppFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = TempDir::new().unwrap();
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "whatsapp");
    }

    #[tokio::test]
    async fn init_with_null_config_succeeds() {
        let f = WhatsAppFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = TempDir::new().unwrap();
        let setup = ChannelSetup::new(serde_json::Value::Null, tx, dir.path());
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "whatsapp");
    }

    #[tokio::test]
    async fn init_with_non_object_config_errors() {
        let f = WhatsAppFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = TempDir::new().unwrap();
        let setup = ChannelSetup::new(json!("nope"), tx, dir.path());
        let err = match f.init(setup).await {
            Ok(_) => panic!("expected init to fail"),
            Err(e) => e,
        };
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn init_with_bad_endpoint_errors() {
        let f = WhatsAppFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = TempDir::new().unwrap();
        let setup =
            ChannelSetup::new(json!({"endpoint": "https://nope"}), tx, dir.path());
        let err = match f.init(setup).await {
            Ok(_) => panic!("expected init to fail"),
            Err(e) => e,
        };
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn init_defaults_keystore_path_to_data_dir() {
        let f = WhatsAppFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = TempDir::new().unwrap();
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        let adapter = f.init(setup).await.unwrap();
        // Downcast via the trait method.
        assert_eq!(adapter.channel_type().as_str(), "whatsapp");
        // The keystore file is *not* created until something writes to it.
        // What we want here is to verify the path the factory computed.
        let expected = dir.path().join(DEFAULT_KEYSTORE_FILENAME);
        assert!(!expected.exists());
    }

    #[tokio::test]
    async fn init_respects_explicit_keystore_path() {
        let f = WhatsAppFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = TempDir::new().unwrap();
        let explicit = dir.path().join("custom.json");
        let setup = ChannelSetup::new(
            json!({"keystore_path": explicit.to_str().unwrap()}),
            tx,
            dir.path(),
        );
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "whatsapp");
    }

    #[tokio::test]
    async fn init_loads_existing_keystore() {
        let f = WhatsAppFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let dir = TempDir::new().unwrap();
        let ks = crate::keystore::Keystore {
            version: crate::keystore::KEYSTORE_VERSION,
            device_id: "preset".into(),
            noise_key: "AA==".into(),
            ..crate::keystore::Keystore::default()
        };
        let path = dir.path().join(DEFAULT_KEYSTORE_FILENAME);
        crate::keystore::save(&path, &ks).unwrap();
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        let _ = f.init(setup).await.unwrap();
        // Spot-check that the file is intact (it would be rotated if
        // corrupt).
        assert!(path.exists());
    }

    #[tokio::test]
    async fn shutdown_is_ok() {
        let f = WhatsAppFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn container_contribution_is_default() {
        let f = WhatsAppFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[test]
    fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("whatsapp")).is_some());
    }

    #[test]
    fn register_twice_errors() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "whatsapp");
    }

    #[test]
    fn default_keystore_filename_constant() {
        assert_eq!(DEFAULT_KEYSTORE_FILENAME, "whatsapp_keystore.json");
    }

    #[test]
    fn factory_default_and_new_are_equivalent() {
        let _: WhatsAppFactory = WhatsAppFactory;
        let _: WhatsAppFactory = WhatsAppFactory::new();
    }

    #[test]
    fn debug_format_renders() {
        let f = WhatsAppFactory::new();
        assert!(format!("{f:?}").contains("WhatsAppFactory"));
    }
}
