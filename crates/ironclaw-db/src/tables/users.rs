//! CRUD for `users`.
//!
//! User identity is derived deterministically from `(kind, identity)` using
//! `UUIDv5` with the nil namespace. This means callers can recover the same
//! `UserId` from a `(kind, identity)` pair without first consulting the
//! database, which makes `get_by_identity` a direct primary-key lookup and
//! makes `upsert` safe to retry.

use crate::central::CentralDb;
use crate::DbError;
use chrono::{DateTime, Utc};
use ironclaw_types::UserId;
use rusqlite::{params, OptionalExtension, Row};
use uuid::Uuid;

/// Namespace seed for deriving deterministic `UserId`s from `(kind, identity)`.
const USER_ID_NAMESPACE: Uuid = Uuid::nil();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub kind: String,
    pub display_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct UpsertUser {
    pub kind: String,
    pub identity: String,
    pub display_name: Option<String>,
}

/// Derive the canonical `UserId` for a `(kind, identity)` pair.
fn derive_user_id(kind: &str, identity: &str) -> UserId {
    let key = format!("{kind}:{identity}");
    UserId(Uuid::new_v5(&USER_ID_NAMESPACE, key.as_bytes()))
}

fn row_to_user(row: &Row<'_>) -> rusqlite::Result<User> {
    let id_str: String = row.get("id")?;
    let id = Uuid::parse_str(&id_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let created_at_str: String = row.get("created_at")?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?
        .with_timezone(&Utc);
    Ok(User {
        id: UserId(id),
        kind: row.get("kind")?,
        display_name: row.get("display_name")?,
        created_at,
    })
}

pub fn list(db: &CentralDb) -> Result<Vec<User>, DbError> {
    let conn = db.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id, kind, display_name, created_at
         FROM users
         ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], row_to_user)?;
    let out: rusqlite::Result<Vec<_>> = rows.collect();
    Ok(out?)
}

pub fn get(db: &CentralDb, id: UserId) -> Result<User, DbError> {
    let conn = db.conn()?;
    conn.query_row(
        "SELECT id, kind, display_name, created_at FROM users WHERE id = ?1",
        params![id.as_uuid().to_string()],
        row_to_user,
    )
    .optional()?
    .ok_or(DbError::NotFound)
}

/// Look up by namespaced identity ("kind", "identity"). Returns `None` if not
/// found. The lookup is a direct primary-key query because `users.id` is
/// derived from `(kind, identity)` via `UUIDv5`.
pub fn get_by_identity(db: &CentralDb, kind: &str, identity: &str) -> Result<Option<User>, DbError> {
    let id = derive_user_id(kind, identity);
    let conn = db.conn()?;
    Ok(conn
        .query_row(
            "SELECT id, kind, display_name, created_at FROM users WHERE id = ?1",
            params![id.as_uuid().to_string()],
            row_to_user,
        )
        .optional()?)
}

/// Upsert based on `(kind, identity)`.
///
/// The row's id is a `UUIDv5` derived deterministically from `(kind, identity)`
/// — this makes the operation idempotent: repeated calls return the same id,
/// updating `display_name` when supplied.
pub fn upsert(db: &CentralDb, req: UpsertUser) -> Result<User, DbError> {
    let UpsertUser {
        kind,
        identity,
        display_name,
    } = req;
    let id = derive_user_id(&kind, &identity);
    let now = Utc::now();
    let conn = db.conn()?;
    conn.execute(
        "INSERT INTO users (id, kind, display_name, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
             display_name = COALESCE(excluded.display_name, users.display_name)",
        params![
            id.as_uuid().to_string(),
            kind,
            display_name,
            now.to_rfc3339(),
        ],
    )?;
    drop(conn);
    get(db, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> CentralDb {
        CentralDb::open_in_memory().unwrap()
    }

    #[test]
    fn upsert_then_get() {
        let db = db();
        let u = upsert(
            &db,
            UpsertUser {
                kind: "telegram".into(),
                identity: "alice".into(),
                display_name: Some("Alice".into()),
            },
        )
        .unwrap();
        let fetched = get(&db, u.id).unwrap();
        assert_eq!(u, fetched);
        assert_eq!(fetched.kind, "telegram");
        assert_eq!(fetched.display_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn get_missing_is_not_found() {
        let db = db();
        let err = get(&db, UserId::new()).unwrap_err();
        assert!(matches!(err, DbError::NotFound));
    }

    #[test]
    fn upsert_is_idempotent_same_id() {
        let db = db();
        let a = upsert(
            &db,
            UpsertUser {
                kind: "slack".into(),
                identity: "bob".into(),
                display_name: None,
            },
        )
        .unwrap();
        let b = upsert(
            &db,
            UpsertUser {
                kind: "slack".into(),
                identity: "bob".into(),
                display_name: Some("Bob".into()),
            },
        )
        .unwrap();
        assert_eq!(a.id, b.id);
        // Newly-supplied display_name should win.
        assert_eq!(b.display_name.as_deref(), Some("Bob"));
    }

    #[test]
    fn upsert_preserves_display_name_when_none_supplied() {
        let db = db();
        let _ = upsert(
            &db,
            UpsertUser {
                kind: "slack".into(),
                identity: "carol".into(),
                display_name: Some("Carol".into()),
            },
        )
        .unwrap();
        let again = upsert(
            &db,
            UpsertUser {
                kind: "slack".into(),
                identity: "carol".into(),
                display_name: None,
            },
        )
        .unwrap();
        assert_eq!(again.display_name.as_deref(), Some("Carol"));
    }

    #[test]
    fn get_by_identity_finds_existing() {
        let db = db();
        let u = upsert(
            &db,
            UpsertUser {
                kind: "telegram".into(),
                identity: "dave".into(),
                display_name: None,
            },
        )
        .unwrap();
        let found = get_by_identity(&db, "telegram", "dave").unwrap();
        assert_eq!(found, Some(u));
    }

    #[test]
    fn get_by_identity_returns_none_for_missing() {
        let db = db();
        assert!(get_by_identity(&db, "telegram", "ghost").unwrap().is_none());
    }

    #[test]
    fn kind_and_identity_namespacing_is_distinct() {
        let db = db();
        let a = upsert(
            &db,
            UpsertUser {
                kind: "telegram".into(),
                identity: "shared".into(),
                display_name: None,
            },
        )
        .unwrap();
        let b = upsert(
            &db,
            UpsertUser {
                kind: "slack".into(),
                identity: "shared".into(),
                display_name: None,
            },
        )
        .unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn list_is_ordered_by_created_at() {
        let db = db();
        for i in 0..3 {
            upsert(
                &db,
                UpsertUser {
                    kind: "telegram".into(),
                    identity: format!("user-{i}"),
                    display_name: None,
                },
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows = list(&db).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].created_at <= rows[1].created_at);
        assert!(rows[1].created_at <= rows[2].created_at);
    }

    #[test]
    fn list_empty_when_no_users() {
        let db = db();
        assert!(list(&db).unwrap().is_empty());
    }
}
