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

use chrono::Utc;
use copperclaw_channels_core::AdapterError;
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, ReplyTo, SenderIdentity,
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
