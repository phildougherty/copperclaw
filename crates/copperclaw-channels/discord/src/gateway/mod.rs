//! Discord gateway client: WebSocket plumbing plus pure protocol helpers.
//!
//! - [`codec`] — frame parsing and outgoing payload builders.
//! - [`lifecycle`] — session state, heartbeat timing, reconnect backoff.
//! - [`GatewayClient`] — thin connector that opens the socket and exposes
//!   the read/write split. The reconnect/heartbeat loops live in
//!   `adapter.rs` where they can be driven by tests via injected futures.

pub mod codec;
pub mod lifecycle;

use futures::{SinkExt, StreamExt};
use copperclaw_channels_core::AdapterError;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// A connected gateway socket. The adapter splits it into read/write halves
/// using the underlying `StreamExt`/`SinkExt` impls.
pub type GatewaySocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connect to the Discord gateway at the given URL.
pub async fn connect(url: &str) -> Result<GatewaySocket, AdapterError> {
    let (socket, _resp) = connect_async(url)
        .await
        .map_err(|e| AdapterError::Transport(format!("gateway connect: {e}")))?;
    Ok(socket)
}

/// Serialize a JSON value and push it down the WebSocket as a text frame.
pub async fn send_json(socket: &mut GatewaySocket, payload: &Value) -> Result<(), AdapterError> {
    let text = payload.to_string();
    socket
        .send(Message::Text(text))
        .await
        .map_err(|e| AdapterError::Transport(format!("gateway send: {e}")))
}

/// Outcome of a `recv` call.
#[derive(Debug)]
pub enum Frame {
    /// A text frame ready to be parsed by [`codec::parse_frame`].
    Text(String),
    /// The peer sent a `Close` with this code (`None` if unframed).
    Closed(Option<u16>),
}

/// Pull the next text frame off the gateway, ignoring pings/pongs. Returns
/// `Frame::Closed` when the stream ends.
pub async fn recv_text(socket: &mut GatewaySocket) -> Result<Frame, AdapterError> {
    loop {
        match socket.next().await {
            None => return Ok(Frame::Closed(None)),
            Some(Ok(Message::Text(t))) => return Ok(Frame::Text(t)),
            Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {
                continue;
            }
            Some(Ok(Message::Close(cf))) => {
                let code = cf.map(|c| u16::from(c.code));
                return Ok(Frame::Closed(code));
            }
            Some(Err(e)) => {
                return Err(AdapterError::Transport(format!("gateway recv: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_to_invalid_url_errors() {
        let err = connect("ws://127.0.0.1:1").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }

    #[tokio::test]
    async fn connect_to_bad_scheme_errors() {
        let err = connect("not-a-url").await.unwrap_err();
        assert!(matches!(err, AdapterError::Transport(_)));
    }
}
