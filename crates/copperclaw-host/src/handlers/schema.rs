//! Handler for `schema.version` — reports the central DB schema version.
//!
//! Returns a JSON object with three fields:
//! - `expected`: number of migrations compiled into this binary.
//! - `applied`: number of migrations recorded in the DB (`schema_version` table).
//! - `status`: `"ok"` | `"pending"` | `"future"`.
//!
//! This is a read-only introspection command available to any caller.

use copperclaw_cclaw::ErrorPayload;
use copperclaw_db::central::CentralDb;
use copperclaw_db::migrate::{applied_central_schema_version, expected_central_schema_version};
use serde_json::{Value, json};

/// Handler: `cclaw schema-version` → `{ expected, applied, status }`.
pub fn version(_args: &Value, central: &CentralDb) -> Result<Value, ErrorPayload> {
    let expected = expected_central_schema_version();
    let conn = central
        .conn()
        .map_err(|e| ErrorPayload::new("db_error", format!("could not open central db: {e}")))?;
    let applied = applied_central_schema_version(&conn)
        .map_err(|e| ErrorPayload::new("db_error", format!("schema_version query failed: {e}")))?
        .unwrap_or(0);
    let status = match applied.cmp(&expected) {
        std::cmp::Ordering::Equal => "ok",
        std::cmp::Ordering::Less => "pending",
        std::cmp::Ordering::Greater => "future",
    };
    Ok(json!({
        "expected": expected,
        "applied": applied,
        "status": status,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::central::CentralDb;

    fn as_usize(v: &Value) -> usize {
        usize::try_from(v.as_u64().unwrap()).unwrap()
    }

    #[test]
    fn version_returns_ok_after_migration() {
        // CentralDb::open_in_memory() already runs Central migrations.
        let db = CentralDb::open_in_memory().unwrap();
        let result = version(&json!({}), &db).unwrap();
        assert_eq!(result["status"], "ok");
        let expected = expected_central_schema_version();
        assert_eq!(as_usize(&result["expected"]), expected);
        assert_eq!(as_usize(&result["applied"]), expected);
    }

    #[test]
    fn version_returns_pending_when_applied_is_zero() {
        // Delete all rows so applied becomes 0.
        let db = CentralDb::open_in_memory().unwrap();
        {
            let conn = db.conn().unwrap();
            conn.execute("DELETE FROM schema_version", []).unwrap();
        }
        let result = version(&json!({}), &db).unwrap();
        assert_eq!(result["status"], "pending");
        assert_eq!(result["applied"].as_u64().unwrap(), 0);
        let expected = expected_central_schema_version();
        assert_eq!(as_usize(&result["expected"]), expected);
    }

    #[test]
    fn version_returns_future_when_applied_exceeds_expected() {
        let db = CentralDb::open_in_memory().unwrap();
        {
            // Simulate a migration written by a future binary.
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO schema_version (name, applied) \
                 VALUES ('999_future', '2099-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }
        let result = version(&json!({}), &db).unwrap();
        assert_eq!(result["status"], "future");
        let expected = expected_central_schema_version();
        assert_eq!(as_usize(&result["applied"]), expected + 1);
    }
}
