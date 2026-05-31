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

/// Max bytes of stdout/stderr (PER STREAM) we'll surface back to the
/// model from `shell`. Tuned so a noisy build can still report status
/// without blowing the context window — most things the agent cares
/// about are at the head or tail of output anyway. If the agent needs
/// more, it should re-run with `tail -n 200` / `head -n 200` / `grep`
/// to narrow the output before invoking `shell`.
///
/// Tool results live in conversation history forever until compaction;
/// size is the dominant cost. Aggressive caps are correctness, not
/// just optimization — Sonnet produces malformed JSON at high context.
const SHELL_OUTPUT_CAP: usize = 32 * 1024;
/// Default timeout for `shell` if the caller doesn't override.
const SHELL_DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard ceiling — the model can't disable timeouts entirely.
const SHELL_MAX_TIMEOUT_SECS: u64 = 600;
/// Max bytes `read_file` will return. Beyond this we return a
/// truncated head + a hint to the model. Most source files are well
/// under this cap; for larger files the model should use `grep` /
/// `shell` to extract just the relevant region.
///
/// Tool results live in conversation history forever until compaction;
/// size is the dominant cost. Aggressive caps are correctness, not
/// just optimization — Sonnet produces malformed JSON at high context.
const READ_FILE_CAP: usize = 128 * 1024;
/// Default timeout for `web_fetch`.
const WEB_FETCH_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Max body bytes `web_fetch` will surface to the model. 16 KiB of
/// markdown-extracted text (~4k tokens) is enough context for the
/// model to understand a page; if it needs more, it can `web_fetch`
/// again with a different URL or strategy.
///
/// Tool results live in conversation history forever until compaction;
/// size is the dominant cost. Aggressive caps are correctness, not
/// just optimization — Sonnet produces malformed JSON at high context.
///
/// Reduced from 32 KiB → 16 KiB on 2026-05-24 after a Telegram session
/// asked for parallel F1 research and the model launched an `explore`
/// subagent that fetched the F1 homepage (JS-heavy SPA → ~30 KiB of
/// markdown-converted bundle text). One fetch's result, accumulated
/// across 3 explore-loop turns of identical history replay, consumed
/// the entire 60k token budget and the subagent stopped with
/// `token budget exceeded` having done effectively zero research. At
/// 16 KiB / ~4k tokens per fetch, the same budget supports ~6
/// real fetches before exhaustion.
const WEB_FETCH_CAP: usize = 16 * 1024;
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
            "Run a bash command inside the container. Returns stdout, stderr, and exit code. Output is capped at 32 KiB per stream — if you expect more, narrow it first with `tail -n 200`, `head -n 200`, or `grep`. Working directory and exported environment variables persist across calls within a session; pass `reset: true` to wipe that state.\n\nDO NOT use `shell` to create or edit files. Use the dedicated tools instead: `write_file` for new files, `edit_file` / `multi_edit` / `apply_patch` for changes, `copy_file` to duplicate. Heredoc patterns like `cat > foo << 'EOF' ... EOF` and `echo \"content\" > foo` waste tokens twice: the file body travels through history as the shell `command` string, AND the tool result. The dedicated tools take the body as a clean `content` field and don't echo it back. Heredoc-style file writes are REJECTED here with a redirect message.",
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

    /// Detect the obvious "write a file via shell" anti-patterns:
    ///
    /// - `cat > path << 'TAG'` / `cat >> path << TAG` (heredoc into file)
    /// - `tee path << TAG` and `tee -a path << TAG`
    /// - `cat <<TAG > path` (heredoc before redirect)
    ///
    /// Returns the matched pattern as a hint for the redirect message.
    ///
    /// We deliberately do NOT block `echo "..." > foo` even though it's
    /// the same anti-pattern — `echo` redirects are often legitimate
    /// short writes (1-2 lines of config) and false-positives would
    /// hurt more than the leak. The heredoc form is the dominant cost.
    pub(crate) fn detect_file_write_anti_pattern(cmd: &str) -> Option<&'static str> {
        // Cheap: collapse whitespace and lower-case to make matching less
        // brittle. We only need to recognise the shape, not parse bash.
        let lower = cmd.to_ascii_lowercase();
        let normalised: String = lower.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalised.contains("cat > ") && normalised.contains("<<") {
            return Some("cat > FILE << EOF");
        }
        if normalised.contains("cat >> ") && normalised.contains("<<") {
            return Some("cat >> FILE << EOF");
        }
        // `tee FILE << EOF` writes the heredoc body to FILE (with or
        // without `-a` for append, with or without an intermediate
        // pipe `> /dev/null` to suppress the echo). The pattern we
        // want is "tee, then heredoc, regardless of redirect" — `tee`
        // by definition writes to whatever filename follows it on the
        // command line.
        if (normalised.contains("tee ") || normalised.starts_with("tee "))
            && normalised.contains("<<")
        {
            return Some("tee FILE << EOF");
        }
        if normalised.contains("cat <<") && normalised.contains("> ") {
            return Some("cat <<EOF > FILE");
        }
        None
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        if input.command.trim().is_empty() {
            return Err(ToolError::Validation("`command` must be non-empty".into()));
        }
        if let Some(pattern) = detect_file_write_anti_pattern(&input.command) {
            // Reject the heredoc-file-write pattern with a precise redirect
            // so the next turn picks the right tool. The body of these
            // commands is usually multiple KB of file content that would
            // otherwise live in history as a shell command string. The
            // dedicated tools (write_file / edit_file / etc.) take it as
            // a clean `content` field that doesn't get echoed back in
            // the tool result.
            return Err(ToolError::Validation(format!(
                "rejected: `{pattern}` writes a file through the shell command string, \
                 which is captured into conversation history twice. Use `write_file` for a new file, \
                 `edit_file` or `multi_edit` or `apply_patch` to modify, or `copy_file` to duplicate. \
                 Re-issue with one of those tools instead of the heredoc pattern."
            )));
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
    //!
    //! Supports two access modes:
    //!   - `bytes` (default): seek to `offset` (0-indexed) and read up
    //!     to `min(limit, READ_FILE_CAP)` bytes. Useful for binary-ish
    //!     spelunking and for "give me 8 KB starting at 1 MB" reads of
    //!     large logs.
    //!   - `lines`: 1-indexed line range. `offset = 1, limit = 50`
    //!     reads the first 50 lines; `offset = 200, limit = 100` reads
    //!     lines 200–299. The line-count cap still applies via
    //!     `READ_FILE_CAP` — bytes accumulated past the cap stop the
    //!     read and flip `truncated`.
    //!
    //! Out-of-range offsets return an empty body with `truncated: false`,
    //! not an error — the agent gets a clean signal that it walked off
    //! the end. Negative offsets are rejected at parse time; for
    //! tail-style reads the agent should use `shell tail -n N`.

    use super::{
        json, parse_args, success_json, CallToolResult, Deserialize, JsonObject,
        PathBuf, Tool, ToolEntry, ToolError, ToolHandler, READ_FILE_CAP,
    };
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

    /// Default behaviour when neither `offset` nor `limit` is given:
    /// keep the historical "read from start, cap at `READ_FILE_CAP`"
    /// shape so existing callers don't change.
    #[derive(Debug, Deserialize)]
    struct Input {
        path: String,
        /// 0-indexed byte offset (`bytes` mode) or 1-indexed line
        /// number (`lines` mode). Negative values are rejected — see
        /// [`parse_input`].
        #[serde(default)]
        offset: Option<i64>,
        /// Number of bytes (`bytes` mode) or lines (`lines` mode) to
        /// read. Bytes are still clamped to `READ_FILE_CAP` regardless.
        #[serde(default)]
        limit: Option<i64>,
        /// Either `"bytes"` (default) or `"lines"`. Anything else is
        /// rejected as a Validation error.
        #[serde(default)]
        mode: Option<String>,
    }

    /// What [`handle`] decided to do after validating the user's input.
    struct Plan {
        path: PathBuf,
        offset: u64,
        limit: u64,
        mode: Mode,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Bytes,
        Lines,
    }

    impl Mode {
        fn as_str(self) -> &'static str {
            match self {
                Self::Bytes => "bytes",
                Self::Lines => "lines",
            }
        }
    }

    pub fn schema() -> Tool {
        super::make_tool(
            "read_file",
            "Read a UTF-8 file. Default reads from the start, capped at 128 KiB. For larger files or precise regions, use `offset` + `limit` + `mode: 'bytes' | 'lines'`. Examples: `{path:'x.log', mode:'lines', offset:200, limit:100}` reads lines 200-299; `{path:'x.log', mode:'bytes', offset:1000000, limit:8000}` reads 8 KB starting at byte 1 MB. Tail-reads aren't supported — use `shell tail -n N` for those.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path":   { "type": "string", "minLength": 1 },
                    "offset": { "type": ["integer", "null"], "minimum": 0 },
                    "limit":  { "type": ["integer", "null"], "minimum": 0 },
                    "mode":   { "type": ["string", "null"], "enum": ["bytes", "lines", null] }
                }
            }),
        )
    }

    /// Translate `Input` into a validated `Plan`. Centralises the
    /// "what does no-args mean", "what counts as a valid mode", and
    /// "is the limit clamped to the cap" logic so [`handle`] stays
    /// focused on I/O.
    fn parse_input(input: &Input) -> Result<Plan, ToolError> {
        // Reject negatives up front; we can't represent them as u64.
        // `unsigned_abs` after the check is safe and avoids the
        // sign-loss cast clippy lints.
        let raw_offset = match input.offset {
            Some(o) if o < 0 => {
                return Err(ToolError::Validation(
                    "`offset` must be >= 0 (tail-reads not supported; use `shell tail -n N`)"
                        .into(),
                ));
            }
            Some(o) => o.unsigned_abs(),
            None => 0,
        };
        let raw_limit = match input.limit {
            Some(l) if l < 0 => {
                return Err(ToolError::Validation("`limit` must be >= 0".into()));
            }
            Some(l) => Some(l.unsigned_abs()),
            None => None,
        };
        let mode = match input.mode.as_deref() {
            None | Some("bytes") => Mode::Bytes,
            Some("lines") => Mode::Lines,
            Some(other) => {
                return Err(ToolError::Validation(format!(
                    "`mode` must be \"bytes\" or \"lines\" (got `{other}`)"
                )));
            }
        };
        let limit = match (mode, raw_limit) {
            // bytes mode: missing limit means "as much as the cap allows"
            (Mode::Bytes, None) => READ_FILE_CAP as u64,
            (Mode::Bytes, Some(l)) => l.min(READ_FILE_CAP as u64),
            // lines mode: missing limit means "until cap"; cap still
            // bounds total bytes accumulated, not the line count.
            (Mode::Lines, None) => u64::MAX,
            (Mode::Lines, Some(l)) => l,
        };
        Ok(Plan {
            path: PathBuf::from(&input.path),
            offset: raw_offset,
            limit,
            mode,
        })
    }

    /// Read a byte-range slice of the file. `truncated` flips on when
    /// we hit `READ_FILE_CAP` and there's still file past the read
    /// window. Out-of-range offsets short-circuit to an empty body
    /// with `truncated: false` — the agent walked off the end, that's
    /// not an error.
    async fn read_bytes_range(
        path: &std::path::Path,
        offset: u64,
        limit: u64,
    ) -> Result<(String, bool, u64), ToolError> {
        let mut f = tokio::fs::File::open(path).await.map_err(|e| {
            ToolError::Internal(format!("read_file({}): {e}", path.display()))
        })?;
        let len = f
            .metadata()
            .await
            .map_err(|e| {
                ToolError::Internal(format!(
                    "read_file({}) stat: {e}",
                    path.display()
                ))
            })?
            .len();
        if offset >= len {
            return Ok((String::new(), false, 0));
        }
        f.seek(SeekFrom::Start(offset)).await.map_err(|e| {
            ToolError::Internal(format!(
                "read_file({}) seek: {e}",
                path.display()
            ))
        })?;
        // Cap reads at READ_FILE_CAP regardless of caller's `limit` to
        // protect the context window.
        let cap = limit.min(READ_FILE_CAP as u64);
        let available = len.saturating_sub(offset);
        let to_read = cap.min(available);
        #[allow(clippy::cast_possible_truncation)]
        let mut buf = vec![0u8; to_read as usize];
        f.read_exact(&mut buf).await.map_err(|e| {
            ToolError::Internal(format!(
                "read_file({}) read: {e}",
                path.display()
            ))
        })?;
        // `truncated` reflects "there was more we didn't return". If
        // the caller's limit was higher than the cap and the file had
        // more bytes past the cap, flag it.
        let truncated = to_read < available;
        // Decode as lossy UTF-8 so a mid-multibyte slice still
        // produces valid text (replacement char in the worst case).
        let body = String::from_utf8_lossy(&buf).into_owned();
        Ok((body, truncated, to_read))
    }

    /// Read a 1-indexed line range. The cap on accumulated bytes
    /// still applies — a runaway line-grab on a giant file stops at
    /// `READ_FILE_CAP` with `truncated: true`. `offset = 0` is
    /// treated as `1` for ergonomics (1-indexed but agents will mix
    /// it up).
    async fn read_lines_range(
        path: &std::path::Path,
        offset: u64,
        limit: u64,
    ) -> Result<(String, bool, u64), ToolError> {
        // Read the whole file up to the cap+1 so we can detect overflow.
        // For very large files this is the same memory pressure as the
        // existing implementation — we don't make it worse.
        let bytes = tokio::fs::read(path).await.map_err(|e| {
            ToolError::Internal(format!("read_file({}): {e}", path.display()))
        })?;
        let text = String::from_utf8_lossy(&bytes);
        // Normalise offset: 1-indexed, but accept 0 as "from the start".
        let start_line = offset.max(1);
        let mut out = String::new();
        let mut count: u64 = 0;
        let mut emitted_lines: u64 = 0;
        // `split_inclusive` keeps the trailing newline on each chunk
        // so we faithfully reconstruct the source slice.
        let mut current_line: u64 = 0;
        let mut truncated = false;
        for line in text.split_inclusive('\n') {
            current_line += 1;
            if current_line < start_line {
                continue;
            }
            if emitted_lines >= limit {
                // We had more file past the requested window.
                truncated = true;
                break;
            }
            let line_bytes = line.len() as u64;
            if count + line_bytes > READ_FILE_CAP as u64 {
                // Cap hit mid-line; flip truncated and stop. We do not
                // emit a partial line — keeping the body line-aligned
                // is more useful to the model than maxing out bytes.
                truncated = true;
                break;
            }
            out.push_str(line);
            count += line_bytes;
            emitted_lines += 1;
        }
        // If the loop exhausted the file without filling `limit`, the
        // body is exactly what the caller asked for — not truncated.
        Ok((out, truncated, count))
    }

    pub async fn handle(
        arguments: Option<JsonObject>,
        _ctx: &dyn crate::context::ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        let input: Input = parse_args(arguments)?;
        let plan = parse_input(&input)?;

        // For `bytes` mode + default path (offset=0, no caller-supplied
        // limit beyond the cap) we preserve the pre-existing read-the-
        // whole-file-and-truncate-the-head shape. Tests pin this.
        let (body, truncated, bytes_read) = match plan.mode {
            Mode::Bytes => {
                read_bytes_range(&plan.path, plan.offset, plan.limit).await?
            }
            Mode::Lines => {
                read_lines_range(&plan.path, plan.offset, plan.limit).await?
            }
        };

        // `limit_applied` is the cap actually enforced for this call.
        // Bytes mode clamps to READ_FILE_CAP at parse time; lines mode
        // surfaces the caller's limit (or `null` when uncapped).
        let limit_applied: Value = match plan.mode {
            Mode::Bytes => Value::Number(plan.limit.into()),
            Mode::Lines => {
                if plan.limit == u64::MAX {
                    Value::Null
                } else {
                    Value::Number(plan.limit.into())
                }
            }
        };

        Ok(success_json(&json!({
            "path": plan.path.display().to_string(),
            "body": body,
            "truncated": truncated,
            "bytes_read": bytes_read,
            "offset": plan.offset,
            "limit_applied": limit_applied,
            "mode": plan.mode.as_str(),
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
    use crate::tools::diff_util::{build_blob_card, build_diff_card, over_blob_cutoff};

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
        ctx: &dyn crate::context::ToolContext,
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
        // Snapshot the pre-write content when this is an *overwrite*
        // (not append, not first write) and the file is small enough
        // to diff safely. The blob-cutoff check is on the new content
        // size too — if the model is overwriting a 50 KB source with
        // a 5 MB binary blob we don't want to read either side.
        // `before_content` is None when (a) `append == true`, (b) the
        // file didn't exist, or (c) either side is over the blob
        // cutoff. The diff-card emit path uses that signal directly.
        let new_size = input.content.len() as u64;
        let new_content = input.content.clone();
        let bytes = input.content.into_bytes();
        let before_content: Option<(String, u64)> = if input.append {
            None
        } else {
            match tokio::fs::metadata(&path).await {
                Ok(m) if m.is_file() => {
                    let prev_size = m.len();
                    if over_blob_cutoff(prev_size, new_size) {
                        // Defer to the BlobReplaced path below.
                        Some((String::new(), prev_size))
                    } else {
                        tokio::fs::read_to_string(&path).await.ok().map(|s| (s, prev_size))
                    }
                }
                _ => None,
            }
        };
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
        // Diff-card emit happens only for non-append overwrites where
        // we snapshotted the prior content (or stubbed it under the
        // blob-cutoff path). Pure new-file writes and append-mode
        // writes intentionally skip the card — there is no "before"
        // worth showing in either case.
        if let Some((before, before_size)) = before_content {
            let display = path.display().to_string();
            if over_blob_cutoff(before_size, new_size) {
                ctx.emit_diff(build_blob_card(&display, before_size, new_size))
                    .await;
            } else if let Some(card) = build_diff_card(&display, &before, &new_content) {
                ctx.emit_diff(card).await;
            }
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
            "Fetch an HTTP(S) URL. Defaults to GET; pass `method: POST` and `body` for posts. HTML responses are automatically converted to markdown to save context — pass `raw: true` to receive the original bytes. **Response body is capped at 16 KiB (~4k tokens)** to keep one fetch from eating an entire subagent's budget; if the response was truncated, `truncated: true` is set in the result. For full-page extraction beyond the cap, use `shell` with `curl` and pipe through `head -c` / `grep` for the specific section you need. Returns `status`, `content_type`, `size_bytes`, `truncated`, and `body`; full response headers are not surfaced — for those use `shell` with `curl -I`.",
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
        // Pull Content-Type before consuming the body — once we call
        // `bytes()` the response is moved. We intentionally do NOT
        // surface the full response headers map: tool results live in
        // history forever and a single CSP / Set-Cookie header can be
        // tens of KiB. The model cares about status + content-type;
        // anything more should go through `shell` with `curl -I`.
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
            "content_type": content_type_header,
            "size_bytes": raw_bytes,
            "truncated": truncated,
            "body": body,
            "elapsed_ms": started.elapsed().as_millis(),
        });
        if let Some(md_len) = conversion.markdown_bytes {
            if let Some(map) = out.as_object_mut() {
                // Overwrite the raw Content-Type with the conversion
                // marker so callers can see at a glance that the body
                // was transformed.
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
/// `(capped_string, truncated_flag)`. The trailing hint nudges the
/// model toward narrowing the source (`tail -n 200`, `head -n 200`,
/// `grep`) rather than re-running the same overflowing command.
fn cap_output(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut cap = max;
    while !s.is_char_boundary(cap) {
        cap -= 1;
    }
    (
        format!(
            "{}…[truncated; narrow with tail/head/grep before re-running]",
            &s[..cap]
        ),
        true,
    )
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
    fn shell_detects_cat_heredoc_file_write() {
        let cases = [
            "cat > /data/app.css << 'CSSEOF'\nbody{}\nCSSEOF",
            "cat > /data/x.txt <<EOF\nhi\nEOF",
            "cat >> /data/log <<TAG\nappend\nTAG",
            "tee /tmp/foo <<'END'\nhi\nEND",
            "tee -a /tmp/foo << END\nhi\nEND",
            "cat <<EOF > /data/x.css\n/*style*/\nEOF",
        ];
        for cmd in cases {
            assert!(
                shell::detect_file_write_anti_pattern(cmd).is_some(),
                "expected detection on: {cmd:?}"
            );
        }
    }

    #[test]
    fn shell_does_not_flag_legitimate_uses() {
        let cases = [
            "ls -la /data",
            "cat /data/app.css",                       // reading, no redirect
            "echo hello > /tmp/x",                      // echo redirect — intentionally not blocked
            "grep -r foo .",
            "tar -czf out.tgz /data",
            "diff -u a.txt b.txt",
            "find . -name '*.rs' -exec wc -l {} \\;",   // contains '>' but inside -exec
        ];
        for cmd in cases {
            assert!(
                shell::detect_file_write_anti_pattern(cmd).is_none(),
                "false positive on: {cmd:?}"
            );
        }
    }

    #[tokio::test]
    async fn shell_rejects_cat_heredoc_with_redirect_message() {
        use rmcp::model::JsonObject;
        let mut args = JsonObject::new();
        args.insert(
            "command".into(),
            serde_json::Value::String(
                "cat > /data/style.css << 'EOF'\nbody{color:red}\nEOF".into(),
            ),
        );
        let ctx = crate::context::MockToolContext::new();
        let err = shell::handle(Some(args), &ctx).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("write_file"), "must redirect to write_file: {msg}");
        assert!(msg.contains("cat > FILE"), "must name the pattern: {msg}");
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
    async fn web_fetch_omits_headers_map_and_surfaces_content_type_scalar() {
        // Regression guard for the response-shape audit: a single CSP
        // header can be tens of KiB, so the full headers map must NOT
        // appear in the tool output. Status + content_type as scalars
        // are the supported surface area.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("x-custom-bloat", "a".repeat(2048))
                    .set_body_raw(b"{\"ok\":true}".as_ref(), "application/json"),
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
        // The headers map (and our 2 KiB bloat header) must not leak.
        assert!(!body.contains("\"headers\""), "headers leaked: {body}");
        assert!(!body.contains("x-custom-bloat"), "header bloat leaked: {body}");
        // But the scalar content_type must be present.
        assert!(
            body.contains("\"content_type\": \"application/json\""),
            "missing content_type: {body}"
        );
        // And status must remain.
        assert!(body.contains("\"status\": 200"), "missing status: {body}");
    }

    #[tokio::test]
    async fn web_fetch_caps_body_at_16k() {
        // Regression guard: the cap is 16 KiB (lowered from 32 on
        // 2026-05-24 — see `WEB_FETCH_CAP` rationale). A 32 KiB JSON
        // payload must come back truncated.
        let server = wiremock::MockServer::start().await;
        let big = "x".repeat(32 * 1024);
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/big"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(big.as_bytes(), "text/plain"),
            )
            .mount(&server)
            .await;
        let res = web_fetch::handle(
            Some(
                json!({"url": format!("{}/big", server.uri())})
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
        // The raw size header should still report the full 32 KiB.
        assert!(body.contains("\"size_bytes\": 32768"), "got: {body}");
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

    /// Append-mode writes intentionally skip the diff-card emit —
    /// there's no meaningful "before" to compare against (we're
    /// adding bytes, not editing them) and the breadcrumb already
    /// tells the user *what tool* ran.
    #[tokio::test]
    async fn write_file_append_does_not_emit_diff_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        let mock = std::sync::Arc::new(crate::context::MockToolContext::new());
        write_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "content": "hello\n",
                    "append": true,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            mock.as_ref(),
        )
        .await
        .unwrap();
        assert!(mock.diff_calls().is_empty());
    }

    /// First-write (target doesn't exist) intentionally skips the
    /// diff-card emit — there's no "before" file to diff against.
    #[tokio::test]
    async fn write_file_first_write_does_not_emit_diff_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.rs");
        let mock = std::sync::Arc::new(crate::context::MockToolContext::new());
        write_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "content": "fn main() {}\n",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            mock.as_ref(),
        )
        .await
        .unwrap();
        assert!(mock.diff_calls().is_empty());
    }

    /// Overwriting an existing file with new content emits exactly
    /// one DiffCard via `ctx.emit_diff` carrying the structured diff
    /// of before-vs-after.
    #[tokio::test]
    async fn write_file_overwrite_emits_diff_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn old() {}\n").unwrap();

        let mock = std::sync::Arc::new(crate::context::MockToolContext::new());
        write_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "content": "fn new() {}\n",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            mock.as_ref(),
        )
        .await
        .unwrap();
        let diffs = mock.diff_calls();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].added, 1);
        assert_eq!(diffs[0].removed, 1);
        assert_eq!(diffs[0].language.as_deref(), Some("rust"));
    }

    /// Overwriting a > 256 KB blob trips the cutoff and emits a
    /// `BlobReplaced` summary card (rendered as a single-line
    /// `DiffCard` with `truncated = true`) instead of a full diff.
    #[tokio::test]
    async fn write_file_overwrite_over_blob_cutoff_emits_blob_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let big = "x".repeat(300 * 1024);
        std::fs::write(&path, &big).unwrap();

        let mock = std::sync::Arc::new(crate::context::MockToolContext::new());
        let new_content = "y".repeat(300 * 1024);
        write_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "content": new_content,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            mock.as_ref(),
        )
        .await
        .unwrap();
        let diffs = mock.diff_calls();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].truncated);
        assert_eq!(diffs[0].added, 0);
        assert_eq!(diffs[0].removed, 0);
        let summary = &diffs[0].hunks[0].lines[0].text;
        assert!(summary.contains("diff suppressed"), "got: {summary}");
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

    /// Default behaviour (no offset/limit/mode): response shape echoes
    /// `mode: "bytes"`, `offset: 0`, and `limit_applied` clamps to the
    /// cap. Regression guard for callers that depend on the
    /// unchanged-by-default contract.
    #[tokio::test]
    async fn read_file_default_behavior_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"abcdef\n").await.unwrap();
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
        assert!(body.contains("\"truncated\": false"), "got: {body}");
        assert!(body.contains("\"mode\": \"bytes\""), "got: {body}");
        assert!(body.contains("\"offset\": 0"), "got: {body}");
        // limit_applied should be the cap (131072 = 128 KiB)
        assert!(
            body.contains("\"limit_applied\": 131072"),
            "got: {body}"
        );
        assert!(body.contains("\"bytes_read\": 7"), "got: {body}");
        assert!(body.contains("abcdef"), "got: {body}");
    }

    /// `mode: "bytes"` with offset+limit reads exactly that byte range.
    #[tokio::test]
    async fn read_file_bytes_mode_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bytes.bin");
        // 0123456789abcdef (16 bytes)
        tokio::fs::write(&path, b"0123456789abcdef").await.unwrap();
        let res = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "mode": "bytes",
                    "offset": 4,
                    "limit": 5,
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
        assert!(body.contains("\"body\": \"45678\""), "got: {body}");
        assert!(body.contains("\"bytes_read\": 5"), "got: {body}");
        assert!(body.contains("\"offset\": 4"), "got: {body}");
        assert!(body.contains("\"limit_applied\": 5"), "got: {body}");
        assert!(body.contains("\"mode\": \"bytes\""), "got: {body}");
        // 16 - (4+5) = 7 bytes still on disk; truncated must flip on.
        assert!(body.contains("\"truncated\": true"), "got: {body}");
    }

    /// `mode: "lines"` with offset+limit reads the exact 1-indexed
    /// line range — `offset:2, limit:3` returns lines 2, 3, 4.
    #[tokio::test]
    async fn read_file_lines_mode_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.txt");
        tokio::fs::write(
            &path,
            b"line1\nline2\nline3\nline4\nline5\nline6\n",
        )
        .await
        .unwrap();
        let res = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "mode": "lines",
                    "offset": 2,
                    "limit": 3,
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
        assert!(body.contains("line2"), "got: {body}");
        assert!(body.contains("line3"), "got: {body}");
        assert!(body.contains("line4"), "got: {body}");
        assert!(!body.contains("line1"), "leaked line1: {body}");
        assert!(!body.contains("line5"), "leaked line5: {body}");
        assert!(body.contains("\"mode\": \"lines\""), "got: {body}");
        assert!(body.contains("\"offset\": 2"), "got: {body}");
        assert!(body.contains("\"limit_applied\": 3"), "got: {body}");
        // There were lines past the window, so truncated flips on.
        assert!(body.contains("\"truncated\": true"), "got: {body}");
    }

    /// Reading the final lines exactly (no overflow) should NOT flag
    /// truncated. Distinguishes "you got everything you asked for"
    /// from "I had to stop early".
    #[tokio::test]
    async fn read_file_lines_mode_exact_tail_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.txt");
        tokio::fs::write(&path, b"a\nb\nc\n").await.unwrap();
        let res = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "mode": "lines",
                    "offset": 2,
                    "limit": 2,
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
        assert!(body.contains("\"truncated\": false"), "got: {body}");
    }

    /// Out-of-range offset returns empty body with `truncated: false`
    /// — agent walking off the end is a clean signal, not an error.
    #[tokio::test]
    async fn read_file_offset_beyond_eof_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();
        let res = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "mode": "bytes",
                    "offset": 1_000_000,
                    "limit": 100,
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
        assert!(body.contains("\"body\": \"\""), "got: {body}");
        assert!(body.contains("\"truncated\": false"), "got: {body}");
        assert!(body.contains("\"bytes_read\": 0"), "got: {body}");
    }

    /// `limit` above READ_FILE_CAP must be clamped to the cap so a
    /// careless caller can't blow the context window.
    #[tokio::test]
    async fn read_file_limit_clamped_to_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        // Just past the cap so the clamp is observable.
        let big = "a".repeat(200 * 1024);
        tokio::fs::write(&path, &big).await.unwrap();
        let res = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "mode": "bytes",
                    "offset": 0,
                    "limit": 10_000_000_u64,
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
        // The cap (131072 = 128 KiB) wins over the caller's 10M.
        assert!(
            body.contains("\"limit_applied\": 131072"),
            "got: {body}"
        );
        assert!(body.contains("\"bytes_read\": 131072"), "got: {body}");
        assert!(body.contains("\"truncated\": true"), "got: {body}");
    }

    /// Negative offset is rejected at parse time with a Validation
    /// error — tail-reads aren't supported via `offset`.
    #[tokio::test]
    async fn read_file_negative_offset_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();
        let err = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "offset": -1,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got: {err:?}"
        );
    }

    /// Negative limit is also rejected.
    #[tokio::test]
    async fn read_file_negative_limit_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();
        let err = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "limit": -5,
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got: {err:?}"
        );
    }

    /// Unknown `mode` value is rejected with a Validation error so
    /// the agent gets an immediate signal rather than silent fallback.
    #[tokio::test]
    async fn read_file_unknown_mode_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt");
        tokio::fs::write(&path, b"hi").await.unwrap();
        let err = read_file::handle(
            Some(
                json!({
                    "path": path.to_string_lossy(),
                    "mode": "chars",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
            ctx().as_ref(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ToolError::Validation(_)),
            "expected Validation, got: {err:?}"
        );
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
        // The trailing hint must mention a narrowing tool so the model
        // knows the recovery path.
        assert!(got.contains("truncated"), "got: {got}");
        assert!(
            got.contains("tail") || got.contains("head") || got.contains("grep"),
            "missing narrowing hint: {got}"
        );
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
