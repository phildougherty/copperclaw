//! Resolves outbound destinations of the form `agent:<name>` and handles the
//! `create_agent` system-action.
//!
//! Two responsibilities live in this module:
//!
//! 1. **Destination parsing** — when an agent calls
//!    `send_message(to: "agent:helper")` the runner serializes the destination
//!    string verbatim. The host's delivery loop calls into this module's
//!    [`parse`] / [`is_agent_destination`] helpers to decide whether to route
//!    through a channel adapter or fan the message into another agent's
//!    `messages_in`.
//!
//! 2. **`create_agent` delivery action** — when an agent calls the
//!    `create_agent` MCP tool, the runner writes a `kind=system` outbound row
//!    with content `{"create_agent": {"name": "...", "instructions": "...",
//!    "channel": "..."}}`. The host's delivery loop parses the action name
//!    and dispatches to [`CreateAgentHandler::handle`], which:
//!
//!    a. Permission-gates via the configured closure (defaults to deny so
//!       production wiring must opt in).
//!    b. Refuses if accepting the request would push the new group past the
//!       configured subagent-depth cap (default [`DEFAULT_MAX_SUBAGENT_DEPTH`]).
//!       Depth = parent's depth + 1 (or 1 when the parent is itself an
//!       un-spawned agent, e.g. the initial agent in the install).
//!    c. INSERTs `agent_groups` + `sessions` (+ optional `messaging_group_agents`).
//!    d. Writes a `create_agent_result` system row into the *parent* session's
//!       `inbound.db` so the calling agent sees the real id on its next turn.
//!
//! The container manager's reconcile loop polls the `sessions` table on a
//! short timer, so the new agent's container will spawn on its next tick
//! without any explicit notification from the handler.

mod create_agent;
mod depth;
mod dispatch;
mod inbound_seed;
mod permissions;

use crate::context::{InterceptorCtx, InterceptorDecision, Module, ModuleContext};
use crate::error::ModuleError;
use async_trait::async_trait;
use copperclaw_types::ChannelType;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub use create_agent::{CreateAgentHandler, CreateAgentModule};
pub use depth::{DEFAULT_MAX_SUBAGENT_DEPTH, MAX_SUBAGENT_DEPTH_CEILING};
pub use dispatch::AgentDispatchModule;
pub use permissions::{
    always_allow, always_deny, users_table_check, CreateAgentPermissionCheck,
    CreateAgentPermissionCtx,
};

/// The `agent:` URL prefix.
pub const AGENT_PREFIX: &str = "agent:";

/// Default channel a `create_agent` call binds to when the payload omits one.
pub const DEFAULT_CREATE_AGENT_CHANNEL: &str = "cli";

/// Platform identifier used for synthetic messaging-groups created via
/// `create_agent`. The spawned agent has no real channel platform — it's
/// addressable only by other agents — so we use a stable "agent-spawn"
/// placeholder so the wiring is unique per-agent.
pub(crate) const SPAWN_PLATFORM_PREFIX: &str = "agent-spawn:";

/// Parsed agent destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef {
    /// Bare agent name (folder slug or display name as configured in the
    /// destinations table).
    pub name: String,
}

/// `true` if `s` looks like an agent destination (`agent:<...>` or
/// `agent://<...>`).
pub fn is_agent_destination(s: &str) -> bool {
    parse(s).is_some()
}

/// Parse `agent:<name>` or `agent://<name>` strings.
pub fn parse(s: &str) -> Option<AgentRef> {
    let s = s.trim();
    let after = s
        .strip_prefix("agent://")
        .or_else(|| s.strip_prefix(AGENT_PREFIX))?;
    let name = after.trim();
    if name.is_empty() {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return None;
    }
    Some(AgentRef {
        name: name.to_owned(),
    })
}

/// Module wraps the helpers above and registers a message interceptor that
/// tags outbound messages whose destination resolves to an agent. Kept as a
/// unit struct so existing call sites (`Box::new(AgentToAgentModule)`) keep
/// compiling; the `create_agent` delivery action is registered separately
/// by [`CreateAgentModule`].
pub struct AgentToAgentModule;

impl Default for AgentToAgentModule {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl Module for AgentToAgentModule {
    fn name(&self) -> &'static str {
        "agent_to_agent"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        ctx.set_message_interceptor(Arc::new(|i: InterceptorCtx| {
            // If the outbound destination's channel_type is `agent`, the host's
            // delivery loop already routes by `agent_group_id`. We pass it
            // through unchanged. The interceptor exists so the host has a hook
            // to log or rewrite agent-bound messages.
            if i
                .channel_type
                .as_ref()
                .is_some_and(|c| c.as_str() == ChannelType::AGENT)
            {
                return InterceptorDecision::Passthrough;
            }
            // For non-agent destinations, also a pass-through — the module's
            // raison d'être is the helper functions, not interception.
            InterceptorDecision::Passthrough
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{DispatchTarget, MockModuleContext};
    use copperclaw_types::{AgentGroupId, MessageKind, OutboundMessage};

    #[test]
    fn parses_simple_agent_name() {
        let r = parse("agent:helper").unwrap();
        assert_eq!(r.name, "helper");
        assert!(is_agent_destination("agent:helper"));
    }

    #[test]
    fn parses_url_form() {
        let r = parse("agent://my.bot").unwrap();
        assert_eq!(r.name, "my.bot");
    }

    #[test]
    fn allows_dash_underscore_dot_in_name() {
        assert_eq!(parse("agent:foo-bar_baz.42").unwrap().name, "foo-bar_baz.42");
    }

    #[test]
    fn rejects_empty_name() {
        assert!(parse("agent:").is_none());
        assert!(parse("agent://").is_none());
    }

    #[test]
    fn rejects_invalid_chars() {
        assert!(parse("agent:hello world").is_none());
        assert!(parse("agent:hello/etc").is_none());
        assert!(parse("agent:hello!").is_none());
    }

    #[test]
    fn rejects_non_agent_strings() {
        assert!(parse("telegram:chat-1").is_none());
        assert!(parse("helper").is_none());
        assert!(parse("").is_none());
        assert!(!is_agent_destination("https://example.com"));
    }

    #[test]
    fn parses_trimmed_input() {
        let r = parse("  agent:helper  ").unwrap();
        assert_eq!(r.name, "helper");
    }

    #[test]
    fn agent_ref_serde_roundtrip() {
        let r = AgentRef {
            name: "helper".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: AgentRef = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[tokio::test]
    async fn install_registers_interceptor() {
        let m = AgentToAgentModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        assert_eq!(ctx.registered(), vec!["message_interceptor"]);
    }

    #[tokio::test]
    async fn interceptor_is_passthrough_for_agent_channel() {
        let m = AgentToAgentModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let hook = ctx.interceptors.lock().unwrap()[0].clone();
        let dec = hook(InterceptorCtx {
            message: OutboundMessage {
                kind: MessageKind::Agent,
                content: serde_json::json!({}),
                files: vec![],
            },
            channel_type: Some(ChannelType::new(ChannelType::AGENT)),
            platform_id: None,
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        });
        assert!(dec.is_passthrough());
    }

    #[tokio::test]
    async fn interceptor_is_passthrough_for_non_agent() {
        let m = AgentToAgentModule;
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let hook = ctx.interceptors.lock().unwrap()[0].clone();
        let dec = hook(InterceptorCtx {
            message: OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({}),
                files: vec![],
            },
            channel_type: Some(ChannelType::new("telegram")),
            platform_id: Some("C1".into()),
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        });
        assert!(dec.is_passthrough());
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(AgentToAgentModule.name(), "agent_to_agent");
    }

    // Compile-time use of DispatchTarget::agent to keep its tests honest.
    #[test]
    fn dispatch_target_agent_used() {
        let t = DispatchTarget::agent(AgentGroupId::new());
        assert_eq!(
            t.channel_type.as_ref().map(ChannelType::as_str),
            Some(ChannelType::AGENT)
        );
    }
}
