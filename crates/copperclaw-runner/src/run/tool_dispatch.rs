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
    // The coarse provenance / autonomy gate (Phase 3) is also dynamic: a
    // `web_fetch` or an untrusted `memory_search` hit earlier in THIS turn
    // taints the context, after which a credentialed external action
    // (`web_fetch` / `web_search` / `install_packages` / `add_mcp_server`)
    // is blocked until fresh approval. An autonomous (heartbeat) turn blocks
    // those actions outright. We read the live signals off the context and
    // stamp them onto the per-call policy alongside the active-skill scope.
    let trust = crate::policy::TurnTrust {
        tainted: deps.tool_ctx.is_context_tainted(),
        approved: deps.tool_ctx.external_action_approved(),
        autonomous: deps.tool_ctx.is_autonomous_turn(),
    };
    let policy = deps
        .policy
        .clone()
        .with_active_skill(deps.tool_ctx.active_skill_allowed_tools())
        .with_trust(trust);
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

    /// Build deps whose `ToolContext` is a *real* `RunnerToolCtx` (so the
    /// provenance signals — taint / autonomous — are honoured by the dispatch
    /// gate) backed by a temp memory store. Returns the ctx separately so the
    /// test can poke `mark_untrusted_context` / `set_turn_provenance`.
    fn deps_with_runner_ctx() -> (tempfile::TempDir, RunnerDeps, Arc<RunnerToolCtx>) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let mem_db = paths.root.join("memory").join("memory.db");
        let ctx = Arc::new(
            RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()).with_memory_db(mem_db),
        );
        let tool_ctx: Arc<dyn ToolContext> = ctx.clone();
        let provider: Arc<dyn AgentProvider> = Arc::new(NoopProvider);
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(provider, tool_ctx, inbound, outbound, archive_dir);
        let mut map: HashMap<String, Arc<copperclaw_mcp::ToolEntry>> = HashMap::new();
        for e in copperclaw_mcp::build_tool_set() {
            map.insert(e.tool.name.to_string(), Arc::new(e));
        }
        deps.tool_map = Arc::new(map);
        deps.policy = ToolPolicy::new(ToolProfile::Full, None);
        (tmp, deps, ctx)
    }

    #[tokio::test]
    async fn untrusted_context_blocks_credentialed_external_at_dispatch() {
        // Headline Phase 3 case wired end-to-end through the dispatch gate:
        // once the turn is tainted (as a web_fetch body would), a credentialed
        // external action (web_search) is blocked absent fresh approval — even
        // though the Full profile would otherwise allow it.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        // Clean turn: web_search dispatches (it'll fail downstream without a
        // network stub, but NOT with a policy deny — that's what we assert).
        let (clean, _i, _e) = invoke_tool(&deps, &call("web_search")).await;
        assert!(
            !clean.contains("untrusted-provenance"),
            "a clean turn must not be gated; got: {clean}"
        );
        // Taint the turn the way a web_fetch body does.
        ctx.mark_untrusted_context("web_fetch:https://evil.example");
        let (blocked, _i, is_error) = invoke_tool(&deps, &call("web_search")).await;
        assert!(is_error);
        assert!(
            blocked.contains("untrusted-provenance"),
            "tainted turn must block credentialed external action; got: {blocked}"
        );
        // Non-credentialed tools still pass on a tainted turn.
        let (mem, _i, _e) = invoke_tool(
            &deps,
            &call_with("memory_search", serde_json::json!({"query":"x"})),
        )
        .await;
        assert!(
            !mem.contains("untrusted-provenance"),
            "memory_search must stay reachable on a tainted turn; got: {mem}"
        );
    }

    #[tokio::test]
    async fn web_search_run_taints_turn_then_blocks_credentialed_external() {
        // END-TO-END headline case for this change: running `web_search`
        // through the real dispatch path must mark the turn untrusted-
        // provenance (its results are attacker-influenceable), after which a
        // subsequent credentialed external action trips the coarse gate absent
        // fresh approval — even under the Full profile.
        //
        // No provider key is configured in the test env, so the web_search
        // handler's downstream provider lookup errors — but the taint hook
        // fires BEFORE that, which is the whole point: the turn is tainted the
        // moment the tool ran, regardless of the network outcome.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        assert!(!ctx.is_context_tainted(), "fresh turn must start untainted");
        // A credentialed external action on the still-clean turn is NOT gated
        // (it'll fail downstream without a network stub, but not with a policy
        // deny — that's what we assert).
        let (clean, _i, _e) = invoke_tool(&deps, &call("install_packages")).await;
        assert!(
            !clean.contains("untrusted-provenance"),
            "clean turn must not be gated; got: {clean}"
        );
        // Run web_search itself. The handler taints the turn up front.
        let (_searched, _i, _e) = invoke_tool(
            &deps,
            &call_with("web_search", serde_json::json!({"query": "anything"})),
        )
        .await;
        assert!(
            ctx.is_context_tainted(),
            "running web_search must taint the turn untrusted-provenance"
        );
        // Now a subsequent credentialed external action trips the gate.
        let (blocked, _i, is_error) = invoke_tool(&deps, &call("install_packages")).await;
        assert!(is_error);
        assert!(
            blocked.contains("untrusted-provenance"),
            "tainted turn must block credentialed external action; got: {blocked}"
        );
        // A second web_search is likewise blocked now (it is itself a
        // credentialed external action, and the turn is tainted).
        let (blocked2, _i, is_error2) = invoke_tool(
            &deps,
            &call_with("web_search", serde_json::json!({"query": "again"})),
        )
        .await;
        assert!(is_error2);
        assert!(
            blocked2.contains("untrusted-provenance"),
            "a tainted turn must block a follow-up web_search too; got: {blocked2}"
        );
        // Non-credentialed tools stay reachable on the tainted turn.
        let (mem, _i, _e) = invoke_tool(
            &deps,
            &call_with("memory_search", serde_json::json!({"query": "x"})),
        )
        .await;
        assert!(
            !mem.contains("untrusted-provenance"),
            "memory_search must stay reachable on a tainted turn; got: {mem}"
        );
    }

    #[tokio::test]
    async fn turn_with_only_trusted_tools_does_not_trip_gate() {
        // Control for the headline case: a turn that runs only trusted tools
        // (here a local read_file) must NOT taint, so a subsequent credentialed
        // external action is permitted by the provenance gate.
        let (tmp, deps, ctx) = deps_with_runner_ctx();
        // Create a file the read_file tool can actually read so the call
        // succeeds end-to-end (a trusted, local read).
        let file = tmp.path().join("note.txt");
        std::fs::write(&file, "local trusted content").unwrap();
        let (read, _i, read_err) = invoke_tool(
            &deps,
            &call_with(
                "read_file",
                serde_json::json!({"path": file.to_string_lossy()}),
            ),
        )
        .await;
        assert!(
            !read_err,
            "read_file of a local file should succeed: {read}"
        );
        assert!(
            !ctx.is_context_tainted(),
            "a trusted local read must not taint the turn"
        );
        // The credentialed external action is NOT gated by provenance (it may
        // still fail downstream for lack of a network stub, but must not carry
        // the untrusted-provenance deny).
        let (after, _i, _e) = invoke_tool(&deps, &call("install_packages")).await;
        assert!(
            !after.contains("untrusted-provenance"),
            "trusted-only turn must not trip the provenance gate; got: {after}"
        );
    }

    #[tokio::test]
    async fn autonomous_turn_blocks_credentialed_external_at_dispatch() {
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        ctx.set_turn_provenance(true, false); // autonomous, no approval
        let (blocked, _i, is_error) = invoke_tool(&deps, &call("web_fetch")).await;
        assert!(is_error);
        assert!(
            blocked.contains("autonomous"),
            "autonomous turn must block credentialed external action; got: {blocked}"
        );
        // But it can still search memory and propose.
        let (mem, _i, _e) = invoke_tool(
            &deps,
            &call_with("memory_search", serde_json::json!({"query": "x"})),
        )
        .await;
        assert!(
            !mem.contains("autonomous"),
            "memory search must stay reachable; got: {mem}"
        );
    }

    #[tokio::test]
    async fn memory_search_get_roundtrip_through_runner_ctx() {
        // memory_search / memory_get resolve against the per-group store the
        // runner ctx opens, and an untrusted hit taints the turn.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        // Seed the store directly (the write side lives host/runner-side; this
        // mirrors what an agent-authored note + a fetched-and-stored snippet
        // would produce).
        {
            let store =
                copperclaw_db::memory::MemoryStore::open(ctx.memory_db_path_for_test().unwrap())
                    .unwrap();
            store
                .upsert(&copperclaw_db::memory::MemoryWrite {
                    key: "runbook",
                    body: "telegram deploy steps",
                    provenance: copperclaw_db::memory::Provenance::Trusted,
                    source: None,
                    embedding: &[],
                })
                .unwrap();
            store
                .upsert(&copperclaw_db::memory::MemoryWrite {
                    key: "scraped",
                    body: "telegram untrusted scraped content",
                    provenance: copperclaw_db::memory::Provenance::Untrusted,
                    source: Some("web_fetch:https://x"),
                    embedding: &[],
                })
                .unwrap();
        }
        // memory_get of the trusted entry does NOT taint the turn.
        let (got, _i, err) = invoke_tool(
            &deps,
            &call_with("memory_get", serde_json::json!({"key": "runbook"})),
        )
        .await;
        assert!(!err, "memory_get should succeed; got: {got}");
        assert!(got.contains("telegram deploy steps"), "got: {got}");
        assert!(!ctx.is_context_tainted(), "a trusted hit must not taint");
        // A search that surfaces the untrusted entry taints the turn.
        let (found, _i, _e) = invoke_tool(
            &deps,
            &call_with("memory_search", serde_json::json!({"query": "telegram"})),
        )
        .await;
        assert!(
            found.contains("untrusted"),
            "search should surface the untrusted hit; got: {found}"
        );
        assert!(
            ctx.is_context_tainted(),
            "an untrusted hit must taint the turn for the coarse gate"
        );
        // …and a credentialed external action is now blocked.
        let (blocked, _i, is_error) = invoke_tool(&deps, &call("web_fetch")).await;
        assert!(
            is_error && blocked.contains("untrusted-provenance"),
            "got: {blocked}"
        );
    }

    fn call_with(name: &str, input: serde_json::Value) -> PendingToolCall {
        PendingToolCall {
            id: "tu_1".into(),
            name: name.into(),
            input,
            parse_error: None,
        }
    }
}
