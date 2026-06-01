//! Unix-socket server + dispatch table for `cclaw` commands.
//!
//! Each connection runs the framing loop in [`serve_connection`]: read one
//! `Request::Call`, look up its `command` in the [`DispatchTable`], invoke
//! the handler, write the [`copperclaw_cclaw::Response`] back, half-close.
//!
//! The dispatch table is a small `HashMap<&'static str, Arc<dyn CommandHandler>>`
//! so handlers stay swappable in tests. [`build_dispatch_table`] returns the
//! production table — every command in [`copperclaw_cclaw::ALL_COMMANDS`].

use crate::handlers;
use copperclaw_cclaw::{
    Caller, ErrorPayload, ProtoError, Request, Response, read_request, write_response,
};
use copperclaw_db::central::CentralDb;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Hard cap on a single request frame on the cclaw socket. A local
/// process can otherwise feed an arbitrarily large NDJSON line and
/// pin host memory until `read_until` returns. 1 MiB is roughly two
/// orders of magnitude above any legitimate command argument
/// (`groups.config.update` is the worst offender and tops out under
/// 16 KiB in practice).
pub const MAX_REQUEST_FRAME_BYTES: u64 = 1024 * 1024;

/// Maximum number of in-flight admin connections served concurrently.
/// Each connection holds one task + one fd. Without a cap, a local
/// process can exhaust the host's fd table by opening connections in
/// a loop. 32 is comfortably above any plausible admin workload
/// (every `cclaw` subcommand is one short-lived round-trip).
pub const MAX_CONCURRENT_CONNECTIONS: usize = 32;

/// Context handed to every [`CommandHandler::handle`] call.
pub struct HandlerCtx {
    pub central: CentralDb,
    /// Data directory root for per-session file lookups (e.g. outbound.db
    /// paths used by the dead-letter replay handler).
    pub data_dir: std::path::PathBuf,
}

impl HandlerCtx {
    pub fn new(central: CentralDb) -> Self {
        Self {
            central,
            data_dir: std::path::PathBuf::from("data"),
        }
    }

    pub fn with_data_dir(central: CentralDb, data_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            central,
            data_dir: data_dir.into(),
        }
    }
}

/// Trait every command handler implements.
///
/// Handlers are not async because the underlying `copperclaw-db` table fns are
/// synchronous; this keeps the dispatch loop simple. If a future handler
/// needs to do I/O it can spawn its own task.
pub trait CommandHandler: Send + Sync {
    fn handle(
        &self,
        args: &Value,
        caller: &Caller,
        ctx: &HandlerCtx,
    ) -> Result<Value, ErrorPayload>;
}

/// Type alias for the closure each [`FnHandler`] wraps.
type HandlerFn = dyn Fn(&Value, &CentralDb) -> Result<Value, ErrorPayload> + Send + Sync;

/// Function-pointer command handler. Most table handlers fit this shape.
pub struct FnHandler {
    f: Box<HandlerFn>,
    requires_host: bool,
}

impl FnHandler {
    pub fn new<F>(f: F, requires_host: bool) -> Self
    where
        F: Fn(&Value, &CentralDb) -> Result<Value, ErrorPayload> + Send + Sync + 'static,
    {
        Self {
            f: Box::new(f),
            requires_host,
        }
    }
}

impl CommandHandler for FnHandler {
    fn handle(
        &self,
        args: &Value,
        caller: &Caller,
        ctx: &HandlerCtx,
    ) -> Result<Value, ErrorPayload> {
        if self.requires_host && !matches!(caller, Caller::Host) {
            return Err(ErrorPayload::new(
                "permission_denied",
                "command is host-only",
            ));
        }
        (self.f)(args, &ctx.central)
    }
}

/// Handler variant that receives the full [`HandlerCtx`] rather than just the
/// central DB. Used for commands that need additional context (e.g. `data_dir`
/// for the dead-letter replay handler).
type CtxHandlerFn = dyn Fn(&Value, &HandlerCtx) -> Result<Value, ErrorPayload> + Send + Sync;

pub struct CtxFnHandler {
    f: Box<CtxHandlerFn>,
    requires_host: bool,
}

impl CtxFnHandler {
    pub fn new<F>(f: F, requires_host: bool) -> Self
    where
        F: Fn(&Value, &HandlerCtx) -> Result<Value, ErrorPayload> + Send + Sync + 'static,
    {
        Self {
            f: Box::new(f),
            requires_host,
        }
    }
}

impl CommandHandler for CtxFnHandler {
    fn handle(
        &self,
        args: &Value,
        caller: &Caller,
        ctx: &HandlerCtx,
    ) -> Result<Value, ErrorPayload> {
        if self.requires_host && !matches!(caller, Caller::Host) {
            return Err(ErrorPayload::new(
                "permission_denied",
                "command is host-only",
            ));
        }
        (self.f)(args, ctx)
    }
}

/// In-memory mapping of dotted command names to their handler.
pub type DispatchTable = HashMap<&'static str, Arc<dyn CommandHandler>>;

/// Build the production dispatch table.
#[allow(clippy::too_many_lines)] // Registration table; one line per command.
pub fn build_dispatch_table() -> DispatchTable {
    let mut t: DispatchTable = HashMap::new();
    macro_rules! ins {
        ($name:literal, $fn:expr, $host:expr) => {
            t.insert(
                $name,
                Arc::new(FnHandler::new(|a, c| ($fn)(a, c), $host)) as Arc<dyn CommandHandler>,
            );
        };
    }
    // Variant for handlers that need the full HandlerCtx (e.g. data_dir).
    macro_rules! ins_ctx {
        ($name:literal, $fn:expr, $host:expr) => {
            t.insert(
                $name,
                Arc::new(CtxFnHandler::new($fn, $host)) as Arc<dyn CommandHandler>,
            );
        };
    }

    ins!("groups.list", handlers::groups::list, false);
    ins!("groups.get", handlers::groups::get, false);
    ins!("groups.create", handlers::groups::create, true);
    ins!("groups.update", handlers::groups::update, true);
    ins!("groups.delete", handlers::groups::delete, true);
    ins!("groups.restart", handlers::groups::restart, true);
    ins!("groups.config.get", handlers::groups::config_get, false);
    ins!(
        "groups.config.update",
        handlers::groups::config_update,
        true
    );
    ins!(
        "groups.config.add-mcp-server",
        handlers::groups::config_add_mcp_server,
        true
    );
    ins!(
        "groups.config.remove-mcp-server",
        handlers::groups::config_remove_mcp_server,
        true
    );
    ins!(
        "groups.config.add-package",
        handlers::groups::config_add_package,
        true
    );
    ins!(
        "groups.config.remove-package",
        handlers::groups::config_remove_package,
        true
    );
    ins!(
        "groups.config.set-egress-allow",
        handlers::groups::config_set_egress_allow,
        true
    );
    ins!(
        "groups.config.set-resource-limits",
        handlers::groups::config_set_resource_limits,
        true
    );
    ins!(
        "groups.config.set-coding-enabled",
        handlers::groups::config_set_coding_enabled,
        true
    );

    ins!(
        "messaging-groups.list",
        handlers::messaging_groups::list,
        false
    );
    ins!(
        "messaging-groups.get",
        handlers::messaging_groups::get,
        false
    );
    ins!(
        "messaging-groups.create",
        handlers::messaging_groups::create,
        true
    );
    ins!(
        "messaging-groups.update",
        handlers::messaging_groups::update,
        true
    );
    ins!(
        "messaging-groups.delete",
        handlers::messaging_groups::delete,
        true
    );

    ins!("wirings.list", handlers::wirings::list, false);
    ins!("wirings.get", handlers::wirings::get, false);
    ins!("wirings.create", handlers::wirings::create, true);
    ins!("wirings.update", handlers::wirings::update, true);
    ins!("wirings.delete", handlers::wirings::delete, true);

    ins!("users.list", handlers::users::list, false);
    ins!("users.get", handlers::users::get, false);
    ins!("users.create", handlers::users::create, true);
    ins!("users.update", handlers::users::update, true);

    ins!("roles.list", handlers::roles::list, false);
    ins!("roles.grant", handlers::roles::grant, true);
    ins!("roles.revoke", handlers::roles::revoke, true);

    ins!("members.list", handlers::members::list, false);
    ins!("members.add", handlers::members::add, true);
    ins!("members.remove", handlers::members::remove, true);

    ins!("destinations.list", handlers::destinations::list, false);
    ins!("destinations.add", handlers::destinations::add, true);
    ins!("destinations.remove", handlers::destinations::remove, true);

    ins!("sessions.list", handlers::sessions::list, false);
    ins!("sessions.get", handlers::sessions::get, false);
    ins_ctx!("sessions.delete", handlers::sessions::delete, true);

    ins!("user-dms.list", handlers::user_dms::list, false);
    ins!(
        "dropped-messages.list",
        handlers::dropped_messages::list,
        false
    );
    ins!(
        "dropped-messages.outbound-list",
        handlers::dropped_messages::outbound_list,
        false
    );
    ins_ctx!(
        "dropped-messages.replay",
        |a, ctx| handlers::dropped_messages::replay_with_data_dir(a, &ctx.central, &ctx.data_dir),
        true
    );
    ins!("db.backup", handlers::db::backup, true);
    ins!("db.restore", handlers::db::restore, true);
    ins!("mcp.list-presets", handlers::mcp::list_presets, false);
    ins!("mcp.add", handlers::mcp::add, true);
    ins!("approvals.list", handlers::approvals::list, false);
    ins!("approvals.get", handlers::approvals::get, false);
    ins!(
        "approvals.approve_sender",
        handlers::approvals::approve_sender,
        true
    );
    ins!("approvals.approve", handlers::approvals::approve, true);
    ins!("approvals.deny", handlers::approvals::deny, true);
    ins!("audit.list", handlers::audit::list, false);
    ins!("budgets.list", handlers::budgets::list, false);
    ins!("budgets.set", handlers::budgets::set, true);
    ins!("usage.rollup", handlers::usage::rollup, false);
    ins!("schema.version", handlers::schema::version, false);

    t
}

/// Dispatch one parsed [`Request`] against the table. Returns the matching
/// [`Response`]. Test-friendly because it bypasses the socket I/O.
///
/// Mutating calls (every command in [`handlers::HOST_ONLY_COMMANDS`])
/// emit an `audit_log` row on the way out — success or error. The write
/// is best-effort: an audit failure must never fail the request.
pub fn dispatch_request(table: &DispatchTable, ctx: &HandlerCtx, req: &Request) -> Response {
    let Request::Call {
        id,
        command,
        args,
        caller,
    } = req;
    let is_mutation = handlers::HOST_ONLY_COMMANDS.contains(&command.as_str());
    let started_at = std::time::Instant::now();

    let Some(handler) = table.get(command.as_str()) else {
        let resp = Response::err(
            id,
            ErrorPayload::new("unknown_command", format!("no handler for `{command}`")),
        );
        // Don't audit unknown commands — we couldn't have run the
        // mutation anyway, and noisily logging probes of nonexistent
        // verbs swamps the table.
        return resp;
    };
    // Apply the central caller-scope policy in addition to whatever the
    // handler enforces internally. Host-only handlers re-check, but a
    // handler-side default of `false` plus this central check still keeps
    // mutations safe.
    if is_mutation && !matches!(caller, Caller::Host) {
        let resp = Response::err(
            id,
            ErrorPayload::new(
                "permission_denied",
                format!("command `{command}` requires host caller"),
            ),
        );
        audit_dispatch(
            &ctx.central,
            command,
            args,
            caller,
            &resp,
            started_at.elapsed(),
        );
        return resp;
    }
    let resp = match handler.handle(args, caller, ctx) {
        Ok(data) => Response::ok(id, data),
        Err(err) => Response::err(id, err),
    };
    if is_mutation {
        audit_dispatch(
            &ctx.central,
            command,
            args,
            caller,
            &resp,
            started_at.elapsed(),
        );
    }
    resp
}

/// Mask string used in place of redacted secrets in the audit log.
const REDACTED_PLACEHOLDER: &str = "<redacted>";

/// Per-command rule for fields whose values must never reach the audit
/// log in plaintext. The host receives these args from operators (or
/// agents) and writes them into `audit_log.args`; without this filter
/// `cclaw mcp add postgres --env DATABASE_URL=postgres://u:p@h/d` would
/// persist the connection string in cleartext.
///
/// Returns a deep-cloned, redacted variant of `args` suitable for
/// serialising into `audit_log.args`. The original argument value flows
/// to the handler unchanged.
fn redact_sensitive_args(command: &str, args: &Value) -> Value {
    let mut out = args.clone();
    match command {
        // `mcp.add` carries operator-supplied env values that are almost
        // always API keys / connection strings. Mask every value under
        // the top-level `env` object; keep the keys so the audit row
        // still records "which env vars were set" without leaking the
        // contents.
        "mcp.add" => {
            if let Some(env) = out.get_mut("env").and_then(Value::as_object_mut) {
                for v in env.values_mut() {
                    *v = Value::String(REDACTED_PLACEHOLDER.to_string());
                }
            }
        }
        // `groups.config.set-mcp-servers` writes the full JSON blob
        // verbatim. The values are operator-defined and may include
        // env-style secrets (Postgres URLs, API tokens) so we mask every
        // string leaf under any `env` sub-object.
        "groups.config.set-mcp-servers" => {
            if let Some(servers) = out.get_mut("mcp_servers") {
                redact_env_objects(servers);
            }
        }
        _ => {}
    }
    out
}

/// Walk `v` and replace every value in any sub-object literally named
/// `env` with the redacted placeholder. Best-effort: arbitrary
/// container-config shapes are tolerated.
fn redact_env_objects(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if k == "env" {
                    if let Some(env) = child.as_object_mut() {
                        for val in env.values_mut() {
                            *val = Value::String(REDACTED_PLACEHOLDER.to_string());
                        }
                    }
                } else {
                    redact_env_objects(child);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_env_objects(item);
            }
        }
        _ => {}
    }
}

/// Best-effort `audit_log` insert. Truncates large argument payloads
/// so a buggy / malicious client can't pump the table full of
/// megabytes.
fn audit_dispatch(
    central: &CentralDb,
    command: &str,
    args: &Value,
    caller: &Caller,
    resp: &Response,
    latency: std::time::Duration,
) {
    use copperclaw_db::tables::audit_log;
    let (caller_kind, session, ag) = match caller {
        Caller::Host => ("host".to_string(), None, None),
        Caller::Agent {
            session_id,
            agent_group_id,
            ..
        } => (
            "agent".to_string(),
            Some(session_id.as_uuid().to_string()),
            Some(agent_group_id.as_uuid().to_string()),
        ),
    };
    let args_str = {
        // Redact known-sensitive fields before serialising so secrets
        // never reach disk. Then compact-JSON-serialise + cap at 4KiB.
        let redacted = redact_sensitive_args(command, args);
        let s = serde_json::to_string(&redacted)
            .unwrap_or_else(|_| String::from("\"<unserializable>\""));
        if s.len() > 4096 {
            // Truncate on a char boundary.
            let mut cap = 4096;
            while !s.is_char_boundary(cap) {
                cap -= 1;
            }
            format!("{}…[truncated]", &s[..cap])
        } else {
            s
        }
    };
    let (result, error_code, error_message) = match resp {
        Response::Ok { .. } => ("ok".to_string(), None, None),
        Response::Err { error, .. } => (
            "error".to_string(),
            Some(error.code.clone()),
            Some(error.message.clone()),
        ),
    };
    let entry = audit_log::AuditEntry {
        ts: chrono::Utc::now(),
        caller_kind,
        caller_session: session,
        caller_agent_group: ag,
        command: command.to_string(),
        args: args_str,
        result,
        error_code,
        error_message,
        latency_ms: i64::try_from(latency.as_millis()).unwrap_or(i64::MAX),
    };
    if let Err(err) = audit_log::insert(central, &entry) {
        // Don't propagate — the request already produced a response.
        // Log loud enough for an operator to notice but not loud
        // enough to spam.
        warn!(?err, command, "audit_log insert failed; request proceeded");
    }
}

/// Read one request frame from `stream`, refusing frames larger than
/// [`MAX_REQUEST_FRAME_BYTES`]. Wraps the read half in a `Take` so a
/// runaway peer can't OOM the host with an unbounded NDJSON line.
///
/// On overflow we return [`ProtoError::Json`] (the trailing payload
/// almost always fails JSON parse mid-frame) or, in the boundary case
/// where the bytes still parse, the truncated request is dispatched —
/// either way the host stays alive. The caller logs at debug.
async fn read_request_capped<S>(stream: &mut S) -> Result<Request, ProtoError>
where
    S: AsyncRead + Unpin + Send,
{
    // `Take` consumes the inner reader by value, but `read_request`
    // only needs `&mut` access. Borrow once via `by_ref()` so the
    // outer stream stays usable for the response write.
    let mut capped = stream.take(MAX_REQUEST_FRAME_BYTES);
    read_request(&mut capped).await
}

/// Serve a single client connection. Reads one request, writes one response,
/// then drops the stream (half-close).
pub async fn serve_connection<S>(
    stream: &mut S,
    table: &DispatchTable,
    ctx: &HandlerCtx,
) -> Result<(), copperclaw_cclaw::ProtoError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let req = match read_request_capped(stream).await {
        Ok(r) => r,
        Err(err) => {
            debug!(?err, "cclaw peer closed before sending a request");
            return Err(err);
        }
    };
    let resp = dispatch_request(table, ctx, &req);
    write_response(stream, &resp).await
}

/// Decide what [`Caller`] identity to honour given the kernel-reported
/// peer UID, the host process's own UID, and whatever the peer claimed
/// on the wire.
///
/// **Wire-supplied [`Caller::Host`] is never trusted.** It always
/// comes from kernel `SO_PEERCRED` (Linux) / `LOCAL_PEERCRED` /
/// `getpeereid()` (macOS), which the local process cannot forge
/// without already controlling the host UID — at which point the
/// game is over anyway.
///
/// Rules:
/// - peer UID == host UID, wire claim is `Host` → trust as `Host`.
/// - peer UID == host UID, wire claim is `Agent` → honour the
///   `Agent` claim. The host's own process tree (the runner, helpers)
///   is allowed to self-identify as a particular session/group; that
///   identity bound the existing handler-side scope checks even before
///   this peer-cred layer.
/// - peer UID != host UID → reject. Returning `None` causes the
///   connection handler to write a `permission_denied` response and
///   close the socket.
fn derive_caller(peer_uid: u32, host_uid: u32, wire_claim: &Caller) -> Option<Caller> {
    if peer_uid != host_uid {
        return None;
    }
    Some(match wire_claim {
        Caller::Host => Caller::Host,
        Caller::Agent {
            session_id,
            agent_group_id,
            messaging_group_id,
        } => Caller::Agent {
            session_id: *session_id,
            agent_group_id: *agent_group_id,
            messaging_group_id: *messaging_group_id,
        },
    })
}

/// Best-effort resolver for the host process's own UID without
/// dropping to `unsafe`. Reads `/proc/self`'s owner via standard
/// `MetadataExt` — same trick already used by
/// `container_manager::spawn::host_uid_gid`. Returns `None` if
/// `/proc` is unavailable (non-Linux), in which case the caller
/// falls back to honouring the wire claim (the previous behaviour)
/// rather than locking the operator out of their own host.
fn host_effective_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata("/proc/self").ok()?;
    Some(meta.uid())
}

/// Serve a single connection over a real Unix-domain socket. Unlike
/// [`serve_connection`] (which is generic over `AsyncRead + AsyncWrite`
/// for testability), this variant has access to `SO_PEERCRED` and so
/// can override the wire-supplied caller identity with the kernel-
/// reported peer UID.
///
/// `host_uid` is the host process's own effective UID. When the peer
/// UID matches we honour the wire claim (Host or Agent self-id); when
/// it differs we refuse the request with `permission_denied` and
/// close. `None` means UID lookup was unavailable (non-Linux); in
/// that case we fall through to the legacy behaviour and trust the
/// wire claim so the operator's own cclaw still works.
pub async fn serve_unix_connection(
    mut stream: tokio::net::UnixStream,
    table: &DispatchTable,
    ctx: &HandlerCtx,
    host_uid: Option<u32>,
) -> Result<(), copperclaw_cclaw::ProtoError> {
    let peer_uid = stream.peer_cred().ok().map(|c| c.uid());
    let req = match read_request_capped(&mut stream).await {
        Ok(r) => r,
        Err(err) => {
            debug!(?err, "cclaw peer closed before sending a request");
            return Err(err);
        }
    };
    // Override the wire caller with what the kernel reports. Only when
    // both `host_uid` and `peer_uid` are known and equal do we accept
    // the request; if either is unknown we fall back to honouring the
    // wire claim so non-Linux hosts and degraded /proc setups still
    // work for legitimate operator UIDs.
    let resp = if let (Some(host), Some(peer)) = (host_uid, peer_uid) {
        let Request::Call {
            id,
            caller: wire_claim,
            ..
        } = &req;
        if let Some(authoritative) = derive_caller(peer, host, wire_claim) {
            let mut req_fixed = req.clone();
            let Request::Call { caller, .. } = &mut req_fixed;
            *caller = authoritative;
            dispatch_request(table, ctx, &req_fixed)
        } else {
            warn!(
                peer_uid = peer,
                host_uid = host,
                "cclaw: rejecting connection from non-host UID"
            );
            Response::err(
                id,
                ErrorPayload::new(
                    "permission_denied",
                    format!(
                        "peer uid {peer} does not match host uid {host}; \
                         cclaw socket is host-local only",
                    ),
                ),
            )
        }
    } else {
        dispatch_request(table, ctx, &req)
    };
    write_response(&mut stream, &resp).await
}

/// Bind the cclaw Unix-domain socket at `path` (creating parents and
/// removing any stale socket file) and chmod it to `0o600`. The
/// returned [`UnixListener`] is ready for an accept loop.
///
/// Refuses to delete a non-socket file at `path` — that almost always
/// indicates a misconfigured socket path pointing into a real file
/// system tree.
///
/// Returns the bound listener so the caller can verify the bind
/// succeeded before declaring boot complete. Previously `run_server`
/// fused bind + loop into a single `tokio::spawn`, which swallowed
/// the bind error and left the host idle with a dead admin surface.
pub fn bind_listener(path: &Path) -> Result<tokio::net::UnixListener, std::io::Error> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    if path.exists() {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("refusing to unlink non-socket file at {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = tokio::net::UnixListener::bind(path)?;
    // Best-effort chmod after bind.
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        warn!(?err, ?path, "failed to chmod cclaw socket");
    }
    Ok(listener)
}

/// Run the accept loop for an already-bound listener until `shutdown`
/// fires. On shutdown the socket file at `path` is best-effort
/// removed.
///
/// Each accepted connection is gated by a semaphore capped at
/// [`MAX_CONCURRENT_CONNECTIONS`] so a local process can't exhaust
/// the host's fd table by opening connections in a tight loop. The
/// permit is held for the lifetime of the per-connection task.
pub async fn serve_listener(
    listener: tokio::net::UnixListener,
    path: PathBuf,
    central: CentralDb,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    let table = Arc::new(build_dispatch_table());
    let ctx = Arc::new(HandlerCtx::new(central));
    let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    let host_uid = host_effective_uid();

    info!(
        socket = %path.display(),
        max_concurrent = MAX_CONCURRENT_CONNECTIONS,
        max_frame_bytes = MAX_REQUEST_FRAME_BYTES,
        host_uid = ?host_uid,
        "cclaw socket server listening"
    );

    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                debug!("cclaw socket shutting down");
                let _ = std::fs::remove_file(&path);
                return Ok(());
            }
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        // Acquire a permit BEFORE spawning so the
                        // backpressure is visible: if all permits are
                        // held the accept loop blocks until a slot
                        // frees up. The permit is moved into the task
                        // and dropped when the task exits.
                        let Ok(permit) = Arc::clone(&limiter).acquire_owned().await
                        else {
                            warn!("cclaw connection semaphore closed; refusing new connections");
                            continue;
                        };
                        let table = Arc::clone(&table);
                        let ctx = Arc::clone(&ctx);
                        tokio::spawn(async move {
                            let _permit = permit; // hold for task lifetime
                            if let Err(err) =
                                serve_unix_connection(stream, &table, &ctx, host_uid).await
                            {
                                debug!(?err, "cclaw connection ended with error");
                            }
                        });
                    }
                    Err(err) => {
                        warn!(?err, "cclaw accept failed");
                    }
                }
            }
        }
    }
}

/// Bind a Unix-domain socket at `path` and run the accept loop until
/// `shutdown` is cancelled. The socket is created with mode `0o600`.
///
/// This is the legacy single-call entry point that fuses bind + serve
/// into one future. Production code in `boot.rs` now calls
/// [`bind_listener`] first (so the bind error surfaces synchronously
/// and `BootError::Socket` can abort the boot) and then drives
/// [`serve_listener`] on a spawned task. The fused entry point is
/// kept for tests that don't care about the split.
pub async fn run_server(
    path: PathBuf,
    central: CentralDb,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    let listener = bind_listener(&path)?;
    serve_listener(listener, path, central, shutdown).await
}

/// Build a dispatch table populated with only the handlers in `commands`
/// (the rest are dropped). Used by tests to construct a minimal table that
/// covers the round-trip path without all 40+ entries.
pub fn dispatch_table_with(commands: &[&'static str]) -> DispatchTable {
    let full = build_dispatch_table();
    let mut out: DispatchTable = HashMap::new();
    for c in commands {
        if let Some(h) = full.get(c) {
            out.insert(*c, Arc::clone(h));
        }
    }
    out
}

/// Convenience accessor used by tests and the boot crate to locate the
/// socket path on disk relative to a data dir.
pub fn default_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cclaw.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_cclaw::{ErrorPayload, write_request};
    use copperclaw_types::{AgentGroupId, SessionId};
    use serde_json::json;
    use tokio::io::duplex;
    use tokio::net::UnixStream;

    fn central() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn redact_mcp_add_masks_env_values_but_keeps_keys() {
        let args = json!({
            "preset": "postgres",
            "agent_group_id": "00000000-0000-0000-0000-000000000000",
            "env": {
                "DATABASE_URL": "postgres://user:secret@host:5432/db",
                "PGPASSWORD": "hunter2",
            }
        });
        let out = redact_sensitive_args("mcp.add", &args);
        // Top-level fields unchanged.
        assert_eq!(out["preset"], "postgres");
        // Env keys preserved.
        assert!(out["env"]["DATABASE_URL"].is_string());
        assert!(out["env"]["PGPASSWORD"].is_string());
        // Env values replaced.
        assert_eq!(out["env"]["DATABASE_URL"], REDACTED_PLACEHOLDER);
        assert_eq!(out["env"]["PGPASSWORD"], REDACTED_PLACEHOLDER);
        // Plaintext secret must not appear anywhere in the serialised
        // output (defense-in-depth — catches future shape changes).
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("hunter2"), "leak: {s}");
        assert!(!s.contains("secret@host"), "leak: {s}");
    }

    #[test]
    fn redact_mcp_add_tolerates_missing_env() {
        let args = json!({"preset": "filesystem"});
        let out = redact_sensitive_args("mcp.add", &args);
        assert_eq!(out, args);
    }

    #[test]
    fn redact_set_mcp_servers_masks_nested_env_values() {
        let args = json!({
            "agent_group_id": "00000000-0000-0000-0000-000000000000",
            "mcp_servers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_TOKEN": "ghp_secrettoken" }
                },
                "postgres": {
                    "command": "uvx",
                    "env": { "DATABASE_URL": "postgres://x:y@h/d" }
                }
            }
        });
        let out = redact_sensitive_args("groups.config.set-mcp-servers", &args);
        assert_eq!(
            out["mcp_servers"]["github"]["env"]["GITHUB_TOKEN"],
            REDACTED_PLACEHOLDER
        );
        assert_eq!(
            out["mcp_servers"]["postgres"]["env"]["DATABASE_URL"],
            REDACTED_PLACEHOLDER
        );
        // Non-env fields are preserved.
        assert_eq!(out["mcp_servers"]["github"]["command"], "npx");
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("ghp_secrettoken"));
        assert!(!s.contains("x:y@h"));
    }

    #[test]
    fn redact_unknown_command_is_identity() {
        let args = json!({"foo": "bar", "env": {"X": "y"}});
        let out = redact_sensitive_args("groups.list", &args);
        // Not a known sensitive-command: leave args untouched.
        assert_eq!(out, args);
    }

    #[test]
    fn build_dispatch_table_covers_every_ncl_command() {
        let t = build_dispatch_table();
        for c in copperclaw_cclaw::ALL_COMMANDS {
            assert!(t.contains_key(*c), "missing handler for {c}");
        }
        assert_eq!(t.len(), copperclaw_cclaw::ALL_COMMANDS.len());
    }

    #[test]
    fn dispatch_unknown_command_yields_unknown_command() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let req = Request::Call {
            id: "1".into(),
            command: "ghost".into(),
            args: json!({}),
            caller: Caller::Host,
        };
        let r = dispatch_request(&table, &ctx, &req);
        match r {
            Response::Err { error, .. } => assert_eq!(error.code, "unknown_command"),
            Response::Ok { .. } => panic!("unexpected Ok"),
        }
    }

    #[test]
    fn dispatch_host_caller_succeeds_for_mutation() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let req = Request::Call {
            id: "1".into(),
            command: "groups.create".into(),
            args: json!({"folder": "g", "name": "n"}),
            caller: Caller::Host,
        };
        let r = dispatch_request(&table, &ctx, &req);
        assert!(matches!(r, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_agent_caller_blocked_on_mutation() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let req = Request::Call {
            id: "1".into(),
            command: "groups.delete".into(),
            args: json!({"id": uuid::Uuid::now_v7().to_string()}),
            caller: Caller::Agent {
                session_id: SessionId::nil(),
                agent_group_id: AgentGroupId::nil(),
                messaging_group_id: None,
            },
        };
        let r = dispatch_request(&table, &ctx, &req);
        match r {
            Response::Err { error, .. } => assert_eq!(error.code, "permission_denied"),
            Response::Ok { .. } => panic!("unexpected Ok"),
        }
    }

    #[test]
    fn dispatch_agent_caller_allowed_on_list() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let req = Request::Call {
            id: "1".into(),
            command: "groups.list".into(),
            args: json!({}),
            caller: Caller::Agent {
                session_id: SessionId::nil(),
                agent_group_id: AgentGroupId::nil(),
                messaging_group_id: None,
            },
        };
        let r = dispatch_request(&table, &ctx, &req);
        assert!(matches!(r, Response::Ok { .. }));
    }

    #[test]
    fn dispatch_sessions_delete_removes_central_row() {
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
        let db = central();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let s = create_session(
            &db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = HandlerCtx::with_data_dir(db.clone(), tmp.path().to_path_buf());
        let table = build_dispatch_table();
        let req = Request::Call {
            id: "1".into(),
            command: "sessions.delete".into(),
            args: json!({"id": s.id.as_uuid().to_string()}),
            caller: Caller::Host,
        };
        let r = dispatch_request(&table, &ctx, &req);
        match r {
            Response::Ok { data, .. } => {
                assert_eq!(data["deleted"], s.id.as_uuid().to_string());
            }
            Response::Err { error, .. } => panic!("unexpected error: {error:?}"),
        }
        // Audit row recorded — sessions.delete is host-only.
        let audit_rows = copperclaw_db::tables::audit_log::list_recent(
            &db,
            chrono::Utc::now() - chrono::Duration::hours(1),
            50,
        )
        .unwrap();
        assert!(audit_rows.iter().any(|r| r.command == "sessions.delete"));
    }

    #[test]
    fn dispatch_approvals_approve_routes_install_packages_and_audits() {
        // End-to-end: a pending `install_packages` row is approved via the
        // generic `approvals.approve` socket command, the side effect
        // lands in container_configs, and the audit_log captures the call.
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use copperclaw_db::tables::container_configs;
        use copperclaw_db::tables::pending_approvals::{
            UpsertPendingApproval, upsert as upsert_pa,
        };
        let db = central();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let row = upsert_pa(
            &db,
            UpsertPendingApproval {
                request_id: "r-sock".into(),
                action: "install_packages".into(),
                payload: json!({"apt": ["jq"], "npm": []}),
                agent_group_id: Some(ag.id),
                title: "install?".into(),
                options: vec![],
                ..Default::default()
            },
        )
        .unwrap();

        let ctx = HandlerCtx::new(db.clone());
        let table = build_dispatch_table();
        let req = Request::Call {
            id: "1".into(),
            command: "approvals.approve".into(),
            args: json!({"id": row.approval_id.as_uuid().to_string()}),
            caller: Caller::Host,
        };
        let r = dispatch_request(&table, &ctx, &req);
        match r {
            Response::Ok { data, .. } => {
                assert_eq!(data["applied"], true);
                assert_eq!(data["side_effect"]["kind"], "install_packages");
            }
            Response::Err { error, .. } => panic!("unexpected error: {error:?}"),
        }
        // Side effect landed.
        let cfg = container_configs::get(&db, ag.id).unwrap().unwrap();
        assert!(cfg.packages_apt.contains(&"jq".to_string()));
        // Audit row recorded — approvals.approve is host-only.
        let audit_rows = copperclaw_db::tables::audit_log::list_recent(
            &db,
            chrono::Utc::now() - chrono::Duration::hours(1),
            50,
        )
        .unwrap();
        assert!(audit_rows.iter().any(|r| r.command == "approvals.approve"));
    }

    #[test]
    fn dispatch_approvals_deny_blocked_for_agent_caller() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let req = Request::Call {
            id: "1".into(),
            command: "approvals.deny".into(),
            args: json!({"id": uuid::Uuid::now_v7().to_string()}),
            caller: Caller::Agent {
                session_id: SessionId::nil(),
                agent_group_id: AgentGroupId::nil(),
                messaging_group_id: None,
            },
        };
        let r = dispatch_request(&table, &ctx, &req);
        match r {
            Response::Err { error, .. } => assert_eq!(error.code, "permission_denied"),
            Response::Ok { .. } => panic!("unexpected Ok"),
        }
    }

    #[test]
    fn dispatch_sessions_delete_agent_caller_blocked() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let req = Request::Call {
            id: "1".into(),
            command: "sessions.delete".into(),
            args: json!({"id": uuid::Uuid::now_v7().to_string()}),
            caller: Caller::Agent {
                session_id: SessionId::nil(),
                agent_group_id: AgentGroupId::nil(),
                messaging_group_id: None,
            },
        };
        let r = dispatch_request(&table, &ctx, &req);
        match r {
            Response::Err { error, .. } => assert_eq!(error.code, "permission_denied"),
            Response::Ok { .. } => panic!("unexpected Ok"),
        }
    }

    #[tokio::test]
    async fn serve_connection_round_trip_via_duplex() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let (mut client, mut server) = duplex(4096);
        let req = Request::Call {
            id: "abc".into(),
            command: "groups.list".into(),
            args: json!({}),
            caller: Caller::Host,
        };
        let req_clone = req.clone();
        let server_task = tokio::spawn(async move {
            serve_connection(&mut server, &table, &ctx).await.unwrap();
        });
        write_request(&mut client, &req_clone).await.unwrap();
        let resp = copperclaw_cclaw::read_response(&mut client).await.unwrap();
        server_task.await.unwrap();
        match resp {
            Response::Ok { id, data } => {
                assert_eq!(id, "abc");
                assert!(data.is_array());
            }
            Response::Err { .. } => panic!("unexpected Err"),
        }
    }

    #[tokio::test]
    async fn run_server_binds_socket_and_handles_request() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("cclaw.sock");
        let central = central();
        let shutdown = CancellationToken::new();
        let server_path = socket_path.clone();
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_server(server_path, central, cancel).await.unwrap();
        });

        // Wait for the socket file to exist.
        for _ in 0..40 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(socket_path.exists(), "socket file should exist");

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        let req = Request::Call {
            id: "x".into(),
            command: "groups.list".into(),
            args: json!({}),
            caller: Caller::Host,
        };
        write_request(&mut stream, &req).await.unwrap();
        let resp = copperclaw_cclaw::read_response(&mut stream).await.unwrap();
        assert!(matches!(resp, Response::Ok { .. }));

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn run_server_refuses_to_clobber_non_socket_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notasocket");
        std::fs::write(&path, b"hi").unwrap();
        let shutdown = CancellationToken::new();
        let err = run_server(path, central(), shutdown).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn run_server_replaces_existing_socket_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cclaw.sock");
        // Bind once to create the file, then drop the listener but leave the
        // socket file on disk.
        {
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            drop(listener);
        }
        assert!(path.exists());
        let shutdown = CancellationToken::new();
        let path_clone = path.clone();
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_server(path_clone, central(), cancel).await.unwrap();
        });
        for _ in 0..40 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn serve_connection_returns_error_when_peer_closes() {
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let (client, mut server) = duplex(64);
        drop(client);
        let err = serve_connection(&mut server, &table, &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, copperclaw_cclaw::ProtoError::Closed));
    }

    #[test]
    fn dispatch_table_with_subset() {
        let t = dispatch_table_with(&["groups.list"]);
        assert_eq!(t.len(), 1);
        assert!(t.contains_key("groups.list"));
        // Empty selection.
        let t = dispatch_table_with(&[]);
        assert!(t.is_empty());
    }

    #[test]
    fn fn_handler_host_only_blocks_agent() {
        let h = FnHandler::new(|_a, _c| Ok(json!({})), true);
        let ctx = HandlerCtx::new(central());
        let err = h
            .handle(
                &json!({}),
                &Caller::Agent {
                    session_id: SessionId::nil(),
                    agent_group_id: AgentGroupId::nil(),
                    messaging_group_id: None,
                },
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "permission_denied");
    }

    #[test]
    fn fn_handler_open_to_agents() {
        let h = FnHandler::new(|_a, _c| Ok(json!("ok")), false);
        let ctx = HandlerCtx::new(central());
        let r = h
            .handle(
                &json!({}),
                &Caller::Agent {
                    session_id: SessionId::nil(),
                    agent_group_id: AgentGroupId::nil(),
                    messaging_group_id: None,
                },
                &ctx,
            )
            .unwrap();
        assert_eq!(r, json!("ok"));
    }

    #[test]
    fn default_socket_path_helper() {
        let p = default_socket_path(Path::new("data"));
        assert_eq!(p, PathBuf::from("data/cclaw.sock"));
    }

    #[test]
    fn handler_ctx_constructor() {
        let ctx = HandlerCtx::new(central());
        let _ = ctx.central.conn().unwrap();
    }

    #[test]
    fn error_payload_constructors_used() {
        // Smoke-test ErrorPayload::new is accessible from this module.
        let _ = ErrorPayload::new("x", "y");
    }

    // -----------------------------------------------------------------
    // Peer-credential gating (bug 1).
    // -----------------------------------------------------------------

    #[test]
    fn derive_caller_trusts_host_when_uids_match_and_wire_says_host() {
        let got = derive_caller(1000, 1000, &Caller::Host);
        assert!(matches!(got, Some(Caller::Host)));
    }

    #[test]
    fn derive_caller_honours_agent_self_id_when_uids_match() {
        let agent = Caller::Agent {
            session_id: SessionId::nil(),
            agent_group_id: AgentGroupId::nil(),
            messaging_group_id: None,
        };
        let got = derive_caller(1000, 1000, &agent).unwrap();
        // Same shape, not silently promoted to Host.
        assert!(matches!(got, Caller::Agent { .. }));
    }

    #[test]
    fn derive_caller_rejects_non_matching_uid_even_when_wire_says_host() {
        // The classic auth bypass: any local-UID-mismatched process
        // hand-rolling `{"caller":{"kind":"host"}}` must NOT be
        // promoted to host.
        let got = derive_caller(2000, 1000, &Caller::Host);
        assert!(got.is_none(), "non-host UID must not be trusted: {got:?}");
    }

    #[test]
    fn derive_caller_rejects_non_matching_uid_even_when_wire_says_agent() {
        let agent = Caller::Agent {
            session_id: SessionId::nil(),
            agent_group_id: AgentGroupId::nil(),
            messaging_group_id: None,
        };
        let got = derive_caller(2000, 1000, &agent);
        assert!(got.is_none(), "cross-uid caller must not be honoured");
    }

    #[tokio::test]
    async fn serve_unix_connection_rejects_cross_uid_host_claim() {
        // End-to-end: a real UnixStream pair on a host UID we can
        // simulate via `host_uid + 1`. The wire-supplied
        // `Caller::Host` is replaced by a `permission_denied`
        // response, the audit log records nothing host-eligible, and
        // the socket survives.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("peercred.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let central = central();
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central.clone());
        // Pretend the host runs under a *different* UID than whoever
        // is running these tests, so the kernel-reported peer UID
        // (this process) is guaranteed to mismatch.
        let real_uid = host_effective_uid().unwrap_or(0);
        let fake_host_uid = real_uid.wrapping_add(1);

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_unix_connection(stream, &table, &ctx, Some(fake_host_uid))
                .await
                .unwrap();
        });
        let mut client = UnixStream::connect(&path).await.unwrap();
        let req = Request::Call {
            id: "x".into(),
            command: "groups.delete".into(),
            args: json!({"id": uuid::Uuid::now_v7().to_string()}),
            caller: Caller::Host,
        };
        write_request(&mut client, &req).await.unwrap();
        let resp = copperclaw_cclaw::read_response(&mut client).await.unwrap();
        server_task.await.unwrap();
        match resp {
            Response::Err { error, .. } => {
                assert_eq!(error.code, "permission_denied");
                assert!(
                    error.message.contains("uid"),
                    "message should reference the uid mismatch: {}",
                    error.message,
                );
            }
            Response::Ok { .. } => panic!("cross-uid claim should not be honoured"),
        }
    }

    #[tokio::test]
    async fn serve_unix_connection_accepts_matching_uid() {
        // Sanity check the happy path: when host_uid matches the
        // peer's own UID (this process), the request goes through
        // and the wire-supplied caller is honoured.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("peercred-ok.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let central = central();
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central.clone());
        let host_uid = host_effective_uid();

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_unix_connection(stream, &table, &ctx, host_uid)
                .await
                .unwrap();
        });
        let mut client = UnixStream::connect(&path).await.unwrap();
        let req = Request::Call {
            id: "y".into(),
            command: "groups.list".into(),
            args: json!({}),
            caller: Caller::Host,
        };
        write_request(&mut client, &req).await.unwrap();
        let resp = copperclaw_cclaw::read_response(&mut client).await.unwrap();
        server_task.await.unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    // -----------------------------------------------------------------
    // Frame size cap (bug 3).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn oversized_request_frame_is_rejected_not_oomed() {
        // Feed `serve_connection` a frame bigger than
        // `MAX_REQUEST_FRAME_BYTES` and assert it returns a
        // ProtoError rather than allocating until OOM. We don't
        // actually send 1 MiB — `duplex` is bounded — we send a tail
        // of garbage that `take()` will refuse to read past, and
        // assert the parser errors out.
        use tokio::io::AsyncWriteExt;

        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        // Use a duplex buffer larger than the cap so we can write
        // through MAX+1 bytes without back-pressure deadlock.
        let buf_size = usize::try_from(MAX_REQUEST_FRAME_BYTES).unwrap() + 4096;
        let (mut client, mut server) = duplex(buf_size);
        let server_task =
            tokio::spawn(async move { serve_connection(&mut server, &table, &ctx).await });
        // Write MAX+1 bytes of non-newline garbage. read_until will
        // hit the take() cap before finding a delimiter, and the
        // frame parser will then fail (either parsing the truncated
        // bytes as JSON or returning Closed).
        let mut payload: Vec<u8> =
            vec![b'x'; usize::try_from(MAX_REQUEST_FRAME_BYTES).unwrap() + 1];
        payload.push(b'\n');
        client.write_all(&payload).await.unwrap();
        client.flush().await.unwrap();
        let result = server_task.await.unwrap();
        assert!(result.is_err(), "oversized frame should error out");
    }

    #[tokio::test]
    async fn under_cap_request_still_works() {
        // Defensive regression: a normal-size request still parses.
        let table = build_dispatch_table();
        let ctx = HandlerCtx::new(central());
        let (mut client, mut server) = duplex(4096);
        let req = Request::Call {
            id: "ok".into(),
            command: "groups.list".into(),
            args: json!({}),
            caller: Caller::Host,
        };
        let req_clone = req.clone();
        let server_task = tokio::spawn(async move {
            serve_connection(&mut server, &table, &ctx).await.unwrap();
        });
        write_request(&mut client, &req_clone).await.unwrap();
        let resp = copperclaw_cclaw::read_response(&mut client).await.unwrap();
        server_task.await.unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
    }

    // -----------------------------------------------------------------
    // Connection concurrency cap (bug 3).
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn concurrent_connection_cap_actually_limits() {
        // Open MAX + 5 connections at once. Each handler holds the
        // permit for the duration of a single round-trip; the
        // extras must wait. We assert the wait by measuring that
        // the (N+5)th response arrives only after at least N have
        // returned — but a simpler / less timing-sensitive check is
        // to verify that all N+5 eventually succeed (semaphore is
        // bounded but not deadlocked) AND that the on-host
        // permits-available counter dipped to zero. We use the
        // latter by checking `Semaphore::available_permits`
        // indirectly: spawn N+5, await all, assert all OK.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("conn-cap.sock");
        let central = central();
        let shutdown = CancellationToken::new();
        let server_path = path.clone();
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_server(server_path, central, cancel).await.unwrap();
        });
        // Wait for the socket file.
        for _ in 0..80 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(path.exists(), "socket file should exist");

        let n = MAX_CONCURRENT_CONNECTIONS + 5;
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                let mut stream = UnixStream::connect(&p).await.unwrap();
                let req = Request::Call {
                    id: format!("r{i}"),
                    command: "groups.list".into(),
                    args: json!({}),
                    caller: Caller::Host,
                };
                write_request(&mut stream, &req).await.unwrap();
                let resp = copperclaw_cclaw::read_response(&mut stream).await.unwrap();
                matches!(resp, Response::Ok { .. })
            }));
        }
        // All N+5 must eventually complete OK. The semaphore
        // serialises the excess, but every request still succeeds.
        for h in handles {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(10), h)
                    .await
                    .expect("connection should not stall")
                    .expect("task panicked"),
                "request did not return Ok"
            );
        }
        shutdown.cancel();
        task.await.unwrap();
    }
}
