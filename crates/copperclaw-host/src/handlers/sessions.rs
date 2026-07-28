//! Handlers for `sessions.*` commands.

use super::{db_err, opt_str, parse_uuid, req_str};
use copperclaw_cclaw::{Caller, ErrorPayload};
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::SessionPaths;
use copperclaw_db::tables::sessions;
use copperclaw_types::{AgentGroupId, ContainerStatus, Session, SessionId};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::path::Path;
use tracing::warn;

/// Number of recent rows per direction attached to `sessions.get`.
const RECENT_ROWS: i64 = 10;

/// Default per-direction row cap for `sessions.tail` when the caller
/// doesn't pass `limit`. Clamped to [`TAIL_LIMIT_MAX`].
const TAIL_LIMIT_DEFAULT: i64 = 50;
const TAIL_LIMIT_MAX: i64 = 500;

/// Content previews are truncated to roughly this many characters
/// (after secret redaction, on a char boundary).
const PREVIEW_CHARS: usize = 120;

// ---- W3.4: architect-state files surfaced by `sessions.get` ---------------
//
// The agent declares restartable services in `/data/.copperclaw/services`
// (replayed at cold boot by the W2.2 runner hook, which logs to
// `/data/.copperclaw/services.log`) and commits architecture decisions to
// `/data/<project>/.copperclaw/DECISIONS.md`. All three are agent-authored
// files on the host-side session root, so `sessions.get` surfaces them
// best-effort — a missing or unreadable file yields an absent field, never
// an error — with every read capped so a huge agent-written file cannot
// bloat the response.

/// Per-file byte cap for architect-state reads (head of the services
/// file; tail of the log / DECISIONS.md).
const STATE_FILE_READ_BYTES: u64 = 8 * 1024;
/// Max declared-service lines attached.
const SERVICES_LINES_MAX: usize = 20;
/// Max `services.log` tail lines attached (timestamped lines only —
/// the per-command output bodies are skipped).
const SERVICES_LOG_TAIL_LINES: usize = 10;
/// Max DECISIONS.md tail lines attached per project.
const DECISIONS_TAIL_LINES: usize = 10;
/// Max project entries scanned for DECISIONS.md.
const DECISIONS_PROJECTS_MAX: usize = 10;
/// Per-line char cap for architect-state lines (after redaction).
const STATE_LINE_CHARS: usize = 200;

pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let status = opt_str(args, "status");
    let mut rows = match status.as_deref() {
        Some("running") => sessions::list_running(central).map_err(db_err)?,
        // Treat both "active" and missing as "list_active".
        _ => sessions::list_active(central).map_err(db_err)?,
    };
    if let Some(ag) = opt_str(args, "agent_group_id") {
        let ag_id = AgentGroupId(parse_uuid(&ag)?);
        rows.retain(|s| s.agent_group_id == ag_id);
    }
    Ok(json!(rows.iter().map(session_to_json).collect::<Vec<_>>()))
}

/// `sessions.get` — the session row plus, when the caller is allowed to
/// read message content, the last [`RECENT_ROWS`] `messages_in` /
/// `messages_out` rows (kind, status, ts, redacted content preview) read
/// **read-only** from the per-session DBs.
///
/// Message previews are attached for [`Caller::Host`] always, and for a
/// [`Caller::Agent`] only when it asks about its *own* session — an agent
/// must not read another session's traffic. Foreign-session agent calls
/// degrade to today's row-only response rather than erroring, so existing
/// container-side introspection keeps working.
pub fn get(
    args: &Value,
    caller: &Caller,
    ctx: &crate::socket::HandlerCtx,
) -> Result<Value, ErrorPayload> {
    let id = SessionId(parse_uuid(&req_str(args, "id")?)?);
    let row = sessions::get(&ctx.central, id).map_err(db_err)?;
    let mut obj = session_to_json(&row);
    if caller_may_read_messages(caller, id) {
        let paths = SessionPaths::new(&ctx.data_dir, row.agent_group_id, id);
        let inbound = read_message_rows(&paths.inbound_db, Direction::In, None, RECENT_ROWS);
        let outbound = read_message_rows(&paths.outbound_db, Direction::Out, None, RECENT_ROWS);
        if let Some(o) = obj.as_object_mut() {
            o.insert("recent_inbound".into(), json!(inbound));
            o.insert("recent_outbound".into(), json!(outbound));
            attach_architect_state(o, &paths.root);
        }
    }
    Ok(obj)
}

/// Attach the W3.4 architect-state fields (`services`,
/// `services_log_tail`, `decisions`) to a `sessions.get` response when
/// the corresponding files exist under the session root. Best-effort:
/// anything missing or unreadable is simply absent.
fn attach_architect_state(obj: &mut serde_json::Map<String, Value>, session_root: &Path) {
    let dot = session_root.join(".copperclaw");
    if let Some(services) = read_declared_services(&dot.join("services")) {
        obj.insert("services".into(), json!(services));
    }
    if let Some(tail) = read_services_log_tail(&dot.join("services.log")) {
        obj.insert("services_log_tail".into(), json!(tail));
    }
    let decisions = read_project_decisions(session_root);
    if !decisions.is_empty() {
        obj.insert("decisions".into(), json!(decisions));
    }
}

/// The non-comment, non-blank lines of `.copperclaw/services` (first
/// [`STATE_FILE_READ_BYTES`] only), sanitized, capped at
/// [`SERVICES_LINES_MAX`]. `None` when the file is missing/unreadable;
/// `Some(vec![])` when it exists but declares nothing.
fn read_declared_services(path: &Path) -> Option<Vec<String>> {
    let head = read_file_head(path, STATE_FILE_READ_BYTES)?;
    Some(
        head.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(sanitize_state_line)
            .take(SERVICES_LINES_MAX)
            .collect(),
    )
}

/// The last [`SERVICES_LOG_TAIL_LINES`] timestamped lines of
/// `.copperclaw/services.log` — the `[ts] $ cmd` / `[ts] status: ...`
/// lines the W2.2 hook writes — skipping captured command output, from
/// at most the trailing [`STATE_FILE_READ_BYTES`] of the file.
fn read_services_log_tail(path: &Path) -> Option<Vec<String>> {
    let tail = read_file_tail(path, STATE_FILE_READ_BYTES)?;
    let stamped: Vec<String> = tail
        .lines()
        .map(str::trim_end)
        .filter(|l| l.starts_with('['))
        .map(sanitize_state_line)
        .collect();
    let skip = stamped.len().saturating_sub(SERVICES_LOG_TAIL_LINES);
    Some(stamped.into_iter().skip(skip).collect())
}

/// Scan the immediate child directories of the session root for
/// `<project>/.copperclaw/DECISIONS.md` and return, per project, the
/// last [`DECISIONS_TAIL_LINES`] non-blank lines of the file. Hidden
/// directories are skipped; at most [`DECISIONS_PROJECTS_MAX`] projects
/// are surfaced (name order, for determinism).
fn read_project_decisions(session_root: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(session_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let path = session_root
                .join(&name)
                .join(".copperclaw")
                .join("DECISIONS.md");
            let tail = read_file_tail(&path, STATE_FILE_READ_BYTES)?;
            let lines: Vec<String> = tail
                .lines()
                .map(str::trim_end)
                .filter(|l| !l.trim().is_empty())
                .map(sanitize_state_line)
                .collect();
            let skip = lines.len().saturating_sub(DECISIONS_TAIL_LINES);
            let tail_lines: Vec<String> = lines.into_iter().skip(skip).collect();
            Some(json!({
                "project": sanitize_state_line(&name),
                "tail": tail_lines,
            }))
        })
        .take(DECISIONS_PROJECTS_MAX)
        .collect()
}

/// Read at most the first `cap` bytes of a file (lossy UTF-8). `None`
/// on any I/O failure — architect-state reads are strictly best-effort.
fn read_file_head(path: &Path, cap: u64) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Read at most the last `cap` bytes of a file (lossy UTF-8), dropping
/// the leading partial line when the file was longer than `cap`. `None`
/// on any I/O failure.
fn read_file_tail(path: &Path, cap: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let truncated = len > cap;
    if truncated {
        file.seek(SeekFrom::Start(len - cap)).ok()?;
    }
    let mut buf = Vec::new();
    file.take(cap).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        // The seek almost certainly landed mid-line; drop the fragment.
        return Some(
            text.split_once('\n')
                .map_or(String::new(), |(_, rest)| rest.to_string()),
        );
    }
    Some(text)
}

/// Sanitize one agent-authored line for display: redact secrets (same
/// pass the message previews use), replace control characters — which
/// would otherwise reach the operator's terminal raw — with spaces, and
/// cap at [`STATE_LINE_CHARS`] chars.
fn sanitize_state_line(line: &str) -> String {
    let redacted = copperclaw_runner::redact_secrets(line);
    let cleaned: String = redacted
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out: String = cleaned.chars().take(STATE_LINE_CHARS).collect();
    if cleaned.chars().count() > STATE_LINE_CHARS {
        out.push_str("...");
    }
    out
}

/// `sessions.tail` — merged, time-ordered recent rows from both
/// per-session DBs, read-only, callable while the session is running
/// (`inbound.db` is `journal_mode=DELETE`; `outbound.db` is WAL — both
/// tolerate a concurrent reader).
///
/// Args: `id` (required), `since_in_seq` / `since_out_seq` (optional —
/// return only rows with a strictly greater `seq`; the `--follow` poll
/// loop uses these), `limit` (optional per-direction cap).
///
/// Unlike `sessions.get`, the whole point of this command is message
/// content, so a foreign-session agent caller is refused outright.
pub fn tail(
    args: &Value,
    caller: &Caller,
    ctx: &crate::socket::HandlerCtx,
) -> Result<Value, ErrorPayload> {
    let id = SessionId(parse_uuid(&req_str(args, "id")?)?);
    if !caller_may_read_messages(caller, id) {
        return Err(ErrorPayload::new(
            "permission_denied",
            "sessions.tail on another session is host-only",
        ));
    }
    let row = sessions::get(&ctx.central, id).map_err(db_err)?;
    let since_in = args.get("since_in_seq").and_then(Value::as_i64);
    let since_out = args.get("since_out_seq").and_then(Value::as_i64);
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .unwrap_or(TAIL_LIMIT_DEFAULT)
        .clamp(1, TAIL_LIMIT_MAX);

    let paths = SessionPaths::new(&ctx.data_dir, row.agent_group_id, id);
    let inbound = read_message_rows(&paths.inbound_db, Direction::In, since_in, limit);
    let outbound = read_message_rows(&paths.outbound_db, Direction::Out, since_out, limit);

    let last_in_seq = max_seq(&inbound).or(since_in).unwrap_or(0);
    let last_out_seq = max_seq(&outbound).or(since_out).unwrap_or(0);

    let mut rows: Vec<MessageRow> = inbound;
    rows.extend(outbound);
    // RFC3339 UTC timestamps sort lexicographically; fall back to seq so
    // same-instant rows keep a stable order.
    rows.sort_by(|a, b| a.ts.cmp(&b.ts).then(a.seq.cmp(&b.seq)));

    Ok(json!({
        "session_id": id.as_uuid().to_string(),
        "rows": rows,
        "last_in_seq": last_in_seq,
        "last_out_seq": last_out_seq,
    }))
}

/// Whether `caller` may read message content for `session`: the host
/// always, an agent only for its own session.
fn caller_may_read_messages(caller: &Caller, session: SessionId) -> bool {
    match caller {
        Caller::Host => true,
        Caller::Agent { session_id, .. } => *session_id == session,
    }
}

/// Direction tag on a tail row: which per-session DB it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    In,
    Out,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

/// One message row as surfaced by `sessions.get` / `sessions.tail`.
#[derive(Debug, serde::Serialize)]
struct MessageRow {
    direction: &'static str,
    seq: i64,
    kind: String,
    status: String,
    ts: String,
    preview: String,
}

fn max_seq(rows: &[MessageRow]) -> Option<i64> {
    rows.iter().map(|r| r.seq).max()
}

/// Read up to `limit` rows from one per-session DB, oldest-first.
///
/// `since_seq: Some(n)` returns rows with `seq > n` (ascending);
/// `None` returns the *last* `limit` rows. A missing or unreadable DB
/// yields an empty list (the session may simply never have spawned) —
/// never an error, because message rows are supplemental to the session
/// row itself.
fn read_message_rows(
    db_path: &Path,
    direction: Direction,
    since_seq: Option<i64>,
    limit: i64,
) -> Vec<MessageRow> {
    let Some(conn) = open_session_db_readonly(db_path) else {
        return Vec::new();
    };
    match query_message_rows(&conn, direction, since_seq, limit) {
        Ok(rows) => rows,
        Err(e) => {
            warn!(
                error = %e,
                path = %db_path.display(),
                "sessions handler: per-session DB query failed; returning no rows",
            );
            Vec::new()
        }
    }
}

/// Open a per-session DB strictly for reading. Prefers a read-only
/// handle; falls back to read-write-no-create because a WAL database
/// whose `-shm` file is absent can refuse pure read-only openers
/// (`SQLITE_READONLY_CANTINIT`). No writes are ever issued either way.
fn open_session_db_readonly(path: &Path) -> Option<Connection> {
    if !path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .or_else(|_| Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE))
        .ok()?;
    conn.execute_batch("PRAGMA busy_timeout=5000;").ok()?;
    Some(conn)
}

fn query_message_rows(
    conn: &Connection,
    direction: Direction,
    since_seq: Option<i64>,
    limit: i64,
) -> rusqlite::Result<Vec<MessageRow>> {
    // `messages_in` carries its own status column; outbound delivery
    // status lives in `processing_ack` keyed by message id.
    let sql = match (direction, since_seq.is_some()) {
        (Direction::In, true) => {
            "SELECT seq, kind, status, timestamp, content FROM messages_in
             WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2"
        }
        (Direction::In, false) => {
            "SELECT seq, kind, status, timestamp, content FROM
               (SELECT seq, kind, status, timestamp, content FROM messages_in
                ORDER BY seq DESC LIMIT ?1)
             ORDER BY seq ASC"
        }
        (Direction::Out, true) => {
            "SELECT m.seq, m.kind, COALESCE(p.status, 'pending') AS status,
                    m.timestamp, m.content
             FROM messages_out m
             LEFT JOIN processing_ack p ON p.message_id = m.id
             WHERE m.seq > ?1 ORDER BY m.seq ASC LIMIT ?2"
        }
        (Direction::Out, false) => {
            "SELECT seq, kind, status, timestamp, content FROM
               (SELECT m.seq AS seq, m.kind AS kind,
                       COALESCE(p.status, 'pending') AS status,
                       m.timestamp AS timestamp, m.content AS content
                FROM messages_out m
                LEFT JOIN processing_ack p ON p.message_id = m.id
                ORDER BY m.seq DESC LIMIT ?1)
             ORDER BY seq ASC"
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let map_row = |row: &rusqlite::Row<'_>| {
        let content: String = row.get("content")?;
        Ok(MessageRow {
            direction: direction.as_str(),
            seq: row.get("seq")?,
            kind: row.get("kind")?,
            status: row.get("status")?,
            ts: row.get("timestamp")?,
            preview: content_preview(&content),
        })
    };
    let rows = if let Some(since) = since_seq {
        stmt.query_map(rusqlite::params![since, limit], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map(rusqlite::params![limit], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(rows)
}

/// Render a short, redacted, single-line preview of a message `content`
/// column. Chat-shaped payloads (`{"text": ...}`) surface the text;
/// anything else surfaces its compact JSON. Secrets are redacted through
/// the same pass the log pipeline uses (`copperclaw_runner::redact_secrets`)
/// *before* truncation so a cut can never expose a partial secret.
fn content_preview(raw: &str) -> String {
    let text = match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => map
            .get("text")
            .and_then(Value::as_str)
            .map_or_else(|| Value::Object(map.clone()).to_string(), str::to_owned),
        Ok(other) => other.to_string(),
        Err(_) => raw.to_string(),
    };
    let redacted = copperclaw_runner::redact_secrets(&text);
    let flat = redacted.replace(['\n', '\r'], " ");
    let mut out: String = flat.chars().take(PREVIEW_CHARS).collect();
    if flat.chars().count() > PREVIEW_CHARS {
        out.push_str("...");
    }
    out
}

/// `sessions.delete` — drop the central DB row, cascade per-session
/// rows that don't have FK cascade today (`agent_turns` + `tasks`) and
/// rows the FK would otherwise refuse to leave behind
/// (`pending_questions`, `pending_approvals`), then rmtree the on-disk
/// session directory.
///
/// Refuses by default when the session's container is not Stopped so
/// the operator gets a chance to call `groups.restart` first; pass
/// `force: true` to delete anyway.
///
/// Filesystem removal is best-effort: a failure there logs a `warn!`
/// but doesn't fail the request, because the central-DB rows are
/// already gone and re-running the command would just `NotFound`.
pub fn delete(args: &Value, ctx: &crate::socket::HandlerCtx) -> Result<Value, ErrorPayload> {
    let id = SessionId(parse_uuid(&req_str(args, "id")?)?);
    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
    let session = sessions::get(&ctx.central, id).map_err(db_err)?;
    if !matches!(session.container_status, ContainerStatus::Stopped) && !force {
        return Err(ErrorPayload::new(
            "container_not_stopped",
            format!(
                "session {} container_status is {:?}; restart the agent group first \
                 (`cclaw groups restart <id>`) or pass --force",
                id.as_uuid(),
                session.container_status
            ),
        ));
    }
    sessions::delete(&ctx.central, id).map_err(db_err)?;
    let removed_dir = remove_session_dir(&ctx.data_dir, session.agent_group_id, id);
    Ok(json!({
        "deleted": id.as_uuid().to_string(),
        "agent_group_id": session.agent_group_id.as_uuid().to_string(),
        "directory_removed": removed_dir,
    }))
}

/// Best-effort removal of the on-disk session tree. Returns the boolean
/// `true` if the directory existed and was removed, `false` if it was
/// missing or removal failed (warn-logged in the latter case). Never
/// fails the parent request.
fn remove_session_dir(data_dir: &Path, agent: AgentGroupId, session: SessionId) -> bool {
    let paths = SessionPaths::new(data_dir, agent, session);
    if !paths.root.exists() {
        return false;
    }
    match std::fs::remove_dir_all(&paths.root) {
        Ok(()) => true,
        Err(e) => {
            warn!(
                error = %e,
                path = %paths.root.display(),
                "sessions.delete: failed to remove on-disk session directory; \
                 central-DB row is already gone so the command still succeeded",
            );
            false
        }
    }
}

fn session_to_json(s: &Session) -> Value {
    json!({
        "id": s.id.as_uuid().to_string(),
        "agent_group_id": s.agent_group_id.as_uuid().to_string(),
        "messaging_group_id": s.messaging_group_id.map(|m| m.as_uuid().to_string()),
        "thread_id": s.thread_id,
        "agent_provider": s.agent_provider,
        "status": session_status_str(s.status),
        "container_status": s.container_status.as_str(),
        "last_active": s.last_active.to_rfc3339(),
        "created_at": s.created_at.to_rfc3339(),
    })
}

fn session_status_str(s: copperclaw_types::SessionStatus) -> &'static str {
    match s {
        copperclaw_types::SessionStatus::Active => "active",
        copperclaw_types::SessionStatus::Archived => "archived",
        copperclaw_types::SessionStatus::Stopped => "stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::HandlerCtx;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::sessions::{
        CreateSession, create as create_session, mark_container_running,
    };

    fn ctx_with(central: CentralDb, data_dir: std::path::PathBuf) -> HandlerCtx {
        HandlerCtx::with_data_dir(central, data_dir)
    }

    fn db_with_session() -> (CentralDb, SessionId, AgentGroupId) {
        let db = CentralDb::open_in_memory().unwrap();
        let g = create_ag(
            &db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let s = create_session(
            &db,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        (db, s.id, g.id)
    }

    #[test]
    fn list_returns_active_sessions() {
        let (db, _s, _g) = db_with_session();
        let v = list(&Value::Null, &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn list_filtered_by_agent_group() {
        let (db, _s, g) = db_with_session();
        let v = list(&json!({"agent_group_id": g.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        let other = AgentGroupId::new();
        let v = list(&json!({"agent_group_id": other.as_uuid().to_string()}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_running_filter_returns_only_running() {
        let (db, s, _g) = db_with_session();
        let v = list(&json!({"status": "running"}), &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
        mark_container_running(&db, s).unwrap();
        let v = list(&json!({"status": "running"}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    #[test]
    fn get_by_id() {
        let (db, s, _g) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();
        assert_eq!(v["id"].as_str().unwrap(), s.as_uuid().to_string());
        // No per-session DBs on disk → empty recents, never an error.
        assert_eq!(v["recent_inbound"], json!([]));
        assert_eq!(v["recent_outbound"], json!([]));
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let err = get(
            &json!({"id": uuid::Uuid::now_v7().to_string()}),
            &Caller::Host,
            &ctx,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn session_status_str_covers_variants() {
        assert_eq!(
            session_status_str(copperclaw_types::SessionStatus::Active),
            "active"
        );
        assert_eq!(
            session_status_str(copperclaw_types::SessionStatus::Archived),
            "archived"
        );
        assert_eq!(
            session_status_str(copperclaw_types::SessionStatus::Stopped),
            "stopped"
        );
    }

    #[test]
    fn delete_removes_stopped_session_row() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = delete(&json!({"id": s.as_uuid().to_string()}), &ctx).unwrap();
        assert_eq!(v["deleted"], s.as_uuid().to_string());
        assert_eq!(v["agent_group_id"], ag.as_uuid().to_string());
        // Session is gone from the central DB.
        let err = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_refuses_running_session_without_force() {
        let (db, s, _ag) = db_with_session();
        mark_container_running(&db, s).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let err = delete(&json!({"id": s.as_uuid().to_string()}), &ctx).unwrap_err();
        assert_eq!(err.code, "container_not_stopped");
        // Session still present.
        assert!(get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).is_ok());
    }

    #[test]
    fn delete_running_session_with_force_succeeds() {
        let (db, s, _ag) = db_with_session();
        mark_container_running(&db, s).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        delete(&json!({"id": s.as_uuid().to_string(), "force": true}), &ctx).unwrap();
        let err = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_missing_session_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let err = delete(&json!({"id": SessionId::new().as_uuid().to_string()}), &ctx).unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    #[test]
    fn delete_removes_on_disk_session_directory() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        // Materialise the session dir before deletion.
        let paths = SessionPaths::new(tmp.path(), ag, s);
        paths.ensure_dirs().unwrap();
        assert!(paths.root.exists());
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = delete(&json!({"id": s.as_uuid().to_string()}), &ctx).unwrap();
        assert_eq!(v["directory_removed"], true);
        assert!(!paths.root.exists(), "session dir should be gone");
    }

    #[test]
    fn delete_succeeds_when_session_dir_missing() {
        let (db, s, _ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = delete(&json!({"id": s.as_uuid().to_string()}), &ctx).unwrap();
        // No directory pre-existed.
        assert_eq!(v["directory_removed"], false);
    }

    // ---- D2: sessions.get message previews + sessions.tail ---------------

    use chrono::{Duration, Utc};
    use copperclaw_db::session::{open_inbound, open_outbound};
    use copperclaw_db::tables::{messages_in, messages_out};
    use copperclaw_types::{MessageId, MessageKind};

    fn inbound_write(text: &str, at_offset_secs: i64) -> messages_in::WriteInbound {
        messages_in::WriteInbound {
            id: MessageId::new(),
            kind: MessageKind::Chat,
            timestamp: Utc::now() + Duration::seconds(at_offset_secs),
            content: json!({"text": text}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        }
    }

    fn outbound_write(
        kind: MessageKind,
        text: &str,
        at_offset_secs: i64,
    ) -> messages_out::WriteOutbound {
        messages_out::WriteOutbound {
            id: MessageId::new(),
            in_reply_to: None,
            timestamp: Utc::now() + Duration::seconds(at_offset_secs),
            deliver_after: None,
            recurrence: None,
            kind,
            platform_id: None,
            channel_type: None,
            thread_id: None,
            content: json!({"text": text}),
        }
    }

    /// Seed the per-session DBs under `data_dir` for `(ag, s)` with
    /// `n_in` inbound chats and `n_out` outbound chats at increasing
    /// timestamps (inbound at even offsets, outbound at odd, so the
    /// merged order strictly interleaves in→out→in→out…).
    fn seed_session_dbs(
        data_dir: &Path,
        ag: AgentGroupId,
        s: SessionId,
        n_in: usize,
        n_out: usize,
    ) {
        let paths = SessionPaths::new(data_dir, ag, s);
        let in_conn = open_inbound(&paths).unwrap();
        for i in 0..n_in {
            let off = i64::try_from(i).unwrap() * 2;
            messages_in::insert(&in_conn, &inbound_write(&format!("inbound {i}"), off)).unwrap();
        }
        let out_conn = open_outbound(&paths).unwrap();
        for i in 0..n_out {
            let off = i64::try_from(i).unwrap() * 2 + 1;
            messages_out::insert(
                &out_conn,
                &outbound_write(MessageKind::Chat, &format!("outbound {i}"), off),
            )
            .unwrap();
        }
    }

    #[test]
    fn get_attaches_last_ten_rows_per_direction() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        seed_session_dbs(tmp.path(), ag, s, 12, 12);
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();
        let inbound = v["recent_inbound"].as_array().unwrap();
        let outbound = v["recent_outbound"].as_array().unwrap();
        assert_eq!(inbound.len(), 10, "capped at the last 10 inbound rows");
        assert_eq!(outbound.len(), 10, "capped at the last 10 outbound rows");
        // Oldest rows (0, 1) fell off; newest are present, oldest-first.
        assert_eq!(inbound[0]["preview"], "inbound 2");
        assert_eq!(inbound[9]["preview"], "inbound 11");
        // Row shape: kind, status, ts, preview.
        for row in inbound.iter().chain(outbound.iter()) {
            assert!(row["kind"].is_string());
            assert!(row["status"].is_string());
            assert!(row["ts"].is_string());
            assert!(row["preview"].is_string());
        }
        assert_eq!(inbound[0]["kind"], "chat");
        assert_eq!(inbound[0]["status"], "pending");
        // Outbound status comes from processing_ack (absent → pending).
        assert_eq!(outbound[0]["status"], "pending");
    }

    #[test]
    fn get_agent_caller_sees_own_messages_but_not_foreign() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        seed_session_dbs(tmp.path(), ag, s, 1, 0);
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let own = Caller::Agent {
            session_id: s,
            agent_group_id: ag,
            messaging_group_id: None,
        };
        let v = get(&json!({"id": s.as_uuid().to_string()}), &own, &ctx).unwrap();
        assert_eq!(v["recent_inbound"].as_array().unwrap().len(), 1);

        let foreign = Caller::Agent {
            session_id: SessionId::new(),
            agent_group_id: ag,
            messaging_group_id: None,
        };
        let v = get(&json!({"id": s.as_uuid().to_string()}), &foreign, &ctx).unwrap();
        // Row-only response: previews are withheld entirely.
        assert!(v.get("recent_inbound").is_none());
        assert!(v.get("recent_outbound").is_none());
        // But the session row itself still comes back (back-compat).
        assert_eq!(v["id"].as_str().unwrap(), s.as_uuid().to_string());
    }

    #[test]
    fn tail_merges_rows_in_time_order_with_directions() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        seed_session_dbs(tmp.path(), ag, s, 2, 2);
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = tail(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();
        let rows = v["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        // Interleaved by timestamp: in, out, in, out.
        let dirs: Vec<&str> = rows
            .iter()
            .map(|r| r["direction"].as_str().unwrap())
            .collect();
        assert_eq!(dirs, vec!["in", "out", "in", "out"]);
        let previews: Vec<&str> = rows
            .iter()
            .map(|r| r["preview"].as_str().unwrap())
            .collect();
        assert_eq!(
            previews,
            vec!["inbound 0", "outbound 0", "inbound 1", "outbound 1"]
        );
        // Cursors reflect the max seq seen per direction.
        assert!(v["last_in_seq"].as_i64().unwrap() > 0);
        assert!(v["last_out_seq"].as_i64().unwrap() > 0);
    }

    #[test]
    fn tail_since_seq_returns_only_newer_rows() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        seed_session_dbs(tmp.path(), ag, s, 2, 2);
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let first = tail(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();
        let (in_seq, out_seq) = (first["last_in_seq"].clone(), first["last_out_seq"].clone());

        // Nothing new yet.
        let again = tail(
            &json!({
                "id": s.as_uuid().to_string(),
                "since_in_seq": in_seq,
                "since_out_seq": out_seq,
            }),
            &Caller::Host,
            &ctx,
        )
        .unwrap();
        assert!(again["rows"].as_array().unwrap().is_empty());
        // Cursors are carried forward even when no rows arrive.
        assert_eq!(again["last_in_seq"], first["last_in_seq"]);
        assert_eq!(again["last_out_seq"], first["last_out_seq"]);

        // A new outbound row lands (concurrent-writer shape: WAL reader).
        let paths = SessionPaths::new(tmp.path(), ag, s);
        let out_conn = open_outbound(&paths).unwrap();
        messages_out::insert(&out_conn, &outbound_write(MessageKind::Chat, "fresh", 99)).unwrap();

        let update = tail(
            &json!({
                "id": s.as_uuid().to_string(),
                "since_in_seq": first["last_in_seq"],
                "since_out_seq": first["last_out_seq"],
            }),
            &Caller::Host,
            &ctx,
        )
        .unwrap();
        let rows = update["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["preview"], "fresh");
        assert_eq!(rows[0]["direction"], "out");
    }

    #[test]
    fn tail_foreign_agent_caller_is_denied() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let foreign = Caller::Agent {
            session_id: SessionId::new(),
            agent_group_id: ag,
            messaging_group_id: None,
        };
        let err = tail(&json!({"id": s.as_uuid().to_string()}), &foreign, &ctx).unwrap_err();
        assert_eq!(err.code, "permission_denied");
    }

    #[test]
    fn tail_missing_session_is_not_found() {
        let db = CentralDb::open_in_memory().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let err = tail(
            &json!({"id": SessionId::new().as_uuid().to_string()}),
            &Caller::Host,
            &ctx,
        )
        .unwrap_err();
        assert_eq!(err.code, "not_found");
    }

    // ---- W3.4: architect-state fields on sessions.get ---------------------

    /// Seed `<root>/.copperclaw/{services,services.log}` and one project
    /// `myapp/.copperclaw/DECISIONS.md` under the session root.
    fn seed_architect_state(session_root: &Path) {
        let dot = session_root.join(".copperclaw");
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::write(
            dot.join("services"),
            "# started by the databases skill\n\nredis-server --daemonize yes --dir /data/redis\npg_ctl -D /data/pg start\n",
        )
        .unwrap();
        std::fs::write(
            dot.join("services.log"),
            "[2026-07-28T10:00:00Z] $ redis-server --daemonize yes --dir /data/redis\n\
             some captured output line\n\
             [2026-07-28T10:00:01Z] status: exit status: 0\n\
             [2026-07-28T10:00:01Z] $ pg_ctl -D /data/pg start\n\
             [2026-07-28T10:00:02Z] status: exit status: 0\n",
        )
        .unwrap();
        let proj = session_root.join("myapp").join(".copperclaw");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("DECISIONS.md"),
            "# Decisions\n\n- 2026-07-28: SQLite over Postgres — single writer, simpler ops\n- 2026-07-28: REST over gRPC\n",
        )
        .unwrap();
    }

    #[test]
    fn get_attaches_architect_state_when_files_exist() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let root = SessionPaths::new(tmp.path(), ag, s).root;
        std::fs::create_dir_all(&root).unwrap();
        seed_architect_state(&root);
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();

        // Declared services: comment + blank lines excluded.
        let services = v["services"].as_array().unwrap();
        assert_eq!(services.len(), 2);
        assert_eq!(
            services[0],
            "redis-server --daemonize yes --dir /data/redis"
        );
        assert_eq!(services[1], "pg_ctl -D /data/pg start");

        // Log tail: timestamped lines only — command output skipped.
        let tail = v["services_log_tail"].as_array().unwrap();
        assert_eq!(tail.len(), 4);
        assert!(
            tail.iter()
                .all(|l| l.as_str().unwrap().starts_with("[2026-07-28"))
        );
        assert!(
            !tail
                .iter()
                .any(|l| l.as_str().unwrap().contains("some captured output line"))
        );

        // Per-project decisions tail: blank lines excluded, content kept.
        let decisions = v["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0]["project"], "myapp");
        let dtail = decisions[0]["tail"].as_array().unwrap();
        assert_eq!(dtail.len(), 3);
        assert_eq!(dtail[0], "# Decisions");
        assert!(dtail[2].as_str().unwrap().contains("REST over gRPC"));
    }

    #[test]
    fn get_architect_state_absent_when_files_missing() {
        let (db, s, _ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();
        assert!(v.get("services").is_none());
        assert!(v.get("services_log_tail").is_none());
        assert!(v.get("decisions").is_none());
    }

    #[test]
    fn get_architect_state_withheld_from_foreign_agent_caller() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let root = SessionPaths::new(tmp.path(), ag, s).root;
        std::fs::create_dir_all(&root).unwrap();
        seed_architect_state(&root);
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let foreign = Caller::Agent {
            session_id: SessionId::new(),
            agent_group_id: ag,
            messaging_group_id: None,
        };
        let v = get(&json!({"id": s.as_uuid().to_string()}), &foreign, &ctx).unwrap();
        assert!(v.get("services").is_none());
        assert!(v.get("services_log_tail").is_none());
        assert!(v.get("decisions").is_none());
    }

    #[test]
    fn get_architect_state_caps_are_enforced() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let root = SessionPaths::new(tmp.path(), ag, s).root;
        let dot = root.join(".copperclaw");
        std::fs::create_dir_all(&dot).unwrap();

        // 50 service lines, one of them very long.
        let mut services = String::new();
        for i in 0..50 {
            services.push_str(&format!("service-command-{i}\n"));
        }
        services.insert_str(0, &format!("long-{}\n", "x".repeat(1000)));
        std::fs::write(dot.join("services"), services).unwrap();

        // A log far beyond the byte cap, ending with 40 stamped lines.
        let mut log = "noise\n".repeat(10_000);
        for i in 0..40 {
            log.push_str(&format!(
                "[2026-07-28T10:00:{i:02}Z] status: exit status: 0\n"
            ));
        }
        std::fs::write(dot.join("services.log"), log).unwrap();

        // 15 project dirs with DECISIONS.md, one file with 100 lines.
        for p in 0..15 {
            let proj = root.join(format!("proj-{p:02}")).join(".copperclaw");
            std::fs::create_dir_all(&proj).unwrap();
            let mut body = String::new();
            for i in 0..100 {
                body.push_str(&format!("- decision {i}\n"));
            }
            std::fs::write(proj.join("DECISIONS.md"), body).unwrap();
        }

        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();

        let services = v["services"].as_array().unwrap();
        assert_eq!(services.len(), SERVICES_LINES_MAX);
        let capped_line = services[0].as_str().unwrap();
        assert_eq!(capped_line.chars().count(), STATE_LINE_CHARS + 3);
        assert!(capped_line.ends_with("..."));

        let tail = v["services_log_tail"].as_array().unwrap();
        assert_eq!(tail.len(), SERVICES_LOG_TAIL_LINES);
        // The newest stamped lines survive the tail cut.
        assert!(tail[9].as_str().unwrap().contains("10:00:39Z"));

        let decisions = v["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), DECISIONS_PROJECTS_MAX);
        for d in decisions {
            let dtail = d["tail"].as_array().unwrap();
            assert_eq!(dtail.len(), DECISIONS_TAIL_LINES);
            // Newest entries win.
            assert_eq!(dtail[DECISIONS_TAIL_LINES - 1], "- decision 99");
        }
    }

    #[test]
    fn architect_state_lines_are_sanitized() {
        let (db, s, ag) = db_with_session();
        let tmp = tempfile::tempdir().unwrap();
        let root = SessionPaths::new(tmp.path(), ag, s).root;
        let dot = root.join(".copperclaw");
        std::fs::create_dir_all(&dot).unwrap();
        let secret = format!("sk-ant-{}", "a1B2".repeat(20));
        std::fs::write(
            dot.join("services"),
            format!("run --key {secret} \u{1b}[2Jcleared\n"),
        )
        .unwrap();
        let ctx = ctx_with(db, tmp.path().to_path_buf());
        let v = get(&json!({"id": s.as_uuid().to_string()}), &Caller::Host, &ctx).unwrap();
        let line = v["services"][0].as_str().unwrap();
        assert!(!line.contains(&secret), "secret must be redacted: {line}");
        assert!(line.contains("[REDACTED]"));
        assert!(
            !line.contains('\u{1b}'),
            "terminal escapes must not pass through raw: {line:?}"
        );
    }

    #[test]
    fn read_file_tail_drops_leading_partial_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.log");
        // First line is huge; a capped tail read lands mid-line and the
        // fragment must be dropped so only whole lines surface.
        let body = format!("{}\nwhole line\n", "y".repeat(9000));
        std::fs::write(&path, body).unwrap();
        let tail = read_file_tail(&path, STATE_FILE_READ_BYTES).unwrap();
        assert_eq!(tail, "whole line\n");
        // Missing file: best-effort None, never an error.
        assert!(read_file_tail(&tmp.path().join("missing"), 100).is_none());
    }

    #[test]
    fn preview_extracts_chat_text_and_truncates() {
        let long = "x".repeat(300);
        let p = content_preview(&json!({"text": long}).to_string());
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 3);
        assert!(p.ends_with("..."));
        // Non-chat shapes surface compact JSON.
        let p = content_preview(&json!({"breadcrumb": {"tool": "shell"}}).to_string());
        assert!(p.contains("breadcrumb"));
        // Non-JSON content passes through raw.
        assert_eq!(content_preview("plain"), "plain");
    }

    #[test]
    fn preview_redacts_secrets_before_truncation() {
        let secret = format!("sk-ant-{}", "a1B2".repeat(20));
        let text = format!("here is a key {secret} in a message");
        let p = content_preview(&json!({"text": text}).to_string());
        assert!(!p.contains(&secret), "raw secret must not survive: {p}");
        assert!(p.contains("[REDACTED]"));
        // Newlines are flattened so a preview is always one line.
        let p = content_preview(&json!({"text": "a\nb\r\nc"}).to_string());
        assert!(!p.contains('\n'));
    }
}
