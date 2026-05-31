//! [`XFactory`] — the [`ChannelFactory`] producing [`XAdapter`] instances.

use crate::adapter::XAdapter;
use crate::config::XConfig;
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use copperclaw_types::ChannelType;
use std::sync::Arc;

/// `ChannelType` string used by this channel.
pub const CHANNEL_TYPE_STR: &str = "x";

/// Factory for [`XAdapter`].
#[derive(Debug, Default)]
pub struct XFactory;

impl XFactory {
    /// Construct a fresh factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for XFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let config = XConfig::from_value(&setup.config)?;
        let adapter = XAdapter::start(config, setup.inbound_tx, setup.data_dir)?;
        Ok(adapter as Arc<dyn ChannelAdapter>)
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        ContainerContribution::default()
    }
}

/// Register the [`XFactory`] with the supplied registry.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(XFactory::new()))
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

    fn config_value(api_url: &str) -> serde_json::Value {
        json!({
            "bearer_token": "tok",
            "user_id": "bot",
            "api_base": api_url,
            "media_base": api_url,
            "poll_interval_ms": 50_000
        })
    }

    async fn mount_empty_poll(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [], "meta": {}
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn channel_type_is_x() {
        let f = XFactory::new();
        assert_eq!(f.channel_type().as_str(), "x");
    }

    #[tokio::test]
    async fn container_contribution_is_empty() {
        let f = XFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn shutdown_is_ok() {
        let f = XFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn init_starts_adapter_and_returns_x_channel_type() {
        let s = MockServer::start().await;
        mount_empty_poll(&s).await;
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(4);
        let setup = ChannelSetup::new(config_value(&s.uri()), tx, dir.path());
        let adapter = XFactory::new().init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "x");
    }

    #[tokio::test]
    async fn init_starts_background_poll_task() {
        // The /2/dm_events mock will be hit exactly because init spawns the
        // poll loop. Verify by emitting a single event and confirming it
        // arrives on the inbound channel.
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "id": "evt-init",
                    "event_type": "MessageCreate",
                    "text": "hi from poll",
                    "sender_id": "other",
                    "dm_conversation_id": "c1",
                    "created_at": "2024-01-01T00:00:00Z"
                }],
                "meta": { "newest_id": "evt-init" }
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(4);
        let setup = ChannelSetup::new(
            json!({
                "bearer_token": "tok",
                "user_id": "bot",
                "api_base": s.uri(),
                "media_base": s.uri(),
                "poll_interval_ms": 5
            }),
            tx,
            dir.path(),
        );
        let _adapter = XFactory::new().init(setup).await.unwrap();
        let evt = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.platform_id, "conversation:c1");
    }

    #[tokio::test]
    async fn init_propagates_config_errors() {
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        match XFactory::new().init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn register_inserts_factory_and_duplicate_errors() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("x")).is_some());
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn default_factory_equals_new() {
        let a = XFactory::new();
        let b = XFactory;
        assert_eq!(a.channel_type(), b.channel_type());
    }

    #[tokio::test]
    async fn factory_debug_format() {
        let f = XFactory::new();
        assert!(format!("{f:?}").contains("XFactory"));
    }

    #[test]
    fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "x");
    }
}
