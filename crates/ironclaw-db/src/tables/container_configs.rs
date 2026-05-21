//! CRUD for `container_configs`.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::{AgentGroupId, Effort};
use rusqlite::{params, OptionalExtension, Row};
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CliScope {
    Disabled,
    Group,
    Global,
}

impl CliScope {
    pub fn as_str(self) -> &'static str {
        match self {
            CliScope::Disabled => "disabled",
            CliScope::Group => "group",
            CliScope::Global => "global",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "disabled" => Some(CliScope::Disabled),
            "group" => Some(CliScope::Group),
            "global" => Some(CliScope::Global),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsSelector {
    /// All available skills are enabled. Serialized as the string `"all"`.
    All,
    /// Explicit allowlist of skill names. Serialized as a JSON array.
    Explicit(Vec<String>),
}

impl Serialize for SkillsSelector {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            SkillsSelector::All => ser.serialize_str("all"),
            SkillsSelector::Explicit(v) => v.serialize(ser),
        }
    }
}

impl<'de> Deserialize<'de> for SkillsSelector {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(de)?;
        match v {
            serde_json::Value::String(s) if s == "all" => Ok(SkillsSelector::All),
            serde_json::Value::String(other) => Err(de::Error::custom(format!(
                "expected \"all\", got \"{other}\""
            ))),
            serde_json::Value::Array(_) => {
                serde_json::from_value::<Vec<String>>(v)
                    .map(SkillsSelector::Explicit)
                    .map_err(de::Error::custom)
            }
            other => Err(de::Error::custom(format!(
                "expected \"all\" or a JSON array, got {other}"
            ))),
        }
    }
}

impl SkillsSelector {
    fn into_json_string(self) -> Result<String, DbError> {
        match self {
            SkillsSelector::All => Ok("\"all\"".to_string()),
            SkillsSelector::Explicit(v) => Ok(serde_json::to_string(&v)?),
        }
    }

    fn from_json_str(s: &str) -> Result<Self, DbError> {
        let v: serde_json::Value = serde_json::from_str(s)?;
        match v {
            serde_json::Value::String(s) if s == "all" => Ok(SkillsSelector::All),
            serde_json::Value::Array(_) => Ok(SkillsSelector::Explicit(serde_json::from_value(v)?)),
            other => Err(DbError::invariant(format!(
                "invalid skills selector JSON: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerConfig {
    pub agent_group_id: AgentGroupId,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub image_tag: Option<String>,
    pub assistant_name: Option<String>,
    pub max_messages_per_prompt: Option<u32>,
    pub skills: SkillsSelector,
    pub mcp_servers: serde_json::Value,
    pub packages_apt: Vec<String>,
    pub packages_npm: Vec<String>,
    pub additional_mounts: serde_json::Value,
    pub cli_scope: CliScope,
    /// Sha256 fingerprint of the rebuild-relevant fields, or `None` when
    /// not yet computed.  The container manager compares this against the
    /// current `image_tag`; a mismatch triggers an image rebuild before
    /// the next spawn.
    pub config_fingerprint: Option<String>,
    /// Opt-in egress allow-list.  Empty vec means allow-all; non-empty
    /// means only the listed `"host:port"` pairs may be reached.
    pub egress_allow: Vec<String>,
    /// Per-group resource caps forwarded to the container runtime.  The
    /// recognised JSON keys are `"cpus"`, `"memory_mb"`, `"pids_limit"`.
    pub resource_limits: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UpsertContainerConfig {
    pub agent_group_id: AgentGroupId,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub image_tag: Option<String>,
    pub assistant_name: Option<String>,
    pub max_messages_per_prompt: Option<u32>,
    pub skills: SkillsSelector,
    pub mcp_servers: serde_json::Value,
    pub packages_apt: Vec<String>,
    pub packages_npm: Vec<String>,
    pub additional_mounts: serde_json::Value,
    pub cli_scope: CliScope,
    pub config_fingerprint: Option<String>,
    pub egress_allow: Vec<String>,
    pub resource_limits: serde_json::Value,
}

fn effort_as_str(e: Effort) -> &'static str {
    match e {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
    }
}

fn parse_effort(s: &str) -> rusqlite::Result<Effort> {
    match s {
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown effort {other}").into(),
        )),
    }
}

fn row_to_container_config(row: &Row<'_>) -> rusqlite::Result<ContainerConfig> {
    let agent_group_id_str: String = row.get("agent_group_id")?;
    let agent_group_uuid = uuid::Uuid::parse_str(&agent_group_id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let effort_str: Option<String> = row.get("effort")?;
    let effort = effort_str.as_deref().map(parse_effort).transpose()?;
    let max_messages: Option<i64> = row.get("max_messages_per_prompt")?;
    let max_messages_per_prompt = max_messages
        .map(|v| u32::try_from(v).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, Box::new(e))
        }))
        .transpose()?;
    let skills_str: String = row.get("skills")?;
    let skills = SkillsSelector::from_json_str(&skills_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let mcp_servers_str: String = row.get("mcp_servers")?;
    let mcp_servers: serde_json::Value = serde_json::from_str(&mcp_servers_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let packages_apt_str: String = row.get("packages_apt")?;
    let packages_apt: Vec<String> = serde_json::from_str(&packages_apt_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let packages_npm_str: String = row.get("packages_npm")?;
    let packages_npm: Vec<String> = serde_json::from_str(&packages_npm_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let additional_mounts_str: String = row.get("additional_mounts")?;
    let additional_mounts: serde_json::Value = serde_json::from_str(&additional_mounts_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let cli_scope_str: String = row.get("cli_scope")?;
    let cli_scope = CliScope::parse(&cli_scope_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown cli_scope {cli_scope_str}").into(),
        )
    })?;
    let config_fingerprint: Option<String> = row.get("config_fingerprint")?;
    let egress_allow_str: String = row.get("egress_allow")?;
    let egress_allow: Vec<String> = serde_json::from_str(&egress_allow_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let resource_limits_str: String = row.get("resource_limits")?;
    let resource_limits: serde_json::Value = serde_json::from_str(&resource_limits_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let updated_at_str: String = row.get("updated_at")?;
    let updated_at = DateTime::parse_from_rfc3339(&updated_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    Ok(ContainerConfig {
        agent_group_id: AgentGroupId(agent_group_uuid),
        provider: row.get("provider")?,
        model: row.get("model")?,
        effort,
        image_tag: row.get("image_tag")?,
        assistant_name: row.get("assistant_name")?,
        max_messages_per_prompt,
        skills,
        mcp_servers,
        packages_apt,
        packages_npm,
        additional_mounts,
        cli_scope,
        config_fingerprint,
        egress_allow,
        resource_limits,
        updated_at,
    })
}

pub fn get(db: &CentralDb, agent_group_id: AgentGroupId) -> Result<Option<ContainerConfig>, DbError> {
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT agent_group_id, provider, model, effort, image_tag, assistant_name,
                    max_messages_per_prompt, skills, mcp_servers, packages_apt,
                    packages_npm, additional_mounts, cli_scope,
                    config_fingerprint, egress_allow, resource_limits, updated_at
             FROM container_configs
             WHERE agent_group_id = ?1",
            params![agent_group_id.as_uuid().to_string()],
            row_to_container_config,
        )
        .optional()?)
}

pub fn upsert(db: &CentralDb, req: UpsertContainerConfig) -> Result<ContainerConfig, DbError> {
    let now = Utc::now();
    let skills_json = req.skills.clone().into_json_string()?;
    let mcp_servers_json = serde_json::to_string(&req.mcp_servers)?;
    let packages_apt_json = serde_json::to_string(&req.packages_apt)?;
    let packages_npm_json = serde_json::to_string(&req.packages_npm)?;
    let additional_mounts_json = serde_json::to_string(&req.additional_mounts)?;
    let egress_allow_json = serde_json::to_string(&req.egress_allow)?;
    let resource_limits_json = serde_json::to_string(&req.resource_limits)?;
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO container_configs
           (agent_group_id, provider, model, effort, image_tag, assistant_name,
            max_messages_per_prompt, skills, mcp_servers, packages_apt,
            packages_npm, additional_mounts, cli_scope,
            config_fingerprint, egress_allow, resource_limits, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT(agent_group_id) DO UPDATE SET
             provider = excluded.provider,
             model = excluded.model,
             effort = excluded.effort,
             image_tag = excluded.image_tag,
             assistant_name = excluded.assistant_name,
             max_messages_per_prompt = excluded.max_messages_per_prompt,
             skills = excluded.skills,
             mcp_servers = excluded.mcp_servers,
             packages_apt = excluded.packages_apt,
             packages_npm = excluded.packages_npm,
             additional_mounts = excluded.additional_mounts,
             cli_scope = excluded.cli_scope,
             config_fingerprint = excluded.config_fingerprint,
             egress_allow = excluded.egress_allow,
             resource_limits = excluded.resource_limits,
             updated_at = excluded.updated_at",
        params![
            req.agent_group_id.as_uuid().to_string(),
            req.provider,
            req.model,
            req.effort.map(effort_as_str),
            req.image_tag,
            req.assistant_name,
            req.max_messages_per_prompt.map(i64::from),
            skills_json,
            mcp_servers_json,
            packages_apt_json,
            packages_npm_json,
            additional_mounts_json,
            req.cli_scope.as_str(),
            req.config_fingerprint,
            egress_allow_json,
            resource_limits_json,
            now.to_rfc3339(),
        ],
    )?;
    Ok(ContainerConfig {
        agent_group_id: req.agent_group_id,
        provider: req.provider,
        model: req.model,
        effort: req.effort,
        image_tag: req.image_tag,
        assistant_name: req.assistant_name,
        max_messages_per_prompt: req.max_messages_per_prompt,
        skills: req.skills,
        mcp_servers: req.mcp_servers,
        packages_apt: req.packages_apt,
        packages_npm: req.packages_npm,
        additional_mounts: req.additional_mounts,
        cli_scope: req.cli_scope,
        config_fingerprint: req.config_fingerprint,
        egress_allow: req.egress_allow,
        resource_limits: req.resource_limits,
        updated_at: now,
    })
}

pub fn get_skills(db: &CentralDb, agent_group_id: AgentGroupId) -> Result<SkillsSelector, DbError> {
    let conn = db.conn()?;
    let s: String = conn
        .query_row(
            "SELECT skills FROM container_configs WHERE agent_group_id = ?1",
            params![agent_group_id.as_uuid().to_string()],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(DbError::NotFound)?;
    SkillsSelector::from_json_str(&s)
}

pub fn set_skills(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    skills: SkillsSelector,
) -> Result<(), DbError> {
    let json = skills.into_json_string()?;
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE container_configs
         SET skills = ?1, updated_at = ?2
         WHERE agent_group_id = ?3",
        params![json, Utc::now().to_rfc3339(), agent_group_id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn get_mcp_servers(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
) -> Result<serde_json::Value, DbError> {
    let conn = db.conn()?;
    let s: String = conn
        .query_row(
            "SELECT mcp_servers FROM container_configs WHERE agent_group_id = ?1",
            params![agent_group_id.as_uuid().to_string()],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(DbError::NotFound)?;
    Ok(serde_json::from_str(&s)?)
}

pub fn set_mcp_servers(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    mcp: serde_json::Value,
) -> Result<(), DbError> {
    let json = serde_json::to_string(&mcp)?;
    drop(mcp);
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE container_configs
         SET mcp_servers = ?1, updated_at = ?2
         WHERE agent_group_id = ?3",
        params![json, Utc::now().to_rfc3339(), agent_group_id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

fn modify_string_array<F>(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    column: &str,
    mutator: F,
) -> Result<(), DbError>
where
    F: FnOnce(&mut Vec<String>),
{
    let conn = db.conn()?;
    let existing: Option<String> = conn
        .query_row(
            &format!("SELECT {column} FROM container_configs WHERE agent_group_id = ?1"),
            params![agent_group_id.as_uuid().to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let raw = existing.ok_or(DbError::NotFound)?;
    let mut current: Vec<String> = serde_json::from_str(&raw)?;
    mutator(&mut current);
    let updated = serde_json::to_string(&current)?;
    let n = conn.execute(
        &format!(
            "UPDATE container_configs
             SET {column} = ?1, updated_at = ?2
             WHERE agent_group_id = ?3"
        ),
        params![updated, Utc::now().to_rfc3339(), agent_group_id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

pub fn add_package_apt(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    pkg: String,
) -> Result<(), DbError> {
    modify_string_array(db, agent_group_id, "packages_apt", |v| {
        if !v.iter().any(|p| p == &pkg) {
            v.push(pkg);
        }
    })
}

pub fn remove_package_apt(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    pkg: &str,
) -> Result<(), DbError> {
    modify_string_array(db, agent_group_id, "packages_apt", |v| {
        v.retain(|p| p != pkg);
    })
}

pub fn add_package_npm(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    pkg: String,
) -> Result<(), DbError> {
    modify_string_array(db, agent_group_id, "packages_npm", |v| {
        if !v.iter().any(|p| p == &pkg) {
            v.push(pkg);
        }
    })
}

pub fn remove_package_npm(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    pkg: &str,
) -> Result<(), DbError> {
    modify_string_array(db, agent_group_id, "packages_npm", |v| {
        v.retain(|p| p != pkg);
    })
}

/// Compute a deterministic sha256 fingerprint over the rebuild-relevant
/// fields of a `ContainerConfig`.
///
/// Any change to `packages_apt`, `packages_npm`, `skills`, or
/// `mcp_servers` produces a different fingerprint. The container manager
/// compares this against `image_tag` to decide whether a rebuild is needed
/// before the next spawn.
pub fn compute_fingerprint(cfg: &ContainerConfig) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"apt=");
    for p in &cfg.packages_apt {
        hasher.update(p.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nnpm=");
    for p in &cfg.packages_npm {
        hasher.update(p.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\nskills=");
    let skills_s = match &cfg.skills {
        SkillsSelector::All => "all".to_string(),
        SkillsSelector::Explicit(v) => v.join(","),
    };
    hasher.update(skills_s.as_bytes());
    hasher.update(b"\nmcp=");
    // Sort the JSON to get a stable serialization independent of key insertion
    // order. We normalise via serde_json's Display which sorts object keys.
    hasher.update(cfg.mcp_servers.to_string().as_bytes());
    hex::encode(hasher.finalize())
}

/// Persist a new `config_fingerprint` and `image_tag` pair atomically.
///
/// Called by the container manager after a successful image rebuild to
/// record both the new tag and the fingerprint of the config that produced
/// it. Future spawns compare the live fingerprint against this stored value
/// to decide whether a rebuild is needed.
pub fn set_image_tag_and_fingerprint(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    image_tag: &str,
    fingerprint: &str,
) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE container_configs
         SET image_tag = ?1, config_fingerprint = ?2, updated_at = ?3
         WHERE agent_group_id = ?4",
        params![
            image_tag,
            fingerprint,
            Utc::now().to_rfc3339(),
            agent_group_id.as_uuid().to_string(),
        ],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Read the egress allow-list for an agent group.
pub fn get_egress_allow(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
) -> Result<Vec<String>, DbError> {
    let conn = db.conn()?;
    let s: Option<String> = conn
        .query_row(
            "SELECT egress_allow FROM container_configs WHERE agent_group_id = ?1",
            params![agent_group_id.as_uuid().to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let raw = s.ok_or(DbError::NotFound)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Overwrite the egress allow-list for an agent group.
pub fn set_egress_allow(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    allow: &[String],
) -> Result<(), DbError> {
    let json = serde_json::to_string(allow)?;
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE container_configs
         SET egress_allow = ?1, updated_at = ?2
         WHERE agent_group_id = ?3",
        params![json, Utc::now().to_rfc3339(), agent_group_id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

/// Read the resource limits JSON for an agent group.
pub fn get_resource_limits(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
) -> Result<serde_json::Value, DbError> {
    let conn = db.conn()?;
    let s: Option<String> = conn
        .query_row(
            "SELECT resource_limits FROM container_configs WHERE agent_group_id = ?1",
            params![agent_group_id.as_uuid().to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let raw = s.ok_or(DbError::NotFound)?;
    Ok(serde_json::from_str(&raw)?)
}

/// Overwrite the resource limits JSON for an agent group.
///
/// Accepted keys: `"cpus"` (string), `"memory_mb"` (integer),
/// `"pids_limit"` (integer).  Unknown keys are silently ignored by the
/// runtime translators; the validator in the iclaw handler rejects them.
pub fn set_resource_limits(
    db: &CentralDb,
    agent_group_id: AgentGroupId,
    limits: &serde_json::Value,
) -> Result<(), DbError> {
    let json = serde_json::to_string(limits)?;
    let conn = db.conn()?;
    let n = conn.execute(
        "UPDATE container_configs
         SET resource_limits = ?1, updated_at = ?2
         WHERE agent_group_id = ?3",
        params![json, Utc::now().to_rfc3339(), agent_group_id.as_uuid().to_string()],
    )?;
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{create as create_ag, CreateAgentGroup};
    use serde_json::json;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn make_agent_group(db: &CentralDb, folder: &str) -> AgentGroupId {
        create_ag(
            db,
            CreateAgentGroup {
                name: folder.into(),
                folder: folder.into(),
                agent_provider: None,
            },
        )
        .unwrap()
        .id
    }

    fn minimal_req(agent_group_id: AgentGroupId) -> UpsertContainerConfig {
        UpsertContainerConfig {
            agent_group_id,
            provider: None,
            model: None,
            effort: None,
            image_tag: None,
            assistant_name: None,
            max_messages_per_prompt: None,
            skills: SkillsSelector::All,
            mcp_servers: json!({}),
            packages_apt: vec![],
            packages_npm: vec![],
            additional_mounts: json!([]),
            cli_scope: CliScope::Group,
            config_fingerprint: None,
            egress_allow: vec![],
            resource_limits: json!({}),
        }
    }

    #[test]
    fn cli_scope_as_str_and_parse() {
        for s in [CliScope::Disabled, CliScope::Group, CliScope::Global] {
            assert_eq!(CliScope::parse(s.as_str()), Some(s));
        }
        assert_eq!(CliScope::parse("nope"), None);
    }

    #[test]
    fn cli_scope_serde_lowercase() {
        assert_eq!(serde_json::to_string(&CliScope::Disabled).unwrap(), "\"disabled\"");
        assert_eq!(serde_json::to_string(&CliScope::Group).unwrap(), "\"group\"");
        assert_eq!(serde_json::to_string(&CliScope::Global).unwrap(), "\"global\"");
    }

    #[test]
    fn skills_selector_serde_all() {
        let s: SkillsSelector = serde_json::from_str("\"all\"").unwrap();
        assert_eq!(s, SkillsSelector::All);
        assert_eq!(serde_json::to_string(&SkillsSelector::All).unwrap(), "\"all\"");
    }

    #[test]
    fn skills_selector_serde_explicit() {
        let s = SkillsSelector::Explicit(vec!["a".into(), "b".into()]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "[\"a\",\"b\"]");
        let back: SkillsSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn skills_selector_from_invalid_json_errors() {
        let err = SkillsSelector::from_json_str("123").unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }

    #[test]
    fn skills_selector_from_unknown_string_errors() {
        let err = SkillsSelector::from_json_str("\"none\"").unwrap_err();
        assert!(matches!(err, DbError::Invariant(_)));
    }

    #[test]
    fn get_missing_returns_none() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        assert!(get(&db, ag).unwrap().is_none());
    }

    #[test]
    fn upsert_then_get_roundtrips_all_fields() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let req = UpsertContainerConfig {
            agent_group_id: ag,
            provider: Some("claude".into()),
            model: Some("opus".into()),
            effort: Some(Effort::Medium),
            image_tag: Some("v1".into()),
            assistant_name: Some("clara".into()),
            max_messages_per_prompt: Some(40),
            skills: SkillsSelector::Explicit(vec!["x".into()]),
            mcp_servers: json!({"a": 1}),
            packages_apt: vec!["jq".into()],
            packages_npm: vec!["typescript".into()],
            additional_mounts: json!([{"src": "/x"}]),
            cli_scope: CliScope::Global,
            config_fingerprint: Some("abc123".into()),
            egress_allow: vec!["api.example.com:443".into()],
            resource_limits: json!({"cpus": "1.5", "memory_mb": 512}),
        };
        let saved = upsert(&db, req.clone()).unwrap();
        let fetched = get(&db, ag).unwrap().unwrap();
        assert_eq!(saved, fetched);
        assert_eq!(fetched.provider.as_deref(), Some("claude"));
        assert_eq!(fetched.model.as_deref(), Some("opus"));
        assert_eq!(fetched.effort, Some(Effort::Medium));
        assert_eq!(fetched.image_tag.as_deref(), Some("v1"));
        assert_eq!(fetched.assistant_name.as_deref(), Some("clara"));
        assert_eq!(fetched.max_messages_per_prompt, Some(40));
        assert_eq!(fetched.skills, SkillsSelector::Explicit(vec!["x".into()]));
        assert_eq!(fetched.mcp_servers, json!({"a": 1}));
        assert_eq!(fetched.packages_apt, vec!["jq".to_string()]);
        assert_eq!(fetched.packages_npm, vec!["typescript".to_string()]);
        assert_eq!(fetched.additional_mounts, json!([{"src": "/x"}]));
        assert_eq!(fetched.cli_scope, CliScope::Global);
        assert_eq!(fetched.config_fingerprint.as_deref(), Some("abc123"));
        assert_eq!(fetched.egress_allow, vec!["api.example.com:443".to_string()]);
        assert_eq!(fetched.resource_limits["cpus"], "1.5");
        assert_eq!(fetched.resource_limits["memory_mb"], 512);
    }

    #[test]
    fn upsert_replaces_existing_row() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let mut req = minimal_req(ag);
        req.provider = Some("a".into());
        upsert(&db, req).unwrap();
        let mut req = minimal_req(ag);
        req.provider = Some("b".into());
        let updated = upsert(&db, req).unwrap();
        assert_eq!(updated.provider.as_deref(), Some("b"));
    }

    #[test]
    fn upsert_fk_violation_for_unknown_group() {
        let db = db();
        let req = minimal_req(AgentGroupId::new());
        let err = upsert(&db, req).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn delete_agent_group_cascades() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        assert!(get(&db, ag).unwrap().is_some());
        let conn = db.conn().unwrap();
        conn.execute(
            "DELETE FROM agent_groups WHERE id = ?1",
            params![ag.as_uuid().to_string()],
        )
        .unwrap();
        drop(conn);
        assert!(get(&db, ag).unwrap().is_none());
    }

    #[test]
    fn get_skills_returns_selector() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        assert_eq!(get_skills(&db, ag).unwrap(), SkillsSelector::All);
    }

    #[test]
    fn get_skills_not_found_when_no_row() {
        let db = db();
        let err = get_skills(&db, AgentGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn set_skills_persists_explicit_list() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        let new = SkillsSelector::Explicit(vec!["x".into(), "y".into()]);
        set_skills(&db, ag, new.clone()).unwrap();
        assert_eq!(get_skills(&db, ag).unwrap(), new);
    }

    #[test]
    fn set_skills_not_found_when_no_row() {
        let db = db();
        let err = set_skills(&db, AgentGroupId::new(), SkillsSelector::All).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn get_mcp_servers_returns_value() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let mut req = minimal_req(ag);
        req.mcp_servers = json!({"name": "x"});
        upsert(&db, req).unwrap();
        assert_eq!(get_mcp_servers(&db, ag).unwrap(), json!({"name": "x"}));
    }

    #[test]
    fn get_mcp_servers_not_found_when_no_row() {
        let db = db();
        let err = get_mcp_servers(&db, AgentGroupId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn set_mcp_servers_overwrites_value() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        set_mcp_servers(&db, ag, json!({"k": 9})).unwrap();
        assert_eq!(get_mcp_servers(&db, ag).unwrap(), json!({"k": 9}));
    }

    #[test]
    fn set_mcp_servers_not_found_when_no_row() {
        let db = db();
        let err = set_mcp_servers(&db, AgentGroupId::new(), json!({})).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn add_package_apt_appends_once() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        add_package_apt(&db, ag, "curl".into()).unwrap();
        add_package_apt(&db, ag, "curl".into()).unwrap();
        let cfg = get(&db, ag).unwrap().unwrap();
        assert_eq!(cfg.packages_apt, vec!["curl".to_string()]);
    }

    #[test]
    fn add_package_apt_not_found_when_no_row() {
        let db = db();
        let err = add_package_apt(&db, AgentGroupId::new(), "x".into()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn remove_package_apt_removes_entry() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        add_package_apt(&db, ag, "curl".into()).unwrap();
        add_package_apt(&db, ag, "jq".into()).unwrap();
        remove_package_apt(&db, ag, "curl").unwrap();
        let cfg = get(&db, ag).unwrap().unwrap();
        assert_eq!(cfg.packages_apt, vec!["jq".to_string()]);
    }

    #[test]
    fn remove_package_apt_missing_pkg_is_noop() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        remove_package_apt(&db, ag, "absent").unwrap();
        assert!(get(&db, ag).unwrap().unwrap().packages_apt.is_empty());
    }

    #[test]
    fn remove_package_apt_not_found_when_no_row() {
        let db = db();
        let err = remove_package_apt(&db, AgentGroupId::new(), "x").unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn add_package_npm_appends_once() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        add_package_npm(&db, ag, "typescript".into()).unwrap();
        add_package_npm(&db, ag, "typescript".into()).unwrap();
        let cfg = get(&db, ag).unwrap().unwrap();
        assert_eq!(cfg.packages_npm, vec!["typescript".to_string()]);
    }

    #[test]
    fn add_package_npm_not_found_when_no_row() {
        let db = db();
        let err = add_package_npm(&db, AgentGroupId::new(), "x".into()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn remove_package_npm_removes_entry() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        add_package_npm(&db, ag, "a".into()).unwrap();
        add_package_npm(&db, ag, "b".into()).unwrap();
        remove_package_npm(&db, ag, "a").unwrap();
        let cfg = get(&db, ag).unwrap().unwrap();
        assert_eq!(cfg.packages_npm, vec!["b".to_string()]);
    }

    #[test]
    fn remove_package_npm_missing_pkg_is_noop() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        remove_package_npm(&db, ag, "absent").unwrap();
        assert!(get(&db, ag).unwrap().unwrap().packages_npm.is_empty());
    }

    #[test]
    fn remove_package_npm_not_found_when_no_row() {
        let db = db();
        let err = remove_package_npm(&db, AgentGroupId::new(), "x").unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn unknown_cli_scope_in_db_errors_on_read() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        let conn = db.conn().unwrap();
        conn.execute(
            "UPDATE container_configs SET cli_scope = 'bogus' WHERE agent_group_id = ?1",
            params![ag.as_uuid().to_string()],
        )
        .unwrap();
        drop(conn);
        let err = get(&db, ag).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn unknown_effort_in_db_errors_on_read() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        let conn = db.conn().unwrap();
        conn.execute(
            "UPDATE container_configs SET effort = 'bogus' WHERE agent_group_id = ?1",
            params![ag.as_uuid().to_string()],
        )
        .unwrap();
        drop(conn);
        let err = get(&db, ag).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn corrupt_skills_json_in_db_errors_on_read() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        let conn = db.conn().unwrap();
        conn.execute(
            "UPDATE container_configs SET skills = 'not-json' WHERE agent_group_id = ?1",
            params![ag.as_uuid().to_string()],
        )
        .unwrap();
        drop(conn);
        let err = get(&db, ag).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn default_values_apply_when_only_required_columns_set() {
        // Insert directly with only required columns to exercise the SQL defaults.
        let db = db();
        let ag = make_agent_group(&db, "g");
        let conn = db.conn().unwrap();
        conn.execute(
            "INSERT INTO container_configs (agent_group_id, updated_at)
             VALUES (?1, ?2)",
            params![ag.as_uuid().to_string(), Utc::now().to_rfc3339()],
        )
        .unwrap();
        drop(conn);
        let cfg = get(&db, ag).unwrap().unwrap();
        assert_eq!(cfg.skills, SkillsSelector::All);
        assert_eq!(cfg.mcp_servers, json!({}));
        assert!(cfg.packages_apt.is_empty());
        assert!(cfg.packages_npm.is_empty());
        assert_eq!(cfg.additional_mounts, json!([]));
        assert_eq!(cfg.cli_scope, CliScope::Group);
        assert!(cfg.config_fingerprint.is_none());
        assert!(cfg.egress_allow.is_empty());
        assert_eq!(cfg.resource_limits, json!({}));
    }

    #[test]
    fn compute_fingerprint_is_stable() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let req = minimal_req(ag);
        let cfg = upsert(&db, req).unwrap();
        let fp1 = compute_fingerprint(&cfg);
        let fp2 = compute_fingerprint(&cfg);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn compute_fingerprint_changes_on_apt_package_add() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let req = minimal_req(ag);
        let cfg = upsert(&db, req).unwrap();
        let fp_before = compute_fingerprint(&cfg);
        add_package_apt(&db, ag, "curl".into()).unwrap();
        let cfg2 = get(&db, ag).unwrap().unwrap();
        let fp_after = compute_fingerprint(&cfg2);
        assert_ne!(fp_before, fp_after);
    }

    #[test]
    fn compute_fingerprint_changes_on_npm_package_add() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let req = minimal_req(ag);
        let cfg = upsert(&db, req).unwrap();
        let fp_before = compute_fingerprint(&cfg);
        add_package_npm(&db, ag, "typescript".into()).unwrap();
        let cfg2 = get(&db, ag).unwrap().unwrap();
        let fp_after = compute_fingerprint(&cfg2);
        assert_ne!(fp_before, fp_after);
    }

    #[test]
    fn compute_fingerprint_changes_on_mcp_server_change() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        let req = minimal_req(ag);
        let cfg = upsert(&db, req).unwrap();
        let fp_before = compute_fingerprint(&cfg);
        set_mcp_servers(&db, ag, json!({"my-server": {"name": "my-server"}})).unwrap();
        let cfg2 = get(&db, ag).unwrap().unwrap();
        let fp_after = compute_fingerprint(&cfg2);
        assert_ne!(fp_before, fp_after);
    }

    #[test]
    fn set_image_tag_and_fingerprint_persists_both() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        set_image_tag_and_fingerprint(&db, ag, "ironclaw/session:sha256-abc", "abc").unwrap();
        let cfg = get(&db, ag).unwrap().unwrap();
        assert_eq!(cfg.image_tag.as_deref(), Some("ironclaw/session:sha256-abc"));
        assert_eq!(cfg.config_fingerprint.as_deref(), Some("abc"));
    }

    #[test]
    fn set_image_tag_and_fingerprint_not_found() {
        let db = db();
        let err = set_image_tag_and_fingerprint(&db, AgentGroupId::new(), "tag", "fp").unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn get_and_set_egress_allow_roundtrips() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        let initial = get_egress_allow(&db, ag).unwrap();
        assert!(initial.is_empty());
        let hosts = vec!["api.example.com:443".to_string(), "db.local:5432".to_string()];
        set_egress_allow(&db, ag, &hosts).unwrap();
        let fetched = get_egress_allow(&db, ag).unwrap();
        assert_eq!(fetched, hosts);
    }

    #[test]
    fn set_egress_allow_empty_list_is_allow_all() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        set_egress_allow(&db, ag, &["x:80".to_string()]).unwrap();
        set_egress_allow(&db, ag, &[]).unwrap();
        let fetched = get_egress_allow(&db, ag).unwrap();
        assert!(fetched.is_empty());
    }

    #[test]
    fn set_egress_allow_not_found() {
        let db = db();
        let err = set_egress_allow(&db, AgentGroupId::new(), &[]).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn get_and_set_resource_limits_roundtrips() {
        let db = db();
        let ag = make_agent_group(&db, "g");
        upsert(&db, minimal_req(ag)).unwrap();
        let initial = get_resource_limits(&db, ag).unwrap();
        assert_eq!(initial, json!({}));
        let limits = json!({"cpus": "1.5", "memory_mb": 512, "pids_limit": 256});
        set_resource_limits(&db, ag, &limits).unwrap();
        let fetched = get_resource_limits(&db, ag).unwrap();
        assert_eq!(fetched["cpus"], "1.5");
        assert_eq!(fetched["memory_mb"], 512);
        assert_eq!(fetched["pids_limit"], 256);
    }

    #[test]
    fn set_resource_limits_not_found() {
        let db = db();
        let err = set_resource_limits(&db, AgentGroupId::new(), &json!({})).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }
}
