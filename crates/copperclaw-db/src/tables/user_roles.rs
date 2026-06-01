//! CRUD for `user_roles`.
//!
//! A role is a `(user, role-kind, scope)` triple. Scope is either a specific
//! `agent_group_id` or `None` for a global role.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, UserId};
use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Role::Owner),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRole {
    pub user_id: UserId,
    pub role: Role,
    pub agent_group_id: Option<AgentGroupId>,
    pub granted_by: Option<UserId>,
    pub granted_at: DateTime<Utc>,
}

fn parse_uuid_col(s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_user_role(row: &Row<'_>) -> rusqlite::Result<UserRole> {
    let user_id_str: String = row.get("user_id")?;
    let user_id = UserId(parse_uuid_col(&user_id_str)?);
    let role_str: String = row.get("role")?;
    let role = Role::parse(&role_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown role: {role_str}"),
            )),
        )
    })?;
    let agent_group_id: Option<String> = row.get("agent_group_id")?;
    let agent_group_id = match agent_group_id {
        Some(s) => Some(AgentGroupId(parse_uuid_col(&s)?)),
        None => None,
    };
    let granted_by: Option<String> = row.get("granted_by")?;
    let granted_by = match granted_by {
        Some(s) => Some(UserId(parse_uuid_col(&s)?)),
        None => None,
    };
    let granted_at_str: String = row.get("granted_at")?;
    let granted_at = DateTime::parse_from_rfc3339(&granted_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    Ok(UserRole {
        user_id,
        role,
        agent_group_id,
        granted_by,
        granted_at,
    })
}

pub fn list_for_user(db: &CentralDb, user: UserId) -> Result<Vec<UserRole>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT user_id, role, agent_group_id, granted_by, granted_at
         FROM user_roles
         WHERE user_id = ?1
         ORDER BY granted_at",
    )?;
    let rows = stmt.query_map(params![user.as_uuid().to_string()], row_to_user_role)?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

pub fn list_for_scope(
    db: &CentralDb,
    agent_group_id: Option<AgentGroupId>,
    role: Role,
) -> Result<Vec<UserRole>, DbError> {
    let conn = db.conn()?;
    let rows: rusqlite::Result<Vec<_>> = if let Some(ag) = agent_group_id {
        let mut stmt = conn.prepare(
            "SELECT user_id, role, agent_group_id, granted_by, granted_at
             FROM user_roles
             WHERE agent_group_id = ?1 AND role = ?2
             ORDER BY granted_at",
        )?;
        stmt.query_map(
            params![ag.as_uuid().to_string(), role.as_str()],
            row_to_user_role,
        )?
        .collect()
    } else {
        let mut stmt = conn.prepare(
            "SELECT user_id, role, agent_group_id, granted_by, granted_at
             FROM user_roles
             WHERE agent_group_id IS NULL AND role = ?1
             ORDER BY granted_at",
        )?;
        stmt.query_map(params![role.as_str()], row_to_user_role)?
            .collect()
    };
    Ok(rows?)
}

pub fn grant(
    db: &CentralDb,
    user: UserId,
    role: Role,
    agent_group_id: Option<AgentGroupId>,
    granted_by: Option<UserId>,
) -> Result<UserRole, DbError> {
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO user_roles (user_id, role, agent_group_id, granted_by, granted_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            user.as_uuid().to_string(),
            role.as_str(),
            agent_group_id.map(|a| a.as_uuid().to_string()),
            granted_by.map(|g| g.as_uuid().to_string()),
            now.to_rfc3339(),
        ],
    )?;
    Ok(UserRole {
        user_id: user,
        role,
        agent_group_id,
        granted_by,
        granted_at: now,
    })
}

pub fn revoke(
    db: &CentralDb,
    user: UserId,
    role: Role,
    agent_group_id: Option<AgentGroupId>,
) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = match agent_group_id {
        Some(ag) => conn.execute(
            "DELETE FROM user_roles
             WHERE user_id = ?1 AND role = ?2 AND agent_group_id = ?3",
            params![
                user.as_uuid().to_string(),
                role.as_str(),
                ag.as_uuid().to_string()
            ],
        )?,
        None => conn.execute(
            "DELETE FROM user_roles
             WHERE user_id = ?1 AND role = ?2 AND agent_group_id IS NULL",
            params![user.as_uuid().to_string(), role.as_str()],
        )?,
    };
    if n == 0 {
        return Err(DbError::NotFound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::agent_groups::{self, CreateAgentGroup};
    use crate::tables::users::{self, UpsertUser};

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    fn mk_user(db: &CentralDb, identity: &str) -> UserId {
        users::upsert(
            db,
            UpsertUser {
                kind: "telegram".into(),
                identity: identity.into(),
                display_name: None,
            },
        )
        .unwrap()
        .id
    }

    fn mk_group(db: &CentralDb, folder: &str) -> AgentGroupId {
        agent_groups::create(
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

    #[test]
    fn role_as_str_roundtrip() {
        assert_eq!(Role::Owner.as_str(), "owner");
        assert_eq!(Role::Admin.as_str(), "admin");
        assert_eq!(Role::parse("owner"), Some(Role::Owner));
        assert_eq!(Role::parse("admin"), Some(Role::Admin));
        assert_eq!(Role::parse("other"), None);
    }

    #[test]
    fn role_serde_is_lowercase() {
        let json = serde_json::to_string(&Role::Owner).unwrap();
        assert_eq!(json, "\"owner\"");
        let back: Role = serde_json::from_str("\"admin\"").unwrap();
        assert_eq!(back, Role::Admin);
    }

    #[test]
    fn grant_then_list_for_user() {
        let db = db();
        let u = mk_user(&db, "alice");
        let granted = grant(&db, u, Role::Owner, None, None).unwrap();
        let rows = list_for_user(&db, u).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], granted);
        assert_eq!(rows[0].agent_group_id, None);
    }

    #[test]
    fn grant_scoped_role_with_granter() {
        let db = db();
        let granter = mk_user(&db, "granter");
        let target = mk_user(&db, "target");
        let group = mk_group(&db, "g1");
        let row = grant(&db, target, Role::Admin, Some(group), Some(granter)).unwrap();
        assert_eq!(row.agent_group_id, Some(group));
        assert_eq!(row.granted_by, Some(granter));
        assert_eq!(row.role, Role::Admin);
    }

    #[test]
    fn list_for_scope_global() {
        let db = db();
        let u1 = mk_user(&db, "u1");
        let u2 = mk_user(&db, "u2");
        let group = mk_group(&db, "g1");
        grant(&db, u1, Role::Owner, None, None).unwrap();
        grant(&db, u2, Role::Owner, Some(group), None).unwrap();
        let global_owners = list_for_scope(&db, None, Role::Owner).unwrap();
        assert_eq!(global_owners.len(), 1);
        assert_eq!(global_owners[0].user_id, u1);
    }

    #[test]
    fn list_for_scope_specific_group() {
        let db = db();
        let u1 = mk_user(&db, "u1");
        let u2 = mk_user(&db, "u2");
        let g1 = mk_group(&db, "g1");
        let g2 = mk_group(&db, "g2");
        grant(&db, u1, Role::Admin, Some(g1), None).unwrap();
        grant(&db, u2, Role::Admin, Some(g2), None).unwrap();
        let admins_g1 = list_for_scope(&db, Some(g1), Role::Admin).unwrap();
        assert_eq!(admins_g1.len(), 1);
        assert_eq!(admins_g1[0].user_id, u1);
    }

    #[test]
    fn list_for_scope_filters_by_role() {
        let db = db();
        let u = mk_user(&db, "u");
        let g = mk_group(&db, "g");
        grant(&db, u, Role::Owner, Some(g), None).unwrap();
        grant(&db, u, Role::Admin, Some(g), None).unwrap();
        let owners = list_for_scope(&db, Some(g), Role::Owner).unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].role, Role::Owner);
    }

    #[test]
    fn list_for_user_returns_all_scopes() {
        let db = db();
        let u = mk_user(&db, "multi");
        let g = mk_group(&db, "g");
        grant(&db, u, Role::Owner, None, None).unwrap();
        grant(&db, u, Role::Admin, Some(g), None).unwrap();
        let rows = list_for_user(&db, u).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn revoke_global_role() {
        let db = db();
        let u = mk_user(&db, "u");
        grant(&db, u, Role::Owner, None, None).unwrap();
        revoke(&db, u, Role::Owner, None).unwrap();
        assert!(list_for_user(&db, u).unwrap().is_empty());
    }

    #[test]
    fn revoke_scoped_role() {
        let db = db();
        let u = mk_user(&db, "u");
        let g = mk_group(&db, "g");
        grant(&db, u, Role::Admin, Some(g), None).unwrap();
        revoke(&db, u, Role::Admin, Some(g)).unwrap();
        assert!(list_for_user(&db, u).unwrap().is_empty());
    }

    #[test]
    fn revoke_missing_is_not_found() {
        let db = db();
        let u = mk_user(&db, "u");
        let err = revoke(&db, u, Role::Owner, None).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn revoke_wrong_scope_is_not_found() {
        let db = db();
        let u = mk_user(&db, "u");
        let g = mk_group(&db, "g");
        grant(&db, u, Role::Owner, None, None).unwrap();
        let err = revoke(&db, u, Role::Owner, Some(g)).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn duplicate_scoped_grant_violates_pk() {
        // SQLite treats NULL values as distinct in PRIMARY KEY columns, so two
        // global grants (NULL agent_group_id) do NOT conflict — only scoped
        // grants share the same PK and collide.
        let db = db();
        let u = mk_user(&db, "u");
        let g = mk_group(&db, "g");
        grant(&db, u, Role::Owner, Some(g), None).unwrap();
        let err = grant(&db, u, Role::Owner, Some(g), None).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn grant_with_missing_user_violates_fk() {
        let db = db();
        let ghost = UserId::new();
        let err = grant(&db, ghost, Role::Owner, None, None).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
