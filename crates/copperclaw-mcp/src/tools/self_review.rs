//! `self_review`: enforced self-review gate before final delivery (M20 Q6 —
//! see `docs/plans/m20-coding-and-design-capability-program.md` Wave 2 Q6,
//! decision (d)).
//!
//! ## Problem this solves
//!
//! `skills/code-review/SKILL.md` carries real diff-review discipline (what
//! to look for, the adversarial pass, `create_agent` for high-stakes
//! changes) but nothing in the build loop ever invokes it, and nothing
//! forces the agent to read its own diff before declaring a prototype
//! ready. The M18 lesson (recorded in `CODING_PREAMBLE`'s doc comment)
//! stands: a prompt-only ritual is exactly what small local models skip.
//! Enforcement via a `todo_update` refusal is what made the verify gate
//! (M18 R3 / M20 Q2) stick.
//!
//! ## Two-phase tool
//!
//! One tool, two shapes selected by which args are present:
//!
//! - **Read** (`project` only): returns the project's diff since the last
//!   review marker (or since the project's first commit, if never
//!   reviewed), capped/chunked via `offset`/`limit` so the model actually
//!   reads it rather than getting a wall of text truncated mid-thought.
//! - **Submit** (`project` + `findings` and/or `no_findings: true`): writes
//!   `<project>/.copperclaw/reviewed` and returns an acknowledgement. Either
//!   shape "clears" the project's dirty-since-review state — this is a
//!   discipline gate, not adversarial security (see decision (d)): a model
//!   *can* submit lazy findings, the gate's job is to force the read-your-
//!   own-diff step to happen at all.
//!
//! ## Dirty-since-review tracking: content hash, not a marker flag
//!
//! Unlike the verify gate's `.copperclaw/dirty` (a presence flag set by
//! every write-family tool via `mark_dirty_for_write`), review-dirtiness
//! is computed on demand: `.copperclaw/reviewed` records the commit the
//! review was anchored to (`base`, `None` if the project had no commits
//! yet) and a sha256 of the diff from that base to the working tree AT
//! SUBMISSION TIME (`workdir_hash`). A later check recomputes that same
//! diff and compares hashes — different means something changed (a new
//! commit, an edit, a revert) since the review; identical means nothing
//! has. This needs no new write-family plumbing (no edit to
//! `verify_gate.rs`'s `mark_dirty_for_write`, `edit_file.rs`,
//! `computer_use.rs`, `multi_edit.rs`, or `apply_patch.rs`): "findings the
//! agent fixes re-dirty the project" falls out for free — an edit changes
//! the working tree, which changes the recomputed hash.
//!
//! A project that isn't a git repository (or is a bare repo with no
//! working tree) is [`ReviewState::NotApplicable`] — the gate never
//! engages for it. Every coding project is supposed to be a git repo from
//! its first edit (see `skills/coding-task/SKILL.md`), so this only ever
//! matters for a project that skipped that step, and it fails open (no new
//! restriction) rather than wedging such a project's completion forever.
//!
//! ## The `todo.rs` completion-gate cap
//!
//! [`REVIEW_CYCLE_CAP`] mirrors [`crate::tools::verify_gate::FIX_CYCLE_CAP`]:
//! the todo-completion gate increments a project's `review_cycles` counter
//! on every refusal it issues (via [`record_review_refusal`]) and, once the
//! cap is hit while the project is still dirty-since-review, auto-
//! transitions the todo to `blocked` instead of refusing forever — "so it
//! can't refuse forever" per the card. A [`submit`]-phase call resets the
//! counter to 0 (`self_review::reset_review_cycles`): calling the tool at
//! all is "doing the work," so it earns a fresh budget. This differs
//! slightly from the verify gate's own increment site (there, a *failed
//! verify run* increments; here, since there's no separate pass/fail
//! subprocess step, a *refused completion attempt* is the incrementing
//! event) but the shape — refuse up to the cap, then auto-block — is the
//! same.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};

/// Hard cap on review-fix cycles per todo before the gate auto-transitions
/// the todo to `blocked` (with a reason attached) instead of refusing
/// forever. Mirrors [`crate::tools::verify_gate::FIX_CYCLE_CAP`].
pub const REVIEW_CYCLE_CAP: u32 = 2;

const STATE_DIR: &str = ".copperclaw";
const REVIEWED_MARKER_FILE: &str = "reviewed";
const REVIEW_CYCLES_FILE: &str = "review_cycles";

/// Default chunk size for the diff-read phase: comfortably readable in one
/// turn without burning the whole context window.
const DEFAULT_CHUNK_BYTES: u64 = 20_000;
/// Hard ceiling on `limit` the caller may request per read.
const MAX_CHUNK_BYTES: u64 = 200_000;
/// Cap on how many findings strings the marker persists (the submitted
/// count is still reported in full via `findings_count`).
const MAX_FINDINGS_STORED: usize = 20;
/// Per-finding character cap so one runaway string can't bloat the marker.
const MAX_FINDING_CHARS: usize = 300;

// ── on-disk marker shape ────────────────────────────────────────────────

/// `.copperclaw/reviewed`: the anchor a review was submitted against, plus
/// enough to detect whether anything has changed since.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ReviewMarker {
    /// Hex commit oid the review was anchored to, or `None` if the project
    /// had no commits yet at submission time (diffed against an empty
    /// tree).
    base: Option<String>,
    /// sha256 (hex) of the diff from `base` to the working tree, computed
    /// AT SUBMISSION TIME. Recomputing that same diff later and comparing
    /// hashes is how dirty-since-review is detected — see the module docs.
    workdir_hash: String,
    reviewed_at: String,
    #[serde(default)]
    findings: Vec<String>,
    #[serde(default)]
    findings_count: usize,
}

fn marker_path(project_root: &Path) -> PathBuf {
    project_root.join(STATE_DIR).join(REVIEWED_MARKER_FILE)
}

fn read_marker(project_root: &Path) -> Option<ReviewMarker> {
    let text = std::fs::read_to_string(marker_path(project_root)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_marker(project_root: &Path, marker: &ReviewMarker) -> std::io::Result<()> {
    let dir = project_root.join(STATE_DIR);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(marker).unwrap_or_default();
    std::fs::write(dir.join(REVIEWED_MARKER_FILE), json)
}

// ── review-cycle counter (async, best-effort — mirrors verify_gate.rs) ──

async fn read_cycles(project_root: &Path) -> u32 {
    match tokio::fs::read_to_string(project_root.join(STATE_DIR).join(REVIEW_CYCLES_FILE)).await {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

async fn write_cycles(project_root: &Path, n: u32) {
    let dir = project_root.join(STATE_DIR);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(
            path = %dir.display(),
            error = %e,
            "self_review: failed to create state dir"
        );
        return;
    }
    if let Err(e) = tokio::fs::write(dir.join(REVIEW_CYCLES_FILE), n.to_string().as_bytes()).await {
        tracing::warn!(
            path = %dir.display(),
            error = %e,
            "self_review: failed to write review-cycles marker"
        );
    }
}

/// Current review-cycle count for `project_root`. Absent / unparseable
/// state resolves to `0`, mirroring [`crate::tools::verify_gate::fix_cycles`].
pub async fn review_cycles(project_root: &Path) -> u32 {
    read_cycles(project_root).await
}

/// Record one refused completion attempt: increments the review-cycle
/// count and returns the new value. Called by the `todo.rs` completion
/// gate on every refusal (see the module docs for why the increment site
/// differs from the verify gate's).
pub async fn record_review_refusal(project_root: &Path) -> u32 {
    let new_count = read_cycles(project_root).await.saturating_add(1);
    write_cycles(project_root, new_count).await;
    new_count
}

/// Reset the review-cycle count to 0. Called after any `self_review`
/// submission (findings or `no_findings`) — engaging with the tool at all
/// earns a fresh budget, mirroring `mark_dirty`/`clear_dirty` resetting
/// [`crate::tools::verify_gate::fix_cycles`].
pub async fn reset_review_cycles(project_root: &Path) {
    let path = project_root.join(STATE_DIR).join(REVIEW_CYCLES_FILE);
    if let Err(e) = tokio::fs::remove_file(&path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "self_review: failed to remove review-cycles marker"
            );
        }
    }
}

// ── git plumbing ─────────────────────────────────────────────────────────

/// True iff `path` (a diff delta's path, relative to the repo root) falls
/// under the project's own `.copperclaw/` state directory — component-wise,
/// so a lookalike name (`.copperclaw-backup/`) is never excluded. Mirrors
/// `verify_gate::is_under_state_dir`'s precedent, reimplemented locally so
/// this module makes zero edits to `verify_gate.rs`.
fn is_state_dir_path(path: &str) -> bool {
    Path::new(path)
        .components()
        .next()
        .is_some_and(|c| c.as_os_str() == STATE_DIR)
}

/// The current HEAD commit, or `None` if the repository has no commits yet
/// (unborn HEAD) — same "does `repo.head()` fail" check `git_log.rs` uses
/// for its empty-repo case.
fn head_oid(repo: &git2::Repository) -> Result<Option<git2::Oid>, git2::Error> {
    match repo.head() {
        Ok(head_ref) => Ok(Some(head_ref.peel_to_commit()?.id())),
        Err(_) => Ok(None),
    }
}

/// The repository's first (oldest) commit reachable from HEAD, or `None`
/// for an empty repo. "Since first commit" is the fallback diff base for a
/// project that has never been reviewed.
fn first_commit_oid(repo: &git2::Repository) -> Result<Option<git2::Oid>, git2::Error> {
    if repo.head().is_err() {
        return Ok(None);
    }
    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    walk.set_sorting(git2::Sort::TIME | git2::Sort::REVERSE)?;
    walk.next().transpose()
}

struct FileChange {
    path: String,
    additions: usize,
    deletions: usize,
}

struct DiffResult {
    text: String,
    files_changed: Vec<FileChange>,
}

/// Diff `base` (a commit, or `None` for "empty tree" — an unborn-HEAD
/// project) against the working tree, filtering out any path under
/// `.copperclaw/` (the gate's own bookkeeping, never a real project
/// change). Shared by the read phase (what to show), the submit phase
/// (what to hash), and [`review_state_blocking`] (the dirty check).
fn compute_project_diff(
    repo: &git2::Repository,
    base: Option<git2::Oid>,
) -> Result<DiffResult, git2::Error> {
    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3);
    let base_tree = match base {
        Some(oid) => Some(repo.find_commit(oid)?.tree()?),
        None => None,
    };
    let diff = repo.diff_tree_to_workdir_with_index(base_tree.as_ref(), Some(&mut opts))?;

    let mut buf: Vec<u8> = Vec::new();
    let mut by_file: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_state_dir_path(&path) {
            return true;
        }
        let entry = by_file.entry(path).or_insert((0, 0));
        match line.origin() {
            '+' => entry.0 += 1,
            '-' => entry.1 += 1,
            _ => {}
        }
        let marker = match line.origin() {
            ' ' | '+' | '-' => Some(line.origin() as u8),
            _ => None,
        };
        if let Some(c) = marker {
            buf.push(c);
        }
        buf.extend_from_slice(line.content());
        true
    })?;

    let files_changed = by_file
        .into_iter()
        .map(|(path, (additions, deletions))| FileChange {
            path,
            additions,
            deletions,
        })
        .collect();

    Ok(DiffResult {
        text: String::from_utf8_lossy(&buf).into_owned(),
        files_changed,
    })
}

fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    format!("{digest:x}")
}

/// Strict, non-discovering open: `project_root` itself must hold the
/// `.git` — the same "one repo per project directory" convention
/// `skills/coding-task/SKILL.md` teaches. Deliberately NOT
/// `git_common::open_repo` (which walks upward via `Repository::discover`):
/// `self_review`'s `project` arg is the project root, not an arbitrary path
/// inside a repo, so an upward walk could accidentally attribute a diff to
/// the wrong (parent) repository.
fn open_project_repo(project_root: &Path) -> Result<git2::Repository, ToolError> {
    git2::Repository::open(project_root).map_err(|e| match e.code() {
        git2::ErrorCode::NotFound => ToolError::Validation(format!(
            "self_review: `{}` is not a git repo — every coding project should be one (see the \
             coding-task skill): run `git init && git add -A && git commit -m \"init: scaffold\"` \
             in that directory, then retry.",
            project_root.display()
        )),
        _ => super::git_common::map_err("self_review: open repo", &e),
    })
}

// ── session-wide review-state classification (consumed by `todo.rs`) ────

/// A project's review-gate status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewState {
    /// No `.copperclaw/reviewed` marker exists yet.
    NeverReviewed,
    /// A marker exists, but the working tree has changed since (new
    /// commit, edit, or revert) — the recomputed diff hash no longer
    /// matches.
    Dirty,
    /// A marker exists and nothing has changed since.
    Clean,
    /// Not a git repository (or a bare one with no working tree) — the
    /// gate does not apply. Fails open: a project that skipped `git init`
    /// is not newly blocked by this card.
    NotApplicable,
}

fn review_state_blocking(project_root: &Path) -> ReviewState {
    let Ok(repo) = git2::Repository::open(project_root) else {
        return ReviewState::NotApplicable;
    };
    if repo.workdir().is_none() {
        return ReviewState::NotApplicable;
    }
    let Some(marker) = read_marker(project_root) else {
        return ReviewState::NeverReviewed;
    };
    let base = marker
        .base
        .as_deref()
        .and_then(|s| git2::Oid::from_str(s).ok());
    match compute_project_diff(&repo, base) {
        Ok(diff) => {
            if hash_text(&diff.text) == marker.workdir_hash {
                ReviewState::Clean
            } else {
                ReviewState::Dirty
            }
        }
        Err(_) => ReviewState::NotApplicable,
    }
}

/// Async wrapper: git2 is blocking, so the classification runs on the
/// blocking pool. Any panic/cancellation in the blocking task degrades to
/// [`ReviewState::NotApplicable`] rather than propagating — this is a
/// best-effort session-wide scan, not a agent-facing call.
pub async fn review_state(project_root: &Path) -> ReviewState {
    let root = project_root.to_path_buf();
    tokio::task::spawn_blocking(move || review_state_blocking(&root))
        .await
        .unwrap_or(ReviewState::NotApplicable)
}

/// Session-wide scan (mirrors `verify_gate::scan_dirty_projects`): every
/// top-level project directory under the data root whose review state is
/// [`ReviewState::NeverReviewed`] or [`ReviewState::Dirty`]. Sorted for
/// determinism.
pub(crate) async fn scan_projects_needing_review() -> Vec<PathBuf> {
    let root = super::verify_gate::data_root();
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if matches!(
            review_state(&path).await,
            ReviewState::NeverReviewed | ReviewState::Dirty
        ) {
            out.push(path);
        }
    }
    out.sort();
    out
}

// ── the tool itself ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Input {
    /// The project directory to review, e.g. the same path passed as
    /// `shell`'s `cwd`. Must already exist and be a git repo.
    project: String,
    /// Byte offset into the diff text (read phase only). Default 0.
    #[serde(default)]
    offset: Option<u64>,
    /// Bytes of diff to return per call (read phase only). Default
    /// [`DEFAULT_CHUNK_BYTES`], clamped to [`MAX_CHUNK_BYTES`].
    #[serde(default)]
    limit: Option<u64>,
    /// Submit phase: concrete issues found while reading the diff. Passing
    /// a non-empty array switches this call to submit mode.
    #[serde(default)]
    findings: Option<Vec<String>>,
    /// Submit phase: an explicit "I read the diff and found nothing worth
    /// flagging." Mutually exclusive with a non-empty `findings`.
    #[serde(default)]
    no_findings: Option<bool>,
}

pub fn schema() -> Tool {
    make_tool(
        "self_review",
        "Enforced self-review gate for the FINAL/delivery todo of a coding project. Two-phase: \
         (1) READ — call with just `project` (the project directory, e.g. the same path you pass \
         as `shell`'s `cwd`) to get the diff since your last review, or since the project's first \
         commit if you've never reviewed it — capped/chunked via `offset`/`limit` bytes so you can \
         actually read it rather than skimming past a wall of text. (2) SUBMIT — call again with \
         either `findings` (a non-empty array of short, concrete issue strings, e.g. \
         `[\"api.py:42 doesn't validate an empty body\"]`) or `no_findings: true` once you've \
         genuinely read the diff; this writes `.copperclaw/reviewed`. Completing the LAST \
         remaining todo of a project refuses until this has happened at least once since the \
         project's last edit — see `load_skill(\"code-review\")` for a depth checklist (what to \
         look for, the adversarial pass) before signing off on anything non-trivial. Requires the \
         project to be its own git repo (every coding project should be — see the coding-task \
         skill); a project that isn't one is never gated by this at all. Commit your work before \
         calling this (per the coding-task skill's discipline) for the tightest incremental diff \
         on your next review — uncommitted changes present at review time re-appear in a later \
         read until they're committed.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["project"],
            "properties": {
                "project": { "type": "string", "minLength": 1 },
                "offset": { "type": ["integer", "null"], "minimum": 0 },
                "limit": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": MAX_CHUNK_BYTES
                },
                "findings": {
                    "type": ["array", "null"],
                    "items": { "type": "string", "minLength": 1 }
                },
                "no_findings": { "type": ["boolean", "null"] }
            }
        }),
    )
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn read_blocking(project_root: &Path, offset: u64, limit: u64) -> Result<Value, ToolError> {
    let repo = open_project_repo(project_root)?;
    let marker = read_marker(project_root);
    let (base, since_label) = match &marker {
        Some(m) => (
            m.base.as_deref().and_then(|s| git2::Oid::from_str(s).ok()),
            "last_review",
        ),
        None => (
            first_commit_oid(&repo)
                .map_err(|e| super::git_common::map_err("self_review: first commit", &e))?,
            "first_commit",
        ),
    };
    let diff = compute_project_diff(&repo, base)
        .map_err(|e| super::git_common::map_err("self_review: diff", &e))?;

    // Do the byte-window arithmetic in `usize` (the diff text's own native
    // length type) and only widen to `u64` for the JSON response — a
    // narrowing `u64 -> usize` cast is the direction that can truncate on a
    // 32-bit target, so `offset`/`limit` are clamped down via
    // `usize::try_from` (saturating to `usize::MAX` on overflow, which
    // `.min(total_bytes)` immediately clamps back down anyway).
    let total_bytes = diff.text.len();
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(total_bytes);
    let end = usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .saturating_add(start)
        .min(total_bytes);
    let chunk = String::from_utf8_lossy(&diff.text.as_bytes()[start..end]).into_owned();
    let truncated = end < total_bytes;
    let total_bytes = total_bytes as u64;
    let start = start as u64;

    let files_changed: Vec<Value> = diff
        .files_changed
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "additions": f.additions,
                "deletions": f.deletions,
            })
        })
        .collect();

    Ok(json!({
        "project": project_root.display().to_string(),
        "mode": "read",
        "since": since_label,
        "diff": chunk,
        "offset": start,
        "total_bytes": total_bytes,
        "truncated": truncated,
        "files_changed": files_changed,
        "reviewed_before": marker.is_some(),
        "hint": "Read the diff above (page with `offset`/`limit` if `truncated` is true), then \
                 call self_review again with `findings` (array of short issue strings) or \
                 `no_findings: true` to record the review. For anything non-trivial, \
                 load_skill(\"code-review\") first for the depth checklist (what to look for, the \
                 adversarial pass).",
    }))
}

fn submit_blocking(project_root: &Path, findings: &[String]) -> Result<Value, ToolError> {
    let repo = open_project_repo(project_root)?;
    let new_base =
        head_oid(&repo).map_err(|e| super::git_common::map_err("self_review: head", &e))?;
    let diff = compute_project_diff(&repo, new_base)
        .map_err(|e| super::git_common::map_err("self_review: diff", &e))?;
    let hash = hash_text(&diff.text);

    let stored_findings: Vec<String> = findings
        .iter()
        .take(MAX_FINDINGS_STORED)
        .map(|f| truncate_chars(f, MAX_FINDING_CHARS))
        .collect();

    let marker = ReviewMarker {
        base: new_base.map(|oid| oid.to_string()),
        workdir_hash: hash,
        reviewed_at: chrono::Utc::now().to_rfc3339(),
        findings: stored_findings,
        findings_count: findings.len(),
    };
    write_marker(project_root, &marker).map_err(|e| {
        ToolError::Internal(format!(
            "self_review: failed to write reviewed marker at {}: {e}",
            marker_path(project_root).display()
        ))
    })?;

    Ok(json!({
        "project": project_root.display().to_string(),
        "mode": "submit",
        "status": "reviewed",
        "no_findings": findings.is_empty(),
        "findings_recorded": findings.len(),
        "marker_written": true,
    }))
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let project_root = PathBuf::from(&input.project);
    if !project_root.is_dir() {
        return Err(ToolError::Validation(format!(
            "self_review: `{}` is not a directory (or doesn't exist) — pass the project path you \
             use as `shell`'s `cwd`.",
            input.project
        )));
    }

    let no_findings = input.no_findings.unwrap_or(false);
    let findings = input.findings.unwrap_or_default();
    if no_findings && !findings.is_empty() {
        return Err(ToolError::Validation(
            "self_review: pass either `findings` (non-empty) or `no_findings: true`, not both."
                .into(),
        ));
    }

    if no_findings || !findings.is_empty() {
        let value = tokio::task::spawn_blocking(move || submit_blocking(&project_root, &findings))
            .await
            .map_err(|e| ToolError::Internal(format!("self_review join: {e}")))??;
        return Ok(success_json(&value));
    }

    let offset = input.offset.unwrap_or(0);
    let limit = input
        .limit
        .unwrap_or(DEFAULT_CHUNK_BYTES)
        .clamp(1, MAX_CHUNK_BYTES);
    let value = tokio::task::spawn_blocking(move || read_blocking(&project_root, offset, limit))
        .await
        .map_err(|e| ToolError::Internal(format!("self_review join: {e}")))??;
    Ok(success_json(&value))
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

    fn obj(value: Value) -> Option<JsonObject> {
        match value {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    fn body_json(result: &CallToolResult) -> Value {
        let text: String = result
            .content
            .iter()
            .filter_map(|c| {
                let raw = serde_json::to_value(c).ok()?;
                raw.get("text")?.as_str().map(str::to_string)
            })
            .collect();
        serde_json::from_str(&text).expect("response is JSON")
    }

    fn init_repo(path: &Path, file: &str, content: &str) {
        super::super::git_common::tests::init_with_commit(path, file, content);
    }

    /// Stage the already-on-disk `app.py` and commit — advances HEAD, the
    /// only thing that moves a review marker's `base` forward.
    fn commit_all(path: &Path, message: &str) {
        let repo = git2::Repository::open(path).unwrap();
        super::super::git_common::tests::commit_existing(&repo, "app.py", message);
    }

    // ── review_state classification ──────────────────────────────────────

    #[tokio::test]
    async fn review_state_not_applicable_for_non_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(review_state(dir.path()).await, ReviewState::NotApplicable);
    }

    #[tokio::test]
    async fn review_state_never_reviewed_for_fresh_repo() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        assert_eq!(review_state(dir.path()).await, ReviewState::NeverReviewed);
    }

    #[tokio::test]
    async fn review_state_clean_after_submit_with_no_further_changes() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        let ctx = MockToolContext::new();
        let args = obj(json!({"project": dir.path().to_string_lossy(), "no_findings": true}));
        handle(args, &ctx).await.unwrap();
        assert_eq!(review_state(dir.path()).await, ReviewState::Clean);
    }

    #[tokio::test]
    async fn review_state_dirty_after_edit_following_a_clean_review() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        let ctx = MockToolContext::new();
        let args = obj(json!({"project": dir.path().to_string_lossy(), "no_findings": true}));
        handle(args, &ctx).await.unwrap();
        assert_eq!(review_state(dir.path()).await, ReviewState::Clean);

        std::fs::write(dir.path().join("app.py"), "print('bye')\n").unwrap();
        assert_eq!(review_state(dir.path()).await, ReviewState::Dirty);
    }

    /// Guards the shared `verify_gate::data_root` override with its own
    /// cross-module lock (mirrors `todo.rs`'s `GateGuard`) so a concurrently
    /// running test elsewhere in the crate can never observe or clobber
    /// this override. Wrapped in a struct (rather than a bare local
    /// `MutexGuard`) so clippy's `await_holding_lock` doesn't flag holding
    /// it across the test's `.await` points — the lock genuinely needs to
    /// span them, exactly like `GateGuard` already does elsewhere in this
    /// crate.
    struct DataRootTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
    }

    impl DataRootTestGuard {
        fn new() -> Self {
            let lock = crate::tools::verify_gate::data_root_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            crate::tools::verify_gate::data_root_test_override_set(dir.path().to_path_buf());
            Self { _lock: lock, dir }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    impl Drop for DataRootTestGuard {
        fn drop(&mut self) {
            crate::tools::verify_gate::data_root_test_override_clear();
        }
    }

    #[tokio::test]
    async fn scan_finds_only_never_reviewed_and_dirty_projects() {
        let g = DataRootTestGuard::new();
        let root = g.path();

        let never = root.join("never-reviewed");
        std::fs::create_dir_all(&never).unwrap();
        init_repo(&never, "a.txt", "1\n");

        let clean = root.join("clean");
        std::fs::create_dir_all(&clean).unwrap();
        init_repo(&clean, "a.txt", "1\n");
        let ctx = MockToolContext::new();
        handle(
            obj(json!({"project": clean.to_string_lossy(), "no_findings": true})),
            &ctx,
        )
        .await
        .unwrap();

        let not_git = root.join("not-git");
        std::fs::create_dir_all(&not_git).unwrap();

        let mut found = scan_projects_needing_review().await;
        found.sort();

        assert_eq!(found, vec![never]);
    }

    // ── the tool: read phase ─────────────────────────────────────────────

    #[tokio::test]
    async fn read_phase_rejects_non_directory() {
        let ctx = MockToolContext::new();
        let err = handle(
            obj(json!({"project": "/definitely/does/not/exist/anywhere"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn read_phase_rejects_non_git_project() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = MockToolContext::new();
        let err = handle(obj(json!({"project": dir.path().to_string_lossy()})), &ctx)
            .await
            .unwrap_err();
        match err {
            ToolError::Validation(msg) => assert!(msg.contains("git init"), "got: {msg}"),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_phase_never_reviewed_shows_diff_since_first_commit() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        std::fs::write(dir.path().join("app.py"), "print('hi')\nprint('there')\n").unwrap();
        let ctx = MockToolContext::new();
        let body = body_json(
            &handle(obj(json!({"project": dir.path().to_string_lossy()})), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(body["mode"], "read");
        assert_eq!(body["since"], "first_commit");
        assert_eq!(body["reviewed_before"], false);
        assert!(
            body["diff"].as_str().unwrap().contains("+print('there')"),
            "got: {body}"
        );
        assert_eq!(body["files_changed"][0]["path"], "app.py");
    }

    #[tokio::test]
    async fn read_phase_after_review_shows_diff_since_last_review_only() {
        // The tight "since last review" delta requires the reviewed state
        // to actually be committed (the coding-task skill's "commit after
        // each working increment" discipline) — the marker's `base`
        // anchors to HEAD at submission time, and a commit is the only
        // thing that moves HEAD. See
        // `read_phase_uncommitted_state_at_review_time_carries_forward`
        // for the documented behaviour when that discipline isn't
        // followed.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        std::fs::write(dir.path().join("app.py"), "print('hi')\nprint('there')\n").unwrap();
        commit_all(dir.path(), "add 'there'");

        let ctx = MockToolContext::new();
        handle(
            obj(json!({"project": dir.path().to_string_lossy(), "no_findings": true})),
            &ctx,
        )
        .await
        .unwrap();

        // A further COMMITTED edit after the review: the read phase must
        // show ONLY the new delta, not the whole project-start diff again.
        std::fs::write(
            dir.path().join("app.py"),
            "print('hi')\nprint('there')\nprint('again')\n",
        )
        .unwrap();
        commit_all(dir.path(), "add 'again'");
        let body = body_json(
            &handle(obj(json!({"project": dir.path().to_string_lossy()})), &ctx)
                .await
                .unwrap(),
        );
        assert_eq!(body["since"], "last_review");
        assert_eq!(body["reviewed_before"], true);
        let diff = body["diff"].as_str().unwrap();
        assert!(diff.contains("+print('again')"), "got: {diff}");
        assert!(
            !diff.contains("+print('there')"),
            "must not re-show the already-reviewed (and since-committed) delta: {diff}"
        );
    }

    #[tokio::test]
    async fn read_phase_uncommitted_state_at_review_time_carries_forward() {
        // Documented characteristic, not a bug: the marker's `base` only
        // ever advances via a commit (it anchors to HEAD at submission
        // time). If the reviewed state was never committed, HEAD hasn't
        // moved, so the next read's "since last review" diff is computed
        // from that same old base and re-includes the already-reviewed
        // uncommitted delta alongside the new one. The dirty-since-review
        // CHECK (the todo-gate's concern) is unaffected either way — it
        // compares content hashes, not text — this only affects what the
        // read phase re-shows.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        std::fs::write(dir.path().join("app.py"), "print('hi')\nprint('there')\n").unwrap();
        let ctx = MockToolContext::new();
        handle(
            obj(json!({"project": dir.path().to_string_lossy(), "no_findings": true})),
            &ctx,
        )
        .await
        .unwrap();

        std::fs::write(
            dir.path().join("app.py"),
            "print('hi')\nprint('there')\nprint('again')\n",
        )
        .unwrap();
        let body = body_json(
            &handle(obj(json!({"project": dir.path().to_string_lossy()})), &ctx)
                .await
                .unwrap(),
        );
        let diff = body["diff"].as_str().unwrap();
        assert!(diff.contains("+print('again')"), "got: {diff}");
        assert!(
            diff.contains("+print('there')"),
            "uncommitted-at-review-time content re-appears until it's committed: {diff}"
        );
    }

    #[tokio::test]
    async fn read_phase_pages_via_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "");
        let big = "x\n".repeat(5000);
        std::fs::write(dir.path().join("app.py"), &big).unwrap();
        let ctx = MockToolContext::new();
        let body = body_json(
            &handle(
                obj(json!({"project": dir.path().to_string_lossy(), "limit": 100})),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(body["truncated"], true);
        assert_eq!(body["diff"].as_str().unwrap().len(), 100);
        assert!(body["total_bytes"].as_u64().unwrap() > 100);
    }

    // ── the tool: submit phase ────────────────────────────────────────────

    #[tokio::test]
    async fn submit_rejects_findings_and_no_findings_together() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        let ctx = MockToolContext::new();
        let err = handle(
            obj(json!({
                "project": dir.path().to_string_lossy(),
                "findings": ["a bug"],
                "no_findings": true,
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn submit_no_findings_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        let ctx = MockToolContext::new();
        let body = body_json(
            &handle(
                obj(json!({"project": dir.path().to_string_lossy(), "no_findings": true})),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(body["mode"], "submit");
        assert_eq!(body["status"], "reviewed");
        assert_eq!(body["no_findings"], true);
        assert_eq!(body["findings_recorded"], 0);
        assert!(marker_path(dir.path()).is_file());
    }

    #[tokio::test]
    async fn submit_with_findings_records_them() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        let ctx = MockToolContext::new();
        let body = body_json(
            &handle(
                obj(json!({
                    "project": dir.path().to_string_lossy(),
                    "findings": ["app.py:1 unused import", "no error handling on write"],
                })),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(body["no_findings"], false);
        assert_eq!(body["findings_recorded"], 2);
        let marker = read_marker(dir.path()).unwrap();
        assert_eq!(marker.findings_count, 2);
        assert_eq!(marker.findings.len(), 2);
    }

    #[tokio::test]
    async fn submit_caps_stored_findings_but_reports_true_count() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "app.py", "print('hi')\n");
        let ctx = MockToolContext::new();
        let many: Vec<String> = (0..30).map(|i| format!("finding {i}")).collect();
        let body = body_json(
            &handle(
                obj(json!({"project": dir.path().to_string_lossy(), "findings": many})),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(body["findings_recorded"], 30);
        let marker = read_marker(dir.path()).unwrap();
        assert_eq!(marker.findings_count, 30);
        assert_eq!(marker.findings.len(), MAX_FINDINGS_STORED);
    }

    // ── review-cycle counter ──────────────────────────────────────────────

    #[tokio::test]
    async fn review_cycles_default_zero_increment_and_reset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        assert_eq!(review_cycles(dir.path()).await, 0);
        assert_eq!(record_review_refusal(dir.path()).await, 1);
        assert_eq!(record_review_refusal(dir.path()).await, 2);
        assert_eq!(review_cycles(dir.path()).await, 2);
        reset_review_cycles(dir.path()).await;
        assert_eq!(review_cycles(dir.path()).await, 0);
    }

    // ── schema ────────────────────────────────────────────────────────────

    #[test]
    fn schema_names_the_tool_and_its_two_phases() {
        let tool = schema();
        assert_eq!(tool.name, "self_review");
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(desc.contains("READ"));
        assert!(desc.contains("SUBMIT"));
        assert!(desc.contains("code-review"));
    }
}
