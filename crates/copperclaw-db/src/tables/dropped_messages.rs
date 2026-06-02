//! CRUD for `dropped_messages`.
//!
//! Append-only audit log of inbound messages the router refused to deliver.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, ChannelType, MessagingGroupId, UserId};
use rusqlite::{Row, params};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedMessage {
    pub id: Uuid,
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub user_id: Option<UserId>,
    pub sender_name: Option<String>,
    pub reason: String,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub agent_group_id: Option<AgentGroupId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct InsertDroppedMessage {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub user_id: Option<UserId>,
    pub sender_name: Option<String>,
    pub reason: String,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub agent_group_id: Option<AgentGroupId>,
}

fn parse_uuid_opt(s: Option<String>) -> rusqlite::Result<Option<Uuid>> {
    match s {
        None => Ok(None),
        Some(s) => Uuid::parse_str(&s).map(Some).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        }),
    }
}

fn row_to_dropped_message(row: &Row<'_>) -> rusqlite::Result<DroppedMessage> {
    let id_str: String = row.get("id")?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let channel_type_str: String = row.get("channel_type")?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let user_id_str: Option<String> = row.get("user_id")?;
    let user_id = parse_uuid_opt(user_id_str)?.map(UserId);
    let messaging_group_id_str: Option<String> = row.get("messaging_group_id")?;
    let messaging_group_id = parse_uuid_opt(messaging_group_id_str)?.map(MessagingGroupId);
    let agent_group_id_str: Option<String> = row.get("agent_group_id")?;
    let agent_group_id = parse_uuid_opt(agent_group_id_str)?.map(AgentGroupId);
    Ok(DroppedMessage {
        id,
        channel_type: ChannelType::new(channel_type_str),
        platform_id: row.get("platform_id")?,
        user_id,
        sender_name: row.get("sender_name")?,
        reason: row.get("reason")?,
        messaging_group_id,
        agent_group_id,
        created_at,
    })
}

pub fn insert(db: &CentralDb, req: InsertDroppedMessage) -> Result<DroppedMessage, DbError> {
    let id = Uuid::now_v7();
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO dropped_messages
           (id, channel_type, platform_id, user_id, sender_name, reason,
            messaging_group_id, agent_group_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id.to_string(),
            req.channel_type.as_str(),
            req.platform_id,
            req.user_id.map(|u| u.as_uuid().to_string()),
            req.sender_name,
            req.reason,
            req.messaging_group_id.map(|m| m.as_uuid().to_string()),
            req.agent_group_id.map(|a| a.as_uuid().to_string()),
            now.to_rfc3339(),
        ],
    )?;
    Ok(DroppedMessage {
        id,
        channel_type: req.channel_type,
        platform_id: req.platform_id,
        user_id: req.user_id,
        sender_name: req.sender_name,
        reason: req.reason,
        messaging_group_id: req.messaging_group_id,
        agent_group_id: req.agent_group_id,
        created_at: now,
    })
}

pub fn list(db: &CentralDb, since: Option<DateTime<Utc>>) -> Result<Vec<DroppedMessage>, DbError> {
    let conn = db.conn()?;
    let rows: Vec<DroppedMessage> = if let Some(ts) = since {
        let mut stmt = conn.prepare(
            "SELECT id, channel_type, platform_id, user_id, sender_name, reason,
                    messaging_group_id, agent_group_id, created_at
             FROM dropped_messages
             WHERE created_at >= ?1
             ORDER BY created_at",
        )?;
        let mapped = stmt.query_map(params![ts.to_rfc3339()], row_to_dropped_message)?;
        let out: rusqlite::Result<Vec<_>> = mapped.collect();
        out?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, channel_type, platform_id, user_id, sender_name, reason,
                    messaging_group_id, agent_group_id, created_at
             FROM dropped_messages
             ORDER BY created_at",
        )?;
        let mapped = stmt.query_map([], row_to_dropped_message)?;
        let out: rusqlite::Result<Vec<_>> = mapped.collect();
        out?
    };
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn sample(platform_id: &str) -> InsertDroppedMessage {
        InsertDroppedMessage {
            channel_type: ChannelType::new("telegram"),
            platform_id: platform_id.into(),
            user_id: None,
            sender_name: Some("Anon".into()),
            reason: "unknown_user".into(),
            messaging_group_id: None,
            agent_group_id: None,
        }
    }

    #[test]
    fn insert_returns_populated_row() {
        let db = db();
        let row = insert(&db, sample("p1")).unwrap();
        assert_eq!(row.platform_id, "p1");
        assert_eq!(row.reason, "unknown_user");
        assert_eq!(row.channel_type, ChannelType::new("telegram"));
        assert_eq!(row.sender_name.as_deref(), Some("Anon"));
    }

    #[test]
    fn insert_assigns_unique_ids() {
        let db = db();
        let a = insert(&db, sample("p1")).unwrap();
        let b = insert(&db, sample("p1")).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn insert_stores_optional_ids() {
        use crate::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use crate::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
        let db = db();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "chat-1".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let user = UserId::new();
        let mut req = sample("p1");
        req.user_id = Some(user);
        req.messaging_group_id = Some(mg.id);
        req.agent_group_id = Some(ag.id);
        let row = insert(&db, req).unwrap();
        assert_eq!(row.user_id, Some(user));
        assert_eq!(row.messaging_group_id, Some(mg.id));
        assert_eq!(row.agent_group_id, Some(ag.id));
    }

    #[test]
    fn list_returns_all_when_since_none() {
        let db = db();
        for i in 0..3 {
            insert(&db, sample(&format!("p{i}"))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let rows = list(&db, None).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].platform_id, "p0");
        assert_eq!(rows[2].platform_id, "p2");
    }

    #[test]
    fn list_filters_by_since() {
        let db = db();
        let _ = insert(&db, sample("p1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let cutoff = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = insert(&db, sample("p2")).unwrap();
        let rows = list(&db, Some(cutoff)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].platform_id, "p2");
    }

    #[test]
    fn list_empty_when_no_rows() {
        let db = db();
        assert!(list(&db, None).unwrap().is_empty());
    }

    #[test]
    fn duplicate_id_insert_fails() {
        // We can't normally trigger a collision via the public API, so simulate
        // by writing a row with a fixed id and then inserting another row with
        // the same id directly.
        let db = db();
        let row = insert(&db, sample("p1")).unwrap();
        let conn = db.conn().unwrap();
        let err = conn
            .execute(
                "INSERT INTO dropped_messages
                   (id, channel_type, platform_id, reason, created_at)
                 VALUES (?1, 'telegram', 'p2', 'r', ?2)",
                params![row.id.to_string(), Utc::now().to_rfc3339()],
            )
            .unwrap_err();
        let err: DbError = err.into();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn corrupt_id_in_row_errors() {
        let db = db();
        let conn = db.conn().unwrap();
        conn.execute(
            "INSERT INTO dropped_messages
               (id, channel_type, platform_id, reason, created_at)
             VALUES ('not-a-uuid', 'telegram', 'p1', 'r', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        drop(conn);
        let err = list(&db, None).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
