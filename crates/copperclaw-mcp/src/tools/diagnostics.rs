//! `diagnostics`: structured lint/typecheck output (M20 Q3 — see
//! `docs/plans/m20-coding-and-design-capability-program.md` Wave 2 Q3).
//!
//! ## Problem this solves
//!
//! All code feedback today comes through `shell`, whose stdout/stderr are
//! head-truncated at 32 KiB PER STREAM
//! (`crate::tools::computer_use::SHELL_OUTPUT_CAP`). A long `tsc` error list
//! truncates precisely where the useful errors are (they cluster near the
//! end of a big build), and the model burns turns re-running the same
//! command with `tail_bytes` to see what it missed. `diagnostics` runs the
//! same tools in machine-readable mode and returns a STRUCTURED, CAPPED
//! digest instead: per-file error/warning counts, the first N full
//! diagnostics (message, `file:line`, rule), and totals — never a raw dump,
//! so truncation stops eating the signal.
//!
//! ## What this is NOT
//!
//! Read-only analysis with **no gate interaction**. `.copperclaw/verify`
//! (M20 Q2's multi-stage gate, `crate::tools::verify_gate`) remains the
//! enforcement path the todo-completion gate checks; `diagnostics` is a
//! fix-cycle *accelerator* the agent can call as often as it likes without
//! touching any dirty/stage/fix-cycle marker. It never shells out through
//! `apply_verify_gate` and never marks a project dirty.
//!
//! ## Tool detection
//!
//! Given a project directory, each of eslint/tsc/ruff is considered
//! "applicable" if either its config file is present at the project root,
//! or a bounded walk of the tree (skipping `node_modules/`, `.git/`,
//! `target/`, `dist/`, `build/`, and Python venv/cache dirs) finds a
//! matching source extension:
//!
//!   - **eslint**: an `.eslintrc*` / `eslint.config.*` file, or any
//!     `.js`/`.jsx`/`.mjs`/`.cjs` file.
//!   - **tsc**: `tsconfig.json`, or any `.ts`/`.tsx` file.
//!   - **ruff**: `ruff.toml` / `.ruff.toml` / a `pyproject.toml` containing
//!     a `[tool.ruff` table, or any `.py` file.
//!
//! A tool absent from the tree entirely is simply omitted from the digest —
//! this tool never reports "not applicable" noise for every tool that isn't
//! relevant, only for the tools it actually tried to run.
//!
//! ## Degradation
//!
//! An applicable tool whose binary isn't on `PATH` (probed at CALL TIME via
//! `command -v`, mirroring `ui_screenshot`'s chromium probe) degrades to a
//! per-tool `"not_available"` note — never a hard tool-call error. This is
//! the pre-Q1-image / minimal-profile case: the tool set may not be baked
//! yet, and the agent should get an actionable note, not a crash.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::ToolError;
use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};

/// Cap on how many *full* diagnostics (message + `file:line` + rule) this
/// tool returns per underlying linter/typechecker. The digest — not the
/// raw dump — is the whole point; see the module docs.
const DEFAULT_MAX_DIAGNOSTICS: usize = 40;
/// Hard ceiling on `max_findings` the caller may request.
const MAX_DIAGNOSTICS_CEILING: usize = 200;
/// Cap on the number of distinct per-file count rows in the digest.
const MAX_FILES_PER_TOOL: usize = 200;
/// Timeout for each linter/typechecker subprocess. These can be slow on a
/// cold `node_modules` or a large `tsc` project; generous but bounded.
const TOOL_TIMEOUT: Duration = Duration::from_secs(120);
/// Bounded safety cap on how many directory entries the applicability
/// walk will visit before giving up on the extension sniff. Prototype-
/// scale projects are nowhere near this; it exists purely so a
/// pathological tree (a stray `node_modules` the hard-skip list somehow
/// missed) can't make this tool hang.
const MAX_SNIFF_ENTRIES: usize = 20_000;
/// Cap on how many source files are passed directly to `tsc` when no
/// `tsconfig.json` is present (the file-list invocation form).
const MAX_TSC_FILES_WITHOUT_CONFIG: usize = 200;
/// Cap on bytes of subprocess stderr/stdout embedded in an "error" note
/// when a tool's output couldn't be parsed as its machine-readable format
/// (e.g. a config error). Small — this is a diagnostic-of-last-resort, not
/// the digest itself.
const ERROR_NOTE_CAP: usize = 2000;

const HARD_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
];

const ESLINT_CONFIG_NAMES: &[&str] = &[
    ".eslintrc",
    ".eslintrc.js",
    ".eslintrc.cjs",
    ".eslintrc.json",
    ".eslintrc.yml",
    ".eslintrc.yaml",
    "eslint.config.js",
    "eslint.config.mjs",
    "eslint.config.cjs",
    "eslint.config.ts",
];
const TSC_CONFIG_NAMES: &[&str] = &["tsconfig.json"];
const RUFF_CONFIG_NAMES: &[&str] = &["ruff.toml", ".ruff.toml"];

/// The three diagnostic tools this card wires up. Per M20 Q3: "detect
/// which of eslint/tsc/ruff apply" — no LSP, no ctags, demand-pull only
/// (see the plan's "Deferred / rejected" section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagTool {
    Eslint,
    Tsc,
    Ruff,
}

impl DiagTool {
    fn name(self) -> &'static str {
        match self {
            Self::Eslint => "eslint",
            Self::Tsc => "tsc",
            Self::Ruff => "ruff",
        }
    }

    fn binary(self) -> &'static str {
        // Same as `name()` today; kept as a separate method because the
        // binary a tool runs and its display name are conceptually
        // different things even though they coincide here.
        self.name()
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Input {
    /// The project directory to analyze, e.g. the same path passed as
    /// `cwd` to `shell`. Must already exist.
    project: String,
    /// Cap on the number of full diagnostics returned PER TOOL. Defaults
    /// to [`DEFAULT_MAX_DIAGNOSTICS`]; clamped to
    /// [`MAX_DIAGNOSTICS_CEILING`].
    #[serde(default)]
    max_findings: Option<usize>,
}

pub fn schema() -> Tool {
    make_tool(
        "diagnostics",
        "Run structured lint/typecheck analysis on a project: detects which of eslint/tsc/ruff \
         apply (config file or file-extension sniff) and runs each in machine-readable mode \
         (`eslint -f json`, `tsc --pretty false`, `ruff --output-format json`), returning a \
         STRUCTURED, CAPPED digest — per-file error/warning counts, the first N full diagnostics \
         (message, file:line, rule), and totals — instead of a raw dump. Use this INSTEAD of \
         piping a linter through `shell` for anything beyond a quick one-off check: `shell`'s \
         output is head-truncated at 32 KiB per stream, which silently eats exactly the errors \
         you need when a long tsc/eslint run overflows it. This tool is READ-ONLY analysis with \
         NO effect on the `.copperclaw/verify` gate — it's a fix-cycle accelerator, not a \
         substitute for writing and passing verify stages. A tool whose binary isn't installed \
         in this image (pre-`prototyping`-profile / minimal profile) reports a clean \
         'not available' note for that tool rather than failing the call; a project with no \
         applicable tool at all says so cleanly instead of erroring.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["project"],
            "properties": {
                "project": { "type": "string", "minLength": 1 },
                "max_findings": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": MAX_DIAGNOSTICS_CEILING
                }
            }
        }),
    )
}

// ── applicability detection ─────────────────────────────────────────────

/// Detect which of eslint/tsc/ruff apply to `root`: config-file presence
/// (checked first, cheap) OR a bounded extension sniff of the tree. Pure
/// filesystem logic — no subprocesses, so it's fully unit-testable against
/// tempdir fixtures.
fn detect_applicable_tools(root: &Path) -> Vec<DiagTool> {
    let has_eslint_config = ESLINT_CONFIG_NAMES.iter().any(|n| root.join(n).is_file());
    let has_tsc_config = TSC_CONFIG_NAMES.iter().any(|n| root.join(n).is_file());
    let has_ruff_config = RUFF_CONFIG_NAMES.iter().any(|n| root.join(n).is_file())
        || std::fs::read_to_string(root.join("pyproject.toml"))
            .is_ok_and(|text| text.contains("[tool.ruff"));

    let (found_javascript, found_typescript, found_python) =
        if has_eslint_config && has_tsc_config && has_ruff_config {
            // All three configs already present — skip the walk entirely.
            (false, false, false)
        } else {
            sniff_extensions(root)
        };

    let mut out = Vec::new();
    if has_eslint_config || found_javascript {
        out.push(DiagTool::Eslint);
    }
    if has_tsc_config || found_typescript {
        out.push(DiagTool::Tsc);
    }
    if has_ruff_config || found_python {
        out.push(DiagTool::Ruff);
    }
    out
}

/// Bounded walk of `root` (skipping [`HARD_SKIP_DIRS`], honouring
/// `.gitignore`) looking for JS-family, TS-family, and Python source
/// files. Stops early once all three have been seen, and gives up after
/// [`MAX_SNIFF_ENTRIES`] regardless.
fn sniff_extensions(root: &Path) -> (bool, bool, bool) {
    let mut found_javascript = false;
    let mut found_typescript = false;
    let mut found_python = false;

    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .follow_links(false)
        .filter_entry(|entry| {
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    if HARD_SKIP_DIRS.contains(&name) {
                        return false;
                    }
                }
            }
            true
        })
        .build();

    for (visited, entry) in walker.enumerate() {
        if visited > MAX_SNIFF_ENTRIES || (found_javascript && found_typescript && found_python) {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        match entry.path().extension().and_then(|e| e.to_str()) {
            Some("ts" | "tsx") => found_typescript = true,
            Some("js" | "jsx" | "mjs" | "cjs") => found_javascript = true,
            Some("py") => found_python = true,
            _ => {}
        }
    }
    (found_javascript, found_typescript, found_python)
}

// ── digest shapes ────────────────────────────────────────────────────────

/// One fully-detailed diagnostic — the "first N" the digest surfaces in
/// full. `file` is relative to the project root when the underlying tool
/// reported an absolute path under it.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct Diagnostic {
    file: String,
    line: u64,
    column: u64,
    severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct FileCount {
    path: String,
    errors: u64,
    warnings: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct Totals {
    errors: u64,
    warnings: u64,
    files_with_issues: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct ToolDigest {
    status: &'static str, // always "ran" once constructed
    totals: Totals,
    files: Vec<FileCount>,
    files_truncated: bool,
    diagnostics: Vec<Diagnostic>,
    diagnostics_truncated: bool,
}

/// Fold a raw list of parsed diagnostics into the capped digest shape:
/// per-file error/warning counts (sorted by path for determinism), the
/// first `max_findings` full diagnostics in the tool's own order, and
/// totals. Pure and independent of how the diagnostics were obtained, so
/// it's unit-tested directly against hand-built `Diagnostic` lists (no
/// subprocess, no real linter needed).
fn build_digest(mut diagnostics: Vec<Diagnostic>, max_findings: usize) -> ToolDigest {
    let mut by_file: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    let mut errors = 0u64;
    let mut warnings = 0u64;
    for d in &diagnostics {
        let entry = by_file.entry(d.file.clone()).or_default();
        if d.severity == "error" {
            entry.0 += 1;
            errors += 1;
        } else {
            entry.1 += 1;
            warnings += 1;
        }
    }
    let files_with_issues = by_file.len() as u64;
    let mut files: Vec<FileCount> = by_file
        .into_iter()
        .map(|(path, (e, w))| FileCount {
            path,
            errors: e,
            warnings: w,
        })
        .collect();
    let files_truncated = files.len() > MAX_FILES_PER_TOOL;
    files.truncate(MAX_FILES_PER_TOOL);

    let diagnostics_truncated = diagnostics.len() > max_findings;
    diagnostics.truncate(max_findings);

    ToolDigest {
        status: "ran",
        totals: Totals {
            errors,
            warnings,
            files_with_issues,
        },
        files,
        files_truncated,
        diagnostics,
        diagnostics_truncated,
    }
}

/// Best-effort "make this path relative to `root`" — the parsed
/// diagnostics from every tool carry whatever form the tool itself
/// reported (absolute for tsc/eslint, relative-to-cwd for ruff); this
/// normalises the common absolute case to a short relative path, and
/// falls back to the tool's own string unchanged when it doesn't share a
/// prefix with `root` (e.g. a symlinked file outside the tree).
fn relativize(root: &Path, raw: &str) -> String {
    Path::new(raw)
        .strip_prefix(root)
        .ok()
        .map_or_else(|| raw.to_string(), |p| p.to_string_lossy().into_owned())
}

// ── eslint (`-f json`) ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EslintFileResult {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(default)]
    messages: Vec<EslintMessage>,
}

#[derive(Debug, Deserialize)]
struct EslintMessage {
    #[serde(default, rename = "ruleId")]
    rule_id: Option<String>,
    message: String,
    #[serde(default)]
    line: u64,
    #[serde(default)]
    column: u64,
    /// eslint severity: 1 = warning, 2 = error.
    severity: u8,
}

fn parse_eslint_json(raw: &str, root: &Path) -> Result<Vec<Diagnostic>, String> {
    let files: Vec<EslintFileResult> =
        serde_json::from_str(raw).map_err(|e| format!("could not parse eslint JSON: {e}"))?;
    let mut out = Vec::new();
    for file in files {
        let rel = relativize(root, &file.file_path);
        for msg in file.messages {
            out.push(Diagnostic {
                file: rel.clone(),
                line: msg.line,
                column: msg.column,
                severity: if msg.severity >= 2 {
                    "error"
                } else {
                    "warning"
                }
                .to_string(),
                rule: msg.rule_id,
                message: msg.message,
            });
        }
    }
    Ok(out)
}

// ── tsc (`--pretty false`) — plain-text, one diagnostic per line ────────

/// `tsc --pretty false` emits one line per diagnostic in the form
/// `path(line,col): error TSxxxx: message` (or `warning` for the rarer
/// warning-level diagnostics). No JSON mode exists for `tsc`, so this is
/// a line parser rather than a deserializer.
fn parse_tsc_output(raw: &str, root: &Path) -> Vec<Diagnostic> {
    let re = tsc_line_regex();
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some(caps) = re.captures(line) else {
            continue;
        };
        let file = relativize(root, &caps[1]);
        let line_no: u64 = caps[2].parse().unwrap_or(0);
        let col_no: u64 = caps[3].parse().unwrap_or(0);
        let severity = caps[4].to_string();
        let code = format!("TS{}", &caps[5]);
        let message = caps[6].trim().to_string();
        out.push(Diagnostic {
            file,
            line: line_no,
            column: col_no,
            severity,
            rule: Some(code),
            message,
        });
    }
    out
}

fn tsc_line_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^(.+?)\((\d+),(\d+)\):\s+(error|warning)\s+TS(\d+):\s*(.*)$")
            .expect("static tsc diagnostic regex must compile")
    })
}

// ── ruff (`--output-format json`) ───────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RuffMessage {
    filename: String,
    #[serde(default)]
    code: Option<String>,
    message: String,
    location: RuffLocation,
}

#[derive(Debug, Deserialize)]
struct RuffLocation {
    row: u64,
    column: u64,
}

fn parse_ruff_json(raw: &str, root: &Path) -> Result<Vec<Diagnostic>, String> {
    let messages: Vec<RuffMessage> =
        serde_json::from_str(raw).map_err(|e| format!("could not parse ruff JSON: {e}"))?;
    Ok(messages
        .into_iter()
        .map(|m| Diagnostic {
            file: relativize(root, &m.filename),
            line: m.location.row,
            column: m.location.column,
            // Ruff violations are all reported at "error" severity in its
            // own model — there is no separate warning tier the JSON
            // output distinguishes.
            severity: "error".to_string(),
            rule: m.code,
            message: m.message,
        })
        .collect())
}

// ── subprocess execution ────────────────────────────────────────────────

/// Probe whether `binary` is runnable via `command -v` — the same
/// call-time-only strategy `ui_screenshot` uses for chromium (never a
/// registration-time check, so the minimal profile degrades per-tool
/// instead of never registering this tool at all).
async fn probe_binary(binary: &str) -> bool {
    tokio::process::Command::new("bash")
        .arg("-c")
        .arg(format!("command -v {binary}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Collect up to `max` `.ts`/`.tsx` file paths under `root` (relative to
/// `root`) for the no-`tsconfig.json` invocation form. Sorted for
/// determinism.
fn collect_ts_files(root: &Path, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .follow_links(false)
        .filter_entry(|entry| {
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    if HARD_SKIP_DIRS.contains(&name) {
                        return false;
                    }
                }
            }
            true
        })
        .build();
    for entry in walker {
        if out.len() >= max {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if matches!(
            entry.path().extension().and_then(|e| e.to_str()),
            Some("ts" | "tsx")
        ) {
            if let Ok(rel) = entry.path().strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

fn truncate_note(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cap = max;
    while !s.is_char_boundary(cap) {
        cap -= 1;
    }
    format!("{}…[truncated]", &s[..cap])
}

/// Run one tool end to end: spawn the subprocess, parse its
/// machine-readable output, and fold it into a capped digest. Returns
/// `Err` only when the tool's output could not be parsed at all (a
/// config error, a crash) — a normal "found N errors" run with a
/// non-zero exit code is NOT an error here, since that's exactly what a
/// linter reports when it finds something.
async fn run_tool(
    tool: DiagTool,
    project_root: &Path,
    max_findings: usize,
) -> Result<ToolDigest, String> {
    let (program, args): (&str, Vec<String>) = match tool {
        DiagTool::Eslint => (
            "eslint",
            vec![".".to_string(), "-f".to_string(), "json".to_string()],
        ),
        DiagTool::Tsc => {
            if project_root.join("tsconfig.json").is_file() {
                (
                    "tsc",
                    vec![
                        "--noEmit".to_string(),
                        "--pretty".to_string(),
                        "false".to_string(),
                    ],
                )
            } else {
                let files = collect_ts_files(project_root, MAX_TSC_FILES_WITHOUT_CONFIG);
                if files.is_empty() {
                    return Err(
                        "no tsconfig.json and no .ts/.tsx files found to typecheck".to_string()
                    );
                }
                let mut a = vec![
                    "--noEmit".to_string(),
                    "--pretty".to_string(),
                    "false".to_string(),
                ];
                a.extend(files);
                ("tsc", a)
            }
        }
        DiagTool::Ruff => (
            "ruff",
            vec![
                "check".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
                ".".to_string(),
            ],
        ),
    };

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&args)
        .current_dir(project_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch {program}: {e}"))?;
    let output = match tokio::time::timeout(TOOL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("{program} wait failed: {e}")),
        Err(_) => {
            return Err(format!(
                "{program} timed out after {}s",
                TOOL_TIMEOUT.as_secs()
            ));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let parsed = match tool {
        DiagTool::Eslint => parse_eslint_json(&stdout, project_root)
            .map_err(|e| format!("{e}; stderr: {}", truncate_note(&stderr, ERROR_NOTE_CAP)))?,
        DiagTool::Tsc => parse_tsc_output(&stdout, project_root),
        DiagTool::Ruff => parse_ruff_json(&stdout, project_root)
            .map_err(|e| format!("{e}; stderr: {}", truncate_note(&stderr, ERROR_NOTE_CAP)))?,
    };
    Ok(build_digest(parsed, max_findings))
}

fn not_available_note(tool: DiagTool) -> Value {
    json!({
        "status": "not_available",
        "note": format!(
            "{name} is not available in this image ({bin} not found on PATH). It ships with the \
             `prototyping` image profile — switch this group to it and restart, e.g. \
             `cclaw groups config update --field 'image_profile=\"prototyping\"' <group-id>` then \
             `cclaw groups restart <group-id>`. Until then `shell`/`diagnostics`' other tools \
             still work.",
            name = tool.name(),
            bin = tool.binary(),
        ),
    })
}

fn error_note(message: &str) -> Value {
    json!({
        "status": "error",
        "note": message,
    })
}

/// Build the full digest for `project_root`: detect applicable tools, then
/// for each, probe/run/parse and assemble the response. Split out of
/// [`handle`] so it's testable without going through the rmcp argument
/// envelope.
async fn diagnose_project(project_root: &Path, max_findings: usize) -> Value {
    let applicable = detect_applicable_tools(project_root);
    if applicable.is_empty() {
        return json!({
            "project": project_root.display().to_string(),
            "applicable_tools": [],
            "results": {},
            "summary": {
                "errors": 0,
                "warnings": 0,
                "tools_ran": [],
                "tools_unavailable": [],
            },
            "note": format!(
                "No eslint/tsc/ruff config file or matching source files (.ts/.tsx, .js/.jsx/.mjs/.cjs, \
                 .py) were found under {} — nothing to run.",
                project_root.display()
            ),
        });
    }

    let mut results = serde_json::Map::new();
    let mut total_errors = 0u64;
    let mut total_warnings = 0u64;
    let mut tools_ran: Vec<&'static str> = Vec::new();
    let mut tools_unavailable: Vec<&'static str> = Vec::new();

    for tool in &applicable {
        if !probe_binary(tool.binary()).await {
            results.insert(tool.name().to_string(), not_available_note(*tool));
            tools_unavailable.push(tool.name());
            copperclaw_metrics::inc_diagnostics_run(tool.name(), "not_available");
            continue;
        }
        match run_tool(*tool, project_root, max_findings).await {
            Ok(digest) => {
                total_errors += digest.totals.errors;
                total_warnings += digest.totals.warnings;
                tools_ran.push(tool.name());
                results.insert(
                    tool.name().to_string(),
                    serde_json::to_value(&digest).unwrap_or(Value::Null),
                );
                copperclaw_metrics::inc_diagnostics_run(tool.name(), "ran");
            }
            Err(message) => {
                results.insert(tool.name().to_string(), error_note(&message));
                copperclaw_metrics::inc_diagnostics_run(tool.name(), "error");
            }
        }
    }

    json!({
        "project": project_root.display().to_string(),
        "applicable_tools": applicable.iter().map(|t| t.name()).collect::<Vec<_>>(),
        "results": Value::Object(results),
        "summary": {
            "errors": total_errors,
            "warnings": total_warnings,
            "tools_ran": tools_ran,
            "tools_unavailable": tools_unavailable,
        },
    })
}

pub async fn handle(
    arguments: Option<JsonObject>,
    _ctx: &dyn crate::context::ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    let max_findings = input
        .max_findings
        .unwrap_or(DEFAULT_MAX_DIAGNOSTICS)
        .clamp(1, MAX_DIAGNOSTICS_CEILING);

    let project_root = PathBuf::from(&input.project);
    if !project_root.is_dir() {
        return Err(ToolError::Validation(format!(
            "diagnostics: `{}` is not a directory (or doesn't exist) — pass the project path you \
             use as `shell`'s `cwd`.",
            input.project
        )));
    }

    let digest = diagnose_project(&project_root, max_findings).await;
    Ok(success_json(&digest))
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

// ── M22 C1: post-edit verify hook ────────────────────────────────────────
//
// After a successful `edit_file`/`multi_edit`/`apply_patch`/`write_file`
// mutation, run the SAME format/typecheck logic this module already owns,
// scoped to JUST the touched file (not the whole repo — that stays the
// on-demand `diagnostics` tool's job), and hand the caller a concise digest
// to append to the tool result. The point is feedback IN THE LOOP: a type
// or lint break surfaces to the *model* on its next turn instead of leaking
// to the user. On a clean edit the digest is absent — no spam.
//
// This runs the same toolchain commands the agent can already invoke by
// hand via `shell`/`diagnostics`; it does not expand the trust boundary,
// it only auto-invokes an already-available check on the file just written
// (see `docs/plans/m22-security-reviews.md`, C1).

/// Env var operators set to disable the post-edit verify hook. Default ON
/// (opt-out only, per the card). Any of `0`/`false`/`off`/`no`
/// (case-insensitive, whitespace-trimmed) turns it off; anything else —
/// including unset — leaves it on.
const POST_EDIT_VERIFY_ENV: &str = "COPPERCLAW_POST_EDIT_VERIFY";

/// Cap on how many full diagnostics the post-edit digest carries. Smaller
/// than [`DEFAULT_MAX_DIAGNOSTICS`] on purpose — this fires after EVERY
/// edit and lives in conversation history, so it stays terse; the model can
/// call the full `diagnostics` tool when it wants the complete list.
const POST_EDIT_MAX_DIAGNOSTICS: usize = 10;

/// Interpret the [`POST_EDIT_VERIFY_ENV`] value. Pure so it's unit-testable
/// without mutating the process environment (forbidden under edition-2024
/// `unsafe`-free rules).
fn parse_post_edit_verify_flag(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
    }
}

/// Test-only in-process override for [`post_edit_verify_enabled`]. Lets the
/// suite force the gate without touching env vars.
#[cfg(test)]
static POST_EDIT_VERIFY_TEST_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<bool>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn post_edit_verify_test_override_set(v: Option<bool>) {
    let cell = POST_EDIT_VERIFY_TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = v;
}

/// Whether the post-edit verify hook is enabled for this session. Default
/// ON; reads [`POST_EDIT_VERIFY_ENV`] at call time (mirroring how
/// `diagnostics`/`ui_screenshot` probe the environment lazily rather than at
/// registration).
fn post_edit_verify_enabled() -> bool {
    #[cfg(test)]
    if let Some(cell) = POST_EDIT_VERIFY_TEST_OVERRIDE.get() {
        if let Some(v) = *cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return v;
        }
    }
    parse_post_edit_verify_flag(std::env::var(POST_EDIT_VERIFY_ENV).ok().as_deref())
}

/// Pick the single applicable checker for a touched file from its
/// extension. `None` when no toolchain this module knows about applies
/// (a `.md`, a `.rs`, a `.toml`, …) — the graceful "nothing to check"
/// path, never an error.
fn tool_for_extension(path: &Path) -> Option<DiagTool> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("ts" | "tsx" | "mts" | "cts") => Some(DiagTool::Tsc),
        Some("js" | "jsx" | "mjs" | "cjs") => Some(DiagTool::Eslint),
        Some("py" | "pyi") => Some(DiagTool::Ruff),
        _ => None,
    }
}

/// Run one checker over a SINGLE file, from its parent directory so config
/// discovery (eslint/ruff/tsc) and path relativisation both key off a short
/// bare filename. Reuses this module's parsers verbatim.
async fn run_tool_on_file(
    tool: DiagTool,
    dir: &Path,
    file_name: &str,
) -> Result<Vec<Diagnostic>, String> {
    let args: Vec<String> = match tool {
        DiagTool::Eslint => vec![file_name.to_string(), "-f".to_string(), "json".to_string()],
        DiagTool::Tsc => vec![
            "--noEmit".to_string(),
            "--pretty".to_string(),
            "false".to_string(),
            file_name.to_string(),
        ],
        DiagTool::Ruff => vec![
            "check".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            file_name.to_string(),
        ],
    };

    let mut cmd = tokio::process::Command::new(tool.binary());
    cmd.args(&args)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch {}: {e}", tool.binary()))?;
    let output = match tokio::time::timeout(TOOL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("{} wait failed: {e}", tool.binary())),
        Err(_) => {
            return Err(format!(
                "{} timed out after {}s",
                tool.binary(),
                TOOL_TIMEOUT.as_secs()
            ));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match tool {
        DiagTool::Eslint => parse_eslint_json(&stdout, dir)
            .map_err(|e| format!("{e}; stderr: {}", truncate_note(&stderr, ERROR_NOTE_CAP))),
        DiagTool::Tsc => Ok(parse_tsc_output(&stdout, dir)),
        DiagTool::Ruff => parse_ruff_json(&stdout, dir)
            .map_err(|e| format!("{e}; stderr: {}", truncate_note(&stderr, ERROR_NOTE_CAP))),
    }
}

/// Fold parsed diagnostics into the concise post-edit digest, or `None`
/// when the file is clean (no diagnostics — the common case, kept silent).
/// Pure, so it's unit-tested directly against hand-built `Diagnostic`
/// lists — the acceptance criteria ("a bad edit yields a non-empty digest",
/// "a clean edit yields none") land here without needing a real linter.
fn build_post_edit_digest(tool: DiagTool, file: &str, diagnostics: &[Diagnostic]) -> Option<Value> {
    if diagnostics.is_empty() {
        return None;
    }
    let errors = diagnostics.iter().filter(|d| d.severity == "error").count() as u64;
    let warnings = diagnostics.len() as u64 - errors;
    let diagnostics_truncated = diagnostics.len() > POST_EDIT_MAX_DIAGNOSTICS;
    let shown: Vec<&Diagnostic> = diagnostics.iter().take(POST_EDIT_MAX_DIAGNOSTICS).collect();
    Some(json!({
        "hook": "post_edit_verify",
        "tool": tool.name(),
        "file": file,
        "errors": errors,
        "warnings": warnings,
        "diagnostics": shown,
        "diagnostics_truncated": diagnostics_truncated,
        "note": format!(
            "post-edit verify ran `{}` on this file and found {errors} error(s), {warnings} \
             warning(s). Fix them before continuing — this check runs automatically after each \
             edit (opt out with {POST_EDIT_VERIFY_ENV}=0).",
            tool.name(),
        ),
    }))
}

/// The public hook: after a successful mutation to `path`, run the
/// applicable checker over just that file and return a concise digest to
/// append to the tool result. Returns `None` — silently, never an error —
/// when the hook is disabled, no toolchain applies to the file type, the
/// checker binary isn't installed in this image, the run couldn't be
/// parsed, or the file is clean. Best-effort by design: a hiccup here must
/// never turn a successful edit into a failure.
pub async fn post_edit_verify(path: &str) -> Option<Value> {
    // M22 C1 metric: emit one `copperclaw_post_edit_verify_total{tool, outcome}`
    // per attempt, labelling each early-exit and terminal outcome. `?` is
    // expanded into explicit branches so every exit is attributed.
    if !post_edit_verify_enabled() {
        copperclaw_metrics::inc_post_edit_verify("none", "disabled");
        return None;
    }
    let file = Path::new(path);
    let Some(tool) = tool_for_extension(file) else {
        copperclaw_metrics::inc_post_edit_verify("none", "unsupported");
        return None;
    };
    if !probe_binary(tool.binary()).await {
        copperclaw_metrics::inc_post_edit_verify(tool.name(), "not_available");
        return None;
    }
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let Some(file_name) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        copperclaw_metrics::inc_post_edit_verify(tool.name(), "error");
        return None;
    };
    let Ok(diagnostics) = run_tool_on_file(tool, &dir, &file_name).await else {
        copperclaw_metrics::inc_post_edit_verify(tool.name(), "error");
        return None;
    };
    let digest = build_post_edit_digest(tool, &file_name, &diagnostics);
    if digest.is_some() {
        copperclaw_metrics::inc_post_edit_verify(tool.name(), "flagged");
        copperclaw_metrics::observe_post_edit_verify_findings(diagnostics.len() as u64);
    } else {
        copperclaw_metrics::inc_post_edit_verify(tool.name(), "clean");
    }
    digest
}

/// Convenience wiring shared by all four mutation tools
/// (`edit_file`/`multi_edit`/`apply_patch`/`write_file`): run
/// [`post_edit_verify`] for `path` and, when it yields a digest, insert it
/// under `post_edit_diagnostics` in the tool's result object. No-op when
/// the digest is absent, so a clean edit's result shape is unchanged.
pub async fn append_post_edit_digest(out: &mut Value, path: &str) {
    if let Some(digest) = post_edit_verify(path).await {
        if let Some(map) = out.as_object_mut() {
            map.insert("post_edit_diagnostics".into(), digest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    // ── applicability detection ─────────────────────────────────────────

    #[test]
    fn detects_tsc_via_tsconfig() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("tsconfig.json"), "{}");
        write(&dir.path().join("src/index.ts"), "const x: number = 1;\n");
        let tools = detect_applicable_tools(dir.path());
        assert_eq!(tools, vec![DiagTool::Tsc]);
    }

    #[test]
    fn detects_tsc_via_extension_sniff_without_config() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/index.ts"), "const x: number = 1;\n");
        let tools = detect_applicable_tools(dir.path());
        assert_eq!(tools, vec![DiagTool::Tsc]);
    }

    #[test]
    fn detects_eslint_via_config_file() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".eslintrc.json"), "{}");
        let tools = detect_applicable_tools(dir.path());
        assert_eq!(tools, vec![DiagTool::Eslint]);
    }

    #[test]
    fn detects_eslint_via_js_extension_sniff() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("src/index.js"), "console.log(1);\n");
        let tools = detect_applicable_tools(dir.path());
        assert_eq!(tools, vec![DiagTool::Eslint]);
    }

    #[test]
    fn python_project_routes_to_ruff_only() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("app.py"), "import os\n");
        write(&dir.path().join("lib/util.py"), "def f():\n    pass\n");
        let tools = detect_applicable_tools(dir.path());
        assert_eq!(tools, vec![DiagTool::Ruff]);
    }

    #[test]
    fn detects_ruff_via_pyproject_toml_table() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n\n[tool.ruff]\nline-length = 100\n",
        );
        let tools = detect_applicable_tools(dir.path());
        assert_eq!(tools, vec![DiagTool::Ruff]);
    }

    #[test]
    fn plain_pyproject_without_ruff_table_is_not_applicable_alone() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n",
        );
        let tools = detect_applicable_tools(dir.path());
        assert!(tools.is_empty());
    }

    #[test]
    fn project_with_no_applicable_tool_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("README.md"), "# hello\n");
        let tools = detect_applicable_tools(dir.path());
        assert!(tools.is_empty());
    }

    #[test]
    fn skips_node_modules_and_git_when_sniffing() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("node_modules/pkg/index.js"), "x");
        write(&dir.path().join(".git/hooks/index.js"), "x");
        let tools = detect_applicable_tools(dir.path());
        assert!(tools.is_empty());
    }

    #[test]
    fn mixed_project_detects_multiple_tools() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("tsconfig.json"), "{}");
        write(&dir.path().join(".eslintrc.json"), "{}");
        write(&dir.path().join("app.py"), "import os\n");
        let mut tools = detect_applicable_tools(dir.path());
        tools.sort_by_key(|t| t.name());
        assert_eq!(tools, vec![DiagTool::Eslint, DiagTool::Ruff, DiagTool::Tsc]);
    }

    // ── parsing: eslint `-f json` ────────────────────────────────────────

    const ESLINT_SAMPLE: &str = r#"[
        {
            "filePath": "/proj/src/index.ts",
            "messages": [
                {"ruleId": "no-unused-vars", "severity": 2, "message": "'x' is defined but never used.", "line": 3, "column": 7},
                {"ruleId": "eqeqeq", "severity": 1, "message": "Expected '===' and instead saw '=='.", "line": 10, "column": 1}
            ],
            "errorCount": 1,
            "warningCount": 1
        },
        {
            "filePath": "/proj/src/clean.ts",
            "messages": [],
            "errorCount": 0,
            "warningCount": 0
        }
    ]"#;

    #[test]
    fn parses_eslint_json_sample() {
        let diags = parse_eslint_json(ESLINT_SAMPLE, Path::new("/proj")).unwrap();
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].file, "src/index.ts");
        assert_eq!(diags[0].severity, "error");
        assert_eq!(diags[0].rule.as_deref(), Some("no-unused-vars"));
        assert_eq!(diags[1].severity, "warning");
    }

    #[test]
    fn parse_eslint_json_rejects_garbage() {
        assert!(parse_eslint_json("not json", Path::new("/proj")).is_err());
    }

    // ── parsing: tsc `--pretty false` ────────────────────────────────────

    #[test]
    fn parses_tsc_plain_text_sample() {
        let raw = "src/index.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.\n\
                    src/other.ts(2,1): warning TS6133: 'y' is declared but its value is never read.\n\
                    Found 2 errors.\n";
        let diags = parse_tsc_output(raw, Path::new("/proj"));
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].file, "src/index.ts");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[0].column, 5);
        assert_eq!(diags[0].severity, "error");
        assert_eq!(diags[0].rule.as_deref(), Some("TS2322"));
        assert!(diags[0].message.contains("not assignable"));
        assert_eq!(diags[1].severity, "warning");
    }

    /// The acceptance case: a ~40-error tsc run must produce a digest well
    /// under the 32 KiB `shell` truncation cap
    /// (`crate::tools::computer_use::SHELL_OUTPUT_CAP`), with counts +
    /// first N full diagnostics.
    #[test]
    fn forty_tsc_errors_digest_stays_well_under_shell_truncation_cap() {
        let mut raw = String::new();
        for i in 0..40 {
            raw.push_str(&format!(
                "src/file{}.ts({},3): error TS2322: Type 'string' is not assignable to type 'number' \
                 in this moderately long diagnostic message so the raw text is non-trivial.\n",
                i % 8,
                i + 1
            ));
        }
        let diags = parse_tsc_output(&raw, Path::new("/proj"));
        assert_eq!(diags.len(), 40);

        let digest = build_digest(diags, DEFAULT_MAX_DIAGNOSTICS);
        assert_eq!(digest.totals.errors, 40);
        assert_eq!(digest.totals.files_with_issues, 8);
        assert_eq!(digest.diagnostics.len(), 40); // under the 40 cap, so all present
        assert!(!digest.diagnostics_truncated);

        let serialized = serde_json::to_string(&digest).unwrap();
        // 32 KiB is `computer_use::SHELL_OUTPUT_CAP`; assert the concrete
        // cap this tool promises: a 40-error digest stays well under it.
        assert!(
            serialized.len() < 32 * 1024,
            "digest was {} bytes",
            serialized.len()
        );
    }

    #[test]
    fn digest_truncates_diagnostics_beyond_max_findings_and_flags_it() {
        let mut diags = Vec::new();
        for i in 0..40 {
            diags.push(Diagnostic {
                file: format!("src/file{i}.ts"),
                line: 1,
                column: 1,
                severity: "error".to_string(),
                rule: Some("TS2322".to_string()),
                message: "boom".to_string(),
            });
        }
        let digest = build_digest(diags, 10);
        assert_eq!(digest.diagnostics.len(), 10);
        assert!(digest.diagnostics_truncated);
        // Totals/file counts are NOT capped by max_findings — only the
        // "first N full diagnostics" list is.
        assert_eq!(digest.totals.errors, 40);
        assert_eq!(digest.totals.files_with_issues, 40);
    }

    // ── parsing: ruff `--output-format json` ─────────────────────────────

    const RUFF_SAMPLE: &str = r#"[
        {
            "cell": null,
            "code": "F401",
            "filename": "/proj/app.py",
            "message": "`os` imported but unused",
            "location": {"row": 1, "column": 1},
            "end_location": {"row": 1, "column": 9}
        },
        {
            "cell": null,
            "code": "E501",
            "filename": "/proj/lib/util.py",
            "message": "Line too long (105 > 88)",
            "location": {"row": 5, "column": 89},
            "end_location": {"row": 5, "column": 105}
        }
    ]"#;

    #[test]
    fn parses_ruff_json_sample() {
        let diags = parse_ruff_json(RUFF_SAMPLE, Path::new("/proj")).unwrap();
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].file, "app.py");
        assert_eq!(diags[0].rule.as_deref(), Some("F401"));
        assert_eq!(diags[1].file, "lib/util.py");
        assert_eq!(diags[1].line, 5);
    }

    #[test]
    fn parse_ruff_json_rejects_garbage() {
        assert!(parse_ruff_json("<html>not json</html>", Path::new("/proj")).is_err());
    }

    // ── binary probing ───────────────────────────────────────────────────

    #[tokio::test]
    async fn probe_binary_is_false_for_a_name_that_cannot_exist() {
        assert!(!probe_binary("copperclaw-diagnostics-tool-that-does-not-exist-xyz123").await);
    }

    #[test]
    fn not_available_note_names_the_tool_and_prototyping_profile() {
        let v = not_available_note(DiagTool::Tsc);
        assert_eq!(v["status"], "not_available");
        let note = v["note"].as_str().unwrap();
        assert!(note.contains("tsc"));
        assert!(note.contains("prototyping"));
    }

    // ── end-to-end (no real linters needed): the "nothing applies" and
    //    "missing binary" degrade paths ───────────────────────────────────

    #[tokio::test]
    async fn diagnose_project_reports_cleanly_when_nothing_applies() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("README.md"), "# hello\n");
        let result = diagnose_project(dir.path(), DEFAULT_MAX_DIAGNOSTICS).await;
        assert_eq!(result["applicable_tools"], json!([]));
        assert!(result["note"].as_str().unwrap().contains("nothing to run"));
    }

    #[tokio::test]
    async fn handle_rejects_a_nonexistent_project_path() {
        let mut args = JsonObject::new();
        args.insert(
            "project".into(),
            "/definitely/does/not/exist/anywhere".into(),
        );
        let ctx = crate::context::MockToolContext::new();
        let err = handle(Some(args), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    // ── schema ────────────────────────────────────────────────────────────

    #[test]
    fn schema_describes_read_only_no_gate_posture() {
        let tool = schema();
        assert_eq!(tool.name, "diagnostics");
        let desc = tool
            .description
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        assert!(desc.contains("read-only"));
        assert!(desc.contains("verify"));
        assert!(desc.contains("eslint"));
        assert!(desc.contains("tsc"));
        assert!(desc.contains("ruff"));
    }

    // ── live integration (Q1 image-gated) ────────────────────────────────

    /// Live end-to-end: a real tsc run against a fixture project with a
    /// deliberate type error. Needs the `prototyping` image's baked
    /// `typescript`/`tsc` — this dev/CI box may not have it, so this is
    /// `#[ignore]`d per the `ui_screenshot_docker_end_to_end` precedent.
    /// Opt in with `cargo test -p copperclaw-mcp -- --ignored
    /// diagnostics_live_tsc_end_to_end`.
    #[tokio::test]
    #[ignore = "requires a real `tsc` on PATH (the prototyping image); opt in with --ignored"]
    async fn diagnostics_live_tsc_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("tsconfig.json"),
            "{\"compilerOptions\":{\"strict\":true}}",
        );
        write(
            &dir.path().join("src/index.ts"),
            "const x: number = \"not a number\";\nconsole.log(x);\n",
        );
        let mut args = JsonObject::new();
        args.insert(
            "project".into(),
            dir.path().to_string_lossy().into_owned().into(),
        );
        let ctx = crate::context::MockToolContext::new();
        let res = handle(Some(args), &ctx)
            .await
            .expect("diagnostics should succeed against a real tsc");
        assert_eq!(res.is_error, Some(false));
        let body = result_json(&res);
        // Assert `tsc` actually RAN (not just degraded to "not_available")
        // and caught the deliberate type error — a weaker assertion (just
        // `is_error == false`) would pass identically whether or not `tsc`
        // was ever on PATH, since the degrade path also returns success.
        assert_eq!(
            body["results"]["tsc"]["status"], "ran",
            "expected a real `tsc` run, got: {body}"
        );
        assert!(body["results"]["tsc"]["totals"]["errors"].as_u64().unwrap() >= 1);
    }

    /// Live end-to-end: a real ruff run against a fixture project with an
    /// unused import. `#[ignore]`d for the same reason as the tsc case
    /// above (this dev box happens to have `ruff`, but CI may not).
    #[tokio::test]
    #[ignore = "requires a real `ruff` on PATH (the prototyping image); opt in with --ignored"]
    async fn diagnostics_live_ruff_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("app.py"), "import os\n\nprint('hi')\n");
        let mut args = JsonObject::new();
        args.insert(
            "project".into(),
            dir.path().to_string_lossy().into_owned().into(),
        );
        let ctx = crate::context::MockToolContext::new();
        let res = handle(Some(args), &ctx)
            .await
            .expect("diagnostics should succeed against a real ruff");
        assert_eq!(res.is_error, Some(false));
        let body = result_json(&res);
        assert_eq!(
            body["results"]["ruff"]["status"], "ran",
            "expected a real `ruff` run, got: {body}"
        );
        assert!(
            body["results"]["ruff"]["totals"]["errors"]
                .as_u64()
                .unwrap()
                >= 1
        );
    }

    // ── M22 C1: post-edit verify hook ────────────────────────────────────

    fn diag(file: &str, severity: &str, rule: &str, message: &str) -> Diagnostic {
        Diagnostic {
            file: file.to_string(),
            line: 1,
            column: 7,
            severity: severity.to_string(),
            rule: Some(rule.to_string()),
            message: message.to_string(),
        }
    }

    #[test]
    fn post_edit_flag_defaults_on_and_honours_opt_out() {
        assert!(parse_post_edit_verify_flag(None));
        assert!(parse_post_edit_verify_flag(Some("1")));
        assert!(parse_post_edit_verify_flag(Some("true")));
        assert!(parse_post_edit_verify_flag(Some("anything-else")));
        for off in ["0", "false", "off", "no", "  OFF ", "False"] {
            assert!(
                !parse_post_edit_verify_flag(Some(off)),
                "`{off}` should disable the hook"
            );
        }
    }

    #[test]
    fn post_edit_test_override_forces_gate() {
        post_edit_verify_test_override_set(Some(false));
        assert!(!post_edit_verify_enabled());
        post_edit_verify_test_override_set(Some(true));
        assert!(post_edit_verify_enabled());
        post_edit_verify_test_override_set(None);
    }

    #[test]
    fn tool_for_extension_maps_known_source_types() {
        assert_eq!(tool_for_extension(Path::new("a.ts")), Some(DiagTool::Tsc));
        assert_eq!(tool_for_extension(Path::new("a.tsx")), Some(DiagTool::Tsc));
        assert_eq!(
            tool_for_extension(Path::new("a.js")),
            Some(DiagTool::Eslint)
        );
        assert_eq!(
            tool_for_extension(Path::new("a.mjs")),
            Some(DiagTool::Eslint)
        );
        assert_eq!(tool_for_extension(Path::new("a.py")), Some(DiagTool::Ruff));
        // No toolchain applies → None, never an error (the .md / .rs case).
        assert_eq!(tool_for_extension(Path::new("README.md")), None);
        assert_eq!(tool_for_extension(Path::new("main.rs")), None);
        assert_eq!(tool_for_extension(Path::new("Cargo.toml")), None);
        assert_eq!(tool_for_extension(Path::new("Makefile")), None);
    }

    /// Acceptance: a bad edit yields a NON-EMPTY digest.
    #[test]
    fn bad_edit_yields_nonempty_digest() {
        let diags = vec![diag(
            "index.ts",
            "error",
            "TS2322",
            "Type 'string' is not assignable to type 'number'.",
        )];
        let digest =
            build_post_edit_digest(DiagTool::Tsc, "index.ts", &diags).expect("digest present");
        assert_eq!(digest["hook"], "post_edit_verify");
        assert_eq!(digest["tool"], "tsc");
        assert_eq!(digest["file"], "index.ts");
        assert_eq!(digest["errors"], 1);
        assert_eq!(digest["warnings"], 0);
        assert_eq!(digest["diagnostics"].as_array().unwrap().len(), 1);
        assert_eq!(digest["diagnostics_truncated"], false);
        assert!(
            digest["note"]
                .as_str()
                .unwrap()
                .contains("post-edit verify")
        );
    }

    /// Acceptance: a clean edit yields NO digest (absent, not empty).
    #[test]
    fn clean_edit_yields_no_digest() {
        assert!(build_post_edit_digest(DiagTool::Ruff, "app.py", &[]).is_none());
    }

    #[test]
    fn digest_counts_errors_and_warnings_and_caps_the_list() {
        let mut diags = Vec::new();
        // 12 errors + 3 warnings → totals uncapped, list capped at 10.
        for i in 0..12 {
            diags.push(diag("index.ts", "error", "TS2322", &format!("err {i}")));
        }
        for i in 0..3 {
            diags.push(diag("index.ts", "warning", "TS6133", &format!("warn {i}")));
        }
        let digest = build_post_edit_digest(DiagTool::Tsc, "index.ts", &diags).unwrap();
        assert_eq!(digest["errors"], 12);
        assert_eq!(digest["warnings"], 3);
        assert_eq!(
            digest["diagnostics"].as_array().unwrap().len(),
            POST_EDIT_MAX_DIAGNOSTICS
        );
        assert_eq!(digest["diagnostics_truncated"], true);
    }

    /// The recorded-digest fixture (acceptance: "Fixture: recorded
    /// diagnostics digest"): building the digest for a canonical tsc type
    /// error must match the golden JSON checked in under `fixtures/`. This
    /// pins the exact shape/wording the model sees fed back next turn.
    #[test]
    fn digest_matches_recorded_fixture() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/diagnostics/post-edit-digest/recorded-digest.json");
        let recorded: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture).unwrap()).unwrap();
        let diags = vec![diag(
            "index.ts",
            "error",
            "TS2322",
            "Type 'string' is not assignable to type 'number'.",
        )];
        let digest = build_post_edit_digest(DiagTool::Tsc, "index.ts", &diags).unwrap();
        assert_eq!(digest, recorded, "post-edit digest drifted from fixture");
    }

    #[tokio::test]
    async fn append_post_edit_digest_is_noop_for_unsupported_type() {
        // A `.md` file has no applicable checker → the result object is
        // returned byte-identical (no `post_edit_diagnostics` key).
        let mut out = json!({ "path": "/tmp/notes.md", "bytes_written": 3 });
        let before = out.clone();
        append_post_edit_digest(&mut out, "/tmp/notes.md").await;
        assert_eq!(out, before);
    }

    #[tokio::test]
    async fn post_edit_verify_is_none_when_disabled() {
        post_edit_verify_test_override_set(Some(false));
        // Even a would-be-checked extension returns None when opted out.
        assert!(post_edit_verify("/tmp/whatever.py").await.is_none());
        post_edit_verify_test_override_set(None);
    }

    #[tokio::test]
    async fn post_edit_verify_is_none_for_unsupported_extension() {
        // Independent of any installed toolchain: `.md` maps to no tool.
        assert!(post_edit_verify("/tmp/readme.md").await.is_none());
    }

    /// Live end-to-end: a real `ruff` run through the post-edit hook against
    /// a `.py` file with an unused import must produce a non-empty digest.
    /// `#[ignore]`d like the other live diagnostics tests — needs `ruff` on
    /// PATH (the `prototyping` image); opt in with
    /// `--ignored post_edit_verify_live_ruff`.
    #[tokio::test]
    #[ignore = "requires a real `ruff` on PATH (the prototyping image); opt in with --ignored"]
    async fn post_edit_verify_live_ruff() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.py");
        write(&path, "import os\n\nprint('hi')\n");
        let digest = post_edit_verify(&path.to_string_lossy())
            .await
            .expect("ruff should flag the unused import");
        assert_eq!(digest["tool"], "ruff");
        assert!(digest["errors"].as_u64().unwrap() >= 1);
    }

    /// Parse the single text block a `success_json`-shaped `CallToolResult`
    /// carries back into a `serde_json::Value` for assertion.
    fn result_json(res: &CallToolResult) -> Value {
        let text = res
            .content
            .iter()
            .find_map(|c| match &c.raw {
                rmcp::model::RawContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .expect("expected a text content block");
        serde_json::from_str(&text).expect("tool result text should be valid JSON")
    }
}
