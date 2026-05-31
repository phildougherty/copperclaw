//! Verify that a write to `inbound.db` from one connection (host
//! semantics — open, write, close) is visible to a separate reader
//! (container semantics — read-only with `mmap_size=0`) within 100ms.
//!
//! On a real bind-mount, WAL mode silently fails this contract because
//! the host-side mmap of the `-shm` file doesn't propagate guest
//! reads. We don't need a real bind-mount to test the semantics — the
//! `journal_mode=DELETE` + open-write-close-per-op pattern works the
//! same way on a local filesystem and serves as a fast regression
//! check that the pragma stays in place.

use chrono::Utc;
use ironclaw_db::session::{open_inbound, open_inbound_ro_no_mmap, SessionPaths};
use ironclaw_db::tables::messages_in;
use ironclaw_types::{AgentGroupId, ChannelType, MessageId, MessageKind, SessionId};
use serde_json::json;
use std::time::{Duration, Instant};

fn write_msg(paths: &SessionPaths) -> MessageId {
    let conn = open_inbound(paths).unwrap();
    let id = MessageId::new();
    messages_in::insert(
        &conn,
        &messages_in::WriteInbound {
            id,
            kind: MessageKind::Chat,
            timestamp: Utc::now(),
            content: json!({"text":"x"}),
            trigger: true,
            on_wake: false,
            process_after: None,
            recurrence: None,
            series_id: None,
            platform_id: Some("p".into()),
            channel_type: Some(ChannelType::new("cli")),
            thread_id: None,
            source_session_id: None,
            reply_to: None,
            is_group: None,
        },
    )
    .unwrap();
    // Connection drop closes the file — this is the "host" lifecycle.
    id
}

fn read_pending(paths: &SessionPaths) -> usize {
    let ro = open_inbound_ro_no_mmap(paths).unwrap();
    messages_in::get_pending(&ro, true, 100).unwrap().len()
}

#[test]
fn write_then_read_observes_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());

    // Initialize the file (this also creates schema).
    let _ = open_inbound(&paths).unwrap();
    assert_eq!(read_pending(&paths), 0);

    let _id = write_msg(&paths);

    // The host write should be visible immediately; we allow up to 100ms
    // to be tolerant of slow CI.
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        if read_pending(&paths) == 1 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "write not visible to ro reader within 100ms"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn journal_mode_is_delete_after_first_open() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
    let conn = open_inbound(&paths).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "delete");
    drop(conn);

    // Reopen and verify it sticks.
    let again = open_inbound(&paths).unwrap();
    let mode: String = again
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "delete");
}

#[test]
fn multiple_writes_visible_in_seq_order() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SessionPaths::new(tmp.path(), AgentGroupId::new(), SessionId::new());
    let _ = open_inbound(&paths).unwrap();

    for _ in 0..5 {
        write_msg(&paths);
    }

    let ro = open_inbound_ro_no_mmap(&paths).unwrap();
    let rows = messages_in::get_pending(&ro, true, 100).unwrap();
    assert_eq!(rows.len(), 5);

    // Seq is descending in get_pending (newest first); verify strict ordering.
    for win in rows.windows(2) {
        assert!(win[0].seq > win[1].seq);
        assert_eq!(win[0].seq % 2, 0, "host seqs must be even");
    }
}
