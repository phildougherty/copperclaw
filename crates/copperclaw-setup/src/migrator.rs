//! Data directory migrator (`--migrate-from`).
//!
//! When the operator passes `--migrate-from <path>`, the binary copies any
//! existing `copperclaw.db` from the supplied directory into the new data directory
//! and then re-runs the central migrations to bring it up to the current
//! schema.

use copperclaw_db::central::CentralDb;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Errors surfaced by the migrator.
#[derive(Debug, thiserror::Error)]
pub enum MigratorError {
    /// The source directory does not exist or is unreadable.
    #[error("source {path} not found")]
    SourceMissing {
        /// Source directory provided by the operator.
        path: PathBuf,
    },
    /// Filesystem error while copying.
    #[error("copy from {from} to {to} failed: {source}")]
    Copy {
        /// Source path.
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// Underlying DB error during migration.
    #[error("central DB migration failed: {0}")]
    Db(#[from] copperclaw_db::DbError),
    /// Misc I/O failure (mkdir, etc.).
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Outcome of a migration attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    /// Whether a database file was copied.
    pub copied_db: bool,
    /// Destination path of the central DB after migration.
    pub central_db_path: PathBuf,
}

/// Copy an existing data directory's central DB into `dest_dir` and run
/// migrations. Idempotent and safe even when no `copperclaw.db` is present at
/// `source_dir`.
pub fn migrate_from(source_dir: &Path, dest_dir: &Path) -> Result<MigrationOutcome, MigratorError> {
    if !source_dir.exists() {
        return Err(MigratorError::SourceMissing {
            path: source_dir.to_path_buf(),
        });
    }
    let dest_data = dest_dir.join("data");
    fs::create_dir_all(&dest_data)?;
    let central_db_path = dest_data.join("copperclaw.db");

    let source_db = source_dir.join("data").join("copperclaw.db");
    let mut copied = false;
    if source_db.exists() {
        if let Some(parent) = central_db_path.parent() {
            fs::create_dir_all(parent).map_err(|source| MigratorError::Copy {
                from: source_db.clone(),
                to: central_db_path.clone(),
                source,
            })?;
        }
        fs::copy(&source_db, &central_db_path).map_err(|source| MigratorError::Copy {
            from: source_db.clone(),
            to: central_db_path.clone(),
            source,
        })?;
        copied = true;
    }

    // Open the destination DB; CentralDb::open runs migrations idempotently.
    let _db = CentralDb::open(&central_db_path)?;

    Ok(MigrationOutcome {
        copied_db: copied,
        central_db_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::central::CentralDb;
    use tempfile::tempdir;

    #[test]
    fn migrate_from_missing_source_errors() {
        let dest = tempdir().unwrap();
        let err =
            migrate_from(Path::new("/definitely/does/not/exist/copperclaw"), dest.path()).unwrap_err();
        assert!(matches!(err, MigratorError::SourceMissing { .. }));
    }

    #[test]
    fn migrate_from_without_db_creates_fresh() {
        let src = tempdir().unwrap();
        let dest = tempdir().unwrap();
        let outcome = migrate_from(src.path(), dest.path()).unwrap();
        assert!(!outcome.copied_db);
        assert!(outcome.central_db_path.exists());
        // Re-open to confirm it migrated cleanly.
        let _db = CentralDb::open(&outcome.central_db_path).unwrap();
    }

    #[test]
    fn migrate_from_copies_existing_db() {
        let src = tempdir().unwrap();
        let src_data = src.path().join("data");
        fs::create_dir_all(&src_data).unwrap();
        let src_db = src_data.join("copperclaw.db");
        // Pre-create a real DB so we know migrations succeed after copy.
        let db = CentralDb::open(&src_db).unwrap();
        drop(db);

        let dest = tempdir().unwrap();
        let outcome = migrate_from(src.path(), dest.path()).unwrap();
        assert!(outcome.copied_db);
        assert!(outcome.central_db_path.exists());
        // The destination DB still opens cleanly with migrations.
        let _db = CentralDb::open(&outcome.central_db_path).unwrap();
    }

    #[test]
    fn migrate_from_is_idempotent() {
        let src = tempdir().unwrap();
        let dest = tempdir().unwrap();
        let _ = migrate_from(src.path(), dest.path()).unwrap();
        let again = migrate_from(src.path(), dest.path()).unwrap();
        assert!(again.central_db_path.exists());
    }

    #[test]
    fn migrator_error_display_source_missing() {
        let e = MigratorError::SourceMissing {
            path: PathBuf::from("/x"),
        };
        assert!(e.to_string().contains("/x"));
    }

    #[test]
    fn migrator_error_display_copy() {
        let e = MigratorError::Copy {
            from: PathBuf::from("/a"),
            to: PathBuf::from("/b"),
            source: io::Error::other("nope"),
        };
        let s = e.to_string();
        assert!(s.contains("/a"));
        assert!(s.contains("/b"));
    }

    #[test]
    fn migrator_error_from_io() {
        let e: MigratorError = io::Error::other("boom").into();
        assert!(matches!(e, MigratorError::Io(_)));
    }
}
