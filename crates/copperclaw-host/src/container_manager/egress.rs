//! Egress allow-list resolution + model-endpoint auto-injection.
//!
//! Phase 0a v1 (Top 10 #6). The host ships a default-deny egress posture
//! that is **opt-in**: [`EgressMode::AllowAll`] (the default) preserves the
//! legacy spawn path, and an operator flips [`EgressMode::DenyDefault`] via
//! `COPPERCLAW_EGRESS_MODE=deny-default` (or `1`/`deny`).
//!
//! ## Migration safety
//!
//! Deny-default must never blackhole the agent's own model traffic. So the
//! host **auto-injects** the model endpoint — derived from the spawning
//! container's `ANTHROPIC_BASE_URL` — into every group's effective
//! allow-list. The per-group `egress_allow` configured by the operator
//! (`cclaw groups config set-egress-allow`) is unioned on top. The result is
//! the *resolved* allow-list handed to the container runtime.
//!
//! ## What is enforced
//!
//! Enforcement lives in `copperclaw-container-rt` (see
//! [`copperclaw_container_rt::EgressMode`]): under deny-default with an empty
//! resolved allow-list bollard cuts the network entirely; with a non-empty
//! list per-host filtering is deferred to a future netns + nftables pass.
//! This module is purely the *policy resolution* half.

use copperclaw_container_rt::EgressMode;

/// Parse the operator-facing `COPPERCLAW_EGRESS_MODE` value into an
/// [`EgressMode`]. Accepts `deny-default` / `deny` / `1` / `true` / `on`
/// (case-insensitive) for deny-default; everything else (including unset and
/// the explicit `allow-all`) keeps the default allow-all posture so the
/// feature is strictly opt-in.
#[must_use]
pub fn parse_egress_mode(raw: Option<&str>) -> EgressMode {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some(
            "deny-default" | "deny_default" | "deny" | "1" | "true" | "yes" | "on" | "enforce",
        ) => EgressMode::DenyDefault,
        _ => EgressMode::AllowAll,
    }
}

/// Derive the `host:port` egress entry for a model endpoint base URL.
///
/// Handles the shapes `ANTHROPIC_BASE_URL` actually takes: a full URL
/// (`https://api.anthropic.com`, `http://host:11434/v1`), or a bare
/// `host[:port]` with no scheme. The port defaults to the scheme default
/// (443 for `https`/none, 80 for `http`) when not explicit. Returns `None`
/// when the input is empty or has no resolvable host (so a malformed
/// `ANTHROPIC_BASE_URL` can never inject a garbage entry).
#[must_use]
pub fn model_endpoint_entry(base_url: &str) -> Option<String> {
    let raw = base_url.trim();
    if raw.is_empty() {
        return None;
    }

    // Split scheme://authority/...  We only need the authority component.
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, r)) => (Some(s.to_ascii_lowercase()), r),
        None => (None, raw),
    };

    // Authority ends at the first '/', '?' or '#'.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Strip any userinfo (`user:pass@host`).
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    if host_port.is_empty() {
        return None;
    }

    let default_port = match scheme.as_deref() {
        Some("http") => 80,
        // https, unknown scheme, or no scheme → assume TLS default.
        _ => 443,
    };

    let (host, port) = split_host_port(host_port, default_port)?;
    if host.is_empty() {
        return None;
    }
    Some(format!("{host}:{port}"))
}

/// Split a `host[:port]` authority into `(host, port)`, applying
/// `default_port` when no explicit port is present. Supports bracketed IPv6
/// literals (`[::1]:443`). Returns `None` when an explicit port is present
/// but not a valid 1..=65535 number.
fn split_host_port(host_port: &str, default_port: u16) -> Option<(String, u16)> {
    // Bracketed IPv6: `[::1]` or `[::1]:443`.
    if let Some(stripped) = host_port.strip_prefix('[') {
        let (host, after) = stripped.split_once(']')?;
        let port = match after.strip_prefix(':') {
            None | Some("") => default_port,
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
        };
        return Some((format!("[{host}]"), port));
    }

    // Bare IPv6 with multiple colons but no brackets — treat the whole
    // thing as the host (no port), since we can't tell where the port is.
    if host_port.matches(':').count() > 1 {
        return Some((host_port.to_string(), default_port));
    }

    match host_port.split_once(':') {
        Some((host, p)) => {
            let port = p.parse::<u16>().ok().filter(|p| *p != 0)?;
            Some((host.to_string(), port))
        }
        None => Some((host_port.to_string(), default_port)),
    }
}

/// Resolve the effective egress allow-list for a spawn.
///
/// Unions the operator-configured per-group `group_allow` with the
/// auto-injected model endpoint (from `base_url`). The model entry is
/// prepended (and deduplicated) so deny-default can never blackhole model
/// traffic even if the operator forgot to list it. Order is otherwise
/// preserved and duplicates are removed.
#[must_use]
pub fn resolve_allow_list(group_allow: &[String], base_url: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(group_allow.len() + 1);
    if let Some(entry) = base_url.and_then(model_endpoint_entry) {
        out.push(entry);
    }
    for e in group_allow {
        if !out.contains(e) {
            out.push(e.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_egress_mode_is_opt_in() {
        // Default / unset / explicit allow-all all stay AllowAll.
        assert_eq!(parse_egress_mode(None), EgressMode::AllowAll);
        assert_eq!(parse_egress_mode(Some("")), EgressMode::AllowAll);
        assert_eq!(parse_egress_mode(Some("allow-all")), EgressMode::AllowAll);
        assert_eq!(parse_egress_mode(Some("off")), EgressMode::AllowAll);
        // Opt-in spellings flip to DenyDefault.
        for v in [
            "deny-default",
            "DENY-DEFAULT",
            "deny_default",
            "deny",
            "1",
            "true",
            "on",
            "  enforce  ",
        ] {
            assert_eq!(
                parse_egress_mode(Some(v)),
                EgressMode::DenyDefault,
                "value {v:?} should enable deny-default"
            );
        }
    }

    #[test]
    fn model_endpoint_full_https_url_defaults_to_443() {
        assert_eq!(
            model_endpoint_entry("https://api.anthropic.com"),
            Some("api.anthropic.com:443".into())
        );
        assert_eq!(
            model_endpoint_entry("https://api.anthropic.com/v1/messages"),
            Some("api.anthropic.com:443".into())
        );
    }

    #[test]
    fn model_endpoint_http_defaults_to_80() {
        assert_eq!(
            model_endpoint_entry("http://router.local/api"),
            Some("router.local:80".into())
        );
    }

    #[test]
    fn model_endpoint_explicit_port_wins() {
        assert_eq!(
            model_endpoint_entry("http://host.docker.internal:11434/v1"),
            Some("host.docker.internal:11434".into())
        );
        assert_eq!(
            model_endpoint_entry("https://openrouter.ai:8443/api/v1"),
            Some("openrouter.ai:8443".into())
        );
    }

    #[test]
    fn model_endpoint_bare_host_no_scheme() {
        assert_eq!(
            model_endpoint_entry("api.example.com"),
            Some("api.example.com:443".into())
        );
        assert_eq!(
            model_endpoint_entry("api.example.com:9000"),
            Some("api.example.com:9000".into())
        );
    }

    #[test]
    fn model_endpoint_strips_userinfo() {
        assert_eq!(
            model_endpoint_entry("https://user:pass@gw.internal:8080/v1"),
            Some("gw.internal:8080".into())
        );
    }

    #[test]
    fn model_endpoint_ipv6_bracketed() {
        assert_eq!(
            model_endpoint_entry("http://[::1]:11434/v1"),
            Some("[::1]:11434".into())
        );
        assert_eq!(
            model_endpoint_entry("https://[2001:db8::1]"),
            Some("[2001:db8::1]:443".into())
        );
    }

    #[test]
    fn model_endpoint_rejects_empty_and_garbage_port() {
        assert_eq!(model_endpoint_entry(""), None);
        assert_eq!(model_endpoint_entry("   "), None);
        // Explicit but invalid port → reject rather than inject garbage.
        assert_eq!(model_endpoint_entry("https://h:0"), None);
        assert_eq!(model_endpoint_entry("https://h:99999"), None);
        assert_eq!(model_endpoint_entry("https://h:abc"), None);
        // Scheme with no host.
        assert_eq!(model_endpoint_entry("https://"), None);
    }

    #[test]
    fn resolve_injects_model_endpoint_even_when_group_empty() {
        let resolved = resolve_allow_list(&[], Some("https://api.anthropic.com"));
        assert_eq!(resolved, vec!["api.anthropic.com:443".to_string()]);
    }

    #[test]
    fn resolve_unions_and_dedupes() {
        let group = vec![
            "db.local:5432".to_string(),
            // Operator already listed the model endpoint — must not double.
            "api.anthropic.com:443".to_string(),
        ];
        let resolved = resolve_allow_list(&group, Some("https://api.anthropic.com"));
        assert_eq!(
            resolved,
            vec![
                "api.anthropic.com:443".to_string(),
                "db.local:5432".to_string(),
            ]
        );
    }

    #[test]
    fn resolve_without_base_url_keeps_group_only() {
        let group = vec!["db.local:5432".to_string()];
        let resolved = resolve_allow_list(&group, None);
        assert_eq!(resolved, group);
    }

    #[test]
    fn resolve_with_unparseable_base_url_keeps_group_only() {
        let group = vec!["db.local:5432".to_string()];
        let resolved = resolve_allow_list(&group, Some(""));
        assert_eq!(resolved, group);
    }
}
