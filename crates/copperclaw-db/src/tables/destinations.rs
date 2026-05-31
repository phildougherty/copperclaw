//! Writes and reads against per-session `inbound.db::destinations`.
//!
//! Host-owned writer; the container reads via `open_inbound_ro_no_mmap` to
//! resolve `send_message(to=..)` targets. The entire table is rebuilt on
//! every container wake via [`replace_all`].

use crate::DbError;
use copperclaw_types::routing::{DestinationKind, DestinationRow};
use copperclaw_types::{AgentGroupId, ChannelType};
use rusqlite::{params, Connection, OptionalExtension, Row};

/// Atomically rebuild the destinations table.
///
/// Runs DELETE-all + INSERT-each inside a single transaction so the container
/// never sees a half-populated table.
pub fn replace_all(conn: &mut Connection, rows: &[DestinationRow]) -> Result<(), DbError> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM destinations", [])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO destinations
               (name, display_name, type, channel_type, platform_id, agent_group_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for row in rows {
            stmt.execute(params![
                row.name,
                row.display_name,
                kind_as_str(row.kind),
                row.channel_type.as_ref().map(ChannelType::as_str),
                row.platform_id,
                row.agent_group_id.map(|a| a.as_uuid().to_string()),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// List every destination, ordered by name.
pub fn list(conn: &Connection) -> Result<Vec<DestinationRow>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT name, display_name, type, channel_type, platform_id, agent_group_id
         FROM destinations
         ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_destination)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Look up a single destination by `name`. Returns `None` if not found.
pub fn get(conn: &Connection, name: &str) -> Result<Option<DestinationRow>, DbError> {
    Ok(conn
        .query_row(
            "SELECT name, display_name, type, channel_type, platform_id, agent_group_id
             FROM destinations WHERE name = ?1",
            params![name],
            row_to_destination,
        )
        .optional()?)
}

fn kind_as_str(kind: DestinationKind) -> &'static str {
    match kind {
        DestinationKind::Channel => "channel",
        DestinationKind::Agent => "agent",
    }
}

fn row_to_destination(row: &Row<'_>) -> rusqlite::Result<DestinationRow> {
    let kind_str: String = row.get("type")?;
    let kind = match kind_str.as_str() {
        "channel" => DestinationKind::Channel,
        "agent" => DestinationKind::Agent,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown destination type {other}").into(),
            ))
        }
    };
    let channel_type: Option<String> = row.get("channel_type")?;
    let agent_group_id: Option<String> = row.get("agent_group_id")?;
    let agent_group_id = agent_group_id
        .as_deref()
        .map(uuid::Uuid::parse_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?
        .map(AgentGroupId);
    Ok(DestinationRow {
        name: row.get("name")?,
        display_name: row.get("display_name")?,
        kind,
        channel_type: channel_type.map(ChannelType::from),
        platform_id: row.get("platform_id")?,
        agent_group_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{open_inbound, SessionPaths};
    use copperclaw_types::{AgentGroupId, SessionId};

    fn fresh_inbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        (tmp, conn)
    }

    fn channel_row(name: &str) -> DestinationRow {
        DestinationRow {
            name: name.into(),
            display_name: format!("{name} display"),
            kind: DestinationKind::Channel,
            channel_type: Some(ChannelType::new("cli")),
            platform_id: Some("chat-1".into()),
            agent_group_id: None,
        }
    }

    fn agent_row(name: &str) -> DestinationRow {
        DestinationRow {
            name: name.into(),
            display_name: format!("{name} display"),
            kind: DestinationKind::Agent,
            channel_type: None,
            platform_id: None,
            agent_group_id: Some(AgentGroupId::new()),
        }
    }

    #[test]
    fn replace_all_then_list() {
        let (_tmp, mut conn) = fresh_inbound();
        let rows = vec![channel_row("alice"), agent_row("bob")];
        replace_all(&mut conn, &rows).unwrap();
        let out = list(&conn).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "alice");
        assert_eq!(out[0].kind, DestinationKind::Channel);
        assert_eq!(out[1].name, "bob");
        assert_eq!(out[1].kind, DestinationKind::Agent);
    }

    #[test]
    fn replace_all_clears_previous_rows() {
        let (_tmp, mut conn) = fresh_inbound();
        replace_all(&mut conn, &[channel_row("a")]).unwrap();
        replace_all(&mut conn, &[channel_row("b")]).unwrap();
        let out = list(&conn).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "b");
    }

    #[test]
    fn replace_all_empty_clears_table() {
        let (_tmp, mut conn) = fresh_inbound();
        replace_all(&mut conn, &[channel_row("a")]).unwrap();
        replace_all(&mut conn, &[]).unwrap();
        assert!(list(&conn).unwrap().is_empty());
    }

    #[test]
    fn replace_all_rolls_back_on_duplicate_name() {
        let (_tmp, mut conn) = fresh_inbound();
        replace_all(&mut conn, &[channel_row("a")]).unwrap();
        let err = replace_all(
            &mut conn,
            &[channel_row("dup"), channel_row("dup")],
        )
        .unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
        // Original row from the first call is preserved — the failed call's
        // DELETE-all and partial inserts were rolled back.
        let out = list(&conn).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "a");
    }

    #[test]
    fn get_returns_some_for_existing_name() {
        let (_tmp, mut conn) = fresh_inbound();
        let row = channel_row("alice");
        replace_all(&mut conn, &[row.clone()]).unwrap();
        let fetched = get(&conn, "alice").unwrap();
        assert_eq!(fetched, Some(row));
    }

    #[test]
    fn get_returns_none_for_missing_name() {
        let (_tmp, conn) = fresh_inbound();
        assert!(get(&conn, "absent").unwrap().is_none());
    }

    #[test]
    fn list_is_ordered_by_name() {
        let (_tmp, mut conn) = fresh_inbound();
        replace_all(
            &mut conn,
            &[channel_row("c"), channel_row("a"), channel_row("b")],
        )
        .unwrap();
        let out = list(&conn).unwrap();
        let names: Vec<_> = out.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn agent_round_trips_group_id() {
        let (_tmp, mut conn) = fresh_inbound();
        let row = agent_row("bot");
        let expected_group = row.agent_group_id;
        replace_all(&mut conn, &[row]).unwrap();
        let got = get(&conn, "bot").unwrap().unwrap();
        assert_eq!(got.agent_group_id, expected_group);
        assert_eq!(got.kind, DestinationKind::Agent);
    }

    #[test]
    fn row_decode_rejects_unknown_type() {
        let (_tmp, conn) = fresh_inbound();
        conn.execute(
            "INSERT INTO destinations (name, display_name, type) VALUES (?1, ?2, ?3)",
            params!["x", "X", "other"],
        )
        .unwrap();
        let err = list(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }

    #[test]
    fn row_decode_rejects_bad_agent_group_uuid() {
        let (_tmp, conn) = fresh_inbound();
        conn.execute(
            "INSERT INTO destinations (name, display_name, type, agent_group_id)
             VALUES (?1, ?2, 'agent', ?3)",
            params!["x", "X", "not-a-uuid"],
        )
        .unwrap();
        let err = list(&conn).unwrap_err();
        assert!(matches!(err, DbError::Sqlite(_)));
    }
}
