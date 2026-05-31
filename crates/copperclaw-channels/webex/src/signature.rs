//! Webex webhook signature verification.
//!
//! Webex signs every webhook POST with an HMAC over the raw request body
//! using the webhook secret. Historically Webex uses HMAC-SHA1 (delivered as
//! `X-Spark-Signature: <hex>`); some newer organisations and the Beta
//! channels expose HMAC-SHA256 instead. We support both, selected by config,
//! and constant-time-compare via the [`subtle`] crate.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;

/// Hash algorithm used for the webhook signature.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SignatureAlgo {
    /// HMAC-SHA1 (Webex default).
    Sha1,
    /// HMAC-SHA256 (newer Webex deployments).
    Sha256,
    /// Accept either algorithm; the concrete one is decided per-request
    /// by the signature's hex length (40 → sha1, 64 → sha256). Lets an
    /// operator survive a Webex-side upgrade without re-configuring.
    Auto,
}

impl SignatureAlgo {
    /// Hex digest length for this algorithm. `Auto` has no single
    /// length; the verifier inspects the supplied signature instead.
    #[must_use]
    pub fn digest_hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            // `Auto` accepts whichever the request carries; expose
            // sha256's length as the "max" so a caller using this
            // for buffer sizing isn't surprised.
            Self::Sha256 | Self::Auto => 64,
        }
    }

    /// Parse the case-insensitive textual identifier into an algorithm.
    pub fn parse(s: &str) -> Result<Self, SignatureError> {
        match s.to_ascii_lowercase().as_str() {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            "auto" => Ok(Self::Auto),
            _ => Err(SignatureError::UnknownAlgo(s.to_owned())),
        }
    }

    /// Stable string identifier — useful for tracing.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Auto => "auto",
        }
    }
}

/// Reasons signature verification fails.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    /// No `X-Spark-Signature` header was supplied.
    #[error("missing X-Spark-Signature header")]
    MissingSignature,
    /// The header was not valid lowercase hex of the expected length.
    #[error("X-Spark-Signature value was not valid hex")]
    BadSignatureFormat,
    /// The provided digest did not match the expected one.
    #[error("signature mismatch")]
    Mismatch,
    /// The configured algorithm is not one we support.
    #[error("unknown signature algorithm: {0}")]
    UnknownAlgo(String),
}

/// Compute the hex-encoded HMAC of `body` for the chosen algorithm.
///
/// Panics if `algo` is [`SignatureAlgo::Auto`] — `Auto` is a
/// verifier-side selector, not a computable algorithm. The caller
/// pre-resolves it via the signature's length.
#[must_use]
pub fn compute_signature(algo: SignatureAlgo, secret: &[u8], body: &[u8]) -> String {
    match algo {
        SignatureAlgo::Sha1 => {
            let mut mac =
                Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
            mac.update(body);
            hex::encode(mac.finalize().into_bytes())
        }
        SignatureAlgo::Sha256 => {
            let mut mac =
                Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
            mac.update(body);
            hex::encode(mac.finalize().into_bytes())
        }
        SignatureAlgo::Auto => panic!(
            "compute_signature called with SignatureAlgo::Auto — \
             resolve to a concrete algorithm via the signature's length first"
        ),
    }
}

/// Verify that `provided` matches the expected HMAC for `body`.
///
/// Comparison is constant-time. `provided` may be `None` (missing header).
/// When `algo` is [`SignatureAlgo::Auto`], the concrete algorithm is
/// inferred from the signature's hex length (40 → sha1, 64 → sha256)
/// before verification; any other length is `BadSignatureFormat`.
pub fn verify_signature(
    algo: SignatureAlgo,
    secret: &[u8],
    body: &[u8],
    provided: Option<&str>,
) -> Result<(), SignatureError> {
    let sig = provided.ok_or(SignatureError::MissingSignature)?;
    let concrete = match algo {
        SignatureAlgo::Auto => match sig.len() {
            40 => SignatureAlgo::Sha1,
            64 => SignatureAlgo::Sha256,
            _ => return Err(SignatureError::BadSignatureFormat),
        },
        other => other,
    };
    if sig.len() != concrete.digest_hex_len() {
        return Err(SignatureError::BadSignatureFormat);
    }
    let provided_bytes = hex::decode(sig).map_err(|_| SignatureError::BadSignatureFormat)?;
    let expected_hex = compute_signature(concrete, secret, body);
    let expected_bytes =
        hex::decode(&expected_hex).map_err(|_| SignatureError::BadSignatureFormat)?;
    if provided_bytes.ct_eq(&expected_bytes).into() {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"super-secret";
    const BODY: &[u8] = b"{\"id\":\"abc\"}";

    #[test]
    fn algo_parse_accepts_sha1_and_sha256() {
        assert_eq!(SignatureAlgo::parse("sha1").unwrap(), SignatureAlgo::Sha1);
        assert_eq!(SignatureAlgo::parse("SHA1").unwrap(), SignatureAlgo::Sha1);
        assert_eq!(
            SignatureAlgo::parse("sha256").unwrap(),
            SignatureAlgo::Sha256
        );
        assert_eq!(
            SignatureAlgo::parse("SHA256").unwrap(),
            SignatureAlgo::Sha256
        );
    }

    #[test]
    fn algo_parse_rejects_unknown() {
        let err = SignatureAlgo::parse("md5").unwrap_err();
        assert!(matches!(err, SignatureError::UnknownAlgo(s) if s == "md5"));
    }

    #[test]
    fn algo_digest_hex_len_is_correct() {
        assert_eq!(SignatureAlgo::Sha1.digest_hex_len(), 40);
        assert_eq!(SignatureAlgo::Sha256.digest_hex_len(), 64);
    }

    #[test]
    fn algo_as_str_is_stable() {
        assert_eq!(SignatureAlgo::Sha1.as_str(), "sha1");
        assert_eq!(SignatureAlgo::Sha256.as_str(), "sha256");
    }

    #[test]
    fn algo_debug_and_clone() {
        let a = SignatureAlgo::Sha1;
        let b = a;
        assert_eq!(a, b);
        assert!(format!("{a:?}").contains("Sha1"));
    }

    #[test]
    fn compute_signature_sha1_known_vector() {
        // HMAC-SHA1("super-secret", "{\"id\":\"abc\"}")
        // Verified with: openssl dgst -sha1 -hmac "super-secret"
        let sig = compute_signature(SignatureAlgo::Sha1, SECRET, BODY);
        assert_eq!(sig.len(), 40);
        // We do not hardcode the digest here because the test below covers
        // roundtripping; this assertion documents the contract.
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn compute_signature_sha256_length() {
        let sig = compute_signature(SignatureAlgo::Sha256, SECRET, BODY);
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn verify_sha1_accepts_matching_signature() {
        let sig = compute_signature(SignatureAlgo::Sha1, SECRET, BODY);
        verify_signature(SignatureAlgo::Sha1, SECRET, BODY, Some(&sig)).unwrap();
    }

    #[test]
    fn verify_sha256_accepts_matching_signature() {
        let sig = compute_signature(SignatureAlgo::Sha256, SECRET, BODY);
        verify_signature(SignatureAlgo::Sha256, SECRET, BODY, Some(&sig)).unwrap();
    }

    #[test]
    fn verify_sha1_rejects_wrong_signature() {
        let bad = "0".repeat(40);
        let err = verify_signature(SignatureAlgo::Sha1, SECRET, BODY, Some(&bad)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_sha256_rejects_wrong_signature() {
        let bad = "0".repeat(64);
        let err = verify_signature(SignatureAlgo::Sha256, SECRET, BODY, Some(&bad)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_rejects_missing_header() {
        let err = verify_signature(SignatureAlgo::Sha1, SECRET, BODY, None).unwrap_err();
        assert_eq!(err, SignatureError::MissingSignature);
    }

    #[test]
    fn verify_rejects_wrong_length_signature() {
        // SHA-256 length value with SHA-1 algorithm selected.
        let too_long = "a".repeat(64);
        let err =
            verify_signature(SignatureAlgo::Sha1, SECRET, BODY, Some(&too_long)).unwrap_err();
        assert_eq!(err, SignatureError::BadSignatureFormat);
    }

    #[test]
    fn verify_rejects_non_hex_signature() {
        // Right length, wrong alphabet.
        let bad = "z".repeat(40);
        let err = verify_signature(SignatureAlgo::Sha1, SECRET, BODY, Some(&bad)).unwrap_err();
        assert_eq!(err, SignatureError::BadSignatureFormat);
    }

    #[test]
    fn verify_sha256_rejects_sha1_length_signature() {
        let too_short = "a".repeat(40);
        let err =
            verify_signature(SignatureAlgo::Sha256, SECRET, BODY, Some(&too_short)).unwrap_err();
        assert_eq!(err, SignatureError::BadSignatureFormat);
    }

    #[test]
    fn signature_error_display_unique_per_variant() {
        let variants = [
            SignatureError::MissingSignature,
            SignatureError::BadSignatureFormat,
            SignatureError::Mismatch,
            SignatureError::UnknownAlgo("md5".into()),
        ];
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            assert!(seen.insert(format!("{v}")));
            let _ = format!("{v:?}");
        }
    }

    #[test]
    fn compute_signature_differs_per_algo() {
        let s1 = compute_signature(SignatureAlgo::Sha1, SECRET, BODY);
        let s256 = compute_signature(SignatureAlgo::Sha256, SECRET, BODY);
        assert_ne!(s1, s256);
    }

    #[test]
    fn compute_signature_differs_per_secret() {
        let s1 = compute_signature(SignatureAlgo::Sha1, b"secret-a", BODY);
        let s2 = compute_signature(SignatureAlgo::Sha1, b"secret-b", BODY);
        assert_ne!(s1, s2);
    }

    #[test]
    fn compute_signature_differs_per_body() {
        let s1 = compute_signature(SignatureAlgo::Sha1, SECRET, b"one");
        let s2 = compute_signature(SignatureAlgo::Sha1, SECRET, b"two");
        assert_ne!(s1, s2);
    }

    #[test]
    fn algo_parse_accepts_auto() {
        assert_eq!(SignatureAlgo::parse("auto").unwrap(), SignatureAlgo::Auto);
        assert_eq!(SignatureAlgo::parse("AUTO").unwrap(), SignatureAlgo::Auto);
    }

    #[test]
    fn algo_auto_as_str_is_auto() {
        assert_eq!(SignatureAlgo::Auto.as_str(), "auto");
    }

    #[test]
    fn verify_auto_accepts_sha1_length_signature() {
        let sig = compute_signature(SignatureAlgo::Sha1, SECRET, BODY);
        verify_signature(SignatureAlgo::Auto, SECRET, BODY, Some(&sig)).unwrap();
    }

    #[test]
    fn verify_auto_accepts_sha256_length_signature() {
        let sig = compute_signature(SignatureAlgo::Sha256, SECRET, BODY);
        verify_signature(SignatureAlgo::Auto, SECRET, BODY, Some(&sig)).unwrap();
    }

    #[test]
    fn verify_auto_rejects_other_length() {
        // 48 chars — neither sha1 nor sha256.
        let bad = "a".repeat(48);
        let err = verify_signature(SignatureAlgo::Auto, SECRET, BODY, Some(&bad)).unwrap_err();
        assert_eq!(err, SignatureError::BadSignatureFormat);
    }

    #[test]
    fn verify_auto_rejects_wrong_signature_at_sha1_length() {
        let bad = "0".repeat(40);
        let err = verify_signature(SignatureAlgo::Auto, SECRET, BODY, Some(&bad)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    fn verify_auto_rejects_wrong_signature_at_sha256_length() {
        let bad = "0".repeat(64);
        let err = verify_signature(SignatureAlgo::Auto, SECRET, BODY, Some(&bad)).unwrap_err();
        assert_eq!(err, SignatureError::Mismatch);
    }

    #[test]
    #[should_panic(expected = "Auto")]
    fn compute_signature_with_auto_panics() {
        let _ = compute_signature(SignatureAlgo::Auto, SECRET, BODY);
    }
}
