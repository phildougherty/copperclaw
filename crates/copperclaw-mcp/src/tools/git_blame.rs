//! `git_blame`: line-by-line blame for a file, in the line range the
//! agent asks about (default: whole file). Each row carries the
//! short SHA + author + RFC3339 date + the line text — enough for
//! the model to answer "who wrote this" without a second tool call.

use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

/// Hard ceiling on the number of blame rows we'll return in one call.
/// Beyond this the agent should narrow the range — blames are slow
/// and verbose.
const MAX_LINES: usize = 5_000;

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    path: Option<String>,
    file: String,
    #[serde(default)]
    from_line: Option<usize>,
    #[serde(default)]
    to_line: Option<usize>,
}

pub fn schema() -> Tool {
    make_tool(
        "git_blame",
        "Line-by-line blame for a file. Returns the commit short SHA, author, RFC 3339 date, and text of each line in the requested range. Out-of-range lines are clamped to the file's actual bounds.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["file"],
            "properties": {
                "path":      { "type": ["string", "null"] },
                "file":      { "type": "string", "minLength": 1 },
                "from_line": { "type": ["integer", "null"], "minimum": 1 },
                "to_line":   { "type": ["integer", "null"], "minimum": 1 }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    if input.file.trim().is_empty() {
        return Err(ToolError::Validation("`file` must be non-empty".into()));
    }
    let root = super::git_common::resolve_path(input.path.as_deref())?;
    let value = tokio::task::spawn_blocking(move || compute(&root, &input))
        .await
        .map_err(|e| ToolError::Internal(format!("git_blame join: {e}")))??;
    Ok(success_json(&value))
}

fn compute(root: &Path, input: &Input) -> Result<serde_json::Value, ToolError> {
    let repo = super::git_common::open_repo(root)?;
    let file = Path::new(&input.file);

    // Resolve the path relative to the working dir so the agent can
    // pass either an absolute path or a repo-relative one.
    let abs_path = if file.is_absolute() {
        file.to_path_buf()
    } else {
        repo.workdir()
            .map_or_else(|| root.join(file), |w| w.join(file))
    };

    // Read the file contents up-front so we can pair each blame hunk
    // with its line text. libgit2's blame gives us (orig_line ->
    // commit) but not the text — we have to splice that in ourselves.
    let contents = std::fs::read_to_string(&abs_path).map_err(|e| {
        // Map ENOENT / NotFound to Validation so the agent retries with
        // a different path rather than treating it as a server bug.
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolError::Validation(format!("git_blame: no such file `{}`", file.display()))
        } else {
            ToolError::Internal(format!("git_blame: read {}: {e}", file.display()))
        }
    })?;
    let lines: Vec<&str> = contents.lines().collect();
    let total_lines = lines.len();

    // Range clamp. Document: out-of-range bounds clamp; an inverted
    // range yields an empty result rather than an error so the agent
    // can recover without a roundtrip.
    let from = input.from_line.unwrap_or(1).max(1);
    let to = input.to_line.unwrap_or(total_lines.max(1));
    let from = from.min(total_lines.max(1));
    let to = to.min(total_lines.max(1));
    if from > to || total_lines == 0 {
        return Ok(json!({ "blame": [] }));
    }
    let span = to - from + 1;
    if span > MAX_LINES {
        return Err(ToolError::Validation(format!(
            "git_blame: requested range too large ({span} lines, max {MAX_LINES}); narrow `from_line`/`to_line`"
        )));
    }

    let rel = match repo.workdir() {
        Some(w) => abs_path.strip_prefix(w).unwrap_or(&abs_path).to_path_buf(),
        None => abs_path.clone(),
    };

    let mut opts = git2::BlameOptions::new();
    opts.min_line(from);
    opts.max_line(to);
    let blame = repo
        .blame_file(&rel, Some(&mut opts))
        .map_err(|e| super::git_common::map_err("git_blame: blame_file", &e))?;

    let mut out: Vec<serde_json::Value> = Vec::new();
    for line_no in from..=to {
        let Some(hunk) = blame.get_line(line_no) else {
            continue;
        };
        let oid = hunk.final_commit_id();
        // Lookup commit so we can extract author/date. If the commit
        // can't be found we still emit a row with the SHA so the agent
        // sees the blame; missing metadata is just empty strings.
        let (author, date) = match repo.find_commit(oid) {
            Ok(c) => (
                c.author().name().unwrap_or("").to_string(),
                super::git_common::time_to_rfc3339(c.time()),
            ),
            Err(_) => (String::new(), String::new()),
        };
        let text = lines.get(line_no - 1).copied().unwrap_or("").to_string();
        out.push(json!({
            "line": line_no,
            "sha": super::git_common::short(&oid),
            "author": author,
            "date": date,
            "text": text,
        }));
    }

    Ok(json!({ "blame": out }))
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
    async fn blames_each_line_to_a_commit() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "alpha\nbeta\n");
        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "file": "a.txt",
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
        let rows = v["blame"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["line"], 1);
        assert_eq!(rows[0]["text"], "alpha");
        assert_eq!(rows[0]["author"], "Test");
        assert_eq!(rows[1]["text"], "beta");
        assert!(rows[0]["sha"].as_str().unwrap().len() == 7);
    }

    #[tokio::test]
    async fn line_range_narrows_output() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(
            td.path(),
            "a.txt",
            "one\ntwo\nthree\nfour\n",
        );
        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "file": "a.txt",
                    "from_line": 2,
                    "to_line": 3,
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
        let rows = v["blame"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["line"], 2);
        assert_eq!(rows[0]["text"], "two");
        assert_eq!(rows[1]["text"], "three");
    }

    #[tokio::test]
    async fn out_of_range_to_line_clamps() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "x\n");
        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "file": "a.txt",
                    "from_line": 1,
                    "to_line": 999,
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
        let rows = v["blame"].as_array().unwrap();
        // Clamps to the single line in the file.
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn missing_file_is_validation_error() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "x\n");
        let err = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "file": "nope.txt",
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

    #[tokio::test]
    async fn inverted_range_returns_empty() {
        let td = tempfile::tempdir().unwrap();
        super::super::git_common::tests::init_with_commit(td.path(), "a.txt", "one\ntwo\n");
        let res = handle(
            Some(
                json!({
                    "path": td.path().to_string_lossy(),
                    "file": "a.txt",
                    "from_line": 2,
                    "to_line": 1,
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
        assert_eq!(v["blame"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn rejects_empty_file_arg() {
        let err = handle(
            Some(json!({"file": ""}).as_object().unwrap().clone()),
            &ctx(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
