//! `copy_file`: duplicate a file inside the container without the
//! bytes ever passing through the model.
//!
//! The naive way to clone a template today is `read_file(src)` →
//! `write_file(dst, content=…)`. That round-trips every byte through
//! the model's context window twice — a 50 KiB template burns ~100
//! KiB of tokens per copy, and `write_file` is UTF-8 only so it can't
//! handle binary payloads at all. This tool side-steps both problems:
//! the host performs the copy at the filesystem layer and the model
//! sees only a small JSON receipt `{src, dst, bytes_copied}`.
//!
//! Same 32 MB hard ceiling as `send_file`'s `path` branch — see
//! [`COPY_FILE_MAX_BYTES`].

use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

/// Hard ceiling on the source file size, matching `send_file`'s `path`
/// branch. Above this the operation is rejected outright — anything
/// bigger should go through `shell` with `cp`.
const COPY_FILE_MAX_BYTES: u64 = 32 * 1024 * 1024; // 32 MB

#[derive(Debug, Deserialize)]
struct Input {
    src: String,
    dst: String,
    #[serde(default)]
    create_parents: bool,
    #[serde(default)]
    overwrite: bool,
}

pub fn schema() -> Tool {
    make_tool(
        "copy_file",
        "Copy a file from `src` to `dst` inside the container. Use this instead of `read_file` + `write_file` for any duplicate — the host transfers the bytes directly and the model never sees them. For binary files (images, archives), this is the ONLY safe way to copy them since `write_file` is UTF-8 only. Hard size ceiling of 32 MB. Pass `overwrite: true` to replace an existing destination; pass `create_parents: true` to mkdir -p the destination's parent.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["src", "dst"],
            "properties": {
                "src":            { "type": "string", "minLength": 1, "description": "Source path inside the container." },
                "dst":            { "type": "string", "minLength": 1, "description": "Destination path. Must not equal src." },
                "create_parents": { "type": "boolean", "description": "Create parent directories of dst if missing. Default false." },
                "overwrite":      { "type": "boolean", "description": "Overwrite dst if it already exists. Default false (errors out)." }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let src = PathBuf::from(&input.src);
    let dst = PathBuf::from(&input.dst);

    // 1. src != dst. We compare the raw strings *and* the constructed
    //    paths so trivial differences (e.g. trailing slashes on a
    //    file path) still fail validation up-front. A canonicalize-
    //    based comparison would be more robust but also slower and
    //    would require both paths to already exist — dst doesn't.
    if src == dst || input.src == input.dst {
        return Err(ToolError::Validation(format!(
            "copy_file: `src` and `dst` must differ (both = `{}`)",
            src.display()
        )));
    }

    // 2. src must exist and be a regular file. `symlink_metadata`
    //    refuses to follow symlinks; we then explicitly reject any
    //    non-regular file (directory, symlink, socket, …). This means
    //    a symlink to a regular file is also rejected — copying a
    //    symlink would either duplicate the link or the target
    //    depending on platform, and "ambiguous" is worse than "say
    //    no" for a tool the model invokes blind.
    let src_meta = tokio::fs::symlink_metadata(&src).await.map_err(|e| {
        ToolError::Validation(format!(
            "copy_file: stat `{}` failed: {e}",
            src.display()
        ))
    })?;
    if !src_meta.file_type().is_file() {
        return Err(ToolError::Validation(format!(
            "copy_file: `{}` is not a regular file",
            src.display()
        )));
    }

    // 3. 32 MB hard ceiling. Matches `send_file`'s `path` branch.
    if src_meta.len() > COPY_FILE_MAX_BYTES {
        return Err(ToolError::Validation(format!(
            "copy_file: `{}` is {} bytes; max is {COPY_FILE_MAX_BYTES}",
            src.display(),
            src_meta.len()
        )));
    }

    // 4. dst existence × overwrite policy. `symlink_metadata` again
    //    so a stale symlink at `dst` doesn't sneak past the check.
    match tokio::fs::symlink_metadata(&dst).await {
        Ok(_) => {
            if !input.overwrite {
                return Err(ToolError::Validation(format!(
                    "copy_file: `{}` already exists; pass `overwrite: true` to replace",
                    dst.display()
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ToolError::Internal(format!(
                "copy_file: stat `{}` failed: {e}",
                dst.display()
            )));
        }
    }

    // 5. mkdir -p the destination's parent when requested. If the
    //    parent is missing and `create_parents` is false we just let
    //    `tokio::fs::copy` fail with the underlying I/O error — that
    //    surfaces as `ToolError::Internal`, mirroring `write_file`'s
    //    behaviour.
    if input.create_parents {
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    ToolError::Internal(format!(
                        "copy_file({}): create_dir_all({}): {e}",
                        dst.display(),
                        parent.display()
                    ))
                })?;
            }
        }
    }

    // 6. Filesystem-level copy. `tokio::fs::copy` is a thin wrapper
    //    around `std::fs::copy`, which on Linux uses `copy_file_range`
    //    when available and preserves the source's permission bits.
    let bytes_copied = tokio::fs::copy(&src, &dst).await.map_err(|e| {
        ToolError::Internal(format!(
            "copy_file({} -> {}): {e}",
            src.display(),
            dst.display()
        ))
    })?;

    Ok(success_json(&json!({
        "src": src.display().to_string(),
        "dst": dst.display().to_string(),
        "bytes_copied": bytes_copied,
    })))
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
    use std::sync::Arc;

    fn ctx() -> Arc<dyn crate::context::ToolContext> {
        Arc::new(crate::context::MockToolContext::new())
    }

    /// Pull the first text block out of a `CallToolResult` so tests
    /// can assert on the JSON body. Mirrors the helper in
    /// `computer_use::tests`.
    fn result_text(r: &CallToolResult) -> String {
        for c in &r.content {
            if let rmcp::model::RawContent::Text(t) = &c.raw {
                return t.text.clone();
            }
        }
        String::new()
    }

    /// Build the `Option<JsonObject>` shape `handle` expects from a
    /// `json!(...)` literal. We don't bother folding this into a
    /// helper that returns `Option<…>`; clippy flags the wrapper as
    /// `unnecessary_wraps` and the inline form mirrors the pattern in
    /// `computer_use::tests`.
    macro_rules! args {
        ($v:tt) => {
            Some(serde_json::json!($v).as_object().unwrap().clone())
        };
    }

    #[tokio::test]
    async fn copy_file_happy_path_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("template.txt");
        let dst = dir.path().join("copy.txt");
        let payload = b"hello world\nthis is a template\n";
        tokio::fs::write(&src, payload).await.unwrap();

        let res = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(
            body.contains(&format!("\"bytes_copied\": {}", payload.len())),
            "got: {body}"
        );
        let got = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn copy_file_missing_src_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("does-not-exist.txt");
        let dst = dir.path().join("dst.txt");
        let err = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got {err:?}"
        );
    }

    #[tokio::test]
    async fn copy_file_existing_dst_without_overwrite_errors() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        tokio::fs::write(&src, b"new").await.unwrap();
        tokio::fs::write(&dst, b"old").await.unwrap();
        let err = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got {err:?}"
        );
        // dst must be untouched after a rejected copy.
        let got = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(got, b"old");
    }

    #[tokio::test]
    async fn copy_file_overwrite_replaces_existing_dst() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        tokio::fs::write(&src, b"new contents").await.unwrap();
        tokio::fs::write(&dst, b"old contents").await.unwrap();
        let res = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
                "overwrite": true,
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"bytes_copied\": 12"), "got: {body}");
        let got = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(got, b"new contents");
    }

    #[tokio::test]
    async fn copy_file_create_parents_makes_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("a/b/c/dst.txt");
        tokio::fs::write(&src, b"hi").await.unwrap();
        let res = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
                "create_parents": true,
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"bytes_copied\": 2"), "got: {body}");
        assert!(dst.exists());
    }

    #[tokio::test]
    async fn copy_file_missing_parent_without_create_parents_internal_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("missing/sub/dst.txt");
        tokio::fs::write(&src, b"hi").await.unwrap();
        let err = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Internal(_)),
            "expected Internal, got {err:?}"
        );
        // And the dst tree must not have been partially created.
        assert!(!dst.exists());
    }

    #[tokio::test]
    async fn copy_file_binary_payload_is_bit_exact() {
        // PNG-style header plus a smattering of non-UTF8 sequences.
        // If we accidentally routed the bytes through a String we'd
        // either fail to decode or mangle them via the lossy replace
        // glyph; a byte-for-byte equality assertion catches both.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("img.bin");
        let dst = dir.path().join("img-copy.bin");
        let mut payload: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, // PNG magic
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            b'I', b'H', b'D', b'R',
        ];
        // Append every byte 0x80..=0xFF — none of these form valid
        // UTF-8 on their own.
        payload.extend(0x80u8..=0xFFu8);
        // And throw in a lone 0xC0 followed by 0x00 — an invalid 2-
        // byte UTF-8 lead with no continuation.
        payload.extend_from_slice(&[0xC0, 0x00, 0xFF, 0xFE, 0xFD]);
        tokio::fs::write(&src, &payload).await.unwrap();

        let res = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(
            body.contains(&format!("\"bytes_copied\": {}", payload.len())),
            "got: {body}"
        );
        let got = tokio::fs::read(&dst).await.unwrap();
        assert_eq!(got, payload, "binary payload mangled by copy");
    }

    #[tokio::test]
    async fn copy_file_src_equals_dst_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();
        let err = handle(
            args!({
                "src": path.to_string_lossy(),
                "dst": path.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got {err:?}"
        );
    }

    #[tokio::test]
    async fn copy_file_src_is_directory_validation_error() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("a-dir");
        tokio::fs::create_dir(&src_dir).await.unwrap();
        let dst = dir.path().join("dst.txt");
        let err = handle(
            args!({
                "src": src_dir.to_string_lossy(),
                "dst": dst.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got {err:?}"
        );
    }

    #[tokio::test]
    async fn copy_file_oversize_source_rejected() {
        // Build a sparse file just over the 32 MB ceiling without
        // actually allocating 32 MB of RAM: seek + write a single
        // byte past the limit. The resulting file's *metadata length*
        // is what the handler checks.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("huge.bin");
        let dst = dir.path().join("huge-copy.bin");
        {
            use tokio::io::AsyncSeekExt;
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::File::create(&src).await.unwrap();
            // Seek to one byte past the ceiling, then write a sentinel
            // byte. `metadata().len()` will report ceiling + 1.
            f.seek(std::io::SeekFrom::Start(COPY_FILE_MAX_BYTES + 1))
                .await
                .unwrap();
            f.write_all(b"x").await.unwrap();
            f.flush().await.unwrap();
        }
        let err = handle(
            args!({
                "src": src.to_string_lossy(),
                "dst": dst.to_string_lossy(),
            }),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got {err:?}"
        );
        // And critically: no partial copy was created at the dst.
        assert!(!dst.exists());
    }
}
