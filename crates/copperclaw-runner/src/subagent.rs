//! Bounded in-process LLM subagent loop.
//!
//! This is a slimmed-down sibling of [`crate::run::drive_turn`]. The
//! parent runner uses `drive_turn` per-inbound; the `explore` tool uses
//! [`run_inner_loop`] to give the model a fresh, bounded loop that
//! returns a single summary string. The two share semantics (LLM turn →
//! `tool_use` → `tool_result` → next turn) but diverge on:
//!
//! - **Persistence.** The subagent loop does not touch
//!   `outbound.db`, does not emit `send_message`, does not call
//!   `processing_ack`, and does not write a `usage_report` system row.
//!   Token accounting is returned to the caller so the runner can charge
//!   the parent's daily budget at its own emission point.
//! - **Scope.** The tool surface is filtered to a caller-supplied
//!   allowlist. Calls outside the allowlist surface as `is_error=true`
//!   `tool_result` blocks so the model sees the refusal text and can
//!   recover; the loop never panics on unknown tools.
//! - **Bounds.** Three hard caps:
//!   - `max_turns` LLM round-trips (cap applied by the caller, see
//!     `copperclaw_mcp::SUBAGENT_MAX_TURNS_LIMIT`).
//!   - `max_input_tokens` cumulative across all turns. The loop bails
//!     between turns once cumulative input crosses the budget.
//!   - `wall_clock` total. The caller wraps the whole call in a
//!     `tokio::time::timeout`; this loop polls the deadline
//!     cooperatively between turns to surface a partial summary
//!     instead of being terminated mid-await.
//!
//! Nested explore is the caller's responsibility — the runner sets
//! `nested = true` on the `SubagentRequest` before re-entering the
//! `explore` tool, and that tool returns `ToolError::Validation`. We
//! do not double-check here.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use copperclaw_mcp::{SubagentResult, SubagentToolCall, ToolContext, ToolEntry};
use copperclaw_providers::{AgentProvider, HistoryMessage, ProviderError, QueryInput, ToolDef};
use copperclaw_types::{Effort, ProviderEvent};

/// Dependencies for one subagent invocation. Borrowed from
/// [`crate::run::RunnerDeps`]; we keep references where possible so
/// the runner can pass them straight through without cloning the
/// provider or the tool map.
pub struct SubagentDeps<'a> {
    pub provider: &'a Arc<dyn AgentProvider>,
    pub tool_ctx: &'a Arc<dyn ToolContext>,
    pub tool_map: &'a Arc<HashMap<String, Arc<ToolEntry>>>,
    /// Base system prompt — typically the parent runner's, prepended
    /// to a short "you are a focused subagent" preamble.
    pub system: &'a str,
    pub model: &'a str,
    pub effort: Effort,
    /// Per-turn `max_tokens` passed to the provider. Distinct from the
    /// subagent's cumulative budget.
    pub per_turn_max_tokens: u32,
    pub temperature: Option<f32>,
    pub assistant_name: Option<&'a str>,
    pub provider_deadline: Duration,
}

/// Inputs to [`run_inner_loop`].
pub struct SubagentInputs {
    pub task: String,
    pub tools_allowed: Vec<String>,
    pub max_turns: u32,
    pub max_input_tokens: u32,
    pub wall_clock: Duration,
}

/// The "you're a focused subagent" preamble bolted onto the parent
/// system prompt. Kept short on purpose — every byte here is paid by
/// every subagent call.
pub const SUBAGENT_PREAMBLE: &str = "You are a focused research subagent. Read the user task carefully, use the \
     read-only tools you have to gather what you need, then produce a SINGLE \
     concise final summary as your last assistant turn. Do NOT chat, do NOT \
     ask questions, do NOT speculate beyond what your tool calls confirmed. \
     The parent agent gets your final text and nothing else.";

/// Build the system prompt the subagent sees: the parent's prompt plus
/// the preamble. Public so it can be reused / unit-tested.
#[must_use]
pub fn build_subagent_system(parent: &str) -> String {
    if parent.trim().is_empty() {
        SUBAGENT_PREAMBLE.to_string()
    } else {
        format!("{parent}\n\n{SUBAGENT_PREAMBLE}")
    }
}

/// Run a bounded LLM loop and return the assistant's last text plus
/// accounting. Errors propagate; the caller maps them to
/// `ToolError::Context`.
///
/// Behaviour summary (in the same order the loop applies them):
///
/// 1. **Wall-clock check** — if the elapsed time exceeds
///    `wall_clock`, return with summary `"explore stopped: wall-clock
///    timeout"`.
/// 2. **Token budget check** — if cumulative input tokens have
///    already exceeded `max_input_tokens`, return with summary
///    `"explore stopped: token budget exceeded"`.
/// 3. **LLM turn** — issue one `provider.query()` call with the
///    current history.
/// 4. **No tool calls** — return with the model's text as summary.
/// 5. **Tool calls** — execute each one (filtered by allowlist),
///    append `tool_result` blocks to history, loop.
/// 6. **`max_turns` reached** — return with whatever the last
///    assistant text was. Empty if the model produced only `tool_use`.
#[allow(clippy::too_many_lines)] // single loop with mid-flight bookkeeping; splitting hurts clarity.
pub async fn run_inner_loop(
    deps: &SubagentDeps<'_>,
    inputs: SubagentInputs,
) -> Result<SubagentResult, ProviderError> {
    let system = build_subagent_system(deps.system);
    let allow: std::collections::HashSet<String> = inputs.tools_allowed.iter().cloned().collect();

    // Filter the parent's tool inventory down to the allowlist. Any
    // tool the parent doesn't have (e.g. "grep" / "glob" until they
    // ship) is silently dropped from the provider-visible list so the
    // model isn't told about tools it can't actually use.
    let tools: Vec<ToolDef> = deps
        .tool_map
        .iter()
        .filter(|(name, _)| allow.contains(name.as_str()))
        .map(|(_, entry)| ToolDef {
            name: entry.tool.name.to_string(),
            description: entry.tool.description.as_deref().unwrap_or("").to_string(),
            input_schema: serde_json::Value::Object((*entry.tool.input_schema).clone()),
        })
        .collect();

    let mut history: Vec<HistoryMessage> = vec![HistoryMessage::User {
        content: inputs.task.clone(),
    }];
    let mut tools_called: Vec<SubagentToolCall> = Vec::new();
    let mut input_tokens_sum: u32 = 0;
    let mut output_tokens_sum: u32 = 0;
    let mut last_assistant_text = String::new();
    let mut turns_used: u32 = 0;

    let started_at = Instant::now();
    let max_turns = inputs.max_turns.max(1);

    for _turn in 0..max_turns {
        // Wall-clock gate. Polled here so the model's *most-recent*
        // text is what we return on timeout, instead of being killed
        // mid-await by the outer `tokio::time::timeout` and dropping
        // the partial summary on the floor.
        if started_at.elapsed() >= inputs.wall_clock {
            return Ok(SubagentResult {
                summary: if last_assistant_text.is_empty() {
                    "explore stopped: wall-clock timeout".to_string()
                } else {
                    last_assistant_text
                },
                turns_used,
                tokens_used: input_tokens_sum.saturating_add(output_tokens_sum),
                tools_called,
            });
        }

        // Token-budget gate (between turns). We bail BEFORE the next
        // LLM call so we don't spend any more of the parent's budget.
        if input_tokens_sum > inputs.max_input_tokens {
            return Ok(SubagentResult {
                summary: "explore stopped: token budget exceeded".to_string(),
                turns_used,
                tokens_used: input_tokens_sum.saturating_add(output_tokens_sum),
                tools_called,
            });
        }

        // One LLM turn. We slim down `run::run_llm_turn` here: no
        // retry-on-stream-error, no usage_report, no per-attempt
        // deadline-then-DeadlineExceeded. The outer wall-clock + the
        // provider's own retries are enough for the read-only
        // workload the subagent does.
        let input = QueryInput {
            system: system.clone(),
            // Subagents prepend their full prompt into `system`; there is
            // no per-inbound conversation-context paragraph here.
            system_context: None,
            model: deps.model.to_string(),
            effort: deps.effort,
            previous_continuation: None,
            history: history.clone(),
            tools: tools.clone(),
            max_tokens: deps.per_turn_max_tokens,
            temperature: deps.temperature,
            assistant_name: deps.assistant_name.map(str::to_string),
            display_name: None,
        };

        let mut query =
            match tokio::time::timeout(deps.provider_deadline, deps.provider.query(input)).await {
                Ok(Ok(q)) => q,
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => {
                    return Err(ProviderError::DeadlineExceeded {
                        deadline_ms: u64::try_from(deps.provider_deadline.as_millis())
                            .unwrap_or(u64::MAX),
                        attempts: 1,
                    });
                }
            };

        let mut text = String::new();
        let mut pending_calls: Vec<PendingCall> = Vec::new();
        let mut failed = false;

        while let Some(event) = query.next_event().await {
            match event {
                // Subagents intentionally drop `Thinking` events on the
                // floor — they're bounded single-shot loops with no user
                // channel to surface reasoning to. The main runner handles
                // the structured-thinking emit path (gated on per-group
                // `surface_thinking`); see `run::provider_call::pump_events`.
                ProviderEvent::Init { .. }
                | ProviderEvent::ToolStart { .. }
                | ProviderEvent::ToolEnd
                | ProviderEvent::Progress { .. }
                | ProviderEvent::Activity
                | ProviderEvent::Thinking { .. } => {}
                ProviderEvent::Usage {
                    input_tokens,
                    output_tokens,
                    ..
                } => {
                    if input_tokens > 0 {
                        input_tokens_sum = input_tokens_sum.saturating_add(input_tokens);
                    }
                    if output_tokens > 0 {
                        output_tokens_sum = output_tokens_sum.saturating_add(output_tokens);
                    }
                }
                ProviderEvent::Result { text: t } => {
                    if let Some(t) = t {
                        text = t;
                    }
                    break;
                }
                ProviderEvent::Error { message, .. } => {
                    tracing::warn!(
                        error = %message,
                        "subagent: provider returned error event; bailing turn"
                    );
                    failed = true;
                    break;
                }
                ProviderEvent::ToolCall { id, name, input } => {
                    pending_calls.push(PendingCall { id, name, input });
                }
                ProviderEvent::ToolInputParseError {
                    tool_use_id,
                    tool_name,
                    parse_error,
                    ..
                } => {
                    // Subagent doesn't get the parse-error feedback
                    // loop the main runner has — keep behaviour
                    // conservative and abort the explore so the
                    // parent agent can see the failure summary. The
                    // top-level runner is where self-correction
                    // happens; subagent turns are bounded and
                    // single-shot.
                    tracing::warn!(
                        tool_use_id = %tool_use_id,
                        tool_name = %tool_name,
                        parse_error = %parse_error,
                        "subagent: tool_use input JSON parse failure; bailing turn"
                    );
                    failed = true;
                    break;
                }
            }
        }
        query.abort().await;

        turns_used = turns_used.saturating_add(1);

        if failed {
            return Ok(SubagentResult {
                summary: if last_assistant_text.is_empty() {
                    "explore stopped: provider error".to_string()
                } else {
                    last_assistant_text
                },
                turns_used,
                tokens_used: input_tokens_sum.saturating_add(output_tokens_sum),
                tools_called,
            });
        }

        if !text.is_empty() {
            last_assistant_text.clone_from(&text);
            history.push(HistoryMessage::Assistant { content: text });
        }
        for call in &pending_calls {
            history.push(HistoryMessage::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }

        if pending_calls.is_empty() {
            // Model produced a final text. We're done.
            return Ok(SubagentResult {
                summary: last_assistant_text,
                turns_used,
                tokens_used: input_tokens_sum.saturating_add(output_tokens_sum),
                tools_called,
            });
        }

        // Execute each tool call. Record it in `tools_called` either
        // way (allowed or refused) so the parent sees what was
        // attempted.
        for call in &pending_calls {
            tools_called.push(SubagentToolCall {
                name: call.name.clone(),
                input: call.input.clone(),
            });
            let (content, is_error) = invoke_subagent_tool(deps, &allow, call).await;
            history.push(HistoryMessage::Tool {
                tool_use_id: call.id.clone(),
                content,
                is_error,
            });
        }
    }

    // Hit the turn cap without a final-text turn.
    Ok(SubagentResult {
        summary: if last_assistant_text.is_empty() {
            "explore stopped: max_turns reached".to_string()
        } else {
            last_assistant_text
        },
        turns_used,
        tokens_used: input_tokens_sum.saturating_add(output_tokens_sum),
        tools_called,
    })
}

#[derive(Debug, Clone)]
struct PendingCall {
    id: String,
    name: String,
    input: serde_json::Value,
}

/// Look up and invoke one tool call, honouring the subagent's
/// allowlist. Refusals (allowlist miss, unknown tool, bad input shape)
/// come back as `is_error=true` text blocks so the model sees the
/// refusal and can adjust.
async fn invoke_subagent_tool(
    deps: &SubagentDeps<'_>,
    allow: &std::collections::HashSet<String>,
    call: &PendingCall,
) -> (String, bool) {
    if !allow.contains(&call.name) {
        return (
            format!(
                "Tool `{}` is not on this subagent's allowlist. Allowed: {:?}",
                call.name,
                allow.iter().cloned().collect::<Vec<_>>()
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
    let arguments = match &call.input {
        serde_json::Value::Object(map) => Some(map.clone()),
        serde_json::Value::Null => None,
        other => {
            return (
                format!(
                    "Tool `{}` input must be a JSON object, got {}",
                    call.name,
                    short_type(other)
                ),
                true,
            );
        }
    };
    match entry.handler.call(arguments, deps.tool_ctx.as_ref()).await {
        Ok(result) => (render_tool_result(&result), false),
        Err(err) => (format!("Tool `{}` failed: {err}", call.name), true),
    }
}

fn render_tool_result(result: &rmcp::model::CallToolResult) -> String {
    let mut out = String::new();
    for block in &result.content {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        match &block.raw {
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

fn short_type(v: &serde_json::Value) -> &'static str {
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
    use super::*;
    use async_trait::async_trait;
    use copperclaw_mcp::{SubagentRequest, tools::ToolHandler};
    use copperclaw_providers::{AgentProvider, AgentQuery};
    use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
    use std::borrow::Cow;
    use std::sync::Mutex as StdMutex;

    /// Provider that yields a pre-baked sequence of events per turn.
    struct ScriptedProvider {
        scripts: StdMutex<Vec<Vec<ProviderEvent>>>,
        observed_queries: std::sync::atomic::AtomicU32,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: StdMutex::new(scripts),
                observed_queries: std::sync::atomic::AtomicU32::new(0),
            })
        }
    }

    #[async_trait]
    impl AgentProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }
        async fn query(&self, _input: QueryInput) -> Result<Box<dyn AgentQuery>, ProviderError> {
            self.observed_queries
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let events = {
                let mut g = self.scripts.lock().unwrap();
                if g.is_empty() {
                    vec![ProviderEvent::Result { text: None }]
                } else {
                    g.remove(0)
                }
            };
            Ok(Box::new(ScriptedQuery {
                events: StdMutex::new(events),
            }))
        }
        fn is_session_invalid(&self, _err: &ProviderError) -> bool {
            false
        }
    }

    struct ScriptedQuery {
        events: StdMutex<Vec<ProviderEvent>>,
    }

    #[async_trait]
    impl AgentQuery for ScriptedQuery {
        async fn push(&mut self, _: String) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), ProviderError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<ProviderEvent> {
            let mut g = self.events.lock().unwrap();
            if g.is_empty() {
                None
            } else {
                Some(g.remove(0))
            }
        }
        async fn abort(&mut self) {}
    }

    /// A read-only "look up" tool used to exercise the `tool_use` path
    /// without depending on filesystem state.
    struct EchoHandler;
    #[async_trait::async_trait]
    impl ToolHandler for EchoHandler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            _ctx: &dyn ToolContext,
        ) -> Result<CallToolResult, copperclaw_mcp::ToolError> {
            let body =
                serde_json::to_string(&arguments).unwrap_or_else(|_| "<unserialisable>".into());
            Ok(CallToolResult::success(vec![Content::text(format!(
                "echo: {body}"
            ))]))
        }
    }

    fn mk_tool(name: &'static str) -> Tool {
        let schema_obj = match serde_json::json!({"type": "object"}) {
            serde_json::Value::Object(m) => m,
            _ => rmcp::model::JsonObject::default(),
        };
        Tool {
            name: Cow::Borrowed(name),
            description: Some(Cow::Borrowed("test tool")),
            input_schema: Arc::new(schema_obj),
            annotations: None,
        }
    }

    fn echo_entry() -> ToolEntry {
        ToolEntry {
            tool: mk_tool("grep"),
            handler: Box::new(EchoHandler),
        }
    }

    fn build_deps<'a>(
        provider: &'a Arc<dyn AgentProvider>,
        tool_ctx: &'a Arc<dyn ToolContext>,
        tool_map: &'a Arc<HashMap<String, Arc<ToolEntry>>>,
    ) -> SubagentDeps<'a> {
        SubagentDeps {
            provider,
            tool_ctx,
            tool_map,
            system: "you are helpful",
            model: "claude-test",
            effort: Effort::Low,
            per_turn_max_tokens: 4096,
            temperature: None,
            assistant_name: None,
            provider_deadline: Duration::from_secs(5),
        }
    }

    fn mk_tool_map(entries: Vec<ToolEntry>) -> Arc<HashMap<String, Arc<ToolEntry>>> {
        let mut m: HashMap<String, Arc<ToolEntry>> = HashMap::new();
        for e in entries {
            m.insert(e.tool.name.to_string(), Arc::new(e));
        }
        Arc::new(m)
    }

    #[tokio::test]
    async fn happy_path_single_result() {
        let provider: Arc<dyn AgentProvider> = ScriptedProvider::new(vec![vec![
            ProviderEvent::Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            ProviderEvent::Result {
                text: Some("the answer is 42".into()),
            },
        ]]);
        let ctx: Arc<dyn ToolContext> = Arc::new(copperclaw_mcp::MockToolContext::new());
        let map = mk_tool_map(vec![echo_entry()]);
        let deps = build_deps(&provider, &ctx, &map);
        let out = run_inner_loop(
            &deps,
            SubagentInputs {
                task: "find the answer".into(),
                tools_allowed: vec!["grep".into()],
                max_turns: 5,
                max_input_tokens: 50_000,
                wall_clock: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
        assert_eq!(out.summary, "the answer is 42");
        assert_eq!(out.turns_used, 1);
        assert_eq!(out.tokens_used, 120);
        assert!(out.tools_called.is_empty());
    }

    #[tokio::test]
    async fn tool_use_then_final_text() {
        let provider: Arc<dyn AgentProvider> = ScriptedProvider::new(vec![
            // Turn 1: model asks to grep, no text.
            vec![ProviderEvent::ToolCall {
                id: "tu_1".into(),
                name: "grep".into(),
                input: serde_json::json!({"q": "foo"}),
            }],
            // Turn 2: model produces the summary.
            vec![ProviderEvent::Result {
                text: Some("found foo in two files".into()),
            }],
        ]);
        let ctx: Arc<dyn ToolContext> = Arc::new(copperclaw_mcp::MockToolContext::new());
        let map = mk_tool_map(vec![echo_entry()]);
        let deps = build_deps(&provider, &ctx, &map);
        let out = run_inner_loop(
            &deps,
            SubagentInputs {
                task: "find foo".into(),
                tools_allowed: vec!["grep".into()],
                max_turns: 5,
                max_input_tokens: 50_000,
                wall_clock: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
        assert_eq!(out.summary, "found foo in two files");
        assert_eq!(out.turns_used, 2);
        assert_eq!(out.tools_called.len(), 1);
        assert_eq!(out.tools_called[0].name, "grep");
    }

    #[tokio::test]
    async fn max_turns_cap_bails_with_last_text() {
        // Model keeps requesting tool calls forever — 3 scripted
        // turns, all tool_use, no Result. We cap at 2 turns. The
        // loop should bail at the cap with the canonical message
        // because the model never produced any assistant text.
        let provider: Arc<dyn AgentProvider> = ScriptedProvider::new(vec![
            vec![ProviderEvent::ToolCall {
                id: "tu_1".into(),
                name: "grep".into(),
                input: serde_json::json!({}),
            }],
            vec![ProviderEvent::ToolCall {
                id: "tu_2".into(),
                name: "grep".into(),
                input: serde_json::json!({}),
            }],
            vec![ProviderEvent::ToolCall {
                id: "tu_3".into(),
                name: "grep".into(),
                input: serde_json::json!({}),
            }],
        ]);
        let ctx: Arc<dyn ToolContext> = Arc::new(copperclaw_mcp::MockToolContext::new());
        let map = mk_tool_map(vec![echo_entry()]);
        let deps = build_deps(&provider, &ctx, &map);
        let out = run_inner_loop(
            &deps,
            SubagentInputs {
                task: "loop forever please".into(),
                tools_allowed: vec!["grep".into()],
                max_turns: 2,
                max_input_tokens: 50_000,
                wall_clock: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
        // Loop bailed at the cap. No assistant text was ever
        // produced, so summary is the canonical "max_turns reached"
        // banner. Both LLM turns fired.
        assert_eq!(out.turns_used, 2);
        assert_eq!(out.summary, "explore stopped: max_turns reached");
        // Both tool calls were attempted.
        assert_eq!(out.tools_called.len(), 2);
    }

    #[tokio::test]
    async fn token_budget_cap_emits_canonical_message() {
        // Turn 1 reports 10_000 input tokens, no Result (just tool_use)
        // so the loop runs another turn — which finds the cumulative
        // tokens are over budget and bails with the canonical message.
        let provider: Arc<dyn AgentProvider> = ScriptedProvider::new(vec![
            vec![
                ProviderEvent::Usage {
                    input_tokens: 10_000,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                },
                ProviderEvent::ToolCall {
                    id: "tu_1".into(),
                    name: "grep".into(),
                    input: serde_json::json!({}),
                },
            ],
            vec![ProviderEvent::Result {
                text: Some("late summary that should not appear".into()),
            }],
        ]);
        let ctx: Arc<dyn ToolContext> = Arc::new(copperclaw_mcp::MockToolContext::new());
        let map = mk_tool_map(vec![echo_entry()]);
        let deps = build_deps(&provider, &ctx, &map);
        let out = run_inner_loop(
            &deps,
            SubagentInputs {
                task: "huge task".into(),
                tools_allowed: vec!["grep".into()],
                max_turns: 5,
                max_input_tokens: 5_000,
                wall_clock: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
        assert_eq!(out.summary, "explore stopped: token budget exceeded");
        // We ran exactly one LLM turn before bailing.
        assert_eq!(out.turns_used, 1);
        assert!(out.tokens_used >= 10_000);
    }

    #[tokio::test]
    async fn disallowed_tool_returns_refusal_history_continues() {
        // Turn 1: model calls `shell` (not in allowlist). Turn 2:
        // model concedes with text.
        let provider: Arc<dyn AgentProvider> = ScriptedProvider::new(vec![
            vec![ProviderEvent::ToolCall {
                id: "tu_1".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            vec![ProviderEvent::Result {
                text: Some("ok, used read_file instead".into()),
            }],
        ]);
        let ctx: Arc<dyn ToolContext> = Arc::new(copperclaw_mcp::MockToolContext::new());
        // Build a tool map with `shell` registered (so it would
        // succeed if allowed) AND `grep`.
        let mut entries = vec![echo_entry()];
        entries.push(ToolEntry {
            tool: mk_tool("shell"),
            handler: Box::new(EchoHandler),
        });
        let map = mk_tool_map(entries);
        let deps = build_deps(&provider, &ctx, &map);
        let out = run_inner_loop(
            &deps,
            SubagentInputs {
                task: "find foo".into(),
                tools_allowed: vec!["grep".into()],
                max_turns: 5,
                max_input_tokens: 50_000,
                wall_clock: Duration::from_secs(60),
            },
        )
        .await
        .unwrap();
        assert_eq!(out.summary, "ok, used read_file instead");
        // The attempted shell call is recorded.
        assert_eq!(out.tools_called.len(), 1);
        assert_eq!(out.tools_called[0].name, "shell");
    }

    #[tokio::test]
    async fn build_subagent_system_prepends_parent() {
        let s = build_subagent_system("PARENT");
        assert!(s.starts_with("PARENT"));
        assert!(s.contains(SUBAGENT_PREAMBLE));
        // Empty parent yields just the preamble.
        let s2 = build_subagent_system("");
        assert_eq!(s2, SUBAGENT_PREAMBLE);
    }

    #[tokio::test]
    async fn subagent_request_roundtrip_serde() {
        // Cheap pin: the SubagentRequest the runner threads through
        // serialises / deserialises so it survives mcp boundary.
        let req = SubagentRequest {
            task: "look".into(),
            max_turns: 3,
            max_tokens: 10_000,
            tools_allowed: vec!["read_file".into()],
            nested: false,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: SubagentRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(req, back);
    }
}
