//! Shared sentinel-file helper for tools that signal the runner from
//! inside a tool call without a request/response.
//!
//! Several agent-callable tools need to ask the runner to do something
//! at the start of its next turn — e.g. `compact_now` (run the
//! existing `compact()`) and `clear_history` (wipe `state.history`).
//! The history lives in `RunnerState`, which `ToolContext` doesn't
//! reach, so the tools drop a sentinel file in `/data` and the
//! runner's main loop polls for it once per turn.
//!
//! This module owns:
//! - the directory resolution (`/data`, overridable via
//!   `COPPERCLAW_SENTINEL_DIR`, with a process-global test override),
//! - `sentinel_path(name)` so tools and the runner agree on file
//!   placement,
//! - `drop_sentinel(name)` as the one-call API for tool handlers.

use std::path::PathBuf;

use crate::error::ToolError;

const SENTINEL_DIR_DEFAULT: &str = "/data";
const SENTINEL_DIR_ENV_OVERRIDE: &str = "COPPERCLAW_SENTINEL_DIR";

#[cfg(test)]
static SENTINEL_DIR_TEST_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub fn sentinel_dir_test_override_set(dir: PathBuf) {
    let cell = SENTINEL_DIR_TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dir);
}

#[cfg(test)]
pub fn sentinel_dir_test_override_clear() {
    if let Some(cell) = SENTINEL_DIR_TEST_OVERRIDE.get() {
        *cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
fn sentinel_dir_test_override() -> Option<PathBuf> {
    SENTINEL_DIR_TEST_OVERRIDE.get().and_then(|m| {
        m.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    })
}

#[cfg(not(test))]
fn sentinel_dir_test_override() -> Option<PathBuf> {
    None
}

/// Serialised access to the process-global override above.
///
/// The override is one static shared by the whole test binary, and cargo
/// runs unit tests on a multi-threaded scheduler — so any two tests that
/// set it concurrently race, and the loser sees the winner's tempdir (or
/// a cleared override) and fails to find its sentinel. Every test that
/// touches the override must hold [`OverrideGuard`], which owns both the
/// lock and the tempdir for its lifetime and clears the override on drop.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Holds the `MutexGuard` as a field rather than a local so clippy's
    /// `await_holding_lock` is satisfied — the guard's lifetime otherwise
    /// straddles every `.await` in the test body.
    pub(crate) struct OverrideGuard {
        dir: tempfile::TempDir,
        _lock: MutexGuard<'static, ()>,
    }

    fn lock() -> &'static Mutex<()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(()))
    }

    impl OverrideGuard {
        pub(crate) fn new() -> Self {
            let lock = lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            super::sentinel_dir_test_override_set(dir.path().to_path_buf());
            Self { dir, _lock: lock }
        }

        /// The tempdir the override points at — where sentinels land.
        pub(crate) fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    impl Drop for OverrideGuard {
        fn drop(&mut self) {
            super::sentinel_dir_test_override_clear();
        }
    }
}

/// Resolve the directory sentinels are dropped in. Production: `/data`.
/// Operators can override via `COPPERCLAW_SENTINEL_DIR`. Tests install a
/// per-fixture tempdir via the `OnceLock` above.
pub fn sentinel_dir() -> PathBuf {
    if let Some(p) = sentinel_dir_test_override() {
        return p;
    }
    std::env::var_os(SENTINEL_DIR_ENV_OVERRIDE)
        .map_or_else(|| PathBuf::from(SENTINEL_DIR_DEFAULT), PathBuf::from)
}

/// Path the sentinel-named `name` will be written to (a dot-prefixed
/// filename under `sentinel_dir()`). The runner uses the same helper to
/// poll for it, so tool and runner always agree on placement.
#[must_use]
pub fn sentinel_path(name: &str) -> PathBuf {
    sentinel_dir().join(format!(".{name}"))
}

/// One-call API for sentinel-dropping tool handlers. Returns `Ok(())`
/// on a successful write; the caller (`tool::handle`) translates that
/// into the tool's success message.
pub async fn drop_sentinel(name: &str) -> Result<(), ToolError> {
    let path = sentinel_path(name);
    tokio::fs::write(&path, b"").await.map_err(|err| {
        ToolError::Internal(format!(
            "drop_sentinel({name}): write {}: {err}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::OverrideGuard;

    #[tokio::test]
    async fn drop_sentinel_writes_dotfile_under_dir() {
        let _g = OverrideGuard::new();
        drop_sentinel("test_pending").await.unwrap();
        assert!(sentinel_path("test_pending").exists());
    }

    #[tokio::test]
    async fn sentinel_path_matches_drop_target() {
        let _g = OverrideGuard::new();
        let path = sentinel_path("anything");
        drop_sentinel("anything").await.unwrap();
        assert!(path.exists(), "tool and runner must compute the same path");
    }
}
