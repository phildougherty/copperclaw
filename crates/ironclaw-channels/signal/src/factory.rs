//! [`SignalFactory`] — the [`ChannelFactory`] producing [`SignalAdapter`]
//! instances.
//!
//! `init` spawns a real `signal-cli daemon --json-rpc` subprocess via
//! [`crate::rpc::JsonRpcClient::spawn`], wires it into a
//! [`SignalAdapter`], and returns the adapter behind `Arc<dyn ChannelAdapter>`.
//!
//! Tests do **not** spawn the subprocess; they construct
//! [`SignalAdapter::with_transport`] directly with a mock transport.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
    ContainerContribution,
};
use ironclaw_types::ChannelType;

use crate::adapter::SignalAdapter;
use crate::config::SignalConfig;
use crate::rpc::JsonRpcClient;

/// Channel-type string used by this channel (`"signal"`).
pub const CHANNEL_TYPE_STR: &str = "signal";

/// Build the argv vector used to spawn `signal-cli` in daemon JSON-RPC
/// mode for the supplied account.
///
/// Public for use by tests that wish to assert the argv shape without
/// actually spawning.
#[must_use]
pub fn build_signal_cli_args(config: &SignalConfig) -> Vec<String> {
    let mut args = Vec::new();
    args.push("-a".to_owned());
    args.push(config.account.clone());
    args.push("--output=json".to_owned());
    args.extend(config.extra_args.iter().cloned());
    args.push("daemon".to_owned());
    args.push("--json-rpc".to_owned());
    args.push("--receive-mode=on-start".to_owned());
    args
}

/// Factory for [`SignalAdapter`].
#[derive(Debug, Default)]
pub struct SignalFactory;

impl SignalFactory {
    /// Construct a fresh factory.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for SignalFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(CHANNEL_TYPE_STR)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let config = SignalConfig::from_value(&setup.config)?;
        let args = build_signal_cli_args(&config);
        let transport = JsonRpcClient::spawn(&config.signal_cli_bin, &args)?;
        let adapter =
            SignalAdapter::with_transport(transport, setup.inbound_tx, setup.data_dir).await;
        Ok(Arc::new(adapter) as Arc<dyn ChannelAdapter>)
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        ContainerContribution::default()
    }
}

/// Register the [`SignalFactory`] with the supplied registry.
pub fn register(registry: &mut ChannelRegistry) -> Result<(), AdapterError> {
    registry.register(Arc::new(SignalFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::SignalAdapter;
    use crate::rpc::{MockTransport, RpcTransport};
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn channel_type_is_signal() {
        let f = SignalFactory::new();
        assert_eq!(f.channel_type().as_str(), "signal");
    }

    #[tokio::test]
    async fn container_contribution_is_empty() {
        let f = SignalFactory::new();
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn shutdown_is_ok() {
        let f = SignalFactory::new();
        f.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn init_with_bad_config_returns_bad_request() {
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        match SignalFactory::new().init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn init_propagates_transport_error_on_bogus_binary() {
        // A valid config that points at a binary that almost certainly does
        // not exist; spawn should fail with Transport.
        let dir = TempDir::new().unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(
            json!({
                "account": "+15551234",
                "signal_cli_bin": "definitely-not-on-path-signal-cli-xyz"
            }),
            tx,
            dir.path(),
        );
        match SignalFactory::new().init(setup).await {
            Err(AdapterError::Transport(_)) => {}
            Err(other) => panic!("expected Transport, got {other:?}"),
            Ok(_) => panic!("expected error, got adapter"),
        }
    }

    #[tokio::test]
    async fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("signal")).is_some());
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn default_factory_equals_new() {
        let a = SignalFactory::new();
        let b = SignalFactory;
        assert_eq!(a.channel_type(), b.channel_type());
    }

    #[tokio::test]
    async fn factory_debug_format() {
        let f = SignalFactory::new();
        assert!(format!("{f:?}").contains("SignalFactory"));
    }

    #[test]
    fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "signal");
    }

    #[test]
    fn build_signal_cli_args_includes_account_and_daemon() {
        let cfg = SignalConfig {
            account: "+15551112222".into(),
            signal_cli_bin: "signal-cli".into(),
            extra_args: vec![],
            restart_on_exit: true,
        };
        let args = build_signal_cli_args(&cfg);
        assert!(args.contains(&"-a".to_owned()));
        assert!(args.contains(&"+15551112222".to_owned()));
        assert!(args.contains(&"daemon".to_owned()));
        assert!(args.contains(&"--json-rpc".to_owned()));
        assert!(args.contains(&"--receive-mode=on-start".to_owned()));
        assert!(args.contains(&"--output=json".to_owned()));
    }

    #[test]
    fn build_signal_cli_args_inserts_extra_args_before_daemon() {
        let cfg = SignalConfig {
            account: "+1".into(),
            signal_cli_bin: "signal-cli".into(),
            extra_args: vec!["--config".into(), "/etc/sc".into()],
            restart_on_exit: false,
        };
        let args = build_signal_cli_args(&cfg);
        let daemon_idx = args.iter().position(|s| s == "daemon").unwrap();
        let cfg_idx = args.iter().position(|s| s == "--config").unwrap();
        assert!(cfg_idx < daemon_idx);
    }

    // Exercise the adapter-construction code path that init() takes after a
    // successful spawn, using a mock transport so we don't fork anything.
    #[tokio::test]
    async fn with_transport_starts_a_usable_adapter() {
        let (mock, ctl) = MockTransport::new();
        ctl.expect_ok("send", json!({"timestamp": 1})).await;
        let arc: Arc<dyn RpcTransport> = Arc::new(mock);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(4);
        let adapter =
            SignalAdapter::with_transport(arc, tx, PathBuf::from("/tmp/signal-fac-test")).await;
        assert_eq!(adapter.channel_type().as_str(), "signal");
        adapter.shutdown().await;
    }
}
