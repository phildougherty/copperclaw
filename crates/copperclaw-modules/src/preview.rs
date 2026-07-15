//! Session-preview proxy broker interface (M17).
//!
//! A session container runs on the Docker bridge with no published ports, but
//! the host can reach the container's bridge IP directly. The preview feature
//! lets an in-container agent expose an HTTP app it built (listening on
//! `0.0.0.0:<port>` INSIDE the container) to the operator's machine / LAN for
//! hands-on testing: the agent calls the first-party `expose_preview` tool, the
//! host validates policy and stands up a token-gated reverse proxy from a host
//! port to `container_ip:port`, and returns a shareable URL. `close_preview`
//! tears it down.
//!
//! The tool calls ride the **external-MCP host-broker relay**: the runner
//! writes a request row to `outbound.db::mcp_call_requests` with the reserved
//! server name `__preview`, and the delivery loop's `drain_mcp_calls` routes
//! `__preview` requests to an injected [`PreviewBroker`] instead of an external
//! MCP server. Keeping the broker behind a trait in `copperclaw-modules` lets
//! the delivery crate (which already depends on `copperclaw-modules`) call it
//! without depending on the host crate where the concrete manager + the axum
//! proxy live.
//!
//! `None` broker (the trait not wired — e.g. a host with no container runtime,
//! or the unit tests) is a first-class state: the delivery branch answers such
//! a `__preview` request with a clear `is_error` rather than hanging the runner's
//! blocking poll.

use async_trait::async_trait;
use copperclaw_types::{AgentGroupId, SessionId};
use thiserror::Error;

/// The minimal session identity the broker needs to resolve a container and
/// its group policy. Derived by the delivery loop from the live delivery
/// `Session` (`copperclaw_types::Session`) — a lightweight, dependency-free
/// hand-off so the broker trait doesn't pull the full session type through
/// every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInfoLite {
    /// The session whose container hosts the app being exposed. The container
    /// name follows the `copperclaw-<session-uuid>` convention.
    pub session_id: SessionId,
    /// The session's agent group — the preview enable/bind policy is keyed on
    /// this in `container_configs`.
    pub agent_group_id: AgentGroupId,
}

impl SessionInfoLite {
    /// Construct from the raw ids.
    #[must_use]
    pub fn new(session_id: SessionId, agent_group_id: AgentGroupId) -> Self {
        Self {
            session_id,
            agent_group_id,
        }
    }
}

/// A live preview the broker stood up. Carries the pieces the runner renders
/// back to the model (and the model relays verbatim to the operator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewExposed {
    /// The shareable tokened URL, e.g.
    /// `http://192.168.1.20:8100/__preview/<token>`. Opening it sets the
    /// gating cookie and redirects to the app root.
    pub url: String,
    /// A short operator-facing note the agent relays alongside the URL (idle
    /// lifetime + the "anyone on your network with the link" caveat).
    pub note: String,
}

/// Why a preview `expose` / `close` could not be satisfied. Every variant
/// renders (via `Display`) into the `is_error` `tool_result` text the runner
/// surfaces to the model — so the messages are written for the agent to read
/// and, where an operator action is required, to relay.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PreviewError {
    /// The group has not opted into previews. The message is copy-pasteable:
    /// it names the exact `cclaw` command the operator must run.
    #[error(
        "Preview is not enabled for this agent group. Ask the operator to run:\n  \
         cclaw groups config update --field preview_enabled=true {group}\n  \
         cclaw groups restart {group}\n\
         then try `expose_preview` again."
    )]
    Disabled { group: String },

    /// A bad argument from the model (port out of range, etc.).
    #[error("Invalid preview request: {0}")]
    BadRequest(String),

    /// The session's container has no reachable bridge IP (not running, or on a
    /// network-isolated egress-denied sandbox with no bridge attachment).
    #[error(
        "This session's container has no reachable network address, so its \
         server can't be exposed. Make sure the container is running and listening \
         on 0.0.0.0:<port> (127.0.0.1 inside the container is NOT reachable)."
    )]
    NoContainerIp,

    /// A per-session or per-host concurrency cap was hit.
    #[error("Preview capacity reached: {0}")]
    CapacityExceeded(String),

    /// No free host port could be bound in the preview range.
    #[error("No free host port available to serve the preview: {0}")]
    NoFreePort(String),

    /// There is no active preview on `port` for this session to close.
    #[error("No active preview on port {port} for this session.")]
    NotFound { port: u16 },

    /// A host-side failure standing up or tearing down the proxy.
    #[error("Preview proxy error: {0}")]
    Internal(String),
}

/// Host-side broker the delivery loop calls to service `__preview` relay
/// requests. Implemented by the host's preview manager.
#[async_trait]
pub trait PreviewBroker: Send + Sync {
    /// Stand up a token-gated reverse proxy from a host port to the session
    /// container's `port`, returning the shareable URL. `name` is an optional
    /// operator-facing label for the preview.
    async fn expose(
        &self,
        session: &SessionInfoLite,
        port: u16,
        name: Option<String>,
    ) -> Result<PreviewExposed, PreviewError>;

    /// Tear down the preview the session opened on `port`. Idempotent from the
    /// caller's view except that closing a port with no active preview returns
    /// [`PreviewError::NotFound`] so the agent learns it had nothing to close.
    async fn close(&self, session_id: SessionId, port: u16) -> Result<(), PreviewError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_error_names_the_operator_command() {
        let e = PreviewError::Disabled {
            group: "ag-123".into(),
        };
        let msg = e.to_string();
        assert!(msg.contains("preview_enabled=true ag-123"));
        assert!(msg.contains("cclaw groups restart ag-123"));
    }

    #[test]
    fn not_found_names_the_port() {
        let e = PreviewError::NotFound { port: 3000 };
        assert!(e.to_string().contains("3000"));
    }

    #[test]
    fn session_info_lite_is_copy() {
        let s = SessionInfoLite::new(SessionId::new(), AgentGroupId::new());
        let s2 = s; // Copy
        assert_eq!(s, s2);
    }
}
