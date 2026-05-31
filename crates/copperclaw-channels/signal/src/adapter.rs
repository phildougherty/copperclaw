//! [`SignalAdapter`] — the [`ChannelAdapter`] implementation for Signal.
//!
//! Owns an [`RpcTransport`] (production: [`crate::rpc::JsonRpcClient`]; in
//! tests: [`crate::rpc::MockTransport`]) and a background forwarder task
//! that drains the daemon's notification stream and pushes parsed
//! [`InboundEvent`]s to the host's `inbound_tx`.
//!
//! Outbound shapes:
//!
//! - Plain text: `OutboundMessage.content = { "text": "..." }`.
//! - With attachments: same plus `OutboundMessage.files`. Each file is
//!   written to `data_dir/outbox/<uuid>/<safe-filename>` and the path is
//!   passed to signal-cli via `attachment`.
//! - System action `edit`: `{ "action": "edit", "target_id": "<ts>",
//!   "text": "..." }`. `target_id` carries the original `targetSentTimestamp`.
//! - System action `reaction`: `{ "action": "reaction", "target_id": "<ts>",
//!   "target_author": "+15551112222", "emoji": "...", "remove": false }`.
//! - System action `delete`: `{ "action": "delete", "target_id": "<ts>" }`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use copperclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use copperclaw_types::{ChannelType, InboundEvent, OutboundFile, OutboundMessage};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api;
use crate::api::SendTarget;
use crate::factory::CHANNEL_TYPE_STR;
use crate::parse::params_to_inbound;
use crate::rpc::{Notification, RpcTransport};

/// Subdirectory of `data_dir` used to stage outbound attachments before
/// passing their paths to signal-cli.
pub const OUTBOX_SUBDIR: &str = "outbox";

/// Maximum length of an attachment filename, in bytes.
pub const MAX_ATTACHMENT_NAME_LEN: usize = 255;

/// Signal channel adapter.
pub struct SignalAdapter {
    channel_type: ChannelType,
    transport: Arc<dyn RpcTransport>,
    data_dir: PathBuf,
    forwarder_task: Mutex<Option<JoinHandle<()>>>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for SignalAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalAdapter")
            .field("channel_type", &self.channel_type)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

impl SignalAdapter {
    /// Construct an adapter with the supplied transport, persistent data
    /// directory, and inbound channel. Spawns a background forwarder task
    /// that translates `receive` notifications into [`InboundEvent`]s.
    ///
    /// This constructor is the one tests use directly (with
    /// [`crate::rpc::MockTransport`]); the production wiring lives in
    /// [`crate::factory::SignalFactory::init`].
    pub async fn with_transport(
        transport: Arc<dyn RpcTransport>,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Self {
        let cancel = CancellationToken::new();
        let notif_rx = transport.take_notifications().await;
        let handle = tokio::spawn(forwarder_loop(
            notif_rx,
            inbound_tx,
            cancel.clone(),
        ));
        Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            transport,
            data_dir,
            forwarder_task: Mutex::new(Some(handle)),
            cancel,
        }
    }

    /// Shared transport (for tests / introspection).
    pub fn transport(&self) -> &Arc<dyn RpcTransport> {
        &self.transport
    }

    /// Configured per-channel data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Stop the notification forwarder and wait for it to exit. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.forwarder_task.lock().await.take() {
            let _ = handle.await;
        }
    }

    /// Stage outbound files into `data_dir/outbox/<uuid>/<filename>` and
    /// return their paths in the same order as `files`.
    async fn stage_attachments(&self, files: &[OutboundFile]) -> Result<Vec<String>, AdapterError> {
        let mut paths = Vec::with_capacity(files.len());
        if files.is_empty() {
            return Ok(paths);
        }
        let bundle = self
            .data_dir
            .join(OUTBOX_SUBDIR)
            .join(uuid::Uuid::new_v4().to_string());
        tokio::fs::create_dir_all(&bundle)
            .await
            .map_err(AdapterError::Io)?;
        for f in files {
            let safe = safe_attachment_name(&f.filename)?;
            let path = bundle.join(&safe);
            tokio::fs::write(&path, &f.data)
                .await
                .map_err(AdapterError::Io)?;
            let s = path
                .to_str()
                .ok_or_else(|| {
                    AdapterError::BadRequest(format!(
                        "signal: attachment path is not valid UTF-8: {path:?}"
                    ))
                })?
                .to_owned();
            paths.push(s);
        }
        Ok(paths)
    }
}

/// Parse a `platform_id` of the form `"user:<e164>"` or `"group:<base64>"`
/// into a [`SendTarget`]. Malformed inputs surface as
/// [`AdapterError::BadRequest`].
pub fn parse_platform_id(platform_id: &str) -> Result<SendTarget, AdapterError> {
    if let Some(rest) = platform_id.strip_prefix("user:") {
        if rest.is_empty() {
            return Err(AdapterError::BadRequest(
                "signal: empty user e164 in platform_id".into(),
            ));
        }
        return Ok(SendTarget::Recipients(vec![rest.to_owned()]));
    }
    if let Some(rest) = platform_id.strip_prefix("group:") {
        if rest.is_empty() {
            return Err(AdapterError::BadRequest(
                "signal: empty group id in platform_id".into(),
            ));
        }
        return Ok(SendTarget::Group(rest.to_owned()));
    }
    Err(AdapterError::BadRequest(format!(
        "signal: platform_id must be `user:<e164>` or `group:<base64>`, got `{platform_id}`"
    )))
}

/// Validate an attachment filename and return the safe form (the filename
/// is used verbatim if it passes).
///
/// Rejects: paths containing `..`, any path separator (`/`, `\`), leading
/// dots, the empty string, or names longer than [`MAX_ATTACHMENT_NAME_LEN`].
pub fn safe_attachment_name(name: &str) -> Result<String, AdapterError> {
    if name.is_empty() {
        return Err(AdapterError::BadRequest(
            "signal: attachment filename is empty".into(),
        ));
    }
    if name.len() > MAX_ATTACHMENT_NAME_LEN {
        return Err(AdapterError::BadRequest(format!(
            "signal: attachment filename `{name}` exceeds {MAX_ATTACHMENT_NAME_LEN} bytes"
        )));
    }
    if name.contains("..") {
        return Err(AdapterError::BadRequest(format!(
            "signal: attachment filename `{name}` contains `..`"
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(AdapterError::BadRequest(format!(
            "signal: attachment filename `{name}` contains a path separator"
        )));
    }
    if name.starts_with('.') {
        return Err(AdapterError::BadRequest(format!(
            "signal: attachment filename `{name}` starts with a dot"
        )));
    }
    Ok(name.to_owned())
}

/// Background task: drain `notif_rx`, parse `receive` notifications, push
/// resulting [`InboundEvent`]s to `inbound_tx`. Exits on cancellation, when
/// the notification source closes, or when the inbound channel is closed.
async fn forwarder_loop(
    mut notif_rx: tokio::sync::mpsc::Receiver<Notification>,
    inbound_tx: Sender<InboundEvent>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("signal: forwarder cancelled");
                return;
            }
            recv = notif_rx.recv() => {
                let Some(notif) = recv else {
                    tracing::debug!("signal: notification stream closed");
                    return;
                };
                if notif.method != "receive" {
                    tracing::debug!(method = %notif.method, "signal: ignoring non-receive notification");
                    continue;
                }
                let Some(event) = params_to_inbound(&notif.params) else {
                    continue;
                };
                if let Err(err) = inbound_tx.send(event).await {
                    tracing::warn!(?err, "signal: inbound channel closed; stopping forwarder");
                    return;
                }
            }
        }
    }
}

/// Extract a string field from a JSON value, returning
/// [`AdapterError::BadRequest`] if missing.
fn required_str<'a>(content: &'a Value, key: &str) -> Result<&'a str, AdapterError> {
    content
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::BadRequest(format!("signal: missing `{key}`")))
}

/// Extract a `target_id` (signal-cli `targetSentTimestamp`) from action
/// content. Accepts string or integer.
fn required_timestamp(content: &Value, key: &str) -> Result<i64, AdapterError> {
    let v = content
        .get(key)
        .ok_or_else(|| AdapterError::BadRequest(format!("signal: missing `{key}`")))?;
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse::<i64>().map_err(|e| {
            AdapterError::BadRequest(format!("signal: `{key}` not a valid integer: {e}"))
        });
    }
    Err(AdapterError::BadRequest(format!(
        "signal: `{key}` must be an integer or string"
    )))
}

#[async_trait]
impl ChannelAdapter for SignalAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        false
    }

    async fn subscribe(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // The signal-cli daemon already streams everything when run with
        // `--receive-mode=on-start`. No per-conversation subscription is
        // needed.
        Ok(())
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        let target = parse_platform_id(platform_id)?;
        api::send_typing(&self.transport, &target, false).await
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let target = parse_platform_id(platform_id)?;

        if let Some(action) = message.content.get("action").and_then(Value::as_str) {
            return self.deliver_action(&target, action, &message.content).await;
        }

        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        if !message.files.is_empty() {
            let paths = self.stage_attachments(&message.files).await?;
            return api::send_with_attachments(&self.transport, &target, &text, &paths).await;
        }

        api::send_text(&self.transport, &target, &text).await
    }

    async fn open_dm(&self, user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Signal has no separate "create DM" call: any e164 with a Signal
        // account is reachable directly. We surface a handle so the host
        // can wire delivery.
        Ok(Some(DmHandle {
            user_id: user_id.to_owned(),
            platform_id: format!("user:{user_id}"),
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        }))
    }
}

impl SignalAdapter {
    async fn deliver_action(
        &self,
        target: &SendTarget,
        action: &str,
        content: &Value,
    ) -> Result<Option<String>, AdapterError> {
        match action {
            "edit" => {
                let ts = required_timestamp(content, "target_id")?;
                let text = required_str(content, "text")?;
                api::send_edit(&self.transport, target, ts, text).await
            }
            "reaction" => {
                let ts = required_timestamp(content, "target_id")?;
                let emoji = content
                    .get("emoji")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let author = required_str(content, "target_author")?;
                let remove = content
                    .get("remove")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                api::send_reaction(
                    &self.transport,
                    target,
                    &emoji,
                    author,
                    ts,
                    remove,
                )
                .await
            }
            "delete" => {
                let ts = required_timestamp(content, "target_id")?;
                api::remote_delete(&self.transport, target, ts).await
            }
            other => Err(AdapterError::Unsupported(format!(
                "signal action `{other}` is not supported"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{MockHandle, MockTransport, Notification};
    use copperclaw_types::MessageKind;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    async fn build_adapter() -> (Arc<SignalAdapter>, MockHandle, TempDir, mpsc::Receiver<InboundEvent>) {
        let (mock, ctl) = MockTransport::new();
        let dir = TempDir::new().unwrap();
        let (tx, rx) = mpsc::channel::<InboundEvent>(8);
        let arc: Arc<dyn RpcTransport> = Arc::new(mock);
        let adapter = SignalAdapter::with_transport(arc, tx, dir.path().to_path_buf()).await;
        (Arc::new(adapter), ctl, dir, rx)
    }

    #[tokio::test]
    async fn channel_type_is_signal_and_does_not_support_threads() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        assert_eq!(adapter.channel_type().as_str(), "signal");
        assert!(!adapter.supports_threads());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_to_user_calls_send_with_recipient() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("send", json!({"timestamp": 1700})).await;
        let id = adapter
            .deliver(
                "user:+15551234",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("1700"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "send");
        assert_eq!(calls[0].1["recipient"][0], "+15551234");
        assert_eq!(calls[0].1["message"], "hi");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_to_group_uses_group_id() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("send", json!({"timestamp": 42})).await;
        let id = adapter
            .deliver(
                "group:Z3JvdXA=",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hello group"}),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("42"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].1["groupId"], "Z3JvdXA=");
        assert!(calls[0].1.get("recipient").is_none());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_files_stages_and_sends_attachments() {
        let (adapter, ctl, dir, _rx) = build_adapter().await;
        ctl.expect_ok("send", json!({"timestamp": 7})).await;
        let id = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "see"}),
                    files: vec![OutboundFile {
                        filename: "doc.txt".into(),
                        data: b"hello".to_vec(),
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("7"));
        let calls = ctl.calls().await;
        let attachments = calls[0].1["attachment"].as_array().unwrap();
        assert_eq!(attachments.len(), 1);
        let path = attachments[0].as_str().unwrap();
        assert!(path.starts_with(dir.path().to_str().unwrap()));
        assert!(path.ends_with("doc.txt"));
        // File actually exists.
        let bytes = tokio::fs::read(path).await.unwrap();
        assert_eq!(bytes, b"hello");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_calls_send_edit_message() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("sendEditMessage", json!({"timestamp": 88})).await;
        let id = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "edit",
                        "target_id": 1700,
                        "text": "new body"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("88"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "sendEditMessage");
        assert_eq!(calls[0].1["targetSentTimestamp"], 1700);
        assert_eq!(calls[0].1["message"], "new body");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_accepts_string_target_id() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("sendEditMessage", json!({"timestamp": 88})).await;
        let _ = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "edit",
                        "target_id": "1700",
                        "text": "x"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls[0].1["targetSentTimestamp"], 1700);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_calls_send_reaction() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("sendReaction", json!({"timestamp": 5})).await;
        // Use ASCII placeholder for the emoji per project style guide.
        let id = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_id": 1700,
                        "target_author": "+2",
                        "emoji": "R",
                        "remove": false
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("5"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "sendReaction");
        assert_eq!(calls[0].1["emoji"], "R");
        assert_eq!(calls[0].1["targetAuthor"], "+2");
        assert_eq!(calls[0].1["targetSentTimestamp"], 1700);
        assert_eq!(calls[0].1["remove"], false);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_remove_defaults_false() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("sendReaction", json!({})).await;
        let _ = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_id": 1700,
                        "target_author": "+2",
                        "emoji": "X"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls[0].1["remove"], false);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_with_remove_true() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("sendReaction", json!({})).await;
        let _ = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_id": 1700,
                        "target_author": "+2",
                        "emoji": "",
                        "remove": true
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls[0].1["remove"], true);
        assert_eq!(calls[0].1["emoji"], "");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_delete_calls_remote_delete() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("remoteDelete", json!({"timestamp": 99})).await;
        let id = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "delete", "target_id": 1700}),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("99"));
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "remoteDelete");
        assert_eq!(calls[0].1["targetSentTimestamp"], 1700);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let err = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "ping"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_missing_target_id_errors() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let err = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "edit", "text": "x"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_missing_text_errors() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let err = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "edit", "target_id": 1700}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_missing_target_author_errors() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let err = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "reaction", "target_id": 1, "emoji": "X"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_bad_platform_id_errors() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let err = adapter
            .deliver(
                "telegram:123",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "x"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_attachment_with_unsafe_name_errors() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let err = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "see"}),
                    files: vec![OutboundFile {
                        filename: "../etc/passwd".into(),
                        data: vec![0],
                    }],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_propagates_rate_error() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_err(
            "send",
            crate::rpc::RpcError {
                code: -3,
                message: "RateLimitException".into(),
                data: None,
            },
        )
        .await;
        let err = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "x"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: None }));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_propagates_auth_error() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_err(
            "send",
            crate::rpc::RpcError {
                code: -1,
                message: "AuthorizationFailedException".into(),
                data: None,
            },
        )
        .await;
        let err = adapter
            .deliver(
                "user:+1",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "x"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_is_noop() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        adapter.subscribe("user:+1", None).await.unwrap();
        adapter.subscribe("group:G", Some("t")).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn set_typing_calls_send_typing_with_stop_false() {
        let (adapter, ctl, _dir, _rx) = build_adapter().await;
        ctl.expect_ok("sendTyping", json!({})).await;
        adapter.set_typing("user:+1", None).await.unwrap();
        let calls = ctl.calls().await;
        assert_eq!(calls[0].0, "sendTyping");
        assert_eq!(calls[0].1["stop"], false);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn set_typing_bad_platform_id_errors() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let err = adapter.set_typing("junk", None).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_returns_user_platform_id_handle() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let handle = adapter.open_dm("+15551112222").await.unwrap().unwrap();
        assert_eq!(handle.platform_id, "user:+15551112222");
        assert_eq!(handle.user_id, "+15551112222");
        assert_eq!(handle.channel_type.as_str(), "signal");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_pushes_inbound_for_receive_notification() {
        let (adapter, ctl, _dir, mut rx) = build_adapter().await;
        ctl.push_notification(Notification {
            method: "receive".into(),
            params: json!({
                "envelope": {
                    "source": "+15551112222",
                    "sourceName": "Alice",
                    "timestamp": 1_700_000_000_000_i64,
                    "dataMessage": {
                        "message": "hi",
                        "timestamp": 1_700_000_000_000_i64
                    }
                }
            }),
        })
        .await;
        let evt = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.platform_id, "user:+15551112222");
        assert_eq!(evt.message.content["text"], "hi");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_skips_non_receive_notification() {
        let (adapter, ctl, _dir, mut rx) = build_adapter().await;
        ctl.push_notification(Notification {
            method: "other".into(),
            params: json!({}),
        })
        .await;
        let res = timeout(Duration::from_millis(50), rx.recv()).await;
        match res {
            Err(_) => {}
            Ok(Some(evt)) => panic!("unexpected event: {evt:?}"),
            Ok(None) => panic!("inbound channel closed unexpectedly"),
        }
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_skips_receive_without_data_message() {
        let (adapter, ctl, _dir, mut rx) = build_adapter().await;
        ctl.push_notification(Notification {
            method: "receive".into(),
            params: json!({
                "envelope": {
                    "source": "+1",
                    "timestamp": 1,
                    "receiptMessage": {"when": 1}
                }
            }),
        })
        .await;
        let res = timeout(Duration::from_millis(50), rx.recv()).await;
        match res {
            Err(_) => {}
            Ok(Some(evt)) => panic!("unexpected event: {evt:?}"),
            Ok(None) => panic!("inbound channel closed unexpectedly"),
        }
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_exits_when_inbound_closed() {
        // Build adapter, drop the receiver, push a notification — forwarder
        // must observe the closed channel and stop without hanging.
        let (mock, ctl) = MockTransport::new();
        let dir = TempDir::new().unwrap();
        let (tx, rx) = mpsc::channel::<InboundEvent>(1);
        drop(rx);
        let arc: Arc<dyn RpcTransport> = Arc::new(mock);
        let adapter = SignalAdapter::with_transport(arc, tx, dir.path().to_path_buf()).await;
        ctl.push_notification(Notification {
            method: "receive".into(),
            params: json!({
                "envelope": {
                    "source": "+1",
                    "timestamp": 1,
                    "dataMessage": {"message": "hi"}
                }
            }),
        })
        .await;
        // shutdown waits for the forwarder; if it didn't exit, the test
        // would hang and time out.
        timeout(Duration::from_secs(2), adapter.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        adapter.shutdown().await;
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn debug_format_renders() {
        let (adapter, _ctl, _dir, _rx) = build_adapter().await;
        let s = format!("{adapter:?}");
        assert!(s.contains("SignalAdapter"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn transport_and_data_dir_accessors() {
        let (adapter, _ctl, dir, _rx) = build_adapter().await;
        let _ = adapter.transport();
        assert_eq!(adapter.data_dir(), dir.path());
        adapter.shutdown().await;
    }

    // ------- parse_platform_id helper tests -------

    #[test]
    fn parse_user_platform_id() {
        let t = parse_platform_id("user:+15551234").unwrap();
        match t {
            SendTarget::Recipients(rs) => assert_eq!(rs, vec!["+15551234"]),
            SendTarget::Group(g) => panic!("unexpected group: {g}"),
        }
    }

    #[test]
    fn parse_group_platform_id() {
        let t = parse_platform_id("group:Z3JvdXA=").unwrap();
        match t {
            SendTarget::Group(g) => assert_eq!(g, "Z3JvdXA="),
            SendTarget::Recipients(rs) => panic!("unexpected recipients: {rs:?}"),
        }
    }

    #[test]
    fn parse_unknown_prefix_errors() {
        let err = parse_platform_id("chat:123").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_empty_user_e164_errors() {
        let err = parse_platform_id("user:").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_empty_group_id_errors() {
        let err = parse_platform_id("group:").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    // ------- safe_attachment_name -------

    #[test]
    fn safe_attachment_name_ok() {
        assert_eq!(safe_attachment_name("doc.txt").unwrap(), "doc.txt");
        assert_eq!(safe_attachment_name("a-b_c.PNG").unwrap(), "a-b_c.PNG");
    }

    #[test]
    fn safe_attachment_name_empty_errors() {
        assert!(matches!(
            safe_attachment_name(""),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn safe_attachment_name_too_long_errors() {
        let too_long = "x".repeat(MAX_ATTACHMENT_NAME_LEN + 1);
        assert!(matches!(
            safe_attachment_name(&too_long),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn safe_attachment_name_dotdot_errors() {
        assert!(matches!(
            safe_attachment_name("..foo"),
            Err(AdapterError::BadRequest(_))
        ));
        assert!(matches!(
            safe_attachment_name("a..b"),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn safe_attachment_name_slash_errors() {
        assert!(matches!(
            safe_attachment_name("dir/file"),
            Err(AdapterError::BadRequest(_))
        ));
        assert!(matches!(
            safe_attachment_name("dir\\file"),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn safe_attachment_name_leading_dot_errors() {
        assert!(matches!(
            safe_attachment_name(".hidden"),
            Err(AdapterError::BadRequest(_))
        ));
    }

    // ------- required_str / required_timestamp -------

    #[test]
    fn required_str_present_returns_value() {
        let v = json!({"k": "v"});
        assert_eq!(required_str(&v, "k").unwrap(), "v");
    }

    #[test]
    fn required_str_missing_errors() {
        let v = json!({});
        assert!(matches!(
            required_str(&v, "k"),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn required_timestamp_accepts_int() {
        let v = json!({"k": 99});
        assert_eq!(required_timestamp(&v, "k").unwrap(), 99);
    }

    #[test]
    fn required_timestamp_accepts_string_int() {
        let v = json!({"k": "99"});
        assert_eq!(required_timestamp(&v, "k").unwrap(), 99);
    }

    #[test]
    fn required_timestamp_missing_errors() {
        let v = json!({});
        assert!(matches!(
            required_timestamp(&v, "k"),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn required_timestamp_unparseable_string_errors() {
        let v = json!({"k": "nan"});
        assert!(matches!(
            required_timestamp(&v, "k"),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn required_timestamp_wrong_type_errors() {
        let v = json!({"k": [1]});
        assert!(matches!(
            required_timestamp(&v, "k"),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn outbox_subdir_constant() {
        assert_eq!(OUTBOX_SUBDIR, "outbox");
    }

    #[test]
    fn max_attachment_name_len_constant() {
        assert_eq!(MAX_ATTACHMENT_NAME_LEN, 255);
    }
}
