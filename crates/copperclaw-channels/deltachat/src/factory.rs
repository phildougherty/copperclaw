//! [`DeltaChatFactory`] — the [`ChannelFactory`] producing
//! [`DeltaChatAdapter`] instances backed by the `deltachat-rpc-server`
//! subprocess.

use crate::adapter::DeltaChatAdapter;
use crate::api;
use crate::config::DeltaChatConfig;
use crate::rpc::{RpcTransport, SubprocessTransport};
use async_trait::async_trait;
use copperclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use copperclaw_types::ChannelType;
use std::sync::Arc;

/// `ChannelType` string used by this channel.
pub const CHANNEL_TYPE_STR: &str = "deltachat";

/// Factory for [`DeltaChatAdapter`].
///
/// The default `init` spawns a real [`SubprocessTransport`] over
/// `deltachat-rpc-server`. Tests construct adapters directly via
/// [`DeltaChatAdapter::start_with_transport`] using
/// [`crate::rpc::MockTransport`].
#[derive(Debug, Default)]
pub struct DeltaChatFactory;

impl DeltaChatFactory {
    /// Construct a fresh factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for DeltaChatFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let config = DeltaChatConfig::from_value(&setup.config)?;
        let transport: Arc<dyn RpcTransport> =
            SubprocessTransport::spawn(&config.rpc_server_bin, &config.extra_args)?;

        // Confirm the configured account exists; if the server reports no
        // accounts at all and the configured id is `0`, we eagerly call
        // `add_account` so the operator's first run "just works". Otherwise
        // we surface a clear BadRequest.
        let accounts = api::get_all_account_ids(transport.as_ref()).await?;
        if !accounts.contains(&config.account_id) {
            if accounts.is_empty() && config.account_id == 0 {
                let _ = api::add_account(transport.as_ref()).await?;
            } else {
                return Err(AdapterError::BadRequest(format!(
                    "deltachat account {} is not configured (known: {:?})",
                    config.account_id, accounts
                )));
            }
        }

        Ok(DeltaChatAdapter::start_with_transport(
            transport,
            config,
            setup.inbound_tx,
            setup.data_dir,
        ))
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        ContainerContribution::default()
    }
}

/// Register the [`DeltaChatFactory`] with the supplied registry.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(DeltaChatFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{MockResponse, MockTransport};
    use copperclaw_types::InboundEvent;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    /// A factory that swaps in a [`MockTransport`] instead of spawning a
    /// real subprocess. Useful for exercising the `init` glue without
    /// requiring `deltachat-rpc-server` on `PATH`.
    struct TestFactory {
        transport: Arc<MockTransport>,
    }

    #[async_trait]
    impl ChannelFactory for TestFactory {
        fn channel_type(&self) -> ChannelType {
            ChannelType::new(CHANNEL_TYPE_STR)
        }

        async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
            let config = DeltaChatConfig::from_value(&setup.config)?;
            let transport: Arc<dyn RpcTransport> = self.transport.clone();
            let accounts = api::get_all_account_ids(transport.as_ref()).await?;
            if !accounts.contains(&config.account_id) {
                return Err(AdapterError::BadRequest(format!(
                    "deltachat account {} is not configured (known: {:?})",
                    config.account_id, accounts
                )));
            }
            Ok(DeltaChatAdapter::start_with_transport(
                transport,
                config,
                setup.inbound_tx,
                setup.data_dir,
            ))
        }
    }

    #[tokio::test]
    async fn channel_type_is_deltachat() {
        let f = DeltaChatFactory::new();
        assert_eq!(f.channel_type().as_str(), "deltachat");
    }

    #[tokio::test]
    async fn container_contribution_is_empty() {
        let f = DeltaChatFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn shutdown_is_ok() {
        let f = DeltaChatFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn default_factory_matches_new() {
        let a = DeltaChatFactory::new();
        let b: DeltaChatFactory = DeltaChatFactory;
        assert_eq!(a.channel_type(), b.channel_type());
    }

    #[tokio::test]
    async fn debug_format_renders() {
        let f = DeltaChatFactory::new();
        assert!(format!("{f:?}").contains("DeltaChatFactory"));
    }

    #[tokio::test]
    async fn init_propagates_config_errors() {
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        match DeltaChatFactory::new().init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn init_with_missing_subprocess_returns_transport_error() {
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(
            json!({
                "account_id": 1,
                "rpc_server_bin": "/no/such/deltachat-rpc-server"
            }),
            tx,
            dir.path(),
        );
        match DeltaChatFactory::new().init(setup).await {
            Err(AdapterError::Transport(_)) => {}
            Err(other) => panic!("expected Transport error, got {other:?}"),
            Ok(_) => panic!("expected Transport error, got Ok"),
        }
    }

    #[tokio::test]
    async fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("deltachat")).is_some());
    }

    #[tokio::test]
    async fn register_duplicate_returns_bad_request() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "deltachat");
    }

    #[tokio::test]
    async fn test_factory_inits_with_existing_account() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::ok("get_all_account_ids", json!([1, 2])))
            .await;
        let f = TestFactory { transport: mock };
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({"account_id": 1}), tx, dir.path());
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "deltachat");
    }

    #[tokio::test]
    async fn test_factory_errors_when_account_missing() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::ok("get_all_account_ids", json!([1])))
            .await;
        let f = TestFactory { transport: mock };
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({"account_id": 99}), tx, dir.path());
        match f.init(setup).await {
            Err(AdapterError::BadRequest(m)) => assert!(m.contains("not configured")),
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }
}
