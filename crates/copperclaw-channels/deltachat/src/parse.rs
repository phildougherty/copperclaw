//! Pure helpers that convert deltachat-rpc-server payloads into
//! [`InboundEvent`]s.
//!
//! These functions are pure (no async, no I/O) so they can be unit-tested
//! against fixture JSON without involving the subprocess.

use crate::api::{ChatInfo, MessageView};
use crate::factory::CHANNEL_TYPE_STR;
use chrono::{DateTime, TimeZone, Utc};
use copperclaw_types::{ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity};
use serde_json::{Value, json};

/// Build the `platform_id` shape this channel uses on inbound and parses
/// on `deliver`: `"account/<account_id>/chat/<chat_id>"`.
pub fn build_platform_id(account_id: u64, chat_id: i64) -> String {
    format!("account/{account_id}/chat/{chat_id}")
}

/// Parsed components of a [`build_platform_id`] string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPlatformId {
    /// Account id portion.
    pub account_id: u64,
    /// Chat id portion.
    pub chat_id: i64,
}

/// Parse the inverse of [`build_platform_id`].
///
/// Returns `None` when the input has the wrong shape; the caller should
/// turn that into [`copperclaw_channels_core::AdapterError::BadRequest`].
pub fn parse_platform_id(s: &str) -> Option<ParsedPlatformId> {
    let mut iter = s.split('/');
    if iter.next()? != "account" {
        return None;
    }
    let account_id: u64 = iter.next()?.parse().ok()?;
    if iter.next()? != "chat" {
        return None;
    }
    let chat_id: i64 = iter.next()?.parse().ok()?;
    if iter.next().is_some() {
        return None;
    }
    Some(ParsedPlatformId {
        account_id,
        chat_id,
    })
}

/// Convert a deltachat `MessageView` + companion `ChatInfo` into an
/// [`InboundEvent`].
///
/// Returns `None` for messages that should be skipped (info-only system
/// messages). Otherwise the resulting event:
///
/// - has `channel_type = "deltachat"`,
/// - `platform_id = "account/<account_id>/chat/<chat_id>"`,
/// - `thread_id = None` (Delta Chat surfaces threading via quoted
///   message ids, which are out of scope for v1),
/// - `message.kind = Chat`,
/// - `message.content = { "text": <text>, "attachment": {...}? }`,
/// - `message.is_group` reflects the chat type,
/// - sender identity is `from_id` (as a string) plus `sender_name`.
pub fn event_to_inbound(
    account_id: u64,
    msg: &MessageView,
    chat: &ChatInfo,
) -> Option<InboundEvent> {
    if msg.is_info {
        return None;
    }
    let channel_type = ChannelType::new(CHANNEL_TYPE_STR);
    let mut content = serde_json::Map::new();
    content.insert("text".to_owned(), Value::String(msg.text.clone()));
    if let Some(path) = msg.file.as_deref() {
        let mut attachment = serde_json::Map::new();
        attachment.insert("path".to_owned(), Value::String(path.to_owned()));
        if let Some(name) = msg.filename.as_deref() {
            attachment.insert("filename".to_owned(), Value::String(name.to_owned()));
        }
        if !msg.view_type.is_empty() {
            attachment.insert("view_type".to_owned(), Value::String(msg.view_type.clone()));
        }
        content.insert("attachment".to_owned(), Value::Object(attachment));
    }

    let timestamp: DateTime<Utc> = Utc
        .timestamp_opt(msg.timestamp, 0)
        .single()
        .unwrap_or_else(Utc::now);

    Some(InboundEvent {
        channel_type: channel_type.clone(),
        platform_id: build_platform_id(account_id, chat.id),
        thread_id: None,
        message: InboundMessage {
            id: msg.id.to_string(),
            kind: MessageKind::Chat,
            content: Value::Object(content),
            timestamp,
            is_mention: None,
            is_group: Some(chat.is_group()),
        },
        reply_to: None,
        sender: Some(SenderIdentity {
            channel_type,
            identity: msg.from_id.to_string(),
            display_name: msg.sender_name.clone(),
        }),
    })
}

/// Best-effort decode of a `get_next_event` payload that pulls out the
/// `kind`, `account_id`, `chat_id`, and `msg_id` fields when present.
///
/// Returns `None` for event shapes that do not describe a single message
/// arrival (`Info`, `Warning`, `Error`, `ChatModified`, …). The adapter's
/// forwarder uses this to decide whether to fetch the message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingMsgRef {
    /// Account this message arrived on.
    pub account_id: u64,
    /// Chat the message lives in.
    pub chat_id: i64,
    /// Server message id.
    pub msg_id: i64,
}

/// Inspect a `get_next_event` payload and return an [`IncomingMsgRef`]
/// when it represents an `IncomingMsg` event.
pub fn extract_incoming_msg(event: &Value) -> Option<IncomingMsgRef> {
    let kind = event.get("kind").and_then(Value::as_str)?;
    if kind != "IncomingMsg" {
        return None;
    }
    let account_id = event.get("account_id").and_then(Value::as_u64)?;
    let chat_id = event.get("chat_id").and_then(Value::as_i64)?;
    let msg_id = event.get("msg_id").and_then(Value::as_i64)?;
    Some(IncomingMsgRef {
        account_id,
        chat_id,
        msg_id,
    })
}

/// Render the deltachat `MessageData` object for an outbound text +
/// (optional) attachment payload.
pub fn build_send_payload(text: &str, file: Option<(&str, &str)>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("text".to_owned(), Value::String(text.to_owned()));
    if let Some((path, filename)) = file {
        obj.insert("file".to_owned(), Value::String(path.to_owned()));
        obj.insert("filename".to_owned(), Value::String(filename.to_owned()));
    }
    Value::Object(obj)
}

/// Render the deltachat `MessageData` object for a quoted reply.
pub fn build_quoted_payload(text: &str, quoted_message_id: i64) -> Value {
    json!({
        "text": text,
        "quoted_message_id": quoted_message_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_msg() -> MessageView {
        MessageView {
            id: 100,
            chat_id: 42,
            from_id: 7,
            text: "hello".into(),
            is_info: false,
            view_type: "Text".into(),
            file: None,
            filename: None,
            file_mime: None,
            file_bytes: None,
            download_state: Some("Done".into()),
            timestamp: 1_700_000_000,
            sender_name: Some("Alice".into()),
        }
    }

    fn base_chat() -> ChatInfo {
        ChatInfo {
            id: 42,
            chat_type: 1,
            name: "Alice".into(),
        }
    }

    #[test]
    fn build_platform_id_uses_expected_shape() {
        assert_eq!(build_platform_id(1, 42), "account/1/chat/42");
        assert_eq!(build_platform_id(0, 0), "account/0/chat/0");
    }

    #[test]
    fn parse_platform_id_roundtrips() {
        let s = build_platform_id(3, 17);
        let p = parse_platform_id(&s).unwrap();
        assert_eq!(p.account_id, 3);
        assert_eq!(p.chat_id, 17);
    }

    #[test]
    fn parse_platform_id_rejects_wrong_segments() {
        assert!(parse_platform_id("foo/1/chat/2").is_none());
        assert!(parse_platform_id("account/1/bar/2").is_none());
    }

    #[test]
    fn parse_platform_id_rejects_non_numeric_ids() {
        assert!(parse_platform_id("account/a/chat/2").is_none());
        assert!(parse_platform_id("account/1/chat/b").is_none());
    }

    #[test]
    fn parse_platform_id_rejects_extra_segments() {
        assert!(parse_platform_id("account/1/chat/2/extra").is_none());
    }

    #[test]
    fn parse_platform_id_rejects_missing_segments() {
        assert!(parse_platform_id("account/1/chat").is_none());
        assert!(parse_platform_id("account/1").is_none());
        assert!(parse_platform_id("account").is_none());
        assert!(parse_platform_id("").is_none());
    }

    #[test]
    fn event_to_inbound_basic_text_message() {
        let msg = base_msg();
        let chat = base_chat();
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        assert_eq!(evt.channel_type.as_str(), "deltachat");
        assert_eq!(evt.platform_id, "account/1/chat/42");
        assert!(evt.thread_id.is_none());
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hello");
        assert!(evt.message.content.get("attachment").is_none());
        assert_eq!(evt.message.is_group, Some(false));
        let sender = evt.sender.unwrap();
        assert_eq!(sender.channel_type.as_str(), "deltachat");
        assert_eq!(sender.identity, "7");
        assert_eq!(sender.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn event_to_inbound_with_attachment_path_and_filename() {
        let mut msg = base_msg();
        msg.text = "see attached".into();
        msg.file = Some("/var/data/x.bin".into());
        msg.filename = Some("x.bin".into());
        msg.view_type = "File".into();
        let chat = base_chat();
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        assert_eq!(evt.message.content["text"], "see attached");
        let att = &evt.message.content["attachment"];
        assert_eq!(att["path"], "/var/data/x.bin");
        assert_eq!(att["filename"], "x.bin");
        assert_eq!(att["view_type"], "File");
    }

    #[test]
    fn event_to_inbound_attachment_without_filename_or_view_type() {
        let mut msg = base_msg();
        msg.text = String::new();
        msg.file = Some("/a/b".into());
        msg.filename = None;
        msg.view_type = String::new();
        let chat = base_chat();
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        let att = &evt.message.content["attachment"];
        assert_eq!(att["path"], "/a/b");
        assert!(att.get("filename").is_none());
        assert!(att.get("view_type").is_none());
    }

    #[test]
    fn event_to_inbound_skips_info_messages() {
        let mut msg = base_msg();
        msg.is_info = true;
        let chat = base_chat();
        assert!(event_to_inbound(1, &msg, &chat).is_none());
    }

    #[test]
    fn event_to_inbound_group_chat_sets_is_group_true() {
        let msg = base_msg();
        let chat = ChatInfo {
            id: 42,
            chat_type: 2,
            name: "Team".into(),
        };
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        assert_eq!(evt.message.is_group, Some(true));
    }

    #[test]
    fn event_to_inbound_mailinglist_and_broadcast_are_group_true() {
        let msg = base_msg();
        for chat_type in [3i64, 4] {
            let chat = ChatInfo {
                id: 1,
                chat_type,
                name: "list".into(),
            };
            let evt = event_to_inbound(1, &msg, &chat).unwrap();
            assert_eq!(evt.message.is_group, Some(true));
        }
    }

    #[test]
    fn event_to_inbound_single_chat_sets_is_group_false() {
        let msg = base_msg();
        let chat = base_chat();
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        assert_eq!(evt.message.is_group, Some(false));
    }

    #[test]
    fn event_to_inbound_id_is_message_id_as_string() {
        let mut msg = base_msg();
        msg.id = 999;
        let chat = base_chat();
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        assert_eq!(evt.message.id, "999");
    }

    #[test]
    fn event_to_inbound_no_sender_name_when_absent() {
        let mut msg = base_msg();
        msg.sender_name = None;
        let chat = base_chat();
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        assert!(evt.sender.unwrap().display_name.is_none());
    }

    #[test]
    fn event_to_inbound_timestamp_uses_unix_seconds() {
        let msg = base_msg();
        let chat = base_chat();
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        assert_eq!(evt.message.timestamp.timestamp(), 1_700_000_000);
    }

    #[test]
    fn event_to_inbound_negative_timestamp_falls_back_to_now() {
        let mut msg = base_msg();
        msg.timestamp = i64::MIN;
        let chat = base_chat();
        // Should not panic; produces *some* timestamp.
        let evt = event_to_inbound(1, &msg, &chat).unwrap();
        // Sanity bound — generated DateTime is within +/- a day of now.
        let now = Utc::now().timestamp();
        let evt_ts = evt.message.timestamp.timestamp();
        assert!((evt_ts - now).abs() < 86_400);
    }

    #[test]
    fn extract_incoming_msg_returns_components_when_present() {
        let evt = json!({"kind": "IncomingMsg", "account_id": 1, "chat_id": 42, "msg_id": 100});
        let r = extract_incoming_msg(&evt).unwrap();
        assert_eq!(r.account_id, 1);
        assert_eq!(r.chat_id, 42);
        assert_eq!(r.msg_id, 100);
    }

    #[test]
    fn extract_incoming_msg_none_for_other_kinds() {
        for kind in ["Info", "Warning", "Error", "MsgsChanged", "ChatModified"] {
            let evt = json!({"kind": kind, "msg": "x"});
            assert!(
                extract_incoming_msg(&evt).is_none(),
                "kind {kind} should not match"
            );
        }
    }

    #[test]
    fn extract_incoming_msg_none_when_fields_missing() {
        let evt = json!({"kind": "IncomingMsg", "account_id": 1});
        assert!(extract_incoming_msg(&evt).is_none());
        let evt = json!({"kind": "IncomingMsg"});
        assert!(extract_incoming_msg(&evt).is_none());
    }

    #[test]
    fn build_send_payload_text_only() {
        let v = build_send_payload("hi", None);
        assert_eq!(v["text"], "hi");
        assert!(v.get("file").is_none());
        assert!(v.get("filename").is_none());
    }

    #[test]
    fn build_send_payload_with_file_attaches_path_and_filename() {
        let v = build_send_payload("caption", Some(("/tmp/x.bin", "x.bin")));
        assert_eq!(v["text"], "caption");
        assert_eq!(v["file"], "/tmp/x.bin");
        assert_eq!(v["filename"], "x.bin");
    }

    #[test]
    fn build_quoted_payload_includes_quoted_message_id() {
        let v = build_quoted_payload("reply", 33);
        assert_eq!(v["text"], "reply");
        assert_eq!(v["quoted_message_id"], 33);
    }

    #[test]
    fn parsed_platform_id_clone_and_debug() {
        let p = ParsedPlatformId {
            account_id: 1,
            chat_id: 2,
        };
        let _ = p.clone();
        assert!(format!("{p:?}").contains("ParsedPlatformId"));
    }

    #[test]
    fn incoming_msg_ref_clone_and_debug() {
        let r = IncomingMsgRef {
            account_id: 1,
            chat_id: 2,
            msg_id: 3,
        };
        let _ = r.clone();
        assert!(format!("{r:?}").contains("IncomingMsgRef"));
    }
}
