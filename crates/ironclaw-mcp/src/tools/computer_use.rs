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

pub mod shell {
    //! `shell`: run a bash command inside the container.

    use super::{
        cap_output, json, parse_args, success_json, CallToolResult, Deserialize,
        Duration, JsonObject, Tool, ToolEntry, ToolError, ToolHandler,
        SHELL_DEFAULT_TIMEOUT_SECS, SHELL_MAX_TIMEOUT_SECS, SHELL_OUTPUT_CAP,
    };

    #[derive(Debug, Deserialize)]
    struct Input {
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    }

    pub fn schema() -> Tool {
        super::make_tool(
            "shell",
            "Run a bash command inside the container. Returns stdout, stderr, and exit code. Output is capped at 64 KiB.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["command"],
                "properties": {
                    "command":      { "type": "string", "minLength": 1 },
                    "cwd":          { "type": ["string", "null"] },
                    "timeout_secs": { "type": ["integer", "null"], "minimum": 1, "maximum": 600 }
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

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(&input.command);
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
    }

    pub fn schema() -> Tool {
        super::make_tool(
            "web_fetch",
            "Fetch an HTTP(S) URL. Defaults to GET; pass `method: POST` and `body` for posts. Response body is capped at 256 KiB.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["url"],
                "properties": {
                    "url":          { "type": "string", "minLength": 1 },
                    "method":       { "type": ["string", "null"] },
                    "body":         { "type": ["string", "null"] },
                    "timeout_secs": { "type": ["integer", "null"], "minimum": 1, "maximum": 120 }
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
        let bytes = resp.bytes().await.map_err(|e| {
            ToolError::Internal(format!("web_fetch({}) read body: {e}", input.url))
        })?;
        let (body, truncated) =
            cap_output(&String::from_utf8_lossy(&bytes), WEB_FETCH_CAP);
        Ok(success_json(&json!({
            "url": input.url,
            "method": method,
            "status": status,
            "headers": headers,
            "size_bytes": bytes.len(),
            "truncated": truncated,
            "body": body,
            "elapsed_ms": started.elapsed().as_millis(),
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
    use std::sync::Arc;

    fn ctx() -> Arc<dyn crate::context::ToolContext> {
        Arc::new(crate::context::MockToolContext::new())
    }

    #[tokio::test]
    async fn shell_echoes_stdout() {
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
