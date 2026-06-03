//! Per-tool dispatch: invoke one model-requested tool call against the
//! runner's tool map, wrap it in a heartbeat ticker + deadline, and render
//! the result into the `Tool` history block the model sees next turn.

use crate::policy::PolicyDecision;

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
    // Layered authorization: host-owned floor + sender role + active
    // skill `allowed-tools` + group tool-profile. The floor (the old
    // `DISALLOWED_TOOLS`) is the innermost layer and can never be
    // re-granted by a looser profile. A deny synthesises a model-facing
    // `tool_result { is_error: true }` so the model can self-correct.
    //
    // The active-skill layer is dynamic: a `load_skill` call earlier in
    // the conversation may have narrowed the scope to the loaded skill's
    // `allowed-tools`. The base policy (profile + sender role) is fixed at
    // spawn; we clone it and apply the live skill scope per call so a
    // read-only skill blocks `shell` for as long as it's loaded.
    let policy = deps
        .policy
        .clone()
        .with_active_skill(deps.tool_ctx.active_skill_allowed_tools());
    if let PolicyDecision::Deny(reason) = policy.evaluate(&call.name) {
        tracing::info!(tool = %call.name, %reason, "tool call denied by policy");
        return (reason, Vec::new(), true);
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_mcp::ToolContext;
    use copperclaw_providers::{AgentProvider, AgentQuery, ProviderError, QueryInput};
    use copperclaw_types::{AgentGroupId, ProviderEvent, SessionId};
    use tokio::sync::Mutex;

    use super::*;
    use crate::policy::{SenderRole, ToolPolicy, ToolProfile};
    use crate::run::RunnerDeps;
    use crate::tools::RunnerToolCtx;

    /// Minimal provider stub — never queried on the deny path under test.
    struct NoopProvider;

    #[async_trait]
    impl AgentProvider for NoopProvider {
        #[allow(clippy::unnecessary_literal_bound)]
        fn name(&self) -> &str {
            "noop"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            Ok(Box::new(NoopQuery))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct NoopQuery;

    #[async_trait]
    impl AgentQuery for NoopQuery {
        async fn push(&mut self, _message: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            None
        }
        async fn abort(&mut self) {}
    }

    /// Build a `RunnerDeps` with a real `shell` + `read_file` tool map,
    /// the supplied base policy, and an optional active-skill scope set on
    /// the `ToolContext` (the way `load_skill` sets it in production). Only
    /// the fields `invoke_tool` reads are load-bearing here.
    fn deps_with_policy_and_skill(
        policy: ToolPolicy,
        active_skill: Option<Vec<String>>,
    ) -> (tempfile::TempDir, RunnerDeps) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let ctx = RunnerToolCtx::new(outbound.clone(), paths.outbox.clone());
        // Mirror `load_skill`: stash the loaded skill's allowed-tools on the
        // context so the dispatch gate reads it live (NOT pre-baked into
        // the policy — that's the whole point of the per-call narrowing).
        ctx.set_active_skill_allowed_tools(active_skill);
        let tool_ctx: Arc<dyn ToolContext> = Arc::new(ctx);
        let provider: Arc<dyn AgentProvider> = Arc::new(NoopProvider);
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(provider, tool_ctx, inbound, outbound, archive_dir);
        // Real handlers so an *allowed* call would actually dispatch
        // (we only assert the deny path; the map presence proves the
        // policy gate fires *before* dispatch).
        let mut map: HashMap<String, Arc<copperclaw_mcp::ToolEntry>> = HashMap::new();
        for e in copperclaw_mcp::build_tool_set() {
            map.insert(e.tool.name.to_string(), Arc::new(e));
        }
        deps.tool_map = Arc::new(map);
        deps.policy = policy;
        (tmp, deps)
    }

    /// Convenience: deps with the given base policy and no active skill.
    fn deps_with_policy(policy: ToolPolicy) -> (tempfile::TempDir, RunnerDeps) {
        deps_with_policy_and_skill(policy, None)
    }

    fn call(name: &str) -> PendingToolCall {
        PendingToolCall {
            id: "tu_1".into(),
            name: name.into(),
            input: serde_json::json!({}),
            parse_error: None,
        }
    }

    #[tokio::test]
    async fn floor_tool_is_denied_before_dispatch() {
        let (_tmp, deps) = deps_with_policy(ToolPolicy::default());
        let (content, _imgs, is_error) = invoke_tool(&deps, &call("CronCreate")).await;
        assert!(is_error);
        assert!(content.contains("host-owned"), "got: {content}");
    }

    #[tokio::test]
    async fn guest_sender_cannot_invoke_shell() {
        let policy = ToolPolicy::new(ToolProfile::Full, Some(SenderRole::Guest));
        let (_tmp, deps) = deps_with_policy(policy);
        let (content, _imgs, is_error) = invoke_tool(&deps, &call("shell")).await;
        assert!(is_error);
        assert!(content.contains("guest"), "got: {content}");
    }

    #[tokio::test]
    async fn skill_allowed_read_blocks_shell_at_dispatch() {
        // Headline Phase 1.1 case: a loaded skill with `allowed-tools:
        // [Read]` (→ read_file) — set on the ToolContext the way
        // `load_skill` does — blocks shell at the dispatch gate even under
        // a Coding profile. The base policy carries NO skill scope; the
        // dispatch gate reads it live from the context per call.
        let base = ToolPolicy::new(ToolProfile::Coding, None);
        let (_tmp, deps) = deps_with_policy_and_skill(base, Some(vec!["read_file".into()]));
        let (content, _imgs, is_error) = invoke_tool(&deps, &call("shell")).await;
        assert!(is_error);
        assert!(content.contains("active skill"), "got: {content}");
        // …and the one allowed tool still dispatches (no policy deny).
        let (allowed_content, _imgs, _err) = invoke_tool(&deps, &call("read_file")).await;
        assert!(
            !allowed_content.contains("active skill"),
            "read_file must survive the active-skill layer; got: {allowed_content}"
        );
    }

    #[tokio::test]
    async fn no_active_skill_leaves_dispatch_unscoped() {
        // With no skill loaded, the context reports no scope and a Coding
        // profile permits shell at dispatch.
        let base = ToolPolicy::new(ToolProfile::Coding, None);
        let (_tmp, deps) = deps_with_policy_and_skill(base, None);
        let (content, _imgs, _is_error) = invoke_tool(&deps, &call("shell")).await;
        assert!(
            !content.contains("active skill"),
            "no skill loaded must not narrow dispatch; got: {content}"
        );
    }

    #[tokio::test]
    async fn messaging_profile_blocks_shell_at_dispatch() {
        let policy = ToolPolicy::new(ToolProfile::Messaging, None);
        let (_tmp, deps) = deps_with_policy(policy);
        let (content, _imgs, is_error) = invoke_tool(&deps, &call("shell")).await;
        assert!(is_error);
        assert!(content.contains("messaging"), "got: {content}");
    }
}
