//! CRUD + state transitions + budget accrual for `goals` (and the append-only
//! `goal_progress` child log) — the first-class long-running objective the
//! sweep drives against (M22 A3).
//!
//! # Where this fits (decision (d): index, don't duplicate)
//!
//! A goal is durable status/progress/budget the sweep can drive against. It
//! INDEXES OVER the existing stores — `agent_todos.json` stays the in-session
//! plan and the per-group memory store stays the fact store — it does not
//! replace them. Concretely a goal records:
//!
//!   * the durable `objective`,
//!   * an explicit `status` lifecycle (`active`/`paused`/`completed`/`abandoned`)
//!     with modelled, validated transitions ([`transition`]),
//!   * an append-only progress log ([`record_progress`] / [`list_progress`]),
//!   * cumulative token spend for reporting (`tokens_consumed`), and
//!   * a link to the driving `task_id` and/or the A1 `grant_id`.
//!
//! # How the sweep drives it
//!
//! `copperclaw_host_sweep::checks::goals` scans [`list_due_checkin`] each pass
//! and, for every active goal whose `next_checkin` has elapsed, synthesises a
//! `kind:task` inbound into the goal's session (the SAME fan-out scheduled
//! tasks use). It then [`mark_checkin`]s the fire and re-arms `next_checkin`
//! from `checkin_recurrence`. The woken agent reports progress via the
//! `update_goal` MCP tool, which lands here as [`record_progress`].
//!
//! # Budget (decision (d))
//!
//! Where a goal has a `grant_id`, the live A1 grant
//! ([`crate::tables::task_grants::effective_grant`]) is the budget AUTHORITY:
//! [`budget_remaining`] consults its
//! [`tokens_remaining`](crate::tables::task_grants::EffectiveGrant::tokens_remaining)
//! rather than a separate authority, so an exhausted / inert grant reads as
//! zero remaining and the sweep can pause the goal. With no grant linked, the
//! goal's own `token_budget` is the cap. `tokens_consumed` accrues cumulatively
//! for reporting either way.

use crate::DbError;
use crate::central::CentralDb;
use crate::tables::task_grants;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, SessionId};
use rusqlite::{OptionalExtension, Row, params};

/// Lifecycle states of a goal. Stored as TEXT.
///
/// `Active` is the only state the sweep fires check-ins for. `Completed` and
/// `Abandoned` are terminal — no transition leaves them (see [`transition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    /// The goal is being actively pursued; the sweep fires its check-ins.
    Active,
    /// Temporarily halted (manually, or by an exhausted budget). No check-ins
    /// fire, but the goal can resume to `Active`.
    Paused,
    /// The objective was achieved. Terminal.
    Completed,
    /// The objective was given up. Terminal.
    Abandoned,
}

impl GoalStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
        }
    }

    /// True for the terminal states (`Completed` / `Abandoned`) — no further
    /// transition is permitted out of these.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Abandoned)
    }
}

impl std::str::FromStr for GoalStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            "abandoned" => Ok(Self::Abandoned),
            other => Err(format!("unknown goal status `{other}`")),
        }
    }
}

/// Decide whether a goal may move from `from` to `to`. The lifecycle is
/// deliberately small and one-way into the terminal states:
///
///   * `Active` ⇄ `Paused` (pause / resume),
///   * `Active` → `Completed` | `Abandoned`,
///   * `Paused` → `Completed` | `Abandoned`,
///   * a no-op self-transition (`from == to`) is allowed for idempotency,
///   * nothing leaves a terminal state, and nothing "resurrects" it.
///
/// Public so the MCP `update_goal` handler and A3's tests share one authority.
#[must_use]
pub fn transition(from: GoalStatus, to: GoalStatus) -> bool {
    use GoalStatus::{Abandoned, Active, Completed, Paused};
    if from == to {
        // Idempotent re-assert of the same state (except terminal, which can't
        // even re-assert — it's frozen).
        return !from.is_terminal();
    }
    matches!(
        (from, to),
        (Active, Paused) | (Paused, Active) | (Active | Paused, Completed | Abandoned)
    )
}

/// One row of `goals`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub objective: String,
    pub status: GoalStatus,
    /// Optional FK to the driving check-in task (migration 028 lineage).
    pub task_id: Option<String>,
    /// Optional FK to the A1 grant that is this goal's budget authority.
    pub grant_id: Option<String>,
    /// The goal's own cumulative token cap; consulted only when `grant_id` is
    /// `None`. `None` = no own cap.
    pub token_budget: Option<i64>,
    /// Running cumulative token spend recorded for reporting.
    pub tokens_consumed: i64,
    /// Optional cron the sweep re-arms `next_checkin` from after each fire.
    pub checkin_recurrence: Option<String>,
    /// Optional prompt injected on a check-in wake; `None` = synthesised.
    pub checkin_prompt: Option<String>,
    /// Absolute instant of the next due check-in; `None` = none pending.
    pub next_checkin: Option<DateTime<Utc>>,
    /// Instant of the most recent check-in fire; `None` until the first.
    pub last_checkin_at: Option<DateTime<Utc>>,
    /// Monotonically increasing count of check-in fires.
    pub checkin_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insert spec for a new goal. `status` is always `Active` at insert.
#[derive(Debug, Clone)]
pub struct NewGoal {
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub objective: String,
    pub task_id: Option<String>,
    pub grant_id: Option<String>,
    pub token_budget: Option<i64>,
    pub checkin_recurrence: Option<String>,
    pub checkin_prompt: Option<String>,
    pub next_checkin: Option<DateTime<Utc>>,
}

/// One appended row of `goal_progress`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalProgress {
    pub id: String,
    pub goal_id: String,
    pub note: String,
    /// Tokens attributed to this progress entry (accrued onto the goal), if any.
    pub tokens: Option<i64>,
    pub created_at: DateTime<Utc>,
}

fn parse_ts(row: &Row<'_>, col: &str) -> rusqlite::Result<Option<DateTime<Utc>>> {
    // Empty-string-as-Some defence, mirroring `tasks::row_to_task`.
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

fn row_to_goal(row: &Row<'_>) -> rusqlite::Result<Goal> {
    let ag_str: String = row.get("agent_group_id")?;
    let ag_uuid = uuid::Uuid::parse_str(&ag_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let sess_str: String = row.get("session_id")?;
    let sess_uuid = uuid::Uuid::parse_str(&sess_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status_str: String = row.get("status")?;
    let status: GoalStatus = status_str.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })?;
    Ok(Goal {
        id: row.get("id")?,
        agent_group_id: AgentGroupId(ag_uuid),
        session_id: SessionId::from(sess_uuid),
        objective: row.get("objective")?,
        status,
        task_id: row.get("task_id")?,
        grant_id: row.get("grant_id")?,
        token_budget: row.get("token_budget")?,
        tokens_consumed: row.get("tokens_consumed")?,
        checkin_recurrence: row.get("checkin_recurrence")?,
        checkin_prompt: row.get("checkin_prompt")?,
        next_checkin: parse_ts(row, "next_checkin")?,
        last_checkin_at: parse_ts(row, "last_checkin_at")?,
        checkin_count: row.get("checkin_count")?,
        created_at: require_ts(row, "created_at")?,
        updated_at: require_ts(row, "updated_at")?,
    })
}

const SELECT_COLS: &str = "id, agent_group_id, session_id, objective, status, task_id, grant_id, \
     token_budget, tokens_consumed, checkin_recurrence, checkin_prompt, next_checkin, \
     last_checkin_at, checkin_count, created_at, updated_at";

/// Insert a new goal (always `status = 'active'`). Returns the inserted row.
pub fn insert(db: &CentralDb, goal: NewGoal) -> Result<Goal, DbError> {
    if goal.objective.trim().is_empty() {
        return Err(DbError::invariant("goal objective must be non-empty"));
    }
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO goals
           (id, agent_group_id, session_id, objective, status, task_id, grant_id,
            token_budget, tokens_consumed, checkin_recurrence, checkin_prompt,
            next_checkin, last_checkin_at, checkin_count, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, 0, ?8, ?9, ?10, NULL, 0, ?11, ?12)",
        params![
            goal.id,
            goal.agent_group_id.as_uuid().to_string(),
            goal.session_id.as_uuid().to_string(),
            goal.objective,
            goal.task_id,
            goal.grant_id,
            goal.token_budget,
            goal.checkin_recurrence,
            goal.checkin_prompt,
            goal.next_checkin.map(|t| t.to_rfc3339()),
            now.to_rfc3339(),
            now.to_rfc3339(),
        ],
    )?;
    Ok(Goal {
        id: goal.id,
        agent_group_id: goal.agent_group_id,
        session_id: goal.session_id,
        objective: goal.objective,
        status: GoalStatus::Active,
        task_id: goal.task_id,
        grant_id: goal.grant_id,
        token_budget: goal.token_budget,
        tokens_consumed: 0,
        checkin_recurrence: goal.checkin_recurrence,
        checkin_prompt: goal.checkin_prompt,
        next_checkin: goal.next_checkin,
        last_checkin_at: None,
        checkin_count: 0,
        created_at: now,
        updated_at: now,
    })
}

/// Fetch one goal by id.
pub fn get(db: &CentralDb, id: &str) -> Result<Option<Goal>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            &format!("SELECT {SELECT_COLS} FROM goals WHERE id = ?1"),
            params![id],
            row_to_goal,
        )
        .optional()?)
}

/// All goals for one session (any status), newest first.
pub fn list_for_session(db: &CentralDb, session_id: SessionId) -> Result<Vec<Goal>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM goals WHERE session_id = ?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map(params![session_id.as_uuid().to_string()], row_to_goal)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// All goals for one agent group (any status), newest first.
pub fn list_for_group(db: &CentralDb, agent_group_id: AgentGroupId) -> Result<Vec<Goal>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM goals WHERE agent_group_id = ?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map(params![agent_group_id.as_uuid().to_string()], row_to_goal)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Active goals whose `next_checkin` has elapsed at `now`. The sweep's
/// due-check-in scan. `now` is passed in (not read from the clock) for
/// deterministic tests, mirroring `tasks::list_due`.
pub fn list_due_checkin(db: &CentralDb, now: DateTime<Utc>) -> Result<Vec<Goal>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM goals
          WHERE status = 'active'
            AND next_checkin IS NOT NULL
            AND next_checkin <= ?1
          ORDER BY next_checkin"
    ))?;
    let rows = stmt.query_map(params![now.to_rfc3339()], row_to_goal)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Count goals in `active` status. Feeds the A3 goal-progress metric wish the
/// sweep can publish each pass.
pub fn count_active(db: &CentralDb) -> Result<u64, DbError> {
    let conn = db.conn()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM goals WHERE status = 'active'",
        [],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Move a goal to `status`, enforcing the [`transition`] lifecycle. Returns
/// `DbError::Invariant` on an illegal transition (e.g. out of a terminal state)
/// and `DbError::NotFound` on an unknown id.
pub fn set_status(db: &CentralDb, id: &str, status: GoalStatus) -> Result<Goal, DbError> {
    let current = get(db, id)?.ok_or(DbError::NotFound)?;
    if !transition(current.status, status) {
        return Err(DbError::invariant(format!(
            "illegal goal transition {} → {}",
            current.status.as_str(),
            status.as_str()
        )));
    }
    // Entering a terminal state clears any pending check-in so the sweep never
    // fires a completed/abandoned goal.
    let clear_checkin = status.is_terminal();
    {
        let conn = db.conn()?;
        let now = Utc::now().to_rfc3339();
        if clear_checkin {
            conn.execute(
                "UPDATE goals SET status = ?1, next_checkin = NULL, updated_at = ?2 WHERE id = ?3",
                params![status.as_str(), now, id],
            )?;
        } else {
            conn.execute(
                "UPDATE goals SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status.as_str(), now, id],
            )?;
        }
    }
    get(db, id)?.ok_or(DbError::NotFound)
}

/// Update the mutable free-text fields of a goal in place. Any `Some(_)`
/// overwrites; `None` leaves the column untouched. Does NOT touch `status`
/// (use [`set_status`]) or the check-in bookkeeping (use [`mark_checkin`] /
/// [`set_next_checkin`]).
#[derive(Debug, Clone, Default)]
pub struct UpdateFields {
    pub objective: Option<String>,
    pub checkin_prompt: Option<Option<String>>,
    pub checkin_recurrence: Option<Option<String>>,
}

pub fn update(db: &CentralDb, id: &str, fields: UpdateFields) -> Result<Goal, DbError> {
    {
        let conn = db.conn()?;
        let now = Utc::now().to_rfc3339();
        if let Some(obj) = fields.objective {
            if obj.trim().is_empty() {
                return Err(DbError::invariant("goal objective must be non-empty"));
            }
            conn.execute(
                "UPDATE goals SET objective = ?1, updated_at = ?2 WHERE id = ?3",
                params![obj, now, id],
            )?;
        }
        if let Some(prompt) = fields.checkin_prompt {
            conn.execute(
                "UPDATE goals SET checkin_prompt = ?1, updated_at = ?2 WHERE id = ?3",
                params![prompt, now, id],
            )?;
        }
        if let Some(rec) = fields.checkin_recurrence {
            conn.execute(
                "UPDATE goals SET checkin_recurrence = ?1, updated_at = ?2 WHERE id = ?3",
                params![rec, now, id],
            )?;
        }
    }
    get(db, id)?.ok_or(DbError::NotFound)
}

/// Record that a goal fired a check-in: stamp `last_checkin_at` and bump
/// `checkin_count`. Called by the sweep's goal fan-out on every fire.
pub fn mark_checkin(db: &CentralDb, id: &str, at: DateTime<Utc>) -> Result<(), DbError> {
    let conn = db.conn()?;
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE goals
            SET last_checkin_at = ?1, checkin_count = checkin_count + 1, updated_at = ?2
          WHERE id = ?3",
        params![at.to_rfc3339(), now, id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Replace `next_checkin` (used to re-arm a recurring check-in, or clear it).
pub fn set_next_checkin(
    db: &CentralDb,
    id: &str,
    next_checkin: Option<DateTime<Utc>>,
) -> Result<(), DbError> {
    let conn = db.conn()?;
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE goals SET next_checkin = ?1, updated_at = ?2 WHERE id = ?3",
        params![next_checkin.map(|t| t.to_rfc3339()), now, id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Append a progress entry and accrue its `tokens` (when given) onto the goal's
/// cumulative `tokens_consumed`. This is the `update_goal(progress)` landing
/// point and the append-only progress log's only writer. `tokens` must be >= 0.
pub fn record_progress(
    db: &CentralDb,
    entry_id: &str,
    goal_id: &str,
    note: &str,
    tokens: Option<i64>,
) -> Result<GoalProgress, DbError> {
    if note.trim().is_empty() {
        return Err(DbError::invariant("goal progress note must be non-empty"));
    }
    if let Some(t) = tokens {
        if t < 0 {
            return Err(DbError::invariant(
                "record_progress: `tokens` must be non-negative",
            ));
        }
    }
    // The goal must exist (FK would catch it, but return a clean NotFound).
    if get(db, goal_id)?.is_none() {
        return Err(DbError::NotFound);
    }
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO goal_progress (id, goal_id, note, tokens, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![entry_id, goal_id, note, tokens, now.to_rfc3339()],
    )?;
    if let Some(t) = tokens {
        conn.execute(
            "UPDATE goals SET tokens_consumed = tokens_consumed + ?1, updated_at = ?2 WHERE id = ?3",
            params![t, now.to_rfc3339(), goal_id],
        )?;
    }
    Ok(GoalProgress {
        id: entry_id.to_string(),
        goal_id: goal_id.to_string(),
        note: note.to_string(),
        tokens,
        created_at: now,
    })
}

/// The full progress log for a goal, oldest first.
pub fn list_progress(db: &CentralDb, goal_id: &str) -> Result<Vec<GoalProgress>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, goal_id, note, tokens, created_at
         FROM goal_progress WHERE goal_id = ?1 ORDER BY created_at, rowid",
    )?;
    let rows = stmt.query_map(params![goal_id], |row| {
        Ok(GoalProgress {
            id: row.get("id")?,
            goal_id: row.get("goal_id")?,
            note: row.get("note")?,
            tokens: row.get("tokens")?,
            created_at: require_ts(row, "created_at")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Tokens the goal may still spend at `now`, or `None` when unbounded.
///
/// Decision (d): where the goal links a `grant_id`, the live A1 grant is the
/// authority — an approved/live grant returns its
/// [`tokens_remaining`](task_grants::EffectiveGrant::tokens_remaining), and an
/// inert (revoked/expired/exhausted/missing) grant returns `Some(0)` (no
/// budget). With no grant linked, the goal's own `token_budget` less its
/// accrued `tokens_consumed` is the cap.
pub fn budget_remaining(
    db: &CentralDb,
    goal_id: &str,
    now: DateTime<Utc>,
) -> Result<Option<i64>, DbError> {
    let goal = get(db, goal_id)?.ok_or(DbError::NotFound)?;
    if let Some(grant_id) = goal.grant_id.as_deref() {
        // The grant is keyed on its OWNING task in `effective_grant`, but a
        // goal references the concrete grant id; resolve liveness against that
        // grant's task so we reuse the single A1 liveness contract.
        let Some(grant) = task_grants::get(db, grant_id)? else {
            return Ok(Some(0));
        };
        return Ok(Some(
            task_grants::effective_grant(db, &grant.task_id, now)?
                // Only the newest grant is authoritative in `effective_grant`;
                // a live result that is a DIFFERENT grant than the goal's means
                // the goal's grant was superseded → treat as no budget.
                .filter(|eff| eff.id == grant_id)
                .and_then(|eff| eff.tokens_remaining())
                .unwrap_or(0),
        ));
    }
    Ok(goal.token_budget.map(|b| (b - goal.tokens_consumed).max(0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use crate::tables::task_grants::{self, NewTaskGrant};
    use crate::tables::tasks::{self, NewTask};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn mk_ag(db: &CentralDb) -> AgentGroupId {
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

    fn mk_goal(id: &str, ag: AgentGroupId, sess: SessionId) -> NewGoal {
        NewGoal {
            id: id.into(),
            agent_group_id: ag,
            session_id: sess,
            objective: "ship the migration".into(),
            task_id: None,
            grant_id: None,
            token_budget: None,
            checkin_recurrence: Some("0 9 * * *".into()),
            checkin_prompt: None,
            next_checkin: Some(Utc::now() + chrono::Duration::hours(1)),
        }
    }

    // -- schema / round-trip -------------------------------------------------

    #[test]
    fn insert_then_get_roundtrips() {
        let db = db();
        let ag = mk_ag(&db);
        let sess = SessionId::new();
        let g = insert(&db, mk_goal("g-1", ag, sess)).unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.checkin_count, 0);
        assert!(g.last_checkin_at.is_none());
        let back = get(&db, "g-1").unwrap().unwrap();
        assert_eq!(back, g);
        assert_eq!(back.objective, "ship the migration");
    }

    #[test]
    fn insert_rejects_blank_objective() {
        let db = db();
        let ag = mk_ag(&db);
        let mut spec = mk_goal("g-1", ag, SessionId::new());
        spec.objective = "   ".into();
        assert!(matches!(
            insert(&db, spec).unwrap_err(),
            DbError::Invariant(_)
        ));
    }

    #[test]
    fn get_missing_is_none() {
        let db = db();
        assert!(get(&db, "ghost").unwrap().is_none());
    }

    #[test]
    fn goal_status_parses_and_emits() {
        for s in ["active", "paused", "completed", "abandoned"] {
            let parsed: GoalStatus = s.parse().unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!("bogus".parse::<GoalStatus>().is_err());
    }

    #[test]
    fn list_for_session_and_group_filter() {
        let db = db();
        let ag = mk_ag(&db);
        let sess_a = SessionId::new();
        let sess_b = SessionId::new();
        insert(&db, mk_goal("a1", ag, sess_a)).unwrap();
        insert(&db, mk_goal("a2", ag, sess_a)).unwrap();
        insert(&db, mk_goal("b1", ag, sess_b)).unwrap();
        assert_eq!(list_for_session(&db, sess_a).unwrap().len(), 2);
        assert_eq!(list_for_group(&db, ag).unwrap().len(), 3);
    }

    // -- state transitions ---------------------------------------------------

    #[test]
    fn transition_matrix() {
        use GoalStatus::{Abandoned, Active, Completed, Paused};
        // Legal.
        assert!(transition(Active, Paused));
        assert!(transition(Paused, Active));
        assert!(transition(Active, Completed));
        assert!(transition(Active, Abandoned));
        assert!(transition(Paused, Completed));
        assert!(transition(Paused, Abandoned));
        // Idempotent self-transition on non-terminal.
        assert!(transition(Active, Active));
        assert!(transition(Paused, Paused));
        // Terminal is frozen — not even a self-transition.
        assert!(!transition(Completed, Completed));
        assert!(!transition(Abandoned, Abandoned));
        assert!(!transition(Completed, Active));
        assert!(!transition(Abandoned, Paused));
        assert!(!transition(Completed, Abandoned));
    }

    #[test]
    fn set_status_enforces_transitions_and_clears_checkin_on_terminal() {
        let db = db();
        let ag = mk_ag(&db);
        let g = insert(&db, mk_goal("g-1", ag, SessionId::new())).unwrap();
        assert!(g.next_checkin.is_some());
        // Active → Paused → Active.
        assert_eq!(
            set_status(&db, "g-1", GoalStatus::Paused).unwrap().status,
            GoalStatus::Paused
        );
        assert_eq!(
            set_status(&db, "g-1", GoalStatus::Active).unwrap().status,
            GoalStatus::Active
        );
        // Active → Completed clears the pending check-in.
        let done = set_status(&db, "g-1", GoalStatus::Completed).unwrap();
        assert_eq!(done.status, GoalStatus::Completed);
        assert!(done.next_checkin.is_none(), "terminal clears next_checkin");
        // Out of terminal is illegal.
        assert!(matches!(
            set_status(&db, "g-1", GoalStatus::Active).unwrap_err(),
            DbError::Invariant(_)
        ));
    }

    #[test]
    fn set_status_unknown_id_is_not_found() {
        let db = db();
        assert!(matches!(
            set_status(&db, "ghost", GoalStatus::Paused).unwrap_err(),
            DbError::NotFound
        ));
    }

    #[test]
    fn update_patches_selected_fields() {
        let db = db();
        let ag = mk_ag(&db);
        insert(&db, mk_goal("g-1", ag, SessionId::new())).unwrap();
        let updated = update(
            &db,
            "g-1",
            UpdateFields {
                objective: Some("new objective".into()),
                checkin_prompt: Some(Some("report progress".into())),
                checkin_recurrence: Some(None),
            },
        )
        .unwrap();
        assert_eq!(updated.objective, "new objective");
        assert_eq!(updated.checkin_prompt.as_deref(), Some("report progress"));
        assert!(updated.checkin_recurrence.is_none());
    }

    #[test]
    fn update_rejects_blank_objective() {
        let db = db();
        let ag = mk_ag(&db);
        insert(&db, mk_goal("g-1", ag, SessionId::new())).unwrap();
        assert!(matches!(
            update(
                &db,
                "g-1",
                UpdateFields {
                    objective: Some("  ".into()),
                    ..Default::default()
                }
            )
            .unwrap_err(),
            DbError::Invariant(_)
        ));
    }

    // -- check-in bookkeeping ------------------------------------------------

    #[test]
    fn mark_checkin_and_next_checkin() {
        let db = db();
        let ag = mk_ag(&db);
        insert(&db, mk_goal("g-1", ag, SessionId::new())).unwrap();
        let t = Utc::now();
        mark_checkin(&db, "g-1", t).unwrap();
        let g = get(&db, "g-1").unwrap().unwrap();
        assert_eq!(g.checkin_count, 1);
        assert_eq!(g.last_checkin_at.unwrap().timestamp(), t.timestamp());
        // Re-arm.
        let next = t + chrono::Duration::days(1);
        set_next_checkin(&db, "g-1", Some(next)).unwrap();
        assert_eq!(
            get(&db, "g-1")
                .unwrap()
                .unwrap()
                .next_checkin
                .unwrap()
                .timestamp(),
            next.timestamp()
        );
        // Clear.
        set_next_checkin(&db, "g-1", None).unwrap();
        assert!(get(&db, "g-1").unwrap().unwrap().next_checkin.is_none());
    }

    #[test]
    fn list_due_checkin_filters_status_and_time() {
        let db = db();
        let ag = mk_ag(&db);
        let sess = SessionId::new();
        let now = Utc::now();
        let mut due = mk_goal("due", ag, sess);
        due.next_checkin = Some(now - chrono::Duration::minutes(1));
        let mut future = mk_goal("future", ag, sess);
        future.next_checkin = Some(now + chrono::Duration::hours(1));
        insert(&db, due).unwrap();
        insert(&db, future).unwrap();
        // A paused goal is never due even if its instant elapsed.
        let mut paused = mk_goal("paused", ag, sess);
        paused.next_checkin = Some(now - chrono::Duration::minutes(1));
        insert(&db, paused).unwrap();
        set_status(&db, "paused", GoalStatus::Paused).unwrap();

        let out = list_due_checkin(&db, now).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "due");
    }

    // -- progress log + budget accrual (acceptance) --------------------------

    #[test]
    fn record_progress_appends_and_accrues() {
        let db = db();
        let ag = mk_ag(&db);
        insert(&db, mk_goal("g-1", ag, SessionId::new())).unwrap();
        record_progress(&db, "p1", "g-1", "did the schema", Some(120)).unwrap();
        record_progress(&db, "p2", "g-1", "wrote the model", Some(80)).unwrap();
        // A note-only entry does not move the accrual.
        record_progress(&db, "p3", "g-1", "no tokens here", None).unwrap();
        let g = get(&db, "g-1").unwrap().unwrap();
        assert_eq!(
            g.tokens_consumed, 200,
            "accrues only the token-bearing entries"
        );
        let log = list_progress(&db, "g-1").unwrap();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].id, "p1");
        assert_eq!(log[2].note, "no tokens here");
    }

    #[test]
    fn record_progress_rejects_blank_note_and_negative_tokens() {
        let db = db();
        let ag = mk_ag(&db);
        insert(&db, mk_goal("g-1", ag, SessionId::new())).unwrap();
        assert!(matches!(
            record_progress(&db, "p", "g-1", "  ", None).unwrap_err(),
            DbError::Invariant(_)
        ));
        assert!(matches!(
            record_progress(&db, "p", "g-1", "note", Some(-1)).unwrap_err(),
            DbError::Invariant(_)
        ));
        // Unknown goal.
        assert!(matches!(
            record_progress(&db, "p", "ghost", "note", None).unwrap_err(),
            DbError::NotFound
        ));
    }

    #[test]
    fn budget_remaining_uses_own_cap_without_grant() {
        let db = db();
        let ag = mk_ag(&db);
        let mut spec = mk_goal("g-1", ag, SessionId::new());
        spec.token_budget = Some(1000);
        insert(&db, spec).unwrap();
        assert_eq!(
            budget_remaining(&db, "g-1", Utc::now()).unwrap(),
            Some(1000)
        );
        record_progress(&db, "p1", "g-1", "spent some", Some(400)).unwrap();
        assert_eq!(budget_remaining(&db, "g-1", Utc::now()).unwrap(), Some(600));
        // Over-spend clamps at zero, never negative.
        record_progress(&db, "p2", "g-1", "spent lots", Some(999)).unwrap();
        assert_eq!(budget_remaining(&db, "g-1", Utc::now()).unwrap(), Some(0));
    }

    #[test]
    fn budget_remaining_is_none_when_unbounded() {
        let db = db();
        let ag = mk_ag(&db);
        insert(&db, mk_goal("g-1", ag, SessionId::new())).unwrap();
        assert_eq!(budget_remaining(&db, "g-1", Utc::now()).unwrap(), None);
    }

    #[test]
    fn budget_remaining_draws_on_the_linked_grant() {
        let db = db();
        let ag = mk_ag(&db);
        let sess = SessionId::new();
        // A task + a live grant on it, then a goal that links the grant.
        tasks::insert(
            &db,
            NewTask {
                id: "t-1".into(),
                agent_group_id: ag,
                session_id: sess,
                name: Some("standup".into()),
                prompt: "post".into(),
                when_spec: "daily at 09:00".into(),
                recurrence: Some("0 9 * * *".into()),
                next_fire: Some(Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .unwrap();
        task_grants::insert_approved(
            &db,
            NewTaskGrant {
                id: "grant-1".into(),
                task_id: "t-1".into(),
                capability_scope: "send_message:telegram".into(),
                token_budget: Some(500),
                max_fires: Some(10),
                expires_at: Some(Utc::now() + chrono::Duration::days(30)),
                granted_by: Some("op".into()),
            },
        )
        .unwrap();
        let mut spec = mk_goal("g-1", ag, sess);
        spec.grant_id = Some("grant-1".into());
        // An own budget must be IGNORED when a grant is linked (grant is authority).
        spec.token_budget = Some(1);
        insert(&db, spec).unwrap();
        assert_eq!(
            budget_remaining(&db, "g-1", Utc::now()).unwrap(),
            Some(500),
            "linked grant is the authority, not the goal's own token_budget"
        );
        // Spend against the grant; the goal's remaining follows the grant.
        task_grants::consume_tokens(&db, "grant-1", 300, Utc::now()).unwrap();
        assert_eq!(budget_remaining(&db, "g-1", Utc::now()).unwrap(), Some(200));
        // Revoke the grant → inert → zero remaining.
        task_grants::revoke(&db, "grant-1", Utc::now()).unwrap();
        assert_eq!(budget_remaining(&db, "g-1", Utc::now()).unwrap(), Some(0));
    }

    #[test]
    fn budget_remaining_zero_when_grant_superseded() {
        // A goal linked to grant A stops drawing budget once a NEWER grant B is
        // authored on the same task — `effective_grant` returns only the newest
        // (B), so the goal's grant (A) is no longer authoritative → zero budget.
        let db = db();
        let ag = mk_ag(&db);
        let sess = SessionId::new();
        tasks::insert(
            &db,
            NewTask {
                id: "t-1".into(),
                agent_group_id: ag,
                session_id: sess,
                name: Some("standup".into()),
                prompt: "post".into(),
                when_spec: "daily at 09:00".into(),
                recurrence: Some("0 9 * * *".into()),
                next_fire: Some(Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .unwrap();
        let grant = |id: &str| NewTaskGrant {
            id: id.into(),
            task_id: "t-1".into(),
            capability_scope: "send_message".into(),
            token_budget: Some(500),
            max_fires: Some(10),
            expires_at: Some(Utc::now() + chrono::Duration::days(30)),
            granted_by: Some("op".into()),
        };
        task_grants::insert_approved(&db, grant("grant-A")).unwrap();
        let mut spec = mk_goal("g-1", ag, sess);
        spec.grant_id = Some("grant-A".into());
        insert(&db, spec).unwrap();
        assert_eq!(budget_remaining(&db, "g-1", Utc::now()).unwrap(), Some(500));
        // Author a newer grant on the same task; A is superseded.
        std::thread::sleep(std::time::Duration::from_millis(2));
        task_grants::insert_approved(&db, grant("grant-B")).unwrap();
        assert_eq!(
            budget_remaining(&db, "g-1", Utc::now()).unwrap(),
            Some(0),
            "goal's grant is no longer the authoritative (newest) grant → no budget"
        );
    }

    #[test]
    fn count_active_tracks_lifecycle() {
        let db = db();
        let ag = mk_ag(&db);
        insert(&db, mk_goal("a", ag, SessionId::new())).unwrap();
        insert(&db, mk_goal("b", ag, SessionId::new())).unwrap();
        assert_eq!(count_active(&db).unwrap(), 2);
        set_status(&db, "a", GoalStatus::Completed).unwrap();
        assert_eq!(count_active(&db).unwrap(), 1);
    }
}
