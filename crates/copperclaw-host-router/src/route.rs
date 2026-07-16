//! Routing algorithm — turn an `InboundEvent` into one or more
//! `messages_in` writes.
//!
//! See `PLAN.md` § 6 T3 for the high-level description. The routing pipeline
//! lives in [`Router::route`]; the supporting types in this module describe
//! the outcome the caller (typically a channel adapter) sees.

use crate::commands::{self, SlashCommand};
use crate::debounce::{DebounceKey, Debouncer, InflightKey, InflightSet};
use crate::error::RouterError;
use crate::hooks::HookChain;
use crate::mention::{MentionDecision, MentionGate};
use crate::session::SessionRoot;

use copperclaw_channels_core::inbound_file::{
    ATTACHMENT_PATH_KEY, FALLBACK_FILENAME, STAGED_PATH_KEY, container_inbox_path,
    remove_staged_file, sanitize_filename,
};
use copperclaw_db::attachments::extract_to_inbox;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::dropped_messages::{InsertDroppedMessage, insert as insert_dropped};
use copperclaw_db::tables::messages_in::{WriteInbound, count_due, insert as insert_in};
use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_out};
use copperclaw_db::tables::messaging_group_agents::{
    MessagingGroupAgent, list_for_mg as list_wirings,
};
use copperclaw_db::tables::messaging_groups::get_by_platform;
use copperclaw_db::tables::sessions::{CreateSession, create as create_session, find_for_agent};
use copperclaw_db::tables::unregistered_senders::{
    UpsertUnregisteredSender, upsert as upsert_unregistered,
};
use copperclaw_modules::context::{
    ApprovalInterceptCtx, ApprovalInterceptDecision, ChannelRequestCtx, GateCtx, GateDecision,
    SenderScopeCtx, SenderScopeDecision,
};
use copperclaw_types::{
    AgentGroupId, InboundEvent, MessageId, MessageKind, MessagingGroupId, SessionId, SessionMode,
};
use dashmap::DashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;
use tokio::sync::Notify;

/// Outcome of routing an inbound event. The caller (a channel adapter or
/// test harness) gets one of these per call and uses it for logging and to
/// decide whether to ack the platform message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Inbound event was routed to one or more sessions. `sessions` holds the
    /// per-target write result.
    Delivered { sessions: Vec<DeliveredTo> },
    /// Event was a host-answered slash command (`/status`): no
    /// `messages_in` row was written and the runner was never woken —
    /// the router synthesized a reply from host state and wrote it
    /// straight into the session's `messages_out` for the delivery
    /// loop. `message_id` / `seq` on each [`DeliveredTo`] refer to that
    /// OUTBOUND row (odd container-parity seq), not an inbound row.
    Answered { sessions: Vec<DeliveredTo> },
    /// Event was rejected before any write happened.
    Dropped { reason: DropReason },
    /// Event is awaiting an out-of-band approval; no inbound row was written
    /// but the platform should still ack so the user isn't shown an error.
    Pending { reason: PendingReason },
}

/// Single per-session delivery record returned by [`Router::route`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredTo {
    pub agent_group_id: AgentGroupId,
    pub session_id: SessionId,
    pub message_id: MessageId,
    pub seq: i64,
}

/// Reasons the router refused to deliver an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// No `messaging_groups` row matches `(channel_type, platform_id)`.
    NoMessagingGroup,
    /// A messaging group exists but no agents are wired to it.
    NoAgents,
    /// The access gate denied the event with the given reason string.
    AccessDenied(String),
    /// An interceptor (e.g. typing module's mute) explicitly dropped the
    /// inbound representation. Currently unused on the inbound path but
    /// reserved so the host can re-use the same `RouteOutcome` shape.
    InterceptorDropped(String),
    /// The event was a duplicate within the debounce window.
    Debounced,
    /// The router refused to write the row because the target session is
    /// the same one that produced the event (re-entry guard).
    ReentryGuard,
    /// The event was ambient group-chat text that did not engage the agent
    /// (no mention, no reply-to-self, no pattern match) and the wiring's
    /// venue requires a mention. See [`crate::mention`].
    MentionGated,
}

/// Reasons the router deferred the event without writing a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingReason {
    /// The sender resolver returned `None` (no `users` row); the
    /// `unregistered_senders` table has been updated.
    SenderUnregistered,
    /// The channel-request gate asked the host to defer until the operator
    /// approves; the inbound row was not written.
    ChannelRequestPending(String),
    /// The event was an in-chat approval button tap (`approve:<id>` /
    /// `deny:<id>`) that the approvals interceptor consumed (resolved the
    /// approval, refused a non-approver, or a race no-op). No inbound row was
    /// written and the runner was not woken; the platform should still ack the
    /// tap so the user isn't shown an error. (M18 G1.)
    ApprovalHandled,
}

/// Inbound router. Resolves messaging groups, fans out per wiring, writes
/// `messages_in` rows.
pub struct Router {
    central: CentralDb,
    session_paths: Arc<dyn SessionRoot + Send + Sync>,
    hooks: HookChain,
    debounce: Debouncer,
    inflight: InflightSet,
    fanout_seq: AtomicI64,
    mention_gate: MentionGate,
    /// Signalled after every successful `messages_in` insert. The host's
    /// container manager awaits this handle (via [`Self::inbound_wake`]) so a
    /// message to an idle/stopped session triggers an immediate reconcile
    /// tick instead of waiting out the poll interval. `Notify` semantics
    /// coalesce bursts into a single stored permit — N inserts while the
    /// manager is mid-tick wake it exactly once more. Purely an accelerator:
    /// nobody is required to listen, and the manager's polling loop remains
    /// the crash-safe fallback.
    inbound_wake: Arc<Notify>,
}

impl Router {
    /// Construct a router with the default mention policy (require a mention
    /// in group chats; never in DMs). Modules wire their hooks via
    /// [`Self::hooks`] / [`Self::hooks_mut`] after construction; the router
    /// stays usable while no hooks are wired (defaults to allow-all).
    ///
    /// To override the mention policy at boot, chain
    /// [`Self::with_mention_gate`] BEFORE wrapping the router in an `Arc`:
    /// the gate is construction-time config, never a `&mut self` setter on
    /// the shared `Arc<Router>`.
    pub fn new(central: CentralDb, session_paths: Arc<dyn SessionRoot + Send + Sync>) -> Self {
        Self {
            central,
            session_paths,
            hooks: HookChain::new(),
            debounce: Debouncer::new(),
            inflight: InflightSet::new(),
            fanout_seq: AtomicI64::new(0),
            mention_gate: MentionGate::default(),
            inbound_wake: Arc::new(Notify::new()),
        }
    }

    /// Set the mention-gating policy at construction time. Consumes and
    /// returns `self` so the host can write
    /// `Arc::new(Router::new(..).with_mention_gate(gate))` — the gate is
    /// fixed before the router is shared, never mutated through the `Arc`.
    #[must_use]
    pub fn with_mention_gate(mut self, gate: MentionGate) -> Self {
        self.mention_gate = gate;
        self
    }

    /// Borrow the configured mention-gating policy.
    pub fn mention_gate(&self) -> &MentionGate {
        &self.mention_gate
    }

    /// Borrow the router's hook chain.
    pub fn hooks(&self) -> &HookChain {
        &self.hooks
    }

    /// Mutable accessor for the hook chain. Kept distinct from [`Self::hooks`]
    /// so test scaffolding can statically distinguish the "I'm wiring" path
    /// from the "I'm running" path.
    pub fn hooks_mut(&mut self) -> &mut HookChain {
        &mut self.hooks
    }

    /// Borrow the central DB handle. Cheap to clone.
    pub fn central(&self) -> &CentralDb {
        &self.central
    }

    /// Borrow the `SessionRoot` adapter.
    pub fn session_paths(&self) -> &Arc<dyn SessionRoot + Send + Sync> {
        &self.session_paths
    }

    /// Borrow the underlying debounce map.
    pub fn debounce(&self) -> &Arc<DashMap<DebounceKey, Instant>> {
        self.debounce.inner()
    }

    /// Borrow the underlying in-flight set.
    pub fn inflight(&self) -> &Arc<DashMap<InflightKey, ()>> {
        self.inflight.inner()
    }

    /// Clone the inbound-wake handle. The router calls
    /// `notify_one` on it after every successful `messages_in` insert; the
    /// host hands the clone to the container manager so its reconcile loop
    /// can `notified().await` alongside its poll timer and react to new
    /// inbound immediately. See the field docs on `inbound_wake` for the
    /// coalescing / fallback contract.
    #[must_use]
    pub fn inbound_wake(&self) -> Arc<Notify> {
        Arc::clone(&self.inbound_wake)
    }

    /// Snapshot of the per-fanout counter — primarily for diagnostics in the
    /// host's `cclaw router stats` rendering.
    pub fn fanout_count(&self) -> i64 {
        self.fanout_seq.load(Ordering::Relaxed)
    }

    /// Route an inbound event. See module docs and PLAN § 6 T3.
    ///
    /// Errors here are reserved for "the router failed to reach its target
    /// state" — sqlite couldn't write, the session directory couldn't be
    /// created, a hook panicked. Everything that's part of the normal
    /// happy / sad path lives in [`RouteOutcome`].
    ///
    /// Note: this fn is `async` for forward compatibility with hook closures
    /// that need to issue I/O. The current pipeline is synchronous; clippy's
    /// `unused_async` lint is suppressed accordingly.
    #[allow(clippy::unused_async)]
    pub async fn route(&self, event: InboundEvent) -> Result<RouteOutcome, RouterError> {
        // Inbound-file contract (M18 C3): whatever the route outcome —
        // delivered, dropped, debounced, pending, or an error — the
        // adapter's staged attachment file is consumed here. Successful
        // fanouts copy the bytes into each target session's inbox before
        // this cleanup runs (see `materialized_content`).
        let staged = staged_attachment_path(&event);
        let outcome = self.route_impl(&event).await;
        if let Some(path) = staged {
            remove_staged_file(&path);
        }
        outcome
    }

    // Same forward-compatibility rationale as `route`'s `#[allow]` above:
    // kept `async` for hook closures that may need to issue I/O later.
    #[allow(clippy::unused_async)]
    async fn route_impl(&self, event: &InboundEvent) -> Result<RouteOutcome, RouterError> {
        // 1. Debounce.
        let dkey = DebounceKey {
            channel_type: event.channel_type.clone(),
            platform_id: event.platform_id.clone(),
            thread_id: event.thread_id.clone(),
            message_id: event.message.id.clone(),
        };
        if self.debounce.check_and_record(dkey, Instant::now()) {
            return Ok(RouteOutcome::Dropped {
                reason: DropReason::Debounced,
            });
        }

        // 2. Resolve messaging group.
        let Some(mg) = get_by_platform(&self.central, &event.channel_type, &event.platform_id)?
        else {
            self.record_drop(event, None, None, "no_messaging_group")?;
            return Ok(RouteOutcome::Dropped {
                reason: DropReason::NoMessagingGroup,
            });
        };

        // 3. Resolve sender to a UserId (if any).
        let user_id = self.hooks.run_sender_resolver(event);

        // 4. Optional channel-request gate (applies before the per-wiring
        //    fanout because a Deny / Pending here applies to all agents).
        if self.hooks.has_channel_request_gate() {
            let ctx = ChannelRequestCtx {
                channel_type: event.channel_type.clone(),
                platform_id: event.platform_id.clone(),
                thread_id: event.thread_id.clone(),
                requester: user_id,
                agent_group_id: None,
            };
            match self.hooks.run_channel_request_gate(ctx) {
                Some(GateDecision::Deny(reason)) => {
                    self.record_drop(event, Some(mg.id), None, &reason)?;
                    return Ok(RouteOutcome::Dropped {
                        reason: DropReason::AccessDenied(reason),
                    });
                }
                Some(GateDecision::Defer | GateDecision::Allow) | None => {}
            }
        }

        // 5. List wirings.
        let wirings = list_wirings(&self.central, mg.id)?;
        if wirings.is_empty() {
            self.record_drop(event, Some(mg.id), None, "no_agents")?;
            return Ok(RouteOutcome::Dropped {
                reason: DropReason::NoAgents,
            });
        }

        // 6. End-user slash-command detection (M18 R1). Detected once per
        //    event; every wiring below takes the same command path. See
        //    [`crate::commands`] for the per-command routing contract.
        let command = SlashCommand::detect(event);

        // 7. Fanout to each wiring.
        let mut sessions = Vec::with_capacity(wirings.len());
        let mut answered: Vec<DeliveredTo> = Vec::new();
        let mut last_pending: Option<PendingReason> = None;
        for wiring in wirings {
            self.fanout_seq.fetch_add(1, Ordering::Relaxed);
            match self.route_one(event, &mg.id, mg.is_group, &wiring, user_id, command)? {
                FanoutOutcome::Delivered(d) => sessions.push(d),
                FanoutOutcome::Answered(d) => answered.push(d),
                FanoutOutcome::Dropped(reason) => {
                    self.record_drop(
                        event,
                        Some(mg.id),
                        Some(wiring.agent_group_id),
                        &drop_reason_label(&reason),
                    )?;
                    if sessions.is_empty() && answered.is_empty() {
                        // First-and-only wiring dropped; propagate the drop.
                        return Ok(RouteOutcome::Dropped { reason });
                    }
                    // Multi-wiring case: continue and report only the
                    // wirings that actually delivered.
                }
                FanoutOutcome::Pending(reason) => {
                    last_pending = Some(reason);
                }
            }
        }

        if !sessions.is_empty() {
            return Ok(RouteOutcome::Delivered { sessions });
        }
        if !answered.is_empty() {
            return Ok(RouteOutcome::Answered { sessions: answered });
        }
        if let Some(reason) = last_pending {
            return Ok(RouteOutcome::Pending { reason });
        }
        // Defensive: should not happen — at least one fanout result
        // must classify the event.
        Ok(RouteOutcome::Dropped {
            reason: DropReason::NoAgents,
        })
    }

    /// Per-wiring delivery.
    fn route_one(
        &self,
        event: &InboundEvent,
        mg_id: &MessagingGroupId,
        mg_is_group: bool,
        wiring: &MessagingGroupAgent,
        user_id: Option<copperclaw_types::UserId>,
        command: Option<SlashCommand>,
    ) -> Result<FanoutOutcome, RouterError> {
        // Access gate per agent group. `resolve_access_gate` applies the
        // host default policy: privileged ops close-fail (a missing or
        // deferred decision denies), while non-privileged routing ops like
        // `deliver_message` keep open-fail. We still run it unconditionally so
        // that policy is uniform whether or not a gate is registered.
        {
            let ctx = GateCtx {
                user: user_id,
                agent_group_id: Some(wiring.agent_group_id),
                messaging_group_id: Some(*mg_id),
                op: "deliver_message".into(),
            };
            if let GateDecision::Deny(reason) = self.hooks.resolve_access_gate(ctx) {
                return Ok(FanoutOutcome::Dropped(DropReason::AccessDenied(reason)));
            }
        }

        // Sender-scope gate. When the sender is unregistered we record it in
        // `unregistered_senders` and let the gate decide whether to defer
        // the delivery (Pending) or pass it through (Allow / Defer).
        let scope_decision = if self.hooks.has_sender_scope_gate() {
            let ctx = SenderScopeCtx {
                event_sender: event.sender.clone(),
                messaging_group_id: Some(*mg_id),
                agent_group_id: wiring.agent_group_id,
                resolved_user: user_id,
            };
            self.hooks.run_sender_scope_gate(ctx)
        } else {
            None
        };

        if user_id.is_none() {
            if let Some(sender) = &event.sender {
                upsert_unregistered(
                    &self.central,
                    UpsertUnregisteredSender {
                        channel_type: sender.channel_type.clone(),
                        platform_id: sender.identity.clone(),
                        user_id: None,
                        sender_name: sender.display_name.clone(),
                        reason: scope_reason(scope_decision.as_ref()),
                        messaging_group_id: Some(*mg_id),
                        agent_group_id: Some(wiring.agent_group_id),
                    },
                )?;
            }
        }

        match &scope_decision {
            Some(SenderScopeDecision::Deny(reason)) => {
                return Ok(FanoutOutcome::Dropped(DropReason::AccessDenied(
                    reason.clone(),
                )));
            }
            Some(SenderScopeDecision::Pending(_reason)) => {
                return Ok(FanoutOutcome::Pending(PendingReason::SenderUnregistered));
            }
            Some(SenderScopeDecision::Allow | SenderScopeDecision::Defer) | None => {}
        }

        // In-chat approval interception (M18 G1). A button tap arrives as a
        // Chat event carrying `content.callback.data` (whitelisted past the
        // mention gate by `is_interaction_payload`). If the payload is an
        // `approve:<id>` / `deny:<id>` approval callback, the host-supplied
        // interceptor resolves it against the SAME DB path the CLI uses,
        // writes the audit row, and edits the card — all without ever writing
        // an inbound row or waking the runner. A non-approval callback (some
        // other module's button) falls through to normal routing.
        //
        // This sits AFTER the sender-scope gate (only senders the group
        // already trusts reach it) and BEFORE the mention gate, per the G1
        // insertion contract.
        if self.hooks.has_approval_interceptor() {
            if let Some(callback_data) = approval_callback_data(event) {
                let ctx = ApprovalInterceptCtx {
                    callback_data,
                    event_sender: event.sender.clone(),
                    resolved_user: user_id,
                    agent_group_id: wiring.agent_group_id,
                    messaging_group_id: Some(*mg_id),
                    channel_type: event.channel_type.clone(),
                    platform_id: event.platform_id.clone(),
                    thread_id: event.thread_id.clone(),
                };
                if let Some(ApprovalInterceptDecision::Handled) =
                    self.hooks.run_approval_interceptor(ctx)
                {
                    return Ok(FanoutOutcome::Pending(PendingReason::ApprovalHandled));
                }
            }
        }

        // Mention gate. Ambient group-chat text that doesn't engage the agent
        // (no @-mention, no reply-to-the-agent's-own-message, no pattern
        // match) is dropped per the wiring's venue policy. Non-text events
        // (callbacks, commands, tasks, agent-to-agent, …) and DMs are never
        // gated — that filtering lives entirely in `MentionGate::decide`.
        //
        // The venue is a group when EITHER the messaging-group row says so OR
        // the inbound event itself reports `is_group == Some(true)`; the OR
        // keeps a stale `messaging_groups.is_group` from silently un-gating a
        // real group chat.
        // Recognised slash commands bypass the gate entirely: issuing a
        // command IS a direct address, exactly like the adapter-stamped
        // `content.command` payloads `MentionGate::decide` already
        // whitelists. Raw-text commands (`/stop` typed into a gated
        // group) have no such stamp on the wire, so the router's own
        // detection stands in for it here.
        let is_group = mg_is_group || event.message.is_group == Some(true);
        if command.is_none() {
            if let MentionDecision::Drop(_label) = self.mention_gate.decide(
                event,
                is_group,
                wiring.engage_mode,
                wiring.engage_pattern.as_deref(),
            ) {
                // The caller records a `dropped_messages` row from the returned
                // `FanoutOutcome::Dropped` via `drop_reason_label` ("mention_gated").
                return Ok(FanoutOutcome::Dropped(DropReason::MentionGated));
            }
        }

        // Resolve the target session for this wiring.
        let session = self.resolve_session(event, mg_id, wiring)?;

        // Re-entry guard: refuse if the inbound carries a `source_session_id`
        // that equals the target session. (We never set source_session_id
        // ourselves at this layer; the agent-to-agent module's outbound
        // shim is what stamps it.)
        if let Some(src) = source_session_for(event) {
            if src == session.id.as_uuid().to_string() {
                return Ok(FanoutOutcome::Dropped(DropReason::ReentryGuard));
            }
        }

        let inflight_key = InflightKey {
            session_id: session.id.as_uuid().to_string(),
        };
        let _guard = self
            .inflight
            .enter(inflight_key)
            .ok_or_else(|| RouterError::invalid_wiring("re-entered in-flight session"))?;

        // R1: count detected slash commands by op + channel (before `/status`
        // returns early below, so status is counted too).
        if let Some(cmd) = command {
            copperclaw_metrics::inc_slash_command(cmd.op(), event.channel_type.as_str());
        }

        // `/status` is answered by the host: synthesize the reply from
        // central-DB state and write it straight to `messages_out`. No
        // inbound row is written and the runner is never woken.
        if command == Some(SlashCommand::Status) {
            let started = std::time::Instant::now();
            let answered = self.answer_status(event, &session);
            copperclaw_metrics::observe_status_answer_seconds(started.elapsed().as_secs_f64());
            return answered;
        }

        // Inbound-file contract (M18 C3): if the adapter staged an
        // attachment download (`content.attachment.staged_path`), copy the
        // bytes into THIS session's `inbox/<msg_id>/<safe_name>` and
        // rewrite the attachment to carry the container-visible `path`
        // (`/data/inbox/...`) before the row content is shaped below. The
        // staged source file itself is removed by `route` once every
        // wiring has been fanned out.
        let event_content = self.materialized_content(event, &session)?;

        // Open inbound.db and write the row.
        let pool = self
            .session_paths
            .inbound_pool(&session.agent_group_id, &session.id)?;
        let message_id = MessageId::new();
        // Pull the parent message id off `event.reply_to.thread_id` —
        // every adapter that populates `InboundEvent.reply_to` (Telegram,
        // Signal, ...) stuffs the parent's platform-side message id there
        // (the `platform_id` field on the `ReplyTo` struct is the *chat*
        // routing handle, which is the same as the inbound's own
        // `platform_id` for in-chat replies). Keeping just the parent
        // message id matches what the runner's context-block needs to
        // say "in reply to the user's earlier message" without lugging
        // the redundant chat handle into per-session storage.
        let reply_to_id = event.reply_to.as_ref().and_then(|r| r.thread_id.clone());
        // Per-command row shaping (see `crate::commands` for the contracts):
        //
        // - `/stop` persists a CONTROL row: `kind = system`, `trigger =
        //   false` (must not spawn a container — `count_due` filters on
        //   trigger, so the container manager's classifier ignores it),
        //   `content.control.op = "stop"`. The M18 R2 card consumes it
        //   mid-turn; the row lands even while a runner turn is in
        //   flight because the write path here never depends on runner
        //   state.
        // - `/compact` / `/clear` pass through as normal trigger rows
        //   with the text normalised to the canonical command so the
        //   runner's existing slash-command sentinel fires.
        // - Unrecognised text (including unknown `/x`) routes unchanged.
        let original_text = event
            .message
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let (kind, content, trigger) = match command {
            Some(cmd @ SlashCommand::Stop) => (
                MessageKind::System,
                commands::control_content(cmd, original_text),
                false,
            ),
            Some(cmd @ (SlashCommand::Compact | SlashCommand::Clear)) => (
                event.message.kind,
                commands::passthrough_content(cmd, &event_content, original_text),
                true,
            ),
            // `/status` returned above; everything else routes unchanged
            // (modulo attachment materialization above).
            Some(SlashCommand::Status) | None => (event.message.kind, event_content, true),
        };
        // M19 U7: a reaction is a lightweight steering signal, NOT a task. It
        // persists as a non-trigger row (like a `/stop` control row) so it
        // never spawns a container on its own — an idle reaction is a no-op,
        // and a reaction that lands mid-turn is consumed by the runner's R2
        // steering seam. This guarantees "a reaction never drives a spurious
        // full turn" (U7 acceptance) regardless of which message it targets.
        let trigger = trigger && !copperclaw_channels_core::is_reaction_content(&content);
        let write = WriteInbound {
            id: message_id,
            kind,
            timestamp: event.message.timestamp,
            content,
            trigger,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some(event.platform_id.clone()),
            channel_type: Some(event.channel_type.clone()),
            thread_id: event.thread_id.clone(),
            source_session_id: source_session_for(event),
            reply_to: reply_to_id,
            is_group: event.message.is_group,
        };
        let seq = pool.with_conn(|c| insert_in(c, &write))?;

        // R1: a `/stop` persists a CONTROL row for the runner to consume.
        if command == Some(SlashCommand::Stop) {
            copperclaw_metrics::inc_control_rows_written("stop");
        }

        copperclaw_metrics::inc_messages_inbound(event.channel_type.as_str());

        // Wake the container manager's reconcile loop so an idle/stopped
        // session spawns within ~one tick instead of waiting out the poll
        // interval. Best-effort accelerator: if nobody is awaiting the
        // handle, the permit is stored (and coalesced) by `Notify`.
        //
        // Control rows (`trigger = false`) skip the wake on purpose: the
        // manager's spawn classifier counts only trigger rows, so waking
        // it for a `/stop` against an idle session would be a guaranteed
        // no-op tick.
        if trigger {
            self.inbound_wake.notify_one();
        }

        Ok(FanoutOutcome::Delivered(DeliveredTo {
            agent_group_id: session.agent_group_id,
            session_id: session.id,
            message_id,
            seq,
        }))
    }

    /// Answer `/status` from host state: load the session + agent-group
    /// rows from the central DB, count the session's due inbound work,
    /// and write a synthesized chat reply straight into the session's
    /// `messages_out`. The delivery loop picks it up like any
    /// runner-emitted row; the runner itself is never involved. The
    /// reply carries the originating event's channel routing explicitly
    /// so delivery does not depend on `session_routing`.
    fn answer_status(
        &self,
        event: &InboundEvent,
        session: &TargetSession,
    ) -> Result<FanoutOutcome, RouterError> {
        let full = copperclaw_db::tables::sessions::get(&self.central, session.id)?;
        let group =
            copperclaw_db::tables::agent_groups::get(&self.central, session.agent_group_id)?;
        let inbound = self
            .session_paths
            .inbound_pool(&session.agent_group_id, &session.id)?;
        let queued = inbound.with_conn(count_due)?;
        let text = commands::render_status(&group.name, &full, queued);

        let outbound = self
            .session_paths
            .outbound_pool(&session.agent_group_id, &session.id)?;
        let message_id = MessageId::new();
        let write = WriteOutbound {
            id: message_id,
            in_reply_to: None,
            timestamp: chrono::Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind: MessageKind::Chat,
            platform_id: Some(event.platform_id.clone()),
            channel_type: Some(event.channel_type.clone()),
            thread_id: event.thread_id.clone(),
            content: serde_json::json!({ "text": text }),
        };
        let seq = outbound.with_conn(|c| insert_out(c, &write))?;
        Ok(FanoutOutcome::Answered(DeliveredTo {
            agent_group_id: session.agent_group_id,
            session_id: session.id,
            message_id,
            seq,
        }))
    }

    /// Session-local materialization of a staged inbound attachment
    /// (M18 C3 — see [`copperclaw_channels_core::inbound_file`] for the
    /// full contract).
    ///
    /// Returns the content value to persist for this wiring's session:
    ///
    /// - No `content.attachment.staged_path` marker: the event content,
    ///   cloned unchanged (the common case — plain text, callbacks,
    ///   adapters not yet migrated to the contract).
    /// - Marker present: the staged bytes are copied into
    ///   `<session_dir>/inbox/<msg_id>/<safe_name>` (re-sanitizing both
    ///   untrusted components and writing through
    ///   `copperclaw_db::attachments::extract_to_inbox`, which rejects
    ///   traversal and symlinks), `staged_path` is stripped, and
    ///   `attachment.path` is set to the container-visible
    ///   `/data/inbox/<msg_id>/<safe_name>` — the session dir is mounted
    ///   at `/data`, so that path resolves to exactly the file written
    ///   here.
    /// - Marker present but the copy failed (staged file vanished, disk
    ///   error): the message still routes; the attachment keeps its
    ///   metadata, loses `staged_path`, gains an `error` note, and gets
    ///   no `path` key. The failure is logged, never fatal — the text of
    ///   the message must reach the agent regardless.
    ///
    /// Fanout note: this runs once per wiring, so every target session
    /// receives its own copy of the file; the shared staged source is
    /// deleted by [`Self::route`] after the fanout completes.
    fn materialized_content(
        &self,
        event: &InboundEvent,
        session: &TargetSession,
    ) -> Result<serde_json::Value, RouterError> {
        let mut content = event.message.content.clone();
        let Some(att) = content
            .get_mut("attachment")
            .and_then(serde_json::Value::as_object_mut)
        else {
            return Ok(content);
        };
        let Some(staged) = att
            .get(STAGED_PATH_KEY)
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
        else {
            return Ok(content);
        };
        // The staged host path never reaches per-session storage: it is
        // meaningless (and misleading) inside the container.
        att.remove(STAGED_PATH_KEY);

        // Both components are sender-controlled: re-sanitize here even
        // though migrated adapters already sanitize the filename.
        let msg_component = sanitize_filename(Some(&event.message.id), "message");
        let supplied_name = att
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let safe_name = sanitize_filename(Some(supplied_name), FALLBACK_FILENAME);

        let session_root = self
            .session_paths
            .ensure_session_dir(&session.agent_group_id, &session.id)?;
        let inbox_root = session_root.join("inbox");
        match copy_staged_into_inbox(&inbox_root, &msg_component, &safe_name, &staged) {
            Ok(()) => {
                let container_path = container_inbox_path(&msg_component, &safe_name);
                att.insert("filename".to_owned(), serde_json::Value::String(safe_name));
                att.insert(
                    ATTACHMENT_PATH_KEY.to_owned(),
                    serde_json::Value::String(container_path),
                );
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    staged = %staged.display(),
                    session_id = %session.id.as_uuid(),
                    "inbound attachment materialization failed; routing message without file"
                );
                att.insert(
                    "error".to_owned(),
                    serde_json::Value::String(format!("attachment file unavailable: {err}")),
                );
            }
        }
        Ok(content)
    }

    /// Resolve the target session for a given wiring, creating one if
    /// nothing matches the wiring's `session_mode`.
    fn resolve_session(
        &self,
        event: &InboundEvent,
        mg_id: &MessagingGroupId,
        wiring: &MessagingGroupAgent,
    ) -> Result<TargetSession, RouterError> {
        let agent_group_id = wiring.agent_group_id;
        let (search_mg, search_thread): (Option<MessagingGroupId>, Option<String>) =
            match wiring.session_mode {
                // Shared: one session per (agent_group, messaging_group); thread is None.
                SessionMode::Shared => (Some(*mg_id), None),
                // PerThread: one session per (agent_group, messaging_group, thread).
                SessionMode::PerThread => (Some(*mg_id), event.thread_id.clone()),
                // AgentShared: one session per agent_group, ignoring mg/thread.
                SessionMode::AgentShared => (None, None),
            };

        if let Some(s) = find_for_agent(
            &self.central,
            agent_group_id,
            search_mg,
            search_thread.as_deref(),
        )? {
            self.session_paths
                .ensure_session_dir(&s.agent_group_id, &s.id)?;
            return Ok(TargetSession {
                agent_group_id: s.agent_group_id,
                id: s.id,
            });
        }

        // No existing session matches — create one.
        let req = CreateSession {
            agent_group_id,
            messaging_group_id: search_mg,
            thread_id: search_thread,
            agent_provider: None,
            source_session_id: None,
        };
        let created = create_session(&self.central, req)
            .map_err(|e| RouterError::session_create(format!("create session: {e}")))?;
        // Ensure the on-disk layout exists so the inbound.db can be opened.
        self.session_paths
            .ensure_session_dir(&created.agent_group_id, &created.id)?;
        // Touch the inbound.db so migrations run; otherwise the first
        // `insert_in` would do it as a side effect, but doing it eagerly
        // surfaces failures here instead of mid-write.
        let pool = self
            .session_paths
            .inbound_pool(&created.agent_group_id, &created.id)?;
        // Seed `session_routing` so the runner's `to: None` outbound
        // reply path knows where to send replies. Without this, an
        // agent that just emits text (the common case for the cli
        // channel) produces outbound rows with no destination, and
        // the delivery service marks them failed with `NoRoute`. The
        // host's wiring picks the channel/platform/thread off the
        // inbound event itself rather than relying on a per-mg
        // routing table because the cli channel's `platform_id` is
        // always `stdin` regardless of mg.
        let routing = copperclaw_types::routing::SessionRouting {
            channel_type: Some(event.channel_type.clone()),
            platform_id: Some(event.platform_id.clone()),
            thread_id: event.thread_id.clone(),
        };
        pool.with_conn(|conn| copperclaw_db::tables::session_routing::write(conn, &routing))
            .map_err(|e| RouterError::session_create(format!("write session_routing: {e}")))?;
        Ok(TargetSession {
            agent_group_id: created.agent_group_id,
            id: created.id,
        })
    }

    /// Append a row to `dropped_messages` for diagnostics. Errors here are
    /// propagated so test fixtures don't silently lose them, but the router
    /// proper has already produced its [`RouteOutcome`].
    fn record_drop(
        &self,
        event: &InboundEvent,
        mg_id: Option<MessagingGroupId>,
        ag_id: Option<AgentGroupId>,
        reason: &str,
    ) -> Result<(), RouterError> {
        let sender_name = event
            .sender
            .as_ref()
            .and_then(|s| s.display_name.clone())
            .or_else(|| event.sender.as_ref().map(|s| s.identity.clone()));
        insert_dropped(
            &self.central,
            InsertDroppedMessage {
                channel_type: event.channel_type.clone(),
                platform_id: event.platform_id.clone(),
                user_id: None,
                sender_name,
                reason: reason.to_owned(),
                messaging_group_id: mg_id,
                agent_group_id: ag_id,
            },
        )?;
        Ok(())
    }
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("hooks", &self.hooks)
            .field("debounce", &self.debounce)
            .field("inflight", &self.inflight)
            .field("fanout_count", &self.fanout_count())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum FanoutOutcome {
    Delivered(DeliveredTo),
    /// Host-answered slash command (`/status`): the `DeliveredTo` points
    /// at the synthesized `messages_out` row, not an inbound row.
    Answered(DeliveredTo),
    Dropped(DropReason),
    Pending(PendingReason),
}

#[derive(Debug, Clone, Copy)]
struct TargetSession {
    agent_group_id: AgentGroupId,
    id: SessionId,
}

fn drop_reason_label(reason: &DropReason) -> String {
    match reason {
        DropReason::NoMessagingGroup => "no_messaging_group".to_owned(),
        DropReason::NoAgents => "no_agents".to_owned(),
        DropReason::AccessDenied(r) => format!("access_denied:{r}"),
        DropReason::InterceptorDropped(r) => format!("interceptor_drop:{r}"),
        DropReason::Debounced => "debounced".to_owned(),
        DropReason::ReentryGuard => "reentry_guard".to_owned(),
        DropReason::MentionGated => "mention_gated".to_owned(),
    }
}

fn scope_reason(decision: Option<&SenderScopeDecision>) -> String {
    match decision {
        Some(SenderScopeDecision::Deny(r)) => format!("scope_deny:{r}"),
        Some(SenderScopeDecision::Pending(r)) => format!("scope_pending:{r}"),
        Some(SenderScopeDecision::Allow) => "scope_allow".to_owned(),
        Some(SenderScopeDecision::Defer) | None => "unknown_sender".to_owned(),
    }
}

/// Host path of a staged inbound attachment, if the event carries the
/// contract's `content.attachment.staged_path` marker.
fn staged_attachment_path(event: &InboundEvent) -> Option<PathBuf> {
    event
        .message
        .content
        .get("attachment")?
        .get(STAGED_PATH_KEY)?
        .as_str()
        .map(PathBuf::from)
}

/// Copy staged bytes into `<inbox_root>/<msg_id>/<filename>` via the
/// hardened `extract_to_inbox` writer. An `AlreadyExists` failure is
/// treated as success: the same platform message re-routed to the same
/// session (e.g. multiple wirings resolving to one session, or a
/// re-delivery outside the debounce window) has already materialized the
/// file at the exact path the rewritten content names.
fn copy_staged_into_inbox(
    inbox_root: &Path,
    msg_id: &str,
    filename: &str,
    staged: &Path,
) -> Result<(), String> {
    let bytes = std::fs::read(staged).map_err(|e| format!("read staged file: {e}"))?;
    match extract_to_inbox(inbox_root, msg_id, filename, &bytes) {
        Ok(_) => Ok(()),
        Err(copperclaw_db::DbError::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(())
        }
        Err(e) => Err(format!("write inbox file: {e}")),
    }
}

/// Extract the callback payload of a button-tap event (M18 G1).
///
/// Adapters synthesize button taps as `Chat` events carrying a `callback`
/// object whose payload key differs per platform: telegram's `callback_query`
/// stores it under `data`, slack's `block_actions` under `value`. This returns
/// whichever is present (preferring `data`) so the approval interceptor can
/// decide whether it's an `approve:<id>` / `deny:<id>` tap. Anything else
/// yields `None` (routed as ordinary text).
fn approval_callback_data(event: &InboundEvent) -> Option<String> {
    let callback = event.message.content.get("callback")?;
    callback
        .get("data")
        .or_else(|| callback.get("value"))
        .and_then(serde_json::Value::as_str)
        .map(std::borrow::ToOwned::to_owned)
}

fn source_session_for(event: &InboundEvent) -> Option<String> {
    // The agent-to-agent module sets `event.message.content["source_session_id"]`
    // when emitting cross-session traffic. Strings are accepted as-is; non-strings
    // are ignored.
    let content = &event.message.content;
    if event.message.kind != MessageKind::Agent {
        return None;
    }
    content
        .get("source_session_id")
        .and_then(serde_json::Value::as_str)
        .map(std::borrow::ToOwned::to_owned)
}

// Helper kept public to the crate so the `lib` module can re-export the
// constant for downstream introspection.
pub use crate::debounce::DEBOUNCE_WINDOW;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::FsSessionRoot;
    use chrono::Utc;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messaging_group_agents::{UpsertWiring, upsert as upsert_wire};
    use copperclaw_db::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
    use copperclaw_modules::context::{GateDecision, InterceptorDecision, SenderScopeDecision};
    use copperclaw_types::{
        ChannelType, EngageMode, InboundMessage, MessageKind, SenderIdentity, SessionMode, UserId,
    };
    use std::sync::Arc;

    struct Fixture {
        router: Router,
        // Kept alive for the duration of the test; the M18 C3 inbound-file
        // tests also read paths under it directly (`session_dir`), so it's
        // no longer purely an RAII guard — not underscore-prefixed.
        tmp: tempfile::TempDir,
        mg_id: MessagingGroupId,
        ag_id: AgentGroupId,
    }

    fn fixture(session_mode: SessionMode) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));

        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("cli"),
                platform_id: "chat-1".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_wire(
            &db,
            UpsertWiring {
                messaging_group_id: mg.id,
                agent_group_id: ag.id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode,
                priority: 0,
            },
        )
        .unwrap();

        let router = Router::new(db, root);
        Fixture {
            router,
            tmp,
            mg_id: mg.id,
            ag_id: ag.id,
        }
    }

    fn event(thread_id: Option<&str>, message_id: &str) -> InboundEvent {
        InboundEvent {
            channel_type: ChannelType::new("cli"),
            platform_id: "chat-1".into(),
            thread_id: thread_id.map(std::borrow::ToOwned::to_owned),
            message: InboundMessage {
                id: message_id.into(),
                kind: MessageKind::Chat,
                content: serde_json::json!({"text":"hi"}),
                timestamp: Utc::now(),
                is_mention: None,
                is_group: None,
            },
            reply_to: None,
            sender: Some(SenderIdentity {
                channel_type: ChannelType::new("cli"),
                identity: "user-1".into(),
                display_name: Some("Alice".into()),
            }),
        }
    }

    /// Fixture whose messaging group is a GROUP chat (`is_group = true`),
    /// wired with the given engage mode/pattern. Used by the mention-gating
    /// tests. The router uses the supplied gate (default if `None`).
    fn group_fixture(
        engage_mode: EngageMode,
        engage_pattern: Option<&str>,
        gate: Option<MentionGate>,
    ) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("cli"),
                platform_id: "chat-1".into(),
                name: None,
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_wire(
            &db,
            UpsertWiring {
                messaging_group_id: mg.id,
                agent_group_id: ag.id,
                engage_mode,
                engage_pattern: engage_pattern.map(std::borrow::ToOwned::to_owned),
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();
        let mut router = Router::new(db, root);
        if let Some(g) = gate {
            router = router.with_mention_gate(g);
        }
        Fixture {
            router,
            tmp,
            mg_id: mg.id,
            ag_id: ag.id,
        }
    }

    /// A group-chat chat event (the channel reports `is_group = Some(true)`).
    fn group_event(message_id: &str) -> InboundEvent {
        let mut ev = event(None, message_id);
        ev.message.is_group = Some(true);
        ev
    }

    #[tokio::test]
    async fn mention_gate_drops_unmentioned_group_text() {
        // Default policy + Mention wiring: ambient group chatter with no
        // mention, no reply-to-self, no pattern is dropped.
        let fx = group_fixture(EngageMode::Mention, None, None);
        let out = fx.router.route(group_event("g1")).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::MentionGated,
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
        // And the drop is recorded with the canonical label.
        let dropped =
            copperclaw_db::tables::dropped_messages::list(fx.router.central(), None).unwrap();
        assert!(dropped.iter().any(|d| d.reason == "mention_gated"));
        let _ = fx.ag_id;
    }

    #[tokio::test]
    async fn mention_gate_processes_native_mention_in_group() {
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("g2");
        ev.message.is_mention = Some(true);
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "native @-mention must engage: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_processes_reply_to_agent_in_group() {
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("g3");
        ev.reply_to = Some(copperclaw_types::ReplyTo {
            channel_type: ChannelType::new("cli"),
            platform_id: "chat-1".into(),
            thread_id: Some("parent-7".into()),
            replying_to_self: Some(true),
        });
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "reply to the agent's own message must engage: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_drops_reply_to_other_user_in_group() {
        // A reply whose parent was NOT the agent (or is unresolved) must not
        // count as a mention — the core fix vs. "treat any reply_to as a
        // mention".
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("g3b");
        ev.reply_to = Some(copperclaw_types::ReplyTo {
            channel_type: ChannelType::new("cli"),
            platform_id: "chat-1".into(),
            thread_id: Some("parent-7".into()),
            replying_to_self: Some(false),
        });
        let out = fx.router.route(ev).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::MentionGated,
            } => {}
            other => panic!("reply to another user must be gated: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mention_gate_drops_reply_with_unknown_parent_author_in_group() {
        // `replying_to_self = None` (adapter could not resolve the parent
        // author) must NOT count as a mention.
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("g3c");
        ev.reply_to = Some(copperclaw_types::ReplyTo {
            channel_type: ChannelType::new("cli"),
            platform_id: "chat-1".into(),
            thread_id: Some("parent-7".into()),
            replying_to_self: None,
        });
        let out = fx.router.route(ev).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::MentionGated,
            } => {}
            other => panic!("unresolved-author reply must be gated: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mention_gate_processes_pattern_match_in_group() {
        // Pattern wiring engages on a regex match instead of a mention.
        let fx = group_fixture(EngageMode::Pattern, Some("(?i)deploy"), None);
        let mut ev = group_event("g4");
        ev.message.content = serde_json::json!({"text":"please DEPLOY the build"});
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "pattern match must engage: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_drops_pattern_miss_in_group() {
        // Pattern wirings still gate text that does not match — but via the
        // pattern, not a mention requirement.
        let fx = group_fixture(EngageMode::Pattern, Some("(?i)deploy"), None);
        let mut ev = group_event("g4b");
        ev.message.content = serde_json::json!({"text":"good morning everyone"});
        let out = fx.router.route(ev).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::MentionGated,
            } => {}
            other => panic!("pattern miss must be gated: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mention_gate_processes_callback_query_in_group() {
        // A button tap / callback query in a group must reach the agent even
        // with no mention — the content carries a `callback` marker.
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("g5");
        ev.message.is_mention = None;
        ev.message.content = serde_json::json!({
            "text": "approve",
            "callback": {"id": "cb-1", "data": "approve"},
        });
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "callback query in a group must be processed: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_processes_command_in_group() {
        // An adapter-stamped `content.command` payload bypasses the gate
        // even when the command itself isn't one the router recognises
        // (recognised ones take their own path — see the slash-command
        // tests below; `/status` in particular is host-Answered).
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("g5b");
        ev.message.content = serde_json::json!({
            "text": "/frobnicate",
            "command": "frobnicate",
        });
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "slash command in a group must be processed: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_processes_non_chat_kind_in_group() {
        // A scheduled task / webhook / system event is never gated.
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("g6");
        ev.message.kind = MessageKind::Task;
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "non-chat kind must bypass the gate: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_always_processes_dm() {
        // DM venue (`is_group = false`) is never gated, even unmentioned.
        let fx = fixture(SessionMode::Shared); // mg.is_group = false
        let mut ev = event(None, "dm1");
        ev.message.is_group = Some(false);
        ev.message.is_mention = None;
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "DM must always be processed: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_per_group_override_pattern_processes_unmentioned() {
        // Per-group override: a Pattern wiring with a catch-all pattern keeps
        // the group ungated even under the default require-in-groups policy.
        let fx = group_fixture(EngageMode::Pattern, Some(".*"), None);
        let out = fx.router.route(group_event("g7")).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "per-group Pattern override must process unmentioned text: {out:?}"
        );
    }

    #[tokio::test]
    async fn mention_gate_construction_override_disables_group_gating() {
        // Construction-time policy with require_in_groups=false un-gates
        // groups host-wide even for a Mention wiring.
        let gate = MentionGate {
            require_in_groups: false,
            require_in_dms: false,
        };
        let fx = group_fixture(EngageMode::Mention, None, Some(gate));
        let out = fx.router.route(group_event("g8")).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "require_in_groups=false must process unmentioned group text: {out:?}"
        );
        assert!(!fx.router.mention_gate().require_in_groups);
    }

    #[tokio::test]
    async fn approval_interceptor_consumes_approve_callback() {
        // M18 G1: an `approve:<id>` callback tap that the interceptor handles
        // yields `Pending(ApprovalHandled)` and writes NO inbound row.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fx = fixture(SessionMode::Shared);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        fx.router
            .hooks()
            .set_approval_interceptor(Arc::new(move |ctx| {
                calls2.fetch_add(1, Ordering::SeqCst);
                if ctx.callback_data.starts_with("approve:")
                    || ctx.callback_data.starts_with("deny:")
                {
                    copperclaw_modules::context::ApprovalInterceptDecision::Handled
                } else {
                    copperclaw_modules::context::ApprovalInterceptDecision::Passthrough
                }
            }));
        let mut ev = event(None, "cb-1");
        ev.message.content = serde_json::json!({
            "text": "approve:abc",
            "callback": {"id": "cb-1", "data": "approve:abc"},
        });
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(
                out,
                RouteOutcome::Pending {
                    reason: PendingReason::ApprovalHandled
                }
            ),
            "approval tap must be handled: {out:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "interceptor ran once");
    }

    #[tokio::test]
    async fn approval_interceptor_passthrough_routes_normally() {
        // A callback the interceptor does NOT recognise falls through to
        // ordinary routing (delivered), and a plain text message never even
        // reaches the interceptor.
        let fx = fixture(SessionMode::Shared);
        fx.router.hooks().set_approval_interceptor(Arc::new(|_ctx| {
            copperclaw_modules::context::ApprovalInterceptDecision::Passthrough
        }));
        let mut ev = event(None, "cb-2");
        ev.message.content = serde_json::json!({
            "text": "expand",
            "callback": {"id": "cb-2", "data": "expand:99"},
        });
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "unrecognised callback must route normally: {out:?}"
        );
    }

    #[tokio::test]
    async fn route_no_messaging_group_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
        let router = Router::new(db, root);
        let out = router.route(event(None, "m1")).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::NoMessagingGroup,
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_no_agents_drops() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("cli"),
                platform_id: "chat-1".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
        let router = Router::new(db, root);
        let out = router.route(event(None, "m1")).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::NoAgents,
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn route_persists_reply_to_and_is_group_into_messages_in() {
        // The runner's "Conversation context" block reads
        // MessageInRow::reply_to / is_group; the router has to forward
        // the channel-populated InboundEvent fields onto the persisted
        // row. Without this test, A's and B's slice-2 work nets out at
        // "the agent thinks every turn is a generic DM with no reply
        // context" because the runner reads from the DB, not the
        // InboundEvent.
        let fx = fixture(SessionMode::Shared);
        let mut ev = event(None, "m-reply");
        ev.message.is_group = Some(true);
        ev.reply_to = Some(copperclaw_types::ReplyTo {
            channel_type: ChannelType::new("cli"),
            platform_id: "chat-1".into(),
            // The parent's platform-side message id — this is what the
            // router persists onto messages_in.reply_to.
            thread_id: Some("parent-msg-77".into()),
            // A reply to the agent's own message — engages the mention gate
            // so the group event is processed (and we can assert persistence).
            replying_to_self: Some(true),
        });
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        assert_eq!(sessions.len(), 1);
        let target = &sessions[0];

        // Read back the row the router just wrote and confirm both
        // fields landed.
        let pool = fx
            .router
            .session_paths()
            .inbound_pool(&target.agent_group_id, &target.session_id)
            .unwrap();
        let rows = pool
            .with_conn(|c| copperclaw_db::tables::messages_in::get_pending(c, true, 10))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].is_group, Some(true));
        assert_eq!(rows[0].reply_to.as_deref(), Some("parent-msg-77"));
    }

    #[tokio::test]
    async fn route_writes_none_reply_to_when_event_has_no_reply() {
        // The complement: a vanilla inbound (no reply_to on the wire)
        // must land as NULL on the row so the runner's context-block
        // doesn't fabricate a "in reply to" clause.
        let fx = fixture(SessionMode::Shared);
        let mut ev = event(None, "m-plain");
        ev.message.is_group = Some(false); // explicit DM
        // ev.reply_to stays None as constructed by the helper.
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let target = &sessions[0];
        let pool = fx
            .router
            .session_paths()
            .inbound_pool(&target.agent_group_id, &target.session_id)
            .unwrap();
        let rows = pool
            .with_conn(|c| copperclaw_db::tables::messages_in::get_pending(c, true, 10))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reply_to, None);
        // is_group=Some(false) is distinct from None: the channel
        // explicitly said "this is a DM"; the runner uses that to
        // render "in a 1-on-1 DM" instead of the thread-fallback
        // phrasing.
        assert_eq!(rows[0].is_group, Some(false));
    }

    #[tokio::test]
    async fn route_delivers_to_session_with_even_seq() {
        let fx = fixture(SessionMode::Shared);
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent_group_id, fx.ag_id);
        assert_eq!(sessions[0].seq % 2, 0, "host writes use even seq");
    }

    #[tokio::test]
    async fn route_debounces_duplicates() {
        let fx = fixture(SessionMode::Shared);
        let first = fx.router.route(event(None, "m1")).await.unwrap();
        assert!(matches!(first, RouteOutcome::Delivered { .. }));
        let second = fx.router.route(event(None, "m1")).await.unwrap();
        match second {
            RouteOutcome::Dropped {
                reason: DropReason::Debounced,
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn access_gate_deny_short_circuits() {
        let fx = fixture(SessionMode::Shared);
        fx.router
            .hooks()
            .set_access_gate(Arc::new(|_| GateDecision::Deny("nope".into())));
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::AccessDenied(r),
            } => assert_eq!(r, "nope"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn channel_request_gate_deny_short_circuits() {
        let fx = fixture(SessionMode::Shared);
        fx.router
            .hooks()
            .set_channel_request_gate(Arc::new(|_| GateDecision::Deny("blocked".into())));
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::AccessDenied(r),
            } => assert_eq!(r, "blocked"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn channel_request_gate_defer_passes_through() {
        let fx = fixture(SessionMode::Shared);
        fx.router
            .hooks()
            .set_channel_request_gate(Arc::new(|_| GateDecision::Defer));
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        assert!(matches!(out, RouteOutcome::Delivered { .. }));
    }

    #[tokio::test]
    async fn sender_resolver_resolves_user() {
        let fx = fixture(SessionMode::Shared);
        let known = UserId::new();
        fx.router
            .hooks()
            .set_sender_resolver(Arc::new(move |_| Some(known)));
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        fx.router.hooks().set_access_gate(Arc::new(move |ctx| {
            cap.lock().unwrap().push(ctx.user);
            GateDecision::Allow
        }));
        let _ = fx.router.route(event(None, "m1")).await.unwrap();
        let access_args = captured.lock().unwrap().first().copied();
        assert_eq!(access_args, Some(Some(known)));
    }

    #[tokio::test]
    async fn sender_scope_pending_yields_pending_outcome() {
        let fx = fixture(SessionMode::Shared);
        fx.router
            .hooks()
            .set_sender_scope_gate(Arc::new(|_| SenderScopeDecision::Pending("wait".into())));
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        match out {
            RouteOutcome::Pending {
                reason: PendingReason::SenderUnregistered,
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
        // The unregistered sender was recorded.
        let rows =
            copperclaw_db::tables::unregistered_senders::list(fx.router.central(), None).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn sender_scope_deny_drops() {
        let fx = fixture(SessionMode::Shared);
        fx.router
            .hooks()
            .set_sender_scope_gate(Arc::new(|_| SenderScopeDecision::Deny("blocked".into())));
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::AccessDenied(r),
            } => assert_eq!(r, "blocked"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn sender_scope_allow_delivers() {
        let fx = fixture(SessionMode::Shared);
        fx.router
            .hooks()
            .set_sender_scope_gate(Arc::new(|_| SenderScopeDecision::Allow));
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        assert!(matches!(out, RouteOutcome::Delivered { .. }));
    }

    #[tokio::test]
    async fn unknown_sender_recorded_when_no_user_resolved() {
        let fx = fixture(SessionMode::Shared);
        // No sender resolver — leaves user_id as None.
        let _ = fx.router.route(event(None, "m1")).await.unwrap();
        let rows =
            copperclaw_db::tables::unregistered_senders::list(fx.router.central(), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].platform_id, "user-1");
    }

    #[tokio::test]
    async fn shared_session_mode_reuses_session_across_threads() {
        let fx = fixture(SessionMode::Shared);
        let a = fx.router.route(event(Some("t1"), "m1")).await.unwrap();
        let b = fx.router.route(event(Some("t2"), "m2")).await.unwrap();
        let (RouteOutcome::Delivered { sessions: sa }, RouteOutcome::Delivered { sessions: sb }) =
            (a, b)
        else {
            panic!("expected delivered");
        };
        assert_eq!(sa[0].session_id, sb[0].session_id);
    }

    #[tokio::test]
    async fn per_thread_session_mode_creates_session_per_thread() {
        let fx = fixture(SessionMode::PerThread);
        let a = fx.router.route(event(Some("t1"), "m1")).await.unwrap();
        let b = fx.router.route(event(Some("t2"), "m2")).await.unwrap();
        let (RouteOutcome::Delivered { sessions: sa }, RouteOutcome::Delivered { sessions: sb }) =
            (a, b)
        else {
            panic!("expected delivered");
        };
        assert_ne!(sa[0].session_id, sb[0].session_id);
    }

    #[tokio::test]
    async fn agent_shared_session_mode_ignores_messaging_group() {
        let fx = fixture(SessionMode::AgentShared);
        // Add a second messaging group + wire to the same agent group.
        let mg2 = upsert_mg(
            fx.router.central(),
            UpsertMessagingGroup {
                channel_type: ChannelType::new("cli"),
                platform_id: "chat-2".into(),
                name: None,
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_wire(
            fx.router.central(),
            UpsertWiring {
                messaging_group_id: mg2.id,
                agent_group_id: fx.ag_id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::AgentShared,
                priority: 0,
            },
        )
        .unwrap();

        let a = fx.router.route(event(None, "m1")).await.unwrap();
        let mut ev = event(None, "m2");
        ev.platform_id = "chat-2".into();
        let b = fx.router.route(ev).await.unwrap();
        let (RouteOutcome::Delivered { sessions: sa }, RouteOutcome::Delivered { sessions: sb }) =
            (a, b)
        else {
            panic!("expected delivered");
        };
        assert_eq!(sa[0].session_id, sb[0].session_id);
        assert_eq!(sa[0].agent_group_id, fx.ag_id);
        // Suppress unused-variable lint on the mg_id field while keeping fx tidy.
        let _ = fx.mg_id;
    }

    #[tokio::test]
    async fn re_entry_guard_blocks_self_fanout() {
        let fx = fixture(SessionMode::Shared);
        // First delivery to seed a session.
        let first = fx.router.route(event(None, "m1")).await.unwrap();
        let RouteOutcome::Delivered { sessions } = first else {
            panic!("expected delivered");
        };
        let sid = sessions[0].session_id;
        // Build an event whose source_session_id matches the target session
        // and whose kind is Agent (to enable the source_session_for lookup).
        let mut ev = event(None, "m2");
        ev.message.kind = MessageKind::Agent;
        ev.message.content =
            serde_json::json!({"text":"loop","source_session_id": sid.as_uuid().to_string()});
        let out = fx.router.route(ev).await.unwrap();
        match out {
            RouteOutcome::Dropped {
                reason: DropReason::ReentryGuard,
            } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn multi_wiring_fanout_writes_both_inbound_dbs() {
        let fx = fixture(SessionMode::Shared);
        // Add a second agent group wired to the same messaging group.
        let ag2 = create_ag(
            fx.router.central(),
            CreateAgentGroup {
                name: "second".into(),
                folder: "second".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        upsert_wire(
            fx.router.central(),
            UpsertWiring {
                messaging_group_id: fx.mg_id,
                agent_group_id: ag2.id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();
        let out = fx.router.route(event(None, "m1")).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered");
        };
        assert_eq!(sessions.len(), 2);
        let ag_ids: std::collections::HashSet<_> =
            sessions.iter().map(|s| s.agent_group_id).collect();
        assert!(ag_ids.contains(&fx.ag_id));
        assert!(ag_ids.contains(&ag2.id));
    }

    #[tokio::test]
    async fn hooks_mut_is_usable() {
        let mut fx = fixture(SessionMode::Shared);
        fx.router
            .hooks_mut()
            .set_message_interceptor(Arc::new(|_| InterceptorDecision::Passthrough));
        assert!(fx.router.hooks().has_message_interceptor());
    }

    #[tokio::test]
    async fn accessors_expose_state() {
        let fx = fixture(SessionMode::Shared);
        assert!(fx.router.central().conn().is_ok());
        assert_eq!(fx.router.fanout_count(), 0);
        let _ = fx.router.route(event(None, "m1")).await.unwrap();
        assert!(fx.router.fanout_count() >= 1);
        // Debounce + inflight handles are reachable for diagnostics.
        let _: &Arc<DashMap<DebounceKey, Instant>> = fx.router.debounce();
        let _: &Arc<DashMap<InflightKey, ()>> = fx.router.inflight();
        let _ = fx.router.session_paths();
    }

    #[test]
    fn drop_reason_labels_render() {
        for r in [
            DropReason::NoMessagingGroup,
            DropReason::NoAgents,
            DropReason::AccessDenied("x".into()),
            DropReason::InterceptorDropped("y".into()),
            DropReason::Debounced,
            DropReason::ReentryGuard,
            DropReason::MentionGated,
        ] {
            let s = drop_reason_label(&r);
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn scope_reason_renders_every_arm() {
        let deny = SenderScopeDecision::Deny("a".into());
        let pending = SenderScopeDecision::Pending("b".into());
        assert_eq!(scope_reason(Some(&deny)), "scope_deny:a");
        assert_eq!(scope_reason(Some(&pending)), "scope_pending:b");
        assert_eq!(
            scope_reason(Some(&SenderScopeDecision::Allow)),
            "scope_allow"
        );
        assert_eq!(
            scope_reason(Some(&SenderScopeDecision::Defer)),
            "unknown_sender"
        );
        assert_eq!(scope_reason(None), "unknown_sender");
    }

    #[test]
    fn source_session_extracts_only_for_agent_kind() {
        let mut ev = event(None, "m1");
        assert!(source_session_for(&ev).is_none());
        ev.message.kind = MessageKind::Agent;
        ev.message.content = serde_json::json!({"source_session_id": "abc"});
        assert_eq!(source_session_for(&ev).as_deref(), Some("abc"));
        ev.message.content = serde_json::json!({"source_session_id": 5});
        assert!(source_session_for(&ev).is_none());
    }

    #[test]
    fn fanout_outcome_debug_renders() {
        let f = FanoutOutcome::Delivered(DeliveredTo {
            agent_group_id: AgentGroupId::new(),
            session_id: SessionId::new(),
            message_id: MessageId::new(),
            seq: 2,
        });
        assert!(format!("{f:?}").contains("Delivered"));
        let f = FanoutOutcome::Answered(DeliveredTo {
            agent_group_id: AgentGroupId::new(),
            session_id: SessionId::new(),
            message_id: MessageId::new(),
            seq: 1,
        });
        assert!(format!("{f:?}").contains("Answered"));
        let f = FanoutOutcome::Dropped(DropReason::Debounced);
        assert!(format!("{f:?}").contains("Dropped"));
        let f = FanoutOutcome::Pending(PendingReason::SenderUnregistered);
        assert!(format!("{f:?}").contains("Pending"));
    }

    #[test]
    fn pending_reason_variants_construct() {
        let _ = PendingReason::SenderUnregistered;
        let _ = PendingReason::ChannelRequestPending("x".into());
    }

    #[tokio::test]
    async fn dropped_messages_recorded_for_no_mg() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
        let router = Router::new(db, root);
        let _ = router.route(event(None, "m1")).await.unwrap();
        let dropped =
            copperclaw_db::tables::dropped_messages::list(router.central(), None).unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].reason, "no_messaging_group");
    }

    #[tokio::test]
    async fn router_debug_renders() {
        let fx = fixture(SessionMode::Shared);
        let s = format!("{:?}", fx.router);
        assert!(s.contains("Router"));
    }

    #[tokio::test]
    async fn delivered_route_signals_inbound_wake() {
        let fx = fixture(SessionMode::Shared);
        let wake = fx.router.inbound_wake();
        let out = fx.router.route(event(None, "w1")).await.unwrap();
        assert!(matches!(out, RouteOutcome::Delivered { .. }));
        // The insert stored a permit; a waiter completes immediately.
        tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
            .await
            .expect("wake permit must be stored after a delivered route");
    }

    #[tokio::test]
    async fn dropped_route_does_not_signal_inbound_wake() {
        // A mention-gated drop writes no messages_in row, so it must not
        // wake the container manager (no spawn storms from ambient chatter).
        let fx = group_fixture(EngageMode::Mention, None, None);
        let wake = fx.router.inbound_wake();
        let out = fx.router.route(group_event("w2")).await.unwrap();
        assert!(matches!(
            out,
            RouteOutcome::Dropped {
                reason: DropReason::MentionGated
            }
        ));
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified()).await;
        assert!(waited.is_err(), "dropped route must not store a permit");
    }

    #[tokio::test]
    async fn inbound_wake_coalesces_burst_of_inserts() {
        // N delivered routes while nobody is listening store exactly ONE
        // permit (Notify semantics): the first waiter completes, the second
        // blocks. This is the no-spawn-storm guarantee at the router end.
        let fx = fixture(SessionMode::Shared);
        let wake = fx.router.inbound_wake();
        for i in 0..10 {
            let out = fx
                .router
                .route(event(None, &format!("b{i}")))
                .await
                .unwrap();
            assert!(matches!(out, RouteOutcome::Delivered { .. }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
            .await
            .expect("first waiter consumes the coalesced permit");
        let second =
            tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified()).await;
        assert!(
            second.is_err(),
            "burst of inserts must coalesce into a single permit"
        );
    }

    // ---- end-user slash commands (M18 R1) ----

    /// Read every `messages_in` row for the routed session.
    fn inbound_rows(fx: &Fixture, target: &DeliveredTo) -> Vec<copperclaw_types::MessageInRow> {
        let pool = fx
            .router
            .session_paths()
            .inbound_pool(&target.agent_group_id, &target.session_id)
            .unwrap();
        pool.with_conn(|c| copperclaw_db::tables::messages_in::get_pending(c, true, 50))
            .unwrap()
    }

    #[tokio::test]
    async fn slash_stop_writes_control_row() {
        let fx = fixture(SessionMode::Shared);
        let mut ev = event(None, "cmd-stop-1");
        ev.message.content = serde_json::json!({"text": "/stop"});
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.kind,
            MessageKind::System,
            "control rows are system-kind"
        );
        assert!(!row.trigger, "control rows must not spawn a container");
        assert!(!row.on_wake);
        assert_eq!(row.status, "pending", "control rows stay pending for R2");
        assert_eq!(row.content["control"]["op"], "stop");
        assert_eq!(row.content["command"], "stop");
        assert_eq!(row.content["text"], "/stop");
    }

    #[tokio::test]
    async fn slash_stop_lands_even_while_turn_in_flight() {
        // Simulate a turn in flight: a prior chat row is pending
        // (picked up but unfinished — runner state never blocks the
        // router's write path). The /stop control row must still land,
        // marked control, alongside it.
        let fx = fixture(SessionMode::Shared);
        let first = fx.router.route(event(None, "busy-1")).await.unwrap();
        let RouteOutcome::Delivered { sessions: s1 } = first else {
            panic!("expected delivered");
        };
        let mut stop = event(None, "cmd-stop-2");
        stop.message.content = serde_json::json!({"text": "/stop"});
        let out = fx.router.route(stop).await.unwrap();
        let RouteOutcome::Delivered { sessions: s2 } = out else {
            panic!("expected delivered, got {out:?}");
        };
        assert_eq!(s1[0].session_id, s2[0].session_id, "same session");
        let rows = inbound_rows(&fx, &s2[0]);
        assert_eq!(rows.len(), 2, "chat row + control row coexist");
        let control = rows
            .iter()
            .find(|r| r.content.get("control").is_some())
            .expect("control row present");
        assert_eq!(control.kind, MessageKind::System);
        assert!(!control.trigger);
        assert_eq!(control.status, "pending");
        assert!(
            control.seq > s1[0].seq,
            "control row sequenced after the in-flight chat row"
        );
    }

    #[tokio::test]
    async fn slash_stop_bypasses_mention_gate_in_group() {
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("cmd-stop-3");
        ev.message.content = serde_json::json!({"text": "/stop"});
        ev.message.is_mention = None;
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Delivered { .. }),
            "unmentioned /stop in a gated group must route: {out:?}"
        );
    }

    #[tokio::test]
    async fn slash_stop_does_not_signal_inbound_wake() {
        // trigger = false rows are invisible to the spawn classifier,
        // so waking the manager for them would be a guaranteed no-op.
        let fx = fixture(SessionMode::Shared);
        let wake = fx.router.inbound_wake();
        let mut ev = event(None, "cmd-stop-4");
        ev.message.content = serde_json::json!({"text": "/cancel"});
        let out = fx.router.route(ev).await.unwrap();
        assert!(matches!(out, RouteOutcome::Delivered { .. }));
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified()).await;
        assert!(waited.is_err(), "control row must not store a wake permit");
    }

    #[tokio::test]
    async fn reaction_bypasses_mention_gate_and_persists_non_trigger_row() {
        // M19 U7: a reaction in a mention-gated group routes past the gate
        // (interaction payload) and persists as a NON-trigger Chat row so it
        // can never spawn a spurious container — the runner consumes it via
        // the mid-turn steering seam.
        let fx = group_fixture(EngageMode::Mention, None, None);
        let wake = fx.router.inbound_wake();
        let mut ev = group_event("react-1");
        ev.message.is_mention = None;
        ev.message.content =
            copperclaw_channels_core::reaction_content("\u{1F44D}", Some("out-7"), Some("alice"));
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("unmentioned reaction in a gated group must route: {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.kind, MessageKind::Chat, "reaction stays chat-kind");
        assert!(!row.trigger, "reaction rows must not spawn a container");
        assert_eq!(row.content["reaction"]["emoji"], "\u{1F44D}");
        assert_eq!(row.content["reaction"]["target_seq"], "out-7");
        // Like a control row, a non-trigger reaction must not store a wake.
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified()).await;
        assert!(waited.is_err(), "reaction row must not store a wake permit");
    }

    #[tokio::test]
    async fn slash_clear_normalises_text_and_stamps_command() {
        let fx = fixture(SessionMode::Shared);
        let mut ev = event(None, "cmd-clear-1");
        ev.message.content = serde_json::json!({"text": "/CLEAR@ReplayBot"});
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.kind, MessageKind::Chat, "passthrough keeps chat kind");
        assert!(row.trigger, "passthrough rows wake the runner normally");
        assert_eq!(
            row.content["text"], "/clear",
            "text normalised to what the runner sentinel matches"
        );
        assert_eq!(row.content["command"], "clear");
        assert_eq!(row.content["original_text"], "/CLEAR@ReplayBot");
    }

    #[tokio::test]
    async fn slash_compact_bypasses_mention_gate_and_wakes() {
        let fx = group_fixture(EngageMode::Mention, None, None);
        let wake = fx.router.inbound_wake();
        let mut ev = group_event("cmd-compact-1");
        ev.message.content = serde_json::json!({"text": "/compact"});
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("unmentioned /compact in a gated group must route: {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        assert_eq!(rows[0].content["text"], "/compact");
        assert_eq!(rows[0].content["command"], "compact");
        assert!(rows[0].content.get("original_text").is_none());
        tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
            .await
            .expect("/compact is a trigger row and must wake the manager");
    }

    #[tokio::test]
    async fn slash_status_answers_from_host_without_inbound_row() {
        let fx = fixture(SessionMode::Shared);
        let wake = fx.router.inbound_wake();
        let mut ev = event(Some("t-7"), "cmd-status-1");
        ev.message.content = serde_json::json!({"text": "/status"});
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Answered { sessions } = out else {
            panic!("expected answered, got {out:?}");
        };
        assert_eq!(sessions.len(), 1);
        let target = &sessions[0];
        assert_eq!(
            target.seq % 2,
            1,
            "outbound rows use odd (container) parity"
        );

        // No inbound row was written and the manager was not woken.
        let rows = inbound_rows(&fx, target);
        assert!(
            rows.is_empty(),
            "/status must not write messages_in: {rows:?}"
        );
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified()).await;
        assert!(waited.is_err(), "/status must not store a wake permit");

        // The synthesized reply landed in messages_out with explicit
        // routing back to the originating channel.
        let outbound = fx
            .router
            .session_paths()
            .outbound_pool(&target.agent_group_id, &target.session_id)
            .unwrap();
        let out_rows = outbound
            .with_conn(copperclaw_db::tables::messages_out::list_due)
            .unwrap();
        assert_eq!(out_rows.len(), 1);
        let reply = &out_rows[0];
        assert_eq!(reply.kind, MessageKind::Chat);
        assert_eq!(reply.platform_id.as_deref(), Some("chat-1"));
        assert_eq!(
            reply.channel_type.as_ref().map(ChannelType::as_str),
            Some("cli")
        );
        assert_eq!(reply.thread_id.as_deref(), Some("t-7"));
        let text = reply.content["text"].as_str().unwrap();
        assert!(text.contains("Agent status"), "{text}");
        assert!(text.contains("queued messages: 0"), "{text}");
        assert!(text.contains("session state: active"), "{text}");
    }

    #[tokio::test]
    async fn slash_status_bypasses_mention_gate_in_group() {
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("cmd-status-2");
        ev.message.content = serde_json::json!({"text": "/status"});
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(out, RouteOutcome::Answered { .. }),
            "unmentioned /status in a gated group must be answered: {out:?}"
        );
    }

    #[tokio::test]
    async fn slash_status_counts_queued_trigger_rows() {
        let fx = fixture(SessionMode::Shared);
        // Queue one normal message first, then ask for status.
        let first = fx.router.route(event(None, "queued-1")).await.unwrap();
        assert!(matches!(first, RouteOutcome::Delivered { .. }));
        let mut ev = event(None, "cmd-status-3");
        ev.message.content = serde_json::json!({"text": "/status"});
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Answered { sessions } = out else {
            panic!("expected answered, got {out:?}");
        };
        let outbound = fx
            .router
            .session_paths()
            .outbound_pool(&sessions[0].agent_group_id, &sessions[0].session_id)
            .unwrap();
        let out_rows = outbound
            .with_conn(copperclaw_db::tables::messages_out::list_due)
            .unwrap();
        let text = out_rows[0].content["text"].as_str().unwrap();
        assert!(text.contains("queued messages: 1"), "{text}");
    }

    #[tokio::test]
    async fn unknown_slash_command_falls_through_unchanged() {
        let fx = fixture(SessionMode::Shared);
        let mut ev = event(None, "cmd-unknown-1");
        ev.message.content = serde_json::json!({"text": "/frobnicate now"});
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, MessageKind::Chat);
        assert!(rows[0].trigger);
        assert_eq!(
            rows[0].content,
            serde_json::json!({"text": "/frobnicate now"}),
            "unknown commands must be byte-identical passthrough"
        );
    }

    #[tokio::test]
    async fn unknown_slash_command_still_subject_to_mention_gate() {
        // Unknown /x is plain text: in a gated group without a mention
        // it drops like any other ambient chatter.
        let fx = group_fixture(EngageMode::Mention, None, None);
        let mut ev = group_event("cmd-unknown-2");
        ev.message.content = serde_json::json!({"text": "/frobnicate"});
        let out = fx.router.route(ev).await.unwrap();
        assert!(
            matches!(
                out,
                RouteOutcome::Dropped {
                    reason: DropReason::MentionGated
                }
            ),
            "unknown command is not a mention-gate bypass: {out:?}"
        );
    }

    #[tokio::test]
    async fn known_command_with_args_falls_through_as_text() {
        // "/stop the build" is prose for the model, not a command.
        let fx = fixture(SessionMode::Shared);
        let mut ev = event(None, "cmd-args-1");
        ev.message.content = serde_json::json!({"text": "/stop the build"});
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        assert_eq!(rows[0].kind, MessageKind::Chat);
        assert!(rows[0].content.get("control").is_none());
        assert_eq!(rows[0].content["text"], "/stop the build");
    }

    // ---- inbound-file contract: session-local materialization (M18 C3) ----

    /// Session root dir for a routed target, via the fixture's tempdir
    /// layout (`FsSessionRoot` mirrors `SessionPaths`).
    fn session_dir(fx: &Fixture, target: &DeliveredTo) -> std::path::PathBuf {
        fx.tmp
            .path()
            .join("sessions")
            .join(target.agent_group_id.as_uuid().to_string())
            .join(target.session_id.as_uuid().to_string())
    }

    /// Build an inbound chat event carrying a staged attachment whose
    /// bytes live at `staged` (per the channels-core contract).
    fn staged_event(message_id: &str, filename: &str, staged: &std::path::Path) -> InboundEvent {
        let mut ev = event(None, message_id);
        ev.message.content = serde_json::json!({
            "text": "here is the file",
            "attachment": {
                "kind": "telegram.document",
                "file_id": "F-1",
                "filename": filename,
                "staged_path": staged.to_string_lossy(),
                "mime_type": "text/csv",
                "size": 9,
            },
        });
        ev
    }

    /// Stage bytes the way an adapter would (unique dir + file).
    fn stage_bytes(dir: &std::path::Path, filename: &str, bytes: &[u8]) -> std::path::PathBuf {
        let staging = dir.join("staging").join("u1");
        std::fs::create_dir_all(&staging).unwrap();
        let p = staging.join(filename);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[tokio::test]
    async fn staged_attachment_materializes_into_session_inbox() {
        let fx = fixture(SessionMode::Shared);
        let staging_root = tempfile::tempdir().unwrap();
        let staged = stage_bytes(staging_root.path(), "spec.csv", b"id,qty\n1,2");

        let out = fx
            .router
            .route(staged_event("777", "spec.csv", &staged))
            .await
            .unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let target = &sessions[0];

        // The persisted row carries the CONTAINER path and no staged_path.
        let rows = inbound_rows(&fx, target);
        assert_eq!(rows.len(), 1);
        let att = &rows[0].content["attachment"];
        assert_eq!(att["path"], "/data/inbox/777/spec.csv");
        assert!(att.get("staged_path").is_none(), "staged_path stripped");
        assert_eq!(att["filename"], "spec.csv");
        assert_eq!(att["kind"], "telegram.document");

        // The bytes are on disk at the host path corresponding to the
        // container path under the /data session-dir mount.
        let on_disk = session_dir(&fx, target).join("inbox/777/spec.csv");
        assert_eq!(std::fs::read(&on_disk).unwrap(), b"id,qty\n1,2");

        // The staged source (and its unique dir) was consumed.
        assert!(!staged.exists(), "staged file must be removed after route");
        assert!(
            !staged.parent().unwrap().exists(),
            "unique staging dir must be removed after route"
        );
    }

    #[tokio::test]
    async fn staged_attachment_sanitizes_hostile_filename_and_msg_id() {
        let fx = fixture(SessionMode::Shared);
        let staging_root = tempfile::tempdir().unwrap();
        let staged = stage_bytes(staging_root.path(), "payload", b"x");

        // Hostile message id and filename: neither may traverse out of
        // the session inbox or smuggle a path separator.
        let out = fx
            .router
            .route(staged_event("../../../etc", "../../evil.sh", &staged))
            .await
            .unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let target = &sessions[0];
        let rows = inbound_rows(&fx, target);
        let att = &rows[0].content["attachment"];
        let path = att["path"].as_str().unwrap();
        assert_eq!(path, "/data/inbox/etc/evil.sh", "sanitized components");
        let inbox = session_dir(&fx, target).join("inbox");
        let on_disk = inbox.join("etc").join("evil.sh");
        assert!(on_disk.exists(), "file lands inside the session inbox");
        // Nothing escaped the inbox root.
        let canonical = on_disk.canonicalize().unwrap();
        assert!(canonical.starts_with(inbox.canonicalize().unwrap()));
    }

    #[tokio::test]
    async fn staged_attachment_missing_file_routes_with_error_note() {
        let fx = fixture(SessionMode::Shared);
        let out = fx
            .router
            .route(staged_event(
                "778",
                "gone.bin",
                std::path::Path::new("/nonexistent/staging/gone.bin"),
            ))
            .await
            .unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("materialization failure must not drop the message: {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        let att = &rows[0].content["attachment"];
        assert!(att.get("path").is_none(), "no path when bytes unavailable");
        assert!(att.get("staged_path").is_none(), "staged_path stripped");
        let err = att["error"].as_str().unwrap();
        assert!(err.contains("attachment file unavailable"), "{err}");
        // Message text still reached the session.
        assert_eq!(rows[0].content["text"], "here is the file");
    }

    #[tokio::test]
    async fn staged_attachment_fans_out_a_copy_per_session() {
        let fx = fixture(SessionMode::Shared);
        // Second agent group wired to the same messaging group.
        let ag2 = create_ag(
            fx.router.central(),
            CreateAgentGroup {
                name: "second".into(),
                folder: "second".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        upsert_wire(
            fx.router.central(),
            UpsertWiring {
                messaging_group_id: fx.mg_id,
                agent_group_id: ag2.id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();

        let staging_root = tempfile::tempdir().unwrap();
        let staged = stage_bytes(staging_root.path(), "spec.csv", b"fanout");
        let out = fx
            .router
            .route(staged_event("779", "spec.csv", &staged))
            .await
            .unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        assert_eq!(sessions.len(), 2);
        for target in &sessions {
            let on_disk = session_dir(&fx, target).join("inbox/779/spec.csv");
            assert_eq!(
                std::fs::read(&on_disk).unwrap(),
                b"fanout",
                "each fanned-out session gets its own copy"
            );
            let rows = inbound_rows(&fx, target);
            assert_eq!(
                rows[0].content["attachment"]["path"],
                "/data/inbox/779/spec.csv"
            );
        }
        assert!(!staged.exists(), "staged source consumed after fanout");
    }

    #[tokio::test]
    async fn staged_attachment_cleaned_up_even_when_route_drops() {
        // No messaging group: the route drops before any fanout, but the
        // staged file must still be consumed so adapters never leak
        // staging state.
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
        let router = Router::new(db, root);
        let staging_root = tempfile::tempdir().unwrap();
        let staged = stage_bytes(staging_root.path(), "a.txt", b"x");
        let out = router
            .route(staged_event("780", "a.txt", &staged))
            .await
            .unwrap();
        assert!(matches!(
            out,
            RouteOutcome::Dropped {
                reason: DropReason::NoMessagingGroup
            }
        ));
        assert!(!staged.exists(), "staged file removed on dropped route");
    }

    #[tokio::test]
    async fn non_staged_attachment_content_is_untouched() {
        // An attachment WITHOUT the staged_path marker (e.g. a legacy
        // metadata-only payload, or an inline-base64 image from an
        // unmigrated adapter) must route byte-identical.
        let fx = fixture(SessionMode::Shared);
        let mut ev = event(None, "781");
        let content = serde_json::json!({
            "text": "look",
            "attachment": {"kind": "telegram.photo", "data_base64": "aGk=", "mime_type": "image/jpeg"},
        });
        ev.message.content = content.clone();
        let out = fx.router.route(ev).await.unwrap();
        let RouteOutcome::Delivered { sessions } = out else {
            panic!("expected delivered, got {out:?}");
        };
        let rows = inbound_rows(&fx, &sessions[0]);
        assert_eq!(rows[0].content, content);
    }
}
