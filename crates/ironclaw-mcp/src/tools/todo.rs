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
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use serde_json::json;

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

pub mod add {
    //! `todo_add`: append a new pending item.

    use super::{
        json, make_tool, next_id, parse_args, read_all, success_json, write_all, CallToolResult,
        Deserialize, JsonObject, ToolEntry, ToolError, ToolHandler, TodoItem, TodoStatus, Utc,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        text: String,
    }

    pub fn schema() -> super::Tool {
        make_tool(
            "todo_add",
            "Append a new pending todo to your per-session scratchpad. Use this to break a multi-step user request into discrete items so you can track which steps you've finished and which are still outstanding. Returns the new item's id; later you can mark it `in_progress` or `completed` via `todo_update`, or drop it with `todo_delete`.",
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
        _ctx: &dyn crate::context::ToolContext,
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
        json, make_tool, parse_args, read_all, success_json, write_all, CallToolResult,
        Deserialize, JsonObject, ToolEntry, ToolError, ToolHandler, TodoStatus, Utc,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        id: u32,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        status: Option<TodoStatus>,
    }

    pub fn schema() -> super::Tool {
        make_tool(
            "todo_update",
            "Update a todo's text and/or status by id. Pass only the fields you want to change; the others stay as-is. Use this to flip a `pending` item to `in_progress` when you start it, then to `completed` when it's done. Errors if no todo has the given id.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id":     { "type": "integer", "minimum": 0 },
                    "text":   { "type": ["string", "null"], "minLength": 1 },
                    "status": { "type": ["string", "null"], "enum": ["pending", "in_progress", "completed", null] }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
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
        json, make_tool, parse_args, read_all, success_json, write_all, CallToolResult,
        Deserialize, JsonObject, ToolEntry, ToolError, ToolHandler,
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
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let mut items = read_all().await?;
        let pos = items
            .iter()
            .position(|i| i.id == input.id)
            .ok_or_else(|| ToolError::Validation(format!("no todo with id {}", input.id)))?;
        let removed = items.remove(pos);
        write_all(&items).await?;
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
