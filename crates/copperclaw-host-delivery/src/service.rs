//! `DeliveryService` — owns the active and sweep loops.
//!
//! Per-session work runs in [`DeliveryService::process_session_once`] (see
//! `loops.rs` for the periodic schedulers that drive it).

use crate::dispatch::{AdapterResolver, HostDispatcher};
use crate::error::DeliveryError;
use crate::system_actions::{parse_system_content, ParsedAction};
use dashmap::DashMap;
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, Card, ChannelAdapter, DiffCard, ErrorCard, ErrorCardKind,
    ThinkingBlock, TodoList,
};
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{open_inbound, open_outbound, SessionPaths};
use copperclaw_db::tables::{delivered, messages_in, messages_out, session_routing};
use copperclaw_modules::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryDispatcher, DispatchTarget,
};
use copperclaw_types::{
    AgentGroupId, ChannelType, ContainerStatus, MessageId, MessageKind, MessageOutRow,
    OutboundMessage, Session, SessionId, SessionStatus,
};
use rusqlite::{Connection, OptionalExtension};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Active loop poll interval (milliseconds). Loops only running sessions.
pub const ACTIVE_POLL_MS: u64 = 1_000;
/// Sweep loop poll interval (milliseconds). Loops all active sessions.
pub const SWEEP_POLL_MS: u64 = 60_000;
/// Hard cap on a single delivery attempt's lifetime, used both for re-entry
/// guard expiry and as the upper bound for exponential backoff.
pub const ABSOLUTE_CEILING_MS: u64 = 1_800_000;
/// How many times we retry an adapter-level failure before giving up.
pub const MAX_DELIVERY_ATTEMPTS: u32 = 3;
/// Base value for exponential backoff between retries.
pub const BACKOFF_BASE_MS: u64 = 5_000;

/// A pair `(session_id, message_out_id)` used to dedupe concurrent attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryKey {
    pub session_id: SessionId,
    pub msg_id: MessageId,
}

impl DeliveryKey {
    pub fn new(session_id: SessionId, msg_id: MessageId) -> Self {
        Self { session_id, msg_id }
    }
}

/// Filesystem-level abstraction over per-session DBs. The host provides a
/// concrete implementation that knows the data root; tests use
/// [`InMemorySessionRoot`] which keeps each session in its own tempdir.
pub trait SessionRoot: Send + Sync {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, DeliveryError>;

    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, DeliveryError>;
}

/// A thin handle to a per-session database file. Each `connect()` call opens
/// a fresh rusqlite `Connection`; per-session DBs see very low concurrency
/// (one host writer at a time) so pooling is not warranted.
#[derive(Debug, Clone)]
pub struct SessionPool {
    paths: SessionPaths,
    kind: PoolKind,
}

#[derive(Debug, Clone, Copy)]
enum PoolKind {
    Inbound,
    Outbound,
}

impl SessionPool {
    pub fn inbound(paths: SessionPaths) -> Self {
        Self {
            paths,
            kind: PoolKind::Inbound,
        }
    }

    pub fn outbound(paths: SessionPaths) -> Self {
        Self {
            paths,
            kind: PoolKind::Outbound,
        }
    }

    /// Open a fresh connection. Inbound pools enforce `journal_mode=DELETE`.
    pub fn connect(&self) -> Result<Connection, DeliveryError> {
        let conn = match self.kind {
            PoolKind::Inbound => open_inbound(&self.paths)?,
            PoolKind::Outbound => open_outbound(&self.paths)?,
        };
        Ok(conn)
    }

    pub fn paths(&self) -> &SessionPaths {
        &self.paths
    }
}

/// Default `SessionRoot` backed by a data-root directory on disk.
pub struct FsSessionRoot {
    data_root: std::path::PathBuf,
}

impl FsSessionRoot {
    pub fn new(data_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            data_root: data_root.into(),
        }
    }
}

impl SessionRoot for FsSessionRoot {
    fn outbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, DeliveryError> {
        let paths = SessionPaths::new(&self.data_root, *agent_group_id, *session_id);
        Ok(SessionPool::outbound(paths))
    }

    fn inbound_pool(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Result<SessionPool, DeliveryError> {
        let paths = SessionPaths::new(&self.data_root, *agent_group_id, *session_id);
        Ok(SessionPool::inbound(paths))
    }
}

/// Outcome of a single `process_session_once` invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryReport {
    /// Rows that were successfully delivered and recorded as `status="ok"`.
    pub delivered: usize,
    /// Rows that exhausted their retry budget and were recorded as `status="failed"`.
    pub failed: usize,
    /// Rows that the adapter deferred (rate-limit, transport blip). Tries left.
    pub deferred: usize,
}

impl DeliveryReport {
    /// Total rows considered. (May exceed delivered + failed + deferred when
    /// rows were skipped by the re-entry guard, but on a clean processing
    /// pass every counted row falls into exactly one bucket.)
    pub fn total(self) -> usize {
        self.delivered + self.failed + self.deferred
    }
}

/// Track in-memory retry state for in-flight messages.
#[derive(Debug, Clone)]
struct RetryState {
    /// Number of attempts already made (>= 1).
    tries: u32,
    /// `Instant` after which the row may be retried.
    not_before: Instant,
    /// Number of chat-split chunks already successfully delivered for the
    /// CURRENT outbound row. Used by `dispatch_chat` to resume mid-split
    /// after a retryable adapter failure (e.g. `AdapterError::Rate` on
    /// chunk 1 of 3) without re-sending the earlier chunks. Naturally
    /// scoped per `(session_id, msg_id)` because that's the
    /// [`DeliveryKey`] the `retries` map is keyed by — each outbound row
    /// owns exactly one chunk-progress counter. Cleared (with the rest
    /// of the entry) when the row is marked delivered or failed in
    /// `process_session_once`.
    chunks_sent: u32,
    /// Platform-side message id from the FIRST chunk's successful
    /// `deliver()` call. The delivery loop records THIS id in the
    /// `delivered` row so subsequent `edit_message` / `add_reaction`
    /// targets the same anchor message every time, even across retries
    /// where the local in-process state would otherwise lose it
    /// (the success of chunk 0 happens in attempt N, but the row only
    /// reaches `delivered::insert` after attempt N+M when the final
    /// chunk lands). Persisted across retries to survive that gap.
    first_chunk_pid: Option<String>,
}

/// Outcome of [`DeliveryService::try_action_via_adapter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionAdapterOutcome {
    /// Adapter call succeeded; caller marks the row delivered.
    Done,
    /// Adapter could not service the request (Unsupported / missing
    /// `external_id` / missing payload field); caller falls through to the
    /// registered-handler path so a fallback chat message can be emitted.
    FallThrough,
}

/// Decision the host wants for a not-yet-delivered row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferOutcome {
    /// Retry after a backoff window — leave the row in place.
    Defer,
    /// Out of retries — record `delivered{status="failed"}`.
    Fail,
}

/// Compute the next delay (milliseconds) for the given `tries` value.
///
/// `BACKOFF_BASE_MS * 2.pow(tries - 1)`, capped at `ABSOLUTE_CEILING_MS`.
fn backoff_delay_ms(tries: u32) -> u64 {
    let exp = tries.saturating_sub(1).min(31);
    let scaled = BACKOFF_BASE_MS.saturating_mul(1u64 << exp);
    scaled.min(ABSOLUTE_CEILING_MS)
}

/// The delivery service — public entry point for the host.
pub struct DeliveryService {
    central: CentralDb,
    session_paths: Arc<dyn SessionRoot>,
    adapters: DashMap<ChannelType, Arc<dyn ChannelAdapter>>,
    actions: DashMap<String, Arc<dyn DeliveryActionHandler>>,
    inflight: DashMap<DeliveryKey, Instant>,
    retries: DashMap<DeliveryKey, RetryState>,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    /// When `true`, a failed `install_packages` / `add_mcp_server`
    /// apply is surfaced as a `DeliveryError::SystemAction` so the
    /// outer loop records the row as failed (and the existing retry
    /// path can have another go) rather than recording the failure
    /// in-line. Initialised from `COPPERCLAW_SELFMOD_HARD_FAIL` at
    /// construct time; an `AtomicBool` so tests can flip it without
    /// touching the process env. Default off.
    selfmod_hard_fail: AtomicBool,
}

impl DeliveryService {
    /// Construct a new delivery service.
    ///
    /// The `adapters` map is owned by the service — the host populates it
    /// after `ChannelFactory::init` returns. The `dispatcher` is exposed back
    /// to modules through [`DeliveryService::dispatcher`].
    ///
    /// In practice the host will:
    /// 1. Create the [`ChannelRegistry`](copperclaw_channels_core::ChannelRegistry) and
    ///    register every channel factory.
    /// 2. For each configured channel, call `factory.init(setup).await` and
    ///    insert the resulting `Arc<dyn ChannelAdapter>` into `adapters`.
    /// 3. Construct the dispatcher (e.g. with [`HostDispatcher::new`]) using
    ///    a resolver that reads the same `adapters` map.
    /// 4. Pass `adapters` and `dispatcher` here.
    pub fn new(
        central: CentralDb,
        session_paths: Arc<dyn SessionRoot>,
        adapters: DashMap<ChannelType, Arc<dyn ChannelAdapter>>,
        dispatcher: Arc<dyn DeliveryDispatcher>,
    ) -> Arc<Self> {
        Arc::new(Self {
            central,
            session_paths,
            adapters,
            actions: DashMap::new(),
            inflight: DashMap::new(),
            retries: DashMap::new(),
            dispatcher,
            selfmod_hard_fail: AtomicBool::new(selfmod_hard_fail_from_env()),
        })
    }

    /// Convenience constructor that wires a default [`HostDispatcher`] backed
    /// by the service's adapter map.
    ///
    /// **Caveat:** because [`DashMap`] is `Clone`-by-copying entries (not by
    /// reference), the dispatcher receives a snapshot of `initial_adapters`;
    /// adapters registered afterwards via [`DeliveryService::register_adapter`]
    /// are not visible to the dispatcher's resolver. Hosts that need that
    /// behavior should instead call [`DeliveryService::new`] with a custom
    /// `dispatcher`.
    pub fn with_default_dispatcher(
        central: CentralDb,
        session_paths: Arc<dyn SessionRoot>,
        initial_adapters: Vec<(ChannelType, Arc<dyn ChannelAdapter>)>,
    ) -> Arc<Self> {
        let dispatcher_map: DashMap<ChannelType, Arc<dyn ChannelAdapter>> = DashMap::new();
        let service_map: DashMap<ChannelType, Arc<dyn ChannelAdapter>> = DashMap::new();
        for (ct, adapter) in initial_adapters {
            dispatcher_map.insert(ct.clone(), Arc::clone(&adapter));
            service_map.insert(ct, adapter);
        }
        let resolver_map = Arc::new(dispatcher_map);
        let resolver: AdapterResolver = {
            let map = Arc::clone(&resolver_map);
            Arc::new(move |ct| map.get(ct).map(|r| r.clone()))
        };
        let dispatcher: Arc<dyn DeliveryDispatcher> = Arc::new(HostDispatcher::new(resolver));
        Arc::new(Self {
            central,
            session_paths,
            adapters: service_map,
            actions: DashMap::new(),
            inflight: DashMap::new(),
            retries: DashMap::new(),
            dispatcher,
            selfmod_hard_fail: AtomicBool::new(selfmod_hard_fail_from_env()),
        })
    }

    /// Reusable dispatcher handle suitable to hand to modules.
    pub fn dispatcher(&self) -> Arc<dyn DeliveryDispatcher> {
        Arc::clone(&self.dispatcher)
    }

    /// Register or replace a delivery action handler.
    pub fn register_action(&self, name: &str, handler: Arc<dyn DeliveryActionHandler>) {
        self.actions.insert(name.to_owned(), handler);
    }

    /// Look up a registered action handler by name. Useful for tests.
    pub fn action(&self, name: &str) -> Option<Arc<dyn DeliveryActionHandler>> {
        self.actions.get(name).map(|r| r.clone())
    }

    /// Look up an adapter by channel type.
    pub fn adapter(&self, channel_type: &ChannelType) -> Option<Arc<dyn ChannelAdapter>> {
        self.adapters.get(channel_type).map(|r| r.clone())
    }

    /// Insert / replace an adapter at runtime (used by the host during boot).
    pub fn register_adapter(&self, channel_type: ChannelType, adapter: Arc<dyn ChannelAdapter>) {
        self.adapters.insert(channel_type, adapter);
    }

    /// Number of rows the service currently considers in-flight.
    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// Whether the service was started with `COPPERCLAW_SELFMOD_HARD_FAIL`
    /// enabled. Exposed for tests and for `cclaw doctor` to report.
    pub fn selfmod_hard_fail(&self) -> bool {
        self.selfmod_hard_fail.load(Ordering::Relaxed)
    }

    /// Override the `selfmod_hard_fail` flag after construction. Used
    /// by tests to flip the mode without touching the process env.
    #[doc(hidden)]
    pub fn set_selfmod_hard_fail(&self, on: bool) {
        self.selfmod_hard_fail.store(on, Ordering::Relaxed);
    }

    /// Read-only view of the central DB. Useful for tests.
    pub fn central(&self) -> &CentralDb {
        &self.central
    }

    /// Access the session-paths abstraction. Useful for tests and for the
    /// loops that need to open per-session DBs directly.
    pub fn session_paths(&self) -> &Arc<dyn SessionRoot> {
        &self.session_paths
    }

    /// Process the outbound queue of a single session once.
    ///
    /// Returns a `DeliveryReport` summarising the number of rows delivered,
    /// failed, or deferred during this pass.
    // The `MessageKind` match below is intentionally heavily documented:
    // each arm spells out why that surface exists and where its native
    // rendering lives. Splitting the arms into one-liner helpers loses
    // that context for marginal-at-best readability; keep the body long
    // and explicit.
    #[allow(clippy::too_many_lines)]
    pub async fn process_session_once(
        &self,
        sess: &Session,
    ) -> Result<DeliveryReport, DeliveryError> {
        let mut report = DeliveryReport::default();

        let outbound_pool = self
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)?;
        let inbound_pool = self
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)?;

        let (rows, delivered_ids, routing) = {
            let out_conn = outbound_pool.connect()?;
            let in_conn = inbound_pool.connect()?;
            let rows = messages_out::list_due(&out_conn)?;
            let delivered_ids = delivered::get_delivered_ids(&in_conn)?;
            let routing = session_routing::read(&in_conn)?;
            (rows, delivered_ids, routing)
        };

        let now = Instant::now();
        for row in rows {
            if delivered_ids.contains(&row.id) {
                continue;
            }
            let key = DeliveryKey::new(sess.id, row.id);

            // Re-entry guard.
            if let Some(started) = self.inflight.get(&key).map(|r| *r) {
                if now.duration_since(started) < Duration::from_millis(ABSOLUTE_CEILING_MS) {
                    continue;
                }
                // Otherwise treat as a stale entry and continue.
            }

            // Backoff guard.
            if let Some(not_before) =
                self.retries.get(&key).map(|r| r.not_before)
            {
                if now < not_before {
                    report.deferred += 1;
                    continue;
                }
            }

            self.inflight.insert(key, now);
            let result = self
                .process_row(sess, &row, routing.as_ref(), &inbound_pool)
                .await;
            self.inflight.remove(&key);

            let channel_label = row
                .channel_type
                .as_ref()
                .map_or_else(|| "unknown".to_owned(), |ct| ct.as_str().to_owned());

            match result {
                Ok(()) => {
                    self.retries.remove(&key);
                    report.delivered += 1;
                    copperclaw_metrics::inc_messages_outbound(&channel_label);
                }
                Err(err) if err.is_retryable() => {
                    let outcome = self.bump_retry(&key, err.retry_after_secs());
                    match outcome {
                        DeferOutcome::Defer => {
                            report.deferred += 1;
                            debug!(?err, ?row.id, "deferring retryable delivery failure");
                        }
                        DeferOutcome::Fail => {
                            let in_conn = inbound_pool.connect()?;
                            // Keep the `failed` row insertion — operators
                            // rely on `cclaw dropped-messages` reading
                            // these. Slice-3.3 ADDS a user-visible
                            // ErrorCard on top so the user actually sees
                            // that their message didn't get out.
                            delivered::insert(&in_conn, row.id, None, "failed")?;
                            self.retries.remove(&key);
                            report.failed += 1;
                            copperclaw_metrics::inc_delivery_failed(&channel_label);
                            warn!(?err, ?row.id, "exhausted retry budget, marking failed");
                            // Best-effort: emit an Error-kind outbound
                            // row addressed back at the originating
                            // channel so the next delivery pass renders
                            // it visibly to the user. Swallow errors —
                            // an emit failure here can't be allowed to
                            // poison the delivery loop, and the `failed`
                            // row above is the load-bearing record.
                            if let Err(emit_err) = self.emit_delivery_failure_error_card(
                                sess,
                                &row,
                                &err.to_string(),
                            ) {
                                warn!(
                                    ?emit_err,
                                    ?row.id,
                                    "could not emit retry-exhaustion ErrorCard"
                                );
                            }
                        }
                    }
                }
                Err(DeliveryError::SystemAction(reason)) => {
                    // System-action parse failures are bugs, not transient
                    // adapter blips — record once and don't keep retrying.
                    let in_conn = inbound_pool.connect()?;
                    delivered::insert(&in_conn, row.id, None, "failed")?;
                    self.retries.remove(&key);
                    report.failed += 1;
                    copperclaw_metrics::inc_delivery_failed(&channel_label);
                    warn!(reason, ?row.id, "system action failed");
                }
                Err(DeliveryError::NoAdapter(ct)) => {
                    // No adapter -> leave row alone, count as deferred.
                    report.deferred += 1;
                    warn!(channel = %ct, ?row.id, "no adapter; leaving row pending");
                }
                Err(DeliveryError::NoRoute(_)) => {
                    let in_conn = inbound_pool.connect()?;
                    delivered::insert(&in_conn, row.id, None, "failed")?;
                    self.retries.remove(&key);
                    report.failed += 1;
                    copperclaw_metrics::inc_delivery_failed(&channel_label);
                    warn!(?row.id, "no route resolvable, marking failed");
                }
                Err(err) => {
                    // Non-retryable adapter error -> mark failed immediately.
                    let in_conn = inbound_pool.connect()?;
                    delivered::insert(&in_conn, row.id, None, "failed")?;
                    self.retries.remove(&key);
                    report.failed += 1;
                    copperclaw_metrics::inc_delivery_failed(&channel_label);
                    warn!(?err, ?row.id, "non-retryable failure, marking failed");
                }
            }
        }

        Ok(report)
    }

    /// Pluck a system action handler invocation off the parsed row, if any.
    async fn process_row(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        routing: Option<&copperclaw_types::routing::SessionRouting>,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let target = Self::resolve_target(row, routing).ok_or(
            DeliveryError::NoRoute(sess.id),
        )?;

        match row.kind {
            MessageKind::System => {
                self.handle_system(sess, row, &target, inbound_pool).await?;
            }
            MessageKind::Agent => {
                // Agent-to-agent delivery: the `agent_to_agent::AgentDispatchModule`
                // owns the implementation via the action registry. Its handler
                // writes the message into the target session's inbound.db.
                //
                // If the handler returns Err, propagate as `SystemAction` so the
                // delivery loop's retry/backoff kicks in (transient SQLite
                // contention is the common cause). If the handler returns Ok,
                // we record the row delivered=ok — but ONLY THEN. Previously
                // this path always recorded delivered=ok even on swallowed
                // errors, which silently lost the parent inbound write.
                //
                // Tests that don't register a real `agent_dispatch` handler hit
                // the "no handler" branch which records delivered=ok with a
                // null platform_message_id — same as before for compatibility
                // with the existing service-level tests.
                if let Some(handler) = self.actions.get("agent_dispatch").map(|r| r.clone()) {
                    let input = DeliveryActionInput {
                        action: "agent_dispatch".into(),
                        payload: row.content.clone(),
                        target: target.clone(),
                        session_id: Some(sess.id),
                        row_id: Some(row.id),
                    };
                    handler
                        .handle(input)
                        .map_err(|err| DeliveryError::SystemAction(err.to_string()))?;
                }
                let in_conn = inbound_pool.connect()?;
                delivered::insert(&in_conn, row.id, None, "ok")?;
            }
            MessageKind::Chat | MessageKind::Task | MessageKind::Webhook => {
                self.dispatch_chat(sess.id, row, &target, inbound_pool).await?;
            }
            // Wave 2 of the cards rollout: deserialize the canonical
            // Card from `content.card` and call the adapter's
            // `deliver_card` hook so channels with native card support
            // (Telegram inline keyboards, Slack Block Kit, …) render
            // the structure. Belt-and-braces: if the adapter explicitly
            // returns `Unsupported`, fall back to a plain `deliver`
            // call with the text rendering. (The trait's default impl
            // ALREADY converts to text via `deliver`, so most adapters
            // will never surface Unsupported — this branch only fires
            // for adapters that deliberately overrode `deliver_card` to
            // opt out of cards entirely.)
            MessageKind::Card => {
                self.dispatch_card(row, &target, inbound_pool).await?;
            }
            // Breadcrumb-kind rows ride a dedicated dispatch that
            // pulls the canonical `Breadcrumb` out of `content.breadcrumb`
            // and hands it to the adapter's `deliver_breadcrumb` hook.
            // Adapters with rich native rendering (Telegram HTML
            // `<code>`, Slack Block Kit `context`, Discord embed
            // footer, Google Chat cards v2, Matrix `m.notice`)
            // render a compact chip; the trait-level default falls
            // back to a `[tool] detail` text line via `deliver`.
            MessageKind::Breadcrumb => {
                self.dispatch_breadcrumb(row, &target, inbound_pool, None).await?;
            }
            // Diff-kind rows: see `dispatch_diff` for the wire shape
            // and per-channel rendering notes.
            MessageKind::Diff => {
                self.dispatch_diff(row, &target, inbound_pool).await?;
            }
            // Placeholder branch for sibling slice-3 surfaces that
            // haven't landed their dedicated dispatcher yet. The
            // long-output-expander surface (this branch's owner) does
            // NOT add new MessageKinds, but other surfaces do. Until
            // those dispatchers exist we route the rows through
            // `dispatch_chat` so the row is still delivered (degraded
            // to text via the row's `content.text` field if present,
            // recorded as failed if not — never silently swallowed).
            // Sibling agents replace this arm with their own
            // dispatchers; nothing else here changes.
            // TodoList-kind rows ride a dedicated dispatch that pulls
            // the canonical `TodoList` out of `content.todo_list`, looks
            // up the prior list's platform message id (so adapters
            // with an edit API replace the chip in place rather than
            // spam a new message on every mutation), and threads a
            // `pin_hint` derived from whether this is the first emit
            // OR the list just transitioned to fully-completed (so
            // the adapter can unpin). Adapters with rich native
            // rendering (Telegram `editMessageText` MarkdownV2 +
            // `pinChatMessage`, Slack Block Kit + `pins.add`,
            // Discord embed `PATCH`, Google Chat Cards v2 + `patch`,
            // Matrix `m.replace` + pinned-events) draw a live
            // checklist; the trait-level default emits a text-line
            // checklist via `deliver`.
            MessageKind::TodoList => {
                self.dispatch_todo_list(row, &target, inbound_pool, sess).await?;
            }
            // Error-kind rows ride a dedicated dispatch that pulls the
            // canonical `ErrorCard` out of `content.error` and hands it
            // to the adapter's `deliver_error` hook. Adapters with rich
            // native rendering (Slack `attachments.color: "danger"`,
            // Discord embed `color = 0xE74C3C`, Matrix `<font
            // color="red">`, Telegram bold HTML, Google Chat decorated
            // icon) draw a red / emphasised affordance. The trait-level
            // default emits `[ERROR: <kind>] <title>\n<summary>` via
            // `deliver` so adapters without an override still surface
            // the failure visibly.
            MessageKind::Error => {
                self.dispatch_error(row, &target, inbound_pool).await?;
            }
            // Thinking-kind rows ride a dedicated dispatch that pulls
            // the canonical `ThinkingBlock` out of `content.thinking`
            // and hands it to the adapter's `deliver_thinking` hook.
            // Adapters with native collapsed-section primitives
            // (Telegram `<blockquote expandable>`, Slack `context`
            // block, Discord muted-grey embed, Google Chat
            // `collapsibleSection`, Matrix `<details>`) render the
            // reasoning collapsed by default; the trait-level default
            // emits a `[reasoning]`-headered quoted block via
            // `deliver` so adapters without an override still surface
            // the block visibly. Rows arrive here only when the
            // runner has confirmed the operator's per-group
            // `surface_thinking` opt-in — see
            // `copperclaw_runner::run::provider_call::pump_events`.
            MessageKind::Thinking => {
                self.dispatch_thinking(row, &target, inbound_pool).await?;
            }
        }
        Ok(())
    }

    async fn handle_system(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let Some(action) = parse_system_content(&row.content)? else {
            // Private metadata only; record as delivered and move on.
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, None, "ok")?;
            return Ok(());
        };

        // `usage_report` is an action the runner emits at the end of
        // every turn. We intercept it here rather than going through
        // the module action registry because the recorder needs the
        // CentralDb the delivery service already holds; passing it
        // through `DeliveryActionHandler` would mean extending the
        // module trait surface. Tradeoff: usage recording belongs to
        // the delivery service, not a module.
        if action.name == "usage_report" {
            record_usage_report(&self.central, row, &action.payload);
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, None, "ok")?;
            return Ok(());
        }

        // `install_packages` / `add_mcp_server` are emitted by the runner
        // when the agent calls the corresponding MCP tool. They are
        // mutations against `container_configs` — applying them here
        // (rather than via the action registry) keeps the central-DB
        // dependency contained, mirrors `usage_report`'s pattern, and
        // ensures the container_configs fingerprint diff machinery in
        // the container manager picks up the change on the next spawn.
        if action.name == "install_packages" {
            let apply = apply_install_packages(&self.central, sess.agent_group_id, &action.payload);
            self.finish_self_mod("install_packages", sess, row, inbound_pool, apply)?;
            return Ok(());
        }
        if action.name == "add_mcp_server" {
            let apply = apply_add_mcp_server(&self.central, sess.agent_group_id, &action.payload);
            self.finish_self_mod("add_mcp_server", sess, row, inbound_pool, apply)?;
            return Ok(());
        }

        // `update_breadcrumb` is the finalisation half of the runner's
        // tool-progress chip pipeline. The payload carries the new
        // (`Done` / `Failed`) `Breadcrumb` shape; we resolve the prior
        // chip's platform message id and re-render via
        // `deliver_breadcrumb(..., existing_message_id=Some(...))`
        // so adapters with an edit API replace the original chip in
        // place. Falls back to a fresh emit when the prior chip's
        // platform id isn't known (no edit-API support, or chip
        // hasn't been delivered yet).
        if action.name == "update_breadcrumb" {
            self.handle_update_breadcrumb(
                row,
                target,
                inbound_pool,
                &action.payload,
                sess,
            )
            .await?;
            return Ok(());
        }

        // "edit" / "reaction" go through the channel adapter's typed APIs.
        // Both fall through to the registered-handler path when the adapter
        // reports `Unsupported` (CLI / webhooks / etc.) OR when the original
        // row's platform message id is missing — the handler is expected to
        // return a fallback `OutboundMessage` we then dispatch normally.
        if let Some(()) = self
            .maybe_handle_edit_or_reaction(sess, row, &action, target, inbound_pool)
            .await?
        {
            return Ok(());
        }

        let handler = self.actions.get(&action.name).map(|r| r.clone());
        let Some(handler) = handler else {
            info!(name = %action.name, "no handler for system action; skipping");
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, None, "ok")?;
            return Ok(());
        };

        // `session_id` (set below) and a defaulted `agent_group_id` are
        // both threaded into `DeliveryActionInput` so the scheduling
        // handler can identify which session a `schedule` op targets.
        // Other handlers ignore the extra context.
        let mut handler_target = target.clone();
        if handler_target.agent_group_id.is_none() {
            handler_target.agent_group_id = Some(sess.agent_group_id);
        }
        let input = DeliveryActionInput {
            action: action.name,
            payload: action.payload,
            target: handler_target,
            session_id: Some(sess.id),
            row_id: Some(row.id),
        };
        let output = handler
            .handle(input)
            .map_err(|err| DeliveryError::SystemAction(err.to_string()))?;

        // If the handler asked us to deliver a message, route it through the
        // normal channel dispatch path.
        if let Some(msg) = output.message {
            let dispatch_target = output.dispatch.unwrap_or_else(|| target.clone());
            let Some(channel_type) = dispatch_target.channel_type.clone() else {
                let in_conn = inbound_pool.connect()?;
                delivered::insert(&in_conn, row.id, None, "ok")?;
                return Ok(());
            };
            let Some(platform_id) = dispatch_target.platform_id.clone() else {
                let in_conn = inbound_pool.connect()?;
                delivered::insert(&in_conn, row.id, None, "ok")?;
                return Ok(());
            };
            let adapter = self
                .adapters
                .get(&channel_type)
                .map(|r| r.clone())
                .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;
            let platform_message_id = call_adapter(
                adapter.as_ref(),
                &platform_id,
                dispatch_target.thread_id.as_deref(),
                &msg,
            )
            .await?;
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        } else {
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, None, "ok")?;
        }
        Ok(())
    }

    /// Top-level dispatch for `edit` / `reaction` system actions. Returns
    /// `Some(())` to signal "row processed, caller may return immediately",
    /// `None` to signal "this isn't an edit/reaction action; continue down
    /// the regular registered-handler path", and propagates errors as-is.
    async fn maybe_handle_edit_or_reaction(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        action: &ParsedAction,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
    ) -> Result<Option<()>, DeliveryError> {
        if action.name != "edit" && action.name != "reaction" {
            return Ok(None);
        }
        let outbound_pool = self
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)?;
        let outcome = self
            .try_action_via_adapter(
                &action.name,
                &action.payload,
                target,
                inbound_pool,
                &outbound_pool,
            )
            .await?;
        match outcome {
            ActionAdapterOutcome::Done => {
                let in_conn = inbound_pool.connect()?;
                delivered::insert(&in_conn, row.id, None, "ok")?;
                Ok(Some(()))
            }
            ActionAdapterOutcome::FallThrough => Ok(None),
        }
    }

    /// Try to dispatch an `edit` / `reaction` system action through the
    /// channel adapter's typed API. Returns:
    /// - `Done` when the adapter call succeeded (the caller marks the row as
    ///   delivered).
    /// - `FallThrough` when the adapter reported `Unsupported`, the original
    ///   row's `external_id` couldn't be located, or any precondition (target,
    ///   adapter, payload shape) is missing. The caller continues into the
    ///   registered-handler path so a fallback chat message can be emitted.
    ///
    /// `Err` is reserved for hard failures (adapter returned a non-Unsupported
    /// error, or a DB read blew up) — the caller propagates and the row goes
    /// down the retry/fail path.
    async fn try_action_via_adapter(
        &self,
        action_name: &str,
        payload: &serde_json::Value,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
        outbound_pool: &SessionPool,
    ) -> Result<ActionAdapterOutcome, DeliveryError> {
        let Some(seq) = payload.get("seq").and_then(serde_json::Value::as_i64) else {
            warn!(action = action_name, "edit/reaction payload missing seq; falling back");
            return Ok(ActionAdapterOutcome::FallThrough);
        };
        // Need a target with channel_type + platform_id; otherwise fall back.
        let (Some(channel_type), Some(platform_id)) =
            (target.channel_type.clone(), target.platform_id.clone())
        else {
            return Ok(ActionAdapterOutcome::FallThrough);
        };
        let Some(adapter) = self.adapters.get(&channel_type).map(|r| r.clone()) else {
            return Err(DeliveryError::NoAdapter(channel_type));
        };

        // Resolve the row's platform_message_id by:
        //   1. Looking up the messages_out row whose seq = `seq` (outbound DB).
        //   2. Reading delivered.platform_message_id for that id (inbound DB).
        // The row identified by `seq` MUST already be delivered for an edit /
        // reaction to make sense; if it isn't (or the platform never gave us
        // an id) we fall back to a synthetic chat message.
        let original_id = {
            let out_conn = outbound_pool.connect()?;
            message_id_for_seq(&out_conn, seq)?
        };
        let Some(original_id) = original_id else {
            warn!(
                action = action_name,
                seq, "no outbound row with this seq; falling back"
            );
            return Ok(ActionAdapterOutcome::FallThrough);
        };
        let external_id = {
            let in_conn = inbound_pool.connect()?;
            platform_message_id_for(&in_conn, original_id)?
        };
        let Some(external_id) = external_id else {
            warn!(
                action = action_name,
                seq, "no platform_message_id recorded for seq; falling back"
            );
            return Ok(ActionAdapterOutcome::FallThrough);
        };

        let call_result = match action_name {
            "edit" => {
                let Some(text) = payload.get("text").and_then(serde_json::Value::as_str) else {
                    warn!("edit payload missing text; falling back");
                    return Ok(ActionAdapterOutcome::FallThrough);
                };
                adapter
                    .edit_message(&platform_id, target.thread_id.as_deref(), &external_id, text)
                    .await
            }
            "reaction" => {
                let Some(emoji) = payload.get("emoji").and_then(serde_json::Value::as_str) else {
                    warn!("reaction payload missing emoji; falling back");
                    return Ok(ActionAdapterOutcome::FallThrough);
                };
                adapter
                    .add_reaction(
                        &platform_id,
                        target.thread_id.as_deref(),
                        &external_id,
                        emoji,
                    )
                    .await
            }
            // TODO(team-er): adding a new edit/reaction-shaped action would
            // require a third arm here.
            _ => return Ok(ActionAdapterOutcome::FallThrough),
        };

        match call_result {
            Ok(()) => Ok(ActionAdapterOutcome::Done),
            Err(AdapterError::Unsupported(reason)) => {
                info!(action = action_name, reason, "adapter unsupported; falling back");
                Ok(ActionAdapterOutcome::FallThrough)
            }
            Err(other) => Err(DeliveryError::Adapter(other)),
        }
    }

    /// Resolve an `install_packages` / `add_mcp_server` apply result
    /// into the right side effects:
    /// - on success → `delivered.status = "ok"` + success counter;
    /// - on failure with hard-fail off → `record_self_mod_failure`
    ///   writes a `failed` delivery row + a `system` inbound row + bumps
    ///   the failure counter;
    /// - on failure with hard-fail on → return `DeliveryError::SystemAction`
    ///   so the outer loop records the row in `dropped-messages` and
    ///   leaves the existing retry path in charge.
    fn finish_self_mod(
        &self,
        action: &'static str,
        sess: &Session,
        row: &MessageOutRow,
        inbound_pool: &SessionPool,
        apply: Result<(), copperclaw_db::DbError>,
    ) -> Result<(), DeliveryError> {
        match apply {
            Ok(()) => {
                copperclaw_metrics::inc_self_mod_succeeded(action);
                let in_conn = inbound_pool.connect()?;
                delivered::insert(&in_conn, row.id, None, "ok")?;
                Ok(())
            }
            Err(err) => {
                if self.selfmod_hard_fail.load(Ordering::Relaxed) {
                    copperclaw_metrics::inc_self_mod_failed(action);
                    error!(
                        session = %sess.id.as_uuid(),
                        agent_group = %sess.agent_group_id.as_uuid(),
                        action,
                        ?err,
                        "self-mod hard-fail; surfacing as DeliveryError",
                    );
                    return Err(DeliveryError::SystemAction(format!(
                        "{action}: {err}"
                    )));
                }
                record_self_mod_failure(sess, row, inbound_pool, action, &err)?;
                Ok(())
            }
        }
    }

    async fn dispatch_chat(
        &self,
        session_id: SessionId,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let channel_type = target
            .channel_type
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let platform_id = target
            .platform_id
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let adapter = self
            .adapters
            .get(&channel_type)
            .map(|r| r.clone())
            .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;

        // Typing indicator — best-effort.
        if let Err(err) = adapter
            .set_typing(&platform_id, target.thread_id.as_deref())
            .await
        {
            debug!(?err, "set_typing failed (ignored)");
        }

        // Slice 3.4 (long-output expander): when the runner has
        // attached an `expander` decorator to the row (because the
        // text exceeded the threshold), route to the adapter's
        // `deliver_collapsible` hook so it can render a native
        // disclosure widget (`<blockquote expandable>`, Slack
        // "Show full" button, Discord embed, Cards v2
        // `collapsibleSection`, Matrix `<details>`, …). The default
        // trait impl falls back to a summary-plus-preview text row.
        //
        // We deliberately branch BEFORE the splitter — `dispatch_collapsible`
        // owns its own length handling (the whole point is to NOT
        // dump the body into chat), so feeding it through
        // `split_chat_content_if_needed` would double up.
        if let Some(expander) = row.content.get("expander").cloned() {
            return self
                .dispatch_collapsible(
                    row,
                    target,
                    inbound_pool,
                    adapter.as_ref(),
                    &platform_id,
                    &expander,
                )
                .await;
        }

        // Split if the adapter advertises a per-message char cap and the
        // body would exceed it. Returns a vec of contents to send in order;
        // the first element's platform_message_id is the one we record so
        // future `edit_message` / `add_reaction` calls target the anchor.
        let parts = split_chat_content_if_needed(&row.content, adapter.max_message_chars());

        // Resume mid-split: when a previous attempt for THIS row delivered
        // the first `chunks_sent` chunks but then failed retryably (rate-
        // limit, transport blip, IO error), the retry MUST skip the
        // already-delivered chunks. Without this, the user sees duplicates
        // of every prior chunk on every retry — up to
        // `MAX_DELIVERY_ATTEMPTS` copies for chunk 0 alone.
        //
        // `chunks_sent` lives on the same `(session_id, msg_id)`-keyed
        // RetryState as `tries` / `not_before`, so the counter is
        // automatically scoped per-row. Cleared (with the rest of the
        // entry) by `process_session_once` when the row finally lands as
        // `delivered=ok` or exhausts its retry budget.
        let key = DeliveryKey::new(session_id, row.id);
        let (start_index, mut first_platform_id) = self.retries.get(&key).map_or(
            (0usize, None),
            |s| (s.chunks_sent as usize, s.first_chunk_pid.clone()),
        );

        let total_parts = parts.len();
        for (i, content) in parts.iter().enumerate().skip(start_index) {
            let outbound = OutboundMessage {
                kind: row.kind,
                content: content.clone(),
                files: vec![],
            };
            let pid = call_adapter(
                adapter.as_ref(),
                &platform_id,
                target.thread_id.as_deref(),
                &outbound,
            )
            .await?;
            if i == 0 {
                first_platform_id = pid;
            }
            // For single-chunk rows the success path runs once, returns
            // Ok, and `process_session_once` clears any retry state — no
            // chunk bookkeeping needed (and we skip the allocation).
            // For split rows we MUST record progress BEFORE the next
            // iteration so a failure on chunk i+1 leaves
            // `chunks_sent = i+1` in the retry-state map; the next retry
            // will then resume at chunk i+1, not from chunk 0.
            if total_parts > 1 {
                let mut entry = self.retries.entry(key).or_insert(RetryState {
                    tries: 0,
                    not_before: Instant::now(),
                    chunks_sent: 0,
                    first_chunk_pid: None,
                });
                // `i` is bounded by `parts.len()` which is bounded by the
                // splitter's chunk count; in practice this fits in u32
                // comfortably (a single outbound row that splits into 2^32
                // chunks is not a thing). Saturate as a belt-and-braces.
                let next_count = u32::try_from(i).unwrap_or(u32::MAX).saturating_add(1);
                entry.chunks_sent = next_count;
                if i == 0 {
                    entry.first_chunk_pid.clone_from(&first_platform_id);
                }
            }
        }

        if parts.len() > 1 {
            copperclaw_metrics::inc_delivery_chat_split(adapter.channel_type().as_str());
        }
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, first_platform_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Dispatch a Chat-kind row whose `content.expander` decorator is
    /// set (slice 3.4 long-output expander surface). Pulls the
    /// full body out of `content.text`, the summary + preview out of
    /// `content.expander`, and calls the adapter's
    /// `deliver_collapsible` hook.
    ///
    /// Belt-and-braces on `AdapterError::Unsupported`: degrade to a
    /// plain `deliver` carrying the summary + preview rendered via
    /// [`copperclaw_channels_core::render_collapsible_text_fallback`].
    /// The trait-level default impl already does this — this branch
    /// only fires for adapters that deliberately override
    /// `deliver_collapsible` to opt out.
    async fn dispatch_collapsible(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
        adapter: &dyn ChannelAdapter,
        platform_id: &str,
        expander: &serde_json::Value,
    ) -> Result<(), DeliveryError> {
        let text = row
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let summary = expander
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("(long output)")
            .to_owned();
        let preview_lines: Vec<String> = expander
            .get("preview_lines")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        let platform_message_id = match adapter
            .deliver_collapsible(
                platform_id,
                target.thread_id.as_deref(),
                &text,
                &summary,
                &preview_lines,
            )
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason,
                    "deliver_collapsible unsupported; falling back to summary text deliver"
                );
                let body = copperclaw_channels_core::render_collapsible_text_fallback(
                    &text,
                    &summary,
                    &preview_lines,
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": body }),
                    files: vec![],
                };
                call_adapter(adapter, platform_id, target.thread_id.as_deref(), &outbound).await?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Dispatch a `MessageKind::Card` row.
    ///
    /// Row content shape (written by the runner's `apply_send_card`):
    ///
    /// ```json
    /// { "card": { ...canonical Card... }, "to": { ...Recipient... } }
    /// ```
    ///
    /// `to` is optional — present only when the model passed an explicit
    /// `to:` to `send_card`. We forward it to the adapter as a routing
    /// hint so wave-2 native renderers can use it for DM-open flows.
    ///
    /// Belt-and-braces: if the adapter returns
    /// `Err(AdapterError::Unsupported(_))`, we treat that as "this
    /// adapter has explicitly opted out of cards" and fall back to a
    /// plain `deliver` call with the text rendering. (The trait's
    /// default `deliver_card` impl already routes to `deliver` with the
    /// text fallback, so most adapters never get here — this branch only
    /// fires for adapters that deliberately overrode `deliver_card` to
    /// return Unsupported.)
    async fn dispatch_card(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let channel_type = target
            .channel_type
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let platform_id = target
            .platform_id
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let adapter = self
            .adapters
            .get(&channel_type)
            .map(|r| r.clone())
            .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;

        // Deserialize the canonical Card. The runner's apply path went
        // through `Card::validate` at the MCP boundary, so a parse
        // failure here is a host bug (e.g. corrupted row) rather than
        // bad input — surface it as SystemAction so the retry loop
        // doesn't keep banging on a row that will never parse.
        let card: Card = match row.content.get("card") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                DeliveryError::SystemAction(format!(
                    "card row content.card failed to deserialise into Card: {e}"
                ))
            })?,
            None => {
                return Err(DeliveryError::SystemAction(
                    "card row missing content.card".into(),
                ));
            }
        };
        // Optional `to` hint — pulled out of the row body without
        // committing to a typed Recipient at this layer (the adapter
        // only needs a string for its DM-open / lookup flow).
        let to_hint = row
            .content
            .get("to")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                row.content
                    .get("to")
                    .and_then(|v| v.get("session_id"))
                    .and_then(|v| v.as_str())
            });

        // Typing indicator — best-effort. Same as the chat path.
        if let Err(err) = adapter
            .set_typing(&platform_id, target.thread_id.as_deref())
            .await
        {
            debug!(?err, "set_typing failed (ignored)");
        }

        let platform_message_id = match adapter
            .deliver_card(&platform_id, target.thread_id.as_deref(), &card, to_hint)
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason,
                    "deliver_card unsupported; falling back to text deliver"
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": card.to_text_fallback() }),
                    files: vec![],
                };
                call_adapter(
                    adapter.as_ref(),
                    &platform_id,
                    target.thread_id.as_deref(),
                    &outbound,
                )
                .await?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Dispatch a `MessageKind::Breadcrumb` row.
    ///
    /// Row content shape (written by `RunnerToolCtx::emit_breadcrumb`):
    ///
    /// ```json
    /// { "breadcrumb": { ...canonical Breadcrumb... } }
    /// ```
    ///
    /// `existing_message_id` is `None` for first-emit (Running) rows;
    /// the `update_breadcrumb` system-action path passes
    /// `Some(prev_platform_id)` so adapters with an in-place edit
    /// API can replace the prior chip's contents. Adapters without
    /// an edit API ignore the argument and emit a fresh chip.
    ///
    /// On `AdapterError::Unsupported` we degrade to a plain
    /// `deliver` call with the breadcrumb's `to_text_fallback` so
    /// the chip is still visible (even though it can't be a real
    /// chip on that channel). Mirrors `dispatch_card`'s belt-and-
    /// braces fallback.
    async fn dispatch_breadcrumb(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
        existing_message_id: Option<&str>,
    ) -> Result<(), DeliveryError> {
        let channel_type = target
            .channel_type
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let platform_id = target
            .platform_id
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let adapter = self
            .adapters
            .get(&channel_type)
            .map(|r| r.clone())
            .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;

        // Pull the canonical Breadcrumb out of `content.breadcrumb`.
        // A parse failure is a host bug (corrupted row) — surface as
        // SystemAction so the retry loop doesn't bang on it forever.
        let breadcrumb: Breadcrumb = match row.content.get("breadcrumb") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                DeliveryError::SystemAction(format!(
                    "breadcrumb row content.breadcrumb failed to deserialise: {e}"
                ))
            })?,
            None => {
                return Err(DeliveryError::SystemAction(
                    "breadcrumb row missing content.breadcrumb".into(),
                ));
            }
        };

        let platform_message_id = match adapter
            .deliver_breadcrumb(
                &platform_id,
                target.thread_id.as_deref(),
                &breadcrumb,
                existing_message_id,
            )
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason,
                    "deliver_breadcrumb unsupported; falling back to text deliver"
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": breadcrumb.to_text_fallback() }),
                    files: vec![],
                };
                call_adapter(
                    adapter.as_ref(),
                    &platform_id,
                    target.thread_id.as_deref(),
                    &outbound,
                )
                .await?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Dispatch a `MessageKind::TodoList` row — the slice-3.2
    /// "live, pinned checklist" surface.
    ///
    /// Row content shape (written by the MCP `todo_*` tool handlers
    /// after every mutation):
    ///
    /// ```json
    /// { "todo_list": { ...canonical TodoList... } }
    /// ```
    ///
    /// Behaviour:
    /// - Look up the most recent prior `TodoList` row in the session
    ///   via [`lookup_prior_kind_external_id`]. When found, thread
    ///   its platform message id through as `existing_message_id` so
    ///   adapters with an edit API (Telegram `editMessageText`, Slack
    ///   `chat.update`, Discord `PATCH`, Google Chat
    ///   `spaces.messages.patch`, Matrix `m.replace`) REPLACE the
    ///   prior chip rather than emit a new message on every mutation.
    /// - Derive `pin_hint`: `true` on the very first emit per session
    ///   (so the adapter pins) AND on the transition to fully-completed
    ///   (so the adapter can unpin). When neither condition holds the
    ///   adapter is told `false` and leaves the existing pin state
    ///   alone.
    /// - On `AdapterError::Unsupported`, downgrade to a plain
    ///   `deliver` call with the list's `to_text_fallback` body so
    ///   the checklist still reaches the user (just without
    ///   edit-in-place or pinning). Mirrors `dispatch_breadcrumb`'s
    ///   belt-and-braces fallback.
    ///
    /// No typing indicator — the agent isn't "typing" a todo list,
    /// the list is structured metadata. Same call-pattern as
    /// `dispatch_breadcrumb`.
    async fn dispatch_todo_list(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
        sess: &Session,
    ) -> Result<(), DeliveryError> {
        let channel_type = target
            .channel_type
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let platform_id = target
            .platform_id
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let adapter = self
            .adapters
            .get(&channel_type)
            .map(|r| r.clone())
            .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;

        // Pull the canonical TodoList out of `content.todo_list`. A
        // parse failure is a host bug (corrupted row) — surface as
        // SystemAction so the retry loop doesn't bang on it forever.
        let list: TodoList = match row.content.get("todo_list") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                DeliveryError::SystemAction(format!(
                    "todo_list row content.todo_list failed to deserialise: {e}"
                ))
            })?,
            None => {
                return Err(DeliveryError::SystemAction(
                    "todo_list row missing content.todo_list".into(),
                ));
            }
        };

        // Look up the prior list's platform message id (when one
        // exists). The predicate is "always true" because the
        // most-recent TodoList row in the session IS the prior chip
        // — there's only ever one logical list per session, so we
        // don't need a tool_name-style scope filter.
        let prior_external_id = lookup_prior_kind_external_id(
            &self
                .session_paths
                .outbound_pool(&sess.agent_group_id, &sess.id)?
                .connect()?,
            &inbound_pool.connect()?,
            MessageKind::TodoList,
            |_| true,
        )?;

        // pin_hint:
        // - First emit (no prior chip yet) → true so the adapter pins.
        // - All-completed transition → true so the adapter can unpin.
        // - Otherwise → false (leave existing pin state alone).
        let pin_hint = prior_external_id.is_none() || list.is_fully_completed();

        let platform_message_id = match adapter
            .deliver_todo_list(
                &platform_id,
                target.thread_id.as_deref(),
                &list,
                prior_external_id.as_deref(),
                pin_hint,
            )
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason,
                    "deliver_todo_list unsupported; falling back to text deliver"
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": list.to_text_fallback() }),
                    files: vec![],
                };
                call_adapter(
                    adapter.as_ref(),
                    &platform_id,
                    target.thread_id.as_deref(),
                    &outbound,
                )
                .await?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Dispatch a `MessageKind::Error` row — the slice-3.3
    /// "visually-distinct error" surface.
    ///
    /// Row content shape (written by the host emit sites in
    /// `host-delivery::service` retry-exhaustion and
    /// `copperclaw-runner::run::mod` terminal-failure-apology):
    ///
    /// ```json
    /// { "error": { ...canonical ErrorCard... } }
    /// ```
    ///
    /// Errors are immutable receipts — there is no `existing_message_id`
    /// argument and no `update_error` system action to mirror
    /// `update_breadcrumb`. Belt-and-braces fallback mirrors
    /// `dispatch_breadcrumb`: if the adapter explicitly returns
    /// `Unsupported`, downgrade to a plain `deliver` call with the
    /// `ErrorCard::to_text_fallback` body so the failure still reaches
    /// the user (just without color styling).
    ///
    /// Note we deliberately do NOT call `set_typing` here — the user
    /// is being shown an error, the visual signal is the error itself,
    /// not "agent is still working". And no `to` hint — error
    /// recipients are always the originating channel.
    async fn dispatch_error(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let channel_type = target
            .channel_type
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let platform_id = target
            .platform_id
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let adapter = self
            .adapters
            .get(&channel_type)
            .map(|r| r.clone())
            .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;

        // Deserialize the canonical ErrorCard. A parse failure here is
        // a host bug (the row was written by one of our own emit sites
        // — there's no model in the loop) so surface as SystemAction
        // and stop retrying.
        let err_card: ErrorCard = match row.content.get("error") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                DeliveryError::SystemAction(format!(
                    "error row content.error failed to deserialise into ErrorCard: {e}"
                ))
            })?,
            None => {
                return Err(DeliveryError::SystemAction(
                    "error row missing content.error".into(),
                ));
            }
        };

        let platform_message_id = match adapter
            .deliver_error(&platform_id, target.thread_id.as_deref(), &err_card)
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason,
                    "deliver_error unsupported; falling back to text deliver"
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": err_card.to_text_fallback() }),
                    files: vec![],
                };
                call_adapter(
                    adapter.as_ref(),
                    &platform_id,
                    target.thread_id.as_deref(),
                    &outbound,
                )
                .await?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Dispatch a `MessageKind::Thinking` row — the slice-3.5 opt-in
    /// surface for the model's `thinking` / `redacted_thinking` blocks.
    ///
    /// Row content shape (written by `RunnerToolCtx::emit_thinking`):
    ///
    /// ```json
    /// { "thinking": { ...canonical ThinkingBlock... } }
    /// ```
    ///
    /// Mirrors `dispatch_error` in shape: deserialise the canonical
    /// [`ThinkingBlock`] from `content.thinking`, hand it to the
    /// adapter's `deliver_thinking` hook, fall back to a plain
    /// `deliver` call with the `[reasoning]`-headered quoted text
    /// rendering on `AdapterError::Unsupported`.
    ///
    /// No typing indicator (the thinking block lands beside the
    /// reply, not between user inputs). No edit-in-place — thinking
    /// blocks are point-in-time receipts.
    ///
    /// The privacy gate lives upstream in
    /// `copperclaw_runner::run::provider_call::pump_events`; rows only
    /// reach this method when the operator has flipped the per-group
    /// `surface_thinking` flag.
    async fn dispatch_thinking(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let channel_type = target
            .channel_type
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let platform_id = target
            .platform_id
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let adapter = self
            .adapters
            .get(&channel_type)
            .map(|r| r.clone())
            .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;

        // Deserialize the canonical ThinkingBlock. The runner went
        // through the schema's caps before writing, so a parse failure
        // here is a host bug (corrupted row) — surface as SystemAction
        // so the retry loop doesn't bang on a row that will never
        // parse.
        let thinking: ThinkingBlock = match row.content.get("thinking") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                DeliveryError::SystemAction(format!(
                    "thinking row content.thinking failed to deserialise: {e}"
                ))
            })?,
            None => {
                return Err(DeliveryError::SystemAction(
                    "thinking row missing content.thinking".into(),
                ));
            }
        };

        let platform_message_id = match adapter
            .deliver_thinking(&platform_id, target.thread_id.as_deref(), &thinking)
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason,
                    "deliver_thinking unsupported; falling back to text deliver"
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": thinking.to_text_fallback() }),
                    files: vec![],
                };
                call_adapter(
                    adapter.as_ref(),
                    &platform_id,
                    target.thread_id.as_deref(),
                    &outbound,
                )
                .await?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Dispatch a `MessageKind::Diff` row.
    ///
    /// Row content shape (written by `RunnerToolCtx::emit_diff`):
    ///
    /// ```json
    /// { "diff": { ...canonical DiffCard... } }
    /// ```
    ///
    /// Mirrors `dispatch_breadcrumb` in shape: deserialise the
    /// canonical [`DiffCard`] from `content.diff`, hand it to the
    /// adapter's `deliver_diff` hook, fall back to a plain `deliver`
    /// call with the unified-diff text rendering on
    /// `AdapterError::Unsupported`.
    ///
    /// No typing indicator: diff cards always follow a tool breadcrumb
    /// which already signalled activity. No `to` hint: diffs are scoped
    /// to the originating channel by construction (file-edit tools run
    /// in the agent's container, not on behalf of a third party).
    async fn dispatch_diff(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let channel_type = target
            .channel_type
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let platform_id = target
            .platform_id
            .clone()
            .ok_or(DeliveryError::NoRoute(SessionId::nil()))?;
        let adapter = self
            .adapters
            .get(&channel_type)
            .map(|r| r.clone())
            .ok_or_else(|| DeliveryError::NoAdapter(channel_type.clone()))?;

        // Pull the canonical DiffCard out of `content.diff`. A parse
        // failure is a host bug (corrupted row) — surface as
        // SystemAction so the retry loop doesn't bang on it forever.
        let diff: DiffCard = match row.content.get("diff") {
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                DeliveryError::SystemAction(format!(
                    "diff row content.diff failed to deserialise: {e}"
                ))
            })?,
            None => {
                return Err(DeliveryError::SystemAction(
                    "diff row missing content.diff".into(),
                ));
            }
        };

        let platform_message_id = match adapter
            .deliver_diff(&platform_id, target.thread_id.as_deref(), &diff)
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason,
                    "deliver_diff unsupported; falling back to text deliver"
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": diff.to_text_fallback() }),
                    files: vec![],
                };
                call_adapter(
                    adapter.as_ref(),
                    &platform_id,
                    target.thread_id.as_deref(),
                    &outbound,
                )
                .await?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    /// Best-effort: emit a `MessageKind::Error` row addressed at the
    /// originating channel of `failed_row` so the user sees a
    /// visually-distinct receipt for "your message did not get out".
    ///
    /// Called from the retry-exhaustion arm of `process_session_once`.
    /// Routing is copied verbatim from `failed_row` (same channel /
    /// platform / thread) so the error lands where the user is
    /// already looking. If `failed_row` has no channel routing
    /// (agent-to-agent, system, …) we silently skip — there is no
    /// human on the other end to surface to.
    ///
    /// Errors propagate so the caller can log them, but the caller
    /// (the retry-exhaustion branch) MUST treat any error as
    /// non-fatal — the `delivered.status = "failed"` write is the
    /// load-bearing artefact for `cclaw dropped-messages`; this card
    /// is purely UI surface.
    fn emit_delivery_failure_error_card(
        &self,
        sess: &Session,
        failed_row: &MessageOutRow,
        reason: &str,
    ) -> Result<(), DeliveryError> {
        // Only emit if the failed row had real channel routing.
        // Agent-to-agent and system rows have no human recipient on
        // the other end — surfacing a card to "nowhere" is worse than
        // silence.
        let (Some(channel_type), Some(platform_id)) = (
            failed_row.channel_type.clone(),
            failed_row.platform_id.clone(),
        ) else {
            return Ok(());
        };
        // Build the card. `retryable = false` here — we just GAVE UP
        // retrying; telling the user we'll retry again would be a lie.
        let trimmed = reason.trim();
        let summary = if trimmed.is_empty() {
            "delivery failed after exhausting the retry budget".to_owned()
        } else {
            // Cap the reason at the summary cap so a giant adapter
            // error doesn't blow the card's validation.
            let capped: String = trimmed.chars().take(400).collect();
            format!(
                "delivery failed after exhausting the retry budget: {capped}"
            )
        };
        let card = ErrorCard::new(ErrorCardKind::Delivery, summary)
            .with_title("Could not deliver message");
        // Write to the session's outbound DB — the next delivery
        // pass picks it up and routes through `dispatch_error`.
        let outbound_pool = self
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)?;
        let out_conn = outbound_pool.connect()?;
        let body = serde_json::json!({ "error": card });
        let write = messages_out::WriteOutbound {
            id: copperclaw_types::MessageId::new(),
            in_reply_to: failed_row.in_reply_to,
            timestamp: chrono::Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind: MessageKind::Error,
            platform_id: Some(platform_id),
            channel_type: Some(channel_type),
            thread_id: failed_row.thread_id.clone(),
            content: body,
        };
        messages_out::insert(&out_conn, &write)?;
        Ok(())
    }

    /// Handle a `MessageKind::System` row whose top-level action is
    /// `update_breadcrumb`. Resolves the most recent Breadcrumb-kind
    /// row matching the same tool / channel / platform and feeds the
    /// adapter the prior chip's platform message id so the chip can
    /// be edited in place.
    ///
    /// Payload shape (from `RunnerToolCtx::emit_breadcrumb_finish`):
    ///
    /// ```json
    /// { "tool_name": "shell", "breadcrumb": { ...canonical Breadcrumb... } }
    /// ```
    ///
    /// Records the row as `delivered.status = "ok"` whether or not
    /// the prior chip was found — the update is best-effort UX, NOT
    /// load-bearing.
    async fn handle_update_breadcrumb(
        &self,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
        payload: &serde_json::Value,
        sess: &Session,
    ) -> Result<(), DeliveryError> {
        let Some(breadcrumb_value) = payload.get("breadcrumb") else {
            warn!("update_breadcrumb payload missing `breadcrumb`; dropping");
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, None, "ok")?;
            return Ok(());
        };
        let breadcrumb: Breadcrumb =
            serde_json::from_value(breadcrumb_value.clone()).map_err(|e| {
                DeliveryError::SystemAction(format!(
                    "update_breadcrumb.breadcrumb failed to deserialise: {e}"
                ))
            })?;
        let tool_name = payload
            .get("tool_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(breadcrumb.tool_name.as_str())
            .to_owned();

        // Best-effort: look up the most recent Breadcrumb-kind row in
        // this session whose `breadcrumb.tool_name` matches. The
        // platform message id (if any) gets fed back as
        // `existing_message_id` so the adapter can edit in place.
        let prior_external_id = lookup_prior_breadcrumb_external_id(
            &self
                .session_paths
                .outbound_pool(&sess.agent_group_id, &sess.id)?
                .connect()?,
            &inbound_pool.connect()?,
            &tool_name,
        )?;

        // Dispatch via the same path as a regular Breadcrumb-kind
        // row, but with the resolved existing_message_id so adapters
        // can edit in place. We synthesise a temporary row carrying
        // the canonical breadcrumb under the same `content.breadcrumb`
        // key the dispatch path expects.
        let synthetic_content = serde_json::json!({ "breadcrumb": breadcrumb });
        let synthetic_row = MessageOutRow {
            id: row.id,
            seq: row.seq,
            in_reply_to: row.in_reply_to,
            timestamp: row.timestamp,
            deliver_after: row.deliver_after,
            recurrence: row.recurrence.clone(),
            kind: MessageKind::Breadcrumb,
            platform_id: row.platform_id.clone(),
            channel_type: row.channel_type.clone(),
            thread_id: row.thread_id.clone(),
            content: synthetic_content,
        };
        self.dispatch_breadcrumb(
            &synthetic_row,
            target,
            inbound_pool,
            prior_external_id.as_deref(),
        )
        .await
    }

    fn resolve_target(
        row: &MessageOutRow,
        routing: Option<&copperclaw_types::routing::SessionRouting>,
    ) -> Option<DispatchTarget> {
        let channel_type = row
            .channel_type
            .clone()
            .or_else(|| routing.and_then(|r| r.channel_type.clone()));
        let platform_id = row
            .platform_id
            .clone()
            .or_else(|| routing.and_then(|r| r.platform_id.clone()));
        let thread_id = row
            .thread_id
            .clone()
            .or_else(|| routing.and_then(|r| r.thread_id.clone()));
        if row.kind == MessageKind::Agent {
            // Agent-to-agent target — channel/platform may be absent.
            return Some(DispatchTarget {
                channel_type,
                platform_id,
                thread_id,
                agent_group_id: None,
            });
        }
        if channel_type.is_some() && platform_id.is_some() {
            return Some(DispatchTarget {
                channel_type,
                platform_id,
                thread_id,
                agent_group_id: None,
            });
        }
        None
    }

    fn bump_retry(&self, key: &DeliveryKey, retry_after_secs: Option<u64>) -> DeferOutcome {
        let now = Instant::now();
        // `or_insert` only initialises when the entry is absent — partial-
        // split progress (`chunks_sent`, `first_chunk_pid`) recorded by
        // `dispatch_chat` BEFORE the failing chunk survives the bump, which
        // is exactly what we need so the next retry skips the already-
        // delivered chunks instead of re-sending them.
        let mut entry = self.retries.entry(*key).or_insert(RetryState {
            tries: 0,
            not_before: now,
            chunks_sent: 0,
            first_chunk_pid: None,
        });
        entry.tries += 1;
        if entry.tries >= MAX_DELIVERY_ATTEMPTS {
            return DeferOutcome::Fail;
        }
        // Honour the adapter's `retry_after` hint when present (Telegram /
        // Slack / GitHub / Linear / Webex all parse it from `Retry-After`
        // or the platform's equivalent). Cap at the absolute ceiling so a
        // pathological hint can't park a row for hours. Fall back to the
        // fixed exponential schedule when no hint is given.
        let delay = match retry_after_secs {
            Some(s) => s
                .saturating_mul(1_000)
                .min(ABSOLUTE_CEILING_MS),
            None => backoff_delay_ms(entry.tries),
        };
        entry.not_before = now + Duration::from_millis(delay);
        DeferOutcome::Defer
    }

    /// Snapshot of sessions visible to the active loop (status=Active,
    /// container=Running).
    pub fn list_running_sessions(&self) -> Result<Vec<Session>, DeliveryError> {
        Ok(copperclaw_db::tables::sessions::list_running(&self.central)?)
    }

    /// Snapshot of sessions visible to the sweep loop (status=Active).
    pub fn list_active_sessions(&self) -> Result<Vec<Session>, DeliveryError> {
        Ok(copperclaw_db::tables::sessions::list_active(&self.central)?)
    }
}

/// Split `content` into a sequence of chat-content values when the adapter
/// advertises a per-message char cap (`max`) and `content.text`'s `char`
/// count exceeds it.
///
/// - Returns `vec![content.clone()]` when no cap is configured, when the
///   text is short enough, or when the row isn't a recognisable text-shaped
///   chat row (e.g. it carries a non-string `text` field). Non-text rows
///   pass through unchanged — splitter is a chat-only concern.
/// - Cuts on paragraph (`\n\n`) first, then on sentence boundaries
///   (`. `, `! `, `? `, also CJK `。`/`！`/`？`), then on a hard char index
///   if neither produced a small-enough chunk.
/// - Preserves the rest of `content`'s shape on every chunk so adapter-
///   specific keys like `parse_mode` continue to apply per part.
pub(crate) fn split_chat_content_if_needed(
    content: &serde_json::Value,
    max: Option<usize>,
) -> Vec<serde_json::Value> {
    let Some(max) = max.filter(|m| *m > 0) else {
        return vec![content.clone()];
    };
    let Some(text) = content.get("text").and_then(|v| v.as_str()) else {
        return vec![content.clone()];
    };
    if text.chars().count() <= max {
        return vec![content.clone()];
    }
    let chunks = split_text_into_chunks(text, max);
    chunks
        .into_iter()
        .map(|chunk| {
            let mut next = content.clone();
            if let Some(obj) = next.as_object_mut() {
                obj.insert("text".to_string(), serde_json::Value::String(chunk));
            }
            next
        })
        .collect()
}

/// Greedy chunker honoring `max` chars per chunk. Preference order for
/// each cut: paragraph boundary (`\n\n`) → sentence boundary → hard cut.
/// Operates on `char` indices, never on bytes.
fn split_text_into_chunks(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let remaining = chars.len() - start;
        if remaining <= max {
            out.push(chars[start..].iter().collect());
            break;
        }
        let window_end = start + max;
        let cut = find_cut(&chars, start, window_end);
        let chunk: String = chars[start..cut].iter().collect();
        out.push(chunk.trim_end().to_string());
        // Skip whitespace at the cut so the next chunk doesn't start with a
        // leading newline or space.
        start = cut;
        while start < chars.len()
            && (chars[start] == ' ' || chars[start] == '\n' || chars[start] == '\t')
        {
            start += 1;
        }
    }
    out.retain(|s| !s.is_empty());
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Find the best cut point in `[lo, hi)` (char indices). Tries paragraph
/// (`\n\n`) → sentence-ender then space (`. `, `! `, `? `, `。`, `！`,
/// `？`) → fallback to `hi` (hard cut).
fn find_cut(chars: &[char], lo: usize, hi: usize) -> usize {
    // Look for the last `\n\n` in the window.
    let mut i = hi.saturating_sub(1);
    while i > lo + 1 {
        if chars[i - 1] == '\n' && chars[i] == '\n' {
            return i + 1;
        }
        i -= 1;
    }
    // Sentence boundary: `.`, `!`, `?` followed by space; or a CJK
    // full-stop / exclamation / question mark.
    let mut i = hi.saturating_sub(1);
    while i > lo {
        let c = chars[i];
        if c == '。' || c == '！' || c == '？' {
            return i + 1;
        }
        if i + 1 < chars.len()
            && (c == '.' || c == '!' || c == '?')
            && chars[i + 1] == ' '
        {
            return i + 1;
        }
        i -= 1;
    }
    // Last space before hi.
    let mut i = hi.saturating_sub(1);
    while i > lo {
        if chars[i] == ' ' || chars[i] == '\n' {
            return i;
        }
        i -= 1;
    }
    hi
}

/// Wrap an adapter `deliver` call so the `?` operator at the call sites can
/// surface the error as `DeliveryError::Adapter(_)`.
///
/// When the adapter rejects the message with a [`AdapterError::BadRequest`]
/// whose body matches a known formatting-error signature
/// (`is_formatting_bad_request`), we ask the adapter for a
/// plain-text fallback via [`ChannelAdapter::plain_text_fallback`] and
/// re-issue the delivery. If the fallback succeeds, the original failure
/// is swallowed and the fallback metric
/// (`copperclaw_delivery_formatting_fallback_total{channel_type}`) is
/// incremented. If the adapter has no fallback (default impl) or the
/// fallback itself fails, the ORIGINAL error is returned so the caller's
/// failure-handling stays unchanged.
async fn call_adapter(
    adapter: &dyn ChannelAdapter,
    platform_id: &str,
    thread_id: Option<&str>,
    message: &OutboundMessage,
) -> Result<Option<String>, DeliveryError> {
    match adapter.deliver(platform_id, thread_id, message).await {
        Ok(id) => Ok(id),
        Err(AdapterError::BadRequest(msg)) if is_formatting_bad_request(&msg) => {
            let original = AdapterError::BadRequest(msg);
            let Some(fallback_msg) = adapter.plain_text_fallback(message) else {
                return Err(DeliveryError::Adapter(original));
            };
            match adapter
                .deliver(platform_id, thread_id, &fallback_msg)
                .await
            {
                Ok(id) => {
                    let ct = adapter.channel_type().as_str();
                    info!(channel = ct, "delivered with reduced formatting");
                    copperclaw_metrics::inc_delivery_formatting_fallback(ct);
                    Ok(id)
                }
                Err(_) => Err(DeliveryError::Adapter(original)),
            }
        }
        Err(other) => Err(DeliveryError::Adapter(other)),
    }
}

/// Return `true` when the `BadRequest` message text matches a known
/// formatting-validation signature. The delivery loop uses this to gate
/// the plain-text retry: only formatting rejections fall back, everything
/// else (e.g. "`chat_id` required") fails fast.
///
/// Patterns covered (case-insensitive):
/// - `parse entities` — Telegram `MarkdownV2` / Markdown / HTML.
/// - `rich text` / `blocks` / `block_kit` / `block kit` — Slack.
/// - `embed` / `embeds` — Discord.
/// - `format` / `formatting` — generic fallback for adapters that surface
///   a less specific error message.
pub(crate) fn is_formatting_bad_request(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("parse entities")
        || m.contains("rich text")
        || m.contains("blocks")
        || m.contains("block_kit")
        || m.contains("block kit")
        || m.contains("embed")
        || m.contains("embeds")
        || m.contains("format")
        || m.contains("formatting")
}

/// Helper used by tests / the sweep loop to filter on container status.
pub(crate) fn is_container_running(s: &Session) -> bool {
    s.container_status == ContainerStatus::Running
}

/// Helper used by tests / the sweep loop to filter on session status.
pub(crate) fn is_session_active(s: &Session) -> bool {
    s.status == SessionStatus::Active
}

/// Look up an outbound row's `MessageId` by its monotonic `seq` value.
/// Returns `Ok(None)` when no row with that seq exists. Pulled out of
/// [`DeliveryService::try_action_via_adapter`] so the SELECT lives in one
/// place and can be unit-tested directly.
fn message_id_for_seq(
    out_conn: &Connection,
    seq: i64,
) -> Result<Option<MessageId>, DeliveryError> {
    let mut stmt = out_conn
        .prepare("SELECT id FROM messages_out WHERE seq = ?1")
        .map_err(copperclaw_db::DbError::from)?;
    let row: Option<String> = stmt
        .query_row([seq], |r| r.get::<_, String>(0))
        .optional()
        .map_err(copperclaw_db::DbError::from)?;
    let Some(id_str) = row else {
        return Ok(None);
    };
    let uuid = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        DeliveryError::SystemAction(format!("invalid outbound row uuid: {e}"))
    })?;
    Ok(Some(MessageId(uuid)))
}

/// Find the most recent Breadcrumb-kind outbound row whose
/// `content.breadcrumb.tool_name` matches `tool_name`, and resolve its
/// platform message id via the `delivered` table. Returns `Ok(None)`
/// when no prior chip exists, the prior chip wasn't delivered yet, or
/// the platform didn't expose a message id (e.g. CLI / webhook channels).
///
/// Thin wrapper around [`lookup_prior_kind_external_id`], the generic
/// helper every edit-in-place dispatch path (Breadcrumb chips,
/// `TodoList` rows, …) shares.
fn lookup_prior_breadcrumb_external_id(
    out_conn: &Connection,
    in_conn: &Connection,
    tool_name: &str,
) -> Result<Option<String>, DeliveryError> {
    lookup_prior_kind_external_id(
        out_conn,
        in_conn,
        MessageKind::Breadcrumb,
        |content| {
            content
                .get("breadcrumb")
                .and_then(|v| v.get("tool_name"))
                .and_then(serde_json::Value::as_str)
                == Some(tool_name)
        },
    )
}

/// Find the most recent outbound row of `kind` whose decoded JSON
/// `content` satisfies the `matches` predicate, and resolve its
/// platform message id via the `delivered` table. Returns `Ok(None)`
/// when no matching row exists, the row wasn't delivered yet, or the
/// platform didn't expose a message id (e.g. CLI / webhook channels).
///
/// This is the generic "edit-in-place" lookup shared by every dispatch
/// path that wants to reuse a prior chip's platform message id —
/// `dispatch_breadcrumb` (via [`lookup_prior_breadcrumb_external_id`]
/// for its tool-name filter), `dispatch_todo_list` (the most recent
/// list-kind row is always the right target — predicate returns
/// `true` unconditionally), future surfaces, … Each caller supplies
/// the predicate so it can scope the lookup to whatever shape its
/// rows carry.
///
/// Edit-in-place is a UX detail (mutations look like one live chip
/// rather than a stream of fresh messages), NOT load-bearing: any
/// failure to resolve a prior id results in a fresh emit, which is
/// always safe. The O(N) scan is bounded by `LIMIT 32`; beyond that
/// horizon we let the adapter emit a fresh chip rather than scan the
/// whole table.
fn lookup_prior_kind_external_id<F>(
    out_conn: &Connection,
    in_conn: &Connection,
    kind: MessageKind,
    matches: F,
) -> Result<Option<String>, DeliveryError>
where
    F: Fn(&serde_json::Value) -> bool,
{
    let kind_str = kind.as_str();
    let mut stmt = out_conn
        .prepare(
            "SELECT id, content FROM messages_out
             WHERE kind = ?1
             ORDER BY seq DESC
             LIMIT 32",
        )
        .map_err(copperclaw_db::DbError::from)?;
    let rows = stmt
        .query_map([kind_str], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
            ))
        })
        .map_err(copperclaw_db::DbError::from)?;
    for row in rows {
        let (id_str, content_str) = row.map_err(copperclaw_db::DbError::from)?;
        let content: serde_json::Value = match serde_json::from_str(&content_str) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !matches(&content) {
            continue;
        }
        let Ok(uuid) = uuid::Uuid::parse_str(&id_str) else {
            continue;
        };
        let message_id = MessageId(uuid);
        if let Some(external) = platform_message_id_for(in_conn, message_id)? {
            return Ok(Some(external));
        }
    }
    Ok(None)
}

/// Look up the platform-side message id recorded against an outbound row
/// in the inbound `delivered` table. Returns `Ok(None)` when the row was
/// either never delivered or the platform didn't expose an id (e.g. CLI).
fn platform_message_id_for(
    in_conn: &Connection,
    message_out_id: MessageId,
) -> Result<Option<String>, DeliveryError> {
    let mut stmt = in_conn
        .prepare(
            "SELECT platform_message_id FROM delivered
             WHERE message_out_id = ?1 AND status = 'ok'",
        )
        .map_err(copperclaw_db::DbError::from)?;
    let row: Option<Option<String>> = stmt
        .query_row(
            [message_out_id.as_uuid().to_string()],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(copperclaw_db::DbError::from)?;
    Ok(row.flatten())
}

/// Translate a `usage_report` system payload into an `agent_turns`
/// row. Best-effort: a malformed payload is logged and dropped so the
/// runner can't poison the delivery loop. The runner schema:
///
/// ```json
/// {
///   "name": "usage_report",
///   "payload": {
///     "agent_group_id": "<uuid>",
///     "session_id":     "<uuid>",
///     "seq":            <i64>,
///     "model":          "<provider model id>",
///     "provider":       "<provider name>",
///     "input_tokens":   <u32>,
///     "output_tokens":  <u32>,
///     "started_at":     "<rfc3339>",
///     "ended_at":       "<rfc3339>",
///     "status":         "ok" | "error",
///     "error":          <string?>
///   }
/// }
/// ```
fn record_usage_report(
    central: &copperclaw_db::central::CentralDb,
    _row: &MessageOutRow,
    payload: &serde_json::Value,
) {
    use copperclaw_db::tables::agent_turns;
    let pick_str = |k: &str| {
        payload
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let pick_i64 = |k: &str| payload.get(k).and_then(serde_json::Value::as_i64);
    let pick_ts = |k: &str| {
        pick_str(k)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
    };

    let Some(session_id) = pick_str("session_id") else {
        warn!("usage_report missing session_id; dropping");
        return;
    };
    let Some(agent_group_id) = pick_str("agent_group_id") else {
        warn!("usage_report missing agent_group_id; dropping");
        return;
    };
    let now = chrono::Utc::now();
    let started_at = pick_ts("started_at").unwrap_or(now);
    let ended_at = pick_ts("ended_at").unwrap_or(now);
    let model = pick_str("model").unwrap_or_else(|| "unknown".to_string());
    let provider = pick_str("provider").unwrap_or_else(|| "unknown".to_string());
    let seq = pick_i64("seq").unwrap_or(0);
    let input_tokens = pick_i64("input_tokens").unwrap_or(0);
    let output_tokens = pick_i64("output_tokens").unwrap_or(0);
    let status = pick_str("status").unwrap_or_else(|| "ok".to_string());
    let error = pick_str("error");

    let turn = agent_turns::NewAgentTurn {
        session_id,
        agent_group_id,
        seq,
        model,
        provider,
        input_tokens,
        output_tokens,
        started_at,
        ended_at,
        status,
        error,
    };
    if let Err(err) = agent_turns::insert(central, &turn) {
        warn!(?err, "agent_turns insert failed; dropping usage report");
    }
}

/// Read the `COPPERCLAW_SELFMOD_HARD_FAIL` env var once at boot. When
/// set to `1` / `true` / `yes` / `on` (case-insensitive), a failed
/// self-mod apply is surfaced as a [`DeliveryError::SystemAction`]
/// from `handle_system` rather than being recorded as a failed
/// delivery, so the existing retry path can have another go. Default
/// is off — see the per-block recovery in
/// [`DeliveryService::handle_system`].
fn selfmod_hard_fail_from_env() -> bool {
    matches!(
        std::env::var("COPPERCLAW_SELFMOD_HARD_FAIL")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// On `install_packages` / `add_mcp_server` apply failure:
/// - logs at `error!` (operator-visible);
/// - increments `copperclaw_self_mod_failed_total{action}`;
/// - marks the outbound row as `delivered.status = "failed"` with the
///   error message in the payload so it surfaces in
///   `cclaw dropped-messages outbound-list`;
/// - writes a `MessageKind::System` row to the session's `inbound.db`
///   carrying a `self_mod_error` envelope so the agent can react on
///   its next turn (without this, the runner thinks the install
///   succeeded and loops).
fn record_self_mod_failure(
    sess: &Session,
    row: &MessageOutRow,
    inbound_pool: &SessionPool,
    action: &str,
    err: &copperclaw_db::DbError,
) -> Result<(), DeliveryError> {
    let err_text = err.to_string();
    error!(
        session = %sess.id.as_uuid(),
        agent_group = %sess.agent_group_id.as_uuid(),
        action,
        error = %err_text,
        "self-mod action failed to apply"
    );
    copperclaw_metrics::inc_self_mod_failed(action);

    let in_conn = inbound_pool.connect()?;
    delivered::insert(&in_conn, row.id, Some(&err_text), "failed")?;

    // Best-effort: surface the failure to the agent. A second write
    // error here would be confusing (the delivery row is already
    // marked failed) — log and move on so we don't poison the loop.
    let inbound_row = messages_in::WriteInbound {
        id: MessageId::new(),
        kind: MessageKind::System,
        timestamp: chrono::Utc::now(),
        content: serde_json::json!({
            "kind": "system",
            "content": {
                "self_mod_error": {
                    "action": action,
                    "error": err_text,
                    "guidance": "The package install was rejected. Inspect the error and either retry with corrected names or proceed without it.",
                }
            }
        }),
        trigger: false,
        on_wake: false,
        process_after: None,
        recurrence: None,
        series_id: None,
        platform_id: None,
        channel_type: None,
        thread_id: None,
        source_session_id: None,
        reply_to: None,
        is_group: None,
    };
    if let Err(insert_err) = messages_in::insert(&in_conn, &inbound_row) {
        warn!(
            session = %sess.id.as_uuid(),
            ?insert_err,
            "self_mod_error inbound write failed; agent will not see the failure"
        );
    }
    Ok(())
}

/// Ensure a `container_configs` row exists for `agent_group_id`,
/// creating a default one if absent. Mirrors the host's MCP-handler
/// `ensure_config_row` so the apply helpers below don't trip on a
/// fresh group that hasn't been configured yet.
fn ensure_config_row(
    central: &copperclaw_db::central::CentralDb,
    agent_group_id: copperclaw_types::AgentGroupId,
) -> Result<(), copperclaw_db::DbError> {
    use copperclaw_db::tables::container_configs;
    if container_configs::get(central, agent_group_id)?.is_some() {
        return Ok(());
    }
    container_configs::upsert(
        central,
        container_configs::UpsertContainerConfig {
            agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: container_configs::SkillsSelector::All,
            mcp_servers: serde_json::json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: serde_json::json!([]),
            cli_scope: container_configs::CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: serde_json::json!({}),
            coding_enabled: false,
            surface_thinking: false,
        },
    )?;
    Ok(())
}

/// Apply an `install_packages` system payload to the group's
/// `container_configs.packages_apt` / `packages_npm`. Idempotent —
/// packages already present are left in place; only new ones are
/// appended. The next container spawn detects the fingerprint diff
/// and triggers an image rebuild.
///
/// Expected payload shape:
/// ```json
/// { "apt": ["jq", "ripgrep"], "npm": ["typescript"], "reason": "..." }
/// ```
fn apply_install_packages(
    central: &copperclaw_db::central::CentralDb,
    agent_group_id: copperclaw_types::AgentGroupId,
    payload: &serde_json::Value,
) -> Result<(), copperclaw_db::DbError> {
    use copperclaw_db::tables::container_configs;
    let str_list = |key: &str| -> Vec<String> {
        payload
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let apt_new = str_list("apt");
    let npm_new = str_list("npm");
    if apt_new.is_empty() && npm_new.is_empty() {
        return Ok(());
    }
    ensure_config_row(central, agent_group_id)?;
    for p in apt_new {
        container_configs::add_package_apt(central, agent_group_id, p)?;
    }
    for p in npm_new {
        container_configs::add_package_npm(central, agent_group_id, p)?;
    }
    Ok(())
}

/// Apply an `add_mcp_server` system payload to the group's
/// `container_configs.mcp_servers` JSON. Merges the new entry under
/// its `name`, replacing any pre-existing entry with the same name
/// so the agent can refresh a server's transport without operator
/// help.
///
/// Expected payload shape:
/// ```json
/// {
///   "name": "linear",
///   "transport": { "command": "npx", "args": ["..."], "env": {"...": "..."} },
///   "reason": "..."
/// }
/// ```
fn apply_add_mcp_server(
    central: &copperclaw_db::central::CentralDb,
    agent_group_id: copperclaw_types::AgentGroupId,
    payload: &serde_json::Value,
) -> Result<(), copperclaw_db::DbError> {
    use copperclaw_db::tables::container_configs;
    let name = match payload.get("name").and_then(serde_json::Value::as_str) {
        Some(n) if !n.trim().is_empty() => n.to_string(),
        _ => return Ok(()),
    };
    let transport = payload.get("transport").cloned().unwrap_or(serde_json::Value::Null);
    ensure_config_row(central, agent_group_id)?;
    let mut current = container_configs::get_mcp_servers(central, agent_group_id)?;
    if !current.is_object() {
        current = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(obj) = current.as_object_mut() {
        obj.insert(name, transport);
    }
    container_configs::set_mcp_servers(central, agent_group_id, current)?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_service, MockRoot};
    use chrono::Utc;
    use copperclaw_channels_core::testing::MockAdapter;
    use copperclaw_channels_core::AdapterError;
    use copperclaw_db::tables::container_configs;
    use copperclaw_db::tables::messages_out::WriteOutbound;
    use copperclaw_modules::context::MockDispatcher;
    use copperclaw_modules::{DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput};
    use copperclaw_modules::ModuleError;
    use copperclaw_types::routing::SessionRouting;
    use copperclaw_types::{MessageKind, OutboundMessage};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    fn make_row(kind: MessageKind, content: serde_json::Value) -> WriteOutbound {
        WriteOutbound {
            id: MessageId::new(),
            in_reply_to: None,
            timestamp: Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind,
            platform_id: Some("plat-1".into()),
            channel_type: Some(ChannelType::new("mock")),
            thread_id: None,
            content,
        }
    }

    fn write_row(pool: &SessionPool, row: &WriteOutbound) {
        let conn = pool.connect().unwrap();
        messages_out::insert(&conn, row).unwrap();
    }

    #[test]
    fn delivery_key_constructor() {
        let s = SessionId::new();
        let m = MessageId::new();
        let k = DeliveryKey::new(s, m);
        assert_eq!(k.session_id, s);
        assert_eq!(k.msg_id, m);
    }

    #[test]
    fn delivery_report_total_sums_buckets() {
        let r = DeliveryReport {
            delivered: 3,
            failed: 1,
            deferred: 2,
        };
        assert_eq!(r.total(), 6);
    }

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        assert_eq!(backoff_delay_ms(1), BACKOFF_BASE_MS);
        assert_eq!(backoff_delay_ms(2), BACKOFF_BASE_MS * 2);
        assert_eq!(backoff_delay_ms(3), BACKOFF_BASE_MS * 4);
        // Way above the cap.
        assert_eq!(backoff_delay_ms(100), ABSOLUTE_CEILING_MS);
    }

    #[test]
    fn backoff_floor_at_tries_zero_is_base() {
        // tries=0 isn't a realistic input but we cover it for branch safety.
        assert_eq!(backoff_delay_ms(0), BACKOFF_BASE_MS);
    }

    #[test]
    fn session_pool_kinds_open_distinct_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths =
            SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let inb = SessionPool::inbound(paths.clone());
        let outb = SessionPool::outbound(paths.clone());
        let _ic = inb.connect().unwrap();
        let _oc = outb.connect().unwrap();
        assert_eq!(inb.paths().inbound_db, paths.inbound_db);
        assert_eq!(outb.paths().outbound_db, paths.outbound_db);
    }

    #[test]
    fn fs_session_root_builds_pools() {
        let tmp = tempfile::tempdir().unwrap();
        let root = FsSessionRoot::new(tmp.path());
        let ag = AgentGroupId::new();
        let sess = SessionId::new();
        let inb = root.inbound_pool(&ag, &sess).unwrap();
        let outb = root.outbound_pool(&ag, &sess).unwrap();
        let _ic = inb.connect().unwrap();
        let _oc = outb.connect().unwrap();
    }

    #[test]
    fn split_chat_passthrough_when_no_cap_or_short_text() {
        let v = json!({"text":"hello"});
        let parts = split_chat_content_if_needed(&v, None);
        assert_eq!(parts.len(), 1);
        let parts = split_chat_content_if_needed(&v, Some(4096));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], v);
    }

    #[test]
    fn split_chat_passthrough_when_no_text_field() {
        let v = json!({"foo":"bar"});
        let parts = split_chat_content_if_needed(&v, Some(10));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], v);
    }

    #[test]
    fn split_chat_breaks_on_paragraph_when_possible() {
        let text = format!("{}\n\n{}", "a".repeat(50), "b".repeat(50));
        let v = json!({"text": text, "parse_mode": "MarkdownV2"});
        let parts = split_chat_content_if_needed(&v, Some(60));
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"].as_str().unwrap(), "a".repeat(50));
        assert_eq!(parts[1]["text"].as_str().unwrap(), "b".repeat(50));
        // Other content keys are preserved on every chunk.
        assert_eq!(parts[0]["parse_mode"].as_str(), Some("MarkdownV2"));
        assert_eq!(parts[1]["parse_mode"].as_str(), Some("MarkdownV2"));
    }

    #[test]
    fn split_chat_breaks_on_sentence_when_no_paragraph() {
        let text = format!(
            "{}. {}",
            "a".repeat(40),
            "b".repeat(40)
        );
        let v = json!({"text": text});
        let parts = split_chat_content_if_needed(&v, Some(50));
        assert_eq!(parts.len(), 2);
        assert!(parts[0]["text"].as_str().unwrap().ends_with('.'));
    }

    #[test]
    fn split_chat_hard_cuts_when_no_natural_boundary() {
        let text = "x".repeat(100);
        let v = json!({"text": text});
        let parts = split_chat_content_if_needed(&v, Some(30));
        assert!(parts.len() >= 4);
        for p in &parts {
            assert!(p["text"].as_str().unwrap().chars().count() <= 30);
        }
    }

    #[test]
    fn split_chat_counts_chars_not_bytes() {
        // CJK char is 3 bytes in UTF-8 but should count as 1.
        let text = "漢".repeat(20);
        let v = json!({"text": text});
        let parts = split_chat_content_if_needed(&v, Some(10));
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"].as_str().unwrap().chars().count(), 10);
    }

    #[test]
    fn helpers_filter_by_status() {
        let mut s = crate::test_support::make_session();
        s.container_status = ContainerStatus::Running;
        assert!(is_container_running(&s));
        s.container_status = ContainerStatus::Idle;
        assert!(!is_container_running(&s));
        s.status = SessionStatus::Active;
        assert!(is_session_active(&s));
        s.status = SessionStatus::Stopped;
        assert!(!is_session_active(&s));
    }

    #[tokio::test]
    async fn happy_path_delivers_chat_row() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_row(&out_pool, &make_row(MessageKind::Chat, json!({"text":"hi"})));
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        assert_eq!(mock.deliveries().len(), 1);
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "ok");
    }

    #[tokio::test]
    async fn reentry_guard_blocks_double_processing() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        // Manually mark in-flight to simulate a concurrent attempt.
        service
            .inflight
            .insert(DeliveryKey::new(sess.id, row.id), Instant::now());
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 0);
        assert!(mock.deliveries().is_empty());
        service.inflight.clear();
        let rpt2 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt2.delivered, 1);
    }

    #[tokio::test]
    async fn rate_retry_after_overrides_fixed_backoff() {
        // When the adapter surfaces Rate { retry_after: Some(s) }, the
        // delivery loop should park the row for ~s seconds instead of
        // using the fixed exponential schedule (which would be 5s for the
        // first attempt). 10s ≠ 5s so the override is observable.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        mock.fail_next_deliver(AdapterError::Rate {
            retry_after: Some(10),
        });
        let before = Instant::now();
        let _ = service.process_session_once(&sess).await.unwrap();
        let key = DeliveryKey::new(sess.id, row.id);
        let entry = service.retries.get(&key).expect("retry state recorded");
        let delay = entry.not_before.saturating_duration_since(before);
        // Allow a tight tolerance for the ~milliseconds spent in the loop.
        assert!(
            delay >= Duration::from_millis(9_500)
                && delay <= Duration::from_millis(10_500),
            "expected ~10s backoff, got {delay:?}",
        );
    }

    #[tokio::test]
    async fn rate_without_hint_falls_back_to_exponential() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        mock.fail_next_deliver(AdapterError::Rate { retry_after: None });
        let before = Instant::now();
        let _ = service.process_session_once(&sess).await.unwrap();
        let key = DeliveryKey::new(sess.id, row.id);
        let entry = service.retries.get(&key).expect("retry state recorded");
        let delay = entry.not_before.saturating_duration_since(before);
        // First-attempt exponential = BACKOFF_BASE_MS (5s).
        assert!(
            delay >= Duration::from_millis(4_500)
                && delay <= Duration::from_millis(5_500),
            "expected ~5s exponential backoff, got {delay:?}",
        );
    }

    #[tokio::test]
    async fn retryable_failure_defers_then_succeeds() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        // First attempt -> rate-limited.
        mock.fail_next_deliver(AdapterError::Rate { retry_after: Some(1) });
        let rpt1 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt1.deferred, 1);
        assert_eq!(rpt1.delivered, 0);
        // Force backoff window to be in the past so the retry can execute.
        if let Some(mut entry) =
            service.retries.get_mut(&DeliveryKey::new(sess.id, row.id))
        {
            entry.not_before = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
        // Second attempt -> success.
        let rpt2 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt2.delivered, 1);
        assert_eq!(mock.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn retry_exhaustion_marks_failed() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);

        let key = DeliveryKey::new(sess.id, row.id);
        for _ in 0..MAX_DELIVERY_ATTEMPTS {
            mock.fail_next_deliver(AdapterError::Transport("502".into()));
            if let Some(mut entry) = service.retries.get_mut(&key) {
                entry.not_before = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
            }
            let _ = service.process_session_once(&sess).await.unwrap();
        }
        // After enough attempts the row must be marked failed —
        // operators rely on the `failed` delivery row for the
        // `cclaw dropped-messages` list. Slice-3.3 ADDED a
        // user-visible ErrorCard emission on top; we assert
        // both invariants in `retry_exhaustion_also_emits_error_card`
        // below. This test stays narrowly focused on the load-bearing
        // `failed` row insertion.
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
    }

    #[tokio::test]
    async fn retry_exhaustion_also_emits_error_card() {
        // Slice-3.3 addition: in addition to the load-bearing `failed`
        // delivery row, the host emits a `MessageKind::Error` outbound
        // back through the originating channel so the user sees a
        // visually-distinct receipt for "your message did not get out".
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);

        let key = DeliveryKey::new(sess.id, row.id);
        for _ in 0..MAX_DELIVERY_ATTEMPTS {
            mock.fail_next_deliver(AdapterError::Transport("502".into()));
            if let Some(mut entry) = service.retries.get_mut(&key) {
                entry.not_before = Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap_or_else(Instant::now);
            }
            let _ = service.process_session_once(&sess).await.unwrap();
        }
        // Read the outbound DB and find the Error-kind row routed at
        // the same channel/platform as the failed row.
        let out_conn = out_pool.connect().unwrap();
        let rows = messages_out::list_due(&out_conn).unwrap();
        let error_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.kind == MessageKind::Error)
            .collect();
        assert_eq!(
            error_rows.len(),
            1,
            "expected exactly one ErrorCard emitted by retry exhaustion"
        );
        let err_row = error_rows[0];
        assert_eq!(err_row.platform_id.as_deref(), Some("plat-1"));
        assert_eq!(
            err_row.channel_type.as_ref().map(copperclaw_types::ChannelType::as_str),
            Some("mock")
        );
        // The row carries the canonical ErrorCard in content.error,
        // tagged with kind=Delivery and `retryable=false` (we GAVE UP
        // retrying — promising another retry would be a lie).
        let card: ErrorCard =
            serde_json::from_value(err_row.content["error"].clone()).unwrap();
        assert_eq!(card.kind, ErrorCardKind::Delivery);
        assert!(!card.retryable);
        // Summary mentions both the retry budget AND the underlying
        // adapter error so operators can grep for the failure mode.
        assert!(card.summary.contains("retry budget"));
        assert!(card.summary.contains("502"));
    }

    #[tokio::test]
    async fn dispatch_error_routes_through_deliver_error_with_full_card() {
        // An Error-kind outbound row (regardless of who wrote it)
        // must land on the adapter's `deliver_error` hook with the
        // canonical ErrorCard reconstructed from `content.error`.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let card = ErrorCard::new(ErrorCardKind::Internal, "tool exit 137")
            .with_title("Tool failed")
            .with_details("stderr: SIGKILL");
        let row = make_row(MessageKind::Error, json!({ "error": card }));
        write_row(&out_pool, &row);

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);

        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "ok");

        // MockAdapter falls back through the default `deliver_error`
        // impl (which routes to `deliver` as Chat-kind with the text
        // fallback); confirm the body is the canonical
        // `[ERROR: tool] …` text fallback so the card actually
        // reached the adapter.
        let calls = mock.deliveries();
        assert_eq!(calls.len(), 1);
        let text = calls[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(text.starts_with("[ERROR: tool]"), "got: {text}");
        assert!(text.contains("Tool failed"));
        assert!(text.contains("tool exit 137"));
        assert!(text.contains("> stderr: SIGKILL"));
    }

    #[tokio::test]
    async fn dispatch_error_with_missing_content_marks_failed() {
        // An Error-kind row whose `content.error` is missing is a host
        // bug (corrupted row, schema drift) — must be marked failed and
        // NOT retried indefinitely. Surfaced as SystemAction by
        // `dispatch_error` and the outer loop turns it into a `failed`
        // delivery row.
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Error, json!({ "not_the_error_key": {} }));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
    }

    #[tokio::test]
    async fn missing_adapter_leaves_row_undelivered() {
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        row.channel_type = Some(ChannelType::new("ghost"));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.deferred, 1);
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        assert!(delivered::list(&in_conn).unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_route_marks_row_failed() {
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        row.channel_type = None;
        row.platform_id = None;
        row.thread_id = None;
        write_row(&out_pool, &row);
        // No session_routing fallback either.
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
    }

    #[tokio::test]
    async fn session_routing_fallback_resolves_target() {
        let (service, _root, sess, mock) = make_service().await;
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        {
            let conn = in_pool.connect().unwrap();
            session_routing::write(
                &conn,
                &SessionRouting {
                    channel_type: Some(ChannelType::new("mock")),
                    platform_id: Some("fallback-plat".into()),
                    thread_id: Some("t-1".into()),
                },
            )
            .unwrap();
        }
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        row.channel_type = None;
        row.platform_id = None;
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        let delivered_calls = mock.deliveries();
        assert_eq!(delivered_calls.len(), 1);
        assert_eq!(delivered_calls[0].platform_id, "fallback-plat");
        assert_eq!(delivered_calls[0].thread_id.as_deref(), Some("t-1"));
    }

    #[tokio::test]
    async fn duplicate_delivered_row_is_skipped() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        // Pre-record the row as already delivered.
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        delivered::insert(&in_conn, row.id, Some("p-1"), "ok").unwrap();
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 0);
        assert!(mock.deliveries().is_empty());
    }

    #[tokio::test]
    async fn system_action_invokes_registered_handler() {
        struct CapturingHandler {
            inputs: Arc<StdMutex<Vec<DeliveryActionInput>>>,
        }
        impl DeliveryActionHandler for CapturingHandler {
            fn handle(
                &self,
                input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                self.inputs.lock().unwrap().push(input);
                Ok(DeliveryActionOutput::default())
            }
        }
        let (service, _root, sess, _mock) = make_service().await;
        let inputs = Arc::new(StdMutex::new(vec![]));
        service.register_action(
            "approve_sender",
            Arc::new(CapturingHandler {
                inputs: inputs.clone(),
            }),
        );

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({ "approve_sender": { "user": "u-1" } }),
        );
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(inputs.lock().unwrap().len(), 1);
        assert_eq!(inputs.lock().unwrap()[0].action, "approve_sender");
    }

    #[tokio::test]
    async fn system_action_with_no_handler_logs_and_completes() {
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({ "unregistered_action": {} }),
        );
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        // No handler means we mark the row as delivered and move on rather
        // than retrying forever.
        assert_eq!(rpt.delivered, 1);
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "ok");
    }

    #[tokio::test]
    async fn system_action_with_malformed_content_marks_failed() {
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // System content that isn't an object -> SystemAction error -> failed.
        let row = make_row(MessageKind::System, json!([1, 2, 3]));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
    }

    #[tokio::test]
    async fn system_action_with_only_underscore_keys_records_ok() {
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::System, json!({ "_meta": true }));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
    }

    #[tokio::test]
    async fn system_action_dispatch_message_via_adapter() {
        struct Producer;
        impl DeliveryActionHandler for Producer {
            fn handle(
                &self,
                _input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                Ok(DeliveryActionOutput {
                    dispatch: None,
                    message: Some(OutboundMessage {
                        kind: MessageKind::Chat,
                        content: json!({"text": "from action"}),
                        files: vec![],
                    }),
                })
            }
        }
        let (service, _root, sess, mock) = make_service().await;
        service.register_action("post", Arc::new(Producer));

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::System, json!({ "post": {} }));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(mock.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn system_action_handler_error_marks_failed() {
        struct Failer;
        impl DeliveryActionHandler for Failer {
            fn handle(
                &self,
                _input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                Err(ModuleError::other("failer", "boom"))
            }
        }
        let (service, _root, sess, _mock) = make_service().await;
        service.register_action("boom_action", Arc::new(Failer));

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::System, json!({ "boom_action": {} }));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
    }

    #[tokio::test]
    async fn agent_kind_marks_delivered_without_adapter() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Agent, json!({ "to": "agent:peer" }));
        row.channel_type = Some(ChannelType::new("agent"));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        // No external channel call.
        assert!(mock.deliveries().is_empty());
    }

    #[tokio::test]
    async fn agent_kind_dispatches_registered_handler() {
        struct Capture {
            saw: Arc<StdMutex<u32>>,
        }
        impl DeliveryActionHandler for Capture {
            fn handle(
                &self,
                _input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                *self.saw.lock().unwrap() += 1;
                Ok(DeliveryActionOutput::default())
            }
        }
        let saw = Arc::new(StdMutex::new(0u32));
        let (service, _root, sess, _mock) = make_service().await;
        service.register_action(
            "agent_dispatch",
            Arc::new(Capture { saw: saw.clone() }),
        );

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Agent, json!({ "to": "peer" }));
        row.channel_type = Some(ChannelType::new("agent"));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(*saw.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn agent_kind_handler_error_marks_failed() {
        struct Failer;
        impl DeliveryActionHandler for Failer {
            fn handle(
                &self,
                _input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                Err(ModuleError::other("a2a", "no peer"))
            }
        }
        let (service, _root, sess, _mock) = make_service().await;
        service.register_action("agent_dispatch", Arc::new(Failer));

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Agent, json!({ "to": "peer" }));
        row.channel_type = Some(ChannelType::new("agent"));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
    }

    /// Drive a full `process_session_once` pass under a local Prometheus
    /// recorder so the test can assert against the fallback metric. Plain
    /// `#[test]` (not `#[tokio::test]`) because `with_local_recorder`
    /// installs the recorder via a thread-local that must stay alive for
    /// the duration of the inner `block_on` — a `#[tokio::test]` would
    /// already own the runtime on this thread and the inner `block_on`
    /// would panic.
    #[test]
    fn delivery_retries_with_plain_text_on_parse_entities_error() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (service, _root, sess, mock) = make_service().await;
                mock.enable_plain_text_fallback(true);
                let out_pool = service
                    .session_paths
                    .outbound_pool(&sess.agent_group_id, &sess.id)
                    .unwrap();
                let row = make_row(
                    MessageKind::Chat,
                    json!({"text": "Hey!", "parse_mode": "MarkdownV2"}),
                );
                write_row(&out_pool, &row);

                // First call fails with a Telegram-style parse-entities
                // error; the queued-failure list is FIFO, so the *second*
                // deliver (the fallback retry) is not preloaded with a
                // failure and therefore succeeds.
                mock.fail_next_deliver(AdapterError::BadRequest(
                    "Bad Request: can't parse entities: Character '!' is reserved".into(),
                ));

                let rpt = service.process_session_once(&sess).await.unwrap();
                assert_eq!(rpt.delivered, 1);
                assert_eq!(rpt.failed, 0);

                // The mock records the fallback delivery — the original
                // failing call never entered the deliveries log.
                let deliveries = mock.deliveries();
                assert_eq!(deliveries.len(), 1);
                let content = &deliveries[0].message.content;
                assert!(content.get("parse_mode").is_none());
                assert!(
                    content["text"]
                        .as_str()
                        .unwrap()
                        .starts_with("[reduced formatting]"),
                    "expected downgraded text, got {content:?}",
                );

                // Row marked delivered with status "ok".
                let in_pool = service
                    .session_paths
                    .inbound_pool(&sess.agent_group_id, &sess.id)
                    .unwrap();
                let in_conn = in_pool.connect().unwrap();
                let listed = delivered::list(&in_conn).unwrap();
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0].status, "ok");
            });
            handle.render()
        });

        assert!(
            body.contains(copperclaw_metrics::DELIVERY_FORMATTING_FALLBACK_TOTAL),
            "expected fallback metric in scrape body:\n{body}",
        );
        assert!(
            body.contains("channel_type=\"mock\""),
            "expected channel_type label in body:\n{body}",
        );
    }

    #[tokio::test]
    async fn delivery_marks_failed_when_plain_text_fallback_also_rejected() {
        let (service, _root, sess, mock) = make_service().await;
        mock.enable_plain_text_fallback(true);
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::Chat,
            json!({"text": "Hey!", "parse_mode": "MarkdownV2"}),
        );
        write_row(&out_pool, &row);

        // BOTH the original and the fallback retry fail with a
        // formatting-style BadRequest. The original error is non-retryable,
        // so the row should be marked failed immediately on the same pass.
        mock.fail_next_deliver(AdapterError::BadRequest(
            "Bad Request: can't parse entities: Character '!' is reserved".into(),
        ));
        mock.fail_next_deliver(AdapterError::BadRequest(
            "Bad Request: can't parse entities: still bad".into(),
        ));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        assert_eq!(rpt.delivered, 0);
        // No successful delivery was ever recorded — both attempts errored.
        assert!(mock.deliveries().is_empty());

        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
    }

    #[tokio::test]
    async fn delivery_does_not_retry_on_other_bad_request() {
        let (service, _root, sess, mock) = make_service().await;
        // Enable fallback so we'd notice an erroneous retry — if the
        // delivery loop wrongly retried, it would land a fallback delivery.
        mock.enable_plain_text_fallback(true);
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text": "hi"}));
        write_row(&out_pool, &row);

        // A non-formatting BadRequest must fail fast — no fallback retry.
        mock.fail_next_deliver(AdapterError::BadRequest("chat_id required".into()));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        assert_eq!(rpt.delivered, 0);
        // No successful delivery was attempted (the only call errored, no
        // retry was issued).
        assert!(mock.deliveries().is_empty());

        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
    }

    #[tokio::test]
    async fn deliver_after_filter_skips_future_rows() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Chat, json!({"text":"future"}));
        row.deliver_after = Some(Utc::now() + chrono::Duration::seconds(60));
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.total(), 0);
        assert!(mock.deliveries().is_empty());
    }

    #[tokio::test]
    async fn register_action_replaces_handler() {
        struct A;
        impl DeliveryActionHandler for A {
            fn handle(
                &self,
                _input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                Ok(DeliveryActionOutput::default())
            }
        }
        let (service, _root, _sess, _mock) = make_service().await;
        service.register_action("x", Arc::new(A));
        assert!(service.action("x").is_some());
        service.register_action("x", Arc::new(A));
        assert!(service.action("x").is_some());
    }

    #[tokio::test]
    async fn register_adapter_overrides_existing() {
        let (service, _root, _sess, _mock) = make_service().await;
        let ct = ChannelType::new("override");
        let mock2: Arc<dyn ChannelAdapter> = Arc::new(MockAdapter::new("override"));
        service.register_adapter(ct.clone(), mock2);
        assert!(service.adapter(&ct).is_some());
    }

    #[tokio::test]
    async fn dispatcher_handle_is_exposed() {
        let (service, _root, _sess, _mock) = make_service().await;
        let _: Arc<dyn DeliveryDispatcher> = service.dispatcher();
    }

    #[tokio::test]
    async fn inflight_len_reflects_active_attempts() {
        let (service, _root, _sess, _mock) = make_service().await;
        assert_eq!(service.inflight_len(), 0);
        service
            .inflight
            .insert(DeliveryKey::new(SessionId::new(), MessageId::new()), Instant::now());
        assert_eq!(service.inflight_len(), 1);
    }

    #[tokio::test]
    async fn central_handle_is_readable() {
        let (service, _root, _sess, _mock) = make_service().await;
        let _ = service.central().conn().unwrap();
    }

    #[tokio::test]
    async fn list_running_returns_only_running_sessions() {
        use copperclaw_db::tables::sessions as s;
        let (service, _root, sess, _mock) = make_service().await;
        // make_service creates an active+stopped session; mark it running.
        s::mark_container_running(service.central(), sess.id).unwrap();
        let running = service.list_running_sessions().unwrap();
        assert_eq!(running.len(), 1);
    }

    #[tokio::test]
    async fn list_active_returns_active_sessions() {
        let (service, _root, _sess, _mock) = make_service().await;
        let active = service.list_active_sessions().unwrap();
        assert_eq!(active.len(), 1);
    }

    #[tokio::test]
    async fn dispatcher_via_mock_records_calls() {
        let dispatcher: Arc<MockDispatcher> = Arc::new(MockDispatcher::default());
        let _arc: Arc<dyn DeliveryDispatcher> = dispatcher.clone();
        let tmp = tempfile::tempdir().unwrap();
        let central = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot> = Arc::new(MockRoot::new(tmp.path().to_path_buf()));
        let adapters: DashMap<ChannelType, Arc<dyn ChannelAdapter>> = DashMap::new();
        let service =
            DeliveryService::new(central, root, adapters, dispatcher.clone());
        let _ = service.dispatcher();
        // Smoke test ensures the dispatcher Arc round-trips cleanly.
    }

    #[tokio::test]
    async fn with_default_dispatcher_constructor_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let central = CentralDb::open_in_memory().unwrap();
        let root: Arc<dyn SessionRoot> = Arc::new(MockRoot::new(tmp.path().to_path_buf()));
        let mock: Arc<dyn ChannelAdapter> = Arc::new(MockAdapter::new("mock"));
        let _ = DeliveryService::with_default_dispatcher(
            central,
            root,
            vec![(ChannelType::new("mock"), mock)],
        );
    }

    // ── install_packages / add_mcp_server apply ───────────────────────────

    fn central_with_ag() -> (copperclaw_db::central::CentralDb, AgentGroupId) {
        use copperclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
        let db = copperclaw_db::central::CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        (db, ag.id)
    }

    #[test]
    fn apply_install_packages_appends_apt_and_npm() {
        let (db, ag) = central_with_ag();
        let payload = json!({"apt": ["jq", "ripgrep"], "npm": ["typescript"], "reason": "x"});
        apply_install_packages(&db, ag, &payload).unwrap();
        let cfg = copperclaw_db::tables::container_configs::get(&db, ag)
            .unwrap()
            .unwrap();
        assert!(cfg.packages_apt.contains(&"jq".to_string()));
        assert!(cfg.packages_apt.contains(&"ripgrep".to_string()));
        assert!(cfg.packages_npm.contains(&"typescript".to_string()));
    }

    #[test]
    fn apply_install_packages_is_idempotent() {
        let (db, ag) = central_with_ag();
        let payload = json!({"apt": ["jq"]});
        apply_install_packages(&db, ag, &payload).unwrap();
        apply_install_packages(&db, ag, &payload).unwrap();
        let cfg = copperclaw_db::tables::container_configs::get(&db, ag)
            .unwrap()
            .unwrap();
        let count = cfg.packages_apt.iter().filter(|p| *p == "jq").count();
        assert_eq!(count, 1, "duplicate writes must not double-add");
    }

    #[test]
    fn apply_install_packages_ignores_blank_and_non_string_entries() {
        let (db, ag) = central_with_ag();
        let payload = json!({"apt": ["", "  ", 42, "jq"]});
        apply_install_packages(&db, ag, &payload).unwrap();
        let cfg = copperclaw_db::tables::container_configs::get(&db, ag)
            .unwrap()
            .unwrap();
        assert_eq!(cfg.packages_apt, vec!["jq".to_string()]);
    }

    #[test]
    fn apply_install_packages_empty_payload_is_noop() {
        let (db, ag) = central_with_ag();
        apply_install_packages(&db, ag, &json!({})).unwrap();
        // Row may or may not exist; either way, no apt/npm contributions.
        let cfg = copperclaw_db::tables::container_configs::get(&db, ag).unwrap();
        if let Some(c) = cfg {
            assert!(c.packages_apt.is_empty());
            assert!(c.packages_npm.is_empty());
        }
    }

    #[test]
    fn apply_add_mcp_server_inserts_named_entry() {
        let (db, ag) = central_with_ag();
        // Seed an empty config row (required for set_mcp_servers to work).
        container_configs::upsert(
            &db,
            container_configs::UpsertContainerConfig {
                agent_group_id: ag,
                provider: None,
                model: None,
                effort: None,
                image_tag: None,
                assistant_name: None,
                max_messages_per_prompt: None,
                skills: container_configs::SkillsSelector::All,
                mcp_servers: json!({}),
                packages_apt: vec![],
                packages_npm: vec![],
                additional_mounts: json!([]),
                cli_scope: container_configs::CliScope::Group,
                config_fingerprint: None,
                egress_allow: vec![],
                resource_limits: json!({}),
                coding_enabled: false,
                surface_thinking: false,
            },
        )
        .unwrap();
        let payload = json!({
            "name": "linear",
            "transport": { "command": "npx", "args": ["-y", "@linear/mcp"] },
            "reason": "ticket lookups",
        });
        apply_add_mcp_server(&db, ag, &payload).unwrap();
        let servers = container_configs::get_mcp_servers(&db, ag).unwrap();
        assert_eq!(servers["linear"]["command"], "npx");
    }

    #[test]
    fn apply_add_mcp_server_replaces_existing_name() {
        let (db, ag) = central_with_ag();
        container_configs::upsert(
            &db,
            container_configs::UpsertContainerConfig {
                agent_group_id: ag,
                provider: None,
                model: None,
                effort: None,
                image_tag: None,
                assistant_name: None,
                max_messages_per_prompt: None,
                skills: container_configs::SkillsSelector::All,
                mcp_servers: json!({"linear": {"command": "old"}}),
                packages_apt: vec![],
                packages_npm: vec![],
                additional_mounts: json!([]),
                cli_scope: container_configs::CliScope::Group,
                config_fingerprint: None,
                egress_allow: vec![],
                resource_limits: json!({}),
                coding_enabled: false,
                surface_thinking: false,
            },
        )
        .unwrap();
        let payload = json!({
            "name": "linear",
            "transport": { "command": "new" },
        });
        apply_add_mcp_server(&db, ag, &payload).unwrap();
        let servers = container_configs::get_mcp_servers(&db, ag).unwrap();
        assert_eq!(servers["linear"]["command"], "new");
    }

    /// `edit` system action with a recorded `platform_message_id` invokes
    /// `ChannelAdapter::edit_message` and marks the row delivered. Verifies
    /// the seq → message id → external_id resolution chain end to end.
    #[tokio::test]
    async fn edit_system_action_routes_through_adapter() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // First, write a chat row and pre-record it as delivered with an
        // external platform id ("p-7"). The runner then emits an "edit"
        // system row referencing the same `seq`.
        let chat = make_row(MessageKind::Chat, json!({"text": "hello"}));
        write_row(&out_pool, &chat);
        let chat_seq = {
            let conn = out_pool.connect().unwrap();
            messages_out::get(&conn, chat.id).unwrap().seq
        };
        {
            let in_conn = in_pool.connect().unwrap();
            delivered::insert(&in_conn, chat.id, Some("p-7"), "ok").unwrap();
        }

        let edit = make_row(
            MessageKind::System,
            json!({"edit": {"seq": chat_seq, "text": "edited body"}}),
        );
        write_row(&out_pool, &edit);
        let rpt = service.process_session_once(&sess).await.unwrap();
        // Only the edit row is processed this pass (chat was already delivered).
        assert_eq!(rpt.delivered, 1);
        let edits = mock.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].platform_id, "plat-1");
        assert_eq!(edits[0].external_id, "p-7");
        assert_eq!(edits[0].new_text, "edited body");
        // No new chat delivery — the fallback was not invoked.
        assert!(mock.deliveries().is_empty());
    }

    /// `reaction` system action routes through `ChannelAdapter::add_reaction`.
    #[tokio::test]
    async fn reaction_system_action_routes_through_adapter() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let chat = make_row(MessageKind::Chat, json!({"text": "hi"}));
        write_row(&out_pool, &chat);
        let chat_seq = {
            let conn = out_pool.connect().unwrap();
            messages_out::get(&conn, chat.id).unwrap().seq
        };
        {
            let in_conn = in_pool.connect().unwrap();
            delivered::insert(&in_conn, chat.id, Some("p-9"), "ok").unwrap();
        }
        let react = make_row(
            MessageKind::System,
            json!({"reaction": {"seq": chat_seq, "emoji": "thumbsup"}}),
        );
        write_row(&out_pool, &react);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        let reactions = mock.reactions();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].external_id, "p-9");
        assert_eq!(reactions[0].emoji, "thumbsup");
    }

    /// When the adapter returns `Unsupported`, the service invokes the
    /// registered handler whose fallback is a synthetic chat message. Tests
    /// the registered-handler hand-off described in `try_action_via_adapter`.
    #[tokio::test]
    async fn unsupported_fallback_sends_new_message() {
        struct Fallback;
        impl DeliveryActionHandler for Fallback {
            fn handle(
                &self,
                input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                let text = input
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(DeliveryActionOutput {
                    dispatch: Some(input.target.clone()),
                    message: Some(OutboundMessage {
                        kind: MessageKind::Chat,
                        content: json!({ "text": format!("(edit) {text}") }),
                        files: vec![],
                    }),
                })
            }
        }
        let (service, _root, sess, mock) = make_service().await;
        service.register_action("edit", Arc::new(Fallback));
        // Tell the adapter to refuse edits — drives the fallback path.
        mock.set_edit_unsupported(true);

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let chat = make_row(MessageKind::Chat, json!({"text": "hello"}));
        write_row(&out_pool, &chat);
        let chat_seq = {
            let conn = out_pool.connect().unwrap();
            messages_out::get(&conn, chat.id).unwrap().seq
        };
        {
            let in_conn = in_pool.connect().unwrap();
            delivered::insert(&in_conn, chat.id, Some("p-1"), "ok").unwrap();
        }
        let edit = make_row(
            MessageKind::System,
            json!({"edit": {"seq": chat_seq, "text": "fallback text"}}),
        );
        write_row(&out_pool, &edit);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        // No edit_message call landed (it returned Unsupported); a new chat
        // delivery was emitted with the "(edit) ..." marker.
        assert!(mock.edits().is_empty());
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(
            deliveries[0].message.content["text"].as_str().unwrap(),
            "(edit) fallback text"
        );
    }

    /// When the row referenced by `seq` was never delivered (no
    /// `platform_message_id` recorded), the service falls back to invoking
    /// the registered handler.
    #[tokio::test]
    async fn edit_handler_falls_back_when_external_id_missing() {
        struct Fallback;
        impl DeliveryActionHandler for Fallback {
            fn handle(
                &self,
                input: DeliveryActionInput,
            ) -> Result<DeliveryActionOutput, ModuleError> {
                let text = input
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(DeliveryActionOutput {
                    dispatch: Some(input.target.clone()),
                    message: Some(OutboundMessage {
                        kind: MessageKind::Chat,
                        content: json!({ "text": format!("(edit) {text}") }),
                        files: vec![],
                    }),
                })
            }
        }
        let (service, _root, sess, mock) = make_service().await;
        service.register_action("edit", Arc::new(Fallback));

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // Emit the edit BEFORE any chat row exists — seq won't match.
        let edit = make_row(
            MessageKind::System,
            json!({"edit": {"seq": 999, "text": "no anchor"}}),
        );
        write_row(&out_pool, &edit);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        // Adapter edit never invoked; fallback dispatched a new chat row.
        assert!(mock.edits().is_empty());
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(
            deliveries[0].message.content["text"].as_str().unwrap(),
            "(edit) no anchor"
        );
    }

    // ── self-mod (install_packages / add_mcp_server) error surfacing ──────

    /// Build a `Session` whose `agent_group_id` is NOT registered in
    /// `service.central()` so that `apply_install_packages` /
    /// `apply_add_mcp_server` fail with an FK-constraint error from
    /// the `container_configs` upsert (FKs are enabled on the central
    /// DB; see `central.rs`). Returns the cooked session.
    fn ghost_session(template: &Session) -> Session {
        Session {
            id: template.id,
            agent_group_id: AgentGroupId::new(),
            messaging_group_id: template.messaging_group_id,
            thread_id: template.thread_id.clone(),
            agent_provider: template.agent_provider.clone(),
            status: template.status,
            container_status: template.container_status,
            last_active: template.last_active,
            created_at: template.created_at,
            source_session_id: template.source_session_id,
        }
    }

    /// Inbound system rows live in `messages_in` — there's no top-level
    /// helper for "give me all the system rows", so query directly.
    fn list_inbound_system_rows(pool: &SessionPool) -> Vec<serde_json::Value> {
        let conn = pool.connect().unwrap();
        let mut stmt = conn
            .prepare("SELECT content FROM messages_in WHERE kind = 'system'")
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let s: String = r.get(0)?;
                Ok(serde_json::from_str::<serde_json::Value>(&s).unwrap())
            })
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    #[tokio::test]
    async fn install_packages_failure_writes_self_mod_error_to_inbound() {
        let (service, _root, sess, _mock) = make_service().await;
        let ghost = ghost_session(&sess);
        let out_pool = service
            .session_paths
            .outbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({ "install_packages": {"apt": ["jq"]} }),
        );
        write_row(&out_pool, &row);

        let _ = service.process_session_once(&ghost).await.unwrap();

        let in_pool = service
            .session_paths
            .inbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let sys_rows = list_inbound_system_rows(&in_pool);
        assert_eq!(sys_rows.len(), 1, "expected one system row, got {sys_rows:?}");
        let envelope = &sys_rows[0];
        let err_obj = &envelope["content"]["self_mod_error"];
        assert_eq!(err_obj["action"], "install_packages");
        assert!(
            err_obj["error"].as_str().is_some(),
            "expected error string: {envelope}",
        );
        assert!(
            err_obj["guidance"].as_str().is_some(),
            "expected guidance string: {envelope}",
        );
    }

    #[tokio::test]
    async fn install_packages_failure_marks_row_failed() {
        let (service, _root, sess, _mock) = make_service().await;
        let ghost = ghost_session(&sess);
        let out_pool = service
            .session_paths
            .outbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({ "install_packages": {"apt": ["jq"]} }),
        );
        write_row(&out_pool, &row);

        let _ = service.process_session_once(&ghost).await.unwrap();

        let in_pool = service
            .session_paths
            .inbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
    }

    #[test]
    fn install_packages_failure_increments_metric() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (service, _root, sess, _mock) = make_service().await;
                let ghost = ghost_session(&sess);
                let out_pool = service
                    .session_paths
                    .outbound_pool(&ghost.agent_group_id, &ghost.id)
                    .unwrap();
                let row = make_row(
                    MessageKind::System,
                    json!({ "install_packages": {"apt": ["jq"]} }),
                );
                write_row(&out_pool, &row);
                let _ = service.process_session_once(&ghost).await.unwrap();
            });
            handle.render()
        });

        assert!(
            body.contains(copperclaw_metrics::SELF_MOD_FAILED_TOTAL),
            "expected self-mod failure metric in scrape body:\n{body}",
        );
        assert!(
            body.contains("action=\"install_packages\""),
            "expected install_packages action label in body:\n{body}",
        );
    }

    #[test]
    fn message_id_for_seq_round_trips() {
        // Pure helper-level coverage so we don't have to spin up the full
        // service to verify the SQL.
        let tmp = tempfile::tempdir().unwrap();
        let paths = copperclaw_db::session::SessionPaths::new(
            tmp.path(),
            AgentGroupId::new(),
            SessionId::new(),
        );
        let conn = copperclaw_db::session::open_outbound(&paths).unwrap();
        let msg = copperclaw_db::tables::messages_out::WriteOutbound {
            id: MessageId::new(),
            in_reply_to: None,
            timestamp: Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind: MessageKind::Chat,
            platform_id: Some("plat".into()),
            channel_type: Some(ChannelType::new("mock")),
            thread_id: None,
            content: json!({"text": "hi"}),
        };
        let seq = messages_out::insert(&conn, &msg).unwrap();
        let got = message_id_for_seq(&conn, seq).unwrap();
        assert_eq!(got, Some(msg.id));
        let missing = message_id_for_seq(&conn, seq + 100).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn platform_message_id_for_returns_none_when_status_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = copperclaw_db::session::SessionPaths::new(
            tmp.path(),
            AgentGroupId::new(),
            SessionId::new(),
        );
        let conn = copperclaw_db::session::open_inbound(&paths).unwrap();
        let id = MessageId::new();
        delivered::insert(&conn, id, Some("p-x"), "failed").unwrap();
        // Failed deliveries don't expose an external_id to subsequent
        // edits/reactions — the row is logically absent.
        assert!(platform_message_id_for(&conn, id).unwrap().is_none());
    }

    #[test]
    fn install_packages_success_increments_succeeded_metric() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let body = metrics::with_local_recorder(&recorder, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (service, _root, sess, _mock) = make_service().await;
                let out_pool = service
                    .session_paths
                    .outbound_pool(&sess.agent_group_id, &sess.id)
                    .unwrap();
                let row = make_row(
                    MessageKind::System,
                    json!({ "install_packages": {"apt": ["ripgrep"]} }),
                );
                write_row(&out_pool, &row);
                let rpt = service.process_session_once(&sess).await.unwrap();
                assert_eq!(rpt.delivered, 1);
            });
            handle.render()
        });

        assert!(
            body.contains(copperclaw_metrics::SELF_MOD_SUCCEEDED_TOTAL),
            "expected self-mod success metric in scrape body:\n{body}",
        );
        assert!(
            body.contains("action=\"install_packages\""),
            "expected install_packages action label in body:\n{body}",
        );
    }

    #[tokio::test]
    async fn add_mcp_server_failure_writes_self_mod_error_to_inbound() {
        let (service, _root, sess, _mock) = make_service().await;
        let ghost = ghost_session(&sess);
        let out_pool = service
            .session_paths
            .outbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({ "add_mcp_server": {"name": "linear", "transport": {"command": "npx"}} }),
        );
        write_row(&out_pool, &row);

        let _ = service.process_session_once(&ghost).await.unwrap();

        let in_pool = service
            .session_paths
            .inbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let sys_rows = list_inbound_system_rows(&in_pool);
        assert_eq!(sys_rows.len(), 1);
        let envelope = &sys_rows[0];
        assert_eq!(
            envelope["content"]["self_mod_error"]["action"],
            "add_mcp_server"
        );
    }

    #[tokio::test]
    async fn add_mcp_server_failure_marks_row_failed() {
        let (service, _root, sess, _mock) = make_service().await;
        let ghost = ghost_session(&sess);
        let out_pool = service
            .session_paths
            .outbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({ "add_mcp_server": {"name": "linear", "transport": {"command": "npx"}} }),
        );
        write_row(&out_pool, &row);

        let _ = service.process_session_once(&ghost).await.unwrap();

        let in_pool = service
            .session_paths
            .inbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
    }

    /// `COPPERCLAW_SELFMOD_HARD_FAIL=1` flips the failure into a returned
    /// `DeliveryError`, so the row is recorded as failed via the
    /// outer loop's `SystemAction` arm (rather than being recorded
    /// inline by `record_self_mod_failure`). The env var is read at
    /// boot and stored on the service; tests flip it via
    /// `set_selfmod_hard_fail` to avoid the Rust 2024 unsafe
    /// requirement on `std::env::set_var`.
    // TODO(team-ip): if operators want to flip this without restart,
    // wire `SIGHUP` to re-read `COPPERCLAW_SELFMOD_HARD_FAIL`.
    #[tokio::test]
    async fn selfmod_hard_fail_env_propagates_error() {
        let (service, _root, sess, _mock) = make_service().await;
        assert!(
            !service.selfmod_hard_fail(),
            "hard-fail must default to off",
        );
        service.set_selfmod_hard_fail(true);
        let ghost = ghost_session(&sess);
        let out_pool = service
            .session_paths
            .outbound_pool(&ghost.agent_group_id, &ghost.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({ "install_packages": {"apt": ["jq"]} }),
        );
        write_row(&out_pool, &row);

        let rpt = service.process_session_once(&ghost).await.unwrap();
        // The hard-fail path returns a `SystemAction` error from
        // `handle_system`. `process_row` propagates it; the outer
        // loop classifies it as a non-retryable failure and records
        // the delivery row as failed in the same pass (see
        // `process_session_once`'s `Err(DeliveryError::SystemAction(_))`
        // arm). The agent-visible inbound row is NOT written in this
        // mode — the row stays in dropped-messages and the operator
        // is expected to investigate.
        assert_eq!(rpt.failed, 1, "expected one failed row on hard-fail");
    }

    #[test]
    fn apply_add_mcp_server_blank_name_is_noop() {
        let (db, ag) = central_with_ag();
        apply_add_mcp_server(&db, ag, &json!({"name": "", "transport": {}})).unwrap();
        apply_add_mcp_server(&db, ag, &json!({"transport": {}})).unwrap();
        // No exception; container_config row should still be unset.
        let cfg = copperclaw_db::tables::container_configs::get(&db, ag).unwrap();
        assert!(cfg.is_none() || !cfg.unwrap().mcp_servers.is_object()
            || copperclaw_db::tables::container_configs::get_mcp_servers(&db, ag)
                .map(|v| v.as_object().is_some_and(serde_json::Map::is_empty))
                .unwrap_or(false));
    }

    /// Card-kind row flows through the new `dispatch_card` path: the
    /// canonical Card is pulled out of `content.card` and the adapter
    /// gets a `deliver_card` call. The MockAdapter doesn't override
    /// `deliver_card`, so it gets the trait-level default which routes
    /// to `deliver` with the text fallback — proving the structure
    /// reached the adapter and was rendered. End-to-end with native
    /// renderers is covered by per-channel adapter crates in wave 2b.
    #[tokio::test]
    async fn card_kind_row_invokes_deliver_card_path() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // Build a representative card and serialise it the way the
        // runner does: `content.card` carries the full Card JSON.
        let card = copperclaw_channels_core::Card {
            title: Some("Order #42".into()),
            body: Some("Confirm?".into()),
            ..copperclaw_channels_core::Card::default()
        };
        let content = json!({
            "card": serde_json::to_value(&card).unwrap(),
        });
        write_row(&out_pool, &make_row(MessageKind::Card, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        // The MockAdapter records every `deliver` call. The default
        // `deliver_card` impl on `ChannelAdapter` converts to text and
        // routes through `deliver`, so the delivery counter ticks once
        // with the text-fallback rendering.
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        let text = deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .expect("text-fallback rendering must land on the `text` field");
        assert!(text.contains("**Order #42**"));
        assert!(text.contains("Confirm?"));
    }

    /// Adapters that explicitly override `deliver_card` to return
    /// `Err(AdapterError::Unsupported)` get the host's belt-and-braces
    /// fallback: a `deliver` call with the text rendering wrapped in a
    /// Chat-kind `OutboundMessage`. Most adapters never trigger this
    /// (the trait-level default already does the text-fallback), but
    /// adapters that want to *refuse* cards outright can.
    #[tokio::test]
    async fn card_kind_row_falls_back_when_adapter_reports_unsupported() {
        // Bespoke adapter that explicitly returns Unsupported from
        // `deliver_card`. The host should fall back to `deliver` with
        // the text rendering.
        use copperclaw_channels_core::AdapterError as AE;
        struct RefusingCardAdapter {
            channel_type: ChannelType,
            text_deliveries: StdMutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl ChannelAdapter for RefusingCardAdapter {
            fn channel_type(&self) -> &ChannelType {
                &self.channel_type
            }
            async fn deliver(
                &self,
                _platform_id: &str,
                _thread_id: Option<&str>,
                message: &OutboundMessage,
            ) -> Result<Option<String>, AE> {
                let text = message
                    .content
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.text_deliveries.lock().unwrap().push(text);
                Ok(Some("plat-id-1".into()))
            }
            async fn deliver_card(
                &self,
                _platform_id: &str,
                _thread_id: Option<&str>,
                _card: &copperclaw_channels_core::Card,
                _to: Option<&str>,
            ) -> Result<Option<String>, AE> {
                Err(AE::Unsupported("this adapter rejects cards".into()))
            }
        }
        let refusing = Arc::new(RefusingCardAdapter {
            channel_type: ChannelType::new("mock"),
            text_deliveries: StdMutex::new(vec![]),
        });

        let (service, _root, sess, _mock) = make_service().await;
        // Replace the registered MockAdapter on channel "mock" with our
        // refusing one.
        service.register_adapter(
            ChannelType::new("mock"),
            refusing.clone() as Arc<dyn ChannelAdapter>,
        );

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let card = copperclaw_channels_core::Card {
            title: Some("Hi".into()),
            ..copperclaw_channels_core::Card::default()
        };
        let content = json!({ "card": serde_json::to_value(&card).unwrap() });
        write_row(&out_pool, &make_row(MessageKind::Card, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        // The refusing adapter's `deliver` was invoked once with the
        // text fallback — proves the host did the belt-and-braces
        // fallback after Unsupported.
        let texts = refusing.text_deliveries.lock().unwrap().clone();
        assert_eq!(texts.len(), 1, "expected exactly one fallback deliver");
        assert!(texts[0].contains("**Hi**"), "got: {:?}", texts[0]);
    }

    /// A Card-kind row whose `content.card` is malformed JSON (or
    /// missing) is a host-level bug, not a user-recoverable transient
    /// — record `failed` and don't keep retrying. The runner's MCP
    /// boundary validates Cards on the way in, so reaching this branch
    /// means the row was corrupted in flight.
    #[tokio::test]
    async fn card_kind_row_with_malformed_card_marks_failed() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // No `card` key at all.
        write_row(
            &out_pool,
            &make_row(MessageKind::Card, json!({"unrelated": "junk"})),
        );

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        assert_eq!(rpt.delivered, 0);
        assert!(mock.deliveries().is_empty());
    }

    // ---------------------------------------------------------------
    // Slice 3.4 — long-output expander dispatch routing.
    //
    // When `dispatch_chat` sees a Chat-kind row with `content.expander`
    // set, it must branch to `dispatch_collapsible` (which in turn
    // calls the adapter's `deliver_collapsible` hook) rather than the
    // ordinary text-splitter pipeline.
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn chat_row_with_expander_routes_via_deliver_collapsible() {
        // Build a chat row that carries the slice-3.4 decorator
        // alongside the full body. The default `deliver_collapsible`
        // impl on MockAdapter routes through `deliver` with a
        // summary + preview body, so we verify the recorded text
        // matches the helper output (proving the host invoked the
        // collapsible hook and not the regular `deliver`).
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let body = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview: Vec<String> = (0..4).map(|i| format!("line {i}")).collect();
        let content = json!({
            "text": body,
            "expander": {
                "summary": "shell produced 40 lines",
                "summary_kind": "lines",
                "preview_lines": preview,
            },
        });
        write_row(&out_pool, &make_row(MessageKind::Chat, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        let text = deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .expect("collapsible fallback text rendering must land on `text`");
        // The trait-default fallback's body starts with the summary
        // and includes the truncation marker for the remaining lines.
        assert!(text.starts_with("shell produced 40 lines"), "got: {text:?}");
        assert!(text.contains("…(36 more lines"), "got: {text:?}");
        // First preview line is in the body too.
        assert!(text.contains("line 0"), "got: {text:?}");
    }

    #[tokio::test]
    async fn chat_row_without_expander_skips_collapsible_path() {
        // Sanity: a regular chat row (no `expander` decorator) must
        // continue to flow through the ordinary text-splitter path.
        // We verify by checking the adapter received exactly the
        // original body — no summary/truncation rewrite.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let content = json!({ "text": "plain short reply" });
        write_row(&out_pool, &make_row(MessageKind::Chat, content));

        service.process_session_once(&sess).await.unwrap();
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        let text = deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(text, "plain short reply");
    }

    #[tokio::test]
    async fn chat_row_with_expander_falls_back_when_unsupported() {
        // Adapters that explicitly override `deliver_collapsible` to
        // return `Err(AdapterError::Unsupported)` get the host's
        // belt-and-braces fallback: a plain `deliver` call with the
        // summary + preview rendering. Dual of the existing card path
        // `card_kind_row_falls_back_when_adapter_reports_unsupported`.
        use copperclaw_channels_core::AdapterError as AE;
        struct RefusingCollapsibleAdapter {
            channel_type: ChannelType,
            text_deliveries: StdMutex<Vec<String>>,
        }
        #[async_trait::async_trait]
        impl ChannelAdapter for RefusingCollapsibleAdapter {
            fn channel_type(&self) -> &ChannelType {
                &self.channel_type
            }
            async fn deliver(
                &self,
                _platform_id: &str,
                _thread_id: Option<&str>,
                message: &OutboundMessage,
            ) -> Result<Option<String>, AE> {
                let text = message
                    .content
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.text_deliveries.lock().unwrap().push(text);
                Ok(Some("plat-id-1".into()))
            }
            async fn deliver_collapsible(
                &self,
                _platform_id: &str,
                _thread_id: Option<&str>,
                _text: &str,
                _summary: &str,
                _preview_lines: &[String],
            ) -> Result<Option<String>, AE> {
                Err(AE::Unsupported("adapter rejects collapsible".into()))
            }
        }
        let refusing = Arc::new(RefusingCollapsibleAdapter {
            channel_type: ChannelType::new("mock"),
            text_deliveries: StdMutex::new(vec![]),
        });

        let (service, _root, sess, _mock) = make_service().await;
        service.register_adapter(
            ChannelType::new("mock"),
            refusing.clone() as Arc<dyn ChannelAdapter>,
        );

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let body = "alpha\nbeta\ngamma";
        let content = json!({
            "text": body,
            "expander": {
                "summary": "shell 3 lines",
                "summary_kind": "lines",
                "preview_lines": ["alpha"],
            },
        });
        write_row(&out_pool, &make_row(MessageKind::Chat, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        let texts = refusing.text_deliveries.lock().unwrap().clone();
        assert_eq!(texts.len(), 1, "expected exactly one fallback deliver");
        assert!(texts[0].starts_with("shell 3 lines"), "got: {:?}", texts[0]);
    }

    #[tokio::test]
    async fn chat_row_with_expander_missing_fields_uses_defaults() {
        // If the row's `content.expander` lacks `summary` or
        // `preview_lines` (corrupted row / runner bug) the dispatch
        // path must still deliver — falling back to sensible
        // defaults rather than failing the row.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let content = json!({
            "text": "some\ntext\nhere",
            "expander": {}, // intentionally empty
        });
        write_row(&out_pool, &make_row(MessageKind::Chat, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        let text = deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        // The default summary string is "(long output)" — verifies
        // the missing-field defensive path.
        assert!(text.contains("(long output)"), "got: {text:?}");
    }

    /// The Breadcrumb-kind dispatch path mirrors the Card-kind one:
    /// the host pulls `content.breadcrumb` out of the row, hands it to
    /// the adapter's `deliver_breadcrumb` hook, and the trait-level
    /// default text-fallback rendering reaches `deliver`. Proves the
    /// row's structured payload survives the round trip and the host
    /// invokes the right adapter method.
    #[tokio::test]
    async fn breadcrumb_kind_row_invokes_deliver_breadcrumb_path() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let breadcrumb = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        let content = json!({
            "breadcrumb": serde_json::to_value(&breadcrumb).unwrap(),
        });
        write_row(&out_pool, &make_row(MessageKind::Breadcrumb, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        let text = deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .expect("trait-default text fallback must land on `text`");
        assert_eq!(text, "[shell] cargo check");
    }

    /// A Breadcrumb-kind row whose `content.breadcrumb` is malformed
    /// or missing is a host-level bug — record `failed` and don't
    /// retry forever.
    #[tokio::test]
    async fn breadcrumb_kind_row_with_malformed_payload_marks_failed() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_row(
            &out_pool,
            &make_row(MessageKind::Breadcrumb, json!({"junk": "x"})),
        );
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        assert_eq!(rpt.delivered, 0);
        assert!(mock.deliveries().is_empty());
    }

    /// The `update_breadcrumb` system action resolves the prior chip's
    /// platform message id (from the inbound `delivered` table) and
    /// re-runs `deliver_breadcrumb` with `existing_message_id=Some`.
    /// The MockAdapter doesn't have a real edit API, so this end-to-
    /// end test verifies the row is recorded as delivered and the
    /// breadcrumb's finished state (Done / summary) reached the
    /// adapter via the text fallback.
    #[tokio::test]
    async fn update_breadcrumb_system_action_dispatches_via_deliver_breadcrumb() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // Step 1: write & process a Running chip so the dispatcher
        // records it in `delivered` with a platform message id.
        let running = copperclaw_channels_core::Breadcrumb::running("shell")
            .with_detail("cargo check");
        write_row(
            &out_pool,
            &make_row(
                MessageKind::Breadcrumb,
                json!({ "breadcrumb": serde_json::to_value(&running).unwrap() }),
            ),
        );
        let _ = service.process_session_once(&sess).await.unwrap();
        assert_eq!(mock.deliveries().len(), 1);

        // Step 2: write the update — System row carrying the
        // `update_breadcrumb` action with the Done shape.
        let done = running.clone().finished(true, Some("passed (0.4s)".into()));
        write_row(
            &out_pool,
            &make_row(
                MessageKind::System,
                json!({
                    "update_breadcrumb": {
                        "tool_name": "shell",
                        "breadcrumb": serde_json::to_value(&done).unwrap(),
                    }
                }),
            ),
        );
        let _ = service.process_session_once(&sess).await.unwrap();
        // Mock's trait-default `deliver_breadcrumb` adds another
        // `deliver` call carrying the finished chip's text fallback.
        let deliveries = mock.deliveries();
        assert!(deliveries.len() >= 2);
        let last_text = deliveries
            .last()
            .unwrap()
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(
            last_text.contains("passed (0.4s)"),
            "finished chip's summary must reach the adapter: {last_text:?}",
        );
    }

    /// A Diff-kind row pulls `content.diff` out, hands the canonical
    /// `DiffCard` to the adapter's `deliver_diff` hook, and the
    /// trait-level default text-fallback rendering reaches `deliver`.
    /// Mirrors `breadcrumb_kind_row_invokes_deliver_breadcrumb_path`.
    #[tokio::test]
    async fn diff_kind_row_invokes_deliver_diff_path() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let card = copperclaw_channels_core::DiffCard {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            hunks: vec![copperclaw_channels_core::DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Remove,
                        text: "fn old() {}".into(),
                    },
                    copperclaw_channels_core::DiffLine {
                        kind: copperclaw_channels_core::DiffLineKind::Add,
                        text: "fn new() {}".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let content = json!({ "diff": serde_json::to_value(&card).unwrap() });
        write_row(&out_pool, &make_row(MessageKind::Diff, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        let text = deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .expect("trait-default text fallback must land on `text`");
        assert!(text.contains("--- a/src/main.rs"));
        assert!(text.contains("+fn new() {}"));
        assert!(text.contains("-fn old() {}"));
        assert!(text.contains("(+1 / -1)"));
    }

    /// A Diff-kind row whose `content.diff` is malformed or missing
    /// is a host-level bug — record `failed` and don't retry forever.
    #[tokio::test]
    async fn diff_kind_row_with_malformed_payload_marks_failed() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_row(
            &out_pool,
            &make_row(MessageKind::Diff, json!({"junk": "x"})),
        );
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        assert_eq!(rpt.delivered, 0);
        assert!(mock.deliveries().is_empty());
    }

    // ── TodoList dispatch ──────────────────────────────────────────

    fn build_dispatch_todo_list() -> copperclaw_channels_core::TodoList {
        copperclaw_channels_core::TodoList {
            items: vec![
                copperclaw_channels_core::TodoListItem {
                    id: 1,
                    text: "Reply with order status".into(),
                    status: copperclaw_channels_core::TodoItemStatus::Pending,
                },
            ],
            title: None,
        }
    }

    /// A TodoList-kind row pulls `content.todo_list` out, hands the
    /// canonical `TodoList` to the adapter's `deliver_todo_list` hook,
    /// and the trait-level default text-fallback rendering reaches
    /// `deliver` carrying the `[ ]` glyph + footer. Mirrors
    /// `breadcrumb_kind_row_invokes_deliver_breadcrumb_path`.
    #[tokio::test]
    async fn todo_list_kind_row_invokes_deliver_todo_list_path() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let list = build_dispatch_todo_list();
        let content = json!({
            "todo_list": serde_json::to_value(&list).unwrap(),
        });
        write_row(&out_pool, &make_row(MessageKind::TodoList, content));
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        let deliveries = mock.deliveries();
        assert_eq!(deliveries.len(), 1);
        let text = deliveries[0]
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .expect("trait-default text fallback must land on `text`");
        // Text fallback includes the default title + the pending glyph.
        assert!(text.starts_with("Plan\n"), "got: {text:?}");
        assert!(text.contains("[ ] Reply with order status"));
    }

    /// A TodoList-kind row whose `content.todo_list` is malformed or
    /// missing is a host-level bug — record `failed` and don't retry
    /// forever. Mirrors `breadcrumb_kind_row_with_malformed_payload_marks_failed`.
    #[tokio::test]
    async fn todo_list_kind_row_with_malformed_payload_marks_failed() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_row(
            &out_pool,
            &make_row(MessageKind::TodoList, json!({"junk": "x"})),
        );
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        assert_eq!(rpt.delivered, 0);
        assert!(mock.deliveries().is_empty());
    }

    // ----------------------------------------------------------------------
    // Splitter / chunk-progress retry coverage (regression for the bug where
    // a partial-success split would re-send chunk 0 on every retry, up to
    // MAX_DELIVERY_ATTEMPTS copies of every already-delivered chunk).
    //
    // The shared `MockAdapter` returns `None` from `max_message_chars`, so
    // these tests wrap it with a minimal forwarding adapter that overrides
    // the cap. Every other method delegates to the inner mock so the
    // existing `deliveries()` / `fail_next_deliver()` helpers keep working.
    // ----------------------------------------------------------------------

    /// Test-only adapter wrapper: forces a `max_message_chars()` cap so the
    /// host's splitter actually fires, while delegating `deliver` to the
    /// inner `MockAdapter`. Adds a `fail_at_call_index` map so tests can
    /// say "the Nth `deliver` call on this wrapper returns `err`" — useful
    /// for "fail chunk 1 but let chunk 0 through" patterns that the inner
    /// mock's FIFO queue can't express. Failures from this map DO NOT
    /// touch the inner mock's recorder, so `deliveries()` reflects the
    /// successful chunks only — same shape as a real adapter that errored
    /// out without sending anything.
    struct SplittingMockAdapter {
        inner: Arc<MockAdapter>,
        cap: usize,
        call_count: StdMutex<u32>,
        // (call-index, error)
        scheduled_failures: StdMutex<Vec<(u32, AdapterError)>>,
    }

    impl SplittingMockAdapter {
        fn new(inner: Arc<MockAdapter>, cap: usize) -> Self {
            Self {
                inner,
                cap,
                call_count: StdMutex::new(0),
                scheduled_failures: StdMutex::new(vec![]),
            }
        }

        /// Schedule a failure for the `index`-th `deliver` call (0-based).
        /// Multiple schedulings stack; the wrapper consumes the entry when
        /// it fires. Indices that never come up (e.g. failed retries
        /// drained early) stay in the list — tests don't have to clean up.
        fn fail_at_call(&self, index: u32, err: AdapterError) {
            self.scheduled_failures
                .lock()
                .expect("poisoned")
                .push((index, err));
        }
    }

    #[async_trait::async_trait]
    impl ChannelAdapter for SplittingMockAdapter {
        fn channel_type(&self) -> &ChannelType {
            self.inner.channel_type()
        }
        fn max_message_chars(&self) -> Option<usize> {
            Some(self.cap)
        }
        async fn deliver(
            &self,
            platform_id: &str,
            thread_id: Option<&str>,
            message: &OutboundMessage,
        ) -> Result<Option<String>, AdapterError> {
            let this_call = {
                let mut c = self.call_count.lock().expect("poisoned");
                let cur = *c;
                *c += 1;
                cur
            };
            // Pop a scheduled failure matching this index, if any.
            let popped = {
                let mut guard = self.scheduled_failures.lock().expect("poisoned");
                guard
                    .iter()
                    .position(|(idx, _)| *idx == this_call)
                    .map(|pos| guard.remove(pos).1)
            };
            if let Some(err) = popped {
                return Err(err);
            }
            self.inner.deliver(platform_id, thread_id, message).await
        }
    }

    /// Build a 3-chunk text under a 10-char cap. Uses paragraph breaks so
    /// the splitter cuts cleanly on `\n\n` boundaries: ten `a`s, ten `b`s,
    /// ten `c`s, total 32 chars body, cap 10 → 3 chunks of 10 chars each.
    fn three_chunk_text() -> String {
        format!(
            "{}\n\n{}\n\n{}",
            "a".repeat(10),
            "b".repeat(10),
            "c".repeat(10)
        )
    }

    /// Install a `SplittingMockAdapter` over the existing `MockAdapter` on
    /// channel "mock". The host re-resolves adapters by channel-type lookup
    /// on every dispatch, so this swap is picked up immediately. Returns
    /// the wrapper Arc so tests can schedule per-call failures.
    fn install_splitting_adapter(
        service: &Arc<DeliveryService>,
        inner: Arc<MockAdapter>,
        cap: usize,
    ) -> Arc<SplittingMockAdapter> {
        let wrapper = Arc::new(SplittingMockAdapter::new(inner, cap));
        service.register_adapter(
            ChannelType::new("mock"),
            wrapper.clone() as Arc<dyn ChannelAdapter>,
        );
        wrapper
    }

    #[tokio::test]
    async fn split_happy_path_no_duplicate_chunks() {
        // 3-chunk text, all 3 succeed on the first attempt. Assert the
        // adapter saw exactly 3 deliver calls, in order, and the retry
        // entry was cleaned up (no chunk-progress state left behind to
        // confuse a future row reusing the same key).
        let (service, _root, sess, mock) = make_service().await;
        install_splitting_adapter(&service, mock.clone(), 10);
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::Chat,
            json!({"text": three_chunk_text()}),
        );
        write_row(&out_pool, &row);

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);
        assert_eq!(rpt.deferred, 0);

        let calls = mock.deliveries();
        assert_eq!(
            calls.len(),
            3,
            "expected exactly 3 chunks (no duplicates), got {} ({:?})",
            calls.len(),
            calls
                .iter()
                .map(|c| c.message.content.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(calls[0].message.content["text"], json!("a".repeat(10)));
        assert_eq!(calls[1].message.content["text"], json!("b".repeat(10)));
        assert_eq!(calls[2].message.content["text"], json!("c".repeat(10)));

        // process_session_once clears the retry-state entry on success.
        let key = DeliveryKey::new(sess.id, row.id);
        assert!(service.retries.get(&key).is_none());

        // The delivered row recorded the FIRST chunk's platform id.
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "ok");
        assert_eq!(listed[0].platform_message_id.as_deref(), Some("mock-1"));
    }

    #[tokio::test]
    async fn split_partial_success_retry_skips_delivered_chunks() {
        // 3-chunk text, chunk 0 succeeds, chunk 1 returns
        // AdapterError::Rate { retry_after: Some(1) }. After the backoff
        // window we run the loop again. The retry MUST resume at chunk 1
        // (not chunk 0), so the inner mock sees the call sequence:
        //   attempt 1: chunk 0 (ok via inner mock), chunk 1 (rate from wrapper)
        //   attempt 2: chunk 1 (ok via inner mock), chunk 2 (ok via inner mock)
        // Inner mock's `deliveries()` records ONLY the calls that landed
        // on it — chunk 0 once, chunk 1 once, chunk 2 once (no
        // duplicate of chunk 0). This is the headline bug.
        let (service, _root, sess, mock) = make_service().await;
        let wrapper = install_splitting_adapter(&service, mock.clone(), 10);
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::Chat,
            json!({"text": three_chunk_text()}),
        );
        write_row(&out_pool, &row);

        // The wrapper sees three `deliver` calls per attempt (one per
        // chunk). On attempt 1: call 0 = chunk 0 (let through), call 1 =
        // chunk 1 (fail Rate). On attempt 2: call 2 = chunk 1 (let
        // through), call 3 = chunk 2 (let through). Schedule the chunk-1
        // failure at wrapper call index 1.
        wrapper.fail_at_call(1, AdapterError::Rate { retry_after: Some(1) });

        let rpt1 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt1.deferred, 1);
        assert_eq!(rpt1.delivered, 0);

        // After attempt 1 the inner mock recorded ONLY chunk 0 — the
        // wrapper short-circuited chunk 1 before it reached the recorder.
        let after1 = mock.deliveries();
        assert_eq!(
            after1.len(),
            1,
            "after attempt 1 expected only chunk 0 recorded on the inner mock, got {after1:?}"
        );
        assert_eq!(after1[0].message.content["text"], json!("a".repeat(10)));

        // Retry-state should reflect chunks_sent=1 and the first chunk's pid.
        let key = DeliveryKey::new(sess.id, row.id);
        {
            let state = service.retries.get(&key).expect("retry state recorded");
            assert_eq!(state.chunks_sent, 1, "must record chunk-0 success");
            assert_eq!(state.first_chunk_pid.as_deref(), Some("mock-1"));
        }

        // Force backoff window into the past so the retry runs.
        if let Some(mut entry) = service.retries.get_mut(&key) {
            entry.not_before = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap_or_else(Instant::now);
        }

        let rpt2 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt2.delivered, 1, "retry should deliver the row");
        assert_eq!(rpt2.failed, 0);

        // The inner mock must have seen exactly 3 successful deliveries
        // in the canonical order — NOT 4 (which would mean chunk 0 was
        // re-sent on the retry). This is the regression assertion.
        let calls = mock.deliveries();
        assert_eq!(
            calls.len(),
            3,
            "expected 3 successful chunks (chunk 0 from attempt 1, chunks 1 + 2 from attempt 2); got {} — duplicate chunk 0 would mean the splitter retry regression",
            calls.len()
        );
        assert_eq!(calls[0].message.content["text"], json!("a".repeat(10)));
        assert_eq!(calls[1].message.content["text"], json!("b".repeat(10)));
        assert_eq!(calls[2].message.content["text"], json!("c".repeat(10)));

        // Retry state cleared on final success.
        assert!(service.retries.get(&key).is_none());
    }

    #[tokio::test]
    async fn split_retry_exhaustion_does_not_replay_first_chunk() {
        // Chunk 0 succeeds on attempt 1, chunk 1 fails on every attempt.
        // After MAX_DELIVERY_ATTEMPTS (3) the row is marked failed.
        // Without the fix, the adapter would see chunk 0 re-sent on
        // every retry: chunk-0 count = 3. With the fix, chunk 0 reaches
        // the inner mock exactly once (attempt 1); attempts 2 and 3
        // resume at chunk 1, hit the scheduled failure, and never touch
        // chunk 0 again.
        let (service, _root, sess, mock) = make_service().await;
        let wrapper = install_splitting_adapter(&service, mock.clone(), 10);
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::Chat,
            json!({"text": three_chunk_text()}),
        );
        write_row(&out_pool, &row);

        // Wrapper call indices that should fail (one per attempt):
        //   attempt 1: calls 0 (chunk 0 ok) + 1 (chunk 1 FAIL).
        //   attempt 2: call 2 (chunk 1 FAIL — resume from index 1).
        //   attempt 3: call 3 (chunk 1 FAIL — resume from index 1).
        wrapper.fail_at_call(1, AdapterError::Transport("502-attempt1".into()));
        wrapper.fail_at_call(2, AdapterError::Transport("502-attempt2".into()));
        wrapper.fail_at_call(3, AdapterError::Transport("502-attempt3".into()));

        let key = DeliveryKey::new(sess.id, row.id);
        for _attempt in 0..MAX_DELIVERY_ATTEMPTS {
            // Force the backoff window into the past for the next loop.
            if let Some(mut entry) = service.retries.get_mut(&key) {
                entry.not_before = Instant::now()
                    .checked_sub(Duration::from_secs(2))
                    .unwrap_or_else(Instant::now);
            }
            let _ = service.process_session_once(&sess).await.unwrap();
        }

        // The row is now marked failed in the inbound `delivered` table.
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");

        // The inner mock saw chunk 0 EXACTLY ONCE across all three
        // attempts — the regression would have it appear three times.
        let calls = mock.deliveries();
        let chunk0_text = "a".repeat(10);
        let chunk0_count = calls
            .iter()
            .filter(|c| c.message.content["text"] == json!(chunk0_text))
            .count();
        assert_eq!(
            chunk0_count, 1,
            "chunk 0 must be delivered exactly once across retries (got {chunk0_count}); pre-fix bug duplicates it on every retry"
        );

        // And chunk 1 never reached the inner mock — it failed at the
        // wrapper boundary every time. Belt-and-braces assertion that
        // matches the test's chunk-routing intent.
        let chunk1_text = "b".repeat(10);
        let chunk1_count = calls
            .iter()
            .filter(|c| c.message.content["text"] == json!(chunk1_text))
            .count();
        assert_eq!(chunk1_count, 0, "chunk 1 should never have reached the inner mock");

        // Retry state cleared once the row was marked failed.
        assert!(service.retries.get(&key).is_none());
    }

    #[tokio::test]
    async fn split_first_chunk_pid_stable_across_retries() {
        // The `delivered` row's `platform_message_id` is the address that
        // future `edit_message` / `add_reaction` calls will target. It
        // MUST be the FIRST chunk's platform id — even if later chunks
        // fail and retry, the recorded id stays anchored to chunk 0.
        //
        // Sequence: chunk 0 ok (pid "mock-1"), chunk 1 fails, retry
        // delivers chunk 1 (mock-2) and chunk 2 (mock-3). The delivered
        // row must show "mock-1", not "mock-2" or "mock-3".
        let (service, _root, sess, mock) = make_service().await;
        let wrapper = install_splitting_adapter(&service, mock.clone(), 10);
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::Chat,
            json!({"text": three_chunk_text()}),
        );
        write_row(&out_pool, &row);

        // Fail wrapper call index 1 (chunk 1 on attempt 1); attempt 2
        // resumes at chunk 1 with no scheduled failure → success.
        wrapper.fail_at_call(1, AdapterError::Transport("502".into()));
        let _ = service.process_session_once(&sess).await.unwrap();

        // Pop the backoff window for the retry.
        let key = DeliveryKey::new(sess.id, row.id);
        if let Some(mut entry) = service.retries.get_mut(&key) {
            entry.not_before = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .unwrap_or_else(Instant::now);
        }
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);

        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "ok");
        // mock-1 is the id the inner MockAdapter returned for chunk 0's
        // `deliver`. mock-2 / mock-3 are the retry deliveries; neither
        // must be recorded as the row's anchor pid.
        assert_eq!(
            listed[0].platform_message_id.as_deref(),
            Some("mock-1"),
            "platform_message_id must remain the first chunk's id across retries"
        );
    }
}
