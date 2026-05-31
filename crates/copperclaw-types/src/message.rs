use crate::channel::{ChannelType, ReplyTo, SenderIdentity};
use crate::id::MessageId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Categories of messages flowing through the system. Each kind is rendered
/// differently for the agent and triggers different host-side handlers on
/// the outbound path.
// Wire-format contract: every variant's serde tag matches `as_str()` /
// `parse_str()` exactly. `snake_case` is required (not `lowercase`) so
// multi-word variants like `TodoList` serialise as `"todo_list"` —
// matching the DB column form and round-tripping through JSON cleanly.
// Single-word variants serialise identically under `lowercase` or
// `snake_case`, so this change is backward-compatible for every kind
// except `TodoList` (which was previously serialised as `"todolist"`
// and could not be parsed back via `parse_str`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Structured card payload. Routed through the same delivery path as
    /// `Chat`, but the host/delivery service hands the card body to the
    /// adapter's `deliver_card` hook so adapters with native card support
    /// (Telegram inline keyboards, Slack Block Kit, Discord embeds,
    /// Google Chat cards v2, …) can render it structurally. Adapters
    /// without a native renderer fall back to a text rendering for free
    /// via the default `deliver_card` impl on `ChannelAdapter`.
    ///
    /// Wave 2 of the cards rollout starts writing rows with this kind
    /// from the runner / MCP `send_card` tool.
    Card,
    /// Compact tool-progress "chip" — what's rendered as
    /// `[shell] cargo check` (Running) → `[shell] cargo check — passed
    /// (0.4s)` (Done) by the adapter's native `deliver_breadcrumb`
    /// hook. The row's `content.breadcrumb` carries the canonical
    /// `Breadcrumb` payload (see `copperclaw_channels_core::Breadcrumb`).
    ///
    /// Routed via the same delivery loop as `Card` rows but with a
    /// distinct `dispatch_breadcrumb` path so adapters with rich
    /// native rendering (Telegram HTML `<code>`, Slack Block Kit
    /// `context`, Discord embed footer, Google Chat cards v2, Matrix
    /// `m.notice` with `<code>`) can draw a small chip — and edit it
    /// in place once the tool finishes — instead of bloating chat
    /// with a fresh row every tool call.
    Breadcrumb,
    /// Structured agent-todo checklist payload — the slice-3.2 "live,
    /// pinned checklist" surface. The runner emits one of these via
    /// `MessageKind::TodoList` after every `todo_add` / `todo_update`
    /// / `todo_delete` MCP-tool mutation, carrying the *full*
    /// post-mutation list in `content.todo_list` as the canonical
    /// `TodoList` payload (see `copperclaw_channels_core::TodoList`).
    ///
    /// Routed via a dedicated `dispatch_todo_list` path so adapters
    /// with rich native rendering (Telegram `editMessageText`
    /// `MarkdownV2`, Slack Block Kit `section` + checkbox accessory,
    /// Discord embed fields, Google Chat Cards v2 `decoratedText`,
    /// Matrix `m.replace`) can draw an inline checklist — and edit it
    /// in place on every mutation — instead of spamming chat with a
    /// fresh full-list row each turn. Adapters that support pinning
    /// (Telegram `pinChatMessage`, Slack `pins.add`, Matrix
    /// `m.room.pinned_events`) pin on first emit and unpin once every
    /// item is `Completed`.
    TodoList,
    /// Structured file-edit diff payload — the slice-3.1 "diff card"
    /// surface. The runner emits one of these via `MessageKind::Diff`
    /// after a successful `edit_file` / `multi_edit` / `apply_patch` /
    /// `write_file` write, carrying the structured diff in
    /// `content.diff` as the canonical `DiffCard` payload (see
    /// `copperclaw_channels_core::DiffCard`).
    ///
    /// Routed via a dedicated `dispatch_diff` path so adapters with
    /// rich native rendering (Telegram `MarkdownV2` ` ```diff ``` `,
    /// Slack Block Kit `rich_text_preformatted` per hunk, Discord
    /// embed with `description` fenced block + color, Google Chat
    /// Cards v2 `decoratedText` per hunk, Matrix `<pre><code
    /// class="language-diff">…</code></pre>`) can draw a real diff with
    /// `+` / `-` gutters — emitted *alongside* the existing
    /// `MessageKind::Breadcrumb` chip, not in place of it
    /// (breadcrumb = "what tool", `DiffCard` = "what changed").
    /// Adapters without a native renderer fall back to a unified-diff
    /// text body via the default `deliver_diff` impl on
    /// `ChannelAdapter`.
    ///
    /// Diff cards are immutable post-emit: if the same file is edited
    /// again the runner emits a fresh card; there is no edit-in-place
    /// machinery for this kind.
    Diff,
    /// Structured host-emitted error card — the slice-3.3
    /// "visually-distinct error" surface. Carries the canonical
    /// `ErrorCard` payload (see `copperclaw_channels_core::ErrorCard`)
    /// in `content.error`.
    ///
    /// Emitted by the HOST, not the model — three sites:
    ///
    /// 1. The runner's terminal-failure-apology pipeline (provider
    ///    retry exhaustion / fatal turn).
    /// 2. The host delivery service when retry exhausts on an outbound
    ///    row (in addition to the existing `delivered.status="failed"`
    ///    row — operators still see the row in `cclaw
    ///    dropped-messages`; the user additionally sees a visible
    ///    error in chat).
    /// 3. Tool handlers that bubble an internal error the runner
    ///    cannot transparently retry.
    ///
    /// Routed via a dedicated `dispatch_error` path so adapters with
    /// rich native rendering (Slack `attachments.color: "danger"`,
    /// Discord embed `color = 0xE74C3C`, Matrix `<font color="red">`,
    /// Telegram bold HTML, Google Chat decorated icon) draw a red /
    /// emphasised affordance instead of looking like normal chat.
    /// Adapters without a native renderer fall back to a
    /// `[ERROR: <kind>] <title>\n<summary>` text rendering via the
    /// default `deliver_error` impl on `ChannelAdapter`.
    ///
    /// Error cards are immutable post-emit: there is no edit-in-place
    /// machinery — once a failure happens its receipt sticks.
    Error,
    /// Opt-in structured reasoning block — the slice-3.5 "thinking
    /// surface". Carries the canonical `ThinkingBlock` payload (see
    /// `copperclaw_channels_core::ThinkingBlock`) in `content.thinking`.
    ///
    /// Emitted by the runner when the provider streams a `thinking`
    /// (or `redacted_thinking`) content block AND the per-group
    /// `surface_thinking` config knob is on. The default is off —
    /// surfacing model chain-of-thought has privacy implications
    /// (mid-thought speculation about the user, debugging notes the
    /// model didn't intend the user to see). Operators opt in per-group
    /// via `cclaw groups config edit <id>`.
    ///
    /// Routed via a dedicated `dispatch_thinking` path so adapters with
    /// rich native rendering (Telegram `<blockquote expandable>`, Slack
    /// `context` block, Discord muted-grey embed, Google Chat Cards v2
    /// `collapsibleSection`, Matrix `<details>`) render the block
    /// collapsed by default — the user opens the disclosure widget
    /// only when they want to read the reasoning. Adapters without a
    /// native renderer fall back to a `[reasoning]`-headered quoted
    /// text block via the default `deliver_thinking` impl on
    /// `ChannelAdapter`.
    ///
    /// Thinking blocks are point-in-time receipts — immutable
    /// post-emit; there is no edit-in-place machinery (no
    /// `update_thinking` system action to mirror `update_breadcrumb`).
    /// `strip_reasoning_blocks` (the orthogonal sanitiser that
    /// removes inline `<thinking>` prose from `Chat` rows) remains
    /// unchanged: that path scrubs prose contamination from the chat
    /// reply, this path emits structured reasoning as its own row.
    Thinking,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Task => "task",
            Self::Webhook => "webhook",
            Self::System => "system",
            Self::Agent => "agent",
            Self::Card => "card",
            Self::Breadcrumb => "breadcrumb",
            Self::TodoList => "todo_list",
            Self::Diff => "diff",
            Self::Error => "error",
            Self::Thinking => "thinking",
        }
    }

    /// Parse a column-stored kind string back into the enum. Returns
    /// `None` for unknown strings so callers can decide how to surface
    /// the error (the database tables return a custom rusqlite error).
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "chat" => Self::Chat,
            "task" => Self::Task,
            "webhook" => Self::Webhook,
            "system" => Self::System,
            "agent" => Self::Agent,
            "card" => Self::Card,
            "breadcrumb" => Self::Breadcrumb,
            "todo_list" => Self::TodoList,
            "diff" => Self::Diff,
            "error" => Self::Error,
            "thinking" => Self::Thinking,
            _ => return None,
        })
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
    /// Platform-side identifier (not an `copperclaw-types::MessageId`).
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
    /// Platform-side parent message id when the wire event was a reply
    /// (Telegram `message.reply_to_message`, Slack `thread_ts` reply,
    /// Discord `referenced_message`, Matrix `m.in_reply_to`, ...).
    /// Populated by the channel adapter onto `InboundEvent.reply_to` and
    /// persisted through the router into `messages_in.reply_to`. `None`
    /// when the message is a top-level send or the channel doesn't carry
    /// a reply link.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Whether the originating venue is a group chat (vs. a 1-on-1 DM).
    /// Populated by the channel adapter onto `InboundEvent.message.is_group`
    /// and persisted through the router into `messages_in.is_group`. `None`
    /// when the channel doesn't distinguish (CLI, file-watcher, webhooks).
    #[serde(default)]
    pub is_group: Option<bool>,
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
            MessageKind::Card,
            MessageKind::Breadcrumb,
            MessageKind::Diff,
            MessageKind::TodoList,
            MessageKind::Error,
            MessageKind::Thinking,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: MessageKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back, "roundtrip failed for {kind:?}");
        }
    }

    #[test]
    fn message_kind_error_is_lowercase_error_in_json() {
        // `serde(rename_all = "lowercase")` keeps the JSON tag flat —
        // confirms the wire shape for the slice-3.3 host-emitted
        // error-card surface.
        let s = serde_json::to_string(&MessageKind::Error).unwrap();
        assert_eq!(s, r#""error""#);
        assert_eq!(MessageKind::Error.as_str(), "error");
        assert_eq!(MessageKind::parse_str("error"), Some(MessageKind::Error));
    }

    #[test]
    fn message_kind_diff_is_lowercase_diff_in_json() {
        let s = serde_json::to_string(&MessageKind::Diff).unwrap();
        assert_eq!(s, r#""diff""#);
        assert_eq!(MessageKind::Diff.as_str(), "diff");
        assert_eq!(MessageKind::parse_str("diff"), Some(MessageKind::Diff));
    }

    #[test]
    fn message_kind_todo_list_round_trips_via_str_and_serde() {
        // DB column round-trip via as_str / parse_str — `todo_list`
        // (snake_case) because the DB store uses snake_case for all
        // multi-word kinds.
        assert_eq!(MessageKind::TodoList.as_str(), "todo_list");
        assert_eq!(
            MessageKind::parse_str("todo_list"),
            Some(MessageKind::TodoList)
        );
        // JSON round-trip via serde — `rename_all = "snake_case"` keeps
        // the wire tag aligned with the DB column form (`"todo_list"`)
        // so anything round-tripped through JSON parses back via
        // `parse_str`. Prior to this fix the wire tag was `"todolist"`
        // (no underscore) and `parse_str("todolist")` returned None,
        // silently losing the kind on any JSON round-trip.
        let json = serde_json::to_string(&MessageKind::TodoList).unwrap();
        assert_eq!(json, r#""todo_list""#);
        let back: MessageKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, MessageKind::TodoList);
    }

    #[test]
    fn message_kind_serde_tag_matches_as_str_for_every_variant() {
        // Pins the new contract: the JSON wire tag for every variant is
        // exactly the string `as_str()` returns, and the value
        // round-trips through `to_string` -> `from_str` -> `parse_str`
        // without loss. Regression guard for the slice-3.2 TodoList
        // divergence (was `"todolist"` on the wire, `"todo_list"` in
        // the DB column).
        for kind in [
            MessageKind::Chat,
            MessageKind::Task,
            MessageKind::Webhook,
            MessageKind::System,
            MessageKind::Agent,
            MessageKind::Card,
            MessageKind::Breadcrumb,
            MessageKind::TodoList,
            MessageKind::Diff,
            MessageKind::Error,
            MessageKind::Thinking,
        ] {
            let expected = kind.as_str();
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(
                json,
                format!("\"{expected}\""),
                "wire tag for {kind:?} drifted from as_str()"
            );
            let back: MessageKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind, "JSON round-trip lost {kind:?}");
            // And the wire tag must parse back via parse_str — the
            // exact path that was broken for TodoList before this fix.
            let stripped = json.trim_matches('"');
            assert_eq!(
                MessageKind::parse_str(stripped),
                Some(kind),
                "parse_str rejected wire tag for {kind:?}"
            );
        }
    }

    #[test]
    fn message_kind_as_str_and_parse_str_roundtrip() {
        for kind in [
            MessageKind::Chat,
            MessageKind::Task,
            MessageKind::Webhook,
            MessageKind::System,
            MessageKind::Agent,
            MessageKind::Card,
            MessageKind::Breadcrumb,
            MessageKind::Diff,
            MessageKind::TodoList,
            MessageKind::Error,
            MessageKind::Thinking,
        ] {
            let s = kind.as_str();
            assert_eq!(MessageKind::parse_str(s), Some(kind));
        }
        assert_eq!(MessageKind::parse_str("nonsense"), None);
    }

    #[test]
    fn message_kind_thinking_is_lowercase_thinking_in_json() {
        // `serde(rename_all = "lowercase")` keeps the JSON tag flat —
        // confirms the wire shape for the slice-3.5 opt-in
        // thinking-block surface.
        let s = serde_json::to_string(&MessageKind::Thinking).unwrap();
        assert_eq!(s, r#""thinking""#);
        assert_eq!(MessageKind::Thinking.as_str(), "thinking");
        assert_eq!(
            MessageKind::parse_str("thinking"),
            Some(MessageKind::Thinking)
        );
    }

    #[test]
    fn message_kind_card_is_lowercase_card_in_json() {
        let s = serde_json::to_string(&MessageKind::Card).unwrap();
        assert_eq!(s, r#""card""#);
        assert_eq!(MessageKind::Card.as_str(), "card");
    }

    #[test]
    fn message_kind_breadcrumb_is_lowercase_breadcrumb_in_json() {
        let s = serde_json::to_string(&MessageKind::Breadcrumb).unwrap();
        assert_eq!(s, r#""breadcrumb""#);
        assert_eq!(MessageKind::Breadcrumb.as_str(), "breadcrumb");
        assert_eq!(
            MessageKind::parse_str("breadcrumb"),
            Some(MessageKind::Breadcrumb)
        );
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
