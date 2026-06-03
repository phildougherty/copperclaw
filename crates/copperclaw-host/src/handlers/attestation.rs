//! Handler for `attestation.list` — read the spawn-time image+runner
//! attestation rows out of the append-only audit log.
//!
//! Every container spawn records a `container.attestation` audit row carrying
//! the image tag + runtime-reported image content digest + host runner-binary
//! digest (see [`crate::attestation`]). This handler surfaces just those rows
//! so an operator can run `cclaw attestation list` and see exactly which
//! third-party-buildable image and which runner each session ran — without
//! scrolling the full mutation audit. Read-only.

use super::db_err;
use crate::attestation::ATTESTATION_COMMAND;
use chrono::{DateTime, Duration, Utc};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::audit_log;
use serde_json::{Value, json};

/// `attestation.list` — recent spawn attestation rows since `since`.
///
/// Arguments:
/// - `since` (string, optional): look-back window (`5m` / `24h` / `7d` /
///   bare-seconds). Defaults to `24h`.
/// - `limit` (int, optional): max rows. Defaults to 50.
pub fn list(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let since_str = args
        .get("since")
        .and_then(Value::as_str)
        .unwrap_or("24h")
        .to_string();
    let since = parse_since(&since_str)?;
    let limit = args
        .get("limit")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .unwrap_or(50);
    // Over-fetch a little then filter to attestation rows; the audit table is
    // append-only and small per window, so a 5x over-fetch is cheap and avoids
    // a schema-specific query.
    let fetch = limit.saturating_mul(5).min(2000);
    let rows = audit_log::list_recent(central, since, fetch).map_err(db_err)?;
    let out: Vec<Value> = rows
        .iter()
        .filter(|r| r.command == ATTESTATION_COMMAND)
        .take(usize::try_from(limit).unwrap_or(50))
        .map(row_to_json)
        .collect();
    Ok(json!(out))
}

/// Flatten an attestation audit row into a digest-forward shape (the digests
/// the spawn recorded live in the `args` JSON; hoist them to top level).
fn row_to_json(e: &audit_log::AuditEntry) -> Value {
    let args: Value = serde_json::from_str(&e.args).unwrap_or(Value::Null);
    json!({
        "ts": e.ts.to_rfc3339(),
        "session": e.caller_session,
        "agent_group": e.caller_agent_group,
        "image_tag": args.get("image_tag").cloned().unwrap_or(Value::Null),
        "image_digest": args.get("image_digest").cloned().unwrap_or(Value::Null),
        "runner_digest": args.get("runner_digest").cloned().unwrap_or(Value::Null),
    })
}

/// Parse a human look-back window into an absolute timestamp. Mirrors
/// `handlers::audit::parse_since` (kept local to avoid widening that module's
/// visibility).
fn parse_since(s: &str) -> Result<DateTime<Utc>, ErrorPayload> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(Utc::now() - Duration::hours(24));
    }
    let (num, suffix) = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .map_or((trimmed, ""), |i| trimmed.split_at(i));
    let n: i64 = num.parse().map_err(|_| {
        ErrorPayload::new("bad_request", format!("invalid `since` value: {trimmed:?}"))
    })?;
    let dur = match suffix.trim() {
        "" | "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        other => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("unknown `since` suffix: {other:?} (use s/m/h/d)"),
            ));
        }
    };
    Ok(Utc::now() - dur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::audit_log::AuditEntry;

    fn att_row(image_digest: &str) -> AuditEntry {
        AuditEntry {
            ts: Utc::now(),
            caller_kind: "host".into(),
            caller_session: Some("sess-1".into()),
            caller_agent_group: Some("ag-1".into()),
            command: ATTESTATION_COMMAND.into(),
            args: json!({
                "image_tag": "copperclaw/session:sha256-abc",
                "image_digest": image_digest,
                "runner_digest": "cafef00d",
            })
            .to_string(),
            result: "ok".into(),
            error_code: None,
            error_message: None,
            latency_ms: 0,
        }
    }

    fn other_row() -> AuditEntry {
        AuditEntry {
            ts: Utc::now(),
            caller_kind: "host".into(),
            caller_session: None,
            caller_agent_group: None,
            command: "groups.create".into(),
            args: "{}".into(),
            result: "ok".into(),
            error_code: None,
            error_message: None,
            latency_ms: 0,
        }
    }

    #[test]
    fn list_returns_only_attestation_rows_with_digests() {
        let db = CentralDb::open_in_memory().unwrap();
        audit_log::insert(&db, &other_row()).unwrap();
        audit_log::insert(&db, &att_row("sha256:deadbeef")).unwrap();
        audit_log::insert(&db, &other_row()).unwrap();

        let v = list(&json!({"since": "1h"}), &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1, "only the attestation row should surface");
        assert_eq!(arr[0]["image_digest"], "sha256:deadbeef");
        assert_eq!(arr[0]["runner_digest"], "cafef00d");
        assert_eq!(arr[0]["session"], "sess-1");
        assert_eq!(arr[0]["image_tag"], "copperclaw/session:sha256-abc");
    }

    #[test]
    fn list_respects_limit() {
        let db = CentralDb::open_in_memory().unwrap();
        for _ in 0..5 {
            audit_log::insert(&db, &att_row("sha256:x")).unwrap();
        }
        let v = list(&json!({"since": "1h", "limit": 2}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[test]
    fn list_empty_when_no_attestation_rows() {
        let db = CentralDb::open_in_memory().unwrap();
        audit_log::insert(&db, &other_row()).unwrap();
        let v = list(&json!({}), &db).unwrap();
        assert!(v.as_array().unwrap().is_empty());
    }
}
