//! Recipient resolution against the session's destinations table and
//! session-routing fallback.
//!
//! When the agent emits a tool effect (`send_message`, `send_file`, …) with
//! `to: None` we resolve it against `session_routing` (reply-in-place).
//! When `to: Some(Recipient::Channel { id })` was supplied, we look the id
//! up in the per-session `destinations` table, accepting either the bare
//! name or a fully-qualified `channel_type:platform_id` string.

use copperclaw_db::tables::destinations;
use copperclaw_db::tables::session_routing;
use copperclaw_mcp::Recipient;
use copperclaw_types::ChannelType;
use copperclaw_types::routing::{DestinationKind, DestinationRow, SessionRouting};
use rusqlite::Connection;

/// A successfully resolved route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoute {
    /// Destination kind.
    pub kind: DestinationKind,
    /// Channel type (e.g. `"telegram"`). `None` for agent-to-agent.
    pub channel_type: Option<ChannelType>,
    /// Platform id (channel-specific chat id). `None` for agent-to-agent.
    pub platform_id: Option<String>,
    /// Thread id (for platforms that support threads), if known.
    pub thread_id: Option<String>,
    /// Destination's `agent_group_id` for agent-to-agent routes.
    pub agent_group_id: Option<copperclaw_types::AgentGroupId>,
}

impl ResolvedRoute {
    fn from_session_routing(r: SessionRouting) -> Option<Self> {
        let SessionRouting {
            channel_type,
            platform_id,
            thread_id,
        } = r;
        if channel_type.is_none() && platform_id.is_none() {
            return None;
        }
        Some(Self {
            kind: DestinationKind::Channel,
            channel_type,
            platform_id,
            thread_id,
            agent_group_id: None,
        })
    }

    fn from_destination(d: DestinationRow) -> Self {
        let DestinationRow {
            name: _,
            display_name: _,
            kind,
            channel_type,
            platform_id,
            agent_group_id,
        } = d;
        Self {
            kind,
            channel_type,
            platform_id,
            thread_id: None,
            agent_group_id,
        }
    }
}

/// Resolve `recipient` against the inbound DB.
///
/// - `None` -> read `session_routing` and return the reply-in-place route.
/// - `Some(Channel { id })` -> look `id` up in `destinations` by name; if no
///   row matches, parse `id` as `"<channel_type>:<platform_id>"` and return
///   the synthesised route.
/// - `Some(Agent { session_id })` -> agent-to-agent route — we don't have
///   the destination's `agent_group_id` from a session id alone, so we just
///   pass the session id through in a synthetic `platform_id` field with the
///   `ChannelType::AGENT` channel type.
/// - `Some(User { id })` -> user-targeted route. We pass it through with
///   `channel_type=None` and the user id in `platform_id` so the host can
///   resolve the DM.
///
/// Returns `Ok(None)` if no route could be determined (which the caller
/// should treat as a soft failure: the host will drop the message).
pub fn resolve_recipient(
    inbound: &Connection,
    recipient: Option<&Recipient>,
) -> Result<Option<ResolvedRoute>, copperclaw_db::DbError> {
    match recipient {
        None => Ok(session_routing::read(inbound)?.and_then(ResolvedRoute::from_session_routing)),
        Some(Recipient::Channel { id }) => resolve_channel(inbound, id),
        Some(Recipient::Agent { session_id }) => Ok(Some(ResolvedRoute {
            kind: DestinationKind::Agent,
            channel_type: Some(ChannelType::new(ChannelType::AGENT)),
            platform_id: Some(session_id.clone()),
            thread_id: None,
            agent_group_id: None,
        })),
        Some(Recipient::User { id }) => Ok(Some(ResolvedRoute {
            kind: DestinationKind::Channel,
            channel_type: None,
            platform_id: Some(id.clone()),
            thread_id: None,
            agent_group_id: None,
        })),
    }
}

fn resolve_channel(
    inbound: &Connection,
    id: &str,
) -> Result<Option<ResolvedRoute>, copperclaw_db::DbError> {
    if let Some(row) = destinations::get(inbound, id)? {
        return Ok(Some(ResolvedRoute::from_destination(row)));
    }
    // Fallback: `channel_type:platform_id` parse.
    if let Some((ct, pid)) = id.split_once(':') {
        if !ct.is_empty() && !pid.is_empty() {
            return Ok(Some(ResolvedRoute {
                kind: DestinationKind::Channel,
                channel_type: Some(ChannelType::new(ct)),
                platform_id: Some(pid.to_string()),
                thread_id: None,
                agent_group_id: None,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::session::{SessionPaths, open_inbound};
    use copperclaw_types::routing::DestinationRow;
    use copperclaw_types::{AgentGroupId, SessionId};

    fn fresh_inbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_inbound(&paths).unwrap();
        (tmp, conn)
    }

    fn channel_dest(name: &str) -> DestinationRow {
        DestinationRow {
            name: name.into(),
            display_name: format!("{name} display"),
            kind: DestinationKind::Channel,
            channel_type: Some(ChannelType::new("telegram")),
            platform_id: Some("chat-99".into()),
            agent_group_id: None,
        }
    }

    #[test]
    fn none_reads_session_routing() {
        let (_tmp, mut conn) = fresh_inbound();
        session_routing::write(
            &conn,
            &SessionRouting {
                channel_type: Some(ChannelType::new("cli")),
                platform_id: Some("chat-1".into()),
                thread_id: Some("t-1".into()),
            },
        )
        .unwrap();
        // shadow the &mut by re-borrowing immutably.
        let _ = &mut conn;
        let r = resolve_recipient(&conn, None).unwrap().unwrap();
        assert_eq!(r.kind, DestinationKind::Channel);
        assert_eq!(
            r.channel_type.as_ref().map(ChannelType::as_str),
            Some("cli")
        );
        assert_eq!(r.platform_id.as_deref(), Some("chat-1"));
        assert_eq!(r.thread_id.as_deref(), Some("t-1"));
    }

    #[test]
    fn none_returns_none_when_routing_blank() {
        let (_tmp, conn) = fresh_inbound();
        assert!(resolve_recipient(&conn, None).unwrap().is_none());
    }

    #[test]
    fn none_returns_none_when_routing_all_nulls() {
        let (_tmp, conn) = fresh_inbound();
        session_routing::write(
            &conn,
            &SessionRouting {
                channel_type: None,
                platform_id: None,
                thread_id: None,
            },
        )
        .unwrap();
        assert!(resolve_recipient(&conn, None).unwrap().is_none());
    }

    #[test]
    fn channel_resolves_via_destinations_table() {
        let (_tmp, mut conn) = fresh_inbound();
        destinations::replace_all(&mut conn, &[channel_dest("alice")]).unwrap();
        let r = resolve_recipient(&conn, Some(&Recipient::Channel { id: "alice".into() }))
            .unwrap()
            .unwrap();
        assert_eq!(r.kind, DestinationKind::Channel);
        assert_eq!(r.channel_type.unwrap().as_str(), "telegram");
        assert_eq!(r.platform_id.as_deref(), Some("chat-99"));
    }

    #[test]
    fn channel_falls_back_to_qualified_form_when_destination_missing() {
        let (_tmp, conn) = fresh_inbound();
        let r = resolve_recipient(
            &conn,
            Some(&Recipient::Channel {
                id: "telegram:chat-123".into(),
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.channel_type.unwrap().as_str(), "telegram");
        assert_eq!(r.platform_id.as_deref(), Some("chat-123"));
    }

    #[test]
    fn channel_returns_none_when_truly_unresolvable() {
        let (_tmp, conn) = fresh_inbound();
        let r = resolve_recipient(
            &conn,
            Some(&Recipient::Channel {
                id: "bogus-no-colon".into(),
            }),
        )
        .unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn channel_with_empty_halves_is_none() {
        let (_tmp, conn) = fresh_inbound();
        assert!(
            resolve_recipient(&conn, Some(&Recipient::Channel { id: ":xyz".into() }))
                .unwrap()
                .is_none()
        );
        assert!(
            resolve_recipient(&conn, Some(&Recipient::Channel { id: "abc:".into() }))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn agent_recipient_maps_to_agent_channel_type() {
        let (_tmp, conn) = fresh_inbound();
        let r = resolve_recipient(
            &conn,
            Some(&Recipient::Agent {
                session_id: "sess_9".into(),
            }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.kind, DestinationKind::Agent);
        assert_eq!(r.channel_type.unwrap().as_str(), ChannelType::AGENT);
        assert_eq!(r.platform_id.as_deref(), Some("sess_9"));
    }

    #[test]
    fn user_recipient_passes_through_id() {
        let (_tmp, conn) = fresh_inbound();
        let r = resolve_recipient(&conn, Some(&Recipient::User { id: "u_42".into() }))
            .unwrap()
            .unwrap();
        assert_eq!(r.kind, DestinationKind::Channel);
        assert!(r.channel_type.is_none());
        assert_eq!(r.platform_id.as_deref(), Some("u_42"));
    }

    #[test]
    fn agent_destination_passes_through_agent_group_id() {
        let (_tmp, mut conn) = fresh_inbound();
        let group = AgentGroupId::new();
        let row = DestinationRow {
            name: "bot".into(),
            display_name: "Bot".into(),
            kind: DestinationKind::Agent,
            channel_type: None,
            platform_id: None,
            agent_group_id: Some(group),
        };
        destinations::replace_all(&mut conn, &[row]).unwrap();
        let r = resolve_recipient(&conn, Some(&Recipient::Channel { id: "bot".into() }))
            .unwrap()
            .unwrap();
        assert_eq!(r.kind, DestinationKind::Agent);
        assert_eq!(r.agent_group_id, Some(group));
    }
}
