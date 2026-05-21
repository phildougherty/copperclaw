//! `DeliveryService` — owns the active and sweep loops.
//!
//! Per-session work runs in [`DeliveryService::process_session_once`] (see
//! `loops.rs` for the periodic schedulers that drive it).

use crate::dispatch::{AdapterResolver, HostDispatcher};
use crate::error::DeliveryError;
use crate::system_actions::parse_system_content;
use dashmap::DashMap;
use ironclaw_channels_core::ChannelAdapter;
use ironclaw_db::central::CentralDb;
use ironclaw_db::session::{open_inbound, open_outbound, SessionPaths};
use ironclaw_db::tables::{delivered, messages_out, session_routing};
use ironclaw_modules::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryDispatcher, DispatchTarget,
};
use ironclaw_types::{
    AgentGroupId, ChannelType, ContainerStatus, MessageId, MessageKind, MessageOutRow,
    OutboundMessage, Session, SessionId, SessionStatus,
};
use rusqlite::Connection;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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

            match result {
                Ok(()) => {
                    self.retries.remove(&key);
                    report.delivered += 1;
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
                    warn!(?row.id, "no route resolvable, marking failed");
                }
                Err(err) => {
                    // Non-retryable adapter error -> mark failed immediately.
                    let in_conn = inbound_pool.connect()?;
                    delivered::insert(&in_conn, row.id, None, "failed")?;
                    self.retries.remove(&key);
                    report.failed += 1;
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
                self.handle_system(row, &target, inbound_pool).await?;
            }
            MessageKind::Agent => {
                // Agent-to-agent delivery: the agent_to_agent module owns the
                // implementation via the action registry. In the absence of
                // that handler the row is recorded as delivered with status
                // "ok" — there is no external channel to invoke.
                if let Some(handler) = self.actions.get("agent_dispatch").map(|r| r.clone()) {
                    let input = DeliveryActionInput {
                        action: "agent_dispatch".into(),
                        payload: row.content.clone(),
                        target: target.clone(),
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

        let handler = self.actions.get(&action.name).map(|r| r.clone());
        let Some(handler) = handler else {
            info!(name = %action.name, "no handler for system action; skipping");
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, None, "ok")?;
            return Ok(());
        };

        let input = DeliveryActionInput {
            action: action.name,
            payload: action.payload,
            target: target.clone(),
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
async fn call_adapter(
    adapter: &dyn ChannelAdapter,
    platform_id: &str,
    thread_id: Option<&str>,
    message: &OutboundMessage,
) -> Result<Option<String>, DeliveryError> {
    adapter
        .deliver(platform_id, thread_id, message)
        .await
        .map_err(DeliveryError::Adapter)
}

/// Helper used by tests / the sweep loop to filter on container status.
pub(crate) fn is_container_running(s: &Session) -> bool {
    s.container_status == ContainerStatus::Running
}

/// Helper used by tests / the sweep loop to filter on session status.
pub(crate) fn is_session_active(s: &Session) -> bool {
    s.status == SessionStatus::Active
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_service, MockRoot};
    use chrono::Utc;
    use ironclaw_channels_core::testing::MockAdapter;
    use ironclaw_channels_core::AdapterError;
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
}
