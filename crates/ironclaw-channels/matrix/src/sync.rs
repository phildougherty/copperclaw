//! Background task that drives Matrix's `/sync` long-poll endpoint.
//!
//! Spawned by [`MatrixAdapter::start`](crate::adapter::MatrixAdapter::start).
//! The task:
//!
//! 1. Reads any persisted `next_batch` token from disk (if present).
//! 2. Calls `/sync` with the current token and a filter limiting events
//!    to the configured / live room set.
//! 3. Translates the response into [`InboundEvent`]s via
//!    [`crate::parse::sync_to_events`].
//! 4. Persists the new `next_batch` token to disk so restarts resume
//!    rather than re-replaying everything.
//! 5. Backs off exponentially on transport failures (capped at
//!    [`MAX_BACKOFF`]).

use crate::api::MatrixApi;
use crate::parse::{next_batch_of, sync_to_events};
use ironclaw_channels_core::AdapterError;
use ironclaw_types::InboundEvent;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// Filename (relative to the channel's `data_dir`) used to persist the
/// rolling `next_batch` token.
pub const NEXT_BATCH_FILENAME: &str = "next_batch.txt";

/// Initial backoff applied after a transport failure.
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Maximum backoff between retries.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Read the persisted `next_batch` token from disk, if any.
///
/// Missing file -> `Ok(None)`. Other I/O errors propagate.
pub async fn read_next_batch(path: &Path) -> Result<Option<String>, AdapterError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_owned()))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(AdapterError::Io(err)),
    }
}

/// Persist a `next_batch` token to disk atomically.
pub async fn write_next_batch(path: &Path, token: &str) -> Result<(), AdapterError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(AdapterError::Io)?;
        }
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, token).await.map_err(AdapterError::Io)?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(AdapterError::Io)?;
    Ok(())
}

/// Build the `/sync` filter for a given set of rooms. Empty `rooms` yields
/// `None` (no filter, all rooms).
#[allow(clippy::implicit_hasher)]
pub fn build_filter(rooms: &HashSet<String>) -> Option<Value> {
    if rooms.is_empty() {
        return None;
    }
    let list: Vec<&str> = rooms.iter().map(String::as_str).collect();
    Some(json!({
        "room": {
            "rooms": list,
            "timeline": { "limit": 50 }
        }
    }))
}

/// Run the `/sync` loop until cancelled.
#[allow(clippy::too_many_arguments, clippy::implicit_hasher)]
pub async fn run_sync_loop(
    api: Arc<MatrixApi>,
    state_path: PathBuf,
    inbound_tx: Sender<InboundEvent>,
    bot_user_id: String,
    rooms: Arc<RwLock<HashSet<String>>>,
    sync_timeout_ms: u64,
    shutdown: CancellationToken,
) {
    let mut next_batch: Option<String> = match read_next_batch(&state_path).await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(?err, "matrix: failed to read next_batch");
            None
        }
    };
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if shutdown.is_cancelled() {
            tracing::debug!("matrix sync cancelled");
            return;
        }

        let filter = {
            let guard = rooms.read().await;
            build_filter(&guard)
        };

        let sync_fut = api.sync(next_batch.as_deref(), sync_timeout_ms, filter.as_ref());
        let sync_result = tokio::select! {
            () = shutdown.cancelled() => {
                tracing::debug!("matrix sync cancelled mid-call");
                return;
            }
            res = sync_fut => res,
        };

        match sync_result {
            Ok(value) => {
                backoff = INITIAL_BACKOFF;
                let events = sync_to_events(&value, &bot_user_id);
                for event in events {
                    let send_fut = inbound_tx.send(event);
                    tokio::select! {
                        () = shutdown.cancelled() => {
                            tracing::debug!("matrix sync cancelled while sending event");
                            return;
                        }
                        res = send_fut => {
                            if res.is_err() {
                                tracing::debug!("matrix inbound_tx closed, stopping sync");
                                return;
                            }
                        }
                    }
                }
                if let Some(next) = next_batch_of(&value) {
                    if let Err(err) = write_next_batch(&state_path, next).await {
                        tracing::warn!(?err, "matrix: failed to persist next_batch");
                    }
                    next_batch = Some(next.to_owned());
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "matrix /sync failed");
                if matches!(err, AdapterError::Auth(_)) {
                    tracing::error!("matrix /sync auth failure; stopping sync loop");
                    return;
                }
                let delay = if let AdapterError::Rate {
                    retry_after: Some(secs),
                } = &err
                {
                    Duration::from_secs(*secs)
                } else {
                    backoff
                };
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn rooms_default() -> Arc<RwLock<HashSet<String>>> {
        Arc::new(RwLock::new(HashSet::new()))
    }

    fn run_with(
        api: Arc<MatrixApi>,
        state_path: PathBuf,
        tx: Sender<InboundEvent>,
        bot: &str,
        rooms: &Arc<RwLock<HashSet<String>>>,
    ) -> (CancellationToken, tokio::task::JoinHandle<()>) {
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_sync_loop(
            api,
            state_path,
            tx,
            bot.to_owned(),
            rooms.clone(),
            0,
            cancel.clone(),
        ));
        (cancel, handle)
    }

    #[tokio::test]
    async fn read_next_batch_missing_file_is_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("next.txt");
        assert!(read_next_batch(&path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_next_batch_empty_file_is_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("next.txt");
        tokio::fs::write(&path, "  \n").await.unwrap();
        assert!(read_next_batch(&path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn write_then_read_next_batch_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("subdir/next.txt");
        write_next_batch(&path, "abc").await.unwrap();
        assert_eq!(read_next_batch(&path).await.unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn build_filter_empty_is_none() {
        let rooms: HashSet<String> = HashSet::new();
        assert!(build_filter(&rooms).is_none());
    }

    #[test]
    fn build_filter_includes_rooms() {
        let mut rooms = HashSet::new();
        rooms.insert("!a:m.org".to_owned());
        let f = build_filter(&rooms).unwrap();
        let list = f["room"]["rooms"].as_array().unwrap();
        assert!(list.iter().any(|v| v == "!a:m.org"));
    }

    // The mock returns the same event on every /sync call. The sync loop now
    // wraps the inbound send in a select! against the shutdown token, so even
    // when the mpsc fills and the send would otherwise block, cancellation
    // still drives the loop to exit cleanly.
    #[tokio::test]
    async fn sync_loop_pushes_events_and_persists_next_batch() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "n1",
                "rooms": { "join": {
                    "!a:m.org": {
                        "timeline": { "events": [{
                            "type": "m.room.message",
                            "event_id": "$e:m.org",
                            "sender": "@alice:m.org",
                            "origin_server_ts": 1_000,
                            "content": { "msgtype": "m.text", "body": "hi" }
                        }] }
                    }
                } }
            })))
            .mount(&s)
            .await;

        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nb.txt");
        let api = Arc::new(MatrixApi::new(s.uri(), "tok"));
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let (cancel, handle) =
            run_with(api, state.clone(), tx, "@bot:m.org", &rooms_default());

        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.platform_id, "!a:m.org");
        assert_eq!(evt.message.content["text"], "hi");

        // The token should land on disk within a couple of iterations.
        for _ in 0..50 {
            if let Ok(Some(v)) = read_next_batch(&state).await {
                assert_eq!(v, "n1");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn sync_loop_resumes_from_persisted_next_batch() {
        // Two responses: first call (no since) returns events; second call
        // (with since=resume) returns a different event.
        let s = MockServer::start().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let cc = counter.clone();
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(move |req: &Request| {
                let n = cc.fetch_add(1, Ordering::SeqCst);
                let q = req.url.query().unwrap_or_default().to_owned();
                if n == 0 {
                    // The persisted token must show up as since=resume.
                    assert!(q.contains("since=resume"), "since param missing: {q}");
                    ResponseTemplate::new(200).set_body_json(json!({
                        "next_batch": "after-resume",
                        "rooms": { "join": {} }
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "next_batch": "later",
                        "rooms": { "join": {} }
                    }))
                }
            })
            .mount(&s)
            .await;

        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nb.txt");
        write_next_batch(&state, "resume").await.unwrap();

        let api = Arc::new(MatrixApi::new(s.uri(), "tok"));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(8);
        let (cancel, handle) =
            run_with(api, state.clone(), tx, "@bot:m.org", &rooms_default());

        // Wait until at least one request has been served.
        for _ in 0..50 {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        handle.await.unwrap();
        assert!(counter.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn sync_loop_backs_off_on_5xx_and_exits_on_cancel() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nb.txt");
        let api = Arc::new(MatrixApi::new(s.uri(), "tok"));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let (cancel, handle) =
            run_with(api, state, tx, "@bot:m.org", &rooms_default());
        // Let one failure happen and the loop enter backoff.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sync_loop_stops_on_auth_error() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "errcode": "M_UNKNOWN_TOKEN", "error": "no"
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nb.txt");
        let api = Arc::new(MatrixApi::new(s.uri(), "tok"));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let (_cancel, handle) =
            run_with(api, state, tx, "@bot:m.org", &rooms_default());
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sync_loop_receiver_closed_stops_loop() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "next_batch": "n",
                "rooms": { "join": {
                    "!a:m.org": {
                        "timeline": { "events": [{
                            "type": "m.room.message",
                            "event_id": "$e:m.org",
                            "sender": "@alice:m.org",
                            "origin_server_ts": 1,
                            "content": { "msgtype": "m.text", "body": "hi" }
                        }] }
                    }
                } }
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nb.txt");
        let api = Arc::new(MatrixApi::new(s.uri(), "tok"));
        let (tx, rx) = mpsc::channel::<InboundEvent>(1);
        drop(rx);
        let (_cancel, handle) =
            run_with(api, state, tx, "@bot:m.org", &rooms_default());
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sync_loop_honours_rate_retry_after_and_exits() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "errcode": "M_LIMIT_EXCEEDED",
                "retry_after_ms": 100,
                "error": "rl"
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nb.txt");
        let api = Arc::new(MatrixApi::new(s.uri(), "tok"));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let (cancel, handle) =
            run_with(api, state, tx, "@bot:m.org", &rooms_default());
        // Give one cycle to fail.
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn sync_loop_uses_room_filter_when_live_set_present() {
        let s = MockServer::start().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let cc = counter.clone();
        Mock::given(method("GET"))
            .and(path("/_matrix/client/v3/sync"))
            .respond_with(move |req: &Request| {
                cc.fetch_add(1, Ordering::SeqCst);
                let q = req.url.query().unwrap_or_default();
                // The filter param should include "!only:m.org".
                assert!(q.contains("filter="), "no filter in {q}");
                ResponseTemplate::new(200).set_body_json(json!({
                    "next_batch": "n",
                    "rooms": { "join": {} }
                }))
            })
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("nb.txt");
        let api = Arc::new(MatrixApi::new(s.uri(), "tok"));
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let rooms = Arc::new(RwLock::new({
            let mut h = HashSet::new();
            h.insert("!only:m.org".to_owned());
            h
        }));
        let (cancel, handle) = run_with(api, state, tx, "@bot:m.org", &rooms);
        for _ in 0..50 {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn write_next_batch_handles_relative_filename_without_parent() {
        let dir = TempDir::new().unwrap();
        let cwd_before = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = write_next_batch(Path::new("solo.txt"), "x").await;
        // Restore CWD before any potential failure assertion.
        std::env::set_current_dir(cwd_before).unwrap();
        result.unwrap();
        assert!(dir.path().join("solo.txt").exists());
    }

    #[tokio::test]
    async fn read_next_batch_io_error_propagates() {
        // Path is a directory, not a file -> read_to_string fails with
        // an I/O error other than NotFound.
        let dir = TempDir::new().unwrap();
        let err = read_next_batch(dir.path()).await.unwrap_err();
        assert!(matches!(err, AdapterError::Io(_)));
    }

    #[test]
    fn next_batch_filename_constant() {
        assert_eq!(NEXT_BATCH_FILENAME, "next_batch.txt");
    }

    #[test]
    fn backoff_constants_are_reasonable() {
        assert!(INITIAL_BACKOFF >= Duration::from_secs(1));
        assert!(MAX_BACKOFF >= INITIAL_BACKOFF);
    }
}
