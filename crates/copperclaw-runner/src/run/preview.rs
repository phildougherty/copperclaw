//! Runner side of the M17 session-preview tools (`expose_preview` /
//! `close_preview`).
//!
//! These are first-party tools the model can call, but they are serviced
//! host-side (the host owns the container's bridge IP, the port range, and the
//! axum reverse proxy). Rather than a bespoke request/response channel, they
//! reuse the **external-MCP host-broker relay**: the runner writes a request
//! row to `outbound.db::mcp_call_requests` under the reserved server name
//! [`PREVIEW_SERVER`] and block-polls `inbound.db::mcp_call_responses` — exactly
//! [`super::external_mcp::dispatch_external`]. The delivery loop's
//! `drain_mcp_calls` special-cases the `__preview` server and routes it to the
//! host-side preview broker.
//!
//! This module supplies the two pieces the dispatcher needs: the advertised
//! [`ToolDef`]s (so the model sees the tools) and the synthetic
//! [`ExternalToolRoute`] that carries the reserved server name + the tool verb
//! through `dispatch_external`.

use copperclaw_providers::ToolDef;

use super::external_mcp::ExternalToolRoute;

/// Reserved MCP "server" name the preview tools relay through. Must match the
/// delivery-side `PREVIEW_SERVER` const (`copperclaw-host-delivery`).
pub const PREVIEW_SERVER: &str = "__preview";

/// The `expose_preview` tool name.
pub const EXPOSE_PREVIEW: &str = "expose_preview";
/// The `close_preview` tool name.
pub const CLOSE_PREVIEW: &str = "close_preview";

/// Whether `name` is one of the reserved preview tools (routed via the
/// `__preview` relay rather than the in-container tool map).
#[must_use]
pub fn is_preview_tool(name: &str) -> bool {
    name == EXPOSE_PREVIEW || name == CLOSE_PREVIEW
}

/// Build the synthetic route that carries a preview tool call through
/// [`super::external_mcp::dispatch_external`]: the reserved `__preview` server
/// plus the verb (`expose_preview` / `close_preview`) the host dispatches on.
#[must_use]
pub fn preview_route(name: &str) -> ExternalToolRoute {
    ExternalToolRoute {
        server: PREVIEW_SERVER.to_string(),
        tool: name.to_string(),
    }
}

/// The two `ToolDef`s advertised to the provider. Always advertised (like the
/// external MCP tools); the runner's policy gate denies them under a
/// minimal/messaging profile or to a guest sender, and the host answers with a
/// clear `is_error` when the group has not opted preview in.
#[must_use]
pub fn preview_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: EXPOSE_PREVIEW.to_string(),
            description: "Expose an HTTP server you started INSIDE this container to the \
                 operator's machine / LAN so they can open it in a browser and test it. The \
                 server MUST be listening on 0.0.0.0:<port> inside the container (a server bound \
                 to 127.0.0.1 inside the container is NOT reachable by the proxy). Returns a \
                 shareable tokened URL — send that URL to the operator verbatim. Use \
                 `close_preview` when done. If it errors that preview is not enabled, relay the \
                 operator command it gives you."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "port": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 65535,
                        "description": "The port your server listens on inside the container (0.0.0.0:<port>)."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional short label for this preview (e.g. \"todo app\")."
                    }
                },
                "required": ["port"]
            }),
        },
        ToolDef {
            name: CLOSE_PREVIEW.to_string(),
            description: "Tear down a preview you previously exposed with `expose_preview`, by \
                 the same container port. Call this when the operator is done testing."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "port": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 65535,
                        "description": "The container port whose preview to close."
                    }
                },
                "required": ["port"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_preview_tools() {
        assert!(is_preview_tool("expose_preview"));
        assert!(is_preview_tool("close_preview"));
        assert!(!is_preview_tool("write_file"));
        assert!(!is_preview_tool("mcp__weather__forecast"));
    }

    #[test]
    fn route_carries_reserved_server_and_verb() {
        let r = preview_route("expose_preview");
        assert_eq!(r.server, "__preview");
        assert_eq!(r.tool, "expose_preview");
    }

    #[test]
    fn tool_defs_advertise_both_with_required_port() {
        let defs = preview_tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["expose_preview", "close_preview"]);
        for d in &defs {
            let required = &d.input_schema["required"];
            assert!(
                required.as_array().unwrap().iter().any(|v| v == "port"),
                "{} must require port",
                d.name
            );
        }
    }
}
