//! Agent-management tools: `create_agent`, `delegate`.

pub mod create_agent {
    //! `create_agent`: ask the host to spawn a fresh sibling agent.

    use crate::context::{CreateAgentSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        name: String,
        instructions: String,
        #[serde(default)]
        channel: Option<String>,
    }

    pub fn schema() -> Tool {
        make_tool(
            "create_agent",
            "Request the host to spawn a sibling agent (own container, full tool access, \
             unbounded — it reports back into your messages_in). The sibling has its OWN \
             fresh workspace at /data. If the project you're CURRENTLY in (your shell's \
             working dir) is a GIT REPO, the sibling also gets a WRITABLE git worktree of \
             THAT repo at /workspace on its own branch (sib/<id>): it can edit AND commit \
             there, isolated from your files. Commits go into the shared object store, so \
             after it finishes you review and merge its branch from inside that project \
             (`git diff main..sib/<id>`, `git merge sib/<id>`); your checked-out files are \
             never touched until you merge. So `cd` into the project (and `git init` it if \
             new) before spawning builders. If you're NOT in a git repo, your workspace is \
             instead mounted READ-ONLY at /parent (review/audit only). ALWAYS point the \
             sibling at its workspace in `instructions` — e.g. 'implement X under \
             /workspace and commit' (git repo) or 'review the code under /parent' \
             (read-only). Use `create_agent` for SUBSTANTIVE PARALLEL work over your \
             codebase or independent research; for a QUICK in-process lookup that shares \
             your live workspace directly, prefer `explore`.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "instructions"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "instructions": { "type": "string", "minLength": 1 },
                    "channel": { "type": ["string", "null"] }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.name.trim().is_empty() {
            return Err(ToolError::Validation("`name` must be non-empty".into()));
        }
        if input.instructions.trim().is_empty() {
            return Err(ToolError::Validation(
                "`instructions` must be non-empty".into(),
            ));
        }
        if let Some(c) = input.channel.as_ref() {
            if c.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`channel`, when present, must be non-empty".into(),
                ));
            }
        }
        let spec = CreateAgentSpec {
            name: input.name,
            instructions: input.instructions,
            channel: input.channel,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::CreateAgent(spec))
            .await?;
        Ok(ack_to_result(&ack))
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
    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod delegate {
    //! `delegate`: spawn a write-capable, parent-only build worker.
    //!
    //! `delegate` is the MIDDLE TIER between the read-only in-process
    //! `explore` subagent and a full persistent `create_agent` sibling.
    //! It reuses the exact `create_agent` container-spawn + git-worktree
    //! mechanics (own session, writable `/workspace` on branch
    //! `sib/<id>`), so a delegate can edit AND commit in isolation and
    //! its writes land in the parent repo via the branch-merge path. It
    //! differs from `create_agent` in being CONTAINED: a delegate is
    //! never wired into a channel and cannot post into the user's chat —
    //! it reports ONLY back to the spawning parent. Fan a few out (call
    //! `delegate` several times) and each gets its own isolated worktree.

    use crate::context::{DelegateSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        name: String,
        instructions: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "delegate",
            "Spawn a write-capable BUILD WORKER — the middle tier between `explore` \
             (read-only, in-process) and `create_agent` (a full, user-facing sibling). \
             Like `create_agent`, a delegate runs in its OWN container and, if the project \
             you're CURRENTLY in (your shell's working dir) is a GIT REPO, gets a WRITABLE \
             git worktree of THAT repo at /workspace on its own branch (sib/<id>): it can \
             edit AND commit there, isolated from your files and from any sibling delegates \
             (each delegate gets its OWN worktree). After it finishes you review and merge \
             its branch (`git diff main..sib/<id>`, `git merge sib/<id>`); your checked-out \
             files are never touched until you merge. UNLIKE `create_agent`, a delegate is \
             CONTAINED: it is NOT wired to any channel and CANNOT message the user's chat — \
             it reports ONLY back to you. Use it for SUBSTANTIVE PARALLEL BUILD work: `cd` \
             into the project (`git init` it if new), then call `delegate` once per \
             independent piece of work — always point each one at /workspace in \
             `instructions` (e.g. 'implement X under /workspace and commit'). For a QUICK \
             read-only lookup that shares your live workspace, prefer `explore`; for a \
             persistent user-facing sibling, use `create_agent`.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "instructions"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "instructions": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.name.trim().is_empty() {
            return Err(ToolError::Validation("`name` must be non-empty".into()));
        }
        if input.instructions.trim().is_empty() {
            return Err(ToolError::Validation(
                "`instructions` must be non-empty".into(),
            ));
        }
        let spec = DelegateSpec {
            name: input.name,
            instructions: input.instructions,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::Delegate(spec))
            .await?;
        Ok(ack_to_result(&ack))
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
    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod delegate_batch {
    //! `delegate_batch`: spawn N `delegate` workers in parallel and BLOCK
    //! until all report, returning one aggregated result.
    //!
    //! This is the single-call parallel fan-out with a JOIN (M19 A1). Where
    //! plain `delegate` is fire-and-forget (each worker reports back
    //! asynchronously on a later turn), `delegate_batch` spawns a bounded
    //! set of workers — each in its OWN container with its OWN writable
    //! `sib/<id>` worktree, each contained (reports only to you, never to
    //! the user's chat, exactly like a single `delegate`) — and gathers
    //! their reports into ONE tool response before returning. A parent that
    //! wants to build three components in parallel and assemble them can now
    //! block on all three in a single turn.
    //!
    //! The spawn machinery is identical to `delegate` (same depth /
    //! permission caps, same isolation); the only new thing is the join.
    //! The runner's `ToolContext::run_delegate_batch` implements the join
    //! seam by block-polling the parent's inbound for each worker's spawn
    //! result and final report.

    use crate::context::{
        DEFAULT_DELEGATE_BATCH_TIMEOUT_SECS, DelegateBatchRequest, DelegateBatchWorker,
        MAX_DELEGATE_BATCH_TIMEOUT_SECS, MAX_DELEGATE_BATCH_WIDTH, ToolContext,
    };
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, parse_args, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        workers: Vec<InputWorker>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    }

    #[derive(Debug, Deserialize)]
    struct InputWorker {
        name: String,
        instructions: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "delegate_batch",
            "Spawn SEVERAL write-capable build workers IN PARALLEL and BLOCK until they ALL \
             finish, then return their reports as ONE aggregated result. This is the fan-out \
             + JOIN primitive: use it when you want to build/investigate multiple INDEPENDENT \
             pieces at once and then ASSEMBLE the results in the same turn (e.g. 'build the API, \
             the CLI, and the docs in parallel, then wire them together'). Each worker is exactly \
             a `delegate`: its OWN container, its OWN writable git worktree at /workspace on its \
             OWN branch (sib/<id>) if you're in a git repo, CONTAINED (it reports only back to \
             you, never to the user's chat). Point EACH worker at /workspace in its \
             `instructions` ('implement X under /workspace and commit'). After the batch returns \
             you review + merge each worker's branch (`git diff main..sib/<id>`, `git merge \
             sib/<id>`). Fan-out is capped at 6 workers per call — split larger spreads into \
             multiple batches, or use plain `delegate` (called N times) for looser, unbounded \
             fan-out whose results arrive asynchronously instead of joined. The call blocks up \
             to `timeout_secs` (default 300, max 600); a worker that fails to spawn or does not \
             report in time surfaces as a per-worker error in the aggregate — it never loses the \
             whole batch. For a QUICK read-only lookup that shares your live workspace, prefer \
             `explore`.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["workers"],
                "properties": {
                    "workers": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_DELEGATE_BATCH_WIDTH,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["name", "instructions"],
                            "properties": {
                                "name": { "type": "string", "minLength": 1 },
                                "instructions": { "type": "string", "minLength": 1 }
                            }
                        }
                    },
                    "timeout_secs": {
                        "type": ["integer", "null"],
                        "minimum": 1,
                        "maximum": MAX_DELEGATE_BATCH_TIMEOUT_SECS
                    }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.workers.is_empty() {
            return Err(ToolError::Validation(
                "`workers` must contain at least one worker".into(),
            ));
        }
        if input.workers.len() > MAX_DELEGATE_BATCH_WIDTH {
            return Err(ToolError::Validation(format!(
                "`delegate_batch` fans out at most {MAX_DELEGATE_BATCH_WIDTH} workers (got {}); \
                 split into multiple batches, or use `delegate` for looser async fan-out",
                input.workers.len()
            )));
        }
        let mut workers = Vec::with_capacity(input.workers.len());
        for w in input.workers {
            if w.name.trim().is_empty() {
                return Err(ToolError::Validation(
                    "each worker `name` must be non-empty".into(),
                ));
            }
            if w.instructions.trim().is_empty() {
                return Err(ToolError::Validation(
                    "each worker `instructions` must be non-empty".into(),
                ));
            }
            workers.push(DelegateBatchWorker {
                name: w.name,
                instructions: w.instructions,
            });
        }
        let timeout_secs = input
            .timeout_secs
            .unwrap_or(DEFAULT_DELEGATE_BATCH_TIMEOUT_SECS)
            .clamp(1, MAX_DELEGATE_BATCH_TIMEOUT_SECS);

        let req = DelegateBatchRequest {
            workers,
            timeout_secs,
        };
        let outcome = ctx.run_delegate_batch(req).await?;

        // A batch where EVERY worker failed to spawn (e.g. refused by the
        // subagent depth cap) is a refusal, not a partial result — surface
        // it as a tool error so the model sees a clear "refused", not an
        // aggregate of failures it might try to consume.
        if outcome.all_spawn_failed() {
            let reason = outcome
                .workers
                .first()
                .and_then(|w| w.error.clone())
                .unwrap_or_else(|| "all workers refused".into());
            return Err(ToolError::Context(format!(
                "delegate_batch refused: {reason}"
            )));
        }
        Ok(success_json(&outcome))
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
    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::context::{DelegateSpec, MockToolContext, OutboundToolEffect};
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
    async fn create_agent_happy() {
        let ctx = MockToolContext::new();
        super::create_agent::handle(
            args_from(
                serde_json::json!({"name": "Greeter", "instructions": "Say hi.", "channel": "telegram:chat-1"}),
            ),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::CreateAgent(s) => {
                assert_eq!(s.name, "Greeter");
                assert_eq!(s.channel.as_deref(), Some("telegram:chat-1"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_agent_empty_name() {
        let ctx = MockToolContext::new();
        let err = super::create_agent::handle(
            args_from(serde_json::json!({"name": " ", "instructions": "i"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn create_agent_empty_instructions() {
        let ctx = MockToolContext::new();
        let err = super::create_agent::handle(
            args_from(serde_json::json!({"name": "n", "instructions": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn create_agent_empty_channel() {
        let ctx = MockToolContext::new();
        let err = super::create_agent::handle(
            args_from(serde_json::json!({"name": "n", "instructions": "i", "channel": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn schema_required_fields() {
        let s = super::create_agent::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["name", "instructions"]));
    }

    #[tokio::test]
    async fn delegate_happy_emits_effect() {
        let ctx = MockToolContext::new();
        super::delegate::handle(
            args_from(
                serde_json::json!({"name": "builder", "instructions": "build X under /workspace"}),
            ),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::Delegate(DelegateSpec { name, instructions }) => {
                assert_eq!(name, "builder");
                assert_eq!(instructions, "build X under /workspace");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn delegate_empty_name_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate::handle(
            args_from(serde_json::json!({"name": " ", "instructions": "i"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn delegate_empty_instructions_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate::handle(
            args_from(serde_json::json!({"name": "n", "instructions": ""})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn delegate_schema_has_no_channel_field() {
        // A delegate is never user-facing, so it takes no `channel`.
        let s = super::delegate::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["name", "instructions"]));
        assert!(
            v["properties"].get("channel").is_none(),
            "delegate must not expose a channel binding"
        );
    }

    // ── delegate_batch (A1) ──────────────────────────────────────────────

    use crate::context::{
        DelegateBatchOutcome, MAX_DELEGATE_BATCH_TIMEOUT_SECS, MAX_DELEGATE_BATCH_WIDTH,
        WorkerOutcome, WorkerStatus,
    };

    #[tokio::test]
    async fn delegate_batch_happy_aggregates_reports() {
        let ctx = MockToolContext::new();
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "api", "instructions": "build the API under /workspace"},
                    {"name": "cli", "instructions": "build the CLI under /workspace"}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        // The mock recorded the request with both workers and the default budget.
        let calls = ctx.delegate_batch_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].workers.len(), 2);
        assert_eq!(
            calls[0].timeout_secs,
            crate::context::DEFAULT_DELEGATE_BATCH_TIMEOUT_SECS
        );
        // The aggregate carries both worker names + their (mock) reports.
        let body = match &out.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            other => panic!("unexpected content block: {other:?}"),
        };
        assert!(body.contains("api"), "got: {body}");
        assert!(body.contains("cli"), "got: {body}");
        assert!(body.contains("mock report from api"), "got: {body}");
    }

    #[tokio::test]
    async fn delegate_batch_empty_workers_rejected() {
        let ctx = MockToolContext::new();
        let err =
            super::delegate_batch::handle(args_from(serde_json::json!({"workers": []})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.delegate_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn delegate_batch_over_width_rejected() {
        let ctx = MockToolContext::new();
        let workers: Vec<_> = (0..=MAX_DELEGATE_BATCH_WIDTH)
            .map(|i| serde_json::json!({"name": format!("w{i}"), "instructions": "x"}))
            .collect();
        let err =
            super::delegate_batch::handle(args_from(serde_json::json!({"workers": workers})), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(err, ToolError::Validation(s) if s.contains("at most")));
        assert!(ctx.delegate_batch_calls().is_empty());
    }

    #[tokio::test]
    async fn delegate_batch_empty_worker_field_rejected() {
        let ctx = MockToolContext::new();
        let err = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": " ", "instructions": "x"}]
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn delegate_batch_timeout_is_clamped() {
        let ctx = MockToolContext::new();
        super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}],
                "timeout_secs": 999_999
            })),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            ctx.delegate_batch_calls()[0].timeout_secs,
            MAX_DELEGATE_BATCH_TIMEOUT_SECS
        );
    }

    #[tokio::test]
    async fn delegate_batch_all_spawn_failed_is_a_refusal() {
        // When every worker failed to spawn (e.g. the subagent depth cap
        // refused the batch), the tool surfaces a clear tool error, not a
        // partial aggregate.
        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![WorkerOutcome {
                name: "a".into(),
                status: WorkerStatus::SpawnFailed,
                report: None,
                error: Some("nested create_agent (max depth = 3)".into()),
                session_id: None,
            }],
        });
        let err = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [{"name": "a", "instructions": "x"}]
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Context(s) if s.contains("refused") && s.contains("max depth")),
            "a fully-refused batch must surface as a refusal tool error"
        );
    }

    #[tokio::test]
    async fn delegate_batch_partial_failure_still_returns_aggregate() {
        // A worker failure surfaces as a per-worker error in the aggregate,
        // NOT a lost turn — as long as at least one worker didn't spawn-fail.
        let ctx = MockToolContext::new();
        ctx.set_next_delegate_batch_outcome(DelegateBatchOutcome {
            workers: vec![
                WorkerOutcome {
                    name: "ok".into(),
                    status: WorkerStatus::Ok,
                    report: Some("done".into()),
                    error: None,
                    session_id: Some("11111111-1111-1111-1111-111111111111".into()),
                },
                WorkerOutcome {
                    name: "slow".into(),
                    status: WorkerStatus::Timeout,
                    report: None,
                    error: Some("worker did not report within 300s".into()),
                    session_id: Some("22222222-2222-2222-2222-222222222222".into()),
                },
            ],
        });
        let out = super::delegate_batch::handle(
            args_from(serde_json::json!({
                "workers": [
                    {"name": "ok", "instructions": "x"},
                    {"name": "slow", "instructions": "y"}
                ]
            })),
            &ctx,
        )
        .await
        .unwrap();
        let body = match &out.content[0].raw {
            rmcp::model::RawContent::Text(t) => t.text.clone(),
            other => panic!("unexpected content block: {other:?}"),
        };
        assert!(body.contains("done"), "got: {body}");
        assert!(body.contains("did not report"), "got: {body}");
    }

    #[test]
    fn delegate_batch_schema_shape() {
        let s = super::delegate_batch::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["workers"]));
        assert_eq!(
            v["properties"]["workers"]["maxItems"],
            serde_json::json!(MAX_DELEGATE_BATCH_WIDTH)
        );
        // A worker takes exactly name + instructions (no channel).
        assert!(
            v["properties"]["workers"]["items"]["properties"]
                .get("channel")
                .is_none()
        );
    }
}
