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
        other => RunOutput::failure(format!("unknown composite op: {other}\n")),
    }
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
            caller,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("audit.list", &e)),
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
}
