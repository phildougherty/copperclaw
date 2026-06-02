//! [`EmacsAdapter`] — `ChannelAdapter` for a long-running Emacs daemon.
//!
//! See the crate-level docs for the elisp contract.

use crate::client::EmacsClient;
use crate::config::EmacsConfig;
use crate::sexp::{self, SexpValue};
use async_trait::async_trait;
use chrono::Utc;
use copperclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, OutboundMessage, SenderIdentity,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// `ChannelType` string used by this channel (`"emacs"`).
pub const CHANNEL_TYPE_STR: &str = "emacs";

/// Placeholder token replaced with a JSON-encoded buffer name in
/// [`EmacsConfig::outbound_sexp_template`](crate::config::EmacsConfig).
pub const TOKEN_BUFFER_JSON: &str = "${BUFFER_JSON}";

/// Placeholder token replaced with a JSON-encoded message body in
/// [`EmacsConfig::outbound_sexp_template`](crate::config::EmacsConfig).
pub const TOKEN_TEXT_JSON: &str = "${TEXT_JSON}";

/// Emacs channel adapter.
///
/// Owns the [`EmacsClient`] used to evaluate elisp forms, the
/// [`CancellationToken`] used to stop the poll task, and the handle to the
/// poll task itself.
pub struct EmacsAdapter {
    channel_type: ChannelType,
    client: Arc<dyn EmacsClient>,
    config: EmacsConfig,
    cancel: CancellationToken,
    poll_handle: Mutex<Option<JoinHandle<()>>>,
}

impl EmacsAdapter {
    /// Construct an adapter and spawn the background poll task.
    pub fn start(
        client: Arc<dyn EmacsClient>,
        config: EmacsConfig,
        inbound_tx: Sender<InboundEvent>,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(poll_loop(
            client.clone(),
            config.clone(),
            inbound_tx,
            cancel.clone(),
        ));
        Arc::new(Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            client,
            config,
            cancel,
            poll_handle: Mutex::new(Some(handle)),
        })
    }

    /// Stop the poll task. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.poll_handle.lock().await.take() {
            let _ = handle.await;
        }
    }

    /// Borrow the resolved config.
    pub fn config(&self) -> &EmacsConfig {
        &self.config
    }
}

impl std::fmt::Debug for EmacsAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmacsAdapter")
            .field("channel_type", &self.channel_type)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChannelAdapter for EmacsAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        // System actions arrive as kind=System with an `action` discriminator.
        if message.kind == MessageKind::System {
            if let Some(action) = message.content.get("action").and_then(Value::as_str) {
                return Err(AdapterError::Unsupported(format!(
                    "emacs channel does not support system action `{action}`"
                )));
            }
        }
        // Files are intentionally unsupported in v1: Emacs file insertion is
        // too platform-specific (where to put the file, what to do with it,
        // etc.).
        if !message.files.is_empty() {
            return Err(AdapterError::Unsupported(
                "emacs channel does not support file attachments in v1".into(),
            ));
        }

        let text = extract_text(&message.content);
        let buffer = if platform_id.is_empty() {
            self.config.default_buffer.as_str()
        } else {
            platform_id
        };
        let sexp = render_outbound(&self.config.outbound_sexp_template, buffer, &text);
        // Discard stdout: we don't surface a platform-side message id.
        let _ = self.client.eval(&sexp).await?;
        Ok(None)
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }
}

/// Substitute the well-known `${...}` tokens into the outbound template.
///
/// Both `buffer` and `text` are inserted as JSON-encoded strings — this
/// produces a syntactically valid elisp string (elisp's printed string form
/// is a superset of JSON's that handles all the same escapes, plus a few
/// more that we don't need to emit).
pub fn render_outbound(template: &str, buffer: &str, text: &str) -> String {
    let buffer_json = json!(buffer).to_string();
    let text_json = json!(text).to_string();
    template
        .replace(TOKEN_BUFFER_JSON, &buffer_json)
        .replace(TOKEN_TEXT_JSON, &text_json)
}

fn extract_text(value: &Value) -> String {
    value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Background poll loop. Runs until `cancel` fires or `inbound_tx` is
/// dropped.
async fn poll_loop(
    client: Arc<dyn EmacsClient>,
    config: EmacsConfig,
    inbound_tx: Sender<InboundEvent>,
    cancel: CancellationToken,
) {
    let interval = std::time::Duration::from_millis(config.poll_interval_ms);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(interval) => {}
        }

        let raw = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            r = client.eval(&config.inbound_queue_sexp) => r,
        };

        match raw {
            Ok(stdout) => {
                let trimmed = stdout.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match sexp::parse(trimmed) {
                    Ok(SexpValue::Nil) => continue,
                    Ok(SexpValue::Alist(pairs)) => {
                        match build_inbound_from_pairs(&pairs) {
                            Some(event) => {
                                if inbound_tx.send(event).await.is_err() {
                                    // Receiver dropped; host is shutting
                                    // down. Exit cleanly.
                                    break;
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "emacs channel: alist missing required `buffer` or `text` keys"
                                );
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "emacs channel: malformed sexp from inbound queue");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "emacs channel: inbound poll failed");
            }
        }
    }
}

/// Build an [`InboundEvent`] from a parsed alist, or return `None` if the
/// required keys are missing.
pub fn build_inbound_from_pairs(pairs: &[(String, String)]) -> Option<InboundEvent> {
    let mut buffer: Option<&str> = None;
    let mut text: Option<&str> = None;
    let mut sender: Option<&str> = None;
    let mut id: Option<&str> = None;
    for (k, v) in pairs {
        match k.as_str() {
            "buffer" => buffer = Some(v),
            "text" => text = Some(v),
            "sender" => sender = Some(v),
            "id" => id = Some(v),
            _ => {}
        }
    }
    let buffer = buffer?;
    let text = text?;
    let message_id = id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
    Some(InboundEvent {
        channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        platform_id: buffer.to_owned(),
        thread_id: None,
        message: InboundMessage {
            id: message_id,
            kind: MessageKind::Chat,
            content: json!({ "text": text }),
            timestamp: Utc::now(),
            is_mention: None,
            is_group: Some(false),
        },
        reply_to: None,
        sender: sender.map(|s| SenderIdentity {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            identity: s.to_owned(),
            display_name: Some(s.to_owned()),
        }),
    })
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::client::MockEmacsClient;
    use copperclaw_types::OutboundFile;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    fn outbound_text(text: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": text }),
            files: vec![],
        }
    }

    fn make_adapter(
        mock: Arc<MockEmacsClient>,
        cfg: EmacsConfig,
    ) -> (Arc<EmacsAdapter>, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let adapter = EmacsAdapter::start(mock, cfg, tx);
        (adapter, rx)
    }

    #[tokio::test]
    async fn channel_type_is_emacs() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        assert_eq!(adapter.channel_type().as_str(), "emacs");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn supports_threads_defaults_false() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        assert!(!adapter.supports_threads());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_invokes_template_with_buffer_and_text() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock.clone(), EmacsConfig::default());
        let id = adapter
            .deliver("*chat*", None, &outbound_text("hello"))
            .await
            .unwrap();
        assert!(id.is_none());
        // Two calls so far: the deliver call, plus possibly a poll call.
        // We assert the deliver call appears.
        let calls = mock.calls();
        let deliver = calls
            .iter()
            .find(|s| s.starts_with("(copperclaw-deliver"))
            .expect("deliver sexp present");
        assert!(deliver.contains("\"*chat*\""));
        assert!(deliver.contains("\"hello\""));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_escapes_quotes_in_text() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock.clone(), EmacsConfig::default());
        adapter
            .deliver("*chat*", None, &outbound_text("she said \"hi\""))
            .await
            .unwrap();
        let calls = mock.calls();
        let deliver = calls
            .iter()
            .find(|s| s.starts_with("(copperclaw-deliver"))
            .expect("deliver sexp present");
        // JSON-encoded: "she said \"hi\""
        assert!(deliver.contains("\"she said \\\"hi\\\"\""));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_escapes_newline_in_text() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock.clone(), EmacsConfig::default());
        adapter
            .deliver("*chat*", None, &outbound_text("line1\nline2"))
            .await
            .unwrap();
        let calls = mock.calls();
        let deliver = calls
            .iter()
            .find(|s| s.starts_with("(copperclaw-deliver"))
            .expect("deliver sexp present");
        assert!(deliver.contains("line1\\nline2"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_uses_default_buffer_when_platform_id_empty() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let mut cfg = EmacsConfig::default();
        cfg.default_buffer = "*fallback*".into();
        let (adapter, _rx) = make_adapter(mock.clone(), cfg);
        adapter
            .deliver("", None, &outbound_text("hi"))
            .await
            .unwrap();
        let calls = mock.calls();
        let deliver = calls
            .iter()
            .find(|s| s.starts_with("(copperclaw-deliver"))
            .expect("deliver sexp present");
        assert!(deliver.contains("\"*fallback*\""));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_to_non_default_buffer_uses_platform_id() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock.clone(), EmacsConfig::default());
        adapter
            .deliver("*other-buf*", None, &outbound_text("hi"))
            .await
            .unwrap();
        let calls = mock.calls();
        assert!(calls.iter().any(|s| s.contains("\"*other-buf*\"")));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_files_returns_unsupported() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": "see attached" }),
            files: vec![OutboundFile {
                filename: "a.txt".into(),
                data: vec![1, 2, 3],
            }],
        };
        let err = adapter.deliver("*chat*", None, &msg).await.unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_system_edit_returns_unsupported() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({ "action": "edit", "target_seq": 1, "text": "x" }),
            files: vec![],
        };
        let err = adapter.deliver("*chat*", None, &msg).await.unwrap_err();
        match err {
            AdapterError::Unsupported(m) => assert!(m.contains("edit")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_system_reaction_returns_unsupported() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({ "action": "reaction", "target_seq": 1, "emoji": "thumbsup" }),
            files: vec![],
        };
        let err = adapter.deliver("*chat*", None, &msg).await.unwrap_err();
        match err {
            AdapterError::Unsupported(m) => assert!(m.contains("reaction")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_propagates_transport_error_from_client() {
        let mock = Arc::new(MockEmacsClient::new());
        mock.push_err(AdapterError::Transport("boom".into()));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        let err = adapter
            .deliver("*chat*", None, &outbound_text("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_propagates_auth_error_from_client() {
        let mock = Arc::new(MockEmacsClient::new());
        mock.push_err(AdapterError::Auth("no server".into()));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        let err = adapter
            .deliver("*chat*", None, &outbound_text("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_missing_text_sends_empty_string() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock.clone(), EmacsConfig::default());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        adapter.deliver("*chat*", None, &msg).await.unwrap();
        let calls = mock.calls();
        assert!(
            calls
                .iter()
                .any(|s| s.starts_with("(copperclaw-deliver") && s.contains("\"\""))
        );
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_default_is_ok() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        adapter.subscribe("*chat*", None).await.unwrap();
        adapter.subscribe("*chat*", Some("t")).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn set_typing_default_is_ok() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        adapter.set_typing("*chat*", None).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        assert!(adapter.open_dm("anyone").await.unwrap().is_none());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn poll_emits_inbound_event_when_alist_returned() {
        let mock = Arc::new(MockEmacsClient::new());
        // First poll returns a message, then nil forever.
        mock.push_ok("((\"buffer\" . \"*chat*\") (\"text\" . \"hi\") (\"sender\" . \"alice\"))\n");
        mock.push_ok("nil\n");
        let mut cfg = EmacsConfig::default();
        cfg.poll_interval_ms = 5;
        let (adapter, mut rx) = make_adapter(mock, cfg);
        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("did not receive event in time")
            .expect("sender closed");
        assert_eq!(evt.channel_type.as_str(), "emacs");
        assert_eq!(evt.platform_id, "*chat*");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hi");
        let sender = evt.sender.expect("sender present");
        assert_eq!(sender.identity, "alice");
        assert_eq!(evt.message.is_group, Some(false));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn poll_does_not_emit_for_nil() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let mut cfg = EmacsConfig::default();
        cfg.poll_interval_ms = 5;
        let (adapter, mut rx) = make_adapter(mock, cfg);
        let r = timeout(Duration::from_millis(120), rx.recv()).await;
        assert!(r.is_err(), "expected no events for nil polls");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn poll_tolerates_malformed_sexp() {
        let mock = Arc::new(MockEmacsClient::new());
        mock.push_ok("garbage\n");
        mock.push_ok("((\"buffer\" . \"*chat*\") (\"text\" . \"after-recovery\"))\n");
        mock.push_ok("nil\n");
        let mut cfg = EmacsConfig::default();
        cfg.poll_interval_ms = 5;
        let (adapter, mut rx) = make_adapter(mock, cfg);
        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("did not receive event in time")
            .expect("sender closed");
        assert_eq!(evt.message.content["text"], "after-recovery");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn poll_tolerates_client_error() {
        let mock = Arc::new(MockEmacsClient::new());
        mock.push_err(AdapterError::Transport("transient".into()));
        mock.push_ok("((\"buffer\" . \"*chat*\") (\"text\" . \"recovered\"))\n");
        mock.push_ok("nil\n");
        let mut cfg = EmacsConfig::default();
        cfg.poll_interval_ms = 5;
        let (adapter, mut rx) = make_adapter(mock, cfg);
        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("did not receive event in time")
            .expect("sender closed");
        assert_eq!(evt.message.content["text"], "recovered");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn poll_alist_missing_required_keys_is_dropped() {
        let mock = Arc::new(MockEmacsClient::new());
        mock.push_ok("((\"sender\" . \"alice\"))\n"); // no buffer or text
        mock.push_ok("nil\n");
        let mut cfg = EmacsConfig::default();
        cfg.poll_interval_ms = 5;
        let (adapter, mut rx) = make_adapter(mock, cfg);
        let r = timeout(Duration::from_millis(120), rx.recv()).await;
        assert!(r.is_err(), "must not emit when required keys missing");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn poll_passes_through_id_when_present() {
        let mock = Arc::new(MockEmacsClient::new());
        mock.push_ok("((\"id\" . \"abc-123\") (\"buffer\" . \"*chat*\") (\"text\" . \"hi\"))\n");
        mock.push_ok("nil\n");
        let mut cfg = EmacsConfig::default();
        cfg.poll_interval_ms = 5;
        let (adapter, mut rx) = make_adapter(mock, cfg);
        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("did not receive event in time")
            .expect("sender closed");
        assert_eq!(evt.message.id, "abc-123");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn poll_respects_cancellation() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let mut cfg = EmacsConfig::default();
        cfg.poll_interval_ms = 1000; // long, so we test cancellation path.
        let (adapter, _rx) = make_adapter(mock, cfg);
        // Issue shutdown; the poll task is in sleep() and should exit via
        // the cancellation select arm.
        let start = std::time::Instant::now();
        adapter.shutdown().await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "shutdown took too long; cancellation likely not honored"
        );
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        adapter.shutdown().await;
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn debug_renders_struct_name() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let (adapter, _rx) = make_adapter(mock, EmacsConfig::default());
        let s = format!("{adapter:?}");
        assert!(s.contains("EmacsAdapter"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn config_accessor_returns_input_config() {
        let mock = Arc::new(MockEmacsClient::always_ok("nil\n"));
        let mut cfg = EmacsConfig::default();
        cfg.default_buffer = "*custom*".into();
        let (adapter, _rx) = make_adapter(mock, cfg.clone());
        assert_eq!(adapter.config(), &cfg);
        adapter.shutdown().await;
    }

    #[test]
    fn render_outbound_substitutes_both_tokens() {
        let s = render_outbound("(d ${BUFFER_JSON} ${TEXT_JSON})", "*chat*", "hi");
        assert_eq!(s, "(d \"*chat*\" \"hi\")");
    }

    #[test]
    fn render_outbound_handles_missing_token_gracefully() {
        // If a user customises the template and forgets one of the tokens,
        // the substituter just leaves the other one in place. Not an error.
        let s = render_outbound("(d ${BUFFER_JSON})", "*chat*", "ignored");
        assert_eq!(s, "(d \"*chat*\")");
    }

    #[test]
    fn render_outbound_handles_no_tokens_at_all() {
        // Constant sexp templates are also valid — useful for triggers.
        let s = render_outbound("(d)", "*chat*", "hi");
        assert_eq!(s, "(d)");
    }

    #[test]
    fn extract_text_returns_empty_when_missing() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({"text": 1})), "");
        assert_eq!(extract_text(&json!({"text": "x"})), "x");
    }

    #[test]
    fn build_inbound_from_pairs_requires_buffer() {
        let pairs = vec![("text".to_string(), "hi".into())];
        assert!(build_inbound_from_pairs(&pairs).is_none());
    }

    #[test]
    fn build_inbound_from_pairs_requires_text() {
        let pairs = vec![("buffer".to_string(), "*chat*".into())];
        assert!(build_inbound_from_pairs(&pairs).is_none());
    }

    #[test]
    fn build_inbound_from_pairs_generates_uuid_when_id_absent() {
        let pairs = vec![
            ("buffer".to_string(), "*chat*".into()),
            ("text".into(), "hi".into()),
        ];
        let evt = build_inbound_from_pairs(&pairs).expect("event");
        assert!(!evt.message.id.is_empty());
        assert!(evt.sender.is_none());
    }

    #[test]
    fn channel_type_str_constant() {
        assert_eq!(CHANNEL_TYPE_STR, "emacs");
    }

    #[test]
    fn token_constants_match_default_template() {
        assert!(crate::config::DEFAULT_OUTBOUND_SEXP_TEMPLATE.contains(TOKEN_BUFFER_JSON));
        assert!(crate::config::DEFAULT_OUTBOUND_SEXP_TEMPLATE.contains(TOKEN_TEXT_JSON));
    }
}
