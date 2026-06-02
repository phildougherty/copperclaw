//! Agent-management tools: `create_agent`.

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
}
