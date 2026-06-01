//! Slack request-signature verification.
//!
//! Slack signs every Events API request with HMAC-SHA256 over
//! `v0:<timestamp>:<body>` using the workspace signing secret, and sends
//! the result in `X-Slack-Signature` as `v0=<hex>`. We also enforce a
//! 5-minute timestamp drift to thwart replay.

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Maximum tolerated skew between the request timestamp and "now".
pub const MAX_TIMESTAMP_DRIFT_SECS: i64 = 5 * 60;

/// Reason a signature check rejected a request.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignatureError {
    #[error("missing X-Slack-Signature header")]
    MissingSignature,
    #[error("missing X-Slack-Request-Timestamp header")]
    MissingTimestamp,
    #[error("X-Slack-Request-Timestamp is not a valid integer")]
    BadTimestamp,
    #[error("request timestamp drift exceeds permitted window")]
    StaleTimestamp,
    #[error("signature does not have the v0= prefix")]
    BadSignatureFormat,
    #[error("signature digest length does not match SHA-256")]
    SignatureLength,
    #[error("signature mismatch")]
    Mismatch,
}

/// Compute the expected `v0=<hex>` value for a request body.
///
/// Public so tests (and the adapter's own request signing for replay) can
/// produce a matching header.
#[must_use]
pub fn compute_signature(signing_secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mac = hmac_sha256(
        signing_secret.as_bytes(),
        &assemble(timestamp.as_bytes(), body),
    );
    format!("v0={}", hex::encode(mac))
}

/// Verify that `signature_header` matches the body and timestamp.
///
/// `now_secs` is the current Unix time in seconds (taken from the caller so
/// tests are deterministic).
pub fn verify_signature(
    signing_secret: &str,
    timestamp_header: Option<&str>,
    signature_header: Option<&str>,
    body: &[u8],
    now_secs: i64,
) -> Result<(), SignatureError> {
    let ts = timestamp_header.ok_or(SignatureError::MissingTimestamp)?;
    let sig = signature_header.ok_or(SignatureError::MissingSignature)?;
    let ts_int: i64 = ts.parse().map_err(|_| SignatureError::BadTimestamp)?;
    if (now_secs - ts_int).abs() > MAX_TIMESTAMP_DRIFT_SECS {
        return Err(SignatureError::StaleTimestamp);
    }
    let hex_part = sig
        .strip_prefix("v0=")
        .ok_or(SignatureError::BadSignatureFormat)?;
    let provided = hex::decode(hex_part).map_err(|_| SignatureError::BadSignatureFormat)?;
    if provided.len() != 32 {
        return Err(SignatureError::SignatureLength);
    }
    let expected = hmac_sha256(signing_secret.as_bytes(), &assemble(ts.as_bytes(), body));
    if constant_time_eq(&expected, &provided) {
        Ok(())
    } else {
        Err(SignatureError::Mismatch)
    }
}

fn assemble(timestamp: &[u8], body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(3 + timestamp.len() + 1 + body.len());
    buf.extend_from_slice(b"v0:");
    buf.extend_from_slice(timestamp);
    buf.push(b':');
    buf.extend_from_slice(body);
    buf
}

/// HMAC-SHA256 implemented in terms of two SHA-256 invocations (RFC 2104).
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(key);
        let digest = h.finalize();
        key_block[..32].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = key_block[i] ^ 0x36;
        opad[i] = key_block[i] ^ 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let out = outer.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
    const TS: &str = "1531420618";
    const BODY: &[u8] = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";

    #[test]
    fn compute_matches_published_example() {
        // Verified against the Slack docs example
        // (https://api.slack.com/authentication/verifying-requests-from-slack).
        let sig = compute_signature(SECRET, TS, BODY);
        assert_eq!(
            sig,
            "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503"
        );
    }

    #[test]
    fn verify_accepts_matching_signature() {
        let sig = compute_signature(SECRET, TS, BODY);
        verify_signature(
            SECRET,
            Some(TS),
            Some(&sig),
            BODY,
            TS.parse::<i64>().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_missing_signature_header() {
        let res = verify_signature(SECRET, Some(TS), None, BODY, TS.parse().unwrap());
        assert_eq!(res, Err(SignatureError::MissingSignature));
    }

    #[test]
    fn verify_rejects_missing_timestamp_header() {
        let res = verify_signature(SECRET, None, Some("v0=00"), BODY, 0);
        assert_eq!(res, Err(SignatureError::MissingTimestamp));
    }

    #[test]
    fn verify_rejects_bad_timestamp() {
        let res = verify_signature(SECRET, Some("not-a-number"), Some("v0=00"), BODY, 0);
        assert_eq!(res, Err(SignatureError::BadTimestamp));
    }

    #[test]
    fn verify_rejects_stale_timestamp() {
        let sig = compute_signature(SECRET, TS, BODY);
        let res = verify_signature(SECRET, Some(TS), Some(&sig), BODY, 9_999_999_999);
        assert_eq!(res, Err(SignatureError::StaleTimestamp));
    }

    #[test]
    fn verify_rejects_signature_without_prefix() {
        let res = verify_signature(SECRET, Some(TS), Some("abcd"), BODY, TS.parse().unwrap());
        assert_eq!(res, Err(SignatureError::BadSignatureFormat));
    }

    #[test]
    fn verify_rejects_non_hex_signature() {
        let res = verify_signature(SECRET, Some(TS), Some("v0=ZZ"), BODY, TS.parse().unwrap());
        assert_eq!(res, Err(SignatureError::BadSignatureFormat));
    }

    #[test]
    fn verify_rejects_short_signature() {
        let res = verify_signature(SECRET, Some(TS), Some("v0=abab"), BODY, TS.parse().unwrap());
        assert_eq!(res, Err(SignatureError::SignatureLength));
    }

    #[test]
    fn verify_rejects_wrong_signature() {
        // 32 zero bytes — same length, wrong digest.
        let bad = format!("v0={}", "00".repeat(32));
        let res = verify_signature(SECRET, Some(TS), Some(&bad), BODY, TS.parse().unwrap());
        assert_eq!(res, Err(SignatureError::Mismatch));
    }

    #[test]
    fn verify_accepts_drift_within_window() {
        let sig = compute_signature(SECRET, TS, BODY);
        let now: i64 = TS.parse::<i64>().unwrap() + 60;
        verify_signature(SECRET, Some(TS), Some(&sig), BODY, now).unwrap();
    }

    #[test]
    fn hmac_handles_long_key() {
        // Key longer than the block size is hashed first.
        let long_key = vec![b'a'; 200];
        let out = hmac_sha256(&long_key, b"msg");
        assert_eq!(out.len(), 32);
    }

    #[test]
    fn constant_time_eq_handles_length_mismatch() {
        assert!(!constant_time_eq(&[1, 2, 3], &[1, 2]));
        assert!(constant_time_eq(&[1, 2, 3], &[1, 2, 3]));
        assert!(!constant_time_eq(&[1, 2, 3], &[1, 2, 4]));
    }

    #[test]
    fn signature_error_display_unique_per_variant() {
        let variants = [
            SignatureError::MissingSignature,
            SignatureError::MissingTimestamp,
            SignatureError::BadTimestamp,
            SignatureError::StaleTimestamp,
            SignatureError::BadSignatureFormat,
            SignatureError::SignatureLength,
            SignatureError::Mismatch,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in &variants {
            assert!(seen.insert(format!("{v}")));
            // Debug derive coverage.
            let _ = format!("{v:?}");
        }
    }
}
