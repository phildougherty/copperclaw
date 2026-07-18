//! CRUD + liveness for `task_grants` — the durable, human-approved, bounded
//! authorization that lets an AUTONOMOUS (scheduled / heartbeat) fire of a task
//! take a real external action (M22 A1).
//!
//! # Where this fits
//!
//! `schedule_task` (`copperclaw-mcp`) can carry an optional `grant`. The grant
//! is NOT stored when the tool runs — authoring is approval-gated exactly like
//! `save_skill`: the host raises an approval card, and only the approval APPLY
//! arm (`copperclaw-host::handlers::approvals`) inserts a row here, already
//! `status = Approved`. So a row in this table is, by construction, one a human
//! approved. The pending state lives entirely in `pending_approvals`.
//!
//! # The read contract A2 depends on ([`effective_grant`])
//!
//! [`effective_grant`] is the SINGLE source of truth for "does this task have a
//! live grant right now, and what's left of it." It returns `Some(EffectiveGrant)`
//! **only** when the newest grant for the task is:
//!
//!   * `status = Approved` (a human approved it), AND
//!   * not revoked (`revoked_at IS NULL`), AND
//!   * not past `expires_at` at the supplied `now`, AND
//!   * has budget remaining (`tokens_consumed < token_budget` when a budget is
//!     set) AND fires remaining (`fires_consumed < max_fires` when set).
//!
//! Any other state — no grant, revoked, expired, or exhausted — reads as `None`
//! (inert). A2's autonomy gate calls this at turn start; a `None` means the
//! autonomous turn stays in read-then-propose (blocked, emits an approval card),
//! a `Some` means A2 may set the turn's `approved` flag *scoped to* the grant
//! (never blanket) and let capabilities the grant [`permits`](EffectiveGrant::permits)
//! through.
//!
//! # `capability_scope` encoding & matching (A2 matches against this)
//!
//! A grant's `capability_scope` is a **space-separated set of scope tokens**.
//! Each token is either `class` or `class:resource`:
//!
//!   * `class` — a capability class, e.g. `send_message`, `send_email`,
//!     `web_fetch`, `create_agent`. Lower-case, no spaces.
//!   * `class:resource` — the same class narrowed to one resource, e.g.
//!     `send_message:telegram` (a specific channel) or
//!     `send_email:standup@corp.example`.
//!
//! An action A2 wants to take names the capability it *requires* in the same
//! `class` or `class:resource` grammar. [`EffectiveGrant::permits`] (and the
//! free function [`scope_permits`]) decide the match with these rules — matching
//! is **case-sensitive** and there is **no implicit cross-class matching**:
//!
//!   1. Exact match: a token equal to `required` permits it.
//!   2. Class-level grant: a **bare** token (`class`, or the explicit wildcard
//!      `class:*`) permits ANY `required` in that same class — both the bare
//!      `class` and any `class:resource`.
//!   3. Resource-scoped grant: a `class:resource` token permits ONLY the exact
//!      `class:resource` required. It does NOT widen to the bare class and does
//!      NOT match a different resource.
//!
//! So a grant scoped `send_message:telegram` permits `send_message:telegram`
//! but neither `send_message:email` nor a bare `send_message`; a grant scoped
//! `send_message` (or `send_message:*`) permits every `send_message:<channel>`.
//! An empty scope permits nothing. When a task has several tokens, the action
//! is permitted if ANY token permits it.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Row, params};

/// Lifecycle of a grant row. Stored as TEXT. A row is only ever inserted
/// `Approved` (by the approval apply arm); [`revoke`] flips it to `Revoked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    /// A human approved this specific bounded grant.
    Approved,
    /// The grant was revoked; it reads inert regardless of budget / expiry.
    Revoked,
}

impl GrantStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Revoked => "revoked",
        }
    }
}

impl std::str::FromStr for GrantStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "approved" => Ok(Self::Approved),
            "revoked" => Ok(Self::Revoked),
            other => Err(format!("unknown grant status `{other}`")),
        }
    }
}

/// One full row of `task_grants`. Callers that only need "is this live and
/// what's left" should use [`effective_grant`] instead of matching on this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskGrant {
    pub id: String,
    pub task_id: String,
    /// Space-separated scope tokens; see the module docs for the grammar.
    pub capability_scope: String,
    pub token_budget: Option<i64>,
    pub tokens_consumed: i64,
    pub max_fires: Option<i64>,
    pub fires_consumed: i64,
    pub expires_at: Option<DateTime<Utc>>,
    pub granted_by: Option<String>,
    pub status: GrantStatus,
    pub approved_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insert spec for an already-APPROVED grant. There is deliberately no
/// "insert pending" path here — the pending state is the `pending_approvals`
/// row; a `task_grants` row exists only after a human approved it.
#[derive(Debug, Clone)]
pub struct NewTaskGrant {
    pub id: String,
    pub task_id: String,
    pub capability_scope: String,
    pub token_budget: Option<i64>,
    pub max_fires: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub granted_by: Option<String>,
}

/// A grant that is LIVE right now: approved, not revoked, not expired, and with
/// budget + fires remaining. Produced only by [`effective_grant`]; its mere
/// existence means the grant is usable. Consumers ([`super::super`] A2's gate)
/// then ask [`permits`](Self::permits) whether a specific action is in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveGrant {
    pub id: String,
    pub task_id: String,
    /// Space-separated scope tokens; see the module docs for the grammar.
    pub capability_scope: String,
    /// Total token bound (`None` = unbounded by tokens).
    pub token_budget: Option<i64>,
    pub tokens_consumed: i64,
    /// Total fire bound (`None` = unbounded by fires).
    pub max_fires: Option<i64>,
    pub fires_consumed: i64,
    pub expires_at: Option<DateTime<Utc>>,
    pub granted_by: Option<String>,
    pub approved_at: Option<DateTime<Utc>>,
}

impl EffectiveGrant {
    /// Does this live grant authorize an action whose required capability is
    /// `required` (a `class` or `class:resource` token)? See the module-level
    /// docs for the full matching grammar. Case-sensitive; no cross-class
    /// widening; an empty scope permits nothing.
    #[must_use]
    pub fn permits(&self, required: &str) -> bool {
        scope_permits(&self.capability_scope, required)
    }

    /// Tokens left before the grant reads inert. `None` when unbounded by
    /// tokens. Never negative (clamped at 0).
    #[must_use]
    pub fn tokens_remaining(&self) -> Option<i64> {
        self.token_budget.map(|b| (b - self.tokens_consumed).max(0))
    }

    /// Fires left before the grant reads inert. `None` when unbounded by fires.
    /// Never negative (clamped at 0).
    #[must_use]
    pub fn fires_remaining(&self) -> Option<i64> {
        self.max_fires.map(|m| (m - self.fires_consumed).max(0))
    }
}

/// Decide whether a whitespace-delimited `scope` set permits a `required`
/// capability token. Public so A2 (and its tests) can match without re-parsing.
/// See the module docs for the grammar; matching is case-sensitive.
#[must_use]
pub fn scope_permits(scope: &str, required: &str) -> bool {
    let required = required.trim();
    if required.is_empty() {
        return false;
    }
    let (req_class, _req_res) = split_scope(required);
    scope.split_whitespace().any(|token| {
        if token == required {
            return true;
        }
        let (tok_class, tok_res) = split_scope(token);
        // Class-level grant: a bare `class` or explicit `class:*` token covers
        // every resource (and the bare class) within the same class. A
        // resource-scoped token only matches via the exact-equality check above.
        let class_level = matches!(tok_res, None | Some("*"));
        class_level && tok_class == req_class
    })
}

/// Split a scope token into `(class, resource)`. `class` → `(class, None)`;
/// `class:resource` → `(class, Some(resource))`. Only the first `:` splits, so
/// a resource may itself contain `:` (e.g. an address).
fn split_scope(token: &str) -> (&str, Option<&str>) {
    match token.split_once(':') {
        Some((class, res)) => (class, Some(res)),
        None => (token, None),
    }
}

fn parse_ts(row: &Row<'_>, col: &str) -> rusqlite::Result<Option<DateTime<Utc>>> {
    // Empty-string-as-Some defence, mirroring `tasks::row_to_task`: a stray
    // `'' ` would otherwise crash chrono's RFC-3339 parser.
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

fn row_to_grant(row: &Row<'_>) -> rusqlite::Result<TaskGrant> {
    let status_str: String = row.get("status")?;
    let status: GrantStatus = status_str.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })?;
    let created_at = parse_ts(row, "created_at")?.ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            "created_at must not be null".into(),
        )
    })?;
    let updated_at = parse_ts(row, "updated_at")?.ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            "updated_at must not be null".into(),
        )
    })?;
    Ok(TaskGrant {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        capability_scope: row.get("capability_scope")?,
        token_budget: row.get("token_budget")?,
        tokens_consumed: row.get("tokens_consumed")?,
        max_fires: row.get("max_fires")?,
        fires_consumed: row.get("fires_consumed")?,
        expires_at: parse_ts(row, "expires_at")?,
        granted_by: row.get("granted_by")?,
        status,
        approved_at: parse_ts(row, "approved_at")?,
        revoked_at: parse_ts(row, "revoked_at")?,
        created_at,
        updated_at,
    })
}

const SELECT_COLS: &str = "id, task_id, capability_scope, token_budget, tokens_consumed, \
     max_fires, fires_consumed, expires_at, granted_by, status, approved_at, \
     revoked_at, created_at, updated_at";

/// Insert an already-approved grant. Called by the approval APPLY arm once a
/// human approves the pending grant card, so the row lands `status = Approved`
/// with `approved_at = now`. Returns the inserted [`TaskGrant`].
pub fn insert_approved(db: &CentralDb, grant: NewTaskGrant) -> Result<TaskGrant, DbError> {
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO task_grants
           (id, task_id, capability_scope, token_budget, tokens_consumed,
            max_fires, fires_consumed, expires_at, granted_by, status,
            approved_at, revoked_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, 0, ?6, ?7, 'approved', ?8, NULL, ?9, ?10)",
        params![
            grant.id,
            grant.task_id,
            grant.capability_scope,
            grant.token_budget,
            grant.max_fires,
            grant.expires_at.map(|t| t.to_rfc3339()),
            grant.granted_by,
            now.to_rfc3339(),
            now.to_rfc3339(),
            now.to_rfc3339(),
        ],
    )?;
    Ok(TaskGrant {
        id: grant.id,
        task_id: grant.task_id,
        capability_scope: grant.capability_scope,
        token_budget: grant.token_budget,
        tokens_consumed: 0,
        max_fires: grant.max_fires,
        fires_consumed: 0,
        expires_at: grant.expires_at,
        granted_by: grant.granted_by,
        status: GrantStatus::Approved,
        approved_at: Some(now),
        revoked_at: None,
        created_at: now,
        updated_at: now,
    })
}

/// Fetch one grant by id.
pub fn get(db: &CentralDb, id: &str) -> Result<Option<TaskGrant>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            &format!("SELECT {SELECT_COLS} FROM task_grants WHERE id = ?1"),
            params![id],
            row_to_grant,
        )
        .optional()?)
}

/// All grants for a task, newest first (any status).
pub fn list_for_task(db: &CentralDb, task_id: &str) -> Result<Vec<TaskGrant>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM task_grants WHERE task_id = ?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map(params![task_id], row_to_grant)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Revoke a grant: flip `status` → `Revoked` and stamp `revoked_at`. After this
/// [`effective_grant`] reads the task inert (unless a newer live grant exists).
/// Idempotent-ish: revoking an already-revoked grant re-stamps the timestamp.
pub fn revoke(db: &CentralDb, id: &str, now: DateTime<Utc>) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE task_grants
            SET status = 'revoked', revoked_at = ?1, updated_at = ?2
          WHERE id = ?3",
        params![now.to_rfc3339(), now.to_rfc3339(), id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Record that a grant authorized one autonomous fire: bump `fires_consumed`.
/// A2 calls this when it lets a granted fire act. Once `fires_consumed` reaches
/// `max_fires` the grant reads inert.
pub fn consume_fire(db: &CentralDb, id: &str, now: DateTime<Utc>) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE task_grants
            SET fires_consumed = fires_consumed + 1, updated_at = ?1
          WHERE id = ?2",
        params![now.to_rfc3339(), id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Record token spend against a grant: add `tokens` to `tokens_consumed`. A2
/// calls this as a granted autonomous turn spends its budget. Once the total
/// reaches `token_budget` the grant reads inert. `tokens` must be >= 0.
pub fn consume_tokens(
    db: &CentralDb,
    id: &str,
    tokens: i64,
    now: DateTime<Utc>,
) -> Result<(), DbError> {
    if tokens < 0 {
        return Err(DbError::invariant(
            "consume_tokens: `tokens` must be non-negative",
        ));
    }
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE task_grants
            SET tokens_consumed = tokens_consumed + ?1, updated_at = ?2
          WHERE id = ?3",
        params![tokens, now.to_rfc3339(), id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// **The read contract A2 consumes.** Return the LIVE grant for `task_id` at
/// `now`, or `None` if the task has no usable grant. Liveness = the newest
/// grant for the task is approved, not revoked, not past `expires_at`, and has
/// budget + fires remaining. See the module docs for the full contract.
///
/// The NEWEST grant (by `created_at`) is authoritative: if it is inert (revoked
/// / expired / exhausted) this returns `None` — it does NOT fall back to an
/// older grant. Re-authoring a grant therefore supersedes the previous one.
///
/// `now` is passed in (not read from the clock) so callers and tests get
/// deterministic expiry, mirroring `tasks::list_due`.
pub fn effective_grant(
    db: &CentralDb,
    task_id: &str,
    now: DateTime<Utc>,
) -> Result<Option<EffectiveGrant>, DbError> {
    let conn = db.conn()?;
    // Newest grant for the task, regardless of status — we apply the liveness
    // gates in Rust so the "inert" reasons stay in one place.
    let newest: Option<TaskGrant> = conn
        .query_row(
            &format!(
                "SELECT {SELECT_COLS} FROM task_grants
                  WHERE task_id = ?1
                  ORDER BY created_at DESC
                  LIMIT 1"
            ),
            params![task_id],
            row_to_grant,
        )
        .optional()?;
    let Some(g) = newest else {
        return Ok(None);
    };
    // Gate 1: must be approved and not revoked.
    if g.status != GrantStatus::Approved || g.revoked_at.is_some() {
        return Ok(None);
    }
    // Gate 2: must not be past expiry.
    if let Some(exp) = g.expires_at {
        if now >= exp {
            return Ok(None);
        }
    }
    // Gate 3: budget + fires must have headroom.
    if let Some(budget) = g.token_budget {
        if g.tokens_consumed >= budget {
            return Ok(None);
        }
    }
    if let Some(max) = g.max_fires {
        if g.fires_consumed >= max {
            return Ok(None);
        }
    }
    Ok(Some(EffectiveGrant {
        id: g.id,
        task_id: g.task_id,
        capability_scope: g.capability_scope,
        token_budget: g.token_budget,
        tokens_consumed: g.tokens_consumed,
        max_fires: g.max_fires,
        fires_consumed: g.fires_consumed,
        expires_at: g.expires_at,
        granted_by: g.granted_by,
        approved_at: g.approved_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use crate::tables::tasks::{self, NewTask};
    use copperclaw_types::{AgentGroupId, SessionId};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn seed_task(db: &CentralDb, id: &str) -> String {
        let ag: AgentGroupId = create_ag(
            db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id;
        tasks::insert(
            db,
            NewTask {
                id: id.into(),
                agent_group_id: ag,
                session_id: SessionId::new(),
                name: Some("standup".into()),
                prompt: "post standup".into(),
                when_spec: "daily at 09:00".into(),
                recurrence: Some("0 9 * * *".into()),
                next_fire: Some(Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .unwrap();
        id.to_string()
    }

    fn new_grant(id: &str, task_id: &str, scope: &str) -> NewTaskGrant {
        NewTaskGrant {
            id: id.into(),
            task_id: task_id.into(),
            capability_scope: scope.into(),
            token_budget: Some(1000),
            max_fires: Some(5),
            expires_at: Some(Utc::now() + chrono::Duration::days(30)),
            granted_by: Some("operator".into()),
        }
    }

    // -- schema / round-trip -------------------------------------------------

    #[test]
    fn insert_then_get_roundtrips() {
        let db = db();
        let t = seed_task(&db, "t-1");
        let g = insert_approved(&db, new_grant("g-1", &t, "send_message:telegram")).unwrap();
        assert_eq!(g.status, GrantStatus::Approved);
        assert!(g.approved_at.is_some());
        assert!(g.revoked_at.is_none());
        let back = get(&db, "g-1").unwrap().unwrap();
        assert_eq!(back, g);
        assert_eq!(back.capability_scope, "send_message:telegram");
        assert_eq!(back.token_budget, Some(1000));
        assert_eq!(back.max_fires, Some(5));
    }

    #[test]
    fn get_missing_is_none() {
        let db = db();
        assert!(get(&db, "ghost").unwrap().is_none());
    }

    #[test]
    fn grant_status_parses_and_emits() {
        for s in ["approved", "revoked"] {
            let parsed: GrantStatus = s.parse().unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!("bogus".parse::<GrantStatus>().is_err());
    }

    // -- effective_grant liveness (acceptance: expired/over-budget/revoked
    //    read inert) -------------------------------------------------------

    #[test]
    fn effective_grant_none_when_no_grant() {
        let db = db();
        let t = seed_task(&db, "t-1");
        assert!(effective_grant(&db, &t, Utc::now()).unwrap().is_none());
    }

    #[test]
    fn effective_grant_live_when_approved_and_bounded() {
        let db = db();
        let t = seed_task(&db, "t-1");
        insert_approved(&db, new_grant("g-1", &t, "send_message:telegram")).unwrap();
        let eff = effective_grant(&db, &t, Utc::now()).unwrap().unwrap();
        assert_eq!(eff.id, "g-1");
        assert!(eff.permits("send_message:telegram"));
        assert_eq!(eff.fires_remaining(), Some(5));
        assert_eq!(eff.tokens_remaining(), Some(1000));
    }

    #[test]
    fn revoked_grant_reads_inert() {
        let db = db();
        let t = seed_task(&db, "t-1");
        insert_approved(&db, new_grant("g-1", &t, "send_message:telegram")).unwrap();
        revoke(&db, "g-1", Utc::now()).unwrap();
        assert!(effective_grant(&db, &t, Utc::now()).unwrap().is_none());
        // The row still exists and records the revocation.
        let row = get(&db, "g-1").unwrap().unwrap();
        assert_eq!(row.status, GrantStatus::Revoked);
        assert!(row.revoked_at.is_some());
    }

    #[test]
    fn expired_grant_reads_inert() {
        let db = db();
        let t = seed_task(&db, "t-1");
        let mut spec = new_grant("g-1", &t, "send_message:telegram");
        spec.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        insert_approved(&db, spec).unwrap();
        // At now, the grant is already past expiry.
        assert!(effective_grant(&db, &t, Utc::now()).unwrap().is_none());
    }

    #[test]
    fn expiry_is_evaluated_against_supplied_now() {
        let db = db();
        let t = seed_task(&db, "t-1");
        let exp = Utc::now() + chrono::Duration::hours(1);
        let mut spec = new_grant("g-1", &t, "send_message:telegram");
        spec.expires_at = Some(exp);
        insert_approved(&db, spec).unwrap();
        // Before expiry: live.
        assert!(
            effective_grant(&db, &t, exp - chrono::Duration::minutes(1))
                .unwrap()
                .is_some()
        );
        // At / after expiry: inert.
        assert!(effective_grant(&db, &t, exp).unwrap().is_none());
        assert!(
            effective_grant(&db, &t, exp + chrono::Duration::minutes(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn over_fire_budget_reads_inert() {
        let db = db();
        let t = seed_task(&db, "t-1");
        let mut spec = new_grant("g-1", &t, "send_message:telegram");
        spec.max_fires = Some(2);
        insert_approved(&db, spec).unwrap();
        consume_fire(&db, "g-1", Utc::now()).unwrap();
        assert_eq!(
            effective_grant(&db, &t, Utc::now())
                .unwrap()
                .unwrap()
                .fires_remaining(),
            Some(1)
        );
        consume_fire(&db, "g-1", Utc::now()).unwrap();
        // Now fires_consumed == max_fires → inert.
        assert!(effective_grant(&db, &t, Utc::now()).unwrap().is_none());
    }

    #[test]
    fn over_token_budget_reads_inert() {
        let db = db();
        let t = seed_task(&db, "t-1");
        let mut spec = new_grant("g-1", &t, "send_message:telegram");
        spec.token_budget = Some(100);
        insert_approved(&db, spec).unwrap();
        consume_tokens(&db, "g-1", 40, Utc::now()).unwrap();
        assert_eq!(
            effective_grant(&db, &t, Utc::now())
                .unwrap()
                .unwrap()
                .tokens_remaining(),
            Some(60)
        );
        consume_tokens(&db, "g-1", 60, Utc::now()).unwrap();
        // tokens_consumed == token_budget → inert.
        assert!(effective_grant(&db, &t, Utc::now()).unwrap().is_none());
    }

    #[test]
    fn unbounded_budget_and_fires_stay_live() {
        let db = db();
        let t = seed_task(&db, "t-1");
        let mut spec = new_grant("g-1", &t, "send_message");
        spec.token_budget = None;
        spec.max_fires = None;
        insert_approved(&db, spec).unwrap();
        let eff = effective_grant(&db, &t, Utc::now()).unwrap().unwrap();
        assert_eq!(eff.tokens_remaining(), None);
        assert_eq!(eff.fires_remaining(), None);
        // Consuming a lot never makes an unbounded grant inert.
        consume_tokens(&db, "g-1", 1_000_000, Utc::now()).unwrap();
        for _ in 0..100 {
            consume_fire(&db, "g-1", Utc::now()).unwrap();
        }
        assert!(effective_grant(&db, &t, Utc::now()).unwrap().is_some());
    }

    #[test]
    fn newest_grant_is_authoritative_and_supersedes() {
        let db = db();
        let t = seed_task(&db, "t-1");
        // Older grant, then a newer revoked one → newest wins → inert.
        insert_approved(&db, new_grant("old", &t, "send_message:telegram")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        insert_approved(&db, new_grant("new", &t, "send_email")).unwrap();
        revoke(&db, "new", Utc::now()).unwrap();
        // Newest ("new") is revoked → inert; we do NOT fall back to "old".
        assert!(effective_grant(&db, &t, Utc::now()).unwrap().is_none());
        // list_for_task returns both, newest first.
        let all = list_for_task(&db, &t).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "new");
    }

    #[test]
    fn consume_and_revoke_unknown_id_error() {
        let db = db();
        assert!(matches!(
            consume_fire(&db, "ghost", Utc::now()).unwrap_err(),
            DbError::NotFound
        ));
        assert!(matches!(
            consume_tokens(&db, "ghost", 1, Utc::now()).unwrap_err(),
            DbError::NotFound
        ));
        assert!(matches!(
            revoke(&db, "ghost", Utc::now()).unwrap_err(),
            DbError::NotFound
        ));
    }

    #[test]
    fn consume_tokens_rejects_negative() {
        let db = db();
        let t = seed_task(&db, "t-1");
        insert_approved(&db, new_grant("g-1", &t, "send_message")).unwrap();
        assert!(matches!(
            consume_tokens(&db, "g-1", -5, Utc::now()).unwrap_err(),
            DbError::Invariant(_)
        ));
    }

    // -- capability_scope matching (A2's contract) --------------------------

    #[test]
    fn scope_exact_match() {
        assert!(scope_permits(
            "send_message:telegram",
            "send_message:telegram"
        ));
        assert!(scope_permits("send_email", "send_email"));
    }

    #[test]
    fn scope_resource_grant_does_not_widen() {
        // A resource-scoped grant matches only that exact resource.
        assert!(!scope_permits(
            "send_message:telegram",
            "send_message:email"
        ));
        assert!(!scope_permits("send_message:telegram", "send_message"));
    }

    #[test]
    fn scope_class_level_grant_covers_all_resources() {
        // Bare class and explicit wildcard both cover every resource + the bare
        // class.
        for grant in ["send_message", "send_message:*"] {
            assert!(scope_permits(grant, "send_message:telegram"), "{grant}");
            assert!(scope_permits(grant, "send_message:email"), "{grant}");
            assert!(scope_permits(grant, "send_message"), "{grant}");
        }
    }

    #[test]
    fn scope_no_cross_class_matching() {
        assert!(!scope_permits("send_message", "send_email"));
        assert!(!scope_permits(
            "send_message:telegram",
            "send_email:telegram"
        ));
    }

    #[test]
    fn scope_multi_token_any_permits() {
        let scope = "send_message:telegram web_fetch send_email:standup@corp";
        assert!(scope_permits(scope, "send_message:telegram"));
        assert!(scope_permits(scope, "web_fetch:https://x")); // web_fetch is class-level
        assert!(scope_permits(scope, "send_email:standup@corp"));
        assert!(!scope_permits(scope, "send_message:email"));
        assert!(!scope_permits(scope, "create_agent"));
    }

    #[test]
    fn scope_empty_permits_nothing() {
        assert!(!scope_permits("", "send_message"));
        assert!(!scope_permits("   ", "send_message"));
        assert!(!scope_permits("send_message", ""));
        assert!(!scope_permits("send_message", "   "));
    }

    #[test]
    fn scope_resource_may_contain_colon() {
        // Only the first ':' splits, so an address-style resource round-trips.
        assert!(scope_permits(
            "send_email:user@host:8080",
            "send_email:user@host:8080"
        ));
        // Class-level send_email still covers it.
        assert!(scope_permits("send_email", "send_email:user@host:8080"));
    }
}
