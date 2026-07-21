//! The M18 Task HUD: one self-editing status message per inbound task.
//!
//! During a multi-minute run the only default feedback used to be a
//! typing bubble; three overlapping progress mechanisms existed
//! (env-gated per-tool breadcrumb chips, a 60s "still working" status
//! row, the pinned todo checklist) and none was on by default. The HUD
//! replaces the first two with ONE message per inbound task, posted at
//! the first tool call and edited in place after every tool batch (and
//! on a wall-clock cadence), carrying: the current todo step
//! (`agent_todos.json`), the last tool + Running/Done, the cumulative
//! tool count, and the elapsed time. The final edit collapses it to a
//! one-line "done in M:SS, N tool calls".
//!
//! Emission rides the existing breadcrumb rails
//! ([`copperclaw_mcp::ToolContext::emit_task_hud`]): the first emit is a
//! `MessageKind::Breadcrumb` row; every subsequent emit is an
//! `update_breadcrumb` System row the host's delivery loop resolves to
//! an in-place `edit_message` on the adapter. Because an adapter
//! without an edit API would degrade every update into a fresh message
//! (the exact new-message spam this HUD exists to kill), the HUD only
//! runs on channels whose adapter is known to support in-place edits
//! (`copperclaw_channels_core::capabilities::supports_message_edit`);
//! bare channels keep the old behaviour — a periodic "still working"
//! status row after each long silent stretch.
//!
//! M22 D5 adds a third leg between those two: channels whose CLIENT
//! renders an append-only frame log as an in-place transcript repaint
//! (`capabilities::renders_client_side_transcript` — today only cli,
//! whose JSONL `chat.log` is read by `cclaw chat`). Their adapter has
//! no `edit_message`, so the HUD emits every frame as a fresh
//! `MessageKind::Breadcrumb` EVENT (never an `update_breadcrumb`
//! edit), with the exact same frame content + fingerprint suppression
//! as the live edit path at the batch boundaries and the finalize
//! collapse; the wall-clock ticker stays edit-only (on an append-only
//! log every tick would be a permanent line — the client repaints the
//! elapsed clock itself). The client dedupes repeated frames and
//! repaints, so the append-only log never reads as spam.
//!
//! Surfaces where the platform draws no typing indicator at all
//! (`capabilities::typing_indicator_visible` — e.g. Slack outside an
//! assistant thread) force full HUD behaviour and a tighter edit
//! cadence, since the HUD is then the user's ONLY working signal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use copperclaw_channels_core::{
    Breadcrumb, BreadcrumbStatus, MAX_DETAIL_CHARS, MAX_SUMMARY_CHARS, MAX_TOOL_NAME_CHARS,
    capabilities,
};
use copperclaw_mcp::ToolContext;

use super::RunnerDeps;
use super::drive_turn::PendingToolCall;
use crate::clock::Clock;
use crate::config::HudMode;
use crate::tools::TASK_HUD_TOOL;

/// Wall-clock budget between user-facing emits on BARE channels before
/// the runner surfaces a "still working" status row. Sized to cover one
/// or two long tool calls (npm install, large grep, full build) without
/// chattering, while still putting a heartbeat in chat before the user
/// concludes the agent has hung. This is the pre-HUD behaviour,
/// preserved verbatim for adapters without an in-place edit API.
const STATUS_INTERVAL: Duration = Duration::from_secs(60);

/// F6: how long a bare-channel run must go before the periodic status row
/// upgrades from "I'll keep going." to a softer "this is taking longer
/// than usual, still going" reassurance. Sized well short of the 5-minute
/// terminal apology (`copperclaw-host-sweep`'s `APOLOGY_AFTER_SECS = 300`)
/// so a slow build degrades gracefully into "still going" instead of
/// cliff-edging straight from a fixed heartbeat into an apology. Because
/// status rows only fire on the [`STATUS_INTERVAL`] cadence, the first row
/// past this mark is the ~180s one.
const INTERMEDIATE_STATUS_AFTER: Duration = Duration::from_secs(150);

/// Minimum wall-clock edit cadence for the live HUD. Between tool-batch
/// boundaries a background ticker refreshes the elapsed clock at this
/// interval so a single long tool call (or a long silent reasoning
/// pass) still visibly ticks.
const HUD_EDIT_INTERVAL: Duration = Duration::from_secs(30);

/// Tighter ticker cadence used when the platform shows no typing
/// indicator on this surface — the HUD is then the only working signal,
/// so it must move faster than a human's "is it hung?" threshold.
const HUD_EDIT_INTERVAL_NO_TYPING: Duration = Duration::from_secs(10);

/// F5: how long a live-HUD turn must run *before its first tool call*
/// before the runner posts an initial "thinking…" frame. Sized so a
/// fast turn (a quick answer, a snappy tool call) resolves and finalizes
/// before this fires — those turns post nothing new and stay
/// byte-stable — while a multi-minute pure-reasoning answer stops
/// looking like a hang within a few seconds. The background HUD task
/// waits this long, then posts the first frame and continues as the
/// elapsed-clock ticker.
const THINKING_THRESHOLD: Duration = Duration::from_secs(6);

/// Default in-container location of the per-session todo store written
/// by the `todo_*` MCP tools. Mirrors `TODO_DEFAULT_PATH` in
/// `copperclaw-mcp` (the session dir is bind-mounted at `/data`).
pub const TODO_STORE_DEFAULT_PATH: &str = "/data/agent_todos.json";

/// M22 B5: bounded window of completed tool steps kept on the HUD's
/// shared state and attached to every emitted frame via
/// `Breadcrumb::steps`. Forty matches the renderers' own working
/// assumption (Telegram's `render_activity_html` folds anything beyond
/// its length budget into a `+N earlier` note); the producer keeps the
/// NEWEST forty so the visible transcript stays live on a long run.
const MAX_TRANSCRIPT_STEPS: usize = 40;

/// M22 B5: char cap on one step's result summary (the italic `— …` tail
/// in the rendered step line). The Telegram step renderer truncates at
/// 120 chars anyway; capping at the producer keeps the serialized frame
/// small and every other surface consistent.
const STEP_SUMMARY_CHARS: usize = 120;

/// M22 B4: maximum number of ` | `-separated fields in a live-frame
/// summary. The summary is the collapsed headline on mobile clients, so
/// beyond four fields it stops being scannable; optional fields are
/// dropped lowest-value-first (cost, then tokens, then the todo ratio)
/// and a pending one-shot note always survives.
const MAX_SUMMARY_FIELDS: usize = 4;

/// M22 B4: char budget for the composed live-frame summary — well under
/// `MAX_SUMMARY_CHARS` (200) so the collapsed headline never wraps into
/// noise on a phone. Optional fields are dropped (cost first, then
/// tokens, then the ratio) until the line fits.
const MAX_SUMMARY_LINE_CHARS: usize = 100;

/// M22 B4: live spend counters for the CURRENT inbound's provider calls,
/// shared between the provider-call layer (writer — tokens are in hand
/// there and nowhere else in-container; the runner cannot reach the
/// central `agent_turns` table) and the Task HUD (reader — the status
/// line and the final collapse). Reset by `drive_turn` at the top of
/// each inbound so the HUD shows per-task spend. All counters are
/// relaxed atomics: the HUD is a best-effort UX surface, not billing —
/// `emit_usage_report` remains the audited record.
#[derive(Debug, Default)]
pub struct TurnSpend {
    /// Cumulative input+output tokens billed across this inbound.
    tokens: AtomicU64,
    /// Cumulative list-price cost in micro-dollars, summed only over
    /// calls whose `(provider, model)` had a pricing-table hit.
    cost_micros: AtomicU64,
    /// At least one call had a pricing-table hit.
    priced_any: AtomicBool,
    /// At least one call had NO pricing-table hit. When set, the cost is
    /// a known undercount, so surfaces omit the dollar field entirely —
    /// a confidently wrong figure is worse than an honest blank.
    unpriced_any: AtomicBool,
}

impl TurnSpend {
    /// Fold one completed provider call's billed tokens into the live
    /// counters, pricing them via the shared table when the model is
    /// known. A zero-token call (providers that surface no usage) is a
    /// no-op so it can never flip the priced/unpriced flags.
    pub fn record(&self, provider: &str, model: &str, input_tokens: u32, output_tokens: u32) {
        let total = u64::from(input_tokens) + u64::from(output_tokens);
        if total == 0 {
            return;
        }
        self.tokens.fetch_add(total, Ordering::Relaxed);
        match copperclaw_types::pricing::price_for(provider, model) {
            Some(price) => {
                // u128 multiply-then-divide: u32 tokens * u64 per-MTok
                // micros overflows u64 in the worst case, never u128.
                let micros = (u128::from(input_tokens) * u128::from(price.input_per_mtok_micros)
                    + u128::from(output_tokens) * u128::from(price.output_per_mtok_micros))
                    / 1_000_000;
                self.cost_micros
                    .fetch_add(u64::try_from(micros).unwrap_or(u64::MAX), Ordering::Relaxed);
                self.priced_any.store(true, Ordering::Relaxed);
            }
            None => self.unpriced_any.store(true, Ordering::Relaxed),
        }
    }

    /// Zero every counter — called by `drive_turn` at inbound entry so
    /// the HUD's spend line is per-task.
    pub fn reset(&self) {
        self.tokens.store(0, Ordering::Relaxed);
        self.cost_micros.store(0, Ordering::Relaxed);
        self.priced_any.store(false, Ordering::Relaxed);
        self.unpriced_any.store(false, Ordering::Relaxed);
    }

    /// Consistent-enough snapshot for one frame render.
    fn view(&self) -> SpendView {
        SpendView {
            tokens: self.tokens.load(Ordering::Relaxed),
            cost_micros: self.cost_micros.load(Ordering::Relaxed),
            all_priced: self.priced_any.load(Ordering::Relaxed)
                && !self.unpriced_any.load(Ordering::Relaxed),
        }
    }
}

/// One frame's read of [`TurnSpend`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SpendView {
    tokens: u64,
    cost_micros: u64,
    /// True only when EVERY recorded call was priced — the gate for
    /// rendering a dollar figure at all (never `$0.00` for unknown).
    all_priced: bool,
}

/// How this inbound's HUD behaves, resolved once at `drive_turn` entry
/// from the configured [`HudMode`] and the originating channel's static
/// capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Live self-editing HUD (post at first tool call, edit per batch +
    /// ticker, collapse on finalize).
    Live,
    /// M22 D5: live HUD frames emitted as fresh `Breadcrumb` EVENTS
    /// instead of in-place edits, for channels whose CLIENT collapses
    /// the append-only frame log into a repaint
    /// (`capabilities::renders_client_side_transcript` — cli's
    /// `chat.log`, rendered by `cclaw chat`). Same frame content and
    /// fingerprint suppression as [`Self::Live`] at the batch
    /// boundaries + the finalize collapse; the wall-clock ticker stays
    /// Live-only (on an append-only log every clock tick would be a
    /// permanent line — the client repaints the elapsed clock itself).
    Transcript,
    /// Only the end-of-task one-line summary (`hud_mode = final` on an
    /// edit-capable channel with a visible typing indicator).
    FinalOnly,
    /// No HUD: periodic "still working" status rows on long silent
    /// stretches (bare channels, `hud_mode = off`, unknown routing).
    StatusRows,
}

/// Mutable HUD state shared with the background ticker.
#[derive(Debug, Default)]
struct Shared {
    /// Cumulative executed tool calls across this inbound's tool loop.
    tool_runs: usize,
    /// Human line for the current activity: `running: shell (cargo
    /// check)` while a batch executes, `last: shell ok` between batches.
    activity: Option<String>,
    /// One-shot annotation surfaced on the next edit then cleared. The
    /// hook R2 ("steering noted") and R5 ("switched provider") ride —
    /// they only need to set a line here.
    note: Option<String>,
    /// True once the HUD message has been posted (first emit done).
    posted: bool,
    /// True once the final collapse edit went out; later emits (ticker
    /// races) are suppressed.
    finalized: bool,
    /// M22 B5: this inbound's completed tool steps, newest-last, bounded
    /// at [`MAX_TRANSCRIPT_STEPS`] (oldest dropped). Attached to every
    /// emitted frame via `Breadcrumb::steps` so rich renderers (Telegram
    /// `render_activity_html`) show the live tool-call transcript. Fresh
    /// per inbound: the HUD (and this state) is constructed at
    /// `drive_turn` entry.
    steps: Vec<Breadcrumb>,
    /// Fingerprint of the last HUD frame actually emitted for this
    /// anchor (see [`frame_fingerprint`]). Guards against a redundant
    /// in-place edit whose rendered content is byte-identical to what
    /// was last sent — Telegram (and some other adapters) 400 such a
    /// no-op edit with "message is not modified". `None` until the first
    /// emit. Every emit path (batch edits, the background ticker, the
    /// finalize collapse) consults + updates this so the suppression
    /// spans all of them.
    last_emitted: Option<String>,
}

/// Per-inbound Task HUD driver. Constructed at `drive_turn` entry,
/// notified around every tool batch, finalized when the turn resolves.
/// All methods are best-effort — the HUD never aborts the turn.
pub(super) struct TaskHud {
    behavior: Behavior,
    /// True when the originating channel's adapter can edit a delivered
    /// message in place (`capabilities::supports_message_edit`). Kept
    /// distinct from [`Behavior`], which also folds in `hud_mode`: the
    /// R6 progressive final answer is independent of the HUD's own
    /// on/off/final mode, so it keys off this raw capability instead.
    answer_edit_capable: bool,
    /// Agent-group id string, for the H1 HUD metric labels.
    agent_group: String,
    started_at: Instant,
    shared: Arc<StdMutex<Shared>>,
    ctx: Arc<dyn ToolContext>,
    todo_path: PathBuf,
    edit_interval: Duration,
    /// Background HUD task (live HUD only; spawned once by [`Self::arm`]
    /// at turn start). It first waits [`THINKING_THRESHOLD`] so a fast
    /// turn finalizes before anything posts, then posts the initial
    /// "thinking…" frame (F5: covers the pre-first-tool / pure-reasoning
    /// wait) and continues as the elapsed-clock ticker, refreshing at
    /// [`Self::edit_interval`]. Aborted on finalize / drop.
    ticker: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Bare-channel status-row cadence anchor. Starts at construction
    /// (== `drive_turn` entry), bumped after each status emit so the
    /// cadence stays at [`STATUS_INTERVAL`] regardless of how long
    /// individual tool calls take.
    last_status_emit_at: StdMutex<Instant>,
    /// M21 S6: the clock every elapsed-time read in this driver goes
    /// through ([`RunnerDeps::clock`]). Production is the real clock
    /// ([`crate::clock::SystemClock`]); deterministic tests inject a
    /// [`crate::clock::TestClock`] so the 60s status-row leg and the
    /// 150s softening are reachable without wall-clock waits.
    clock: Arc<dyn Clock>,
    /// M22 B1: the loop-level notice queue ([`RunnerDeps::notices`]).
    /// [`Self::new`] drains one queued note into [`Shared::note`] so it
    /// rides this turn's first frame / status row; [`Self::finalize`]
    /// pushes an un-rendered note BACK so a fast pure-text turn (which
    /// posts no frame at all) defers the note to a later turn instead of
    /// silently dropping it.
    notices: Arc<super::Notices>,
    /// M22 B4: the live per-inbound spend counters
    /// ([`RunnerDeps::spend`]) — written by the provider-call layer,
    /// read here for the status line's token/cost fields and the final
    /// collapse.
    spend: Arc<TurnSpend>,
}

impl TaskHud {
    /// Resolve the HUD behaviour for this inbound and build the driver.
    pub(super) fn new(deps: &RunnerDeps) -> Self {
        let origin = deps.tool_ctx.originating_channel();
        let behavior = match (&origin, deps.hud_mode) {
            // Operator opted out (`off`): today's behaviour everywhere.
            // Same for an inbound with no user channel routing
            // (agent-dispatched rows resolved by delivery's wiring
            // fallback, child sessions, mocks): the runner can't know
            // the adapter's capabilities, so degrade safely to the
            // status-row path.
            (_, HudMode::Off) | (None, _) => Behavior::StatusRows,
            (Some(oc), mode) => {
                let typing_visible = capabilities::typing_indicator_visible(
                    &oc.channel_type,
                    oc.platform_id.as_deref().unwrap_or(""),
                    oc.thread_id.as_deref(),
                );
                // No typing signal on this surface: the HUD is the
                // only working indicator, so `final` is forced back
                // to the live HUD. Precedence (M22 D5): a real edit
                // API wins (in-place edits), then a client-side
                // transcript renderer (fresh-message frames the
                // client collapses), then the bare status-row leg.
                if capabilities::supports_message_edit(&oc.channel_type) {
                    if mode == HudMode::Final && typing_visible {
                        Behavior::FinalOnly
                    } else {
                        Behavior::Live
                    }
                } else if capabilities::renders_client_side_transcript(&oc.channel_type) {
                    if mode == HudMode::Final && typing_visible {
                        Behavior::FinalOnly
                    } else {
                        Behavior::Transcript
                    }
                } else {
                    Behavior::StatusRows
                }
            }
        };
        let edit_interval = match &origin {
            Some(oc)
                if !capabilities::typing_indicator_visible(
                    &oc.channel_type,
                    oc.platform_id.as_deref().unwrap_or(""),
                    oc.thread_id.as_deref(),
                ) =>
            {
                HUD_EDIT_INTERVAL_NO_TYPING
            }
            _ => HUD_EDIT_INTERVAL,
        };
        let answer_edit_capable = origin
            .as_ref()
            .is_some_and(|oc| capabilities::supports_message_edit(&oc.channel_type));
        // H1: record when the HUD degrades to status-rows (bare channel,
        // hud_mode=off, or no originating-channel routing).
        if behavior == Behavior::StatusRows {
            let ct = origin
                .as_ref()
                .map_or("none", |oc| oc.channel_type.as_str());
            let reason = match (&origin, deps.hud_mode) {
                (_, HudMode::Off) => "hud_mode_off",
                (None, _) => "no_originating_channel",
                _ => "no_message_edit",
            };
            copperclaw_metrics::inc_hud_degraded(ct, reason);
        }
        let now = deps.clock.now();
        // B1: adopt one queued loop-level notice (auto-compaction fired
        // before this turn) as the turn's pending one-shot note, so it
        // rides the first live frame / status row through the existing
        // note machinery. FIFO, one per turn — further queued notices
        // wait for later turns.
        let shared = Shared {
            note: deps.notices.drain_one(),
            ..Shared::default()
        };
        Self {
            behavior,
            answer_edit_capable,
            agent_group: deps.agent_group_id.to_string(),
            started_at: now,
            shared: Arc::new(StdMutex::new(shared)),
            ctx: deps.tool_ctx.clone(),
            todo_path: deps.todo_path.clone(),
            edit_interval,
            ticker: StdMutex::new(None),
            last_status_emit_at: StdMutex::new(now),
            clock: Arc::clone(&deps.clock),
            notices: Arc::clone(&deps.notices),
            spend: Arc::clone(&deps.spend),
        }
    }

    /// M22 B5: fold one COMPLETED tool call into the transcript window.
    /// Called from `drive_turn`'s batch-results loop (the one place the
    /// per-call result text exists) with the model-requested name/input
    /// and the tool's rendered result. Bounded at
    /// [`MAX_TRANSCRIPT_STEPS`] newest steps; the next emitted frame
    /// carries the updated window via `Breadcrumb::steps`.
    pub(super) fn record_step(
        &self,
        name: &str,
        input: &serde_json::Value,
        result: &str,
        is_error: bool,
    ) {
        let step = Breadcrumb {
            tool_name: cap_chars(name, MAX_TOOL_NAME_CHARS),
            detail: crate::tools::breadcrumb_detail(name, input),
            status: if is_error {
                BreadcrumbStatus::Failed
            } else {
                BreadcrumbStatus::Done
            },
            summary: step_result_summary(result),
            steps: Vec::new(),
        };
        if let Ok(mut s) = self.shared.lock() {
            if s.steps.len() >= MAX_TRANSCRIPT_STEPS {
                s.steps.remove(0);
            }
            s.steps.push(step);
        }
    }

    /// One-shot annotation for the next HUD edit. Cleared after it has
    /// been rendered once; only live HUD frames render it. This is the
    /// M18 R2 "steering noted" / R5 "switched to <provider>" hook —
    /// `run_llm_turn` calls it on a mid-turn provider failover.
    pub(super) fn add_note(&self, text: &str) {
        if let Ok(mut s) = self.shared.lock() {
            s.note = Some(text.to_owned());
        }
    }

    /// Test-only: peek the pending one-shot note without consuming it.
    /// Lets the R5 failover tests assert `add_note` fired on a mid-turn
    /// provider switch even under the `StatusRows` behaviour (where the
    /// note stays pending until the next 60s status row takes it).
    #[cfg(test)]
    pub(super) fn note_for_test(&self) -> Option<String> {
        self.shared.lock().ok().and_then(|s| s.note.clone())
    }

    /// R6: whether this inbound's final answer may grow via in-place
    /// edits — true only on an edit-capable originating channel (the
    /// same `supports_message_edit` gate the HUD uses), and never for
    /// child sessions (their `originating_channel()` is `None`).
    pub(super) fn answer_edit_capable(&self) -> bool {
        self.answer_edit_capable
    }

    /// R6: wall-clock elapsed since `drive_turn` entry. Threaded into
    /// the progressive-final gate (>30s turns only) so it reads the ONE
    /// clock the HUD already tracks (`started_at`) rather than the
    /// caller re-deriving its own. Reads through the S6 clock seam so
    /// the run loop's timed gates advance with a test clock too.
    pub(super) fn elapsed(&self) -> Duration {
        self.clock.now().saturating_duration_since(self.started_at)
    }

    /// A tool batch is about to execute. Live HUD: post (first batch) or
    /// edit the HUD to show the running tools. Also records the batch's
    /// lead tool for the activity line.
    pub(super) async fn on_batch_start(&self, calls: &[PendingToolCall]) {
        if !self.emits_live_frames() {
            return;
        }
        let activity = calls.first().map(|c| {
            let detail = crate::tools::breadcrumb_detail(&c.name, &c.input);
            let extra = calls.len().saturating_sub(1);
            let mut line = match detail {
                Some(d) => format!("running: {} ({d})", c.name),
                None => format!("running: {}", c.name),
            };
            if extra > 0 {
                line.push_str(&format!(" +{extra} more"));
            }
            line
        });
        if let Ok(mut s) = self.shared.lock() {
            s.activity = activity;
        }
        self.emit_live_update("batch_start").await;
    }

    /// A tool batch finished. `tool_runs` is the cumulative executed
    /// count, `last_tool` the batch's final tool name, `all_ok` whether
    /// every call in the batch succeeded. Live HUD: edit in place. Bare
    /// channels: surface the old periodic "still working" status row
    /// when the silent stretch exceeds [`STATUS_INTERVAL`].
    pub(super) async fn on_batch_end(
        &self,
        tool_runs: usize,
        last_tool: Option<&str>,
        all_ok: bool,
    ) {
        match self.behavior {
            Behavior::Live | Behavior::Transcript => {
                if let Ok(mut s) = self.shared.lock() {
                    s.tool_runs = tool_runs;
                    s.activity = last_tool.map(|t| {
                        let verdict = if all_ok { "ok" } else { "failed" };
                        format!("last: {t} {verdict}")
                    });
                }
                self.emit_live_update("batch_end").await;
            }
            Behavior::FinalOnly => {
                if let Ok(mut s) = self.shared.lock() {
                    s.tool_runs = tool_runs;
                }
            }
            Behavior::StatusRows => {
                if let Ok(mut s) = self.shared.lock() {
                    s.tool_runs = tool_runs;
                }
                self.maybe_emit_status_row(tool_runs, last_tool).await;
            }
        }
    }

    /// Terminal edit: collapse the HUD to the one-line summary. `ok`
    /// mirrors the turn outcome (`Done` vs `Failed`). Aborts the ticker
    /// first so no stale Running edit can race past the collapse.
    pub(super) async fn finalize(&self, ok: bool) {
        if let Ok(mut guard) = self.ticker.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
        let (posted, tool_runs, already_finalized, unrendered_note, steps) =
            match self.shared.lock() {
                Ok(mut s) => {
                    let snapshot = (
                        s.posted,
                        s.tool_runs,
                        s.finalized,
                        s.note.take(),
                        std::mem::take(&mut s.steps),
                    );
                    s.finalized = true;
                    snapshot
                }
                Err(_) => return,
            };
        // B1: a note this turn never rendered (fast pure-text turn — no
        // frame posted, no status row due) goes back to the queue so a
        // later turn surfaces it instead of dropping it on the floor.
        if let Some(note) = unrendered_note {
            self.notices.push(note);
        }
        if already_finalized || (tool_runs == 0 && !posted) {
            // Double finalize, or a fast turn that never posted a frame:
            // no HUD to collapse and no final line worth posting. (A
            // zero-tool turn that DID post an F5 "thinking…" frame still
            // collapses below, so the thinking frame never dangles.)
            return;
        }
        let elapsed_total = self.elapsed();
        copperclaw_metrics::observe_hud_finalize_seconds(elapsed_total.as_secs_f64());
        let elapsed = fmt_mmss(elapsed_total.as_secs());
        let plural = if tool_runs == 1 { "" } else { "s" };
        // F5: a pure-reasoning turn collapses its "thinking…" frame to a
        // clean "done in M:SS" (the "0 tool calls" tail would read oddly).
        let mut summary = match (ok, tool_runs) {
            (true, 0) => format!("done in {elapsed}"),
            (false, 0) => format!("stopped after {elapsed}"),
            (true, n) => format!("done in {elapsed}, {n} tool call{plural}"),
            (false, n) => format!("stopped after {elapsed}, {n} tool call{plural}"),
        };
        // B4: append the task's spend, in this line's existing comma
        // style. Zero recorded tokens (providers that surface no usage)
        // renders nothing, keeping the pre-B4 bytes; the cost renders
        // only when EVERY call was priced (never $0.00 for unknown).
        let spend = self.spend.view();
        if let Some(t) = fmt_tokens(spend.tokens) {
            summary.push_str(&format!(", {t}"));
        }
        if let Some(c) = fmt_cost(spend.cost_micros, spend.all_priced) {
            summary.push_str(&format!(", {c}"));
        }
        // B5: the collapse keeps the accumulated step transcript attached
        // so the tool log stays readable in scrollback after the run.
        let breadcrumb = Breadcrumb {
            tool_name: TASK_HUD_TOOL.to_owned(),
            detail: None,
            status: if ok {
                BreadcrumbStatus::Done
            } else {
                BreadcrumbStatus::Failed
            },
            summary: Some(cap_chars(&summary, MAX_SUMMARY_CHARS)),
            steps,
        };
        match self.behavior {
            // Collapse the live HUD in place. Skip if the collapse frame
            // is byte-identical to the last live frame already on screen
            // (a no-op edit Telegram would 400 on).
            Behavior::Live if posted => {
                if record_and_should_emit(&self.shared, &breadcrumb, false) {
                    copperclaw_metrics::inc_hud_edits(&self.agent_group, "finalize");
                    self.ctx.emit_task_hud(&breadcrumb, false).await;
                }
            }
            // D5: the Transcript collapse is the same frame emitted as a
            // fresh EVENT (no edit anchor exists on an append-only log);
            // the client folds it over the running frames. The same
            // fingerprint guard applies so a redundant duplicate of the
            // last frame is never appended.
            Behavior::Transcript if posted => {
                if record_and_should_emit(&self.shared, &breadcrumb, false) {
                    copperclaw_metrics::inc_hud_edits(&self.agent_group, "finalize");
                    self.ctx.emit_task_hud(&breadcrumb, true).await;
                }
            }
            // `final`: the one-liner is the only HUD emission at all.
            Behavior::FinalOnly => {
                copperclaw_metrics::inc_hud_post(&self.agent_group);
                self.ctx.emit_task_hud(&breadcrumb, true).await;
            }
            _ => {}
        }
    }

    /// Compose + emit the current live-HUD frame (post on first call,
    /// in-place edit afterwards). Skipped after finalization.
    async fn emit_live_update(&self, trigger: &str) {
        let Some((breadcrumb, first)) = self.compose_running_frame() else {
            return;
        };
        // Suppress an in-place edit whose rendered content is byte-identical
        // to the last frame sent for this anchor — it would be a no-op the
        // adapter (Telegram) 400s on. First posts always go through.
        if !record_and_should_emit(&self.shared, &breadcrumb, first) {
            return;
        }
        if first {
            copperclaw_metrics::inc_hud_post(&self.agent_group);
        } else {
            // On the Transcript path this counts re-emitted frames, not
            // literal edits — same lifecycle position, same counter (the
            // M22 program adds no new metrics).
            copperclaw_metrics::inc_hud_edits(&self.agent_group, trigger);
        }
        // D5: Transcript frames are always fresh messages — an
        // append-only log has no edit anchor, so `first` is forced true
        // at the emit boundary (the client dedupes and repaints).
        self.ctx
            .emit_task_hud(&breadcrumb, self.emit_as_new(first))
            .await;
    }

    /// True when this inbound emits live HUD frames at all — the
    /// edit-based [`Behavior::Live`] path or the D5 event-based
    /// [`Behavior::Transcript`] path. The two share the batch
    /// start/end emission points, the finalize collapse, and the
    /// fingerprint suppression; the wall-clock ticker (and its F5
    /// thinking frame) stays Live-only — see [`Self::arm`].
    fn emits_live_frames(&self) -> bool {
        matches!(self.behavior, Behavior::Live | Behavior::Transcript)
    }

    /// Whether `emit_task_hud` should open a fresh message for this
    /// frame. `first` frames always do; on [`Behavior::Transcript`]
    /// EVERY frame does (no edit anchor exists on an append-only log —
    /// `existing_message_id` stays `None` all the way through
    /// delivery).
    fn emit_as_new(&self, first: bool) -> bool {
        first || self.behavior == Behavior::Transcript
    }

    /// Build the Running-state breadcrumb from shared state + the todo
    /// store + the wall clock, flipping `posted` when this is the first
    /// frame. `None` when the HUD is already finalized (ticker race).
    fn compose_running_frame(&self) -> Option<(Breadcrumb, bool)> {
        running_frame(
            &self.shared,
            self.started_at,
            self.clock.now(),
            &self.todo_path,
            &self.spend,
        )
    }

    /// F5: arm the background HUD task at turn start (live HUD only).
    /// It waits [`THINKING_THRESHOLD`] — so a turn that finalizes first
    /// posts nothing and stays byte-stable — then posts the initial
    /// "thinking…" frame (covering the pre-first-tool / pure-reasoning
    /// wait the HUD used to leave blank) and continues as the
    /// elapsed-clock ticker at [`Self::edit_interval`].
    ///
    /// When a tool call arrives before the threshold, [`Self::on_batch_start`]
    /// posts the first frame itself; this task's first post then simply
    /// becomes an in-place edit (the no-op guard drops it if unchanged),
    /// so there is never a duplicate HUD message. Idempotent: a second
    /// call is a no-op once the task is spawned.
    pub(super) fn arm(&self) {
        // D5: the ticker stays LIVE-ONLY by design. On the edit path
        // its clock refreshes vanish into one edited message; on an
        // append-only transcript log every tick would be a PERMANENT
        // frame line (a 10-minute silent run = 20 junk rows replayed
        // forever), so the Transcript path emits only at batch
        // boundaries + finalize and leaves the live elapsed-clock
        // repaint to the client (`cclaw chat` owns a real terminal).
        if self.behavior != Behavior::Live {
            return;
        }
        let Ok(mut guard) = self.ticker.lock() else {
            return;
        };
        if guard.is_some() {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let ctx = Arc::clone(&self.ctx);
        let todo_path = self.todo_path.clone();
        let started_at = self.started_at;
        let interval = self.edit_interval;
        let agent_group = self.agent_group.clone();
        let clock = Arc::clone(&self.clock);
        let spend = Arc::clone(&self.spend);
        *guard = Some(tokio::spawn(async move {
            // Pre-first-tool wait: hold off the initial post so fast turns
            // (which finalize before this elapses) never post.
            tokio::time::sleep(THINKING_THRESHOLD).await;
            loop {
                let Some((frame, first)) =
                    running_frame(&shared, started_at, clock.now(), &todo_path, &spend)
                else {
                    break;
                };
                // Suppress a frame byte-identical to the last one sent (a
                // no-op edit Telegram 400s on) — but a `first` post always
                // goes through.
                if record_and_should_emit(&shared, &frame, first) {
                    if first {
                        copperclaw_metrics::inc_hud_post(&agent_group);
                        // M19 F5: this armed-task first post is the pre-first-tool
                        // "thinking…" frame (distinct from the tool-triggered post
                        // in emit_live_update).
                        copperclaw_metrics::inc_hud_thinking_frame(&agent_group);
                    } else {
                        copperclaw_metrics::inc_hud_edits(&agent_group, "ticker");
                    }
                    ctx.emit_task_hud(&frame, first).await;
                }
                tokio::time::sleep(interval).await;
            }
        }));
    }

    /// Bare-channel fallback: the periodic "still working" heartbeat for
    /// adapters without an edit API (and `hud_mode = off`). The emit goes
    /// direct to outbound as a Chat row via `emit_status`; it does NOT
    /// touch the model's history, and child-agent sessions skip inside
    /// `RunnerToolCtx::emit_status`, so this stays quiet for sub-agents.
    ///
    /// F6 enriches the row: it now carries the current todo step (the same
    /// `step N/M: …` detail the Live HUD shows) so bare channels get real
    /// progress rather than a fixed string, and past
    /// [`INTERMEDIATE_STATUS_AFTER`] it softens to a "taking longer than
    /// usual, still going" reassurance so the run degrades gracefully
    /// toward the 5-minute apology instead of cliff-edging into it.
    async fn maybe_emit_status_row(&self, tool_runs: usize, last_tool: Option<&str>) {
        let now = self.clock.now();
        let due = self
            .last_status_emit_at
            .lock()
            .map(|at| now.saturating_duration_since(*at) >= STATUS_INTERVAL)
            .unwrap_or(false);
        if !due {
            return;
        }
        let elapsed_secs = now.saturating_duration_since(self.started_at).as_secs();
        let todo_step = current_todo_step(&self.todo_path);
        // B1: take (one-shot) the pending note so it rides this row and
        // never a later one — the bare-channel mirror of the live HUD's
        // render-then-clear in `running_frame`.
        let note = self.shared.lock().ok().and_then(|mut s| s.note.take());
        let status = compose_status_row(elapsed_secs, tool_runs, last_tool, todo_step, note);
        self.ctx.emit_status(&status).await;
        if let Ok(mut at) = self.last_status_emit_at.lock() {
            *at = self.clock.now();
        }
    }
}

/// Compose one bare-channel status row (F6). Carries the elapsed clock,
/// the cumulative tool count, the latest tool, and — when the session's
/// todo store has one — the current `step N/M: …` detail so bare channels
/// show real progress. Past [`INTERMEDIATE_STATUS_AFTER`] the closing
/// reassurance softens to "taking longer than usual, still going". A
/// pending one-shot note (M22 B1 — e.g. the auto-compaction notice) is
/// appended after the step clause; the caller takes it from [`Shared`]
/// so it appears on exactly one row. Pure so the folding + threshold can
/// be unit-tested without wall-clock waits.
fn compose_status_row(
    elapsed_secs: u64,
    tool_runs: usize,
    last_tool: Option<&str>,
    todo_step: Option<String>,
    note: Option<String>,
) -> String {
    let last = last_tool.unwrap_or("thinking");
    let plural = if tool_runs == 1 { "" } else { "s" };
    let mut status = format!(
        "Still working on this — {elapsed_secs}s in, \
         {tool_runs} tool call{plural} so far (latest: {last})"
    );
    if let Some(step) = todo_step {
        status.push_str(" — ");
        status.push_str(&step);
    }
    if let Some(n) = note {
        status.push_str(" — ");
        status.push_str(&n);
    }
    if elapsed_secs >= INTERMEDIATE_STATUS_AFTER.as_secs() {
        status.push_str(". This is taking longer than usual, but I'm still going.");
    } else {
        status.push_str(". I'll keep going.");
    }
    status
}

impl Drop for TaskHud {
    fn drop(&mut self) {
        // Belt-and-braces: never leave the ticker editing a dead HUD.
        if let Ok(mut guard) = self.ticker.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}

/// Compose one Running-state HUD frame from shared state + the todo
/// store + the wall clock. `now` comes from the caller's [`Clock`] (the
/// S6 seam) so the elapsed rendering is deterministic under a test
/// clock. Marks the HUD `posted` and returns whether this was the first
/// frame. `None` once finalized (ticker race) — the caller stops
/// emitting.
fn running_frame(
    shared: &StdMutex<Shared>,
    started_at: Instant,
    now: Instant,
    todo_path: &Path,
    spend: &TurnSpend,
) -> Option<(Breadcrumb, bool)> {
    let (first, tool_runs, activity, note, steps) = {
        let mut s = shared.lock().ok()?;
        if s.finalized {
            return None;
        }
        let first = !s.posted;
        s.posted = true;
        (
            first,
            s.tool_runs,
            s.activity.clone(),
            s.note.take(),
            s.steps.clone(),
        )
    };
    let elapsed = fmt_mmss(now.saturating_duration_since(started_at).as_secs());
    let progress = current_todo_progress(todo_path);
    // B3: the compact `step N/M` ratio rides the summary (the collapsed
    // headline on mobile); the full `step N/M: <text>` line stays in
    // detail as before.
    let ratio = progress
        .as_ref()
        .map(|(step_no, total, _)| format!("step {step_no}/{total}"));
    let summary = compose_summary(
        tool_runs,
        // F5: before the first tool runs the frame is the pre-first-tool
        // "thinking…" wait (posted by the armed background task after
        // THINKING_THRESHOLD). Once a tool has run it's the usual
        // tool-count summary.
        tool_runs == 0 && activity.is_none(),
        &elapsed,
        ratio.as_deref(),
        spend.view(),
        note.as_deref(),
    );
    let mut detail_parts: Vec<String> = Vec::new();
    if let Some((step_no, total, text)) = progress {
        detail_parts.push(format!("step {step_no}/{total}: {text}"));
    }
    if let Some(a) = activity {
        detail_parts.push(a);
    }
    let detail = if detail_parts.is_empty() {
        None
    } else {
        Some(cap_chars(&detail_parts.join(" | "), MAX_DETAIL_CHARS))
    };
    let breadcrumb = Breadcrumb {
        tool_name: TASK_HUD_TOOL.to_owned(),
        detail,
        status: BreadcrumbStatus::Running,
        summary: Some(cap_chars(&summary, MAX_SUMMARY_CHARS)),
        steps,
    };
    Some((breadcrumb, first))
}

/// Compose one live-frame summary line (M22 B3/B4). Field order keeps
/// the pre-M22 head (`N tool calls | M:SS` / `thinking… | M:SS`)
/// byte-identical, then appends the new optional fields — todo ratio,
/// rounded token count, rounded cost — capped at [`MAX_SUMMARY_FIELDS`]
/// total. Optional fields are dropped lowest-value-first (cost, then
/// tokens, then the ratio); a pending one-shot note is never dropped and
/// keeps its historical tail position. Pure so the field cap, drop
/// order, and length bound are unit-testable.
fn compose_summary(
    tool_runs: usize,
    thinking: bool,
    elapsed: &str,
    ratio: Option<&str>,
    spend: SpendView,
    note: Option<&str>,
) -> String {
    let plural = if tool_runs == 1 { "" } else { "s" };
    let head = if thinking {
        "thinking…".to_owned()
    } else {
        format!("{tool_runs} tool call{plural}")
    };
    let fields: Vec<String> = vec![head, elapsed.to_owned()];
    let mut optional: Vec<String> = Vec::new();
    if let Some(r) = ratio {
        optional.push(r.to_owned());
    }
    if let Some(t) = fmt_tokens(spend.tokens) {
        optional.push(t);
    }
    if let Some(c) = fmt_cost(spend.cost_micros, spend.all_priced) {
        optional.push(c);
    }
    // Truncation drops from the END of the optional list, so the cost
    // goes first, then tokens, then the ratio — and a pending note
    // (never dropped) tightens the budget by one.
    let budget = MAX_SUMMARY_FIELDS.saturating_sub(fields.len() + usize::from(note.is_some()));
    optional.truncate(budget);
    // Length backstop on the same drop order: an extreme field (a
    // pathological token count alongside a pending note) sheds optional
    // fields until the line fits the mobile budget.
    let compose = |fields: &[String], optional: &[String], note: Option<&str>| {
        let mut all: Vec<&str> = fields.iter().map(String::as_str).collect();
        all.extend(optional.iter().map(String::as_str));
        if let Some(n) = note {
            all.push(n);
        }
        all.join(" | ")
    };
    let mut line = compose(&fields, &optional, note);
    while line.chars().count() > MAX_SUMMARY_LINE_CHARS && !optional.is_empty() {
        optional.pop();
        line = compose(&fields, &optional, note);
    }
    line
}

/// Render a cumulative token count for the status line, rounded to the
/// nearest 100 so the frame fingerprint doesn't change (and force an
/// in-place edit) on every LLM call: `3_237` → `3.2k tokens`, `800` →
/// `800 tokens`, `38_049` → `38k tokens`. `None` when the rounded count
/// is zero (providers that surface no usage) so those frames stay
/// byte-identical to the pre-B4 shape.
fn fmt_tokens(tokens: u64) -> Option<String> {
    let rounded = tokens.saturating_add(50) / 100 * 100;
    if rounded == 0 {
        return None;
    }
    if rounded < 1000 {
        return Some(format!("{rounded} tokens"));
    }
    let k = rounded / 1000;
    let tenths = rounded % 1000 / 100;
    if tenths == 0 {
        Some(format!("{k}k tokens"))
    } else {
        Some(format!("{k}.{tenths}k tokens"))
    }
}

/// Render the cumulative cost for the status line, rounded to the
/// nearest cent. `None` unless EVERY recorded call was priced (an
/// unknown model must render as absence, never `$0.00`) and the rounded
/// amount is at least one cent (a leading `$0.00` is noise, and ollama's
/// genuinely-zero local cost stays blank by the same rule).
fn fmt_cost(micros: u64, all_priced: bool) -> Option<String> {
    if !all_priced {
        return None;
    }
    let cents = micros.saturating_add(5_000) / 10_000;
    if cents == 0 {
        return None;
    }
    Some(format!("${}.{:02}", cents / 100, cents % 100))
}

/// One tool call's result, reduced to the step's post-completion
/// summary (the `— …` tail in the rendered step line): the first
/// non-empty line of the human-readable content, capped at
/// [`STEP_SUMMARY_CHARS`]. Many first-party tools return a JSON
/// envelope (the shell tool's `{command, exit_code, stdout, …}`), so a
/// JSON-object result surfaces its first populated human field instead
/// of the literal opening brace. `None` when nothing readable exists so
/// the renderer omits the tail entirely.
fn step_result_summary(result: &str) -> Option<String> {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(result) {
        for key in ["stdout", "message", "text", "content", "error", "stderr"] {
            if let Some(line) = map
                .get(key)
                .and_then(serde_json::Value::as_str)
                .and_then(first_line)
            {
                return Some(line);
            }
        }
        // A silent command: fall back to its exit code (deterministic,
        // unlike the envelope's elapsed_ms) rather than raw JSON.
        if let Some(code) = map.get("exit_code").and_then(serde_json::Value::as_i64) {
            return Some(format!("exit {code}"));
        }
        return None;
    }
    first_line(result)
}

/// First non-empty trimmed line of `s`, capped at
/// [`STEP_SUMMARY_CHARS`].
fn first_line(s: &str) -> Option<String> {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(cap_chars(line, STEP_SUMMARY_CHARS))
}

/// Fingerprint one composed HUD frame for no-op-edit suppression. Two
/// frames with equal fingerprints carry identical breadcrumb content and
/// therefore render (deterministically, in the delivery loop) to
/// byte-identical platform text — so an in-place edit to the second is a
/// no-op that Telegram rejects with "message is not modified". Uses the
/// serialised breadcrumb (the exact payload handed to the delivery loop)
/// so the fingerprint tracks precisely what would be rendered.
fn frame_fingerprint(breadcrumb: &Breadcrumb) -> String {
    serde_json::to_string(breadcrumb).unwrap_or_default()
}

/// Decide whether a composed HUD frame should actually be emitted,
/// recording it as the last-emitted content for this anchor when so.
///
/// A `first` post is always emitted (it opens a fresh message, never an
/// edit). A subsequent edit is SUPPRESSED only when its rendered content
/// is byte-identical to the frame last sent for this same anchor — the
/// redundant no-op edit Telegram 400s on. Any real content change (the
/// common case: the elapsed clock ticks, the tool count grows, the
/// activity line flips) emits normally. Best-effort: a poisoned lock
/// emits rather than suppress, so the guard can never silence a real
/// update.
fn record_and_should_emit(shared: &StdMutex<Shared>, breadcrumb: &Breadcrumb, first: bool) -> bool {
    let Ok(mut s) = shared.lock() else {
        return true;
    };
    let fp = frame_fingerprint(breadcrumb);
    if !first && s.last_emitted.as_deref() == Some(fp.as_str()) {
        return false;
    }
    s.last_emitted = Some(fp);
    true
}

/// `M:SS` wall-clock rendering (`83` → `1:23`).
fn fmt_mmss(total_secs: u64) -> String {
    format!("{}:{:02}", total_secs / 60, total_secs % 60)
}

/// Char-cap `s` to `max`, appending an ASCII ellipsis marker when
/// truncated (the HUD line is plain text on every channel).
fn cap_chars(s: &str, max: usize) -> String {
    copperclaw_channels_core::vocab::truncate_chars(s, max, "...")
}

/// On-disk shape of one `agent_todos.json` entry (see
/// `copperclaw-mcp/src/tools/todo.rs`). Only the fields the HUD reads.
#[derive(serde::Deserialize)]
struct TodoEntry {
    text: String,
    status: String,
}

/// Current todo step line for the HUD (`step 2/5: build the UI`), read
/// best-effort from the session's todo store. The "current" item is the
/// first `in_progress`/`pending` item AFTER the last `completed` one —
/// an early item stuck `in_progress` (its completion refused by the
/// evidence gate and never retried) must not pin the label while later
/// steps advance past it. Seen live 2026-07-16: a 13-minute Telegram
/// build whose label read "step 1/11: Scaffold vite..." from start to
/// finish because items 1-2 were stranded `in_progress`. When nothing
/// active follows the last completed item, falls back to the whole-list
/// scan (first `in_progress`, else first `pending`). `None` when the
/// store is missing, unparseable, or empty — the HUD simply omits the
/// segment.
fn current_todo_step(path: &Path) -> Option<String> {
    let (step_no, total, text) = current_todo_progress(path)?;
    Some(format!("step {step_no}/{total}: {text}"))
}

/// Structured variant of [`current_todo_step`]: `(step_no, total,
/// current item text)`. M22 B3 renders the compact `step N/M` ratio in
/// the frame SUMMARY and the full line in detail from one store read.
fn current_todo_progress(path: &Path) -> Option<(usize, usize, String)> {
    let bytes = std::fs::read(path).ok()?;
    let items: Vec<TodoEntry> = serde_json::from_slice(&bytes).ok()?;
    if items.is_empty() {
        return None;
    }
    let total = items.len();
    let completed = items.iter().filter(|i| i.status == "completed").count();
    let start = items
        .iter()
        .rposition(|i| i.status == "completed")
        .map_or(0, |p| p + 1);
    let is_active = |i: &&TodoEntry| i.status == "in_progress" || i.status == "pending";
    let current = items[start..]
        .iter()
        .find(is_active)
        .or_else(|| items.iter().find(|i| i.status == "in_progress"))
        .or_else(|| items.iter().find(|i| i.status == "pending"))?;
    // Step number = completed + 1 (the one being worked), clamped to total.
    let step_no = (completed + 1).min(total);
    Some((step_no, total, current.text.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_mmss_renders_minutes_and_padded_seconds() {
        assert_eq!(fmt_mmss(0), "0:00");
        assert_eq!(fmt_mmss(9), "0:09");
        assert_eq!(fmt_mmss(83), "1:23");
        assert_eq!(fmt_mmss(600), "10:00");
    }

    // ── F6: richer bare-channel status row + intermediate signal ────────

    #[test]
    fn status_row_carries_todo_step_when_present() {
        let with = compose_status_row(
            65,
            3,
            Some("shell"),
            Some("step 2/5: build the UI".into()),
            None,
        );
        assert!(
            with.contains("step 2/5: build the UI"),
            "bare status must fold in the current todo step: {with}"
        );
        assert!(with.contains("3 tool calls"));
        assert!(with.contains("latest: shell"));
        // No todo store → no step segment, and it stays a clean sentence.
        let without = compose_status_row(65, 1, Some("read_file"), None, None);
        assert!(
            !without.contains("step "),
            "no step clause when absent: {without}"
        );
        assert!(
            without.contains("1 tool call "),
            "singular tool count: {without}"
        );
    }

    #[test]
    fn intermediate_message_fires_past_threshold_not_before() {
        // Before the threshold: the plain "I'll keep going." tail.
        let before = compose_status_row(120, 4, Some("shell"), None, None);
        assert!(before.ends_with("I'll keep going."), "got: {before}");
        assert!(!before.contains("taking longer"), "premature: {before}");
        // At/past the threshold (the ~180s row): the softened reassurance.
        let after = compose_status_row(
            INTERMEDIATE_STATUS_AFTER.as_secs(),
            9,
            Some("cargo"),
            Some("step 3/6: wire it up".into()),
            None,
        );
        assert!(
            after.contains("This is taking longer than usual, but I'm still going."),
            "intermediate signal must fire past the threshold: {after}"
        );
        assert!(
            after.contains("step 3/6: wire it up"),
            "the todo step still rides the intermediate row: {after}"
        );
    }

    #[test]
    fn status_row_appends_note_after_step_clause() {
        // B1: a pending one-shot note (auto-compaction) rides the bare
        // status row, after the todo-step clause and before the closing
        // reassurance sentence.
        let note = "history auto-compacted (14 msgs -> 5, ~180k tokens)";
        let with = compose_status_row(
            65,
            3,
            Some("shell"),
            Some("step 2/5: build the UI".into()),
            Some(note.into()),
        );
        assert!(
            with.contains(&format!("step 2/5: build the UI — {note}. ")),
            "note must follow the step clause and precede the tail: {with}"
        );
        assert!(with.ends_with("I'll keep going."), "tail intact: {with}");
        // No note → byte-identical to the pre-B1 row.
        let without = compose_status_row(
            65,
            3,
            Some("shell"),
            Some("step 2/5: build the UI".into()),
            None,
        );
        assert!(
            !without.contains("auto-compacted"),
            "no note clause when absent: {without}"
        );
    }

    #[test]
    fn cap_chars_truncates_with_ascii_marker() {
        assert_eq!(cap_chars("short", 10), "short");
        let long = "a".repeat(30);
        let capped = cap_chars(&long, 10);
        assert_eq!(capped.chars().count(), 10);
        assert!(capped.ends_with("..."));
    }

    #[test]
    fn current_todo_step_prefers_in_progress_then_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent_todos.json");
        std::fs::write(
            &path,
            r#"[
                {"id":1,"text":"scaffold","status":"completed","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
                {"id":2,"text":"build the UI","status":"in_progress","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
                {"id":3,"text":"ship","status":"pending","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            current_todo_step(&path).as_deref(),
            Some("step 2/3: build the UI")
        );
        // No in_progress: fall back to the first pending.
        std::fs::write(
            &path,
            r#"[
                {"id":1,"text":"scaffold","status":"completed","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
                {"id":3,"text":"ship","status":"pending","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}
            ]"#,
        )
        .unwrap();
        assert_eq!(current_todo_step(&path).as_deref(), Some("step 2/2: ship"));
    }

    #[test]
    fn current_todo_step_skips_items_stranded_before_the_last_completed() {
        // Regression, live 2026-07-16: items 1-2 stayed `in_progress`
        // (their completion was refused by the evidence gate and never
        // retried) while items 3-9 completed — the HUD label read
        // "step 1/11: Scaffold vite..." for the whole 13-minute run.
        // The current item must be the first ACTIVE one after the last
        // completed item, not the first in_progress in the whole list.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent_todos.json");
        std::fs::write(
            &path,
            r#"[
                {"id":1,"text":"scaffold","status":"in_progress","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
                {"id":2,"text":"seed data","status":"in_progress","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
                {"id":3,"text":"build shell","status":"completed","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
                {"id":4,"text":"ship preview","status":"pending","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            current_todo_step(&path).as_deref(),
            Some("step 2/4: ship preview"),
            "stranded early in_progress items must not pin the label"
        );
        // Nothing active after the last completed item: fall back to the
        // whole-list scan so a stuck item still beats showing nothing.
        std::fs::write(
            &path,
            r#"[
                {"id":1,"text":"scaffold","status":"in_progress","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"},
                {"id":2,"text":"build shell","status":"completed","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            current_todo_step(&path).as_deref(),
            Some("step 2/2: scaffold"),
            "fall back to the stranded item when nothing follows the last completed"
        );
    }

    #[test]
    fn pending_note_renders_on_next_frame_only() {
        // The R2/R5 hook: a note set between edits appears on exactly
        // one subsequent frame, then clears.
        let tmp = tempfile::tempdir().unwrap();
        let todo_path = tmp.path().join("agent_todos.json");
        let shared = StdMutex::new(Shared {
            tool_runs: 3,
            activity: Some("last: shell ok".into()),
            note: Some("steering noted".into()),
            posted: true,
            ..Shared::default()
        });
        let spend = TurnSpend::default();
        let started = Instant::now();
        let (frame, first) =
            running_frame(&shared, started, Instant::now(), &todo_path, &spend).unwrap();
        assert!(!first, "HUD was already posted");
        let summary = frame.summary.as_deref().unwrap();
        assert!(
            summary.contains("steering noted"),
            "note must ride the next edit; got: {summary}"
        );
        // Next frame: the note is one-shot.
        let (frame2, _) =
            running_frame(&shared, started, Instant::now(), &todo_path, &spend).unwrap();
        assert!(
            !frame2
                .summary
                .as_deref()
                .unwrap()
                .contains("steering noted"),
            "note must clear after one render"
        );
    }

    #[test]
    fn running_frame_stops_after_finalization() {
        let tmp = tempfile::tempdir().unwrap();
        let todo_path = tmp.path().join("agent_todos.json");
        let shared = StdMutex::new(Shared {
            tool_runs: 1,
            posted: true,
            finalized: true,
            ..Shared::default()
        });
        let spend = TurnSpend::default();
        assert!(
            running_frame(&shared, Instant::now(), Instant::now(), &todo_path, &spend).is_none(),
            "no frame may be composed after the final collapse"
        );
    }

    #[test]
    fn redundant_edit_is_suppressed_but_changed_edit_still_emits() {
        // The H1/R6 no-op-edit guard: two consecutive emits of identical
        // rendered content produce only ONE edit (the second is
        // suppressed), while a changed frame still edits. This is what
        // keeps the pinned Task HUD from firing an `editMessageText` that
        // Telegram would 400 with "message is not modified".
        let shared = StdMutex::new(Shared::default());
        let frame_a = Breadcrumb {
            tool_name: TASK_HUD_TOOL.to_owned(),
            detail: Some("step 1/2: build the UI".into()),
            status: BreadcrumbStatus::Running,
            summary: Some("2 tool calls | 0:30".into()),
            steps: Vec::new(),
        };
        // First post always emits (it opens the message, not an edit).
        assert!(record_and_should_emit(&shared, &frame_a, true));
        // An edit with byte-identical content is a no-op — suppress it.
        assert!(
            !record_and_should_emit(&shared, &frame_a, false),
            "a byte-identical edit must be suppressed"
        );
        // A frame with changed content still edits.
        let frame_b = Breadcrumb {
            summary: Some("3 tool calls | 0:31".into()),
            ..frame_a.clone()
        };
        assert!(
            record_and_should_emit(&shared, &frame_b, false),
            "a changed frame must still edit"
        );
        // ...and repeating that same changed frame is suppressed again.
        assert!(
            !record_and_should_emit(&shared, &frame_b, false),
            "a repeat of the last-emitted frame must be suppressed"
        );
    }

    #[test]
    fn first_post_is_never_suppressed_even_if_repeated() {
        // A `first: true` emit opens a fresh message, never an edit, so it
        // must always go through regardless of the recorded fingerprint.
        let shared = StdMutex::new(Shared::default());
        let frame = Breadcrumb {
            tool_name: TASK_HUD_TOOL.to_owned(),
            detail: None,
            status: BreadcrumbStatus::Running,
            summary: Some("1 tool call | 0:05".into()),
            steps: Vec::new(),
        };
        assert!(record_and_should_emit(&shared, &frame, true));
        assert!(
            record_and_should_emit(&shared, &frame, true),
            "a first post is always emitted"
        );
    }

    #[test]
    fn current_todo_step_none_when_missing_or_all_done() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agent_todos.json");
        assert!(current_todo_step(&path).is_none(), "missing file");
        std::fs::write(&path, "not json").unwrap();
        assert!(current_todo_step(&path).is_none(), "unparseable");
        std::fs::write(
            &path,
            r#"[{"id":1,"text":"done thing","status":"completed","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}]"#,
        )
        .unwrap();
        assert!(current_todo_step(&path).is_none(), "all completed");
    }

    // ── F5: pre-first-tool / pure-reasoning "thinking…" HUD frame ───────

    use crate::run::RunnerDeps;
    use crate::tools::{OriginatingRouting, RunnerToolCtx};
    use async_trait::async_trait;
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_db::tables::messages_out;
    use copperclaw_providers::{AgentProvider, AgentQuery, ProviderError, QueryInput};
    use copperclaw_types::{AgentGroupId, MessageKind, ProviderEvent, SessionId};
    use rusqlite::Connection;
    use tokio::sync::Mutex;

    struct NoopProvider;

    #[async_trait]
    impl AgentProvider for NoopProvider {
        fn name(&self) -> &'static str {
            "noop"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            Ok(Box::new(NoopQuery))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct NoopQuery;

    #[async_trait]
    impl AgentQuery for NoopQuery {
        async fn push(&mut self, _: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            None
        }
        async fn abort(&mut self) {}
    }

    /// Build `RunnerDeps` whose originating channel is `channel_type`
    /// (set on the ctx the way the runner does from the inbound row) and
    /// whose `hud_mode` is `mode`. Returns the shared outbound handle so
    /// the test can inspect the HUD rows the ctx writes.
    fn deps_for_channel(
        channel_type: &str,
        mode: crate::config::HudMode,
    ) -> (tempfile::TempDir, Arc<Mutex<Connection>>, RunnerDeps) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let ctx = RunnerToolCtx::new(outbound.clone(), paths.outbox.clone());
        ctx.set_originating(OriginatingRouting {
            channel_type: Some(channel_type.to_string()),
            platform_id: Some("chat-1".into()),
            thread_id: None,
            in_reply_to: None,
            source_session_id: None,
        });
        let tool_ctx: Arc<dyn ToolContext> = Arc::new(ctx);
        let provider: Arc<dyn AgentProvider> = Arc::new(NoopProvider);
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps =
            RunnerDeps::minimal(provider, tool_ctx, inbound, outbound.clone(), archive_dir);
        deps.hud_mode = mode;
        (tmp, outbound, deps)
    }

    async fn hud_rows(outbound: &Arc<Mutex<Connection>>) -> Vec<copperclaw_types::MessageOutRow> {
        let conn = outbound.lock().await;
        messages_out::list_due(&conn).unwrap()
    }

    /// Yield repeatedly so the spawned HUD task can run its emit (which
    /// awaits the outbound lock + a sqlite write) under paused time.
    async fn settle() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn thinking_frame_posts_after_threshold_then_finalizes() {
        // A 20s pure-reasoning turn on an edit-capable channel: nothing
        // posts before the threshold, a "thinking…" frame posts once past
        // it, and finalize collapses that frame (even though zero tools
        // ran) instead of leaving it dangling.
        let (_tmp, outbound, deps) =
            deps_for_channel("telegram", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        assert_eq!(hud.behavior, Behavior::Live, "telegram is edit-capable");
        hud.arm();
        // Let the spawned task run up to its first sleep so its timer is
        // registered before we advance the paused clock.
        tokio::task::yield_now().await;

        // Before the threshold: still silent.
        tokio::time::advance(THINKING_THRESHOLD - Duration::from_secs(1)).await;
        settle().await;
        assert!(
            hud_rows(&outbound).await.is_empty(),
            "no HUD frame may post before the thinking threshold"
        );

        // Past the threshold: exactly one "thinking…" Breadcrumb post.
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        let rows = hud_rows(&outbound).await;
        let posts: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == MessageKind::Breadcrumb)
            .collect();
        assert_eq!(
            posts.len(),
            1,
            "one thinking frame; got {} rows",
            rows.len()
        );
        let bc: Breadcrumb =
            serde_json::from_value(posts[0].content["breadcrumb"].clone()).unwrap();
        assert!(
            bc.summary.as_deref().unwrap().starts_with("thinking…"),
            "pre-first-tool frame reads as thinking…; got {:?}",
            bc.summary
        );

        // Simulate the ~20s reasoning wait, then finalize a zero-tool turn.
        tokio::time::advance(Duration::from_secs(13)).await;
        hud.finalize(true).await;
        let rows = hud_rows(&outbound).await;
        let collapse = rows
            .iter()
            .filter(|r| r.kind == MessageKind::System)
            .find_map(|r| {
                serde_json::from_value::<Breadcrumb>(
                    r.content["update_breadcrumb"]["breadcrumb"].clone(),
                )
                .ok()
            })
            .expect("finalize must collapse the thinking frame on a zero-tool turn");
        assert_eq!(collapse.status, BreadcrumbStatus::Done);
        assert!(
            collapse.summary.as_deref().unwrap().starts_with("done in"),
            "zero-tool collapse omits the tool count; got {:?}",
            collapse.summary
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fast_pure_reasoning_turn_posts_nothing() {
        // A turn that finalizes before the threshold (a quick answer) must
        // post no HUD frame at all — byte-stable with today's behaviour.
        let (_tmp, outbound, deps) =
            deps_for_channel("telegram", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        hud.arm();
        tokio::task::yield_now().await;
        // Finish well under the threshold.
        tokio::time::advance(Duration::from_secs(3)).await;
        hud.finalize(true).await;
        // Even if the armed task's timer would have fired later, it was
        // aborted by finalize — advancing past the threshold posts nothing.
        tokio::time::advance(THINKING_THRESHOLD).await;
        settle().await;
        assert!(
            hud_rows(&outbound).await.is_empty(),
            "a sub-threshold turn must post nothing"
        );
    }

    // ── S6: the test-clock seam pins the StatusRows timed legs ──────────
    //
    // Before M21 S6 these legs were unfixturable (the M18 X2 known gap):
    // `maybe_emit_status_row` read `Instant::elapsed()` directly, so the
    // 60s first-fire needed a real 60-second wall-clock wait. With the
    // injected TestClock the whole cadence is traversed deterministically.

    use crate::clock::TestClock;

    #[tokio::test]
    async fn status_rows_60s_first_fire_and_150s_softening_pinned_by_test_clock() {
        // webhooks has no message-edit API and no client-side transcript
        // renderer, so the HUD degrades to the bare-channel StatusRows
        // behaviour under the default hud_mode. (Before M22 D5 this test
        // rode cli, which now takes the Transcript path.)
        let (_tmp, outbound, mut deps) =
            deps_for_channel("webhooks", crate::config::HudMode::default());
        let clock = TestClock::new();
        deps.clock = Arc::new(clock.clone());
        let hud = TaskHud::new(&deps);
        assert_eq!(
            hud.behavior,
            Behavior::StatusRows,
            "webhooks is a bare channel"
        );

        // 30s in: a finishing batch is under the 60s cadence — silence.
        clock.advance(Duration::from_secs(30));
        hud.on_batch_end(1, Some("shell"), true).await;
        assert!(
            hud_rows(&outbound).await.is_empty(),
            "no status row may fire before the 60s interval"
        );

        // 61s in: the first status row fires with the plain tail.
        clock.advance(Duration::from_secs(31));
        hud.on_batch_end(2, Some("shell"), true).await;
        let rows = hud_rows(&outbound).await;
        assert_eq!(rows.len(), 1, "exactly one row at the 60s first fire");
        assert_eq!(rows[0].kind, MessageKind::Chat, "status rows are Chat rows");
        let text = rows[0].content["text"].as_str().unwrap();
        assert!(
            text.contains("61s in") && text.contains("2 tool calls"),
            "elapsed + count read the test clock/state: {text}"
        );
        assert!(
            text.ends_with("I'll keep going."),
            "plain tail pre-150s: {text}"
        );
        assert!(
            !text.contains("taking longer"),
            "no premature softening: {text}"
        );

        // 91s in — only 30s after the last emit: the cadence anchor was
        // bumped at the first fire, so this batch stays silent.
        clock.advance(Duration::from_secs(30));
        hud.on_batch_end(3, Some("shell"), true).await;
        assert_eq!(
            hud_rows(&outbound).await.len(),
            1,
            "the 60s cadence must hold between rows"
        );

        // 151s in: due again AND past INTERMEDIATE_STATUS_AFTER — the
        // row softens to the "taking longer than usual" reassurance.
        clock.advance(Duration::from_secs(60));
        hud.on_batch_end(4, Some("cargo"), true).await;
        let rows = hud_rows(&outbound).await;
        assert_eq!(rows.len(), 2, "second row at the softened 150s leg");
        let text = rows[1].content["text"].as_str().unwrap();
        assert!(
            text.contains("151s in"),
            "softened row reads the clock: {text}"
        );
        assert!(
            text.contains("This is taking longer than usual, but I'm still going."),
            "past 150s the reassurance softens: {text}"
        );
    }

    #[test]
    fn hud_elapsed_reads_the_injected_clock() {
        // `TaskHud::elapsed` feeds the run loop's timed gates (the R6
        // progressive-final >30s check); it must advance with the seam.
        let (_tmp, _outbound, mut deps) =
            deps_for_channel("cli", crate::config::HudMode::default());
        let clock = TestClock::new();
        deps.clock = Arc::new(clock.clone());
        let hud = TaskHud::new(&deps);
        assert_eq!(hud.elapsed(), Duration::ZERO, "no advance -> no elapsed");
        clock.advance(Duration::from_secs(45));
        assert_eq!(hud.elapsed(), Duration::from_secs(45));
    }

    #[tokio::test(start_paused = true)]
    async fn hud_mode_off_posts_no_thinking_frame() {
        // hud_mode=off degrades to status rows: arm is a no-op and no
        // thinking frame posts however long the turn reasons.
        let (_tmp, outbound, deps) = deps_for_channel("telegram", crate::config::HudMode::Off);
        let hud = TaskHud::new(&deps);
        assert_eq!(hud.behavior, Behavior::StatusRows);
        hud.arm();
        tokio::time::advance(THINKING_THRESHOLD + Duration::from_secs(30)).await;
        settle().await;
        assert!(
            hud_rows(&outbound).await.is_empty(),
            "hud_mode=off must never post a thinking frame"
        );
    }

    // ── M22 B1: loop-level notices drain into the one-shot note slot ────

    #[test]
    fn new_drains_one_queued_notice_into_pending_note() {
        // TaskHud::new adopts exactly ONE queued notice (FIFO) as the
        // turn's pending note; further notices wait for later turns.
        let (_tmp, _outbound, deps) =
            deps_for_channel("telegram", crate::config::HudMode::default());
        deps.notices
            .push("history auto-compacted (14 msgs -> 5, ~180k tokens)");
        deps.notices.push("second notice");
        let hud = TaskHud::new(&deps);
        assert_eq!(
            hud.note_for_test().as_deref(),
            Some("history auto-compacted (14 msgs -> 5, ~180k tokens)"),
            "the oldest queued notice becomes the pending one-shot note"
        );
        assert_eq!(
            deps.notices.drain_one().as_deref(),
            Some("second notice"),
            "only one notice may be drained per turn"
        );
    }

    #[test]
    fn queued_notice_renders_on_next_frame_only() {
        // The drained notice rides the existing one-shot machinery:
        // exactly one live frame carries it, then it clears (the mirror
        // of pending_note_renders_on_next_frame_only for the B1 path).
        let tmp = tempfile::tempdir().unwrap();
        let todo_path = tmp.path().join("agent_todos.json");
        let (_t, _outbound, deps) = deps_for_channel("telegram", crate::config::HudMode::default());
        deps.notices
            .push("history auto-compacted (14 msgs -> 5, ~180k tokens)");
        let hud = TaskHud::new(&deps);
        let started = Instant::now();
        let (frame, first) =
            running_frame(&hud.shared, started, Instant::now(), &todo_path, &hud.spend)
                .expect("live frame composes");
        assert!(first, "nothing posted yet");
        assert!(
            frame
                .summary
                .as_deref()
                .unwrap()
                .contains("history auto-compacted (14 msgs -> 5, ~180k tokens)"),
            "the drained notice must ride the first frame; got {:?}",
            frame.summary
        );
        // Next frame: one-shot, cleared.
        let (frame2, _) =
            running_frame(&hud.shared, started, Instant::now(), &todo_path, &hud.spend).unwrap();
        assert!(
            !frame2
                .summary
                .as_deref()
                .unwrap()
                .contains("auto-compacted"),
            "the notice must clear after one render; got {:?}",
            frame2.summary
        );
    }

    #[tokio::test]
    async fn finalize_requeues_a_notice_the_turn_never_rendered() {
        // A fast pure-text turn posts no frame at all — the drained
        // notice must go BACK to the queue for a later turn instead of
        // being dropped on the floor.
        let (_tmp, outbound, deps) =
            deps_for_channel("telegram", crate::config::HudMode::default());
        deps.notices
            .push("history auto-compacted (14 msgs -> 5, ~180k tokens)");
        let hud = TaskHud::new(&deps);
        hud.finalize(true).await;
        assert!(
            hud_rows(&outbound).await.is_empty(),
            "a zero-tool unposted turn emits nothing"
        );
        assert_eq!(
            deps.notices.drain_one().as_deref(),
            Some("history auto-compacted (14 msgs -> 5, ~180k tokens)"),
            "an unrendered notice must be requeued at finalize"
        );
    }

    // ── M22 B3/B4: summary fields, rounding, and the four-field cap ─────

    #[test]
    fn summary_orders_head_ratio_tokens_cost() {
        // Head (tools + clock) keeps its pre-M22 bytes; ratio, tokens
        // and cost append in that order, ` | `-joined.
        let spend = SpendView {
            tokens: 3_237,
            cost_micros: 120_000, // $0.12
            all_priced: true,
        };
        let s = compose_summary(6, false, "1:47", Some("step 2/5"), spend, None);
        assert_eq!(s, "6 tool calls | 1:47 | step 2/5 | 3.2k tokens");
        // Without a ratio the cost fits inside the four-field cap.
        let s = compose_summary(6, false, "1:47", None, spend, None);
        assert_eq!(s, "6 tool calls | 1:47 | 3.2k tokens | $0.12");
        // No new fields at all: byte-identical to the pre-M22 summary.
        let s = compose_summary(3, false, "0:42", None, SpendView::default(), None);
        assert_eq!(s, "3 tool calls | 0:42");
        let s = compose_summary(0, true, "0:07", None, SpendView::default(), None);
        assert_eq!(s, "thinking… | 0:07");
    }

    #[test]
    fn summary_caps_at_four_fields_dropping_cost_then_tokens() {
        let spend = SpendView {
            tokens: 3_237,
            cost_micros: 120_000,
            all_priced: true,
        };
        // All five candidates: cost is dropped first.
        let s = compose_summary(6, false, "1:47", Some("step 2/5"), spend, None);
        assert!(!s.contains('$'), "cost must drop first at the cap: {s}");
        assert_eq!(s.split(" | ").count(), 4, "capped at four fields: {s}");
        // A pending note is never dropped and tightens the budget by
        // one: tokens go next, the ratio survives.
        let s = compose_summary(
            6,
            false,
            "1:47",
            Some("step 2/5"),
            spend,
            Some("extended tool budget (4/8 turns)"),
        );
        assert_eq!(
            s,
            "6 tool calls | 1:47 | step 2/5 | extended tool budget (4/8 turns)"
        );
        assert_eq!(s.split(" | ").count(), 4, "note counts toward the cap: {s}");
        assert!(!s.contains("tokens"), "tokens drop after cost: {s}");
    }

    #[test]
    fn summary_length_bounded_across_matrix() {
        // Every combination in the matrix stays comfortably scannable on
        // mobile: <= 100 chars, well under MAX_SUMMARY_CHARS (200).
        let spends = [
            SpendView::default(),
            SpendView {
                tokens: 850,
                cost_micros: 9_800,
                all_priced: true,
            },
            SpendView {
                tokens: u64::MAX,
                cost_micros: u64::MAX,
                all_priced: true,
            },
        ];
        let notes = [
            None,
            Some("extended tool budget (900/900 turns)"),
            Some("history auto-compacted (14 msgs -> 5, ~180k tokens)"),
        ];
        let ratios = [None, Some("step 2/5"), Some("step 99/99")];
        for tool_runs in [0usize, 1, 9_999] {
            for spend in spends {
                for note in notes {
                    for ratio in ratios {
                        let s = compose_summary(
                            tool_runs,
                            tool_runs == 0,
                            "999:59",
                            ratio,
                            spend,
                            note,
                        );
                        assert!(
                            s.chars().count() <= 100,
                            "summary must stay <= 100 chars ({} for {s:?})",
                            s.chars().count()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn tokens_round_to_nearest_hundred() {
        assert_eq!(fmt_tokens(0), None, "zero recorded tokens render nothing");
        assert_eq!(fmt_tokens(49), None, "sub-50 rounds to zero -> omitted");
        assert_eq!(fmt_tokens(50), Some("100 tokens".into()));
        assert_eq!(fmt_tokens(800), Some("800 tokens".into()));
        assert_eq!(fmt_tokens(3_237), Some("3.2k tokens".into()));
        assert_eq!(fmt_tokens(3_160), Some("3.2k tokens".into()));
        assert_eq!(fmt_tokens(38_049), Some("38k tokens".into()));
        assert_eq!(fmt_tokens(1_000), Some("1k tokens".into()));
    }

    #[test]
    fn cost_renders_only_when_all_calls_priced_and_nonzero() {
        assert_eq!(fmt_cost(120_000, true), Some("$0.12".into()));
        assert_eq!(fmt_cost(1_234_999, true), Some("$1.23".into()));
        // Unknown model somewhere in the run: omit entirely, never $0.00.
        assert_eq!(fmt_cost(120_000, false), None);
        // Sub-half-cent (and ollama's genuinely-zero cost): omitted.
        assert_eq!(fmt_cost(4_000, true), None);
        assert_eq!(fmt_cost(0, true), None);
    }

    #[test]
    fn turn_spend_prices_known_models_and_flags_unknown() {
        let spend = TurnSpend::default();
        // Sonnet list price $3/$15 per MTok: 900 in + 100 out = 4200 micros.
        spend.record("anthropic", "claude-sonnet-4-6", 900, 100);
        let v = spend.view();
        assert_eq!(v.tokens, 1_000);
        assert_eq!(v.cost_micros, 4_200);
        assert!(v.all_priced, "a priced-only run renders cost");
        // An unpriced call poisons the cost (a known undercount) but
        // still counts its tokens.
        spend.record("openrouter", "some-unknown-model", 100, 0);
        let v = spend.view();
        assert_eq!(v.tokens, 1_100);
        assert!(!v.all_priced, "any unpriced call must suppress the cost");
        // Zero-token calls (no usage surfaced) are complete no-ops.
        let clean = TurnSpend::default();
        clean.record("openrouter", "some-unknown-model", 0, 0);
        assert_eq!(clean.view(), SpendView::default());
        // reset() restores the pristine state.
        spend.reset();
        assert_eq!(spend.view(), SpendView::default());
    }

    // ── M22 fingerprint stability (the binding Verification gates) ──────

    #[test]
    fn old_shape_frame_fingerprints_identically_to_pre_m22() {
        // (a) A frame with `steps` empty and every new field absent must
        // serialize byte-identically to the pre-M22 shape — serde's
        // skip_serializing_if defaults keep old frames byte-stable, so
        // no spurious in-place edit fires on upgrade.
        let tmp = tempfile::tempdir().unwrap();
        let todo_path = tmp.path().join("agent_todos.json");
        let shared = StdMutex::new(Shared {
            tool_runs: 3,
            activity: Some("last: shell ok".into()),
            posted: true,
            ..Shared::default()
        });
        let spend = TurnSpend::default();
        let now = Instant::now();
        let (frame, _) = running_frame(&shared, now, now, &todo_path, &spend).unwrap();
        assert_eq!(
            frame_fingerprint(&frame),
            r#"{"tool_name":"task","detail":"last: shell ok","status":"running","summary":"3 tool calls | 0:00"}"#,
            "old-shape frames must fingerprint exactly as before M22"
        );
    }

    #[test]
    fn sub_rounding_token_changes_fingerprint_identically() {
        // (b) Two frames differing only in sub-rounding token counts
        // (3,160 vs 3,240 -> both "3.2k") must fingerprint identically,
        // so B4's rounding suppresses the no-op edit that
        // record_and_should_emit would otherwise let through.
        let tmp = tempfile::tempdir().unwrap();
        let todo_path = tmp.path().join("agent_todos.json");
        let now = Instant::now();
        let frame_for = |tokens: u64, micros: u64| {
            let shared = StdMutex::new(Shared {
                tool_runs: 3,
                activity: Some("last: shell ok".into()),
                posted: true,
                ..Shared::default()
            });
            let spend = TurnSpend::default();
            spend.tokens.store(tokens, Ordering::Relaxed);
            spend.cost_micros.store(micros, Ordering::Relaxed);
            spend.priced_any.store(true, Ordering::Relaxed);
            let (frame, _) = running_frame(&shared, now, now, &todo_path, &spend).unwrap();
            frame_fingerprint(&frame)
        };
        // Both cost figures round to the same cent as well.
        assert_eq!(
            frame_for(3_160, 9_480),
            frame_for(3_240, 9_720),
            "sub-rounding token deltas must not change the fingerprint"
        );
    }

    // ── M22 B5: the bounded step transcript ─────────────────────────────

    #[test]
    fn record_step_captures_detail_status_and_result_summary() {
        let (_tmp, _outbound, deps) =
            deps_for_channel("telegram", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        hud.record_step(
            "shell",
            &serde_json::json!({"command": "echo step one"}),
            "step one\nsecond line ignored",
            false,
        );
        hud.record_step(
            "read_file",
            &serde_json::json!({"path": "/data/missing.txt"}),
            "no such file: /data/missing.txt",
            true,
        );
        let s = hud.shared.lock().unwrap();
        assert_eq!(s.steps.len(), 2);
        assert_eq!(s.steps[0].tool_name, "shell");
        assert_eq!(s.steps[0].detail.as_deref(), Some("echo step one"));
        assert_eq!(s.steps[0].status, BreadcrumbStatus::Done);
        assert_eq!(
            s.steps[0].summary.as_deref(),
            Some("step one"),
            "the step summary is the result's first non-empty line"
        );
        assert_eq!(s.steps[1].status, BreadcrumbStatus::Failed);
        assert!(s.steps[1].steps.is_empty(), "steps never nest");
    }

    #[test]
    fn step_result_summary_unwraps_json_envelopes() {
        // The shell tool's JSON envelope surfaces stdout, not "{".
        let shell = r#"{"command":"echo step one","exit_code":0,"stdout":"step one\n","stderr":"","elapsed_ms":4}"#;
        assert_eq!(step_result_summary(shell).as_deref(), Some("step one"));
        // Silent command: deterministic exit code, never elapsed_ms.
        let silent = r#"{"command":"true","exit_code":0,"stdout":"","stderr":"","elapsed_ms":9}"#;
        assert_eq!(step_result_summary(silent).as_deref(), Some("exit 0"));
        // Failed command with only stderr populated.
        let failed = r#"{"command":"x","exit_code":1,"stdout":"","stderr":"x: not found\n"}"#;
        assert_eq!(step_result_summary(failed).as_deref(), Some("x: not found"));
        // Plain-text result: first non-empty line, capped.
        assert_eq!(
            step_result_summary("\n  hello world  \nsecond").as_deref(),
            Some("hello world")
        );
        assert_eq!(step_result_summary(""), None);
        let long = "a".repeat(400);
        let capped = step_result_summary(&long).unwrap();
        assert_eq!(capped.chars().count(), STEP_SUMMARY_CHARS);
    }

    #[test]
    fn record_step_keeps_newest_forty() {
        let (_tmp, _outbound, deps) =
            deps_for_channel("telegram", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        for i in 0..45 {
            hud.record_step(
                "shell",
                &serde_json::json!({"command": format!("echo {i}")}),
                &format!("{i}"),
                false,
            );
        }
        let s = hud.shared.lock().unwrap();
        assert_eq!(s.steps.len(), MAX_TRANSCRIPT_STEPS);
        assert_eq!(
            s.steps[0].detail.as_deref(),
            Some("echo 5"),
            "the oldest steps are dropped, keeping the newest window"
        );
        assert_eq!(s.steps.last().unwrap().detail.as_deref(), Some("echo 44"));
    }

    #[test]
    fn running_frame_attaches_accumulated_steps() {
        let tmp = tempfile::tempdir().unwrap();
        let todo_path = tmp.path().join("agent_todos.json");
        let (_t, _outbound, deps) = deps_for_channel("telegram", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        hud.record_step(
            "shell",
            &serde_json::json!({"command": "cargo check"}),
            "ok",
            false,
        );
        let now = Instant::now();
        let (frame, _) = running_frame(&hud.shared, now, now, &todo_path, &hud.spend).unwrap();
        assert_eq!(frame.steps.len(), 1, "frames carry the step transcript");
        assert_eq!(frame.steps[0].tool_name, "shell");
        assert_eq!(frame.steps[0].detail.as_deref(), Some("cargo check"));
    }

    #[tokio::test]
    async fn finalize_collapse_keeps_steps_and_appends_spend() {
        // The done-collapse keeps the step transcript attached and
        // appends the task's spend in the line's comma style.
        let (_tmp, outbound, deps) =
            deps_for_channel("telegram", crate::config::HudMode::default());
        deps.spend
            .record("anthropic", "claude-sonnet-4-6", 37_000, 1_049);
        let hud = TaskHud::new(&deps);
        hud.record_step(
            "shell",
            &serde_json::json!({"command": "cargo check"}),
            "ok",
            false,
        );
        hud.on_batch_start(&[]).await;
        hud.on_batch_end(1, Some("shell"), true).await;
        hud.finalize(true).await;
        let rows = hud_rows(&outbound).await;
        let collapse = rows
            .iter()
            .filter(|r| r.kind == MessageKind::System)
            .filter_map(|r| {
                serde_json::from_value::<Breadcrumb>(
                    r.content["update_breadcrumb"]["breadcrumb"].clone(),
                )
                .ok()
            })
            .find(|b| b.status == BreadcrumbStatus::Done)
            .expect("finalize collapse row");
        assert_eq!(collapse.steps.len(), 1, "the collapse keeps the transcript");
        let summary = collapse.summary.as_deref().unwrap();
        assert!(
            summary.contains("1 tool call, 38k tokens, $0.13"),
            "the collapse carries rounded spend: {summary}"
        );
    }

    #[tokio::test]
    async fn status_row_carries_queued_notice_exactly_once() {
        // Bare channels: the drained notice rides the first due status
        // row and never a later one. (webhooks, not cli — M22 D5 moved
        // cli onto the Transcript path.)
        let (_tmp, outbound, mut deps) =
            deps_for_channel("webhooks", crate::config::HudMode::default());
        let clock = TestClock::new();
        deps.clock = Arc::new(clock.clone());
        deps.notices
            .push("history auto-compacted (14 msgs -> 5, ~180k tokens)");
        let hud = TaskHud::new(&deps);
        assert_eq!(
            hud.behavior,
            Behavior::StatusRows,
            "webhooks is a bare channel"
        );

        clock.advance(Duration::from_secs(61));
        hud.on_batch_end(1, Some("shell"), true).await;
        let rows = hud_rows(&outbound).await;
        assert_eq!(rows.len(), 1, "first due status row fires");
        let text = rows[0].content["text"].as_str().unwrap();
        assert!(
            text.contains("history auto-compacted (14 msgs -> 5, ~180k tokens)"),
            "the notice rides the first status row: {text}"
        );

        clock.advance(Duration::from_secs(61));
        hud.on_batch_end(2, Some("shell"), true).await;
        let rows = hud_rows(&outbound).await;
        assert_eq!(rows.len(), 2, "second status row fires on cadence");
        let text = rows[1].content["text"].as_str().unwrap();
        assert!(
            !text.contains("auto-compacted"),
            "the notice is one-shot — it must not repeat: {text}"
        );
    }

    // ── M22 D5: the cli Transcript path (frames as events) ──────────────

    #[test]
    fn behavior_routes_edit_capable_transcript_and_bare_channels() {
        // The D5 precedence: a real edit API wins (Live), then a
        // client-side transcript renderer (Transcript), then the bare
        // status-row fallback.
        let (_t1, _o1, deps) = deps_for_channel("telegram", crate::config::HudMode::default());
        assert_eq!(TaskHud::new(&deps).behavior, Behavior::Live);
        let (_t2, _o2, deps) = deps_for_channel("cli", crate::config::HudMode::default());
        assert_eq!(TaskHud::new(&deps).behavior, Behavior::Transcript);
        let (_t3, _o3, deps) = deps_for_channel("webhooks", crate::config::HudMode::default());
        assert_eq!(TaskHud::new(&deps).behavior, Behavior::StatusRows);
        // hud_mode still governs the transcript leg like the edit leg:
        // `final` collapses to the one-liner, `off` degrades fully.
        let (_t4, _o4, deps) = deps_for_channel("cli", crate::config::HudMode::Final);
        assert_eq!(TaskHud::new(&deps).behavior, Behavior::FinalOnly);
        let (_t5, _o5, deps) = deps_for_channel("cli", crate::config::HudMode::Off);
        assert_eq!(TaskHud::new(&deps).behavior, Behavior::StatusRows);
    }

    #[tokio::test]
    async fn transcript_emits_fresh_breadcrumb_events_never_edits() {
        // Every Transcript frame is a fresh MessageKind::Breadcrumb row
        // (an event the client dedupes); no update_breadcrumb System
        // row — the edit transport — may ever appear for cli.
        let (_tmp, outbound, deps) = deps_for_channel("cli", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        hud.record_step(
            "shell",
            &serde_json::json!({"command": "cargo check"}),
            "ok",
            false,
        );
        hud.on_batch_end(1, Some("shell"), true).await;
        hud.on_batch_end(2, Some("read_file"), true).await;
        let rows = hud_rows(&outbound).await;
        assert_eq!(rows.len(), 2, "one event per changed frame");
        for row in &rows {
            assert_eq!(
                row.kind,
                MessageKind::Breadcrumb,
                "transcript frames are fresh Breadcrumb events, never System edits"
            );
        }
        let bc: Breadcrumb = serde_json::from_value(rows[1].content["breadcrumb"].clone()).unwrap();
        assert_eq!(
            bc.steps.len(),
            1,
            "frames carry the cumulative step transcript"
        );
        assert!(
            bc.summary.as_deref().unwrap().starts_with("2 tool calls"),
            "frames carry the cumulative summary: {:?}",
            bc.summary
        );
    }

    #[tokio::test]
    async fn transcript_cadence_suppresses_byte_identical_frames() {
        // The record_and_should_emit discipline holds on the Transcript
        // path: a frame byte-identical to the last one emitted is NOT
        // appended again (a chatty run must not flood the log), while a
        // changed frame still emits. The clock is frozen so the elapsed
        // field cannot differ between the two emits.
        let (_tmp, outbound, mut deps) = deps_for_channel("cli", crate::config::HudMode::default());
        deps.clock = Arc::new(TestClock::new());
        let hud = TaskHud::new(&deps);
        hud.on_batch_end(1, Some("shell"), true).await;
        assert_eq!(hud_rows(&outbound).await.len(), 1, "first frame emits");
        // Identical state, identical clock: suppressed.
        hud.on_batch_end(1, Some("shell"), true).await;
        assert_eq!(
            hud_rows(&outbound).await.len(),
            1,
            "a byte-identical transcript frame must be suppressed"
        );
        // Changed state still emits.
        hud.on_batch_end(2, Some("shell"), true).await;
        assert_eq!(
            hud_rows(&outbound).await.len(),
            2,
            "a changed transcript frame must still emit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transcript_arm_posts_no_ticker_thinking_frame() {
        // The wall-clock ticker (and its F5 thinking frame) stays
        // Live-only: on an append-only log every tick would be a
        // permanent line, so arming the HUD on cli must post nothing
        // however long the turn reasons — the client repaints the
        // elapsed clock itself.
        let (_tmp, outbound, deps) = deps_for_channel("cli", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        assert_eq!(hud.behavior, Behavior::Transcript);
        hud.arm();
        tokio::time::advance(THINKING_THRESHOLD + Duration::from_secs(60)).await;
        settle().await;
        assert!(
            hud_rows(&outbound).await.is_empty(),
            "no ticker frames may post on the transcript path"
        );
    }

    #[tokio::test]
    async fn transcript_finalize_appends_the_collapse_event() {
        // finalize on the Transcript path appends the done-collapse as a
        // fresh Breadcrumb event (steps attached), never a System edit.
        let (_tmp, outbound, deps) = deps_for_channel("cli", crate::config::HudMode::default());
        let hud = TaskHud::new(&deps);
        hud.record_step(
            "shell",
            &serde_json::json!({"command": "cargo check"}),
            "ok",
            false,
        );
        hud.on_batch_end(1, Some("shell"), true).await;
        hud.finalize(true).await;
        let rows = hud_rows(&outbound).await;
        assert!(
            rows.iter().all(|r| r.kind == MessageKind::Breadcrumb),
            "no System (edit) row may appear on the transcript path"
        );
        let collapse: Breadcrumb =
            serde_json::from_value(rows.last().unwrap().content["breadcrumb"].clone()).unwrap();
        assert_eq!(collapse.status, BreadcrumbStatus::Done);
        assert!(
            collapse.summary.as_deref().unwrap().starts_with("done in"),
            "the collapse event carries the final line: {:?}",
            collapse.summary
        );
        assert_eq!(collapse.steps.len(), 1, "the collapse keeps the transcript");
    }
}
