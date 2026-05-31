//! Handlers for `dropped-messages.list`, `dropped-messages.outbound-list`,
//! and `dropped-messages.replay`.
//!
//! The first handler lists *inbound* dropped messages (router refusals). The
//! latter two are for *outbound* dead-letter rows: delivery failures after
//! all retries were exhausted.

use super::{db_err, opt_str};
use chrono::DateTime;
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{open_outbound, SessionPaths};
use copperclaw_db::tables::{dropped_messages, messages_out, outbound_dropped_messages};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_types::MessageId;
use serde_json::{json, Value};
use uuid::Uuid;

pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let since = match opt_str(args, "since") {
        Some(s) => Some(
            DateTime::parse_from_rfc3339(&s)
                .map_err(|e| {
                    ErrorPayload::new("bad_request", format!("invalid `since` timestamp: {e}"))
                })?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };
    let rows = dropped_messages::list(central, since).map_err(db_err)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id.to_string(),
                "channel_type": r.channel_type.as_str(),
                "platform_id": r.platform_id,
                "user_id": r.user_id.map(|u| u.as_uuid().to_string()),
                "sender_name": r.sender_name,
                "reason": r.reason,
                "messaging_group_id": r.messaging_group_id.map(|m| m.as_uuid().to_string()),
                "agent_group_id": r.agent_group_id.map(|a| a.as_uuid().to_string()),
                "created_at": r.created_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(json!(out))
}

/// Parse a `--since` argument value. Accepts ISO-8601 timestamps and simple
/// relative windows like `24h`, `1h`, `30m`, `7d`.
fn parse_since(s: &str) -> Result<DateTime<chrono::Utc>, ErrorPayload> {
    // Try ISO-8601 first.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc));
    }
    // Relative shorthand: <N>(s|m|h|d). Use char iteration rather than
    // byte-index split_at so multi-byte UTF-8 inputs like "é" or "5🦀"
    // surface as a parse error instead of panicking inside split_at.
    let bad = || {
        ErrorPayload::new(
            "bad_request",
            format!("invalid `since` value `{s}`; use an ISO-8601 timestamp or a shorthand like `24h`, `30m`, `7d`"),
        )
    };
    let unit_char = s.chars().next_back().ok_or_else(bad)?;
    // The digits portion is everything up to the last char.
    let digits_end = s.len() - unit_char.len_utf8();
    let digits = &s[..digits_end];
    let n: i64 = digits.parse().map_err(|_| bad())?;
    let duration = match unit_char {
        's' => chrono::Duration::seconds(n),
        'm' => chrono::Duration::minutes(n),
        'h' => chrono::Duration::hours(n),
        'd' => chrono::Duration::days(n),
        _ => return Err(bad()),
    };
    Ok(chrono::Utc::now() - duration)
}

/// `dropped-messages.outbound-list` — list outbound dead-letter rows.
///
/// Returns rows from `outbound_dropped_messages` ordered most-recent first.
/// Optional `since` filter and `limit` cap (default 50).
pub fn outbound_list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let since = match opt_str(args, "since") {
        Some(s) => Some(parse_since(&s)?),
        None => None,
    };
    let limit: Option<i64> = args.get("limit").and_then(Value::as_i64);

    let rows = outbound_dropped_messages::list(central, since, limit).map_err(db_err)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.id.to_string(),
                "session_id": r.session_id.as_uuid().to_string(),
                "agent_group_id": r.agent_group_id.as_uuid().to_string(),
                "message_out_id": r.message_out_id.as_uuid().to_string(),
                "channel_type": r.channel_type.as_ref().map(copperclaw_types::ChannelType::as_str),
                "platform_id": r.platform_id,
                "thread_id": r.thread_id,
                "kind": r.kind.as_str(),
                "last_error": r.last_error,
                "dropped_at": r.dropped_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(json!(out))
}

/// `dropped-messages.replay` — re-queue an outbound dead-letter row.
///
/// Looks up the dead-letter row by its id, opens the originating session's
/// `outbound.db`, inserts a fresh copy of the message with reset status, then
/// deletes the dead-letter row so it doesn't appear again in `outbound-list`.
///
/// The `data_dir` must be passed through from the `HandlerCtx`; this handler
/// signature takes it as a separate string parameter for testability.
pub fn replay_with_data_dir(
    args: &Value,
    central: &CentralDb,
    data_dir: &std::path::Path,
) -> Result<Value, ErrorPayload> {
    let id_str = super::req_str(args, "id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| ErrorPayload::new("bad_request", format!("invalid dead-letter id: {e}")))?;

    let record = outbound_dropped_messages::get(central, id).map_err(|e| match e {
        copperclaw_db::DbError::NotFound => {
            ErrorPayload::new("not_found", format!("no outbound dead-letter row with id {id}"))
        }
        other => db_err(other),
    })?;

    // Open the session's outbound DB and insert a fresh copy of the message.
    let paths = SessionPaths::new(
        data_dir,
        record.agent_group_id,
        record.session_id,
    );
    let conn = open_outbound(&paths).map_err(|e| {
        ErrorPayload::new(
            "io_error",
            format!("could not open outbound DB for session {}: {e}", record.session_id.as_uuid()),
        )
    })?;
    let new_id = MessageId::new();
    let msg = messages_out::WriteOutbound {
        id: new_id,
        in_reply_to: None,
        timestamp: chrono::Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: record.kind,
        platform_id: record.platform_id.clone(),
        channel_type: record.channel_type.clone(),
        thread_id: record.thread_id.clone(),
        content: record.content.clone(),
    };
    messages_out::insert(&conn, &msg).map_err(|e| {
        ErrorPayload::new("db_error", format!("failed to re-insert message: {e}"))
    })?;

    // Delete the dead-letter row so it doesn't show up in outbound-list again.
    outbound_dropped_messages::delete(central, id).map_err(db_err)?;

    Ok(json!({
        "replayed": true,
        "dead_letter_id": id.to_string(),
        "new_message_id": new_id.as_uuid().to_string(),
        "session_id": record.session_id.as_uuid().to_string(),
        "agent_group_id": record.agent_group_id.as_uuid().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::dropped_messages::{insert, InsertDroppedMessage};
    use copperclaw_types::ChannelType;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn list_empty() {
        let db = db();
        let v = list(&Value::Null, &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn list_returns_inserted() {
        let db = db();
        insert(
            &db,
            InsertDroppedMessage {
                channel_type: ChannelType::new("cli"),
                platform_id: "p".into(),
                user_id: None,
                sender_name: None,
                reason: "no_messaging_group".into(),
                messaging_group_id: None,
                agent_group_id: None,
            },
        )
        .unwrap();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn list_since_filter_used() {
        let db = db();
        insert(
            &db,
            InsertDroppedMessage {
                channel_type: ChannelType::new("cli"),
                platform_id: "p".into(),
                user_id: None,
                sender_name: None,
                reason: "x".into(),
                messaging_group_id: None,
                agent_group_id: None,
            },
        )
        .unwrap();
        // Use a future timestamp so the filter rejects every row.
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let v = list(
            &json!({"since": future.to_rfc3339()}),
            &db,
        )
        .unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn list_bad_since_errors() {
        let db = db();
        let err = list(&json!({"since": "not-a-ts"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn parse_since_rejects_multibyte_inputs_without_panicking() {
        // Inputs that exercise the previous byte-index split_at bug:
        // - "é" is 2 bytes, single char; split_at(len-1) lands on the
        //   middle of the e-acute and panics.
        // - "5🦀" has a 4-byte trailing crab; split_at(len-1) lands
        //   inside the crab bytes and panics.
        // - "hello" has no recognised unit suffix.
        // - "" is empty and must not panic.
        for bad in ["é", "5🦀", "hello", "", "🦀"] {
            let err = parse_since(bad);
            assert!(
                err.is_err(),
                "parse_since({bad:?}) should error, got {err:?}",
            );
            assert_eq!(err.unwrap_err().code, "bad_request");
        }
    }

    #[test]
    fn parse_since_accepts_valid_shorthand() {
        assert!(parse_since("24h").is_ok());
        assert!(parse_since("30m").is_ok());
        assert!(parse_since("7d").is_ok());
        assert!(parse_since("90s").is_ok());
    }

    #[test]
    fn outbound_list_with_multibyte_since_errors_cleanly() {
        // End-to-end: feeding the handler a multi-byte `since` value
        // returns a bad_request error rather than panicking the host
        // task.
        let db = db();
        let err = outbound_list(&json!({"since": "5🦀"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }
}
