//! Routing algorithm — turn an `InboundEvent` into one or more
//! `messages_in` writes.
//!
//! See `PLAN.md` § 6 T3 for the high-level description. The routing pipeline
//! lives in [`Router::route`]; the supporting types in this module describe
//! the outcome the caller (typically a channel adapter) sees.

use crate::debounce::{DebounceKey, Debouncer, InflightKey, InflightSet};
use crate::error::RouterError;
use crate::hooks::HookChain;
use crate::session::SessionRoot;

use dashmap::DashMap;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::dropped_messages::{
    insert as insert_dropped, InsertDroppedMessage,
};
use copperclaw_db::tables::messages_in::{insert as insert_in, WriteInbound};
use copperclaw_db::tables::messaging_group_agents::{list_for_mg as list_wirings, MessagingGroupAgent};
use copperclaw_db::tables::messaging_groups::get_by_platform;
use copperclaw_db::tables::sessions::{
    create as create_session, find_for_agent, CreateSession,
};
use copperclaw_db::tables::unregistered_senders::{
    upsert as upsert_unregistered, UpsertUnregisteredSender,
};
use copperclaw_modules::context::{
    ChannelRequestCtx, GateCtx, GateDecision, SenderScopeCtx, SenderScopeDecision,
};
use copperclaw_types::{
    AgentGroupId, InboundEvent, MessageId, MessageKind, MessagingGroupId, SessionId, SessionMode,
};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Outcome of routing an inbound event. The caller (a channel adapter or
/// test harness) gets one of these per call and uses it for logging and to
/// decide whether to ack the platform message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    /// Inbound event was routed to one or more sessions. `sessions` holds the
    /// per-target write result.
    Delivered { sessions: Vec<DeliveredTo> },
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
}

impl Router {
    /// Construct a router. Modules wire their hooks via [`Self::hooks`] /
    /// [`Self::hooks_mut`] after construction; the router stays usable while
    /// no hooks are wired (defaults to allow-all).
    pub fn new(central: CentralDb, session_paths: Arc<dyn SessionRoot + Send + Sync>) -> Self {
        Self {
            central,
            session_paths,
            hooks: HookChain::new(),
            debounce: Debouncer::new(),
            inflight: InflightSet::new(),
            fanout_seq: AtomicI64::new(0),
        }
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
            self.record_drop(&event, None, None, "no_messaging_group")?;
            return Ok(RouteOutcome::Dropped {
                reason: DropReason::NoMessagingGroup,
            });
        };

        // 3. Resolve sender to a UserId (if any).
        let user_id = self.hooks.run_sender_resolver(&event);

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
                    self.record_drop(&event, Some(mg.id), None, &reason)?;
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
            self.record_drop(&event, Some(mg.id), None, "no_agents")?;
            return Ok(RouteOutcome::Dropped {
                reason: DropReason::NoAgents,
            });
        }

        // 6. Fanout to each wiring.
        let mut sessions = Vec::with_capacity(wirings.len());
        let mut last_pending: Option<PendingReason> = None;
        for wiring in wirings {
            self.fanout_seq.fetch_add(1, Ordering::Relaxed);
            match self.route_one(&event, &mg.id, &wiring, user_id)? {
                FanoutOutcome::Delivered(d) => sessions.push(d),
                FanoutOutcome::Dropped(reason) => {
                    self.record_drop(
                        &event,
                        Some(mg.id),
                        Some(wiring.agent_group_id),
                        &drop_reason_label(&reason),
                    )?;
                    if sessions.is_empty() {
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

        if sessions.is_empty() {
            if let Some(reason) = last_pending {
                return Ok(RouteOutcome::Pending { reason });
            }
            // Defensive: should not happen — at least one fanout result
            // must classify the event.
            return Ok(RouteOutcome::Dropped {
                reason: DropReason::NoAgents,
            });
        }
        Ok(RouteOutcome::Delivered { sessions })
    }

    /// Per-wiring delivery.
    fn route_one(
        &self,
        event: &InboundEvent,
        mg_id: &MessagingGroupId,
        wiring: &MessagingGroupAgent,
        user_id: Option<copperclaw_types::UserId>,
    ) -> Result<FanoutOutcome, RouterError> {
        // Access gate per agent group.
        if self.hooks.has_access_gate() {
            let ctx = GateCtx {
                user: user_id,
                agent_group_id: Some(wiring.agent_group_id),
                messaging_group_id: Some(*mg_id),
                op: "deliver_message".into(),
            };
            match self.hooks.run_access_gate(ctx) {
                Some(GateDecision::Deny(reason)) => {
                    return Ok(FanoutOutcome::Dropped(DropReason::AccessDenied(reason)));
                }
                Some(GateDecision::Defer | GateDecision::Allow) | None => {}
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
        let reply_to_id = event
            .reply_to
            .as_ref()
            .and_then(|r| r.thread_id.clone());
        let write = WriteInbound {
            id: message_id,
            kind: event.message.kind,
            timestamp: event.message.timestamp,
            content: event.message.content.clone(),
            trigger: true,
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

        copperclaw_metrics::inc_messages_inbound(event.channel_type.as_str());

        Ok(FanoutOutcome::Delivered(DeliveredTo {
            agent_group_id: session.agent_group_id,
            session_id: session.id,
            message_id,
            seq,
        }))
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

        if let Some(s) =
            find_for_agent(&self.central, agent_group_id, search_mg, search_thread.as_deref())?
        {
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
        pool.with_conn(|conn| {
            copperclaw_db::tables::session_routing::write(conn, &routing)
        })
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
    use copperclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use copperclaw_db::tables::messaging_group_agents::{upsert as upsert_wire, UpsertWiring};
    use copperclaw_db::tables::messaging_groups::{upsert as upsert_mg, UpsertMessagingGroup};
    use copperclaw_modules::context::{
        GateDecision, InterceptorDecision, SenderScopeDecision,
    };
    use copperclaw_types::{
        ChannelType, EngageMode, InboundMessage, MessageKind, SenderIdentity, SessionMode, UserId,
    };
    use std::sync::Arc;

    struct Fixture {
        router: Router,
        // Keep tempdir alive for the duration of the test.
        _tmp: tempfile::TempDir,
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
            _tmp: tmp,
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
        let rows = copperclaw_db::tables::unregistered_senders::list(fx.router.central(), None)
            .unwrap();
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
        let rows = copperclaw_db::tables::unregistered_senders::list(fx.router.central(), None)
            .unwrap();
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
        assert_eq!(scope_reason(Some(&SenderScopeDecision::Allow)), "scope_allow");
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
        let dropped = copperclaw_db::tables::dropped_messages::list(router.central(), None).unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].reason, "no_messaging_group");
    }

    #[tokio::test]
    async fn router_debug_renders() {
        let fx = fixture(SessionMode::Shared);
        let s = format!("{:?}", fx.router);
        assert!(s.contains("Router"));
    }
}
