//! Filesystem-mount safety helpers.
//!
//! When the host bind-mounts a directory into a session container we have to
//! be sure the supplied host path does not contain symlinks that resolve
//! outside the agent-group's allowed root. The host's mount layer calls
//! [`validate_mount_target`] before requesting the bind mount from the
//! container runtime.
//!
//! This file deliberately duplicates the *philosophy* of the attachment
//! safety helpers in `ironclaw-db::attachments` (no `..`, no leading dots,
//! no path separators inside a single component, no NUL or control chars).
//! We do not import that crate — module surface should not depend on the DB
//! crate; instead the rules are restated and tested here.

use crate::context::{Module, ModuleContext, MountHostContext};
use crate::error::ModuleError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// Errors a mount validation can raise.
#[derive(Debug, Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountError {
    /// `host_path` was empty.
    #[error("mount path is empty")]
    Empty,
    /// `host_path` was not absolute.
    #[error("mount path must be absolute: `{0}`")]
    NotAbsolute(String),
    /// `host_path` contained a `..` component.
    #[error("mount path contains parent-dir component: `{0}`")]
    ParentTraversal(String),
    /// A path component had a leading dot, NUL, or control character.
    #[error("mount path component `{component}` in `{path}` is unsafe")]
    UnsafeComponent { path: String, component: String },
    /// `host_path` resolved (canonically or lexically) outside `root`.
    #[error("mount path `{path}` escapes root `{root}`")]
    EscapesRoot { path: String, root: String },
    /// `host_path` contains a symlink at one of its path components.
    #[error("mount path `{0}` traverses a symlink")]
    SymlinkInPath(String),
}

/// Returns `Ok(())` if `host_path` is a safe target to bind-mount into a
/// container backed by `root`.
///
/// The check is purely **lexical and symlink-aware** — it does NOT require the
/// path to exist. If any component is a symlink (and `host_path` exists), the
/// check fails. If the path does not yet exist, the check succeeds provided
/// the lexical resolution stays inside `root`.
pub fn validate_mount_target(host_path: &Path, root: &Path) -> Result<(), MountError> {
    let raw = host_path.to_string_lossy().into_owned();
    let root_str = root.to_string_lossy().into_owned();
    if raw.is_empty() {
        return Err(MountError::Empty);
    }
    if !host_path.is_absolute() {
        return Err(MountError::NotAbsolute(raw));
    }
    if !root.is_absolute() {
        return Err(MountError::NotAbsolute(root_str));
    }

    for comp in host_path.components() {
        match comp {
            Component::ParentDir => {
                return Err(MountError::ParentTraversal(raw));
            }
            Component::Normal(os) => {
                let s = os.to_string_lossy();
                if s.is_empty()
                    || s.starts_with('.')
                    || s.contains('\0')
                    || s.chars().any(char::is_control)
                {
                    return Err(MountError::UnsafeComponent {
                        path: raw.clone(),
                        component: s.into_owned(),
                    });
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
        }
    }

    // Lexical containment: starts_with on canonicalized prefixes.
    let lex_path = lexical_normalize(host_path);
    let lex_root = lexical_normalize(root);
    if !lex_path.starts_with(&lex_root) {
        return Err(MountError::EscapesRoot {
            path: raw,
            root: root_str,
        });
    }

    // Symlink scan. We walk each existing prefix and reject if any is a
    // symlink. Non-existent prefixes are skipped (the host will create them).
    let mut cur = PathBuf::new();
    for comp in lex_path.components() {
        cur.push(comp.as_os_str());
        if let Ok(meta) = std::fs::symlink_metadata(&cur) {
            if meta.file_type().is_symlink() {
                return Err(MountError::SymlinkInPath(raw));
            }
        }
    }
    Ok(())
}

/// Lexical (non-IO) path normalization that drops `.`/`./` components and
/// collapses redundant separators. Does NOT resolve symlinks (that's why we
/// scan separately above).
pub fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Module impl.
pub struct MountSecurityModule {
    host: Option<MountHostContext>,
}

impl Default for MountSecurityModule {
    fn default() -> Self {
        Self::new()
    }
}

impl MountSecurityModule {
    pub fn new() -> Self {
        Self { host: None }
    }

    pub fn with_host(host: MountHostContext) -> Self {
        Self { host: Some(host) }
    }

    pub fn host(&self) -> Option<&MountHostContext> {
        self.host.as_ref()
    }

    /// Validate against the configured host root (if any). Returns
    /// `MountError::Empty` if no host has been set.
    pub fn validate(&self, host_path: &Path) -> Result<(), MountError> {
        let host = self.host.as_ref().ok_or(MountError::Empty)?;
        validate_mount_target(host_path, &host.session_root)
    }
}

#[async_trait]
impl Module for MountSecurityModule {
    fn name(&self) -> &'static str {
        "mount_security"
    }

    async fn install(&self, _ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        // This module is a library; it does not register hooks. The host's
        // bind-mount layer calls `validate_mount_target` directly. We still
        // implement `Module` so it can be enumerated in `iclaw modules list`.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockModuleContext;

    fn root() -> PathBuf {
        PathBuf::from("/srv/ironclaw/data/sessions")
    }

    #[test]
    fn accepts_normal_path_inside_root() {
        let p = root().join("ag-1/sess-2/inbox");
        validate_mount_target(&p, &root()).unwrap();
    }

    #[test]
    fn rejects_empty_path() {
        let err = validate_mount_target(Path::new(""), &root()).unwrap_err();
        assert!(matches!(err, MountError::Empty));
    }

    #[test]
    fn rejects_relative_path() {
        let err = validate_mount_target(Path::new("inbox/x"), &root()).unwrap_err();
        assert!(matches!(err, MountError::NotAbsolute(_)));
    }

    #[test]
    fn rejects_relative_root() {
        let err = validate_mount_target(Path::new("/x"), Path::new("rel")).unwrap_err();
        assert!(matches!(err, MountError::NotAbsolute(_)));
    }

    #[test]
    fn rejects_parent_traversal() {
        let p = root().join("../etc/passwd");
        let err = validate_mount_target(&p, &root()).unwrap_err();
        assert!(matches!(err, MountError::ParentTraversal(_)));
    }

    #[test]
    fn rejects_leading_dot_component() {
        let p = root().join(".ssh");
        let err = validate_mount_target(&p, &root()).unwrap_err();
        assert!(matches!(err, MountError::UnsafeComponent { .. }));
    }

    #[test]
    fn rejects_null_byte_component() {
        let p = root().join("a\0b");
        let err = validate_mount_target(&p, &root()).unwrap_err();
        assert!(matches!(err, MountError::UnsafeComponent { .. }));
    }

    #[test]
    fn rejects_control_char_component() {
        let p = root().join("ab\nc");
        let err = validate_mount_target(&p, &root()).unwrap_err();
        assert!(matches!(err, MountError::UnsafeComponent { .. }));
    }

    #[test]
    fn rejects_escape_via_different_root() {
        let p = PathBuf::from("/etc/passwd");
        let err = validate_mount_target(&p, &root()).unwrap_err();
        assert!(matches!(err, MountError::EscapesRoot { .. }));
    }

    #[test]
    fn detects_symlink_in_existing_prefix() {
        let tmp = tempdir();
        let root = tmp.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let target = tmp.join("outside");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.join("escape");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let p = link.join("file");
        let err = validate_mount_target(&p, &root).unwrap_err();
        assert!(matches!(err, MountError::SymlinkInPath(_)));
    }

    #[test]
    fn accepts_nonexistent_path_inside_root() {
        let tmp = tempdir();
        let root = tmp.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let p = root.join("never-created/sub/dir");
        validate_mount_target(&p, &root).unwrap();
    }

    #[test]
    fn lexical_normalize_drops_curdir() {
        let p = Path::new("/a/./b/./c");
        let out = lexical_normalize(p);
        assert_eq!(out, PathBuf::from("/a/b/c"));
    }

    #[test]
    fn module_validate_requires_configured_host() {
        let m = MountSecurityModule::new();
        let err = m.validate(Path::new("/x")).unwrap_err();
        assert!(matches!(err, MountError::Empty));
    }

    #[test]
    fn module_validate_delegates() {
        let m = MountSecurityModule::with_host(MountHostContext {
            session_root: root(),
        });
        m.validate(&root().join("a/b")).unwrap();
        assert!(m.validate(&PathBuf::from("/etc")).is_err());
        assert!(m.host().is_some());
    }

    #[tokio::test]
    async fn install_is_noop() {
        let m = MountSecurityModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert!(ctx.registered().is_empty());
        assert_eq!(m.name(), "mount_security");
    }

    #[test]
    fn mount_error_display_contains_paths() {
        let e = MountError::NotAbsolute("foo".into());
        assert!(e.to_string().contains("foo"));
        let e = MountError::ParentTraversal("path-here".into());
        assert!(e.to_string().contains("path-here"));
        let e = MountError::UnsafeComponent {
            path: "P".into(),
            component: "C".into(),
        };
        assert!(e.to_string().contains('P') && e.to_string().contains('C'));
        let e = MountError::EscapesRoot {
            path: "the-path".into(),
            root: "the-root".into(),
        };
        assert!(
            e.to_string().contains("the-path") && e.to_string().contains("the-root")
        );
        let e = MountError::SymlinkInPath("x".into());
        assert!(e.to_string().contains('x'));
        let e = MountError::Empty;
        assert!(e.to_string().contains("empty"));
    }

    #[test]
    fn mount_error_serde_roundtrip() {
        for e in [
            MountError::Empty,
            MountError::NotAbsolute("x".into()),
            MountError::ParentTraversal("x".into()),
            MountError::UnsafeComponent {
                path: "x".into(),
                component: "y".into(),
            },
            MountError::EscapesRoot {
                path: "x".into(),
                root: "y".into(),
            },
            MountError::SymlinkInPath("x".into()),
        ] {
            let s = serde_json::to_string(&e).unwrap();
            let back: MountError = serde_json::from_str(&s).unwrap();
            assert_eq!(e, back);
        }
    }

    // Tiny tempdir helper (no `tempfile` dep available in this crate's deps).
    fn tempdir() -> PathBuf {
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("ironclaw-mount-test-{pid}-{now}");
        let p = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
