//! Pending approval workflow.
//!
//! Two hooks are wired:
//!
//! 1. `set_sender_scope_gate` — when an inbound event comes from an unknown
//!    sender, the gate returns [`SenderScopeDecision::Pending`] so the host
//!    will record a `pending_sender_approvals` row and drop the event.
//!
//! 2. `register_delivery_action("approval_card")` — when an agent emits an
//!    outbound system message requesting an approval card, this handler
//!    builds a structured card payload pointed at an approver and returns it
//!    via [`DeliveryActionOutput::message`].

use crate::context::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput, DispatchTarget, Module,
    ModuleContext, SenderScopeCtx, SenderScopeDecision,
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

/// Approvals module.
pub struct ApprovalsModule {
    /// In-memory pending list, fed by the host via `record_pending` calls.
    store: Arc<Mutex<PendingStore>>,
    /// Set of `(channel_type, identity)` tuples that are already approved.
    known_senders: Arc<Mutex<Vec<SenderIdentity>>>,
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
        }
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
        ctx.set_sender_scope_gate(Arc::new(move |s: SenderScopeCtx| {
            let Some(sender) = &s.event_sender else {
                return SenderScopeDecision::Defer;
            };
            let known = known.lock().unwrap();
            if known
                .iter()
                .any(|k| k.channel_type == sender.channel_type && k.identity == sender.identity)
            {
                SenderScopeDecision::Defer
            } else {
                SenderScopeDecision::Pending(format!(
                    "sender `{}:{}` is pending approval",
                    sender.channel_type, sender.identity
                ))
            }
        }));
        ctx.register_delivery_action("approval_card", Arc::new(ApprovalCardHandler));
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
    async fn install_registers_scope_gate_and_action() {
        let m = ApprovalsModule::new();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let regs = ctx.registered();
        assert!(regs.contains(&"sender_scope_gate"));
        assert!(regs.contains(&"delivery_action"));
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
}
