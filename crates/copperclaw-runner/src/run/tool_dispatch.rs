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
    // Layered authorization: sender role + active skill `allowed-tools`
    // + group tool-profile + the coarse provenance/autonomy gate. A deny
    // synthesises a model-facing `tool_result { is_error: true }` so the
    // model can self-correct.
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
    // M17 session-preview tools (`expose_preview` / `close_preview`) are
    // host-owned but ride the SAME host-broker relay as external MCP: the
    // runner writes a request row under the reserved `__preview` server name and
    // block-polls for the host's response. The policy gate above already ran
    // (both are coding-profile + credentialed-external). Reuse
    // `dispatch_external` with a synthetic route so there's one wait loop.
    if super::preview::is_preview_tool(&call.name) {
        let _hb = HeartbeatTicker::start(deps.heartbeat_path.clone());
        let route = super::preview::preview_route(&call.name);
        return super::external_mcp::dispatch_external(
            deps,
            &route,
            &call.name,
            call.input.clone(),
        )
        .await;
    }
    // External MCP tool? Route to the host-proxied request/response path
    // instead of the in-container tool_map. The policy gate above already ran
    // (an `mcp__`-prefixed name is credentialed-external, so it inherits the
    // provenance / autonomy block). Keep the heartbeat fresh across the
    // round-trip exactly like a local tool dispatch.
    if let Some(route) = deps.external_tools.get(&call.name) {
        let _hb = HeartbeatTicker::start(deps.heartbeat_path.clone());
        return super::external_mcp::dispatch_external(deps, route, &call.name, call.input.clone())
            .await;
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
    async fn profile_denied_tool_is_refused_before_dispatch() {
        // `shell` IS registered in the tool map (see deps_with_policy),
        // so an allow would dispatch it — the deny proves the policy
        // gate fires before the handler is reached.
        let policy = ToolPolicy::new(ToolProfile::Messaging, None);
        let (_tmp, deps) = deps_with_policy(policy);
        let (content, _imgs, is_error) = invoke_tool(&deps, &call("shell")).await;
        assert!(is_error);
        assert!(content.contains("messaging"), "got: {content}");
    }

    #[tokio::test]
    async fn unknown_tool_errors_at_dispatch_under_full() {
        // With the decorative DISALLOWED_TOOLS floor deleted (M18 R0),
        // a name outside the inventory passes the open Full profile and
        // fails at the tool-map lookup instead.
        let (_tmp, deps) = deps_with_policy(ToolPolicy::default());
        let (content, _imgs, is_error) = invoke_tool(&deps, &call("CronCreate")).await;
        assert!(is_error);
        assert!(content.contains("Unknown tool"), "got: {content}");
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
        // external action (web_fetch) is blocked absent fresh approval — even
        // though the Full profile would otherwise allow it.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        // Clean turn: web_fetch dispatches (it'll fail downstream without a
        // network stub, but NOT with a policy deny — that's what we assert).
        let (clean, _i, _e) = invoke_tool(&deps, &call("web_fetch")).await;
        assert!(
            !clean.contains("untrusted-provenance"),
            "a clean turn must not be gated; got: {clean}"
        );
        // Taint the turn the way a web_fetch body does.
        ctx.mark_untrusted_context("web_fetch:https://evil.example");
        let (blocked, _i, is_error) = invoke_tool(&deps, &call("web_fetch")).await;
        assert!(is_error);
        assert!(
            blocked.contains("untrusted-provenance"),
            "tainted turn must block credentialed external action; got: {blocked}"
        );
        // `web_search` is provider-pinned (taint-exempt) and stays reachable
        // on the tainted turn — see PROVIDER_PINNED_SEARCH_TOOLS.
        let (search, _i, _e) = invoke_tool(
            &deps,
            &call_with("web_search", serde_json::json!({"query": "x"})),
        )
        .await;
        assert!(
            !search.contains("untrusted-provenance"),
            "web_search must stay reachable on a tainted turn; got: {search}"
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
        // A second web_search stays REACHABLE on its own taint — it is
        // provider-pinned (no attacker-chosen endpoint), so iterative research
        // (search, read, refine, search again) is possible in one turn. It
        // still fails downstream without a provider key, but NOT with a
        // policy deny — see PROVIDER_PINNED_SEARCH_TOOLS.
        let (followup, _i, _e2) = invoke_tool(
            &deps,
            &call_with("web_search", serde_json::json!({"query": "again"})),
        )
        .await;
        assert!(
            !followup.contains("untrusted-provenance"),
            "a follow-up web_search must not be taint-gated; got: {followup}"
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

    #[tokio::test]
    async fn memory_save_writes_trusted_then_downgrades_when_tainted() {
        // A5: agent-facing memory write through the real runner ctx + store.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();

        // Clean turn: save is recorded as trusted and reads back as trusted.
        let (out, _i, err) = invoke_tool(
            &deps,
            &call_with(
                "memory_save",
                serde_json::json!({"key": "fact", "body": "the sky is blue", "source": "agent"}),
            ),
        )
        .await;
        assert!(!err, "trusted save should succeed; got: {out}");
        assert!(out.contains("\"provenance\": \"trusted\""), "got: {out}");
        assert!(out.contains("\"downgraded\": false"), "got: {out}");

        let (got, _i, _e) = invoke_tool(
            &deps,
            &call_with("memory_search", serde_json::json!({"query": "sky"})),
        )
        .await;
        assert!(got.contains("the sky is blue"), "got: {got}");
        assert!(got.contains("trusted"), "got: {got}");
        // A trusted-only round-trip must not have tainted the turn.
        assert!(
            !ctx.is_context_tainted(),
            "trusted save/search must not taint"
        );

        // Taint the turn (as a web_fetch / untrusted hit would), then a save is
        // honestly downgraded to untrusted — never laundered into trusted.
        ctx.mark_untrusted_context("test:taint");
        assert!(ctx.is_context_tainted());
        let (out2, _i, err2) = invoke_tool(
            &deps,
            &call_with(
                "memory_save",
                serde_json::json!({"key": "scraped", "body": "value from a web page"}),
            ),
        )
        .await;
        assert!(!err2, "downgraded save should still succeed; got: {out2}");
        assert!(
            out2.contains("\"provenance\": \"untrusted\""),
            "got: {out2}"
        );
        assert!(out2.contains("\"downgraded\": true"), "got: {out2}");

        let (got2, _i, _e) = invoke_tool(
            &deps,
            &call_with("memory_get", serde_json::json!({"key": "scraped"})),
        )
        .await;
        assert!(got2.contains("untrusted"), "got: {got2}");
    }

    fn call_with(name: &str, input: serde_json::Value) -> PendingToolCall {
        PendingToolCall {
            id: "tu_1".into(),
            name: name.into(),
            input,
            parse_error: None,
        }
    }

    // ── external MCP tools: host-proxied dispatch through invoke_tool ─────────

    use crate::run::external_mcp::ExternalToolRoute;
    use copperclaw_db::tables::mcp_calls::{self, McpCallResponse};

    #[tokio::test]
    async fn external_mcp_call_round_trips_through_invoke_tool_and_taints() {
        // Headline external-MCP case end-to-end through the dispatch gate: the
        // model calls a namespaced external tool, invoke_tool routes it to the
        // host-proxied path (request → host responds → result), and the result
        // taints the turn (external MCP output is attacker-influenceable).
        let (_tmp, mut deps, ctx) = deps_with_runner_ctx();
        let mut routes = HashMap::new();
        routes.insert(
            "mcp__weather__forecast".to_string(),
            ExternalToolRoute {
                server: "weather".into(),
                tool: "forecast".into(),
            },
        );
        deps.external_tools = Arc::new(routes);

        // A fake host: watch outbound for the request the runner writes, then
        // answer it on inbound (exactly what the delivery executor does).
        let outbound = deps.outbound.clone();
        let inbound = deps.inbound.clone();
        let host = tokio::spawn(async move {
            loop {
                let pending = {
                    let conn = outbound.lock().await;
                    mcp_calls::list_requests(&conn).unwrap()
                };
                if let Some(req) = pending.into_iter().next() {
                    assert_eq!(req.server, "weather");
                    assert_eq!(req.tool, "forecast");
                    assert_eq!(req.input["city"], "NYC");
                    let conn = inbound.lock().await;
                    mcp_calls::insert_response(
                        &conn,
                        &McpCallResponse {
                            request_id: req.request_id,
                            is_error: false,
                            result: "sunny, 72F".into(),
                        },
                    )
                    .unwrap();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        assert!(!ctx.is_context_tainted(), "fresh turn starts untainted");
        let (content, imgs, is_error) = invoke_tool(
            &deps,
            &call_with("mcp__weather__forecast", serde_json::json!({"city": "NYC"})),
        )
        .await;
        host.await.unwrap();

        assert!(!is_error, "successful external call must not be an error");
        assert!(content.contains("sunny"), "got: {content}");
        assert!(imgs.is_empty());
        assert!(
            ctx.is_context_tainted(),
            "an external MCP result must taint the turn (untrusted-provenance)"
        );

        // …and the request row was GC'd by the runner after consuming it.
        let leftover = {
            let conn = deps.outbound.lock().await;
            mcp_calls::list_requests(&conn).unwrap()
        };
        assert!(
            leftover.is_empty(),
            "runner must delete its consumed request"
        );
    }

    #[tokio::test]
    async fn expose_preview_writes_a_reserved_preview_request_row() {
        // M17: `expose_preview` is a first-party tool routed through the SAME
        // host-broker relay as external MCP, under the reserved `__preview`
        // server. invoke_tool must write a request row with server=`__preview`,
        // tool=`expose_preview`, carrying the port — then consume the host's
        // response.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        let outbound = deps.outbound.clone();
        let inbound = deps.inbound.clone();
        let host = tokio::spawn(async move {
            loop {
                let pending = {
                    let conn = outbound.lock().await;
                    mcp_calls::list_requests(&conn).unwrap()
                };
                if let Some(req) = pending.into_iter().next() {
                    assert_eq!(req.server, "__preview");
                    assert_eq!(req.tool, "expose_preview");
                    assert_eq!(req.input["port"], 3000);
                    let conn = inbound.lock().await;
                    mcp_calls::insert_response(
                        &conn,
                        &McpCallResponse {
                            request_id: req.request_id,
                            is_error: false,
                            result: "http://127.0.0.1:8100/__preview/tok\nnote".into(),
                        },
                    )
                    .unwrap();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let (content, imgs, is_error) = invoke_tool(
            &deps,
            &call_with("expose_preview", serde_json::json!({ "port": 3000 })),
        )
        .await;
        host.await.unwrap();

        assert!(!is_error, "got: {content}");
        assert!(content.contains("/__preview/tok"), "got: {content}");
        assert!(imgs.is_empty());
        // The preview response is host-composed (URL + note) — unlike a real
        // external MCP result it must NOT taint the turn. (The LAN preview verbs
        // are themselves exempt from the taint gate as of M19 A7, but keeping
        // the relay response clean still matters: it must not taint OTHER
        // credentialed external actions the same turn goes on to take.)
        assert!(
            !ctx.is_context_tainted(),
            "a __preview relay response must not taint the turn"
        );
        // The consumed request row was GC'd by the runner.
        let leftover = {
            let conn = deps.outbound.lock().await;
            mcp_calls::list_requests(&conn).unwrap()
        };
        assert!(leftover.is_empty());
    }

    #[tokio::test]
    async fn tainted_turn_exposes_lan_preview_but_is_blocked_from_public() {
        // X-rider W3 (A7): the reclassification driven end-to-end through the
        // real `invoke_tool` dispatch gate (A7's own tests are pure-policy unit
        // tests; this is the e2e-flavored complement the X-rider asks for).
        //
        // A web-tainted "research then build" turn:
        //   1. CAN `expose_preview` to the LAN — the M19 A7 carve-out exempts a
        //      LAN-only preview from the taint gate, so the relay actually runs
        //      and returns the shareable URL (before A7 this was denied as a
        //      credentialed external action, the M18 papercut).
        //   2. STILL CANNOT `make_preview_public` on the same tainted turn — the
        //      outward public verb is NOT in the LAN carve-out, so it is blocked
        //      at the policy gate BEFORE it can queue a `__preview` relay row.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        // Taint the turn exactly as a web_fetch body / untrusted memory hit
        // would (the "research then build" case).
        ctx.mark_untrusted_context("web_fetch:https://research.example");
        assert!(
            ctx.is_context_tainted(),
            "turn must be tainted for the case"
        );

        // A fake host answers the ONE expected `__preview` relay (expose_preview)
        // with a LAN URL, then stops. `make_preview_public` must never reach the
        // relay, so the host only ever sees the expose request.
        let outbound = deps.outbound.clone();
        let inbound = deps.inbound.clone();
        let host = tokio::spawn(async move {
            loop {
                let pending = {
                    let conn = outbound.lock().await;
                    mcp_calls::list_requests(&conn).unwrap()
                };
                if let Some(req) = pending.into_iter().next() {
                    assert_eq!(req.server, "__preview");
                    assert_eq!(
                        req.tool, "expose_preview",
                        "only the LAN verb may reach the relay on a tainted turn"
                    );
                    let conn = inbound.lock().await;
                    mcp_calls::insert_response(
                        &conn,
                        &McpCallResponse {
                            request_id: req.request_id,
                            is_error: false,
                            result: "http://192.168.1.9:8100/__preview/tok\nLAN only".into(),
                        },
                    )
                    .unwrap();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        // 1. LAN expose SUCCEEDS despite the taint (A7 carve-out).
        let (exposed, _i, expose_err) = invoke_tool(
            &deps,
            &call_with("expose_preview", serde_json::json!({ "port": 3000 })),
        )
        .await;
        host.await.unwrap();
        assert!(
            !expose_err,
            "a LAN expose_preview must succeed on a tainted turn (A7); got: {exposed}"
        );
        assert!(
            !exposed.contains("untrusted-provenance"),
            "expose_preview must NOT be taint-gated (A7); got: {exposed}"
        );
        assert!(
            exposed.contains("/__preview/tok"),
            "expose_preview must return the LAN URL; got: {exposed}"
        );

        // 2. The outward public verb on the SAME tainted turn is BLOCKED at the
        //    gate — untrusted-provenance deny, and it never queues a relay row.
        let (blocked, _i, public_err) = invoke_tool(
            &deps,
            &call_with("make_preview_public", serde_json::json!({ "port": 3000 })),
        )
        .await;
        assert!(
            public_err,
            "make_preview_public must be blocked on a tainted turn"
        );
        assert!(
            blocked.contains("untrusted-provenance"),
            "make_preview_public must be taint-gated (contrast to expose_preview); got: {blocked}"
        );
        let queued = {
            let conn = deps.outbound.lock().await;
            mcp_calls::list_requests(&conn).unwrap()
        };
        assert!(
            queued.is_empty(),
            "the blocked public verb must not queue a __preview relay row; got: {queued:?}"
        );
    }

    #[tokio::test]
    async fn external_mcp_call_blocked_on_tainted_turn_before_dispatch() {
        // Defense-in-depth: an external MCP call is a credentialed external
        // action, so a turn already tainted by untrusted-provenance content
        // blocks it at the policy gate — it never reaches the host-proxy path.
        let (_tmp, mut deps, ctx) = deps_with_runner_ctx();
        let mut routes = HashMap::new();
        routes.insert(
            "mcp__weather__forecast".to_string(),
            ExternalToolRoute {
                server: "weather".into(),
                tool: "forecast".into(),
            },
        );
        deps.external_tools = Arc::new(routes);

        ctx.mark_untrusted_context("web_fetch:https://evil.example");
        let (blocked, _imgs, is_error) = invoke_tool(
            &deps,
            &call_with("mcp__weather__forecast", serde_json::json!({"city": "NYC"})),
        )
        .await;
        assert!(is_error);
        assert!(
            blocked.contains("untrusted-provenance"),
            "tainted turn must block the external MCP call at the gate; got: {blocked}"
        );
        // Nothing was queued — the gate fired before the host-proxy path.
        let queued = {
            let conn = deps.outbound.lock().await;
            mcp_calls::list_requests(&conn).unwrap()
        };
        assert!(queued.is_empty(), "blocked call must not queue a request");
    }

    // ── delegate_batch (A1): fan-out + join end-to-end through invoke_tool ─────

    use copperclaw_db::tables::messages_in::{self, WriteInbound};
    use copperclaw_db::tables::messages_out;

    /// Build deps whose `RunnerToolCtx` has BOTH the inbound and outbound
    /// handles wired (`with_join`), so `delegate_batch` can spawn `Delegate`
    /// rows to outbound and block-poll inbound for the join. Returns the
    /// inbound/outbound handles so a fake host can drive the round-trip.
    fn deps_with_join() -> (
        tempfile::TempDir,
        RunnerDeps,
        Arc<Mutex<rusqlite::Connection>>,
        Arc<Mutex<rusqlite::Connection>>,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inbound = Arc::new(Mutex::new(open_inbound(&paths).unwrap()));
        let outbound = Arc::new(Mutex::new(open_outbound(&paths).unwrap()));
        let ctx =
            RunnerToolCtx::new(outbound.clone(), paths.outbox.clone()).with_join(inbound.clone());
        let tool_ctx: Arc<dyn ToolContext> = Arc::new(ctx);
        let provider: Arc<dyn AgentProvider> = Arc::new(NoopProvider);
        let archive_dir = paths.outbox.join("_compactions");
        let mut deps = RunnerDeps::minimal(
            provider,
            tool_ctx,
            inbound.clone(),
            outbound.clone(),
            archive_dir,
        );
        let mut map: HashMap<String, Arc<copperclaw_mcp::ToolEntry>> = HashMap::new();
        for e in copperclaw_mcp::build_tool_set() {
            map.insert(e.tool.name.to_string(), Arc::new(e));
        }
        deps.tool_map = Arc::new(map);
        deps.policy = ToolPolicy::new(ToolProfile::Full, None);
        (tmp, deps, inbound, outbound)
    }

    /// Fake host: for every `{"delegate": {...}}` System row the runner
    /// writes to outbound (not yet handled), write a `created`
    /// `delegate_result` into inbound (detail = the worker's instructions, so
    /// the join can attribute it) and then the worker's Chat report keyed by
    /// its child `session_id`. Optionally, `fail` forces a `rejected` result
    /// instead of `created` (models the depth cap).
    async fn fake_host_answer_delegates(
        inbound: &Arc<Mutex<rusqlite::Connection>>,
        outbound: &Arc<Mutex<rusqlite::Connection>>,
        fail: bool,
    ) {
        let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..200 {
            let rows = {
                let conn = outbound.lock().await;
                messages_out::list_due(&conn).unwrap()
            };
            for row in rows {
                let Some(d) = row.content.get("delegate") else {
                    continue;
                };
                let key = row.id.as_uuid().to_string();
                if !handled.insert(key) {
                    continue;
                }
                let instructions = d
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let name = d
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("w")
                    .to_owned();
                if fail {
                    write_inbound(
                        inbound,
                        copperclaw_types::MessageKind::System,
                        serde_json::json!({"delegate_result": {
                            "status": "rejected",
                            "detail": "nested create_agent (max depth = 3)"
                        }}),
                        None,
                    )
                    .await;
                    continue;
                }
                let sid = SessionId::new().as_uuid().to_string();
                write_inbound(
                    inbound,
                    copperclaw_types::MessageKind::System,
                    serde_json::json!({"delegate_result": {
                        "status": "created",
                        "session_id": sid,
                        "detail": instructions,
                    }}),
                    None,
                )
                .await;
                write_inbound(
                    inbound,
                    copperclaw_types::MessageKind::Chat,
                    serde_json::json!({"text": format!("{name} finished")}),
                    Some(sid),
                )
                .await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn write_inbound(
        inbound: &Arc<Mutex<rusqlite::Connection>>,
        kind: copperclaw_types::MessageKind,
        content: serde_json::Value,
        source_session_id: Option<String>,
    ) {
        let conn = inbound.lock().await;
        let msg = WriteInbound {
            id: copperclaw_types::MessageId::new(),
            kind,
            timestamp: chrono::Utc::now(),
            content,
            trigger: matches!(kind, copperclaw_types::MessageKind::Chat),
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id,
            reply_to: None,
            is_group: None,
        };
        messages_in::insert(&conn, &msg).unwrap();
    }

    #[tokio::test]
    async fn delegate_batch_fans_out_three_and_joins_one_result() {
        // Headline A1 acceptance: a parent `delegate_batch` of 3 workers
        // spawns three isolated delegates and returns ONE aggregated result
        // after all three report — through the real invoke_tool dispatch.
        let (_tmp, deps, inbound, outbound) = deps_with_join();
        let host = tokio::spawn({
            let inbound = inbound.clone();
            let outbound = outbound.clone();
            async move { fake_host_answer_delegates(&inbound, &outbound, false).await }
        });

        let (content, imgs, is_error) = invoke_tool(
            &deps,
            &call_with(
                "delegate_batch",
                serde_json::json!({
                    "workers": [
                        {"name": "api", "instructions": "build api under /workspace"},
                        {"name": "cli", "instructions": "build cli under /workspace"},
                        {"name": "docs", "instructions": "write docs under /workspace"}
                    ],
                    "timeout_secs": 10
                }),
            ),
        )
        .await;
        host.abort();

        assert!(
            !is_error,
            "a completed batch is not a tool error; got: {content}"
        );
        assert!(imgs.is_empty());
        // ONE aggregated response carrying every worker's report.
        assert!(content.contains("api finished"), "got: {content}");
        assert!(content.contains("cli finished"), "got: {content}");
        assert!(content.contains("docs finished"), "got: {content}");

        // Three isolated `Delegate` spawn rows were written to outbound —
        // each becomes its own container + `sib/<id>` worktree host-side.
        let spawn_rows = {
            let conn = outbound.lock().await;
            messages_out::list_due(&conn)
                .unwrap()
                .into_iter()
                .filter(|r| r.content.get("delegate").is_some())
                .count()
        };
        assert_eq!(
            spawn_rows, 3,
            "delegate_batch must spawn one delegate per worker"
        );

        // The join consumed every spawn-result + report row it owned — none
        // linger to re-trigger a spurious parent turn.
        let pending = {
            let conn = inbound.lock().await;
            messages_in::get_pending(&conn, true, 50).unwrap()
        };
        assert!(
            pending.is_empty(),
            "batch rows must be consumed; got {pending:?}"
        );
    }

    /// Fake host that answers a MIXED batch: every delegate row whose
    /// worker `name` is in `fail_names` gets a `rejected` spawn result (no
    /// child, no report — a per-worker spawn failure); every other worker is
    /// `created` and then reports. Models "one of N workers couldn't be
    /// spawned while the rest succeed" — the partial-failure case A1's join
    /// aggregates without losing the turn.
    async fn fake_host_answer_delegates_mixed(
        inbound: &Arc<Mutex<rusqlite::Connection>>,
        outbound: &Arc<Mutex<rusqlite::Connection>>,
        fail_names: &[&str],
    ) {
        let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..200 {
            let rows = {
                let conn = outbound.lock().await;
                messages_out::list_due(&conn).unwrap()
            };
            for row in rows {
                let Some(d) = row.content.get("delegate") else {
                    continue;
                };
                let key = row.id.as_uuid().to_string();
                if !handled.insert(key) {
                    continue;
                }
                let instructions = d
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let name = d
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("w")
                    .to_owned();
                if fail_names.contains(&name.as_str()) {
                    // A per-worker spawn failure: rejected result, no child.
                    write_inbound(
                        inbound,
                        copperclaw_types::MessageKind::System,
                        serde_json::json!({"delegate_result": {
                            "status": "rejected",
                            "detail": format!("worker `{name}` failed to spawn (disk full)")
                        }}),
                        None,
                    )
                    .await;
                    continue;
                }
                let sid = SessionId::new().as_uuid().to_string();
                write_inbound(
                    inbound,
                    copperclaw_types::MessageKind::System,
                    serde_json::json!({"delegate_result": {
                        "status": "created",
                        "session_id": sid,
                        "detail": instructions,
                    }}),
                    None,
                )
                .await;
                write_inbound(
                    inbound,
                    copperclaw_types::MessageKind::Chat,
                    serde_json::json!({"text": format!("{name} finished")}),
                    Some(sid),
                )
                .await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn delegate_batch_partial_worker_failure_surfaces_in_aggregate() {
        // X-rider W3 (A1): the partial-failure acceptance driven end-to-end
        // through the REAL `invoke_tool` dispatch (A1's own mixed test is at
        // the `join_workers` unit level / the mock-ctx handler level; this is
        // the runner-dispatch complement). A batch of 3 workers where ONE
        // fails to spawn while the other two build + report must return ONE
        // aggregated result — NOT a tool error (partial != total refusal), NOT
        // a lost turn — carrying both successful reports AND the failed
        // worker's per-worker error.
        let (_tmp, deps, inbound, outbound) = deps_with_join();
        let host = tokio::spawn({
            let inbound = inbound.clone();
            let outbound = outbound.clone();
            async move { fake_host_answer_delegates_mixed(&inbound, &outbound, &["docs"]).await }
        });

        let (content, imgs, is_error) = invoke_tool(
            &deps,
            &call_with(
                "delegate_batch",
                serde_json::json!({
                    "workers": [
                        {"name": "api", "instructions": "build api under /workspace"},
                        {"name": "cli", "instructions": "build cli under /workspace"},
                        {"name": "docs", "instructions": "write docs under /workspace"}
                    ],
                    "timeout_secs": 10
                }),
            ),
        )
        .await;
        host.abort();

        // A partial failure is NOT a tool error — the parent still gets the
        // two successful reports to assemble.
        assert!(
            !is_error,
            "a partial failure must still return an aggregate, not a tool error; got: {content}"
        );
        assert!(imgs.is_empty());
        // Both successful workers' reports are in the ONE aggregate.
        assert!(content.contains("api finished"), "got: {content}");
        assert!(content.contains("cli finished"), "got: {content}");
        // …and the failed worker surfaces as a per-worker error, not silence.
        assert!(
            content.contains("docs") && content.contains("failed to spawn"),
            "the failed worker must surface in the aggregate as a per-worker error; got: {content}"
        );
        // The docs worker never produced a success report.
        assert!(
            !content.contains("docs finished"),
            "the failed worker must not report success; got: {content}"
        );

        // Every batch row (2 created + 2 reports + 1 rejected) was consumed —
        // none linger to re-trigger a spurious parent turn.
        let pending = {
            let conn = inbound.lock().await;
            messages_in::get_pending(&conn, true, 50).unwrap()
        };
        assert!(
            pending.is_empty(),
            "batch rows must be consumed; got {pending:?}"
        );
    }

    #[tokio::test]
    async fn delegate_batch_from_max_depth_child_is_refused() {
        // Depth-cap acceptance: when the host rejects every spawn (a batch
        // from a max-depth child), the tool surfaces a refusal (is_error),
        // not a partial aggregate.
        let (_tmp, deps, inbound, outbound) = deps_with_join();
        let host = tokio::spawn({
            let inbound = inbound.clone();
            let outbound = outbound.clone();
            async move { fake_host_answer_delegates(&inbound, &outbound, true).await }
        });

        let (content, _imgs, is_error) = invoke_tool(
            &deps,
            &call_with(
                "delegate_batch",
                serde_json::json!({
                    "workers": [
                        {"name": "a", "instructions": "x"},
                        {"name": "b", "instructions": "y"}
                    ],
                    "timeout_secs": 10
                }),
            ),
        )
        .await;
        host.abort();

        assert!(is_error, "a fully-refused batch must be a tool error");
        assert!(
            content.contains("refused") && content.contains("max depth"),
            "refusal must name the depth cap; got: {content}"
        );
    }

    #[tokio::test]
    async fn delegate_batch_without_join_wiring_refuses_cleanly() {
        // A ctx with no inbound handle (never wired `with_join`) must refuse
        // rather than hang or panic — proves the batch never silently no-ops.
        let (_tmp, deps, _ctx) = deps_with_runner_ctx();
        let (content, _imgs, is_error) = invoke_tool(
            &deps,
            &call_with(
                "delegate_batch",
                serde_json::json!({"workers": [{"name": "a", "instructions": "x"}]}),
            ),
        )
        .await;
        assert!(is_error);
        assert!(content.contains("not wired"), "got: {content}");
    }
}
