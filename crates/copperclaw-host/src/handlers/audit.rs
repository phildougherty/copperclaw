//! Handlers for `audit.*` commands. Read-only — no caller-side mutation
//! verbs since the audit table is append-only and managed by the dispatch
//! layer itself.

use super::{db_err, opt_str};
use chrono::{DateTime, Duration, Utc};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log;
use serde_json::{Value, json};

/// `audit.list` — return the most recent audit entries since `since`.
pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let since_str = opt_str(args, "since").unwrap_or_else(|| "24h".to_string());
    let since = parse_since(&since_str)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .unwrap_or(50);
    let rows = audit_log::list_recent(central, since, limit).map_err(db_err)?;
    Ok(json!(rows.iter().map(entry_to_json).collect::<Vec<_>>()))
}

/// Parse human-friendly look-back windows into an absolute timestamp.
///
/// Accepts:
/// - bare integers — interpreted as seconds (`3600`)
/// - `Ns` / `Nm` / `Nh` / `Nd` shorthand (`5m`, `24h`, `7d`)
fn parse_since(s: &str) -> Result<DateTime<Utc>, ErrorPayload> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Utc::now() - Duration::hours(24));
    }
    let (num, suffix) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map_or((trimmed, ""), |i| trimmed.split_at(i));
    let n: i64 = num.parse().map_err(|_| {
        ErrorPayload::new(
            "bad_request".to_string(),
            format!("invalid `since` value: {trimmed:?}"),
        )
    })?;
    let dur = match suffix.trim() {
        "" | "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        other => {
            return Err(ErrorPayload::new(
                "bad_request".to_string(),
                format!("unknown `since` suffix: {other:?} (use s/m/h/d)"),
            ));
        }
    };
    Ok(Utc::now() - dur)
}

fn entry_to_json(e: &audit_log::AuditEntry) -> Value {
    json!({
        "ts": e.ts.to_rfc3339(),
        "caller_kind": e.caller_kind,
        "caller_session": e.caller_session,
        "caller_agent_group": e.caller_agent_group,
        "command": e.command,
        "args": e.args,
        "result": e.result,
        "error_code": e.error_code,
        "error_message": e.error_message,
        "latency_ms": e.latency_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::audit_log::AuditEntry;
    use serde_json::json;

    #[test]
    fn parse_since_bare_integer_is_seconds() {
        let now = Utc::now();
        let parsed = parse_since("60").unwrap();
        let diff = (now - parsed).num_seconds();
        assert!((59..=61).contains(&diff), "diff = {diff}");
    }

    #[test]
    fn parse_since_supports_h_m_d_suffixes() {
        assert!(parse_since("5m").is_ok());
        assert!(parse_since("24h").is_ok());
        assert!(parse_since("7d").is_ok());
    }

    #[test]
    fn parse_since_rejects_unknown_suffix() {
        let err = parse_since("5w").unwrap_err();
        assert!(err.message.contains("unknown"));
    }

    #[test]
    fn parse_since_rejects_garbage() {
        let err = parse_since("not-a-number").unwrap_err();
        assert!(err.message.contains("invalid"));
    }

    #[test]
    fn list_returns_recent_rows() {
        let db = CentralDb::open_in_memory().unwrap();
        audit_log::insert(
            &db,
            &AuditEntry {
                ts: Utc::now(),
                caller_kind: "host".into(),
                caller_session: None,
                caller_agent_group: None,
                command: "groups.create".into(),
                args: "{}".into(),
                result: "ok".into(),
                error_code: None,
                error_message: None,
                latency_ms: 3,
            },
        )
        .unwrap();
        let v = list(&json!({"since": "1h", "limit": 10}), &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["command"], "groups.create");
        assert_eq!(arr[0]["result"], "ok");
    }

    #[test]
    fn list_default_since_is_24h() {
        let db = CentralDb::open_in_memory().unwrap();
        audit_log::insert(
            &db,
            &AuditEntry {
                ts: Utc::now() - Duration::hours(2),
                caller_kind: "host".into(),
                caller_session: None,
                caller_agent_group: None,
                command: "x".into(),
                args: "{}".into(),
                result: "ok".into(),
                error_code: None,
                error_message: None,
                latency_ms: 0,
            },
        )
        .unwrap();
        let v = list(&json!({}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }
}
