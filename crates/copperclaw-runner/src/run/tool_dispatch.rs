//! Per-tool dispatch: invoke one model-requested tool call against the
//! runner's tool map, wrap it in a heartbeat ticker + deadline, and render
//! the result into the `Tool` history block the model sees next turn.

use crate::disallowed::is_disallowed;

use super::drive_turn::PendingToolCall;
use super::provider_call::HeartbeatTicker;
use super::RunnerDeps;

/// Execute one tool call against the runner's tool map. Returns the
/// `(content, is_error)` pair for the `HistoryMessage::Tool` row the
/// model sees on the next turn.
pub(super) async fn invoke_tool(
    deps: &RunnerDeps,
    call: &PendingToolCall,
) -> (String, bool) {
    if is_disallowed(&call.name) {
        return (
            format!(
                "Tool `{}` is disallowed inside the copperclaw container.",
                call.name
            ),
            true,
        );
    }
    let Some(entry) = deps.tool_map.get(&call.name) else {
        return (
            format!("Unknown tool `{}` — no handler registered.", call.name),
            true,
        );
    };
    // ToolHandler::call wants `Option<JsonObject>`; convert from the
    // Value we got off the wire.
    let arguments = match &call.input {
        serde_json::Value::Object(map) => Some(map.clone()),
        serde_json::Value::Null => None,
        _ => {
            return (
                format!(
                    "Tool `{}` input must be a JSON object, got {}",
                    call.name,
                    short_type(&call.input)
                ),
                true,
            );
        }
    };
    // Keep the heartbeat file fresh for the duration of the tool
    // call. Without this a `shell { cmd: "npm install …" }` (~60-90s
    // on a fresh image) drifts past the host's 60s staleness
    // threshold and the host SIGKILLs the container. Drops on
    // function return; the background task is aborted.
    let _hb = HeartbeatTicker::start(deps.heartbeat_path.clone());
    // Per-tool hard deadline so a wedged tool can't run forever.
    // The provider call has its own deadline (`provider_deadline`);
    // tool dispatch did not, until now. Defaulting to a generous
    // 15 min ceiling — `npm install`, `cargo build`, `apt-get
    // install gcc` are all the kinds of tools we want to permit;
    // anything past that is presumed wedged.
    let call_fut = entry.handler.call(arguments, deps.tool_ctx.as_ref());
    let timeout = std::time::Duration::from_secs(deps.tool_deadline_secs);
    match tokio::time::timeout(timeout, call_fut).await {
        Ok(Ok(result)) => (render_tool_result(&result), false),
        Ok(Err(err)) => (format!("Tool `{}` failed: {err}", call.name), true),
        Err(_) => (
            format!(
                "Tool `{}` did not return within {}s (per-tool deadline); the runner aborted it. Consider breaking the work into smaller steps.",
                call.name, deps.tool_deadline_secs
            ),
            true,
        ),
    }
}

/// Pluck the textual content out of a `CallToolResult`. Multiple
/// blocks get joined with double newlines; non-text blocks
/// (resources, images) are rendered as their type tag so the model
/// at least sees they happened.
pub(super) fn render_tool_result(result: &rmcp::model::CallToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        let raw = &block.raw;
        match raw {
            rmcp::model::RawContent::Text(t) => out.push_str(&t.text),
            rmcp::model::RawContent::Image(_) => out.push_str("<image>"),
            rmcp::model::RawContent::Audio(_) => out.push_str("<audio>"),
            rmcp::model::RawContent::Resource(_) => out.push_str("<resource>"),
        }
    }
    if out.is_empty() {
        "(tool produced no output)".to_string()
    } else {
        out
    }
}

pub(super) fn short_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}
