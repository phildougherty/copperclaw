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

        // F3(c): opportunistically stamp any card whose TTL lapsed terminal, so
        // a silently-expired approval never lingers with live buttons. This is
        // the one host surface that both runs on approval activity and holds the
        // delivery dispatcher; a periodic sweep is a lane-H follow-up.
        let expired = crate::handlers::approvals::expire_and_edit_cards(&central, &dispatcher);
        if expired > 0 {
            tracing::info!(expired, "approvals: stamped expired approval cards on tap");
        }

        // Authorisation: only Owner/Admin (global or group-scoped) may resolve.
        let Some((_uid, approver)) = resolve_approver(&central, agent_group_id, &ctx) else {
            copperclaw_metrics::inc_approval_tap("unauthorized");
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
                    copperclaw_metrics::inc_approval_tap(match verb {
                        Verb::Approve => "approved",
                        Verb::Deny => "denied",
                    });
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
                    // race, or a double-tap) with the SAME verb the winner used.
                    // First resolution won; F3(b): tell the losing tapper who
                    // resolved it instead of leaving their tap looking broken.
                    copperclaw_metrics::inc_approval_tap("conflict_notified");
                    tracing::info!(
                        approval_id = %approval_id.as_uuid(),
                        "approvals: in-chat tap on an already-resolved approval; notifying loser"
                    );
                    reply(&dispatcher, &ctx, &resolved_note(&central, approval_id));
                }
                ApprovalInterceptDecision::Handled
            }
            Err(err) => {
                // `conflict` (row already denied/approved/expired via the
                // OPPOSITE verb, or lapsed) or `not_found` (swept entirely).
                // Either way the request is settled: the race loser must not
                // crash and must not double-resolve.
                tracing::info!(
                    approval_id = %approval_id.as_uuid(),
                    code = %err.code,
                    "approvals: in-chat resolution not applied"
                );
                match err.code.as_str() {
                    // F3(b): the row settled under a conflicting decision — name
                    // the resolver (or say it expired) rather than staying mute.
                    "conflict" => {
                        copperclaw_metrics::inc_approval_tap("conflict_notified");
                        reply(&dispatcher, &ctx, &resolved_note(&central, approval_id));
                    }
                    "not_found" => {
                        copperclaw_metrics::inc_approval_tap("conflict_notified");
                        reply(&dispatcher, &ctx, "That approval is no longer pending.");
                    }
                    _ => {
                        copperclaw_metrics::inc_approval_tap("race_noop");
                    }
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

/// Stamp the delivered approval card with its terminal "<Verb> by <name>"
/// state so it never keeps showing live Approve/Deny buttons after resolution.
///
/// F3(a): the card is edited in place when a `platform_message_id` was recorded
/// at delivery. When it was NOT — the delivering adapter returned no id, or the
/// card degraded to a plain-text fallback with no editable anchor — we do NOT
/// silently skip (the old bug, which left live buttons). Instead we post the
/// resolution as a short follow-up reply on the tapping surface, so the outcome
/// is always visible even on the fallback-id path.
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
    let text = format!("{} by {approver}", verb.past());
    let target = DispatchTarget::channel(
        ctx.channel_type.clone(),
        ctx.platform_id.clone(),
        ctx.thread_id.clone(),
    );
    if let Some(pmid) = pmid {
        dispatcher.edit_message(&target, &pmid, &text);
    } else {
        // Fallback-id path: no editable anchor was recorded at delivery. Post
        // the resolution as a follow-up reply rather than leaving the card live.
        tracing::info!(
            approval_id = %approval_id.as_uuid(),
            "approvals: no platform_message_id recorded; posting resolution as a follow-up reply"
        );
        copperclaw_metrics::inc_approval_tap("resolved_fallback_reply");
        reply(dispatcher, ctx, &text);
    }
}

/// Build the "already resolved" note shown to a losing tapper (F3b). Reads the
/// most recent decision so it names the human (or system) that actually settled
/// the request; an expiry gets a distinct, actionable line.
fn resolved_note(central: &CentralDb, id: ApprovalId) -> String {
    use copperclaw_db::tables::pending_approvals::DecisionOutcome;
    match pending_approvals::list_decisions(central, Some(id), 1) {
        Ok(decs) => match decs.first() {
            Some(d) if d.outcome == DecisionOutcome::Expire => {
                "This approval request expired before it was resolved. Ask the agent to try again."
                    .to_owned()
            }
            Some(d) => format!("This request was already resolved by {}.", d.decided_by),
            None => "That approval is no longer pending.".to_owned(),
        },
        Err(err) => {
            tracing::warn!(
                ?err,
                "approvals: could not read decision log for loser note"
            );
            "That approval is no longer pending.".to_owned()
        }
    }
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

    /// Seed a pending sender-approval with an explicit `platform_message_id`
    /// (None models a card the delivering adapter reported no id for) and an
    /// optional explicit deadline.
    fn seed_pending_with(
        db: &CentralDb,
        ag: AgentGroupId,
        req: &str,
        pmid: Option<&str>,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> ApprovalId {
        upsert(
            db,
            UpsertPendingApproval {
                request_id: req.into(),
                action: "sender".into(),
                payload: serde_json::json!({}),
                agent_group_id: Some(ag),
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("100".into()),
                platform_message_id: pmid.map(str::to_owned),
                expires_at,
                title: "Approve sender?".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap()
        .approval_id
    }

    #[test]
    fn approver_tap_falls_back_to_reply_when_no_platform_message_id() {
        // F3(a): the delivering adapter recorded no platform_message_id (the
        // old silent-skip bug left live buttons). The tap must still surface
        // the resolution — as a follow-up reply — not vanish.
        let db = db();
        let ag = seed_ag(&db);
        seed_user(&db, "owner-1", Some(Role::Owner), None);
        let id = seed_pending_with(&db, ag, "req-nopmid", None, None);
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);

        interceptor(ctx(id, "approve", ag, "owner-1"));

        // Resolved in the DB, but there is no card to edit...
        assert_eq!(get_row(&db, id).unwrap().status, ApprovalStatus::Approved);
        assert_eq!(mock.edit_count(), 0, "no editable card");
        // ...so the resolution lands as a follow-up reply instead.
        let sent = mock.dispatched.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1.content["text"], "Approved by Tapper");
    }

    #[test]
    fn conflict_loser_same_verb_is_told_who_resolved() {
        // F3(b): CLI approved first; a chat Approve tap arrives second (same
        // verb → Ok{applied:false}). The loser must be told, not left silent.
        let db = db();
        let ag = seed_ag(&db);
        seed_user(&db, "owner-1", Some(Role::Owner), None);
        let id = seed_sender_pending(&db, ag);
        crate::handlers::approvals::resolve_approve(&db, id, "host").unwrap();
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);

        interceptor(ctx(id, "approve", ag, "owner-1"));

        assert_eq!(mock.edit_count(), 0, "winner already stamped; no re-edit");
        let sent = mock.dispatched.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].1.content["text"],
            "This request was already resolved by host."
        );
    }

    #[test]
    fn conflict_loser_opposite_verb_is_told_who_resolved() {
        // F3(b): CLI approved first; a chat Deny tap arrives second (opposite
        // verb → Err{conflict}). Same "already resolved by <name>" reply.
        let db = db();
        let ag = seed_ag(&db);
        seed_user(&db, "owner-1", Some(Role::Owner), None);
        let id = seed_sender_pending(&db, ag);
        crate::handlers::approvals::resolve_approve(&db, id, "host").unwrap();
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);

        interceptor(ctx(id, "deny", ag, "owner-1"));

        let sent = mock.dispatched.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].1.content["text"],
            "This request was already resolved by host."
        );
    }

    #[test]
    fn expired_card_is_stamped_terminal_on_a_later_tap() {
        // F3(c): a separate approval lapsed its TTL with a live card. When any
        // approval tap runs the interceptor, the opportunistic sweep stamps the
        // stale card terminal (no live buttons left dangling).
        let db = db();
        let ag = seed_ag(&db);
        seed_user(&db, "owner-1", Some(Role::Owner), None);
        // Stale: overdue, has a delivered card.
        let stale = seed_pending_with(
            &db,
            ag,
            "req-stale",
            Some("stale-card"),
            Some(chrono::Utc::now() - chrono::Duration::minutes(5)),
        );
        // Live: the approval actually being tapped.
        let live = seed_sender_pending(&db, ag);

        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let interceptor = build_approval_interceptor(db.clone(), dispatcher);

        interceptor(ctx(live, "approve", ag, "owner-1"));

        // The stale row lapsed and its card was stamped terminal.
        assert_eq!(get_row(&db, stale).unwrap().status, ApprovalStatus::Expired);
        let edits = mock.edits.lock().unwrap();
        let stale_edit = edits
            .iter()
            .find(|e| e.1 == "stale-card")
            .expect("card edit");
        assert!(stale_edit.2.contains("expired"));
        // The tapped approval resolved normally.
        assert_eq!(get_row(&db, live).unwrap().status, ApprovalStatus::Approved);
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
