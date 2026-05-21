//! Linear webhook signature verification.
//!
//! Linear signs every webhook POST with HMAC-SHA256 over the raw request
//! body using the workspace webhook secret, and sends the lowercase hex
//! digest in `Linear-Signature`. We constant-time-compare against an
//! expected digest computed locally.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Reason a signature check rejected a request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    /// The `Linear-Signature` header was not present.
    #[error("missing Linear-Signature header")]
    MissingSignature,
    /// The header value did not decode as hex.
    #[error("Linear-Signature is not valid hex")]
    BadSignatureFormat,
    /// The header decoded but did not have the expected SHA-256 length.
    #[error("Linear-Signature digest length does not match SHA-256")]
    SignatureLength,
    /// The computed digest did not match the provided digest.
    #[error("signature mismatch")]
    Mismatch,
}

/// Compute the expected lower-case hex digest for a webhook body.
///
/// Public so tests (and the adapter's own replay tooling) can produce a
/// matching header value.
#[must_use]
pub fn compute_signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(body);
    let result = mac.finalize().into_bytes();
    hex::encode(result)
}

/// Verify that `signature_header` matches the body under `secret`.
///
/// The comparison is constant-time via the [`subtle`] crate.
pub fn verify_signature(
    secret: &str,
    signature_header: Option<&str>,
    body: &[u8],
) -> Result<(), SignatureError> {
    let sig = signature_header.ok_or(SignatureError::MissingSignature)?;
    let provided = hex::decode(sig).map_err(|_| SignatureError::BadSignatureFormat)?;
    if provided.len() != 32 {
        return Err(SignatureError::SignatureLength);
    }
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    if expected.ct_eq(&provided).into() {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "linear-test-secret";
    const BODY: &[u8] = b"{\"type\":\"Comment\",\"action\":\"create\"}";

    #[test]
    fn compute_is_64_hex_chars() {
        let sig = compute_signature(SECRET, BODY);
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn compute_is_deterministic() {
        let a = compute_signature(SECRET, BODY);
        let b = compute_signature(SECRET, BODY);
        assert_eq!(a, b);
    }

    #[test]
    fn verify_accepts_matching_signature() {
        let sig = compute_signature(SECRET, BODY);
        verify_signature(SECRET, Some(&sig), BODY).unwrap();
    }

    #[test]
    fn verify_rejects_missing_header() {
        let res = verify_signature(SECRET, None, BODY);
        assert_eq!(res, Err(SignatureError::MissingSignature));
    }

    #[test]
    fn verify_rejects_non_hex_header() {
        let res = verify_signature(SECRET, Some("not-hex-at-all-zzz"), BODY);
        assert_eq!(res, Err(SignatureError::BadSignatureFormat));
    }

    #[test]
    fn verify_rejects_short_signature() {
        let short = hex::encode([0u8; 16]);
        let res = verify_signature(SECRET, Some(&short), BODY);
        assert_eq!(res, Err(SignatureError::SignatureLength));
    }

    #[test]
    fn verify_rejects_long_signature() {
        let long = hex::encode([0u8; 64]);
        let res = verify_signature(SECRET, Some(&long), BODY);
        assert_eq!(res, Err(SignatureError::SignatureLength));
    }

    #[test]
    fn verify_rejects_wrong_signature() {
        let bad = hex::encode([0u8; 32]);
        let res = verify_signature(SECRET, Some(&bad), BODY);
        assert_eq!(res, Err(SignatureError::Mismatch));
    }

    #[test]
    fn verify_uses_constant_time_compare_for_almost_match() {
        // Compute valid sig, flip one byte; should still be Mismatch, not panic.
        let sig = compute_signature(SECRET, BODY);
        let mut bytes = hex::decode(&sig).unwrap();
        bytes[0] ^= 0x01;
        let tweaked = hex::encode(&bytes);
        assert_eq!(
            verify_signature(SECRET, Some(&tweaked), BODY),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn verify_with_different_secret_fails() {
        let sig = compute_signature(SECRET, BODY);
        let res = verify_signature("other-secret", Some(&sig), BODY);
        assert_eq!(res, Err(SignatureError::Mismatch));
    }

    #[test]
    fn verify_with_different_body_fails() {
        let sig = compute_signature(SECRET, BODY);
        let res = verify_signature(SECRET, Some(&sig), b"different body");
        assert_eq!(res, Err(SignatureError::Mismatch));
    }

    #[test]
    fn empty_body_signature_roundtrips() {
        let sig = compute_signature(SECRET, b"");
        verify_signature(SECRET, Some(&sig), b"").unwrap();
    }

    #[test]
    fn empty_secret_signature_roundtrips() {
        // The hmac crate accepts any key length, including zero.
        let sig = compute_signature("", BODY);
        verify_signature("", Some(&sig), BODY).unwrap();
    }

    #[test]
    fn signature_error_display_unique_per_variant() {
        let variants = [
            SignatureError::MissingSignature,
            SignatureError::BadSignatureFormat,
            SignatureError::SignatureLength,
            SignatureError::Mismatch,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            assert!(seen.insert(format!("{v}")));
            let _ = format!("{v:?}");
        }
    }

    #[test]
    fn verify_uppercase_hex_is_accepted() {
        // hex::decode is case-insensitive.
        let sig = compute_signature(SECRET, BODY);
        let upper = sig.to_uppercase();
        verify_signature(SECRET, Some(&upper), BODY).unwrap();
    }
}
