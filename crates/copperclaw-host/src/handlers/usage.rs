//! `usage.*` handlers — per-agent-group token rollups read from the
//! `agent_turns` table the runner populates via `usage_report` system
//! rows.

use super::{db_err, opt_str};
use chrono::{DateTime, Duration, Utc};
use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::tables::agent_turns;
use copperclaw_types::pricing::{self, ModelPrice, PRICING_AS_OF};
use serde_json::{Value, json};

/// `usage.rollup` — list per-group token counts since `since`.
pub fn rollup(args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let since_str = opt_str(args, "since").unwrap_or_else(|| "24h".to_string());
    let since = parse_since(&since_str)?;
    let rows = agent_turns::rollup_since(central, since).map_err(db_err)?;
    let by_model = agent_turns::rollup_by_model_since(central, since).map_err(db_err)?;
    Ok(json!(
        rows.iter()
            .map(|r| rollup_to_json(r, &by_model))
            .collect::<Vec<_>>()
    ))
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

/// Cost of `input`/`output` tokens at `price`, in micro-dollars.
///
/// Prices are micros **per megatoken**, so the products are summed first
/// and divided by 1M once, all in `u128` — multiplying first is what keeps
/// small token counts from truncating to zero (e.g. 100 input tokens at
/// $3/MTok is `100 * 3_000_000 / 1_000_000 = 300` micros, whereas dividing
/// first would floor to 0). The result comfortably fits `u64`.
///
/// `pub(crate)` so the container manager's `daily_cost_cap` gate and the
/// `budgets.list` handler reuse the exact same arithmetic (via
/// [`priced_spend_since`]) instead of duplicating pricing math.
pub(crate) fn cost_micros(price: &ModelPrice, input_tokens: i64, output_tokens: i64) -> u64 {
    let input = u128::try_from(input_tokens.max(0)).unwrap_or(0);
    let output = u128::try_from(output_tokens.max(0)).unwrap_or(0);
    let micros = (input * u128::from(price.input_per_mtok_micros)
        + output * u128::from(price.output_per_mtok_micros))
        / 1_000_000;
    u64::try_from(micros).unwrap_or(u64::MAX)
}

/// Convert the operator-facing dollar cap (`group_budgets.daily_cost_cap`,
/// stored as an f64 in **USD per day**) to micro-dollars for comparison
/// against the integer-micros spend arithmetic. Non-finite or non-positive
/// caps read as "no cap".
///
/// `pub(crate)` so the spawn gate (`container_manager/budgets.rs`) and the
/// `budgets.list` breach flags convert the cap identically.
pub(crate) fn cost_cap_micros(cap_dollars: f64) -> Option<u64> {
    if !cap_dollars.is_finite() || cap_dollars <= 0.0 {
        return None;
    }
    // Operator-entered dollar amounts are far inside f64's exact integer
    // range; round to the nearest micro-dollar.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let micros = (cap_dollars * 1_000_000.0).round() as u64;
    Some(micros)
}

/// One agent group's priced spend over a window, as computed by
/// [`priced_spend_since`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PricedSpend {
    /// Micro-dollar sum over the `(model, provider)` slices that HAVE a
    /// known price. Slices with no price are excluded, so this is a
    /// floor on true spend, never an overstatement.
    pub micros: u64,
    /// Turns whose `(model, provider)` could not be priced and therefore
    /// contribute nothing to `micros`. Callers that display or gate on
    /// `micros` use this to say so instead of silently treating unknown
    /// cost as zero.
    pub unpriced_turns: i64,
}

/// Roll up one agent group's priced spend since `since`.
///
/// Shared cost-arithmetic seam: the `daily_cost_cap` spawn gate
/// (`container_manager/budgets.rs`), the credential broker's budget
/// verdict, and the `budgets.list` handler all price spend through this
/// one function so they can never disagree with `cclaw usage`.
///
/// Unpriced models (no entry in `copperclaw_types::pricing`) are NOT
/// counted as zero silently: their turns are excluded from `micros` and
/// reported in `unpriced_turns`. A dollar cap therefore gates on what is
/// computable — the honest alternative to either inventing a price or
/// letting an unknown model bypass every priced one.
pub(crate) fn priced_spend_since(
    central: &CentralDb,
    agent_group_id: &str,
    since: DateTime<Utc>,
) -> Result<PricedSpend, copperclaw_db::DbError> {
    let by_model = agent_turns::rollup_by_model_since(central, since)?;
    let mut spend = PricedSpend {
        micros: 0,
        unpriced_turns: 0,
    };
    for m in by_model
        .iter()
        .filter(|m| m.agent_group_id == agent_group_id)
    {
        match pricing::price_for(&m.provider, &m.model) {
            Some(p) => {
                spend.micros =
                    spend
                        .micros
                        .saturating_add(cost_micros(&p, m.input_tokens, m.output_tokens));
            }
            None => spend.unpriced_turns += m.turns,
        }
    }
    Ok(spend)
}

/// One `(model, provider)` slice of a group's usage, with its priced cost.
/// Unknown model/provider -> `"cost_micros": null` — surfaces render a
/// blank, never a confidently wrong zero.
fn model_rollup_to_json(m: &agent_turns::ModelUsageRollup) -> Value {
    let cost = pricing::price_for(&m.provider, &m.model)
        .map(|p| cost_micros(&p, m.input_tokens, m.output_tokens));
    json!({
        "model": m.model,
        "provider": m.provider,
        "turns": m.turns,
        "input_tokens": m.input_tokens,
        "output_tokens": m.output_tokens,
        "cost_micros": cost,
    })
}

/// Backward compatibility is binding here: every pre-existing key
/// (`agent_group_id`, `turns`, `input_tokens`, `output_tokens`,
/// `total_tokens`, `first_at`, `last_at`) keeps its name and shape so no
/// `cclaw` output regresses. `models`, `cost_micros`, and `pricing_as_of`
/// are additive. The group-level `cost_micros` is null unless *every*
/// model slice priced — a partial sum presented as a total would be the
/// same lie as a zero.
fn rollup_to_json(
    r: &agent_turns::UsageRollup,
    by_model: &[agent_turns::ModelUsageRollup],
) -> Value {
    let slices: Vec<&agent_turns::ModelUsageRollup> = by_model
        .iter()
        .filter(|m| m.agent_group_id == r.agent_group_id)
        .collect();
    let group_cost: Option<u64> = slices.iter().try_fold(0u64, |acc, m| {
        pricing::price_for(&m.provider, &m.model)
            .map(|p| acc.saturating_add(cost_micros(&p, m.input_tokens, m.output_tokens)))
    });
    let models: Vec<Value> = slices.iter().map(|m| model_rollup_to_json(m)).collect();
    json!({
        "agent_group_id": r.agent_group_id,
        "turns": r.turns,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "total_tokens": r.input_tokens + r.output_tokens,
        "first_at": r.first_at.to_rfc3339(),
        "last_at": r.last_at.to_rfc3339(),
        "models": models,
        "cost_micros": group_cost,
        "pricing_as_of": PRICING_AS_OF,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_turns::NewAgentTurn;

    #[test]
    fn rollup_returns_per_group_sums() {
        let db = CentralDb::open_in_memory().unwrap();
        for (ag, input, output) in [("ag-a", 100, 200), ("ag-a", 50, 50), ("ag-b", 10, 20)] {
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

    fn seed(db: &CentralDb, ag: &str, model: &str, provider: &str, input: i64, output: i64) {
        agent_turns::insert(
            db,
            &NewAgentTurn {
                session_id: "s-1".into(),
                agent_group_id: ag.into(),
                seq: 1,
                model: model.into(),
                provider: provider.into(),
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

    #[test]
    fn rollup_known_model_costs_exactly() {
        let db = CentralDb::open_in_memory().unwrap();
        // Sonnet: $3/MTok in, $15/MTok out.
        // 100 * 3_000_000 / 1_000_000 + 200 * 15_000_000 / 1_000_000
        //   = 300 + 3000 = 3300 micros.
        seed(&db, "ag-cost", "claude-sonnet-4-6", "anthropic", 100, 200);
        let v = rollup(&json!({"since": "1h"}), &db).unwrap();
        let row = &v.as_array().unwrap()[0];
        let models = row["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["model"], "claude-sonnet-4-6");
        assert_eq!(models[0]["provider"], "anthropic");
        assert_eq!(models[0]["turns"], 1);
        assert_eq!(models[0]["input_tokens"], 100);
        assert_eq!(models[0]["output_tokens"], 200);
        assert_eq!(models[0]["cost_micros"], 3300);
        assert_eq!(row["cost_micros"], 3300);
    }

    #[test]
    fn rollup_unknown_model_cost_is_null_never_zero() {
        let db = CentralDb::open_in_memory().unwrap();
        seed(
            &db,
            "ag-unk",
            "mystery-model-9",
            "somevendor",
            1_000_000,
            1_000_000,
        );
        let v = rollup(&json!({"since": "1h"}), &db).unwrap();
        let row = &v.as_array().unwrap()[0];
        let models = row["models"].as_array().unwrap();
        // Pin null specifically: a confidently wrong 0 is the failure mode.
        assert_eq!(models[0]["cost_micros"], Value::Null);
        assert_ne!(models[0]["cost_micros"], json!(0));
        assert_eq!(row["cost_micros"], Value::Null);
        assert_ne!(row["cost_micros"], json!(0));
    }

    #[test]
    fn rollup_group_cost_null_when_any_model_unknown() {
        let db = CentralDb::open_in_memory().unwrap();
        seed(&db, "ag-mix", "claude-sonnet-4-6", "anthropic", 100, 200);
        seed(&db, "ag-mix", "mystery-model-9", "somevendor", 100, 200);
        let v = rollup(&json!({"since": "1h"}), &db).unwrap();
        let row = &v.as_array().unwrap()[0];
        // The priced slice still carries its own cost...
        let models = row["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        let priced = models
            .iter()
            .find(|m| m["model"] == "claude-sonnet-4-6")
            .unwrap();
        assert_eq!(priced["cost_micros"], 3300);
        // ...but the group total refuses to present a partial sum.
        assert_eq!(row["cost_micros"], Value::Null);
    }

    #[test]
    fn rollup_carries_pricing_as_of() {
        let db = CentralDb::open_in_memory().unwrap();
        seed(&db, "ag", "claude-sonnet-4-6", "anthropic", 1, 1);
        let v = rollup(&json!({"since": "1h"}), &db).unwrap();
        let row = &v.as_array().unwrap()[0];
        assert_eq!(row["pricing_as_of"], PRICING_AS_OF);
    }

    #[test]
    fn rollup_full_shape_is_stable() {
        // Pins the complete per-group object: every pre-existing key with
        // its original name and shape, plus the additive cost keys. If this
        // test breaks, `cclaw usage` output has changed.
        let db = CentralDb::open_in_memory().unwrap();
        let t = Utc::now();
        agent_turns::insert(
            &db,
            &NewAgentTurn {
                session_id: "s-1".into(),
                agent_group_id: "ag-shape".into(),
                seq: 1,
                model: "claude-haiku-4-5".into(),
                provider: "anthropic".into(),
                input_tokens: 1_000,
                output_tokens: 2_000,
                started_at: t,
                ended_at: t,
                status: "ok".into(),
                error: None,
            },
        )
        .unwrap();
        let v = rollup(&json!({"since": "1h"}), &db).unwrap();
        // Haiku 4.5: $1/MTok in, $5/MTok out.
        // 1_000 * 1_000_000 / 1_000_000 + 2_000 * 5_000_000 / 1_000_000
        //   = 1_000 + 10_000 = 11_000 micros.
        let expected = json!([{
            "agent_group_id": "ag-shape",
            "turns": 1,
            "input_tokens": 1_000,
            "output_tokens": 2_000,
            "total_tokens": 3_000,
            "first_at": t.to_rfc3339(),
            "last_at": t.to_rfc3339(),
            "models": [{
                "model": "claude-haiku-4-5",
                "provider": "anthropic",
                "turns": 1,
                "input_tokens": 1_000,
                "output_tokens": 2_000,
                "cost_micros": 11_000,
            }],
            "cost_micros": 11_000,
            "pricing_as_of": PRICING_AS_OF,
        }]);
        assert_eq!(v, expected);
    }

    #[test]
    fn priced_spend_since_sums_priced_and_reports_unpriced() {
        let db = CentralDb::open_in_memory().unwrap();
        // Priced: sonnet 100 in + 200 out = 300 + 3000 = 3300 micros.
        seed(&db, "ag-spend", "claude-sonnet-4-6", "anthropic", 100, 200);
        // Unpriced: excluded from the sum, surfaced in unpriced_turns.
        seed(
            &db,
            "ag-spend",
            "mystery-model-9",
            "somevendor",
            1_000_000,
            1_000_000,
        );
        // Another group's turns must not leak into this group's spend.
        seed(&db, "ag-other", "claude-sonnet-4-6", "anthropic", 999, 999);
        let spend = priced_spend_since(&db, "ag-spend", Utc::now() - Duration::hours(1)).unwrap();
        assert_eq!(spend.micros, 3_300);
        assert_eq!(spend.unpriced_turns, 1);
    }

    #[test]
    fn priced_spend_since_empty_group_is_zero_with_no_unpriced() {
        let db = CentralDb::open_in_memory().unwrap();
        seed(&db, "ag-other", "claude-sonnet-4-6", "anthropic", 100, 100);
        let spend = priced_spend_since(&db, "ag-none", Utc::now() - Duration::hours(1)).unwrap();
        assert_eq!(spend.micros, 0);
        assert_eq!(spend.unpriced_turns, 0);
    }

    #[test]
    fn priced_spend_since_ollama_is_priced_at_zero_not_unpriced() {
        // Known-free local provider: positive knowledge of $0, so the
        // turns are priced (at zero), not excluded as unpriced.
        let db = CentralDb::open_in_memory().unwrap();
        seed(&db, "ag-local", "qwen3.6:27b", "ollama", 100_000, 100_000);
        let spend = priced_spend_since(&db, "ag-local", Utc::now() - Duration::hours(1)).unwrap();
        assert_eq!(spend.micros, 0);
        assert_eq!(spend.unpriced_turns, 0);
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
