//! CRUD for `pending_channel_approvals`.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, MessagingGroupId, UserId};
use rusqlite::{OptionalExtension, Row, params};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChannelApproval {
    pub messaging_group_id: MessagingGroupId,
    pub agent_group_id: AgentGroupId,
    pub original_message: serde_json::Value,
    pub approver_user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct UpsertChannelApproval {
    pub messaging_group_id: MessagingGroupId,
    pub agent_group_id: AgentGroupId,
    pub original_message: serde_json::Value,
    pub approver_user_id: UserId,
    pub title: String,
    pub options: Vec<String>,
}

fn row_to_pending_channel_approval(row: &Row<'_>) -> rusqlite::Result<PendingChannelApproval> {
    let mg_str: String = row.get("messaging_group_id")?;
    let mg = uuid::Uuid::parse_str(&mg_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let ag_str: String = row.get("agent_group_id")?;
    let ag = uuid::Uuid::parse_str(&ag_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let approver_str: String = row.get("approver_user_id")?;
    let approver = uuid::Uuid::parse_str(&approver_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let original_str: String = row.get("original_message")?;
    let original_message: serde_json::Value = serde_json::from_str(&original_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let options_json: String = row.get("options_json")?;
    let options: Vec<String> = serde_json::from_str(&options_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(PendingChannelApproval {
        messaging_group_id: MessagingGroupId(mg),
        agent_group_id: AgentGroupId(ag),
        original_message,
        approver_user_id: UserId(approver),
        created_at,
        title: row.get("title")?,
        options,
    })
}

pub fn list(db: &CentralDb) -> Result<Vec<PendingChannelApproval>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT messaging_group_id, agent_group_id, original_message,
                approver_user_id, created_at, title, options_json
         FROM pending_channel_approvals
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], row_to_pending_channel_approval)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(db: &CentralDb, mg: MessagingGroupId) -> Result<PendingChannelApproval, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT messaging_group_id, agent_group_id, original_message,
                approver_user_id, created_at, title, options_json
         FROM pending_channel_approvals WHERE messaging_group_id = ?1",
        params![mg.as_uuid().to_string()],
        row_to_pending_channel_approval,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

pub fn upsert(
    db: &CentralDb,
    req: UpsertChannelApproval,
) -> Result<PendingChannelApproval, DbError> {
    let conn = db.conn()?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT messaging_group_id FROM pending_channel_approvals WHERE messaging_group_id = ?1",
            params![req.messaging_group_id.as_uuid().to_string()],
            |r| r.get(0),
        )
        .optional()?;

    let original_str = req.original_message.to_string();
    let options_json = serde_json::to_string(&req.options)?;

    if existing.is_some() {
        conn.execute(
            "UPDATE pending_channel_approvals
             SET agent_group_id = ?1,
                 original_message = ?2,
                 approver_user_id = ?3,
                 title = ?4,
                 options_json = ?5
             WHERE messaging_group_id = ?6",
            params![
                req.agent_group_id.as_uuid().to_string(),
                original_str,
                req.approver_user_id.as_uuid().to_string(),
                req.title,
                options_json,
                req.messaging_group_id.as_uuid().to_string(),
            ],
        )?;
        drop(conn);
        return get(db, req.messaging_group_id);
    }

    let now = Utc::now();
    conn.execute(
        "INSERT INTO pending_channel_approvals
           (messaging_group_id, agent_group_id, original_message,
            approver_user_id, created_at, title, options_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            req.messaging_group_id.as_uuid().to_string(),
            req.agent_group_id.as_uuid().to_string(),
            original_str,
            req.approver_user_id.as_uuid().to_string(),
            now.to_rfc3339(),
            req.title,
            options_json,
        ],
    )?;
    Ok(PendingChannelApproval {
        messaging_group_id: req.messaging_group_id,
        agent_group_id: req.agent_group_id,
        original_message: req.original_message,
        approver_user_id: req.approver_user_id,
        created_at: now,
        title: req.title,
        options: req.options,
    })
}

pub fn delete(db: &CentralDb, mg: MessagingGroupId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM pending_channel_approvals WHERE messaging_group_id = ?1",
        params![mg.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use crate::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
    use copperclaw_types::ChannelType;
    use serde_json::json;

    struct Fixture {
        db: CentralDb,
        mg_id: MessagingGroupId,
        ag_id: AgentGroupId,
        user_id: UserId,
    }

    fn fixture() -> Fixture {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "greeter".into(),
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
                name: Some("Test".into()),
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let user_id = UserId::new();
        let conn = db.conn().unwrap();
        conn.execute(
            "INSERT INTO users (id, kind, display_name, created_at)
             VALUES (?1, 'person', 'tester', ?2)",
            params![user_id.as_uuid().to_string(), Utc::now().to_rfc3339()],
        )
        .unwrap();
        drop(conn);
        Fixture {
            db,
            mg_id: mg.id,
            ag_id: ag.id,
            user_id,
        }
    }

    fn sample(fx: &Fixture) -> UpsertChannelApproval {
        UpsertChannelApproval {
            messaging_group_id: fx.mg_id,
            agent_group_id: fx.ag_id,
            original_message: json!({"text":"hi"}),
            approver_user_id: fx.user_id,
            title: "Approve channel?".into(),
            options: vec!["approve".into(), "deny".into()],
        }
    }

    fn extra_mg(fx: &Fixture, platform_id: &str) -> MessagingGroupId {
        upsert_mg(
            &fx.db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("slack"),
                platform_id: platform_id.into(),
                name: None,
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn upsert_then_get() {
        let fx = fixture();
        let a = upsert(&fx.db, sample(&fx)).unwrap();
        let fetched = get(&fx.db, a.messaging_group_id).unwrap();
        assert_eq!(a, fetched);
        assert_eq!(fetched.title, "Approve channel?");
        assert_eq!(
            fetched.options,
            vec!["approve".to_string(), "deny".to_string()]
        );
        assert_eq!(fetched.original_message, json!({"text":"hi"}));
    }

    #[test]
    fn upsert_updates_existing_row() {
        let fx = fixture();
        let first = upsert(&fx.db, sample(&fx)).unwrap();
        let mut req = sample(&fx);
        req.title = "Updated".into();
        req.original_message = json!({"text":"new"});
        let second = upsert(&fx.db, req).unwrap();
        assert_eq!(first.messaging_group_id, second.messaging_group_id);
        assert_eq!(second.title, "Updated");
        assert_eq!(second.original_message, json!({"text":"new"}));
    }

    #[test]
    fn get_missing_is_not_found() {
        let fx = fixture();
        let err = get(&fx.db, MessagingGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn list_is_ordered_by_created_at() {
        let fx = fixture();
        let mg2 = extra_mg(&fx, "chat-2");
        let mg3 = extra_mg(&fx, "chat-3");
        upsert(&fx.db, sample(&fx)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut req2 = sample(&fx);
        req2.messaging_group_id = mg2;
        upsert(&fx.db, req2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut req3 = sample(&fx);
        req3.messaging_group_id = mg3;
        upsert(&fx.db, req3).unwrap();

        let rows = list(&fx.db).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].messaging_group_id, fx.mg_id);
        assert_eq!(rows[2].messaging_group_id, mg3);
    }

    #[test]
    fn upsert_without_messaging_group_fails() {
        let fx = fixture();
        let mut req = sample(&fx);
        req.messaging_group_id = MessagingGroupId::new();
        let err = upsert(&fx.db, req).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn delete_works() {
        let fx = fixture();
        let a = upsert(&fx.db, sample(&fx)).unwrap();
        delete(&fx.db, a.messaging_group_id).unwrap();
        assert!(matches!(
            get(&fx.db, a.messaging_group_id).unwrap_err(),
            DbError::NotFound
        ));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let fx = fixture();
        let err = delete(&fx.db, MessagingGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }
}
