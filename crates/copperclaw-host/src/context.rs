//! Host implementation of [`copperclaw_modules::ModuleContext`].
//!
//! The host owns one [`HostContext`]. Each `Module::install` call receives an
//! `Arc<dyn ModuleContext>` pointing at the same `HostContext`, which fans
//! the registrations out to the router's owned hook chain (accessed via
//! `Router::hooks()`) and to the delivery service's action registry +
//! dispatcher.

use async_trait::async_trait;
use copperclaw_host_delivery::DeliveryService;
use copperclaw_host_router::{HookChain, Router};
use copperclaw_modules::context::{
    AccessGate, ChannelRequestGate, DeliveryActionHandler, DeliveryReadyCallback,
    MessageInterceptor, ModuleContext, SenderResolver, SenderScopeGate,
};
use std::sync::Arc;

/// Where the host writes hooks back to.
///
/// In production this is [`HookSink::Router`] which forwards into
/// `router.hooks()`. Tests use [`HookSink::Chain`] to write to a standalone
/// [`HookChain`] (so they can introspect what was registered without
/// constructing a full router).
pub enum HookSink {
    /// Production: forward hook installs into the router's hook chain.
    Router(Arc<Router>),
    /// Tests: write into a standalone hook chain.
    Chain(Arc<HookChain>),
}

impl HookSink {
    fn chain(&self) -> &HookChain {
        match self {
            Self::Router(r) => r.hooks(),
            Self::Chain(c) => c.as_ref(),
        }
    }
}

/// Host adapter that bridges modules to the router/delivery wiring.
pub struct HostContext {
    sink: HookSink,
    delivery: Arc<DeliveryService>,
    central: copperclaw_db::central::CentralDb,
}

impl HostContext {
    /// Read access to the central DB. Exposed so the host can build
    /// module closures (e.g. `ApprovalsModule`'s persistent lookup)
    /// without threading the DB through every module constructor.
    pub fn central(&self) -> &copperclaw_db::central::CentralDb {
        &self.central
    }

    /// Build a host context that forwards hook installs into `router.hooks()`.
    pub fn for_router(router: Arc<Router>, delivery: Arc<DeliveryService>) -> Arc<Self> {
        let central = delivery.central().clone();
        Arc::new(Self {
            sink: HookSink::Router(router),
            delivery,
            central,
        })
    }

    /// Build a host context that writes hooks into a standalone chain.
    /// Used by tests where constructing a full router is overkill.
    pub fn new(hooks: Arc<HookChain>, delivery: Arc<DeliveryService>) -> Arc<Self> {
        let central = delivery.central().clone();
        Arc::new(Self {
            sink: HookSink::Chain(hooks),
            delivery,
            central,
        })
    }

    /// Borrow the underlying delivery service handle.
    pub fn delivery(&self) -> &Arc<DeliveryService> {
        &self.delivery
    }

    /// Borrow the underlying hook chain. For test introspection.
    pub fn hook_chain(&self) -> &HookChain {
        self.sink.chain()
    }
}

impl std::fmt::Debug for HostContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostContext")
            .field("hooks", &self.sink.chain())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModuleContext for HostContext {
    fn set_sender_resolver(&self, f: SenderResolver) {
        self.sink.chain().set_sender_resolver(f);
    }

    fn set_access_gate(&self, f: AccessGate) {
        self.sink.chain().set_access_gate(f);
    }

    fn set_sender_scope_gate(&self, f: SenderScopeGate) {
        self.sink.chain().set_sender_scope_gate(f);
    }

    fn set_message_interceptor(&self, f: MessageInterceptor) {
        self.sink.chain().set_message_interceptor(f);
    }

    fn set_channel_request_gate(&self, f: ChannelRequestGate) {
        self.sink.chain().set_channel_request_gate(f);
    }

    fn register_delivery_action(&self, name: &str, h: Arc<dyn DeliveryActionHandler>) {
        self.delivery.register_action(name, h);
    }

    fn on_delivery_adapter_ready(&self, cb: DeliveryReadyCallback) {
        // The delivery service is already constructed by the time we hand
        // `HostContext` to modules, so the dispatcher is available now.
        cb(self.delivery.dispatcher());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::central::CentralDb;
    use copperclaw_host_delivery::FsSessionRoot as DelivFsSessionRoot;
    use copperclaw_host_router::{FsSessionRoot as RouterFsSessionRoot, Router, SessionRoot};
    use copperclaw_modules::context::{
        DeliveryActionInput, DeliveryActionOutput, GateCtx, GateDecision, InterceptorCtx,
        InterceptorDecision, SenderScopeCtx, SenderScopeDecision,
    };
    use copperclaw_modules::ModuleError;
    use copperclaw_types::{
        AgentGroupId, ChannelType, InboundEvent, InboundMessage, MessageKind, OutboundMessage,
        UserId,
    };

    struct CapturingAction;
    impl DeliveryActionHandler for CapturingAction {
        fn handle(
            &self,
            _input: DeliveryActionInput,
        ) -> Result<DeliveryActionOutput, ModuleError> {
            Ok(DeliveryActionOutput::default())
        }
    }

    fn delivery_service() -> (Arc<DeliveryService>, tempfile::TempDir) {
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root: Arc<dyn copperclaw_host_delivery::SessionRoot> =
            Arc::new(DelivFsSessionRoot::new(tmp.path()));
        let svc = DeliveryService::with_default_dispatcher(central, root, Vec::new());
        (svc, tmp)
    }

    fn router() -> (Arc<Router>, tempfile::TempDir) {
        let central = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> =
            Arc::new(RouterFsSessionRoot::new(tmp.path()));
        let router = Arc::new(Router::new(central, root));
        (router, tmp)
    }

    fn fresh_event() -> InboundEvent {
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

    #[tokio::test]
    async fn sender_resolver_propagates_via_chain() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let ctx: Arc<dyn ModuleContext> = HostContext::new(hooks.clone(), delivery);
        let known = UserId::new();
        ctx.set_sender_resolver(Arc::new(move |_| Some(known)));
        assert!(hooks.has_sender_resolver());
        assert_eq!(hooks.run_sender_resolver(&fresh_event()), Some(known));
    }

    #[tokio::test]
    async fn sender_resolver_propagates_via_router() {
        let (router, _tmp1) = router();
        let (delivery, _tmp2) = delivery_service();
        let ctx: Arc<dyn ModuleContext> =
            HostContext::for_router(Arc::clone(&router), delivery);
        let known = UserId::new();
        ctx.set_sender_resolver(Arc::new(move |_| Some(known)));
        assert!(router.hooks().has_sender_resolver());
    }

    #[tokio::test]
    async fn access_gate_propagates() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let ctx: Arc<dyn ModuleContext> = HostContext::new(hooks.clone(), delivery);
        ctx.set_access_gate(Arc::new(|_| GateDecision::Allow));
        let gctx = GateCtx {
            user: None,
            agent_group_id: Some(AgentGroupId::new()),
            messaging_group_id: None,
            op: "x".into(),
        };
        assert_eq!(hooks.run_access_gate(gctx), Some(GateDecision::Allow));
    }

    #[tokio::test]
    async fn sender_scope_gate_propagates() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let ctx: Arc<dyn ModuleContext> = HostContext::new(hooks.clone(), delivery);
        ctx.set_sender_scope_gate(Arc::new(|_| SenderScopeDecision::Allow));
        let sctx = SenderScopeCtx {
            event_sender: None,
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        };
        let d = hooks.run_sender_scope_gate(sctx).unwrap();
        assert!(d.is_allow());
    }

    #[tokio::test]
    async fn message_interceptor_propagates() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let ctx: Arc<dyn ModuleContext> = HostContext::new(hooks.clone(), delivery);
        ctx.set_message_interceptor(Arc::new(|_| InterceptorDecision::Passthrough));
        assert!(hooks.has_message_interceptor());
        let ictx = InterceptorCtx {
            message: OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({}),
                files: vec![],
            },
            channel_type: None,
            platform_id: None,
            thread_id: None,
            agent_group_id: AgentGroupId::new(),
        };
        let d = hooks.run_message_interceptor(ictx).unwrap();
        assert!(d.is_passthrough());
    }

    #[tokio::test]
    async fn channel_request_gate_propagates() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let ctx: Arc<dyn ModuleContext> = HostContext::new(hooks.clone(), delivery);
        ctx.set_channel_request_gate(Arc::new(|_| GateDecision::Deny("n".into())));
        assert!(hooks.has_channel_request_gate());
    }

    #[tokio::test]
    async fn delivery_action_registers_with_service() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let ctx: Arc<dyn ModuleContext> = HostContext::new(hooks, delivery.clone());
        ctx.register_delivery_action("alpha", Arc::new(CapturingAction));
        assert!(delivery.action("alpha").is_some());
    }

    #[tokio::test]
    async fn on_delivery_adapter_ready_invokes_callback_now() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let ctx: Arc<dyn ModuleContext> = HostContext::new(hooks, delivery);
        let invoked = Arc::new(std::sync::Mutex::new(0u32));
        let counter = invoked.clone();
        ctx.on_delivery_adapter_ready(Arc::new(move |_dispatcher| {
            *counter.lock().unwrap() += 1;
        }));
        assert_eq!(*invoked.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn accessors_expose_internals() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let host = HostContext::new(hooks.clone(), delivery.clone());
        assert!(Arc::ptr_eq(host.delivery(), &delivery));
        // hook_chain accessor.
        assert!(!host.hook_chain().has_sender_resolver());
    }

    #[tokio::test]
    async fn debug_renders_struct_name() {
        let hooks = Arc::new(HookChain::new());
        let (delivery, _tmp) = delivery_service();
        let host = HostContext::new(hooks, delivery);
        let s = format!("{host:?}");
        assert!(s.contains("HostContext"));
    }

    #[test]
    fn channel_type_unused_smoke() {
        // Lint suppression for the imported alias.
        let _ = ChannelType::new("cli");
    }
}
