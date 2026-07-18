//! Per-tool dispatch: invoke one model-requested tool call against the
//! runner's tool map, wrap it in a heartbeat ticker + deadline, and render
//! the result into the `Tool` history block the model sees next turn.

use crate::policy::PolicyDecision;

use super::RunnerDeps;
use super::drive_turn::PendingToolCall;
use super::provider_call::HeartbeatTicker;

use chrono::{DateTime, Utc};

/// M22 A2 — the autonomy gate.
///
/// A scheduled / heartbeat (autonomous) turn is blocked from every
/// credentialed external action by default (`policy.rs` layer 4). This card
/// opens that gate — but **only** per-task, bounded, and pre-authorized: an
/// autonomous fire may take a credentialed external action ONLY when the
/// firing task carries a live, human-approved capability grant (M22 A1) whose
/// scope permits *that specific action*. Everything else stays blocked and
/// falls to read-then-propose (an approval/wall card).
///
/// # Why the runner
///
/// Self-generated wakes bypass the router (decision **b**), so the gate lives
/// in the runner where every autonomous turn's tool dispatch is funnelled
/// through [`invoke_tool`]. There is no path from an autonomous turn to an
/// external sink that does not pass this function.
///
/// # How the grant reaches the runner
///
/// The runner runs in-container and cannot reach the central `task_grants`
/// table directly (it lives outside the bind mount — the same reason
/// `tasks.json` is a host-written snapshot). So the host writes the firing
/// task's *effective* grant — the exact output of
/// `copperclaw_db::tables::task_grants::effective_grant` — into
/// `<data_root>/grant.json` at fire/spawn time (see the A2 security review for
/// the required companion writer + budget-writeback plumbing). `run_loop`
/// loads it at turn start via [`super::load_turn_grant`] and stashes it on
/// [`RunnerDeps::active_grant`]; this dispatch gate consults it per call.
///
/// The on-disk shape is exactly this struct. `effective_grant` only ever
/// produces a *live* grant, but the runner re-checks liveness ([`Self::is_live`])
/// as defence-in-depth against a stale snapshot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct TurnGrant {
    /// The `task_grants.id` this snapshot came from (for the consume writeback).
    pub grant_id: String,
    /// The firing task this grant belongs to. `run_loop` verifies it matches
    /// the turn's originating task so a stale snapshot for a *different* task
    /// can never authorize this fire.
    pub task_id: String,
    /// Space-separated scope tokens; the A1 `class` / `class:resource` grammar.
    pub capability_scope: String,
    /// Tokens left before the grant reads inert. `None` = unbounded.
    #[serde(default)]
    pub tokens_remaining: Option<i64>,
    /// Fires left before the grant reads inert. `None` = unbounded.
    #[serde(default)]
    pub fires_remaining: Option<i64>,
    /// Hard expiry. `None` = no expiry.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl TurnGrant {
    /// Does this grant's scope permit an action requiring capability
    /// `required`? Reuses A1's authoritative matcher — case-sensitive, no
    /// cross-class widening, empty scope permits nothing.
    pub(crate) fn permits(&self, required: &str) -> bool {
        copperclaw_db::tables::task_grants::scope_permits(&self.capability_scope, required)
    }

    /// Defence-in-depth liveness re-check against the runner's own clock: not
    /// expired, and budget/fires (when bounded) not exhausted. The host only
    /// writes a live grant, but a snapshot can go stale between spawn and a
    /// long turn, and a bug in the writer must fail *closed*.
    pub(crate) fn is_live(&self, now: DateTime<Utc>) -> bool {
        if let Some(exp) = self.expires_at {
            if now >= exp {
                return false;
            }
        }
        if matches!(self.fires_remaining, Some(f) if f <= 0) {
            return false;
        }
        if matches!(self.tokens_remaining, Some(t) if t <= 0) {
            return false;
        }
        true
    }
}

/// Per-turn autonomy-gate state stashed on [`RunnerDeps::active_grant`].
/// `run_loop` rewrites it at the top of every turn (the grant for an
/// autonomous fire, or `None` for a human turn), resetting `fire_consumed` so
/// each fire is charged at most once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GrantGateState {
    /// The live grant governing this autonomous turn, or `None` when the turn
    /// is human-driven or has no usable grant.
    pub grant: Option<TurnGrant>,
    /// Set once this turn has charged its one fire against the grant, so a
    /// multi-action turn consumes exactly one fire (a fire = one turn).
    pub fire_consumed: bool,
}

/// Map a model-requested tool call to the capability token the autonomy gate
/// requires the grant to permit. **The token is derived only from the tool
/// name and its structural arguments — never from model- or content-supplied
/// free text** — so a prompt-injected turn cannot widen its own authorization:
/// to run `install_packages` the grant must literally carry `install_packages`
/// (or a bare class token), and `mcp__gh__*` requires `mcp:gh`.
///
/// The mapping is intentionally narrow and fail-closed: a tool with no special
/// case maps to its bare tool name as the required class, so a resource-scoped
/// grant that does not name it never matches.
fn required_capability(name: &str, input: &serde_json::Value) -> String {
    // External MCP tools (`mcp__<server>__<tool>`) → `mcp:<server>`: the grant
    // authorizes a whole external server, keyed off the runtime-controlled
    // namespace, never the tool arguments.
    if let Some(rest) = name.strip_prefix(crate::policy::EXTERNAL_MCP_PREFIX) {
        let server = rest.split("__").next().unwrap_or(rest);
        return format!("mcp:{server}");
    }
    match name {
        // `web_fetch` → `web_fetch:<host>` when the URL host is parseable, so a
        // grant can be scoped to one host (`web_fetch:example.com`) while a bare
        // `web_fetch` class grant still covers it. An unparseable URL falls back
        // to the bare class — which a host-scoped grant deliberately will NOT
        // match (fail closed).
        "web_fetch" => match url_host(input) {
            Some(host) => format!("web_fetch:{host}"),
            None => "web_fetch".to_string(),
        },
        // Every other credentialed-external tool maps to its own name as the
        // required capability class.
        other => other.to_string(),
    }
}

/// Extract a lower-cased host from a tool call's `url` argument, if present and
/// parseable with a minimal, dependency-free parse. Userinfo and port are
/// stripped so `https://user:pw@Example.com:8443/x` → `example.com`.
fn url_host(input: &serde_json::Value) -> Option<String> {
    let url = input.get("url").and_then(|v| v.as_str())?;
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    // Strip any userinfo (`user:pw@host`), then the port.
    let host_port = authority.rsplit('@').next()?;
    let host = host_port.split(':').next()?.trim();
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The outcome of consulting the autonomy grant for one autonomous
/// credentialed-external call.
enum AutonomyVerdict {
    /// Not gated here (not autonomous, or not a credentialed external action).
    /// The existing policy layers decide.
    NotGated,
    /// A live grant permits this exact action: open the autonomy gate for THIS
    /// call only (the taint gate still applies independently). Carries the
    /// grant so the fire can be charged when the action actually dispatches.
    Granted(TurnGrant),
    /// An autonomous credentialed-external action that is present-but-out-of-
    /// scope on a live grant: stays blocked with a model-facing, classifiable
    /// reason (read-then-propose).
    Blocked(String),
}

/// Consult the per-turn autonomy grant for `call`. See [`AutonomyVerdict`].
/// Pure over the gate state + the current time so it is unit-testable without a
/// live turn.
fn autonomy_verdict(
    autonomous: bool,
    grant: Option<&TurnGrant>,
    call_name: &str,
    input: &serde_json::Value,
    now: DateTime<Utc>,
) -> AutonomyVerdict {
    // Only autonomous turns are gated, and only for the credentialed-external
    // action set (the exact set `policy.rs` blocks outright on an autonomous
    // turn). Everything else — memory search, messaging, local tools — always
    // passes so an autonomous turn can still read-then-propose.
    if !autonomous || !crate::policy::is_credentialed_external(call_name) {
        return AutonomyVerdict::NotGated;
    }
    let required = required_capability(call_name, input);
    match grant {
        Some(g) if g.is_live(now) && g.permits(&required) => AutonomyVerdict::Granted(g.clone()),
        Some(g) if g.is_live(now) => AutonomyVerdict::Blocked(format!(
            "Tool `{call_name}` takes a credentialed external action requiring capability \
             `{required}`, which is outside this task's approved grant (scope: `{}`) on this \
             autonomous (heartbeat/scheduled) turn. Search memory and propose the action for a \
             human turn to approve instead.",
            g.capability_scope
        )),
        // No grant, or the snapshot is present but inert (expired / exhausted).
        // Fall through to the existing policy autonomous deny, which carries the
        // same stable `autonomous (heartbeat/scheduled) turn` hint the blocker
        // classifier keys on.
        _ => AutonomyVerdict::NotGated,
    }
}

/// Charge one fire against the active grant, at most once per turn. Called when
/// a granted autonomous action is about to actually dispatch (a fire is
/// happening). Emits a `grant_consume` System row to `outbound.db`; the host's
/// delivery loop applies it to the central grant via
/// `task_grants::consume_fire` (companion plumbing — see the A2 security
/// review). Best-effort: a failed emit must not abort the turn.
async fn charge_grant_fire_once(deps: &RunnerDeps, grant: &TurnGrant) {
    use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_out};
    {
        // Dedupe within the turn: a fire is one *turn*, not one action.
        let mut gate = match deps.active_grant.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if gate.fire_consumed {
            return;
        }
        gate.fire_consumed = true;
    }
    let payload = serde_json::json!({
        "grant_consume": {
            "grant_id": grant.grant_id,
            "task_id": grant.task_id,
            "fires": 1,
        }
    });
    let row = WriteOutbound {
        id: copperclaw_types::MessageId::new(),
        in_reply_to: None,
        timestamp: Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: copperclaw_types::MessageKind::System,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        content: payload,
    };
    let outbound = deps.outbound.lock().await;
    if let Err(err) = insert_out(&outbound, &row) {
        tracing::warn!(?err, grant_id = %grant.grant_id, "grant_consume insert failed");
    }
}

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
    let mut trust = crate::policy::TurnTrust {
        tainted: deps.tool_ctx.is_context_tainted(),
        approved: deps.tool_ctx.external_action_approved(),
        autonomous: deps.tool_ctx.is_autonomous_turn(),
    };
    // M22 A2 — the autonomy gate. On an autonomous (scheduled/heartbeat) turn a
    // credentialed external action is blocked by default; the ONLY thing that
    // opens it is a live, human-approved capability grant (M22 A1) whose scope
    // permits THIS specific action. `external_action_approved()` (the blanket
    // taint-clearing bool) is deliberately NOT consulted for the autonomy
    // decision — that bool never flips `true` for an autonomous turn. Consulting
    // the grant per call is what makes the approval capability-scoped rather
    // than blanket. See [`TurnGrant`] / [`autonomy_verdict`].
    let active_grant = {
        let gate = match deps.active_grant.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        gate.grant.clone()
    };
    let mut granted_fire: Option<TurnGrant> = None;
    match autonomy_verdict(
        trust.autonomous,
        active_grant.as_ref(),
        &call.name,
        &call.input,
        Utc::now(),
    ) {
        AutonomyVerdict::NotGated => {}
        AutonomyVerdict::Granted(g) => {
            // Scoped autonomy approval for THIS call only: drop the autonomous
            // block so the policy layers admit the action. The taint gate still
            // applies independently (a grant opens autonomy, not taint), so a
            // granted action on a web-tainted turn stays blocked until fresh
            // approval — exactly as before.
            trust.autonomous = false;
            granted_fire = Some(g);
        }
        AutonomyVerdict::Blocked(reason) => {
            tracing::info!(tool = %call.name, %reason, "autonomous action outside task grant scope");
            return (reason, Vec::new(), true);
        }
    }
    let policy = deps
        .policy
        .clone()
        .with_active_skill(deps.tool_ctx.active_skill_allowed_tools())
        .with_trust(trust);
    if let PolicyDecision::Deny(reason) = policy.evaluate(&call.name) {
        tracing::info!(tool = %call.name, %reason, "tool call denied by policy");
        return (reason, Vec::new(), true);
    }
    // A2: a granted autonomous action survived every policy layer and is about
    // to dispatch — charge one fire against the grant (once per turn). Placed
    // AFTER the policy allow so an action the taint gate still blocks is never
    // charged (block-not-charge).
    if let Some(grant) = &granted_fire {
        charge_grant_fire_once(deps, grant).await;
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
        Ok(Ok(result)) => {
            // M22 C6: the see→fix (screenshot) gate's dispatch-level side
            // effects — a successful `ui_screenshot` satisfies the loop and
            // clears pending markers; a successful edit-family tool re-opens
            // it. This is the one layer that observes every tool call, so the
            // clear (whose tool is out of C6's scope) lives here.
            apply_see_fix_hooks(deps, &call.name).await;
            (
                render_tool_result(&result),
                extract_tool_images(&result),
                false,
            )
        }
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

/// C6: which see→fix side effect a just-succeeded tool call triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeeFixAction {
    /// A fresh `ui_screenshot` satisfied the loop — clear pending markers.
    Clear,
    /// An edit-family tool re-opened the loop — mark UI tasks as needing a
    /// post-fix screenshot.
    Mark,
    /// The tool is irrelevant to the see→fix gate.
    None,
}

/// Decide the see→fix side effect for a successful `tool_name`. Keyed off
/// the tool NAME (not per-tool arg parsing) so it stays tool-agnostic
/// across the edit tools C6 does not own. `write_file` is deliberately
/// absent: its own handler marks the specific project it touched
/// (`computer_use::write_file`), so re-marking here would be redundant.
/// `ui_screenshot` is here — it is out of C6's file scope, and this
/// dispatch layer is the one place that observes every tool call, so its
/// clear must live here.
fn see_fix_action(tool_name: &str) -> SeeFixAction {
    match tool_name {
        "ui_screenshot" => SeeFixAction::Clear,
        "edit_file" | "multi_edit" | "apply_patch" | "copy_file" => SeeFixAction::Mark,
        _ => SeeFixAction::None,
    }
}

/// C6: apply a successful tool call's see→fix side effect. Gated by the
/// shared verify-gate off-switch (decision (d): one switch for the whole
/// gate family). Best-effort filesystem work — a no-op when the session
/// has no UI task, or when the tool is irrelevant to the gate.
async fn apply_see_fix_hooks(deps: &RunnerDeps, tool_name: &str) {
    let action = see_fix_action(tool_name);
    if action == SeeFixAction::None || !deps.tool_ctx.verify_gate_enabled() {
        return;
    }
    match action {
        SeeFixAction::Clear => {
            copperclaw_mcp::tools::self_review::clear_all_needs_screenshot().await;
        }
        SeeFixAction::Mark => {
            copperclaw_mcp::tools::self_review::mark_all_ui_tasks_need_screenshot().await;
        }
        SeeFixAction::None => {}
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

    #[test]
    fn see_fix_action_maps_tool_names() {
        // C6: the dispatch-hook decision is a pure name → action mapping.
        // ui_screenshot clears the loop; the edit-family tools re-open it;
        // write_file is handled by its own tool handler (so it's None here);
        // everything else is irrelevant.
        assert_eq!(see_fix_action("ui_screenshot"), SeeFixAction::Clear);
        assert_eq!(see_fix_action("edit_file"), SeeFixAction::Mark);
        assert_eq!(see_fix_action("multi_edit"), SeeFixAction::Mark);
        assert_eq!(see_fix_action("apply_patch"), SeeFixAction::Mark);
        assert_eq!(see_fix_action("copy_file"), SeeFixAction::Mark);
        assert_eq!(see_fix_action("write_file"), SeeFixAction::None);
        assert_eq!(see_fix_action("read_file"), SeeFixAction::None);
        assert_eq!(see_fix_action("shell"), SeeFixAction::None);
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

    // ── M22 A2: the autonomy gate (capability grants) ─────────────────────

    /// A grant with the given scope, unbounded budget/fires and no expiry.
    fn live_grant(scope: &str) -> TurnGrant {
        TurnGrant {
            grant_id: "g-1".into(),
            task_id: "t-1".into(),
            capability_scope: scope.into(),
            tokens_remaining: None,
            fires_remaining: None,
            expires_at: None,
        }
    }

    /// Install (or clear) the turn grant on `deps`, resetting the fire latch.
    fn install_grant(deps: &RunnerDeps, grant: Option<TurnGrant>) {
        let mut gate = deps.active_grant.lock().unwrap();
        gate.grant = grant;
        gate.fire_consumed = false;
    }

    /// Count `grant_consume` fire rows the runner emitted to `outbound`.
    async fn grant_consume_rows(deps: &RunnerDeps) -> usize {
        let conn = deps.outbound.lock().await;
        messages_out::list_due(&conn)
            .unwrap()
            .into_iter()
            .filter(|r| r.content.get("grant_consume").is_some())
            .count()
    }

    #[test]
    fn required_capability_maps_each_action() {
        use serde_json::json;
        assert_eq!(
            required_capability("install_packages", &json!({})),
            "install_packages"
        );
        assert_eq!(required_capability("web_search", &json!({})), "web_search");
        assert_eq!(
            required_capability("make_preview_public", &json!({})),
            "make_preview_public"
        );
        // External MCP → `mcp:<server>`, keyed off the runtime namespace.
        assert_eq!(
            required_capability("mcp__gh__create_issue", &json!({})),
            "mcp:gh"
        );
        // web_fetch → host-scoped when the URL parses (userinfo/port stripped,
        // lower-cased), bare class otherwise.
        assert_eq!(
            required_capability("web_fetch", &json!({"url": "https://Example.com/x?y"})),
            "web_fetch:example.com"
        );
        assert_eq!(
            required_capability(
                "web_fetch",
                &json!({"url": "https://user:pw@Host.Example:8443/p"})
            ),
            "web_fetch:host.example"
        );
        assert_eq!(required_capability("web_fetch", &json!({})), "web_fetch");
    }

    #[test]
    fn autonomy_verdict_gates_only_autonomous_credentialed_external() {
        use serde_json::json;
        let now = Utc::now();
        let g = live_grant("web_fetch");
        // A human (non-autonomous) turn is never gated here.
        assert!(matches!(
            autonomy_verdict(false, Some(&g), "web_fetch", &json!({}), now),
            AutonomyVerdict::NotGated
        ));
        // An autonomous turn's non-credentialed tool is never gated.
        assert!(matches!(
            autonomy_verdict(true, Some(&g), "read_file", &json!({}), now),
            AutonomyVerdict::NotGated
        ));
        // In scope → Granted.
        assert!(matches!(
            autonomy_verdict(true, Some(&g), "web_fetch", &json!({}), now),
            AutonomyVerdict::Granted(_)
        ));
        // Live grant, out of scope → Blocked (read-then-propose).
        assert!(matches!(
            autonomy_verdict(true, Some(&g), "install_packages", &json!({}), now),
            AutonomyVerdict::Blocked(_)
        ));
        // No grant → NotGated, so the policy layer's blanket autonomous deny
        // fires downstream (still blocked, just not by this gate).
        assert!(matches!(
            autonomy_verdict(true, None, "install_packages", &json!({}), now),
            AutonomyVerdict::NotGated
        ));
    }

    #[test]
    fn autonomy_verdict_inert_grant_authorizes_nothing() {
        use serde_json::json;
        let now = Utc::now();
        // Expired.
        let mut g = live_grant("web_fetch");
        g.expires_at = Some(now - chrono::Duration::minutes(1));
        assert!(matches!(
            autonomy_verdict(true, Some(&g), "web_fetch", &json!({}), now),
            AutonomyVerdict::NotGated
        ));
        // Fires exhausted.
        let mut g = live_grant("web_fetch");
        g.fires_remaining = Some(0);
        assert!(matches!(
            autonomy_verdict(true, Some(&g), "web_fetch", &json!({}), now),
            AutonomyVerdict::NotGated
        ));
        // Token budget exhausted.
        let mut g = live_grant("web_fetch");
        g.tokens_remaining = Some(0);
        assert!(matches!(
            autonomy_verdict(true, Some(&g), "web_fetch", &json!({}), now),
            AutonomyVerdict::NotGated
        ));
    }

    #[tokio::test]
    async fn granted_autonomous_action_dispatches_and_charges_one_fire() {
        // Headline A2 acceptance (granted-act): an autonomous turn whose firing
        // task carries a grant permitting `web_fetch` may take that action —
        // the autonomy block is cleared for it — and the fire is charged once.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        ctx.set_turn_provenance(true, false); // autonomous, no blanket approval
        install_grant(&deps, Some(live_grant("web_fetch")));

        let (out, _i, _e) = invoke_tool(&deps, &call("web_fetch")).await;
        // Not blocked by the autonomy gate (it fails downstream without a
        // network stub / url, but NOT with an autonomy or grant-scope deny).
        assert!(
            !out.contains("autonomous (heartbeat/scheduled) turn"),
            "a granted action must clear the autonomy block; got: {out}"
        );
        assert!(
            !out.contains("outside this task's approved grant"),
            "a granted in-scope action must not be scope-blocked; got: {out}"
        );
        assert_eq!(
            grant_consume_rows(&deps).await,
            1,
            "a granted autonomous action must charge exactly one fire"
        );

        // A SECOND granted action in the SAME turn charges no additional fire —
        // a fire is one turn, not one action.
        let _ = invoke_tool(&deps, &call("web_fetch")).await;
        assert_eq!(
            grant_consume_rows(&deps).await,
            1,
            "a second granted action in the same turn must not re-charge the fire"
        );
    }

    #[tokio::test]
    async fn ungranted_autonomous_action_is_blocked_and_can_still_propose() {
        // Headline A2 acceptance (ungranted-propose): an autonomous turn with no
        // grant is blocked from a credentialed external action, charges no fire,
        // and can still read memory + message to propose.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        ctx.set_turn_provenance(true, false);
        install_grant(&deps, None);

        let (blocked, _i, is_error) = invoke_tool(&deps, &call("install_packages")).await;
        assert!(is_error);
        assert!(
            blocked.contains("autonomous (heartbeat/scheduled) turn"),
            "an ungranted autonomous action must be blocked; got: {blocked}"
        );
        // Read-then-propose survives.
        let (mem, _i, _e) = invoke_tool(
            &deps,
            &call_with("memory_search", serde_json::json!({"query": "x"})),
        )
        .await;
        assert!(!mem.contains("autonomous"), "memory must stay reachable");
        assert_eq!(
            grant_consume_rows(&deps).await,
            0,
            "a blocked action must never charge a fire"
        );
    }

    #[tokio::test]
    async fn out_of_scope_grant_blocks_with_clear_reason_and_no_charge() {
        // A grant that covers `web_fetch` does NOT authorize `install_packages`:
        // the action stays blocked with a clear, classifiable reason and no fire
        // is charged.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        ctx.set_turn_provenance(true, false);
        install_grant(&deps, Some(live_grant("web_fetch")));

        let (blocked, _i, is_error) = invoke_tool(&deps, &call("install_packages")).await;
        assert!(is_error);
        assert!(
            blocked.contains("outside this task's approved grant"),
            "an out-of-scope action must name the grant; got: {blocked}"
        );
        assert!(
            blocked.contains("web_fetch"),
            "the deny must show the grant scope; got: {blocked}"
        );
        // The blocker classifier routes it to the Autonomous wall card.
        assert_eq!(
            crate::run::blocker::classify(&blocked),
            Some(crate::run::blocker::BlockerCategory::Autonomous)
        );
        assert_eq!(grant_consume_rows(&deps).await, 0);
    }

    #[tokio::test]
    async fn grant_opens_autonomy_but_not_the_taint_gate() {
        // A grant clears the AUTONOMY block for its scope, but the taint gate is
        // independent: a granted `web_fetch` on a web-tainted turn stays blocked
        // until fresh approval, and no fire is charged (block-not-charge).
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        ctx.set_turn_provenance(true, false);
        ctx.mark_untrusted_context("web_fetch:https://evil.example");
        install_grant(&deps, Some(live_grant("web_fetch")));

        let (blocked, _i, is_error) = invoke_tool(&deps, &call("web_fetch")).await;
        assert!(is_error);
        assert!(
            blocked.contains("untrusted-provenance"),
            "a grant opens autonomy, not taint; got: {blocked}"
        );
        assert_eq!(
            grant_consume_rows(&deps).await,
            0,
            "a taint-blocked action must not charge a fire"
        );
    }

    #[tokio::test]
    async fn expired_grant_leaves_autonomous_action_blocked() {
        // An inert (expired) snapshot authorizes nothing — the action falls back
        // to the policy layer's blanket autonomous deny.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        ctx.set_turn_provenance(true, false);
        let mut g = live_grant("web_fetch");
        g.expires_at = Some(Utc::now() - chrono::Duration::minutes(1));
        install_grant(&deps, Some(g));

        let (blocked, _i, is_error) = invoke_tool(&deps, &call("web_fetch")).await;
        assert!(is_error);
        assert!(
            blocked.contains("autonomous (heartbeat/scheduled) turn"),
            "an expired grant must not authorize; got: {blocked}"
        );
        assert_eq!(grant_consume_rows(&deps).await, 0);
    }

    #[tokio::test]
    async fn grant_does_not_affect_human_turns() {
        // Non-autonomous (human-driven) turns are unchanged: a credentialed
        // external action passes without consulting the grant and charges no
        // fire, even if a grant happens to be present.
        let (_tmp, deps, ctx) = deps_with_runner_ctx();
        ctx.set_turn_provenance(false, false); // human turn
        install_grant(&deps, Some(live_grant("web_fetch")));
        let (out, _i, _e) = invoke_tool(&deps, &call("install_packages")).await;
        assert!(
            !out.contains("autonomous") && !out.contains("approved grant"),
            "a human turn must not be autonomy-gated; got: {out}"
        );
        assert_eq!(
            grant_consume_rows(&deps).await,
            0,
            "a human turn must never charge a grant fire"
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
