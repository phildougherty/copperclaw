//! Unix-socket server + dispatch table for `iclaw` commands.
//!
//! Each connection runs the framing loop in [`serve_connection`]: read one
//! `Request::Call`, look up its `command` in the [`DispatchTable`], invoke
//! the handler, write the [`ironclaw_iclaw::Response`] back, half-close.
//!
//! The dispatch table is a small `HashMap<&'static str, Arc<dyn CommandHandler>>`
//! so handlers stay swappable in tests. [`build_dispatch_table`] returns the
//! production table — every command in [`ironclaw_iclaw::ALL_COMMANDS`].

use crate::handlers;
use ironclaw_db::central::CentralDb;
use ironclaw_iclaw::{
    read_request, write_response, Caller, ErrorPayload, Request, Response,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Context handed to every [`CommandHandler::handle`] call.
pub struct HandlerCtx {
    pub central: CentralDb,
}

impl HandlerCtx {
    pub fn new(central: CentralDb) -> Self {
        Self { central }
    }
}

/// Trait every command handler implements.
///
/// Handlers are not async because the underlying `ironclaw-db` table fns are
/// synchronous; this keeps the dispatch loop simple. If a future handler
/// needs to do I/O it can spawn its own task.
pub trait CommandHandler: Send + Sync {
    fn handle(&self, args: &Value, caller: &Caller, ctx: &HandlerCtx) -> Result<Value, ErrorPayload>;
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

/// In-memory mapping of dotted command names to their handler.
pub type DispatchTable = HashMap<&'static str, Arc<dyn CommandHandler>>;

/// Build the production dispatch table.
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

    ins!("groups.list", handlers::groups::list, false);
    ins!("groups.get", handlers::groups::get, false);
    ins!("groups.create", handlers::groups::create, true);
    ins!("groups.update", handlers::groups::update, true);
    ins!("groups.delete", handlers::groups::delete, true);
    ins!("groups.restart", handlers::groups::restart, true);
    ins!("groups.config.get", handlers::groups::config_get, false);
    ins!("groups.config.update", handlers::groups::config_update, true);
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

    ins!("messaging-groups.list", handlers::messaging_groups::list, false);
    ins!("messaging-groups.get", handlers::messaging_groups::get, false);
    ins!("messaging-groups.create", handlers::messaging_groups::create, true);
    ins!("messaging-groups.update", handlers::messaging_groups::update, true);
    ins!("messaging-groups.delete", handlers::messaging_groups::delete, true);

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

    ins!("user-dms.list", handlers::user_dms::list, false);
    ins!("dropped-messages.list", handlers::dropped_messages::list, false);
    ins!("approvals.list", handlers::approvals::list, false);
    ins!("approvals.get", handlers::approvals::get, false);
    ins!(
        "approvals.approve_sender",
        handlers::approvals::approve_sender,
        true
    );
    ins!("audit.list", handlers::audit::list, false);
    ins!("budgets.list", handlers::budgets::list, false);
    ins!("budgets.set", handlers::budgets::set, true);
    ins!("usage.rollup", handlers::usage::rollup, false);

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
    use ironclaw_db::tables::audit_log;
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
        // Compact JSON, capped at 4KiB to keep audit rows small.
        let s = serde_json::to_string(args).unwrap_or_else(|_| String::from("\"<unserializable>\""));
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
        warn!(
            ?err,
            command,
            "audit_log insert failed; request proceeded"
        );
    }
}

/// Serve a single client connection. Reads one request, writes one response,
/// then drops the stream (half-close).
pub async fn serve_connection<S>(
    stream: &mut S,
    table: &DispatchTable,
    ctx: &HandlerCtx,
) -> Result<(), ironclaw_iclaw::ProtoError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let req = match read_request(stream).await {
        Ok(r) => r,
        Err(err) => {
            debug!(?err, "iclaw peer closed before sending a request");
            return Err(err);
        }
    };
    let resp = dispatch_request(table, ctx, &req);
    write_response(stream, &resp).await
}

/// Bind a Unix-domain socket at `path` and run the accept loop until
/// `shutdown` is cancelled. The socket is created with mode `0o600`.
///
/// Any existing socket file at `path` is removed first if it is itself a
/// socket; the function refuses to delete other file kinds.
pub async fn run_server(
    path: PathBuf,
    central: CentralDb,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    if path.exists() {
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.file_type().is_socket() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("refusing to unlink non-socket file at {}", path.display()),
            ));
        }
        std::fs::remove_file(&path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = tokio::net::UnixListener::bind(&path)?;
    // Best-effort chmod after bind.
    if let Err(err) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        warn!(?err, ?path, "failed to chmod iclaw socket");
    }

    let table = Arc::new(build_dispatch_table());
    let ctx = Arc::new(HandlerCtx::new(central));

    info!(socket = %path.display(), "iclaw socket server listening");

    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                debug!("iclaw socket shutting down");
                let _ = std::fs::remove_file(&path);
                return Ok(());
            }
            res = listener.accept() => {
                match res {
                    Ok((mut stream, _addr)) => {
                        let table = Arc::clone(&table);
                        let ctx = Arc::clone(&ctx);
                        tokio::spawn(async move {
                            if let Err(err) = serve_connection(&mut stream, &table, &ctx).await {
                                debug!(?err, "iclaw connection ended with error");
                            }
                        });
                    }
                    Err(err) => {
                        warn!(?err, "iclaw accept failed");
                    }
                }
            }
        }
    }
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
    data_dir.join("iclaw.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_iclaw::{ErrorPayload, write_request};
    use ironclaw_types::{AgentGroupId, SessionId};
    use serde_json::json;
    use tokio::io::duplex;
    use tokio::net::UnixStream;

    fn central() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn build_dispatch_table_covers_every_ncl_command() {
        let t = build_dispatch_table();
        for c in ironclaw_iclaw::ALL_COMMANDS {
            assert!(t.contains_key(*c), "missing handler for {c}");
        }
        assert_eq!(t.len(), ironclaw_iclaw::ALL_COMMANDS.len());
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
        let resp = ironclaw_iclaw::read_response(&mut client).await.unwrap();
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
        let socket_path = tmp.path().join("iclaw.sock");
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
        let resp = ironclaw_iclaw::read_response(&mut stream).await.unwrap();
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
        let path = tmp.path().join("iclaw.sock");
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
        let err = serve_connection(&mut server, &table, &ctx).await.unwrap_err();
        assert!(matches!(err, ironclaw_iclaw::ProtoError::Closed));
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
        assert_eq!(p, PathBuf::from("data/iclaw.sock"));
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
}
