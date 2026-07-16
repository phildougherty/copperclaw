//! M18 R3 verification-gate primitives: file-marker-based "dirty since
//! last verify" tracking per project directory.
//!
//! A project is any first-level directory under `/data` (the
//! container's bind-mounted session dir). State lives under
//! `<project_root>/.copperclaw/`:
//!
//! - `verify`       — the recorded verify command, one line. The agent
//!   writes this itself (see the `coding-task` skill); this module
//!   only *reads* it.
//! - `dirty`         — presence = dirty since the last successful
//!   verify run; content is irrelevant (an empty file is fine).
//! - `fix_cycles`    — plain integer text; absent means `0`.
//! - `last_failure`  — tail text of the most recent failed verify run.
//!
//! Every mutator here is best-effort: I/O errors are logged and
//! swallowed rather than propagated, because a marker-file write
//! failure must never fail the *real* tool call (an edit, a shell
//! run) that triggered it.
//!
//! `project_root_of` is the one function here that isn't best-effort —
//! it's pure path/fs-metadata logic with no I/O side effects, so it
//! returns a plain `Option<PathBuf>`.

use std::path::{Component, Path, PathBuf};

/// Hard cap on verify-fix cycles per todo before the gate auto-
/// transitions the todo to `blocked` (with the failure attached)
/// instead of refusing forever.
pub const FIX_CYCLE_CAP: u32 = 2;

const DATA_ROOT_DEFAULT: &str = "/data";
const STATE_DIR_NAME: &str = ".copperclaw";
const VERIFY_FILE: &str = "verify";
const DIRTY_FILE: &str = "dirty";
const FIX_CYCLES_FILE: &str = "fix_cycles";
const LAST_FAILURE_FILE: &str = "last_failure";
/// Safety cap on the stored `last_failure` tail so a single
/// pathological verify run can't grow `.copperclaw/last_failure`
/// without bound. Callers are expected to already pass a pre-
/// truncated tail (e.g. via `shell`'s `tail_bytes`); this is a
/// second, independent bound.
const LAST_FAILURE_MAX_BYTES: usize = 8 * 1024;

#[cfg(test)]
static DATA_ROOT_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn data_root_test_override_set(path: PathBuf) {
    let cell = DATA_ROOT_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
}

#[cfg(test)]
pub(crate) fn data_root_test_override_clear() {
    if let Some(cell) = DATA_ROOT_OVERRIDE.get() {
        *cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
fn data_root_override() -> Option<PathBuf> {
    DATA_ROOT_OVERRIDE.get().and_then(|m| {
        m.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    })
}

#[cfg(not(test))]
fn data_root_override() -> Option<PathBuf> {
    None
}

/// Resolve the container's project-data root: `/data` in production,
/// test-overridable via [`data_root_test_override_set`].
pub(crate) fn data_root() -> PathBuf {
    data_root_override().unwrap_or_else(|| PathBuf::from(DATA_ROOT_DEFAULT))
}

/// Cross-module lock so `verify_gate` / `computer_use` / `todo` tests
/// that override the shared data root don't race each other. Mirrors
/// `todo.rs`'s own `todo_env_lock`.
#[cfg(test)]
pub(crate) fn data_root_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Resolve `path` to the project root that owns it, if any.
///
/// - `<data_root>/<name>/...` (two or more components past the data
///   root) → `Some(<data_root>/<name>)` — always, regardless of
///   whether `<name>` exists yet on disk.
/// - `<data_root>/<name>` (exactly one component past the data root)
///   → `Some(<data_root>/<name>)` only when that path is an actual
///   directory on disk; a bare top-level *file* directly under the
///   data root is not a project.
/// - `<data_root>` itself, or any path outside `<data_root>` → `None`.
///
/// Pure path logic plus one `fs::metadata` stat for the single-
/// component case — no marker-file I/O, so unlike the rest of this
/// module it isn't `async` / best-effort.
pub fn project_root_of(path: &str) -> Option<PathBuf> {
    let root = data_root();
    let rel = Path::new(path).strip_prefix(&root).ok()?;
    let mut comps = rel.components();
    let Some(Component::Normal(name)) = comps.next() else {
        return None;
    };
    let candidate = root.join(name);
    if comps.next().is_some() {
        return Some(candidate);
    }
    if candidate.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

fn state_dir(project_root: &Path) -> PathBuf {
    project_root.join(STATE_DIR_NAME)
}

async fn remove_if_present(path: &Path) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "verify_gate: failed to remove marker file"
            );
        }
    }
}

async fn write_best_effort(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::warn!(
                path = %parent.display(),
                error = %e,
                "verify_gate: failed to create state dir"
            );
            return;
        }
    }
    if let Err(e) = tokio::fs::write(path, contents).await {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "verify_gate: failed to write marker file"
        );
    }
}

async fn read_best_effort(path: &Path) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "verify_gate: failed to read marker file"
            );
            None
        }
    }
}

async fn reset_fix_cycles(project_root: &Path) {
    remove_if_present(&state_dir(project_root).join(FIX_CYCLES_FILE)).await;
}

/// Mark `project_root` dirty (an edit landed since the last successful
/// verify) and reset its fix-cycle count to 0 — a fresh edit after a
/// bad stretch deserves a fresh [`FIX_CYCLE_CAP`]-attempt budget, not
/// a permanently doomed todo.
pub async fn mark_dirty(project_root: &Path) {
    write_best_effort(&state_dir(project_root).join(DIRTY_FILE), b"").await;
    reset_fix_cycles(project_root).await;
}

/// Whether `project_root` is dirty since its last successful verify.
pub async fn is_dirty(project_root: &Path) -> bool {
    tokio::fs::try_exists(state_dir(project_root).join(DIRTY_FILE))
        .await
        .unwrap_or(false)
}

/// Record a successful verify run: clears the dirty marker and resets
/// the fix-cycle count to 0.
pub async fn clear_dirty(project_root: &Path) {
    remove_if_present(&state_dir(project_root).join(DIRTY_FILE)).await;
    reset_fix_cycles(project_root).await;
}

/// Record a failed verify run: increments the fix-cycle count, stores
/// `tail` as the latest failure text, and returns the new count. Does
/// NOT touch the dirty marker — a failed verify leaves the project
/// dirty; there's nothing to clear.
pub async fn record_verify_failure(project_root: &Path, tail: &str) -> u32 {
    let new_count = fix_cycles(project_root).await.saturating_add(1);
    write_best_effort(
        &state_dir(project_root).join(FIX_CYCLES_FILE),
        new_count.to_string().as_bytes(),
    )
    .await;
    let truncated = tail_truncate(tail, LAST_FAILURE_MAX_BYTES);
    write_best_effort(
        &state_dir(project_root).join(LAST_FAILURE_FILE),
        truncated.as_bytes(),
    )
    .await;
    new_count
}

/// Current fix-cycle count for `project_root`. Absent / unparseable
/// state resolves to `0`.
pub async fn fix_cycles(project_root: &Path) -> u32 {
    read_best_effort(&state_dir(project_root).join(FIX_CYCLES_FILE))
        .await
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Tail text of the most recent failed verify run, if any.
pub async fn last_failure(project_root: &Path) -> Option<String> {
    read_best_effort(&state_dir(project_root).join(LAST_FAILURE_FILE)).await
}

/// Resolve the effective verify command for `project_root`.
/// `override_cmd` (from `ToolContext::check_command_override`) wins
/// when `Some`; otherwise reads and trims the agent-recorded `verify`
/// marker, returning `None` if it's missing or empty.
pub async fn recorded_verify_command(
    project_root: &Path,
    override_cmd: Option<&str>,
) -> Option<String> {
    if let Some(cmd) = override_cmd {
        return Some(cmd.trim().to_string());
    }
    read_best_effort(&state_dir(project_root).join(VERIFY_FILE))
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Shared post-write hook for the edit-family tools (`write_file`,
/// `edit_file`, `multi_edit`, `apply_patch`): if the gate is enabled
/// for this session and `path` resolves to a project, mark that
/// project dirty. Best-effort and a no-op when the gate is off or the
/// path isn't inside any `/data/<project>` — callers invoke this
/// unconditionally after every successful write.
pub(crate) async fn mark_dirty_for_write(ctx: &dyn crate::context::ToolContext, path: &str) {
    if !ctx.verify_gate_enabled() {
        return;
    }
    if let Some(root) = project_root_of(path) {
        mark_dirty(&root).await;
    }
}

/// Session-wide scan for dirty projects: every first-level directory
/// under the data root whose `.copperclaw/dirty` marker is present.
/// Best-effort — an unreadable data root yields an empty list rather
/// than an error. Sorted for deterministic iteration order.
pub(crate) async fn scan_dirty_projects() -> Vec<PathBuf> {
    let root = data_root();
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_dir() && is_dirty(&path).await {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Keep the last `max_bytes` bytes of `s`, rounded forward to the
/// nearest UTF-8 char boundary so the result is always valid `str`.
fn tail_truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len() - max_bytes;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DataRootGuard {
        dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl DataRootGuard {
        fn new() -> Self {
            let lock = data_root_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            data_root_test_override_set(dir.path().to_path_buf());
            Self { dir, _lock: lock }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    impl Drop for DataRootGuard {
        fn drop(&mut self) {
            data_root_test_override_clear();
        }
    }

    #[test]
    fn project_root_of_resolves_nested_subpath() {
        let g = DataRootGuard::new();
        let proj = g.path().join("myproj");
        std::fs::create_dir_all(proj.join("src")).unwrap();
        let file = proj.join("src").join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        assert_eq!(
            project_root_of(file.to_str().unwrap()),
            Some(proj),
            "nested subpath resolves without needing the file to exist"
        );
    }

    #[test]
    fn project_root_of_resolves_nonexistent_nested_subpath() {
        let g = DataRootGuard::new();
        let proj = g.path().join("myproj");
        // Deliberately do NOT create anything on disk — a fresh
        // write_file to a brand-new project dir must still resolve.
        let file = proj.join("server.js");
        assert_eq!(project_root_of(file.to_str().unwrap()), Some(proj));
    }

    #[test]
    fn project_root_of_resolves_project_root_directory_itself() {
        let g = DataRootGuard::new();
        let proj = g.path().join("myproj");
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(project_root_of(proj.to_str().unwrap()), Some(proj));
    }

    #[test]
    fn project_root_of_rejects_bare_top_level_file() {
        let g = DataRootGuard::new();
        let file = g.path().join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(project_root_of(file.to_str().unwrap()), None);
    }

    #[test]
    fn project_root_of_rejects_nonexistent_single_component_path() {
        let g = DataRootGuard::new();
        // A path shaped like a project root but that doesn't exist as
        // a directory (and isn't a file either) — no project.
        let ghost = g.path().join("ghost");
        assert_eq!(project_root_of(ghost.to_str().unwrap()), None);
    }

    #[test]
    fn project_root_of_rejects_data_root_itself() {
        let g = DataRootGuard::new();
        assert_eq!(project_root_of(g.path().to_str().unwrap()), None);
    }

    #[test]
    fn project_root_of_rejects_paths_outside_data_root() {
        let _g = DataRootGuard::new();
        assert_eq!(project_root_of("/etc/passwd"), None);
        assert_eq!(project_root_of("/tmp/whatever/file.txt"), None);
    }

    #[test]
    fn project_root_of_rejects_parent_dir_escape() {
        let g = DataRootGuard::new();
        let escaped = format!("{}/../etc/passwd", g.path().display());
        assert_eq!(project_root_of(&escaped), None);
    }

    #[tokio::test]
    async fn mark_dirty_then_is_dirty_round_trips() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        assert!(!is_dirty(&proj).await);
        mark_dirty(&proj).await;
        assert!(is_dirty(&proj).await);
    }

    #[tokio::test]
    async fn clear_dirty_removes_marker_and_resets_cycles() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        mark_dirty(&proj).await;
        record_verify_failure(&proj, "boom").await;
        record_verify_failure(&proj, "boom again").await;
        assert_eq!(fix_cycles(&proj).await, 2);
        clear_dirty(&proj).await;
        assert!(!is_dirty(&proj).await);
        assert_eq!(fix_cycles(&proj).await, 0);
    }

    #[tokio::test]
    async fn mark_dirty_resets_fix_cycles() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        record_verify_failure(&proj, "first failure").await;
        assert_eq!(fix_cycles(&proj).await, 1);
        // A fresh edit deserves a fresh budget.
        mark_dirty(&proj).await;
        assert_eq!(fix_cycles(&proj).await, 0);
    }

    #[tokio::test]
    async fn record_verify_failure_increments_and_stores_tail() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let n1 = record_verify_failure(&proj, "first error").await;
        assert_eq!(n1, 1);
        let n2 = record_verify_failure(&proj, "second error").await;
        assert_eq!(n2, 2);
        assert_eq!(last_failure(&proj).await.as_deref(), Some("second error"));
    }

    #[tokio::test]
    async fn record_verify_failure_does_not_touch_dirty() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        assert!(!is_dirty(&proj).await);
        record_verify_failure(&proj, "boom").await;
        // A failed verify leaves the project dirty state untouched —
        // it never sets dirty from a clean state here, and (more to
        // the point for the real call path) never clears it either.
        assert!(!is_dirty(&proj).await);
        mark_dirty(&proj).await;
        record_verify_failure(&proj, "boom").await;
        assert!(is_dirty(&proj).await, "failure must not clear dirty");
    }

    #[tokio::test]
    async fn fix_cycles_defaults_to_zero() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(fix_cycles(&proj).await, 0);
    }

    #[tokio::test]
    async fn last_failure_none_when_absent() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(last_failure(&proj).await, None);
    }

    #[tokio::test]
    async fn recorded_verify_command_reads_and_trims_marker() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "  npm test\n").unwrap();
        assert_eq!(
            recorded_verify_command(&proj, None).await.as_deref(),
            Some("npm test")
        );
    }

    #[tokio::test]
    async fn recorded_verify_command_none_when_missing_or_empty() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(recorded_verify_command(&proj, None).await, None);

        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "   \n").unwrap();
        assert_eq!(recorded_verify_command(&proj, None).await, None);
    }

    #[tokio::test]
    async fn recorded_verify_command_override_wins_over_marker() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "npm test").unwrap();
        assert_eq!(
            recorded_verify_command(&proj, Some("cargo check"))
                .await
                .as_deref(),
            Some("cargo check")
        );
    }

    #[tokio::test]
    async fn scan_dirty_projects_finds_only_dirty_dirs() {
        let g = DataRootGuard::new();
        let clean = g.path().join("clean-proj");
        let dirty = g.path().join("dirty-proj");
        std::fs::create_dir_all(&clean).unwrap();
        std::fs::create_dir_all(&dirty).unwrap();
        mark_dirty(&dirty).await;
        let found = scan_dirty_projects().await;
        assert_eq!(found, vec![dirty]);
    }

    #[tokio::test]
    async fn scan_dirty_projects_empty_when_none_dirty() {
        let g = DataRootGuard::new();
        std::fs::create_dir_all(g.path().join("clean")).unwrap();
        assert!(scan_dirty_projects().await.is_empty());
    }

    #[test]
    fn tail_truncate_keeps_last_bytes_on_char_boundary() {
        let s = "hello world";
        assert_eq!(tail_truncate(s, 100), "hello world");
        assert_eq!(tail_truncate(s, 5), "world");
        // Multi-byte chars: cap lands mid-character, rounds forward.
        let multibyte = "a".repeat(3) + "€€€"; // € is 3 bytes in UTF-8
        let truncated = tail_truncate(&multibyte, 4);
        assert!(truncated.is_char_boundary(0));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }
}
