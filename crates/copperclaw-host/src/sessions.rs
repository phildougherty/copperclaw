//! On-disk session layout shared by router, delivery, and sweep.
//!
//! Each of those three crates declares its own `SessionRoot` trait that
//! returns a per-crate `SessionPool` wrapper. The host implements **all
//! three traits on the same struct** so the data-root directory is a single
//! source of truth.

use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
use copperclaw_host_delivery as delivery;
use copperclaw_host_router as router;
use copperclaw_host_sweep as sweep;
use copperclaw_types::{AgentGroupId, SessionId};
use std::path::{Path, PathBuf};

/// Single host implementation of every `SessionRoot` trait.
///
/// The struct owns nothing but the data-root path; each method opens a fresh
/// per-session DB connection. Cheap to clone and `Send + Sync`.
pub struct FsSessionRoot {
    data_root: PathBuf,
}

impl FsSessionRoot {
    /// Build a new root anchored at `data_root`. The directory does not need
    /// to exist yet — each `ensure_session_dir`/`*_pool` call creates it on
    /// demand.
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }

    /// Borrow the configured data root.
    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    /// Compute the [`SessionPaths`] layout for `(agent_group_id, session_id)`.
    pub fn paths(&self, agent: AgentGroupId, session: SessionId) -> SessionPaths {
        SessionPaths::new(&self.data_root, agent, session)
    }
}

impl std::fmt::Debug for FsSessionRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsSessionRoot")
            .field("data_root", &self.data_root)
            .finish()
    }
}

// ----- router -------------------------------------------------------------

impl router::SessionRoot for FsSessionRoot {
    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<router::SessionPool, router::RouterError> {
        let paths = self.paths(*agent_group_id, *session_id);
        paths
            .ensure_dirs()
            .map_err(|e| router::RouterError::session_create(format!("ensure_dirs: {e}")))?;
        router::SessionPool::open(&paths)
    }

    fn ensure_session_dir(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<PathBuf, router::RouterError> {
        let paths = self.paths(*agent_group_id, *session_id);
        paths
            .ensure_dirs()
            .map_err(|e| router::RouterError::session_create(format!("ensure_dirs: {e}")))?;
        Ok(paths.root)
    }
}

// ----- delivery -----------------------------------------------------------

impl delivery::SessionRoot for FsSessionRoot {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<delivery::SessionPool, delivery::DeliveryError> {
        let paths = self.paths(*agent_group_id, *session_id);
        paths
            .ensure_dirs()
            .map_err(|e| delivery::DeliveryError::Db(copperclaw_db::DbError::Io(e)))?;
        Ok(delivery::SessionPool::outbound(paths))
    }

    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<delivery::SessionPool, delivery::DeliveryError> {
        let paths = self.paths(*agent_group_id, *session_id);
        paths
            .ensure_dirs()
            .map_err(|e| delivery::DeliveryError::Db(copperclaw_db::DbError::Io(e)))?;
        Ok(delivery::SessionPool::inbound(paths))
    }
}

// ----- sweep --------------------------------------------------------------

impl sweep::SessionRoot for FsSessionRoot {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<sweep::SessionPool, sweep::SweepError> {
        let paths = self.paths(*agent_group_id, *session_id);
        paths.ensure_dirs()?;
        let conn = open_outbound(&paths)?;
        Ok(sweep::SessionPool::new(conn))
    }

    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<sweep::SessionPool, sweep::SweepError> {
        let paths = self.paths(*agent_group_id, *session_id);
        paths.ensure_dirs()?;
        let conn = open_inbound(&paths)?;
        Ok(sweep::SessionPool::new(conn))
    }

    fn heartbeat_path(&self, agent_group_id: &AgentGroupId, session_id: &SessionId) -> PathBuf {
        self.paths(*agent_group_id, *session_id).heartbeat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_host_router::SessionRoot as _;

    #[test]
    fn new_and_accessor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        assert_eq!(root.data_root(), tmp.path());
    }

    #[test]
    fn debug_impl_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let s = format!("{root:?}");
        assert!(s.contains("FsSessionRoot"));
    }

    #[test]
    fn router_inbound_pool_creates_dirs_and_returns_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let pool = router::SessionRoot::inbound_pool(&root, &ag, &sess).unwrap();
        let count: i64 = pool.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM messages_in", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(count, 0);
    }

    #[test]
    fn router_ensure_session_dir_returns_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let p = root.ensure_session_dir(&ag, &sess).unwrap();
        assert!(p.exists());
        assert!(p.join("inbox").exists());
    }

    #[test]
    fn delivery_inbound_outbound_pools_work() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let outb = delivery::SessionRoot::outbound_pool(&root, &ag, &sess).unwrap();
        let inb = delivery::SessionRoot::inbound_pool(&root, &ag, &sess).unwrap();
        let _ = outb.connect().unwrap();
        let _ = inb.connect().unwrap();
    }

    #[test]
    fn sweep_inbound_outbound_pools_work() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let mut outb = sweep::SessionRoot::outbound_pool(&root, &ag, &sess).unwrap();
        let _ = outb.conn();
        let _ = outb.conn_mut();
        let mut inb = sweep::SessionRoot::inbound_pool(&root, &ag, &sess).unwrap();
        let _ = inb.conn();
        let _ = inb.conn_mut();
    }

    #[test]
    fn sweep_heartbeat_path_is_under_data_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let hb = sweep::SessionRoot::heartbeat_path(&root, &ag, &sess);
        assert!(hb.starts_with(tmp.path()));
        assert_eq!(hb.file_name().unwrap(), ".heartbeat");
    }

    #[test]
    fn paths_helper_produces_paths_struct() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let p = root.paths(AgentGroupId::new(), SessionId::new());
        assert!(p.inbound_db.to_string_lossy().ends_with("inbound.db"));
        assert!(p.outbound_db.to_string_lossy().ends_with("outbound.db"));
    }
}
