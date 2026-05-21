//! CRUD for `unregistered_senders`.
//!
//! Tracks platform senders that have been seen but are not yet associated
//! with a known user. The first encounter inserts a row; subsequent
//! encounters bump `message_count` and refresh `last_seen`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, ChannelType, MessagingGroupId, UserId};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnregisteredSender {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub user_id: Option<UserId>,
    pub sender_name: Option<String>,
    pub reason: String,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub agent_group_id: Option<AgentGroupId>,
    pub message_count: u32,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UpsertUnregisteredSender {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub user_id: Option<UserId>,
    pub sender_name: Option<String>,
    pub reason: String,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub agent_group_id: Option<AgentGroupId>,
}

fn parse_uuid_opt(s: Option<String>) -> rusqlite::Result<Option<uuid::Uuid>> {
    match s {
        None => Ok(None),
        Some(s) => uuid::Uuid::parse_str(&s)
            .map(Some)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))),
    }
}

fn row_to_unregistered_sender(row: &Row<'_>) -> rusqlite::Result<UnregisteredSender> {
    let channel_type_str: String = row.get("channel_type")?;
    let first_seen_str: String = row.get("first_seen")?;
    let first_seen = DateTime::parse_from_rfc3339(&first_seen_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    let last_seen_str: String = row.get("last_seen")?;
    let last_seen = DateTime::parse_from_rfc3339(&last_seen_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    let user_id_str: Option<String> = row.get("user_id")?;
    let user_id = parse_uuid_opt(user_id_str)?.map(UserId);
    let messaging_group_id_str: Option<String> = row.get("messaging_group_id")?;
    let messaging_group_id = parse_uuid_opt(messaging_group_id_str)?.map(MessagingGroupId);
    let agent_group_id_str: Option<String> = row.get("agent_group_id")?;
    let agent_group_id = parse_uuid_opt(agent_group_id_str)?.map(AgentGroupId);
    let message_count_i: i64 = row.get("message_count")?;
    Ok(UnregisteredSender {
        channel_type: ChannelType::new(channel_type_str),
        platform_id: row.get("platform_id")?,
        user_id,
        sender_name: row.get("sender_name")?,
        reason: row.get("reason")?,
        messaging_group_id,
        agent_group_id,
        message_count: u32::try_from(message_count_i).unwrap_or(0),
        first_seen,
        last_seen,
    })
}

/// First-seen insert; subsequent calls increment `message_count` and update `last_seen`.
pub fn upsert(db: &CentralDb, req: UpsertUnregisteredSender) -> Result<UnregisteredSender, DbError> {
    let conn = db.conn()?;
    let now = Utc::now();
    let existing: Option<i64> = conn
        .query_row(
            "SELECT message_count FROM unregistered_senders
             WHERE channel_type = ?1 AND platform_id = ?2",
            params![req.channel_type.as_str(), req.platform_id],
            |r| r.get(0),
        )
        .optional()?;

    if existing.is_some() {
        conn.execute(
            "UPDATE unregistered_senders
             SET user_id = COALESCE(?1, user_id),
                 sender_name = COALESCE(?2, sender_name),
                 reason = ?3,
                 messaging_group_id = COALESCE(?4, messaging_group_id),
                 agent_group_id = COALESCE(?5, agent_group_id),
                 message_count = message_count + 1,
                 last_seen = ?6
             WHERE channel_type = ?7 AND platform_id = ?8",
            params![
                req.user_id.map(|u| u.as_uuid().to_string()),
                req.sender_name,
                req.reason,
                req.messaging_group_id.map(|m| m.as_uuid().to_string()),
                req.agent_group_id.map(|a| a.as_uuid().to_string()),
                now.to_rfc3339(),
                req.channel_type.as_str(),
                req.platform_id,
            ],
        )?;
        drop(conn);
        return get(db, &req.channel_type, &req.platform_id)?.ok_or_else(|| {
            DbError::invariant("unregistered_senders row missing immediately after upsert")
        });
    }

    conn.execute(
        "INSERT INTO unregistered_senders
           (channel_type, platform_id, user_id, sender_name, reason,
            messaging_group_id, agent_group_id, message_count,
            first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
        params![
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
    Ok(UnregisteredSender {
        channel_type: req.channel_type,
        platform_id: req.platform_id,
        user_id: req.user_id,
        sender_name: req.sender_name,
        reason: req.reason,
        messaging_group_id: req.messaging_group_id,
        agent_group_id: req.agent_group_id,
        message_count: 1,
        first_seen: now,
        last_seen: now,
    })
}

pub fn list(db: &CentralDb, since: Option<DateTime<Utc>>) -> Result<Vec<UnregisteredSender>, DbError> {
    let conn = db.conn()?;
    let rows: Vec<UnregisteredSender> = if let Some(ts) = since {
        let mut stmt = conn.prepare(
            "SELECT channel_type, platform_id, user_id, sender_name, reason,
                    messaging_group_id, agent_group_id, message_count,
                    first_seen, last_seen
             FROM unregistered_senders
             WHERE last_seen >= ?1
             ORDER BY last_seen",
        )?;
        let mapped = stmt.query_map(params![ts.to_rfc3339()], row_to_unregistered_sender)?;
        let out: rusqlite::Result<Vec<_>> = mapped.collect();
        out?
    } else {
        let mut stmt = conn.prepare(
            "SELECT channel_type, platform_id, user_id, sender_name, reason,
                    messaging_group_id, agent_group_id, message_count,
                    first_seen, last_seen
             FROM unregistered_senders
             ORDER BY last_seen",
        )?;
        let mapped = stmt.query_map([], row_to_unregistered_sender)?;
        let out: rusqlite::Result<Vec<_>> = mapped.collect();
        out?
    };
    Ok(rows)
}

pub fn get(
    db: &CentralDb,
    channel_type: &ChannelType,
    platform_id: &str,
) -> Result<Option<UnregisteredSender>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT channel_type, platform_id, user_id, sender_name, reason,
                    messaging_group_id, agent_group_id, message_count,
                    first_seen, last_seen
             FROM unregistered_senders
             WHERE channel_type = ?1 AND platform_id = ?2",
            params![channel_type.as_str(), platform_id],
            row_to_unregistered_sender,
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn sample(platform_id: &str) -> UpsertUnregisteredSender {
        UpsertUnregisteredSender {
            channel_type: ChannelType::new("telegram"),
            platform_id: platform_id.into(),
            user_id: None,
            sender_name: Some("Anonymous".into()),
            reason: "unknown_user".into(),
            messaging_group_id: None,
            agent_group_id: None,
        }
    }

    #[test]
    fn upsert_inserts_row_on_first_call() {
        let db = db();
        let row = upsert(&db, sample("p1")).unwrap();
        assert_eq!(row.platform_id, "p1");
        assert_eq!(row.message_count, 1);
        assert_eq!(row.first_seen, row.last_seen);
        assert_eq!(row.reason, "unknown_user");
    }

    #[test]
    fn upsert_increments_count_and_updates_last_seen() {
        let db = db();
        let first = upsert(&db, sample("p1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = upsert(&db, sample("p1")).unwrap();
        assert_eq!(second.message_count, 2);
        assert_eq!(second.first_seen, first.first_seen);
        assert!(second.last_seen > first.last_seen);
    }

    #[test]
    fn upsert_preserves_existing_fields_when_none_supplied() {
        let db = db();
        let mut req = sample("p1");
        req.sender_name = Some("Original".into());
        let _ = upsert(&db, req).unwrap();
        let mut update = sample("p1");
        update.sender_name = None;
        let second = upsert(&db, update).unwrap();
        assert_eq!(second.sender_name.as_deref(), Some("Original"));
    }

    #[test]
    fn upsert_replaces_reason_each_call() {
        let db = db();
        let mut req = sample("p1");
        req.reason = "blocked".into();
        let _ = upsert(&db, req).unwrap();
        let mut req2 = sample("p1");
        req2.reason = "rate_limited".into();
        let second = upsert(&db, req2).unwrap();
        assert_eq!(second.reason, "rate_limited");
    }

    #[test]
    fn upsert_associates_ids_when_supplied() {
        use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};
        use crate::tables::messaging_groups::{upsert as upsert_mg, UpsertMessagingGroup};
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
        let row = upsert(&db, req).unwrap();
        assert_eq!(row.user_id, Some(user));
        assert_eq!(row.messaging_group_id, Some(mg.id));
        assert_eq!(row.agent_group_id, Some(ag.id));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = db();
        let ct = ChannelType::new("telegram");
        assert!(get(&db, &ct, "absent").unwrap().is_none());
    }

    #[test]
    fn get_distinguishes_channel_types() {
        let db = db();
        upsert(&db, sample("p1")).unwrap();
        let other = ChannelType::new("slack");
        assert!(get(&db, &other, "p1").unwrap().is_none());
    }

    #[test]
    fn list_returns_all_when_since_none() {
        let db = db();
        for i in 0..3 {
            upsert(&db, sample(&format!("p{i}"))).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let rows = list(&db, None).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn list_filters_by_since() {
        let db = db();
        let _ = upsert(&db, sample("p1")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let cutoff = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _ = upsert(&db, sample("p2")).unwrap();
        let rows = list(&db, Some(cutoff)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].platform_id, "p2");
    }

    #[test]
    fn list_empty_when_no_rows() {
        let db = db();
        assert!(list(&db, None).unwrap().is_empty());
    }
}
