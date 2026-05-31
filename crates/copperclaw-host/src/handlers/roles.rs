//! Handlers for `roles.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::user_roles;
use copperclaw_cclaw::ErrorPayload;
use copperclaw_types::{AgentGroupId, UserId};
use serde_json::{json, Value};

pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    // Filter by user_id when supplied; otherwise return globals.
    if let Some(user) = opt_str(args, "user") {
        let id = UserId(parse_uuid(&user)?);
        let rows = user_roles::list_for_user(central, id).map_err(db_err)?;
        Ok(json!(rows.iter().map(role_to_json).collect::<Vec<_>>()))
    } else {
        // No user filter — list every owner+admin (global + scoped) by scanning
        // both roles across all known users. The DB does not expose a "list all"
        // query, so we emulate it by joining through `users.list`.
        let users = copperclaw_db::tables::users::list(central).map_err(db_err)?;
        let mut out = Vec::new();
        for u in users {
            for row in user_roles::list_for_user(central, u.id).map_err(db_err)? {
                out.push(role_to_json(&row));
            }
        }
        Ok(json!(out))
    }
}

pub fn grant(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let user = UserId(parse_uuid(&req_str(args, "user")?)?);
    let role = parse_role(&req_str(args, "role")?)?;
    let agent_group_id = match opt_str(args, "agent_group_id") {
        Some(s) => Some(AgentGroupId(parse_uuid(&s)?)),
        None => None,
    };
    let row = user_roles::grant(central, user, role, agent_group_id, None).map_err(db_err)?;
    Ok(role_to_json(&row))
}

pub fn revoke(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let user = UserId(parse_uuid(&req_str(args, "user")?)?);
    let role = parse_role(&req_str(args, "role")?)?;
    let agent_group_id = match opt_str(args, "agent_group_id") {
        Some(s) => Some(AgentGroupId(parse_uuid(&s)?)),
        None => None,
    };
    user_roles::revoke(central, user, role, agent_group_id).map_err(db_err)?;
    Ok(json!({"revoked": true}))
}

fn parse_role(s: &str) -> Result<user_roles::Role, ErrorPayload> {
    user_roles::Role::parse(s).ok_or_else(|| {
        ErrorPayload::new("bad_request", format!("unknown role `{s}`"))
    })
}

fn role_to_json(r: &user_roles::UserRole) -> Value {
    json!({
        "user_id": r.user_id.as_uuid().to_string(),
        "role": r.role.as_str(),
        "agent_group_id": r.agent_group_id.map(|a| a.as_uuid().to_string()),
        "granted_by": r.granted_by.map(|g| g.as_uuid().to_string()),
        "granted_at": r.granted_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::users::{self, UpsertUser};

    fn db_with_user(identity: &str) -> (CentralDb, UserId) {
        let db = CentralDb::open_in_memory().unwrap();
        let u = users::upsert(
            &db,
            UpsertUser {
                kind: "telegram".into(),
                identity: identity.into(),
                display_name: None,
            },
        )
        .unwrap();
        (db, u.id)
    }

    #[test]
    fn grant_then_list_for_user() {
        let (db, u) = db_with_user("alice");
        grant(
            &json!({"user": u.as_uuid().to_string(), "role": "owner"}),
            &db,
        )
        .unwrap();
        let v = list(&json!({"user": u.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn list_without_filter_aggregates() {
        let (db, u) = db_with_user("alice");
        grant(
            &json!({"user": u.as_uuid().to_string(), "role": "owner"}),
            &db,
        )
        .unwrap();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn grant_unknown_role_errors() {
        let (db, u) = db_with_user("alice");
        let err = grant(
            &json!({"user": u.as_uuid().to_string(), "role": "wizard"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn revoke_removes_grant() {
        let (db, u) = db_with_user("alice");
        grant(
            &json!({"user": u.as_uuid().to_string(), "role": "owner"}),
            &db,
        )
        .unwrap();
        revoke(
            &json!({"user": u.as_uuid().to_string(), "role": "owner"}),
            &db,
        )
        .unwrap();
        let v = list(&json!({"user": u.as_uuid().to_string()}), &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn revoke_missing_is_not_found() {
        let (db, u) = db_with_user("alice");
        let err = revoke(
            &json!({"user": u.as_uuid().to_string(), "role": "owner"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn grant_scoped_to_agent_group() {
        use copperclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
        let (db, u) = db_with_user("alice");
        let g = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let v = grant(
            &json!({
                "user": u.as_uuid().to_string(),
                "role": "admin",
                "agent_group_id": g.id.as_uuid().to_string(),
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["role"], "admin");
        assert!(v["agent_group_id"].is_string());
    }
}
