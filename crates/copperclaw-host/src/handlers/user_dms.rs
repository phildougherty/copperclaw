//! Handler for `user-dms.list`.

use super::db_err;
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{user_dms, users};
use serde_json::{Value, json};

pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    // The table is keyed per-user with no global list query, so we walk
    // every known user. Cheap for human-scale deployments.
    let users = users::list(central).map_err(db_err)?;
    let mut out = Vec::new();
    for u in users {
        let rows = user_dms::list(central, u.id).map_err(db_err)?;
        for r in rows {
            out.push(json!({
                "user_id": r.user_id.as_uuid().to_string(),
                "channel_type": r.channel_type.as_str(),
                "messaging_group_id": r.messaging_group_id.as_uuid().to_string(),
                "resolved_at": r.resolved_at.to_rfc3339(),
            }));
        }
    }
    Ok(json!(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
    use copperclaw_db::tables::user_dms::upsert as upsert_dm;
    use copperclaw_db::tables::users::{UpsertUser, upsert as upsert_user};
    use copperclaw_types::ChannelType;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn list_empty_when_no_dms() {
        let db = db();
        let v = list(&Value::Null, &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }

    #[test]
    fn list_aggregates_across_users() {
        let db = db();
        let u = upsert_user(
            &db,
            UpsertUser {
                kind: "telegram".into(),
                identity: "alice".into(),
                display_name: None,
            },
        )
        .unwrap();
        let mg = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "p".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_dm(&db, u.id, ChannelType::new("telegram"), mg.id).unwrap();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v.as_array().unwrap()[0]["channel_type"], "telegram");
    }
}
