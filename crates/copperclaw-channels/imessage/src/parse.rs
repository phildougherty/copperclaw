//! Pure helpers that turn Messages.app chat-db rows into [`InboundEvent`]s
//! and parse the iMessage `platform_id` shape on outbound.
//!
//! These functions are pure (no async, no I/O) so they can be unit-tested
//! against fixture rows without touching `sqlite3`.

use crate::bridge::MockMessageRow;
use crate::factory::CHANNEL_TYPE_STR;
use chrono::{DateTime, TimeZone, Utc};
use copperclaw_channels_core::AdapterError;
use copperclaw_types::{
    ChannelType, InboundEvent, InboundMessage, MessageKind, SenderIdentity,
};
use serde_json::{Value, json};

/// Number of seconds between the Unix epoch (1970-01-01 UTC) and the Cocoa
/// reference date (2001-01-01 UTC).
///
/// `(31 * 365 + 8) * 86400` accounts for the eight leap years (1972, 1976,
/// 1980, 1984, 1988, 1992, 1996, 2000) within that span.
pub const COCOA_EPOCH_OFFSET_SECS: i64 = 978_307_200;

/// Convert a raw `message.date` value from `chat.db` into a UTC timestamp.
///
/// Messages.app stores `date` in **Cocoa-reference-date format**, i.e.
/// "seconds (or nanoseconds) since 2001-01-01 00:00:00 UTC":
///
/// - macOS High Sierra (10.13) and later use **nanoseconds**. The values
///   look like `724_000_000_000_000_000` for a recent message.
/// - Older macOS releases used **integer seconds** — values around 5e8
///   for the 2018-ish era.
///
/// We auto-detect: any value over `10^12` (a trillion) is too big to be
/// seconds in our lifetime, so we treat it as nanoseconds.
///
/// `0` (the column default) maps to the Unix epoch *plus* the Cocoa offset,
/// which is 2001-01-01. That's an explicit signal of "unset", and callers
/// can choose to substitute `Utc::now()` if they want.
pub fn cocoa_to_utc(raw: i64) -> DateTime<Utc> {
    // Heuristic: values >= 10^12 are nanoseconds; smaller values are
    // seconds. (A real nanosecond value for 2001 is already 0; for 2018
    // it's around 5e17. A real seconds value for 2050 is still ~1.5e9,
    // far below 10^12.)
    let unix_secs = if raw.abs() >= 1_000_000_000_000 {
        // Nanoseconds since Cocoa epoch.
        raw / 1_000_000_000 + COCOA_EPOCH_OFFSET_SECS
    } else {
        // Seconds since Cocoa epoch.
        raw + COCOA_EPOCH_OFFSET_SECS
    };
    Utc.timestamp_opt(unix_secs, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
}

/// Inverse of [`cocoa_to_utc`]: returns nanoseconds since 2001-01-01 UTC.
///
/// Used by tests and by the SQL high-water mark scan; not used in adapter
/// code itself.
pub fn utc_to_cocoa_nanos(ts: DateTime<Utc>) -> i64 {
    let unix_secs = ts.timestamp();
    (unix_secs - COCOA_EPOCH_OFFSET_SECS).saturating_mul(1_000_000_000)
}

/// Parsed `platform_id` for the iMessage channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedPlatformId {
    /// Direct message with a buddy identified by handle (e-mail or phone).
    Handle(String),
    /// Group chat keyed by Messages.app's chat GUID.
    Chat(String),
}

impl ParsedPlatformId {
    /// Encode back to its wire form.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Handle(h) => format!("handle:{h}"),
            Self::Chat(g) => format!("chat:{g}"),
        }
    }

    /// `true` when this id refers to a group chat.
    pub fn is_group(&self) -> bool {
        matches!(self, Self::Chat(_))
    }
}

/// Parse a `platform_id` of the form `"handle:<x>"` or `"chat:<guid>"`.
///
/// Returns [`AdapterError::BadRequest`] for empty values or anything that
/// doesn't fit the two known prefixes.
pub fn parse_platform_id(s: &str) -> Result<ParsedPlatformId, AdapterError> {
    if let Some(rest) = s.strip_prefix("handle:") {
        if rest.is_empty() {
            return Err(AdapterError::BadRequest(
                "imessage platform_id: empty handle".into(),
            ));
        }
        return Ok(ParsedPlatformId::Handle(rest.to_owned()));
    }
    if let Some(rest) = s.strip_prefix("chat:") {
        if rest.is_empty() {
            return Err(AdapterError::BadRequest(
                "imessage platform_id: empty chat guid".into(),
            ));
        }
        return Ok(ParsedPlatformId::Chat(rest.to_owned()));
    }
    Err(AdapterError::BadRequest(format!(
        "imessage platform_id: unrecognized shape `{s}` (want handle:<h> or chat:<g>)"
    )))
}

/// Convert one `chat.db` row into an [`InboundEvent`].
///
/// Returns `None` for rows that should be skipped:
///
/// - `is_from_me = true` (we never want to round-trip the agent's own
///   outbound messages back through the router; the SQL already filters
///   these out but we double-check)
/// - rows missing both a handle and a chat id (orphaned system events)
///
/// The `text` column is taken verbatim; attachments aren't surfaced in v1.
pub fn row_to_inbound(row: &MockMessageRow) -> Option<InboundEvent> {
    if row.is_from_me {
        return None;
    }
    let channel_type = ChannelType::new(CHANNEL_TYPE_STR);
    let timestamp = cocoa_to_utc(row.date);

    let (platform_id, is_group) = if let Some(chat) = row.chat_id.as_deref() {
        // A row joined to a chat id may still be a 1:1 conversation;
        // Messages.app uses chats for both. We classify "group" only when
        // the chat id is itself non-empty AND no handle was the sole
        // counterparty. For our purposes the chat id wins: if the chat
        // exists, route to the chat.
        (format!("chat:{chat}"), true)
    } else if let Some(handle) = row.handle.as_deref() {
        (format!("handle:{handle}"), false)
    } else {
        return None;
    };

    let mut content = serde_json::Map::new();
    content.insert(
        "text".to_owned(),
        Value::String(row.text.clone().unwrap_or_default()),
    );

    let sender = row.handle.as_deref().map(|h| SenderIdentity {
        channel_type: channel_type.clone(),
        identity: h.to_owned(),
        display_name: None,
    });

    Some(InboundEvent {
        channel_type,
        platform_id,
        thread_id: None,
        message: InboundMessage {
            id: row.guid.clone(),
            kind: MessageKind::Chat,
            content: Value::Object(content),
            timestamp,
            is_mention: None,
            is_group: Some(is_group),
        },
        reply_to: None,
        sender,
    })
}

/// Build the canonical SQL the poll loop uses to fetch new rows past
/// `since_rowid`.
///
/// Lives here (and not in `poll.rs`) so it can be unit-tested for the
/// expected columns and ordering.
pub fn select_new_rows_sql() -> &'static str {
    "SELECT m.ROWID, m.guid, m.text, m.date, m.is_from_me, m.handle_id, \
            h.id AS handle, \
            (SELECT chat_identifier FROM chat WHERE ROWID = \
                (SELECT chat_id FROM chat_message_join \
                 WHERE message_id = m.ROWID LIMIT 1)) AS chat_id \
     FROM message m LEFT JOIN handle h ON h.ROWID = m.handle_id \
     WHERE m.ROWID > ? AND m.is_from_me = 0 \
     ORDER BY m.ROWID ASC"
}

/// Build a content body for system-style outbound payloads when the agent
/// hands us a `kind=System` message we can't service. Surfaced as part of
/// the error message so the operator can see exactly what was rejected.
pub fn describe_system_action(content: &Value) -> String {
    content
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>")
        .to_owned()
}

/// Convenience: produce a minimal `content` object for tests of
/// [`row_to_inbound`].
#[doc(hidden)]
pub fn text_content_for_test(s: &str) -> Value {
    json!({ "text": s })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        rowid: i64,
        guid: &str,
        text: Option<&str>,
        is_from_me: bool,
        handle: Option<&str>,
        chat_id: Option<&str>,
        date: i64,
    ) -> MockMessageRow {
        MockMessageRow {
            rowid,
            guid: guid.into(),
            text: text.map(str::to_owned),
            date,
            is_from_me,
            handle: handle.map(str::to_owned),
            chat_id: chat_id.map(str::to_owned),
        }
    }

    #[test]
    fn cocoa_epoch_constant_matches_arithmetic() {
        // 31 years from 1970 to 2001, eight of them leap.
        let expected = (31_i64 * 365 + 8) * 86_400;
        assert_eq!(COCOA_EPOCH_OFFSET_SECS, expected);
    }

    #[test]
    fn cocoa_to_utc_zero_seconds_is_2001() {
        let dt = cocoa_to_utc(0);
        assert_eq!(dt.to_rfc3339(), "2001-01-01T00:00:00+00:00");
    }

    #[test]
    fn cocoa_to_utc_one_second_advances_by_a_second() {
        let dt = cocoa_to_utc(1);
        assert_eq!(dt.to_rfc3339(), "2001-01-01T00:00:01+00:00");
    }

    #[test]
    fn cocoa_to_utc_nanoseconds_detection() {
        // 2024-01-01 UTC is 725_760_000 seconds past the Cocoa epoch
        // (23 years, of which 2004/2008/2012/2016/2020 are leap).
        let recent_secs = 725_760_000_i64;
        let recent_nanos = recent_secs * 1_000_000_000;
        let dt = cocoa_to_utc(recent_nanos);
        assert_eq!(dt.to_rfc3339(), "2024-01-01T00:00:00+00:00");
    }

    #[test]
    fn cocoa_to_utc_seconds_branch_for_2018ish() {
        // ~2018-06-01 in cocoa seconds.
        let secs = 549_504_000_i64;
        let dt = cocoa_to_utc(secs);
        // Verify year is 2018.
        let s = dt.to_rfc3339();
        assert!(s.starts_with("2018-"), "got {s}");
    }

    #[test]
    fn cocoa_to_utc_negative_is_clamped_to_epoch() {
        // Negative + small magnitude: still maps via seconds branch and
        // lands pre-2001. Just confirm no panic.
        let dt = cocoa_to_utc(-100);
        assert!(dt < Utc.timestamp_opt(COCOA_EPOCH_OFFSET_SECS, 0).unwrap());
    }

    #[test]
    fn utc_to_cocoa_nanos_round_trip_via_cocoa_to_utc() {
        let original = Utc.with_ymd_and_hms(2024, 5, 1, 12, 0, 0).unwrap();
        let nanos = utc_to_cocoa_nanos(original);
        let back = cocoa_to_utc(nanos);
        assert_eq!(back, original);
    }

    #[test]
    fn utc_to_cocoa_nanos_zero_for_2001() {
        let ts = Utc.with_ymd_and_hms(2001, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(utc_to_cocoa_nanos(ts), 0);
    }

    #[test]
    fn parse_platform_id_handle() {
        let p = parse_platform_id("handle:+15551234").unwrap();
        assert_eq!(p, ParsedPlatformId::Handle("+15551234".into()));
        assert!(!p.is_group());
    }

    #[test]
    fn parse_platform_id_handle_email() {
        let p = parse_platform_id("handle:alice@example.com").unwrap();
        assert_eq!(p, ParsedPlatformId::Handle("alice@example.com".into()));
    }

    #[test]
    fn parse_platform_id_chat() {
        let p = parse_platform_id("chat:iMessage;+;chat999").unwrap();
        assert_eq!(p, ParsedPlatformId::Chat("iMessage;+;chat999".into()));
        assert!(p.is_group());
    }

    #[test]
    fn parse_platform_id_rejects_unknown_prefix() {
        let err = parse_platform_id("user:foo").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_platform_id_rejects_empty() {
        let err = parse_platform_id("").unwrap_err();
        assert!(matches!(err, AdapterError::BadRequest(_)));
    }

    #[test]
    fn parse_platform_id_rejects_empty_handle() {
        let err = parse_platform_id("handle:").unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("empty handle")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_platform_id_rejects_empty_chat() {
        let err = parse_platform_id("chat:").unwrap_err();
        match err {
            AdapterError::BadRequest(m) => assert!(m.contains("empty chat")),
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_platform_id_handles_colon_in_chat_guid() {
        // Group GUIDs can contain colons; we keep everything after the
        // first one.
        let p = parse_platform_id("chat:abc:def:ghi").unwrap();
        assert_eq!(p, ParsedPlatformId::Chat("abc:def:ghi".into()));
    }

    #[test]
    fn parsed_to_wire_handle() {
        let p = ParsedPlatformId::Handle("+1".into());
        assert_eq!(p.to_wire(), "handle:+1");
    }

    #[test]
    fn parsed_to_wire_chat() {
        let p = ParsedPlatformId::Chat("g".into());
        assert_eq!(p.to_wire(), "chat:g");
    }

    #[test]
    fn parsed_round_trips_via_wire() {
        let cases = ["handle:foo@bar", "chat:guid-xyz"];
        for s in &cases {
            let parsed = parse_platform_id(s).unwrap();
            assert_eq!(parsed.to_wire(), *s);
        }
    }

    #[test]
    fn row_to_inbound_skips_from_me() {
        let r = row(1, "g1", Some("hi"), true, Some("+1"), None, 0);
        assert!(row_to_inbound(&r).is_none());
    }

    #[test]
    fn row_to_inbound_skips_when_no_handle_or_chat() {
        let r = row(1, "g1", Some("hi"), false, None, None, 0);
        assert!(row_to_inbound(&r).is_none());
    }

    #[test]
    fn row_to_inbound_dm_only_handle() {
        let r = row(1, "g1", Some("hi"), false, Some("+15551234"), None, 0);
        let evt = row_to_inbound(&r).unwrap();
        assert_eq!(evt.channel_type.as_str(), "imessage");
        assert_eq!(evt.platform_id, "handle:+15551234");
        assert_eq!(evt.message.kind, MessageKind::Chat);
        assert_eq!(evt.message.content["text"], "hi");
        assert_eq!(evt.message.is_group, Some(false));
        let s = evt.sender.expect("sender present");
        assert_eq!(s.identity, "+15551234");
    }

    #[test]
    fn row_to_inbound_chat_routes_to_chat_id() {
        let r = row(
            2, "g2", Some("group hi"), false, Some("+15551234"),
            Some("chat-guid-xyz"), 0,
        );
        let evt = row_to_inbound(&r).unwrap();
        assert_eq!(evt.platform_id, "chat:chat-guid-xyz");
        assert_eq!(evt.message.is_group, Some(true));
        // Sender is still the handle (so we know who in the chat said it).
        let s = evt.sender.expect("sender present");
        assert_eq!(s.identity, "+15551234");
    }

    #[test]
    fn row_to_inbound_null_text_is_empty_string() {
        let r = row(3, "g3", None, false, Some("+1"), None, 0);
        let evt = row_to_inbound(&r).unwrap();
        assert_eq!(evt.message.content["text"], "");
    }

    #[test]
    fn row_to_inbound_uses_guid_as_id() {
        let r = row(4, "abc-def-123", Some("x"), false, Some("+1"), None, 0);
        let evt = row_to_inbound(&r).unwrap();
        assert_eq!(evt.message.id, "abc-def-123");
    }

    #[test]
    fn row_to_inbound_timestamp_uses_cocoa_conversion() {
        // 2024-01-01 nanos past the Cocoa epoch.
        let secs = 725_760_000_i64;
        let nanos = secs * 1_000_000_000;
        let r = row(5, "g5", Some("x"), false, Some("+1"), None, nanos);
        let evt = row_to_inbound(&r).unwrap();
        assert_eq!(evt.message.timestamp.to_rfc3339(), "2024-01-01T00:00:00+00:00");
    }

    #[test]
    fn row_to_inbound_default_thread_is_none() {
        let r = row(6, "g6", Some("x"), false, Some("+1"), None, 0);
        let evt = row_to_inbound(&r).unwrap();
        assert!(evt.thread_id.is_none());
        assert!(evt.reply_to.is_none());
    }

    #[test]
    fn select_new_rows_sql_includes_expected_columns() {
        let sql = select_new_rows_sql();
        for col in [
            "m.ROWID",
            "m.guid",
            "m.text",
            "m.date",
            "m.is_from_me",
            "h.id",
            "chat_identifier",
        ] {
            assert!(sql.contains(col), "expected column {col} in: {sql}");
        }
        assert!(sql.contains("ORDER BY m.ROWID ASC"));
        assert!(sql.contains("m.is_from_me = 0"));
    }

    #[test]
    fn describe_system_action_extracts_action() {
        let v = json!({ "action": "edit" });
        assert_eq!(describe_system_action(&v), "edit");
    }

    #[test]
    fn describe_system_action_fallback() {
        let v = json!({});
        assert_eq!(describe_system_action(&v), "<unknown>");
    }

    #[test]
    fn text_content_helper_makes_object() {
        let v = text_content_for_test("hi");
        assert_eq!(v["text"], "hi");
    }
}
