//! Goal tools (M22 A3): `create_goal`, `list_goals`, `update_goal`.
//!
//! A goal is a durable, long-running objective the sweep drives check-ins
//! against (decision (d): it INDEXES over the in-session todo plan + the memory
//! store, it does not replace them). `create_goal`/`update_goal` emit effects
//! the runner writes as `{"goal": {...}}` system rows, which the host persists
//! into the central `goals` table (mirroring the `schedule_*` path).
//! `list_goals` is a read hook — goal state lives on the host, so the container
//! returns an empty list, exactly like `list_tasks`.

pub mod create_goal {
    //! `create_goal`: declare a durable long-running objective.

    use crate::context::{CreateGoalSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use chrono::{DateTime, Utc};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        objective: String,
        #[serde(default)]
        checkin_recurrence: Option<String>,
        #[serde(default)]
        first_checkin: Option<DateTime<Utc>>,
        #[serde(default)]
        checkin_prompt: Option<String>,
        #[serde(default)]
        token_budget: Option<i64>,
    }

    pub fn schema() -> Tool {
        make_tool(
            "create_goal",
            "Declare a durable, long-running GOAL — a multi-step objective the runtime tracks \
             across days with its own status, progress log, and (optional) token budget, and \
             wakes you to check in on. Use this for objectives that outlive a single conversation \
             (not a one-off task: use `schedule_task` for a timed prompt). Set `checkin_recurrence` \
             (cron) to be woken periodically to report progress via `update_goal`. A goal is \
             internal tracking state — it does not by itself authorize any external action.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["objective"],
                "properties": {
                    "objective": { "type": "string", "minLength": 1, "description": "What the goal is for." },
                    "checkin_recurrence": { "type": ["string", "null"], "description": "Cron (croner) cadence for check-in wakes." },
                    "first_checkin": { "type": ["string", "null"], "format": "date-time", "description": "Absolute UTC time of the first check-in (else derived from the recurrence)." },
                    "checkin_prompt": { "type": ["string", "null"], "description": "Prompt injected on a check-in wake (else synthesised from the objective)." },
                    "token_budget": { "type": ["integer", "null"], "minimum": 1, "description": "The goal's own cumulative token cap (ignored when a grant is linked)." }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.objective.trim().is_empty() {
            return Err(ToolError::Validation(
                "`objective` must be non-empty".into(),
            ));
        }
        if let Some(rec) = input.checkin_recurrence.as_ref() {
            if rec.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`checkin_recurrence`, when present, must be non-empty".into(),
                ));
            }
        }
        if let Some(b) = input.token_budget {
            if b <= 0 {
                return Err(ToolError::Validation(
                    "`token_budget`, when present, must be positive".into(),
                ));
            }
        }
        let spec = CreateGoalSpec {
            objective: input.objective,
            checkin_recurrence: input.checkin_recurrence,
            first_checkin: input.first_checkin,
            checkin_prompt: input.checkin_prompt,
            token_budget: input.token_budget,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::CreateGoal(spec))
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

pub mod list_goals {
    //! `list_goals`: list this agent's goals.

    use crate::context::ToolContext;
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};

    pub fn schema() -> Tool {
        make_tool(
            "list_goals",
            "List this agent's long-running goals with their status, next check-in, and progress.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        )
    }

    pub async fn handle(
        _arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let goals = ctx.list_goals().await?;
        Ok(success_json(&goals))
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

pub mod update_goal {
    //! `update_goal`: report progress and/or transition a goal's lifecycle.

    use crate::context::{OutboundToolEffect, ToolContext, UpdateGoalSpec};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    /// Legal `status` values, mirroring `copperclaw_db::tables::goals::GoalStatus`.
    const STATUSES: [&str; 4] = ["active", "paused", "completed", "abandoned"];

    #[derive(Debug, Deserialize)]
    struct Input {
        id: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        progress: Option<String>,
        #[serde(default)]
        progress_tokens: Option<i64>,
        #[serde(default)]
        objective: Option<String>,
    }

    pub fn schema() -> Tool {
        make_tool(
            "update_goal",
            "Update a long-running goal (from `create_goal`, id supplied in the check-in wake): \
             report `progress`, transition `status` (active/paused/completed/abandoned), or refine \
             the `objective`. Call this on a goal check-in wake to record what you did.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "minLength": 1 },
                    "status": { "type": ["string", "null"], "enum": ["active", "paused", "completed", "abandoned", null] },
                    "progress": { "type": ["string", "null"], "description": "A progress note appended to the goal's log." },
                    "progress_tokens": { "type": ["integer", "null"], "minimum": 0, "description": "Tokens attributed to this progress report (accrued cumulatively)." },
                    "objective": { "type": ["string", "null"], "description": "A revised objective." }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.id.trim().is_empty() {
            return Err(ToolError::Validation("`id` must be non-empty".into()));
        }
        if input.status.is_none() && input.progress.is_none() && input.objective.is_none() {
            return Err(ToolError::Validation(
                "must supply at least one of `status`, `progress`, or `objective`".into(),
            ));
        }
        if let Some(s) = input.status.as_deref() {
            if !STATUSES.contains(&s) {
                return Err(ToolError::Validation(format!(
                    "`status` must be one of {STATUSES:?}"
                )));
            }
        }
        if let Some(p) = input.progress.as_ref() {
            if p.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`progress`, when present, must be non-empty".into(),
                ));
            }
        }
        if let Some(o) = input.objective.as_ref() {
            if o.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`objective`, when present, must be non-empty".into(),
                ));
            }
        }
        if let Some(t) = input.progress_tokens {
            if t < 0 {
                return Err(ToolError::Validation(
                    "`progress_tokens`, when present, must be non-negative".into(),
                ));
            }
        }
        let spec = UpdateGoalSpec {
            id: input.id,
            status: input.status,
            progress: input.progress,
            progress_tokens: input.progress_tokens,
            objective: input.objective,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::UpdateGoal(spec))
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

#[cfg(test)]
mod tests {
    use crate::context::{GoalSummary, MockToolContext, OutboundToolEffect};
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
    async fn create_goal_emits_effect() {
        let ctx = MockToolContext::new();
        super::create_goal::handle(
            args_from(serde_json::json!({
                "objective": "keep the docs current",
                "checkin_recurrence": "0 9 * * *",
                "token_budget": 50000
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::CreateGoal(s) => {
                assert_eq!(s.objective, "keep the docs current");
                assert_eq!(s.checkin_recurrence.as_deref(), Some("0 9 * * *"));
                assert_eq!(s.token_budget, Some(50000));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_goal_rejects_blank_objective() {
        let ctx = MockToolContext::new();
        let err =
            super::create_goal::handle(args_from(serde_json::json!({ "objective": "  " })), &ctx)
                .await
                .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn create_goal_rejects_blank_recurrence_and_bad_budget() {
        let ctx = MockToolContext::new();
        for bad in [
            serde_json::json!({ "objective": "x", "checkin_recurrence": " " }),
            serde_json::json!({ "objective": "x", "token_budget": 0 }),
        ] {
            let err = super::create_goal::handle(args_from(bad), &ctx)
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::Validation(_)));
        }
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn update_goal_reports_progress() {
        let ctx = MockToolContext::new();
        super::update_goal::handle(
            args_from(serde_json::json!({
                "id": "g-1",
                "progress": "wrote the schema",
                "progress_tokens": 120
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::UpdateGoal(s) => {
                assert_eq!(s.id, "g-1");
                assert_eq!(s.progress.as_deref(), Some("wrote the schema"));
                assert_eq!(s.progress_tokens, Some(120));
                assert!(s.status.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_goal_transitions_status() {
        let ctx = MockToolContext::new();
        super::update_goal::handle(
            args_from(serde_json::json!({ "id": "g-1", "status": "completed" })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::UpdateGoal(s) => assert_eq!(s.status.as_deref(), Some("completed")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_goal_requires_a_field() {
        let ctx = MockToolContext::new();
        let err = super::update_goal::handle(args_from(serde_json::json!({ "id": "g-1" })), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn update_goal_rejects_bad_status() {
        let ctx = MockToolContext::new();
        let err = super::update_goal::handle(
            args_from(serde_json::json!({ "id": "g-1", "status": "bogus" })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn update_goal_blank_id() {
        let ctx = MockToolContext::new();
        let err = super::update_goal::handle(
            args_from(serde_json::json!({ "id": " ", "status": "paused" })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn list_goals_returns_seeded() {
        let ctx = MockToolContext::new();
        ctx.set_goals(vec![GoalSummary {
            id: "g-1".into(),
            objective: "keep the docs current".into(),
            status: "active".into(),
            next_checkin: None,
            checkin_count: 2,
            tokens_consumed: 300,
        }]);
        let res = super::list_goals::handle(None, &ctx).await.unwrap();
        assert_eq!(res.is_error, Some(false));
        let text = res.content[0].as_text().expect("text content").text.clone();
        assert!(text.contains("g-1"), "got: {text}");
    }

    #[test]
    fn goal_schemas_have_required() {
        let v: serde_json::Value =
            serde_json::to_value(&*super::create_goal::schema().input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["objective"]));
        let v: serde_json::Value =
            serde_json::to_value(&*super::update_goal::schema().input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["id"]));
    }
}
