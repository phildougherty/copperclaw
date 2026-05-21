#![allow(
    // The "WhatsApp", "WebSocket", "Curve25519", "Signal", and similar
    // proper nouns appear throughout the module docs; backticking each
    // would make the prose harder to scan.
    clippy::doc_markdown,
    // The wire format encodes lengths into `u24` and `u20` fields via
    // explicit `(len >> 8) as u8` shifts; the truncation is correct by
    // construction and double-guarded by an explicit length check above.
    clippy::cast_possible_truncation,
)]

//! WhatsApp channel adapter — native (Baileys-style) WebSocket gateway.
//!
//! This crate implements the **transport, framing, and protocol state
//! machine** for WhatsApp's reverse-engineered WebSocket interface (the same
//! surface the open-source [Baileys] TypeScript client speaks). It is the
//! native counterpart to the [`whatsapp-cloud`] crate, which targets Meta's
//! Cloud API and is unrelated.
//!
//! [Baileys]: https://github.com/WhiskeySockets/Baileys
//! [`whatsapp-cloud`]: ../ironclaw_channels_whatsapp_cloud/index.html
//!
//! # Scope — important
//!
//! Implementing the full WhatsApp protocol requires a complete Signal
//! Protocol stack (Curve25519 keypairs, X3DH, Double Ratchet, Sender Keys
//! for groups, the Noise XX handshake, and AES-GCM authenticated framing).
//! That work is a multi-week effort and is **explicitly out of scope** for
//! this slice.
//!
//! This crate delivers everything **above** the cryptographic boundary:
//!
//! - The WebSocket transport, including connect/heartbeat/reconnect.
//! - The post-handshake **frame codec** (`[flags][u24 length][payload]`).
//! - The **WhatsApp binary XML** tag/attribute encoder and decoder
//!   (enough of the tag table to round-trip an `iq` ping/pong; the full
//!   table is documented as deferred).
//! - The **Noise XX handshake state machine** — message ordering and
//!   transitions are tested; the actual cryptographic operations are
//!   delegated to a pluggable [`CryptoBackend`] trait.
//! - **Keystore persistence** — atomic-write JSON file for device identity
//!   plus session state, with corruption recovery.
//! - The **gateway lifecycle** (connect, heartbeat, reconnect, cancellation)
//!   driven through a [`WsTransport`] trait so tests never open a real
//!   socket.
//! - **Inbound event parsing** of already-decrypted payloads into
//!   [`ironclaw_types::InboundEvent`].
//! - A real [`crypto::DalekBackend`] (X25519 / HKDF-SHA256 /
//!   AES-256-GCM / Ed25519) is now the wired-in default, backed by the
//!   audited `x25519-dalek`, `ed25519-dalek`, `hkdf`, and `aes-gcm`
//!   crates. The legacy no-op [`crypto::StubBackend`] is retained for
//!   tests that want the "no crypto installed" codepath. Outbound
//!   `deliver` still returns [`AdapterError::Unsupported`] even with
//!   the real backend installed because the Signal Protocol session
//!   state machine (X3DH + Double Ratchet + sender keys + the message
//!   envelope construction) that sits **above** the primitives is a
//!   separate piece of work and has not been written yet.
//!
//! [`AdapterError`]: ironclaw_channels_core::AdapterError
//! [`AdapterError::Unsupported`]: ironclaw_channels_core::AdapterError::Unsupported
//! [`CryptoBackend`]: crate::crypto::CryptoBackend
//! [`WsTransport`]: crate::gateway::transport::WsTransport
//!
//! # Plugging in a real crypto backend
//!
//! A future contributor wiring up real e2e crypto should:
//!
//! 1. Add a crate (e.g. depending on `libsignal-protocol` or
//!    `snow` + `aes-gcm` + `curve25519-dalek` + `hkdf` + `sha2`).
//! 2. Implement [`crypto::CryptoBackend`] over those primitives. The trait
//!    surface is tight: keypair generation, X25519 DH, HKDF
//!    extract/expand, AEAD seal/open, Ed25519 sign/verify. All inputs are
//!    `&[u8]`; all outputs are `Vec<u8>`. There are no Signal Protocol
//!    types in the trait — the Signal session state lives behind it.
//! 3. Construct the adapter with [`WhatsAppAdapter::with_crypto_backend`].
//! 4. Implement the encryption side of `deliver` by composing the
//!    backend's primitives.
//!
//! # `platform_id` shapes
//!
//! - `"user:<wa_id>"` — direct message to one WhatsApp user. The `wa_id`
//!   is the e164 phone number without the leading `+`, exactly as the
//!   protocol carries it.
//! - `"group:<group_jid>"` — group chat. The JID has the form
//!   `<creator>-<timestamp>` per WhatsApp's group-id convention; we treat
//!   it as an opaque string.
//!
//! Other shapes surface as [`AdapterError::BadRequest`].
//!
//! # Unsupported v1 surfaces
//!
//! Until a real [`CryptoBackend`] is plugged in:
//!
//! - All outbound delivery returns [`AdapterError::Unsupported`].
//! - `edit_message`, `add_reaction`, `set_typing` return
//!   [`AdapterError::Unsupported`]. WhatsApp supports these operations,
//!   but only with the encrypted protocol stack.
//! - QR-code pairing is out of scope. The config can carry already-paired
//!   credentials in the keystore JSON; pairing must be done by external
//!   tooling and the resulting state placed at
//!   `data_dir/whatsapp_keystore.json`.
//!
//! [`CryptoBackend`]: crate::crypto::CryptoBackend
//!
//! # Module layout
//!
//! - [`config`] — `WhatsAppConfig`, JSON parser.
//! - [`keystore`] — atomic-write JSON keystore for device identity / session.
//! - [`wire`] — protocol framing.
//!     - [`wire::frame`] — `[flags][u24 length][payload]` codec.
//!     - [`wire::binary_xml`] — WhatsApp's binary XML encoding.
//!     - [`wire::noise`] — Noise XX handshake state machine.
//! - [`crypto`] — `CryptoBackend` trait and the no-op `StubBackend`.
//! - [`gateway`] — WebSocket plumbing.
//!     - [`gateway::transport`] — `WsTransport` trait, real and mock impls.
//!     - [`gateway::lifecycle`] — connect/heartbeat/reconnect state.
//!     - [`gateway::codec`] — frame-to-event mapping.
//! - [`parse`] — already-decrypted payload to `InboundEvent`.
//! - [`adapter`] — `WhatsAppAdapter` itself.
//! - [`factory`] — `WhatsAppFactory` + `register`.
//! - [`testing`] — test-only re-exports (`MockTransport`, `StubBackend`,
//!   fixture builders) for downstream tests.

pub mod adapter;
pub mod config;
pub mod crypto;
pub mod factory;
pub mod gateway;
pub mod keystore;
pub mod parse;
pub mod testing;
pub mod wire;

pub use adapter::WhatsAppAdapter;
pub use config::WhatsAppConfig;
pub use factory::{CHANNEL_TYPE_STR, WhatsAppFactory, register};

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_channels_core::ChannelRegistry;
    use ironclaw_types::ChannelType;

    #[test]
    fn channel_type_str_is_whatsapp() {
        assert_eq!(CHANNEL_TYPE_STR, "whatsapp");
    }

    #[test]
    fn register_inserts_factory() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("whatsapp")).is_some());
    }

    #[test]
    fn channel_type_is_distinct_from_whatsapp_cloud() {
        // Defensive: the two channels share a name prefix and must remain
        // distinct identifiers.
        assert_ne!(CHANNEL_TYPE_STR, "whatsapp-cloud");
    }
}
