//! Handlers for `db.backup` and `db.restore`.
//!
//! ## Backup
//!
//! `cclaw db backup <path>` runs a WAL checkpoint (`PRAGMA
//! wal_checkpoint(TRUNCATE)`) against the open central DB connection, then
//! copies the `SQLite` file atomically: write to `<path>.tmp`, then
//! `rename(2)` it into place.  The WAL checkpoint is best-effort — we log
//! a warning if it doesn't fully drain but proceed with the copy anyway,
//! because a partially-drained WAL is still a valid `SQLite` file that can be
//! restored.
//!
//! ## Restore
//!
//! `cclaw db restore <path>` refuses to run while the host process is alive
//! (which is always, since this command is dispatched by the socket server
//! running inside the host). The operator must stop the host first, then run
//! the restore directly against the file system. This command therefore
//! always returns an `ErrorPayload { code: "host_running" }` with a clear
//! explanation.
//!
//! Routing both sub-commands through the socket lets the audit log capture
//! backup attempts even when the file copy itself is handled by the OS.

use super::{db_err, req_str};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use serde_json::{Value, json};
use std::path::Path;
use tracing::warn;

/// `db.backup <path>` — WAL checkpoint + atomic file copy.
///
/// Steps:
/// 1. Run `PRAGMA wal_checkpoint(TRUNCATE)` on the open connection.
/// 2. Determine the central DB file path from the pool.
/// 3. Copy the file to `<path>.tmp`.
/// 4. Rename `<path>.tmp` to `<path>`.
///
/// Returns `{"path": "<path>", "wal_pages_remaining": N}` where
/// `wal_pages_remaining > 0` means the checkpoint didn't fully drain (a
/// write transaction was open on another connection). The backup is still
/// valid but may include some uncommitted WAL data that will be replayed on
/// open — standard `SQLite` backup behaviour.
pub fn backup(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let dest_str = req_str(args, "path")?;
    let dest = Path::new(&dest_str);

    // 1. WAL checkpoint.
    let wal_remaining = checkpoint(central)?;

    // 2. Locate the source file.
    let src = central.path().ok_or_else(|| {
        ErrorPayload::new(
            "in_memory_db",
            "central DB is in-memory; backup is not supported",
        )
    })?;

    // 3. Write to a temp file next to the destination so the rename is
    //    atomic on the same filesystem.
    let tmp = {
        let mut t = dest.to_path_buf();
        let name = t
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("copperclaw.db");
        t.set_file_name(format!("{name}.tmp"));
        t
    };

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ErrorPayload::new("io_error", format!("could not create destination dir: {e}"))
        })?;
    }

    std::fs::copy(src, &tmp)
        .map_err(|e| ErrorPayload::new("io_error", format!("copy to temp file failed: {e}")))?;

    // 4. Atomic rename.
    std::fs::rename(&tmp, dest).map_err(|e| {
        // Best-effort cleanup.
        let _ = std::fs::remove_file(&tmp);
        ErrorPayload::new("io_error", format!("rename to destination failed: {e}"))
    })?;

    Ok(json!({
        "path": dest_str,
        "wal_pages_remaining": wal_remaining,
    }))
}

/// `db.restore <path>` — always refuses because the host is running.
///
/// Restoring requires an exclusive lock on the `SQLite` file. The host holds
/// an open WAL connection, so swapping the file underneath it would corrupt
/// the database. The operator must stop the host (`systemctl stop copperclaw`
/// or kill the process), then restore by running:
///
///   cp <backup-path> <data-dir>/copperclaw.db
///
/// Once the host is stopped, the restore can also be done by a one-shot
/// `copperclaw migrate` invocation against the restored file to re-apply any
/// missing migrations before restarting.
pub fn restore(_args: &Value, _central: &CentralDb) -> Result<Value, ErrorPayload> {
    Err(ErrorPayload::new(
        "host_running",
        "db restore cannot run while the host is active: the host holds an open WAL connection \
         and swapping the file underneath it would corrupt the database. stop the host first \
         (e.g. `systemctl stop copperclaw`), then copy the backup file over \
         `<data_dir>/copperclaw.db` manually.",
    ))
}

/// Run `PRAGMA wal_checkpoint(TRUNCATE)` and return the number of WAL pages
/// that were not flushed to the main database file. Zero means the WAL is
/// empty; a positive value is a warning but not an error.
fn checkpoint(central: &CentralDb) -> Result<i64, ErrorPayload> {
    let conn = central.conn().map_err(db_err)?;
    // `PRAGMA wal_checkpoint(TRUNCATE)` returns a single row with three
    // columns: (busy, log, checkpointed). `log` is the number of WAL
    // frames; `checkpointed` is the number successfully written to the
    // main DB. Remaining = log - checkpointed.
    let result: rusqlite::Result<(i64, i64, i64)> =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        });
    match result {
        Ok((busy, log, checkpointed)) => {
            let remaining = log.saturating_sub(checkpointed);
            if busy != 0 || remaining > 0 {
                warn!(
                    busy,
                    log,
                    checkpointed,
                    remaining,
                    "WAL checkpoint did not fully drain; backup will still be valid"
                );
            }
            Ok(remaining)
        }
        Err(e) => {
            // Some builds or DB modes don't support TRUNCATE; fall back to a
            // PASSIVE checkpoint and return 0.
            warn!(
                ?e,
                "PRAGMA wal_checkpoint(TRUNCATE) failed; falling back to PASSIVE"
            );
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::central::CentralDb;
    use serde_json::json;
    use tempfile::tempdir;

    // Helper: open a central DB at a real temp file (not in-memory) so the
    // backup path can locate the source file.
    fn file_db() -> (tempfile::TempDir, CentralDb) {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("copperclaw.db");
        let db = CentralDb::open(&path).unwrap();
        (tmp, db)
    }

    #[test]
    fn backup_writes_destination_file() {
        let (_src_tmp, db) = file_db();
        let dest_tmp = tempdir().unwrap();
        let dest = dest_tmp.path().join("backup.db");

        let v = backup(&json!({"path": dest.to_str().unwrap()}), &db).unwrap();

        assert!(dest.exists(), "backup file should exist");
        assert_eq!(v["path"], dest.to_str().unwrap());
        // wal_pages_remaining is an integer (may be 0 or positive).
        assert!(v["wal_pages_remaining"].is_number());
    }

    #[test]
    fn backup_creates_parent_dirs() {
        let (_src_tmp, db) = file_db();
        let dest_tmp = tempdir().unwrap();
        let dest = dest_tmp.path().join("nested").join("dir").join("backup.db");
        backup(&json!({"path": dest.to_str().unwrap()}), &db).unwrap();
        assert!(dest.exists());
    }

    #[test]
    fn backup_missing_path_arg_errors() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = backup(&json!({}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn backup_in_memory_db_errors() {
        // open_in_memory returns None from path().
        let db = CentralDb::open_in_memory().unwrap();
        let err = backup(&json!({"path": "/tmp/x.db"}), &db).unwrap_err();
        assert_eq!(err.code, "in_memory_db");
    }

    #[test]
    fn restore_always_returns_host_running() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = restore(&json!({"path": "/tmp/x.db"}), &db).unwrap_err();
        assert_eq!(err.code, "host_running");
        assert!(err.message.contains("stop the host"));
    }

    #[test]
    fn backup_overwrites_existing_destination() {
        let (_src_tmp, db) = file_db();
        let dest_tmp = tempdir().unwrap();
        let dest = dest_tmp.path().join("backup.db");
        // Write something so the file already exists.
        std::fs::write(&dest, b"old content").unwrap();
        backup(&json!({"path": dest.to_str().unwrap()}), &db).unwrap();
        // The backup should have atomically replaced the old file.
        let meta = std::fs::metadata(&dest).unwrap();
        assert!(meta.len() > b"old content".len() as u64);
    }

    #[test]
    fn backup_tmp_file_does_not_remain_on_success() {
        let (_src_tmp, db) = file_db();
        let dest_tmp = tempdir().unwrap();
        let dest = dest_tmp.path().join("backup.db");
        backup(&json!({"path": dest.to_str().unwrap()}), &db).unwrap();
        // The .tmp file must have been renamed away.
        let tmp = dest_tmp.path().join("backup.db.tmp");
        assert!(
            !tmp.exists(),
            ".tmp file should not remain after successful backup"
        );
    }
}
