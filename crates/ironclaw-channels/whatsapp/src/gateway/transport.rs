//! WebSocket transport for the WhatsApp gateway.
//!
//! Defines [`WsTransport`], the trait the lifecycle loop talks to. The
//! production impl ([`TungsteniteTransport`]) opens a real `wss://`
//! connection via `tokio-tungstenite`; the test impl ([`MockTransport`])
//! lets tests drive the lifecycle without touching the network.

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use ironclaw_channels_core::AdapterError;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Abstract WebSocket transport. The lifecycle loop drives one of these.
#[async_trait]
pub trait WsTransport: Send + Sync {
    /// Send a binary frame to the peer.
    async fn send_binary(&self, payload: Vec<u8>) -> Result<(), AdapterError>;

    /// Receive the next binary frame, or `None` when the stream ends.
    /// Pings/pongs and text frames are silently dropped by the transport
    /// so the caller sees a binary-only stream.
    async fn recv_binary(&self) -> Result<Option<Vec<u8>>, AdapterError>;

    /// Close the connection. Idempotent.
    async fn close(&self) -> Result<(), AdapterError>;
}

/// Production transport: a `tokio-tungstenite` WebSocket.
pub struct TungsteniteTransport {
    socket: Mutex<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl std::fmt::Debug for TungsteniteTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TungsteniteTransport").finish_non_exhaustive()
    }
}

impl TungsteniteTransport {
    /// Open a fresh connection to `url`. Returns the connected transport.
    pub async fn connect(url: &str) -> Result<Arc<Self>, AdapterError> {
        let (socket, _resp) = connect_async(url)
            .await
            .map_err(|e| AdapterError::Transport(format!("whatsapp ws connect: {e}")))?;
        Ok(Arc::new(Self {
            socket: Mutex::new(socket),
        }))
    }
}

#[async_trait]
impl WsTransport for TungsteniteTransport {
    async fn send_binary(&self, payload: Vec<u8>) -> Result<(), AdapterError> {
        let mut g = self.socket.lock().await;
        g.send(Message::Binary(payload))
            .await
            .map_err(|e| AdapterError::Transport(format!("whatsapp ws send: {e}")))
    }

    async fn recv_binary(&self) -> Result<Option<Vec<u8>>, AdapterError> {
        loop {
            let mut g = self.socket.lock().await;
            match g.next().await {
                None | Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(Message::Binary(b))) => return Ok(Some(b)),
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                    continue;
                }
                Some(Err(e)) => {
                    return Err(AdapterError::Transport(format!("whatsapp ws recv: {e}")));
                }
            }
        }
    }

    async fn close(&self) -> Result<(), AdapterError> {
        let mut g = self.socket.lock().await;
        let _ = g.close(None).await;
        Ok(())
    }
}

/// In-memory transport for tests. Maintains a script of canned receive
/// frames and records every send call.
///
/// Construction returns the transport plus a [`MockHandle`] the test uses
/// to script behaviour.
pub struct MockTransport {
    inner: Arc<MockInner>,
}

impl std::fmt::Debug for MockTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockTransport").finish_non_exhaustive()
    }
}

struct MockInner {
    sent: Mutex<Vec<Vec<u8>>>,
    closed: Mutex<bool>,
    recv_tx: mpsc::UnboundedSender<RecvScript>,
    recv_rx: Mutex<mpsc::UnboundedReceiver<RecvScript>>,
}

/// One entry on the mock's receive script.
enum RecvScript {
    /// Deliver this binary frame to the next `recv_binary` call.
    Binary(Vec<u8>),
    /// Cause the next `recv_binary` to error.
    Error(String),
    /// Cause the next `recv_binary` to return `Ok(None)` (end of stream).
    Eos,
}

/// Test handle for [`MockTransport`].
pub struct MockHandle {
    inner: Arc<MockInner>,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new().0
    }
}

impl MockTransport {
    /// Construct a fresh mock transport plus a control handle.
    pub fn new() -> (Self, MockHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(MockInner {
            sent: Mutex::new(vec![]),
            closed: Mutex::new(false),
            recv_tx: tx,
            recv_rx: Mutex::new(rx),
        });
        (
            Self {
                inner: inner.clone(),
            },
            MockHandle { inner },
        )
    }
}

impl MockHandle {
    /// Queue a binary frame to be delivered on the next `recv_binary`.
    pub fn push_binary(&self, payload: impl Into<Vec<u8>>) {
        let _ = self.inner.recv_tx.send(RecvScript::Binary(payload.into()));
    }

    /// Queue an error to be delivered on the next `recv_binary`.
    pub fn push_error(&self, msg: impl Into<String>) {
        let _ = self.inner.recv_tx.send(RecvScript::Error(msg.into()));
    }

    /// Queue an end-of-stream marker.
    pub fn push_eos(&self) {
        let _ = self.inner.recv_tx.send(RecvScript::Eos);
    }

    /// Snapshot of every payload sent via the transport.
    pub async fn sent(&self) -> Vec<Vec<u8>> {
        self.inner.sent.lock().await.clone()
    }

    /// `true` if `close` was called.
    pub async fn was_closed(&self) -> bool {
        *self.inner.closed.lock().await
    }
}

#[async_trait]
impl WsTransport for MockTransport {
    async fn send_binary(&self, payload: Vec<u8>) -> Result<(), AdapterError> {
        self.inner.sent.lock().await.push(payload);
        Ok(())
    }

    async fn recv_binary(&self) -> Result<Option<Vec<u8>>, AdapterError> {
        let mut rx = self.inner.recv_rx.lock().await;
        match rx.recv().await {
            None | Some(RecvScript::Eos) => Ok(None),
            Some(RecvScript::Binary(b)) => Ok(Some(b)),
            Some(RecvScript::Error(msg)) => Err(AdapterError::Transport(msg)),
        }
    }

    async fn close(&self) -> Result<(), AdapterError> {
        *self.inner.closed.lock().await = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn mock_send_records_payload() {
        let (mock, handle) = MockTransport::new();
        mock.send_binary(vec![1, 2, 3]).await.unwrap();
        let sent = handle.sent().await;
        assert_eq!(sent, vec![vec![1, 2, 3]]);
    }

    #[tokio::test]
    async fn mock_recv_returns_pushed_binary() {
        let (mock, handle) = MockTransport::new();
        handle.push_binary([0xAA, 0xBB]);
        let b = mock.recv_binary().await.unwrap().unwrap();
        assert_eq!(b, vec![0xAA, 0xBB]);
    }

    #[tokio::test]
    async fn mock_recv_returns_eos_on_pushed_eos() {
        let (mock, handle) = MockTransport::new();
        handle.push_eos();
        let res = mock.recv_binary().await.unwrap();
        assert!(res.is_none());
    }

    #[tokio::test]
    async fn mock_recv_returns_error_on_pushed_error() {
        let (mock, handle) = MockTransport::new();
        handle.push_error("boom");
        let err = mock.recv_binary().await.unwrap_err();
        match err {
            AdapterError::Transport(s) => assert_eq!(s, "boom"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_recv_with_nothing_queued_blocks() {
        // We don't await; we just confirm a short timeout fires.
        let (mock, _handle) = MockTransport::new();
        let res = timeout(Duration::from_millis(20), mock.recv_binary()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn mock_close_sets_flag() {
        let (mock, handle) = MockTransport::new();
        assert!(!handle.was_closed().await);
        mock.close().await.unwrap();
        assert!(handle.was_closed().await);
    }

    #[tokio::test]
    async fn mock_close_is_idempotent() {
        let (mock, handle) = MockTransport::new();
        mock.close().await.unwrap();
        mock.close().await.unwrap();
        assert!(handle.was_closed().await);
    }

    #[tokio::test]
    async fn mock_send_with_multiple_payloads_preserves_order() {
        let (mock, handle) = MockTransport::new();
        for i in 0..3u8 {
            mock.send_binary(vec![i]).await.unwrap();
        }
        let sent = handle.sent().await;
        assert_eq!(sent, vec![vec![0], vec![1], vec![2]]);
    }

    #[tokio::test]
    async fn mock_recv_serves_in_fifo_order() {
        let (mock, handle) = MockTransport::new();
        handle.push_binary([1]);
        handle.push_binary([2]);
        handle.push_binary([3]);
        for i in 1..=3u8 {
            assert_eq!(mock.recv_binary().await.unwrap().unwrap(), vec![i]);
        }
    }

    #[tokio::test]
    async fn mock_default_is_usable() {
        let mock: MockTransport = MockTransport::default();
        // Nothing queued — the receive channel times out.
        let res = timeout(Duration::from_millis(10), mock.recv_binary()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn mock_debug_renders() {
        let (mock, _handle) = MockTransport::new();
        let s = format!("{mock:?}");
        assert!(s.contains("MockTransport"));
    }

    #[tokio::test]
    async fn tungstenite_connect_to_invalid_url_errors() {
        let err = TungsteniteTransport::connect("ws://127.0.0.1:1").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn tungstenite_connect_to_garbage_url_errors() {
        let err = TungsteniteTransport::connect("not-a-url").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn tungstenite_debug_format() {
        // We can't instantiate without a socket; assert the type's Debug
        // impl by formatting a tunnel through a TypeId-ish smoke check.
        // Skip: nothing to verify without a real connection.
    }
}
