//! `DeliveryDispatcher` implementation used by modules to push synthetic
//! outbound messages and typing indicators through the host's delivery path.
//!
//! Modules acquire a `Arc<dyn DeliveryDispatcher>` via
//! [`ModuleContext::on_delivery_adapter_ready`]. They can then ask the host to
//! emit typing indicators (best-effort) or dispatch fully-formed messages
//! without going through the `messages_out` table.

use copperclaw_channels_core::ChannelAdapter;
use copperclaw_modules::{DeliveryDispatcher, DispatchTarget};
use copperclaw_types::{ChannelType, OutboundMessage};
use std::sync::Arc;
use tokio::runtime::Handle;
use tracing::warn;

/// Looks up an adapter for a given channel type.
///
/// The host hands an adapter resolver to the dispatcher so modules can target
/// any registered channel without holding a reference to the registry. A
/// closure is more flexible than a snapshot map because it lets the host
/// register adapters lazily during boot.
pub type AdapterResolver =
    Arc<dyn Fn(&ChannelType) -> Option<Arc<dyn ChannelAdapter>> + Send + Sync>;

/// Implementation of `DeliveryDispatcher` for the host. Holds a reference to
/// the adapter resolver and spawns adapter calls on the current tokio runtime
/// so synchronous modules can fire off async work.
pub struct HostDispatcher {
    resolver: AdapterResolver,
    runtime: Handle,
}

impl HostDispatcher {
    /// Build a dispatcher that resolves adapters via `resolver` and spawns
    /// adapter calls on the current tokio runtime.
    pub fn new(resolver: AdapterResolver) -> Self {
        Self {
            resolver,
            runtime: Handle::current(),
        }
    }

    /// Test seam: build a dispatcher bound to an explicit runtime handle.
    /// This lets tests construct the dispatcher outside of an enclosing
    /// `#[tokio::test]` setup.
    pub fn with_runtime(resolver: AdapterResolver, runtime: Handle) -> Self {
        Self { resolver, runtime }
    }

    fn resolve(&self, target: &DispatchTarget) -> Option<Arc<dyn ChannelAdapter>> {
        target.channel_type.as_ref().and_then(|ct| (self.resolver)(ct))
    }
}

impl DeliveryDispatcher for HostDispatcher {
    fn set_typing(&self, target: &DispatchTarget) {
        let Some(adapter) = self.resolve(target) else {
            return;
        };
        let Some(platform_id) = target.platform_id.clone() else {
            return;
        };
        let thread_id = target.thread_id.clone();
        self.runtime.spawn(async move {
            if let Err(err) = adapter
                .set_typing(&platform_id, thread_id.as_deref())
                .await
            {
                warn!(?err, "dispatcher: set_typing failed");
            }
        });
    }

    fn dispatch(&self, target: &DispatchTarget, message: &OutboundMessage) {
        let Some(adapter) = self.resolve(target) else {
            warn!(
                channel = ?target.channel_type,
                "dispatcher: no adapter for synthetic dispatch"
            );
            return;
        };
        let Some(platform_id) = target.platform_id.clone() else {
            warn!("dispatcher: synthetic dispatch missing platform_id");
            return;
        };
        let thread_id = target.thread_id.clone();
        let message = message.clone();
        self.runtime.spawn(async move {
            if let Err(err) = adapter
                .deliver(&platform_id, thread_id.as_deref(), &message)
                .await
            {
                warn!(?err, "dispatcher: deliver failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::testing::MockAdapter;
    use copperclaw_channels_core::AdapterError;
    use copperclaw_types::{MessageKind, OutboundMessage};
    use serde_json::json;
    use std::sync::Mutex;

    fn outbound() -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "hi"}),
            files: vec![],
        }
    }

    fn target() -> DispatchTarget {
        DispatchTarget::channel(
            ChannelType::new("mock"),
            "platform-1".into(),
            Some("thread-1".into()),
        )
    }

    fn make_resolver(adapter: Arc<dyn ChannelAdapter>) -> AdapterResolver {
        let stored: Arc<Mutex<Option<Arc<dyn ChannelAdapter>>>> =
            Arc::new(Mutex::new(Some(adapter)));
        Arc::new(move |_ct| stored.lock().unwrap().clone())
    }

    fn empty_resolver() -> AdapterResolver {
        Arc::new(|_| None)
    }

    #[tokio::test]
    async fn dispatch_calls_adapter_deliver() {
        let mock: Arc<MockAdapter> = Arc::new(MockAdapter::new("mock"));
        let resolver = make_resolver(mock.clone() as Arc<dyn ChannelAdapter>);
        let dispatcher = HostDispatcher::new(resolver);
        dispatcher.dispatch(&target(), &outbound());
        // Allow spawned task to run.
        for _ in 0..50 {
            if !mock.deliveries().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(mock.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn dispatch_without_adapter_is_silent() {
        let dispatcher = HostDispatcher::new(empty_resolver());
        dispatcher.dispatch(&target(), &outbound());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    #[tokio::test]
    async fn dispatch_without_platform_id_is_silent() {
        let mock: Arc<MockAdapter> = Arc::new(MockAdapter::new("mock"));
        let resolver = make_resolver(mock.clone() as Arc<dyn ChannelAdapter>);
        let dispatcher = HostDispatcher::new(resolver);
        let mut t = target();
        t.platform_id = None;
        dispatcher.dispatch(&t, &outbound());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(mock.deliveries().is_empty());
    }

    #[tokio::test]
    async fn dispatch_without_channel_is_silent() {
        let mock: Arc<MockAdapter> = Arc::new(MockAdapter::new("mock"));
        let resolver = make_resolver(mock.clone() as Arc<dyn ChannelAdapter>);
        let dispatcher = HostDispatcher::new(resolver);
        let mut t = target();
        t.channel_type = None;
        dispatcher.dispatch(&t, &outbound());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(mock.deliveries().is_empty());
    }

    #[tokio::test]
    async fn set_typing_calls_adapter() {
        let mock: Arc<MockAdapter> = Arc::new(MockAdapter::new("mock"));
        let resolver = make_resolver(mock.clone() as Arc<dyn ChannelAdapter>);
        let dispatcher = HostDispatcher::new(resolver);
        dispatcher.set_typing(&target());
        // MockAdapter doesn't track typing, but the call should not panic.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    #[tokio::test]
    async fn set_typing_without_adapter_is_silent() {
        let dispatcher = HostDispatcher::new(empty_resolver());
        dispatcher.set_typing(&target());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    #[tokio::test]
    async fn set_typing_without_platform_id_is_silent() {
        let mock: Arc<MockAdapter> = Arc::new(MockAdapter::new("mock"));
        let resolver = make_resolver(mock.clone() as Arc<dyn ChannelAdapter>);
        let dispatcher = HostDispatcher::new(resolver);
        let mut t = target();
        t.platform_id = None;
        dispatcher.set_typing(&t);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    #[tokio::test]
    async fn deliver_failure_logs_and_does_not_panic() {
        let mock: Arc<MockAdapter> = Arc::new(MockAdapter::new("mock"));
        mock.fail_next_deliver(AdapterError::Transport("nope".into()));
        let resolver = make_resolver(mock.clone() as Arc<dyn ChannelAdapter>);
        let dispatcher = HostDispatcher::new(resolver);
        dispatcher.dispatch(&target(), &outbound());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        // Failure was consumed; no panic.
    }

    #[tokio::test]
    async fn with_runtime_constructs() {
        let mock: Arc<MockAdapter> = Arc::new(MockAdapter::new("mock"));
        let resolver = make_resolver(mock.clone() as Arc<dyn ChannelAdapter>);
        let dispatcher = HostDispatcher::with_runtime(resolver, Handle::current());
        dispatcher.dispatch(&target(), &outbound());
        for _ in 0..50 {
            if !mock.deliveries().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(mock.deliveries().len(), 1);
    }
}
