//! CLI (stdin/stdout) channel adapter — used for tests and local REPL.
//!
//! See `PLAN.md` § 6 (T6a).
//!
//! Each line read from `stdin` becomes an `InboundEvent` with
//! `channel_type = "cli"`, `platform_id = "stdin"`, and a chat
//! `InboundMessage` carrying `{"text": <line>}`.
//!
//! Each outbound message is rendered to `stdout` as a single line prefixed
//! with a configurable label (default `"agent> "`).
//!
//! For tests the reader and writer are pluggable; the default
//! [`CliFactory`] binds to `tokio::io::stdin`/`tokio::io::stdout`.

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_channels_core::{
    AdapterError, ChannelAdapter, ChannelFactory, ChannelSetup, ContainerContribution, DmHandle,
};
use ironclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, OutboundMessage, SenderIdentity,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

/// Default prefix prepended to each outbound message rendered on stdout.
pub const DEFAULT_LABEL: &str = "agent> ";

/// Channel-type string used by this channel (`"cli"`).
pub const CHANNEL_TYPE_STR: &str = ChannelType::CLI;

/// Configuration read from `ChannelSetup::config`.
///
/// All fields optional: an empty `{}` is valid and produces the defaults.
#[derive(Debug, Clone)]
struct CliConfig {
    label: String,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            label: DEFAULT_LABEL.to_owned(),
        }
    }
}

impl CliConfig {
    fn from_value(value: &Value) -> Result<Self, AdapterError> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let obj = value
            .as_object()
            .ok_or_else(|| AdapterError::BadRequest("cli config must be a JSON object".into()))?;
        let label = match obj.get("label") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => DEFAULT_LABEL.to_owned(),
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "cli config field `label` must be a string".into(),
                ));
            }
        };
        Ok(Self { label })
    }
}

/// CLI channel adapter.
///
/// Holds the shared writer (behind a `Mutex` so concurrent `deliver` calls
/// produce intact lines) and the join handle for the stdin-reader task.
pub struct CliAdapter {
    channel_type: ChannelType,
    label: String,
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
}

impl CliAdapter {
    /// Construct an adapter with explicit reader/writer. Spawns a background
    /// task that reads lines from `reader` and pushes [`InboundEvent`]s to
    /// `inbound_tx` until EOF or the channel is closed.
    pub fn new_with_io<R, W>(
        reader: R,
        writer: W,
        inbound_tx: Sender<InboundEvent>,
        label: impl Into<String>,
    ) -> Self
    where
        R: AsyncBufRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let task = tokio::spawn(read_loop(reader, inbound_tx));
        Self {
            channel_type: ChannelType::new(ChannelType::CLI),
            label: label.into(),
            writer: Arc::new(Mutex::new(Box::new(writer))),
            reader_task: Mutex::new(Some(task)),
        }
    }

    /// Construct an adapter bound to `tokio::io::stdin`/`tokio::io::stdout`.
    pub fn new_stdio(inbound_tx: Sender<InboundEvent>, label: impl Into<String>) -> Self {
        Self::new_with_io(
            BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
            inbound_tx,
            label,
        )
    }

    /// Abort the background stdin reader (used by tests; not part of the
    /// trait surface).
    pub async fn shutdown_reader(&self) {
        if let Some(handle) = self.reader_task.lock().await.take() {
            handle.abort();
        }
    }
}

async fn read_loop<R>(reader: R, tx: Sender<InboundEvent>)
where
    R: AsyncBufRead + Send + Unpin + 'static,
{
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let event = build_inbound_event(&line);
                if tx.send(event).await.is_err() {
                    // Receiver dropped — exit loop quietly.
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::warn!(error = %err, "cli channel stdin read failed");
                break;
            }
        }
    }
}

fn build_inbound_event(line: &str) -> InboundEvent {
    InboundEvent {
        channel_type: ChannelType::new(ChannelType::CLI),
        platform_id: "stdin".to_owned(),
        thread_id: None,
        message: InboundMessage {
            id: uuid::Uuid::new_v4().to_string(),
            kind: MessageKind::Chat,
            content: json!({ "text": line }),
            timestamp: Utc::now(),
            is_mention: None,
            is_group: None,
        },
        reply_to: None,
        sender: Some(SenderIdentity {
            channel_type: ChannelType::new(ChannelType::CLI),
            identity: "local".to_owned(),
            display_name: Some("local".to_owned()),
        }),
    }
}

/// Extract a human-readable line from an [`OutboundMessage`].
///
/// - If `content` is an object containing a `text` string, use it verbatim.
/// - Otherwise, render the JSON in compact form. This keeps tests
///   deterministic when other channels' shapes leak through.
fn render_outbound(message: &OutboundMessage) -> String {
    let body = if let Some(text) = message
        .content
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        text
    } else {
        message.content.to_string()
    };

    if message.files.is_empty() {
        body
    } else {
        let names: Vec<&str> = message.files.iter().map(|f| f.filename.as_str()).collect();
        if body.is_empty() {
            format!("[files: {}]", names.join(", "))
        } else {
            format!("{body} [files: {}]", names.join(", "))
        }
    }
}

#[async_trait]
impl ChannelAdapter for CliAdapter {
    fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    // Threads, subscribe, set_typing, open_dm: use trait defaults
    // (Ok(()) / Ok(None) / false). They are explicit no-ops here because
    // stdin/stdout doesn't have those concepts.
    async fn set_typing(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn deliver(
        &self,
        _platform_id: &str,
        _thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        let line = render_outbound(message);
        let mut guard = self.writer.lock().await;
        guard.write_all(self.label.as_bytes()).await?;
        guard.write_all(line.as_bytes()).await?;
        guard.write_all(b"\n").await?;
        guard.flush().await?;
        Ok(None)
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }
}

/// Factory for [`CliAdapter`].
///
/// Default `init` reads `setup.config` for an optional `label` string and
/// binds to `tokio::io::stdin`/`tokio::io::stdout`. Tests that need a
/// hermetic reader/writer should construct [`CliAdapter`] directly via
/// [`CliAdapter::new_with_io`].
#[derive(Debug, Default)]
pub struct CliFactory;

impl CliFactory {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ChannelFactory for CliFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(ChannelType::CLI)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let config = CliConfig::from_value(&setup.config)?;
        Ok(Arc::new(CliAdapter::new_stdio(
            setup.inbound_tx,
            config.label,
        )))
    }

    async fn shutdown(&self) -> Result<(), AdapterError> {
        Ok(())
    }

    fn container_contribution(&self) -> ContainerContribution {
        // CLI channel needs nothing inside the container — agents speak to
        // it through the host's normal inbound/outbound rails.
        ContainerContribution::default()
    }
}

/// Register this channel's factory with a [`ChannelRegistry`].
///
/// The per-channel `register(reg: &mut ChannelRegistry)` function is the
/// pattern called out in `PLAN.md` § 6 (T6).
///
/// [`ChannelRegistry`]: ironclaw_channels_core::ChannelRegistry
pub fn register(
    registry: &mut ironclaw_channels_core::ChannelRegistry,
) -> Result<(), AdapterError> {
    registry.register(Arc::new(CliFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_channels_core::ChannelRegistry;
    use ironclaw_types::OutboundFile;
    use std::io::Cursor;
    use tokio::io::BufReader;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    fn outbound_text(text: &str) -> OutboundMessage {
        OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": text }),
            files: vec![],
        }
    }

    #[tokio::test]
    async fn stdin_lines_become_inbound_events() {
        let input = "hello\nworld\n";
        let reader = BufReader::new(Cursor::new(input.as_bytes()));
        let writer: Vec<u8> = Vec::new();
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let _adapter = CliAdapter::new_with_io(reader, writer, tx, DEFAULT_LABEL);

        let evt1 = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt1.channel_type.as_str(), "cli");
        assert_eq!(evt1.platform_id, "stdin");
        assert_eq!(evt1.thread_id, None);
        assert_eq!(evt1.message.kind, MessageKind::Chat);
        assert_eq!(evt1.message.content["text"], "hello");
        let sender = evt1.sender.expect("sender present");
        assert_eq!(sender.identity, "local");
        assert_eq!(sender.display_name.as_deref(), Some("local"));
        assert_eq!(sender.channel_type.as_str(), "cli");

        let evt2 = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt2.message.content["text"], "world");
    }

    #[tokio::test]
    async fn empty_input_produces_no_events() {
        let reader = BufReader::new(Cursor::new(b""));
        let writer: Vec<u8> = Vec::new();
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(2);
        let _adapter = CliAdapter::new_with_io(reader, writer, tx, DEFAULT_LABEL);

        let res = timeout(Duration::from_millis(100), rx.recv()).await;
        match res {
            // sender dropped after EOF (channel closed) or timeout (no events
            // arrived) — both are valid no-event outcomes.
            Ok(None) | Err(_) => {}
            Ok(Some(evt)) => panic!("unexpected event: {evt:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_writes_label_and_text() {
        let reader = BufReader::new(Cursor::new(b""));
        let buf: Vec<u8> = Vec::new();
        let writer = Cursor::new(buf);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        // Use a duplex-able writer: capture in a Vec via Arc<Mutex<_>>.
        // Simplest path: write into an in-memory buffer behind the adapter
        // by passing it as the writer; then read it out via shutdown_reader trick.
        // Instead, use a `tokio::io::duplex` so we can read what was written.
        drop((tx, writer));

        let (mut client, server) = tokio::io::duplex(1024);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, "agent> ");
        let id = adapter
            .deliver("ignored", None, &outbound_text("hi"))
            .await
            .unwrap();
        assert!(id.is_none());

        // Read what was written to `client`.
        let mut out = vec![0u8; 64];
        let n = tokio::io::AsyncReadExt::read(&mut client, &mut out)
            .await
            .unwrap();
        let text = std::str::from_utf8(&out[..n]).unwrap();
        assert_eq!(text, "agent> hi\n");
    }

    #[tokio::test]
    async fn deliver_renders_non_text_content_as_json() {
        let (mut client, server) = tokio::io::duplex(1024);
        let reader = BufReader::new(Cursor::new(b""));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, "");

        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "shape": "rect" }),
            files: vec![],
        };
        adapter.deliver("p", None, &msg).await.unwrap();

        let mut out = vec![0u8; 128];
        let n = tokio::io::AsyncReadExt::read(&mut client, &mut out)
            .await
            .unwrap();
        let text = std::str::from_utf8(&out[..n]).unwrap();
        // Object with a single key — compact JSON form.
        assert_eq!(text, "{\"shape\":\"rect\"}\n");
    }

    #[tokio::test]
    async fn deliver_appends_attachment_list() {
        let (mut client, server) = tokio::io::duplex(1024);
        let reader = BufReader::new(Cursor::new(b""));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, "agent> ");

        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": "see attached" }),
            files: vec![
                OutboundFile {
                    filename: "a.txt".into(),
                    data: vec![1, 2, 3],
                },
                OutboundFile {
                    filename: "b.png".into(),
                    data: vec![],
                },
            ],
        };
        adapter.deliver("p", None, &msg).await.unwrap();

        let mut out = vec![0u8; 128];
        let n = tokio::io::AsyncReadExt::read(&mut client, &mut out)
            .await
            .unwrap();
        let text = std::str::from_utf8(&out[..n]).unwrap();
        assert_eq!(text, "agent> see attached [files: a.txt, b.png]\n");
    }

    #[tokio::test]
    async fn deliver_attachment_only_when_text_empty() {
        let (mut client, server) = tokio::io::duplex(1024);
        let reader = BufReader::new(Cursor::new(b""));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, "");

        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": "" }),
            files: vec![OutboundFile {
                filename: "x.bin".into(),
                data: vec![],
            }],
        };
        adapter.deliver("p", None, &msg).await.unwrap();

        let mut out = vec![0u8; 128];
        let n = tokio::io::AsyncReadExt::read(&mut client, &mut out)
            .await
            .unwrap();
        let text = std::str::from_utf8(&out[..n]).unwrap();
        assert_eq!(text, "[files: x.bin]\n");
    }

    #[tokio::test]
    async fn set_typing_is_noop_ok() {
        let reader = BufReader::new(Cursor::new(b""));
        let (_, server) = tokio::io::duplex(8);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, DEFAULT_LABEL);
        adapter.set_typing("p", None).await.unwrap();
        adapter.set_typing("p", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_default_is_ok() {
        let reader = BufReader::new(Cursor::new(b""));
        let (_, server) = tokio::io::duplex(8);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, DEFAULT_LABEL);
        adapter.subscribe("p", None).await.unwrap();
        adapter.subscribe("p", Some("t")).await.unwrap();
    }

    #[tokio::test]
    async fn open_dm_returns_none() {
        let reader = BufReader::new(Cursor::new(b""));
        let (_, server) = tokio::io::duplex(8);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, DEFAULT_LABEL);
        assert!(adapter.open_dm("anyone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn supports_threads_default_false() {
        let reader = BufReader::new(Cursor::new(b""));
        let (_, server) = tokio::io::duplex(8);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, DEFAULT_LABEL);
        assert!(!adapter.supports_threads());
        assert_eq!(adapter.channel_type().as_str(), "cli");
    }

    #[tokio::test]
    async fn factory_reports_channel_type_and_empty_contribution() {
        let f = CliFactory::new();
        assert_eq!(f.channel_type().as_str(), "cli");
        assert!(f.container_contribution().is_empty());
    }

    #[tokio::test]
    async fn factory_init_with_default_config() {
        // Exercise both `new` and the `Default` impl.
        let f: CliFactory = <CliFactory as Default>::default();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({}), tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "cli");
    }

    #[tokio::test]
    async fn factory_init_with_null_config() {
        let f = CliFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(Value::Null, tx, "/tmp");
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "cli");
    }

    #[tokio::test]
    async fn factory_init_with_label_override() {
        let f = CliFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({ "label": "ic> " }), tx, "/tmp");
        // We can't introspect the label directly but we can confirm init succeeds.
        let _adapter = f.init(setup).await.unwrap();
    }

    #[tokio::test]
    async fn factory_init_rejects_non_object_config() {
        let f = CliFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!("not an object"), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn factory_init_rejects_non_string_label() {
        let f = CliFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(json!({ "label": 42 }), tx, "/tmp");
        match f.init(setup).await {
            Err(AdapterError::BadRequest(_)) => {}
            Err(other) => panic!("expected BadRequest, got {other:?}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn factory_shutdown_is_ok() {
        let f = CliFactory::new();
        f.shutdown().await.unwrap();
    }

    #[test]
    fn register_inserts_cli_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("cli")).is_some());
        // Re-registering is an error per registry contract.
        let err = register(&mut reg).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[tokio::test]
    async fn shutdown_reader_aborts_background_task() {
        let reader = BufReader::new(Cursor::new(b""));
        let (_, server) = tokio::io::duplex(8);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, DEFAULT_LABEL);
        adapter.shutdown_reader().await;
        // Calling again is a safe no-op.
        adapter.shutdown_reader().await;
    }

    #[test]
    fn render_outbound_text_path() {
        let msg = outbound_text("hi");
        assert_eq!(render_outbound(&msg), "hi");
    }

    #[test]
    fn build_inbound_event_shape() {
        let evt = build_inbound_event("line");
        assert_eq!(evt.channel_type.as_str(), "cli");
        assert_eq!(evt.platform_id, "stdin");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.content["text"], "line");
        assert!(!evt.message.id.is_empty());
        // Two ids in a row must differ.
        let a = build_inbound_event("x").message.id;
        let b = build_inbound_event("x").message.id;
        assert_ne!(a, b);
    }

    #[test]
    fn cli_config_defaults_when_null() {
        let c = CliConfig::from_value(&Value::Null).unwrap();
        assert_eq!(c.label, DEFAULT_LABEL);
    }

    #[test]
    fn cli_config_uses_object_label() {
        let c = CliConfig::from_value(&json!({"label": "x> "})).unwrap();
        assert_eq!(c.label, "x> ");
    }

    #[test]
    fn cli_config_defaults_missing_label() {
        let c = CliConfig::from_value(&json!({})).unwrap();
        assert_eq!(c.label, DEFAULT_LABEL);
    }

    #[test]
    fn cli_config_null_label_in_object_is_default() {
        let c = CliConfig::from_value(&json!({"label": null})).unwrap();
        assert_eq!(c.label, DEFAULT_LABEL);
    }

    #[test]
    fn channel_type_str_constant_matches() {
        assert_eq!(CHANNEL_TYPE_STR, "cli");
    }
}
