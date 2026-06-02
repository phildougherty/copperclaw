//! Self-modification tools: `install_packages`, `add_mcp_server`.
//!
//! These tools never actually mutate the container themselves — they request
//! an approval. The runner translates the effect into an approval row.

pub mod install_packages {
    //! `install_packages`: request installation of apt / npm packages.

    use crate::context::{InstallSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        #[serde(default)]
        apt: Vec<String>,
        #[serde(default)]
        npm: Vec<String>,
        reason: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "install_packages",
            "Request installation of apt and/or npm packages. Subject to approval.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["reason"],
                "properties": {
                    "apt": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 }
                    },
                    "npm": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 }
                    },
                    "reason": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.reason.trim().is_empty() {
            return Err(ToolError::Validation("`reason` must be non-empty".into()));
        }
        if input.apt.is_empty() && input.npm.is_empty() {
            return Err(ToolError::Validation(
                "must request at least one apt or npm package".into(),
            ));
        }
        for pkg in input.apt.iter().chain(input.npm.iter()) {
            if pkg.trim().is_empty() {
                return Err(ToolError::Validation(
                    "package names must be non-empty".into(),
                ));
            }
        }
        let spec = InstallSpec {
            apt: input.apt,
            npm: input.npm,
            reason: input.reason,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::InstallPackages(spec))
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

pub mod add_mcp_server {
    //! `add_mcp_server`: request the host to register a new MCP server for
    //! this agent.

    use crate::context::{AddMcpServerSpec, OutboundToolEffect, ToolContext};
    use crate::error::ToolError;
    use crate::tools::{ToolEntry, ToolHandler, ack_to_result, make_tool, parse_args};
    use rmcp::model::{CallToolResult, JsonObject, Tool};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Input {
        name: String,
        transport: serde_json::Value,
        reason: String,
    }

    pub fn schema() -> Tool {
        make_tool(
            "add_mcp_server",
            "Request the host to add an MCP server. Transport shape is host-defined.",
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "transport", "reason"],
                "properties": {
                    "name": { "type": "string", "minLength": 1 },
                    "transport": { "type": "object" },
                    "reason": { "type": "string", "minLength": 1 }
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
        if input.reason.trim().is_empty() {
            return Err(ToolError::Validation("`reason` must be non-empty".into()));
        }
        if !input.transport.is_object() {
            return Err(ToolError::Validation(
                "`transport` must be an object".into(),
            ));
        }
        let spec = AddMcpServerSpec {
            name: input.name,
            transport: input.transport,
            reason: input.reason,
        };
        let ack = ctx
            .emit_outbound(OutboundToolEffect::AddMcpServer(spec))
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
    async fn install_happy_apt() {
        let ctx = MockToolContext::new();
        super::install_packages::handle(
            args_from(serde_json::json!({"apt": ["ripgrep"], "reason": "search"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::InstallPackages(s) => {
                assert_eq!(s.apt, vec!["ripgrep"]);
                assert!(s.npm.is_empty());
                assert_eq!(s.reason, "search");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn install_happy_npm() {
        let ctx = MockToolContext::new();
        super::install_packages::handle(
            args_from(serde_json::json!({"npm": ["typescript"], "reason": "build"})),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::InstallPackages(s) => {
                assert_eq!(s.npm, vec!["typescript"]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn install_must_have_at_least_one_package() {
        let ctx = MockToolContext::new();
        let err = super::install_packages::handle(
            args_from(serde_json::json!({"reason": "nothing"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn install_blank_package_rejected() {
        let ctx = MockToolContext::new();
        let err = super::install_packages::handle(
            args_from(serde_json::json!({"apt": [" "], "reason": "r"})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn install_blank_reason() {
        let ctx = MockToolContext::new();
        let err = super::install_packages::handle(
            args_from(serde_json::json!({"apt": ["x"], "reason": "  "})),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn add_mcp_server_happy() {
        let ctx = MockToolContext::new();
        super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": "git",
                "transport": {"kind": "stdio", "cmd": "uvx", "args": ["mcp-server-git"]},
                "reason": "git ops"
            })),
            &ctx,
        )
        .await
        .unwrap();
        match &ctx.calls()[0] {
            OutboundToolEffect::AddMcpServer(s) => {
                assert_eq!(s.name, "git");
                assert_eq!(s.transport["kind"], "stdio");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn add_mcp_server_blank_name() {
        let ctx = MockToolContext::new();
        let err = super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": " ",
                "transport": {},
                "reason": "r"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn add_mcp_server_blank_reason() {
        let ctx = MockToolContext::new();
        let err = super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": "git",
                "transport": {},
                "reason": ""
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn add_mcp_server_transport_not_object() {
        let ctx = MockToolContext::new();
        let err = super::add_mcp_server::handle(
            args_from(serde_json::json!({
                "name": "git",
                "transport": "stdio",
                "reason": "r"
            })),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn schemas_have_required() {
        let s = super::install_packages::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(v["required"], serde_json::json!(["reason"]));

        let s = super::add_mcp_server::schema();
        let v: serde_json::Value = serde_json::to_value(&*s.input_schema).unwrap();
        assert_eq!(
            v["required"],
            serde_json::json!(["name", "transport", "reason"])
        );
    }
}
