//! Pending approval workflow.
//!
//! Three hooks are wired:
//!
//! 1. `set_sender_scope_gate` — when an inbound event comes from an unknown
//!    sender, the gate returns [`SenderScopeDecision::Pending`] so the host
//!    will record an `unregistered_senders` row and drop the event.
//!
//! 2. `register_delivery_action("approval_card")` — when an agent emits an
//!    outbound system message requesting an approval card, this handler
//!    builds a structured card payload pointed at an approver and returns it
//!    via [`DeliveryActionOutput::message`].
//!
//! 3. `on_delivery_adapter_ready` — captures the [`DeliveryDispatcher`] so
//!    the module can post an "approve?" notification to the operator through
//!    the agent group's primary messaging channel the first time an unknown
//!    sender is recorded. De-duplication is provided by the host-side
//!    notifier consulting `unregistered_senders` before posting — no second
//!    notification is posted for the same `(messaging_group, sender_identity)`
//!    pair. If the agent group has no associated messaging group the
//!    notification is silently skipped (logged at info).

use crate::context::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput, DeliveryDispatcher,
    DispatchTarget, Module, ModuleContext, SenderScopeCtx, SenderScopeDecision,
};
use crate::error::ModuleError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_types::{
    AgentGroupId, ApprovalId, ApprovalKind, ChannelType, MessageKind, MessagingGroupId,
    OutboundMessage, SenderIdentity, UserId,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// One row of [`ApprovalsModule::pending_approvals_summary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSummary {
    pub id: ApprovalId,
    pub kind: ApprovalKind,
    pub messaging_group_id: Option<MessagingGroupId>,
    pub agent_group_id: Option<AgentGroupId>,
    pub requester: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub description: String,
}

#[derive(Debug, Default)]
struct PendingStore {
    pending: Vec<ApprovalSummary>,
}

/// Persistent-store lookup the gate consults when its in-memory
/// `known_senders` set misses. Returns `true` to approve the sender,
/// `false` to leave the decision to the in-memory list (and then
/// Pending). Sourced from the central `users` table by the host;
/// kept as a closure here so the modules crate doesn't take a
/// circular dep on `ironclaw-db`.
pub type SenderLookup =
    Arc<dyn Fn(&SenderIdentity) -> bool + Send + Sync>;

/// Context passed to [`NewPendingNotifier`] when a new sender lands in the
/// pending queue for the first time.
#[derive(Debug, Clone)]
pub struct NewPendingCtx {
    /// The identity of the sender that was just placed in pending.
    pub sender: SenderIdentity,
    /// The agent group the sender tried to reach.
    pub agent_group_id: AgentGroupId,
    /// The messaging group the event arrived on, if known.
    pub messaging_group_id: Option<MessagingGroupId>,
    /// Timestamp of the first contact attempt.
    pub first_seen: DateTime<Utc>,
}

/// Callback invoked by the approvals gate the first time a new sender is
/// placed in pending. The host wires this to a closure that posts a
/// notification through the agent group's primary messaging channel.
/// The closure is called synchronously inside the gate (which itself runs
/// on the router's hot path), so it must be fast. Any I/O should be
/// dispatched asynchronously via the [`DeliveryDispatcher`].
pub type NewPendingNotifier =
    Arc<dyn Fn(NewPendingCtx, Arc<dyn DeliveryDispatcher>) + Send + Sync>;

/// Approvals module.
pub struct ApprovalsModule {
    /// In-memory pending list, fed by the host via `record_pending` calls.
    store: Arc<Mutex<PendingStore>>,
    /// Set of `(channel_type, identity)` tuples that are already approved.
    known_senders: Arc<Mutex<Vec<SenderIdentity>>>,
    /// Persistent-store lookup invoked when the in-memory set
    /// misses. `None` = in-memory only.
    persistent_lookup: Option<SenderLookup>,
    /// Optional callback fired the first time a new sender hits pending.
    /// Wired by the host at boot to post an in-channel "approve?" prompt.
    new_pending_notifier: Option<NewPendingNotifier>,
    /// Dispatcher captured via `on_delivery_adapter_ready`; used by the
    /// gate closure to post notifications without blocking the router.
    dispatcher: Arc<Mutex<Option<Arc<dyn DeliveryDispatcher>>>>,
}

impl Default for ApprovalsModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalsModule {
    pub fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(PendingStore::default())),
            known_senders: Arc::new(Mutex::new(Vec::new())),
            persistent_lookup: None,
            new_pending_notifier: None,
            dispatcher: Arc::new(Mutex::new(None)),
        }
    }

    /// Build a module that starts with the supplied senders already
    /// approved. Used by the host to pre-trust deterministic
    /// platform-side identities (e.g. the `cli` channel's `local`
    /// sender, where the "user" is the operator running `ironclaw run`
    /// itself — there is nothing meaningful to approve).
    #[must_use]
    pub fn with_initial_approved(senders: Vec<SenderIdentity>) -> Self {
        Self {
            store: Arc::new(Mutex::new(PendingStore::default())),
            known_senders: Arc::new(Mutex::new(senders)),
            persistent_lookup: None,
            new_pending_notifier: None,
            dispatcher: Arc::new(Mutex::new(None)),
        }
    }

    /// Builder: attach a persistent-store lookup. The gate consults
    /// it after the in-memory set; only on a double miss is the
    /// sender declared `Pending`. The host wires this to a closure
    /// that queries the central `users` table.
    #[must_use]
    pub fn with_persistent_lookup(mut self, lookup: SenderLookup) -> Self {
        self.persistent_lookup = Some(lookup);
        self
    }

    /// Builder: attach a notifier that fires the first time a new unknown
    /// sender is placed in pending. The host wires this to post an
    /// in-channel "approve?" message to the operator.
    #[must_use]
    pub fn with_new_pending_notifier(mut self, notifier: NewPendingNotifier) -> Self {
        self.new_pending_notifier = Some(notifier);
        self
    }

    /// Mark a sender as approved so the gate stops returning `Pending` for
    /// them.
    pub fn approve_sender(&self, identity: SenderIdentity) {
        self.known_senders.lock().unwrap().push(identity);
    }

    /// Public for diagnostics + tests; the gate uses an internal copy.
    pub fn is_known(&self, identity: &SenderIdentity) -> bool {
        self.known_senders
            .lock()
            .unwrap()
            .iter()
            .any(|k| k.channel_type == identity.channel_type && k.identity == identity.identity)
    }

    /// Push a new pending approval into the in-memory store. The host calls
    /// this when its sender-scope gate produces `Pending`.
    pub fn record_pending(&self, summary: ApprovalSummary) {
        self.store.lock().unwrap().pending.push(summary);
    }

    /// Remove a pending approval by id (called once an admin approves/rejects).
    pub fn resolve(&self, id: ApprovalId) {
        self.store.lock().unwrap().pending.retain(|p| p.id != id);
    }

    /// Build the summary list, optionally filtered by kind.
    pub fn pending_approvals_summary(&self, kind: Option<ApprovalKind>) -> Vec<ApprovalSummary> {
        let store = self.store.lock().unwrap();
        store
            .pending
            .iter()
            .filter(|p| kind.is_none_or(|k| k == p.kind))
            .cloned()
            .collect()
    }
}

/// Handler that implements the `approval_card` delivery action. The agent
/// sends a `System` message with `content == {"approval_id": "...", "title":
/// "...", "to": {channel_type, platform_id, thread_id?}}` and this handler
/// reshapes it into a `Chat`-kind card aimed at the approver.
pub struct ApprovalCardHandler;

impl DeliveryActionHandler for ApprovalCardHandler {
    fn handle(
        &self,
        input: DeliveryActionInput,
    ) -> Result<DeliveryActionOutput, ModuleError> {
        let approval_id = input
            .payload
            .get("approval_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("approvals", "missing approval_id"))?;
        let title = input
            .payload
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Approval required");
        let to = input.payload.get("to");
        let channel_type = to
            .and_then(|t| t.get("channel_type"))
            .and_then(|v| v.as_str())
            .map(ChannelType::new);
        let platform_id = to
            .and_then(|t| t.get("platform_id"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let thread_id = to
            .and_then(|t| t.get("thread_id"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let target = match (channel_type, platform_id) {
            (Some(ct), Some(pid)) => Some(DispatchTarget::channel(ct, pid, thread_id)),
            _ => None,
        };
        let card_message = OutboundMessage {
            kind: MessageKind::Chat,
            content: serde_json::json!({
                "card": {
                    "type": "approval",
                    "approval_id": approval_id,
                    "title": title,
                },
            }),
            files: vec![],
        };
        Ok(DeliveryActionOutput {
            dispatch: target,
            message: Some(card_message),
        })
    }
}

#[async_trait]
impl Module for ApprovalsModule {
    fn name(&self) -> &'static str {
        "approvals"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        let known = Arc::clone(&self.known_senders);
        let persistent = self.persistent_lookup.clone();
        let notifier = self.new_pending_notifier.clone();
        let dispatcher_slot = Arc::clone(&self.dispatcher);

        ctx.set_sender_scope_gate(Arc::new(move |s: SenderScopeCtx| {
            let Some(sender) = &s.event_sender else {
                return SenderScopeDecision::Defer;
            };
            // Fast path: in-memory pre-approved list.
            {
                let known = known.lock().unwrap();
                if known.iter().any(|k| {
                    k.channel_type == sender.channel_type
                        && k.identity == sender.identity
                }) {
                    return SenderScopeDecision::Defer;
                }
            }
            // Persistent path: ask the host. The closure is the
            // central `users` table lookup so an `iclaw approvals
            // approve` mutation is reflected here on the very next
            // inbound, no host restart needed.
            if let Some(lookup) = &persistent {
                if lookup(sender) {
                    return SenderScopeDecision::Defer;
                }
            }
            // Sender is unknown. Fire the new-pending notifier if one is
            // registered and a dispatcher is available. The notifier is
            // responsible for de-duplicating via the DB upsert's
            // `newly_inserted` flag so this hot path stays cheap.
            if let Some(ref notify) = notifier {
                if let Some(ref dispatcher) = *dispatcher_slot.lock().unwrap() {
                    let ctx_info = NewPendingCtx {
                        sender: sender.clone(),
                        agent_group_id: s.agent_group_id,
                        messaging_group_id: s.messaging_group_id,
                        first_seen: Utc::now(),
                    };
                    notify(ctx_info, Arc::clone(dispatcher));
                }
            }
            SenderScopeDecision::Pending(format!(
                "sender `{}:{}` is pending approval",
                sender.channel_type, sender.identity
            ))
        }));
        ctx.register_delivery_action("approval_card", Arc::new(ApprovalCardHandler));

        // Capture the dispatcher reference so the gate closure can post
        // notifications without a circular dependency on the delivery crate.
        let dispatcher_slot2 = Arc::clone(&self.dispatcher);
        ctx.on_delivery_adapter_ready(Arc::new(move |d| {
            *dispatcher_slot2.lock().unwrap() = Some(d);
        }));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockModuleContext;
    use ironclaw_types::{AgentGroupId, ApprovalId};

    fn summary(kind: ApprovalKind) -> ApprovalSummary {
        ApprovalSummary {
            id: ApprovalId::new(),
            kind,
            messaging_group_id: None,
            agent_group_id: None,
            requester: None,
            created_at: Utc::now(),
            description: "test".into(),
        }
    }

    #[test]
    fn pending_store_records_and_resolves() {
        let m = ApprovalsModule::new();
        let s = summary(ApprovalKind::Sender);
        let id = s.id;
        m.record_pending(s);
        assert_eq!(m.pending_approvals_summary(None).len(), 1);
        m.resolve(id);
        assert!(m.pending_approvals_summary(None).is_empty());
    }

    #[test]
    fn pending_filter_by_kind() {
        let m = ApprovalsModule::new();
        m.record_pending(summary(ApprovalKind::Sender));
        m.record_pending(summary(ApprovalKind::InstallPackages));
        m.record_pending(summary(ApprovalKind::AddMcpServer));
        assert_eq!(
            m.pending_approvals_summary(Some(ApprovalKind::Sender)).len(),
            1
        );
        assert_eq!(
            m.pending_approvals_summary(Some(ApprovalKind::InstallPackages))
                .len(),
            1
        );
        assert_eq!(m.pending_approvals_summary(None).len(), 3);
    }

    #[test]
    fn approve_sender_marks_known() {
        let m = ApprovalsModule::new();
        let s = SenderIdentity {
            channel_type: ChannelType::new("telegram"),
            identity: "u-1".into(),
            display_name: None,
        };
        assert!(!m.is_known(&s));
        m.approve_sender(s.clone());
        assert!(m.is_known(&s));
    }

    #[tokio::test]
    async fn install_registers_scope_gate_action_and_delivery_ready() {
        let m = ApprovalsModule::new();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let regs = ctx.registered();
        assert!(regs.contains(&"sender_scope_gate"));
        assert!(regs.contains(&"delivery_action"));
        assert!(regs.contains(&"delivery_ready"), "approvals must register on_delivery_adapter_ready");
        assert_eq!(ctx.delivery_actions(), vec!["approval_card"]);
    }

    #[tokio::test]
    async fn scope_gate_defers_when_event_has_no_sender() {
        let m = ApprovalsModule::new();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: None,
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        assert!(decision.is_defer());
    }

    #[tokio::test]
    async fn scope_gate_pending_for_unknown_sender() {
        let m = ApprovalsModule::new();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: Some(SenderIdentity {
                channel_type: ChannelType::new("telegram"),
                identity: "u-9".into(),
                display_name: None,
            }),
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        assert!(decision.is_pending());
    }

    #[tokio::test]
    async fn scope_gate_defers_for_known_sender() {
        let m = ApprovalsModule::new();
        let sender = SenderIdentity {
            channel_type: ChannelType::new("slack"),
            identity: "U-1".into(),
            display_name: None,
        };
        m.approve_sender(sender.clone());
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: Some(sender),
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        assert!(decision.is_defer());
    }

    #[test]
    fn approval_card_handler_builds_card() {
        let handler = ApprovalCardHandler;
        let input = DeliveryActionInput {
            action: "approval_card".into(),
            payload: serde_json::json!({
                "approval_id": "abc-123",
                "title": "Please approve",
                "to": {
                    "channel_type": "slack",
                    "platform_id": "U-admin",
                    "thread_id": "T-9",
                },
            }),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: None,
            },
        };
        let out = handler.handle(input).unwrap();
        let dispatch = out.dispatch.unwrap();
        assert_eq!(
            dispatch.channel_type.as_ref().map(ChannelType::as_str),
            Some("slack")
        );
        assert_eq!(dispatch.platform_id.as_deref(), Some("U-admin"));
        assert_eq!(dispatch.thread_id.as_deref(), Some("T-9"));
        let msg = out.message.unwrap();
        let card = msg.content.get("card").unwrap();
        assert_eq!(card.get("approval_id").unwrap(), "abc-123");
        assert_eq!(card.get("title").unwrap(), "Please approve");
    }

    #[test]
    fn approval_card_handler_rejects_missing_id() {
        let handler = ApprovalCardHandler;
        let input = DeliveryActionInput {
            action: "approval_card".into(),
            payload: serde_json::json!({}),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: None,
            },
        };
        let err = handler.handle(input).unwrap_err();
        assert!(err.to_string().contains("approval_id"));
    }

    #[test]
    fn approval_card_handler_omits_dispatch_when_to_missing() {
        let handler = ApprovalCardHandler;
        let input = DeliveryActionInput {
            action: "approval_card".into(),
            payload: serde_json::json!({
                "approval_id": "x",
            }),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: None,
            },
        };
        let out = handler.handle(input).unwrap();
        assert!(out.dispatch.is_none());
        assert!(out.message.is_some());
    }

    #[test]
    fn summary_serde_roundtrip() {
        let s = summary(ApprovalKind::Channel);
        let json = serde_json::to_string(&s).unwrap();
        let back: ApprovalSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(ApprovalsModule::default().name(), "approvals");
    }

    // -----------------------------------------------------------------------
    // Notifier tests
    // -----------------------------------------------------------------------

    /// Helper: build a sender identity for tests.
    fn unknown_sender(channel: &str, id: &str) -> SenderIdentity {
        SenderIdentity {
            channel_type: ChannelType::new(channel),
            identity: id.into(),
            display_name: Some(format!("{channel}/{id}")),
        }
    }

    #[tokio::test]
    async fn notifier_fires_when_sender_is_pending() {
        use crate::context::MockDispatcher;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = Arc::clone(&call_count);

        let notifier: NewPendingNotifier = Arc::new(move |ctx, dispatcher| {
            call_count2.fetch_add(1, Ordering::SeqCst);
            // Post a synthetic dispatch so we can assert it was called.
            let target = DispatchTarget::channel(
                ctx.sender.channel_type.clone(),
                ctx.sender.identity.clone(),
                None,
            );
            let msg = OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({"text": "pending approval notice"}),
                files: vec![],
            };
            dispatcher.dispatch(&target, &msg);
        });

        let m = ApprovalsModule::new().with_new_pending_notifier(notifier);
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();

        // Seed a dispatcher.
        let mock_dispatcher = MockDispatcher::new();
        let d: Arc<dyn DeliveryDispatcher> = mock_dispatcher.clone();
        ctx.fire_delivery_ready(&d);

        // Trigger the gate for an unknown sender.
        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: Some(unknown_sender("telegram", "u-99")),
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        assert!(decision.is_pending());
        assert_eq!(call_count.load(Ordering::SeqCst), 1, "notifier must fire once");
        assert_eq!(mock_dispatcher.dispatched_count(), 1, "dispatch must have been called");
    }

    #[tokio::test]
    async fn notifier_does_not_fire_without_dispatcher() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = Arc::clone(&call_count);

        let notifier: NewPendingNotifier = Arc::new(move |_ctx, _dispatcher| {
            call_count2.fetch_add(1, Ordering::SeqCst);
        });

        let m = ApprovalsModule::new().with_new_pending_notifier(notifier);
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        // Intentionally do NOT call fire_delivery_ready.

        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: Some(unknown_sender("slack", "U-55")),
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        assert!(decision.is_pending());
        // Notifier must NOT have fired because no dispatcher was available.
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn notifier_does_not_fire_for_known_sender() {
        use crate::context::MockDispatcher;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count2 = Arc::clone(&call_count);

        let notifier: NewPendingNotifier =
            Arc::new(move |_ctx, _d| { call_count2.fetch_add(1, Ordering::SeqCst); });

        let sender = unknown_sender("discord", "D-1");
        let m = ApprovalsModule::new()
            .with_new_pending_notifier(notifier);
        m.approve_sender(sender.clone());

        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();

        let mock_dispatcher = MockDispatcher::new();
        let d: Arc<dyn DeliveryDispatcher> = mock_dispatcher.clone();
        ctx.fire_delivery_ready(&d);

        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: Some(sender),
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        // Known sender — gate defers, notifier must NOT fire.
        assert!(decision.is_defer());
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn notifier_receives_agent_group_and_messaging_group() {
        use crate::context::MockDispatcher;

        let captured: Arc<Mutex<Option<NewPendingCtx>>> = Arc::new(Mutex::new(None));
        let cap2 = Arc::clone(&captured);
        let notifier: NewPendingNotifier = Arc::new(move |ctx, _d| {
            *cap2.lock().unwrap() = Some(ctx);
        });

        let m = ApprovalsModule::new().with_new_pending_notifier(notifier);
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();

        let mock_dispatcher = MockDispatcher::new();
        let d: Arc<dyn DeliveryDispatcher> = mock_dispatcher;
        ctx.fire_delivery_ready(&d);

        let ag_id = AgentGroupId::new();
        let mg_id = MessagingGroupId::new();
        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        (gate)(SenderScopeCtx {
            event_sender: Some(unknown_sender("teams", "T-7")),
            messaging_group_id: Some(mg_id),
            agent_group_id: ag_id,
            resolved_user: None,
        });

        let c = captured.lock().unwrap().clone().unwrap();
        assert_eq!(c.agent_group_id, ag_id);
        assert_eq!(c.messaging_group_id, Some(mg_id));
        assert_eq!(c.sender.identity, "T-7");
    }

    #[tokio::test]
    async fn without_notifier_pending_decision_still_works() {
        // Ensure the module still functions correctly without a notifier.
        let m = ApprovalsModule::new(); // no notifier
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();

        let gate = ctx.sender_scope_gates.lock().unwrap()[0].clone();
        let decision = (gate)(SenderScopeCtx {
            event_sender: Some(unknown_sender("gchat", "space-1")),
            messaging_group_id: None,
            agent_group_id: AgentGroupId::new(),
            resolved_user: None,
        });
        assert!(decision.is_pending());
    }
}
