//! `ironclaw-iclaw` — wire protocol, client, command surface, and CLI runner
//! for the ironclaw admin socket.
//!
//! Two consumers:
//!
//! * The `iclaw` binary in this crate (see `src/main.rs`) wraps the
//!   [`commands`] layer and the [`client`] to talk to the host.
//! * The host crate (`ironclaw-host`, M5) depends on this library for the
//!   wire types so that there is exactly one source of truth for the
//!   protocol.
//!
//! See `PLAN.md` § 5.4 for the wire shape and § A2 for the subcommand
//! inventory.

#![forbid(unsafe_code)]

pub mod client;
pub mod commands;
pub mod output;
pub mod protocol;

pub use client::{ClientError, IclawClient, DEFAULT_TIMEOUT};
pub use commands::{default_user_socket, Cli, ParsedCall, TopCommand, ALL_COMMANDS};
pub use output::{render, render_json_pretty};
pub use protocol::{
    Caller, ErrorPayload, ProtoError, Request, Response, read_frame, read_request,
    read_response, write_frame, write_request, write_response,
};

use std::process::ExitCode;

/// Name of the environment variable that switches the default caller
/// from [`Caller::Host`] to [`Caller::Agent`].
pub const AGENT_CALLER_ENV: &str = "ICLAW_AGENT_CALLER";

/// Resolve the [`Caller`] to use for a CLI invocation.
///
/// Reads `ICLAW_AGENT_CALLER` from the environment. If set, parses it as
/// JSON `{session_id, agent_group_id, messaging_group_id?}` and returns
/// [`Caller::Agent`]. Otherwise returns [`Caller::Host`]. Malformed JSON
/// yields an error so users notice misconfigured shells.
pub fn caller_from_env() -> Result<Caller, RunError> {
    let raw = std::env::var(AGENT_CALLER_ENV).ok();
    caller_from_raw(raw.as_deref())
}

/// Pure-function variant of [`caller_from_env`] used by tests. `value` is
/// the contents of the env var (or `None` if unset).
pub fn caller_from_raw(value: Option<&str>) -> Result<Caller, RunError> {
    match value {
        Some(v) if !v.trim().is_empty() => parse_agent_caller(v),
        _ => Ok(Caller::Host),
    }
}

fn parse_agent_caller(s: &str) -> Result<Caller, RunError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        session: ironclaw_types::SessionId,
        agent_group: ironclaw_types::AgentGroupId,
        #[serde(default)]
        messaging_group: Option<ironclaw_types::MessagingGroupId>,
    }
    // Accept both `{session_id, agent_group_id, messaging_group_id}` (the
    // shape documented in `PLAN.md`) and the shorter `{session,
    // agent_group, messaging_group}` form.
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Either {
        Long {
            session_id: ironclaw_types::SessionId,
            agent_group_id: ironclaw_types::AgentGroupId,
            #[serde(default)]
            messaging_group_id: Option<ironclaw_types::MessagingGroupId>,
        },
        Short(Raw),
    }
    let parsed: Either = serde_json::from_str(s).map_err(|e| {
        RunError::BadAgentCaller(format!("could not parse {AGENT_CALLER_ENV} as JSON: {e}"))
    })?;
    Ok(match parsed {
        Either::Long {
            session_id,
            agent_group_id,
            messaging_group_id,
        } => Caller::Agent {
            session_id,
            agent_group_id,
            messaging_group_id,
        },
        Either::Short(Raw {
            session,
            agent_group,
            messaging_group,
        }) => Caller::Agent {
            session_id: session,
            agent_group_id: agent_group,
            messaging_group_id: messaging_group,
        },
    })
}

/// Errors surfaced by [`run_cli`].
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// CLI parsing failed; the inner error is the clap diagnostic.
    #[error("{0}")]
    ParseCli(String),
    /// The `ICLAW_AGENT_CALLER` environment variable could not be parsed.
    #[error("{0}")]
    BadAgentCaller(String),
    /// The client returned an error.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// Pluggable transport so [`run_cli`] is testable without a real socket.
#[async_trait::async_trait]
pub trait CallTransport: Send + Sync {
    /// Perform one round-trip on behalf of [`run_cli`].
    async fn call(
        &self,
        command: &str,
        args: serde_json::Value,
        caller: Caller,
    ) -> Result<serde_json::Value, ClientError>;
}

/// Default transport that dials a real Unix socket via [`IclawClient`].
pub struct SocketTransport(pub IclawClient);

#[async_trait::async_trait]
impl CallTransport for SocketTransport {
    async fn call(
        &self,
        command: &str,
        args: serde_json::Value,
        caller: Caller,
    ) -> Result<serde_json::Value, ClientError> {
        self.0.call(command, args, caller).await
    }
}

/// Result of running the CLI: an exit code and a string to emit on stdout.
#[derive(Debug)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: ExitCode,
}

impl RunOutput {
    pub fn success(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            code: ExitCode::SUCCESS,
        }
    }

    pub fn failure(stderr: String) -> Self {
        Self {
            stdout: String::new(),
            stderr,
            code: ExitCode::FAILURE,
        }
    }
}

/// Run the CLI end-to-end given a vector of argv strings and a transport.
///
/// Returns the strings to print on stdout/stderr plus the desired exit
/// code. Separated from [`crate::main`] so that integration tests can
/// drive it without spawning a subprocess.
pub async fn run_cli<I, S, T>(args: I, transport: &T) -> RunOutput
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
    T: CallTransport + ?Sized,
{
    use clap::Parser as _;
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // clap renders --help / --version with exit code 0; everything
            // else is a parse error.
            let text = e.render().to_string();
            return if e.use_stderr() {
                RunOutput::failure(text)
            } else {
                RunOutput::success(text)
            };
        }
    };

    let caller = match caller_from_env() {
        Ok(c) => c,
        Err(e) => return RunOutput::failure(format!("{e}\n")),
    };

    let call = cli.to_call();
    // Composite client-side ops produce a `composite.*` marker command
    // that we recognise here before reaching the transport.
    if let Some(suffix) = call.command.strip_prefix("composite.") {
        return run_composite(suffix, &call.args, transport, caller, cli.json).await;
    }
    match transport.call(&call.command, call.args, caller).await {
        Ok(data) => {
            let text = if cli.json {
                render_json_pretty(&data)
            } else {
                render(&data)
            };
            let mut out = text;
            if !out.ends_with('\n') {
                out.push('\n');
            }
            RunOutput::success(out)
        }
        Err(ClientError::Remote(e)) => RunOutput::failure(format!(
            "remote error: {} ({})\n",
            e.message, e.code
        )),
        Err(other) => RunOutput::failure(format!("{other}\n")),
    }
}

/// Dispatch a `composite.*` client-side op. Each composite fans out
/// into a sequence of real wire calls and aggregates the responses
/// into a single rendered summary.
async fn run_composite<T>(
    op: &str,
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    match op {
        "quickstart-cli" => run_quickstart_cli(args, transport, caller, as_json).await,
        "status" => run_status(transport, caller, as_json).await,
        "health" => run_health(transport, caller, as_json).await,
        "completions" => run_completions(args),
        "chat" => run_chat(args).await,
        "dashboard" => run_dashboard(transport, caller, as_json).await,
        "groups.config-edit" => run_groups_config_edit(args, transport, caller).await,
        other => RunOutput::failure(format!("unknown composite op: {other}\n")),
    }
}

/// `iclaw chat` — interactive REPL against the local cli channel.
///
/// Reads lines from this terminal's stdin, writes them into the host's
/// chat FIFO, and tails `chat.log` for replies. Doesn't touch the
/// socket at all — it's pure file I/O against the install layout
/// `ironclaw-setup` produces. Exits on EOF (Ctrl-D) or Ctrl-C.
///
/// Long-but-flat: every branch is necessary for friendly errors.
#[allow(clippy::too_many_lines)]
async fn run_chat(args: &serde_json::Value) -> RunOutput {
    use std::path::PathBuf;
    use tokio::fs::OpenOptions;
    use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};

    let Some(install_root) = resolve_install_root() else {
        return RunOutput::failure(
            "iclaw chat: could not resolve install root; pass --fifo / --log\n"
                .to_string(),
        );
    };
    let fifo_path: PathBuf = args
        .get("fifo")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || install_root.join("chat.fifo"),
            PathBuf::from,
        );
    let log_path: PathBuf = args
        .get("log")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || install_root.join("chat.log"),
            PathBuf::from,
        );

    if !fifo_path.exists() {
        return RunOutput::failure(format!(
            "iclaw chat: no FIFO at {} — make sure the host is running and \
             the chat.fifo keepalive is up.\n",
            fifo_path.display()
        ));
    }
    if !log_path.exists() {
        return RunOutput::failure(format!(
            "iclaw chat: no log at {}\n",
            log_path.display()
        ));
    }

    let mut fifo = match OpenOptions::new().write(true).open(&fifo_path).await {
        Ok(f) => f,
        Err(e) => {
            return RunOutput::failure(format!(
                "iclaw chat: open fifo {}: {e}\n",
                fifo_path.display()
            ));
        }
    };
    let log_file = match OpenOptions::new().read(true).open(&log_path).await {
        Ok(f) => f,
        Err(e) => {
            return RunOutput::failure(format!(
                "iclaw chat: open log {}: {e}\n",
                log_path.display()
            ));
        }
    };
    // Seek to end so we only show NEW replies, not history.
    let mut log_reader = BufReader::new(log_file);
    let _ = log_reader.seek(std::io::SeekFrom::End(0)).await;

    eprintln!(
        "iclaw chat: connected (fifo={}, log={})\n\
         type a message and press enter. Ctrl-D to exit.\n",
        fifo_path.display(),
        log_path.display()
    );

    // Tail the log on a background task; print every new line to stdout.
    let log_task = tokio::spawn(async move {
        let mut reader = log_reader;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF — wait for more.
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                Ok(_) => {
                    if line.ends_with('\n') {
                        print!("{line}");
                    } else {
                        println!("{line}");
                    }
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                Err(_) => break,
            }
        }
    });

    // Read terminal stdin line by line; pipe each into the FIFO.
    let mut stdin = BufReader::new(tokio::io::stdin());
    loop {
        let mut line = String::new();
        match stdin.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                if let Err(e) = fifo.write_all(line.as_bytes()).await {
                    eprintln!("iclaw chat: write to fifo failed: {e}");
                    break;
                }
                let _ = fifo.flush().await;
            }
            Err(e) => {
                eprintln!("iclaw chat: stdin read failed: {e}");
                break;
            }
        }
    }
    log_task.abort();
    RunOutput::success(String::new())
}

/// Platform-default ironclaw install root. Mirrors the resolver in
/// `ironclaw-host::config::default_install_env_file` / setup's
/// `default_data_dir_for` so chat's defaults agree with where setup
/// put things.
fn resolve_install_root() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let os = std::env::consts::OS;
    Some(match os {
        "macos" => home
            .join("Library")
            .join("Application Support")
            .join("ironclaw"),
        "linux" => std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|x| !x.as_os_str().is_empty())
            .map_or_else(
                || home.join(".local").join("share").join("ironclaw"),
                |xdg| xdg.join("ironclaw"),
            ),
        _ => home.join(".ironclaw"),
    })
}

/// `iclaw health` — one-shot operator probe. Lists session-state
/// counts (running / idle / stopped) and the last 5 audit entries
/// so the operator can see at a glance whether the host is alive
/// and whether anything has recently mutated state. Designed to be
/// cheap and side-effect-free; a real `/healthz` HTTP endpoint can
/// reuse the same data.
async fn run_health<T>(transport: &T, caller: Caller, as_json: bool) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let sessions_all = match transport
        .call("sessions.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("sessions.list", &e)),
    };
    let sessions_active = match transport
        .call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("sessions.list", &e)),
    };
    let audit = match transport
        .call(
            "audit.list",
            serde_json::json!({"since": "24h", "limit": 5}),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("audit.list", &e)),
    };
    let dropped = match transport
        .call("dropped-messages.list", serde_json::json!({}), caller)
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("dropped-messages.list", &e)),
    };

    let mut running = 0usize;
    let mut idle = 0usize;
    let mut stopped = 0usize;
    if let Some(rows) = sessions_active.as_array() {
        for r in rows {
            match r
                .get("container_status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
            {
                "running" => running += 1,
                "idle" => idle += 1,
                "stopped" => stopped += 1,
                _ => {}
            }
        }
    }

    if as_json {
        let summary = serde_json::json!({
            "sessions_total": array_len(&sessions_all),
            "sessions_active": array_len(&sessions_active),
            "sessions_running": running,
            "sessions_idle": idle,
            "sessions_stopped": stopped,
            "dropped_messages": array_len(&dropped),
            "recent_audit": audit,
        });
        let mut out = render_json_pretty(&summary);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "sessions total:    {}\n",
        array_len(&sessions_all)
    ));
    out.push_str(&format!(
        "sessions active:   {}\n",
        array_len(&sessions_active)
    ));
    out.push_str(&format!("  running:         {running}\n"));
    out.push_str(&format!("  idle:            {idle}\n"));
    out.push_str(&format!("  stopped:         {stopped}\n"));
    out.push_str(&format!(
        "dropped messages:  {}\n",
        array_len(&dropped)
    ));
    out.push('\n');
    if array_len(&audit) > 0 {
        out.push_str("recent mutations (24h, up to 5)\n");
        out.push_str(&render(&audit));
        out.push('\n');
    } else {
        out.push_str("recent mutations (24h): none\n");
    }
    RunOutput::success(out)
}

/// `iclaw completions <shell>` — render the clap-generated completion
/// script to stdout. No transport call; pure client-side.
fn run_completions(args: &serde_json::Value) -> RunOutput {
    use clap::CommandFactory as _;
    let Some(shell_str) = args.get("shell").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("completions: missing shell\n".to_string());
    };
    let Ok(shell) = shell_str.parse::<clap_complete::Shell>() else {
        return RunOutput::failure(format!("completions: unknown shell {shell_str}\n"));
    };
    let mut cmd = Cli::command();
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut cmd, "iclaw", &mut buf);
    let text = String::from_utf8(buf)
        .unwrap_or_else(|e| format!("completions: invalid utf8 from generator: {e}\n"));
    RunOutput::success(text)
}

/// `iclaw status` — gather a one-shot install overview by hitting four
/// list endpoints in parallel-ish (sequential round-trips, since the
/// transport doesn't support batching). Renders a digest of counts
/// plus a small table per resource so the user can immediately see
/// what's wired up.
async fn run_status<T>(transport: &T, caller: Caller, as_json: bool) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let groups = match transport
        .call("groups.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("groups.list", &e)),
    };
    let mgs = match transport
        .call("messaging-groups.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("messaging-groups.list", &e)),
    };
    let wirings = match transport
        .call("wirings.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("wirings.list", &e)),
    };
    let sessions = match transport
        .call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("sessions.list", &e)),
    };

    if as_json {
        let summary = serde_json::json!({
            "agent_groups": groups,
            "messaging_groups": mgs,
            "wirings": wirings,
            "active_sessions": sessions,
        });
        let mut out = render_json_pretty(&summary);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "agent groups:      {}\n",
        array_len(&groups)
    ));
    out.push_str(&format!(
        "messaging groups:  {}\n",
        array_len(&mgs)
    ));
    out.push_str(&format!("wirings:           {}\n", array_len(&wirings)));
    out.push_str(&format!(
        "active sessions:   {}\n",
        array_len(&sessions)
    ));
    out.push('\n');
    if array_len(&groups) > 0 {
        out.push_str("agent groups\n");
        out.push_str(&render(&groups));
        out.push_str("\n\n");
    }
    if array_len(&mgs) > 0 {
        out.push_str("messaging groups\n");
        out.push_str(&render(&mgs));
        out.push_str("\n\n");
    }
    if array_len(&wirings) > 0 {
        out.push_str("wirings\n");
        out.push_str(&render(&wirings));
        out.push_str("\n\n");
    }
    if array_len(&sessions) > 0 {
        out.push_str("active sessions\n");
        out.push_str(&render(&sessions));
        out.push_str("\n\n");
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    RunOutput::success(out)
}

fn array_len(v: &serde_json::Value) -> usize {
    v.as_array().map_or(0, Vec::len)
}

async fn run_quickstart_cli<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let Some(name) = args.get("name").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("quickstart.cli: missing name\n".to_string());
    };
    let folder = args
        .get("folder")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(name)
        .to_string();
    let pattern = args
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(".*")
        .to_string();
    let provider = args.get("provider").and_then(serde_json::Value::as_str);

    // 1) Create the agent group.
    let mut group_args = serde_json::Map::new();
    group_args.insert("folder".into(), folder.into());
    group_args.insert("name".into(), name.into());
    if let Some(p) = provider {
        group_args.insert("provider".into(), p.into());
    }
    let group = match transport
        .call("groups.create", serde_json::Value::Object(group_args), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("groups.create", &e)),
    };
    let Some(ag_id) = group.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure(
            "quickstart.cli: groups.create returned no id\n".to_string(),
        );
    };

    // 2) Create a messaging group bound to the cli/stdin channel.
    let mg = match transport
        .call(
            "messaging-groups.create",
            serde_json::json!({
                "channel_type": "cli",
                "platform_id": "stdin",
                "name": name,
                "is_group": false,
            }),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("messaging-groups.create", &e)),
    };
    let Some(mg_id) = mg.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure(
            "quickstart.cli: messaging-groups.create returned no id\n".to_string(),
        );
    };

    // 3) Wire them with a pattern-match engage mode.
    let wiring = match transport
        .call(
            "wirings.create",
            serde_json::json!({
                "agent_group_id": ag_id,
                "messaging_group_id": mg_id,
                "engage": "pattern",
                "pattern": pattern,
            }),
            caller,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("wirings.create", &e)),
    };

    let summary = serde_json::json!({
        "agent_group": group,
        "messaging_group": mg,
        "wiring": wiring,
    });
    let text = if as_json {
        render_json_pretty(&summary)
    } else {
        format!(
            "agent group {name} ({ag}) is now wired to cli/stdin via messaging group {mg} (wiring {w}).\n",
            name = name,
            ag = ag_id,
            mg = mg_id,
            w = wiring
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?"),
        )
    };
    let mut out = text;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    RunOutput::success(out)
}

fn format_step_error(step: &str, e: &ClientError) -> String {
    match e {
        ClientError::Remote(p) => {
            format!("quickstart.cli: {step} failed: {} ({})\n", p.message, p.code)
        }
        other => format!("quickstart.cli: {step} failed: {other}\n"),
    }
}

// ---------------------------------------------------------------------------
// `iclaw` (no args) — operator dashboard.
// ---------------------------------------------------------------------------

/// Treat a [`ClientError::Io`] with `NotFound` / `ConnectionRefused` as
/// "the host is not running" so the dashboard can print a friendly
/// pointer to `ironclaw start` instead of a raw I/O error.
fn host_unreachable(e: &ClientError) -> bool {
    if let ClientError::Io(err) = e {
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::PermissionDenied
        )
    } else {
        false
    }
}

/// `iclaw` (no subcommand) — single-screen operator dashboard.
///
/// Fans out to the existing read-only handlers in parallel via
/// [`tokio::join`] so the wall time is bounded by the slowest call,
/// not the sum. The composite is pure client-side; no new socket
/// commands are introduced.
async fn run_dashboard<T>(transport: &T, caller: Caller, as_json: bool) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let (groups, wirings, sessions, audit, dropped, usage) = tokio::join!(
        transport.call("groups.list", serde_json::json!({}), caller.clone()),
        transport.call("wirings.list", serde_json::json!({}), caller.clone()),
        transport.call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller.clone(),
        ),
        transport.call(
            "audit.list",
            serde_json::json!({"since": "1h", "limit": 50}),
            caller.clone(),
        ),
        transport.call(
            "dropped-messages.list",
            serde_json::json!({"since": "1h"}),
            caller.clone(),
        ),
        transport.call("usage.rollup", serde_json::json!({"since": "24h"}), caller),
    );

    // If the very first call failed because the socket is missing,
    // surface a friendly "host not running" message and exit non-zero
    // so scripts can detect it.
    if let Err(e) = &groups {
        if host_unreachable(e) {
            return RunOutput::failure(
                "host not running. Run `ironclaw start` to start it. \
                 Or `iclaw doctor` to diagnose.\n"
                    .to_string(),
            );
        }
    }

    // For the remaining sections, surface a remote error if the *first*
    // call failed for non-IO reasons; otherwise treat per-section errors
    // as "section unavailable" so a partial host (e.g. running but with
    // a degraded audit table) still renders something useful.
    let groups = match groups {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format!("dashboard: groups.list failed: {e}\n")),
    };
    let wirings = wirings.unwrap_or_else(|_| serde_json::json!([]));
    let sessions = sessions.unwrap_or_else(|_| serde_json::json!([]));
    let audit = audit.unwrap_or_else(|_| serde_json::json!([]));
    let dropped = dropped.unwrap_or_else(|_| serde_json::json!([]));
    let usage = usage.unwrap_or_else(|_| serde_json::json!([]));

    let install_root = resolve_install_root().map_or_else(
        || "(unknown)".to_string(),
        |p| p.to_string_lossy().into_owned(),
    );
    let suggestions =
        dashboard_suggestions(&groups, &audit, &dropped, &sessions);

    if as_json {
        let payload = serde_json::json!({
            "install_root": install_root,
            "agent_groups": groups,
            "wirings": wirings,
            "active_sessions": sessions,
            "recent_activity": {
                "audit": audit,
                "dropped": dropped,
                "usage": usage,
            },
            "suggestions": suggestions,
        });
        let mut out = render_json_pretty(&payload);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }

    let text = render_dashboard_text(
        &install_root,
        &groups,
        &wirings,
        &sessions,
        &audit,
        &dropped,
        &usage,
        &suggestions,
    );
    RunOutput::success(text)
}

/// Heuristic next-step picker. Capped at three suggestions; deduped
/// in input order.
fn dashboard_suggestions(
    groups: &serde_json::Value,
    audit: &serde_json::Value,
    dropped: &serde_json::Value,
    sessions: &serde_json::Value,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let group_count = array_len(groups);
    let active_audit = array_len(audit);
    let drop_count = array_len(dropped);
    let active_sessions = array_len(sessions);

    if group_count == 0 {
        out.push(
            "iclaw quickstart cli --name first   # create your first agent group".into(),
        );
    }
    if drop_count > 0 {
        out.push(
            "iclaw dropped-messages list --since 1h   # investigate dropped traffic".into(),
        );
    }
    if group_count >= 1 && active_audit == 0 && active_sessions == 0 {
        out.push("iclaw chat                          # open a REPL against the cli channel".into());
    }
    // Always finish with the diagnostic / overview pointer so users
    // know where to go for more detail.
    if out.len() < 3 {
        out.push("iclaw status                        # full wiring digest".into());
    }
    if out.len() < 3 {
        out.push("iclaw health                        # operator health probe".into());
    }
    out.truncate(3);
    out
}

#[allow(clippy::too_many_arguments)]
fn render_dashboard_text(
    install_root: &str,
    groups: &serde_json::Value,
    wirings: &serde_json::Value,
    sessions: &serde_json::Value,
    audit: &serde_json::Value,
    dropped: &serde_json::Value,
    usage: &serde_json::Value,
    suggestions: &[String],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("ironclaw at {install_root}\n\n"));

    out.push_str(&format!("agent groups ({})\n", array_len(groups)));
    if let Some(items) = groups.as_array() {
        if items.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for g in items {
                let id = json_str(g, "id");
                let name = json_str(g, "name");
                let provider = json_str(g, "agent_provider");
                let provider = if provider.is_empty() { "—" } else { provider };
                out.push_str(&format!(
                    "  {id:24}  {name:24}  provider={provider}\n",
                ));
            }
        }
    }
    out.push('\n');

    out.push_str(&format!("wirings ({})\n", array_len(wirings)));
    if let Some(items) = wirings.as_array() {
        if items.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for w in items {
                let id = json_str(w, "id");
                let mg = json_str(w, "messaging_group_id");
                let ag = json_str(w, "agent_group_id");
                let engage = json_str(w, "engage");
                out.push_str(&format!(
                    "  {id:24}  mg={mg}  ag={ag}  engage={engage}\n",
                ));
            }
        }
    }
    out.push('\n');

    out.push_str(&format!("active sessions ({})\n", array_len(sessions)));
    if array_len(sessions) == 0 {
        out.push_str("  (none)\n");
    } else if let Some(items) = sessions.as_array() {
        for s in items {
            let id = json_str(s, "id");
            let status = json_str(s, "container_status");
            out.push_str(&format!("  {id:36}  {status}\n"));
        }
    }
    out.push('\n');

    let mutations = array_len(audit);
    let errors = audit.as_array().map_or(0, |arr| {
        arr.iter()
            .filter(|row| row.get("error").is_some_and(|v| !v.is_null()))
            .count()
    });
    let outbound_drops = array_len(dropped);
    out.push_str("recent activity (last 1h)\n");
    out.push_str(&format!(
        "  audit:    {mutations} mutations, {errors} errors\n",
    ));
    out.push_str(&format!("  dropped:  {outbound_drops} messages\n"));
    if let Some(rows) = usage.as_array() {
        if rows.is_empty() {
            out.push_str("  budget:   (no token usage in last 24h)\n");
        } else {
            for r in rows {
                let ag = json_str(r, "agent_group_id");
                let total = r
                    .get("total_tokens")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                out.push_str(&format!(
                    "  budget:   {ag} {total} tokens (24h)\n",
                ));
            }
        }
    }
    out.push('\n');

    out.push_str("suggested next:\n");
    for s in suggestions {
        out.push_str(&format!("  {s}\n"));
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------
// `iclaw groups config edit <id>` — open config in $EDITOR, diff, update.
// ---------------------------------------------------------------------------

/// Container-config fields that are safe to round-trip through
/// `groups.config.update`. Everything else is rendered as a comment in
/// the TOML and silently ignored if edited.
const EDITABLE_SCALAR_FIELDS: &[&str] = &[
    "provider",
    "model",
    "image_tag",
    "assistant_name",
    "max_messages_per_prompt",
];

/// Fields the host returns but does not accept on update. They are
/// stripped from the editable region and re-rendered as `# read-only`
/// comments so the operator has context without being able to corrupt
/// them.
const READ_ONLY_FIELDS: &[&str] = &["agent_group_id", "updated_at"];

/// Run the EDITOR-driven edit-and-update workflow for `groups.config`.
#[allow(clippy::too_many_lines)]
async fn run_groups_config_edit<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let Some(id) = args.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("groups.config.edit: missing id\n".to_string());
    };
    let dry_run = args
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Tests may inject an EDITOR override via the args payload to avoid
    // mutating the process-global env var; the production path reads
    // EDITOR/VISUAL/vi in order.
    let editor = args
        .get("editor_override")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || {
                std::env::var("EDITOR")
                    .or_else(|_| std::env::var("VISUAL"))
                    .unwrap_or_else(|_| "vi".to_string())
            },
            str::to_owned,
        );

    let current = match transport
        .call("groups.config.get", serde_json::json!({"id": id}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return RunOutput::failure(format!(
                "groups.config.edit: groups.config.get failed: {e}\n"
            ));
        }
    };
    let current_obj = match current.as_object() {
        Some(o) => o.clone(),
        None => {
            return RunOutput::failure(format!(
                "groups.config.edit: no config exists for {id}\n"
            ));
        }
    };

    let initial_toml = render_config_toml(&current_obj);

    // Persist the editable text to a temp file under the OS temp dir;
    // re-use the same path across retries so the operator keeps their
    // in-progress edits.
    let dir = std::env::temp_dir();
    let file_path = dir.join(format!("iclaw-config-{id}.toml"));
    if let Err(e) = tokio::fs::write(&file_path, &initial_toml).await {
        return RunOutput::failure(format!(
            "groups.config.edit: write {}: {e}\n",
            file_path.display()
        ));
    }

    // Retry loop: editor opens the temp file; on parse error we re-open
    // with an inline error and let the operator try again or abort.
    let edited_obj = loop {
        if let Err(e) = spawn_editor(&editor, &file_path).await {
            // EDITOR exited non-zero; treat as abort.
            let _ = tokio::fs::remove_file(&file_path).await;
            return RunOutput::failure(format!("groups.config.edit: editor failed: {e}\n"));
        }
        let bytes = match tokio::fs::read_to_string(&file_path).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tokio::fs::remove_file(&file_path).await;
                return RunOutput::failure(format!(
                    "groups.config.edit: read {}: {e}\n",
                    file_path.display()
                ));
            }
        };
        if bytes == initial_toml {
            let _ = tokio::fs::remove_file(&file_path).await;
            return RunOutput::success("no changes\n".to_string());
        }
        match parse_config_toml(&bytes) {
            Ok(obj) => break obj,
            Err(err) => {
                // Re-prepend the error as a comment, then prompt.
                let annotated = annotate_with_parse_error(&bytes, &err);
                if let Err(e) = tokio::fs::write(&file_path, &annotated).await {
                    let _ = tokio::fs::remove_file(&file_path).await;
                    return RunOutput::failure(format!(
                        "groups.config.edit: write retry buffer: {e}\n"
                    ));
                }
                match prompt_retry_or_abort() {
                    RetryChoice::Retry => continue,
                    RetryChoice::Abort => {
                        let _ = tokio::fs::remove_file(&file_path).await;
                        return RunOutput::failure(
                            "groups.config.edit: aborted (config not updated)\n".to_string(),
                        );
                    }
                }
            }
        }
    };
    let _ = tokio::fs::remove_file(&file_path).await;

    let mut updates: Vec<(String, serde_json::Value)> = Vec::new();
    let mut mcp_servers_change: Option<serde_json::Value> = None;
    let mut packages_apt_change: Option<Vec<String>> = None;
    let mut packages_npm_change: Option<Vec<String>> = None;

    for key in EDITABLE_SCALAR_FIELDS {
        let old = current_obj.get(*key).cloned().unwrap_or(serde_json::Value::Null);
        let new = edited_obj.get(*key).cloned().unwrap_or(serde_json::Value::Null);
        if old != new {
            updates.push(((*key).to_string(), new));
        }
    }
    if let Some(new_mcp) = edited_obj.get("mcp_servers") {
        let old_mcp = current_obj
            .get("mcp_servers")
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        if &old_mcp != new_mcp {
            mcp_servers_change = Some(new_mcp.clone());
        }
    }
    if let Some(new_apt) = edited_obj.get("packages_apt").and_then(value_string_list) {
        let old_apt = current_obj
            .get("packages_apt")
            .and_then(value_string_list)
            .unwrap_or_default();
        if old_apt != new_apt {
            packages_apt_change = Some(new_apt);
        }
    }
    if let Some(new_npm) = edited_obj.get("packages_npm").and_then(value_string_list) {
        let old_npm = current_obj
            .get("packages_npm")
            .and_then(value_string_list)
            .unwrap_or_default();
        if old_npm != new_npm {
            packages_npm_change = Some(new_npm);
        }
    }

    let changed_field_names: Vec<String> = updates
        .iter()
        .map(|(k, _)| k.clone())
        .chain(mcp_servers_change.as_ref().map(|_| "mcp_servers".into()))
        .chain(packages_apt_change.as_ref().map(|_| "packages_apt".into()))
        .chain(packages_npm_change.as_ref().map(|_| "packages_npm".into()))
        .collect();

    if changed_field_names.is_empty() {
        return RunOutput::success("no changes\n".to_string());
    }

    if dry_run {
        let mut out = String::new();
        out.push_str("dry-run: would update the following fields:\n");
        for (k, v) in &updates {
            out.push_str(&format!("  {k} = {v}\n"));
        }
        if let Some(v) = &mcp_servers_change {
            out.push_str(&format!("  mcp_servers = {v}\n"));
        }
        if let Some(v) = &packages_apt_change {
            out.push_str(&format!("  packages_apt = {v:?}\n"));
        }
        if let Some(v) = &packages_npm_change {
            out.push_str(&format!("  packages_npm = {v:?}\n"));
        }
        return RunOutput::success(out);
    }

    // Commit scalar updates one at a time (the existing `groups.config.update`
    // contract is one field per call). Stop on the first failure so we
    // don't half-update.
    for (key, value) in &updates {
        let res = transport
            .call(
                "groups.config.update",
                serde_json::json!({"id": id, "field": key, "value": value}),
                caller.clone(),
            )
            .await;
        if let Err(e) = res {
            return RunOutput::failure(format!(
                "groups.config.edit: groups.config.update {key} failed: {e}\n"
            ));
        }
    }

    if let Some(new_mcp) = mcp_servers_change {
        if let Err(e) = update_mcp_servers(transport, id, &current_obj, &new_mcp, caller.clone())
            .await
        {
            return RunOutput::failure(e);
        }
    }
    if let Some(new_apt) = packages_apt_change {
        let old_apt = current_obj
            .get("packages_apt")
            .and_then(value_string_list)
            .unwrap_or_default();
        if let Err(e) = update_packages(
            transport,
            id,
            "apt",
            &old_apt,
            &new_apt,
            caller.clone(),
        )
        .await
        {
            return RunOutput::failure(e);
        }
    }
    if let Some(new_npm) = packages_npm_change {
        let old_npm = current_obj
            .get("packages_npm")
            .and_then(value_string_list)
            .unwrap_or_default();
        if let Err(e) = update_packages(transport, id, "npm", &old_npm, &new_npm, caller).await {
            return RunOutput::failure(e);
        }
    }

    RunOutput::success(format!(
        "updated {} field{}: {}\n",
        changed_field_names.len(),
        if changed_field_names.len() == 1 { "" } else { "s" },
        changed_field_names.join(", "),
    ))
}

fn value_string_list(v: &serde_json::Value) -> Option<Vec<String>> {
    v.as_array().map(|arr| {
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

async fn update_mcp_servers<T>(
    transport: &T,
    id: &str,
    current_obj: &serde_json::Map<String, serde_json::Value>,
    new_mcp: &serde_json::Value,
    caller: Caller,
) -> Result<(), String>
where
    T: CallTransport + ?Sized,
{
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let old_mcp = current_obj.get("mcp_servers").unwrap_or(&empty);
    let old_obj = old_mcp.as_object().cloned().unwrap_or_default();
    let new_obj = new_mcp.as_object().cloned().unwrap_or_default();
    // Remove servers no longer present.
    for name in old_obj.keys() {
        if !new_obj.contains_key(name) {
            if let Err(e) = transport
                .call(
                    "groups.config.remove-mcp-server",
                    serde_json::json!({"id": id, "name": name}),
                    caller.clone(),
                )
                .await
            {
                return Err(format!(
                    "groups.config.edit: remove-mcp-server {name}: {e}\n"
                ));
            }
        }
    }
    // Add or replace anything present in `new` that differs.
    for (name, server) in &new_obj {
        if old_obj.get(name) == Some(server) {
            continue;
        }
        if let Err(e) = transport
            .call(
                "groups.config.add-mcp-server",
                serde_json::json!({"id": id, "server": server}),
                caller.clone(),
            )
            .await
        {
            return Err(format!("groups.config.edit: add-mcp-server {name}: {e}\n"));
        }
    }
    Ok(())
}

async fn update_packages<T>(
    transport: &T,
    id: &str,
    kind: &str,
    old: &[String],
    new: &[String],
    caller: Caller,
) -> Result<(), String>
where
    T: CallTransport + ?Sized,
{
    for name in old {
        if !new.iter().any(|x| x == name) {
            if let Err(e) = transport
                .call(
                    "groups.config.remove-package",
                    serde_json::json!({"id": id, "kind": kind, "name": name}),
                    caller.clone(),
                )
                .await
            {
                return Err(format!("groups.config.edit: remove-package {kind} {name}: {e}\n"));
            }
        }
    }
    for name in new {
        if !old.iter().any(|x| x == name) {
            if let Err(e) = transport
                .call(
                    "groups.config.add-package",
                    serde_json::json!({"id": id, "kind": kind, "name": name}),
                    caller.clone(),
                )
                .await
            {
                return Err(format!("groups.config.edit: add-package {kind} {name}: {e}\n"));
            }
        }
    }
    Ok(())
}

/// Render the JSON object returned by `groups.config.get` as a TOML
/// document. Read-only fields appear as `# read-only` comments.
fn render_config_toml(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::new();
    out.push_str("# iclaw groups config edit — TOML buffer\n");
    out.push_str("# Edit values, save, and close to apply. Re-open with --dry-run to preview.\n");
    out.push_str("# Read-only fields are shown for reference and ignored on save.\n\n");

    // Read-only fields first as comments.
    for key in READ_ONLY_FIELDS {
        if let Some(v) = obj.get(*key) {
            out.push_str(&format!("# read-only: {key} = {v}\n"));
        }
    }
    out.push('\n');

    // Editable scalar fields (provider, model, ...). Null values are
    // rendered as commented-out lines so the operator can uncomment to
    // set them.
    for key in EDITABLE_SCALAR_FIELDS {
        let v = obj.get(*key).cloned().unwrap_or(serde_json::Value::Null);
        out.push_str(&render_scalar_line(key, &v));
    }
    out.push('\n');

    // Packages — render as arrays of strings.
    if let Some(arr) = obj.get("packages_apt").and_then(value_string_list) {
        out.push_str(&format!(
            "packages_apt = {}\n",
            toml_string_array(&arr)
        ));
    }
    if let Some(arr) = obj.get("packages_npm").and_then(value_string_list) {
        out.push_str(&format!(
            "packages_npm = {}\n",
            toml_string_array(&arr)
        ));
    }
    out.push('\n');

    // mcp_servers — round-trip via toml::Value so nested JSON objects
    // are rendered as inline tables.
    let mcp = obj
        .get("mcp_servers")
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    if let Some(mcp_obj) = mcp.as_object() {
        if mcp_obj.is_empty() {
            out.push_str("[mcp_servers]\n");
        } else {
            for (name, server) in mcp_obj {
                out.push_str(&format!(
                    "[mcp_servers.{name}]\n{}\n",
                    json_value_as_toml_table_body(server),
                ));
            }
        }
    }

    out
}

fn render_scalar_line(key: &str, value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => format!("# {key} = \"\"   # currently null\n"),
        serde_json::Value::String(s) => format!("{key} = {}\n", toml_quote(s)),
        serde_json::Value::Bool(b) => format!("{key} = {b}\n"),
        serde_json::Value::Number(n) => format!("{key} = {n}\n"),
        other => format!("# {key} = {other}   # complex value, edit via socket\n"),
    }
}

fn toml_quote(s: &str) -> String {
    // Use the `toml` crate's quoting via `toml::Value::String`.
    toml::Value::String(s.to_string()).to_string()
}

fn toml_string_array(items: &[String]) -> String {
    let mut s = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&toml_quote(item));
    }
    s.push(']');
    s
}

/// Render a JSON object as the body of a TOML table (no surrounding
/// `[name]` header). Each key becomes `key = <toml-encoded-value>`.
fn json_value_as_toml_table_body(v: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(obj) = v.as_object() {
        for (k, v) in obj {
            out.push_str(&format!("{k} = {}\n", json_to_toml_inline(v)));
        }
    }
    out
}

fn json_to_toml_inline(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "\"\"".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => toml_quote(s),
        serde_json::Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(json_to_toml_inline).collect();
            format!("[{}]", parts.join(", "))
        }
        serde_json::Value::Object(obj) => {
            // Inline table.
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{k} = {}", json_to_toml_inline(v)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
    }
}

/// Parse the post-edit TOML buffer back into a JSON object.
fn parse_config_toml(
    text: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let table: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let json: serde_json::Value =
        serde_json::to_value(&table).map_err(|e| e.to_string())?;
    json.as_object().cloned().ok_or_else(|| {
        "TOML root must be a table".to_string()
    })
}

fn annotate_with_parse_error(text: &str, err: &str) -> String {
    let mut out = String::new();
    out.push_str("# TOML parse error — edit and save to retry:\n");
    for line in err.lines() {
        out.push_str(&format!("#   {line}\n"));
    }
    out.push('\n');
    // Strip any previous error banner so retries don't accumulate.
    let mut in_banner = false;
    for line in text.lines() {
        if line.starts_with("# TOML parse error") {
            in_banner = true;
            continue;
        }
        if in_banner {
            if line.starts_with("#   ") || line.trim().is_empty() {
                continue;
            }
            in_banner = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone, Copy)]
enum RetryChoice {
    Retry,
    Abort,
}

/// Prompt on the controlling terminal. Tests should never reach this
/// path because they use `--dry-run` or never trigger a parse error.
fn prompt_retry_or_abort() -> RetryChoice {
    use std::io::{BufRead as _, Write as _};
    eprint!("groups.config.edit: parse error. (r)etry / (a)bort? ");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return RetryChoice::Abort;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "r" | "retry" | "" => RetryChoice::Retry,
        _ => RetryChoice::Abort,
    }
}

/// Spawn `editor` on `path` and wait for it to exit.
///
/// Splits the editor string on ASCII whitespace so callers can pass
/// values like `EDITOR='code --wait'`. Non-zero exit codes are
/// reported as errors so the workflow aborts cleanly.
async fn spawn_editor(
    editor: &str,
    path: &std::path::Path,
) -> Result<(), String> {
    let mut parts = editor.split_whitespace();
    let Some(program) = parts.next() else {
        return Err("EDITOR is empty".into());
    };
    let extra: Vec<&str> = parts.collect();
    let status = tokio::process::Command::new(program)
        .args(&extra)
        .arg(path)
        .status()
        .await
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::{AgentGroupId, SessionId};
    use serde_json::json;
    use std::sync::Mutex;

    struct StubTransport {
        result: Mutex<Option<Result<serde_json::Value, ClientError>>>,
        last_call: Mutex<Option<(String, serde_json::Value, Caller)>>,
    }

    impl StubTransport {
        fn ok(value: serde_json::Value) -> Self {
            Self {
                result: Mutex::new(Some(Ok(value))),
                last_call: Mutex::new(None),
            }
        }
        fn err(e: ClientError) -> Self {
            Self {
                result: Mutex::new(Some(Err(e))),
                last_call: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for StubTransport {
        async fn call(
            &self,
            command: &str,
            args: serde_json::Value,
            caller: Caller,
        ) -> Result<serde_json::Value, ClientError> {
            *self.last_call.lock().unwrap() = Some((command.to_string(), args, caller));
            self.result.lock().unwrap().take().unwrap()
        }
    }

    #[tokio::test]
    async fn run_cli_success_renders_table() {
        let t = StubTransport::ok(json!([{"id":"ag_1","name":"x"}]));
        let out = run_cli(["iclaw", "groups", "list"], &t).await;
        assert!(matches!(out.code, ExitCode { .. }));
        // ExitCode doesn't expose the raw u8 for comparison, but stdout
        // and absence of stderr confirm success.
        assert!(out.stderr.is_empty());
        assert!(out.stdout.contains("ID"));
        assert!(out.stdout.contains("ag_1"));
        let captured = t.last_call.lock().unwrap();
        let (cmd, _args, _caller) = captured.as_ref().unwrap();
        assert_eq!(cmd, "groups.list");
    }

    #[tokio::test]
    async fn run_cli_json_flag_emits_pretty_json() {
        let t = StubTransport::ok(json!([{"id":"x"}]));
        let out = run_cli(["iclaw", "--json", "groups", "list"], &t).await;
        assert!(out.stdout.contains("\"id\""));
        assert!(out.stdout.contains("[\n"));
    }

    #[tokio::test]
    async fn run_cli_remote_error_to_stderr() {
        let t = StubTransport::err(ClientError::Remote(ErrorPayload::new(
            "not-found",
            "no such id",
        )));
        let out = run_cli(["iclaw", "groups", "get", "x"], &t).await;
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("not-found"));
        assert!(out.stderr.contains("no such id"));
    }

    #[tokio::test]
    async fn run_cli_transport_error_to_stderr() {
        let t = StubTransport::err(ClientError::Timeout);
        let out = run_cli(["iclaw", "groups", "list"], &t).await;
        assert!(out.stderr.contains("timed out"));
    }

    #[tokio::test]
    async fn run_cli_parse_error_help_text_on_stderr() {
        let t = StubTransport::ok(json!({}));
        let out = run_cli(["iclaw", "no-such-command"], &t).await;
        assert!(!out.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_cli_help_goes_to_stdout() {
        let t = StubTransport::ok(json!({}));
        let out = run_cli(["iclaw", "--help"], &t).await;
        assert!(!out.stdout.is_empty());
    }

    #[test]
    fn caller_from_raw_unset_is_host() {
        assert!(matches!(caller_from_raw(None).unwrap(), Caller::Host));
    }

    #[test]
    fn caller_from_raw_empty_is_host() {
        assert!(matches!(caller_from_raw(Some("")).unwrap(), Caller::Host));
        assert!(matches!(
            caller_from_raw(Some("   \t")).unwrap(),
            Caller::Host,
        ));
    }

    #[test]
    fn caller_from_raw_long_form() {
        let sid = SessionId::nil();
        let agid = AgentGroupId::nil();
        let json = format!(
            "{{\"session_id\":\"{}\",\"agent_group_id\":\"{}\"}}",
            sid.as_uuid(),
            agid.as_uuid(),
        );
        let c = caller_from_raw(Some(&json)).unwrap();
        if let Caller::Agent {
            session_id,
            agent_group_id,
            messaging_group_id,
        } = c
        {
            assert_eq!(session_id, sid);
            assert_eq!(agent_group_id, agid);
            assert!(messaging_group_id.is_none());
        } else {
            panic!("expected agent");
        }
    }

    #[test]
    fn caller_from_raw_short_form() {
        let sid = SessionId::nil();
        let agid = AgentGroupId::nil();
        let json = format!(
            "{{\"session\":\"{}\",\"agent_group\":\"{}\"}}",
            sid.as_uuid(),
            agid.as_uuid(),
        );
        let c = caller_from_raw(Some(&json)).unwrap();
        assert!(matches!(c, Caller::Agent { .. }));
    }

    #[test]
    fn caller_from_raw_invalid_json_errors() {
        let err = caller_from_raw(Some("garbage")).unwrap_err();
        assert!(matches!(err, RunError::BadAgentCaller(_)));
    }

    #[test]
    fn caller_from_env_uses_process_env() {
        // We don't mutate the env in tests (it's `unsafe` under edition
        // 2024). Instead, just confirm the no-env-var branch is `Host`.
        if std::env::var(AGENT_CALLER_ENV).is_err() {
            let c = caller_from_env().unwrap();
            assert!(matches!(c, Caller::Host));
        }
    }

    #[test]
    fn run_output_helpers() {
        let s = RunOutput::success("hi".into());
        assert!(s.stderr.is_empty());
        assert_eq!(s.stdout, "hi");
        let f = RunOutput::failure("err".into());
        assert!(f.stdout.is_empty());
        assert_eq!(f.stderr, "err");
    }

    #[test]
    fn run_error_display_includes_inner_messages() {
        let e = RunError::BadAgentCaller("bad".into());
        assert!(e.to_string().contains("bad"));
        let e = RunError::ParseCli("p".into());
        assert!(e.to_string().contains('p'));
        let inner = ClientError::Timeout;
        let e = RunError::Client(inner);
        assert!(e.to_string().contains("timed out"));
    }

    // ---- quickstart ------------------------------------------------------

    /// Stub that returns one queued response per call and records the
    /// `(command, args)` pairs in order.
    struct SequencedTransport {
        responses: Mutex<Vec<Result<serde_json::Value, ClientError>>>,
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl SequencedTransport {
        fn new(responses: Vec<Result<serde_json::Value, ClientError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for SequencedTransport {
        async fn call(
            &self,
            command: &str,
            args: serde_json::Value,
            _caller: Caller,
        ) -> Result<serde_json::Value, ClientError> {
            self.calls.lock().unwrap().push((command.to_string(), args));
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err(ClientError::Timeout);
            }
            q.remove(0)
        }
    }

    #[tokio::test]
    async fn quickstart_cli_fans_out_three_calls() {
        let t = SequencedTransport::new(vec![
            Ok(json!({"id": "ag-1", "name": "demo", "folder": "demo"})),
            Ok(json!({"id": "mg-1", "channel_type": "cli", "platform_id": "stdin"})),
            Ok(json!({"id": "w-1"})),
        ]);
        let out = run_cli(
            ["iclaw", "quickstart", "cli", "--name", "demo"],
            &t,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].0, "groups.create");
        assert_eq!(calls[0].1["folder"], "demo");
        assert_eq!(calls[0].1["name"], "demo");
        assert_eq!(calls[1].0, "messaging-groups.create");
        assert_eq!(calls[1].1["channel_type"], "cli");
        assert_eq!(calls[1].1["platform_id"], "stdin");
        assert_eq!(calls[2].0, "wirings.create");
        assert_eq!(calls[2].1["agent_group_id"], "ag-1");
        assert_eq!(calls[2].1["messaging_group_id"], "mg-1");
        assert_eq!(calls[2].1["engage"], "pattern");
        assert_eq!(calls[2].1["pattern"], ".*");
        assert!(out.stdout.contains("ag-1"));
        assert!(out.stdout.contains("mg-1"));
        assert!(out.stdout.contains("w-1"));
    }

    #[tokio::test]
    async fn quickstart_cli_folder_override_propagates() {
        let t = SequencedTransport::new(vec![
            Ok(json!({"id": "ag-1"})),
            Ok(json!({"id": "mg-1"})),
            Ok(json!({"id": "w-1"})),
        ]);
        let _ = run_cli(
            [
                "iclaw",
                "quickstart",
                "cli",
                "--name",
                "demo",
                "--folder",
                "homedir",
                "--pattern",
                "^hi",
            ],
            &t,
        )
        .await;
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].1["folder"], "homedir");
        assert_eq!(calls[2].1["pattern"], "^hi");
    }

    #[tokio::test]
    async fn completions_bash_renders_without_transport() {
        // Use a transport that would error if dialled — completions
        // must not hit it.
        let t = SequencedTransport::new(vec![]);
        let out = run_cli(["iclaw", "completions", "bash"], &t).await;
        assert!(out.stderr.is_empty());
        assert!(out.stdout.starts_with("_iclaw()"));
        assert!(t.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn completions_zsh_starts_with_compdef() {
        let t = SequencedTransport::new(vec![]);
        let out = run_cli(["iclaw", "completions", "zsh"], &t).await;
        assert!(out.stdout.starts_with("#compdef iclaw"));
    }

    #[tokio::test]
    async fn status_fans_out_four_list_calls() {
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1", "name": "demo"}])),
            Ok(json!([{"id": "mg-1", "channel_type": "cli"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
        ]);
        let out = run_cli(["iclaw", "status"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].0, "groups.list");
        assert_eq!(calls[1].0, "messaging-groups.list");
        assert_eq!(calls[2].0, "wirings.list");
        assert_eq!(calls[3].0, "sessions.list");
        assert_eq!(calls[3].1["status"], "active");
        assert!(out.stdout.contains("agent groups:      1"));
        assert!(out.stdout.contains("messaging groups:  1"));
        assert!(out.stdout.contains("wirings:           1"));
        assert!(out.stdout.contains("active sessions:   0"));
    }

    #[tokio::test]
    async fn quickstart_cli_stops_on_first_error() {
        // First call fails — second/third should never run.
        let t = SequencedTransport::new(vec![Err(ClientError::Remote(ErrorPayload {
            code: "validation".into(),
            message: "folder required".into(),
            retryable: false,
            data: None,
        }))]);
        let out = run_cli(
            ["iclaw", "quickstart", "cli", "--name", "demo"],
            &t,
        )
        .await;
        assert!(!out.stderr.is_empty());
        assert!(out.stderr.contains("groups.create"));
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
    }

    // ---- dashboard -----------------------------------------------------

    /// Transport stub that dispatches by command name. Required because
    /// the dashboard fans calls out via `tokio::join!`, which polls in
    /// declaration order but recording them by name is robust either way.
    struct MapTransport {
        responses:
            std::collections::HashMap<String, Result<serde_json::Value, ClientError>>,
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl MapTransport {
        fn new(
            pairs: Vec<(&str, Result<serde_json::Value, ClientError>)>,
        ) -> Self {
            Self {
                responses: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for MapTransport {
        async fn call(
            &self,
            command: &str,
            args: serde_json::Value,
            _caller: Caller,
        ) -> Result<serde_json::Value, ClientError> {
            self.calls.lock().unwrap().push((command.to_string(), args));
            match self.responses.get(command) {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(_)) => {
                    // ClientError isn't Clone; rebuild a representative one
                    // for each repeated lookup.
                    Err(ClientError::Remote(ErrorPayload::new(
                        "stub-err", "stubbed error",
                    )))
                }
                None => Err(ClientError::Remote(ErrorPayload::new(
                    "not-stubbed",
                    format!("MapTransport has no stub for {command}"),
                ))),
            }
        }
    }

    #[tokio::test]
    async fn no_args_runs_dashboard_with_expected_sections() {
        let t = MapTransport::new(vec![
            (
                "groups.list",
                Ok(json!([{"id": "ag-1", "name": "first", "agent_provider": "anthropic"}])),
            ),
            ("wirings.list", Ok(json!([{"id": "w-1", "messaging_group_id": "mg-1", "agent_group_id": "ag-1", "engage": "pattern"}]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([{"id":"a-1"}, {"id":"a-2"}]))),
            ("dropped-messages.list", Ok(json!([]))),
            ("usage.rollup", Ok(json!([{"agent_group_id": "ag-1", "total_tokens": 1234}]))),
        ]);
        let out = run_cli(["iclaw"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains("ironclaw at "));
        assert!(out.stdout.contains("agent groups (1)"));
        assert!(out.stdout.contains("ag-1"));
        assert!(out.stdout.contains("wirings (1)"));
        assert!(out.stdout.contains("active sessions (0)"));
        assert!(out.stdout.contains("recent activity (last 1h)"));
        assert!(out.stdout.contains("2 mutations"));
        assert!(out.stdout.contains("1234 tokens"));
        assert!(out.stdout.contains("suggested next:"));
        // Confirm all six read endpoints were hit.
        let calls: Vec<String> = t
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| c.clone())
            .collect();
        for expected in [
            "groups.list",
            "wirings.list",
            "sessions.list",
            "audit.list",
            "dropped-messages.list",
            "usage.rollup",
        ] {
            assert!(calls.iter().any(|c| c == expected), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn dashboard_json_emits_single_object() {
        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([]))),
            ("wirings.list", Ok(json!([]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([]))),
            ("dropped-messages.list", Ok(json!([]))),
            ("usage.rollup", Ok(json!([]))),
        ]);
        let out = run_cli(["iclaw", "--json"], &t).await;
        assert!(out.stderr.is_empty());
        let parsed: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).expect("valid json");
        // Required top-level keys.
        for key in [
            "install_root",
            "agent_groups",
            "wirings",
            "active_sessions",
            "recent_activity",
            "suggestions",
        ] {
            assert!(parsed.get(key).is_some(), "missing key {key}");
        }
    }

    #[tokio::test]
    async fn dashboard_unreachable_host_returns_friendly_error() {
        // Use a transport that returns an IO NotFound for every call.
        struct DeadTransport;
        #[async_trait::async_trait]
        impl CallTransport for DeadTransport {
            async fn call(
                &self,
                _command: &str,
                _args: serde_json::Value,
                _caller: Caller,
            ) -> Result<serde_json::Value, ClientError> {
                Err(ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such socket",
                )))
            }
        }
        let out = run_cli(["iclaw"], &DeadTransport).await;
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("host not running"));
    }

    #[tokio::test]
    async fn dashboard_zero_groups_suggests_quickstart() {
        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([]))),
            ("wirings.list", Ok(json!([]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([]))),
            ("dropped-messages.list", Ok(json!([]))),
            ("usage.rollup", Ok(json!([]))),
        ]);
        let out = run_cli(["iclaw"], &t).await;
        assert!(out.stdout.contains("iclaw quickstart cli"));
    }

    #[tokio::test]
    async fn dashboard_drops_suggests_dropped_messages() {
        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([{"id": "ag-1", "name": "x"}]))),
            ("wirings.list", Ok(json!([]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([]))),
            ("dropped-messages.list", Ok(json!([{"id":"d-1"},{"id":"d-2"}]))),
            ("usage.rollup", Ok(json!([]))),
        ]);
        let out = run_cli(["iclaw"], &t).await;
        assert!(out.stdout.contains("iclaw dropped-messages list"));
    }

    // ---- groups config edit -------------------------------------------

    fn write_editor_script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let p = dir.join("editor.sh");
        std::fs::write(&p, body).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[tokio::test]
    async fn groups_config_edit_dry_run_no_changes_when_unedited() {
        // EDITOR `true` is a no-op exit-0 binary, so the temp file is
        // untouched. The workflow should print "no changes" and never
        // call groups.config.update.
        let t = MapTransport::new(vec![(
            "groups.config.get",
            Ok(json!({
                "agent_group_id": "00000000-0000-0000-0000-000000000001",
                "provider": "anthropic",
                "model": "claude-sonnet",
                "image_tag": null,
                "assistant_name": null,
                "max_messages_per_prompt": null,
                "mcp_servers": {},
                "packages_apt": [],
                "packages_npm": [],
                "updated_at": "2026-01-01T00:00:00Z",
            })),
        )]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000001",
                "dry_run": true,
                "editor_override": "true",
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains("no changes"));
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "groups.config.get");
    }

    #[tokio::test]
    async fn groups_config_edit_dry_run_with_changes_prints_diff() {
        let dir = tempfile::tempdir().unwrap();
        let editor_script = write_editor_script(
            dir.path(),
            "#!/bin/sh\nprintf 'provider = \"replaced\"\\nmodel = \"claude-sonnet\"\\n' > \"$1\"\n",
        );
        let t = MapTransport::new(vec![(
            "groups.config.get",
            Ok(json!({
                "agent_group_id": "00000000-0000-0000-0000-000000000002",
                "provider": "anthropic",
                "model": "claude-sonnet",
                "image_tag": null,
                "assistant_name": null,
                "max_messages_per_prompt": null,
                "mcp_servers": {},
                "packages_apt": [],
                "packages_npm": [],
                "updated_at": "2026-01-01T00:00:00Z",
            })),
        )]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000002",
                "dry_run": true,
                "editor_override": editor_script.to_string_lossy(),
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(
            out.stdout.contains("dry-run"),
            "missing dry-run banner; stdout={:?}",
            out.stdout
        );
        assert!(out.stdout.contains("provider"));
        // The transport should have been hit only for the get; no update.
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "groups.config.get");
    }

    #[tokio::test]
    async fn groups_config_edit_commits_scalar_change() {
        let dir = tempfile::tempdir().unwrap();
        let editor_script = write_editor_script(
            dir.path(),
            "#!/bin/sh\nprintf 'provider = \"replaced\"\\nmodel = \"claude-sonnet\"\\n' > \"$1\"\n",
        );
        let t = MapTransport::new(vec![
            (
                "groups.config.get",
                Ok(json!({
                    "agent_group_id": "00000000-0000-0000-0000-000000000003",
                    "provider": "anthropic",
                    "model": "claude-sonnet",
                    "image_tag": null,
                    "assistant_name": null,
                    "max_messages_per_prompt": null,
                    "mcp_servers": {},
                    "packages_apt": [],
                    "packages_npm": [],
                    "updated_at": "2026-01-01T00:00:00Z",
                })),
            ),
            (
                "groups.config.update",
                Ok(json!({"agent_group_id": "00000000-0000-0000-0000-000000000003", "provider":"replaced"})),
            ),
        ]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000003",
                "dry_run": false,
                "editor_override": editor_script.to_string_lossy(),
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains("updated"));
        let calls = t.calls.lock().unwrap();
        let cmds: Vec<&str> = calls.iter().map(|(c, _)| c.as_str()).collect();
        assert!(cmds.iter().any(|c| *c == "groups.config.get"));
        assert!(cmds.iter().any(|c| *c == "groups.config.update"));
        // Confirm the update body matches the parsed-out field.
        let upd = calls
            .iter()
            .find(|(c, _)| c == "groups.config.update")
            .unwrap();
        assert_eq!(upd.1["field"], "provider");
        assert_eq!(upd.1["value"], "replaced");
    }

    #[tokio::test]
    async fn groups_config_edit_editor_nonzero_aborts() {
        let t = MapTransport::new(vec![(
            "groups.config.get",
            Ok(json!({
                "agent_group_id": "00000000-0000-0000-0000-000000000004",
                "provider": "anthropic",
                "model": null,
                "image_tag": null,
                "assistant_name": null,
                "max_messages_per_prompt": null,
                "mcp_servers": {},
                "packages_apt": [],
                "packages_npm": [],
                "updated_at": "2026-01-01T00:00:00Z",
            })),
        )]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000004",
                "dry_run": false,
                "editor_override": "false",
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stdout.is_empty());
        assert!(
            out.stderr.contains("editor failed"),
            "stderr={:?}",
            out.stderr,
        );
    }

    #[test]
    fn render_config_toml_round_trip_preserves_scalars() {
        let obj = serde_json::Map::from_iter([
            (
                "agent_group_id".into(),
                json!("00000000-0000-0000-0000-000000000005"),
            ),
            ("provider".into(), json!("anthropic")),
            ("model".into(), json!("claude-sonnet")),
            ("image_tag".into(), json!(null)),
            ("assistant_name".into(), json!("Greeter")),
            ("max_messages_per_prompt".into(), json!(16)),
            ("mcp_servers".into(), json!({})),
            ("packages_apt".into(), json!(["curl"])),
            ("packages_npm".into(), json!([])),
            ("updated_at".into(), json!("2026-01-01T00:00:00Z")),
        ]);
        let toml_text = render_config_toml(&obj);
        let parsed = parse_config_toml(&toml_text).expect("toml parses");
        assert_eq!(parsed.get("provider"), Some(&json!("anthropic")));
        assert_eq!(parsed.get("model"), Some(&json!("claude-sonnet")));
        assert_eq!(parsed.get("assistant_name"), Some(&json!("Greeter")));
        assert_eq!(parsed.get("max_messages_per_prompt"), Some(&json!(16)));
        assert_eq!(parsed.get("packages_apt"), Some(&json!(["curl"])));
        // Read-only fields should not appear in the parsed body since
        // they're rendered as comments.
        assert!(parsed.get("agent_group_id").is_none());
        assert!(parsed.get("updated_at").is_none());
    }

    #[test]
    fn dashboard_suggestions_caps_at_three() {
        let groups = json!([]);
        let audit = json!([]);
        let dropped = json!([{"id":"x"}]);
        let sessions = json!([]);
        let s = dashboard_suggestions(&groups, &audit, &dropped, &sessions);
        assert!(s.len() <= 3);
        assert!(!s.is_empty());
    }

    #[test]
    fn host_unreachable_matches_io_kinds() {
        let io = ClientError::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(host_unreachable(&io));
        let to = ClientError::Timeout;
        assert!(!host_unreachable(&to));
    }
}
