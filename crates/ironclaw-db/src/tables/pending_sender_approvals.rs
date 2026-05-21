//! CRUD for `pending_sender_approvals`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, ApprovalId, MessagingGroupId, UserId};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSenderApproval {
    pub id: ApprovalId,
    pub messaging_group_id: MessagingGroupId,
    pub agent_group_id: AgentGroupId,
    pub sender_identity: String,
    pub sender_name: Option<String>,
    pub original_message: serde_json::Value,
    pub approver_user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct UpsertSenderApproval {
    pub messaging_group_id: MessagingGroupId,
    pub agent_group_id: AgentGroupId,
    pub sender_identity: String,
    pub sender_name: Option<String>,
    pub original_message: serde_json::Value,
    pub approver_user_id: UserId,
    pub title: String,
    pub options: Vec<String>,
}

fn row_to_pending_sender_approval(row: &Row<'_>) -> rusqlite::Result<PendingSenderApproval> {
    let id_str: String = row.get("id")?;
    let id = uuid::Uuid::parse_str(&id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let mg_str: String = row.get("messaging_group_id")?;
    let mg = uuid::Uuid::parse_str(&mg_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let ag_str: String = row.get("agent_group_id")?;
    let ag = uuid::Uuid::parse_str(&ag_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let approver_str: String = row.get("approver_user_id")?;
    let approver = uuid::Uuid::parse_str(&approver_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let original_str: String = row.get("original_message")?;
    let original_message: serde_json::Value = serde_json::from_str(&original_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    let options_json: String = row.get("options_json")?;
    let options: Vec<String> = serde_json::from_str(&options_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    Ok(PendingSenderApproval {
        id: ApprovalId(id),
        messaging_group_id: MessagingGroupId(mg),
        agent_group_id: AgentGroupId(ag),
        sender_identity: row.get("sender_identity")?,
        sender_name: row.get("sender_name")?,
        original_message,
        approver_user_id: UserId(approver),
        created_at,
        title: row.get("title")?,
        options,
    })
}

pub fn list(db: &CentralDb, mg: Option<MessagingGroupId>) -> Result<Vec<PendingSenderApproval>, DbError> {
    let conn = db.conn()?;
    if let Some(mg) = mg {
        let mut stmt = conn.prepare(
            "SELECT id, messaging_group_id, agent_group_id, sender_identity, sender_name,
                    original_message, approver_user_id, created_at, title, options_json
             FROM pending_sender_approvals
             WHERE messaging_group_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![mg.as_uuid().to_string()], row_to_pending_sender_approval)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, messaging_group_id, agent_group_id, sender_identity, sender_name,
                    original_message, approver_user_id, created_at, title, options_json
             FROM pending_sender_approvals
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_pending_sender_approval)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

pub fn get(db: &CentralDb, id: ApprovalId) -> Result<PendingSenderApproval, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT id, messaging_group_id, agent_group_id, sender_identity, sender_name,
                original_message, approver_user_id, created_at, title, options_json
         FROM pending_sender_approvals WHERE id = ?1",
        params![id.as_uuid().to_string()],
        row_to_pending_sender_approval,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

/// Insert or update a pending sender approval row.
///
/// Returns `(row, newly_inserted)`. `newly_inserted` is `true` when the row
/// did not exist before this call and was just created; `false` when an
/// existing row was updated in place. Callers that want to fire a one-shot
/// notification (e.g. the approvals module's in-channel prompt) should check
/// the flag and suppress duplicate notifications.
pub fn upsert(
    db: &CentralDb,
    req: UpsertSenderApproval,
) -> Result<(PendingSenderApproval, bool), DbError> {
    let conn = db.conn()?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM pending_sender_approvals
             WHERE messaging_group_id = ?1 AND sender_identity = ?2",
            params![req.messaging_group_id.as_uuid().to_string(), req.sender_identity],
            |r| r.get(0),
        )
        .optional()?;

    let original_str = req.original_message.to_string();
    let options_json = serde_json::to_string(&req.options)?;

    if let Some(id_str) = existing {
        conn.execute(
            "UPDATE pending_sender_approvals
             SET agent_group_id = ?1,
                 sender_name = ?2,
                 original_message = ?3,
                 approver_user_id = ?4,
                 title = ?5,
                 options_json = ?6
             WHERE id = ?7",
            params![
                req.agent_group_id.as_uuid().to_string(),
                req.sender_name,
                original_str,
                req.approver_user_id.as_uuid().to_string(),
                req.title,
                options_json,
                id_str,
            ],
        )?;
        let id = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| DbError::Invariant(format!("invalid uuid in pending_sender_approvals.id: {e}")))?;
        drop(conn);
        return Ok((get(db, ApprovalId(id))?, false));
    }

    let id = ApprovalId::new();
    let now = Utc::now();
    conn.execute(
        "INSERT INTO pending_sender_approvals
           (id, messaging_group_id, agent_group_id, sender_identity, sender_name,
            original_message, approver_user_id, created_at, title, options_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id.as_uuid().to_string(),
            req.messaging_group_id.as_uuid().to_string(),
            req.agent_group_id.as_uuid().to_string(),
            req.sender_identity,
            req.sender_name,
            original_str,
            req.approver_user_id.as_uuid().to_string(),
            now.to_rfc3339(),
            req.title,
            options_json,
        ],
    )?;
    Ok((
        PendingSenderApproval {
            id,
            messaging_group_id: req.messaging_group_id,
            agent_group_id: req.agent_group_id,
            sender_identity: req.sender_identity,
            sender_name: req.sender_name,
            original_message: req.original_message,
            approver_user_id: req.approver_user_id,
            created_at: now,
            title: req.title,
            options: req.options,
        },
        true,
    ))
}

/// Return `true` if a pending-sender-approval row exists for the given
/// `(messaging_group_id, sender_identity)` pair. Used by the approvals module's
/// notifier to avoid re-posting an in-channel prompt when the row was already
/// created by a previous attempt.
pub fn exists_for(
    db: &CentralDb,
    mg: MessagingGroupId,
    sender_identity: &str,
) -> Result<bool, DbError> {
    let conn = db.conn()?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pending_sender_approvals
         WHERE messaging_group_id = ?1 AND sender_identity = ?2",
        rusqlite::params![mg.as_uuid().to_string(), sender_identity],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

pub fn delete(db: &CentralDb, id: ApprovalId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM pending_sender_approvals WHERE id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use crate::tables::messaging_groups::{upsert as upsert_mg, UpsertMessagingGroup};
    use ironclaw_types::ChannelType;
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
                is_group: false,
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

    fn sample(fx: &Fixture, sender_identity: &str) -> UpsertSenderApproval {
        UpsertSenderApproval {
            messaging_group_id: fx.mg_id,
            agent_group_id: fx.ag_id,
            sender_identity: sender_identity.into(),
            sender_name: Some("Unknown".into()),
            original_message: json!({"text":"hi"}),
            approver_user_id: fx.user_id,
            title: "Approve sender?".into(),
            options: vec!["approve".into(), "deny".into()],
        }
    }

    #[test]
    fn upsert_then_get() {
        let fx = fixture();
        let (a, newly_inserted) = upsert(&fx.db, sample(&fx, "alice")).unwrap();
        assert!(newly_inserted, "first insert should be newly_inserted=true");
        let fetched = get(&fx.db, a.id).unwrap();
        assert_eq!(a, fetched);
        assert_eq!(fetched.sender_identity, "alice");
        assert_eq!(fetched.options, vec!["approve".to_string(), "deny".to_string()]);
        assert_eq!(fetched.original_message, json!({"text":"hi"}));
    }

    #[test]
    fn upsert_updates_existing_row() {
        let fx = fixture();
        let (first, first_new) = upsert(&fx.db, sample(&fx, "alice")).unwrap();
        assert!(first_new);
        let mut req = sample(&fx, "alice");
        req.title = "Renamed".into();
        req.original_message = json!({"text":"new"});
        req.sender_name = Some("Alice".into());
        let (second, second_new) = upsert(&fx.db, req).unwrap();
        assert!(!second_new, "second upsert of same identity should be newly_inserted=false");
        assert_eq!(first.id, second.id, "upsert should reuse id");
        assert_eq!(second.title, "Renamed");
        assert_eq!(second.sender_name.as_deref(), Some("Alice"));
        assert_eq!(second.original_message, json!({"text":"new"}));
    }

    #[test]
    fn upsert_newly_inserted_flag_is_false_for_update() {
        let fx = fixture();
        let (_first, first_new) = upsert(&fx.db, sample(&fx, "charlie")).unwrap();
        assert!(first_new);
        let (_second, second_new) = upsert(&fx.db, sample(&fx, "charlie")).unwrap();
        assert!(!second_new, "repeat upsert must not signal newly_inserted");
    }

    #[test]
    fn get_missing_is_not_found() {
        let fx = fixture();
        let err = get(&fx.db, ApprovalId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn list_returns_all_when_no_filter() {
        let fx = fixture();
        upsert(&fx.db, sample(&fx, "alice")).unwrap();
        upsert(&fx.db, sample(&fx, "bob")).unwrap();
        let all = list(&fx.db, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_filters_by_messaging_group() {
        let fx = fixture();
        upsert(&fx.db, sample(&fx, "alice")).unwrap();
        let other_mg = upsert_mg(
            &fx.db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("slack"),
                platform_id: "chat-2".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let mut other = sample(&fx, "alice");
        other.messaging_group_id = other_mg.id;
        upsert(&fx.db, other).unwrap();
        let filtered = list(&fx.db, Some(fx.mg_id)).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].messaging_group_id, fx.mg_id);
    }

    #[test]
    fn upsert_without_messaging_group_fails() {
        let fx = fixture();
        let mut req = sample(&fx, "alice");
        req.messaging_group_id = MessagingGroupId::new();
        let err = upsert(&fx.db, req).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn delete_works() {
        let fx = fixture();
        let (a, _) = upsert(&fx.db, sample(&fx, "alice")).unwrap();
        delete(&fx.db, a.id).unwrap();
        assert!(matches!(get(&fx.db, a.id).unwrap_err(), DbError::NotFound));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let fx = fixture();
        let err = delete(&fx.db, ApprovalId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn exists_for_returns_false_when_absent() {
        let fx = fixture();
        let found = exists_for(&fx.db, fx.mg_id, "nobody").unwrap();
        assert!(!found);
    }

    #[test]
    fn exists_for_returns_true_after_upsert() {
        let fx = fixture();
        upsert(&fx.db, sample(&fx, "carol")).unwrap();
        assert!(exists_for(&fx.db, fx.mg_id, "carol").unwrap());
    }

    #[test]
    fn exists_for_is_scoped_to_messaging_group() {
        let fx = fixture();
        upsert(&fx.db, sample(&fx, "dave")).unwrap();
        // A different messaging group should not match.
        let other_mg = crate::tables::messaging_groups::upsert(
            &fx.db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("discord"),
                platform_id: "ch-other".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        assert!(!exists_for(&fx.db, other_mg.id, "dave").unwrap());
        assert!(exists_for(&fx.db, fx.mg_id, "dave").unwrap());
    }
}
