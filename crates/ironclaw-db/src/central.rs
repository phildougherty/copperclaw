//! Central database (`data/ironclaw.db`) — pooled, WAL-mode `SQLite`.

use crate::migrate::{run_migrations, MigrationSet};
use crate::DbError;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::path::Path;
use std::sync::Arc;

/// Thread-safe handle to the central database. Cheap to clone.
#[derive(Clone)]
pub struct CentralDb {
    inner: Arc<Pool<SqliteConnectionManager>>,
    /// Filesystem path to the `SQLite` file, if it was opened from a real file
    /// (as opposed to an in-memory DB). `None` for in-memory databases.
    db_path: Option<std::path::PathBuf>,
}

impl CentralDb {
    /// Open (or create) the central database at `path`. Applies WAL mode,
    /// runs all pending migrations, and returns a pooled handle.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let manager = SqliteConnectionManager::file(path)
            .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE)
            .with_init(|c| {
                c.execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA synchronous=NORMAL;
                     PRAGMA foreign_keys=ON;
                     PRAGMA busy_timeout=5000;",
                )
            });

        // `min_idle(0)` skips r2d2's eager pre-warming. Without this, the
        // builder opens `max_size` connections in parallel, all running the
        // `PRAGMA journal_mode=WAL` init batch — they race the first writer
        // and r2d2 logs `ERROR database is locked` for each loser. The pool
        // still succeeds (it retries), but the log noise looks like a real
        // failure. Lazy creation avoids the race entirely.
        let pool = Pool::builder().max_size(8).min_idle(Some(0)).build(manager)?;

        {
            let mut conn = pool.get()?;
            run_migrations(&mut conn, MigrationSet::Central)?;
        }

        Ok(Self {
            inner: Arc::new(pool),
            db_path: Some(path.to_path_buf()),
        })
    }

    /// Open an in-memory database for tests.
    pub fn open_in_memory() -> Result<Self, DbError> {
        let manager = SqliteConnectionManager::memory().with_init(|c| {
            c.execute_batch(
                "PRAGMA foreign_keys=ON;
                 PRAGMA busy_timeout=5000;",
            )
        });
        // Use a single connection so the in-memory DB is shared across `get()`s.
        let pool = Pool::builder().max_size(1).build(manager)?;
        {
            let mut conn = pool.get()?;
            run_migrations(&mut conn, MigrationSet::Central)?;
        }
        Ok(Self {
            inner: Arc::new(pool),
            db_path: None,
        })
    }

    /// Return the filesystem path to the `SQLite` file, or `None` for
    /// in-memory databases. Used by the backup handler to locate the source
    /// file for the copy.
    pub fn path(&self) -> Option<&std::path::Path> {
        self.db_path.as_deref()
    }

    /// Borrow a pooled connection.
    pub fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>, DbError> {
        Ok(self.inner.get()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_runs_migrations() {
        let db = CentralDb::open_in_memory().unwrap();
        let conn = db.conn().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_groups'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn open_on_disk_creates_file_and_migrates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ironclaw.db");
        let db = CentralDb::open(&path).unwrap();
        assert!(path.exists());

        // Reopen — migrations should be idempotent.
        drop(db);
        let _again = CentralDb::open(&path).unwrap();
    }

    #[test]
    fn central_db_handle_is_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open(tmp.path().join("ironclaw.db")).unwrap();
        let cloned = db.clone();
        let _conn_a = db.conn().unwrap();
        let _conn_b = cloned.conn().unwrap();
    }
}
