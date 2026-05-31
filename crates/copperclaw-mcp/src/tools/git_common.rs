//! Shared helpers for the `git_*` tool family: repository discovery,
//! path resolution, error-message wrapping. Kept in one place so the
//! four tools render libgit2 errors identically to the agent.

use crate::error::ToolError;
use std::path::{Path, PathBuf};

/// Resolve the caller-supplied `path` (or cwd if `None`) into an
/// absolute path. We accept any path *inside* a git repo — `git2`
/// then walks upwards to find the `.git` dir.
pub(super) fn resolve_path(path: Option<&str>) -> Result<PathBuf, ToolError> {
    let p = match path {
        Some(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => std::env::current_dir().map_err(|e| {
            ToolError::Internal(format!("git: read cwd failed: {e}"))
        })?,
    };
    Ok(p)
}

/// Open the git repository containing `path`. Walks upward like
/// `git rev-parse --show-toplevel`. All libgit2 errors are caught
/// and re-rendered as friendly validation messages.
pub(super) fn open_repo(path: &Path) -> Result<git2::Repository, ToolError> {
    if !path.exists() {
        return Err(ToolError::Validation(format!(
            "git: path does not exist: {}",
            path.display()
        )));
    }
    git2::Repository::discover(path).map_err(|e| match e.code() {
        git2::ErrorCode::NotFound => ToolError::Validation(format!(
            "git: not a git repo (or any parent): {}",
            path.display()
        )),
        _ => map_err("git: open repo", &e),
    })
}

/// Resolve a ref or revspec (`HEAD`, `HEAD~1`, `main`, `abc1234`) to a
/// commit. Friendly errors when the ref isn't known.
pub(super) fn resolve_commit<'r>(
    repo: &'r git2::Repository,
    spec: &str,
) -> Result<git2::Commit<'r>, ToolError> {
    let obj = repo.revparse_single(spec).map_err(|e| match e.code() {
        git2::ErrorCode::NotFound | git2::ErrorCode::InvalidSpec => {
            ToolError::Validation(format!("git: no such ref `{spec}`"))
        }
        _ => map_err(&format!("git: revparse `{spec}`"), &e),
    })?;
    let commit = obj.peel_to_commit().map_err(|e| {
        ToolError::Validation(format!(
            "git: `{spec}` does not resolve to a commit ({})",
            e.message()
        ))
    })?;
    Ok(commit)
}

/// Translate a libgit2 error into a generic `ToolError::Internal`.
/// Callers that want a more specific message (e.g. `NotFound` →
/// `Validation`) should match `error.code()` themselves first.
pub(super) fn map_err(prefix: &str, e: &git2::Error) -> ToolError {
    ToolError::Internal(format!("{prefix}: {}", e.message()))
}

/// Short (7-char) form of a commit SHA. We don't try to be clever
/// about uniqueness — the agent gets the full SHA too if it wants
/// disambiguation.
pub(super) fn short(oid: &git2::Oid) -> String {
    let s = oid.to_string();
    s.chars().take(7).collect()
}

/// Render a `git2::Time` as an RFC 3339 / ISO-8601 string in UTC.
/// libgit2 stores epoch seconds + tz offset; we normalise to UTC so
/// the agent doesn't have to think about timezones.
pub(super) fn time_to_rfc3339(t: git2::Time) -> String {
    let secs = t.seconds();
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0).map_or_else(
        || format!("@{secs}"),
        |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    //! Test helpers shared by every git_* tool's #[cfg(test)] module.
    use git2::{Repository, Signature};
    use std::path::Path;

    /// `git init`, write `file=content`, stage + commit "initial".
    /// Returns the commit OID. Configures `user.name` / `user.email`
    /// on the repo so the commit lands even on hosts that don't have
    /// a global git config.
    pub fn init_with_commit(path: &Path, file: &str, content: &str) -> git2::Oid {
        let repo = Repository::init(path).unwrap();
        // Pin a deterministic default branch so test assertions don't
        // depend on the host's `init.defaultBranch` setting.
        repo.set_head("refs/heads/main").unwrap();
        let fp = path.join(file);
        std::fs::write(&fp, content).unwrap();
        commit_existing(&repo, file, "initial").0
    }

    /// Stage `file` (already on disk) and commit with `message`.
    /// Returns `(oid, short)`.
    pub fn commit_existing(
        repo: &Repository,
        file: &str,
        message: &str,
    ) -> (git2::Oid, String) {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file)).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let sig = Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let oid = if let Some(parent) = parent {
            repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])
                .unwrap()
        } else {
            repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[])
                .unwrap()
        };
        let short = oid.to_string().chars().take(7).collect();
        (oid, short)
    }

    /// Modify `file` in-place, stage + commit with `message`.
    pub fn rewrite_and_commit(
        repo: &Repository,
        file: &str,
        new_content: &str,
        message: &str,
    ) -> (git2::Oid, String) {
        let path = repo.workdir().unwrap().join(file);
        std::fs::write(&path, new_content).unwrap();
        commit_existing(repo, file, message)
    }
}
