//! Writes and reads against per-session `inbound.db::session_routing`.
//!
//! Single-row table (`id = 1`). The host rewrites it on every container wake
//! so the runner can fall back to "reply in place" when an agent response
//! doesn't carry an explicit destination.

use crate::DbError;
use ironclaw_types::routing::SessionRouting;
use ironclaw_types::ChannelType;
use rusqlite::{params, Connection, OptionalExtension};

/// Read the current routing row, or `None` if it has never been written.
pub fn read(conn: &Connection) -> Result<Option<SessionRouting>, DbError> {
    let row = conn
        .query_row(
            "SELECT channel_type, platform_id, thread_id
             FROM session_routing WHERE id = 1",
            [],
            |row| {
                let channel_type: Option<String> = row.get("channel_type")?;
                Ok(SessionRouting {
                    channel_type: channel_type.map(ChannelType::from),
                    platform_id: row.get("platform_id")?,
                    thread_id: row.get("thread_id")?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Upsert the routing row. Uses `INSERT OR REPLACE` with the fixed `id = 1`
/// so callers don't need to know whether a previous wake initialized it.
pub fn write(conn: &Connection, routing: &SessionRouting) -> Result<(), DbError> {
    conn.execute(
        "INSERT OR REPLACE INTO session_routing
           (id, channel_type, platform_id, thread_id)
         VALUES (1, ?1, ?2, ?3)",
        params![
            routing.channel_type.as_ref().map(ChannelType::as_str),
            routing.platform_id,
            routing.thread_id,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open_inbound, SessionPaths};
    use ironclaw_types::{AgentGroupId, SessionId};

    fn fresh_inbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        (tmp, conn)
    }

    #[test]
    fn read_returns_none_when_empty() {
        let (_tmp, conn) = fresh_inbound();
        assert!(read(&conn).unwrap().is_none());
    }

    #[test]
    fn write_then_read_roundtrips() {
        let (_tmp, conn) = fresh_inbound();
        let routing = SessionRouting {
            channel_type: Some(ChannelType::new("cli")),
            platform_id: Some("chat-1".into()),
            thread_id: Some("t-1".into()),
        };
        write(&conn, &routing).unwrap();
        assert_eq!(read(&conn).unwrap(), Some(routing));
    }

    #[test]
    fn write_replaces_existing_row() {
        let (_tmp, conn) = fresh_inbound();
        write(
            &conn,
            &SessionRouting {
                channel_type: Some(ChannelType::new("cli")),
                platform_id: Some("first".into()),
                thread_id: None,
            },
        )
        .unwrap();
        let next = SessionRouting {
            channel_type: Some(ChannelType::new("telegram")),
            platform_id: Some("second".into()),
            thread_id: Some("t".into()),
        };
        write(&conn, &next).unwrap();
        assert_eq!(read(&conn).unwrap(), Some(next));
    }

    #[test]
    fn write_allows_all_nulls() {
        let (_tmp, conn) = fresh_inbound();
        let routing = SessionRouting {
            channel_type: None,
            platform_id: None,
            thread_id: None,
        };
        write(&conn, &routing).unwrap();
        assert_eq!(read(&conn).unwrap(), Some(routing));
    }

    #[test]
    fn manual_insert_with_wrong_id_is_rejected() {
        let (_tmp, conn) = fresh_inbound();
        let err = conn
            .execute(
                "INSERT INTO session_routing (id, channel_type, platform_id, thread_id)
                 VALUES (2, NULL, NULL, NULL)",
                [],
            )
            .unwrap_err();
        // The CHECK (id = 1) constraint must reject this.
        assert!(matches!(err, rusqlite::Error::SqliteFailure(_, _)));
    }
}
