//! Host-side OAuth token store for external MCP servers.
//!
//! See `migrations/022_mcp_oauth_tokens.sql` for the schema and the security
//! rationale. The cardinal rule: these tokens live on the HOST, in the central
//! DB, and are NEVER written into a session container's env or `runner.json`.
//! The container reaches an OAuth MCP server through a host-mediated dial; the
//! host injects the real token at that point.
//!
//! One row per `(agent_group_id, server_name)`. [`upsert`] overwrites in place
//! (keyed on the unique constraint), so re-running an OAuth flow refreshes the
//! stored token rather than accumulating duplicates.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::AgentGroupId;
use rusqlite::{OptionalExtension, Row, params};

/// A stored OAuth token for one external MCP server, scoped to one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthToken {
    pub agent_group_id: AgentGroupId,
    pub server_name: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub scope: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input for [`upsert`]. Timestamps are set by the function.
#[derive(Debug, Clone)]
pub struct UpsertMcpOAuthToken {
    pub agent_group_id: AgentGroupId,
    pub server_name: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub scope: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Insert or replace the token for `(agent_group_id, server_name)`.
///
/// `created_at` is preserved across upserts (the `ON CONFLICT` clause only
/// touches the mutable columns + `updated_at`), so the row keeps its original
/// creation instant while the token rotates.
pub fn upsert(db: &CentralDb, req: &UpsertMcpOAuthToken) -> Result<(), DbError> {
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO mcp_oauth_tokens
           (agent_group_id, server_name, access_token, refresh_token,
            token_type, scope, expires_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(agent_group_id, server_name) DO UPDATE SET
            access_token  = excluded.access_token,
            refresh_token = excluded.refresh_token,
            token_type    = excluded.token_type,
            scope         = excluded.scope,
            expires_at    = excluded.expires_at,
            updated_at    = excluded.updated_at",
        params![
            req.agent_group_id.as_uuid().to_string(),
            req.server_name,
            req.access_token,
            req.refresh_token,
            req.token_type,
            req.scope,
            req.expires_at.map(|t| t.to_rfc3339()),
            now.to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// Fetch the stored token for `(agent_group_id, server_name)`, or `None`.
pub fn get(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    server_name: &str,
) -> Result<Option<McpOAuthToken>, DbError> {
    let conn = db.conn()?;
    let row = conn
        .query_row(
            "SELECT agent_group_id, server_name, access_token, refresh_token,
                    token_type, scope, expires_at, created_at, updated_at
             FROM mcp_oauth_tokens
             WHERE agent_group_id = ?1 AND server_name = ?2",
            params![agent_group_id.as_uuid().to_string(), server_name],
            row_to_token,
        )
        .optional()?;
    Ok(row)
}

/// List every stored token for a group (newest-updated first). The
/// `access_token` / `refresh_token` are included; callers that surface this to
/// operators (e.g. `cclaw`) must redact — see the host inspect handler, which
/// returns only metadata.
pub fn list_for_group(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
) -> Result<Vec<McpOAuthToken>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id, server_name, access_token, refresh_token,
                token_type, scope, expires_at, created_at, updated_at
         FROM mcp_oauth_tokens
         WHERE agent_group_id = ?1
         ORDER BY updated_at DESC",
    )?;
    let rows = stmt
        .query_map(params![agent_group_id.as_uuid().to_string()], row_to_token)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Delete the token for `(agent_group_id, server_name)`. Returns the number of
/// rows removed (0 when there was nothing stored).
pub fn delete(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    server_name: &str,
) -> Result<usize, DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM mcp_oauth_tokens WHERE agent_group_id = ?1 AND server_name = ?2",
        params![agent_group_id.as_uuid().to_string(), server_name],
    )?;
    Ok(n)
}

fn row_to_token(row: &Row<'_>) -> rusqlite::Result<McpOAuthToken> {
    let ag_str: String = row.get(0)?;
    let agent_group_id: AgentGroupId = uuid::Uuid::parse_str(&ag_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .into();
    Ok(McpOAuthToken {
        agent_group_id,
        server_name: row.get(1)?,
        access_token: row.get(2)?,
        refresh_token: row.get(3)?,
        token_type: row.get(4)?,
        scope: row.get(5)?,
        expires_at: parse_opt_ts(row, 6)?,
        created_at: parse_ts(row, 7)?,
        updated_at: parse_ts(row, 8)?,
    })
}

fn parse_ts(row: &Row<'_>, idx: usize) -> rusqlite::Result<DateTime<Utc>> {
    let s: String = row.get(idx)?;
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn parse_opt_ts(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let s: Option<String> = row.get(idx)?;
    match s {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(&s)
            .map(|d| Some(d.with_timezone(&Utc)))
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    idx,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn group() -> AgentGroupId {
        AgentGroupId::new()
    }

    fn req(ag: AgentGroupId, server: &str, access: &str) -> UpsertMcpOAuthToken {
        UpsertMcpOAuthToken {
            agent_group_id: ag,
            server_name: server.into(),
            access_token: access.into(),
            refresh_token: Some("refresh-1".into()),
            token_type: "Bearer".into(),
            scope: Some("read write".into()),
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
        }
    }

    #[test]
    fn upsert_then_get_round_trips() {
        let db = db();
        let ag = group();
        upsert(&db, &req(ag, "github", "tok-abc")).unwrap();
        let got = get(&db, ag, "github").unwrap().expect("token present");
        assert_eq!(got.access_token, "tok-abc");
        assert_eq!(got.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(got.token_type, "Bearer");
        assert_eq!(got.scope.as_deref(), Some("read write"));
        assert!(got.expires_at.is_some());
    }

    #[test]
    fn get_returns_none_for_unknown() {
        let db = db();
        assert!(get(&db, group(), "nope").unwrap().is_none());
    }

    #[test]
    fn upsert_overwrites_and_preserves_created_at() {
        let db = db();
        let ag = group();
        upsert(&db, &req(ag, "github", "first")).unwrap();
        let first = get(&db, ag, "github").unwrap().unwrap();
        // Re-store with a new access token.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut second_req = req(ag, "github", "second");
        second_req.refresh_token = Some("refresh-2".into());
        upsert(&db, &second_req).unwrap();
        let second = get(&db, ag, "github").unwrap().unwrap();
        assert_eq!(second.access_token, "second");
        assert_eq!(second.refresh_token.as_deref(), Some("refresh-2"));
        // Only one row exists for the (group, server) pair.
        assert_eq!(list_for_group(&db, ag).unwrap().len(), 1);
        // created_at preserved; updated_at advanced.
        assert_eq!(second.created_at, first.created_at);
        assert!(second.updated_at >= first.updated_at);
    }

    #[test]
    fn list_for_group_is_isolated_per_group() {
        let db = db();
        let ag1 = group();
        let ag2 = group();
        upsert(&db, &req(ag1, "github", "a")).unwrap();
        upsert(&db, &req(ag1, "linear", "b")).unwrap();
        upsert(&db, &req(ag2, "github", "c")).unwrap();
        assert_eq!(list_for_group(&db, ag1).unwrap().len(), 2);
        assert_eq!(list_for_group(&db, ag2).unwrap().len(), 1);
    }

    #[test]
    fn delete_removes_only_the_named_token() {
        let db = db();
        let ag = group();
        upsert(&db, &req(ag, "github", "a")).unwrap();
        upsert(&db, &req(ag, "linear", "b")).unwrap();
        assert_eq!(delete(&db, ag, "github").unwrap(), 1);
        assert!(get(&db, ag, "github").unwrap().is_none());
        assert!(get(&db, ag, "linear").unwrap().is_some());
        // Deleting a missing token is a 0-row no-op, not an error.
        assert_eq!(delete(&db, ag, "github").unwrap(), 0);
    }

    #[test]
    fn null_optional_fields_round_trip() {
        let db = db();
        let ag = group();
        let r = UpsertMcpOAuthToken {
            agent_group_id: ag,
            server_name: "minimal".into(),
            access_token: "tok".into(),
            refresh_token: None,
            token_type: "Bearer".into(),
            scope: None,
            expires_at: None,
        };
        upsert(&db, &r).unwrap();
        let got = get(&db, ag, "minimal").unwrap().unwrap();
        assert!(got.refresh_token.is_none());
        assert!(got.scope.is_none());
        assert!(got.expires_at.is_none());
    }
}
