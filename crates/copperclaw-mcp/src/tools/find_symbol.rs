//! `find_symbol`: read-only symbol navigation — go-to-definition,
//! find-references, and a hover signature — over a repository.
//!
//! The agent previously located a definition or its callers by issuing a
//! full-repo `grep` for the identifier and eyeballing which hit was the
//! declaration. That is slow (walks the whole tree for a name it could
//! index), lossy (no def-vs-reference distinction, no signature), and
//! burns tokens. `find_symbol` is the structured alternative: given a
//! symbol name (and optional repo root) it returns `{definitions,
//! references, hover}` with `file:line` anchors.
//!
//! ## Backend strategy (decision (f): container-local, read-only)
//!
//! This tool is the *read* half of the C3 symbol surface. The *write*
//! half — building the on-disk index — is the container-local bridge in
//! `copperclaw_runner::run::lsp`, invoked from the C2 attach flow's
//! `trigger_symbol_index` seam. The bridge prefers a language server
//! (rust-analyzer / typescript-language-server) when one is baked into the
//! image and falls back to `universal-ctags` (already baked — see
//! `copperclaw-setup`'s image bake list) otherwise; the queryable artifact
//! it emits is a ctags `tags` file at [`TAGS_REL_PATH`].
//!
//! `find_symbol` degrades cleanly across three tiers, in order:
//!
//! 1. **ctags index** — parse `<root>/.copperclaw/tags` if the bridge
//!    already built one. Authoritative `file:line` + kind + signature.
//! 2. **on-demand ctags** — if there is no index but `ctags` is on `PATH`,
//!    run it over the root to stdout (never writing into the repo) and
//!    parse that. Covers a repo the attach flow never indexed.
//! 3. **scoped grep fallback** — when no ctags backend is available at all
//!    (a minimal image), match definition-shaped source lines directly.
//!    Always returns *something* usable rather than erroring.
//!
//! References are always resolved by a whole-word scan of the tree
//! (ctags does not emit call sites reliably), mirroring `grep`'s walker so
//! `.gitignore` is honoured and `target/`, `node_modules/`, `.git/` are
//! skipped unconditionally.
//!
//! A full live LSP JSON-RPC client (spawning rust-analyzer per query for
//! semantic go-to-def) is intentionally *not* implemented here — decision
//! (f)'s pragmatism note prefers a correct, tested ctags-backed tool with a
//! clean degradation path over a fragile in-tool language-server client.
//! The bridge records which server would fit so a future card can add that
//! path without changing this tool's contract.

use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Where the C3 bridge writes the ctags index, relative to the repo root.
/// Shared with `copperclaw_runner::run::lsp` (the builder) so the read and
/// write halves agree on one location.
pub const TAGS_REL_PATH: &str = ".copperclaw/tags";

/// Default cap on returned definitions.
const DEFAULT_MAX_DEFS: usize = 50;
/// Hard ceiling on definitions.
const MAX_DEFS_CEILING: usize = 200;
/// Default cap on returned references.
const DEFAULT_MAX_REFS: usize = 100;
/// Hard ceiling on references — long call-site lists are pure context cost.
const MAX_REFS_CEILING: usize = 1000;
/// Per-line byte cap on reference text (mirrors `grep::LINE_CAP_BYTES`,
/// smaller because a reference row is a single call site, not a match dump).
const LINE_CAP_BYTES: usize = 2048;
/// Directories skipped unconditionally (build / vendor / VCS noise).
/// Mirrors `grep::HARD_SKIP_DIRS` / `glob::HARD_SKIP_DIRS`.
const HARD_SKIP_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// JSON-RPC input for the `find_symbol` tool.
#[derive(Debug, Deserialize)]
struct Input {
    /// The symbol name to resolve (exact, case-sensitive).
    symbol: String,
    /// Repo root to search under. Empty / unset → current working dir.
    #[serde(default)]
    path: Option<String>,
    /// Cap on returned definitions.
    #[serde(default)]
    max_results: Option<usize>,
    /// Cap on returned references.
    #[serde(default)]
    max_references: Option<usize>,
    /// When `false`, skip the reference scan (definitions + hover only).
    /// Defaults to `true`.
    #[serde(default = "default_true")]
    include_references: bool,
}

const fn default_true() -> bool {
    true
}

/// A resolved definition site.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(Deserialize))]
struct Definition {
    /// Path relative to the search root (as ctags records it, or as the
    /// walker computes it for the grep fallback).
    path: String,
    /// 1-based line number, when resolvable. `null` only when a ctags entry
    /// carried neither a `line:` field nor a numeric ex-command.
    line: Option<usize>,
    /// ctags "kind" (function / struct / class / …) when known.
    kind: Option<String>,
    /// The definition's source line — the hover/signature text.
    signature: Option<String>,
}

/// A reference (call site / mention) of the symbol.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(Deserialize))]
struct Reference {
    path: String,
    line: usize,
    text: String,
}

/// Top-level output envelope.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(Deserialize))]
struct Output {
    /// The symbol that was resolved (echoed for clarity).
    symbol: String,
    /// Definition sites, most-authoritative first.
    definitions: Vec<Definition>,
    /// The hover text: the first definition's signature, when any.
    hover: Option<String>,
    /// Reference / call sites found by the whole-word scan.
    references: Vec<Reference>,
    /// Which backend produced the definitions: `"ctags-index"`,
    /// `"ctags-ondemand"`, `"grep"`, or `"none"` (nothing found).
    definition_source: String,
    /// `true` when the definition list hit its cap.
    definitions_truncated: bool,
    /// `true` when the reference list hit its cap.
    references_truncated: bool,
}

// ── ctags `tags`-file parsing (shared with the runner-side builder) ──────

/// One parsed ctags tag entry. Public so the C3 index builder
/// (`copperclaw_runner::run::lsp`) can count / verify the index it just
/// wrote without re-implementing the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    /// The tag (symbol) name.
    pub name: String,
    /// The file the tag was found in, exactly as ctags recorded it
    /// (relative to the directory ctags was run in).
    pub file: String,
    /// 1-based line, from a `line:` extension field or a numeric ex-command.
    pub line: Option<usize>,
    /// The ctags "kind" (e.g. `function`, `f`, `struct`), when present.
    pub kind: Option<String>,
    /// The recovered source line (from a `/pattern/` ex-command), when the
    /// ex-command was a search pattern rather than a bare line number.
    pub pattern: Option<String>,
}

/// Parse the body of a ctags `tags` file (u-ctags / exuberant format).
///
/// Format per data line: `name<TAB>file<TAB>excmd;"<TAB>ext-fields…`.
/// The ex-command is either a `/pattern/` search, a `?pattern?` search, or
/// a bare line number; extension fields are tab-separated `key:value`
/// pairs plus an optional bare single-letter kind. Header lines (`!_TAG_…`)
/// are skipped. Malformed lines are skipped rather than erroring — a tags
/// file is machine-generated but we never want one bad row to blank a
/// lookup.
#[must_use]
pub fn parse_tags(content: &str) -> Vec<TagEntry> {
    content.lines().filter_map(parse_tag_line).collect()
}

fn parse_tag_line(line: &str) -> Option<TagEntry> {
    if line.starts_with("!_TAG_") || line.trim().is_empty() {
        return None;
    }
    // `splitn(3)` keeps any tabs inside the ex-command pattern in `rest`.
    let mut it = line.splitn(3, '\t');
    let name = it.next()?.trim();
    let file = it.next()?;
    let rest = it.next().unwrap_or("");
    if name.is_empty() || file.is_empty() {
        return None;
    }

    // The ex-command runs up to the `;"` terminator (extended format); the
    // remainder is tab-separated extension fields. Old-format tags omit the
    // terminator entirely — then the whole `rest` is the ex-command.
    let (excmd, ext) = match rest.find(";\"") {
        Some(idx) => (&rest[..idx], rest[idx + 2..].trim_start_matches('\t')),
        None => (rest, ""),
    };

    let mut line_no = numeric_excmd(excmd);
    let pattern = pattern_excmd(excmd);
    let mut kind = None;
    for field in ext.split('\t') {
        if field.is_empty() {
            continue;
        }
        if let Some((k, v)) = field.split_once(':') {
            match k {
                "line" => {
                    if let Ok(n) = v.parse::<usize>() {
                        line_no = Some(n);
                    }
                }
                "kind" => kind = Some(v.to_string()),
                _ => {}
            }
        } else if kind.is_none() && field.len() <= 2 {
            // A bare single/double-char field is the short kind letter
            // (u-ctags emits `f` when `--fields` doesn't force `kind:`).
            kind = Some(field.to_string());
        }
    }

    Some(TagEntry {
        name: name.to_string(),
        file: file.to_string(),
        line: line_no,
        kind,
        pattern,
    })
}

/// A ctags ex-command that is a bare line number.
fn numeric_excmd(excmd: &str) -> Option<usize> {
    excmd.trim().parse::<usize>().ok()
}

/// Recover the source line from a `/^…$/` or `?^…$?` ex-command pattern,
/// unescaping ctags' `\/`, `\?`, and `\\`, and dropping the `^`/`$`
/// anchors ctags adds. Returns `None` for a numeric ex-command.
fn pattern_excmd(excmd: &str) -> Option<String> {
    let excmd = excmd.trim();
    let bytes = excmd.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let delim = bytes[0];
    if delim != b'/' && delim != b'?' {
        return None;
    }
    let inner = &excmd[1..excmd.len().saturating_sub(1)];
    let inner = inner.strip_prefix('^').unwrap_or(inner);
    let inner = inner.strip_suffix('$').unwrap_or(inner);
    // Unescape the delimiter and backslash.
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(next @ ('/' | '?' | '\\')) => out.push(next),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ── tool schema / handler ────────────────────────────────────────────────

/// Build the rmcp `Tool` descriptor.
pub fn schema() -> Tool {
    make_tool(
        "find_symbol",
        "Resolve a symbol to its definition(s), references, and a hover signature under `path` (default cwd). Read-only. Uses a ctags index when present, on-demand ctags otherwise, and a scoped definition-line scan as a last resort. Prefer this over grepping for a definition.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["symbol"],
            "properties": {
                "symbol":             { "type": "string", "minLength": 1 },
                "path":               { "type": ["string", "null"] },
                "max_results":        { "type": ["integer", "null"], "minimum": 1, "maximum": 200 },
                "max_references":     { "type": ["integer", "null"], "minimum": 1, "maximum": 1000 },
                "include_references": { "type": "boolean" }
            }
        }),
    )
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;

    let symbol = input.symbol.trim().to_string();
    if symbol.is_empty() {
        return Err(ToolError::Validation("`symbol` must be non-empty".into()));
    }

    let root = match input
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
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

    let definition_cap = input
        .max_results
        .unwrap_or(DEFAULT_MAX_DEFS)
        .clamp(1, MAX_DEFS_CEILING);
    let reference_cap = input
        .max_references
        .unwrap_or(DEFAULT_MAX_REFS)
        .clamp(1, MAX_REFS_CEILING);
    let include_refs = input.include_references;

    // The file walk + ctags subprocess are blocking; offload so the tokio
    // runtime isn't pinned (same posture as `grep` / `glob`).
    let output = tokio::task::spawn_blocking(move || {
        run_find_symbol(&root, &symbol, definition_cap, reference_cap, include_refs)
    })
    .await
    .map_err(|e| ToolError::Internal(format!("find_symbol task panicked: {e}")))?;

    Ok(success_json(&output))
}

/// Resolve `symbol` under `root`: definitions across the three-tier backend
/// strategy, then a whole-word reference scan.
fn run_find_symbol(
    root: &Path,
    symbol: &str,
    definition_cap: usize,
    reference_cap: usize,
    include_refs: bool,
) -> Output {
    let (mut definitions, definition_source) = resolve_definitions(root, symbol, definition_cap);
    let definitions_truncated = definitions.len() > definition_cap;
    definitions.truncate(definition_cap);

    let hover = definitions
        .iter()
        .find_map(|d| d.signature.clone())
        .or_else(|| definitions.first().and_then(|d| d.kind.clone()));

    let (references, references_truncated) = if include_refs {
        scan_references(root, symbol, reference_cap)
    } else {
        (Vec::new(), false)
    };

    Output {
        symbol: symbol.to_string(),
        definitions,
        hover,
        references,
        definition_source: definition_source.to_string(),
        definitions_truncated,
        references_truncated,
    }
}

/// Definitions across the three tiers. Returns the (possibly over-cap) list
/// plus a label for which backend produced it.
fn resolve_definitions(
    root: &Path,
    symbol: &str,
    definition_cap: usize,
) -> (Vec<Definition>, &'static str) {
    // Tier 1: a bridge-built ctags index.
    if let Some(content) = read_index(root) {
        let defs = defs_from_tags(&content, symbol, root, definition_cap);
        if !defs.is_empty() {
            return (defs, "ctags-index");
        }
    }
    // Tier 2: on-demand ctags to stdout (never writes into the repo).
    if let Some(content) = run_ctags_stdout(root) {
        let defs = defs_from_tags(&content, symbol, root, definition_cap);
        if !defs.is_empty() {
            return (defs, "ctags-ondemand");
        }
    }
    // Tier 3: scoped scan for definition-shaped source lines.
    let defs = grep_definitions(root, symbol, definition_cap);
    if defs.is_empty() {
        (defs, "none")
    } else {
        (defs, "grep")
    }
}

/// Read `<root>/.copperclaw/tags` if the bridge already built it.
fn read_index(root: &Path) -> Option<String> {
    let path = root.join(TAGS_REL_PATH);
    if !path.is_file() {
        return None;
    }
    std::fs::read_to_string(&path).ok()
}

/// Turn matching ctags entries into definitions, resolving a line from the
/// `line:` field, a numeric ex-command, or (last) by searching the file for
/// the recorded pattern. Collects up to `definition_cap + 1` so the caller can
/// detect truncation.
fn defs_from_tags(
    content: &str,
    symbol: &str,
    root: &Path,
    definition_cap: usize,
) -> Vec<Definition> {
    let mut out = Vec::new();
    for tag in parse_tags(content) {
        if tag.name != symbol {
            continue;
        }
        let line = tag.line.or_else(|| {
            tag.pattern
                .as_deref()
                .and_then(|p| line_of_pattern(root, &tag.file, p))
        });
        out.push(Definition {
            path: tag.file,
            line,
            kind: normalise_kind(tag.kind.as_deref()),
            signature: tag.pattern,
        });
        if out.len() > definition_cap {
            break;
        }
    }
    out
}

/// Find the 1-based line of `pattern` (a recovered source line) within
/// `<root>/<file>`, matching the whole trimmed line. Bounded, best-effort.
fn line_of_pattern(root: &Path, file: &str, pattern: &str) -> Option<usize> {
    let path = root.join(file);
    let f = std::fs::File::open(&path).ok()?;
    let reader = BufReader::new(f);
    let needle = pattern.trim();
    for (idx, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        if line.trim() == needle {
            return Some(idx + 1);
        }
    }
    None
}

/// Map ctags' short single-letter kinds to readable words where the mapping
/// is unambiguous across languages; otherwise pass the kind through.
fn normalise_kind(kind: Option<&str>) -> Option<String> {
    let k = kind?;
    let mapped = match k {
        "f" => "function",
        "c" => "class",
        "s" => "struct",
        "g" => "enum",
        "m" => "member",
        "v" => "variable",
        "t" => "type",
        "i" => "interface",
        "n" => "namespace",
        "d" => "macro",
        other => other,
    };
    Some(mapped.to_string())
}

// ── on-demand ctags ──────────────────────────────────────────────────────

/// Locate a `ctags` binary on `PATH`, if any.
fn ctags_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ctags");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Run `ctags` over `root`, emitting the index to stdout (`-f -`) so we
/// never write into the (possibly external) repo. `--fields=+n` forces line
/// numbers; `--recurse` walks the tree. Returns `None` if ctags is absent
/// or the run fails.
fn run_ctags_stdout(root: &Path) -> Option<String> {
    let bin = ctags_on_path()?;
    let output = std::process::Command::new(bin)
        .arg("--recurse")
        .arg("--fields=+n")
        .arg("-f")
        .arg("-")
        .arg(".")
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

// ── grep fallback (definitions) + reference scan ─────────────────────────

/// Build a regex that matches a *definition* of `symbol` across common
/// languages: a declaration keyword immediately before the name, or an
/// assignment of a function/arrow to it.
fn definition_regex(symbol: &str) -> regex::Regex {
    let s = regex::escape(symbol);
    // Keyword-before: `fn foo` / `function foo` / `def foo` / `class foo` /
    //   `struct foo` / `const foo` / `type foo` / `export function foo` …
    // Assignment: `foo = function` / `foo := func` / `foo: (…) =>` / `foo = (…) =>`.
    let pat = format!(
        r"(?m)\b(?:fn|function|func|def|class|struct|enum|trait|type|interface|impl|const|let|var|module|namespace|public|private|protected|static)\s+{s}\b|\b{s}\s*[:=]\s*(?:async\s+)?(?:function\b|func\b|\([^\n)]*\)\s*(?:=>|->|\{{)|[A-Za-z_][\w:<>]*\s*=>)"
    );
    // `s` is regex-escaped, so this never fails to compile; unwrap is safe.
    regex::Regex::new(&pat).expect("definition regex compiles")
}

/// Tier 3: scan the tree for lines that *declare* `symbol`.
fn grep_definitions(root: &Path, symbol: &str, definition_cap: usize) -> Vec<Definition> {
    let re = definition_regex(symbol);
    let mut out = Vec::new();
    let walker = build_walker(root);
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        let Some(lines) = read_text_lines(path) else {
            continue;
        };
        for (idx, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                out.push(Definition {
                    path: rel.display().to_string(),
                    line: Some(idx + 1),
                    kind: None,
                    signature: Some(cap_line(line.trim())),
                });
                if out.len() > definition_cap {
                    return out;
                }
            }
        }
    }
    out
}

/// Scan the tree for whole-word occurrences of `symbol` — the reference /
/// call-site list. Returns `(references, truncated)`.
fn scan_references(root: &Path, symbol: &str, reference_cap: usize) -> (Vec<Reference>, bool) {
    let Ok(re) = regex::Regex::new(&format!(r"\b{}\b", regex::escape(symbol))) else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let mut truncated = false;
    let walker = build_walker(root);
    'outer: for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        let Some(lines) = read_text_lines(path) else {
            continue;
        };
        for (idx, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                if out.len() >= reference_cap {
                    truncated = true;
                    break 'outer;
                }
                out.push(Reference {
                    path: rel.display().to_string(),
                    line: idx + 1,
                    text: cap_line(line.trim()),
                });
            }
        }
    }
    (out, truncated)
}

/// The shared `ignore`-crate walker: honours `.gitignore`, skips the
/// hard-skip dirs unconditionally, never follows symlinks. Mirrors
/// `grep`/`glob`.
fn build_walker(root: &Path) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.follow_links(false).filter_entry(|entry| {
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            if let Some(name) = entry.file_name().to_str() {
                if HARD_SKIP_DIRS.contains(&name) {
                    return false;
                }
            }
        }
        true
    });
    builder.build()
}

/// Read a file as UTF-8 lines, skipping anything that looks binary (a NUL
/// byte in the first chunk) or unreadable. Mirrors `grep`'s binary sniff.
fn read_text_lines(path: &Path) -> Option<Vec<String>> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    Some(text.lines().map(str::to_string).collect())
}

/// Truncate `s` to [`LINE_CAP_BYTES`] on a char boundary, appending a
/// marker so the agent can tell.
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

    /// Absolute path to the checked-in Node fixture repo (C2's).
    fn fixture_repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/repos/todo-tracker")
            .canonicalize()
            .expect("fixture repo exists")
    }

    // ── ctags `tags`-file parsing (unit) ─────────────────────────────

    #[test]
    fn parses_extended_format_with_line_field() {
        let tags = "\
!_TAG_FILE_FORMAT\t2\t/extended format/\n\
addTodo\tsrc/store.js\t/^export function addTodo(text) {$/;\"\tf\tline:22\n\
markDone\tsrc/store.js\t/^export function markDone(id) {$/;\"\tf\tline:44\n";
        let entries = parse_tags(tags);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "addTodo");
        assert_eq!(entries[0].file, "src/store.js");
        assert_eq!(entries[0].line, Some(22));
        assert_eq!(entries[0].kind.as_deref(), Some("f"));
        assert_eq!(
            entries[0].pattern.as_deref(),
            Some("export function addTodo(text) {")
        );
        assert_eq!(entries[1].line, Some(44));
    }

    #[test]
    fn parses_numeric_excmd_without_line_field() {
        let tags = "foo\tsrc/a.rs\t42;\"\tf\n";
        let entries = parse_tags(tags);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].line, Some(42));
        assert_eq!(entries[0].pattern, None);
    }

    #[test]
    fn skips_header_and_blank_lines() {
        let tags = "!_TAG_PROGRAM_NAME\tUniversal Ctags\t//\n\n";
        assert!(parse_tags(tags).is_empty());
    }

    #[test]
    fn kind_field_form_is_parsed() {
        let tags = "Widget\tsrc/w.rs\t/^struct Widget {$/;\"\tkind:struct\tline:7\n";
        let entries = parse_tags(tags);
        assert_eq!(entries[0].kind.as_deref(), Some("struct"));
        assert_eq!(entries[0].line, Some(7));
    }

    // ── go-to-def via a bridge-built index (unit) ────────────────────

    /// Acceptance (unit): a symbol lookup returns file:line for a fixture
    /// repo when the C3 index is present.
    #[tokio::test]
    async fn index_lookup_returns_file_and_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".copperclaw")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/store.js"),
            "export function addTodo(text) {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join(TAGS_REL_PATH),
            "addTodo\tsrc/store.js\t/^export function addTodo(text) {$/;\"\tf\tline:1\n",
        )
        .unwrap();

        let res = handle(
            args(json!({
                "symbol": "addTodo",
                "path": dir.path().to_string_lossy(),
                "include_references": false,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.definition_source, "ctags-index");
        assert_eq!(out.definitions.len(), 1);
        assert_eq!(out.definitions[0].path, "src/store.js");
        assert_eq!(out.definitions[0].line, Some(1));
        assert_eq!(out.definitions[0].kind.as_deref(), Some("function"));
        assert_eq!(
            out.hover.as_deref(),
            Some("export function addTodo(text) {")
        );
    }

    // ── grep fallback: definitions + references on the real fixture ──

    /// Acceptance (unit + integration): with NO ctags index and (in CI) no
    /// ctags binary, the tool still resolves `addTodo`'s definition in
    /// store.js and its reference in index.js — i.e. the agent resolves a
    /// definition without issuing a full-repo grep of its own.
    #[tokio::test]
    async fn resolves_definition_and_references_without_an_index() {
        let repo = fixture_repo();
        let res = handle(
            args(json!({
                "symbol": "addTodo",
                "path": repo.to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);

        // A definition in store.js at the `export function addTodo` line.
        assert!(
            !out.definitions.is_empty(),
            "expected a definition, source={}",
            out.definition_source
        );
        let def = out
            .definitions
            .iter()
            .find(|d| d.path.ends_with("store.js"))
            .expect("definition is in store.js");
        assert!(def.line.is_some());
        assert!(
            def.signature
                .as_deref()
                .is_some_and(|s| s.contains("addTodo")),
            "hover signature carries the def line: {:?}",
            def.signature
        );

        // A reference from index.js (the import + the call site).
        assert!(
            out.references.iter().any(|r| r.path.ends_with("index.js")),
            "expected a reference in index.js, got: {:?}",
            out.references
        );
        // …and the def site itself is among the references (store.js).
        assert!(out.references.iter().any(|r| r.path.ends_with("store.js")));
    }

    #[tokio::test]
    async fn include_references_false_skips_the_scan() {
        let repo = fixture_repo();
        let res = handle(
            args(json!({
                "symbol": "addTodo",
                "path": repo.to_string_lossy(),
                "include_references": false,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert!(out.references.is_empty());
        assert!(!out.definitions.is_empty());
    }

    #[tokio::test]
    async fn unknown_symbol_yields_empty_not_error() {
        let repo = fixture_repo();
        let res = handle(
            args(json!({
                "symbol": "no_such_symbol_anywhere_xyz",
                "path": repo.to_string_lossy(),
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.definition_source, "none");
        assert!(out.definitions.is_empty());
        assert!(out.references.is_empty());
        assert!(out.hover.is_none());
    }

    #[tokio::test]
    async fn rust_definition_shapes_are_matched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub struct Widget {}\nfn helper() {}\nlet mk = |x| x;\npub fn build_widget() -> Widget { Widget {} }\n",
        )
        .unwrap();
        // struct definition.
        let res = handle(
            args(json!({"symbol": "Widget", "path": dir.path().to_string_lossy(), "include_references": false})),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert!(
            out.definitions.iter().any(|d| d.line == Some(1)),
            "struct Widget at line 1: {:?}",
            out.definitions
        );
        // fn definition.
        let res = handle(
            args(json!({"symbol": "helper", "path": dir.path().to_string_lossy(), "include_references": false})),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert!(out.definitions.iter().any(|d| d.line == Some(2)));
    }

    #[tokio::test]
    async fn missing_path_errors() {
        let err = handle(
            args(json!({
                "symbol": "x",
                "path": "/nonexistent/path/xyz12345",
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn empty_symbol_errors() {
        let err = handle(args(json!({"symbol": "   "})), ctx().as_ref())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn max_references_caps_and_flags_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for _ in 0..40 {
            body.push_str("touch(sym);\n");
        }
        std::fs::write(dir.path().join("a.js"), body).unwrap();
        let res = handle(
            args(json!({
                "symbol": "sym",
                "path": dir.path().to_string_lossy(),
                "max_references": 5,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.references.len(), 5);
        assert!(out.references_truncated);
    }

    #[tokio::test]
    async fn hard_skip_dirs_are_not_scanned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(
            dir.path().join("node_modules/pkg/index.js"),
            "function target() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("app.js"), "function target() {}\n").unwrap();
        let res = handle(
            args(json!({"symbol": "target", "path": dir.path().to_string_lossy()})),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert!(
            out.definitions
                .iter()
                .all(|d| !d.path.contains("node_modules")),
            "node_modules must be skipped: {:?}",
            out.definitions
        );
        assert!(
            out.references
                .iter()
                .all(|r| !r.path.contains("node_modules"))
        );
    }

    #[test]
    fn pattern_excmd_unescapes_and_strips_anchors() {
        assert_eq!(pattern_excmd(r"/^foo\/bar$/").as_deref(), Some("foo/bar"));
        assert_eq!(pattern_excmd("123"), None);
        assert_eq!(pattern_excmd(r"?^baz$?").as_deref(), Some("baz"));
    }

    #[test]
    fn schema_declares_required_fields() {
        let s = schema();
        let schema: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(schema["required"], serde_json::json!(["symbol"]));
    }

    /// When ctags IS on PATH (dev boxes), the on-demand tier resolves the
    /// fixture without a pre-built index. Skipped where ctags is absent (CI)
    /// so the suite stays hermetic — the grep tier already covers that case.
    #[tokio::test]
    async fn ondemand_ctags_used_when_available() {
        if ctags_on_path().is_none() {
            eprintln!("skipping: no ctags on PATH");
            return;
        }
        let repo = fixture_repo();
        let res = handle(
            args(json!({
                "symbol": "markDone",
                "path": repo.to_string_lossy(),
                "include_references": false,
            })),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let out = parse_output(&res);
        assert_eq!(out.definition_source, "ctags-ondemand");
        assert!(out.definitions.iter().any(|d| d.path.ends_with("store.js")));
    }
}
