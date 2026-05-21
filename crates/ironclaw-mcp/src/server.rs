//! rmcp `ServerHandler` for the ironclaw tool inventory.
//!
//! The handler owns an `Arc<dyn ToolContext>` (the runner / mock) and an
//! immutable table of registered tools. `list_tools` and `call_tool` are
//! served from that table; everything else uses rmcp's defaults.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, ErrorData as McpModelError, Implementation,
    ListToolsResult, PaginatedRequestParam, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::{build_tool_set, ToolEntry};

/// rmcp `ServerHandler` exposing the 15 ironclaw tools.
#[derive(Clone)]
pub struct IronclawServer {
    inner: Arc<Inner>,
}

struct Inner {
    ctx: Arc<dyn ToolContext>,
    tools: HashMap<String, Arc<ToolEntry>>,
    /// Stable order for `list_tools` output, matching `build_tool_set`.
    tool_order: Vec<String>,
    info: ServerInfo,
}

impl IronclawServer {
    /// Build a server bound to the given context.
    pub fn new(ctx: Arc<dyn ToolContext>) -> Self {
        let entries = build_tool_set();
        let tool_order: Vec<String> = entries.iter().map(|t| t.tool.name.to_string()).collect();
        let tools: HashMap<String, Arc<ToolEntry>> = entries
            .into_iter()
            .map(|t| (t.tool.name.to_string(), Arc::new(t)))
            .collect();
        let info = ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "ironclaw-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            instructions: Some(
                "ironclaw agent container MCP server: messaging, scheduling, self-mod tools."
                    .into(),
            ),
        };
        Self {
            inner: Arc::new(Inner {
                ctx,
                tools,
                tool_order,
                info,
            }),
        }
    }

    /// Snapshot of registered tool descriptors, in stable order.
    pub fn tool_descriptors(&self) -> Vec<Tool> {
        self.inner
            .tool_order
            .iter()
            .filter_map(|n| self.inner.tools.get(n).map(|t| t.tool.clone()))
            .collect()
    }

    /// Dispatch `tool_name` against the bound context. Returns a
    /// `CallToolResult` either way: validation/context errors are surfaced
    /// as `is_error: true` text-content results so the calling agent can
    /// react. The Err return is reserved for protocol-level failures.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Option<rmcp::model::JsonObject>,
    ) -> Result<CallToolResult, McpModelError> {
        let Some(entry) = self.inner.tools.get(name) else {
            return Err(McpModelError::invalid_request(
                format!("unknown tool: {name}"),
                None,
            ));
        };
        match entry.handler.call(arguments, self.inner.ctx.as_ref()).await {
            Ok(result) => Ok(result),
            Err(err) => Ok(tool_error_to_result(&err)),
        }
    }
}

/// Translate a `ToolError` into a `CallToolResult` with `is_error: true`.
fn tool_error_to_result(err: &ToolError) -> CallToolResult {
    let body = serde_json::json!({
        "error_kind": err.kind(),
        "message": err.to_string(),
    });
    let text = serde_json::to_string(&body).unwrap_or_else(|e| format!("{e}"));
    CallToolResult::error(vec![Content::text(text)])
}

impl ServerHandler for IronclawServer {
    fn get_info(&self) -> ServerInfo {
        self.inner.info.clone()
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpModelError>> + Send + '_
    {
        let descriptors = self.tool_descriptors();
        std::future::ready(Ok(ListToolsResult::with_all_items(descriptors)))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, McpModelError>> + Send + '_ {
        let this = self.clone();
        async move { this.dispatch(request.name.as_ref(), request.arguments).await }
    }
}

/// Build a fully-configured `IronclawServer` ready to be `serve`d on a
/// transport (e.g. `TokioChildProcess` for stdio).
///
/// ```ignore
/// use std::sync::Arc;
/// use rmcp::ServiceExt;
/// use ironclaw_mcp::{build_server, MockToolContext};
///
/// # tokio_test::block_on(async {
/// let ctx = Arc::new(MockToolContext::new());
/// let server = build_server(ctx);
/// // server.serve(transport).await ...
/// # })
/// ```
pub fn build_server(ctx: Arc<dyn ToolContext>) -> IronclawServer {
    IronclawServer::new(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{MockToolContext, OutboundToolEffect, ToolEffectAck};
    use rmcp::model::JsonObject;

    fn args(value: serde_json::Value) -> Option<JsonObject> {
        match value {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        }
    }

    #[tokio::test]
    async fn lists_all_in_process_tools() {
        let ctx = Arc::new(MockToolContext::new());
        let server = build_server(ctx);
        let tools = server.tool_descriptors();
        // Order is fixed in `build_tool_set` — first and last should
        // be stable as new tools get appended.
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(names[0], "send_message");
        // The 15 messaging/scheduling/agent tools come first, then
        // the `computer_use` family. `web_fetch` is the current tail.
        assert_eq!(*names.last().unwrap(), "web_fetch");
        assert_eq!(tools.len(), crate::tools::build_tool_set().len());
    }

    #[tokio::test]
    async fn dispatch_routes_to_handler() {
        let ctx = Arc::new(MockToolContext::new());
        ctx.set_next_ack(ToolEffectAck::Message { seq: 99 });
        let server = build_server(ctx.clone());
        let res = server
            .dispatch("send_message", args(serde_json::json!({"text": "hi"})))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
        assert!(matches!(
            &ctx.calls()[0],
            OutboundToolEffect::SendMessage(_)
        ));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_is_protocol_error() {
        let ctx = Arc::new(MockToolContext::new());
        let server = build_server(ctx);
        let err = server.dispatch("not_a_tool", None).await.unwrap_err();
        assert!(err.message.contains("unknown tool"));
    }

    #[tokio::test]
    async fn dispatch_validation_error_returned_as_is_error() {
        let ctx = Arc::new(MockToolContext::new());
        let server = build_server(ctx);
        let res = server
            .dispatch("send_message", args(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("validation"), "got: {text}");
    }

    #[tokio::test]
    async fn dispatch_context_failure_returned_as_is_error() {
        let ctx = Arc::new(MockToolContext::new());
        ctx.fail_next_emit(ToolError::Context("db".into()));
        let server = build_server(ctx);
        let res = server
            .dispatch("send_message", args(serde_json::json!({"text": "hi"})))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("context"), "got: {text}");
    }

    #[tokio::test]
    async fn dispatch_internal_error_returned_as_is_error() {
        let ctx = Arc::new(MockToolContext::new());
        ctx.fail_next_emit(ToolError::Internal("bug".into()));
        let server = build_server(ctx);
        let res = server
            .dispatch("send_message", args(serde_json::json!({"text": "hi"})))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("internal"), "got: {text}");
    }

    #[test]
    fn server_info_advertises_tools_capability() {
        let ctx = Arc::new(MockToolContext::new());
        let server = build_server(ctx);
        let info = server.get_info();
        assert_eq!(info.server_info.name, "ironclaw-mcp");
        assert!(info.capabilities.tools.is_some());
    }
}
