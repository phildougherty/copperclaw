//! [`MatrixFactory`] — the [`ChannelFactory`] producing [`MatrixAdapter`]
//! instances.

use crate::adapter::MatrixAdapter;
use crate::config::MatrixConfig;
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use copperclaw_types::ChannelType;
use std::sync::Arc;

/// `ChannelType` string used by this channel.
pub const CHANNEL_TYPE_STR: &str = "matrix";

/// Factory for [`MatrixAdapter`].
#[derive(Debug, Default)]
pub struct MatrixFactory;

impl MatrixFactory {
    /// Construct a fresh factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for MatrixFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let config = MatrixConfig::from_value(&setup.config)?;
        let adapter = MatrixAdapter::start(config, setup.inbound_tx, setup.data_dir)?;
        Ok(adapter as Arc<dyn ChannelAdapter>)
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        ContainerContribution::default()
    }
}

/// Register the [`MatrixFactory`] with the supplied registry.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(MatrixFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::InboundEvent;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_value(uri: &str) -> serde_json::Value {
        json!({
            "homeserver_url": uri,
            "access_token": "tok",
            "user_id": "@bot:m.org",
            "sync_timeout_ms": 0
        })
    }

    async fn mount_empty_sync(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "n",
                "rooms": { "join": {} }
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn channel_type_is_matrix() {
        let f = MatrixFactory::new();
        assert_eq!(f.channel_type().as_str(), "matrix");
    }

    #[tokio::test]
    async fn container_contribution_is_empty() {
        let f = MatrixFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn shutdown_is_ok() {
        let f = MatrixFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn init_starts_adapter_and_persists_next_batch_file() {
        let s = MockServer::start().await;
        // The /sync endpoint returns a next_batch; the loop should persist it.
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "first-batch",
                "rooms": { "join": {} }
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(4);
        let setup = ChannelSetup::new(config_value(&s.uri()), tx, dir.path());
        let adapter = MatrixFactory::new().init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "matrix");
        // Allow the sync loop to write the token.
        for _ in 0..50 {
            if dir.path().join("next_batch.txt").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(dir.path().join("next_batch.txt").exists());
    }

    #[tokio::test]
    async fn init_resumes_from_persisted_next_batch_file() {
        let s = MockServer::start().await;
        mount_empty_sync(&s).await;
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("next_batch.txt"), "resume-token")
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(4);
        let setup = ChannelSetup::new(config_value(&s.uri()), tx, dir.path());
        let _adapter = MatrixFactory::new().init(setup).await.unwrap();
        // No direct observation of the request since param here; the resume
        // path is exercised in detail in sync.rs unit tests.
    }

    #[tokio::test]
    async fn init_propagates_config_errors() {
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        match MatrixFactory::new().init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("matrix")).is_some());
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn default_factory_equals_new() {
        let a = MatrixFactory::new();
        let b = MatrixFactory;
        assert_eq!(a.channel_type(), b.channel_type());
    }

    #[tokio::test]
    async fn factory_debug_format() {
        let f = MatrixFactory::new();
        assert!(format!("{f:?}").contains("MatrixFactory"));
    }

    #[test]
    fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "matrix");
    }
}
