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
//! Surfaces where the platform draws no typing indicator at all
//! (`capabilities::typing_indicator_visible` — e.g. Slack outside an
//! assistant thread) force full HUD behaviour and a tighter edit
//! cadence, since the HUD is then the user's ONLY working signal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use copperclaw_channels_core::{
    Breadcrumb, BreadcrumbStatus, MAX_DETAIL_CHARS, MAX_SUMMARY_CHARS, capabilities,
};
use copperclaw_mcp::ToolContext;

use super::RunnerDeps;
use super::drive_turn::PendingToolCall;
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

/// How this inbound's HUD behaves, resolved once at `drive_turn` entry
/// from the configured [`HudMode`] and the originating channel's static
/// capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Live self-editing HUD (post at first tool call, edit per batch +
    /// ticker, collapse on finalize).
    Live,
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
                if capabilities::supports_message_edit(&oc.channel_type) {
                    let typing_visible = capabilities::typing_indicator_visible(
                        &oc.channel_type,
                        oc.platform_id.as_deref().unwrap_or(""),
                        oc.thread_id.as_deref(),
                    );
                    // No typing signal on this surface: the HUD is the
                    // only working indicator, so `final` is forced back
                    // to the live HUD.
                    if mode == HudMode::Final && typing_visible {
                        Behavior::FinalOnly
                    } else {
                        Behavior::Live
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
        let now = Instant::now();
        Self {
            behavior,
            answer_edit_capable,
            agent_group: deps.agent_group_id.to_string(),
            started_at: now,
            shared: Arc::new(StdMutex::new(Shared::default())),
            ctx: deps.tool_ctx.clone(),
            todo_path: deps.todo_path.clone(),
            edit_interval,
            ticker: StdMutex::new(None),
            last_status_emit_at: StdMutex::new(now),
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
    /// note is never rendered/taken).
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
    /// caller re-deriving its own.
    pub(super) fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// A tool batch is about to execute. Live HUD: post (first batch) or
    /// edit the HUD to show the running tools. Also records the batch's
    /// lead tool for the activity line.
    pub(super) async fn on_batch_start(&self, calls: &[PendingToolCall]) {
        if self.behavior != Behavior::Live {
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
            Behavior::Live => {
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
        let (posted, tool_runs, already_finalized) = match self.shared.lock() {
            Ok(mut s) => {
                let snapshot = (s.posted, s.tool_runs, s.finalized);
                s.finalized = true;
                snapshot
            }
            Err(_) => return,
        };
        if already_finalized || (tool_runs == 0 && !posted) {
            // Double finalize, or a fast turn that never posted a frame:
            // no HUD to collapse and no final line worth posting. (A
            // zero-tool turn that DID post an F5 "thinking…" frame still
            // collapses below, so the thinking frame never dangles.)
            return;
        }
        copperclaw_metrics::observe_hud_finalize_seconds(self.started_at.elapsed().as_secs_f64());
        let elapsed = fmt_mmss(self.started_at.elapsed().as_secs());
        let plural = if tool_runs == 1 { "" } else { "s" };
        // F5: a pure-reasoning turn collapses its "thinking…" frame to a
        // clean "done in M:SS" (the "0 tool calls" tail would read oddly).
        let summary = match (ok, tool_runs) {
            (true, 0) => format!("done in {elapsed}"),
            (false, 0) => format!("stopped after {elapsed}"),
            (true, n) => format!("done in {elapsed}, {n} tool call{plural}"),
            (false, n) => format!("stopped after {elapsed}, {n} tool call{plural}"),
        };
        let breadcrumb = Breadcrumb {
            tool_name: TASK_HUD_TOOL.to_owned(),
            detail: None,
            status: if ok {
                BreadcrumbStatus::Done
            } else {
                BreadcrumbStatus::Failed
            },
            summary: Some(cap_chars(&summary, MAX_SUMMARY_CHARS)),
            steps: Vec::new(),
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
            copperclaw_metrics::inc_hud_edits(&self.agent_group, trigger);
        }
        self.ctx.emit_task_hud(&breadcrumb, first).await;
    }

    /// Build the Running-state breadcrumb from shared state + the todo
    /// store + the wall clock, flipping `posted` when this is the first
    /// frame. `None` when the HUD is already finalized (ticker race).
    fn compose_running_frame(&self) -> Option<(Breadcrumb, bool)> {
        running_frame(&self.shared, self.started_at, &self.todo_path)
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
        *guard = Some(tokio::spawn(async move {
            // Pre-first-tool wait: hold off the initial post so fast turns
            // (which finalize before this elapses) never post.
            tokio::time::sleep(THINKING_THRESHOLD).await;
            loop {
                let Some((frame, first)) = running_frame(&shared, started_at, &todo_path) else {
                    break;
                };
                // Suppress a frame byte-identical to the last one sent (a
                // no-op edit Telegram 400s on) — but a `first` post always
                // goes through.
                if record_and_should_emit(&shared, &frame, first) {
                    if first {
                        copperclaw_metrics::inc_hud_post(&agent_group);
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
        let due = self
            .last_status_emit_at
            .lock()
            .map(|at| at.elapsed() >= STATUS_INTERVAL)
            .unwrap_or(false);
        if !due {
            return;
        }
        let elapsed_secs = self.started_at.elapsed().as_secs();
        let todo_step = current_todo_step(&self.todo_path);
        let status = compose_status_row(elapsed_secs, tool_runs, last_tool, todo_step);
        self.ctx.emit_status(&status).await;
        if let Ok(mut at) = self.last_status_emit_at.lock() {
            *at = Instant::now();
        }
    }
}

/// Compose one bare-channel status row (F6). Carries the elapsed clock,
/// the cumulative tool count, the latest tool, and — when the session's
/// todo store has one — the current `step N/M: …` detail so bare channels
/// show real progress. Past [`INTERMEDIATE_STATUS_AFTER`] the closing
/// reassurance softens to "taking longer than usual, still going". Pure
/// so the folding + threshold can be unit-tested without wall-clock waits.
fn compose_status_row(
    elapsed_secs: u64,
    tool_runs: usize,
    last_tool: Option<&str>,
    todo_step: Option<String>,
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
/// store + the wall clock. Marks the HUD `posted` and returns whether
/// this was the first frame. `None` once finalized (ticker race) — the
/// caller stops emitting.
fn running_frame(
    shared: &StdMutex<Shared>,
    started_at: Instant,
    todo_path: &Path,
) -> Option<(Breadcrumb, bool)> {
    let (first, tool_runs, activity, note) = {
        let mut s = shared.lock().ok()?;
        if s.finalized {
            return None;
        }
        let first = !s.posted;
        s.posted = true;
        (first, s.tool_runs, s.activity.clone(), s.note.take())
    };
    let elapsed = fmt_mmss(started_at.elapsed().as_secs());
    let plural = if tool_runs == 1 { "" } else { "s" };
    // F5: before the first tool runs the frame is the pre-first-tool
    // "thinking…" wait (posted by the armed background task after
    // THINKING_THRESHOLD). Once a tool has run it's the usual tool-count
    // summary.
    let mut summary = if tool_runs == 0 && activity.is_none() {
        format!("thinking… | {elapsed}")
    } else {
        format!("{tool_runs} tool call{plural} | {elapsed}")
    };
    if let Some(n) = note {
        summary.push_str(" | ");
        summary.push_str(&n);
    }
    let mut detail_parts: Vec<String> = Vec::new();
    if let Some(step) = current_todo_step(todo_path) {
        detail_parts.push(step);
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
        steps: Vec::new(),
    };
    Some((breadcrumb, first))
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
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{truncated}...")
}

/// On-disk shape of one `agent_todos.json` entry (see
/// `copperclaw-mcp/src/tools/todo.rs`). Only the fields the HUD reads.
#[derive(serde::Deserialize)]
struct TodoEntry {
    text: String,
    status: String,
}

/// Current todo step line for the HUD (`step 2/5: build the UI`), read
/// best-effort from the session's todo store. Preference order: the
/// first `in_progress` item, else the first `pending` item. `None` when
/// the store is missing, unparseable, or empty — the HUD simply omits
/// the segment.
fn current_todo_step(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let items: Vec<TodoEntry> = serde_json::from_slice(&bytes).ok()?;
    if items.is_empty() {
        return None;
    }
    let total = items.len();
    let completed = items.iter().filter(|i| i.status == "completed").count();
    let current = items
        .iter()
        .find(|i| i.status == "in_progress")
        .or_else(|| items.iter().find(|i| i.status == "pending"))?;
    // Step number = completed + 1 (the one being worked), clamped to total.
    let step_no = (completed + 1).min(total);
    Some(format!("step {step_no}/{total}: {}", current.text))
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
        let with = compose_status_row(65, 3, Some("shell"), Some("step 2/5: build the UI".into()));
        assert!(
            with.contains("step 2/5: build the UI"),
            "bare status must fold in the current todo step: {with}"
        );
        assert!(with.contains("3 tool calls"));
        assert!(with.contains("latest: shell"));
        // No todo store → no step segment, and it stays a clean sentence.
        let without = compose_status_row(65, 1, Some("read_file"), None);
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
        let before = compose_status_row(120, 4, Some("shell"), None);
        assert!(before.ends_with("I'll keep going."), "got: {before}");
        assert!(!before.contains("taking longer"), "premature: {before}");
        // At/past the threshold (the ~180s row): the softened reassurance.
        let after = compose_status_row(
            INTERMEDIATE_STATUS_AFTER.as_secs(),
            9,
            Some("cargo"),
            Some("step 3/6: wire it up".into()),
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
            finalized: false,
            last_emitted: None,
        });
        let started = Instant::now();
        let (frame, first) = running_frame(&shared, started, &todo_path).unwrap();
        assert!(!first, "HUD was already posted");
        let summary = frame.summary.as_deref().unwrap();
        assert!(
            summary.contains("steering noted"),
            "note must ride the next edit; got: {summary}"
        );
        // Next frame: the note is one-shot.
        let (frame2, _) = running_frame(&shared, started, &todo_path).unwrap();
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
            activity: None,
            note: None,
            posted: true,
            finalized: true,
            last_emitted: None,
        });
        assert!(
            running_frame(&shared, Instant::now(), &todo_path).is_none(),
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
}
