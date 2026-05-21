//! Background task driving Telegram's `getUpdates` long-poll endpoint.
//!
//! Spawned by `TelegramAdapter::new` when the configured mode is
//! [`crate::config::IngressMode::LongPoll`]. The task runs until its
//! [`CancellationToken`] is triggered.

use crate::api::TelegramApi;
use crate::config::LongPollConfig;
use crate::ingress::{IngressSettings, updates_to_events};
use ironclaw_channels_core::AdapterError;
use ironclaw_types::InboundEvent;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

/// Initial backoff applied after a transport failure.
pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Maximum backoff between retries on continued failure.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Run the long-poll loop until cancelled.
///
/// The loop:
/// 1. Calls `getUpdates` with the current `offset`.
/// 2. Translates each [`crate::types::Update`] into [`InboundEvent`]s
///    (downloading attachments per `settings`) and pushes them through
///    `inbound_tx`.
/// 3. Advances `offset` to `max(update_id) + 1` so seen updates are not
///    re-served.
/// 4. On transport-level failures, applies exponential backoff capped at
///    [`MAX_BACKOFF`].
pub async fn run_long_poll(
    api: TelegramApi,
    cfg: LongPollConfig,
    settings: IngressSettings,
    inbound_tx: Sender<InboundEvent>,
    cancel: CancellationToken,
) {
    let mut offset: i64 = 0;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if cancel.is_cancelled() {
            tracing::debug!("telegram long-poll cancelled");
            return;
        }

        let allowed = cfg.allowed_updates.clone();
        let poll = api.get_updates(offset, cfg.timeout_secs, cfg.limit, &allowed);

        let updates = tokio::select! {
            () = cancel.cancelled() => {
                tracing::debug!("telegram long-poll cancelled mid-call");
                return;
            }
            res = poll => res,
        };

        match updates {
            Ok(updates) => {
                backoff = INITIAL_BACKOFF;
                for update in &updates {
                    if update.update_id >= offset {
                        offset = update.update_id + 1;
                    }
                    let events = updates_to_events(update, &api, &settings).await;
                    for event in events {
                        if inbound_tx.send(event).await.is_err() {
                            tracing::debug!("telegram inbound_tx closed, stopping poll");
                            return;
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "telegram getUpdates failed");
                if matches!(err, AdapterError::Auth(_)) {
                    // Bot token is bad; do not loop forever.
                    return;
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DEFAULT_MAX_ATTACHMENT_BYTES, LongPollConfig};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration as StdDuration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn lp_cfg() -> LongPollConfig {
        LongPollConfig {
            timeout_secs: 0,
            limit: 100,
            allowed_updates: vec![],
        }
    }

    fn settings(dir: PathBuf, bot: Option<&str>) -> IngressSettings {
        IngressSettings {
            attachment_download: true,
            max_attachment_bytes: DEFAULT_MAX_ATTACHMENT_BYTES,
            bot_username: bot.map(str::to_owned),
            data_dir: dir,
        }
    }

    #[tokio::test]
    async fn pushes_inbound_event_from_update() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": [{
                    "update_id": 1,
                    "message": {
                        "message_id": 9,
                        "date": 1,
                        "chat": { "id": 100, "type": "private" },
                        "text": "hi"
                    }
                }]
            })))
            .mount(&s)
            .await;

        let api = TelegramApi::new(s.uri(), "tok");
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(8);
        let cancel = CancellationToken::new();
        let dir = TempDir::new().unwrap();
        let handle = tokio::spawn(run_long_poll(
            api,
            lp_cfg(),
            settings(dir.path().to_path_buf(), Some("ironbot")),
            tx,
            cancel.clone(),
        ));

        let evt = tokio::time::timeout(StdDuration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.platform_id, "100");
        assert_eq!(evt.message.content["text"], "hi");

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn offset_advances_past_seen_updates() {
        let s = MockServer::start().await;
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = call_count.clone();
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(move |_req: &Request| {
                let n = cc.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "ok": true,
                        "result": [{
                            "update_id": 5,
                            "message": {
                                "message_id": 1, "date": 1,
                                "chat": { "id": 1, "type": "private" },
                                "text": "a"
                            }
                        }]
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []}))
                }
            })
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(4);
        let cancel = CancellationToken::new();
        let dir = TempDir::new().unwrap();
        let handle = tokio::spawn(run_long_poll(
            api,
            lp_cfg(),
            settings(dir.path().to_path_buf(), None),
            tx,
            cancel.clone(),
        ));

        let _ = tokio::time::timeout(StdDuration::from_secs(2), rx.recv())
            .await
            .unwrap();
        // Give the loop time to make at least one more call.
        tokio::time::sleep(StdDuration::from_millis(150)).await;
        cancel.cancel();
        handle.await.unwrap();

        assert!(call_count.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn cancellation_stops_loop() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let dir = TempDir::new().unwrap();
        let handle = tokio::spawn(run_long_poll(
            api,
            lp_cfg(),
            settings(dir.path().to_path_buf(), None),
            tx,
            cancel.clone(),
        ));

        cancel.cancel();
        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn auth_error_stops_loop() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "ok": false, "error_code": 401, "description": "Unauthorized"
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let dir = TempDir::new().unwrap();
        let handle = tokio::spawn(run_long_poll(
            api,
            lp_cfg(),
            settings(dir.path().to_path_buf(), None),
            tx,
            cancel.clone(),
        ));

        // No need to cancel; the task should exit on its own due to auth error.
        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn receiver_closed_stops_loop() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": [{
                    "update_id": 1,
                    "message": {
                        "message_id": 1, "date": 1,
                        "chat": { "id": 1, "type": "private" },
                        "text": "x"
                    }
                }]
            })))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let (tx, rx) = mpsc::channel::<InboundEvent>(1);
        drop(rx);
        let cancel = CancellationToken::new();
        let dir = TempDir::new().unwrap();
        let handle = tokio::spawn(run_long_poll(
            api,
            lp_cfg(),
            settings(dir.path().to_path_buf(), None),
            tx,
            cancel.clone(),
        ));

        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn transport_error_applies_backoff_then_exits_on_cancel() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream"))
            .mount(&s)
            .await;
        let api = TelegramApi::new(s.uri(), "tok");
        let (tx, _rx) = mpsc::channel::<InboundEvent>(1);
        let cancel = CancellationToken::new();
        let dir = TempDir::new().unwrap();
        let handle = tokio::spawn(run_long_poll(
            api,
            lp_cfg(),
            settings(dir.path().to_path_buf(), None),
            tx,
            cancel.clone(),
        ));
        // Let one failure occur and the loop enter backoff.
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        cancel.cancel();
        tokio::time::timeout(StdDuration::from_secs(2), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn document_update_is_downloaded_and_pushed() {
        let s = MockServer::start().await;
        // First getUpdates returns a document; subsequent calls return empty.
        let counter = Arc::new(AtomicUsize::new(0));
        let cc = counter.clone();
        Mock::given(method("POST"))
            .and(path("/bottok/getUpdates"))
            .respond_with(move |_req: &Request| {
                let n = cc.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "ok": true,
                        "result": [{
                            "update_id": 1,
                            "message": {
                                "message_id": 7, "date": 1,
                                "chat": { "id": 100, "type": "private" },
                                "document": {
                                    "file_id": "F", "file_unique_id": "U",
                                    "file_name": "a.txt", "mime_type": "text/plain",
                                    "file_size": 5
                                }
                            }
                        }]
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []}))
                }
            })
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/bottok/getFile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": {
                    "file_id": "F", "file_unique_id": "U",
                    "file_size": 5, "file_path": "documents/a.txt"
                }
            })))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/file/bottok/documents/a.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&s)
            .await;

        let api = TelegramApi::new(s.uri(), "tok");
        let (tx, mut rx) = mpsc::channel::<InboundEvent>(4);
        let cancel = CancellationToken::new();
        let dir = TempDir::new().unwrap();
        let handle = tokio::spawn(run_long_poll(
            api,
            lp_cfg(),
            settings(dir.path().to_path_buf(), None),
            tx,
            cancel.clone(),
        ));

        let evt = tokio::time::timeout(StdDuration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(evt.message.kind, ironclaw_types::MessageKind::Chat);
        let att = &evt.message.content["attachment"];
        assert_eq!(att["kind"], "telegram.document");
        let on_disk = att["path"].as_str().unwrap();
        assert_eq!(std::fs::read(on_disk).unwrap(), b"hello");

        cancel.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn backoff_constants_are_reasonable() {
        assert!(INITIAL_BACKOFF >= Duration::from_secs(1));
        assert!(MAX_BACKOFF >= INITIAL_BACKOFF);
    }
}
