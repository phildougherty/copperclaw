//! Inbound-file staging contract (M18 C3).
//!
//! Channel adapters download attachments before the router has resolved
//! which session(s) the message fans out to, so an adapter can never know
//! the final on-disk destination for the bytes. Historically Telegram wrote
//! downloads into the *channel's* `data_dir/inbox/<msg_id>/` and put that
//! host path into `content.attachment.path` — but the container mounts the
//! *session* directory at `/data`, so the file was unreachable by the agent
//! that had just been told about it.
//!
//! The contract that fixes this splits responsibility in two:
//!
//! 1. **Adapter side (staging).** The adapter downloads the attachment to a
//!    unique temp path via [`stage_inbound_file`] and describes it on the
//!    inbound event as `content.attachment` with at least:
//!
//!    - `filename` — sanitized single path component (see
//!      [`sanitize_filename`]).
//!    - [`STAGED_PATH_KEY`] (`"staged_path"`) — the absolute host path of
//!      the staged bytes. This key is the marker the router acts on; an
//!      attachment without it is passed through untouched.
//!
//!    plus whatever channel metadata it already carries (`kind`,
//!    `mime_type`, `size`, `file_id`, an optional inline `data_base64`
//!    for images, ...). The adapter MUST NOT put a `path` key on the
//!    attachment — that key belongs to the router.
//!
//! 2. **Router side (materialization).** At route time — when the target
//!    session is known — the router copies the staged bytes into
//!    `<session_dir>/inbox/<msg_id>/<safe_name>`, removes
//!    [`STAGED_PATH_KEY`] from the persisted content, and sets
//!    [`ATTACHMENT_PATH_KEY`] (`"path"`) to the *container-visible* path
//!    [`container_inbox_path`] (`/data/inbox/<msg_id>/<safe_name>`). The
//!    session dir is bind-mounted at `/data` (see the host's
//!    `container_manager::spawn::CONTAINER_SESSION_DIR`), so the agent can
//!    read the file at exactly the path the message names. After the
//!    fanout completes the router deletes the staged file, whatever the
//!    route outcome, so adapters never need to garbage-collect staging.
//!
//! Filenames and message ids originate from untrusted senders. Both sides
//! sanitize: the adapter names the staged file via [`sanitize_filename`],
//! and the router independently re-sanitizes the filename and the message
//! id before writing inside the session inbox (defense in depth — the
//! final write also goes through `copperclaw_db::attachments::
//! extract_to_inbox`, which rejects traversal, symlinks, and overwrites).

use crate::error::AdapterError;
use std::path::{Path, PathBuf};

/// Attachment-object key holding the host-side staged path an adapter
/// downloaded to. Presence of this key is what opts an attachment into
/// router-side materialization; the router removes it before persisting.
pub const STAGED_PATH_KEY: &str = "staged_path";

/// Attachment-object key holding the container-visible path the router
/// writes after materialization. Never set by adapters.
pub const ATTACHMENT_PATH_KEY: &str = "path";

/// Where the session directory is mounted inside the agent container,
/// suffixed with the inbox subdirectory. Mirrors the host's
/// `container_manager::spawn::CONTAINER_SESSION_DIR` (`/data`) plus the
/// `inbox/` component of `copperclaw_db::session::SessionPaths`.
pub const CONTAINER_INBOX_DIR: &str = "/data/inbox";

/// Subdirectory of a channel's `data_dir` where staged downloads live
/// until the router materializes (and then deletes) them.
pub const STAGING_SUBDIR: &str = "staging";

/// Longest filename we accept for an inbound attachment (single path
/// component; most filesystems cap components at 255 bytes).
pub const MAX_INBOUND_FILENAME_LEN: usize = 255;

/// Fallback filename used when a sender-supplied name sanitizes to
/// nothing usable.
pub const FALLBACK_FILENAME: &str = "attachment.bin";

/// Sanitise a sender-supplied filename into something safe to use as a
/// single path component. Falls back to `fallback` when the supplied name
/// is empty or unusable.
///
/// Every character outside `[A-Za-z0-9.-_]` is replaced with `_`, which
/// removes path separators, control characters, and NUL by construction.
/// Leading dots / underscores are trimmed to avoid hidden-file names and
/// path-traversal residue (`..` becomes `__` then `""`); trailing
/// characters are preserved so callers can see how the original differed.
#[must_use]
pub fn sanitize_filename(name: Option<&str>, fallback: &str) -> String {
    let raw = name.unwrap_or("").trim();
    if raw.is_empty() {
        return fallback.to_owned();
    }
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_start_matches(['.', '_']);
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    if trimmed.len() > MAX_INBOUND_FILENAME_LEN {
        // Safe to slice by byte index: every retained char is ASCII.
        return trimmed[..MAX_INBOUND_FILENAME_LEN].to_owned();
    }
    trimmed.to_owned()
}

/// Container-visible path for a materialized inbound attachment:
/// `/data/inbox/<msg_id>/<filename>`. Both components must already be
/// sanitized single path components (the router guarantees this before
/// calling).
#[must_use]
pub fn container_inbox_path(msg_id: &str, filename: &str) -> String {
    format!("{CONTAINER_INBOX_DIR}/{msg_id}/{filename}")
}

/// Stage downloaded attachment bytes for router-side materialization.
///
/// Writes `bytes` to `<staging_root>/<unique>/<safe_name>`, where
/// `<unique>` is a fresh UUID directory so concurrent downloads (and
/// platform message-id collisions across chats) can never clobber each
/// other, and `<safe_name>` is `filename` passed through
/// [`sanitize_filename`]. Returns the staged path; its final component is
/// the sanitized name the adapter should surface as
/// `content.attachment.filename`.
///
/// The staged file's lifetime is owned by the router: it is deleted after
/// route-time materialization (or after the route is dropped).
pub async fn stage_inbound_file(
    staging_root: &Path,
    filename: &str,
    bytes: &[u8],
) -> Result<PathBuf, AdapterError> {
    let safe_name = sanitize_filename(Some(filename), FALLBACK_FILENAME);
    let dir = staging_root.join(uuid::Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        AdapterError::Transport(format!("staging dir create {} failed: {e}", dir.display()))
    })?;
    let path = dir.join(safe_name);
    tokio::fs::write(&path, bytes).await.map_err(|e| {
        AdapterError::Transport(format!("staging write {} failed: {e}", path.display()))
    })?;
    Ok(path)
}

/// Best-effort removal of a staged file and its (per-file unique) parent
/// directory. Called by the router once the fanout is complete; errors
/// are ignored because a leaked staging file is harmless and the
/// alternative (failing the route) is not.
pub fn remove_staged_file(path: &Path) {
    let _ = std::fs::remove_file(path);
    if let Some(dir) = path.parent() {
        // Only removes the directory when it is empty — which is the
        // normal case since `stage_inbound_file` creates one dir per file.
        let _ = std::fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_falls_back_when_input_is_only_dots() {
        assert_eq!(sanitize_filename(Some("...."), "x"), "x");
        assert_eq!(sanitize_filename(None, "x"), "x");
        assert_eq!(sanitize_filename(Some(""), "x"), "x");
    }

    #[test]
    fn sanitize_truncates_long_input() {
        let long = "a".repeat(1024);
        let safe = sanitize_filename(Some(&long), "x");
        assert!(safe.len() <= MAX_INBOUND_FILENAME_LEN);
    }

    #[test]
    fn sanitize_replaces_unsafe_characters() {
        assert_eq!(sanitize_filename(Some("a b.c"), "x"), "a_b.c");
        assert_eq!(sanitize_filename(Some("weird?name!"), "x"), "weird_name_");
        assert_eq!(sanitize_filename(Some("../../etc/passwd"), "x"), "etc_passwd");
        assert_eq!(sanitize_filename(Some("nul\0byte"), "x"), "nul_byte");
        assert_eq!(sanitize_filename(Some("ctrl\x07bell"), "x"), "ctrl_bell");
    }

    #[test]
    fn sanitize_strips_leading_hidden_dot() {
        assert_eq!(sanitize_filename(Some(".bashrc"), "x"), "bashrc");
    }

    #[test]
    fn container_inbox_path_shape() {
        assert_eq!(
            container_inbox_path("555", "spec.csv"),
            "/data/inbox/555/spec.csv"
        );
    }

    #[tokio::test]
    async fn stage_writes_bytes_under_unique_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let a = stage_inbound_file(tmp.path(), "a.txt", b"one").await.unwrap();
        let b = stage_inbound_file(tmp.path(), "a.txt", b"two").await.unwrap();
        assert_ne!(a, b, "same filename must stage to distinct paths");
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        assert_eq!(std::fs::read(&b).unwrap(), b"two");
        assert!(a.starts_with(tmp.path()));
    }

    #[tokio::test]
    async fn stage_sanitizes_traversal_names() {
        let tmp = tempfile::tempdir().unwrap();
        let p = stage_inbound_file(tmp.path(), "../../evil.sh", b"x")
            .await
            .unwrap();
        assert!(p.starts_with(tmp.path()), "staged path escaped root: {p:?}");
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "evil.sh");
    }

    #[tokio::test]
    async fn stage_falls_back_on_unusable_name() {
        let tmp = tempfile::tempdir().unwrap();
        let p = stage_inbound_file(tmp.path(), "....", b"x").await.unwrap();
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            FALLBACK_FILENAME
        );
    }

    #[tokio::test]
    async fn remove_staged_file_cleans_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let p = stage_inbound_file(tmp.path(), "a.txt", b"x").await.unwrap();
        let dir = p.parent().unwrap().to_path_buf();
        remove_staged_file(&p);
        assert!(!p.exists());
        assert!(!dir.exists(), "unique staging dir must be removed");
        // Idempotent on a missing path.
        remove_staged_file(&p);
    }
}
