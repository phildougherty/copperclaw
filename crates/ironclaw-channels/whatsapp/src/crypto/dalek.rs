//! Real [`CryptoBackend`] implementation backed by well-known,
//! audited Rust crypto crates.
//!
//! Algorithms:
//!
//! - **Curve25519 / X25519** via [`x25519_dalek`]
//!   (`StaticSecret` + `PublicKey`) for keypair generation and ECDH.
//! - **HKDF-SHA256** via [`hkdf`] (RFC 5869) for the key schedule.
//! - **AES-256-GCM** via [`aes_gcm`] for AEAD seal / open. The trait
//!   contract requires a **32-byte** key and a **12-byte** nonce.
//! - **Ed25519** via [`ed25519_dalek`] for sign / verify.
//!
//! ## Wire-level shape
//!
//! For the [`crate::crypto::KeyPair`] type returned by
//! [`DalekBackend::generate_keypair`]:
//!
//! - `private` is the raw 32-byte `StaticSecret` (clamp is applied at
//!   use time inside `x25519-dalek`; the bytes here are **before** clamping).
//! - `public` is the 32-byte clamped public key, the canonical
//!   little-endian Curve25519 point encoding suitable for the wire.
//!
//! Anyone integrating this backend with the WhatsApp / Signal handshake
//! should treat `private` as the secret scalar (in its `StaticSecret`
//! pre-clamped form) and `public` as the X25519 public point.
//!
//! ## What this backend does NOT do
//!
//! The Signal Protocol session state machine (X3DH, Double Ratchet,
//! sender keys, the message envelope construction) is a separate piece
//! of work that sits **above** this trait. This backend supplies only
//! the cryptographic primitives.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key as AesKey, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand_core::OsRng;
use sha2::Sha256;
use x25519_dalek::{PublicKey as XPublicKey, StaticSecret};

use super::{CryptoBackend, CryptoError, KeyPair};

/// Expected lengths for the various key / nonce / signature byte
/// strings the trait accepts. Centralised so error messages and tests
/// stay consistent.
pub(crate) const X25519_KEY_LEN: usize = 32;
pub(crate) const ED25519_SECRET_LEN: usize = 32;
pub(crate) const ED25519_PUBLIC_LEN: usize = 32;
pub(crate) const ED25519_SIGNATURE_LEN: usize = 64;
pub(crate) const AES256_KEY_LEN: usize = 32;
pub(crate) const AES_GCM_NONCE_LEN: usize = 12;

/// HKDF-SHA256 maximum output length: `255 * HashLen` where
/// `HashLen = 32` (SHA-256 digest size). See RFC 5869 § 2.3.
pub(crate) const HKDF_SHA256_MAX_OUTPUT: usize = 255 * 32;

/// The real [`CryptoBackend`].
///
/// Holds no state — every operation derives its randomness from
/// [`OsRng`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DalekBackend;

impl DalekBackend {
    /// Construct a fresh backend. Equivalent to `DalekBackend::default()`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn invalid<S: Into<String>>(msg: S) -> CryptoError {
    CryptoError::InvalidInput(msg.into())
}

fn ensure_len(label: &str, got: usize, want: usize) -> Result<(), CryptoError> {
    if got == want {
        Ok(())
    } else {
        Err(invalid(format!(
            "{label}: expected {want} bytes, got {got}"
        )))
    }
}

fn to_array32(label: &str, bytes: &[u8]) -> Result<[u8; 32], CryptoError> {
    ensure_len(label, bytes.len(), 32)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Ok(out)
}

impl CryptoBackend for DalekBackend {
    fn generate_keypair(&self) -> Result<KeyPair, CryptoError> {
        // `StaticSecret::random_from_rng` keeps the raw secret bytes
        // accessible via `to_bytes` so callers can persist them; the
        // alternative `EphemeralSecret` is *deliberately* unsuitable
        // because it consumes itself on the DH operation and never
        // exposes its scalar.
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = XPublicKey::from(&secret);
        Ok(KeyPair::new(
            public.as_bytes().to_vec(),
            secret.to_bytes().to_vec(),
        ))
    }

    fn dh(&self, priv_key: &[u8], pub_key: &[u8]) -> Result<Vec<u8>, CryptoError> {
        ensure_len("x25519 private key", priv_key.len(), X25519_KEY_LEN)?;
        ensure_len("x25519 public key", pub_key.len(), X25519_KEY_LEN)?;
        let priv_bytes = to_array32("x25519 private key", priv_key)?;
        let pub_bytes = to_array32("x25519 public key", pub_key)?;
        let secret = StaticSecret::from(priv_bytes);
        let public = XPublicKey::from(pub_bytes);
        let shared = secret.diffie_hellman(&public);
        Ok(shared.as_bytes().to_vec())
    }

    fn hkdf_extract(&self, salt: &[u8], ikm: &[u8]) -> Result<Vec<u8>, CryptoError> {
        // `Hkdf::<Sha256>::extract` returns `(prk, hkdf)`; we want just
        // the PRK bytes. A `None` salt is acceptable per RFC 5869
        // (treated as `HashLen` zeros) — pass the slice through.
        let salt_opt = if salt.is_empty() { None } else { Some(salt) };
        let (prk, _hkdf) = Hkdf::<Sha256>::extract(salt_opt, ikm);
        Ok(prk.to_vec())
    }

    fn hkdf_expand(
        &self,
        prk: &[u8],
        info: &[u8],
        length: usize,
    ) -> Result<Vec<u8>, CryptoError> {
        if length == 0 {
            return Ok(Vec::new());
        }
        if length > HKDF_SHA256_MAX_OUTPUT {
            return Err(invalid(format!(
                "hkdf_expand: length {length} exceeds HKDF-SHA256 limit {HKDF_SHA256_MAX_OUTPUT}"
            )));
        }
        // `from_prk` rejects PRKs shorter than the hash output length
        // (32 bytes for SHA-256), which mirrors the RFC's requirement.
        let hkdf = Hkdf::<Sha256>::from_prk(prk).map_err(|_| {
            invalid(format!(
                "hkdf_expand: PRK must be at least 32 bytes, got {}",
                prk.len()
            ))
        })?;
        let mut out = vec![0u8; length];
        hkdf.expand(info, &mut out)
            .map_err(|e| invalid(format!("hkdf_expand: {e}")))?;
        Ok(out)
    }

    fn aead_seal(
        &self,
        key: &[u8],
        nonce: &[u8],
        ad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        ensure_len("aes-256-gcm key", key.len(), AES256_KEY_LEN)?;
        ensure_len("aes-gcm nonce", nonce.len(), AES_GCM_NONCE_LEN)?;
        let cipher = Aes256Gcm::new(AesKey::<Aes256Gcm>::from_slice(key));
        cipher
            .encrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad: ad,
                },
            )
            .map_err(|_| invalid("aead_seal: encryption failed"))
    }

    fn aead_open(
        &self,
        key: &[u8],
        nonce: &[u8],
        ad: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        ensure_len("aes-256-gcm key", key.len(), AES256_KEY_LEN)?;
        ensure_len("aes-gcm nonce", nonce.len(), AES_GCM_NONCE_LEN)?;
        let cipher = Aes256Gcm::new(AesKey::<Aes256Gcm>::from_slice(key));
        cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: ad,
                },
            )
            .map_err(|_| CryptoError::AuthenticationFailed)
    }

    fn sign(&self, priv_key: &[u8], data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        ensure_len("ed25519 secret key", priv_key.len(), ED25519_SECRET_LEN)?;
        let secret = to_array32("ed25519 secret key", priv_key)?;
        let signing = SigningKey::from_bytes(&secret);
        let sig: Signature = signing.sign(data);
        Ok(sig.to_bytes().to_vec())
    }

    fn verify(
        &self,
        pub_key: &[u8],
        data: &[u8],
        sig: &[u8],
    ) -> Result<bool, CryptoError> {
        ensure_len("ed25519 public key", pub_key.len(), ED25519_PUBLIC_LEN)?;
        let pub_bytes = to_array32("ed25519 public key", pub_key)?;
        ensure_len("ed25519 signature", sig.len(), ED25519_SIGNATURE_LEN)?;
        let verifying = VerifyingKey::from_bytes(&pub_bytes)
            .map_err(|e| invalid(format!("ed25519 public key: {e}")))?;
        let mut sig_arr = [0u8; ED25519_SIGNATURE_LEN];
        sig_arr.copy_from_slice(sig);
        let signature = Signature::from_bytes(&sig_arr);
        Ok(verifying.verify(data, &signature).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a hex string, panicking with context on failure. Lets the
    /// RFC vectors live as readable hex literals.
    fn hex(s: &str) -> Vec<u8> {
        let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        hex_decode(&cleaned).unwrap_or_else(|e| panic!("bad hex literal: {e}: {s:?}"))
    }

    fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
        if s.len() % 2 != 0 {
            return Err(format!("odd length: {}", s.len()));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&s[i..i + 2], 16)
                    .map_err(|e| format!("at {i}: {e}"))
            })
            .collect()
    }

    fn ed25519_public_from_secret(secret_bytes: &[u8; 32]) -> Vec<u8> {
        SigningKey::from_bytes(secret_bytes)
            .verifying_key()
            .to_bytes()
            .to_vec()
    }

    // -------- generate_keypair --------

    #[test]
    fn generate_keypair_returns_32_byte_keys() {
        let kp = DalekBackend.generate_keypair().unwrap();
        assert_eq!(kp.public.len(), 32);
        assert_eq!(kp.private.len(), 32);
    }

    #[test]
    fn generate_keypair_yields_distinct_keys_each_call() {
        let a = DalekBackend.generate_keypair().unwrap();
        let b = DalekBackend.generate_keypair().unwrap();
        // OsRng is not deterministic; an unlucky collision here would
        // mean the OS RNG is broken — which is fine to fail loudly on.
        assert_ne!(a.public, b.public);
        assert_ne!(a.private, b.private);
    }

    #[test]
    fn generate_keypair_priv_derives_to_returned_pub() {
        let kp = DalekBackend.generate_keypair().unwrap();
        let priv_bytes: [u8; 32] = kp.private.as_slice().try_into().unwrap();
        let derived = XPublicKey::from(&StaticSecret::from(priv_bytes));
        assert_eq!(derived.as_bytes().as_slice(), kp.public.as_slice());
    }

    #[test]
    fn generate_keypair_round_trip_dh_matches() {
        // Two locally-generated keypairs should agree on a shared
        // secret regardless of direction.
        let a = DalekBackend.generate_keypair().unwrap();
        let b = DalekBackend.generate_keypair().unwrap();
        let s1 = DalekBackend.dh(&a.private, &b.public).unwrap();
        let s2 = DalekBackend.dh(&b.private, &a.public).unwrap();
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), 32);
    }

    // -------- dh: RFC 7748 § 5.2 known answer vectors --------

    #[test]
    fn dh_rfc7748_section_5_2_first_vector() {
        // Scalar
        let scalar = hex(
            "a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4",
        );
        // u-coordinate
        let u = hex(
            "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c",
        );
        // Expected output u-coordinate
        let want = hex(
            "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552",
        );
        let got = DalekBackend.dh(&scalar, &u).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn dh_rfc7748_section_5_2_second_vector() {
        let scalar = hex(
            "4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d",
        );
        let u = hex(
            "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493",
        );
        let want = hex(
            "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957",
        );
        let got = DalekBackend.dh(&scalar, &u).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn dh_rejects_short_private_key() {
        let err = DalekBackend.dh(&[0u8; 31], &[0u8; 32]).unwrap_err();
        match err {
            CryptoError::InvalidInput(m) => assert!(m.contains("private")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn dh_rejects_long_private_key() {
        let err = DalekBackend.dh(&[0u8; 33], &[0u8; 32]).unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn dh_rejects_empty_private_key() {
        let err = DalekBackend.dh(&[], &[0u8; 32]).unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn dh_rejects_short_public_key() {
        let err = DalekBackend.dh(&[0u8; 32], &[0u8; 31]).unwrap_err();
        match err {
            CryptoError::InvalidInput(m) => assert!(m.contains("public")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn dh_rejects_long_public_key() {
        let err = DalekBackend.dh(&[0u8; 32], &[0u8; 33]).unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn dh_rejects_empty_public_key() {
        let err = DalekBackend.dh(&[0u8; 32], &[]).unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn dh_returns_32_byte_secret() {
        let a = DalekBackend.generate_keypair().unwrap();
        let b = DalekBackend.generate_keypair().unwrap();
        let s = DalekBackend.dh(&a.private, &b.public).unwrap();
        assert_eq!(s.len(), 32);
    }

    // -------- hkdf_extract: RFC 5869 A.1 + A.2 --------

    #[test]
    fn hkdf_extract_rfc5869_a1() {
        let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = hex("000102030405060708090a0b0c");
        let want = hex(
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
        );
        let prk = DalekBackend.hkdf_extract(&salt, &ikm).unwrap();
        assert_eq!(prk, want);
    }

    #[test]
    fn hkdf_extract_rfc5869_a2() {
        let ikm = hex(
            "000102030405060708090a0b0c0d0e0f
             101112131415161718191a1b1c1d1e1f
             202122232425262728292a2b2c2d2e2f
             303132333435363738393a3b3c3d3e3f
             404142434445464748494a4b4c4d4e4f",
        );
        let salt = hex(
            "606162636465666768696a6b6c6d6e6f
             707172737475767778797a7b7c7d7e7f
             808182838485868788898a8b8c8d8e8f
             909192939495969798999a9b9c9d9e9f
             a0a1a2a3a4a5a6a7a8a9aaabacadaeaf",
        );
        let want = hex(
            "06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244",
        );
        let prk = DalekBackend.hkdf_extract(&salt, &ikm).unwrap();
        assert_eq!(prk, want);
    }

    #[test]
    fn hkdf_extract_empty_salt_matches_a3_vector() {
        // RFC 5869 A.3: empty salt, ikm = 22 zero bytes? Actually A.3 ikm = 0b*22 with empty salt.
        let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = b"";
        let want = hex(
            "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
        );
        let prk = DalekBackend.hkdf_extract(salt, &ikm).unwrap();
        assert_eq!(prk, want);
    }

    #[test]
    fn hkdf_extract_returns_32_bytes() {
        let prk = DalekBackend.hkdf_extract(b"salt", b"ikm").unwrap();
        assert_eq!(prk.len(), 32);
    }

    // -------- hkdf_expand: RFC 5869 A.1 + A.2 --------

    #[test]
    fn hkdf_expand_rfc5869_a1() {
        let prk = hex(
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
        );
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        let want = hex(
            "3cb25f25faacd57a90434f64d0362f2a
             2d2d0a90cf1a5a4c5db02d56ecc4c5bf
             34007208d5b887185865",
        );
        let got = DalekBackend.hkdf_expand(&prk, &info, 42).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn hkdf_expand_rfc5869_a2() {
        let prk = hex(
            "06a6b88c5853361a06104c9ceb35b45c
             ef760014904671014a193f40c15fc244",
        );
        let info = hex(
            "b0b1b2b3b4b5b6b7b8b9babbbcbdbebf
             c0c1c2c3c4c5c6c7c8c9cacbcccdcecf
             d0d1d2d3d4d5d6d7d8d9dadbdcdddedf
             e0e1e2e3e4e5e6e7e8e9eaebecedeeef
             f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
        );
        let want = hex(
            "b11e398dc80327a1c8e7f78c596a4934
             4f012eda2d4efad8a050cc4c19afa97c
             59045a99cac7827271cb41c65e590e09
             da3275600c2f09b8367793a9aca3db71
             cc30c58179ec3e87c14c01d5c1f3434f
             1d87",
        );
        let got = DalekBackend.hkdf_expand(&prk, &info, 82).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn hkdf_expand_rfc5869_a3_empty_info() {
        // A.3 vector: PRK from A.3 extract, empty info, L=42.
        let prk = hex(
            "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
        );
        let want = hex(
            "8da4e775a563c18f715f802a063c5a31
             b8a11f5c5ee1879ec3454e5f3c738d2d
             9d201395faa4b61a96c8",
        );
        let got = DalekBackend.hkdf_expand(&prk, b"", 42).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn hkdf_expand_zero_length_is_empty_vec() {
        let prk = vec![0u8; 32];
        let out = DalekBackend.hkdf_expand(&prk, b"info", 0).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn hkdf_expand_rejects_oversized_length() {
        let prk = vec![0u8; 32];
        let err = DalekBackend
            .hkdf_expand(&prk, b"info", HKDF_SHA256_MAX_OUTPUT + 1)
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn hkdf_expand_accepts_max_length() {
        let prk = vec![0u8; 32];
        let out = DalekBackend
            .hkdf_expand(&prk, b"info", HKDF_SHA256_MAX_OUTPUT)
            .unwrap();
        assert_eq!(out.len(), HKDF_SHA256_MAX_OUTPUT);
    }

    #[test]
    fn hkdf_expand_rejects_short_prk() {
        let err = DalekBackend
            .hkdf_expand(&[0u8; 16], b"info", 32)
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn hkdf_expand_short_info_changes_output() {
        let prk = vec![1u8; 32];
        let a = DalekBackend.hkdf_expand(&prk, b"info-a", 16).unwrap();
        let b = DalekBackend.hkdf_expand(&prk, b"info-b", 16).unwrap();
        assert_ne!(a, b);
    }

    // -------- AEAD round-trip + tamper detection --------

    fn fresh_aes_key() -> [u8; 32] {
        use rand::RngCore;
        let mut k = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut k);
        k
    }

    fn fresh_nonce() -> [u8; 12] {
        use rand::RngCore;
        let mut n = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut n);
        n
    }

    #[test]
    fn aead_round_trip_no_ad() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let pt = b"the quick brown fox";
        let ct = DalekBackend.aead_seal(&key, &nonce, b"", pt).unwrap();
        assert_ne!(ct, pt);
        let out = DalekBackend.aead_open(&key, &nonce, b"", &ct).unwrap();
        assert_eq!(out, pt);
    }

    #[test]
    fn aead_round_trip_with_ad() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let pt = b"payload";
        let ad = b"additional-authenticated-data";
        let ct = DalekBackend.aead_seal(&key, &nonce, ad, pt).unwrap();
        let out = DalekBackend.aead_open(&key, &nonce, ad, &ct).unwrap();
        assert_eq!(out, pt);
    }

    #[test]
    fn aead_round_trip_empty_plaintext() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let ct = DalekBackend.aead_seal(&key, &nonce, b"a", b"").unwrap();
        // empty plaintext → 16-byte tag only.
        assert_eq!(ct.len(), 16);
        let out = DalekBackend.aead_open(&key, &nonce, b"a", &ct).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn aead_ciphertext_length_is_plaintext_plus_tag() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let pt = vec![0u8; 100];
        let ct = DalekBackend.aead_seal(&key, &nonce, b"", &pt).unwrap();
        assert_eq!(ct.len(), pt.len() + 16);
    }

    #[test]
    fn aead_tamper_in_ciphertext_fails_authentication() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let pt = b"important";
        let mut ct = DalekBackend.aead_seal(&key, &nonce, b"", pt).unwrap();
        ct[0] ^= 0x01;
        let err = DalekBackend.aead_open(&key, &nonce, b"", &ct).unwrap_err();
        assert_eq!(err, CryptoError::AuthenticationFailed);
    }

    #[test]
    fn aead_tamper_in_tag_fails_authentication() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let pt = b"important";
        let mut ct = DalekBackend.aead_seal(&key, &nonce, b"", pt).unwrap();
        let last = ct.len() - 1;
        ct[last] ^= 0xff;
        let err = DalekBackend.aead_open(&key, &nonce, b"", &ct).unwrap_err();
        assert_eq!(err, CryptoError::AuthenticationFailed);
    }

    #[test]
    fn aead_wrong_nonce_fails_authentication() {
        let key = fresh_aes_key();
        let nonce1 = fresh_nonce();
        let mut nonce2 = nonce1;
        nonce2[0] ^= 0x01;
        let ct = DalekBackend.aead_seal(&key, &nonce1, b"", b"x").unwrap();
        let err = DalekBackend.aead_open(&key, &nonce2, b"", &ct).unwrap_err();
        assert_eq!(err, CryptoError::AuthenticationFailed);
    }

    #[test]
    fn aead_wrong_key_fails_authentication() {
        let key1 = fresh_aes_key();
        let mut key2 = key1;
        key2[0] ^= 0x01;
        let nonce = fresh_nonce();
        let ct = DalekBackend.aead_seal(&key1, &nonce, b"", b"x").unwrap();
        let err = DalekBackend.aead_open(&key2, &nonce, b"", &ct).unwrap_err();
        assert_eq!(err, CryptoError::AuthenticationFailed);
    }

    #[test]
    fn aead_wrong_ad_fails_authentication() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let ct = DalekBackend.aead_seal(&key, &nonce, b"ad-a", b"x").unwrap();
        let err = DalekBackend.aead_open(&key, &nonce, b"ad-b", &ct).unwrap_err();
        assert_eq!(err, CryptoError::AuthenticationFailed);
    }

    #[test]
    fn aead_truncated_ciphertext_fails_authentication() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        let ct = DalekBackend.aead_seal(&key, &nonce, b"", b"hello").unwrap();
        let err = DalekBackend
            .aead_open(&key, &nonce, b"", &ct[..ct.len() - 1])
            .unwrap_err();
        assert_eq!(err, CryptoError::AuthenticationFailed);
    }

    #[test]
    fn aead_short_ciphertext_fails_authentication() {
        let key = fresh_aes_key();
        let nonce = fresh_nonce();
        // 8 bytes can't possibly be a valid AES-GCM output (no tag).
        let err = DalekBackend.aead_open(&key, &nonce, b"", &[0u8; 8]).unwrap_err();
        assert_eq!(err, CryptoError::AuthenticationFailed);
    }

    #[test]
    fn aead_seal_rejects_short_key() {
        let err = DalekBackend
            .aead_seal(&[0u8; 31], &[0u8; 12], b"", b"x")
            .unwrap_err();
        match err {
            CryptoError::InvalidInput(m) => assert!(m.contains("key")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn aead_seal_rejects_long_key() {
        let err = DalekBackend
            .aead_seal(&[0u8; 33], &[0u8; 12], b"", b"x")
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn aead_seal_rejects_short_nonce() {
        let err = DalekBackend
            .aead_seal(&[0u8; 32], &[0u8; 11], b"", b"x")
            .unwrap_err();
        match err {
            CryptoError::InvalidInput(m) => assert!(m.contains("nonce")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn aead_seal_rejects_long_nonce() {
        let err = DalekBackend
            .aead_seal(&[0u8; 32], &[0u8; 13], b"", b"x")
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn aead_seal_rejects_empty_nonce() {
        let err = DalekBackend
            .aead_seal(&[0u8; 32], &[], b"", b"x")
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn aead_open_rejects_short_key() {
        let err = DalekBackend
            .aead_open(&[0u8; 31], &[0u8; 12], b"", b"abcdefghijklmnop")
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn aead_open_rejects_short_nonce() {
        let err = DalekBackend
            .aead_open(&[0u8; 32], &[0u8; 11], b"", b"abcdefghijklmnop")
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn aead_open_rejects_long_nonce() {
        let err = DalekBackend
            .aead_open(&[0u8; 32], &[0u8; 13], b"", b"abcdefghijklmnop")
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    // -------- sign / verify --------

    #[test]
    fn sign_verify_round_trip() {
        let secret = [7u8; 32];
        let public = ed25519_public_from_secret(&secret);
        let sig = DalekBackend.sign(&secret, b"hello").unwrap();
        assert_eq!(sig.len(), 64);
        assert!(DalekBackend.verify(&public, b"hello", &sig).unwrap());
    }

    #[test]
    fn verify_rejects_flipped_signature() {
        let secret = [3u8; 32];
        let public = ed25519_public_from_secret(&secret);
        let mut sig = DalekBackend.sign(&secret, b"abc").unwrap();
        sig[0] ^= 0x01;
        assert!(!DalekBackend.verify(&public, b"abc", &sig).unwrap());
    }

    #[test]
    fn verify_rejects_flipped_data() {
        let secret = [5u8; 32];
        let public = ed25519_public_from_secret(&secret);
        let sig = DalekBackend.sign(&secret, b"abc").unwrap();
        assert!(!DalekBackend.verify(&public, b"abd", &sig).unwrap());
    }

    #[test]
    fn verify_rejects_signature_from_other_key() {
        let secret_a = [1u8; 32];
        let secret_b = [2u8; 32];
        let public_b = ed25519_public_from_secret(&secret_b);
        let sig = DalekBackend.sign(&secret_a, b"data").unwrap();
        assert!(!DalekBackend.verify(&public_b, b"data", &sig).unwrap());
    }

    #[test]
    fn sign_rejects_short_secret() {
        let err = DalekBackend.sign(&[0u8; 31], b"x").unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn sign_rejects_long_secret() {
        let err = DalekBackend.sign(&[0u8; 33], b"x").unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn sign_rejects_empty_secret() {
        let err = DalekBackend.sign(&[], b"x").unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn verify_rejects_short_public_key() {
        let secret = [4u8; 32];
        let sig = DalekBackend.sign(&secret, b"x").unwrap();
        let err = DalekBackend.verify(&[0u8; 31], b"x", &sig).unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn verify_rejects_long_public_key() {
        let err = DalekBackend
            .verify(&[0u8; 33], b"x", &[0u8; 64])
            .unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn verify_rejects_short_signature() {
        let public = [0u8; 32];
        let err = DalekBackend.verify(&public, b"x", &[0u8; 63]).unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn verify_rejects_long_signature() {
        let public = [0u8; 32];
        let err = DalekBackend.verify(&public, b"x", &[0u8; 65]).unwrap_err();
        assert!(matches!(err, CryptoError::InvalidInput(_)));
    }

    #[test]
    fn verify_with_non_canonical_public_key_either_errors_or_returns_false() {
        // Some 32-byte values are not valid compressed Ed25519 points
        // (decompression fails); others decompress to a point off the
        // prime-order subgroup. ed25519-dalek treats the first class as
        // an InvalidInput error and the second as a verification
        // failure (Ok(false)). We accept either outcome — the
        // important property is that no panic occurs and a zero-byte
        // signature is never accepted as valid.
        let bad_pub = [0xffu8; 32];
        let sig = [0u8; 64];
        match DalekBackend.verify(&bad_pub, b"x", &sig) {
            Ok(false) | Err(CryptoError::InvalidInput(_)) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    // -------- Ed25519 RFC 8032 § 7.1 known answer vectors --------

    // Vector 1: empty message
    #[test]
    fn ed25519_rfc8032_test1_sign() {
        let sk = hex(
            "9d61b19deffd5a60ba844af492ec2cc4
             4449c5697b326919703bac031cae7f60",
        );
        let want_pk = hex(
            "d75a980182b10ab7d54bfed3c964073a
             0ee172f3daa62325af021a68f707511a",
        );
        let want_sig = hex(
            "e5564300c360ac729086e2cc806e828a
             84877f1eb8e5d974d873e06522490155
             5fb8821590a33bacc61e39701cf9b46b
             d25bf5f0595bbe24655141438e7a100b",
        );
        let pk = ed25519_public_from_secret(&sk.as_slice().try_into().unwrap());
        assert_eq!(pk, want_pk);
        let sig = DalekBackend.sign(&sk, b"").unwrap();
        assert_eq!(sig, want_sig);
        assert!(DalekBackend.verify(&want_pk, b"", &want_sig).unwrap());
    }

    // Vector 2: single byte message
    #[test]
    fn ed25519_rfc8032_test2_sign() {
        let sk = hex(
            "4ccd089b28ff96da9db6c346ec114e0f
             5b8a319f35aba624da8cf6ed4fb8a6fb",
        );
        let want_pk = hex(
            "3d4017c3e843895a92b70aa74d1b7ebc
             9c982ccf2ec4968cc0cd55f12af4660c",
        );
        let want_sig = hex(
            "92a009a9f0d4cab8720e820b5f642540
             a2b27b5416503f8fb3762223ebdb69da
             085ac1e43e15996e458f3613d0f11d8c
             387b2eaeb4302aeeb00d291612bb0c00",
        );
        let msg = hex("72");
        let pk = ed25519_public_from_secret(&sk.as_slice().try_into().unwrap());
        assert_eq!(pk, want_pk);
        let sig = DalekBackend.sign(&sk, &msg).unwrap();
        assert_eq!(sig, want_sig);
        assert!(DalekBackend.verify(&want_pk, &msg, &want_sig).unwrap());
    }

    // Vector 3: two-byte message
    #[test]
    fn ed25519_rfc8032_test3_sign() {
        let sk = hex(
            "c5aa8df43f9f837bedb7442f31dcb7b1
             66d38535076f094b85ce3a2e0b4458f7",
        );
        let want_pk = hex(
            "fc51cd8e6218a1a38da47ed00230f058
             0816ed13ba3303ac5deb911548908025",
        );
        let want_sig = hex(
            "6291d657deec24024827e69c3abe01a3
             0ce548a284743a445e3680d7db5ac3ac
             18ff9b538d16f290ae67f760984dc659
             4a7c15e9716ed28dc027beceea1ec40a",
        );
        let msg = hex("af82");
        let sig = DalekBackend.sign(&sk, &msg).unwrap();
        assert_eq!(sig, want_sig);
        assert!(DalekBackend.verify(&want_pk, &msg, &want_sig).unwrap());
    }

    // -------- cross-backend interop & trait object --------

    #[test]
    fn two_independent_backend_instances_interoperate() {
        let a = DalekBackend.generate_keypair().unwrap();
        let b = DalekBackend::new().generate_keypair().unwrap();
        let s1 = DalekBackend.dh(&a.private, &b.public).unwrap();
        let s2 = DalekBackend::new().dh(&b.private, &a.public).unwrap();
        assert_eq!(s1, s2);
    }

    #[test]
    fn trait_object_can_call_every_primitive() {
        let backend: Box<dyn CryptoBackend> = Box::new(DalekBackend::new());
        let kp = backend.generate_keypair().unwrap();
        let other = backend.generate_keypair().unwrap();
        let shared = backend.dh(&kp.private, &other.public).unwrap();
        assert_eq!(shared.len(), 32);
        let prk = backend.hkdf_extract(b"salt", b"ikm").unwrap();
        let out = backend.hkdf_expand(&prk, b"info", 16).unwrap();
        assert_eq!(out.len(), 16);
        let key = [0u8; 32];
        let nonce = [0u8; 12];
        let ct = backend.aead_seal(&key, &nonce, b"", b"hi").unwrap();
        let pt = backend.aead_open(&key, &nonce, b"", &ct).unwrap();
        assert_eq!(pt, b"hi");
        let sig = backend.sign(&[1u8; 32], b"msg").unwrap();
        let pub_key = ed25519_public_from_secret(&[1u8; 32]);
        assert!(backend.verify(&pub_key, b"msg", &sig).unwrap());
    }

    #[test]
    fn arc_dyn_can_call_every_primitive() {
        use std::sync::Arc;
        let backend: Arc<dyn CryptoBackend> = Arc::new(DalekBackend);
        let kp = backend.generate_keypair().unwrap();
        assert_eq!(kp.private.len(), 32);
        assert_eq!(kp.public.len(), 32);
    }

    // -------- Debug / Default / Clone / Copy --------

    #[test]
    fn dalek_backend_default_is_unit() {
        let _ = DalekBackend;
        // `<DalekBackend as Default>::default()` keeps the trait
        // method reachable even though `DalekBackend` is a unit struct
        // (clippy::default_constructed_unit_structs flags the bare
        // `DalekBackend::default()` form).
        let _ = <DalekBackend as Default>::default();
        let _ = DalekBackend::new();
    }

    #[test]
    fn dalek_backend_clone_and_copy() {
        let a = DalekBackend;
        let b = a;
        // `Copy` is enough here — we want to assert both the `Copy` and
        // `Clone` impls are present, but clippy rightly complains about
        // calling `.clone()` on a `Copy` type. Use the trait-qualified
        // form so the call is unambiguous.
        let c = <DalekBackend as Clone>::clone(&a);
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn dalek_backend_debug_renders_struct_name() {
        assert!(format!("{DalekBackend:?}").contains("DalekBackend"));
    }

    // -------- Constants exposed for use sites --------

    #[test]
    fn lengths_match_protocol_expectations() {
        assert_eq!(X25519_KEY_LEN, 32);
        assert_eq!(ED25519_SECRET_LEN, 32);
        assert_eq!(ED25519_PUBLIC_LEN, 32);
        assert_eq!(ED25519_SIGNATURE_LEN, 64);
        assert_eq!(AES256_KEY_LEN, 32);
        assert_eq!(AES_GCM_NONCE_LEN, 12);
        assert_eq!(HKDF_SHA256_MAX_OUTPUT, 255 * 32);
    }

    // -------- A couple of "real" combinations --------

    #[test]
    fn x25519_dh_output_can_seed_hkdf() {
        // Realistic: do DH, feed into HKDF to derive a chain key,
        // then derive AEAD key material with hkdf_expand. The whole
        // pipeline should execute without error and be reproducible.
        let alice = DalekBackend.generate_keypair().unwrap();
        let bob = DalekBackend.generate_keypair().unwrap();
        let s_ab = DalekBackend.dh(&alice.private, &bob.public).unwrap();
        let s_ba = DalekBackend.dh(&bob.private, &alice.public).unwrap();
        assert_eq!(s_ab, s_ba);
        let prk = DalekBackend.hkdf_extract(b"wa-handshake", &s_ab).unwrap();
        let key_material = DalekBackend.hkdf_expand(&prk, b"chat", 64).unwrap();
        assert_eq!(key_material.len(), 64);
        let aes_key = &key_material[..32];
        let nonce = &key_material[32..44];
        let ct = DalekBackend.aead_seal(aes_key, nonce, b"ad", b"pt").unwrap();
        let pt = DalekBackend.aead_open(aes_key, nonce, b"ad", &ct).unwrap();
        assert_eq!(pt, b"pt");
    }

    #[test]
    fn signing_a_handshake_transcript_round_trips() {
        // Sign the bytes that would be carried in WhatsApp's handshake
        // identity proof: the static public key of one party.
        let alice_x = DalekBackend.generate_keypair().unwrap();
        let identity_secret = [9u8; 32];
        let identity_public = ed25519_public_from_secret(&identity_secret);
        let sig = DalekBackend.sign(&identity_secret, &alice_x.public).unwrap();
        assert!(
            DalekBackend
                .verify(&identity_public, &alice_x.public, &sig)
                .unwrap()
        );
    }

    #[test]
    fn dh_with_zero_public_key_yields_all_zero_secret() {
        // X25519 with the all-zero u-coordinate produces an all-zero
        // shared secret; protocol code is expected to reject this, but
        // the primitive itself does not error (consistent with RFC 7748).
        let priv_key = [1u8; 32];
        let pub_key = [0u8; 32];
        let s = DalekBackend.dh(&priv_key, &pub_key).unwrap();
        assert_eq!(s, vec![0u8; 32]);
    }

    #[test]
    fn dh_is_commutative_across_many_random_pairs() {
        for _ in 0..16 {
            let a = DalekBackend.generate_keypair().unwrap();
            let b = DalekBackend.generate_keypair().unwrap();
            let s1 = DalekBackend.dh(&a.private, &b.public).unwrap();
            let s2 = DalekBackend.dh(&b.private, &a.public).unwrap();
            assert_eq!(s1, s2);
        }
    }

    #[test]
    fn aead_round_trip_across_many_random_payloads() {
        for n in [0usize, 1, 16, 17, 32, 100, 1024] {
            let key = fresh_aes_key();
            let nonce = fresh_nonce();
            let pt: Vec<u8> = (0..n).map(|i| (i & 0xff) as u8).collect();
            let ct = DalekBackend.aead_seal(&key, &nonce, b"ad", &pt).unwrap();
            let out = DalekBackend.aead_open(&key, &nonce, b"ad", &ct).unwrap();
            assert_eq!(out, pt);
        }
    }

    #[test]
    fn sign_verify_round_trip_across_many_messages() {
        let secret = [11u8; 32];
        let public = ed25519_public_from_secret(&secret);
        for n in [0usize, 1, 16, 17, 32, 100, 1024] {
            let msg: Vec<u8> = (0..n).map(|i| ((i * 7) & 0xff) as u8).collect();
            let sig = DalekBackend.sign(&secret, &msg).unwrap();
            assert!(DalekBackend.verify(&public, &msg, &sig).unwrap());
        }
    }

    #[test]
    fn hkdf_expand_short_lengths_are_prefixes_of_longer_expansions() {
        // HKDF-Expand is defined such that expanding to L_short bytes
        // yields the same prefix as expanding to L_long >= L_short.
        // This is a useful internal consistency property.
        let prk = vec![3u8; 32];
        let long = DalekBackend.hkdf_expand(&prk, b"info", 64).unwrap();
        let short = DalekBackend.hkdf_expand(&prk, b"info", 16).unwrap();
        assert_eq!(&long[..16], &short[..]);
    }

    #[test]
    fn hkdf_expand_different_prks_yield_different_outputs() {
        let p1 = vec![1u8; 32];
        let p2 = vec![2u8; 32];
        let a = DalekBackend.hkdf_expand(&p1, b"info", 32).unwrap();
        let b = DalekBackend.hkdf_expand(&p2, b"info", 32).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn aead_seal_different_nonces_yield_different_ciphertexts() {
        let key = fresh_aes_key();
        let nonce1 = [1u8; 12];
        let nonce2 = [2u8; 12];
        let ct1 = DalekBackend.aead_seal(&key, &nonce1, b"", b"pt").unwrap();
        let ct2 = DalekBackend.aead_seal(&key, &nonce2, b"", b"pt").unwrap();
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn aead_seal_is_deterministic_for_fixed_inputs() {
        // AES-GCM with fixed key+nonce+ad+pt is a deterministic
        // function; the implementation must agree run-to-run.
        let key = [42u8; 32];
        let nonce = [0u8; 12];
        let ad = b"hdr";
        let pt = b"hello world";
        let ct1 = DalekBackend.aead_seal(&key, &nonce, ad, pt).unwrap();
        let ct2 = DalekBackend.aead_seal(&key, &nonce, ad, pt).unwrap();
        assert_eq!(ct1, ct2);
    }

    #[test]
    fn sign_is_deterministic_for_fixed_secret_and_message() {
        // Ed25519 signatures are deterministic (RFC 8032 §5.1.6).
        let secret = [21u8; 32];
        let msg = b"deterministic";
        let s1 = DalekBackend.sign(&secret, msg).unwrap();
        let s2 = DalekBackend.sign(&secret, msg).unwrap();
        assert_eq!(s1, s2);
    }
}
