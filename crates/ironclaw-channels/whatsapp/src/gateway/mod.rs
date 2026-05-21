//! Gateway plumbing for the WhatsApp WebSocket.
//!
//! - [`transport`] — `WsTransport` trait, tokio-tungstenite real impl,
//!   and a `MockTransport` for tests.
//! - [`lifecycle`] — pure connect/heartbeat/reconnect timing helpers.
//! - [`codec`] — frame-to-event mapping.

pub mod codec;
pub mod lifecycle;
pub mod transport;
