//! Goal check-in fan-out (M22 A3).
//!
//! Scans the central `goals` table for `active` goals whose `next_checkin` has
//! elapsed and, for each, synthesises a `kind: task` inbound into the goal's
//! session's `inbound.db` — the SAME fan-out scheduled tasks use
//! (`checks::scheduling`), so a goal wake reuses the scheduler path rather than
//! inventing a parallel wake mechanism. The wake check (`wake.rs`) picks up the
//! new pending row on the next tick and transitions the session container back
//! to `running`; the woken agent reports progress via the `update_goal` MCP
//! tool.
//!
//! Budget (decision (d)): before firing, the goal's remaining budget is read
//! via [`copperclaw_db::tables::goals::budget_remaining`], which draws on the
//! linked A1 grant when one is set. A goal whose budget is exhausted
//! (`Some(0)`) is PAUSED instead of woken — the grant is the authority, so an
//! exhausted / revoked / expired grant stops the goal from spending more —
//! and a later grant top-up + `resume` lets it continue.
//!
//! After a fire the goal's `next_checkin` is re-armed from `checkin_recurrence`
//! (a croner expression); a goal with no recurrence fires its check-in once and
//! then has `next_checkin` cleared (it stays `active` but quiet until an
//! explicit re-arm or an external driving task).

use crate::error::SweepError;
use crate::service::{SeriesFanout, SessionRoot};
use chrono::{DateTime, Utc};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::goals::{self, GoalStatus};
use copperclaw_db::tables::messages_in::{WriteInbound, insert as insert_in};
use copperclaw_modules::scheduling::{When, compute_next_fire};
use copperclaw_types::{MessageId, MessageKind};

/// Outcome of one goal check-in sweep pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GoalCheckinReport {
    /// One [`SeriesFanout`] per goal that fired a check-in this pass. The series
    /// id is the goal id so operators can correlate wakes to their goal.
    pub fired: Vec<SeriesFanout>,
    /// Goal ids paused this pass because their budget was exhausted.
    pub budget_paused: Vec<String>,
}

impl GoalCheckinReport {
    /// True when the pass did nothing observable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fired.is_empty() && self.budget_paused.is_empty()
    }
}

/// Run one sweep over `goals`. For every active goal whose `next_checkin` has
/// elapsed: gate on remaining budget (pause if exhausted), else synthesise a
/// `kind: task` check-in inbound, record the fire, and re-arm `next_checkin`.
pub fn check(
    central: &CentralDb,
    root: &dyn SessionRoot,
    now: DateTime<Utc>,
) -> Result<GoalCheckinReport, SweepError> {
    let due = goals::list_due_checkin(central, now)?;
    let mut report = GoalCheckinReport::default();
    for goal in due {
        // Budget gate (decision (d)): a goal whose budget authority is exhausted
        // is paused, not woken. `None` (unbounded) always has headroom.
        if matches!(goals::budget_remaining(central, &goal.id, now)?, Some(0)) {
            goals::set_status(central, &goal.id, GoalStatus::Paused)?;
            report.budget_paused.push(goal.id);
            continue;
        }

        // Build the check-in inbound. The prompt is the goal's own
        // `checkin_prompt` when set, else a synthesised one referencing the
        // objective so the agent knows to report progress.
        let prompt = goal.checkin_prompt.clone().unwrap_or_else(|| {
            format!(
                "Goal check-in — report progress on your objective and call `update_goal` \
                 to record it (or mark it completed/abandoned): {}",
                goal.objective
            )
        });
        let msg_id = MessageId::new();
        let write = WriteInbound {
            id: msg_id,
            kind: MessageKind::Task,
            timestamp: now,
            content: serde_json::json!({
                "text": prompt,
                "goal_id": goal.id,
                "goal_objective": goal.objective,
                "task_id": goal.task_id,
            }),
            trigger: true,
            on_wake: true,
            process_after: None,
            recurrence: None,
            // The goal id is the series id so wakes correlate to the goal.
            series_id: Some(goal.id.clone()),
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        };
        let mut inbound = root.inbound_pool(&goal.agent_group_id, &goal.session_id)?;
        insert_in(inbound.conn_mut(), &write)?;

        // Record the fire (durable: `last_checkin_at` + `checkin_count`) before
        // re-arming so each occurrence accrues one count.
        goals::mark_checkin(central, &goal.id, now)?;

        // Re-arm from the check-in recurrence, or clear when there is none.
        let next = goal
            .checkin_recurrence
            .as_deref()
            .filter(|rec| !rec.trim().is_empty())
            .and_then(|rec| compute_next_fire(&When::At(now), now, Some(rec)));
        goals::set_next_checkin(central, &goal.id, next)?;

        report.fired.push(SeriesFanout {
            series_id: goal.id,
            new_message_id: msg_id,
            next_fire: now,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MemSessionRoot, seed_running_session};
    use chrono::{Duration as ChDuration, TimeZone};
    use copperclaw_db::tables::goals::NewGoal;
    use copperclaw_db::tables::task_grants::{self, NewTaskGrant};
    use copperclaw_db::tables::tasks::{self, NewTask};

    fn fixture() -> (
        CentralDb,
        MemSessionRoot,
        copperclaw_types::Session,
        DateTime<Utc>,
    ) {
        let central = CentralDb::open_in_memory().unwrap();
        let root = MemSessionRoot::new();
        let session = seed_running_session(&central);
        let now = chrono::Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap();
        (central, root, session, now)
    }

    fn seed_goal(
        central: &CentralDb,
        sess: &copperclaw_types::Session,
        id: &str,
        next_checkin: Option<DateTime<Utc>>,
        recurrence: Option<&str>,
    ) {
        goals::insert(
            central,
            NewGoal {
                id: id.into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                objective: "keep the docs current".into(),
                task_id: None,
                grant_id: None,
                token_budget: None,
                checkin_recurrence: recurrence.map(str::to_owned),
                checkin_prompt: None,
                next_checkin,
            },
        )
        .unwrap();
    }

    fn count_inbound(root: &MemSessionRoot, session: &copperclaw_types::Session) -> i64 {
        let mut pool = root
            .inbound_pool(&session.agent_group_id, &session.id)
            .unwrap();
        pool.conn_mut()
            .query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn no_due_goals_returns_empty() {
        let (central, root, _sess, now) = fixture();
        assert!(check(&central, &root, now).unwrap().is_empty());
    }

    #[test]
    fn due_goal_fires_task_kind_wake_inbound() {
        let (central, root, sess, now) = fixture();
        seed_goal(
            &central,
            &sess,
            "g-1",
            Some(now - ChDuration::minutes(1)),
            Some("0 9 * * *"),
        );
        let report = check(&central, &root, now).unwrap();
        assert_eq!(report.fired.len(), 1);
        assert_eq!(report.fired[0].series_id, "g-1");
        assert_eq!(count_inbound(&root, &sess), 1);
        // The synthesised inbound is a `task`-kind on-wake row carrying goal_id.
        let mut pool = root.inbound_pool(&sess.agent_group_id, &sess.id).unwrap();
        let (kind, on_wake, content): (String, i64, String) = pool
            .conn_mut()
            .query_row(
                "SELECT kind, on_wake, content FROM messages_in LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "task");
        assert_eq!(on_wake, 1);
        assert!(
            content.contains("g-1"),
            "content carries goal_id: {content}"
        );
    }

    #[test]
    fn recurring_goal_rearms_next_checkin() {
        let (central, root, sess, now) = fixture();
        seed_goal(
            &central,
            &sess,
            "g-1",
            Some(now - ChDuration::minutes(1)),
            Some("0 9 * * *"),
        );
        check(&central, &root, now).unwrap();
        let g = goals::get(&central, "g-1").unwrap().unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.checkin_count, 1);
        assert!(g.next_checkin.unwrap() > now, "re-armed into the future");
    }

    #[test]
    fn one_shot_checkin_clears_next_checkin_but_stays_active() {
        let (central, root, sess, now) = fixture();
        seed_goal(
            &central,
            &sess,
            "g-1",
            Some(now - ChDuration::minutes(1)),
            None,
        );
        check(&central, &root, now).unwrap();
        let g = goals::get(&central, "g-1").unwrap().unwrap();
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.checkin_count, 1);
        assert!(g.next_checkin.is_none(), "no recurrence → check-in cleared");
        // A second sweep does nothing (no longer due).
        assert!(
            check(&central, &root, now + ChDuration::hours(1))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paused_and_completed_goals_do_not_fire() {
        let (central, root, sess, now) = fixture();
        seed_goal(
            &central,
            &sess,
            "paused",
            Some(now - ChDuration::minutes(1)),
            None,
        );
        goals::set_status(&central, "paused", GoalStatus::Paused).unwrap();
        seed_goal(
            &central,
            &sess,
            "done",
            Some(now - ChDuration::minutes(1)),
            None,
        );
        goals::set_status(&central, "done", GoalStatus::Completed).unwrap();
        assert!(check(&central, &root, now).unwrap().is_empty());
        assert_eq!(count_inbound(&root, &sess), 0);
    }

    #[test]
    fn exhausted_grant_budget_pauses_goal_instead_of_firing() {
        let (central, root, sess, now) = fixture();
        // Task + grant with a tiny token budget, already fully consumed.
        tasks::insert(
            &central,
            NewTask {
                id: "t-1".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: Some("standup".into()),
                prompt: "post".into(),
                when_spec: "daily at 09:00".into(),
                recurrence: Some("0 9 * * *".into()),
                next_fire: Some(now + ChDuration::hours(1)),
            },
        )
        .unwrap();
        task_grants::insert_approved(
            &central,
            NewTaskGrant {
                id: "grant-1".into(),
                task_id: "t-1".into(),
                capability_scope: "send_message".into(),
                token_budget: Some(100),
                max_fires: Some(10),
                expires_at: Some(now + ChDuration::days(30)),
                granted_by: Some("op".into()),
            },
        )
        .unwrap();
        task_grants::consume_tokens(&central, "grant-1", 100, now).unwrap();
        goals::insert(
            &central,
            NewGoal {
                id: "g-1".into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                objective: "post standups".into(),
                task_id: Some("t-1".into()),
                grant_id: Some("grant-1".into()),
                token_budget: None,
                checkin_recurrence: Some("0 9 * * *".into()),
                checkin_prompt: None,
                next_checkin: Some(now - ChDuration::minutes(1)),
            },
        )
        .unwrap();
        let report = check(&central, &root, now).unwrap();
        assert!(report.fired.is_empty(), "exhausted budget → no wake");
        assert_eq!(report.budget_paused, vec!["g-1".to_string()]);
        assert_eq!(count_inbound(&root, &sess), 0);
        assert_eq!(
            goals::get(&central, "g-1").unwrap().unwrap().status,
            GoalStatus::Paused
        );
    }

    /// Acceptance (integration): a recurring goal reports progress across
    /// several wakes. Each sweep fires a check-in; between wakes the agent (here
    /// simulated) records progress, and the goal accrues check-in count +
    /// cumulative progress across the fires.
    #[test]
    fn multi_fire_goal_reports_progress_across_wakes() {
        let (central, root, sess, mut now) = fixture();
        seed_goal(
            &central,
            &sess,
            "g-1",
            Some(now - ChDuration::minutes(1)),
            Some("0 9 * * *"),
        );

        for i in 0..3 {
            // Fire this occurrence.
            let report = check(&central, &root, now).unwrap();
            assert_eq!(report.fired.len(), 1, "wake {i} fires exactly one check-in");
            // The woken agent reports progress (recorded against the goal).
            goals::record_progress(
                &central,
                &format!("p{i}"),
                "g-1",
                &format!("progress report {i}"),
                Some(50),
            )
            .unwrap();
            // Advance past the re-armed next_checkin for the next occurrence.
            let g = goals::get(&central, "g-1").unwrap().unwrap();
            now = g.next_checkin.unwrap() + ChDuration::seconds(1);
        }

        let g = goals::get(&central, "g-1").unwrap().unwrap();
        assert_eq!(g.checkin_count, 3, "three wakes across the run");
        assert_eq!(g.tokens_consumed, 150, "cumulative progress accrued");
        assert_eq!(goals::list_progress(&central, "g-1").unwrap().len(), 3);
        // Three synthesised check-in inbounds landed in the session.
        assert_eq!(count_inbound(&root, &sess), 3);
    }
}
