//! Interactive question + card support.
//!
//! Backs the `ask_user_question` and `send_card` MCP tools. Registers two
//! delivery actions:
//!
//! * `"ask_user_question"` — renders a question card with selectable options
//!   and stores a pending question keyed by `question_id`. Reply handling is
//!   the runner's job (the runner watches `messages_in` for replies that
//!   reference the question), but the module manages timeouts.
//!
//! * `"send_card"` — pass-through action that wraps an arbitrary card payload
//!   into an `OutboundMessage`.

use crate::context::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput, DispatchTarget, Module,
    ModuleContext,
};
use crate::error::ModuleError;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use copperclaw_types::{
    AgentGroupId, ChannelType, MessageId, MessageKind, OutboundMessage, SessionId,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Identifier for an outstanding `ask_user_question` call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuestionId(pub String);

impl QuestionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Where a question came from and where its card went — captured at ask
/// time so the sweep's question-expiry check (M21 F2) can surface a
/// lapsed question back to the right session and channel. Every field is
/// optional because tests (and hypothetical non-delivery callers) may
/// register questions without a live outbound row; the expiry check
/// degrades gracefully when origin data is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOrigin {
    /// Session whose outbound row carried the `ask_user_question` action.
    pub session_id: Option<SessionId>,
    /// Agent group owning that session (needed to open per-session DBs).
    pub agent_group_id: Option<AgentGroupId>,
    /// The `messages_out` id of the System row that rendered the card.
    /// The sweep resolves this to a `seq` so the expiry edit can stamp
    /// the delivered card terminal in place.
    pub message_out_id: Option<MessageId>,
    /// Channel routing the card was dispatched with.
    pub channel_type: Option<ChannelType>,
    pub platform_id: Option<String>,
    pub thread_id: Option<String>,
}

/// A question awaiting a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub id: QuestionId,
    pub title: String,
    pub options: Vec<String>,
    pub asked_at: DateTime<Utc>,
    pub timeout_at: DateTime<Utc>,
    pub answered: Option<String>,
    /// Ask-time provenance for expiry surfacing (M21 F2).
    #[serde(default)]
    pub origin: QuestionOrigin,
}

impl PendingQuestion {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.answered.is_none() && now >= self.timeout_at
    }
}

/// In-memory state for the interactive module. Production hosts can mirror
/// this into `pending_questions` for durability.
#[derive(Debug, Default)]
struct State {
    pending: HashMap<QuestionId, PendingQuestion>,
}

/// Interactive module.
///
/// Cheap to clone: clones share the same underlying pending-question
/// state (the state lives behind an `Arc`). The host relies on this to
/// hand one handle to the module installer (which registers the
/// `ask_user_question` delivery action) and a second handle to the
/// sweep service, whose question-expiry check calls
/// [`Self::sweep_expired`] against the same live set (M21 F2).
#[derive(Clone)]
pub struct InteractiveModule {
    state: Arc<Mutex<State>>,
    default_timeout: Duration,
}

impl Default for InteractiveModule {
    fn default() -> Self {
        // 24 hours by default.
        Self::with_timeout(Duration::seconds(24 * 60 * 60))
    }
}

impl InteractiveModule {
    pub fn with_timeout(default_timeout: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            default_timeout,
        }
    }

    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    /// Register a new pending question. `origin` records where the ask
    /// came from so a later expiry can be surfaced back to the same
    /// session and channel; pass `QuestionOrigin::default()` when no
    /// outbound-row context exists (tests).
    pub fn ask(
        &self,
        id: QuestionId,
        title: String,
        options: Vec<String>,
        origin: QuestionOrigin,
        now: DateTime<Utc>,
    ) -> PendingQuestion {
        let q = PendingQuestion {
            id: id.clone(),
            title,
            options,
            asked_at: now,
            timeout_at: now + self.default_timeout,
            answered: None,
            origin,
        };
        self.state.lock().unwrap().pending.insert(id, q.clone());
        q
    }

    /// Record a reply for an existing question. Returns true if the question
    /// existed and had not previously been answered.
    pub fn answer(&self, id: &QuestionId, reply: String) -> bool {
        let mut state = self.state.lock().unwrap();
        match state.pending.get_mut(id) {
            Some(q) if q.answered.is_none() => {
                q.answered = Some(reply);
                true
            }
            _ => false,
        }
    }

    /// Get a snapshot of a question.
    pub fn get(&self, id: &QuestionId) -> Option<PendingQuestion> {
        self.state.lock().unwrap().pending.get(id).cloned()
    }

    /// Snapshot all pending questions.
    pub fn pending(&self) -> Vec<PendingQuestion> {
        self.state
            .lock()
            .unwrap()
            .pending
            .values()
            .cloned()
            .collect()
    }

    /// Sweep expired questions: every unanswered question whose
    /// `timeout_at` has passed is removed from the pending set and
    /// returned as a full snapshot (M21 F2 — the sweep's expiry check
    /// needs the title + origin to surface the lapse, not just the id).
    /// Answered questions are never selected, and questions still inside
    /// their TTL are untouched. Idempotent: a second call at the same
    /// instant returns an empty list.
    pub fn sweep_expired(&self, now: DateTime<Utc>) -> Vec<PendingQuestion> {
        let mut state = self.state.lock().unwrap();
        let expired_ids: Vec<QuestionId> = state
            .pending
            .iter()
            .filter(|(_, q)| q.is_expired(now))
            .map(|(id, _)| id.clone())
            .collect();
        let mut expired = Vec::with_capacity(expired_ids.len());
        for id in &expired_ids {
            if let Some(q) = state.pending.remove(id) {
                expired.push(q);
            }
        }
        // Deterministic order for callers that surface each expiry.
        expired.sort_by(|a, b| a.asked_at.cmp(&b.asked_at).then(a.id.0.cmp(&b.id.0)));
        expired
    }
}

/// `ask_user_question` action: builds a card message addressed to `to` and
/// records a pending question.
struct AskHandler {
    state: Arc<Mutex<State>>,
    default_timeout: Duration,
}

impl DeliveryActionHandler for AskHandler {
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
        // The runner emits the question id as `id`
        // (`ask_user_question.id`, see copperclaw-runner tools.rs); accept
        // that, falling back to `question_id` for any caller using the
        // internal name. Reading only `question_id` here silently dropped
        // every real `ask_user_question` (card never rendered, no pending
        // row) with "missing question_id".
        let id = input
            .payload
            .get("id")
            .or_else(|| input.payload.get("question_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("interactive", "missing question id (`id`)"))?
            .to_owned();
        let title = input
            .payload
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("interactive", "missing title"))?
            .to_owned();
        let options: Vec<String> = input
            .payload
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|o| o.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if options.is_empty() {
            return Err(ModuleError::other(
                "interactive",
                "ask_user_question requires at least one option",
            ));
        }
        let now = Utc::now();
        let qid = QuestionId(id.clone());
        let dispatch = dispatch_from_payload(&input.payload);
        // Capture ask-time provenance so the sweep's expiry check can
        // route the "this question expired" surfacing (M21 F2). The
        // routing prefers an explicit payload `to` override (that is
        // where the card actually goes) and falls back to the target
        // the delivery service resolved for the row.
        let routed = dispatch.as_ref().unwrap_or(&input.target);
        let origin = QuestionOrigin {
            session_id: input.session_id,
            agent_group_id: input.target.agent_group_id,
            message_out_id: input.row_id,
            channel_type: routed.channel_type.clone(),
            platform_id: routed.platform_id.clone(),
            thread_id: routed.thread_id.clone(),
        };
        let q = PendingQuestion {
            id: qid.clone(),
            title: title.clone(),
            options: options.clone(),
            asked_at: now,
            timeout_at: now + self.default_timeout,
            answered: None,
            origin,
        };
        self.state.lock().unwrap().pending.insert(qid, q);
        Ok(DeliveryActionOutput {
            dispatch,
            message: Some(OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({
                    "card": {
                        "type": "question",
                        "question_id": id,
                        "title": title,
                        "options": options,
                    },
                }),
                files: vec![],
            }),
        })
    }
}

/// `edit` action: registered so the host's delivery service routes the
/// system row to us. The service first tries the channel adapter's typed
/// edit API (`ChannelAdapter::edit_message`); if the adapter returns
/// `Unsupported` (CLI / webhooks / etc.) or the original platform message
/// id can't be located, the service falls through to this handler, which
/// returns a synthetic chat message of the form `"(edit) <new_text>"`.
///
/// The handler intentionally does not look up the adapter or the session
/// DBs itself — modules don't depend on `copperclaw-channels-core` or
/// `copperclaw-db`, so the typed-adapter call lives in the delivery service
/// and the module owns only the fallback shape.
struct EditHandler;

impl DeliveryActionHandler for EditHandler {
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
        let text = input
            .payload
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("interactive", "edit payload missing text"))?
            .to_owned();
        let dispatch = dispatch_from_payload(&input.payload).or_else(|| Some(input.target.clone()));
        Ok(DeliveryActionOutput {
            dispatch,
            message: Some(OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({ "text": format!("(edit) {text}") }),
                files: vec![],
            }),
        })
    }
}

/// `reaction` action: shares the same fallback contract as [`EditHandler`].
/// When the channel adapter doesn't expose a reaction API the service
/// invokes us and we return `"(reaction: <emoji>)"` as a regular chat row.
struct ReactionHandler;

impl DeliveryActionHandler for ReactionHandler {
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
        let emoji = input
            .payload
            .get("emoji")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::other("interactive", "reaction payload missing emoji"))?
            .to_owned();
        let dispatch = dispatch_from_payload(&input.payload).or_else(|| Some(input.target.clone()));
        Ok(DeliveryActionOutput {
            dispatch,
            message: Some(OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({ "text": format!("(reaction: {emoji})") }),
                files: vec![],
            }),
        })
    }
}

/// `send_card` action: wraps payload's `card` field into an outbound message.
struct CardHandler;

impl DeliveryActionHandler for CardHandler {
    fn handle(&self, input: DeliveryActionInput) -> Result<DeliveryActionOutput, ModuleError> {
        let card = input
            .payload
            .get("card")
            .ok_or_else(|| ModuleError::other("interactive", "missing card payload"))?
            .clone();
        let dispatch = dispatch_from_payload(&input.payload);
        Ok(DeliveryActionOutput {
            dispatch,
            message: Some(OutboundMessage {
                kind: MessageKind::Chat,
                content: serde_json::json!({ "card": card }),
                files: vec![],
            }),
        })
    }
}

fn dispatch_from_payload(payload: &serde_json::Value) -> Option<DispatchTarget> {
    let to = payload.get("to")?;
    let channel_type = to
        .get("channel_type")
        .and_then(|v| v.as_str())
        .map(ChannelType::new);
    let platform_id = to
        .get("platform_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let thread_id = to
        .get("thread_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    match (channel_type, platform_id) {
        (Some(ct), Some(pid)) => Some(DispatchTarget::channel(ct, pid, thread_id)),
        _ => None,
    }
}

#[async_trait]
impl Module for InteractiveModule {
    fn name(&self) -> &'static str {
        "interactive"
    }

    async fn install(&self, ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        ctx.register_delivery_action(
            "ask_user_question",
            Arc::new(AskHandler {
                state: Arc::clone(&self.state),
                default_timeout: self.default_timeout,
            }),
        );
        ctx.register_delivery_action("send_card", Arc::new(CardHandler));
        ctx.register_delivery_action("edit", Arc::new(EditHandler));
        ctx.register_delivery_action("reaction", Arc::new(ReactionHandler));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockModuleContext;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn ask_and_answer() {
        let m = InteractiveModule::default();
        let id = QuestionId::new("q-1");
        let q = m.ask(
            id.clone(),
            "?".into(),
            vec!["yes".into(), "no".into()],
            QuestionOrigin::default(),
            now(),
        );
        assert_eq!(q.options.len(), 2);
        assert!(q.answered.is_none());
        assert!(m.answer(&id, "yes".into()));
        let stored = m.get(&id).unwrap();
        assert_eq!(stored.answered.as_deref(), Some("yes"));
    }

    #[test]
    fn double_answer_returns_false() {
        let m = InteractiveModule::default();
        let id = QuestionId::new("q-2");
        m.ask(
            id.clone(),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            now(),
        );
        assert!(m.answer(&id, "a".into()));
        assert!(!m.answer(&id, "a".into()));
    }

    #[test]
    fn answer_unknown_returns_false() {
        let m = InteractiveModule::default();
        assert!(!m.answer(&QuestionId::new("ghost"), "x".into()));
    }

    #[test]
    fn pending_lists_all() {
        let m = InteractiveModule::default();
        m.ask(
            QuestionId::new("q-1"),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            now(),
        );
        m.ask(
            QuestionId::new("q-2"),
            "?".into(),
            vec!["b".into()],
            QuestionOrigin::default(),
            now(),
        );
        assert_eq!(m.pending().len(), 2);
    }

    #[test]
    fn sweep_expired_removes_old_questions() {
        let m = InteractiveModule::with_timeout(Duration::milliseconds(100));
        let id = QuestionId::new("q-1");
        let t0 = now();
        m.ask(
            id.clone(),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0,
        );
        let later = t0 + Duration::seconds(1);
        let expired = m.sweep_expired(later);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, id);
        assert_eq!(expired[0].title, "?");
        assert!(m.pending().is_empty());
        // Idempotent: nothing left to sweep.
        assert!(m.sweep_expired(later).is_empty());
    }

    #[test]
    fn sweep_keeps_answered_questions() {
        let m = InteractiveModule::with_timeout(Duration::milliseconds(100));
        let id = QuestionId::new("q-1");
        let t0 = now();
        m.ask(
            id.clone(),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0,
        );
        m.answer(&id, "a".into());
        let later = t0 + Duration::seconds(1);
        assert!(m.sweep_expired(later).is_empty());
        assert_eq!(m.pending().len(), 1);
    }

    /// TTL honored precisely: a question one second SHY of its timeout is
    /// untouched; the same sweep instant one second LATER selects it.
    #[test]
    fn sweep_expiry_selection_honors_the_ttl_boundary() {
        let m = InteractiveModule::with_timeout(Duration::seconds(60));
        let id = QuestionId::new("q-ttl");
        let t0 = now();
        m.ask(
            id.clone(),
            "pick".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0,
        );
        assert!(
            m.sweep_expired(t0 + Duration::seconds(59)).is_empty(),
            "inside the TTL the question must not be selected",
        );
        assert_eq!(m.pending().len(), 1);
        let expired = m.sweep_expired(t0 + Duration::seconds(60));
        assert_eq!(expired.len(), 1, "at the TTL boundary it lapses");
        assert_eq!(expired[0].id, id);
    }

    /// A mixed pending set: only the lapsed unanswered question is
    /// swept; the fresh one and the answered-but-old one both survive.
    #[test]
    fn sweep_selects_only_lapsed_unanswered_questions() {
        let m = InteractiveModule::with_timeout(Duration::seconds(10));
        let t0 = now();
        let lapsed = QuestionId::new("q-lapsed");
        m.ask(
            lapsed.clone(),
            "old".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0,
        );
        let answered = QuestionId::new("q-answered");
        m.ask(
            answered.clone(),
            "answered".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0,
        );
        m.answer(&answered, "a".into());
        let fresh = QuestionId::new("q-fresh");
        m.ask(
            fresh.clone(),
            "fresh".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0 + Duration::seconds(30),
        );
        let expired = m.sweep_expired(t0 + Duration::seconds(20));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, lapsed);
        let mut left: Vec<String> = m.pending().into_iter().map(|q| q.id.0).collect();
        left.sort();
        assert_eq!(left, vec!["q-answered".to_string(), "q-fresh".to_string()]);
    }

    /// Clones share pending state — the seam boot relies on to hand one
    /// handle to the module installer and one to the sweep (M21 F2).
    #[test]
    fn clones_share_pending_state() {
        let m = InteractiveModule::with_timeout(Duration::seconds(1));
        let sweeper = m.clone();
        let t0 = now();
        m.ask(
            QuestionId::new("q-shared"),
            "?".into(),
            vec!["a".into()],
            QuestionOrigin::default(),
            t0,
        );
        let expired = sweeper.sweep_expired(t0 + Duration::seconds(2));
        assert_eq!(expired.len(), 1, "clone must see the original's ask");
        assert!(
            m.pending().is_empty(),
            "sweep through the clone drains both"
        );
    }

    #[test]
    fn is_expired_logic() {
        let now = now();
        let q = PendingQuestion {
            id: QuestionId::new("q"),
            title: "t".into(),
            options: vec!["a".into()],
            asked_at: now,
            timeout_at: now + Duration::seconds(10),
            answered: None,
            origin: QuestionOrigin::default(),
        };
        assert!(!q.is_expired(now));
        assert!(q.is_expired(now + Duration::seconds(11)));
        let mut q2 = q.clone();
        q2.answered = Some("a".into());
        assert!(!q2.is_expired(now + Duration::seconds(11)));
    }

    #[tokio::test]
    async fn install_registers_four_actions() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let mut actions = ctx.delivery_actions();
        actions.sort();
        assert_eq!(
            actions,
            vec!["ask_user_question", "edit", "reaction", "send_card"]
        );
    }

    #[tokio::test]
    async fn ask_handler_records_and_builds_card() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers
            .iter()
            .find(|(n, _)| n == "ask_user_question")
            .unwrap();
        let out = handler
            .handle(DeliveryActionInput {
                action: "ask_user_question".into(),
                payload: serde_json::json!({
                    "question_id": "q-abc",
                    "title": "Approve?",
                    "options": ["yes", "no"],
                    "to": {"channel_type": "slack", "platform_id": "U-1"},
                }),
                target: DispatchTarget {
                    channel_type: None,
                    platform_id: None,
                    thread_id: None,
                    agent_group_id: None,
                },
                session_id: None,
                row_id: None,
            })
            .unwrap();
        let dispatch = out.dispatch.unwrap();
        assert_eq!(dispatch.platform_id.as_deref(), Some("U-1"));
        let msg = out.message.unwrap();
        assert_eq!(msg.content["card"]["question_id"], "q-abc");
        let stored = m.get(&QuestionId::new("q-abc")).unwrap();
        // Origin routing follows the explicit payload `to` override.
        assert_eq!(
            stored.origin.channel_type.as_ref().map(ChannelType::as_str),
            Some("slack")
        );
        assert_eq!(stored.origin.platform_id.as_deref(), Some("U-1"));
    }

    #[tokio::test]
    async fn ask_handler_accepts_runner_id_field() {
        // Regression: the runner emits the question id as `id` (not
        // `question_id`); reading only `question_id` dropped every real
        // ask_user_question with "missing question_id".
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers
            .iter()
            .find(|(n, _)| n == "ask_user_question")
            .unwrap();
        let session_id = SessionId::new();
        let agent_group_id = AgentGroupId::new();
        let row_id = MessageId::new();
        let out = handler
            .handle(DeliveryActionInput {
                action: "ask_user_question".into(),
                payload: serde_json::json!({
                    "id": "q_runner",
                    "title": "Pick layout?",
                    "options": ["a", "b"],
                }),
                target: DispatchTarget {
                    channel_type: Some("telegram".into()),
                    platform_id: Some("chat-1".into()),
                    thread_id: None,
                    agent_group_id: Some(agent_group_id),
                },
                session_id: Some(session_id),
                row_id: Some(row_id),
            })
            .unwrap();
        let msg = out.message.expect("question card must be built");
        assert_eq!(msg.content["card"]["question_id"], "q_runner");
        let stored = m.get(&QuestionId::new("q_runner")).unwrap();
        // With no payload `to`, the origin captures the resolved target
        // plus the delivery-threaded session / row identifiers — the
        // provenance the sweep's expiry check routes with (M21 F2).
        assert_eq!(stored.origin.session_id, Some(session_id));
        assert_eq!(stored.origin.agent_group_id, Some(agent_group_id));
        assert_eq!(stored.origin.message_out_id, Some(row_id));
        assert_eq!(
            stored.origin.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(stored.origin.platform_id.as_deref(), Some("chat-1"));
    }

    #[tokio::test]
    async fn ask_handler_rejects_missing_fields() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers
            .iter()
            .find(|(n, _)| n == "ask_user_question")
            .unwrap();
        let bad = handler.handle(DeliveryActionInput {
            action: "ask_user_question".into(),
            payload: serde_json::json!({}),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: None,
            },
            session_id: None,
            row_id: None,
        });
        assert!(bad.is_err());
        let bad = handler.handle(DeliveryActionInput {
            action: "ask_user_question".into(),
            payload: serde_json::json!({"question_id": "x"}),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: None,
            },
            session_id: None,
            row_id: None,
        });
        assert!(bad.is_err());
        let bad = handler.handle(DeliveryActionInput {
            action: "ask_user_question".into(),
            payload: serde_json::json!({"question_id": "x", "title": "t", "options": []}),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: None,
            },
            session_id: None,
            row_id: None,
        });
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn card_handler_wraps_card() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers.iter().find(|(n, _)| n == "send_card").unwrap();
        let out = handler
            .handle(DeliveryActionInput {
                action: "send_card".into(),
                payload: serde_json::json!({
                    "card": {"type": "image", "url": "https://example.com/x.png"},
                }),
                target: DispatchTarget {
                    channel_type: None,
                    platform_id: None,
                    thread_id: None,
                    agent_group_id: None,
                },
                session_id: None,
                row_id: None,
            })
            .unwrap();
        let msg = out.message.unwrap();
        assert_eq!(msg.content["card"]["type"], "image");
    }

    #[tokio::test]
    async fn card_handler_rejects_missing_card() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers.iter().find(|(n, _)| n == "send_card").unwrap();
        let out = handler.handle(DeliveryActionInput {
            action: "send_card".into(),
            payload: serde_json::json!({}),
            target: DispatchTarget {
                channel_type: None,
                platform_id: None,
                thread_id: None,
                agent_group_id: None,
            },
            session_id: None,
            row_id: None,
        });
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn edit_handler_routes_to_correct_adapter() {
        // The handler doesn't drive the adapter directly — that's the
        // delivery service's job. This test pins the contract the service
        // depends on: given a target whose channel_type is "telegram",
        // the dispatch surfaced back to the service preserves the channel
        // type so the service knows which adapter to call.
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers.iter().find(|(n, _)| n == "edit").unwrap();
        let out = handler
            .handle(DeliveryActionInput {
                action: "edit".into(),
                payload: serde_json::json!({ "seq": 7, "text": "edited" }),
                target: DispatchTarget {
                    channel_type: Some(ChannelType::new("telegram")),
                    platform_id: Some("chat-1".into()),
                    thread_id: None,
                    agent_group_id: None,
                },
                session_id: None,
                row_id: None,
            })
            .unwrap();
        let dispatch = out.dispatch.expect("dispatch target");
        assert_eq!(
            dispatch.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(dispatch.platform_id.as_deref(), Some("chat-1"));
        let msg = out.message.expect("fallback message");
        assert_eq!(msg.kind, MessageKind::Chat);
        assert_eq!(msg.content["text"].as_str().unwrap(), "(edit) edited");
    }

    #[tokio::test]
    async fn reaction_handler_routes_to_correct_adapter() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers.iter().find(|(n, _)| n == "reaction").unwrap();
        let out = handler
            .handle(DeliveryActionInput {
                action: "reaction".into(),
                payload: serde_json::json!({ "seq": 7, "emoji": "thumbsup" }),
                target: DispatchTarget {
                    channel_type: Some(ChannelType::new("slack")),
                    platform_id: Some("C-1".into()),
                    thread_id: Some("100.0".into()),
                    agent_group_id: None,
                },
                session_id: None,
                row_id: None,
            })
            .unwrap();
        let dispatch = out.dispatch.expect("dispatch target");
        assert_eq!(
            dispatch.channel_type.as_ref().map(ChannelType::as_str),
            Some("slack")
        );
        assert_eq!(dispatch.thread_id.as_deref(), Some("100.0"));
        let msg = out.message.expect("fallback message");
        assert_eq!(
            msg.content["text"].as_str().unwrap(),
            "(reaction: thumbsup)"
        );
    }

    #[tokio::test]
    async fn edit_handler_falls_back_when_external_id_missing() {
        // This test stands in for "the service couldn't find an external_id,
        // so it invoked the handler to obtain a fallback message". The
        // handler doesn't observe the external_id directly — it just
        // emits the `"(edit) <text>"` chat message; we verify the shape.
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers.iter().find(|(n, _)| n == "edit").unwrap();
        let out = handler
            .handle(DeliveryActionInput {
                action: "edit".into(),
                payload: serde_json::json!({ "seq": 99, "text": "second try" }),
                target: DispatchTarget {
                    channel_type: Some(ChannelType::new("cli")),
                    platform_id: Some("plat".into()),
                    thread_id: None,
                    agent_group_id: None,
                },
                session_id: None,
                row_id: None,
            })
            .unwrap();
        let msg = out.message.expect("fallback message");
        assert_eq!(msg.kind, MessageKind::Chat);
        assert_eq!(msg.content["text"].as_str().unwrap(), "(edit) second try");
        // Dispatch defaults back to the inbound target so the service can
        // route the fallback chat row to the same channel.
        let dispatch = out.dispatch.expect("dispatch target");
        assert_eq!(
            dispatch.channel_type.as_ref().map(ChannelType::as_str),
            Some("cli")
        );
    }

    #[tokio::test]
    async fn edit_handler_rejects_missing_text() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers.iter().find(|(n, _)| n == "edit").unwrap();
        let err = handler
            .handle(DeliveryActionInput {
                action: "edit".into(),
                payload: serde_json::json!({ "seq": 1 }),
                target: DispatchTarget {
                    channel_type: None,
                    platform_id: None,
                    thread_id: None,
                    agent_group_id: None,
                },
                session_id: None,
                row_id: None,
            })
            .unwrap_err();
        assert!(format!("{err}").contains("text"));
    }

    #[tokio::test]
    async fn reaction_handler_rejects_missing_emoji() {
        let m = InteractiveModule::default();
        let ctx = MockModuleContext::new();
        m.install(ctx.clone()).await.unwrap();
        let handlers = ctx.action_handlers.lock().unwrap();
        let (_, handler) = handlers.iter().find(|(n, _)| n == "reaction").unwrap();
        let err = handler
            .handle(DeliveryActionInput {
                action: "reaction".into(),
                payload: serde_json::json!({ "seq": 1 }),
                target: DispatchTarget {
                    channel_type: None,
                    platform_id: None,
                    thread_id: None,
                    agent_group_id: None,
                },
                session_id: None,
                row_id: None,
            })
            .unwrap_err();
        assert!(format!("{err}").contains("emoji"));
    }

    #[test]
    fn question_id_serde() {
        let id = QuestionId::new("abc");
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"abc\"");
        let back: QuestionId = serde_json::from_str(&s).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn default_timeout_accessor() {
        let m = InteractiveModule::with_timeout(Duration::seconds(99));
        assert_eq!(m.default_timeout(), Duration::seconds(99));
    }

    #[test]
    fn name_is_stable() {
        assert_eq!(InteractiveModule::default().name(), "interactive");
    }
}
