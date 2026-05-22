//! Tools that let the agent observe and modify the world inside its
//! sandboxed container: shell, filesystem reads/writes, HTTP fetches.
//!
//! Safety model: each container is isolated and ephemeral — that's the
//! whole point of the architecture. So these tools intentionally trust
//! the agent inside the container, and only worry about *blast within
//! the container*: output caps so a `find /` doesn't blow up the
//! provider's context window, file-size caps so the model can't
//! accidentally pipe a 10GB file through itself, request timeouts
//! that prevent hangs.
//!
//! Anything destructive the agent does is bounded by the container's
//! lifetime — the manager idle-stops it after 5 minutes of quiet, and
//! crashes restart it from clean state.

use crate::error::ToolError;
use crate::tools::{make_tool, parse_args, success_json, ToolEntry, ToolHandler};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

/// Max bytes of stdout/stderr we'll surface back to the model from
/// `shell`. Tuned so a noisy build can still report status without
/// blowing the context window — most things the agent cares about
/// are at the head or tail of output anyway.
const SHELL_OUTPUT_CAP: usize = 64 * 1024;
/// Default timeout for `shell` if the caller doesn't override.
const SHELL_DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard ceiling — the model can't disable timeouts entirely.
const SHELL_MAX_TIMEOUT_SECS: u64 = 600;
/// Max bytes `read_file` will return. Beyond this we return a
/// truncated head + a hint to the model.
const READ_FILE_CAP: usize = 1024 * 1024;
/// Default timeout for `web_fetch`.
const WEB_FETCH_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Max body bytes `web_fetch` will surface to the model.
const WEB_FETCH_CAP: usize = 256 * 1024;
/// Default path of the per-session shell state file. The container
/// runtime bind-mounts the session's host directory at `/data`, so
/// the state file is automatically scoped to a single session — no
/// cross-session leakage. Tests override this via
/// `IRONCLAW_SHELL_STATE_FILE` (see [`shell_state_path`]).
const SHELL_STATE_DEFAULT_PATH: &str = "/data/.shell_state";
/// Env var name that overrides [`SHELL_STATE_DEFAULT_PATH`]. Used by
/// the test suite to redirect the state file into a tempdir.
const SHELL_STATE_ENV_OVERRIDE: &str = "IRONCLAW_SHELL_STATE_FILE";

pub mod shell {
    //! `shell`: run a bash command inside the container.

    use super::{
        cap_output, json, parse_args, shell_state_path, success_json, CallToolResult,
        Deserialize, Duration, JsonObject, Tool, ToolEntry, ToolError, ToolHandler,
        SHELL_DEFAULT_TIMEOUT_SECS, SHELL_MAX_TIMEOUT_SECS, SHELL_OUTPUT_CAP,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// When true, wipe the persistent shell-state file before
        /// running. `cd`/exports from earlier calls are discarded.
        #[serde(default)]
        reset: bool,
    }

    pub fn schema() -> Tool {
        super::make_tool(
            "shell",
            "Run a bash command inside the container. Returns stdout, stderr, and exit code. Output is capped at 64 KiB. Working directory and exported environment variables persist across calls within a session; pass `reset: true` to wipe that state.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["command"],
                "properties": {
                    "command":      { "type": "string", "minLength": 1 },
                    "cwd":          { "type": ["string", "null"] },
                    "timeout_secs": { "type": ["integer", "null"], "minimum": 1, "maximum": 600 },
                    "reset":        { "type": "boolean" }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.command.trim().is_empty() {
            return Err(ToolError::Validation("`command` must be non-empty".into()));
        }
        let timeout = Duration::from_secs(
            input
                .timeout_secs
                .unwrap_or(SHELL_DEFAULT_TIMEOUT_SECS)
                .min(SHELL_MAX_TIMEOUT_SECS),
        );

        // Resolve the per-session state file. `reset: true` removes it
        // before we wrap the user's command so the source-on-entry step
        // is a no-op for this call. The post-command capture below then
        // writes a fresh state file from the resulting env.
        let state_path = shell_state_path();
        if input.reset {
            // Ignore NotFound — that's the "already clean" case.
            match tokio::fs::remove_file(&state_path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(ToolError::Internal(format!(
                        "shell reset failed: remove({}): {e}",
                        state_path.display()
                    )));
                }
            }
        }

        // Build the wrapped command. The leading `source` is guarded by
        // `[ -f ... ]` so the first call (no state yet) is fine. The
        // trailing capture writes `cd $(printf %q "$PWD")` plus the
        // filtered `export -p` so the next call resumes the agent's
        // working directory and non-secret environment.
        //
        // We intentionally use `(set +e; …) ; status=$?` semantics for
        // the user's command so its exit code surfaces back through
        // `$?` even though the capture step runs afterwards.
        let wrapped = build_wrapped_command(&input.command, &state_path);

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(&wrapped);
        if let Some(cwd) = &input.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn().map_err(|e| {
            ToolError::Internal(format!("shell spawn failed: {e}"))
        })?;

        let started = std::time::Instant::now();
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(ToolError::Internal(format!("shell wait failed: {e}")));
            }
            Err(_) => {
                return Ok(success_json(&json!({
                    "command": input.command,
                    "timed_out": true,
                    "timeout_secs": timeout.as_secs(),
                    "elapsed_ms": started.elapsed().as_millis(),
                })));
            }
        };

        let (stdout, stdout_truncated) =
            cap_output(&String::from_utf8_lossy(&output.stdout), SHELL_OUTPUT_CAP);
        let (stderr, stderr_truncated) =
            cap_output(&String::from_utf8_lossy(&output.stderr), SHELL_OUTPUT_CAP);

        Ok(success_json(&json!({
            "command": input.command,
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "elapsed_ms": started.elapsed().as_millis(),
        })))
    }

    /// Compose the bash one-liner that sources the prior state, runs
    /// the user's command, and writes a fresh state file. Kept as a
    /// free function so tests can pin the exact wrapping.
    ///
    /// The filter clause inside `export -p | grep -vE ...` is the
    /// secret-redaction step: any name ending in `_TOKEN`, `_KEY`,
    /// `_SECRET`, or starting with `ANTHROPIC_` is dropped from the
    /// persisted snapshot so a `web_fetch` of a credential URL or a
    /// runner-injected env var doesn't bleed into the state file. The
    /// pattern is intentionally conservative — false positives only
    /// mean the agent has to re-export the value in the next call.
    fn build_wrapped_command(user_cmd: &str, state_path: &std::path::Path) -> String {
        let state_str = state_path.display().to_string();
        // Both the user command and the wrapped capture step go into
        // a single `bash -c` invocation so they share an env.
        format!(
            "[ -f {state} ] && source {state}; \
             {user_cmd}; \
             __ic_status=$?; \
             {{ echo \"cd $(printf %q \\\"$PWD\\\")\"; \
                export -p | grep -vE '^declare -x (ANTHROPIC_|[A-Za-z_][A-Za-z0-9_]*(_TOKEN|_KEY|_SECRET))='; \
             }} > {state} 2>/dev/null; \
             exit $__ic_status",
            state = shell_quote(&state_str),
            user_cmd = user_cmd,
        )
    }

    /// Single-quote a path for safe substitution inside the wrapped
    /// bash command. Inputs are state-file paths we control, so the
    /// minimal `'...'` quoting (with `'` -> `'\''`) is sufficient.
    fn shell_quote(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    }

    #[cfg(test)]
    pub(super) fn build_wrapped_command_for_test(
        user_cmd: &str,
        state_path: &std::path::Path,
    ) -> String {
        build_wrapped_command(user_cmd, state_path)
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn crate::context::ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }

    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod read_file {
    //! `read_file`: read a UTF-8 file from the container filesystem.

    use super::{
        json, parse_args, success_json, CallToolResult, Deserialize, JsonObject,
        PathBuf, Tool, ToolEntry, ToolError, ToolHandler, READ_FILE_CAP,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        path: String,
    }

    pub fn schema() -> Tool {
        super::make_tool(
            "read_file",
            "Read a UTF-8 file from the container filesystem. Capped at 1 MiB; larger files are truncated to the head with `truncated: true` in the response.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "minLength": 1 }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let path = PathBuf::from(&input.path);
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            ToolError::Internal(format!("read_file({}): {e}", path.display()))
        })?;
        let truncated = bytes.len() > READ_FILE_CAP;
        let head = if truncated {
            // Truncate on a UTF-8 char boundary so the model gets
            // valid text — slicing raw bytes could split a multi-
            // byte rune.
            let s = String::from_utf8_lossy(&bytes[..READ_FILE_CAP]);
            s.into_owned()
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok(success_json(&json!({
            "path": path.display().to_string(),
            "size_bytes": bytes.len(),
            "truncated": truncated,
            "content": head,
        })))
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn crate::context::ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }

    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod write_file {
    //! `write_file`: write UTF-8 text to a file inside the container.

    use super::{
        json, parse_args, success_json, CallToolResult, Deserialize, JsonObject,
        PathBuf, Tool, ToolEntry, ToolError, ToolHandler,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        path: String,
        content: String,
        #[serde(default = "default_create_parents")]
        create_parents: bool,
        #[serde(default)]
        append: bool,
    }

    fn default_create_parents() -> bool {
        true
    }

    pub fn schema() -> Tool {
        super::make_tool(
            "write_file",
            "Write UTF-8 text to a file inside the container. Creates parent directories by default; pass `append: true` to add to an existing file rather than overwrite.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path", "content"],
                "properties": {
                    "path":           { "type": "string", "minLength": 1 },
                    "content":        { "type": "string" },
                    "create_parents": { "type": "boolean" },
                    "append":         { "type": "boolean" }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let path = PathBuf::from(&input.path);
        if input.create_parents {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        ToolError::Internal(format!(
                            "write_file({}): create_dir_all({}): {e}",
                            path.display(),
                            parent.display()
                        ))
                    })?;
                }
            }
        }
        let bytes = input.content.into_bytes();
        if input.append {
            use tokio::io::AsyncWriteExt;
            let mut f = tokio::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .await
                .map_err(|e| {
                    ToolError::Internal(format!(
                        "write_file({}) append open: {e}",
                        path.display()
                    ))
                })?;
            f.write_all(&bytes).await.map_err(|e| {
                ToolError::Internal(format!(
                    "write_file({}) append: {e}",
                    path.display()
                ))
            })?;
        } else {
            tokio::fs::write(&path, &bytes).await.map_err(|e| {
                ToolError::Internal(format!("write_file({}): {e}", path.display()))
            })?;
        }
        Ok(success_json(&json!({
            "path": path.display().to_string(),
            "bytes_written": bytes.len(),
            "appended": input.append,
        })))
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn crate::context::ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }

    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

pub mod web_fetch {
    //! `web_fetch`: HTTP GET (or POST) against a URL from the container.

    use super::{
        cap_output, json, parse_args, success_json, CallToolResult, Deserialize,
        Duration, JsonObject, Tool, ToolEntry, ToolError, ToolHandler,
        WEB_FETCH_CAP, WEB_FETCH_DEFAULT_TIMEOUT_SECS,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        url: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// When true, return the raw response body bytes unmodified
        /// even if the response is HTML. Default behavior converts
        /// HTML to markdown to save the model context window.
        #[serde(default)]
        raw: bool,
    }

    pub fn schema() -> Tool {
        super::make_tool(
            "web_fetch",
            "Fetch an HTTP(S) URL. Defaults to GET; pass `method: POST` and `body` for posts. HTML responses are automatically converted to markdown to save context — pass `raw: true` to receive the original bytes. Response body is capped at 256 KiB.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["url"],
                "properties": {
                    "url":          { "type": "string", "minLength": 1 },
                    "method":       { "type": ["string", "null"] },
                    "body":         { "type": ["string", "null"] },
                    "timeout_secs": { "type": ["integer", "null"], "minimum": 1, "maximum": 120 },
                    "raw":          { "type": "boolean" }
                }
            }),
        )
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let timeout = Duration::from_secs(
            input.timeout_secs.unwrap_or(WEB_FETCH_DEFAULT_TIMEOUT_SECS).min(120),
        );
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ToolError::Internal(format!("web_fetch client build: {e}")))?;
        let method = input
            .method
            .as_deref()
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let started = std::time::Instant::now();
        let req = match method.as_str() {
            "GET" => client.get(&input.url),
            "POST" => {
                let mut r = client.post(&input.url);
                if let Some(body) = &input.body {
                    r = r.body(body.clone());
                }
                r
            }
            other => {
                return Err(ToolError::Validation(format!(
                    "web_fetch: unsupported method `{other}` (only GET and POST)"
                )));
            }
        };
        let resp = req.send().await.map_err(|e| {
            ToolError::Internal(format!("web_fetch({}): {e}", input.url))
        })?;
        let status = resp.status().as_u16();
        let headers: serde_json::Map<String, serde_json::Value> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    serde_json::Value::String(
                        v.to_str().unwrap_or("<binary>").to_string(),
                    ),
                )
            })
            .collect();
        // Pull Content-Type before consuming the body — once we call
        // `bytes()` the response is moved.
        let content_type_header = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = resp.bytes().await.map_err(|e| {
            ToolError::Internal(format!("web_fetch({}) read body: {e}", input.url))
        })?;

        let raw_bytes = bytes.len();
        let conversion = decide_body_conversion(
            content_type_header.as_deref(),
            input.raw,
            &bytes,
        );
        let (body, truncated) = cap_output(&conversion.body, WEB_FETCH_CAP);
        let mut out = json!({
            "url": input.url,
            "method": method,
            "status": status,
            "headers": headers,
            "size_bytes": raw_bytes,
            "truncated": truncated,
            "body": body,
            "elapsed_ms": started.elapsed().as_millis(),
        });
        if let Some(md_len) = conversion.markdown_bytes {
            if let Some(map) = out.as_object_mut() {
                map.insert(
                    "content_type".into(),
                    serde_json::Value::String("text/html → markdown".into()),
                );
                map.insert(
                    "raw_html_bytes".into(),
                    serde_json::Value::Number(raw_bytes.into()),
                );
                map.insert(
                    "markdown_bytes".into(),
                    serde_json::Value::Number(md_len.into()),
                );
            }
        }
        Ok(success_json(&out))
    }

    /// What [`handle`] decided to do with the response body.
    struct BodyConversion {
        /// Stringified body — markdown when conversion fired, otherwise
        /// raw bytes as lossy UTF-8.
        body: String,
        /// `Some(len)` when the conversion ran; `None` when we passed
        /// the body through unchanged.
        markdown_bytes: Option<usize>,
    }

    /// Decide whether to convert the response body from HTML to
    /// markdown, and run the conversion. Factored out of [`handle`] so
    /// the latter stays under clippy's `too_many_lines` ceiling.
    fn decide_body_conversion(
        content_type: Option<&str>,
        raw_requested: bool,
        bytes: &[u8],
    ) -> BodyConversion {
        let is_html = content_type.is_some_and(is_html_content_type);
        if !is_html || raw_requested {
            return BodyConversion {
                body: String::from_utf8_lossy(bytes).into_owned(),
                markdown_bytes: None,
            };
        }
        let html = String::from_utf8_lossy(bytes).into_owned();
        // `htmd` strips `<script>` / `<style>`, preserves links, and
        // formats headings + lists. Defaults mirror turndown.
        let md = htmd::HtmlToMarkdown::new()
            .convert(&html)
            .unwrap_or_else(|_| html.clone());
        let md_len = md.len();
        BodyConversion {
            body: md,
            markdown_bytes: Some(md_len),
        }
    }

    /// Return true when a Content-Type header indicates HTML. Tolerates
    /// charset parameters, casing differences, and surrounding
    /// whitespace.
    pub(super) fn is_html_content_type(ct: &str) -> bool {
        // Strip parameters (`text/html; charset=utf-8`), trim, lowercase.
        let primary = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        primary == "text/html" || primary == "application/xhtml+xml"
    }

    struct Handler;
    #[async_trait::async_trait]
    impl ToolHandler for Handler {
        async fn call(
            &self,
            arguments: Option<JsonObject>,
            ctx: &dyn crate::context::ToolContext,
        ) -> Result<CallToolResult, ToolError> {
            handle(arguments, ctx).await
        }
    }

    pub fn entry() -> ToolEntry {
        ToolEntry {
            tool: schema(),
            handler: Box::new(Handler),
        }
    }
}

/// In-process override of the shell state file path. Only used by
/// the test suite — production code never sets this. Wrapped in a
/// `Mutex` so tests can install their own per-test tempdir without
/// resorting to (forbidden) `unsafe` env-var mutation.
#[cfg(test)]
static SHELL_STATE_TEST_OVERRIDE: std::sync::OnceLock<
    std::sync::Mutex<Option<PathBuf>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
fn shell_state_test_override_set(path: PathBuf) {
    let cell = SHELL_STATE_TEST_OVERRIDE
        .get_or_init(|| std::sync::Mutex::new(None));
    *cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
}

#[cfg(test)]
fn shell_state_test_override_clear() {
    if let Some(cell) = SHELL_STATE_TEST_OVERRIDE.get() {
        *cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
fn shell_state_test_override() -> Option<PathBuf> {
    SHELL_STATE_TEST_OVERRIDE
        .get()
        .and_then(|m| {
            m.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
}

#[cfg(not(test))]
fn shell_state_test_override() -> Option<PathBuf> {
    None
}

/// Resolve the path to the per-session shell state file. Defaults to
/// [`SHELL_STATE_DEFAULT_PATH`]; operators can move the file via
/// [`SHELL_STATE_ENV_OVERRIDE`]; tests install their own path via
/// the in-process override.
fn shell_state_path() -> PathBuf {
    if let Some(p) = shell_state_test_override() {
        return p;
    }
    std::env::var_os(SHELL_STATE_ENV_OVERRIDE)
        .map_or_else(|| PathBuf::from(SHELL_STATE_DEFAULT_PATH), PathBuf::from)
}

/// Cap a string to `max` bytes on a char boundary. Returns
/// `(capped_string, truncated_flag)`.
fn cap_output(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut cap = max;
    while !s.is_char_boundary(cap) {
        cap -= 1;
    }
    (format!("{}…[truncated]", &s[..cap]), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex, OnceLock};

    fn ctx() -> Arc<dyn crate::context::ToolContext> {
        Arc::new(crate::context::MockToolContext::new())
    }

    /// Serialise shell tests so the process-global state override
    /// doesn't get clobbered across parallel test threads. Each test
    /// owns its own state path through [`ShellTestGuard`].
    fn shell_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// RAII guard that points the shell state file at a fresh tempdir
    /// for the lifetime of the guard. Clears the override on drop so
    /// tests don't leak state into each other.
    struct ShellTestGuard {
        _dir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ShellTestGuard {
        fn new() -> Self {
            // Recover from a poisoned mutex — a panic in an earlier
            // test shouldn't wedge the entire test file.
            let lock = shell_env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("shell_state");
            shell_state_test_override_set(path);
            Self { _dir: dir, _lock: lock }
        }
    }

    impl Drop for ShellTestGuard {
        fn drop(&mut self) {
            shell_state_test_override_clear();
        }
    }

    #[tokio::test]
    async fn shell_echoes_stdout() {
        let _g = ShellTestGuard::new();
        let res = shell::handle(
            Some(
                json!({"command": "echo hello"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"exit_code\": 0"), "got: {body}");
        assert!(body.contains("hello"), "got: {body}");
    }

    #[tokio::test]
    async fn shell_reports_nonzero_exit() {
        let _g = ShellTestGuard::new();
        let res = shell::handle(
            Some(
                json!({"command": "false"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"exit_code\": 1"), "got: {body}");
    }

    #[tokio::test]
    async fn shell_honors_timeout() {
        let _g = ShellTestGuard::new();
        let res = shell::handle(
            Some(
                json!({"command": "sleep 5", "timeout_secs": 1})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"timed_out\": true"), "got: {body}");
    }

    #[tokio::test]
    async fn shell_rejects_empty_command() {
        let _g = ShellTestGuard::new();
        let err = shell::handle(
            Some(
                json!({"command": "   "})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn shell_persists_exported_env_across_calls() {
        let _g = ShellTestGuard::new();
        // First call exports FOO; the wrapper captures it.
        shell::handle(
            Some(
                json!({"command": "export FOO=bar"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        // Second call sources the captured state and echoes $FOO.
        let res = shell::handle(
            Some(
                json!({"command": "echo \"FOO=$FOO\""})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("FOO=bar"), "got: {body}");
    }

    #[tokio::test]
    async fn shell_persists_cwd_across_calls() {
        let _g = ShellTestGuard::new();
        shell::handle(
            Some(
                json!({"command": "cd /tmp"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let res = shell::handle(
            Some(
                json!({"command": "pwd"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("/tmp"), "got: {body}");
    }

    #[tokio::test]
    async fn shell_reset_wipes_state() {
        let _g = ShellTestGuard::new();
        shell::handle(
            Some(
                json!({"command": "export FOO=bar"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let res = shell::handle(
            Some(
                json!({"command": "echo \"FOO=$FOO\"", "reset": true})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        // FOO must be empty after reset.
        assert!(body.contains("FOO=\\n") || body.contains("FOO=\""), "got: {body}");
        // And specifically it should not contain `FOO=bar`.
        assert!(!body.contains("FOO=bar"), "got: {body}");
    }

    #[tokio::test]
    async fn shell_filters_anthropic_env_from_state() {
        let _g = ShellTestGuard::new();
        // Export a secret; the wrapper should NOT persist it.
        shell::handle(
            Some(
                json!({"command": "export ANTHROPIC_API_KEY=sekrit"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let res = shell::handle(
            Some(
                json!({"command": "echo \"K=$ANTHROPIC_API_KEY\""})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(!body.contains("sekrit"), "leaked secret: {body}");
        // Sanity: state file exists but doesn't contain the secret.
        let state_path = shell_state_path();
        if state_path.exists() {
            let content = std::fs::read_to_string(&state_path).unwrap_or_default();
            assert!(
                !content.contains("ANTHROPIC_API_KEY"),
                "state file leaked: {content}"
            );
        }
    }

    #[tokio::test]
    async fn shell_filters_token_key_secret_suffixes() {
        let _g = ShellTestGuard::new();
        shell::handle(
            Some(
                json!({"command": "export GH_TOKEN=t1 AWS_SECRET=s1 STRIPE_KEY=k1 SAFE_VAR=v1"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let state_path = shell_state_path();
        let content = std::fs::read_to_string(&state_path).unwrap_or_default();
        assert!(!content.contains("GH_TOKEN"), "GH_TOKEN leaked: {content}");
        assert!(!content.contains("AWS_SECRET"), "AWS_SECRET leaked: {content}");
        assert!(!content.contains("STRIPE_KEY"), "STRIPE_KEY leaked: {content}");
        assert!(content.contains("SAFE_VAR"), "SAFE_VAR missing: {content}");
    }

    #[test]
    fn shell_wrapped_command_sources_state_and_redirects_capture() {
        let path = std::path::Path::new("/tmp/test-state");
        let wrapped = shell::build_wrapped_command_for_test("echo hi", path);
        assert!(wrapped.contains("[ -f '/tmp/test-state' ] && source"));
        assert!(wrapped.contains("echo hi"));
        assert!(wrapped.contains("export -p"));
        assert!(wrapped.contains("ANTHROPIC_"));
        assert!(wrapped.contains("_TOKEN"));
        assert!(wrapped.contains("_KEY"));
        assert!(wrapped.contains("_SECRET"));
        assert!(wrapped.contains("exit $__ic_status"));
    }

    #[test]
    fn is_html_content_type_recognises_charset_param() {
        assert!(web_fetch::is_html_content_type("text/html"));
        assert!(web_fetch::is_html_content_type("text/html; charset=utf-8"));
        assert!(web_fetch::is_html_content_type("text/HTML"));
        assert!(web_fetch::is_html_content_type("  text/html ; charset=ascii"));
        assert!(web_fetch::is_html_content_type("application/xhtml+xml"));
        assert!(!web_fetch::is_html_content_type("application/json"));
        assert!(!web_fetch::is_html_content_type("text/plain"));
    }

    #[tokio::test]
    async fn web_fetch_converts_html_to_markdown() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_raw(
                        "<html><head><title>T</title></head><body><h1>Hello</h1><p>World <a href=\"x\">link</a></p></body></html>".as_bytes(),
                        "text/html",
                    ),
            )
            .mount(&server)
            .await;
        let res = web_fetch::handle(
            Some(
                json!({"url": format!("{}/", server.uri())})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("# Hello"), "got: {body}");
        assert!(body.contains("[link](x)"), "got: {body}");
        assert!(body.contains("text/html → markdown"), "got: {body}");
        assert!(body.contains("raw_html_bytes"), "got: {body}");
        assert!(body.contains("markdown_bytes"), "got: {body}");
        // The HTML tags themselves should be gone.
        assert!(!body.contains("<h1>"), "tags still present: {body}");
    }

    #[tokio::test]
    async fn web_fetch_passes_through_json_unchanged() {
        let server = wiremock::MockServer::start().await;
        let payload = r#"{"items":[1,2,3]}"#;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(payload.as_bytes(), "application/json"),
            )
            .mount(&server)
            .await;
        let res = web_fetch::handle(
            Some(
                json!({"url": format!("{}/api", server.uri())})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains(r#"\"items\":[1,2,3]"#), "got: {body}");
        // No conversion flag present.
        assert!(!body.contains("text/html → markdown"), "got: {body}");
        assert!(!body.contains("raw_html_bytes"), "got: {body}");
    }

    #[tokio::test]
    async fn web_fetch_raw_flag_returns_html_unmodified() {
        let server = wiremock::MockServer::start().await;
        let html = "<html><body><h1>Hi</h1></body></html>";
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(html.as_bytes(), "text/html"),
            )
            .mount(&server)
            .await;
        let res = web_fetch::handle(
            Some(
                json!({"url": format!("{}/", server.uri()), "raw": true})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        // Body should still contain the raw HTML tags.
        assert!(body.contains("<h1>Hi</h1>"), "got: {body}");
        assert!(!body.contains("text/html → markdown"), "got: {body}");
    }

    #[tokio::test]
    async fn web_fetch_html_with_charset_param_triggers_conversion() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw(
                    "<p>hi</p>".as_bytes(),
                    "text/html; charset=ISO-8859-1",
                ),
            )
            .mount(&server)
            .await;
        let res = web_fetch::handle(
            Some(
                json!({"url": format!("{}/", server.uri())})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("text/html → markdown"), "got: {body}");
        assert!(!body.contains("<p>hi</p>"), "raw HTML still present: {body}");
    }

    #[tokio::test]
    async fn read_then_write_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/foo.txt");
        // write
        let res = write_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "content": "hello world",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"bytes_written\": 11"), "got: {body}");
        // read
        let res = read_file::handle(
            Some(
                json!({"path": path.to_string_lossy()})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("hello world"), "got: {body}");
        assert!(body.contains("\"truncated\": false"), "got: {body}");
    }

    #[tokio::test]
    async fn write_file_append_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        for line in ["one\n", "two\n"] {
            write_file::handle(
                Some(
                    json!({
                        "path": path.to_string_lossy(),
                        "content": line,
                        "append": true,
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
                ctx().as_ref(),
            )
            .await
            .unwrap();
        }
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "one\ntwo\n");
    }

    #[tokio::test]
    async fn read_file_truncates_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // 2 MiB of 'a'
        let big = "a".repeat(2 * 1024 * 1024);
        tokio::fs::write(&path, &big).await.unwrap();
        let res = read_file::handle(
            Some(
                json!({"path": path.to_string_lossy()})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap();
        let body = result_text(&res);
        assert!(body.contains("\"truncated\": true"), "got: {body}");
    }

    #[tokio::test]
    async fn write_file_no_create_parents_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope/foo.txt");
        let err = write_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "content": "x",
                    "create_parents": false,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[test]
    fn cap_output_truncates_at_char_boundary() {
        let s = "héllo".repeat(100);
        let (got, truncated) = cap_output(&s, 10);
        assert!(truncated);
        assert!(got.ends_with("…[truncated]"));
        // The truncated prefix must still be valid UTF-8 (the function
        // guarantees we backed up to a char boundary).
        assert!(got.is_char_boundary(0));
    }

    #[test]
    fn cap_output_passes_through_short_strings() {
        let (got, truncated) = cap_output("hello", 100);
        assert_eq!(got, "hello");
        assert!(!truncated);
    }

    /// Pull the first text block out of a `CallToolResult`. Helpers
    /// don't ship in rmcp; we just match the variant.
    fn result_text(r: &CallToolResult) -> String {
        for c in &r.content {
            if let rmcp::model::RawContent::Text(t) = &c.raw {
                return t.text.clone();
            }
        }
        String::new()
    }
}
