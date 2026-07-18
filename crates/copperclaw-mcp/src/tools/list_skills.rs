//! `list_skills`: enumerate the skills available to this session (M22 S3).
//!
//! The read-only companion to [`crate::tools::load_skill`] and
//! [`crate::tools::save_skill`]: it lists the skills the host selected for
//! this session — one entry per skill with its `name`, `version`, and
//! `description` — so the agent can see what it can `load_skill` (or re-save
//! with `save_skill`) without guessing names.
//!
//! Like `load_skill`, the source of truth is the per-session skills catalogue
//! the host writes next to `runner.json` (`/data/skills.json`) in
//! **callable** skills mode. In **inline** mode (the default) every selected
//! skill body is already in the system prompt and no catalogue file exists —
//! `list_skills` then returns an empty list plus an explanatory `note` rather
//! than an error, so the agent gets a clean, actionable answer either way.
//!
//! The tool is intentionally read-only and side-effect-free, so it is safe to
//! expose unconditionally (it rides the `READONLY_TOOLS` policy tier). It takes
//! no arguments.

use std::path::PathBuf;

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Serialize;
use serde_json::{Value, json};

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, success_json};

/// Default location of the per-session skills catalogue. Matches
/// `copperclaw_host::container_manager::SKILLS_CATALOGUE_FILENAME`, the
/// bind-mount target in the container, and `load_skill`'s reader.
const SKILLS_CATALOGUE_DEFAULT_PATH: &str = "/data/skills.json";

/// In-process test override for the catalogue path. Production reads the
/// default path; tests install their own tempfile so we avoid `unsafe`
/// env-var mutation (forbidden by the workspace lint). Mirrors the same
/// pattern `load_skill` uses.
#[cfg(test)]
static SKILLS_CATALOGUE_TEST_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn skills_catalogue_test_override_set(path: PathBuf) {
    let cell = SKILLS_CATALOGUE_TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
}

#[cfg(test)]
fn skills_catalogue_test_override_clear() {
    if let Some(cell) = SKILLS_CATALOGUE_TEST_OVERRIDE.get() {
        *cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
fn skills_catalogue_test_override() -> Option<PathBuf> {
    SKILLS_CATALOGUE_TEST_OVERRIDE.get().and_then(|m| {
        m.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    })
}

#[cfg(not(test))]
fn skills_catalogue_test_override() -> Option<PathBuf> {
    None
}

fn catalogue_path() -> PathBuf {
    if let Some(p) = skills_catalogue_test_override() {
        return p;
    }
    PathBuf::from(SKILLS_CATALOGUE_DEFAULT_PATH)
}

/// One row in the `list_skills` response.
#[derive(Debug, Serialize)]
struct SkillSummary {
    /// Kebab-case skill name (the slug `load_skill` / `save_skill` take).
    name: String,
    /// Effective skill version (catalogue `version`, default 1 when absent).
    version: u64,
    /// One-line skill description.
    description: String,
}

/// The `list_skills` tool result.
#[derive(Debug, Serialize)]
struct ListSkillsResult {
    /// The selected skills, sorted by name.
    skills: Vec<SkillSummary>,
    /// Present only when there is a reason the list is empty that the agent
    /// should understand (inline-skills mode, or a genuinely empty catalogue).
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn schema() -> Tool {
    make_tool(
        "list_skills",
        "List the skills available to this session: each entry is a skill's \
         `name`, `version`, and `description`. Read-only and side-effect-free \
         — use it to discover which skills you can `load_skill` (or re-save with \
         `save_skill`) instead of guessing names. Takes no arguments. In \
         inline-skills mode the skill bodies are already in your system prompt \
         and this returns an empty list with an explanatory note.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    )
}

pub async fn handle(
    _arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let path = catalogue_path();
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => {
            // M22 S3 metric: a catalogue-backed (callable-mode) list.
            copperclaw_metrics::inc_skills_listed("catalogue");
            b
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Inline-skills mode: no catalogue on disk. Return an empty list
            // with a note rather than an error — a "list" verb answering
            // "nothing enumerable here, and here's why" is friendlier and more
            // actionable than a failure.
            // M22 S3 metric: an inline-mode (no catalogue) list.
            copperclaw_metrics::inc_skills_listed("inline_empty");
            return Ok(success_json(&ListSkillsResult {
                skills: Vec::new(),
                note: Some(format!(
                    "no skills catalogue at {} — this host is running in inline-skills mode, so any selected skills are already spliced into your system prompt (load_skill/list_skills are not needed to see them)",
                    path.display()
                )),
            }));
        }
        Err(err) => {
            return Err(ToolError::Internal(format!(
                "could not read skills catalogue at {}: {err}",
                path.display()
            )));
        }
    };

    let entries: Vec<Value> = serde_json::from_slice(&bytes).map_err(|err| {
        ToolError::Internal(format!(
            "skills catalogue at {} did not parse as JSON: {err}",
            path.display()
        ))
    })?;

    let mut skills: Vec<SkillSummary> = entries
        .iter()
        .filter_map(|e| {
            let name = e.get("name").and_then(Value::as_str)?.to_string();
            let description = e
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // The catalogue may not carry a version yet (the host writer is
            // out of this card's scope); default to 1, matching the
            // frontmatter default so the surface is stable regardless.
            let version = e.get("version").and_then(Value::as_u64).unwrap_or(1);
            Some(SkillSummary {
                name,
                version,
                description,
            })
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    let note = if skills.is_empty() {
        Some(format!(
            "the skills catalogue at {} is empty — no skills are selected for this session",
            path.display()
        ))
    } else {
        None
    };

    Ok(success_json(&ListSkillsResult { skills, note }))
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

pub fn entry() -> ToolEntry {
    ToolEntry {
        tool: schema(),
        handler: Box::new(Handler),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockToolContext;
    use std::sync::{Mutex, OnceLock};

    /// Serialise tests so the process-global override doesn't get clobbered
    /// across parallel test threads.
    fn catalogue_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// RAII guard that points `catalogue_path()` at a tempfile for the guard's
    /// lifetime and clears the override on drop.
    struct CatalogueGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl CatalogueGuard {
        fn new(json_body: &str) -> Self {
            let lock = catalogue_env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("skills.json");
            std::fs::write(&path, json_body).unwrap();
            skills_catalogue_test_override_set(path);
            Self {
                _dir: dir,
                _lock: lock,
            }
        }

        /// Point the override at a deliberately-missing path (inline-mode case).
        fn missing() -> Self {
            let lock = catalogue_env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("missing.json");
            skills_catalogue_test_override_set(path);
            Self {
                _dir: dir,
                _lock: lock,
            }
        }
    }

    impl Drop for CatalogueGuard {
        fn drop(&mut self) {
            skills_catalogue_test_override_clear();
        }
    }

    fn result_json(r: &CallToolResult) -> Value {
        let text = r
            .content
            .iter()
            .filter_map(|c| {
                let raw = serde_json::to_value(c).ok()?;
                raw.get("text")?.as_str().map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str(&text).expect("result is JSON")
    }

    #[tokio::test]
    async fn lists_catalogue_skills_with_version_and_description() {
        let _g = CatalogueGuard::new(
            r#"[
                {"name": "zed", "description": "the zed skill", "body": "b", "version": 3},
                {"name": "alpha", "description": "the alpha skill", "body": "b"}
            ]"#,
        );
        let ctx = MockToolContext::new();
        let res = handle(None, &ctx).await.unwrap();
        assert_eq!(res.is_error, Some(false));
        let v = result_json(&res);
        let skills = v.get("skills").and_then(Value::as_array).unwrap();
        assert_eq!(skills.len(), 2);
        // Sorted by name: alpha first (defaults to version 1), then zed.
        assert_eq!(skills[0]["name"], "alpha");
        assert_eq!(skills[0]["version"], 1);
        assert_eq!(skills[0]["description"], "the alpha skill");
        assert_eq!(skills[1]["name"], "zed");
        assert_eq!(skills[1]["version"], 3);
        // No note on a non-empty list.
        assert!(v.get("note").is_none());
    }

    #[tokio::test]
    async fn inline_mode_returns_empty_list_with_note() {
        let _g = CatalogueGuard::missing();
        let ctx = MockToolContext::new();
        let res = handle(None, &ctx).await.unwrap();
        assert_eq!(res.is_error, Some(false));
        let v = result_json(&res);
        assert!(v["skills"].as_array().unwrap().is_empty());
        assert!(v["note"].as_str().unwrap().contains("inline-skills mode"));
    }

    #[tokio::test]
    async fn empty_catalogue_reports_no_skills_selected() {
        let _g = CatalogueGuard::new("[]");
        let ctx = MockToolContext::new();
        let res = handle(None, &ctx).await.unwrap();
        let v = result_json(&res);
        assert!(v["skills"].as_array().unwrap().is_empty());
        assert!(v["note"].as_str().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn malformed_catalogue_json_is_an_error() {
        let _g = CatalogueGuard::new("not json");
        let ctx = MockToolContext::new();
        let err = handle(None, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[test]
    fn entry_returns_tool_with_correct_name_and_no_required_args() {
        let e = entry();
        assert_eq!(e.tool.name.as_ref(), "list_skills");
        let v: serde_json::Value = serde_json::to_value(&*e.tool.input_schema).unwrap();
        // No required arguments.
        assert!(v.get("required").is_none());
    }
}
