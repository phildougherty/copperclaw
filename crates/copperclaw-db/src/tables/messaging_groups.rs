//! CRUD for `messaging_groups`.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{ChannelType, MessagingGroupId};
use rusqlite::{OptionalExtension, Row, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingGroup {
    pub id: MessagingGroupId,
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub name: Option<String>,
    pub is_group: bool,
    pub unknown_sender_policy: String,
    pub denied_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UpsertMessagingGroup {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub name: Option<String>,
    pub is_group: bool,
    pub unknown_sender_policy: String,
}

fn row_to_messaging_group(row: &Row<'_>) -> rusqlite::Result<MessagingGroup> {
    let id_str: String = row.get("id")?;
    let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let channel_type_str: String = row.get("channel_type")?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let denied_at_str: Option<String> = row.get("denied_at")?;
    let denied_at = denied_at_str
        .as_deref()
        .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let is_group_int: i64 = row.get("is_group")?;
    Ok(MessagingGroup {
        id: MessagingGroupId(id),
        channel_type: ChannelType::new(channel_type_str),
        platform_id: row.get("platform_id")?,
        name: row.get("name")?,
        is_group: is_group_int != 0,
        unknown_sender_policy: row.get("unknown_sender_policy")?,
        denied_at,
        created_at,
    })
}

pub fn list(db: &CentralDb) -> Result<Vec<MessagingGroup>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, channel_type, platform_id, name, is_group,
                unknown_sender_policy, denied_at, created_at
         FROM messaging_groups
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], row_to_messaging_group)?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

pub fn get(db: &CentralDb, id: MessagingGroupId) -> Result<MessagingGroup, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT id, channel_type, platform_id, name, is_group,
                unknown_sender_policy, denied_at, created_at
         FROM messaging_groups WHERE id = ?1",
        params![id.as_uuid().to_string()],
        row_to_messaging_group,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

pub fn get_by_platform(
    db: &CentralDb,
    channel_type: &ChannelType,
    platform_id: &str,
) -> Result<Option<MessagingGroup>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT id, channel_type, platform_id, name, is_group,
                    unknown_sender_policy, denied_at, created_at
             FROM messaging_groups
             WHERE channel_type = ?1 AND platform_id = ?2",
            params![channel_type.as_str(), platform_id],
            row_to_messaging_group,
        )
        .optional()?)
}

pub fn get_with_agent_count(
    db: &CentralDb,
    channel_type: &ChannelType,
    platform_id: &str,
) -> Result<Option<(MessagingGroup, u32)>, DbError> {
    let Some(mg) = get_by_platform(db, channel_type, platform_id)? else {
        return Ok(None);
    };
    let conn = db.conn()?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messaging_group_agents WHERE messaging_group_id = ?1",
        params![mg.id.as_uuid().to_string()],
        |r| r.get(0),
    )?;
    let count = u32::try_from(count.max(0)).unwrap_or(u32::MAX);
    Ok(Some((mg, count)))
}

pub fn upsert(db: &CentralDb, req: UpsertMessagingGroup) -> Result<MessagingGroup, DbError> {
    let conn = db.conn()?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM messaging_groups WHERE channel_type = ?1 AND platform_id = ?2",
            params![req.channel_type.as_str(), req.platform_id],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(id_str) = existing {
        conn.execute(
            "UPDATE messaging_groups
             SET name = ?1, is_group = ?2, unknown_sender_policy = ?3
             WHERE id = ?4",
            params![
                req.name,
                i64::from(req.is_group),
                req.unknown_sender_policy,
                id_str,
            ],
        )?;
        let id = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| DbError::Invariant(format!("invalid uuid in messaging_groups.id: {e}")))?;
        drop(conn);
        return get(db, MessagingGroupId(id));
    }

    let id = MessagingGroupId::new();
    let now = Utc::now();
    conn.execute(
        "INSERT INTO messaging_groups
           (id, channel_type, platform_id, name, is_group,
            unknown_sender_policy, denied_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
        params![
            id.as_uuid().to_string(),
            req.channel_type.as_str(),
            req.platform_id,
            req.name,
            i64::from(req.is_group),
            req.unknown_sender_policy,
            now.to_rfc3339(),
        ],
    )?;
    Ok(MessagingGroup {
        id,
        channel_type: req.channel_type,
        platform_id: req.platform_id,
        name: req.name,
        is_group: req.is_group,
        unknown_sender_policy: req.unknown_sender_policy,
        denied_at: None,
        created_at: now,
    })
}

pub fn delete(db: &CentralDb, id: MessagingGroupId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM messaging_groups WHERE id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn mark_denied(db: &CentralDb, id: MessagingGroupId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE messaging_groups SET denied_at = ?1 WHERE id = ?2",
        params![Utc::now().to_rfc3339(), id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn sample(platform_id: &str) -> UpsertMessagingGroup {
        UpsertMessagingGroup {
            channel_type: ChannelType::new("telegram"),
            platform_id: platform_id.into(),
            name: Some("Test Group".into()),
            is_group: true,
            unknown_sender_policy: "strict".into(),
        }
    }

    #[test]
    fn upsert_then_get() {
        let db = db();
        let mg = upsert(&db, sample("p1")).unwrap();
        let fetched = get(&db, mg.id).unwrap();
        assert_eq!(mg, fetched);
        assert_eq!(fetched.platform_id, "p1");
        assert!(fetched.is_group);
        assert_eq!(fetched.unknown_sender_policy, "strict");
        assert!(fetched.denied_at.is_none());
    }

    #[test]
    fn upsert_updates_existing_row() {
        let db = db();
        let first = upsert(&db, sample("p1")).unwrap();
        let mut req = sample("p1");
        req.name = Some("Renamed".into());
        req.is_group = false;
        req.unknown_sender_policy = "request_approval".into();
        let second = upsert(&db, req).unwrap();
        assert_eq!(first.id, second.id, "upsert should reuse id");
        assert_eq!(second.name.as_deref(), Some("Renamed"));
        assert!(!second.is_group);
        assert_eq!(second.unknown_sender_policy, "request_approval");
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = db();
        let err = get(&db, MessagingGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn get_by_platform_returns_option() {
        let db = db();
        let ct = ChannelType::new("telegram");
        assert!(get_by_platform(&db, &ct, "absent").unwrap().is_none());
        upsert(&db, sample("present")).unwrap();
        assert!(get_by_platform(&db, &ct, "present").unwrap().is_some());
    }

    #[test]
    fn get_by_platform_distinguishes_channel_types() {
        let db = db();
        upsert(&db, sample("p1")).unwrap();
        let other = ChannelType::new("slack");
        assert!(get_by_platform(&db, &other, "p1").unwrap().is_none());
    }

    #[test]
    fn get_with_agent_count_none_when_missing() {
        let db = db();
        let ct = ChannelType::new("telegram");
        assert!(get_with_agent_count(&db, &ct, "absent").unwrap().is_none());
    }

    #[test]
    fn get_with_agent_count_zero_when_no_wiring() {
        let db = db();
        let mg = upsert(&db, sample("p1")).unwrap();
        let (found, count) = get_with_agent_count(&db, &mg.channel_type, &mg.platform_id)
            .unwrap()
            .unwrap();
        assert_eq!(found.id, mg.id);
        assert_eq!(count, 0);
    }

    #[test]
    fn get_with_agent_count_reflects_wiring_count() {
        use crate::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        let db = db();
        let mg = upsert(&db, sample("p1")).unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "greeter".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let conn = db.conn().unwrap();
        conn.execute(
            "INSERT INTO messaging_group_agents
               (id, messaging_group_id, agent_group_id, engage_mode,
                engage_pattern, sender_scope, ignored_message_policy,
                session_mode, priority, created_at)
             VALUES (?1, ?2, ?3, 'mention', NULL, 'all', 'drop', 'shared', 0, ?4)",
            params![
                uuid::Uuid::now_v7().to_string(),
                mg.id.as_uuid().to_string(),
                ag.id.as_uuid().to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .unwrap();
        drop(conn);
        let (_, count) = get_with_agent_count(&db, &mg.channel_type, &mg.platform_id)
            .unwrap()
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn list_is_ordered_by_created_at() {
        let db = db();
        for i in 0..3 {
            upsert(&db, sample(&format!("p{i}"))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows = list(&db).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].platform_id, "p0");
        assert_eq!(rows[2].platform_id, "p2");
    }

    #[test]
    fn delete_works() {
        let db = db();
        let mg = upsert(&db, sample("p1")).unwrap();
        delete(&db, mg.id).unwrap();
        assert!(matches!(get(&db, mg.id).unwrap_err(), DbError::NotFound));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let db = db();
        let err = delete(&db, MessagingGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn mark_denied_sets_timestamp() {
        let db = db();
        let mg = upsert(&db, sample("p1")).unwrap();
        assert!(mg.denied_at.is_none());
        mark_denied(&db, mg.id).unwrap();
        let after = get(&db, mg.id).unwrap();
        assert!(after.denied_at.is_some());
    }

    #[test]
    fn mark_denied_missing_is_not_found() {
        let db = db();
        let err = mark_denied(&db, MessagingGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }
}
