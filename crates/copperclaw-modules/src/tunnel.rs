//! Public tunnel module (M18 V5).
//!
//! The session-preview proxy ([`crate::preview`]) exposes an in-container app to
//! the operator's machine / LAN. That is enough to test on your own network but
//! fails the "send it to my cofounder" moment — the preview URL is a LAN address.
//! This module fronts a **live preview's host port** with a *public* tunnel via
//! an **operator-provided** tunnel binary (cloudflared first; the
//! [`TunnelProvider`] trait is shaped so tailscale-funnel can slot in later).
//!
//! ## Non-negotiable safety posture
//!
//! Exposing a service to the public internet is the single most dangerous thing
//! this runtime can do on a user's behalf, so every guard rail is mandatory and
//! fail-closed:
//!
//! * **OFF by default, per-group opt-in.** [`TunnelExposeRequest::enabled`] is
//!   sourced host-side from the agent group's config; a group that has not opted
//!   in gets [`TunnelError::NotEnabled`] and no tunnel is ever attempted.
//! * **Every exposure is approval-gated.** [`TunnelBroker::expose`] raises a
//!   `CredentialedExternalAction` approval (the G1 in-chat / CLI approval flow)
//!   and refuses to stand up a tunnel until an authorised approver has tapped
//!   Approve. There is no path from an agent request to a public URL that does
//!   not pass through a human decision recorded in `pending_approvals`.
//! * **Audit-rowed.** Every request, exposure, and teardown writes an
//!   `audit_log` row (command `tunnel`) so `cclaw audit list` shows the full
//!   lifecycle of every public exposure.
//! * **Auto-teardown with the preview it fronts.** A tunnel is only ever a front
//!   for a live preview; when that preview closes or is reaped the host calls
//!   [`TunnelBroker::close_for_preview`] (or [`TunnelBroker::close_all_for_session`]
//!   on container stop) and the tunnel process is killed — no orphaned public
//!   tunnels outlive the app they exposed.
//! * **Never bundles binaries.** The tunnel binary is operator-provided. When it
//!   is absent [`TunnelProvider::preflight`] returns [`TunnelError::BinaryNotFound`]
//!   with copy-pasteable install instructions — a clean actionable error, never
//!   a panic or a silent no-op.
//!
//! ## Flow
//!
//! 1. The agent (via the host relay) asks to make a preview public.
//! 2. [`TunnelBroker::expose`] checks the group opted in, preflights the binary,
//!    and — on the first call — writes a `CredentialedExternalAction` pending
//!    approval and returns [`TunnelOutcome::Pending`]. The host posts the G1
//!    approval card.
//! 3. An approver taps Approve; the G1 interceptor / CLI resolves the row via the
//!    shared DB decision path (`resolve_approve` → the
//!    `credentialed_external_action` apply arm), flipping it to `approved`.
//! 4. The agent retries; [`TunnelBroker::expose`] now finds the approved grant,
//!    spawns the tunnel binary, parses the public URL it advertises, tracks it
//!    for teardown, audits the exposure, and returns
//!    [`TunnelOutcome::Exposed`] with the shareable public URL the runner relays
//!    into the P3 ritual card.
//! 5. When the fronted preview closes, the host tears the tunnel down.

use crate::context::{Module, ModuleContext};
use crate::error::ModuleError;
use async_trait::async_trait;
use chrono::Utc;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log::{self, AuditEntry};
use copperclaw_db::tables::pending_approvals::{self, ApprovalStatus, UpsertPendingApproval};
use copperclaw_types::{AgentGroupId, ApprovalId, ChannelType, SessionId};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

/// The `pending_approvals.action` string every tunnel exposure is gated on. It
/// deliberately matches [`copperclaw_types::ApprovalKind::CredentialedExternalAction`]
/// (`snake_case`) so the host's generic approve dispatcher routes a tapped card to
/// the `credentialed_external_action` apply arm.
pub const TUNNEL_APPROVAL_ACTION: &str = "credentialed_external_action";

/// The `audit_log.command` recorded for every tunnel mutation.
pub const TUNNEL_AUDIT_COMMAND: &str = "tunnel";

/// Default tunnel binary name, resolved from `PATH`. Overridable via the
/// `COPPERCLAW_TUNNEL_BINARY` env var (operator points it at a full path).
const DEFAULT_CLOUDFLARED_BINARY: &str = "cloudflared";

/// Env var an operator sets to point at a non-`PATH` tunnel binary.
const TUNNEL_BINARY_ENV: &str = "COPPERCLAW_TUNNEL_BINARY";

/// How long [`CloudflaredProvider::open`] waits for the binary to advertise a
/// public URL before giving up and tearing the child down.
const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded retries on a transient `ExecutableFileBusy` (ETXTBSY) when spawning
/// the tunnel binary. ETXTBSY means the binary was momentarily open for writing
/// — e.g. the operator just installed/updated `cloudflared`, or a concurrent
/// `fork` in another thread transiently held a writable fd across the `exec`.
/// A short bounded backoff clears the window; a genuinely-stuck binary still
/// fails cleanly with [`TunnelError::Spawn`].
const SPAWN_ETXTBSY_RETRIES: u8 = 20;
/// Backoff between [`SPAWN_ETXTBSY_RETRIES`] attempts.
const SPAWN_ETXTBSY_BACKOFF: Duration = Duration::from_millis(25);

/// True for a transient "text file busy" spawn error (see
/// [`SPAWN_ETXTBSY_RETRIES`]).
fn is_text_busy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ExecutableFileBusy
}

/// Copy-pasteable install instructions surfaced when the tunnel binary is
/// absent. Copperclaw never bundles it — the operator provides it.
fn cloudflared_install_hint() -> String {
    "Copperclaw never bundles the tunnel binary — install cloudflared and put it \
     on PATH (or set COPPERCLAW_TUNNEL_BINARY to its full path):\n  \
     - Debian/Ubuntu: install the `cloudflared` package from https://pkg.cloudflare.com/\n  \
     - macOS: brew install cloudflared\n  \
     - Any OS: download a release binary from \
     https://github.com/cloudflare/cloudflared/releases\n\
     then ask the agent to expose the preview again."
        .to_string()
}

/// The operator-facing note relayed alongside a public URL. Blunt about the
/// blast radius on purpose.
fn exposure_note() -> String {
    "This is a PUBLIC link — anyone on the internet who has it can reach this \
     app. It stays up only while the preview is open and is torn down \
     automatically when the preview closes."
        .to_string()
}

/// The approval-card title for a public-tunnel exposure. Names the app (when
/// the preview carries one), the host port, and the session (M24 S1 / M19 A3
/// follow-up 2) so an operator with several concurrent previews can tell
/// exactly which exposure they are approving.
fn tunnel_card_title(req: &TunnelExposeRequest) -> String {
    let session = req.session_id.as_uuid();
    match req
        .preview_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(name) => format!(
            "Expose preview \"{name}\" (host port {}, session {session}) to the public internet?",
            req.host_port
        ),
        None => format!(
            "Expose the preview on host port {} (session {session}) to the public internet?",
            req.host_port
        ),
    }
}

/// The note returned while an exposure is waiting on an approver's tap.
fn pending_note() -> String {
    "Public exposure needs operator approval. An approval card has been sent; \
     once an operator taps Approve, ask to expose the preview again and the \
     public link will be created."
        .to_string()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a tunnel `expose` / provider `open` could not be satisfied. Every variant
/// renders (via `Display`) into text written for the agent to read and, where an
/// operator action is required, to relay.
#[derive(Debug, Error)]
pub enum TunnelError {
    /// The agent group has not opted into public tunnels (the secure default).
    #[error(
        "Public tunnels are OFF for agent group {group}. An operator must opt \
         this group into public exposure before a preview can be made public; \
         previews stay LAN-only until then."
    )]
    NotEnabled { group: String },

    /// The operator-provided tunnel binary is absent. Message names the binary
    /// and the exact install steps.
    #[error("The tunnel binary `{binary}` was not found.\n{install_hint}")]
    BinaryNotFound {
        binary: String,
        install_hint: String,
    },

    /// The approver declined the exposure. No tunnel is created.
    #[error(
        "The operator declined to make this preview public (approval {approval_id} \
         was denied)."
    )]
    Denied { approval_id: String },

    /// A bad argument (empty upstream, port 0, …).
    #[error("Invalid tunnel request: {0}")]
    BadRequest(String),

    /// The binary was found but could not be started (e.g. not executable).
    #[error("Could not start the tunnel binary `{binary}`: {source}")]
    Spawn {
        binary: String,
        source: std::io::Error,
    },

    /// The binary ran but never advertised a public URL within the timeout.
    #[error(
        "Timed out after {seconds}s waiting for `{binary}` to report a public \
         URL. The tunnel binary started but did not print a usable URL — check \
         that it is a working cloudflared and that the app is listening."
    )]
    Timeout { binary: String, seconds: u64 },

    /// The binary exited (or closed its output) before advertising a URL.
    #[error(
        "The tunnel binary `{binary}` exited before reporting a public URL. \
         Check that it is a working cloudflared build."
    )]
    NoUrl { binary: String },

    /// A host-side bookkeeping failure (DB, etc.).
    #[error("Tunnel bookkeeping error: {0}")]
    Internal(String),
}

// ---------------------------------------------------------------------------
// Provider trait + the concrete cloudflared provider
// ---------------------------------------------------------------------------

/// A freshly opened tunnel process and the public URL it advertised. Owns the
/// child so the broker can kill it on teardown; dropping it also kills the child
/// (the spawn sets `kill_on_drop`), so a lost handle never leaks a public
/// tunnel.
#[derive(Debug)]
pub struct OpenedTunnel {
    public_url: String,
    child: Option<Child>,
}

impl OpenedTunnel {
    /// Wrap a spawned tunnel child + its advertised URL.
    #[must_use]
    pub fn new(public_url: String, child: Child) -> Self {
        Self {
            public_url,
            child: Some(child),
        }
    }

    /// A tunnel with no child to reap (a provider that manages its own process
    /// lifecycle out of band — e.g. a future named-tunnel daemon).
    #[must_use]
    pub fn detached(public_url: String) -> Self {
        Self {
            public_url,
            child: None,
        }
    }

    /// The public URL the tunnel advertised.
    #[must_use]
    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// Kill the tunnel process and reap it. Idempotent.
    pub async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

/// A tunnel backend. `cloudflared` is the first implementation; the trait is the
/// seam a `tailscale-funnel` (or named-tunnel) provider slots into later.
#[async_trait]
pub trait TunnelProvider: Send + Sync {
    /// Stable provider name for diagnostics / audit (`"cloudflared"`).
    fn name(&self) -> &'static str;

    /// The binary this provider drives (for error text).
    fn binary(&self) -> String;

    /// Confirm the operator-provided binary is present and runnable WITHOUT
    /// opening a tunnel. Returns [`TunnelError::BinaryNotFound`] with install
    /// instructions when absent — never panics.
    async fn preflight(&self) -> Result<(), TunnelError>;

    /// Open a public tunnel fronting `upstream` (e.g. `http://127.0.0.1:8100`),
    /// returning the live child + the public URL it advertised.
    async fn open(&self, upstream: &str) -> Result<OpenedTunnel, TunnelError>;
}

/// Drives an operator-provided `cloudflared` binary as a quick tunnel
/// (`cloudflared tunnel --url <upstream>`), parsing the `*.trycloudflare.com`
/// URL it prints. No credentials are handled here: a quick tunnel is anonymous,
/// and any account credentials a named tunnel would need live in the operator's
/// own `cloudflared` config outside this process — this module never reads,
/// stores, or forwards a Cloudflare token.
pub struct CloudflaredProvider {
    binary: PathBuf,
    open_timeout: Duration,
}

impl Default for CloudflaredProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudflaredProvider {
    /// Build a provider using `COPPERCLAW_TUNNEL_BINARY` if set, else
    /// `cloudflared` from `PATH`.
    #[must_use]
    pub fn new() -> Self {
        let binary = std::env::var_os(TUNNEL_BINARY_ENV)
            .map_or_else(|| PathBuf::from(DEFAULT_CLOUDFLARED_BINARY), PathBuf::from);
        Self {
            binary,
            open_timeout: DEFAULT_OPEN_TIMEOUT,
        }
    }

    /// Point the provider at a specific binary path (tests, non-`PATH` installs).
    #[must_use]
    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self {
            binary: path.into(),
            open_timeout: DEFAULT_OPEN_TIMEOUT,
        }
    }

    /// Override how long [`Self::open`] waits for the URL (tests use a short
    /// timeout).
    #[must_use]
    pub fn with_open_timeout(mut self, timeout: Duration) -> Self {
        self.open_timeout = timeout;
        self
    }

    fn binary_display(&self) -> String {
        self.binary.to_string_lossy().into_owned()
    }

    fn not_found(&self) -> TunnelError {
        TunnelError::BinaryNotFound {
            binary: self.binary_display(),
            install_hint: cloudflared_install_hint(),
        }
    }
}

#[async_trait]
impl TunnelProvider for CloudflaredProvider {
    fn name(&self) -> &'static str {
        "cloudflared"
    }

    fn binary(&self) -> String {
        self.binary_display()
    }

    async fn preflight(&self) -> Result<(), TunnelError> {
        // Run `--version` as a cheap "is it there and runnable" probe. Any exit
        // status counts as present; only a spawn error is disqualifying.
        let mut attempt = 0u8;
        loop {
            match Command::new(&self.binary)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(self.not_found()),
                Err(e) if is_text_busy(&e) && attempt < SPAWN_ETXTBSY_RETRIES => {
                    attempt += 1;
                    tokio::time::sleep(SPAWN_ETXTBSY_BACKOFF).await;
                }
                Err(source) => {
                    return Err(TunnelError::Spawn {
                        binary: self.binary_display(),
                        source,
                    });
                }
            }
        }
    }

    async fn open(&self, upstream: &str) -> Result<OpenedTunnel, TunnelError> {
        let mut attempt = 0u8;
        let mut child = loop {
            match Command::new(&self.binary)
                .arg("tunnel")
                .arg("--url")
                .arg(upstream)
                .arg("--no-autoupdate")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(child) => break child,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(self.not_found());
                }
                Err(e) if is_text_busy(&e) && attempt < SPAWN_ETXTBSY_RETRIES => {
                    attempt += 1;
                    tokio::time::sleep(SPAWN_ETXTBSY_BACKOFF).await;
                }
                Err(source) => {
                    return Err(TunnelError::Spawn {
                        binary: self.binary_display(),
                        source,
                    });
                }
            }
        };

        // cloudflared prints the quick-tunnel URL to stderr inside a box-drawing
        // banner; a mock binary may print to stdout. Scan BOTH concurrently and
        // take the first `https://` URL. `stream_count` EOFs with no URL ⇒ NoUrl.
        let (tx, mut rx) = mpsc::channel::<UrlEvent>(8);
        let mut stream_count = 0u8;
        if let Some(stdout) = child.stdout.take() {
            stream_count += 1;
            tokio::spawn(scan_stream(BufReader::new(stdout), tx.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            stream_count += 1;
            tokio::spawn(scan_stream(BufReader::new(stderr), tx.clone()));
        }
        drop(tx); // so `rx` closes once every scanner task ends

        let mut eofs = 0u8;
        let found = loop {
            match timeout(self.open_timeout, rx.recv()).await {
                Ok(Some(UrlEvent::Found(url))) => break Some(url),
                Ok(Some(UrlEvent::Eof)) => {
                    eofs += 1;
                    if eofs >= stream_count {
                        break None;
                    }
                }
                Ok(None) => break None,
                Err(_elapsed) => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err(TunnelError::Timeout {
                        binary: self.binary_display(),
                        seconds: self.open_timeout.as_secs(),
                    });
                }
            }
        };

        if let Some(public_url) = found {
            info!(
                provider = self.name(),
                upstream, public_url, "public tunnel opened"
            );
            Ok(OpenedTunnel::new(public_url, child))
        } else {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(TunnelError::NoUrl {
                binary: self.binary_display(),
            })
        }
    }
}

/// Event a per-stream scanner reports back to [`CloudflaredProvider::open`].
enum UrlEvent {
    /// The first `https://` URL seen on this stream.
    Found(String),
    /// This stream reached EOF.
    Eof,
}

/// Read `reader` line by line, report the first `https://` URL, then keep
/// draining (so the child's pipe never blocks) until EOF.
async fn scan_stream<R>(reader: R, tx: mpsc::Sender<UrlEvent>)
where
    R: AsyncBufRead + Unpin + Send,
{
    let mut lines = reader.lines();
    let mut reported = false;
    while let Ok(Some(line)) = lines.next_line().await {
        if !reported {
            if let Some(url) = extract_tunnel_url(&line) {
                reported = true;
                let _ = tx.send(UrlEvent::Found(url)).await;
            }
        }
        // Keep reading after a hit to drain the pipe.
    }
    let _ = tx.send(UrlEvent::Eof).await;
}

/// Pull the first `https://…` token out of a log line, trimming the box-drawing
/// / punctuation cloudflared wraps it in. Returns `None` when there is no URL.
fn extract_tunnel_url(line: &str) -> Option<String> {
    let idx = line.find("https://")?;
    let rest = &line[idx..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '|' | '<' | '>'))
        .unwrap_or(rest.len());
    let url = rest[..end].trim_end_matches(['.', ',', ')', ']', '}']);
    if url.len() > "https://".len() {
        Some(url.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Broker
// ---------------------------------------------------------------------------

/// A request to make a live preview public. `enabled` is sourced host-side from
/// the agent group's config — this module does not decide opt-in, it enforces it.
#[derive(Debug, Clone)]
pub struct TunnelExposeRequest {
    pub session_id: SessionId,
    pub agent_group_id: AgentGroupId,
    /// The preview's host port being fronted (the local port the tunnel points
    /// at). Also the teardown key.
    pub host_port: u16,
    /// The local upstream the tunnel fronts, e.g. `http://127.0.0.1:8100`.
    pub upstream: String,
    /// Stable, non-secret identifier of the SPECIFIC preview instance being
    /// fronted (M24 S1 / M19 A3 follow-up 1). Host ports are pool-reused within
    /// a session, so `(session, host_port)` alone is not a safe approval
    /// identity: a stale approved row could re-stand-up a public tunnel for a
    /// DIFFERENT app that later reacquired the same port. The nonce is minted
    /// per preview instance host-side, so retries of the same live preview map
    /// to the same approval row (the pending-dedup UX is preserved) while a new
    /// preview — even on a reused port — always raises a FRESH approval.
    /// Required: an empty nonce is a [`TunnelError::BadRequest`] (fail-closed).
    pub preview_nonce: String,
    /// The app/project name the preview was exposed under (if the agent gave
    /// one). Rendered into the approval card title so an operator with several
    /// concurrent previews can tell which app they are approving.
    pub preview_name: Option<String>,
    /// Per-group opt-in. `false` ⇒ [`TunnelError::NotEnabled`] (secure default).
    pub enabled: bool,
    /// Channel coordinates the approval card is posted to, if the host has a
    /// primary messaging channel for the group. Stored on the approval row so
    /// the G1 interceptor edits the right card.
    pub notify: Option<(ChannelType, String)>,
}

/// A live public URL the broker stood up. Carries what the runner relays into the
/// P3 ritual card verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelExposed {
    pub public_url: String,
    pub note: String,
}

/// Outcome of [`TunnelBroker::expose`].
#[derive(Debug, Clone)]
pub enum TunnelOutcome {
    /// The tunnel is live; relay `public_url` to the operator.
    Exposed(TunnelExposed),
    /// Waiting on an approver's tap. `approval_id` is the pending row.
    Pending { approval_id: String, note: String },
}

/// One tracked live tunnel, keyed for teardown by `(session_id, host_port)`.
struct TrackedTunnel {
    session_id: SessionId,
    agent_group_id: AgentGroupId,
    host_port: u16,
    public_url: String,
    tunnel: OpenedTunnel,
}

/// Host-side broker that gates, stands up, tracks, and tears down public
/// tunnels. Holds the central DB (approvals + audit) and the tunnel provider.
pub struct TunnelBroker {
    provider: Arc<dyn TunnelProvider>,
    central: CentralDb,
    active: AsyncMutex<Vec<TrackedTunnel>>,
}

impl TunnelBroker {
    /// Construct a broker over `central` + `provider`.
    #[must_use]
    pub fn new(central: CentralDb, provider: Arc<dyn TunnelProvider>) -> Arc<Self> {
        Arc::new(Self {
            provider,
            central,
            active: AsyncMutex::new(Vec::new()),
        })
    }

    /// Stable natural key for one exposure request's approval row. Includes the
    /// per-preview nonce (M24 S1): host ports are pool-reused within a session,
    /// so keying on `(session, host_port)` alone would let a stale approved row
    /// satisfy a NEW request for a different app on a reacquired port. With the
    /// nonce in the identity, an old grant can never match a new preview
    /// instance; retries of the SAME live preview still dedup onto one row.
    fn request_id(session_id: SessionId, host_port: u16, preview_nonce: &str) -> String {
        format!(
            "tunnel:{}:{host_port}:{preview_nonce}",
            session_id.as_uuid()
        )
    }

    /// Request (and, once approved, stand up) a public tunnel fronting a preview.
    ///
    /// Fail-closed and approval-gated:
    /// * not opted in ⇒ [`TunnelError::NotEnabled`];
    /// * binary absent ⇒ [`TunnelError::BinaryNotFound`] (before any approval is
    ///   raised — no point asking to approve something that can't run);
    /// * no live grant ⇒ raises a `CredentialedExternalAction` approval and
    ///   returns [`TunnelOutcome::Pending`];
    /// * approver declined ⇒ [`TunnelError::Denied`];
    /// * approved ⇒ spawns the tunnel, tracks it, audits `expose`, and returns
    ///   [`TunnelOutcome::Exposed`].
    pub async fn expose(&self, req: TunnelExposeRequest) -> Result<TunnelOutcome, TunnelError> {
        if req.upstream.trim().is_empty() {
            return Err(TunnelError::BadRequest("upstream url is required".into()));
        }
        if req.host_port == 0 {
            return Err(TunnelError::BadRequest("host_port must be non-zero".into()));
        }
        if req.preview_nonce.trim().is_empty() {
            // Fail-closed: without a per-preview nonce the approval identity
            // would degrade back to the pool-reusable (session, host_port) key.
            return Err(TunnelError::BadRequest(
                "preview_nonce is required (per-preview approval identity)".into(),
            ));
        }

        // 1. Opt-in gate (secure default). A blocked attempt is still audited.
        if !req.enabled {
            self.audit(
                req.session_id,
                req.agent_group_id,
                "request",
                req.host_port,
                &req.upstream,
                "blocked-not-enabled",
            );
            // M19 A3: refused by the opt-in gate.
            copperclaw_metrics::inc_public_tunnel("denied", "not_enabled");
            return Err(TunnelError::NotEnabled {
                group: req.agent_group_id.as_uuid().to_string(),
            });
        }

        // 2. Idempotent: a live tunnel already fronts this (session, host_port).
        {
            let active = self.active.lock().await;
            if let Some(t) = active
                .iter()
                .find(|t| t.session_id == req.session_id && t.host_port == req.host_port)
            {
                return Ok(TunnelOutcome::Exposed(TunnelExposed {
                    public_url: t.public_url.clone(),
                    note: exposure_note(),
                }));
            }
        }

        // 3. Binary must be present + runnable BEFORE we ask a human to approve.
        self.provider.preflight().await?;

        // 4. Consult the approval grant for this exposure.
        let request_id = Self::request_id(req.session_id, req.host_port, &req.preview_nonce);
        let existing = self.find_approval(&request_id)?;
        let now = Utc::now();
        match existing {
            Some(row) if row.status == ApprovalStatus::Approved => self.stand_up(&req, &row).await,
            Some(row) if row.status == ApprovalStatus::Denied => {
                // M19 A3: operator denied the exposure.
                copperclaw_metrics::inc_public_tunnel("denied", "approval_denied");
                Err(TunnelError::Denied {
                    approval_id: row.approval_id.as_uuid().to_string(),
                })
            }
            Some(row) if row.is_actionable_at(now) => Ok(TunnelOutcome::Pending {
                approval_id: row.approval_id.as_uuid().to_string(),
                note: pending_note(),
            }),
            // None, or a stale (expired/revoked) row: raise a fresh approval.
            _ => self.raise_approval(&req, &request_id),
        }
    }

    /// Spawn the tunnel for an approved request, track it, audit the exposure,
    /// and **consume the grant** so it is single-use.
    ///
    /// Three security bindings live here:
    /// * **Bind the spawned upstream to the APPROVED payload, not the retry
    ///   request.** The upstream the tunnel fronts is read from `grant.payload`
    ///   (what the operator saw and approved on the card), never from the
    ///   agent-supplied retry `req`. This closes a confused-deputy where a retry
    ///   could re-drive `stand_up` against a different internal target than the
    ///   one approved.
    /// * **Consume-first, fail-closed (M24 S1).** The grant is revoked BEFORE
    ///   the tunnel binary is spawned, serialized under the `active` lock so a
    ///   concurrent expose cannot double-spend it. If consumption fails for any
    ///   reason (row gone, no longer approved, DB error) the stand-up is
    ///   aborted — no tunnel process is ever started on a grant that could not
    ///   be spent. The deliberate trade: if the tunnel binary then fails to
    ///   open, the grant is already spent and the retry raises a FRESH approval
    ///   (the operator may be re-asked; a public tunnel can never outlive its
    ///   consumed grant's accounting).
    /// * **One-shot grant.** The revoked row can never authorize a second
    ///   public URL — a later re-expose of the same preview after teardown must
    ///   earn a fresh human tap.
    async fn stand_up(
        &self,
        req: &TunnelExposeRequest,
        grant: &pending_approvals::PendingApproval,
    ) -> Result<TunnelOutcome, TunnelError> {
        // Upstream comes from the approved grant, falling back to the request
        // only if the stored payload lacks it (older rows).
        let upstream = grant
            .payload
            .get("upstream")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(req.upstream.as_str())
            .to_string();
        {
            // Idempotency + consumption under one lock: a racing expose either
            // finds the winner's live tunnel here, or fails its own consume
            // below (the row is no longer approved) — never a second spawn on
            // one grant.
            let active = self.active.lock().await;
            if let Some(t) = active
                .iter()
                .find(|t| t.session_id == req.session_id && t.host_port == req.host_port)
            {
                return Ok(TunnelOutcome::Exposed(TunnelExposed {
                    public_url: t.public_url.clone(),
                    note: exposure_note(),
                }));
            }
            if let Err(err) = self.consume_grant(grant.approval_id) {
                self.audit(
                    req.session_id,
                    req.agent_group_id,
                    "expose",
                    req.host_port,
                    &upstream,
                    "aborted-grant-consume-failed",
                );
                copperclaw_metrics::inc_public_tunnel("aborted", "grant_consume_failed");
                return Err(err);
            }
        }
        let opened = match self.provider.open(&upstream).await {
            Ok(opened) => opened,
            Err(err) => {
                // The grant is already spent (consume-first): audit the aborted
                // stand-up so an operator can see why they will be re-asked.
                self.audit(
                    req.session_id,
                    req.agent_group_id,
                    "expose",
                    req.host_port,
                    &upstream,
                    "open-failed-grant-consumed",
                );
                copperclaw_metrics::inc_public_tunnel("aborted", "open_failed");
                return Err(err);
            }
        };
        let public_url = opened.public_url().to_string();
        {
            let mut active = self.active.lock().await;
            // Defensive re-check: consumption is one-shot, so a concurrent
            // duplicate should be impossible here — but never leak a child.
            if let Some(t) = active
                .iter()
                .find(|t| t.session_id == req.session_id && t.host_port == req.host_port)
            {
                let url = t.public_url.clone();
                drop(active);
                let mut opened = opened;
                opened.shutdown().await;
                return Ok(TunnelOutcome::Exposed(TunnelExposed {
                    public_url: url,
                    note: exposure_note(),
                }));
            }
            active.push(TrackedTunnel {
                session_id: req.session_id,
                agent_group_id: req.agent_group_id,
                host_port: req.host_port,
                public_url: public_url.clone(),
                tunnel: opened,
            });
        }
        self.audit(
            req.session_id,
            req.agent_group_id,
            "expose",
            req.host_port,
            &upstream,
            "opened",
        );
        // M19 A3: a public tunnel stood up on an approved grant.
        copperclaw_metrics::inc_public_tunnel("opened", "");
        info!(
            session = %req.session_id.as_uuid(),
            host_port = req.host_port,
            provider = self.provider.name(),
            public_url,
            "public tunnel exposed"
        );
        Ok(TunnelOutcome::Exposed(TunnelExposed {
            public_url,
            note: exposure_note(),
        }))
    }

    /// Spend an approval grant so it cannot authorize a second public exposure.
    /// Fail-closed (M24 S1): re-reads the row and refuses unless it is still
    /// `Approved`, then flips it to `Revoked`; ANY failure (row gone, already
    /// spent, DB error) is an error the caller must treat as "do not stand up".
    /// Called BEFORE the tunnel binary is spawned — a grant that cannot be
    /// spent never fronts a public URL.
    fn consume_grant(&self, approval_id: ApprovalId) -> Result<(), TunnelError> {
        let row = pending_approvals::get(&self.central, approval_id).map_err(|e| {
            TunnelError::Internal(format!(
                "could not read approval grant {} to consume it: {e}",
                approval_id.as_uuid()
            ))
        })?;
        if row.status != ApprovalStatus::Approved {
            return Err(TunnelError::Internal(format!(
                "approval grant {} is no longer approved (status: {}); a fresh operator \
                 approval is required before this preview can be made public",
                approval_id.as_uuid(),
                row.status.as_str()
            )));
        }
        pending_approvals::update_status(&self.central, approval_id, ApprovalStatus::Revoked)
            .map_err(|e| {
                warn!(?e, "tunnel: could not consume approval grant");
                TunnelError::Internal(format!(
                    "could not consume approval grant {}: {e}",
                    approval_id.as_uuid()
                ))
            })?;
        // The decision row is bookkeeping on an already-revoked grant —
        // best-effort is still safe here.
        let _ = pending_approvals::record_decision(
            &self.central,
            approval_id,
            TUNNEL_APPROVAL_ACTION,
            pending_approvals::DecisionOutcome::Revoke,
            "system:tunnel-consumed",
            Some("grant consumed by one-shot public tunnel exposure"),
        );
        Ok(())
    }

    /// Persist a `CredentialedExternalAction` pending approval for this exposure
    /// and audit the request. The stable `request_id` dedups retries via the
    /// pending-only ON CONFLICT index.
    fn raise_approval(
        &self,
        req: &TunnelExposeRequest,
        request_id: &str,
    ) -> Result<TunnelOutcome, TunnelError> {
        let (channel_type, platform_id) = match &req.notify {
            Some((ct, pid)) => (Some(ct.clone()), Some(pid.clone())),
            None => (None, None),
        };
        let payload = serde_json::json!({
            "kind": "tunnel",
            "provider": self.provider.name(),
            "session_id": req.session_id.as_uuid().to_string(),
            "host_port": req.host_port,
            "upstream": req.upstream,
            "preview_nonce": req.preview_nonce,
            "preview_name": req.preview_name,
        });
        let approval = pending_approvals::upsert(
            &self.central,
            UpsertPendingApproval {
                session_id: Some(req.session_id),
                request_id: request_id.to_string(),
                action: TUNNEL_APPROVAL_ACTION.to_string(),
                payload,
                agent_group_id: Some(req.agent_group_id),
                channel_type,
                platform_id,
                title: tunnel_card_title(req),
                options: vec![],
                ..Default::default()
            },
        )
        .map_err(|e| TunnelError::Internal(format!("persist approval: {e}")))?;
        self.audit(
            req.session_id,
            req.agent_group_id,
            "request",
            req.host_port,
            &req.upstream,
            "pending-approval",
        );
        // M19 A3: a pending-approval card was raised for the exposure.
        copperclaw_metrics::inc_public_tunnel("approval_raised", "");
        info!(
            session = %req.session_id.as_uuid(),
            host_port = req.host_port,
            approval_id = %approval.approval_id.as_uuid(),
            "public tunnel exposure awaiting approval"
        );
        Ok(TunnelOutcome::Pending {
            approval_id: approval.approval_id.as_uuid().to_string(),
            note: pending_note(),
        })
    }

    /// Find the pending/settled approval row for an exposure by its natural key.
    fn find_approval(
        &self,
        request_id: &str,
    ) -> Result<Option<pending_approvals::PendingApproval>, TunnelError> {
        let rows = pending_approvals::list(&self.central, Some(TUNNEL_APPROVAL_ACTION), None)
            .map_err(|e| TunnelError::Internal(format!("list approvals: {e}")))?;
        // Newest row wins if a stale one was re-raised (list is created_at DESC).
        Ok(rows.into_iter().find(|r| r.request_id == request_id))
    }

    /// Tear down the tunnel fronting `(session, host_port)`. Called by the host
    /// when the preview it fronts closes / is reaped. Returns whether a tunnel
    /// existed.
    pub async fn close_for_preview(&self, session_id: SessionId, host_port: u16) -> bool {
        self.teardown(session_id, host_port, "preview-closed").await
    }

    /// Tear down every tunnel a session opened (its container stopped / was
    /// removed). Returns how many were torn down.
    pub async fn close_all_for_session(&self, session_id: SessionId) -> usize {
        let ports: Vec<u16> = {
            let active = self.active.lock().await;
            active
                .iter()
                .filter(|t| t.session_id == session_id)
                .map(|t| t.host_port)
                .collect()
        };
        let mut n = 0;
        for port in ports {
            if self.teardown(session_id, port, "session-stop").await {
                n += 1;
            }
        }
        n
    }

    /// Remove + kill the tunnel for `(session, host_port)`, auditing the
    /// teardown. Returns whether an entry existed.
    async fn teardown(&self, session_id: SessionId, host_port: u16, reason: &str) -> bool {
        let removed = {
            let mut active = self.active.lock().await;
            active
                .iter()
                .position(|t| t.session_id == session_id && t.host_port == host_port)
                .map(|pos| active.remove(pos))
        };
        match removed {
            Some(mut entry) => {
                entry.tunnel.shutdown().await;
                self.audit(
                    entry.session_id,
                    entry.agent_group_id,
                    "close",
                    entry.host_port,
                    "",
                    reason,
                );
                // M19 A3: tunnel torn down; `reason` names the cause.
                copperclaw_metrics::inc_public_tunnel("torn_down", reason);
                info!(
                    session = %session_id.as_uuid(),
                    host_port,
                    reason,
                    "public tunnel torn down"
                );
                true
            }
            None => false,
        }
    }

    /// Live tunnel count (test / introspection helper).
    pub async fn active_count(&self) -> usize {
        self.active.lock().await.len()
    }

    /// Write one `audit_log` row for a tunnel mutation. Best-effort: a DB error
    /// is logged and swallowed, matching the preview audit contract.
    fn audit(
        &self,
        session_id: SessionId,
        agent_group_id: AgentGroupId,
        action: &str,
        host_port: u16,
        upstream: &str,
        reason: &str,
    ) {
        let args = serde_json::json!({
            "action": action,
            "provider": self.provider.name(),
            "session_id": session_id.as_uuid().to_string(),
            "host_port": host_port,
            "upstream": upstream,
            "reason": reason,
        })
        .to_string();
        let entry = AuditEntry {
            ts: Utc::now(),
            caller_kind: "host".to_string(),
            caller_session: Some(session_id.as_uuid().to_string()),
            caller_agent_group: Some(agent_group_id.as_uuid().to_string()),
            command: TUNNEL_AUDIT_COMMAND.to_string(),
            args,
            result: "ok".to_string(),
            error_code: None,
            error_message: None,
            latency_ms: 0,
        };
        if let Err(err) = audit_log::insert(&self.central, &entry) {
            warn!(?err, "could not write tunnel audit row");
        }
    }
}

// ---------------------------------------------------------------------------
// Agent-facing broker trait (M19 A3)
// ---------------------------------------------------------------------------

/// What the delivery loop renders back to the model for a `make_preview_public`
/// relay call (M19 A3). The host-side implementation ([`PublicTunnelBroker`])
/// maps the agent's container-port request onto a live preview's host port,
/// enforces opt-in, and drives [`TunnelBroker::expose`]; this enum carries the
/// three model-legible outcomes so the delivery branch never has to reason
/// about tunnel internals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicTunnelReply {
    /// The tunnel is live. `public_url` is the shareable link (the tokened
    /// preview URL fronted by the public tunnel); `note` is the blunt
    /// "this is PUBLIC" caveat the agent relays verbatim.
    Exposed { public_url: String, note: String },
    /// Awaiting an operator's approval tap. `note` tells the agent to try again
    /// once approved — NOT an error (the request was accepted and is pending).
    Pending { note: String },
    /// A rendered, model-facing failure (not opted in, no live preview, binary
    /// absent, approver declined, …). The delivery branch surfaces it as an
    /// `is_error` tool result.
    Error(String),
}

/// Host-side broker the delivery loop calls to service a `make_preview_public`
/// (`__preview` relay) request (M19 A3). Implemented by the host, which owns the
/// mapping from the agent's *container* port to the live preview's *host* port
/// and the per-group opt-in — this trait keeps the delivery crate free of that
/// wiring, exactly as [`crate::preview::PreviewBroker`] does for `expose_preview`.
///
/// Every outcome (including every failure) is a [`PublicTunnelReply`]: the
/// runner's blocking poll is never left unanswered. The public exposure it
/// drives stays approval-gated end to end via [`TunnelBroker::expose`] — this
/// trait adds a verb, never a bypass.
#[async_trait]
pub trait PublicTunnelBroker: Send + Sync {
    /// Make the live preview on `container_port` public. Resolves the preview's
    /// host port, enforces opt-in, and raises / consults the
    /// `CredentialedExternalAction` approval via [`TunnelBroker::expose`].
    async fn make_public(
        &self,
        session: crate::preview::SessionInfoLite,
        container_port: u16,
    ) -> PublicTunnelReply;
}

// ---------------------------------------------------------------------------
// Module
// ---------------------------------------------------------------------------

/// The registrable module. Like [`crate::agent_to_agent::AgentToAgentModule`] it
/// registers no router/delivery hooks — public exposure is driven host-side
/// through the [`TunnelBroker`], which is approval-gated end to end. Registering
/// the module surfaces it in `cclaw modules list` and gives the host a single
/// place to hold the broker.
pub struct TunnelModule {
    broker: Arc<TunnelBroker>,
}

impl TunnelModule {
    /// Wrap a broker as a module.
    #[must_use]
    pub fn new(broker: Arc<TunnelBroker>) -> Self {
        Self { broker }
    }

    /// The broker this module carries (for host wiring: preview-teardown hook,
    /// the expose relay).
    #[must_use]
    pub fn broker(&self) -> Arc<TunnelBroker> {
        Arc::clone(&self.broker)
    }
}

#[async_trait]
impl Module for TunnelModule {
    fn name(&self) -> &'static str {
        "tunnel"
    }

    async fn install(&self, _ctx: Arc<dyn ModuleContext>) -> Result<(), ModuleError> {
        // OFF by default: no hooks. Every exposure flows through the broker's
        // approval gate; there is nothing to register on the router.
        Ok(())
    }
}

/// True when `path` names an existing file (a cheap best-effort presence check
/// the host can use before wiring the provider). Not a substitute for
/// [`TunnelProvider::preflight`], which actually runs the binary.
#[must_use]
pub fn binary_present(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::pending_approvals::{
        DecisionOutcome, record_decision, update_status,
    };

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    /// Seed a real agent group + session so approval-row FKs
    /// (`pending_approvals.agent_group_id → agent_groups`,
    /// `.session_id → sessions`) are satisfiable.
    fn seed_group_and_session(central: &CentralDb) -> (AgentGroupId, SessionId) {
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
        let ag = create_ag(
            central,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id;
        let session = create_session(
            central,
            CreateSession {
                agent_group_id: ag,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
        .id;
        (ag, session)
    }

    fn tunnel_audit_rows(central: &CentralDb) -> Vec<(String, String)> {
        // (action, reason) pairs from every `tunnel` audit row.
        audit_log::list_recent(central, Utc::now() - chrono::Duration::hours(1), 100)
            .unwrap()
            .into_iter()
            .filter(|e| e.command == TUNNEL_AUDIT_COMMAND)
            .map(|e| {
                let v: serde_json::Value = serde_json::from_str(&e.args).unwrap();
                (
                    v["action"].as_str().unwrap_or_default().to_string(),
                    v["reason"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// Write an executable mock "tunnel binary" that prints `line` then sleeps
    /// (so the child stays alive as a real tunnel process would). Returns its
    /// path; the tempdir is kept alive by the returned guard.
    fn mock_binary(line: &str, sleep_secs: u32) -> (tempfile::TempDir, PathBuf) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mock-cloudflared");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        // `--version` (the preflight probe) must return instantly; only the
        // `tunnel` path echoes a URL and lingers like a real quick tunnel.
        writeln!(f, "if [ \"$1\" = \"--version\" ]; then").unwrap();
        writeln!(f, "  echo 'cloudflared version 0.0.0-mock'; exit 0").unwrap();
        writeln!(f, "fi").unwrap();
        writeln!(f, "echo '{line}'").unwrap();
        writeln!(f, "sleep {sleep_secs}").unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        (dir, path)
    }

    #[test]
    fn extract_url_from_cloudflared_banner() {
        let line = "2024-01-01 INF |  https://calm-frost-1234.trycloudflare.com  |";
        assert_eq!(
            extract_tunnel_url(line).as_deref(),
            Some("https://calm-frost-1234.trycloudflare.com")
        );
    }

    #[test]
    fn extract_url_plain_and_trailing_punct() {
        assert_eq!(
            extract_tunnel_url("url=https://x.trycloudflare.com.").as_deref(),
            Some("https://x.trycloudflare.com")
        );
        assert_eq!(extract_tunnel_url("no url here"), None);
        assert_eq!(extract_tunnel_url("https://"), None);
    }

    #[tokio::test]
    async fn absent_binary_is_clean_actionable_error_not_a_panic() {
        let provider = CloudflaredProvider::with_binary("/nonexistent/definitely-not-cloudflared");
        let err = provider.preflight().await.unwrap_err();
        match &err {
            TunnelError::BinaryNotFound { binary, .. } => {
                assert!(binary.contains("definitely-not-cloudflared"));
            }
            other => panic!("expected BinaryNotFound, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("was not found"));
        assert!(msg.contains("cloudflared"), "names the install steps");
        assert!(msg.contains("never bundles"), "states the no-bundle policy");
    }

    #[tokio::test]
    async fn provider_opens_and_parses_mock_binary_url() {
        let (_guard, path) = mock_binary("INF |  https://mock-tunnel.trycloudflare.com  |", 30);
        let provider =
            CloudflaredProvider::with_binary(path).with_open_timeout(Duration::from_secs(30));
        let mut opened = provider.open("http://127.0.0.1:8100").await.unwrap();
        assert_eq!(opened.public_url(), "https://mock-tunnel.trycloudflare.com");
        opened.shutdown().await; // kills the sleeping child
    }

    #[tokio::test]
    async fn provider_reports_no_url_when_binary_prints_nothing_useful() {
        let (_guard, path) = mock_binary("starting up, nothing to see", 0);
        // Generous timeout: the mock exits immediately, so `open` returns `NoUrl`
        // as soon as both streams hit EOF — a large ceiling just guarantees the
        // EOF path wins over the timeout path even under heavy parallel load.
        let provider =
            CloudflaredProvider::with_binary(path).with_open_timeout(Duration::from_secs(30));
        let err = provider.open("http://127.0.0.1:8100").await.unwrap_err();
        assert!(matches!(err, TunnelError::NoUrl { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn not_enabled_group_is_refused_and_audited() {
        let central = db();
        let provider = Arc::new(CloudflaredProvider::with_binary("/nonexistent/cf"));
        let broker = TunnelBroker::new(central.clone(), provider);
        let err = broker
            .expose(TunnelExposeRequest {
                session_id: SessionId::new(),
                agent_group_id: AgentGroupId::new(),
                host_port: 8100,
                upstream: "http://127.0.0.1:8100".into(),
                preview_nonce: "nonce-a".into(),
                preview_name: None,
                enabled: false,
                notify: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, TunnelError::NotEnabled { .. }));
        // No approval was raised, and the refusal was audited.
        assert!(
            pending_approvals::list(&central, Some(TUNNEL_APPROVAL_ACTION), None)
                .unwrap()
                .is_empty()
        );
        let rows = tunnel_audit_rows(&central);
        assert_eq!(rows, vec![("request".into(), "blocked-not-enabled".into())]);
    }

    #[tokio::test]
    async fn enabled_but_absent_binary_errors_before_raising_approval() {
        let central = db();
        let provider = Arc::new(CloudflaredProvider::with_binary("/nonexistent/cf"));
        let broker = TunnelBroker::new(central.clone(), provider);
        let err = broker
            .expose(TunnelExposeRequest {
                session_id: SessionId::new(),
                agent_group_id: AgentGroupId::new(),
                host_port: 8100,
                upstream: "http://127.0.0.1:8100".into(),
                preview_nonce: "nonce-b".into(),
                preview_name: None,
                enabled: true,
                notify: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, TunnelError::BinaryNotFound { .. }));
        // Fail-closed: no human is asked to approve something that cannot run.
        assert!(
            pending_approvals::list(&central, Some(TUNNEL_APPROVAL_ACTION), None)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn full_flow_expose_approve_surface_url_teardown() {
        let central = db();
        let (_guard, path) = mock_binary("INF |  https://cofounder-demo.trycloudflare.com  |", 30);
        let provider = Arc::new(
            CloudflaredProvider::with_binary(path).with_open_timeout(Duration::from_secs(30)),
        );
        let broker = TunnelBroker::new(central.clone(), provider);

        let (agent_group_id, session_id) = seed_group_and_session(&central);
        let req = TunnelExposeRequest {
            session_id,
            agent_group_id,
            host_port: 8100,
            upstream: "http://127.0.0.1:8100".into(),
            preview_nonce: "nonce-flow".into(),
            preview_name: Some("cofounder-demo".into()),
            enabled: true,
            notify: Some((ChannelType::new("telegram"), "chat-1".into())),
        };

        // 1. First expose ⇒ pending approval (no tunnel yet).
        let approval_id = match broker.expose(req.clone()).await.unwrap() {
            TunnelOutcome::Pending { approval_id, .. } => approval_id,
            TunnelOutcome::Exposed(e) => panic!("expected Pending, got Exposed {e:?}"),
        };
        assert_eq!(broker.active_count().await, 0, "no tunnel before approval");
        let pending =
            pending_approvals::list(&central, Some(TUNNEL_APPROVAL_ACTION), None).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payload["kind"], "tunnel");
        assert_eq!(pending[0].payload["host_port"], 8100);
        // S1 (a): the approval identity carries the per-preview nonce, not just
        // the pool-reusable (session, host_port).
        assert!(
            pending[0].request_id.ends_with(":nonce-flow"),
            "request id includes the preview nonce: {}",
            pending[0].request_id
        );
        // S1 (b): the card names the app, the host port, and the session.
        assert!(pending[0].title.contains("cofounder-demo"), "names the app");
        assert!(pending[0].title.contains("8100"), "names the host port");
        assert!(
            pending[0].title.contains(&session_id.as_uuid().to_string()),
            "names the session"
        );

        // 2. A repeat expose while pending stays pending (no duplicate rows).
        assert!(matches!(
            broker.expose(req.clone()).await.unwrap(),
            TunnelOutcome::Pending { .. }
        ));
        assert_eq!(
            pending_approvals::list(&central, Some(TUNNEL_APPROVAL_ACTION), None)
                .unwrap()
                .len(),
            1,
            "retry must not stack a second pending row"
        );

        // 3. Operator approves (mirrors the host `resolve_approve` apply arm:
        //    flip status + record the decision).
        let approval_uuid =
            copperclaw_types::ApprovalId(uuid::Uuid::parse_str(&approval_id).unwrap());
        update_status(&central, approval_uuid, ApprovalStatus::Approved).unwrap();
        record_decision(
            &central,
            approval_uuid,
            TUNNEL_APPROVAL_ACTION,
            DecisionOutcome::Approve,
            "operator",
            None,
        )
        .unwrap();

        // 4. Re-expose ⇒ the public URL is surfaced (relayed into the ritual card).
        let exposed = match broker.expose(req.clone()).await.unwrap() {
            TunnelOutcome::Exposed(e) => e,
            TunnelOutcome::Pending { approval_id, .. } => {
                panic!("expected Exposed, got Pending {approval_id}")
            }
        };
        assert_eq!(
            exposed.public_url,
            "https://cofounder-demo.trycloudflare.com"
        );
        assert!(exposed.note.contains("PUBLIC"));
        assert_eq!(broker.active_count().await, 1);

        // 5. A further expose is idempotent (same URL, still one tunnel).
        assert!(matches!(
            broker.expose(req.clone()).await.unwrap(),
            TunnelOutcome::Exposed(e) if e.public_url == exposed.public_url
        ));
        assert_eq!(broker.active_count().await, 1);

        // The grant was consumed on stand-up (single-use): the approved row is
        // now revoked, so it can never authorize a second public URL.
        let after_grant = pending_approvals::get(&central, approval_uuid).unwrap();
        assert_eq!(after_grant.status, ApprovalStatus::Revoked);

        // 6. Preview closes ⇒ the tunnel is torn down (no orphaned public URL).
        assert!(broker.close_for_preview(session_id, 8100).await);
        assert_eq!(broker.active_count().await, 0);
        // Closing again is a no-op.
        assert!(!broker.close_for_preview(session_id, 8100).await);

        // 7. One-shot: re-exposing after teardown needs a FRESH approval — the
        //    consumed grant does not silently re-open a public tunnel.
        assert!(matches!(
            broker.expose(req.clone()).await.unwrap(),
            TunnelOutcome::Pending { .. }
        ));
        assert_eq!(broker.active_count().await, 0);

        // Audit completeness: request → expose → close all present.
        let actions: Vec<String> = tunnel_audit_rows(&central)
            .into_iter()
            .map(|(a, _)| a)
            .collect();
        assert!(actions.contains(&"request".to_string()));
        assert!(actions.contains(&"expose".to_string()));
        assert!(actions.contains(&"close".to_string()));
    }

    #[tokio::test]
    async fn denied_approval_refuses_exposure() {
        let central = db();
        let (_guard, path) = mock_binary("INF https://x.trycloudflare.com", 30);
        let provider = Arc::new(CloudflaredProvider::with_binary(path));
        let broker = TunnelBroker::new(central.clone(), provider);
        let (agent_group_id, session_id) = seed_group_and_session(&central);
        let req = TunnelExposeRequest {
            session_id,
            agent_group_id,
            host_port: 8100,
            upstream: "http://127.0.0.1:8100".into(),
            preview_nonce: "nonce-denied".into(),
            preview_name: None,
            enabled: true,
            notify: None,
        };
        let approval_id = match broker.expose(req.clone()).await.unwrap() {
            TunnelOutcome::Pending { approval_id, .. } => approval_id,
            TunnelOutcome::Exposed(e) => panic!("expected Pending, got Exposed {e:?}"),
        };
        let approval_uuid =
            copperclaw_types::ApprovalId(uuid::Uuid::parse_str(&approval_id).unwrap());
        update_status(&central, approval_uuid, ApprovalStatus::Denied).unwrap();
        let err = broker.expose(req).await.unwrap_err();
        assert!(matches!(err, TunnelError::Denied { .. }));
        assert_eq!(broker.active_count().await, 0);
    }

    #[tokio::test]
    async fn close_all_for_session_tears_down_every_tunnel() {
        let central = db();
        let (_guard, path) = mock_binary("INF https://a.trycloudflare.com", 30);
        let provider = Arc::new(
            CloudflaredProvider::with_binary(path).with_open_timeout(Duration::from_secs(30)),
        );
        let broker = TunnelBroker::new(central.clone(), provider);
        let (agent_group_id, session_id) = seed_group_and_session(&central);

        for port in [8100u16, 8101] {
            let req = TunnelExposeRequest {
                session_id,
                agent_group_id,
                host_port: port,
                upstream: format!("http://127.0.0.1:{port}"),
                preview_nonce: format!("nonce-{port}"),
                preview_name: None,
                enabled: true,
                notify: None,
            };
            let approval_id = match broker.expose(req.clone()).await.unwrap() {
                TunnelOutcome::Pending { approval_id, .. } => approval_id,
                TunnelOutcome::Exposed(e) => panic!("expected Pending, got Exposed {e:?}"),
            };
            let approval_uuid =
                copperclaw_types::ApprovalId(uuid::Uuid::parse_str(&approval_id).unwrap());
            update_status(&central, approval_uuid, ApprovalStatus::Approved).unwrap();
            assert!(matches!(
                broker.expose(req).await.unwrap(),
                TunnelOutcome::Exposed(_)
            ));
        }
        assert_eq!(broker.active_count().await, 2);
        assert_eq!(broker.close_all_for_session(session_id).await, 2);
        assert_eq!(broker.active_count().await, 0);
    }

    #[tokio::test]
    async fn empty_preview_nonce_is_refused_fail_closed() {
        // Without a nonce the approval identity would degrade back to the
        // pool-reusable (session, host_port) key — refuse before any gate.
        let central = db();
        let provider = Arc::new(CloudflaredProvider::with_binary("/nonexistent/cf"));
        let broker = TunnelBroker::new(central, provider);
        let err = broker
            .expose(TunnelExposeRequest {
                session_id: SessionId::new(),
                agent_group_id: AgentGroupId::new(),
                host_port: 8100,
                upstream: "http://127.0.0.1:8100".into(),
                preview_nonce: "   ".into(),
                preview_name: None,
                enabled: true,
                notify: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, TunnelError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn stale_approved_grant_on_reused_port_needs_fresh_approval() {
        // M24 S1 (a): host ports are pool-reused. An old, still-Approved row for
        // (session, host_port) minted under a PREVIOUS preview instance must not
        // satisfy a NEW preview's exposure request — the nonce in the request
        // identity forces a fresh approval (and a fresh human tap).
        let central = db();
        let (_guard, path) = mock_binary("INF https://stale.trycloudflare.com", 30);
        let provider = Arc::new(
            CloudflaredProvider::with_binary(path).with_open_timeout(Duration::from_secs(30)),
        );
        let broker = TunnelBroker::new(central.clone(), provider);
        let (agent_group_id, session_id) = seed_group_and_session(&central);

        // Old preview instance requests exposure; the operator approves; the
        // grant is never consumed (simulating the stale-approved-row hazard).
        let old_req = TunnelExposeRequest {
            session_id,
            agent_group_id,
            host_port: 8100,
            upstream: "http://127.0.0.1:8100".into(),
            preview_nonce: "old-preview".into(),
            preview_name: Some("old-app".into()),
            enabled: true,
            notify: None,
        };
        let approval_id = match broker.expose(old_req).await.unwrap() {
            TunnelOutcome::Pending { approval_id, .. } => approval_id,
            TunnelOutcome::Exposed(e) => panic!("expected Pending, got Exposed {e:?}"),
        };
        let approval_uuid =
            copperclaw_types::ApprovalId(uuid::Uuid::parse_str(&approval_id).unwrap());
        update_status(&central, approval_uuid, ApprovalStatus::Approved).unwrap();

        // A DIFFERENT app later reacquires host port 8100 in the same session
        // (fresh preview instance ⇒ fresh nonce). The stale grant must NOT
        // stand up a tunnel: the request goes Pending on a NEW approval row.
        let new_req = TunnelExposeRequest {
            session_id,
            agent_group_id,
            host_port: 8100,
            upstream: "http://127.0.0.1:8100".into(),
            preview_nonce: "new-preview".into(),
            preview_name: Some("new-app".into()),
            enabled: true,
            notify: None,
        };
        match broker.expose(new_req).await.unwrap() {
            TunnelOutcome::Pending {
                approval_id: id, ..
            } => {
                assert_ne!(id, approval_id, "a fresh approval row was raised");
            }
            TunnelOutcome::Exposed(e) => {
                panic!("stale grant re-stood-up a public tunnel: {e:?}")
            }
        }
        assert_eq!(broker.active_count().await, 0, "no tunnel without a tap");
        // The stale grant is untouched and the new row is pending.
        let rows = pending_approvals::list(&central, Some(TUNNEL_APPROVAL_ACTION), None).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|r| r.request_id.ends_with(":new-preview")
                    && r.status == ApprovalStatus::Pending)
        );
    }

    #[tokio::test]
    async fn consume_failure_aborts_before_stand_up() {
        // M24 S1 (a): grant consumption is fail-closed and happens BEFORE the
        // tunnel binary is spawned. A grant that cannot be spent (here: revoked
        // in the DB under a stale in-memory Approved snapshot) must abort the
        // stand-up with a grant error — NOT reach the provider (the absent
        // binary would surface as BinaryNotFound/Spawn if it were attempted).
        let central = db();
        let provider = Arc::new(CloudflaredProvider::with_binary("/nonexistent/cf"));
        let broker = TunnelBroker::new(central.clone(), provider);
        let (agent_group_id, session_id) = seed_group_and_session(&central);
        let req = TunnelExposeRequest {
            session_id,
            agent_group_id,
            host_port: 8100,
            upstream: "http://127.0.0.1:8100".into(),
            preview_nonce: "nonce-consume".into(),
            preview_name: None,
            enabled: true,
            notify: None,
        };
        // Seed the approval row directly (the absent binary blocks `expose`'s
        // preflight path, which is exactly why stand_up must not reach it).
        let grant = pending_approvals::upsert(
            &central,
            UpsertPendingApproval {
                session_id: Some(session_id),
                request_id: TunnelBroker::request_id(session_id, 8100, "nonce-consume"),
                action: TUNNEL_APPROVAL_ACTION.to_string(),
                payload: serde_json::json!({"upstream": "http://127.0.0.1:8100"}),
                agent_group_id: Some(agent_group_id),
                title: "t".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap();
        update_status(&central, grant.approval_id, ApprovalStatus::Approved).unwrap();
        // Stale snapshot says Approved; the DB row is then revoked (as a racing
        // consumer would have done).
        let mut stale = pending_approvals::get(&central, grant.approval_id).unwrap();
        assert_eq!(stale.status, ApprovalStatus::Approved);
        update_status(&central, grant.approval_id, ApprovalStatus::Revoked).unwrap();
        stale.status = ApprovalStatus::Approved; // keep the snapshot stale

        let err = broker.stand_up(&req, &stale).await.unwrap_err();
        match &err {
            TunnelError::Internal(msg) => {
                assert!(msg.contains("no longer approved"), "got {msg}");
            }
            other => panic!(
                "expected the grant-consume Internal error (binary must never \
                 be attempted), got {other:?}"
            ),
        }
        assert_eq!(broker.active_count().await, 0, "stand-up aborted");
    }

    #[tokio::test]
    async fn open_failure_after_consume_spends_grant_fail_closed() {
        // M24 S1 (a): consume-first ordering on the live path. When the tunnel
        // binary fails to advertise a URL, the grant was ALREADY spent — the
        // retry must raise a fresh approval rather than reuse the grant.
        let central = db();
        // Mock binary passes preflight but exits without printing a URL.
        let (_guard, path) = mock_binary("starting up, no url here", 0);
        let provider = Arc::new(
            CloudflaredProvider::with_binary(path).with_open_timeout(Duration::from_secs(30)),
        );
        let broker = TunnelBroker::new(central.clone(), provider);
        let (agent_group_id, session_id) = seed_group_and_session(&central);
        let req = TunnelExposeRequest {
            session_id,
            agent_group_id,
            host_port: 8100,
            upstream: "http://127.0.0.1:8100".into(),
            preview_nonce: "nonce-openfail".into(),
            preview_name: None,
            enabled: true,
            notify: None,
        };
        let approval_id = match broker.expose(req.clone()).await.unwrap() {
            TunnelOutcome::Pending { approval_id, .. } => approval_id,
            TunnelOutcome::Exposed(e) => panic!("expected Pending, got Exposed {e:?}"),
        };
        let approval_uuid =
            copperclaw_types::ApprovalId(uuid::Uuid::parse_str(&approval_id).unwrap());
        update_status(&central, approval_uuid, ApprovalStatus::Approved).unwrap();

        let err = broker.expose(req.clone()).await.unwrap_err();
        assert!(matches!(err, TunnelError::NoUrl { .. }), "got {err:?}");
        assert_eq!(broker.active_count().await, 0);
        // The grant was consumed BEFORE the failed stand-up.
        let after = pending_approvals::get(&central, approval_uuid).unwrap();
        assert_eq!(after.status, ApprovalStatus::Revoked);
        // The retry cannot reuse it: a fresh approval is raised.
        assert!(matches!(
            broker.expose(req).await.unwrap(),
            TunnelOutcome::Pending { .. }
        ));
    }

    #[tokio::test]
    async fn module_installs_without_registering_hooks() {
        use crate::context::MockModuleContext;
        let central = db();
        let provider = Arc::new(CloudflaredProvider::with_binary("/nonexistent/cf"));
        let broker = TunnelBroker::new(central, provider);
        let module = TunnelModule::new(Arc::clone(&broker));
        assert_eq!(module.name(), "tunnel");
        assert!(Arc::ptr_eq(&module.broker(), &broker));
        let ctx = MockModuleContext::new();
        module
            .install(Arc::clone(&ctx) as Arc<dyn ModuleContext>)
            .await
            .unwrap();
        assert!(
            ctx.registered().is_empty(),
            "tunnel module registers no router/delivery hooks (OFF by default)"
        );
    }
}
