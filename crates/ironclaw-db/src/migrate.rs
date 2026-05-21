//! Migration runner — applies embedded SQL files in lexicographic order,
//! recording each by filename in `schema_version`.
//!
//! Names (filename without `.sql`) are the dedup key; future migrations
//! can be added in either tree (central / session) without renumbering
//! existing migrations.

use crate::DbError;
use rusqlite::{params, Connection};

/// Discriminator for which migration set to apply.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MigrationSet {
    Central,
    SessionInbound,
    SessionOutbound,
}

struct Migration {
    name: &'static str,
    sql: &'static str,
}

const CENTRAL: &[Migration] = &[
    Migration {
        name: "001_initial",
        sql: include_str!("../migrations/001_initial.sql"),
    },
    Migration {
        name: "004_audit_log",
        sql: include_str!("../migrations/004_audit_log.sql"),
    },
    Migration {
        name: "005_agent_turns",
        sql: include_str!("../migrations/005_agent_turns.sql"),
    },
    Migration {
        name: "006_group_budgets",
        sql: include_str!("../migrations/006_group_budgets.sql"),
    },
];

const SESSION_INBOUND: &[Migration] = &[Migration {
    name: "002_session_inbound",
    sql: include_str!("../migrations/002_session_inbound.sql"),
}];

const SESSION_OUTBOUND: &[Migration] = &[Migration {
    name: "003_session_outbound",
    sql: include_str!("../migrations/003_session_outbound.sql"),
}];

impl MigrationSet {
    fn migrations(self) -> &'static [Migration] {
        match self {
            Self::Central => CENTRAL,
            Self::SessionInbound => SESSION_INBOUND,
            Self::SessionOutbound => SESSION_OUTBOUND,
        }
    }
}

/// Apply all pending migrations from the given set to the connection.
/// Idempotent: each migration is applied at most once.
pub fn run_migrations(conn: &mut Connection, set: MigrationSet) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
           version INTEGER PRIMARY KEY AUTOINCREMENT,
           name    TEXT NOT NULL UNIQUE,
           applied TEXT NOT NULL
         );",
    )?;

    let applied: std::collections::HashSet<String> = {
        let mut stmt = conn.prepare("SELECT name FROM schema_version")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };

    for migration in set.migrations() {
        if applied.contains(migration.name) {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql).map_err(|source| DbError::Migration {
            name: migration.name.to_string(),
            source,
        })?;
        tx.execute(
            "INSERT INTO schema_version (name, applied) VALUES (?1, ?2)",
            params![
                migration.name,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        tracing::info!(target: "ironclaw_db::migrate", migration = migration.name, "applied");
    }

    Ok(())
}

/// Returns the list of migration names recorded in `schema_version`.
pub fn applied_migrations(conn: &Connection) -> Result<Vec<String>, DbError> {
    let mut stmt = conn.prepare("SELECT name FROM schema_version ORDER BY version")?;
    let names = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(names.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn central_migrations_apply_cleanly() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::Central).unwrap();
        let applied = applied_migrations(&conn).unwrap();
        // Order-independent: the central set grows over time but the
        // contents are fully specified by the const tables here.
        let expected: std::collections::HashSet<String> =
            CENTRAL.iter().map(|m| m.name.to_string()).collect();
        let got: std::collections::HashSet<String> = applied.into_iter().collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn migrations_are_idempotent() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::Central).unwrap();
        run_migrations(&mut conn, MigrationSet::Central).unwrap();
        let applied = applied_migrations(&conn).unwrap();
        // Idempotent — running twice produces the same set, not a
        // doubled count.
        assert_eq!(applied.len(), CENTRAL.len());
    }

    #[test]
    fn central_creates_expected_tables() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::Central).unwrap();
        for table in [
            "agent_groups",
            "messaging_groups",
            "messaging_group_agents",
            "users",
            "user_roles",
            "agent_group_members",
            "user_dms",
            "sessions",
            "pending_questions",
            "pending_approvals",
            "pending_sender_approvals",
            "pending_channel_approvals",
            "agent_destinations",
            "unregistered_senders",
            "dropped_messages",
            "container_configs",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "expected table `{table}` to exist");
        }
    }

    #[test]
    fn session_inbound_migrations_apply() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::SessionInbound).unwrap();
        for table in ["messages_in", "delivered", "destinations", "session_routing"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing table `{table}`");
        }
    }

    #[test]
    fn session_outbound_migrations_apply() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::SessionOutbound).unwrap();
        for table in [
            "messages_out",
            "processing_ack",
            "session_state",
            "container_state",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing table `{table}`");
        }
    }
}
