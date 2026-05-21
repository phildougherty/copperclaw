//! WhatsApp wire-protocol module group.
//!
//! - [`frame`] — the post-handshake `[flags][u24 length][payload]` codec
//!   that all WhatsApp Web frames live inside.
//! - [`binary_xml`] — the WhatsApp binary XML (tag-soup) encoder/decoder.
//!   Implemented to the extent needed to round-trip a small set of
//!   `iq` queries; the full token table is documented as deferred work.
//! - [`noise`] — the Noise XX handshake state machine. Pure state
//!   transitions; the cryptographic operations are delegated to a
//!   [`CryptoBackend`].
//!
//! [`CryptoBackend`]: crate::crypto::CryptoBackend

pub mod binary_xml;
pub mod frame;
pub mod noise;
