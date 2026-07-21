//! Test fixtures and an in-memory [`SessionRoot`] implementation.
//!
//! This module is compiled for both `cfg(test)` (unit tests) and as
//! `pub(crate)` so the per-check modules can share fixtures. It is not
//! part of the crate's public API.

use crate::error::SweepError;
use crate::service::{SessionPool, SessionRoot};
use chrono::{DateTime, TimeZone, Utc};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
use copperclaw_db::tables::container_state;
use copperclaw_db::tables::messages_in::{WriteInbound, insert as insert_in};
use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_out};
use copperclaw_db::tables::processing_ack::{ProcessingStatus, insert as insert_ack};
use copperclaw_db::tables::sessions::{
    CreateSession, create as create_sess, mark_container_running,
};
use copperclaw_types::{AgentGroupId, ChannelType, MessageId, MessageKind, Session, SessionId};
use rusqlite::params;
use std::path::PathBuf;
use std::time::SystemTime;

/// In-memory [`SessionRoot`] — opens on-disk sqlite files under a temp
/// directory so that fresh `Connection::open` calls see the same data.
///
/// We back per-session DBs with real files inside the same `TempDir` so
/// that successive `outbound_pool` calls observe each other's writes.
pub struct MemSessionRoot {
    tmp: tempfile::TempDir,
    // If true, `inbound_pool` / `outbound_pool` always return Err so we
    // can exercise the error-swallowing path in `SweepService::run_once`.
    strict_unknown: bool,
}

impl MemSessionRoot {
    pub fn new() -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
            strict_unknown: false,
        }
    }

    /// Build a root that refuses to open any per-session pool. Used by
    /// error-path tests.
    pub fn new_strict_unknown() -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
            strict_unknown: true,
        }
    }

    /// Set the `.heartbeat` file mtime to the given instant by writing the
    /// file and calling `File::set_modified`.
    pub fn write_heartbeat(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
        when: SystemTime,
    ) {
        let path = self.heartbeat_path(agent_group_id, session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(when).unwrap();
    }

    fn refuse_if_strict(&self) -> Result<(), SweepError> {
        if self.strict_unknown {
            Err(SweepError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "session not registered (strict mode)",
            )))
        } else {
            Ok(())
        }
    }
}

impl SessionRoot for MemSessionRoot {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, SweepError> {
        self.refuse_if_strict()?;
        let paths = copperclaw_db::session::SessionPaths::new(
            self.tmp.path(),
            *agent_group_id,
            *session_id,
        );
        let conn = copperclaw_db::session::open_outbound(&paths)?;
        Ok(SessionPool::new(conn))
    }

    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, SweepError> {
        self.refuse_if_strict()?;
        let paths = copperclaw_db::session::SessionPaths::new(
            self.tmp.path(),
            *agent_group_id,
            *session_id,
        );
        let conn = copperclaw_db::session::open_inbound(&paths)?;
        Ok(SessionPool::new(conn))
    }

    fn heartbeat_path(&self, agent_group_id: &AgentGroupId, session_id: &SessionId) -> PathBuf {
        copperclaw_db::session::SessionPaths::new(self.tmp.path(), *agent_group_id, *session_id)
            .heartbeat
    }

    fn session_paths(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> copperclaw_db::session::SessionPaths {
        copperclaw_db::session::SessionPaths::new(self.tmp.path(), *agent_group_id, *session_id)
    }
}

/// Create an `agent_group` + session in the central DB, mark the
/// container as running, and return the freshly-loaded `Session` row.
pub fn seed_running_session(central: &CentralDb) -> Session {
    let ag = create_ag(
        central,
        CreateAgentGroup {
            name: "sweep-test".into(),
            folder: format!("sweep-{}", uuid::Uuid::new_v4()),
            agent_provider: None,
        },
    )
    .unwrap();
    let sess = create_sess(
        central,
        CreateSession {
            agent_group_id: ag.id,
            messaging_group_id: None,
            thread_id: None,
            agent_provider: None,
            source_session_id: None,
        },
    )
    .unwrap();
    mark_container_running(central, sess.id).unwrap();
    copperclaw_db::tables::sessions::get(central, sess.id).unwrap()
}

pub fn seed_stuck_tool(root: &MemSessionRoot, session: &Session, started_at: DateTime<Utc>) {
    let mut pool = root
        .outbound_pool(&session.agent_group_id, &session.id)
        .unwrap();
    container_state::set(
        pool.conn_mut(),
        &container_state::ContainerState {
            current_tool: Some("bash".into()),
            tool_declared_timeout_ms: Some(10_000),
            tool_started_at: Some(started_at),
            updated_at: Some(Utc::now()),
        },
    )
    .unwrap();
}

/// Corrupt one of a session's per-session DB files (`inbound.db` or
/// `outbound.db`) by materialising it, then overwriting it (and dropping
/// any WAL sidecars) with bytes that are not a valid `SQLite` database. Used
/// by the M21 O2 integrity tests.
pub fn corrupt_session_db(root: &MemSessionRoot, session: &Session, db: &str) {
    // Materialise both DBs first so the session dir + files exist.
    let _ = root
        .outbound_pool(&session.agent_group_id, &session.id)
        .unwrap();
    let _ = root
        .inbound_pool(&session.agent_group_id, &session.id)
        .unwrap();
    let paths = root.session_paths(&session.agent_group_id, &session.id);
    let target = match db {
        "inbound.db" => paths.inbound_db,
        "outbound.db" => paths.outbound_db,
        other => panic!("unknown db file: {other}"),
    };
    // Drop WAL sidecars so the garbage file is authoritative.
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = target.clone().into_os_string();
        sidecar.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
    }
    std::fs::write(
        &target,
        b"not a sqlite database at all -- corrupted for the test",
    )
    .unwrap();
}

pub fn seed_stale_heartbeat(root: &MemSessionRoot, session: &Session, when: DateTime<Utc>) {
    let sys = SystemTime::from(when);
    root.write_heartbeat(&session.agent_group_id, &session.id, sys);
}

/// Seed a stuck `processing_ack`: a `processing` claim that is older than
/// `CLAIM_STUCK_MS` and has a corresponding `messages_in` row but no reply.
/// Returns the message id.
pub fn seed_stuck_processing_ack(
    root: &MemSessionRoot,
    session: &Session,
    when: DateTime<Utc>,
) -> MessageId {
    let msg = insert_inbound_message(root, session);
    let mut pool = root
        .outbound_pool(&session.agent_group_id, &session.id)
        .unwrap();
    pool.conn_mut()
        .execute(
            "INSERT INTO processing_ack (message_id, status, status_changed)
             VALUES (?1, ?2, ?3)",
            params![
                msg.as_uuid().to_string(),
                ProcessingStatus::Processing.as_str(),
                when.to_rfc3339()
            ],
        )
        .unwrap();
    msg
}

pub fn seed_due_message(root: &MemSessionRoot, session: &Session, process_after: DateTime<Utc>) {
    insert_inbound_message_with_process_after(root, session, Some(process_after));
}

pub fn seed_recurrence(
    root: &MemSessionRoot,
    session: &Session,
    cron: &str,
    process_after: DateTime<Utc>,
) -> MessageId {
    insert_recurring_inbound(
        root,
        session,
        cron,
        Some("series-x".into()),
        Some(process_after),
    )
}

pub fn insert_inbound_message(root: &MemSessionRoot, session: &Session) -> MessageId {
    insert_inbound_message_with_process_after(root, session, None)
}

pub fn insert_inbound_message_with_process_after(
    root: &MemSessionRoot,
    session: &Session,
    process_after: Option<DateTime<Utc>>,
) -> MessageId {
    let id = MessageId::new();
    let msg = WriteInbound {
        id,
        kind: MessageKind::Chat,
        timestamp: Utc::now(),
        content: serde_json::json!({"text": "hi"}),
        trigger: true,
        on_wake: false,
        process_after,
        recurrence: None,
        series_id: None,
        platform_id: Some("p-1".into()),
        channel_type: Some(ChannelType::new("cli")),
        thread_id: None,
        source_session_id: None,
        reply_to: None,
        is_group: None,
    };
    let mut pool = root
        .inbound_pool(&session.agent_group_id, &session.id)
        .unwrap();
    insert_in(pool.conn_mut(), &msg).unwrap();
    id
}

pub fn insert_recurring_inbound(
    root: &MemSessionRoot,
    session: &Session,
    recurrence: &str,
    series_id: Option<String>,
    process_after: Option<DateTime<Utc>>,
) -> MessageId {
    let id = MessageId::new();
    let msg = WriteInbound {
        id,
        kind: MessageKind::Task,
        timestamp: Utc::now(),
        content: serde_json::json!({"text": "recurring"}),
        trigger: true,
        on_wake: false,
        process_after,
        recurrence: Some(recurrence.to_string()),
        series_id,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        source_session_id: None,
        reply_to: None,
        is_group: None,
    };
    let mut pool = root
        .inbound_pool(&session.agent_group_id, &session.id)
        .unwrap();
    insert_in(pool.conn_mut(), &msg).unwrap();
    id
}

pub fn insert_outbound_reply(
    root: &MemSessionRoot,
    session: &Session,
    in_reply_to: MessageId,
) -> MessageId {
    let id = MessageId::new();
    let msg = WriteOutbound {
        id,
        in_reply_to: Some(in_reply_to),
        timestamp: Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: MessageKind::Chat,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        content: serde_json::json!({"text": "ok"}),
    };
    let mut pool = root
        .outbound_pool(&session.agent_group_id, &session.id)
        .unwrap();
    insert_out(pool.conn_mut(), &msg).unwrap();
    id
}

#[allow(dead_code)]
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 21, 12, 0, 0).unwrap()
}

// Suppress unused-import warning on insert_ack in some build configurations.
#[allow(dead_code)]
fn _unused_imports_anchor() {
    let _ = insert_ack as fn(_, _, _) -> _;
}
