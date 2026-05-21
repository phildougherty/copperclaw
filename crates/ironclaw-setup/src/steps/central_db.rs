//! Step 3 — central DB init.
//!
//! Opens `<data_dir>/data/ironclaw.db` via `ironclaw-db`, which runs the
//! `MigrationSet::Central` migrations idempotently.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use ironclaw_db::central::CentralDb;
use ironclaw_db::migrate::{applied_central_schema_version, expected_central_schema_version};

/// Step implementation.
#[derive(Debug, Default)]
pub struct CentralDbStep;

impl Step for CentralDbStep {
    fn name(&self) -> &'static str {
        "central_db"
    }

    fn description(&self) -> &'static str {
        "Initialise the central ironclaw database"
    }

    fn is_skippable(&self) -> bool {
        false
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        _prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        if cfg.central_db_path.as_os_str().is_empty() {
            cfg.central_db_path = cfg.data_dir.join("data").join("ironclaw.db");
        }
        // Detect a downgrade scenario before touching the file:
        // if the DB already exists and reports more applied
        // migrations than this binary knows about, refuse to
        // proceed rather than silently running against a future
        // schema. Mirrors `ironclaw_host::boot::check_schema_version`
        // (exit code 5 / `BootError::SchemaMismatch`).
        check_schema_version(&cfg.central_db_path)?;
        open_and_migrate(&cfg.central_db_path)?;
        Ok(StepResult::ok(format!(
            "central DB initialised at {}",
            cfg.central_db_path.display()
        )))
    }
}

/// Open + migrate the DB at `path`. Wraps `CentralDb::open` so steps can
/// surface a friendly error.
pub fn open_and_migrate(path: &std::path::Path) -> Result<(), StepError> {
    CentralDb::open(path).map_err(|e| StepError::Other(format!("central DB open failed: {e}")))?;
    Ok(())
}

/// Refuse to proceed when the DB at `path` already exists and reports
/// more applied migrations than the binary's expected migration set.
///
/// This mirrors `ironclaw_host::boot::check_schema_version`: the wizard
/// must not run against a "future" schema (an older binary touching a
/// DB that was migrated by a newer ironclaw release). Pre-existing
/// fresh / equal / behind DBs all return `Ok(())`; the open step
/// applies any pending migrations afterward.
pub fn check_schema_version(path: &std::path::Path) -> Result<(), StepError> {
    if !path.exists() {
        return Ok(());
    }
    // Open the DB read-only via a fresh rusqlite handle. We deliberately
    // don't go through `CentralDb::open` here because that path runs
    // migrations as a side-effect — the whole point of this check is to
    // catch the downgrade before we touch the schema.
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .map_err(|e| StepError::Other(format!("open central DB for schema check: {e}")))?;
    let expected = expected_central_schema_version();
    let applied = applied_central_schema_version(&conn)
        .map_err(|e| StepError::Other(format!("read schema version: {e}")))?
        .unwrap_or(0);
    if applied > expected {
        return Err(StepError::Other(format!(
            "schema mismatch: on-disk DB has {applied} applied migrations but \
             this binary only knows {expected}; refusing to run against a future \
             schema (downgrade detected)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    #[test]
    fn open_and_migrate_creates_db() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ironclaw.db");
        open_and_migrate(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn open_and_migrate_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ironclaw.db");
        open_and_migrate(&path).unwrap();
        open_and_migrate(&path).unwrap();
    }

    #[test]
    fn open_and_migrate_failure_is_other_error() {
        // Pass a directory instead of a file — open should fail.
        let dir = tempdir().unwrap();
        let bad_path = dir.path().to_path_buf();
        let err = open_and_migrate(&bad_path).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    #[test]
    fn step_run_initialises_db() {
        let dir = tempdir().unwrap();
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            central_db_path: dir.path().join("data/ironclaw.db"),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new();
        let res = CentralDbStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.config_changed);
        assert!(cfg.central_db_path.exists());
    }

    #[test]
    fn step_fills_central_path_when_empty() {
        let dir = tempdir().unwrap();
        let mut cfg = SetupConfig {
            data_dir: dir.path().to_path_buf(),
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new();
        CentralDbStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert_eq!(cfg.central_db_path, dir.path().join("data/ironclaw.db"));
    }

    #[test]
    fn step_metadata() {
        let s = CentralDbStep;
        assert_eq!(s.name(), "central_db");
        assert!(!s.description().is_empty());
        assert!(!s.is_skippable());
    }
}
