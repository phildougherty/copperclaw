//! CRUD for `unregistered_senders`.
//!
//! Tracks platform senders that have been seen but are not yet associated
//! with a known user. The first encounter inserts a row; subsequent
//! encounters bump `message_count` and refresh `last_seen`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, ChannelType, MessagingGroupId, UserId};
use rusqlite::{params, OptionalExtension, Row};
// Note: the primary key is `(channel_type, platform_id)` per migration
// `001_initial.sql`, which is exactly what we target in the ON CONFLICT
// clause below so the upsert collapses to one atomic statement.

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

/// First-seen insert; subsequent calls increment `message_count` and
/// update `last_seen`.
///
/// Implemented as a single atomic `INSERT ... ON CONFLICT DO UPDATE`
/// against the `(channel_type, platform_id)` primary key. The previous
/// SELECT-then-`INSERT`/`UPDATE` shape raced when two concurrent
/// first-time inbounds for the same sender hit the pooled writer at
/// once — both observed missing row, both `INSERT`ed, the loser
/// bubbled a `UNIQUE` violation and the router dead-lettered the
/// inbound. The atomic form turns the race into a deterministic
/// increment.
pub fn upsert(db: &CentralDb, req: UpsertUnregisteredSender) -> Result<UnregisteredSender, DbError> {
    // Destructure once so the SQL bindings are obviously moves (instead
    // of looking like field reads on an owned value clippy can't tell
    // are consumed). Keeps the existing public-API by-value signature.
    let UpsertUnregisteredSender {
        channel_type,
        platform_id,
        user_id,
        sender_name,
        reason,
        messaging_group_id,
        agent_group_id,
    } = req;
    let conn = db.conn()?;
    let now = Utc::now();
    conn.execute(
        "INSERT INTO unregistered_senders
           (channel_type, platform_id, user_id, sender_name, reason,
            messaging_group_id, agent_group_id, message_count,
            first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)
         ON CONFLICT(channel_type, platform_id) DO UPDATE SET
           user_id            = COALESCE(excluded.user_id, user_id),
           sender_name        = COALESCE(excluded.sender_name, sender_name),
           reason             = excluded.reason,
           messaging_group_id = COALESCE(excluded.messaging_group_id, messaging_group_id),
           agent_group_id     = COALESCE(excluded.agent_group_id, agent_group_id),
           message_count      = message_count + 1,
           last_seen          = excluded.last_seen",
        params![
            channel_type.as_str(),
            platform_id,
            user_id.map(|u| u.as_uuid().to_string()),
            sender_name,
            reason,
            messaging_group_id.map(|m| m.as_uuid().to_string()),
            agent_group_id.map(|a| a.as_uuid().to_string()),
            now.to_rfc3339(),
        ],
    )?;
    drop(conn);
    get(db, &channel_type, &platform_id)?
        .ok_or_else(|| DbError::invariant("unregistered_senders row missing immediately after upsert"))
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

    /// Race regression: 16 concurrent upserts against the same
    /// `(channel_type, platform_id)` against a real file-backed pool
    /// (max=8) must collapse to one row with `message_count == 16` and
    /// never bubble a UNIQUE violation. Pre-fix the SELECT-then-INSERT
    /// shape would have several losers raise
    /// `SQLITE_CONSTRAINT_PRIMARYKEY` and the router would
    /// dead-letter their inbounds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn upsert_is_atomic_under_concurrent_writers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("copperclaw.db");
        let db = CentralDb::open(&path).unwrap();
        let channel = ChannelType::new("telegram");
        let platform_id = "shared-platform-id";

        let mut handles = Vec::new();
        for _ in 0..16 {
            let db = db.clone();
            let channel = channel.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                upsert(
                    &db,
                    UpsertUnregisteredSender {
                        channel_type: channel,
                        platform_id: platform_id.into(),
                        user_id: None,
                        sender_name: Some("racer".into()),
                        reason: "unknown_user".into(),
                        messaging_group_id: None,
                        agent_group_id: None,
                    },
                )
            }));
        }

        for h in handles {
            // Both layers must succeed: the join, and the DB call. If
            // the loser of a race bubbled a UNIQUE violation we'd see
            // a `DbError::Sqlite(...)` here and the test would fail.
            h.await
                .expect("spawn_blocking task panicked")
                .expect("upsert returned a DB error under contention");
        }

        let rows = list(&db, None).unwrap();
        assert_eq!(rows.len(), 1, "exactly one row should exist after 16 concurrent upserts");
        assert_eq!(rows[0].platform_id, platform_id);
        assert_eq!(rows[0].message_count, 16, "every concurrent call must contribute to the count");
    }
}
