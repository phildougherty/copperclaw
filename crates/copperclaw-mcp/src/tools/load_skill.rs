//! `load_skill`: retrieve a skill's full body on demand.
//!
//! In `Inline` skills mode (the default), every selected skill's
//! `SKILL.md` body is already part of the agent's system prompt — this
//! tool is unnecessary and returns a clear "no catalogue" error if
//! called.
//!
//! In `Callable` mode the host writes a per-session `skills.json` next
//! to `runner.json` containing one entry per selected skill
//! (`{name, description, body}`). The system prompt only carries a
//! name+description index; this tool reads `skills.json` and returns the
//! matching body so the agent can act on the skill before deciding which
//! tool to call.
//!
//! The tool is intentionally read-only and side-effect-free, so it's
//! safe to expose unconditionally. Whether it succeeds or not depends on
//! whether the host wrote `skills.json` for this session.
//!
//! See `crates/copperclaw-host/src/container_manager.rs` for the
//! catalogue-writing side (`build_skills_catalogue` /
//! `SKILLS_CATALOGUE_FILENAME`).

use std::path::PathBuf;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args};

/// Default location of the per-session skills catalogue. Matches
/// `copperclaw_host::container_manager::SKILLS_CATALOGUE_FILENAME` and
/// the bind-mount target in the container.
const SKILLS_CATALOGUE_DEFAULT_PATH: &str = "/data/skills.json";

/// In-process test override for the catalogue path. Production reads
/// the default path; tests install their own tempfile so we avoid
/// `unsafe` env-var mutation (forbidden by the workspace lint).
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

/// Entity-encode `&` and `"` for safe inclusion in an XML-style attribute.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

#[derive(Debug, Deserialize)]
struct Input {
    /// Skill name (the kebab-case slug from the SKILL.md `name`
    /// frontmatter, i.e. what appears in the system-prompt index).
    name: String,
}

pub fn schema() -> Tool {
    make_tool(
        "load_skill",
        "Return the full SKILL.md body for the named skill. Use this when the system prompt shows only a skill index (name + description) and you want to read the underlying instructions before acting. Errors if the per-session skills catalogue is unavailable or the name is unknown.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The skill's kebab-case name (matches the SKILL.md frontmatter `name`)."
                }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ToolError::Validation("`name` must be non-empty".into()));
    }

    let path = catalogue_path();
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolError::Internal(format!(
                "skills catalogue not found at {} — this host is running in inline-skills mode, so skill bodies are already in your system prompt and load_skill is not needed",
                path.display()
            )));
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

    if entries.is_empty() {
        return Err(ToolError::Validation(format!(
            "the skills catalogue at {} is empty for this session — no skills are selected",
            path.display()
        )));
    }

    let entry = entries
        .iter()
        .find(|e| e.get("name").and_then(Value::as_str) == Some(name))
        .ok_or_else(|| {
            let known: Vec<String> = entries
                .iter()
                .filter_map(|e| e.get("name").and_then(Value::as_str).map(str::to_string))
                .collect();
            ToolError::Validation(format!(
                "no skill named `{name}` in catalogue (known: {})",
                known.join(", ")
            ))
        })?;

    let description = entry
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let body = entry
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Internal(format!("catalogue entry for `{name}` has no body")))?;

    let rendered = format!(
        "<skill name=\"{}\" description=\"{}\">\n{}\n</skill>",
        escape_attr(name),
        escape_attr(description),
        body.trim_end()
    );
    Ok(CallToolResult::success(vec![Content::text(rendered)]))
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

    /// Serialise tests so the process-global override doesn't get
    /// clobbered across parallel test threads.
    fn catalogue_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// RAII guard that points `catalogue_path()` at a tempfile for the
    /// guard's lifetime and clears the override on drop.
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

        /// Point the override at a deliberately-missing path. Used for
        /// the "no catalogue" error-message test.
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

    #[allow(clippy::unnecessary_wraps)]
    fn arg(name: &str) -> Option<JsonObject> {
        let mut map = JsonObject::default();
        map.insert("name".into(), serde_json::Value::from(name));
        Some(map)
    }

    fn extract_text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| {
                let raw = serde_json::to_value(c).ok()?;
                raw.get("text")?.as_str().map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn returns_body_for_known_skill() {
        let _g = CatalogueGuard::new(
            r#"[
                {"name": "alpha", "description": "the alpha skill", "body": "Alpha body line 1\nAlpha body line 2"},
                {"name": "beta", "description": "the beta skill", "body": "Beta body"}
            ]"#,
        );
        let ctx = MockToolContext::new();
        let result = handle(arg("alpha"), &ctx).await.unwrap();
        let text = extract_text(&result);
        assert!(text.contains("<skill name=\"alpha\""));
        assert!(text.contains("the alpha skill"));
        assert!(text.contains("Alpha body line 1"));
        assert!(text.contains("Alpha body line 2"));
        assert!(text.contains("</skill>"));
        // Beta body must not bleed in — we asked for alpha.
        assert!(!text.contains("Beta body"));
    }

    #[tokio::test]
    async fn errors_with_useful_message_when_name_missing_from_catalogue() {
        let _g = CatalogueGuard::new(r#"[{"name": "alpha", "description": "", "body": ""}]"#);
        let ctx = MockToolContext::new();
        let err = handle(arg("nonexistent"), &ctx).await.unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("nonexistent"));
                // The error should list the names we *do* know so the
                // model can self-correct in the next call.
                assert!(msg.contains("alpha"));
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_when_catalogue_missing_with_explanatory_message() {
        let _g = CatalogueGuard::missing();
        let ctx = MockToolContext::new();
        let err = handle(arg("alpha"), &ctx).await.unwrap_err();
        match err {
            ToolError::Internal(msg) => {
                assert!(msg.contains("inline-skills mode"));
                assert!(msg.contains("not needed"));
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errors_when_catalogue_is_malformed_json() {
        let _g = CatalogueGuard::new("not json");
        let ctx = MockToolContext::new();
        let err = handle(arg("alpha"), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[tokio::test]
    async fn rejects_empty_name() {
        let _g = CatalogueGuard::new("[]");
        let ctx = MockToolContext::new();
        let err = handle(arg(""), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn description_special_chars_are_escaped_in_response() {
        let _g = CatalogueGuard::new(
            r#"[{"name": "alpha", "description": "uses \"quotes\" & ampersand", "body": "body"}]"#,
        );
        let ctx = MockToolContext::new();
        let result = handle(arg("alpha"), &ctx).await.unwrap();
        let text = extract_text(&result);
        assert!(text.contains("&quot;quotes&quot;"));
        assert!(text.contains("&amp;"));
    }

    #[tokio::test]
    async fn name_special_chars_are_escaped_in_response() {
        // kebab-case validation upstream prevents these names in practice,
        // but the rendering layer should escape symmetrically with `description`.
        let _g =
            CatalogueGuard::new(r#"[{"name": "weird\"&name", "description": "d", "body": "b"}]"#);
        let ctx = MockToolContext::new();
        let result = handle(arg("weird\"&name"), &ctx).await.unwrap();
        let text = extract_text(&result);
        assert!(
            text.contains("name=\"weird&quot;&amp;name\""),
            "got: {text}"
        );
        assert!(!text.contains("name=\"weird\"&name\""), "got: {text}");
    }

    #[tokio::test]
    async fn empty_catalogue_reports_no_skills_selected() {
        let _g = CatalogueGuard::new("[]");
        let ctx = MockToolContext::new();
        let err = handle(arg("alpha"), &ctx).await.unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("empty"), "got: {msg}");
                assert!(msg.contains("no skills are selected"), "got: {msg}");
                // The misleading "(known: )" tail must not appear.
                assert!(!msg.contains("(known: "), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn entry_returns_tool_with_correct_name() {
        let e = entry();
        assert_eq!(e.tool.name.as_ref(), "load_skill");
    }
}
