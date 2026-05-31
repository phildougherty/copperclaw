//! Per-session database management.
//!
//! Each session has:
//! - `inbound.db` (host writes, container reads) — `journal_mode=DELETE`.
//! - `outbound.db` (container writes, host reads) — WAL.
//! - `.heartbeat` (container touches; host stats mtime).
//! - `inbox/<msg_id>/<filename>` — host-extracted attachment files.
//! - `outbox/<msg_id>/<filename>` — container-written attachment files.

use crate::migrate::{run_migrations, MigrationSet};
use crate::DbError;
use copperclaw_types::{AgentGroupId, SessionId};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Filesystem layout for a single session.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub root: PathBuf,
    pub inbound_db: PathBuf,
    pub outbound_db: PathBuf,
    pub heartbeat: PathBuf,
    pub inbox: PathBuf,
    pub outbox: PathBuf,
}

impl SessionPaths {
    pub fn new(data_root: impl AsRef<Path>, agent: AgentGroupId, session: SessionId) -> Self {
        let root = data_root
            .as_ref()
            .join("sessions")
            .join(agent.as_uuid().to_string())
            .join(session.as_uuid().to_string());
        Self {
            inbound_db: root.join("inbound.db"),
            outbound_db: root.join("outbound.db"),
            heartbeat: root.join(".heartbeat"),
            inbox: root.join("inbox"),
            outbox: root.join("outbox"),
            root,
        }
    }

    /// Create the session directory tree (idempotent).
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.inbox)?;
        std::fs::create_dir_all(&self.outbox)?;
        Ok(())
    }

    /// `mtime` of the heartbeat file, or `None` if the file doesn't exist.
    pub fn heartbeat_mtime(&self) -> std::io::Result<Option<SystemTime>> {
        match std::fs::metadata(&self.heartbeat) {
            Ok(m) => Ok(Some(m.modified()?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Open or create the host-owned `inbound.db`.
///
/// Enforces `journal_mode=DELETE` — WAL mode silently corrupts data when
/// the file is bind-mounted into a container, because the `-shm` mmap
/// region doesn't propagate guest-side writes back to the host.
pub fn open_inbound(paths: &SessionPaths) -> Result<Connection, DbError> {
    paths.ensure_dirs()?;
    let mut conn = Connection::open_with_flags(
        &paths.inbound_db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    )?;
    run_migrations(&mut conn, MigrationSet::SessionInbound)?;
    Ok(conn)
}

/// Open or create the container-owned `outbound.db`.
pub fn open_outbound(paths: &SessionPaths) -> Result<Connection, DbError> {
    paths.ensure_dirs()?;
    let mut conn = Connection::open_with_flags(
        &paths.outbound_db,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    )?;
    run_migrations(&mut conn, MigrationSet::SessionOutbound)?;
    Ok(conn)
}

/// Open `inbound.db` read-only with mmap disabled — used by the container
/// poll loop to ensure host-written rows become visible promptly across the
/// bind-mount.
pub fn open_inbound_ro_no_mmap(paths: &SessionPaths) -> Result<Connection, DbError> {
    let conn = Connection::open_with_flags(
        &paths.inbound_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    conn.execute_batch(
        "PRAGMA mmap_size=0;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(conn)
}

/// Open `inbound.db` read-write with mmap disabled. Used by the
/// container poll loop when it needs to update `messages_in.status`
/// (mark completed / failed) after a successful turn. Same no-mmap
/// gymnastics as [`open_inbound_ro_no_mmap`] — WAL + mmap doesn't
/// propagate writes across a bind mount, so we stay in DELETE mode.
pub fn open_inbound_rw_no_mmap(paths: &SessionPaths) -> Result<Connection, DbError> {
    let conn = Connection::open_with_flags(
        &paths.inbound_db,
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    conn.execute_batch(
        "PRAGMA mmap_size=0;
         PRAGMA busy_timeout=5000;",
    )?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_paths_layout_is_deterministic() {
        let ag = AgentGroupId::nil();
        let sess = SessionId::nil();
        let p = SessionPaths::new("/tmp/data", ag, sess);
        assert!(p.root.to_string_lossy().contains("sessions"));
        assert_eq!(p.inbound_db.file_name().unwrap(), "inbound.db");
        assert_eq!(p.outbound_db.file_name().unwrap(), "outbound.db");
        assert_eq!(p.heartbeat.file_name().unwrap(), ".heartbeat");
    }

    #[test]
    fn open_inbound_creates_tables_and_uses_delete_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "delete");

        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages_in'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn open_outbound_creates_tables_and_uses_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='messages_out'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn heartbeat_mtime_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        paths.ensure_dirs().unwrap();
        assert!(paths.heartbeat_mtime().unwrap().is_none());
    }

    #[test]
    fn heartbeat_mtime_reads_after_touch() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        let t = paths.heartbeat_mtime().unwrap();
        assert!(t.is_some());
    }

    #[test]
    fn ro_no_mmap_opens_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let _ = open_inbound(&paths).unwrap();
        let ro = open_inbound_ro_no_mmap(&paths).unwrap();
        let mmap_size: i64 = ro.query_row("PRAGMA mmap_size", [], |r| r.get(0)).unwrap();
        assert_eq!(mmap_size, 0);
    }
}
