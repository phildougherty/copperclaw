//! Public test-support exports.
//!
//! Downstream crates can use these to drive integration tests against
//! the WhatsApp adapter without depending on a real WebSocket or a real
//! crypto backend.
//!
//! - [`MockTransport`] / [`MockHandle`] — fake the WebSocket transport.
//! - [`StubBackend`] — the no-op `CryptoBackend` the adapter installs by
//!   default.
//! - [`build_iq_ping_frame`] — fixture builder for a `<iq type="get"/>`
//!   ping frame, useful as a synthetic input on top of `MockTransport`.
//! - [`run_gateway_for_test`] — convenience wrapper around
//!   [`GatewayRunner::run_once`] for tests.

use std::sync::Arc;

use ironclaw_channels_core::AdapterError;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use crate::crypto::StubBackend;
pub use crate::gateway::lifecycle::{
    GatewayRunner, LifecycleEvent, RunOnceOutcome, next_backoff,
};
pub use crate::gateway::transport::{MockHandle, MockTransport, WsTransport};
pub use crate::wire::binary_xml::{Node, NodeContent, NodeString, Tag, token_index};

/// Build a single `<iq type="get"><ping/></iq>` frame with the given id.
/// Useful as a fixture input for end-to-end tests.
pub fn build_iq_ping_frame(id: &str) -> Vec<u8> {
    let node = Node {
        tag: Tag::named("iq"),
        attributes: vec![
            (
                NodeString::Indexed(token_index("id").unwrap()),
                NodeString::from_literal(id),
            ),
            (
                NodeString::Indexed(token_index("type").unwrap()),
                NodeString::Indexed(token_index("get").unwrap()),
            ),
        ],
        content: NodeContent::Children(vec![Node::empty(Tag::named("ping"))]),
    };
    crate::gateway::codec::encode_outbound(&node)
        .expect("test fixture: iq ping must encode")
}

/// Build a `<iq type="result"><pong/></iq>` frame.
pub fn build_iq_pong_frame(id: &str) -> Vec<u8> {
    let node = Node {
        tag: Tag::named("iq"),
        attributes: vec![
            (
                NodeString::Indexed(token_index("id").unwrap()),
                NodeString::from_literal(id),
            ),
            (
                NodeString::Indexed(token_index("type").unwrap()),
                NodeString::Indexed(token_index("result").unwrap()),
            ),
        ],
        content: NodeContent::Children(vec![Node::empty(Tag::named("pong"))]),
    };
    crate::gateway::codec::encode_outbound(&node)
        .expect("test fixture: iq pong must encode")
}

/// Drive one connection cycle of the [`GatewayRunner`] against `transport`.
/// Returns once the runner exits (via EoS, cancel, or error).
pub async fn run_gateway_for_test<T: WsTransport + ?Sized>(
    runner: &GatewayRunner,
    transport: Arc<T>,
    events: &mpsc::Sender<LifecycleEvent>,
    cancel: &CancellationToken,
) -> RunOnceOutcome {
    runner.run_once(transport, events, cancel).await
}

/// Convenience: build a [`MockTransport`] that delivers a single ping
/// frame and then closes. Returns the transport and the bytes pushed.
pub fn mock_with_single_ping(id: &str) -> (MockTransport, MockHandle, Vec<u8>) {
    let (mock, handle) = MockTransport::new();
    let bytes = build_iq_ping_frame(id);
    handle.push_binary(bytes.clone());
    handle.push_eos();
    (mock, handle, bytes)
}

/// Convenience: turn a `Result<Vec<u8>, AdapterError>` into an
/// `AdapterError::Transport` containing a hint about test framing.
pub fn transport_err(msg: &str) -> AdapterError {
    AdapterError::Transport(format!("test: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::codec::decode_inbound;

    #[test]
    fn build_iq_ping_decodes_to_iq_get() {
        let bytes = build_iq_ping_frame("abc");
        let (frame, used) = decode_inbound(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        match frame {
            crate::gateway::codec::InboundFrame::Iq { id, kind, body } => {
                assert_eq!(id, "abc");
                assert_eq!(kind, "get");
                assert_eq!(body.unwrap().tag.as_str(), Some("ping"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn build_iq_pong_decodes_to_iq_result() {
        let bytes = build_iq_pong_frame("def");
        let (frame, _) = decode_inbound(&bytes).unwrap();
        match frame {
            crate::gateway::codec::InboundFrame::Iq { id, kind, body } => {
                assert_eq!(id, "def");
                assert_eq!(kind, "result");
                assert_eq!(body.unwrap().tag.as_str(), Some("pong"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_with_single_ping_returns_a_consumable_transport() {
        let (mock, _handle, bytes) = mock_with_single_ping("xyz");
        let recv1 = mock.recv_binary().await.unwrap().unwrap();
        assert_eq!(recv1, bytes);
        let recv2 = mock.recv_binary().await.unwrap();
        assert!(recv2.is_none(), "second recv should yield eos");
    }

    #[tokio::test]
    async fn run_gateway_for_test_returns_outcome() {
        let runner = GatewayRunner::with_timings(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(10),
            None,
        );
        let (mock, _handle, _bytes) = mock_with_single_ping("123");
        let arc = Arc::new(mock);
        let (tx, _rx) = mpsc::channel::<LifecycleEvent>(8);
        let cancel = CancellationToken::new();
        let outcome = run_gateway_for_test(&runner, arc, &tx, &cancel).await;
        assert!(matches!(outcome, RunOnceOutcome::Disconnected));
    }

    #[test]
    fn transport_err_helper_prefixes_with_test() {
        match transport_err("hello") {
            AdapterError::Transport(s) => assert!(s.starts_with("test:")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn next_backoff_is_re_exported() {
        let _: std::time::Duration = next_backoff(0);
    }
}
