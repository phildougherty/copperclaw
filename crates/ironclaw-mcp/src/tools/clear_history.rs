//! `clear_history`: nuke this session's conversation history at the start
//! of the next turn.
//!
//! Same sentinel-file pattern as `compact_now`, different mode of erasure:
//! `compact_now` keeps a summary of the old turns, `clear_history` drops
//! them entirely. The new turn starts as if no prior conversation
//! existed. Useful when the agent has wandered into a stuck or
//! contaminated state and wants a clean slate without an operator
//! intervention.
//!
//! Mechanism: write `<session_dir>/.history_clear_pending`. The runner's
//! main loop checks for it at the top of each iteration, sets
//! `state.history.clear()`, removes the sentinel. The runner drives the
//! action for the same reason as `compact_now` — history lives in
//! `RunnerState`, not in `ToolContext`.

use std::path::PathBuf;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde_json::json;

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::{make_tool, ToolEntry, ToolHandler};

const PENDING_DEFAULT_PATH: &str = "/data/.history_clear_pending";
const PENDING_ENV_OVERRIDE: &str = "IRONCLAW_CLEAR_PENDING_FILE";

#[cfg(test)]
static PENDING_TEST_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn pending_test_override_set(p: PathBuf) {
    let cell = PENDING_TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(p);
}

#[cfg(test)]
pub(super) fn pending_test_override_clear() {
    if let Some(cell) = PENDING_TEST_OVERRIDE.get() {
        *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
fn pending_test_override() -> Option<PathBuf> {
    PENDING_TEST_OVERRIDE.get().and_then(|m| {
        m.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    })
}

#[cfg(not(test))]
fn pending_test_override() -> Option<PathBuf> {
    None
}

pub fn pending_path() -> PathBuf {
    if let Some(p) = pending_test_override() {
        return p;
    }
    std::env::var_os(PENDING_ENV_OVERRIDE)
        .map_or_else(|| PathBuf::from(PENDING_DEFAULT_PATH), PathBuf::from)
}

pub fn schema() -> Tool {
    make_tool(
        "clear_history",
        "Request that the runner clear this session's conversation history immediately. The next turn starts as if no prior conversation existed (the session metadata, scheduled tasks, and channel wiring all stay intact). Compare with `compact_now` which keeps a summary; this drops everything. The clear runs at the start of the next turn — this tool returns success as soon as the request is recorded.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    )
}

pub async fn handle(
    _arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let path = pending_path();
    tokio::fs::write(&path, b"").await.map_err(|err| {
        ToolError::Internal(format!(
            "clear_history: write sentinel at {}: {err}",
            path.display()
        ))
    })?;
    Ok(CallToolResult::success(vec![Content::text(
        "history clear requested; will run at the start of the next turn".to_string(),
    )]))
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockToolContext;

    #[tokio::test]
    async fn writes_sentinel_file() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join(".history_clear_pending");
        pending_test_override_set(path.clone());
        let ctx = MockToolContext::new();
        let _ = handle(None, &ctx).await.unwrap();
        assert!(path.exists(), "sentinel file should exist after call");
        pending_test_override_clear();
    }
}
