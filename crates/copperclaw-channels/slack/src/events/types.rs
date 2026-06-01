//! Slack Events API payload types.
//!
//! We only deserialize fields we actually use; everything else is captured
//! into the catch-all `extra` map so payloads with future fields still parse.

use serde::Deserialize;
use serde_json::Value;

/// Outer envelope Slack POSTs to the webhook.
///
/// The two supported variants are `url_verification` (handshake) and
/// `event_callback` (everything else).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SlackEventEnvelope {
    #[serde(rename = "url_verification")]
    UrlVerification {
        challenge: String,
        #[serde(default)]
        token: Option<String>,
    },
    #[serde(rename = "event_callback")]
    EventCallback(SlackEventCallback),
}

/// Payload inside an `event_callback` envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackEventCallback {
    pub event_id: String,
    pub event: SlackEvent,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub api_app_id: Option<String>,
}

/// Concrete inner Slack event. Unsupported `type`s deserialize as `Other`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SlackEvent {
    /// `message` (and all subtype-channel variants like `message.im`,
    /// `message.channels`, `message.app_mention`). Slack uses `type:
    /// "message"` for all of them, distinguished by `channel_type` (`im`,
    /// `channel`, `group`, `mpim`) and `subtype`.
    #[serde(rename = "message")]
    Message(MessageEvent),
    /// `app_mention`.
    #[serde(rename = "app_mention")]
    AppMention(MessageEvent),
    /// Anything else — we ignore but still accept.
    #[serde(other)]
    Other,
}

/// Fields that every message-shaped event carries (message + `app_mention`).
#[derive(Debug, Clone, Deserialize)]
pub struct MessageEvent {
    /// Slack message ts — also serves as the platform-side message id.
    pub ts: String,
    /// Channel id. `C...` channels, `G...` groups/mpdms, `D...` DMs.
    pub channel: String,
    /// User id who authored the message (None for bot messages without one).
    #[serde(default)]
    pub user: Option<String>,
    /// Message text.
    #[serde(default)]
    pub text: Option<String>,
    /// If present, the message belongs to that thread.
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// `im`, `channel`, `group`, `mpim`, …
    #[serde(default)]
    pub channel_type: Option<String>,
    /// Optional message subtype (e.g. `bot_message`, `message_changed`).
    #[serde(default)]
    pub subtype: Option<String>,
    /// Block kit payload. Captured verbatim and forwarded in `content`.
    #[serde(default)]
    pub blocks: Option<Value>,
}

impl MessageEvent {
    /// Whether the message channel id looks like a group/channel (Slack
    /// prefixes `C` for channels and `G` for private groups / mpdm).
    #[must_use]
    pub fn is_group_channel(&self) -> bool {
        self.channel.starts_with('C') || self.channel.starts_with('G')
    }

    /// Does the text mention `<@bot_user_id>`?
    #[must_use]
    pub fn mentions_user(&self, bot_user_id: &str) -> bool {
        let needle = format!("<@{bot_user_id}>");
        self.text.as_deref().is_some_and(|t| t.contains(&needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_url_verification_envelope() {
        let v = json!({"type":"url_verification","challenge":"abc","token":"t"});
        let env: SlackEventEnvelope = serde_json::from_value(v).unwrap();
        match env {
            SlackEventEnvelope::UrlVerification { challenge, token } => {
                assert_eq!(challenge, "abc");
                assert_eq!(token.as_deref(), Some("t"));
            }
            SlackEventEnvelope::EventCallback(cb) => panic!("got callback: {cb:?}"),
        }
    }

    #[test]
    fn parses_url_verification_without_token() {
        let v = json!({"type":"url_verification","challenge":"hi"});
        let env: SlackEventEnvelope = serde_json::from_value(v).unwrap();
        match env {
            SlackEventEnvelope::UrlVerification { challenge, token } => {
                assert_eq!(challenge, "hi");
                assert!(token.is_none());
            }
            SlackEventEnvelope::EventCallback(cb) => panic!("got callback: {cb:?}"),
        }
    }

    #[test]
    fn parses_message_event() {
        let v = json!({
            "type":"event_callback",
            "event_id":"Ev1",
            "team_id":"T1",
            "api_app_id":"A1",
            "event": {
                "type":"message",
                "ts":"1.0",
                "channel":"C1",
                "user":"U1",
                "text":"hello",
                "channel_type":"channel"
            }
        });
        let env: SlackEventEnvelope = serde_json::from_value(v).unwrap();
        let SlackEventEnvelope::EventCallback(cb) = env else {
            panic!("expected callback");
        };
        assert_eq!(cb.event_id, "Ev1");
        assert_eq!(cb.team_id.as_deref(), Some("T1"));
        assert_eq!(cb.api_app_id.as_deref(), Some("A1"));
        match cb.event {
            SlackEvent::Message(m) => {
                assert_eq!(m.ts, "1.0");
                assert_eq!(m.channel, "C1");
                assert_eq!(m.user.as_deref(), Some("U1"));
                assert_eq!(m.text.as_deref(), Some("hello"));
                assert_eq!(m.channel_type.as_deref(), Some("channel"));
                assert!(m.thread_ts.is_none());
                assert!(m.subtype.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn parses_app_mention_event() {
        let v = json!({
            "type":"event_callback",
            "event_id":"Ev2",
            "event": {
                "type":"app_mention",
                "ts":"2.0",
                "channel":"C2",
                "user":"U2",
                "text":"<@B0> hi",
                "blocks":[{"type":"rich_text"}],
                "thread_ts":"1.0"
            }
        });
        let env: SlackEventEnvelope = serde_json::from_value(v).unwrap();
        let SlackEventEnvelope::EventCallback(cb) = env else {
            panic!("expected callback");
        };
        match cb.event {
            SlackEvent::AppMention(m) => {
                assert_eq!(m.thread_ts.as_deref(), Some("1.0"));
                assert!(m.blocks.is_some());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_type_falls_back_to_other() {
        let v = json!({
            "type":"event_callback",
            "event_id":"Ev3",
            "event": {"type":"reaction_added"}
        });
        let env: SlackEventEnvelope = serde_json::from_value(v).unwrap();
        let SlackEventEnvelope::EventCallback(cb) = env else {
            panic!("expected callback");
        };
        assert!(matches!(cb.event, SlackEvent::Other));
    }

    #[test]
    fn is_group_channel_recognizes_c_and_g_prefixes() {
        let mut m = MessageEvent {
            ts: "1".into(),
            channel: "C123".into(),
            user: None,
            text: None,
            thread_ts: None,
            channel_type: None,
            subtype: None,
            blocks: None,
        };
        assert!(m.is_group_channel());
        m.channel = "G123".into();
        assert!(m.is_group_channel());
        m.channel = "D123".into();
        assert!(!m.is_group_channel());
    }

    #[test]
    fn mentions_user_checks_explicit_token() {
        let m = MessageEvent {
            ts: "1".into(),
            channel: "C1".into(),
            user: Some("U1".into()),
            text: Some("hey <@UBOT> there".into()),
            thread_ts: None,
            channel_type: None,
            subtype: None,
            blocks: None,
        };
        assert!(m.mentions_user("UBOT"));
        assert!(!m.mentions_user("UOTHER"));
    }

    #[test]
    fn mentions_user_handles_missing_text() {
        let m = MessageEvent {
            ts: "1".into(),
            channel: "C1".into(),
            user: None,
            text: None,
            thread_ts: None,
            channel_type: None,
            subtype: None,
            blocks: None,
        };
        assert!(!m.mentions_user("UBOT"));
    }
}
