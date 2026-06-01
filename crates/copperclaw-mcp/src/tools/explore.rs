//! `explore`: a lightweight in-process subagent.
//!
//! `explore` opens a fresh provider query against the same upstream the
//! parent runner uses, runs a bounded LLM tool-use loop with a
//! caller-supplied task as the user message, and returns a single
//! summary string. It exists so an agent can ask "look at these files
//! and tell me what's there" without spawning a whole new container
//! (the heavyweight `create_agent` path).
//!
//! Properties enforced here:
//! - **Bounded turns**: `max_turns` (default 5, hard cap 10).
//! - **Bounded tokens**: `max_tokens` input budget (default `50_000`,
//!   hard cap `200_000`). Output tokens count for accounting but are
//!   not budgeted — the model's per-turn `max_tokens` bounds them.
//! - **Bounded wall-clock**: 60s `tokio::time::timeout` around the
//!   whole loop. On expiry the partial summary is returned (the
//!   runner's `spawn_subagent` impl polls the deadline cooperatively;
//!   the timeout below is the *hard* fallback).
//! - **Read-only by default**: the `tools` allowlist defaults to
//!   `["grep", "glob", "read_file", "web_fetch"]`. Anything else has
//!   to be passed explicitly by the caller. `grep` and `glob` are not
//!   yet registered in-tree; the runner will refuse calls to them with
//!   "unknown tool" until they exist, which is the safe default.
//! - **No nested explore**: a subagent that emits another `explore`
//!   `tool_use` is refused at validation time with `ToolError::Validation`.
//!   The runner threads `nested = true` through the `ToolContext` so
//!   the inner `explore` handler can see it.
//!
//! Output JSON shape:
//!
//! ```json
//! {
//!   "summary": "...",
//!   "turns_used": 3,
//!   "tokens_used": 12453,
//!   "tools_called": [{"name": "read_file", "input": {...}}, ...]
//! }
//! ```

use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;

use crate::context::{
    SubagentRequest, SubagentResult, ToolContext, SUBAGENT_MAX_TOKENS_LIMIT,
    SUBAGENT_MAX_TURNS_LIMIT, SUBAGENT_WALL_CLOCK_SECS,
};
use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};

/// Default `max_turns` when the caller omits it.
pub const DEFAULT_MAX_TURNS: u32 = 5;
/// Default `max_tokens` when the caller omits it.
pub const DEFAULT_MAX_TOKENS: u32 = 50_000;
/// Default tool allowlist. Read-only set.
///
/// `grep` and `glob` are listed for forward-compatibility — the runner
/// will return "unknown tool" if they aren't registered, which is the
/// safe default. The subagent loop continues on unknown tools.
pub const DEFAULT_TOOLS: &[&str] = &["grep", "glob", "read_file", "web_fetch"];

#[derive(Debug, Deserialize)]
struct Input {
    task: String,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    /// Internal flag the runner sets when the caller is itself a
    /// subagent. Never set by the model — passing `nested: true` from
    /// outside is harmless (the handler still refuses) but the field
    /// is `#[serde(default)]` for forward-compat.
    #[serde(default)]
    nested: bool,
}

/// Build the rmcp `Tool` descriptor for `explore`.
pub fn schema() -> Tool {
    make_tool(
        "explore",
        "Open a lightweight in-process subagent that investigates using YOUR workspace. \
         It runs INSIDE your container, so it SEES YOUR FILES AND CODE (`/data`, the repo \
         you're working on). It runs a bounded LLM tool-use loop (default 5 turns, 50_000 \
         CUMULATIVE input-token budget across all turns, 60s wall-clock, read-only tools — \
         grep, glob, read_file, web_fetch) and returns a single summary string, shielding \
         the parent's context from the intermediate work. The `task` must be self-contained; \
         the subagent does not see the parent's conversation history.\n\n\
         **`explore` vs `create_agent` — both can read your code; pick by SIZE:**\n\
         - `explore` runs IN-PROCESS in YOUR container (sees your live `/data` directly) \
         and is bounded + cheap. Use it for QUICK, focused lookups: 'find where X is \
         handled', 'what does this module do', 'does the build pass'. Run a few in parallel \
         for a fast multi-angle skim. Returns a summary to you; raise `max_turns` (≤10) / \
         `max_tokens` (≤200k) for a bit more depth.\n\
         - `create_agent` spawns a full sibling agent in its OWN container. If your \
         workspace is a git repo it gets a WRITABLE worktree at `/workspace` (its own \
         branch — it can edit AND commit, and you review/merge the branch afterwards); \
         otherwise your code is READ-ONLY at `/parent`. It's UNBOUNDED (no turn/token cap) \
         but heavier. Use it for SUBSTANTIVE parallel work — a review/audit (one auditor \
         per area, each analysing `/parent` or `/workspace`) or parallel implementation \
         (one sibling per area, each committing on its branch) — then synthesise/merge.\n\n\
         **Budget caveat:** the 50k token budget is CUMULATIVE input tokens across all \
         subagent turns; each turn replays prior history + tool results, so one fetch of a \
         large page can dominate the budget. For broad multi-area review, prefer several \
         parallel `explore` calls (each with its own budget) over one `explore` doing \
         everything.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["task"],
            "properties": {
                "task":       { "type": "string", "minLength": 1 },
                "max_turns":  {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": SUBAGENT_MAX_TURNS_LIMIT
                },
                "max_tokens": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": SUBAGENT_MAX_TOKENS_LIMIT
                },
                "tools": {
                    "type": ["array", "null"],
                    "items": { "type": "string", "minLength": 1 }
                }
            }
        }),
    )
}

/// Dispatch a parsed `explore` call into the context, surfacing the
/// `SubagentResult` as a JSON tool result. Public for the tests in
/// this module; callers should use [`entry`] to register the tool.
pub async fn handle(
    arguments: Option<JsonObject>,
    ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let input: Input = parse_args(arguments)?;
    if input.task.trim().is_empty() {
        return Err(ToolError::Validation("`task` must be non-empty".into()));
    }
    if input.nested {
        // The runner's spawn_subagent threads nested=true; refuse so a
        // subagent can't recursively spawn subagents. (Mock contexts
        // never set this, so unit tests for the happy path keep working.)
        return Err(ToolError::Validation(
            "`explore` may not be called from inside another explore subagent".into(),
        ));
    }

    let mut max_turns = input.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
    if max_turns == 0 {
        max_turns = 1;
    }
    if max_turns > SUBAGENT_MAX_TURNS_LIMIT {
        max_turns = SUBAGENT_MAX_TURNS_LIMIT;
    }

    let mut max_tokens = input.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    if max_tokens == 0 {
        max_tokens = DEFAULT_MAX_TOKENS;
    }
    if max_tokens > SUBAGENT_MAX_TOKENS_LIMIT {
        max_tokens = SUBAGENT_MAX_TOKENS_LIMIT;
    }

    let tools_allowed: Vec<String> = match input.tools {
        Some(v) if !v.is_empty() => v,
        _ => DEFAULT_TOOLS.iter().map(|s| (*s).to_string()).collect(),
    };

    let req = SubagentRequest {
        task: input.task,
        max_turns,
        max_tokens,
        tools_allowed,
        nested: false,
    };

    // Hard wall-clock cap. The runner's `spawn_subagent` impl polls a
    // matching deadline cooperatively, but if it ever hangs in a
    // provider call this timeout is the fallback so the parent's tool
    // call returns within bound.
    let timeout = Duration::from_secs(SUBAGENT_WALL_CLOCK_SECS);
    let result: SubagentResult = match tokio::time::timeout(timeout, ctx.spawn_subagent(req)).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(err)) => return Err(err),
        Err(_elapsed) => SubagentResult {
            summary: "explore stopped: wall-clock timeout".to_string(),
            turns_used: 0,
            tokens_used: 0,
            tools_called: Vec::new(),
        },
    };
    Ok(success_json(&result))
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

/// Register `explore` with the in-process tool inventory.
pub fn entry() -> ToolEntry {
    ToolEntry {
        tool: schema(),
        handler: Box::new(Handler),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{MockToolContext, SubagentResult, SubagentToolCall};
    use crate::error::ToolError;
    use rmcp::model::JsonObject;
    use serde_json::Value;

    fn args_from(value: Value) -> Option<JsonObject> {
        match value {
            Value::Object(m) => Some(m),
            _ => None,
        }
    }

    #[tokio::test]
    async fn happy_path_returns_summary_from_context() {
        let ctx = MockToolContext::new();
        ctx.set_next_subagent_result(SubagentResult {
            summary: "found 3 callers".into(),
            turns_used: 2,
            tokens_used: 4321,
            tools_called: vec![SubagentToolCall {
                name: "read_file".into(),
                input: serde_json::json!({"path": "a.rs"}),
            }],
        });
        let out = handle(
            args_from(serde_json::json!({"task": "find callers of foo"})),
            &ctx,
        )
        .await
        .unwrap();
        // The CallToolResult's text block carries the JSON-pretty
        // SubagentResult.
        let body = match &out.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            other => panic!("unexpected content block: {other:?}"),
        };
        assert!(body.contains("found 3 callers"));
        assert!(body.contains("read_file"));
        // The request landed in the mock with default budgets.
        let calls = ctx.subagent_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].task, "find callers of foo");
        assert_eq!(calls[0].max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(calls[0].max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(
            calls[0].tools_allowed,
            DEFAULT_TOOLS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>()
        );
        assert!(!calls[0].nested);
    }

    #[tokio::test]
    async fn empty_task_validates() {
        let ctx = MockToolContext::new();
        let err = handle(args_from(serde_json::json!({"task": ""})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn nested_call_is_refused() {
        // Simulate the runner threading nested=true through. The
        // handler must refuse without touching the context.
        let ctx = MockToolContext::new();
        let err = handle(
            args_from(serde_json::json!({"task": "do thing", "nested": true})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(s) if s.contains("nested") || s.contains("inside")));
        assert!(ctx.subagent_calls().is_empty());
    }

    #[tokio::test]
    async fn caller_max_turns_is_clamped() {
        let ctx = MockToolContext::new();
        handle(
            args_from(serde_json::json!({
                "task": "x",
                "max_turns": 9999
            })),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.subagent_calls();
        assert_eq!(calls[0].max_turns, SUBAGENT_MAX_TURNS_LIMIT);
    }

    #[tokio::test]
    async fn caller_max_tokens_is_clamped() {
        let ctx = MockToolContext::new();
        handle(
            args_from(serde_json::json!({
                "task": "x",
                "max_tokens": 50_000_000_u64
            })),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.subagent_calls();
        assert_eq!(calls[0].max_tokens, SUBAGENT_MAX_TOKENS_LIMIT);
    }

    #[tokio::test]
    async fn explicit_tools_override_default() {
        let ctx = MockToolContext::new();
        handle(
            args_from(serde_json::json!({
                "task": "x",
                "tools": ["read_file", "web_search"]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.subagent_calls();
        assert_eq!(calls[0].tools_allowed, vec!["read_file", "web_search"]);
    }

    #[tokio::test]
    async fn context_error_propagates() {
        let ctx = MockToolContext::new();
        ctx.fail_next_subagent(ToolError::Context("provider down".into()));
        let err = handle(args_from(serde_json::json!({"task": "x"})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Context(_)));
    }

    #[test]
    fn schema_required_task_field() {
        let s = schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["task"]));
    }
}
