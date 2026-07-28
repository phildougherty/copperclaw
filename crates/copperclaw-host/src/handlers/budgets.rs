//! Handlers for `budgets.*` commands.

use super::usage::{cost_cap_micros, priced_spend_since};
use super::{db_err, parse_agent_group_id, req_str};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::{agent_turns, group_budgets};
use serde_json::{Value, json};

/// `budgets.list` — every configured budget, with today's spend beside
/// each cap so a breached budget is visible where the caps live.
///
/// Additive columns per row: `tokens_today` (input + output since UTC
/// midnight), `cost_today_micros` (priced spend since UTC midnight — the
/// SAME rollup the `daily_cost_cap` spawn gate compares, so list and gate
/// can never disagree), `cost_today_unpriced_turns` (turns excluded from
/// that sum because their model has no known price; when > 0 the dollar
/// figure is a floor, not a total), and the host-computed breach flags
/// `over_daily_token_cap` / `over_daily_cost_cap`.
pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let rows = group_budgets::list(central).map_err(db_err)?;
    let midnight = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is a valid time")
        .and_utc();
    let mut out = Vec::with_capacity(rows.len());
    for b in &rows {
        let ag_id = b.agent_group_id.as_uuid().to_string();
        let tokens_today = agent_turns::tokens_since(central, &ag_id, midnight).map_err(db_err)?;
        let spend = priced_spend_since(central, &ag_id, midnight).map_err(db_err)?;
        let over_token_cap = b.daily_token_cap.is_some_and(|cap| tokens_today >= cap);
        let over_cost_cap = b
            .daily_cost_cap
            .and_then(cost_cap_micros)
            .is_some_and(|cap_micros| spend.micros >= cap_micros);
        let mut v = budget_to_json(b);
        let obj = v.as_object_mut().expect("budget_to_json returns an object");
        obj.insert("tokens_today".into(), json!(tokens_today));
        obj.insert("cost_today_micros".into(), json!(spend.micros));
        obj.insert(
            "cost_today_unpriced_turns".into(),
            json!(spend.unpriced_turns),
        );
        obj.insert("over_daily_token_cap".into(), json!(over_token_cap));
        obj.insert("over_daily_cost_cap".into(), json!(over_cost_cap));
        out.push(v);
    }
    Ok(json!(out))
}

/// `budgets.set` — upsert. Pass `daily_tokens: null` (via the
/// `--clear` flag in cclaw) to remove the daily cap.
/// `turns_per_minute` and `turns_per_hour` follow the same convention:
/// absent = keep existing value, 0 or null = remove cap.
/// `daily_cost` (USD per day, fractional allowed) follows it too:
/// absent = keep existing value, 0/negative or null = remove cap.
pub fn set(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let agent_group_id = parse_agent_group_id(args, "agent_group_id")?;

    // Helper: parse an optional non-negative integer cap from the args.
    // null → None (remove), 0 → None (remove), positive → Some(n).
    let parse_cap = |field: &str| -> Result<Option<Option<i64>>, ErrorPayload> {
        match args.get(field) {
            None => Ok(None), // field absent — don't touch this column
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::Number(n)) => {
                let n = n.as_i64().ok_or_else(|| {
                    ErrorPayload::new("bad_request", format!("{field} must be an integer"))
                })?;
                Ok(Some(if n <= 0 { None } else { Some(n) }))
            }
            Some(other) => Err(ErrorPayload::new(
                "bad_request",
                format!("{field} must be a non-negative integer or null, got {other}"),
            )),
        }
    };

    // `daily_tokens` is required (or null/0) when the field is present.
    // Kept required for backwards compatibility with the existing handler
    // contract; the new rate-limit flags are optional.
    let daily_token_cap = match args.get("daily_tokens") {
        Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let n = n.as_i64().ok_or_else(|| {
                ErrorPayload::new(
                    "bad_request",
                    format!("daily_tokens must be an integer, got {n}"),
                )
            })?;
            if n <= 0 { None } else { Some(n) }
        }
        Some(other) => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("daily_tokens must be a non-negative integer or null, got {other}"),
            ));
        }
        None => {
            return Err(ErrorPayload::new(
                "bad_request",
                "daily_tokens is required (pass null to clear)",
            ));
        }
    };

    // Fetch existing row so we can preserve any caps the caller didn't specify.
    let existing = group_budgets::get(central, agent_group_id).map_err(db_err)?;
    let prev = existing.as_ref();

    let agent_turns_per_minute_cap = match parse_cap("turns_per_minute")? {
        Some(v) => v,
        None => prev.and_then(|r| r.agent_turns_per_minute_cap),
    };
    let agent_turns_per_hour_cap = match parse_cap("turns_per_hour")? {
        Some(v) => v,
        None => prev.and_then(|r| r.agent_turns_per_hour_cap),
    };

    // `daily_cost` — the dollar cap (USD per day), enforced by the same
    // spawn gate as `daily_token_cap`. Absent = preserve the stored cap
    // (before this field existed, every `budgets.set` silently wiped it);
    // null or <= 0 = remove; positive finite number = set.
    let daily_cost_cap = match args.get("daily_cost") {
        None => prev.and_then(|r| r.daily_cost_cap),
        Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let v = n.as_f64().ok_or_else(|| {
                ErrorPayload::new(
                    "bad_request",
                    format!("daily_cost must be a number, got {n}"),
                )
            })?;
            if !v.is_finite() {
                return Err(ErrorPayload::new(
                    "bad_request",
                    "daily_cost must be a finite number",
                ));
            }
            if v <= 0.0 { None } else { Some(v) }
        }
        Some(other) => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!("daily_cost must be a non-negative number or null, got {other}"),
            ));
        }
    };

    let _ = req_str; // silence dead-code lint in trimmed builds
    let row = group_budgets::upsert(
        central,
        group_budgets::UpsertGroupBudget {
            agent_group_id,
            daily_token_cap,
            daily_cost_cap,
            agent_turns_per_minute_cap,
            agent_turns_per_hour_cap,
        },
    )
    .map_err(db_err)?;
    Ok(budget_to_json(&row))
}

fn budget_to_json(b: &group_budgets::GroupBudget) -> Value {
    json!({
        "agent_group_id": b.agent_group_id.as_uuid().to_string(),
        "daily_token_cap": b.daily_token_cap,
        "daily_cost_cap": b.daily_cost_cap,
        "agent_turns_per_minute_cap": b.agent_turns_per_minute_cap,
        "agent_turns_per_hour_cap": b.agent_turns_per_hour_cap,
        "updated_at": b.updated_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::AgentGroupId;
    use serde_json::json;

    #[test]
    fn set_creates_then_updates() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        let v1 = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v1["daily_token_cap"], 1000);
        let v2 = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 2500,
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v2["daily_token_cap"], 2500);
    }

    #[test]
    fn set_null_clears_cap() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        set(
            &json!({"agent_group_id": ag.as_uuid().to_string(), "daily_tokens": 5000}),
            &db,
        )
        .unwrap();
        let v = set(
            &json!({"agent_group_id": ag.as_uuid().to_string(), "daily_tokens": Value::Null}),
            &db,
        )
        .unwrap();
        assert!(v["daily_token_cap"].is_null());
    }

    #[test]
    fn set_rejects_negative_via_cclaw_zero_normalization() {
        // The cclaw `--clear` flag sends null. Bare `--daily-tokens 0`
        // is the operator saying "no cap", and we normalize that to
        // null at the handler so list output is consistent.
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        let v = set(
            &json!({"agent_group_id": ag.as_uuid().to_string(), "daily_tokens": 0}),
            &db,
        )
        .unwrap();
        assert!(v["daily_token_cap"].is_null());
    }

    #[test]
    fn set_missing_field_is_bad_request() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        let err = set(&json!({"agent_group_id": ag.as_uuid().to_string()}), &db).unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn list_returns_existing_rows() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        set(
            &json!({"agent_group_id": ag.as_uuid().to_string(), "daily_tokens": 42}),
            &db,
        )
        .unwrap();
        let v = list(&json!({}), &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["daily_token_cap"], 42);
    }

    #[test]
    fn set_turns_per_minute_and_hour_round_trip() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        let v = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "turns_per_minute": 5,
                "turns_per_hour": 60,
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["agent_turns_per_minute_cap"], 5);
        assert_eq!(v["agent_turns_per_hour_cap"], 60);
        // Verify list output includes the new caps.
        let rows = list(&json!({}), &db).unwrap();
        let row = &rows.as_array().unwrap()[0];
        assert_eq!(row["agent_turns_per_minute_cap"], 5);
        assert_eq!(row["agent_turns_per_hour_cap"], 60);
    }

    #[test]
    fn set_zero_turns_per_minute_clears_cap() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "turns_per_minute": 5,
            }),
            &db,
        )
        .unwrap();
        let v = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "turns_per_minute": 0,
            }),
            &db,
        )
        .unwrap();
        assert!(v["agent_turns_per_minute_cap"].is_null());
    }

    /// Insert one `agent_turns` row for `ag` timestamped now (inside the
    /// UTC-midnight window `budgets.list` rolls up).
    fn seed_turn(db: &CentralDb, ag: &str, model: &str, provider: &str, input: i64, output: i64) {
        copperclaw_db::tables::agent_turns::insert(
            db,
            &copperclaw_db::tables::agent_turns::NewAgentTurn {
                session_id: "s-1".into(),
                agent_group_id: ag.into(),
                seq: 1,
                model: model.into(),
                provider: provider.into(),
                input_tokens: input,
                output_tokens: output,
                started_at: chrono::Utc::now(),
                ended_at: chrono::Utc::now(),
                status: "ok".into(),
                error: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn set_daily_cost_round_trips_and_zero_clears() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        let v = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "daily_cost": 2.5,
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["daily_cost_cap"], 2.5);
        // 0 removes the cost cap (same convention as the other caps).
        let v = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "daily_cost": 0,
            }),
            &db,
        )
        .unwrap();
        assert!(v["daily_cost_cap"].is_null());
    }

    #[test]
    fn set_omitting_daily_cost_preserves_existing() {
        // Regression: before the `daily_cost` field existed, every
        // `budgets.set` silently wiped a stored cost cap.
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "daily_cost": 1.25,
            }),
            &db,
        )
        .unwrap();
        let v = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 2000,
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["daily_cost_cap"], 1.25);
    }

    #[test]
    fn set_rejects_non_numeric_daily_cost() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        let err = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "daily_cost": "lots",
            }),
            &db,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_request");
    }

    #[test]
    fn list_includes_today_spend_and_breach_flags() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        // Cap $0.01 (10_000 micros) + token cap well above usage.
        set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1_000_000,
                "daily_cost": 0.01,
            }),
            &db,
        )
        .unwrap();
        let ag_str = ag.as_uuid().to_string();
        // Priced spend: sonnet 1000 in + 500 out = 10_500 micros (over).
        seed_turn(&db, &ag_str, "claude-sonnet-4-6", "anthropic", 1_000, 500);
        // Unpriced spend: excluded from the sum, counted separately.
        seed_turn(&db, &ag_str, "mystery-model-9", "somevendor", 10, 10);

        let rows = list(&json!({}), &db).unwrap();
        let row = &rows.as_array().unwrap()[0];
        assert_eq!(row["daily_cost_cap"], 0.01);
        assert_eq!(row["tokens_today"], 1_520);
        assert_eq!(row["cost_today_micros"], 10_500);
        assert_eq!(row["cost_today_unpriced_turns"], 1);
        assert_eq!(row["over_daily_cost_cap"], true);
        assert_eq!(row["over_daily_token_cap"], false);
    }

    #[test]
    fn list_flags_false_when_caps_unset() {
        // Cap unset means no cost gate: heavy spend, no breach flag.
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": Value::Null,
            }),
            &db,
        )
        .unwrap();
        let ag_str = ag.as_uuid().to_string();
        seed_turn(
            &db,
            &ag_str,
            "claude-sonnet-4-6",
            "anthropic",
            1_000_000,
            1_000_000,
        );
        let rows = list(&json!({}), &db).unwrap();
        let row = &rows.as_array().unwrap()[0];
        assert_eq!(row["over_daily_token_cap"], false);
        assert_eq!(row["over_daily_cost_cap"], false);
        // Spend is still visible even with no caps configured.
        assert_eq!(row["cost_today_micros"], 18_000_000);
    }

    #[test]
    fn set_omitting_rate_caps_preserves_existing() {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = AgentGroupId::new();
        // First call sets both rate caps.
        set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 1000,
                "turns_per_minute": 10,
                "turns_per_hour": 100,
            }),
            &db,
        )
        .unwrap();
        // Second call does not include rate-cap fields — they should be preserved.
        let v = set(
            &json!({
                "agent_group_id": ag.as_uuid().to_string(),
                "daily_tokens": 2000,
            }),
            &db,
        )
        .unwrap();
        assert_eq!(v["agent_turns_per_minute_cap"], 10);
        assert_eq!(v["agent_turns_per_hour_cap"], 100);
    }
}
