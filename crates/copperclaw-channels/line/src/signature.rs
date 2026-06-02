//! LINE webhook signature verification.
//!
//! LINE signs every webhook delivery with HMAC-SHA256 over the raw
//! request body, then **base64-encodes** the result and ships it as
//! `X-Line-Signature`. This is the same algorithm GitHub uses but a
//! different encoding (GitHub hex, LINE base64), so we keep a
//! channel-specific verifier rather than trying to share code with
//! the GitHub channel.
//!
//! All comparisons go through `hmac::Mac::verify_slice` for
//! constant-time equality.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Outcome of [`verify`].
#[derive(Debug, PartialEq, Eq)]
pub enum SignatureOutcome {
    /// Verified successfully.
    Ok,
    /// Header was present but didn't parse as base64 or didn't match.
    Mismatch(&'static str),
    /// Header was absent.
    HeaderMissing,
}

/// Verify the body against the configured channel secret + header
/// value. `header_value` is the raw string the request carried in
/// `X-Line-Signature` (or `None` if absent).
#[must_use]
pub fn verify(body: &[u8], channel_secret: &str, header_value: Option<&str>) -> SignatureOutcome {
    let Some(raw) = header_value else {
        return SignatureOutcome::HeaderMissing;
    };
    let trimmed = raw.trim();
    let Ok(received) = STANDARD.decode(trimmed) else {
        return SignatureOutcome::Mismatch("not base64");
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(channel_secret.as_bytes()) else {
        return SignatureOutcome::Mismatch("bad secret");
    };
    mac.update(body);
    if mac.verify_slice(&received).is_ok() {
        SignatureOutcome::Ok
    } else {
        SignatureOutcome::Mismatch("digest mismatch")
    }
}

/// Compute the base64-encoded HMAC-SHA256 digest of `body` under
/// `channel_secret`. Test helper; matches LINE's wire format so
/// callers minting fake webhooks can mimic the server.
#[must_use]
pub fn compute_base64(channel_secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(channel_secret.as_bytes()).expect("HMAC key");
    mac.update(body);
    STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_when_digest_matches() {
        let body = b"{\"events\":[]}";
        let sig = compute_base64("topsecret", body);
        assert_eq!(verify(body, "topsecret", Some(&sig)), SignatureOutcome::Ok);
    }

    #[test]
    fn ok_when_header_has_surrounding_whitespace() {
        let body = b"x";
        let sig = compute_base64("s", body);
        let padded = format!("  {sig}  ");
        assert_eq!(verify(body, "s", Some(&padded)), SignatureOutcome::Ok);
    }

    #[test]
    fn header_missing_reported() {
        assert_eq!(verify(b"x", "s", None), SignatureOutcome::HeaderMissing);
    }

    #[test]
    fn non_base64_reported() {
        assert_eq!(
            verify(b"x", "s", Some("***not base64***")),
            SignatureOutcome::Mismatch("not base64")
        );
    }

    #[test]
    fn wrong_secret_is_mismatch() {
        let body = b"hi";
        let sig = compute_base64("right", body);
        assert_eq!(
            verify(body, "wrong", Some(&sig)),
            SignatureOutcome::Mismatch("digest mismatch")
        );
    }

    #[test]
    fn tampered_body_mismatch() {
        let signed = b"original";
        let sig = compute_base64("k", signed);
        assert_eq!(
            verify(b"tampered", "k", Some(&sig)),
            SignatureOutcome::Mismatch("digest mismatch")
        );
    }

    #[test]
    fn compute_base64_is_stable_and_44_chars() {
        let a = compute_base64("k", b"body");
        let b = compute_base64("k", b"body");
        assert_eq!(a, b);
        // SHA-256 is 32 bytes → 44 chars base64.
        assert_eq!(a.len(), 44);
    }
}
