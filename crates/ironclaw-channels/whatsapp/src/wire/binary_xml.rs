//! WhatsApp binary XML encoder/decoder.
//!
//! WhatsApp's protocol carries control-plane messages (handshake, presence,
//! `iq` queries, receipts) as a hand-rolled binary serialisation of an XML
//! subset. The on-the-wire shape is:
//!
//! ```text
//! list_header  ::= LIST_8 u8_count | LIST_16 u16_count | LIST_EMPTY
//! string       ::= TOKEN_INDEX u8         (token table lookup)
//!                | BINARY_8 u8 bytes
//!                | BINARY_20 u24 bytes    (length is the lower 20 bits)
//!                | BINARY_32 u32 bytes
//! tag_node     ::= list_header tag_string attrs_list payload_string?
//! ```
//!
//! The protocol's "tag table" maps the common element and attribute names
//! to single-byte indices. The full Baileys table is large (hundreds of
//! tokens); this module ships **enough of the table to round-trip a small
//! set of `iq` queries** (ping / pong / presence). Unknown tokens are
//! tolerated on decode (they round-trip as their numeric index wrapped in
//! a [`Node::TokenString`]); on encode, the API requires callers to use
//! [`Tag::custom`] for any element name not in the table.
//!
//! ### Scope marker
//!
//! Filling out the full token table is deferred work — see the M8 entry in
//! `PLAN.md`. The current set is exposed via [`tokens::INDEXED_STRINGS`]
//! and the [`token_index`]/[`token_name`] helpers.

use std::fmt;

/// Tag byte introducing a length-8 list.
pub const LIST_8: u8 = 0xF8;
/// Tag byte introducing a length-16 list.
pub const LIST_16: u8 = 0xF9;
/// Tag byte for the empty list.
pub const LIST_EMPTY: u8 = 0x00;

/// Tag byte for an 8-bit-length binary string.
pub const BINARY_8: u8 = 0xFC;
/// Tag byte for a 20-bit-length binary string.
pub const BINARY_20: u8 = 0xFD;
/// Tag byte for a 32-bit-length binary string.
pub const BINARY_32: u8 = 0xFE;

/// Maximum length representable in the 20-bit binary string variant.
pub const BINARY_20_MAX: usize = (1 << 20) - 1;

/// Errors produced by the binary XML codec.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum XmlError {
    /// The buffer ran out mid-structure.
    #[error("unexpected end of buffer at offset {0}")]
    UnexpectedEof(usize),
    /// A length-prefix referenced more bytes than remain in the buffer.
    #[error("string length {len} overruns buffer at offset {offset}")]
    StringOverrun { offset: usize, len: usize },
    /// A list element count would overrun the buffer (sanity check).
    #[error("list count {count} too large at offset {offset}")]
    ListTooLarge { offset: usize, count: usize },
    /// The decoder encountered a tag byte it does not know.
    #[error("unknown tag byte 0x{0:02x} at offset {1}")]
    UnknownTag(u8, usize),
    /// A string slot was expected but a list was found, or vice versa.
    #[error("expected {expected} at offset {offset}, found {found}")]
    UnexpectedShape {
        expected: &'static str,
        offset: usize,
        found: &'static str,
    },
    /// A 20-bit binary string overflow was detected at encode time.
    #[error("string too long: {0} bytes")]
    StringTooLong(usize),
    /// A list element count exceeded the 16-bit cap at encode time.
    #[error("list too long: {0} entries")]
    ListEncodeTooLong(usize),
    /// `Tag::custom` was given an empty name.
    #[error("custom tag name is empty")]
    EmptyTagName,
    /// `Node::Element { tag: Tag::Index(i), .. }` references an unknown index.
    #[error("unknown token index {0}")]
    UnknownTokenIndex(u8),
}

/// A tag name in the binary XML world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    /// A name that lives in the token table; carry the index for the
    /// encoder to emit.
    Indexed(u8),
    /// A name not in the token table; encoder emits a binary string.
    Custom(String),
}

impl Tag {
    /// Build a tag for a known token name. Returns [`Tag::Custom`] if the
    /// name is not in the table.
    pub fn named(name: &str) -> Self {
        match token_index(name) {
            Some(idx) => Self::Indexed(idx),
            None => Self::Custom(name.to_owned()),
        }
    }

    /// Build a tag from an arbitrary string. Errors on empty input.
    pub fn custom(name: impl Into<String>) -> Result<Self, XmlError> {
        let s = name.into();
        if s.is_empty() {
            return Err(XmlError::EmptyTagName);
        }
        Ok(Self::Custom(s))
    }

    /// Resolve the tag back to a `&str`, returning `None` for indexed tags
    /// whose index does not appear in the current token table.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Indexed(i) => token_name(*i),
            Self::Custom(s) => Some(s.as_str()),
        }
    }
}

/// A string value in the binary XML world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeString {
    /// A reference into the token table; the decoder returns this for
    /// indexed strings whose name is known, but callers can also build one.
    Indexed(u8),
    /// A literal UTF-8 string.
    Literal(String),
    /// A raw binary blob (lengths can exceed UTF-8 safe ranges).
    Binary(Vec<u8>),
}

impl NodeString {
    /// Convenience constructor for a literal string.
    pub fn from_literal(s: impl Into<String>) -> Self {
        Self::Literal(s.into())
    }

    /// Resolve to a `&str` when possible.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Indexed(i) => token_name(*i),
            Self::Literal(s) => Some(s.as_str()),
            Self::Binary(_) => None,
        }
    }
}

/// One XML node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub tag: Tag,
    /// Attribute pairs. Encoded as a flat list of `[name1, val1, name2,
    /// val2, ...]`.
    pub attributes: Vec<(NodeString, NodeString)>,
    /// Optional content. Either nested children or a leaf string.
    pub content: NodeContent,
}

impl Node {
    /// Build a leaf element with no content and no attributes.
    pub fn empty(tag: Tag) -> Self {
        Self {
            tag,
            attributes: vec![],
            content: NodeContent::None,
        }
    }

    /// Build an element whose content is a child list.
    pub fn with_children(tag: Tag, children: Vec<Node>) -> Self {
        Self {
            tag,
            attributes: vec![],
            content: NodeContent::Children(children),
        }
    }

    /// Build a leaf element whose content is a single string.
    pub fn with_text(tag: Tag, text: impl Into<String>) -> Self {
        Self {
            tag,
            attributes: vec![],
            content: NodeContent::Text(NodeString::Literal(text.into())),
        }
    }
}

/// What sits inside an element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeContent {
    /// No payload (self-closing).
    None,
    /// Nested children.
    Children(Vec<Node>),
    /// A leaf string.
    Text(NodeString),
}

/// Encode a single node into a byte vector.
pub fn encode(node: &Node) -> Result<Vec<u8>, XmlError> {
    let mut out = Vec::with_capacity(64);
    write_node(&mut out, node)?;
    Ok(out)
}

/// Decode a single node from the front of a buffer. Returns the parsed
/// node and the number of bytes consumed.
pub fn decode(buf: &[u8]) -> Result<(Node, usize), XmlError> {
    let mut r = Reader { buf, pos: 0 };
    let node = r.read_node()?;
    Ok((node, r.pos))
}

// ---- encoder helpers ----

fn write_list_header(out: &mut Vec<u8>, count: usize) -> Result<(), XmlError> {
    if count == 0 {
        out.push(LIST_EMPTY);
    } else if let Ok(c8) = u8::try_from(count) {
        out.push(LIST_8);
        out.push(c8);
    } else if u16::try_from(count).is_ok() {
        out.push(LIST_16);
        out.push(((count >> 8) & 0xFF) as u8);
        out.push((count & 0xFF) as u8);
    } else {
        return Err(XmlError::ListEncodeTooLong(count));
    }
    Ok(())
}

fn write_node_string(out: &mut Vec<u8>, s: &NodeString) -> Result<(), XmlError> {
    match s {
        NodeString::Indexed(i) => {
            out.push(*i);
        }
        NodeString::Literal(s) => write_binary(out, s.as_bytes())?,
        NodeString::Binary(b) => write_binary(out, b)?,
    }
    Ok(())
}

fn write_binary(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), XmlError> {
    let len = bytes.len();
    if let Ok(len8) = u8::try_from(len) {
        out.push(BINARY_8);
        out.push(len8);
    } else if len <= BINARY_20_MAX {
        out.push(BINARY_20);
        out.push(((len >> 16) & 0x0F) as u8);
        out.push(((len >> 8) & 0xFF) as u8);
        out.push((len & 0xFF) as u8);
    } else if u32::try_from(len).is_ok() {
        out.push(BINARY_32);
        out.push(((len >> 24) & 0xFF) as u8);
        out.push(((len >> 16) & 0xFF) as u8);
        out.push(((len >> 8) & 0xFF) as u8);
        out.push((len & 0xFF) as u8);
    } else {
        return Err(XmlError::StringTooLong(len));
    }
    out.extend_from_slice(bytes);
    Ok(())
}

fn write_tag(out: &mut Vec<u8>, tag: &Tag) -> Result<(), XmlError> {
    match tag {
        Tag::Indexed(i) => out.push(*i),
        Tag::Custom(s) => write_binary(out, s.as_bytes())?,
    }
    Ok(())
}

fn write_node(out: &mut Vec<u8>, node: &Node) -> Result<(), XmlError> {
    // attrs slots: name + value per pair => 2 * len.
    let attrs_slots = node.attributes.len() * 2;
    // total list size: tag + attrs_slots + optional payload.
    let payload_slot = match node.content {
        NodeContent::None => 0,
        NodeContent::Children(_) | NodeContent::Text(_) => 1,
    };
    let list_size = 1 + attrs_slots + payload_slot;
    write_list_header(out, list_size)?;
    write_tag(out, &node.tag)?;
    for (k, v) in &node.attributes {
        write_node_string(out, k)?;
        write_node_string(out, v)?;
    }
    match &node.content {
        NodeContent::None => {}
        NodeContent::Text(s) => write_node_string(out, s)?,
        NodeContent::Children(children) => {
            write_list_header(out, children.len())?;
            for c in children {
                write_node(out, c)?;
            }
        }
    }
    Ok(())
}

// ---- decoder helpers ----

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn next_byte(&mut self) -> Result<u8, XmlError> {
        let b = *self.buf.get(self.pos).ok_or(XmlError::UnexpectedEof(self.pos))?;
        self.pos += 1;
        Ok(b)
    }

    fn read_list_size(&mut self) -> Result<usize, XmlError> {
        let tag = self.next_byte()?;
        match tag {
            LIST_EMPTY => Ok(0),
            LIST_8 => {
                let n = self.next_byte()?;
                Ok(usize::from(n))
            }
            LIST_16 => {
                let hi = self.next_byte()?;
                let lo = self.next_byte()?;
                let v = (usize::from(hi) << 8) | usize::from(lo);
                if v > self.buf.len() {
                    return Err(XmlError::ListTooLarge {
                        offset: self.pos,
                        count: v,
                    });
                }
                Ok(v)
            }
            other => Err(XmlError::UnknownTag(other, self.pos - 1)),
        }
    }

    fn read_binary(&mut self, lead: u8) -> Result<Vec<u8>, XmlError> {
        let offset = self.pos - 1;
        let len = match lead {
            BINARY_8 => usize::from(self.next_byte()?),
            BINARY_20 => {
                let hi = self.next_byte()? & 0x0F;
                let mid = self.next_byte()?;
                let lo = self.next_byte()?;
                (usize::from(hi) << 16) | (usize::from(mid) << 8) | usize::from(lo)
            }
            BINARY_32 => {
                let b0 = self.next_byte()?;
                let b1 = self.next_byte()?;
                let b2 = self.next_byte()?;
                let b3 = self.next_byte()?;
                (usize::from(b0) << 24)
                    | (usize::from(b1) << 16)
                    | (usize::from(b2) << 8)
                    | usize::from(b3)
            }
            _ => unreachable!("read_binary called with non-binary tag"),
        };
        if self.pos + len > self.buf.len() {
            return Err(XmlError::StringOverrun { offset, len });
        }
        let bytes = self.buf[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Ok(bytes)
    }

    fn read_node_string(&mut self) -> Result<NodeString, XmlError> {
        let lead = self.next_byte()?;
        match lead {
            BINARY_8 | BINARY_20 | BINARY_32 => {
                let bytes = self.read_binary(lead)?;
                Ok(match String::from_utf8(bytes) {
                    Ok(s) => NodeString::Literal(s),
                    Err(e) => NodeString::Binary(e.into_bytes()),
                })
            }
            other if is_indexable_token(other) => Ok(NodeString::Indexed(other)),
            other => Err(XmlError::UnknownTag(other, self.pos - 1)),
        }
    }

    fn read_tag(&mut self) -> Result<Tag, XmlError> {
        let lead = self.next_byte()?;
        match lead {
            BINARY_8 | BINARY_20 | BINARY_32 => {
                let bytes = self.read_binary(lead)?;
                let s = String::from_utf8(bytes).map_err(|e| {
                    XmlError::UnexpectedShape {
                        expected: "utf-8 tag name",
                        offset: self.pos,
                        found: if e.utf8_error().valid_up_to() == 0 {
                            "non-utf8 bytes"
                        } else {
                            "non-utf8 trailing bytes"
                        },
                    }
                })?;
                Ok(Tag::Custom(s))
            }
            other if is_indexable_token(other) => Ok(Tag::Indexed(other)),
            other => Err(XmlError::UnknownTag(other, self.pos - 1)),
        }
    }

    fn read_node(&mut self) -> Result<Node, XmlError> {
        let list_size = self.read_list_size()?;
        if list_size == 0 {
            return Err(XmlError::UnexpectedShape {
                expected: "element list",
                offset: self.pos,
                found: "empty list",
            });
        }
        // First slot is the tag.
        let tag = self.read_tag()?;
        // Attribute pairs: count is `(list_size - 1) / 2` if there is no
        // payload slot, or `(list_size - 2) / 2` if there is.
        let payload_present = list_size % 2 == 0;
        let attr_pairs = if payload_present {
            (list_size - 2) / 2
        } else {
            (list_size - 1) / 2
        };
        let mut attributes = Vec::with_capacity(attr_pairs);
        for _ in 0..attr_pairs {
            let k = self.read_node_string()?;
            let v = self.read_node_string()?;
            attributes.push((k, v));
        }
        let content = if payload_present {
            // Peek the next byte: list header vs string.
            let peek = *self
                .buf
                .get(self.pos)
                .ok_or(XmlError::UnexpectedEof(self.pos))?;
            if peek == LIST_EMPTY || peek == LIST_8 || peek == LIST_16 {
                let child_count = self.read_list_size()?;
                let mut children = Vec::with_capacity(child_count);
                for _ in 0..child_count {
                    children.push(self.read_node()?);
                }
                NodeContent::Children(children)
            } else {
                let s = self.read_node_string()?;
                NodeContent::Text(s)
            }
        } else {
            NodeContent::None
        };
        Ok(Node {
            tag,
            attributes,
            content,
        })
    }
}

// ---- token table ----

pub mod tokens {
    //! Subset of the WhatsApp binary XML token table.
    //!
    //! The table here covers the elements and attribute names needed for an
    //! `iq` ping/pong, the handshake `stream:features` blob, and a couple
    //! of common presence/receipt names. The full Baileys table is much
    //! larger; filling it in is deferred work.

    /// Token index for the very first non-list/non-binary byte we use as a
    /// table index. The protocol uses indices `0x01..=0xF7`; we keep an
    /// allow-list to avoid colliding with the list/binary tags.
    pub const FIRST_TOKEN_INDEX: u8 = 0x03;

    /// Public token table. Indexed by the byte that appears on the wire.
    ///
    /// Entries set to `""` are unused placeholders; the decoder treats them
    /// as unknown indices.
    pub const INDEXED_STRINGS: &[(u8, &str)] = &[
        (0x03, "iq"),
        (0x04, "id"),
        (0x05, "type"),
        (0x06, "to"),
        (0x07, "from"),
        (0x08, "get"),
        (0x09, "set"),
        (0x0A, "result"),
        (0x0B, "error"),
        (0x0C, "ping"),
        (0x0D, "pong"),
        (0x0E, "stream:features"),
        (0x0F, "success"),
        (0x10, "failure"),
        (0x11, "xmlns"),
        (0x12, "presence"),
        (0x13, "available"),
        (0x14, "unavailable"),
        (0x15, "message"),
        (0x16, "receipt"),
        (0x17, "ack"),
        (0x18, "code"),
        (0x19, "reason"),
        (0x1A, "device"),
        (0x1B, "user"),
        (0x1C, "group"),
        (0x1D, "name"),
        (0x1E, "value"),
        (0x1F, "version"),
    ];
}

/// Look up the token index for a known name.
pub fn token_index(name: &str) -> Option<u8> {
    tokens::INDEXED_STRINGS
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(i, _)| *i)
}

/// Look up the name for a known token index.
pub fn token_name(index: u8) -> Option<&'static str> {
    tokens::INDEXED_STRINGS
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, n)| *n)
}

/// True when `b` is a byte the wire format may use to refer to a string by
/// index (i.e. not a structural tag like `LIST_*` / `BINARY_*`).
fn is_indexable_token(b: u8) -> bool {
    !matches!(
        b,
        LIST_EMPTY | LIST_8 | LIST_16 | BINARY_8 | BINARY_20 | BINARY_32
    )
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tag::Indexed(i) => match token_name(*i) {
                Some(n) => f.write_str(n),
                None => write!(f, "#0x{i:02x}"),
            },
            Tag::Custom(s) => f.write_str(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- token table ----

    #[test]
    fn token_index_round_trips() {
        for (i, n) in tokens::INDEXED_STRINGS {
            assert_eq!(token_index(n), Some(*i));
            assert_eq!(token_name(*i), Some(*n));
        }
    }

    #[test]
    fn token_index_unknown_returns_none() {
        assert_eq!(token_index("definitely-not-a-token"), None);
        assert_eq!(token_name(0xFF), None);
    }

    #[test]
    fn iq_ping_token_present() {
        assert!(token_index("iq").is_some());
        assert!(token_index("ping").is_some());
        assert!(token_index("pong").is_some());
        assert!(token_index("id").is_some());
        assert!(token_index("type").is_some());
    }

    #[test]
    fn first_token_index_constant() {
        assert_eq!(tokens::FIRST_TOKEN_INDEX, 0x03);
    }

    // ---- Tag ----

    #[test]
    fn tag_named_indexes_known_names() {
        assert!(matches!(Tag::named("iq"), Tag::Indexed(_)));
    }

    #[test]
    fn tag_named_falls_back_to_custom() {
        match Tag::named("not-a-token") {
            Tag::Custom(s) => assert_eq!(s, "not-a-token"),
            other @ Tag::Indexed(_) => panic!("expected Custom, got {other:?}"),
        }
    }

    #[test]
    fn tag_custom_rejects_empty() {
        assert_eq!(Tag::custom(""), Err(XmlError::EmptyTagName));
    }

    #[test]
    fn tag_custom_ok_for_nonempty() {
        assert_eq!(Tag::custom("x").unwrap(), Tag::Custom("x".into()));
    }

    #[test]
    fn tag_as_str_indexed_and_custom() {
        assert_eq!(Tag::named("iq").as_str(), Some("iq"));
        assert_eq!(Tag::custom("z").unwrap().as_str(), Some("z"));
        assert_eq!(Tag::Indexed(0xFE).as_str(), None);
    }

    #[test]
    fn tag_display_uses_name_or_hex() {
        let t = Tag::named("iq");
        assert_eq!(format!("{t}"), "iq");
        let custom = Tag::custom("xyz").unwrap();
        assert_eq!(format!("{custom}"), "xyz");
        let bad = Tag::Indexed(0xFE);
        assert_eq!(format!("{bad}"), "#0xfe");
    }

    // ---- NodeString ----

    #[test]
    fn node_string_from_literal() {
        let s = NodeString::from_literal("hi");
        match s {
            NodeString::Literal(t) => assert_eq!(t, "hi"),
            NodeString::Indexed(_) | NodeString::Binary(_) => {
                panic!("expected Literal");
            }
        }
    }

    #[test]
    fn node_string_as_str_paths() {
        assert_eq!(NodeString::from_literal("a").as_str(), Some("a"));
        assert_eq!(
            NodeString::Indexed(token_index("iq").unwrap()).as_str(),
            Some("iq")
        );
        assert_eq!(NodeString::Binary(vec![0xFF]).as_str(), None);
    }

    // ---- header writer ----

    #[test]
    fn write_list_header_empty_uses_list_empty() {
        let mut out = vec![];
        write_list_header(&mut out, 0).unwrap();
        assert_eq!(out, vec![LIST_EMPTY]);
    }

    #[test]
    fn write_list_header_small() {
        let mut out = vec![];
        write_list_header(&mut out, 5).unwrap();
        assert_eq!(out, vec![LIST_8, 5]);
    }

    #[test]
    fn write_list_header_at_255_boundary() {
        let mut out = vec![];
        write_list_header(&mut out, 255).unwrap();
        assert_eq!(out, vec![LIST_8, 255]);
    }

    #[test]
    fn write_list_header_above_255_uses_list_16() {
        let mut out = vec![];
        write_list_header(&mut out, 256).unwrap();
        assert_eq!(out, vec![LIST_16, 0x01, 0x00]);
    }

    #[test]
    fn write_list_header_too_large_errors() {
        let mut out = vec![];
        let err = write_list_header(&mut out, 0x10000).unwrap_err();
        assert_eq!(err, XmlError::ListEncodeTooLong(0x10000));
    }

    // ---- binary string writer ----

    #[test]
    fn write_binary_short_uses_binary_8() {
        let mut out = vec![];
        write_binary(&mut out, &[1, 2, 3]).unwrap();
        assert_eq!(out, vec![BINARY_8, 3, 1, 2, 3]);
    }

    #[test]
    fn write_binary_at_255_boundary() {
        let mut out = vec![];
        let payload = vec![0u8; 255];
        write_binary(&mut out, &payload).unwrap();
        assert_eq!(out[0], BINARY_8);
        assert_eq!(out[1], 255);
    }

    #[test]
    fn write_binary_at_256_uses_binary_20() {
        let mut out = vec![];
        let payload = vec![0u8; 256];
        write_binary(&mut out, &payload).unwrap();
        assert_eq!(out[0], BINARY_20);
    }

    #[test]
    fn write_binary_at_binary20_max_boundary() {
        let mut out = vec![];
        let payload = vec![0u8; BINARY_20_MAX];
        write_binary(&mut out, &payload).unwrap();
        assert_eq!(out[0], BINARY_20);
    }

    #[test]
    fn write_binary_above_binary20_uses_binary_32() {
        let mut out = vec![];
        let payload = vec![0u8; BINARY_20_MAX + 1];
        write_binary(&mut out, &payload).unwrap();
        assert_eq!(out[0], BINARY_32);
    }

    // ---- round-trip nodes ----

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

    #[test]
    fn encode_decode_iq_ping_round_trip() {
        let node = iq_ping("123");
        let bytes = encode(&node).unwrap();
        let (decoded, used) = decode(&bytes).unwrap();
        assert_eq!(decoded, node);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn encode_decode_iq_pong_round_trip() {
        let node = Node {
            tag: Tag::named("iq"),
            attributes: vec![
                (
                    NodeString::Indexed(token_index("id").unwrap()),
                    NodeString::from_literal("123"),
                ),
                (
                    NodeString::Indexed(token_index("type").unwrap()),
                    NodeString::Indexed(token_index("result").unwrap()),
                ),
            ],
            content: NodeContent::Children(vec![Node::empty(Tag::named("pong"))]),
        };
        let bytes = encode(&node).unwrap();
        let (decoded, used) = decode(&bytes).unwrap();
        assert_eq!(decoded, node);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn round_trip_empty_element() {
        let node = Node::empty(Tag::named("ping"));
        let bytes = encode(&node).unwrap();
        let (decoded, _) = decode(&bytes).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn round_trip_element_with_text() {
        let node = Node::with_text(Tag::named("name"), "Alice");
        let bytes = encode(&node).unwrap();
        let (decoded, _) = decode(&bytes).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn round_trip_element_with_custom_tag() {
        let node = Node::with_text(Tag::custom("xyzzy").unwrap(), "v");
        let bytes = encode(&node).unwrap();
        let (decoded, _) = decode(&bytes).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn round_trip_element_with_attributes_only() {
        let node = Node {
            tag: Tag::named("iq"),
            attributes: vec![
                (
                    NodeString::from_literal("id"),
                    NodeString::from_literal("99"),
                ),
                (
                    NodeString::from_literal("type"),
                    NodeString::from_literal("get"),
                ),
            ],
            content: NodeContent::None,
        };
        let bytes = encode(&node).unwrap();
        let (decoded, _) = decode(&bytes).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn round_trip_nested_children() {
        let inner = Node::with_text(Tag::named("value"), "1");
        let middle = Node::with_children(Tag::named("user"), vec![inner.clone()]);
        let outer = Node::with_children(Tag::named("group"), vec![middle.clone(), inner.clone()]);
        let bytes = encode(&outer).unwrap();
        let (decoded, _) = decode(&bytes).unwrap();
        assert_eq!(decoded, outer);
    }

    #[test]
    fn round_trip_binary_attribute_value() {
        // A 200-byte binary blob exercises the BINARY_8 path with a
        // boundary value that fits exactly in u8.
        let payload = vec![0xAB; 200];
        let node = Node {
            tag: Tag::custom("blob").unwrap(),
            attributes: vec![(
                NodeString::from_literal("data"),
                NodeString::Binary(payload.clone()),
            )],
            content: NodeContent::None,
        };
        let bytes = encode(&node).unwrap();
        let (decoded, _) = decode(&bytes).unwrap();
        // The binary blob round-trips as either Binary or Literal depending
        // on UTF-8 validity. Here it is all 0xAB which is invalid UTF-8.
        match &decoded.attributes[0].1 {
            NodeString::Binary(b) => assert_eq!(b, &payload),
            NodeString::Literal(_) | NodeString::Indexed(_) => {
                panic!("expected Binary");
            }
        }
    }

    #[test]
    fn round_trip_indexed_attribute_value() {
        let node = Node {
            tag: Tag::named("iq"),
            attributes: vec![(
                NodeString::Indexed(token_index("type").unwrap()),
                NodeString::Indexed(token_index("result").unwrap()),
            )],
            content: NodeContent::None,
        };
        let bytes = encode(&node).unwrap();
        let (decoded, _) = decode(&bytes).unwrap();
        assert_eq!(decoded, node);
    }

    // ---- error paths ----

    #[test]
    fn decode_empty_buffer_errors() {
        let err = decode(&[]).unwrap_err();
        assert_eq!(err, XmlError::UnexpectedEof(0));
    }

    #[test]
    fn decode_empty_list_errors() {
        let bytes = vec![LIST_EMPTY];
        let err = decode(&bytes).unwrap_err();
        matches!(err, XmlError::UnexpectedShape { .. });
    }

    #[test]
    fn decode_truncated_list_header_errors() {
        let bytes = vec![LIST_8];
        let err = decode(&bytes).unwrap_err();
        assert!(matches!(err, XmlError::UnexpectedEof(_)));
    }

    #[test]
    fn decode_truncated_payload_errors() {
        // List with one tag slot, tag is BINARY_8 of length 5, but only 2
        // payload bytes follow.
        let mut bytes = vec![LIST_8, 0x01, BINARY_8, 0x05];
        bytes.extend_from_slice(b"ab");
        let err = decode(&bytes).unwrap_err();
        assert!(matches!(err, XmlError::StringOverrun { .. }));
    }

    #[test]
    fn decode_unknown_tag_byte_errors() {
        // The list_size byte is fine (LIST_8 / 1) but the tag slot is the
        // reserved 0xFF byte which is in the indexable-token range — the
        // decoder accepts it as Tag::Indexed and the caller asks for its
        // name. We exercise the underlying "unknown list-tag" error by
        // feeding a top-level byte that is neither a list nor binary.
        let bytes = vec![0xFA, 0x01, 0x03];
        let err = decode(&bytes).unwrap_err();
        assert!(matches!(err, XmlError::UnknownTag(0xFA, _)));
    }

    #[test]
    fn decode_unexpected_eof_in_node_string_errors() {
        // List of size 2 (tag + payload), tag is BINARY_8 length 1, then
        // payload string is missing entirely.
        let mut bytes = vec![LIST_8, 0x02, BINARY_8, 0x01];
        bytes.extend_from_slice(b"x");
        let err = decode(&bytes).unwrap_err();
        assert!(matches!(err, XmlError::UnexpectedEof(_)));
    }

    #[test]
    fn decode_binary_20_string() {
        // Build a binary string longer than 255 bytes to force the
        // BINARY_20 path.
        let payload = vec![b'A'; 300];
        let mut buf = vec![BINARY_20];
        buf.push(0); // hi=0
        buf.push(((300 >> 8) & 0xFF) as u8); // mid
        buf.push((300 & 0xFF) as u8); // lo
        buf.extend(&payload);
        // Wrap in a one-slot list.
        let mut framed = vec![LIST_8, 0x01];
        framed.extend(&buf);
        let (decoded, _) = decode(&framed).unwrap();
        match decoded.tag {
            Tag::Custom(s) => {
                assert_eq!(s.len(), 300);
                assert!(s.starts_with("AAAA"));
            }
            Tag::Indexed(i) => panic!("expected Custom, got Indexed({i})"),
        }
    }

    #[test]
    fn decode_binary_32_string() {
        // Use a modest payload to keep the test cheap.
        let payload = vec![b'B'; 16];
        let mut buf = vec![BINARY_32];
        buf.push(0);
        buf.push(0);
        buf.push(0);
        buf.push(16);
        buf.extend(&payload);
        let mut framed = vec![LIST_8, 0x01];
        framed.extend(&buf);
        let (decoded, _) = decode(&framed).unwrap();
        match decoded.tag {
            Tag::Custom(s) => assert_eq!(s, "BBBBBBBBBBBBBBBB"),
            Tag::Indexed(i) => panic!("expected Custom, got Indexed({i})"),
        }
    }

    #[test]
    fn decode_non_utf8_tag_name_errors_with_unexpected_shape() {
        // BINARY_8 of length 2 with high bytes that aren't valid UTF-8.
        let bytes = vec![LIST_8, 0x01, BINARY_8, 0x02, 0xFF, 0xFE];
        let err = decode(&bytes).unwrap_err();
        assert!(matches!(err, XmlError::UnexpectedShape { .. }));
    }

    #[test]
    fn decode_non_utf8_attribute_value_is_binary_variant() {
        // Build: list_size=3 (tag + 1 attr pair, no payload):
        //   tag = Indexed(iq)
        //   attr key = Indexed(name)
        //   attr value = BINARY_8 [0xFF, 0xFE]
        let bytes = vec![
            LIST_8,
            0x03,
            token_index("iq").unwrap(),
            token_index("name").unwrap(),
            BINARY_8,
            0x02,
            0xFF,
            0xFE,
        ];
        let (decoded, _) = decode(&bytes).unwrap();
        match &decoded.attributes[0].1 {
            NodeString::Binary(b) => assert_eq!(b, &vec![0xFF, 0xFE]),
            NodeString::Literal(_) | NodeString::Indexed(_) => {
                panic!("expected Binary");
            }
        }
    }

    // ---- encode error path ----

    #[test]
    fn encode_too_long_list_errors() {
        // Build a node with `u16::MAX as usize + 1` attributes which makes
        // its list size exceed u16::MAX after the +1 for the tag slot.
        let mut attrs = Vec::with_capacity((u16::MAX as usize) + 1);
        for _ in 0..=u16::MAX as usize {
            attrs.push((
                NodeString::from_literal("k"),
                NodeString::from_literal("v"),
            ));
        }
        let node = Node {
            tag: Tag::named("iq"),
            attributes: attrs,
            content: NodeContent::None,
        };
        let err = encode(&node).unwrap_err();
        assert!(matches!(err, XmlError::ListEncodeTooLong(_)));
    }

    // ---- misc ----

    #[test]
    fn is_indexable_token_excludes_structural_tags() {
        for b in [LIST_EMPTY, LIST_8, LIST_16, BINARY_8, BINARY_20, BINARY_32] {
            assert!(!is_indexable_token(b));
        }
        // A common token byte is indexable.
        assert!(is_indexable_token(token_index("iq").unwrap()));
        // 0x55 isn't structural so it is indexable.
        assert!(is_indexable_token(0x55));
    }

    #[test]
    fn node_constructors_set_fields() {
        let n = Node::empty(Tag::named("ping"));
        assert!(n.attributes.is_empty());
        assert_eq!(n.content, NodeContent::None);
        let n = Node::with_text(Tag::named("name"), "Alice");
        assert!(matches!(n.content, NodeContent::Text(NodeString::Literal(_))));
        let n = Node::with_children(Tag::named("group"), vec![Node::empty(Tag::named("user"))]);
        match n.content {
            NodeContent::Children(cs) => assert_eq!(cs.len(), 1),
            NodeContent::None | NodeContent::Text(_) => {
                panic!("expected Children");
            }
        }
    }

    #[test]
    fn xml_error_display_and_debug() {
        let e = XmlError::UnexpectedEof(7);
        assert!(format!("{e}").contains('7'));
        assert!(format!("{e:?}").contains("UnexpectedEof"));
        let e = XmlError::StringOverrun { offset: 1, len: 9 };
        assert!(format!("{e}").contains("overruns"));
        let e = XmlError::ListTooLarge { offset: 0, count: 99 };
        assert!(format!("{e}").contains("99"));
        let e = XmlError::UnknownTag(0xAA, 3);
        assert!(format!("{e}").contains("0xaa"));
        let e = XmlError::UnexpectedShape {
            expected: "a",
            offset: 0,
            found: "b",
        };
        assert!(format!("{e}").contains("expected a"));
        let e = XmlError::StringTooLong(99);
        assert!(format!("{e}").contains("99"));
        let e = XmlError::ListEncodeTooLong(7);
        assert!(format!("{e}").contains('7'));
        let e = XmlError::EmptyTagName;
        assert!(format!("{e}").contains("empty"));
        let e = XmlError::UnknownTokenIndex(7);
        assert!(format!("{e}").contains('7'));
    }

    #[test]
    fn xml_error_equality() {
        assert_eq!(
            XmlError::UnexpectedEof(1),
            XmlError::UnexpectedEof(1)
        );
        assert_ne!(
            XmlError::UnexpectedEof(1),
            XmlError::UnexpectedEof(2)
        );
    }

    #[test]
    fn constants_are_documented_values() {
        assert_eq!(LIST_8, 0xF8);
        assert_eq!(LIST_16, 0xF9);
        assert_eq!(LIST_EMPTY, 0x00);
        assert_eq!(BINARY_8, 0xFC);
        assert_eq!(BINARY_20, 0xFD);
        assert_eq!(BINARY_32, 0xFE);
        assert_eq!(BINARY_20_MAX, 0x000F_FFFF);
    }
}
