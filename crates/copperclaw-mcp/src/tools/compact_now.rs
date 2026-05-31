//! `compact_now`: request that the runner compact this session's
//! conversation history at the start of its next turn.
//!
//! Compaction normally fires automatically when estimated history
//! tokens cross `model_input_window - safety_margin - output_reserve`.
//! This tool lets the agent pay the summarisation cost proactively
//! after a chunky tool loop whose context is unlikely to be referenced
//! again. Mechanism: see `crate::tools::sentinel`.

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde_json::json;

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::sentinel::{drop_sentinel, sentinel_path};
use crate::tools::{make_tool, ToolEntry, ToolHandler};

/// Sentinel filename (without the leading dot) the runner polls for.
pub const SENTINEL_NAME: &str = "history_compact_pending";

/// Convenience for the runner: the full path it watches each turn.
#[must_use]
pub fn pending_path() -> std::path::PathBuf {
    sentinel_path(SENTINEL_NAME)
}

pub fn schema() -> Tool {
    make_tool(
        "compact_now",
        "Request that the runner compact this session's conversation history immediately, replacing the oldest half of the transcript with a single summary. Useful after a heavy tool loop whose context the agent doesn't need to keep. The compaction runs at the start of the next turn — this tool returns success as soon as the request is recorded.",
        json!({ "type": "object", "additionalProperties": false, "properties": {} }),
    )
}

pub async fn handle(
    _arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    drop_sentinel(SENTINEL_NAME).await?;
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
    ToolEntry { tool: schema(), handler: Box::new(Handler) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockToolContext;
    use crate::tools::sentinel::{sentinel_dir_test_override_clear, sentinel_dir_test_override_set};

    #[tokio::test]
    async fn writes_sentinel_file() {
        let td = tempfile::tempdir().unwrap();
        sentinel_dir_test_override_set(td.path().to_path_buf());
        let ctx = MockToolContext::new();
        let _ = handle(None, &ctx).await.unwrap();
        assert!(td.path().join(".history_compact_pending").exists());
        sentinel_dir_test_override_clear();
    }
}
