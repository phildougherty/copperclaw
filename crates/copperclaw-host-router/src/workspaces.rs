//! Workspace discovery for the `/projects` and `/switch <name>` commands.
//!
//! A telegram (or any) chat maps to ONE session whose `/data` dir can hold
//! MANY working directories side by side — a git repo, or just a folder of
//! files the operator is organizing. This module lists those "workspaces"
//! (for the host-answered `/projects` reply) and resolves which one is
//! currently active (for the active marker) by reading the same
//! `.shell_state` file the container's `shell` tool maintains and that
//! `/switch` overwrites with a bare `cd '/data/<name>'` line.
//!
//! The active-dir parse deliberately mirrors the host's
//! `container_manager::spawn::parse_shell_pwd` (a private fn in the host
//! crate) rather than depending on it: the router can't reach into the
//! host binary, and `/switch`-written state carries only the `cd` line
//! (no dumped `PWD=` declaration), which the host's PWD-only parser would
//! miss. So this parser prefers the `cd` line and falls back to `PWD=`.

use std::path::Path;

/// The container mount point for a session root (`<session_root>` is
/// bind-mounted here). Workspace names are the first path segment beneath
/// it.
pub const CONTAINER_SESSION_DIR: &str = "/data";

/// The shell-state file under the session root (container path
/// `/data/.shell_state`).
const SHELL_STATE_FILE: &str = ".shell_state";

/// Top-level session-root entries that are runtime plumbing, never
/// operator workspaces. Dotfiles/dot-dirs (`.jobs`, `.copperclaw`,
/// `.shell_state`, …) are excluded separately by the leading-`.` rule, so
/// this list only needs the non-dot system dirs. `inbox`/`outbox` are the
/// session's message-I/O dirs; `skills`/`memory` are agent infrastructure;
/// `node_modules` is dependency noise that would otherwise masquerade as a
/// workspace at the root.
const SYSTEM_DIRS: &[&str] = &["skills", "memory", "inbox", "outbox", "node_modules"];

/// One workspace directory under a session root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Directory name (a single path segment under `/data`).
    pub name: String,
    /// True when the directory contains a `.git` entry (a code repo).
    pub is_git: bool,
    /// True when this is the session's currently-active workspace.
    pub active: bool,
}

/// List the workspaces under `session_root`, sorted by name, with the
/// active one flagged. Non-directory entries, dot-entries, and the
/// [`SYSTEM_DIRS`] are excluded. A missing/unreadable root yields an empty
/// list (the caller renders the friendly empty message).
#[must_use]
pub fn list_workspaces(session_root: &Path) -> Vec<Workspace> {
    let active = active_workspace_name(session_root);
    let Ok(entries) = std::fs::read_dir(session_root) else {
        return Vec::new();
    };
    let mut out: Vec<Workspace> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_system_dir(&name) {
            continue;
        }
        let is_git = entry.path().join(".git").exists();
        let active = active.as_deref() == Some(name.as_str());
        out.push(Workspace {
            name,
            is_git,
            active,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Whether a top-level entry name is runtime plumbing rather than a
/// workspace.
fn is_system_dir(name: &str) -> bool {
    name.starts_with('.') || SYSTEM_DIRS.contains(&name)
}

/// The name of the currently-active workspace (the first path segment of
/// the shell cwd beneath `/data`), or `None` when there's no shell state,
/// the cwd is the session root itself (`/data`), or the cwd is outside
/// `/data` (e.g. the agent `cd`'d to `/tmp`).
#[must_use]
pub fn active_workspace_name(session_root: &Path) -> Option<String> {
    let state = std::fs::read_to_string(session_root.join(SHELL_STATE_FILE)).ok()?;
    active_from_shell_state(&state)
}

/// Extract the active workspace name from raw `.shell_state` contents.
fn active_from_shell_state(state: &str) -> Option<String> {
    let cwd = parse_shell_cwd(state)?;
    let rel = cwd.strip_prefix(CONTAINER_SESSION_DIR)?;
    // Guard against a sibling prefix like `/datax/...`: the char after
    // `/data` must be a separator (or the string must end there).
    if !rel.is_empty() && !rel.starts_with('/') {
        return None;
    }
    let seg = rel.trim_start_matches('/').split('/').next().unwrap_or("");
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_owned())
    }
}

/// Resolve the shell cwd from `.shell_state`. Prefers an explicit
/// `cd <path>` line — present both in the shell tool's dump (its leading
/// line) and in `/switch`-written state (its ONLY line) — and falls back
/// to a dumped `PWD=` declaration when no `cd` line is present.
fn parse_shell_cwd(state: &str) -> Option<String> {
    for line in state.lines() {
        if let Some(rest) = line.trim().strip_prefix("cd ") {
            let path = unquote_shell(rest.trim());
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    parse_shell_pwd(state)
}

/// Extract `$PWD` from a dumped bash environment. Mirrors the host's
/// `container_manager::spawn::parse_shell_pwd`: match `... PWD="<value>"`
/// while skipping `OLDPWD` (its `PWD` substring is preceded by an
/// alphanumeric).
fn parse_shell_pwd(state: &str) -> Option<String> {
    for line in state.lines() {
        let Some(idx) = line.find("PWD=") else {
            continue;
        };
        if idx > 0 && line.as_bytes()[idx - 1].is_ascii_alphanumeric() {
            continue;
        }
        let val = line[idx + 4..].trim().trim_matches('"');
        if !val.is_empty() {
            return Some(val.to_owned());
        }
    }
    None
}

/// Strip a single matched pair of surrounding single or double quotes.
/// `printf %q` (what the shell tool uses) leaves safe paths bare and
/// single-quotes ones with special chars; `/switch` always single-quotes.
fn unquote_shell(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return s[1..s.len() - 1].to_owned();
        }
    }
    s.to_owned()
}

/// The container-visible path (`/data/<name>`) for a workspace, used both
/// to build the `/switch` shell-state line and in the host reply.
#[must_use]
pub fn container_workspace_path(name: &str) -> String {
    format!("{CONTAINER_SESSION_DIR}/{name}")
}

/// The `.shell_state` contents `/switch` writes so the agent's next shell
/// command runs in the new workspace: a single `cd '<container_path>'`
/// line. `name` is validated to `^[A-Za-z0-9._-]+$` upstream, so the
/// single-quoted path can never contain a quote to escape.
#[must_use]
pub fn switch_shell_state(name: &str) -> String {
    format!("cd '{}'\n", container_workspace_path(name))
}

/// Render the `/projects` host reply from a workspace listing.
#[must_use]
pub fn render_projects(workspaces: &[Workspace]) -> String {
    if workspaces.is_empty() {
        return "No workspaces yet — say 'build me X', or /switch <name> to start one.".to_owned();
    }
    let mut lines = vec!["Workspaces (/data):".to_owned()];
    for w in workspaces {
        let marker = if w.active { "\u{2192}" } else { " " };
        let git = if w.is_git { " (git)" } else { "" };
        lines.push(format!("{marker} {name}{git}", name = w.name));
    }
    lines.join("\n")
}

/// Render the `/switch <name>` success reply. `created` distinguishes a
/// brand-new workspace from one that already existed.
#[must_use]
pub fn render_switch_ok(name: &str, created: bool) -> String {
    let tag = if created { "new" } else { "existing" };
    format!(
        "Switched to workspace `{name}` — {path} ({tag})\ncontext reset; send your next message to continue there.",
        path = container_workspace_path(name),
    )
}

/// Render the `/switch` reply for a rejected argument (missing/invalid).
#[must_use]
pub fn render_switch_invalid(raw: &str) -> String {
    if raw.is_empty() {
        "Usage: /switch <name> — names may use letters, digits, '.', '_' and '-' only.".to_owned()
    } else {
        format!(
            "Invalid workspace name `{raw}` — use letters, digits, '.', '_' and '-' only \
             (no '/', '..', or spaces)."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_dir(root: &Path, name: &str) {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }

    #[test]
    fn empty_root_lists_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(list_workspaces(tmp.path()).is_empty());
        assert_eq!(
            render_projects(&[]),
            "No workspaces yet — say 'build me X', or /switch <name> to start one."
        );
    }

    #[test]
    fn lists_workspaces_excluding_system_and_dot_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Real workspaces.
        touch_dir(root, "fairway-focus");
        touch_dir(root, "notes");
        // System / infra dirs — must be excluded.
        for sys in ["skills", "memory", "inbox", "outbox", "node_modules"] {
            touch_dir(root, sys);
        }
        // Dot dirs — excluded by the leading-`.` rule.
        touch_dir(root, ".jobs");
        touch_dir(root, ".copperclaw");
        // A stray file (not a dir) — excluded.
        std::fs::write(root.join("README"), b"x").unwrap();

        let ws = list_workspaces(root);
        let names: Vec<&str> = ws.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["fairway-focus", "notes"]);
    }

    #[test]
    fn tags_git_repos_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_dir(root, "zeta");
        touch_dir(root, "alpha");
        std::fs::create_dir_all(root.join("alpha/.git")).unwrap();

        let ws = list_workspaces(root);
        assert_eq!(ws[0].name, "alpha");
        assert!(ws[0].is_git, "alpha has a .git dir");
        assert_eq!(ws[1].name, "zeta");
        assert!(!ws[1].is_git);
    }

    #[test]
    fn marks_active_from_switch_written_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_dir(root, "proj-a");
        touch_dir(root, "proj-b");
        // `/switch`-style state: a single bare `cd` line, no PWD dump.
        std::fs::write(root.join(SHELL_STATE_FILE), switch_shell_state("proj-b")).unwrap();

        let ws = list_workspaces(root);
        let a = ws.iter().find(|w| w.name == "proj-a").unwrap();
        let b = ws.iter().find(|w| w.name == "proj-b").unwrap();
        assert!(!a.active);
        assert!(b.active, "proj-b is the shell-state cwd");

        let text = render_projects(&ws);
        assert!(text.contains("\u{2192} proj-b"), "active marker: {text}");
        assert!(text.contains("  proj-a"), "inactive indent: {text}");
    }

    #[test]
    fn active_from_shell_tool_pwd_dump() {
        // The shell tool writes a leading `cd` line AND a dumped env; both
        // agree. Parsing either must find the workspace.
        let state =
            "cd /data/live-proj\ndeclare -x HOME=\"/data\"\ndeclare -x PWD=\"/data/live-proj\"\n";
        assert_eq!(active_from_shell_state(state).as_deref(), Some("live-proj"));
        // PWD-only (no cd line) still resolves via the fallback.
        let pwd_only = "declare -x OLDPWD=\"/data\"\ndeclare -x PWD=\"/data/other\"\n";
        assert_eq!(active_from_shell_state(pwd_only).as_deref(), Some("other"));
    }

    #[test]
    fn active_none_for_root_or_outside_data() {
        assert_eq!(active_from_shell_state("cd /data\n"), None);
        assert_eq!(active_from_shell_state("cd '/data'\n"), None);
        assert_eq!(active_from_shell_state("cd /tmp\n"), None);
        // Sibling-prefix guard: `/datax` is not under `/data`.
        assert_eq!(active_from_shell_state("cd /datax/foo\n"), None);
        assert_eq!(active_from_shell_state(""), None);
    }

    #[test]
    fn switch_shell_state_is_single_quoted_cd() {
        assert_eq!(switch_shell_state("my-proj"), "cd '/data/my-proj'\n");
    }

    #[test]
    fn render_switch_ok_new_vs_existing() {
        let new = render_switch_ok("app", true);
        assert!(new.contains("Switched to workspace `app`"));
        assert!(new.contains("/data/app (new)"));
        assert!(new.contains("context reset"));
        let existing = render_switch_ok("app", false);
        assert!(existing.contains("/data/app (existing)"));
    }

    #[test]
    fn render_switch_invalid_variants() {
        assert!(render_switch_invalid("").starts_with("Usage: /switch <name>"));
        let bad = render_switch_invalid("../etc");
        assert!(bad.contains("../etc"));
        assert!(bad.contains("no '/'"));
    }
}
