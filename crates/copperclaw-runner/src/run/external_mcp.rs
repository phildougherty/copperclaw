//! Runner side of host-proxied external MCP tool calls.
//!
//! The host writes `<session_dir>/mcp_tools.json` at spawn — the filter-
//! stripped set of tools every configured external MCP server advertises,
//! each tagged with its owning `server`. This module:
//!
//! 1. [`load_external_tools`] — reads that manifest at startup and turns it
//!    into the `ToolDef`s the runner advertises to the provider (namespaced
//!    `mcp__<server>__<tool>` so they never collide with first-party tools)
//!    plus a route table the dispatcher uses.
//! 2. [`dispatch_external`] — when the model calls one of those tools, writes
//!    a request row to `outbound.db` and blocks-polls `inbound.db` for the
//!    host's response (the host holds the live connection, the credentials,
//!    and the per-server filter, and executes the call host-side so the
//!    container stays sandboxed under deny-default egress).
//!
//! The request lives in `outbound.db` (runner-written) and the response in
//! `inbound.db` (host-written), preserving the single-writer-per-bind-mounted-
//! DB invariant. External MCP output is attacker-influenceable, so a completed
//! call taints the turn ([`ToolContext::mark_untrusted_context`]) exactly like
//! a `web_fetch` body — the dispatch gate then blocks any later credentialed
//! external action absent fresh approval.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use copperclaw_db::tables::mcp_calls::{self, McpCallRequest};
use copperclaw_providers::ToolDef;
use serde::Deserialize;
use tokio::time::sleep;

use super::RunnerDeps;
use super::tool_dispatch::ToolImage;
use crate::policy::EXTERNAL_MCP_PREFIX;

/// How long the runner waits for the host to answer an external MCP call
/// before giving up and returning an error `tool_result`. Bounded well under any
/// reasonable interactive expectation; a wedged host/server can't stall the
/// turn forever. The host's active loop answers within ~1s in the normal case.
pub const EXTERNAL_MCP_DEADLINE_SECS: u64 = 120;

/// Poll cadence (ms) while waiting on the host's response row.
const EXTERNAL_MCP_POLL_MS: u64 = 200;

/// Routing for one advertised external MCP tool: the owning server and the
/// real remote tool name (the advertised name is `mcp__<server>__<tool>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalToolRoute {
    /// The configured external server name (the `mcp_servers` key).
    pub server: String,
    /// The remote tool name, stripped of the `mcp__<server>__` prefix.
    pub tool: String,
}

/// One entry in `mcp_tools.json` (written by the host's spawn seam).
#[derive(Debug, Deserialize)]
struct ManifestEntry {
    server: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: serde_json::Value,
}

/// Build the advertised name for a remote tool: `mcp__<server>__<tool>`.
#[must_use]
pub fn advertised_name(server: &str, tool: &str) -> String {
    format!("{EXTERNAL_MCP_PREFIX}{server}__{tool}")
}

/// Read `<session_dir>/mcp_tools.json` and produce the provider `ToolDef`s to
/// advertise plus the advertised-name → route table for dispatch.
///
/// A missing or malformed manifest yields empty sets (no external tools) —
/// never an error: external MCP is purely additive and a broken manifest must
/// not wedge the runner. The host already filter-stripped denied tools before
/// writing the manifest, so everything here is permitted to advertise.
#[must_use]
pub fn load_external_tools(
    session_dir: &Path,
) -> (Vec<ToolDef>, HashMap<String, ExternalToolRoute>) {
    let path = session_dir.join(copperclaw_host_mcp_tools_filename());
    // No manifest = no external tools (the common case for groups with no
    // configured external MCP servers).
    let Ok(bytes) = std::fs::read(&path) else {
        return (Vec::new(), HashMap::new());
    };
    let Ok(entries) = serde_json::from_slice::<Vec<ManifestEntry>>(&bytes) else {
        tracing::warn!(
            path = %path.display(),
            "could not parse mcp_tools.json; no external MCP tools this session"
        );
        return (Vec::new(), HashMap::new());
    };
    let mut defs = Vec::with_capacity(entries.len());
    let mut routes = HashMap::with_capacity(entries.len());
    for entry in entries {
        let advertised = advertised_name(&entry.server, &entry.name);
        defs.push(ToolDef {
            name: advertised.clone(),
            description: entry.description.unwrap_or_default(),
            input_schema: entry.input_schema,
        });
        routes.insert(
            advertised,
            ExternalToolRoute {
                server: entry.server,
                tool: entry.name,
            },
        );
    }
    (defs, routes)
}

/// The manifest filename the host writes. Kept as a local const so the runner
/// does not depend on `copperclaw-host` (which would be a dependency cycle).
fn copperclaw_host_mcp_tools_filename() -> &'static str {
    "mcp_tools.json"
}

/// Execute one external MCP tool call host-proxied: write the request to
/// `outbound.db`, block-poll `inbound.db` for the host's response (bounded by
/// [`EXTERNAL_MCP_DEADLINE_SECS`]), taint the turn, and render the result into
/// the `(content, images, is_error)` triple `invoke_tool` returns.
///
/// Images are not surfaced for external MCP tools in this version — the
/// response is the host-rendered text; an image-bearing remote result is noted
/// as `<image>` host-side. The empty image vec keeps the signature uniform.
pub(super) async fn dispatch_external(
    deps: &RunnerDeps,
    route: &ExternalToolRoute,
    advertised: &str,
    input: serde_json::Value,
) -> (String, Vec<ToolImage>, bool) {
    let request_id = uuid::Uuid::new_v4().to_string();
    let req = McpCallRequest {
        request_id: request_id.clone(),
        server: route.server.clone(),
        tool: route.tool.clone(),
        input,
    };

    // 1. Queue the request (runner is the sole writer of outbound.db).
    {
        let conn = deps.outbound.lock().await;
        if let Err(err) = mcp_calls::insert_request(&conn, &req) {
            return (
                format!("Could not queue external MCP call `{advertised}`: {err}"),
                Vec::new(),
                true,
            );
        }
    }

    // 2. Block-poll inbound.db for the host's response, bounded by the deadline.
    let outcome = poll_for_response(
        &deps.inbound,
        &request_id,
        Duration::from_secs(EXTERNAL_MCP_DEADLINE_SECS),
    )
    .await;

    // 3. GC the request row now that we have an answer (or errored) — the
    //    runner owns outbound.db, and removing it lets the host GC the
    //    matching response.
    {
        let conn = deps.outbound.lock().await;
        let _ = mcp_calls::delete_request(&conn, &request_id);
    }

    // X1: for the reserved preview relay, count host-answered (served) vs the
    // runner's blocking-poll giving up (timeout).
    if route.server == crate::run::preview::PREVIEW_SERVER {
        match &outcome {
            Ok(Some(_)) => copperclaw_metrics::inc_preview_expose("served"),
            Ok(None) => copperclaw_metrics::inc_preview_expose("timeout"),
            Err(_) => {}
        }
    }

    let resp = match outcome {
        Ok(Some(resp)) => resp,
        Ok(None) => timeout_response(&request_id, advertised),
        Err(err) => {
            return (
                format!("External MCP call `{advertised}` failed reading the host response: {err}"),
                Vec::new(),
                true,
            );
        }
    };

    // 4. Taint the turn: external MCP output is attacker-influenceable, so a
    //    later credentialed external action must trip the provenance gate
    //    (same treatment as a web_fetch body). Conservative: taint on any
    //    received response — over-restriction here only fails safe.
    //
    //    Exception: the reserved `__preview` relay (M17 session-preview tools)
    //    is answered by the HOST's preview broker, not a remote server — the
    //    response (URL + fixed note, or a host-composed error) carries no
    //    attacker-influenceable content, and tainting it would block the
    //    paired same-turn `close_preview` (itself credentialed-external).
    if route.server != crate::run::preview::PREVIEW_SERVER {
        deps.tool_ctx
            .mark_untrusted_context(&format!("mcp:{}:{}", route.server, route.tool));
    }

    (resp.result, Vec::new(), resp.is_error)
}

/// Poll `inbound.db` for the response to `request_id`, returning `Ok(Some)`
/// when the host answers, `Ok(None)` when `deadline` elapses first, or `Err`
/// on a genuine DB read failure (a not-yet-migrated inbound DB reads as "no
/// response yet", not an error — see [`mcp_calls::get_response`]). Factored out
/// of [`dispatch_external`] so the timeout branch is testable with a short
/// deadline instead of the production [`EXTERNAL_MCP_DEADLINE_SECS`].
async fn poll_for_response(
    inbound: &tokio::sync::Mutex<rusqlite::Connection>,
    request_id: &str,
    deadline: Duration,
) -> Result<Option<mcp_calls::McpCallResponse>, copperclaw_db::DbError> {
    let start = Instant::now();
    loop {
        let lookup = {
            let conn = inbound.lock().await;
            mcp_calls::get_response(&conn, request_id)
        };
        match lookup {
            Ok(Some(resp)) => return Ok(Some(resp)),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
        if start.elapsed() >= deadline {
            return Ok(None);
        }
        sleep(Duration::from_millis(EXTERNAL_MCP_POLL_MS)).await;
    }
}

/// The synthetic error response used when the host never answers in time.
fn timeout_response(request_id: &str, advertised: &str) -> mcp_calls::McpCallResponse {
    mcp_calls::McpCallResponse {
        request_id: request_id.to_owned(),
        is_error: true,
        result: format!(
            "External MCP tool `{advertised}` did not return within {EXTERNAL_MCP_DEADLINE_SECS}s \
             (the host did not respond). The call may still be running host-side; try again or \
             break the work into smaller steps."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertised_name_is_namespaced() {
        assert_eq!(
            advertised_name("weather", "forecast"),
            "mcp__weather__forecast"
        );
    }

    #[test]
    fn missing_manifest_yields_no_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let (defs, routes) = load_external_tools(tmp.path());
        assert!(defs.is_empty());
        assert!(routes.is_empty());
    }

    #[test]
    fn malformed_manifest_yields_no_tools() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mcp_tools.json"), b"not json").unwrap();
        let (defs, routes) = load_external_tools(tmp.path());
        assert!(defs.is_empty());
        assert!(routes.is_empty());
    }

    #[test]
    fn manifest_builds_namespaced_defs_and_routes() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!([
            {
                "server": "weather",
                "name": "forecast",
                "description": "get the forecast",
                "input_schema": {"type": "object"}
            },
            {
                "server": "gh",
                "name": "create_issue",
                "description": null,
                "input_schema": {"type": "object"}
            }
        ]);
        std::fs::write(
            tmp.path().join("mcp_tools.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let (defs, routes) = load_external_tools(tmp.path());
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"mcp__weather__forecast"));
        assert!(names.contains(&"mcp__gh__create_issue"));

        let route = routes.get("mcp__weather__forecast").unwrap();
        assert_eq!(route.server, "weather");
        assert_eq!(route.tool, "forecast");

        // A null description becomes empty, not a panic.
        let gh_def = defs
            .iter()
            .find(|d| d.name == "mcp__gh__create_issue")
            .unwrap();
        assert_eq!(gh_def.description, "");
    }

    // ── poll_for_response (timeout branch is cheap to test in isolation) ──────

    use std::sync::Arc;

    use copperclaw_db::session::{SessionPaths, open_inbound};
    use copperclaw_db::tables::mcp_calls::McpCallResponse;
    use copperclaw_types::{AgentGroupId, SessionId};
    use tokio::sync::Mutex;

    fn fresh_inbound() -> (tempfile::TempDir, Arc<Mutex<rusqlite::Connection>>) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        (tmp, Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn poll_times_out_when_no_response_arrives() {
        let (_tmp, inbound) = fresh_inbound();
        // No response is ever written; a short deadline returns Ok(None).
        let got = poll_for_response(&inbound, "never", Duration::from_millis(120))
            .await
            .unwrap();
        assert!(got.is_none(), "absent response must time out to None");
    }

    #[tokio::test]
    async fn poll_returns_a_seeded_response() {
        let (_tmp, inbound) = fresh_inbound();
        {
            let conn = inbound.lock().await;
            mcp_calls::insert_response(
                &conn,
                &McpCallResponse {
                    request_id: "r1".into(),
                    is_error: false,
                    result: "sunny, 72F".into(),
                },
            )
            .unwrap();
        }
        let got = poll_for_response(&inbound, "r1", Duration::from_secs(5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.result, "sunny, 72F");
        assert!(!got.is_error);
    }

    #[tokio::test]
    async fn poll_returns_a_response_written_concurrently() {
        // Models the live flow: the runner is already polling when the host
        // writes the response a beat later.
        let (_tmp, inbound) = fresh_inbound();
        let writer = inbound.clone();
        let host = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let conn = writer.lock().await;
            mcp_calls::insert_response(
                &conn,
                &McpCallResponse {
                    request_id: "later".into(),
                    is_error: false,
                    result: "arrived".into(),
                },
            )
            .unwrap();
        });
        let got = poll_for_response(&inbound, "later", Duration::from_secs(5))
            .await
            .unwrap()
            .unwrap();
        host.await.unwrap();
        assert_eq!(got.result, "arrived");
    }

    #[test]
    fn timeout_response_is_an_error_naming_the_tool() {
        let r = timeout_response("rid", "mcp__weather__forecast");
        assert!(r.is_error);
        assert!(r.result.contains("mcp__weather__forecast"));
        assert_eq!(r.request_id, "rid");
    }
}
