//! CRUD for `user_dms`.
//!
//! Records the resolved messaging-group used for direct messages between the
//! system and a given user on a given channel type.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{ChannelType, MessagingGroupId, UserId};
use rusqlite::{OptionalExtension, Row, params};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDm {
    pub user_id: UserId,
    pub channel_type: ChannelType,
    pub messaging_group_id: MessagingGroupId,
    pub resolved_at: DateTime<Utc>,
}

fn parse_uuid_col(s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_user_dm(row: &Row<'_>) -> rusqlite::Result<UserDm> {
    let user_id_str: String = row.get("user_id")?;
    let user_id = UserId(parse_uuid_col(&user_id_str)?);
    let channel_type_str: String = row.get("channel_type")?;
    let mg_str: String = row.get("messaging_group_id")?;
    let messaging_group_id = MessagingGroupId(parse_uuid_col(&mg_str)?);
    let resolved_at_str: String = row.get("resolved_at")?;
    let resolved_at = DateTime::parse_from_rfc3339(&resolved_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    Ok(UserDm {
        user_id,
        channel_type: ChannelType::new(channel_type_str),
        messaging_group_id,
        resolved_at,
    })
}

pub fn get(
    db: &CentralDb,
    user: UserId,
    channel_type: &ChannelType,
) -> Result<Option<UserDm>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT user_id, channel_type, messaging_group_id, resolved_at
             FROM user_dms
             WHERE user_id = ?1 AND channel_type = ?2",
            params![user.as_uuid().to_string(), channel_type.as_str()],
            row_to_user_dm,
        )
        .optional()?)
}

pub fn upsert(
    db: &CentralDb,
    user: UserId,
    channel_type: ChannelType,
    mg: MessagingGroupId,
) -> Result<UserDm, DbError> {
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO user_dms (user_id, channel_type, messaging_group_id, resolved_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id, channel_type) DO UPDATE SET
             messaging_group_id = excluded.messaging_group_id,
             resolved_at = excluded.resolved_at",
        params![
            user.as_uuid().to_string(),
            channel_type.as_str(),
            mg.as_uuid().to_string(),
            now.to_rfc3339(),
        ],
    )?;
    Ok(UserDm {
        user_id: user,
        channel_type,
        messaging_group_id: mg,
        resolved_at: now,
    })
}

pub fn list(db: &CentralDb, user: UserId) -> Result<Vec<UserDm>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT user_id, channel_type, messaging_group_id, resolved_at
         FROM user_dms
         WHERE user_id = ?1
         ORDER BY resolved_at",
    )?;
    let rows = stmt.query_map(params![user.as_uuid().to_string()], row_to_user_dm)?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::messaging_groups::{self, UpsertMessagingGroup};
    use crate::tables::users::{self, UpsertUser};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn mk_user(db: &CentralDb, identity: &str) -> UserId {
        users::upsert(
            db,
            UpsertUser {
                kind: "telegram".into(),
                identity: identity.into(),
                display_name: None,
            },
        )
        .unwrap()
        .id
    }

    fn mk_mg(db: &CentralDb, channel: &str, platform: &str) -> MessagingGroupId {
        messaging_groups::upsert(
            db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new(channel),
                platform_id: platform.into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn upsert_then_get() {
        let db = db();
        let u = mk_user(&db, "alice");
        let mg = mk_mg(&db, "telegram", "12345");
        let ct = ChannelType::new("telegram");
        let dm = upsert(&db, u, ct.clone(), mg).unwrap();
        let fetched = get(&db, u, &ct).unwrap();
        assert_eq!(fetched, Some(dm));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = db();
        let u = mk_user(&db, "alice");
        let ct = ChannelType::new("telegram");
        assert!(get(&db, u, &ct).unwrap().is_none());
    }

    #[test]
    fn upsert_replaces_messaging_group() {
        let db = db();
        let u = mk_user(&db, "alice");
        let mg1 = mk_mg(&db, "telegram", "111");
        let mg2 = mk_mg(&db, "telegram", "222");
        let ct = ChannelType::new("telegram");
        upsert(&db, u, ct.clone(), mg1).unwrap();
        let updated = upsert(&db, u, ct.clone(), mg2).unwrap();
        assert_eq!(updated.messaging_group_id, mg2);
        let fetched = get(&db, u, &ct).unwrap().unwrap();
        assert_eq!(fetched.messaging_group_id, mg2);
    }

    #[test]
    fn list_returns_all_channel_types_for_user() {
        let db = db();
        let u = mk_user(&db, "alice");
        let mg_tg = mk_mg(&db, "telegram", "tg-1");
        let mg_sl = mk_mg(&db, "slack", "sl-1");
        upsert(&db, u, ChannelType::new("telegram"), mg_tg).unwrap();
        upsert(&db, u, ChannelType::new("slack"), mg_sl).unwrap();
        let rows = list(&db, u).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn list_is_scoped_per_user() {
        let db = db();
        let a = mk_user(&db, "alice");
        let b = mk_user(&db, "bob");
        let mg = mk_mg(&db, "telegram", "shared");
        upsert(&db, a, ChannelType::new("telegram"), mg).unwrap();
        assert_eq!(list(&db, a).unwrap().len(), 1);
        assert!(list(&db, b).unwrap().is_empty());
    }

    #[test]
    fn list_orders_by_resolved_at() {
        let db = db();
        let u = mk_user(&db, "alice");
        let mg1 = mk_mg(&db, "telegram", "tg-1");
        let mg2 = mk_mg(&db, "slack", "sl-1");
        upsert(&db, u, ChannelType::new("telegram"), mg1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        upsert(&db, u, ChannelType::new("slack"), mg2).unwrap();
        let rows = list(&db, u).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].resolved_at <= rows[1].resolved_at);
    }

    #[test]
    fn list_empty_when_no_dms() {
        let db = db();
        let u = mk_user(&db, "ghost");
        assert!(list(&db, u).unwrap().is_empty());
    }

    #[test]
    fn upsert_with_missing_user_violates_fk() {
        let db = db();
        let mg = mk_mg(&db, "telegram", "x");
        let ghost = UserId::new();
        let err = upsert(&db, ghost, ChannelType::new("telegram"), mg).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn upsert_with_missing_messaging_group_violates_fk() {
        let db = db();
        let u = mk_user(&db, "alice");
        let ghost = MessagingGroupId::new();
        let err = upsert(&db, u, ChannelType::new("telegram"), ghost).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
