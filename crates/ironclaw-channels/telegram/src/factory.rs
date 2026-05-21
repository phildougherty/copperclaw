//! [`TelegramFactory`] — the [`ChannelFactory`] producing
//! [`TelegramAdapter`] instances.

use crate::adapter::{CHANNEL_TYPE_STR, TelegramAdapter};
use crate::api::TelegramApi;
use crate::config::TelegramConfig;
use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use ironclaw_types::ChannelType;
use std::sync::Arc;

/// Factory for [`TelegramAdapter`].
#[derive(Debug, Default)]
pub struct TelegramFactory;

impl TelegramFactory {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for TelegramFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let config = TelegramConfig::from_value(&setup.config)?;
        // Validate the bot token by calling getMe and capture the username so
        // the ingress loop can resolve `is_mention` without a second round-trip.
        let api = TelegramApi::new(config.api_base.clone(), config.bot_token.clone());
        let me = api.get_me().await?;
        let username = me.username;
        let adapter = TelegramAdapter::start_with_api(
            api,
            config,
            username,
            setup.inbound_tx,
            setup.data_dir,
        )
        .await?;
        Ok(adapter as Arc<dyn ChannelAdapter>)
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        // No global resources at the factory level; per-adapter cleanup is
        // driven by the adapter's own cancellation token.
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        // Telegram talks to the platform via the host; nothing extra is
        // needed inside the agent container.
        ContainerContribution::default()
    }
}

/// Register the [`TelegramFactory`] with the supplied registry.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(TelegramFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_channels_core::ChannelRegistry;
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn config_value(uri: &str) -> serde_json::Value {
        json!({
            "bot_token": "tok",
            "api_base": uri,
            "mode": "long_poll",
            "long_poll": { "timeout_secs": 0, "limit": 100 }
        })
    }

    async fn mount_get_me(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "id": 1, "is_bot": true, "username": "ironbot" }
            })))
            .mount(s)
            .await;
    }

    async fn mount_get_updates_empty(s: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": []
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn channel_type_is_telegram() {
        let f = TelegramFactory::new();
        assert_eq!(f.channel_type().as_str(), "telegram");
    }

    #[tokio::test]
    async fn container_contribution_is_empty() {
        let f = TelegramFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn shutdown_is_ok() {
        let f = TelegramFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn init_validates_bot_token_and_returns_adapter() {
        let s = MockServer::start().await;
        mount_get_me(&s).await;
        mount_get_updates_empty(&s).await;

        let (tx, _rx) = mpsc::channel::<InboundEvent>(4);
        let setup = ChannelSetup::new(config_value(&s.uri()), tx, "/tmp");
        let adapter = TelegramFactory::new().init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "telegram");
    }

    #[tokio::test]
    async fn init_rejects_invalid_token() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/bottok/getMe"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "ok": false, "error_code": 401, "description": "Unauthorized"
            })))
            .mount(&s)
            .await;
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(config_value(&s.uri()), tx, "/tmp");
        match TelegramFactory::new().init(setup).await {
            Err(AdapterError::Auth(_)) => {}
            Err(other) => panic!("expected Auth, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn init_propagates_config_errors() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({}), tx, "/tmp");
        match TelegramFactory::new().init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn default_factory_equals_new() {
        let a = TelegramFactory::new();
        let b = TelegramFactory;
        assert_eq!(a.channel_type(), b.channel_type());
    }

    #[tokio::test]
    async fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("telegram")).is_some());
        // Duplicate registration is rejected.
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn factory_debug_format() {
        let f = TelegramFactory::new();
        assert!(format!("{f:?}").contains("TelegramFactory"));
    }
}
