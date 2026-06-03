//! Image + runner-binary attestation primitives.
//!
//! Today copperclaw's session images are sha256 *content-addressed* (the tag
//! is `copperclaw/session:sha256-<fingerprint>`), which protects against
//! accidental drift but is NOT a cryptographic signature — anyone who can
//! write to the local image store could replace the image behind a tag.
//!
//! This module provides the pure core for a real digest comparison:
//!
//! - [`normalize_digest`] canonicalises a digest string (lowercases hex,
//!   strips an optional `sha256:` / `sha256-` prefix) so two spellings of the
//!   same digest compare equal.
//! - [`DigestComparison`] is the result of comparing an *observed* digest
//!   (what the runtime reports for the image about to be / actually spawned)
//!   against an *expected* digest (recorded at a prior trusted point — e.g.
//!   the digest pinned at the last `rebuild.sh`).
//!
//! The privileged/runtime half (querying the daemon for the live digest,
//! recording the pin) lives in the runtime backends and the host; this module
//! is pure so the comparison logic is unit-tested without a daemon. A mismatch
//! is the security signal: the image behind the tag changed out from under us.

/// Outcome of comparing an observed image/runner digest against the expected
/// (recorded) one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestComparison {
    /// Observed digest equals the expected digest — attestation holds.
    Match,
    /// Observed digest differs from the expected digest — the artifact behind
    /// the tag changed. This is the security-relevant case.
    Mismatch,
    /// No expected digest was recorded yet (first boot / fresh install), so
    /// there is nothing to compare against. The caller records the observed
    /// digest as the new baseline rather than treating this as a failure.
    NoBaseline,
    /// The observed digest could not be obtained (runtime didn't report one).
    /// Distinct from `Mismatch` so callers don't cry wolf on a runtime that
    /// simply doesn't expose digests.
    Unknown,
}

impl DigestComparison {
    /// Stable token for logs / audit / JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DigestComparison::Match => "match",
            DigestComparison::Mismatch => "mismatch",
            DigestComparison::NoBaseline => "no-baseline",
            DigestComparison::Unknown => "unknown",
        }
    }

    /// Whether this comparison should be treated as an attestation FAILURE
    /// (i.e. a real mismatch). `NoBaseline` and `Unknown` are not failures —
    /// they're "nothing to assert yet".
    #[must_use]
    pub fn is_failure(self) -> bool {
        matches!(self, DigestComparison::Mismatch)
    }
}

/// Canonicalise a digest string for comparison.
///
/// - Strips a leading `sha256:` or `sha256-` algorithm prefix (Docker reports
///   `sha256:<hex>` for `.Id`; our tags use `sha256-<hex>`).
/// - Lowercases the hex (hex digests are case-insensitive).
/// - Trims surrounding whitespace.
///
/// Returns `None` for an empty / whitespace-only input so a blank digest can't
/// masquerade as a real one.
#[must_use]
pub fn normalize_digest(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = trimmed
        .strip_prefix("sha256:")
        .or_else(|| trimmed.strip_prefix("sha256-"))
        .unwrap_or(trimmed);
    if body.is_empty() {
        return None;
    }
    Some(body.to_ascii_lowercase())
}

/// Compare an observed digest against an expected one, both optional.
///
/// Truth table:
/// - `expected = None`               → [`DigestComparison::NoBaseline`]
/// - `observed = None`               → [`DigestComparison::Unknown`]
/// - both present, normalise equal   → [`DigestComparison::Match`]
/// - both present, normalise differ  → [`DigestComparison::Mismatch`]
///
/// `expected = None` takes precedence over `observed = None`: if we have no
/// baseline there is nothing to assert regardless of whether the runtime
/// reported a live digest.
#[must_use]
pub fn compare_digests(observed: Option<&str>, expected: Option<&str>) -> DigestComparison {
    let Some(expected) = expected.and_then(normalize_digest) else {
        return DigestComparison::NoBaseline;
    };
    let Some(observed) = observed.and_then(normalize_digest) else {
        return DigestComparison::Unknown;
    };
    if observed == expected {
        DigestComparison::Match
    } else {
        DigestComparison::Mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_sha256_colon_prefix() {
        assert_eq!(normalize_digest("sha256:ABC123").as_deref(), Some("abc123"));
    }

    #[test]
    fn normalize_strips_sha256_dash_prefix() {
        assert_eq!(
            normalize_digest("sha256-DEADBEEF").as_deref(),
            Some("deadbeef")
        );
    }

    #[test]
    fn normalize_lowercases_and_trims() {
        assert_eq!(normalize_digest("  AaBbCc  ").as_deref(), Some("aabbcc"));
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(normalize_digest("").is_none());
        assert!(normalize_digest("   ").is_none());
        assert!(normalize_digest("sha256:").is_none());
        assert!(normalize_digest("sha256-").is_none());
    }

    #[test]
    fn compare_match_across_prefixes() {
        let c = compare_digests(Some("sha256:abc"), Some("sha256-ABC"));
        assert_eq!(c, DigestComparison::Match);
        assert!(!c.is_failure());
    }

    #[test]
    fn compare_mismatch_is_failure() {
        let c = compare_digests(Some("sha256:abc"), Some("sha256:def"));
        assert_eq!(c, DigestComparison::Mismatch);
        assert!(c.is_failure());
        assert_eq!(c.as_str(), "mismatch");
    }

    #[test]
    fn compare_no_baseline_when_expected_absent() {
        assert_eq!(
            compare_digests(Some("abc"), None),
            DigestComparison::NoBaseline
        );
        // Even with no observed digest, no baseline wins.
        assert_eq!(compare_digests(None, None), DigestComparison::NoBaseline);
    }

    #[test]
    fn compare_unknown_when_observed_absent_but_expected_present() {
        assert_eq!(
            compare_digests(None, Some("abc")),
            DigestComparison::Unknown
        );
        assert!(!DigestComparison::Unknown.is_failure());
    }

    #[test]
    fn compare_blank_observed_is_unknown() {
        // A blank observed digest must not be treated as a match.
        assert_eq!(
            compare_digests(Some("   "), Some("abc")),
            DigestComparison::Unknown
        );
    }

    #[test]
    fn tokens_are_stable() {
        assert_eq!(DigestComparison::Match.as_str(), "match");
        assert_eq!(DigestComparison::NoBaseline.as_str(), "no-baseline");
        assert_eq!(DigestComparison::Unknown.as_str(), "unknown");
    }
}
