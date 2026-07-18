//! `DeliveryService` — owns the active and sweep loops.
//!
//! Per-session work runs in [`DeliveryService::process_session_once`] (see
//! `loops.rs` for the periodic schedulers that drive it).

use crate::dispatch::{AdapterResolver, HostDispatcher};
use crate::error::DeliveryError;
use crate::system_actions::{ParsedAction, parse_system_content};
use copperclaw_channels_core::{
    AdapterError, Breadcrumb, Card, ChannelAdapter, DiffCard, ErrorCard, ErrorCardKind,
    ThinkingBlock, TodoItemStatus, TodoList, TodoListItem,
};
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
use copperclaw_db::tables::{
    agent_groups, container_configs, delivered, mcp_calls, messages_in, messages_out,
    outbound_dropped_messages, pending_approvals, session_routing, sessions,
};
use copperclaw_modules::{
    DeliveryActionHandler, DeliveryActionInput, DeliveryDispatcher, DispatchTarget, PreviewBroker,
    PublicTunnelBroker, PublicTunnelReply, SessionInfoLite,
};
use copperclaw_types::{
    AgentGroupId, ChannelType, ContainerStatus, MessageId, MessageKind, MessageOutRow,
    OutboundMessage, Session, SessionId, SessionStatus,
};
use dashmap::{DashMap, DashSet};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
/// Host-side ceiling on a single external MCP tool call. Bounds a hung server
/// so the detached drain task (and its per-session guard) can't live forever.
/// Kept below the runner's `EXTERNAL_MCP_DEADLINE_SECS` (120s) so the runner
/// still receives the host's error response before its own poll gives up.
pub const HOST_MCP_CALL_DEADLINE_SECS: u64 = 90;
/// Reserved MCP "server" name (M17) the runner uses to route `expose_preview`
/// / `close_preview` tool calls through the external-MCP relay to the host-side
/// preview broker. Must never collide with a real external server name — the
/// `groups.config.add-mcp-server` handler rejects it.
pub const PREVIEW_SERVER: &str = "__preview";
/// Base value for exponential backoff between retries.
pub const BACKOFF_BASE_MS: u64 = 5_000;
/// Age ceiling (hours) for a pending outbound row whose channel has no live
/// adapter (M21 S5). Below the ceiling the row stays pending — the adapter
/// may simply not have been wired yet this boot; past it the row is
/// dead-lettered into the central `outbound_dropped_messages` table with
/// reason [`NO_ADAPTER_DROP_REASON`], so a permanently-unconfigured or
/// removed channel can't accumulate unbounded pending outbound that nothing
/// drains and nothing reports. Recoverable: `cclaw dropped-messages replay`
/// re-queues the row once the channel is configured.
pub const NO_ADAPTER_MAX_AGE_HOURS: i64 = 24;
/// Reason prefix recorded in `outbound_dropped_messages.last_error` for rows
/// dead-lettered by the no-adapter age ceiling (M21 S5). Operator tooling
/// (`cclaw doctor`, M21 O1) can match this prefix to distinguish no-adapter
/// dead letters from adapter-failure ones.
pub const NO_ADAPTER_DROP_REASON: &str = "no_adapter";

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
///
/// Since M21 S3 this map is a **write-through cache** over the `tries` /
/// `not_before` columns on the row itself (migration 029): every
/// [`DeliveryService::bump_retry`] mirrors the counter and the wall-clock
/// backoff window onto the row, and the cache is primed lazily from the
/// persisted columns on the first poll of each session
/// ([`DeliveryService::prime_retry_cache`]), so attempt budgets and
/// backoff windows survive a host restart. `chunks_sent` /
/// `first_chunk_pid` stay in-memory only — a restart mid-split re-sends
/// the row's chunks from the top, the pre-existing behavior.
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
/// Parse the `port` field of a `__preview` tool input into a valid TCP port
/// (1..=65535). Returns `None` for a missing / non-integer / out-of-range
/// value so the caller can render a clear `is_error` to the model.
fn parse_preview_port(input: &serde_json::Value) -> Option<u16> {
    let n = input.get("port")?.as_u64()?;
    if n == 0 || n > u64::from(u16::MAX) {
        return None;
    }
    u16::try_from(n).ok()
}

fn backoff_delay_ms(tries: u32) -> u64 {
    let exp = tries.saturating_sub(1).min(31);
    let scaled = BACKOFF_BASE_MS.saturating_mul(1u64 << exp);
    scaled.min(ABSOLUTE_CEILING_MS)
}

/// True when a pending outbound row addressed at a channel with no live
/// adapter has aged past the [`NO_ADAPTER_MAX_AGE_HOURS`] ceiling (M21 S5).
/// Age is measured from the row's own `timestamp` (when the runner emitted
/// it) against wall-clock `now`; a future-dated timestamp (clock skew) is
/// never expired.
fn no_adapter_expired(
    row_timestamp: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    now.signed_duration_since(row_timestamp) >= chrono::Duration::hours(NO_ADAPTER_MAX_AGE_HOURS)
}

/// Flatten the `content` array from an [`copperclaw_mcp::call_external_tool`]
/// result (the serialized rmcp `CallToolResult` content blocks) into plain
/// model-facing text. Text blocks are joined with blank lines; non-text blocks
/// (images / resources) are rendered as a short type tag so the model at least
/// sees they happened. Rendering host-side keeps the runner free of any rmcp
/// content-shape knowledge — it just stores and replays this string.
fn render_mcp_content(content: Option<&serde_json::Value>) -> String {
    let Some(serde_json::Value::Array(blocks)) = content else {
        return "(external MCP tool produced no output)".to_string();
    };
    let mut out = String::new();
    for block in blocks {
        let piece = match block.get("type").and_then(serde_json::Value::as_str) {
            // A text block (or an untyped block carrying a `text` field) renders
            // as its text; any other typed block renders as a short type tag.
            Some("text") | None => block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            Some(other) => format!("<{other}>"),
        };
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&piece);
    }
    if out.is_empty() {
        "(external MCP tool produced no output)".to_string()
    } else {
        out
    }
}

/// RAII guard for the per-session external-MCP drain in-flight set. Removing
/// the session's entry on drop (including a panic-unwind in the detached drain
/// task) guarantees a future tick can always re-spawn a drain for the session.
struct McpDrainGuard {
    map: Arc<DashMap<SessionId, Instant>>,
    session_id: SessionId,
}

impl Drop for McpDrainGuard {
    fn drop(&mut self) {
        self.map.remove(&self.session_id);
    }
}

/// The delivery service — public entry point for the host.
pub struct DeliveryService {
    central: CentralDb,
    session_paths: Arc<dyn SessionRoot>,
    adapters: DashMap<ChannelType, Arc<dyn ChannelAdapter>>,
    actions: DashMap<String, Arc<dyn DeliveryActionHandler>>,
    inflight: DashMap<DeliveryKey, Instant>,
    retries: DashMap<DeliveryKey, RetryState>,
    /// Sessions whose persisted retry state (migration 029) has been loaded
    /// into `retries` this host lifetime. Guards the lazy one-shot priming
    /// in [`DeliveryService::prime_retry_cache`]; a fresh service instance
    /// (i.e. a host restart) starts empty and re-primes from the rows.
    retries_primed: DashSet<SessionId>,
    /// Per-session count of pending outbound rows currently deferred because
    /// their channel has no live adapter (M21 S5). Refreshed on every
    /// processing pass of the session — set while such rows are waiting
    /// (below the [`NO_ADAPTER_MAX_AGE_HOURS`] ceiling), cleared once they
    /// deliver or dead-letter — so the aggregate read by
    /// [`DeliveryService::no_adapter_backlog`] is a rolling snapshot of the
    /// no-adapter backlog.
    no_adapter_backlog: DashMap<SessionId, usize>,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    /// When `true`, a failed `install_packages` / `add_mcp_server`
    /// apply is surfaced as a `DeliveryError::SystemAction` so the
    /// outer loop records the row as failed (and the existing retry
    /// path can have another go) rather than recording the failure
    /// in-line. Initialised from `COPPERCLAW_SELFMOD_HARD_FAIL` at
    /// construct time; an `AtomicBool` so tests can flip it without
    /// touching the process env. Default off.
    selfmod_hard_fail: AtomicBool,
    /// Per-root-session locks serialising `TodoList` delivery across a
    /// whole `create_agent` family. The parent and its sibling agents all
    /// render into ONE pinned plan card (rolled up); two family members
    /// emitting concurrently would otherwise both "first-emit pin" and
    /// produce duplicate pins. Keyed by the family ROOT session id.
    todo_locks: DashMap<SessionId, Arc<tokio::sync::Mutex<()>>>,
    /// In-memory cache of the rolled-up plan's pinned-message external id,
    /// keyed by family ROOT session id. Makes the single-anchor robust to
    /// emit ordering (a child rendering before the parent has pinned would
    /// otherwise miss the root's DB records and pin a second card). Falls
    /// back to the root's persisted `delivered` records on a cache miss
    /// (e.g. after a host restart); cleared when the plan fully completes
    /// so a later fresh plan re-pins.
    todo_anchors: DashMap<SessionId, String>,
    /// Per-session in-flight guard for host-proxied external MCP tool calls.
    /// A session present here has a drain task running; the active loop skips
    /// re-spawning one so a slow external call is never executed twice. `Arc`
    /// so the detached drain task can clear its own entry on completion. Keyed
    /// by session id (the runner has at most one external call outstanding per
    /// session — it blocks on each `tool_result` before the model can call again).
    mcp_drain_inflight: Arc<DashMap<SessionId, Instant>>,
    /// Host-side broker for the reserved `__preview` MCP server (M17 session
    /// preview proxy). Set once at boot via [`DeliveryService::set_preview_broker`]
    /// after the service is wrapped in an `Arc` (the preview manager needs the
    /// container runtime, which is resolved later in boot). `None` (unset) means
    /// this host has no preview support — a `__preview` request is answered with
    /// a clear `is_error` rather than being left to hang the runner's poll.
    preview_broker: std::sync::OnceLock<Arc<dyn PreviewBroker>>,
    /// Host-side broker for the M19 A3 public-tunnel verb (`make_preview_public`),
    /// relayed through the SAME reserved `__preview` server. Set once at boot via
    /// [`DeliveryService::set_tunnel_broker`]. `None` (unset — this host has no
    /// tunnel support wired, e.g. no container runtime) answers a
    /// `make_preview_public` relay with a clear `is_error` rather than hanging.
    /// The exposure it drives stays approval-gated end to end.
    tunnel_broker: std::sync::OnceLock<Arc<dyn PublicTunnelBroker>>,
    /// Per-agent-group data root (`COPPERCLAW_GROUPS_DIR`). When set, an
    /// approved `save_skill` (M19 A4) writes into
    /// `<groups_dir>/<agent_group_id>/skills/<name>/SKILL.md`, which the next
    /// container spawn discovers. Set once at boot via
    /// [`DeliveryService::set_groups_dir`]; `None` means this host has no
    /// per-group skills root, so a `save_skill` request is refused with a
    /// clear self-mod failure rather than silently dropped.
    groups_dir: std::sync::OnceLock<PathBuf>,
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
            retries_primed: DashSet::new(),
            no_adapter_backlog: DashMap::new(),
            dispatcher,
            selfmod_hard_fail: AtomicBool::new(selfmod_hard_fail_from_env()),
            todo_locks: DashMap::new(),
            todo_anchors: DashMap::new(),
            mcp_drain_inflight: Arc::new(DashMap::new()),
            preview_broker: std::sync::OnceLock::new(),
            tunnel_broker: std::sync::OnceLock::new(),
            groups_dir: std::sync::OnceLock::new(),
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
            retries_primed: DashSet::new(),
            no_adapter_backlog: DashMap::new(),
            dispatcher,
            selfmod_hard_fail: AtomicBool::new(selfmod_hard_fail_from_env()),
            todo_locks: DashMap::new(),
            todo_anchors: DashMap::new(),
            mcp_drain_inflight: Arc::new(DashMap::new()),
            preview_broker: std::sync::OnceLock::new(),
            tunnel_broker: std::sync::OnceLock::new(),
            groups_dir: std::sync::OnceLock::new(),
        })
    }

    /// Reusable dispatcher handle suitable to hand to modules.
    pub fn dispatcher(&self) -> Arc<dyn DeliveryDispatcher> {
        Arc::clone(&self.dispatcher)
    }

    /// Wire the host-side preview broker (M17). Called once at boot after the
    /// service is wrapped in an `Arc` (the preview manager depends on the
    /// container runtime, resolved later in boot). Returns whether the broker
    /// was set (a second call is a no-op — the `OnceLock` keeps the first).
    pub fn set_preview_broker(&self, broker: Arc<dyn PreviewBroker>) -> bool {
        self.preview_broker.set(broker).is_ok()
    }

    /// Wire the host-side public-tunnel broker (M19 A3). Called once at boot
    /// after the preview manager + tunnel broker are constructed. Returns
    /// whether the broker was set (a second call is a no-op — the `OnceLock`
    /// keeps the first).
    pub fn set_tunnel_broker(&self, broker: Arc<dyn PublicTunnelBroker>) -> bool {
        self.tunnel_broker.set(broker).is_ok()
    }

    /// Wire the per-agent-group data root (`COPPERCLAW_GROUPS_DIR`) used by an
    /// approved `save_skill` (M19 A4) to place the skill under
    /// `<groups_dir>/<agent_group_id>/skills`. Called once at boot; a second
    /// call is a no-op (the `OnceLock` keeps the first). Returns whether it was
    /// set.
    pub fn set_groups_dir(&self, dir: PathBuf) -> bool {
        self.groups_dir.set(dir).is_ok()
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

    /// Number of pending outbound rows currently deferred because their
    /// channel has no live adapter, aggregated across sessions (M21 S5).
    ///
    /// Rows below the [`NO_ADAPTER_MAX_AGE_HOURS`] ceiling wait here for the
    /// channel to come up; rows past it are dead-lettered with reason
    /// [`NO_ADAPTER_DROP_REASON`]. A persistently non-zero value means a
    /// channel is unconfigured (or was removed) while sessions still address
    /// outbound at it.
    ///
    /// O1/S1 handoff: the S1 admin-socket status handler (lane H) is the
    /// intended surface for this count — it reads this method and exposes it
    /// so `cclaw doctor` (M21 O1) can flag a no-adapter backlog. Until S1
    /// lands, this method is the only read side.
    pub fn no_adapter_backlog(&self) -> usize {
        self.no_adapter_backlog.iter().map(|e| *e.value()).sum()
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

        // Prime the in-memory retry cache from the persisted `tries` /
        // `not_before` columns (M21 S3) on the first poll of this session
        // this host lifetime — attempt budgets and backoff windows survive
        // a restart instead of resetting to zero.
        self.prime_retry_cache(sess, &outbound_pool, &delivered_ids)?;

        let now = Instant::now();
        // Rows deferred this pass because their channel has no live adapter
        // (M21 S5) — becomes this session's slice of the no-adapter backlog
        // gauge after the loop.
        let mut no_adapter_pending: usize = 0;
        for row in rows {
            if delivered_ids.contains(&row.id) {
                continue;
            }
            let key = DeliveryKey::new(sess.id, row.id);

            // Backoff guard. Checked before we claim the in-flight slot so a
            // row still inside its retry window doesn't leave a stale claim.
            if let Some(not_before) = self.retries.get(&key).map(|r| r.not_before) {
                if now < not_before {
                    report.deferred += 1;
                    continue;
                }
            }

            // Re-entry guard — claim this (session, row) for the current pass.
            //
            // ROOT CAUSE of the `delivered.message_out_id` UNIQUE violations:
            // the active loop (1s) and the sweep loop (60s) share ONE
            // `DeliveryService`, so both run passes over the same running
            // session and can process the same row concurrently. The old guard
            // was a non-atomic get-then-insert: two passes on separate runtime
            // worker threads could both observe an empty slot and both send +
            // record the row, and the second `delivered::insert` blew up the
            // UNIQUE constraint (which then poisoned the whole pass). Claiming
            // the slot through the `entry` API holds the shard lock across the
            // read-and-insert, so at most one pass sends the row at a time.
            //
            // A stale claim (older than the ceiling — e.g. a prior pass that
            // panicked before clearing it) is reclaimed rather than blocking
            // the row forever. This closes the *concurrent* window; the
            // *sequential-stale* window (pass A finishes and clears the slot
            // before pass B, working from a snapshot taken before A recorded,
            // reaches the row) is covered by `delivered::insert` being
            // idempotent — a duplicate record is a no-op, never a poison.
            {
                use dashmap::mapref::entry::Entry;
                let claimed = match self.inflight.entry(key) {
                    Entry::Occupied(mut occ) => {
                        if now.duration_since(*occ.get())
                            < Duration::from_millis(ABSOLUTE_CEILING_MS)
                        {
                            false
                        } else {
                            occ.insert(now);
                            true
                        }
                    }
                    Entry::Vacant(vac) => {
                        vac.insert(now);
                        true
                    }
                };
                if !claimed {
                    continue;
                }
            }

            // Persisted-exhaustion guard (M21 S3). A row whose PERSISTED
            // attempt counter already reached the budget in a prior host
            // lifetime (the host died between the final bump and the
            // `failed` record) is dead-lettered now, without burning
            // another attempt — this is what makes exhaustion dead-letter
            // exactly once across restarts. Within one lifetime this never
            // fires: `bump_retry` returning `Fail` records the failure and
            // removes the entry in the same pass, under the same claim.
            if self
                .retries
                .get(&key)
                .is_some_and(|r| r.tries >= MAX_DELIVERY_ATTEMPTS)
            {
                self.inflight.remove(&key);
                self.record_exhausted_row(
                    sess,
                    &row,
                    &key,
                    &inbound_pool,
                    "retry budget exhausted before a host restart",
                )?;
                report.failed += 1;
                warn!(?row.id, "persisted retry budget exhausted, marking failed");
                continue;
            }

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
                    let outcome = self.bump_retry(&key, err.retry_after_secs(), &outbound_pool);
                    match outcome {
                        DeferOutcome::Defer => {
                            report.deferred += 1;
                            debug!(?err, ?row.id, "deferring retryable delivery failure");
                        }
                        DeferOutcome::Fail => {
                            self.record_exhausted_row(
                                sess,
                                &row,
                                &key,
                                &inbound_pool,
                                &err.to_string(),
                            )?;
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
                    copperclaw_metrics::inc_delivery_failed(&channel_label);
                    warn!(reason, ?row.id, "system action failed");
                }
                Err(DeliveryError::NoAdapter(ct)) => {
                    // No live adapter for the row's channel. Below the age
                    // ceiling the row stays pending — the adapter may simply
                    // not have been wired yet this boot. Past it the row is
                    // dead-lettered (M21 S5) so a permanently-unconfigured or
                    // removed channel can't accumulate unbounded pending
                    // outbound that nothing drains and nothing reports.
                    if no_adapter_expired(row.timestamp, chrono::Utc::now()) {
                        self.record_no_adapter_expired(
                            sess,
                            &row,
                            &key,
                            &ct,
                            routing.as_ref(),
                            &inbound_pool,
                        )?;
                        report.failed += 1;
                        warn!(
                            channel = %ct,
                            ?row.id,
                            age_ceiling_hours = NO_ADAPTER_MAX_AGE_HOURS,
                            "no adapter past age ceiling; dead-lettering row"
                        );
                    } else {
                        no_adapter_pending += 1;
                        report.deferred += 1;
                        warn!(channel = %ct, ?row.id, "no adapter; leaving row pending");
                    }
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

        // Refresh this session's slice of the no-adapter backlog gauge
        // (M21 S5): set while rows wait on a missing adapter, cleared once
        // they deliver or dead-letter, so `no_adapter_backlog()` stays a
        // rolling snapshot rather than a monotonic counter.
        if no_adapter_pending > 0 {
            self.no_adapter_backlog.insert(sess.id, no_adapter_pending);
        } else {
            self.no_adapter_backlog.remove(&sess.id);
        }

        // Host-proxied external MCP tool calls: drain any requests the runner
        // wrote to outbound.db, execute them against the configured external
        // server (through the per-server filter), and write the result back to
        // inbound.db for the runner's blocking poll. Best-effort — an external
        // MCP hiccup must never poison the message-delivery pass.
        if let Err(err) = self.drain_mcp_calls(sess, &outbound_pool, &inbound_pool) {
            warn!(
                ?err,
                session = %sess.id.as_uuid(),
                "external MCP call drain failed"
            );
        }

        Ok(report)
    }

    /// Execute the external MCP tool-call requests the runner left in
    /// `outbound.db::mcp_call_requests` and write each result back to
    /// `inbound.db::mcp_call_responses`, then GC responses the runner has
    /// already consumed. Returns the number of calls executed this pass.
    ///
    /// The fast, synchronous part runs on the delivery tick: snapshot the
    /// pending requests and GC consumed responses. The external calls
    /// themselves run in a **detached task** off the delivery critical path —
    /// the active loop processes sessions sequentially, so awaiting a slow or
    /// hung external server here would head-of-line block message delivery for
    /// every other session. A per-session in-flight guard
    /// ([`Self::mcp_drain_inflight`]) keeps a slow call from being executed
    /// twice across ticks; the runner only ever has one external call
    /// outstanding per session, so session granularity is sufficient.
    fn drain_mcp_calls(
        &self,
        sess: &Session,
        outbound_pool: &SessionPool,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        // Snapshot outstanding requests + GC consumed responses — both fast,
        // local DB ops; release the connections immediately.
        let (requests, live_ids) = {
            let out = outbound_pool.connect()?;
            (
                mcp_calls::list_requests(&out)?,
                mcp_calls::request_ids(&out)?,
            )
        };
        {
            let inb = inbound_pool.connect()?;
            mcp_calls::gc_orphan_responses(&inb, &live_ids)?;
        }
        if requests.is_empty() {
            return Ok(());
        }

        // One drain task per session at a time. A slow call already in flight
        // owns the guard; skip until it finishes (and writes its response).
        if self.mcp_drain_inflight.contains_key(&sess.id) {
            return Ok(());
        }

        let answered = {
            let inb = inbound_pool.connect()?;
            mcp_calls::response_ids(&inb)?
        };
        let todo: Vec<mcp_calls::McpCallRequest> = requests
            .into_iter()
            .filter(|r| !answered.contains(&r.request_id))
            .collect();
        if todo.is_empty() {
            return Ok(());
        }

        // The group's external-server config, read once for the whole batch.
        let servers = container_configs::get_mcp_servers(&self.central, sess.agent_group_id)
            .unwrap_or(serde_json::Value::Null);

        // The reserved `__preview` server (M17) routes to the host-side preview
        // broker instead of an external MCP server. Capture a clone for the
        // detached task (None => this host has no preview support).
        let preview_broker = self.preview_broker.get().map(Arc::clone);
        // The M19 A3 `make_preview_public` verb rides the same `__preview` relay
        // but routes to the tunnel broker instead of the preview broker.
        let tunnel_broker = self.tunnel_broker.get().map(Arc::clone);
        let preview_session = SessionInfoLite::new(sess.id, sess.agent_group_id);
        // M21 F4: the session id scopes the external-MCP connection cache —
        // connections are reused across this session's calls, never shared
        // across sessions.
        let mcp_scope = sess.id.as_uuid().to_string();

        // Claim the guard and hand everything to a detached task. The guard is
        // cleared on drop (including panic), so a wedged task can't permanently
        // block the session's future drains.
        self.mcp_drain_inflight.insert(sess.id, Instant::now());
        let guard = McpDrainGuard {
            map: Arc::clone(&self.mcp_drain_inflight),
            session_id: sess.id,
        };
        let inbound_pool = inbound_pool.clone();
        tokio::spawn(async move {
            let _guard = guard; // cleared on drop
            for req in todo {
                let resp = if req.server == PREVIEW_SERVER {
                    Self::execute_preview_call(
                        preview_broker.as_ref(),
                        tunnel_broker.as_ref(),
                        preview_session,
                        &req,
                    )
                    .await
                } else {
                    Self::execute_mcp_call_bounded(&servers, &req, &mcp_scope).await
                };
                match inbound_pool.connect() {
                    Ok(inb) => {
                        if let Err(err) = mcp_calls::insert_response(&inb, &resp) {
                            warn!(?err, request_id = %req.request_id, "could not write external MCP response");
                        }
                    }
                    Err(err) => {
                        warn!(
                            ?err,
                            "could not open inbound.db to write external MCP response"
                        );
                    }
                }
            }
        });
        Ok(())
    }

    /// Service a `__preview` relay request against the host-side preview broker
    /// (M17). Every failure mode becomes an `is_error` response — the runner's
    /// blocking poll is never left unanswered.
    ///
    /// A `None` broker (this host has no preview support — no container runtime,
    /// or the feature not wired) answers with a clear `is_error` rather than
    /// hanging. An unknown reserved tool name is likewise an `is_error`.
    async fn execute_preview_call(
        broker: Option<&Arc<dyn PreviewBroker>>,
        tunnel: Option<&Arc<dyn PublicTunnelBroker>>,
        session: SessionInfoLite,
        req: &mcp_calls::McpCallRequest,
    ) -> mcp_calls::McpCallResponse {
        let err = |msg: String| mcp_calls::McpCallResponse {
            request_id: req.request_id.clone(),
            is_error: true,
            result: msg,
        };
        // M19 A3: `make_preview_public` routes to the tunnel broker (its own
        // opt-in + approval gate), NOT the preview broker. Handled first so it
        // does not depend on the preview-broker presence check below.
        if req.tool.as_str() == "make_preview_public" {
            let Some(port) = parse_preview_port(&req.input) else {
                return err(
                    "`make_preview_public` requires an integer `port` between 1 and 65535 (the \
                     SAME container port you passed to `expose_preview`)."
                        .to_string(),
                );
            };
            let Some(tunnel) = tunnel else {
                return err(
                    "Public tunnels are not available on this host (no tunnel support wired). \
                     The operator must run copperclaw on a Docker host with public tunnels \
                     enabled to use `make_preview_public`."
                        .to_string(),
                );
            };
            return match tunnel.make_public(session, port).await {
                // A live public URL, or a pending-approval note: both are
                // successful (non-error) tool results the agent acts on.
                PublicTunnelReply::Exposed { public_url, note } => mcp_calls::McpCallResponse {
                    request_id: req.request_id.clone(),
                    is_error: false,
                    result: format!("{public_url}\n{note}"),
                },
                PublicTunnelReply::Pending { note } => mcp_calls::McpCallResponse {
                    request_id: req.request_id.clone(),
                    is_error: false,
                    result: note,
                },
                PublicTunnelReply::Error(msg) => err(msg),
            };
        }
        let Some(broker) = broker else {
            return err(
                "Preview is not available on this host (no container runtime / preview support). \
                 The operator must run copperclaw on a Docker host to use `expose_preview`."
                    .to_string(),
            );
        };
        match req.tool.as_str() {
            "expose_preview" => {
                let Some(port) = parse_preview_port(&req.input) else {
                    return err(
                        "`expose_preview` requires an integer `port` between 1 and 65535 (the \
                         port your server listens on INSIDE the container)."
                            .to_string(),
                    );
                };
                let name = req
                    .input
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                match broker.expose(&session, port, name).await {
                    Ok(exposed) => mcp_calls::McpCallResponse {
                        request_id: req.request_id.clone(),
                        is_error: false,
                        result: format!("{}\n{}", exposed.url, exposed.note),
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            "close_preview" => {
                let Some(port) = parse_preview_port(&req.input) else {
                    return err(
                        "`close_preview` requires an integer `port` between 1 and 65535."
                            .to_string(),
                    );
                };
                match broker.close(session.session_id, port).await {
                    Ok(()) => mcp_calls::McpCallResponse {
                        request_id: req.request_id.clone(),
                        is_error: false,
                        result: format!("Preview on port {port} closed."),
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            other => err(format!(
                "Unknown preview action `{other}` (expected `expose_preview`, `close_preview`, or `make_preview_public`)."
            )),
        }
    }

    /// [`Self::execute_mcp_call`] wrapped in a host-side deadline so a hung
    /// external server can't keep the drain task (and its session guard) alive
    /// indefinitely. The deadline is shorter than the runner's own poll
    /// deadline so the runner still receives the error response in time.
    async fn execute_mcp_call_bounded(
        servers: &serde_json::Value,
        req: &mcp_calls::McpCallRequest,
        session_scope: &str,
    ) -> mcp_calls::McpCallResponse {
        let fut = Self::execute_mcp_call(servers, req, session_scope);
        match tokio::time::timeout(Duration::from_secs(HOST_MCP_CALL_DEADLINE_SECS), fut).await {
            Ok(resp) => resp,
            Err(_) => mcp_calls::McpCallResponse {
                request_id: req.request_id.clone(),
                is_error: true,
                result: format!(
                    "External MCP tool `{}` (server `{}`) did not respond within {HOST_MCP_CALL_DEADLINE_SECS}s.",
                    req.tool, req.server
                ),
            },
        }
    }

    /// Execute one external MCP tool-call request against the configured
    /// server, through its per-server filter, and render the result into the
    /// host-written response row. Every failure mode (missing server, connect
    /// failure, filter-denied, remote error) becomes an `is_error` response —
    /// the runner's blocking poll is never left unanswered.
    ///
    /// Connections are reused across a session's sequential calls (M21 F4):
    /// `session_scope` keys `copperclaw-mcp`'s per-session connection cache,
    /// which idle-reaps and transparently retries a dead cached connection
    /// once on a fresh one before erroring.
    async fn execute_mcp_call(
        servers: &serde_json::Value,
        req: &mcp_calls::McpCallRequest,
        session_scope: &str,
    ) -> mcp_calls::McpCallResponse {
        let Some(entry) = servers.get(&req.server) else {
            return mcp_calls::McpCallResponse {
                request_id: req.request_id.clone(),
                is_error: true,
                result: format!(
                    "External MCP server `{}` is not configured for this group.",
                    req.server
                ),
            };
        };
        match copperclaw_mcp::call_external_tool_cached(
            session_scope,
            entry,
            &req.tool,
            req.input.clone(),
        )
        .await
        {
            Ok(value) => {
                let is_error = value
                    .get("is_error")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                mcp_calls::McpCallResponse {
                    request_id: req.request_id.clone(),
                    is_error,
                    result: render_mcp_content(value.get("content")),
                }
            }
            Err(err) => mcp_calls::McpCallResponse {
                request_id: req.request_id.clone(),
                is_error: true,
                result: format!(
                    "External MCP tool `{}` (server `{}`) failed: {err}",
                    req.tool, req.server
                ),
            },
        }
    }

    /// Pluck a system action handler invocation off the parsed row, if any.
    async fn process_row(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        routing: Option<&copperclaw_types::routing::SessionRouting>,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let target = Self::resolve_target(row, routing).ok_or(DeliveryError::NoRoute(sess.id))?;

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
                self.dispatch_chat(sess.id, row, &target, inbound_pool)
                    .await?;
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
                self.dispatch_breadcrumb(row, &target, inbound_pool, None)
                    .await?;
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
                self.dispatch_todo_list(row, &target, inbound_pool, sess)
                    .await?;
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
        // `save_skill` (M19 A4): the runner emits this when the agent calls the
        // `save_skill` tool. Unlike `install_packages` / `add_mcp_server`, which
        // apply immediately, this is APPROVAL-GATED — we raise a pending
        // approval + dispatch a card here and only WRITE the SKILL.md when an
        // operator approves (the `save_skill` apply arm in the host's approvals
        // handler). Secure-by-default: nothing lands on disk without approval.
        if action.name == "save_skill" {
            self.raise_save_skill_approval(sess, row, target, inbound_pool, &action.payload)?;
            return Ok(());
        }
        // `task_grant` (M22 A1): the runner emits this alongside a `schedule`
        // create when the agent's `schedule_task` call carried a `grant`. Like
        // `save_skill` it is APPROVAL-GATED — we resolve the concrete task id
        // (the schedule create row, processed just before this one, already
        // created the task), raise a pending approval + card, and persist the
        // `task_grants` row only when an operator approves (the `task_grant`
        // apply arm in the host's approvals handler). Nothing is authorized
        // without approval.
        if action.name == "task_grant" {
            self.raise_task_grant_approval(sess, row, target, inbound_pool, &action.payload)?;
            return Ok(());
        }
        // `goal` (M22 A3): the runner emits this when the agent calls
        // `create_goal` / `update_goal`. A goal is internal tracking state — it
        // authorizes nothing on its own — so unlike `save_skill` / `task_grant`
        // it applies IMMEDIATELY against the central `goals` table (mirroring
        // `install_packages`), not via an approval card. Any external ACTION a
        // goal check-in wake later takes stays gated by A2's grant machinery.
        if action.name == "goal" {
            let apply = apply_goal(&self.central, sess, &action.payload);
            self.finish_self_mod("goal", sess, row, inbound_pool, apply)?;
            return Ok(());
        }
        // `condition` (M22 A4): the runner emits this when the agent calls
        // `register_condition` / `set_condition_flag`. Like `goal` it is
        // internal tracking state — it authorizes nothing on its own (any
        // autonomous action a condition-driven wake later takes stays gated by
        // A2's grant machinery at fire time) — so it applies IMMEDIATELY against
        // the central `conditions` / `condition_flags` tables, not via an
        // approval card.
        if action.name == "condition" {
            let apply = apply_condition(&self.central, sess, &action.payload);
            self.finish_self_mod("condition", sess, row, inbound_pool, apply)?;
            return Ok(());
        }
        // `grant_consume` (M22 A2H): the runner emits this when a GRANTED
        // autonomous action actually fires (`charge_grant_fire_once` in the
        // runner's `tool_dispatch`). It is pure internal accounting — it
        // authorizes nothing, it only DEBITS an already-approved grant — so it
        // applies IMMEDIATELY (like `goal` / `condition`, NOT approval-gated),
        // decrementing the central `task_grants` row via `consume_fire` (+
        // `consume_tokens` when the payload carries a token count). This is what
        // makes `max_fires` / token budgets enforceable ACROSS fires: without
        // it the runner's per-turn/in-snapshot bounds hold, but the central
        // grant never depletes, so `effective_grant` (hence the next spawn's
        // grant.json) would never read the grant exhausted.
        if action.name == "grant_consume" {
            let apply = apply_grant_consume(&self.central, &action.payload);
            self.finish_self_mod("grant_consume", sess, row, inbound_pool, apply)?;
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
            self.handle_update_breadcrumb(row, target, inbound_pool, &action.payload, sess)
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
        // Capture what we need after the handler consumes the action (M18 G1:
        // the approval-card action carries the `approval_id` so we can persist
        // the delivered card's platform message id into `pending_approvals`).
        let action_name = action.name.clone();
        let approval_id_for_card = (action_name == "approval_card")
            .then(|| {
                action
                    .payload
                    .get("approval_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
                    .map(copperclaw_types::ApprovalId)
            })
            .flatten();
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
            let platform_message_id = deliver_action_message(
                adapter.as_ref(),
                &platform_id,
                dispatch_target.thread_id.as_deref(),
                &msg,
            )
            .await?;
            let in_conn = inbound_pool.connect()?;
            delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
            // M18 G1: remember where the approval card landed so the in-chat
            // approvals interceptor can later edit it to "Approved by <name>".
            // Best-effort — a failure here must not fail the delivery.
            if let (Some(approval_id), Some(pmid)) =
                (approval_id_for_card, platform_message_id.as_deref())
            {
                if let Err(err) = copperclaw_db::tables::pending_approvals::set_platform_message_id(
                    &self.central,
                    approval_id,
                    pmid,
                ) {
                    warn!(
                        ?err,
                        "approvals: could not persist card platform_message_id"
                    );
                }
            }
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
            warn!(
                action = action_name,
                "edit/reaction payload missing seq; falling back"
            );
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
                    .edit_message(
                        &platform_id,
                        target.thread_id.as_deref(),
                        &external_id,
                        text,
                    )
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

        // M19 U3: meter outbound emoji reactions centrally (covers every
        // adapter). Typing is metered on the dispatcher's set_typing path.
        let is_reaction = action_name == "reaction";
        match call_result {
            Ok(()) => {
                if is_reaction {
                    copperclaw_metrics::inc_adapter_reaction(channel_type.as_str(), "ok");
                }
                Ok(ActionAdapterOutcome::Done)
            }
            Err(AdapterError::Unsupported(reason)) => {
                if is_reaction {
                    copperclaw_metrics::inc_adapter_reaction(channel_type.as_str(), "unsupported");
                }
                info!(
                    action = action_name,
                    reason, "adapter unsupported; falling back"
                );
                Ok(ActionAdapterOutcome::FallThrough)
            }
            Err(other) => {
                if is_reaction {
                    copperclaw_metrics::inc_adapter_reaction(channel_type.as_str(), "error");
                }
                Err(DeliveryError::Adapter(other))
            }
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
                    return Err(DeliveryError::SystemAction(format!("{action}: {err}")));
                }
                record_self_mod_failure(sess, row, inbound_pool, action, &err)?;
                Ok(())
            }
        }
    }

    /// Raise an approval for an agent-authored `save_skill` request (M19 A4)
    /// and dispatch an approve/deny card to the originating channel.
    ///
    /// Nothing is written to disk here — the SKILL.md lands only when an
    /// operator approves (the host's `save_skill` approval apply arm). The
    /// pending row carries the validated skill body plus the host-computed
    /// destination + containment root, so the apply arm needs no extra config.
    /// Idempotent on `(agent_group, name)` via a stable `request_id`, so an
    /// agent retrying `save_skill` for the same name doesn't stack cards.
    ///
    /// When no per-group skills root is configured (`groups_dir` unset), the
    /// request is refused with a self-mod failure so the agent learns rather
    /// than the row being silently dropped.
    fn raise_save_skill_approval(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
        payload: &serde_json::Value,
    ) -> Result<(), DeliveryError> {
        let ag = sess.agent_group_id;
        let name = payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let content = payload
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        let Some(groups_dir) = self.groups_dir.get() else {
            record_save_skill_failure(
                sess,
                row,
                inbound_pool,
                "this host has no per-group skills directory configured \
                 (COPPERCLAW_GROUPS_DIR is unset), so save_skill cannot persist \
                 a skill; ask an operator to configure it",
            )?;
            return Ok(());
        };
        if name.is_empty() || content.is_empty() {
            record_save_skill_failure(
                sess,
                row,
                inbound_pool,
                "save_skill payload is missing `name` or `content`",
            )?;
            return Ok(());
        }

        let dest_dir = groups_dir.join(ag.as_uuid().to_string()).join("skills");
        let payload_out = serde_json::json!({
            "name": name,
            "content": content,
            "reason": reason,
            // Host-computed (trusted) destination + containment root; the apply
            // arm writes under `dest_dir` and refuses anything that canonically
            // escapes `allowed_root`.
            "dest_dir": dest_dir.to_string_lossy(),
            "allowed_root": groups_dir.to_string_lossy(),
        });

        let approval = match pending_approvals::upsert(
            &self.central,
            pending_approvals::UpsertPendingApproval {
                request_id: format!("save-skill:{}:{name}", ag.as_uuid()),
                action: "save_skill".to_string(),
                payload: payload_out,
                agent_group_id: Some(ag),
                channel_type: target.channel_type.clone(),
                platform_id: target.platform_id.clone(),
                title: format!("Save skill: {name}"),
                ..Default::default()
            },
        ) {
            Ok(a) => a,
            Err(err) => {
                // A DB failure here is transient-ish; surface it as a self-mod
                // failure so the agent (and dropped-messages) see it.
                record_save_skill_failure(
                    sess,
                    row,
                    inbound_pool,
                    &format!("could not record the save_skill approval: {err}"),
                )?;
                return Ok(());
            }
        };

        // Best-effort card to the originating channel. The `approve:<id>` /
        // `deny:<id>` buttons route through the same G1 interceptor + DB path
        // the CLI `cclaw approvals approve <id>` uses, so the request is
        // actionable even on channels without buttons (operator CLI).
        if target.channel_type.is_some() && target.platform_id.is_some() {
            let approval_id = approval.approval_id.as_uuid().to_string();
            let body = if reason.trim().is_empty() {
                format!("The agent wants to save a reusable skill `{name}` for future sessions.")
            } else {
                format!(
                    "The agent wants to save a reusable skill `{name}` for future sessions.\n\nReason: {reason}"
                )
            };
            let card = OutboundMessage {
                kind: MessageKind::Card,
                content: serde_json::json!({
                    "card": {
                        "title": format!("Save skill: {name}"),
                        "body": body,
                        "buttons": [
                            { "label": "Save this skill", "value": format!("approve:{approval_id}"), "style": "primary" },
                            { "label": "Not now", "value": format!("deny:{approval_id}"), "style": "danger" },
                        ],
                    },
                }),
                files: vec![],
            };
            self.dispatcher.dispatch(target, &card);
        }

        copperclaw_metrics::inc_self_mod_succeeded("save_skill");
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, None, "ok")?;
        Ok(())
    }

    /// Raise an approval for an agent-authored task capability `grant` (M22 A1)
    /// and dispatch an approve/deny card to the originating channel.
    ///
    /// The grant references its task by name (`task_name`); the concrete task id
    /// is assigned host-side, so we resolve it HERE from the session's tasks
    /// (the `schedule` create row that created it was processed just before this
    /// row) and stamp it into the pending payload. Nothing is authorized here —
    /// the `task_grants` row lands only when an operator approves (the host's
    /// `task_grant` approval apply arm). Idempotent on `(session, task_name)` via
    /// a stable `request_id`, so a retry doesn't stack cards. If the task can't
    /// be resolved (e.g. its create failed), the request is refused with a
    /// self-mod failure so the agent learns rather than the row silently
    /// dropping.
    fn raise_task_grant_approval(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        target: &DispatchTarget,
        inbound_pool: &SessionPool,
        payload: &serde_json::Value,
    ) -> Result<(), DeliveryError> {
        let ag = sess.agent_group_id;
        let task_name = payload
            .get("task_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let capability_scope = payload
            .get("capability_scope")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let reason = payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();

        if task_name.is_empty() || capability_scope.trim().is_empty() {
            record_task_grant_failure(
                sess,
                row,
                inbound_pool,
                "task_grant payload is missing `task_name` or `capability_scope`",
            )?;
            return Ok(());
        }

        // Resolve the concrete task id from the just-created task.
        let task = match copperclaw_db::tables::tasks::latest_for_session_by_name(
            &self.central,
            sess.id,
            &task_name,
        ) {
            Ok(Some(t)) => t,
            Ok(None) => {
                record_task_grant_failure(
                    sess,
                    row,
                    inbound_pool,
                    &format!(
                        "could not find a task named `{task_name}` to attach the grant to; the \
                         task must be scheduled in the same call"
                    ),
                )?;
                return Ok(());
            }
            Err(err) => {
                record_task_grant_failure(
                    sess,
                    row,
                    inbound_pool,
                    &format!("could not resolve the task for the grant: {err}"),
                )?;
                return Ok(());
            }
        };

        // Host-trusted payload: the resolved task id plus the agent-proposed
        // (already tool-validated) scope + bounds. The apply arm inserts the
        // grant from exactly this.
        let payload_out = serde_json::json!({
            "task_id": task.id,
            "task_name": task_name,
            "capability_scope": capability_scope,
            "token_budget": payload.get("token_budget").cloned().unwrap_or(serde_json::Value::Null),
            "max_fires": payload.get("max_fires").cloned().unwrap_or(serde_json::Value::Null),
            "expires_at": payload.get("expires_at").cloned().unwrap_or(serde_json::Value::Null),
            "reason": reason,
        });

        let approval = match pending_approvals::upsert(
            &self.central,
            pending_approvals::UpsertPendingApproval {
                request_id: format!("task-grant:{}:{task_name}", task.id),
                action: "task_grant".to_string(),
                payload: payload_out,
                agent_group_id: Some(ag),
                session_id: Some(sess.id),
                channel_type: target.channel_type.clone(),
                platform_id: target.platform_id.clone(),
                title: format!("Authorize task `{task_name}`"),
                ..Default::default()
            },
        ) {
            Ok(a) => a,
            Err(err) => {
                record_task_grant_failure(
                    sess,
                    row,
                    inbound_pool,
                    &format!("could not record the task_grant approval: {err}"),
                )?;
                return Ok(());
            }
        };

        if target.channel_type.is_some() && target.platform_id.is_some() {
            let approval_id = approval.approval_id.as_uuid().to_string();
            let body = if reason.trim().is_empty() {
                format!(
                    "The agent wants to pre-authorize the scheduled task `{task_name}` to act \
                     autonomously within `{capability_scope}`."
                )
            } else {
                format!(
                    "The agent wants to pre-authorize the scheduled task `{task_name}` to act \
                     autonomously within `{capability_scope}`.\n\nReason: {reason}"
                )
            };
            let card = OutboundMessage {
                kind: MessageKind::Card,
                content: serde_json::json!({
                    "card": {
                        "title": format!("Authorize task `{task_name}`"),
                        "body": body,
                        "buttons": [
                            { "label": "Authorize", "value": format!("approve:{approval_id}"), "style": "primary" },
                            { "label": "Not now", "value": format!("deny:{approval_id}"), "style": "danger" },
                        ],
                    },
                }),
                files: vec![],
            };
            self.dispatcher.dispatch(target, &card);
        }

        copperclaw_metrics::inc_self_mod_succeeded("task_grant");
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, None, "ok")?;
        Ok(())
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
        let parts = split_chat_content_if_needed(
            &row.content,
            adapter.max_message_chars(),
            adapter.channel_type().as_str(),
        );

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
        let (start_index, mut first_platform_id) =
            self.retries.get(&key).map_or((0usize, None), |s| {
                (s.chunks_sent as usize, s.first_chunk_pid.clone())
            });

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
                    reason, "deliver_collapsible unsupported; falling back to summary text deliver"
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
                    reason, "deliver_card unsupported; falling back to text deliver"
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
    /// Row content shape (written by `RunnerToolCtx::emit_task_hud` via
    /// `insert_breadcrumb_row`):
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
                    reason, "deliver_breadcrumb unsupported; falling back to text deliver"
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
            // Stale anchor: the chip this edit targeted is gone. Re-post a
            // fresh chip instead of failing — same recovery as the todo card
            // (see `dispatch_todo_list` / `is_stale_edit_target`). Breadcrumbs
            // resolve their anchor from `delivered` each emit, so the fresh
            // row's record becomes the next lookup target; no cache to clear.
            Err(AdapterError::BadRequest(msg))
                if existing_message_id.is_some() && is_stale_edit_target(&msg) =>
            {
                warn!(
                    channel = adapter.channel_type().as_str(),
                    stale_anchor = existing_message_id.unwrap_or(""),
                    "breadcrumb edit target is gone; re-posting a fresh chip"
                );
                adapter
                    .deliver_breadcrumb(
                        &platform_id,
                        target.thread_id.as_deref(),
                        &breadcrumb,
                        None,
                    )
                    .await
                    .map_err(DeliveryError::Adapter)?
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
        // Validate the triggering row up front (corrupt row → fail fast
        // so the retry loop doesn't bang on it). The card itself is
        // rebuilt from the whole family below, so this is only a guard.
        if row.content.get("todo_list").is_none() {
            return Err(DeliveryError::SystemAction(
                "todo_list row missing content.todo_list".into(),
            ));
        }

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

        // Roll the whole `create_agent` family into ONE pinned card owned
        // by the family ROOT: each child's plan becomes a labeled, indented
        // section under the parent's, and a child never pins its own
        // message. Serialise per-root so two members emitting concurrently
        // don't both "first-emit pin" and produce duplicate pinned cards.
        let root = resolve_root(&self.central, sess);
        let lock = self
            .todo_locks
            .entry(root.id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let root_list = self.latest_todo_list(&root.agent_group_id, &root.id);
        let mut child_lists: Vec<(String, TodoList)> = Vec::new();
        for child in family_children(&self.central, root.id) {
            match self.latest_todo_list(&child.agent_group_id, &child.id) {
                Some(list) if !list.items.is_empty() => {
                    let name = agent_groups::get(&self.central, child.agent_group_id)
                        .map_or_else(|_| "sub-agent".to_string(), |g| g.name);
                    child_lists.push((name, list));
                }
                _ => {}
            }
        }
        let combined = build_combined(root_list, &child_lists);

        let in_conn = inbound_pool.connect()?;
        if combined.items.is_empty() {
            // Nothing to render yet — mark the row delivered (so the loop
            // doesn't retry) and leave any existing pin alone.
            delivered::insert(&in_conn, row.id, None, "ok")?;
            return Ok(());
        }

        // The single pinned anchor is keyed by the ROOT session. Prefer the
        // in-memory cache (robust to emit ordering across the family); fall
        // back to the root's persisted `delivered` records (e.g. after a
        // host restart, where the parent's pinned card survives).
        let prior_external_id = match self.todo_anchors.get(&root.id) {
            Some(id) => Some(id.clone()),
            None => lookup_prior_kind_external_id(
                &self
                    .session_paths
                    .outbound_pool(&root.agent_group_id, &root.id)?
                    .connect()?,
                &self
                    .session_paths
                    .inbound_pool(&root.agent_group_id, &root.id)?
                    .connect()?,
                MessageKind::TodoList,
                |_| true,
            )?,
        };

        // pin_hint:
        // - First emit (no prior chip yet) → true so the adapter pins.
        // - All-completed transition → true so the adapter can unpin.
        // - Otherwise → false (leave existing pin state alone).
        let pin_hint = prior_external_id.is_none() || combined.is_fully_completed();

        // M19 U1/U2: record the edit-in-place-vs-create intent for the pinned
        // rich surface (a prior anchor → edit; none → fresh create).
        copperclaw_metrics::inc_adapter_surface_write(
            channel_type.as_str(),
            if prior_external_id.is_some() {
                "edit"
            } else {
                "create"
            },
        );

        let platform_message_id = match adapter
            .deliver_todo_list(
                &platform_id,
                target.thread_id.as_deref(),
                &combined,
                prior_external_id.as_deref(),
                pin_hint,
            )
            .await
        {
            Ok(id) => id,
            Err(AdapterError::Unsupported(reason)) => {
                info!(
                    channel = adapter.channel_type().as_str(),
                    reason, "deliver_todo_list unsupported; falling back to text deliver"
                );
                let outbound = OutboundMessage {
                    kind: MessageKind::Chat,
                    content: serde_json::json!({ "text": combined.to_text_fallback() }),
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
            // Stale anchor: the card this edit targeted is gone (deleted, too
            // old to edit, or pinned before a host restart / long idle gap).
            // Don't fail the row forever — drop the dead anchor and re-post a
            // FRESH card (pin it) so the plan keeps updating. Without this, the
            // stale `todo_anchors` / `delivered` anchor is re-resolved on every
            // subsequent emit and every todo update fails indefinitely.
            Err(AdapterError::BadRequest(msg))
                if prior_external_id.is_some() && is_stale_edit_target(&msg) =>
            {
                warn!(
                    channel = adapter.channel_type().as_str(),
                    stale_anchor = prior_external_id.as_deref().unwrap_or(""),
                    "todo card edit target is gone; re-posting a fresh card"
                );
                self.todo_anchors.remove(&root.id);
                adapter
                    .deliver_todo_list(
                        &platform_id,
                        target.thread_id.as_deref(),
                        &combined,
                        None, // fresh post, no edit target
                        true, // pin the new card
                    )
                    .await
                    .map_err(DeliveryError::Adapter)?
            }
            Err(other) => return Err(DeliveryError::Adapter(other)),
        };
        // Track the family's single anchor for subsequent edits. Drop it
        // once the whole plan is done so a later fresh plan re-pins rather
        // than editing a stale (unpinned) card.
        match (&platform_message_id, combined.is_fully_completed()) {
            (Some(id), false) => {
                self.todo_anchors.insert(root.id, id.clone());
            }
            (_, true) => {
                self.todo_anchors.remove(&root.id);
            }
            (None, false) => {}
        }
        delivered::insert(&in_conn, row.id, platform_message_id.as_deref(), "ok")?;
        // M19 F4: a blocked-todo chip was actually rendered on this channel.
        if combined.blocked_count() > 0 {
            let has_reason = combined
                .items
                .iter()
                .any(|item| item.blocked_reason_text().is_some());
            copperclaw_metrics::inc_blocked_todo_render(channel_type.as_str(), has_reason);
        }
        Ok(())
    }

    /// Read a session's most-recent `TodoList` payload from its outbound
    /// DB (the canonical post-mutation list the runner emitted). `None`
    /// when the session has no todo list yet or its DB can't be opened.
    fn latest_todo_list(
        &self,
        agent_group_id: &AgentGroupId,
        session_id: &SessionId,
    ) -> Option<TodoList> {
        let conn = self
            .session_paths
            .outbound_pool(agent_group_id, session_id)
            .ok()?
            .connect()
            .ok()?;
        let mut stmt = conn
            .prepare("SELECT content FROM messages_out WHERE kind = ?1 ORDER BY seq DESC LIMIT 1")
            .ok()?;
        let content: String = stmt
            .query_row([MessageKind::TodoList.as_str()], |r| r.get(0))
            .ok()?;
        let value: serde_json::Value = serde_json::from_str(&content).ok()?;
        serde_json::from_value(value.get("todo_list")?.clone()).ok()
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
                    reason, "deliver_error unsupported; falling back to text deliver"
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
                    reason, "deliver_thinking unsupported; falling back to text deliver"
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
                    reason, "deliver_diff unsupported; falling back to text deliver"
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
            format!("delivery failed after exhausting the retry budget: {capped}")
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
    /// Payload shape (from `RunnerToolCtx::emit_task_hud` via
    /// `insert_update_breadcrumb_row`):
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

    fn bump_retry(
        &self,
        key: &DeliveryKey,
        retry_after_secs: Option<u64>,
        outbound_pool: &SessionPool,
    ) -> DeferOutcome {
        let now = Instant::now();
        let (tries, delay_ms, outcome) = {
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
                (entry.tries, None, DeferOutcome::Fail)
            } else {
                // Honour the adapter's `retry_after` hint when present (Telegram /
                // Slack / GitHub / Linear / Webex all parse it from `Retry-After`
                // or the platform's equivalent). Cap at the absolute ceiling so a
                // pathological hint can't park a row for hours. Fall back to the
                // fixed exponential schedule when no hint is given.
                let delay = match retry_after_secs {
                    Some(s) => s.saturating_mul(1_000).min(ABSOLUTE_CEILING_MS),
                    None => backoff_delay_ms(entry.tries),
                };
                entry.not_before = now + Duration::from_millis(delay);
                (entry.tries, Some(delay), DeferOutcome::Defer)
            }
        };
        // Write-through (M21 S3): mirror the counter and the wall-clock
        // backoff window onto the row (migration 029) so a host restart
        // resumes at the persisted count instead of resetting. Runs after
        // the entry guard is dropped so the DashMap shard lock is never
        // held across DB IO. Best-effort — the in-memory entry stays
        // authoritative for this lifetime, and a bookkeeping write failure
        // must not poison the delivery pass. The final bump (`Fail`)
        // persists the exhausted count with NO window: if the host dies
        // before the `failed` record lands, the persisted-exhaustion guard
        // in `process_session_once` dead-letters the row on the next boot
        // without burning another attempt.
        let wall_not_before = delay_ms.map(|ms| {
            chrono::Utc::now()
                + chrono::Duration::milliseconds(i64::try_from(ms).unwrap_or(i64::MAX))
        });
        let persisted = outbound_pool.connect().and_then(|conn| {
            messages_out::set_retry_state(&conn, key.msg_id, tries, wall_not_before)
                .map_err(DeliveryError::from)
        });
        if let Err(err) = persisted {
            warn!(?err, msg_id = ?key.msg_id, "could not persist delivery retry state");
        }
        outcome
    }

    /// Load persisted retry state (M21 S3, migration 029) into the in-memory
    /// `retries` cache — once per session per host lifetime, on the first
    /// poll. Wall-clock `not_before` windows are converted to monotonic
    /// deadlines: a window still in the future keeps its remaining span
    /// (capped at the absolute ceiling so a pathological persisted value
    /// can't park a row for hours); an elapsed window becomes retry-now.
    /// Rows already recorded in `delivered` are terminal and skipped, as is
    /// any key the cache already holds (same-lifetime state is fresher).
    fn prime_retry_cache(
        &self,
        sess: &Session,
        outbound_pool: &SessionPool,
        delivered_ids: &std::collections::HashSet<MessageId>,
    ) -> Result<(), DeliveryError> {
        if self.retries_primed.contains(&sess.id) {
            return Ok(());
        }
        let persisted = {
            let out_conn = outbound_pool.connect()?;
            messages_out::list_retry_state(&out_conn)?
        };
        let now_wall = chrono::Utc::now();
        let now = Instant::now();
        for state in persisted {
            if delivered_ids.contains(&state.id) {
                continue;
            }
            let key = DeliveryKey::new(sess.id, state.id);
            if let dashmap::mapref::entry::Entry::Vacant(vacant) = self.retries.entry(key) {
                let not_before = match state.not_before {
                    Some(t) if t > now_wall => {
                        let remaining = (t - now_wall)
                            .to_std()
                            .unwrap_or_default()
                            .min(Duration::from_millis(ABSOLUTE_CEILING_MS));
                        now + remaining
                    }
                    _ => now,
                };
                // M21 S3 (M1 rider): a persisted, non-zero attempt count is a
                // genuine resume across a host restart (a fresh row starts at
                // 0 and is not counted).
                if state.tries > 0 {
                    copperclaw_metrics::inc_delivery_retry_resumed();
                }
                vacant.insert(RetryState {
                    tries: state.tries,
                    not_before,
                    chunks_sent: 0,
                    first_chunk_pid: None,
                });
            }
        }
        // Marked only after a fully successful load so a transient DB error
        // above (propagated via `?`) retries the priming on the next pass.
        self.retries_primed.insert(sess.id);
        Ok(())
    }

    /// Dead-letter one outbound row that exhausted its retry budget: record
    /// `delivered{status="failed"}` (the load-bearing artefact operators
    /// read via `cclaw dropped-messages`), clear the in-memory retry entry,
    /// bump the failure metric, and best-effort emit the delivery-failure
    /// `ErrorCard` back at the originating channel. Shared by the in-lifetime
    /// exhaustion path (`bump_retry` returning `Fail`) and the M21 S3
    /// persisted-exhaustion guard that fires after a host restart.
    fn record_exhausted_row(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        key: &DeliveryKey,
        inbound_pool: &SessionPool,
        reason: &str,
    ) -> Result<(), DeliveryError> {
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, None, "failed")?;
        self.retries.remove(key);
        let channel_label = row
            .channel_type
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), |ct| ct.as_str().to_owned());
        copperclaw_metrics::inc_delivery_failed(&channel_label);
        // M21 S3 (M1 rider): dead-letter by reason (retry budget exhausted).
        copperclaw_metrics::inc_delivery_dead_letter(
            copperclaw_metrics::DEAD_LETTER_REASON_RETRY_EXHAUSTED,
        );
        // Best-effort: emit an Error-kind outbound row addressed back at
        // the originating channel so the next delivery pass renders it
        // visibly to the user. Swallow errors — an emit failure here can't
        // be allowed to poison the delivery loop, and the `failed` row
        // above is the load-bearing record.
        if let Err(emit_err) = self.emit_delivery_failure_error_card(sess, row, reason) {
            warn!(?emit_err, ?row.id, "could not emit retry-exhaustion ErrorCard");
        }
        Ok(())
    }

    /// Dead-letter one pending outbound row whose channel has had no live
    /// adapter for longer than [`NO_ADAPTER_MAX_AGE_HOURS`] (M21 S5): write
    /// a central `outbound_dropped_messages` row with reason
    /// [`NO_ADAPTER_DROP_REASON`] so the failure is visible in
    /// `cclaw dropped-messages outbound-list` and recoverable with
    /// `cclaw dropped-messages replay` once the channel is configured, then
    /// record `delivered{status="failed"}` so the row stops being polled.
    ///
    /// Deliberately NO user-facing `ErrorCard` here (contrast
    /// [`Self::record_exhausted_row`]): these rows by definition have no
    /// deliverable channel, and an Error row addressed back at the same dead
    /// channel would itself sit pending for the ceiling and then
    /// dead-letter — garbage begetting garbage.
    ///
    /// Ephemeral UI kinds (breadcrumb / `todo_list` / diff / error /
    /// thinking) are failed WITHOUT a central dead-letter row: the
    /// `outbound_dropped_messages` table only round-trips the replayable
    /// kinds (chat / task / webhook / system / agent / card — an
    /// out-of-vocabulary kind would poison `outbound-list` for every row),
    /// and replaying hours-stale UI chrome is meaningless. For those, the
    /// terminal `failed` record plus the log line is the whole story.
    fn record_no_adapter_expired(
        &self,
        sess: &Session,
        row: &MessageOutRow,
        key: &DeliveryKey,
        channel_type: &ChannelType,
        routing: Option<&copperclaw_types::routing::SessionRouting>,
        inbound_pool: &SessionPool,
    ) -> Result<(), DeliveryError> {
        let replayable = matches!(
            row.kind,
            MessageKind::Chat
                | MessageKind::Task
                | MessageKind::Webhook
                | MessageKind::System
                | MessageKind::Agent
                | MessageKind::Card
        );
        if replayable {
            // Same channel/platform fallback order as `resolve_target` so
            // the dead-letter row carries the destination a replay needs.
            let platform_id = row
                .platform_id
                .clone()
                .or_else(|| routing.and_then(|r| r.platform_id.clone()));
            let thread_id = row
                .thread_id
                .clone()
                .or_else(|| routing.and_then(|r| r.thread_id.clone()));
            let last_error = format!(
                "{NO_ADAPTER_DROP_REASON}: no live adapter for channel '{ct}' for over \
                 {NO_ADAPTER_MAX_AGE_HOURS}h; replay with `cclaw dropped-messages replay` \
                 once the channel is configured",
                ct = channel_type.as_str(),
            );
            // Dead-letter FIRST, terminal record second: if the `failed`
            // write below errors, the next pass repeats both (at worst a
            // duplicate dead-letter row an operator can see and delete),
            // whereas the reverse order could mark the row terminal and then
            // lose the only replayable copy.
            outbound_dropped_messages::insert(
                &self.central,
                outbound_dropped_messages::InsertOutboundDropped {
                    session_id: sess.id,
                    agent_group_id: sess.agent_group_id,
                    message_out_id: row.id,
                    channel_type: Some(channel_type.clone()),
                    platform_id,
                    thread_id,
                    kind: row.kind,
                    content: row.content.clone(),
                    last_error,
                },
            )?;
        }
        let in_conn = inbound_pool.connect()?;
        delivered::insert(&in_conn, row.id, None, "failed")?;
        self.retries.remove(key);
        copperclaw_metrics::inc_delivery_failed(channel_type.as_str());
        // M21 S5 (M1 rider): dead-letter by reason (no live adapter).
        copperclaw_metrics::inc_delivery_dead_letter(
            copperclaw_metrics::DEAD_LETTER_REASON_NO_ADAPTER,
        );
        Ok(())
    }

    /// Snapshot of sessions visible to the active loop (status=Active,
    /// container=Running).
    pub fn list_running_sessions(&self) -> Result<Vec<Session>, DeliveryError> {
        Ok(copperclaw_db::tables::sessions::list_running(
            &self.central,
        )?)
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
    channel_type: &str,
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
    let chunks = split_text_into_chunks(text, max, channel_type);
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

/// Greedy, fence-aware chunker honoring `max` chars per chunk. Migrated to
/// [`copperclaw_channels_core::markdown`] in M18/C5b (the shared renderer
/// absorbed C2's splitter + `fence.rs`); this thin delegate preserves the
/// call site and keeps `host-delivery`'s C2 splitter tests exercising the
/// migrated logic. See [`copperclaw_channels_core::markdown::split_into_chunks`]
/// for the cut-preference and fence close/reopen rules.
fn split_text_into_chunks(text: &str, max: usize, channel_type: &str) -> Vec<String> {
    copperclaw_channels_core::markdown::split_into_chunks(text, max, channel_type)
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
            match adapter.deliver(platform_id, thread_id, &fallback_msg).await {
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

/// Deliver a delivery-action's output message (M18 G1). `MessageKind::Card`
/// messages — the approval card, which carries `approve:<id>` / `deny:<id>`
/// buttons — render through the adapter's native `deliver_card` hook so the
/// buttons appear; an adapter that reports `Unsupported`, or a payload that
/// isn't a canonical card, degrades to a plain-text `deliver`. Every other
/// kind goes straight through [`call_adapter`] (unchanged behaviour, including
/// its formatting-bad-request fallback).
async fn deliver_action_message(
    adapter: &dyn ChannelAdapter,
    platform_id: &str,
    thread_id: Option<&str>,
    message: &OutboundMessage,
) -> Result<Option<String>, DeliveryError> {
    if message.kind == MessageKind::Card {
        if let Some(card_val) = message.content.get("card") {
            if let Ok(card) = serde_json::from_value::<Card>(card_val.clone()) {
                match adapter
                    .deliver_card(platform_id, thread_id, &card, None)
                    .await
                {
                    Ok(id) => return Ok(id),
                    Err(AdapterError::Unsupported(_)) => {
                        let fallback = OutboundMessage {
                            kind: MessageKind::Chat,
                            content: serde_json::json!({ "text": card.to_text_fallback() }),
                            files: vec![],
                        };
                        return call_adapter(adapter, platform_id, thread_id, &fallback).await;
                    }
                    Err(other) => return Err(DeliveryError::Adapter(other)),
                }
            }
        }
    }
    call_adapter(adapter, platform_id, thread_id, message).await
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

/// Return `true` when a `BadRequest` indicates the message an edit targeted is
/// **gone** — deleted, too old for the platform to edit, or never existed
/// against this chat. This is distinct from a formatting rejection
/// ([`is_formatting_bad_request`]): the request shape was fine, the *anchor*
/// is stale. The delivery loop uses this to recover an in-place card update
/// (a pinned todo/plan card whose anchor outlived a host restart or a long
/// idle gap) by re-posting a FRESH card instead of failing the row forever.
///
/// Patterns covered (case-insensitive), across the channels that support
/// in-place edits:
/// - Telegram: `message to edit not found`, `message can't be edited`,
///   `message_id_invalid`.
/// - Slack: `message_not_found`, `cant_update_message`.
/// - Discord: `unknown message`.
pub(crate) fn is_stale_edit_target(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("message to edit not found")
        || m.contains("message can't be edited")
        || m.contains("message cant be edited")
        || m.contains("message_id_invalid")
        || m.contains("message_not_found")
        || m.contains("cant_update_message")
        || m.contains("unknown message")
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
fn message_id_for_seq(out_conn: &Connection, seq: i64) -> Result<Option<MessageId>, DeliveryError> {
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
    let uuid = uuid::Uuid::parse_str(&id_str)
        .map_err(|e| DeliveryError::SystemAction(format!("invalid outbound row uuid: {e}")))?;
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
    lookup_prior_kind_external_id(out_conn, in_conn, MessageKind::Breadcrumb, |content| {
        content
            .get("breadcrumb")
            .and_then(|v| v.get("tool_name"))
            .and_then(serde_json::Value::as_str)
            == Some(tool_name)
    })
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
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
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
        .query_row([message_out_id.as_uuid().to_string()], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()
        .map_err(copperclaw_db::DbError::from)?;
    Ok(row.flatten())
}

/// Walk `source_session_id` to the family ROOT — the top-most ancestor,
/// which owns the single pinned plan card. Bounded to defend against a
/// cycle / very deep chain; returns the last reachable session.
fn resolve_root(central: &CentralDb, sess: &Session) -> Session {
    let mut current = sess.clone();
    for _ in 0..32 {
        let Some(parent_id) = current.source_session_id else {
            return current;
        };
        match sessions::get(central, parent_id) {
            Ok(parent) => current = parent,
            // Parent gone (deleted) — treat the current node as the root.
            Err(_) => return current,
        }
    }
    current
}

/// Active `create_agent` children of `root_id` (siblings record the
/// parent as their `source_session_id`). Direct children only — a
/// grandchild rolls up under its own parent's card.
fn family_children(central: &CentralDb, root_id: SessionId) -> Vec<Session> {
    sessions::list_active(central)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.source_session_id == Some(root_id))
        .collect()
}

/// Collapse a child's per-item statuses into one header status:
/// `Completed` iff every item is; `Blocked` if any item is stuck (the most
/// actionable signal — a blocked step stalls the whole child); `InProgress`
/// once any work has started (an item in progress, or some-but-not-all
/// completed); else `Pending`.
fn aggregate_status(items: &[TodoListItem]) -> TodoItemStatus {
    if !items.is_empty() && items.iter().all(|i| i.status == TodoItemStatus::Completed) {
        return TodoItemStatus::Completed;
    }
    if items.iter().any(|i| i.status == TodoItemStatus::Blocked) {
        return TodoItemStatus::Blocked;
    }
    if items.iter().any(|i| {
        matches!(
            i.status,
            TodoItemStatus::InProgress | TodoItemStatus::Completed
        )
    }) {
        return TodoItemStatus::InProgress;
    }
    TodoItemStatus::Pending
}

/// Build the single rolled-up card: the root's items, then one labeled,
/// indented section per child (`↳ <name>` header carrying the child's
/// aggregate status, followed by its items indented). Item ids are
/// renumbered so they stay unique across the combined list — renderers
/// key per-item state on `id`.
fn build_combined(root: Option<TodoList>, children: &[(String, TodoList)]) -> TodoList {
    let title = root.as_ref().and_then(|l| l.title.clone());
    let mut items: Vec<TodoListItem> = root.map(|l| l.items).unwrap_or_default();
    let mut next_id: u32 = items
        .iter()
        .map(|i| i.id)
        .max()
        .map_or(0, |m| m.wrapping_add(1));
    for (name, child) in children {
        items.push(TodoListItem {
            id: next_id,
            text: format!("↳ {name}"),
            status: aggregate_status(&child.items),
            blocked_reason: None,
        });
        next_id = next_id.wrapping_add(1);
        for it in &child.items {
            items.push(TodoListItem {
                id: next_id,
                text: format!("    {}", it.text),
                status: it.status,
                blocked_reason: None,
            });
            next_id = next_id.wrapping_add(1);
        }
    }
    TodoList { items, title }
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
/// Persist a runner-emitted `usage_report` payload into `agent_turns`. The
/// `payload` is the inner object of the system row's `{"usage_report": {...}}`
/// envelope (i.e. `ParsedAction::payload` from
/// [`crate::system_actions::parse_system_content`]).
///
/// `pub` so the host crate's provider-resilience integration test can drive
/// the REAL record step of the emit→record→fold path, rather than re-creating
/// the row through the test-only `agent_turns::insert` helper. The crucial
/// field is `error`: without it persisted, the host's health fold can never
/// classify a failure and an automatic failover never fires.
pub fn record_usage_report(
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

/// Record a `save_skill` request that could not even be queued for approval
/// (missing config or a malformed payload): mark the delivery row failed and
/// surface a `self_mod_error` inbound so the agent learns. Mirrors
/// [`record_self_mod_failure`] but takes a plain reason string (the failure is
/// a config / payload problem, not a `DbError`).
fn record_save_skill_failure(
    sess: &Session,
    row: &MessageOutRow,
    inbound_pool: &SessionPool,
    reason: &str,
) -> Result<(), DeliveryError> {
    warn!(
        session = %sess.id.as_uuid(),
        agent_group = %sess.agent_group_id.as_uuid(),
        reason,
        "save_skill request refused before approval",
    );
    copperclaw_metrics::inc_self_mod_failed("save_skill");
    let in_conn = inbound_pool.connect()?;
    delivered::insert(&in_conn, row.id, Some(reason), "failed")?;
    let inbound_row = messages_in::WriteInbound {
        id: MessageId::new(),
        kind: MessageKind::System,
        timestamp: chrono::Utc::now(),
        content: serde_json::json!({
            "kind": "system",
            "content": {
                "self_mod_error": {
                    "action": "save_skill",
                    "error": reason,
                    "guidance": "The skill could not be saved. Inspect the error; if it is a validation issue, correct the SKILL.md and retry.",
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
            "save_skill self_mod_error inbound write failed; agent will not see the failure"
        );
    }
    Ok(())
}

/// Record a `task_grant` request that could not be queued for approval
/// (unresolvable task or malformed payload): mark the delivery row failed and
/// surface a `self_mod_error` inbound so the agent learns. The M22 A1 twin of
/// [`record_save_skill_failure`].
fn record_task_grant_failure(
    sess: &Session,
    row: &MessageOutRow,
    inbound_pool: &SessionPool,
    reason: &str,
) -> Result<(), DeliveryError> {
    warn!(
        session = %sess.id.as_uuid(),
        agent_group = %sess.agent_group_id.as_uuid(),
        reason,
        "task_grant request refused before approval",
    );
    copperclaw_metrics::inc_self_mod_failed("task_grant");
    let in_conn = inbound_pool.connect()?;
    delivered::insert(&in_conn, row.id, Some(reason), "failed")?;
    let inbound_row = messages_in::WriteInbound {
        id: MessageId::new(),
        kind: MessageKind::System,
        timestamp: chrono::Utc::now(),
        content: serde_json::json!({
            "kind": "system",
            "content": {
                "self_mod_error": {
                    "action": "task_grant",
                    "error": reason,
                    "guidance": "The capability grant could not be authorized. The task itself may still have been scheduled; if you meant to grant it, schedule the task and its grant in the same call.",
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
            "task_grant self_mod_error inbound write failed; agent will not see the failure"
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
            tool_profile: None,
            preview_enabled: false,
            preview_bind: None,
            check_command: None,
            verify_gate: true,
            image_profile: copperclaw_types::ImageProfile::Minimal,
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
    let transport = payload
        .get("transport")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
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

/// Apply a `goal` system payload against the central `goals` table (M22 A3).
///
/// Two ops:
///   * `create` — INSERT a new goal for this session, computing the first
///     `next_checkin` from `first_checkin` (absolute) or the `checkin_recurrence`
///     cron; and
///   * `update` — patch a goal by id: an `objective` refine, a `status`
///     transition (validated by `goals::set_status`), and/or a `progress` note
///     appended to the goal's log (accruing `progress_tokens`).
///
/// A goal is internal state (decision (d)), so this applies immediately — no
/// approval gate. An unknown op / malformed payload surfaces as a
/// `DbError::Invariant` so the delivery loop records a self-mod failure the
/// agent can see, rather than silently dropping the row.
fn apply_goal(
    central: &copperclaw_db::central::CentralDb,
    sess: &Session,
    payload: &serde_json::Value,
) -> Result<(), copperclaw_db::DbError> {
    use copperclaw_db::tables::goals::{self, GoalStatus, NewGoal, UpdateFields};
    use copperclaw_modules::scheduling::{When, compute_next_fire};

    let op = payload.get("op").and_then(serde_json::Value::as_str);
    let body = payload.get("payload").unwrap_or(&serde_json::Value::Null);
    let str_field = |key: &str| -> Option<String> {
        body.get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    let i64_field =
        |key: &str| -> Option<i64> { body.get(key).and_then(serde_json::Value::as_i64) };
    let ts_field = |key: &str| -> Option<chrono::DateTime<chrono::Utc>> {
        body.get(key)
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc))
    };

    match op {
        Some("create") => {
            let objective = str_field("objective").ok_or_else(|| {
                copperclaw_db::DbError::invariant("goal create: missing objective")
            })?;
            let checkin_recurrence = str_field("checkin_recurrence");
            let now = chrono::Utc::now();
            // First check-in: an explicit `first_checkin`, else derived from the
            // recurrence, else none (a goal driven only by an external task).
            let next_checkin = ts_field("first_checkin").or_else(|| {
                checkin_recurrence
                    .as_deref()
                    .and_then(|rec| compute_next_fire(&When::At(now), now, Some(rec)))
            });
            goals::insert(
                central,
                NewGoal {
                    id: uuid::Uuid::new_v4().to_string(),
                    agent_group_id: sess.agent_group_id,
                    session_id: sess.id,
                    objective,
                    task_id: None,
                    grant_id: None,
                    token_budget: i64_field("token_budget"),
                    checkin_recurrence,
                    checkin_prompt: str_field("checkin_prompt"),
                    next_checkin,
                },
            )?;
            Ok(())
        }
        Some("update") => {
            let id = str_field("id")
                .ok_or_else(|| copperclaw_db::DbError::invariant("goal update: missing id"))?;
            // Objective refine first, then status transition, then progress.
            if let Some(objective) = str_field("objective") {
                goals::update(
                    central,
                    &id,
                    UpdateFields {
                        objective: Some(objective),
                        ..Default::default()
                    },
                )?;
            }
            if let Some(status_str) = str_field("status") {
                let status: GoalStatus = status_str
                    .parse()
                    .map_err(|e: String| copperclaw_db::DbError::invariant(e))?;
                goals::set_status(central, &id, status)?;
            }
            if let Some(note) = str_field("progress") {
                goals::record_progress(
                    central,
                    &uuid::Uuid::new_v4().to_string(),
                    &id,
                    &note,
                    i64_field("progress_tokens"),
                )?;
            }
            Ok(())
        }
        other => Err(copperclaw_db::DbError::invariant(format!(
            "goal: unknown op {other:?}"
        ))),
    }
}

/// Apply a `{"condition": {...}}` system row (M22 A4): register / deregister a
/// durable HEARTBEAT-style condition, or set / clear a per-session flag latch.
/// Mirrors [`apply_goal`] — internal tracking state applied immediately.
fn apply_condition(
    central: &copperclaw_db::central::CentralDb,
    sess: &Session,
    payload: &serde_json::Value,
) -> Result<(), copperclaw_db::DbError> {
    use copperclaw_db::tables::conditions::{self, NewCondition};

    let op = payload.get("op").and_then(serde_json::Value::as_str);
    let body = payload.get("payload").unwrap_or(&serde_json::Value::Null);
    let str_field = |key: &str| -> Option<String> {
        body.get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    let i64_field =
        |key: &str| -> Option<i64> { body.get(key).and_then(serde_json::Value::as_i64) };

    match op {
        Some("register") => {
            let id = str_field("id").ok_or_else(|| {
                copperclaw_db::DbError::invariant("condition register: missing id")
            })?;
            let kind = str_field("kind").ok_or_else(|| {
                copperclaw_db::DbError::invariant("condition register: missing kind")
            })?;
            let prompt = str_field("prompt").ok_or_else(|| {
                copperclaw_db::DbError::invariant("condition register: missing prompt")
            })?;
            conditions::upsert(
                central,
                NewCondition {
                    id,
                    agent_group_id: sess.agent_group_id,
                    session_id: sess.id,
                    kind,
                    threshold: i64_field("threshold"),
                    flag: str_field("flag"),
                    prompt,
                    grant_id: str_field("grant_id"),
                },
            )?;
            Ok(())
        }
        Some("remove") => {
            let id = str_field("id")
                .ok_or_else(|| copperclaw_db::DbError::invariant("condition remove: missing id"))?;
            conditions::soft_remove(central, &id, chrono::Utc::now())?;
            Ok(())
        }
        Some("set_flag") => {
            let flag = str_field("flag").ok_or_else(|| {
                copperclaw_db::DbError::invariant("condition set_flag: missing flag")
            })?;
            let value = body
                .get("value")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if value {
                conditions::set_flag(
                    central,
                    sess.agent_group_id,
                    sess.id,
                    &flag,
                    chrono::Utc::now(),
                )?;
            } else {
                conditions::clear_flag(central, sess.id, &flag)?;
            }
            Ok(())
        }
        other => Err(copperclaw_db::DbError::invariant(format!(
            "condition: unknown op {other:?}"
        ))),
    }
}

/// Apply a `{"grant_consume": {...}}` system row (M22 A2H): DEBIT an
/// already-approved `task_grants` row as a granted autonomous fire happens.
///
/// Payload shape (emitted by the runner's `charge_grant_fire_once`):
/// `{ "grant_id": "...", "task_id": "...", "fires": 1 }`, optionally carrying a
/// `"tokens"` count. `task_id` is informational (the fire was already authorized
/// against `grant_id` in-runner); we debit `grant_id` directly. `fires` defaults
/// to 1 and is clamped non-negative; each unit is one [`consume_fire`] call
/// (which bumps `fires_consumed` by one), so a grant with `max_fires` genuinely
/// depletes across fires and `effective_grant` reads it inert once spent.
///
/// This is internal accounting, NOT approval-gated — it can only *reduce* an
/// existing authorization, never widen one.
fn apply_grant_consume(
    central: &copperclaw_db::central::CentralDb,
    payload: &serde_json::Value,
) -> Result<(), copperclaw_db::DbError> {
    use copperclaw_db::tables::task_grants;

    let grant_id = payload
        .get("grant_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| copperclaw_db::DbError::invariant("grant_consume: missing grant_id"))?;
    let now = chrono::Utc::now();

    // A `grant_consume` row means at least one fire happened; default to 1 and
    // clamp negatives away so a malformed payload can never *credit* a grant.
    let fires = payload
        .get("fires")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(1)
        .max(0);
    for _ in 0..fires {
        task_grants::consume_fire(central, grant_id, now)?;
    }

    // Optional token spend. `consume_tokens` rejects negatives itself; we only
    // call it for a positive count.
    if let Some(tokens) = payload.get("tokens").and_then(serde_json::Value::as_i64) {
        if tokens > 0 {
            task_grants::consume_tokens(central, grant_id, tokens, now)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockRoot, make_service};
    use chrono::Utc;
    use copperclaw_channels_core::AdapterError;
    use copperclaw_channels_core::testing::MockAdapter;
    use copperclaw_db::tables::container_configs;
    use copperclaw_db::tables::messages_out::WriteOutbound;
    use copperclaw_modules::ModuleError;
    use copperclaw_modules::context::MockDispatcher;
    use copperclaw_modules::{DeliveryActionHandler, DeliveryActionInput, DeliveryActionOutput};
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

    /// Regression for the `delivered.message_out_id` UNIQUE poison. A pass that
    /// re-processes a row already recorded as `delivered=ok` (because its
    /// delivered snapshot went stale relative to a racing pass) must complete
    /// green — NOT raise a Db error that then poisons every subsequent pass.
    #[tokio::test]
    async fn re_delivering_already_recorded_row_is_ok_not_poison() {
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
        let row = {
            let c = out_pool.connect().unwrap();
            messages_out::get(&c, chat.id).unwrap()
        };

        // Mimic the pass that won the race: the row is already recorded ok.
        {
            let c = in_pool.connect().unwrap();
            assert!(delivered::insert(&c, chat.id, Some("p-1"), "ok").unwrap());
        }

        // The losing pass re-runs `process_row` for the same row off a stale
        // snapshot. Under the old plain INSERT this returned Err(Db(UNIQUE));
        // now the record is an idempotent no-op and the row delivers cleanly.
        service
            .process_row(&sess, &row, None, &in_pool)
            .await
            .expect("re-delivering an already-recorded row must not error");

        // Exactly one delivered row survives — the duplicate record was dropped.
        let rows = {
            let c = in_pool.connect().unwrap();
            delivered::list(&c).unwrap()
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "ok");
        assert_eq!(rows[0].platform_message_id.as_deref(), Some("p-1"));
        // The re-send did reach the adapter (the accepted rare-duplicate-send
        // tradeoff); what matters is the pass stayed green.
        assert_eq!(mock.deliveries().len(), 1);
    }

    /// Root-cause sequence: the active loop and the sweep loop share one
    /// `DeliveryService` and can run passes over the same running session
    /// concurrently. The atomic in-flight claim must let at most one pass send
    /// the row, leaving a single delivered record and no error from either
    /// pass. (Before the fix, both passes could send + record and the second
    /// insert blew up the UNIQUE constraint.)
    #[tokio::test]
    async fn concurrent_passes_deliver_row_once() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        write_row(
            &out_pool,
            &make_row(MessageKind::Chat, json!({"text": "hi"})),
        );

        // Two overlapping passes over the same session, as the active + sweep
        // loops would produce for a running session.
        let a = service.process_session_once(&sess);
        let b = service.process_session_once(&sess);
        let (ra, rb) = tokio::join!(a, b);
        let ra = ra.expect("pass A must not error");
        let rb = rb.expect("pass B must not error");

        // Exactly one pass delivered the row; the other found it claimed /
        // already recorded and skipped it. Never two DB records, never a poison.
        assert_eq!(ra.delivered + rb.delivered, 1);
        assert_eq!(mock.deliveries().len(), 1);
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let rows = {
            let c = in_pool.connect().unwrap();
            delivered::list(&c).unwrap()
        };
        assert_eq!(rows.len(), 1);
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
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
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
        let parts = split_chat_content_if_needed(&v, None, "test");
        assert_eq!(parts.len(), 1);
        let parts = split_chat_content_if_needed(&v, Some(4096), "test");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], v);
    }

    #[test]
    fn split_chat_passthrough_when_no_text_field() {
        let v = json!({"foo":"bar"});
        let parts = split_chat_content_if_needed(&v, Some(10), "test");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], v);
    }

    #[test]
    fn split_chat_breaks_on_paragraph_when_possible() {
        let text = format!("{}\n\n{}", "a".repeat(50), "b".repeat(50));
        let v = json!({"text": text, "parse_mode": "MarkdownV2"});
        let parts = split_chat_content_if_needed(&v, Some(60), "test");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"].as_str().unwrap(), "a".repeat(50));
        assert_eq!(parts[1]["text"].as_str().unwrap(), "b".repeat(50));
        // Other content keys are preserved on every chunk.
        assert_eq!(parts[0]["parse_mode"].as_str(), Some("MarkdownV2"));
        assert_eq!(parts[1]["parse_mode"].as_str(), Some("MarkdownV2"));
    }

    #[test]
    fn split_chat_breaks_on_sentence_when_no_paragraph() {
        let text = format!("{}. {}", "a".repeat(40), "b".repeat(40));
        let v = json!({"text": text});
        let parts = split_chat_content_if_needed(&v, Some(50), "test");
        assert_eq!(parts.len(), 2);
        assert!(parts[0]["text"].as_str().unwrap().ends_with('.'));
    }

    #[test]
    fn split_chat_hard_cuts_when_no_natural_boundary() {
        let text = "x".repeat(100);
        let v = json!({"text": text});
        let parts = split_chat_content_if_needed(&v, Some(30), "test");
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
        let parts = split_chat_content_if_needed(&v, Some(10), "test");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"].as_str().unwrap().chars().count(), 10);
    }

    /// Split `text` at `max` and assert the shared fence-splitting
    /// invariants: every chunk fits the cap and parses with balanced
    /// fences. Returns the chunks for case-specific assertions.
    fn split_balanced(text: &str, max: usize) -> Vec<String> {
        let chunks = split_text_into_chunks(text, max, "test");
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.chars().count() <= max,
                "chunk {i} exceeds cap {max}: {} chars",
                c.chars().count()
            );
            assert!(
                crate::fence::is_balanced(c),
                "chunk {i} has unbalanced fences:\n{c}"
            );
        }
        chunks
    }

    #[test]
    fn split_fence_longer_than_cap_closes_and_reopens() {
        // A single fenced block longer than the cap: the splitter must
        // close the fence at the cut and reopen it — same info string —
        // on the next chunk, cutting at a line boundary.
        let code: String = (0..40).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("line_{i:03}_abcdefghij\n"));
            acc
        });
        let text = format!("```rust\n{code}```");
        let chunks = split_balanced(&text, 200);
        assert!(chunks.len() > 1, "fence must split: {chunks:?}");
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.starts_with("```rust\n"),
                "chunk {i} must open with the info string: {c}"
            );
            assert!(c.ends_with("```"), "chunk {i} must close the fence: {c}");
        }
        // No code line is torn in half: every original line appears
        // intact in exactly one chunk.
        let joined = chunks.join("\n");
        for i in 0..40 {
            let line = format!("line_{i:03}_abcdefghij");
            assert_eq!(
                joined.matches(&line).count(),
                1,
                "line {i} torn or duplicated"
            );
        }
    }

    #[test]
    fn split_prefers_pre_fence_cut_when_fence_fits_next_chunk() {
        // Intro line + a two-line fence that fits the cap on its own but
        // not together with the intro. find_cut's natural cut (the last
        // newline in the window) lands INSIDE the fence; the splitter
        // must move it BEFORE the fence, which is then delivered whole
        // in the second chunk.
        let intro = "x".repeat(60);
        let fence = format!("```py\n{}\n{}\n```", "y".repeat(30), "y".repeat(30));
        let text = format!("{intro}\n{fence}");
        let chunks = split_balanced(&text, 100);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert_eq!(chunks[0], intro);
        assert_eq!(chunks[1], fence);
    }

    #[test]
    fn split_does_not_cut_at_blank_line_inside_fitting_fence() {
        // The fence contains a blank line — a paragraph boundary that
        // find_cut picks as the natural cut — and fits the window whole;
        // the trailing text pushes the total over the cap. The cut must
        // land AFTER the fence, not at the blank line inside it.
        let fence = format!("```\n{}\n\n{}\n```", "a".repeat(20), "b".repeat(20));
        let tail = "t".repeat(80);
        let text = format!("{fence}\n{tail}");
        let chunks = split_balanced(&text, 100);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert_eq!(chunks[0], fence);
        assert_eq!(chunks[1], tail);
    }

    #[test]
    fn split_pre_block_longer_than_cap_closes_and_reopens_tag() {
        // Telegram HTML <pre> block longer than the cap: close with
        // </pre> at the cut, reopen with the original tag (attributes
        // preserved) on the next chunk.
        let body: String = (0..30).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("row_{i:02}_0123456789\n"));
            acc
        });
        let text = format!("<pre language=\"c\">{body}</pre>");
        let chunks = split_balanced(&text, 120);
        assert!(chunks.len() > 1, "{chunks:?}");
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.starts_with("<pre language=\"c\">"),
                "chunk {i} must reopen the tag: {c}"
            );
            assert!(c.ends_with("</pre>"), "chunk {i} must close the tag: {c}");
        }
    }

    #[test]
    fn split_preserves_code_indentation_across_reopen() {
        // Cutting inside a fence consumes only the newline at the cut, so
        // the next code line keeps its leading indentation.
        let code: String = (0..30).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("    indented_{i:02}_stmt();\n"));
            acc
        });
        let text = format!("```c\n{code}```");
        let chunks = split_balanced(&text, 150);
        assert!(chunks.len() > 1, "{chunks:?}");
        for (i, c) in chunks.iter().skip(1).enumerate() {
            let first_code_line = c.lines().nth(1).unwrap_or("");
            assert!(
                first_code_line.starts_with("    indented_"),
                "chunk {} lost indentation after reopen: {c}",
                i + 1
            );
        }
    }

    #[test]
    fn split_unfenced_text_behaviour_unchanged_by_fence_scan() {
        // Inline backticks and mid-line triple-backticks are not fences;
        // the paragraph cut behaves exactly as before.
        let text = format!(
            "uses `inline` and ```mid-line``` marks {}\n\n{}",
            "a".repeat(40),
            "b".repeat(50)
        );
        let chunks = split_balanced(&text, 90);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert!(chunks[1].starts_with('b'));
    }

    #[test]
    fn split_chat_fenced_block_chunks_all_balanced_via_public_entry() {
        // End-to-end through split_chat_content_if_needed: content shape
        // (parse_mode) is preserved on every fence-split chunk.
        let code: String = (0..60).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("value_{i:03}=compute_segment({i});\n"));
            acc
        });
        let text = format!("Here is the script:\n\n```python\n{code}```");
        let v = json!({"text": text, "parse_mode": "MarkdownV2"});
        let parts = split_chat_content_if_needed(&v, Some(500), "test");
        assert!(parts.len() > 2, "{}", parts.len());
        for p in &parts {
            let t = p["text"].as_str().unwrap();
            assert!(t.chars().count() <= 500);
            assert!(crate::fence::is_balanced(t), "unbalanced: {t}");
            assert_eq!(p["parse_mode"].as_str(), Some("MarkdownV2"));
        }
        assert_eq!(parts[0]["text"].as_str().unwrap(), "Here is the script:");
        assert!(
            parts[1]["text"]
                .as_str()
                .unwrap()
                .starts_with("```python\n")
        );
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
        write_row(
            &out_pool,
            &make_row(MessageKind::Chat, json!({"text":"hi"})),
        );
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
            delay >= Duration::from_millis(9_500) && delay <= Duration::from_millis(10_500),
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
            delay >= Duration::from_millis(4_500) && delay <= Duration::from_millis(5_500),
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
        mock.fail_next_deliver(AdapterError::Rate {
            retry_after: Some(1),
        });
        let rpt1 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt1.deferred, 1);
        assert_eq!(rpt1.delivered, 0);
        // Force backoff window to be in the past so the retry can execute.
        if let Some(mut entry) = service.retries.get_mut(&DeliveryKey::new(sess.id, row.id)) {
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
            err_row
                .channel_type
                .as_ref()
                .map(copperclaw_types::ChannelType::as_str),
            Some("mock")
        );
        // The row carries the canonical ErrorCard in content.error,
        // tagged with kind=Delivery and `retryable=false` (we GAVE UP
        // retrying — promising another retry would be a lie).
        let card: ErrorCard = serde_json::from_value(err_row.content["error"].clone()).unwrap();
        assert_eq!(card.kind, ErrorCardKind::Delivery);
        assert!(!card.retryable);
        // Summary mentions both the retry budget AND the underlying
        // adapter error so operators can grep for the failure mode.
        assert!(card.summary.contains("retry budget"));
        assert!(card.summary.contains("502"));
    }

    // ------------------------------------------------------------------
    // M21 S3 — persisted retry state (migration 029). The in-memory
    // retries map is a write-through cache over the row's `tries` /
    // `not_before` columns, primed lazily on the first poll of a session
    // so attempt budgets and backoff windows survive a host restart.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn bump_retry_writes_through_to_the_row() {
        // A retryable failure must mirror the attempt counter and the
        // wall-clock backoff window onto the outbound row itself.
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
        let before = Utc::now();
        let _ = service.process_session_once(&sess).await.unwrap();

        let out_conn = out_pool.connect().unwrap();
        let persisted = messages_out::list_retry_state(&out_conn).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].id, row.id);
        assert_eq!(persisted[0].tries, 1);
        let not_before = persisted[0].not_before.expect("window persisted");
        let delay = not_before - before;
        assert!(
            delay >= chrono::Duration::milliseconds(9_500)
                && delay <= chrono::Duration::milliseconds(10_500),
            "expected ~10s persisted window, got {delay}",
        );
    }

    #[tokio::test]
    async fn priming_honors_persisted_not_before() {
        // A row parked mid-backoff when the host went down must NOT fire
        // immediately on boot: the first poll primes the cache from the
        // row and the remaining window is honoured.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        {
            let out_conn = out_pool.connect().unwrap();
            messages_out::set_retry_state(
                &out_conn,
                row.id,
                1,
                Some(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();
        }

        // "Boot": a fresh service over the same DBs (empty caches).
        let (restarted, mock2) = crate::test_support::restart_service(&service);
        drop(mock);
        let rpt = restarted.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.deferred, 1, "row mid-backoff must defer, not fire");
        assert_eq!(rpt.delivered, 0);
        assert!(
            mock2.deliveries().is_empty(),
            "no attempt inside the persisted window"
        );

        // The primed cache carries the persisted attempt count.
        let key = DeliveryKey::new(sess.id, row.id);
        {
            let entry = restarted.retries.get(&key).expect("cache primed");
            assert_eq!(entry.tries, 1);
        }

        // Once the (in-memory) window is popped, the retry proceeds.
        if let Some(mut entry) = restarted.retries.get_mut(&key) {
            entry.not_before = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
        let rpt2 = restarted.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt2.delivered, 1);
        assert_eq!(mock2.deliveries().len(), 1);
    }

    #[tokio::test]
    async fn restart_resumes_persisted_attempt_count() {
        // Integration: kill and restart the delivery service mid-retry.
        // Two failed attempts before the "restart" plus one after must
        // exhaust MAX_DELIVERY_ATTEMPTS (3) — the restart must NOT reset
        // the budget — and exhaustion must dead-letter exactly once: one
        // delivered{status="failed"} row and one ErrorCard.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);

        let key = DeliveryKey::new(sess.id, row.id);
        for _ in 0..(MAX_DELIVERY_ATTEMPTS - 1) {
            mock.fail_next_deliver(AdapterError::Transport("502".into()));
            if let Some(mut entry) = service.retries.get_mut(&key) {
                entry.not_before = Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .unwrap_or_else(Instant::now);
            }
            let _ = service.process_session_once(&sess).await.unwrap();
        }
        {
            let out_conn = out_pool.connect().unwrap();
            let persisted = messages_out::list_retry_state(&out_conn).unwrap();
            assert_eq!(persisted.len(), 1);
            assert_eq!(persisted[0].tries, MAX_DELIVERY_ATTEMPTS - 1);
            // Simulate downtime longer than the backoff window: rewind the
            // persisted window into the past, counter untouched.
            messages_out::set_retry_state(
                &out_conn,
                row.id,
                MAX_DELIVERY_ATTEMPTS - 1,
                Some(Utc::now() - chrono::Duration::seconds(1)),
            )
            .unwrap();
        }

        // "Restart": fresh service, fresh adapter, same DBs.
        let (restarted, mock2) = crate::test_support::restart_service(&service);
        drop(service);
        drop(mock);
        mock2.fail_next_deliver(AdapterError::Transport("502 again".into()));
        let rpt = restarted.process_session_once(&sess).await.unwrap();
        assert_eq!(
            rpt.failed, 1,
            "third attempt after restart must exhaust the persisted budget"
        );
        assert_eq!(
            mock2.deliveries().len(),
            0,
            "the exhausting attempt failed; nothing delivered"
        );

        // Exactly one failed record...
        let in_pool = restarted
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        let failed: Vec<_> = listed.iter().filter(|d| d.status == "failed").collect();
        assert_eq!(
            failed.len(),
            1,
            "exactly one delivered{{status=failed}} row"
        );
        // ...and exactly one ErrorCard, even across further passes.
        let _ = restarted.process_session_once(&sess).await.unwrap();
        let out_conn = out_pool.connect().unwrap();
        let error_rows = messages_out::list_due(&out_conn)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MessageKind::Error)
            .count();
        assert_eq!(error_rows, 1, "exactly one ErrorCard across the restart");
    }

    #[tokio::test]
    async fn persisted_exhaustion_dead_letters_without_a_fresh_attempt() {
        // The host died between the final bump (tries persisted at the
        // budget) and the `failed` record. On the next boot the row must be
        // dead-lettered WITHOUT burning a fourth adapter attempt, and only
        // once.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        {
            let out_conn = out_pool.connect().unwrap();
            messages_out::set_retry_state(&out_conn, row.id, MAX_DELIVERY_ATTEMPTS, None).unwrap();
        }

        let (restarted, mock2) = crate::test_support::restart_service(&service);
        drop(mock);
        let rpt = restarted.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1);
        assert!(
            mock2.deliveries().is_empty(),
            "an exhausted row must not get another adapter attempt"
        );

        let in_pool = restarted
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        let listed = delivered::list(&in_conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");

        // A second pass changes nothing: the delivered record is terminal.
        let rpt2 = restarted.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt2.failed, 0);
        let out_conn = out_pool.connect().unwrap();
        let error_rows = messages_out::list_due(&out_conn)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MessageKind::Error)
            .count();
        assert_eq!(error_rows, 1, "dead-letter ErrorCard emitted exactly once");
    }

    #[test]
    fn no_adapter_ceiling_math() {
        // Unit: the 24h age ceiling (M21 S5). Below the ceiling not
        // expired; at and past it expired; a future-dated row (clock
        // skew) never expires.
        let now = Utc::now();
        assert!(!no_adapter_expired(now, now), "fresh row is not expired");
        assert!(
            !no_adapter_expired(
                now - chrono::Duration::hours(NO_ADAPTER_MAX_AGE_HOURS)
                    + chrono::Duration::seconds(1),
                now
            ),
            "one second inside the ceiling is not expired"
        );
        assert!(
            no_adapter_expired(now - chrono::Duration::hours(NO_ADAPTER_MAX_AGE_HOURS), now),
            "exactly at the ceiling is expired"
        );
        assert!(
            no_adapter_expired(
                now - chrono::Duration::hours(NO_ADAPTER_MAX_AGE_HOURS + 1),
                now
            ),
            "past the ceiling is expired"
        );
        assert!(
            !no_adapter_expired(now + chrono::Duration::hours(1), now),
            "a future-dated timestamp is never expired"
        );
    }

    #[tokio::test]
    async fn no_adapter_rows_below_ceiling_stay_pending() {
        // A row addressed at a channel with no live adapter but younger
        // than the age ceiling is untouched: still pending, no terminal
        // record, no dead letter — only counted deferred and in the
        // no-adapter backlog.
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Chat, json!({"text": "hi"}));
        row.channel_type = Some(ChannelType::new("ghost"));
        write_row(&out_pool, &row);

        for pass in 0..2 {
            let rpt = service.process_session_once(&sess).await.unwrap();
            assert_eq!(rpt.deferred, 1, "pass {pass}: row defers");
            assert_eq!(rpt.failed, 0, "pass {pass}: row is not failed");
            assert_eq!(rpt.delivered, 0, "pass {pass}: nothing delivered");
        }

        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_conn = in_pool.connect().unwrap();
        assert!(
            delivered::list(&in_conn).unwrap().is_empty(),
            "no terminal record below the ceiling"
        );
        assert!(
            outbound_dropped_messages::list(service.central(), None, None)
                .unwrap()
                .is_empty(),
            "no dead letter below the ceiling"
        );
        let out_conn = out_pool.connect().unwrap();
        assert_eq!(
            messages_out::list_due(&out_conn).unwrap().len(),
            1,
            "the row is still pending in messages_out"
        );
        assert_eq!(service.no_adapter_backlog(), 1);
    }

    #[tokio::test]
    async fn no_adapter_rows_expire_into_dead_letters_and_replay() {
        // Integration (M21 S5 acceptance): an unconfigured channel's rows
        // expire into `outbound_dropped_messages` with reason `no_adapter`
        // — exactly once, with no user-facing ErrorCard — and replay
        // cleanly once the adapter exists.
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let mut row = make_row(MessageKind::Chat, json!({"text": "belated hello"}));
        row.channel_type = Some(ChannelType::new("ghost"));
        row.timestamp = Utc::now() - chrono::Duration::hours(NO_ADAPTER_MAX_AGE_HOURS + 1);
        write_row(&out_pool, &row);

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1, "aged no-adapter row dead-letters");
        assert_eq!(rpt.deferred, 0);

        // Dead letter recorded centrally with the no_adapter reason and the
        // full routing + content a replay needs.
        let drops = outbound_dropped_messages::list(service.central(), None, None).unwrap();
        assert_eq!(drops.len(), 1);
        let drop = &drops[0];
        assert!(
            drop.last_error.starts_with(NO_ADAPTER_DROP_REASON),
            "reason prefix is `no_adapter`, got: {}",
            drop.last_error
        );
        assert_eq!(drop.message_out_id, row.id);
        assert_eq!(drop.channel_type, Some(ChannelType::new("ghost")));
        assert_eq!(drop.platform_id.as_deref(), Some("plat-1"));
        assert_eq!(drop.kind, MessageKind::Chat);
        assert_eq!(drop.content, json!({"text": "belated hello"}));

        // Terminal record written; backlog cleared; NO ErrorCard emitted
        // (these rows by definition have no deliverable channel).
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        {
            let in_conn = in_pool.connect().unwrap();
            let listed = delivered::list(&in_conn).unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].status, "failed");
        }
        assert_eq!(service.no_adapter_backlog(), 0);
        {
            let out_conn = out_pool.connect().unwrap();
            let error_rows = messages_out::list_due(&out_conn)
                .unwrap()
                .into_iter()
                .filter(|r| r.kind == MessageKind::Error)
                .count();
            assert_eq!(
                error_rows, 0,
                "no user-facing ErrorCard for no_adapter drops"
            );
        }

        // Exactly once: a second pass adds nothing.
        let rpt2 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt2.failed, 0);
        assert_eq!(
            outbound_dropped_messages::list(service.central(), None, None)
                .unwrap()
                .len(),
            1
        );

        // Replay once the adapter exists — mirror the
        // `dropped-messages.replay` handler: re-insert a fresh copy of the
        // message from the dead-letter record, then delete the record.
        let ghost = Arc::new(MockAdapter::new("ghost"));
        service.register_adapter(ChannelType::new("ghost"), ghost.clone());
        let replayed = WriteOutbound {
            id: MessageId::new(),
            in_reply_to: None,
            timestamp: Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind: drop.kind,
            platform_id: drop.platform_id.clone(),
            channel_type: drop.channel_type.clone(),
            thread_id: drop.thread_id.clone(),
            content: drop.content.clone(),
        };
        write_row(&out_pool, &replayed);
        assert!(outbound_dropped_messages::delete(service.central(), drop.id).unwrap());

        let rpt3 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt3.delivered, 1, "replayed row delivers cleanly");
        assert_eq!(rpt3.failed, 0);
        let deliveries = ghost.deliveries();
        assert_eq!(deliveries.len(), 1);
        assert!(
            outbound_dropped_messages::list(service.central(), None, None)
                .unwrap()
                .is_empty(),
            "dead letter consumed by the replay"
        );
    }

    #[tokio::test]
    async fn no_adapter_backlog_counts_and_clears() {
        // The backlog gauge reflects the rows currently waiting on a
        // missing adapter and drops back to zero once they deliver.
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        for text in ["one", "two"] {
            let mut row = make_row(MessageKind::Chat, json!({ "text": text }));
            row.channel_type = Some(ChannelType::new("ghost"));
            write_row(&out_pool, &row);
        }
        assert_eq!(service.no_adapter_backlog(), 0, "empty before any pass");

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.deferred, 2);
        assert_eq!(service.no_adapter_backlog(), 2);

        // Wire the adapter: the rows drain and the backlog clears.
        let ghost = Arc::new(MockAdapter::new("ghost"));
        service.register_adapter(ChannelType::new("ghost"), ghost.clone());
        let rpt2 = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt2.delivered, 2);
        assert_eq!(service.no_adapter_backlog(), 0);
        assert_eq!(ghost.deliveries().len(), 2);
    }

    #[tokio::test]
    async fn no_adapter_expiry_of_ui_chrome_kind_skips_dead_letter() {
        // Ephemeral UI kinds (here: Error) past the ceiling are failed
        // terminally but NOT dead-lettered — `outbound_dropped_messages`
        // only round-trips replayable kinds, and replaying hours-stale UI
        // chrome is meaningless.
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let card = ErrorCard::new(ErrorCardKind::Delivery, "stale chrome");
        let mut row = make_row(MessageKind::Error, json!({ "error": card }));
        row.channel_type = Some(ChannelType::new("ghost"));
        row.timestamp = Utc::now() - chrono::Duration::hours(NO_ADAPTER_MAX_AGE_HOURS + 1);
        write_row(&out_pool, &row);

        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.failed, 1, "aged UI-chrome row is failed terminally");
        assert!(
            outbound_dropped_messages::list(service.central(), None, None)
                .unwrap()
                .is_empty(),
            "no dead letter for a non-replayable kind"
        );
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
    async fn priming_skips_rows_already_delivered() {
        // Terminal rows (present in `delivered`) keep their historical
        // `tries` audit trail on the row, but must not be primed back into
        // the live retry cache after a restart.
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(MessageKind::Chat, json!({"text":"hi"}));
        write_row(&out_pool, &row);
        // One rate-limited attempt (persists tries=1), then success.
        mock.fail_next_deliver(AdapterError::Rate {
            retry_after: Some(1),
        });
        let _ = service.process_session_once(&sess).await.unwrap();
        let key = DeliveryKey::new(sess.id, row.id);
        if let Some(mut entry) = service.retries.get_mut(&key) {
            entry.not_before = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
        }
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);

        let (restarted, _mock2) = crate::test_support::restart_service(&service);
        let _ = restarted.process_session_once(&sess).await.unwrap();
        assert!(
            !restarted.retries.contains_key(&key),
            "delivered row must not re-enter the retry cache on priming"
        );
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
        let row = make_row(MessageKind::System, json!({ "unregistered_action": {} }));
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

    /// M18 G1: delivering the `approval_card` action renders the card (buttons)
    /// and persists the delivered card's platform message id back onto the
    /// `pending_approvals` row, so the in-chat interceptor can later edit it.
    #[tokio::test]
    async fn approval_card_action_persists_platform_message_id() {
        use copperclaw_db::tables::pending_approvals::{self, UpsertPendingApproval};
        let (service, _root, sess, mock) = make_service().await;
        service.register_action(
            "approval_card",
            Arc::new(copperclaw_modules::approvals::ApprovalCardHandler),
        );
        // Seed a pending approval for this session's agent group.
        let approval = pending_approvals::upsert(
            service.central(),
            UpsertPendingApproval {
                request_id: "req-card".into(),
                action: "sender".into(),
                agent_group_id: Some(sess.agent_group_id),
                title: "Approve sender?".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(approval.platform_message_id.is_none());

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let row = make_row(
            MessageKind::System,
            json!({
                "approval_card": {
                    "approval_id": approval.approval_id.as_uuid().to_string(),
                    "title": "Approve sender?",
                    "to": { "channel_type": "mock", "platform_id": "P1" }
                }
            }),
        );
        write_row(&out_pool, &row);
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        // Card rendered (mock's default deliver_card routes to deliver).
        assert_eq!(mock.deliveries().len(), 1);
        // Its platform message id landed on the approval row.
        let after = pending_approvals::get(service.central(), approval.approval_id).unwrap();
        assert!(
            after.platform_message_id.is_some(),
            "approval card platform_message_id must be persisted"
        );
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
        service.register_action("agent_dispatch", Arc::new(Capture { saw: saw.clone() }));

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
        service.inflight.insert(
            DeliveryKey::new(SessionId::new(), MessageId::new()),
            Instant::now(),
        );
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
        let service = DeliveryService::new(central, root, adapters, dispatcher.clone());
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
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
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

    /// A real session row over `central_with_ag`, so `apply_goal`'s FK
    /// (`goals.agent_group_id`) resolves.
    fn central_ag_session() -> (copperclaw_db::central::CentralDb, Session) {
        let (db, ag) = central_with_ag();
        let sess = copperclaw_db::tables::sessions::create(
            &db,
            copperclaw_db::tables::sessions::CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        (db, sess)
    }

    /// M22 A3: the host `goal` handler persists a `create` op into the central
    /// `goals` table, computing the first check-in from the recurrence, then an
    /// `update` op records progress + transitions the lifecycle — the full
    /// create→progress→complete round-trip through the delivery-action payload.
    #[test]
    fn apply_goal_create_then_update_roundtrip() {
        use copperclaw_db::tables::goals::{self, GoalStatus};
        let (db, sess) = central_ag_session();

        apply_goal(
            &db,
            &sess,
            &json!({
                "op": "create",
                "payload": {
                    "objective": "keep the docs current",
                    "checkin_recurrence": "0 9 * * *",
                    "token_budget": 5000
                }
            }),
        )
        .unwrap();

        let created = goals::list_for_session(&db, sess.id).unwrap();
        assert_eq!(created.len(), 1);
        let goal = &created[0];
        assert_eq!(goal.objective, "keep the docs current");
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.token_budget, Some(5000));
        assert!(
            goal.next_checkin.is_some(),
            "first check-in derived from the recurrence"
        );

        // Report progress.
        apply_goal(
            &db,
            &sess,
            &json!({
                "op": "update",
                "payload": { "id": goal.id, "progress": "drafted README", "progress_tokens": 200 }
            }),
        )
        .unwrap();
        let after = goals::get(&db, &goal.id).unwrap().unwrap();
        assert_eq!(after.tokens_consumed, 200);
        assert_eq!(goals::list_progress(&db, &goal.id).unwrap().len(), 1);

        // Complete it.
        apply_goal(
            &db,
            &sess,
            &json!({ "op": "update", "payload": { "id": goal.id, "status": "completed" } }),
        )
        .unwrap();
        assert_eq!(
            goals::get(&db, &goal.id).unwrap().unwrap().status,
            GoalStatus::Completed
        );
    }

    /// Seed a task for `sess` so a `task_grants` row (FK → tasks) can be
    /// inserted against it.
    fn seed_task_for(db: &copperclaw_db::central::CentralDb, sess: &Session, task_id: &str) {
        copperclaw_db::tables::tasks::insert(
            db,
            copperclaw_db::tables::tasks::NewTask {
                id: task_id.into(),
                agent_group_id: sess.agent_group_id,
                session_id: sess.id,
                name: Some("t".into()),
                prompt: "p".into(),
                when_spec: Utc::now().to_rfc3339(),
                recurrence: None,
                next_fire: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn grant_consume_decrements_fires_and_exhausts_grant() {
        use copperclaw_db::tables::task_grants::{self, NewTaskGrant};
        let (db, sess) = central_ag_session();
        seed_task_for(&db, &sess, "task_g");
        task_grants::insert_approved(
            &db,
            NewTaskGrant {
                id: "grant_1".into(),
                task_id: "task_g".into(),
                capability_scope: "web_fetch".into(),
                token_budget: Some(1000),
                max_fires: Some(2),
                expires_at: None,
                granted_by: None,
            },
        )
        .unwrap();

        // First fire (with a token spend) → one fire + 250 tokens consumed;
        // the grant is still live (1 fire, 750 tokens left).
        apply_grant_consume(
            &db,
            &json!({ "grant_id": "grant_1", "task_id": "task_g", "fires": 1, "tokens": 250 }),
        )
        .unwrap();
        let live = task_grants::effective_grant(&db, "task_g", Utc::now())
            .unwrap()
            .expect("grant still live after one fire");
        assert_eq!(live.fires_remaining(), Some(1));
        assert_eq!(live.tokens_remaining(), Some(750));

        // Second fire → fires exhausted → effective_grant reads inert (None),
        // which is exactly what makes the NEXT spawn's grant.json absent and the
        // runner's autonomy gate closed.
        apply_grant_consume(
            &db,
            &json!({ "grant_id": "grant_1", "task_id": "task_g", "fires": 1 }),
        )
        .unwrap();
        assert!(
            task_grants::effective_grant(&db, "task_g", Utc::now())
                .unwrap()
                .is_none(),
            "grant reads inert once max_fires is reached"
        );
    }

    #[test]
    fn grant_consume_defaults_fires_to_one_and_rejects_missing_grant_id() {
        use copperclaw_db::tables::task_grants::{self, NewTaskGrant};
        let (db, sess) = central_ag_session();
        seed_task_for(&db, &sess, "task_g");
        task_grants::insert_approved(
            &db,
            NewTaskGrant {
                id: "grant_1".into(),
                task_id: "task_g".into(),
                capability_scope: "web_fetch".into(),
                token_budget: None,
                max_fires: Some(3),
                expires_at: None,
                granted_by: None,
            },
        )
        .unwrap();

        // No explicit `fires` → defaults to 1.
        apply_grant_consume(&db, &json!({ "grant_id": "grant_1" })).unwrap();
        let live = task_grants::effective_grant(&db, "task_g", Utc::now())
            .unwrap()
            .unwrap();
        assert_eq!(live.fires_remaining(), Some(2));

        // A payload with no grant_id is a hard error (surfaces as a self-mod
        // failure, not a silent no-op).
        assert!(apply_grant_consume(&db, &json!({ "task_id": "task_g" })).is_err());
    }

    #[test]
    fn apply_goal_rejects_unknown_op_and_missing_objective() {
        let (db, sess) = central_ag_session();
        assert!(apply_goal(&db, &sess, &json!({ "op": "bogus", "payload": {} })).is_err());
        assert!(
            apply_goal(&db, &sess, &json!({ "op": "create", "payload": {} })).is_err(),
            "create without an objective is refused"
        );
    }

    #[test]
    fn apply_goal_update_illegal_transition_errors() {
        use copperclaw_db::tables::goals::{self, GoalStatus};
        let (db, sess) = central_ag_session();
        apply_goal(
            &db,
            &sess,
            &json!({ "op": "create", "payload": { "objective": "x" } }),
        )
        .unwrap();
        let id = goals::list_for_session(&db, sess.id).unwrap()[0].id.clone();
        goals::set_status(&db, &id, GoalStatus::Completed).unwrap();
        // Out of a terminal state is illegal → surfaces as an error the agent sees.
        assert!(
            apply_goal(
                &db,
                &sess,
                &json!({ "op": "update", "payload": { "id": id, "status": "active" } })
            )
            .is_err()
        );
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
                tool_profile: None,
                preview_enabled: false,
                preview_bind: None,
                check_command: None,
                verify_gate: true,
                image_profile: copperclaw_types::ImageProfile::Minimal,
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
                tool_profile: None,
                preview_enabled: false,
                preview_bind: None,
                check_command: None,
                verify_gate: true,
                image_profile: copperclaw_types::ImageProfile::Minimal,
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

    // ── M19 A4: save_skill raises an approval (does NOT write on its own) ──

    #[tokio::test]
    async fn save_skill_row_raises_pending_approval_with_dest_and_content() {
        let (service, tmp, sess, _mock) = make_service().await;
        let groups_dir = tmp.path().join("groups");
        service.set_groups_dir(groups_dir.clone());

        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let content = "---\nname: greet\ndescription: Say hi\n---\n# Greet\n";
        // A routed system row so the card has a target channel.
        let mut row = make_row(
            MessageKind::System,
            json!({ "save_skill": {"name": "greet", "content": content, "reason": "handy"} }),
        );
        row.channel_type = Some(ChannelType::new("mock"));
        row.platform_id = Some("plat-1".into());
        write_row(&out_pool, &row);

        let _ = service.process_session_once(&sess).await.unwrap();

        // A pending approval was raised (nothing written to disk yet).
        let rows = pending_approvals::list(service.central(), Some("save_skill"), None).unwrap();
        assert_eq!(rows.len(), 1, "expected one save_skill approval");
        let approval = &rows[0];
        assert_eq!(approval.agent_group_id, Some(sess.agent_group_id));
        assert_eq!(approval.payload["name"], "greet");
        assert_eq!(approval.payload["content"], content);
        let expected_dest = groups_dir
            .join(sess.agent_group_id.as_uuid().to_string())
            .join("skills");
        assert_eq!(
            approval.payload["dest_dir"],
            expected_dest.to_string_lossy().as_ref()
        );
        assert_eq!(
            approval.payload["allowed_root"],
            groups_dir.to_string_lossy().as_ref()
        );
        // The skill must NOT exist yet — approval gates the write. (The
        // approve→write→discovery half is covered end-to-end by the host's
        // approvals handler test `approve_save_skill_writes_and_is_discovered
        // _on_next_spawn`.) The approval card dispatch is best-effort and
        // fire-and-forget on a spawned task, so it isn't asserted here.
        assert!(!expected_dest.join("greet").exists());
    }

    #[tokio::test]
    async fn save_skill_row_without_groups_dir_surfaces_self_mod_error() {
        let (service, _tmp, sess, _mock) = make_service().await;
        // Deliberately do NOT set groups_dir.
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let content = "---\nname: greet\ndescription: Say hi\n---\nbody\n";
        let row = make_row(
            MessageKind::System,
            json!({ "save_skill": {"name": "greet", "content": content, "reason": "r"} }),
        );
        write_row(&out_pool, &row);

        let _ = service.process_session_once(&sess).await.unwrap();

        // No approval raised; the agent gets a self_mod_error explaining why.
        let rows = pending_approvals::list(service.central(), Some("save_skill"), None).unwrap();
        assert!(rows.is_empty());
        let in_pool = service
            .session_paths
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let sys_rows = list_inbound_system_rows(&in_pool);
        assert_eq!(sys_rows.len(), 1);
        assert_eq!(
            sys_rows[0]["content"]["self_mod_error"]["action"],
            "save_skill"
        );
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
        assert_eq!(
            sys_rows.len(),
            1,
            "expected one system row, got {sys_rows:?}"
        );
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
        assert!(
            cfg.is_none()
                || !cfg.unwrap().mcp_servers.is_object()
                || copperclaw_db::tables::container_configs::get_mcp_servers(&db, ag)
                    .map(|v| v.as_object().is_some_and(serde_json::Map::is_empty))
                    .unwrap_or(false)
        );
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
        let breadcrumb =
            copperclaw_channels_core::Breadcrumb::running("shell").with_detail("cargo check");
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
        let running =
            copperclaw_channels_core::Breadcrumb::running("shell").with_detail("cargo check");
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

    /// A breadcrumb in-place edit whose anchor chip is gone must re-post a
    /// fresh chip rather than fail the row — the same stale-anchor recovery as
    /// the todo card.
    #[tokio::test]
    async fn update_breadcrumb_recovers_from_stale_edit_anchor() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // Step 1: a Running chip, recorded in `delivered` with a platform id
        // (this becomes the edit anchor the update resolves to).
        let running =
            copperclaw_channels_core::Breadcrumb::running("shell").with_detail("cargo check");
        write_row(
            &out_pool,
            &make_row(
                MessageKind::Breadcrumb,
                json!({ "breadcrumb": serde_json::to_value(&running).unwrap() }),
            ),
        );
        let _ = service.process_session_once(&sess).await.unwrap();
        assert_eq!(mock.deliveries().len(), 1);

        // Step 2: the in-place edit fails as the platform does for a gone
        // message; the loop must re-post a fresh chip instead of failing.
        mock.fail_next_deliver(AdapterError::BadRequest(
            "Bad Request: message to edit not found".into(),
        ));
        let done = running.clone().finished(true, Some("passed".into()));
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
        let rpt = service.process_session_once(&sess).await.unwrap();
        assert_eq!(
            rpt.failed, 0,
            "a stale breadcrumb anchor must not fail the row"
        );
        assert_eq!(rpt.delivered, 1);
        // Fresh chip re-posted: a second delivery landed (the failed edit is
        // not recorded by the mock).
        assert_eq!(mock.deliveries().len(), 2);
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
            items: vec![copperclaw_channels_core::TodoListItem {
                id: 1,
                text: "Reply with order status".into(),
                status: copperclaw_channels_core::TodoItemStatus::Pending,
                blocked_reason: None,
            }],
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

    #[test]
    fn stale_edit_target_matches_platform_phrasings() {
        for s in [
            "Bad Request: message to edit not found",
            "Bad Request: message can't be edited",
            "message_not_found",
            "cant_update_message",
            "Unknown Message",
            "MESSAGE_ID_INVALID",
        ] {
            assert!(
                is_stale_edit_target(s),
                "should match stale-edit-target: {s}"
            );
        }
        for s in [
            "Bad Request: can't parse entities",
            "chat_id is empty",
            "rate limited",
            "",
        ] {
            assert!(!is_stale_edit_target(s), "should NOT match: {s}");
        }
    }

    /// Headline resilience case: a todo/plan card whose pinned anchor is gone
    /// (deleted, too old, or survived a host restart) must NOT fail the row
    /// forever. The delivery loop drops the dead anchor and re-posts a fresh
    /// card, so the plan keeps updating.
    #[tokio::test]
    async fn todo_list_stale_edit_anchor_reposts_fresh_card() {
        let (service, _root, sess, mock) = make_service().await;
        let out_pool = service
            .session_paths
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        // A stale anchor: the previously-pinned card no longer exists.
        service
            .todo_anchors
            .insert(sess.id, "stale-old-card".to_string());
        // The first (edit) deliver fails exactly as Telegram does for a gone
        // message; the fresh re-post that follows succeeds (queue is empty).
        mock.fail_next_deliver(AdapterError::BadRequest(
            "Bad Request: message to edit not found".into(),
        ));
        let list = build_dispatch_todo_list();
        let content = json!({ "todo_list": serde_json::to_value(&list).unwrap() });
        write_row(&out_pool, &make_row(MessageKind::TodoList, content));

        let rpt = service.process_session_once(&sess).await.unwrap();
        // Recovered — the row delivered via a fresh card, not marked failed.
        assert_eq!(rpt.failed, 0, "a stale anchor must not fail the row");
        assert_eq!(rpt.delivered, 1);
        // Exactly one delivery landed: the failed edit isn't recorded, the
        // fresh re-post is.
        assert_eq!(mock.deliveries().len(), 1);
        // The dead anchor was dropped and replaced with the fresh card's id.
        let anchor = service.todo_anchors.get(&sess.id).map(|r| r.clone());
        assert!(anchor.is_some(), "a fresh anchor must be recorded");
        assert_ne!(
            anchor.as_deref(),
            Some("stale-old-card"),
            "the stale anchor must be replaced"
        );
    }

    #[test]
    fn aggregate_status_rolls_up_child_items() {
        use copperclaw_channels_core::{TodoItemStatus as S, TodoListItem as Item};
        let it = |s| Item {
            id: 0,
            text: "x".into(),
            status: s,
            blocked_reason: None,
        };
        assert_eq!(aggregate_status(&[]), S::Pending);
        assert_eq!(
            aggregate_status(&[it(S::Pending), it(S::Pending)]),
            S::Pending
        );
        assert_eq!(
            aggregate_status(&[it(S::Completed), it(S::Completed)]),
            S::Completed
        );
        // Mixed (some done, some not) and any-in-progress both read as work
        // underway.
        assert_eq!(
            aggregate_status(&[it(S::Completed), it(S::Pending)]),
            S::InProgress
        );
        assert_eq!(aggregate_status(&[it(S::InProgress)]), S::InProgress);
        // F4: a blocked item wins over in-progress/pending — the child is
        // stalled, and the rolled-up header must say so.
        assert_eq!(
            aggregate_status(&[it(S::InProgress), it(S::Blocked)]),
            S::Blocked
        );
        assert_eq!(aggregate_status(&[it(S::Blocked)]), S::Blocked);
    }

    #[test]
    fn build_combined_nests_children_with_unique_ids() {
        use copperclaw_channels_core::{TodoItemStatus as S, TodoList, TodoListItem as Item};
        let root = TodoList {
            title: Some("Plan".into()),
            items: vec![Item {
                id: 1,
                text: "Spawn A".into(),
                status: S::InProgress,
                blocked_reason: None,
            }],
        };
        let child = TodoList {
            title: None,
            items: vec![
                Item {
                    id: 1,
                    text: "Read".into(),
                    status: S::Completed,
                    blocked_reason: None,
                },
                Item {
                    id: 2,
                    text: "Write".into(),
                    status: S::InProgress,
                    blocked_reason: None,
                },
            ],
        };
        let out = build_combined(Some(root), &[("builder-A".to_string(), child)]);
        // Root title is preserved; child folds in as a labeled, indented section.
        assert_eq!(out.title.as_deref(), Some("Plan"));
        let texts: Vec<&str> = out.items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Spawn A", "↳ builder-A", "    Read", "    Write"]
        );
        // The header carries the child's aggregate status (mixed → InProgress).
        assert_eq!(out.items[1].status, S::InProgress);
        // Ids stay unique across the combined list (renderers key on id).
        let ids: std::collections::HashSet<u32> = out.items.iter().map(|i| i.id).collect();
        assert_eq!(ids.len(), out.items.len());
    }

    #[test]
    fn build_combined_empty_when_nothing_to_show() {
        assert!(build_combined(None, &[]).items.is_empty());
    }

    /// End-to-end: a parent's TodoList row renders ONE card that folds each
    /// live `create_agent` child's plan in as a labeled section — the
    /// "single pinned plan, children rolled up" behaviour.
    #[tokio::test]
    async fn todo_list_rolls_up_child_plans_into_one_card() {
        use copperclaw_channels_core::{TodoItemStatus, TodoList, TodoListItem};
        let (service, _root, parent, mock) = make_service().await;
        let central = service.central();

        // A create_agent child: its own agent group, recording the parent.
        let child_ag = copperclaw_db::tables::agent_groups::create(
            central,
            copperclaw_db::tables::agent_groups::CreateAgentGroup {
                name: "builder-A".into(),
                folder: "builder-a".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let child_sess = copperclaw_db::tables::sessions::create(
            central,
            copperclaw_db::tables::sessions::CreateSession {
                agent_group_id: child_ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: Some(parent.id),
            },
        )
        .unwrap();

        let to_content =
            |list: &TodoList| json!({ "todo_list": serde_json::to_value(list).unwrap() });

        // Child emits its plan into its own outbound.
        let child_list = TodoList {
            title: None,
            items: vec![TodoListItem {
                id: 1,
                text: "Implement feature".into(),
                status: TodoItemStatus::InProgress,
                blocked_reason: None,
            }],
        };
        let child_pool = service
            .session_paths()
            .outbound_pool(&child_ag.id, &child_sess.id)
            .unwrap();
        write_row(
            &child_pool,
            &make_row(MessageKind::TodoList, to_content(&child_list)),
        );

        // Parent emits its plan and is processed.
        let parent_list = TodoList {
            title: None,
            items: vec![TodoListItem {
                id: 1,
                text: "Spawn builder".into(),
                status: TodoItemStatus::InProgress,
                blocked_reason: None,
            }],
        };
        let parent_pool = service
            .session_paths()
            .outbound_pool(&parent.agent_group_id, &parent.id)
            .unwrap();
        write_row(
            &parent_pool,
            &make_row(MessageKind::TodoList, to_content(&parent_list)),
        );

        let rpt = service.process_session_once(&parent).await.unwrap();
        assert_eq!(rpt.delivered, 1);
        assert_eq!(rpt.failed, 0);

        // One delivered card containing the parent step AND the child section.
        let deliveries = mock.deliveries();
        let text = deliveries
            .last()
            .unwrap()
            .message
            .content
            .get("text")
            .and_then(|v| v.as_str())
            .expect("text fallback");
        assert!(
            text.contains("Spawn builder"),
            "parent step missing: {text}"
        );
        assert!(
            text.contains("\u{21b3} builder-A"),
            "child section header missing: {text}"
        );
        assert!(
            text.contains("Implement feature"),
            "child item missing: {text}"
        );
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
        let row = make_row(MessageKind::Chat, json!({"text": three_chunk_text()}));
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
        let row = make_row(MessageKind::Chat, json!({"text": three_chunk_text()}));
        write_row(&out_pool, &row);

        // The wrapper sees three `deliver` calls per attempt (one per
        // chunk). On attempt 1: call 0 = chunk 0 (let through), call 1 =
        // chunk 1 (fail Rate). On attempt 2: call 2 = chunk 1 (let
        // through), call 3 = chunk 2 (let through). Schedule the chunk-1
        // failure at wrapper call index 1.
        wrapper.fail_at_call(
            1,
            AdapterError::Rate {
                retry_after: Some(1),
            },
        );

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
        let row = make_row(MessageKind::Chat, json!({"text": three_chunk_text()}));
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
        assert_eq!(
            chunk1_count, 0,
            "chunk 1 should never have reached the inner mock"
        );

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
        let row = make_row(MessageKind::Chat, json!({"text": three_chunk_text()}));
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

    // ── external MCP host-proxy: rendering + missing-server path ──────────────

    #[test]
    fn render_mcp_content_flattens_text_blocks() {
        let content = serde_json::json!([
            {"type": "text", "text": "line one"},
            {"type": "text", "text": "line two"}
        ]);
        assert_eq!(render_mcp_content(Some(&content)), "line one\n\nline two");
    }

    #[test]
    fn render_mcp_content_tags_non_text_blocks() {
        let content = serde_json::json!([
            {"type": "text", "text": "see attached"},
            {"type": "image", "data": "..."}
        ]);
        assert_eq!(
            render_mcp_content(Some(&content)),
            "see attached\n\n<image>"
        );
    }

    #[test]
    fn render_mcp_content_handles_empty_and_missing() {
        assert_eq!(
            render_mcp_content(None),
            "(external MCP tool produced no output)"
        );
        assert_eq!(
            render_mcp_content(Some(&serde_json::json!([]))),
            "(external MCP tool produced no output)"
        );
    }

    #[tokio::test]
    async fn execute_mcp_call_errors_when_server_is_not_configured() {
        // The runner named a server the group's config doesn't have — every
        // failure mode lands as an is_error response so the runner's poll is
        // never left unanswered.
        let servers = serde_json::json!({"weather": {"command": "x"}});
        let req = mcp_calls::McpCallRequest {
            request_id: "r1".into(),
            server: "ghost".into(),
            tool: "forecast".into(),
            input: serde_json::json!({}),
        };
        let resp = DeliveryService::execute_mcp_call(&servers, &req, "test-scope-r1").await;
        assert!(resp.is_error);
        assert!(resp.result.contains("ghost"), "got: {}", resp.result);
        assert_eq!(resp.request_id, "r1");
    }

    #[tokio::test]
    async fn execute_mcp_call_errors_when_connect_fails() {
        // A configured-but-unreachable stdio server surfaces a connect failure
        // as an is_error response naming the tool + server.
        let servers = serde_json::json!({
            "weather": {"command": "/no/such/binary-xyz-copperclaw-test"}
        });
        let req = mcp_calls::McpCallRequest {
            request_id: "r2".into(),
            server: "weather".into(),
            tool: "forecast".into(),
            input: serde_json::json!({}),
        };
        let resp = DeliveryService::execute_mcp_call(&servers, &req, "test-scope-r2").await;
        assert!(resp.is_error);
        assert!(resp.result.contains("forecast"), "got: {}", resp.result);
    }

    #[tokio::test]
    async fn drain_mcp_calls_spawns_and_writes_a_response() {
        // End-to-end host side: a request in outbound.db is drained off the
        // critical path (detached task) and an is_error response appears in
        // inbound.db — here the named server isn't configured for the group.
        let (service, _root, sess, _mock) = make_service().await;
        let out_pool = service
            .session_paths()
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_pool = service
            .session_paths()
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        {
            let out = out_pool.connect().unwrap();
            mcp_calls::insert_request(
                &out,
                &mcp_calls::McpCallRequest {
                    request_id: "rq".into(),
                    server: "ghost".into(),
                    tool: "t".into(),
                    input: serde_json::json!({}),
                },
            )
            .unwrap();
        }

        service.drain_mcp_calls(&sess, &out_pool, &in_pool).unwrap();

        // The call runs detached; poll inbound.db for the response row.
        let mut resp = None;
        for _ in 0..200 {
            let inb = in_pool.connect().unwrap();
            if let Some(r) = mcp_calls::get_response(&inb, "rq").unwrap() {
                resp = Some(r);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let resp = resp.expect("host drain must write a response");
        assert!(resp.is_error);
        assert!(resp.result.contains("ghost"), "got: {}", resp.result);
    }

    // ── M17 preview relay: `__preview` reserved server routing ───────────────

    use copperclaw_modules::{PreviewError, PreviewExposed};

    /// A mock preview broker recording its calls and returning a canned result.
    struct MockPreviewBroker {
        expose_result: Result<PreviewExposed, PreviewError>,
        calls: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl PreviewBroker for MockPreviewBroker {
        async fn expose(
            &self,
            _session: &SessionInfoLite,
            port: u16,
            name: Option<String>,
        ) -> Result<PreviewExposed, PreviewError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("expose:{port}:{}", name.unwrap_or_default()));
            self.expose_result.clone()
        }
        async fn close(&self, _session_id: SessionId, port: u16) -> Result<(), PreviewError> {
            self.calls.lock().unwrap().push(format!("close:{port}"));
            Ok(())
        }
    }

    fn preview_req(tool: &str, input: serde_json::Value) -> mcp_calls::McpCallRequest {
        mcp_calls::McpCallRequest {
            request_id: "pv".into(),
            server: PREVIEW_SERVER.into(),
            tool: tool.into(),
            input,
        }
    }

    #[tokio::test]
    async fn preview_call_without_broker_is_error() {
        let req = preview_req("expose_preview", json!({ "port": 3000 }));
        let resp = DeliveryService::execute_preview_call(
            None,
            None,
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(resp.is_error);
        assert!(resp.result.contains("not available on this host"));
    }

    #[tokio::test]
    async fn preview_expose_routes_to_broker_and_renders_url() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let broker: Arc<dyn PreviewBroker> = Arc::new(MockPreviewBroker {
            expose_result: Ok(PreviewExposed {
                url: "http://192.168.1.9:8100/__preview/tok".into(),
                note: "Valid until idle for 30 minutes.".into(),
            }),
            calls: Arc::clone(&calls),
        });
        let req = preview_req("expose_preview", json!({ "port": 3000, "name": "demo" }));
        let resp = DeliveryService::execute_preview_call(
            Some(&broker),
            None,
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(!resp.is_error);
        assert!(
            resp.result
                .contains("http://192.168.1.9:8100/__preview/tok")
        );
        assert!(resp.result.contains("30 minutes"));
        assert_eq!(calls.lock().unwrap().as_slice(), ["expose:3000:demo"]);
    }

    #[tokio::test]
    async fn preview_expose_broker_error_is_surfaced() {
        let broker: Arc<dyn PreviewBroker> = Arc::new(MockPreviewBroker {
            expose_result: Err(PreviewError::Disabled {
                group: "ag-1".into(),
            }),
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let req = preview_req("expose_preview", json!({ "port": 3000 }));
        let resp = DeliveryService::execute_preview_call(
            Some(&broker),
            None,
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(resp.is_error);
        assert!(resp.result.contains("preview_enabled=true ag-1"));
    }

    #[tokio::test]
    async fn preview_missing_port_is_error() {
        let broker: Arc<dyn PreviewBroker> = Arc::new(MockPreviewBroker {
            expose_result: Ok(PreviewExposed {
                url: "x".into(),
                note: "y".into(),
            }),
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let req = preview_req("expose_preview", json!({}));
        let resp = DeliveryService::execute_preview_call(
            Some(&broker),
            None,
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(resp.is_error);
        assert!(resp.result.contains("integer `port`"));
    }

    #[tokio::test]
    async fn preview_unknown_tool_is_error() {
        let broker: Arc<dyn PreviewBroker> = Arc::new(MockPreviewBroker {
            expose_result: Ok(PreviewExposed {
                url: "x".into(),
                note: "y".into(),
            }),
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let req = preview_req("frobnicate", json!({ "port": 1 }));
        let resp = DeliveryService::execute_preview_call(
            Some(&broker),
            None,
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(resp.is_error);
        assert!(resp.result.contains("Unknown preview action"));
    }

    /// A mock public-tunnel broker returning a canned reply and recording the
    /// container port it was asked to make public (M19 A3).
    struct MockTunnelBroker {
        reply: PublicTunnelReply,
        calls: Arc<StdMutex<Vec<u16>>>,
    }

    #[async_trait::async_trait]
    impl PublicTunnelBroker for MockTunnelBroker {
        async fn make_public(
            &self,
            _session: SessionInfoLite,
            container_port: u16,
        ) -> PublicTunnelReply {
            self.calls.lock().unwrap().push(container_port);
            self.reply.clone()
        }
    }

    #[tokio::test]
    async fn make_public_without_tunnel_broker_is_error() {
        // The `__preview` relay is present but no tunnel broker is wired: a
        // `make_preview_public` gets a clean is_error, never a hang.
        let broker: Arc<dyn PreviewBroker> = Arc::new(MockPreviewBroker {
            expose_result: Ok(PreviewExposed {
                url: "x".into(),
                note: "y".into(),
            }),
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let req = preview_req("make_preview_public", json!({ "port": 3000 }));
        let resp = DeliveryService::execute_preview_call(
            Some(&broker),
            None,
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(resp.is_error);
        assert!(resp.result.contains("Public tunnels are not available"));
    }

    #[tokio::test]
    async fn make_public_pending_is_not_an_error() {
        // First call → pending approval. That is a successful (non-error) result
        // the agent acts on (wait for the tap), not a failure.
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let tunnel: Arc<dyn PublicTunnelBroker> = Arc::new(MockTunnelBroker {
            reply: PublicTunnelReply::Pending {
                note: "Public exposure needs operator approval. A card was sent.".into(),
            },
            calls: Arc::clone(&calls),
        });
        let req = preview_req("make_preview_public", json!({ "port": 8100 }));
        let resp = DeliveryService::execute_preview_call(
            None,
            Some(&tunnel),
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(!resp.is_error);
        assert!(resp.result.contains("needs operator approval"));
        assert_eq!(calls.lock().unwrap().as_slice(), [8100]);
    }

    #[tokio::test]
    async fn make_public_exposed_renders_url_and_note() {
        // After approval the broker returns the shareable PUBLIC url + caveat.
        let tunnel: Arc<dyn PublicTunnelBroker> = Arc::new(MockTunnelBroker {
            reply: PublicTunnelReply::Exposed {
                public_url: "https://cofounder-demo.trycloudflare.com/__preview/tok".into(),
                note:
                    "This is a PUBLIC link — anyone on the internet who has it can reach this app."
                        .into(),
            },
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let req = preview_req("make_preview_public", json!({ "port": 8100 }));
        let resp = DeliveryService::execute_preview_call(
            None,
            Some(&tunnel),
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(!resp.is_error);
        assert!(resp.result.contains("trycloudflare.com/__preview/tok"));
        assert!(resp.result.contains("PUBLIC link"));
    }

    #[tokio::test]
    async fn make_public_broker_error_is_surfaced() {
        let tunnel: Arc<dyn PublicTunnelBroker> = Arc::new(MockTunnelBroker {
            reply: PublicTunnelReply::Error(
                "No live preview on port 8100. Run `expose_preview` first.".into(),
            ),
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let req = preview_req("make_preview_public", json!({ "port": 8100 }));
        let resp = DeliveryService::execute_preview_call(
            None,
            Some(&tunnel),
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(resp.is_error);
        assert!(resp.result.contains("No live preview"));
    }

    #[tokio::test]
    async fn make_public_missing_port_is_error() {
        let tunnel: Arc<dyn PublicTunnelBroker> = Arc::new(MockTunnelBroker {
            reply: PublicTunnelReply::Pending { note: "x".into() },
            calls: Arc::new(StdMutex::new(Vec::new())),
        });
        let req = preview_req("make_preview_public", json!({}));
        let resp = DeliveryService::execute_preview_call(
            None,
            Some(&tunnel),
            SessionInfoLite::new(SessionId::new(), AgentGroupId::new()),
            &req,
        )
        .await;
        assert!(resp.is_error);
        assert!(resp.result.contains("integer `port`"));
    }

    #[test]
    fn parse_preview_port_bounds() {
        assert_eq!(parse_preview_port(&json!({ "port": 8080 })), Some(8080));
        assert_eq!(parse_preview_port(&json!({ "port": 1 })), Some(1));
        assert_eq!(parse_preview_port(&json!({ "port": 65535 })), Some(65535));
        assert_eq!(parse_preview_port(&json!({ "port": 0 })), None);
        assert_eq!(parse_preview_port(&json!({ "port": 65536 })), None);
        assert_eq!(parse_preview_port(&json!({ "port": "80" })), None);
        assert_eq!(parse_preview_port(&json!({})), None);
    }

    #[tokio::test]
    async fn drain_routes_preview_request_to_wired_broker() {
        // End-to-end through drain_mcp_calls: a `__preview` request row is
        // serviced by the wired broker, and the response lands in inbound.db.
        let (service, _root, sess, _mock) = make_service().await;
        let calls = Arc::new(StdMutex::new(Vec::new()));
        service.set_preview_broker(Arc::new(MockPreviewBroker {
            expose_result: Ok(PreviewExposed {
                url: "http://127.0.0.1:8100/__preview/abc".into(),
                note: "note".into(),
            }),
            calls: Arc::clone(&calls),
        }));

        let out_pool = service
            .session_paths()
            .outbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        let in_pool = service
            .session_paths()
            .inbound_pool(&sess.agent_group_id, &sess.id)
            .unwrap();
        {
            let out = out_pool.connect().unwrap();
            mcp_calls::insert_request(
                &out,
                &preview_req("expose_preview", json!({ "port": 5000 })),
            )
            .unwrap();
        }

        service.drain_mcp_calls(&sess, &out_pool, &in_pool).unwrap();

        let mut resp = None;
        for _ in 0..200 {
            let inb = in_pool.connect().unwrap();
            if let Some(r) = mcp_calls::get_response(&inb, "pv").unwrap() {
                resp = Some(r);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let resp = resp.expect("host drain must answer the preview request");
        assert!(!resp.is_error, "got: {}", resp.result);
        assert!(resp.result.contains("http://127.0.0.1:8100/__preview/abc"));
        assert_eq!(calls.lock().unwrap().as_slice(), ["expose:5000:"]);
    }
}
