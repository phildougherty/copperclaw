//! Post-handshake WhatsApp wire framing.
//!
//! Once the Noise XX handshake completes, every WhatsApp frame on the
//! WebSocket has this shape:
//!
//! ```text
//! +---------+---------------+---------------------------+
//! | flags   | length (u24)  | payload                   |
//! | 1 byte  | 3 bytes BE    | `length` bytes            |
//! +---------+---------------+---------------------------+
//! ```
//!
//! - `flags` is an 8-bit bitmask. The two currently-defined bits in the
//!   reverse-engineered protocol are `0x80` (the payload is gzip-compressed)
//!   and `0x02` (the payload is an end-to-end encrypted Signal payload
//!   rather than binary XML). This module treats the flags byte as opaque
//!   and exposes [`FrameFlags`] convenience helpers.
//! - `length` is the payload length, big-endian, 24 bits. The maximum
//!   payload size is therefore `(1 << 24) - 1` bytes — see
//!   [`MAX_PAYLOAD_LEN`].
//!
//! This module is **transport- and crypto-free**: it just (de)serialises
//! framed bytes. Decoding produces a single `Frame` per call so callers
//! can drive a streaming reader by re-invoking `decode` against a growing
//! buffer.

use std::fmt;

/// Maximum payload length representable by the u24 length field, in bytes.
pub const MAX_PAYLOAD_LEN: usize = (1 << 24) - 1;

/// Number of bytes occupied by the fixed-size frame header (`flags` plus
/// `u24 length`).
pub const HEADER_LEN: usize = 4;

/// Bit set in the `flags` byte when the payload is gzip-compressed.
pub const FLAG_COMPRESSED: u8 = 0x80;

/// Bit set in the `flags` byte when the payload is a Signal-encrypted
/// chunk rather than binary XML.
pub const FLAG_ENCRYPTED: u8 = 0x02;

/// A parsed (or to-be-encoded) WhatsApp frame.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub flags: FrameFlags,
    pub payload: Vec<u8>,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("flags", &self.flags)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl Frame {
    /// Build a plain (no-flags) frame.
    pub fn plain(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            flags: FrameFlags::empty(),
            payload: payload.into(),
        }
    }
}

/// Type-safe wrapper around the `flags` byte.
#[derive(Copy, Clone, Default, PartialEq, Eq)]
pub struct FrameFlags(pub u8);

impl FrameFlags {
    /// All bits clear.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Construct a [`FrameFlags`] from a raw byte.
    pub const fn from_bits(b: u8) -> Self {
        Self(b)
    }

    /// Raw byte value.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// True when the compressed-payload flag is set.
    pub const fn is_compressed(self) -> bool {
        (self.0 & FLAG_COMPRESSED) != 0
    }

    /// True when the encrypted-payload flag is set.
    pub const fn is_encrypted(self) -> bool {
        (self.0 & FLAG_ENCRYPTED) != 0
    }

    /// Return a copy with the compressed flag set.
    #[must_use]
    pub const fn with_compressed(self) -> Self {
        Self(self.0 | FLAG_COMPRESSED)
    }

    /// Return a copy with the encrypted flag set.
    #[must_use]
    pub const fn with_encrypted(self) -> Self {
        Self(self.0 | FLAG_ENCRYPTED)
    }
}

impl fmt::Debug for FrameFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FrameFlags(0x{:02x}{}{})",
            self.0,
            if self.is_compressed() { " compressed" } else { "" },
            if self.is_encrypted() { " encrypted" } else { "" }
        )
    }
}

/// Errors emitted by the frame codec.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    /// The supplied buffer is too short to contain a complete frame.
    ///
    /// Carries the number of additional bytes the caller would need to
    /// supply for the frame to be parseable.
    #[error("incomplete frame: need {needed} more byte(s)")]
    Incomplete { needed: usize },
    /// The encoded payload exceeds [`MAX_PAYLOAD_LEN`].
    #[error("payload too large: {0} bytes (max {MAX_PAYLOAD_LEN})")]
    TooLarge(usize),
}

/// Encode a [`Frame`] into a fresh `Vec<u8>`.
pub fn encode(frame: &Frame) -> Result<Vec<u8>, FrameError> {
    if frame.payload.len() > MAX_PAYLOAD_LEN {
        return Err(FrameError::TooLarge(frame.payload.len()));
    }
    let mut out = Vec::with_capacity(HEADER_LEN + frame.payload.len());
    out.push(frame.flags.bits());
    let len = frame.payload.len();
    // Big-endian u24 — top three bytes of the u32, dropping the high byte.
    out.push(((len >> 16) & 0xFF) as u8);
    out.push(((len >> 8) & 0xFF) as u8);
    out.push((len & 0xFF) as u8);
    out.extend_from_slice(&frame.payload);
    Ok(out)
}

/// Decode one frame from the front of `buf`.
///
/// Returns the parsed [`Frame`] and the number of bytes consumed (the
/// caller advances its buffer by that amount). Returns
/// [`FrameError::Incomplete`] when the buffer contains a partial frame.
pub fn decode(buf: &[u8]) -> Result<(Frame, usize), FrameError> {
    if buf.len() < HEADER_LEN {
        return Err(FrameError::Incomplete {
            needed: HEADER_LEN - buf.len(),
        });
    }
    let flags = FrameFlags(buf[0]);
    let len = (usize::from(buf[1]) << 16) | (usize::from(buf[2]) << 8) | usize::from(buf[3]);
    if buf.len() < HEADER_LEN + len {
        return Err(FrameError::Incomplete {
            needed: HEADER_LEN + len - buf.len(),
        });
    }
    let payload = buf[HEADER_LEN..HEADER_LEN + len].to_vec();
    Ok((Frame { flags, payload }, HEADER_LEN + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(flags: u8, payload: Vec<u8>) -> Frame {
        Frame {
            flags: FrameFlags::from_bits(flags),
            payload,
        }
    }

    // ---------------- encode ----------------

    #[test]
    fn encode_empty_frame_header_only() {
        let bytes = encode(&frame(0x00, vec![])).unwrap();
        assert_eq!(bytes, vec![0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn encode_writes_flags_byte() {
        let bytes = encode(&frame(0x80, vec![1, 2, 3])).unwrap();
        assert_eq!(bytes[0], 0x80);
        assert_eq!(bytes[1..4], [0x00, 0x00, 0x03]);
        assert_eq!(&bytes[4..], &[1, 2, 3]);
    }

    #[test]
    fn encode_length_at_63() {
        let payload = vec![0xAB; 63];
        let bytes = encode(&Frame::plain(payload.clone())).unwrap();
        assert_eq!(bytes[1..4], [0x00, 0x00, 0x3F]);
        assert_eq!(&bytes[4..], &payload[..]);
    }

    #[test]
    fn encode_length_at_64() {
        let payload = vec![0xCD; 64];
        let bytes = encode(&Frame::plain(payload)).unwrap();
        assert_eq!(bytes[1..4], [0x00, 0x00, 0x40]);
        assert_eq!(bytes.len(), HEADER_LEN + 64);
    }

    #[test]
    fn encode_length_at_255() {
        let payload = vec![0u8; 255];
        let bytes = encode(&Frame::plain(payload)).unwrap();
        assert_eq!(bytes[1..4], [0x00, 0x00, 0xFF]);
    }

    #[test]
    fn encode_length_at_256() {
        let payload = vec![0u8; 256];
        let bytes = encode(&Frame::plain(payload)).unwrap();
        assert_eq!(bytes[1..4], [0x00, 0x01, 0x00]);
    }

    #[test]
    fn encode_length_at_65535() {
        let payload = vec![0u8; 65_535];
        let bytes = encode(&Frame::plain(payload)).unwrap();
        assert_eq!(bytes[1..4], [0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn encode_length_at_65536() {
        let payload = vec![0u8; 65_536];
        let bytes = encode(&Frame::plain(payload)).unwrap();
        assert_eq!(bytes[1..4], [0x01, 0x00, 0x00]);
    }

    #[test]
    fn encode_length_at_max_payload() {
        let payload = vec![0u8; MAX_PAYLOAD_LEN];
        let bytes = encode(&Frame::plain(payload)).unwrap();
        assert_eq!(bytes[1..4], [0xFF, 0xFF, 0xFF]);
        assert_eq!(bytes.len(), HEADER_LEN + MAX_PAYLOAD_LEN);
    }

    #[test]
    fn encode_rejects_payload_above_max() {
        let payload = vec![0u8; MAX_PAYLOAD_LEN + 1];
        let err = encode(&Frame::plain(payload)).unwrap_err();
        assert_eq!(err, FrameError::TooLarge(MAX_PAYLOAD_LEN + 1));
    }

    #[test]
    fn encode_compressed_flag() {
        let f = Frame {
            flags: FrameFlags::empty().with_compressed(),
            payload: vec![0xAA],
        };
        let bytes = encode(&f).unwrap();
        assert_eq!(bytes[0], FLAG_COMPRESSED);
    }

    #[test]
    fn encode_encrypted_flag() {
        let f = Frame {
            flags: FrameFlags::empty().with_encrypted(),
            payload: vec![],
        };
        let bytes = encode(&f).unwrap();
        assert_eq!(bytes[0], FLAG_ENCRYPTED);
    }

    #[test]
    fn encode_combined_flags() {
        let flags = FrameFlags::empty().with_compressed().with_encrypted();
        let f = Frame {
            flags,
            payload: vec![],
        };
        let bytes = encode(&f).unwrap();
        assert_eq!(bytes[0], FLAG_COMPRESSED | FLAG_ENCRYPTED);
    }

    // ---------------- decode ----------------

    #[test]
    fn decode_zero_length_frame() {
        let bytes = [0x00, 0x00, 0x00, 0x00];
        let (f, used) = decode(&bytes).unwrap();
        assert_eq!(used, HEADER_LEN);
        assert!(f.payload.is_empty());
    }

    #[test]
    fn decode_reads_flag_byte() {
        let bytes = [0x80, 0x00, 0x00, 0x01, 0xAA];
        let (f, used) = decode(&bytes).unwrap();
        assert_eq!(used, 5);
        assert!(f.flags.is_compressed());
        assert!(!f.flags.is_encrypted());
        assert_eq!(f.payload, vec![0xAA]);
    }

    #[test]
    fn decode_length_63() {
        let mut bytes = vec![0x00, 0x00, 0x00, 0x3F];
        bytes.extend(vec![0xCC; 63]);
        let (f, used) = decode(&bytes).unwrap();
        assert_eq!(used, HEADER_LEN + 63);
        assert_eq!(f.payload.len(), 63);
    }

    #[test]
    fn decode_length_256() {
        let mut bytes = vec![0x00, 0x00, 0x01, 0x00];
        bytes.extend(vec![0x11; 256]);
        let (f, used) = decode(&bytes).unwrap();
        assert_eq!(used, HEADER_LEN + 256);
        assert_eq!(f.payload.len(), 256);
    }

    #[test]
    fn decode_length_65536() {
        let mut bytes = vec![0x00, 0x01, 0x00, 0x00];
        bytes.extend(vec![0u8; 65_536]);
        let (_, used) = decode(&bytes).unwrap();
        assert_eq!(used, HEADER_LEN + 65_536);
    }

    #[test]
    fn decode_incomplete_header_is_an_error() {
        assert_eq!(
            decode(&[]),
            Err(FrameError::Incomplete { needed: HEADER_LEN })
        );
        assert_eq!(
            decode(&[0x00]),
            Err(FrameError::Incomplete { needed: HEADER_LEN - 1 })
        );
        assert_eq!(
            decode(&[0x00, 0x00, 0x00]),
            Err(FrameError::Incomplete { needed: 1 })
        );
    }

    #[test]
    fn decode_incomplete_payload_is_an_error() {
        // Length advertised: 5, available: 2.
        let bytes = [0x00, 0x00, 0x00, 0x05, 0xAA, 0xBB];
        let err = decode(&bytes).unwrap_err();
        assert_eq!(err, FrameError::Incomplete { needed: 3 });
    }

    #[test]
    fn decode_consumes_only_one_frame_from_concatenated_buffer() {
        // Build buf = frame(0x80, [1,2,3]) + frame(0x00, [9]).
        let mut buf = encode(&frame(0x80, vec![1, 2, 3])).unwrap();
        let second = encode(&frame(0x00, vec![9])).unwrap();
        buf.extend(second.clone());
        let (f1, used) = decode(&buf).unwrap();
        assert!(f1.flags.is_compressed());
        assert_eq!(f1.payload, vec![1, 2, 3]);
        assert_eq!(used, HEADER_LEN + 3);
        let (f2, used2) = decode(&buf[used..]).unwrap();
        assert_eq!(f2.payload, vec![9]);
        assert_eq!(used2, HEADER_LEN + 1);
    }

    #[test]
    fn decode_round_trips_all_flag_combinations() {
        for flags in 0u8..=0xFFu8 {
            let f = frame(flags, vec![0xDE, 0xAD, 0xBE, 0xEF]);
            let bytes = encode(&f).unwrap();
            let (back, _used) = decode(&bytes).unwrap();
            assert_eq!(back, f, "round trip failed for flags 0x{flags:02x}");
        }
    }

    #[test]
    fn round_trip_lengths_at_boundaries() {
        for len in [0, 1, 63, 64, 255, 256, 4095, 4096, 65_535, 65_536, 1 << 20] {
            let payload = vec![0xA5; len];
            let bytes = encode(&Frame::plain(payload.clone())).unwrap();
            let (back, used) = decode(&bytes).unwrap();
            assert_eq!(back.payload, payload, "round trip failed for len={len}");
            assert_eq!(used, HEADER_LEN + len);
        }
    }

    #[test]
    fn round_trip_at_max_payload_len() {
        // Cheap check that doesn't actually allocate 16 MB: just verify the
        // header encodes properly with the maximum advertised length.
        let f = Frame {
            flags: FrameFlags::empty(),
            payload: vec![0u8; MAX_PAYLOAD_LEN],
        };
        let bytes = encode(&f).unwrap();
        assert_eq!(bytes[1..4], [0xFF, 0xFF, 0xFF]);
        let (back, _used) = decode(&bytes).unwrap();
        assert_eq!(back.payload.len(), MAX_PAYLOAD_LEN);
    }

    // ---------------- FrameFlags ----------------

    #[test]
    fn frame_flags_empty_has_no_bits() {
        let f = FrameFlags::empty();
        assert_eq!(f.bits(), 0);
        assert!(!f.is_compressed());
        assert!(!f.is_encrypted());
    }

    #[test]
    fn frame_flags_with_helpers_set_bits() {
        assert_eq!(FrameFlags::empty().with_compressed().bits(), FLAG_COMPRESSED);
        assert_eq!(FrameFlags::empty().with_encrypted().bits(), FLAG_ENCRYPTED);
        assert!(FrameFlags::empty().with_compressed().is_compressed());
        assert!(FrameFlags::empty().with_encrypted().is_encrypted());
    }

    #[test]
    fn frame_flags_combined() {
        let f = FrameFlags::empty().with_compressed().with_encrypted();
        assert!(f.is_compressed());
        assert!(f.is_encrypted());
        assert_eq!(f.bits(), FLAG_COMPRESSED | FLAG_ENCRYPTED);
    }

    #[test]
    fn frame_flags_from_bits_roundtrip() {
        for b in 0u8..=0xFFu8 {
            assert_eq!(FrameFlags::from_bits(b).bits(), b);
        }
    }

    #[test]
    fn frame_flags_debug_renders() {
        let s = format!("{:?}", FrameFlags::empty().with_compressed());
        assert!(s.contains("compressed"));
        let s = format!("{:?}", FrameFlags::empty().with_encrypted());
        assert!(s.contains("encrypted"));
    }

    #[test]
    fn frame_debug_renders_with_payload_len() {
        let f = Frame::plain(vec![1, 2, 3]);
        let s = format!("{f:?}");
        assert!(s.contains("payload_len"));
        assert!(s.contains('3'));
    }

    #[test]
    fn frame_plain_constructor_has_empty_flags() {
        let f = Frame::plain(vec![1]);
        assert_eq!(f.flags.bits(), 0);
        assert_eq!(f.payload, vec![1]);
    }

    #[test]
    fn frame_clone_eq() {
        let a = Frame::plain(vec![1, 2]);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn frame_error_display() {
        assert_eq!(
            format!("{}", FrameError::Incomplete { needed: 3 }),
            "incomplete frame: need 3 more byte(s)"
        );
        let s = format!("{}", FrameError::TooLarge(99));
        assert!(s.contains("99"));
        assert!(s.contains("payload too large"));
    }

    #[test]
    fn frame_error_eq_and_debug() {
        let a = FrameError::Incomplete { needed: 1 };
        let b = FrameError::Incomplete { needed: 1 };
        assert_eq!(a, b);
        let s = format!("{a:?}");
        assert!(s.contains("Incomplete"));
    }

    #[test]
    fn constants_align_with_spec() {
        assert_eq!(MAX_PAYLOAD_LEN, 0x00FF_FFFF);
        assert_eq!(HEADER_LEN, 4);
        assert_eq!(FLAG_COMPRESSED, 0x80);
        assert_eq!(FLAG_ENCRYPTED, 0x02);
    }
}
