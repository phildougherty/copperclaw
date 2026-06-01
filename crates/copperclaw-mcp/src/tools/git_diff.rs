//! `git_diff`: produce a unified diff between two refs (or, by default,
//! the working tree vs the index). Output mirrors `git diff` text the
//! agent already knows how to read, but with a stable JSON envelope:
//! `{diff, truncated, files_changed: [{path, additions, deletions}]}`.

use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

/// Default byte cap; comfortably above a typical multi-file commit
/// without burning the whole context window on whitespace churn.
const DEFAULT_MAX_BYTES: usize = 200_000;
/// Hard ceiling; the agent can ask for more but never above 1 MiB.
const MAX_BYTES_CAP: usize = 1024 * 1024;
/// Default context lines, matches `git diff`.
const DEFAULT_CONTEXT: u32 = 3;
/// Cap context lines so a 1k-context request can't blow the output.
const MAX_CONTEXT: u32 = 100;

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    files: Option<Vec<String>>,
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

pub fn schema() -> Tool {
    make_tool(
        "git_diff",
        "Produce a unified diff between two refs, or — if both `from` and `to` are omitted — the working-tree diff (uncommitted changes). Output includes the full diff text plus a per-file additions/deletions summary. Capped at 200 KiB by default.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path":      { "type": ["string", "null"] },
                "from":      { "type": ["string", "null"], "description": "Base ref (default: working tree)." },
                "to":        { "type": ["string", "null"], "description": "Tip ref (default: working tree)." },
                "files":     { "type": ["array", "null"], "items": { "type": "string" } },
                "context":   { "type": ["integer", "null"], "minimum": 0, "maximum": MAX_CONTEXT },
                "max_bytes": { "type": ["integer", "null"], "minimum": 1, "maximum": MAX_BYTES_CAP }
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
        .map_err(|e| ToolError::Internal(format!("git_diff join: {e}")))??;
    Ok(success_json(&value))
}

fn compute(root: &Path, input: &Input) -> Result<serde_json::Value, ToolError> {
    let repo = super::git_common::open_repo(root)?;
    let max_bytes = input
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(1, MAX_BYTES_CAP);
    let context = input.context.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT);

    let mut opts = git2::DiffOptions::new();
    opts.context_lines(context);
    if let Some(files) = &input.files {
        for f in files {
            opts.pathspec(f);
        }
    }

    let diff = build_diff(&repo, input, &mut opts)?;
    let (buf, summary, truncated) = render_diff(&diff, max_bytes)?;

    let diff_text = String::from_utf8_lossy(&buf).into_owned();
    let diff_text = if truncated {
        format!("{diff_text}\n[…truncated]")
    } else {
        diff_text
    };

    let files_changed: Vec<serde_json::Value> = summary
        .into_iter()
        .map(|(path, (adds, dels))| {
            json!({
                "path": path,
                "additions": adds,
                "deletions": dels,
            })
        })
        .collect();

    Ok(json!({
        "diff": diff_text,
        "truncated": truncated,
        "files_changed": files_changed,
    }))
}

/// Pick the right libgit2 diff function for the requested mode.
///
/// Selection matrix:
/// - `from` + `to` set     → tree-to-tree.
/// - `from` set only       → tree-to-workdir (compare base against working tree).
/// - `to` set only         → workdir-to-tree (mirror image; libgit2 doesn't
///   distinguish, both walk the same delta set).
/// - neither set           → index-to-workdir (plain `git diff`).
fn build_diff<'r>(
    repo: &'r git2::Repository,
    input: &Input,
    opts: &mut git2::DiffOptions,
) -> Result<git2::Diff<'r>, ToolError> {
    match (input.from.as_deref(), input.to.as_deref()) {
        (Some(from), Some(to)) => {
            let a = super::git_common::resolve_commit(repo, from)?;
            let b = super::git_common::resolve_commit(repo, to)?;
            repo.diff_tree_to_tree(
                Some(&a.tree().unwrap()),
                Some(&b.tree().unwrap()),
                Some(opts),
            )
            .map_err(|e| super::git_common::map_err("git_diff: tree-to-tree", &e))
        }
        (Some(from), None) => {
            let a = super::git_common::resolve_commit(repo, from)?;
            repo.diff_tree_to_workdir_with_index(Some(&a.tree().unwrap()), Some(opts))
                .map_err(|e| super::git_common::map_err("git_diff: tree-to-workdir", &e))
        }
        (None, Some(to)) => {
            let b = super::git_common::resolve_commit(repo, to)?;
            repo.diff_tree_to_workdir_with_index(Some(&b.tree().unwrap()), Some(opts))
                .map_err(|e| super::git_common::map_err("git_diff: workdir-to-tree", &e))
        }
        (None, None) => repo
            .diff_index_to_workdir(None, Some(opts))
            .map_err(|e| super::git_common::map_err("git_diff: index-to-workdir", &e)),
    }
}

type DiffSummary = std::collections::BTreeMap<String, (usize, usize)>;

/// Walk a `git2::Diff` once, building both the patch text (capped at
/// `max_bytes`) and the per-file additions/deletions summary. Returns
/// `(patch_bytes, summary, truncated_flag)`.
fn render_diff(
    diff: &git2::Diff<'_>,
    max_bytes: usize,
) -> Result<(Vec<u8>, DiffSummary, bool), ToolError> {
    let mut summary: DiffSummary = DiffSummary::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut over_cap = false;

    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let entry = summary.entry(path).or_insert((0, 0));
        match line.origin() {
            '+' => entry.0 += 1,
            '-' => entry.1 += 1,
            _ => {}
        }
        if !over_cap {
            // Build the textual patch with the origin-marker char `git
            // diff` would emit. 'F' (file header) / 'H' (hunk header)
            // lines already include the marker in `content`; don't
            // double it.
            let marker = match line.origin() {
                ' ' | '+' | '-' => Some(line.origin() as u8),
                _ => None,
            };
            if let Some(c) = marker {
                buf.push(c);
            }
            buf.extend_from_slice(line.content());
            if buf.len() >= max_bytes {
                over_cap = true;
            }
        }
        true
    })
    .map_err(|e| super::git_common::map_err("git_diff: print", &e))?;

    if over_cap && buf.len() > max_bytes {
        buf.truncate(max_bytes);
    }
    Ok((buf, summary, over_cap))
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
    async fn working_tree_diff_after_modify() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "one\n");
        std::fs::write(td.path().join("a.txt"), "one\ntwo\n").unwrap();

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
        let diff = v["diff"].as_str().unwrap();
        assert!(diff.contains("+two"), "diff: {diff}");
        let fc = v["files_changed"].as_array().unwrap();
        assert_eq!(fc.len(), 1);
        assert_eq!(fc[0]["path"], "a.txt");
        assert_eq!(fc[0]["additions"], 1);
    }

    #[tokio::test]
    async fn ref_to_ref_diff() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "v1\n");
        let repo = git2::Repository::open(td.path()).unwrap();
        super::super::git_common::tests::rewrite_and_commit(&repo, "a.txt", "v2\n", "bump");

        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "from": "HEAD~1",
                    "to": "HEAD",
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
        let diff = v["diff"].as_str().unwrap();
        assert!(diff.contains("-v1"), "diff: {diff}");
        assert!(diff.contains("+v2"), "diff: {diff}");
        let fc = v["files_changed"].as_array().unwrap();
        assert_eq!(fc[0]["additions"], 1);
        assert_eq!(fc[0]["deletions"], 1);
    }

    #[tokio::test]
    async fn max_bytes_truncation() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "big.txt", "x\n");
        // Append a lot of new content.
        let new = "y\n".repeat(50_000);
        std::fs::write(td.path().join("big.txt"), new).unwrap();

        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "max_bytes": 1024,
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
        assert_eq!(v["truncated"], true);
    }

    #[tokio::test]
    async fn no_changes_yields_empty_diff() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "one\n");
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
        assert_eq!(v["truncated"], false);
        assert_eq!(v["diff"], "");
        assert_eq!(v["files_changed"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn bad_ref_is_validation_error() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "1");
        let err = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "from": "definitely-not-a-ref",
                    "to": "HEAD",
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
