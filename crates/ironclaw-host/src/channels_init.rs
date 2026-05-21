//! Channel registry construction and per-channel initialization.
//!
//! The host owns one [`ChannelRegistry`] and exactly one mpsc `Sender` per
//! channel adapter (the receiver side stays in the boot loop and feeds the
//! router). This module hides both pieces behind two helpers:
//!
//! - [`build_registry`] — register every channel factory the host knows about.
//! - [`init_channels`] — for each [`ChannelInit`] in the config, call
//!   `factory.init(setup).await` and collect the resulting adapters.

use crate::config::ChannelInit;
use ironclaw_channels_cli::CliFactory;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelRegistry, ChannelSetup,
};
use ironclaw_types::{ChannelType, InboundEvent};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

/// Default mpsc buffer for inbound events from each channel adapter.
pub const DEFAULT_INBOUND_BUFFER: usize = 256;

/// Build a registry pre-populated with the in-tree channel factories.
///
/// Every channel crate in the workspace is registered here. A factory only
/// produces a live adapter when `init_channels` is called with a matching
/// `ChannelInit` from the host config — registering is cheap and lets the
/// host pick up any subset by configuration alone.
pub fn build_registry() -> ChannelRegistry {
    type RegisterFn = fn(&mut ChannelRegistry) -> Result<(), AdapterError>;
    let mut reg = ChannelRegistry::new();
    let registrations: &[(&str, RegisterFn)] = &[
        ("cli", ironclaw_channels_cli::register),
        ("telegram", ironclaw_channels_telegram::register),
        ("slack", ironclaw_channels_slack::register),
        ("discord", ironclaw_channels_discord::register),
        ("resend", ironclaw_channels_resend::register),
        ("github", ironclaw_channels_github::register),
        ("linear", ironclaw_channels_linear::register),
        ("webex", ironclaw_channels_webex::register),
        ("matrix", ironclaw_channels_matrix::register),
        ("teams", ironclaw_channels_teams::register),
        ("gchat", ironclaw_channels_gchat::register),
        ("imessage", ironclaw_channels_imessage::register),
        ("wechat", ironclaw_channels_wechat::register),
        ("whatsapp-cloud", ironclaw_channels_whatsapp_cloud::register),
        ("signal", ironclaw_channels_signal::register),
        ("deltachat", ironclaw_channels_deltachat::register),
        ("emacs", ironclaw_channels_emacs::register),
        ("x", ironclaw_channels_x::register),
    ];
    for (name, register) in registrations {
        if let Err(err) = register(&mut reg) {
            tracing::warn!(channel = *name, ?err, "failed to register channel factory");
        }
    }
    reg
}

/// Per-channel init result.
///
/// `channel_type` is the `ChannelType` the factory advertised. `adapter` is
/// what the host will hand to `DeliveryService` (and store in the
/// dispatcher's resolver map).
#[derive(Clone)]
pub struct InitializedChannel {
    pub channel_type: ChannelType,
    pub adapter: Arc<dyn ChannelAdapter>,
}

impl std::fmt::Debug for InitializedChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InitializedChannel")
            .field("channel_type", &self.channel_type)
            .finish_non_exhaustive()
    }
}

/// Initialize every channel in `inits` against `registry`. Each one shares
/// `inbound_tx` so the host has a single mpsc receiver to forward to the
/// router.
///
/// Channels whose factory is missing or whose `init` fails are logged at
/// warn level and skipped — a misconfigured channel must not take the whole
/// host down.
pub async fn init_channels(
    registry: &ChannelRegistry,
    inits: &[ChannelInit],
    inbound_tx: Sender<InboundEvent>,
    data_root: &Path,
) -> Vec<InitializedChannel> {
    let mut out = Vec::with_capacity(inits.len());
    for init in inits {
        let ct = ChannelType::new(&init.channel_type);
        let Some(factory) = registry.get(&ct) else {
            tracing::warn!(
                channel_type = %ct,
                "no channel factory registered; skipping",
            );
            continue;
        };
        let data_dir = data_root.join("channels").join(&init.channel_type);
        if let Err(err) = std::fs::create_dir_all(&data_dir) {
            tracing::warn!(channel_type = %ct, ?err, "failed to create channel data dir");
            continue;
        }
        let setup = ChannelSetup {
            config: init.config.clone(),
            inbound_tx: inbound_tx.clone(),
            data_dir,
        };
        match factory.init(setup).await {
            Ok(adapter) => out.push(InitializedChannel {
                channel_type: ct,
                adapter,
            }),
            Err(err) => {
                tracing::warn!(channel_type = %ct, ?err, "channel init failed; skipping");
            }
        }
    }
    out
}

/// Construct a fresh `CliFactory` — exposed so tests can construct one
/// without depending on the channels-cli crate directly.
pub fn cli_factory() -> Arc<dyn ChannelFactory> {
    Arc::new(CliFactory::new())
}

/// Convenience: bind a fresh in-process channel adapter for tests. Returns
/// the adapter and a receiver that yields any events the adapter publishes.
pub async fn init_inproc_factory(
    factory: Arc<dyn ChannelFactory>,
    config: serde_json::Value,
    inbound_tx: Sender<InboundEvent>,
    data_dir: PathBuf,
) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
    let setup = ChannelSetup {
        config,
        inbound_tx,
        data_dir,
    };
    factory.init(setup).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn build_registry_has_cli() {
        let reg = build_registry();
        assert!(reg.get(&ChannelType::new("cli")).is_some());
    }

    #[test]
    fn build_registry_has_every_in_tree_channel() {
        let reg = build_registry();
        for name in [
            "cli",
            "telegram",
            "slack",
            "discord",
            "resend",
            "github",
            "linear",
            "webex",
            "matrix",
            "teams",
            "gchat",
            "imessage",
            "wechat",
            "whatsapp-cloud",
            "signal",
            "deltachat",
            "emacs",
            "x",
        ] {
            assert!(
                reg.get(&ChannelType::new(name)).is_some(),
                "channel {name} not registered",
            );
        }
    }

    #[tokio::test]
    async fn init_skips_unknown_channel() {
        let reg = build_registry();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let tmp = tempfile::tempdir().unwrap();
        let out = init_channels(
            &reg,
            &[ChannelInit {
                channel_type: "ghost".into(),
                config: serde_json::json!({}),
            }],
            tx,
            tmp.path(),
        )
        .await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn init_cli_channel_succeeds() {
        let reg = build_registry();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let tmp = tempfile::tempdir().unwrap();
        let out = init_channels(
            &reg,
            &[ChannelInit {
                channel_type: "cli".into(),
                config: serde_json::json!({}),
            }],
            tx,
            tmp.path(),
        )
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].channel_type.as_str(), "cli");
    }

    #[tokio::test]
    async fn init_cli_channel_with_bad_config_is_skipped() {
        let reg = build_registry();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let tmp = tempfile::tempdir().unwrap();
        let out = init_channels(
            &reg,
            &[ChannelInit {
                channel_type: "cli".into(),
                config: serde_json::json!("not an object"),
            }],
            tx,
            tmp.path(),
        )
        .await;
        // Bad config -> init returned Err -> we skip it.
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn init_creates_per_channel_data_dir() {
        let reg = build_registry();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let tmp = tempfile::tempdir().unwrap();
        let _ = init_channels(
            &reg,
            &[ChannelInit {
                channel_type: "cli".into(),
                config: serde_json::json!({}),
            }],
            tx,
            tmp.path(),
        )
        .await;
        assert!(tmp.path().join("channels").join("cli").exists());
    }

    #[test]
    fn default_inbound_buffer_constant_is_published() {
        // Compile-time guard: ensures the value remains usize.
        let _: usize = DEFAULT_INBOUND_BUFFER;
    }

    #[test]
    fn cli_factory_helper_returns_arc() {
        let f = cli_factory();
        assert_eq!(f.channel_type().as_str(), "cli");
    }

    #[tokio::test]
    async fn init_inproc_factory_succeeds_for_cli() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let tmp = tempfile::tempdir().unwrap();
        let adapter = init_inproc_factory(
            cli_factory(),
            serde_json::json!({}),
            tx,
            tmp.path().to_path_buf(),
        )
        .await
        .unwrap();
        assert_eq!(adapter.channel_type().as_str(), "cli");
    }

    #[test]
    fn initialized_channel_debug_renders() {
        let f = cli_factory();
        let _ = f;
        // We can't construct a real adapter outside an async context here, so
        // assert the Debug stub renders for an `InitializedChannel` with the
        // type only.
        let s = format!(
            "{:?}",
            InitializedChannel {
                channel_type: ChannelType::new("cli"),
                adapter: Arc::new(NoopAdapter::new()),
            }
        );
        assert!(s.contains("InitializedChannel"));
    }

    /// Local no-op adapter used only by the test above.
    struct NoopAdapter {
        ct: ChannelType,
    }
    impl NoopAdapter {
        fn new() -> Self {
            Self {
                ct: ChannelType::new("cli"),
            }
        }
    }
    #[async_trait::async_trait]
    impl ChannelAdapter for NoopAdapter {
        fn channel_type(&self) -> &ChannelType {
            &self.ct
        }
        async fn deliver(
            &self,
            _platform_id: &str,
            _thread_id: Option<&str>,
            _message: &ironclaw_types::OutboundMessage,
        ) -> Result<Option<String>, AdapterError> {
            Ok(None)
        }
    }
}
