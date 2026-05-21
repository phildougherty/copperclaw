//! Background task that polls `/2/dm_events` for new DMs.
//!
//! The X v2 streaming endpoint (`/2/dm_events/stream`) is not generally
//! available, so we settle for polling. The loop:
//!
//! 1. Reads the persisted `since_id` token from disk (if present).
//! 2. Calls `dm_events_page(since_id)` once per `poll_interval_ms`.
//! 3. Translates results into [`InboundEvent`]s and pushes them on the
//!    inbound channel.
//! 4. Persists the page's `meta.newest_id` so restarts resume.
//! 5. Backs off on transport / rate failures.

use crate::api::XApi;
use crate::parse::{newest_id_of, page_to_events};
use ironclaw_channels_core::AdapterError;
use ironclaw_types::InboundEvent;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// Default page size requested per poll.
pub const PAGE_SIZE: u32 = 100;

/// Initial backoff applied after a transport-class failure.
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Maximum backoff between retries (after exponential growth).
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Read the persisted `since_id` token from disk. Missing file -> `None`.
pub async fn read_since_id(path: &Path) -> Result<Option<String>, AdapterError> {
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

/// Persist a `since_id` token to disk atomically via `.tmp` + rename.
pub async fn write_since_id(path: &Path, token: &str) -> Result<(), AdapterError> {
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

/// Run the polling loop until cancelled or the inbound channel is closed.
pub async fn run_poll_loop(
    api: Arc<XApi>,
    state_path: PathBuf,
    inbound_tx: Sender<InboundEvent>,
    bot_user_id: String,
    poll_interval_ms: u64,
    shutdown: CancellationToken,
) {
    let mut since_id = match read_since_id(&state_path).await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(?err, "x: failed to read since_id");
            None
        }
    };
    let mut backoff = INITIAL_BACKOFF;
    let poll_interval = Duration::from_millis(poll_interval_ms);

    loop {
        if shutdown.is_cancelled() {
            tracing::debug!("x poll cancelled");
            return;
        }

        let fetch_fut = api.dm_events_page(since_id.as_deref(), PAGE_SIZE);
        let result = tokio::select! {
            () = shutdown.cancelled() => {
                tracing::debug!("x poll cancelled mid-call");
                return;
            }
            res = fetch_fut => res,
        };

        match result {
            Ok(page) => {
                backoff = INITIAL_BACKOFF;
                let events = page_to_events(&page, &bot_user_id);
                for event in events {
                    let send_fut = inbound_tx.send(event);
                    let send_res = tokio::select! {
                        () = shutdown.cancelled() => {
                            tracing::debug!("x poll cancelled while sending");
                            return;
                        }
                        res = send_fut => res,
                    };
                    if send_res.is_err() {
                        tracing::debug!("x inbound_tx closed, stopping poll");
                        return;
                    }
                }
                if let Some(newest) = newest_id_of(&page) {
                    if let Err(err) = write_since_id(&state_path, newest).await {
                        tracing::warn!(?err, "x: failed to persist since_id");
                    }
                    since_id = Some(newest.to_owned());
                }
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(poll_interval) => {}
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "x dm_events poll failed");
                if matches!(err, AdapterError::Auth(_)) {
                    tracing::error!("x dm_events auth failure; stopping poll loop");
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn api(server_url: &str) -> Arc<XApi> {
        Arc::new(XApi::with_client(
            reqwest::Client::new(),
            server_url,
            server_url,
            "tok",
        ))
    }

    fn empty_page() -> serde_json::Value {
        json!({ "data": [], "meta": {} })
    }

    #[tokio::test]
    async fn read_since_id_missing_is_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sid.txt");
        assert!(read_since_id(&path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_since_id_empty_is_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sid.txt");
        tokio::fs::write(&path, "  \n").await.unwrap();
        assert!(read_since_id(&path).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn write_then_read_since_id_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sub/sid.txt");
        write_since_id(&path, "abc").await.unwrap();
        assert_eq!(read_since_id(&path).await.unwrap().as_deref(), Some("abc"));
    }

    #[tokio::test]
    async fn write_since_id_handles_relative_filename_without_parent() {
        let dir = TempDir::new().unwrap();
        let cwd_before = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = write_since_id(Path::new("solo.txt"), "x").await;
        std::env::set_current_dir(cwd_before).unwrap();
        result.unwrap();
        assert!(dir.path().join("solo.txt").exists());
    }

    #[tokio::test]
    async fn read_since_id_io_error_propagates() {
        let dir = TempDir::new().unwrap();
        // Reading a directory as a file returns an io error other than NotFound.
        let err = read_since_id(dir.path()).await.unwrap_err();
        assert!(matches!(err, AdapterError::Io(_)));
    }

    #[tokio::test]
    async fn poll_loop_pushes_events_and_persists_newest_id() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "id": "e1",
                    "event_type": "MessageCreate",
                    "text": "hi",
                    "sender_id": "u1",
                    "dm_conversation_id": "c1",
                    "created_at": "2024-01-01T00:00:00Z"
                }],
                "meta": { "newest_id": "e1" }
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("sid.txt");
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(4);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poll_loop(
            api(&s.uri()),
            state.clone(),
            tx,
            "bot".into(),
            5,
            cancel.clone(),
        ));

        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.platform_id, "conversation:c1");
        assert_eq!(evt.message.content["text"], "hi");

        for _ in 0..50 {
            if let Ok(Some(v)) = read_since_id(&state).await {
                assert_eq!(v, "e1");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn poll_loop_resumes_from_persisted_since_id() {
        let s = MockServer::start().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let cc = counter.clone();
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .and(query_param("since_id", "resume-me"))
            .respond_with(move |_req: &Request| {
                cc.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "data": [],
                    "meta": {}
                }))
            })
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("sid.txt");
        write_since_id(&state, "resume-me").await.unwrap();
        let (tx, _rx) = mpsc::channel::<InboundEvent>(4);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poll_loop(
            api(&s.uri()),
            state,
            tx,
            "bot".into(),
            5,
            cancel.clone(),
        ));
        for _ in 0..50 {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(counter.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn poll_loop_backs_off_on_5xx_and_exits_on_cancel() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("sid.txt");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poll_loop(
            api(&s.uri()),
            state,
            tx,
            "bot".into(),
            5,
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn poll_loop_stops_on_auth_error() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "errors": [{"message": "bad"}]
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("sid.txt");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poll_loop(
            api(&s.uri()),
            state,
            tx,
            "bot".into(),
            5,
            cancel,
        ));
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn poll_loop_honours_rate_retry_after_and_exits() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "1")
                    .set_body_string(""),
            )
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("sid.txt");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poll_loop(
            api(&s.uri()),
            state,
            tx,
            "bot".into(),
            5,
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn poll_loop_receiver_closed_stops_loop() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "id": "e1",
                    "event_type": "MessageCreate",
                    "text": "hi",
                    "sender_id": "u1",
                    "dm_conversation_id": "c1",
                    "created_at": "2024-01-01T00:00:00Z"
                }],
                "meta": { "newest_id": "e1" }
            })))
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("sid.txt");
        let (tx, rx) = mpsc::channel::<InboundEvent>(1);
        drop(rx);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poll_loop(
            api(&s.uri()),
            state,
            tx,
            "bot".into(),
            5,
            cancel,
        ));
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn poll_loop_starts_with_no_persisted_since_id() {
        let s = MockServer::start().await;
        let counter = Arc::new(AtomicUsize::new(0));
        let cc = counter.clone();
        Mock::given(method("GET"))
            .and(path("/2/dm_events"))
            .respond_with(move |req: &Request| {
                cc.fetch_add(1, Ordering::SeqCst);
                let q = req.url.query().unwrap_or_default();
                // No persisted since_id at startup -> the param must be absent.
                assert!(!q.contains("since_id="), "unexpected since_id: {q}");
                ResponseTemplate::new(200).set_body_json(empty_page())
            })
            .mount(&s)
            .await;
        let dir = TempDir::new().unwrap();
        let state = dir.path().join("sid.txt");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poll_loop(
            api(&s.uri()),
            state,
            tx,
            "bot".into(),
            5,
            cancel.clone(),
        ));
        for _ in 0..50 {
            if counter.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[test]
    fn poll_constants_are_reasonable() {
        assert_eq!(PAGE_SIZE, 100);
        assert!(INITIAL_BACKOFF >= Duration::from_secs(1));
        assert!(MAX_BACKOFF >= INITIAL_BACKOFF);
    }
}
