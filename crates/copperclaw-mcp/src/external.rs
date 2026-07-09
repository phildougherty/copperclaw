//! Single-server connect + call primitive for **external** MCP servers.
//!
//! The host owns every external MCP connection (it holds the server
//! credentials and the per-server include/exclude filter), so both the
//! session-spawn manifest seam (`copperclaw-host`) and the host-proxied
//! tool-call executor (`copperclaw-host-delivery`) connect a configured
//! server the same way. That shared logic lives here so there is exactly one
//! transport-selection + filter-wrap implementation and the two callers can
//! never drift.
//!
//! "Connect per call" is deliberate: the host keeps no live in-memory
//! connection registry (it is stateless poll-from-DB by design), so
//! [`call_external_tool`] connects the one named server, calls the one tool
//! through its filter, and drops the connection. A short-lived connection
//! cache is a later optimization, not a correctness requirement.

use std::collections::HashMap;

use serde_json::Value;

use crate::client::{FilteredMcpClient, McpClient};
use crate::error::McpError;
use crate::filter::ToolFilter;

/// Connect one configured external MCP server entry and wrap it in its
/// per-server [`ToolFilter`]. A server with no declared filter gets a default
/// (open) filter, so the wrap is a transparent pass-through in that case.
///
/// The entry is one value from the group's `mcp_servers` object — it carries
/// the transport (`command`/`args`/`env` for stdio, or `url`/`headers` for
/// HTTP SSE) and the optional `allowed_tools` / `denied_tools` filter keys.
pub async fn connect_filtered(server_entry: &Value) -> Result<FilteredMcpClient, McpError> {
    let filter = ToolFilter::from_server_entry(server_entry);
    let client = connect_transport(server_entry).await?;
    Ok(FilteredMcpClient::new(client, filter))
}

/// Connect the given server entry, enforce its per-server filter, and call
/// `tool` with `input`. The connection is dropped when the call returns.
///
/// A filter-denied tool is refused with [`McpError::ToolFiltered`] before the
/// remote is ever contacted — the same gate that strips it from the advertised
/// manifest — so list and call can never disagree about what is permitted.
pub async fn call_external_tool(
    server_entry: &Value,
    tool: &str,
    input: Value,
) -> Result<Value, McpError> {
    let client = connect_filtered(server_entry).await?;
    client.call_tool(tool, input).await
}

/// Build a live [`McpClient`] from a server entry. Supports the two transports
/// [`McpClient`] implements: HTTP SSE (`url` + `headers`) and stdio
/// (`command` + `args` + `env`). HTTP SSE wins if both are present.
async fn connect_transport(entry: &Value) -> Result<McpClient, McpError> {
    if let Some(url) = entry.get("url").and_then(Value::as_str) {
        let headers = string_map(entry.get("headers"));
        return McpClient::connect_http_sse(url, headers).await;
    }
    if let Some(command) = entry.get("command").and_then(Value::as_str) {
        let args: Vec<String> = entry
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let env = string_map(entry.get("env"));
        return McpClient::connect_stdio(command, args, env).await;
    }
    Err(McpError::Protocol(
        "mcp_servers entry has neither a `command` (stdio) nor a `url` (http-sse) transport".into(),
    ))
}

/// Parse a JSON object of string→string (env / headers). Non-string values are
/// skipped; a non-object is treated as empty.
fn string_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn entry_without_transport_is_a_protocol_error() {
        let entry = serde_json::json!({"denied_tools": ["x"]});
        let err = connect_filtered(&entry).await.unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn stdio_to_missing_binary_is_a_transport_error() {
        let entry = serde_json::json!({
            "command": "/path/that/does/not/exist/copperclaw-mcp-extern-test",
            "args": ["--serve"],
        });
        let err = connect_filtered(&entry).await.unwrap_err();
        assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn call_external_tool_propagates_connect_failure() {
        // No live server: the connect fails before any tool call, surfaced to
        // the caller (the host executor renders this as an is_error response).
        let entry = serde_json::json!({"command": "/no/such/binary-xyz"});
        let err = call_external_tool(&entry, "anything", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
    }

    #[test]
    fn string_map_skips_non_strings_and_non_objects() {
        let v = serde_json::json!({"A": "1", "B": 2, "C": "3"});
        let m = string_map(Some(&v));
        assert_eq!(m.get("A").map(String::as_str), Some("1"));
        assert_eq!(m.get("C").map(String::as_str), Some("3"));
        assert!(!m.contains_key("B"));
        assert!(string_map(None).is_empty());
        assert!(string_map(Some(&serde_json::json!("not an object"))).is_empty());
    }
}
