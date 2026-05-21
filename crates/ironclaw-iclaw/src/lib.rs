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
pub use commands::{Cli, ParsedCall, TopCommand, ALL_COMMANDS};
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
}
