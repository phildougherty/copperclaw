//! Channel adapter and factory traits.

use crate::container::ContainerContribution;
use crate::dm::DmHandle;
use crate::error::AdapterError;
use crate::setup::ChannelSetup;
use async_trait::async_trait;
use ironclaw_types::{ChannelType, OutboundMessage};
use std::sync::Arc;

/// A channel adapter speaks to one external platform (Telegram, Slack, …).
///
/// The host calls into the adapter to subscribe to events, signal typing,
/// deliver outbound messages, and (where supported) open DMs.
///
/// All methods except `deliver` have sensible defaults so channel
/// implementations only override what the platform actually supports.
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    /// The `ChannelType` this adapter handles (e.g. `ChannelType::new("telegram")`).
    fn channel_type(&self) -> &ChannelType;

    /// Whether the platform has a distinct concept of threads.
    /// Defaults to `false`.
    fn supports_threads(&self) -> bool {
        false
    }

    /// Begin observing the given conversation for inbound events. For
    /// channels that already stream everything (e.g. long-polling bots)
    /// this is a no-op. Defaults to `Ok(())`.
    async fn subscribe(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        Ok(())
    }

    /// Send a typing indicator to the platform. Defaults to `Ok(())` so
    /// channels without typing support are silent no-ops.
    async fn set_typing(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        Ok(())
    }

    /// Deliver an outbound message. Returns the platform-side message id
    /// when known (`None` if the platform doesn't expose one).
    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError>;

    /// Open a direct-message thread with the given user. Defaults to
    /// `Ok(None)` — channels that don't support DMs leave it alone.
    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }

    /// Strip channel-specific formatting from an outbound message body,
    /// returning a plain-text fallback the channel will accept even when
    /// the original failed a formatting validation.
    ///
    /// The delivery loop calls this when a `deliver` call returned
    /// `AdapterError::BadRequest(_)` with a known formatting-error signature
    /// (e.g. Telegram's "can't parse entities", Slack's block-kit errors,
    /// Discord's embed errors). The returned message must be safe to feed
    /// directly back through `deliver`.
    ///
    /// Default impl returns `None` — adapters without a known plain-text
    /// recovery should fail fast instead of degrading silently. Channels that
    /// know how to strip formatting metadata (parse_mode for Telegram,
    /// `blocks` for Slack, `embeds` for Discord) override this.
    ///
    /// The text body itself is preserved (emoji, unicode included) — only
    /// formatting metadata is removed. Implementations are expected to
    /// prepend a "[reduced formatting] " marker to the text so the user
    /// knows the message arrived in a downgraded shape.
    fn plain_text_fallback(&self, _msg: &OutboundMessage) -> Option<OutboundMessage> {
        None
    }
}

/// A factory builds a `ChannelAdapter` for a particular channel kind.
///
/// Factories are registered with the `ChannelRegistry` at startup. The
/// host looks up factories by `channel_type` and calls `init` once per
/// configured channel instance.
#[async_trait]
pub trait ChannelFactory: Send + Sync {
    /// Channel type produced by this factory.
    fn channel_type(&self) -> ChannelType;

    /// Build the adapter from the host-provided `ChannelSetup`.
    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError>;

    /// Gracefully tear down any global resources. Defaults to `Ok(())`.
    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    /// What this channel contributes to an agent container. Defaults to
    /// `ContainerContribution::default()` (nothing).
    fn container_contribution(&self) -> ContainerContribution {
        ContainerContribution::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MockAdapter, MockFactory};
    use ironclaw_types::MessageKind;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn outbound() -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "hi"}),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn default_subscribe_returns_ok() {
        // MockAdapter does not override subscribe, so we exercise the default
        // body declared on the trait.
        let a = MockAdapter::new("x");
        a.subscribe("p", None).await.unwrap();
        a.subscribe("p", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn default_set_typing_returns_ok() {
        let a = MockAdapter::new("x");
        a.set_typing("p", None).await.unwrap();
    }

    #[tokio::test]
    async fn default_open_dm_returns_none() {
        let a = MockAdapter::new("x");
        let res = a.open_dm("u").await.unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn default_supports_threads_is_false() {
        let a = MockAdapter::new("x");
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn deliver_records_and_returns_id() {
        let a = MockAdapter::new("x");
        let id = a.deliver("plat-1", None, &outbound()).await.unwrap();
        assert!(id.is_some());
        assert_eq!(a.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn factory_init_returns_adapter() {
        let f = MockFactory::new("mock");
        let (tx, _rx) = mpsc::channel(1);
        let setup = ChannelSetup::new(json!({}), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "mock");
    }

    #[tokio::test]
    async fn factory_default_shutdown_is_ok() {
        let f = MockFactory::new("mock");
        f.shutdown().await.unwrap();
    }

    #[test]
    fn factory_default_container_contribution_is_empty() {
        let f = MockFactory::new("mock");
        assert!(f.container_contribution().is_empty());
    }
}
