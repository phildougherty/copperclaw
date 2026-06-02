//! Hook chain for the router.
//!
//! The router's behavior is steered by a small set of closures registered
//! by modules at boot. Each slot holds an `Option<...>` so the host can
//! distinguish "no module wired this hook" from "the module returned no
//! decision" — the former falls back to the default policy in
//! [`crate::route`], the latter does not.
//!
//! Concurrency: the host expects to register hooks during boot, before
//! any `Router::route` calls run. To allow modules to install themselves
//! from `async fn`s we still wrap each slot in a `Mutex`; the lock is
//! held only for the brief replacement and for the duration of the
//! callback invocation inside `route`.

use copperclaw_modules::context::{
    AccessGate, ChannelRequestCtx, ChannelRequestGate, GateCtx, GateDecision, InterceptorCtx,
    InterceptorDecision, MessageInterceptor, SenderResolver, SenderScopeCtx, SenderScopeDecision,
    SenderScopeGate,
};
use copperclaw_types::{InboundEvent, UserId};
use std::sync::Mutex;

/// Storage for every router-owned hook. The host instantiates a single
/// `HookChain` and shares it with whichever `ModuleContext` adapter wires
/// modules into it.
///
/// All fields are public to the crate root via accessor methods; tests reach
/// in through `Router::hooks()` / `Router::hooks_mut()`.
#[derive(Default)]
pub struct HookChain {
    sender_resolver: Mutex<Option<SenderResolver>>,
    access_gate: Mutex<Option<AccessGate>>,
    sender_scope_gate: Mutex<Option<SenderScopeGate>>,
    message_interceptor: Mutex<Option<MessageInterceptor>>,
    channel_request_gate: Mutex<Option<ChannelRequestGate>>,
}

impl HookChain {
    /// Create an empty hook chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install (or replace) the sender resolver hook.
    pub fn set_sender_resolver(&self, f: SenderResolver) {
        *self.sender_resolver.lock().expect("sender_resolver mutex") = Some(f);
    }

    /// Install (or replace) the access-gate hook.
    pub fn set_access_gate(&self, f: AccessGate) {
        *self.access_gate.lock().expect("access_gate mutex") = Some(f);
    }

    /// Install (or replace) the sender-scope-gate hook.
    pub fn set_sender_scope_gate(&self, f: SenderScopeGate) {
        *self.sender_scope_gate.lock().expect("sender_scope mutex") = Some(f);
    }

    /// Install (or replace) the message-interceptor hook.
    ///
    /// The router itself does not consume this hook on the inbound path —
    /// the interceptor fires on the *outbound* path inside the delivery
    /// loop. The router stores it so the host's `ModuleContext` adapter
    /// has a single home for the closure.
    pub fn set_message_interceptor(&self, f: MessageInterceptor) {
        *self
            .message_interceptor
            .lock()
            .expect("message_interceptor mutex") = Some(f);
    }

    /// Install (or replace) the channel-request-gate hook.
    pub fn set_channel_request_gate(&self, f: ChannelRequestGate) {
        *self
            .channel_request_gate
            .lock()
            .expect("channel_request_gate mutex") = Some(f);
    }

    /// True if any sender-resolver is currently registered.
    pub fn has_sender_resolver(&self) -> bool {
        self.sender_resolver
            .lock()
            .expect("sender_resolver mutex")
            .is_some()
    }

    /// True if any access-gate is currently registered.
    pub fn has_access_gate(&self) -> bool {
        self.access_gate
            .lock()
            .expect("access_gate mutex")
            .is_some()
    }

    /// True if any sender-scope-gate is currently registered.
    pub fn has_sender_scope_gate(&self) -> bool {
        self.sender_scope_gate
            .lock()
            .expect("sender_scope mutex")
            .is_some()
    }

    /// True if any message-interceptor is currently registered.
    pub fn has_message_interceptor(&self) -> bool {
        self.message_interceptor
            .lock()
            .expect("message_interceptor mutex")
            .is_some()
    }

    /// True if any channel-request-gate is currently registered.
    pub fn has_channel_request_gate(&self) -> bool {
        self.channel_request_gate
            .lock()
            .expect("channel_request_gate mutex")
            .is_some()
    }

    /// Run the sender-resolver hook (if any) and return `Some(UserId)` if
    /// it resolved.
    pub fn run_sender_resolver(&self, event: &InboundEvent) -> Option<UserId> {
        let guard = self.sender_resolver.lock().expect("sender_resolver mutex");
        let cb = guard.as_ref()?.clone();
        drop(guard);
        cb(event)
    }

    /// Run the access-gate hook (if any). When no gate is registered the
    /// host's default policy is `Allow`; callers receive `None` so they can
    /// implement that default themselves.
    pub fn run_access_gate(&self, ctx: GateCtx) -> Option<GateDecision> {
        let guard = self.access_gate.lock().expect("access_gate mutex");
        let cb = guard.as_ref()?.clone();
        drop(guard);
        Some(cb(ctx))
    }

    /// Run the sender-scope-gate hook (if any). `None` => no policy
    /// registered; the caller falls back to allow-by-default.
    pub fn run_sender_scope_gate(&self, ctx: SenderScopeCtx) -> Option<SenderScopeDecision> {
        let guard = self.sender_scope_gate.lock().expect("sender_scope mutex");
        let cb = guard.as_ref()?.clone();
        drop(guard);
        Some(cb(ctx))
    }

    /// Run the message-interceptor hook (if any). Used by the outbound
    /// delivery loop, but exposed here so the host's `ModuleContext` adapter
    /// can hand the closure to delivery without an extra registry.
    pub fn run_message_interceptor(&self, ctx: InterceptorCtx) -> Option<InterceptorDecision> {
        let guard = self
            .message_interceptor
            .lock()
            .expect("message_interceptor mutex");
        let cb = guard.as_ref()?.clone();
        drop(guard);
        Some(cb(ctx))
    }

    /// Run the channel-request-gate hook (if any).
    pub fn run_channel_request_gate(&self, ctx: ChannelRequestCtx) -> Option<GateDecision> {
        let guard = self
            .channel_request_gate
            .lock()
            .expect("channel_request_gate mutex");
        let cb = guard.as_ref()?.clone();
        drop(guard);
        Some(cb(ctx))
    }

    /// Take an owned snapshot of the message-interceptor closure (or `None`
    /// if no interceptor is registered). Used by the delivery crate to grab
    /// a reference without holding the chain's mutex across the call.
    pub fn message_interceptor(&self) -> Option<MessageInterceptor> {
        self.message_interceptor
            .lock()
            .expect("message_interceptor mutex")
            .clone()
    }
}

impl std::fmt::Debug for HookChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookChain")
            .field("sender_resolver", &self.has_sender_resolver())
            .field("access_gate", &self.has_access_gate())
            .field("sender_scope_gate", &self.has_sender_scope_gate())
            .field("message_interceptor", &self.has_message_interceptor())
            .field("channel_request_gate", &self.has_channel_request_gate())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::{AgentGroupId, ChannelType, InboundMessage, MessageKind};
    use std::sync::Arc;

    fn event() -> InboundEvent {
        InboundEvent {
            channel_type: ChannelType::new("cli"),
            platform_id: "p1".into(),
            thread_id: None,
            message: InboundMessage {
                id: "m1".into(),
                kind: MessageKind::Chat,
                content: serde_json::json!({"text":"hi"}),
                timestamp: chrono::Utc::now(),
                is_mention: None,
                is_group: None,
            },
            reply_to: None,
            sender: None,
        }
    }

    #[test]
    fn default_chain_has_no_hooks() {
        let h = HookChain::new();
        assert!(!h.has_sender_resolver());
        assert!(!h.has_access_gate());
        assert!(!h.has_sender_scope_gate());
        assert!(!h.has_message_interceptor());
        assert!(!h.has_channel_request_gate());
    }

    #[test]
    fn sender_resolver_runs_after_registration() {
        let h = HookChain::new();
        let id = UserId::new();
        h.set_sender_resolver(Arc::new(move |_| Some(id)));
        assert!(h.has_sender_resolver());
        assert_eq!(h.run_sender_resolver(&event()), Some(id));
    }

    #[test]
    fn access_gate_runs_after_registration() {
        let h = HookChain::new();
        h.set_access_gate(Arc::new(|_| GateDecision::Allow));
        assert!(h.has_access_gate());
        let ctx = GateCtx {
            user: None,
            agent_group_id: Some(AgentGroupId::new()),
            messaging_group_id: None,
            op: "deliver_message".into(),
        };
        assert_eq!(h.run_access_gate(ctx), Some(GateDecision::Allow));
    }

    #[test]
    fn sender_scope_gate_runs_after_registration() {
        let h = HookChain::new();
        h.set_sender_scope_gate(Arc::new(|_| SenderScopeDecision::Pending("p".into())));
        assert!(h.has_sender_scope_gate());
        let ctx = SenderScopeCtx {
            event_sender: None,
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        };
        let decision = h.run_sender_scope_gate(ctx).unwrap();
        assert!(decision.is_pending());
    }

    #[test]
    fn message_interceptor_runs_after_registration() {
        let h = HookChain::new();
        h.set_message_interceptor(Arc::new(|_| InterceptorDecision::Passthrough));
        assert!(h.has_message_interceptor());
        let ctx = InterceptorCtx {
            message: copperclaw_types::OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({"text":"hi"}),
                files: vec![],
            },
            channel_type: None,
            platform_id: None,
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        };
        let decision = h.run_message_interceptor(ctx).unwrap();
        assert!(decision.is_passthrough());
    }

    #[test]
    fn channel_request_gate_runs_after_registration() {
        let h = HookChain::new();
        h.set_channel_request_gate(Arc::new(|_| GateDecision::Deny("nope".into())));
        assert!(h.has_channel_request_gate());
        let ctx = ChannelRequestCtx {
            channel_type: ChannelType::new("cli"),
            platform_id: "p1".into(),
            thread_id: None,
            requester: None,
            agent_group_id: None,
        };
        let decision = h.run_channel_request_gate(ctx).unwrap();
        assert!(decision.is_deny());
    }

    #[test]
    fn message_interceptor_clonable_snapshot() {
        let h = HookChain::new();
        assert!(h.message_interceptor().is_none());
        h.set_message_interceptor(Arc::new(|_| InterceptorDecision::Passthrough));
        let snap = h.message_interceptor().expect("snapshot exists");
        let ctx = InterceptorCtx {
            message: copperclaw_types::OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({"text":"hi"}),
                files: vec![],
            },
            channel_type: None,
            platform_id: None,
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        };
        assert!(snap(ctx).is_passthrough());
    }

    #[test]
    fn missing_hooks_run_returns_none() {
        let h = HookChain::new();
        assert!(h.run_sender_resolver(&event()).is_none());
        let gctx = GateCtx {
            user: None,
            agent_group_id: None,
            messaging_group_id: None,
            op: "x".into(),
        };
        assert!(h.run_access_gate(gctx).is_none());
        let sctx = SenderScopeCtx {
            event_sender: None,
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        };
        assert!(h.run_sender_scope_gate(sctx).is_none());
        let ictx = InterceptorCtx {
            message: copperclaw_types::OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({}),
                files: vec![],
            },
            channel_type: None,
            platform_id: None,
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        };
        assert!(h.run_message_interceptor(ictx).is_none());
        let cctx = ChannelRequestCtx {
            channel_type: ChannelType::new("cli"),
            platform_id: "p".into(),
            thread_id: None,
            requester: None,
            agent_group_id: None,
        };
        assert!(h.run_channel_request_gate(cctx).is_none());
    }

    #[test]
    fn debug_impl_includes_flags() {
        let h = HookChain::new();
        let s = format!("{h:?}");
        assert!(s.contains("HookChain"));
        assert!(s.contains("sender_resolver: false"));
    }
}
