//! Handlers for `messaging-groups.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::messaging_groups;
use ironclaw_iclaw::ErrorPayload;
use ironclaw_types::{ChannelType, MessagingGroupId};
use serde_json::{json, Value};

pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let rows = messaging_groups::list(central).map_err(db_err)?;
    Ok(json!(rows.iter().map(mg_to_json).collect::<Vec<_>>()))
}

pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_mg_id(args, "id")?;
    let row = messaging_groups::get(central, id).map_err(db_err)?;
    Ok(mg_to_json(&row))
}

pub fn create(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let channel_type = req_str(args, "channel_type")?;
    let platform_id = req_str(args, "platform_id")?;
    let name = opt_str(args, "name");
    let is_group = args.get("is_group").and_then(Value::as_bool).unwrap_or(false);
    let unknown_sender_policy = opt_str(args, "unknown_sender_policy")
        .unwrap_or_else(|| "strict".to_owned());
    let row = messaging_groups::upsert(
        central,
        messaging_groups::UpsertMessagingGroup {
            channel_type: ChannelType::new(channel_type),
            platform_id,
            name,
            is_group,
            unknown_sender_policy,
        },
    )
    .map_err(db_err)?;
    Ok(mg_to_json(&row))
}

pub fn update(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_mg_id(args, "id")?;
    let existing = messaging_groups::get(central, id).map_err(db_err)?;
    let name = if args.get("name").is_some() {
        opt_str(args, "name")
    } else {
        existing.name.clone()
    };
    let is_group = args
        .get("is_group")
        .and_then(Value::as_bool)
        .unwrap_or(existing.is_group);
    let unknown_sender_policy = opt_str(args, "unknown_sender_policy")
        .unwrap_or(existing.unknown_sender_policy.clone());
    let row = messaging_groups::upsert(
        central,
        messaging_groups::UpsertMessagingGroup {
            channel_type: existing.channel_type,
            platform_id: existing.platform_id,
            name,
            is_group,
            unknown_sender_policy,
        },
    )
    .map_err(db_err)?;
    Ok(mg_to_json(&row))
}

pub fn delete(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = parse_mg_id(args, "id")?;
    messaging_groups::delete(central, id).map_err(db_err)?;
    Ok(json!({"deleted": id.as_uuid().to_string()}))
}

fn parse_mg_id(args: &Value, key: &str) -> Result<MessagingGroupId, ErrorPayload> {
    let s = req_str(args, key)?;
    Ok(MessagingGroupId(parse_uuid(&s)?))
}

fn mg_to_json(m: &messaging_groups::MessagingGroup) -> Value {
    json!({
        "id": m.id.as_uuid().to_string(),
        "channel_type": m.channel_type.as_str(),
        "platform_id": m.platform_id,
        "name": m.name,
        "is_group": m.is_group,
        "unknown_sender_policy": m.unknown_sender_policy,
        "denied_at": m.denied_at.map(|t| t.to_rfc3339()),
        "created_at": m.created_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn list_get_create_roundtrip() {
        let db = db();
        assert!(list(&Value::Null, &db).unwrap().as_array().unwrap().is_empty());
        let created = create(
            &json!({"channel_type": "cli", "platform_id": "p1"}),
            &db,
        )
        .unwrap();
        let id = created["id"].as_str().unwrap();
        let got = get(&json!({"id": id}), &db).unwrap();
        assert_eq!(got["platform_id"], "p1");
        let listed = list(&Value::Null, &db).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
    }

    #[test]
    fn create_missing_fields_errors() {
        let db = db();
        let err = create(&json!({"channel_type": "cli"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn update_sets_name_and_preserves_other_fields() {
        let db = db();
        let created = create(
            &json!({"channel_type": "cli", "platform_id": "p1", "is_group": true}),
            &db,
        )
        .unwrap();
        let id = created["id"].as_str().unwrap();
        let updated = update(
            &json!({"id": id, "name": "Renamed"}),
            &db,
        )
        .unwrap();
        assert_eq!(updated["name"], "Renamed");
        assert_eq!(updated["is_group"], true);
    }

    #[test]
    fn update_unknown_id_errors() {
        let db = db();
        let err = update(
            &json!({"id": uuid::Uuid::now_v7().to_string()}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_removes_row() {
        let db = db();
        let created = create(
            &json!({"channel_type": "cli", "platform_id": "p2"}),
            &db,
        )
        .unwrap();
        let id = created["id"].as_str().unwrap();
        delete(&json!({"id": id}), &db).unwrap();
        let err = get(&json!({"id": id}), &db).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn create_with_explicit_unknown_sender_policy() {
        let db = db();
        let v = create(
            &json!({
                "channel_type": "telegram",
                "platform_id": "p3",
                "unknown_sender_policy": "request_approval",
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["unknown_sender_policy"], "request_approval");
    }
}
