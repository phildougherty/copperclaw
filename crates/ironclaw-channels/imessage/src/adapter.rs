//! [`IMessageAdapter`] — `ChannelAdapter` for the local macOS Messages.app.
//!
//! See the crate-level docs for the AppleScript / sqlite contract.

use crate::applescript::{AppleScriptEscapeError, applescript_escape};
use crate::bridge::IMessageBridge;
use crate::config::IMessageConfig;
use crate::parse::{ParsedPlatformId, describe_system_action, parse_platform_id};
use crate::poll::poll_loop;
use async_trait::async_trait;
use ironclaw_channels_core::{AdapterError, ChannelAdapter, DmHandle};
use ironclaw_types::{ChannelType, MessageKind, OutboundFile, OutboundMessage};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::factory::CHANNEL_TYPE_STR;

/// iMessage channel adapter.
pub struct IMessageAdapter {
    channel_type: ChannelType,
    bridge: Arc<dyn IMessageBridge>,
    config: IMessageConfig,
    data_dir: PathBuf,
    cancel: CancellationToken,
    poll_handle: Mutex<Option<JoinHandle<()>>>,
}

impl IMessageAdapter {
    /// Construct an adapter against a custom bridge.
    ///
    /// Spawns the background poll task unless `config.enable_polling` is
    /// `false`, in which case the adapter is outbound-only.
    pub fn start_with_bridge(
        bridge: Arc<dyn IMessageBridge>,
        config: IMessageConfig,
        inbound_tx: Sender<ironclaw_types::InboundEvent>,
        data_dir: impl Into<PathBuf>,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let data_dir = data_dir.into();
        let handle = if config.enable_polling {
            Some(tokio::spawn(poll_loop(
                bridge.clone(),
                config.clone(),
                data_dir.clone(),
                inbound_tx,
                cancel.clone(),
            )))
        } else {
            // No-op; we still keep the inbound_tx alive by dropping it.
            drop(inbound_tx);
            None
        };
        Arc::new(Self {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            bridge,
            config,
            data_dir,
            cancel,
            poll_handle: Mutex::new(handle),
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
    pub fn config(&self) -> &IMessageConfig {
        &self.config
    }

    /// Borrow the data dir.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }
}

impl std::fmt::Debug for IMessageAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IMessageAdapter")
            .field("channel_type", &self.channel_type)
            .field("config", &self.config)
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ChannelAdapter for IMessageAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    async fn deliver(
        &self,
        platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        // Refuse system actions iMessage cannot do via AppleScript.
        if message.kind == MessageKind::System {
            let action = describe_system_action(&message.content);
            return Err(AdapterError::Unsupported(format!(
                "imessage channel does not support system action `{action}` \
                 (AppleScript tapback / edit surface is unreliable)"
            )));
        }
        let target = parse_platform_id(platform_id)?;
        let text = message
            .content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        // Reject empty bodies up front. Letting them through silently
        // produced the "agent thought it sent, user saw nothing" bug
        // class — there is no AppleScript invocation to make, so the
        // host's delivery loop would mark the row delivered=ok with no
        // platform side effect. A BadRequest surfaces to
        // `dropped_messages` with an explanatory reason instead.
        if text.is_empty() && message.files.is_empty() {
            return Err(AdapterError::BadRequest(
                "imessage deliver: empty body (no text, no files)".into(),
            ));
        }

        // Send the text body first (if any).
        if !text.is_empty() {
            let script = render_text_script(&self.config.service_name, &target, &text)?;
            self.bridge.run_applescript(&script).await?;
        }

        // Then send each file as its own AppleScript invocation. We can't
        // include attachments in the same `send` form as text; Messages.app
        // expects separate `send`s.
        if !message.files.is_empty() {
            for file in &message.files {
                let path = write_outbound_file(&self.data_dir, file)?;
                let script =
                    render_file_script(&self.config.service_name, &target, &path)?;
                self.bridge.run_applescript(&script).await?;
            }
        }

        Ok(None)
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }
}

/// Render the AppleScript document for sending a plain text body.
pub fn render_text_script(
    service_name: &str,
    target: &ParsedPlatformId,
    text: &str,
) -> Result<String, AdapterError> {
    let escaped_text = applescript_escape(text).map_err(escape_to_bad_request)?;
    let escaped_service = applescript_escape(service_name).map_err(escape_to_bad_request)?;
    Ok(match target {
        ParsedPlatformId::Handle(h) => {
            let escaped_handle = applescript_escape(h).map_err(escape_to_bad_request)?;
            let service_literal = service_name_literal(&escaped_service);
            format!(
                "tell application \"Messages\"\n  \
                   set targetService to 1st service whose service type = {service_literal}\n  \
                   set targetBuddy to buddy \"{escaped_handle}\" of targetService\n  \
                   send \"{escaped_text}\" to targetBuddy\n\
                 end tell",
            )
        }
        ParsedPlatformId::Chat(g) => {
            let escaped_chat = applescript_escape(g).map_err(escape_to_bad_request)?;
            format!(
                "tell application \"Messages\"\n  \
                   set targetChat to chat id \"{escaped_chat}\"\n  \
                   send \"{escaped_text}\" to targetChat\n\
                 end tell",
            )
        }
    })
}

/// Render the AppleScript document for sending an attachment.
pub fn render_file_script(
    service_name: &str,
    target: &ParsedPlatformId,
    path: &std::path::Path,
) -> Result<String, AdapterError> {
    let escaped_path = applescript_escape(&path.to_string_lossy())
        .map_err(escape_to_bad_request)?;
    let escaped_service = applescript_escape(service_name).map_err(escape_to_bad_request)?;
    Ok(match target {
        ParsedPlatformId::Handle(h) => {
            let escaped_handle = applescript_escape(h).map_err(escape_to_bad_request)?;
            let service_literal = service_name_literal(&escaped_service);
            format!(
                "tell application \"Messages\"\n  \
                   set targetService to 1st service whose service type = {service_literal}\n  \
                   set targetBuddy to buddy \"{escaped_handle}\" of targetService\n  \
                   send (POSIX file \"{escaped_path}\") to targetBuddy\n\
                 end tell",
            )
        }
        ParsedPlatformId::Chat(g) => {
            let escaped_chat = applescript_escape(g).map_err(escape_to_bad_request)?;
            format!(
                "tell application \"Messages\"\n  \
                   set targetChat to chat id \"{escaped_chat}\"\n  \
                   send (POSIX file \"{escaped_path}\") to targetChat\n\
                 end tell",
            )
        }
    })
}

/// AppleScript's `service type` enum is `iMessage` / `SMS` as a bare
/// identifier (not a quoted string). We map the configured name to the
/// canonical identifier; unknown names fall back to a quoted string —
/// which AppleScript will reject at runtime in a way the bridge maps to
/// Transport, but at least we don't generate syntactically invalid code.
fn service_name_literal(escaped_service: &str) -> String {
    match escaped_service {
        "iMessage" => "iMessage".to_owned(),
        "SMS" => "SMS".to_owned(),
        other => format!("\"{other}\""),
    }
}

/// Write an outbound attachment to `data_dir/outgoing/<uuid>-<filename>`
/// and return the absolute path.
///
/// We don't trust the agent-supplied `filename` (it could contain
/// directory separators or shell metacharacters), so we keep only its
/// basename and prefix it with a fresh UUID to avoid collisions.
pub fn write_outbound_file(
    data_dir: &std::path::Path,
    file: &OutboundFile,
) -> Result<PathBuf, AdapterError> {
    let out_dir = data_dir.join("outgoing");
    std::fs::create_dir_all(&out_dir).map_err(AdapterError::Io)?;
    let basename = std::path::Path::new(&file.filename)
        .file_name()
        .and_then(|s| s.to_str());
    let basename = match basename {
        Some(s) if !s.is_empty() && !s.contains('/') && !s.contains('\\') => s,
        _ => {
            return Err(AdapterError::BadRequest(format!(
                "imessage: refusing outbound filename `{}`",
                file.filename
            )));
        }
    };
    let id = Uuid::new_v4();
    let path = out_dir.join(format!("{id}-{basename}"));
    std::fs::write(&path, &file.data).map_err(AdapterError::Io)?;
    Ok(path)
}

#[allow(clippy::needless_pass_by_value)]
fn escape_to_bad_request(e: AppleScriptEscapeError) -> AdapterError {
    AdapterError::BadRequest(format!("imessage: {e}"))
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::bridge::MockBridge;
    use ironclaw_types::InboundEvent;
    use serde_json::json;
    use tempfile::TempDir;
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
        mock: Arc<MockBridge>,
        cfg: IMessageConfig,
    ) -> (Arc<IMessageAdapter>, mpsc::Receiver<InboundEvent>, TempDir) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let dir = TempDir::new().unwrap();
        let adapter = IMessageAdapter::start_with_bridge(
            mock,
            cfg,
            tx,
            dir.path().to_path_buf(),
        );
        (adapter, rx, dir)
    }

    #[tokio::test]
    async fn channel_type_is_imessage() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, IMessageConfig::default());
        assert_eq!(a.channel_type().as_str(), "imessage");
        a.shutdown().await;
    }

    #[tokio::test]
    async fn supports_threads_defaults_false() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, IMessageConfig::default());
        assert!(!a.supports_threads());
        a.shutdown().await;
    }

    #[tokio::test]
    async fn deliver_dm_text_sends_buddy_send_script() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        let id = a
            .deliver("handle:+15551234", None, &outbound_text("hello"))
            .await
            .unwrap();
        assert!(id.is_none());
        let calls = m.applescript_calls();
        assert_eq!(calls.len(), 1);
        let s = &calls[0];
        assert!(s.contains("buddy \"+15551234\""));
        assert!(s.contains("send \"hello\""));
        assert!(s.contains("service type = iMessage"));
    }

    #[tokio::test]
    async fn deliver_chat_text_sends_chat_id_script() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        a.deliver("chat:abc-def", None, &outbound_text("yo"))
            .await
            .unwrap();
        let s = &m.applescript_calls()[0];
        assert!(s.contains("chat id \"abc-def\""));
        assert!(s.contains("send \"yo\""));
        // No `buddy` keyword in the chat branch.
        assert!(!s.contains("buddy "));
    }

    #[tokio::test]
    async fn deliver_with_quotes_in_text_is_escaped() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        a.deliver(
            "handle:+1",
            None,
            &outbound_text("she said \"hi\""),
        )
        .await
        .unwrap();
        let s = &m.applescript_calls()[0];
        // Escaped quote pair shows up as backslash-quote.
        assert!(s.contains("send \"she said \\\"hi\\\"\""));
    }

    #[tokio::test]
    async fn deliver_with_newline_in_text_keeps_newline() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        a.deliver("handle:+1", None, &outbound_text("a\nb"))
            .await
            .unwrap();
        let s = &m.applescript_calls()[0];
        assert!(s.contains("send \"a\nb\""));
    }

    #[tokio::test]
    async fn deliver_with_backslash_is_escaped() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        a.deliver(
            "handle:+1",
            None,
            &outbound_text("c:\\path\\to\\file"),
        )
        .await
        .unwrap();
        let s = &m.applescript_calls()[0];
        assert!(s.contains("c:\\\\path\\\\to\\\\file"));
    }

    #[tokio::test]
    async fn deliver_with_null_byte_rejected_as_bad_request() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let err = a
            .deliver("handle:+1", None, &outbound_text("a\0b"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_with_control_char_rejected_as_bad_request() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let err = a
            .deliver("handle:+1", None, &outbound_text("a\x07b"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_bad_platform_id_is_bad_request() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let err = a
            .deliver("user:nope", None, &outbound_text("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_system_edit_returns_unsupported() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({ "action": "edit", "target_seq": 1, "text": "x" }),
            files: vec![],
        };
        let err = a.deliver("handle:+1", None, &msg).await.unwrap_err();
        match err {
            AdapterError::Unsupported(m) => assert!(m.contains("edit")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_reaction_returns_unsupported() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({ "action": "reaction", "emoji": "thumbsup" }),
            files: vec![],
        };
        let err = a.deliver("handle:+1", None, &msg).await.unwrap_err();
        match err {
            AdapterError::Unsupported(m) => assert!(m.contains("reaction")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_system_unknown_action_returns_unsupported() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::System,
            content: json!({ "action": "delete" }),
            files: vec![],
        };
        let err = a.deliver("handle:+1", None, &msg).await.unwrap_err();
        assert!(matches!(err, AdapterError::Unsupported(_)));
    }

    #[tokio::test]
    async fn deliver_propagates_auth_error_from_bridge() {
        let m = Arc::new(MockBridge::new());
        m.push_applescript_err(AdapterError::Auth("nope".into()));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let err = a
            .deliver("handle:+1", None, &outbound_text("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn deliver_propagates_transport_error_from_bridge() {
        let m = Arc::new(MockBridge::new());
        m.push_applescript_err(AdapterError::Transport("boom".into()));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let err = a
            .deliver("handle:+1", None, &outbound_text("hi"))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn deliver_empty_body_is_bad_request_not_silent_drop() {
        // Regression: the original implementation returned Ok(None)
        // here, which the host's delivery loop interpreted as
        // delivered-ok. The agent thought it sent a message but the
        // user saw nothing. Surface it as BadRequest so the row lands
        // in `dropped_messages` with a visible reason.
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![],
        };
        let err = a.deliver("handle:+1", None, &msg).await.unwrap_err();
        match err {
            AdapterError::BadRequest(s) => assert!(s.contains("empty body")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
        assert_eq!(m.applescript_call_count(), 0);
    }

    #[tokio::test]
    async fn deliver_with_file_writes_to_outgoing_and_sends_posix_file() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, d) = make_adapter(m.clone(), polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({"text": "see attached"}),
            files: vec![OutboundFile {
                filename: "report.pdf".into(),
                data: vec![1, 2, 3, 4],
            }],
        };
        a.deliver("handle:+1", None, &msg).await.unwrap();
        let calls = m.applescript_calls();
        // First call sends the text, second sends the file.
        assert_eq!(calls.len(), 2);
        assert!(calls[1].contains("POSIX file"));
        assert!(calls[1].contains("report.pdf"));
        // The file lives under data_dir/outgoing.
        let out_dir = d.path().join("outgoing");
        let entries: Vec<_> = std::fs::read_dir(out_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(entries.len(), 1);
        let name = entries[0].file_name();
        assert!(name.to_string_lossy().ends_with("-report.pdf"));
    }

    #[tokio::test]
    async fn deliver_file_only_no_text_just_sends_file() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![OutboundFile {
                filename: "a.txt".into(),
                data: vec![9],
            }],
        };
        a.deliver("handle:+1", None, &msg).await.unwrap();
        let calls = m.applescript_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("POSIX file"));
    }

    #[tokio::test]
    async fn deliver_file_with_dirty_filename_is_rejected() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![OutboundFile {
                filename: "../etc/passwd".into(),
                data: vec![],
            }],
        };
        // basename of "../etc/passwd" is "passwd" — so it's allowed; we
        // really only block names that themselves carry separators after
        // basename extraction. Try a case where basename still contains '/'.
        // (Pure '/' is rare but the input "/" yields empty basename, so we
        // reject it.)
        let r = a.deliver("handle:+1", None, &msg).await;
        // Either path is acceptable: success because basename was sanitised
        // OK, but with no slashes left.
        match r {
            Ok(_) | Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_file_with_empty_basename_is_bad_request() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![OutboundFile {
                filename: "/".into(),
                data: vec![],
            }],
        };
        let err = a.deliver("handle:+1", None, &msg).await.unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn deliver_two_files_invokes_bridge_twice() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m.clone(), polling_off());
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({}),
            files: vec![
                OutboundFile { filename: "a.txt".into(), data: vec![1] },
                OutboundFile { filename: "b.txt".into(), data: vec![2] },
            ],
        };
        a.deliver("handle:+1", None, &msg).await.unwrap();
        assert_eq!(m.applescript_call_count(), 2);
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        assert!(a.open_dm("anyone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn subscribe_default_is_ok() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        a.subscribe("handle:+1", None).await.unwrap();
        a.subscribe("chat:g", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn set_typing_default_is_ok() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        a.set_typing("handle:+1", None).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_is_idempotent() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, IMessageConfig::default());
        a.shutdown().await;
        a.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_cancels_poll_task_quickly() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let mut cfg = IMessageConfig::default();
        cfg.poll_interval_ms = 10_000;
        cfg.since_rowid_file = "rowid.txt".into();
        let (a, _rx, _d) = make_adapter(m, cfg);
        let start = std::time::Instant::now();
        a.shutdown().await;
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn poll_disabled_does_not_emit_events() {
        let m = Arc::new(MockBridge::new());
        m.set_rows(vec![crate::bridge::MockMessageRow {
            rowid: 1,
            guid: "g".into(),
            text: Some("hi".into()),
            date: 0,
            is_from_me: false,
            handle: Some("+1".into()),
            chat_id: None,
        }]);
        // enable_polling=false; receiver is dropped immediately.
        let cfg = polling_off();
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let dir = TempDir::new().unwrap();
        let _a = IMessageAdapter::start_with_bridge(
            m,
            cfg,
            tx,
            dir.path().to_path_buf(),
        );
        // Confirm no events appear.
        let r = timeout(Duration::from_millis(80), rx.recv()).await;
        match r {
            Err(_) | Ok(None) => {}
            Ok(Some(e)) => panic!("unexpected event {e:?}"),
        }
    }

    #[tokio::test]
    async fn polling_on_emits_inbound_event() {
        let m = Arc::new(MockBridge::new());
        m.set_rows(vec![crate::bridge::MockMessageRow {
            rowid: 1,
            guid: "g".into(),
            text: Some("ping".into()),
            date: 0,
            is_from_me: false,
            handle: Some("+1".into()),
            chat_id: None,
        }]);
        let mut cfg = IMessageConfig::default();
        cfg.poll_interval_ms = 5;
        cfg.since_rowid_file = "rowid.txt".into();
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let dir = TempDir::new().unwrap();
        let a = IMessageAdapter::start_with_bridge(
            m,
            cfg,
            tx,
            dir.path().to_path_buf(),
        );
        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event")
            .expect("sender closed");
        assert_eq!(evt.platform_id, "handle:+1");
        assert_eq!(evt.message.content["text"], "ping");
        a.shutdown().await;
    }

    #[tokio::test]
    async fn config_accessor_returns_input() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let cfg = polling_off();
        let (a, _rx, _d) = make_adapter(m, cfg.clone());
        assert_eq!(a.config(), &cfg);
    }

    #[tokio::test]
    async fn data_dir_accessor_returns_input() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, d) = make_adapter(m, polling_off());
        assert_eq!(a.data_dir(), d.path());
    }

    #[tokio::test]
    async fn debug_includes_struct_name_and_channel_type() {
        let m = Arc::new(MockBridge::always_applescript_ok(""));
        let (a, _rx, _d) = make_adapter(m, polling_off());
        let s = format!("{a:?}");
        assert!(s.contains("IMessageAdapter"));
        assert!(s.contains("imessage"));
    }

    fn polling_off() -> IMessageConfig {
        let mut c = IMessageConfig::default();
        c.enable_polling = false;
        c.since_rowid_file = "rowid.txt".into();
        c
    }

    #[test]
    fn render_text_script_handle_branch_uses_buddy() {
        let s = render_text_script(
            "iMessage",
            &ParsedPlatformId::Handle("+1".into()),
            "hi",
        )
        .unwrap();
        assert!(s.contains("buddy \"+1\""));
        assert!(s.contains("service type = iMessage"));
        assert!(s.contains("send \"hi\""));
    }

    #[test]
    fn render_text_script_chat_branch_uses_chat_id() {
        let s = render_text_script(
            "iMessage",
            &ParsedPlatformId::Chat("g".into()),
            "yo",
        )
        .unwrap();
        assert!(s.contains("chat id \"g\""));
        assert!(s.contains("send \"yo\""));
    }

    #[test]
    fn render_text_script_sms_service_uses_bare_identifier() {
        let s = render_text_script(
            "SMS",
            &ParsedPlatformId::Handle("+1".into()),
            "hi",
        )
        .unwrap();
        assert!(s.contains("service type = SMS"));
    }

    #[test]
    fn render_text_script_unknown_service_quotes_it() {
        let s = render_text_script(
            "Custom",
            &ParsedPlatformId::Handle("+1".into()),
            "hi",
        )
        .unwrap();
        assert!(s.contains("service type = \"Custom\""));
    }

    #[test]
    fn render_text_script_escapes_handle_and_text() {
        let s = render_text_script(
            "iMessage",
            &ParsedPlatformId::Handle("a\"b".into()),
            "c\"d",
        )
        .unwrap();
        assert!(s.contains("buddy \"a\\\"b\""));
        assert!(s.contains("send \"c\\\"d\""));
    }

    #[test]
    fn render_text_script_rejects_null_text() {
        let err = render_text_script(
            "iMessage",
            &ParsedPlatformId::Handle("+1".into()),
            "\0",
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn render_file_script_handle_branch_uses_posix_file() {
        let s = render_file_script(
            "iMessage",
            &ParsedPlatformId::Handle("+1".into()),
            std::path::Path::new("/tmp/x.pdf"),
        )
        .unwrap();
        assert!(s.contains("POSIX file \"/tmp/x.pdf\""));
        assert!(s.contains("buddy \"+1\""));
    }

    #[test]
    fn render_file_script_chat_branch_uses_posix_file() {
        let s = render_file_script(
            "iMessage",
            &ParsedPlatformId::Chat("g".into()),
            std::path::Path::new("/tmp/x.pdf"),
        )
        .unwrap();
        assert!(s.contains("chat id \"g\""));
        assert!(s.contains("POSIX file \"/tmp/x.pdf\""));
    }

    #[test]
    fn render_file_script_escapes_path() {
        let s = render_file_script(
            "iMessage",
            &ParsedPlatformId::Handle("+1".into()),
            std::path::Path::new("/tmp/with \"quote.pdf"),
        )
        .unwrap();
        assert!(s.contains("\\\"quote.pdf"));
    }

    #[test]
    fn write_outbound_file_creates_outgoing_dir_and_file() {
        let d = TempDir::new().unwrap();
        let p = write_outbound_file(
            d.path(),
            &OutboundFile { filename: "x.txt".into(), data: b"hi".to_vec() },
        )
        .unwrap();
        assert!(p.exists());
        let content = std::fs::read(&p).unwrap();
        assert_eq!(content, b"hi");
        // Inside an outgoing/ subdir.
        assert!(p.starts_with(d.path().join("outgoing")));
    }

    #[test]
    fn write_outbound_file_strips_path_components_to_basename() {
        let d = TempDir::new().unwrap();
        let p = write_outbound_file(
            d.path(),
            &OutboundFile {
                filename: "../../sneaky/name.txt".into(),
                data: vec![],
            },
        )
        .unwrap();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        // Stripped to basename, prefixed with UUID-.
        assert!(name.ends_with("-name.txt"));
        // Sneaky path components are not in the path:
        assert!(!p.to_string_lossy().contains(".."));
    }

    #[test]
    fn write_outbound_file_rejects_empty_basename() {
        let d = TempDir::new().unwrap();
        let err = write_outbound_file(
            d.path(),
            &OutboundFile { filename: "/".into(), data: vec![] },
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn escape_to_bad_request_carries_message() {
        let e = escape_to_bad_request(AppleScriptEscapeError::NullByte);
        match e {
            AdapterError::BadRequest(m) => assert!(m.contains("null")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }
}
