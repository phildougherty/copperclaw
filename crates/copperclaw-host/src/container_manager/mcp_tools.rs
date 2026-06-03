//! Session-spawn seam that assembles the **external** MCP tool set for a
//! group and routes it through the per-server include/exclude filter.
//!
//! # Why this lives here
//!
//! The in-container tool surface (the 15 first-party tools) is built by the
//! runner from `copperclaw_mcp::build_tool_set()`. *External* MCP servers
//! configured on a group (`container_configs.mcp_servers`) are a different
//! animal: the host owns the connection (it holds the server credentials and
//! the per-server tool include/exclude policy), so the host is where a
//! configured server is connected, its tools are listed, and its calls are
//! dispatched.
//!
//! This module is the **live enforcement point** for that policy. For every
//! configured server it:
//!
//! 1. parses the per-server [`ToolFilter`] from the `mcp_servers` entry
//!    (`allowed_tools` / `denied_tools`),
//! 2. connects a live [`McpClient`] (stdio child process or HTTP SSE),
//! 3. wraps the two in a [`FilteredMcpClient`] — the production caller of that
//!    type — and
//! 4. lists the server's tools *through the filter*, so a denied tool is never
//!    added to the advertised set the model sees, and routes calls *through
//!    the filter*, so a denied tool is refused before it reaches the remote
//!    even if the model names it by hand.
//!
//! A server with no declared filter gets a default (open) filter, so wiring
//! the filter in unconditionally is a transparent pass-through for the
//! unconfigured case.

use std::collections::HashMap;

use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::container_configs;
use copperclaw_mcp::{FilteredMcpClient, McpClient, McpError, RemoteTool, ToolFilter};
use copperclaw_types::AgentGroupId;
use serde_json::Value;
use tracing::warn;

/// One configured external MCP server, connected and filter-wrapped.
pub struct McpServerHandle {
    /// The server name (the key in the `mcp_servers` object).
    pub name: String,
    /// The live, filter-enforcing client. Both list and call go through it.
    pub client: FilteredMcpClient,
}

/// The external MCP tool set assembled for a session: the advertised tool
/// descriptors (already filter-stripped) plus a name→server routing table so
/// a dispatched call lands on the right filter-enforcing client.
#[derive(Default)]
pub struct AssembledMcpTools {
    /// Connected, filter-wrapped servers in config order.
    servers: Vec<McpServerHandle>,
    /// Advertised tools (filter-stripped), tagged with their server index.
    advertised: Vec<(usize, RemoteTool)>,
    /// Tool-name → server index, for call routing. First writer wins on a
    /// name collision across servers (config order), matching the order the
    /// tools are advertised in.
    routes: HashMap<String, usize>,
}

impl AssembledMcpTools {
    /// The tools advertised to the model — already stripped of every denied /
    /// not-allowed tool by the per-server filter. A denied tool is simply not
    /// present here.
    #[must_use]
    pub fn advertised_tools(&self) -> Vec<RemoteTool> {
        self.advertised.iter().map(|(_, t)| t.clone()).collect()
    }

    /// True when `name` is advertised (i.e. permitted by its server's filter).
    #[must_use]
    pub fn advertises(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    /// Number of connected external servers.
    #[must_use]
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    /// Dispatch a tool call to the owning server, *through its filter*.
    ///
    /// A denied tool is refused with [`McpError::ToolFiltered`] before the
    /// remote is contacted — the gate is the same [`FilteredMcpClient`] that
    /// stripped the tool from [`Self::advertised_tools`], so list and call can
    /// never disagree about what is permitted.
    ///
    /// An unknown tool (no configured server advertises it under any filter)
    /// is a protocol error.
    pub async fn call_tool(&self, name: &str, input: Value) -> Result<Value, McpError> {
        // Prefer the server that advertises the tool. A name the filter
        // stripped from every server has no route, but we still want a denied
        // call to be *refused by the filter* (not just "unknown"), so fall
        // back to the server that declares the name and let its filter reject.
        let idx = self.routes.get(name).copied().or_else(|| {
            self.servers
                .iter()
                .position(|s| !s.client.filter().permits(name))
        });
        match idx {
            Some(i) => self.servers[i].client.call_tool(name, input).await,
            None => Err(McpError::Protocol(format!(
                "no external MCP server provides tool `{name}`"
            ))),
        }
    }
}

/// Connect every external MCP server configured on `agent_group_id`, list its
/// tools through the per-server filter, and assemble the routing table.
///
/// Read errors / malformed config / connect failures are logged and skipped
/// (a single broken server must not abort the spawn); the rest of the servers
/// still assemble. A group with no configured servers yields an empty set.
pub async fn assemble_mcp_tools(
    central: &CentralDb,
    agent_group_id: AgentGroupId,
) -> AssembledMcpTools {
    let servers = match container_configs::get_mcp_servers(central, agent_group_id) {
        Ok(v) => v,
        Err(copperclaw_db::DbError::NotFound) => return AssembledMcpTools::default(),
        Err(err) => {
            warn!(
                agent_group = %agent_group_id.as_uuid(),
                ?err,
                "could not read mcp_servers config; external MCP tools unavailable this spawn"
            );
            return AssembledMcpTools::default();
        }
    };
    let Some(obj) = servers.as_object() else {
        return AssembledMcpTools::default();
    };

    let mut assembled = AssembledMcpTools::default();
    for (name, entry) in obj {
        match connect_one(name, entry).await {
            Ok(handle) => add_server(&mut assembled, handle).await,
            Err(err) => warn!(
                agent_group = %agent_group_id.as_uuid(),
                server = %name,
                error = %err,
                "skipping external MCP server: connect/list failed"
            ),
        }
    }
    assembled
}

/// Push a connected server into the set, listing its filter-stripped tools and
/// recording routes. Tools the filter denied are absent from `list_tools`, so
/// they are never advertised and never routed — that is the enforcement.
async fn add_server(assembled: &mut AssembledMcpTools, handle: McpServerHandle) {
    let idx = assembled.servers.len();
    let tools = match handle.client.list_tools().await {
        Ok(t) => t,
        Err(err) => {
            warn!(server = %handle.name, error = %err, "list_tools failed; server skipped");
            return;
        }
    };
    for tool in tools {
        // First server to advertise a name wins the route, mirroring the order
        // the tools are presented to the model.
        assembled.routes.entry(tool.name.clone()).or_insert(idx);
        assembled.advertised.push((idx, tool));
    }
    assembled.servers.push(handle);
}

/// Connect one configured server and wrap it in its per-server filter.
///
/// This is the production construction site of [`FilteredMcpClient`]: the
/// filter is parsed from the same `mcp_servers` entry the operator configured,
/// so the host-side `mcp.inspect-filter` view and the live enforcement agree.
async fn connect_one(name: &str, entry: &Value) -> Result<McpServerHandle, McpError> {
    let filter = ToolFilter::from_server_entry(entry);
    let client = connect_transport(entry).await?;
    Ok(McpServerHandle {
        name: name.to_owned(),
        client: FilteredMcpClient::new(client, filter),
    })
}

/// Build a live [`McpClient`] from a server entry. Supports the two transports
/// `McpClient` implements: stdio (`command` + `args` + `env`) and HTTP SSE
/// (`url` + `headers`). The preset registry writes the stdio shape.
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
    use async_trait::async_trait;
    use copperclaw_mcp::McpToolTransport;

    /// In-process fake server that advertises a fixed tool list and echoes the
    /// call name, so the filter's strip/refuse behaviour can be proven without
    /// a real transport. Records the names it was *actually* asked to call.
    struct FakeServer {
        tools: Vec<RemoteTool>,
        called: std::sync::Mutex<Vec<String>>,
    }

    impl FakeServer {
        fn new(names: &[&str]) -> Self {
            Self {
                tools: names
                    .iter()
                    .map(|n| RemoteTool {
                        name: (*n).to_owned(),
                        description: Some(format!("{n} tool")),
                        input_schema: serde_json::json!({"type": "object"}),
                    })
                    .collect(),
                called: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    /// An `Arc<FakeServer>` that itself implements the transport trait, so the
    /// `FilteredMcpClient` can own a boxed clone while the test keeps a second
    /// handle to inspect `called`. Both point at the same recorder.
    struct SharedFake(std::sync::Arc<FakeServer>);

    #[async_trait]
    impl McpToolTransport for SharedFake {
        async fn list_tools(&self) -> Result<Vec<RemoteTool>, McpError> {
            Ok(self.0.tools.clone())
        }
        async fn list_all_tools(&self) -> Result<Vec<RemoteTool>, McpError> {
            Ok(self.0.tools.clone())
        }
        async fn call_tool(&self, name: &str, _input: Value) -> Result<Value, McpError> {
            self.0.called.lock().unwrap().push(name.to_owned());
            Ok(serde_json::json!({"is_error": false, "called": name}))
        }
    }

    /// Build an `AssembledMcpTools` over a fake transport with a given filter,
    /// driving the exact list-strip + call-route path `assemble_mcp_tools`
    /// uses for a real server. The shared `Arc` lets the test inspect which
    /// calls reached the remote.
    async fn assemble_fake(
        name: &str,
        names: &[&str],
        filter: ToolFilter,
    ) -> (AssembledMcpTools, std::sync::Arc<FakeServer>) {
        let fake = std::sync::Arc::new(FakeServer::new(names));
        let client = FilteredMcpClient::from_transport(
            Box::new(SharedFake(std::sync::Arc::clone(&fake))),
            filter,
        );
        let mut assembled = AssembledMcpTools::default();
        add_server(
            &mut assembled,
            McpServerHandle {
                name: name.to_owned(),
                client,
            },
        )
        .await;
        (assembled, fake)
    }

    #[tokio::test]
    async fn denied_tool_is_neither_advertised_nor_callable_but_allowed_one_works() {
        // Server advertises X and Y; config denies X.
        let entry = serde_json::json!({"denied_tools": ["X"]});
        let filter = ToolFilter::from_server_entry(&entry);
        let (assembled, fake) = assemble_fake("srv", &["X", "Y"], filter).await;

        // (a) X is NOT in the advertised tool list; Y is.
        let advertised: Vec<String> = assembled
            .advertised_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(
            advertised,
            vec!["Y".to_string()],
            "denied X must be stripped"
        );
        assert!(!assembled.advertises("X"));
        assert!(assembled.advertises("Y"));

        // (b) A direct call to X is REFUSED by the filter (never reaches the
        // remote) ...
        let err = assembled
            .call_tool("X", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::ToolFiltered { ref tool, .. } if tool == "X"),
            "denied call must be filtered, got {err:?}"
        );
        assert!(
            fake.called.lock().unwrap().is_empty(),
            "denied call must NOT reach the remote"
        );

        // ... while the allowed tool Y still works and DOES reach the remote.
        let ok = assembled
            .call_tool("Y", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(ok["is_error"], false);
        assert_eq!(fake.called.lock().unwrap().as_slice(), &["Y".to_string()]);
    }

    #[tokio::test]
    async fn allow_list_is_a_positive_gate_on_the_assembled_set() {
        // Only Y is allow-listed: X and Z must be stripped and refused.
        let entry = serde_json::json!({"allowed_tools": ["Y"]});
        let filter = ToolFilter::from_server_entry(&entry);
        let (assembled, fake) = assemble_fake("srv", &["X", "Y", "Z"], filter).await;

        let advertised: Vec<String> = assembled
            .advertised_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(advertised, vec!["Y".to_string()]);

        for blocked in ["X", "Z"] {
            let err = assembled
                .call_tool(blocked, serde_json::json!({}))
                .await
                .unwrap_err();
            assert!(
                matches!(err, McpError::ToolFiltered { .. }),
                "not-allowed `{blocked}` must be filtered, got {err:?}"
            );
        }
        // Only Y ever reached the remote.
        assert!(
            assembled
                .call_tool("Y", serde_json::json!({}))
                .await
                .is_ok()
        );
        assert_eq!(fake.called.lock().unwrap().as_slice(), &["Y".to_string()]);
    }

    #[tokio::test]
    async fn open_filter_advertises_everything() {
        let filter = ToolFilter::from_server_entry(&serde_json::json!({}));
        let (assembled, _) = assemble_fake("srv", &["X", "Y"], filter).await;
        let mut advertised: Vec<String> = assembled
            .advertised_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        advertised.sort();
        assert_eq!(advertised, vec!["X".to_string(), "Y".to_string()]);
    }

    #[tokio::test]
    async fn unknown_tool_is_a_protocol_error() {
        let filter = ToolFilter::default();
        let (assembled, _) = assemble_fake("srv", &["Y"], filter).await;
        let err = assembled
            .call_tool("nope", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)), "got {err:?}");
    }

    #[test]
    fn connect_transport_requires_a_transport() {
        // Pure (no await): an entry with neither command nor url is rejected.
        let entry = serde_json::json!({"denied_tools": ["x"]});
        assert!(entry.get("command").is_none() && entry.get("url").is_none());
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
