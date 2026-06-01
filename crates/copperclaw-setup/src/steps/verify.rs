//! Step 12 — smoke-test the central DB.
//!
//! Opens the freshly migrated DB and lists `agent_groups`. The user gets a
//! row count so they can confirm setup wired everything up correctly.

use crate::config::SetupConfig;
use crate::prompt::Prompt;
use crate::state::SetupState;
use crate::steps::{Step, StepError, StepResult};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::agent_groups;

/// Step implementation.
#[derive(Debug, Default)]
pub struct VerifyStep;

impl Step for VerifyStep {
    fn name(&self) -> &'static str {
        "verify"
    }

    fn description(&self) -> &'static str {
        "Smoke-test the central DB"
    }

    fn run(
        &self,
        cfg: &mut SetupConfig,
        _prompt: &dyn Prompt,
        _state: &mut SetupState,
    ) -> Result<StepResult, StepError> {
        let count = count_agent_groups(&cfg.central_db_path)?;
        Ok(StepResult::ok(format!(
            "verify: agent_groups has {count} row(s)"
        )))
    }
}

/// Open the central DB at `path` and return the number of `agent_groups`.
pub fn count_agent_groups(path: &std::path::Path) -> Result<usize, StepError> {
    let db =
        CentralDb::open(path).map_err(|e| StepError::Other(format!("open central DB: {e}")))?;
    let rows =
        agent_groups::list(&db).map_err(|e| StepError::Other(format!("list agent_groups: {e}")))?;
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::Scripted;
    use tempfile::tempdir;

    #[test]
    fn count_agent_groups_zero_on_fresh_db() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("copperclaw.db");
        let count = count_agent_groups(&path).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn count_agent_groups_reflects_inserts() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("copperclaw.db");
        let db = CentralDb::open(&path).unwrap();
        for i in 0..3 {
            agent_groups::create(
                &db,
                agent_groups::CreateAgentGroup {
                    name: format!("g{i}"),
                    folder: format!("f{i}"),
                    agent_provider: None,
                },
            )
            .unwrap();
        }
        drop(db);
        assert_eq!(count_agent_groups(&path).unwrap(), 3);
    }

    #[test]
    fn count_agent_groups_open_failure_is_other_error() {
        let dir = tempdir().unwrap();
        // Pass the directory itself to force an open error.
        let err = count_agent_groups(dir.path()).unwrap_err();
        assert!(matches!(err, StepError::Other(_)));
    }

    #[test]
    fn step_run_returns_count_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("copperclaw.db");
        // Pre-create the DB so the step can re-open it.
        let _ = CentralDb::open(&path).unwrap();
        let mut cfg = SetupConfig {
            central_db_path: path,
            ..SetupConfig::default()
        };
        let mut state = SetupState::new();
        let prompt = Scripted::new();
        let res = VerifyStep.run(&mut cfg, &prompt, &mut state).unwrap();
        assert!(res.messages[0].contains("agent_groups"));
    }

    #[test]
    fn step_metadata() {
        let s = VerifyStep;
        assert_eq!(s.name(), "verify");
        assert!(!s.description().is_empty());
        assert!(s.is_skippable());
    }
}
