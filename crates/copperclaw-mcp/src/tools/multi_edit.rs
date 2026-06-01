//! `multi_edit`: apply N find-replaces atomically to ONE file.
//!
//! This is the batched cousin of [`edit_file`](super::edit_file). The
//! agent supplies a list of `{old_string, new_string, replace_all?}`
//! entries; the tool applies each one to the *current* state of the
//! file (so a later edit can match text that an earlier edit just
//! introduced), then atomically writes the final result via the same
//! sibling-temp + fsync + rename dance.
//!
//! Why this exists at all: today an agent that needs to make five
//! tweaks to one file calls `edit_file` five times, and each call
//! drags along whatever overlapping context it needs to disambiguate
//! its `old_string`. That's a lot of repeated tokens for a problem
//! that's fundamentally one round-trip. `multi_edit` lets the model
//! pass each `old_string` exactly once and applies them in order.
//!
//! Atomicity: if *any* edit in the batch fails (no match, ambiguous
//! match without `replace_all`, etc.) the whole call fails and the
//! file on disk is unchanged. Edits are accumulated in memory and
//! only the final string is written.
//!
//! The 50-edit hard cap (also enforced by the JSON Schema) is a
//! belt-and-braces guard against a model that decides to "refactor
//! the file" by emitting a hundred individual swaps.

use std::path::{Path, PathBuf};

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

use crate::error::ToolError;
use crate::tools::diff_util::{build_blob_card, build_diff_card, over_blob_cutoff};
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};

/// Hard cap on the number of edits in a single call. Mirrored in the
/// JSON Schema's `maxItems` so the runtime check is just a backstop
/// for clients that ignore schema validation.
const MAX_EDITS: usize = 50;

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    edits: Vec<EditSpec>,
}

#[derive(Debug, Deserialize)]
struct EditSpec {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

pub fn schema() -> Tool {
    make_tool(
        "multi_edit",
        "Apply multiple find-replaces atomically to a single file. Use this instead of N sequential `edit_file` calls — the model passes each `old_string` only once instead of repeating overlapping context across separate tool calls. Same exact-match semantics as `edit_file`: each `old_string` must appear in the current file state (later edits see earlier edits applied), must be unique unless `replace_all: true`. Failed edits roll back the whole call — partial application never happens.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "edits"],
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 50,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["old_string", "new_string"],
                        "properties": {
                            "old_string":  { "type": "string", "minLength": 1 },
                            "new_string":  { "type": "string" },
                            "replace_all": { "type": "boolean" }
                        }
                    }
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

    if input.edits.is_empty() {
        return Err(ToolError::Validation(
            "`edits` must contain at least one entry".into(),
        ));
    }
    if input.edits.len() > MAX_EDITS {
        return Err(ToolError::Validation(format!(
            "`edits` capped at {MAX_EDITS} entries per call; got {}",
            input.edits.len()
        )));
    }

    // Per-edit invariants. We validate the whole batch up front so a
    // bad entry at index 7 doesn't trick us into doing the first six
    // swaps and then bailing — atomicity already guarantees the file
    // wouldn't change, but the error message is friendlier when it
    // points at the actual problem instead of "edit #7 happened to
    // be the one that hit the file".
    for (i, e) in input.edits.iter().enumerate() {
        if e.old_string.is_empty() {
            return Err(ToolError::Validation(format!(
                "edit #{i}: `old_string` must be non-empty"
            )));
        }
        if e.old_string == e.new_string {
            return Err(ToolError::Validation(format!(
                "edit #{i}: `old_string` and `new_string` must differ"
            )));
        }
    }

    let path = PathBuf::from(&input.path);
    let display = path.display().to_string();
    let edits = input.edits;

    // Same threading rationale as edit_file: read/write/rename/fsync
    // are synchronous and we don't want to fight `tokio::fs` for it.
    let result = tokio::task::spawn_blocking(move || do_multi_edit(&path, &edits))
        .await
        .map_err(|e| ToolError::Internal(format!("multi_edit({display}) join: {e}")))??;

    // Best-effort diff card — same pattern as `edit_file`. See
    // `slice-3-native-ui.md` § Surface 1.
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
        "edits_applied": result.edits_applied,
        "total_replacements": result.total_replacements,
    })))
}

struct MultiEditOutcome {
    path: String,
    edits_applied: usize,
    total_replacements: usize,
    /// Pre-edit content snapshot, kept so we can build a `DiffCard`
    /// after the atomic write succeeds.
    before: String,
    /// Post-edit content (the bytes the atomic rename just landed
    /// on disk).
    after: String,
}

fn do_multi_edit(path: &Path, edits: &[EditSpec]) -> Result<MultiEditOutcome, ToolError> {
    // Same symlink handling as edit_file: stat once, follow if the
    // entry is a symlink, then refuse anything that isn't a regular
    // file. We keep the *resolved* metadata around so we can restore
    // the permissions onto the temp file pre-rename.
    let lmeta = std::fs::symlink_metadata(path)
        .map_err(|e| ToolError::Internal(format!("multi_edit({}): stat: {e}", path.display())))?;
    let meta = if lmeta.file_type().is_symlink() {
        std::fs::metadata(path).map_err(|e| {
            ToolError::Internal(format!(
                "multi_edit({}): stat (resolved): {e}",
                path.display()
            ))
        })?
    } else {
        lmeta
    };
    if !meta.is_file() {
        return Err(ToolError::Validation(format!(
            "multi_edit({}): not a regular file",
            path.display()
        )));
    }

    let original = std::fs::read_to_string(path)
        .map_err(|e| ToolError::Internal(format!("multi_edit({}): read: {e}", path.display())))?;

    // Apply edits in order against a mutable buffer. Failures bail
    // out before we ever touch disk. We clone `original` here so the
    // pre-edit snapshot survives the loop — the diff card builder
    // wants both sides post-write.
    let mut current = original.clone();
    let mut total_replacements = 0usize;
    for (i, e) in edits.iter().enumerate() {
        let count = current.matches(&e.old_string).count();
        if count == 0 {
            return Err(ToolError::Validation(format!(
                "multi_edit({}): edit #{i}: `old_string` not found in current file state",
                path.display()
            )));
        }
        if !e.replace_all && count > 1 {
            return Err(ToolError::Validation(format!(
                "multi_edit({}): edit #{i}: `old_string` matches {count} times; include more surrounding context so the match is unique, or pass `replace_all: true`",
                path.display()
            )));
        }
        if e.replace_all {
            current = current.replace(&e.old_string, &e.new_string);
            total_replacements += count;
        } else {
            // count == 1 here, by the check above.
            current = current.replacen(&e.old_string, &e.new_string, 1);
            total_replacements += 1;
        }
    }

    atomic_write(path, current.as_bytes(), &meta)?;

    Ok(MultiEditOutcome {
        path: path.display().to_string(),
        edits_applied: edits.len(),
        total_replacements,
        before: original,
        after: current,
    })
}

/// Write `bytes` to `path` atomically: sibling temp file, fsync,
/// rename, with the original's mode restored onto the temp before
/// the rename. Lifted verbatim from `edit_file` so the two tools
/// share semantics — if/when this needs to move into a shared helper
/// it should move for both at once.
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
                "multi_edit({}): path has no file name component",
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
    let tmp = parent.join(format!(".{file_name}.multi_edit.{pid}.{nanos}.tmp"));

    let mut guard = TmpGuard(&tmp, false);

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| {
                ToolError::Internal(format!(
                    "multi_edit({}): open tmp {}: {e}",
                    path.display(),
                    tmp.display()
                ))
            })?;
        f.write_all(bytes).map_err(|e| {
            ToolError::Internal(format!("multi_edit({}): write tmp: {e}", path.display()))
        })?;
        f.sync_all().map_err(|e| {
            ToolError::Internal(format!("multi_edit({}): fsync tmp: {e}", path.display()))
        })?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = orig_meta.permissions().mode();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| {
            ToolError::Internal(format!("multi_edit({}): chmod tmp: {e}", path.display()))
        })?;
    }
    #[cfg(not(unix))]
    let _ = orig_meta;

    std::fs::rename(&tmp, path).map_err(|e| {
        ToolError::Internal(format!(
            "multi_edit({}): rename tmp -> target: {e}",
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
    fn two_edits_to_same_file_both_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "let x = 1;\nlet y = 2;\n").unwrap();

        let res = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                { "old_string": "let x = 1;", "new_string": "let x = 99;" },
                { "old_string": "let y = 2;", "new_string": "let y = 100;" },
            ],
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"edits_applied\": 2"), "got: {body}");
        assert!(body.contains("\"total_replacements\": 2"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "let x = 99;\nlet y = 100;\n");
    }

    #[test]
    fn one_failed_edit_leaves_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let original = "let x = 1;\nlet y = 2;\n";
        std::fs::write(&path, original).unwrap();

        // First edit succeeds, second has no match. The whole call
        // must fail and the file must be untouched.
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                { "old_string": "let x = 1;", "new_string": "let x = 99;" },
                { "old_string": "nope-not-here", "new_string": "x" },
            ],
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("not found"), "got: {msg}");
                assert!(msg.contains("edit #1"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        // File untouched on disk.
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, original);
    }

    #[test]
    fn later_edit_can_reference_earlier_edits_output() {
        // The second edit's old_string ("INTRODUCED") only exists
        // in the file *after* the first edit runs. This is the
        // headline behaviour: edits compose left-to-right against
        // the current buffer.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hello world\n").unwrap();

        let res = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                { "old_string": "world", "new_string": "INTRODUCED" },
                { "old_string": "INTRODUCED", "new_string": "rust" },
            ],
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"edits_applied\": 2"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "hello rust\n");
    }

    #[test]
    fn replace_all_flag_works_per_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "foo foo bar foo\nbaz\n").unwrap();

        let res = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                // First edit: replace_all sweeps every "foo".
                { "old_string": "foo", "new_string": "FOO", "replace_all": true },
                // Second edit: unique match (no flag needed).
                { "old_string": "baz", "new_string": "BAZ" },
            ],
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"edits_applied\": 2"), "got: {body}");
        // 3 from the replace_all + 1 from the unique swap.
        assert!(body.contains("\"total_replacements\": 4"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "FOO FOO bar FOO\nBAZ\n");
    }

    #[test]
    fn fifty_first_edit_is_rejected() {
        // Build a payload of 51 valid edits. Each is a unique
        // single-char swap so the payload would otherwise succeed —
        // it's the count that should bounce us. We don't even need
        // a real file because the cap check runs before we read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "x\n").unwrap();

        let mut edits = Vec::with_capacity(51);
        for i in 0..51 {
            edits.push(json!({
                "old_string": format!("placeholder-{i}"),
                "new_string": format!("replacement-{i}"),
            }));
        }

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "edits": edits,
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("capped at 50"), "got: {msg}");
                assert!(msg.contains("got 51"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        // File untouched.
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "x\n");
    }

    #[test]
    fn ambiguous_match_without_replace_all_rejects_whole_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let original = "x\nx\ny\n";
        std::fs::write(&path, original).unwrap();

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                // Valid first edit; would change "y" to "Y".
                { "old_string": "y", "new_string": "Y" },
                // Ambiguous second edit; no replace_all.
                { "old_string": "x", "new_string": "X" },
            ],
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("matches 2 times"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, original);
    }

    #[test]
    fn empty_edits_array_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hi\n").unwrap();

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [],
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn empty_old_string_rejected_with_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hi\n").unwrap();

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                { "old_string": "hi", "new_string": "ho" },
                { "old_string": "",   "new_string": "x" },
            ],
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("edit #1"), "got: {msg}");
                assert!(msg.contains("non-empty"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn old_equals_new_rejected_with_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hi\n").unwrap();

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                { "old_string": "hi", "new_string": "hi" },
            ],
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("edit #0"), "got: {msg}");
                assert!(msg.contains("must differ"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn rejects_when_path_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = call(&json!({
            "path": dir.path().to_string_lossy(),
            "edits": [
                { "old_string": "x", "new_string": "y" },
            ],
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
    fn rejects_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.rs");
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                { "old_string": "x", "new_string": "y" },
            ],
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[test]
    #[cfg(unix)]
    fn preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sh");
        std::fs::write(&path, "echo old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o741)).unwrap();

        call(&json!({
            "path": path.to_string_lossy(),
            "edits": [
                { "old_string": "old", "new_string": "new" },
            ],
        }))
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o741, "mode should survive the atomic rename");
    }

    #[test]
    fn schema_is_well_formed() {
        let s = schema();
        assert_eq!(s.name, "multi_edit");
        let body = serde_json::to_string(&s.input_schema).unwrap();
        assert!(body.contains("edits"));
        assert!(body.contains("old_string"));
        assert!(body.contains("new_string"));
        assert!(body.contains("replace_all"));
        assert!(body.contains("maxItems"));
    }
}
