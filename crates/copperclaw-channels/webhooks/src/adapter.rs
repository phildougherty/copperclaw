//! [`ChannelAdapter`] for the webhooks channel.
//!
//! The webhooks channel is intentionally inbound-only:
//!
//! - `subscribe` is a no-op (the axum server is already listening on
//!   bind; per-platform subscription doesn't apply).
//! - `set_typing` is a no-op.
//! - `deliver` returns [`AdapterError::Unsupported`] — webhooks don't
//!   have a reply address. Agents that need to "respond" to a webhook
//!   event should pick another channel (HTTP via an outbound module,
//!   email via resend, etc.) configured separately.
//! - `open_dm` returns `None`.
//!
//! The adapter owns the server task handle from the factory so it can
//! abort it cleanly during shutdown.

use async_trait::async_trait;
use copperclaw_channels_core::{AdapterError, ChannelAdapter};
use copperclaw_types::{ChannelType, OutboundMessage};
use std::sync::Mutex;
use tokio::task::JoinHandle;

/// Webhooks adapter.
pub struct WebhooksAdapter {
    channel_type: ChannelType,
    server: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for WebhooksAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let server_state = self
            .server
            .lock()
            .map_or("poisoned", |g| g.as_ref().map_or("stopped", |_| "running"));
        f.debug_struct("WebhooksAdapter")
            .field("channel_type", &self.channel_type)
            .field("server", &server_state)
            .finish()
    }
}

impl WebhooksAdapter {
    /// Build an adapter without a running server. The factory calls
    /// [`Self::set_server_handle`] right after spawning.
    #[must_use]
    pub fn new(channel_type: ChannelType) -> Self {
        Self {
            channel_type,
            server: Mutex::new(None),
        }
    }

    /// Stash the join handle for the spawned axum task. Called once by
    /// the factory; subsequent calls drop any previously-attached handle
    /// (we don't expect that path but it stays safe).
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        if let Ok(mut slot) = self.server.lock() {
            *slot = Some(handle);
        }
    }

    /// Stop the background server. Idempotent; no-op when no handle is
    /// attached.
    pub fn abort_server(&self) {
        if let Ok(mut slot) = self.server.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }
    }
}

impl Drop for WebhooksAdapter {
    fn drop(&mut self) {
        self.abort_server();
    }
}

#[async_trait]
impl ChannelAdapter for WebhooksAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        false
    }

    async fn deliver(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
        _message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        Err(AdapterError::Unsupported(
            "webhooks channel is inbound-only; configure a separate outbound channel for replies"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::MessageKind;
    use serde_json::json;
    use tokio::time::{Duration, sleep};

    fn msg() -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "hi"}),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn channel_type_returns_configured_value() {
        let a = WebhooksAdapter::new(ChannelType::new("webhooks"));
        assert_eq!(a.channel_type().as_str(), "webhooks");
    }

    #[tokio::test]
    async fn deliver_is_unsupported() {
        let a = WebhooksAdapter::new(ChannelType::new("webhooks"));
        let err = a.deliver("plat", None, &msg()).await.unwrap_err();
        match err {
            AdapterError::Unsupported(m) => assert!(m.contains("inbound-only")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn defaults_are_no_ops() {
        let a = WebhooksAdapter::new(ChannelType::new("webhooks"));
        a.subscribe("p", None).await.unwrap();
        a.set_typing("p", None).await.unwrap();
        assert!(a.open_dm("u").await.unwrap().is_none());
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn abort_server_is_idempotent() {
        let a = WebhooksAdapter::new(ChannelType::new("webhooks"));
        // First call: nothing attached, no-op.
        a.abort_server();
        // Now attach a real task and confirm it gets cancelled.
        let handle = tokio::spawn(async {
            sleep(Duration::from_secs(60)).await;
        });
        a.set_server_handle(handle);
        a.abort_server();
        // Calling again is still safe.
        a.abort_server();
    }

    #[tokio::test]
    async fn drop_cancels_running_server() {
        let handle = tokio::spawn(async {
            sleep(Duration::from_secs(60)).await;
        });
        let aborted_marker = handle.abort_handle();
        let a = WebhooksAdapter::new(ChannelType::new("webhooks"));
        a.set_server_handle(handle);
        drop(a);
        // `abort` schedules cancellation; the runtime needs at least one
        // yield to actually settle the task.
        for _ in 0..50 {
            if aborted_marker.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(aborted_marker.is_finished());
    }

    #[test]
    fn debug_format_renders() {
        let a = WebhooksAdapter::new(ChannelType::new("webhooks"));
        let s = format!("{a:?}");
        assert!(s.contains("WebhooksAdapter"));
    }
}
