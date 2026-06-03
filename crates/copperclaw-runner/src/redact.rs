//! Secret redaction for log / transcript output.
//!
//! Provider keys and bearer tokens must never reach `copperclaw.log`, the
//! runner's stdout, or any transcript line. The provider error bodies and
//! tool inputs the runner logs are the most common leak vector (a 401 body
//! echoes the offending `Authorization` header on some gateways; a misrouted
//! `set_env` can splash a key into a breadcrumb), so we scrub the shapes that
//! identify a secret *before* the string is handed to `tracing` or written
//! out.
//!
//! Implemented with a hand-rolled scanner rather than `regex` so neither this
//! crate nor `copperclaw-host` has to pull `regex` into its *runtime*
//! dependency set just for log hygiene. The shapes we match are simple
//! enough (a known prefix + a run of token characters, or `Bearer <token>`)
//! that a linear scan is both clearer and faster than a backtracking regex.
//!
//! Matched shapes:
//! - `sk-…` / `sk-or-v1-…` and similar `sk-`-prefixed provider keys.
//! - `Bearer <token>` (the token after a literal `Bearer ` / `bearer `).
//! - Long opaque secret-ish runs (>= [`MIN_SECRET_RUN`] token chars) that
//!   look like an API key even without a recognised prefix.
//!
//! Ordinary prose, short identifiers, UUIDs, hex SHAs, and numbers are left
//! untouched — the floor on run length plus the "must mix in a non-hex /
//! non-trivial character" heuristic keeps false positives off normal text.

/// What replaces a redacted secret in the output. Kept short and obvious so
/// an operator scanning a log can tell at a glance that a value was scrubbed.
pub const REDACTED: &str = "[REDACTED]";

/// Minimum length of an unprefixed token run before we treat it as a
/// candidate secret. Real provider keys are long (Anthropic `sk-ant-…` keys
/// run ~100 chars, `OpenAI` `sk-…` ~50, `OpenRouter` `sk-or-v1-…` ~70); 40 is
/// comfortably above ordinary identifiers, file paths, and git SHAs (40 hex
/// chars — excluded separately by the all-hex check) while staying below the
/// shortest real key shape we care about.
const MIN_SECRET_RUN: usize = 40;

/// Is `c` a character that can appear inside a token/secret run? Covers the
/// alphabet of base64url / hex / the `sk-…`-style keys: ASCII alphanumerics
/// plus `-` and `_`. Deliberately excludes `.` and `/` so we don't swallow
/// file paths, version strings, or URLs wholesale.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// True when `run` looks like an opaque secret rather than ordinary text:
/// long enough, and NOT a pure hex string (those are git SHAs / content
/// hashes, which are not secrets and appear constantly in logs). A real API
/// key mixes upper/lower/digits and usually `-`/`_`, so requiring at least
/// one non-hex-digit character is a cheap, effective discriminator.
fn looks_like_secret(run: &str) -> bool {
    if run.len() < MIN_SECRET_RUN {
        return false;
    }
    // All-hex (lower or upper) → SHA / content hash, not a secret.
    let all_hex = run.chars().all(|c| c.is_ascii_hexdigit());
    if all_hex {
        return false;
    }
    // Require a digit AND a letter so a long all-letter word (rare, but
    // possible in prose / base64-less identifiers) doesn't trip the floor.
    let has_digit = run.chars().any(|c| c.is_ascii_digit());
    let has_alpha = run.chars().any(|c| c.is_ascii_alphabetic());
    has_digit && has_alpha
}

/// Scrub provider-key / token shapes out of `input`, returning an owned
/// `String` with each match replaced by [`REDACTED`].
///
/// Cheap fast-path: if the input contains none of the trigger substrings
/// (`sk-`, `Bearer`/`bearer`) and is shorter than [`MIN_SECRET_RUN`], it can
/// hold no match, so we hand back the borrowed-then-owned input without a
/// second pass. Hot log lines (most of them) take this path.
#[must_use]
pub fn redact_secrets(input: &str) -> String {
    // Fast path: nothing that could match.
    if input.len() < MIN_SECRET_RUN
        && !input.contains("sk-")
        && !input.contains("Bearer")
        && !input.contains("bearer")
    {
        return input.to_string();
    }

    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        // Only consider matches at a token boundary so we don't redact the
        // `sk-` inside e.g. `task-sk-foo`. A boundary is start-of-string or a
        // preceding non-token byte.
        let at_boundary = i == 0 || !is_token_char(input[..i].chars().next_back().unwrap_or(' '));

        // `Bearer <token>` / `bearer <token>`.
        if at_boundary {
            if let Some(rest) = strip_prefix_ci(&input[i..], "bearer ") {
                let prefix_len = input[i..].len() - rest.len();
                let token_len = token_run_len(rest.as_bytes());
                if token_len > 0 {
                    out.push_str(&input[i..i + prefix_len]);
                    out.push_str(REDACTED);
                    i += prefix_len + token_len;
                    continue;
                }
            }
        }

        // `sk-`-prefixed key (covers `sk-`, `sk-or-v1-`, `sk-ant-…`, etc.):
        // the whole `sk-` + token run is the secret.
        if at_boundary && input[i..].starts_with("sk-") {
            let run_len = token_run_len(&bytes[i..]);
            // `sk-` is 3 chars; require something after it.
            if run_len > 3 {
                out.push_str(REDACTED);
                i += run_len;
                continue;
            }
        }

        // Generic long opaque run.
        if at_boundary {
            let run_len = token_run_len(&bytes[i..]);
            if run_len >= MIN_SECRET_RUN && looks_like_secret(&input[i..i + run_len]) {
                out.push_str(REDACTED);
                i += run_len;
                continue;
            }
        }

        // No match here: copy one full char (UTF-8 safe) and advance.
        let ch = input[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Length in bytes of the leading run of token characters in `bytes`
/// (ASCII-only, so byte length == char count for the run).
fn token_run_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|&&b| is_token_char(b as char))
        .count()
}

/// Case-insensitive `str::strip_prefix` for ASCII `prefix`. Returns the
/// remainder after the prefix, or `None` when `s` doesn't start with it.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let pb = prefix.as_bytes();
    let sb = s.as_bytes();
    if sb.len() < pb.len() {
        return None;
    }
    for (a, b) in sb.iter().zip(pb.iter()) {
        if !a.eq_ignore_ascii_case(b) {
            return None;
        }
    }
    Some(&s[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_openai_style_sk_key() {
        let input = "auth failed for key sk-abcdEF0123456789abcdEF0123456789 today";
        let out = redact_secrets(input);
        assert!(out.contains(REDACTED), "expected redaction, got: {out}");
        assert!(!out.contains("sk-abcd"), "raw key leaked: {out}");
        assert!(out.starts_with("auth failed for key "));
        assert!(out.ends_with(" today"));
    }

    #[test]
    fn redacts_openrouter_sk_or_v1_key() {
        let input = "OPENROUTER=sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef";
        let out = redact_secrets(input);
        assert!(!out.contains("sk-or-v1-0123"), "raw key leaked: {out}");
        assert!(out.contains(REDACTED));
        assert_eq!(out, format!("OPENROUTER={REDACTED}"));
    }

    #[test]
    fn redacts_anthropic_sk_ant_key() {
        let key = "sk-ant-api03-AbCdEf0123456789AbCdEf0123456789AbCdEf0123456789xyz";
        let out = redact_secrets(key);
        assert_eq!(out, REDACTED);
    }

    #[test]
    fn redacts_bearer_token_preserving_keyword() {
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiabcdefABCDEF0123456789xyz";
        let out = redact_secrets(input);
        assert!(out.contains("Bearer "), "keyword should survive: {out}");
        assert!(!out.contains("eyJhbGc"), "token leaked: {out}");
        assert_eq!(out, format!("Authorization: Bearer {REDACTED}"));
    }

    #[test]
    fn redacts_lowercase_bearer() {
        let out = redact_secrets("bearer abc123DEF456ghi789JKL0");
        assert_eq!(out, format!("bearer {REDACTED}"));
    }

    #[test]
    fn redacts_long_opaque_run() {
        // A 48-char mixed token with no known prefix.
        let secret = "Ab3Cd9Ef2Gh5Ij8Kl1Mn4Op7Qr0St3Uv6Wx9Yz2Bc5De8Fg";
        let out = redact_secrets(&format!("token={secret}"));
        assert_eq!(out, format!("token={REDACTED}"));
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let cases = [
            "the agent replied with a friendly greeting",
            "provider deadline exceeded after 60000ms (attempt 3/3)",
            "GET /v1/messages 200 in 1423ms",
            "session 019e4905-e124-7d61-8b46-728b53a72fc5 spawned",
            "rebuilt image copperclaw-session:abc123",
        ];
        for c in cases {
            assert_eq!(redact_secrets(c), c, "false-positive redaction on: {c}");
        }
    }

    #[test]
    fn leaves_git_sha_untouched() {
        // 40-char all-hex string is a SHA, not a secret.
        let sha = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        assert_eq!(sha.len(), 40);
        assert_eq!(redact_secrets(sha), sha);
    }

    #[test]
    fn does_not_redact_sk_substring_mid_token() {
        // `task-sk-...` — the `sk-` is not at a token boundary, so it is not
        // a key prefix and (being short) the whole run is left alone.
        let input = "ran task-sk-build now";
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn empty_and_short_inputs_pass_through() {
        assert_eq!(redact_secrets(""), "");
        assert_eq!(redact_secrets("ok"), "ok");
        assert_eq!(redact_secrets("short message"), "short message");
    }

    #[test]
    fn redacts_multiple_secrets_in_one_line() {
        let input = "primary sk-AAAA1111BBBB2222CCCC3333DDDD4444 and Bearer ZZZ999YYY888XXX777www0";
        let out = redact_secrets(input);
        assert!(!out.contains("sk-AAAA"), "first key leaked: {out}");
        assert!(!out.contains("ZZZ999"), "bearer token leaked: {out}");
        assert_eq!(
            out.matches(REDACTED).count(),
            2,
            "both secrets redacted: {out}"
        );
    }
}
