//! Host-proxied external MCP tool-call RPC over the two per-session DBs.
//!
//! The request side (`mcp_call_requests`) lives in `outbound.db` — the runner
//! is its sole writer; the host reads it. The response side
//! (`mcp_call_responses`) lives in `inbound.db` — the host is its sole writer;
//! the runner reads it. Correlated by `request_id`. This split is what keeps
//! the single-writer-per-bind-mounted-DB invariant intact while still giving a
//! request/response round-trip: neither process ever writes the other's file.
//!
//! Flow:
//! 1. runner: [`insert_request`] (outbound) → blocks-polls [`get_response`] (inbound)
//! 2. host:   [`list_requests`] (outbound) → execute → [`insert_response`] (inbound)
//! 3. runner: [`get_response`] hit → renders the `tool_result` → [`delete_request`] (outbound)
//! 4. host:   [`gc_orphan_responses`] drops a response whose request the runner consumed
//!
//! GC by the *response* side keying off the *request* side means a response is
//! only ever removed after the runner has deleted its request (i.e. after it
//! has consumed the result), so a result is never reaped before it is read.

use crate::DbError;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashSet;

/// A pending external MCP tool-call request, written by the runner to
/// `outbound.db::mcp_call_requests`.
#[derive(Debug, Clone)]
pub struct McpCallRequest {
    /// Correlation id (UUID string) tying the request to its response.
    pub request_id: String,
    /// The configured external server name (the `mcp_servers` key).
    pub server: String,
    /// The remote tool name (already stripped of the `mcp__<server>__` prefix).
    pub tool: String,
    /// The JSON arguments the model supplied.
    pub input: serde_json::Value,
}

/// A rendered external MCP tool-call response, written by the host to
/// `inbound.db::mcp_call_responses`.
#[derive(Debug, Clone)]
pub struct McpCallResponse {
    pub request_id: String,
    /// Mirrors the `tool_result` `is_error` flag.
    pub is_error: bool,
    /// The already-rendered model-facing text.
    pub result: String,
}

/// Insert a request row (runner side, outbound.db). Idempotent on
/// `request_id`: a duplicate id is ignored (the UUID makes a collision a
/// non-event).
pub fn insert_request(conn: &Connection, req: &McpCallRequest) -> Result<(), DbError> {
    conn.execute(
        "INSERT OR IGNORE INTO mcp_call_requests
           (request_id, server, tool, input, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            req.request_id,
            req.server,
            req.tool,
            req.input.to_string(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// List all outstanding requests (host side, outbound.db), oldest first.
pub fn list_requests(conn: &Connection) -> Result<Vec<McpCallRequest>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT request_id, server, tool, input
         FROM mcp_call_requests ORDER BY created_at, request_id",
    )?;
    let rows = stmt.query_map([], |row| {
        let input_str: String = row.get("input")?;
        let input = serde_json::from_str(&input_str).unwrap_or(serde_json::Value::Null);
        Ok(McpCallRequest {
            request_id: row.get("request_id")?,
            server: row.get("server")?,
            tool: row.get("tool")?,
            input,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The set of request ids still present in `outbound.db` (host side). Used to
/// GC orphan responses — a response whose request id is absent here has been
/// consumed by the runner and can be dropped.
pub fn request_ids(conn: &Connection) -> Result<HashSet<String>, DbError> {
    let mut stmt = conn.prepare("SELECT request_id FROM mcp_call_requests")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Delete a consumed request (runner side, outbound.db) after its response has
/// been read and rendered.
pub fn delete_request(conn: &Connection, request_id: &str) -> Result<(), DbError> {
    conn.execute(
        "DELETE FROM mcp_call_requests WHERE request_id = ?1",
        params![request_id],
    )?;
    Ok(())
}

/// Insert a response row (host side, inbound.db). Idempotent on `request_id`:
/// re-executing an already-answered request is a no-op, so the host's dedup
/// (don't execute a request that already has a response) is belt-and-braces.
pub fn insert_response(conn: &Connection, resp: &McpCallResponse) -> Result<(), DbError> {
    conn.execute(
        "INSERT OR IGNORE INTO mcp_call_responses
           (request_id, is_error, result, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            resp.request_id,
            i64::from(resp.is_error),
            resp.result,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Read a response by request id (runner side, inbound.db). Returns `None`
/// when the host has not answered yet. Tolerates a not-yet-migrated inbound DB
/// (the table is created the first time the host opens inbound.db) by treating
/// a missing table as "no response yet".
pub fn get_response(
    conn: &Connection,
    request_id: &str,
) -> Result<Option<McpCallResponse>, DbError> {
    let row = conn
        .query_row(
            "SELECT request_id, is_error, result
             FROM mcp_call_responses WHERE request_id = ?1",
            params![request_id],
            |row| {
                let is_error: i64 = row.get("is_error")?;
                Ok(McpCallResponse {
                    request_id: row.get("request_id")?,
                    is_error: is_error != 0,
                    result: row.get("result")?,
                })
            },
        )
        .optional();
    match row {
        Ok(v) => Ok(v),
        // A read against an inbound.db the host hasn't migrated yet (the table
        // arrives with migration 024) is not an error to the caller — there is
        // simply no response. Any other SQL error propagates.
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// The set of answered request ids (host side, inbound.db). Used to skip
/// re-executing a request that already has a response.
pub fn response_ids(conn: &Connection) -> Result<HashSet<String>, DbError> {
    let mut stmt = conn.prepare("SELECT request_id FROM mcp_call_responses")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Drop responses whose request the runner has already consumed (host side,
/// inbound.db). `live_request_ids` is the current `outbound.db` request set;
/// any response id not in it is an orphan and is removed. Returns the count
/// deleted.
pub fn gc_orphan_responses<S: std::hash::BuildHasher>(
    conn: &Connection,
    live_request_ids: &HashSet<String, S>,
) -> Result<usize, DbError> {
    let answered = response_ids(conn)?;
    let mut deleted = 0;
    for id in answered {
        if !live_request_ids.contains(&id) {
            conn.execute(
                "DELETE FROM mcp_call_responses WHERE request_id = ?1",
                params![id],
            )?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionPaths, open_inbound, open_outbound};
    use copperclaw_types::{AgentGroupId, SessionId};

    fn dbs() -> (tempfile::TempDir, Connection, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let out = open_outbound(&paths).unwrap();
        let inb = open_inbound(&paths).unwrap();
        (tmp, out, inb)
    }

    fn req(id: &str) -> McpCallRequest {
        McpCallRequest {
            request_id: id.into(),
            server: "weather".into(),
            tool: "forecast".into(),
            input: serde_json::json!({"city": "NYC"}),
        }
    }

    #[test]
    fn request_round_trips_through_outbound() {
        let (_t, out, _inb) = dbs();
        insert_request(&out, &req("r1")).unwrap();
        let rows = list_requests(&out).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "r1");
        assert_eq!(rows[0].server, "weather");
        assert_eq!(rows[0].tool, "forecast");
        assert_eq!(rows[0].input["city"], "NYC");
    }

    #[test]
    fn insert_request_is_idempotent() {
        let (_t, out, _inb) = dbs();
        insert_request(&out, &req("r1")).unwrap();
        insert_request(&out, &req("r1")).unwrap();
        assert_eq!(list_requests(&out).unwrap().len(), 1);
    }

    #[test]
    fn response_round_trips_through_inbound() {
        let (_t, _out, inb) = dbs();
        assert!(get_response(&inb, "r1").unwrap().is_none());
        insert_response(
            &inb,
            &McpCallResponse {
                request_id: "r1".into(),
                is_error: false,
                result: "sunny".into(),
            },
        )
        .unwrap();
        let got = get_response(&inb, "r1").unwrap().unwrap();
        assert!(!got.is_error);
        assert_eq!(got.result, "sunny");
    }

    #[test]
    fn error_response_preserves_flag() {
        let (_t, _out, inb) = dbs();
        insert_response(
            &inb,
            &McpCallResponse {
                request_id: "bad".into(),
                is_error: true,
                result: "denied".into(),
            },
        )
        .unwrap();
        let got = get_response(&inb, "bad").unwrap().unwrap();
        assert!(got.is_error);
    }

    #[test]
    fn delete_request_removes_it() {
        let (_t, out, _inb) = dbs();
        insert_request(&out, &req("r1")).unwrap();
        delete_request(&out, "r1").unwrap();
        assert!(list_requests(&out).unwrap().is_empty());
        assert!(request_ids(&out).unwrap().is_empty());
    }

    #[test]
    fn gc_drops_only_orphan_responses() {
        let (_t, out, inb) = dbs();
        // Two responses; only one still has a live request.
        insert_request(&out, &req("live")).unwrap();
        for id in ["live", "consumed"] {
            insert_response(
                &inb,
                &McpCallResponse {
                    request_id: id.into(),
                    is_error: false,
                    result: "x".into(),
                },
            )
            .unwrap();
        }
        let live = request_ids(&out).unwrap();
        let deleted = gc_orphan_responses(&inb, &live).unwrap();
        assert_eq!(deleted, 1, "only the consumed response is an orphan");
        assert!(get_response(&inb, "live").unwrap().is_some());
        assert!(get_response(&inb, "consumed").unwrap().is_none());
    }

    #[test]
    fn get_response_tolerates_missing_table() {
        // A fresh inbound.db opened RO/no-migrate has no mcp_call_responses
        // table; the runner's poll must treat that as "no response yet".
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        paths.ensure_dirs().unwrap();
        // Open a bare connection WITHOUT running migrations.
        let bare = rusqlite::Connection::open(&paths.inbound_db).unwrap();
        assert!(get_response(&bare, "whatever").unwrap().is_none());
    }
}
