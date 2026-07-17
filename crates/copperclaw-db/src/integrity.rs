//! Read-only `SQLite` integrity probing (`PRAGMA quick_check`).
//!
//! Additive, non-mutating helpers shared by the host sweep's DB-integrity
//! check (M21 O2). Nothing here runs a migration, writes a row, or alters
//! the file: [`quick_check`] opens the database, asks `SQLite` to verify its
//! b-tree structure, and reports the outcome. `quick_check` is the cheaper
//! sibling of `integrity_check` — it skips the (expensive) index-vs-table
//! cross-validation, which is why the sweep can afford to run it on a
//! rotating subset of the fleet each pass (decision (f) in the M21 plan).

use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Outcome of a `PRAGMA quick_check` on one database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickCheckOutcome {
    /// `SQLite` returned the single row `ok` — the database is structurally
    /// sound.
    Healthy,
    /// The file does not exist. Not corruption: a freshly-created session
    /// may not have materialised its per-session DBs yet, so callers skip
    /// it rather than quarantine it.
    Missing,
    /// The database could not be opened, or `quick_check` reported one or
    /// more problems. The payload is a human-readable detail string (the
    /// open error, or the joined problem lines) suitable for a log line
    /// and a quarantine sidecar.
    Corrupt(String),
}

impl QuickCheckOutcome {
    /// True only for [`QuickCheckOutcome::Corrupt`].
    pub fn is_corrupt(&self) -> bool {
        matches!(self, QuickCheckOutcome::Corrupt(_))
    }
}

/// Run `PRAGMA quick_check` on the database at `path`, read-only, without
/// running any migration.
///
/// A missing file returns [`QuickCheckOutcome::Missing`]. A file that
/// cannot be opened as a database, or that fails the check, returns
/// [`QuickCheckOutcome::Corrupt`] with a detail string. Opening is
/// read-only (`SQLITE_OPEN_READ_ONLY`, no `CREATE`) so this never
/// resurrects a deleted DB or mutates a live one.
pub fn quick_check(path: &Path) -> QuickCheckOutcome {
    if !path.exists() {
        return QuickCheckOutcome::Missing;
    }
    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => return QuickCheckOutcome::Corrupt(format!("open failed: {e}")),
    };
    quick_check_conn(&conn)
}

/// Run `PRAGMA quick_check` on an already-open connection. Used for the
/// central DB, whose pooled handle is already held by the host, so there
/// is no separate file to reopen.
pub fn quick_check_conn(conn: &Connection) -> QuickCheckOutcome {
    let mut stmt = match conn.prepare("PRAGMA quick_check") {
        Ok(s) => s,
        Err(e) => return QuickCheckOutcome::Corrupt(format!("quick_check prepare failed: {e}")),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(iter) => iter,
        Err(e) => return QuickCheckOutcome::Corrupt(format!("quick_check query failed: {e}")),
    };
    let mut lines = Vec::new();
    for row in rows {
        match row {
            Ok(s) => lines.push(s),
            Err(e) => return QuickCheckOutcome::Corrupt(format!("quick_check row error: {e}")),
        }
    }
    // A healthy database returns exactly one row: the literal "ok".
    if lines.len() == 1 && lines[0] == "ok" {
        QuickCheckOutcome::Healthy
    } else if lines.is_empty() {
        QuickCheckOutcome::Corrupt("quick_check returned no rows".to_string())
    } else {
        QuickCheckOutcome::Corrupt(lines.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.db");
        assert_eq!(quick_check(&path), QuickCheckOutcome::Missing);
    }

    #[test]
    fn healthy_db_reports_healthy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ok.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        assert_eq!(quick_check(&path), QuickCheckOutcome::Healthy);
    }

    #[test]
    fn garbage_file_is_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("garbage.db");
        std::fs::write(&path, b"this is definitely not a sqlite database file").unwrap();
        let outcome = quick_check(&path);
        assert!(outcome.is_corrupt(), "got {outcome:?}");
    }

    #[test]
    fn conn_helper_matches_path_helper_on_healthy_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        assert_eq!(quick_check_conn(&conn), QuickCheckOutcome::Healthy);
    }
}
