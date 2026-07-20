//! Scheduling tools: `schedule_task`, `list_tasks`, `cancel_task`,
//! `pause_task`, `resume_task`, `update_task`.

/// Reject an empty / blank task id.
fn validate_task_id(id: &str) -> Result<(), crate::error::ToolError> {
    if id.trim().is_empty() {
        return Err(crate::error::ToolError::Validation(
            "`id` must be non-empty".into(),
        ));
    }
    Ok(())
}

pub mod schedule_task {
    //! `schedule_task`: enqueue a new scheduled prompt, optionally with a
    //! bounded capability `grant` that lets the task ACT on an autonomous fire
    //! (M22 A1). Authoring a grant is approval-gated: the tool validates it
    //! synchronously here, then the host raises an approval card and persists
    //! the grant only on operator approval — the task itself is created either
    //! way.

    use crate::context::{AuthorTaskGrantSpec, OutboundToolEffect, ScheduleSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use chrono::{DateTime, Duration, Utc};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    /// Longest expiry horizon a grant may request. A grant must expire; capping
    /// the horizon keeps a "bounded" grant from becoming an effectively-standing
    /// one (decision (b): no standing / always-approved grants).
    pub const MAX_GRANT_HORIZON_DAYS: i64 = 365;

    #[derive(Debug, Deserialize)]
    struct Input {
        name: String,
        #[serde(default)]
        when: Option<DateTime<Utc>>,
        prompt: String,
        #[serde(default)]
        recurrence: Option<String>,
        #[serde(default)]
        grant: Option<GrantInput>,
    }

    /// The optional capability grant an agent proposes for the task it is
    /// scheduling. Validated synchronously; persisted only after approval.
    #[derive(Debug, Deserialize)]
    struct GrantInput {
        capability_scope: String,
        #[serde(default)]
        token_budget: Option<i64>,
        #[serde(default)]
        max_fires: Option<i64>,
        expires_at: DateTime<Utc>,
        reason: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "schedule_task",
            "Schedule a prompt to be delivered at a given time, recurrence, or both. \
             Optionally attach a `grant` to PRE-AUTHORIZE the task to take a bounded \
             external action on its autonomous fires (e.g. actually send a message) \
             instead of only drafting one. A grant is approval-gated: it takes effect \
             only after a human approves the specific scope + budget + expiry, and it \
             is bounded (must expire) and revocable.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "prompt"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "when": { "type": ["string", "null"], "format": "date-time" },
                    "prompt": { "type": "string", "minLength": 1 },
                    "recurrence": { "type": ["string", "null"] },
                    "grant": {
                        "type": ["object", "null"],
                        "additionalProperties": false,
                        "required": ["capability_scope", "expires_at", "reason"],
                        "properties": {
                            "capability_scope": {
                                "type": "string",
                                "minLength": 1,
                                "description": "Space-separated capability tokens (`class` or `class:resource`, e.g. `send_message:telegram`) the task may use autonomously."
                            },
                            "token_budget": { "type": ["integer", "null"], "minimum": 1, "description": "Max tokens the grant may spend across all fires." },
                            "max_fires": { "type": ["integer", "null"], "minimum": 1, "description": "Max autonomous fires the grant authorizes." },
                            "expires_at": { "type": "string", "format": "date-time", "description": "Absolute UTC instant the grant expires (required; at most a year out)." },
                            "reason": { "type": "string", "minLength": 1, "description": "Why the task should be pre-authorized (shown on the approval card)." }
                        }
                    }
                }
            }),
        )
    }

    /// Validate a scope string: non-empty, and every whitespace-delimited token
    /// has a non-empty `class` part. Mirrors the grammar the DB matcher uses.
    fn validate_capability_scope(scope: &str) -> Result<(), ToolError> {
        let trimmed = scope.trim();
        if trimmed.is_empty() {
            return Err(ToolError::Validation(
                "`grant.capability_scope` must be non-empty".into(),
            ));
        }
        for token in trimmed.split_whitespace() {
            let class = token.split(':').next().unwrap_or("");
            if class.is_empty() {
                return Err(ToolError::Validation(format!(
                    "`grant.capability_scope` token `{token}` has an empty capability class"
                )));
            }
        }
        Ok(())
    }

    /// Validate the proposed grant against the secure-by-default bounds and turn
    /// it into an [`AuthorTaskGrantSpec`]. Rejects (synchronously, before any
    /// effect is emitted) a grant that is unbounded, non-expiring, past-dated,
    /// or too far out.
    fn build_grant_spec(task_name: &str, g: GrantInput) -> Result<AuthorTaskGrantSpec, ToolError> {
        validate_capability_scope(&g.capability_scope)?;
        if g.reason.trim().is_empty() {
            return Err(ToolError::Validation(
                "`grant.reason` must be non-empty".into(),
            ));
        }
        if let Some(b) = g.token_budget {
            if b <= 0 {
                return Err(ToolError::Validation(
                    "`grant.token_budget`, when present, must be positive".into(),
                ));
            }
        }
        if let Some(m) = g.max_fires {
            if m <= 0 {
                return Err(ToolError::Validation(
                    "`grant.max_fires`, when present, must be positive".into(),
                ));
            }
        }
        // Bounded, never standing: require at least one spend bound in addition
        // to the mandatory expiry.
        if g.token_budget.is_none() && g.max_fires.is_none() {
            return Err(ToolError::Validation(
                "`grant` must set at least one of `token_budget` or `max_fires` (grants are \
                 bounded, never unlimited)"
                    .into(),
            ));
        }
        let now = Utc::now();
        if g.expires_at <= now {
            return Err(ToolError::Validation(
                "`grant.expires_at` must be in the future".into(),
            ));
        }
        if g.expires_at > now + Duration::days(MAX_GRANT_HORIZON_DAYS) {
            return Err(ToolError::Validation(format!(
                "`grant.expires_at` must be at most {MAX_GRANT_HORIZON_DAYS} days out"
            )));
        }
        Ok(AuthorTaskGrantSpec {
            task_name: task_name.to_string(),
            capability_scope: g
                .capability_scope
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            token_budget: g.token_budget,
            max_fires: g.max_fires,
            expires_at: g.expires_at,
            reason: g.reason,
        })
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.name.trim().is_empty() {
            return Err(ToolError::Validation("`name` must be non-empty".into()));
        }
        if input.prompt.trim().is_empty() {
            return Err(ToolError::Validation("`prompt` must be non-empty".into()));
        }
        if input.when.is_none() && input.recurrence.as_deref().is_none_or(str::is_empty) {
            return Err(ToolError::Validation(
                "must supply at least one of `when` or `recurrence`".into(),
            ));
        }
        if let Some(rec) = input.recurrence.as_ref() {
            if rec.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`recurrence`, when present, must be non-empty".into(),
                ));
            }
        }
        // Validate the grant BEFORE emitting anything so a bad grant rejects the
        // whole call (no task created, no card raised).
        let grant_spec = match input.grant {
            Some(g) => Some(build_grant_spec(input.name.trim(), g)?),
            None => None,
        };
        let spec = ScheduleSpec {
            name: input.name,
            when: input.when,
            prompt: input.prompt,
            recurrence: input.recurrence,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::ScheduleTask(spec))
            .await?;
        // On a grant request, emit the grant-authoring effect right after the
        // task-create effect (same order the host processes them, so the task
        // exists when the host binds + raises the grant approval).
        if let Some(grant_spec) = grant_spec {
            ctx.emit_outbound(OutboundToolEffect::AuthorTaskGrant(grant_spec))
                .await?;
        }
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

pub mod list_tasks {
    //! `list_tasks`: return all known scheduled tasks for this agent.

    use crate::context::ToolContext;
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, make_tool, success_json};
    use rmcp::model::{CallToolResult, JsonObject, Tool};

    pub fn schema() -> Tool {
        make_tool(
            "list_tasks",
            "List all scheduled tasks for this agent.",
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
        let tasks = ctx.list_tasks().await?;
        Ok(success_json(&tasks))
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

pub mod cancel_task {
    //! `cancel_task`: cancel a scheduled task by id.

    use super::validate_task_id;
    use crate::context::{OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        id: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "cancel_task",
            "Cancel a scheduled task by id.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": { "id": { "type": "string", "minLength": 1 } }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        validate_task_id(&input.id)?;
        let ack = ctx
            .emit_outbound(OutboundToolEffect::CancelTask { id: input.id })
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

pub mod pause_task {
    //! `pause_task`: pause a scheduled task by id.

    use super::validate_task_id;
    use crate::context::{OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        id: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "pause_task",
            "Pause a scheduled task by id; the task can later be resumed.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": { "id": { "type": "string", "minLength": 1 } }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        validate_task_id(&input.id)?;
        let ack = ctx
            .emit_outbound(OutboundToolEffect::PauseTask { id: input.id })
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

pub mod resume_task {
    //! `resume_task`: resume a paused task by id.

    use super::validate_task_id;
    use crate::context::{OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        id: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "resume_task",
            "Resume a previously paused task by id.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": { "id": { "type": "string", "minLength": 1 } }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        validate_task_id(&input.id)?;
        let ack = ctx
            .emit_outbound(OutboundToolEffect::ResumeTask { id: input.id })
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

pub mod update_task {
    //! `update_task`: update a scheduled task in place. At least one of
    //! `prompt`/`when`/`recurrence` must be supplied.

    use super::validate_task_id;
    use crate::context::{OutboundToolEffect, ToolContext, UpdateTaskSpec};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use chrono::{DateTime, Utc};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    /// Update payload. We use double-`Option` only for `when` and
    /// `recurrence` so the caller can explicitly clear them (`null`).
    #[allow(clippy::option_option)]
    #[derive(Debug, Deserialize)]
    struct Input {
        id: String,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default, deserialize_with = "deserialize_double_option")]
        when: Option<Option<DateTime<Utc>>>,
        #[serde(default, deserialize_with = "deserialize_double_option")]
        recurrence: Option<Option<String>>,
    }

    #[allow(clippy::option_option)]
    fn deserialize_double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: serde::Deserializer<'de>,
        T: serde::Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }

    pub fn schema() -> Tool {
        make_tool(
            "update_task",
            "Update a scheduled task in place. Pass `null` for `when`/`recurrence` to clear them.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "minLength": 1 },
                    "prompt": { "type": ["string", "null"] },
                    "when": { "type": ["string", "null"], "format": "date-time" },
                    "recurrence": { "type": ["string", "null"] }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        validate_task_id(&input.id)?;
        if input.prompt.is_none() && input.when.is_none() && input.recurrence.is_none() {
            return Err(ToolError::Validation(
                "must supply at least one of `prompt`, `when`, or `recurrence` to update".into(),
            ));
        }
        if let Some(p) = input.prompt.as_ref() {
            if p.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`prompt`, when present, must be non-empty".into(),
                ));
            }
        }
        if let Some(Some(r)) = input.recurrence.as_ref() {
            if r.trim().is_empty() {
                return Err(ToolError::Validation(
                    "`recurrence`, when present, must be non-empty (use `null` to clear)".into(),
                ));
            }
        }
        let spec = UpdateTaskSpec {
            id: input.id,
            prompt: input.prompt,
            when: input.when,
            recurrence: input.recurrence,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::UpdateTask(spec))
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
    use crate::context::{MockToolContext, OutboundToolEffect, TaskSummary};
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
    async fn schedule_with_when() {
        let ctx = MockToolContext::new();
        super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "morning",
                "when": "2026-05-21T06:00:00Z",
                "prompt": "wake"
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::ScheduleTask(s) => {
                assert_eq!(s.name, "morning");
                assert!(s.when.is_some());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn schedule_with_recurrence_only() {
        let ctx = MockToolContext::new();
        super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "hourly",
                "prompt": "tick",
                "recurrence": "0 * * * *"
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::ScheduleTask(s) => {
                assert_eq!(s.recurrence.as_deref(), Some("0 * * * *"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn schedule_requires_when_or_recurrence() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({"name": "n", "prompt": "p"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn schedule_blank_name() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": " ", "prompt": "p", "recurrence": "0 * * * *"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn schedule_blank_recurrence() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "n", "prompt": "p", "recurrence": "   "
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn schedule_blank_prompt() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "n", "prompt": "", "recurrence": "0 * * * *"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn schedule_with_grant_emits_both_effects() {
        let ctx = MockToolContext::new();
        // Clock-relative expiry so the test stays valid as real time advances.
        let expires = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339();
        super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "standup",
                "prompt": "post yesterday's commits",
                "recurrence": "0 9 * * *",
                "grant": {
                    "capability_scope": "send_message:telegram",
                    "token_budget": 50000,
                    "max_fires": 30,
                    "expires_at": expires,
                    "reason": "morning standup posting"
                }
            })),
            &ctx,
        )
        .await
        .unwrap();
        let calls = ctx.calls();
        assert_eq!(calls.len(), 2, "task-create then grant-author");
        assert!(matches!(&calls[0], OutboundToolEffect::ScheduleTask(_)));
        match &calls[1] {
            OutboundToolEffect::AuthorTaskGrant(g) => {
                assert_eq!(g.task_name, "standup");
                assert_eq!(g.capability_scope, "send_message:telegram");
                assert_eq!(g.token_budget, Some(50000));
                assert_eq!(g.max_fires, Some(30));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn schedule_without_grant_emits_only_schedule() {
        let ctx = MockToolContext::new();
        super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "n", "prompt": "p", "recurrence": "0 9 * * *"
            })),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(ctx.calls().len(), 1);
        assert!(matches!(
            &ctx.calls()[0],
            OutboundToolEffect::ScheduleTask(_)
        ));
    }

    #[tokio::test]
    async fn grant_requires_a_spend_bound() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "n", "prompt": "p", "recurrence": "0 9 * * *",
                "grant": {
                    "capability_scope": "send_message:telegram",
                    "expires_at": "2026-08-01T00:00:00Z",
                    "reason": "r"
                }
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        // Nothing emitted on validation failure — no task created either.
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn grant_rejects_past_expiry() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "n", "prompt": "p", "recurrence": "0 9 * * *",
                "grant": {
                    "capability_scope": "send_message:telegram",
                    "max_fires": 5,
                    "expires_at": "2000-01-01T00:00:00Z",
                    "reason": "r"
                }
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn grant_rejects_far_future_expiry() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "n", "prompt": "p", "recurrence": "0 9 * * *",
                "grant": {
                    "capability_scope": "send_message:telegram",
                    "max_fires": 5,
                    "expires_at": "2100-01-01T00:00:00Z",
                    "reason": "r"
                }
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn grant_rejects_empty_scope_token() {
        let ctx = MockToolContext::new();
        let err = super::schedule_task::handle(
            args_from(serde_json::json!({
                "name": "n", "prompt": "p", "recurrence": "0 9 * * *",
                "grant": {
                    "capability_scope": ":telegram",
                    "max_fires": 5,
                    "expires_at": "2026-08-01T00:00:00Z",
                    "reason": "r"
                }
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn list_tasks_returns_seeded() {
        let ctx = MockToolContext::new();
        ctx.set_tasks(vec![TaskSummary {
            id: "task_1".into(),
            name: "tick".into(),
            status: "active".into(),
            when: None,
            recurrence: Some("0 * * * *".into()),
        }]);
        let res = super::list_tasks::handle(None, &ctx).await.unwrap();
        assert_eq!(res.is_error, Some(false));
        let text = res.content[0].as_text().expect("text content").text.clone();
        assert!(text.contains("task_1"), "got: {text}");
    }

    #[tokio::test]
    async fn list_tasks_propagates_failure() {
        let ctx = MockToolContext::new();
        ctx.fail_next_list(ToolError::Context("boom".into()));
        let err = super::list_tasks::handle(None, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Context(_)));
    }

    #[tokio::test]
    async fn cancel_happy() {
        let ctx = MockToolContext::new();
        super::cancel_task::handle(args_from(serde_json::json!({"id": "task_1"})), &ctx)
            .await
            .unwrap();
        assert!(matches!(
            &ctx.calls()[0],
            OutboundToolEffect::CancelTask { id } if id == "task_1"
        ));
    }

    #[tokio::test]
    async fn cancel_blank_id() {
        let ctx = MockToolContext::new();
        let err = super::cancel_task::handle(args_from(serde_json::json!({"id": " "})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn pause_happy_and_blank() {
        let ctx = MockToolContext::new();
        super::pause_task::handle(args_from(serde_json::json!({"id": "task_1"})), &ctx)
            .await
            .unwrap();
        assert!(matches!(
            &ctx.calls()[0],
            OutboundToolEffect::PauseTask { id } if id == "task_1"
        ));
        let err = super::pause_task::handle(args_from(serde_json::json!({"id": ""})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn resume_happy_and_blank() {
        let ctx = MockToolContext::new();
        super::resume_task::handle(args_from(serde_json::json!({"id": "task_1"})), &ctx)
            .await
            .unwrap();
        assert!(matches!(
            &ctx.calls()[0],
            OutboundToolEffect::ResumeTask { id } if id == "task_1"
        ));
        let err = super::resume_task::handle(args_from(serde_json::json!({"id": "  "})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn update_task_happy_prompt_only() {
        let ctx = MockToolContext::new();
        super::update_task::handle(
            args_from(serde_json::json!({"id": "task_1", "prompt": "new prompt"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::UpdateTask(s) => {
                assert_eq!(s.id, "task_1");
                assert_eq!(s.prompt.as_deref(), Some("new prompt"));
                assert!(s.when.is_none());
                assert!(s.recurrence.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_task_clear_recurrence_with_null() {
        let ctx = MockToolContext::new();
        super::update_task::handle(
            args_from(serde_json::json!({"id": "task_1", "recurrence": null})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::UpdateTask(s) => {
                assert_eq!(s.recurrence, Some(None));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_task_clear_when_with_null() {
        let ctx = MockToolContext::new();
        super::update_task::handle(
            args_from(serde_json::json!({"id": "task_1", "when": null})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::UpdateTask(s) => {
                assert_eq!(s.when, Some(None));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_task_requires_at_least_one_field() {
        let ctx = MockToolContext::new();
        let err = super::update_task::handle(args_from(serde_json::json!({"id": "task_1"})), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn update_task_blank_prompt() {
        let ctx = MockToolContext::new();
        let err = super::update_task::handle(
            args_from(serde_json::json!({"id": "task_1", "prompt": "  "})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn update_task_blank_recurrence_string() {
        let ctx = MockToolContext::new();
        let err = super::update_task::handle(
            args_from(serde_json::json!({"id": "task_1", "recurrence": " "})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn update_task_blank_id() {
        let ctx = MockToolContext::new();
        let err = super::update_task::handle(
            args_from(serde_json::json!({"id": "", "prompt": "x"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn scheduling_schemas_have_required() {
        let s = super::schedule_task::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["name", "prompt"]));

        for f in [
            super::cancel_task::schema,
            super::pause_task::schema,
            super::resume_task::schema,
        ] {
            let s = f();
            let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
            assert_eq!(v["required"], serde_json::json!(["id"]));
        }

        let s = super::update_task::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["id"]));
    }
}
