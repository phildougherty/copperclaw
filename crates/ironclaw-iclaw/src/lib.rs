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
        "doctor" => run_doctor(args, transport, caller, as_json).await,
        "completions" => run_completions(args),
        "chat" => run_chat(args).await,
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

/// Severity of a single `doctor` check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckLevel {
    Ok,
    Warn,
    Fail,
}

impl CheckLevel {
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "OK   ",
            Self::Warn => "WARN ",
            Self::Fail => "FAIL ",
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One result from `iclaw doctor`. `fix` is shown to the operator
/// only when level != Ok, so an all-green report stays short.
#[derive(Debug, Clone)]
struct Check {
    name: &'static str,
    level: CheckLevel,
    detail: String,
    /// Optional shell command the operator can run to remediate.
    fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, level: CheckLevel::Ok, detail: detail.into(), fix: None }
    }
    fn warn(name: &'static str, detail: impl Into<String>, fix: Option<&str>) -> Self {
        Self {
            name,
            level: CheckLevel::Warn,
            detail: detail.into(),
            fix: fix.map(String::from),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, fix: Option<&str>) -> Self {
        Self {
            name,
            level: CheckLevel::Fail,
            detail: detail.into(),
            fix: fix.map(String::from),
        }
    }
    fn to_json(&self) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        o.insert("name".into(), serde_json::Value::String(self.name.into()));
        o.insert(
            "level".into(),
            serde_json::Value::String(self.level.as_str().into()),
        );
        o.insert("detail".into(), serde_json::Value::String(self.detail.clone()));
        if let Some(f) = &self.fix {
            o.insert("fix".into(), serde_json::Value::String(f.clone()));
        }
        serde_json::Value::Object(o)
    }
}

/// `iclaw doctor` — composite first-run diagnostic. Walks the install
/// end-to-end and reports per-check status plus a `fix:` line on each
/// non-OK row so an operator can copy-paste their way to a working
/// install.
///
/// Sequence:
/// 1. `groups.list` — central DB reachable through the host socket.
///    Subsumes "host process running" + "socket file present" +
///    "central DB readable".
/// 2. Group / wiring counts — gating for `iclaw chat` to do
///    anything useful.
/// 3. Recent audit errors — surfacing flapping mutations.
/// 4. Dropped-message backlog.
/// 5. Env-var sanity — `ANTHROPIC_API_KEY` present, plus a courtesy
///    note about which `web_search` providers are wired up.
///
/// The transport calls run sequentially because the iclaw socket is
/// strictly single-flight per connection; ~5 round-trips total against
/// a Unix socket is sub-millisecond in practice.
#[allow(clippy::too_many_lines)] // Sequential checks are easier to follow inline.
async fn run_doctor<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let _no_ping = args.get("no_ping").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let mut checks: Vec<Check> = Vec::new();

    // 1. Host reachable + central DB readable.
    let groups = match transport
        .call("groups.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => {
            checks.push(Check::ok(
                "host-reachable",
                "iclaw socket responded; central DB is readable",
            ));
            v
        }
        Err(e) => {
            checks.push(Check::fail(
                "host-reachable",
                format!("could not reach the host socket: {e}"),
                Some("start the host: `ironclaw run` (or `systemctl start ironclaw` if installed as a service)"),
            ));
            return finalise_doctor(&checks, as_json);
        }
    };

    // 2. At least one agent group must exist for any of the messaging
    //    paths to fire.
    let group_count = array_len(&groups);
    if group_count == 0 {
        checks.push(Check::fail(
            "agent-group",
            "no agent groups configured — there is no one for inbound messages to route to",
            Some("create the default group: `iclaw quickstart cli --name first`"),
        ));
    } else {
        checks.push(Check::ok(
            "agent-group",
            format!("{group_count} group(s) configured"),
        ));
    }

    // 3. Wirings — messaging-group ↔ agent-group bindings.
    let wirings = transport
        .call("wirings.list", serde_json::json!({}), caller.clone())
        .await
        .ok();
    match wirings.as_ref().map(array_len) {
        Some(0) => checks.push(Check::warn(
            "wiring",
            "no messaging-group wirings — agent groups exist but nothing routes inbound to them",
            Some("`iclaw quickstart cli --name first` creates the default group + wiring in one call"),
        )),
        Some(n) => checks.push(Check::ok("wiring", format!("{n} wiring(s) configured"))),
        None => checks.push(Check::warn(
            "wiring",
            "could not list wirings",
            Some("re-run `iclaw doctor` after the host comes back; check stderr for `wirings.list` errors"),
        )),
    }

    // 4. Sessions snapshot (informational, never fatal — having zero
    //    sessions is the steady state for a brand-new install).
    if let Ok(active) = transport
        .call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller.clone(),
        )
        .await
    {
        let n = array_len(&active);
        if n > 0 {
            checks.push(Check::ok("sessions", format!("{n} active session(s)")));
        } else {
            checks.push(Check::ok(
                "sessions",
                "no active sessions yet — send a message via `iclaw chat` to create one",
            ));
        }
    }

    // 5. Recent audit failures — anything that flapped in the last
    //    hour points the operator at the broken command.
    if let Ok(audit) = transport
        .call(
            "audit.list",
            serde_json::json!({"since": "1h", "limit": 50}),
            caller.clone(),
        )
        .await
    {
        let bad: Vec<&serde_json::Value> = audit
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|r| {
                        r.get("result").and_then(serde_json::Value::as_str)
                            == Some("error")
                    })
                    .collect()
            })
            .unwrap_or_default();
        if bad.is_empty() {
            checks.push(Check::ok("audit-errors", "no failed mutations in the last hour"));
        } else {
            let sample: Vec<String> = bad
                .iter()
                .take(3)
                .map(|r| {
                    let cmd = r
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let code = r
                        .get("error_code")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    format!("{cmd} -> {code}")
                })
                .collect();
            checks.push(Check::warn(
                "audit-errors",
                format!("{} failed mutation(s) in the last hour: {}", bad.len(), sample.join(", ")),
                Some("`iclaw audit list --since 1h` to see the full set"),
            ));
        }
    }

    // 6. Dropped-message backlog.
    if let Ok(dropped) = transport
        .call("dropped-messages.list", serde_json::json!({}), caller.clone())
        .await
    {
        let n = array_len(&dropped);
        if n == 0 {
            checks.push(Check::ok("dropped-messages", "no dropped inbound messages"));
        } else {
            checks.push(Check::warn(
                "dropped-messages",
                format!("{n} dropped inbound message(s)"),
                Some("`iclaw dropped-messages list` to inspect; usually means a sender hit the approval gate"),
            ));
        }
    }

    // 7. Env-var sanity (local checks, no transport call). These
    //    look at the iclaw client's env which is the operator's env;
    //    the host inherits the same env when launched from a
    //    setup-written `.env` file, so a missing var here is almost
    //    certainly the same one the host saw on boot.
    if std::env::var("ANTHROPIC_API_KEY").map(|s| !s.is_empty()).unwrap_or(false) {
        checks.push(Check::ok(
            "anthropic-key",
            "ANTHROPIC_API_KEY is set in this shell's env",
        ));
    } else {
        checks.push(Check::warn(
            "anthropic-key",
            "ANTHROPIC_API_KEY is unset in this shell — the host reads it from its own env / .env, so this is only conclusive when you launched `ironclaw run` from this shell",
            Some("set ANTHROPIC_API_KEY in the install's .env (typically under $XDG_DATA_HOME/ironclaw/.env on Linux)"),
        ));
    }

    // 8. Web-search providers (informational only — none configured
    //    is a perfectly valid install).
    let providers: Vec<&'static str> = [
        ("TAVILY_API_KEY", "tavily"),
        ("EXA_API_KEY", "exa"),
        ("BRAVE_SEARCH_API_KEY", "brave"),
        ("SERPAPI_API_KEY", "serpapi"),
    ]
    .iter()
    .filter_map(|(var, name)| {
        std::env::var(var).ok().filter(|s| !s.is_empty()).map(|_| *name)
    })
    .collect();
    if providers.is_empty() {
        checks.push(Check::ok(
            "web-search",
            "no web_search providers configured (the tool will surface a friendly error if the agent calls it)",
        ));
    } else {
        checks.push(Check::ok(
            "web-search",
            format!("{} provider(s) configured: {}", providers.len(), providers.join(", ")),
        ));
    }

    finalise_doctor(&checks, as_json)
}

/// Render the collected `doctor` checks and pick the exit code.
fn finalise_doctor(checks: &[Check], as_json: bool) -> RunOutput {
    let any_fail = checks.iter().any(|c| c.level == CheckLevel::Fail);
    if as_json {
        let payload = serde_json::json!({
            "status": if any_fail { "fail" } else { "ok" },
            "checks": checks.iter().map(Check::to_json).collect::<Vec<_>>(),
        });
        let mut out = render_json_pretty(&payload);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return if any_fail {
            RunOutput::failure(out)
        } else {
            RunOutput::success(out)
        };
    }
    let mut out = String::new();
    for c in checks {
        out.push_str(&format!("[{}] {:<18} {}\n", c.level.tag(), c.name, c.detail));
        if c.level != CheckLevel::Ok {
            if let Some(fix) = &c.fix {
                out.push_str(&format!("       fix: {fix}\n"));
            }
        }
    }
    if any_fail {
        out.push_str("\nat least one check is in FAIL state; see the `fix:` lines above\n");
        RunOutput::failure(out)
    } else if checks.iter().any(|c| c.level == CheckLevel::Warn) {
        out.push_str("\ninstall is reachable but has warnings; see the `fix:` lines above\n");
        RunOutput::success(out)
    } else {
        out.push_str("\nall checks passed; install is ready for `iclaw chat`\n");
        RunOutput::success(out)
    }
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

    // ── iclaw doctor ───────────────────────────────────────────────

    #[tokio::test]
    async fn doctor_socket_unreachable_fails_with_fix() {
        // First call (groups.list) errors → doctor bails out with a FAIL row
        // pointing at `ironclaw run`.
        let t = SequencedTransport::new(vec![Err(ClientError::Timeout)]);
        let out = run_cli(["iclaw", "doctor"], &t).await;
        assert!(out.stdout.is_empty(), "doctor must use stderr for fail: stdout={:?}", out.stdout);
        assert!(out.stderr.contains("FAIL"));
        assert!(out.stderr.contains("host-reachable"));
        assert!(out.stderr.contains("ironclaw run"));
        // It should *not* keep going and hit downstream endpoints once
        // the socket is gone.
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[tokio::test]
    async fn doctor_empty_install_warns_about_groups_and_wirings() {
        // groups=[], wirings=[], active sessions=[], audit=[], dropped=[]
        let t = SequencedTransport::new(vec![
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["iclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("FAIL"), "must include FAIL for missing agent group");
        assert!(combined.contains("agent-group"));
        assert!(combined.contains("iclaw quickstart cli"));
        // Wirings is gated on group existence; with zero groups the
        // wiring row will show WARN (no wirings). Either way, the
        // remediation hint mentions quickstart.
    }

    #[tokio::test]
    async fn doctor_happy_path_has_no_fails() {
        // groups=1, wirings=1, sessions=[], audit=[], dropped=[]
        // We deliberately do NOT assert on the ANTHROPIC_API_KEY check
        // result — the iclaw process inherits the test runner's env,
        // which may or may not have the key set, and doctor's purpose
        // is to surface that mismatch as a WARN rather than treating
        // it as a hard failure.
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["iclaw", "doctor"], &t).await;
        // Happy-path doctor never lands in stderr because no checks fail.
        assert!(
            out.stderr.is_empty(),
            "expected no stderr on happy doctor; got: {:?}",
            out.stderr,
        );
        assert!(out.stdout.contains("OK"));
        assert!(out.stdout.contains("host-reachable"));
        assert!(out.stdout.contains("agent-group"));
        assert!(out.stdout.contains("wiring"));
        // No FAIL rows, and the trailer is one of the two non-failure
        // forms ("all checks passed" or "install is reachable but has
        // warnings"), never the failure trailer.
        assert!(!out.stdout.contains("FAIL"));
        assert!(
            out.stdout.contains("all checks passed")
                || out.stdout.contains("install is reachable but has warnings"),
            "unexpected trailer: {:?}",
            out.stdout,
        );
    }

    #[tokio::test]
    async fn doctor_surfaces_recent_audit_errors() {
        // Group + wiring present, but the audit list returns a failing row.
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([
                {
                    "command": "groups.create",
                    "result": "error",
                    "error_code": "invalid_input"
                }
            ])),
            Ok(json!([])),
        ]);
        let out = run_cli(["iclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("WARN"));
        assert!(combined.contains("audit-errors"));
        assert!(combined.contains("groups.create -> invalid_input"));
    }

    #[tokio::test]
    async fn doctor_dropped_messages_warn() {
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([{"id": "drop-1"}, {"id": "drop-2"}])),
        ]);
        let out = run_cli(["iclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("dropped-messages"));
        assert!(combined.contains("2 dropped"));
    }

    #[tokio::test]
    async fn doctor_json_mode_emits_structured_payload() {
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["iclaw", "--json", "doctor"], &t).await;
        let payload: serde_json::Value = serde_json::from_str(out.stdout.trim())
            .expect("doctor --json must be parseable JSON");
        assert_eq!(payload["status"], "ok");
        let checks = payload["checks"].as_array().unwrap();
        assert!(checks.iter().any(|c| c["name"] == "host-reachable"));
        assert!(checks.iter().any(|c| c["name"] == "agent-group"));
        // Every entry must have a level + detail; failing rows must have fix.
        for c in checks {
            assert!(c["level"].is_string());
            assert!(c["detail"].is_string());
            if c["level"] == "fail" {
                assert!(c["fix"].is_string());
            }
        }
    }

    #[tokio::test]
    async fn doctor_json_fail_status_when_any_check_fails() {
        let t = SequencedTransport::new(vec![Err(ClientError::Timeout)]);
        let out = run_cli(["iclaw", "--json", "doctor"], &t).await;
        // FAIL paths go to stderr per RunOutput::failure convention.
        let payload: serde_json::Value = serde_json::from_str(out.stderr.trim())
            .expect("--json fail must still emit JSON");
        assert_eq!(payload["status"], "fail");
    }
}
