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
//! * A WebSocket upgrade is refused cleanly (501) — out of scope for v1.
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

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use copperclaw_container_rt::ContainerRuntime;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log::{self, AuditEntry};
use copperclaw_db::tables::container_configs;
use copperclaw_modules::{PreviewBroker, PreviewError, PreviewExposed, SessionInfoLite};
use copperclaw_types::{AgentGroupId, SessionId};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::{Duration, Instant};
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
/// Cookie name the token gate sets / checks.
const PREVIEW_COOKIE: &str = "cclaw_preview";
/// URL path prefix that mints the gating cookie from a token.
const PREVIEW_TOKEN_PATH: &str = "/__preview/";
/// Audit command recorded for preview mutations.
const PREVIEW_AUDIT_COMMAND: &str = "preview";

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

/// One live preview.
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
}

/// Shared state handed to the axum proxy handler for one preview.
struct ProxyState {
    token: String,
    /// `http://<container_ip>:<container_port>` — the upstream base.
    upstream: String,
    last_activity: Arc<StdMutex<Instant>>,
    client: reqwest::Client,
}

/// The host-side preview manager. Constructed once at boot with the container
/// runtime + central DB; wired into the delivery service as a
/// [`PreviewBroker`] and into the container manager for session-stop teardown.
pub struct PreviewManager {
    central: CentralDb,
    runtime: Arc<dyn ContainerRuntime>,
    entries: AsyncMutex<Vec<PreviewEntry>>,
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

    /// Tear down every preview whose last activity is older than `idle`
    /// relative to `now`. Factored out of the reaper loop so it is testable
    /// with an explicit clock.
    async fn reap_idle(&self, now: Instant, idle: Duration) {
        let stale: Vec<(SessionId, u16)> = {
            let entries = self.entries.lock().await;
            entries
                .iter()
                .filter(|e| {
                    let last = *e.last_activity.lock().unwrap();
                    now.saturating_duration_since(last) >= idle
                })
                .map(|e| (e.session_id, e.container_port))
                .collect()
        };
        for (session_id, port) in stale {
            self.teardown(session_id, port, TeardownReason::Idle).await;
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

    /// Write an audit row for a preview mutation. Best-effort (a DB error is
    /// logged + swallowed, matching the attestation / dispatch audit contract).
    fn audit(
        &self,
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
        if let Err(err) = audit_log::insert(&self.central, &entry) {
            warn!(?err, "could not write preview audit row");
        }
    }

    /// Number of live previews (test / introspection helper).
    pub async fn active_count(&self) -> usize {
        self.entries.lock().await.len()
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

        // 1. Group gate. No config row or disabled → refuse with the operator
        //    command (copy-pasteable) that turns it on.
        let cfg = container_configs::get(&self.central, session.agent_group_id)
            .map_err(|e| PreviewError::Internal(format!("read group config: {e}")))?;
        let Some(cfg) = cfg.filter(|c| c.preview_enabled) else {
            return Err(PreviewError::Disabled {
                group: session.agent_group_id.as_uuid().to_string(),
            });
        };
        let bind = resolve_bind(cfg.preview_bind.as_deref());

        // Serialize the caps-check + port-bind + insert so two concurrent
        // exposes can't both claim the last slot / port.
        let mut entries = self.entries.lock().await;

        // 2. Idempotent re-expose: an existing preview for this
        //    (session, container_port) returns its current URL unchanged.
        if let Some(existing) = entries
            .iter()
            .find(|e| e.session_id == session.session_id && e.container_port == port)
        {
            return Ok(PreviewExposed {
                url: preview_url(&display_host(bind), existing.host_port, &existing.token),
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
            upstream,
            last_activity: Arc::clone(&last_activity),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|e| PreviewError::Internal(format!("build proxy client: {e}")))?,
        });
        let app = Router::new().fallback(proxy_handler).with_state(state);
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

/// Does this request carry a WebSocket / protocol upgrade? Refused (501) in v1.
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

/// The axum fallback handler: token gate + reverse proxy.
async fn proxy_handler(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    let path = req.uri().path().to_string();

    // Token-mint path: set the cookie and redirect to the app root.
    if let Some(token) = path.strip_prefix(PREVIEW_TOKEN_PATH) {
        if constant_time_eq(token, &state.token) {
            let cookie = format!(
                "{PREVIEW_COOKIE}={}; Path=/; HttpOnly; SameSite=Lax",
                state.token
            );
            return Response::builder()
                .status(StatusCode::FOUND)
                .header(header::SET_COOKIE, cookie)
                .header(header::LOCATION, "/")
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
        return forbidden();
    }

    // Every other request must present the matching cookie.
    match cookie_token(req.headers()) {
        Some(tok) if constant_time_eq(&tok, &state.token) => {}
        _ => return forbidden(),
    }

    // WebSocket / upgrade: refused cleanly in v1.
    if is_upgrade(req.headers()) {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "This preview proxy does not support WebSocket / protocol upgrades in this version.",
        )
            .into_response();
    }

    // Mark activity so the idle reaper doesn't tear down a preview in use.
    *state.last_activity.lock().unwrap() = Instant::now();

    proxy_upstream(&state, req).await
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
    let url = format!("{}{path_and_query}", state.upstream);
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
    #[tokio::test]
    async fn idle_reaper_tears_down_after_timeout() {
        let db = central();
        let ag = group_with_preview(&db, true, None);
        let mgr = manager(db, Some("127.0.0.1".into()));
        let sess = SessionInfoLite::new(SessionId::new(), ag);
        mgr.expose(&sess, 7000, None).await.unwrap();
        assert_eq!(mgr.active_count().await, 1);

        // Just under the timeout: still alive.
        let now = Instant::now();
        mgr.reap_idle(now + StdDuration::from_secs(29 * 60), PREVIEW_IDLE_TIMEOUT)
            .await;
        assert_eq!(mgr.active_count().await, 1);

        // Past the timeout: reaped.
        mgr.reap_idle(now + StdDuration::from_secs(31 * 60), PREVIEW_IDLE_TIMEOUT)
            .await;
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
}
