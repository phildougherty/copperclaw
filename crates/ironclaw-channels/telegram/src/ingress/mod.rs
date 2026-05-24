//! Ingress strategies (long-poll and webhook) for the Telegram adapter.

pub mod long_poll;
pub mod webhook;

use crate::api::TelegramApi;
use crate::types::{
    Audio, CallbackQuery, Document, Message, PhotoSize, Sticker, Update, Video, VideoNote, Voice,
};
use chrono::{DateTime, TimeZone, Utc};
use ironclaw_channels_core::AdapterError;
use ironclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const MAX_INBOUND_FILENAME_LEN: usize = 255;

/// Settings the ingress layer needs to materialise an [`InboundEvent`].
#[derive(Debug, Clone)]
pub struct IngressSettings {
    /// When `true`, file-bearing updates trigger a `getFile` + download and
    /// are surfaced as [`MessageKind::Chat`] with `content["attachment"]`
    /// metadata pointing at the downloaded bytes.
    pub attachment_download: bool,
    /// Refuse to download anything larger than this many bytes; oversized
    /// attachments fall back to [`MessageKind::System`] with a
    /// `reason: "too_large"` note.
    pub max_attachment_bytes: u64,
    /// Resolved bot username (from `getMe`) used for mention detection.
    pub bot_username: Option<String>,
    /// Per-channel data directory; downloaded files land under
    /// `data_dir/inbox/<msg_id>/<filename>`.
    pub data_dir: PathBuf,
}

/// One inbound attachment after `getFile` resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttachmentDescriptor {
    /// Telegram file id for the source variant.
    file_id: String,
    /// Filename to use on disk after sanitisation.
    filename: String,
    /// MIME type if Telegram provided one (`None` for stickers without one).
    mime_type: Option<String>,
    /// Telegram-reported size, if any (informational; the actual byte count
    /// after download is authoritative).
    file_size: Option<u64>,
    /// High-level attachment category: `document` / `photo` / `audio` /
    /// `video` / `voice` / `video_note` / `sticker`.
    kind: &'static str,
}

/// Convert a Telegram [`Update`] into zero or one [`InboundEvent`]s,
/// downloading any attachment via `api` when the settings allow it.
///
/// Two shapes are handled today:
///
/// - `update.message`: regular chat / attachment messages → routed through
///   [`message_to_event`].
/// - `update.callback_query`: a user tapped an inline-keyboard button on
///   a card the agent sent. The button's `data` (i.e. the
///   [`ironclaw_channels_core::CardButton::value`]) is surfaced to the
///   agent as a plain chat event so the agent can react as if the user
///   had typed the value. The callback is acked via `answerCallbackQuery`
///   so the user's client stops showing the loading spinner.
///
/// Returns `Ok(Vec)` rather than streaming so callers (long-poll, webhook)
/// can `await` it once and then push the results.
pub async fn updates_to_events(
    update: &Update,
    api: &TelegramApi,
    settings: &IngressSettings,
) -> Vec<InboundEvent> {
    let mut out = Vec::new();
    if let Some(msg) = update.message.as_ref() {
        if let Some(evt) = message_to_event(msg, api, settings).await {
            out.push(evt);
        }
    }
    if let Some(cb) = update.callback_query.as_ref() {
        if let Some(evt) = callback_query_to_event(cb, api).await {
            out.push(evt);
        }
    }
    out
}

/// Synthesise an inbound chat event from a `callback_query` update and
/// fire the (fire-and-forget) `answerCallbackQuery` ack so the user's
/// client stops showing the spinner.
///
/// The event payload mimics a plain chat message:
///
/// - `kind = MessageKind::Chat`
/// - `content = { "text": callback_data }` — the agent receives the
///   button's `value` verbatim so it can branch on it.
/// - `channel_type = "telegram"`, `platform_id = chat_id` so the host
///   routes it back through the same wiring as a normal user message.
///
/// Returns `None` when the callback is unroutable — no `data` field,
/// or no chat id to route by. The ack is still attempted in those
/// cases so Telegram's spinner clears.
async fn callback_query_to_event(
    cb: &CallbackQuery,
    api: &TelegramApi,
) -> Option<InboundEvent> {
    // Best-effort ack — never block the event on it. Pass `None` so no
    // toast pops up over the chat input (the value is already arriving
    // as a normal chat message; an extra toast would be noise).
    if let Err(err) = api.answer_callback_query(&cb.id, None).await {
        tracing::warn!(
            error = %err,
            callback_id = cb.id.as_str(),
            "telegram answerCallbackQuery failed; surfacing event anyway",
        );
    }

    let data = cb.data.as_deref()?;
    // Prefer the chat the message lives in. Fall back to the user's own id
    // when the message envelope is absent (Telegram drops it for callbacks
    // on messages older than ~48h).
    let (platform_id, thread_id, is_group, original_message_id, ts) =
        if let Some(msg) = cb.message.as_ref() {
            (
                msg.chat.id.to_string(),
                msg.message_thread_id.map(|t| t.to_string()),
                matches!(msg.chat.kind.as_str(), "group" | "supergroup"),
                msg.message_id,
                ts_to_datetime(msg.date),
            )
        } else {
            (cb.from.id.to_string(), None, false, 0, Utc::now())
        };

    let channel_type = ChannelType::new(crate::CHANNEL_TYPE_STR);
    Some(InboundEvent {
        channel_type: channel_type.clone(),
        platform_id,
        thread_id,
        message: InboundMessage {
            // The callback id is unique; reuse it as the platform-side
            // message id so dedupe in the router still works.
            id: cb.id.clone(),
            kind: MessageKind::Chat,
            content: json!({
                "text": data,
                // Tag the event so handlers can tell a button-tap apart
                // from a typed message when the distinction matters.
                "callback": {
                    "id": cb.id,
                    "data": data,
                    "original_message_id": original_message_id,
                },
            }),
            timestamp: ts,
            is_mention: None,
            is_group: Some(is_group),
        },
        reply_to: None,
        sender: Some(SenderIdentity {
            channel_type,
            identity: cb.from.id.to_string(),
            display_name: cb
                .from
                .username
                .clone()
                .or_else(|| cb.from.first_name.clone()),
        }),
    })
}

async fn message_to_event(
    msg: &Message,
    api: &TelegramApi,
    settings: &IngressSettings,
) -> Option<InboundEvent> {
    let channel_type = ChannelType::new(crate::CHANNEL_TYPE_STR);
    let is_group = matches!(msg.chat.kind.as_str(), "group" | "supergroup");

    let attachment = pick_attachment(msg);
    let caption_or_text = msg
        .text
        .clone()
        .or_else(|| msg.caption.clone())
        .unwrap_or_default();

    let (kind, content) = if attachment.is_none() {
        // Unknown shape (e.g. callback) with no body and no attachment.
        msg.text.as_ref()?;
        (MessageKind::Chat, json!({ "text": caption_or_text }))
    } else if !settings.attachment_download {
        // Caller has opted out of downloading; preserve the legacy
        // metadata-only behaviour so this code path is still reachable.
        let att = attachment.expect("just checked");
        (MessageKind::System, legacy_metadata_value(&att))
    } else {
        let descriptor = attachment.expect("just checked");
        match download_one(api, settings, msg.message_id, &descriptor).await {
            DownloadOutcome::Ok { path, size } => {
                let mut content_obj = serde_json::Map::new();
                content_obj.insert("text".to_owned(), Value::String(caption_or_text.clone()));
                content_obj.insert(
                    "attachment".to_owned(),
                    Value::Object(attachment_json(&descriptor, &path, size)),
                );
                (MessageKind::Chat, Value::Object(content_obj))
            }
            DownloadOutcome::TooLarge { reported } => {
                let mut v = legacy_metadata_value(&descriptor);
                if let Value::Object(obj) = &mut v {
                    obj.insert("reason".to_owned(), Value::String("too_large".to_owned()));
                    obj.insert(
                        "limit".to_owned(),
                        Value::from(settings.max_attachment_bytes),
                    );
                    if let Some(r) = reported {
                        obj.insert("reported_size".to_owned(), Value::from(r));
                    }
                }
                (MessageKind::System, v)
            }
            DownloadOutcome::Failed { error } => {
                let mut v = legacy_metadata_value(&descriptor);
                if let Value::Object(obj) = &mut v {
                    obj.insert(
                        "reason".to_owned(),
                        Value::String("download_failed".to_owned()),
                    );
                    obj.insert("error".to_owned(), Value::String(format!("{error}")));
                }
                (MessageKind::System, v)
            }
        }
    };

    let is_mention = settings
        .bot_username
        .as_deref()
        .map(|name| message_mentions(msg, name));

    Some(InboundEvent {
        channel_type: channel_type.clone(),
        platform_id: msg.chat.id.to_string(),
        thread_id: msg.message_thread_id.map(|t| t.to_string()),
        message: InboundMessage {
            id: msg.message_id.to_string(),
            kind,
            content,
            timestamp: ts_to_datetime(msg.date),
            is_mention,
            is_group: Some(is_group),
        },
        reply_to: None,
        sender: msg.from.as_ref().map(|u| SenderIdentity {
            channel_type: channel_type.clone(),
            identity: u.id.to_string(),
            display_name: u.username.clone(),
        }),
    })
}

/// Pick the single attachment we should download from a Telegram `Message`.
///
/// Telegram messages carry at most one of `document` / `photo` / `audio` /
/// `video` / `voice` / `video_note` / `sticker`. We pick the largest photo
/// variant when several sizes are present.
fn pick_attachment(msg: &Message) -> Option<AttachmentDescriptor> {
    if let Some(doc) = msg.document.as_ref() {
        return Some(from_document(doc));
    }
    if !msg.photo.is_empty() {
        return Some(from_photo(&msg.photo, msg.message_id));
    }
    if let Some(audio) = msg.audio.as_ref() {
        return Some(from_audio(audio, msg.message_id));
    }
    if let Some(video) = msg.video.as_ref() {
        return Some(from_video(video, msg.message_id));
    }
    if let Some(voice) = msg.voice.as_ref() {
        return Some(from_voice(voice, msg.message_id));
    }
    if let Some(vn) = msg.video_note.as_ref() {
        return Some(from_video_note(vn, msg.message_id));
    }
    if let Some(sticker) = msg.sticker.as_ref() {
        return Some(from_sticker(sticker, msg.message_id));
    }
    None
}

fn from_document(doc: &Document) -> AttachmentDescriptor {
    let filename = sanitize_filename(doc.file_name.as_deref(), "document.bin");
    AttachmentDescriptor {
        file_id: doc.file_id.clone(),
        filename,
        mime_type: doc.mime_type.clone(),
        file_size: doc.file_size,
        kind: "document",
    }
}

fn from_photo(sizes: &[PhotoSize], message_id: i64) -> AttachmentDescriptor {
    // Pick the largest available variant by `file_size`, falling back to the
    // last entry (Telegram returns photo sizes ordered smallest -> largest).
    let largest = sizes
        .iter()
        .max_by_key(|p| (p.file_size.unwrap_or(0), p.width.saturating_mul(p.height)))
        .or_else(|| sizes.last())
        .expect("non-empty");
    AttachmentDescriptor {
        file_id: largest.file_id.clone(),
        filename: format!("photo-{message_id}.jpg"),
        mime_type: Some("image/jpeg".to_owned()),
        file_size: largest.file_size,
        kind: "photo",
    }
}

fn from_audio(audio: &Audio, message_id: i64) -> AttachmentDescriptor {
    let fallback = format!("audio-{message_id}.bin");
    AttachmentDescriptor {
        file_id: audio.file_id.clone(),
        filename: sanitize_filename(audio.file_name.as_deref(), &fallback),
        mime_type: audio.mime_type.clone(),
        file_size: audio.file_size,
        kind: "audio",
    }
}

fn from_video(video: &Video, message_id: i64) -> AttachmentDescriptor {
    let fallback = format!("video-{message_id}.mp4");
    AttachmentDescriptor {
        file_id: video.file_id.clone(),
        filename: sanitize_filename(video.file_name.as_deref(), &fallback),
        mime_type: video.mime_type.clone(),
        file_size: video.file_size,
        kind: "video",
    }
}

fn from_voice(voice: &Voice, message_id: i64) -> AttachmentDescriptor {
    let filename = format!("voice-{message_id}.ogg");
    AttachmentDescriptor {
        file_id: voice.file_id.clone(),
        filename,
        mime_type: voice.mime_type.clone().or_else(|| Some("audio/ogg".into())),
        file_size: voice.file_size,
        kind: "voice",
    }
}

fn from_video_note(vn: &VideoNote, message_id: i64) -> AttachmentDescriptor {
    AttachmentDescriptor {
        file_id: vn.file_id.clone(),
        filename: format!("video-note-{message_id}.mp4"),
        mime_type: Some("video/mp4".to_owned()),
        file_size: vn.file_size,
        kind: "video_note",
    }
}

fn from_sticker(sticker: &Sticker, message_id: i64) -> AttachmentDescriptor {
    let ext = if sticker.is_video {
        "webm"
    } else if sticker.is_animated {
        "tgs"
    } else {
        "webp"
    };
    let mime = if sticker.is_video {
        Some("video/webm".to_owned())
    } else if sticker.is_animated {
        Some("application/x-tgsticker".to_owned())
    } else {
        Some("image/webp".to_owned())
    };
    AttachmentDescriptor {
        file_id: sticker.file_id.clone(),
        filename: format!("sticker-{message_id}.{ext}"),
        mime_type: mime,
        file_size: sticker.file_size,
        kind: "sticker",
    }
}

/// Surface the legacy metadata-only JSON the adapter used before
/// attachment downloads landed. Reused both by the
/// `attachment_download = false` path and by every download-failure
/// fallback.
fn legacy_metadata_value(att: &AttachmentDescriptor) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "kind".to_owned(),
        Value::String(format!("telegram.{}", att.kind)),
    );
    obj.insert("file_id".to_owned(), Value::String(att.file_id.clone()));
    obj.insert(
        "file_name".to_owned(),
        att.filename
            .is_empty()
            .then_some(Value::Null)
            .unwrap_or_else(|| Value::String(att.filename.clone())),
    );
    obj.insert(
        "mime_type".to_owned(),
        att.mime_type
            .clone()
            .map_or(Value::Null, Value::String),
    );
    obj.insert(
        "file_size".to_owned(),
        att.file_size.map_or(Value::Null, Value::from),
    );
    Value::Object(obj)
}

/// Build the `content["attachment"]` object describing a downloaded file.
fn attachment_json(
    att: &AttachmentDescriptor,
    path: &Path,
    actual_size: u64,
) -> serde_json::Map<String, Value> {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "kind".to_owned(),
        Value::String(format!("telegram.{}", att.kind)),
    );
    obj.insert("file_id".to_owned(), Value::String(att.file_id.clone()));
    obj.insert("filename".to_owned(), Value::String(att.filename.clone()));
    obj.insert(
        "path".to_owned(),
        Value::String(path.to_string_lossy().into_owned()),
    );
    obj.insert(
        "mime_type".to_owned(),
        att.mime_type
            .clone()
            .map_or(Value::Null, Value::String),
    );
    obj.insert("size".to_owned(), Value::from(actual_size));
    obj
}

/// Outcome of a single attachment fetch.
enum DownloadOutcome {
    /// File written under `data_dir/inbox/<msg_id>/<filename>`.
    Ok { path: PathBuf, size: u64 },
    /// Telegram-reported size exceeded the configured cap; we never invoked
    /// `getFile`. `reported` is the size Telegram included with the update,
    /// if any.
    TooLarge { reported: Option<u64> },
    /// `getFile` or the binary download failed; the captured error is
    /// surfaced verbatim in the `MessageKind::System` fallback so the host
    /// can log / alert.
    Failed { error: AdapterError },
}

async fn download_one(
    api: &TelegramApi,
    settings: &IngressSettings,
    message_id: i64,
    descriptor: &AttachmentDescriptor,
) -> DownloadOutcome {
    if let Some(size) = descriptor.file_size {
        if size > settings.max_attachment_bytes {
            return DownloadOutcome::TooLarge {
                reported: Some(size),
            };
        }
    }
    let meta = match api.get_file(&descriptor.file_id).await {
        Ok(m) => m,
        Err(error) => return DownloadOutcome::Failed { error },
    };
    if let Some(size) = meta.file_size {
        if size > settings.max_attachment_bytes {
            return DownloadOutcome::TooLarge {
                reported: Some(size),
            };
        }
    }
    let Some(file_path) = meta.file_path.as_deref() else {
        return DownloadOutcome::Failed {
            error: AdapterError::Transport(
                "telegram getFile returned no file_path".to_owned(),
            ),
        };
    };
    let bytes = match api.download_file(file_path).await {
        Ok(b) => b,
        Err(error) => return DownloadOutcome::Failed { error },
    };
    if (bytes.len() as u64) > settings.max_attachment_bytes {
        return DownloadOutcome::TooLarge {
            reported: Some(bytes.len() as u64),
        };
    }
    match write_attachment(
        &settings.data_dir,
        message_id,
        &descriptor.filename,
        &bytes,
    )
    .await
    {
        Ok(path) => DownloadOutcome::Ok {
            path,
            size: bytes.len() as u64,
        },
        Err(error) => DownloadOutcome::Failed { error },
    }
}

/// Write `bytes` into `<data_dir>/inbox/<msg_id>/<filename>`, creating the
/// parent directories as needed. Returns the on-disk path.
async fn write_attachment(
    data_dir: &Path,
    message_id: i64,
    filename: &str,
    bytes: &[u8],
) -> Result<PathBuf, AdapterError> {
    let inbox = data_dir.join("inbox").join(message_id.to_string());
    tokio::fs::create_dir_all(&inbox).await.map_err(|e| {
        AdapterError::Transport(format!(
            "telegram inbox create {} failed: {e}",
            inbox.display()
        ))
    })?;
    let path = inbox.join(filename);
    tokio::fs::write(&path, bytes).await.map_err(|e| {
        AdapterError::Transport(format!(
            "telegram inbox write {} failed: {e}",
            path.display()
        ))
    })?;
    Ok(path)
}

/// Sanitise a Telegram-supplied filename into something safe to use as a
/// single path component. Falls back to `fallback` when the supplied name
/// is empty or unusable.
fn sanitize_filename(name: Option<&str>, fallback: &str) -> String {
    let raw = name.unwrap_or("").trim();
    if raw.is_empty() {
        return fallback.to_owned();
    }
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Trim leading dots / underscores to avoid hidden-file names and to drop
    // path-traversal residue (e.g. `..` becomes `__` then `""`). Trailing
    // characters are preserved so callers can see how the original differed.
    let trimmed = cleaned.trim_start_matches(['.', '_']);
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    if trimmed.len() > MAX_INBOUND_FILENAME_LEN {
        return trimmed[..MAX_INBOUND_FILENAME_LEN].to_owned();
    }
    trimmed.to_owned()
}

fn message_mentions(msg: &Message, bot_username: &str) -> bool {
    let Some(text) = msg.text.as_deref() else {
        return false;
    };
    let target = format!("@{bot_username}");

    for entity in &msg.entities {
        match entity.kind.as_str() {
            "mention" => {
                if let (Some(start), Some(len)) =
                    (usize::try_from(entity.offset).ok(), usize::try_from(entity.length).ok())
                {
                    if let Some(slice) = slice_utf16(text, start, len) {
                        if slice.eq_ignore_ascii_case(&target) {
                            return true;
                        }
                    }
                }
            }
            "text_mention" => {
                if let Some(user) = entity.user.as_ref() {
                    if user
                        .username
                        .as_deref()
                        .is_some_and(|u| u.eq_ignore_ascii_case(bot_username))
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Slice a UTF-16 view of `text` between `offset` and `offset + length`.
/// Telegram entity offsets are UTF-16; this helper is best-effort and
/// returns `None` if the slice is malformed.
fn slice_utf16(text: &str, offset: usize, length: usize) -> Option<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let end = offset.checked_add(length)?;
    let slice = units.get(offset..end)?;
    String::from_utf16(slice).ok()
}

fn ts_to_datetime(date: i64) -> DateTime<Utc> {
    match Utc.timestamp_opt(date, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Chat, Document, MessageEntity, User};
    use ironclaw_types::MessageKind as Mk;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base_chat(kind: &str) -> Chat {
        Chat {
            id: 100,
            kind: kind.into(),
            title: None,
            username: None,
        }
    }

    fn text_msg(text: &str) -> Message {
        Message {
            message_id: 7,
            message_thread_id: None,
            from: Some(User {
                id: 200,
                is_bot: false,
                first_name: Some("Alice".into()),
                last_name: None,
                username: Some("alice".into()),
            }),
            chat: base_chat("private"),
            date: 1_700_000_000,
            text: Some(text.into()),
            caption: None,
            entities: vec![],
            document: None,
            photo: vec![],
            audio: None,
            video: None,
            voice: None,
            video_note: None,
            sticker: None,
        }
    }

    fn has_extension(name: &str, ext: &str) -> bool {
        std::path::Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
    }

    fn default_settings(dir: &Path) -> IngressSettings {
        IngressSettings {
            attachment_download: true,
            max_attachment_bytes: crate::config::DEFAULT_MAX_ATTACHMENT_BYTES,
            bot_username: None,
            data_dir: dir.to_path_buf(),
        }
    }

    async fn dummy_api() -> (TelegramApi, MockServer) {
        let s = MockServer::start().await;
        let api = TelegramApi::new(s.uri(), "tok");
        (api, s)
    }

    #[tokio::test]
    async fn text_message_maps_to_chat_event() {
        let m = text_msg("hello");
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let evts = updates_to_events(&update, &api, &default_settings(dir.path())).await;
        assert_eq!(evts.len(), 1);
        let e = &evts[0];
        assert_eq!(e.channel_type.as_str(), "telegram");
        assert_eq!(e.platform_id, "100");
        assert!(e.thread_id.is_none());
        assert_eq!(e.message.kind, Mk::Chat);
        assert_eq!(e.message.content["text"], "hello");
        assert_eq!(e.message.is_group, Some(false));
        let sender = e.sender.as_ref().expect("sender");
        assert_eq!(sender.identity, "200");
        assert_eq!(sender.display_name.as_deref(), Some("alice"));
        assert_eq!(sender.channel_type.as_str(), "telegram");
    }

    #[tokio::test]
    async fn document_message_without_download_falls_back_to_system() {
        let mut m = text_msg("ignored");
        m.text = None;
        m.document = Some(Document {
            file_id: "F".into(),
            file_unique_id: Some("U".into()),
            file_name: Some("a.txt".into()),
            mime_type: Some("text/plain".into()),
            file_size: Some(99),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let mut settings = default_settings(dir.path());
        settings.attachment_download = false;
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].message.kind, Mk::System);
        assert_eq!(evts[0].message.content["kind"], "telegram.document");
        assert_eq!(evts[0].message.content["file_id"], "F");
    }

    #[tokio::test]
    async fn group_chat_marks_is_group_true() {
        let mut m = text_msg("hi");
        m.chat = base_chat("group");
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &default_settings(dir.path()),
        )
        .await;
        assert_eq!(evts[0].message.is_group, Some(true));
    }

    #[tokio::test]
    async fn supergroup_chat_marks_is_group_true() {
        let mut m = text_msg("hi");
        m.chat = base_chat("supergroup");
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &default_settings(dir.path()),
        )
        .await;
        assert_eq!(evts[0].message.is_group, Some(true));
    }

    #[tokio::test]
    async fn forum_thread_id_propagates() {
        let mut m = text_msg("hi");
        m.message_thread_id = Some(42);
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &default_settings(dir.path()),
        )
        .await;
        assert_eq!(evts[0].thread_id.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn empty_update_produces_no_events() {
        let update = Update {
            update_id: 1,
            message: None,
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        assert!(updates_to_events(&update, &api, &default_settings(dir.path()))
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn message_without_text_or_attachment_is_skipped() {
        let mut m = text_msg("ignored");
        m.text = None;
        m.document = None;
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &default_settings(dir.path()),
        )
        .await;
        assert!(evts.is_empty());
    }

    #[tokio::test]
    async fn mention_entity_matching_bot_username_is_mention_true() {
        let mut m = text_msg("@ironbot hi");
        m.entities = vec![MessageEntity {
            kind: "mention".into(),
            offset: 0,
            length: 8,
            user: None,
        }];
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let mut settings = default_settings(dir.path());
        settings.bot_username = Some("ironbot".into());
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &settings,
        )
        .await;
        assert_eq!(evts[0].message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn mention_entity_other_username_is_mention_false() {
        let mut m = text_msg("@other hi");
        m.entities = vec![MessageEntity {
            kind: "mention".into(),
            offset: 0,
            length: 6,
            user: None,
        }];
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let mut settings = default_settings(dir.path());
        settings.bot_username = Some("ironbot".into());
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &settings,
        )
        .await;
        assert_eq!(evts[0].message.is_mention, Some(false));
    }

    #[tokio::test]
    async fn text_mention_entity_with_user_matches() {
        let mut m = text_msg("Hi You");
        m.entities = vec![MessageEntity {
            kind: "text_mention".into(),
            offset: 3,
            length: 3,
            user: Some(User {
                id: 1,
                is_bot: true,
                first_name: None,
                last_name: None,
                username: Some("IronBot".into()),
            }),
        }];
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let mut settings = default_settings(dir.path());
        settings.bot_username = Some("ironbot".into());
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &settings,
        )
        .await;
        assert_eq!(evts[0].message.is_mention, Some(true));
    }

    #[tokio::test]
    async fn text_mention_entity_without_user_does_not_match() {
        let mut m = text_msg("Hi You");
        m.entities = vec![MessageEntity {
            kind: "text_mention".into(),
            offset: 0,
            length: 2,
            user: None,
        }];
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let mut settings = default_settings(dir.path());
        settings.bot_username = Some("ironbot".into());
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &settings,
        )
        .await;
        assert_eq!(evts[0].message.is_mention, Some(false));
    }

    #[tokio::test]
    async fn is_mention_none_when_bot_username_not_known() {
        let m = text_msg("hi");
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &default_settings(dir.path()),
        )
        .await;
        assert_eq!(evts[0].message.is_mention, None);
    }

    #[tokio::test]
    async fn unknown_entity_type_is_not_mention() {
        let mut m = text_msg("@ironbot hi");
        m.entities = vec![MessageEntity {
            kind: "hashtag".into(),
            offset: 0,
            length: 8,
            user: None,
        }];
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let mut settings = default_settings(dir.path());
        settings.bot_username = Some("ironbot".into());
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &settings,
        )
        .await;
        assert_eq!(evts[0].message.is_mention, Some(false));
    }

    #[test]
    fn slice_utf16_returns_some_for_valid_range() {
        let s = slice_utf16("hello world", 6, 5);
        assert_eq!(s.as_deref(), Some("world"));
    }

    #[test]
    fn slice_utf16_returns_none_when_overflow() {
        assert!(slice_utf16("a", 100, 1).is_none());
    }

    #[test]
    fn slice_utf16_returns_none_on_arith_overflow() {
        assert!(slice_utf16("a", usize::MAX, 1).is_none());
    }

    #[test]
    fn ts_to_datetime_valid_seconds() {
        let dt = ts_to_datetime(0);
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn ts_to_datetime_falls_back_for_extreme_inputs() {
        // i64::MAX overflows the conversion; the helper falls back to "now".
        let _ = ts_to_datetime(i64::MAX);
    }

    #[tokio::test]
    async fn message_without_sender_produces_event_with_no_sender() {
        let mut m = text_msg("hi");
        m.from = None;
        let dir = TempDir::new().unwrap();
        let (api, _s) = dummy_api().await;
        let evts = updates_to_events(
            &Update {
                update_id: 1,
                message: Some(m),
                edited_message: None,
                channel_post: None,
            callback_query: None,
            },
            &api,
            &default_settings(dir.path()),
        )
        .await;
        assert!(evts[0].sender.is_none());
    }

    // --- Attachment download tests ---

    fn doc_msg(file_id: &str, name: &str, size: Option<u64>) -> Message {
        let mut m = text_msg("ignored");
        m.text = None;
        m.document = Some(Document {
            file_id: file_id.into(),
            file_unique_id: Some("U".into()),
            file_name: Some(name.into()),
            mime_type: Some("text/plain".into()),
            file_size: size,
        });
        m
    }

    async fn mount_get_file(server: &MockServer, file_id: &str, file_path: &str, size: u64) {
        let body = json!({
            "ok": true,
            "result": {
                "file_id": file_id,
                "file_unique_id": "U",
                "file_size": size,
                "file_path": file_path,
            }
        });
        Mock::given(method("POST"))
            .and(path("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mount_file_download(server: &MockServer, file_path: &str, bytes: Vec<u8>) {
        let path_with_slash = format!("/file/bottok/{file_path}");
        Mock::given(method("GET"))
            .and(path(path_with_slash))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn document_download_succeeds_and_writes_to_inbox() {
        let server = MockServer::start().await;
        mount_get_file(&server, "F", "documents/file_1.txt", 5).await;
        mount_file_download(&server, "documents/file_1.txt", b"hello".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let m = doc_msg("F", "a.txt", Some(5));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts.len(), 1);
        let e = &evts[0];
        assert_eq!(e.message.kind, Mk::Chat);
        let att = &e.message.content["attachment"];
        assert_eq!(att["kind"], "telegram.document");
        assert_eq!(att["filename"], "a.txt");
        assert_eq!(att["size"], 5);
        let on_disk = att["path"].as_str().unwrap();
        let bytes = std::fs::read(on_disk).unwrap();
        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn document_oversized_by_message_metadata_falls_back_to_system() {
        let server = MockServer::start().await;
        // No mocks for getFile / download — we should never call them.
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let mut settings = default_settings(dir.path());
        settings.max_attachment_bytes = 4;
        let m = doc_msg("F", "big.bin", Some(1024));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts.len(), 1);
        let e = &evts[0];
        assert_eq!(e.message.kind, Mk::System);
        assert_eq!(e.message.content["reason"], "too_large");
        assert_eq!(e.message.content["limit"], 4);
        assert_eq!(e.message.content["reported_size"], 1024);
    }

    #[tokio::test]
    async fn document_oversized_after_getfile_falls_back_to_system() {
        let server = MockServer::start().await;
        mount_get_file(&server, "F", "documents/big.bin", 1024).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let mut settings = default_settings(dir.path());
        settings.max_attachment_bytes = 16;
        // Message metadata reports a small size, but getFile reports a big one.
        let m = doc_msg("F", "big.bin", Some(8));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts[0].message.kind, Mk::System);
        assert_eq!(evts[0].message.content["reason"], "too_large");
    }

    #[tokio::test]
    async fn document_oversized_after_body_read_falls_back_to_system() {
        let server = MockServer::start().await;
        mount_get_file(&server, "F", "documents/big.bin", 0).await;
        mount_file_download(&server, "documents/big.bin", vec![0u8; 32]).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let mut settings = default_settings(dir.path());
        settings.max_attachment_bytes = 16;
        let m = doc_msg("F", "big.bin", None);
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts[0].message.kind, Mk::System);
        assert_eq!(evts[0].message.content["reason"], "too_large");
    }

    #[tokio::test]
    async fn getfile_auth_failure_falls_back_to_system_with_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "ok": false, "error_code": 401, "description": "Unauthorized"
            })))
            .mount(&server)
            .await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let m = doc_msg("F", "a.txt", Some(5));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let e = &evts[0];
        assert_eq!(e.message.kind, Mk::System);
        assert_eq!(e.message.content["reason"], "download_failed");
        let err = e.message.content["error"].as_str().unwrap();
        assert!(err.contains("auth") || err.contains("Unauthorized"), "got `{err}`");
    }

    #[tokio::test]
    async fn download_body_5xx_falls_back_to_system_with_error() {
        let server = MockServer::start().await;
        mount_get_file(&server, "F", "documents/a.txt", 5).await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/documents/a.txt"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&server)
            .await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let m = doc_msg("F", "a.txt", Some(5));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts[0].message.kind, Mk::System);
        assert_eq!(evts[0].message.content["reason"], "download_failed");
    }

    #[tokio::test]
    async fn getfile_missing_file_path_falls_back_to_system() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "file_id": "F", "file_unique_id": "U", "file_size": 1 }
            })))
            .mount(&server)
            .await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let m = doc_msg("F", "a.txt", Some(1));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts[0].message.kind, Mk::System);
        assert_eq!(evts[0].message.content["reason"], "download_failed");
    }

    #[tokio::test]
    async fn photo_picks_largest_variant_and_writes_jpg() {
        let server = MockServer::start().await;
        mount_get_file(&server, "L", "photos/big.jpg", 9).await;
        mount_file_download(&server, "photos/big.jpg", b"jpegjpeg!".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("a photo");
        m.text = None;
        m.caption = Some("look".into());
        m.photo = vec![
            PhotoSize {
                file_id: "S".into(),
                file_unique_id: None,
                width: 90,
                height: 90,
                file_size: Some(1),
            },
            PhotoSize {
                file_id: "M".into(),
                file_unique_id: None,
                width: 320,
                height: 320,
                file_size: Some(3),
            },
            PhotoSize {
                file_id: "L".into(),
                file_unique_id: None,
                width: 800,
                height: 800,
                file_size: Some(9),
            },
        ];
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let e = &evts[0];
        assert_eq!(e.message.kind, Mk::Chat);
        assert_eq!(e.message.content["text"], "look");
        let att = &e.message.content["attachment"];
        assert_eq!(att["kind"], "telegram.photo");
        let filename = att["filename"].as_str().unwrap();
        assert!(has_extension(filename, "jpg"), "got `{filename}`");
        assert_eq!(att["mime_type"], "image/jpeg");
    }

    #[tokio::test]
    async fn audio_message_downloads_with_supplied_filename() {
        let server = MockServer::start().await;
        mount_get_file(&server, "A", "music/song.mp3", 4).await;
        mount_file_download(&server, "music/song.mp3", b"abcd".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.audio = Some(Audio {
            file_id: "A".into(),
            file_unique_id: None,
            duration: 60,
            performer: Some("p".into()),
            title: Some("t".into()),
            file_name: Some("song.mp3".into()),
            mime_type: Some("audio/mpeg".into()),
            file_size: Some(4),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert_eq!(att["kind"], "telegram.audio");
        assert_eq!(att["filename"], "song.mp3");
        assert_eq!(att["mime_type"], "audio/mpeg");
    }

    #[tokio::test]
    async fn video_message_downloads_with_fallback_filename_when_missing() {
        let server = MockServer::start().await;
        mount_get_file(&server, "V", "videos/v.mp4", 6).await;
        mount_file_download(&server, "videos/v.mp4", vec![0u8; 6]).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.video = Some(Video {
            file_id: "V".into(),
            file_unique_id: None,
            width: 1280,
            height: 720,
            duration: 12,
            file_name: None,
            mime_type: Some("video/mp4".into()),
            file_size: Some(6),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert_eq!(att["kind"], "telegram.video");
        let filename = att["filename"].as_str().unwrap();
        assert!(filename.starts_with("video-"));
        assert!(has_extension(filename, "mp4"));
    }

    #[tokio::test]
    async fn voice_message_downloads_with_default_mime() {
        let server = MockServer::start().await;
        mount_get_file(&server, "VO", "voices/v.ogg", 3).await;
        mount_file_download(&server, "voices/v.ogg", b"oga".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.voice = Some(Voice {
            file_id: "VO".into(),
            file_unique_id: None,
            duration: 4,
            mime_type: None,
            file_size: Some(3),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert_eq!(att["kind"], "telegram.voice");
        assert_eq!(att["mime_type"], "audio/ogg");
    }

    #[tokio::test]
    async fn video_note_downloads_with_synthesised_filename() {
        let server = MockServer::start().await;
        mount_get_file(&server, "VN", "video_notes/n.mp4", 4).await;
        mount_file_download(&server, "video_notes/n.mp4", vec![1, 2, 3, 4]).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.video_note = Some(VideoNote {
            file_id: "VN".into(),
            file_unique_id: None,
            length: 100,
            duration: 3,
            file_size: Some(4),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert_eq!(att["kind"], "telegram.video_note");
        let filename = att["filename"].as_str().unwrap();
        assert!(filename.starts_with("video-note-"));
        assert!(has_extension(filename, "mp4"));
    }

    #[tokio::test]
    async fn static_sticker_uses_webp_extension() {
        let server = MockServer::start().await;
        mount_get_file(&server, "ST", "stickers/s.webp", 3).await;
        mount_file_download(&server, "stickers/s.webp", b"wpb".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.sticker = Some(Sticker {
            file_id: "ST".into(),
            file_unique_id: None,
            width: 512,
            height: 512,
            is_animated: false,
            is_video: false,
            emoji: Some("smile".into()),
            set_name: None,
            file_size: Some(3),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert_eq!(att["kind"], "telegram.sticker");
        assert!(has_extension(att["filename"].as_str().unwrap(), "webp"));
        assert_eq!(att["mime_type"], "image/webp");
    }

    #[tokio::test]
    async fn animated_sticker_uses_tgs_extension() {
        let server = MockServer::start().await;
        mount_get_file(&server, "ST2", "stickers/s.tgs", 1).await;
        mount_file_download(&server, "stickers/s.tgs", b"x".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.sticker = Some(Sticker {
            file_id: "ST2".into(),
            file_unique_id: None,
            width: 512,
            height: 512,
            is_animated: true,
            is_video: false,
            emoji: None,
            set_name: None,
            file_size: Some(1),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert!(has_extension(att["filename"].as_str().unwrap(), "tgs"));
        assert_eq!(att["mime_type"], "application/x-tgsticker");
    }

    #[tokio::test]
    async fn video_sticker_uses_webm_extension() {
        let server = MockServer::start().await;
        mount_get_file(&server, "ST3", "stickers/s.webm", 1).await;
        mount_file_download(&server, "stickers/s.webm", b"x".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.sticker = Some(Sticker {
            file_id: "ST3".into(),
            file_unique_id: None,
            width: 512,
            height: 512,
            is_animated: false,
            is_video: true,
            emoji: None,
            set_name: None,
            file_size: Some(1),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert!(has_extension(att["filename"].as_str().unwrap(), "webm"));
        assert_eq!(att["mime_type"], "video/webm");
    }

    #[tokio::test]
    async fn caption_is_surfaced_as_text_when_no_text_field() {
        let server = MockServer::start().await;
        mount_get_file(&server, "F", "documents/x.bin", 1).await;
        mount_file_download(&server, "documents/x.bin", b"x".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let mut m = text_msg("ignored");
        m.text = None;
        m.caption = Some("see attached".into());
        m.document = Some(Document {
            file_id: "F".into(),
            file_unique_id: None,
            file_name: Some("x.bin".into()),
            mime_type: None,
            file_size: Some(1),
        });
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        assert_eq!(evts[0].message.content["text"], "see attached");
    }

    #[tokio::test]
    async fn dangerous_filename_is_sanitised() {
        let server = MockServer::start().await;
        mount_get_file(&server, "F", "documents/safe.bin", 1).await;
        mount_file_download(&server, "documents/safe.bin", b"x".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let m = doc_msg("F", "../../etc/passwd", Some(1));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        let filename = att["filename"].as_str().unwrap();
        assert!(!filename.contains('/'), "got `{filename}`");
        assert!(!filename.starts_with('.'), "got `{filename}`");
    }

    #[tokio::test]
    async fn empty_filename_falls_back_to_default() {
        let server = MockServer::start().await;
        mount_get_file(&server, "F", "documents/x.bin", 1).await;
        mount_file_download(&server, "documents/x.bin", b"x".to_vec()).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();
        let settings = default_settings(dir.path());
        let m = doc_msg("F", "", Some(1));
        let update = Update {
            update_id: 1,
            message: Some(m),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let evts = updates_to_events(&update, &api, &settings).await;
        let att = &evts[0].message.content["attachment"];
        assert_eq!(att["filename"], "document.bin");
    }

    #[test]
    fn sanitize_filename_falls_back_when_input_is_only_dots() {
        assert_eq!(sanitize_filename(Some("...."), "x"), "x");
        assert_eq!(sanitize_filename(None, "x"), "x");
        assert_eq!(sanitize_filename(Some(""), "x"), "x");
    }

    #[test]
    fn sanitize_filename_truncates_long_input() {
        let long = "a".repeat(1024);
        let safe = sanitize_filename(Some(&long), "x");
        assert!(safe.len() <= MAX_INBOUND_FILENAME_LEN);
    }

    #[test]
    fn sanitize_filename_replaces_unsafe_characters() {
        assert_eq!(sanitize_filename(Some("a b.c"), "x"), "a_b.c");
        assert_eq!(sanitize_filename(Some("weird?name!"), "x"), "weird_name_");
    }

    #[test]
    fn legacy_metadata_carries_known_fields() {
        let att = AttachmentDescriptor {
            file_id: "F".into(),
            filename: "a.txt".into(),
            mime_type: Some("text/plain".into()),
            file_size: Some(7),
            kind: "document",
        };
        let v = legacy_metadata_value(&att);
        assert_eq!(v["kind"], "telegram.document");
        assert_eq!(v["file_id"], "F");
        assert_eq!(v["file_name"], "a.txt");
        assert_eq!(v["mime_type"], "text/plain");
        assert_eq!(v["file_size"], 7);
    }

    // ---------------------------------------------------------------
    // Wave 2b: callback_query → InboundEvent.
    // ---------------------------------------------------------------

    async fn mount_ack(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/bottok/answerCallbackQuery"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": true
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn callback_query_routes_via_chat_id_and_tags_payload() {
        let server = MockServer::start().await;
        mount_ack(&server).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();

        let update = Update {
            update_id: 99,
            message: None,
            edited_message: None,
            channel_post: None,
            callback_query: Some(CallbackQuery {
                id: "cb-1".into(),
                from: User {
                    id: 7,
                    is_bot: false,
                    first_name: Some("Bob".into()),
                    last_name: None,
                    username: Some("bob".into()),
                },
                message: Some(text_msg("ignored")),
                data: Some("approve:42".into()),
            }),
        };
        let evts =
            updates_to_events(&update, &api, &default_settings(dir.path())).await;
        assert_eq!(evts.len(), 1);
        let e = &evts[0];
        assert_eq!(e.channel_type.as_str(), "telegram");
        // text_msg uses chat.id = 100.
        assert_eq!(e.platform_id, "100");
        assert_eq!(e.message.kind, Mk::Chat);
        assert_eq!(e.message.content["text"], "approve:42");
        assert_eq!(e.message.content["callback"]["id"], "cb-1");
        assert_eq!(e.message.content["callback"]["data"], "approve:42");
        // Sender carries the user who tapped, not the user from the
        // original message.
        let s = e.sender.as_ref().unwrap();
        assert_eq!(s.identity, "7");
        assert_eq!(s.display_name.as_deref(), Some("bob"));
    }

    #[tokio::test]
    async fn callback_query_falls_back_to_user_id_when_message_missing() {
        let server = MockServer::start().await;
        mount_ack(&server).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();

        let update = Update {
            update_id: 1,
            message: None,
            edited_message: None,
            channel_post: None,
            callback_query: Some(CallbackQuery {
                id: "cb-2".into(),
                from: User {
                    id: 555,
                    is_bot: false,
                    first_name: Some("X".into()),
                    last_name: None,
                    username: None,
                },
                message: None,
                data: Some("x".into()),
            }),
        };
        let evts =
            updates_to_events(&update, &api, &default_settings(dir.path())).await;
        assert_eq!(evts.len(), 1);
        // Platform id falls back to the user id.
        assert_eq!(evts[0].platform_id, "555");
        assert_eq!(evts[0].message.content["text"], "x");
        assert_eq!(evts[0].message.content["callback"]["original_message_id"], 0);
    }

    #[tokio::test]
    async fn callback_query_without_data_yields_no_event_but_still_acks() {
        let server = MockServer::start().await;
        mount_ack(&server).await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();

        let update = Update {
            update_id: 1,
            message: None,
            edited_message: None,
            channel_post: None,
            callback_query: Some(CallbackQuery {
                id: "cb-3".into(),
                from: User {
                    id: 1,
                    is_bot: false,
                    first_name: None,
                    last_name: None,
                    username: None,
                },
                message: Some(text_msg("x")),
                data: None,
            }),
        };
        let evts =
            updates_to_events(&update, &api, &default_settings(dir.path())).await;
        assert!(evts.is_empty());
        // The ack endpoint must still have been hit.
        let reqs = server.received_requests().await.unwrap();
        assert!(
            reqs.iter()
                .any(|r| r.url.path().ends_with("/answerCallbackQuery")),
            "expected an answerCallbackQuery request"
        );
    }

    #[tokio::test]
    async fn callback_query_event_emitted_even_when_ack_fails() {
        // Resilience: the ack failing should not swallow the inbound event.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/answerCallbackQuery"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "ok": false, "error_code": 400, "description": "expired"
            })))
            .mount(&server)
            .await;
        let api = TelegramApi::new(server.uri(), "tok");
        let dir = TempDir::new().unwrap();

        let update = Update {
            update_id: 1,
            message: None,
            edited_message: None,
            channel_post: None,
            callback_query: Some(CallbackQuery {
                id: "cb-4".into(),
                from: User {
                    id: 2,
                    is_bot: false,
                    first_name: None,
                    last_name: None,
                    username: Some("u".into()),
                },
                message: Some(text_msg("y")),
                data: Some("payload".into()),
            }),
        };
        let evts =
            updates_to_events(&update, &api, &default_settings(dir.path())).await;
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].message.content["text"], "payload");
    }
}
