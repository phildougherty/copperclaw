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
use copperclaw_types::InboundEvent;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Top-level fixture metadata loaded from `manifest.json`.
///
/// The `model_rich_*` / `trigger_sweep` / `runner_drain` flags are
/// independent, orthogonal opt-in toggles the harness reads directly off
/// the manifest — a plain deserialized config record, not a state machine.
/// Collapsing them into enums would only obscure the 1:1 JSON mapping.
#[allow(clippy::struct_excessive_bools)]
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
    /// - `"approvals"` — installs [`copperclaw_modules::ApprovalsModule`]
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
    /// Override the per-channel `max_message_chars` cap the harness's
    /// wrapped `MockAdapter` reports. Keys are channel-type strings
    /// (e.g. `"telegram"`); values are the cap in chars. Adapters
    /// without an entry fall back to the harness's built-in defaults
    /// (`telegram=4096`, `slack=40000`, `discord=2000`); other channel
    /// types report `None` (splitter disabled — matches the trait
    /// default). Used by the `*-long-message-split` fixtures to
    /// exercise the chat-text splitter in the delivery loop.
    #[serde(default)]
    pub adapter_caps: BTreeMap<String, usize>,
    /// Queue an adapter failure to fire on the next `deliver` call
    /// before the harness drives any inbound. Each entry maps to one
    /// `MockAdapter::fail_next_deliver` call on the named channel's
    /// adapter. Used by `rate-limited-retry` to script a `Rate { retry_after }`
    /// failure on the first delivery attempt.
    #[serde(default)]
    pub pre_delivery_failures: Vec<PreDeliveryFailure>,
    /// When `Some(ms)`, after the per-step delivery pass the harness
    /// sleeps the given milliseconds and then re-runs
    /// `DeliveryService::process_session_once` for the same session.
    /// Lets fixtures pin "row was deferred on first tick, delivered on
    /// the second tick after waiting `retry_after`" without poking at
    /// `DeliveryService`'s private retry state. Default `None`
    /// (one delivery pass per inbound, the legacy behaviour).
    #[serde(default)]
    pub redrive_after_ms: Option<u64>,
    /// When true, the per-step runner runs in DRAIN mode instead of the
    /// default `max_turns = Some(1)` mode: `run_loop` is raced against a
    /// watcher that polls the session's `messages_in` until no `pending`
    /// rows remain, then cancels the loop. Needed by the slash-command
    /// fixtures (`/clear`, `/compact`): the runner handles a pure
    /// slash-command batch synchronously and `continue`s WITHOUT
    /// counting a turn, so a `max_turns`-bounded loop would never
    /// return. Default false (the legacy one-turn behaviour).
    #[serde(default)]
    pub runner_drain: bool,
    /// Override the runner's per-inbound tool-turn depth cap
    /// (`RunnerDeps::max_tool_turns`, hardcoded to 5 in the harness's
    /// `run_one_turn` for every other fixture). A scripted turn
    /// sequence with more than 5 sequential tool rounds (e.g. the X1
    /// golden-path fixture: scaffold, verify, expose preview, ritual
    /// card) needs a higher cap or the runner would stop mid-sequence
    /// with no final text. `None` keeps the existing default of 5.
    #[serde(default)]
    pub max_tool_turns: Option<usize>,
    /// M22 Wave 0: override the runner's smart auto-continue HARD ceiling
    /// (`RunnerDeps::max_tool_turns_hard`) independently of the soft cap.
    /// The harness default keeps `hard == soft` (extension disabled, the
    /// historical flat-cap behaviour every existing fixture pins), so only
    /// a fixture that explicitly sets this — e.g.
    /// `telegram/budget-extension`'s `soft=2, hard=8` — exercises the
    /// progress-gated `Continue` path deterministically. `None` keeps
    /// `hard == max_tool_turns` (or the built-in 5).
    #[serde(default)]
    pub max_tool_turns_hard: Option<usize>,
    /// M22 Wave 0: set the runner's SOFT compaction target
    /// (`CompactionCfg::soft_target_tokens`, the replay twin of the
    /// production `COPPERCLAW_SOFT_COMPACTION_TARGET` env knob). The
    /// harness default keeps it `0` (soft trigger disabled — the hard
    /// window ceiling of ~188k estimated tokens is unreachable by any
    /// scripted fixture), so only a fixture that explicitly sets this —
    /// `telegram/auto-compaction` — trips the `should_compact` gate in
    /// `run_loop` and exercises the automatic compaction path.
    #[serde(default)]
    pub compaction_soft_target_tokens: Option<usize>,
    /// M19 F1: model the harness's wrapped `MockAdapter` as an
    /// edit-capable *rich* adapter for the Task HUD breadcrumb surface.
    ///
    /// The bare `MockAdapter` overrides neither `deliver_breadcrumb` nor
    /// the rich surfaces, so every HUD frame degrades to a fresh plain
    /// `deliver` (a re-post) — the exact new-message spam the M18 HUD and
    /// M19 F1 exist to kill. Real edit-capable adapters (telegram, slack,
    /// matrix, …) instead post the chip once and `editMessageText` it in
    /// place on every later frame. When this is `true` the harness's
    /// `CappedAdapter` faithfully models that contract: the first
    /// `deliver_breadcrumb` (no `existing_message_id`) is a post recorded
    /// as a `Breadcrumb`-kind delivery returning a stable anchor id, and
    /// every later frame (`existing_message_id = Some(anchor)`) is routed
    /// through the inner mock's `edit_message` so it lands in
    /// `MockAdapter::edits()` targeting that one anchor — proving the HUD
    /// edits one message in place rather than re-posting. Only F1's
    /// `matrix/hud-live-edit` fixture sets this; every other fixture keeps
    /// the byte-identical degrade-to-text behaviour. Default `false`.
    #[serde(default)]
    pub model_rich_breadcrumbs: bool,
    /// M19 U4/U5 (X-rider W2): model the harness's wrapped `MockAdapter`
    /// as a *rich* adapter for the [`Card`](copperclaw_channels_core::Card)
    /// surface.
    ///
    /// The bare `MockAdapter` overrides no rich surface, so the trait
    /// default `deliver_card` flattens every card to a plain
    /// `MessageKind::Chat` text row via `Card::to_text_fallback` — buttons
    /// become `- [Label] -> url` prose. Real card-capable adapters
    /// (Google Chat Cards v2, Matrix `formatted_body`, Slack Block Kit,
    /// deltachat/line's `render.rs`) instead render the card
    /// *structurally*, keeping its buttons as real actionable elements.
    /// When this is `true` the harness's `CappedAdapter` models that
    /// contract: `deliver_card` records a `MessageKind::Card`-kind
    /// delivery whose `content.card` preserves the full structured card
    /// (title / body / fields / buttons) instead of the flattened text —
    /// so `snapshot_delivered` shows a native card on the wire, not prose.
    /// The per-adapter *wire* rendering (gchat Cards-v2 JSON, matrix HTML,
    /// …) is proven by each adapter crate's own unit tests; this flag lets
    /// the pipeline fixture prove the host routes a card to `deliver_card`
    /// with its structure intact rather than degrading it host-side. Only
    /// the U4/U5 card parity fixtures set this. Default `false`.
    #[serde(default)]
    pub model_rich_cards: bool,
}

/// Script one `MockAdapter::fail_next_deliver` call. `kind` decides the
/// `AdapterError` variant:
///
/// - `"rate"` — `AdapterError::Rate { retry_after }` (in seconds; `None`
///   when omitted).
/// - `"transport"` — `AdapterError::Transport(message_or_default)`.
/// - `"bad_request"` — `AdapterError::BadRequest(message_or_default)`.
///
/// Failures are queued in FIFO order on the named channel's adapter,
/// matching the [`MockAdapter::fail_next_deliver`](
/// copperclaw_channels_core::testing::MockAdapter::fail_next_deliver)
/// contract.
#[derive(Debug, Clone, Deserialize)]
pub struct PreDeliveryFailure {
    pub channel: String,
    pub kind: String,
    #[serde(default)]
    pub retry_after: Option<u64>,
    #[serde(default)]
    pub message: Option<String>,
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
///
/// M21 S6: any entry may additionally set `advance_clock_ms` — when the
/// i-th scripted call is served, the harness advances its shared runner
/// [`TestClock`](copperclaw_runner::TestClock) by that many
/// milliseconds. This is the declarative hook for making time pass
/// *mid-turn* (inside one runner's tool loop), which is what the Task
/// HUD's 60s status-row cadence and 150s softening legs need; no real
/// wall-clock wait occurs.
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
    #[serde(default)]
    pub advance_clock_ms: Option<u64>,
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
    let bytes = fs::read(path).with_context(|| format!("read manifest at {}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse manifest at {}", path.display()))?;
    Ok(manifest)
}

fn load_inbound(dir: &Path) -> Result<Vec<InboundEvent>> {
    let entries = sorted_files(dir, &["json"])?;
    let mut events = Vec::with_capacity(entries.len());
    for path in entries {
        let bytes =
            fs::read(&path).with_context(|| format!("read inbound at {}", path.display()))?;
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
        let bytes =
            fs::read(&path).with_context(|| format!("read claude turn at {}", path.display()))?;
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
    let text =
        fs::read_to_string(path).with_context(|| format!("read jsonl at {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(trimmed)
            .with_context(|| format!("parse jsonl line {} in {}", i + 1, path.display()))?;
        out.push(v);
    }
    Ok(out)
}

fn sorted_files(dir: &Path, exts: &[&str]) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
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
