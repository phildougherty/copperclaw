//! `artifact_path`: tell the agent (and through it, the operator) where
//! files written into `/data` end up on the host filesystem.
//!
//! Closes the "I built it but nobody can find it" delivery gap. When
//! the agent finishes producing artifacts, it calls this and includes
//! the returned host path in its reply so the operator can `cd` to it
//! or open files directly. Complements `send_file` (which pushes
//! artifacts through the channel adapter) — pick whichever is more
//! appropriate for the artifact size and the channel's capabilities.
//!
//! Discovery: the runner writes the resolved host path into
//! `/data/.host_path` at startup (see `runner/state.rs` for the
//! producer). This tool just reads that file. If the file's absent
//! the tool returns an instructive error rather than guessing.

use std::path::PathBuf;

use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde_json::json;

use crate::context::ToolContext;
use crate::error::ToolError;
use crate::tools::{make_tool, success_json, ToolEntry, ToolHandler};

/// Default location of the host-path discovery file inside the container.
const HOST_PATH_FILE_DEFAULT: &str = "/data/.host_path";
const HOST_PATH_FILE_ENV_OVERRIDE: &str = "COPPERCLAW_HOST_PATH_FILE";

#[cfg(test)]
static HOST_PATH_FILE_TEST_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn host_path_file_test_override_set(path: PathBuf) {
    let cell = HOST_PATH_FILE_TEST_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
}

#[cfg(test)]
pub(super) fn host_path_file_test_override_clear() {
    if let Some(cell) = HOST_PATH_FILE_TEST_OVERRIDE.get() {
        *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
fn host_path_file_test_override() -> Option<PathBuf> {
    HOST_PATH_FILE_TEST_OVERRIDE.get().and_then(|m| {
        m.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    })
}

#[cfg(not(test))]
fn host_path_file_test_override() -> Option<PathBuf> {
    None
}

fn host_path_file() -> PathBuf {
    if let Some(p) = host_path_file_test_override() {
        return p;
    }
    std::env::var_os(HOST_PATH_FILE_ENV_OVERRIDE)
        .map_or_else(|| PathBuf::from(HOST_PATH_FILE_DEFAULT), PathBuf::from)
}

pub fn schema() -> Tool {
    make_tool(
        "artifact_path",
        "Return the host-side path corresponding to the container's `/data` directory. Anything you wrote under `/data/*` lives at `<host_path>/<same-relative-name>` on the operator's machine. Include this path in your reply to the operator so they can `cd` to it or open files directly. Complement to `send_file` (which pushes artifacts through the channel).",
        json!({ "type": "object", "additionalProperties": false, "properties": {} }),
    )
}

pub async fn handle(
    _arguments: Option<JsonObject>,
    _ctx: &dyn ToolContext,
) -> Result<CallToolResult, ToolError> {
    let path = host_path_file();
    let bytes = tokio::fs::read(&path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            ToolError::Internal(format!(
                "artifact_path: discovery file {} not found — the host has not written it for this session. If you're a development run, the operator can find your files under their `~/.local/share/copperclaw/data/sessions/` tree.",
                path.display()
            ))
        } else {
            ToolError::Internal(format!("artifact_path: read {}: {err}", path.display()))
        }
    })?;
    let host_path = String::from_utf8(bytes)
        .map_err(|err| ToolError::Internal(format!("artifact_path: utf8: {err}")))?
        .trim()
        .to_string();
    Ok(success_json(&json!({
        "container_path": "/data",
        "host_path": host_path,
        "note": "Files you write under /data/* appear at <host_path>/<same-relative-name> on the operator's machine. Include the host_path in your reply so the operator can find what you built.",
    })))
}

struct Handler;
#[async_trait::async_trait]
impl ToolHandler for Handler {
    async fn call(
        &self,
        arguments: Option<JsonObject>,
        ctx: &dyn ToolContext,
    ) -> Result<CallToolResult, ToolError> {
        handle(arguments, ctx).await
    }
}

pub fn entry() -> ToolEntry {
    ToolEntry { tool: schema(), handler: Box::new(Handler) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MockToolContext;

    #[tokio::test]
    async fn returns_host_path_from_discovery_file() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join(".host_path");
        std::fs::write(&f, "/home/phil/projects/foo").unwrap();
        host_path_file_test_override_set(f);
        let ctx = MockToolContext::new();
        let result = handle(None, &ctx).await.unwrap();
        let text = result
            .content
            .iter()
            .filter_map(|c| {
                let v = serde_json::to_value(c).ok()?;
                v.get("text").and_then(|t| t.as_str()).map(str::to_string)
            })
            .collect::<String>();
        assert!(text.contains("/home/phil/projects/foo"));
        assert!(text.contains("/data"));
        host_path_file_test_override_clear();
    }

    #[tokio::test]
    async fn error_when_discovery_file_missing() {
        let td = tempfile::tempdir().unwrap();
        host_path_file_test_override_set(td.path().join("nope"));
        let ctx = MockToolContext::new();
        let err = handle(None, &ctx).await.unwrap_err();
        match err {
            ToolError::Internal(m) => {
                assert!(m.contains("not found"));
                assert!(m.contains("sessions/"));
            }
            other => panic!("expected Internal, got {other:?}"),
        }
        host_path_file_test_override_clear();
    }
}
