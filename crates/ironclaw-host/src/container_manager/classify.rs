//! Reconcile-loop classifier: decide what to do with a session this tick.

use super::spawn::container_name;
use super::{ContainerManager, ManagerError};
use ironclaw_db::session::{open_inbound, open_outbound, SessionPaths};
use ironclaw_db::tables::messages_out::{insert as insert_outbound, WriteOutbound};
use ironclaw_db::tables::processing_ack::{self, ProcessingStatus};
use ironclaw_db::tables::{messages_in, sessions};
use ironclaw_host_sweep::APOLOGY_TRIES_MARKER;
use ironclaw_types::{ChannelType, ContainerStatus, MessageId, MessageKind, Session};
use rusqlite::{params, OptionalExtension};
use std::time::Duration;
use tracing::{info, warn};

/// User-facing apology emitted by the crash-restart path when one or
/// more inbound messages were in-flight on the dying container. Kept
/// distinct from the host-sweep `pending_too_long` apology text so an
/// operator scanning chat logs can tell which path fired. The tone is
/// deliberately concrete ("restart the agent container") so the user
/// understands the bot didn't ghost them.
pub(crate) const CRASH_RESTART_APOLOGY_TEXT: &str =
    "Hit a snag mid-task and need to restart the agent container. \
     Some progress may have been lost. I'll pick back up — try sending \
     a follow-up if I don't continue on my own.";

/// How many tail lines of container stdout/stderr to capture in the
/// crash-log file. ~200 covers the typical panic + immediate context
/// without bloating the session dir on a busy box.
const CRASH_LOG_TAIL_LINES: u32 = 200;

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
                self.apply_crash_restart(session).await?;
                Ok(())
            }
        }
    }

    /// Body of [`ReconcileAction::CrashRestart`]. Captures container
    /// logs to a crash-log file, removes the container, emits a chat
    /// apology for every in-flight `processing_ack` row, marks those
    /// rows as Failed (so the host-sweep `processing` reset path
    /// doesn't double-fire), and stamps `messages_in.tries =
    /// APOLOGY_TRIES_MARKER` so the host-sweep `apology`
    /// `PendingTooLong` path also stays out. Finally marks the session
    /// container `Stopped` so the next reconcile tick respawns.
    ///
    /// Idempotent: a second pass finds no `processing_ack` rows in
    /// `processing` status (the first pass marked them `Failed`) and
    /// the inbound rows are at `tries=99`, so no duplicate apology is
    /// emitted.
    pub(super) async fn apply_crash_restart(
        &self,
        session: &Session,
    ) -> Result<(), ManagerError> {
        let name = container_name(session.agent_group_id, session.id);
        let paths = SessionPaths::new(
            &self.cfg.data_dir,
            session.agent_group_id,
            session.id,
        );

        // 1. Capture last few hundred lines of stdout/stderr BEFORE the
        //    container disappears. Best-effort: any failure here is
        //    non-fatal — operators still have host logs + the apology
        //    row even if we can't archive the runner's last words.
        capture_crash_log(&*self.runtime, &name, &paths).await;

        // 2. Remove (not just stop) so the next spawn doesn't collide
        //    on the container name. `remove` is a stop+rm that treats
        //    404 as success, so it's safe to call even when the
        //    container is already gone.
        let _ = self.runtime.remove(&name).await;

        // 3. Emit one chat apology per in-flight processing_ack row so
        //    the user knows the agent didn't ghost them. The host-sweep
        //    apology path used to be the only signal here, and waited up
        //    to APOLOGY_AFTER_SECS (5 min). The runner-restart path now
        //    fires the apology immediately.
        if let Err(err) = emit_crash_restart_apologies(&paths, session) {
            // Don't fail the whole tick on apology-emit failure — the
            // session still needs to be marked Stopped so the next
            // tick can respawn.
            warn!(
                session = %session.id.as_uuid(),
                ?err,
                "could not emit crash-restart apology rows"
            );
        }

        // 4. Existing behaviour: flip the session row + metric + warn.
        sessions::mark_container_stopped(&self.central, session.id)
            .map_err(ManagerError::Db)?;
        ironclaw_metrics::inc_containers_crashed();
        warn!(
            session = %session.id.as_uuid(),
            "heartbeat stale; running → stopped (will respawn)"
        );
        Ok(())
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

/// One in-flight inbound that needs a crash-restart apology. Pulled out
/// of the inbound DB by looking up the row matching a still-`processing`
/// `processing_ack` claim.
#[derive(Debug, Clone)]
struct InFlightRouting {
    message_id: MessageId,
    channel_type: ChannelType,
    platform_id: String,
    thread_id: Option<String>,
}

/// Capture the tail of the container's stdout/stderr to
/// `<session_root>/crash-<utc-rfc3339>.log`. Non-fatal — any failure is
/// logged at WARN and the caller continues.
///
/// The crash-log file goes in the session root dir (alongside
/// `runner.json`), NOT in `outbox/` (which is reserved for delivery
/// attachments).
async fn capture_crash_log(
    runtime: &dyn ironclaw_container_rt::ContainerRuntime,
    container_name: &str,
    paths: &SessionPaths,
) {
    // Path uses the UTC instant the host detected the crash so multiple
    // crash files don't clobber each other if a session crashes more
    // than once in a session's lifetime. RFC3339 with ':' replaced by
    // '-' keeps the filename portable across filesystems (Windows
    // doesn't allow ':').
    let now = chrono::Utc::now().to_rfc3339().replace(':', "-");
    let file_path = paths.root.join(format!("crash-{now}.log"));

    let body = match runtime.logs(container_name, CRASH_LOG_TAIL_LINES).await {
        Ok(body) => body,
        Err(err) => {
            warn!(
                container = container_name,
                ?err,
                "could not capture container logs before crash-restart removal"
            );
            return;
        }
    };

    // Empty body is legitimate (default-impl runtimes return ""); skip
    // writing the file in that case so we don't litter session dirs
    // with zero-byte placeholders.
    if body.is_empty() {
        return;
    }

    if let Err(err) = std::fs::create_dir_all(&paths.root) {
        warn!(
            path = %paths.root.display(),
            ?err,
            "could not ensure session root dir for crash log"
        );
        return;
    }

    if let Err(err) = std::fs::write(&file_path, body.as_bytes()) {
        warn!(
            path = %file_path.display(),
            ?err,
            "could not write crash log file"
        );
    }
}

/// Scan in-flight `processing_ack` claims, emit one chat apology per
/// row with usable channel routing, mark each claim as Failed, and
/// stamp `messages_in.tries = APOLOGY_TRIES_MARKER` on the matching
/// inbound row so the host-sweep `apology` path also stays out.
///
/// All effects are idempotent across reconcile-tick repeats: the
/// scan filters by `processing_ack.status='processing'`, which the
/// first pass updates to `Failed`. A second pass therefore sees no
/// rows and emits no apologies.
fn emit_crash_restart_apologies(
    paths: &SessionPaths,
    session: &Session,
) -> Result<(), ManagerError> {
    let inbound = open_inbound(paths).map_err(ManagerError::Db)?;
    let outbound = open_outbound(paths).map_err(ManagerError::Db)?;

    let processing = list_processing_acks(&outbound)?;
    if processing.is_empty() {
        info!(
            session = %session.id.as_uuid(),
            "crash-restart: no in-flight processing_ack rows; no apology emitted"
        );
        return Ok(());
    }

    let now = chrono::Utc::now();
    let mut emitted = 0u32;
    for message_id in processing {
        // Look up the inbound row's routing. If the row is missing or
        // lacks channel routing, fall through to the mark-as-Failed
        // step so we don't loop on the same row again.
        let routing = lookup_inbound_routing(&inbound, message_id)?;

        if let Some(routing) = routing {
            let apology = WriteOutbound {
                id: MessageId::new(),
                in_reply_to: Some(routing.message_id),
                timestamp: now,
                deliver_after: None,
                recurrence: None,
                kind: MessageKind::Chat,
                channel_type: Some(routing.channel_type.clone()),
                platform_id: Some(routing.platform_id.clone()),
                thread_id: routing.thread_id.clone(),
                content: serde_json::json!({ "text": CRASH_RESTART_APOLOGY_TEXT }),
            };
            insert_outbound(&outbound, &apology).map_err(ManagerError::Db)?;
            emitted += 1;
        }

        // Stamp the inbound row so the host-sweep apology path won't
        // also fire `pending_too_long` for it. We do this even when
        // routing was missing — the row stays pending but won't get a
        // second apology from the sweep.
        mark_inbound_tries(&inbound, message_id)?;

        // Flip the claim to Failed so the host-sweep `processing` reset
        // path won't also fire and create a duplicate retry. The
        // runner-restart path owns this inbound from here on.
        if let Err(err) =
            processing_ack::update_status(&outbound, message_id, ProcessingStatus::Failed)
        {
            warn!(
                session = %session.id.as_uuid(),
                message = %message_id.as_uuid(),
                ?err,
                "could not mark processing_ack Failed; sweep may double-fire"
            );
        }
    }

    info!(
        session = %session.id.as_uuid(),
        emitted,
        "crash-restart apologies emitted"
    );
    Ok(())
}

/// Read every `processing_ack` row currently in `processing` status.
/// Returns just the `MessageId`s; downstream code does the inbound
/// lookup. Sorted by `status_changed ASC` to keep ordering
/// deterministic for tests.
fn list_processing_acks(
    outbound: &rusqlite::Connection,
) -> Result<Vec<MessageId>, ManagerError> {
    let mut stmt = outbound
        .prepare(
            "SELECT message_id FROM processing_ack
             WHERE status = 'processing'
             ORDER BY status_changed ASC",
        )
        .map_err(|e| ManagerError::Db(ironclaw_db::DbError::from(e)))?;
    let rows = stmt
        .query_map([], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })
        .map_err(|e| ManagerError::Db(ironclaw_db::DbError::from(e)))?;
    let mut out = Vec::new();
    for row in rows {
        let id_str = row.map_err(|e| ManagerError::Db(ironclaw_db::DbError::from(e)))?;
        match uuid::Uuid::parse_str(&id_str) {
            Ok(uuid) => out.push(MessageId(uuid)),
            Err(err) => {
                warn!(
                    raw = id_str,
                    ?err,
                    "skipping unparseable processing_ack.message_id"
                );
            }
        }
    }
    Ok(out)
}

/// Fetch a single inbound row by id and project to the columns the
/// crash-restart apology needs. Returns `Ok(None)` when:
///
/// - the row is missing (orphaned `processing_ack` row), or
/// - the row lacks BOTH `channel_type` and `platform_id` (not
///   chat-routed).
fn lookup_inbound_routing(
    inbound: &rusqlite::Connection,
    message_id: MessageId,
) -> Result<Option<InFlightRouting>, ManagerError> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = inbound
        .query_row(
            "SELECT channel_type, platform_id, thread_id FROM messages_in WHERE id = ?1",
            params![message_id.as_uuid().to_string()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| ManagerError::Db(ironclaw_db::DbError::from(e)))?;

    let Some((channel_type, platform_id, thread_id)) = row else {
        return Ok(None);
    };

    match (channel_type, platform_id) {
        (Some(ct), Some(pid)) if !ct.is_empty() && !pid.is_empty() => {
            Ok(Some(InFlightRouting {
                message_id,
                channel_type: ChannelType::from(ct),
                platform_id: pid,
                thread_id,
            }))
        }
        _ => Ok(None),
    }
}

/// Stamp `tries = APOLOGY_TRIES_MARKER` on the matching inbound row.
/// Mirrors `host_sweep::apology::mark_tries_apology_sent`.
fn mark_inbound_tries(
    inbound: &rusqlite::Connection,
    message_id: MessageId,
) -> Result<(), ManagerError> {
    inbound
        .execute(
            "UPDATE messages_in SET tries = ?1 WHERE id = ?2",
            params![APOLOGY_TRIES_MARKER, message_id.as_uuid().to_string()],
        )
        .map_err(|e| ManagerError::Db(ironclaw_db::DbError::from(e)))?;
    Ok(())
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
        // Default DEFAULT_HEARTBEAT_STALE_SECS is 120s (raised from 60s
        // to preserve a 2x margin over DEFAULT_PROVIDER_DEADLINE_MS —
        // see spawn.rs::DEFAULT_HEARTBEAT_STALE_SECS). 240s puts us
        // comfortably past the threshold with margin for test wall-clock
        // jitter instead of sitting right at the boundary.
        let old =
            std::time::SystemTime::now() - std::time::Duration::from_secs(240);
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
        // some fictional crash window. With defaults (120s crash, 300s
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

    /// In-flight processing_ack row + chat-routed inbound → exactly
    /// one chat-kind outbound apology with the routing fields
    /// preserved. The processing_ack row flips to Failed and the
    /// inbound row's `tries` jumps to `APOLOGY_TRIES_MARKER` so the
    /// host-sweep apology + processing-reset paths both stay out.
    #[tokio::test]
    async fn crash_restart_emits_apology_for_in_flight_inbound() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;

        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();

        // Seed an inbound row + a matching processing_ack claim. This
        // is the "container picked up the message and was working on
        // it when it crashed" shape.
        let msg_id = ironclaw_types::MessageId::new();
        let inbound = open_inbound(&paths).unwrap();
        messages_in::insert(
            &inbound,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: ironclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "what's the weather"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("tg-42".into()),
                channel_type: Some(ironclaw_types::ChannelType::new("telegram")),
                thread_id: Some("thread-7".into()),
                source_session_id: None,
            },
        )
        .unwrap();
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(&outbound, msg_id, ProcessingStatus::Processing).unwrap();

        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();

        // Exactly one chat outbound row landed with the right routing.
        let chats: Vec<_> = ironclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(chats.len(), 1, "expected exactly one apology");
        let apology = &chats[0];
        assert_eq!(apology.in_reply_to, Some(msg_id));
        assert_eq!(
            apology.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(apology.platform_id.as_deref(), Some("tg-42"));
        assert_eq!(apology.thread_id.as_deref(), Some("thread-7"));
        let text = apology
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert!(
            text.contains("restart") && text.contains("agent container"),
            "apology text should mention restarting the agent: {text:?}"
        );

        // processing_ack row flipped to Failed so the host-sweep
        // processing-reset path won't double-fire.
        let claim = processing_ack::get(&outbound, msg_id).unwrap().unwrap();
        assert_eq!(claim.status, ProcessingStatus::Failed);

        // messages_in.tries was stamped so the host-sweep apology
        // PendingTooLong path won't double-fire.
        let tries: i64 = inbound
            .query_row(
                "SELECT tries FROM messages_in WHERE id = ?1",
                params![msg_id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tries, APOLOGY_TRIES_MARKER);

        // Idempotency: a second reconcile tick doesn't double-emit.
        // (The first pass marked the claim Failed, so the second pass
        // scans no rows and emits nothing.)
        let mut session2 = session.clone();
        sessions::mark_container_running(&db, session2.id).unwrap();
        session2.container_status = ContainerStatus::Running;
        mgr.apply(&session2, ReconcileAction::CrashRestart)
            .await
            .unwrap();
        let chats2: Vec<_> = ironclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(
            chats2.len(),
            1,
            "second crash-restart tick must not emit a duplicate apology"
        );
    }

    /// Empty inbound DB + no processing_ack rows → no apology row
    /// lands. Covers the corner case where the container died before
    /// picking up the inbound (or no inbound was in-flight at all).
    #[tokio::test]
    async fn crash_restart_emits_no_apology_without_in_flight_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;

        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        // Touch the per-session DBs so they exist (an inbound row may
        // exist without a processing_ack — that still means "nothing
        // was in-flight when the container died").
        let _ = open_inbound(&paths).unwrap();
        let outbound = open_outbound(&paths).unwrap();

        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();

        let chats: Vec<_> = ironclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .collect();
        assert!(
            chats.is_empty(),
            "no apology expected when no processing_ack rows are in-flight, got {chats:?}",
        );

        // And the session still flips to Stopped so the next tick respawns.
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    /// An in-flight processing_ack row whose inbound has NO channel
    /// routing (no `channel_type` / `platform_id`) must NOT emit an
    /// apology — there's nowhere to send it — but the processing_ack
    /// row should still flip to Failed and the inbound's `tries`
    /// should still be stamped so the host-sweep paths stay out.
    #[tokio::test]
    async fn crash_restart_skips_apology_when_routing_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;

        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let msg_id = ironclaw_types::MessageId::new();
        let inbound = open_inbound(&paths).unwrap();
        messages_in::insert(
            &inbound,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: ironclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "task"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: None,
                channel_type: None,
                thread_id: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(&outbound, msg_id, ProcessingStatus::Processing).unwrap();

        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();

        let chats: Vec<_> = ironclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == ironclaw_types::MessageKind::Chat)
            .collect();
        assert!(chats.is_empty(), "no apology when routing is missing");
        let claim = processing_ack::get(&outbound, msg_id).unwrap().unwrap();
        assert_eq!(claim.status, ProcessingStatus::Failed);
        let tries: i64 = inbound
            .query_row(
                "SELECT tries FROM messages_in WHERE id = ?1",
                params![msg_id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tries, APOLOGY_TRIES_MARKER);
    }
}
