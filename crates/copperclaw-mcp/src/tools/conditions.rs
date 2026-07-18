//! Condition tools (M22 A4): `register_condition`, `set_condition_flag`.
//!
//! These are the registration surface that revives the previously-dormant
//! HEARTBEAT-style condition check-in in the host sweep
//! (`copperclaw_host_sweep::checks::condition_checkin`). A *condition* is a
//! durable, event-driven wake: the sweep fires a `kind:task` check-in into the
//! session on the RISING edge of the condition's predicate (idle past a floor,
//! a set flag, or a backlog of pending inbound) rather than on a clock deadline.
//!
//! `register_condition` emits a `{"condition": {...}}` system row the host
//! persists into the central `conditions` table (mirroring the `create_goal`
//! path). `set_condition_flag` toggles the per-session latch a `flag` condition
//! watches. Both are grant-scoped: a condition is internal tracking state that
//! authorizes nothing on its own — any autonomous action the woken turn takes
//! stays gated by A2's grant machinery at fire time.

/// The three condition kinds, mirroring
/// `copperclaw_host_sweep::checks::condition_checkin::ConditionKind`.
pub(crate) const KINDS: [&str; 3] = ["pending_inbound", "idle", "flag"];

pub mod register_condition {
    //! `register_condition`: declare (or deregister) a durable event-driven wake.

    use super::KINDS;
    use crate::context::{OutboundToolEffect, RegisterConditionSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        id: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        threshold: Option<i64>,
        #[serde(default)]
        flag: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        grant_id: Option<String>,
        #[serde(default)]
        remove: bool,
    }

    pub fn schema() -> Tool {
        make_tool(
            "register_condition",
            "Register a durable, EVENT-DRIVEN wake (a HEARTBEAT-style condition): the runtime \
             wakes you with a check-in only when a stored predicate BECOMES true (a rising edge), \
             not on a clock (use `schedule_task` for timed wakes). Kinds: `idle` (wake after the \
             session has been quiet for `threshold` seconds), `pending_inbound` (wake once at least \
             `threshold` messages are queued), or `flag` (wake when the named `flag` latch is set — \
             raise/lower it with `set_condition_flag`). `id` is a stable key you choose; \
             re-registering the same id replaces it. Pass `remove: true` with the `id` to \
             deregister. A condition is internal tracking state — it authorizes no external action \
             on its own.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "minLength": 1, "description": "Stable key you choose; re-registering replaces." },
                    "kind": { "type": ["string", "null"], "enum": ["pending_inbound", "idle", "flag", null], "description": "The observable tested (required unless removing)." },
                    "threshold": { "type": ["integer", "null"], "minimum": 1, "description": "`idle`: idle-seconds floor. `pending_inbound`: minimum queued count." },
                    "flag": { "type": ["string", "null"], "description": "`flag` kind: the watched flag name." },
                    "prompt": { "type": ["string", "null"], "description": "Text delivered on the check-in wake (required unless removing)." },
                    "grant_id": { "type": ["string", "null"], "description": "Optional task-grant id linked for fire-time authorization (A2)." },
                    "remove": { "type": "boolean", "default": false, "description": "Deregister the condition `id` instead of registering." }
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
        if !input.remove {
            // Registration requires a valid kind, its kind-specific field, and a prompt.
            let kind = input.kind.as_deref().ok_or_else(|| {
                ToolError::Validation("`kind` is required when registering".into())
            })?;
            if !KINDS.contains(&kind) {
                return Err(ToolError::Validation(format!(
                    "`kind` must be one of {KINDS:?}"
                )));
            }
            match kind {
                "pending_inbound" | "idle" => {
                    if input.threshold.is_none_or(|t| t < 1) {
                        return Err(ToolError::Validation(format!(
                            "`{kind}` requires a positive `threshold`"
                        )));
                    }
                }
                "flag" => {
                    if input.flag.as_deref().is_none_or(|f| f.trim().is_empty()) {
                        return Err(ToolError::Validation(
                            "`flag` kind requires a non-empty `flag` name".into(),
                        ));
                    }
                }
                _ => unreachable!("kind validated against KINDS above"),
            }
            if input.prompt.as_deref().is_none_or(|p| p.trim().is_empty()) {
                return Err(ToolError::Validation(
                    "`prompt` is required when registering".into(),
                ));
            }
        }
        let spec = RegisterConditionSpec {
            id: input.id,
            kind: input.kind.unwrap_or_default(),
            threshold: input.threshold,
            flag: input.flag,
            prompt: input.prompt,
            grant_id: input.grant_id,
            remove: input.remove,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::RegisterCondition(spec))
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

pub mod set_condition_flag {
    //! `set_condition_flag`: set or clear the latch a `flag` condition watches.

    use crate::context::{OutboundToolEffect, SetConditionFlagSpec, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        flag: String,
        value: bool,
    }

    pub fn schema() -> Tool {
        make_tool(
            "set_condition_flag",
            "Raise or lower a named per-session FLAG latch — the settable signal a `flag` \
             condition (see `register_condition`) fires on. `value: true` sets the flag (a `flag` \
             condition watching it wakes you on the next sweep), `value: false` clears it. Use this \
             to arm an event-driven wake for later (e.g. raise a `deploying` flag now so an idle+flag \
             condition can nudge you when the deploy goes quiet).",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["flag", "value"],
                "properties": {
                    "flag": { "type": "string", "minLength": 1, "description": "The flag name to set or clear." },
                    "value": { "type": "boolean", "description": "true = set the flag, false = clear it." }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.flag.trim().is_empty() {
            return Err(ToolError::Validation("`flag` must be non-empty".into()));
        }
        let spec = SetConditionFlagSpec {
            flag: input.flag,
            value: input.value,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::SetConditionFlag(spec))
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
    use crate::context::{MockToolContext, OutboundToolEffect};
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
    async fn register_idle_condition_emits_effect() {
        let ctx = MockToolContext::new();
        super::register_condition::handle(
            args_from(serde_json::json!({
                "id": "idle-watchdog",
                "kind": "idle",
                "threshold": 300,
                "prompt": "you've gone quiet — check in"
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::RegisterCondition(s) => {
                assert_eq!(s.id, "idle-watchdog");
                assert_eq!(s.kind, "idle");
                assert_eq!(s.threshold, Some(300));
                assert!(!s.remove);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_flag_condition_emits_effect() {
        let ctx = MockToolContext::new();
        super::register_condition::handle(
            args_from(serde_json::json!({
                "id": "on-deploy",
                "kind": "flag",
                "flag": "deploying",
                "prompt": "the deploy flag is set"
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::RegisterCondition(s) => {
                assert_eq!(s.flag.as_deref(), Some("deploying"));
                assert_eq!(s.kind, "flag");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn remove_needs_only_id() {
        let ctx = MockToolContext::new();
        super::register_condition::handle(
            args_from(serde_json::json!({ "id": "idle-watchdog", "remove": true })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::RegisterCondition(s) => {
                assert!(s.remove);
                assert_eq!(s.id, "idle-watchdog");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_rejects_missing_kind_prompt_and_threshold() {
        let ctx = MockToolContext::new();
        for bad in [
            // Missing kind.
            serde_json::json!({ "id": "c", "prompt": "p" }),
            // idle without threshold.
            serde_json::json!({ "id": "c", "kind": "idle", "prompt": "p" }),
            // flag without flag name.
            serde_json::json!({ "id": "c", "kind": "flag", "prompt": "p" }),
            // Missing prompt.
            serde_json::json!({ "id": "c", "kind": "idle", "threshold": 300 }),
            // Bad kind.
            serde_json::json!({ "id": "c", "kind": "bogus", "prompt": "p" }),
            // Blank id.
            serde_json::json!({ "id": "  ", "kind": "idle", "threshold": 1, "prompt": "p" }),
        ] {
            let err = super::register_condition::handle(args_from(bad), &ctx)
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::Validation(_)));
        }
        assert!(ctx.calls().is_empty());
    }

    #[tokio::test]
    async fn set_condition_flag_emits_effect() {
        let ctx = MockToolContext::new();
        super::set_condition_flag::handle(
            args_from(serde_json::json!({ "flag": "deploying", "value": true })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::SetConditionFlag(s) => {
                assert_eq!(s.flag, "deploying");
                assert!(s.value);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_condition_flag_rejects_blank() {
        let ctx = MockToolContext::new();
        let err = super::set_condition_flag::handle(
            args_from(serde_json::json!({ "flag": " ", "value": false })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
        assert!(ctx.calls().is_empty());
    }

    #[test]
    fn condition_schemas_have_required() {
        let v: Value =
            serde_json::to_value(&*super::register_condition::schema().input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["id"]));
        let v: Value =
            serde_json::to_value(&*super::set_condition_flag::schema().input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["flag", "value"]));
    }
}
