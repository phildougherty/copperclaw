//! Session-scoped DB access surface the router uses.
//!
//! `SessionRoot` is the integration point with the host's filesystem layout —
//! the host implements it once and hands the router an `Arc<dyn SessionRoot>`.
//! For tests we provide [`FsSessionRoot`], a tempfile-friendly implementation
//! rooted at any directory.

use crate::error::RouterError;
use ironclaw_db::session::{open_inbound, SessionPaths};
use ironclaw_types::{AgentGroupId, SessionId};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Lightweight wrapper around the per-session `inbound.db` writer.
///
/// `messages_in` is host-owned, so we keep a single connection serialized
/// behind a mutex. The router holds these via the `SessionRoot` trait so
/// production code can swap in pooled implementations later.
pub struct SessionPool {
    conn: Mutex<Connection>,
}

impl SessionPool {
    /// Wrap an already-opened `inbound.db` connection.
    pub fn from_connection(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Open the inbound.db at `paths` and wrap the resulting connection.
    pub fn open(paths: &SessionPaths) -> Result<Self, RouterError> {
        let conn = open_inbound(paths)
            .map_err(|e| RouterError::session_create(format!("open inbound: {e}")))?;
        Ok(Self::from_connection(conn))
    }

    /// Run `f` against the underlying connection. Panics if the inner
    /// mutex is poisoned — a poisoned writer connection means the host has
    /// already lost the integrity guarantee, so failing fast is correct.
    pub fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let conn = self.conn.lock().expect("session inbound mutex poisoned");
        f(&conn)
    }
}

impl std::fmt::Debug for SessionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPool").finish()
    }
}

/// Strategy object the router uses to locate (and create) per-session
/// `inbound.db` writers. The host implements this against its on-disk data
/// root; tests substitute [`FsSessionRoot`] over a temp dir.
pub trait SessionRoot: Send + Sync {
    /// Open the inbound pool for `(agent_group_id, session_id)`. Implementations
    /// are responsible for any caching they want; the router calls this once
    /// per fan-out target.
    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, RouterError>;

    /// Ensure the on-disk session directory tree exists for `(agent_group_id,
    /// session_id)`. Returns the absolute path of the session root.
    fn ensure_session_dir(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<PathBuf, RouterError>;
}

/// `SessionRoot` impl rooted at a single data directory. Each call to
/// [`SessionRoot::inbound_pool`] opens a fresh connection — sufficient for
/// the host's serialized writer model and trivial to use from tests.
pub struct FsSessionRoot {
    data_root: PathBuf,
}

impl FsSessionRoot {
    /// Build a root anchored at `data_root`. The directory does not have to
    /// exist yet; `ensure_session_dir` will `mkdir -p` it on demand.
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    /// Borrow the configured data root.
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Compute the `SessionPaths` layout for `(agent_group_id, session_id)`.
    pub fn paths(&self, agent_group_id: &AgentGroupId, session_id: &SessionId) -> SessionPaths {
        SessionPaths::new(&self.data_root, *agent_group_id, *session_id)
    }
}

impl SessionRoot for FsSessionRoot {
    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, RouterError> {
        let paths = self.paths(agent_group_id, session_id);
        SessionPool::open(&paths)
    }

    fn ensure_session_dir(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<PathBuf, RouterError> {
        let paths = self.paths(agent_group_id, session_id);
        paths
            .ensure_dirs()
            .map_err(|e| RouterError::session_create(format!("ensure_dirs: {e}")))?;
        Ok(paths.root.clone())
    }
}

impl std::fmt::Debug for FsSessionRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsSessionRoot")
            .field("data_root", &self.data_root)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_db::tables::messages_in::{insert as insert_in, WriteInbound};
    use ironclaw_types::{ChannelType, MessageId, MessageKind};

    #[test]
    fn fs_session_root_ensure_dir_creates_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let path = root.ensure_session_dir(&ag, &sess).unwrap();
        assert!(path.exists());
        assert!(path.join("inbox").exists());
        assert!(path.join("outbox").exists());
    }

    #[test]
    fn fs_session_root_open_inbound_pool_writes_row() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        root.ensure_session_dir(&ag, &sess).unwrap();
        let pool = root.inbound_pool(&ag, &sess).unwrap();
        let seq = pool.with_conn(|c| {
            insert_in(
                c,
                &WriteInbound {
                    id: MessageId::new(),
                    kind: MessageKind::Chat,
                    timestamp: chrono::Utc::now(),
                    content: serde_json::json!({"text":"hi"}),
                    trigger: true,
                    on_wake: false,
                    process_after: None,
                    recurrence: None,
                    series_id: None,
                    platform_id: Some("p1".into()),
                    channel_type: Some(ChannelType::new("cli")),
                    thread_id: None,
                    source_session_id: None,
                },
            )
            .unwrap()
        });
        assert_eq!(seq % 2, 0);
    }

    #[test]
    fn fs_session_root_paths_match_session_paths_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let p = root.paths(&ag, &sess);
        assert!(p.root.starts_with(tmp.path()));
        assert_eq!(p.inbound_db.file_name().unwrap(), "inbound.db");
    }

    #[test]
    fn session_pool_from_connection_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let pool = SessionPool::open(&paths).unwrap();
        let count: i64 = pool.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(count, 0);
    }

    #[test]
    fn session_pool_debug_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let pool = SessionPool::open(&paths).unwrap();
        assert!(format!("{pool:?}").contains("SessionPool"));
    }

    #[test]
    fn fs_session_root_debug_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        assert!(format!("{root:?}").contains("FsSessionRoot"));
    }

    #[test]
    fn data_root_accessor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        assert_eq!(root.data_root(), tmp.path());
    }
}
