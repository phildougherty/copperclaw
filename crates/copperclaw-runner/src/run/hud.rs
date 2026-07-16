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

/// Minimum wall-clock edit cadence for the live HUD. Between tool-batch
/// boundaries a background ticker refreshes the elapsed clock at this
/// interval so a single long tool call (or a long silent reasoning
/// pass) still visibly ticks.
const HUD_EDIT_INTERVAL: Duration = Duration::from_secs(30);

/// Tighter ticker cadence used when the platform shows no typing
/// indicator on this surface — the HUD is then the only working signal,
/// so it must move faster than a human's "is it hung?" threshold.
const HUD_EDIT_INTERVAL_NO_TYPING: Duration = Duration::from_secs(10);

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
}

/// Per-inbound Task HUD driver. Constructed at `drive_turn` entry,
/// notified around every tool batch, finalized when the turn resolves.
/// All methods are best-effort — the HUD never aborts the turn.
pub(super) struct TaskHud {
    behavior: Behavior,
    started_at: Instant,
    shared: Arc<StdMutex<Shared>>,
    ctx: Arc<dyn ToolContext>,
    todo_path: PathBuf,
    edit_interval: Duration,
    /// Background elapsed-clock ticker (live HUD only; spawned on first
    /// post). Aborted on finalize / drop.
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
        let now = Instant::now();
        Self {
            behavior,
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
    /// M18 R2/R5 hook ("steering noted", "switched provider") — those
    /// cards only need to call this, so it ships (tested) ahead of its
    /// first production caller.
    #[allow(dead_code)]
    pub(super) fn add_note(&self, text: &str) {
        if let Ok(mut s) = self.shared.lock() {
            s.note = Some(text.to_owned());
        }
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
        self.emit_live_update().await;
        self.ensure_ticker();
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
                self.emit_live_update().await;
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
        if already_finalized || tool_runs == 0 {
            // Nothing ran (or double finalize): no HUD to collapse and
            // no final line worth posting.
            return;
        }
        let elapsed = fmt_mmss(self.started_at.elapsed().as_secs());
        let plural = if tool_runs == 1 { "" } else { "s" };
        let summary = if ok {
            format!("done in {elapsed}, {tool_runs} tool call{plural}")
        } else {
            format!("stopped after {elapsed}, {tool_runs} tool call{plural}")
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
            // Collapse the live HUD in place.
            Behavior::Live if posted => self.ctx.emit_task_hud(&breadcrumb, false).await,
            // `final`: the one-liner is the only HUD emission at all.
            Behavior::FinalOnly => self.ctx.emit_task_hud(&breadcrumb, true).await,
            _ => {}
        }
    }

    /// Compose + emit the current live-HUD frame (post on first call,
    /// in-place edit afterwards). Skipped after finalization.
    async fn emit_live_update(&self) {
        let Some((breadcrumb, first)) = self.compose_running_frame() else {
            return;
        };
        self.ctx.emit_task_hud(&breadcrumb, first).await;
    }

    /// Build the Running-state breadcrumb from shared state + the todo
    /// store + the wall clock, flipping `posted` when this is the first
    /// frame. `None` when the HUD is already finalized (ticker race).
    fn compose_running_frame(&self) -> Option<(Breadcrumb, bool)> {
        running_frame(&self.shared, self.started_at, &self.todo_path)
    }

    /// Spawn the background elapsed-clock ticker once the HUD is
    /// posted, guaranteeing an edit at least every `edit_interval`
    /// even when a single tool call (or provider turn) runs long.
    fn ensure_ticker(&self) {
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
        *guard = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                // The HUD is already posted by the time the ticker
                // spawns, so every ticker frame is an in-place edit.
                let Some((frame, _first)) = running_frame(&shared, started_at, &todo_path) else {
                    break;
                };
                ctx.emit_task_hud(&frame, false).await;
            }
        }));
    }

    /// Bare-channel fallback: the pre-HUD "still working" heartbeat,
    /// preserved verbatim (text, cadence, and Chat-row emit path) so
    /// adapters without an edit API — and `hud_mode = off` — behave
    /// exactly as before this change. The emit goes direct to outbound
    /// as a Chat row via `emit_status`; it does NOT touch the model's
    /// history, and child-agent sessions skip inside
    /// `RunnerToolCtx::emit_status`.
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
        let last = last_tool.unwrap_or("thinking");
        let plural = if tool_runs == 1 { "" } else { "s" };
        let status = format!(
            "Still working on this — {elapsed_secs}s in, \
             {tool_runs} tool call{plural} so far (latest: {last}). \
             I'll keep going."
        );
        self.ctx.emit_status(&status).await;
        if let Ok(mut at) = self.last_status_emit_at.lock() {
            *at = Instant::now();
        }
    }
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
    let mut summary = format!("{tool_runs} tool call{plural} | {elapsed}");
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
        });
        assert!(
            running_frame(&shared, Instant::now(), &todo_path).is_none(),
            "no frame may be composed after the final collapse"
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
}
