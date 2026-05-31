//! `usage.*` handlers — per-agent-group token rollups read from the
//! `agent_turns` table the runner populates via `usage_report` system
//! rows.

use super::{db_err, opt_str};
use chrono::{DateTime, Duration, Utc};
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::agent_turns;
use copperclaw_cclaw::ErrorPayload;
use serde_json::{json, Value};

/// `usage.rollup` — list per-group token counts since `since`.
pub fn rollup(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let since_str = opt_str(args, "since").unwrap_or_else(|| "24h".to_string());
    let since = parse_since(&since_str)?;
    let rows = agent_turns::rollup_since(central, since).map_err(db_err)?;
    Ok(json!(rows.iter().map(rollup_to_json).collect::<Vec<_>>()))
}

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

fn rollup_to_json(r: &agent_turns::UsageRollup) -> Value {
    json!({
        "agent_group_id": r.agent_group_id,
        "turns": r.turns,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "total_tokens": r.input_tokens + r.output_tokens,
        "first_at": r.first_at.to_rfc3339(),
        "last_at": r.last_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_turns::NewAgentTurn;

    #[test]
    fn rollup_returns_per_group_sums() {
        let db = CentralDb::open_in_memory().unwrap();
        for (ag, input, output) in [
            ("ag-a", 100, 200),
            ("ag-a", 50, 50),
            ("ag-b", 10, 20),
        ] {
            agent_turns::insert(
                &db,
                &NewAgentTurn {
                    session_id: "s-1".into(),
                    agent_group_id: ag.into(),
                    seq: 1,
                    model: "claude".into(),
                    provider: "anthropic".into(),
                    input_tokens: input,
                    output_tokens: output,
                    started_at: Utc::now(),
                    ended_at: Utc::now(),
                    status: "ok".into(),
                    error: None,
                },
            )
            .unwrap();
        }
        let v = rollup(&json!({"since": "1h"}), &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Ordered desc by output_tokens — ag-a has 250 total output.
        assert_eq!(arr[0]["agent_group_id"], "ag-a");
        assert_eq!(arr[0]["input_tokens"], 150);
        assert_eq!(arr[0]["output_tokens"], 250);
        assert_eq!(arr[0]["total_tokens"], 400);
        assert_eq!(arr[0]["turns"], 2);
    }

    #[test]
    fn rollup_rejects_bad_since() {
        let db = CentralDb::open_in_memory().unwrap();
        let err = rollup(&json!({"since": "asdf"}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn rollup_default_window_returns_recent() {
        let db = CentralDb::open_in_memory().unwrap();
        agent_turns::insert(
            &db,
            &NewAgentTurn {
                session_id: "s".into(),
                agent_group_id: "ag".into(),
                seq: 1,
                model: "claude".into(),
                provider: "anthropic".into(),
                input_tokens: 5,
                output_tokens: 5,
                started_at: Utc::now(),
                ended_at: Utc::now(),
                status: "ok".into(),
                error: None,
            },
        )
        .unwrap();
        let v = rollup(&json!({}), &db).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
    }
}
