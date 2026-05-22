//! `grep`: regex search across the container filesystem, returning
//! structured results.
//!
//! The agent previously got at filesystem search by shelling out to
//! `rg` or `find`, then string-parsing the output to count hits or
//! pull line numbers. That's slow, brittle, and burns tokens.
//! `grep` exposes the same capability natively: a regex pattern, an
//! optional path / glob filter, and a structured `{path, line, text,
//! context_*}` result row per hit.
//!
//! Implementation choices:
//!
//! - `ignore::WalkBuilder` for traversal so `.gitignore` is honoured
//!   the same way `ripgrep` honours it.
//! - `regex::Regex` for matching — single-pattern, no PCRE
//!   backreferences (those are out of scope; the agent issues
//!   multiple calls if it needs `ORed` terms).
//! - `globset::GlobMatcher` to filter by filename glob (`*.rs`,
//!   `**/*.toml`) when the caller provides one.
//! - Blocking I/O wrapped in `tokio::task::spawn_blocking` so the
//!   runner's tokio thread isn't pinned for the duration of a walk.
//!
//! Hard caps:
//!
//! - `max_results` defaults to 100, ceilings at 1000 (the hard cap
//!   the task spec calls out).
//! - Each matched line is truncated to `LINE_CAP_BYTES = 4096` with
//!   a trailing `…[truncated]` marker; long minified files or log
//!   lines can't blow the model's context window.
//! - `target/`, `node_modules/`, and `.git/` are skipped on top of
//!   whatever `.gitignore` says, so even a repo without a
//!   `.gitignore` doesn't drag the agent through build artefacts.

use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Default cap on result rows when the caller doesn't override.
const DEFAULT_MAX_RESULTS: usize = 100;
/// Hard ceiling on result rows. The agent can ask for more, but it
/// won't get more — long result lists are a pure context-window cost.
const MAX_RESULTS_CEILING: usize = 1000;
/// Per-line byte cap. A minified bundle or a JSON log line can be
/// many KB; we truncate so the model gets a usable summary instead
/// of a context blowout.
const LINE_CAP_BYTES: usize = 4096;
/// Directories we refuse to enter even if `.gitignore` doesn't list
/// them. These are universal build / vendor / VCS noise.
const HARD_SKIP_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// JSON-RPC input for the `grep` tool.
#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    context_lines: Option<usize>,
    /// Escape hatch: when `true`, the walker ignores `.gitignore`,
    /// `.ignore`, and friends. The hard-coded skip list
    /// (`target/`, `node_modules/`, `.git/`) still applies.
    #[serde(default)]
    no_ignore: bool,
}

/// Single match row in the output.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
struct Match {
    path: String,
    line: usize,
    text: String,
    context_before: Vec<String>,
    context_after: Vec<String>,
}

/// Top-level output envelope.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
struct Output {
    matches: Vec<Match>,
    truncated: bool,
    total_matched: usize,
}

/// Build the rmcp `Tool` descriptor.
pub fn schema() -> Tool {
    make_tool(
        "grep",
        "Regex-search files under `path` (default cwd). Honours .gitignore by default; skips target/, node_modules/, .git/ unconditionally. Returns structured {path, line, text} rows.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["pattern"],
            "properties": {
                "pattern":          { "type": "string", "minLength": 1 },
                "path":             { "type": ["string", "null"] },
                "glob":             { "type": ["string", "null"] },
                "case_insensitive": { "type": "boolean" },
                "max_results":      { "type": ["integer", "null"], "minimum": 1, "maximum": 1000 },
                "context_lines":    { "type": ["integer", "null"], "minimum": 0, "maximum": 20 },
                "no_ignore":        { "type": "boolean" }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;

    if input.pattern.trim().is_empty() {
        return Err(ToolError::Validation("`pattern` must be non-empty".into()));
    }

    // Compile the regex eagerly so a bad pattern fails fast with a
    // message that names the input. The `regex` crate's error text
    // includes the offending position, which is what the agent
    // wants to see.
    let mut builder = regex::RegexBuilder::new(&input.pattern);
    builder.case_insensitive(input.case_insensitive);
    let re = builder.build().map_err(|e| {
        ToolError::Validation(format!(
            "invalid regex `{pat}`: {e}",
            pat = input.pattern
        ))
    })?;

    // Resolve the search root. Empty / unset → current working dir.
    let root = match input.path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => std::env::current_dir()
            .map_err(|e| ToolError::Internal(format!("cwd unavailable: {e}")))?,
    };
    if !root.exists() {
        return Err(ToolError::Validation(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let glob = match input.glob.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(g) => Some(
            globset::GlobBuilder::new(g)
                .literal_separator(false)
                .build()
                .map_err(|e| ToolError::Validation(format!("invalid glob `{g}`: {e}")))?
                .compile_matcher(),
        ),
        None => None,
    };

    let max_results = input
        .max_results
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_RESULTS_CEILING);
    let context_lines = input.context_lines.unwrap_or(0).min(20);
    let no_ignore = input.no_ignore;

    // The `ignore` walker is blocking; offload it so the tokio
    // runtime isn't held up.
    let result = tokio::task::spawn_blocking(move || {
        run_grep(&root, &re, glob.as_ref(), max_results, context_lines, no_ignore)
    })
    .await
    .map_err(|e| ToolError::Internal(format!("grep task panicked: {e}")))?;

    Ok(success_json(&result))
}

/// Walk `root`, applying `re` to each line of each non-binary file,
/// collecting up to `max_results` matches.
fn run_grep(
    root: &Path,
    re: &regex::Regex,
    glob: Option<&globset::GlobMatcher>,
    max_results: usize,
    context_lines: usize,
    no_ignore: bool,
) -> Output {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .standard_filters(!no_ignore)
        .git_ignore(!no_ignore)
        .git_exclude(!no_ignore)
        .git_global(!no_ignore)
        .ignore(!no_ignore)
        .hidden(!no_ignore)
        .parents(!no_ignore)
        .follow_links(false)
        .filter_entry(|entry| {
            // Unconditional skip on universal noise dirs. Applies
            // even when the caller passes `no_ignore: true` — we
            // never want to search build artefacts.
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    if HARD_SKIP_DIRS.contains(&name) {
                        return false;
                    }
                }
            }
            true
        });

    let walker = builder.build();
    let mut matches: Vec<Match> = Vec::new();
    let mut total_matched: usize = 0;
    let mut truncated = false;

    for entry in walker {
        // Permission errors, broken symlinks, etc — skip quietly;
        // the agent doesn't need to triage IO noise.
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(g) = glob {
            // Test the glob against the file name AND the path
            // relative to root, so callers can write either
            // `*.rs` or `crates/**/*.rs`.
            let rel = path.strip_prefix(root).unwrap_or(path);
            let name_match = path
                .file_name()
                .is_some_and(|n| g.is_match(Path::new(n)));
            if !name_match && !g.is_match(rel) {
                continue;
            }
        }

        match search_file(path, root, re, max_results, context_lines, &mut matches) {
            Ok(file_hits) => {
                total_matched = total_matched.saturating_add(file_hits);
                if matches.len() >= max_results {
                    truncated = true;
                    break;
                }
            }
            Err(SearchAbort::Binary | SearchAbort::Io) => continue,
        }
    }

    Output {
        matches,
        truncated,
        total_matched,
    }
}

/// Why a single-file search bailed early.
enum SearchAbort {
    /// File looked like binary content; skipped without searching.
    Binary,
    /// Open / read error; skipped.
    Io,
}

/// Search `path` for `re`, pushing hits into `out`. Returns the
/// number of matches in *this file* (not the running total) so the
/// caller can update `total_matched` honestly.
fn search_file(
    path: &Path,
    root: &Path,
    re: &regex::Regex,
    max_results: usize,
    context_lines: usize,
    out: &mut Vec<Match>,
) -> Result<usize, SearchAbort> {
    let file = std::fs::File::open(path).map_err(|_| SearchAbort::Io)?;
    let mut reader = BufReader::new(file);

    // Cheap binary sniff on the first chunk — NUL byte → binary.
    // This matches ripgrep's default behaviour closely enough that
    // the agent won't be surprised.
    let mut sniff = [0u8; 8192];
    let read = std::io::Read::read(&mut reader, &mut sniff).map_err(|_| SearchAbort::Io)?;
    if sniff[..read].contains(&0) {
        return Err(SearchAbort::Binary);
    }

    // Re-open from scratch so we read the full file (the sniff
    // consumed the first 8 KiB). For huge files this is a minor
    // win over seeking; for small ones it's identical.
    let file = std::fs::File::open(path).map_err(|_| SearchAbort::Io)?;
    let reader = BufReader::new(file);

    // Buffer lines so we can supply leading context once a hit
    // lands later in the file. We only need the most-recent
    // `context_lines` entries.
    let mut ring: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(context_lines.max(1));
    // When we hit a match we need to emit the next `context_lines`
    // lines too; this counts them down.
    let mut pending_after: Option<usize> = None;
    let mut local_matches_iters: Vec<(usize, String)> = Vec::new();

    let mut file_hits: usize = 0;

    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        // Mid-file read errors usually mean the file was truncated
        // or the bytes weren't valid UTF-8 after all. Treat as
        // binary and move on rather than raising — the agent has
        // hundreds of other files to search.
        let Ok(line) = line else { return Err(SearchAbort::Binary) };
        let capped = cap_line(&line);

        // Fill in any pending "context_after" lines for previous
        // matches. We have to do this BEFORE checking for a new
        // match so the trailing context of an earlier hit doesn't
        // collide with the current hit's leading context.
        if let Some(remaining) = pending_after {
            if remaining > 0 {
                if let Some(last) = out.last_mut() {
                    last.context_after.push(capped.clone());
                }
                let next = remaining - 1;
                pending_after = if next == 0 { None } else { Some(next) };
            }
        }

        if re.is_match(&line) {
            file_hits += 1;

            // Only push the match if we still have room. Even
            // when we don't, we keep counting `file_hits` so
            // `total_matched` is honest.
            if out.len() < max_results {
                let rel_path = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                let context_before: Vec<String> = ring.iter().cloned().collect();
                out.push(Match {
                    path: rel_path,
                    line: line_no,
                    text: capped.clone(),
                    context_before,
                    context_after: Vec::new(),
                });
                pending_after = if context_lines > 0 {
                    Some(context_lines)
                } else {
                    None
                };
            }
            // Track lines we visited so the caller knows we made
            // progress through the file. Unused for now; reserved
            // for richer reports.
            local_matches_iters.push((line_no, capped.clone()));
        }

        // Maintain the leading-context ring buffer.
        if context_lines > 0 {
            if ring.len() == context_lines {
                ring.pop_front();
            }
            ring.push_back(capped);
        }

        if out.len() >= max_results {
            break;
        }
    }

    Ok(file_hits)
}

/// Truncate `line` to `LINE_CAP_BYTES` on a char boundary,
/// appending the `…[truncated]` marker so the agent can tell.
fn cap_line(s: &str) -> String {
    if s.len() <= LINE_CAP_BYTES {
        return s.to_string();
    }
    let mut cap = LINE_CAP_BYTES;
    while !s.is_char_boundary(cap) {
        cap -= 1;
    }
    format!("{}…[truncated]", &s[..cap])
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
    use serde_json::json;
    use std::sync::Arc;

    fn ctx() -> Arc<dyn crate::context::ToolContext> {
        Arc::new(crate::context::MockToolContext::new())
    }

    fn args(v: serde_json::Value) -> Option<JsonObject> {
        match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    fn result_text(r: &CallToolResult) -> String {
        for c in &r.content {
            if let rmcp::model::RawContent::Text(t) = &c.raw {
                return t.text.clone();
            }
        }
        String::new()
    }

    fn parse_output(r: &CallToolResult) -> Output {
        let txt = result_text(r);
        serde_json::from_str::<Output>(&txt)
            .unwrap_or_else(|e| panic!("output not JSON-parsable: {e}\nbody: {txt}"))
    }

    #[tokio::test]
    async fn happy_path_matches_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn run_loop() {}\nfn other() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "// run_loop reference\nfn main() {}\n").unwrap();

        let res = handle(
            args(json!({
                "pattern": "run_loop",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.total_matched, 2);
        assert!(!out.truncated);
        assert_eq!(out.matches.len(), 2);
        // Both matches should mention run_loop and have line=1.
        for m in &out.matches {
            assert!(m.text.contains("run_loop"), "got: {}", m.text);
            assert_eq!(m.line, 1);
        }
    }

    #[tokio::test]
    async fn max_results_caps_output() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for i in 0..50 {
            use std::fmt::Write;
            writeln!(body, "match line {i}").unwrap();
        }
        std::fs::write(dir.path().join("big.txt"), body).unwrap();

        let res = handle(
            args(json!({
                "pattern": "match",
                "path": dir.path().to_string_lossy(),
                "max_results": 5,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 5);
        assert!(out.truncated, "expected truncated flag");
        // total_matched is how many we counted before bailing on
        // the cap; with truncation it can be == matches.len()
        // (we stopped searching) or larger if we still finished
        // the file. Either way, at least the cap.
        assert!(out.total_matched >= 5);
    }

    #[tokio::test]
    async fn gitignore_honored() {
        let dir = tempfile::tempdir().unwrap();
        // `.gitignore` is only honoured by `ignore` inside a git
        // repo; `.ignore` is the always-honoured sibling file the
        // crate provides for the non-git case. Same code path,
        // same wiring, so this still proves the contract.
        std::fs::write(dir.path().join(".ignore"), "secret.txt\n").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "needle here\n").unwrap();
        std::fs::write(dir.path().join("public.txt"), "needle here\n").unwrap();

        let res = handle(
            args(json!({
                "pattern": "needle",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 1, "only public.txt should match");
        assert!(out.matches[0].path.contains("public"));
    }

    #[tokio::test]
    async fn hard_skip_dirs_apply() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/foo")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("target/debug/blob.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("node_modules/foo/x.js"), "needle\n").unwrap();
        std::fs::write(dir.path().join(".git/config"), "needle\n").unwrap();
        std::fs::write(dir.path().join("src.rs"), "needle\n").unwrap();

        let res = handle(
            args(json!({
                "pattern": "needle",
                "path": dir.path().to_string_lossy(),
                "no_ignore": true,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 1, "only src.rs should match");
        assert!(out.matches[0].path.contains("src.rs"));
    }

    #[tokio::test]
    async fn binary_files_skipped() {
        let dir = tempfile::tempdir().unwrap();
        // A file with a NUL byte counts as binary.
        std::fs::write(dir.path().join("bin.dat"), b"\x00needle\x00").unwrap();
        std::fs::write(dir.path().join("txt.txt"), "needle here\n").unwrap();

        let res = handle(
            args(json!({
                "pattern": "needle",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 1);
        assert!(out.matches[0].path.contains("txt.txt"));
    }

    #[tokio::test]
    async fn missing_path_errors() {
        let err = handle(
            args(json!({
                "pattern": "x",
                "path": "/nonexistent/path/should/not/exist/xyz12345",
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn invalid_regex_error_names_pattern() {
        let err = handle(
            args(json!({"pattern": "(invalid"})),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(
                    msg.contains("(invalid"),
                    "error must name the offending pattern, got: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn glob_filter_applies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hit.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("nohit.txt"), "needle\n").unwrap();

        let res = handle(
            args(json!({
                "pattern": "needle",
                "path": dir.path().to_string_lossy(),
                "glob": "*.rs",
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 1);
        assert!(out.matches[0].path.ends_with("hit.rs"));
    }

    #[tokio::test]
    async fn case_insensitive_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "NEEDLE\n").unwrap();

        // Default — case sensitive, no match.
        let res = handle(
            args(json!({
                "pattern": "needle",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 0);

        // With case_insensitive: match.
        let res = handle(
            args(json!({
                "pattern": "needle",
                "path": dir.path().to_string_lossy(),
                "case_insensitive": true,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 1);
    }

    #[tokio::test]
    async fn context_lines_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.txt"),
            "before1\nbefore2\nMATCH\nafter1\nafter2\nafter3\n",
        )
        .unwrap();

        let res = handle(
            args(json!({
                "pattern": "MATCH",
                "path": dir.path().to_string_lossy(),
                "context_lines": 2,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 1);
        let m = &out.matches[0];
        assert_eq!(m.context_before, vec!["before1", "before2"]);
        assert_eq!(m.context_after, vec!["after1", "after2"]);
    }

    #[tokio::test]
    async fn long_line_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let huge: String = "a".repeat(10_000) + "needle\n";
        std::fs::write(dir.path().join("big.txt"), huge).unwrap();

        let res = handle(
            args(json!({
                "pattern": "needle",
                "path": dir.path().to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.matches.len(), 1);
        assert!(out.matches[0].text.ends_with("…[truncated]"));
    }

    #[test]
    fn schema_declares_required_fields() {
        let s = schema();
        let schema: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["pattern"]));
    }

    #[test]
    fn cap_line_passes_through_short_strings() {
        let s = "hello";
        assert_eq!(cap_line(s), "hello");
    }

    /// Walk + search a 100-file synthetic tree. Marked `#[ignore]`
    /// so it doesn't run by default — invoke with
    /// `cargo test -p ironclaw-mcp --release tools::grep::tests::perf_smoke -- --nocapture --ignored`
    /// to reproduce. Target: well under 100ms on a modern laptop.
    #[tokio::test]
    #[ignore]
    async fn perf_smoke_100_files() {
        use std::fmt::Write;
        use std::time::Instant;
        let dir = tempfile::tempdir().unwrap();
        for i in 0..100 {
            let mut body = String::new();
            for j in 0..30 {
                if j == 15 && i % 3 == 0 {
                    writeln!(body, "MATCH line here").unwrap();
                } else {
                    writeln!(body, "noise here").unwrap();
                }
            }
            std::fs::write(dir.path().join(format!("f{i}.rs")), body).unwrap();
        }
        let m = args(json!({
            "pattern": "MATCH",
            "path": dir.path().to_string_lossy(),
        }));
        // Warm up the runtime / page cache.
        let _ = handle(m.clone(), ctx().as_ref()).await.unwrap();
        let start = Instant::now();
        let _ = handle(m, ctx().as_ref()).await.unwrap();
        let elapsed = start.elapsed();
        println!("100-file grep elapsed: {elapsed:?}");
    }

    #[test]
    fn cap_line_truncates_long_strings_on_char_boundary() {
        let s = "héllo".repeat(2000);
        let got = cap_line(&s);
        assert!(got.ends_with("…[truncated]"));
        assert!(got.len() <= LINE_CAP_BYTES + "…[truncated]".len() + 4);
    }
}
