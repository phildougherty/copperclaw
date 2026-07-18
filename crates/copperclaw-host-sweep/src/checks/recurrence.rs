//! Recurrence consolidation (M22 A5).
//!
//! Historically this check was a *second, per-session* recurrence engine: for
//! each `messages_in` row with a non-null `recurrence` it self-replicated a
//! fresh `messages_in` row at the next computed fire time. That duplicated the
//! central `tasks` scheduler (`checks/scheduling.rs`, migration 028) and —
//! unlike it — had no lifecycle surface: an operator could not `list` / `pause`
//! / `resume` / `cancel` a per-session recurring series.
//!
//! M22 A5 collapses the two mechanisms onto one. This module is now a
//! **deprecation shim**: rather than self-replicate, it forwards each legacy
//! per-session recurring series into the central `tasks` scheduler and then
//! neutralises the source rows so the old path never fires that series again.
//! Once forwarded, the series is a first-class `tasks` row and gains
//! `list_tasks` / `pause_task` / `resume_task` / `cancel_task` for free.
//!
//! The forward is idempotent. The synthesised task id is derived
//! deterministically from the series (`recurring:<session>:<series_key>`), so a
//! series is migrated exactly once; and because the migration clears the
//! `recurrence` column on the source rows, an already-migrated series no longer
//! appears in the per-session scan on the next pass. An operator who later
//! cancels the migrated task is respected — the deterministic id means the shim
//! will not resurrect a task that already exists in any state.
//!
//! Backward compatibility: existing recurring sessions on disk (rows written
//! before this change, or by any straggler code path that still sets
//! `messages_in.recurrence`) are converted automatically the first time the
//! sweep observes them. That data-path conversion lives here, at runtime,
//! because per-session recurrence rows live in each session's `inbound.db` — a
//! central-DB SQL migration cannot reach them, so no numbered migration is
//! added for A5.

use crate::error::SweepError;
use crate::service::{SeriesFanout, SessionRoot};
use chrono::{DateTime, Utc};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::tasks::{self, NewTask};
use copperclaw_modules::scheduling::{compute_next_fire, parse_when};
use copperclaw_types::{AgentGroupId, MessageId, SessionId};
use rusqlite::Connection;

/// The newest still-recurring member of one legacy per-session series, plus
/// the fields the shim needs to forward it into the central scheduler.
#[derive(Debug, Clone)]
struct RecurrenceParent {
    id: MessageId,
    recurrence: String,
    series_id: Option<String>,
    content: serde_json::Value,
}

/// Forward every legacy per-session recurring series in this session into the
/// central `tasks` scheduler, returning one [`SeriesFanout`] per series
/// consolidated during this pass.
///
/// This no longer inserts `messages_in` rows: the central scheduler
/// (`checks/scheduling.rs`) owns firing from here on. Each returned
/// [`SeriesFanout`] carries the series id, the source row's id (so operators
/// can still correlate a consolidation back to its originating message), and
/// the task's next computed fire time.
pub fn check(
    central: &CentralDb,
    root: &dyn SessionRoot,
    agent_group_id: &AgentGroupId,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Result<Vec<SeriesFanout>, SweepError> {
    let mut inbound = root.inbound_pool(agent_group_id, session_id)?;
    let parents = newest_per_series(inbound.conn(), now)?;
    let mut out = Vec::with_capacity(parents.len());
    for parent in parents {
        let next_fire = match compute_next(&parent.recurrence, now) {
            Ok(Some(t)) => t,
            // A recurrence that yields no future occurrence is inert — leave the
            // row untouched (do not migrate, do not clear) so behaviour matches
            // the historical "skip" path exactly.
            Ok(None) => continue,
            Err(_e) => {
                // Skip rows with unparseable cron, exactly as the pre-A5 engine
                // did. We deliberately do NOT clear these — an operator fixing
                // the expression out-of-band should still be able to migrate it.
                tracing::warn!(
                    target: "copperclaw_host_sweep::recurrence",
                    recurrence = %parent.recurrence,
                    "skipping series with unparseable recurrence",
                );
                continue;
            }
        };

        let series_id = parent
            .series_id
            .clone()
            .unwrap_or_else(|| parent.id.as_uuid().to_string());

        // Deprecation shim: forward the series into the central `tasks`
        // scheduler. Idempotent on the deterministic task id — if a task
        // already exists for this series (in any status, including one an
        // operator cancelled), we do not recreate it.
        let task_id = migrated_task_id(session_id, &series_id);
        if tasks::get(central, &task_id)?.is_none() {
            tasks::insert(
                central,
                NewTask {
                    id: task_id,
                    agent_group_id: *agent_group_id,
                    session_id: *session_id,
                    name: Some(format!("recurring-{series_id}")),
                    prompt: prompt_text(&parent.content),
                    // `when_spec` is a cron string here (5/6 fields), which
                    // `parse_when` accepts; `recurrence` re-arms it each fire.
                    when_spec: parent.recurrence.clone(),
                    recurrence: Some(parent.recurrence.clone()),
                    next_fire: Some(next_fire),
                },
            )?;
        }

        // Neutralise the legacy self-replication path for this series so the
        // central scheduler is now the single source of truth. Clearing
        // `recurrence` also makes the migration self-idempotent: the series
        // stops appearing in `newest_per_series` on subsequent passes.
        clear_series_recurrence(inbound.conn_mut(), &series_id)?;

        out.push(SeriesFanout {
            series_id,
            new_message_id: parent.id,
            next_fire,
        });
    }
    Ok(out)
}

/// Deterministic id for the central task a legacy series is forwarded into.
/// Keyed on `(session, series)` so a series migrates exactly once, and the
/// `recurring:` prefix keeps it from ever colliding with an agent-authored
/// `task_<uuid>` id.
fn migrated_task_id(session_id: &SessionId, series_id: &str) -> String {
    format!("recurring:{}:{}", session_id.as_uuid(), series_id)
}

/// Pull the human-facing prompt out of a recurring row's content, falling back
/// to the raw JSON when there is no `text` field.
fn prompt_text(content: &serde_json::Value) -> String {
    content
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| content.to_string(), str::to_owned)
}

/// Clear the `recurrence` column on every row of one series (identified by
/// `COALESCE(series_id, id)`), so the legacy per-session path stops firing it.
fn clear_series_recurrence(conn: &Connection, series_key: &str) -> Result<(), SweepError> {
    conn.execute(
        "UPDATE messages_in
            SET recurrence = NULL
          WHERE COALESCE(series_id, id) = ?1
            AND recurrence IS NOT NULL",
        rusqlite::params![series_key],
    )?;
    Ok(())
}

fn compute_next(recurrence: &str, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>, SweepError> {
    let when = parse_when(recurrence).map_err(|e| SweepError::ScheduleParse(e.to_string()))?;
    Ok(compute_next_fire(&when, now, Some(recurrence)))
}

/// Find the newest still-recurring member of each series whose `process_after`
/// is at or before `now`. A series is identified by `series_id` when present
/// and by the row's own `id` otherwise (so the very first member of a series is
/// still correlatable).
fn newest_per_series(
    conn: &Connection,
    now: DateTime<Utc>,
) -> Result<Vec<RecurrenceParent>, SweepError> {
    let now_str = now.to_rfc3339();
    // Pick the row with the largest `seq` per series_id-or-id key.
    let mut stmt = conn.prepare(
        "SELECT id, process_after, recurrence, series_id, content, seq,
                COALESCE(series_id, id) AS series_key
         FROM messages_in
         WHERE recurrence IS NOT NULL AND TRIM(recurrence) != ''
         ORDER BY series_key, seq DESC",
    )?;
    let mut rows = stmt.query([])?;

    let mut seen_keys = std::collections::HashSet::new();
    let mut parents = Vec::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get("series_key")?;
        if !seen_keys.insert(key) {
            continue;
        }
        let process_after_str: Option<String> = row.get("process_after")?;
        let process_after = match process_after_str.as_deref() {
            None => None,
            Some(s) => Some(
                DateTime::parse_from_rfc3339(s)
                    .map_err(|e| SweepError::ScheduleParse(e.to_string()))?
                    .with_timezone(&Utc),
            ),
        };
        // Only forward the parent if it is at-or-past its own fire time.
        if let Some(pa) = process_after {
            if pa.to_rfc3339() > now_str {
                continue;
            }
        }

        let id_str: String = row.get("id")?;
        let id_uuid = uuid::Uuid::parse_str(&id_str)
            .map_err(|e| SweepError::ScheduleParse(format!("bad uuid {id_str}: {e}")))?;
        let recurrence: String = row.get("recurrence")?;
        let series_id: Option<String> = row.get("series_id")?;
        let content_str: String = row.get("content")?;
        let content: serde_json::Value = serde_json::from_str(&content_str)
            .map_err(|e| SweepError::ScheduleParse(format!("bad content json: {e}")))?;

        parents.push(RecurrenceParent {
            id: MessageId::from(id_uuid),
            recurrence,
            series_id,
            content,
        });
    }
    Ok(parents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemSessionRoot, insert_recurring_inbound, seed_running_session};
    use chrono::{Duration as ChDuration, TimeZone};
    use copperclaw_db::tables::messages_in::{WriteInbound, insert};
    use copperclaw_db::tables::tasks::TaskStatus;
    use copperclaw_types::MessageKind;

    fn fixture() -> (
        CentralDb,
        MemSessionRoot,
        copperclaw_types::Session,
        DateTime<Utc>,
    ) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let sess = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        (central, root, sess, now)
    }

    /// Read the `recurrence` column of a `messages_in` row back out.
    fn row_recurrence(
        root: &MemSessionRoot,
        sess: &copperclaw_types::Session,
        id: MessageId,
    ) -> Option<String> {
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        pool.conn_mut()
            .query_row(
                "SELECT recurrence FROM messages_in WHERE id = ?1",
                rusqlite::params![id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn empty_messages_in_returns_empty() {
        let (central, root, sess, now) = fixture();
        let _ = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
        assert!(
            tasks::list_for_session(&central, sess.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parent_in_the_future_does_not_migrate() {
        let (central, root, sess, now) = fixture();
        let parent = insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-a".into()),
            Some(now + ChDuration::hours(1)),
        );
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
        // No task, and the source row keeps its recurrence for a later pass.
        assert!(
            tasks::list_for_session(&central, sess.id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            row_recurrence(&root, &sess, parent).as_deref(),
            Some("0 */2 * * *"),
        );
    }

    /// The A5 acceptance unit: a due per-session recurrence is created as a
    /// central `tasks` row instead of self-replicating a `messages_in` row.
    #[test]
    fn due_recurrence_is_created_as_a_task() {
        let (central, root, sess, now) = fixture();
        let parent = insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-a".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].series_id, "series-a");
        assert_eq!(r[0].new_message_id, parent);
        assert!(r[0].next_fire > now);

        // Exactly one task row, active, recurring, next_fire in the future.
        let tasks_now = tasks::list_for_session(&central, sess.id).unwrap();
        assert_eq!(tasks_now.len(), 1);
        let t = &tasks_now[0];
        assert_eq!(t.status, TaskStatus::Active);
        assert_eq!(t.recurrence.as_deref(), Some("0 */2 * * *"));
        assert_eq!(t.prompt, "recurring");
        assert!(t.next_fire.unwrap() > now);

        // And the legacy path is neutralised: the source row no longer recurs.
        assert_eq!(row_recurrence(&root, &sess, parent), None);
    }

    #[test]
    fn null_series_id_keys_task_on_parent_id() {
        let (central, root, sess, now) = fixture();
        let parent = insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            None,
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].series_id, parent.as_uuid().to_string());
        assert_eq!(tasks::list_for_session(&central, sess.id).unwrap().len(), 1);
    }

    #[test]
    fn only_newest_member_of_series_migrates_once() {
        let (central, root, sess, now) = fixture();
        // Two members of the same series; a single task is created.
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-b".into()),
            Some(now - ChDuration::hours(4)),
        );
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-b".into()),
            Some(now - ChDuration::minutes(1)),
        );
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].series_id, "series-b");
        assert_eq!(tasks::list_for_session(&central, sess.id).unwrap().len(), 1);
    }

    #[test]
    fn migration_is_idempotent_across_passes() {
        let (central, root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-idem".into()),
            Some(now - ChDuration::minutes(5)),
        );
        // First pass migrates; second pass is a no-op (series already cleared).
        let r1 = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r1.len(), 1);
        let r2 = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r2.is_empty());
        assert_eq!(tasks::list_for_session(&central, sess.id).unwrap().len(), 1);
    }

    #[test]
    fn cancelled_task_is_not_resurrected() {
        let (central, root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-cancel".into()),
            Some(now - ChDuration::minutes(5)),
        );
        check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        let id = migrated_task_id(&sess.id, "series-cancel");
        tasks::set_status(&central, &id, TaskStatus::Cancelled).unwrap();

        // A fresh recurring row for the SAME series must not recreate the task
        // an operator cancelled — the deterministic id + presence check guard it.
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-cancel".into()),
            Some(now - ChDuration::minutes(1)),
        );
        check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        let t = tasks::get(&central, &id).unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Cancelled);
        assert_eq!(tasks::list_for_session(&central, sess.id).unwrap().len(), 1);
    }

    #[test]
    fn empty_recurrence_string_is_ignored() {
        let (central, root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "",
            Some("series-empty".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
        assert!(
            tasks::list_for_session(&central, sess.id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unparseable_recurrence_is_skipped_without_error() {
        let (central, root, sess, now) = fixture();
        let parent = insert_recurring_inbound(
            &root,
            &sess,
            "not a valid cron",
            Some("series-bad".into()),
            Some(now - ChDuration::minutes(5)),
        );
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert!(r.is_empty());
        assert!(
            tasks::list_for_session(&central, sess.id)
                .unwrap()
                .is_empty()
        );
        // Unparseable rows are left intact (not cleared) so an out-of-band fix
        // can still migrate them later.
        assert_eq!(
            row_recurrence(&root, &sess, parent).as_deref(),
            Some("not a valid cron"),
        );
    }

    #[test]
    fn task_prompt_falls_back_to_raw_content_without_text() {
        let (central, root, sess, now) = fixture();
        // Hand-rolled insert with a content shape that has no `text` field.
        let id = MessageId::new();
        let write = WriteInbound {
            id,
            kind: MessageKind::Task,
            timestamp: now,
            content: serde_json::json!({"payload": 7}),
            trigger: true,
            on_wake: false,
            process_after: Some(now - ChDuration::minutes(5)),
            recurrence: Some("0 */2 * * *".into()),
            series_id: Some("series-raw".into()),
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        };
        {
            let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
            insert(pool.conn_mut(), &write).unwrap();
        }
        check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        let t = &tasks::list_for_session(&central, sess.id).unwrap()[0];
        assert!(t.prompt.contains("payload"));
    }

    #[test]
    fn no_process_after_still_migrates() {
        // A recurring row that has never fired (process_after IS NULL) should
        // migrate immediately.
        let (central, root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            "0 */2 * * *",
            Some("series-null-pa".into()),
            None,
        );
        let r = check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(tasks::list_for_session(&central, sess.id).unwrap().len(), 1);
    }

    /// A5 acceptance integration (DB/sweep layer): a recurring session, once
    /// consolidated, is managed through the same central-task lifecycle helpers
    /// that back the `list_tasks` / `pause_task` MCP tools. Pausing the migrated
    /// task stops the central scheduler from firing it.
    #[test]
    fn migrated_recurrence_is_managed_via_list_and_pause() {
        use crate::checks::scheduling;
        let (central, root, sess, now) = fixture();
        insert_recurring_inbound(
            &root,
            &sess,
            // Every-2-hours cron whose next occurrence is at/after `now`.
            "0 */2 * * *",
            Some("series-managed".into()),
            Some(now - ChDuration::minutes(5)),
        );
        // Consolidate the per-session recurrence into a central task.
        check(&central, &root, &sess.agent_group_id, &sess.id, now).unwrap();

        // `list_tasks` reads `tasks::list_for_session`: the recurrence now shows
        // up as a first-class, active, recurring task.
        let listed = tasks::list_for_session(&central, sess.id).unwrap();
        assert_eq!(listed.len(), 1);
        let task_id = listed[0].id.clone();
        assert_eq!(listed[0].status, TaskStatus::Active);
        assert!(listed[0].recurrence.is_some());

        // Force the task due, then `pause_task` it (== `tasks::set_status`
        // Paused). A paused task is excluded from the central scheduler's
        // due-list, so no wake inbound is fired.
        tasks::set_next_fire(&central, &task_id, Some(now - ChDuration::seconds(1))).unwrap();
        tasks::set_status(&central, &task_id, TaskStatus::Paused).unwrap();
        let fired = scheduling::check(&central, &root, now).unwrap();
        assert!(
            fired.is_empty(),
            "a paused recurring task must not fire via the central scheduler",
        );

        // Resume it and confirm the central scheduler now drives it.
        tasks::set_status(&central, &task_id, TaskStatus::Active).unwrap();
        tasks::set_next_fire(&central, &task_id, Some(now - ChDuration::seconds(1))).unwrap();
        let fired = scheduling::check(&central, &root, now).unwrap();
        assert_eq!(fired.len(), 1, "an active recurring task fires centrally");
        assert_eq!(fired[0].series_id, task_id);
    }
}
