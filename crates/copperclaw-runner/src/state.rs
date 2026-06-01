//! Persisted runner state in `outbound.db::session_state`.
//!
//! Two keys:
//! - `runner.history` — JSON-encoded `Vec<HistoryMessage>`. The full chat
//!   transcript the runner replays to the provider on every turn.
//! - `runner.continuation` — opaque continuation token string returned by
//!   the provider via [`ProviderEvent::Init`]. Resumed across restarts.
//!
//! [`ProviderEvent::Init`]: copperclaw_types::ProviderEvent::Init

use copperclaw_db::DbError;
use copperclaw_db::tables::session_state;
use copperclaw_providers::HistoryMessage;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Key used to store the message history.
pub const KEY_HISTORY: &str = "runner.history";
/// Key used to store the provider continuation token.
pub const KEY_CONTINUATION: &str = "runner.continuation";

/// Bundle returned by [`load_state`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedState {
    /// Replayed message history (empty for a fresh session).
    pub history: Vec<HistoryMessage>,
    /// Opaque continuation token, if one was persisted.
    pub continuation: Option<String>,
}

/// Load history and continuation from `outbound`. Missing keys decode as
/// empty / `None`. Malformed history blobs are surfaced as an error so the
/// caller can decide to start fresh rather than silently swallowing them.
pub fn load_state(outbound: &Connection) -> Result<PersistedState, DbError> {
    let history = match session_state::get(outbound, KEY_HISTORY)? {
        None => Vec::new(),
        Some(s) => serde_json::from_str(&s)?,
    };
    let continuation = session_state::get(outbound, KEY_CONTINUATION)?;
    Ok(PersistedState {
        history,
        continuation,
    })
}

/// Save history and continuation to `outbound`. Pass `continuation = None`
/// to clear any previous value.
pub fn save_state(
    outbound: &Connection,
    history: &[HistoryMessage],
    continuation: Option<&str>,
) -> Result<(), DbError> {
    let json = serde_json::to_string(history)?;
    session_state::set(outbound, KEY_HISTORY, &json)?;
    match continuation {
        Some(c) => session_state::set(outbound, KEY_CONTINUATION, c)?,
        None => {
            // Tolerate not-found: a fresh session has nothing to clear.
            match session_state::delete(outbound, KEY_CONTINUATION) {
                Ok(()) | Err(DbError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::session::{SessionPaths, open_outbound};
    use copperclaw_types::{AgentGroupId, SessionId};

    fn fresh_outbound() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
        let conn = open_outbound(&paths).unwrap();
        (tmp, conn)
    }

    #[test]
    fn load_returns_empty_when_unset() {
        let (_tmp, conn) = fresh_outbound();
        let st = load_state(&conn).unwrap();
        assert!(st.history.is_empty());
        assert!(st.continuation.is_none());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let (_tmp, conn) = fresh_outbound();
        let history = vec![
            HistoryMessage::User {
                content: "hi".into(),
            },
            HistoryMessage::Assistant {
                content: "hello".into(),
            },
        ];
        save_state(&conn, &history, Some("cont-abc")).unwrap();
        let st = load_state(&conn).unwrap();
        assert_eq!(st.history, history);
        assert_eq!(st.continuation.as_deref(), Some("cont-abc"));
    }

    #[test]
    fn save_with_none_continuation_clears_existing() {
        let (_tmp, conn) = fresh_outbound();
        save_state(&conn, &[], Some("c1")).unwrap();
        save_state(&conn, &[], None).unwrap();
        let st = load_state(&conn).unwrap();
        assert!(st.continuation.is_none());
    }

    #[test]
    fn save_with_none_continuation_when_absent_is_ok() {
        let (_tmp, conn) = fresh_outbound();
        save_state(&conn, &[], None).unwrap();
        let st = load_state(&conn).unwrap();
        assert!(st.continuation.is_none());
    }

    #[test]
    fn malformed_history_errors() {
        let (_tmp, conn) = fresh_outbound();
        session_state::set(&conn, KEY_HISTORY, "not json").unwrap();
        let err = load_state(&conn).unwrap_err();
        assert!(matches!(err, DbError::Json(_)));
    }

    #[test]
    fn load_returns_continuation_without_history() {
        let (_tmp, conn) = fresh_outbound();
        session_state::set(&conn, KEY_CONTINUATION, "c1").unwrap();
        let st = load_state(&conn).unwrap();
        assert!(st.history.is_empty());
        assert_eq!(st.continuation.as_deref(), Some("c1"));
    }

    #[test]
    fn save_overwrites_history_each_time() {
        let (_tmp, conn) = fresh_outbound();
        save_state(
            &conn,
            &[HistoryMessage::User {
                content: "first".into(),
            }],
            None,
        )
        .unwrap();
        save_state(
            &conn,
            &[HistoryMessage::User {
                content: "second".into(),
            }],
            None,
        )
        .unwrap();
        let st = load_state(&conn).unwrap();
        assert_eq!(st.history.len(), 1);
        match &st.history[0] {
            HistoryMessage::User { content } => assert_eq!(content, "second"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn keys_are_distinct_constants() {
        assert_ne!(KEY_HISTORY, KEY_CONTINUATION);
        assert!(KEY_HISTORY.starts_with("runner."));
        assert!(KEY_CONTINUATION.starts_with("runner."));
    }
}
