use crate::channel::{ChannelType, ReplyTo, SenderIdentity};
use crate::id::MessageId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Categories of messages flowing through the system. Each kind is rendered
/// differently for the agent and triggers different host-side handlers on
/// the outbound path.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    /// Normal user message on a channel.
    Chat,
    /// Scheduled task fired by the scheduler.
    Task,
    /// Webhook event from an external service.
    Webhook,
    /// Synthetic system message (CLI request, ack, action handler payload).
    System,
    /// Inter-agent message (delivery routes via the host, not a channel adapter).
    Agent,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Task => "task",
            Self::Webhook => "webhook",
            Self::System => "system",
            Self::Agent => "agent",
        }
    }
}

/// An event handed off from a channel adapter to the router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundEvent {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub thread_id: Option<String>,
    pub message: InboundMessage,
    #[serde(default)]
    pub reply_to: Option<ReplyTo>,
    #[serde(default)]
    pub sender: Option<SenderIdentity>,
}

/// Payload portion of an inbound event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Platform-side identifier (not an `ironclaw-types::MessageId`).
    pub id: String,
    pub kind: MessageKind,
    pub content: serde_json::Value,
    pub timestamp: DateTime<Utc>,
    #[serde(default)]
    pub is_mention: Option<bool>,
    #[serde(default)]
    pub is_group: Option<bool>,
}

/// A message the container's agent emitted that must be delivered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub kind: MessageKind,
    pub content: serde_json::Value,
    #[serde(default)]
    pub files: Vec<OutboundFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundFile {
    pub filename: String,
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        use std::fmt::Write;
        let mut out = String::with_capacity((bytes.len() / 3 + 1) * 4);
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut buf = [0u8; 3];
        for chunk in bytes.chunks(3) {
            for (i, b) in chunk.iter().enumerate() {
                buf[i] = *b;
            }
            let b0 = buf[0];
            let b1 = if chunk.len() > 1 { buf[1] } else { 0 };
            let b2 = if chunk.len() > 2 { buf[2] } else { 0 };
            let n: u32 = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
            let _ = write!(out, "{}", alphabet[((n >> 18) & 63) as usize] as char);
            let _ = write!(out, "{}", alphabet[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                let _ = write!(out, "{}", alphabet[((n >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                let _ = write!(out, "{}", alphabet[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        let bytes = s.trim().as_bytes();
        let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
        let val = |c: u8| -> Result<u8, &'static str> {
            Ok(match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => 0,
                _ => return Err("invalid base64 char"),
            })
        };
        for chunk in bytes.chunks(4) {
            if chunk.len() < 4 {
                return Err(serde::de::Error::custom("bad base64 length"));
            }
            let v0 = val(chunk[0]).map_err(serde::de::Error::custom)?;
            let v1 = val(chunk[1]).map_err(serde::de::Error::custom)?;
            let v2 = val(chunk[2]).map_err(serde::de::Error::custom)?;
            let v3 = val(chunk[3]).map_err(serde::de::Error::custom)?;
            let n: u32 =
                (u32::from(v0) << 18) | (u32::from(v1) << 12) | (u32::from(v2) << 6) | u32::from(v3);
            out.push(((n >> 16) & 0xFF) as u8);
            if chunk[2] != b'=' {
                out.push(((n >> 8) & 0xFF) as u8);
            }
            if chunk[3] != b'=' {
                out.push((n & 0xFF) as u8);
            }
        }
        Ok(out)
    }
}

/// A row that has been written to `messages_in`. Modules and routers receive
/// these to dispatch system actions, agent-to-agent messages, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageInRow {
    pub id: MessageId,
    pub seq: i64,
    pub kind: MessageKind,
    pub timestamp: DateTime<Utc>,
    pub status: String,
    pub process_after: Option<DateTime<Utc>>,
    pub recurrence: Option<String>,
    pub series_id: Option<String>,
    pub tries: u32,
    pub trigger: bool,
    pub platform_id: Option<String>,
    pub channel_type: Option<ChannelType>,
    pub thread_id: Option<String>,
    pub content: serde_json::Value,
    pub source_session_id: Option<String>,
    pub on_wake: bool,
}

/// A row read from `messages_out`. The host's delivery loop iterates these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageOutRow {
    pub id: MessageId,
    pub seq: i64,
    pub in_reply_to: Option<MessageId>,
    pub timestamp: DateTime<Utc>,
    pub deliver_after: Option<DateTime<Utc>>,
    pub recurrence: Option<String>,
    pub kind: MessageKind,
    pub platform_id: Option<String>,
    pub channel_type: Option<ChannelType>,
    pub thread_id: Option<String>,
    pub content: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_kind_serde() {
        for kind in [
            MessageKind::Chat,
            MessageKind::Task,
            MessageKind::Webhook,
            MessageKind::System,
            MessageKind::Agent,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: MessageKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back, "roundtrip failed for {kind:?}");
        }
    }

    #[test]
    fn outbound_file_base64_roundtrip() {
        let original = OutboundFile {
            filename: "x.bin".into(),
            data: (0u8..=255).collect(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: OutboundFile = serde_json::from_str(&json).unwrap();
        assert_eq!(original.data, back.data);
    }

    #[test]
    fn inbound_event_roundtrip() {
        let evt = InboundEvent {
            channel_type: ChannelType::new("telegram"),
            platform_id: "chat-123".into(),
            thread_id: None,
            message: InboundMessage {
                id: "msg-9".into(),
                kind: MessageKind::Chat,
                content: json!({"text":"hi"}),
                timestamp: Utc::now(),
                is_mention: Some(true),
                is_group: None,
            },
            reply_to: None,
            sender: None,
        };
        let json = serde_json::to_string(&evt).unwrap();
        let back: InboundEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(evt.platform_id, back.platform_id);
        assert_eq!(evt.message.kind, back.message.kind);
    }
}
