//! Convert already-decrypted WhatsApp message payloads to [`InboundEvent`].
//!
//! Real WhatsApp messages are encrypted end-to-end and only become parseable
//! after the Signal Protocol session decrypts them. Because the
//! [`crate::crypto::CryptoBackend`] is stubbed out for this slice, the
//! production data path never actually reaches these helpers — but they
//! exist so:
//!
//! 1. The interface a future contributor needs to implement is fixed.
//! 2. The pure JSON-fixture-driven test suite can exercise every kind of
//!    inbound event (text, file, system status updates).
//!
//! The accepted JSON shape mirrors what a hypothetical Signal decryption
//! routine would emit:
//!
//! ```json
//! {
//!   "from": "+15551112222",       // or "12345-67890@g.us"
//!   "from_name": "Alice",          // optional display name
//!   "is_group": false,
//!   "timestamp_ms": 1700000000000,
//!   "message": {
//!     "id": "wamid.AAA",
//!     "kind": "text",              // text | file | system
//!     "text": "hello",
//!     "file": { "filename": "x.pdf", "mime": "application/pdf",
//!               "size": 1234 },
//!     "system": { "action": "subject_changed", "value": "New name" }
//!   }
//! }
//! ```
//!
//! Only the kinds the test fixtures exercise are wired up.

use chrono::{DateTime, TimeZone, Utc};
use ironclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity,
};
use serde_json::{Value, json};

use crate::factory::CHANNEL_TYPE_STR;

/// Turn a decrypted payload into an [`InboundEvent`].
///
/// Returns `None` if the payload lacks the fields needed to route the
/// event (missing `from`, missing `message`, unknown kind).
pub fn payload_to_inbound(payload: &Value) -> Option<InboundEvent> {
    let from = payload.get("from").and_then(Value::as_str)?;
    let from_name = payload.get("from_name").and_then(Value::as_str);
    let is_group = payload
        .get("is_group")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ts_ms = payload
        .get("timestamp_ms")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let timestamp = ms_to_chrono(ts_ms);
    let message = payload.get("message")?;

    let id = message.get("id").and_then(Value::as_str)?.to_owned();
    let kind = message.get("kind").and_then(Value::as_str)?;
    let (content, kind_enum) = match kind {
        "text" => {
            let text = message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (json!({"text": text}), MessageKind::Chat)
        }
        "file" => {
            let file = message.get("file")?;
            let text = message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (
                json!({
                    "text": text,
                    "file": {
                        "filename": file.get("filename").and_then(Value::as_str).unwrap_or_default(),
                        "mime": file.get("mime").and_then(Value::as_str).unwrap_or_default(),
                        "size": file.get("size").and_then(Value::as_i64).unwrap_or_default(),
                    }
                }),
                MessageKind::Chat,
            )
        }
        "system" => {
            let sys = message.get("system")?;
            (
                json!({
                    "action": sys.get("action").and_then(Value::as_str).unwrap_or_default(),
                    "value": sys.get("value").cloned().unwrap_or(Value::Null),
                }),
                MessageKind::System,
            )
        }
        _ => return None,
    };

    let platform_id = if is_group {
        format!("group:{from}")
    } else {
        format!("user:{from}")
    };
    let sender = Some(SenderIdentity {
        channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        identity: from.to_owned(),
        display_name: from_name.map(str::to_owned),
    });
    Some(InboundEvent {
        channel_type: ChannelType::new(CHANNEL_TYPE_STR),
        platform_id,
        thread_id: None,
        message: InboundMessage {
            id,
            kind: kind_enum,
            content,
            timestamp,
            is_mention: None,
            is_group: Some(is_group),
        },
        reply_to: None,
        sender,
    })
}

fn ms_to_chrono(ms: i64) -> DateTime<Utc> {
    let secs = ms / 1000;
    let nsec = u32::try_from(ms.rem_euclid(1000)).unwrap_or(0) * 1_000_000;
    Utc.timestamp_opt(secs, nsec)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dm_text_fixture() -> Value {
        json!({
            "from": "15551112222",
            "from_name": "Alice",
            "is_group": false,
            "timestamp_ms": 1_700_000_000_000_i64,
            "message": {
                "id": "wamid.AAA",
                "kind": "text",
                "text": "hi"
            }
        })
    }

    fn group_text_fixture() -> Value {
        json!({
            "from": "12345-67890@g.us",
            "from_name": null,
            "is_group": true,
            "timestamp_ms": 1_700_000_000_000_i64,
            "message": {
                "id": "wamid.BBB",
                "kind": "text",
                "text": "hello group"
            }
        })
    }

    fn file_fixture() -> Value {
        json!({
            "from": "15551112222",
            "timestamp_ms": 0,
            "message": {
                "id": "wamid.FILE",
                "kind": "file",
                "text": "see attached",
                "file": {
                    "filename": "doc.pdf",
                    "mime": "application/pdf",
                    "size": 1234
                }
            }
        })
    }

    fn system_fixture() -> Value {
        json!({
            "from": "12345-67890@g.us",
            "is_group": true,
            "timestamp_ms": 1,
            "message": {
                "id": "wamid.SYS",
                "kind": "system",
                "system": {
                    "action": "subject_changed",
                    "value": "New Subject"
                }
            }
        })
    }

    #[test]
    fn parse_dm_text() {
        let evt = payload_to_inbound(&dm_text_fixture()).unwrap();
        assert_eq!(evt.channel_type.as_str(), CHANNEL_TYPE_STR);
        assert_eq!(evt.platform_id, "user:15551112222");
        assert_eq!(evt.message.id, "wamid.AAA");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hi");
        assert_eq!(evt.message.is_group, Some(false));
        assert_eq!(evt.sender.as_ref().unwrap().display_name.as_deref(), Some("Alice"));
        assert_eq!(evt.thread_id, None);
    }

    #[test]
    fn parse_group_text() {
        let evt = payload_to_inbound(&group_text_fixture()).unwrap();
        assert_eq!(evt.platform_id, "group:12345-67890@g.us");
        assert_eq!(evt.message.is_group, Some(true));
        assert_eq!(evt.message.content["text"], "hello group");
        assert!(evt.sender.as_ref().unwrap().display_name.is_none());
    }

    #[test]
    fn parse_file_message() {
        let evt = payload_to_inbound(&file_fixture()).unwrap();
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["file"]["filename"], "doc.pdf");
        assert_eq!(evt.message.content["file"]["mime"], "application/pdf");
        assert_eq!(evt.message.content["file"]["size"], 1234);
        assert_eq!(evt.message.content["text"], "see attached");
    }

    #[test]
    fn parse_system_message() {
        let evt = payload_to_inbound(&system_fixture()).unwrap();
        assert_eq!(evt.message.kind, MessageKind::System);
        assert_eq!(evt.message.content["action"], "subject_changed");
        assert_eq!(evt.message.content["value"], "New Subject");
    }

    #[test]
    fn missing_from_returns_none() {
        let v = json!({
            "timestamp_ms": 0,
            "message": {"id": "x", "kind": "text", "text": "y"}
        });
        assert!(payload_to_inbound(&v).is_none());
    }

    #[test]
    fn missing_message_returns_none() {
        let v = json!({"from": "x"});
        assert!(payload_to_inbound(&v).is_none());
    }

    #[test]
    fn missing_id_returns_none() {
        let v = json!({
            "from": "+1",
            "message": {"kind": "text", "text": "y"}
        });
        assert!(payload_to_inbound(&v).is_none());
    }

    #[test]
    fn missing_kind_returns_none() {
        let v = json!({
            "from": "+1",
            "message": {"id": "1", "text": "y"}
        });
        assert!(payload_to_inbound(&v).is_none());
    }

    #[test]
    fn unknown_kind_returns_none() {
        let v = json!({
            "from": "+1",
            "message": {"id": "1", "kind": "voice-note"}
        });
        assert!(payload_to_inbound(&v).is_none());
    }

    #[test]
    fn file_kind_without_file_returns_none() {
        let v = json!({
            "from": "+1",
            "message": {"id": "1", "kind": "file"}
        });
        assert!(payload_to_inbound(&v).is_none());
    }

    #[test]
    fn system_kind_without_system_returns_none() {
        let v = json!({
            "from": "+1",
            "message": {"id": "1", "kind": "system"}
        });
        assert!(payload_to_inbound(&v).is_none());
    }

    #[test]
    fn missing_text_defaults_to_empty_string() {
        let v = json!({
            "from": "+1",
            "message": {"id": "1", "kind": "text"}
        });
        let evt = payload_to_inbound(&v).unwrap();
        assert_eq!(evt.message.content["text"], "");
    }

    #[test]
    fn missing_is_group_defaults_to_false() {
        let v = json!({
            "from": "user-1",
            "message": {"id": "1", "kind": "text", "text": "x"}
        });
        let evt = payload_to_inbound(&v).unwrap();
        assert_eq!(evt.platform_id, "user:user-1");
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[test]
    fn timestamp_ms_zero_yields_unix_epoch() {
        let v = json!({
            "from": "u",
            "timestamp_ms": 0,
            "message": {"id": "1", "kind": "text", "text": "x"}
        });
        let evt = payload_to_inbound(&v).unwrap();
        assert_eq!(evt.message.timestamp.timestamp_millis(), 0);
    }

    #[test]
    fn ms_to_chrono_handles_positive() {
        let dt = ms_to_chrono(1_700_000_000_000);
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }

    #[test]
    fn ms_to_chrono_handles_zero() {
        let dt = ms_to_chrono(0);
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn ms_to_chrono_negative_yields_fallback() {
        let dt = ms_to_chrono(-1);
        // We don't promise a specific past time; we just promise no panic.
        let _ = dt.timestamp();
    }

    #[test]
    fn sender_identity_populated() {
        let evt = payload_to_inbound(&dm_text_fixture()).unwrap();
        let s = evt.sender.unwrap();
        assert_eq!(s.identity, "15551112222");
        assert_eq!(s.display_name.as_deref(), Some("Alice"));
        assert_eq!(s.channel_type.as_str(), "whatsapp");
    }

    #[test]
    fn parse_file_message_with_missing_optional_fields() {
        // Confirm that the file kind tolerates missing sub-fields by
        // defaulting them rather than refusing to parse.
        let v = json!({
            "from": "+1",
            "message": {
                "id": "1",
                "kind": "file",
                "file": {}
            }
        });
        let evt = payload_to_inbound(&v).unwrap();
        assert_eq!(evt.message.content["file"]["filename"], "");
        assert_eq!(evt.message.content["file"]["mime"], "");
        assert_eq!(evt.message.content["file"]["size"], 0);
    }

    #[test]
    fn parse_system_message_with_null_value() {
        let v = json!({
            "from": "+1",
            "message": {
                "id": "1",
                "kind": "system",
                "system": {"action": "left", "value": null}
            }
        });
        let evt = payload_to_inbound(&v).unwrap();
        assert_eq!(evt.message.content["action"], "left");
        assert_eq!(evt.message.content["value"], serde_json::Value::Null);
    }
}
