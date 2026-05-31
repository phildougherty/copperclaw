//! Background watcher that surfaces the agent's todo list to the user
//! channel as it changes.
//!
//! The agent's `todo_add` / `todo_update` / `todo_delete` MCP tools
//! manipulate `<session_dir>/agent_todos.json`. That file is the
//! agent's planning scratchpad — operators have asked for visibility
//! into it from the user's chat: "tell me when a multi-step plan
//! starts" and "tell me when tasks complete."
//!
//! This module:
//! - Polls each running session's `agent_todos.json` every
//!   [`TICK_INTERVAL`] seconds.
//! - Diffs against the previous snapshot (held per session id in a
//!   small in-memory map).
//! - Emits one chat-kind notification through the host's
//!   [`DeliveryDispatcher`] when:
//!     1. Todos first appear on a session (the agent started planning).
//!     2. Items transition to `completed` since the last tick (the
//!        agent finished a step).
//! - Skips per-tick noise: only the deltas above produce messages.
//!   New items added mid-run are folded into the "plan grew" line.
//!
//! Hidden behind `COPPERCLAW_TODO_NOTIFICATIONS=1` (default off — too
//! noisy for production deployments where the operator wants the
//! agent to be quiet between substantive replies). The user-facing
//! Telegram bot enabled it in `.env` for live tests.

use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{messaging_groups, sessions};
use copperclaw_modules::{DeliveryDispatcher, DispatchTarget};
use copperclaw_types::{MessageKind, OutboundMessage, SessionId};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Default poll interval. Slower than the typing ticker because todo
/// changes are coarser-grained — the agent typically completes one
/// item per LLM turn (~5-30s), so 5s is plenty granular.
pub const TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Env var controlling whether the watcher emits notifications. Off
/// by default; flip to `1` / `true` to enable for live operator
/// testing.
pub const ENABLE_ENV_VAR: &str = "COPPERCLAW_TODO_NOTIFICATIONS";

const TODOS_FILENAME: &str = "agent_todos.json";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct TodoItem {
    pub id: i64,
    pub text: String,
    pub status: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// Background watcher. One per host, polls every [`TICK_INTERVAL`].
pub struct TodoWatcher {
    central: CentralDb,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    data_root: PathBuf,
    /// Last-seen todos per session — used to compute deltas. None
    /// means we haven't observed this session yet; `Some(vec)` is
    /// the most recent snapshot we noticed.
    last_seen: Mutex<HashMap<SessionId, Vec<TodoItem>>>,
    interval: Duration,
}

impl TodoWatcher {
    pub fn new(
        central: CentralDb,
        dispatcher: Arc<dyn DeliveryDispatcher>,
        data_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            central,
            dispatcher,
            data_root: data_root.into(),
            last_seen: Mutex::new(HashMap::new()),
            interval: TICK_INTERVAL,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Walk every running session, diff its todos.json against the
    /// last-seen snapshot, and emit notifications for new items and
    /// status->completed transitions. Returns the number of
    /// notifications emitted this tick.
    pub(crate) fn tick(&self) -> usize {
        let running = match sessions::list_running(&self.central) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "todo_watcher: list_running failed; skipping pass");
                return 0;
            }
        };
        let mut emitted = 0usize;
        for s in running {
            let Some(mg_id) = s.messaging_group_id else {
                continue;
            };
            let path = session_todos_path(&self.data_root, s.agent_group_id, s.id);
            let current = match read_todos(&path) {
                Ok(v) => v,
                Err(err) => {
                    debug!(
                        ?err,
                        session = %s.id.as_uuid(),
                        "todo_watcher: read_todos failed; skipping",
                    );
                    continue;
                }
            };
            let mut last = self.last_seen.lock().unwrap();
            let previous = last.entry(s.id).or_default().clone();
            // Compute notifications.
            let notifications = diff_to_notifications(&previous, &current);
            // Update snapshot regardless of whether we found notifications
            // (we want to track new state so future deltas are correct).
            last.insert(s.id, current.clone());
            drop(last);

            if notifications.is_empty() {
                continue;
            }
            // Resolve channel target.
            let mg = match messaging_groups::get(&self.central, mg_id) {
                Ok(m) => m,
                Err(err) => {
                    debug!(
                        ?err,
                        session = %s.id.as_uuid(),
                        "todo_watcher: messaging_groups::get failed",
                    );
                    continue;
                }
            };
            let target = DispatchTarget::channel(mg.channel_type, mg.platform_id, s.thread_id);
            for text in notifications {
                let msg = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": text }),
                    files: Vec::new(),
                };
                self.dispatcher.dispatch(&target, &msg);
                emitted += 1;
            }
        }
        emitted
    }

    /// Loop until shutdown. No-op (returns immediately) when the env
    /// var is off — operators that don't want todo notifications
    /// shouldn't pay even the polling cost.
    pub async fn run_loop(self: Arc<Self>, shutdown: CancellationToken) {
        if !enable_from_env() {
            debug!(
                "todo_watcher: {ENABLE_ENV_VAR} not set; skipping background watcher",
            );
            return;
        }
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.interval) => {
                    let _ = self.tick();
                }
            }
        }
    }
}

fn enable_from_env() -> bool {
    matches!(
        std::env::var(ENABLE_ENV_VAR).ok().as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn session_todos_path(
    data_root: &Path,
    ag: copperclaw_types::AgentGroupId,
    sess: SessionId,
) -> PathBuf {
    data_root
        .join("sessions")
        .join(ag.as_uuid().to_string())
        .join(sess.as_uuid().to_string())
        .join(TODOS_FILENAME)
}

fn read_todos(path: &Path) -> std::io::Result<Vec<TodoItem>> {
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Pure-function delta producer — easy to unit-test without disk or
/// a dispatcher. Returns the list of text strings to send (one per
/// emit), in the order they should be sent.
pub(crate) fn diff_to_notifications(
    previous: &[TodoItem],
    current: &[TodoItem],
) -> Vec<String> {
    use std::collections::HashSet;
    let mut out = Vec::new();

    // 1. First-time plan announcement: previous was empty, current
    //    has items. Emit ONE compact rollup listing the plan.
    if previous.is_empty() && !current.is_empty() {
        let lines: Vec<String> = current
            .iter()
            .map(|t| format!("  • {}", t.text))
            .collect();
        out.push(format!(
            "[todo] Plan ({} steps):\n{}",
            current.len(),
            lines.join("\n"),
        ));
        return out;
    }

    let prev_by_id: HashMap<i64, &TodoItem> =
        previous.iter().map(|t| (t.id, t)).collect();

    // 2. Newly-completed items (status transitions to "completed").
    let completed: Vec<&TodoItem> = current
        .iter()
        .filter(|t| t.status == "completed")
        .filter(|t| prev_by_id.get(&t.id).is_none_or(|prev| prev.status != "completed"))
        .collect();
    if !completed.is_empty() {
        let lines: Vec<String> = completed
            .iter()
            .map(|t| format!("  [done] {}", t.text))
            .collect();
        out.push(format!(
            "Step{} complete:\n{}",
            if completed.len() == 1 { "" } else { "s" },
            lines.join("\n"),
        ));
    }

    // 3. Newly-added items (ids not in previous, not already
    //    completed). Emit a compact rollup.
    let added: Vec<&TodoItem> = current
        .iter()
        .filter(|t| !prev_by_id.contains_key(&t.id))
        .filter(|t| t.status != "completed")
        .collect();
    let prev_ids: HashSet<i64> = prev_by_id.keys().copied().collect();
    if !added.is_empty() && !prev_ids.is_empty() {
        // Only emit "plan grew" if there WAS a previous plan; the
        // first-time announcement above handles the no-previous case.
        let lines: Vec<String> = added
            .iter()
            .map(|t| format!("  + {}", t.text))
            .collect();
        out.push(format!(
            "Plan grew (+{} steps):\n{}",
            added.len(),
            lines.join("\n"),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: i64, text: &str, status: &str) -> TodoItem {
        TodoItem {
            id,
            text: text.into(),
            status: status.into(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn first_time_emits_plan_rollup() {
        let out = diff_to_notifications(
            &[],
            &[t(1, "Research", "pending"), t(2, "Build", "pending")],
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("[todo] Plan (2 steps):"));
        assert!(out[0].contains("Research"));
        assert!(out[0].contains("Build"));
    }

    /// Returns true if `c` falls inside one of the Unicode emoji
    /// blocks. Used by the no-emoji-in-notifications test to enforce
    /// the project's "no emojis" rule on watcher output without false-
    /// flagging existing non-ASCII typography (e.g. bullets, em dashes)
    /// that are not emoji.
    fn is_emoji_codepoint(c: char) -> bool {
        let n = c as u32;
        // Miscellaneous Symbols and Pictographs (includes the
        // historical 📋 clipboard at U+1F4CB).
        (0x1F300..=0x1F5FF).contains(&n)
        // Emoticons.
        || (0x1F600..=0x1F64F).contains(&n)
        // Transport and Map Symbols.
        || (0x1F680..=0x1F6FF).contains(&n)
        // Supplemental Symbols and Pictographs.
        || (0x1F900..=0x1F9FF).contains(&n)
        // Symbols and Pictographs Extended-A.
        || (0x1FA70..=0x1FAFF).contains(&n)
        // Miscellaneous Symbols (includes U+2705 white-check ✅).
        || (0x2600..=0x27BF).contains(&n)
        // Regional indicator (flag) symbols.
        || (0x1F1E6..=0x1F1FF).contains(&n)
    }

    #[test]
    fn notifications_contain_no_emoji() {
        // Every notification body the watcher might emit must be free
        // of emoji per the project-wide "no emojis" rule (CLAUDE.md).
        // Exercises all three code paths in `diff_to_notifications`:
        //   1. first-time plan rollup,
        //   2. completion announcement,
        //   3. "plan grew" line.
        let cases: Vec<(Vec<TodoItem>, Vec<TodoItem>)> = vec![
            // 1. First-time plan.
            (vec![], vec![t(1, "Research", "pending"), t(2, "Build", "pending")]),
            // 2. Completion.
            (
                vec![t(1, "Research", "in_progress")],
                vec![t(1, "Research", "completed")],
            ),
            // 3. Plan grew.
            (
                vec![t(1, "First", "pending")],
                vec![t(1, "First", "pending"), t(2, "Second", "pending")],
            ),
        ];
        for (prev, curr) in cases {
            let out = diff_to_notifications(&prev, &curr);
            assert!(!out.is_empty(), "case produced no notification");
            for s in &out {
                for c in s.chars() {
                    assert!(
                        !is_emoji_codepoint(c),
                        "emoji char {c:?} (U+{:04X}) leaked into todo notification: {s:?}",
                        c as u32,
                    );
                }
            }
        }
    }

    #[test]
    fn empty_to_empty_emits_nothing() {
        assert!(diff_to_notifications(&[], &[]).is_empty());
    }

    #[test]
    fn completion_emits_done_message() {
        let prev = vec![t(1, "Research", "in_progress"), t(2, "Build", "pending")];
        let curr = vec![t(1, "Research", "completed"), t(2, "Build", "pending")];
        let out = diff_to_notifications(&prev, &curr);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("Step complete:"));
        assert!(out[0].contains("Research"));
    }

    #[test]
    fn multi_completion_collapses_to_one_message() {
        let prev = vec![t(1, "A", "in_progress"), t(2, "B", "in_progress")];
        let curr = vec![t(1, "A", "completed"), t(2, "B", "completed")];
        let out = diff_to_notifications(&prev, &curr);
        assert_eq!(out.len(), 1, "two completions in one tick = one message");
        assert!(out[0].contains("Steps complete:"));
        assert!(out[0].contains('A') && out[0].contains('B'));
    }

    #[test]
    fn already_completed_doesnt_re_emit() {
        let prev = vec![t(1, "X", "completed")];
        let curr = vec![t(1, "X", "completed")];
        let out = diff_to_notifications(&prev, &curr);
        assert!(out.is_empty(), "status unchanged should not re-notify");
    }

    #[test]
    fn newly_added_after_initial_plan_emits_grew() {
        let prev = vec![t(1, "First", "pending")];
        let curr = vec![t(1, "First", "pending"), t(2, "Second", "pending")];
        let out = diff_to_notifications(&prev, &curr);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("Plan grew"));
        assert!(out[0].contains("Second"));
    }

    #[test]
    fn newly_added_already_completed_doesnt_emit_grew() {
        // If the agent added then immediately completed in the same
        // tick, the completion path covers it; don't double-emit a
        // "plan grew" line for completed items.
        let prev = vec![t(1, "First", "pending")];
        let curr = vec![t(1, "First", "pending"), t(2, "Second", "completed")];
        let out = diff_to_notifications(&prev, &curr);
        assert!(
            out.iter().all(|s| !s.starts_with("Plan grew")),
            "shouldn't emit 'plan grew' for already-completed items: {out:?}",
        );
        // BUT it should emit a completion notification:
        assert!(out.iter().any(|s| s.contains("Step complete:")));
    }

    #[test]
    fn deletes_dont_notify() {
        // Removing an item shouldn't trigger any notification —
        // operators care about progress, not housekeeping.
        let prev = vec![t(1, "Keep", "pending"), t(2, "Remove", "pending")];
        let curr = vec![t(1, "Keep", "pending")];
        let out = diff_to_notifications(&prev, &curr);
        assert!(out.is_empty(), "deletes should be silent: {out:?}");
    }
}
