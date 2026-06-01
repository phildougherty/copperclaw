//! [`DeltaChatAdapter`] — the [`ChannelAdapter`] implementation.
//!
//! Holds the shared [`RpcTransport`], the resolved `account_id`, and the
//! forwarder task that polls events into the host's `inbound_tx`.

use crate::api;
use crate::config::DeltaChatConfig;
use crate::factory::CHANNEL_TYPE_STR;
use crate::parse::{build_send_payload, event_to_inbound, extract_incoming_msg, parse_platform_id};
use crate::rpc::RpcTransport;
use async_trait::async_trait;
use copperclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use copperclaw_types::{ChannelType, InboundEvent, OutboundFile, OutboundMessage};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Delta Chat channel adapter.
///
/// Construct via [`DeltaChatAdapter::start`] (or
/// [`DeltaChatAdapter::start_with_transport`] in tests). Drop the returned
/// `Arc` after calling [`DeltaChatAdapter::shutdown`] to release the
/// background event loop.
pub struct DeltaChatAdapter {
    channel_type: ChannelType,
    transport: Arc<dyn RpcTransport>,
    config: DeltaChatConfig,
    data_dir: PathBuf,
    cancel: CancellationToken,
    forwarder_handle: Mutex<Option<JoinHandle<()>>>,
}

impl DeltaChatAdapter {
    /// Build an adapter on top of a caller-supplied transport.
    ///
    /// Spawns the forwarder task that translates `get_next_event` into
    /// [`InboundEvent`]s. The forwarder honours the cancellation token
    /// returned by [`DeltaChatAdapter::shutdown`].
    pub fn start_with_transport(
        transport: Arc<dyn RpcTransport>,
        config: DeltaChatConfig,
        inbound_tx: Sender<InboundEvent>,
        data_dir: PathBuf,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let settings = AttachmentSettings {
            attachment_download: config.attachment_download,
            blob_dir: config.blob_dir.clone().map(PathBuf::from),
            max_attachment_bytes: config.max_attachment_bytes,
        };
        let forwarder_handle = tokio::spawn(run_forwarder(
            transport.clone(),
            config.account_id,
            inbound_tx,
            config.event_poll_ms,
            settings,
            cancel.clone(),
        ));
        Arc::new(Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            transport,
            config,
            data_dir,
            cancel,
            forwarder_handle: Mutex::new(Some(forwarder_handle)),
        })
    }

    /// The resolved configuration.
    pub fn config(&self) -> &DeltaChatConfig {
        &self.config
    }

    /// The data directory the host gave us.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Shared transport handle.
    pub fn transport(&self) -> &Arc<dyn RpcTransport> {
        &self.transport
    }

    /// Stop the forwarder task and wait for it to finish. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.forwarder_handle.lock().await.take() {
            let _ = handle.await;
        }
    }
}

impl std::fmt::Debug for DeltaChatAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaChatAdapter")
            .field("channel_type", &self.channel_type)
            .field("account_id", &self.config.account_id)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

/// Settings the forwarder needs to materialise attachments.
#[derive(Debug, Clone)]
pub(crate) struct AttachmentSettings {
    pub attachment_download: bool,
    pub blob_dir: Option<PathBuf>,
    pub max_attachment_bytes: u64,
}

async fn run_forwarder(
    transport: Arc<dyn RpcTransport>,
    account_id: u64,
    inbound_tx: Sender<InboundEvent>,
    poll_ms: u64,
    settings: AttachmentSettings,
    cancel: CancellationToken,
) {
    let backoff = Duration::from_millis(poll_ms.max(1));
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let event_fut = transport.next_event();
        let event = tokio::select! {
            () = cancel.cancelled() => return,
            res = event_fut => res,
        };
        let event = match event {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(?err, "deltachat: get_next_event failed");
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                continue;
            }
        };
        if let Err(err) = handle_event(
            transport.as_ref(),
            account_id,
            &settings,
            &event,
            &inbound_tx,
        )
        .await
        {
            tracing::warn!(?err, "deltachat: failed to forward event");
        }
        // If the receiver is gone, stop the loop.
        if inbound_tx.is_closed() {
            return;
        }
    }
}

async fn handle_event(
    transport: &dyn RpcTransport,
    expected_account: u64,
    settings: &AttachmentSettings,
    event: &Value,
    inbound_tx: &Sender<InboundEvent>,
) -> Result<(), AdapterError> {
    let Some(refs) = extract_incoming_msg(event) else {
        // Info / Warning / Error / etc. — log and move on. Errors at this
        // layer are deltachat-side telemetry; surface them at debug.
        tracing::debug!(?event, "deltachat: non-message event");
        return Ok(());
    };
    if refs.account_id != expected_account {
        tracing::debug!(
            event_account = refs.account_id,
            expected = expected_account,
            "deltachat: skipping event for other account"
        );
        return Ok(());
    }
    let mut msg = api::get_message(transport, refs.account_id, refs.msg_id).await?;
    let chat = api::get_basic_chat_info(transport, refs.account_id, refs.chat_id).await?;

    // If the message advertises a partial download state, ask the server to
    // pull the full body and re-fetch. We give up after one retry — the
    // server-side download is fire-and-forget but typically completes by the
    // time it returns (it emits a `MsgsChanged` event when the blob lands).
    let mut partial_download_error: Option<AdapterError> = None;
    if settings.attachment_download
        && msg.file.is_some()
        && needs_full_download(msg.download_state.as_deref())
    {
        match api::download_full_msg(transport, refs.account_id, refs.msg_id).await {
            Ok(()) => {
                msg = api::get_message(transport, refs.account_id, refs.msg_id).await?;
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "deltachat: download_full_msg failed; surfacing fallback"
                );
                partial_download_error = Some(err);
            }
        }
    }

    let Some(mut inbound) = event_to_inbound(refs.account_id, &msg, &chat) else {
        return Ok(());
    };

    if let Some(err) = partial_download_error {
        mark_download_failed(&mut inbound, &msg, &err);
    } else if settings.attachment_download {
        if let Some(file_path) = msg.file.as_deref() {
            apply_attachment_download(&mut inbound, settings, &msg, file_path).await;
        }
    }

    forward(inbound_tx, inbound).await
}

async fn forward(
    inbound_tx: &Sender<InboundEvent>,
    inbound: InboundEvent,
) -> Result<(), AdapterError> {
    if let Err(err) = inbound_tx.send(inbound).await {
        tracing::debug!(?err, "deltachat: inbound channel closed");
    }
    Ok(())
}

fn needs_full_download(state: Option<&str>) -> bool {
    match state {
        Some("Done") | None => false,
        Some(_) => true,
    }
}

/// Read the blob off disk and patch the inbound event's `content`
/// accordingly. On any failure (file missing, too large, IO error) we
/// downgrade to `MessageKind::System` with `reason` and `error` notes
/// so the host can audit/retry.
async fn apply_attachment_download(
    inbound: &mut InboundEvent,
    settings: &AttachmentSettings,
    msg: &api::MessageView,
    reported_path: &str,
) {
    let on_disk = resolve_blob_path(reported_path, settings.blob_dir.as_deref());

    // Quick metadata-based gate before we read anything.
    if let Some(size) = msg.file_bytes {
        if size > settings.max_attachment_bytes {
            mark_too_large(inbound, msg, Some(size), settings.max_attachment_bytes);
            return;
        }
    }

    let metadata = match tokio::fs::metadata(&on_disk).await {
        Ok(m) => m,
        Err(err) => {
            mark_download_failed(
                inbound,
                msg,
                &AdapterError::Transport(format!(
                    "deltachat blob stat {} failed: {err}",
                    on_disk.display()
                )),
            );
            return;
        }
    };

    let on_disk_len = metadata.len();
    if on_disk_len > settings.max_attachment_bytes {
        mark_too_large(
            inbound,
            msg,
            Some(on_disk_len),
            settings.max_attachment_bytes,
        );
        return;
    }

    // We don't actually need to load the bytes into memory — the host can
    // read the file off the resolved path. But we *do* verify it's readable
    // so a corrupt symlink/permission issue is caught before we hand the
    // path to the agent.
    if let Err(err) = tokio::fs::File::open(&on_disk).await {
        mark_download_failed(
            inbound,
            msg,
            &AdapterError::Transport(format!(
                "deltachat blob open {} failed: {err}",
                on_disk.display()
            )),
        );
        return;
    }

    let Value::Object(content) = &mut inbound.message.content else {
        return;
    };
    let Some(att) = content.get_mut("attachment").and_then(Value::as_object_mut) else {
        return;
    };
    att.insert(
        "bytes_path".to_owned(),
        Value::String(on_disk.to_string_lossy().into_owned()),
    );
    att.insert("size".to_owned(), Value::from(on_disk_len));
    if let Some(mime) = msg.file_mime.as_ref() {
        att.insert("mime".to_owned(), Value::String(mime.clone()));
    }
}

fn resolve_blob_path(reported: &str, blob_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = blob_dir {
        let basename = std::path::Path::new(reported)
            .file_name()
            .map_or_else(|| std::ffi::OsStr::new(reported), |n| n);
        return dir.join(basename);
    }
    PathBuf::from(reported)
}

fn mark_too_large(
    inbound: &mut InboundEvent,
    msg: &api::MessageView,
    actual: Option<u64>,
    limit: u64,
) {
    let mut obj = base_attachment_metadata(msg);
    obj.insert("reason".to_owned(), Value::String("too_large".to_owned()));
    obj.insert("limit".to_owned(), Value::from(limit));
    if let Some(size) = actual {
        obj.insert("reported_size".to_owned(), Value::from(size));
    }
    inbound.message.kind = copperclaw_types::MessageKind::System;
    inbound.message.content = json!({"attachment": Value::Object(obj)});
}

fn mark_download_failed(inbound: &mut InboundEvent, msg: &api::MessageView, err: &AdapterError) {
    let mut obj = base_attachment_metadata(msg);
    obj.insert(
        "reason".to_owned(),
        Value::String("download_failed".to_owned()),
    );
    obj.insert("error".to_owned(), Value::String(err.to_string()));
    inbound.message.kind = copperclaw_types::MessageKind::System;
    inbound.message.content = json!({"attachment": Value::Object(obj)});
}

fn base_attachment_metadata(msg: &api::MessageView) -> serde_json::Map<String, Value> {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "kind".to_owned(),
        Value::String(format!("deltachat.{}", msg.view_type.to_ascii_lowercase())),
    );
    if let Some(p) = msg.file.as_ref() {
        obj.insert("path".to_owned(), Value::String(p.clone()));
    }
    if let Some(name) = msg.filename.as_ref() {
        obj.insert("filename".to_owned(), Value::String(name.clone()));
    }
    if let Some(mime) = msg.file_mime.as_ref() {
        obj.insert("mime".to_owned(), Value::String(mime.clone()));
    }
    if let Some(size) = msg.file_bytes {
        obj.insert("file_bytes".to_owned(), Value::from(size));
    }
    if let Some(state) = msg.download_state.as_ref() {
        obj.insert("download_state".to_owned(), Value::String(state.clone()));
    }
    obj
}

#[async_trait]
impl ChannelAdapter for DeltaChatAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    fn supports_threads(&self) -> bool {
        false
    }

    async fn subscribe(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // No server-side subscribe step; just validate the shape so
        // misconfiguration surfaces early.
        if parse_platform_id(platform_id).is_none() {
            return Err(AdapterError::BadRequest(format!(
                "deltachat platform_id must be `account/<id>/chat/<id>`, got `{platform_id}`"
            )));
        }
        Ok(())
    }

    async fn set_typing(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        // Delta Chat does not have a typing-indicator API; treat it as a
        // silent no-op rather than NotImplemented so the host's typing
        // pings don't pollute the error metric.
        Ok(())
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let parsed = parse_platform_id(platform_id).ok_or_else(|| {
            AdapterError::BadRequest(format!(
                "deltachat platform_id must be `account/<id>/chat/<id>`, got `{platform_id}`"
            ))
        })?;
        if parsed.account_id != self.config.account_id {
            return Err(AdapterError::BadRequest(format!(
                "deltachat platform_id account {} does not match configured account {}",
                parsed.account_id, self.config.account_id
            )));
        }

        // System-action shape: { "action": "reaction"|"delete"|"edit", ... }
        if let Some(action) = message.content.get("action").and_then(Value::as_str) {
            return self
                .deliver_action(parsed.chat_id, action, &message.content)
                .await;
        }

        let text = extract_text(&message.content);

        if message.files.is_empty() {
            let payload = build_send_payload(&text, None);
            let id = api::send_msg(
                self.transport.as_ref(),
                self.config.account_id,
                parsed.chat_id,
                payload,
            )
            .await?;
            return Ok(Some(id.to_string()));
        }

        // For each file write to data_dir/outgoing/<filename> and pass
        // the path through to send_msg. The first file picks up the
        // caption (if any); subsequent files send with their filename.
        let outgoing_dir = self.data_dir.join("outgoing");
        tokio::fs::create_dir_all(&outgoing_dir).await?;
        let mut last_id: Option<String> = None;
        for (idx, file) in message.files.iter().enumerate() {
            let path = persist_outgoing_file(&outgoing_dir, file).await?;
            let caption = if idx == 0 { text.as_str() } else { "" };
            let payload = build_send_payload(
                caption,
                Some((
                    path.to_str().ok_or_else(|| {
                        AdapterError::BadRequest(
                            "deltachat outgoing file path is not valid utf-8".into(),
                        )
                    })?,
                    &file.filename,
                )),
            );
            let id = api::send_msg(
                self.transport.as_ref(),
                self.config.account_id,
                parsed.chat_id,
                payload,
            )
            .await?;
            last_id = Some(id.to_string());
        }
        Ok(last_id)
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        // Delta Chat DMs require knowing the contact id; defer to admin
        // configuration of a `messaging_groups` row.
        Ok(None)
    }
}

impl DeltaChatAdapter {
    async fn deliver_action(
        &self,
        chat_id: i64,
        action: &str,
        content: &Value,
    ) -> Result<Option<String>, AdapterError> {
        match action {
            "reaction" => {
                let target = required_i64(content, "target_msg_id").or_else(|_| {
                    // Support `target_platform_id` as a stringified id, too.
                    required_str(content, "target_platform_id")?
                        .parse::<i64>()
                        .map_err(|_| {
                            AdapterError::BadRequest(
                                "deltachat target_platform_id must parse as an integer".into(),
                            )
                        })
                })?;
                let emoji = required_str(content, "emoji")?;
                let id = api::send_reaction(
                    self.transport.as_ref(),
                    self.config.account_id,
                    target,
                    &[emoji.to_owned()],
                )
                .await?;
                Ok(Some(id.to_string()))
            }
            "delete" => {
                let target = required_i64(content, "target_msg_id").or_else(|_| {
                    required_str(content, "target_platform_id")?
                        .parse::<i64>()
                        .map_err(|_| {
                            AdapterError::BadRequest(
                                "deltachat target_platform_id must parse as an integer".into(),
                            )
                        })
                })?;
                api::delete_messages(self.transport.as_ref(), self.config.account_id, &[target])
                    .await?;
                // Drop the unused chat id parameter — delete is by message id.
                let _ = chat_id;
                Ok(None)
            }
            "edit" => Err(AdapterError::Unsupported(
                "deltachat does not support editing messages".into(),
            )),
            other => Err(AdapterError::Unsupported(format!(
                "deltachat action `{other}` is not supported"
            ))),
        }
    }
}

fn extract_text(value: &Value) -> String {
    value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, AdapterError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::BadRequest(format!("missing `{key}` in deltachat action")))
}

fn required_i64(value: &Value, key: &str) -> Result<i64, AdapterError> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| AdapterError::BadRequest(format!("missing `{key}` in deltachat action")))
}

async fn persist_outgoing_file(dir: &Path, file: &OutboundFile) -> Result<PathBuf, AdapterError> {
    let safe = safe_filename(&file.filename);
    let unique = format!("{}-{}", chrono::Utc::now().timestamp_millis(), safe);
    let path = dir.join(unique);
    tokio::fs::write(&path, &file.data).await?;
    Ok(path)
}

fn safe_filename(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "attachment".into();
    }
    trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// Helper functions used by tests in this module — and only by tests, but
// living in the parent so the `MessageView` helper has a single home.
#[cfg(test)]
fn build_message_view(id: i64, chat_id: i64, text: &str) -> crate::api::MessageView {
    crate::api::MessageView {
        id,
        chat_id,
        from_id: 7,
        text: text.into(),
        is_info: false,
        view_type: "Text".into(),
        file: None,
        filename: None,
        file_mime: None,
        file_bytes: None,
        download_state: Some("Done".into()),
        timestamp: 1_700_000_000,
        sender_name: Some("Alice".into()),
    }
}

#[cfg(test)]
fn build_chat_info(id: i64, chat_type: i64) -> crate::api::ChatInfo {
    crate::api::ChatInfo {
        id,
        chat_type,
        name: "name".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{MockResponse, MockTransport};
    use copperclaw_types::MessageKind;
    use serde_json::json;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn config_for(account: u64) -> DeltaChatConfig {
        DeltaChatConfig {
            account_id: account,
            rpc_server_bin: crate::config::DEFAULT_RPC_SERVER_BIN.to_owned(),
            extra_args: vec![],
            event_poll_ms: 10,
            attachment_download: true,
            blob_dir: None,
            max_attachment_bytes: crate::config::DEFAULT_MAX_ATTACHMENT_BYTES,
        }
    }

    fn build(
        transport: Arc<dyn RpcTransport>,
        account: u64,
    ) -> (Arc<DeltaChatAdapter>, TempDir, mpsc::Receiver<InboundEvent>) {
        let dir = TempDir::new().unwrap();
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let adapter = DeltaChatAdapter::start_with_transport(
            transport,
            config_for(account),
            tx,
            dir.path().to_path_buf(),
        );
        (adapter, dir, rx)
    }

    #[tokio::test]
    async fn channel_type_is_deltachat() {
        let m: Arc<dyn RpcTransport> = Arc::new(MockTransport::new());
        let (adapter, _dir, _rx) = build(m, 1);
        assert_eq!(adapter.channel_type().as_str(), "deltachat");
        assert!(!adapter.supports_threads());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_text_routes_to_send_msg() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::ok("send_msg", json!(101)))
            .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, _rx) = build(m, 1);
        let id = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({ "text": "hi" }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("101"));
        let calls = mock.observed().await;
        assert_eq!(calls[0].method, "send_msg");
        assert_eq!(calls[0].params, json!([1, 42, {"text": "hi"}]));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_files_writes_to_data_dir_and_passes_path() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::ok("send_msg", json!(7)))
            .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, dir, _rx) = build(m, 1);
        let id = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "caption"}),
                    files: vec![OutboundFile {
                        filename: "doc.txt".into(),
                        data: b"hello".to_vec(),
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("7"));

        let outgoing = dir.path().join("outgoing");
        assert!(outgoing.exists(), "outgoing dir should be created");
        let entries: Vec<_> = std::fs::read_dir(&outgoing)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1);
        let written = std::fs::read(entries[0].path()).unwrap();
        assert_eq!(written, b"hello");

        let calls = mock.observed().await;
        assert_eq!(calls[0].method, "send_msg");
        let params = &calls[0].params;
        assert_eq!(params[0], 1);
        assert_eq!(params[1], 42);
        assert_eq!(params[2]["text"], "caption");
        assert_eq!(params[2]["filename"], "doc.txt");
        assert!(params[2]["file"].as_str().unwrap().contains("outgoing/"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_with_multiple_files_caption_first_then_filename() {
        let mock = Arc::new(MockTransport::new());
        mock.push_responses([
            MockResponse::ok("send_msg", json!(1)),
            MockResponse::ok("send_msg", json!(2)),
        ])
        .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, _rx) = build(m, 1);
        let id = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "cap"}),
                    files: vec![
                        OutboundFile {
                            filename: "a.txt".into(),
                            data: b"a".to_vec(),
                        },
                        OutboundFile {
                            filename: "b.txt".into(),
                            data: b"b".to_vec(),
                        },
                    ],
                },
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("2"));
        let calls = mock.observed().await;
        assert_eq!(calls[0].params[2]["text"], "cap");
        assert_eq!(calls[1].params[2]["text"], "");
        assert_eq!(calls[0].params[2]["filename"], "a.txt");
        assert_eq!(calls[1].params[2]["filename"], "b.txt");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_routes_to_send_reaction() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::ok("send_reaction", json!(0)))
            .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, _rx) = build(m, 1);
        adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_msg_id": 88,
                        "emoji": "+1"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        let calls = mock.observed().await;
        assert_eq!(calls[0].method, "send_reaction");
        assert_eq!(calls[0].params, json!([1, 88, ["+1"]]));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_accepts_target_platform_id_as_string() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::ok("send_reaction", json!(0)))
            .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, _rx) = build(m, 1);
        adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_platform_id": "88",
                        "emoji": "+1"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        let calls = mock.observed().await;
        assert_eq!(calls[0].params, json!([1, 88, ["+1"]]));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_missing_target_errors() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "reaction", "emoji": "+1"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_missing_emoji_errors() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "reaction", "target_msg_id": 1}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_reaction_unparseable_platform_id_errors() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({
                        "action": "reaction",
                        "target_platform_id": "not-a-num",
                        "emoji": "+1"
                    }),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_delete_routes_to_delete_messages() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::ok("delete_messages", Value::Null))
            .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, _rx) = build(m, 1);
        let id = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "delete", "target_msg_id": 5}),
                    files: vec![],
                },
            )
            .await
            .unwrap();
        assert!(id.is_none());
        let calls = mock.observed().await;
        assert_eq!(calls[0].method, "delete_messages");
        assert_eq!(calls[0].params, json!([1, [5]]));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_delete_missing_target_errors() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "delete"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_edit_returns_unsupported() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "edit", "target_msg_id": 1, "text": "x"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(m) if m.contains("edit")));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_unknown_action_is_unsupported() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::System,
                    content: json!({"action": "shrug"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_malformed_platform_id_errors() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "wrong-shape",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("account/")));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_mismatched_account_errors() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/9/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(m) if m.contains("does not match")));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_propagates_send_msg_error() {
        let mock = Arc::new(MockTransport::new());
        mock.push_response(MockResponse::err(
            "send_msg",
            AdapterError::Rate { retry_after: None },
        ))
        .await;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter
            .deliver(
                "account/1/chat/42",
                None,
                &OutboundMessage {
                    kind: MessageKind::Chat,
                    content: json!({"text": "hi"}),
                    files: vec![],
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Rate { retry_after: None }));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_with_valid_platform_id_is_ok() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        adapter.subscribe("account/1/chat/42", None).await.unwrap();
        adapter
            .subscribe("account/1/chat/42", Some("t"))
            .await
            .unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn subscribe_with_bad_platform_id_errors() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        let err = adapter.subscribe("nope", None).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn set_typing_is_noop_ok() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        adapter.set_typing("account/1/chat/42", None).await.unwrap();
        adapter.set_typing("anything", Some("t")).await.unwrap();
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        assert!(adapter.open_dm("7").await.unwrap().is_none());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_pushes_inbound_event_for_incoming_msg() {
        let mock = Arc::new(MockTransport::new());
        // First the forwarder will call get_next_event (consumed from the event queue).
        mock.push_event(json!({
            "kind": "IncomingMsg",
            "account_id": 1,
            "chat_id": 42,
            "msg_id": 100
        }))
        .await;
        // Then handle_event will call get_message and get_basic_chat_info.
        mock.push_responses([
            MockResponse::ok(
                "get_message",
                json!({
                    "id": 100, "chat_id": 42, "from_id": 7,
                    "text": "hi", "is_info": false,
                    "view_type": "Text", "timestamp": 1_700_000_000,
                    "sender_name": "Alice"
                }),
            ),
            MockResponse::ok(
                "get_basic_chat_info",
                json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            ),
        ])
        .await;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build(m, 1);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("event timeout")
            .expect("event missing");
        assert_eq!(evt.channel_type.as_str(), "deltachat");
        assert_eq!(evt.platform_id, "account/1/chat/42");
        assert_eq!(evt.message.content["text"], "hi");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_skips_event_for_other_account() {
        let mock = Arc::new(MockTransport::new());
        mock.push_event(json!({
            "kind": "IncomingMsg",
            "account_id": 9,
            "chat_id": 42,
            "msg_id": 100
        }))
        .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, mut rx) = build(m, 1);
        // No matching event arrives; expect a timeout.
        let res = tokio::time::timeout(Duration::from_millis(80), rx.recv()).await;
        assert!(
            res.is_err(),
            "should not have forwarded a foreign-account event"
        );
        // No get_message call was made.
        let calls = mock.observed().await;
        assert!(calls.iter().all(|c| c.method != "get_message"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_skips_info_messages() {
        let mock = Arc::new(MockTransport::new());
        mock.push_event(json!({
            "kind": "IncomingMsg",
            "account_id": 1,
            "chat_id": 42,
            "msg_id": 100
        }))
        .await;
        mock.push_responses([
            MockResponse::ok(
                "get_message",
                json!({
                    "id": 100, "chat_id": 42, "from_id": 7,
                    "text": "Alice joined", "is_info": true,
                    "view_type": "Text", "timestamp": 1_700_000_000
                }),
            ),
            MockResponse::ok(
                "get_basic_chat_info",
                json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            ),
        ])
        .await;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build(m, 1);
        let res = tokio::time::timeout(Duration::from_millis(120), rx.recv()).await;
        assert!(res.is_err(), "info-message should be skipped");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_ignores_non_message_events() {
        let mock = Arc::new(MockTransport::new());
        mock.push_event(json!({"kind": "Info", "msg": "noise"}))
            .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, mut rx) = build(m, 1);
        let res = tokio::time::timeout(Duration::from_millis(80), rx.recv()).await;
        assert!(res.is_err(), "Info events should not produce inbound");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_recovers_after_transport_error() {
        let mock = Arc::new(MockTransport::new());
        // First poll fails, then succeeds with a real event.
        mock.push_event_error(AdapterError::Transport("flake".into()))
            .await;
        mock.push_event(json!({
            "kind": "IncomingMsg",
            "account_id": 1,
            "chat_id": 42,
            "msg_id": 100
        }))
        .await;
        mock.push_responses([
            MockResponse::ok(
                "get_message",
                json!({
                    "id": 100, "chat_id": 42, "from_id": 7,
                    "text": "hi", "is_info": false,
                    "view_type": "Text", "timestamp": 1_700_000_000
                }),
            ),
            MockResponse::ok(
                "get_basic_chat_info",
                json!({"id": 42, "chat_type": 1, "name": "x"}),
            ),
        ])
        .await;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build(m, 1);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("event timeout")
            .expect("event missing");
        assert_eq!(evt.platform_id, "account/1/chat/42");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_is_idempotent_and_stops_forwarder() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, _rx) = build(m, 1);
        adapter.shutdown().await;
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn debug_format_includes_account_and_type() {
        let m: Arc<dyn RpcTransport> = Arc::new(MockTransport::new());
        let (adapter, _dir, _rx) = build(m, 42);
        let s = format!("{adapter:?}");
        assert!(s.contains("DeltaChatAdapter"));
        assert!(s.contains("42"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn config_and_data_dir_accessors() {
        let m: Arc<dyn RpcTransport> = Arc::new(MockTransport::new());
        let (adapter, dir, _rx) = build(m, 5);
        assert_eq!(adapter.config().account_id, 5);
        assert_eq!(adapter.data_dir(), dir.path());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn transport_accessor_returns_shared_handle() {
        let mock = Arc::new(MockTransport::new());
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, _rx) = build(m, 1);
        // Two Arcs point at the same transport.
        assert!(Arc::ptr_eq(
            adapter.transport(),
            &(mock as Arc<dyn RpcTransport>)
        ));
        adapter.shutdown().await;
    }

    #[test]
    fn safe_filename_strips_unwelcome_characters() {
        assert_eq!(safe_filename("simple.txt"), "simple.txt");
        assert_eq!(safe_filename("with space.txt"), "with_space.txt");
        assert_eq!(safe_filename("../etc/passwd"), ".._etc_passwd");
        assert_eq!(safe_filename(""), "attachment");
        assert_eq!(safe_filename("    "), "attachment");
    }

    #[test]
    fn extract_text_handles_missing_field() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({"text": "x"})), "x");
        assert_eq!(extract_text(&json!({"text": 7})), "");
    }

    #[test]
    fn required_str_and_i64_helpers_error_on_missing() {
        assert!(matches!(
            required_str(&json!({}), "x").unwrap_err(),
            AdapterError::BadRequest(_)
        ));
        assert!(matches!(
            required_i64(&json!({}), "x").unwrap_err(),
            AdapterError::BadRequest(_)
        ));
    }

    #[test]
    fn build_message_and_chat_helpers_smoke() {
        let m = build_message_view(1, 1, "hi");
        assert_eq!(m.text, "hi");
        let c = build_chat_info(1, 2);
        assert_eq!(c.chat_type, 2);
    }

    // --- Attachment download tests ---

    fn config_no_download(account: u64) -> DeltaChatConfig {
        let mut cfg = config_for(account);
        cfg.attachment_download = false;
        cfg
    }

    fn build_with_config(
        transport: Arc<dyn RpcTransport>,
        cfg: DeltaChatConfig,
    ) -> (Arc<DeltaChatAdapter>, TempDir, mpsc::Receiver<InboundEvent>) {
        let dir = TempDir::new().unwrap();
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let adapter =
            DeltaChatAdapter::start_with_transport(transport, cfg, tx, dir.path().to_path_buf());
        (adapter, dir, rx)
    }

    /// Set up the response queue in the order `handle_event` consumes it:
    /// `get_message` -> `get_basic_chat_info` -> (optional)
    /// `download_full_msg` -> `get_message`.
    async fn enqueue_incoming(
        mock: &MockTransport,
        msg_view: Value,
        chat_info: Value,
        download_chain: Option<(MockResponse, Value)>,
    ) {
        mock.push_event(json!({
            "kind": "IncomingMsg",
            "account_id": 1,
            "chat_id": 42,
            "msg_id": 100
        }))
        .await;
        mock.push_responses([
            MockResponse::ok("get_message", msg_view),
            MockResponse::ok("get_basic_chat_info", chat_info),
        ])
        .await;
        if let Some((dl_resp, after_view)) = download_chain {
            mock.push_responses([dl_resp, MockResponse::ok("get_message", after_view)])
                .await;
        }
    }

    fn writable_blob(dir: &Path, body: &[u8]) -> PathBuf {
        let path = dir.join("blob.bin");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[tokio::test]
    async fn forwarder_reads_blob_when_attachment_download_enabled() {
        let tmp = TempDir::new().unwrap();
        let blob = writable_blob(tmp.path(), b"hello-world");
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "see attached", "is_info": false,
                "view_type": "File",
                "file": blob.to_string_lossy(),
                "filename": "hello.txt",
                "file_mime": "text/plain",
                "file_bytes": 11,
                "download_state": "Done",
                "timestamp": 1_700_000_000,
                "sender_name": "Alice"
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build(m, 1);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["path"], blob.to_string_lossy().into_owned());
        assert_eq!(att["bytes_path"], blob.to_string_lossy().into_owned());
        assert_eq!(att["size"], 11);
        assert_eq!(att["mime"], "text/plain");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_keeps_legacy_path_when_attachment_download_disabled() {
        let tmp = TempDir::new().unwrap();
        let blob = writable_blob(tmp.path(), b"hello-world");
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "see attached", "is_info": false,
                "view_type": "File",
                "file": blob.to_string_lossy(),
                "filename": "hello.txt",
                "download_state": "Done",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build_with_config(m, config_no_download(1));
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        // Without attachment_download we should still get the chat event, but
        // there's no `bytes_path` / `size`.
        assert_eq!(evt.message.kind, MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        assert!(att.get("bytes_path").is_none());
        assert!(att.get("size").is_none());
        assert_eq!(att["path"], blob.to_string_lossy().into_owned());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_triggers_download_full_msg_for_partial_download() {
        let tmp = TempDir::new().unwrap();
        let blob = writable_blob(tmp.path(), b"after-download");
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "available later", "is_info": false,
                "view_type": "File",
                "file": blob.to_string_lossy(),
                "filename": "later.bin",
                "download_state": "Available",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            Some((
                MockResponse::ok("download_full_msg", Value::Null),
                json!({
                    "id": 100, "chat_id": 42, "from_id": 7,
                    "text": "available later", "is_info": false,
                    "view_type": "File",
                    "file": blob.to_string_lossy(),
                    "filename": "later.bin",
                    "file_bytes": 14,
                    "download_state": "Done",
                    "timestamp": 1_700_000_000
                }),
            )),
        )
        .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, mut rx) = build(m, 1);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["bytes_path"], blob.to_string_lossy().into_owned());
        assert_eq!(att["size"], 14);
        let calls = mock.observed().await;
        assert!(
            calls.iter().any(|c| c.method == "download_full_msg"),
            "expected download_full_msg call"
        );
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_falls_back_to_system_when_download_full_msg_errors() {
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "needs body", "is_info": false,
                "view_type": "File",
                "file": "/no/such/path",
                "filename": "needs.bin",
                "download_state": "Available",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        mock.push_responses([MockResponse::err(
            "download_full_msg",
            AdapterError::Transport("imap unreachable".into()),
        )])
        .await;
        // Note: even after this failure, handle_event still issues
        // get_basic_chat_info — the chat info is already queued above.
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build(m, 1);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["reason"], "download_failed");
        assert!(att["error"].as_str().unwrap().contains("imap unreachable"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_falls_back_when_blob_missing_on_disk() {
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "phantom", "is_info": false,
                "view_type": "File",
                "file": "/no/such/blob.bin",
                "filename": "phantom.bin",
                "download_state": "Done",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build(m, 1);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(
            evt.message.content["attachment"]["reason"],
            "download_failed"
        );
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_too_large_by_reported_size_yields_system() {
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "big", "is_info": false,
                "view_type": "File",
                "file": "/no/touch",
                "filename": "big.bin",
                "file_bytes": 10 * 1024 * 1024,
                "download_state": "Done",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let mut cfg = config_for(1);
        cfg.max_attachment_bytes = 1024;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build_with_config(m, cfg);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["reason"], "too_large");
        assert_eq!(att["limit"], 1024);
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_too_large_by_on_disk_size_yields_system() {
        let tmp = TempDir::new().unwrap();
        let blob = writable_blob(tmp.path(), &vec![0u8; 4096]);
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "big", "is_info": false,
                "view_type": "File",
                "file": blob.to_string_lossy(),
                "filename": "big.bin",
                "download_state": "Done",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let mut cfg = config_for(1);
        cfg.max_attachment_bytes = 1024;
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build_with_config(m, cfg);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["attachment"]["reason"], "too_large");
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_uses_blob_dir_to_resolve_path() {
        let tmp = TempDir::new().unwrap();
        let blob = writable_blob(tmp.path(), b"shared");
        let mock = Arc::new(MockTransport::new());
        // The server reports a remote-style path; the adapter should
        // resolve it against the configured blob_dir.
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "shared", "is_info": false,
                "view_type": "File",
                "file": "/dc/blob.bin",
                "filename": "shared.bin",
                "file_bytes": 6,
                "download_state": "Done",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let mut cfg = config_for(1);
        cfg.blob_dir = Some(tmp.path().to_string_lossy().into_owned());
        let m: Arc<dyn RpcTransport> = mock;
        let (adapter, _dir, mut rx) = build_with_config(m, cfg);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["bytes_path"], blob.to_string_lossy().into_owned());
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_text_only_message_has_no_attachment_handling() {
        let mock = Arc::new(MockTransport::new());
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "plain", "is_info": false,
                "view_type": "Text",
                "download_state": "Done",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, mut rx) = build(m, 1);
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert!(evt.message.content.get("attachment").is_none());
        // Should not have invoked download_full_msg.
        let calls = mock.observed().await;
        assert!(!calls.iter().any(|c| c.method == "download_full_msg"));
        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn forwarder_skips_blob_when_attachment_download_disabled_even_if_state_partial() {
        let tmp = TempDir::new().unwrap();
        let blob = writable_blob(tmp.path(), b"ignored");
        let mock = Arc::new(MockTransport::new());
        // No download_full_msg response queued — we'll fail loudly if the
        // adapter tries to invoke it.
        enqueue_incoming(
            &mock,
            json!({
                "id": 100, "chat_id": 42, "from_id": 7,
                "text": "available but skipped", "is_info": false,
                "view_type": "File",
                "file": blob.to_string_lossy(),
                "filename": "skip.bin",
                "download_state": "Available",
                "timestamp": 1_700_000_000
            }),
            json!({"id": 42, "chat_type": 1, "name": "Alice"}),
            None,
        )
        .await;
        let m: Arc<dyn RpcTransport> = mock.clone();
        let (adapter, _dir, mut rx) = build_with_config(m, config_no_download(1));
        let evt = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        let calls = mock.observed().await;
        assert!(!calls.iter().any(|c| c.method == "download_full_msg"));
        adapter.shutdown().await;
    }

    #[test]
    fn needs_full_download_handles_known_states() {
        assert!(!needs_full_download(None));
        assert!(!needs_full_download(Some("Done")));
        assert!(needs_full_download(Some("Available")));
        assert!(needs_full_download(Some("InProgress")));
        assert!(needs_full_download(Some("Failure")));
    }

    #[test]
    fn resolve_blob_path_uses_blob_dir_when_configured() {
        let p = resolve_blob_path("/server/path/blob.bin", Some(Path::new("/local/dc")));
        assert_eq!(p.to_string_lossy(), "/local/dc/blob.bin");
    }

    #[test]
    fn resolve_blob_path_passes_through_when_no_dir() {
        let p = resolve_blob_path("/server/blob.bin", None);
        assert_eq!(p.to_string_lossy(), "/server/blob.bin");
    }

    #[test]
    fn base_attachment_metadata_includes_known_fields() {
        let msg = api::MessageView {
            id: 1,
            chat_id: 1,
            from_id: 1,
            text: String::new(),
            is_info: false,
            view_type: "Image".into(),
            file: Some("/blob.bin".into()),
            filename: Some("pic.jpg".into()),
            file_mime: Some("image/jpeg".into()),
            file_bytes: Some(99),
            download_state: Some("Available".into()),
            timestamp: 0,
            sender_name: None,
        };
        let obj = base_attachment_metadata(&msg);
        assert_eq!(obj["kind"], "deltachat.image");
        assert_eq!(obj["path"], "/blob.bin");
        assert_eq!(obj["filename"], "pic.jpg");
        assert_eq!(obj["mime"], "image/jpeg");
        assert_eq!(obj["file_bytes"], 99);
        assert_eq!(obj["download_state"], "Available");
    }
}
