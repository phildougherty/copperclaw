//! Channel adapter and factory traits.

use crate::card::Card;
use crate::container::ContainerContribution;
use crate::dm::DmHandle;
use crate::error::AdapterError;
use crate::setup::ChannelSetup;
use async_trait::async_trait;
use ironclaw_types::{ChannelType, MessageKind, OutboundMessage};
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

    /// Render and deliver a portable [`Card`].
    ///
    /// The default implementation converts the card to the plain-text
    /// rendering from [`Card::to_text_fallback`] and routes it through
    /// [`Self::deliver`] as a `MessageKind::Chat` outbound message — so
    /// every adapter gets a usable card rendering for free, even before
    /// it provides a native implementation.
    ///
    /// Adapters with rich card support (Telegram inline keyboards, Slack
    /// Block Kit, Discord embeds, Google Chat cards v2, etc.) should
    /// override this to render the card structurally. Wave 2 of the cards
    /// rollout will implement those overrides — see the doc comment at
    /// the top of `card.rs` for the schema contract.
    ///
    /// `to` is a routing hint the host pulls from `SendCardSpec::to`. It
    /// is currently unused by the default impl (which uses `platform_id`)
    /// but is part of the signature so wave 2's native renderers can pass
    /// it through to platform DM-open flows.
    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        let _ = to;
        let text = card.to_text_fallback();
        let outbound = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({ "text": text }),
            files: vec![],
        };
        self.deliver(platform_id, thread_id, &outbound).await
    }

    /// Open a direct-message thread with the given user. Defaults to
    /// `Ok(None)` — channels that don't support DMs leave it alone.
    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }

    /// Edit an existing message. `external_id` is the platform's id for
    /// the original message (Telegram's `message_id`, Slack's `ts`,
    /// Discord's message id, etc.).
    ///
    /// Default impl returns `Err(AdapterError::Unsupported(_))` so adapters
    /// that don't expose an edit API fall through cleanly to a
    /// "fallback: send a new message" path in the host's delivery service.
    async fn edit_message(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        new_text: &str,
    ) -> Result<(), AdapterError> {
        let _ = (platform_id, thread_id, external_id, new_text);
        Err(AdapterError::Unsupported("edit_message".into()))
    }

    /// React to a message with an emoji. `external_id` is the platform's
    /// id for the target message (same shape as [`Self::edit_message`]).
    ///
    /// Default impl returns `Err(AdapterError::Unsupported(_))` so adapters
    /// that don't expose a reaction API fall through cleanly to a
    /// "fallback: send a new message" path in the host's delivery service.
    async fn add_reaction(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        let _ = (platform_id, thread_id, external_id, emoji);
        Err(AdapterError::Unsupported("add_reaction".into()))
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

    /// Adapter that only implements the mandatory `channel_type` /
    /// `deliver` methods — used to verify the trait-level defaults for
    /// `edit_message` and `add_reaction` return `Unsupported`.
    struct MinimalAdapter {
        channel_type: ChannelType,
    }

    #[async_trait]
    impl ChannelAdapter for MinimalAdapter {
        fn channel_type(&self) -> &ChannelType {
            &self.channel_type
        }
        async fn deliver(
            &self,
            _platform_id: &str,
            _thread_id: Option<&str>,
            _message: &OutboundMessage,
        ) -> Result<Option<String>, AdapterError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn default_edit_message_returns_unsupported() {
        let a = MinimalAdapter {
            channel_type: ChannelType::new("minimal"),
        };
        let err = a
            .edit_message("p", None, "ext-1", "new text")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(ref s) if s == "edit_message"));
    }

    #[tokio::test]
    async fn default_add_reaction_returns_unsupported() {
        let a = MinimalAdapter {
            channel_type: ChannelType::new("minimal"),
        };
        let err = a
            .add_reaction("p", None, "ext-1", "thumbsup")
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(ref s) if s == "add_reaction"));
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

    #[tokio::test]
    async fn default_deliver_card_falls_back_to_text_deliver() {
        // Cards with no native renderer get the trait-level fallback:
        // convert to text via `Card::to_text_fallback` and call `deliver`.
        let a = MockAdapter::new("ch");
        let card = crate::card::Card {
            title: Some("Hello".into()),
            body: Some("World".into()),
            ..crate::card::Card::default()
        };
        let id = a
            .deliver_card("plat-1", None, &card, None)
            .await
            .unwrap();
        assert!(id.is_some());
        let calls = a.deliveries();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message.kind, MessageKind::Chat);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.contains("**Hello**"));
        assert!(text.contains("World"));
    }

    #[tokio::test]
    async fn default_deliver_card_propagates_routing_args() {
        let a = MockAdapter::new("ch");
        let card = crate::card::Card {
            title: Some("Hi".into()),
            ..crate::card::Card::default()
        };
        a.deliver_card("plat-9", Some("thread-7"), &card, Some("user-1"))
            .await
            .unwrap();
        let calls = a.deliveries();
        assert_eq!(calls[0].platform_id, "plat-9");
        assert_eq!(calls[0].thread_id.as_deref(), Some("thread-7"));
    }
}
