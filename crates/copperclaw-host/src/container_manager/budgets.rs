//! Per-group daily/turn budget gates and the in-channel notice replies.

use super::{ContainerManager, ManagerError};
use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
use copperclaw_types::{AgentGroupId, Session};
use tracing::{info, warn};

impl ContainerManager {
    /// Returns true when the group's `daily_token_cap` is set AND
    /// today's accumulated input + output tokens already meet or
    /// exceed it. Day boundary = UTC midnight, matching what an
    /// operator setting "daily" cap would naturally expect.
    pub(super) fn is_over_budget(&self, session: &Session) -> Result<bool, ManagerError> {
        use copperclaw_db::tables::{agent_turns, group_budgets};
        let Some(budget) =
            group_budgets::get(&self.central, session.agent_group_id).map_err(ManagerError::Db)?
        else {
            return Ok(false);
        };
        let Some(cap) = budget.daily_token_cap else {
            return Ok(false);
        };
        let midnight = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is a valid time")
            .and_utc();
        let used = agent_turns::tokens_since(
            &self.central,
            &session.agent_group_id.as_uuid().to_string(),
            midnight,
        )
        .map_err(ManagerError::Db)?;
        Ok(used >= cap)
    }

    /// Dedup window: if we already posted a budget-exhausted notice
    /// for this group inside the last hour, skip emitting another
    /// one. Picked to be long enough that a chatty user doesn't get
    /// repeated reminders but short enough that they get *some*
    /// follow-up if they're still chatting an hour later.
    const BUDGET_NOTICE_WINDOW_SECS: i64 = 3600;

    /// Dedup window for the rate-limit gate. Shorter than the budget
    /// window because the cap itself recovers on a minute / hour
    /// cadence — a user retrying after the cap clears should not be
    /// suppressed.
    const RATE_LIMIT_NOTICE_WINDOW_SECS: i64 = 60;

    /// When the budget gate refuses to spawn, post an in-channel
    /// reply telling the user the cap is hit and when it resets.
    /// Dedups per agent-group via [`Self::last_budget_notice`]; logs
    /// + swallows errors so the gate stays the source of truth.
    ///
    /// The reply is routed via `session_routing` (the
    /// `(channel_type, platform_id, thread_id)` the router stored on
    /// session create) so the user sees it back on the channel that
    /// asked the question.
    pub(super) fn maybe_post_budget_exhausted(
        &self,
        session: &Session,
        paths: &SessionPaths,
    ) -> Result<(), ManagerError> {
        // Compute next midnight UTC so the reply tells the user when
        // the cap resets.
        let now = chrono::Utc::now();
        let next_reset = (now.date_naive() + chrono::Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is always valid")
            .and_utc();
        let text = format!(
            "I have reached this agent's daily token budget. New requests will resume after {} UTC. \
Operators can raise the cap with `cclaw groups budget set --agent-group-id <id> --daily-tokens N`.",
            next_reset.format("%Y-%m-%d %H:%M"),
        );
        self.post_cap_reply(
            session,
            paths,
            &text,
            &self.last_budget_notice,
            Self::BUDGET_NOTICE_WINDOW_SECS,
            "budget-exhausted",
        )
    }

    /// Returns `Some((notification_text, gate_label))` when a per-minute
    /// or per-hour LLM rate cap has been reached, `None` when both caps
    /// are clear (or unset). Used by the spawn gate to short-circuit
    /// before calling the runtime and to derive the message for the
    /// in-channel notification. The `gate_label` is one of
    /// `copperclaw_metrics::BUDGET_GATE_TURNS_PER_MINUTE` or
    /// `..._TURNS_PER_HOUR`; callers pipe it straight into
    /// `copperclaw_metrics::inc_budget_exhausted` for the `gate` label.
    pub(super) fn rate_limit_message(
        &self,
        session: &Session,
    ) -> Result<Option<(String, &'static str)>, ManagerError> {
        use copperclaw_db::tables::{agent_turns, group_budgets};
        let Some(budget) =
            group_budgets::get(&self.central, session.agent_group_id).map_err(ManagerError::Db)?
        else {
            return Ok(None);
        };
        let ag_id = session.agent_group_id.as_uuid().to_string();
        let now = chrono::Utc::now();

        if let Some(cap) = budget.agent_turns_per_minute_cap {
            let since = now - chrono::Duration::seconds(60);
            let count =
                agent_turns::turns_since(&self.central, &ag_id, since).map_err(ManagerError::Db)?;
            if count >= cap {
                return Ok(Some((
                    format!(
                        "Per-minute LLM rate limit reached for this agent \
                         ({count} calls in the last minute, cap is {cap}). \
                         New requests resume within a minute. \
                         Operators can raise the cap with `cclaw groups budget set --agent-group-id <id> --turns-per-minute N`."
                    ),
                    copperclaw_metrics::BUDGET_GATE_TURNS_PER_MINUTE,
                )));
            }
        }

        if let Some(cap) = budget.agent_turns_per_hour_cap {
            let since = now - chrono::Duration::seconds(3600);
            let count =
                agent_turns::turns_since(&self.central, &ag_id, since).map_err(ManagerError::Db)?;
            if count >= cap {
                return Ok(Some((
                    format!(
                        "Per-hour LLM rate limit reached for this agent \
                         ({count} calls in the last hour, cap is {cap}). \
                         New requests resume within the hour. \
                         Operators can raise the cap with `cclaw groups budget set --agent-group-id <id> --turns-per-hour N`."
                    ),
                    copperclaw_metrics::BUDGET_GATE_TURNS_PER_HOUR,
                )));
            }
        }

        Ok(None)
    }

    /// Same dispatch path as the budget-exhausted reply, but with the
    /// dedup map keyed off [`Self::rate_limit_notified`] and a shorter
    /// window so a user retrying after the cap clears isn't silenced.
    pub(super) fn maybe_post_rate_limit_reply(
        &self,
        session: &Session,
        paths: &SessionPaths,
        text: &str,
    ) -> Result<(), ManagerError> {
        self.post_cap_reply(
            session,
            paths,
            text,
            &self.rate_limit_notified,
            Self::RATE_LIMIT_NOTICE_WINDOW_SECS,
            "rate-limit",
        )
    }

    /// Shared body for the cap-reply paths. Holds the dedup mutex
    /// only across the lookup + insert, then writes a Chat-kind
    /// outbound row routed via `session_routing` so the delivery
    /// loop dispatches it through the channel adapter.
    ///
    /// Bumps `copperclaw_budget_exhausted_suppressed_total` when the
    /// dedup window swallows the reply, and
    /// `copperclaw_budget_exhausted_replies_total` when a reply is
    /// actually written to outbound. The "no routing target" branch
    /// does NOT increment the reply counter — nothing was sent.
    #[allow(clippy::unused_self)] // kept as a method so callers can use `self.dispatch_cap_reply(...)`.
    fn post_cap_reply(
        &self,
        session: &Session,
        paths: &SessionPaths,
        text: &str,
        dedup: &std::sync::Mutex<
            std::collections::HashMap<AgentGroupId, chrono::DateTime<chrono::Utc>>,
        >,
        window_secs: i64,
        label: &'static str,
    ) -> Result<(), ManagerError> {
        let ag_id_str = session.agent_group_id.as_uuid().to_string();
        let now = chrono::Utc::now();
        {
            let mut state = dedup.lock().expect("cap-reply dedup mutex poisoned");
            if let Some(prev) = state.get(&session.agent_group_id) {
                let elapsed = now.signed_duration_since(*prev).num_seconds();
                if elapsed.abs() < window_secs {
                    copperclaw_metrics::inc_budget_exhausted_suppressed(&ag_id_str);
                    return Ok(());
                }
            }
            state.insert(session.agent_group_id, now);
        }

        let routing = {
            let conn = open_inbound(paths).map_err(ManagerError::Db)?;
            copperclaw_db::tables::session_routing::read(&conn).map_err(ManagerError::Db)?
        };
        let Some(routing) = routing else {
            warn!(
                session = %session.id.as_uuid(),
                kind = label,
                "cap notice skipped: no session_routing target",
            );
            return Ok(());
        };

        let outbound = open_outbound(paths).map_err(ManagerError::Db)?;
        let row = copperclaw_db::tables::messages_out::WriteOutbound {
            id: copperclaw_types::MessageId::new(),
            in_reply_to: None,
            timestamp: now,
            deliver_after: None,
            recurrence: None,
            kind: copperclaw_types::MessageKind::Chat,
            platform_id: routing.platform_id.clone(),
            channel_type: routing.channel_type.clone(),
            thread_id: routing.thread_id.clone(),
            content: serde_json::json!({ "text": text }),
        };
        copperclaw_db::tables::messages_out::insert(&outbound, &row).map_err(ManagerError::Db)?;
        copperclaw_metrics::inc_budget_exhausted_reply(&ag_id_str);
        info!(
            session = %session.id.as_uuid(),
            agent_group = %session.agent_group_id.as_uuid(),
            channel_type = ?routing.channel_type,
            kind = label,
            "posted cap reply to original sender"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::spawn::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messages_in;
    use copperclaw_db::tables::sessions::{self, CreateSession, create as create_session};
    use copperclaw_types::{ContainerStatus, SessionId};
    use std::path::PathBuf;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: SkillsMode::Inline,
            gpu_passthrough: false,
            forward_env: Vec::new(),
            egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
        }
    }

    fn fixture_session(db: &CentralDb) -> Session {
        let ag = create_ag(
            db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        create_session(
            db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
    }

    fn make_mgr(tmp: &tempfile::TempDir) -> (ContainerManager, CentralDb) {
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        (mgr, db)
    }

    fn set_daily_cap(db: &CentralDb, ag: AgentGroupId, cap: i64) {
        copperclaw_db::tables::group_budgets::upsert(
            db,
            copperclaw_db::tables::group_budgets::UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: Some(cap),
                daily_cost_cap: None,
                agent_turns_per_minute_cap: None,
                agent_turns_per_hour_cap: None,
            },
        )
        .unwrap();
    }

    /// Upsert a `group_budgets` row with only the per-minute / per-hour
    /// rate caps set. Used by the rate-limit tests.
    fn set_rate_caps(
        db: &CentralDb,
        ag: AgentGroupId,
        per_min: Option<i64>,
        per_hour: Option<i64>,
    ) {
        use copperclaw_db::tables::group_budgets::{UpsertGroupBudget, upsert};
        upsert(
            db,
            UpsertGroupBudget {
                agent_group_id: ag,
                daily_token_cap: None,
                daily_cost_cap: None,
                agent_turns_per_minute_cap: per_min,
                agent_turns_per_hour_cap: per_hour,
            },
        )
        .unwrap();
    }

    /// Seed `count` recent `agent_turns` for the session's group so
    /// the rate-limit gate sees them. Each turn is timestamped within
    /// the last 5 seconds (well inside both the per-minute and
    /// per-hour windows).
    fn seed_turns(db: &CentralDb, ag: AgentGroupId, count: usize) {
        use copperclaw_db::tables::agent_turns::{NewAgentTurn, insert};
        let now = chrono::Utc::now();
        for i in 0..count {
            #[allow(clippy::cast_possible_wrap)]
            let seq = i as i64;
            insert(
                db,
                &NewAgentTurn {
                    session_id: "sess-test".into(),
                    agent_group_id: ag.as_uuid().to_string(),
                    seq,
                    model: "claude-sonnet-4-6".into(),
                    provider: "anthropic".into(),
                    input_tokens: 10,
                    output_tokens: 20,
                    started_at: now - chrono::Duration::seconds(5),
                    ended_at: now - chrono::Duration::seconds(1),
                    status: "ok".into(),
                    error: None,
                },
            )
            .unwrap();
        }
    }

    fn record_today_tokens(db: &CentralDb, ag: AgentGroupId, input: i64, output: i64) {
        copperclaw_db::tables::agent_turns::insert(
            db,
            &copperclaw_db::tables::agent_turns::NewAgentTurn {
                agent_group_id: ag.as_uuid().to_string(),
                session_id: SessionId(uuid::Uuid::new_v4()).as_uuid().to_string(),
                seq: 1,
                model: "stub".into(),
                provider: "stub".into(),
                started_at: chrono::Utc::now(),
                ended_at: chrono::Utc::now(),
                input_tokens: input,
                output_tokens: output,
                status: "ok".into(),
                error: None,
            },
        )
        .unwrap();
    }

    fn seed_routing(paths: &SessionPaths) {
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(paths).unwrap();
        copperclaw_db::tables::session_routing::write(
            &conn,
            &copperclaw_types::routing::SessionRouting {
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                platform_id: Some("stdin".into()),
                thread_id: None,
            },
        )
        .unwrap();
    }

    fn count_outbound_text_replies(paths: &SessionPaths) -> Vec<String> {
        let conn = open_outbound(paths).unwrap();
        let rows = copperclaw_db::tables::messages_out::list_due(&conn).unwrap();
        rows.into_iter()
            .filter_map(|r| {
                if matches!(r.kind, copperclaw_types::MessageKind::Chat) {
                    r.content
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect()
    }

    fn seed_pending_chat_inbound(paths: &SessionPaths) {
        let conn = open_inbound(paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn maybe_post_budget_exhausted_writes_reply_when_routing_known() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        seed_routing(&paths);

        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].contains("daily token budget"));
        assert!(replies[0].contains("cclaw groups budget set"));
    }

    #[test]
    fn maybe_post_budget_exhausted_dedups_within_window() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        seed_routing(&paths);

        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();
        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();
        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
    }

    #[test]
    fn maybe_post_budget_exhausted_skips_when_no_routing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let _ = open_inbound(&paths).unwrap();

        mgr.maybe_post_budget_exhausted(&session, &paths).unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert!(replies.is_empty());
    }

    #[tokio::test]
    async fn maybe_spawn_posts_one_reply_per_window_when_over_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_routing(&paths);
        seed_pending_chat_inbound(&paths);

        set_daily_cap(&db, session.agent_group_id, 100);
        record_today_tokens(&db, session.agent_group_id, 150, 50);

        let spawned1 = mgr.maybe_spawn(&session).await.unwrap();
        let spawned2 = mgr.maybe_spawn(&session).await.unwrap();
        assert!(!spawned1, "must not spawn when over budget");
        assert!(!spawned2);
        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
    }

    /// Render the Prometheus body for whichever recorder is active.
    /// Used by the budget-gate metric tests; pair with
    /// `metrics::with_local_recorder` to get isolated counter state.
    fn render_prometheus(handle: &metrics_exporter_prometheus::PrometheusHandle) -> String {
        handle.render()
    }

    /// End-to-end: drive `maybe_spawn` twice against an over-budget group
    /// and assert the three budget counters land at the expected totals.
    /// First call: refusal + reply (no dedup hit). Second call: refusal +
    /// dedup suppression. Total: 2 refusals, 1 reply, 1 suppression.
    ///
    /// Plain `#[test]` (not `#[tokio::test]`) so `with_local_recorder` can
    /// own the thread for the duration of the inner runtime's `block_on`.
    /// `#[tokio::test]` would already be driving a runtime on this thread
    /// and the inner `block_on` would panic.
    #[test]
    fn maybe_spawn_emits_budget_counters_for_daily_token_cap() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            // tokio runtime is already active (#[tokio::test]); spawn the
            // gate work on a fresh blocking task so the local recorder
            // remains in scope for the metric calls. Easier: use a
            // single-threaded async block_on inside the closure.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let tmp = tempfile::tempdir().unwrap();
                let db = CentralDb::open_in_memory().unwrap();
                let mgr = ContainerManager::new(
                    db.clone(),
                    std::sync::Arc::new(crate::tests::NoopRuntime::default()),
                    manager_cfg(tmp.path().to_path_buf()),
                );
                let session = fixture_session(&db);
                let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
                paths.ensure_dirs().unwrap();
                seed_routing(&paths);
                seed_pending_chat_inbound(&paths);
                set_daily_cap(&db, session.agent_group_id, 100);
                record_today_tokens(&db, session.agent_group_id, 150, 50);
                // Two refusals: the first writes a reply, the second is
                // dedup-suppressed inside the BUDGET_NOTICE_WINDOW_SECS
                // window.
                let _ = mgr.maybe_spawn(&session).await.unwrap();
                let _ = mgr.maybe_spawn(&session).await.unwrap();
            });
            render_prometheus(&handle)
        });

        // `copperclaw_budget_exhausted_total{gate=daily_tokens, ...} 2`
        assert!(
            body.contains(copperclaw_metrics::BUDGET_EXHAUSTED_TOTAL),
            "exhausted counter missing:\n{body}"
        );
        assert!(
            body.contains("gate=\"daily_tokens\""),
            "daily_tokens gate label missing:\n{body}"
        );
        assert!(
            find_counter_value(&body, copperclaw_metrics::BUDGET_EXHAUSTED_TOTAL) == Some(2),
            "expected exhausted_total=2, body:\n{body}"
        );
        assert!(
            find_counter_value(&body, copperclaw_metrics::BUDGET_EXHAUSTED_REPLIES_TOTAL)
                == Some(1),
            "expected replies_total=1, body:\n{body}"
        );
        assert!(
            find_counter_value(&body, copperclaw_metrics::BUDGET_EXHAUSTED_SUPPRESSED_TOTAL)
                == Some(1),
            "expected suppressed_total=1, body:\n{body}"
        );
    }

    /// Per-minute rate-limit gate fires `gate=turns_per_minute`. Plain
    /// `#[test]` for the same `with_local_recorder` reason as above.
    #[test]
    fn maybe_spawn_emits_turns_per_minute_gate_label() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let tmp = tempfile::tempdir().unwrap();
                let db = CentralDb::open_in_memory().unwrap();
                let mgr = ContainerManager::new(
                    db.clone(),
                    std::sync::Arc::new(crate::tests::NoopRuntime::default()),
                    manager_cfg(tmp.path().to_path_buf()),
                );
                let session = fixture_session(&db);
                let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
                paths.ensure_dirs().unwrap();
                seed_routing(&paths);
                seed_pending_chat_inbound(&paths);
                set_rate_caps(&db, session.agent_group_id, Some(1), None);
                seed_turns(&db, session.agent_group_id, 1);
                let _ = mgr.maybe_spawn(&session).await.unwrap();
            });
            render_prometheus(&handle)
        });
        assert!(
            body.contains("gate=\"turns_per_minute\""),
            "turns_per_minute gate label missing:\n{body}"
        );
        assert!(
            find_counter_value(&body, copperclaw_metrics::BUDGET_EXHAUSTED_TOTAL) == Some(1),
            "expected exhausted_total=1 for per-minute gate, body:\n{body}"
        );
    }

    /// Per-hour rate-limit gate fires `gate=turns_per_hour`. Plain
    /// `#[test]` for the same `with_local_recorder` reason as above.
    #[test]
    fn maybe_spawn_emits_turns_per_hour_gate_label() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let tmp = tempfile::tempdir().unwrap();
                let db = CentralDb::open_in_memory().unwrap();
                let mgr = ContainerManager::new(
                    db.clone(),
                    std::sync::Arc::new(crate::tests::NoopRuntime::default()),
                    manager_cfg(tmp.path().to_path_buf()),
                );
                let session = fixture_session(&db);
                let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
                paths.ensure_dirs().unwrap();
                seed_routing(&paths);
                seed_pending_chat_inbound(&paths);
                set_rate_caps(&db, session.agent_group_id, None, Some(1));
                seed_turns(&db, session.agent_group_id, 1);
                let _ = mgr.maybe_spawn(&session).await.unwrap();
            });
            render_prometheus(&handle)
        });
        assert!(
            body.contains("gate=\"turns_per_hour\""),
            "turns_per_hour gate label missing:\n{body}"
        );
    }

    /// Walk the Prometheus text body and return the integer value of the
    /// first sample whose name matches `metric_name`. Whitespace tolerant.
    /// Returns `None` if the metric isn't in the body.
    fn find_counter_value(body: &str, metric_name: &str) -> Option<u64> {
        // Prometheus text format: `<name>{<labels>} <value>` or `<name> <value>`.
        // We sum across all label combinations for the metric.
        let mut total: u64 = 0;
        let mut seen = false;
        for line in body.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            // Match either `name{...}` or `name ` exactly.
            let name_matches = line
                .strip_prefix(metric_name)
                .is_some_and(|rest| rest.starts_with('{') || rest.starts_with(' '));
            if !name_matches {
                continue;
            }
            // Value is the last whitespace-separated token.
            if let Some(value) = line.split_whitespace().last() {
                if let Ok(parsed) = value.parse::<f64>() {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let parsed_u = parsed as u64;
                    total += parsed_u;
                    seen = true;
                }
            }
        }
        if seen { Some(total) } else { None }
    }

    // ---- rate-limit gate (per-minute / per-hour) -------------------------

    #[test]
    fn rate_limit_message_none_when_caps_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        assert!(mgr.rate_limit_message(&session).unwrap().is_none());
        set_rate_caps(&db, session.agent_group_id, None, None);
        assert!(mgr.rate_limit_message(&session).unwrap().is_none());
    }

    #[test]
    fn rate_limit_message_fires_on_per_minute_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        set_rate_caps(&db, session.agent_group_id, Some(3), None);
        seed_turns(&db, session.agent_group_id, 2);
        assert!(mgr.rate_limit_message(&session).unwrap().is_none());
        seed_turns(&db, session.agent_group_id, 1);
        let (msg, gate) = mgr.rate_limit_message(&session).unwrap().unwrap();
        assert!(msg.contains("Per-minute"), "{msg}");
        assert!(msg.contains("cap is 3"), "{msg}");
        assert_eq!(gate, copperclaw_metrics::BUDGET_GATE_TURNS_PER_MINUTE);
    }

    #[test]
    fn rate_limit_message_fires_on_per_hour_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        set_rate_caps(&db, session.agent_group_id, None, Some(5));
        seed_turns(&db, session.agent_group_id, 5);
        let (msg, gate) = mgr.rate_limit_message(&session).unwrap().unwrap();
        assert!(msg.contains("Per-hour"), "{msg}");
        assert!(msg.contains("cap is 5"), "{msg}");
        assert_eq!(gate, copperclaw_metrics::BUDGET_GATE_TURNS_PER_HOUR);
    }

    #[tokio::test]
    async fn tick_refuses_spawn_when_per_minute_cap_reached() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_pending_chat_inbound(&paths);

        set_rate_caps(&db, session.agent_group_id, Some(1), None);
        seed_turns(&db, session.agent_group_id, 1);
        mgr.tick().await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
        assert!(runtime.spawn_calls().is_empty());
    }

    #[tokio::test]
    async fn tick_refuses_spawn_when_per_hour_cap_reached() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_pending_chat_inbound(&paths);

        set_rate_caps(&db, session.agent_group_id, None, Some(2));
        seed_turns(&db, session.agent_group_id, 2);
        mgr.tick().await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
        assert!(runtime.spawn_calls().is_empty());
    }

    #[test]
    fn rate_limit_dedup_within_window_emits_exactly_one_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        seed_routing(&paths);

        let text = "rate-limit reply text";
        mgr.maybe_post_rate_limit_reply(&session, &paths, text)
            .unwrap();
        mgr.maybe_post_rate_limit_reply(&session, &paths, text)
            .unwrap();
        mgr.maybe_post_rate_limit_reply(&session, &paths, text)
            .unwrap();

        let replies = count_outbound_text_replies(&paths);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0], text);
    }

    /// `enter_degraded_mode` writes an apology row to every session
    /// with a pending chat inbound, routed via that inbound's
    /// `(channel_type, platform_id, thread_id)`.
    #[test]
    fn degraded_mode_emits_apology_to_pending_inbounds() {
        use crate::image_health::{
            DEGRADED_APOLOGY_TEXT, HealthDegradedReason, enter_degraded_mode,
        };

        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();

        // Two sessions, each with a pending chat inbound. Use
        // distinct agent groups so the unique-folder constraint
        // doesn't trip.
        let ag1 = create_ag(
            &db,
            CreateAgentGroup {
                name: "deg-a".into(),
                folder: "deg-a".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let ag2 = create_ag(
            &db,
            CreateAgentGroup {
                name: "deg-b".into(),
                folder: "deg-b".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let s1 = create_session(
            &db,
            CreateSession {
                agent_group_id: ag1.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let s2 = create_session(
            &db,
            CreateSession {
                agent_group_id: ag2.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let paths1 = SessionPaths::new(tmp.path(), s1.agent_group_id, s1.id);
        let paths2 = SessionPaths::new(tmp.path(), s2.agent_group_id, s2.id);
        paths1.ensure_dirs().unwrap();
        paths2.ensure_dirs().unwrap();
        seed_pending_chat_inbound(&paths1);
        seed_pending_chat_inbound(&paths2);

        let reason = HealthDegradedReason::ImageNotFound {
            tag: "copperclaw/session:nope".into(),
        };
        let notified = enter_degraded_mode(&db, tmp.path(), &reason);
        assert_eq!(notified, 2, "expected 2 sessions notified, got {notified}");

        for paths in [&paths1, &paths2] {
            let replies = count_outbound_text_replies(paths);
            assert_eq!(
                replies.len(),
                1,
                "each session should get exactly one apology row, got {replies:?}"
            );
            let body = &replies[0];
            assert!(
                body.contains("temporarily degraded"),
                "apology text must mention degraded state: {body}"
            );
            // The exact text matches DEGRADED_APOLOGY_TEXT.
            assert_eq!(body.as_str(), DEGRADED_APOLOGY_TEXT);
        }
    }
}
