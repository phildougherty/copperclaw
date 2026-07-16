//! Axum router for the Slack Events API webhook.
//!
//! The router serves two payload shapes on the same path so operators can
//! point Slack's "Request URL" (events) and "Interactivity Request URL"
//! (block-kit actions) at the same endpoint:
//!
//! - JSON `event_callback` envelopes (the canonical Events API),
//!   demultiplexed by [`SlackEventEnvelope`].
//! - Form-encoded `payload=<urlencoded-json>` (interactive components — what
//!   Block Kit `button` taps land as). The JSON inside is a `block_actions`
//!   payload; we parse it via [`parse_block_actions`] and synthesise an
//!   inbound chat event whose text is the tapped button's `value` so the
//!   agent sees the tap as if the user typed the value, mirroring the
//!   Telegram `callback_query` pattern.

use crate::api::SlackApi;
use crate::config::DEFAULT_MAX_ATTACHMENT_BYTES;
use crate::events::types::{MessageEvent, SlackEvent, SlackEventEnvelope, SlackFile};
use crate::signature::verify_signature;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::post,
};
use chrono::{TimeZone, Utc};
use copperclaw_channels_core::AdapterError;
use copperclaw_channels_core::inbound_file::{
    FALLBACK_FILENAME, STAGED_PATH_KEY, STAGING_SUBDIR, sanitize_filename, stage_inbound_file,
};
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, ReplyTo, SenderIdentity,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};

/// Largest image we inline as base64 into the inbound message, mirroring
/// the Telegram adapter's cap. The base64 rides in the transcript
/// (re-sent every turn until compaction) and a tiled vision model gains
/// nothing past a few megapixels — so cap it and leave larger images
/// path-only for the agent to read with tools.
const MAX_INLINE_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// Maximum number of recent `event_id`s to keep for duplicate suppression.
pub const DEDUP_CAPACITY: usize = 256;

/// In-memory LRU-ish ring of `event_id`s seen so we can suppress retries.
#[derive(Debug, Default)]
pub struct EventDedup {
    seen: Mutex<VecDeque<String>>,
}

impl EventDedup {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true on first sight; false if the id was already in the ring.
    pub async fn observe(&self, id: &str) -> bool {
        let mut guard = self.seen.lock().await;
        if guard.iter().any(|s| s == id) {
            return false;
        }
        if guard.len() == DEDUP_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(id.to_owned());
        true
    }
}

/// Shared state for the Slack events HTTP handler.
#[derive(Clone)]
pub struct SlackEventsState {
    pub signing_secret: Arc<String>,
    pub inbound_tx: Sender<InboundEvent>,
    pub dedup: Arc<EventDedup>,
    /// Bot user id (resolved from `auth.test`) used to detect mentions in
    /// text bodies. `None` disables mention detection from text (the
    /// `app_mention` event itself still sets `is_mention`).
    pub bot_user_id: Arc<Option<String>>,
    /// Channel-type label attached to emitted events. Lives in state rather
    /// than a constant so tests can override it.
    pub channel_type: ChannelType,
    /// Override for "now" in seconds — only used by tests for deterministic
    /// signature drift checks.
    pub now_secs_override: Option<i64>,
    /// Web API client used to download inbound `url_private` files with the
    /// bot token. `None` disables inbound-file handling (files on a message
    /// are ignored) — the default until the factory wires it via
    /// [`SlackEventsState::with_attachments`].
    pub api: Option<SlackApi>,
    /// Cap on inbound file size; larger files fall back to a `too_large`
    /// system row instead of being downloaded.
    pub max_attachment_bytes: u64,
    /// Per-channel data directory. Inbound downloads are STAGED under
    /// `data_dir/staging/<unique>/<filename>` and surfaced on the
    /// attachment as `staged_path`, per the channels-core inbound-file
    /// contract ([`copperclaw_channels_core::inbound_file`]); the router
    /// materializes them into the resolved session at route time. `None`
    /// disables inbound-file handling.
    pub data_dir: Option<PathBuf>,
}

impl SlackEventsState {
    #[must_use]
    pub fn new(
        signing_secret: impl Into<String>,
        inbound_tx: Sender<InboundEvent>,
        bot_user_id: Option<String>,
        channel_type: ChannelType,
    ) -> Self {
        Self {
            signing_secret: Arc::new(signing_secret.into()),
            inbound_tx,
            dedup: Arc::new(EventDedup::new()),
            bot_user_id: Arc::new(bot_user_id),
            channel_type,
            now_secs_override: None,
            api: None,
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            data_dir: None,
        }
    }

    /// Enable inbound-file handling: messages carrying `files[]` have their
    /// first file downloaded from `url_private` (bot-token auth), size-capped
    /// at `max_attachment_bytes`, and staged under `data_dir` for the router
    /// to materialize into the resolved session. Without this the adapter
    /// ignores inbound files (pre-C4a behaviour).
    #[must_use]
    pub fn with_attachments(
        mut self,
        api: SlackApi,
        max_attachment_bytes: u64,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        self.api = Some(api);
        self.max_attachment_bytes = max_attachment_bytes;
        self.data_dir = Some(data_dir.into());
        self
    }

    fn now_secs(&self) -> i64 {
        self.now_secs_override
            .unwrap_or_else(|| Utc::now().timestamp())
    }
}

/// Build the Slack events router. Mounts the handler at the given `path`.
pub fn build_events_router(path: &str, state: SlackEventsState) -> Router {
    Router::new()
        .route(path, post(handle_events))
        .with_state(state)
}

async fn handle_events(
    State(state): State<SlackEventsState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ts = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok());
    let sig = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok());
    if verify_signature(&state.signing_secret, ts, sig, &body, state.now_secs()).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Interactivity payloads (block_actions) arrive form-encoded as
    // `payload=<urlencoded-json>`. Detect by the Content-Type header (Slack
    // always sets `application/x-www-form-urlencoded` for these) so we don't
    // misroute JSON envelopes that happen to start with `payload=`.
    if is_form_urlencoded(&headers) {
        return handle_interactive(&state, &body).await;
    }

    let envelope: SlackEventEnvelope = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match envelope {
        SlackEventEnvelope::UrlVerification { challenge, .. } => {
            (StatusCode::OK, Json(json!({"challenge": challenge}))).into_response()
        }
        SlackEventEnvelope::EventCallback(cb) => {
            if !state.dedup.observe(&cb.event_id).await {
                // Already-seen — respond 200 OK so Slack stops retrying.
                return StatusCode::OK.into_response();
            }
            let event = match cb.event {
                SlackEvent::Message(m) => convert_message(&state, &m, false).await,
                SlackEvent::AppMention(m) => convert_message(&state, &m, true).await,
                SlackEvent::Other => return StatusCode::OK.into_response(),
            };
            if let Err(err) = state.inbound_tx.send(event).await {
                tracing::warn!(error=%err, "slack inbound channel closed");
            }
            StatusCode::OK.into_response()
        }
    }
}

/// Handle an interactive payload (`block_actions` / `interactive_message`).
///
/// Slack expects an empty `200 OK` within 3 s — the tap spinner clears as
/// soon as we respond, so we ACK immediately and ship the synthesised
/// inbound event to the host channel without round-tripping any further
/// Slack API calls.
async fn handle_interactive(state: &SlackEventsState, body: &[u8]) -> Response {
    // Body is `payload=<urlencoded-json>`. Strip the prefix, then percent-
    // decode (form encoding == percent encoding with `+` standing in for a
    // space; we use a tiny inline decoder so we don't pull a new crate just
    // for this).
    let Ok(body_str) = std::str::from_utf8(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(("payload", encoded)) = body_str.split_once('=') else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(json_text) = form_decode(encoded) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(payload): Result<Value, _> = serde_json::from_str(&json_text) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    if let Some(evt) = parse_block_actions(state, &payload) {
        // Best-effort send; the ACK still goes out either way so the user's
        // client clears its spinner. Slack will retry the interactive
        // payload if we send a non-2xx, so failing the channel send by
        // returning an error here would create user-visible duplicates.
        if let Err(err) = state.inbound_tx.send(evt).await {
            tracing::warn!(error=%err, "slack inbound channel closed (interactive)");
        }
    }
    // Empty 200 OK — Slack's "do nothing else" ACK.
    StatusCode::OK.into_response()
}

/// Map a `block_actions` JSON payload to a chat-shaped `InboundEvent`.
///
/// Returns `None` when the payload isn't a `block_actions` shape, no
/// `actions[]` entry was usable (no `value`), or required routing fields
/// are missing — in which case the caller still ACKs to clear the spinner.
///
/// The synthesised event mimics a regular chat message so the agent's
/// existing branching code Just Works:
///
/// - `kind = MessageKind::Chat`,
/// - `content.text = action.value` (the tapped button's canonical value),
/// - `content.callback` carries the platform metadata an agent might want
///   (`action_id`, `block_id`, `trigger_id`, container) without inflating
///   the primary `text` field.
pub fn parse_block_actions(state: &SlackEventsState, payload: &Value) -> Option<InboundEvent> {
    let kind = payload.get("type").and_then(Value::as_str)?;
    if kind != "block_actions" {
        return None;
    }

    let action = payload
        .get("actions")
        .and_then(Value::as_array)
        .and_then(|a| a.first())?;
    let value = action.get("value").and_then(Value::as_str)?;
    let action_id = action.get("action_id").and_then(Value::as_str);
    let block_id = action.get("block_id").and_then(Value::as_str);

    // Slack puts the channel + message + ts inside `container` for block
    // taps in a regular channel. For DMs the same fields land under
    // `channel`. Try `container` first, then fall back to `channel`.
    let container = payload.get("container");
    let channel_id = container
        .and_then(|c| c.get("channel_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .get("channel")
                .and_then(|c| c.get("id"))
                .and_then(Value::as_str)
        })?;

    let message_ts = container
        .and_then(|c| c.get("message_ts"))
        .and_then(Value::as_str);
    // Slack threading: when the original card lives inside a thread, the
    // interactive payload carries `container.thread_ts`. Otherwise the tap
    // is at the channel root and we leave thread_id None.
    let thread_id = container
        .and_then(|c| c.get("thread_ts"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let user = payload.get("user");
    let user_id = user.and_then(|u| u.get("id")).and_then(Value::as_str);
    let display_name = user
        .and_then(|u| u.get("username"))
        .and_then(Value::as_str)
        .or_else(|| user.and_then(|u| u.get("name")).and_then(Value::as_str))
        .map(str::to_owned);

    let trigger_id = payload.get("trigger_id").and_then(Value::as_str);
    let response_url = payload.get("response_url").and_then(Value::as_str);

    let sender = user_id.map(|uid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: uid.to_owned(),
        display_name,
    });

    // The interactive payload doesn't carry the message ts in the way a
    // regular `message` event does, so use the action_id + trigger_id (or
    // the original message_ts when present) as a unique-enough id for
    // dedup on the host side. This matches Telegram's strategy of using
    // the callback_query id as the inbound message id.
    let message_id = if let Some(a) = action_id {
        if let Some(ts) = message_ts {
            format!("{a}:{ts}")
        } else if let Some(t) = trigger_id {
            format!("{a}:{t}")
        } else {
            a.to_owned()
        }
    } else {
        // Falling back to the value keeps the event stable across
        // retries when no other id is present.
        value.to_owned()
    };

    let mut callback = json!({
        "value": value,
    });
    if let Some(a) = action_id {
        callback["action_id"] = Value::String(a.to_owned());
    }
    if let Some(b) = block_id {
        callback["block_id"] = Value::String(b.to_owned());
    }
    if let Some(ts) = message_ts {
        callback["message_ts"] = Value::String(ts.to_owned());
    }
    if let Some(t) = trigger_id {
        callback["trigger_id"] = Value::String(t.to_owned());
    }
    if let Some(r) = response_url {
        callback["response_url"] = Value::String(r.to_owned());
    }

    let content = json!({
        "text": value,
        "callback": callback,
    });

    let is_group = Some(channel_id.starts_with('C') || channel_id.starts_with('G'));

    Some(InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: channel_id.to_owned(),
        thread_id,
        message: InboundMessage {
            id: message_id,
            kind: MessageKind::Chat,
            content,
            timestamp: Utc::now(),
            is_mention: None,
            is_group,
        },
        reply_to: None,
        sender,
    })
}

fn is_form_urlencoded(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| {
            // Header may include `; charset=...` — match the prefix.
            s.trim()
                .to_ascii_lowercase()
                .starts_with("application/x-www-form-urlencoded")
        })
}

/// Inline form-decoder. Replaces `+` with space and percent-decodes the rest.
/// Returns an error only when a percent escape is malformed.
fn form_decode(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(());
                }
                let hi = hex_digit(bytes[i + 1])?;
                let lo = hex_digit(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn hex_digit(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

async fn convert_message(
    state: &SlackEventsState,
    m: &MessageEvent,
    is_app_mention: bool,
) -> InboundEvent {
    let mut content = json!({"text": m.text.clone().unwrap_or_default()});
    if let Some(blocks) = m.blocks.clone() {
        content["blocks"] = blocks;
    }

    // Inbound-file handling (M18 C4a). When file downloads are enabled
    // (the factory wired an API client + staging dir) and the message
    // carries an upload, download its bytes with the bot token, enforce
    // the size cap, and stage per the channels-core inbound-file
    // contract. Success surfaces `content.attachment` (with `staged_path`,
    // never `path` — the router owns that key); the size/transport
    // failure paths downgrade the event to a `MessageKind::System` row
    // with the same `too_large` / `download_failed` taxonomy the Telegram
    // adapter emits, so a failed download is never a silent drop.
    //
    // Slack messages can carry multiple files; we handle the first (the
    // common "here's the CSV/spec" case) — mirroring the single-attachment
    // model the Telegram ingress uses.
    let mut kind = MessageKind::Chat;
    if let (Some(api), Some(data_dir)) = (state.api.as_ref(), state.data_dir.as_ref()) {
        if let Some(file) = m.files.as_ref().and_then(|files| files.first()) {
            let channel = state.channel_type.as_str();
            match download_file(api, data_dir, state.max_attachment_bytes, file).await {
                FileOutcome::Ok { attachment, bytes } => {
                    copperclaw_metrics::inc_inbound_file(channel, "ok");
                    copperclaw_metrics::observe_inbound_file_bytes(channel, bytes);
                    content["attachment"] = Value::Object(attachment);
                }
                FileOutcome::TooLarge { reported } => {
                    copperclaw_metrics::inc_inbound_file(channel, "too_large");
                    kind = MessageKind::System;
                    content = too_large_content(file, state.max_attachment_bytes, reported);
                }
                FileOutcome::Failed { error } => {
                    copperclaw_metrics::inc_inbound_file(channel, "download_failed");
                    kind = MessageKind::System;
                    content = download_failed_content(file, &error);
                }
            }
        }
    }

    let is_mention = if is_app_mention {
        Some(true)
    } else {
        state
            .bot_user_id
            .as_ref()
            .as_ref()
            .map(|bot| m.mentions_user(bot))
    };
    let is_group = Some(m.is_group_channel());
    let sender = m.user.as_ref().map(|uid| SenderIdentity {
        channel_type: state.channel_type.clone(),
        identity: uid.clone(),
        display_name: None,
    });
    let timestamp = parse_slack_ts(&m.ts);
    // Slack uses `thread_ts` for two distinct things:
    //   * On the thread root, `thread_ts == ts`.
    //   * On a reply within a thread, `thread_ts` points at the root
    //     (which IS the message being replied to from Slack's POV).
    // Only the latter is a real reply, so we surface `reply_to` only when
    // the two timestamps differ.
    let reply_to = m
        .thread_ts
        .as_deref()
        .filter(|parent| *parent != m.ts.as_str())
        .map(|parent| ReplyTo {
            channel_type: state.channel_type.clone(),
            platform_id: m.channel.clone(),
            thread_id: Some(parent.to_owned()),
            // Thread-root author not resolved here — must not count as a
            // mention. Native @-mentions are detected separately.
            replying_to_self: None,
        });
    InboundEvent {
        channel_type: state.channel_type.clone(),
        platform_id: m.channel.clone(),
        thread_id: m.thread_ts.clone(),
        message: InboundMessage {
            id: m.ts.clone(),
            kind,
            content,
            timestamp,
            is_mention,
            is_group,
        },
        reply_to,
        sender,
    }
}

/// Outcome of downloading one inbound Slack file.
enum FileOutcome {
    /// Downloaded + staged; the map is the `content.attachment` object
    /// (carries `staged_path`, never `path`).
    Ok {
        attachment: serde_json::Map<String, Value>,
        /// Downloaded byte count (for `copperclaw_inbound_file_bytes`).
        bytes: u64,
    },
    /// Reported or actual size exceeded `max_attachment_bytes`. `reported`
    /// is the byte count we compared against the cap, when known.
    TooLarge { reported: Option<u64> },
    /// The download (or staging write) failed; the error is surfaced
    /// verbatim in the `download_failed` system row.
    Failed { error: AdapterError },
}

/// Download a single Slack file from its `url_private` link and stage it
/// per the channels-core inbound-file contract. Enforces the size cap on
/// both the Slack-reported size (pre-download) and the actual byte count
/// (post-download), mirroring the Telegram ingress checks.
async fn download_file(
    api: &SlackApi,
    data_dir: &Path,
    max_attachment_bytes: u64,
    file: &SlackFile,
) -> FileOutcome {
    if let Some(size) = file.size {
        if size > max_attachment_bytes {
            return FileOutcome::TooLarge {
                reported: Some(size),
            };
        }
    }
    let Some(url) = file.download_url() else {
        return FileOutcome::Failed {
            error: AdapterError::Transport("slack file has no url_private".to_owned()),
        };
    };
    let bytes = match api.download_file(url).await {
        Ok(b) => b,
        Err(error) => return FileOutcome::Failed { error },
    };
    if bytes.len() as u64 > max_attachment_bytes {
        return FileOutcome::TooLarge {
            reported: Some(bytes.len() as u64),
        };
    }
    let filename = sanitize_filename(file.name.as_deref(), FALLBACK_FILENAME);
    // Stage per the channels-core inbound-file contract: the router owns
    // the final `<session>/inbox/<msg_id>/<filename>` placement (and the
    // cleanup of the staged copy) because only it knows the resolved
    // session at route time.
    match stage_inbound_file(&data_dir.join(STAGING_SUBDIR), &filename, &bytes).await {
        Ok(path) => {
            let mut attachment = attachment_json(file, &filename, &path, bytes.len() as u64);
            inline_image_base64(&mut attachment, file, &bytes);
            FileOutcome::Ok {
                attachment,
                bytes: bytes.len() as u64,
            }
        }
        Err(error) => FileOutcome::Failed { error },
    }
}

/// Build the `content.attachment` object for a staged Slack download. Per
/// the inbound-file contract it carries `staged_path` (the host-side
/// staging location) and NO `path` key — the router sets `path` to the
/// container-visible `/data/inbox/...` location at materialization time.
fn attachment_json(
    file: &SlackFile,
    filename: &str,
    staged_path: &Path,
    actual_size: u64,
) -> serde_json::Map<String, Value> {
    let mut obj = serde_json::Map::new();
    obj.insert("kind".to_owned(), Value::String("slack.file".to_owned()));
    if let Some(id) = file.id.as_deref() {
        obj.insert("file_id".to_owned(), Value::String(id.to_owned()));
    }
    obj.insert("filename".to_owned(), Value::String(filename.to_owned()));
    obj.insert(
        STAGED_PATH_KEY.to_owned(),
        Value::String(staged_path.to_string_lossy().into_owned()),
    );
    obj.insert(
        "mime_type".to_owned(),
        file.mimetype.clone().map_or(Value::Null, Value::String),
    );
    obj.insert("size".to_owned(), Value::from(actual_size));
    obj
}

/// If `file` is an image within the size cap, add a `data_base64` field to
/// the attachment so the runner can lift it into a vision content block —
/// vision parity with the Telegram adapter's `inline_image_base64`. Larger
/// or non-image files keep the path-only form.
fn inline_image_base64(
    attachment: &mut serde_json::Map<String, Value>,
    file: &SlackFile,
    bytes: &[u8],
) {
    let is_image = file
        .mimetype
        .as_deref()
        .is_some_and(|m| m.starts_with("image/"));
    if !is_image || bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
        return;
    }
    attachment.insert(
        "data_base64".to_owned(),
        Value::String(copperclaw_types::encode_base64(bytes)),
    );
}

/// Metadata-only content for the `too_large` system-row fallback (nothing
/// was staged). Mirrors the Telegram adapter's shape.
fn too_large_content(file: &SlackFile, limit: u64, reported: Option<u64>) -> Value {
    let mut obj = file_metadata(file);
    obj.insert("reason".to_owned(), Value::String("too_large".to_owned()));
    obj.insert("limit".to_owned(), Value::from(limit));
    if let Some(r) = reported {
        obj.insert("reported_size".to_owned(), Value::from(r));
    }
    Value::Object(obj)
}

/// Metadata-only content for the `download_failed` system-row fallback.
fn download_failed_content(file: &SlackFile, error: &AdapterError) -> Value {
    let mut obj = file_metadata(file);
    obj.insert(
        "reason".to_owned(),
        Value::String("download_failed".to_owned()),
    );
    obj.insert("error".to_owned(), Value::String(format!("{error}")));
    Value::Object(obj)
}

/// Shared metadata block for the system-row fallbacks: the Slack file id,
/// name, mime type, and reported size, echoed so the host can log / alert
/// on what was dropped.
fn file_metadata(file: &SlackFile) -> serde_json::Map<String, Value> {
    let mut obj = serde_json::Map::new();
    obj.insert("kind".to_owned(), Value::String("slack.file".to_owned()));
    obj.insert(
        "file_id".to_owned(),
        file.id.clone().map_or(Value::Null, Value::String),
    );
    obj.insert(
        "file_name".to_owned(),
        file.name.clone().map_or(Value::Null, Value::String),
    );
    obj.insert(
        "mime_type".to_owned(),
        file.mimetype.clone().map_or(Value::Null, Value::String),
    );
    obj.insert(
        "file_size".to_owned(),
        file.size.map_or(Value::Null, Value::from),
    );
    obj
}

/// Convert Slack's `ts` (`<seconds>.<microseconds>`) into a `DateTime<Utc>`.
/// Falls back to the current time if parsing fails.
pub(crate) fn parse_slack_ts(ts: &str) -> chrono::DateTime<Utc> {
    let trimmed = ts.split('.').next().unwrap_or(ts);
    if let Ok(secs) = trimmed.parse::<i64>() {
        if let Some(dt) = Utc.timestamp_opt(secs, 0).single() {
            return dt;
        }
    }
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::compute_signature;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tokio::sync::mpsc;
    use tower::ServiceExt;
    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SECRET: &str = "test-secret";
    const TS: &str = "1700000000";

    fn make_state(bot: Option<String>) -> (SlackEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let mut s = SlackEventsState::new(SECRET, tx, bot, ChannelType::new("slack"));
        s.now_secs_override = Some(TS.parse().unwrap());
        (s, rx)
    }

    fn signed_request(state: &SlackEventsState, path: &str, body: &[u8]) -> Request<Body> {
        let sig = compute_signature(&state.signing_secret, TS, body);
        Request::builder()
            .method("POST")
            .uri(path)
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body.to_vec()))
            .unwrap()
    }

    #[tokio::test]
    async fn url_verification_returns_challenge() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"url_verification","challenge":"abc"
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["challenge"], "abc");
    }

    #[tokio::test]
    async fn rejects_bad_signature() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"url_verification","challenge":"abc"
        }))
        .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", format!("v0={}", "00".repeat(32)))
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_stale_timestamp() {
        let (mut state, _rx) = make_state(None);
        state.now_secs_override = Some(10_000_000_000);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"url_verification","challenge":"abc"
        }))
        .unwrap();
        // Sign with the (stale) TS so the only thing wrong is drift.
        let sig = compute_signature(&state.signing_secret, TS, &body);
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn malformed_json_returns_400() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = b"not json".to_vec();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn message_event_emits_inbound() {
        let (state, mut rx) = make_state(Some("UBOT".into()));
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev1",
            "event":{
                "type":"message",
                "ts":"1700000001.000001",
                "channel":"C1",
                "user":"U1",
                "text":"hi <@UBOT>",
                "channel_type":"channel",
                "blocks":[{"type":"rt"}]
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.channel_type.as_str(), "slack");
        assert_eq!(evt.platform_id, "C1");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.content["text"], "hi <@UBOT>");
        assert!(evt.message.content["blocks"].is_array());
        assert_eq!(evt.message.id, "1700000001.000001");
        assert_eq!(evt.message.is_mention, Some(true));
        assert_eq!(evt.message.is_group, Some(true));
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "U1");
        assert_eq!(sender.channel_type.as_str(), "slack");
    }

    #[tokio::test]
    async fn app_mention_sets_is_mention_true() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev2",
            "event":{
                "type":"app_mention",
                "ts":"1700000002.0",
                "channel":"C9",
                "user":"U9",
                "text":"hey",
                "thread_ts":"1700000001.0"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
        assert_eq!(evt.thread_id.as_deref(), Some("1700000001.0"));
    }

    #[tokio::test]
    async fn dm_channel_is_not_group() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev3",
            "event":{
                "type":"message",
                "ts":"1700000003.0",
                "channel":"D1",
                "user":"U1",
                "text":"dm"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[tokio::test]
    async fn duplicate_event_id_is_suppressed() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type":"event_callback",
            "event_id":"DUPE",
            "event":{
                "type":"message",
                "ts":"1700000010.0",
                "channel":"C1",
                "user":"U1",
                "text":"once"
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Resend.
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Only one event delivered.
        let _first = rx.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_event_type_is_acked_without_emit() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev4",
            "event":{"type":"reaction_added"}
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn no_bot_user_id_yields_none_is_mention_for_plain_messages() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev5",
            "event":{
                "type":"message",
                "ts":"1700000005.0",
                "channel":"C1",
                "user":"U1",
                "text":"hi <@UBOT>"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.message.is_mention.is_none());
    }

    #[tokio::test]
    async fn bot_user_id_without_mention_marks_false() {
        let (state, mut rx) = make_state(Some("UBOT".into()));
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev6",
            "event":{
                "type":"message",
                "ts":"1700000006.0",
                "channel":"C1",
                "user":"U1",
                "text":"just chatting"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.is_mention, Some(false));
    }

    #[tokio::test]
    async fn missing_user_yields_no_sender() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev7",
            "event":{
                "type":"message",
                "ts":"1700000007.0",
                "channel":"C1",
                "text":"ghost"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(evt.sender.is_none());
    }

    #[tokio::test]
    async fn event_dedup_capacity_drops_oldest() {
        let dedup = EventDedup::new();
        for i in 0..DEDUP_CAPACITY {
            assert!(dedup.observe(&format!("e{i}")).await);
        }
        // Re-observe one of the existing ids → false.
        assert!(!dedup.observe("e0").await);
        // Add one more → drops "e0" from the ring (but it stays denied because
        // we just re-checked it ABOVE; instead let's overflow then observe a new one
        // followed by the original).
        assert!(dedup.observe("e256").await);
        // The earliest id ("e1" now) should no longer be in ring once one is dropped
        // after a fresh insert. But since we already inserted DEDUP_CAPACITY and then
        // one more above ("e256"), the oldest dropped was "e1" if e0 was bumped to
        // back via re-observe? No — re-observe does NOT bump in our impl.
        // So inserting "e256" dropped "e0" from a full ring. Re-observing "e0" should
        // succeed again now.
        assert!(dedup.observe("e0").await);
    }

    #[tokio::test]
    async fn thread_reply_populates_reply_to() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        // thread_ts != ts → this is a reply inside an existing thread.
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev-Reply",
            "event":{
                "type":"message",
                "ts":"1700000020.000010",
                "channel":"C-CHAN",
                "user":"U-REPLIER",
                "text":"+1",
                "thread_ts":"1700000010.000001"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        let rt = evt
            .reply_to
            .expect("reply_to populated for in-thread reply");
        assert_eq!(rt.channel_type.as_str(), "slack");
        assert_eq!(rt.platform_id, "C-CHAN");
        assert_eq!(rt.thread_id.as_deref(), Some("1700000010.000001"));
    }

    #[tokio::test]
    async fn thread_root_does_not_populate_reply_to() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        // thread_ts == ts → the message IS the thread root, not a reply.
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"Ev-Root",
            "event":{
                "type":"message",
                "ts":"1700000030.000001",
                "channel":"C-CHAN",
                "user":"U-AUTHOR",
                "text":"starting a thread",
                "thread_ts":"1700000030.000001"
            }
        }))
        .unwrap();
        let req = signed_request(&state, "/slack/events", &body);
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(
            evt.reply_to.is_none(),
            "thread root carries thread_ts==ts; that should NOT be a reply_to"
        );
    }

    #[test]
    fn parse_slack_ts_returns_unix_timestamp() {
        let dt = parse_slack_ts("1700000000.000001");
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }

    #[test]
    fn parse_slack_ts_bad_input_falls_back_to_now() {
        // Just ensure it doesn't panic — value compared with a recent range.
        let before = Utc::now().timestamp() - 5;
        let dt = parse_slack_ts("not-a-ts");
        assert!(dt.timestamp() >= before);
    }

    /// Build a Slack `block_actions` payload, percent-encode it, and POST
    /// it as `payload=<encoded>` so the interactive branch exercises the
    /// same shape Slack actually sends.
    fn signed_interactive_request(
        state: &SlackEventsState,
        path: &str,
        payload_json: &str,
    ) -> Request<Body> {
        let encoded = percent_encode_form(payload_json);
        let body = format!("payload={encoded}");
        let body_bytes = body.into_bytes();
        let sig = compute_signature(&state.signing_secret, TS, &body_bytes);
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body_bytes))
            .unwrap()
    }

    /// Minimal `application/x-www-form-urlencoded` value encoder — only the
    /// reserved set so test strings round-trip predictably.
    fn percent_encode_form(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for &b in s.as_bytes() {
            let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
            if unreserved {
                out.push(b as char);
            } else if b == b' ' {
                out.push('+');
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    #[tokio::test]
    async fn interactive_block_actions_emits_inbound_with_value_text() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U42", "username": "alice"},
            "trigger_id": "trig-1",
            "response_url": "https://hooks.slack.test/r",
            "container": {
                "type": "message",
                "channel_id": "C100",
                "message_ts": "1700000050.000100"
            },
            "actions": [{
                "type": "button",
                "action_id": "card_btn_0",
                "block_id": "card_actions",
                "value": "deploy:yes"
            }]
        });
        let req = signed_interactive_request(&state, "/slack/events", &payload.to_string());
        let resp = app.oneshot(req).await.unwrap();
        // Slack requires a 2xx ACK so the spinner clears.
        assert_eq!(resp.status(), StatusCode::OK);

        let evt = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("event delivered within timeout")
            .expect("inbound channel open");
        assert_eq!(evt.channel_type.as_str(), "slack");
        assert_eq!(evt.platform_id, "C100");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.kind, MessageKind::Chat);
        // Text mimics what the user "typed" — the button value.
        assert_eq!(evt.message.content["text"], "deploy:yes");
        // Callback metadata is preserved for agents that want to branch on it.
        assert_eq!(evt.message.content["callback"]["action_id"], "card_btn_0");
        assert_eq!(evt.message.content["callback"]["value"], "deploy:yes");
        assert_eq!(
            evt.message.content["callback"]["message_ts"],
            "1700000050.000100"
        );
        let sender = evt.sender.expect("sender");
        assert_eq!(sender.identity, "U42");
        assert_eq!(sender.display_name.as_deref(), Some("alice"));
        // is_group derived from the `C` channel prefix.
        assert_eq!(evt.message.is_group, Some(true));
    }

    #[tokio::test]
    async fn interactive_dm_routes_to_channel_id_under_channel_field() {
        // Slack puts the channel under `channel.id` instead of
        // `container.channel_id` for some legacy / DM interactive shapes.
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U7", "name": "bob"},
            "channel": {"id": "D1", "name": "directmessage"},
            "actions": [{
                "type": "button",
                "action_id": "card_btn_1",
                "value": "approve"
            }]
        });
        let req = signed_interactive_request(&state, "/slack/events", &payload.to_string());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.platform_id, "D1");
        assert_eq!(evt.message.is_group, Some(false));
        assert_eq!(evt.message.content["text"], "approve");
    }

    #[tokio::test]
    async fn interactive_preserves_thread_ts_when_card_was_in_thread() {
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U7"},
            "container": {
                "channel_id": "C1",
                "message_ts": "1700000060.000010",
                "thread_ts": "1700000050.000001"
            },
            "actions": [{
                "type": "button",
                "action_id": "card_btn_0",
                "value": "ack"
            }]
        });
        let req = signed_interactive_request(&state, "/slack/events", &payload.to_string());
        let _ = app.oneshot(req).await.unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.thread_id.as_deref(), Some("1700000050.000001"));
    }

    #[tokio::test]
    async fn interactive_url_button_taps_have_no_value_so_no_event() {
        // Slack doesn't fire block_actions for `url` buttons — but we still
        // defend the parser against payloads with a missing `value`.
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let payload = json!({
            "type": "block_actions",
            "user": {"id": "U7"},
            "container": {"channel_id": "C1", "message_ts": "1.0"},
            "actions": [{
                "type": "button",
                "action_id": "card_btn_0",
                "url": "https://example.com"
            }]
        });
        let req = signed_interactive_request(&state, "/slack/events", &payload.to_string());
        let resp = app.oneshot(req).await.unwrap();
        // ACK still 200.
        assert_eq!(resp.status(), StatusCode::OK);
        // But no event.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn interactive_unsigned_request_returns_401() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = b"payload=%7B%7D".to_vec();
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", format!("v0={}", "00".repeat(32)))
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn interactive_malformed_form_body_returns_400_no_panic() {
        let (state, _rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        // No `payload=` prefix.
        let body = b"foo=bar".to_vec();
        let sig = compute_signature(&state.signing_secret, TS, &body);
        let req = Request::builder()
            .method("POST")
            .uri("/slack/events")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-slack-request-timestamp", TS)
            .header("x-slack-signature", sig)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn form_decode_handles_plus_and_percent_escapes() {
        assert_eq!(form_decode("hello+world").unwrap(), "hello world");
        assert_eq!(form_decode("a%20b").unwrap(), "a b");
        assert_eq!(form_decode("%7B%22a%22%3A1%7D").unwrap(), "{\"a\":1}");
        // Truncated escape — should error rather than panic.
        assert!(form_decode("%2").is_err());
        // Non-hex digit.
        assert!(form_decode("%ZZ").is_err());
    }

    #[test]
    fn parse_block_actions_returns_none_for_other_payload_types() {
        let (state, _rx) = make_state(None);
        let payload = json!({"type": "view_submission"});
        assert!(parse_block_actions(&state, &payload).is_none());
    }

    #[test]
    fn is_form_urlencoded_handles_charset_suffix() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded; charset=utf-8"
                .parse()
                .unwrap(),
        );
        assert!(is_form_urlencoded(&h));

        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        assert!(!is_form_urlencoded(&h));
    }

    #[test]
    fn now_secs_falls_back_to_real_clock_without_override() {
        let (tx, _rx) = mpsc::channel(1);
        let s = SlackEventsState::new("s", tx, None, ChannelType::new("slack"));
        let now = s.now_secs();
        let real = Utc::now().timestamp();
        assert!((now - real).abs() <= 5);
    }

    // ---- M18 C4a: inbound file download + staging ----

    /// State with inbound-file downloads wired to a mock Slack server +
    /// staging dir, plus the deterministic `now_secs` override so signed
    /// requests validate.
    fn make_state_with_attachments(
        server_uri: &str,
        data_dir: &std::path::Path,
        max_bytes: u64,
    ) -> (SlackEventsState, mpsc::Receiver<InboundEvent>) {
        let (tx, rx) = mpsc::channel::<InboundEvent>(16);
        let mut s = SlackEventsState::new(SECRET, tx, None, ChannelType::new("slack"))
            .with_attachments(
                SlackApi::new(server_uri, "xoxb-test"),
                max_bytes,
                data_dir.to_path_buf(),
            );
        s.now_secs_override = Some(TS.parse().unwrap());
        (s, rx)
    }

    /// A signed `event_callback` body for a `message` event carrying a
    /// single file with the given `url_private`, name, mimetype, and
    /// (optional) reported size.
    fn message_with_file_body(
        url_private: &str,
        name: &str,
        mimetype: &str,
        size: Option<u64>,
    ) -> Vec<u8> {
        let mut file = json!({
            "id": "F1",
            "name": name,
            "mimetype": mimetype,
            "url_private": url_private,
        });
        if let Some(s) = size {
            file["size"] = json!(s);
        }
        serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"EvFile",
            "event":{
                "type":"message",
                "ts":"1700000100.000001",
                "channel":"C1",
                "user":"U1",
                "text":"here is the spec",
                "files":[file]
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn inbound_file_download_stages_and_sets_staged_path_not_path() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/files/spec.csv"))
            // Slack requires the bot token as a bearer header on url_private.
            .and(header("authorization", "Bearer xoxb-test"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"id,qty\n1,2\n".to_vec()))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) =
            make_state_with_attachments(&server.uri(), dir.path(), DEFAULT_MAX_ATTACHMENT_BYTES);
        let app = build_events_router("/slack/events", state.clone());
        let url = format!("{}/files/spec.csv", server.uri());
        let body = message_with_file_body(&url, "spec.csv", "text/csv", Some(11));
        let resp = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        // Text is preserved alongside the staged attachment.
        assert_eq!(evt.message.content["text"], "here is the spec");
        let att = &evt.message.content["attachment"];
        assert_eq!(att["kind"], "slack.file");
        assert_eq!(att["file_id"], "F1");
        assert_eq!(att["filename"], "spec.csv");
        assert_eq!(att["mime_type"], "text/csv");
        assert_eq!(att["size"], 11);
        // Contract: the adapter STAGES the download and never sets `path`
        // (the router owns that key at materialization time).
        assert!(
            att.get("path").is_none(),
            "adapters must not set attachment.path: {att}"
        );
        let staged = att["staged_path"].as_str().unwrap();
        let staged_path = std::path::Path::new(staged);
        assert!(
            staged_path.starts_with(dir.path().join("staging")),
            "staged under <data_dir>/staging: {staged}"
        );
        assert_eq!(std::fs::read(staged_path).unwrap(), b"id,qty\n1,2\n");
    }

    #[tokio::test]
    async fn inbound_file_oversized_by_reported_size_falls_back_to_system() {
        // Reported size exceeds the cap → never download.
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) = make_state_with_attachments(&server.uri(), dir.path(), 4);
        let app = build_events_router("/slack/events", state.clone());
        let url = format!("{}/files/big.bin", server.uri());
        let body = message_with_file_body(&url, "big.bin", "application/octet-stream", Some(1024));
        let _ = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["reason"], "too_large");
        assert_eq!(evt.message.content["limit"], 4);
        assert_eq!(evt.message.content["reported_size"], 1024);
        assert_eq!(evt.message.content["file_name"], "big.bin");
        // No download call was made.
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "must not download an over-cap file"
        );
    }

    #[tokio::test]
    async fn inbound_file_oversized_after_body_read_falls_back_to_system() {
        // No reported size; the actual bytes exceed the cap.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/files/big.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8; 32]))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) = make_state_with_attachments(&server.uri(), dir.path(), 16);
        let app = build_events_router("/slack/events", state.clone());
        let url = format!("{}/files/big.bin", server.uri());
        let body = message_with_file_body(&url, "big.bin", "application/octet-stream", None);
        let _ = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["reason"], "too_large");
        assert_eq!(evt.message.content["reported_size"], 32);
    }

    #[tokio::test]
    async fn inbound_file_download_failure_falls_back_to_system_with_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/files/spec.csv"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) =
            make_state_with_attachments(&server.uri(), dir.path(), DEFAULT_MAX_ATTACHMENT_BYTES);
        let app = build_events_router("/slack/events", state.clone());
        let url = format!("{}/files/spec.csv", server.uri());
        let body = message_with_file_body(&url, "spec.csv", "text/csv", Some(11));
        let _ = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["reason"], "download_failed");
        let err = evt.message.content["error"].as_str().unwrap();
        assert!(err.contains("503"), "got `{err}`");
    }

    #[tokio::test]
    async fn inbound_file_missing_url_private_is_download_failed_not_silent_drop() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) =
            make_state_with_attachments(&server.uri(), dir.path(), DEFAULT_MAX_ATTACHMENT_BYTES);
        let app = build_events_router("/slack/events", state.clone());
        // File object with no url_private / url_private_download.
        let body = serde_json::to_vec(&json!({
            "type":"event_callback",
            "event_id":"EvNoUrl",
            "event":{
                "type":"message",
                "ts":"1700000101.000001",
                "channel":"C1",
                "user":"U1",
                "text":"oops",
                "files":[{"id":"F9","name":"ghost.txt","mimetype":"text/plain"}]
            }
        }))
        .unwrap();
        let _ = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["reason"], "download_failed");
    }

    #[tokio::test]
    async fn inbound_small_image_inlines_data_base64() {
        let server = MockServer::start().await;
        let png = b"\x89PNG\r\n\x1a\nfake-image-bytes".to_vec();
        Mock::given(method("GET"))
            .and(wm_path("/files/pic.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) =
            make_state_with_attachments(&server.uri(), dir.path(), DEFAULT_MAX_ATTACHMENT_BYTES);
        let app = build_events_router("/slack/events", state.clone());
        let url = format!("{}/files/pic.png", server.uri());
        let body = message_with_file_body(&url, "pic.png", "image/png", Some(png.len() as u64));
        let _ = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        let evt = rx.recv().await.unwrap();
        let att = &evt.message.content["attachment"];
        assert_eq!(att["kind"], "slack.file");
        // Vision parity: the image bytes ride inline as base64.
        assert_eq!(
            att["data_base64"].as_str().unwrap(),
            copperclaw_types::encode_base64(&png)
        );
    }

    #[tokio::test]
    async fn non_image_file_does_not_inline_data_base64() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/files/spec.csv"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"a,b\n".to_vec()))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (state, mut rx) =
            make_state_with_attachments(&server.uri(), dir.path(), DEFAULT_MAX_ATTACHMENT_BYTES);
        let app = build_events_router("/slack/events", state.clone());
        let url = format!("{}/files/spec.csv", server.uri());
        let body = message_with_file_body(&url, "spec.csv", "text/csv", Some(4));
        let _ = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        let evt = rx.recv().await.unwrap();
        let att = &evt.message.content["attachment"];
        assert!(att.get("data_base64").is_none());
    }

    #[tokio::test]
    async fn files_ignored_when_attachments_not_wired() {
        // Backward compat: without with_attachments the adapter ignores
        // files entirely (pre-C4a behaviour) — a plain Chat event, no
        // attachment, and no download attempt.
        let (state, mut rx) = make_state(None);
        let app = build_events_router("/slack/events", state.clone());
        let body = message_with_file_body(
            "https://files.slack.test/x",
            "spec.csv",
            "text/csv",
            Some(11),
        );
        let _ = app
            .oneshot(signed_request(&state, "/slack/events", &body))
            .await
            .unwrap();
        let evt = rx.recv().await.unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert!(evt.message.content.get("attachment").is_none());
        assert_eq!(evt.message.content["text"], "here is the spec");
    }
}
