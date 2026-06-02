//! Minimal Telegram Bot API types used by the adapter.
//!
//! Only the fields the adapter cares about are declared. Unknown fields are
//! ignored so the structs survive Telegram adding new attributes.

use serde::{Deserialize, Serialize};

/// A wrapper for the Bot API response envelope: `{ ok, result, ... }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(bound(deserialize = "T: serde::de::DeserializeOwned"))]
pub struct ApiResponse<T> {
    pub ok: bool,
    #[serde(default = "none_t")]
    pub result: Option<T>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub error_code: Option<i64>,
    #[serde(default)]
    pub parameters: Option<ResponseParameters>,
}

fn none_t<T>() -> Option<T> {
    None
}

/// `ResponseParameters` carrying optional `retry_after`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResponseParameters {
    #[serde(default)]
    pub retry_after: Option<u64>,
}

/// A Telegram user. Bots, regular users, and admins all share the shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

/// Chat metadata. Only `id` and `type` are meaningful for routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

/// Telegram `MessageEntity` covering the variants we surface as mentions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEntity {
    #[serde(rename = "type")]
    pub kind: String,
    pub offset: i64,
    pub length: i64,
    #[serde(default)]
    pub user: Option<User>,
}

/// Telegram document attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// One size variant of a photo. Telegram returns several sizes per photo;
/// the adapter picks the largest for download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub width: u64,
    #[serde(default)]
    pub height: u64,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Audio attachment (music file with metadata).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Audio {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub performer: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Video attachment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Video {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub width: u64,
    #[serde(default)]
    pub height: u64,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Voice (audio) message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Voice {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Round video message ("video note").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoNote {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub length: u64,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Sticker (webp / tgs / webm). Surfaced as a regular file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sticker {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub width: u64,
    #[serde(default)]
    pub height: u64,
    #[serde(default)]
    pub is_animated: bool,
    #[serde(default)]
    pub is_video: bool,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub set_name: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Response from `getFile`. The `file_path` is a relative path to be
/// suffixed onto `<api_base>/file/bot<token>/` for the actual download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub file_path: Option<String>,
}

/// A Telegram message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    #[serde(default)]
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub date: i64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
    #[serde(default)]
    pub document: Option<Document>,
    #[serde(default)]
    pub photo: Vec<PhotoSize>,
    #[serde(default)]
    pub audio: Option<Audio>,
    #[serde(default)]
    pub video: Option<Video>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub video_note: Option<VideoNote>,
    #[serde(default)]
    pub sticker: Option<Sticker>,
    /// When this message is itself a reply, Telegram embeds the parent
    /// message under `reply_to_message`. We surface the parent's
    /// `message_id` on the [`copperclaw_types::InboundEvent::reply_to`]
    /// field so the agent can stitch the conversation together.
    ///
    /// Boxed because the type is otherwise recursive.
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
}

/// A Telegram `Update`. The adapter handles message- and `callback_query`-
/// bearing updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub edited_message: Option<Message>,
    #[serde(default)]
    pub channel_post: Option<Message>,
    /// Update payload sent when a user taps an `inline_keyboard` button on
    /// a message produced by [`crate::adapter::TelegramAdapter::deliver_card`].
    /// The `data` field carries the button's `value` from the canonical
    /// [`copperclaw_channels_core::Card`] schema.
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

/// Telegram `CallbackQuery` — emitted when a user taps an `inline_keyboard`
/// button with a `callback_data` payload.
///
/// Only the fields the adapter needs to synthesise an [`crate::InboundEvent`]
/// and ack via `answerCallbackQuery` are declared; unknown fields are
/// ignored so the struct survives Telegram adding new attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackQuery {
    /// Opaque callback id — required for `answerCallbackQuery` to stop the
    /// button's loading spinner.
    pub id: String,
    /// The user who tapped the button.
    pub from: User,
    /// The message the button was attached to. `None` when the original
    /// message is too old to be referenced — we still emit an event so the
    /// agent can react, but routing falls back to the `from.id` chat id.
    #[serde(default)]
    pub message: Option<Message>,
    /// Payload from the button's `callback_data` field. This is exactly
    /// the [`copperclaw_channels_core::CardButton::value`] string the agent
    /// supplied when constructing the card.
    #[serde(default)]
    pub data: Option<String>,
}

/// Return shape of `sendMessage` and `sendDocument`.
#[derive(Debug, Clone, Deserialize)]
pub struct SentMessage {
    pub message_id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_with_text_message_deserializes() {
        let raw = json!({
            "update_id": 5,
            "message": {
                "message_id": 1,
                "date": 1,
                "chat": { "id": 100, "type": "private" },
                "from": { "id": 200, "is_bot": false, "username": "alice" },
                "text": "hi bot"
            }
        });
        let update: Update = serde_json::from_value(raw).unwrap();
        assert_eq!(update.update_id, 5);
        let msg = update.message.expect("message");
        assert_eq!(msg.chat.id, 100);
        assert_eq!(msg.chat.kind, "private");
        assert_eq!(msg.text.as_deref(), Some("hi bot"));
        assert_eq!(
            msg.from.as_ref().and_then(|u| u.username.as_deref()),
            Some("alice")
        );
    }

    #[test]
    fn message_with_entities_deserializes() {
        let raw = json!({
            "message_id": 7,
            "date": 1,
            "chat": { "id": 1, "type": "group" },
            "text": "@botty look",
            "entities": [
                { "type": "mention", "offset": 0, "length": 6 }
            ]
        });
        let msg: Message = serde_json::from_value(raw).unwrap();
        assert_eq!(msg.entities.len(), 1);
        assert_eq!(msg.entities[0].kind, "mention");
    }

    #[test]
    fn message_with_document_deserializes() {
        let raw = json!({
            "message_id": 9,
            "date": 1,
            "chat": { "id": 1, "type": "private" },
            "document": {
                "file_id": "F",
                "file_unique_id": "U",
                "file_name": "a.txt",
                "mime_type": "text/plain",
                "file_size": 12
            }
        });
        let msg: Message = serde_json::from_value(raw).unwrap();
        let d = msg.document.expect("doc");
        assert_eq!(d.file_id, "F");
        assert_eq!(d.file_name.as_deref(), Some("a.txt"));
        assert_eq!(d.file_size, Some(12));
    }

    #[test]
    fn update_with_text_message_in_forum_thread() {
        let raw = json!({
            "update_id": 1,
            "message": {
                "message_id": 1,
                "message_thread_id": 42,
                "date": 1,
                "chat": { "id": -100, "type": "supergroup" },
                "text": "x"
            }
        });
        let update: Update = serde_json::from_value(raw).unwrap();
        let msg = update.message.unwrap();
        assert_eq!(msg.message_thread_id, Some(42));
    }

    #[test]
    fn api_response_ok_envelope() {
        let raw = json!({"ok": true, "result": { "message_id": 9 }});
        let resp: ApiResponse<SentMessage> = serde_json::from_value(raw).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.result.unwrap().message_id, 9);
    }

    #[test]
    fn api_response_error_envelope_with_retry_after() {
        let raw = json!({
            "ok": false,
            "error_code": 429,
            "description": "Too Many Requests",
            "parameters": { "retry_after": 15 }
        });
        let resp: ApiResponse<SentMessage> = serde_json::from_value(raw).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error_code, Some(429));
        assert_eq!(resp.parameters.unwrap().retry_after, Some(15));
    }

    #[test]
    fn user_full_clone() {
        let u = User {
            id: 1,
            is_bot: true,
            first_name: Some("B".into()),
            last_name: None,
            username: Some("b".into()),
        };
        let u2 = u.clone();
        assert_eq!(u, u2);
    }

    #[test]
    fn entity_clone_and_eq() {
        let e = MessageEntity {
            kind: "text_mention".into(),
            offset: 0,
            length: 1,
            user: Some(User {
                id: 1,
                is_bot: false,
                first_name: None,
                last_name: None,
                username: Some("u".into()),
            }),
        };
        assert_eq!(e, e.clone());
    }

    #[test]
    fn update_serialize_roundtrip() {
        let u = Update {
            update_id: 1,
            message: Some(Message {
                message_id: 1,
                message_thread_id: None,
                from: None,
                chat: Chat {
                    id: 1,
                    kind: "private".into(),
                    title: None,
                    username: None,
                },
                date: 0,
                text: Some("hi".into()),
                caption: None,
                entities: vec![],
                document: None,
                photo: vec![],
                audio: None,
                video: None,
                voice: None,
                video_note: None,
                sticker: None,
                reply_to_message: None,
            }),
            edited_message: None,
            channel_post: None,
            callback_query: None,
        };
        let json = serde_json::to_string(&u).unwrap();
        let back: Update = serde_json::from_str(&json).unwrap();
        assert_eq!(u, back);
    }

    #[test]
    fn response_parameters_default() {
        let rp = ResponseParameters::default();
        assert!(rp.retry_after.is_none());
    }
}
