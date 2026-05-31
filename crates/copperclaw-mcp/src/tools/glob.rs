//! `glob`: list files under a root matching a gitignore-style glob.
//!
//! Pairs with `grep` — when the agent knows the *kind* of file it
//! wants (`**/*.toml`, `crates/**/src/lib.rs`) but not which exact
//! path holds it, `glob` returns the candidate list cheaply. Then
//! the agent feeds those paths into `read_file` or `grep` as needed.
//!
//! Like `grep`, traversal honours `.gitignore` (via `ignore::WalkBuilder`)
//! and unconditionally skips `target/`, `node_modules/`, and `.git/`.
//! Results are sorted lexicographically so callers can snapshot them
//! reliably.

use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Default cap on returned paths.
const DEFAULT_MAX_RESULTS: usize = 1000;
/// Hard ceiling on returned paths. Above this the caller should
/// narrow the pattern.
const MAX_RESULTS_CEILING: usize = 10_000;
/// Directories we refuse to enter unconditionally (build / vendor /
/// VCS noise). Mirrors `grep::HARD_SKIP_DIRS`.
const HARD_SKIP_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// JSON-RPC input.
#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    /// When `true`, the walker ignores `.gitignore` and friends.
    /// The hard-skip list (`target/`, `node_modules/`, `.git/`)
    /// still applies.
    #[serde(default)]
    no_ignore: bool,
}

/// Output envelope.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
struct Output {
    paths: Vec<String>,
    truncated: bool,
    total_matched: usize,
}

pub fn schema() -> Tool {
    make_tool(
        "glob",
        "List files under `path` (default cwd) matching a gitignore-style glob (e.g. `**/*.rs`). Honours .gitignore by default; skips target/, node_modules/, .git/ unconditionally. Returns sorted paths.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["pattern"],
            "properties": {
                "pattern":     { "type": "string", "minLength": 1 },
                "path":        { "type": ["string", "null"] },
                "max_results": { "type": ["integer", "null"], "minimum": 1, "maximum": 10000 },
                "no_ignore":   { "type": "boolean" }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    if input.pattern.trim().is_empty() {
        return Err(ToolError::Validation("`pattern` must be non-empty".into()));
    }

    let matcher = globset::GlobBuilder::new(&input.pattern)
        .literal_separator(false)
        .build()
        .map_err(|e| {
            ToolError::Validation(format!("invalid glob `{}`: {e}", input.pattern))
        })?
        .compile_matcher();

    let root = match input.path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()
            .map_err(|e| ToolError::Internal(format!("cwd unavailable: {e}")))?,
    };
    if !root.exists() {
        return Err(ToolError::Validation(format!(
            "path does not exist: {}",
            root.display()
        )));
    }
    // When the caller passed an absolute path, return absolute
    // results; otherwise return paths relative to the search
    // root. Matches the contract documented in the schema.
    let return_absolute = input
        .path
        .as_deref()
        .is_some_and(|p| Path::new(p).is_absolute());

    let max_results = input
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CEILING);
    let no_ignore = input.no_ignore;

    let output = tokio::task::spawn_blocking(move || {
        run_glob(&root, &matcher, max_results, no_ignore, return_absolute)
    })
    .await
    .map_err(|e| ToolError::Internal(format!("glob task panicked: {e}")))?;

    Ok(success_json(&output))
}

fn run_glob(
    root: &Path,
    matcher: &globset::GlobMatcher,
    max_results: usize,
    no_ignore: bool,
    return_absolute: bool,
) -> Output {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(!no_ignore)
        .git_ignore(!no_ignore)
        .git_exclude(!no_ignore)
        .git_global(!no_ignore)
        .ignore(!no_ignore)
        .hidden(!no_ignore)
        .parents(!no_ignore)
        .follow_links(false)
        .filter_entry(|entry| {
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    if HARD_SKIP_DIRS.contains(&name) {
                        return false;
                    }
                }
            }
            true
        });

    let walker = builder.build();

    let mut all: Vec<String> = Vec::new();
    let mut total_matched: usize = 0;
    let mut truncated = false;

    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);

        // Test both the relative path and the bare filename so
        // callers can write either `**/*.rs` or `*.rs` without
        // worrying about the difference.
        let name_match = path
            .file_name()
            .is_some_and(|n| matcher.is_match(Path::new(n)));
        if !matcher.is_match(rel) && !name_match {
            continue;
        }

        total_matched += 1;
        if all.len() < max_results {
            let display = if return_absolute {
                path.display().to_string()
            } else {
                rel.display().to_string()
            };
            all.push(display);
        } else {
            truncated = true;
        }
    }

    all.sort();

    Output {
        paths: all,
        truncated,
        total_matched,
    }
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn crate::context::ToolContext,
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
    use serde_json::json;
    use std::sync::Arc;

    fn ctx() -> Arc<dyn crate::context::ToolContext> {
        Arc::new(crate::context::MockToolContext::new())
    }

    fn args(v: serde_json::Value) -> Option<JsonObject> {
        match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    fn result_text(r: &CallToolResult) -> String {
        for c in &r.content {
            if let rmcp::model::RawContent::Text(t) = &c.raw {
                return t.text.clone();
            }
        }
        String::new()
    }

    fn parse_output(r: &CallToolResult) -> Output {
        let txt = result_text(r);
        serde_json::from_str::<Output>(&txt)
            .unwrap_or_else(|e| panic!("output not JSON-parsable: {e}\nbody: {txt}"))
    }

    #[tokio::test]
    async fn happy_path_sorted_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.rs"), "").unwrap();

        let res = handle(
            args(json!({
                "pattern": "**/*.rs",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.paths.len(), 3);
        assert_eq!(out.total_matched, 3);
        assert!(!out.truncated);
        // Confirm sort.
        let mut sorted = out.paths.clone();
        sorted.sort();
        assert_eq!(out.paths, sorted);
    }

    #[tokio::test]
    async fn max_results_caps_output() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            std::fs::write(dir.path().join(format!("f{i}.rs")), "").unwrap();
        }
        let res = handle(
            args(json!({
                "pattern": "**/*.rs",
                "path": dir.path().to_string_lossy(),
                "max_results": 10,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.paths.len(), 10);
        assert!(out.truncated);
        assert_eq!(out.total_matched, 50);
    }

    #[tokio::test]
    async fn gitignore_honored() {
        let dir = tempfile::tempdir().unwrap();
        // `.gitignore` is only honoured by `ignore` inside a git
        // repo; `.ignore` is the always-honoured sibling file the
        // crate provides for the non-git case.
        std::fs::write(dir.path().join(".ignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "").unwrap();
        let res = handle(
            args(json!({
                "pattern": "*.rs",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.paths.len(), 1);
        assert!(out.paths[0].contains("kept.rs"));
    }

    #[tokio::test]
    async fn no_matches_returns_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only.txt"), "").unwrap();
        let res = handle(
            args(json!({
                "pattern": "*.rs",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.paths.len(), 0);
        assert_eq!(out.total_matched, 0);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn missing_path_errors() {
        let err = handle(
            args(json!({
                "pattern": "*.rs",
                "path": "/nonexistent/path/xyz12345",
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn invalid_glob_errors() {
        let err = handle(
            args(json!({
                "pattern": "[invalid",
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("[invalid"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hard_skip_dirs_apply_even_with_no_ignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/build.rs"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules/foo.rs"), "").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "").unwrap();

        let res = handle(
            args(json!({
                "pattern": "**/*.rs",
                "path": dir.path().to_string_lossy(),
                "no_ignore": true,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.paths.len(), 1);
        assert!(out.paths[0].contains("kept.rs"));
    }

    #[tokio::test]
    async fn bare_glob_matches_filename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/a.toml"), "").unwrap();
        std::fs::write(dir.path().join("b.toml"), "").unwrap();

        let res = handle(
            args(json!({
                "pattern": "*.toml",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.paths.len(), 2, "got: {:?}", out.paths);
    }

    #[test]
    fn schema_declares_required_fields() {
        let s = schema();
        let schema: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
    }
}
