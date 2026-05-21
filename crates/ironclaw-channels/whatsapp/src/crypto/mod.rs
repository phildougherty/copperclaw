//! Cryptographic backend trait for the WhatsApp channel.
//!
//! The trait exposes the minimum surface the higher layers need:
//!
//! - Curve25519 keypair generation (for the device identity, the
//!   ephemeral handshake key, and per-session pre-keys).
//! - X25519 Diffie-Hellman.
//! - HKDF extract / expand.
//! - AES-GCM (or any AEAD) seal / open.
//! - Ed25519 sign / verify (the device identity is also a signing key in
//!   WhatsApp's Signal session bring-up).
//!
//! The Noise XX state machine and the outbound encryption path both
//! consume operations through this trait, so a future contributor can
//! drop in `libsignal-protocol` or `snow + aes-gcm + curve25519-dalek`
//! without touching the layers above.
//!
//! ## What about Signal session state?
//!
//! Signal session state (ratchet chain, ephemerals, message keys) is
//! deliberately **not** exposed on this trait. The backend owns its own
//! session store internally; the channel adapter sees only opaque
//! plaintext/ciphertext on the boundaries.
//!
//! ## Backends
//!
//! [`dalek::DalekBackend`] is the wired-in default. It implements the
//! primitives against the audited `x25519-dalek`, `ed25519-dalek`,
//! `hkdf`, and `aes-gcm` crates.
//!
//! [`stub::StubBackend`] is retained for tests that want to exercise
//! the "no crypto installed" path: every method on the stub returns
//! [`CryptoError::NotImplemented`].
//!
//! Note: the `DalekBackend` only ships the cryptographic primitives.
//! The Signal Protocol session-state machinery (X3DH, Double Ratchet,
//! sender keys, message envelope construction) sits **above** this
//! trait and is still deferred — see
//! [`crate::adapter::WhatsAppAdapter::deliver`] for the gating point.

pub mod dalek;
pub mod stub;

pub use dalek::DalekBackend;
pub use stub::StubBackend;

/// Errors from a [`CryptoBackend`] operation.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum CryptoError {
    /// The backend does not implement this operation. The argument is
    /// the operation name as a static string, intended for log messages.
    #[error("crypto operation `{0}` not implemented")]
    NotImplemented(&'static str),
    /// The backend rejected the inputs (bad key length, bad nonce, etc.).
    #[error("crypto: invalid input: {0}")]
    InvalidInput(String),
    /// AEAD authentication failed (decryption produced a tag mismatch).
    #[error("crypto: authentication failed")]
    AuthenticationFailed,
}

/// A Curve25519 / Ed25519 keypair as raw bytes.
///
/// `public` and `private` are the canonical encodings (32 bytes each for
/// Curve25519 / Ed25519).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPair {
    pub public: Vec<u8>,
    pub private: Vec<u8>,
}

impl KeyPair {
    /// Construct a keypair from raw bytes.
    pub fn new(public: impl Into<Vec<u8>>, private: impl Into<Vec<u8>>) -> Self {
        Self {
            public: public.into(),
            private: private.into(),
        }
    }
}

/// The trait that the WhatsApp channel layers consume.
///
/// Every method takes byte slices and returns byte vectors so the trait
/// stays free of Curve25519 / Signal Protocol types — a future
/// contributor can implement this against `libsignal-protocol`,
/// `snow + curve25519-dalek + aes-gcm + hkdf + ed25519-dalek`, ring,
/// or any other library.
///
/// All operations are **infallible by intent** in a real backend: only
/// truly exceptional conditions (e.g. a tag mismatch, an invalid key
/// length) should error. The stub backend returns
/// [`CryptoError::NotImplemented`] from every method.
pub trait CryptoBackend: Send + Sync {
    /// Generate a fresh Curve25519 keypair suitable for an X25519 DH or
    /// for use as a Signal pre-key.
    fn generate_keypair(&self) -> Result<KeyPair, CryptoError>;

    /// X25519 Diffie-Hellman. Returns the 32-byte shared secret.
    fn dh(&self, priv_key: &[u8], pub_key: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// HKDF extract. Returns the 32-byte PRK.
    fn hkdf_extract(&self, salt: &[u8], ikm: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// HKDF expand. Returns the requested number of bytes.
    fn hkdf_expand(
        &self,
        prk: &[u8],
        info: &[u8],
        length: usize,
    ) -> Result<Vec<u8>, CryptoError>;

    /// AEAD seal (AES-256-GCM in WhatsApp's case). Returns
    /// `ciphertext || tag`.
    fn aead_seal(
        &self,
        key: &[u8],
        nonce: &[u8],
        ad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError>;

    /// AEAD open. Returns the plaintext, or
    /// [`CryptoError::AuthenticationFailed`] on a tag mismatch.
    fn aead_open(
        &self,
        key: &[u8],
        nonce: &[u8],
        ad: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError>;

    /// Ed25519 signature. Returns the 64-byte signature.
    fn sign(&self, priv_key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// Ed25519 verify. Returns `Ok(true)` for a good signature,
    /// `Ok(false)` for a bad signature, or an error for malformed
    /// inputs.
    fn verify(&self, pub_key: &[u8], data: &[u8], sig: &[u8]) -> Result<bool, CryptoError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_new_clones_bytes() {
        let kp = KeyPair::new(vec![1, 2], vec![3, 4]);
        assert_eq!(kp.public, vec![1, 2]);
        assert_eq!(kp.private, vec![3, 4]);
    }

    #[test]
    fn keypair_clone_and_eq() {
        let kp = KeyPair::new(vec![1], vec![2]);
        let copy = kp.clone();
        assert_eq!(kp, copy);
    }

    #[test]
    fn keypair_debug_renders() {
        let kp = KeyPair::new(vec![1], vec![2]);
        assert!(format!("{kp:?}").contains("KeyPair"));
    }

    #[test]
    fn crypto_error_displays() {
        let e = CryptoError::NotImplemented("dh");
        assert!(format!("{e}").contains("dh"));
        let e = CryptoError::InvalidInput("bad".into());
        assert!(format!("{e}").contains("bad"));
        let e = CryptoError::AuthenticationFailed;
        assert_eq!(format!("{e}"), "crypto: authentication failed");
    }

    #[test]
    fn crypto_error_eq_and_clone_and_debug() {
        let a = CryptoError::NotImplemented("x");
        let b = CryptoError::NotImplemented("x");
        assert_eq!(a, b);
        let c = a.clone();
        assert_eq!(a, c);
        assert!(format!("{a:?}").contains("NotImplemented"));
    }
}
