//! Pure helpers that take raw WebSocket frames apart and re-emit them as
//! higher-level events.
//!
//! Once a frame has been pulled off the transport, it goes through:
//!
//! 1. [`crate::wire::frame::decode`] — strip the `[flags][u24][payload]`
//!    header.
//! 2. If the payload is binary-XML, [`crate::wire::binary_xml::decode`]
//!    parses it into a `Node` tree.
//! 3. [`classify_node`] turns the tree into one of [`InboundFrame`]'s
//!    variants.
//!
//! The codec is **plain, no async, no I/O** so it can be tested with
//! fixture bytes.

use crate::wire::binary_xml::{self, Node, NodeContent, NodeString, Tag};
use crate::wire::frame::{self, Frame, FrameError};

/// One inbound frame after decode + classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundFrame {
    /// A `<iq type="result"/>` / `<iq type="get"/>` frame. `id` is the
    /// `id="..."` attribute and `kind` is the literal type string.
    Iq {
        id: String,
        kind: String,
        body: Option<Node>,
    },
    /// A `<message/>` element. We surface the entire node so callers can
    /// route on the structure they care about.
    Message(Node),
    /// A `<presence/>` element.
    Presence(Node),
    /// A `<receipt/>` element.
    Receipt(Node),
    /// Anything else: surfaced verbatim. Caller decides what to do.
    Other(Node),
}

/// Errors raised by the codec.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    /// The outer `[flags][u24][payload]` framing was malformed.
    #[error("frame: {0}")]
    Frame(#[from] FrameError),
    /// The binary XML decoder rejected the payload.
    #[error("xml: {0}")]
    Xml(#[from] binary_xml::XmlError),
    /// The frame's flags indicate the payload is encrypted, but no
    /// crypto backend has been wired up.
    #[error("encrypted payload requires a real crypto backend")]
    EncryptedNotSupported,
    /// The frame's flags indicate the payload is gzip-compressed; we do
    /// not (yet) implement gzip.
    #[error("compressed payload not yet supported")]
    CompressedNotSupported,
}

/// Decode one WhatsApp frame end-to-end: strip the framing, parse the
/// binary XML, classify the result.
pub fn decode_inbound(bytes: &[u8]) -> Result<(InboundFrame, usize), CodecError> {
    let (frame, used) = frame::decode(bytes)?;
    if frame.flags.is_encrypted() {
        return Err(CodecError::EncryptedNotSupported);
    }
    if frame.flags.is_compressed() {
        return Err(CodecError::CompressedNotSupported);
    }
    let (node, _xml_used) = binary_xml::decode(&frame.payload)?;
    Ok((classify_node(node), used))
}

/// Encode an outbound binary-XML node into the wire framing. This is
/// the symmetric helper to [`decode_inbound`] for tests and the
/// (eventual) outbound path.
pub fn encode_outbound(node: &Node) -> Result<Vec<u8>, CodecError> {
    let payload = binary_xml::encode(node)?;
    let frame = Frame::plain(payload);
    frame::encode(&frame).map_err(CodecError::from)
}

/// Classify a parsed XML node into an [`InboundFrame`].
pub fn classify_node(node: Node) -> InboundFrame {
    let name = node.tag.as_str().map(str::to_owned);
    match name.as_deref() {
        Some("iq") => {
            let id = node
                .attributes
                .iter()
                .find(|(k, _)| node_string_eq(k, "id"))
                .and_then(|(_, v)| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let kind = node
                .attributes
                .iter()
                .find(|(k, _)| node_string_eq(k, "type"))
                .and_then(|(_, v)| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let body = match node.content {
                NodeContent::None => None,
                NodeContent::Children(mut cs) if cs.len() == 1 => Some(cs.remove(0)),
                NodeContent::Children(cs) => Some(Node {
                    tag: Tag::Custom("iq-children".into()),
                    attributes: vec![],
                    content: NodeContent::Children(cs),
                }),
                NodeContent::Text(s) => Some(Node {
                    tag: Tag::Custom("iq-text".into()),
                    attributes: vec![],
                    content: NodeContent::Text(s),
                }),
            };
            InboundFrame::Iq { id, kind, body }
        }
        Some("message") => InboundFrame::Message(node),
        Some("presence") => InboundFrame::Presence(node),
        Some("receipt") => InboundFrame::Receipt(node),
        _ => InboundFrame::Other(node),
    }
}

fn node_string_eq(ns: &NodeString, target: &str) -> bool {
    ns.as_str() == Some(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::binary_xml::{
        Node, NodeContent, NodeString, Tag, encode, token_index,
    };
    use crate::wire::frame::{FLAG_COMPRESSED, FLAG_ENCRYPTED, Frame, FrameFlags};

    fn iq_ping(id: &str) -> Node {
        Node {
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
        }
    }

    fn iq_pong(id: &str) -> Node {
        Node {
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
        }
    }

    fn wrap_frame(node: &Node, flags: u8) -> Vec<u8> {
        let payload = encode(node).unwrap();
        let f = Frame {
            flags: FrameFlags::from_bits(flags),
            payload,
        };
        crate::wire::frame::encode(&f).unwrap()
    }

    // ---- decode_inbound ----

    #[test]
    fn decode_inbound_yields_iq_get_for_ping_frame() {
        let bytes = wrap_frame(&iq_ping("abc"), 0);
        let (frame, used) = decode_inbound(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        match frame {
            InboundFrame::Iq { id, kind, body } => {
                assert_eq!(id, "abc");
                assert_eq!(kind, "get");
                let body = body.expect("ping body");
                assert_eq!(body.tag.as_str(), Some("ping"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decode_inbound_yields_iq_result_for_pong_frame() {
        let bytes = wrap_frame(&iq_pong("def"), 0);
        let (frame, _) = decode_inbound(&bytes).unwrap();
        match frame {
            InboundFrame::Iq { id, kind, body } => {
                assert_eq!(id, "def");
                assert_eq!(kind, "result");
                assert_eq!(body.unwrap().tag.as_str(), Some("pong"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decode_inbound_classifies_message_node() {
        let node = Node::empty(Tag::named("message"));
        let bytes = wrap_frame(&node, 0);
        let (frame, _) = decode_inbound(&bytes).unwrap();
        assert!(matches!(frame, InboundFrame::Message(_)));
    }

    #[test]
    fn decode_inbound_classifies_presence_node() {
        let node = Node::empty(Tag::named("presence"));
        let bytes = wrap_frame(&node, 0);
        let (frame, _) = decode_inbound(&bytes).unwrap();
        assert!(matches!(frame, InboundFrame::Presence(_)));
    }

    #[test]
    fn decode_inbound_classifies_receipt_node() {
        let node = Node::empty(Tag::named("receipt"));
        let bytes = wrap_frame(&node, 0);
        let (frame, _) = decode_inbound(&bytes).unwrap();
        assert!(matches!(frame, InboundFrame::Receipt(_)));
    }

    #[test]
    fn decode_inbound_falls_back_to_other_for_unknown_tag() {
        let node = Node::empty(Tag::custom("blob").unwrap());
        let bytes = wrap_frame(&node, 0);
        let (frame, _) = decode_inbound(&bytes).unwrap();
        match frame {
            InboundFrame::Other(n) => assert_eq!(n.tag.as_str(), Some("blob")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decode_inbound_rejects_encrypted_frame() {
        let node = Node::empty(Tag::named("message"));
        let bytes = wrap_frame(&node, FLAG_ENCRYPTED);
        let err = decode_inbound(&bytes).unwrap_err();
        assert_eq!(err, CodecError::EncryptedNotSupported);
    }

    #[test]
    fn decode_inbound_rejects_compressed_frame() {
        let node = Node::empty(Tag::named("message"));
        let bytes = wrap_frame(&node, FLAG_COMPRESSED);
        let err = decode_inbound(&bytes).unwrap_err();
        assert_eq!(err, CodecError::CompressedNotSupported);
    }

    #[test]
    fn decode_inbound_propagates_frame_error_for_short_buffer() {
        let err = decode_inbound(&[0, 0]).unwrap_err();
        assert!(matches!(err, CodecError::Frame(_)));
    }

    #[test]
    fn decode_inbound_propagates_xml_error_for_garbage_payload() {
        // Build a frame with garbage XML inside.
        let f = Frame::plain(vec![0xAA, 0xBB]);
        let bytes = crate::wire::frame::encode(&f).unwrap();
        let err = decode_inbound(&bytes).unwrap_err();
        assert!(matches!(err, CodecError::Xml(_)));
    }

    // ---- encode_outbound ----

    #[test]
    fn encode_outbound_round_trips_iq_ping() {
        let node = iq_ping("rtt-1");
        let bytes = encode_outbound(&node).unwrap();
        let (back, used) = decode_inbound(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        match back {
            InboundFrame::Iq { id, .. } => assert_eq!(id, "rtt-1"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- classify_node ----

    #[test]
    fn classify_iq_with_text_payload_renders_iq_text() {
        let node = Node {
            tag: Tag::named("iq"),
            attributes: vec![(
                NodeString::from_literal("id"),
                NodeString::from_literal("xx"),
            )],
            content: NodeContent::Text(NodeString::from_literal("hello")),
        };
        let frame = classify_node(node);
        match frame {
            InboundFrame::Iq { id, body, .. } => {
                assert_eq!(id, "xx");
                assert!(matches!(body.unwrap().tag, Tag::Custom(ref s) if s == "iq-text"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn classify_iq_with_no_body() {
        let node = Node {
            tag: Tag::named("iq"),
            attributes: vec![
                (
                    NodeString::from_literal("id"),
                    NodeString::from_literal("1"),
                ),
                (
                    NodeString::from_literal("type"),
                    NodeString::from_literal("get"),
                ),
            ],
            content: NodeContent::None,
        };
        match classify_node(node) {
            InboundFrame::Iq { body, .. } => assert!(body.is_none()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn classify_iq_with_multiple_children_wraps_them() {
        let node = Node {
            tag: Tag::named("iq"),
            attributes: vec![(
                NodeString::from_literal("id"),
                NodeString::from_literal("1"),
            )],
            content: NodeContent::Children(vec![
                Node::empty(Tag::named("ping")),
                Node::empty(Tag::named("pong")),
            ]),
        };
        match classify_node(node) {
            InboundFrame::Iq { body, .. } => {
                let body = body.unwrap();
                assert!(matches!(body.tag, Tag::Custom(ref s) if s == "iq-children"));
                match body.content {
                    NodeContent::Children(cs) => assert_eq!(cs.len(), 2),
                    other => panic!("expected children, got {other:?}"),
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn classify_node_uses_attribute_id_via_indexed_lookup() {
        let node = Node {
            tag: Tag::named("iq"),
            attributes: vec![
                (
                    NodeString::Indexed(token_index("id").unwrap()),
                    NodeString::from_literal("zz"),
                ),
                (
                    NodeString::Indexed(token_index("type").unwrap()),
                    NodeString::Indexed(token_index("error").unwrap()),
                ),
            ],
            content: NodeContent::None,
        };
        match classify_node(node) {
            InboundFrame::Iq { id, kind, .. } => {
                assert_eq!(id, "zz");
                assert_eq!(kind, "error");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn classify_node_default_missing_attributes_to_empty_string() {
        // No id, no type. The classifier should still produce an Iq, just
        // with empty fields — defensive behaviour, parser doesn't crash.
        let node = Node::empty(Tag::named("iq"));
        match classify_node(node) {
            InboundFrame::Iq { id, kind, body } => {
                assert!(id.is_empty());
                assert!(kind.is_empty());
                assert!(body.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn node_string_eq_helper() {
        assert!(node_string_eq(&NodeString::from_literal("x"), "x"));
        assert!(!node_string_eq(&NodeString::from_literal("x"), "y"));
        assert!(node_string_eq(
            &NodeString::Indexed(token_index("iq").unwrap()),
            "iq"
        ));
        assert!(!node_string_eq(&NodeString::Binary(vec![0xFF]), "anything"));
    }

    #[test]
    fn inbound_frame_eq_and_debug() {
        let a = InboundFrame::Iq {
            id: "1".into(),
            kind: "get".into(),
            body: None,
        };
        let b = InboundFrame::Iq {
            id: "1".into(),
            kind: "get".into(),
            body: None,
        };
        assert_eq!(a, b);
        assert!(format!("{a:?}").contains("Iq"));
    }

    #[test]
    fn codec_error_display() {
        let e = CodecError::EncryptedNotSupported;
        assert!(format!("{e}").contains("encrypted"));
        let e = CodecError::CompressedNotSupported;
        assert!(format!("{e}").contains("compressed"));
        let e: CodecError = FrameError::Incomplete { needed: 1 }.into();
        assert!(format!("{e}").contains("frame:"));
        let e: CodecError = binary_xml::XmlError::UnexpectedEof(2).into();
        assert!(format!("{e}").contains("xml:"));
    }

    #[test]
    fn codec_error_eq() {
        assert_eq!(
            CodecError::EncryptedNotSupported,
            CodecError::EncryptedNotSupported
        );
        assert_eq!(
            CodecError::CompressedNotSupported,
            CodecError::CompressedNotSupported
        );
    }
}
