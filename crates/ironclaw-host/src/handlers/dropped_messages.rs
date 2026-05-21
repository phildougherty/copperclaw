//! Handler for `dropped-messages.list`.

use super::{db_err, opt_str};
use chrono::DateTime;
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::dropped_messages;
use ironclaw_iclaw::ErrorPayload;
use serde_json::{json, Value};

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

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::dropped_messages::{insert, InsertDroppedMessage};
    use ironclaw_types::ChannelType;

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
}
