//! Handlers for `approvals.*` commands.

use super::{db_err, parse_uuid, req_str};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::pending_approvals;
use ironclaw_iclaw::ErrorPayload;
use ironclaw_types::ApprovalId;
use serde_json::{json, Value};

pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let rows = pending_approvals::list(central, None, None).map_err(db_err)?;
    Ok(json!(rows.iter().map(approval_to_json).collect::<Vec<_>>()))
}

pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id = ApprovalId(parse_uuid(&req_str(args, "id")?)?);
    let row = pending_approvals::get(central, id).map_err(db_err)?;
    Ok(approval_to_json(&row))
}

fn approval_to_json(a: &pending_approvals::PendingApproval) -> Value {
    json!({
        "approval_id": a.approval_id.as_uuid().to_string(),
        "session_id": a.session_id.map(|s| s.as_uuid().to_string()),
        "request_id": a.request_id,
        "action": a.action,
        "payload": a.payload,
        "agent_group_id": a.agent_group_id.map(|g| g.as_uuid().to_string()),
        "channel_type": a.channel_type.as_ref().map(|c| c.as_str().to_owned()),
        "platform_id": a.platform_id,
        "platform_message_id": a.platform_message_id,
        "expires_at": a.expires_at.map(|t| t.to_rfc3339()),
        "status": a.status.as_str(),
        "title": a.title,
        "options": a.options,
        "created_at": a.created_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::pending_approvals::{upsert, UpsertPendingApproval};

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
    fn list_after_insert() {
        let db = db();
        upsert(
            &db,
            UpsertPendingApproval {
                request_id: "r1".into(),
                action: "send".into(),
                payload: json!({}),
                title: "Approve?".into(),
                options: vec!["yes".into(), "no".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn get_returns_row() {
        let db = db();
        let a = upsert(
            &db,
            UpsertPendingApproval {
                request_id: "r1".into(),
                action: "send".into(),
                payload: json!({}),
                title: "x".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        let v = get(
            &json!({"id": a.approval_id.as_uuid().to_string()}),
            &db,
        )
        .unwrap();
        assert_eq!(v["request_id"], "r1");
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = db();
        let err = get(
            &json!({"id": uuid::Uuid::now_v7().to_string()}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }
}
