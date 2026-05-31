//! Pure functions that turn a signal-cli `receive` notification envelope
//! into an [`InboundEvent`].
//!
//! These helpers do not touch the subprocess or the filesystem so they can
//! be unit-tested with fixture JSON.
//!
//! The envelope shape we care about (sent by `signal-cli daemon --json-rpc`):
//!
//! ```json
//! {
//!   "envelope": {
//!     "source": "+15551112222",
//!     "sourceUuid": "abc-uuid",
//!     "sourceName": "Alice",
//!     "timestamp": 1700000000000,
//!     "dataMessage": {
//!       "message": "hi",
//!       "groupInfo": {
//!         "groupId": "<base64-id>",
//!         "type": "DELIVER"
//!       },
//!       "attachments": [
//!         { "id": "...", "filename": "x.jpg", "contentType": "image/jpeg" }
//!       ]
//!     }
//!   }
//! }
//! ```

use chrono::{DateTime, TimeZone, Utc};
use ironclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, ReplyTo, SenderIdentity,
};
use serde_json::{Value, json};

use crate::factory::CHANNEL_TYPE_STR;

/// Convert a `receive` notification's `params` value into an
/// [`InboundEvent`], when the payload corresponds to a chat (data) message.
///
/// Returns `None` for events that do not carry a `dataMessage` (delivery
/// receipts, typing notifications, sync messages — these are all part of
/// the same stream but are not actionable as inbound user input).
pub fn params_to_inbound(params: &Value) -> Option<InboundEvent> {
    let envelope = params.get("envelope")?;
    envelope_to_inbound(envelope)
}

/// Convert an `envelope` JSON object into an [`InboundEvent`].
pub fn envelope_to_inbound(envelope: &Value) -> Option<InboundEvent> {
    let data = envelope.get("dataMessage")?;
    if !data.is_object() {
        return None;
    }

    let source = envelope
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let source_name = envelope
        .get("sourceName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let timestamp_ms = envelope
        .get("timestamp")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let group_id = data
        .get("groupInfo")
        .and_then(|g| g.get("groupId"))
        .and_then(Value::as_str);
    let is_group = group_id.is_some();
    let platform_id = if let Some(gid) = group_id {
        format!("group:{gid}")
    } else if !source.is_empty() {
        format!("user:{source}")
    } else {
        // No group and no source — we cannot build a routable platform id.
        return None;
    };

    let text = data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let attachments = data
        .get("attachments")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    json!({
                        "id": a.get("id").and_then(Value::as_str).unwrap_or_default(),
                        "filename": a
                            .get("filename")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        "content_type": a
                            .get("contentType")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let content = if attachments.is_empty() {
        json!({ "text": text })
    } else {
        json!({ "text": text, "attachments": attachments })
    };

    let channel_type = ChannelType::new(CHANNEL_TYPE_STR);
    let message_id = timestamp_ms.to_string();

    // signal-cli surfaces "this message quotes <parent>" via the `quote`
    // object on the dataMessage. The quote's `id` is the millisecond
    // timestamp of the quoted message — which is exactly the format we
    // use for `message.id`, so it pairs up cleanly on the agent side.
    let reply_to = data
        .get("quote")
        .and_then(|q| q.get("id"))
        .and_then(Value::as_i64)
        .map(|qid| ReplyTo {
            channel_type: channel_type.clone(),
            platform_id: platform_id.clone(),
            thread_id: Some(qid.to_string()),
        });

    let sender_identity = if source.is_empty() {
        None
    } else {
        Some(SenderIdentity {
            channel_type: channel_type.clone(),
            identity: source.to_owned(),
            display_name: source_name,
        })
    };

    Some(InboundEvent {
        channel_type: channel_type.clone(),
        platform_id,
        thread_id: None,
        message: InboundMessage {
            id: message_id,
            kind: MessageKind::Chat,
            content,
            timestamp: ms_to_datetime(timestamp_ms),
            is_mention: None,
            is_group: Some(is_group),
        },
        reply_to,
        sender: sender_identity,
    })
}

/// Convert a milliseconds-since-epoch i64 to a UTC datetime.
///
/// Defensive: extreme values that cannot be represented fall back to
/// `Utc::now()` rather than panicking.
pub fn ms_to_datetime(ms: i64) -> DateTime<Utc> {
    let secs = ms.div_euclid(1000);
    let nanos = u32::try_from(ms.rem_euclid(1000).unsigned_abs() * 1_000_000).unwrap_or(0);
    match Utc.timestamp_opt(secs, nanos) {
        chrono::LocalResult::Single(dt) => dt,
        _ => Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one_to_one(message: &str) -> Value {
        json!({
            "envelope": {
                "source": "+15551112222",
                "sourceUuid": "u-1",
                "sourceName": "Alice",
                "timestamp": 1_700_000_000_000_i64,
                "dataMessage": {
                    "message": message,
                    "timestamp": 1_700_000_000_000_i64,
                }
            }
        })
    }

    fn group_msg(message: &str, gid: &str) -> Value {
        json!({
            "envelope": {
                "source": "+15551112222",
                "sourceUuid": "u-1",
                "sourceName": "Alice",
                "timestamp": 1_700_000_000_000_i64,
                "dataMessage": {
                    "message": message,
                    "timestamp": 1_700_000_000_000_i64,
                    "groupInfo": {
                        "groupId": gid,
                        "type": "DELIVER"
                    }
                }
            }
        })
    }

    #[test]
    fn one_to_one_chat_becomes_user_platform_id() {
        let evt = params_to_inbound(&one_to_one("hi")).unwrap();
        assert_eq!(evt.channel_type.as_str(), "signal");
        assert_eq!(evt.platform_id, "user:+15551112222");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hi");
        assert_eq!(evt.message.is_group, Some(false));
        assert!(evt.thread_id.is_none());
        assert!(evt.message.is_mention.is_none());
        let sender = evt.sender.as_ref().expect("sender");
        assert_eq!(sender.identity, "+15551112222");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
        assert_eq!(sender.channel_type.as_str(), "signal");
    }

    #[test]
    fn group_chat_becomes_group_platform_id() {
        let evt = params_to_inbound(&group_msg("hi", "Z3JvdXAxMjM=")).unwrap();
        assert_eq!(evt.platform_id, "group:Z3JvdXAxMjM=");
        assert_eq!(evt.message.is_group, Some(true));
        assert_eq!(evt.message.content["text"], "hi");
    }

    #[test]
    fn message_id_is_envelope_timestamp_string() {
        let evt = params_to_inbound(&one_to_one("hi")).unwrap();
        assert_eq!(evt.message.id, "1700000000000");
    }

    #[test]
    fn no_data_message_returns_none() {
        // Delivery receipts and typing notifications carry no dataMessage.
        let env = json!({
            "envelope": {
                "source": "+15551112222",
                "timestamp": 1_700_000_000_000_i64,
                "receiptMessage": { "when": 1_700_000_000_000_i64 }
            }
        });
        assert!(params_to_inbound(&env).is_none());
    }

    #[test]
    fn missing_envelope_returns_none() {
        let v = json!({ "method": "receive" });
        assert!(params_to_inbound(&v).is_none());
    }

    #[test]
    fn data_message_must_be_object() {
        let env = json!({
            "envelope": {
                "source": "+1",
                "timestamp": 0,
                "dataMessage": "junk"
            }
        });
        assert!(params_to_inbound(&env).is_none());
    }

    #[test]
    fn missing_source_and_no_group_returns_none() {
        let env = json!({
            "envelope": {
                "timestamp": 1,
                "dataMessage": { "message": "x" }
            }
        });
        assert!(params_to_inbound(&env).is_none());
    }

    #[test]
    fn missing_source_with_group_is_routable() {
        // Some sync messages can lack `source` but have a group id; we should
        // still build an event since the group is the platform target.
        let env = json!({
            "envelope": {
                "timestamp": 1,
                "dataMessage": {
                    "message": "from group",
                    "groupInfo": { "groupId": "g==", "type": "DELIVER" }
                }
            }
        });
        let evt = params_to_inbound(&env).unwrap();
        assert_eq!(evt.platform_id, "group:g==");
        assert!(evt.sender.is_none());
    }

    #[test]
    fn empty_body_with_attachments_is_chat() {
        let env = json!({
            "envelope": {
                "source": "+1",
                "timestamp": 1,
                "dataMessage": {
                    "attachments": [
                        { "id": "a1", "filename": "x.jpg", "contentType": "image/jpeg" }
                    ]
                }
            }
        });
        let evt = params_to_inbound(&env).unwrap();
        assert_eq!(evt.message.content["text"], "");
        let arr = evt.message.content["attachments"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "a1");
        assert_eq!(arr[0]["filename"], "x.jpg");
        assert_eq!(arr[0]["content_type"], "image/jpeg");
    }

    #[test]
    fn attachments_without_filename_uses_empty_string() {
        let env = json!({
            "envelope": {
                "source": "+1",
                "timestamp": 1,
                "dataMessage": {
                    "message": "see file",
                    "attachments": [{ "id": "only-id" }]
                }
            }
        });
        let evt = params_to_inbound(&env).unwrap();
        let arr = evt.message.content["attachments"].as_array().unwrap();
        assert_eq!(arr[0]["id"], "only-id");
        assert_eq!(arr[0]["filename"], "");
        assert_eq!(arr[0]["content_type"], "");
    }

    #[test]
    fn missing_source_name_yields_none_display_name() {
        let env = json!({
            "envelope": {
                "source": "+1",
                "timestamp": 1,
                "dataMessage": { "message": "x" }
            }
        });
        let evt = params_to_inbound(&env).unwrap();
        let sender = evt.sender.as_ref().expect("sender");
        assert!(sender.display_name.is_none());
    }

    #[test]
    fn timestamp_propagates_from_envelope() {
        let evt = params_to_inbound(&one_to_one("hi")).unwrap();
        assert_eq!(evt.message.timestamp.timestamp(), 1_700_000_000);
    }

    #[test]
    fn missing_message_field_defaults_to_empty() {
        let env = json!({
            "envelope": {
                "source": "+1",
                "timestamp": 1,
                "dataMessage": {}
            }
        });
        let evt = params_to_inbound(&env).unwrap();
        assert_eq!(evt.message.content["text"], "");
    }

    #[test]
    fn empty_attachments_array_omits_attachments_key() {
        let env = json!({
            "envelope": {
                "source": "+1",
                "timestamp": 1,
                "dataMessage": { "message": "hi", "attachments": [] }
            }
        });
        let evt = params_to_inbound(&env).unwrap();
        assert!(evt.message.content.get("attachments").is_none());
    }

    #[test]
    fn channel_type_is_signal_constant() {
        let evt = params_to_inbound(&one_to_one("hi")).unwrap();
        assert_eq!(evt.channel_type.as_str(), CHANNEL_TYPE_STR);
    }

    #[test]
    fn ms_to_datetime_zero() {
        let dt = ms_to_datetime(0);
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn ms_to_datetime_extreme_input_does_not_panic() {
        // The conversion either succeeds or falls back to now(); never panics.
        let _ = ms_to_datetime(i64::MAX);
        let _ = ms_to_datetime(i64::MIN);
    }

    #[test]
    fn envelope_to_inbound_direct_entry_point() {
        let v = json!({
            "source": "+1",
            "timestamp": 1,
            "dataMessage": { "message": "yo" }
        });
        let evt = envelope_to_inbound(&v).unwrap();
        assert_eq!(evt.platform_id, "user:+1");
    }

    #[test]
    fn quote_populates_reply_to_with_quoted_timestamp() {
        let env = json!({
            "envelope": {
                "source": "+15551112222",
                "timestamp": 1_700_000_005_000_i64,
                "dataMessage": {
                    "message": "agreed",
                    "quote": {
                        "id": 1_700_000_000_000_i64,
                        "author": "+15553334444",
                        "text": "should we ship?"
                    }
                }
            }
        });
        let evt = params_to_inbound(&env).unwrap();
        let rt = evt.reply_to.expect("reply_to populated from quote.id");
        assert_eq!(rt.channel_type.as_str(), "signal");
        assert_eq!(rt.platform_id, "user:+15551112222");
        assert_eq!(rt.thread_id.as_deref(), Some("1700000000000"));
    }

    #[test]
    fn message_without_quote_leaves_reply_to_none() {
        let evt = params_to_inbound(&one_to_one("just thinking out loud")).unwrap();
        assert!(evt.reply_to.is_none());
    }

    #[test]
    fn group_id_distinct_from_user_id_format() {
        let group = params_to_inbound(&group_msg("g", "abc")).unwrap();
        let user = params_to_inbound(&one_to_one("u")).unwrap();
        assert!(group.platform_id.starts_with("group:"));
        assert!(user.platform_id.starts_with("user:"));
    }
}
