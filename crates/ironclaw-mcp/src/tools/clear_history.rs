//! `clear_history`: wipe this session's conversation history at the
//! start of the next turn.
//!
//! Same sentinel-file pattern as `compact_now`, different mode of
//! erasure: `compact_now` keeps a summary, this drops everything
//! including the message that asked to clear. Mechanism: see
//! `crate::tools::sentinel`.

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde_json::json;

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::sentinel::{drop_sentinel, sentinel_path};
use crate::tools::{make_tool, ToolEntry, ToolHandler};

pub const SENTINEL_NAME: &str = "history_clear_pending";

#[must_use]
pub fn pending_path() -> std::path::PathBuf {
    sentinel_path(SENTINEL_NAME)
}

pub fn schema() -> Tool {
    make_tool(
        "clear_history",
        "Request that the runner clear this session's conversation history immediately. The next turn starts as if no prior conversation existed (session metadata, scheduled tasks, and channel wiring all stay intact). Compare with `compact_now` which keeps a summary; this drops everything. The clear runs at the start of the next turn — this tool returns success as soon as the request is recorded.",
        json!({ "type": "object", "additionalProperties": false, "properties": {} }),
    )
}

pub async fn handle(
    _arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    drop_sentinel(SENTINEL_NAME).await?;
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
        assert!(td.path().join(".history_clear_pending").exists());
        sentinel_dir_test_override_clear();
    }
}
