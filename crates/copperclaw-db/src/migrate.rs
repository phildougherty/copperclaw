//! Migration runner — applies embedded SQL files in lexicographic order,
//! recording each by filename in `schema_version`.
//!
//! Names (filename without `.sql`) are the dedup key; future migrations
//! can be added in either tree (central / session) without renumbering
//! existing migrations.

use crate::DbError;
use rusqlite::{Connection, params};

/// Discriminator for which migration set to apply.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MigrationSet {
    Central,
    SessionInbound,
    SessionOutbound,
    /// Per-agent-group searchable memory store (`memory.db`). One file per
    /// agent group, applied by [`crate::memory::MemoryStore::open`].
    Memory,
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
    Migration {
        name: "007_container_config_extensions",
        sql: include_str!("../migrations/007_container_config_extensions.sql"),
    },
    Migration {
        name: "008_outbound_dropped_messages",
        sql: include_str!("../migrations/008_outbound_dropped_messages.sql"),
    },
    Migration {
        name: "009_rate_limit_caps",
        sql: include_str!("../migrations/009_rate_limit_caps.sql"),
    },
    Migration {
        name: "010_tasks",
        sql: include_str!("../migrations/010_tasks.sql"),
    },
    Migration {
        name: "011_agent_group_subagent_depth",
        sql: include_str!("../migrations/011_agent_group_subagent_depth.sql"),
    },
    Migration {
        name: "012_container_config_coding_enabled",
        sql: include_str!("../migrations/012_container_config_coding_enabled.sql"),
    },
    Migration {
        name: "013_sessions_source_session",
        sql: include_str!("../migrations/013_sessions_source_session.sql"),
    },
    Migration {
        name: "014_container_config_surface_thinking",
        sql: include_str!("../migrations/014_container_config_surface_thinking.sql"),
    },
    Migration {
        name: "016_pending_approvals_unique",
        sql: include_str!("../migrations/016_pending_approvals_unique.sql"),
    },
    Migration {
        name: "017_approval_decisions",
        sql: include_str!("../migrations/017_approval_decisions.sql"),
    },
    Migration {
        name: "018_dm_pairing_codes",
        sql: include_str!("../migrations/018_dm_pairing_codes.sql"),
    },
    Migration {
        name: "019_container_config_tool_profile",
        sql: include_str!("../migrations/019_container_config_tool_profile.sql"),
    },
    Migration {
        name: "020_provider_profiles",
        sql: include_str!("../migrations/020_provider_profiles.sql"),
    },
    Migration {
        name: "022_mcp_oauth_tokens",
        sql: include_str!("../migrations/022_mcp_oauth_tokens.sql"),
    },
];

const SESSION_INBOUND: &[Migration] = &[
    Migration {
        name: "002_session_inbound",
        sql: include_str!("../migrations/002_session_inbound.sql"),
    },
    Migration {
        name: "015_messages_in_reply_to_is_group",
        sql: include_str!("../migrations/015_messages_in_reply_to_is_group.sql"),
    },
];

const SESSION_OUTBOUND: &[Migration] = &[Migration {
    name: "003_session_outbound",
    sql: include_str!("../migrations/003_session_outbound.sql"),
}];

const MEMORY: &[Migration] = &[Migration {
    name: "021_memory_store",
    sql: include_str!("../migrations/021_memory_store.sql"),
}];

impl MigrationSet {
    fn migrations(self) -> &'static [Migration] {
        match self {
            Self::Central => CENTRAL,
            Self::SessionInbound => SESSION_INBOUND,
            Self::SessionOutbound => SESSION_OUTBOUND,
            Self::Memory => MEMORY,
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
        tx.execute_batch(migration.sql)
            .map_err(|source| DbError::Migration {
                name: migration.name.to_string(),
                source,
            })?;
        tx.execute(
            "INSERT INTO schema_version (name, applied) VALUES (?1, ?2)",
            params![migration.name, chrono::Utc::now().to_rfc3339(),],
        )?;
        tx.commit()?;
        tracing::info!(target: "copperclaw_db::migrate", migration = migration.name, "applied");
    }

    Ok(())
}

/// Returns the list of migration names recorded in `schema_version`.
pub fn applied_migrations(conn: &Connection) -> Result<Vec<String>, DbError> {
    let mut stmt = conn.prepare("SELECT name FROM schema_version ORDER BY version")?;
    let names = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(names.collect::<Result<_, _>>()?)
}

/// The "expected" schema version for the central DB: the number of
/// entries in the in-binary [`CENTRAL`] migrations list.
///
/// Design rationale: migrations are append-only (never removed or
/// reordered), so the count is monotonically increasing and is
/// equivalent to the highest sequence number. Comparing this value
/// against [`applied_central_schema_version`] lets the host detect
/// both forward- and backward-compatibility problems at boot.
#[must_use]
pub fn expected_central_schema_version() -> usize {
    CENTRAL.len()
}

/// The "applied" schema version for the central DB: the number of
/// rows recorded in `schema_version` (i.e. the count of migrations
/// that have actually been run against this database file).
///
/// Returns `Ok(None)` when `schema_version` doesn't exist yet (a
/// completely fresh DB that hasn't been migrated). Returns `Ok(Some(n))`
/// otherwise.
pub fn applied_central_schema_version(conn: &Connection) -> Result<Option<usize>, DbError> {
    // Check whether the table exists at all first (fresh DB, pre-migrate).
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if !table_exists {
        return Ok(None);
    }
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))?;
    // COUNT(*) is always non-negative; the try_from can't fail in practice.
    Ok(Some(usize::try_from(count).unwrap_or(0)))
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
            "approval_decisions",
            "pending_channel_approvals",
            "agent_destinations",
            "unregistered_senders",
            "dropped_messages",
            "outbound_dropped_messages",
            "container_configs",
            "tasks",
            "provider_profiles",
            "provider_health",
            "mcp_oauth_tokens",
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
        for table in [
            "messages_in",
            "delivered",
            "destinations",
            "session_routing",
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

    #[test]
    fn expected_central_schema_version_matches_central_list_length() {
        assert_eq!(expected_central_schema_version(), CENTRAL.len());
        assert!(expected_central_schema_version() > 0);
    }

    #[test]
    fn applied_central_schema_version_is_none_on_fresh_db() {
        let conn = fresh();
        let v = applied_central_schema_version(&conn).unwrap();
        assert_eq!(v, None);
    }

    #[test]
    fn applied_central_schema_version_equals_expected_after_migration() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::Central).unwrap();
        let applied = applied_central_schema_version(&conn).unwrap();
        assert_eq!(applied, Some(expected_central_schema_version()));
    }

    #[test]
    fn applied_central_schema_version_reflects_future_row() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::Central).unwrap();
        // Simulate a future migration written by a newer binary.
        conn.execute(
            "INSERT INTO schema_version (name, applied) VALUES ('999_future', '2099-01-01T00:00:00Z')",
            [],
        ).unwrap();
        let applied = applied_central_schema_version(&conn).unwrap();
        assert_eq!(applied, Some(expected_central_schema_version() + 1));
    }

    #[test]
    fn memory_migrations_apply_and_create_fts() {
        let mut conn = fresh();
        run_migrations(&mut conn, MigrationSet::Memory).unwrap();
        for (kind, name) in [
            ("table", "memory_entries"),
            ("table", "memory_fts"),
            ("trigger", "memory_entries_ai"),
            ("trigger", "memory_entries_ad"),
            ("trigger", "memory_entries_au"),
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type=?1 AND name=?2",
                    params![kind, name],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing {kind} `{name}`");
        }
        // FTS5 is compiled into the bundled SQLite — a MATCH must work.
        conn.execute_batch(
            "INSERT INTO memory_entries (mem_key, body, created_at, updated_at)
             VALUES ('k', 'hello world', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "FTS5 MATCH should find the inserted row");
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
