//! Mapping from Discord `MESSAGE_CREATE` dispatch payloads to
//! `copperclaw_types::InboundEvent`.
//!
//! All functions here are pure — they take a `serde_json::Value` plus the
//! bot's own user id and return either a fully formed `InboundEvent` or an
//! `AdapterError::BadRequest` when required fields are missing.
//!
//! ## Thread mapping
//!
//! Discord exposes threads as **separate channels**: a thread message has
//! its own `channel_id` (the thread channel) distinct from the parent
//! channel. There is no separate `thread.id` in the message payload.
//!
//! We model that as:
//!
//! - `platform_id = d.channel_id` (the actual destination — replies go to
//!   the same channel id, whether that's a normal channel or a thread).
//! - `thread_id = None` unless the message references another message via
//!   `message_reference.message_id`, in which case we surface that as the
//!   thread id so the router can keep the conversation together.
//!
//! This matches the slim mapping the spec asks for and keeps replies
//! addressing the same Discord channel id the message came from.

use crate::rest::DiscordRest;
use chrono::Utc;
use copperclaw_channels_core::AdapterError;
use copperclaw_channels_core::inbound_file::{
    STAGED_PATH_KEY, STAGING_SUBDIR, sanitize_filename, stage_inbound_file,
};
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, ReplyTo, SenderIdentity,
};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// Channel-type string registered by this crate (`"discord"`).
pub const CHANNEL_TYPE_STR: &str = "discord";

/// Largest image we inline as base64 into the inbound message so a
/// vision-capable model sees the picture without a tool read. Mirrors the
/// Telegram precedent (`telegram/src/ingress/mod.rs`); larger or non-image
/// attachments keep the path-only (staged) form.
const MAX_INLINE_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// Settings the ingress layer needs to download + stage inbound attachments
/// per the channels-core inbound-file contract.
#[derive(Debug, Clone)]
pub struct AttachmentSettings {
    /// When true, file-bearing messages have their first attachment fetched
    /// from the Discord CDN and staged for the router. When false, the raw
    /// `content.attachments` URL metadata is forwarded untouched.
    pub attachment_download: bool,
    /// Refuse to download anything larger than this many bytes; oversized
    /// attachments fall back to a `MessageKind::System` `too_large` row.
    pub max_attachment_bytes: u64,
    /// Per-channel data directory; downloads are STAGED under
    /// `data_dir/staging/<unique>/<filename>` and surfaced on the attachment
    /// as `staged_path`. The router moves the bytes into the resolved
    /// session's `inbox/<msg_id>/<filename>` at route time and rewrites the
    /// attachment `path` to the container-visible `/data/inbox/...`.
    pub data_dir: PathBuf,
}

/// One inbound Discord attachment lifted from `d.attachments[i]`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiscordAttachment {
    /// Public CDN URL to fetch the bytes from.
    url: String,
    /// Sanitized single-path-component filename to stage under.
    filename: String,
    /// Discord-reported `content_type`, if any.
    content_type: Option<String>,
    /// Discord-reported `size` in bytes (authoritative pre-download check).
    reported_size: Option<u64>,
}

/// Convert a `MESSAGE_CREATE` dispatch payload (the `d` field of a gateway
/// frame) into an `InboundEvent`.
///
/// `bot_user_id` is the bot's own Discord user id. When provided, any
/// mention of that id in `d.mentions` sets `is_mention = true`.
pub fn message_create_to_inbound(
    d: &Value,
    bot_user_id: Option<&str>,
) -> Result<InboundEvent, AdapterError> {
    let obj = d
        .as_object()
        .ok_or_else(|| AdapterError::BadRequest("MESSAGE_CREATE.d is not an object".into()))?;

    let id = extract_string(obj, "id")
        .ok_or_else(|| AdapterError::BadRequest("MESSAGE_CREATE.d.id missing".into()))?;

    let channel_id = extract_string(obj, "channel_id")
        .ok_or_else(|| AdapterError::BadRequest("MESSAGE_CREATE.d.channel_id missing".into()))?;

    let guild_id = extract_string(obj, "guild_id");
    let is_group = Some(guild_id.is_some());

    let parent_message_id = obj
        .get("message_reference")
        .and_then(Value::as_object)
        .and_then(|m| m.get("message_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Historical: `message_reference.message_id` was the only way to keep
    // related Discord messages stitched together on the inbound side, so
    // this adapter surfaced it as `thread_id`. Semantically it IS a reply,
    // not a thread (Discord threads are separate channels), so we now also
    // surface it as `reply_to`. The `thread_id` mirror stays to avoid
    // breaking existing routing.
    let thread_id = parent_message_id.clone();
    let reply_to = parent_message_id.as_deref().map(|parent| ReplyTo {
        channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        platform_id: channel_id.clone(),
        thread_id: Some(parent.to_owned()),
        // Parent author not resolved here — must not count as a mention.
        replying_to_self: None,
    });

    let content_text = extract_string(obj, "content").unwrap_or_default();
    let embeds = obj.get("embeds").cloned().unwrap_or(Value::Array(vec![]));
    let attachments = obj
        .get("attachments")
        .cloned()
        .unwrap_or(Value::Array(vec![]));

    let content = json!({
        "text": content_text,
        "embeds": embeds,
        "attachments": attachments,
    });

    let is_mention = Some(bot_mentioned(obj, bot_user_id));

    let sender = obj.get("author").and_then(Value::as_object).map(|a| {
        let identity = a
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default();
        let display_name = a
            .get("global_name")
            .and_then(Value::as_str)
            .or_else(|| a.get("username").and_then(Value::as_str))
            .map(str::to_owned);
        SenderIdentity {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            identity,
            display_name,
        }
    });

    Ok(InboundEvent {
        channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        platform_id: channel_id,
        thread_id,
        message: InboundMessage {
            id,
            kind: MessageKind::Chat,
            content,
            timestamp: Utc::now(),
            is_mention,
            is_group,
        },
        reply_to,
        sender,
    })
}

/// Convert a `MESSAGE_REACTION_ADD` dispatch payload (the `d` field of a
/// gateway frame) into an inbound-reaction event (M19 U7).
///
/// Only unicode emoji reactions (`emoji.id == null`) carry a steering signal;
/// custom-guild-emoji reactions produce no event. `target_seq` is the reacted-
/// to `message_id`, which the runner matches against its `delivered` table to
/// decide the reaction landed on its own message. Returns `Ok(None)` for a
/// custom emoji or a payload missing the routing fields.
pub fn message_reaction_add_to_inbound(d: &Value) -> Result<Option<InboundEvent>, AdapterError> {
    let obj = d.as_object().ok_or_else(|| {
        AdapterError::BadRequest("MESSAGE_REACTION_ADD.d is not an object".into())
    })?;

    let channel_id = extract_string(obj, "channel_id").ok_or_else(|| {
        AdapterError::BadRequest("MESSAGE_REACTION_ADD.d.channel_id missing".into())
    })?;
    let message_id = extract_string(obj, "message_id").ok_or_else(|| {
        AdapterError::BadRequest("MESSAGE_REACTION_ADD.d.message_id missing".into())
    })?;

    let emoji_obj = obj.get("emoji").and_then(Value::as_object);
    // Custom guild emoji carry an `id`; only unicode emoji (id null/absent)
    // have a steerable `name`.
    let is_custom = emoji_obj
        .and_then(|e| e.get("id"))
        .is_some_and(|id| !id.is_null());
    let emoji = emoji_obj
        .and_then(|e| e.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let (Some(emoji), false) = (emoji, is_custom) else {
        return Ok(None);
    };

    let is_group = Some(extract_string(obj, "guild_id").is_some());
    let user_id = extract_string(obj, "user_id");

    Ok(Some(InboundEvent {
        channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        platform_id: channel_id,
        thread_id: None,
        message: InboundMessage {
            id: format!(
                "dcreact:{message_id}:{}:{emoji}",
                user_id.as_deref().unwrap_or("?")
            ),
            kind: MessageKind::Chat,
            content: copperclaw_channels_core::reaction_content(
                &emoji,
                Some(&message_id),
                user_id.as_deref(),
            ),
            timestamp: Utc::now(),
            is_mention: None,
            is_group,
        },
        reply_to: None,
        sender: user_id.map(|id| SenderIdentity {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            identity: id,
            display_name: None,
        }),
    }))
}

/// Convert a `MESSAGE_CREATE` payload into an `InboundEvent`, additionally
/// downloading and staging the message's first attachment when
/// `settings.attachment_download` is set.
///
/// This is the async superset of [`message_create_to_inbound`]: it builds the
/// same base event, then — per the C4b inbound-file work — fetches the first
/// attachment from the Discord CDN, enforces `max_attachment_bytes`, and
/// stages it via [`stage_inbound_file`] so the router can materialize it into
/// the resolved session's inbox. Discord messages can carry several
/// attachments, but the router's session-local materialization acts on a
/// single `content.attachment` (the frozen C3 contract), so — like Telegram —
/// we stage the first one; the raw `content.attachments` array is preserved
/// for metadata.
///
/// System-row taxonomy (mirrors Telegram): an oversized attachment yields
/// `MessageKind::System` with `content.attachment.reason = "too_large"`; a
/// download error yields `reason = "download_failed"`. Neither is ever a
/// silent drop.
pub async fn message_create_to_inbound_downloaded(
    d: &Value,
    bot_user_id: Option<&str>,
    rest: &DiscordRest,
    settings: &AttachmentSettings,
) -> Result<InboundEvent, AdapterError> {
    let mut event = message_create_to_inbound(d, bot_user_id)?;
    if !settings.attachment_download {
        return Ok(event);
    }
    // `message_create_to_inbound` already validated that `d` is an object.
    let Some(obj) = d.as_object() else {
        return Ok(event);
    };
    let Some(att) = pick_attachment(obj) else {
        return Ok(event);
    };
    apply_attachment(&mut event, &att, rest, settings).await;
    Ok(event)
}

/// Pick the first attachment carrying a usable CDN `url` from
/// `d.attachments`.
fn pick_attachment(obj: &Map<String, Value>) -> Option<DiscordAttachment> {
    let arr = obj.get("attachments").and_then(Value::as_array)?;
    arr.iter().find_map(|entry| {
        let e = entry.as_object()?;
        let url = e.get("url").and_then(Value::as_str)?;
        if url.is_empty() {
            return None;
        }
        Some(DiscordAttachment {
            url: url.to_owned(),
            filename: sanitize_filename(
                e.get("filename").and_then(Value::as_str),
                "attachment.bin",
            ),
            content_type: e
                .get("content_type")
                .and_then(Value::as_str)
                .map(str::to_owned),
            reported_size: e.get("size").and_then(Value::as_u64),
        })
    })
}

/// Download, size-check, and stage `att`, mutating `event` in place. On
/// success sets `content.attachment` (with `staged_path`, no `path`) and
/// keeps `kind = Chat`; on too-large / failure switches `kind` to `System`
/// and records the reason.
async fn apply_attachment(
    event: &mut InboundEvent,
    att: &DiscordAttachment,
    rest: &DiscordRest,
    settings: &AttachmentSettings,
) {
    let channel = event.channel_type.as_str().to_owned();
    // Cheap pre-check against the platform-reported size before we fetch.
    if let Some(size) = att.reported_size {
        if size > settings.max_attachment_bytes {
            copperclaw_metrics::inc_inbound_file(&channel, "too_large");
            set_too_large(event, att, settings.max_attachment_bytes, Some(size));
            return;
        }
    }
    let bytes = match rest.download_cdn_file(&att.url).await {
        Ok(b) => b,
        Err(error) => {
            copperclaw_metrics::inc_inbound_file(&channel, "download_failed");
            set_download_failed(event, att, &error);
            return;
        }
    };
    if bytes.len() as u64 > settings.max_attachment_bytes {
        copperclaw_metrics::inc_inbound_file(&channel, "too_large");
        set_too_large(
            event,
            att,
            settings.max_attachment_bytes,
            Some(bytes.len() as u64),
        );
        return;
    }
    let staged = match stage_inbound_file(
        &settings.data_dir.join(STAGING_SUBDIR),
        &att.filename,
        &bytes,
    )
    .await
    {
        Ok(p) => p,
        Err(error) => {
            copperclaw_metrics::inc_inbound_file(&channel, "download_failed");
            set_download_failed(event, att, &error);
            return;
        }
    };
    copperclaw_metrics::inc_inbound_file(&channel, "ok");
    copperclaw_metrics::observe_inbound_file_bytes(&channel, bytes.len() as u64);
    set_staged(event, att, &staged, bytes.len() as u64).await;
}

/// Base `content.attachment` metadata common to every outcome.
fn attachment_meta(att: &DiscordAttachment) -> Map<String, Value> {
    let mut o = Map::new();
    o.insert(
        "kind".to_owned(),
        Value::String("discord.attachment".to_owned()),
    );
    o.insert("filename".to_owned(), Value::String(att.filename.clone()));
    o.insert(
        "mime_type".to_owned(),
        att.content_type.clone().map_or(Value::Null, Value::String),
    );
    o
}

/// Replace `content.attachment` on `event` (leaving `text` / `embeds` /
/// `attachments` intact).
fn set_content_attachment(event: &mut InboundEvent, att_obj: Map<String, Value>) {
    if let Some(content) = event.message.content.as_object_mut() {
        content.insert("attachment".to_owned(), Value::Object(att_obj));
    }
}

/// Success: staged file present. Sets `staged_path` (never `path`) and, for
/// small images, an inline `data_base64` for vision parity.
async fn set_staged(
    event: &mut InboundEvent,
    att: &DiscordAttachment,
    staged_path: &Path,
    actual_size: u64,
) {
    let mut o = attachment_meta(att);
    o.insert(
        STAGED_PATH_KEY.to_owned(),
        Value::String(staged_path.to_string_lossy().into_owned()),
    );
    o.insert("size".to_owned(), Value::from(actual_size));
    inline_image_base64(&mut o, att, staged_path, actual_size).await;
    set_content_attachment(event, o);
    // kind stays Chat.
}

/// Oversized fallback: `MessageKind::System`, `reason = "too_large"`.
fn set_too_large(
    event: &mut InboundEvent,
    att: &DiscordAttachment,
    limit: u64,
    reported: Option<u64>,
) {
    let mut o = attachment_meta(att);
    o.insert("reason".to_owned(), Value::String("too_large".to_owned()));
    o.insert("limit".to_owned(), Value::from(limit));
    if let Some(r) = reported {
        o.insert("reported_size".to_owned(), Value::from(r));
    }
    set_content_attachment(event, o);
    event.message.kind = MessageKind::System;
}

/// Download-failure fallback: `MessageKind::System`,
/// `reason = "download_failed"`, with the error surfaced verbatim.
fn set_download_failed(event: &mut InboundEvent, att: &DiscordAttachment, error: &AdapterError) {
    let mut o = attachment_meta(att);
    o.insert(
        "reason".to_owned(),
        Value::String("download_failed".to_owned()),
    );
    o.insert("error".to_owned(), Value::String(format!("{error}")));
    set_content_attachment(event, o);
    event.message.kind = MessageKind::System;
}

/// If `att` is an image within the size cap, read the staged file and add a
/// `data_base64` field so the runner can lift it into a vision content block.
/// Best-effort: a read failure just leaves the path-only form.
async fn inline_image_base64(
    att_obj: &mut Map<String, Value>,
    att: &DiscordAttachment,
    path: &Path,
    size: u64,
) {
    let is_image = att
        .content_type
        .as_deref()
        .is_some_and(|m| m.starts_with("image/"));
    if !is_image || size > MAX_INLINE_IMAGE_BYTES {
        return;
    }
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            att_obj.insert(
                "data_base64".to_owned(),
                Value::String(copperclaw_types::encode_base64(&bytes)),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "failed to inline inbound discord image");
        }
    }
}

/// Discord interaction `type` values relevant to this adapter.
///
/// `MESSAGE_COMPONENT` (`3`) is what a tapped Block-Kit-equivalent button
/// arrives as — i.e. someone clicked one of the buttons on a card we sent
/// via `deliver_card`. Other types (`PING`/`APPLICATION_COMMAND`/etc.) are
/// ignored by the card-callback path.
pub const INTERACTION_TYPE_MESSAGE_COMPONENT: i64 = 3;

/// Output of [`interaction_create_to_inbound`].
///
/// Carries the synthesised `InboundEvent` plus the
/// (`interaction_id`, `interaction_token`) pair the adapter uses to ACK
/// the interaction via `POST /interactions/{id}/{token}/callback`.
/// Bundling both keeps callers from having to re-parse the payload to fire
/// the ACK.
#[derive(Debug, Clone)]
pub struct InteractionInbound {
    /// Inbound event ready for the host router.
    pub event: InboundEvent,
    /// Discord interaction id (echoed into the ACK URL).
    pub interaction_id: String,
    /// Discord interaction token (echoed into the ACK URL).
    pub interaction_token: String,
}

/// Convert an `INTERACTION_CREATE` dispatch payload (the `d` field of a
/// gateway frame) into an [`InteractionInbound`].
///
/// Returns `Ok(None)` for interaction types we don't currently surface
/// (the adapter still ACKs Discord-side via the returned id/token in the
/// caller, when desired). Returns `Err(AdapterError::BadRequest(_))` only
/// when required envelope fields (`id`, `token`, or `data.custom_id`) are
/// missing — those are gateway-contract violations, not "uninteresting
/// interaction" cases.
pub fn interaction_create_to_inbound(
    d: &Value,
    bot_user_id: Option<&str>,
) -> Result<Option<InteractionInbound>, AdapterError> {
    let obj = d
        .as_object()
        .ok_or_else(|| AdapterError::BadRequest("INTERACTION_CREATE.d is not an object".into()))?;

    let interaction_id = extract_string(obj, "id")
        .ok_or_else(|| AdapterError::BadRequest("INTERACTION_CREATE.d.id missing".into()))?;
    let interaction_token = extract_string(obj, "token")
        .ok_or_else(|| AdapterError::BadRequest("INTERACTION_CREATE.d.token missing".into()))?;
    let kind = obj.get("type").and_then(Value::as_i64).unwrap_or(0);
    if kind != INTERACTION_TYPE_MESSAGE_COMPONENT {
        // Not a card-tap. Caller decides whether to ACK or ignore.
        return Ok(None);
    }

    let custom_id = obj
        .get("data")
        .and_then(Value::as_object)
        .and_then(|d| d.get("custom_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AdapterError::BadRequest(
                "INTERACTION_CREATE.d.data.custom_id missing on MESSAGE_COMPONENT".into(),
            )
        })?
        .to_owned();
    let component_type = obj
        .get("data")
        .and_then(Value::as_object)
        .and_then(|d| d.get("component_type"))
        .and_then(Value::as_i64);

    let channel_id = extract_string(obj, "channel_id").ok_or_else(|| {
        AdapterError::BadRequest("INTERACTION_CREATE.d.channel_id missing".into())
    })?;
    let guild_id = extract_string(obj, "guild_id");
    let is_group = Some(guild_id.is_some());

    // Discord may put the user info under either `member.user` (guild
    // interactions, where the guild member is also surfaced) or `user`
    // (DM interactions). Try both for parity with `MESSAGE_CREATE`.
    let user_obj = obj
        .get("member")
        .and_then(Value::as_object)
        .and_then(|m| m.get("user"))
        .and_then(Value::as_object)
        .or_else(|| obj.get("user").and_then(Value::as_object));
    let sender = user_obj.map(|u| {
        let identity = u
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_default();
        let display_name = u
            .get("global_name")
            .and_then(Value::as_str)
            .or_else(|| u.get("username").and_then(Value::as_str))
            .map(str::to_owned);
        SenderIdentity {
            channel_type: ChannelType::new(CHANNEL_TYPE_STR),
            identity,
            display_name,
        }
    });

    let original_message_id = obj
        .get("message")
        .and_then(Value::as_object)
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    // The synthesised inbound text IS the button's value (custom_id ==
    // canonical CardButton::value). Callback metadata rides in a sub-object
    // so agents that care can branch on it without parsing the text.
    let mut callback = json!({
        "value": custom_id.clone(),
        "interaction_id": interaction_id.clone(),
    });
    if let Some(ct) = component_type {
        callback["component_type"] = Value::from(ct);
    }
    if let Some(mid) = &original_message_id {
        callback["original_message_id"] = Value::String(mid.clone());
    }
    let content = json!({
        "text": custom_id.clone(),
        "callback": callback,
    });

    // `is_mention` is conceptually-undefined for a button tap — the user
    // didn't @ anyone, they clicked. Mirror Telegram's choice (which leaves
    // it None on callbacks) so downstream filters that gate on @-mentions
    // don't accidentally fire for taps.
    let _ = bot_user_id; // Reserved for future use; mirrors message_create_to_inbound signature.

    let event = InboundEvent {
        channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        platform_id: channel_id,
        // Discord delivers interactions for messages inside threads on the
        // thread's own channel id, so thread_id is None for the same reason
        // it's None on a top-level MESSAGE_CREATE.
        thread_id: None,
        message: InboundMessage {
            // Use the interaction id as the platform-side message id so the
            // router's dedupe sees a unique row per tap. Matches Telegram's
            // strategy of reusing callback_query.id.
            id: interaction_id.clone(),
            kind: MessageKind::Chat,
            content,
            timestamp: Utc::now(),
            is_mention: None,
            is_group,
        },
        reply_to: None,
        sender,
    };

    Ok(Some(InteractionInbound {
        event,
        interaction_id,
        interaction_token,
    }))
}

/// True when `bot_user_id` is one of the entries in `d.mentions[*].id`.
pub fn bot_mentioned(obj: &Map<String, Value>, bot_user_id: Option<&str>) -> bool {
    let Some(bot_id) = bot_user_id else {
        return false;
    };
    obj.get("mentions")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter().any(|m| {
                m.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s == bot_id)
            })
        })
}

fn extract_string(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---- C4b: inbound-attachment download + staging ----

    fn attach_settings(dir: &Path) -> AttachmentSettings {
        AttachmentSettings {
            attachment_download: true,
            max_attachment_bytes: crate::config::DEFAULT_MAX_ATTACHMENT_BYTES,
            data_dir: dir.to_path_buf(),
        }
    }

    fn rest_for(server: &MockServer) -> DiscordRest {
        DiscordRest::new(Client::new(), "tok", server.uri())
    }

    /// Build a `MESSAGE_CREATE` payload with a single attachment whose `url`
    /// points at `server`'s `/cdn/<name>` path.
    fn payload_with_attachment(
        server: &MockServer,
        name: &str,
        content_type: Option<&str>,
        size: Option<u64>,
    ) -> Value {
        let mut att = json!({
            "id": "att-1",
            "filename": name,
            "url": format!("{}/cdn/{name}", server.uri()),
        });
        if let Some(ct) = content_type {
            att["content_type"] = json!(ct);
        }
        if let Some(s) = size {
            att["size"] = json!(s);
        }
        json!({
            "id": "m-att",
            "channel_id": "c1",
            "guild_id": "g1",
            "content": "here is the file",
            "author": { "id": "u1", "username": "alice", "global_name": "Alice" },
            "mentions": [],
            "embeds": [],
            "attachments": [att]
        })
    }

    async fn mount_cdn(server: &MockServer, name: &str, bytes: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(format!("/cdn/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn attachment_download_stages_and_sets_staged_path_not_path() {
        let server = MockServer::start().await;
        mount_cdn(&server, "spec.csv", b"id,qty\n1,2\n".to_vec()).await;
        let dir = TempDir::new().unwrap();
        let payload = payload_with_attachment(&server, "spec.csv", Some("text/csv"), Some(11));
        let evt = message_create_to_inbound_downloaded(
            &payload,
            None,
            &rest_for(&server),
            &attach_settings(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["kind"], "discord.attachment");
        assert_eq!(att["filename"], "spec.csv");
        assert_eq!(att["mime_type"], "text/csv");
        assert_eq!(att["size"], 11);
        // Contract: adapters set staged_path, never path.
        assert!(
            att.get("path").is_none(),
            "adapters must not set attachment.path: {att}"
        );
        let staged = att[STAGED_PATH_KEY].as_str().unwrap();
        let staged_path = Path::new(staged);
        assert!(
            staged_path.starts_with(dir.path().join(STAGING_SUBDIR)),
            "staged under <data_dir>/staging: {staged}"
        );
        assert_eq!(std::fs::read(staged_path).unwrap(), b"id,qty\n1,2\n");
        // The raw attachments array is preserved for metadata.
        assert_eq!(
            evt.message.content["attachments"][0]["filename"],
            "spec.csv"
        );
    }

    #[tokio::test]
    async fn attachment_download_disabled_leaves_urls_untouched() {
        let server = MockServer::start().await;
        // No CDN mock — download must never be attempted.
        let dir = TempDir::new().unwrap();
        let mut settings = attach_settings(dir.path());
        settings.attachment_download = false;
        let payload = payload_with_attachment(&server, "spec.csv", Some("text/csv"), Some(11));
        let evt =
            message_create_to_inbound_downloaded(&payload, None, &rest_for(&server), &settings)
                .await
                .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert!(evt.message.content.get("attachment").is_none());
        assert_eq!(
            evt.message.content["attachments"][0]["filename"],
            "spec.csv"
        );
    }

    #[tokio::test]
    async fn attachment_oversized_by_reported_size_yields_too_large_system_row() {
        let server = MockServer::start().await;
        // No CDN mock — we must reject before fetching.
        let dir = TempDir::new().unwrap();
        let mut settings = attach_settings(dir.path());
        settings.max_attachment_bytes = 4;
        let payload =
            payload_with_attachment(&server, "big.bin", Some("application/zip"), Some(999));
        let evt =
            message_create_to_inbound_downloaded(&payload, None, &rest_for(&server), &settings)
                .await
                .unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["reason"], "too_large");
        assert_eq!(att["limit"], 4);
        assert_eq!(att["reported_size"], 999);
    }

    #[tokio::test]
    async fn attachment_oversized_after_body_read_yields_too_large() {
        let server = MockServer::start().await;
        mount_cdn(&server, "big.bin", vec![0u8; 32]).await;
        let dir = TempDir::new().unwrap();
        let mut settings = attach_settings(dir.path());
        settings.max_attachment_bytes = 16;
        // No reported size, so the cap is only enforceable after download.
        let payload = payload_with_attachment(&server, "big.bin", None, None);
        let evt =
            message_create_to_inbound_downloaded(&payload, None, &rest_for(&server), &settings)
                .await
                .unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["attachment"]["reason"], "too_large");
    }

    #[tokio::test]
    async fn attachment_download_failure_yields_download_failed_system_row() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cdn/spec.csv"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let dir = TempDir::new().unwrap();
        let payload = payload_with_attachment(&server, "spec.csv", Some("text/csv"), Some(11));
        let evt = message_create_to_inbound_downloaded(
            &payload,
            None,
            &rest_for(&server),
            &attach_settings(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["reason"], "download_failed");
        assert!(att["error"].as_str().unwrap().contains("503"));
    }

    #[tokio::test]
    async fn small_image_attachment_inlines_data_base64() {
        let server = MockServer::start().await;
        mount_cdn(&server, "pic.png", b"\x89PNGfake".to_vec()).await;
        let dir = TempDir::new().unwrap();
        let payload = payload_with_attachment(&server, "pic.png", Some("image/png"), Some(8));
        let evt = message_create_to_inbound_downloaded(
            &payload,
            None,
            &rest_for(&server),
            &attach_settings(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        let b64 = att["data_base64"].as_str().expect("data_base64 present");
        assert_eq!(
            b64,
            copperclaw_types::encode_base64(b"\x89PNGfake"),
            "inline base64 must be the raw image bytes"
        );
    }

    #[tokio::test]
    async fn non_image_attachment_has_no_data_base64() {
        let server = MockServer::start().await;
        mount_cdn(&server, "spec.csv", b"id,qty\n1,2\n".to_vec()).await;
        let dir = TempDir::new().unwrap();
        let payload = payload_with_attachment(&server, "spec.csv", Some("text/csv"), Some(11));
        let evt = message_create_to_inbound_downloaded(
            &payload,
            None,
            &rest_for(&server),
            &attach_settings(dir.path()),
        )
        .await
        .unwrap();
        assert!(
            evt.message.content["attachment"]
                .get("data_base64")
                .is_none()
        );
    }

    #[tokio::test]
    async fn message_without_attachments_passes_through() {
        let server = MockServer::start().await;
        let dir = TempDir::new().unwrap();
        let evt = message_create_to_inbound_downloaded(
            &sample_payload(),
            None,
            &rest_for(&server),
            &attach_settings(dir.path()),
        )
        .await
        .unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert!(evt.message.content.get("attachment").is_none());
    }

    #[tokio::test]
    async fn attachment_hostile_filename_is_sanitized_when_staged() {
        let server = MockServer::start().await;
        mount_cdn(&server, "evil", b"x".to_vec()).await;
        let dir = TempDir::new().unwrap();
        // The CDN url path is /cdn/evil, but the reported filename is hostile.
        let mut att = json!({
            "id": "att-1",
            "filename": "../../etc/passwd",
            "url": format!("{}/cdn/evil", server.uri()),
            "size": 1,
        });
        att["content_type"] = json!("application/octet-stream");
        let payload = json!({
            "id": "m-att", "channel_id": "c1", "guild_id": "g1",
            "content": "", "author": { "id": "u1", "username": "a" },
            "mentions": [], "embeds": [], "attachments": [att]
        });
        let evt = message_create_to_inbound_downloaded(
            &payload,
            None,
            &rest_for(&server),
            &attach_settings(dir.path()),
        )
        .await
        .unwrap();
        let att = &evt.message.content["attachment"];
        assert_eq!(att["filename"], "etc_passwd");
        let staged = Path::new(att[STAGED_PATH_KEY].as_str().unwrap());
        assert!(staged.starts_with(dir.path().join(STAGING_SUBDIR)));
        assert_eq!(staged.file_name().unwrap().to_str().unwrap(), "etc_passwd");
    }

    fn sample_payload() -> Value {
        json!({
            "id": "100",
            "channel_id": "c1",
            "guild_id": "g1",
            "content": "hello there",
            "author": { "id": "u1", "username": "alice", "global_name": "Alice" },
            "mentions": [],
            "embeds": [],
            "attachments": []
        })
    }

    fn reaction_payload(emoji_name: &str, emoji_id: Option<&str>) -> Value {
        json!({
            "user_id": "u1",
            "channel_id": "c1",
            "message_id": "m9",
            "guild_id": "g1",
            "emoji": { "id": emoji_id, "name": emoji_name },
        })
    }

    #[test]
    fn reaction_add_maps_to_inbound_reaction_event() {
        // M19 U7: a 👍 on message m9 becomes a normalized reaction inbound row.
        let evt = message_reaction_add_to_inbound(&reaction_payload("\u{1F44D}", None))
            .unwrap()
            .expect("unicode reaction routes");
        assert_eq!(evt.channel_type.as_str(), "discord");
        assert_eq!(evt.platform_id, "c1");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.is_group, Some(true));
        let r = copperclaw_channels_core::parse_reaction(&evt.message.content).unwrap();
        assert_eq!(r.emoji, "\u{1F44D}");
        assert_eq!(r.target_seq.as_deref(), Some("m9"));
        assert_eq!(r.actor.as_deref(), Some("u1"));
        assert_eq!(evt.sender.unwrap().identity, "u1");
    }

    #[test]
    fn custom_emoji_reaction_produces_no_event() {
        // A custom guild emoji carries an id → not a steerable unicode emoji.
        let out =
            message_reaction_add_to_inbound(&reaction_payload("partyblob", Some("123456789")))
                .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn reaction_missing_message_id_is_bad_request() {
        let mut p = reaction_payload("\u{1F44D}", None);
        p.as_object_mut().unwrap().remove("message_id");
        assert!(message_reaction_add_to_inbound(&p).is_err());
    }

    #[test]
    fn maps_minimum_fields() {
        let evt = message_create_to_inbound(&sample_payload(), None).unwrap();
        assert_eq!(evt.channel_type.as_str(), "discord");
        assert_eq!(evt.platform_id, "c1");
        assert_eq!(evt.message.id, "100");
        assert_eq!(evt.message.content["text"], "hello there");
        assert_eq!(evt.message.is_group, Some(true));
        assert!(evt.thread_id.is_none());
        let sender = evt.sender.unwrap();
        assert_eq!(sender.identity, "u1");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn dm_message_marks_is_group_false() {
        let mut payload = sample_payload();
        payload.as_object_mut().unwrap().remove("guild_id");
        let evt = message_create_to_inbound(&payload, None).unwrap();
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[test]
    fn bot_mention_sets_is_mention_true() {
        let mut payload = sample_payload();
        payload["mentions"] = json!([{"id": "bot-id"}, {"id": "other"}]);
        let evt = message_create_to_inbound(&payload, Some("bot-id")).unwrap();
        assert_eq!(evt.message.is_mention, Some(true));
    }

    #[test]
    fn bot_mention_false_when_not_present() {
        let mut payload = sample_payload();
        payload["mentions"] = json!([{"id": "other"}]);
        let evt = message_create_to_inbound(&payload, Some("bot-id")).unwrap();
        assert_eq!(evt.message.is_mention, Some(false));
    }

    #[test]
    fn bot_mention_false_when_bot_id_missing() {
        let mut payload = sample_payload();
        payload["mentions"] = json!([{"id": "bot-id"}]);
        let evt = message_create_to_inbound(&payload, None).unwrap();
        assert_eq!(evt.message.is_mention, Some(false));
    }

    #[test]
    fn message_reference_becomes_thread_id() {
        let mut payload = sample_payload();
        payload["message_reference"] = json!({ "message_id": "parent-99" });
        let evt = message_create_to_inbound(&payload, None).unwrap();
        assert_eq!(evt.thread_id.as_deref(), Some("parent-99"));
    }

    #[test]
    fn missing_channel_id_errors() {
        let mut payload = sample_payload();
        payload.as_object_mut().unwrap().remove("channel_id");
        let err = message_create_to_inbound(&payload, None).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn missing_id_errors() {
        let mut payload = sample_payload();
        payload.as_object_mut().unwrap().remove("id");
        let err = message_create_to_inbound(&payload, None).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn non_object_payload_errors() {
        let err = message_create_to_inbound(&json!(7), None).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn embeds_and_attachments_preserved() {
        let mut payload = sample_payload();
        payload["embeds"] = json!([{"title": "t"}]);
        payload["attachments"] = json!([{"filename": "a.png"}]);
        let evt = message_create_to_inbound(&payload, None).unwrap();
        assert_eq!(evt.message.content["embeds"][0]["title"], "t");
        assert_eq!(evt.message.content["attachments"][0]["filename"], "a.png");
    }

    #[test]
    fn empty_content_yields_empty_text() {
        let mut payload = sample_payload();
        payload["content"] = json!("");
        let evt = message_create_to_inbound(&payload, None).unwrap();
        assert_eq!(evt.message.content["text"], "");
    }

    #[test]
    fn author_falls_back_to_username() {
        let mut payload = sample_payload();
        payload["author"]
            .as_object_mut()
            .unwrap()
            .remove("global_name");
        let evt = message_create_to_inbound(&payload, None).unwrap();
        assert_eq!(evt.sender.unwrap().display_name.as_deref(), Some("alice"));
    }

    #[test]
    fn author_missing_yields_no_sender() {
        let mut payload = sample_payload();
        payload.as_object_mut().unwrap().remove("author");
        let evt = message_create_to_inbound(&payload, None).unwrap();
        assert!(evt.sender.is_none());
    }

    #[test]
    fn message_reference_populates_reply_to() {
        let mut payload = sample_payload();
        payload["message_reference"] = json!({ "message_id": "parent-99" });
        let evt = message_create_to_inbound(&payload, None).unwrap();
        let rt = evt
            .reply_to
            .expect("reply_to populated from message_reference");
        assert_eq!(rt.channel_type.as_str(), "discord");
        assert_eq!(rt.platform_id, "c1");
        assert_eq!(rt.thread_id.as_deref(), Some("parent-99"));
    }

    #[test]
    fn missing_message_reference_leaves_reply_to_none() {
        // sample_payload has no message_reference at all.
        let evt = message_create_to_inbound(&sample_payload(), None).unwrap();
        assert!(evt.reply_to.is_none());
    }

    fn sample_interaction() -> Value {
        json!({
            "id": "int-123",
            "token": "tok-abc",
            "type": 3,
            "channel_id": "c1",
            "guild_id": "g1",
            "member": {
                "user": { "id": "u1", "username": "alice", "global_name": "Alice" }
            },
            "data": {
                "custom_id": "deploy:yes",
                "component_type": 2
            },
            "message": { "id": "card-msg-77" }
        })
    }

    #[test]
    fn interaction_message_component_synthesises_chat_event() {
        let out = interaction_create_to_inbound(&sample_interaction(), Some("bot-id"))
            .unwrap()
            .expect("Some(InteractionInbound)");
        assert_eq!(out.interaction_id, "int-123");
        assert_eq!(out.interaction_token, "tok-abc");
        let evt = out.event;
        assert_eq!(evt.channel_type.as_str(), "discord");
        assert_eq!(evt.platform_id, "c1");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "deploy:yes");
        assert_eq!(evt.message.content["callback"]["value"], "deploy:yes");
        assert_eq!(
            evt.message.content["callback"]["original_message_id"],
            "card-msg-77"
        );
        assert_eq!(evt.message.is_group, Some(true));
        // Button taps don't count as @-mentions — leave it None so mention
        // gates don't accidentally fire.
        assert!(evt.message.is_mention.is_none());
        let sender = evt.sender.unwrap();
        assert_eq!(sender.identity, "u1");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn interaction_dm_uses_user_field_and_marks_not_group() {
        let mut payload = sample_interaction();
        payload.as_object_mut().unwrap().remove("guild_id");
        payload.as_object_mut().unwrap().remove("member");
        payload["user"] = json!({"id": "u-dm", "username": "bob"});
        let out = interaction_create_to_inbound(&payload, None)
            .unwrap()
            .unwrap();
        assert_eq!(out.event.message.is_group, Some(false));
        let sender = out.event.sender.unwrap();
        assert_eq!(sender.identity, "u-dm");
        assert_eq!(sender.display_name.as_deref(), Some("bob"));
    }

    #[test]
    fn interaction_non_component_type_returns_none() {
        let mut payload = sample_interaction();
        payload["type"] = json!(1); // PING
        let out = interaction_create_to_inbound(&payload, None).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn interaction_missing_custom_id_is_bad_request() {
        let mut payload = sample_interaction();
        payload["data"].as_object_mut().unwrap().remove("custom_id");
        let err = interaction_create_to_inbound(&payload, None).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn interaction_missing_id_or_token_is_bad_request() {
        let mut payload = sample_interaction();
        payload.as_object_mut().unwrap().remove("id");
        assert!(matches!(
            interaction_create_to_inbound(&payload, None),
            Err(AdapterError::BadRequest(_))
        ));

        let mut payload = sample_interaction();
        payload.as_object_mut().unwrap().remove("token");
        assert!(matches!(
            interaction_create_to_inbound(&payload, None),
            Err(AdapterError::BadRequest(_))
        ));
    }

    #[test]
    fn interaction_non_object_payload_is_bad_request() {
        let err = interaction_create_to_inbound(&json!(7), None).unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn bot_mentioned_handles_missing_mentions_array() {
        let mut payload = sample_payload();
        payload.as_object_mut().unwrap().remove("mentions");
        let obj = payload.as_object().unwrap();
        assert!(!bot_mentioned(obj, Some("bot-id")));
    }
}
