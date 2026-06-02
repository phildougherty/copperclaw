// Diff code intrinsically uses many old/new pairs (old_line/new_line,
// old_lines/new_lines, etc.) — the `similar_names` lint fires on every
// one. Suppress at module scope rather than spraying #[allow] across
// dozens of bindings. Same justification for `too_many_lines` on the
// unified-diff parser body — the natural shape is a single linear walk
// over the input and chopping it up hurts readability.
#![allow(clippy::similar_names, clippy::too_many_lines)]

//! `apply_patch`: apply a unified diff to a single file.
//!
//! Sibling to `edit_file` — where `edit_file` is the right primitive for
//! a single string-level swap, `apply_patch` is the right primitive for
//! a multi-region refactor. A unified diff carries the surrounding
//! context once per hunk; the equivalent sequence of `edit_file` calls
//! would repeat the same neighbourhood context in every `old_string`,
//! which gets quadratic on dense edits. For three-or-more changes in
//! one file we expect this to land at 3-10× more compact than the
//! `edit_file` sequence the agent would otherwise emit.
//!
//! Format: standard `diff -u`. Optional `--- a/path` / `+++ b/path`
//! file-level header lines (consumed and ignored — `path` is the
//! source of truth, see schema). Hunks start with
//! `@@ -OLDSTART,OLDLEN +NEWSTART,NEWLEN @@`; line counts after the
//! comma are optional (`@@ -50 +50 @@` is accepted as len=1). Each
//! hunk body is a run of lines prefixed with ` ` (context), `-`
//! (delete), or `+` (add).
//!
//! No fuzzy matching. Every context-or-delete line must match the
//! file's current contents at the claimed offset exactly, or the call
//! fails before any bytes are written.
//!
//! Atomicity: like `edit_file`, the write goes through a sibling temp
//! file, fsync, then `rename(2)`. If any hunk in the patch fails to
//! apply the file is left exactly as it was — we compute the full new
//! content in memory before touching the filesystem.

use std::path::{Path, PathBuf};

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

use crate::error::ToolError;
use crate::tools::diff_util::{build_blob_card, build_diff_card, over_blob_cutoff};
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    patch: String,
}

pub fn schema() -> Tool {
    make_tool(
        "apply_patch",
        "Apply a unified diff (output of `git diff` / `diff -u`) to a single file. For multi-region edits this is much more compact than a sequence of `edit_file` calls — the model writes a small diff instead of repeating overlapping `old_string` context. Hunks must apply cleanly — no fuzzy matching, exact-context required. Atomic: failure leaves the file untouched.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "patch"],
            "properties": {
                "path":  {
                    "type": "string",
                    "minLength": 1,
                    "description": "Target file path inside the container."
                },
                "patch": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Unified diff body. Optional --- /+++ header lines accepted but ignored — `path` is the source of truth. Hunks start with `@@ -OLDSTART,OLDLEN +NEWSTART,NEWLEN @@` (line counts optional). Lines prefixed with ` ` (context), `-` (delete), `+` (add)."
                }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;

    if input.patch.is_empty() {
        return Err(ToolError::Validation("`patch` must be non-empty".into()));
    }

    let path = PathBuf::from(&input.path);
    let display = path.display().to_string();
    let patch = input.patch.clone();

    let result = tokio::task::spawn_blocking(move || do_apply(&path, &patch))
        .await
        .map_err(|e| ToolError::Internal(format!("apply_patch({display}) join: {e}")))??;

    // Best-effort diff card — same pattern as `edit_file`.
    let before_size = result.before.len() as u64;
    let after_size = result.after.len() as u64;
    if over_blob_cutoff(before_size, after_size) {
        ctx.emit_diff(build_blob_card(&result.path, before_size, after_size))
            .await;
    } else if let Some(card) = build_diff_card(&result.path, &result.before, &result.after) {
        ctx.emit_diff(card).await;
    }

    Ok(success_json(&json!({
        "path": result.path,
        "hunks_applied": result.hunks_applied,
        "lines_added": result.lines_added,
        "lines_removed": result.lines_removed,
    })))
}

struct ApplyOutcome {
    path: String,
    hunks_applied: usize,
    lines_added: usize,
    lines_removed: usize,
    /// Pre-edit content snapshot, kept so we can build a `DiffCard`
    /// after the atomic write succeeds.
    before: String,
    /// Post-edit content (the bytes the atomic rename just landed
    /// on disk).
    after: String,
}

/// One parsed hunk. `old_start` is 1-based (matching unified diff
/// convention). `lines` are the raw body lines, each carrying its
/// leading `' '`, `'-'`, or `'+'`.
#[derive(Debug)]
struct Hunk {
    header: String,
    old_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Delete(String),
    Add(String),
}

/// Parse a unified-diff body into a list of hunks. Ignores optional
/// `---` / `+++` file-level header lines. Returns Validation errors
/// on malformed input.
///
/// Hand-rolled so we don't drag in a new dep just for `diff -u`
/// parsing, which is barely a parser.
fn parse_patch(patch: &str) -> Result<Vec<Hunk>, ToolError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    // Iterate by line, preserving the original separator semantics:
    // `lines()` strips trailing `\n` / `\r\n` which is what we want
    // because we'll re-join with `\n` when composing the new file.
    for (idx, raw) in patch.lines().enumerate() {
        // File-level headers — accepted but ignored, per spec.
        if raw.starts_with("--- ") || raw.starts_with("+++ ") {
            // Allowed only outside of a hunk body. Inside one, a `--- `
            // line would be ambiguous with `-`-prefixed delete lines
            // that happen to start with `-- `; we side-step that by
            // only treating it as a header when we're between hunks.
            if current.is_none() {
                continue;
            }
            // Inside an active hunk: fall through to the normal body
            // dispatcher below so it's treated as a delete line.
        }

        if let Some(rest) = raw.strip_prefix("@@") {
            // Stash the previous hunk if we were in one.
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            let (old_start, header_str) = parse_hunk_header(rest, raw, idx)?;
            current = Some(Hunk {
                header: header_str,
                old_start,
                lines: Vec::new(),
            });
            continue;
        }

        let Some(hunk) = current.as_mut() else {
            // Lines outside any hunk that aren't headers — tolerate
            // common leading metadata like `diff --git ...`, `index
            // ...`, `Binary files ...` by simply skipping. The model
            // is expected to emit hunks; anything else is noise.
            if raw.is_empty()
                || raw.starts_with("diff ")
                || raw.starts_with("index ")
                || raw.starts_with("Binary ")
                || raw.starts_with("\\ ")
            {
                continue;
            }
            // Unknown line outside a hunk → reject so we don't
            // silently mis-handle some weird format.
            return Err(ToolError::Validation(format!(
                "apply_patch: unexpected line {} outside any hunk: {raw:?}",
                idx + 1
            )));
        };

        // `\ No newline at end of file` is a unified-diff convention;
        // we don't model trailing-newline-or-not, so just drop it.
        if raw.starts_with("\\ ") {
            continue;
        }

        // Empty line inside a hunk body is treated as a context line
        // with no payload. Some tools emit `""` instead of `" "` for
        // truly empty context lines.
        if raw.is_empty() {
            hunk.lines.push(HunkLine::Context(String::new()));
            continue;
        }

        let (kind, payload) = raw.split_at(1);
        match kind {
            " " => hunk.lines.push(HunkLine::Context(payload.to_string())),
            "-" => hunk.lines.push(HunkLine::Delete(payload.to_string())),
            "+" => hunk.lines.push(HunkLine::Add(payload.to_string())),
            other => {
                return Err(ToolError::Validation(format!(
                    "apply_patch: unknown hunk line prefix {other:?} at line {}",
                    idx + 1
                )));
            }
        }
    }

    if let Some(h) = current.take() {
        hunks.push(h);
    }

    if hunks.is_empty() {
        return Err(ToolError::Validation(
            "apply_patch: patch contains no hunks".into(),
        ));
    }

    Ok(hunks)
}

/// Parse a hunk header line. `rest` is everything after the leading
/// `@@`. Returns `(old_start, full_header_for_errors)`.
///
/// Accepted shapes (leading/trailing whitespace tolerated):
///   ` -50,5 +50,7 @@ optional trailing context`
///   ` -50 +50 @@`
fn parse_hunk_header(rest: &str, full: &str, idx: usize) -> Result<(usize, String), ToolError> {
    let body = rest.trim_start();
    let close = body.find("@@").ok_or_else(|| {
        ToolError::Validation(format!(
            "apply_patch: malformed hunk header at line {}: missing closing `@@`",
            idx + 1
        ))
    })?;
    let inner = body[..close].trim();
    // Split on whitespace → ["-50,5", "+50,7"] (or len-1 variants).
    let mut parts = inner.split_ascii_whitespace();
    let old = parts.next().ok_or_else(|| {
        ToolError::Validation(format!(
            "apply_patch: malformed hunk header at line {}: missing old range",
            idx + 1
        ))
    })?;
    let _new = parts.next().ok_or_else(|| {
        ToolError::Validation(format!(
            "apply_patch: malformed hunk header at line {}: missing new range",
            idx + 1
        ))
    })?;
    let old_rest = old.strip_prefix('-').ok_or_else(|| {
        ToolError::Validation(format!(
            "apply_patch: malformed hunk header at line {}: old range must start with `-`",
            idx + 1
        ))
    })?;
    // Drop the optional `,LEN` — we don't actually rely on it; we
    // walk the body line-by-line.
    let old_start_str = old_rest.split(',').next().unwrap_or(old_rest);
    let old_start: usize = old_start_str.parse().map_err(|_| {
        ToolError::Validation(format!(
            "apply_patch: malformed hunk header at line {}: bad old start {old_start_str:?}",
            idx + 1
        ))
    })?;
    Ok((old_start, full.to_string()))
}

fn do_apply(path: &Path, patch: &str) -> Result<ApplyOutcome, ToolError> {
    let hunks = parse_patch(patch)?;

    // Stat first — same rules as edit_file. Follow symlinks; reject
    // non-regular targets.
    let lmeta = std::fs::symlink_metadata(path)
        .map_err(|e| ToolError::Internal(format!("apply_patch({}): stat: {e}", path.display())))?;
    let meta = if lmeta.file_type().is_symlink() {
        std::fs::metadata(path).map_err(|e| {
            ToolError::Internal(format!(
                "apply_patch({}): stat (resolved): {e}",
                path.display()
            ))
        })?
    } else {
        lmeta
    };
    if !meta.is_file() {
        return Err(ToolError::Validation(format!(
            "apply_patch({}): not a regular file",
            path.display()
        )));
    }

    let original = std::fs::read_to_string(path)
        .map_err(|e| ToolError::Internal(format!("apply_patch({}): read: {e}", path.display())))?;

    // Track whether the original ended in a newline so we can
    // reproduce that exactly. `split('\n')` gives us N+1 elements
    // when the input ends in `\n` (the last is `""`).
    let trailing_newline = original.ends_with('\n');
    let src_lines: Vec<&str> = if original.is_empty() {
        Vec::new()
    } else if trailing_newline {
        // Drop the trailing empty element from split — we'll re-add
        // the final newline when joining.
        let mut v: Vec<&str> = original.split('\n').collect();
        v.pop();
        v
    } else {
        original.split('\n').collect()
    };

    // `cursor` is a 0-based index into src_lines. Hunks describe
    // 1-based `old_start`. We walk forward only — hunks in a
    // unified diff are ordered by file offset.
    let mut cursor: usize = 0;
    let mut out: Vec<String> = Vec::with_capacity(src_lines.len());
    let mut lines_added: usize = 0;
    let mut lines_removed: usize = 0;

    for hunk in &hunks {
        // Convert 1-based to 0-based. `old_start = 0` is a special
        // case unified diff uses for "before line 1" (i.e. file is
        // empty / pure-add hunk); treat it as 0 in our 0-based
        // model.
        let target_idx = hunk.old_start.saturating_sub(1);

        if target_idx < cursor {
            return Err(ToolError::Validation(format!(
                "apply_patch: hunk {} goes backwards (old_start {}, cursor at line {})",
                hunk.header,
                hunk.old_start,
                cursor + 1
            )));
        }
        if target_idx > src_lines.len() {
            return Err(ToolError::Validation(format!(
                "apply_patch: hunk {} starts past end of file (old_start {}, file has {} lines)",
                hunk.header,
                hunk.old_start,
                src_lines.len()
            )));
        }

        // Copy unchanged lines up to the hunk's start.
        out.extend(
            src_lines[cursor..target_idx]
                .iter()
                .map(|s| (*s).to_string()),
        );
        cursor = target_idx;

        // Walk the hunk body.
        for line in &hunk.lines {
            match line {
                HunkLine::Context(expected) => {
                    let actual = src_lines.get(cursor).ok_or_else(|| {
                        ToolError::Validation(format!(
                            "hunk {}: expected context {expected:?} at line {}, found end of file",
                            hunk.header,
                            cursor + 1
                        ))
                    })?;
                    if *actual != expected.as_str() {
                        return Err(ToolError::Validation(format!(
                            "hunk {}: expected context {expected:?} at line {}, found {actual:?}",
                            hunk.header,
                            cursor + 1
                        )));
                    }
                    out.push(expected.clone());
                    cursor += 1;
                }
                HunkLine::Delete(expected) => {
                    let actual = src_lines.get(cursor).ok_or_else(|| {
                        ToolError::Validation(format!(
                            "hunk {}: expected delete {expected:?} at line {}, found end of file",
                            hunk.header,
                            cursor + 1
                        ))
                    })?;
                    if *actual != expected.as_str() {
                        return Err(ToolError::Validation(format!(
                            "hunk {}: expected delete {expected:?} at line {}, found {actual:?}",
                            hunk.header,
                            cursor + 1
                        )));
                    }
                    cursor += 1;
                    lines_removed += 1;
                }
                HunkLine::Add(payload) => {
                    out.push(payload.clone());
                    lines_added += 1;
                }
            }
        }
    }

    // Flush the rest of the file.
    out.extend(src_lines[cursor..].iter().map(|s| (*s).to_string()));

    let mut new_content = out.join("\n");
    if trailing_newline && !new_content.is_empty() {
        new_content.push('\n');
    } else if trailing_newline && new_content.is_empty() {
        // Original was a lone "\n"; preserve.
        new_content.push('\n');
    }

    atomic_write(path, new_content.as_bytes(), &meta)?;

    Ok(ApplyOutcome {
        path: path.display().to_string(),
        hunks_applied: hunks.len(),
        lines_added,
        lines_removed,
        before: original,
        after: new_content,
    })
}

/// Same atomic-write recipe as `edit_file`: sibling temp file, fsync,
/// rename. See `edit_file::atomic_write` for the rationale; kept
/// duplicated here so the two tools can evolve independently and
/// because the existing helper is private to that module.
fn atomic_write(path: &Path, bytes: &[u8], orig_meta: &std::fs::Metadata) -> Result<(), ToolError> {
    use std::io::Write;

    struct TmpGuard<'a>(&'a Path, bool);
    impl Drop for TmpGuard<'_> {
        fn drop(&mut self) {
            if !self.1 {
                let _ = std::fs::remove_file(self.0);
            }
        }
    }

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            ToolError::Validation(format!(
                "apply_patch({}): path has no file name component",
                path.display()
            ))
        })?
        .to_string_lossy()
        .into_owned();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let tmp = parent.join(format!(".{file_name}.apply_patch.{pid}.{nanos}.tmp"));

    let mut guard = TmpGuard(&tmp, false);

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| {
                ToolError::Internal(format!(
                    "apply_patch({}): open tmp {}: {e}",
                    path.display(),
                    tmp.display()
                ))
            })?;
        f.write_all(bytes).map_err(|e| {
            ToolError::Internal(format!("apply_patch({}): write tmp: {e}", path.display()))
        })?;
        f.sync_all().map_err(|e| {
            ToolError::Internal(format!("apply_patch({}): fsync tmp: {e}", path.display()))
        })?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = orig_meta.permissions().mode();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| {
            ToolError::Internal(format!("apply_patch({}): chmod tmp: {e}", path.display()))
        })?;
    }
    #[cfg(not(unix))]
    let _ = orig_meta;

    std::fs::rename(&tmp, path).map_err(|e| {
        ToolError::Internal(format!(
            "apply_patch({}): rename tmp -> target: {e}",
            path.display()
        ))
    })?;
    guard.1 = true;

    Ok(())
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

    fn result_text(r: &CallToolResult) -> String {
        for c in &r.content {
            if let rmcp::model::RawContent::Text(t) = &c.raw {
                return t.text.clone();
            }
        }
        String::new()
    }

    fn call(args: &serde_json::Value) -> Result<CallToolResult, ToolError> {
        let map = args.as_object().unwrap().clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle(Some(map), ctx().as_ref()))
    }

    #[test]
    fn happy_path_single_hunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let patch = "\
@@ -1,5 +1,5 @@
 line1
 line2
-line3
+LINE3
 line4
 line5
";
        let res = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"hunks_applied\": 1"), "got: {body}");
        assert!(body.contains("\"lines_added\": 1"), "got: {body}");
        assert!(body.contains("\"lines_removed\": 1"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "line1\nline2\nLINE3\nline4\nline5\n");
    }

    #[test]
    fn multi_hunk_patch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n").unwrap();

        let patch = "\
@@ -1,3 +1,3 @@
 1
-2
+TWO
 3
@@ -7,3 +7,3 @@
 7
-8
+EIGHT
 9
";
        let res = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"hunks_applied\": 2"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "1\nTWO\n3\n4\n5\n6\n7\nEIGHT\n9\n10\n");
    }

    #[test]
    fn add_only_hunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let patch = "\
@@ -1,3 +1,5 @@
 alpha
+inserted1
+inserted2
 beta
 gamma
";
        let res = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"lines_added\": 2"), "got: {body}");
        assert!(body.contains("\"lines_removed\": 0"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "alpha\ninserted1\ninserted2\nbeta\ngamma\n");
    }

    #[test]
    fn remove_only_hunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\n").unwrap();

        let patch = "\
@@ -1,4 +1,2 @@
 alpha
-beta
-gamma
 delta
";
        let res = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"lines_added\": 0"), "got: {body}");
        assert!(body.contains("\"lines_removed\": 2"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "alpha\ndelta\n");
    }

    #[test]
    fn mixed_add_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();

        let patch = "\
@@ -1,5 +1,6 @@
 a
-b
-c
+B
+C
+CC
 d
 e
";
        let res = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"lines_added\": 3"), "got: {body}");
        assert!(body.contains("\"lines_removed\": 2"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "a\nB\nC\nCC\nd\ne\n");
    }

    #[test]
    fn hunk_header_without_line_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "only\n").unwrap();

        let patch = "\
@@ -1 +1 @@
-only
+ONLY
";
        let res = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"hunks_applied\": 1"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "ONLY\n");
    }

    #[test]
    fn mismatched_context_atomic_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let original = "first\nsecond\nthird\n";
        std::fs::write(&path, original).unwrap();

        let patch = "\
@@ -1,3 +1,3 @@
 first
-WRONG
+second_changed
 third
";
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("@@"), "error should quote hunk header: {msg}");
                assert!(
                    msg.contains("WRONG"),
                    "error should mention expected: {msg}"
                );
                assert!(msg.contains("second"), "error should mention actual: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        // File untouched.
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, original);

        // No stray temp files.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".apply_patch."))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }

    #[test]
    fn header_lines_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

        let patch = "\
--- a/anywhere.txt
+++ b/anywhere.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
";
        let res = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"hunks_applied\": 1"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "one\nTWO\nthree\n");
    }

    #[test]
    fn empty_patch_is_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hi\n").unwrap();

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "patch": "",
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)), "{err:?}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "hi\n");
    }

    #[test]
    fn header_only_no_hunks_is_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hi\n").unwrap();

        let patch = "--- a/foo\n+++ b/foo\n";
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("no hunks"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "hi\n");
    }

    #[test]
    fn missing_target_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.txt");
        let patch = "\
@@ -1 +1 @@
-a
+b
";
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)), "{err:?}");
    }

    #[test]
    fn target_is_directory_errors() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
@@ -1 +1 @@
-a
+b
";
        let err = call(&json!({
            "path": dir.path().to_string_lossy(),
            "patch": patch,
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("not a regular file"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn preserves_no_trailing_newline() {
        // File doesn't end in `\n`; result shouldn't either.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "alpha\nbeta").unwrap();

        let patch = "\
@@ -1,2 +1,2 @@
 alpha
-beta
+BETA
";
        call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap();

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "alpha\nBETA");
    }

    #[test]
    fn rejects_unknown_prefix_in_hunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\n").unwrap();

        // `*` is not a valid hunk line prefix.
        let patch = "\
@@ -1 +1 @@
*x
+y
";
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)), "{err:?}");
    }

    #[test]
    fn malformed_hunk_header_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x\n").unwrap();

        let patch = "\
@@ this is not a hunk header @@
-x
+y
";
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "patch": patch,
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)), "{err:?}");
    }

    #[test]
    fn schema_is_well_formed() {
        let s = schema();
        assert_eq!(s.name, "apply_patch");
        let body = serde_json::to_string(&s.input_schema).unwrap();
        assert!(body.contains("path"));
        assert!(body.contains("patch"));
    }
}
