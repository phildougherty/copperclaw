//! All MCP tools provided by an ironclaw agent container.
//!
//! Each tool lives in its own submodule and exposes:
//! - `pub fn schema() -> rmcp::model::Tool` — the rmcp tool descriptor with a
//!   hand-written JSON Schema (we deliberately do not use `schemars` here
//!   so that the schema is a contract, not whatever derives happen to emit).
//! - `pub async fn handle(args, ctx) -> Result<CallToolResult, ToolError>` —
//!   parses the rmcp arguments, validates, then calls into `ToolContext`.
//!
//! [`build_tool_set`] returns the full inventory.

use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Serialize;
use serde_json::Value;

use crate::context::{ToolContext, ToolEffectAck};
use crate::error::ToolError;

pub mod agents;
pub mod computer_use;
pub mod core;
pub mod interactive;
pub mod scheduling;
pub mod self_mod;
pub mod web_search;

/// A registered tool: its schema plus a type-erased async handler.
pub struct ToolEntry {
    /// rmcp tool descriptor returned in `tools/list`.
    pub tool: Tool,
    /// Handler invoked by `tools/call` for this tool name.
    pub handler: Box<dyn ToolHandler>,
}

/// Type-erased handler.
#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    /// Run the tool with `arguments` (the raw JSON-RPC `arguments` map).
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError>;
}

/// Build the full set of in-process tools the agent can call. Order
/// matches `PLAN.md` § 7 (messaging → interactive → agents → self-
/// mod → scheduling), with the new `computer_use` family appended.
/// Adding a tool here exposes it to the model on the *next* container
/// spawn — no schema migration, no other wiring.
pub fn build_tool_set() -> Vec<ToolEntry> {
    vec![
        core::send_message::entry(),
        core::send_file::entry(),
        core::edit_message::entry(),
        core::add_reaction::entry(),
        interactive::ask_user_question::entry(),
        interactive::send_card::entry(),
        agents::create_agent::entry(),
        self_mod::install_packages::entry(),
        self_mod::add_mcp_server::entry(),
        scheduling::schedule_task::entry(),
        scheduling::list_tasks::entry(),
        scheduling::cancel_task::entry(),
        scheduling::pause_task::entry(),
        scheduling::resume_task::entry(),
        scheduling::update_task::entry(),
        computer_use::shell::entry(),
        computer_use::read_file::entry(),
        computer_use::write_file::entry(),
        computer_use::web_fetch::entry(),
        web_search::entry(),
    ]
}

/// Lookup table form of [`build_tool_set`] for the server router.
pub fn build_tool_map() -> std::collections::HashMap<String, Arc<ToolEntry>> {
    build_tool_set()
        .into_iter()
        .map(|t| (t.tool.name.to_string(), Arc::new(t)))
        .collect()
}

/// Convert an arbitrary serializable value into a `CallToolResult` with a
/// single text block carrying its pretty-printed JSON.
pub(crate) fn success_json<T: Serialize>(value: &T) -> CallToolResult {
    let body = serde_json::to_string_pretty(value)
        .unwrap_or_else(|e| format!("(serialise error: {e})"));
    CallToolResult::success(vec![Content::text(body)])
}

/// Convert a [`ToolEffectAck`] into the `CallToolResult` returned to the
/// caller. We keep the JSON shape so the calling agent can introspect.
pub(crate) fn ack_to_result(ack: &ToolEffectAck) -> CallToolResult {
    success_json(ack)
}

/// Decode the rmcp `arguments` map into a typed input struct.
///
/// On failure returns a `ToolError::Validation` carrying the serde message.
pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolError> {
    let map = arguments.unwrap_or_default();
    let value = Value::Object(map);
    serde_json::from_value(value).map_err(|e| ToolError::Validation(e.to_string()))
}

/// Build a JSON Schema as an rmcp `JsonObject` from `serde_json::json!`.
pub(crate) fn schema_obj(value: Value) -> Arc<JsonObject> {
    let map = match value {
        Value::Object(m) => m,
        _ => JsonObject::default(),
    };
    Arc::new(map)
}

/// Build a `rmcp::model::Tool` from parts.
pub(crate) fn make_tool(name: &'static str, description: &'static str, schema: Value) -> Tool {
    Tool {
        name: std::borrow::Cow::Borrowed(name),
        description: Some(std::borrow::Cow::Borrowed(description)),
        input_schema: schema_obj(schema),
        annotations: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_set_lists_every_in_process_tool() {
        let set = build_tool_set();
        let names: Vec<&str> = set.iter().map(|t| t.tool.name.as_ref()).collect();
        // Messaging core + interactive + agents + self-mod +
        // scheduling + computer_use. Each name listed exactly once.
        let expected: Vec<&str> = vec![
            "send_message",
            "send_file",
            "edit_message",
            "add_reaction",
            "ask_user_question",
            "send_card",
            "create_agent",
            "install_packages",
            "add_mcp_server",
            "schedule_task",
            "list_tasks",
            "cancel_task",
            "pause_task",
            "resume_task",
            "update_task",
            "shell",
            "read_file",
            "write_file",
            "web_fetch",
            "web_search",
        ];
        assert_eq!(set.len(), expected.len());
        for tool in &expected {
            assert!(
                names.contains(tool),
                "missing tool: {tool} in {names:?}"
            );
        }
    }

    #[test]
    fn tool_map_keys_match_tool_set_count() {
        let m = build_tool_map();
        assert_eq!(m.len(), build_tool_set().len());
    }

    #[test]
    fn parse_args_decodes_or_errors() {
        #[derive(serde::Deserialize)]
        struct In {
            x: i32,
        }
        let mut map = JsonObject::default();
        map.insert("x".into(), Value::from(7));
        let parsed: In = parse_args(Some(map)).unwrap();
        assert_eq!(parsed.x, 7);

        let bad: Result<In, _> = parse_args(None);
        assert!(matches!(bad, Err(ToolError::Validation(_))));
    }
}
