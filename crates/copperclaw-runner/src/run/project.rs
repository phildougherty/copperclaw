//! M22 C2 — open/attach an *existing* repository as the working project.
//!
//! Every coding flow before M22 assumed a blank prototype: the agent
//! `git init`s a fresh dir under `/data`, scaffolds, and writes its own
//! `.copperclaw/verify`. There was no first-class way to point the agent
//! at a repository it did **not** create — a `git clone <url>` — and have
//! the verify + self-review gates treat that existing code as a real
//! project.
//!
//! This module is that attach flow. It runs entirely on a *local path*
//! that already exists in the sandbox — the clone itself is ordinary
//! `shell` git run by the agent (architecture decision **(a)**: no
//! mutating `git_clone`/`git_commit` MCP tool). Given such a path the
//! flow:
//!
//! 1. **Infers verify stages** from the repo's own toolchain manifests
//!    (`Cargo.toml`, `package.json` scripts, `Makefile`, `pyproject.toml`)
//!    — plus, when the repo's `.env.example`/`.env` declares a database
//!    URL, a client-free `db:` TCP health stage (M23 W2.4) — and writes
//!    them to `<repo>/.copperclaw/verify` in the *exact* format
//!    the M20 multi-stage verify gate already consumes
//!    ([`copperclaw_mcp::tools::verify_gate::recorded_stages`]) — so the
//!    stages round-trip through the gate's own parser unchanged.
//! 2. **Seeds `<repo>/.copperclaw/DECISIONS.md`** with a short, factual
//!    summary drawn from the repo's README + top-level structure (the
//!    tool-free decision-log convention compaction already pins).
//! 3. **Triggers the C3 symbol index** through the [`trigger_symbol_index`]
//!    seam — a deliberate, documented no-op that card C3 (`find_symbol.rs`
//!    / `run/lsp.rs`) fills in later without touching this flow.
//! 4. **Marks the project attached** (`<repo>/.copperclaw/attached`) so the
//!    verify + self-review gates apply to the existing code and so the
//!    attach is idempotent — a second pass is a clean no-op and never
//!    clobbers an agent-authored `verify`.
//!
//! ## What triggers an attach
//!
//! [`auto_attach_pending`] is called from the runner poll loop
//! (`run/mod.rs`). It scans the data root for directories that look like a
//! *cloned* repo and attaches any not yet attached. The discriminator
//! between "an existing repo I cloned" and "a blank prototype I'm
//! scaffolding" is a configured **`origin` git remote**: `git clone` sets
//! one, `git init` does not (see [`has_origin_remote`]). That keeps the
//! flow off blank prototypes — which never gain an origin — while catching
//! exactly the clone case decision (a) describes, with no new tool and no
//! heuristic guesswork.
//!
//! ## Security posture (see `docs/plans/m22-security-reviews.md`, C2)
//!
//! The repo is arbitrary external content. This flow therefore **only
//! reads** files under the repo path and **only writes** inside
//! `<repo>/.copperclaw/`. It never executes repo code, never fetches
//! anything (no new network capability — the clone's egress is the
//! agent's shell git, already behind the modules egress/SSRF guard), reads
//! manifests/READMEs under a byte cap, does not traverse symlinked
//! directories, and maps only an **allowlist** of `package.json` script
//! names to fixed stage commands so a hostile manifest key can neither
//! inject extra verify lines nor smuggle a command.

use std::path::{Path, PathBuf};

use copperclaw_mcp::tools::verify_gate::Stage;

/// Per-project convention dir (mirrors `verify_gate`'s `.copperclaw`).
const STATE_DIR: &str = ".copperclaw";
/// The multi-stage verify file the M20 gate reads.
const VERIFY_FILE: &str = "verify";
/// Tool-free decision log compaction pins the tail of.
const DECISIONS_FILE: &str = "DECISIONS.md";
/// Presence marks a repo attached — idempotency + gate-applicability flag.
const ATTACHED_MARKER: &str = "attached";

/// Cap on bytes read from any single manifest / README. A real manifest
/// is a few KiB; the cap is a backstop against a pathological (or hostile)
/// multi-megabyte file blowing memory during a purely structural scan.
const MAX_READ_BYTES: u64 = 256 * 1024;
/// Cap on the README excerpt folded into the seeded decision log.
const MAX_README_SUMMARY_BYTES: usize = 600;
/// Cap on top-level entries listed in the seeded structure summary.
const MAX_STRUCTURE_ENTRIES: usize = 24;
/// Backstop on inferred stages so a pathological polyglot repo can't write
/// an unbounded verify file. A real repo lands in low single digits.
const MAX_INFERRED_STAGES: usize = 8;

/// Outcome of an attach pass over one repo path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachSummary {
    /// The repo that was (or would have been) attached.
    pub repo: PathBuf,
    /// `true` when this call performed the attach; `false` when the repo
    /// was already attached (idempotent no-op).
    pub attached: bool,
    /// The verify stages written to `.copperclaw/verify` (empty when none
    /// could be inferred, or when this was an idempotent no-op).
    pub verify_stages: Vec<Stage>,
    /// Whether a fresh `DECISIONS.md` was seeded this pass.
    pub seeded_decisions: bool,
}

// ── verify-stage inference ──────────────────────────────────────────────

/// Read at most [`MAX_READ_BYTES`] of `path` as UTF-8 (lossy), or `None`
/// if it doesn't exist / can't be read. Bounded so a hostile giant
/// manifest can't be slurped whole.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_READ_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The `package.json` script names we recognise, in the order they should
/// appear as verify stages, each paired with the stage name we emit. The
/// stage names are *fixed here* — never taken from the manifest — so a
/// manifest key like `"test\nrm -rf /"` can't inject a second verify line.
const NPM_SCRIPT_STAGES: &[(&str, &str)] = &[
    ("lint", "lint"),
    ("typecheck", "typecheck"),
    ("type-check", "typecheck"),
    ("test", "test"),
    ("build", "build"),
];

/// npm's scaffold placeholder test script — inferring a stage for it would
/// hard-fail every verify, so we skip it.
const NPM_PLACEHOLDER_TEST: &str = "Error: no test specified";

/// Infer Node stages from a `package.json`'s `scripts` map. Only allowlist
/// keys map to stages, and the command is always `npm run <key>` (or the
/// canonical `npm test`) — never the raw script body — so nothing from the
/// manifest reaches a shell verbatim through this flow.
fn infer_npm_stages(pkg_json: &str, out: &mut Vec<Stage>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(pkg_json) else {
        return;
    };
    let Some(scripts) = value.get("scripts").and_then(|s| s.as_object()) else {
        return;
    };
    for (key, stage_name) in NPM_SCRIPT_STAGES {
        let Some(body) = scripts.get(*key).and_then(|v| v.as_str()) else {
            continue;
        };
        if *key == "test" && body.contains(NPM_PLACEHOLDER_TEST) {
            continue;
        }
        let command = if *key == "test" {
            "npm test".to_string()
        } else {
            format!("npm run {key}")
        };
        push_unique_stage(out, stage_name, &command);
    }
}

/// Infer Rust stages from the presence of a `Cargo.toml`.
fn infer_cargo_stages(out: &mut Vec<Stage>) {
    push_unique_stage(out, "fmt", "cargo fmt --check");
    push_unique_stage(out, "check", "cargo check");
    push_unique_stage(out, "clippy", "cargo clippy");
    push_unique_stage(out, "test", "cargo test");
}

/// Infer stages from a `Makefile`'s target names. Only the conventional
/// `check` / `test` targets map, and only when actually declared.
fn infer_make_stages(makefile: &str, out: &mut Vec<Stage>) {
    for target in ["check", "test"] {
        if makefile_has_target(makefile, target) {
            push_unique_stage(out, target, &format!("make {target}"));
        }
    }
}

/// A Makefile declares `target` iff some line begins with `target:` (after
/// trimming leading whitespace). Recipe lines are tab-indented and never
/// match a bare `name:` at column zero, so this doesn't misfire on them.
fn makefile_has_target(makefile: &str, target: &str) -> bool {
    let needle = format!("{target}:");
    makefile.lines().any(|line| {
        let line = line.trim_start();
        line.strip_prefix(&needle)
            // A real target header is `name:` or `name: deps`, not
            // `name::=` (a `::=` assignment) — require the char after the
            // colon to not itself be `=`/`:`.
            .is_some_and(|rest| !rest.starts_with('=') && !rest.starts_with(':'))
    })
}

/// Infer Python stages from `pyproject.toml` content: ruff / mypy / pytest
/// as their tool tables (or a `tests/` dir) indicate.
fn infer_pyproject_stages(pyproject: &str, repo: &Path, out: &mut Vec<Stage>) {
    if pyproject.contains("[tool.ruff") || pyproject.contains("ruff") {
        push_unique_stage(out, "lint", "ruff check .");
    }
    if pyproject.contains("[tool.mypy") || pyproject.contains("mypy") {
        push_unique_stage(out, "typecheck", "mypy .");
    }
    if pyproject.contains("[tool.pytest")
        || pyproject.contains("pytest")
        || repo.join("tests").is_dir()
    {
        push_unique_stage(out, "test", "pytest");
    }
}

// ── M23 W2.4: database health-stage inference ───────────────────────────
//
// A project that declares a database dependency should not pass verify
// while that database is dead or was never restarted after an idle-stop
// (daemons die with the container; `/data` survives). Detection is the
// conventional env-file contract: a `DATABASE_URL=` / `REDIS_URL=` /
// `MONGO_URL=` / `MONGODB_URI=` line in `.env.example` or `.env` at the
// repo root. The emitted check is a plain bash TCP dial
// (`bash -c 'exec 3<>/dev/tcp/<host>/<port>'`) so it never depends on a
// DB client binary being installed in the container — host and port are
// parsed here, at inference time. Malformed or empty values are skipped
// silently; this can never fail an attach.

/// Env files probed for database URL declarations, in precedence order —
/// the first file that declares a var wins for that var.
const DB_ENV_FILES: &[&str] = &[".env.example", ".env"];

/// The env vars we recognise as database declarations, each paired with
/// the fixed stage name we emit. Stage names are *fixed here* — never
/// derived from repo content — mirroring [`NPM_SCRIPT_STAGES`]. Both
/// Mongo spellings map to the same stage name (deduped by
/// [`push_unique_stage`]).
const DB_URL_VARS: &[(&str, &str)] = &[
    ("DATABASE_URL", "db"),
    ("REDIS_URL", "db-redis"),
    ("MONGO_URL", "db-mongo"),
    ("MONGODB_URI", "db-mongo"),
];

/// Default TCP port per URL scheme, for URLs that omit an explicit port.
/// Unknown schemes get no default — a portless URL with an unknown
/// scheme is skipped rather than guessed at.
fn scheme_default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "postgres" | "postgresql" => Some(5432),
        "mysql" | "mariadb" => Some(3306),
        "redis" | "rediss" => Some(6379),
        "mongodb" => Some(27017),
        _ => None,
    }
}

/// Extract `var`'s value from dotenv-style `contents`: a `VAR=value`
/// line, optionally `export`-prefixed, value optionally quoted. Empty
/// values are skipped (scanning continues) so a commented-out template
/// line like `DATABASE_URL=` never shadows a later real one.
fn env_var_value(contents: &str, var: &str) -> Option<String> {
    for line in contents.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some(rest) = line.strip_prefix(var) else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if value.is_empty() {
            continue;
        }
        return Some(value.to_string());
    }
    None
}

/// Whether `host` is safe to splice into the single-quoted `bash -c`
/// command: hostname / IPv4 / IPv6 characters only. Anything else (a
/// quote, whitespace, a shell metacharacter from a hostile `.env`) makes
/// the URL "malformed" and the stage is silently skipped — repo content
/// can never smuggle shell syntax through this flow.
fn is_safe_db_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

/// Parse a database URL into the `(host, port)` a TCP reachability check
/// should dial. `None` for anything malformed: no `scheme://`, an
/// unparsable explicit port, an unknown scheme with no port, port 0, or
/// a host containing characters outside the hostname alphabet. Host
/// defaults to `127.0.0.1` when the URL omits it.
fn db_endpoint(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    // Authority = up to the first path / query / fragment delimiter,
    // with any `user:pass@` credentials stripped.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);

    let (host, explicit_port) = if let Some(inner) = hostport.strip_prefix('[') {
        // Bracketed IPv6: `[::1]` or `[::1]:5433`.
        let (host, after) = inner.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse::<u16>().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        (host, port)
    } else if hostport.matches(':').count() > 1 {
        // Bare (unbracketed) IPv6 — all colons belong to the address.
        (hostport, None)
    } else if let Some((h, p)) = hostport.rsplit_once(':') {
        (h, Some(p.parse::<u16>().ok()?))
    } else {
        (hostport, None)
    };

    let port = match explicit_port {
        Some(0) => return None,
        Some(p) => p,
        None => scheme_default_port(&scheme)?,
    };
    let host = if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    };
    if !is_safe_db_host(&host) {
        return None;
    }
    Some((host, port))
}

/// Infer database health stages from the repo's env files: one stage per
/// recognised URL var, in [`DB_URL_VARS`] order, each a client-free TCP
/// dial bash can run alone. Best-effort — anything unreadable or
/// malformed contributes nothing.
fn infer_db_stages(repo: &Path, out: &mut Vec<Stage>) {
    let mut values: Vec<Option<String>> = vec![None; DB_URL_VARS.len()];
    for file in DB_ENV_FILES {
        let Some(contents) = read_capped(&repo.join(file)) else {
            continue;
        };
        for (i, (var, _)) in DB_URL_VARS.iter().enumerate() {
            if values[i].is_none() {
                values[i] = env_var_value(&contents, var);
            }
        }
    }
    for (i, (_, stage_name)) in DB_URL_VARS.iter().enumerate() {
        let Some(url) = values[i].as_deref() else {
            continue;
        };
        let Some((host, port)) = db_endpoint(url) else {
            continue;
        };
        push_unique_stage(
            out,
            stage_name,
            &format!("bash -c 'exec 3<>/dev/tcp/{host}/{port}'"),
        );
    }
}

/// Push a stage unless one with the same name is already present (first
/// ecosystem wins). Keeps stage names unique — the verify gate keys its
/// per-stage pass/fail state by name, so a duplicate name would corrupt
/// the "every stage green" check.
fn push_unique_stage(out: &mut Vec<Stage>, name: &str, command: &str) {
    if out.len() >= MAX_INFERRED_STAGES || out.iter().any(|s| s.name == name) {
        return;
    }
    out.push(Stage {
        name: name.to_string(),
        command: command.to_string(),
    });
}

/// Infer the ordered verify stages for `repo` from its toolchain
/// manifests. Ecosystems are probed in a fixed order (Rust, Node, Python,
/// Make) and their stages concatenated with duplicate stage-names deduped.
/// A repo with no recognised manifest yields an empty list — attach still
/// proceeds (DECISIONS + marker), it just records no verify command, and
/// the gate stays inert for that project exactly as for a repo whose agent
/// never wrote a `verify`.
#[must_use]
pub fn infer_verify_stages(repo: &Path) -> Vec<Stage> {
    let mut stages = Vec::new();
    if repo.join("Cargo.toml").is_file() {
        infer_cargo_stages(&mut stages);
    }
    if let Some(pkg) = read_capped(&repo.join("package.json")) {
        infer_npm_stages(&pkg, &mut stages);
    }
    if let Some(py) = read_capped(&repo.join("pyproject.toml")) {
        infer_pyproject_stages(&py, repo, &mut stages);
    }
    if let Some(mk) = read_capped(&repo.join("Makefile")) {
        infer_make_stages(&mk, &mut stages);
    }
    infer_db_stages(repo, &mut stages);
    stages
}

/// Render inferred stages into the exact `.copperclaw/verify` text the M20
/// gate parses: one `name: command` line per stage, in order. Round-trips
/// through [`copperclaw_mcp::tools::verify_gate::recorded_stages`]
/// byte-for-stage (asserted in tests).
#[must_use]
pub fn render_verify_file(stages: &[Stage]) -> String {
    let mut out = String::new();
    for s in stages {
        out.push_str(&s.name);
        out.push_str(": ");
        out.push_str(&s.command);
        out.push('\n');
    }
    out
}

// ── DECISIONS.md seed ───────────────────────────────────────────────────

/// Build the seeded decision-log lines for `repo`: purely factual, drawn
/// only from the repo's own README + on-disk structure (never invented).
/// Returns the lines to append under a small attach header.
fn build_decisions_seed(repo: &Path, stages: &[Stage]) -> String {
    let name = repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let mut body = format!(
        "## Attached existing repository ({date})\n\n\
         - Opened existing repository `{name}` as the working project (attached, not scaffolded).\n"
    );
    if let Some(url) = origin_remote_url(repo) {
        body.push_str(&format!("- Clone origin: {url}\n"));
    }
    if stages.is_empty() {
        body.push_str("- No toolchain manifest recognised; no verify stages inferred.\n");
    } else {
        let names: Vec<&str> = stages.iter().map(|s| s.name.as_str()).collect();
        body.push_str(&format!(
            "- Inferred verify stages from repo manifests: {}.\n",
            names.join(", ")
        ));
    }
    if let Some(summary) = readme_summary(repo) {
        body.push_str(&format!("- README summary: {summary}\n"));
    }
    let structure = top_level_structure(repo);
    if !structure.is_empty() {
        body.push_str(&format!("- Top-level layout: {}.\n", structure.join(", ")));
    }
    body
}

/// First meaningful prose from the repo README (any of a few conventional
/// names), collapsed to a single line and byte-capped. Markdown heading
/// markers are stripped; nothing is paraphrased — the text is the repo's
/// own, verbatim.
fn readme_summary(repo: &Path) -> Option<String> {
    let readme = ["README.md", "README", "README.txt", "readme.md"]
        .iter()
        .map(|n| repo.join(n))
        .find(|p| p.is_file())?;
    let text = read_capped(&readme)?;
    let mut collapsed = String::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('#').trim();
        if line.is_empty() {
            if collapsed.is_empty() {
                continue;
            }
            break;
        }
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(line);
        if collapsed.len() >= MAX_README_SUMMARY_BYTES {
            break;
        }
    }
    if collapsed.is_empty() {
        return None;
    }
    Some(truncate_on_boundary(&collapsed, MAX_README_SUMMARY_BYTES))
}

/// Sorted top-level entry names (dirs suffixed `/`), skipping hidden
/// entries and the state dir, capped at [`MAX_STRUCTURE_ENTRIES`]. A plain
/// `read_dir` — symlinks are listed by name but never traversed.
fn top_level_structure(repo: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(repo) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Some(if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    names.sort();
    names.truncate(MAX_STRUCTURE_ENTRIES);
    names
}

/// Keep the first `max_bytes` bytes of `s`, rounded back to a char
/// boundary so the result is valid UTF-8.
fn truncate_on_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ── git origin discrimination ───────────────────────────────────────────

/// Whether `repo` is a git checkout with a configured `origin` remote —
/// i.e. it was `git clone`d (existing repo) rather than `git init`ed
/// (blank prototype). Reads only `<repo>/.git/config`; no subprocess.
#[must_use]
pub fn has_origin_remote(repo: &Path) -> bool {
    origin_remote_url(repo).is_some()
        || git_config(repo).is_some_and(|c| c.contains("[remote \"origin\"]"))
}

/// Read `<repo>/.git/config` if `.git` is a directory (the ordinary clone
/// layout). Bounded read; `None` for a non-git dir or a `.git` file
/// (worktree/submodule linkage — not a top-level clone we auto-attach).
fn git_config(repo: &Path) -> Option<String> {
    let git_dir = repo.join(".git");
    if !git_dir.is_dir() {
        return None;
    }
    read_capped(&git_dir.join("config"))
}

/// The `origin` remote URL from `.git/config`, if present. A light parse:
/// find the `[remote "origin"]` section header and the next `url =` line
/// within it.
fn origin_remote_url(repo: &Path) -> Option<String> {
    let config = git_config(repo)?;
    let mut in_origin = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_origin = trimmed == "[remote \"origin\"]";
            continue;
        }
        if in_origin {
            if let Some(rest) = trimmed.strip_prefix("url") {
                if let Some(eq) = rest.find('=') {
                    let url = rest[eq + 1..].trim();
                    if !url.is_empty() {
                        return Some(url.to_string());
                    }
                }
            }
        }
    }
    None
}

// ── C3 symbol-index seam ────────────────────────────────────────────────

/// C3 symbol-index hook. C2 attaches a repo and calls this so the C3
/// symbol-index bridge ([`crate::run::lsp`], feeding the `find_symbol` MCP
/// tool) builds or refreshes the index for `repo`.
///
/// Delegates to [`crate::run::lsp::build_symbol_index`], which picks the
/// richest available backend (a fitting language server when present,
/// otherwise `universal-ctags`) and degrades cleanly to no index — leaving
/// `find_symbol` to its scoped scan — on a minimal image. Best-effort and
/// read-only with respect to the repo's own files (the sole write is
/// `<repo>/.copperclaw/tags`), so it can never take an attach down. The
/// signature is unchanged from the C2 seam so this wires in without
/// touching the attach flow.
pub async fn trigger_symbol_index(repo: &Path) {
    let outcome = crate::run::lsp::build_symbol_index(repo).await;
    // M22 C3 metric: one build per attach, labelled by backend, with the
    // symbol count observed.
    copperclaw_metrics::inc_symbol_index_build(outcome.backend.label());
    copperclaw_metrics::observe_symbol_index_symbols(outcome.symbols_indexed as u64);
    tracing::info!(
        target: "copperclaw_runner",
        repo = %repo.display(),
        backend = outcome.backend.label(),
        symbols = outcome.symbols_indexed,
        "C3 symbol index: built for attached repository"
    );
}

// ── attach orchestration ────────────────────────────────────────────────

fn state_dir(repo: &Path) -> PathBuf {
    repo.join(STATE_DIR)
}

/// Whether `repo` has already been through the attach flow.
fn is_attached(repo: &Path) -> bool {
    state_dir(repo).join(ATTACHED_MARKER).is_file()
}

/// Whether `repo` already carries an agent-authored (or previously
/// inferred) `.copperclaw/verify` we must not clobber.
fn has_verify_file(repo: &Path) -> bool {
    let path = state_dir(repo).join(VERIFY_FILE);
    std::fs::metadata(&path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
}

/// Attach `repo` as an existing project: infer + record verify stages,
/// seed the decision log, trigger the C3 index seam, and drop the attached
/// marker. Idempotent — a second call is a no-op — and it never overwrites
/// an existing `.copperclaw/verify`.
///
/// Best-effort on individual writes (a marker-file write failure is logged,
/// not fatal), mirroring `verify_gate`'s posture: attaching a project must
/// not take the turn down.
pub async fn attach_project(repo: &Path) -> AttachSummary {
    if is_attached(repo) {
        return AttachSummary {
            repo: repo.to_path_buf(),
            attached: false,
            verify_stages: Vec::new(),
            seeded_decisions: false,
        };
    }

    let stages = infer_verify_stages(repo);
    let sdir = state_dir(repo);
    if let Err(err) = tokio::fs::create_dir_all(&sdir).await {
        tracing::warn!(
            target: "copperclaw_runner",
            repo = %repo.display(), error = %err,
            "C2 attach: could not create .copperclaw state dir; skipping attach"
        );
        return AttachSummary {
            repo: repo.to_path_buf(),
            attached: false,
            verify_stages: Vec::new(),
            seeded_decisions: false,
        };
    }

    // 1. Verify stages — never clobber an existing verify file.
    if !stages.is_empty() && !has_verify_file(repo) {
        write_best_effort(
            &sdir.join(VERIFY_FILE),
            render_verify_file(&stages).as_bytes(),
        )
        .await;
    }

    // 2. Seed / append the decision log. Append (don't overwrite) so a
    //    repo that already ships a DECISIONS.md keeps its history.
    let seeded = append_decisions(repo, &sdir, &stages).await;

    // 3. C3 symbol-index seam.
    trigger_symbol_index(repo).await;

    // 4. Attached marker (also the gate-applicability signal): record the
    //    timestamp so an operator can see when the repo was attached.
    write_best_effort(
        &sdir.join(ATTACHED_MARKER),
        chrono::Utc::now().to_rfc3339().as_bytes(),
    )
    .await;

    // M22 C2 metric: count one genuine attach + observe how many verify stages
    // were inferred from the repo's manifests.
    copperclaw_metrics::inc_repo_attach();
    copperclaw_metrics::observe_repo_attach_verify_stages_inferred(stages.len() as u64);

    tracing::info!(
        target: "copperclaw_runner",
        repo = %repo.display(),
        stages = stages.len(),
        "C2 attach: opened existing repository as working project"
    );

    AttachSummary {
        repo: repo.to_path_buf(),
        attached: true,
        verify_stages: stages,
        seeded_decisions: seeded,
    }
}

/// Append the attach seed to `<repo>/.copperclaw/DECISIONS.md`, creating
/// the file if absent. Returns whether a write happened.
async fn append_decisions(repo: &Path, sdir: &Path, stages: &[Stage]) -> bool {
    let seed = build_decisions_seed(repo, stages);
    let path = sdir.join(DECISIONS_FILE);
    let existing = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let mut contents = existing;
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    if !contents.is_empty() {
        contents.push('\n');
    }
    contents.push_str(&seed);
    write_best_effort(&path, contents.as_bytes()).await;
    true
}

async fn write_best_effort(path: &Path, contents: &[u8]) {
    if let Err(err) = tokio::fs::write(path, contents).await {
        tracing::warn!(
            target: "copperclaw_runner",
            path = %path.display(), error = %err,
            "C2 attach: marker-file write failed (continuing)"
        );
    }
}

// ── auto-attach scan (the run/mod.rs seam feeds this) ───────────────────

/// A directory is an *attachable existing repo* iff it is a git checkout
/// with an `origin` remote (a clone, not a blank `git init` prototype),
/// carries a recognised toolchain manifest, and has not been attached yet.
/// The origin-remote gate is what keeps auto-attach off prototypes the
/// agent is building from scratch.
#[must_use]
pub fn is_attachable_existing_repo(dir: &Path) -> bool {
    if !dir.is_dir() || is_attached(dir) || has_verify_file(dir) {
        return false;
    }
    if !has_origin_remote(dir) {
        return false;
    }
    has_recognised_manifest(dir)
}

fn has_recognised_manifest(dir: &Path) -> bool {
    ["Cargo.toml", "package.json", "pyproject.toml", "Makefile"]
        .iter()
        .any(|m| dir.join(m).is_file())
}

/// Resolve the container's data root the same way `verify_gate` does:
/// `COPPERCLAW_DATA_ROOT` when set, else `/data`. Keeping the resolution
/// identical means auto-attach and the verify gate always agree on which
/// directories are projects.
pub(super) fn resolve_data_root() -> PathBuf {
    std::env::var_os("COPPERCLAW_DATA_ROOT").map_or_else(|| PathBuf::from("/data"), PathBuf::from)
}

/// Scan the data root and attach every not-yet-attached existing repo
/// found directly beneath it. Returns the summaries of repos actually
/// attached this pass (empty on the common steady-state poll where nothing
/// new was cloned). Best-effort: an unreadable data root yields an empty
/// result rather than an error. Called from the runner poll loop.
pub async fn auto_attach_pending() -> Vec<AttachSummary> {
    auto_attach_in(&resolve_data_root()).await
}

/// [`auto_attach_pending`] against an explicit root — the testable core.
pub async fn auto_attach_in(data_root: &Path) -> Vec<AttachSummary> {
    let mut attached = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(data_root).await else {
        return attached;
    };
    // Collect candidate paths first (sorted) so behaviour is deterministic.
    let mut candidates: Vec<PathBuf> = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        candidates.push(entry.path());
    }
    candidates.sort();
    for path in candidates {
        if is_attachable_existing_repo(&path) {
            let summary = attach_project(&path).await;
            if summary.attached {
                attached.push(summary);
            }
        }
    }
    attached
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute path to the checked-in fixture repo.
    fn fixture_repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/repos/todo-tracker")
            .canonicalize()
            .expect("fixture repo exists")
    }

    /// Copy a directory tree (files + subdirs) — enough to stage the
    /// read-only fixture into a writable tempdir for attach tests.
    fn copy_tree(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap().flatten() {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }

    /// Give `repo` a `.git/config` with an `origin` remote so the
    /// clone-discriminator fires (a real `.git` dir can't be committed as
    /// a fixture — git ignores nested `.git` trees — so tests synthesise
    /// the one file the discriminator reads).
    fn make_cloned(repo: &Path, url: &str) {
        let git = repo.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(
            git.join("config"),
            format!("[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"),
        )
        .unwrap();
    }

    fn staged_fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("todo-tracker");
        copy_tree(&fixture_repo(), &repo);
        (tmp, repo)
    }

    // ── verify-stage inference (unit) ────────────────────────────────

    #[test]
    fn infers_npm_stages_from_fixture_package_json() {
        let stages = infer_verify_stages(&fixture_repo());
        assert_eq!(
            stages,
            vec![
                Stage {
                    name: "lint".into(),
                    command: "npm run lint".into()
                },
                Stage {
                    name: "typecheck".into(),
                    command: "npm run typecheck".into()
                },
                Stage {
                    name: "test".into(),
                    command: "npm test".into()
                },
                Stage {
                    name: "build".into(),
                    command: "npm run build".into()
                },
            ],
            "fixture package.json scripts map to lint/typecheck/test/build in order"
        );
    }

    #[test]
    fn infers_cargo_stages_for_a_rust_repo() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["fmt", "check", "clippy", "test"]
        );
        assert_eq!(stages[1].command, "cargo check");
    }

    #[test]
    fn infers_make_targets_only_when_declared() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Makefile"),
            "test:\n\t./run-tests.sh\n\nlint:\n\t./lint.sh\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        // `test` is declared → mapped; `check` is not declared → absent;
        // `lint` isn't in the Makefile allowlist → absent.
        assert_eq!(
            stages,
            vec![Stage {
                name: "test".into(),
                command: "make test".into()
            }]
        );
    }

    #[test]
    fn infers_pyproject_stages_from_tool_tables() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[tool.ruff]\nline-length = 100\n[tool.pytest.ini_options]\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages,
            vec![
                Stage {
                    name: "lint".into(),
                    command: "ruff check .".into()
                },
                Stage {
                    name: "test".into(),
                    command: "pytest".into()
                },
            ]
        );
    }

    #[test]
    fn skips_npm_placeholder_test_script() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        assert!(
            infer_verify_stages(tmp.path()).is_empty(),
            "the scaffold placeholder test must not become a (perma-failing) stage"
        );
    }

    #[test]
    fn hostile_manifest_key_cannot_inject_a_verify_line() {
        let tmp = tempfile::tempdir().unwrap();
        // A malicious key with an embedded newline + injected command. It
        // is not in the allowlist, so it maps to nothing at all.
        std::fs::write(
            tmp.path().join("package.json"),
            "{\"scripts\":{\"test\\nrm -rf /\":\"whatever\",\"test\":\"jest\"}}",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages,
            vec![Stage {
                name: "test".into(),
                command: "npm test".into()
            }],
            "only the allowlisted `test` key maps; the injection key is ignored, command is fixed"
        );
    }

    #[test]
    fn polyglot_repo_dedupes_stage_names_first_wins() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(
            tmp.path().join("Makefile"),
            "test:\n\techo hi\ncheck:\n\techo hi\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        // Cargo contributes fmt/check/clippy/test first; Make's test/check
        // collide by name and are dropped.
        assert_eq!(
            stages
                .iter()
                .map(|s| s.command.as_str())
                .collect::<Vec<_>>(),
            vec![
                "cargo fmt --check",
                "cargo check",
                "cargo clippy",
                "cargo test"
            ],
        );
    }

    // ── M23 W2.4: db health-stage inference (unit) ───────────────────

    #[test]
    fn infers_db_stage_from_env_example() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".env.example"),
            "# app config\nDATABASE_URL=postgres://app:secret@localhost/app\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages,
            vec![Stage {
                name: "db".into(),
                command: "bash -c 'exec 3<>/dev/tcp/localhost/5432'".into()
            }],
            ".env.example DATABASE_URL yields a client-free TCP db stage"
        );
    }

    #[test]
    fn infers_db_stages_from_env_when_no_example() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".env"),
            "REDIS_URL=redis://cache/0\nMONGODB_URI=\"mongodb://mongo\"\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages,
            vec![
                Stage {
                    name: "db-redis".into(),
                    command: "bash -c 'exec 3<>/dev/tcp/cache/6379'".into()
                },
                Stage {
                    name: "db-mongo".into(),
                    command: "bash -c 'exec 3<>/dev/tcp/mongo/27017'".into()
                },
            ],
            "plain .env works too; quoted values are unwrapped; scheme defaults apply"
        );
    }

    #[test]
    fn db_stage_scheme_and_host_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        // No host in the URL → 127.0.0.1; mysql scheme → 3306.
        std::fs::write(
            tmp.path().join(".env.example"),
            "export DATABASE_URL=mysql:///app\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages,
            vec![Stage {
                name: "db".into(),
                command: "bash -c 'exec 3<>/dev/tcp/127.0.0.1/3306'".into()
            }]
        );
    }

    #[test]
    fn db_stage_respects_explicit_port() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".env.example"),
            "DATABASE_URL=postgres://user@db.internal:6543/app?sslmode=disable\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages,
            vec![Stage {
                name: "db".into(),
                command: "bash -c 'exec 3<>/dev/tcp/db.internal/6543'".into()
            }],
            "an explicit port in the URL overrides the scheme default"
        );
    }

    #[test]
    fn malformed_db_url_lines_are_skipped_silently() {
        let tmp = tempfile::tempdir().unwrap();
        // No scheme, unparsable port, unknown scheme with no port, empty
        // value, and a hostile host that would escape the single-quoted
        // bash command — every one contributes nothing, no error.
        std::fs::write(
            tmp.path().join(".env.example"),
            concat!(
                "DATABASE_URL=not-a-url\n",
                "REDIS_URL=redis://cache:notaport/0\n",
                "MONGO_URL=weird://host/db\n",
                "MONGODB_URI=\n",
            ),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".env"),
            "DATABASE_URL=postgres://x'; rm -rf /'@evil/app\n",
        )
        .unwrap();
        assert!(
            infer_verify_stages(tmp.path()).is_empty(),
            "malformed / hostile URL values are skipped, never a hard error"
        );
    }

    #[test]
    fn no_db_vars_means_no_db_stage() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(
            tmp.path().join(".env.example"),
            "APP_ENV=production\nLOG_LEVEL=info\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert!(
            stages.iter().all(|s| !s.name.starts_with("db")),
            "an env file with no recognised URL vars adds no db stage"
        );
    }

    #[test]
    fn db_stage_appends_after_manifest_stages() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(
            tmp.path().join(".env.example"),
            "DATABASE_URL=postgres://localhost/app\n",
        )
        .unwrap();
        let names: Vec<String> = infer_verify_stages(tmp.path())
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, vec!["fmt", "check", "clippy", "test", "db"]);
    }

    #[test]
    fn env_example_wins_over_env_for_the_same_var() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".env.example"),
            "DATABASE_URL=postgres://declared:5433/app\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join(".env"),
            "DATABASE_URL=postgres://other:9999/app\n",
        )
        .unwrap();
        let stages = infer_verify_stages(tmp.path());
        assert_eq!(
            stages[0].command,
            "bash -c 'exec 3<>/dev/tcp/declared/5433'"
        );
    }

    #[tokio::test]
    async fn db_stage_roundtrips_through_the_gate_parser() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(
            repo.join(".env.example"),
            "DATABASE_URL=postgres://127.0.0.1/app\n",
        )
        .unwrap();
        let inferred = infer_verify_stages(repo);
        std::fs::create_dir_all(repo.join(STATE_DIR)).unwrap();
        std::fs::write(
            repo.join(STATE_DIR).join(VERIFY_FILE),
            render_verify_file(&inferred),
        )
        .unwrap();
        let parsed = copperclaw_mcp::tools::verify_gate::recorded_stages(repo, None).await;
        assert_eq!(
            parsed, inferred,
            "the db stage parses back through the M20 gate unchanged"
        );
    }

    #[tokio::test]
    async fn attach_with_db_env_never_clobbers_existing_verify() {
        let (_tmp, repo) = staged_fixture();
        std::fs::write(
            repo.join(".env.example"),
            "DATABASE_URL=postgres://localhost/app\n",
        )
        .unwrap();
        let sdir = repo.join(STATE_DIR);
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(sdir.join(VERIFY_FILE), "custom: ./my-check.sh\n").unwrap();
        attach_project(&repo).await;
        assert_eq!(
            std::fs::read_to_string(sdir.join(VERIFY_FILE)).unwrap(),
            "custom: ./my-check.sh\n",
            "db inference only applies when attach is generating the verify file"
        );
    }

    // ── format round-trips through the real gate parser ──────────────

    #[tokio::test]
    async fn rendered_verify_roundtrips_through_the_gate_parser() {
        let (_tmp, repo) = staged_fixture();
        let inferred = infer_verify_stages(&repo);
        // Write the file exactly as attach does, then read it back through
        // the M20 gate's own parser — the whole point is byte-compatibility.
        std::fs::create_dir_all(repo.join(STATE_DIR)).unwrap();
        std::fs::write(
            repo.join(STATE_DIR).join(VERIFY_FILE),
            render_verify_file(&inferred),
        )
        .unwrap();
        let parsed = copperclaw_mcp::tools::verify_gate::recorded_stages(&repo, None).await;
        assert_eq!(
            parsed, inferred,
            "attach's verify file must parse back identically"
        );
    }

    // ── attach orchestration (unit) ──────────────────────────────────

    #[tokio::test]
    async fn attach_writes_verify_decisions_and_marker() {
        let (_tmp, repo) = staged_fixture();
        let summary = attach_project(&repo).await;
        assert!(summary.attached);
        assert_eq!(summary.verify_stages.len(), 4);
        assert!(summary.seeded_decisions);
        let sdir = repo.join(STATE_DIR);
        assert!(sdir.join(VERIFY_FILE).is_file());
        assert!(sdir.join(ATTACHED_MARKER).is_file());
        let decisions = std::fs::read_to_string(sdir.join(DECISIONS_FILE)).unwrap();
        assert!(decisions.contains("Attached existing repository"));
        assert!(
            decisions.contains("todo-tracker"),
            "decision seed names the repo"
        );
        assert!(
            decisions.contains("README summary:"),
            "decision seed folds in the repo's own README prose"
        );
    }

    #[tokio::test]
    async fn attach_is_idempotent() {
        let (_tmp, repo) = staged_fixture();
        let first = attach_project(&repo).await;
        assert!(first.attached);
        let verify_before =
            std::fs::read_to_string(repo.join(STATE_DIR).join(VERIFY_FILE)).unwrap();
        let second = attach_project(&repo).await;
        assert!(!second.attached, "second attach is a no-op");
        let verify_after = std::fs::read_to_string(repo.join(STATE_DIR).join(VERIFY_FILE)).unwrap();
        assert_eq!(
            verify_before, verify_after,
            "verify file untouched on re-attach"
        );
    }

    #[tokio::test]
    async fn attach_never_clobbers_an_existing_verify() {
        let (_tmp, repo) = staged_fixture();
        let sdir = repo.join(STATE_DIR);
        std::fs::create_dir_all(&sdir).unwrap();
        std::fs::write(sdir.join(VERIFY_FILE), "custom: ./my-check.sh\n").unwrap();
        attach_project(&repo).await;
        assert_eq!(
            std::fs::read_to_string(sdir.join(VERIFY_FILE)).unwrap(),
            "custom: ./my-check.sh\n",
            "an agent-authored verify survives attach"
        );
    }

    // ── clone discrimination ─────────────────────────────────────────

    #[test]
    fn origin_remote_detected_from_git_config() {
        let (_tmp, repo) = staged_fixture();
        assert!(!has_origin_remote(&repo), "no .git yet");
        make_cloned(&repo, "https://example.com/acme/todo-tracker.git");
        assert!(has_origin_remote(&repo));
        assert_eq!(
            origin_remote_url(&repo).as_deref(),
            Some("https://example.com/acme/todo-tracker.git")
        );
    }

    #[test]
    fn blank_prototype_without_origin_is_not_attachable() {
        let tmp = tempfile::tempdir().unwrap();
        let proto = tmp.path().join("fresh-proto");
        std::fs::create_dir_all(&proto).unwrap();
        std::fs::write(proto.join("package.json"), r#"{"scripts":{"test":"jest"}}"#).unwrap();
        // `git init` with no remote → a bare .git/config, no origin.
        std::fs::create_dir_all(proto.join(".git")).unwrap();
        std::fs::write(
            proto.join(".git").join("config"),
            "[core]\n\tbare = false\n",
        )
        .unwrap();
        assert!(
            !is_attachable_existing_repo(&proto),
            "a blank prototype (no origin remote) must never auto-attach"
        );
    }

    #[test]
    fn cloned_repo_with_manifest_is_attachable() {
        let (_tmp, repo) = staged_fixture();
        make_cloned(&repo, "git@github.com:acme/todo-tracker.git");
        assert!(is_attachable_existing_repo(&repo));
    }

    // ── auto-attach scan + the see→edit→gate-passes integration ──────

    #[tokio::test]
    async fn auto_attach_only_touches_cloned_repos() {
        let root = tempfile::tempdir().unwrap();
        // A cloned existing repo (attachable) …
        let cloned = root.path().join("todo-tracker");
        copy_tree(&fixture_repo(), &cloned);
        make_cloned(&cloned, "https://example.com/acme/todo-tracker.git");
        // … a blank prototype (no origin) …
        let proto = root.path().join("fresh-proto");
        std::fs::create_dir_all(&proto).unwrap();
        std::fs::write(proto.join("Cargo.toml"), "[package]\nname=\"p\"\n").unwrap();
        std::fs::create_dir_all(proto.join(".git")).unwrap();
        std::fs::write(proto.join(".git").join("config"), "[core]\n").unwrap();
        // … and a non-project dir (e.g. the memory store).
        std::fs::create_dir_all(root.path().join("memory")).unwrap();

        let attached = auto_attach_in(root.path()).await;
        assert_eq!(attached.len(), 1, "only the cloned repo attaches");
        assert_eq!(attached[0].repo, cloned);
        assert!(cloned.join(STATE_DIR).join(ATTACHED_MARKER).is_file());
        assert!(!proto.join(STATE_DIR).join(ATTACHED_MARKER).exists());

        // Idempotent: a second scan attaches nothing new.
        assert!(auto_attach_in(root.path()).await.is_empty());
    }

    /// Acceptance integration: the agent opens a fixture repo, the attach
    /// flow infers its verify, an edit marks the project dirty, running
    /// (each stage of) the inferred verify clears it, and the completion
    /// gate would pass. Driven through the *real* M20 verify-gate state
    /// machine so it proves C2's output plugs into the existing gate.
    #[tokio::test]
    async fn opened_repo_edit_then_inferred_verify_clears_the_gate() {
        use copperclaw_mcp::tools::verify_gate;

        let (_tmp, repo) = staged_fixture();
        make_cloned(&repo, "https://example.com/acme/todo-tracker.git");

        // Open/attach.
        let summary = attach_project(&repo).await;
        assert!(summary.attached);
        let stages = verify_gate::recorded_stages(&repo, None).await;
        assert_eq!(stages, summary.verify_stages);
        assert!(!stages.is_empty());

        // An edit to a source file marks the project dirty (as the edit
        // tools do via `mark_dirty`).
        std::fs::write(repo.join("src").join("store.js"), "// edited\n").unwrap();
        verify_gate::mark_dirty(&repo).await;
        assert!(verify_gate::is_dirty(&repo).await);
        assert!(!verify_gate::all_stages_passed(&repo, &stages).await);

        // Run the inferred verify: each recorded stage passes (the shell
        // tool records this on a clean exit). Once every stage is green the
        // dirty marker clears — the gate now passes.
        for stage in &stages {
            verify_gate::record_stage_result(&repo, &stage.name, true).await;
        }
        assert!(verify_gate::all_stages_passed(&repo, &stages).await);
        verify_gate::clear_dirty(&repo).await;
        assert!(
            !verify_gate::is_dirty(&repo).await,
            "with every inferred stage green the completion gate passes"
        );
    }

    #[test]
    fn readme_summary_uses_repo_own_prose() {
        let (_tmp, repo) = staged_fixture();
        let summary = readme_summary(&repo).expect("fixture has a README");
        assert!(summary.starts_with("todo-tracker"));
        assert!(summary.len() <= MAX_README_SUMMARY_BYTES);
    }
}
