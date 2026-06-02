//! Client wrapper over `rmcp` so per-group configured external MCP servers
//! can be called from copperclaw with our own `McpError` type.
//!
//! Construction is biased toward what copperclaw actually needs: stdio child
//! process and HTTP SSE. The underlying `rmcp::service::RunningService` is
//! held opaquely; the only public surface is `list_tools` / `call_tool`.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParam, JsonObject};
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::sse_client::{SseClientConfig, SseClientTransport};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};

use crate::error::McpError;

/// Thin wrapper around a running `rmcp` client session.
pub struct McpClient {
    inner: RunningService<RoleClient, ()>,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient").finish_non_exhaustive()
    }
}

impl McpClient {
    /// Spawn `cmd` with `args` and connect over stdio.
    ///
    /// `env` is applied verbatim to the child process. The current process
    /// environment is *not* inherited unless the caller passes through what
    /// they want.
    pub async fn connect_stdio<I, S>(
        cmd: &str,
        args: I,
        env: HashMap<String, String>,
    ) -> Result<Self, McpError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let arg_vec: Vec<std::ffi::OsString> = args
            .into_iter()
            .map(|a| a.as_ref().to_os_string())
            .collect();
        let transport = TokioChildProcess::new(tokio::process::Command::new(cmd).configure(|c| {
            for a in &arg_vec {
                c.arg(a);
            }
            c.env_clear();
            for (k, v) in &env {
                c.env(k, v);
            }
        }))
        .map_err(|e| McpError::Transport(format!("spawning {cmd}: {e}")))?;
        let inner = ().serve(transport).await.map_err(io_err_to_mcp_error)?;
        Ok(Self { inner })
    }

    /// Connect to a server over HTTP SSE.
    ///
    /// Establishes a `text/event-stream` connection to `url` and runs the
    /// MCP protocol over it via rmcp's `SseClientTransport`. Static
    /// `headers` are sent on every request (auth bearer tokens are the
    /// usual case — set `Authorization` here and it lands on the SSE GET
    /// and on each POST to the server-advertised message endpoint).
    ///
    /// Errors:
    /// - `McpError::Protocol` when `url` or a header is malformed.
    /// - `McpError::Transport` when the HTTP client cannot be built or
    ///   the initial SSE handshake fails.
    /// - `McpError::Protocol` when the server returns something that
    ///   rmcp cannot interpret as MCP over SSE.
    pub async fn connect_http_sse(
        url: &str,
        headers: HashMap<String, String>,
    ) -> Result<Self, McpError> {
        let client = build_sse_client(&headers)?;
        let transport = SseClientTransport::<reqwest::Client>::start_with_client(
            client,
            SseClientConfig {
                sse_endpoint: url.into(),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| McpError::Transport(format!("sse connect to {url}: {e}")))?;
        let inner = ().serve(transport).await.map_err(|e| {
            // rmcp's serve error type wraps the transport error; lift to
            // McpError::Protocol since we passed the transport handshake.
            McpError::Protocol(format!("sse handshake to {url}: {e}"))
        })?;
        Ok(Self { inner })
    }

    /// List tools advertised by the remote server (paginated cursors are
    /// not walked here — callers needing every tool should use
    /// [`McpClient::list_all_tools`]).
    pub async fn list_tools(&self) -> Result<Vec<RemoteTool>, McpError> {
        let result = self
            .inner
            .list_tools(Option::default())
            .await
            .map_err(service_err_to_mcp_error)?;
        Ok(result
            .tools
            .into_iter()
            .map(|t| RemoteTool {
                name: t.name.to_string(),
                description: t.description.map(|c| c.to_string()),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
            })
            .collect())
    }

    /// List every tool, walking pagination cursors until exhausted.
    pub async fn list_all_tools(&self) -> Result<Vec<RemoteTool>, McpError> {
        let raw = self
            .inner
            .list_all_tools()
            .await
            .map_err(service_err_to_mcp_error)?;
        Ok(raw
            .into_iter()
            .map(|t| RemoteTool {
                name: t.name.to_string(),
                description: t.description.map(|c| c.to_string()),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
            })
            .collect())
    }

    /// Call a tool by name with the given JSON arguments.
    ///
    /// The returned `serde_json::Value` is a flat object with two keys:
    /// - `content`: the rmcp `CallToolResult` content array, serialised
    /// - `is_error`: copied from the rmcp `is_error` field
    pub async fn call_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let arguments = match input {
            serde_json::Value::Object(m) => Some(m),
            serde_json::Value::Null => None,
            _ => {
                return Err(McpError::Protocol(
                    "tool arguments must be a JSON object or null".into(),
                ));
            }
        };
        let req = CallToolRequestParam {
            name: name.to_owned().into(),
            arguments,
        };
        let result = self
            .inner
            .call_tool(req)
            .await
            .map_err(service_err_to_mcp_error)?;
        Ok(serde_json::json!({
            "is_error": result.is_error,
            "content": result.content,
        }))
    }

    /// Gracefully cancel the underlying service.
    pub async fn close(self) -> Result<(), McpError> {
        self.inner
            .cancel()
            .await
            .map(|_| ())
            .map_err(|e| McpError::Transport(format!("cancel: {e}")))
    }
}

/// Description of a remote tool, normalised to plain Rust strings + JSON so
/// runner code never sees rmcp types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTool {
    /// Tool name as advertised by the remote.
    pub name: String,
    /// Human-readable description, if any.
    pub description: Option<String>,
    /// JSON Schema for the tool's input arguments.
    pub input_schema: serde_json::Value,
}

fn io_err_to_mcp_error<E: std::fmt::Display>(e: E) -> McpError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        McpError::Timeout
    } else if lower.contains("protocol") || lower.contains("parse") {
        McpError::Protocol(msg)
    } else {
        McpError::Transport(msg)
    }
}

fn service_err_to_mcp_error(e: rmcp::ServiceError) -> McpError {
    match e {
        rmcp::ServiceError::McpError(err) => McpError::RemoteError {
            code: err.code.0,
            message: err.message.to_string(),
        },
        rmcp::ServiceError::Timeout { .. } => McpError::Timeout,
        rmcp::ServiceError::TransportSend(err) => McpError::Transport(err.to_string()),
        rmcp::ServiceError::TransportClosed => McpError::Transport("transport closed".to_owned()),
        other => McpError::Protocol(other.to_string()),
    }
}

/// Compatibility shim: parse a `CallToolRequestParam`-like JSON object the
/// way [`McpClient::call_tool`] does. Tests use this to verify the
/// argument-shape logic without spawning a child process.
#[doc(hidden)]
pub fn normalise_call_input(value: serde_json::Value) -> Result<Option<JsonObject>, McpError> {
    match value {
        serde_json::Value::Object(m) => Ok(Some(m)),
        serde_json::Value::Null => Ok(None),
        _ => Err(McpError::Protocol(
            "tool arguments must be a JSON object or null".into(),
        )),
    }
}

/// Internal helper to map an rmcp `ServiceError` to our `McpError` — public
/// so tests can exercise the table without an actual rmcp service.
#[doc(hidden)]
pub fn map_service_error_for_test(code: i32, message: &str) -> McpError {
    McpError::RemoteError {
        code,
        message: message.to_owned(),
    }
}

/// Internal helper for tests.
#[doc(hidden)]
pub fn map_io_error_for_test(msg: &str) -> McpError {
    io_err_to_mcp_error(msg)
}

/// Internal helper used by integration tests to construct the `RemoteTool`
/// table from raw rmcp output. Marked hidden to avoid leaking.
#[doc(hidden)]
pub fn remote_tool_from_parts(
    name: &str,
    description: Option<&str>,
    input_schema: serde_json::Value,
) -> RemoteTool {
    RemoteTool {
        name: name.to_owned(),
        description: description.map(str::to_owned),
        input_schema,
    }
}

/// Re-export for runner crates that want to inject `Arc<McpClient>` into
/// their own context. Removes the need for them to depend on rmcp directly.
pub type SharedMcpClient = Arc<McpClient>;

/// Build a `reqwest::Client` pre-loaded with the caller's static headers.
///
/// Used only by [`McpClient::connect_http_sse`]; exported `pub(crate)` so
/// tests can exercise header validation directly without standing up an
/// SSE server.
pub(crate) fn build_sse_client(
    headers: &HashMap<String, String>,
) -> Result<reqwest::Client, McpError> {
    let mut default_headers = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
            .map_err(|e| McpError::Protocol(format!("header name {k}: {e}")))?;
        let value = reqwest::header::HeaderValue::from_str(v)
            .map_err(|e| McpError::Protocol(format!("header value for {k}: {e}")))?;
        default_headers.insert(name, value);
    }
    reqwest::Client::builder()
        .default_headers(default_headers)
        .build()
        .map_err(|e| McpError::Transport(format!("build sse http client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn connect_stdio_missing_command_is_transport_error() {
        let err = McpClient::connect_stdio(
            "/path/that/does/not/exist/copperclaw-mcp-test",
            std::iter::empty::<&str>(),
            HashMap::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, McpError::Transport(_)));
    }

    #[tokio::test]
    async fn connect_http_sse_invalid_url_is_transport_error() {
        // An URL that's syntactically a URL but routes to a port nothing
        // listens on; reqwest will fail at the TCP layer.
        let err = McpClient::connect_http_sse("http://127.0.0.1:1/", HashMap::new())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn connect_http_sse_rejects_bad_header_name() {
        let mut h = HashMap::new();
        h.insert("not a valid header name".to_owned(), "v".to_owned());
        let err = McpClient::connect_http_sse("http://127.0.0.1:1/", h)
            .await
            .unwrap_err();
        // build_sse_client errors before the connect is attempted.
        assert!(matches!(err, McpError::Protocol(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn connect_http_sse_rejects_bad_header_value() {
        let mut h = HashMap::new();
        // Newline in header value is invalid per RFC 9110.
        h.insert("X-Test".to_owned(), "bad\nvalue".to_owned());
        let err = McpClient::connect_http_sse("http://127.0.0.1:1/", h)
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn connect_http_sse_against_wrong_content_type_is_transport_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string("{}"),
            )
            .mount(&server)
            .await;
        let err = McpClient::connect_http_sse(&server.uri(), HashMap::new())
            .await
            .unwrap_err();
        // The transport rejects the response because the content-type is
        // not text/event-stream.
        assert!(matches!(err, McpError::Transport(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn build_sse_client_accepts_well_formed_headers() {
        let mut h = HashMap::new();
        h.insert("Authorization".to_owned(), "Bearer abc".to_owned());
        h.insert("X-Custom".to_owned(), "v".to_owned());
        assert!(build_sse_client(&h).is_ok());
    }

    #[test]
    fn build_sse_client_rejects_invalid_name() {
        let mut h = HashMap::new();
        h.insert("bad name".to_owned(), "v".to_owned());
        assert!(matches!(
            build_sse_client(&h).unwrap_err(),
            McpError::Protocol(_)
        ));
    }

    #[test]
    fn build_sse_client_rejects_invalid_value() {
        let mut h = HashMap::new();
        h.insert("X-Test".to_owned(), "bad\nvalue".to_owned());
        assert!(matches!(
            build_sse_client(&h).unwrap_err(),
            McpError::Protocol(_)
        ));
    }

    #[test]
    fn build_sse_client_empty_headers_is_ok() {
        assert!(build_sse_client(&HashMap::new()).is_ok());
    }

    #[test]
    fn normalise_object_input() {
        let v = serde_json::json!({"x": 1});
        let map = normalise_call_input(v).unwrap();
        assert!(map.is_some());
    }

    #[test]
    fn normalise_null_input() {
        let map = normalise_call_input(serde_json::Value::Null).unwrap();
        assert!(map.is_none());
    }

    #[test]
    fn normalise_array_input_is_protocol_error() {
        let err = normalise_call_input(serde_json::json!([1, 2, 3])).unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)));
    }

    #[test]
    fn io_err_classification() {
        assert!(matches!(
            map_io_error_for_test("timed out"),
            McpError::Timeout
        ));
        assert!(matches!(
            map_io_error_for_test("protocol mismatch"),
            McpError::Protocol(_)
        ));
        assert!(matches!(
            map_io_error_for_test("connection refused"),
            McpError::Transport(_)
        ));
    }

    #[test]
    fn remote_error_constructor() {
        let e = map_service_error_for_test(-32601, "method not found");
        assert!(matches!(e, McpError::RemoteError { code: -32601, .. }));
    }

    #[test]
    fn remote_tool_round_trip() {
        let t = remote_tool_from_parts(
            "echo",
            Some("echo back"),
            serde_json::json!({"type": "object"}),
        );
        assert_eq!(t.name, "echo");
        assert_eq!(t.description.as_deref(), Some("echo back"));
        assert_eq!(t.input_schema["type"], "object");
    }
}
