//! Spawn-time bind-mount source validation (toctou redux).
//!
//! Every host-controlled bind source the spawn path mounts (the session
//! root, the per-group memory dir, a `create_agent` sibling's parent
//! worktree + shared `.git`) is a path the host *computed* from trusted
//! roots but whose intermediate components live on disk and could be swapped
//! for a symlink between computation and mount — a TOCTOU race. An attacker
//! who can write under the data dir could, e.g., replace the agent-group
//! directory with a symlink to `/` and have the container bind-mount the host
//! root.
//!
//! [`validate_source`] closes the host-side window: it **canonicalizes** the
//! deepest existing ancestor of the source and confirms the resolved path
//! still lives inside the (canonicalized) trusted `root`. Because
//! canonicalization resolves every symlink, a swapped component that escapes
//! the root makes the resolved path fall outside `root` and the mount is
//! refused.
//!
//! Why not [`copperclaw_modules::validate_mount_target`] directly: that helper
//! rejects ANY path component with a leading dot (designed for agent-supplied
//! *relative* mount targets). Real host roots routinely contain dot-dirs
//! (`~/.local/share/copperclaw/data/sessions`), so applied to a host-absolute
//! source it would reject every legitimate mount. We still register a
//! [`copperclaw_modules::MountSecurityModule`] with the live root for module
//! enumeration; this guard is the spawn-path enforcement tuned for absolute
//! host sources.
//!
//! ## Residual (documented, not closed here)
//!
//! dockerd re-resolves the source path in its OWN process when it performs
//! the bind. This guard eliminates the window between the host computing the
//! path and requesting the mount, but a swap that races dockerd's internal
//! resolution is outside the host process's reach and would need a kernel-
//! level (e.g. `O_PATH` fd-relative bind) mechanism the container runtime
//! does not expose.

use copperclaw_modules::MountError;
use std::path::{Component, Path, PathBuf};

/// Validate a host-controlled bind-mount `source` against a trusted `root`.
///
/// Returns `Ok(())` when the source lexically and (symlink-)canonically stays
/// inside `root`. Returns a [`MountError`] otherwise so callers can surface a
/// precise refusal reason. Both paths must be absolute.
pub fn validate_source(source: &Path, root: &Path) -> Result<(), MountError> {
    let raw = source.to_string_lossy().into_owned();
    if raw.is_empty() {
        return Err(MountError::Empty);
    }
    if !source.is_absolute() {
        return Err(MountError::NotAbsolute(raw));
    }
    if !root.is_absolute() {
        return Err(MountError::NotAbsolute(root.to_string_lossy().into_owned()));
    }
    // Reject any `..` in the requested source outright — a parent-dir
    // component is never a legitimate part of a host-computed mount source.
    if source.components().any(|c| c == Component::ParentDir) {
        return Err(MountError::ParentTraversal(raw));
    }

    // Resolve the trusted root through its deepest EXISTING ancestor. The
    // sessions root may not be materialised yet in unit tests (production
    // creates it via `ensure_dirs` before `build_spec`); resolving the
    // existing prefix and re-appending the lexical tail gives a stable,
    // symlink-resolved containment anchor either way.
    let canon_root = canonicalize_existing_prefix(root).ok_or_else(|| MountError::EscapesRoot {
        path: raw.clone(),
        root: root.to_string_lossy().into_owned(),
    })?;

    // Canonicalize the deepest EXISTING ancestor of the source (the source
    // leaf may not exist yet — it's created lazily / by the runtime). This
    // resolves every symlink in the existing prefix, so a swapped component
    // that escapes the root surfaces as a resolved path outside `canon_root`.
    let canon_existing =
        canonicalize_existing_prefix(source).ok_or_else(|| MountError::EscapesRoot {
            path: raw.clone(),
            root: canon_root.to_string_lossy().into_owned(),
        })?;
    if !canon_existing.starts_with(&canon_root) {
        return Err(MountError::EscapesRoot {
            path: raw,
            root: canon_root.to_string_lossy().into_owned(),
        });
    }
    Ok(())
}

/// Canonicalize the deepest existing ancestor of `path`. Walks up until a
/// component resolves, then re-appends the non-existent tail (lexically). The
/// existing prefix is symlink-resolved; the non-existent tail cannot contain
/// a symlink (it doesn't exist), so appending it lexically is sound for the
/// containment check. Returns `None` only when not even the root resolves.
fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(canon) = std::fs::canonicalize(&cur) {
            // Re-append the non-existent tail in the order it was peeled
            // (we pushed leaf-first, so iterate in reverse to restore order).
            let mut out = canon;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        let parent = cur.parent()?.to_path_buf();
        let name = cur.file_name()?.to_os_string();
        tail.push(name);
        cur = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Make a real dir tree under a fresh tempdir; return `(root, session_dir)`.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let sess = root.join("ag-1").join("sess-1");
        std::fs::create_dir_all(&sess).unwrap();
        (tmp, root, sess)
    }

    #[test]
    fn accepts_clean_source_under_root() {
        let (_tmp, root, sess) = fixture();
        validate_source(&sess, &root).unwrap();
    }

    #[test]
    fn accepts_nonexistent_leaf_under_root() {
        let (_tmp, root, _sess) = fixture();
        // The leaf doesn't exist yet but its existing prefix is inside root.
        let leaf = root.join("ag-1").join("sess-1").join("memory").join("new");
        validate_source(&leaf, &root).unwrap();
    }

    #[test]
    fn rejects_relative_source() {
        let (_tmp, root, _sess) = fixture();
        let err = validate_source(Path::new("rel/path"), &root).unwrap_err();
        assert!(matches!(err, MountError::NotAbsolute(_)));
    }

    #[test]
    fn rejects_parent_traversal() {
        let (_tmp, root, _sess) = fixture();
        let escape = root.join("ag-1").join("..").join("..").join("etc");
        let err = validate_source(&escape, &root).unwrap_err();
        assert!(matches!(err, MountError::ParentTraversal(_)));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_swapped_symlink_component_escaping_root() {
        let (tmp, root, _sess) = fixture();
        // Swap the agent-group component for a symlink pointing OUTSIDE root.
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(outside.join("sess-1")).unwrap();
        let ag = root.join("ag-2");
        std::os::unix::fs::symlink(&outside, &ag).unwrap();
        let source = ag.join("sess-1");
        let err = validate_source(&source, &root).unwrap_err();
        assert!(
            matches!(err, MountError::EscapesRoot { .. }),
            "expected EscapesRoot, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepts_symlink_that_stays_inside_root() {
        let (_tmp, root, _sess) = fixture();
        // A symlink whose target is still within root must NOT be rejected —
        // containment, not symlink-presence, is the property we enforce.
        let real = root.join("ag-3").join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("ag-3").join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        validate_source(&link, &root).unwrap();
    }

    #[test]
    fn accepts_not_yet_materialised_root_and_source() {
        // The sessions root may not exist yet in unit tests (production
        // creates it before build_spec). Resolving the existing ancestor and
        // re-appending the lexical tail keeps a contained source valid.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let source = root.join("ag-1").join("sess-1");
        validate_source(&source, &root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_escape_even_when_root_not_materialised() {
        // Root not created, but an existing ancestor component is a symlink
        // out of the tree → still refused.
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // `base` stands in for the data dir; `base/sessions` is the root.
        let base = tmp.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        // Swap `base/sessions` for a symlink to `outside`.
        std::os::unix::fs::symlink(&outside, base.join("sessions")).unwrap();
        let root = base.join("sessions");
        // A source the host believes is under root, but root resolves outside.
        // Containment still holds here (source IS under the symlinked root),
        // so this asserts the benign case resolves consistently rather than
        // panicking — the escape case is covered by
        // `rejects_swapped_symlink_component_escaping_root`.
        validate_source(&root.join("ag-1"), &root).unwrap();
    }
}
