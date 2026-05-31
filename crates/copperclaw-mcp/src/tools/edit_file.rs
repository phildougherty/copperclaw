//! `edit_file`: exact string-replacement edit on a file.
//!
//! Mirrors the semantics of Claude Code's `Edit` tool — the agent
//! supplies an `old_string` plus enough surrounding context that the
//! match is unique in the file, and the tool swaps it for `new_string`.
//! Uniqueness is the safety property: if the agent only included
//! `let x = 1;` and there were three such lines, blindly replacing
//! the first hit would corrupt the file. Forcing the agent to add
//! context until the match is unique is what makes the swap safe.
//!
//! Set `replace_all = true` to skip the uniqueness check and rewrite
//! every occurrence — the right shape for renames / refactors where
//! the agent *does* want to hit every site.
//!
//! Why this exists at all: today the agent reaches for `write_file`
//! for one-line tweaks, which means re-emitting the whole file on
//! every edit. That's token-expensive on large files and routinely
//! corrupts whitespace / trailing newlines as a side effect. A
//! string-level diff primitive removes both problems.
//!
//! Atomicity: writes go through a sibling temp file in the same
//! directory, fsync, then `rename(2)` — so a crash mid-write leaves
//! the original intact (the rename is the only mutating step that
//! the reader sees). Permissions are copied from the original onto
//! the temp file before the rename so the result keeps the original
//! mode.
//!
//! Sandboxing: matches the existing `read_file` / `write_file`
//! posture exactly — the container is the sandbox, the tool trusts
//! the agent inside it. `..`, `/proc/`, and absolute paths are all
//! permitted because the container's filesystem is ephemeral and
//! per-session. See `skills/write-file/SKILL.md` for the rationale.

use std::path::{Path, PathBuf};

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;

use crate::error::ToolError;
use crate::tools::diff_util::{build_blob_card, build_diff_card, over_blob_cutoff};
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

pub fn schema() -> Tool {
    make_tool(
        "edit_file",
        "Replace an exact substring inside an existing file. `old_string` must appear exactly once unless `replace_all` is true. Use `read_file` first to grab enough surrounding context to make `old_string` unique. Preserves the file's mode and writes atomically via a sibling temp file. Not for creating new files — use `write_file` for that.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "old_string", "new_string"],
            "properties": {
                "path":        { "type": "string", "minLength": 1 },
                "old_string":  { "type": "string", "minLength": 1 },
                "new_string":  { "type": "string" },
                "replace_all": { "type": "boolean" }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;

    if input.old_string == input.new_string {
        return Err(ToolError::Validation(
            "`old_string` and `new_string` must differ".into(),
        ));
    }
    if input.old_string.is_empty() {
        return Err(ToolError::Validation(
            "`old_string` must be non-empty".into(),
        ));
    }

    let path = PathBuf::from(&input.path);

    // Block on a thread — the read/write/rename/fsync sequence is
    // synchronous, and we want fs::OpenOptions + libc::fsync without
    // chasing async equivalents for every step.
    let display = path.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        do_edit(&path, &input.old_string, &input.new_string, input.replace_all)
    })
    .await
    .map_err(|e| ToolError::Internal(format!("edit_file({display}) join: {e}")))??;

    // Best-effort: compute a DiffCard from the pre/post snapshots and
    // hand it to the runner's emit_diff hook. The hook is a no-op on
    // contexts that don't surface diff cards (mock / subagent / etc).
    // We pass the `before_size` / `after_size` through `over_blob_cutoff`
    // first — a 4 MB file overwrite shouldn't try to compute a real
    // diff. See `slice-3-native-ui.md` § Surface 1 for the rationale.
    emit_diff_card(ctx, &result).await;

    Ok(success_json(&json!({
        "path": result.path,
        "replacements": result.replacements,
        "bytes_written": result.bytes_written,
    })))
}

struct EditOutcome {
    path: String,
    replacements: usize,
    bytes_written: usize,
    /// Pre-edit content (kept so the handler can compute a `DiffCard`
    /// after the atomic write succeeds). Always populated.
    before: String,
    /// Post-edit content (the bytes the atomic rename just landed on
    /// disk). Always populated.
    after: String,
}

async fn emit_diff_card(ctx: &dyn crate::context::ToolContext, r: &EditOutcome) {
    let before_size = r.before.len() as u64;
    let after_size = r.after.len() as u64;
    if over_blob_cutoff(before_size, after_size) {
        ctx.emit_diff(build_blob_card(&r.path, before_size, after_size))
            .await;
        return;
    }
    if let Some(card) = build_diff_card(&r.path, &r.before, &r.after) {
        ctx.emit_diff(card).await;
    }
}

fn do_edit(
    path: &Path,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<EditOutcome, ToolError> {
    // symlink_metadata so we can tell a symlink apart from a regular
    // file; we follow symlinks (matching the spec's "follow symlinks
    // but reject if target is non-regular") by then calling metadata
    // for the real check.
    let lmeta = std::fs::symlink_metadata(path).map_err(|e| {
        ToolError::Internal(format!(
            "edit_file({}): stat: {e}",
            path.display()
        ))
    })?;
    let meta = if lmeta.file_type().is_symlink() {
        std::fs::metadata(path).map_err(|e| {
            ToolError::Internal(format!(
                "edit_file({}): stat (resolved): {e}",
                path.display()
            ))
        })?
    } else {
        lmeta
    };
    if !meta.is_file() {
        return Err(ToolError::Validation(format!(
            "edit_file({}): not a regular file",
            path.display()
        )));
    }

    let original = std::fs::read_to_string(path).map_err(|e| {
        ToolError::Internal(format!(
            "edit_file({}): read: {e}",
            path.display()
        ))
    })?;

    let count = original.matches(old_string).count();
    if count == 0 {
        return Err(ToolError::Validation(format!(
            "edit_file({}): `old_string` not found",
            path.display()
        )));
    }
    if !replace_all && count > 1 {
        return Err(ToolError::Validation(format!(
            "edit_file({}): `old_string` matches {count} times; include more surrounding context so the match is unique, or pass `replace_all: true`",
            path.display()
        )));
    }

    let (new_content, replacements) = if replace_all {
        (original.replace(old_string, new_string), count)
    } else {
        // Single-shot replace; we already verified count == 1.
        (original.replacen(old_string, new_string, 1), 1)
    };

    atomic_write(path, new_content.as_bytes(), &meta)?;

    Ok(EditOutcome {
        path: path.display().to_string(),
        replacements,
        bytes_written: new_content.len(),
        before: original,
        after: new_content,
    })
}

/// Write `bytes` to `path` atomically: write to a sibling temp file,
/// fsync, then rename. Restore the original mode onto the temp file
/// before the rename so the resulting file keeps the original's
/// permissions.
///
/// We don't try to restore ownership (uid/gid) — that would require
/// `chown`, which needs `CAP_CHOWN` we don't reliably have inside the
/// container. The atomic rename keeps whatever ownership the temp
/// file inherits from the calling process, which is also what
/// `write_file` does today.
fn atomic_write(
    path: &Path,
    bytes: &[u8],
    orig_meta: &std::fs::Metadata,
) -> Result<(), ToolError> {
    use std::io::Write;

    // Cleanup guard: if anything below fails, drop the temp file.
    // Declared above the first statement to keep clippy::
    // items_after_statements happy.
    struct TmpGuard<'a>(&'a Path, bool);
    impl Drop for TmpGuard<'_> {
        fn drop(&mut self) {
            if !self.1 {
                let _ = std::fs::remove_file(self.0);
            }
        }
    }

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).map_or_else(
        || PathBuf::from("."),
        Path::to_path_buf,
    );
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            ToolError::Validation(format!(
                "edit_file({}): path has no file name component",
                path.display()
            ))
        })?
        .to_string_lossy()
        .into_owned();

    // Sibling temp name: same directory so `rename` stays on the
    // same filesystem (rename across mounts is EXDEV). Use pid + ns
    // timestamp to avoid collision with other in-flight edits.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let tmp = parent.join(format!(".{file_name}.edit_file.{pid}.{nanos}.tmp"));

    let mut guard = TmpGuard(&tmp, false);

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| {
                ToolError::Internal(format!(
                    "edit_file({}): open tmp {}: {e}",
                    path.display(),
                    tmp.display()
                ))
            })?;
        f.write_all(bytes).map_err(|e| {
            ToolError::Internal(format!(
                "edit_file({}): write tmp: {e}",
                path.display()
            ))
        })?;
        // fsync the contents before rename — without this a crash
        // between rename and journal flush can leave a zero-length
        // file in place of the original on ext4 with data=ordered.
        f.sync_all().map_err(|e| {
            ToolError::Internal(format!(
                "edit_file({}): fsync tmp: {e}",
                path.display()
            ))
        })?;
    }

    // Restore the original mode onto the temp file before the
    // rename, so readers never see a window with the wrong perms.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = orig_meta.permissions().mode();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(
            |e| {
                ToolError::Internal(format!(
                    "edit_file({}): chmod tmp: {e}",
                    path.display()
                ))
            },
        )?;
    }
    // On non-unix the platform's default copy of perms (or absence
    // thereof) is fine; the spec is unix-targeted.
    #[cfg(not(unix))]
    let _ = orig_meta;

    std::fs::rename(&tmp, path).map_err(|e| {
        ToolError::Internal(format!(
            "edit_file({}): rename tmp -> target: {e}",
            path.display()
        ))
    })?;
    guard.1 = true; // rename consumed the temp, no cleanup needed.

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

    /// Pull the first text block out of a `CallToolResult`.
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
        // Block on the future — the tool itself runs entirely on a
        // spawn_blocking thread, the outer handle is async only to
        // match the trait signature.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(handle(Some(map), ctx().as_ref()))
    }

    #[test]
    fn happy_path_replaces_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "let x = 1;\nlet y = 2;\n").unwrap();

        let res = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "let x = 1;",
            "new_string": "let x = 99;",
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"replacements\": 1"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "let x = 99;\nlet y = 2;\n");
    }

    #[test]
    fn rejects_when_old_string_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hello world\n").unwrap();

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "nope",
            "new_string": "x",
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("not found"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        // File untouched.
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "hello world\n");
    }

    #[test]
    fn rejects_ambiguous_match_without_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "x\nx\n").unwrap();

        let err = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "x",
            "new_string": "y",
        }))
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(msg.contains("matches 2 times"), "got: {msg}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
        // File untouched.
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "x\nx\n");
    }

    #[test]
    fn replace_all_rewrites_every_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "foo foo bar foo\n").unwrap();

        let res = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "foo",
            "new_string": "FOO",
            "replace_all": true,
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"replacements\": 3"), "got: {body}");

        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "FOO FOO bar FOO\n");
    }

    #[test]
    fn rejects_when_old_equals_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hi\n").unwrap();
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "hi",
            "new_string": "hi",
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn rejects_when_old_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hi\n").unwrap();
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "",
            "new_string": "x",
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn rejects_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.rs");
        let err = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "x",
            "new_string": "y",
        }))
        .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[test]
    fn rejects_when_path_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = call(&json!({
            "path": dir.path().to_string_lossy(),
            "old_string": "x",
            "new_string": "y",
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
    #[cfg(unix)]
    fn preserves_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.sh");
        std::fs::write(&path, "echo old\n").unwrap();
        // 0o741 — distinct from default 0o644 so the test is meaningful.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o741))
            .unwrap();

        call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "old",
            "new_string": "new",
        }))
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o741, "mode should survive the atomic rename");
    }

    #[test]
    fn write_failure_leaves_original_intact() {
        // Drop write permission from the parent directory so the
        // create_new of the temp file fails. Read still works
        // (the original file is owned by us and readable), and the
        // tool's pre-write checks succeed; only the actual write
        // fails. The original must be untouched and the temp must
        // not linger.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("a.rs");
            std::fs::write(&path, "let x = 1;\n").unwrap();
            // 0o555 = r-x for everyone, no write on the dir.
            let orig = std::fs::metadata(dir.path()).unwrap().permissions();
            std::fs::set_permissions(
                dir.path(),
                std::fs::Permissions::from_mode(0o555),
            )
            .unwrap();

            let err = call(&json!({
                "path": path.to_string_lossy(),
                "old_string": "let x = 1;",
                "new_string": "let x = 2;",
            }))
            .unwrap_err();
            assert!(matches!(err, ToolError::Internal(_)), "{err:?}");

            // Restore so tempdir cleanup works.
            std::fs::set_permissions(dir.path(), orig).unwrap();

            // Original file unchanged.
            let got = std::fs::read_to_string(&path).unwrap();
            assert_eq!(got, "let x = 1;\n");

            // No stray temp.
            let leftovers: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains(".edit_file."))
                .collect();
            assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
        }
    }

    #[test]
    fn replaces_inside_larger_unique_context() {
        // Show the "include context to disambiguate" idiom: bare
        // `return x;` would match twice, but with the surrounding
        // line it's unique.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(
            &path,
            "fn a() {\n    return x;\n}\nfn b() {\n    return x;\n}\n",
        )
        .unwrap();

        let res = call(&json!({
            "path": path.to_string_lossy(),
            "old_string": "fn b() {\n    return x;\n}",
            "new_string": "fn b() {\n    return x + 1;\n}",
        }))
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"replacements\": 1"), "got: {body}");
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            got,
            "fn a() {\n    return x;\n}\nfn b() {\n    return x + 1;\n}\n"
        );
    }

    #[test]
    fn schema_is_well_formed() {
        let s = schema();
        assert_eq!(s.name, "edit_file");
        let body = serde_json::to_string(&s.input_schema).unwrap();
        assert!(body.contains("old_string"));
        assert!(body.contains("new_string"));
        assert!(body.contains("replace_all"));
    }

    /// After a successful single edit the tool must emit one
    /// `DiffCard` via `ctx.emit_diff` carrying the structured diff.
    /// This is the slice-3.1 wiring contract: tests for the
    /// per-channel native renderers downstream rely on this card
    /// reaching the runner.
    #[test]
    fn successful_edit_emits_diff_card_via_ctx() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"old\");\n}\n").unwrap();

        let mock = Arc::new(crate::context::MockToolContext::new());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let args = json!({
            "path": path.to_string_lossy(),
            "old_string": "    println!(\"old\");",
            "new_string": "    println!(\"new\");",
        });
        let map = args.as_object().unwrap().clone();
        let ctx_ref: &dyn crate::context::ToolContext = mock.as_ref();
        rt.block_on(handle(Some(map), ctx_ref)).unwrap();

        let diffs = mock.diff_calls();
        assert_eq!(diffs.len(), 1, "exactly one diff card per successful edit");
        let card = &diffs[0];
        assert_eq!(card.path, path.display().to_string());
        assert_eq!(card.added, 1);
        assert_eq!(card.removed, 1);
        assert_eq!(card.language.as_deref(), Some("rust"));
    }

    /// Failed edits (no match, ambiguous match) must NOT emit a diff
    /// card — the file wasn't touched, there's nothing to surface.
    #[test]
    fn failed_edit_does_not_emit_diff_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "hello world\n").unwrap();

        let mock = Arc::new(crate::context::MockToolContext::new());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let args = json!({
            "path": path.to_string_lossy(),
            "old_string": "nope",
            "new_string": "x",
        });
        let map = args.as_object().unwrap().clone();
        let ctx_ref: &dyn crate::context::ToolContext = mock.as_ref();
        let res = rt.block_on(handle(Some(map), ctx_ref));
        assert!(res.is_err());
        assert!(mock.diff_calls().is_empty());
    }
}
