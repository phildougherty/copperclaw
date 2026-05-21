//! CRUD for `pending_approvals`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, ApprovalId, ChannelType, SessionId};
use rusqlite::{params, OptionalExtension, Row};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ApprovalStatus::Pending),
            "approved" => Some(ApprovalStatus::Approved),
            "denied" => Some(ApprovalStatus::Denied),
            "expired" => Some(ApprovalStatus::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub approval_id: ApprovalId,
    pub session_id: Option<SessionId>,
    pub request_id: String,
    pub action: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub agent_group_id: Option<AgentGroupId>,
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub platform_message_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub status: ApprovalStatus,
    pub title: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpsertPendingApproval {
    pub session_id: Option<SessionId>,
    pub request_id: String,
    pub action: String,
    pub payload: serde_json::Value,
    pub agent_group_id: Option<AgentGroupId>,
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub platform_message_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub title: String,
    pub options: Vec<String>,
}

fn row_to_pending_approval(row: &Row<'_>) -> rusqlite::Result<PendingApproval> {
    let approval_id_str: String = row.get("approval_id")?;
    let approval_id = uuid::Uuid::parse_str(&approval_id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let session_id_opt: Option<String> = row.get("session_id")?;
    let session_id = session_id_opt
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .map(SessionId);
    let agent_group_id_opt: Option<String> = row.get("agent_group_id")?;
    let agent_group_id = agent_group_id_opt
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .map(AgentGroupId);
    let channel_type: Option<String> = row.get("channel_type")?;
    let payload_str: String = row.get("payload")?;
    let payload: serde_json::Value = serde_json::from_str(&payload_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    let expires_at_str: Option<String> = row.get("expires_at")?;
    let expires_at = expires_at_str
        .as_deref()
        .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let status_str: String = row.get("status")?;
    let status = ApprovalStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown status {status_str}").into(),
        )
    })?;
    let options_json: String = row.get("options_json")?;
    let options: Vec<String> = serde_json::from_str(&options_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    Ok(PendingApproval {
        approval_id: ApprovalId(approval_id),
        session_id,
        request_id: row.get("request_id")?,
        action: row.get("action")?,
        payload,
        created_at,
        agent_group_id,
        channel_type: channel_type.map(ChannelType::from),
        platform_id: row.get("platform_id")?,
        platform_message_id: row.get("platform_message_id")?,
        expires_at,
        status,
        title: row.get("title")?,
        options,
    })
}

pub fn list(
    db: &CentralDb,
    action: Option<&str>,
    status: Option<ApprovalStatus>,
) -> Result<Vec<PendingApproval>, DbError> {
    let conn = db.conn()?;
    let mut sql = String::from(
        "SELECT approval_id, session_id, request_id, action, payload, created_at,
                agent_group_id, channel_type, platform_id, platform_message_id,
                expires_at, status, title, options_json
         FROM pending_approvals",
    );
    let mut clauses: Vec<&str> = Vec::new();
    if action.is_some() {
        clauses.push("action = ?1");
    }
    if status.is_some() {
        if clauses.is_empty() {
            clauses.push("status = ?1");
        } else {
            clauses.push("status = ?2");
        }
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at");

    let mut stmt = conn.prepare(&sql)?;
    let rows = match (action, status) {
        (Some(a), Some(s)) => stmt.query_map(params![a, s.as_str()], row_to_pending_approval)?,
        (Some(a), None) => stmt.query_map(params![a], row_to_pending_approval)?,
        (None, Some(s)) => stmt.query_map(params![s.as_str()], row_to_pending_approval)?,
        (None, None) => stmt.query_map([], row_to_pending_approval)?,
    };
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(db: &CentralDb, id: ApprovalId) -> Result<PendingApproval, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT approval_id, session_id, request_id, action, payload, created_at,
                agent_group_id, channel_type, platform_id, platform_message_id,
                expires_at, status, title, options_json
         FROM pending_approvals WHERE approval_id = ?1",
        params![id.as_uuid().to_string()],
        row_to_pending_approval,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

pub fn upsert(db: &CentralDb, req: UpsertPendingApproval) -> Result<PendingApproval, DbError> {
    let conn = db.conn()?;
    let existing: Option<String> = conn
        .query_row(
            "SELECT approval_id FROM pending_approvals WHERE request_id = ?1 AND action = ?2",
            params![req.request_id, req.action],
            |r| r.get(0),
        )
        .optional()?;

    let payload_str = req.payload.to_string();
    let options_json = serde_json::to_string(&req.options)?;

    if let Some(id_str) = existing {
        conn.execute(
            "UPDATE pending_approvals
             SET session_id = ?1,
                 payload = ?2,
                 agent_group_id = ?3,
                 channel_type = ?4,
                 platform_id = ?5,
                 platform_message_id = ?6,
                 expires_at = ?7,
                 title = ?8,
                 options_json = ?9
             WHERE approval_id = ?10",
            params![
                req.session_id.map(|s| s.as_uuid().to_string()),
                payload_str,
                req.agent_group_id.map(|a| a.as_uuid().to_string()),
                req.channel_type.as_ref().map(ChannelType::as_str),
                req.platform_id,
                req.platform_message_id,
                req.expires_at.map(|t| t.to_rfc3339()),
                req.title,
                options_json,
                id_str,
            ],
        )?;
        let id = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| DbError::Invariant(format!("invalid uuid in pending_approvals.approval_id: {e}")))?;
        drop(conn);
        return get(db, ApprovalId(id));
    }

    let id = ApprovalId::new();
    let now = Utc::now();
    conn.execute(
        "INSERT INTO pending_approvals
           (approval_id, session_id, request_id, action, payload, created_at,
            agent_group_id, channel_type, platform_id, platform_message_id,
            expires_at, status, title, options_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', ?12, ?13)",
        params![
            id.as_uuid().to_string(),
            req.session_id.map(|s| s.as_uuid().to_string()),
            req.request_id,
            req.action,
            payload_str,
            now.to_rfc3339(),
            req.agent_group_id.map(|a| a.as_uuid().to_string()),
            req.channel_type.as_ref().map(ChannelType::as_str),
            req.platform_id,
            req.platform_message_id,
            req.expires_at.map(|t| t.to_rfc3339()),
            req.title,
            options_json,
        ],
    )?;
    Ok(PendingApproval {
        approval_id: id,
        session_id: req.session_id,
        request_id: req.request_id,
        action: req.action,
        payload: req.payload,
        created_at: now,
        agent_group_id: req.agent_group_id,
        channel_type: req.channel_type,
        platform_id: req.platform_id,
        platform_message_id: req.platform_message_id,
        expires_at: req.expires_at,
        status: ApprovalStatus::Pending,
        title: req.title,
        options: req.options,
    })
}

pub fn update_status(db: &CentralDb, id: ApprovalId, status: ApprovalStatus) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE pending_approvals SET status = ?1 WHERE approval_id = ?2",
        params![status.as_str(), id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn delete(db: &CentralDb, id: ApprovalId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM pending_approvals WHERE approval_id = ?1",
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
    use serde_json::json;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn sample(request_id: &str) -> UpsertPendingApproval {
        UpsertPendingApproval {
            session_id: None,
            request_id: request_id.into(),
            action: "send_message".into(),
            payload: json!({"text":"hi"}),
            agent_group_id: None,
            channel_type: Some(ChannelType::new("telegram")),
            platform_id: Some("chat-1".into()),
            platform_message_id: Some("msg-1".into()),
            expires_at: None,
            title: "Send?".into(),
            options: vec!["approve".into(), "deny".into()],
        }
    }

    #[test]
    fn status_round_trips_through_strings() {
        for s in [
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Denied,
            ApprovalStatus::Expired,
        ] {
            assert_eq!(ApprovalStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(ApprovalStatus::parse("bogus"), None);
    }

    #[test]
    fn upsert_then_get() {
        let db = db();
        let a = upsert(&db, sample("r1")).unwrap();
        let fetched = get(&db, a.approval_id).unwrap();
        assert_eq!(a, fetched);
        assert_eq!(fetched.status, ApprovalStatus::Pending);
        assert_eq!(fetched.title, "Send?");
        assert_eq!(fetched.options, vec!["approve".to_string(), "deny".to_string()]);
        assert_eq!(fetched.payload, json!({"text":"hi"}));
    }

    #[test]
    fn upsert_updates_existing_row() {
        let db = db();
        let first = upsert(&db, sample("r1")).unwrap();
        let mut req = sample("r1");
        req.title = "Updated".into();
        req.payload = json!({"text":"bye"});
        let second = upsert(&db, req).unwrap();
        assert_eq!(first.approval_id, second.approval_id, "upsert should reuse id");
        assert_eq!(second.title, "Updated");
        assert_eq!(second.payload, json!({"text":"bye"}));
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = db();
        let err = get(&db, ApprovalId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn list_filters_by_action() {
        let db = db();
        upsert(&db, sample("r1")).unwrap();
        let mut other = sample("r2");
        other.action = "other_action".into();
        upsert(&db, other).unwrap();
        let only = list(&db, Some("send_message"), None).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].request_id, "r1");
    }

    #[test]
    fn list_filters_by_status() {
        let db = db();
        let a = upsert(&db, sample("r1")).unwrap();
        let mut other = sample("r2");
        other.action = "other".into();
        upsert(&db, other).unwrap();
        update_status(&db, a.approval_id, ApprovalStatus::Approved).unwrap();
        let approved = list(&db, None, Some(ApprovalStatus::Approved)).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].approval_id, a.approval_id);
        let pending = list(&db, None, Some(ApprovalStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request_id, "r2");
    }

    #[test]
    fn list_filters_by_action_and_status() {
        let db = db();
        let a = upsert(&db, sample("r1")).unwrap();
        update_status(&db, a.approval_id, ApprovalStatus::Approved).unwrap();
        let mut second = sample("r2");
        second.action = "send_message".into();
        upsert(&db, second).unwrap();
        let rows = list(&db, Some("send_message"), Some(ApprovalStatus::Approved)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].approval_id, a.approval_id);
    }

    #[test]
    fn list_with_no_filters() {
        let db = db();
        upsert(&db, sample("r1")).unwrap();
        let mut other = sample("r2");
        other.action = "other".into();
        upsert(&db, other).unwrap();
        let all = list(&db, None, None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn update_status_works() {
        let db = db();
        let a = upsert(&db, sample("r1")).unwrap();
        update_status(&db, a.approval_id, ApprovalStatus::Approved).unwrap();
        let after = get(&db, a.approval_id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Approved);
    }

    #[test]
    fn update_status_missing_is_not_found() {
        let db = db();
        let err = update_status(&db, ApprovalId::new(), ApprovalStatus::Approved).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn delete_works() {
        let db = db();
        let a = upsert(&db, sample("r1")).unwrap();
        delete(&db, a.approval_id).unwrap();
        assert!(matches!(get(&db, a.approval_id).unwrap_err(), DbError::NotFound));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let db = db();
        let err = delete(&db, ApprovalId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }
}
