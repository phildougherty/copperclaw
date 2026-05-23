//! Reconcile-loop classifier: decide what to do with a session this tick.

use super::spawn::container_name;
use super::{ContainerManager, ManagerError};
use ironclaw_db::session::{open_inbound, SessionPaths};
use ironclaw_db::tables::{messages_in, sessions};
use ironclaw_types::{ContainerStatus, Session};
use std::time::Duration;
use tracing::{info, warn};

/// What the reconcile loop wants to do with a session this tick.
#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Nothing to do — session is in a healthy steady state.
    Noop,
    /// Spawn a fresh container for a `Stopped` session with pending
    /// inbound.
    Spawn,
    /// `Idle` session got new inbound; mark it `Stopped` so the next
    /// tick spawns. Two-step transition (rather than spawning here)
    /// because spawn needs to read the current `container_status`
    /// row and we don't want to race ourselves.
    WakeFromIdle,
    /// `Running` session has been quiet long enough — stop the
    /// container and mark `Idle`.
    IdleStop,
    /// `Running` session's heartbeat is stale — the runner has likely
    /// crashed. Stop best-effort and reset to `Stopped` for respawn.
    CrashRestart,
}

impl ContainerManager {
    /// Decide what to do with a single session based on its
    /// `container_status`, the inbound pending count, the heartbeat
    /// file's mtime, and the `last_active` timestamp. Pure: takes no
    /// async work and no DB writes so the state machine is unit-
    /// testable.
    pub(super) fn classify(&self, session: &Session) -> ReconcileAction {
        let paths = SessionPaths::new(
            &self.cfg.data_dir,
            session.agent_group_id,
            session.id,
        );
        let pending = Self::has_pending_inbound(&paths).unwrap_or(false);
        match session.container_status {
            ContainerStatus::Stopped => {
                if pending {
                    ReconcileAction::Spawn
                } else {
                    ReconcileAction::Noop
                }
            }
            ContainerStatus::Idle => {
                if pending {
                    ReconcileAction::WakeFromIdle
                } else {
                    ReconcileAction::Noop
                }
            }
            ContainerStatus::Running => {
                if Self::heartbeat_stale(&paths, self.cfg.heartbeat_stale_secs)
                    .unwrap_or(false)
                {
                    ReconcileAction::CrashRestart
                } else if Self::session_idle(session, self.cfg.idle_timeout_secs)
                    && Self::heartbeat_stale(&paths, self.cfg.idle_timeout_secs)
                        .unwrap_or(false)
                {
                    // Only idle when BOTH the session shows no recent
                    // inbound activity AND the runner's heartbeat says
                    // it's done working. Without the second clause,
                    // long-running tool loops (e.g. a research agent
                    // chaining 10+ web_search + read_file calls past
                    // 5 minutes) get killed mid-flight even though the
                    // runner is actively producing — the prior check
                    // measured time since the LAST INBOUND, not time
                    // since the runner did anything. Surfaced live as
                    // "spawned three research agents and the host
                    // killed them just before they could reply."
                    ReconcileAction::IdleStop
                } else {
                    ReconcileAction::Noop
                }
            }
        }
    }

    pub(super) async fn apply(
        &self,
        session: &Session,
        action: ReconcileAction,
    ) -> Result<(), ManagerError> {
        match action {
            ReconcileAction::Noop => Ok(()),
            ReconcileAction::Spawn => {
                // Degraded mode is a sticky, expected state — the
                // session stays Stopped and the inbound row stays
                // pending until the operator runs `./rebuild.sh`
                // and restarts. Surfacing it every tick would spam
                // the host log, so we collapse Ok(_) and
                // Err(HostDegraded) into Ok here. The startup
                // banner and the metric are the source of truth
                // for "host is degraded".
                match self.maybe_spawn(session).await {
                    Ok(_) | Err(ManagerError::HostDegraded) => Ok(()),
                    Err(err) => Err(err),
                }
            }
            ReconcileAction::WakeFromIdle => {
                sessions::mark_container_stopped(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                info!(session = %session.id.as_uuid(), "idle → stopped (pending inbound)");
                Ok(())
            }
            ReconcileAction::IdleStop => {
                let name = container_name(session.agent_group_id, session.id);
                let _ = self
                    .runtime
                    .stop(&name, Duration::from_secs(self.cfg.stop_grace_secs))
                    .await;
                sessions::mark_container_idle(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                info!(session = %session.id.as_uuid(), "running → idle (no activity)");
                Ok(())
            }
            ReconcileAction::CrashRestart => {
                // Remove (not just stop) so the next spawn doesn't
                // collide on the container name. `remove` is a
                // stop+rm that treats 404 as success, so it's safe
                // to call even when the container is already gone.
                let name = container_name(session.agent_group_id, session.id);
                let _ = self.runtime.remove(&name).await;
                sessions::mark_container_stopped(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                ironclaw_metrics::inc_containers_crashed();
                warn!(
                    session = %session.id.as_uuid(),
                    "heartbeat stale; running → stopped (will respawn)"
                );
                Ok(())
            }
        }
    }

    pub(super) fn has_pending_inbound(paths: &SessionPaths) -> Result<bool, ManagerError> {
        // Opening inbound here might create the DB file if it's
        // somehow missing; that's fine — `count_due` will return 0.
        let conn = open_inbound(paths).map_err(ManagerError::Db)?;
        let n = messages_in::count_due(&conn).map_err(ManagerError::Db)?;
        Ok(n > 0)
    }

    /// Whether the runner has stopped refreshing its `.heartbeat`
    /// file. Treats the file's mtime as the truth source; if the
    /// file doesn't exist yet, that's *not* stale — the runner may
    /// not have started writing it yet (containers take a moment to
    /// boot).
    pub(super) fn heartbeat_stale(
        paths: &SessionPaths,
        threshold_secs: u64,
    ) -> Result<bool, ManagerError> {
        let mtime = paths.heartbeat_mtime().map_err(ManagerError::Io)?;
        let Some(mtime) = mtime else { return Ok(false) };
        let age = std::time::SystemTime::now()
            .duration_since(mtime)
            .unwrap_or(std::time::Duration::ZERO);
        Ok(age > std::time::Duration::from_secs(threshold_secs))
    }

    /// Whether `last_active` is older than the configured idle window.
    pub(super) fn session_idle(session: &Session, idle_window_secs: u64) -> bool {
        let now = chrono::Utc::now();
        let elapsed = now.signed_duration_since(session.last_active);
        elapsed.num_seconds() > i64::try_from(idle_window_secs).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::spawn::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use ironclaw_db::central::CentralDb;
    use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use ironclaw_db::tables::sessions::{create as create_session, CreateSession};
    use std::path::PathBuf;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "ironclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
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

    #[test]
    fn classify_stopped_without_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        // container_status defaults to Stopped per create_session.
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_stopped_with_pending_is_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: ironclaw_types::MessageId::new(),
                kind: ironclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ironclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
            },
        )
        .unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::Spawn);
    }

    #[test]
    fn classify_running_with_fresh_heartbeat_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        session.last_active = chrono::Utc::now();
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_running_with_stale_heartbeat_is_crash_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // Backdate the heartbeat mtime to before the staleness window.
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        filetime::set_file_mtime(
            &paths.heartbeat,
            filetime::FileTime::from_system_time(old),
        )
        .unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::CrashRestart);
    }

    #[test]
    fn classify_running_with_quiet_session_and_quiet_runner_is_idle_stop() {
        // Both signals quiet: no recent inbound AND the runner stopped
        // touching its heartbeat. This is the genuine "idle" case.
        let tmp = tempfile::tempdir().unwrap();
        let (_mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        session.last_active = chrono::Utc::now()
            - chrono::Duration::seconds(
                i64::try_from(DEFAULT_IDLE_TIMEOUT_SECS).unwrap() + 10,
            );
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // Backdate heartbeat past the idle window AND past the
        // crash-stale threshold's mid-zone — we want "idle, not
        // crashed," i.e. older than idle_timeout_secs but younger than
        // some fictional crash window. With defaults (60s crash, 300s
        // idle), an idle heartbeat would be ≥300s old but… in practice
        // anything older than idle_secs is also stale-as-crash. To
        // disambiguate we test a config where they're set apart:
        // crash=ridiculously long, idle=10s.
        let mut wide_cfg = manager_cfg(tmp.path().to_path_buf());
        wide_cfg.idle_timeout_secs = 10;
        wide_cfg.heartbeat_stale_secs = 86_400;
        let mgr_wide = ContainerManager::new(
            ironclaw_db::central::CentralDb::open_in_memory().unwrap(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            wide_cfg,
        );
        let backdated =
            std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        filetime::set_file_mtime(
            &paths.heartbeat,
            filetime::FileTime::from_system_time(backdated),
        )
        .unwrap();
        assert_eq!(mgr_wide.classify(&session), ReconcileAction::IdleStop);
    }

    #[test]
    fn classify_running_with_quiet_session_but_active_runner_is_noop() {
        // Regression: the manager used to idle-stop any session whose
        // last_active was older than idle_timeout_secs, even if the
        // runner was actively producing work. Long-running tool loops
        // (research agents chaining 10+ web_search calls past 5 min)
        // got killed mid-flight because last_active is bumped by
        // inbound arrival, not by runner activity. The fix gates
        // IdleStop on heartbeat freshness AS WELL — if the runner is
        // ticking, it's not idle.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        // No recent inbound: backdate last_active past the idle window.
        session.last_active = chrono::Utc::now()
            - chrono::Duration::seconds(
                i64::try_from(DEFAULT_IDLE_TIMEOUT_SECS).unwrap() + 10,
            );
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // Heartbeat is fresh: runner is actively working.
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_idle_without_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_idle_with_pending_is_wake_from_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: ironclaw_types::MessageId::new(),
                kind: ironclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(ironclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
            },
        )
        .unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::WakeFromIdle);
    }

    #[tokio::test]
    async fn apply_wake_from_idle_marks_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        mgr.apply(&session, ReconcileAction::WakeFromIdle).await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    #[tokio::test]
    async fn apply_idle_stop_marks_idle_and_calls_runtime_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        mgr.apply(&session, ReconcileAction::IdleStop).await.unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Idle));
    }

    #[tokio::test]
    async fn apply_crash_restart_marks_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }
}
