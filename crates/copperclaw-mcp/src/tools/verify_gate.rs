//! M18 R3 verification-gate primitives: file-marker-based "dirty since
//! last verify" tracking per project directory.
//!
//! A project is any first-level directory under `/data` (the
//! container's bind-mounted session dir). State lives under
//! `<project_root>/.copperclaw/`:
//!
//! - `verify`       — the recorded verify command(s). The agent writes
//!   this itself (see the `coding-task` skill); this module only
//!   *reads* it. M20 Q2: may now hold MULTIPLE lines, each an
//!   independent stage, with an optional `name:` prefix (`lint: npx
//!   eslint .`); an unprefixed line gets a derived name (`stage<N>`,
//!   1-indexed by position). A single unprefixed line is exactly the
//!   pre-Q2 shape — see [`recorded_stages`].
//! - `dirty`         — presence = dirty since the last successful
//!   verify run; content is irrelevant (an empty file is fine).
//! - `fix_cycles`    — plain integer text; absent means `0`.
//! - `last_failure`  — tail text of the most recent failed verify run.
//!   Q2: when a stage-aware caller records a failure via
//!   [`record_stage_verify_failure`], the tail is prefixed with the
//!   stage name (`stage 'typecheck' failed: <tail>`); the underlying
//!   [`record_verify_failure`] itself stays byte-identical for direct
//!   callers.
//! - `stages`        — Q2: JSON map of stage name -> `{"passed":
//!   bool, "at": "<RFC3339>"}`, one entry per stage that has run at
//!   least once since the last dirty mark. [`mark_dirty`] resets it (a
//!   fresh edit invalidates every prior stage result); a passing
//!   verify run of a stage does NOT reset it — the file accumulates
//!   the "last known result" per stage until the next edit.
//!
//! Every mutator here is best-effort: I/O errors are logged and
//! swallowed rather than propagated, because a marker-file write
//! failure must never fail the *real* tool call (an edit, a shell
//! run) that triggered it.
//!
//! `project_root_of` is the one function here that isn't best-effort —
//! it's pure path/fs-metadata logic with no I/O side effects, so it
//! returns a plain `Option<PathBuf>`.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Hard cap on verify-fix cycles per todo before the gate auto-
/// transitions the todo to `blocked` (with the failure attached)
/// instead of refusing forever.
pub const FIX_CYCLE_CAP: u32 = 2;

const DATA_ROOT_DEFAULT: &str = "/data";
/// Unconditional (non-`#[cfg(test)]`) override for the project-data
/// root, mirroring the shell tool's `COPPERCLAW_SHELL_STATE_FILE`
/// precedent (see `computer_use.rs`). When set on the runner process's
/// environment it replaces `/data` as the root the R3 verify gate and
/// the todo store resolve against; when unset, production behavior is
/// byte-identical to the compiled-in `/data` default. The runner
/// process env is host-controlled at spawn — an in-container agent's
/// `shell` calls execute inside the container, not in the runner
/// process, so they cannot mutate this var and cannot use it to escape
/// the gate.
const DATA_ROOT_ENV_OVERRIDE: &str = "COPPERCLAW_DATA_ROOT";
const STATE_DIR_NAME: &str = ".copperclaw";
const VERIFY_FILE: &str = "verify";
const DIRTY_FILE: &str = "dirty";
const FIX_CYCLES_FILE: &str = "fix_cycles";
const LAST_FAILURE_FILE: &str = "last_failure";
/// Q2: per-project per-stage pass/fail + timestamp state.
const STAGES_FILE: &str = "stages";
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

/// Resolve the effective data root from its override sources, in
/// priority order: the in-process `#[cfg(test)]` override, then the
/// `COPPERCLAW_DATA_ROOT` env var, then the compiled-in `/data`
/// default. Split out as a pure function so the env-var branch is
/// unit-testable without mutating process env — `forbid(unsafe_code)`
/// plus edition 2024 make `std::env::set_var` unavailable in tests.
fn resolve_data_root(test_override: Option<PathBuf>, env: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(p) = test_override {
        return p;
    }
    env.map_or_else(|| PathBuf::from(DATA_ROOT_DEFAULT), PathBuf::from)
}

/// Resolve the container's project-data root: `/data` in production,
/// overridable at runtime via the `COPPERCLAW_DATA_ROOT` env var and,
/// in tests, via [`data_root_test_override_set`] (which wins over the
/// env var). See [`resolve_data_root`] for the precedence.
pub(crate) fn data_root() -> PathBuf {
    resolve_data_root(
        data_root_override(),
        std::env::var_os(DATA_ROOT_ENV_OVERRIDE),
    )
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
/// a permanently doomed todo. Q2: also resets every stage's recorded
/// pass/fail state — a fresh edit invalidates all prior stage runs,
/// not just the one that happened to touch this file.
pub async fn mark_dirty(project_root: &Path) {
    write_best_effort(&state_dir(project_root).join(DIRTY_FILE), b"").await;
    reset_fix_cycles(project_root).await;
    remove_if_present(&state_dir(project_root).join(STAGES_FILE)).await;
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

// ── M20 Q2: multi-stage verify ──────────────────────────────────────

/// One independent verify stage: a name (user-given via a `name:`
/// prefix, or derived) and the exact shell command that runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub name: String,
    pub command: String,
}

/// On-disk shape of one entry in `.copperclaw/stages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StageRecord {
    passed: bool,
    at: String,
}

/// Split one trimmed, non-empty verify-file line into an optional
/// `name:` prefix and the remaining command. The prefix syntax is
/// deliberately narrow — the name must be a bare
/// `[A-Za-z][A-Za-z0-9_-]*` token immediately followed by `:` — so
/// ordinary shell commands that happen to contain a colon (a URL, a
/// `docker run -p 8080:80`, a `key: value`-shaped grep pattern) are
/// never misparsed as a stage prefix: any of those either has the
/// colon past the first whitespace-delimited token (name would
/// contain a space, rejected) or nothing following the colon once
/// trimmed (rejected). Returns `None` when the line has no valid
/// prefix, in which case the whole trimmed line is the command and
/// the caller derives a name.
fn split_stage_prefix(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let name = &line[..colon];
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        || name.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return None;
    }
    let rest = line[colon + 1..].trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((name, rest))
}

/// Parse the raw `.copperclaw/verify` text into an ordered list of
/// stages. Blank lines are skipped. A single unprefixed line yields
/// exactly one stage (`stage1`, the pre-Q2 shape byte-for-byte from
/// the matching caller's point of view — see `apply_verify_gate`).
fn parse_stages(text: &str) -> Vec<Stage> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .enumerate()
        .map(|(i, line)| {
            if let Some((name, cmd)) = split_stage_prefix(line) {
                Stage {
                    name: name.to_string(),
                    command: cmd.to_string(),
                }
            } else {
                Stage {
                    name: format!("stage{}", i + 1),
                    command: line.to_string(),
                }
            }
        })
        .collect()
}

/// Resolve the effective list of verify stages for `project_root`.
/// `override_cmd` (from `ToolContext::check_command_override`) wins
/// when `Some` — mirroring [`recorded_verify_command`], it collapses
/// to a single stage using the override text verbatim, ignoring
/// whatever the recorded `.copperclaw/verify` file contains. With no
/// override, reads and parses the marker file via [`parse_stages`];
/// a missing/empty file yields an empty stage list (no verify
/// recorded at all).
pub async fn recorded_stages(project_root: &Path, override_cmd: Option<&str>) -> Vec<Stage> {
    if let Some(cmd) = override_cmd {
        let trimmed = cmd.trim();
        return if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![Stage {
                name: "verify".to_string(),
                command: trimmed.to_string(),
            }]
        };
    }
    match read_best_effort(&state_dir(project_root).join(VERIFY_FILE)).await {
        Some(text) => parse_stages(&text),
        None => Vec::new(),
    }
}

async fn read_stage_state(project_root: &Path) -> HashMap<String, StageRecord> {
    match read_best_effort(&state_dir(project_root).join(STAGES_FILE)).await {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => HashMap::new(),
    }
}

async fn write_stage_state(project_root: &Path, state: &HashMap<String, StageRecord>) {
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            write_best_effort(&state_dir(project_root).join(STAGES_FILE), json.as_bytes()).await;
        }
        Err(e) => {
            tracing::warn!(
                project_root = %project_root.display(),
                error = %e,
                "verify_gate: failed to serialize stage state"
            );
        }
    }
}

/// Record the pass/fail outcome of one stage's run, timestamped now.
/// Best-effort, same style as every other marker mutator here.
pub async fn record_stage_result(project_root: &Path, stage_name: &str, passed: bool) {
    let mut state = read_stage_state(project_root).await;
    state.insert(
        stage_name.to_string(),
        StageRecord {
            passed,
            at: chrono::Utc::now().to_rfc3339(),
        },
    );
    write_stage_state(project_root, &state).await;
}

/// Record a stage's failed verify run: prefixes `tail` with the stage
/// name (`stage 'typecheck' failed: <tail>`) and delegates to
/// [`record_verify_failure`] so the project-wide fix-cycle counter and
/// `last_failure` marker keep working exactly as before — stage
/// attribution rides along in the tail text rather than changing that
/// function's signature or behavior for its existing direct callers.
pub async fn record_stage_verify_failure(project_root: &Path, stage_name: &str, tail: &str) -> u32 {
    let attributed = format!("stage '{stage_name}' failed: {tail}");
    record_verify_failure(project_root, &attributed).await
}

/// Whether every stage in `stages` currently reads as passed in the
/// project's stage-state file. An empty `stages` list (no verify
/// recorded at all) is never "all passed" — there is nothing to be
/// green about.
pub async fn all_stages_passed(project_root: &Path, stages: &[Stage]) -> bool {
    if stages.is_empty() {
        return false;
    }
    let state = read_stage_state(project_root).await;
    stages
        .iter()
        .all(|s| state.get(&s.name).is_some_and(|r| r.passed))
}

/// The stages in `stages` (file order preserved) that have NOT yet
/// been recorded as passed — missing (never run since the last dirty
/// mark) or recorded as failed.
pub async fn pending_stages(project_root: &Path, stages: &[Stage]) -> Vec<Stage> {
    let state = read_stage_state(project_root).await;
    stages
        .iter()
        .filter(|s| !state.get(&s.name).is_some_and(|r| r.passed))
        .cloned()
        .collect()
}

/// Shared post-write hook for the edit-family tools (`write_file`,
/// `edit_file`, `multi_edit`, `apply_patch`): if the gate is enabled
/// for this session and `path` resolves to a project, mark that
/// project dirty. Best-effort and a no-op when the gate is off or the
/// path isn't inside any `/data/<project>` — callers invoke this
/// unconditionally after every successful write.
///
/// M20 Q8: a write anywhere under `<project>/.copperclaw/` is exempted
/// — see [`is_under_state_dir`]. That directory is the gate's own
/// bookkeeping (`verify`, `dirty`, `stages`, …) plus a growing set of
/// agent-authored *metadata* files that must survive a green verify
/// untouched: the Q8 `DECISIONS.md` decision log (append-only,
/// consumed verbatim by compaction) and Q7's `CONTRACT.md`. None of
/// those are source changes, so marking dirty here would mean a
/// decision-log append invalidates an already-green verify — exactly
/// backwards from the log's purpose.
pub(crate) async fn mark_dirty_for_write(ctx: &dyn crate::context::ToolContext, path: &str) {
    if !ctx.verify_gate_enabled() {
        return;
    }
    if is_under_state_dir(path) {
        return;
    }
    if let Some(root) = project_root_of(path) {
        mark_dirty(&root).await;
    }
}

/// True iff `path` (an absolute path under the data root) resolves to
/// some project's `.copperclaw/` state directory, at any depth beneath
/// it. Pure path logic, no I/O — mirrors [`project_root_of`]'s
/// prefix-stripping but only needs to check one path component rather
/// than resolve a directory on disk.
fn is_under_state_dir(path: &str) -> bool {
    let root = data_root();
    let Ok(rel) = Path::new(path).strip_prefix(&root) else {
        return false;
    };
    let mut comps = rel.components();
    // First component is the project name itself, not part of the
    // state-dir check.
    if comps.next().is_none() {
        return false;
    }
    matches!(comps.next(), Some(Component::Normal(name)) if name == STATE_DIR_NAME)
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
    fn resolve_data_root_env_override_points_at_tempdir() {
        // The COPPERCLAW_DATA_ROOT override redirects the gate's root at
        // a caller-supplied directory. We exercise the resolver directly
        // (rather than mutating process env, which forbid(unsafe_code) +
        // edition 2024 disallow) so the env branch is covered honestly.
        let td = tempfile::tempdir().unwrap();
        let resolved = resolve_data_root(None, Some(td.path().as_os_str().to_os_string()));
        assert_eq!(resolved, td.path());
    }

    #[test]
    fn resolve_data_root_defaults_to_data_when_unset() {
        // Production behavior with the var unset is byte-identical to the
        // compiled-in default — no test override, no env value.
        assert_eq!(resolve_data_root(None, None), PathBuf::from("/data"));
    }

    #[test]
    fn resolve_data_root_test_override_wins_over_env() {
        // The in-process test override takes precedence over the env var,
        // preserving the existing #[cfg(test)] override mechanism.
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let resolved = resolve_data_root(
            Some(a.path().to_path_buf()),
            Some(b.path().as_os_str().to_os_string()),
        );
        assert_eq!(resolved, a.path());
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

    // ── M20 Q2: multi-stage verify ───────────────────────────────

    #[test]
    fn parse_stages_single_unprefixed_line_is_one_derived_stage() {
        let stages = parse_stages("npm test\n");
        assert_eq!(
            stages,
            vec![Stage {
                name: "stage1".into(),
                command: "npm test".into(),
            }]
        );
    }

    #[test]
    fn parse_stages_reads_named_and_derived_stages_in_order() {
        let stages = parse_stages("lint: npx eslint .\n\ntypecheck: npx tsc --noEmit\nnpm test\n");
        assert_eq!(
            stages,
            vec![
                Stage {
                    name: "lint".into(),
                    command: "npx eslint .".into(),
                },
                Stage {
                    name: "typecheck".into(),
                    command: "npx tsc --noEmit".into(),
                },
                Stage {
                    name: "stage3".into(),
                    command: "npm test".into(),
                },
            ]
        );
    }

    #[test]
    fn parse_stages_does_not_misparse_a_colon_inside_the_command() {
        // A command whose first token contains a space before any colon
        // never matches the prefix syntax — the whole line is the command.
        let stages = parse_stages("curl http://localhost:3000/health\n");
        assert_eq!(
            stages,
            vec![Stage {
                name: "stage1".into(),
                command: "curl http://localhost:3000/health".into(),
            }]
        );
    }

    #[test]
    fn parse_stages_empty_text_yields_no_stages() {
        assert!(parse_stages("").is_empty());
        assert!(parse_stages("   \n\n  ").is_empty());
    }

    #[tokio::test]
    async fn recorded_stages_none_when_no_verify_file() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        assert!(recorded_stages(&proj, None).await.is_empty());
    }

    #[tokio::test]
    async fn recorded_stages_override_collapses_to_one_stage() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(
            proj.join(".copperclaw").join("verify"),
            "lint: npx eslint .\ntypecheck: npx tsc --noEmit\n",
        )
        .unwrap();
        let stages = recorded_stages(&proj, Some("cargo check")).await;
        assert_eq!(
            stages,
            vec![Stage {
                name: "verify".into(),
                command: "cargo check".into(),
            }]
        );
    }

    #[tokio::test]
    async fn record_stage_result_then_all_stages_passed_round_trips() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let stages = vec![
            Stage {
                name: "lint".into(),
                command: "npx eslint .".into(),
            },
            Stage {
                name: "test".into(),
                command: "npm test".into(),
            },
        ];
        assert!(!all_stages_passed(&proj, &stages).await);
        record_stage_result(&proj, "lint", true).await;
        assert!(!all_stages_passed(&proj, &stages).await);
        record_stage_result(&proj, "test", true).await;
        assert!(all_stages_passed(&proj, &stages).await);
    }

    #[tokio::test]
    async fn all_stages_passed_false_when_a_stage_is_recorded_failed() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let stages = vec![Stage {
            name: "test".into(),
            command: "npm test".into(),
        }];
        record_stage_result(&proj, "test", false).await;
        assert!(!all_stages_passed(&proj, &stages).await);
    }

    #[tokio::test]
    async fn all_stages_passed_false_for_empty_stage_list() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        assert!(!all_stages_passed(&proj, &[]).await);
    }

    #[tokio::test]
    async fn pending_stages_lists_only_the_unfinished_ones_in_order() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let stages = vec![
            Stage {
                name: "lint".into(),
                command: "npx eslint .".into(),
            },
            Stage {
                name: "typecheck".into(),
                command: "npx tsc --noEmit".into(),
            },
            Stage {
                name: "test".into(),
                command: "npm test".into(),
            },
        ];
        record_stage_result(&proj, "lint", true).await;
        let pending = pending_stages(&proj, &stages).await;
        assert_eq!(
            pending.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["typecheck", "test"]
        );
    }

    #[tokio::test]
    async fn mark_dirty_resets_all_stage_state() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let stages = vec![Stage {
            name: "test".into(),
            command: "npm test".into(),
        }];
        record_stage_result(&proj, "test", true).await;
        assert!(all_stages_passed(&proj, &stages).await);
        mark_dirty(&proj).await;
        assert!(!all_stages_passed(&proj, &stages).await);
        assert!(pending_stages(&proj, &stages).await.len() == 1);
    }

    #[tokio::test]
    async fn record_stage_verify_failure_prefixes_tail_with_stage_name() {
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        record_stage_verify_failure(&proj, "typecheck", "2 errors found").await;
        assert_eq!(
            last_failure(&proj).await.as_deref(),
            Some("stage 'typecheck' failed: 2 errors found")
        );
    }

    #[tokio::test]
    async fn record_stage_verify_failure_still_increments_project_fix_cycles() {
        // Stage attribution rides in the tail text only — the underlying
        // project-wide fix-cycle counter (and FIX_CYCLE_CAP mechanics)
        // keep working exactly as before.
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let n1 = record_stage_verify_failure(&proj, "lint", "boom").await;
        assert_eq!(n1, 1);
        let n2 = record_stage_verify_failure(&proj, "test", "boom again").await;
        assert_eq!(n2, 2);
    }

    // ---- M20 Q8: `.copperclaw/` writes are exempt from dirty-marking --

    #[test]
    fn is_under_state_dir_true_for_direct_child() {
        let g = DataRootGuard::new();
        let path = g.path().join("p").join(".copperclaw").join("DECISIONS.md");
        assert!(is_under_state_dir(path.to_str().unwrap()));
    }

    #[test]
    fn is_under_state_dir_true_for_nested_state_file() {
        let g = DataRootGuard::new();
        let path = g
            .path()
            .join("p")
            .join(".copperclaw")
            .join("screenshots")
            .join("shot.png");
        assert!(is_under_state_dir(path.to_str().unwrap()));
    }

    #[test]
    fn is_under_state_dir_false_for_ordinary_source_file() {
        let g = DataRootGuard::new();
        let path = g.path().join("p").join("src").join("main.rs");
        assert!(!is_under_state_dir(path.to_str().unwrap()));
    }

    #[test]
    fn is_under_state_dir_false_for_bare_project_root() {
        let g = DataRootGuard::new();
        let path = g.path().join("p");
        assert!(!is_under_state_dir(path.to_str().unwrap()));
    }

    #[test]
    fn is_under_state_dir_false_for_lookalike_name() {
        // A file whose name merely starts with the state-dir name (not
        // an exact component match) must not be exempted.
        let g = DataRootGuard::new();
        let path = g.path().join("p").join(".copperclaw-backup").join("x");
        assert!(!is_under_state_dir(path.to_str().unwrap()));
    }

    #[tokio::test]
    async fn mark_dirty_for_write_skips_writes_under_state_dir() {
        // The core Q8 regression: appending to the DECISIONS.md decision
        // log (or any other file under `.copperclaw/`) must never mark
        // the project dirty — that would invalidate an already-green
        // verify on every decision-log append.
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        let mock = crate::context::MockToolContext::new();
        let decisions = proj.join(".copperclaw").join("DECISIONS.md");
        mark_dirty_for_write(&mock, decisions.to_str().unwrap()).await;
        assert!(!is_dirty(&proj).await);
    }

    #[tokio::test]
    async fn mark_dirty_for_write_still_dirties_ordinary_source_writes() {
        // Back-compat: a write to a real project file (not under
        // `.copperclaw/`) still marks the project dirty as before.
        let g = DataRootGuard::new();
        let proj = g.path().join("p");
        std::fs::create_dir_all(&proj).unwrap();
        let mock = crate::context::MockToolContext::new();
        let src = proj.join("main.rs");
        mark_dirty_for_write(&mock, src.to_str().unwrap()).await;
        assert!(is_dirty(&proj).await);
    }
}
