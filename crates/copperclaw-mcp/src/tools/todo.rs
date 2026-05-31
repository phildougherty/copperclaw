//! `todo_*` tools: a per-session scratchpad for self-tracking work.
//!
//! Useful for any agent — messaging, scheduling, coding — that wants to
//! break a multi-step request into discrete items and remember which it
//! has finished. Backed by a JSON file at `/data/agent_todos.json`
//! inside the container, which lives on the bind-mounted session
//! directory and so survives runner restarts within the same session.
//!
//! Four sibling tools:
//!
//! - `todo_add(text)`        — append a new pending item, returns its id.
//! - `todo_list()`           — return every item with id + status + text.
//! - `todo_update(id, …)`    — change an item's text and/or status.
//! - `todo_delete(id)`       — drop an item.
//!
//! Storage shape (one entry per object):
//!
//! ```json
//! [
//!   {"id": 1, "text": "...", "status": "pending|in_progress|completed",
//!    "created_at": "RFC3339", "updated_at": "RFC3339"}
//! ]
//! ```
//!
//! The file is rewritten in full on every mutation; concurrency is bounded
//! by the runner's single-threaded loop, so there is no inter-call locking.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use copperclaw_channels_core::{TodoItemStatus, TodoList, TodoListItem, TODO_MAX_ITEMS};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::context::{EmitTodoListSpec, OutboundToolEffect, ToolContext};
use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};

/// Default location of the per-session todo file. The session dir is
/// bind-mounted to `/data`, so todos persist across runner restarts of
/// the same session but never bleed across sessions.
const TODO_DEFAULT_PATH: &str = "/data/agent_todos.json";

#[cfg(test)]
static TODO_TEST_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn todo_test_override_set(path: PathBuf) {
    let cell = TODO_TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
}

#[cfg(test)]
pub(super) fn todo_test_override_clear() {
    if let Some(cell) = TODO_TEST_OVERRIDE.get() {
        *cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
fn todo_test_override() -> Option<PathBuf> {
    TODO_TEST_OVERRIDE.get().and_then(|m| {
        m.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    })
}

#[cfg(not(test))]
fn todo_test_override() -> Option<PathBuf> {
    None
}

fn todo_path() -> PathBuf {
    if let Some(p) = todo_test_override() {
        return p;
    }
    PathBuf::from(TODO_DEFAULT_PATH)
}

/// Wipe the per-session todo store. Used by the runner's `/clear`
/// slash command so a wiped conversation history doesn't leave a
/// stale plan visible — caught live on 2026-05-24 when a Telegram
/// session's `/clear` left an email-triage plan from a prior task
/// in place, then the model appended the next task's items on top,
/// producing a "13/26 done" Frankenstein plan with items from three
/// unrelated runs. Returns `Ok(true)` when a store was removed,
/// `Ok(false)` when there was nothing to remove (already clean).
/// Best-effort: callers swallow errors so a missing-or-locked store
/// doesn't abort the `/clear` confirmation.
pub async fn clear_store() -> Result<bool, std::io::Error> {
    let path = todo_path();
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoItem {
    id: u32,
    text: String,
    status: TodoStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

async fn read_all() -> Result<Vec<TodoItem>, ToolError> {
    let path = todo_path();
    match tokio::fs::read(&path).await {
        Ok(bytes) => match serde_json::from_slice::<Vec<TodoItem>>(&bytes) {
            Ok(items) => Ok(items),
            Err(e) => {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let mut quarantine = path.clone().into_os_string();
                quarantine.push(format!(".corrupt-{nanos}"));
                let quarantine = PathBuf::from(quarantine);
                tracing::warn!(
                    path = %path.display(),
                    quarantine = %quarantine.display(),
                    error = %e,
                    "todo store was unparseable; moving aside and starting fresh"
                );
                if let Err(rename_err) = tokio::fs::rename(&path, &quarantine).await {
                    tracing::warn!(
                        path = %path.display(),
                        quarantine = %quarantine.display(),
                        error = %rename_err,
                        "could not quarantine corrupt todo store; leaving in place"
                    );
                }
                Ok(Vec::new())
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(ToolError::Internal(format!(
            "could not read todo store at {}: {err}",
            path.display()
        ))),
    }
}

async fn write_all(items: &[TodoItem]) -> Result<(), ToolError> {
    let path = todo_path();
    let json_bytes = serde_json::to_vec_pretty(items)
        .map_err(|e| ToolError::Internal(format!("todo serialise failed: {e}")))?;
    // Sibling tempfile + rename so a mid-write crash leaves either the
    // old file intact (rename pending) or the new file intact (rename
    // done) — never a truncated half. Same directory keeps the rename
    // on one filesystem (cross-mount rename is EXDEV).
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if let Err(e) = tokio::fs::write(&tmp, &json_bytes).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(ToolError::Internal(format!(
            "could not write todo store temp at {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(ToolError::Internal(format!(
            "could not rename todo store temp into place at {}: {e}",
            path.display()
        )));
    }
    Ok(())
}

fn next_id(items: &[TodoItem]) -> u32 {
    items.iter().map(|i| i.id).max().unwrap_or(0).saturating_add(1)
}

/// Map a storage-side `TodoStatus` onto the canonical wire-shape
/// [`TodoItemStatus`] from `copperclaw-channels-core`. The two enums
/// have the same shape but live in separate crates — one is the
/// per-session on-disk representation, the other is the schema the
/// channel adapters render.
fn status_to_wire(s: TodoStatus) -> TodoItemStatus {
    match s {
        TodoStatus::Pending => TodoItemStatus::Pending,
        TodoStatus::InProgress => TodoItemStatus::InProgress,
        TodoStatus::Completed => TodoItemStatus::Completed,
    }
}

/// Build a canonical [`TodoList`] from the on-disk items. Truncates
/// `text` per-item to [`copperclaw_channels_core::TODO_MAX_ITEM_TEXT_CHARS`]
/// and caps the total list at [`TODO_MAX_ITEMS`] so a runaway agent
/// can't blow past the schema validator. Returns `None` when the list
/// is empty (nothing to emit — there's no "empty plan" UX).
fn build_wire_list(items: &[TodoItem]) -> Option<TodoList> {
    if items.is_empty() {
        return None;
    }
    let wire_items: Vec<TodoListItem> = items
        .iter()
        .take(TODO_MAX_ITEMS)
        .map(|it| {
            let trimmed = it.text.trim();
            // Truncate at the schema cap. Cap > 200 chars by
            // dropping the tail with an ellipsis so the adapter
            // doesn't reject the row.
            let text = if trimmed.chars().count()
                > copperclaw_channels_core::TODO_MAX_ITEM_TEXT_CHARS
            {
                let cap = copperclaw_channels_core::TODO_MAX_ITEM_TEXT_CHARS - 1;
                let mut out: String = trimmed.chars().take(cap).collect();
                out.push('…');
                out
            } else {
                trimmed.to_owned()
            };
            TodoListItem {
                id: it.id,
                text,
                status: status_to_wire(it.status),
            }
        })
        .collect();
    Some(TodoList {
        items: wire_items,
        title: None,
    })
}

/// Emit the post-mutation `TodoList` through the runner's outbound
/// pipeline so the host's `dispatch_todo_list` path can render it
/// natively (edit-in-place + pinned) on platforms that support it.
///
/// Best-effort: a failed emit doesn't fail the underlying mutation.
/// The list has already been persisted to the on-disk store; the
/// chat surface is decoration. Empty lists are NOT emitted — there
/// is no "empty plan" UX, and the host falls back to the legacy
/// `todo_watcher` notifications for the "all done" message.
///
/// Validation failure is logged and swallowed — a list whose items
/// can't pass `TodoList::validate` was already accepted at the tool
/// boundary (we trust the on-disk store's prior validation), so a
/// failure here is a host bug that warrants a log but not a tool
/// error.
async fn emit_after_mutation(ctx: &dyn ToolContext, items: &[TodoItem]) {
    let Some(list) = build_wire_list(items) else {
        return;
    };
    if let Err(err) = list.validate() {
        tracing::debug!(
            error = %err,
            "skipping TodoList emit; built list failed validation",
        );
        return;
    }
    let effect = OutboundToolEffect::EmitTodoList(EmitTodoListSpec { list });
    if let Err(err) = ctx.emit_outbound(effect).await {
        tracing::debug!(
            error = %err,
            "TodoList emit failed; mutation already persisted on disk",
        );
    }
}

pub mod add {
    //! `todo_add`: append a new pending item.

    use super::{
        emit_after_mutation, json, make_tool, next_id, parse_args, read_all, success_json,
        write_all, CallToolResult, Deserialize, JsonObject, ToolEntry, ToolError, ToolHandler,
        TodoItem, TodoStatus, Utc,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        text: String,
    }

    pub fn schema() -> super::Tool {
        make_tool(
            "todo_add",
            "Append a new pending todo to your per-session scratchpad. Use this to break a multi-step user request into discrete items so you can track which steps you've finished and which are still outstanding. Returns the new item's id; later you can mark it `in_progress` or `completed` via `todo_update`, or drop it with `todo_delete`.\n\n**Granularity rule:** prefer many small items over a few coarse ones. For a multi-phase build (research + design + scaffold + per-component + verify + ship), use AT LEAST 5-10 items, ideally one item per file or sub-task. A 3-item plan like `[research, design, build]` for a real prototype build is almost always too coarse — the user sees `2/3 done` while you're still writing 20 more files, which lies about progress. If the user asks for something that touches >5 files or runs >10 minutes, plan in ≥5 items.\n\nThe host renders your todos as a live, pinned checklist on channels that support it (Telegram, Slack, etc.) — pick item text the user will appreciate seeing (imperative, specific). Avoid verbose internal-jargon items.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["text"],
                "properties": {
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Short, imperative description of the step (e.g. \"Reply with the order status\", \"Schedule the follow-up for tomorrow\")."
                    }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let trimmed = input.text.trim();
        if trimmed.is_empty() {
            return Err(ToolError::Validation("`text` must be non-empty".into()));
        }
        let mut items = read_all().await?;
        let now = Utc::now();
        let item = TodoItem {
            id: next_id(&items),
            text: trimmed.to_string(),
            status: TodoStatus::Pending,
            created_at: now,
            updated_at: now,
        };
        let item_for_response = item.clone();
        items.push(item);
        write_all(&items).await?;
        // Emit the full post-mutation list so the host's
        // dispatch_todo_list path can render it natively (edit in
        // place + pinned). Best-effort: persistence on disk above
        // is the load-bearing operation.
        emit_after_mutation(ctx, &items).await;
        Ok(success_json(&item_for_response))
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
}

pub mod list {
    //! `todo_list`: return every item with id + status + text.

    use super::{
        json, make_tool, parse_args, read_all, success_json, CallToolResult, Deserialize,
        JsonObject, ToolEntry, ToolError, ToolHandler,
    };

    #[derive(Debug, Deserialize, Default)]
    struct Input {}

    pub fn schema() -> super::Tool {
        make_tool(
            "todo_list",
            "Return every todo in your per-session scratchpad, oldest first. Each entry carries an `id`, `text`, `status` (`pending` / `in_progress` / `completed`), and timestamps. Useful at the start of a turn to remind yourself what's outstanding.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let _: Input = parse_args(arguments)?;
        let items = read_all().await?;
        Ok(success_json(&items))
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
}

pub mod update {
    //! `todo_update`: change an item's text and/or status.

    use super::{
        emit_after_mutation, json, make_tool, parse_args, read_all, success_json, write_all,
        CallToolResult, Deserialize, JsonObject, ToolEntry, ToolError, ToolHandler, TodoStatus,
        Utc,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        id: u32,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        status: Option<TodoStatus>,
        /// Concrete proof of completion when flipping to `completed`.
        /// Required only for `status: "completed"`. Should reference
        /// files you wrote, commands you ran, or outputs you saw —
        /// not aspirational prose. See [`is_acceptable_evidence`] for
        /// the rejection criteria (too short, generic phrases, etc.).
        #[serde(default)]
        evidence: Option<String>,
    }

    pub fn schema() -> super::Tool {
        make_tool(
            "todo_update",
            "Update a todo's text and/or status by id. Pass only the fields you want to change; the others stay as-is. Use this to flip a `pending` item to `in_progress` when you start it, then to `completed` when it's done.\n\n**`completed` means VERIFIED done — not started, not partly done, not 'I wrote the first file.'** Examples that are NOT completion:\n  - You wrote `package.json` and `server.js` for a 'build prototype' item but haven't tested anything yet → still `in_progress`.\n  - You started a multi-file refactor and finished file 1 of 5 → still `in_progress`.\n  - You scaffolded the project but haven't run it → still `in_progress`.\n\nIf you're about to make MORE tool calls related to an item, it's not done yet. Only mark complete after the item's outcome is observable (tests passed, server started, file content verified, etc.).\n\n**When setting `status` to `\"completed\"`, you MUST also provide an `evidence` field (≥40 chars)** naming the specific files you wrote AND the verification step you ran (`ran npm test, 12/12 passed`, `curl returned 200 with expected JSON`, `verified server starts on port 3000`). Pure delivery claims (`wrote 5 files`) are accepted only when the item itself is a pure-write item; for `build` / `implement` / `verify` items the evidence must mention a check that actually ran. Generic phrases like \"done\" / \"finished\" / \"all set\" are rejected. This is an anti-fabrication guard: if you can't cite a verification step, leave the todo `in_progress`. Errors if no todo has the given id.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id":       { "type": "integer", "minimum": 0 },
                    "text":     { "type": ["string", "null"], "minLength": 1 },
                    "status":   { "type": ["string", "null"], "enum": ["pending", "in_progress", "completed", null] },
                    "evidence": { "type": ["string", "null"], "minLength": 1 }
                }
            }),
        )
    }

    /// Reject obvious non-evidence strings — generic affirmations,
    /// too-short blurbs that can't possibly cite a file path or
    /// command output. The goal isn't perfect detection but a
    /// force-function that makes the agent stop and think before
    /// declaring a task complete.
    const FORBIDDEN_GENERIC: &[&str] = &[
        "done",
        "complete",
        "completed",
        "finished",
        "all set",
        "all done",
        "good to go",
        "ready",
        "yes",
        "ok",
        "okay",
        "all good",
        "looks good",
        "lgtm",
        "shipped",
        "wrapped up",
    ];

    /// Minimum evidence length. Bumped from 20 → 40 on 2026-05-24
    /// after a Telegram session marked "Build working prototype"
    /// complete with the evidence "wrote package.json and server.js"
    /// (47 chars, passed the old gate) then kept writing 20 more
    /// files for 2+ more minutes. At 40 chars the evidence has to
    /// actually mention BOTH a deliverable and a verification, which
    /// is the bar we want.
    const MIN_EVIDENCE_LEN: usize = 40;

    /// Positive-signal terms the evidence string SHOULD contain at
    /// least one of. The check isn't perfect (a model can always
    /// hallucinate "ran tests, all passed"), but it catches the
    /// common premature-completion shape where the evidence cites
    /// only file writes (`wrote X, Y, Z`) without any observable
    /// verification step.
    ///
    /// Two categories:
    ///   - Verification verbs (the work was checked, not just done)
    ///   - Concrete-artifact markers (a real path, an extension, a
    ///     command output, a status code) prove SOMETHING tangible
    ///     happened. A pure "I designed the architecture" without
    ///     any of these terms fails.
    const VERIFICATION_VERBS: &[&str] = &[
        "ran ",
        "tested ",
        "test ",
        "tests ",
        "verified ",
        "verify ",
        "checked ",
        "check ",
        "validated ",
        "validate ",
        "confirmed ",
        "confirm ",
        "passed",
        "succeeded",
        "succeed ",
        "output",
        "returned ",
        "started ",
        "starts ",
        "listed ",
        "responded",
        "executed ",
        "execute ",
        "compiled",
        "compile ",
        "built ",
        "build succeeded",
    ];

    /// True if the evidence string mentions at least one concrete
    /// signal — a file path (`/foo`, `./bar`, `foo/bar`), a dot-
    /// extension (`.rs`, `.py`, `.json`), or a verification verb.
    /// All three categories prove SOMETHING observable happened.
    fn evidence_has_concrete_signal(text: &str) -> bool {
        let lowered = text.to_ascii_lowercase();
        // File-path-shaped: contains a slash with at least one
        // non-separator on each side (rules out a stray comma or
        // sentence-final period).
        let has_path = lowered
            .split_whitespace()
            .any(|tok| tok.contains('/') && tok.len() >= 3);
        // Dot-extension-shaped: contains `.` followed by 2-6
        // alphanumeric chars at a word boundary (e.g. `.py`,
        // `.json`, `.tsx`).
        let has_extension = lowered.split_whitespace().any(|tok| {
            tok.rsplit_once('.').is_some_and(|(_, ext)| {
                let ext = ext.trim_end_matches([',', '.', ';', ')', ']']);
                (2..=6).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
            })
        });
        let has_verb = VERIFICATION_VERBS.iter().any(|v| lowered.contains(v));
        has_path || has_extension || has_verb
    }

    pub(crate) fn is_acceptable_evidence(text: &str) -> bool {
        let trimmed = text.trim();
        if trimmed.len() < MIN_EVIDENCE_LEN {
            // Real evidence ("wrote backend/server.rs and ran cargo
            // test, 12 passed") needs at least this much to mention
            // both a deliverable and a verification.
            return false;
        }
        let lowered = trimmed.to_ascii_lowercase();
        if FORBIDDEN_GENERIC.iter().any(|g| lowered == *g) {
            return false;
        }
        evidence_has_concrete_signal(trimmed)
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.text.is_none() && input.status.is_none() {
            return Err(ToolError::Validation(
                "must pass at least one of `text` or `status` to update".into(),
            ));
        }
        let mut items = read_all().await?;
        let pos = items
            .iter()
            .position(|i| i.id == input.id)
            .ok_or_else(|| ToolError::Validation(format!("no todo with id {}", input.id)))?;
        // Anti-fabrication guard: when marking a todo `completed`,
        // require concrete evidence. Runs AFTER the id lookup so
        // unknown-id errors still surface first (more useful diag).
        if matches!(input.status, Some(TodoStatus::Completed)) {
            let Some(ev) = input.evidence.as_deref() else {
                return Err(ToolError::Validation(
                    "must provide `evidence` when setting status to `completed` — cite the files you wrote, commands you ran, or specific outputs that prove the work is done; generic phrases like \"done\" are rejected".into(),
                ));
            };
            if !is_acceptable_evidence(ev) {
                return Err(ToolError::Validation(
                    "`evidence` must be a substantive citation (≥40 chars) that mentions BOTH \
                     a concrete deliverable AND a verification step. Acceptable: a file path \
                     (e.g. `src/server.js`, `/data/foo.py`), a dot-extension reference (e.g. \
                     `.json`, `.rs`), or a verification verb (`ran tests, all passed`, `curl \
                     returned 200`, `verified server starts on port 3000`). Pure delivery \
                     claims without verification (`wrote 5 files`, `done`, `finished`) are \
                     rejected for non-write items. If you can't cite a verification step, \
                     leave the todo `in_progress`.".into(),
                ));
            }
        }
        if let Some(text) = input.text {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(ToolError::Validation(
                    "`text` must be non-empty when provided".into(),
                ));
            }
            items[pos].text = trimmed.to_string();
        }
        if let Some(status) = input.status {
            items[pos].status = status;
        }
        items[pos].updated_at = Utc::now();
        let updated = items[pos].clone();
        write_all(&items).await?;
        // Emit the full post-mutation list — see `add::handle` for
        // the rationale.
        emit_after_mutation(ctx, &items).await;
        Ok(success_json(&updated))
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
}

pub mod delete {
    //! `todo_delete`: drop an item.

    use super::{
        emit_after_mutation, json, make_tool, parse_args, read_all, success_json, write_all,
        CallToolResult, Deserialize, JsonObject, ToolEntry, ToolError, ToolHandler,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        id: u32,
    }

    pub fn schema() -> super::Tool {
        make_tool(
            "todo_delete",
            "Drop a todo by id. Useful when a step turned out to be unnecessary or got rolled into another. Errors if no todo has the given id.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": { "type": "integer", "minimum": 0 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let mut items = read_all().await?;
        let pos = items
            .iter()
            .position(|i| i.id == input.id)
            .ok_or_else(|| ToolError::Validation(format!("no todo with id {}", input.id)))?;
        let removed = items.remove(pos);
        write_all(&items).await?;
        // Emit the full post-mutation list — see `add::handle` for
        // the rationale. Note: an empty list (the last item was
        // deleted) is intentionally NOT emitted; `build_wire_list`
        // returns None on an empty input.
        emit_after_mutation(ctx, &items).await;
        Ok(success_json(&removed))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockToolContext;
    use std::sync::{Mutex, OnceLock};

    fn todo_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct TodoGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl TodoGuard {
        fn new() -> Self {
            let lock = todo_env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            todo_test_override_set(dir.path().join("agent_todos.json"));
            Self { _dir: dir, _lock: lock }
        }
    }

    impl Drop for TodoGuard {
        fn drop(&mut self) {
            todo_test_override_clear();
        }
    }

    fn obj(value: serde_json::Value) -> Option<JsonObject> {
        match value {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    fn body_json(result: &CallToolResult) -> serde_json::Value {
        let text: String = result
            .content
            .iter()
            .filter_map(|c| {
                let raw = serde_json::to_value(c).ok()?;
                raw.get("text")?.as_str().map(str::to_string)
            })
            .collect();
        serde_json::from_str(&text).expect("response is JSON")
    }

    #[tokio::test]
    async fn add_then_list_returns_the_added_item() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = add::handle(obj(json!({"text": "Reply to user"})), &ctx)
            .await
            .unwrap();
        let added_json = body_json(&added);
        assert_eq!(added_json["text"], "Reply to user");
        assert_eq!(added_json["status"], "pending");
        let listed = list::handle(obj(json!({})), &ctx).await.unwrap();
        let listed_json = body_json(&listed);
        let arr = listed_json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "Reply to user");
    }

    #[tokio::test]
    async fn add_assigns_monotonic_ids() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let a = body_json(&add::handle(obj(json!({"text": "one"})), &ctx).await.unwrap());
        let b = body_json(&add::handle(obj(json!({"text": "two"})), &ctx).await.unwrap());
        let c = body_json(&add::handle(obj(json!({"text": "three"})), &ctx).await.unwrap());
        let a_id = a["id"].as_u64().unwrap();
        let b_id = b["id"].as_u64().unwrap();
        let c_id = c["id"].as_u64().unwrap();
        assert!(a_id < b_id && b_id < c_id, "ids must be monotonic");
    }

    #[tokio::test]
    async fn add_rejects_empty_text() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let err = add::handle(obj(json!({"text": "   "})), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn list_on_empty_store_returns_empty_array() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let listed = list::handle(obj(json!({})), &ctx).await.unwrap();
        let arr = body_json(&listed);
        assert_eq!(arr.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn update_changes_status_and_text() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "original"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        let updated = body_json(
            &update::handle(
                obj(json!({"id": id, "text": "revised", "status": "in_progress"})),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(updated["text"], "revised");
        assert_eq!(updated["status"], "in_progress");
        // Updated_at should advance past created_at.
        let created = updated["created_at"].as_str().unwrap();
        let updated_at = updated["updated_at"].as_str().unwrap();
        assert!(updated_at >= created);
    }

    #[tokio::test]
    async fn update_completed_without_evidence_is_rejected() {
        // The anti-fabrication guard: status="completed" requires
        // a substantive `evidence` field.
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "build backend"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        let err = update::handle(
            obj(json!({"id": id, "status": "completed"})),
            &ctx,
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => assert!(
                msg.contains("evidence"),
                "expected evidence-required error, got: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_completed_with_generic_evidence_is_rejected() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "x"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        for generic in &["done", "complete", "finished", "all set", "ok"] {
            let err = update::handle(
                obj(json!({"id": id, "status": "completed", "evidence": generic})),
                &ctx,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, ToolError::Validation(msg) if msg.contains("substantive")),
                "expected rejection for generic evidence {generic:?}",
            );
        }
    }

    #[tokio::test]
    async fn update_completed_with_substantive_evidence_succeeds() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "x"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        let updated = body_json(
            &update::handle(
                obj(json!({
                    "id": id,
                    "status": "completed",
                    "evidence": "wrote backend/server.rs and ran cargo test (all 14 pass)"
                })),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(updated["status"], "completed");
    }

    #[tokio::test]
    async fn update_completed_rejects_short_evidence_under_40_chars() {
        // Slice-3.5 tighten: bumped min from 20 → 40 chars after a
        // Telegram session marked "Build prototype" complete with
        // "wrote 2 files" (13 chars passed old gate, fails new).
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "x"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        // 39 chars — just under the new threshold, even with a path.
        let err = update::handle(
            obj(json!({
                "id": id,
                "status": "completed",
                "evidence": "wrote src/foo.js (no tests run yet)"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(msg) if msg.contains("substantive")),
            "expected substantive-evidence rejection",
        );
    }

    #[tokio::test]
    async fn update_completed_rejects_evidence_without_concrete_signal() {
        // Even ≥40 chars, evidence must mention a path / extension /
        // verification verb. Pure prose without concrete signals
        // (e.g. "the architecture is sound and matches the design
        // I had in mind for this work") fails — the model can't
        // launder vague claims with sheer wordiness.
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "design architecture"})), &ctx)
                .await
                .unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        let err = update::handle(
            obj(json!({
                "id": id,
                "status": "completed",
                "evidence": "the architecture is sound and matches my design intent for this work"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(msg) if msg.contains("substantive")),
            "expected substantive-evidence rejection for content without concrete signal",
        );
    }

    #[tokio::test]
    async fn update_completed_accepts_evidence_with_verification_verb() {
        // Verification-verb-only evidence passes even without a path
        // (e.g. for "send email" / "post message" style items that
        // don't write to disk).
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "send confirmation email"})), &ctx)
                .await
                .unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        let updated = body_json(
            &update::handle(
                obj(json!({
                    "id": id,
                    "status": "completed",
                    "evidence": "sent email via SendGrid; status 202 returned, verified delivery"
                })),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert_eq!(updated["status"], "completed");
    }

    #[tokio::test]
    async fn update_in_progress_doesnt_need_evidence() {
        // Only completed status gates on evidence — in_progress /
        // pending updates are uninhibited.
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "x"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        update::handle(
            obj(json!({"id": id, "status": "in_progress"})),
            &ctx,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_requires_at_least_one_field() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "x"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        let err = update::handle(obj(json!({"id": id})), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Validation(msg) if msg.contains("at least one")));
    }

    #[tokio::test]
    async fn update_errors_on_unknown_id() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let err = update::handle(
            obj(json!({"id": 999, "status": "completed"})),
            &ctx,
        )
        .await
        .unwrap_err();
        match err {
            ToolError::Validation(msg) => assert!(msg.contains("999")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_removes_item() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "drop me"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        delete::handle(obj(json!({"id": id})), &ctx).await.unwrap();
        let listed = body_json(&list::handle(obj(json!({})), &ctx).await.unwrap());
        assert_eq!(listed.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn delete_errors_on_unknown_id() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let err = delete::handle(obj(json!({"id": 42})), &ctx).await.unwrap_err();
        match err {
            ToolError::Validation(msg) => assert!(msg.contains("42")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn all_four_entries_register_with_expected_names() {
        let names = [
            add::entry().tool.name.to_string(),
            list::entry().tool.name.to_string(),
            update::entry().tool.name.to_string(),
            delete::entry().tool.name.to_string(),
        ];
        assert_eq!(names, ["todo_add", "todo_list", "todo_update", "todo_delete"]);
    }

    // ── TodoList emit pipeline ────────────────────────────────────

    #[tokio::test]
    async fn add_emits_full_post_mutation_todo_list() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        add::handle(obj(json!({"text": "Reply with order status"})), &ctx)
            .await
            .unwrap();
        let calls = ctx.calls();
        let emits: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                OutboundToolEffect::EmitTodoList(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(emits.len(), 1, "expected one EmitTodoList per add");
        let list = &emits[0].list;
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].text, "Reply with order status");
        assert_eq!(list.items[0].status, TodoItemStatus::Pending);
    }

    #[tokio::test]
    async fn update_emits_full_post_mutation_todo_list() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let added = body_json(
            &add::handle(obj(json!({"text": "x"})), &ctx).await.unwrap(),
        );
        let id = added["id"].as_u64().unwrap();
        update::handle(
            obj(json!({"id": id, "status": "in_progress"})),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.calls();
        let emits: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                OutboundToolEffect::EmitTodoList(s) => Some(s),
                _ => None,
            })
            .collect();
        // 2 mutations → 2 emits.
        assert_eq!(emits.len(), 2);
        // Final emit reflects the in_progress status.
        assert_eq!(emits[1].list.items[0].status, TodoItemStatus::InProgress);
    }

    #[tokio::test]
    async fn delete_emits_unless_list_becomes_empty() {
        // Empty lists are intentionally NOT emitted — `build_wire_list`
        // returns None, so the second delete (which empties the store)
        // skips the emit.
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let a = body_json(
            &add::handle(obj(json!({"text": "one"})), &ctx).await.unwrap(),
        );
        let b = body_json(
            &add::handle(obj(json!({"text": "two"})), &ctx).await.unwrap(),
        );
        let a_id = a["id"].as_u64().unwrap();
        let b_id = b["id"].as_u64().unwrap();
        // 2 adds → 2 emits so far.
        delete::handle(obj(json!({"id": a_id})), &ctx).await.unwrap();
        // 1 item left → emit.
        delete::handle(obj(json!({"id": b_id})), &ctx).await.unwrap();
        // 0 items left → NO emit.
        let emits: Vec<_> = ctx
            .calls()
            .into_iter()
            .filter(|c| matches!(c, OutboundToolEffect::EmitTodoList(_)))
            .collect();
        assert_eq!(emits.len(), 3, "expected 3 emits: 2 adds + 1 non-empty delete");
    }

    #[tokio::test]
    async fn emit_failure_does_not_fail_the_mutation() {
        // The on-disk mutation is load-bearing; the chat surface is
        // decoration. A failed emit must not bubble out of the tool
        // handler.
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        ctx.fail_next_emit(ToolError::Context("simulated failure".into()));
        // The add succeeds; the emit fails silently.
        let result = add::handle(obj(json!({"text": "x"})), &ctx).await;
        assert!(result.is_ok(), "add must succeed even when emit fails");
        // Confirm the disk store actually persisted.
        let listed = body_json(&list::handle(obj(json!({})), &ctx).await.unwrap());
        assert_eq!(listed.as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_wire_list_truncates_overlong_item_text() {
        let long_text = "a".repeat(
            copperclaw_channels_core::TODO_MAX_ITEM_TEXT_CHARS + 50,
        );
        let items = vec![TodoItem {
            id: 1,
            text: long_text,
            status: TodoStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }];
        let list = super::build_wire_list(&items).expect("non-empty input");
        // Should fit within the schema cap.
        list.validate().expect("truncated list must validate");
        assert!(list.items[0].text.ends_with('…'));
    }

    #[test]
    fn build_wire_list_returns_none_on_empty() {
        assert!(super::build_wire_list(&[]).is_none());
    }

    #[tokio::test]
    async fn write_is_atomic_no_partial_file() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let path = todo_path();
        let mut tmp_os = path.clone().into_os_string();
        tmp_os.push(".tmp");
        let tmp_path = PathBuf::from(tmp_os);
        // Pre-seed garbage at the temp path; write_all must overwrite
        // and then rename it away.
        tokio::fs::write(&tmp_path, b"garbage").await.unwrap();
        add::handle(obj(json!({"text": "atomic"})), &ctx).await.unwrap();
        assert!(
            !tokio::fs::try_exists(&tmp_path).await.unwrap(),
            "{} should have been renamed away after write_all",
            tmp_path.display()
        );
        assert!(tokio::fs::try_exists(&path).await.unwrap());
    }

    #[tokio::test]
    async fn corrupt_store_is_quarantined_and_reset() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let path = todo_path();
        tokio::fs::write(&path, b"not json").await.unwrap();
        let listed = list::handle(obj(json!({})), &ctx).await.unwrap();
        let arr = body_json(&listed);
        assert_eq!(arr.as_array().unwrap().len(), 0);
        assert!(
            !tokio::fs::try_exists(&path).await.unwrap(),
            "corrupt file should have been moved aside"
        );
        let parent = path.parent().unwrap();
        let prefix = format!("{}.corrupt-", path.file_name().unwrap().to_string_lossy());
        let mut entries = tokio::fs::read_dir(parent).await.unwrap();
        let mut found = false;
        while let Some(entry) = entries.next_entry().await.unwrap() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                found = true;
                break;
            }
        }
        assert!(found, "expected a *.corrupt-* quarantine file in {}", parent.display());
    }

    #[tokio::test]
    async fn partial_truncated_json_is_recoverable() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let path = todo_path();
        tokio::fs::write(&path, b"[{\"id\":1,\"text\":\"foo\"").await.unwrap();
        let listed = list::handle(obj(json!({})), &ctx).await.unwrap();
        let arr = body_json(&listed);
        assert_eq!(arr.as_array().unwrap().len(), 0);
        let parent = path.parent().unwrap();
        let prefix = format!("{}.corrupt-", path.file_name().unwrap().to_string_lossy());
        let mut entries = tokio::fs::read_dir(parent).await.unwrap();
        let mut found = false;
        while let Some(entry) = entries.next_entry().await.unwrap() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                found = true;
                break;
            }
        }
        assert!(found, "expected truncated file to be quarantined");
    }

    #[tokio::test]
    async fn add_after_corrupt_recovery_works() {
        let _g = TodoGuard::new();
        let ctx = MockToolContext::new();
        let path = todo_path();
        tokio::fs::write(&path, b"definitely not json").await.unwrap();
        let added = add::handle(obj(json!({"text": "new"})), &ctx).await.unwrap();
        let added_json = body_json(&added);
        assert_eq!(added_json["text"], "new");
        let listed = body_json(&list::handle(obj(json!({})), &ctx).await.unwrap());
        let arr = listed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["text"], "new");
    }
}
