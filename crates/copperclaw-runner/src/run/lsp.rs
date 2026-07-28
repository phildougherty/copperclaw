//! M22 C3 — the container-local symbol-index bridge (decision **(f)**).
//!
//! This is the *write* half of the C3 symbol surface: it builds the on-disk
//! index that the read-only `find_symbol` MCP tool
//! ([`copperclaw_mcp::tools::find_symbol`]) consumes. C2's attach flow
//! (`run/project.rs`) reaches it through the `trigger_symbol_index` seam
//! after opening an existing repository, so a newly-attached repo is
//! navigable by symbol on the next turn instead of grepped blind.
//!
//! ## Backend selection + clean degradation
//!
//! The bridge runs entirely inside the sandbox — **no host-side language
//! server, no writes outside `<repo>/.copperclaw/`, no outbound network**
//! (decision **(f)**). It picks the richest backend the image actually
//! carries and degrades cleanly when a heavier one is absent:
//!
//! 1. **Language-server *hint*** (rust-analyzer for a `Cargo.toml` repo,
//!    typescript-language-server / `tsserver` for a `package.json` /
//!    `tsconfig.json` repo) — probed for on `PATH` and *recorded* when
//!    present, nothing more: no server process is launched, no JSON-RPC
//!    is spoken, no query is ever answered by the server. The hint exists
//!    so a future card can add a live go-to-def path without disturbing
//!    this bridge or the tool contract. A real LSP client is intentionally
//!    deferred (its fragility is exactly what decision **(f)**'s pragmatism
//!    note steers away from); the queryable artifact today is the ctags
//!    index below, which every language the agent builds in can produce.
//! 2. **universal-ctags** — the actual index builder. `universal-ctags` is
//!    baked into the baseline session image (see `copperclaw-setup`'s image
//!    bake list), so this is the reliable common path. It writes a `tags`
//!    file to [`copperclaw_mcp::tools::find_symbol::TAGS_REL_PATH`], the one
//!    location the tool reads.
//! 3. **none** — on a minimal image with neither a server nor ctags, the
//!    bridge writes nothing and records that `find_symbol` will fall back to
//!    its scoped definition scan at query time. Nothing breaks; navigation
//!    just costs a walk instead of an index read.
//!
//! Building the index is strictly read-only with respect to the repo's own
//! files: ctags only *reads* sources and the sole write is the `tags` file
//! under `.copperclaw/` — the same write boundary C2's attach flow observes.

use std::path::{Path, PathBuf};

use copperclaw_mcp::tools::find_symbol::{TAGS_REL_PATH, parse_tags};

/// Which backend actually produced (or would have produced) the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// The ctags index was built, and a fitting language-server binary was
    /// *detected* on `PATH` alongside it. Despite the name, no server is
    /// launched or queried — symbol lookup is served entirely from the
    /// ctags index; the server name is a recorded hint for a future live
    /// go-to-def path (see the module docs).
    LanguageServerAssisted {
        /// The server binary that was found on `PATH` (never run).
        server: &'static str,
    },
    /// The ctags index was built (no language server present, or none fits).
    Ctags,
    /// Neither a language server nor ctags is available — no index written.
    None,
}

impl Backend {
    /// Stable label for logging.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::LanguageServerAssisted { .. } => "language-server-assisted",
            Self::Ctags => "ctags",
            Self::None => "none",
        }
    }
}

/// Outcome of one index build over a repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexOutcome {
    /// The backend selected.
    pub backend: Backend,
    /// Absolute path to the written `tags` file, when one was built.
    pub tags_path: Option<PathBuf>,
    /// Number of tag entries in the built index (0 when none was built).
    pub symbols_indexed: usize,
}

/// A language server that fits a repo, plus the concrete binary names to
/// probe for on `PATH` (first match wins).
struct LanguageServerFit {
    /// Human label used in logs / the recorded outcome.
    name: &'static str,
    /// Binary names to look for, in preference order.
    binaries: &'static [&'static str],
}

/// The language server that fits `repo`, by manifest, if any. Only decides
/// *which* server would serve go-to-def — availability is probed separately
/// so an absent binary degrades to ctags rather than being assumed present.
fn language_server_fit(repo: &Path) -> Option<LanguageServerFit> {
    if repo.join("Cargo.toml").is_file() {
        return Some(LanguageServerFit {
            name: "rust-analyzer",
            binaries: &["rust-analyzer"],
        });
    }
    if repo.join("package.json").is_file()
        || repo.join("tsconfig.json").is_file()
        || repo.join("jsconfig.json").is_file()
    {
        return Some(LanguageServerFit {
            name: "typescript-language-server",
            binaries: &["typescript-language-server", "tsserver"],
        });
    }
    None
}

/// Whether a binary named `name` exists on `PATH`.
fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

/// The first available language-server binary for `repo`, if the fitting
/// server is actually installed.
fn available_language_server(repo: &Path) -> Option<&'static str> {
    let fit = language_server_fit(repo)?;
    fit.binaries
        .iter()
        .copied()
        .find(|bin| binary_on_path(bin))
        .map(|_| fit.name)
}

/// Locate a `ctags` binary on `PATH`, if any.
fn ctags_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("ctags"))
        .find(|c| c.is_file())
}

/// Build (or refresh) the symbol index for `repo`.
///
/// Returns the [`IndexOutcome`] describing which backend ran. Best-effort:
/// an unwritable `.copperclaw/` dir or a ctags failure degrades to
/// [`Backend::None`] rather than propagating — indexing must never take a
/// turn down (same posture as C2's attach flow). Never writes outside
/// `<repo>/.copperclaw/` and never egresses.
pub async fn build_symbol_index(repo: &Path) -> IndexOutcome {
    let server = available_language_server(repo);
    let Some(ctags) = ctags_on_path() else {
        // No index builder available. find_symbol's grep tier covers this.
        return IndexOutcome {
            backend: Backend::None,
            tags_path: None,
            symbols_indexed: 0,
        };
    };

    let repo = repo.to_path_buf();
    // The ctags subprocess + the follow-up read are blocking; keep them off
    // the runner's async worker.
    let built = tokio::task::spawn_blocking(move || run_ctags_index(&ctags, &repo))
        .await
        .unwrap_or(None);

    match built {
        Some((tags_path, symbols_indexed)) => IndexOutcome {
            backend: match server {
                Some(server) => Backend::LanguageServerAssisted { server },
                None => Backend::Ctags,
            },
            tags_path: Some(tags_path),
            symbols_indexed,
        },
        None => IndexOutcome {
            backend: Backend::None,
            tags_path: None,
            symbols_indexed: 0,
        },
    }
}

/// Run ctags over `repo`, writing `<repo>/.copperclaw/tags`. Returns the
/// tags path + the number of entries, or `None` on any failure. Blocking.
fn run_ctags_index(ctags: &Path, repo: &Path) -> Option<(PathBuf, usize)> {
    let tags_path = repo.join(TAGS_REL_PATH);
    // Ensure `.copperclaw/` exists (the only dir we write to).
    let state_dir = tags_path.parent()?;
    std::fs::create_dir_all(state_dir).ok()?;

    // `--recurse` walks the tree; `--fields=+n` forces line numbers so the
    // tool gets a reliable file:line without re-searching. `-f <tags>`
    // writes into `.copperclaw/` and nowhere else; the trailing `.` scans
    // the repo root (cwd).
    let status = std::process::Command::new(ctags)
        .arg("--recurse")
        .arg("--fields=+n")
        .arg("-f")
        .arg(&tags_path)
        .arg(".")
        .current_dir(repo)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    let content = std::fs::read_to_string(&tags_path).ok()?;
    let count = parse_tags(&content).len();
    Some((tags_path, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_server_fit_by_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        // No manifest → no fit.
        assert!(language_server_fit(tmp.path()).is_none());

        // Rust.
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(
            language_server_fit(tmp.path()).map(|f| f.name),
            Some("rust-analyzer")
        );

        // Node (package.json wins over the absent Cargo case in a fresh dir).
        let node = tempfile::tempdir().unwrap();
        std::fs::write(node.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            language_server_fit(node.path()).map(|f| f.name),
            Some("typescript-language-server")
        );
    }

    #[tokio::test]
    async fn build_index_writes_only_under_state_dir_or_degrades() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(repo.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let src = repo.join("src/lib.rs");
        std::fs::write(
            &src,
            "pub fn build_widget() -> u8 { 0 }\npub struct Widget;\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&src).unwrap();

        let outcome = build_symbol_index(repo).await;

        // The repo's own source is never mutated, whatever backend ran.
        assert_eq!(std::fs::read_to_string(&src).unwrap(), before);

        if ctags_on_path().is_some() {
            // ctags present: a real index lands under `.copperclaw/`.
            assert!(matches!(
                outcome.backend,
                Backend::Ctags | Backend::LanguageServerAssisted { .. }
            ));
            let tags = outcome.tags_path.expect("tags path when ctags ran");
            assert!(tags.ends_with(TAGS_REL_PATH), "tags under .copperclaw/");
            assert!(tags.is_file());
            assert!(
                outcome.symbols_indexed > 0,
                "the two defs should be indexed"
            );
            // The only new top-level entry is `.copperclaw/`.
            assert!(!repo.join("tags").exists(), "no stray top-level tags file");
        } else {
            // No ctags (hermetic CI): degrade to None, write nothing.
            assert_eq!(outcome.backend, Backend::None);
            assert!(outcome.tags_path.is_none());
            assert_eq!(outcome.symbols_indexed, 0);
            assert!(!repo.join(TAGS_REL_PATH).exists());
        }
    }

    #[test]
    fn backend_labels_are_stable() {
        assert_eq!(Backend::Ctags.label(), "ctags");
        assert_eq!(Backend::None.label(), "none");
        assert_eq!(
            Backend::LanguageServerAssisted {
                server: "rust-analyzer"
            }
            .label(),
            "language-server-assisted"
        );
    }
}
