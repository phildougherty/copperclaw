//! `WhatsApp` Cloud webhook signature verification.
//!
//! Meta signs every notification POST with HMAC-SHA256 of the raw body
//! using the configured app secret, sending the result in
//! `X-Hub-Signature-256: sha256=<hex>`. We use `hmac` + `sha2` to compute
//! the expected MAC and `subtle::ConstantTimeEq` to compare.
//!
//! Unlike Slack, `WhatsApp` signs only the body — no timestamp prefix —
//! so there is no skew check here.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

/// Header that carries the signature on inbound webhook POSTs.
pub const SIGNATURE_HEADER: &str = "X-Hub-Signature-256";

/// Reason a signature check rejected a request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    /// No `X-Hub-Signature-256` header was present.
    #[error("missing X-Hub-Signature-256 header")]
    MissingSignature,
    /// The header value does not start with the `sha256=` prefix.
    #[error("signature does not have the sha256= prefix")]
    BadSignatureFormat,
    /// The hex portion is not a valid hex string or wrong length.
    #[error("signature digest is not valid sha256 hex")]
    SignatureLength,
    /// The computed MAC does not match the supplied MAC.
    #[error("signature mismatch")]
    Mismatch,
}

/// Compute the expected `sha256=<hex>` signature for a body.
///
/// Public so tests (and any replay harness) can build a matching header.
#[must_use]
pub fn compute_signature(app_secret: &str, body: &[u8]) -> String {
    format!("sha256={}", hex::encode(hmac_sha256(app_secret.as_bytes(), body)))
}

/// Verify a `sha256=<hex>` header against the raw body using the app secret.
///
/// Returns `Ok(())` on a constant-time match, otherwise a typed
/// [`SignatureError`].
pub fn verify_signature(
    app_secret: &str,
    signature_header: Option<&str>,
    body: &[u8],
) -> Result<(), SignatureError> {
    let sig = signature_header.ok_or(SignatureError::MissingSignature)?;
    let hex_part = sig
        .strip_prefix("sha256=")
        .ok_or(SignatureError::BadSignatureFormat)?;
    let provided = hex::decode(hex_part).map_err(|_| SignatureError::BadSignatureFormat)?;
    if provided.len() != 32 {
        return Err(SignatureError::SignatureLength);
    }
    let expected = hmac_sha256(app_secret.as_bytes(), body);
    if expected.ct_eq(&provided).into() {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whatsapp-app-secret";
    const BODY: &[u8] = br#"{"object":"whatsapp_business_account","entry":[]}"#;

    #[test]
    fn compute_produces_expected_length_hex() {
        let sig = compute_signature(SECRET, BODY);
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), "sha256=".len() + 64);
    }

    #[test]
    fn verify_accepts_matching_signature() {
        let sig = compute_signature(SECRET, BODY);
        verify_signature(SECRET, Some(&sig), BODY).unwrap();
    }

    #[test]
    fn verify_rejects_missing_header() {
        let err = verify_signature(SECRET, None, BODY).unwrap_err();
        assert_eq!(err, SignatureError::MissingSignature);
    }

    #[test]
    fn verify_rejects_missing_prefix() {
        let no_prefix = "deadbeef";
        let err = verify_signature(SECRET, Some(no_prefix), BODY).unwrap_err();
        assert_eq!(err, SignatureError::BadSignatureFormat);
    }

    #[test]
    fn verify_rejects_wrong_prefix() {
        let wrong = format!("sha1={}", "ab".repeat(32));
        let err = verify_signature(SECRET, Some(&wrong), BODY).unwrap_err();
        assert_eq!(err, SignatureError::BadSignatureFormat);
    }

    #[test]
    fn verify_rejects_non_hex() {
        let bad = "sha256=ZZZZ";
        let err = verify_signature(SECRET, Some(bad), BODY).unwrap_err();
        assert_eq!(err, SignatureError::BadSignatureFormat);
    }

    #[test]
    fn verify_rejects_short_digest() {
        let bad = format!("sha256={}", "ab".repeat(16));
        let err = verify_signature(SECRET, Some(&bad), BODY).unwrap_err();
        assert_eq!(err, SignatureError::SignatureLength);
    }

    #[test]
    fn verify_rejects_wrong_digest() {
        let wrong = format!("sha256={}", "00".repeat(32));
        let err = verify_signature(SECRET, Some(&wrong), BODY).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_rejects_when_body_tampered() {
        let sig = compute_signature(SECRET, BODY);
        let tampered = b"different body";
        let err = verify_signature(SECRET, Some(&sig), tampered).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_rejects_when_secret_wrong() {
        let sig = compute_signature(SECRET, BODY);
        let err = verify_signature("other-secret", Some(&sig), BODY).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn signature_error_display_is_unique_per_variant() {
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
    fn signature_header_constant_value() {
        assert_eq!(SIGNATURE_HEADER, "X-Hub-Signature-256");
    }

    #[test]
    fn empty_body_still_signs_consistently() {
        let sig = compute_signature(SECRET, b"");
        verify_signature(SECRET, Some(&sig), b"").unwrap();
    }

    #[test]
    fn long_key_handled() {
        let long = "x".repeat(200);
        let sig = compute_signature(&long, BODY);
        verify_signature(&long, Some(&sig), BODY).unwrap();
    }
}
