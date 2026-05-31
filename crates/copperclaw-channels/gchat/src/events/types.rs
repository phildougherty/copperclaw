//! Google Chat HTTP push event payload types.
//!
//! Google Chat POSTs events to the app's configured URL. We deserialize only
//! the fields the adapter uses; everything else is captured into the
//! catch-all `Other` variant or `serde_json::Value` fields.

use serde::Deserialize;
use serde_json::Value;

/// Outer envelope Google Chat POSTs to the webhook.
///
/// Google's actual schema is `{"type": "MESSAGE", "eventTime": "...",
/// "space": {...}, "user": {...}, "message": {...}}`. We treat the `type`
/// field as the tag and surface a `GchatEvent` for the kinds the adapter
/// cares about.
#[derive(Debug, Clone, Deserialize)]
pub struct GchatEventEnvelope {
    /// Event kind dispatch.
    #[serde(flatten)]
    pub event: GchatEvent,
    /// Space the event was raised in. Always present in Google's payloads
    /// for the events we care about.
    pub space: GchatSpace,
    /// The user who triggered the event. Optional only because not every
    /// event type carries one in the wire schema.
    #[serde(default)]
    pub user: Option<GchatUser>,
    /// The message payload (present for `MESSAGE` and `CARD_CLICKED`).
    #[serde(default)]
    pub message: Option<GchatMessage>,
}

/// Concrete event kind, distinguished by the `type` field.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum GchatEvent {
    /// A user sent a message in the space.
    #[serde(rename = "MESSAGE")]
    Message,
    /// A user clicked a card action button.
    #[serde(rename = "CARD_CLICKED")]
    CardClicked,
    /// The bot was added to a space.
    #[serde(rename = "ADDED_TO_SPACE")]
    AddedToSpace,
    /// The bot was removed from a space.
    #[serde(rename = "REMOVED_TO_SPACE", alias = "REMOVED_FROM_SPACE")]
    RemovedFromSpace,
    /// Any other event type — acknowledged but not surfaced.
    #[serde(other)]
    Other,
}

/// Google Chat space (room or DM) where the event was raised.
#[derive(Debug, Clone, Deserialize)]
pub struct GchatSpace {
    /// Full resource path, e.g. `spaces/AAQAtjsc`.
    pub name: String,
    /// One of `ROOM`, `DM`, etc.
    #[serde(default)]
    #[serde(rename = "type")]
    pub space_type: Option<String>,
}

impl GchatSpace {
    /// Whether the space is a multi-user room (vs a DM).
    #[must_use]
    pub fn is_room(&self) -> bool {
        self.space_type.as_deref() == Some("ROOM")
    }
}

/// Google Chat user that produced the event.
#[derive(Debug, Clone, Deserialize)]
pub struct GchatUser {
    /// Full resource path, e.g. `users/12345`.
    pub name: String,
    /// Best-effort display name; may be missing.
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
    /// One of `HUMAN`, `BOT`.
    #[serde(default)]
    #[serde(rename = "type")]
    pub user_type: Option<String>,
}

impl GchatUser {
    /// Whether the user is a bot (used to filter out our own messages to
    /// avoid loops).
    #[must_use]
    pub fn is_bot(&self) -> bool {
        self.user_type.as_deref() == Some("BOT")
    }
}

/// The message payload of `MESSAGE` and `CARD_CLICKED` events.
#[derive(Debug, Clone, Deserialize)]
pub struct GchatMessage {
    /// Full message resource path (used for dedup and as the
    /// platform-side message id).
    pub name: String,
    /// User-visible text body. Missing for some card-only messages.
    #[serde(default)]
    pub text: Option<String>,
    /// Same text as `text` but with bot @-mentions stripped. When present
    /// and different from `text`, the message mentioned the bot.
    #[serde(default, rename = "argumentText")]
    pub argument_text: Option<String>,
    /// Thread the message belongs to.
    #[serde(default)]
    pub thread: Option<GchatThread>,
    /// Card-click action (only on `CARD_CLICKED`).
    #[serde(default)]
    pub action: Option<Value>,
    /// Card-click parameters (only on `CARD_CLICKED`).
    #[serde(default)]
    pub parameters: Option<Value>,
}

/// Thread reference inside a message.
#[derive(Debug, Clone, Deserialize)]
pub struct GchatThread {
    /// Full thread resource path, e.g. `spaces/AAQ/threads/XYZ`.
    pub name: String,
}

impl GchatMessage {
    /// Heuristic: if `argumentText` is set and differs from `text`, the
    /// message contained a `@bot` token that Chat stripped, i.e. the bot
    /// was mentioned.
    #[must_use]
    pub fn was_mentioned(&self) -> bool {
        match (&self.argument_text, &self.text) {
            (Some(arg), Some(text)) => arg != text,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_message_envelope() {
        let v = json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/AAA", "type": "ROOM"},
            "user": {"name": "users/1", "displayName": "Alice", "type": "HUMAN"},
            "message": {
                "name": "spaces/AAA/messages/M1",
                "text": "hello bot",
                "argumentText": "hello",
                "thread": {"name": "spaces/AAA/threads/T1"}
            }
        });
        let env: GchatEventEnvelope = serde_json::from_value(v).unwrap();
        assert!(matches!(env.event, GchatEvent::Message));
        assert_eq!(env.space.name, "spaces/AAA");
        assert!(env.space.is_room());
        let user = env.user.expect("user");
        assert_eq!(user.name, "users/1");
        assert_eq!(user.display_name.as_deref(), Some("Alice"));
        assert!(!user.is_bot());
        let m = env.message.expect("message");
        assert_eq!(m.name, "spaces/AAA/messages/M1");
        assert_eq!(m.text.as_deref(), Some("hello bot"));
        assert_eq!(m.argument_text.as_deref(), Some("hello"));
        assert!(m.was_mentioned());
        let thread = m.thread.expect("thread");
        assert_eq!(thread.name, "spaces/AAA/threads/T1");
    }

    #[test]
    fn parses_card_clicked_envelope() {
        let v = json!({
            "type": "CARD_CLICKED",
            "space": {"name": "spaces/X", "type": "ROOM"},
            "user": {"name": "users/1"},
            "message": {
                "name": "spaces/X/messages/MM",
                "action": {"actionMethodName": "do_thing"},
                "parameters": [{"key": "id", "value": "42"}]
            }
        });
        let env: GchatEventEnvelope = serde_json::from_value(v).unwrap();
        assert!(matches!(env.event, GchatEvent::CardClicked));
        let m = env.message.expect("message");
        assert!(m.action.is_some());
        assert!(m.parameters.is_some());
    }

    #[test]
    fn parses_added_to_space() {
        let v = json!({
            "type": "ADDED_TO_SPACE",
            "space": {"name": "spaces/X", "type": "ROOM"}
        });
        let env: GchatEventEnvelope = serde_json::from_value(v).unwrap();
        assert!(matches!(env.event, GchatEvent::AddedToSpace));
        assert!(env.user.is_none());
        assert!(env.message.is_none());
    }

    #[test]
    fn parses_removed_from_space() {
        let v = json!({
            "type": "REMOVED_FROM_SPACE",
            "space": {"name": "spaces/X"}
        });
        let env: GchatEventEnvelope = serde_json::from_value(v).unwrap();
        assert!(matches!(env.event, GchatEvent::RemovedFromSpace));
    }

    #[test]
    fn unknown_event_type_falls_back_to_other() {
        let v = json!({
            "type": "DUMMY_EVENT",
            "space": {"name": "spaces/X"}
        });
        let env: GchatEventEnvelope = serde_json::from_value(v).unwrap();
        assert!(matches!(env.event, GchatEvent::Other));
    }

    #[test]
    fn space_is_room_returns_true_only_for_room_type() {
        let s = GchatSpace {
            name: "spaces/X".into(),
            space_type: Some("ROOM".into()),
        };
        assert!(s.is_room());
        let s = GchatSpace {
            name: "spaces/X".into(),
            space_type: Some("DM".into()),
        };
        assert!(!s.is_room());
        let s = GchatSpace {
            name: "spaces/X".into(),
            space_type: None,
        };
        assert!(!s.is_room());
    }

    #[test]
    fn user_is_bot_only_for_bot_type() {
        let u = GchatUser {
            name: "users/1".into(),
            display_name: None,
            user_type: Some("BOT".into()),
        };
        assert!(u.is_bot());
        let u = GchatUser {
            name: "users/1".into(),
            display_name: None,
            user_type: Some("HUMAN".into()),
        };
        assert!(!u.is_bot());
        let u = GchatUser {
            name: "users/1".into(),
            display_name: None,
            user_type: None,
        };
        assert!(!u.is_bot());
    }

    #[test]
    fn was_mentioned_false_when_argument_equals_text() {
        let m = GchatMessage {
            name: "spaces/X/messages/M".into(),
            text: Some("hi".into()),
            argument_text: Some("hi".into()),
            thread: None,
            action: None,
            parameters: None,
        };
        assert!(!m.was_mentioned());
    }

    #[test]
    fn was_mentioned_false_when_argument_missing() {
        let m = GchatMessage {
            name: "spaces/X/messages/M".into(),
            text: Some("hi".into()),
            argument_text: None,
            thread: None,
            action: None,
            parameters: None,
        };
        assert!(!m.was_mentioned());
    }

    #[test]
    fn was_mentioned_true_when_argument_differs() {
        let m = GchatMessage {
            name: "spaces/X/messages/M".into(),
            text: Some("@bot hi".into()),
            argument_text: Some("hi".into()),
            thread: None,
            action: None,
            parameters: None,
        };
        assert!(m.was_mentioned());
    }

    #[test]
    fn message_without_text_decodes() {
        let v = json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/X"},
            "user": {"name": "users/1"},
            "message": {"name": "spaces/X/messages/M"}
        });
        let env: GchatEventEnvelope = serde_json::from_value(v).unwrap();
        let m = env.message.expect("message");
        assert!(m.text.is_none());
        assert!(m.thread.is_none());
    }
}
