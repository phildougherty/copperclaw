//! All MCP tools provided by a copperclaw agent container.
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
pub mod apply_patch;
pub mod artifact_path;
pub mod browser_interact;
pub mod browser_render;
pub mod clear_history;
pub mod compact_now;
pub mod computer_use;
pub mod conditions;
pub mod copy_file;
pub mod core;
pub mod diagnostics;
pub(crate) mod diff_util;
pub mod edit_file;
pub mod explore;
pub mod find_symbol;
pub mod git_blame;
pub(crate) mod git_common;
pub mod git_diff;
pub mod git_log;
pub mod git_status;
pub mod glob;
pub mod goals;
pub mod grep;
pub mod interactive;
pub mod list_skills;
pub mod load_skill;
pub mod memory;
pub mod multi_edit;
pub mod net_guard;
pub mod save_skill;
pub mod scheduling;
pub mod self_mod;
pub mod self_review;
pub mod sentinel;
pub mod todo;
pub mod ui_inspect;
pub mod ui_screenshot;
pub mod verify_gate;
pub mod view_image;
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
    let mut set = vec![
        core::send_message::entry(),
        core::send_file::entry(),
        core::edit_message::entry(),
        core::add_reaction::entry(),
        interactive::ask_user_question::entry(),
        interactive::send_card::entry(),
        agents::create_agent::entry(),
        agents::delegate::entry(),
        agents::delegate_batch::entry(),
        self_mod::install_packages::entry(),
        self_mod::add_mcp_server::entry(),
        save_skill::entry(),
        scheduling::schedule_task::entry(),
        scheduling::list_tasks::entry(),
        scheduling::cancel_task::entry(),
        scheduling::pause_task::entry(),
        scheduling::resume_task::entry(),
        scheduling::update_task::entry(),
        // M22 A3: first-class long-running goals — durable objectives the sweep
        // drives check-ins against (create/list/update). Registered alongside
        // the scheduling tools they extend (a goal indexes over the scheduler).
        goals::create_goal::entry(),
        goals::list_goals::entry(),
        goals::update_goal::entry(),
        // M22 A4: durable event-driven condition wakes (revives the dormant
        // sweep condition check-in) + the settable flag latch a `flag`
        // condition watches. Registered alongside the scheduling/goal tools
        // they sit beside — a condition is an event-triggered sibling of a
        // scheduled task.
        conditions::register_condition::entry(),
        conditions::set_condition_flag::entry(),
        computer_use::shell::entry(),
        edit_file::entry(),
        multi_edit::entry(),
        apply_patch::entry(),
        copy_file::entry(),
        computer_use::read_file::entry(),
        computer_use::write_file::entry(),
        computer_use::web_fetch::entry(),
        browser_render::entry(),
        // M20 D1: in-container screenshot of the agent's own app. Always
        // registered (unlike `browser_interact`'s stricter opt-in) — the
        // `coding`/`full` profile allow-list gates who can reach it, and the
        // handler itself probes for chromium at call time so the minimal
        // image profile degrades cleanly instead of crashing.
        ui_screenshot::entry(),
        // M20 D5: console errors + element geometry for the agent's own app
        // — the loopback-only diagnostic sibling of `ui_screenshot` (full
        // console detail + a selector's box/curated computed style).
        // Registered alongside it in the Coding/Full profile tier; same
        // call-time chromium probe degrades cleanly on the minimal profile.
        ui_inspect::entry(),
        // M20 Q3: structured lint/typecheck digest — a read-only fix-cycle
        // accelerator with no `.copperclaw/verify` gate interaction (that
        // stays Q2's enforcement path). Registered alongside `ui_screenshot`
        // in the Coding/Full profile tier; degrades per-tool when
        // eslint/tsc/ruff aren't baked into the image (pre-Q1 / minimal).
        diagnostics::entry(),
        // M20 Q6: enforced self-review gate before final delivery — see the
        // module docs. Registered alongside `ui_screenshot`/`diagnostics` in
        // the Coding/Full profile tier; the `todo.rs` completion gate is
        // what actually enforces it, this tool is just the read/submit
        // surface the agent calls.
        self_review::entry(),
        view_image::entry(),
        // Git inspection tools — read-only structured access to a
        // libgit2-backed repository view. Registered alphabetically.
        git_blame::entry(),
        git_diff::entry(),
        git_log::entry(),
        git_status::entry(),
        glob::entry(),
        grep::entry(),
        // M22 C3: read-only symbol navigation (go-to-def / find-refs / hover)
        // backed by the container-local ctags/LSP index bridge, degrading to
        // on-demand ctags then a scoped definition scan.
        find_symbol::entry(),
        web_search::entry(),
        explore::entry(),
        load_skill::entry(),
        // M22 S3: read-only enumeration of the session's selected skills
        // (name + version + description), the list companion to load_skill /
        // save_skill. Rides the READONLY_TOOLS policy tier.
        list_skills::entry(),
        memory::memory_search::entry(),
        memory::memory_get::entry(),
        memory::memory_save::entry(),
        todo::add::entry(),
        todo::list::entry(),
        todo::update::entry(),
        todo::delete::entry(),
        compact_now::entry(),
        clear_history::entry(),
        artifact_path::entry(),
    ];

    // A2 (Phase 5b): the interactive browser is registered ONLY when its
    // stricter, SEPARATE opt-in is set (both COPPERCLAW_BROWSER_ENABLED and
    // COPPERCLAW_BROWSER_INTERACTIVE truthy). With the flag off the tool is
    // entirely absent — the tool set, schemas, and behaviour are byte-identical
    // to the read-only baseline, so the model never sees the interactive verb.
    if browser_interact::interactive_opt_in(&browser_render::SystemEnv) {
        set.push(browser_interact::entry());
    }

    set
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
    let body =
        serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("(serialise error: {e})"));
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
            "delegate",
            "delegate_batch",
            "install_packages",
            "add_mcp_server",
            "save_skill",
            "schedule_task",
            "list_tasks",
            "cancel_task",
            "pause_task",
            "resume_task",
            "update_task",
            "create_goal",
            "list_goals",
            "update_goal",
            "register_condition",
            "set_condition_flag",
            "shell",
            "edit_file",
            "multi_edit",
            "apply_patch",
            "copy_file",
            "read_file",
            "write_file",
            "web_fetch",
            "browser_render",
            "ui_screenshot",
            "ui_inspect",
            "diagnostics",
            "self_review",
            "view_image",
            "git_blame",
            "git_diff",
            "git_log",
            "git_status",
            "glob",
            "grep",
            "find_symbol",
            "web_search",
            "explore",
            "load_skill",
            "list_skills",
            "memory_search",
            "memory_get",
            "memory_save",
            "todo_add",
            "todo_list",
            "todo_update",
            "todo_delete",
            "compact_now",
            "clear_history",
            "artifact_path",
        ];
        assert_eq!(set.len(), expected.len());
        for tool in &expected {
            assert!(names.contains(tool), "missing tool: {tool} in {names:?}");
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
