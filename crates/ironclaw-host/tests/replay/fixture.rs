//! Replay fixture loader.
//!
//! See `docs/replay-fixtures.md`. A fixture is a directory:
//!
//! ```text
//! fixtures/<channel>/<scenario>/
//! ├── manifest.json
//! ├── central.sql
//! ├── inbound/NNN-*.json   // serialized `InboundEvent`
//! ├── claude/NNN-turn.json // sequence of Anthropic SSE events
//! └── expected/
//!     ├── inbound-events.jsonl
//!     ├── messages-in.jsonl
//!     ├── messages-out.jsonl
//!     └── delivered.jsonl
//! ```
//!
//! For the v1 harness the manifest uses JSON (the design doc shows TOML;
//! switching is a single dep away when a future fixture needs the human-
//! friendlier syntax). All other on-disk shapes match the design.
#![allow(dead_code)]

use anyhow::{Context, Result};
use ironclaw_types::InboundEvent;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level fixture metadata loaded from `manifest.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub channel: String,
    #[serde(default)]
    pub description: String,
    pub schema: u32,
    #[serde(default)]
    pub replay: ReplayPlan,
    /// Map of regex → replacement applied to every JSONL line before
    /// diffing. The order in which entries are applied is the iteration
    /// order of a `BTreeMap`, i.e. lexicographic by regex source — deterministic
    /// across runs.
    #[serde(default)]
    pub substitutions: BTreeMap<String, String>,
    /// Optional per-LLM-call response plan for failure-mode fixtures.
    /// When absent the harness falls back to the legacy behaviour of
    /// dispensing the i-th `claude/NNN-turn.json` for the i-th request.
    /// Each entry maps to one upstream call. See [`ProviderResponseSpec`].
    #[serde(default)]
    pub provider_responses: Vec<ProviderResponseSpec>,
    /// Optional list of host-side gates the harness must wire BEFORE
    /// running the fixture. Accepted values:
    ///
    /// - `"approvals"` — installs [`ironclaw_modules::ApprovalsModule`]
    ///   on the router so unknown senders trigger the sender-scope gate
    ///   (returning `Pending` and dispatching the "approve?" notice
    ///   through the delivery dispatcher).
    /// - `"budget"` — instead of running the in-process runner after
    ///   route, the harness drives a `ContainerManager::tick()`. This
    ///   exercises the daily-token-cap gate which posts a
    ///   "budget exhausted" reply to the session's outbound DB.
    ///
    /// Default: empty (existing happy-path fixtures don't set this).
    #[serde(default)]
    pub gates: Vec<String>,
    /// When true, the harness calls `SweepService::run_once()` after
    /// applying `central.sql` and BEFORE processing any inbound events.
    /// Used by the `scheduled-wake` fixture to deterministically drive
    /// the due-message wake check without waiting for the 60s sweep
    /// tick. The seeded session is then processed through the in-
    /// process runner as if a fresh inbound had arrived.
    #[serde(default)]
    pub trigger_sweep: bool,
}

/// One scripted response from the harness's LLM stub. `kind` decides what
/// the wiremock-served `/v1/messages` endpoint does on the i-th call:
///
/// - `"success"` — return the `claude/NNN-turn.json` file named by
///   `file` (defaults to the i-th turn file in directory order).
/// - `"error"` — return an HTTP error with `status` (default 503) and
///   `message` (default `"service unavailable"`).
/// - `"timeout"` — never respond. Combined with a tight per-step budget
///   on the test side this simulates an upstream that hangs.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderResponseSpec {
    pub kind: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplayPlan {
    /// One of `webhook | gateway | poll | rpc | direct`. The `direct`
    /// variant — only used by the v1 cli fixture — pushes
    /// `InboundEvent`s straight at the router rather than through a
    /// channel transport mock.
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_step_timeout_ms")]
    pub step_timeout_ms: u64,
}

impl Default for ReplayPlan {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            step_timeout_ms: default_step_timeout_ms(),
        }
    }
}

fn default_mode() -> String {
    "direct".to_string()
}

fn default_step_timeout_ms() -> u64 {
    5_000
}

/// One captured Claude turn — a list of SSE events the wiremock-served
/// `/v1/messages` endpoint hands back in order.
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeTurn {
    pub events: Vec<serde_json::Value>,
}

/// Loaded fixture. All file contents read into memory eagerly so the
/// harness has a stable view of the on-disk state for the run.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub central_sql: String,
    /// Optional SQL applied to every active session's inbound.db AFTER
    /// migrations + `central.sql` but BEFORE any inbound events are
    /// processed (and before `trigger_sweep` fires). Used by the
    /// scheduled-wake fixture to seed a "due now" `messages_in` row in
    /// a session that already exists in `central.sql`.
    ///
    /// Loaded from `inbound.sql` in the fixture root if present; empty
    /// when the file is absent. The harness applies the SQL once per
    /// session listed in `sessions::list_active(central)`.
    pub inbound_sql: String,
    /// Inbound `InboundEvent`s in file-name order.
    pub inbound: Vec<InboundEvent>,
    /// Claude turn responses in file-name order. Element `i` is served
    /// for the i-th call into the Anthropic mock when the manifest does
    /// not declare an explicit `provider_responses` plan.
    pub claude_turns: Vec<ClaudeTurn>,
    /// Same payloads as `claude_turns`, keyed by file basename, so
    /// `provider_responses` entries can refer to a specific turn file by
    /// name regardless of the directory ordering.
    pub claude_turns_by_name: BTreeMap<String, ClaudeTurn>,
    pub expected: ExpectedStreams,
}

/// The four JSONL streams a fixture asserts on, each as a vector of
/// already-parsed `serde_json::Value`s. Missing files default to an
/// empty vector — fixtures that don't assert on, e.g., `delivered` can
/// simply omit the file.
#[derive(Debug, Clone, Default)]
pub struct ExpectedStreams {
    pub inbound_events: Vec<serde_json::Value>,
    pub messages_in: Vec<serde_json::Value>,
    pub messages_out: Vec<serde_json::Value>,
    pub delivered: Vec<serde_json::Value>,
}

impl Fixture {
    /// Load a fixture from a directory. The directory is expected to
    /// contain `manifest.json`, `central.sql`, an `inbound/` and a
    /// `claude/` subdirectory, and an `expected/` subdirectory.
    pub fn load(root: impl Into<PathBuf>) -> Result<Self> {
        let root: PathBuf = root.into();
        let manifest = load_manifest(&root.join("manifest.json"))?;
        let central_sql = fs::read_to_string(root.join("central.sql"))
            .with_context(|| format!("read central.sql from {}", root.display()))?;
        let inbound_sql_path = root.join("inbound.sql");
        let inbound_sql = if inbound_sql_path.exists() {
            fs::read_to_string(&inbound_sql_path)
                .with_context(|| format!("read inbound.sql from {}", inbound_sql_path.display()))?
        } else {
            String::new()
        };
        let inbound = load_inbound(&root.join("inbound"))?;
        let (claude_turns, claude_turns_by_name) = load_claude(&root.join("claude"))?;
        let expected = load_expected(&root.join("expected"))?;
        Ok(Self {
            root,
            manifest,
            central_sql,
            inbound_sql,
            inbound,
            claude_turns,
            claude_turns_by_name,
            expected,
        })
    }
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let bytes = fs::read(path)
        .with_context(|| format!("read manifest at {}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse manifest at {}", path.display()))?;
    Ok(manifest)
}

fn load_inbound(dir: &Path) -> Result<Vec<InboundEvent>> {
    let entries = sorted_files(dir, &["json"])?;
    let mut events = Vec::with_capacity(entries.len());
    for path in entries {
        let bytes = fs::read(&path)
            .with_context(|| format!("read inbound at {}", path.display()))?;
        let event: InboundEvent = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse inbound at {}", path.display()))?;
        events.push(event);
    }
    Ok(events)
}

fn load_claude(dir: &Path) -> Result<(Vec<ClaudeTurn>, BTreeMap<String, ClaudeTurn>)> {
    if !dir.exists() {
        return Ok((Vec::new(), BTreeMap::new()));
    }
    let entries = sorted_files(dir, &["json"])?;
    let mut turns = Vec::with_capacity(entries.len());
    let mut by_name: BTreeMap<String, ClaudeTurn> = BTreeMap::new();
    for path in entries {
        let bytes = fs::read(&path)
            .with_context(|| format!("read claude turn at {}", path.display()))?;
        let turn: ClaudeTurn = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse claude turn at {}", path.display()))?;
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            by_name.insert(name.to_string(), turn.clone());
        }
        turns.push(turn);
    }
    Ok((turns, by_name))
}

fn load_expected(dir: &Path) -> Result<ExpectedStreams> {
    Ok(ExpectedStreams {
        inbound_events: load_jsonl(&dir.join("inbound-events.jsonl"))?,
        messages_in: load_jsonl(&dir.join("messages-in.jsonl"))?,
        messages_out: load_jsonl(&dir.join("messages-out.jsonl"))?,
        delivered: load_jsonl(&dir.join("delivered.jsonl"))?,
    })
}

fn load_jsonl(path: &Path) -> Result<Vec<serde_json::Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("read jsonl at {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed).with_context(|| {
            format!("parse jsonl line {} in {}", i + 1, path.display())
        })?;
        out.push(v);
    }
    Ok(out)
}

fn sorted_files(dir: &Path, exts: &[&str]) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| exts.iter().any(|w| w.eq_ignore_ascii_case(e)));
        if ext_ok {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}
