//! Scheduled-task fan-out.
//!
//! Scans the central `tasks` table for `active` tasks whose `next_fire`
//! has elapsed and, for each, synthesises a `kind: task` inbound message
//! into the originating session's `inbound.db`. The wake check (see
//! `wake.rs`) picks up the new pending row on the next tick and
//! transitions the session container back to `running`.
//!
//! Recurring tasks (`recurrence IS NOT NULL`) are re-armed by bumping
//! their `next_fire` to the next computed occurrence. One-shot tasks
//! (no recurrence) transition to `status = 'completed'`.
//!
//! Cancelled, paused, and completed tasks are filtered out by the SQL
//! `WHERE status = 'active'` predicate in `tasks::list_due`.

use crate::error::SweepError;
use crate::service::{SessionRoot, SeriesFanout};
use chrono::{DateTime, Utc};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::messages_in::{insert as insert_in, WriteInbound};
use ironclaw_db::tables::tasks::{self, TaskStatus};
use ironclaw_modules::scheduling::{compute_next_fire, parse_when, When};
use ironclaw_types::{MessageId, MessageKind};

/// Run one sweep over `tasks`. For every row with `status='active'` and
/// `next_fire <= now`, synthesise an inbound `kind: task` message into
/// the originating session and either re-arm (recurring) or mark
/// completed (one-shot).
///
/// Returns one [`SeriesFanout`] per task that fired during this pass.
/// The series id is the task id so operators can correlate fires back
/// to their source row.
pub fn check(
    central: &CentralDb,
    root: &dyn SessionRoot,
    now: DateTime<Utc>,
) -> Result<Vec<SeriesFanout>, SweepError> {
    let due = tasks::list_due(central, now)?;
    let mut out = Vec::with_capacity(due.len());
    for task in due {
        // Build the inbound row.
        let msg_id = MessageId::new();
        let write = WriteInbound {
            id: msg_id,
            kind: MessageKind::Task,
            timestamp: now,
            content: serde_json::json!({
                "text": task.prompt,
                "task_id": task.id,
                "task_name": task.name,
            }),
            trigger: true,
            on_wake: true,
            process_after: None,
            recurrence: None,
            series_id: Some(task.id.clone()),
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id: None,
        };
        let mut inbound = root.inbound_pool(&task.agent_group_id, &task.session_id)?;
        insert_in(inbound.conn_mut(), &write)?;

        // Re-arm or complete. Recurring tasks always re-arm using the
        // recurrence expression; one-shot tasks transition to
        // `completed` and clear `next_fire` so they never fire again.
        let next_fire =
            task.recurrence
                .as_deref()
                .and_then(|rec| next_fire_for(&task.when_spec, Some(rec), now));
        if let Some(next) = next_fire {
            tasks::set_next_fire(central, &task.id, Some(next))?;
        } else {
            tasks::set_status(central, &task.id, TaskStatus::Completed)?;
            tasks::set_next_fire(central, &task.id, None)?;
        }

        out.push(SeriesFanout {
            series_id: task.id,
            new_message_id: msg_id,
            next_fire: now,
        });
    }
    Ok(out)
}

/// Compute the next fire time for a task given its `when_spec` /
/// `recurrence`. Returns `None` if neither yields a future occurrence.
fn next_fire_for(
    when_spec: &str,
    recurrence: Option<&str>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    // Parse `when_spec` first; on parse failure, fall back to recurrence
    // alone (no `When` literal needed for a pure cron-style task).
    let parsed = parse_when(when_spec).ok();
    if let Some(when) = parsed.as_ref() {
        if let Some(next) = compute_next_fire(when, now, recurrence) {
            return Some(next);
        }
    }
    // No parse / past one-shot — try recurrence alone via a stub `When::At(now)`.
    if let Some(rec) = recurrence {
        if !rec.trim().is_empty() {
            return compute_next_fire(&When::At(now), now, Some(rec));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{seed_running_session, MemSessionRoot};
    use chrono::{Duration as ChDuration, TimeZone};
    use ironclaw_db::central::CentralDb;
    use ironclaw_db::tables::tasks::NewTask;

    fn fixture() -> (CentralDb, MemSessionRoot, ironclaw_types::Session, DateTime<Utc>) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        (central, root, session, now)
    }

    fn count_inbound(root: &MemSessionRoot, session: &ironclaw_types::Session) -> i64 {
        let mut pool = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        pool.conn_mut()
            .query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn no_due_tasks_returns_empty() {
        let (central, root, _sess, now) = fixture();
        let r = check(&central, &root, now).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn due_task_fires_wake_inbound() {
        let (central, root, sess, now) = fixture();
        tasks::insert(
            &central,
            NewTask {
                id: "t-1".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: Some("ping".into()),
                prompt: "wake up".into(),
                when_spec: "in 1s".into(),
                recurrence: None,
                next_fire: Some(now - ChDuration::seconds(1)),
            },
        )
        .unwrap();
        let r = check(&central, &root, now).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].series_id, "t-1");
        assert_eq!(count_inbound(&root, &sess), 1);
    }

    #[test]
    fn fired_inbound_has_task_kind_and_on_wake() {
        let (central, root, sess, now) = fixture();
        tasks::insert(
            &central,
            NewTask {
                id: "t-1".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: None,
                prompt: "do".into(),
                when_spec: "in 1s".into(),
                recurrence: None,
                next_fire: Some(now - ChDuration::seconds(1)),
            },
        )
        .unwrap();
        check(&central, &root, now).unwrap();
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let (kind, on_wake): (String, i64) = pool
            .conn_mut()
            .query_row(
                "SELECT kind, on_wake FROM messages_in LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "task");
        assert_eq!(on_wake, 1);
    }

    #[test]
    fn recurring_task_reschedules_after_firing() {
        let (central, root, sess, now) = fixture();
        tasks::insert(
            &central,
            NewTask {
                id: "t-recur".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: None,
                prompt: "daily ping".into(),
                when_spec: "daily at 09:00".into(),
                recurrence: Some("0 9 * * *".into()),
                next_fire: Some(now - ChDuration::minutes(1)),
            },
        )
        .unwrap();
        check(&central, &root, now).unwrap();
        let t = tasks::get(&central, "t-recur").unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Active);
        // next_fire should have been bumped to a future time.
        assert!(t.next_fire.unwrap() > now);
    }

    #[test]
    fn one_shot_task_marks_completed_after_firing() {
        let (central, root, sess, now) = fixture();
        tasks::insert(
            &central,
            NewTask {
                id: "t-once".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: None,
                prompt: "once".into(),
                when_spec: "in 1s".into(),
                recurrence: None,
                next_fire: Some(now - ChDuration::seconds(1)),
            },
        )
        .unwrap();
        check(&central, &root, now).unwrap();
        let t = tasks::get(&central, "t-once").unwrap().unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert!(t.next_fire.is_none());
    }

    #[test]
    fn cancelled_task_does_not_fire() {
        let (central, root, sess, now) = fixture();
        tasks::insert(
            &central,
            NewTask {
                id: "t-cancel".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: None,
                prompt: "x".into(),
                when_spec: "in 1s".into(),
                recurrence: None,
                next_fire: Some(now - ChDuration::seconds(1)),
            },
        )
        .unwrap();
        tasks::set_status(&central, "t-cancel", TaskStatus::Cancelled).unwrap();
        let r = check(&central, &root, now).unwrap();
        assert!(r.is_empty());
        assert_eq!(count_inbound(&root, &sess), 0);
    }

    #[test]
    fn paused_task_does_not_fire() {
        let (central, root, sess, now) = fixture();
        tasks::insert(
            &central,
            NewTask {
                id: "t-pause".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: None,
                prompt: "x".into(),
                when_spec: "in 1s".into(),
                recurrence: None,
                next_fire: Some(now - ChDuration::seconds(1)),
            },
        )
        .unwrap();
        tasks::set_status(&central, "t-pause", TaskStatus::Paused).unwrap();
        let r = check(&central, &root, now).unwrap();
        assert!(r.is_empty());
        assert_eq!(count_inbound(&root, &sess), 0);
    }

    #[test]
    fn future_task_does_not_fire() {
        let (central, root, sess, now) = fixture();
        tasks::insert(
            &central,
            NewTask {
                id: "t-future".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: None,
                prompt: "x".into(),
                when_spec: "in 1h".into(),
                recurrence: None,
                next_fire: Some(now + ChDuration::hours(1)),
            },
        )
        .unwrap();
        let r = check(&central, &root, now).unwrap();
        assert!(r.is_empty());
        assert_eq!(count_inbound(&root, &sess), 0);
    }

    #[test]
    fn next_fire_for_one_shot_in_past_returns_none() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        // `daily at` always yields a future occurrence even after a past one,
        // so use a literal past timestamp.
        assert!(next_fire_for("2020-01-01T00:00:00Z", None, now).is_none());
    }

    #[test]
    fn next_fire_for_cron_recurrence_returns_future() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        let next = next_fire_for("2020-01-01T00:00:00Z", Some("0 9 * * *"), now).unwrap();
        assert!(next > now);
    }
}
