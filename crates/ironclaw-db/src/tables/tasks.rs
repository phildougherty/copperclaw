//! CRUD for `tasks` — scheduled agent tasks.
//!
//! The runner emits a `kind: system` outbound row with
//! `content == { "schedule": { "op": ..., "payload": ... } }` whenever
//! the agent calls `schedule_task` / `list_tasks` / `cancel_task` /
//! `pause_task` / `resume_task` / `update_task`. The host's delivery
//! action handler (`SchedulingModule`) writes here; the sweep loop
//! reads here to fire due tasks.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, SessionId};
use rusqlite::{params, OptionalExtension, Row};

/// Lifecycle states of a task. Stored as TEXT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Active,
    Paused,
    Cancelled,
    Completed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
        }
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "cancelled" => Ok(Self::Cancelled),
            "completed" => Ok(Self::Completed),
            other => Err(format!("unknown task status `{other}`")),
        }
    }
}

/// One row of `tasks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub name: Option<String>,
    pub prompt: String,
    pub when_spec: String,
    pub recurrence: Option<String>,
    pub next_fire: Option<DateTime<Utc>>,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Insert spec.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub id: String,
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub name: Option<String>,
    pub prompt: String,
    pub when_spec: String,
    pub recurrence: Option<String>,
    pub next_fire: Option<DateTime<Utc>>,
}

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let agent_group_id_str: String = row.get("agent_group_id")?;
    let ag_uuid = uuid::Uuid::parse_str(&agent_group_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let session_id_str: String = row.get("session_id")?;
    let sess_uuid = uuid::Uuid::parse_str(&session_id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let status_str: String = row.get("status")?;
    let status: TaskStatus = status_str.parse().map_err(|e: String| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
    })?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let updated_at_str: String = row.get("updated_at")?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    // Empty-string-as-Some defence: a `next_fire = ''` row would
    // otherwise crash the parser with chrono's `TooShort`.
    let next_fire_str: Option<String> = row.get("next_fire")?;
    let next_fire = match next_fire_str.as_deref() {
        None | Some("") => None,
        Some(ts) => Some(
            DateTime::parse_from_rfc3339(ts)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
        ),
    };
    Ok(Task {
        id: row.get("id")?,
        agent_group_id: AgentGroupId(ag_uuid),
        session_id: SessionId::from(sess_uuid),
        name: row.get("name")?,
        prompt: row.get("prompt")?,
        when_spec: row.get("when_spec")?,
        recurrence: row.get("recurrence")?,
        next_fire,
        status,
        created_at,
        updated_at,
    })
}

/// Insert a new task. Returns the inserted `Task` with timestamps populated.
pub fn insert(db: &CentralDb, task: NewTask) -> Result<Task, DbError> {
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO tasks
           (id, agent_group_id, session_id, name, prompt, when_spec,
            recurrence, next_fire, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?10)",
        params![
            task.id,
            task.agent_group_id.as_uuid().to_string(),
            task.session_id.as_uuid().to_string(),
            task.name,
            task.prompt,
            task.when_spec,
            task.recurrence,
            task.next_fire.map(|t| t.to_rfc3339()),
            now.to_rfc3339(),
            now.to_rfc3339(),
        ],
    )?;
    Ok(Task {
        id: task.id,
        agent_group_id: task.agent_group_id,
        session_id: task.session_id,
        name: task.name,
        prompt: task.prompt,
        when_spec: task.when_spec,
        recurrence: task.recurrence,
        next_fire: task.next_fire,
        status: TaskStatus::Active,
        created_at: now,
        updated_at: now,
    })
}

/// Get one task by id.
pub fn get(db: &CentralDb, id: &str) -> Result<Option<Task>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT id, agent_group_id, session_id, name, prompt, when_spec,
                    recurrence, next_fire, status, created_at, updated_at
             FROM tasks
             WHERE id = ?1",
            params![id],
            row_to_task,
        )
        .optional()?)
}

/// List tasks for one session (any status).
pub fn list_for_session(db: &CentralDb, session_id: SessionId) -> Result<Vec<Task>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, agent_group_id, session_id, name, prompt, when_spec,
                recurrence, next_fire, status, created_at, updated_at
         FROM tasks
         WHERE session_id = ?1
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![session_id.as_uuid().to_string()], row_to_task)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// List all tasks in `active` status whose `next_fire <= now`.
pub fn list_due(db: &CentralDb, now: DateTime<Utc>) -> Result<Vec<Task>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, agent_group_id, session_id, name, prompt, when_spec,
                recurrence, next_fire, status, created_at, updated_at
         FROM tasks
         WHERE status = 'active'
           AND next_fire IS NOT NULL
           AND next_fire <= ?1
         ORDER BY next_fire",
    )?;
    let rows = stmt.query_map(params![now.to_rfc3339()], row_to_task)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Update status only.
pub fn set_status(db: &CentralDb, id: &str, status: TaskStatus) -> Result<(), DbError> {
    let conn = db.conn()?;
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status.as_str(), now, id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Update `next_fire` (used both when re-arming a recurring task and when
/// patching via the `update` op).
pub fn set_next_fire(
    db: &CentralDb,
    id: &str,
    next_fire: Option<DateTime<Utc>>,
) -> Result<(), DbError> {
    let conn = db.conn()?;
    let now = Utc::now().to_rfc3339();
    let n = conn.execute(
        "UPDATE tasks SET next_fire = ?1, updated_at = ?2 WHERE id = ?3",
        params![next_fire.map(|t| t.to_rfc3339()), now, id],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Patch fields on a task. Any `Some(_)` field overwrites; `None` leaves
/// the column untouched. Returns the post-update row.
#[derive(Debug, Clone, Default)]
pub struct UpdateFields {
    pub prompt: Option<String>,
    pub when_spec: Option<String>,
    pub recurrence: Option<Option<String>>,
    pub next_fire: Option<Option<DateTime<Utc>>>,
}

pub fn update(db: &CentralDb, id: &str, fields: UpdateFields) -> Result<Task, DbError> {
    {
        let conn = db.conn()?;
        let now = Utc::now().to_rfc3339();
        if let Some(p) = fields.prompt {
            conn.execute(
                "UPDATE tasks SET prompt = ?1, updated_at = ?2 WHERE id = ?3",
                params![p, now, id],
            )?;
        }
        if let Some(w) = fields.when_spec {
            conn.execute(
                "UPDATE tasks SET when_spec = ?1, updated_at = ?2 WHERE id = ?3",
                params![w, now, id],
            )?;
        }
        if let Some(rec) = fields.recurrence {
            conn.execute(
                "UPDATE tasks SET recurrence = ?1, updated_at = ?2 WHERE id = ?3",
                params![rec, now, id],
            )?;
        }
        if let Some(nf) = fields.next_fire {
            conn.execute(
                "UPDATE tasks SET next_fire = ?1, updated_at = ?2 WHERE id = ?3",
                params![nf.map(|t| t.to_rfc3339()), now, id],
            )?;
        }
    }
    get(db, id)?.ok_or(DbError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn mk_ag(db: &CentralDb, name: &str) -> AgentGroupId {
        create_ag(
            db,
            CreateAgentGroup {
                name: name.into(),
                folder: name.into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    fn mk_task(ag: AgentGroupId, sess: SessionId, id: &str) -> NewTask {
        NewTask {
            id: id.into(),
            agent_group_id: ag,
            session_id: sess,
            name: Some("test".into()),
            prompt: "do the thing".into(),
            when_spec: "in 1h".into(),
            recurrence: None,
            next_fire: Some(Utc::now() + chrono::Duration::hours(1)),
        }
    }

    #[test]
    fn insert_then_get() {
        let db = db();
        let ag = mk_ag(&db, "g");
        let sess = SessionId::new();
        let t = insert(&db, mk_task(ag, sess, "t-1")).unwrap();
        assert_eq!(t.id, "t-1");
        let fetched = get(&db, "t-1").unwrap().unwrap();
        assert_eq!(fetched.id, "t-1");
        assert_eq!(fetched.status, TaskStatus::Active);
    }

    #[test]
    fn get_missing_is_none() {
        let db = db();
        assert!(get(&db, "ghost").unwrap().is_none());
    }

    #[test]
    fn list_for_session_filters() {
        let db = db();
        let ag = mk_ag(&db, "g");
        let sess_a = SessionId::new();
        let sess_b = SessionId::new();
        insert(&db, mk_task(ag, sess_a, "a1")).unwrap();
        insert(&db, mk_task(ag, sess_a, "a2")).unwrap();
        insert(&db, mk_task(ag, sess_b, "b1")).unwrap();
        let listed = list_for_session(&db, sess_a).unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn list_due_returns_only_active_and_past_fire() {
        let db = db();
        let ag = mk_ag(&db, "g");
        let sess = SessionId::new();
        let now = Utc::now();
        let mut t1 = mk_task(ag, sess, "past");
        t1.next_fire = Some(now - chrono::Duration::minutes(1));
        let mut t2 = mk_task(ag, sess, "future");
        t2.next_fire = Some(now + chrono::Duration::hours(1));
        insert(&db, t1).unwrap();
        insert(&db, t2).unwrap();
        let due = list_due(&db, now).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "past");
    }

    #[test]
    fn list_due_excludes_paused_cancelled_completed() {
        let db = db();
        let ag = mk_ag(&db, "g");
        let sess = SessionId::new();
        let now = Utc::now();
        let mut spec = mk_task(ag, sess, "a");
        spec.next_fire = Some(now - chrono::Duration::minutes(1));
        insert(&db, spec).unwrap();
        set_status(&db, "a", TaskStatus::Paused).unwrap();
        assert!(list_due(&db, now).unwrap().is_empty());
    }

    #[test]
    fn set_status_updates() {
        let db = db();
        let ag = mk_ag(&db, "g");
        let sess = SessionId::new();
        insert(&db, mk_task(ag, sess, "a")).unwrap();
        set_status(&db, "a", TaskStatus::Cancelled).unwrap();
        let t = get(&db, "a").unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Cancelled);
    }

    #[test]
    fn set_status_unknown_id_errors() {
        let db = db();
        assert!(matches!(
            set_status(&db, "ghost", TaskStatus::Paused).unwrap_err(),
            DbError::NotFound
        ));
    }

    #[test]
    fn update_fields_patches_selected_columns() {
        let db = db();
        let ag = mk_ag(&db, "g");
        let sess = SessionId::new();
        insert(&db, mk_task(ag, sess, "a")).unwrap();
        let updated = update(
            &db,
            "a",
            UpdateFields {
                prompt: Some("new prompt".into()),
                when_spec: Some("daily at 09:00".into()),
                recurrence: Some(Some("0 9 * * *".into())),
                next_fire: Some(Some(Utc::now() + chrono::Duration::hours(2))),
            },
        )
        .unwrap();
        assert_eq!(updated.prompt, "new prompt");
        assert_eq!(updated.when_spec, "daily at 09:00");
        assert_eq!(updated.recurrence.as_deref(), Some("0 9 * * *"));
    }

    #[test]
    fn task_status_parses_and_emits() {
        for s in ["active", "paused", "cancelled", "completed"] {
            let parsed: TaskStatus = s.parse().unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!("bogus".parse::<TaskStatus>().is_err());
    }

    #[test]
    fn set_next_fire_replaces_value() {
        let db = db();
        let ag = mk_ag(&db, "g");
        let sess = SessionId::new();
        insert(&db, mk_task(ag, sess, "a")).unwrap();
        let new = Utc::now() + chrono::Duration::days(1);
        set_next_fire(&db, "a", Some(new)).unwrap();
        let t = get(&db, "a").unwrap().unwrap();
        assert_eq!(t.next_fire.unwrap().timestamp(), new.timestamp());
    }
}
