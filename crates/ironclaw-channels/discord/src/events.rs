//! Mapping from Discord `MESSAGE_CREATE` dispatch payloads to
//! `ironclaw_types::InboundEvent`.
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

use chrono::Utc;
use ironclaw_channels_core::AdapterError;
use ironclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity,
};
use serde_json::{Map, Value, json};

/// Channel-type string registered by this crate (`"discord"`).
pub const CHANNEL_TYPE_STR: &str = "discord";

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

    let channel_id = extract_string(obj, "channel_id").ok_or_else(|| {
        AdapterError::BadRequest("MESSAGE_CREATE.d.channel_id missing".into())
    })?;

    let guild_id = extract_string(obj, "guild_id");
    let is_group = Some(guild_id.is_some());

    let thread_id = obj
        .get("message_reference")
        .and_then(Value::as_object)
        .and_then(|m| m.get("message_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

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
        reply_to: None,
        sender,
    })
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
    fn bot_mentioned_handles_missing_mentions_array() {
        let mut payload = sample_payload();
        payload.as_object_mut().unwrap().remove("mentions");
        let obj = payload.as_object().unwrap();
        assert!(!bot_mentioned(obj, Some("bot-id")));
    }
}
