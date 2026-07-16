//! Session-preview reverse proxy manager (M17).
//!
//! A session container runs on the Docker bridge with no published ports, but
//! the host can reach the container's bridge IP directly. This module lets an
//! in-container agent expose an HTTP app it built (listening on
//! `0.0.0.0:<port>` INSIDE the container) to the operator's machine / LAN for
//! hands-on testing.
//!
//! ## Flow
//!
//! The agent calls the first-party `expose_preview` tool. That tool call rides
//! the external-MCP host-broker relay (reserved server name `__preview`); the
//! delivery loop routes it to [`PreviewManager`] (wired as a
//! [`copperclaw_modules::PreviewBroker`]). [`PreviewManager::expose`]:
//!
//! 1. checks the group opted into previews (`container_configs.preview_enabled`);
//! 2. resolves the container's bridge IP (`ContainerRuntime::container_ip`);
//! 3. allocates a free host port from [`PREVIEW_PORT_MIN`]..=[`PREVIEW_PORT_MAX`];
//! 4. mints a 128-bit token and stands up a token-gated axum reverse proxy from
//!    `bind:host_port` to `http://container_ip:container_port`;
//! 5. audits the exposure and returns the shareable tokened URL.
//!
//! ## Token gate
//!
//! * `GET /__preview/<token>` sets `Set-Cookie: cclaw_preview=<token>; …` and
//!   302-redirects to `/`.
//! * Every other request must present the matching cookie, else 403.
//! * Valid requests are proxied upstream, hop-by-hop headers stripped, bodies
//!   streamed both ways.
//! * A WebSocket upgrade is bridged (V1): the axum side completes the upgrade
//!   with the browser and a `tokio-tungstenite` client opens `ws://` to the
//!   container app, forwarding frames both ways. An open socket (and any frame
//!   on it) counts as activity for the idle reaper.
//!
//! ## Lifecycle
//!
//! Each preview tracks a last-activity timestamp; a background reaper tears it
//! down after [`PREVIEW_IDLE_TIMEOUT`] of inactivity. Teardown also fires on an
//! explicit `close_preview`, when the session's container stops / is removed
//! (the container manager calls [`PreviewManager::close_all_for_session`]), and
//! on host shutdown (the reaper loop's [`CancellationToken`] cancels every
//! live preview). Every teardown writes an audit row with the reason.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{
    CloseFrame as AxumCloseFrame, Message as AxumWsMessage, WebSocket, WebSocketUpgrade,
};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use copperclaw_container_rt::ContainerRuntime;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log::{self, AuditEntry};
use copperclaw_db::tables::pending_approvals::{self, ApprovalStatus, UpsertPendingApproval};
use copperclaw_db::tables::{container_configs, messaging_group_agents, messaging_groups};
use copperclaw_modules::{
    DeliveryDispatcher, DispatchTarget, PreviewBroker, PreviewError, PreviewExposed,
    SessionInfoLite,
};
use copperclaw_types::{AgentGroupId, MessageKind, OutboundMessage, SessionId};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{Duration, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TungsteniteCloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Lowest host port the preview proxy may bind (inclusive).
pub const PREVIEW_PORT_MIN: u16 = 8100;
/// Highest host port the preview proxy may bind (inclusive).
pub const PREVIEW_PORT_MAX: u16 = 8199;
/// Cap on concurrent previews for a single session.
pub const PREVIEW_MAX_PER_SESSION: usize = 4;
/// Cap on concurrent previews across the whole host.
pub const PREVIEW_MAX_PER_HOST: usize = 16;
/// Idle lifetime after which the reaper tears a preview down.
pub const PREVIEW_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How often the reaper scans for idle previews.
pub const PREVIEW_REAPER_TICK: Duration = Duration::from_secs(60);
/// While a WebSocket bridge is open, `last_activity` is bumped at least this
/// often even with no frame traffic — so the idle reaper treats an open socket
/// as an in-use preview. Must stay well under [`PREVIEW_IDLE_TIMEOUT`].
const PREVIEW_WS_ACTIVITY_TICK: Duration = Duration::from_secs(60);
/// Cookie name the token gate sets / checks.
const PREVIEW_COOKIE: &str = "cclaw_preview";
/// URL path prefix that mints the gating cookie from a token.
const PREVIEW_TOKEN_PATH: &str = "/__preview/";
/// Audit command recorded for preview mutations.
const PREVIEW_AUDIT_COMMAND: &str = "preview";
/// Title shown on the one-tap enable-preview approval card (V2).
const ENABLE_PREVIEW_CARD_TITLE: &str = "Enable previews for this group?";

/// Why a preview was torn down — recorded in the audit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeardownReason {
    /// The agent called `close_preview`.
    Closed,
    /// The reaper tore it down after the idle timeout.
    Idle,
    /// The session's container stopped or was removed.
    SessionStop,
    /// The host is shutting down.
    Shutdown,
}

impl TeardownReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Idle => "idle",
            Self::SessionStop => "session-stop",
            Self::Shutdown => "shutdown",
        }
    }
}

/// One live (or idle-tombstoned) preview.
struct PreviewEntry {
    session_id: SessionId,
    agent_group_id: AgentGroupId,
    /// The port the app listens on INSIDE the container.
    container_port: u16,
    /// The port the proxy binds on the host.
    host_port: u16,
    token: String,
    name: Option<String>,
    container_ip: String,
    /// Last time a valid proxied request was seen (tokio clock, so a
    /// paused-clock test can advance it deterministically).
    last_activity: Arc<StdMutex<Instant>>,
    /// Cancels this preview's axum server task on teardown.
    cancel: CancellationToken,
    /// The shared proxy state the axum serving task holds. Kept here too so the
    /// idle reaper can flip it to [`PreviewPhase::Tombstone`] WITHOUT dropping
    /// the listener (V2): the bound port keeps serving an "expired" page that
    /// offers a single tokened re-expose.
    proxy: Arc<ProxyState>,
}

/// Which phase a preview's bound host port is serving (V2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewPhase {
    /// Reverse-proxying live to the container upstream.
    Live,
    /// Idle-reaped: the upstream proxying is torn down (freeing the
    /// container-side resources the reaper exists to reclaim) but the port
    /// stays bound, serving the static "preview expired" tombstone page. A
    /// tokened `GET /__preview/<token>` re-exposes the same session:port once.
    Tombstone,
}

/// Mutable, lock-guarded part of one preview's proxy state. Shared between the
/// axum serving task, the idle reaper (which flips `phase` to `Tombstone`), and
/// the tombstone recovery path (which flips it back to `Live`).
struct ProxyStateInner {
    phase: PreviewPhase,
    /// `http://<container_ip>:<container_port>` — the upstream base. Only
    /// meaningful while `phase == Live`; re-resolved on a tombstone recovery
    /// (the container IP may have changed across a restart).
    upstream: String,
    /// A tombstone offers exactly ONE tokened re-expose per token. Once a
    /// recovery succeeds this is set; a subsequent reap→tombstone→GET then
    /// shows the terminal "ask the agent to re-expose" page rather than
    /// recovering a second time. An explicit agent `expose_preview` re-lease
    /// resets it (a fresh, deliberate exposure earns a fresh recovery budget).
    recovery_used: bool,
}

/// Shared state handed to the axum proxy handler for one preview. The static
/// fields are the token gate + HTTP client; [`ProxyStateInner`] holds the
/// phase-dependent bits. The recovery inputs (`runtime`, `central`, identity)
/// let a tombstone re-expose the same session:port without going back through
/// [`PreviewManager`].
struct ProxyState {
    token: String,
    last_activity: Arc<StdMutex<Instant>>,
    client: reqwest::Client,
    inner: StdMutex<ProxyStateInner>,
    runtime: Arc<dyn ContainerRuntime>,
    central: CentralDb,
    session_id: SessionId,
    agent_group_id: AgentGroupId,
    /// `copperclaw-<session-uuid>` — the container the reaper freed the proxy
    /// to, re-resolved on recovery to confirm it is still up.
    container_name: String,
    container_port: u16,
    host_port: u16,
}

impl ProxyState {
    /// Current phase (cheap read).
    fn phase(&self) -> PreviewPhase {
        self.inner.lock().unwrap().phase
    }

    /// The live upstream base (`http://<ip>:<port>`). Snapshot-cloned so the
    /// caller doesn't hold the inner lock across an await.
    fn upstream(&self) -> String {
        self.inner.lock().unwrap().upstream.clone()
    }

    /// Idle-reap this preview: drop the upstream proxying and leave the port
    /// serving the tombstone page. A no-op if it is already tombstoned. Does
    /// NOT cancel the listener or release the port — that stays for
    /// session-stop / close / shutdown teardown.
    fn tombstone(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.phase == PreviewPhase::Live {
            inner.phase = PreviewPhase::Tombstone;
            copperclaw_metrics::inc_preview_tombstoned();
        }
    }

    /// Attempt the single tokened re-expose a tombstone allows. Returns `true`
    /// when the preview is Live afterwards (either it just recovered, or a
    /// racing request already recovered it); `false` for the terminal case
    /// (recovery already used, or the container is no longer up). On a genuine
    /// recovery this re-resolves the container IP, flips to `Live`, bumps the
    /// idle clock, and writes the same audit row a fresh expose does.
    async fn try_recover(&self) -> bool {
        {
            let inner = self.inner.lock().unwrap();
            if inner.phase == PreviewPhase::Live {
                return true;
            }
            if inner.recovery_used {
                copperclaw_metrics::inc_preview_tombstone_recovery("terminal_spent");
                return false;
            }
        }
        // Container still up? (async — must not hold the inner lock across it.)
        let Ok(Some(ip)) = self.runtime.container_ip(&self.container_name).await else {
            copperclaw_metrics::inc_preview_tombstone_recovery("terminal_container_gone");
            return false;
        };
        let mut inner = self.inner.lock().unwrap();
        if inner.phase == PreviewPhase::Live {
            return true;
        }
        if inner.recovery_used {
            copperclaw_metrics::inc_preview_tombstone_recovery("terminal_spent");
            return false;
        }
        inner.recovery_used = true;
        inner.upstream = format!("http://{ip}:{}", self.container_port);
        inner.phase = PreviewPhase::Live;
        drop(inner);
        copperclaw_metrics::inc_preview_tombstone_recovery("recovered");
        copperclaw_metrics::dec_preview_tombstoned();
        *self.last_activity.lock().unwrap() = Instant::now();
        write_preview_audit(
            &self.central,
            self.session_id,
            self.agent_group_id,
            "expose",
            Some(self.host_port),
            self.container_port,
            "reopened",
        );
        true
    }

    /// Agent-initiated re-lease of a tombstoned preview (an explicit
    /// `expose_preview` on a port whose proxy was idle-reaped). Re-resolves the
    /// container IP, flips back to `Live`, and resets the one-shot recovery
    /// budget. Best-effort: returns `false` (leaving it tombstoned) when the
    /// container is gone, in which case the shareable URL still works via the
    /// tombstone's own recovery path.
    async fn revive(&self) -> bool {
        let Ok(Some(ip)) = self.runtime.container_ip(&self.container_name).await else {
            return false;
        };
        {
            let mut inner = self.inner.lock().unwrap();
            let was_tombstone = inner.phase == PreviewPhase::Tombstone;
            inner.recovery_used = false;
            inner.upstream = format!("http://{ip}:{}", self.container_port);
            inner.phase = PreviewPhase::Live;
            if was_tombstone {
                copperclaw_metrics::dec_preview_tombstoned();
            }
        }
        *self.last_activity.lock().unwrap() = Instant::now();
        write_preview_audit(
            &self.central,
            self.session_id,
            self.agent_group_id,
            "expose",
            Some(self.host_port),
            self.container_port,
            "reopened",
        );
        true
    }
}

/// The live proxy backing a `(session, container_port)` preview: the host port
/// it is bound to and the token that gates it. Returned by
/// [`PreviewManager::live_proxy_for`] so the M19 A3 public-tunnel broker can
/// front the preview's host port and compose the tokened public URL. Only ever
/// describes a [`PreviewPhase::Live`] preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePreviewProxy {
    /// The host port the preview proxy is bound to (what the public tunnel
    /// fronts, and the tunnel's teardown key).
    pub host_port: u16,
    /// The 128-bit hex gating token. The public tunnel fronts the token-gated
    /// proxy, so the shareable public URL must carry `.../__preview/<token>` —
    /// public exposure stays token-gated (defence in depth).
    pub token: String,
}

/// The host-side preview manager. Constructed once at boot with the container
/// runtime + central DB; wired into the delivery service as a
/// [`PreviewBroker`] and into the container manager for session-stop teardown.
pub struct PreviewManager {
    central: CentralDb,
    runtime: Arc<dyn ContainerRuntime>,
    entries: AsyncMutex<Vec<PreviewEntry>>,
    /// Delivery dispatcher, wired at boot via
    /// [`PreviewManager::set_approval_dispatcher`]. Used to post the one-tap
    /// "Enable previews for this group" approval card (V2) when an
    /// `expose_preview` hits [`PreviewError::Disabled`]. `None` (never wired —
    /// e.g. a host with no delivery loop, or the unit tests) means the disabled
    /// path stays a plain error with no card, exactly as before V2.
    approval_dispatcher: OnceLock<Arc<dyn DeliveryDispatcher>>,
    /// V5 public-tunnel broker (M19 A3), wired at boot via
    /// [`PreviewManager::set_tunnel_broker`]. When a preview is fully torn down
    /// (close / session-stop / shutdown) OR idle-tombstoned, the manager asks
    /// this broker to tear down any public tunnel fronting the preview's host
    /// port — "auto-teardown with the preview" so no public tunnel outlives the
    /// app it fronted. `None` (never wired — a host without public-tunnel
    /// support, or the unit tests) means there is nothing to tear down.
    tunnel_broker: OnceLock<Arc<copperclaw_modules::TunnelBroker>>,
}

impl PreviewManager {
    /// Construct a manager. The reaper is started separately via
    /// [`PreviewManager::spawn_reaper`].
    #[must_use]
    pub fn new(central: CentralDb, runtime: Arc<dyn ContainerRuntime>) -> Arc<Self> {
        Arc::new(Self {
            central,
            runtime,
            entries: AsyncMutex::new(Vec::new()),
            approval_dispatcher: OnceLock::new(),
            tunnel_broker: OnceLock::new(),
        })
    }

    /// Wire the delivery dispatcher used to post the one-tap enable-preview
    /// approval card (V2). Called once at boot after the delivery service is
    /// up. Returns `false` if a dispatcher was already set (idempotent guard).
    pub fn set_approval_dispatcher(&self, dispatcher: Arc<dyn DeliveryDispatcher>) -> bool {
        self.approval_dispatcher.set(dispatcher).is_ok()
    }

    /// Wire the V5 public-tunnel broker (M19 A3) so preview teardown also tears
    /// down any public tunnel fronting the preview. Called once at boot; a
    /// second call is a no-op (the `OnceLock` keeps the first). Returns whether
    /// it was set.
    pub fn set_tunnel_broker(&self, broker: Arc<copperclaw_modules::TunnelBroker>) -> bool {
        self.tunnel_broker.set(broker).is_ok()
    }

    /// Ask the wired tunnel broker (if any) to tear down a public tunnel
    /// fronting `(session, host_port)`. Best-effort: returns immediately when no
    /// broker is wired or no tunnel exists. The tunnel broker keys teardown on
    /// the preview's HOST port (the local port cloudflared fronted).
    async fn teardown_tunnel(&self, session_id: SessionId, host_port: u16) {
        if let Some(broker) = self.tunnel_broker.get() {
            if broker.close_for_preview(session_id, host_port).await {
                info!(
                    session = %session_id.as_uuid(),
                    host_port,
                    "public tunnel torn down with its preview"
                );
            }
        }
    }

    /// Look up the live proxy backing `(session, container_port)`: its bound
    /// host port + gating token, but only while the preview is [`PreviewPhase::Live`]
    /// (a tombstoned or absent preview yields `None`). Used by the M19 A3
    /// public-tunnel broker to front the live preview's host port and compose the
    /// tokened public URL. Returns `None` when there is nothing live to front.
    pub async fn live_proxy_for(
        &self,
        session_id: SessionId,
        container_port: u16,
    ) -> Option<LivePreviewProxy> {
        let entries = self.entries.lock().await;
        let entry = entries
            .iter()
            .find(|e| e.session_id == session_id && e.container_port == container_port)?;
        if entry.proxy.phase() != PreviewPhase::Live {
            return None;
        }
        Some(LivePreviewProxy {
            host_port: entry.host_port,
            token: entry.token.clone(),
        })
    }

    /// Spawn the idle reaper. It scans every [`PREVIEW_REAPER_TICK`] and tears
    /// down any preview idle longer than [`PREVIEW_IDLE_TIMEOUT`]; on
    /// `shutdown` it tears down every live preview and exits.
    pub fn spawn_reaper(self: &Arc<Self>, shutdown: CancellationToken) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => {
                        this.teardown_all(TeardownReason::Shutdown).await;
                        return;
                    }
                    () = tokio::time::sleep(PREVIEW_REAPER_TICK) => {
                        this.reap_idle(Instant::now(), PREVIEW_IDLE_TIMEOUT).await;
                    }
                }
            }
        });
    }

    /// Tombstone every LIVE preview whose last activity is older than `idle`
    /// relative to `now`. Factored out of the reaper loop so it is testable
    /// with an explicit clock.
    ///
    /// V2: reaping no longer cancels the listener / releases the port. It tears
    /// down only the upstream proxying and leaves the bound port serving the
    /// "preview expired" tombstone page (which offers a single tokened
    /// re-expose). Full teardown — cancel + port release — still happens on
    /// session stop / close / shutdown via [`Self::teardown`], exactly as
    /// before. Already-tombstoned entries are skipped so they are not
    /// re-audited every tick.
    async fn reap_idle(&self, now: Instant, idle: Duration) {
        let entries = self.entries.lock().await;
        for e in entries.iter() {
            if e.proxy.phase() != PreviewPhase::Live {
                continue;
            }
            let last = *e.last_activity.lock().unwrap();
            if now.saturating_duration_since(last) < idle {
                continue;
            }
            e.proxy.tombstone();
            // A3: a tombstoned preview no longer proxies to the container, so
            // any public tunnel fronting it would serve the "expired" page to
            // the public internet. Tear the tunnel down on idle — re-exposing
            // publicly later earns a FRESH approval (the grant is one-shot).
            self.teardown_tunnel(e.session_id, e.host_port).await;
            write_preview_audit(
                &self.central,
                e.session_id,
                e.agent_group_id,
                "tombstone",
                Some(e.host_port),
                e.container_port,
                TeardownReason::Idle.as_str(),
            );
            info!(
                session = %e.session_id.as_uuid(),
                host_port = e.host_port,
                container_port = e.container_port,
                "preview idle-tombstoned (port kept for one tokened re-expose)"
            );
        }
    }

    /// Tear down every live preview (host shutdown).
    async fn teardown_all(&self, reason: TeardownReason) {
        let all: Vec<(SessionId, u16)> = {
            let entries = self.entries.lock().await;
            entries
                .iter()
                .map(|e| (e.session_id, e.container_port))
                .collect()
        };
        for (session_id, port) in all {
            self.teardown(session_id, port, reason).await;
        }
    }

    /// Tear down every preview a session opened (its container stopped / was
    /// removed). Called by the container manager on idle-stop / crash-restart.
    pub async fn close_all_for_session(&self, session_id: SessionId, reason: TeardownReason) {
        let ports: Vec<u16> = {
            let entries = self.entries.lock().await;
            entries
                .iter()
                .filter(|e| e.session_id == session_id)
                .map(|e| e.container_port)
                .collect()
        };
        for port in ports {
            self.teardown(session_id, port, reason).await;
        }
    }

    /// Remove the entry for `(session, container_port)` if present, cancel its
    /// server task, and audit the teardown. Returns whether an entry existed.
    async fn teardown(
        &self,
        session_id: SessionId,
        container_port: u16,
        reason: TeardownReason,
    ) -> bool {
        let removed = {
            let mut entries = self.entries.lock().await;
            entries
                .iter()
                .position(|e| e.session_id == session_id && e.container_port == container_port)
                .map(|pos| entries.remove(pos))
        };
        match removed {
            Some(entry) => {
                // V2: keep the tombstoned gauge balanced when a tombstoned
                // preview is fully torn down (rather than recovered).
                if entry.proxy.phase() == PreviewPhase::Tombstone {
                    copperclaw_metrics::dec_preview_tombstoned();
                }
                // A3: auto-teardown any public tunnel fronting this preview's
                // host port — no public tunnel outlives the app it fronted.
                self.teardown_tunnel(entry.session_id, entry.host_port)
                    .await;
                entry.cancel.cancel();
                self.audit(
                    entry.session_id,
                    entry.agent_group_id,
                    "close",
                    Some(entry.host_port),
                    entry.container_port,
                    reason.as_str(),
                );
                info!(
                    session = %session_id.as_uuid(),
                    host_port = entry.host_port,
                    container_port,
                    container_ip = %entry.container_ip,
                    name = entry.name.as_deref().unwrap_or(""),
                    reason = reason.as_str(),
                    "preview torn down"
                );
                true
            }
            None => false,
        }
    }

    /// Write an audit row for a preview mutation. Thin method wrapper over the
    /// free [`write_preview_audit`] so the axum-side recovery path (which has no
    /// `&PreviewManager`) can audit with identical shape.
    fn audit(
        &self,
        session_id: SessionId,
        agent_group_id: AgentGroupId,
        action: &str,
        host_port: Option<u16>,
        container_port: u16,
        reason: &str,
    ) {
        write_preview_audit(
            &self.central,
            session_id,
            agent_group_id,
            action,
            host_port,
            container_port,
            reason,
        );
    }

    /// V2 one-tap enablement: raise a G1 approval card that flips
    /// `preview_enabled` when tapped. Called from the `expose` group-gate when
    /// the group has not opted into previews, so a phone-only operator can
    /// enable previews without a terminal — secure-by-default is preserved
    /// (previews stay OFF until an authorised operator taps Enable).
    ///
    /// Best-effort and idempotent:
    /// * If a live `enable_preview` approval already exists for this group, no
    ///   second card is raised (agents retry `expose_preview` repeatedly).
    /// * The card is posted to the group's primary messaging channel (the first
    ///   wiring — the same "primary" rule the pending-sender notifier uses).
    /// * The card body reuses [`PreviewError::Disabled`]'s copy-pasteable
    ///   `cclaw` fix text, so a desk operator still sees the CLI equivalent.
    /// * The tap routes through G1's merged interceptor + DB decision path
    ///   (`approve:<id>` → [`crate::handlers::approvals::resolve_approve`] →
    ///   the `enable_preview` apply arm).
    fn request_enable_approval(&self, session: &SessionInfoLite) {
        let ag = session.agent_group_id;

        // Idempotent: skip if a still-actionable enable-preview card is already
        // outstanding for this group.
        let now = Utc::now();
        match pending_approvals::list(
            &self.central,
            Some("enable_preview"),
            Some(ApprovalStatus::Pending),
        ) {
            Ok(rows) => {
                if rows
                    .iter()
                    .any(|r| r.agent_group_id == Some(ag) && r.is_actionable_at(now))
                {
                    copperclaw_metrics::inc_preview_enable_card("skipped_already_pending");
                    return;
                }
            }
            Err(err) => {
                warn!(
                    ?err,
                    "preview: could not check for an existing enable-preview approval"
                );
                return;
            }
        }

        let Some(dispatcher) = self.approval_dispatcher.get() else {
            copperclaw_metrics::inc_preview_enable_card("skipped_no_dispatcher");
            info!(
                agent_group_id = %ag.as_uuid(),
                "preview: no approval dispatcher wired; skipping enable-preview card"
            );
            return;
        };

        // Resolve the group's primary messaging channel (first wiring, ordered
        // priority desc then created_at — same as the sender notifier).
        let wirings = match messaging_group_agents::list_for_ag(&self.central, ag) {
            Ok(w) => w,
            Err(err) => {
                info!(agent_group_id = %ag.as_uuid(), ?err, "preview: could not list wirings for enable-preview card; skipping");
                return;
            }
        };
        let Some(wiring) = wirings.first() else {
            copperclaw_metrics::inc_preview_enable_card("skipped_no_messaging_group");
            info!(agent_group_id = %ag.as_uuid(), "preview: agent group has no messaging groups; skipping enable-preview card");
            return;
        };
        let mg = match messaging_groups::get(&self.central, wiring.messaging_group_id) {
            Ok(g) => g,
            Err(err) => {
                info!(messaging_group_id = %wiring.messaging_group_id.as_uuid(), ?err, "preview: could not fetch messaging group; skipping enable-preview card");
                return;
            }
        };

        // Persist the pending approval (stable request_id → the ON CONFLICT
        // partial index keeps retries from stacking duplicate pending rows).
        let body = PreviewError::Disabled {
            group: ag.as_uuid().to_string(),
        }
        .to_string();
        let approval = match pending_approvals::upsert(
            &self.central,
            UpsertPendingApproval {
                request_id: format!("enable-preview:{}", ag.as_uuid()),
                action: "enable_preview".to_string(),
                payload: serde_json::json!({}),
                agent_group_id: Some(ag),
                channel_type: Some(mg.channel_type.clone()),
                platform_id: Some(mg.platform_id.clone()),
                title: ENABLE_PREVIEW_CARD_TITLE.to_string(),
                options: vec![],
                ..Default::default()
            },
        ) {
            Ok(a) => a,
            Err(err) => {
                warn!(
                    ?err,
                    "preview: could not persist enable-preview approval row"
                );
                return;
            }
        };

        // Emit the card (same canonical Card shape `ApprovalCardHandler` builds,
        // so every adapter renders it natively and G1's interceptor recognises
        // the `approve:<id>` / `deny:<id>` callbacks).
        let approval_id = approval.approval_id.as_uuid().to_string();
        let target = DispatchTarget::channel(mg.channel_type.clone(), mg.platform_id.clone(), None);
        let card = OutboundMessage {
            kind: MessageKind::Card,
            content: serde_json::json!({
                "card": {
                    "title": ENABLE_PREVIEW_CARD_TITLE,
                    "body": body,
                    "buttons": [
                        { "label": "Enable previews for this group", "value": format!("approve:{approval_id}"), "style": "primary" },
                        { "label": "Not now", "value": format!("deny:{approval_id}"), "style": "danger" },
                    ],
                },
            }),
            files: vec![],
        };
        dispatcher.dispatch(&target, &card);
        copperclaw_metrics::inc_preview_enable_card("raised");
        info!(
            agent_group_id = %ag.as_uuid(),
            approval_id = %approval_id,
            notify_channel = mg.channel_type.as_str(),
            "preview: raised one-tap enable-preview approval card"
        );
    }

    /// Number of tracked previews — live AND idle-tombstoned (both still hold a
    /// bound host port until teardown). Test / introspection helper.
    pub async fn active_count(&self) -> usize {
        self.entries.lock().await.len()
    }
}

/// Write an audit row for a preview mutation. Best-effort (a DB error is
/// logged + swallowed, matching the attestation / dispatch audit contract).
/// Free function so both [`PreviewManager`] and the axum-side tombstone
/// recovery path can call it.
fn write_preview_audit(
    central: &CentralDb,
    session_id: SessionId,
    agent_group_id: AgentGroupId,
    action: &str,
    host_port: Option<u16>,
    container_port: u16,
    reason: &str,
) {
    let args = serde_json::json!({
        "action": action,
        "session_id": session_id.as_uuid().to_string(),
        "container_port": container_port,
        "host_port": host_port,
        "reason": reason,
    })
    .to_string();
    let entry = AuditEntry {
        ts: Utc::now(),
        caller_kind: "host".to_string(),
        caller_session: Some(session_id.as_uuid().to_string()),
        caller_agent_group: Some(agent_group_id.as_uuid().to_string()),
        command: PREVIEW_AUDIT_COMMAND.to_string(),
        args,
        result: "ok".to_string(),
        error_code: None,
        error_message: None,
        latency_ms: 0,
    };
    if let Err(err) = audit_log::insert(central, &entry) {
        warn!(?err, "could not write preview audit row");
    }
}

/// The container name for a session — the `copperclaw-<session-uuid>`
/// convention the container manager spawns under.
fn container_name_for(session_id: SessionId) -> String {
    format!("copperclaw-{}", session_id.as_uuid())
}

/// Resolve the host interface the proxy binds from the group's `preview_bind`.
/// `None`/absent or unparseable → loopback (`127.0.0.1`), the safe default.
fn resolve_bind(preview_bind: Option<&str>) -> IpAddr {
    match preview_bind {
        Some(s) => s.trim().parse::<IpAddr>().unwrap_or_else(|_| {
            warn!(bind = %s, "preview_bind did not parse as an IP; falling back to 127.0.0.1");
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }),
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

/// The host string to render into the shareable URL for a given bind address.
///
/// * A loopback bind → `127.0.0.1` (reachable from the host only).
/// * A specific routable bind IP → that IP verbatim.
/// * The unspecified bind (`0.0.0.0`) → the host's primary LAN IP, detected via
///   the UDP-connect trick (open a UDP socket "connected" to a public IP — no
///   packet is sent — and read the kernel-chosen source address). Falls back to
///   `127.0.0.1` when detection fails.
fn display_host(bind: IpAddr) -> String {
    if bind.is_unspecified() {
        primary_lan_ip().unwrap_or_else(|| "127.0.0.1".to_string())
    } else {
        bind.to_string()
    }
}

/// Detect the host's primary LAN IP without sending any packets: a UDP socket
/// "connected" to a public address exposes the kernel-selected source IP via
/// `local_addr`. Returns `None` on any failure (no network, etc.).
fn primary_lan_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // 8.8.8.8:80 is never contacted — connect() only fixes the route/source.
    sock.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    if ip.is_unspecified() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// Build the operator-facing note relayed alongside the URL.
fn exposure_note() -> String {
    "Valid until idle for 30 minutes. Anyone with this link on your network can open it."
        .to_string()
}

#[async_trait::async_trait]
impl PreviewBroker for PreviewManager {
    async fn expose(
        &self,
        session: &SessionInfoLite,
        port: u16,
        name: Option<String>,
    ) -> Result<PreviewExposed, PreviewError> {
        if port == 0 {
            return Err(PreviewError::BadRequest(
                "port must be between 1 and 65535".into(),
            ));
        }

        // 1. Group gate. No config row or disabled → still refuse (secure by
        //    default: previews stay OFF), but ALSO raise a one-tap G1 approval
        //    card so a phone-only operator can enable them without a terminal
        //    (V2). The agent gets the copy-pasteable `cclaw` fix text as before.
        let cfg = container_configs::get(&self.central, session.agent_group_id)
            .map_err(|e| PreviewError::Internal(format!("read group config: {e}")))?;
        let Some(cfg) = cfg.filter(|c| c.preview_enabled) else {
            self.request_enable_approval(session);
            return Err(PreviewError::Disabled {
                group: session.agent_group_id.as_uuid().to_string(),
            });
        };
        let bind = resolve_bind(cfg.preview_bind.as_deref());

        // Serialize the caps-check + port-bind + insert so two concurrent
        // exposes can't both claim the last slot / port.
        let mut entries = self.entries.lock().await;

        // 2. Idempotent re-expose: an existing preview for this
        //    (session, container_port) returns its current URL unchanged. If it
        //    was idle-tombstoned (V2), revive it in place first so the same URL
        //    proxies live again — an explicit re-expose earns a fresh lease
        //    (and a fresh one-shot recovery budget).
        if let Some(existing) = entries
            .iter()
            .find(|e| e.session_id == session.session_id && e.container_port == port)
        {
            let url = preview_url(&display_host(bind), existing.host_port, &existing.token);
            let proxy = Arc::clone(&existing.proxy);
            if proxy.phase() == PreviewPhase::Tombstone {
                // Best-effort: if the container is gone the URL still recovers
                // via the tombstone's own path, so we return it regardless.
                let _ = proxy.revive().await;
            }
            return Ok(PreviewExposed {
                url,
                note: exposure_note(),
            });
        }

        // 3. Concurrency caps.
        let per_session = entries
            .iter()
            .filter(|e| e.session_id == session.session_id)
            .count();
        if per_session >= PREVIEW_MAX_PER_SESSION {
            return Err(PreviewError::CapacityExceeded(format!(
                "this session already has {PREVIEW_MAX_PER_SESSION} previews open; close one first"
            )));
        }
        if entries.len() >= PREVIEW_MAX_PER_HOST {
            return Err(PreviewError::CapacityExceeded(format!(
                "the host already has {PREVIEW_MAX_PER_HOST} previews open across all sessions"
            )));
        }

        // 4. Container IP.
        let name_c = container_name_for(session.session_id);
        let container_ip = self
            .runtime
            .container_ip(&name_c)
            .await
            .map_err(|e| PreviewError::Internal(format!("resolve container ip: {e}")))?
            .ok_or(PreviewError::NoContainerIp)?;

        // 5. Allocate a free host port and bind its listener.
        let used: std::collections::HashSet<u16> = entries.iter().map(|e| e.host_port).collect();
        let (listener, host_port) = bind_free_port(bind, &used).await.ok_or_else(|| {
            PreviewError::NoFreePort(format!(
                "no port free in {PREVIEW_PORT_MIN}..={PREVIEW_PORT_MAX}"
            ))
        })?;

        // 6. Mint token + stand up the proxy.
        let token = mint_token();
        let cancel = CancellationToken::new();
        let last_activity = Arc::new(StdMutex::new(Instant::now()));
        let upstream = format!("http://{container_ip}:{port}");
        let state = Arc::new(ProxyState {
            token: token.clone(),
            last_activity: Arc::clone(&last_activity),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| PreviewError::Internal(format!("build proxy client: {e}")))?,
            inner: StdMutex::new(ProxyStateInner {
                phase: PreviewPhase::Live,
                upstream,
                recovery_used: false,
            }),
            runtime: Arc::clone(&self.runtime),
            central: self.central.clone(),
            session_id: session.session_id,
            agent_group_id: session.agent_group_id,
            container_name: name_c,
            container_port: port,
            host_port,
        });
        let app = Router::new()
            .fallback(proxy_handler)
            .with_state(Arc::clone(&state));
        let serve_cancel = cancel.clone();
        tokio::spawn(async move {
            let shutdown = async move { serve_cancel.cancelled().await };
            if let Err(err) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
            {
                warn!(?err, "preview proxy server exited with error");
            }
        });

        let display = display_host(bind);
        let url = preview_url(&display, host_port, &token);

        entries.push(PreviewEntry {
            session_id: session.session_id,
            agent_group_id: session.agent_group_id,
            container_port: port,
            host_port,
            token,
            name: name.clone(),
            container_ip,
            last_activity,
            cancel,
            proxy: state,
        });
        drop(entries);

        self.audit(
            session.session_id,
            session.agent_group_id,
            "expose",
            Some(host_port),
            port,
            "opened",
        );
        info!(
            session = %session.session_id.as_uuid(),
            container_port = port,
            host_port,
            bind = %bind,
            name = name.as_deref().unwrap_or(""),
            "preview exposed"
        );

        Ok(PreviewExposed {
            url,
            note: exposure_note(),
        })
    }

    async fn close(&self, session_id: SessionId, port: u16) -> Result<(), PreviewError> {
        if self
            .teardown(session_id, port, TeardownReason::Closed)
            .await
        {
            Ok(())
        } else {
            Err(PreviewError::NotFound { port })
        }
    }
}

/// The host-side implementation of the M19 A3 public-tunnel verb
/// (`make_preview_public`). It bridges the M17 preview subsystem (which owns the
/// mapping from a container port to a live preview's HOST port + gating token)
/// and the merged V5 [`TunnelBroker`] (which owns the approval gate + the
/// operator-provided cloudflared binary). Wired into the delivery service as a
/// [`copperclaw_modules::PublicTunnelBroker`] so a `make_preview_public` relay
/// call routes here.
///
/// Secure-by-default posture (every guard fail-closed):
/// * **Off unless the operator opted in.** `enabled` is the host env master
///   switch (`COPPERCLAW_PUBLIC_TUNNEL_ENABLED`, default false); combined with
///   the group's per-group `preview_enabled`, both must be true or the tunnel
///   broker returns [`copperclaw_modules::TunnelError::NotEnabled`].
/// * **A live preview must already exist.** Making a preview public requires an
///   `expose_preview` first — there is no path that stands up a fresh listener.
/// * **Every exposure is approval-gated.** The tunnel broker raises a
///   `CredentialedExternalAction` approval and stands nothing up until an
///   operator taps Approve (the runner-side taint gate also blocks the verb on a
///   tainted turn — it is NOT a LAN-exempt verb).
/// * **The public tunnel stays token-gated.** It fronts the token-gated preview
///   proxy, so the shareable URL carries `.../__preview/<token>` — defence in
///   depth: a public URL without the token 403s.
pub struct PublicPreviewTunnel {
    preview: Arc<PreviewManager>,
    tunnel: Arc<copperclaw_modules::TunnelBroker>,
    central: CentralDb,
    /// Host env master switch (`COPPERCLAW_PUBLIC_TUNNEL_ENABLED`). `false`
    /// (default) ⇒ the public capability is off host-wide regardless of any
    /// per-group setting; the tunnel broker returns `NotEnabled`.
    enabled: bool,
}

impl PublicPreviewTunnel {
    /// Wire the broker over the preview manager + V5 tunnel broker. `enabled` is
    /// the host env master switch, resolved once at boot.
    #[must_use]
    pub fn new(
        preview: Arc<PreviewManager>,
        tunnel: Arc<copperclaw_modules::TunnelBroker>,
        central: CentralDb,
        enabled: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            preview,
            tunnel,
            central,
            enabled,
        })
    }

    /// Resolve the group's primary messaging channel (first wiring — the same
    /// "primary" rule the enable-preview card + pending-sender notifier use) so
    /// the approval card lands where the operator will see it. `None` when the
    /// group has no wiring (the approval is still raised; it just won't post a
    /// card to a channel).
    fn primary_notify(&self, ag: AgentGroupId) -> Option<(copperclaw_types::ChannelType, String)> {
        let wirings = messaging_group_agents::list_for_ag(&self.central, ag).ok()?;
        let wiring = wirings.first()?;
        let mg = messaging_groups::get(&self.central, wiring.messaging_group_id).ok()?;
        Some((mg.channel_type.clone(), mg.platform_id.clone()))
    }
}

#[async_trait::async_trait]
impl copperclaw_modules::PublicTunnelBroker for PublicPreviewTunnel {
    async fn make_public(
        &self,
        session: SessionInfoLite,
        container_port: u16,
    ) -> copperclaw_modules::PublicTunnelReply {
        use copperclaw_modules::{PublicTunnelReply, TunnelExposeRequest, TunnelOutcome};

        // 1. There must be a LIVE preview on this container port — the public
        //    tunnel fronts an existing preview, it never stands up a listener.
        let Some(proxy) = self
            .preview
            .live_proxy_for(session.session_id, container_port)
            .await
        else {
            return PublicTunnelReply::Error(format!(
                "There is no live preview on container port {container_port} to make public. Run \
                 `expose_preview` with port {container_port} first (confirm it returns a working \
                 URL), then call `make_preview_public` with the same port."
            ));
        };

        // 2. Per-group opt-in: the group must still have previews enabled. The
        //    env master switch is ANDed in; both feed the tunnel broker's own
        //    fail-closed `enabled` gate (a false value ⇒ NotEnabled, audited).
        let preview_enabled = container_configs::get(&self.central, session.agent_group_id)
            .ok()
            .flatten()
            .is_some_and(|c| c.preview_enabled);
        let enabled = self.enabled && preview_enabled;

        // 3. cloudflared runs on the host and fronts the preview proxy's host
        //    port over loopback (the proxy binds loopback or 0.0.0.0, both
        //    reachable via 127.0.0.1). Fronting the token-gated proxy keeps the
        //    public tunnel token-gated too.
        let upstream = format!("http://127.0.0.1:{}", proxy.host_port);
        let notify = self.primary_notify(session.agent_group_id);

        let req = TunnelExposeRequest {
            session_id: session.session_id,
            agent_group_id: session.agent_group_id,
            host_port: proxy.host_port,
            upstream,
            enabled,
            notify,
        };

        match self.tunnel.expose(req).await {
            Ok(TunnelOutcome::Exposed(exposed)) => {
                // Compose the tokened public URL: the tunnel fronts the
                // token-gated proxy, so the shareable link must carry the token
                // path (a tokenless hit 403s). This is what the agent relays and
                // puts on the ritual card's public-URL button.
                let base = exposed.public_url.trim_end_matches('/');
                let public_url = format!("{base}{PREVIEW_TOKEN_PATH}{}", proxy.token);
                PublicTunnelReply::Exposed {
                    public_url,
                    note: exposed.note,
                }
            }
            Ok(TunnelOutcome::Pending { note, .. }) => PublicTunnelReply::Pending { note },
            Err(e) => PublicTunnelReply::Error(e.to_string()),
        }
    }
}

/// Render the shareable tokened URL. An IPv6 display host is bracketed per
/// RFC 3986 so the port separator stays unambiguous.
fn preview_url(display_host: &str, host_port: u16, token: &str) -> String {
    if display_host.contains(':') {
        format!("http://[{display_host}]:{host_port}{PREVIEW_TOKEN_PATH}{token}")
    } else {
        format!("http://{display_host}:{host_port}{PREVIEW_TOKEN_PATH}{token}")
    }
}

/// Mint a 128-bit random token as lowercase hex.
fn mint_token() -> String {
    let bytes: [u8; 16] = rand::random();
    hex::encode(bytes)
}

/// Bind the first free port in [`PREVIEW_PORT_MIN`]..=[`PREVIEW_PORT_MAX`] on
/// `bind`, skipping ports already tracked in `used` and any that fail to bind.
/// Returns the bound listener + its port, or `None` when the range is
/// exhausted.
async fn bind_free_port(
    bind: IpAddr,
    used: &std::collections::HashSet<u16>,
) -> Option<(tokio::net::TcpListener, u16)> {
    for port in PREVIEW_PORT_MIN..=PREVIEW_PORT_MAX {
        if used.contains(&port) {
            continue;
        }
        let addr = SocketAddr::new(bind, port);
        if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
            return Some((listener, port));
        }
    }
    None
}

/// Hop-by-hop headers (RFC 7230 §6.1) plus proxy-specific ones — never
/// forwarded across the proxy in either direction.
fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-connection"
    )
}

/// Copy `src` into a fresh `HeaderMap`, dropping hop-by-hop headers and (for
/// the request direction) the `Host` header so the upstream client sets its own.
fn filter_headers(src: &HeaderMap, drop_host: bool) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(src.len());
    for (name, value) in src {
        if is_hop_by_hop(name) {
            continue;
        }
        if drop_host && name == header::HOST {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Does this request carry a WebSocket / protocol upgrade? Bridged (V1) rather
/// than refused — see [`proxy_ws`].
fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.get(header::UPGRADE).is_some_and(|v| !v.is_empty())
        || headers
            .get(header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
}

/// Extract the preview cookie value from a request's `Cookie` headers.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(s) = value.to_str() else { continue };
        for pair in s.split(';') {
            let pair = pair.trim();
            if let Some(rest) = pair.strip_prefix(PREVIEW_COOKIE) {
                if let Some(tok) = rest.strip_prefix('=') {
                    return Some(tok.to_string());
                }
            }
        }
    }
    None
}

/// The axum fallback handler: token gate + reverse proxy. When the preview has
/// been idle-tombstoned (V2) it routes to [`tombstone_handler`] instead.
async fn proxy_handler(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    if state.phase() == PreviewPhase::Tombstone {
        return tombstone_handler(&state, req).await;
    }

    let path = req.uri().path().to_string();

    // Token-mint path: set the cookie and redirect to the app root.
    if let Some(token) = path.strip_prefix(PREVIEW_TOKEN_PATH) {
        if constant_time_eq(token, &state.token) {
            return cookie_mint_redirect(&state.token);
        }
        return forbidden();
    }

    // Every other request must present the matching cookie.
    match cookie_token(req.headers()) {
        Some(tok) if constant_time_eq(&tok, &state.token) => {}
        _ => {
            // V1: a cookieless WS upgrade is refused here (before proxy_ws).
            if is_upgrade(req.headers()) {
                copperclaw_metrics::inc_preview_ws_upgrade("refused");
            }
            return forbidden();
        }
    }

    // WebSocket / protocol upgrade: bridge it to the container app. The cookie
    // gate above has already passed, so only authenticated upgrades reach here.
    if is_upgrade(req.headers()) {
        return proxy_ws(&state, req).await;
    }

    // Mark activity so the idle reaper doesn't tear down a preview in use.
    *state.last_activity.lock().unwrap() = Instant::now();

    proxy_upstream(&state, req).await
}

/// Build the 302 that mints the gating cookie and redirects to the app root.
/// Shared by the live token-mint path and a successful tombstone recovery.
fn cookie_mint_redirect(token: &str) -> Response {
    let cookie = format!("{PREVIEW_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax");
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::SET_COOKIE, cookie)
        .header(header::LOCATION, "/")
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Handler for a tombstoned preview (V2). The upstream proxying is gone but the
/// port stays bound. A tokened `GET /__preview/<token>` attempts the single
/// re-expose the tombstone allows (constant-time token check unchanged); every
/// other request — and a token whose one recovery is already spent, or whose
/// container is gone — gets the static "preview expired" page.
async fn tombstone_handler(state: &Arc<ProxyState>, req: Request) -> Response {
    let path = req.uri().path();
    if let Some(token) = path.strip_prefix(PREVIEW_TOKEN_PATH) {
        if !constant_time_eq(token, &state.token) {
            return forbidden();
        }
        if state.try_recover().await {
            // Re-exposed: mint the cookie + redirect to the (now live) app.
            return cookie_mint_redirect(&state.token);
        }
        // One recovery already used, or the container is no longer up.
        return tombstone_page(true);
    }
    tombstone_page(false)
}

/// The static "preview expired" page a tombstoned port serves. `terminal` picks
/// the copy: the non-terminal page points the operator back at their original
/// link (which recovers the preview once); the terminal page tells them to ask
/// the agent to re-expose. Plain text, no emojis. Served 200 so browsers render
/// the body rather than a generic error chrome.
fn tombstone_page(terminal: bool) -> Response {
    let body = if terminal {
        "This preview has expired and can no longer be reopened automatically \
         (its one-time reopen was already used, or the app's container has \
         stopped). Ask the agent to run `expose_preview` again for a fresh link.\n"
    } else {
        "This preview has expired after being idle. Open the original preview \
         link the agent gave you once more to reopen it, or ask the agent to run \
         `expose_preview` again for a fresh link.\n"
    };
    (StatusCode::OK, body).into_response()
}

/// A 403 explaining how to obtain access.
fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        "Forbidden: open the tokened preview link the agent gave you (it sets a one-time access cookie).\n",
    )
        .into_response()
}

/// Constant-time string comparison for the token (avoids leaking length /
/// prefix match timing on the gate).
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Forward a validated request to the upstream container app, streaming the
/// request and response bodies, and return the upstream response.
async fn proxy_upstream(state: &ProxyState, req: Request) -> Response {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), std::string::ToString::to_string);
    let url = format!("{}{path_and_query}", state.upstream());
    let req_headers = filter_headers(req.headers(), true);

    let body = req.into_body();
    let reqwest_body = reqwest::Body::wrap_stream(body.into_data_stream());

    let upstream_resp = state
        .client
        .request(method, &url)
        .headers(req_headers)
        .body(reqwest_body)
        .send()
        .await;

    match upstream_resp {
        Ok(resp) => {
            let status = resp.status();
            let resp_headers = filter_headers(resp.headers(), false);
            let mut out = Response::builder().status(status);
            if let Some(h) = out.headers_mut() {
                *h = resp_headers;
            }
            let stream = resp.bytes_stream();
            out.body(Body::from_stream(stream))
                .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            format!("Preview upstream error: the app in the container did not respond ({err}).\n"),
        )
            .into_response(),
    }
}

/// The concrete type `tokio-tungstenite` hands back for a plain `ws://`
/// connection — the container app is reached over the Docker bridge, no TLS.
type UpstreamWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Build the `ws://<ip>:<port><path?query>` upstream URL from the stored
/// `http://<ip>:<port>` proxy base and the incoming request's path + query.
fn ws_upstream_url(http_upstream: &str, path_and_query: &str) -> String {
    let authority = http_upstream
        .strip_prefix("http://")
        .unwrap_or(http_upstream);
    format!("ws://{authority}{path_and_query}")
}

/// A 502 for a failed upstream WebSocket handshake, mirroring the HTTP path's
/// bad-gateway copy.
fn bad_gateway_ws(err: &str) -> Response {
    copperclaw_metrics::inc_preview_ws_upgrade("upstream_502");
    warn!(error = err, "preview WebSocket upstream connect failed");
    (
        StatusCode::BAD_GATEWAY,
        "Preview upstream error: the app in the container did not accept the WebSocket connection.\n",
    )
        .into_response()
}

/// Handle a WebSocket upgrade: complete the upgrade toward the browser and
/// bridge frames to/from the container app's `ws://` upstream.
///
/// The token cookie gate in [`proxy_handler`] has already passed, so only
/// authenticated upgrades reach here. The upstream socket is opened *before*
/// the browser-side upgrade is finalized, so a container that isn't serving a
/// WebSocket at this path fails fast with a 502 (rather than leaving the
/// browser holding a half-open upgrade), and so the subprotocol the container
/// selected can be mirrored back to the browser.
async fn proxy_ws(state: &ProxyState, req: Request) -> Response {
    let (mut parts, _body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| "/".to_string(), std::string::ToString::to_string);
    let requested_protocol = parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned();

    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };

    // Open the upstream socket first (fail-fast 502 + subprotocol mirroring).
    let upstream = state.upstream();
    let ws_url = ws_upstream_url(&upstream, &path_and_query);
    let mut client_req = match ws_url.into_client_request() {
        Ok(r) => r,
        Err(err) => return bad_gateway_ws(&err.to_string()),
    };
    if let Some(proto) = requested_protocol {
        client_req
            .headers_mut()
            .insert(header::SEC_WEBSOCKET_PROTOCOL, proto);
    }
    let (upstream_ws, upstream_resp) = match connect_async(client_req).await {
        Ok(pair) => pair,
        Err(err) => return bad_gateway_ws(&err.to_string()),
    };
    let negotiated = upstream_resp
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Opening the socket is itself activity; the bridge keeps bumping while it
    // stays open so the reaper never tears a live preview out from under a tab.
    *state.last_activity.lock().unwrap() = Instant::now();
    let last_activity = Arc::clone(&state.last_activity);

    let ws = match negotiated {
        Some(proto) => ws.protocols([proto]),
        None => ws,
    };
    copperclaw_metrics::inc_preview_ws_upgrade("ok");
    ws.on_upgrade(move |client_ws| bridge_ws(client_ws, upstream_ws, last_activity))
}

/// Bridge frames between the browser-facing `client` socket and the container
/// `upstream` socket until either side closes or errors. Every frame — plus a
/// periodic tick while the socket is merely open — bumps `last_activity`, so
/// the idle reaper treats an open WebSocket as an in-use preview (an HTTP
/// request only bumps on each request, so a long-lived silent socket would
/// otherwise be reaped mid-session).
async fn bridge_ws(
    mut client: WebSocket,
    mut upstream: UpstreamWs,
    last_activity: Arc<StdMutex<Instant>>,
) {
    // V1: meter the live bridge — active count, per-direction frames/bytes,
    // and total lifetime.
    copperclaw_metrics::inc_preview_ws_active();
    let bridge_started = Instant::now();
    let mut activity = tokio::time::interval(PREVIEW_WS_ACTIVITY_TICK);
    activity.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = activity.tick() => {
                *last_activity.lock().unwrap() = Instant::now();
            }
            from_client = client.next() => {
                match from_client {
                    Some(Ok(msg)) => {
                        *last_activity.lock().unwrap() = Instant::now();
                        let ts_msg = axum_msg_to_tungstenite(msg);
                        copperclaw_metrics::inc_preview_ws_frame("browser_to_container");
                        copperclaw_metrics::add_preview_ws_bytes(
                            "browser_to_container",
                            ts_msg.len() as u64,
                        );
                        let closing = matches!(ts_msg, TungsteniteMessage::Close(_));
                        if upstream.send(ts_msg).await.is_err() {
                            break;
                        }
                        if closing {
                            break;
                        }
                    }
                    _ => break,
                }
            }
            from_upstream = upstream.next() => {
                match from_upstream {
                    Some(Ok(msg)) => {
                        *last_activity.lock().unwrap() = Instant::now();
                        copperclaw_metrics::inc_preview_ws_frame("container_to_browser");
                        copperclaw_metrics::add_preview_ws_bytes(
                            "container_to_browser",
                            msg.len() as u64,
                        );
                        let Some(ax_msg) = tungstenite_msg_to_axum(msg) else {
                            continue;
                        };
                        let closing = matches!(ax_msg, AxumWsMessage::Close(_));
                        if client.send(ax_msg).await.is_err() {
                            break;
                        }
                        if closing {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }
    copperclaw_metrics::dec_preview_ws_active();
    copperclaw_metrics::observe_preview_ws_session_seconds(bridge_started.elapsed().as_secs_f64());
    let _ = client.close().await;
    let _ = upstream.close(None).await;
}

/// Convert an axum inbound frame into the tungstenite frame sent upstream.
/// Mirrors axum's own (private) `Message::into_tungstenite`.
fn axum_msg_to_tungstenite(msg: AxumWsMessage) -> TungsteniteMessage {
    match msg {
        AxumWsMessage::Text(t) => TungsteniteMessage::Text(t),
        AxumWsMessage::Binary(b) => TungsteniteMessage::Binary(b),
        AxumWsMessage::Ping(p) => TungsteniteMessage::Ping(p),
        AxumWsMessage::Pong(p) => TungsteniteMessage::Pong(p),
        AxumWsMessage::Close(Some(cf)) => TungsteniteMessage::Close(Some(TungsteniteCloseFrame {
            code: CloseCode::from(cf.code),
            reason: cf.reason,
        })),
        AxumWsMessage::Close(None) => TungsteniteMessage::Close(None),
    }
}

/// Convert a tungstenite upstream frame into the axum frame sent to the
/// browser. Mirrors axum's own (private) `Message::from_tungstenite`; raw
/// `Frame` frames are dropped as the tungstenite maintainers recommend.
fn tungstenite_msg_to_axum(msg: TungsteniteMessage) -> Option<AxumWsMessage> {
    match msg {
        TungsteniteMessage::Text(t) => Some(AxumWsMessage::Text(t)),
        TungsteniteMessage::Binary(b) => Some(AxumWsMessage::Binary(b)),
        TungsteniteMessage::Ping(p) => Some(AxumWsMessage::Ping(p)),
        TungsteniteMessage::Pong(p) => Some(AxumWsMessage::Pong(p)),
        TungsteniteMessage::Close(Some(cf)) => Some(AxumWsMessage::Close(Some(AxumCloseFrame {
            code: cf.code.into(),
            reason: cf.reason,
        }))),
        TungsteniteMessage::Close(None) => Some(AxumWsMessage::Close(None)),
        TungsteniteMessage::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_128_bit_hex() {
        let t = mint_token();
        assert_eq!(t.len(), 32, "16 bytes → 32 hex chars");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(mint_token(), mint_token(), "tokens are random");
    }

    #[test]
    fn preview_url_shape() {
        let u = preview_url("192.168.1.5", 8100, "deadbeef");
        assert_eq!(u, "http://192.168.1.5:8100/__preview/deadbeef");
        // IPv6 hosts are bracketed so the port separator is unambiguous.
        let v6 = preview_url("::1", 8100, "deadbeef");
        assert_eq!(v6, "http://[::1]:8100/__preview/deadbeef");
    }

    #[test]
    fn resolve_bind_defaults_to_loopback() {
        assert_eq!(resolve_bind(None), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(
            resolve_bind(Some("garbage")),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            resolve_bind(Some("0.0.0.0")),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            resolve_bind(Some("192.168.1.9")),
            "192.168.1.9".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn display_host_for_loopback_and_specific() {
        assert_eq!(display_host(IpAddr::V4(Ipv4Addr::LOCALHOST)), "127.0.0.1");
        assert_eq!(display_host("10.1.2.3".parse().unwrap()), "10.1.2.3");
    }

    #[test]
    fn container_name_follows_convention() {
        let s = SessionId::new();
        assert_eq!(container_name_for(s), format!("copperclaw-{}", s.as_uuid()));
    }

    #[test]
    fn hop_by_hop_headers_detected() {
        for h in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
            "proxy-authorization",
        ] {
            assert!(is_hop_by_hop(
                &HeaderName::from_bytes(h.as_bytes()).unwrap()
            ));
        }
        assert!(!is_hop_by_hop(&header::CONTENT_TYPE));
    }

    #[test]
    fn cookie_token_parsing() {
        let mut h = HeaderMap::new();
        h.append(
            header::COOKIE,
            "foo=bar; cclaw_preview=abc123; baz=qux".parse().unwrap(),
        );
        assert_eq!(cookie_token(&h), Some("abc123".to_string()));
        let empty = HeaderMap::new();
        assert_eq!(cookie_token(&empty), None);
    }

    #[test]
    fn upgrade_detection() {
        let mut h = HeaderMap::new();
        assert!(!is_upgrade(&h));
        h.append(header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_upgrade(&h));
        let mut h2 = HeaderMap::new();
        h2.append(header::CONNECTION, "Upgrade".parse().unwrap());
        assert!(is_upgrade(&h2));
    }

    #[test]
    fn ws_upstream_url_swaps_scheme_and_keeps_path() {
        assert_eq!(
            ws_upstream_url("http://172.17.0.2:3000", "/hmr?token=x"),
            "ws://172.17.0.2:3000/hmr?token=x"
        );
        assert_eq!(
            ws_upstream_url("http://127.0.0.1:8080", "/"),
            "ws://127.0.0.1:8080/"
        );
    }

    #[test]
    fn ws_message_conversions_round_trip() {
        // Text / binary survive both directions.
        let t = axum_msg_to_tungstenite(AxumWsMessage::Text("hi".into()));
        assert_eq!(
            tungstenite_msg_to_axum(t),
            Some(AxumWsMessage::Text("hi".into()))
        );
        let b = axum_msg_to_tungstenite(AxumWsMessage::Binary(vec![1, 2, 3]));
        assert_eq!(
            tungstenite_msg_to_axum(b),
            Some(AxumWsMessage::Binary(vec![1, 2, 3]))
        );
        // Close code + reason are preserved.
        let c = axum_msg_to_tungstenite(AxumWsMessage::Close(Some(AxumCloseFrame {
            code: 1000,
            reason: "bye".into(),
        })));
        match tungstenite_msg_to_axum(c) {
            Some(AxumWsMessage::Close(Some(cf))) => {
                assert_eq!(cf.code, 1000);
                assert_eq!(cf.reason, "bye");
            }
            other => panic!("expected Close frame, got {other:?}"),
        }
        // Ping payloads survive upstream→browser.
        assert_eq!(
            tungstenite_msg_to_axum(TungsteniteMessage::Ping(vec![9])),
            Some(AxumWsMessage::Ping(vec![9]))
        );
    }

    #[test]
    fn constant_time_eq_matches_str_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn bind_free_port_range_exhausted_returns_none() {
        // A `used` set covering the whole preview range means no port is free
        // — the allocator returns None without attempting any bind.
        let used: std::collections::HashSet<u16> = (PREVIEW_PORT_MIN..=PREVIEW_PORT_MAX).collect();
        let got = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(bind_free_port(IpAddr::V4(Ipv4Addr::LOCALHOST), &used));
        assert!(got.is_none(), "fully-used range must yield no free port");
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use copperclaw_container_rt::build::ImageBuildSpec;
    use copperclaw_container_rt::spec::{ContainerHandle, ContainerSpec};
    use copperclaw_container_rt::{RtError, RuntimeKind};
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::container_configs::{
        self, CliScope, SkillsSelector, UpsertContainerConfig,
    };
    use serde_json::json;

    /// A container runtime stub whose only meaningful method is
    /// [`ContainerRuntime::container_ip`], which returns a preconfigured value.
    struct IpRuntime {
        ip: Option<String>,
    }

    #[async_trait]
    impl ContainerRuntime for IpRuntime {
        async fn ensure_running(&self) -> Result<(), RtError> {
            Ok(())
        }
        async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
            Ok(())
        }
        async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
            Ok(ContainerHandle::new("id", spec.name))
        }
        async fn stop(&self, _name: &str, _grace: StdDuration) -> Result<(), RtError> {
            Ok(())
        }
        async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
            Ok(spec.image_tag())
        }
        async fn container_ip(&self, _name: &str) -> Result<Option<String>, RtError> {
            Ok(self.ip.clone())
        }
    }

    // Keep RuntimeKind referenced so an unused-import lint can't fire if the
    // stub is trimmed later.
    const _: RuntimeKind = RuntimeKind::Docker;

    fn central() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    /// Create a group and its container config, optionally opting into previews.
    fn group_with_preview(
        db: &CentralDb,
        preview_enabled: bool,
        bind: Option<&str>,
    ) -> AgentGroupId {
        let ag = create_ag(
            db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id;
        container_configs::upsert(
            db,
            UpsertContainerConfig {
                agent_group_id: ag,
                provider: None,
                model: None,
                effort: None,
                image_tag: None,
                assistant_name: None,
                max_messages_per_prompt: None,
                skills: SkillsSelector::All,
                mcp_servers: json!({}),
                packages_apt: vec![],
                packages_npm: vec![],
                additional_mounts: json!([]),
                cli_scope: CliScope::Group,
                config_fingerprint: None,
                egress_allow: vec![],
                resource_limits: json!({}),
                coding_enabled: false,
                surface_thinking: false,
                tool_profile: None,
                preview_enabled,
                preview_bind: bind.map(str::to_string),
                check_command: None,
                verify_gate: true,
                image_profile: copperclaw_types::ImageProfile::Minimal,
            },
        )
        .unwrap();
        ag
    }

    fn manager(db: CentralDb, ip: Option<String>) -> Arc<PreviewManager> {
        let rt: Arc<dyn ContainerRuntime> = Arc::new(IpRuntime { ip });
        PreviewManager::new(db, rt)
    }

    /// Spawn a trivial upstream "container app" on loopback; returns its port
    /// and a router that echoes a fixed body on `/` and reflects the path.
    async fn spawn_upstream() -> u16 {
        let app = Router::new().route("/", axum::routing::get(|| async { "hello from the app" }));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// Spawn a trivial upstream "container app" that upgrades `/` to a
    /// WebSocket and echoes every data frame back until the peer closes.
    async fn spawn_ws_upstream() -> u16 {
        use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};

        async fn echo(mut socket: WebSocket) {
            while let Some(Ok(msg)) = socket.recv().await {
                if matches!(msg, Message::Close(_)) {
                    break;
                }
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
        }
        async fn handler(ws: WebSocketUpgrade) -> Response {
            ws.on_upgrade(echo)
        }

        let app = Router::new().route("/", axum::routing::get(handler));
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    #[tokio::test]
    async fn websocket_bridge_echoes_with_cookie_and_refuses_without() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let upstream_port = spawn_ws_upstream().await;
        let db = central();
        let ag = group_with_preview(&db, true, None); // loopback bind
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);

        let exposed = mgr
            .expose(&sess, upstream_port, Some("ws-demo".into()))
            .await
            .unwrap();

        // Parse the host port + token out of the shareable URL.
        let token = exposed.url.split("/__preview/").nth(1).unwrap().to_string();
        let base = exposed.url.split("/__preview/").next().unwrap();
        let host_port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();

        // With the cookie: the upgrade proxies to the echo app and round-trips.
        let mut req = format!("ws://127.0.0.1:{host_port}/")
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            axum::http::header::COOKIE,
            format!("cclaw_preview={token}").parse().unwrap(),
        );
        let (mut ws, _resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        ws.send(WsMsg::Text("round-trip".into())).await.unwrap();
        let got = ws.next().await.unwrap().unwrap();
        assert_eq!(got, WsMsg::Text("round-trip".into()));
        ws.close(None).await.unwrap();

        // Without the cookie: the gate 403s before any upgrade.
        let no_cookie = format!("ws://127.0.0.1:{host_port}/")
            .into_client_request()
            .unwrap();
        let err = tokio_tungstenite::connect_async(no_cookie)
            .await
            .unwrap_err();
        match err {
            tokio_tungstenite::tungstenite::Error::Http(resp) => {
                assert_eq!(resp.status().as_u16(), 403, "gate refuses before upgrade");
            }
            other => panic!("expected an HTTP 403 rejection, got {other:?}"),
        }

        mgr.close(sess.session_id, upstream_port).await.unwrap();
        assert_eq!(mgr.active_count().await, 0);
    }

    #[tokio::test]
    async fn expose_disabled_group_is_refused_with_operator_command() {
        let db = central();
        let ag = group_with_preview(&db, false, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        let err = mgr.expose(&sess, 3000, None).await.unwrap_err();
        match err {
            PreviewError::Disabled { group } => {
                assert_eq!(group, ag.as_uuid().to_string());
            }
            other => panic!("expected Disabled, got {other:?}"),
        }
        assert_eq!(mgr.active_count().await, 0);
    }

    #[tokio::test]
    async fn expose_without_container_ip_is_refused() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, None); // runtime reports no bridge IP
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        let err = mgr.expose(&sess, 3000, None).await.unwrap_err();
        assert!(matches!(err, PreviewError::NoContainerIp));
    }

    #[tokio::test]
    async fn token_flow_redirect_cookie_and_proxy() {
        let upstream_port = spawn_upstream().await;
        let db = central();
        let ag = group_with_preview(&db, true, None); // loopback bind
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);

        let exposed = mgr
            .expose(&sess, upstream_port, Some("demo".into()))
            .await
            .unwrap();
        // URL shape: loopback display host + a port in the preview range.
        assert!(exposed.url.starts_with("http://127.0.0.1:81"));
        assert!(exposed.url.contains("/__preview/"));
        assert!(exposed.note.contains("30 minutes"));

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // 1. The tokened link sets the cookie and 302-redirects to `/`.
        let r = client.get(&exposed.url).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 302);
        let set_cookie = r
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(set_cookie.starts_with("cclaw_preview="));
        assert!(set_cookie.contains("HttpOnly"));
        assert_eq!(r.headers().get(reqwest::header::LOCATION).unwrap(), "/");
        let token = set_cookie
            .strip_prefix("cclaw_preview=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        // Base URL for the proxy (strip the /__preview/<token> tail).
        let base = exposed.url.split("/__preview/").next().unwrap().to_string();

        // 2. No cookie → 403.
        let r = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 403);

        // 3. Wrong cookie → 403.
        let r = client
            .get(format!("{base}/"))
            .header(reqwest::header::COOKIE, "cclaw_preview=deadbeef")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);

        // 4. Good cookie → proxied to the upstream app.
        let r = client
            .get(format!("{base}/"))
            .header(reqwest::header::COOKIE, format!("cclaw_preview={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.text().await.unwrap(), "hello from the app");

        // 5. Closing tears it down; a second proxied request now fails to connect.
        mgr.close(sess.session_id, upstream_port).await.unwrap();
        assert_eq!(mgr.active_count().await, 0);
        // Closing again → NotFound.
        assert!(matches!(
            mgr.close(sess.session_id, upstream_port).await,
            Err(PreviewError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn idempotent_reexpose_returns_same_port() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        let a = mgr.expose(&sess, 4000, None).await.unwrap();
        let b = mgr.expose(&sess, 4000, None).await.unwrap();
        assert_eq!(a.url, b.url, "re-exposing the same port is idempotent");
        assert_eq!(mgr.active_count().await, 1);
        mgr.close(sess.session_id, 4000).await.unwrap();
    }

    #[tokio::test]
    async fn per_session_cap_is_enforced() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        for p in 0..PREVIEW_MAX_PER_SESSION {
            let port = 5000 + u16::try_from(p).unwrap();
            mgr.expose(&sess, port, None).await.unwrap();
        }
        let err = mgr.expose(&sess, 5999, None).await.unwrap_err();
        assert!(matches!(err, PreviewError::CapacityExceeded(_)));
        assert_eq!(mgr.active_count().await, PREVIEW_MAX_PER_SESSION);
        mgr.close_all_for_session(sess.session_id, TeardownReason::Closed)
            .await;
        assert_eq!(mgr.active_count().await, 0);
    }

    #[tokio::test]
    async fn close_all_for_session_only_touches_that_session() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let s1 = SessionInfoLite::new(SessionId::new(), ag);
        let s2 = SessionInfoLite::new(SessionId::new(), ag);
        mgr.expose(&s1, 6000, None).await.unwrap();
        mgr.expose(&s2, 6001, None).await.unwrap();
        assert_eq!(mgr.active_count().await, 2);
        mgr.close_all_for_session(s1.session_id, TeardownReason::SessionStop)
            .await;
        assert_eq!(mgr.active_count().await, 1, "only s1's preview closed");
        mgr.close_all_for_session(s2.session_id, TeardownReason::Shutdown)
            .await;
        assert_eq!(mgr.active_count().await, 0);
    }

    // Deterministic clock injection: `reap_idle` takes an explicit `now`, so
    // the idle-timeout boundary is exercised without depending on wall-clock
    // time (equivalent determinism to a paused tokio clock, no extra feature).
    //
    // V2: idle-reaping TOMBSTONES the preview (drops upstream proxying, keeps
    // the port) rather than tearing it down — so the entry stays tracked and
    // its phase flips to `Tombstone`. Full teardown still comes from
    // close/session-stop/shutdown.
    #[tokio::test]
    async fn idle_reaper_tombstones_after_timeout_but_keeps_port() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        mgr.expose(&sess, 7000, None).await.unwrap();
        assert_eq!(mgr.active_count().await, 1);

        // Just under the timeout: still live.
        let now = Instant::now();
        mgr.reap_idle(now + StdDuration::from_secs(29 * 60), PREVIEW_IDLE_TIMEOUT)
            .await;
        assert_eq!(mgr.active_count().await, 1);
        {
            let entries = mgr.entries.lock().await;
            assert_eq!(entries[0].proxy.phase(), PreviewPhase::Live);
        }

        // Past the timeout: tombstoned, but still tracked (port kept for the
        // one-shot re-expose).
        mgr.reap_idle(now + StdDuration::from_secs(31 * 60), PREVIEW_IDLE_TIMEOUT)
            .await;
        assert_eq!(
            mgr.active_count().await,
            1,
            "tombstoned entry stays tracked"
        );
        {
            let entries = mgr.entries.lock().await;
            assert_eq!(entries[0].proxy.phase(), PreviewPhase::Tombstone);
        }

        // A second reap over an already-tombstoned entry is a no-op.
        mgr.reap_idle(now + StdDuration::from_secs(62 * 60), PREVIEW_IDLE_TIMEOUT)
            .await;
        assert_eq!(mgr.active_count().await, 1);

        // Teardown (close) still releases the port.
        mgr.close(sess.session_id, 7000).await.unwrap();
        assert_eq!(mgr.active_count().await, 0);
    }

    #[tokio::test]
    async fn reaper_shutdown_tears_down_all() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        mgr.expose(&sess, 8000, None).await.unwrap();
        assert_eq!(mgr.active_count().await, 1);
        let shutdown = CancellationToken::new();
        mgr.spawn_reaper(shutdown.clone());
        shutdown.cancel();
        // Give the reaper task a moment to observe the cancellation.
        for _ in 0..50 {
            if mgr.active_count().await == 0 {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        assert_eq!(mgr.active_count().await, 0);
    }

    // -----------------------------------------------------------------------
    // V2 part 1: one-tap enable-preview approval card.
    // -----------------------------------------------------------------------

    use copperclaw_db::tables::messaging_group_agents::{UpsertWiring, upsert as upsert_wiring};
    use copperclaw_db::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
    use copperclaw_types::{ChannelType, EngageMode, SessionMode};

    /// Wire the group to a primary messaging group so the enable-preview card
    /// has somewhere to go. Returns the messaging group's platform id.
    fn wire_primary_channel(db: &CentralDb, ag: AgentGroupId) -> String {
        let mg = upsert_mg(
            db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "chat-1".into(),
                name: Some("primary".into()),
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_wiring(
            db,
            UpsertWiring {
                messaging_group_id: mg.id,
                agent_group_id: ag,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "all".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();
        mg.platform_id
    }

    #[tokio::test]
    async fn disabled_expose_raises_card_then_approve_lets_retry_succeed() {
        use copperclaw_modules::context::MockDispatcher;

        let db = central();
        let ag = group_with_preview(&db, false, None); // previews OFF
        let platform_id = wire_primary_channel(&db, ag);
        let mgr = manager(db.clone(), Some("127.0.0.1".into()));
        let mock = MockDispatcher::new();
        let d: Arc<dyn DeliveryDispatcher> = mock.clone();
        mgr.set_approval_dispatcher(d);
        let sess = SessionInfoLite::new(SessionId::new(), ag);

        // 1. Disabled expose → still refused (secure by default), but a card
        //    and a pending approval are raised.
        let err = mgr.expose(&sess, 3000, None).await.unwrap_err();
        assert!(matches!(err, PreviewError::Disabled { .. }));
        let rows =
            pending_approvals::list(&db, Some("enable_preview"), Some(ApprovalStatus::Pending))
                .unwrap();
        assert_eq!(rows.len(), 1, "one enable_preview approval raised");
        let approval_id = rows[0].approval_id;
        assert_eq!(rows[0].agent_group_id, Some(ag));
        // Card dispatched to the primary channel, carrying approve/deny buttons.
        assert_eq!(mock.dispatched_count(), 1);
        {
            let dispatched = mock.dispatched.lock().unwrap();
            let (target, msg) = &dispatched[0];
            assert_eq!(target.platform_id.as_deref(), Some(platform_id.as_str()));
            assert_eq!(msg.kind, MessageKind::Card);
            let buttons = msg.content["card"]["buttons"].as_array().unwrap();
            assert_eq!(
                buttons[0]["value"],
                format!("approve:{}", approval_id.as_uuid())
            );
            // Body reuses the copy-pasteable cclaw fix text.
            assert!(
                msg.content["card"]["body"]
                    .as_str()
                    .unwrap()
                    .contains("preview_enabled=true")
            );
        }

        // 2. A retry while still disabled must NOT raise a second card.
        let _ = mgr.expose(&sess, 3000, None).await.unwrap_err();
        assert_eq!(mock.dispatched_count(), 1, "no duplicate card on retry");
        assert_eq!(
            pending_approvals::list(&db, Some("enable_preview"), Some(ApprovalStatus::Pending))
                .unwrap()
                .len(),
            1
        );

        // 3. Operator taps Enable → resolves through the shared CLI DB path.
        crate::handlers::approvals::resolve_approve(&db, approval_id, "owner-1").unwrap();
        assert!(
            container_configs::get(&db, ag)
                .unwrap()
                .unwrap()
                .preview_enabled
        );

        // 4. The agent's retry now succeeds.
        let ok = mgr.expose(&sess, 3000, None).await.unwrap();
        assert!(ok.url.contains("/__preview/"));
        mgr.close(sess.session_id, 3000).await.unwrap();
    }

    #[tokio::test]
    async fn disabled_expose_without_dispatcher_is_plain_error() {
        // No dispatcher wired (pre-V2 behaviour): the disabled path stays a
        // plain error with no card and no approval row.
        let db = central();
        let ag = group_with_preview(&db, false, None);
        wire_primary_channel(&db, ag);
        let mgr = manager(db.clone(), Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        let err = mgr.expose(&sess, 3000, None).await.unwrap_err();
        assert!(matches!(err, PreviewError::Disabled { .. }));
        assert!(
            pending_approvals::list(&db, Some("enable_preview"), None)
                .unwrap()
                .is_empty()
        );
    }

    // -----------------------------------------------------------------------
    // V2 part 2: tombstone recovery — one re-expose per token, then terminal.
    // -----------------------------------------------------------------------

    /// Build a bare `ProxyState` in Tombstone phase for unit-testing the
    /// recovery state machine directly (no bound port needed).
    fn tombstoned_state(runtime: Arc<dyn ContainerRuntime>) -> ProxyState {
        ProxyState {
            token: "tok".into(),
            last_activity: Arc::new(StdMutex::new(Instant::now())),
            client: reqwest::Client::new(),
            inner: StdMutex::new(ProxyStateInner {
                phase: PreviewPhase::Tombstone,
                upstream: String::new(),
                recovery_used: false,
            }),
            runtime,
            central: central(),
            session_id: SessionId::new(),
            agent_group_id: AgentGroupId::new(),
            container_name: "copperclaw-x".into(),
            container_port: 3000,
            host_port: 8100,
        }
    }

    #[tokio::test]
    async fn try_recover_is_one_shot_per_token() {
        let rt: Arc<dyn ContainerRuntime> = Arc::new(IpRuntime {
            ip: Some("127.0.0.1".into()),
        });
        let state = tombstoned_state(rt);
        // First recovery succeeds and flips to Live.
        assert!(state.try_recover().await);
        assert_eq!(state.phase(), PreviewPhase::Live);
        // Simulate a second idle-reap.
        state.tombstone();
        assert_eq!(state.phase(), PreviewPhase::Tombstone);
        // The one recovery is spent → terminal.
        assert!(!state.try_recover().await);
        assert_eq!(state.phase(), PreviewPhase::Tombstone);
    }

    #[tokio::test]
    async fn try_recover_terminal_when_container_gone() {
        // Container no longer up → recovery refused, but the one-shot budget is
        // NOT consumed (a later attempt after the container returns can still
        // recover).
        let rt: Arc<dyn ContainerRuntime> = Arc::new(IpRuntime { ip: None });
        let state = tombstoned_state(rt);
        assert!(!state.try_recover().await);
        assert_eq!(state.phase(), PreviewPhase::Tombstone);
        assert!(!state.inner.lock().unwrap().recovery_used);
    }

    #[tokio::test]
    async fn tombstone_serves_page_and_recovers_once_end_to_end() {
        let upstream_port = spawn_upstream().await;
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        let exposed = mgr
            .expose(&sess, upstream_port, Some("demo".into()))
            .await
            .unwrap();
        let token = exposed.url.split("/__preview/").nth(1).unwrap().to_string();
        let base = exposed.url.split("/__preview/").next().unwrap().to_string();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        // Prime the cookie and confirm the app proxies live.
        assert_eq!(client.get(&exposed.url).send().await.unwrap().status(), 302);
        let cookied = |path: String| {
            let c = client.clone();
            let tok = token.clone();
            async move {
                c.get(path)
                    .header(reqwest::header::COOKIE, format!("cclaw_preview={tok}"))
                    .send()
                    .await
                    .unwrap()
            }
        };
        let r = cookied(format!("{base}/")).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "hello from the app");

        // Idle-reap → tombstone. Port stays; a normal request shows the page.
        mgr.reap_idle(
            Instant::now() + StdDuration::from_secs(31 * 60),
            PREVIEW_IDLE_TIMEOUT,
        )
        .await;
        assert_eq!(mgr.active_count().await, 1);
        let r = cookied(format!("{base}/")).await;
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("expired"));

        // Tokened GET recovers it ONCE → 302 mint + redirect, live again.
        assert_eq!(client.get(&exposed.url).send().await.unwrap().status(), 302);
        let r = cookied(format!("{base}/")).await;
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await.unwrap(), "hello from the app");

        // Reap again → tombstone; the second tokened GET is terminal (200 page,
        // NOT a 302), telling the user to ask the agent to re-expose.
        mgr.reap_idle(
            Instant::now() + StdDuration::from_secs(31 * 60),
            PREVIEW_IDLE_TIMEOUT,
        )
        .await;
        let r = client.get(&exposed.url).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("expose_preview"));

        // Full teardown still releases the port.
        mgr.close(sess.session_id, upstream_port).await.unwrap();
        assert_eq!(mgr.active_count().await, 0);
    }

    #[tokio::test]
    async fn agent_reexpose_revives_a_tombstoned_preview() {
        let upstream_port = spawn_upstream().await;
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        let a = mgr.expose(&sess, upstream_port, None).await.unwrap();

        // Reap → tombstone.
        mgr.reap_idle(
            Instant::now() + StdDuration::from_secs(31 * 60),
            PREVIEW_IDLE_TIMEOUT,
        )
        .await;
        {
            let entries = mgr.entries.lock().await;
            assert_eq!(entries[0].proxy.phase(), PreviewPhase::Tombstone);
        }

        // The agent explicitly re-exposes the same port: same URL, revived Live,
        // fresh recovery budget.
        let b = mgr.expose(&sess, upstream_port, None).await.unwrap();
        assert_eq!(a.url, b.url, "re-expose is idempotent on the URL");
        {
            let entries = mgr.entries.lock().await;
            assert_eq!(entries[0].proxy.phase(), PreviewPhase::Live);
            assert!(!entries[0].proxy.inner.lock().unwrap().recovery_used);
        }
        mgr.close(sess.session_id, upstream_port).await.unwrap();
    }

    // ── M19 A3: public-tunnel verb (PublicPreviewTunnel) ─────────────────

    use copperclaw_modules::{
        CloudflaredProvider, PublicTunnelBroker, PublicTunnelReply, TUNNEL_APPROVAL_ACTION,
        TunnelBroker,
    };

    /// Write an executable mock cloudflared that answers `--version` instantly
    /// and, on `tunnel`, prints a quick-tunnel URL then lingers like the real
    /// binary. Returns the tempdir guard + the binary path.
    fn mock_cloudflared(url: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mock-cloudflared");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "if [ \"$1\" = \"--version\" ]; then").unwrap();
        writeln!(f, "  echo 'cloudflared version 0.0.0-mock'; exit 0").unwrap();
        writeln!(f, "fi").unwrap();
        writeln!(f, "echo 'INF |  {url}  |'").unwrap();
        writeln!(f, "sleep 30").unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        (dir, path)
    }

    /// Seed a real session row for `ag` so the tunnel approval row's FKs
    /// (`pending_approvals.session_id → sessions`) are satisfiable.
    fn seed_session(db: &CentralDb, ag: AgentGroupId) -> SessionId {
        use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
        create_session(
            db,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
        .id
    }

    /// Approve the single outstanding tunnel approval row (mirrors the host
    /// `resolve_approve` apply arm: flip status + record the decision).
    fn approve_tunnel(db: &CentralDb) {
        use copperclaw_db::tables::pending_approvals::{
            self, ApprovalStatus, DecisionOutcome, record_decision, update_status,
        };
        let rows = pending_approvals::list(db, Some(TUNNEL_APPROVAL_ACTION), None).unwrap();
        let row = rows.first().expect("a pending tunnel approval");
        update_status(db, row.approval_id, ApprovalStatus::Approved).unwrap();
        record_decision(
            db,
            row.approval_id,
            TUNNEL_APPROVAL_ACTION,
            DecisionOutcome::Approve,
            "operator",
            None,
        )
        .unwrap();
    }

    fn public_tunnel(
        preview: &Arc<PreviewManager>,
        db: &CentralDb,
        binary: &std::path::Path,
        enabled: bool,
    ) -> Arc<PublicPreviewTunnel> {
        let provider: Arc<dyn copperclaw_modules::TunnelProvider> = Arc::new(
            CloudflaredProvider::with_binary(binary).with_open_timeout(StdDuration::from_secs(30)),
        );
        let broker = TunnelBroker::new(db.clone(), provider);
        preview.set_tunnel_broker(Arc::clone(&broker));
        PublicPreviewTunnel::new(Arc::clone(preview), broker, db.clone(), enabled)
    }

    #[tokio::test]
    async fn make_public_full_flow_approve_surface_url_teardown() {
        let upstream_port = spawn_upstream().await;
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let session_id = seed_session(&db, ag);
        let mgr = manager(db.clone(), Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(session_id, ag);

        // A live LAN preview first, so the tunnel has a host port to front.
        let exposed = mgr.expose(&sess, upstream_port, None).await.unwrap();
        let token = exposed.url.split("/__preview/").nth(1).unwrap().to_string();

        let (_guard, bin) = mock_cloudflared("https://cofounder-demo.trycloudflare.com");
        let pt = public_tunnel(&mgr, &db, &bin, true);

        // 1. First make_public ⇒ pending approval (no tunnel yet).
        match pt.make_public(sess, upstream_port).await {
            PublicTunnelReply::Pending { note } => assert!(note.contains("approval")),
            other => panic!("expected Pending, got {other:?}"),
        }
        assert_eq!(mgr.tunnel_broker.get().unwrap().active_count().await, 0);

        // 2. Operator approves.
        approve_tunnel(&db);

        // 3. Re-call ⇒ the tokened PUBLIC url is surfaced.
        let public_url = match pt.make_public(sess, upstream_port).await {
            PublicTunnelReply::Exposed { public_url, note } => {
                assert!(note.contains("PUBLIC"));
                public_url
            }
            other => panic!("expected Exposed, got {other:?}"),
        };
        assert_eq!(
            public_url,
            format!("https://cofounder-demo.trycloudflare.com/__preview/{token}"),
            "public url fronts the token-gated proxy"
        );
        assert_eq!(mgr.tunnel_broker.get().unwrap().active_count().await, 1);

        // 4. Closing the preview tears the tunnel down with it (A3 auto-teardown).
        mgr.close(session_id, upstream_port).await.unwrap();
        assert_eq!(mgr.tunnel_broker.get().unwrap().active_count().await, 0);
    }

    #[tokio::test]
    async fn make_public_without_live_preview_is_clean_error() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let session_id = seed_session(&db, ag);
        let mgr = manager(db.clone(), Some("127.0.0.1".into()));
        let (_guard, bin) = mock_cloudflared("https://x.trycloudflare.com");
        let pt = public_tunnel(&mgr, &db, &bin, true);
        let sess = SessionInfoLite::new(session_id, ag);

        // No expose_preview first ⇒ nothing live to front.
        match pt.make_public(sess, 3000).await {
            PublicTunnelReply::Error(msg) => {
                assert!(msg.contains("no live preview"), "got {msg}");
                assert!(msg.contains("expose_preview"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn make_public_off_by_default_is_not_enabled() {
        let upstream_port = spawn_upstream().await;
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let session_id = seed_session(&db, ag);
        let mgr = manager(db.clone(), Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(session_id, ag);
        mgr.expose(&sess, upstream_port, None).await.unwrap();

        let (_guard, bin) = mock_cloudflared("https://x.trycloudflare.com");
        // Host env master switch OFF ⇒ NotEnabled, no approval raised.
        let pt = public_tunnel(&mgr, &db, &bin, false);
        match pt.make_public(sess, upstream_port).await {
            PublicTunnelReply::Error(msg) => assert!(msg.contains("OFF"), "got {msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(
            pending_approvals::list(&db, Some(TUNNEL_APPROVAL_ACTION), None)
                .unwrap()
                .is_empty(),
            "an off-by-default request raises no approval"
        );
    }

    #[tokio::test]
    async fn make_public_absent_binary_is_clean_error() {
        let upstream_port = spawn_upstream().await;
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let session_id = seed_session(&db, ag);
        let mgr = manager(db.clone(), Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(session_id, ag);
        mgr.expose(&sess, upstream_port, None).await.unwrap();

        let pt = public_tunnel(&mgr, &db, std::path::Path::new("/nonexistent/cf"), true);
        match pt.make_public(sess, upstream_port).await {
            PublicTunnelReply::Error(msg) => {
                assert!(msg.contains("was not found"), "got {msg}");
                assert!(msg.contains("cloudflared"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
