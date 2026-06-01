//! [`ChannelAdapter`] for LINE.
//!
//! LINE distinguishes "reply" (free, requires a token from a recent
//! inbound webhook, ~30s TTL) from "push" (paid, anytime). The
//! adapter caches reply tokens by `platform_id` during ingress so
//! `deliver` can prefer the cheap path. The cache is one-shot:
//! consuming a token removes it.
//!
//! Outbound supports plain text only. Reactions / images / stickers
//! would each need their own message-object shape; that's a
//! follow-up. An outbound with files surfaces an explicit
//! [`AdapterError::Unsupported`] rather than silently dropping the
//! attachment.

use crate::api::LineApi;
use async_trait::async_trait;
use copperclaw_channels_core::{AdapterError, ChannelAdapter};
use copperclaw_types::{ChannelType, OutboundMessage};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;

/// Tiny FIFO of `(platform_id -> latest_reply_token)`. Reply tokens
/// are single-use and expire ~30s after the inbound event LINE
/// stamped them on, so we just keep the most-recent one per source
/// and consume on send.
#[derive(Debug, Default)]
pub struct ReplyTokenCache {
    inner: Mutex<HashMap<String, String>>,
}

impl ReplyTokenCache {
    /// Build a fresh empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the most-recent reply token for `platform_id`. Overwrites
    /// any previously-stored token.
    pub fn put(&self, platform_id: String, token: String) {
        if let Ok(mut g) = self.lock_inner() {
            g.insert(platform_id, token);
        }
    }

    /// Consume the stored token for `platform_id` (if any).
    pub fn take(&self, platform_id: &String) -> Option<String> {
        self.lock_inner()
            .ok()
            .and_then(|mut g| g.remove(platform_id))
    }

    fn lock_inner(&self) -> Result<MutexGuard<'_, HashMap<String, String>>, ()> {
        self.inner.lock().map_err(|_| ())
    }
}

/// LINE adapter. Holds the REST client, the shared reply-token cache
/// (also handed to the webhook router), and the join handle for the
/// outgoing-webhook server so we can cancel on drop.
pub struct LineAdapter {
    channel_type: ChannelType,
    api: LineApi,
    reply_tokens: std::sync::Arc<ReplyTokenCache>,
    server: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for LineAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let server_state = self
            .server
            .lock()
            .map_or("poisoned", |g| g.as_ref().map_or("stopped", |_| "running"));
        // `api` carries the bearer token; render a placeholder.
        f.debug_struct("LineAdapter")
            .field("channel_type", &self.channel_type)
            .field("api", &"<LineApi>")
            .field("reply_tokens", &"<ReplyTokenCache>")
            .field("server", &server_state)
            .finish()
    }
}

impl LineAdapter {
    /// Build a new adapter. The factory shares its
    /// [`ReplyTokenCache`] with the webhook router so inbound and
    /// outbound see the same tokens.
    #[must_use]
    pub fn new(
        channel_type: ChannelType,
        api: LineApi,
        reply_tokens: std::sync::Arc<ReplyTokenCache>,
    ) -> Self {
        Self {
            channel_type,
            api,
            reply_tokens,
            server: Mutex::new(None),
        }
    }

    /// Attach the webhook server's join handle.
    pub fn set_server_handle(&self, handle: JoinHandle<()>) {
        if let Ok(mut slot) = self.server.lock() {
            *slot = Some(handle);
        }
    }

    /// Abort the background webhook server; idempotent.
    pub fn abort_server(&self) {
        if let Ok(mut slot) = self.server.lock() {
            if let Some(handle) = slot.take() {
                handle.abort();
            }
        }
    }
}

impl Drop for LineAdapter {
    fn drop(&mut self) {
        self.abort_server();
    }
}

#[async_trait]
impl ChannelAdapter for LineAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    /// LINE `Messaging API` text messages cap at 5 000 chars.
    fn max_message_chars(&self) -> Option<usize> {
        Some(5000)
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        if !message.files.is_empty() {
            return Err(AdapterError::Unsupported(
                "line file uploads not implemented yet".into(),
            ));
        }
        let action = message
            .content
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("post");
        if action != "post" {
            return Err(AdapterError::BadRequest(format!(
                "unsupported line action: {action}"
            )));
        }
        let text = message
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AdapterError::BadRequest("missing `text` in outbound content".into()))?;

        if let Some(token) = self.reply_tokens.take(&platform_id.to_string()) {
            // Reply path: free but single-use. If the token is stale
            // (LINE returns 400 with `invalid reply token`) we still
            // surface BadRequest — the caller can retry with a fresh
            // inbound, or future work can fall back to push here.
            self.api.reply(&token, text).await?;
        } else {
            self.api.push(platform_id, text).await?;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::MessageKind;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::time::{Duration, sleep};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn outbound(text: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": text}),
            files: vec![],
        }
    }

    fn make(server: &MockServer) -> (LineAdapter, Arc<ReplyTokenCache>) {
        let api = LineApi::new(&server.uri(), "tok");
        let cache = Arc::new(ReplyTokenCache::new());
        let a = LineAdapter::new(ChannelType::new("line"), api, cache.clone());
        (a, cache)
    }

    #[tokio::test]
    async fn deliver_prefers_reply_when_token_present() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/reply"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&mock)
            .await;
        let (a, cache) = make(&mock);
        cache.put("U1".into(), "rt-1".into());
        let id = a.deliver("U1", None, &outbound("hi")).await.unwrap();
        assert!(id.is_none());
        // Token should be consumed.
        assert!(cache.take(&"U1".to_string()).is_none());
    }

    #[tokio::test]
    async fn deliver_falls_back_to_push_without_token() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/bot/message/push"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&mock)
            .await;
        let (a, _cache) = make(&mock);
        let id = a.deliver("U1", None, &outbound("hi")).await.unwrap();
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn deliver_without_text_is_bad_request() {
        let mock = MockServer::start().await;
        let (a, _cache) = make(&mock);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        assert!(matches!(
            a.deliver("U1", None, &msg).await.unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_bad_request() {
        let mock = MockServer::start().await;
        let (a, _cache) = make(&mock);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"action":"react","text":"x"}),
            files: vec![],
        };
        assert!(matches!(
            a.deliver("U1", None, &msg).await.unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn deliver_files_unsupported_for_now() {
        let mock = MockServer::start().await;
        let (a, _cache) = make(&mock);
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text":"hi"}),
            files: vec![copperclaw_types::OutboundFile {
                filename: "a.png".into(),
                data: vec![0; 3],
            }],
        };
        match a.deliver("U1", None, &msg).await.unwrap_err() {
            AdapterError::Unsupported(m) => assert!(m.contains("file uploads")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn defaults_are_no_ops() {
        let mock = MockServer::start().await;
        let (a, _cache) = make(&mock);
        a.subscribe("p", None).await.unwrap();
        a.set_typing("p", None).await.unwrap();
        assert!(a.open_dm("u").await.unwrap().is_none());
        assert!(!a.supports_threads());
    }

    #[tokio::test]
    async fn drop_cancels_running_server() {
        let mock = MockServer::start().await;
        let handle = tokio::spawn(async {
            sleep(Duration::from_secs(60)).await;
        });
        let marker = handle.abort_handle();
        let (a, _) = make(&mock);
        a.set_server_handle(handle);
        drop(a);
        for _ in 0..50 {
            if marker.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(marker.is_finished());
    }

    #[test]
    fn debug_format_renders() {
        let api = LineApi::new("https://api.line.me", "t");
        let a = LineAdapter::new(
            ChannelType::new("line"),
            api,
            Arc::new(ReplyTokenCache::new()),
        );
        let s = format!("{a:?}");
        assert!(s.contains("LineAdapter"));
    }

    #[test]
    fn reply_token_cache_round_trip() {
        let cache = ReplyTokenCache::new();
        cache.put("U1".into(), "rt".into());
        assert_eq!(cache.take(&"U1".to_string()).as_deref(), Some("rt"));
        assert!(cache.take(&"U1".to_string()).is_none());
    }

    #[test]
    fn reply_token_cache_overwrites_latest() {
        let cache = ReplyTokenCache::new();
        cache.put("U".into(), "old".into());
        cache.put("U".into(), "new".into());
        assert_eq!(cache.take(&"U".to_string()).as_deref(), Some("new"));
    }
}
