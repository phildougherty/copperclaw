//! Handlers for `users.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::users;
use copperclaw_cclaw::ErrorPayload;
use copperclaw_types::UserId;
use serde_json::{json, Value};

pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let rows = users::list(central).map_err(db_err)?;
    Ok(json!(rows.iter().map(user_to_json).collect::<Vec<_>>()))
}

pub fn get(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id_str = req_str(args, "id")?;
    let id = UserId(parse_uuid(&id_str)?);
    let row = users::get(central, id).map_err(db_err)?;
    Ok(user_to_json(&row))
}

/// `users.create` — identity is `<kind>:<handle>`.
pub fn create(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let identity_full = req_str(args, "identity")?;
    let (kind, identity) = identity_full
        .split_once(':')
        .ok_or_else(|| {
            ErrorPayload::new("bad_request", "identity must be `<kind>:<handle>`")
        })?;
    let display_name = opt_str(args, "display_name");
    let row = users::upsert(
        central,
        users::UpsertUser {
            kind: kind.to_owned(),
            identity: identity.to_owned(),
            display_name,
        },
    )
    .map_err(db_err)?;
    Ok(user_to_json(&row))
}

/// `users.update` — only `display_name` is updatable.
pub fn update(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let id_str = req_str(args, "id")?;
    let id = UserId(parse_uuid(&id_str)?);
    let existing = users::get(central, id).map_err(db_err)?;
    let display_name = if args.get("display_name").is_some() {
        opt_str(args, "display_name")
    } else {
        existing.display_name
    };
    // upsert by kind+identity isn't available via `id` alone; we need to
    // reconstruct identity from the original row. The `users` table only
    // tracks `kind` directly; the original `identity` is recoverable because
    // the row's id is UUIDv5(kind:identity). For update of display_name we
    // can re-issue upsert with the recovered (kind, identity) — but the
    // identity isn't stored. We instead update the row in place via a direct
    // SQL statement.
    let conn = central.conn().map_err(db_err)?;
    conn.execute(
        "UPDATE users SET display_name = ?1 WHERE id = ?2",
        rusqlite::params![display_name, id.as_uuid().to_string()],
    )
    .map_err(|e| db_err(copperclaw_db::DbError::Sqlite(e)))?;
    drop(conn);
    let row = users::get(central, id).map_err(db_err)?;
    Ok(user_to_json(&row))
}

fn user_to_json(u: &users::User) -> Value {
    json!({
        "id": u.id.as_uuid().to_string(),
        "kind": u.kind,
        "display_name": u.display_name,
        "created_at": u.created_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn create_with_identity_round_trips() {
        let db = db();
        let v = create(
            &json!({"identity": "telegram:alice", "display_name": "Alice"}),
            &db,
        )
        .unwrap();
        assert_eq!(v["kind"], "telegram");
        assert_eq!(v["display_name"], "Alice");
    }

    #[test]
    fn create_without_colon_errors() {
        let db = db();
        let err = create(&json!({"identity": "noeq"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn list_returns_rows() {
        let db = db();
        create(&json!({"identity": "x:y"}), &db).unwrap();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn get_by_id_works() {
        let db = db();
        let v = create(&json!({"identity": "x:y"}), &db).unwrap();
        let id = v["id"].as_str().unwrap();
        let got = get(&json!({"id": id}), &db).unwrap();
        assert_eq!(got["kind"], "x");
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

    #[test]
    fn update_changes_display_name() {
        let db = db();
        let v = create(&json!({"identity": "x:y", "display_name": "Old"}), &db).unwrap();
        let id = v["id"].as_str().unwrap();
        let v = update(&json!({"id": id, "display_name": "New"}), &db).unwrap();
        assert_eq!(v["display_name"], "New");
    }

    #[test]
    fn update_can_clear_display_name() {
        let db = db();
        let v = create(&json!({"identity": "x:y", "display_name": "Old"}), &db).unwrap();
        let id = v["id"].as_str().unwrap();
        let v = update(&json!({"id": id, "display_name": Value::Null}), &db).unwrap();
        assert!(v["display_name"].is_null());
    }

    #[test]
    fn update_unknown_id_errors() {
        let db = db();
        let err = update(
            &json!({"id": uuid::Uuid::now_v7().to_string(), "display_name": "x"}),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }
}
