//! Error types for the copperclaw MCP server, tool handlers, and MCP client.
//!
//! These types are deliberately small and stringly-typed so that handlers do
//! not need to leak provider/runner internals into the MCP wire surface.
//!
//! Mapping rules:
//! - `ToolError::Validation` is returned to the caller as an `is_error`
//!   `CallToolResult` with the human-readable reason (so the caller agent
//!   sees what went wrong and can retry).
//! - `ToolError::Context` is an internal context plumbing failure (the
//!   `ToolContext` impl failed); also surfaced as `is_error` with a generic
//!   message.
//! - `ToolError::Internal` is reserved for unexpected bugs and surfaces as
//!   `is_error` plus a `tracing::error!` log.

use thiserror::Error;

/// Failure modes for tool handlers.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ToolError {
    /// Caller-supplied input did not pass validation.
    #[error("validation error: {0}")]
    Validation(String),
    /// The `ToolContext` impl (runner, mock, etc.) failed to satisfy the
    /// requested side effect.
    #[error("context error: {0}")]
    Context(String),
    /// Internal / unexpected error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ToolError {
    /// Short tag for logging / metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Validation(_) => "validation",
            Self::Context(_) => "context",
            Self::Internal(_) => "internal",
        }
    }
}

/// Failure modes for `McpClient` calls into external MCP servers.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum McpError {
    /// Transport-level failure (process spawn, socket, stdio, etc.).
    #[error("transport error: {0}")]
    Transport(String),
    /// Protocol / framing error (bad JSON-RPC, schema mismatch).
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The remote MCP server returned an error response.
    #[error("remote error {code}: {message}")]
    RemoteError {
        /// JSON-RPC error code reported by the remote.
        code: i32,
        /// Human-readable error message.
        message: String,
    },
    /// The remote did not respond within the allowed window.
    #[error("timeout")]
    Timeout,
    /// The call was refused by the per-server tool include/exclude filter
    /// before it ever reached the remote — a denied (or not-allowed) tool.
    /// Carrying this as a distinct variant lets the runner surface a precise
    /// "blocked by policy" message instead of a generic protocol error.
    #[error("tool `{tool}` blocked by server filter: {reason}")]
    ToolFiltered {
        /// The tool name that was refused.
        tool: String,
        /// Stable reason token (`"denied"` / `"not-allowed"`).
        reason: &'static str,
    },
}

impl McpError {
    /// Short tag for logging / metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Transport(_) => "transport",
            Self::Protocol(_) => "protocol",
            Self::RemoteError { .. } => "remote",
            Self::Timeout => "timeout",
            Self::ToolFiltered { .. } => "filtered",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_error_kinds() {
        assert_eq!(ToolError::Validation("x".into()).kind(), "validation");
        assert_eq!(ToolError::Context("x".into()).kind(), "context");
        assert_eq!(ToolError::Internal("x".into()).kind(), "internal");
    }

    #[test]
    fn tool_error_display() {
        let e = ToolError::Validation("missing field".into());
        assert_eq!(e.to_string(), "validation error: missing field");
        let e = ToolError::Context("db locked".into());
        assert_eq!(e.to_string(), "context error: db locked");
        let e = ToolError::Internal("oops".into());
        assert_eq!(e.to_string(), "internal error: oops");
    }

    #[test]
    fn mcp_error_kinds() {
        assert_eq!(McpError::Transport("x".into()).kind(), "transport");
        assert_eq!(McpError::Protocol("x".into()).kind(), "protocol");
        assert_eq!(
            McpError::RemoteError {
                code: -32601,
                message: "method not found".into()
            }
            .kind(),
            "remote"
        );
        assert_eq!(McpError::Timeout.kind(), "timeout");
    }

    #[test]
    fn mcp_error_display() {
        let e = McpError::Transport("pipe closed".into());
        assert_eq!(e.to_string(), "transport error: pipe closed");
        let e = McpError::Protocol("bad json".into());
        assert_eq!(e.to_string(), "protocol error: bad json");
        let e = McpError::RemoteError {
            code: -32000,
            message: "boom".into(),
        };
        assert_eq!(e.to_string(), "remote error -32000: boom");
        let e = McpError::Timeout;
        assert_eq!(e.to_string(), "timeout");
    }

    #[test]
    fn errors_are_clonable_and_eq() {
        let a = ToolError::Validation("v".into());
        let b = a.clone();
        assert_eq!(a, b);

        let a = McpError::RemoteError {
            code: 1,
            message: "m".into(),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
