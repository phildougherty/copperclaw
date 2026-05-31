//! Format `messages_in` rows into the text body of one provider turn.
//!
//! When multiple pending rows are picked up in a single poll we coalesce
//! them into a single user-side message. The format is human-readable and
//! deterministic so the model can rely on it.

use ironclaw_types::MessageInRow;

/// Output of [`format_messages`] — the user-side prompt plus the picked
/// rows in stable order. The caller persists `rows` (e.g. to ack each one).
#[derive(Debug, Clone)]
pub struct FormattedTurn {
    /// User-facing prompt text. Pass into [`ironclaw_providers::HistoryMessage::User`].
    pub prompt: String,
    /// Source rows in chronological (seq-ascending) order.
    pub rows: Vec<MessageInRow>,
}

/// Format `messages` into a single user-side prompt.
///
/// The output groups every message under a `[channel/platform/thread]`
/// header (or `[system]` for synthetic system messages) followed by the
/// message body. The function consumes the input and returns it back via
/// `FormattedTurn::rows` in seq-ascending order so the caller can drive
/// processing in the right sequence.
#[must_use]
pub fn format_messages(mut messages: Vec<MessageInRow>) -> FormattedTurn {
    messages.sort_by_key(|m| m.seq);
    let mut prompt = String::new();
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            prompt.push_str("\n\n");
        }
        prompt.push('[');
        prompt.push_str(m.kind.as_str());
        if let Some(ct) = &m.channel_type {
            prompt.push_str(" channel=");
            prompt.push_str(ct.as_str());
        }
        if let Some(pid) = &m.platform_id {
            prompt.push_str(" platform=");
            prompt.push_str(pid);
        }
        if let Some(tid) = &m.thread_id {
            prompt.push_str(" thread=");
            prompt.push_str(tid);
        }
        prompt.push_str(" seq=");
        prompt.push_str(&m.seq.to_string());
        prompt.push(']');
        prompt.push('\n');
        prompt.push_str(&body_text(&m.content));
    }
    FormattedTurn {
        prompt,
        rows: messages,
    }
}

/// Best-effort extraction of a text body from a stored JSON content blob.
///
/// Recognised shapes (in order):
/// 1. `{ "text": "..." }` — chat/system messages.
/// 2. `{ "prompt": "..." }` — scheduled task fires.
/// 3. Anything else — the JSON is rendered as a compact string.
fn body_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.get("text").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(s) = content.get("prompt").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    content.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ironclaw_types::{ChannelType, MessageId, MessageInRow, MessageKind};
    use serde_json::json;

    fn row(seq: i64, content: serde_json::Value) -> MessageInRow {
        MessageInRow {
            id: MessageId::new(),
            seq,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            status: "pending".into(),
            process_after: None,
            recurrence: None,
            series_id: None,
            tries: 0,
            trigger: true,
            platform_id: Some("chat-1".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            content,
            source_session_id: None,
            on_wake: false,
            reply_to: None,
            is_group: None,
        }
    }

    #[test]
    fn single_message_renders_header_and_text() {
        let ft = format_messages(vec![row(2, json!({"text": "hi there"}))]);
        assert!(ft.prompt.contains("[chat"));
        assert!(ft.prompt.contains("channel=cli"));
        assert!(ft.prompt.contains("platform=chat-1"));
        assert!(ft.prompt.contains("seq=2"));
        assert!(ft.prompt.contains("hi there"));
        assert_eq!(ft.rows.len(), 1);
    }

    #[test]
    fn multiple_messages_separated_by_blank_line_and_seq_ordered() {
        let ft = format_messages(vec![
            row(4, json!({"text": "later"})),
            row(2, json!({"text": "earlier"})),
        ]);
        let pos_earlier = ft.prompt.find("earlier").unwrap();
        let pos_later = ft.prompt.find("later").unwrap();
        assert!(pos_earlier < pos_later);
        assert!(ft.prompt.contains("\n\n["));
        assert_eq!(ft.rows[0].seq, 2);
        assert_eq!(ft.rows[1].seq, 4);
    }

    #[test]
    fn falls_back_to_prompt_field_for_tasks() {
        let ft = format_messages(vec![row(2, json!({"prompt": "wake task"}))]);
        assert!(ft.prompt.contains("wake task"));
    }

    #[test]
    fn falls_back_to_compact_json_when_unknown_shape() {
        let ft = format_messages(vec![row(2, json!({"event": "x"}))]);
        // JSON-rendered to_string is what we expect.
        assert!(ft.prompt.contains("{\"event\":\"x\"}"));
    }

    #[test]
    fn empty_input_renders_empty_prompt() {
        let ft = format_messages(vec![]);
        assert!(ft.prompt.is_empty());
        assert!(ft.rows.is_empty());
    }

    #[test]
    fn missing_channel_or_thread_omitted_from_header() {
        let mut m = row(2, json!({"text": "x"}));
        m.channel_type = None;
        m.platform_id = None;
        m.thread_id = None;
        let ft = format_messages(vec![m]);
        assert!(!ft.prompt.contains("channel="));
        assert!(!ft.prompt.contains("platform="));
        assert!(!ft.prompt.contains("thread="));
        assert!(ft.prompt.contains("seq=2"));
    }

    #[test]
    fn thread_id_appears_when_set() {
        let mut m = row(2, json!({"text": "x"}));
        m.thread_id = Some("t-99".into());
        let ft = format_messages(vec![m]);
        assert!(ft.prompt.contains("thread=t-99"));
    }

    #[test]
    fn system_kind_header_renders_as_system() {
        let mut m = row(2, json!({"text": "ack"}));
        m.kind = MessageKind::System;
        let ft = format_messages(vec![m]);
        assert!(ft.prompt.starts_with("[system"));
    }
}
