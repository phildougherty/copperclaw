//! Reusable mocks for downstream channel crates and the host.
//!
//! Lives under a public module (not `#[cfg(test)]`) so other crates can
//! exercise channel-driven code paths with a deterministic adapter.

use crate::adapter::{ChannelAdapter, ChannelFactory};
use crate::container::ContainerContribution;
use crate::dm::DmHandle;
use crate::error::AdapterError;
use crate::setup::ChannelSetup;
use async_trait::async_trait;
use ironclaw_types::{ChannelType, OutboundMessage};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Captured `deliver` call.
#[derive(Debug, Clone)]
pub struct DeliveredMessage {
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub message: OutboundMessage,
}

/// Captured `edit_message` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEdit {
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub external_id: String,
    pub new_text: String,
}

/// Captured `add_reaction` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedReaction {
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub external_id: String,
    pub emoji: String,
}

/// Deterministic adapter for tests. Tracks every `deliver` call and lets
/// callers override behavior with optional canned outcomes.
pub struct MockAdapter {
    channel_type: ChannelType,
    supports_threads: bool,
    deliveries: Mutex<Vec<DeliveredMessage>>,
    delivery_id_prefix: String,
    deliver_should_error: Mutex<VecDeque<AdapterError>>,
    open_dm_result: Mutex<Option<DmHandle>>,
    /// When set, [`ChannelAdapter::plain_text_fallback`] returns a downgraded
    /// version of the input message: any `parse_mode`, `blocks`, `embeds`
    /// fields are stripped and the `text` field is prepended with
    /// `"[reduced formatting] "`. Lets the delivery-loop tests simulate an
    /// adapter that owns a real fallback path without depending on a
    /// concrete channel crate.
    plain_text_fallback_enabled: Mutex<bool>,
    edits: Mutex<Vec<CapturedEdit>>,
    reactions: Mutex<Vec<CapturedReaction>>,
    /// When `true`, [`ChannelAdapter::edit_message`] returns `Unsupported`
    /// instead of recording the call. Used to simulate channels (CLI,
    /// webhooks, etc.) whose default impl falls through.
    edit_returns_unsupported: Mutex<bool>,
    /// Same shape as `edit_returns_unsupported` but for `add_reaction`.
    reaction_returns_unsupported: Mutex<bool>,
}

impl MockAdapter {
    /// Adapter that reports the given channel type. `deliver` returns
    /// `Ok(Some("<channel_type>-<n>"))` and records the call.
    pub fn new(channel_type: impl Into<String>) -> Self {
        let ct: String = channel_type.into();
        Self {
            delivery_id_prefix: ct.clone(),
            channel_type: ChannelType::new(ct),
            supports_threads: false,
            deliveries: Mutex::new(vec![]),
            deliver_should_error: Mutex::new(VecDeque::new()),
            open_dm_result: Mutex::new(None),
            plain_text_fallback_enabled: Mutex::new(false),
            edits: Mutex::new(vec![]),
            reactions: Mutex::new(vec![]),
            edit_returns_unsupported: Mutex::new(false),
            reaction_returns_unsupported: Mutex::new(false),
        }
    }

    /// Snapshot the recorded `edit_message` calls.
    pub fn edits(&self) -> Vec<CapturedEdit> {
        self.edits.lock().expect("poisoned").clone()
    }

    /// Snapshot the recorded `add_reaction` calls.
    pub fn reactions(&self) -> Vec<CapturedReaction> {
        self.reactions.lock().expect("poisoned").clone()
    }

    /// Cause `edit_message` to return `AdapterError::Unsupported(_)` instead
    /// of recording the call. Used to drive the host's fallback path.
    pub fn set_edit_unsupported(&self, on: bool) {
        *self.edit_returns_unsupported.lock().expect("poisoned") = on;
    }

    /// Same as [`Self::set_edit_unsupported`] but for `add_reaction`.
    pub fn set_reaction_unsupported(&self, on: bool) {
        *self.reaction_returns_unsupported.lock().expect("poisoned") = on;
    }

    /// Toggle whether the adapter reports thread support.
    #[must_use]
    pub fn with_threads(mut self, supports: bool) -> Self {
        self.supports_threads = supports;
        self
    }

    /// Snapshot the recorded deliveries.
    pub fn deliveries(&self) -> Vec<DeliveredMessage> {
        self.deliveries.lock().expect("poisoned").clone()
    }

    /// Cause the next call to `deliver` to fail with `err`. Cleared on use.
    /// Calling this multiple times queues the failures FIFO — useful for
    /// the delivery-loop tests that exercise the formatting-fallback retry,
    /// which expects both the primary call AND the retry to fail.
    pub fn fail_next_deliver(&self, err: AdapterError) {
        self.deliver_should_error
            .lock()
            .expect("poisoned")
            .push_back(err);
    }

    /// Enable / disable the [`ChannelAdapter::plain_text_fallback`] hook
    /// for this mock. Defaults to disabled (`None`). When enabled, the
    /// fallback returns the input message with any `parse_mode`, `blocks`,
    /// `embeds` keys stripped and the `text` field prepended with
    /// `"[reduced formatting] "`.
    pub fn enable_plain_text_fallback(&self, on: bool) {
        *self.plain_text_fallback_enabled.lock().expect("poisoned") = on;
    }

    /// Make `open_dm` return this handle.
    pub fn set_dm_handle(&self, handle: DmHandle) {
        *self.open_dm_result.lock().expect("poisoned") = Some(handle);
    }
}

#[async_trait]
impl ChannelAdapter for MockAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        self.supports_threads
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        if let Some(err) = self
            .deliver_should_error
            .lock()
            .expect("poisoned")
            .pop_front()
        {
            return Err(err);
        }
        let mut guard = self.deliveries.lock().expect("poisoned");
        guard.push(DeliveredMessage {
            platform_id: platform_id.to_owned(),
            thread_id: thread_id.map(str::to_owned),
            message: message.clone(),
        });
        Ok(Some(format!(
            "{}-{}",
            self.delivery_id_prefix,
            guard.len()
        )))
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(self.open_dm_result.lock().expect("poisoned").clone())
    }

    async fn edit_message(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        new_text: &str,
    ) -> Result<(), AdapterError> {
        if *self.edit_returns_unsupported.lock().expect("poisoned") {
            return Err(AdapterError::Unsupported("edit_message".into()));
        }
        self.edits.lock().expect("poisoned").push(CapturedEdit {
            platform_id: platform_id.to_owned(),
            thread_id: thread_id.map(str::to_owned),
            external_id: external_id.to_owned(),
            new_text: new_text.to_owned(),
        });
        Ok(())
    }

    async fn add_reaction(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        if *self.reaction_returns_unsupported.lock().expect("poisoned") {
            return Err(AdapterError::Unsupported("add_reaction".into()));
        }
        self.reactions
            .lock()
            .expect("poisoned")
            .push(CapturedReaction {
                platform_id: platform_id.to_owned(),
                thread_id: thread_id.map(str::to_owned),
                external_id: external_id.to_owned(),
                emoji: emoji.to_owned(),
            });
        Ok(())
    }

    fn plain_text_fallback(&self, msg: &OutboundMessage) -> Option<OutboundMessage> {
        if !*self.plain_text_fallback_enabled.lock().expect("poisoned") {
            return None;
        }
        let mut content = msg.content.clone();
        if let Some(obj) = content.as_object_mut() {
            obj.remove("parse_mode");
            obj.remove("blocks");
            obj.remove("embeds");
            // Prepend the downgrade marker to the text body.
            let text = obj
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            let downgraded = format!("[reduced formatting] {text}");
            obj.insert(
                "text".to_owned(),
                serde_json::Value::String(downgraded),
            );
        }
        Some(OutboundMessage {
            kind: msg.kind,
            content,
            files: msg.files.clone(),
        })
    }
}

/// Factory producing `MockAdapter`. Records every `init` call.
pub struct MockFactory {
    channel_type: ChannelType,
    init_count: Mutex<usize>,
    shutdown_count: Mutex<usize>,
    contribution: ContainerContribution,
}

impl MockFactory {
    pub fn new(channel_type: impl Into<String>) -> Self {
        Self {
            channel_type: ChannelType::new(channel_type),
            init_count: Mutex::new(0),
            shutdown_count: Mutex::new(0),
            contribution: ContainerContribution::default(),
        }
    }

    /// Override the container contribution reported by `container_contribution`.
    #[must_use]
    pub fn with_contribution(mut self, contribution: ContainerContribution) -> Self {
        self.contribution = contribution;
        self
    }

    pub fn init_count(&self) -> usize {
        *self.init_count.lock().expect("poisoned")
    }

    pub fn shutdown_count(&self) -> usize {
        *self.shutdown_count.lock().expect("poisoned")
    }
}

#[async_trait]
impl ChannelFactory for MockFactory {
    fn channel_type(&self) -> ChannelType {
        self.channel_type.clone()
    }

    async fn init(&self, _setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        *self.init_count.lock().expect("poisoned") += 1;
        Ok(Arc::new(MockAdapter::new(self.channel_type.as_str())))
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        *self.shutdown_count.lock().expect("poisoned") += 1;
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        self.contribution.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::MessageKind;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn outbound(text: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": text }),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn mock_adapter_records_deliveries() {
        let a = MockAdapter::new("ch");
        let id1 = a.deliver("p1", None, &outbound("a")).await.unwrap();
        let id2 = a.deliver("p1", Some("t"), &outbound("b")).await.unwrap();
        assert_eq!(id1.as_deref(), Some("ch-1"));
        assert_eq!(id2.as_deref(), Some("ch-2"));
        let calls = a.deliveries();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].thread_id.as_deref(), Some("t"));
    }

    #[tokio::test]
    async fn mock_adapter_can_fail_next_deliver() {
        let a = MockAdapter::new("ch");
        a.fail_next_deliver(AdapterError::Rate { retry_after: Some(5) });
        let err = a.deliver("p", None, &outbound("x")).await.unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: Some(5) }));
        // Next call succeeds; failure was consumed.
        a.deliver("p", None, &outbound("y")).await.unwrap();
    }

    #[tokio::test]
    async fn mock_adapter_open_dm_returns_configured_handle() {
        let a = MockAdapter::new("ch");
        let h = DmHandle {
            user_id: "u".into(),
            platform_id: "p".into(),
            channel_type: ChannelType::new("ch"),
        };
        a.set_dm_handle(h.clone());
        let got = a.open_dm("u").await.unwrap();
        assert_eq!(got, Some(h));
    }

    #[tokio::test]
    async fn mock_adapter_with_threads_toggle() {
        let a = MockAdapter::new("ch").with_threads(true);
        assert!(a.supports_threads());
    }

    #[tokio::test]
    async fn mock_factory_init_and_shutdown_counts() {
        let f = MockFactory::new("ch");
        let (tx, _rx) = mpsc::channel(1);
        let setup = ChannelSetup::new(json!({}), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "ch");
        assert_eq!(f.init_count(), 1);
        f.shutdown().await.unwrap();
        assert_eq!(f.shutdown_count(), 1);
    }

    #[test]
    fn mock_factory_with_contribution() {
        let mut contribution = ContainerContribution::default();
        contribution.env.push(("K".into(), "V".into()));
        let f = MockFactory::new("ch").with_contribution(contribution.clone());
        assert_eq!(f.container_contribution(), contribution);
    }

    #[test]
    fn delivered_message_clone_and_debug() {
        let dm = DeliveredMessage {
            platform_id: "p".into(),
            thread_id: None,
            message: outbound("x"),
        };
        let _ = dm.clone();
        assert!(format!("{dm:?}").contains("platform_id"));
    }
}
