//! Handlers for `members.*` commands.

use super::{db_err, parse_uuid, req_str};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::agent_group_members;
use copperclaw_types::{AgentGroupId, UserId};
use serde_json::{Value, json};

pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let ag = AgentGroupId(parse_uuid(&req_str(args, "agent_group_id")?)?);
    let rows = agent_group_members::list(central, ag).map_err(db_err)?;
    Ok(json!(rows.iter().map(member_to_json).collect::<Vec<_>>()))
}

pub fn add(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let ag = AgentGroupId(parse_uuid(&req_str(args, "agent_group_id")?)?);
    let user = UserId(parse_uuid(&req_str(args, "user")?)?);
    let row = agent_group_members::add(central, user, ag, None).map_err(db_err)?;
    Ok(member_to_json(&row))
}

pub fn remove(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let ag = AgentGroupId(parse_uuid(&req_str(args, "agent_group_id")?)?);
    let user = UserId(parse_uuid(&req_str(args, "user")?)?);
    agent_group_members::remove(central, user, ag).map_err(db_err)?;
    Ok(json!({"removed": true}))
}

fn member_to_json(m: &agent_group_members::Member) -> Value {
    json!({
        "user_id": m.user_id.as_uuid().to_string(),
        "agent_group_id": m.agent_group_id.as_uuid().to_string(),
        "added_by": m.added_by.map(|a| a.as_uuid().to_string()),
        "added_at": m.added_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::users::{self, UpsertUser};

    fn db_with_user_group() -> (CentralDb, UserId, AgentGroupId) {
        let db = CentralDb::open_in_memory().unwrap();
        let u = users::upsert(
            &db,
            UpsertUser {
                kind: "x".into(),
                identity: "y".into(),
                display_name: None,
            },
        )
        .unwrap();
        let g = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        (db, u.id, g.id)
    }

    #[test]
    fn add_then_list() {
        let (db, u, g) = db_with_user_group();
        add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "user": u.as_uuid().to_string(),
            }),
            &db,
        )
        .unwrap();
        let v = list(&json!({"agent_group_id": g.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn remove_works() {
        let (db, u, g) = db_with_user_group();
        add(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "user": u.as_uuid().to_string(),
            }),
            &db,
        )
        .unwrap();
        remove(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "user": u.as_uuid().to_string(),
            }),
            &db,
        )
        .unwrap();
        let v = list(&json!({"agent_group_id": g.as_uuid().to_string()}), &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn remove_missing_errors() {
        let (db, u, g) = db_with_user_group();
        let err = remove(
            &json!({
                "agent_group_id": g.as_uuid().to_string(),
                "user": u.as_uuid().to_string(),
            }),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn missing_arg_errors() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = list(&json!({}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }
}
