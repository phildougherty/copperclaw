//! The handoff structure a channel factory receives at `init`.

use ironclaw_types::InboundEvent;
use std::path::PathBuf;
use tokio::sync::mpsc::Sender;

/// Per-channel initialization context.
///
/// This is the contract between the host and every `ChannelFactory::init`.
///
/// - `config` — the JSON blob the host loaded for this channel (from the
///   central DB row `channels.config_json`). Channel-specific schema.
/// - `inbound_tx` — bounded mpsc sender; the adapter pushes `InboundEvent`s
///   here as platform events arrive. The host owns the receiver side.
/// - `data_dir` — a host-side directory the channel may use freely for
///   credential caches, session state, attachment scratch, etc. The host
///   guarantees the directory exists and is unique per channel instance.
#[derive(Debug, Clone)]
pub struct ChannelSetup {
    pub config: serde_json::Value,
    pub inbound_tx: Sender<InboundEvent>,
    pub data_dir: PathBuf,
}

impl ChannelSetup {
    /// Convenience constructor. Most call sites will build the struct
    /// directly; this exists for tests and adapters that prefer the
    /// builder-style call.
    pub fn new(
        config: serde_json::Value,
        inbound_tx: Sender<InboundEvent>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config,
            inbound_tx,
            data_dir: data_dir.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;

    #[test]
    fn new_populates_fields() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({"k": "v"}), tx, "/tmp/x");
        assert_eq!(setup.config["k"], "v");
        assert_eq!(setup.data_dir, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn struct_literal_works() {
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup {
            config: json!(null),
            inbound_tx: tx,
            data_dir: PathBuf::from("/tmp"),
        };
        let _ = setup.clone();
    }
}
