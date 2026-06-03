//! CRUD for `pending_approvals`.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, ApprovalId, ChannelType, SessionId};
use rusqlite::{OptionalExtension, Row, params};

/// Default time-to-live for a freshly recorded pending approval: ~1 hour.
/// Callers that don't supply an explicit `expires_at` get this window so an
/// unanswered approval lapses instead of lingering as a live grant forever.
pub const DEFAULT_APPROVAL_TTL: chrono::Duration = chrono::Duration::hours(1);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
    Revoked,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Expired => "expired",
            ApprovalStatus::Revoked => "revoked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ApprovalStatus::Pending),
            "approved" => Some(ApprovalStatus::Approved),
            "denied" => Some(ApprovalStatus::Denied),
            "expired" => Some(ApprovalStatus::Expired),
            "revoked" => Some(ApprovalStatus::Revoked),
            _ => None,
        }
    }

    /// True when this is a settled (non-actionable) state. Terminal rows
    /// cannot be approved, denied, or revoked again.
    pub fn is_terminal(self) -> bool {
        !matches!(self, ApprovalStatus::Pending)
    }
}

/// One outcome recorded against a pending approval. Mirrors the
/// `approval_decisions` table (migration `017_approval_decisions`).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DecisionOutcome {
    Approve,
    Deny,
    Expire,
    Revoke,
}

impl DecisionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionOutcome::Approve => "approve",
            DecisionOutcome::Deny => "deny",
            DecisionOutcome::Expire => "expire",
            DecisionOutcome::Revoke => "revoke",
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

impl PendingApproval {
    /// True when the row carries an `expires_at` that is at or before `now`.
    /// Independent of `status` — a row that has already been resolved can
    /// still report "its window has passed" without that being meaningful.
    #[must_use]
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now)
    }

    /// True when the row can still be acted upon: it is `Pending` and has not
    /// passed its expiry window. An expired-but-still-`pending` row (the
    /// expiry sweep hasn't run yet) is NOT actionable.
    #[must_use]
    pub fn is_actionable_at(&self, now: DateTime<Utc>) -> bool {
        self.status == ApprovalStatus::Pending && !self.is_expired_at(now)
    }
}

/// One row of the append-only `approval_decisions` log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalDecision {
    pub id: i64,
    pub approval_id: ApprovalId,
    pub action: String,
    pub outcome: DecisionOutcome,
    pub decided_by: String,
    pub reason: Option<String>,
    pub decided_at: DateTime<Utc>,
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
    let approval_id = uuid::Uuid::parse_str(&approval_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let session_id_opt: Option<String> = row.get("session_id")?;
    let session_id = session_id_opt
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .map(SessionId);
    let agent_group_id_opt: Option<String> = row.get("agent_group_id")?;
    let agent_group_id = agent_group_id_opt
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .map(AgentGroupId);
    let channel_type: Option<String> = row.get("channel_type")?;
    let payload_str: String = row.get("payload")?;
    let payload: serde_json::Value = serde_json::from_str(&payload_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let expires_at_str: Option<String> = row.get("expires_at")?;
    let expires_at = expires_at_str
        .as_deref()
        .map(|s| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let status_str: String = row.get("status")?;
    let status = ApprovalStatus::parse(&status_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown status {status_str}").into(),
        )
    })?;
    let options_json: String = row.get("options_json")?;
    let options: Vec<String> = serde_json::from_str(&options_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
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

/// Insert a new pending approval, or update the existing pending row
/// for the same `(request_id, action)` in place.
///
/// Implemented as a single atomic `INSERT ... ON CONFLICT DO UPDATE`
/// against the partial unique index
/// `pending_approvals_request_action_uq` (migration
/// `016_pending_approvals_unique.sql`, scoped to `status = 'pending'`).
/// The previous SELECT-then-INSERT/UPDATE shape raced on a pooled
/// writer — two concurrent upserts for the same key could both miss
/// the SELECT and both INSERT, producing silent duplicate pending
/// rows. The atomic form collapses the race.
///
/// The conflict scope is `status = 'pending'` so already-terminal
/// rows (denied / approved / expired) remain in place as historical
/// receipts even if the same `(request_id, action)` is re-asked
/// later — the re-ask gets a fresh pending row, the old terminal
/// row is preserved.
pub fn upsert(db: &CentralDb, req: UpsertPendingApproval) -> Result<PendingApproval, DbError> {
    // Destructure once so SQL bindings are obvious moves, satisfying
    // clippy's `needless_pass_by_value` while keeping the public API.
    let UpsertPendingApproval {
        session_id,
        request_id,
        action,
        payload,
        agent_group_id,
        channel_type,
        platform_id,
        platform_message_id,
        expires_at,
        title,
        options,
    } = req;
    let conn = db.conn()?;
    let payload_str = payload.to_string();
    let options_json = serde_json::to_string(&options)?;
    let id = ApprovalId::new();
    let now = Utc::now();
    // A pending approval that is never answered should lapse rather than
    // linger as a live grant forever. When the caller doesn't pin an
    // explicit deadline, default to ~1h from now.
    let expires_at = expires_at.or_else(|| Some(now + DEFAULT_APPROVAL_TTL));

    conn.execute(
        "INSERT INTO pending_approvals
           (approval_id, session_id, request_id, action, payload, created_at,
            agent_group_id, channel_type, platform_id, platform_message_id,
            expires_at, status, title, options_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', ?12, ?13)
         ON CONFLICT(request_id, action) WHERE status = 'pending' DO UPDATE SET
           session_id          = excluded.session_id,
           payload             = excluded.payload,
           agent_group_id      = excluded.agent_group_id,
           channel_type        = excluded.channel_type,
           platform_id         = excluded.platform_id,
           platform_message_id = excluded.platform_message_id,
           expires_at          = excluded.expires_at,
           title               = excluded.title,
           options_json        = excluded.options_json",
        params![
            id.as_uuid().to_string(),
            session_id.map(|s| s.as_uuid().to_string()),
            request_id,
            action,
            payload_str,
            now.to_rfc3339(),
            agent_group_id.map(|a| a.as_uuid().to_string()),
            channel_type.as_ref().map(ChannelType::as_str),
            platform_id,
            platform_message_id,
            expires_at.map(|t| t.to_rfc3339()),
            title,
            options_json,
        ],
    )?;

    // Reload by the natural key (request_id, action) restricted to the
    // pending row — that's the row we just touched, regardless of
    // whether the INSERT or the DO UPDATE branch fired. Reusing the
    // existing row parser keeps the field marshalling consistent.
    let id_str: String = conn.query_row(
        "SELECT approval_id FROM pending_approvals
         WHERE request_id = ?1 AND action = ?2 AND status = 'pending'",
        params![request_id, action],
        |r| r.get(0),
    )?;
    let parsed = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        DbError::Invariant(format!(
            "invalid uuid in pending_approvals.approval_id: {e}"
        ))
    })?;
    drop(conn);
    get(db, ApprovalId(parsed))
}

pub fn update_status(
    db: &CentralDb,
    id: ApprovalId,
    status: ApprovalStatus,
) -> Result<(), DbError> {
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

/// Append one decision receipt to the append-only `approval_decisions`
/// log. Never updates or deletes — every approve / deny / expire / revoke
/// lands a fresh row. `action` is copied from the pending row for
/// query convenience; `decided_by` is a free-text actor label.
pub fn record_decision(
    db: &CentralDb,
    approval_id: ApprovalId,
    action: &str,
    outcome: DecisionOutcome,
    decided_by: &str,
    reason: Option<&str>,
) -> Result<(), DbError> {
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO approval_decisions
           (approval_id, action, outcome, decided_by, reason, decided_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            approval_id.as_uuid().to_string(),
            action,
            outcome.as_str(),
            decided_by,
            reason,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn row_to_decision(row: &Row<'_>) -> rusqlite::Result<ApprovalDecision> {
    let approval_id_str: String = row.get("approval_id")?;
    let approval_id = uuid::Uuid::parse_str(&approval_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let outcome_str: String = row.get("outcome")?;
    let outcome = match outcome_str.as_str() {
        "approve" => DecisionOutcome::Approve,
        "deny" => DecisionOutcome::Deny,
        "expire" => DecisionOutcome::Expire,
        "revoke" => DecisionOutcome::Revoke,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown decision outcome {other}").into(),
            ));
        }
    };
    let decided_at_str: String = row.get("decided_at")?;
    let decided_at = DateTime::parse_from_rfc3339(&decided_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    Ok(ApprovalDecision {
        id: row.get("id")?,
        approval_id: ApprovalId(approval_id),
        action: row.get("action")?,
        outcome,
        decided_by: row.get("decided_by")?,
        reason: row.get("reason")?,
        decided_at,
    })
}

/// List decision receipts newest-first. `approval_id = Some(..)` scopes to
/// one approval's history; `None` returns the global log capped at `limit`.
pub fn list_decisions(
    db: &CentralDb,
    approval_id: Option<ApprovalId>,
    limit: i64,
) -> Result<Vec<ApprovalDecision>, DbError> {
    let conn = db.conn()?;
    if let Some(id) = approval_id {
        let mut stmt = conn.prepare(
            "SELECT id, approval_id, action, outcome, decided_by, reason, decided_at
             FROM approval_decisions
             WHERE approval_id = ?1
             ORDER BY decided_at DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![id.as_uuid().to_string(), limit], row_to_decision)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, approval_id, action, outcome, decided_by, reason, decided_at
             FROM approval_decisions
             ORDER BY decided_at DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_decision)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Sweep pending rows whose `expires_at` is at or before `now`, flipping
/// them to `expired` and appending an `expire` decision (`decided_by =
/// "system:expiry"`) for each. Returns the ids that were swept.
///
/// Idempotent and race-safe: the SELECT + UPDATE + decision INSERT run in a
/// single `IMMEDIATE` transaction, and the `expire` decision is recorded only
/// when the guarded `UPDATE ... WHERE status = 'pending'` actually flips a row.
/// `IMMEDIATE` takes the write lock up front so a second concurrent sweep
/// blocks on `busy_timeout` until the first commits (rather than deadlocking
/// into `SQLITE_BUSY` mid-transaction); by then the rows read `expired`, its
/// SELECT returns nothing, and it does no work. Either way at most one `expire`
/// decision lands per row.
pub fn sweep_expired(db: &CentralDb, now: DateTime<Utc>) -> Result<Vec<ApprovalId>, DbError> {
    let mut conn = db.conn()?;
    let now_str = now.to_rfc3339();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut candidates = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT approval_id, action FROM pending_approvals
             WHERE status = 'pending' AND expires_at IS NOT NULL AND expires_at <= ?1",
        )?;
        let rows = stmt.query_map(params![now_str], |r| {
            let id: String = r.get(0)?;
            let action: String = r.get(1)?;
            Ok((id, action))
        })?;
        for row in rows {
            let (id_str, action) = row?;
            let parsed = uuid::Uuid::parse_str(&id_str).map_err(|e| {
                DbError::Invariant(format!(
                    "invalid uuid in pending_approvals.approval_id: {e}"
                ))
            })?;
            candidates.push((ApprovalId(parsed), action));
        }
    }
    let mut swept = Vec::new();
    for (id, action) in candidates {
        let flipped = tx.execute(
            "UPDATE pending_approvals SET status = 'expired'
             WHERE approval_id = ?1 AND status = 'pending'",
            params![id.as_uuid().to_string()],
        )?;
        // Only log the expire decision when this transaction is the one that
        // flipped the row. A concurrent sweep that won the UPDATE leaves us at
        // zero rows-affected, so we skip the INSERT and avoid a duplicate.
        if flipped == 1 {
            tx.execute(
                "INSERT INTO approval_decisions
                   (approval_id, action, outcome, decided_by, reason, decided_at)
                 VALUES (?1, ?2, 'expire', 'system:expiry', NULL, ?3)",
                params![id.as_uuid().to_string(), action, now_str],
            )?;
            swept.push(id);
        }
    }
    tx.commit()?;
    Ok(swept)
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
        assert_eq!(
            fetched.options,
            vec!["approve".to_string(), "deny".to_string()]
        );
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
        assert_eq!(
            first.approval_id, second.approval_id,
            "upsert should reuse id"
        );
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
        assert!(matches!(
            get(&db, a.approval_id).unwrap_err(),
            DbError::NotFound
        ));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let db = db();
        let err = delete(&db, ApprovalId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn upsert_after_denial_creates_fresh_pending_row() {
        // The partial unique index is scoped to `status = 'pending'`,
        // so a terminal row (denied / approved / expired) is preserved
        // as a historical receipt and the next upsert for the same
        // `(request_id, action)` produces a fresh pending row beside
        // it. Pins the contract for the
        // `016_pending_approvals_unique` migration.
        let db = db();
        let first = upsert(&db, sample("r1")).unwrap();
        update_status(&db, first.approval_id, ApprovalStatus::Denied).unwrap();
        let second = upsert(&db, sample("r1")).unwrap();
        assert_ne!(first.approval_id, second.approval_id);
        assert_eq!(second.status, ApprovalStatus::Pending);
        let denied = list(&db, None, Some(ApprovalStatus::Denied)).unwrap();
        assert_eq!(denied.len(), 1, "old denied row stays in place");
        let pending = list(&db, None, Some(ApprovalStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1, "exactly one pending row exists");
    }

    #[test]
    fn upsert_defaults_expiry_to_one_hour() {
        let db = db();
        let before = Utc::now();
        let a = upsert(&db, sample("r-ttl")).unwrap();
        let exp = a
            .expires_at
            .expect("default TTL should populate expires_at");
        let lo = before + chrono::Duration::minutes(59);
        let hi = before + chrono::Duration::minutes(61);
        assert!(exp > lo && exp < hi, "expiry should be ~1h out, got {exp}");
    }

    #[test]
    fn upsert_respects_explicit_expiry() {
        let db = db();
        let pinned = Utc::now() + chrono::Duration::minutes(5);
        let mut req = sample("r-pin");
        req.expires_at = Some(pinned);
        let a = upsert(&db, req).unwrap();
        assert_eq!(a.expires_at, Some(pinned));
    }

    #[test]
    fn is_actionable_reflects_status_and_expiry() {
        let db = db();
        let mut req = sample("r-act");
        let now = Utc::now();
        req.expires_at = Some(now + chrono::Duration::hours(1));
        let a = upsert(&db, req).unwrap();
        assert!(a.is_actionable_at(now));
        // Past its expiry but still flagged pending: NOT actionable.
        assert!(!a.is_actionable_at(now + chrono::Duration::hours(2)));
        assert!(a.is_expired_at(now + chrono::Duration::hours(2)));
    }

    #[test]
    fn sweep_expired_flips_pending_and_logs_decision() {
        let db = db();
        let mut req = sample("r-sweep");
        req.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        let a = upsert(&db, req).unwrap();
        let swept = sweep_expired(&db, Utc::now()).unwrap();
        assert_eq!(swept, vec![a.approval_id]);
        let after = get(&db, a.approval_id).unwrap();
        assert_eq!(after.status, ApprovalStatus::Expired);
        let decisions = list_decisions(&db, Some(a.approval_id), 10).unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].outcome, DecisionOutcome::Expire);
        assert_eq!(decisions[0].decided_by, "system:expiry");
    }

    #[test]
    fn sweep_expired_ignores_future_and_terminal_rows() {
        let db = db();
        // Future expiry — untouched.
        let mut future = sample("r-future");
        future.expires_at = Some(Utc::now() + chrono::Duration::hours(1));
        let f = upsert(&db, future).unwrap();
        // Already approved — untouched even if past expiry.
        let mut done = sample("r-done");
        done.action = "other".into();
        done.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        let d = upsert(&db, done).unwrap();
        update_status(&db, d.approval_id, ApprovalStatus::Approved).unwrap();
        let swept = sweep_expired(&db, Utc::now()).unwrap();
        assert!(swept.is_empty());
        assert_eq!(
            get(&db, f.approval_id).unwrap().status,
            ApprovalStatus::Pending
        );
        assert_eq!(
            get(&db, d.approval_id).unwrap().status,
            ApprovalStatus::Approved
        );
    }

    #[test]
    fn sweep_expired_is_idempotent() {
        let db = db();
        let mut req = sample("r-idem-sweep");
        req.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        upsert(&db, req).unwrap();
        let first = sweep_expired(&db, Utc::now()).unwrap();
        assert_eq!(first.len(), 1);
        let second = sweep_expired(&db, Utc::now()).unwrap();
        assert!(second.is_empty(), "second sweep must be a no-op");
        // Exactly one expire decision was logged.
        let decisions = list_decisions(&db, None, 100).unwrap();
        assert_eq!(decisions.len(), 1);
    }

    #[test]
    fn record_and_list_decisions_newest_first() {
        let db = db();
        let a = upsert(&db, sample("r-dec")).unwrap();
        record_decision(
            &db,
            a.approval_id,
            "send_message",
            DecisionOutcome::Revoke,
            "host",
            Some("operator changed their mind"),
        )
        .unwrap();
        record_decision(
            &db,
            a.approval_id,
            "send_message",
            DecisionOutcome::Approve,
            "host",
            None,
        )
        .unwrap();
        let scoped = list_decisions(&db, Some(a.approval_id), 10).unwrap();
        assert_eq!(scoped.len(), 2);
        // Newest first (the approve was inserted second).
        assert_eq!(scoped[0].outcome, DecisionOutcome::Approve);
        assert_eq!(scoped[1].outcome, DecisionOutcome::Revoke);
        assert_eq!(
            scoped[1].reason.as_deref(),
            Some("operator changed their mind")
        );
        // Global log returns the same rows.
        let global = list_decisions(&db, None, 10).unwrap();
        assert_eq!(global.len(), 2);
    }

    #[test]
    fn status_revoked_round_trips() {
        assert_eq!(
            ApprovalStatus::parse("revoked"),
            Some(ApprovalStatus::Revoked)
        );
        assert_eq!(ApprovalStatus::Revoked.as_str(), "revoked");
        assert!(ApprovalStatus::Revoked.is_terminal());
        assert!(!ApprovalStatus::Pending.is_terminal());
    }

    /// Race regression: 16 concurrent upserts against the same
    /// `(request_id, action)` against a real file-backed pool must
    /// collapse to one pending row, no panics, no UNIQUE violations.
    /// Pre-fix the SELECT-then-INSERT shape produced silent duplicate
    /// pending rows under contention.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn upsert_is_atomic_under_concurrent_writers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("copperclaw.db");
        let db = CentralDb::open(&path).unwrap();

        let mut handles = Vec::new();
        for i in 0..16 {
            let db = db.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                let mut req = sample("shared-request");
                // Vary the title so an UPDATE branch firing is observable
                // if we want to dig — the key invariant under test is
                // pending-row count, not which writer "wins".
                req.title = format!("racer-{i}");
                upsert(&db, req)
            }));
        }

        for h in handles {
            h.await
                .expect("spawn_blocking task panicked")
                .expect("upsert returned a DB error under contention");
        }

        let pending = list(&db, None, Some(ApprovalStatus::Pending)).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "exactly one pending row should exist after 16 concurrent upserts"
        );
        assert_eq!(pending[0].request_id, "shared-request");
        assert_eq!(pending[0].action, "send_message");
    }

    /// Race regression: two concurrent sweeps over the same window of
    /// genuinely-expired rows must land *exactly one* `expire` decision per
    /// row. Pre-fix `sweep_expired` was non-transactional and INSERTed the
    /// decision unconditionally, so the loser of the guarded UPDATE (zero
    /// rows-affected) still logged a duplicate `expire` receipt.
    ///
    /// We hammer the race: each trial seeds a batch of already-expired rows on
    /// a fresh file-backed pool, then fires two sweeps that rendezvous on a
    /// `Barrier` so both run their candidate SELECT before either writes. A
    /// generous row count widens the per-row UPDATE/INSERT loop so the slower
    /// sweep is still inside it when the faster one starts flipping. Several
    /// trials make the interleave near-certain. The invariant — one decision
    /// per row, every row terminal — must hold on every trial.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_sweeps_log_one_decision_per_expired_row() {
        use std::sync::Barrier;

        for trial in 0..8 {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("copperclaw.db");
            let db = CentralDb::open(&path).unwrap();

            // Seed many rows already past their TTL.
            let past = Utc::now() - chrono::Duration::minutes(1);
            let row_count: usize = 64;
            let mut ids = Vec::new();
            for i in 0..row_count {
                let mut req = sample(&format!("r-race-{trial}-{i}"));
                req.expires_at = Some(past);
                ids.push(upsert(&db, req).unwrap().approval_id);
            }

            // Two sweeps over the same window, released together by the barrier.
            let now = Utc::now();
            let barrier = std::sync::Arc::new(Barrier::new(2));
            let mut handles = Vec::new();
            for _ in 0..2 {
                let db = db.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                handles.push(tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    sweep_expired(&db, now)
                }));
            }
            let mut total_swept = 0usize;
            for h in handles {
                let swept = h
                    .await
                    .expect("spawn_blocking task panicked")
                    .expect("sweep_expired returned a DB error under contention");
                total_swept += swept.len();
            }

            // Across both sweeps, each row is reported swept exactly once.
            assert_eq!(
                total_swept, row_count,
                "trial {trial}: each expired row should be reported swept by exactly one sweep"
            );

            // Every row is terminal, with precisely one expire decision logged.
            for id in &ids {
                assert_eq!(get(&db, *id).unwrap().status, ApprovalStatus::Expired);
                let decisions = list_decisions(&db, Some(*id), 10).unwrap();
                assert_eq!(
                    decisions.len(),
                    1,
                    "trial {trial}: exactly one expire decision must exist per row \
                     despite the double sweep"
                );
                assert_eq!(decisions[0].outcome, DecisionOutcome::Expire);
            }
            // Global decision count matches the seeded row count — no duplicates.
            let all = list_decisions(&db, None, 10_000).unwrap();
            assert_eq!(
                all.len(),
                row_count,
                "trial {trial}: no duplicate expire decisions"
            );
        }
    }
}
