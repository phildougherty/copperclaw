//! Context compaction.
//!
//! When the input transcript approaches the model's input window the runner
//! asks the provider to summarise the oldest half of the conversation,
//! archives the pre-compaction transcript to `outbox/_compactions/<RFC3339>.md`,
//! and replaces the summarised slice with a synthetic
//! `HistoryMessage::User { content: "compact_boundary: <summary>" }` entry.
//!
//! The strategy is deliberately conservative — "summarise oldest half" is
//! the cheap, predictable shape we want for the first port. A future iter
//! may switch to a tokens-based slice; the call site only needs
//! [`compact`] so we can swap the implementation behind it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use copperclaw_providers::{AgentProvider, HistoryMessage, QueryInput};
use copperclaw_types::{Effort, ProviderEvent};

/// Default conservative window we plan against (Claude Sonnet 4.5 / 4.7).
pub const DEFAULT_INPUT_WINDOW: usize = 200_000;
/// Default safety margin in tokens before we trigger compaction. Sized to
/// absorb the static request overhead the naive `estimate_tokens` doesn't
/// count: the system prompt, tool-schema JSON, and the per-turn
/// `max_tokens` output reservation. Bumped from 8K → 16K after the
/// runner shipped a 124KB inlined-skills prompt that ate the old margin
/// in one bite. Operators on much-larger windows can shrink this back
/// down via `RunnerConfigFile.safety_margin_tokens`.
pub const DEFAULT_SAFETY_MARGIN: usize = 16_000;
/// Default per-turn output reservation. Compaction subtracts this from
/// the window in addition to `safety_margin_tokens` so the API doesn't
/// reject the request for `input + max_tokens > window`. Matches the
/// runner's default `max_tokens` so the two move together.
pub const DEFAULT_OUTPUT_RESERVE: usize = 4_096;
/// Default *soft* compaction target in tokens.
///
/// The hard ceiling (`model_input_window − safety_margin − output_reserve`,
/// ~180K on a 200K window) only ever fires to avoid a 400 from the
/// provider. The dominant cost on a long session isn't that one rejection
/// — it's replaying a steady ~30–60K transcript verbatim on *every* turn,
/// turn after turn. This much-lower target summarises the oldest half long
/// before the ceiling, capping the per-turn replay at roughly this size.
///
/// 40K is a deliberate middle ground: high enough that a normal multi-turn
/// task isn't summarised mid-thought, low enough that a runaway session
/// can't sit at 120K for a hundred turns. The hard ceiling stays in place
/// as the safety net (whichever threshold is lower triggers first). Set
/// `soft_target_tokens` to `0` to disable the soft trigger entirely and
/// recover the historical hard-window-only behaviour.
pub const DEFAULT_SOFT_TARGET: usize = 40_000;
/// Default *soft* compaction target for code-oriented tool profiles
/// ([`Coding`](crate::policy::ToolProfile::Coding) /
/// [`Full`](crate::policy::ToolProfile::Full)).
///
/// A long autonomous build (the whole point of the M18 program) needs its
/// mid-task detail — file layouts, error text, the running plan — kept in
/// context far longer than a chat session does, or the summarizer erases
/// the very working memory the build depends on. 80K on a 200K window
/// roughly doubles the pre-summarize working set while still leaving ~100K
/// of hard-ceiling headroom (`180K − 80K`), so a 90-minute build no longer
/// summarizes away what it's mid-way through. Messaging / Minimal profiles
/// keep [`DEFAULT_SOFT_TARGET`] — the raise is deliberately profile-scoped.
/// The hard ceiling still clamps this via
/// [`CompactionCfg::effective_threshold`], so a misconfigured huge target
/// can never push the trigger past the safety net.
pub const DEFAULT_SOFT_TARGET_CODING: usize = 80_000;

/// Resolve the default soft compaction target for a tool profile. Code-
/// oriented profiles get the higher [`DEFAULT_SOFT_TARGET_CODING`] so long
/// builds retain mid-task detail; everything else keeps [`DEFAULT_SOFT_TARGET`].
///
/// This is only the *default* — an explicit `soft_compaction_target_tokens`
/// in the runner config (or the `COPPERCLAW_SOFT_COMPACTION_TARGET` env var)
/// still overrides it, and the hard ceiling always clamps the result.
#[must_use]
pub fn default_soft_target_for_profile(profile: crate::policy::ToolProfile) -> usize {
    use crate::policy::ToolProfile;
    match profile {
        ToolProfile::Coding | ToolProfile::Full => DEFAULT_SOFT_TARGET_CODING,
        ToolProfile::Minimal | ToolProfile::Messaging => DEFAULT_SOFT_TARGET,
    }
}

/// Average characters per token used by [`estimate_tokens`].
///
/// ## Why an approximation instead of `tiktoken-rs`
///
/// `tiktoken-rs`'s `cl100k_base` is *GPT's* tokenizer, not Claude's — so it
/// is itself only an approximation of the number we actually plan against
/// (the Claude token count the provider windows and bills on), while adding
/// ~1.5 MB of embedded BPE vocab plus a `fancy-regex` dependency to every
/// in-container runner binary and measurable cold-compile time. Because this
/// estimate only drives the *compaction trigger* — backstopped by a 16K
/// safety margin, a 4K output reserve, and the hard-ceiling clamp — sub-
/// percent tokenizer precision buys nothing that the margins don't already
/// cover. A calibrated character ratio is well within the error budget and
/// costs no bytes, no crate, no compile time.
///
/// ## Error bounds
///
/// Anthropic's published guidance puts Claude's tokenizer at roughly **3.5
/// characters per token for English prose**, running denser (~3.0) on source
/// code and JSON. The previous flat `chars / 4` therefore *under*-counted
/// real usage by ~12% on prose and ~25% on code — on a long code-dense build
/// the true context could sail past the window while the estimate still read
/// "safe". We divide by `3.5` (the conservative prose figure) over a
/// *whitespace-collapsed* character count (see [`ws_collapsed_len`]): runs of
/// ASCII whitespace — the indentation and blank lines that dominate code and
/// pretty-printed JSON, and the single biggest source of the old estimate's
/// drift on code-dense transcripts — collapse to one char first, because the
/// tokenizer merges them too. Net effect versus the real `count_tokens` API
/// on mixed build transcripts: within roughly ±10%, biased to *over*-count,
/// so compaction fires a touch early rather than overflowing the window.
///
/// The `3.5` ratio is applied as the exact rational `TOKEN_NUM / TOKEN_DEN`
/// (= 2/7) so [`estimate_tokens`] can ceil-divide in integer math — no float
/// cast (and no pedantic-clippy precision-loss lint) for a ratio that is an
/// approximation to begin with.
const TOKEN_NUM: usize = 2;
const TOKEN_DEN: usize = 7;

/// Marker prefixing the pinned project-facts header (see
/// [`build_project_facts_header`]). A compacted history carries exactly one
/// such entry, always at the front, regenerated verbatim from on-disk R3
/// state on every compaction rather than handed to the summarizer.
pub const PROJECT_FACTS_MARKER: &str = "project_facts:";

/// System prompt the runner sends to the provider when asking for a summary.
pub const SUMMARY_SYSTEM_PROMPT: &str = "Summarize the following conversation succinctly. Preserve any decisions, \
open questions, identifiers, and unresolved tool requests. Be terse.";

/// Tuning knobs for compaction. Defaults match the constants above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionCfg {
    /// Total input window of the target model in tokens.
    pub model_input_window: usize,
    /// How much headroom to keep below the window. Compaction fires when the
    /// estimated transcript size exceeds `window - margin - output_reserve`.
    pub safety_margin_tokens: usize,
    /// Tokens reserved for the model's response (matches the runner's
    /// per-turn `max_tokens`). Subtracted from the window so the request
    /// never violates `input + max_tokens > window` — the failure mode
    /// that surfaced as a hard 400 on Haiku 4.5 with a long transcript.
    pub output_reserve_tokens: usize,
    /// Soft compaction target in tokens. When non-zero, compaction fires
    /// once the estimate exceeds this value — well below the hard
    /// ceiling — so a long session doesn't replay a large transcript
    /// every turn. `0` disables the soft trigger and falls back to the
    /// hard-window-only rule. See [`DEFAULT_SOFT_TARGET`].
    pub soft_target_tokens: usize,
    /// Provider model name to use for the summarisation turn.
    pub summary_model: String,
    /// Effort hint for the summarisation turn.
    pub summary_effort: Effort,
    /// Max output tokens for the summarisation turn.
    pub summary_max_tokens: u32,
    /// Where to archive pre-compaction transcripts. Typically
    /// `<session_dir>/outbox/_compactions`.
    pub archive_dir: PathBuf,
    /// Container data root that projects and the todo store live under —
    /// `/data` in production (the same root `todo.rs` and `verify_gate.rs`
    /// hardcode). Source for the pinned project-facts header
    /// ([`build_project_facts_header`]); test-overridable so the header can
    /// be exercised against a temp dir.
    pub data_root: PathBuf,
}

impl CompactionCfg {
    /// The hard ceiling, in tokens: `model_input_window` less the
    /// `safety_margin_tokens` (static request overhead the estimator
    /// doesn't count — system prompt, tool schemas, formatting) and the
    /// `output_reserve_tokens` (the per-turn output budget the provider
    /// enforces as part of the total window). Crossing this risks a hard
    /// 400 from the provider, so it is the non-negotiable safety net.
    #[must_use]
    pub fn hard_threshold(&self) -> usize {
        self.model_input_window
            .saturating_sub(self.safety_margin_tokens)
            .saturating_sub(self.output_reserve_tokens)
    }

    /// The effective threshold compaction triggers at: the lower of the
    /// soft target and the hard ceiling. A non-zero `soft_target_tokens`
    /// pulls the trigger far below the window so a long session stops
    /// replaying a large transcript every turn; `0` disables the soft
    /// trigger and leaves only the hard ceiling. The hard ceiling always
    /// caps the result, so a misconfigured soft target larger than the
    /// window can never push the trigger past the safety net.
    #[must_use]
    pub fn effective_threshold(&self) -> usize {
        let hard = self.hard_threshold();
        if self.soft_target_tokens == 0 {
            hard
        } else {
            self.soft_target_tokens.min(hard)
        }
    }

    /// True iff `estimated_tokens` has crossed the effective threshold
    /// (the lower of the soft target and the hard ceiling).
    #[must_use]
    pub fn should_compact(&self, estimated_tokens: usize) -> bool {
        estimated_tokens > self.effective_threshold()
    }
}

/// Estimate the token count of `messages` with a calibrated character
/// heuristic (see [`TOKEN_NUM`] / [`TOKEN_DEN`] for the estimator choice and
/// its error bounds). Counts the textual payload of each [`HistoryMessage`]
/// variant over a whitespace-collapsed character count; tool-use input JSON
/// is rendered as a compact string for sizing purposes.
#[must_use]
pub fn estimate_tokens(messages: &[HistoryMessage]) -> usize {
    let mut chars: usize = 0;
    for m in messages {
        chars += chars_of(m);
    }
    // tokens = ceil(chars / 3.5) = ceil(chars * 2 / 7), integer math so a
    // short-but-nonempty transcript never rounds down to zero tokens.
    (chars * TOKEN_NUM).div_ceil(TOKEN_DEN)
}

/// Count the characters of `s`, treating each maximal run of ASCII
/// whitespace as a single character. Indentation, blank lines, and the
/// padding that dominates source code and pretty-printed JSON collapse the
/// way the tokenizer merges them, so a code-dense transcript no longer
/// inflates the estimate — and trips compaction early — purely on
/// whitespace it will never spend one token per character on.
fn ws_collapsed_len(s: &str) -> usize {
    let mut count = 0usize;
    let mut prev_ws = false;
    for c in s.chars() {
        let is_ws = c.is_ascii_whitespace();
        if is_ws && prev_ws {
            continue;
        }
        count += 1;
        prev_ws = is_ws;
    }
    count
}

fn chars_of(m: &HistoryMessage) -> usize {
    match m {
        HistoryMessage::User { content } | HistoryMessage::Assistant { content } => {
            ws_collapsed_len(content)
        }
        HistoryMessage::ToolUse { id, name, input } => {
            ws_collapsed_len(id) + ws_collapsed_len(name) + ws_collapsed_len(&input.to_string())
        }
        HistoryMessage::Tool {
            tool_use_id,
            content,
            is_error: _,
        } => ws_collapsed_len(tool_use_id) + ws_collapsed_len(content),
        HistoryMessage::Image {
            media_type,
            data: _,
        } => {
            // An image's token cost is tile-based, not its base64 length —
            // counting `data` would massively overestimate and trigger
            // needless compaction. Contribute a flat ~1500-token estimate:
            // 1500 tokens × CHARS_PER_TOKEN (3.5) ≈ 5250 "chars".
            ws_collapsed_len(media_type) + 5_250
        }
    }
}

/// Choose a split point near `len/2` that never bisects a `tool_use` /
/// `tool_result` group.
///
/// The slice before the pivot (`oldest`) is sent to the provider to be
/// summarised. A slice ending on a dangling `ToolUse` — or a `newest`
/// slice starting on an orphan `Tool` result — is rejected by strict
/// providers (minimax: "tool call and result not match"), which fails
/// compaction and crash-loops the runner. Advance the midpoint forward
/// past any straddled tool group so both halves are self-contained.
fn pair_safe_pivot(history: &[HistoryMessage]) -> usize {
    let mut pivot = history.len() / 2;
    while pivot < history.len()
        && (matches!(history[pivot], HistoryMessage::Tool { .. })
            || pivot
                .checked_sub(1)
                .is_some_and(|i| matches!(history[i], HistoryMessage::ToolUse { .. })))
    {
        pivot += 1;
    }
    pivot
}

/// True iff `m` is a pinned project-facts header (see
/// [`build_project_facts_header`]) emitted by a previous compaction.
fn is_pinned_facts_header(m: &HistoryMessage) -> bool {
    matches!(m, HistoryMessage::User { content } if content.starts_with(PROJECT_FACTS_MARKER))
}

/// Replace the oldest half of `history` with a single summarised user-side
/// `compact_boundary` entry, and re-pin a fresh project-facts header at the
/// front. Writes the pre-compaction transcript to
/// `cfg.archive_dir/<RFC3339>.md` as a side effect.
///
/// The project-facts header (project path, verify command, branch, plan) is
/// sourced fresh from on-disk R3 state on every compaction and pinned
/// **verbatim** — never handed to the summarizer to paraphrase — so those
/// facts survive summarization losslessly no matter how many times a long
/// build compacts. Any header pinned by a prior compaction is stripped first
/// so exactly one, always-current copy is carried forward.
///
/// If `history.len() < 4` the function is a no-op and returns the input
/// unchanged — there isn't enough material to summarise meaningfully.
pub async fn compact(
    history: Vec<HistoryMessage>,
    provider: &dyn AgentProvider,
    cfg: &CompactionCfg,
) -> Result<Vec<HistoryMessage>> {
    if history.len() < 4 {
        return Ok(history);
    }
    // Strip any facts header pinned by a previous compaction; a fresh,
    // current one is prepended below so headers never accumulate.
    let mut history = history;
    history.retain(|m| !is_pinned_facts_header(m));

    // Regenerate the pinned header from current on-disk state. Verbatim,
    // never summarised — this is the whole point of the header.
    let facts = build_project_facts_header(&cfg.data_root).await;
    if let Some(header) = &facts {
        // R4: byte size of the verbatim project facts header carried across a
        // compaction (absent for a pure-chat session).
        copperclaw_metrics::observe_compaction_facts_header_bytes(header.len());
    }

    let pivot = pair_safe_pivot(&history);
    if pivot == 0 || pivot >= history.len() {
        // The whole transcript is one unsplittable tool group (rare). Leave
        // it intact rather than send a half-pair to the provider — but still
        // re-pin the facts header so they survive even a no-op compaction.
        return Ok(prepend_facts(facts, history));
    }
    let oldest = history[..pivot].to_vec();
    let newest = history[pivot..].to_vec();

    write_archive(&cfg.archive_dir, &history)
        .with_context(|| format!("archive transcript to {}", cfg.archive_dir.display()))?;

    let summary = summarise(provider, cfg, oldest).await?;

    let mut out = Vec::with_capacity(newest.len() + 2);
    if let Some(header) = facts {
        out.push(HistoryMessage::User { content: header });
    }
    out.push(HistoryMessage::User {
        content: format!("compact_boundary: {summary}"),
    });
    out.extend(newest);
    Ok(out)
}

/// Prepend the pinned facts header (if any) to `history`. Used on the
/// unsplittable-transcript path where there's nothing to summarise but the
/// facts must still be re-pinned.
fn prepend_facts(facts: Option<String>, history: Vec<HistoryMessage>) -> Vec<HistoryMessage> {
    match facts {
        Some(header) => {
            let mut out = Vec::with_capacity(history.len() + 1);
            out.push(HistoryMessage::User { content: header });
            out.extend(history);
            out
        }
        None => history,
    }
}

/// Assemble the pinned **project-facts header** from on-disk state under
/// `data_root`: the R3 verify-gate markers (verify stages, M20 Q2) plus git
/// branch, a generated file inventory (M20 Q8a), the `DECISIONS.md` tail
/// (M20 Q8c), and the agent todo list (the running plan). Returns `None`
/// when there are no projects and no todos — a pure-chat session gets no
/// header, byte-identical to pre-R4 behaviour.
///
/// Deterministic and read-only: called fresh on every compaction, it always
/// renders the same bytes for the same on-disk state, which is what lets the
/// header round-trip verbatim through repeated compactions. Every
/// per-project section below is best-effort — a missing git repo, a
/// missing/unreadable `DECISIONS.md`, or a missing `.copperclaw/verify`
/// pins nothing for that section rather than aborting the whole header.
async fn build_project_facts_header(data_root: &Path) -> Option<String> {
    let mut body = String::new();

    let projects = scan_projects(data_root).await;
    for proj in &projects {
        let name = proj
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<project>");
        body.push_str("- ");
        body.push_str(name);
        if let Some(branch) = git_branch(proj).await {
            body.push_str(" (branch: ");
            body.push_str(&branch);
            body.push(')');
        }
        body.push('\n');
        push_verify_stages(&mut body, proj).await;
        push_file_inventory(&mut body, proj).await;
        push_decisions_tail(&mut body, proj).await;
    }

    let todos = read_todos(data_root).await;
    if !todos.is_empty() {
        body.push_str("plan / key decisions (todo list):\n");
        for t in &todos {
            body.push_str(t);
            body.push('\n');
        }
    }

    if body.is_empty() {
        return None;
    }
    // A trailing newline would make the header sensitive to formatting drift
    // across renders; trim it so the pinned bytes are stable.
    let trimmed = body.trim_end();
    Some(format!(
        "{PROJECT_FACTS_MARKER}\n(pinned verbatim through compaction — do not \
         summarise or drop)\n{trimmed}"
    ))
}

/// Per-project state directory name, mirroring
/// `copperclaw_mcp::tools::verify_gate`'s private `STATE_DIR_NAME` — kept as
/// a local constant here since this crate only needs it for one path join
/// ([`push_decisions_tail`]) and the verify-gate module doesn't export it.
const STATE_DIR_NAME: &str = ".copperclaw";
/// M20 Q8c: the decision-log convention file, agent-appended, one line per
/// decision. No new tool — written with ordinary edit tools; see
/// [`push_decisions_tail`]. Writes here are exempted from the R3 verify
/// gate's dirty-marking (`verify_gate::mark_dirty_for_write`'s
/// `.copperclaw/`-subtree exemption), so a decision-log append never
/// invalidates an already-green verify.
const DECISIONS_FILE_NAME: &str = "DECISIONS.md";

/// Cap on the number of verify stages pinned per project (M20 Q8b). Purely
/// a backstop against a pathological `.copperclaw/verify` — a real
/// multi-stage file (Q2) is a handful of lines at most.
const MAX_VERIFY_STAGES_PINNED: usize = 20;

/// Cap on the number of file paths pinned per project's generated file
/// inventory (M20 Q8a). `git ls-files` already excludes ignored build
/// artifacts (`node_modules/`, `target/`, `dist/`), so a healthy
/// prototype's tracked-file count is usually well under this; the cap is a
/// backstop against a pathological project (a vendored dependency
/// accidentally committed) blowing the pinned header past its soft-target
/// budget.
const MAX_INVENTORY_FILES: usize = 200;

/// Cap on the number of trailing `DECISIONS.md` lines pinned per project
/// (M20 Q8c). Only the tail survives a long build's repeated compactions —
/// older decisions stay on disk in the file itself, just not re-pinned
/// every round.
const MAX_DECISIONS_LINES: usize = 30;

/// Pin `proj`'s verify stages (M20 Q2's multi-line `.copperclaw/verify`,
/// each line an independent stage with an optional `name:` prefix)
/// **verbatim**, mirroring exactly how a single legacy verify command was
/// pinned pre-Q8: a project with exactly one stage (the pre-Q2 shape, or a
/// post-Q2 file that still has only one line) renders the identical
/// `  verify: <command>` line byte-for-byte. Two or more stages each get
/// their own `    <name>: <command>` line under a `  verify:` heading, so
/// every stage — not just the first — survives compaction. Capped at
/// [`MAX_VERIFY_STAGES_PINNED`]. Best-effort: a missing/unreadable
/// `.copperclaw/verify` pins `(none recorded)`, exactly as before Q8.
async fn push_verify_stages(body: &mut String, proj: &Path) {
    let stages = copperclaw_mcp::tools::verify_gate::recorded_stages(proj, None).await;
    if stages.is_empty() {
        body.push_str("  verify: (none recorded)\n");
        return;
    }
    if stages.len() == 1 {
        body.push_str("  verify: ");
        body.push_str(&stages[0].command);
        body.push('\n');
        return;
    }
    let total = stages.len();
    let capped: Vec<_> = stages.into_iter().take(MAX_VERIFY_STAGES_PINNED).collect();
    body.push_str("  verify:\n");
    for s in &capped {
        body.push_str("    ");
        body.push_str(&s.name);
        body.push_str(": ");
        body.push_str(&s.command);
        body.push('\n');
    }
    if total > capped.len() {
        body.push_str(&format!(
            "    … ({} more stages omitted)\n",
            total - capped.len()
        ));
    }
}

/// Pin a capped, generated file inventory for `proj` (M20 Q8a), sourced
/// from `git ls-files` — tracked files only, so build artifacts and
/// dependency directories never appear (they're git-ignored). Nothing is
/// pinned when the project has no git repo, `git` isn't on `PATH`, or the
/// command fails for any reason — best-effort, never abort compaction.
/// Capped at [`MAX_INVENTORY_FILES`] paths.
async fn push_file_inventory(body: &mut String, proj: &Path) {
    let files = git_ls_files(proj).await;
    if files.is_empty() {
        return;
    }
    let total = files.len();
    let capped = &files[..total.min(MAX_INVENTORY_FILES)];
    body.push_str("  files (");
    body.push_str(&total.to_string());
    body.push_str("):\n");
    for f in capped {
        body.push_str("    ");
        body.push_str(f);
        body.push('\n');
    }
    if total > capped.len() {
        body.push_str(&format!(
            "    … ({} more files omitted)\n",
            total - capped.len()
        ));
    }
}

/// Run `git -C <project_root> ls-files` and return its stdout lines.
/// Best-effort: a missing binary, a missing/non-git directory, or a
/// non-zero exit all yield an empty list rather than an error.
async fn git_ls_files(project_root: &Path) -> Vec<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .arg("ls-files")
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Pin the tail of `<proj>/.copperclaw/DECISIONS.md` (M20 Q8c): a new
/// lightweight, tool-free convention where the agent appends one line per
/// decision ("chose X over Y because Z") with ordinary edit tools. Only
/// the last [`MAX_DECISIONS_LINES`] non-empty lines are pinned — the file
/// itself keeps the full history on disk. Best-effort: a missing or
/// unreadable file (or one with no decisions yet) pins nothing, so a
/// project with no `DECISIONS.md` compacts exactly as it did before Q8
/// except for the new inventory/stage sections.
async fn push_decisions_tail(body: &mut String, proj: &Path) {
    let path = proj.join(STATE_DIR_NAME).join(DECISIONS_FILE_NAME);
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return;
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return;
    }
    let tail_start = lines.len().saturating_sub(MAX_DECISIONS_LINES);
    let tail = &lines[tail_start..];
    body.push_str("  decisions (last ");
    body.push_str(&tail.len().to_string());
    body.push_str("):\n");
    for d in tail {
        body.push_str("    - ");
        body.push_str(d);
        body.push('\n');
    }
}

/// First-level directories under `data_root` that look like a project: they
/// carry a `.git` or a `.copperclaw` marker dir. Sorted for a deterministic
/// header. Best-effort — an unreadable root yields an empty list.
async fn scan_projects(data_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(data_root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(".git").exists() || path.join(".copperclaw").exists() {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Read `<project>/.git/HEAD` and return the current branch name, or `None`
/// for a detached HEAD or a missing/unreadable file. Reads the ref file
/// directly rather than spawning `git` — cheap, non-blocking, no subprocess.
async fn git_branch(project_root: &Path) -> Option<String> {
    let head = tokio::fs::read_to_string(project_root.join(".git").join("HEAD"))
        .await
        .ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/").map(str::to_string)
}

/// Read the agent todo store (`<data_root>/agent_todos.json`) and render each
/// item as one pinned line. Parsed as loose JSON so this stays decoupled from
/// `copperclaw-mcp`'s private `TodoItem` type; absent / unparseable store
/// yields an empty list.
async fn read_todos(data_root: &Path) -> Vec<String> {
    let path = data_root.join("agent_todos.json");
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return Vec::new();
    };
    let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|it| {
            let id = it.get("id").and_then(serde_json::Value::as_u64);
            let text = it
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let status = it
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending");
            let glyph = match status {
                "completed" => "[x]",
                "in_progress" => "[~]",
                "blocked" => "[!]",
                _ => "[ ]",
            };
            let blocked = it
                .get("blocked_reason")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| format!("  (blocked: {s})"))
                .unwrap_or_default();
            match id {
                Some(n) => format!("  {glyph} {n}. {text}{blocked}"),
                None => format!("  {glyph} {text}{blocked}"),
            }
        })
        .collect()
}

/// Drive one summarisation turn against the provider, collecting the final
/// [`ProviderEvent::Result`] text into a string.
async fn summarise(
    provider: &dyn AgentProvider,
    cfg: &CompactionCfg,
    oldest: Vec<HistoryMessage>,
) -> Result<String> {
    let input = QueryInput {
        system: SUMMARY_SYSTEM_PROMPT.into(),
        system_context: None,
        model: cfg.summary_model.clone(),
        effort: cfg.summary_effort,
        previous_continuation: None,
        history: oldest,
        tools: Vec::new(),
        max_tokens: cfg.summary_max_tokens,
        temperature: Some(0.0),
        assistant_name: None,
        display_name: None,
    };
    let mut query = provider
        .query(input)
        .await
        .context("summarisation provider query failed")?;
    let mut summary = String::new();
    while let Some(event) = query.next_event().await {
        match event {
            ProviderEvent::Result { text } => {
                if let Some(t) = text {
                    summary.push_str(&t);
                }
                break;
            }
            ProviderEvent::Error { message, .. } => {
                anyhow::bail!("summarisation error from provider: {message}");
            }
            _ => continue,
        }
    }
    if summary.trim().is_empty() {
        anyhow::bail!("provider returned an empty summary");
    }
    Ok(summary)
}

fn write_archive(dir: &Path, history: &[HistoryMessage]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = Utc::now().to_rfc3339();
    let safe = stamp.replace(':', "-");
    let target = dir.join(format!("{safe}.md"));
    let mut body = String::new();
    body.push_str("# Compaction archive\n\n");
    body.push_str("Archived at ");
    body.push_str(&stamp);
    body.push_str("\n\n");
    for (i, m) in history.iter().enumerate() {
        body.push_str(&format!("## {i}. "));
        match m {
            HistoryMessage::User { content } => {
                body.push_str("user\n\n");
                body.push_str(content);
            }
            HistoryMessage::Assistant { content } => {
                body.push_str("assistant\n\n");
                body.push_str(content);
            }
            HistoryMessage::ToolUse { id, name, input } => {
                body.push_str("tool_use\n\n");
                body.push_str(&format!("id: {id}\nname: {name}\ninput: {input}"));
            }
            HistoryMessage::Tool {
                tool_use_id,
                content,
                is_error,
            } => {
                body.push_str("tool_result\n\n");
                body.push_str(&format!(
                    "tool_use_id: {tool_use_id}\nis_error: {is_error}\n\n{content}"
                ));
            }
            HistoryMessage::Image { media_type, data } => {
                // Record presence + size only; the base64 payload would
                // bloat the archive for no human benefit.
                body.push_str("image\n\n");
                body.push_str(&format!(
                    "media_type: {media_type}\nbase64_bytes: {}",
                    data.len()
                ));
            }
        }
        body.push_str("\n\n");
    }
    std::fs::write(&target, body)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use copperclaw_providers::{AgentProvider, AgentQuery, ProviderError};
    use std::sync::Mutex;

    fn cfg_with_dir(dir: PathBuf) -> CompactionCfg {
        CompactionCfg {
            model_input_window: 200_000,
            safety_margin_tokens: 16_000,
            output_reserve_tokens: 4_096,
            // 0 = soft trigger disabled, so the existing hard-ceiling
            // assertions below are unaffected. The soft-trigger tests
            // set this explicitly.
            soft_target_tokens: 0,
            summary_model: "claude-sonnet-4-6".into(),
            summary_effort: Effort::Low,
            summary_max_tokens: 1024,
            // Empty by default: no projects, no todo store, so
            // `build_project_facts_header` returns `None` and the existing
            // compaction assertions (which don't expect a header) hold. The
            // facts-header tests point this at a populated temp dir.
            data_root: dir.clone(),
            archive_dir: dir,
        }
    }

    #[test]
    fn should_compact_threshold() {
        // 200_000 - 16_000 (margin) - 4_096 (output reserve) = 179_904
        let cfg = cfg_with_dir(PathBuf::from("/tmp/x"));
        assert!(!cfg.should_compact(100));
        assert!(!cfg.should_compact(179_904));
        assert!(cfg.should_compact(179_905));
        assert!(cfg.should_compact(1_000_000));
    }

    fn tu(id: &str) -> HistoryMessage {
        HistoryMessage::ToolUse {
            id: id.into(),
            name: "t".into(),
            input: serde_json::json!({}),
        }
    }
    fn tr(id: &str) -> HistoryMessage {
        HistoryMessage::Tool {
            tool_use_id: id.into(),
            content: "ok".into(),
            is_error: false,
        }
    }
    fn txt() -> HistoryMessage {
        HistoryMessage::User {
            content: "x".into(),
        }
    }

    #[test]
    fn pivot_does_not_split_a_tool_pair() {
        // Naive len/2 pivot (3) lands on the Tool result of the pair at 2..4.
        let h = vec![txt(), txt(), tu("a"), tr("a"), txt(), txt()];
        let p = pair_safe_pivot(&h);
        assert!(p >= 4, "pivot {p} must clear the tool group");
        // oldest must not end on a dangling ToolUse...
        assert!(!matches!(h[p - 1], HistoryMessage::ToolUse { .. }));
        // ...and newest must not start on an orphan Tool result.
        assert!(p >= h.len() || !matches!(h[p], HistoryMessage::Tool { .. }));
    }

    #[test]
    fn pivot_clears_a_straddled_parallel_tool_group() {
        // Parallel batch [tu a, tu b, tr a, tr b] straddles the naive
        // midpoint (4). The pivot must advance to the end of the group (6).
        let h = vec![
            txt(),
            txt(),
            tu("a"),
            tu("b"),
            tr("a"),
            tr("b"),
            txt(),
            txt(),
        ];
        let p = pair_safe_pivot(&h); // naive pivot = 4 (history[4] = tr a)
        assert_eq!(p, 6);
        assert!(!matches!(h[p - 1], HistoryMessage::ToolUse { .. }));
        assert!(!matches!(h[p], HistoryMessage::Tool { .. }));
    }

    #[test]
    fn pivot_is_plain_midpoint_when_no_pair_straddles() {
        let h = vec![txt(), txt(), txt(), txt()];
        assert_eq!(pair_safe_pivot(&h), 2);
    }

    #[test]
    fn should_compact_when_margin_exceeds_window() {
        let cfg = CompactionCfg {
            model_input_window: 100,
            safety_margin_tokens: 500,
            output_reserve_tokens: 0,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        // saturating_sub keeps threshold at 0; any positive estimate compacts.
        assert!(cfg.should_compact(1));
    }

    #[test]
    fn should_compact_accounts_for_output_reserve() {
        // Regression for the live Haiku-4.5 200K-window overflow: with
        // an 8K safety margin and a 4K output reserve, the model rejects
        // requests where `input + max_tokens > window`. The threshold
        // must subtract BOTH so the API never sees that combination.
        let cfg = CompactionCfg {
            model_input_window: 200_000,
            safety_margin_tokens: 8_000,
            output_reserve_tokens: 4_096,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        // 200_000 - 8_000 - 4_096 = 187_904
        assert!(!cfg.should_compact(187_904));
        assert!(cfg.should_compact(187_905));
        // The pre-fix bug: estimated_tokens=195_000 would NOT have
        // triggered compaction under the old `input - margin` rule
        // (195_000 < 192_000 was false — but `195_000 > 192_000` was
        // true, so OLD code did compact at this point. The new failure
        // path was around `input=190_000` with `max_tokens=4_096`:
        // old rule: 190K < 192K → no compact → 194K total → fail.
        // New rule: 190K > 187_904 → compact → safe.
        assert!(cfg.should_compact(190_000));
    }

    #[test]
    fn soft_target_fires_far_below_hard_ceiling() {
        // The whole point: on a 200K window with a 40K soft target, a
        // 50K transcript compacts immediately instead of riding to ~180K.
        let cfg = CompactionCfg {
            soft_target_tokens: 40_000,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        assert_eq!(cfg.effective_threshold(), 40_000);
        assert!(!cfg.should_compact(40_000));
        assert!(cfg.should_compact(40_001));
        assert!(cfg.should_compact(50_000));
        // ...and well below the hard ceiling (179_904).
        assert!(cfg.should_compact(100_000));
    }

    #[test]
    fn soft_target_zero_falls_back_to_hard_ceiling() {
        // 0 disables the soft trigger: behaviour is identical to the
        // historical hard-window-only rule (179_904 on this config).
        let cfg = cfg_with_dir(PathBuf::from("/tmp")); // soft_target_tokens = 0
        assert_eq!(cfg.effective_threshold(), 179_904);
        assert!(!cfg.should_compact(50_000));
        assert!(!cfg.should_compact(179_904));
        assert!(cfg.should_compact(179_905));
    }

    #[test]
    fn hard_ceiling_caps_an_oversized_soft_target() {
        // A misconfigured soft target larger than the window must never
        // push the trigger past the safety net — the hard ceiling wins.
        let cfg = CompactionCfg {
            soft_target_tokens: 10_000_000,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        assert_eq!(cfg.effective_threshold(), cfg.hard_threshold());
        assert_eq!(cfg.effective_threshold(), 179_904);
    }

    #[test]
    fn default_soft_target_is_well_below_default_window() {
        const _: () = assert!(DEFAULT_SOFT_TARGET < DEFAULT_INPUT_WINDOW);
        // A long synthetic transcript estimated near a typical steady
        // context (~50K tokens) must trip the default soft target.
        let cfg = CompactionCfg {
            soft_target_tokens: DEFAULT_SOFT_TARGET,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        // ~50K tokens ≈ 200K chars across many user turns.
        let history: Vec<HistoryMessage> = (0..1_000)
            .map(|i| HistoryMessage::User {
                content: format!("turn {i}: ").repeat(20),
            })
            .collect();
        let est = estimate_tokens(&history);
        assert!(
            est > DEFAULT_SOFT_TARGET,
            "synthetic transcript ({est} tok) should exceed the soft target"
        );
        assert!(cfg.should_compact(est));
        // The same transcript would NOT have triggered the hard-ceiling
        // rule, proving the soft target is what saves the per-turn replay.
        let hard_only = CompactionCfg {
            soft_target_tokens: 0,
            ..cfg
        };
        assert!(!hard_only.should_compact(est));
    }

    #[test]
    fn estimate_tokens_for_simple_text() {
        let h = vec![HistoryMessage::User {
            content: "a".repeat(8),
        }];
        // 8 chars / 3.5 chars-per-token, rounded up.
        assert_eq!(estimate_tokens(&h), 3);
    }

    #[test]
    fn estimate_tokens_handles_all_variants() {
        let h = vec![
            HistoryMessage::User {
                content: "abcd".into(),
            },
            HistoryMessage::Assistant {
                content: "wxyz".into(),
            },
            HistoryMessage::ToolUse {
                id: "tu_1".into(),
                name: "tool".into(),
                input: serde_json::json!({"k": "v"}),
            },
            HistoryMessage::Tool {
                tool_use_id: "tu_1".into(),
                content: "result".into(),
                is_error: false,
            },
        ];
        // Just sanity-check that we got a positive number and didn't panic.
        assert!(estimate_tokens(&h) > 0);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    /// Provider stub that returns a canned summary text.
    struct StubProvider {
        canned_summary: String,
    }

    #[async_trait]
    impl AgentProvider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            Ok(Box::new(StubQuery {
                events: Mutex::new(vec![
                    ProviderEvent::Init {
                        continuation: "c1".into(),
                    },
                    ProviderEvent::Result {
                        text: Some(self.canned_summary.clone()),
                    },
                ]),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    /// Provider stub that returns an Error event.
    struct ErrorProvider;

    #[async_trait]
    impl AgentProvider for ErrorProvider {
        fn name(&self) -> &'static str {
            "err"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            Ok(Box::new(StubQuery {
                events: Mutex::new(vec![ProviderEvent::Error {
                    message: "synthetic".into(),
                    retryable: false,
                }]),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct StubQuery {
        events: Mutex<Vec<ProviderEvent>>,
    }

    #[async_trait]
    impl AgentQuery for StubQuery {
        async fn push(&mut self, _message: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            let mut g = self.events.lock().unwrap();
            if g.is_empty() {
                None
            } else {
                Some(g.remove(0))
            }
        }
        async fn abort(&mut self) {}
    }

    fn long_history(n: usize) -> Vec<HistoryMessage> {
        (0..n)
            .map(|i| HistoryMessage::User {
                content: format!("msg-{i}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn compact_replaces_oldest_half_with_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = StubProvider {
            canned_summary: "SUMMARY".into(),
        };
        let h = long_history(8);
        let out = compact(h, &provider, &cfg).await.unwrap();
        assert_eq!(out.len(), 5);
        match &out[0] {
            HistoryMessage::User { content } => {
                assert!(content.starts_with("compact_boundary: "));
                assert!(content.contains("SUMMARY"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_archives_transcript_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = StubProvider {
            canned_summary: "S".into(),
        };
        let _ = compact(long_history(4), &provider, &cfg).await.unwrap();
        // At least one archive file landed in the directory.
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(!files.is_empty(), "expected an archive file");
        // And it has the markdown shape we wrote.
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(body.starts_with("# Compaction archive"));
    }

    #[tokio::test]
    async fn compact_noop_for_tiny_history() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = StubProvider {
            canned_summary: "S".into(),
        };
        let h = long_history(3);
        let out = compact(h.clone(), &provider, &cfg).await.unwrap();
        assert_eq!(out, h);
    }

    #[tokio::test]
    async fn compact_propagates_provider_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let provider = ErrorProvider;
        let err = compact(long_history(8), &provider, &cfg).await.unwrap_err();
        assert!(err.to_string().contains("synthetic"));
    }

    #[tokio::test]
    async fn compact_errors_on_empty_summary() {
        struct EmptyProvider;
        #[async_trait]
        impl AgentProvider for EmptyProvider {
            fn name(&self) -> &'static str {
                "empty"
            }
            async fn query(
                &self,
                _input: QueryInput,
            ) -> Result<Box<dyn AgentQuery>, ProviderError> {
                Ok(Box::new(StubQuery {
                    events: Mutex::new(vec![ProviderEvent::Result {
                        text: Some(String::new()),
                    }]),
                }))
            }
            fn is_session_invalid(&self, _err: &ProviderError) -> bool {
                false
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf());
        let err = compact(long_history(8), &EmptyProvider, &cfg)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty summary"));
    }

    #[test]
    fn summary_system_prompt_is_non_empty() {
        // Static assertion: a runtime check on a const string is optimised
        // out, so encode the invariant in `const _: () =`.
        const _: () = assert!(!SUMMARY_SYSTEM_PROMPT.is_empty());
        // And give the test something dynamic to look at so the test is
        // visible in coverage reports.
        let copy = SUMMARY_SYSTEM_PROMPT.to_string();
        assert_eq!(copy, SUMMARY_SYSTEM_PROMPT);
    }

    #[test]
    fn default_constants_are_positive() {
        const _: () = assert!(DEFAULT_INPUT_WINDOW > DEFAULT_SAFETY_MARGIN);
        // Runtime touch so the symbols are referenced from the test binary.
        let _ = DEFAULT_INPUT_WINDOW.to_string();
        let _ = DEFAULT_SAFETY_MARGIN.to_string();
    }

    // ---- Part (a): estimator ------------------------------------------

    #[test]
    fn ws_collapsed_len_merges_whitespace_runs() {
        assert_eq!(ws_collapsed_len("abc"), 3);
        // Four-space indent + newline collapses to a single char.
        assert_eq!(ws_collapsed_len("a\n    b"), 3); // 'a', <ws run>, 'b'
        assert_eq!(ws_collapsed_len("   "), 1);
        assert_eq!(ws_collapsed_len(""), 0);
    }

    #[test]
    fn estimate_uses_ceil_division() {
        // 1 char / 3.5 rounds up to 1 rather than truncating to 0.
        let h = vec![HistoryMessage::User {
            content: "x".into(),
        }];
        assert_eq!(estimate_tokens(&h), 1);
    }

    #[test]
    fn whitespace_collapse_tames_code_dense_transcripts() {
        // A code-dense body is dominated by indentation. The naive
        // char-count treated every indent space as ~0.25 tokens; the
        // collapsed estimate does not, so the same logical content
        // estimates lower and no longer trips compaction on whitespace.
        let indented = HistoryMessage::Assistant {
            content: "fn main() {\n        let x = 1;\n        let y = 2;\n}".into(),
        };
        let flat = HistoryMessage::Assistant {
            content: "fn main() { let x = 1; let y = 2; }".into(),
        };
        // Same logical tokens; the indented form must not estimate higher.
        assert!(estimate_tokens(&[indented]) <= estimate_tokens(&[flat]) + 1);
    }

    // ---- Part (b): profile-conditional soft target --------------------

    #[test]
    fn coding_profiles_get_the_raised_soft_target() {
        use crate::policy::ToolProfile;
        // The raised target is still well under the hard ceiling, so the
        // clamp in `effective_threshold` never has to fight it.
        const _: () = assert!(DEFAULT_SOFT_TARGET_CODING > DEFAULT_SOFT_TARGET);
        const _: () = assert!(DEFAULT_SOFT_TARGET_CODING < DEFAULT_INPUT_WINDOW);
        assert_eq!(
            default_soft_target_for_profile(ToolProfile::Coding),
            DEFAULT_SOFT_TARGET_CODING
        );
        assert_eq!(
            default_soft_target_for_profile(ToolProfile::Full),
            DEFAULT_SOFT_TARGET_CODING
        );
        // Chat profiles keep today's target — the raise is profile-scoped.
        assert_eq!(
            default_soft_target_for_profile(ToolProfile::Messaging),
            DEFAULT_SOFT_TARGET
        );
        assert_eq!(
            default_soft_target_for_profile(ToolProfile::Minimal),
            DEFAULT_SOFT_TARGET
        );
    }

    #[test]
    fn raised_coding_target_is_still_clamped_by_hard_ceiling() {
        // Part (b) must not remove the clamp. Even a huge misconfigured
        // target can't push the trigger past the safety net.
        let cfg = CompactionCfg {
            soft_target_tokens: DEFAULT_SOFT_TARGET_CODING,
            ..cfg_with_dir(PathBuf::from("/tmp"))
        };
        assert_eq!(cfg.effective_threshold(), DEFAULT_SOFT_TARGET_CODING);
        assert!(cfg.effective_threshold() < cfg.hard_threshold());
    }

    // ---- Part (c): pinned project-facts header ------------------------

    /// Lay down a project + todo store under `root` so
    /// `build_project_facts_header` has real R3 state to read.
    fn seed_project_state(root: &Path) {
        let proj = root.join("todo-app");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::write(proj.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "npm test\n").unwrap();
        std::fs::write(
            root.join("agent_todos.json"),
            r#"[
                {"id":1,"text":"Scaffold the app","status":"completed"},
                {"id":2,"text":"Add the API","status":"in_progress"},
                {"id":3,"text":"Deploy","status":"blocked","blocked_reason":"verify failed: 2 tests red"}
            ]"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn facts_header_carries_project_verify_branch_and_plan() {
        let tmp = tempfile::tempdir().unwrap();
        seed_project_state(tmp.path());
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(header.starts_with(PROJECT_FACTS_MARKER));
        assert!(header.contains("todo-app"));
        assert!(header.contains("branch: main"));
        assert!(header.contains("verify: npm test"));
        assert!(header.contains("[x] 1. Scaffold the app"));
        assert!(header.contains("[~] 2. Add the API"));
        assert!(header.contains("[!] 3. Deploy"));
        assert!(header.contains("blocked: verify failed"));
    }

    #[tokio::test]
    async fn facts_header_none_for_pure_chat_session() {
        let tmp = tempfile::tempdir().unwrap();
        // No project dirs, no todo store — a pure-chat group.
        assert_eq!(build_project_facts_header(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn facts_header_survives_three_compactions_verbatim() {
        let data = tempfile::tempdir().unwrap();
        seed_project_state(data.path());
        let archive = tempfile::tempdir().unwrap();
        let cfg = CompactionCfg {
            data_root: data.path().to_path_buf(),
            ..cfg_with_dir(archive.path().to_path_buf())
        };
        let provider = StubProvider {
            canned_summary: "SUMMARY".into(),
        };

        // The exact bytes we expect pinned, computed from the same on-disk
        // state the compaction reads.
        let expected = build_project_facts_header(data.path()).await.unwrap();

        let mut history = long_history(12);
        for round in 0..3 {
            history = compact(history, &provider, &cfg).await.unwrap();
            // Exactly one pinned header, always at the front, byte-identical.
            let headers: Vec<_> = history
                .iter()
                .filter(|m| is_pinned_facts_header(m))
                .collect();
            assert_eq!(
                headers.len(),
                1,
                "round {round}: expected exactly one pinned facts header"
            );
            match &history[0] {
                HistoryMessage::User { content } => assert_eq!(
                    content, &expected,
                    "round {round}: facts header must be pinned verbatim"
                ),
                other => panic!("round {round}: expected header first, got {other:?}"),
            }
            // The summary boundary is still present right after the header.
            assert!(matches!(
                &history[1],
                HistoryMessage::User { content } if content.starts_with("compact_boundary: ")
            ));
        }
    }

    #[tokio::test]
    async fn facts_header_absent_when_no_project_state() {
        // Regression guard for byte-stability: with an empty data root the
        // compacted output has no header — identical to pre-R4 shape.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_dir(tmp.path().to_path_buf()); // data_root == archive == empty tmp
        let provider = StubProvider {
            canned_summary: "S".into(),
        };
        let out = compact(long_history(8), &provider, &cfg).await.unwrap();
        assert!(!is_pinned_facts_header(&out[0]));
        match &out[0] {
            HistoryMessage::User { content } => assert!(content.starts_with("compact_boundary: ")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- M20 Q8: file inventory, all verify stages, decisions tail ----

    /// Run `git` in `dir`, panicking on failure — test-only helper, real
    /// `git` subprocess so `git ls-files` in `push_file_inventory` has a
    /// real index to read.
    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git spawn");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Lay down a real git repo with `files` written and staged (`git add`
    /// puts them in the index, which `git ls-files` reads — no commit
    /// needed). Used by the Q8a file-inventory tests.
    fn seed_git_project(root: &Path, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let proj = root.join(name);
        std::fs::create_dir_all(&proj).unwrap();
        run_git(&proj, &["init", "-q"]);
        run_git(&proj, &["config", "user.email", "test@example.com"]);
        run_git(&proj, &["config", "user.name", "test"]);
        for (path, content) in files {
            let file_path = proj.join(path);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&file_path, content).unwrap();
        }
        run_git(&proj, &["add", "."]);
        proj
    }

    #[tokio::test]
    async fn facts_header_includes_file_inventory_from_git_ls_files() {
        let tmp = tempfile::tempdir().unwrap();
        seed_git_project(
            tmp.path(),
            "webapp",
            &[
                ("src/main.ts", "console.log('hi')"),
                ("README.md", "# webapp"),
            ],
        );
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(header.contains("files (2):"), "header:\n{header}");
        assert!(header.contains("src/main.ts"));
        assert!(header.contains("README.md"));
    }

    #[tokio::test]
    async fn facts_header_no_file_inventory_when_not_a_git_repo() {
        // A `.copperclaw`-only project (no `.git`) is still a project
        // (`scan_projects` picks it up), but `git ls-files` has nothing to
        // read — best-effort means the inventory section is simply absent,
        // not an error.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("bare");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(header.contains("bare"));
        assert!(!header.contains("files ("));
    }

    #[tokio::test]
    async fn file_inventory_caps_at_max_files_and_notes_the_overflow() {
        let tmp = tempfile::tempdir().unwrap();
        let total = MAX_INVENTORY_FILES + 25;
        let files: Vec<(String, String)> = (0..total)
            .map(|i| (format!("file_{i:04}.txt"), "x".to_string()))
            .collect();
        let file_refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        seed_git_project(tmp.path(), "big", &file_refs);
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(
            header.contains(&format!("files ({total}):")),
            "header:\n{header}"
        );
        // Exactly MAX_INVENTORY_FILES path lines pinned, plus the overflow note.
        let file_lines = header
            .lines()
            .filter(|l| l.trim_start().starts_with("file_"))
            .count();
        assert_eq!(file_lines, MAX_INVENTORY_FILES);
        assert!(header.contains(&format!(
            "{} more files omitted",
            total - MAX_INVENTORY_FILES
        )));
    }

    #[tokio::test]
    async fn facts_header_pins_every_verify_stage_not_just_the_first() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("multi-stage-app");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(
            proj.join(".copperclaw").join("verify"),
            "lint: npx eslint .\ntypecheck: tsc --noEmit\nnpm test\n",
        )
        .unwrap();
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(header.contains("verify:\n"), "header:\n{header}");
        assert!(header.contains("lint: npx eslint ."));
        assert!(header.contains("typecheck: tsc --noEmit"));
        // The unprefixed third line gets the Q2-derived name `stage3`.
        assert!(header.contains("stage3: npm test"));
    }

    #[tokio::test]
    async fn facts_header_single_stage_verify_matches_pre_q8_format_exactly() {
        // Back-compat: a project with exactly one verify line (the pre-Q2,
        // and thus pre-Q8, common case) still renders the single
        // `  verify: <command>` line, not the multi-stage heading shape.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("simple-app");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "npm test\n").unwrap();
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        // No trailing-newline assumption: this project is the only content,
        // so the header's final `trim_end()` strips it from the last line.
        assert!(header.ends_with("  verify: npm test"), "header:\n{header}");
        assert!(!header.contains("verify:\n"));
    }

    #[tokio::test]
    async fn facts_header_includes_decisions_tail_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("decided-app");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(
            proj.join(".copperclaw").join("DECISIONS.md"),
            "chose sqlite over postgres because no server needed\n\
             chose vite over webpack because it's baked and faster\n",
        )
        .unwrap();
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(header.contains("decisions (last 2):"), "header:\n{header}");
        assert!(header.contains("chose sqlite over postgres because no server needed"));
        assert!(header.contains("chose vite over webpack because it's baked and faster"));
    }

    #[tokio::test]
    async fn facts_header_no_decisions_section_when_decisions_md_absent() {
        // The card's explicit acceptance case: a project with no
        // DECISIONS.md compacts exactly as today, plus the inventory.
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("no-decisions-app");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(proj.join(".copperclaw").join("verify"), "npm test\n").unwrap();
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(!header.contains("decisions ("));
        // Everything else (verify, project name) is present as before.
        assert!(header.contains("no-decisions-app"));
        assert!(header.contains("verify: npm test"));
    }

    #[tokio::test]
    async fn decisions_tail_caps_at_max_lines_keeping_the_most_recent() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("chatty-app");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        let total = MAX_DECISIONS_LINES + 10;
        let mut body = String::new();
        for i in 0..total {
            body.push_str(&format!("decision {i}\n"));
        }
        std::fs::write(proj.join(".copperclaw").join("DECISIONS.md"), body).unwrap();
        let header = build_project_facts_header(tmp.path()).await.unwrap();
        assert!(
            header.contains(&format!("decisions (last {MAX_DECISIONS_LINES}):")),
            "header:\n{header}"
        );
        // The oldest decisions are gone; the most recent one survives.
        assert!(!header.contains("decision 0\n"));
        assert!(header.contains(&format!("decision {}", total - 1)));
    }

    #[tokio::test]
    async fn compacted_coding_session_pins_inventory_stages_and_decisions() {
        // The card's headline acceptance case, exercised through the real
        // `compact()` entry point rather than the header builder directly.
        let data = tempfile::tempdir().unwrap();
        seed_git_project(
            data.path(),
            "full-app",
            &[("src/index.ts", "export {}"), ("package.json", "{}")],
        );
        let proj = data.path().join("full-app");
        std::fs::create_dir_all(proj.join(".copperclaw")).unwrap();
        std::fs::write(
            proj.join(".copperclaw").join("verify"),
            "lint: npx eslint .\ntest: npm test\n",
        )
        .unwrap();
        std::fs::write(
            proj.join(".copperclaw").join("DECISIONS.md"),
            "chose typescript over plain js because the scaffold bakes it\n",
        )
        .unwrap();

        let archive = tempfile::tempdir().unwrap();
        let cfg = CompactionCfg {
            data_root: data.path().to_path_buf(),
            ..cfg_with_dir(archive.path().to_path_buf())
        };
        let provider = StubProvider {
            canned_summary: "SUMMARY".into(),
        };
        let out = compact(long_history(8), &provider, &cfg).await.unwrap();
        let header = match &out[0] {
            HistoryMessage::User { content } => content.clone(),
            other => panic!("expected pinned header first, got {other:?}"),
        };
        assert!(header.contains("files (2):"));
        assert!(header.contains("src/index.ts"));
        assert!(header.contains("package.json"));
        assert!(header.contains("lint: npx eslint ."));
        assert!(header.contains("test: npm test"));
        assert!(header.contains("chose typescript over plain js because the scaffold bakes it"));
    }

    #[test]
    fn pair_safe_pivot_behavior_is_unchanged_by_q8() {
        // Explicit acceptance check: Q8 touches only the facts-header
        // assembly, never the pivot/summarisation path.
        let h = vec![txt(), txt(), tu("a"), tr("a"), txt(), txt()];
        assert_eq!(pair_safe_pivot(&h), 4);
    }
}
