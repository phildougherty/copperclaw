//! Handlers for `budgets.*` commands.

use super::{db_err, parse_agent_group_id, req_str};
use ironclaw_db::central::CentralDb;
use ironclaw_db::tables::group_budgets;
use ironclaw_iclaw::ErrorPayload;
use serde_json::{json, Value};

/// `budgets.list` — every configured budget.
pub fn list(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let rows = group_budgets::list(central).map_err(db_err)?;
    Ok(json!(rows.iter().map(budget_to_json).collect::<Vec<_>>()))
}

/// `budgets.set` — upsert. Pass `daily_tokens: null` (via the
/// `--clear` flag in iclaw) to remove the cap.
pub fn set(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let agent_group_id = parse_agent_group_id(args, "agent_group_id")?;
    // `daily_tokens` may be: absent, null, or a positive integer.
    // Treat null and 0 the same — "remove the cap".
    let daily_token_cap = match args.get("daily_tokens") {
        Some(Value::Null) => None,
        Some(Value::Number(n)) => {
            let n = n.as_i64().ok_or_else(|| {
                ErrorPayload::new(
                    "bad_request",
                    format!("daily_tokens must be an integer, got {n}"),
                )
            })?;
            if n <= 0 {
                None
            } else {
                Some(n)
            }
        }
        Some(other) => {
            return Err(ErrorPayload::new(
                "bad_request",
                format!(
                    "daily_tokens must be a non-negative integer or null, got {other}"
                ),
            ));
        }
        None => {
            return Err(ErrorPayload::new(
                "bad_request",
                "daily_tokens is required (pass null to clear)",
            ));
        }
    };
    let _ = req_str; // silence dead-code lint in trimmed builds
    let row = group_budgets::upsert(
        central,
        group_budgets::UpsertGroupBudget {
            agent_group_id,
            daily_token_cap,
            daily_cost_cap: None,
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
        "updated_at": b.updated_at.to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_types::AgentGroupId;
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
    fn set_rejects_negative_via_iclaw_zero_normalization() {
        // The iclaw `--clear` flag sends null. Bare `--daily-tokens 0`
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
        let err = set(
            &json!({"agent_group_id": ag.as_uuid().to_string()}),
            &db,
        )
        .unwrap_err();
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
}
