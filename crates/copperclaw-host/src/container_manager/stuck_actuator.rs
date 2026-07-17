//! M21 S2 (decision (a)): the container manager as the sweep's
//! [`StuckActuator`].
//!
//! The sweep detects tools past `ABSOLUTE_CEILING_MS` from per-session
//! DB state but owns no container lifecycle; the manager owns lifecycle
//! but cannot see a hung tool (the runner keeps its heartbeat fresh
//! across tool dispatch — deliberately, so long-but-legitimate tools
//! are not killed by the crash path). This impl is the seam between
//! them: the host injects the manager into the `SweepService` at boot
//! (`boot.rs`), and each ceiling detection lands here as a
//! [`ReconcileAction::StuckRestart`] applied through the manager's
//! normal `apply` path — single-writer ownership preserved.
//!
//! The restart re-verifies liveness first: the detection is a sweep
//! snapshot and the session may have stopped, idled, or been deleted
//! since. Anything but a `Running` container is a quiet no-op.

use super::{ContainerManager, ReconcileAction};
use copperclaw_db::DbError;
use copperclaw_db::tables::sessions;
use copperclaw_host_sweep::{ActuatorError, StuckActuator};
use copperclaw_types::{ContainerStatus, SessionId};
use tracing::{debug, info};

#[async_trait::async_trait]
impl StuckActuator for ContainerManager {
    async fn restart_stuck(&self, session_id: SessionId) -> Result<(), ActuatorError> {
        let session = match sessions::get(&self.central, session_id) {
            Ok(session) => session,
            Err(DbError::NotFound) => {
                // Deleted between detection and actuation — nothing to
                // restart, and nothing to report.
                debug!(
                    session = %session_id.as_uuid(),
                    "stuck restart skipped: session no longer exists"
                );
                return Ok(());
            }
            Err(err) => return Err(Box::new(err)),
        };
        if !matches!(session.container_status, ContainerStatus::Running) {
            // Already stopped or idled (a crash restart or idle-stop
            // beat us to it) — the stale tool-state row will clear when
            // the next runner writes fresh state; restarting a
            // not-running container would only burn a backoff step.
            debug!(
                session = %session_id.as_uuid(),
                status = ?session.container_status,
                "stuck restart skipped: container is not running"
            );
            return Ok(());
        }
        info!(
            session = %session_id.as_uuid(),
            "sweep reported tool past absolute ceiling; restarting container"
        );
        self.apply(&session, ReconcileAction::StuckRestart)
            .await
            .map_err(|err| Box::new(err) as ActuatorError)
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
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use copperclaw_db::tables::{container_state, messages_in, messages_out, processing_ack};
    use copperclaw_host_sweep::SweepService;
    use copperclaw_host_sweep::service::FilesystemSessionRoot;
    use copperclaw_types::{ChannelType, MessageId, MessageKind, Session};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: None,
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

    /// Seed one chat-routed inbound row and return its id.
    fn seed_routed_inbound(paths: &SessionPaths, text: &str) -> MessageId {
        let msg_id = MessageId::new();
        let conn = open_inbound(paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({ "text": text }),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("tg-42".into()),
                channel_type: Some(ChannelType::new("telegram")),
                thread_id: Some("thread-7".into()),
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        msg_id
    }

    /// Write a `container_state` row whose tool started `minutes_ago`
    /// minutes ago — past the 30-minute ceiling when `minutes_ago > 30`.
    fn seed_hung_tool(paths: &SessionPaths, minutes_ago: i64) {
        let now = chrono::Utc::now();
        let outbound = open_outbound(paths).unwrap();
        container_state::set(
            &outbound,
            &container_state::ContainerState {
                current_tool: Some("bash".into()),
                tool_declared_timeout_ms: Some(3_600_000),
                tool_started_at: Some(now - chrono::Duration::minutes(minutes_ago)),
                updated_at: Some(now),
            },
        )
        .unwrap();
    }

    fn count_chat_rows(outbound: &rusqlite::Connection) -> usize {
        messages_out::list_due(outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MessageKind::Chat)
            .count()
    }

    /// The S2 integration acceptance, end to end on the mock runtime:
    /// a tool hung past the ceiling, one sweep pass with the manager
    /// wired as actuator -> `StuckRestart` applied (session `Stopped`,
    /// tool state cleared), the crash-restart apology written once
    /// (deduped across a second pass), and the next inbound spawns
    /// normally once the S4 backoff elapses.
    #[tokio::test(start_paused = true)]
    async fn hung_tool_past_ceiling_is_restarted_within_one_sweep_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let central = CentralDb::open_in_memory().unwrap();
        let mgr = Arc::new(ContainerManager::new(
            central.clone(),
            Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        ));
        let mut session = fixture_session(&central);
        sessions::mark_container_running(&central, session.id).unwrap();
        session.container_status = ContainerStatus::Running;

        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        // Fresh heartbeat: the runner process is alive — exactly the
        // shape the crash path can never catch (decision (a)).
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // The in-flight inbound the hung tool was working on. Recent
        // timestamp so the sweep's own PendingTooLong apology stays
        // out — the crash-restart apology machinery is what must fire.
        let msg_id = seed_routed_inbound(&paths, "please build the thing");
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(
            &outbound,
            msg_id,
            processing_ack::ProcessingStatus::Processing,
        )
        .unwrap();
        // The hung tool: 31 minutes into a declared 1-hour timeout —
        // past the unconditional 30-minute ceiling.
        seed_hung_tool(&paths, 31);

        let sweep = SweepService::new(
            central.clone(),
            Arc::new(FilesystemSessionRoot::new(tmp.path())),
        );
        sweep.set_stuck_actuator(Arc::clone(&mgr) as Arc<dyn copperclaw_host_sweep::StuckActuator>);

        // One sweep pass: detection + actuation.
        let report = sweep.run_once_actuated().await.unwrap();
        assert_eq!(report.stuck_past_ceiling, vec![session.id]);

        // The restart landed: session Stopped, tool state cleared.
        let updated = sessions::get(&central, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
        let state = container_state::get(&outbound).unwrap().unwrap();
        assert!(state.current_tool.is_none(), "tool state must be cleared");
        assert!(state.tool_started_at.is_none());

        // Exactly one crash-restart apology, routed at the inbound.
        assert_eq!(count_chat_rows(&outbound), 1);
        let apology = messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .find(|r| r.kind == MessageKind::Chat)
            .unwrap();
        assert_eq!(apology.in_reply_to, Some(msg_id));
        let text = apology
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert!(
            text.contains("snag") && text.contains("restart"),
            "user hears the recovery, not silence: {text:?}",
        );

        // A second sweep pass is quiet: the cleared tool state means no
        // re-detection, and the Failed processing_ack means no second
        // apology even if a restart were re-requested.
        let report2 = sweep.run_once_actuated().await.unwrap();
        assert!(report2.stuck_past_ceiling.is_empty(), "no re-detection");
        assert_eq!(count_chat_rows(&outbound), 1, "apology stays deduped");

        // Next inbound processes normally: pending work spawns once the
        // S4 backoff (first step, 5s) elapses.
        let _next = seed_routed_inbound(&paths, "are you back?");
        let mut stopped = sessions::get(&central, session.id).unwrap();
        stopped.container_status = ContainerStatus::Stopped;
        assert_eq!(
            mgr.classify(&stopped),
            ReconcileAction::Noop,
            "inside the backoff window the respawn is deferred",
        );
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        assert_eq!(
            mgr.classify(&stopped),
            ReconcileAction::Spawn,
            "after the backoff the next inbound spawns normally",
        );
    }

    /// A stale detection against a session whose container is no longer
    /// running is a quiet no-op: no apology, no state change, no error.
    #[tokio::test]
    async fn restart_stuck_is_noop_when_container_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let central = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            central.clone(),
            Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let session = fixture_session(&central);
        // container_status defaults to Stopped.
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_hung_tool(&paths, 45);

        mgr.restart_stuck(session.id).await.unwrap();

        let updated = sessions::get(&central, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
        let outbound = open_outbound(&paths).unwrap();
        assert_eq!(count_chat_rows(&outbound), 0, "no apology for a no-op");
        // Tool state deliberately untouched — the next runner clears it.
        assert!(
            container_state::get(&outbound)
                .unwrap()
                .unwrap()
                .current_tool
                .is_some(),
        );
    }

    /// A session deleted between detection and actuation is also a
    /// quiet no-op rather than an error.
    #[tokio::test]
    async fn restart_stuck_is_noop_when_session_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let central = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            central,
            Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        mgr.restart_stuck(SessionId::new()).await.unwrap();
    }

    /// The backoff-participation decision, pinned: stuck restarts walk
    /// the same S4 curve as crashes, so a session whose tool wedges
    /// immediately after every respawn is respawned at increasing
    /// intervals (5s then 15s), never hot-looped on the sweep cadence.
    #[tokio::test(start_paused = true)]
    async fn repeated_stuck_restarts_escalate_the_backoff() {
        let tmp = tempfile::tempdir().unwrap();
        let central = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            central.clone(),
            Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let mut session = fixture_session(&central);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let _ = seed_routed_inbound(&paths, "task");

        // Stuck restart 1.
        sessions::mark_container_running(&central, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        seed_hung_tool(&paths, 31);
        mgr.restart_stuck(session.id).await.unwrap();
        session.container_status = ContainerStatus::Stopped;
        assert_eq!(
            mgr.classify(&session),
            ReconcileAction::Noop,
            "first stuck restart defers the respawn (5s window)",
        );
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        assert_eq!(mgr.classify(&session), ReconcileAction::Spawn);

        // Stuck restart 2: the window escalates to 15s.
        sessions::mark_container_running(&central, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        seed_hung_tool(&paths, 31);
        mgr.restart_stuck(session.id).await.unwrap();
        session.container_status = ContainerStatus::Stopped;
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        assert_eq!(
            mgr.classify(&session),
            ReconcileAction::Noop,
            "6s < the escalated 15s window: still deferred",
        );
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert_eq!(mgr.classify(&session), ReconcileAction::Spawn);
    }
}
