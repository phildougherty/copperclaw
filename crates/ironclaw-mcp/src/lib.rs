//! Ironclaw MCP: the in-container tool surface plus a thin client wrapper
//! for talking to externally configured MCP servers.
//!
//! Two halves:
//!
//! 1. **Server** (`server`, `tools`, `context`): builds an rmcp
//!    `ServerHandler` exposing 15 tools defined by `PLAN.md` § 7. Each
//!    handler is pure — it validates input and produces an
//!    [`OutboundToolEffect`]. The runner (T5) implements
//!    [`ToolContext`] to satisfy those effects against `outbound.db` and
//!    the scheduler.
//!
//! 2. **Client** (`client`): wraps `rmcp` so the runner can talk to
//!    per-group configured MCP servers (stdio child process today; HTTP
//!    SSE to come) without depending on rmcp directly.
//!
//! The MCP plumbing was kept out of the tool modules themselves so that
//! tests are dirt-cheap (no transport, no Tokio runtime needed for the
//! pure handler tests beyond the `#[tokio::test]` macro).

pub mod client;
pub mod context;
pub mod error;
pub mod server;
pub mod tools;

pub use client::{McpClient, RemoteTool, SharedMcpClient};
pub use context::{
    AddMcpServerSpec, AddReactionSpec, AskUserQuestionSpec, CreateAgentSpec, EditMessageSpec,
    InstallSpec, MockToolContext, OutboundToolEffect, Recipient, ScheduleSpec, SendCardSpec,
    SendFileSpec, SendMessageSpec, SubagentRequest, SubagentResult, SubagentToolCall,
    TaskSummary, ToolContext, ToolEffectAck, UpdateTaskSpec, SUBAGENT_MAX_TOKENS_LIMIT,
    SUBAGENT_MAX_TURNS_LIMIT, SUBAGENT_WALL_CLOCK_SECS,
};
pub use error::{McpError, ToolError};
pub use server::{build_server, IronclawServer};
pub use tools::{build_tool_map, build_tool_set, ToolEntry, ToolHandler};

#[cfg(test)]
mod smoke {
    //! Cross-module smoke tests that don't fit neatly in one module.
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn server_can_dispatch_every_tool_with_minimal_input() {
        let ctx: Arc<dyn ToolContext> = Arc::new(MockToolContext::new());
        let server = build_server(ctx);

        let inputs: &[(&str, serde_json::Value)] = &[
            ("send_message", serde_json::json!({"text": "hi"})),
            (
                "send_file",
                serde_json::json!({"filename": "x", "data": "aGk="}),
            ),
            (
                "edit_message",
                serde_json::json!({"message_id": 1, "text": "x"}),
            ),
            (
                "add_reaction",
                serde_json::json!({"message_id": 1, "emoji": "x"}),
            ),
            (
                "ask_user_question",
                serde_json::json!({"title": "t", "options": ["a"]}),
            ),
            ("send_card", serde_json::json!({"card": {}})),
            (
                "create_agent",
                serde_json::json!({"name": "n", "instructions": "i"}),
            ),
            (
                "install_packages",
                serde_json::json!({"apt": ["x"], "reason": "r"}),
            ),
            (
                "add_mcp_server",
                serde_json::json!({
                    "name": "n", "transport": {}, "reason": "r"
                }),
            ),
            (
                "schedule_task",
                serde_json::json!({
                    "name": "n", "prompt": "p", "recurrence": "0 * * * *"
                }),
            ),
            ("list_tasks", serde_json::json!({})),
            ("cancel_task", serde_json::json!({"id": "task_1"})),
            ("pause_task", serde_json::json!({"id": "task_1"})),
            ("resume_task", serde_json::json!({"id": "task_1"})),
            (
                "update_task",
                serde_json::json!({"id": "task_1", "prompt": "p"}),
            ),
            // git_* smoke covered in each tool's own test module — they
            // need a real on-disk repo, which the smoke harness doesn't
            // set up. Skipping here is intentional.
            ("explore", serde_json::json!({"task": "go look"})),
        ];

        for (name, body) in inputs {
            let args = match body.clone() {
                serde_json::Value::Object(m) => Some(m),
                _ => None,
            };
            let res = server.dispatch(name, args).await.unwrap();
            assert_eq!(
                res.is_error,
                Some(false),
                "tool {name} returned is_error=true",
            );
        }
    }
}
