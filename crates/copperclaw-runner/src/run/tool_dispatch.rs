//! Per-tool dispatch: invoke one model-requested tool call against the
//! runner's tool map, wrap it in a heartbeat ticker + deadline, and render
//! the result into the `Tool` history block the model sees next turn.

use crate::disallowed::is_disallowed;

use super::RunnerDeps;
use super::drive_turn::PendingToolCall;
use super::provider_call::HeartbeatTicker;

/// Execute one tool call against the runner's tool map. Returns
/// `(content, images, is_error)`: the text for the `HistoryMessage::Tool`
/// row, any image blocks the tool returned (each becomes a follow-on
/// `HistoryMessage::Image` so vision models can see them), and whether
/// the call errored.
pub(super) async fn invoke_tool(
    deps: &RunnerDeps,
    call: &PendingToolCall,
) -> (String, Vec<ToolImage>, bool) {
    if is_disallowed(&call.name) {
        return (
            format!(
                "Tool `{}` is disallowed inside the copperclaw container.",
                call.name
            ),
            Vec::new(),
            true,
        );
    }
    let Some(entry) = deps.tool_map.get(&call.name) else {
        return (
            format!("Unknown tool `{}` — no handler registered.", call.name),
            Vec::new(),
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
                Vec::new(),
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
        Ok(Ok(result)) => (
            render_tool_result(&result),
            extract_tool_images(&result),
            false,
        ),
        Ok(Err(err)) => (
            format!("Tool `{}` failed: {err}", call.name),
            Vec::new(),
            true,
        ),
        Err(_) => (
            format!(
                "Tool `{}` did not return within {}s (per-tool deadline); the runner aborted it. Consider breaking the work into smaller steps.",
                call.name, deps.tool_deadline_secs
            ),
            Vec::new(),
            true,
        ),
    }
}

/// An image block a tool returned: `(mime_type, base64_data)`.
pub(super) type ToolImage = (String, String);

/// Pull any image content blocks out of a `CallToolResult`. Each becomes
/// a `HistoryMessage::Image` so vision-capable models actually see the
/// pixels (the text render only notes `<image>`).
pub(super) fn extract_tool_images(result: &rmcp::model::CallToolResult) -> Vec<ToolImage> {
    result
        .content
        .iter()
        .filter_map(|block| match &block.raw {
            rmcp::model::RawContent::Image(img) => Some((img.mime_type.clone(), img.data.clone())),
            _ => None,
        })
        .collect()
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
