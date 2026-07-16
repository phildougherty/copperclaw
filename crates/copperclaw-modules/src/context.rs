//! Module ↔ host contract surface.
//!
//! Modules register themselves against [`ModuleContext`]. The host (T3)
//! implements this trait against its hook chain; until the host crate lands,
//! a `MockModuleContext` in tests captures every registration and lets us
//! assert that each module wires the expected set of hooks.

use crate::error::ModuleError;
use async_trait::async_trait;
use copperclaw_types::{
    AgentGroupId, ChannelType, InboundEvent, MessageId, MessagingGroupId, OutboundMessage,
    SenderIdentity, SessionId, UserId,
};
use std::sync::Arc;

/// A module is a discrete bundle of hooks. Modules are constructed by the
/// host's boot sequence, then `install` is called once for each in priority
/// order. After `install` returns the module SHOULD not retain a reference
/// to the context except via the closures it registered.
#[async_trait]
pub trait Module: Send + Sync {
    /// Stable, human-readable name for diagnostics and `cclaw modules list`.
    fn name(&self) -> &'static str;

    /// Wire all hooks this module needs. Called once at host boot.
    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError>;
}

/// Closure type a sender-resolver hook receives.
pub type SenderResolver = Arc<dyn Fn(&InboundEvent) -> Option<UserId> + Send + Sync>;

/// Closure registered via [`ModuleContext::set_access_gate`].
pub type AccessGate = Arc<dyn Fn(GateCtx) -> GateDecision + Send + Sync>;
/// Closure registered via [`ModuleContext::set_sender_scope_gate`].
pub type SenderScopeGate = Arc<dyn Fn(SenderScopeCtx) -> SenderScopeDecision + Send + Sync>;
/// Closure registered via [`ModuleContext::set_message_interceptor`].
pub type MessageInterceptor = Arc<dyn Fn(InterceptorCtx) -> InterceptorDecision + Send + Sync>;
/// Closure registered via [`ModuleContext::set_channel_request_gate`].
pub type ChannelRequestGate = Arc<dyn Fn(ChannelRequestCtx) -> GateDecision + Send + Sync>;
/// Closure registered via [`ModuleContext::on_delivery_adapter_ready`].
pub type DeliveryReadyCallback = Arc<dyn Fn(Arc<dyn DeliveryDispatcher>) + Send + Sync>;

/// Module hook surface, implemented by the host. Each setter registers a hook
/// keyed by hook kind; the host chains multiple modules' callbacks together in
/// registration order.
#[async_trait]
pub trait ModuleContext: Send + Sync {
    /// Resolve an inbound event's sender to a known `UserId`. The first
    /// resolver to return `Some` wins.
    fn set_sender_resolver(&self, f: SenderResolver);

    /// Gate access to an agent-group operation by the calling user's role.
    fn set_access_gate(&self, f: AccessGate);

    /// Gate whether an unknown sender on a known messaging-group is allowed
    /// to engage the agent on this turn.
    fn set_sender_scope_gate(&self, f: SenderScopeGate);

    /// Intercept an outbound message just before delivery. Modules may
    /// rewrite, drop, or pass it through unchanged.
    fn set_message_interceptor(&self, f: MessageInterceptor);

    /// Gate whether a channel-level engagement request (e.g. an unknown
    /// messaging group asks the agent to join) is allowed.
    fn set_channel_request_gate(&self, f: ChannelRequestGate);

    /// Register a named delivery action; the runner can target it from
    /// `messages_out` via `OutboundMessage.kind == System` with
    /// `content.action == "<name>"`.
    fn register_delivery_action(&self, name: &str, h: Arc<dyn DeliveryActionHandler>);

    /// Called by the host once the delivery loop is alive. Modules that need
    /// to push messages of their own initiative (the typing module, the
    /// approvals module's pending-card refresher) capture the dispatcher
    /// reference here. The host passes an `Arc<dyn DeliveryDispatcher>` so
    /// the module can store the dispatcher for the lifetime of the host.
    fn on_delivery_adapter_ready(&self, cb: DeliveryReadyCallback);
}

/// Capability flags the host can advertise so modules know what to register.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountHostContext {
    /// Absolute path of the session root the host enforces all bind mounts
    /// to stay inside of.
    pub session_root: std::path::PathBuf,
}

// ---------------------------------------------------------------------------
// Access gate
// ---------------------------------------------------------------------------

/// Input to an access-gate decision.
#[derive(Debug, Clone)]
pub struct GateCtx {
    /// The user invoking the operation (resolved via `set_sender_resolver`).
    pub user: Option<UserId>,
    /// The agent group the operation targets, if known.
    pub agent_group_id: Option<AgentGroupId>,
    /// The messaging group the request originated from, if any.
    pub messaging_group_id: Option<MessagingGroupId>,
    /// A short, stable identifier for the operation being gated. Modules
    /// match on this string to decide whether to allow the call.
    pub op: String,
}

/// Output of an access-gate decision. `Defer` lets the host fall through to
/// the next gate in the chain; `Allow` and `Deny` short-circuit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Operation may proceed.
    Allow,
    /// Operation is denied. `reason` is shown to the requester.
    Deny(String),
    /// This gate has no opinion; defer to the next gate in the chain. If no
    /// gate produces a decision, the host's default policy applies.
    Defer,
}

impl GateDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny(_))
    }
    pub fn is_defer(&self) -> bool {
        matches!(self, Self::Defer)
    }
}

// ---------------------------------------------------------------------------
// Sender scope gate
// ---------------------------------------------------------------------------

/// Input to a sender-scope gate. The host populates this for every inbound
/// event on a wiring whose `sender_scope` is `known`.
#[derive(Debug, Clone)]
pub struct SenderScopeCtx {
    pub event_sender: Option<SenderIdentity>,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub agent_group_id: AgentGroupId,
    /// Resolved user id for `event_sender`, if any.
    pub resolved_user: Option<UserId>,
}

/// Output of a sender-scope decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderScopeDecision {
    /// Sender is allowed to engage on this turn.
    Allow,
    /// Sender is denied. The router will drop the event into
    /// `dropped_messages` with this reason.
    Deny(String),
    /// Sender is unknown; the approvals flow should be invoked. The host
    /// writes an `unregistered_senders` row and drops the event.
    Pending(String),
    /// No opinion; defer to the next gate.
    Defer,
}

impl SenderScopeDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny(_))
    }
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending(_))
    }
    pub fn is_defer(&self) -> bool {
        matches!(self, Self::Defer)
    }
}

// ---------------------------------------------------------------------------
// Message interceptor
// ---------------------------------------------------------------------------

/// Input to a message-interceptor hook.
#[derive(Debug, Clone)]
pub struct InterceptorCtx {
    /// The outbound message that is about to be delivered.
    pub message: OutboundMessage,
    /// Channel + platform identifier of the destination.
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub thread_id: Option<String>,
    /// The agent group that produced the message.
    pub agent_group_id: AgentGroupId,
}

/// Result of an interceptor decision.
#[derive(Debug, Clone)]
pub enum InterceptorDecision {
    /// Pass the message through unchanged.
    Passthrough,
    /// Replace the message with this one (after rewriting / sanitizing).
    Replace(OutboundMessage),
    /// Drop the message entirely; record `reason` in the log.
    Drop(String),
}

impl InterceptorDecision {
    pub fn is_passthrough(&self) -> bool {
        matches!(self, Self::Passthrough)
    }
    pub fn is_replace(&self) -> bool {
        matches!(self, Self::Replace(_))
    }
    pub fn is_drop(&self) -> bool {
        matches!(self, Self::Drop(_))
    }
}

// ---------------------------------------------------------------------------
// Approval interception (M18 G1 — in-chat approvals)
// ---------------------------------------------------------------------------

/// Closure registered on the router's approval-interceptor hook. It recognises
/// an in-chat approval button tap (`approve:<id>` / `deny:<id>` callback),
/// verifies the tapping identity against the group's operator/approver set,
/// resolves the approval via the SAME DB path the CLI uses, writes the audit
/// row, and edits the card. Non-approver taps get a short "not authorized"
/// reply and the card stays live.
///
/// The closure is supplied by the host (which owns the central DB, the roles
/// table, and the CLI resolution path); the type lives in the modules crate so
/// the router can hold the hook slot without a circular dependency on the host.
pub type ApprovalInterceptor =
    Arc<dyn Fn(ApprovalInterceptCtx) -> ApprovalInterceptDecision + Send + Sync>;

/// Input to the [`ApprovalInterceptor`] hook. The router populates this for a
/// callback-carrying inbound event just before the mention gate.
#[derive(Debug, Clone)]
pub struct ApprovalInterceptCtx {
    /// Raw callback payload carried on the tap (`content.callback.data`),
    /// e.g. `approve:0190...` / `deny:0190...`.
    pub callback_data: String,
    /// The identity that tapped the button.
    pub event_sender: Option<SenderIdentity>,
    /// Resolved user id for `event_sender`, if the sender resolver matched a
    /// row in the central `users` table.
    pub resolved_user: Option<UserId>,
    /// The agent group whose wiring the tap arrived on.
    pub agent_group_id: AgentGroupId,
    /// The messaging group the tap arrived on, if known.
    pub messaging_group_id: Option<MessagingGroupId>,
    /// Channel coordinates of the card being tapped — used to edit the card
    /// in place and to deliver a "not authorized" reply on the same surface.
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub thread_id: Option<String>,
}

/// Outcome of the [`ApprovalInterceptor`] hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalInterceptDecision {
    /// The tap WAS an approval callback and the interceptor handled it
    /// (resolved, refused, or a race no-op). The router must not write an
    /// inbound row or wake the runner for this event.
    Handled,
    /// The tap was not an approval callback the interceptor recognises; the
    /// router continues routing it as an ordinary event.
    Passthrough,
}

impl ApprovalInterceptDecision {
    pub fn is_handled(&self) -> bool {
        matches!(self, Self::Handled)
    }
    pub fn is_passthrough(&self) -> bool {
        matches!(self, Self::Passthrough)
    }
}

// ---------------------------------------------------------------------------
// Channel-request gate
// ---------------------------------------------------------------------------

/// Input to a channel-request gate (used when a previously-unseen messaging
/// group is asking the host to engage on its behalf).
#[derive(Debug, Clone)]
pub struct ChannelRequestCtx {
    pub channel_type: ChannelType,
    pub platform_id: String,
    pub thread_id: Option<String>,
    /// The user (if known) who initiated the request.
    pub requester: Option<UserId>,
    pub agent_group_id: Option<AgentGroupId>,
}

// ---------------------------------------------------------------------------
// Delivery actions
// ---------------------------------------------------------------------------

/// Input passed to a registered delivery action.
#[derive(Debug, Clone)]
pub struct DeliveryActionInput {
    /// The name the action was registered under.
    pub action: String,
    /// Free-form JSON payload from the agent / module that emitted the
    /// system message.
    pub payload: serde_json::Value,
    /// Target the host resolved for this action; the action handler may
    /// override the dispatch target via [`DeliveryActionOutput::dispatch`].
    pub target: DispatchTarget,
    /// Identifier of the session whose outbound row produced this action.
    /// Populated by the host's delivery service when invoking the handler;
    /// `None` in some tests that construct `DeliveryActionInput` directly.
    pub session_id: Option<SessionId>,
    /// The outbound row's `MessageId`, threaded through so handlers can
    /// derive deterministic inbound ids for idempotent cross-session
    /// writes (used by `agent_dispatch` to dedup retries: the parent's
    /// inbound row reuses this id, and `INSERT OR IGNORE` semantics in
    /// `messages_in::insert_idempotent` make a retry a no-op).
    /// `None` in tests that construct `DeliveryActionInput` directly
    /// without a real outbound row.
    pub row_id: Option<MessageId>,
}

/// What a delivery action wants the host to do.
#[derive(Debug, Clone, Default)]
pub struct DeliveryActionOutput {
    /// Optional override of the dispatch target.
    pub dispatch: Option<DispatchTarget>,
    /// Optional message to deliver. If `None`, the action only had side
    /// effects (e.g. state mutation).
    pub message: Option<OutboundMessage>,
}

/// Where an outbound message goes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchTarget {
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub thread_id: Option<String>,
    /// For agent-to-agent routing, the target agent group id.
    pub agent_group_id: Option<AgentGroupId>,
}

impl DispatchTarget {
    pub fn channel(
        channel_type: ChannelType,
        platform_id: String,
        thread_id: Option<String>,
    ) -> Self {
        Self {
            channel_type: Some(channel_type),
            platform_id: Some(platform_id),
            thread_id,
            agent_group_id: None,
        }
    }

    pub fn agent(agent_group_id: AgentGroupId) -> Self {
        Self {
            channel_type: Some(ChannelType::new(ChannelType::AGENT)),
            platform_id: None,
            thread_id: None,
            agent_group_id: Some(agent_group_id),
        }
    }
}

/// Trait implemented by delivery action handlers.
pub trait DeliveryActionHandler: Send + Sync {
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError>;
}

/// Outcome of a best-effort [`DeliveryDispatcher::set_typing`] call, reported
/// back asynchronously so callers that re-fire on a cadence (the host's
/// `TypingTicker`) can back off when the channel rate-limits us instead of
/// hammering the adapter every tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypingOutcome {
    /// The adapter accepted the typing action (or it was a no-op).
    Ok,
    /// The channel rate-limited the typing action. Callers should skip this
    /// target for `retry_after` seconds (or a sane default when `None`).
    RateLimited { retry_after: Option<u64> },
}

/// Implemented by the host's delivery loop. Modules use it to push messages
/// of their own initiative (typing indicators, refreshed approval cards).
pub trait DeliveryDispatcher: Send + Sync {
    /// Best-effort: ask the channel adapter to emit a typing indicator.
    ///
    /// Returns a receiver that resolves to the dispatch [`TypingOutcome`] once
    /// the (spawned, async) adapter call completes, or `None` when nothing was
    /// dispatched (no adapter / missing target fields). Callers that don't care
    /// about rate-limit feedback may simply drop it.
    fn set_typing(
        &self,
        target: &DispatchTarget,
    ) -> Option<tokio::sync::oneshot::Receiver<TypingOutcome>>;

    /// Push a synthetic outbound message through the normal delivery path.
    fn dispatch(&self, target: &DispatchTarget, message: &OutboundMessage);

    /// Best-effort: ask the channel adapter to edit a previously delivered
    /// message in place, addressed by its platform-side message id. Used by
    /// the in-chat approvals flow (M18 G1) to stamp a resolved card
    /// "Approved by <name>" / "Denied by <name>". The default is a no-op so
    /// dispatchers that don't support editing (test doubles) need not override
    /// it; the host's real dispatcher drives `ChannelAdapter::edit_message`.
    fn edit_message(&self, _target: &DispatchTarget, _platform_message_id: &str, _text: &str) {}
}

// ---------------------------------------------------------------------------
// Mock context for testing
// ---------------------------------------------------------------------------

pub use mock::{MockDispatcher, MockModuleContext};

mod mock {
    use super::{
        AccessGate, ChannelRequestGate, DeliveryActionHandler, DeliveryDispatcher,
        DeliveryReadyCallback, DispatchTarget, MessageInterceptor, ModuleContext, SenderResolver,
        SenderScopeGate,
    };
    use copperclaw_types::OutboundMessage;
    use std::sync::{Arc, Mutex};

    /// Mock implementation of `ModuleContext` used by every module's test.
    /// Records the names of every hook registered, plus a tiny callable copy
    /// of each closure so tests can exercise the registered logic.
    #[derive(Default)]
    pub struct MockModuleContext {
        pub registered: Mutex<Vec<&'static str>>,
        pub delivery_actions: Mutex<Vec<String>>,

        pub sender_resolvers: Mutex<Vec<SenderResolver>>,
        pub access_gates: Mutex<Vec<AccessGate>>,
        pub sender_scope_gates: Mutex<Vec<SenderScopeGate>>,
        pub interceptors: Mutex<Vec<MessageInterceptor>>,
        pub channel_request_gates: Mutex<Vec<ChannelRequestGate>>,
        pub action_handlers: Mutex<Vec<(String, Arc<dyn DeliveryActionHandler>)>>,
        pub on_ready_callbacks: Mutex<Vec<DeliveryReadyCallback>>,
    }

    impl MockModuleContext {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub fn registered(&self) -> Vec<&'static str> {
            self.registered.lock().unwrap().clone()
        }

        pub fn delivery_actions(&self) -> Vec<String> {
            self.delivery_actions.lock().unwrap().clone()
        }

        pub fn fire_delivery_ready(&self, dispatcher: &Arc<dyn DeliveryDispatcher>) {
            let cbs: Vec<_> = self.on_ready_callbacks.lock().unwrap().clone();
            for cb in cbs {
                cb(Arc::clone(dispatcher));
            }
        }
    }

    impl ModuleContext for MockModuleContext {
        fn set_sender_resolver(&self, f: SenderResolver) {
            self.registered.lock().unwrap().push("sender_resolver");
            self.sender_resolvers.lock().unwrap().push(f);
        }
        fn set_access_gate(&self, f: AccessGate) {
            self.registered.lock().unwrap().push("access_gate");
            self.access_gates.lock().unwrap().push(f);
        }
        fn set_sender_scope_gate(&self, f: SenderScopeGate) {
            self.registered.lock().unwrap().push("sender_scope_gate");
            self.sender_scope_gates.lock().unwrap().push(f);
        }
        fn set_message_interceptor(&self, f: MessageInterceptor) {
            self.registered.lock().unwrap().push("message_interceptor");
            self.interceptors.lock().unwrap().push(f);
        }
        fn set_channel_request_gate(&self, f: ChannelRequestGate) {
            self.registered.lock().unwrap().push("channel_request_gate");
            self.channel_request_gates.lock().unwrap().push(f);
        }
        fn register_delivery_action(&self, name: &str, h: Arc<dyn DeliveryActionHandler>) {
            self.registered.lock().unwrap().push("delivery_action");
            self.delivery_actions.lock().unwrap().push(name.to_owned());
            self.action_handlers
                .lock()
                .unwrap()
                .push((name.to_owned(), h));
        }
        fn on_delivery_adapter_ready(&self, cb: DeliveryReadyCallback) {
            self.registered.lock().unwrap().push("delivery_ready");
            self.on_ready_callbacks.lock().unwrap().push(cb);
        }
    }

    /// Mock implementation of `DeliveryDispatcher`. Records every call.
    #[derive(Default)]
    pub struct MockDispatcher {
        pub typing_calls: Mutex<Vec<DispatchTarget>>,
        pub dispatched: Mutex<Vec<(DispatchTarget, OutboundMessage)>>,
        /// Recorded `edit_message` calls: `(target, platform_message_id, text)`.
        pub edits: Mutex<Vec<(DispatchTarget, String, String)>>,
    }

    impl MockDispatcher {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        pub fn typing_count(&self) -> usize {
            self.typing_calls.lock().unwrap().len()
        }
        pub fn dispatched_count(&self) -> usize {
            self.dispatched.lock().unwrap().len()
        }
        pub fn edit_count(&self) -> usize {
            self.edits.lock().unwrap().len()
        }
    }

    impl DeliveryDispatcher for MockDispatcher {
        fn set_typing(
            &self,
            target: &DispatchTarget,
        ) -> Option<tokio::sync::oneshot::Receiver<super::TypingOutcome>> {
            self.typing_calls.lock().unwrap().push(target.clone());
            None
        }
        fn dispatch(&self, target: &DispatchTarget, message: &OutboundMessage) {
            self.dispatched
                .lock()
                .unwrap()
                .push((target.clone(), message.clone()));
        }
        fn edit_message(&self, target: &DispatchTarget, platform_message_id: &str, text: &str) {
            self.edits.lock().unwrap().push((
                target.clone(),
                platform_message_id.to_owned(),
                text.to_owned(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::{ChannelType, MessageKind, OutboundMessage};

    #[test]
    fn gate_decision_helpers() {
        assert!(GateDecision::Allow.is_allow());
        assert!(GateDecision::Deny("no".into()).is_deny());
        assert!(GateDecision::Defer.is_defer());
        assert!(!GateDecision::Allow.is_deny());
    }

    #[test]
    fn sender_scope_helpers() {
        assert!(SenderScopeDecision::Allow.is_allow());
        assert!(SenderScopeDecision::Deny("no".into()).is_deny());
        assert!(SenderScopeDecision::Pending("wait".into()).is_pending());
        assert!(SenderScopeDecision::Defer.is_defer());
    }

    #[test]
    fn interceptor_helpers() {
        let msg = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({"text": "hi"}),
            files: vec![],
        };
        assert!(InterceptorDecision::Passthrough.is_passthrough());
        assert!(InterceptorDecision::Replace(msg.clone()).is_replace());
        assert!(InterceptorDecision::Drop("nope".into()).is_drop());
    }

    #[test]
    fn dispatch_target_channel_constructor() {
        let target = DispatchTarget::channel(
            ChannelType::new("telegram"),
            "chat-1".into(),
            Some("thread-2".into()),
        );
        assert_eq!(
            target.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(target.platform_id.as_deref(), Some("chat-1"));
        assert_eq!(target.thread_id.as_deref(), Some("thread-2"));
        assert!(target.agent_group_id.is_none());
    }

    #[test]
    fn dispatch_target_agent_constructor() {
        use copperclaw_types::AgentGroupId;
        let id = AgentGroupId::new();
        let target = DispatchTarget::agent(id);
        assert_eq!(target.agent_group_id, Some(id));
        assert_eq!(
            target.channel_type.as_ref().map(ChannelType::as_str),
            Some(ChannelType::AGENT)
        );
    }
}
