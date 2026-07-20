//! CRUD for `conditions` + the settable `condition_flags` latches (M22 A4) —
//! the durable registration surface that revives the sweep's dormant
//! HEARTBEAT-style condition check-ins.
//!
//! # Why this exists
//!
//! Before M22 A4 the sweep's `ConditionStore`
//! (`copperclaw_host_sweep::checks::condition_checkin`) was in-memory and
//! default-empty with no registration surface, so `IdleForAtLeastSecs` /
//! `FlagSet` conditions could never be created and never fired. This table is
//! the persistent home the sweep reloads its store from each pass, so a
//! registered condition survives a host restart (the rising-edge latch stays
//! in-memory — re-arming from "never seen" after a restart is correct).
//!
//! # Shape
//!
//! A [`StoredCondition`] is kind-tagged: `pending_inbound` / `idle` carry an
//! integer `threshold` (the `min` pending count / the idle-seconds floor);
//! `flag` carries a watched `flag` name. A `flag` condition holds while a
//! matching row exists in [`condition_flags`](set_flag) for its session — the
//! sampler reads those to populate `ConditionContext.flags_set`.
//!
//! Registration REPLACES by id (an agent-chosen stable key), mirroring
//! `ConditionStore::register`. Deregistration is a soft-delete ([`soft_remove`]
//! sets `removed_at`) so the sweep stops reloading it while an operator can
//! still audit that it existed.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, SessionId};
use rusqlite::{OptionalExtension, Row, params};

/// One row of `conditions` (active or soft-removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCondition {
    /// Agent-chosen stable key (PK).
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    /// `pending_inbound` | `idle` | `flag`.
    pub kind: String,
    /// `min` pending count (`pending_inbound`) or idle-seconds floor (`idle`);
    /// `None` for `flag`.
    pub threshold: Option<i64>,
    /// Watched flag name for `flag`; `None` otherwise.
    pub flag: Option<String>,
    /// Prompt delivered to the woken agent on a fire.
    pub prompt: String,
    /// Optional A2 fire-time grant linkage; `None` = no linked grant.
    pub grant_id: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Soft-delete instant; `None` = active.
    pub removed_at: Option<DateTime<Utc>>,
}

/// Insert / replace spec for a condition.
#[derive(Debug, Clone)]
pub struct NewCondition {
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub kind: String,
    pub threshold: Option<i64>,
    pub flag: Option<String>,
    pub prompt: String,
    pub grant_id: Option<String>,
}

fn parse_ts(row: &Row<'_>, col: &str) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let stored: Option<String> = row.get(col)?;
    match stored.as_deref() {
        None | Some("") => Ok(None),
        Some(ts) => Ok(Some(
            DateTime::parse_from_rfc3339(ts)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
        )),
    }
}

fn require_ts(row: &Row<'_>, col: &str) -> rusqlite::Result<DateTime<Utc>> {
    parse_ts(row, col)?.ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("{col} must not be null").into(),
        )
    })
}

fn row_to_condition(row: &Row<'_>) -> rusqlite::Result<StoredCondition> {
    let ag_str: String = row.get("agent_group_id")?;
    let ag_uuid = uuid::Uuid::parse_str(&ag_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let sess_str: String = row.get("session_id")?;
    let sess_uuid = uuid::Uuid::parse_str(&sess_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(StoredCondition {
        id: row.get("id")?,
        agent_group_id: AgentGroupId(ag_uuid),
        session_id: SessionId::from(sess_uuid),
        kind: row.get("kind")?,
        threshold: row.get("threshold")?,
        flag: row.get("flag")?,
        prompt: row.get("prompt")?,
        grant_id: row.get("grant_id")?,
        created_at: require_ts(row, "created_at")?,
        removed_at: parse_ts(row, "removed_at")?,
    })
}

const SELECT_COLS: &str = "id, agent_group_id, session_id, kind, threshold, flag, prompt, grant_id, created_at, \
     removed_at";

/// Register (or REPLACE by id) a condition, clearing any prior soft-delete so a
/// re-registered id is active again. Returns the (now-active) stored row.
pub fn upsert(db: &CentralDb, spec: NewCondition) -> Result<StoredCondition, DbError> {
    if spec.id.trim().is_empty() {
        return Err(DbError::invariant("condition id must be non-empty"));
    }
    if spec.prompt.trim().is_empty() {
        return Err(DbError::invariant("condition prompt must be non-empty"));
    }
    let now = Utc::now();
    db.conn()?.execute(
        "INSERT INTO conditions
           (id, agent_group_id, session_id, kind, threshold, flag, prompt, grant_id,
            created_at, removed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)
         ON CONFLICT(id) DO UPDATE SET
           agent_group_id = excluded.agent_group_id,
           session_id     = excluded.session_id,
           kind           = excluded.kind,
           threshold      = excluded.threshold,
           flag           = excluded.flag,
           prompt         = excluded.prompt,
           grant_id       = excluded.grant_id,
           removed_at     = NULL",
        params![
            spec.id,
            spec.agent_group_id.as_uuid().to_string(),
            spec.session_id.as_uuid().to_string(),
            spec.kind,
            spec.threshold,
            spec.flag,
            spec.prompt,
            spec.grant_id,
            now.to_rfc3339(),
        ],
    )?;
    // Build the active row from the spec rather than re-reading. `created_at`
    // reflects this call; on a REPLACE the DB retains the original `created_at`,
    // which is a cosmetic difference callers do not depend on.
    Ok(StoredCondition {
        id: spec.id,
        agent_group_id: spec.agent_group_id,
        session_id: spec.session_id,
        kind: spec.kind,
        threshold: spec.threshold,
        flag: spec.flag,
        prompt: spec.prompt,
        grant_id: spec.grant_id,
        created_at: now,
        removed_at: None,
    })
}

/// All currently-active (not soft-removed) conditions, across every session —
/// the set the sweep reloads into its `ConditionStore` each pass.
pub fn list_active(db: &CentralDb) -> Result<Vec<StoredCondition>, DbError> {
    let conn = db.conn()?;
    let sql = format!("SELECT {SELECT_COLS} FROM conditions WHERE removed_at IS NULL ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_condition)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Fetch one condition by id (active or removed).
pub fn get(db: &CentralDb, id: &str) -> Result<Option<StoredCondition>, DbError> {
    let conn = db.conn()?;
    let sql = format!("SELECT {SELECT_COLS} FROM conditions WHERE id = ?1");
    let out = conn
        .query_row(&sql, params![id], row_to_condition)
        .optional()?;
    Ok(out)
}

/// Soft-delete a condition (sets `removed_at`). Returns whether an ACTIVE row
/// was removed (already-removed / unknown ids return `false`).
pub fn soft_remove(db: &CentralDb, id: &str, now: DateTime<Utc>) -> Result<bool, DbError> {
    let conn = db.conn()?;
    let affected = conn.execute(
        "UPDATE conditions SET removed_at = ?1 WHERE id = ?2 AND removed_at IS NULL",
        params![now.to_rfc3339(), id],
    )?;
    Ok(affected > 0)
}

/// Set a per-session flag latch (idempotent — re-setting refreshes `set_at`).
/// The `flag` condition kind holds while this row exists.
pub fn set_flag(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    session_id: SessionId,
    flag: &str,
    now: DateTime<Utc>,
) -> Result<(), DbError> {
    if flag.trim().is_empty() {
        return Err(DbError::invariant("condition flag name must be non-empty"));
    }
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO condition_flags (agent_group_id, session_id, flag, set_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id, flag) DO UPDATE SET set_at = excluded.set_at",
        params![
            agent_group_id.as_uuid().to_string(),
            session_id.as_uuid().to_string(),
            flag,
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Clear a per-session flag latch. Returns whether a set flag was cleared.
pub fn clear_flag(db: &CentralDb, session_id: SessionId, flag: &str) -> Result<bool, DbError> {
    let conn = db.conn()?;
    let affected = conn.execute(
        "DELETE FROM condition_flags WHERE session_id = ?1 AND flag = ?2",
        params![session_id.as_uuid().to_string(), flag],
    )?;
    Ok(affected > 0)
}

/// The currently-set flag names for a session — read by the sweep sampler to
/// populate `ConditionContext.flags_set`.
pub fn list_flags_for_session(
    db: &CentralDb,
    session_id: SessionId,
) -> Result<Vec<String>, DbError> {
    let conn = db.conn()?;
    let mut stmt =
        conn.prepare("SELECT flag FROM condition_flags WHERE session_id = ?1 ORDER BY flag")?;
    let rows = stmt.query_map(params![session_id.as_uuid().to_string()], |r| {
        r.get::<_, String>(0)
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{CreateAgentGroup, create as create_ag};

    fn seed_group(db: &CentralDb) -> AgentGroupId {
        create_ag(
            db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    fn new_cond(ag: AgentGroupId, sess: SessionId, id: &str, kind: &str) -> NewCondition {
        NewCondition {
            id: id.into(),
            agent_group_id: ag,
            session_id: sess,
            kind: kind.into(),
            threshold: Some(300),
            flag: None,
            prompt: "check in".into(),
            grant_id: None,
        }
    }

    #[test]
    fn upsert_then_list_active_roundtrips() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        let stored = upsert(&db, new_cond(ag, sess, "c1", "idle")).unwrap();
        assert_eq!(stored.id, "c1");
        assert_eq!(stored.kind, "idle");
        assert_eq!(stored.threshold, Some(300));
        assert!(stored.removed_at.is_none());
        let active = list_active(&db).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "c1");
    }

    #[test]
    fn upsert_replaces_by_id() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        upsert(&db, new_cond(ag, sess, "c1", "idle")).unwrap();
        let mut replaced = new_cond(ag, sess, "c1", "pending_inbound");
        replaced.threshold = Some(5);
        upsert(&db, replaced).unwrap();
        let active = list_active(&db).unwrap();
        assert_eq!(active.len(), 1, "replace, not append");
        assert_eq!(active[0].kind, "pending_inbound");
        assert_eq!(active[0].threshold, Some(5));
    }

    #[test]
    fn soft_remove_hides_from_active_but_keeps_row() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        upsert(&db, new_cond(ag, sess, "c1", "idle")).unwrap();
        assert!(soft_remove(&db, "c1", Utc::now()).unwrap());
        assert!(list_active(&db).unwrap().is_empty());
        // A second remove is a no-op (already removed).
        assert!(!soft_remove(&db, "c1", Utc::now()).unwrap());
        // The row still exists (audit trail) and carries removed_at.
        let row = get(&db, "c1").unwrap().unwrap();
        assert!(row.removed_at.is_some());
    }

    #[test]
    fn re_upsert_after_remove_reactivates() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        upsert(&db, new_cond(ag, sess, "c1", "idle")).unwrap();
        soft_remove(&db, "c1", Utc::now()).unwrap();
        upsert(&db, new_cond(ag, sess, "c1", "idle")).unwrap();
        let active = list_active(&db).unwrap();
        assert_eq!(active.len(), 1, "re-registration clears the soft-delete");
    }

    #[test]
    fn flags_set_list_and_clear() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        assert!(list_flags_for_session(&db, sess).unwrap().is_empty());
        set_flag(&db, ag, sess, "deploying", Utc::now()).unwrap();
        set_flag(&db, ag, sess, "alarm", Utc::now()).unwrap();
        // Idempotent re-set.
        set_flag(&db, ag, sess, "alarm", Utc::now()).unwrap();
        let flags = list_flags_for_session(&db, sess).unwrap();
        assert_eq!(flags, vec!["alarm".to_string(), "deploying".to_string()]);
        assert!(clear_flag(&db, sess, "alarm").unwrap());
        assert!(!clear_flag(&db, sess, "alarm").unwrap());
        assert_eq!(
            list_flags_for_session(&db, sess).unwrap(),
            vec!["deploying".to_string()]
        );
    }

    #[test]
    fn flags_are_per_session() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = seed_group(&db);
        let a = SessionId::new();
        let b = SessionId::new();
        set_flag(&db, ag, a, "go", Utc::now()).unwrap();
        assert_eq!(
            list_flags_for_session(&db, a).unwrap(),
            vec!["go".to_string()]
        );
        assert!(list_flags_for_session(&db, b).unwrap().is_empty());
    }

    #[test]
    fn upsert_rejects_blank_id_and_prompt() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = seed_group(&db);
        let sess = SessionId::new();
        let mut blank_id = new_cond(ag, sess, "  ", "idle");
        blank_id.id = "  ".into();
        assert!(upsert(&db, blank_id).is_err());
        let mut blank_prompt = new_cond(ag, sess, "c1", "idle");
        blank_prompt.prompt = " ".into();
        assert!(upsert(&db, blank_prompt).is_err());
    }
}
