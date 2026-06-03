//! CRUD for `provider_profiles` + `provider_health` (migration
//! `020_provider_profiles`).
//!
//! These two tables back M16 Phase 4 (provider resilience):
//!
//! * [`set_chain`] / [`get_chain`] persist the per-group fallback chain +
//!   per-channel pin map as opaque JSON (the typed shape lives in
//!   `copperclaw-providers::failover::FallbackChain`; this layer stays a thin
//!   store so `copperclaw-db` keeps its single `copperclaw-types` edge and
//!   doesn't pull in the providers crate).
//! * [`upsert_health`] / [`list_health`] / [`get_health`] record the runtime
//!   health of each `(provider, model, key_id)` triple. The host hydrates the
//!   health map before a spawn, picks the active entry, and writes back the
//!   degrade / recover deltas.
//!
//! A group with no `provider_profiles` row has [`get_chain`] return `None`:
//! the host keeps its historical single-provider selection untouched.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::AgentGroupId;
use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;

/// The persisted chain blob for one agent group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    pub agent_group_id: AgentGroupId,
    /// JSON array of chain entries (see `FallbackChain::entries`). `Null` /
    /// empty array means "no chain configured".
    pub chain: Value,
    /// JSON object mapping `channel_type -> model id`. `Null` / empty object
    /// means "no pins".
    pub model_by_channel: Value,
    /// Re-probe cool-down override in seconds; `None` -> the host default.
    pub reprobe_after_secs: Option<u64>,
    pub updated_at: DateTime<Utc>,
}

/// One runtime-health row for a `(provider, model, key_id)` triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthRow {
    pub agent_group_id: AgentGroupId,
    pub provider: String,
    pub model: String,
    pub key_id: String,
    pub status: String,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub last_reason: Option<String>,
    pub failure_count: u32,
    pub updated_at: DateTime<Utc>,
}

/// Upsert request for one health row. Mirrors [`HealthRow`] minus the audit
/// timestamps the store stamps itself.
#[derive(Debug, Clone)]
pub struct UpsertHealth {
    pub agent_group_id: AgentGroupId,
    pub provider: String,
    pub model: String,
    pub key_id: String,
    pub status: String,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
    pub last_reason: Option<String>,
    pub failure_count: u32,
}

fn parse_dt(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn opt_parse_dt(s: Option<&str>) -> rusqlite::Result<Option<DateTime<Utc>>> {
    s.map(parse_dt).transpose()
}

fn parse_json(s: Option<String>) -> rusqlite::Result<Value> {
    match s {
        None => Ok(Value::Null),
        Some(text) => serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        }),
    }
}

fn row_to_profile(row: &Row<'_>) -> rusqlite::Result<ProviderProfile> {
    let id_str: String = row.get("agent_group_id")?;
    let agent_group_id = uuid::Uuid::parse_str(&id_str)
        .map(AgentGroupId)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let chain: Option<String> = row.get("chain")?;
    let model_by_channel: Option<String> = row.get("model_by_channel")?;
    let reprobe: Option<i64> = row.get("reprobe_after_secs")?;
    let updated_at_str: String = row.get("updated_at")?;
    Ok(ProviderProfile {
        agent_group_id,
        chain: parse_json(chain)?,
        model_by_channel: parse_json(model_by_channel)?,
        reprobe_after_secs: reprobe.and_then(|n| u64::try_from(n).ok()),
        updated_at: parse_dt(&updated_at_str)?,
    })
}

fn row_to_health(row: &Row<'_>) -> rusqlite::Result<HealthRow> {
    let id_str: String = row.get("agent_group_id")?;
    let agent_group_id = uuid::Uuid::parse_str(&id_str)
        .map(AgentGroupId)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let cooldown_until: Option<String> = row.get("cooldown_until")?;
    let last_failure_at: Option<String> = row.get("last_failure_at")?;
    let failure_count: i64 = row.get("failure_count")?;
    let updated_at_str: String = row.get("updated_at")?;
    Ok(HealthRow {
        agent_group_id,
        provider: row.get("provider")?,
        model: row.get("model")?,
        key_id: row.get("key_id")?,
        status: row.get("status")?,
        cooldown_until: opt_parse_dt(cooldown_until.as_deref())?,
        last_failure_at: opt_parse_dt(last_failure_at.as_deref())?,
        last_reason: row.get("last_reason")?,
        failure_count: u32::try_from(failure_count.max(0)).unwrap_or(0),
        updated_at: parse_dt(&updated_at_str)?,
    })
}

/// Fetch the provider profile (chain + pins) for a group, or `None` when no
/// row exists (single-provider behaviour).
pub fn get_chain(db: &CentralDb, id: AgentGroupId) -> Result<Option<ProviderProfile>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT agent_group_id, chain, model_by_channel, reprobe_after_secs, updated_at
             FROM provider_profiles WHERE agent_group_id = ?1",
            params![id.as_uuid().to_string()],
            row_to_profile,
        )
        .optional()?)
}

/// Upsert the chain + pin map + re-probe override for a group. Passing a JSON
/// `Null` for `chain` / `model_by_channel` stores SQL NULL (clears it).
pub fn set_chain(
    db: &CentralDb,
    id: AgentGroupId,
    chain: &Value,
    model_by_channel: &Value,
    reprobe_after_secs: Option<u64>,
    now: DateTime<Utc>,
) -> Result<ProviderProfile, DbError> {
    let chain_text = if chain.is_null() {
        None
    } else {
        Some(serde_json::to_string(chain)?)
    };
    let mbc_text = if model_by_channel.is_null() {
        None
    } else {
        Some(serde_json::to_string(model_by_channel)?)
    };
    let reprobe_i64 = reprobe_after_secs.and_then(|n| i64::try_from(n).ok());
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO provider_profiles
           (agent_group_id, chain, model_by_channel, reprobe_after_secs, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_group_id) DO UPDATE SET
           chain = excluded.chain,
           model_by_channel = excluded.model_by_channel,
           reprobe_after_secs = excluded.reprobe_after_secs,
           updated_at = excluded.updated_at",
        params![
            id.as_uuid().to_string(),
            chain_text,
            mbc_text,
            reprobe_i64,
            now.to_rfc3339(),
        ],
    )?;
    drop(conn);
    get_chain(db, id)?.ok_or(DbError::NotFound)
}

/// Delete the provider profile for a group (revert to single-provider). Also
/// clears the group's health rows so a future chain starts clean. Idempotent.
pub fn clear_chain(db: &CentralDb, id: AgentGroupId) -> Result<(), DbError> {
    let conn = db.conn()?;
    conn.execute(
        "DELETE FROM provider_profiles WHERE agent_group_id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    conn.execute(
        "DELETE FROM provider_health WHERE agent_group_id = ?1",
        params![id.as_uuid().to_string()],
    )?;
    Ok(())
}

/// List every health row for a group. Used to hydrate the in-memory health
/// map before selection and by `cclaw groups provider status`.
pub fn list_health(db: &CentralDb, id: AgentGroupId) -> Result<Vec<HealthRow>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT agent_group_id, provider, model, key_id, status, cooldown_until,
                last_failure_at, last_reason, failure_count, updated_at
         FROM provider_health WHERE agent_group_id = ?1
         ORDER BY provider, model, key_id",
    )?;
    let rows = stmt.query_map(params![id.as_uuid().to_string()], row_to_health)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Fetch one health row, or `None`.
pub fn get_health(
    db: &CentralDb,
    id: AgentGroupId,
    provider: &str,
    model: &str,
    key_id: &str,
) -> Result<Option<HealthRow>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT agent_group_id, provider, model, key_id, status, cooldown_until,
                    last_failure_at, last_reason, failure_count, updated_at
             FROM provider_health
             WHERE agent_group_id = ?1 AND provider = ?2 AND model = ?3 AND key_id = ?4",
            params![id.as_uuid().to_string(), provider, model, key_id],
            row_to_health,
        )
        .optional()?)
}

/// Upsert one health row.
pub fn upsert_health(
    db: &CentralDb,
    req: &UpsertHealth,
    now: DateTime<Utc>,
) -> Result<HealthRow, DbError> {
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO provider_health
           (agent_group_id, provider, model, key_id, status, cooldown_until,
            last_failure_at, last_reason, failure_count, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(agent_group_id, provider, model, key_id) DO UPDATE SET
           status = excluded.status,
           cooldown_until = excluded.cooldown_until,
           last_failure_at = excluded.last_failure_at,
           last_reason = excluded.last_reason,
           failure_count = excluded.failure_count,
           updated_at = excluded.updated_at",
        params![
            req.agent_group_id.as_uuid().to_string(),
            req.provider,
            req.model,
            req.key_id,
            req.status,
            req.cooldown_until.map(|d| d.to_rfc3339()),
            req.last_failure_at.map(|d| d.to_rfc3339()),
            req.last_reason,
            i64::from(req.failure_count),
            now.to_rfc3339(),
        ],
    )?;
    drop(conn);
    get_health(
        db,
        req.agent_group_id,
        &req.provider,
        &req.model,
        &req.key_id,
    )?
    .ok_or(DbError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{self, CreateAgentGroup};
    use serde_json::json;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn group(db: &CentralDb) -> AgentGroupId {
        agent_groups::create(
            db,
            CreateAgentGroup {
                name: "g".into(),
                folder: "g".into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn no_row_is_none() {
        let db = db();
        let g = group(&db);
        assert!(get_chain(&db, g).unwrap().is_none());
    }

    #[test]
    fn set_and_get_chain_roundtrip() {
        let db = db();
        let g = group(&db);
        let chain = json!([
            {"provider": "anthropic", "model": "claude-sonnet-4-6",
             "keys": [{"id": "primary", "api_key_env": "ANTHROPIC_API_KEY"}]},
            {"provider": "ollama", "model": "qwen3.6:27b"}
        ]);
        let mbc = json!({"telegram": "claude-opus-4-1"});
        let now = Utc::now();
        let stored = set_chain(&db, g, &chain, &mbc, Some(90), now).unwrap();
        assert_eq!(stored.chain, chain);
        assert_eq!(stored.model_by_channel, mbc);
        assert_eq!(stored.reprobe_after_secs, Some(90));

        let got = get_chain(&db, g).unwrap().unwrap();
        assert_eq!(got.chain, chain);
        assert_eq!(got.model_by_channel, mbc);
    }

    #[test]
    fn set_chain_upserts() {
        let db = db();
        let g = group(&db);
        let now = Utc::now();
        set_chain(
            &db,
            g,
            &json!([{"provider": "a", "model": "m"}]),
            &Value::Null,
            None,
            now,
        )
        .unwrap();
        let updated = set_chain(
            &db,
            g,
            &json!([{"provider": "b", "model": "n"}]),
            &Value::Null,
            None,
            now,
        )
        .unwrap();
        assert_eq!(updated.chain[0]["provider"], "b");
        // Still exactly one row.
        let conn = db.conn().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM provider_profiles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn null_chain_stored_as_sql_null() {
        let db = db();
        let g = group(&db);
        let stored = set_chain(&db, g, &Value::Null, &Value::Null, None, Utc::now()).unwrap();
        assert!(stored.chain.is_null());
        assert!(stored.model_by_channel.is_null());
    }

    #[test]
    fn health_upsert_and_list() {
        let db = db();
        let g = group(&db);
        let now = Utc::now();
        let cooldown = now + chrono::Duration::minutes(2);
        let row = upsert_health(
            &db,
            &UpsertHealth {
                agent_group_id: g,
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
                key_id: "primary".into(),
                status: "rate_limited".into(),
                cooldown_until: Some(cooldown),
                last_failure_at: Some(now),
                last_reason: Some("rate_limit".into()),
                failure_count: 1,
            },
            now,
        )
        .unwrap();
        assert_eq!(row.status, "rate_limited");
        assert_eq!(row.failure_count, 1);
        assert_eq!(row.last_reason.as_deref(), Some("rate_limit"));

        let listed = list_health(&db, g).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key_id, "primary");
    }

    #[test]
    fn health_upsert_overwrites_same_triple() {
        let db = db();
        let g = group(&db);
        let now = Utc::now();
        let mk = |status: &str, count: u32| UpsertHealth {
            agent_group_id: g,
            provider: "anthropic".into(),
            model: "m".into(),
            key_id: "k".into(),
            status: status.into(),
            cooldown_until: None,
            last_failure_at: Some(now),
            last_reason: None,
            failure_count: count,
        };
        upsert_health(&db, &mk("down", 1), now).unwrap();
        let row = upsert_health(&db, &mk("healthy", 0), now).unwrap();
        assert_eq!(row.status, "healthy");
        assert_eq!(list_health(&db, g).unwrap().len(), 1);
    }

    #[test]
    fn clear_chain_removes_profile_and_health() {
        let db = db();
        let g = group(&db);
        let now = Utc::now();
        set_chain(
            &db,
            g,
            &json!([{"provider": "a", "model": "m"}]),
            &Value::Null,
            None,
            now,
        )
        .unwrap();
        upsert_health(
            &db,
            &UpsertHealth {
                agent_group_id: g,
                provider: "a".into(),
                model: "m".into(),
                key_id: String::new(),
                status: "down".into(),
                cooldown_until: None,
                last_failure_at: Some(now),
                last_reason: Some("server_error".into()),
                failure_count: 3,
            },
            now,
        )
        .unwrap();
        clear_chain(&db, g).unwrap();
        assert!(get_chain(&db, g).unwrap().is_none());
        assert!(list_health(&db, g).unwrap().is_empty());
    }

    #[test]
    fn ambient_key_id_empty_string_roundtrips() {
        let db = db();
        let g = group(&db);
        let now = Utc::now();
        upsert_health(
            &db,
            &UpsertHealth {
                agent_group_id: g,
                provider: "ollama".into(),
                model: "qwen3.6:27b".into(),
                key_id: String::new(),
                status: "healthy".into(),
                cooldown_until: None,
                last_failure_at: None,
                last_reason: None,
                failure_count: 0,
            },
            now,
        )
        .unwrap();
        let got = get_health(&db, g, "ollama", "qwen3.6:27b", "")
            .unwrap()
            .unwrap();
        assert_eq!(got.key_id, "");
    }

    #[test]
    fn deleting_group_cascades_profile_rows() {
        let db = db();
        let g = group(&db);
        let now = Utc::now();
        set_chain(
            &db,
            g,
            &json!([{"provider": "a", "model": "m"}]),
            &Value::Null,
            None,
            now,
        )
        .unwrap();
        upsert_health(
            &db,
            &UpsertHealth {
                agent_group_id: g,
                provider: "a".into(),
                model: "m".into(),
                key_id: String::new(),
                status: "healthy".into(),
                cooldown_until: None,
                last_failure_at: None,
                last_reason: None,
                failure_count: 0,
            },
            now,
        )
        .unwrap();
        agent_groups::delete(&db, g).unwrap();
        assert!(get_chain(&db, g).unwrap().is_none());
        assert!(list_health(&db, g).unwrap().is_empty());
    }
}
