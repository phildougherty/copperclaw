//! Inbound poll loop + `since_rowid` persistence.

use crate::bridge::IMessageBridge;
use crate::config::IMessageConfig;
use crate::parse::row_to_inbound;
use ironclaw_channels_core::AdapterError;
use ironclaw_types::InboundEvent;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// Compute the absolute path to the `since_rowid` persistence file from a
/// `data_dir` and the file name configured on [`IMessageConfig`].
pub fn since_rowid_path(data_dir: &Path, file: &str) -> PathBuf {
    data_dir.join(file)
}

/// Load the persisted last-seen `ROWID`, returning `0` when the file is
/// missing or malformed.
///
/// We never propagate an error here: a corrupt or absent rowid file just
/// means we'll start from the beginning (and the chat-db is huge, so the
/// first poll will warn but then catch up). Operators can `rm` the file to
/// force a re-scan.
pub fn load_since_rowid(path: &Path) -> i64 {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().parse().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Persist the latest seen `ROWID` to the configured file.
pub fn save_since_rowid(path: &Path, rowid: i64) -> Result<(), AdapterError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(AdapterError::Io)?;
    }
    std::fs::write(path, rowid.to_string()).map_err(AdapterError::Io)?;
    Ok(())
}

/// One iteration of the poll loop.
///
/// Reads the rowid floor from `since_rowid_path`, asks the bridge for
/// every row past it, forwards each translatable row through `inbound_tx`,
/// and finally persists the new high-water mark.
///
/// Returns `Ok(rows_emitted)` on success — useful for tests asserting how
/// many events came out of a given batch. Errors propagate as
/// [`AdapterError`] from the bridge; callers (the poll loop) log and
/// continue.
pub async fn poll_once(
    bridge: &dyn IMessageBridge,
    inbound_tx: &Sender<InboundEvent>,
    since_rowid_path: &Path,
) -> Result<usize, AdapterError> {
    let since = load_since_rowid(since_rowid_path);
    let rows = bridge.query_new_rows(since).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut emitted = 0_usize;
    let mut high_water = since;
    for row in &rows {
        if row.rowid > high_water {
            high_water = row.rowid;
        }
        let Some(event) = row_to_inbound(row) else {
            continue;
        };
        if inbound_tx.send(event).await.is_err() {
            // Receiver dropped — host shutting down. Persist what we
            // have so we don't replay the batch on next start.
            let _ = save_since_rowid(since_rowid_path, high_water);
            return Err(AdapterError::Transport(
                "imessage: inbound receiver closed".into(),
            ));
        }
        emitted += 1;
    }
    save_since_rowid(since_rowid_path, high_water)?;
    Ok(emitted)
}

/// Run the inbound poll loop until `cancel` fires.
///
/// Errors from individual iterations are logged and the loop continues —
/// chat.db is on the user's local disk, so a transient sqlite hiccup
/// should not take the channel down.
pub async fn poll_loop(
    bridge: Arc<dyn IMessageBridge>,
    cfg: IMessageConfig,
    data_dir: PathBuf,
    inbound_tx: Sender<InboundEvent>,
    cancel: CancellationToken,
) {
    let since_path = since_rowid_path(&data_dir, &cfg.since_rowid_file);
    let interval = std::time::Duration::from_millis(cfg.poll_interval_ms);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            () = tokio::time::sleep(interval) => {}
        }
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            r = poll_once(bridge.as_ref(), &inbound_tx, &since_path) => r,
        };
        match result {
            Ok(_n) => {}
            Err(err) => {
                tracing::warn!(error = %err, "imessage: poll iteration failed");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::bridge::{MockBridge, MockMessageRow};
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, timeout};

    fn dir() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn row(rowid: i64, handle: &str, text: &str) -> MockMessageRow {
        MockMessageRow {
            rowid,
            guid: format!("g-{rowid}"),
            text: Some(text.into()),
            date: 0,
            is_from_me: false,
            handle: Some(handle.into()),
            chat_id: None,
        }
    }

    #[test]
    fn since_rowid_path_joins_data_dir_and_file() {
        let p = since_rowid_path(Path::new("/tmp/x"), "rowid.txt");
        assert_eq!(p, PathBuf::from("/tmp/x/rowid.txt"));
    }

    #[test]
    fn load_since_rowid_returns_zero_when_missing() {
        let d = dir();
        let p = d.path().join("nope.txt");
        assert_eq!(load_since_rowid(&p), 0);
    }

    #[test]
    fn load_since_rowid_returns_zero_for_malformed_content() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        std::fs::write(&p, "not-a-number").unwrap();
        assert_eq!(load_since_rowid(&p), 0);
    }

    #[test]
    fn load_since_rowid_trims_trailing_whitespace() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        std::fs::write(&p, "  42  \n").unwrap();
        assert_eq!(load_since_rowid(&p), 42);
    }

    #[test]
    fn save_since_rowid_round_trips() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        save_since_rowid(&p, 99).unwrap();
        assert_eq!(load_since_rowid(&p), 99);
    }

    #[test]
    fn save_since_rowid_creates_missing_dirs() {
        let d = dir();
        let p = d.path().join("nested").join("rowid.txt");
        save_since_rowid(&p, 5).unwrap();
        assert!(p.exists());
        assert_eq!(load_since_rowid(&p), 5);
    }

    #[test]
    fn save_since_rowid_overwrites_previous_value() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        save_since_rowid(&p, 1).unwrap();
        save_since_rowid(&p, 2).unwrap();
        assert_eq!(load_since_rowid(&p), 2);
    }

    #[tokio::test]
    async fn poll_once_empty_rows_returns_zero() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        let m = MockBridge::new();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let n = poll_once(&m, &tx, &p).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn poll_once_persists_rowid_after_batch() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        let m = MockBridge::new();
        m.set_rows(vec![row(7, "+1", "hi"), row(8, "+2", "yo")]);
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let n = poll_once(&m, &tx, &p).await.unwrap();
        assert_eq!(n, 2);
        // Both events were forwarded.
        let _ = rx.recv().await.unwrap();
        let _ = rx.recv().await.unwrap();
        // High water = max rowid.
        assert_eq!(load_since_rowid(&p), 8);
    }

    #[tokio::test]
    async fn poll_once_filters_using_since_rowid() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        save_since_rowid(&p, 5).unwrap();
        let m = MockBridge::new();
        m.set_rows(vec![
            row(3, "+1", "old"),
            row(6, "+2", "new"),
            row(7, "+3", "newer"),
        ]);
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let n = poll_once(&m, &tx, &p).await.unwrap();
        assert_eq!(n, 2);
        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();
        assert_eq!(e1.message.content["text"], "new");
        assert_eq!(e2.message.content["text"], "newer");
        assert_eq!(load_since_rowid(&p), 7);
        // The bridge was asked with the persisted since=5.
        assert_eq!(m.query_calls(), vec![5]);
    }

    #[tokio::test]
    async fn poll_once_propagates_bridge_error() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        let m = MockBridge::new();
        m.push_query_err(AdapterError::Transport("boom".into()));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let err = poll_once(&m, &tx, &p).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
        // Persisted rowid is unchanged.
        assert_eq!(load_since_rowid(&p), 0);
    }

    #[tokio::test]
    async fn poll_once_skips_rows_that_translate_to_none() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        let m = MockBridge::new();
        // is_from_me=true row that the mock's query path would normally
        // filter, but if it slipped through, row_to_inbound would still
        // drop it. We bypass the mock's filter by setting it manually via
        // a normal row that has neither handle nor chat (translatable to
        // None).
        m.set_rows(vec![MockMessageRow {
            rowid: 11,
            guid: "g".into(),
            text: Some("x".into()),
            date: 0,
            is_from_me: false,
            handle: None,
            chat_id: None,
        }]);
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let n = poll_once(&m, &tx, &p).await.unwrap();
        assert_eq!(n, 0);
        // Rowid still advanced.
        assert_eq!(load_since_rowid(&p), 11);
    }

    #[tokio::test]
    async fn poll_once_when_receiver_dropped_returns_transport_and_persists() {
        let d = dir();
        let p = d.path().join("rowid.txt");
        let m = MockBridge::new();
        m.set_rows(vec![row(1, "+1", "a"), row(2, "+2", "b")]);
        // Drop receiver immediately.
        let (tx, rx) = mpsc::channel::<InboundEvent>(1);
        drop(rx);
        let err = poll_once(&m, &tx, &p).await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
        // Even on the error path we persisted (best effort).
        assert!(load_since_rowid(&p) >= 1);
    }

    #[tokio::test]
    async fn poll_loop_emits_events_then_stops_on_cancel() {
        let d = dir();
        let cfg = {
            let mut c = IMessageConfig::default();
            c.poll_interval_ms = 5;
            c.since_rowid_file = "rowid.txt".into();
            c
        };
        let m: Arc<dyn IMessageBridge> = Arc::new({
            let m = MockBridge::new();
            m.set_rows(vec![row(1, "+1", "hi")]);
            m
        });
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(poll_loop(
            m.clone(),
            cfg,
            d.path().to_path_buf(),
            tx,
            cancel.clone(),
        ));
        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("did not receive event")
            .expect("sender closed");
        assert_eq!(evt.platform_id, "handle:+1");
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn poll_loop_survives_bridge_errors() {
        let d = dir();
        let cfg = {
            let mut c = IMessageConfig::default();
            c.poll_interval_ms = 5;
            c.since_rowid_file = "rowid.txt".into();
            c
        };
        let bridge = {
            let m = MockBridge::new();
            m.push_query_err(AdapterError::Transport("hiccup".into()));
            m.push_rows(vec![row(2, "+1", "after-error")]);
            Arc::new(m) as Arc<dyn IMessageBridge>
        };
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(poll_loop(
            bridge,
            cfg,
            d.path().to_path_buf(),
            tx,
            cancel.clone(),
        ));
        let evt = timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("did not recover after error")
            .expect("sender closed");
        assert_eq!(evt.message.content["text"], "after-error");
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn poll_loop_with_no_rows_idles_and_can_be_cancelled() {
        let d = dir();
        let cfg = {
            let mut c = IMessageConfig::default();
            c.poll_interval_ms = 10;
            c.since_rowid_file = "rowid.txt".into();
            c
        };
        let bridge: Arc<dyn IMessageBridge> = Arc::new(MockBridge::new());
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(poll_loop(
            bridge,
            cfg,
            d.path().to_path_buf(),
            tx,
            cancel.clone(),
        ));
        let r = timeout(Duration::from_millis(80), rx.recv()).await;
        // No events; either timeout (Err) or None.
        match r {
            Err(_) | Ok(None) => {}
            Ok(Some(e)) => panic!("unexpected event {e:?}"),
        }
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn poll_loop_respects_immediate_cancel() {
        let d = dir();
        let cfg = {
            let mut c = IMessageConfig::default();
            c.poll_interval_ms = 10_000; // 10s — would block on sleep
            c.since_rowid_file = "rowid.txt".into();
            c
        };
        let bridge: Arc<dyn IMessageBridge> = Arc::new(MockBridge::new());
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(poll_loop(
            bridge,
            cfg,
            d.path().to_path_buf(),
            tx,
            cancel.clone(),
        ));
        // Cancel almost immediately; the select should pick it.
        cancel.cancel();
        let start = std::time::Instant::now();
        handle.await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
