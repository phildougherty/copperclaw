//! CRUD for `agent_group_members`.
//!
//! Membership grants a user access to an agent group's sessions and conversations.

use crate::DbError;
use crate::central::CentralDb;
use chrono::{DateTime, Utc};
use copperclaw_types::{AgentGroupId, UserId};
use rusqlite::{Row, params};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: UserId,
    pub agent_group_id: AgentGroupId,
    pub added_by: Option<UserId>,
    pub added_at: DateTime<Utc>,
}

fn parse_uuid_col(s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_member(row: &Row<'_>) -> rusqlite::Result<Member> {
    let user_id_str: String = row.get("user_id")?;
    let user_id = UserId(parse_uuid_col(&user_id_str)?);
    let agent_group_id_str: String = row.get("agent_group_id")?;
    let agent_group_id = AgentGroupId(parse_uuid_col(&agent_group_id_str)?);
    let added_by: Option<String> = row.get("added_by")?;
    let added_by = match added_by {
        Some(s) => Some(UserId(parse_uuid_col(&s)?)),
        None => None,
    };
    let added_at_str: String = row.get("added_at")?;
    let added_at = DateTime::parse_from_rfc3339(&added_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    Ok(Member {
        user_id,
        agent_group_id,
        added_by,
        added_at,
    })
}

pub fn list(db: &CentralDb, agent_group: AgentGroupId) -> Result<Vec<Member>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT user_id, agent_group_id, added_by, added_at
         FROM agent_group_members
         WHERE agent_group_id = ?1
         ORDER BY added_at",
    )?;
    let rows = stmt.query_map(params![agent_group.as_uuid().to_string()], row_to_member)?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

pub fn add(
    db: &CentralDb,
    user: UserId,
    agent_group: AgentGroupId,
    added_by: Option<UserId>,
) -> Result<Member, DbError> {
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO agent_group_members (user_id, agent_group_id, added_by, added_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            user.as_uuid().to_string(),
            agent_group.as_uuid().to_string(),
            added_by.map(|a| a.as_uuid().to_string()),
            now.to_rfc3339(),
        ],
    )?;
    Ok(Member {
        user_id: user,
        agent_group_id: agent_group,
        added_by,
        added_at: now,
    })
}

pub fn remove(db: &CentralDb, user: UserId, agent_group: AgentGroupId) -> Result<(), DbError> {
    let conn = db.conn()?;
    let n = conn.execute(
        "DELETE FROM agent_group_members
         WHERE user_id = ?1 AND agent_group_id = ?2",
        params![
            user.as_uuid().to_string(),
            agent_group.as_uuid().to_string()
        ],
    )?;
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
    fn add_then_list() {
        let db = db();
        let u = mk_user(&db, "alice");
        let g = mk_group(&db, "team");
        let m = add(&db, u, g, None).unwrap();
        let rows = list(&db, g).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], m);
    }

    #[test]
    fn add_with_adder_tracks_attribution() {
        let db = db();
        let adder = mk_user(&db, "adder");
        let target = mk_user(&db, "target");
        let g = mk_group(&db, "team");
        let m = add(&db, target, g, Some(adder)).unwrap();
        assert_eq!(m.added_by, Some(adder));
    }

    #[test]
    fn list_is_scoped_by_group() {
        let db = db();
        let u = mk_user(&db, "u");
        let g1 = mk_group(&db, "g1");
        let g2 = mk_group(&db, "g2");
        add(&db, u, g1, None).unwrap();
        let rows = list(&db, g2).unwrap();
        assert!(rows.is_empty());
        let rows = list(&db, g1).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn list_orders_by_added_at() {
        let db = db();
        let g = mk_group(&db, "g");
        for i in 0..3 {
            let u = mk_user(&db, &format!("user-{i}"));
            add(&db, u, g, None).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows = list(&db, g).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].added_at <= rows[1].added_at);
        assert!(rows[1].added_at <= rows[2].added_at);
    }

    #[test]
    fn list_empty_when_no_members() {
        let db = db();
        let g = mk_group(&db, "empty");
        assert!(list(&db, g).unwrap().is_empty());
    }

    #[test]
    fn remove_works() {
        let db = db();
        let u = mk_user(&db, "u");
        let g = mk_group(&db, "g");
        add(&db, u, g, None).unwrap();
        remove(&db, u, g).unwrap();
        assert!(list(&db, g).unwrap().is_empty());
    }

    #[test]
    fn remove_missing_is_not_found() {
        let db = db();
        let u = mk_user(&db, "u");
        let g = mk_group(&db, "g");
        let err = remove(&db, u, g).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn duplicate_add_violates_pk() {
        let db = db();
        let u = mk_user(&db, "u");
        let g = mk_group(&db, "g");
        add(&db, u, g, None).unwrap();
        let err = add(&db, u, g, None).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn add_with_missing_user_violates_fk() {
        let db = db();
        let g = mk_group(&db, "g");
        let ghost = UserId::new();
        let err = add(&db, ghost, g, None).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn add_with_missing_group_violates_fk() {
        let db = db();
        let u = mk_user(&db, "u");
        let ghost_g = AgentGroupId::new();
        let err = add(&db, u, ghost_g, None).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
