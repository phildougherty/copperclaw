//! Pure mapping from `dm_events` JSON to [`InboundEvent`]s.
//!
//! Kept free of HTTP so it can be unit-tested with captured fixtures.

use chrono::{DateTime, Utc};
use ironclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity,
};
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::factory::CHANNEL_TYPE_STR;

/// Convert a `dm_events` response page into a flat list of [`InboundEvent`]s.
///
/// `bot_user_id` is the bot's numeric user id; events whose `sender_id`
/// matches are filtered out to prevent feedback loops.
pub fn page_to_events(page: &Value, bot_user_id: &str) -> Vec<InboundEvent> {
    let Some(events) = page.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let user_lookup = build_user_lookup(page);
    let mut out = Vec::with_capacity(events.len());
    for event in events {
        if let Some(evt) = event_to_inbound(event, bot_user_id, &user_lookup) {
            out.push(evt);
        }
    }
    out
}

/// Build a single [`InboundEvent`] from a `dm_events` element.
///
/// Returns `None` for:
/// - non-`MessageCreate` events,
/// - events authored by the bot (`sender_id == bot_user_id`),
/// - events lacking a `dm_conversation_id` (cannot route a reply).
pub fn event_to_inbound<S: std::hash::BuildHasher>(
    event: &Value,
    bot_user_id: &str,
    user_lookup: &HashMap<String, UserInfo, S>,
) -> Option<InboundEvent> {
    let event_type = event
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("MessageCreate");
    if event_type != "MessageCreate" {
        return None;
    }
    let sender_id = event.get("sender_id").and_then(Value::as_str)?;
    if sender_id == bot_user_id {
        return None;
    }
    let dm_conversation_id = event
        .get("dm_conversation_id")
        .and_then(Value::as_str)?
        .to_owned();
    let event_id = event
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let timestamp = event
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or_else(Utc::now);

    let display_name = user_lookup.get(sender_id).and_then(UserInfo::display_name);
    let channel_type = ChannelType::new(CHANNEL_TYPE_STR);

    Some(InboundEvent {
        channel_type: channel_type.clone(),
        platform_id: format!("conversation:{dm_conversation_id}"),
        thread_id: None,
        message: InboundMessage {
            id: event_id,
            kind: MessageKind::Chat,
            content: json!({ "text": text }),
            timestamp,
            is_mention: None,
            // v1: we do not have a reliable group/dm discriminator on this
            // endpoint; leave as `None` and let downstream wiring decide.
            is_group: None,
        },
        reply_to: None,
        sender: Some(SenderIdentity {
            channel_type,
            identity: sender_id.to_owned(),
            display_name,
        }),
    })
}

/// Build a `{ user_id -> UserInfo }` map from the page's `includes.users`
/// expansion. Returns an empty map when the expansion is absent.
pub fn build_user_lookup(page: &Value) -> HashMap<String, UserInfo> {
    let mut map = HashMap::new();
    let Some(users) = page
        .get("includes")
        .and_then(|v| v.get("users"))
        .and_then(Value::as_array)
    else {
        return map;
    };
    for user in users {
        let Some(id) = user.get("id").and_then(Value::as_str) else {
            continue;
        };
        let name = user
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let username = user
            .get("username")
            .and_then(Value::as_str)
            .map(str::to_owned);
        map.insert(id.to_owned(), UserInfo { name, username });
    }
    map
}

/// Read `meta.newest_id` from a `dm_events` page; this is the value we
/// persist as the next call's `since_id`.
pub fn newest_id_of(page: &Value) -> Option<&str> {
    page.get("meta")
        .and_then(|m| m.get("newest_id"))
        .and_then(Value::as_str)
}

/// Expanded user metadata returned by `includes.users`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInfo {
    /// Display name (`name` field on the v2 user object).
    pub name: Option<String>,
    /// Handle (`username` field).
    pub username: Option<String>,
}

impl UserInfo {
    /// Pick the best human-readable display name. Prefers `name`, falls
    /// back to `@username`.
    pub fn display_name(&self) -> Option<String> {
        if let Some(name) = self.name.clone() {
            return Some(name);
        }
        self.username.as_ref().map(|u| format!("@{u}"))
    }
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message_event(sender: &str, conv: &str, text: &str, id: &str) -> Value {
        json!({
            "id": id,
            "event_type": "MessageCreate",
            "text": text,
            "sender_id": sender,
            "dm_conversation_id": conv,
            "created_at": "2024-01-02T03:04:05Z"
        })
    }

    fn page_with_events(events: &[Value]) -> Value {
        json!({
            "data": events,
            "meta": { "newest_id": "evt-newest" }
        })
    }

    #[test]
    fn message_event_becomes_chat_event() {
        let page = page_with_events(&[message_event("u1", "c1", "hi", "e1")]);
        let evts = page_to_events(&page, "bot");
        assert_eq!(evts.len(), 1);
        let evt = &evts[0];
        assert_eq!(evt.channel_type.as_str(), "x");
        assert_eq!(evt.platform_id, "conversation:c1");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.id, "e1");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hi");
        assert_eq!(evt.message.is_mention, None);
        assert_eq!(evt.message.is_group, None);
        let sender = evt.sender.as_ref().unwrap();
        assert_eq!(sender.identity, "u1");
        assert_eq!(sender.display_name, None);
    }

    #[test]
    fn bot_own_event_is_filtered() {
        let page = page_with_events(&[message_event("bot", "c1", "self", "e1")]);
        let evts = page_to_events(&page, "bot");
        assert!(evts.is_empty());
    }

    #[test]
    fn event_missing_sender_is_skipped() {
        let event = json!({
            "id": "x",
            "event_type": "MessageCreate",
            "text": "hi",
            "dm_conversation_id": "c1"
        });
        let page = page_with_events(&[event]);
        let evts = page_to_events(&page, "bot");
        assert!(evts.is_empty());
    }

    #[test]
    fn event_missing_conversation_is_skipped() {
        let event = json!({
            "id": "x",
            "event_type": "MessageCreate",
            "text": "hi",
            "sender_id": "u1"
        });
        let page = page_with_events(&[event]);
        let evts = page_to_events(&page, "bot");
        assert!(evts.is_empty());
    }

    #[test]
    fn non_message_event_is_skipped() {
        let event = json!({
            "id": "x",
            "event_type": "ParticipantsJoin",
            "sender_id": "u1",
            "dm_conversation_id": "c1"
        });
        let page = page_with_events(&[event]);
        let evts = page_to_events(&page, "bot");
        assert!(evts.is_empty());
    }

    #[test]
    fn missing_event_type_assumed_message_create() {
        let event = json!({
            "id": "x",
            "text": "hi",
            "sender_id": "u1",
            "dm_conversation_id": "c1"
        });
        let page = page_with_events(&[event]);
        let evts = page_to_events(&page, "bot");
        assert_eq!(evts.len(), 1);
    }

    #[test]
    fn missing_text_becomes_empty_string() {
        let event = json!({
            "id": "x",
            "event_type": "MessageCreate",
            "sender_id": "u1",
            "dm_conversation_id": "c1"
        });
        let page = page_with_events(&[event]);
        let evts = page_to_events(&page, "bot");
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].message.content["text"], "");
    }

    #[test]
    fn missing_data_yields_no_events() {
        let evts = page_to_events(&json!({}), "bot");
        assert!(evts.is_empty());
    }

    #[test]
    fn data_not_array_yields_no_events() {
        let page = json!({ "data": "junk" });
        let evts = page_to_events(&page, "bot");
        assert!(evts.is_empty());
    }

    #[test]
    fn expansions_populate_sender_display_name_from_name() {
        let page = json!({
            "data": [message_event("u1", "c1", "hi", "e1")],
            "includes": {
                "users": [
                    { "id": "u1", "name": "Alice", "username": "alice42" }
                ]
            }
        });
        let evts = page_to_events(&page, "bot");
        let sender = evts[0].sender.as_ref().unwrap();
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn expansions_fall_back_to_handle_when_name_absent() {
        let page = json!({
            "data": [message_event("u1", "c1", "hi", "e1")],
            "includes": {
                "users": [
                    { "id": "u1", "username": "alice42" }
                ]
            }
        });
        let evts = page_to_events(&page, "bot");
        let sender = evts[0].sender.as_ref().unwrap();
        assert_eq!(sender.display_name.as_deref(), Some("@alice42"));
    }

    #[test]
    fn build_user_lookup_skips_users_without_id() {
        let page = json!({
            "includes": {
                "users": [
                    { "name": "Anon" },
                    { "id": "u2", "username": "two" }
                ]
            }
        });
        let map = build_user_lookup(&page);
        assert_eq!(map.len(), 1);
        assert_eq!(map["u2"].username.as_deref(), Some("two"));
    }

    #[test]
    fn build_user_lookup_no_includes_is_empty() {
        assert!(build_user_lookup(&json!({})).is_empty());
        assert!(build_user_lookup(&json!({"includes": {}})).is_empty());
        assert!(build_user_lookup(&json!({"includes": {"users": "junk"}})).is_empty());
    }

    #[test]
    fn newest_id_present() {
        assert_eq!(newest_id_of(&json!({"meta":{"newest_id":"x"}})), Some("x"));
    }

    #[test]
    fn newest_id_missing() {
        assert!(newest_id_of(&json!({})).is_none());
        assert!(newest_id_of(&json!({"meta": {}})).is_none());
    }

    #[test]
    fn timestamp_parses_rfc3339() {
        let page = page_with_events(&[message_event("u1", "c1", "hi", "e1")]);
        let evts = page_to_events(&page, "bot");
        let ts = evts[0].message.timestamp;
        assert_eq!(ts.timestamp(), 1_704_164_645);
    }

    #[test]
    fn timestamp_falls_back_to_now_when_malformed() {
        let event = json!({
            "id": "x",
            "event_type": "MessageCreate",
            "text": "hi",
            "sender_id": "u1",
            "dm_conversation_id": "c1",
            "created_at": "not-a-date"
        });
        let page = page_with_events(&[event]);
        let evts = page_to_events(&page, "bot");
        assert_eq!(evts.len(), 1);
        // Just confirm it's a sane recent timestamp.
        let now = Utc::now().timestamp();
        let ts = evts[0].message.timestamp.timestamp();
        assert!((ts - now).abs() < 5);
    }

    #[test]
    fn missing_event_id_becomes_empty_string() {
        let event = json!({
            "event_type": "MessageCreate",
            "text": "hi",
            "sender_id": "u1",
            "dm_conversation_id": "c1"
        });
        let page = page_with_events(&[event]);
        let evts = page_to_events(&page, "bot");
        assert_eq!(evts[0].message.id, "");
    }

    #[test]
    fn user_info_display_name_prefers_name_over_username() {
        let info = UserInfo {
            name: Some("Alice".into()),
            username: Some("alice42".into()),
        };
        assert_eq!(info.display_name().as_deref(), Some("Alice"));
    }

    #[test]
    fn user_info_display_name_uses_handle_when_only_username() {
        let info = UserInfo {
            name: None,
            username: Some("alice42".into()),
        };
        assert_eq!(info.display_name().as_deref(), Some("@alice42"));
    }

    #[test]
    fn user_info_display_name_returns_none_when_both_absent() {
        assert!(UserInfo::default().display_name().is_none());
    }

    #[test]
    fn parse_rfc3339_valid() {
        assert!(parse_rfc3339("2024-01-02T03:04:05Z").is_some());
        assert!(parse_rfc3339("2024-01-02T03:04:05+00:00").is_some());
    }

    #[test]
    fn parse_rfc3339_invalid() {
        assert!(parse_rfc3339("nope").is_none());
        assert!(parse_rfc3339("").is_none());
    }

    #[test]
    fn multiple_events_are_emitted_in_order() {
        let page = page_with_events(&[
            message_event("u1", "c1", "first", "e1"),
            message_event("u2", "c2", "second", "e2"),
        ]);
        let evts = page_to_events(&page, "bot");
        assert_eq!(evts.len(), 2);
        assert_eq!(evts[0].message.content["text"], "first");
        assert_eq!(evts[1].message.content["text"], "second");
    }
}
