//! `DeliveryService` — owns the active and sweep loops.
//!
//! Per-session work runs in [`DeliveryService::process_session_once`] (see
//! `loops.rs` for the periodic schedulers that drive it).

use crate::dispatch::{AdapterResolver, HostDispatcher};
use crate::error::DeliveryError;
use crate::system_actions::{parse_system_content, ParsedAction};
use dashmap::DashMap;
use ironclaw_channels_core::{AdapterError, ChannelAdapter};
use ironclaw_db::central::CentralDb;
use ironclaw_db::session::{open_inbound, open_outbound, SessionPaths};
use ironclaw_db::tables::{delivered, messages_in, messages_out, session_routing};
use ironclaw_modules::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryDispatcher, DispatchTarget,
};
use ironclaw_types::{
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
#[derive(Debug, Clone, Copy)]
struct RetryState {
    /// Number of attempts already made (>= 1).
    tries: u32,
    /// `Instant` after which the row may be retried.
    not_before: Instant,
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
    /// in-line. Initialised from `IRONCLAW_SELFMOD_HARD_FAIL` at
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
    /// 1. Create the [`ChannelRegistry`](ironclaw_channels_core::ChannelRegistry) and
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

    /// Whether the service was started with `IRONCLAW_SELFMOD_HARD_FAIL`
    /// enabled. Exposed for tests and for `iclaw doctor` to report.
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
            if let Some(state) = self.retries.get(&key).map(|r| *r) {
                if now < state.not_before {
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
                    ironclaw_metrics::inc_messages_outbound(&channel_label);
                }
                Err(err) if err.is_retryable() => {
                    let outcome = self.bump_retry(&key);
                    match outcome {
                        DeferOutcome::Defer => {
                            report.deferred += 1;
                            debug!(?err, ?row.id, "deferring retryable delivery failure");
                        }
                        DeferOutcome::Fail => {
                            let in_conn = inbound_pool.connect()?;
                            delivered::insert(&in_conn, row.id, None, "failed")?;
                            self.retries.remove(&key);
                            report.failed += 1;
                            ironclaw_metrics::inc_delivery_failed(&channel_label);
                            warn!(?err, ?row.id, "exhausted retry budget, marking failed");
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
                    ironclaw_metrics::inc_delivery_failed(&channel_label);
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
                    ironclaw_metrics::inc_delivery_failed(&channel_label);
                    warn!(?row.id, "no route resolvable, marking failed");
                }
                Err(err) => {
                    // Non-retryable adapter error -> mark failed immediately.
                    let in_conn = inbound_pool.connect()?;
                    delivered::insert(&in_conn, row.id, None, "failed")?;
                    self.retries.remove(&key);
                    report.failed += 1;
                    ironclaw_metrics::inc_delivery_failed(&channel_label);
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
        routing: Option<&ironclaw_types::routing::SessionRouting>,
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
                // Agent-to-agent delivery: the agent_to_agent module owns the
                // implementation via the action registry. In the absence of
                // that handler the row is recorded as delivered with status
                // "ok" — there is no external channel to invoke.
                if let Some(handler) = self.actions.get("agent_dispatch").map(|r| r.clone()) {
                    // TODO(team-sc): session_id wired through for the
                    // scheduling action handler; other handlers ignore.
                    let input = DeliveryActionInput {
                        action: "agent_dispatch".into(),
                        payload: row.content.clone(),
                        target: target.clone(),
                        session_id: Some(sess.id),
                    };
                    let _ = handler
                        .handle(input)
                        .map_err(|err| DeliveryError::SystemAction(err.to_string()))?;
                }
                let in_conn = inbound_pool.connect()?;
                delivered::insert(&in_conn, row.id, None, "ok")?;
            }
            MessageKind::Chat | MessageKind::Task | MessageKind::Webhook => {
                self.dispatch_chat(row, &target, inbound_pool).await?;
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

        // TODO(team-sc): wire session_id + agent_group_id through to the
        // delivery action handler so the scheduling module can identify
        // which session a `schedule` op targets. Other handlers ignore.
        let mut handler_target = target.clone();
        if handler_target.agent_group_id.is_none() {
            handler_target.agent_group_id = Some(sess.agent_group_id);
        }
        let input = DeliveryActionInput {
            action: action.name,
            payload: action.payload,
            target: handler_target,
            session_id: Some(sess.id),
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
        apply: Result<(), ironclaw_db::DbError>,
    ) -> Result<(), DeliveryError> {
        match apply {
            Ok(()) => {
                ironclaw_metrics::inc_self_mod_succeeded(action);
                let in_conn = inbound_pool.connect()?;
                delivered::insert(&in_conn, row.id, None, "ok")?;
                Ok(())
            }
            Err(err) => {
                if self.selfmod_hard_fail.load(Ordering::Relaxed) {
                    ironclaw_metrics::inc_self_mod_failed(action);
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

        let outbound = OutboundMessage {
            kind: row.kind,
            content: row.content.clone(),
            files: vec![],
        };
        let platform_message_id = call_adapter(
            adapter.as_ref(),
            &platform_id,
            target.thread_id.as_deref(),
            &outbound,
        )
        .await?;
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        Ok(())
    }

    fn resolve_target(
        row: &MessageOutRow,
        routing: Option<&ironclaw_types::routing::SessionRouting>,
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

    fn bump_retry(&self, key: &DeliveryKey) -> DeferOutcome {
        let now = Instant::now();
        let mut entry = self.retries.entry(*key).or_insert(RetryState {
            tries: 0,
            not_before: now,
        });
        entry.tries += 1;
        if entry.tries >= MAX_DELIVERY_ATTEMPTS {
            return DeferOutcome::Fail;
        }
        let delay = backoff_delay_ms(entry.tries);
        entry.not_before = now + Duration::from_millis(delay);
        DeferOutcome::Defer
    }

    /// Snapshot of sessions visible to the active loop (status=Active,
    /// container=Running).
    pub fn list_running_sessions(&self) -> Result<Vec<Session>, DeliveryError> {
        Ok(ironclaw_db::tables::sessions::list_running(&self.central)?)
    }

    /// Snapshot of sessions visible to the sweep loop (status=Active).
    pub fn list_active_sessions(&self) -> Result<Vec<Session>, DeliveryError> {
        Ok(ironclaw_db::tables::sessions::list_active(&self.central)?)
    }
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
/// (`ironclaw_delivery_formatting_fallback_total{channel_type}`) is
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
                    ironclaw_metrics::inc_delivery_formatting_fallback(ct);
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
        .map_err(ironclaw_db::DbError::from)?;
    let row: Option<String> = stmt
        .query_row([seq], |r| r.get::<_, String>(0))
        .optional()
        .map_err(ironclaw_db::DbError::from)?;
    let Some(id_str) = row else {
        return Ok(None);
    };
    let uuid = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        DeliveryError::SystemAction(format!("invalid outbound row uuid: {e}"))
    })?;
    Ok(Some(MessageId(uuid)))
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
        .map_err(ironclaw_db::DbError::from)?;
    let row: Option<Option<String>> = stmt
        .query_row(
            [message_out_id.as_uuid().to_string()],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(ironclaw_db::DbError::from)?;
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
    central: &ironclaw_db::central::CentralDb,
    _row: &MessageOutRow,
    payload: &serde_json::Value,
) {
    use ironclaw_db::tables::agent_turns;
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

/// Read the `IRONCLAW_SELFMOD_HARD_FAIL` env var once at boot. When
/// set to `1` / `true` / `yes` / `on` (case-insensitive), a failed
/// self-mod apply is surfaced as a [`DeliveryError::SystemAction`]
/// from `handle_system` rather than being recorded as a failed
/// delivery, so the existing retry path can have another go. Default
/// is off — see the per-block recovery in
/// [`DeliveryService::handle_system`].
fn selfmod_hard_fail_from_env() -> bool {
    matches!(
        std::env::var("IRONCLAW_SELFMOD_HARD_FAIL")
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
/// - increments `ironclaw_self_mod_failed_total{action}`;
/// - marks the outbound row as `delivered.status = "failed"` with the
///   error message in the payload so it surfaces in
///   `iclaw dropped-messages outbound-list`;
/// - writes a `MessageKind::System` row to the session's `inbound.db`
///   carrying a `self_mod_error` envelope so the agent can react on
///   its next turn (without this, the runner thinks the install
///   succeeded and loops).
fn record_self_mod_failure(
    sess: &Session,
    row: &MessageOutRow,
    inbound_pool: &SessionPool,
    action: &str,
    err: &ironclaw_db::DbError,
) -> Result<(), DeliveryError> {
    let err_text = err.to_string();
    error!(
        session = %sess.id.as_uuid(),
        agent_group = %sess.agent_group_id.as_uuid(),
        action,
        error = %err_text,
        "self-mod action failed to apply"
    );
    ironclaw_metrics::inc_self_mod_failed(action);

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
    central: &ironclaw_db::central::CentralDb,
    agent_group_id: ironclaw_types::AgentGroupId,
) -> Result<(), ironclaw_db::DbError> {
    use ironclaw_db::tables::container_configs;
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
    central: &ironclaw_db::central::CentralDb,
    agent_group_id: ironclaw_types::AgentGroupId,
    payload: &serde_json::Value,
) -> Result<(), ironclaw_db::DbError> {
    use ironclaw_db::tables::container_configs;
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
    central: &ironclaw_db::central::CentralDb,
    agent_group_id: ironclaw_types::AgentGroupId,
    payload: &serde_json::Value,
) -> Result<(), ironclaw_db::DbError> {
    use ironclaw_db::tables::container_configs;
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
    use ironclaw_channels_core::testing::MockAdapter;
    use ironclaw_channels_core::AdapterError;
    use ironclaw_db::tables::container_configs;
    use ironclaw_db::tables::messages_out::WriteOutbound;
    use ironclaw_modules::context::MockDispatcher;
    use ironclaw_modules::{DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput};
    use ironclaw_modules::ModuleError;
    use ironclaw_types::routing::SessionRouting;
    use ironclaw_types::{MessageKind, OutboundMessage};
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
        // After enough attempts the row must be marked failed.
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
            body.contains(ironclaw_metrics::DELIVERY_FORMATTING_FALLBACK_TOTAL),
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
        use ironclaw_db::tables::sessions as s;
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

    fn central_with_ag() -> (ironclaw_db::central::CentralDb, AgentGroupId) {
        use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
        let db = ironclaw_db::central::CentralDb::open_in_memory().unwrap();
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
        let cfg = ironclaw_db::tables::container_configs::get(&db, ag)
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
        let cfg = ironclaw_db::tables::container_configs::get(&db, ag)
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
        let cfg = ironclaw_db::tables::container_configs::get(&db, ag)
            .unwrap()
            .unwrap();
        assert_eq!(cfg.packages_apt, vec!["jq".to_string()]);
    }

    #[test]
    fn apply_install_packages_empty_payload_is_noop() {
        let (db, ag) = central_with_ag();
        apply_install_packages(&db, ag, &json!({})).unwrap();
        // Row may or may not exist; either way, no apt/npm contributions.
        let cfg = ironclaw_db::tables::container_configs::get(&db, ag).unwrap();
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
            body.contains(ironclaw_metrics::SELF_MOD_FAILED_TOTAL),
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
        let paths = ironclaw_db::session::SessionPaths::new(
            tmp.path(),
            AgentGroupId::new(),
            SessionId::new(),
        );
        let conn = ironclaw_db::session::open_outbound(&paths).unwrap();
        let msg = ironclaw_db::tables::messages_out::WriteOutbound {
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
        let paths = ironclaw_db::session::SessionPaths::new(
            tmp.path(),
            AgentGroupId::new(),
            SessionId::new(),
        );
        let conn = ironclaw_db::session::open_inbound(&paths).unwrap();
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
            body.contains(ironclaw_metrics::SELF_MOD_SUCCEEDED_TOTAL),
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

    /// `IRONCLAW_SELFMOD_HARD_FAIL=1` flips the failure into a returned
    /// `DeliveryError`, so the row is recorded as failed via the
    /// outer loop's `SystemAction` arm (rather than being recorded
    /// inline by `record_self_mod_failure`). The env var is read at
    /// boot and stored on the service; tests flip it via
    /// `set_selfmod_hard_fail` to avoid the Rust 2024 unsafe
    /// requirement on `std::env::set_var`.
    // TODO(team-ip): if operators want to flip this without restart,
    // wire `SIGHUP` to re-read `IRONCLAW_SELFMOD_HARD_FAIL`.
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
        let cfg = ironclaw_db::tables::container_configs::get(&db, ag).unwrap();
        assert!(cfg.is_none() || !cfg.unwrap().mcp_servers.is_object()
            || ironclaw_db::tables::container_configs::get_mcp_servers(&db, ag)
                .map(|v| v.as_object().is_some_and(serde_json::Map::is_empty))
                .unwrap_or(false));
    }
}
