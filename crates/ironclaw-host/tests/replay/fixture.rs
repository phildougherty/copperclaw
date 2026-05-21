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
    /// Inbound `InboundEvent`s in file-name order.
    pub inbound: Vec<InboundEvent>,
    /// Claude turn responses in file-name order. Element `i` is served
    /// for the i-th call into the Anthropic mock.
    pub claude_turns: Vec<ClaudeTurn>,
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
        let inbound = load_inbound(&root.join("inbound"))?;
        let claude_turns = load_claude(&root.join("claude"))?;
        let expected = load_expected(&root.join("expected"))?;
        Ok(Self {
            root,
            manifest,
            central_sql,
            inbound,
            claude_turns,
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

fn load_claude(dir: &Path) -> Result<Vec<ClaudeTurn>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let entries = sorted_files(dir, &["json"])?;
    let mut turns = Vec::with_capacity(entries.len());
    for path in entries {
        let bytes = fs::read(&path)
            .with_context(|| format!("read claude turn at {}", path.display()))?;
        let turn: ClaudeTurn = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse claude turn at {}", path.display()))?;
        turns.push(turn);
    }
    Ok(turns)
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
