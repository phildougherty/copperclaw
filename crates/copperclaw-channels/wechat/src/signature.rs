//! `WeChat` Work webhook signature verification and payload decryption.
//!
//! # Signature is SHA1-over-sorted-concat, **NOT** HMAC
//!
//! Work Weixin signs every callback POST with `msg_signature`, computed as
//!
//! ```text
//! msg_signature = sha1_hex(sort([token, timestamp, nonce, encrypted_text]).join(""))
//! ```
//!
//! where the four strings are sorted lexicographically (ASCII) before
//! concatenation. This is the **same shape as the consumer `WeChat` MP
//! signature** — but it is not HMAC. Callers grepping for `HMAC` in this
//! crate intentionally find nothing; this is a plain SHA1 digest over a
//! deterministic concatenation. We still compare with constant-time
//! equality via [`subtle::ConstantTimeEq`] so timing attacks against the
//! token are not feasible.
//!
//! # Payload decryption (`WXBizMsgCrypt`)
//!
//! Encrypted payloads are AES-256-CBC with PKCS7 padding and an
//! IV taken from the first 16 bytes of the same key. The plaintext layout
//! is:
//!
//! ```text
//! [16 random bytes][4 byte msg_len (network-order u32)][msg_bytes][corpid_bytes]
//! ```
//!
//! Successful decryption returns the inner XML message. The trailing
//! receiver corpid is validated against the configured `corp_id` to
//! guarantee the payload was minted for this tenant.

use aes::Aes256;
use base64::Engine;
use cbc::Decryptor;
use cipher::{BlockModeDecrypt, KeyIvInit};
use sha1::{Digest, Sha1};
use subtle::ConstantTimeEq;
use thiserror::Error;

type Aes256CbcDec = Decryptor<Aes256>;

/// Header name used by the events router for the SHA1 signature.
pub const SIGNATURE_QUERY_PARAM: &str = "msg_signature";

/// Reason a signature check rejected a request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    /// `msg_signature` was missing from the query string.
    #[error("missing msg_signature query parameter")]
    MissingSignature,
    /// The hex portion of the signature is not 40 lowercase hex chars.
    #[error("signature is not a 40-char lowercase hex sha1 digest")]
    SignatureFormat,
    /// The computed digest does not match the supplied one.
    #[error("signature mismatch")]
    Mismatch,
    /// The base64-decoded ciphertext length was not a multiple of 16.
    #[error("ciphertext length is not a multiple of the AES block size")]
    CiphertextLength,
    /// Base64 decoding of the ciphertext failed.
    #[error("ciphertext is not valid base64")]
    CiphertextBase64,
    /// AES-256-CBC decryption / padding strip failed.
    #[error("aes decrypt or pkcs7 unpad failed")]
    Decrypt,
    /// The plaintext was shorter than `16 (rand) + 4 (len) + 0 + corpid`.
    #[error("decrypted payload is shorter than the framing requires")]
    PayloadShort,
    /// The framed length field exceeds the remaining buffer.
    #[error("framed msg length exceeds the decrypted buffer")]
    PayloadLength,
    /// The trailing receiver corpid did not match the configured `corp_id`.
    #[error("decrypted corpid did not match the configured corp_id")]
    CorpIdMismatch,
    /// The supplied AES key was not 43 chars / did not decode to 32 bytes.
    #[error("aes key is not 43 chars / does not decode to 32 bytes")]
    AesKeyShape,
}

/// Compute `msg_signature` exactly as Work Weixin does.
///
/// The four input strings are sorted lexicographically (ASCII),
/// concatenated with no separator, hashed with SHA1, and rendered as
/// lowercase hex.
#[must_use]
pub fn compute_msg_signature(
    token: &str,
    timestamp: &str,
    nonce: &str,
    encrypted_text: &str,
) -> String {
    let mut parts: [&str; 4] = [token, timestamp, nonce, encrypted_text];
    parts.sort_unstable();
    let mut hasher = Sha1::new();
    for p in parts {
        hasher.update(p.as_bytes());
    }
    let digest = hasher.finalize();
    hex::encode(digest)
}

/// Verify a signature in constant time.
///
/// Returns `Ok(())` only if `msg_signature` matches the value derived
/// from the other four inputs.
pub fn verify_msg_signature(
    token: &str,
    timestamp: &str,
    nonce: &str,
    encrypted_text: &str,
    msg_signature: Option<&str>,
) -> Result<(), SignatureError> {
    let provided = msg_signature.ok_or(SignatureError::MissingSignature)?;
    if provided.len() != 40 || !provided.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SignatureError::SignatureFormat);
    }
    let expected = compute_msg_signature(token, timestamp, nonce, encrypted_text);
    if expected.as_bytes().ct_eq(provided.as_bytes()).into() {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

/// Decode a 43-char base64 `encoding_aes_key` into a 32-byte AES key.
///
/// The `=` pad character is appended internally to satisfy the standard
/// base64 alphabet.
pub fn decode_aes_key(encoding_aes_key: &str) -> Result<[u8; 32], SignatureError> {
    if encoding_aes_key.len() != 43 {
        return Err(SignatureError::AesKeyShape);
    }
    let padded = format!("{encoding_aes_key}=");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .map_err(|_| SignatureError::AesKeyShape)?;
    if bytes.len() != 32 {
        return Err(SignatureError::AesKeyShape);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Decrypt a base64-encoded Work Weixin payload.
///
/// On success returns the inner XML message bytes; verifies that the
/// trailing receiver corpid matches `expected_corp_id`.
pub fn decrypt_payload(
    encoding_aes_key: &str,
    encrypted_text: &str,
    expected_corp_id: &str,
) -> Result<Vec<u8>, SignatureError> {
    let aes_key = decode_aes_key(encoding_aes_key)?;
    let cipher_bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted_text.as_bytes())
        .map_err(|_| SignatureError::CiphertextBase64)?;
    if cipher_bytes.is_empty() || cipher_bytes.len() % 16 != 0 {
        return Err(SignatureError::CiphertextLength);
    }
    // IV = first 16 bytes of the key.
    let iv: [u8; 16] = aes_key[..16].try_into().expect("slice len 16");
    let dec =
        Aes256CbcDec::new_from_slices(&aes_key, &iv).map_err(|_| SignatureError::AesKeyShape)?;
    let plain = dec
        .decrypt_padded_vec::<cipher::block_padding::Pkcs7>(&cipher_bytes)
        .map_err(|_| SignatureError::Decrypt)?;
    // plaintext layout: [16 rand][4 len][msg][corpid]
    if plain.len() < 16 + 4 {
        return Err(SignatureError::PayloadShort);
    }
    let len_bytes: [u8; 4] = plain[16..20].try_into().expect("slice len 4");
    let msg_len = u32::from_be_bytes(len_bytes) as usize;
    if 16 + 4 + msg_len > plain.len() {
        return Err(SignatureError::PayloadLength);
    }
    let msg = &plain[20..20 + msg_len];
    let corpid = &plain[20 + msg_len..];
    if corpid != expected_corp_id.as_bytes() {
        return Err(SignatureError::CorpIdMismatch);
    }
    Ok(msg.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};

    type Aes256CbcEnc = cbc::Encryptor<Aes256>;

    fn good_aes_key() -> String {
        let raw = [3u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        encoded.trim_end_matches('=').to_owned()
    }

    /// Encrypt the way Work Weixin's gateway does, for round-trip tests.
    fn encrypt_payload(encoding_aes_key: &str, message: &[u8], corp_id: &str) -> String {
        let aes_key = decode_aes_key(encoding_aes_key).unwrap();
        let iv: [u8; 16] = aes_key[..16].try_into().unwrap();
        let mut buf = Vec::new();
        // 16 random bytes (deterministic in tests).
        buf.extend_from_slice(&[0xAB; 16]);
        let len = u32::try_from(message.len()).unwrap().to_be_bytes();
        buf.extend_from_slice(&len);
        buf.extend_from_slice(message);
        buf.extend_from_slice(corp_id.as_bytes());
        let enc = Aes256CbcEnc::new_from_slices(&aes_key, &iv).unwrap();
        let cipher_bytes = enc.encrypt_padded_vec::<Pkcs7>(&buf);
        base64::engine::general_purpose::STANDARD.encode(cipher_bytes)
    }

    #[test]
    fn compute_signature_sorts_inputs() {
        // Order-independent: any permutation must produce the same digest.
        let a = compute_msg_signature("z-token", "1700000000", "abc", "ZZZ");
        let b = compute_msg_signature("abc", "1700000000", "ZZZ", "z-token");
        let c = compute_msg_signature("ZZZ", "abc", "z-token", "1700000000");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn compute_signature_is_40_hex_chars() {
        let sig = compute_msg_signature("t", "ts", "n", "e");
        assert_eq!(sig.len(), 40);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn compute_signature_matches_known_vector() {
        // Computed by sorting ["b","a","c","d"] -> ["a","b","c","d"] and
        // sha1("abcd"). Locks the algorithm in place.
        let sig = compute_msg_signature("b", "a", "c", "d");
        assert_eq!(sig, "81fe8bfe87576c3ecb22426f8e57847382917acf");
    }

    #[test]
    fn verify_accepts_valid_signature() {
        let token = "tok";
        let ts = "1700000000";
        let nonce = "n";
        let enc = "PAYLOAD";
        let sig = compute_msg_signature(token, ts, nonce, enc);
        verify_msg_signature(token, ts, nonce, enc, Some(&sig)).unwrap();
    }

    #[test]
    fn verify_rejects_missing_signature() {
        let err = verify_msg_signature("t", "ts", "n", "e", None).unwrap_err();
        assert_eq!(err, SignatureError::MissingSignature);
    }

    #[test]
    fn verify_rejects_bad_hex() {
        let err = verify_msg_signature("t", "ts", "n", "e", Some("not-hex")).unwrap_err();
        assert_eq!(err, SignatureError::SignatureFormat);
    }

    #[test]
    fn verify_rejects_short_hex() {
        let short = "ab".repeat(10); // 20 chars
        let err = verify_msg_signature("t", "ts", "n", "e", Some(&short)).unwrap_err();
        assert_eq!(err, SignatureError::SignatureFormat);
    }

    #[test]
    fn verify_rejects_uppercase_signature() {
        // Work Weixin returns lowercase only — uppercase counts as a
        // mismatch.
        let sig = compute_msg_signature("t", "ts", "n", "e");
        let upper = sig.to_uppercase();
        // Uppercase is still ascii hex so the format check passes but the
        // comparison fails.
        let err = verify_msg_signature("t", "ts", "n", "e", Some(&upper)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_rejects_mismatch() {
        let bad = "0".repeat(40);
        let err = verify_msg_signature("t", "ts", "n", "e", Some(&bad)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_rejects_tampered_token() {
        let sig = compute_msg_signature("tok", "ts", "n", "e");
        let err = verify_msg_signature("other-tok", "ts", "n", "e", Some(&sig)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let sig = compute_msg_signature("tok", "ts", "n", "e");
        let err = verify_msg_signature("tok", "ts", "n", "tampered", Some(&sig)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn decode_aes_key_accepts_proper_key() {
        let raw = decode_aes_key(&good_aes_key()).unwrap();
        assert_eq!(raw.len(), 32);
    }

    #[test]
    fn decode_aes_key_rejects_short() {
        let err = decode_aes_key("short").unwrap_err();
        assert_eq!(err, SignatureError::AesKeyShape);
    }

    #[test]
    fn decode_aes_key_rejects_non_base64() {
        let mut k = String::from("!");
        k.push_str(&"a".repeat(42));
        let err = decode_aes_key(&k).unwrap_err();
        assert_eq!(err, SignatureError::AesKeyShape);
    }

    #[test]
    fn decrypt_roundtrip_yields_original_message() {
        let key = good_aes_key();
        let msg = br"<xml><Foo>bar</Foo></xml>";
        let corp = "wx-corp";
        let enc = encrypt_payload(&key, msg, corp);
        let out = decrypt_payload(&key, &enc, corp).unwrap();
        assert_eq!(out, msg.to_vec());
    }

    #[test]
    fn decrypt_rejects_bad_base64() {
        let key = good_aes_key();
        let err = decrypt_payload(&key, "!!!not-base64!!!", "wx-corp").unwrap_err();
        assert_eq!(err, SignatureError::CiphertextBase64);
    }

    #[test]
    fn decrypt_rejects_non_block_size() {
        let key = good_aes_key();
        // 5 bytes -> base64 length not aligned to 16.
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 5]);
        let err = decrypt_payload(&key, &short, "wx-corp").unwrap_err();
        assert_eq!(err, SignatureError::CiphertextLength);
    }

    #[test]
    fn decrypt_rejects_wrong_corpid() {
        let key = good_aes_key();
        let msg = b"hello";
        let enc = encrypt_payload(&key, msg, "wx-corp");
        let err = decrypt_payload(&key, &enc, "other-corp").unwrap_err();
        assert_eq!(err, SignatureError::CorpIdMismatch);
    }

    #[test]
    fn decrypt_rejects_corrupted_ciphertext() {
        let key = good_aes_key();
        let msg = b"hello";
        let mut enc_bytes = base64::engine::general_purpose::STANDARD
            .decode(encrypt_payload(&key, msg, "wx-corp"))
            .unwrap();
        // Flip a byte in the middle.
        enc_bytes[20] ^= 0x01;
        let bad = base64::engine::general_purpose::STANDARD.encode(enc_bytes);
        let err = decrypt_payload(&key, &bad, "wx-corp").unwrap_err();
        // The padding strip is the most common failure mode here.
        assert!(matches!(
            err,
            SignatureError::Decrypt | SignatureError::CorpIdMismatch
        ));
    }

    #[test]
    fn decrypt_rejects_bad_aes_key() {
        let err = decrypt_payload("short", "abc=", "wx-corp").unwrap_err();
        assert_eq!(err, SignatureError::AesKeyShape);
    }

    #[test]
    fn decrypt_rejects_truncated_plaintext() {
        // Encrypt a payload, then truncate it so the framing layout breaks
        // after AES decrypt — except CBC requires multiples of 16 so we
        // craft a deliberately-short body of exactly one block (16 bytes
        // of zeros). After PKCS7 padding strip this yields an empty
        // plaintext, which is too short to host the 20-byte framing
        // header.
        let key = good_aes_key();
        let aes_key = decode_aes_key(&key).unwrap();
        let iv: [u8; 16] = aes_key[..16].try_into().unwrap();
        let enc = Aes256CbcEnc::new_from_slices(&aes_key, &iv).unwrap();
        let cipher = enc.encrypt_padded_vec::<Pkcs7>(&[]);
        let enc_b64 = base64::engine::general_purpose::STANDARD.encode(cipher);
        let err = decrypt_payload(&key, &enc_b64, "wx-corp").unwrap_err();
        assert_eq!(err, SignatureError::PayloadShort);
    }

    #[test]
    fn decrypt_rejects_oversized_msg_len() {
        // Build a plaintext with msg_len > available bytes.
        let key = good_aes_key();
        let aes_key = decode_aes_key(&key).unwrap();
        let iv: [u8; 16] = aes_key[..16].try_into().unwrap();
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; 16]);
        let lie: u32 = 9_999_999;
        buf.extend_from_slice(&lie.to_be_bytes());
        buf.extend_from_slice(b"short");
        buf.extend_from_slice(b"wx-corp");
        let enc = Aes256CbcEnc::new_from_slices(&aes_key, &iv).unwrap();
        let cipher = enc.encrypt_padded_vec::<Pkcs7>(&buf);
        let enc_b64 = base64::engine::general_purpose::STANDARD.encode(cipher);
        let err = decrypt_payload(&key, &enc_b64, "wx-corp").unwrap_err();
        assert_eq!(err, SignatureError::PayloadLength);
    }

    #[test]
    fn signature_error_display_unique_per_variant() {
        let variants = [
            SignatureError::MissingSignature,
            SignatureError::SignatureFormat,
            SignatureError::Mismatch,
            SignatureError::CiphertextLength,
            SignatureError::CiphertextBase64,
            SignatureError::Decrypt,
            SignatureError::PayloadShort,
            SignatureError::PayloadLength,
            SignatureError::CorpIdMismatch,
            SignatureError::AesKeyShape,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            assert!(seen.insert(format!("{v}")));
            let _ = format!("{v:?}");
        }
    }

    #[test]
    fn signature_query_param_constant() {
        assert_eq!(SIGNATURE_QUERY_PARAM, "msg_signature");
    }
}
