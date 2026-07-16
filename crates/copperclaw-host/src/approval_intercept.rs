//! In-chat approval interception (M18 G1).
//!
//! Approval cards go out carrying `approve:<id>` / `deny:<id>` button
//! callbacks. When an operator taps one, the adapter synthesizes a `Chat`
//! event with `content.callback.data`, whitelisted past the mention gate. The
//! router hands that event to the closure built here (registered on its
//! [`copperclaw_host_router::HookChain`] approval-interceptor slot) BEFORE the
//! mention gate, so the tap resolves the approval instead of landing in the
//! agent's transcript.
//!
//! The closure:
//!
//! 1. Recognises `approve:<id>` / `deny:<id>`; anything else passes through.
//! 2. Resolves the tapping identity to a central `users` row and checks it
//!    holds `Owner`/`Admin` — globally or scoped to the approval's agent group
//!    ([`handlers::roles`] infra). Non-approvers get a short "not authorized"
//!    reply, an audit row, and the card stays live.
//! 3. Resolves the approval through the SAME DB path the CLI uses
//!    ([`handlers::approvals::resolve_approve`] / `resolve_deny`), so the CLI
//!    and in-chat routes can never diverge and a race resolves once (first
//!    wins; the loser sees the row already terminal and no-ops).
//! 4. On a decision it actually applied, writes an audit row and edits the
//!    card in place to "Approved by <name>" / "Denied by <name>" using the
//!    `platform_message_id` persisted at delivery time.
//!
//! ## Approver identity decision
//!
//! We reuse the Owner/Admin roles infra (`user_roles`, surfaced via
//! `cclaw roles grant`) rather than "any registered sender in the primary
//! group". Rationale: the roles table is the project's existing
//! privilege-grant mechanism (the create-agent gate already keys off it), it
//! is per-user and audit-friendly, and it lets an operator authorise approvals
//! without also widening who may *talk* to the agent. A sender-scope pass is a
//! necessary precondition (only trusted senders reach the interceptor) but is
//! deliberately NOT sufficient — resolving an approval is a privileged act.

use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log::{self, AuditEntry};
use copperclaw_db::tables::user_roles::{self, Role};
use copperclaw_db::tables::{pending_approvals, users};
use copperclaw_modules::{
    ApprovalInterceptCtx, ApprovalInterceptDecision, ApprovalInterceptor, DeliveryDispatcher,
    DispatchTarget,
};
use copperclaw_types::{AgentGroupId, ApprovalId, MessageKind, OutboundMessage, UserId};
use std::sync::Arc;

/// Which button was tapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Approve,
    Deny,
}

impl Verb {
    /// Past-tense label stamped onto the resolved card.
    fn past(self) -> &'static str {
        match self {
            Verb::Approve => "Approved",
            Verb::Deny => "Denied",
        }
    }
}

/// Parse an approval callback payload (`approve:<id>` / `deny:<id>`). Returns
/// the verb + the raw id string, or `None` when the payload is some other
/// module's callback (the router then routes it normally).
fn parse_callback(data: &str) -> Option<(Verb, &str)> {
    if let Some(rest) = data.strip_prefix("approve:") {
        Some((Verb::Approve, rest))
    } else {
        data.strip_prefix("deny:").map(|rest| (Verb::Deny, rest))
    }
}

/// Resolve the tapping identity to an approver. Returns `Some((user_id,
/// display_name))` when the sender holds Owner/Admin (global or scoped to
/// `agent_group_id`); `None` otherwise. Resolution goes through the central
/// `users` table directly — the router wires no sender resolver, so we cannot
/// rely on `ctx.resolved_user` being populated.
fn resolve_approver(
    central: &CentralDb,
    agent_group_id: AgentGroupId,
    ctx: &ApprovalInterceptCtx,
) -> Option<(UserId, String)> {
    let sender = ctx.event_sender.as_ref()?;
    let user = users::get_by_identity(central, sender.channel_type.as_str(), &sender.identity)
        .ok()
        .flatten()?;
    let roles = user_roles::list_for_user(central, user.id).ok()?;
    let authorized = roles.iter().any(|r| {
        matches!(r.role, Role::Owner | Role::Admin)
            && (r.agent_group_id.is_none() || r.agent_group_id == Some(agent_group_id))
    });
    if !authorized {
        return None;
    }
    let name = sender
        .display_name
        .clone()
        .or(user.display_name)
        .unwrap_or_else(|| sender.identity.clone());
    Some((user.id, name))
}

/// Append an audit row for an in-chat approval action. Best-effort: a failure
/// to write the audit row must not abort the resolution.
fn audit(
    central: &CentralDb,
    agent_group_id: AgentGroupId,
    approval_id: ApprovalId,
    outcome: &str,
    result: &str,
    error_code: Option<&str>,
    actor: &str,
) {
    let entry = AuditEntry {
        ts: chrono::Utc::now(),
        caller_kind: "host".to_owned(),
        caller_session: None,
        caller_agent_group: Some(agent_group_id.as_uuid().to_string()),
        command: format!("approvals.{outcome}"),
        args: format!(
            "{{\"source\":\"in_chat\",\"approval_id\":\"{}\",\"actor\":\"{}\"}}",
            approval_id.as_uuid(),
            actor.replace('"', "'")
        ),
        result: result.to_owned(),
        error_code: error_code.map(str::to_owned),
        error_message: None,
        latency_ms: 0,
    };
    if let Err(err) = audit_log::insert(central, &entry) {
        tracing::warn!(?err, "approvals: failed to write in-chat audit row");
    }
}

/// Deliver a short plain-text reply on the tapping surface.
fn reply(dispatcher: &Arc<dyn DeliveryDispatcher>, ctx: &ApprovalInterceptCtx, text: &str) {
    let target = DispatchTarget::channel(
        ctx.channel_type.clone(),
        ctx.platform_id.clone(),
        ctx.thread_id.clone(),
    );
    let msg = OutboundMessage {
        kind: MessageKind::Chat,
        content: serde_json::json!({ "text": text }),
        files: vec![],
    };
    dispatcher.dispatch(&target, &msg);
}

/// Build the approval interceptor closure. Captures the central DB (for the
/// shared CLI resolution path + roles) and the delivery dispatcher (for the
/// card edit + refusal reply). Wired onto the router's hook chain at boot and
/// in the replay harness.
#[must_use]
pub fn build_approval_interceptor(
    central: CentralDb,
    dispatcher: Arc<dyn DeliveryDispatcher>,
) -> ApprovalInterceptor {
    Arc::new(move |ctx: ApprovalInterceptCtx| {
        let Some((verb, id_str)) = parse_callback(&ctx.callback_data) else {
            // Not an approval callback — let the router route it normally.
            return ApprovalInterceptDecision::Passthrough;
        };
        // From here on the tap is unambiguously ours: whatever the outcome we
        // consume it (never leak an approval button into the agent transcript).
        let Ok(uuid) = uuid::Uuid::parse_str(id_str.trim()) else {
            tracing::warn!(data = %ctx.callback_data, "approvals: malformed approval callback id");
            return ApprovalInterceptDecision::Handled;
        };
        let approval_id = ApprovalId(uuid);
        let agent_group_id = ctx.agent_group_id;

        // Authorisation: only Owner/Admin (global or group-scoped) may resolve.
        let Some((_uid, approver)) = resolve_approver(&central, agent_group_id, &ctx) else {
            audit(
                &central,
                agent_group_id,
                approval_id,
                verb_label(verb),
                "error",
                Some("unauthorized"),
                ctx.event_sender
                    .as_ref()
                    .map_or("unknown", |s| s.identity.as_str()),
            );
            reply(
                &dispatcher,
                &ctx,
                "You are not authorized to resolve this approval.",
            );
            return ApprovalInterceptDecision::Handled;
        };

        // Resolve through the SAME DB path the CLI uses.
        let result = match verb {
            Verb::Approve => {
                crate::handlers::approvals::resolve_approve(&central, approval_id, &approver)
            }
            Verb::Deny => {
                crate::handlers::approvals::resolve_deny(&central, approval_id, &approver)
            }
        };

        match result {
            Ok(value) => {
                let applied = value
                    .get("applied")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if applied {
                    audit(
                        &central,
                        agent_group_id,
                        approval_id,
                        verb_label(verb),
                        "ok",
                        None,
                        &approver,
                    );
                    edit_card(&central, &dispatcher, &ctx, approval_id, verb, &approver);
                } else {
                    // Already resolved before this tap landed (CLI/in-chat
                    // race, or a double-tap). First resolution won; this is a
                    // deliberate no-op — no second edit, no error surfaced.
                    tracing::info!(
                        approval_id = %approval_id.as_uuid(),
                        "approvals: in-chat tap on an already-resolved approval; no-op"
                    );
                }
                ApprovalInterceptDecision::Handled
            }
            Err(err) => {
                // `conflict` (row already denied/approved/expired) or
                // `not_found` (swept). Either way the request is settled: the
                // race loser must not crash and must not double-resolve.
                tracing::info!(
                    approval_id = %approval_id.as_uuid(),
                    code = %err.code,
                    "approvals: in-chat resolution not applied"
                );
                if err.code == "not_found" {
                    reply(&dispatcher, &ctx, "That approval is no longer pending.");
                }
                ApprovalInterceptDecision::Handled
            }
        }
    })
}

fn verb_label(verb: Verb) -> &'static str {
    match verb {
        Verb::Approve => "approve",
        Verb::Deny => "deny",
    }
}

/// Edit the delivered approval card to its terminal "<Verb> by <name>" text.
/// Best-effort: a missing `platform_message_id` (card delivery never recorded
/// one) simply skips the edit — the DB state is already authoritative.
fn edit_card(
    central: &CentralDb,
    dispatcher: &Arc<dyn DeliveryDispatcher>,
    ctx: &ApprovalInterceptCtx,
    approval_id: ApprovalId,
    verb: Verb,
    approver: &str,
) {
    let pmid = match pending_approvals::get(central, approval_id) {
        Ok(row) => row.platform_message_id,
        Err(err) => {
            tracing::warn!(?err, "approvals: could not reload row for card edit");
            None
        }
    };
    let Some(pmid) = pmid else {
        tracing::info!(
            approval_id = %approval_id.as_uuid(),
            "approvals: no platform_message_id recorded; skipping card edit"
        );
        return;
    };
    let target = DispatchTarget::channel(
        ctx.channel_type.clone(),
        ctx.platform_id.clone(),
        ctx.thread_id.clone(),
    );
    let text = format!("{} by {approver}", verb.past());
    dispatcher.edit_message(&target, &pmid, &text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::pending_approvals::{
        ApprovalStatus, UpsertPendingApproval, get as get_row, upsert,
    };
    use copperclaw_db::tables::users::{UpsertUser, upsert as upsert_user};
    use copperclaw_modules::context::MockDispatcher;
    use copperclaw_types::{ChannelType, SenderIdentity};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn seed_ag(db: &CentralDb) -> AgentGroupId {
        // Unique folder per call — `agent_groups.folder` is UNIQUE.
        let slug = uuid::Uuid::new_v4().simple().to_string();
        create_ag(
            db,
            CreateAgentGroup {
                name: format!("g-{slug}"),
                folder: format!("g-{slug}"),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    /// Insert a registered user and (optionally) grant them a role.
    fn seed_user(db: &CentralDb, identity: &str, role: Option<Role>, scope: Option<AgentGroupId>) {
        let u = upsert_user(
            db,
            UpsertUser {
                kind: "telegram".into(),
                identity: identity.into(),
                display_name: Some(format!("user-{identity}")),
            },
        )
        .unwrap();
        if let Some(r) = role {
            user_roles::grant(db, u.id, r, scope, None).unwrap();
        }
    }

    fn seed_sender_pending(db: &CentralDb, ag: AgentGroupId) -> ApprovalId {
        upsert(
            db,
            UpsertPendingApproval {
                request_id: "req-1".into(),
                action: "sender".into(),
                payload: serde_json::json!({}),
                agent_group_id: Some(ag),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("100".into()),
                platform_message_id: Some("card-msg-1".into()),
                title: "Approve sender?".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap()
        .approval_id
    }

    fn ctx(
        id: ApprovalId,
        verb: &str,
        ag: AgentGroupId,
        sender_identity: &str,
    ) -> ApprovalInterceptCtx {
        ApprovalInterceptCtx {
            callback_data: format!("{verb}:{}", id.as_uuid()),
            event_sender: Some(SenderIdentity {
                channel_type: ChannelType::new("telegram"),
                identity: sender_identity.into(),
                display_name: Some("Tapper".into()),
            }),
            resolved_user: None,
            agent_group_id: ag,
            messaging_group_id: None,
            channel_type: ChannelType::new("telegram"),
            platform_id: "100".into(),
            thread_id: None,
        }
    }

    #[test]
    fn parse_callback_recognises_verbs() {
        assert_eq!(parse_callback("approve:abc"), Some((Verb::Approve, "abc")));
        assert_eq!(parse_callback("deny:xyz"), Some((Verb::Deny, "xyz")));
        assert!(parse_callback("expand:42").is_none());
        assert!(parse_callback("approve").is_none());
    }

    #[test]
    fn approver_tap_resolves_edits_and_audits() {
        let db = db();
        let ag = seed_ag(&db);
        seed_user(&db, "owner-1", Some(Role::Owner), None);
        let id = seed_sender_pending(&db, ag);
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);

        let decision = interceptor(ctx(id, "approve", ag, "owner-1"));
        assert_eq!(decision, ApprovalInterceptDecision::Handled);

        // DB resolved via the shared path.
        assert_eq!(get_row(&db, id).unwrap().status, ApprovalStatus::Approved);
        // Decision log names the approver, not "host".
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].decided_by, "Tapper");
        // Audit row written (ok).
        let audits =
            audit_log::list_recent(&db, chrono::Utc::now() - chrono::Duration::hours(1), 50)
                .unwrap();
        assert!(
            audits
                .iter()
                .any(|a| a.command == "approvals.approve" && a.result == "ok")
        );
        // Card edited in place.
        let edits = mock.edits.lock().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1, "card-msg-1");
        assert_eq!(edits[0].2, "Approved by Tapper");
    }

    #[test]
    fn stranger_tap_refused_and_audited_card_stays_live() {
        let db = db();
        let ag = seed_ag(&db);
        // Registered sender but NO role grant.
        seed_user(&db, "stranger", None, None);
        let id = seed_sender_pending(&db, ag);
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);

        let decision = interceptor(ctx(id, "approve", ag, "stranger"));
        assert_eq!(decision, ApprovalInterceptDecision::Handled);

        // Row stays live (pending), no decision logged.
        assert_eq!(get_row(&db, id).unwrap().status, ApprovalStatus::Pending);
        assert!(
            pending_approvals::list_decisions(&db, Some(id), 10)
                .unwrap()
                .is_empty()
        );
        // Audit records the unauthorized attempt.
        let audits =
            audit_log::list_recent(&db, chrono::Utc::now() - chrono::Duration::hours(1), 50)
                .unwrap();
        assert!(
            audits
                .iter()
                .any(|a| a.result == "error" && a.error_code.as_deref() == Some("unauthorized"))
        );
        // A "not authorized" reply was dispatched; no card edit.
        assert_eq!(mock.dispatched_count(), 1);
        assert_eq!(mock.edit_count(), 0);
    }

    #[test]
    fn group_scoped_admin_authorised() {
        let db = db();
        let ag = seed_ag(&db);
        seed_user(&db, "admin-scoped", Some(Role::Admin), Some(ag));
        let id = seed_sender_pending(&db, ag);
        let dispatcher: Arc<dyn DeliveryDispatcher> = MockDispatcher::new();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);
        interceptor(ctx(id, "deny", ag, "admin-scoped"));
        assert_eq!(get_row(&db, id).unwrap().status, ApprovalStatus::Denied);
    }

    #[test]
    fn admin_scoped_to_other_group_not_authorised() {
        let db = db();
        let ag = seed_ag(&db);
        let other = seed_ag(&db);
        seed_user(&db, "admin-other", Some(Role::Admin), Some(other));
        let id = seed_sender_pending(&db, ag);
        let dispatcher: Arc<dyn DeliveryDispatcher> = MockDispatcher::new();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);
        interceptor(ctx(id, "approve", ag, "admin-other"));
        assert_eq!(get_row(&db, id).unwrap().status, ApprovalStatus::Pending);
    }

    #[test]
    fn non_approval_callback_passes_through() {
        let db = db();
        let ag = seed_ag(&db);
        let dispatcher: Arc<dyn DeliveryDispatcher> = MockDispatcher::new();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);
        let mut c = ctx(ApprovalId::new(), "approve", ag, "x");
        c.callback_data = "expand:42".into();
        assert_eq!(interceptor(c), ApprovalInterceptDecision::Passthrough);
    }

    #[test]
    fn cli_then_chat_race_is_no_op_second() {
        let db = db();
        let ag = seed_ag(&db);
        seed_user(&db, "owner-1", Some(Role::Owner), None);
        let id = seed_sender_pending(&db, ag);
        // CLI approves first (decided_by = "host").
        crate::handlers::approvals::resolve_approve(&db, id, "host").unwrap();
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);
        // In-chat tap arrives second: handled, but a no-op (already approved).
        let decision = interceptor(ctx(id, "approve", ag, "owner-1"));
        assert_eq!(decision, ApprovalInterceptDecision::Handled);
        // Still exactly one decision (the CLI's), no second edit.
        let decs = pending_approvals::list_decisions(&db, Some(id), 10).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].decided_by, "host");
        assert_eq!(mock.edit_count(), 0);
    }
}
