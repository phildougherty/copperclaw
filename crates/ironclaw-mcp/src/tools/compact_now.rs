//! `compact_now`: request that the runner compact this session's
//! conversation history at the start of its next turn.
//!
//! Compaction normally fires automatically when the estimated history
//! token count crosses `model_input_window - safety_margin -
//! output_reserve`. This tool lets the agent trigger compaction
//! proactively — useful when the agent knows it just finished a chunky
//! tool loop whose context is unlikely to be referenced again (a deep
//! research dive, a one-off code generation, etc.) and would rather pay
//! the summarisation cost now than wait until the threshold trips.
//!
//! The mechanism is a sentinel file at
//! `<session_dir>/.history_compact_pending`. The runner's main loop
//! checks for the file at the top of each iteration, runs `compact()`
//! when present, and removes the sentinel. The runner has to drive the
//! action because the history lives in its `RunnerState` (not in
//! `ToolContext`), and reaching across that boundary from a tool would
//! mean threading a `Mutex<History>` through every call site.

use std::path::PathBuf;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde_json::json;

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::{make_tool, ToolEntry, ToolHandler};

const PENDING_DEFAULT_PATH: &str = "/data/.history_compact_pending";
const PENDING_ENV_OVERRIDE: &str = "IRONCLAW_COMPACT_PENDING_FILE";

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

/// Resolve the sentinel-file path. Production reads the default; tests
/// install their own per-fixture path via the `OnceLock` above.
pub fn pending_path() -> PathBuf {
    if let Some(p) = pending_test_override() {
        return p;
    }
    std::env::var_os(PENDING_ENV_OVERRIDE)
        .map_or_else(|| PathBuf::from(PENDING_DEFAULT_PATH), PathBuf::from)
}

pub fn schema() -> Tool {
    make_tool(
        "compact_now",
        "Request that the runner compact this session's conversation history immediately, replacing the oldest half of the transcript with a single summary. Useful after a heavy tool loop whose context the agent doesn't need to keep. The compaction runs at the start of the next turn — this tool returns success as soon as the request is recorded.",
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
            "compact_now: write sentinel at {}: {err}",
            path.display()
        ))
    })?;
    Ok(CallToolResult::success(vec![Content::text(
        "compaction requested; will run at the start of the next turn".to_string(),
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
        let path = td.path().join(".history_compact_pending");
        pending_test_override_set(path.clone());
        let ctx = MockToolContext::new();
        let result = handle(None, &ctx).await.unwrap();
        assert!(path.exists(), "sentinel file should exist after call");
        // The result body says compaction is queued.
        let text = result
            .content
            .iter()
            .filter_map(|c| {
                let v = serde_json::to_value(c).ok()?;
                v.get("text").and_then(|t| t.as_str()).map(str::to_string)
            })
            .collect::<String>();
        assert!(text.contains("compaction requested"));
        pending_test_override_clear();
    }
}
