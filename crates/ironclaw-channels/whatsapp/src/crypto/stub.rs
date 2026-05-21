//! No-op crypto backend.
//!
//! [`StubBackend`] returns [`CryptoError::NotImplemented`] from every
//! primitive. The default [`crate::WhatsAppAdapter`] uses this backend,
//! and surfaces the resulting failures as
//! [`ironclaw_channels_core::AdapterError::Unsupported`] from `deliver`,
//! `edit_message`, `add_reaction`, and `set_typing`.
//!
//! A future contributor wiring real crypto should:
//!
//! 1. Implement [`crate::crypto::CryptoBackend`] against
//!    `libsignal-protocol` (or another library — the trait is
//!    library-agnostic).
//! 2. Construct the adapter with
//!    [`crate::WhatsAppAdapter::with_crypto_backend`].

use super::{CryptoBackend, CryptoError, KeyPair};

/// The default no-op [`CryptoBackend`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StubBackend;

impl CryptoBackend for StubBackend {
    fn generate_keypair(&self) -> Result<KeyPair, CryptoError> {
        Err(CryptoError::NotImplemented("generate_keypair"))
    }

    fn dh(&self, _priv_key: &[u8], _pub_key: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::NotImplemented("dh"))
    }

    fn hkdf_extract(&self, _salt: &[u8], _ikm: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::NotImplemented("hkdf_extract"))
    }

    fn hkdf_expand(
        &self,
        _prk: &[u8],
        _info: &[u8],
        _length: usize,
    ) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::NotImplemented("hkdf_expand"))
    }

    fn aead_seal(
        &self,
        _key: &[u8],
        _nonce: &[u8],
        _ad: &[u8],
        _plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::NotImplemented("aead_seal"))
    }

    fn aead_open(
        &self,
        _key: &[u8],
        _nonce: &[u8],
        _ad: &[u8],
        _ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::NotImplemented("aead_open"))
    }

    fn sign(&self, _priv_key: &[u8], _data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Err(CryptoError::NotImplemented("sign"))
    }

    fn verify(
        &self,
        _pub_key: &[u8],
        _data: &[u8],
        _sig: &[u8],
    ) -> Result<bool, CryptoError> {
        Err(CryptoError::NotImplemented("verify"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op_name(err: CryptoError) -> &'static str {
        match err {
            CryptoError::NotImplemented(s) => s,
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    #[test]
    fn generate_keypair_is_not_implemented() {
        assert_eq!(op_name(StubBackend.generate_keypair().unwrap_err()), "generate_keypair");
    }

    #[test]
    fn dh_is_not_implemented() {
        assert_eq!(op_name(StubBackend.dh(b"", b"").unwrap_err()), "dh");
    }

    #[test]
    fn hkdf_extract_is_not_implemented() {
        assert_eq!(op_name(StubBackend.hkdf_extract(b"", b"").unwrap_err()), "hkdf_extract");
    }

    #[test]
    fn hkdf_expand_is_not_implemented() {
        assert_eq!(
            op_name(StubBackend.hkdf_expand(b"", b"", 32).unwrap_err()),
            "hkdf_expand"
        );
    }

    #[test]
    fn aead_seal_is_not_implemented() {
        assert_eq!(
            op_name(
                StubBackend
                    .aead_seal(b"k", b"n", b"a", b"p")
                    .unwrap_err()
            ),
            "aead_seal"
        );
    }

    #[test]
    fn aead_open_is_not_implemented() {
        assert_eq!(
            op_name(
                StubBackend
                    .aead_open(b"k", b"n", b"a", b"c")
                    .unwrap_err()
            ),
            "aead_open"
        );
    }

    #[test]
    fn sign_is_not_implemented() {
        assert_eq!(op_name(StubBackend.sign(b"", b"").unwrap_err()), "sign");
    }

    #[test]
    fn verify_is_not_implemented() {
        assert_eq!(op_name(StubBackend.verify(b"", b"", b"").unwrap_err()), "verify");
    }

    #[test]
    fn stub_backend_is_default_and_copy() {
        let a = StubBackend;
        let b = a;
        let c = StubBackend;
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn stub_backend_debug() {
        assert_eq!(format!("{StubBackend:?}"), "StubBackend");
    }

    #[test]
    fn stub_backend_default_constructor() {
        let _ = StubBackend;
        let _ = StubBackend;
    }
}
