//! GitHub webhook signature verification.
//!
//! GitHub signs every webhook delivery with HMAC-SHA256 over the raw request
//! body using the configured shared secret, and sends the result in
//! `X-Hub-Signature-256` as `sha256=<hex>`. We verify by recomputing the MAC
//! and comparing in constant time via the `subtle` crate.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Reason a signature check rejected a request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    /// `X-Hub-Signature-256` header missing.
    #[error("missing X-Hub-Signature-256 header")]
    MissingSignature,
    /// Signature value did not have the `sha256=` prefix.
    #[error("signature does not have the sha256= prefix")]
    BadSignatureFormat,
    /// Signature contained non-hex characters or was the wrong length.
    #[error("signature is not valid sha256 hex")]
    BadSignatureHex,
    /// Computed MAC did not match the value in the header.
    #[error("signature mismatch")]
    Mismatch,
}

/// Compute the expected `sha256=<hex>` value for a request body.
///
/// Public so tests (and the adapter, for replay scenarios) can produce a
/// matching header.
#[must_use]
pub fn compute_signature(secret: &str, body: &[u8]) -> String {
    let mac = hmac_sha256(secret.as_bytes(), body);
    format!("sha256={}", hex::encode(mac))
}

/// Verify that `signature_header` matches the request body, using `secret`.
///
/// - `signature_header` is the raw `X-Hub-Signature-256` value, e.g.
///   `sha256=abcd...`. Missing → [`SignatureError::MissingSignature`].
/// - Body is compared with a constant-time check.
pub fn verify_signature(
    secret: &str,
    signature_header: Option<&str>,
    body: &[u8],
) -> Result<(), SignatureError> {
    let sig = signature_header.ok_or(SignatureError::MissingSignature)?;
    let hex_part = sig
        .strip_prefix("sha256=")
        .ok_or(SignatureError::BadSignatureFormat)?;
    let provided = hex::decode(hex_part).map_err(|_| SignatureError::BadSignatureHex)?;
    if provided.len() != 32 {
        return Err(SignatureError::BadSignatureHex);
    }
    let expected = hmac_sha256(secret.as_bytes(), body);
    if expected.ct_eq(&provided).into() {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

/// HMAC-SHA256 via the `hmac` crate. Returns the 32-byte digest.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    // Reference value from the GitHub docs:
    // https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries
    const EXPECTED: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn compute_matches_github_example() {
        assert_eq!(compute_signature(SECRET, BODY), EXPECTED);
    }

    #[test]
    fn verify_accepts_matching_signature() {
        verify_signature(SECRET, Some(EXPECTED), BODY).unwrap();
    }

    #[test]
    fn verify_rejects_missing_signature_header() {
        let res = verify_signature(SECRET, None, BODY);
        assert_eq!(res, Err(SignatureError::MissingSignature));
    }

    #[test]
    fn verify_rejects_signature_without_prefix() {
        let res = verify_signature(SECRET, Some("abcd"), BODY);
        assert_eq!(res, Err(SignatureError::BadSignatureFormat));
    }

    #[test]
    fn verify_rejects_md5_prefix() {
        // Old `X-Hub-Signature` (sha1) format — must be rejected.
        let res = verify_signature(SECRET, Some("sha1=abcd"), BODY);
        assert_eq!(res, Err(SignatureError::BadSignatureFormat));
    }

    #[test]
    fn verify_rejects_non_hex_signature() {
        let res = verify_signature(SECRET, Some("sha256=ZZZZ"), BODY);
        assert_eq!(res, Err(SignatureError::BadSignatureHex));
    }

    #[test]
    fn verify_rejects_short_signature() {
        let res = verify_signature(SECRET, Some("sha256=abab"), BODY);
        assert_eq!(res, Err(SignatureError::BadSignatureHex));
    }

    #[test]
    fn verify_rejects_wrong_signature() {
        let wrong = format!("sha256={}", "00".repeat(32));
        let res = verify_signature(SECRET, Some(&wrong), BODY);
        assert_eq!(res, Err(SignatureError::Mismatch));
    }

    #[test]
    fn verify_rejects_modified_body() {
        let mut modified = BODY.to_vec();
        modified[0] ^= 1;
        let res = verify_signature(SECRET, Some(EXPECTED), &modified);
        assert_eq!(res, Err(SignatureError::Mismatch));
    }

    #[test]
    fn verify_uses_constant_time_compare() {
        // Surrogate for "real constant-time check": flipping bytes in different
        // positions must yield the same `Mismatch` result, and the function
        // must return Mismatch (not MissingSignature/BadSignatureHex) for
        // wrong-but-correctly-sized digests.
        let mut bad_bytes = [0u8; 32];
        bad_bytes[0] = 1;
        let bad = format!("sha256={}", hex::encode(bad_bytes));
        let res = verify_signature(SECRET, Some(&bad), BODY);
        assert_eq!(res, Err(SignatureError::Mismatch));
        bad_bytes[0] = 0;
        bad_bytes[31] = 1;
        let bad = format!("sha256={}", hex::encode(bad_bytes));
        let res = verify_signature(SECRET, Some(&bad), BODY);
        assert_eq!(res, Err(SignatureError::Mismatch));
    }

    #[test]
    fn compute_round_trips_through_verify() {
        let secret = "another-secret-value";
        let body = b"{\"action\":\"opened\"}".to_vec();
        let sig = compute_signature(secret, &body);
        verify_signature(secret, Some(&sig), &body).unwrap();
    }

    #[test]
    fn compute_differs_per_secret() {
        let a = compute_signature("alpha", BODY);
        let b = compute_signature("beta", BODY);
        assert_ne!(a, b);
    }

    #[test]
    fn compute_differs_per_body() {
        let a = compute_signature(SECRET, b"a");
        let b = compute_signature(SECRET, b"b");
        assert_ne!(a, b);
    }

    #[test]
    fn signature_error_display_unique_per_variant() {
        let variants = [
            SignatureError::MissingSignature,
            SignatureError::BadSignatureFormat,
            SignatureError::BadSignatureHex,
            SignatureError::Mismatch,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            assert!(seen.insert(format!("{v}")));
            let _ = format!("{v:?}");
        }
    }

    #[test]
    fn hmac_sha256_long_key_works() {
        let long_key = vec![b'a'; 200];
        let out = hmac_sha256(&long_key, b"msg");
        assert_eq!(out.len(), 32);
    }

    #[test]
    fn hmac_sha256_empty_key_works() {
        let out = hmac_sha256(b"", b"msg");
        assert_eq!(out.len(), 32);
    }
}
