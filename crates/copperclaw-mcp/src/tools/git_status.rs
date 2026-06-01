//! `git_status`: report the working-tree state of a git repository as
//! structured JSON, including branch / upstream tracking and per-file
//! status flags. Backed by `git2` (libgit2) so no shelling out.
//!
//! Output shape (stable):
//!
//! ```json
//! {
//!   "branch":    "main",
//!   "ahead":     3,
//!   "behind":    0,
//!   "clean":     false,
//!   "staged":    [{"path": "src/foo.rs", "status": "M"}],
//!   "unstaged":  [{"path": "src/bar.rs", "status": "M"}],
//!   "untracked": ["scratch.txt"]
//! }
//! ```

use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    path: Option<String>,
}

pub fn schema() -> Tool {
    make_tool(
        "git_status",
        "Inspect a git repository's working-tree state. Returns the current branch, ahead/behind counts vs upstream, and per-file staged / unstaged / untracked lists. Read-only — does not commit, push, or modify anything.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": ["string", "null"],
                    "description": "Path inside (or pointing at) a git repo. Defaults to the current working directory."
                }
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
    // libgit2 calls are blocking; offload to the blocking pool so the
    // MCP server's async runtime doesn't stall on a slow disk.
    let value = tokio::task::spawn_blocking(move || compute(&root))
        .await
        .map_err(|e| ToolError::Internal(format!("git_status join: {e}")))??;
    Ok(success_json(&value))
}

fn compute(root: &Path) -> Result<serde_json::Value, ToolError> {
    let repo = super::git_common::open_repo(root)?;

    // Branch name. HEAD may be unborn (fresh `git init`) or detached;
    // both are valid states the agent might encounter.
    let (branch, head_resolves) = match repo.head() {
        Ok(head) => {
            if head.is_branch() {
                let name = head.shorthand().unwrap_or("HEAD").to_string();
                (name, true)
            } else {
                // Detached HEAD — report the short OID so the agent can
                // see what it's looking at.
                let short = head
                    .target()
                    .map_or_else(|| "HEAD".to_string(), |oid| short_oid(&oid.to_string()));
                (format!("HEAD detached at {short}"), true)
            }
        }
        Err(_) => ("(unborn)".to_string(), false),
    };

    // Ahead/behind vs upstream. Only meaningful when we have a branch
    // *and* it has an upstream configured.
    let (ahead, behind) = if head_resolves {
        upstream_delta(&repo).unwrap_or((0, 0))
    } else {
        (0, 0)
    };

    // Per-file status. libgit2's `Status` is a bitfield; we collapse
    // into the porcelain-style M/A/D/R/T/? letters the agent already
    // knows from `git status --short`.
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .include_ignored(false)
        .include_unmodified(false)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| super::git_common::map_err("git_status: statuses", &e))?;

    let mut staged: Vec<serde_json::Value> = Vec::new();
    let mut unstaged: Vec<serde_json::Value> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();

    for entry in statuses.iter() {
        let path = entry.path().unwrap_or("").to_string();
        let flags = entry.status();
        if path.is_empty() {
            continue;
        }
        if flags.is_wt_new() && !flags.is_index_new() {
            // Pure untracked: not staged, not in HEAD.
            untracked.push(path.clone());
            continue;
        }
        // Index (staged) component.
        if let Some(letter) = index_letter(flags) {
            staged.push(json!({ "path": path.clone(), "status": letter }));
        }
        // Working-tree (unstaged) component.
        if let Some(letter) = worktree_letter(flags) {
            unstaged.push(json!({ "path": path.clone(), "status": letter }));
        }
    }

    let clean = staged.is_empty() && unstaged.is_empty() && untracked.is_empty();

    Ok(json!({
        "branch": branch,
        "ahead": ahead,
        "behind": behind,
        "clean": clean,
        "staged": staged,
        "unstaged": unstaged,
        "untracked": untracked,
    }))
}

/// Compute ahead/behind vs the current branch's upstream. Returns
/// `None` if there is no upstream (e.g. branch never pushed).
fn upstream_delta(repo: &git2::Repository) -> Option<(usize, usize)> {
    let head = repo.head().ok()?;
    let local_oid = head.target()?;
    // `branch_upstream_name` returns the full refname of the configured
    // upstream, e.g. `refs/remotes/origin/main`.
    let head_name = head.name()?;
    let upstream_ref = repo
        .branch_upstream_name(head_name)
        .ok()?
        .as_str()
        .map(String::from)?;
    let upstream_oid = repo.refname_to_id(&upstream_ref).ok()?;
    let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid).ok()?;
    Some((ahead, behind))
}

/// Map the index-side bits of a `Status` to a porcelain letter.
fn index_letter(s: git2::Status) -> Option<&'static str> {
    if s.is_index_new() {
        Some("A")
    } else if s.is_index_modified() {
        Some("M")
    } else if s.is_index_deleted() {
        Some("D")
    } else if s.is_index_renamed() {
        Some("R")
    } else if s.is_index_typechange() {
        Some("T")
    } else {
        None
    }
}

/// Map the worktree-side bits of a `Status` to a porcelain letter.
/// `?` is only emitted for files that are *not* also staged; the
/// caller handles that pure-untracked case separately.
fn worktree_letter(s: git2::Status) -> Option<&'static str> {
    if s.is_wt_modified() {
        Some("M")
    } else if s.is_wt_deleted() {
        Some("D")
    } else if s.is_wt_renamed() {
        Some("R")
    } else if s.is_wt_typechange() {
        Some("T")
    } else if s.is_wt_new() {
        Some("?")
    } else {
        None
    }
}

fn short_oid(oid_hex: &str) -> String {
    oid_hex.chars().take(7).collect()
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
    async fn reports_clean_after_initial_commit() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "README.md", "hello");

        let res = handle(
            Some(
                json!({"path": td.path().to_string_lossy()})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        assert_eq!(v["clean"], true);
        assert_eq!(v["staged"].as_array().unwrap().len(), 0);
        assert_eq!(v["unstaged"].as_array().unwrap().len(), 0);
        assert_eq!(v["untracked"].as_array().unwrap().len(), 0);
        assert!(v["branch"].as_str().is_some());
    }

    #[tokio::test]
    async fn reports_untracked_file() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "README.md", "hello");
        std::fs::write(td.path().join("scratch.txt"), b"hi").unwrap();

        let res = handle(
            Some(
                json!({"path": td.path().to_string_lossy()})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        assert_eq!(v["clean"], false);
        let untracked: Vec<String> = serde_json::from_value(v["untracked"].clone()).unwrap();
        assert!(untracked.iter().any(|p| p == "scratch.txt"));
    }

    #[tokio::test]
    async fn reports_unstaged_modification() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "README.md", "hello");
        std::fs::write(td.path().join("README.md"), b"changed").unwrap();

        let res = handle(
            Some(
                json!({"path": td.path().to_string_lossy()})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        assert_eq!(v["clean"], false);
        let unstaged = v["unstaged"].as_array().unwrap();
        assert_eq!(unstaged.len(), 1);
        assert_eq!(unstaged[0]["path"], "README.md");
        assert_eq!(unstaged[0]["status"], "M");
    }

    #[tokio::test]
    async fn errors_on_non_repo() {
        let td = tempfile::tempdir().unwrap();
        let err = handle(
            Some(
                json!({"path": td.path().to_string_lossy()})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(err.to_string().contains("not a git repo"));
    }

    #[tokio::test]
    async fn handles_empty_repo_gracefully() {
        let td = tempfile::tempdir().unwrap();
        git2::Repository::init(td.path()).unwrap();
        let res = handle(
            Some(
                json!({"path": td.path().to_string_lossy()})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            &ctx(),
        )
        .await
        .unwrap();
        let v = parse(&res);
        // Empty repo: HEAD unborn, clean, nothing tracked.
        assert_eq!(v["branch"], "(unborn)");
        assert_eq!(v["clean"], true);
    }
}
