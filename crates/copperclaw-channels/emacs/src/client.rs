//! Subprocess abstraction for `emacsclient -e <sexp>`.
//!
//! The real implementation ([`EmacsClientCli`]) spawns the binary configured
//! in [`EmacsConfig`](crate::config::EmacsConfig). Tests pass an alternative
//! implementation (e.g. [`MockEmacsClient`]) so they don't need a running
//! Emacs server.

use crate::config::EmacsConfig;
use async_trait::async_trait;
use copperclaw_channels_core::AdapterError;
use std::process::ExitStatus;
use std::sync::Mutex;
use tokio::process::Command;

/// Trait for evaluating an elisp form via some transport.
///
/// `eval(sexp)` corresponds to running `emacsclient -e <sexp>` and returning
/// stdout on success. On non-zero exit the implementation maps the stderr
/// payload to a typed [`AdapterError`].
#[async_trait]
pub trait EmacsClient: Send + Sync {
    /// Evaluate an elisp form and return the printed representation of the
    /// result (trailing newline included if the underlying tool emitted
    /// one; callers should `.trim()` if needed).
    async fn eval(&self, sexp: &str) -> Result<String, AdapterError>;
}

/// `emacsclient` invocation built from an [`EmacsConfig`].
///
/// This is split out from [`tokio::process::Command`] construction so tests
/// can inspect the planned argv without actually spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmacsClientPlan {
    /// Program to spawn.
    pub program: String,
    /// Arguments excluding the `-e <sexp>` pair (which is appended per
    /// call). Includes any socket-name / socket-dir flags.
    pub base_args: Vec<String>,
}

impl EmacsClientPlan {
    /// Build a plan from the relevant config fields.
    pub fn from_config(cfg: &EmacsConfig) -> Self {
        let mut base_args: Vec<String> = Vec::new();
        if let Some(name) = &cfg.socket_name {
            base_args.push("-s".into());
            base_args.push(name.clone());
        }
        if let Some(dir) = &cfg.socket_dir {
            base_args.push("--socket-name".into());
            base_args.push(dir.display().to_string());
        }
        Self {
            program: cfg.client_bin.clone(),
            base_args,
        }
    }

    /// Build the full argv (without the program) for a given sexp.
    pub fn full_args(&self, sexp: &str) -> Vec<String> {
        let mut v = self.base_args.clone();
        v.push("-e".into());
        v.push(sexp.to_owned());
        v
    }

    /// Build a [`tokio::process::Command`] ready to spawn.
    pub fn to_command(&self, sexp: &str) -> Command {
        let mut cmd = Command::new(&self.program);
        for arg in &self.base_args {
            cmd.arg(arg);
        }
        cmd.arg("-e").arg(sexp);
        cmd
    }
}

/// Real [`EmacsClient`] that spawns `emacsclient` for each evaluation.
#[derive(Debug)]
pub struct EmacsClientCli {
    plan: EmacsClientPlan,
}

impl EmacsClientCli {
    /// Construct from a config.
    pub fn from_config(cfg: &EmacsConfig) -> Self {
        Self {
            plan: EmacsClientPlan::from_config(cfg),
        }
    }

    /// Construct from a pre-built plan.
    pub fn from_plan(plan: EmacsClientPlan) -> Self {
        Self { plan }
    }

    /// Borrow the [`EmacsClientPlan`].
    pub fn plan(&self) -> &EmacsClientPlan {
        &self.plan
    }
}

#[async_trait]
impl EmacsClient for EmacsClientCli {
    async fn eval(&self, sexp: &str) -> Result<String, AdapterError> {
        let mut cmd = self.plan.to_command(sexp);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let output = cmd.output().await.map_err(|e| {
            // Spawn failure (binary missing, permission denied, etc.) is a
            // transport-class error: the server is not reachable.
            AdapterError::Transport(format!(
                "spawn `{program}` failed: {e}",
                program = self.plan.program,
            ))
        })?;
        classify_output(
            &output.status,
            &output.stdout,
            &output.stderr,
            &self.plan.program,
        )
    }
}

/// Pure helper: turn an `emacsclient` exit into either stdout-as-string or a
/// typed [`AdapterError`].
///
/// Exposed for tests; not part of the channel's public surface conceptually.
pub fn classify_output(
    status: &ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
    program: &str,
) -> Result<String, AdapterError> {
    let stderr_str = String::from_utf8_lossy(stderr);
    if status.success() {
        return Ok(String::from_utf8_lossy(stdout).into_owned());
    }
    // emacsclient prints "can't find socket; have you started the server?"
    // (older releases use "no socket name set by user") when no daemon is
    // running. Treat that family of messages as Auth-class: the channel
    // can't reach the platform.
    let lc = stderr_str.to_lowercase();
    if lc.contains("can't find socket")
        || lc.contains("can not find socket")
        || lc.contains("no socket")
    {
        return Err(AdapterError::Auth(format!(
            "emacs server not reachable: {}",
            stderr_str.trim()
        )));
    }
    Err(AdapterError::Transport(format!(
        "`{program}` exited with {status}: {}",
        stderr_str.trim()
    )))
}

/// In-memory [`EmacsClient`] for tests.
///
/// Pushes each call's sexp onto an internal log and replies with the next
/// scripted response. If the script is exhausted, the last response is
/// reused for every subsequent call.
#[derive(Debug, Default)]
pub struct MockEmacsClient {
    state: Mutex<MockState>,
}

#[derive(Debug, Default)]
struct MockState {
    responses: Vec<Result<String, AdapterError>>,
    /// Log of sexps the client received, oldest first.
    log: Vec<String>,
    /// Cursor into responses.
    cursor: usize,
}

impl MockEmacsClient {
    /// Construct with a single response that will be returned for every
    /// call.
    pub fn always_ok(reply: impl Into<String>) -> Self {
        let mock = Self::default();
        mock.push_ok(reply);
        mock
    }

    /// Construct empty; use [`push_ok`](Self::push_ok) /
    /// [`push_err`](Self::push_err) to add scripted responses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a successful response to the script.
    pub fn push_ok(&self, reply: impl Into<String>) {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.responses.push(Ok(reply.into()));
    }

    /// Append a failure response to the script.
    pub fn push_err(&self, err: AdapterError) {
        let mut g = self.state.lock().expect("mock mutex poisoned");
        g.responses.push(Err(err));
    }

    /// Snapshot of the recorded call log.
    pub fn calls(&self) -> Vec<String> {
        let g = self.state.lock().expect("mock mutex poisoned");
        g.log.clone()
    }

    /// Number of recorded calls.
    pub fn call_count(&self) -> usize {
        let g = self.state.lock().expect("mock mutex poisoned");
        g.log.len()
    }
}

#[async_trait]
impl EmacsClient for MockEmacsClient {
    async fn eval(&self, sexp: &str) -> Result<String, AdapterError> {
        let response = {
            let mut g = self.state.lock().expect("mock mutex poisoned");
            g.log.push(sexp.to_owned());
            if g.responses.is_empty() {
                return Err(AdapterError::Transport(
                    "mock emacs client: no scripted responses".into(),
                ));
            }
            let idx = g.cursor.min(g.responses.len() - 1);
            g.cursor = (g.cursor + 1).min(g.responses.len());
            // Clone the result (we don't move it out so it can be re-served
            // if the script is exhausted).
            match &g.responses[idx] {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(clone_adapter_error(e)),
            }
        };
        response
    }
}

/// `AdapterError` is `#[derive(Debug, Error)]` but not `Clone`, so we
/// produce a faithful copy by re-stringifying the variant. Lossy for `Io`
/// (we collapse to `Transport`), but the only consumers are tests.
fn clone_adapter_error(err: &AdapterError) -> AdapterError {
    match err {
        AdapterError::Io(e) => AdapterError::Transport(format!("io: {e}")),
        AdapterError::Transport(s) => AdapterError::Transport(s.clone()),
        AdapterError::Auth(s) => AdapterError::Auth(s.clone()),
        AdapterError::Rate { retry_after } => AdapterError::Rate {
            retry_after: *retry_after,
        },
        AdapterError::BadRequest(s) => AdapterError::BadRequest(s.clone()),
        AdapterError::NotImplemented => AdapterError::NotImplemented,
        AdapterError::Unsupported(s) => AdapterError::Unsupported(s.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::EmacsConfig;
    use std::path::PathBuf;

    fn cfg_with(bin: &str, name: Option<&str>, dir: Option<&str>) -> EmacsConfig {
        let mut c = EmacsConfig::default();
        c.client_bin = bin.into();
        c.socket_name = name.map(str::to_owned);
        c.socket_dir = dir.map(PathBuf::from);
        c
    }

    #[test]
    fn plan_default_has_no_socket_args() {
        let plan = EmacsClientPlan::from_config(&EmacsConfig::default());
        assert_eq!(plan.program, "emacsclient");
        assert!(plan.base_args.is_empty());
    }

    #[test]
    fn plan_with_socket_name_only() {
        let plan = EmacsClientPlan::from_config(&cfg_with("emacsclient", Some("copperclaw"), None));
        assert_eq!(plan.base_args, vec!["-s".to_string(), "copperclaw".into()]);
    }

    #[test]
    fn plan_with_socket_dir_only() {
        let plan = EmacsClientPlan::from_config(&cfg_with("emacsclient", None, Some("/run/x")));
        assert_eq!(
            plan.base_args,
            vec!["--socket-name".to_string(), "/run/x".into()]
        );
    }

    #[test]
    fn plan_with_both_socket_name_and_dir() {
        let plan = EmacsClientPlan::from_config(&cfg_with(
            "emacsclient",
            Some("copperclaw"),
            Some("/run/x"),
        ));
        assert_eq!(
            plan.base_args,
            vec![
                "-s".to_string(),
                "copperclaw".into(),
                "--socket-name".into(),
                "/run/x".into()
            ]
        );
    }

    #[test]
    fn plan_overrides_program_name() {
        let plan =
            EmacsClientPlan::from_config(&cfg_with("/opt/emacs/bin/emacsclient", None, None));
        assert_eq!(plan.program, "/opt/emacs/bin/emacsclient");
    }

    #[test]
    fn full_args_appends_e_and_sexp() {
        let plan = EmacsClientPlan::from_config(&cfg_with("emacsclient", Some("x"), None));
        let args = plan.full_args("(foo)");
        assert_eq!(args, vec!["-s", "x", "-e", "(foo)"]);
    }

    #[test]
    fn full_args_with_no_socket() {
        let plan = EmacsClientPlan::from_config(&EmacsConfig::default());
        let args = plan.full_args("nil");
        assert_eq!(args, vec!["-e", "nil"]);
    }

    #[test]
    fn to_command_program_matches() {
        let plan = EmacsClientPlan::from_config(&cfg_with("emacsclient", None, None));
        let cmd = plan.to_command("(foo)");
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), std::ffi::OsStr::new("emacsclient"));
    }

    #[test]
    fn to_command_includes_socket_args_and_e_sexp() {
        let plan = EmacsClientPlan::from_config(&cfg_with(
            "emacsclient",
            Some("copperclaw"),
            Some("/run/x"),
        ));
        let cmd = plan.to_command("(copperclaw-pop-inbound)");
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "-s".to_string(),
                "copperclaw".into(),
                "--socket-name".into(),
                "/run/x".into(),
                "-e".into(),
                "(copperclaw-pop-inbound)".into()
            ]
        );
    }

    fn fake_status(code: i32) -> std::process::ExitStatus {
        // ExitStatus has no portable constructor on stable; use Command on
        // a deterministic noop. We exec /bin/sh with explicit exit code.
        use std::process::Command as StdCommand;
        let out = StdCommand::new("/bin/sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .output()
            .expect("spawn /bin/sh");
        out.status
    }

    #[test]
    fn classify_output_success_returns_stdout() {
        let s = fake_status(0);
        let res = classify_output(&s, b"nil\n", b"", "emacsclient").unwrap();
        assert_eq!(res, "nil\n");
    }

    #[test]
    fn classify_output_no_server_maps_to_auth() {
        let s = fake_status(1);
        let err = classify_output(
            &s,
            b"",
            b"emacsclient: can't find socket; have you started the server?\n",
            "emacsclient",
        )
        .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_output_no_socket_variant() {
        let s = fake_status(1);
        let err = classify_output(&s, b"", b"emacsclient: no socket name set\n", "emacsclient")
            .unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[test]
    fn classify_output_other_failure_is_transport() {
        let s = fake_status(2);
        let err = classify_output(&s, b"", b"some other error\n", "emacsclient").unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn classify_output_includes_program_name_in_transport() {
        let s = fake_status(1);
        let err = classify_output(&s, b"", b"boom\n", "custom-emacsclient").unwrap_err();
        match err {
            AdapterError::Transport(msg) => assert!(msg.contains("custom-emacsclient")),
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_client_records_calls() {
        let mock = MockEmacsClient::new();
        mock.push_ok("nil\n");
        mock.push_ok("(())");
        let r1 = mock.eval("(a)").await.unwrap();
        let r2 = mock.eval("(b)").await.unwrap();
        assert_eq!(r1, "nil\n");
        assert_eq!(r2, "(())");
        let calls = mock.calls();
        assert_eq!(calls, vec!["(a)".to_string(), "(b)".into()]);
        assert_eq!(mock.call_count(), 2);
    }

    #[tokio::test]
    async fn mock_client_reuses_last_response_when_exhausted() {
        let mock = MockEmacsClient::always_ok("nil\n");
        let r1 = mock.eval("(a)").await.unwrap();
        let r2 = mock.eval("(b)").await.unwrap();
        assert_eq!(r1, r2);
    }

    #[tokio::test]
    async fn mock_client_with_no_responses_errors() {
        let mock = MockEmacsClient::new();
        let err = mock.eval("(a)").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn mock_client_can_yield_errors() {
        let mock = MockEmacsClient::new();
        mock.push_err(AdapterError::Auth("nope".into()));
        let err = mock.eval("(a)").await.unwrap_err();
        assert!(matches!(err, AdapterError::Auth(_)));
    }

    #[tokio::test]
    async fn real_client_spawn_failure_is_transport() {
        let mut cfg = EmacsConfig::default();
        cfg.client_bin = "/does/not/exist/emacsclient-xyzzy".into();
        let client = EmacsClientCli::from_config(&cfg);
        let err = client.eval("nil").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[test]
    fn real_client_exposes_plan() {
        let cfg = EmacsConfig::default();
        let client = EmacsClientCli::from_config(&cfg);
        assert_eq!(client.plan().program, "emacsclient");
    }

    #[test]
    fn real_client_from_plan_preserves_program() {
        let plan = EmacsClientPlan {
            program: "x".into(),
            base_args: vec![],
        };
        let client = EmacsClientCli::from_plan(plan);
        assert_eq!(client.plan().program, "x");
    }

    #[test]
    fn clone_adapter_error_covers_all_variants() {
        let cases = vec![
            AdapterError::Transport("t".into()),
            AdapterError::Auth("a".into()),
            AdapterError::Rate {
                retry_after: Some(5),
            },
            AdapterError::Rate { retry_after: None },
            AdapterError::BadRequest("b".into()),
            AdapterError::NotImplemented,
            AdapterError::Unsupported("u".into()),
            AdapterError::Io(std::io::Error::new(std::io::ErrorKind::Other, "x")),
        ];
        for err in &cases {
            let cloned = clone_adapter_error(err);
            // Display should not be empty; for Io we collapse to Transport.
            let _ = format!("{cloned}");
        }
    }
}
