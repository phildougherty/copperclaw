//! Generic HMAC-SHA256 signature verification.
//!
//! Most webhook providers sign request bodies the same way: the receiver
//! computes `HMAC-SHA256(secret, raw_body)` and compares against a value
//! the sender passes in a header. The hex digest can be wrapped in a
//! prefix like `sha256=` (GitHub) or `t=...,v1=...` (Stripe, more
//! involved). For the parity-bound first slice we cover the common case:
//! a single hex digest with an optional plain prefix.
//!
//! Constant-time comparison is critical — the underlying primitive in
//! `hmac` does it for us via `Mac::verify_slice`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Outcome of [`verify`].
#[derive(Debug, PartialEq, Eq)]
pub enum SignatureOutcome {
    /// Verified successfully.
    Ok,
    /// Header was present but didn't parse as hex / didn't match.
    Mismatch(&'static str),
    /// Header was absent — caller decides whether to reject.
    HeaderMissing,
}

/// Verify the body against the configured secret + header value.
///
/// `header_value` is the raw string the request carried in the configured
/// signature header (or `None` if it was absent). When `header_value` is
/// `Some` but the strip / decode / compare fails, [`SignatureOutcome::Mismatch`]
/// carries a short reason. The reasons are constants (no formatting) so
/// they're cheap to log without leaking attacker-controlled bytes.
#[must_use]
pub fn verify(
    body: &[u8],
    secret: &str,
    prefix: &str,
    header_value: Option<&str>,
) -> SignatureOutcome {
    let Some(raw) = header_value else {
        return SignatureOutcome::HeaderMissing;
    };
    let trimmed = raw.trim();
    let hex_part = if prefix.is_empty() {
        trimmed
    } else if let Some(rest) = trimmed.strip_prefix(prefix) {
        rest
    } else {
        return SignatureOutcome::Mismatch("missing prefix");
    };
    let Ok(received) = hex::decode(hex_part) else {
        return SignatureOutcome::Mismatch("not hex");
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return SignatureOutcome::Mismatch("bad secret");
    };
    mac.update(body);
    if mac.verify_slice(&received).is_ok() {
        SignatureOutcome::Ok
    } else {
        SignatureOutcome::Mismatch("digest mismatch")
    }
}

/// Compute the hex-encoded HMAC-SHA256 digest of `body` under `secret`.
/// Test helper, but exposed because tooling occasionally needs to mint
/// signatures for outbound calls in user code.
#[must_use]
pub fn compute_hex(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_when_digest_matches_no_prefix() {
        let body = b"hello, hooks";
        let sig = compute_hex("topsecret", body);
        assert_eq!(
            verify(body, "topsecret", "", Some(&sig)),
            SignatureOutcome::Ok
        );
    }

    #[test]
    fn ok_when_digest_matches_with_prefix() {
        let body = b"hello, hooks";
        let sig = compute_hex("topsecret", body);
        let header = format!("sha256={sig}");
        assert_eq!(
            verify(body, "topsecret", "sha256=", Some(&header)),
            SignatureOutcome::Ok
        );
    }

    #[test]
    fn ok_when_header_has_surrounding_whitespace() {
        let body = b"x";
        let sig = compute_hex("s", body);
        let padded = format!("  {sig}  ");
        assert_eq!(
            verify(body, "s", "", Some(&padded)),
            SignatureOutcome::Ok
        );
    }

    #[test]
    fn header_missing_reported_distinctly() {
        let out = verify(b"x", "s", "", None);
        assert_eq!(out, SignatureOutcome::HeaderMissing);
    }

    #[test]
    fn missing_prefix_is_mismatch() {
        let body = b"x";
        let sig = compute_hex("s", body);
        let header = sig; // sender forgot the `sha256=` prefix
        assert_eq!(
            verify(body, "s", "sha256=", Some(&header)),
            SignatureOutcome::Mismatch("missing prefix")
        );
    }

    #[test]
    fn non_hex_is_mismatch() {
        let out = verify(b"x", "s", "", Some("not-a-hex-string"));
        assert_eq!(out, SignatureOutcome::Mismatch("not hex"));
    }

    #[test]
    fn different_secret_produces_mismatch() {
        let body = b"hi";
        let sig = compute_hex("right-key", body);
        let out = verify(body, "wrong-key", "", Some(&sig));
        assert_eq!(out, SignatureOutcome::Mismatch("digest mismatch"));
    }

    #[test]
    fn tampered_body_produces_mismatch() {
        let signed = b"original";
        let sig = compute_hex("k", signed);
        let out = verify(b"tampered", "k", "", Some(&sig));
        assert_eq!(out, SignatureOutcome::Mismatch("digest mismatch"));
    }

    #[test]
    fn compute_hex_is_stable() {
        // Reproducibility test — same inputs always yield same digest.
        let a = compute_hex("k", b"body");
        let b = compute_hex("k", b"body");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn compute_hex_changes_with_key() {
        let a = compute_hex("k1", b"x");
        let b = compute_hex("k2", b"x");
        assert_ne!(a, b);
    }
}
