//! Sqlite-backed [`TaskStore`] for the scheduling module.
//!
//! This adapter lives in the sweep crate (rather than in `ironclaw-modules`)
//! so the modules crate stays decoupled from `ironclaw-db`. Boot wires
//! the host's [`CentralDb`] through here and passes the resulting store
//! into [`SchedulingModule::with_store`].
//!
//! The same `tasks` table is read directly by [`crate::checks::scheduling`]
//! during the sweep loop's due-task fan-out.

use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::tasks::{self, NewTask, TaskStatus as DbTaskStatus, UpdateFields};
use ironclaw_db::DbError;
use ironclaw_modules::scheduling::{
    CreateTaskSpec, TaskRecord, TaskStatus, TaskStore, UpdateTaskFields,
};
use ironclaw_modules::ModuleError;

/// Production [`TaskStore`] backed by the central sqlite database.
pub struct SqliteTaskStore {
    central: CentralDb,
}

impl SqliteTaskStore {
    pub fn new(central: CentralDb) -> Self {
        Self { central }
    }
}

fn db_to_module_status(s: DbTaskStatus) -> TaskStatus {
    match s {
        DbTaskStatus::Active => TaskStatus::Active,
        DbTaskStatus::Paused => TaskStatus::Paused,
        DbTaskStatus::Cancelled => TaskStatus::Cancelled,
        DbTaskStatus::Completed => TaskStatus::Completed,
    }
}

fn module_to_db_status(s: TaskStatus) -> DbTaskStatus {
    match s {
        TaskStatus::Active => DbTaskStatus::Active,
        TaskStatus::Paused => DbTaskStatus::Paused,
        TaskStatus::Cancelled => DbTaskStatus::Cancelled,
        TaskStatus::Completed => DbTaskStatus::Completed,
    }
}

fn row_to_module(t: tasks::Task) -> TaskRecord {
    TaskRecord {
        id: t.id,
        agent_group_id: t.agent_group_id,
        session_id: t.session_id,
        name: t.name,
        prompt: t.prompt,
        when_spec: t.when_spec,
        recurrence: t.recurrence,
        next_fire: t.next_fire,
        status: db_to_module_status(t.status),
        created_at: t.created_at,
        updated_at: t.updated_at,
    }
}

fn db_err_to_module(e: &DbError) -> ModuleError {
    ModuleError::other("scheduling", e.to_string())
}

impl TaskStore for SqliteTaskStore {
    fn create(&self, spec: CreateTaskSpec) -> Result<TaskRecord, ModuleError> {
        let id = format!("task_{}", uuid::Uuid::now_v7());
        let row = tasks::insert(
            &self.central,
            NewTask {
                id,
                agent_group_id: spec.agent_group_id,
                session_id: spec.session_id,
                name: spec.name,
                prompt: spec.prompt,
                when_spec: spec.when_spec,
                recurrence: spec.recurrence,
                next_fire: spec.next_fire,
            },
        )
        .map_err(|e| db_err_to_module(&e))?;
        Ok(row_to_module(row))
    }

    fn get(&self, id: &str) -> Result<Option<TaskRecord>, ModuleError> {
        Ok(tasks::get(&self.central, id)
            .map_err(|e| db_err_to_module(&e))?
            .map(row_to_module))
    }

    fn list_for_session(
        &self,
        session_id: ironclaw_types::SessionId,
    ) -> Result<Vec<TaskRecord>, ModuleError> {
        Ok(tasks::list_for_session(&self.central, session_id)
            .map_err(|e| db_err_to_module(&e))?
            .into_iter()
            .map(row_to_module)
            .collect())
    }

    fn set_status(&self, id: &str, status: TaskStatus) -> Result<(), ModuleError> {
        tasks::set_status(&self.central, id, module_to_db_status(status))
            .map_err(|e| db_err_to_module(&e))
    }

    fn update(&self, id: &str, fields: UpdateTaskFields) -> Result<TaskRecord, ModuleError> {
        let db_fields = UpdateFields {
            prompt: fields.prompt,
            when_spec: fields.when_spec,
            recurrence: fields.recurrence,
            next_fire: fields.next_fire,
        };
        let row = tasks::update(&self.central, id, db_fields).map_err(|e| db_err_to_module(&e))?;
        Ok(row_to_module(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_types::{AgentGroupId, SessionId};

    fn fresh() -> (SqliteTaskStore, AgentGroupId, SessionId) {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id;
        let sess = SessionId::new();
        (SqliteTaskStore::new(db), ag, sess)
    }

    #[test]
    fn create_then_get() {
        let (store, ag, sess) = fresh();
        let rec = store
            .create(CreateTaskSpec {
                agent_group_id: ag,
                session_id: sess,
                name: Some("n".into()),
                prompt: "p".into(),
                when_spec: "in 1m".into(),
                recurrence: None,
                next_fire: Some(chrono::Utc::now()),
            })
            .unwrap();
        let back = store.get(&rec.id).unwrap().unwrap();
        assert_eq!(back.id, rec.id);
        assert_eq!(back.status, TaskStatus::Active);
    }

    #[test]
    fn set_status_roundtrips_each_variant() {
        let (store, ag, sess) = fresh();
        let rec = store
            .create(CreateTaskSpec {
                agent_group_id: ag,
                session_id: sess,
                name: None,
                prompt: "p".into(),
                when_spec: "in 1m".into(),
                recurrence: None,
                next_fire: None,
            })
            .unwrap();
        for s in [
            TaskStatus::Paused,
            TaskStatus::Active,
            TaskStatus::Completed,
            TaskStatus::Cancelled,
        ] {
            store.set_status(&rec.id, s).unwrap();
            assert_eq!(store.get(&rec.id).unwrap().unwrap().status, s);
        }
    }

    #[test]
    fn update_patches_fields() {
        let (store, ag, sess) = fresh();
        let rec = store
            .create(CreateTaskSpec {
                agent_group_id: ag,
                session_id: sess,
                name: None,
                prompt: "old".into(),
                when_spec: "in 1m".into(),
                recurrence: None,
                next_fire: None,
            })
            .unwrap();
        let updated = store
            .update(
                &rec.id,
                UpdateTaskFields {
                    prompt: Some("new".into()),
                    when_spec: Some("daily at 09:00".into()),
                    recurrence: Some(Some("0 9 * * *".into())),
                    next_fire: None,
                },
            )
            .unwrap();
        assert_eq!(updated.prompt, "new");
        assert_eq!(updated.when_spec, "daily at 09:00");
        assert_eq!(updated.recurrence.as_deref(), Some("0 9 * * *"));
    }

    #[test]
    fn list_for_session_returns_only_matching() {
        let (store, ag, sess) = fresh();
        for _ in 0..3 {
            store
                .create(CreateTaskSpec {
                    agent_group_id: ag,
                    session_id: sess,
                    name: None,
                    prompt: "p".into(),
                    when_spec: "in 1m".into(),
                    recurrence: None,
                    next_fire: None,
                })
                .unwrap();
        }
        let other = SessionId::new();
        store
            .create(CreateTaskSpec {
                agent_group_id: ag,
                session_id: other,
                name: None,
                prompt: "p".into(),
                when_spec: "in 1m".into(),
                recurrence: None,
                next_fire: None,
            })
            .unwrap();
        assert_eq!(store.list_for_session(sess).unwrap().len(), 3);
        assert_eq!(store.list_for_session(other).unwrap().len(), 1);
    }
}
