//! CLI channel adapter — used for tests, local REPL, and the
//! `cclaw chat` bridge.
//!
//! Two modes:
//!
//! 1. **stdio mode** (default). Reads lines from the host's own
//!    `tokio::io::stdin()` and writes labelled outbound replies to
//!    `tokio::io::stdout()`. This is the developer REPL used by
//!    `cargo run -p copperclaw-host run` in a terminal.
//!
//! 2. **FIFO/log mode**. Reads lines from a named pipe and appends
//!    outbound replies to a plain log file. This is the bridge that
//!    backs `cclaw chat`: the cli command writes to the FIFO and tails
//!    the log, while the host (running anywhere — foreground, systemd,
//!    `copperclaw start` background daemon) reads the FIFO and appends
//!    to the log.
//!
//! ## FIFO survival across writer drain
//!
//! The FIFO is opened with `O_RDWR | O_NONBLOCK`. Holding a writer
//! handle ourselves keeps the kernel from sending us EOF when every
//! external writer (e.g. an `cclaw chat` invocation) closes the FIFO.
//! Without this the read loop would hit EOF on the first Ctrl-D and
//! the cli channel would die for the rest of the host's lifetime.
//!
//! ## Wire format
//!
//! **Inbound** (both modes): one message per line, plain UTF-8 text, no
//! JSON wrapping. Each inbound line becomes an `InboundEvent` with
//! `channel_type = "cli"`, `platform_id = "stdin"` (legacy name — kept
//! stable so existing messaging-group rows match regardless of which
//! mode the host runs in), and a chat `InboundMessage` carrying
//! `{"text": <line>}`.
//!
//! **Outbound** depends on the mode:
//!
//! - stdio mode writes legacy labelled text (`agent> hello`) so the
//!   developer REPL stays human-readable.
//! - FIFO/log mode appends one [`CliFrame`] JSONL frame per delivery so
//!   structured payloads (breadcrumbs, todo lists, diffs, …) survive to
//!   the `cclaw chat` renderer. See [`CliFrame`] for the wire contract
//!   and the per-line sniff rule readers use to tell the two formats
//!   apart.
//!
//! See `PLAN.md` § 6 (T6a).

use async_trait::async_trait;
use chrono::Utc;
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, Card, ChannelAdapter, ChannelFactory, ChannelSetup,
    ContainerContribution, DiffCard, DmHandle, ErrorCard, ThinkingBlock, TodoList,
};
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, OutboundMessage, SenderIdentity,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

/// Default prefix prepended to each outbound message rendered on stdout.
pub const DEFAULT_LABEL: &str = "agent> ";

/// Channel-type string used by this channel (`"cli"`).
pub const CHANNEL_TYPE_STR: &str = ChannelType::CLI;

/// One structured outbound frame in the FIFO/log-mode `chat.log` stream.
///
/// ## Wire contract (stable — `cclaw chat` deserializes these)
///
/// In FIFO/log mode every outbound delivery appends exactly ONE line to
/// the log file: the compact `serde_json` encoding of this enum. The
/// `kind` field is the discriminator and the structured payload lives
/// under a sibling field named after the kind, reusing the core schema
/// types' existing serde shapes (`Breadcrumb`, `TodoList`, `DiffCard`,
/// …) so the wire matches the `messages-out.jsonl` conventions and
/// readers deserialize with the same structs the runner emitted:
///
/// ```json
/// {"kind":"chat","text":"hello"}
/// {"kind":"breadcrumb","breadcrumb":{"tool_name":"shell","status":"running"}}
/// {"kind":"todo_list","todo_list":{"items":[{"id":1,"text":"a","status":"pending"}]}}
/// ```
///
/// ## Sniff rule for readers
///
/// Logs written before this format existed contain legacy labelled text
/// (`agent> hello`). Readers MUST sniff per line:
/// `line.starts_with('{')` means "parse as a `CliFrame`", anything else
/// is legacy plain text. A frame line always starts with `{`, and a
/// legacy line never does under the default `"agent> "` label. `cclaw
/// chat` seeks to `SeekFrom::End(0)` before tailing, so mixed-era
/// history is never replayed and the format transition is free.
///
/// JSON string escaping keeps embedded newlines inside the frame, so a
/// multi-line body is always exactly one log line — fixing the legacy
/// corruption where the label prefixed only the first physical line of
/// a multi-line reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CliFrame {
    /// Plain chat text, plus attachment filenames when the outbound row
    /// carried files.
    Chat {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },
    /// A portable [`Card`].
    Card { card: Card },
    /// A tool [`Breadcrumb`] chip.
    Breadcrumb { breadcrumb: Breadcrumb },
    /// A file-edit [`DiffCard`].
    Diff { diff: DiffCard },
    /// The long-output collapsible treatment of a chat body.
    Collapsible {
        text: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        preview_lines: Vec<String>,
    },
    /// A live [`TodoList`] checklist.
    TodoList { todo_list: TodoList },
    /// A host-emitted [`ErrorCard`].
    Error { error: ErrorCard },
    /// A model-reasoning [`ThinkingBlock`].
    Thinking { thinking: ThinkingBlock },
}

/// Configuration read from `ChannelSetup::config`.
///
/// All fields optional: an empty `{}` is valid and produces the
/// stdio-mode defaults. When `fifo` and/or `log` are present, the
/// adapter switches to the named-pipe bridge used by `cclaw chat`.
#[derive(Debug, Clone)]
struct CliConfig {
    label: String,
    fifo: Option<PathBuf>,
    log: Option<PathBuf>,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            label: DEFAULT_LABEL.to_owned(),
            fifo: None,
            log: None,
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
        let fifo = match obj.get("fifo") {
            Some(Value::String(s)) if !s.is_empty() => Some(PathBuf::from(s)),
            Some(Value::Null | Value::String(_)) | None => None,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "cli config field `fifo` must be a string".into(),
                ));
            }
        };
        let log = match obj.get("log") {
            Some(Value::String(s)) if !s.is_empty() => Some(PathBuf::from(s)),
            Some(Value::Null | Value::String(_)) | None => None,
            Some(_) => {
                return Err(AdapterError::BadRequest(
                    "cli config field `log` must be a string".into(),
                ));
            }
        };
        Ok(Self { label, fifo, log })
    }
}

/// CLI channel adapter.
///
/// Holds the shared writer (behind a `Mutex` so concurrent `deliver` calls
/// produce intact lines) and the join handle for the reader task.
pub struct CliAdapter {
    channel_type: ChannelType,
    label: String,
    /// `true` when the writer is the `chat.log` file (FIFO/log mode):
    /// outbound deliveries are serialized as [`CliFrame`] JSONL frames.
    /// `false` on the stdio path, which keeps the legacy labelled-text
    /// formatting for the human-facing developer REPL.
    structured: bool,
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    /// When the adapter is in FIFO mode we keep a writer handle to the
    /// FIFO open for the adapter's whole lifetime. This is the
    /// "reader is its own writer" trick that prevents the kernel from
    /// sending us EOF when external writers (e.g. `cclaw chat`)
    /// disconnect. Dropping this handle when the adapter is dropped
    /// closes the kernel-side write end cleanly.
    _fifo_writer_keepalive: Option<FifoKeepalive>,
}

/// Owned file descriptor whose only job is to keep the FIFO's write
/// side open. On Unix we hold a `std::fs::File`; on non-Unix this is
/// just a placeholder that the code never constructs (FIFO mode
/// returns an error before reaching the field on those platforms).
#[cfg(unix)]
type FifoKeepalive = std::fs::File;
#[cfg(not(unix))]
type FifoKeepalive = ();

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
            structured: false,
            writer: Arc::new(Mutex::new(Box::new(writer))),
            reader_task: Mutex::new(Some(task)),
            _fifo_writer_keepalive: None,
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

    /// Construct an adapter in FIFO/log mode.
    ///
    /// `fifo` and `log` are optional independently: when only `fifo` is
    /// set, replies still go to `stdout`; when only `log` is set,
    /// inbound still reads from `stdin`. Practically the host wires
    /// both, but the partial modes keep tests and tooling flexible.
    ///
    /// Returns the adapter, or an `AdapterError::Io` if the FIFO can't
    /// be created, the log can't be touched, or the FIFO can't be
    /// opened with `O_RDWR | O_NONBLOCK`.
    pub async fn new_with_paths(
        fifo: Option<&Path>,
        log: Option<&Path>,
        inbound_tx: Sender<InboundEvent>,
        label: impl Into<String>,
    ) -> Result<Self, AdapterError> {
        #[cfg(unix)]
        let mut keepalive: Option<FifoKeepalive> = None;
        #[cfg(not(unix))]
        let keepalive: Option<FifoKeepalive> = None;

        let reader_task: JoinHandle<()> = if let Some(fifo_path) = fifo {
            #[cfg(unix)]
            {
                ensure_fifo(fifo_path).map_err(|e| {
                    AdapterError::Io(std::io::Error::new(
                        e.kind(),
                        format!("cli channel: ensure fifo {}: {e}", fifo_path.display()),
                    ))
                })?;
                let (receiver, keep) = open_fifo_receiver(fifo_path).map_err(|e| {
                    AdapterError::Io(std::io::Error::new(
                        e.kind(),
                        format!("cli channel: open fifo {}: {e}", fifo_path.display()),
                    ))
                })?;
                keepalive = Some(keep);
                let reader = BufReader::new(receiver);
                tokio::spawn(read_loop(reader, inbound_tx))
            }
            #[cfg(not(unix))]
            {
                let _ = fifo_path;
                return Err(AdapterError::Io(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "FIFO mode is only supported on Unix",
                )));
            }
        } else {
            tokio::spawn(read_loop(BufReader::new(tokio::io::stdin()), inbound_tx))
        };

        let writer: Box<dyn AsyncWrite + Send + Unpin> = if let Some(log_path) = log {
            let file = ensure_and_open_log(log_path).await.map_err(|e| {
                AdapterError::Io(std::io::Error::new(
                    e.kind(),
                    format!("cli channel: open log {}: {e}", log_path.display()),
                ))
            })?;
            Box::new(file)
        } else {
            Box::new(tokio::io::stdout())
        };

        Ok(Self {
            channel_type: ChannelType::new(ChannelType::CLI),
            label: label.into(),
            // Structured JSONL frames only when writing to the log
            // file; when `log` is unset replies still go to stdout,
            // which keeps the human-readable legacy formatting.
            structured: log.is_some(),
            writer: Arc::new(Mutex::new(writer)),
            reader_task: Mutex::new(Some(reader_task)),
            _fifo_writer_keepalive: keepalive,
        })
    }

    /// Serialize `frame` compactly and append it as one line to the
    /// writer (log-mode only). One frame per line is the invariant the
    /// [`CliFrame`] sniff rule depends on — `serde_json` string escaping
    /// guarantees the encoded frame itself contains no raw newline.
    async fn write_frame(&self, frame: &CliFrame) -> Result<(), AdapterError> {
        let line = serde_json::to_string(frame).map_err(|e| {
            AdapterError::Io(std::io::Error::other(format!("encode cli frame: {e}")))
        })?;
        let mut guard = self.writer.lock().await;
        guard.write_all(line.as_bytes()).await?;
        guard.write_all(b"\n").await?;
        guard.flush().await?;
        Ok(())
    }

    /// Abort the background reader (used by tests; not part of the
    /// trait surface).
    pub async fn shutdown_reader(&self) {
        if let Some(handle) = self.reader_task.lock().await.take() {
            handle.abort();
        }
    }
}

/// Create the FIFO at `path` if it doesn't already exist. Mode is
/// `0o600`. The workspace forbids `unsafe`, so we shell out to
/// `/usr/bin/env mkfifo` rather than call `libc::mkfifo` directly. The
/// extra fork is fine; this runs exactly once per host boot.
fn ensure_fifo(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("0600")
        .arg(path)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "mkfifo exited with {status}"
        )));
    }
    Ok(())
}

/// Open the FIFO at `path` for reading via tokio's
/// [`tokio::net::unix::pipe::Receiver`], plus a sibling keepalive
/// writer handle to prevent EOF when external writers drain.
///
/// Both descriptors are opened with `O_RDWR | O_NONBLOCK`. The
/// receiver is the tokio pipe wrapper (proper epoll/kqueue
/// integration); the keepalive is a plain `std::fs::File` whose only
/// purpose is to keep the kernel's writer-end refcount above zero.
///
/// We open twice rather than dup-ing because `tokio::net::unix::pipe`
/// doesn't expose a public dup-style API; opening the FIFO inode twice
/// gives us two independent descriptors that share the underlying pipe
/// buffer.
#[cfg(unix)]
fn open_fifo_receiver(
    path: &Path,
) -> std::io::Result<(tokio::net::unix::pipe::Receiver, FifoKeepalive)> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true);
    opts.custom_flags(libc::O_NONBLOCK);
    let read_file = opts.open(path)?;

    let mut keep_opts = std::fs::OpenOptions::new();
    keep_opts.read(true).write(true);
    keep_opts.custom_flags(libc::O_NONBLOCK);
    let keepalive = keep_opts.open(path)?;

    let receiver = tokio::net::unix::pipe::Receiver::from_file_unchecked(read_file)?;
    Ok((receiver, keepalive))
}

/// Create the log file if missing (mode `0o600`) and return an
/// append-mode handle. Each `deliver` call flushes after writing so
/// the tailing reader in `cclaw chat` sees output promptly.
async fn ensure_and_open_log(path: &Path) -> std::io::Result<tokio::fs::File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    opts.open(path).await
}

async fn read_loop<R>(reader: R, tx: Sender<InboundEvent>)
where
    R: AsyncBufRead + Send + Unpin + 'static,
{
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                // Ignore stray blank lines so the FIFO opening dance
                // doesn't generate a spurious `{"text":""}` event.
                if line.is_empty() {
                    continue;
                }
                let event = build_inbound_event(&line);
                if tx.send(event).await.is_err() {
                    // Receiver dropped — exit loop quietly.
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                tracing::warn!(error = %err, "cli channel read failed");
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
        if self.structured {
            let text = message
                .content
                .get("text")
                .and_then(Value::as_str)
                .map_or_else(|| message.content.to_string(), str::to_owned);
            let files = message
                .files
                .iter()
                .map(|f| f.filename.clone())
                .collect::<Vec<_>>();
            self.write_frame(&CliFrame::Chat { text, files }).await?;
            return Ok(None);
        }
        let line = render_outbound(message);
        let mut guard = self.writer.lock().await;
        guard.write_all(self.label.as_bytes()).await?;
        guard.write_all(line.as_bytes()).await?;
        guard.write_all(b"\n").await?;
        guard.flush().await?;
        Ok(None)
    }

    // -------------------------------------------------------------------
    // Rich deliver hooks. The delivery service calls these on every
    // adapter regardless of capability; the trait defaults flatten the
    // structured payload to text via `to_text_fallback()` before
    // `deliver` ever runs, which is exactly where cli structure used to
    // die. In log mode each hook serializes its payload as a
    // [`CliFrame`] instead, so `cclaw chat` can render it natively; on
    // the stdio path each hook reproduces the trait-default text
    // flattening so the developer REPL stays human-readable.
    //
    // The log is append-only and `deliver` returns no message id, so
    // `existing_message_id` / `pin_hint` are ignored — the client
    // dedupes and repaints frames itself (M22 Finding 3).
    // -------------------------------------------------------------------

    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        _to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        if self.structured {
            self.write_frame(&CliFrame::Card { card: card.clone() })
                .await?;
            return Ok(None);
        }
        let text = card.to_text_fallback();
        self.deliver_text_capped(platform_id, thread_id, &text)
            .await
    }

    async fn deliver_breadcrumb(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        breadcrumb: &Breadcrumb,
        _existing_message_id: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        if self.structured {
            self.write_frame(&CliFrame::Breadcrumb {
                breadcrumb: breadcrumb.clone(),
            })
            .await?;
            return Ok(None);
        }
        let text = breadcrumb.to_text_fallback();
        self.deliver_text_capped(platform_id, thread_id, &text)
            .await
    }

    async fn deliver_diff(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        diff: &DiffCard,
    ) -> Result<Option<String>, AdapterError> {
        if self.structured {
            self.write_frame(&CliFrame::Diff { diff: diff.clone() })
                .await?;
            return Ok(None);
        }
        let text = diff.to_text_fallback();
        self.deliver_text_capped(platform_id, thread_id, &text)
            .await
    }

    async fn deliver_collapsible(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        text: &str,
        summary: &str,
        preview_lines: &[String],
    ) -> Result<Option<String>, AdapterError> {
        if self.structured {
            self.write_frame(&CliFrame::Collapsible {
                text: text.to_owned(),
                summary: summary.to_owned(),
                preview_lines: preview_lines.to_vec(),
            })
            .await?;
            return Ok(None);
        }
        let body = copperclaw_channels_core::render_collapsible_text_fallback(
            text,
            summary,
            preview_lines,
        );
        self.deliver_text_capped(platform_id, thread_id, &body)
            .await
    }

    async fn deliver_todo_list(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        list: &TodoList,
        _existing_message_id: Option<&str>,
        _pin_hint: bool,
    ) -> Result<Option<String>, AdapterError> {
        if self.structured {
            self.write_frame(&CliFrame::TodoList {
                todo_list: list.clone(),
            })
            .await?;
            return Ok(None);
        }
        let text = list.to_text_fallback();
        self.deliver_text_capped(platform_id, thread_id, &text)
            .await
    }

    async fn deliver_error(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        err: &ErrorCard,
    ) -> Result<Option<String>, AdapterError> {
        if self.structured {
            self.write_frame(&CliFrame::Error { error: err.clone() })
                .await?;
            return Ok(None);
        }
        let text = err.to_text_fallback();
        self.deliver_text_capped(platform_id, thread_id, &text)
            .await
    }

    async fn deliver_thinking(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        thinking: &ThinkingBlock,
    ) -> Result<Option<String>, AdapterError> {
        if self.structured {
            self.write_frame(&CliFrame::Thinking {
                thinking: thinking.clone(),
            })
            .await?;
            return Ok(None);
        }
        let text = thinking.to_text_fallback();
        self.deliver_text_capped(platform_id, thread_id, &text)
            .await
    }

    async fn open_dm(&self, _user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        Ok(None)
    }
}

/// Factory for [`CliAdapter`].
///
/// Default `init` reads `setup.config` for an optional `label` string,
/// `fifo` path, and `log` path. With no paths set the factory binds to
/// `tokio::io::stdin`/`tokio::io::stdout` (the developer REPL); with a
/// `fifo` and `log` set, it bridges `cclaw chat`.
#[derive(Debug, Default)]
pub struct CliFactory {
    fifo: Option<PathBuf>,
    log: Option<PathBuf>,
}

impl CliFactory {
    /// Construct a factory that uses stdio unless overridden by
    /// `setup.config` (`fifo` / `log` keys).
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a factory pre-bound to a FIFO and/or log file. Values
    /// in the `setup.config` JSON still win — this is purely a default
    /// the host can pass via a typed constructor instead of stuffing
    /// the same fields into JSON.
    pub fn with_paths(fifo: Option<PathBuf>, log: Option<PathBuf>) -> Self {
        Self { fifo, log }
    }
}

#[async_trait]
impl ChannelFactory for CliFactory {
    fn channel_type(&self) -> ChannelType {
        ChannelType::new(ChannelType::CLI)
    }

    async fn init(&self, setup: ChannelSetup) -> Result<Arc<dyn ChannelAdapter>, AdapterError> {
        let mut config = CliConfig::from_value(&setup.config)?;
        // Factory-level defaults fill in when the JSON didn't specify.
        if config.fifo.is_none() {
            config.fifo.clone_from(&self.fifo);
        }
        if config.log.is_none() {
            config.log.clone_from(&self.log);
        }

        match (config.fifo.as_deref(), config.log.as_deref()) {
            (None, None) => Ok(Arc::new(CliAdapter::new_stdio(
                setup.inbound_tx,
                config.label,
            ))),
            (fifo, log) => {
                let adapter =
                    CliAdapter::new_with_paths(fifo, log, setup.inbound_tx, config.label).await?;
                Ok(Arc::new(adapter))
            }
        }
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
/// [`ChannelRegistry`]: copperclaw_channels_core::ChannelRegistry
pub fn register(
    registry: &mut copperclaw_channels_core::ChannelRegistry,
) -> Result<(), AdapterError> {
    registry.register(Arc::new(CliFactory::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::ChannelRegistry;
    use copperclaw_types::OutboundFile;
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
    async fn blank_lines_are_skipped() {
        // Stray blank lines (e.g. from the FIFO opening dance, or a
        // user pressing Enter on an empty prompt) MUST NOT generate
        // `{"text":""}` events — that was the original bug.
        let input = "\n\nhello\n\n";
        let reader = BufReader::new(Cursor::new(input.as_bytes()));
        let writer: Vec<u8> = Vec::new();
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let _adapter = CliAdapter::new_with_io(reader, writer, tx, DEFAULT_LABEL);

        let evt = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.content["text"], "hello");
        // No further events should arrive.
        let res = timeout(Duration::from_millis(100), rx.recv()).await;
        match res {
            Ok(None) | Err(_) => {}
            Ok(Some(evt)) => panic!("unexpected event: {evt:?}"),
        }
    }

    #[tokio::test]
    async fn deliver_writes_label_and_text() {
        // stdio mode (`new_with_io`) keeps the LEGACY labelled-text
        // formatting — only FIFO/log mode emits `CliFrame` JSONL. This
        // pins the legacy half of the sniff contract.
        let reader = BufReader::new(Cursor::new(b""));
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
        // stdio mode: legacy formatting (compact JSON body, no frame).
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
        // stdio mode: legacy `[files: …]` suffix formatting.
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
        // stdio mode: legacy attachment-only rendering.
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
        // `render_outbound` is the LEGACY stdio-mode renderer only; the
        // structured log path serializes a `CliFrame` and never calls it.
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
        assert!(c.fifo.is_none());
        assert!(c.log.is_none());
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
    fn cli_config_reads_fifo_and_log_paths() {
        let c =
            CliConfig::from_value(&json!({"fifo": "/tmp/x.fifo", "log": "/tmp/x.log"})).unwrap();
        assert_eq!(c.fifo.as_deref(), Some(Path::new("/tmp/x.fifo")));
        assert_eq!(c.log.as_deref(), Some(Path::new("/tmp/x.log")));
    }

    #[test]
    fn cli_config_empty_string_path_is_none() {
        let c = CliConfig::from_value(&json!({"fifo": "", "log": ""})).unwrap();
        assert!(c.fifo.is_none());
        assert!(c.log.is_none());
    }

    #[test]
    fn channel_type_str_constant_matches() {
        assert_eq!(CHANNEL_TYPE_STR, "cli");
    }

    // -------------------------------------------------------------------
    // FIFO/log mode integration tests
    // -------------------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_mode_round_trip_emits_inbound_event() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("chat.fifo");
        let log = dir.path().join("chat.log");
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let adapter = CliAdapter::new_with_paths(Some(&fifo), Some(&log), tx, "agent> ")
            .await
            .unwrap();
        assert!(fifo.exists(), "FIFO should have been created");
        assert!(log.exists(), "log should have been created");

        // Open the FIFO as a writer and send a line.
        let mut writer = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&fifo)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut writer, b"hello from chat\n")
            .await
            .unwrap();
        drop(writer);

        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.content["text"], "hello from chat");
        assert_eq!(evt.channel_type.as_str(), "cli");
        assert_eq!(evt.platform_id, "stdin");

        // Deliver writes a structured JSONL chat frame to the log
        // (log mode never uses the label — that is stdio-only).
        adapter
            .deliver("p", None, &outbound_text("reply"))
            .await
            .unwrap();
        let log_contents = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(log_contents, "{\"kind\":\"chat\",\"text\":\"reply\"}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_survives_writer_drain() {
        // Open the FIFO via our adapter, then open + drop an external
        // writer, then open + send via another writer. The adapter
        // must still emit the second event — which proves the
        // O_RDWR-keepalive trick prevents EOF on writer disconnect.
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("chat.fifo");
        let log = dir.path().join("chat.log");
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let _adapter = CliAdapter::new_with_paths(Some(&fifo), Some(&log), tx, "agent> ")
            .await
            .unwrap();

        // First "cclaw chat": writes one line and closes.
        {
            let mut w = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&fifo)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut w, b"hello\n")
                .await
                .unwrap();
        }
        let evt1 = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt1.message.content["text"], "hello");

        // Second "cclaw chat" — a real EOF crash would mean this
        // never reaches the adapter.
        {
            let mut w = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&fifo)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut w, b"world\n")
                .await
                .unwrap();
        }
        let evt2 = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt2.message.content["text"], "world");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn log_mode_appends_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("chat.log");
        // Seed an existing line so we can verify append (not truncate).
        tokio::fs::write(&log, b"prior\n").await.unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(2);
        let adapter = CliAdapter::new_with_paths(None, Some(&log), tx, "agent> ")
            .await
            .unwrap();
        adapter
            .deliver("p", None, &outbound_text("first"))
            .await
            .unwrap();
        adapter
            .deliver("p", None, &outbound_text("second"))
            .await
            .unwrap();
        // Appends (never truncates) one JSONL chat frame per delivery;
        // the seeded legacy line is left untouched, which is exactly the
        // mixed-era log the sniff rule handles.
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "prior\n{\"kind\":\"chat\",\"text\":\"first\"}\n{\"kind\":\"chat\",\"text\":\"second\"}\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn factory_init_with_fifo_and_log_config() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("chat.fifo");
        let log = dir.path().join("chat.log");
        let f = CliFactory::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let setup = ChannelSetup::new(
            json!({
                "fifo": fifo.to_string_lossy(),
                "log": log.to_string_lossy(),
            }),
            tx,
            dir.path(),
        );
        let adapter = f.init(setup).await.unwrap();
        assert_eq!(adapter.channel_type().as_str(), "cli");
        assert!(fifo.exists());
        assert!(log.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn factory_with_paths_uses_constructor_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("chat.fifo");
        let log = dir.path().join("chat.log");
        let f = CliFactory::with_paths(Some(fifo.clone()), Some(log.clone()));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        // Empty JSON config — factory falls back to the constructor paths.
        let setup = ChannelSetup::new(json!({}), tx, dir.path());
        let _adapter = f.init(setup).await.unwrap();
        assert!(fifo.exists());
        assert!(log.exists());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_fifo_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.fifo");
        ensure_fifo(&p).unwrap();
        // Second call must not error even though the FIFO already exists.
        ensure_fifo(&p).unwrap();
        assert!(p.exists());
    }

    // -------------------------------------------------------------------
    // Structured JSONL frames (FIFO/log mode). Every test pins the exact
    // frame bytes — the wire contract `cclaw chat` deserializes.
    // -------------------------------------------------------------------

    use copperclaw_channels_core::{
        BreadcrumbStatus, DiffHunk, DiffLine, DiffLineKind, ErrorCardKind, TodoItemStatus,
        TodoListItem,
    };

    /// Build a log-mode adapter (structured JSONL output) writing into a
    /// fresh log file inside `dir`. No FIFO — inbound is irrelevant here.
    async fn log_adapter(dir: &tempfile::TempDir) -> (CliAdapter, PathBuf) {
        let log = dir.path().join("chat.log");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(2);
        let adapter = CliAdapter::new_with_paths(None, Some(&log), tx, "agent> ")
            .await
            .unwrap();
        (adapter, log)
    }

    #[tokio::test]
    async fn log_mode_chat_frame_includes_files() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: json!({ "text": "see attached" }),
            files: vec![
                OutboundFile {
                    filename: "a.txt".into(),
                    data: vec![1],
                },
                OutboundFile {
                    filename: "b.png".into(),
                    data: vec![],
                },
            ],
        };
        adapter.deliver("p", None, &msg).await.unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"chat\",\"text\":\"see attached\",\"files\":[\"a.txt\",\"b.png\"]}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_multiline_body_stays_one_frame_line() {
        // The legacy format corrupted the tail here: only the first
        // physical line carried the label. JSON string escaping keeps
        // the whole body inside a single log line.
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        adapter
            .deliver("p", None, &outbound_text("one\ntwo\nthree"))
            .await
            .unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(body, "{\"kind\":\"chat\",\"text\":\"one\\ntwo\\nthree\"}\n");
        assert_eq!(body.lines().count(), 1, "one frame must be one line");
    }

    #[tokio::test]
    async fn log_mode_breadcrumb_frame_exact_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let bc = Breadcrumb {
            tool_name: "shell".into(),
            detail: Some("cargo check".into()),
            status: BreadcrumbStatus::Running,
            summary: None,
            steps: vec![],
        };
        let id = adapter
            .deliver_breadcrumb("p", None, &bc, None)
            .await
            .unwrap();
        assert!(id.is_none(), "append-only log anchors no edits");
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"breadcrumb\",\"breadcrumb\":{\"tool_name\":\"shell\",\"detail\":\"cargo check\",\"status\":\"running\"}}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_breadcrumb_ignores_existing_message_id() {
        // `existing_message_id` is an edit anchor; the log is append-only
        // so a fresh frame is emitted and no id is returned.
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let bc = Breadcrumb {
            tool_name: "shell".into(),
            detail: None,
            status: BreadcrumbStatus::Done,
            summary: Some("passed (1.8s)".into()),
            steps: vec![],
        };
        let id = adapter
            .deliver_breadcrumb("p", None, &bc, Some("prev-9"))
            .await
            .unwrap();
        assert!(id.is_none());
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"breadcrumb\",\"breadcrumb\":{\"tool_name\":\"shell\",\"status\":\"done\",\"summary\":\"passed (1.8s)\"}}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_todo_list_frame_exact_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let list = TodoList {
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "write tests".into(),
                    status: TodoItemStatus::InProgress,
                    blocked_reason: None,
                },
                TodoListItem {
                    id: 2,
                    text: "run gate".into(),
                    status: TodoItemStatus::Pending,
                    blocked_reason: None,
                },
            ],
            title: Some("Plan".into()),
        };
        let id = adapter
            .deliver_todo_list("p", None, &list, Some("prev-1"), true)
            .await
            .unwrap();
        assert!(id.is_none(), "pin hint and edit anchor are ignored");
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"todo_list\",\"todo_list\":{\"items\":[{\"id\":1,\"text\":\"write tests\",\"status\":\"in_progress\"},{\"id\":2,\"text\":\"run gate\",\"status\":\"pending\"}],\"title\":\"Plan\"}}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_diff_frame_exact_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let diff = DiffCard {
            path: "src/lib.rs".into(),
            language: None,
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Remove,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Add,
                        text: "new".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        adapter.deliver_diff("p", None, &diff).await.unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"diff\",\"diff\":{\"path\":\"src/lib.rs\",\"hunks\":[{\"old_start\":1,\"old_lines\":1,\"new_start\":1,\"new_lines\":1,\"lines\":[{\"kind\":\"remove\",\"text\":\"old\"},{\"kind\":\"add\",\"text\":\"new\"}]}],\"added\":1,\"removed\":1}}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_card_frame_exact_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let card = Card {
            title: Some("Deploy".into()),
            body: Some("Ready to ship".into()),
            ..Card::default()
        };
        adapter.deliver_card("p", None, &card, None).await.unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"card\",\"card\":{\"title\":\"Deploy\",\"body\":\"Ready to ship\"}}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_collapsible_frame_exact_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        adapter
            .deliver_collapsible(
                "p",
                None,
                "line1\nline2",
                "2 lines of output",
                &["line1".to_owned()],
            )
            .await
            .unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"collapsible\",\"text\":\"line1\\nline2\",\"summary\":\"2 lines of output\",\"preview_lines\":[\"line1\"]}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_error_frame_exact_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let err = ErrorCard {
            title: "Something went wrong".into(),
            summary: "provider failed".into(),
            kind: ErrorCardKind::Provider,
            details: None,
            retryable: true,
        };
        adapter.deliver_error("p", None, &err).await.unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"error\",\"error\":{\"title\":\"Something went wrong\",\"summary\":\"provider failed\",\"kind\":\"provider\",\"retryable\":true}}\n"
        );
    }

    #[tokio::test]
    async fn log_mode_thinking_frame_exact_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        let thinking = ThinkingBlock {
            text: "pondering the diff".into(),
            redacted: false,
            model: None,
        };
        adapter
            .deliver_thinking("p", None, &thinking)
            .await
            .unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        assert_eq!(
            body,
            "{\"kind\":\"thinking\",\"thinking\":{\"text\":\"pondering the diff\",\"redacted\":false}}\n"
        );
    }

    #[tokio::test]
    async fn stdio_mode_rich_hooks_keep_legacy_text_flattening() {
        // On the stdio path the hooks reproduce the trait-default
        // behaviour: flatten to `to_text_fallback()` and route through
        // `deliver`, which prefixes the label. No JSON frame appears.
        let (mut client, server) = tokio::io::duplex(4096);
        let reader = BufReader::new(Cursor::new(b""));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let adapter = CliAdapter::new_with_io(reader, server, tx, "agent> ");
        let bc = Breadcrumb {
            tool_name: "shell".into(),
            detail: Some("cargo check".into()),
            status: BreadcrumbStatus::Running,
            summary: None,
            steps: vec![],
        };
        adapter
            .deliver_breadcrumb("p", None, &bc, None)
            .await
            .unwrap();
        let mut out = vec![0u8; 512];
        let n = tokio::io::AsyncReadExt::read(&mut client, &mut out)
            .await
            .unwrap();
        let text = std::str::from_utf8(&out[..n]).unwrap();
        assert!(text.starts_with("agent> "), "label kept on stdio: {text:?}");
        assert!(text.contains("shell"), "fallback text present: {text:?}");
        assert!(!text.starts_with('{'), "stdio must not emit JSON frames");
    }

    #[tokio::test]
    async fn sniff_contract_frame_lines_parse_and_legacy_lines_do_not() {
        // The reader-side contract documented on `CliFrame`: a frame
        // line starts with '{' and deserializes; a legacy labelled line
        // does neither.
        let dir = tempfile::tempdir().unwrap();
        let (adapter, log) = log_adapter(&dir).await;
        adapter
            .deliver("p", None, &outbound_text("hello there"))
            .await
            .unwrap();
        let body = tokio::fs::read_to_string(&log).await.unwrap();
        let frame_line = body.lines().next().unwrap();
        assert!(frame_line.starts_with('{'));
        let frame: CliFrame = serde_json::from_str(frame_line).unwrap();
        assert_eq!(
            frame,
            CliFrame::Chat {
                text: "hello there".to_owned(),
                files: vec![],
            }
        );

        let legacy_line = "agent> hello there";
        assert!(!legacy_line.starts_with('{'));
        assert!(serde_json::from_str::<CliFrame>(legacy_line).is_err());
    }

    #[test]
    fn cli_frame_round_trips_every_kind() {
        // Serialize -> deserialize identity for each variant, proving
        // the reused core serde shapes survive the frame wrapper.
        let frames = vec![
            CliFrame::Chat {
                text: "hi".into(),
                files: vec!["a.txt".into()],
            },
            CliFrame::Card {
                card: Card {
                    title: Some("t".into()),
                    ..Card::default()
                },
            },
            CliFrame::Breadcrumb {
                breadcrumb: Breadcrumb {
                    tool_name: "shell".into(),
                    detail: None,
                    status: BreadcrumbStatus::Failed,
                    summary: Some("timeout".into()),
                    steps: vec![],
                },
            },
            CliFrame::Diff {
                diff: DiffCard {
                    path: "a.rs".into(),
                    language: Some("rust".into()),
                    hunks: vec![],
                    added: 0,
                    removed: 0,
                    truncated: true,
                },
            },
            CliFrame::Collapsible {
                text: "full".into(),
                summary: "sum".into(),
                preview_lines: vec![],
            },
            CliFrame::TodoList {
                todo_list: TodoList {
                    items: vec![TodoListItem {
                        id: 7,
                        text: "x".into(),
                        status: TodoItemStatus::Blocked,
                        blocked_reason: Some("stuck".into()),
                    }],
                    title: None,
                },
            },
            CliFrame::Error {
                error: ErrorCard {
                    title: "t".into(),
                    summary: "s".into(),
                    kind: ErrorCardKind::Internal,
                    details: Some("stderr".into()),
                    retryable: false,
                },
            },
            CliFrame::Thinking {
                thinking: ThinkingBlock {
                    text: String::new(),
                    redacted: true,
                    model: Some("claude-opus-4-7".into()),
                },
            },
        ];
        for frame in frames {
            let line = serde_json::to_string(&frame).unwrap();
            assert!(!line.contains('\n'), "frame must be single-line: {line}");
            let back: CliFrame = serde_json::from_str(&line).unwrap();
            assert_eq!(back, frame);
        }
    }
}
