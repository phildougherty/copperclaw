//! `DiscordFactory` — wires a `DiscordAdapter` from a `ChannelSetup`.

use crate::adapter::{DiscordAdapter, container_contribution_for};
use crate::config::DiscordConfig;
use crate::events::CHANNEL_TYPE_STR;
use crate::rest::DiscordRest;
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelSetup, ContainerContribution,
};
use copperclaw_types::ChannelType;
use std::sync::{Arc, Mutex};

/// Factory for the Discord channel.
#[derive(Debug, Default)]
pub struct DiscordFactory {
    /// Latest bot token observed at `init` time. Used to populate
    /// `container_contribution`. Wrapped in `Mutex` so factory methods
    /// can update it from `&self`.
    last_token: Mutex<Option<String>>,
}

impl DiscordFactory {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ChannelFactory for DiscordFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let cfg = DiscordConfig::from_value(&setup.config)?;
        if let Ok(mut g) = self.last_token.lock() {
            *g = Some(cfg.bot_token.clone());
        }
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| AdapterError::Transport(format!("reqwest build: {e}")))?;
        let rest = DiscordRest::new(client, &cfg.bot_token, &cfg.api_base);
        let adapter = Arc::new(DiscordAdapter::new(rest, cfg, setup.inbound_tx));
        adapter.spawn_gateway().await;
        Ok(adapter as Arc<dyn ChannelAdapter>)
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        // Background tasks live on each adapter, not on the factory.
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        if let Ok(g) = self.last_token.lock() {
            if let Some(tok) = g.as_ref() {
                return container_contribution_for(tok);
            }
        }
        // No init yet — surface an empty contribution rather than failing.
        ContainerContribution::default()
    }
}

/// Register the Discord factory with a `ChannelRegistry`.
pub fn register(
    registry: &mut copperclaw_channels_core::ChannelRegistry,
) -> Result<(), AdapterError> {
    registry.register(Arc::new(DiscordFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::ChannelRegistry;
    use copperclaw_types::InboundEvent;
    use serde_json::json;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn channel_type_is_discord() {
        let f = DiscordFactory::new();
        assert_eq!(f.channel_type().as_str(), "discord");
    }

    #[tokio::test]
    async fn init_with_valid_config_succeeds() {
        let f = DiscordFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(
            json!({
                "bot_token": "tok",
                "gateway_url": "ws://127.0.0.1:1",
                "api_base": "http://127.0.0.1:1"
            }),
            tx,
            "/tmp",
        );
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "discord");
    }

    #[tokio::test]
    async fn init_rejects_missing_bot_token() {
        let f = DiscordFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({}), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn init_rejects_non_object_config() {
        let f = DiscordFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!("nope"), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn shutdown_is_ok() {
        let f = DiscordFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn container_contribution_before_init_is_empty() {
        let f = DiscordFactory::new();
        let c = f.container_contribution();
        assert!(c.is_empty());
    }

    #[tokio::test]
    async fn container_contribution_after_init_carries_token_env() {
        let f = DiscordFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(
            json!({
                "bot_token": "the-token",
                "gateway_url": "ws://127.0.0.1:1",
                "api_base": "http://127.0.0.1:1"
            }),
            tx,
            "/tmp",
        );
        let _ = f.init(setup).await.unwrap();
        let c = f.container_contribution();
        assert_eq!(c.env, vec![("DISCORD_BOT_TOKEN".into(), "the-token".into())]);
    }

    #[test]
    fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("discord")).is_some());
    }

    #[test]
    fn register_twice_errors() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn factory_default_and_new_are_equivalent() {
        let _: DiscordFactory = DiscordFactory::default();
        let _: DiscordFactory = DiscordFactory::new();
    }

    #[test]
    fn debug_format_is_available() {
        let f = DiscordFactory::new();
        let _ = format!("{f:?}");
    }
}
