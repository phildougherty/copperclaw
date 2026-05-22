//! `git_log`: enumerate commits reachable from a ref, with optional
//! `since` / file-scope filtering. Output is one JSON object per
//! commit so the model can reason structurally instead of parsing
//! `git log --oneline` text.

use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
// PathBuf is still used in `file_filter` / `path_matches_filter`.

/// Default page size; matches what most chat-style `git log` views
/// surface.
const DEFAULT_MAX_COUNT: usize = 20;
/// Hard ceiling so the model can't accidentally enumerate 50k commits.
const MAX_COUNT_CAP: usize = 200;

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    r#ref: Option<String>,
    #[serde(default)]
    max_count: Option<usize>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    files: Option<Vec<String>>,
}

pub fn schema() -> Tool {
    make_tool(
        "git_log",
        "List commits reachable from a ref (default HEAD). Optional `since` date filter and per-file scoping. Returns structured commit objects with sha / short_sha / author / email / RFC3339 date / subject / body / files_changed.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path":      { "type": ["string", "null"] },
                "ref":       { "type": ["string", "null"], "description": "Ref to start walking from (default HEAD)." },
                "max_count": { "type": ["integer", "null"], "minimum": 1, "maximum": MAX_COUNT_CAP },
                "since":     { "type": ["string", "null"], "description": "ISO date or RFC3339 timestamp; commits older than this are dropped." },
                "files":     { "type": ["array", "null"], "items": { "type": "string" } }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let root = super::git_common::resolve_path(input.path.as_deref())?;
    let value = tokio::task::spawn_blocking(move || compute(&root, &input))
        .await
        .map_err(|e| ToolError::Internal(format!("git_log join: {e}")))??;
    Ok(success_json(&value))
}

fn compute(root: &Path, input: &Input) -> Result<serde_json::Value, ToolError> {
    let repo = super::git_common::open_repo(root)?;

    let reference = input.r#ref.as_deref().unwrap_or("HEAD");
    // Empty repo (HEAD unborn) → graceful empty response, not error.
    if repo.head().is_err() && reference == "HEAD" {
        return Ok(json!({ "commits": [] }));
    }

    let start = super::git_common::resolve_commit(&repo, reference)?;

    let since_secs = match input.since.as_deref() {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };
    let file_filter: Option<Vec<PathBuf>> = input
        .files
        .as_ref()
        .map(|v| v.iter().map(PathBuf::from).collect());

    let max_count = input
        .max_count
        .unwrap_or(DEFAULT_MAX_COUNT)
        .clamp(1, MAX_COUNT_CAP);

    let mut walker = repo
        .revwalk()
        .map_err(|e| super::git_common::map_err("git_log: revwalk", &e))?;
    walker
        .push(start.id())
        .map_err(|e| super::git_common::map_err("git_log: push", &e))?;
    walker
        .set_sorting(git2::Sort::TIME)
        .map_err(|e| super::git_common::map_err("git_log: sort", &e))?;

    let mut out: Vec<serde_json::Value> = Vec::new();
    for oid_res in walker {
        let oid = match oid_res {
            Ok(o) => o,
            Err(e) => return Err(super::git_common::map_err("git_log: walk", &e)),
        };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        if let Some(min) = since_secs {
            if commit.time().seconds() < min {
                // Walker is TIME-ordered (newest first); once we see a
                // commit older than `since`, all subsequent ones are
                // also older. Break early to save work.
                break;
            }
        }
        let files_changed = files_changed_count(&repo, &commit)?;
        if let Some(filter) = &file_filter {
            if !commit_touches_any(&repo, &commit, filter)? {
                continue;
            }
        }

        let author = commit.author();
        let subject = commit
            .summary()
            .unwrap_or("")
            .to_string();
        let body = commit
            .message()
            .map(|m| {
                // `git2::Commit::message` returns the full message
                // including the subject line; trim it to get just the
                // body the agent wants.
                let after_subject = m.split_once('\n').map_or("", |x| x.1);
                after_subject.trim_start_matches('\n').to_string()
            })
            .unwrap_or_default();

        out.push(json!({
            "sha":        oid.to_string(),
            "short_sha":  super::git_common::short(&oid),
            "author":     author.name().unwrap_or("").to_string(),
            "email":      author.email().unwrap_or("").to_string(),
            "date":       super::git_common::time_to_rfc3339(commit.time()),
            "subject":    subject,
            "body":       body,
            "files_changed": files_changed,
        }));

        if out.len() >= max_count {
            break;
        }
    }

    Ok(json!({ "commits": out }))
}

/// Parse `since` strings the agent might supply. We accept:
/// - RFC 3339 (`2026-05-01T00:00:00Z`)
/// - ISO date (`2026-05-01`) — interpreted as midnight UTC.
fn parse_since(s: &str) -> Result<i64, ToolError> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = d
            .and_hms_opt(0, 0, 0)
            .map(|nd| nd.and_utc().timestamp());
        if let Some(ts) = dt {
            return Ok(ts);
        }
    }
    Err(ToolError::Validation(format!(
        "git_log: cannot parse `since`={s:?}; use ISO date YYYY-MM-DD or RFC 3339"
    )))
}

/// Count files changed in `commit` vs its first parent. Merge commits
/// are reported as 0 (no single canonical diff to attribute).
fn files_changed_count(
    repo: &git2::Repository,
    commit: &git2::Commit<'_>,
) -> Result<usize, ToolError> {
    if commit.parent_count() == 0 {
        // Initial commit: diff against empty tree.
        let tree = commit
            .tree()
            .map_err(|e| super::git_common::map_err("git_log: tree", &e))?;
        let diff = repo
            .diff_tree_to_tree(None, Some(&tree), None)
            .map_err(|e| super::git_common::map_err("git_log: diff", &e))?;
        return Ok(diff.deltas().len());
    }
    if commit.parent_count() > 1 {
        return Ok(0);
    }
    let parent = commit
        .parent(0)
        .map_err(|e| super::git_common::map_err("git_log: parent", &e))?;
    let a = parent
        .tree()
        .map_err(|e| super::git_common::map_err("git_log: parent tree", &e))?;
    let b = commit
        .tree()
        .map_err(|e| super::git_common::map_err("git_log: tree", &e))?;
    let diff = repo
        .diff_tree_to_tree(Some(&a), Some(&b), None)
        .map_err(|e| super::git_common::map_err("git_log: diff", &e))?;
    Ok(diff.deltas().len())
}

/// `true` if `commit`'s diff vs its first parent touches any path in
/// `filter`. Conservative substring/prefix match so the agent can
/// pass either `src/foo.rs` or `src/` and get the expected behavior.
fn commit_touches_any(
    repo: &git2::Repository,
    commit: &git2::Commit<'_>,
    filter: &[PathBuf],
) -> Result<bool, ToolError> {
    let new_tree = commit
        .tree()
        .map_err(|e| super::git_common::map_err("git_log: tree", &e))?;
    let old_tree = if commit.parent_count() == 0 {
        None
    } else {
        Some(
            commit
                .parent(0)
                .and_then(|p| p.tree())
                .map_err(|e| super::git_common::map_err("git_log: parent tree", &e))?,
        )
    };
    let diff = repo
        .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), None)
        .map_err(|e| super::git_common::map_err("git_log: diff", &e))?;
    for delta in diff.deltas() {
        for side in [delta.new_file().path(), delta.old_file().path()].iter().flatten() {
            if path_matches_filter(side, filter) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn path_matches_filter(p: &Path, filter: &[PathBuf]) -> bool {
    let s = p.to_string_lossy();
    filter.iter().any(|f| {
        let fs = f.to_string_lossy();
        s == fs || s.starts_with(&format!("{fs}/")) || s.contains(fs.as_ref())
    })
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
    use crate::context::MockToolContext;
    use serde_json::Value;

    fn ctx() -> MockToolContext {
        MockToolContext::new()
    }

    fn result_text(r: &CallToolResult) -> String {
        for c in &r.content {
            if let rmcp::model::RawContent::Text(t) = &c.raw {
                return t.text.clone();
            }
        }
        String::new()
    }

    fn parse(r: &CallToolResult) -> Value {
        serde_json::from_str(&result_text(r)).unwrap()
    }

    #[tokio::test]
    async fn lists_recent_commits_newest_first() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "1");
        let repo = git2::Repository::open(td.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        super::super::git_common::tests::rewrite_and_commit(&repo, "a.txt", "2", "second");

        let res = handle(
            Some(json!({"path": td.path().to_string_lossy()}).as_object().unwrap().clone()),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0]["subject"], "second");
        assert_eq!(commits[1]["subject"], "initial");
        assert!(commits[0]["short_sha"].as_str().unwrap().len() == 7);
    }

    #[tokio::test]
    async fn max_count_caps_results() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "0");
        let repo = git2::Repository::open(td.path()).unwrap();
        for i in 1..5 {
            super::super::git_common::tests::rewrite_and_commit(
                &repo,
                "a.txt",
                &i.to_string(),
                &format!("c{i}"),
            );
        }
        let res = handle(
            Some(
                json!({"path": td.path().to_string_lossy(), "max_count": 2})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        assert_eq!(v["commits"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn since_filter_drops_older_commits() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "1");
        let repo = git2::Repository::open(td.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        super::super::git_common::tests::rewrite_and_commit(&repo, "a.txt", "2", "second");

        // Choose a `since` of "now + 1 hour" — should drop both commits.
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "since": future.to_rfc3339(),
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        assert_eq!(v["commits"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn files_filter_restricts_to_touching_commits() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "1");
        let repo = git2::Repository::open(td.path()).unwrap();
        // Commit b.txt — does not touch a.txt.
        std::fs::write(td.path().join("b.txt"), "x").unwrap();
        super::super::git_common::tests::commit_existing(&repo, "b.txt", "add b");

        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "files": ["a.txt"],
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0]["subject"], "initial");
    }

    #[tokio::test]
    async fn empty_repo_yields_empty_commit_list() {
        let td = tempfile::tempdir().unwrap();
        git2::Repository::init(td.path()).unwrap();
        let res = handle(
            Some(json!({"path": td.path().to_string_lossy()}).as_object().unwrap().clone()),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        assert_eq!(v["commits"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn non_repo_errors() {
        let td = tempfile::tempdir().unwrap();
        let err = handle(
            Some(json!({"path": td.path().to_string_lossy()}).as_object().unwrap().clone()),
            &ctx(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn bad_since_is_validation_error() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "1");
        let err = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "since": "yesterday",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
