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
//! host **auto-injects** the *real* model endpoint into every group's
//! effective allow-list — derived from whichever base URL the group's
//! resolved provider actually talks to, not a hardcoded
//! `api.anthropic.com` assumption:
//!
//!   - **anthropic / openrouter / ollama-shim** (and any unknown provider
//!     that speaks the Anthropic envelope) use `ANTHROPIC_BASE_URL`,
//!     defaulting to `https://api.anthropic.com` when unset — matching
//!     [`copperclaw_providers::anthropic::DEFAULT_BASE_URL`].
//!   - **ollama** (native) uses `OLLAMA_BASE_URL`, defaulting to
//!     `http://localhost:11434` when unset — matching the runner's own
//!     ollama-provider fallback.
//!   - **codex** has no HTTP endpoint (it brokers a local subprocess), so
//!     nothing is injected.
//!
//! Crucially the default is applied even when the env var is *unset*, so
//! the common single-provider deployment (Anthropic with no
//! `ANTHROPIC_BASE_URL` override) still reaches `api.anthropic.com:443`
//! under deny-default. The earlier code only injected when
//! `ANTHROPIC_BASE_URL` was explicitly set, which black-holed exactly that
//! default deployment.
//!
//! The per-group `egress_allow` configured by the operator
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

/// The Anthropic-shaped provider's default base URL, mirrored from
/// [`copperclaw_providers::anthropic::DEFAULT_BASE_URL`]. Used as the
/// fallback when `ANTHROPIC_BASE_URL` is unset so the common
/// single-provider Anthropic deployment still reaches `api.anthropic.com`
/// under deny-default.
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// The native-ollama provider's default base URL, mirrored from the
/// runner's `build_provider` ollama arm. The host injects this when
/// `OLLAMA_BASE_URL` is unset so a deny-default ollama group still reaches
/// its model endpoint. (In a typical Docker deployment the operator
/// overrides this to `http://172.17.0.1:11434` or `host.docker.internal`;
/// either way we inject whatever they configured.)
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Resolve the *actual* model-endpoint base URL a group's provider talks
/// to, so deny-default injects the real host:port rather than a hardcoded
/// `api.anthropic.com`.
///
/// `provider` is the group's already-resolved provider string (after the
/// same precedence + alias normalisation the runner config applies:
/// `session.agent_provider` → per-group config → host default, with
/// `"claude"` ⇒ `"anthropic"`). `anthropic_base_url` is the current
/// rotatable `ANTHROPIC_BASE_URL`; `ollama_base_url` is the forwarded
/// `OLLAMA_BASE_URL`. Both are the post-`.env` values (empty strings are
/// already filtered to `None` upstream).
///
/// Returns the base URL string to parse into a host:port entry, or `None`
/// when the provider has no HTTP model endpoint (`codex`).
#[must_use]
pub fn model_base_url_for_provider(
    provider: Option<&str>,
    anthropic_base_url: Option<&str>,
    ollama_base_url: Option<&str>,
) -> Option<String> {
    match provider {
        // Native ollama talks to OLLAMA_BASE_URL, falling back to the
        // runner's own localhost default when unset.
        Some("ollama") => Some(
            ollama_base_url
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(DEFAULT_OLLAMA_BASE_URL)
                .to_string(),
        ),
        // Codex brokers a local subprocess — no HTTP endpoint to allow.
        Some("codex") => None,
        // Everything else speaks the Anthropic envelope (anthropic,
        // openrouter via ANTHROPIC_BASE_URL, ollama-shim, and any unknown
        // provider the runner falls back to anthropic for). Default to the
        // canonical Anthropic endpoint when ANTHROPIC_BASE_URL is unset so
        // the common no-override deployment is never black-holed.
        _ => Some(
            anthropic_base_url
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(DEFAULT_ANTHROPIC_BASE_URL)
                .to_string(),
        ),
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

/// Resolve the effective egress allow-list for a spawn, given the group's
/// resolved provider and the host's current model-endpoint env.
///
/// Derives the real model base URL via [`model_base_url_for_provider`]
/// (provider-aware: Anthropic/OpenRouter from `ANTHROPIC_BASE_URL`, ollama
/// from `OLLAMA_BASE_URL`, each with its real default when unset), parses
/// it into a `host:port` entry, and unions it with the operator-configured
/// per-group `group_allow`. The model entry is prepended (and deduplicated)
/// so deny-default can never blackhole model traffic even if the operator
/// forgot to list it — and even when the group's allow-list is empty. Order
/// is otherwise preserved and duplicates are removed.
#[must_use]
pub fn resolve_allow_list_for_provider(
    group_allow: &[String],
    provider: Option<&str>,
    anthropic_base_url: Option<&str>,
    ollama_base_url: Option<&str>,
) -> Vec<String> {
    let base_url = model_base_url_for_provider(provider, anthropic_base_url, ollama_base_url);
    resolve_allow_list(group_allow, base_url.as_deref())
}

/// Resolve the effective egress allow-list for a spawn from an
/// already-derived model `base_url`.
///
/// Unions the operator-configured per-group `group_allow` with the
/// auto-injected model endpoint (parsed from `base_url`). The model entry is
/// prepended (and deduplicated) so deny-default can never blackhole model
/// traffic even if the operator forgot to list it. Order is otherwise
/// preserved and duplicates are removed. Prefer
/// [`resolve_allow_list_for_provider`] on the spawn path so the injected
/// endpoint is provider-correct.
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

    // --- Provider-aware model-endpoint derivation (migration-safety fix) ---

    #[test]
    fn model_base_url_anthropic_default_when_base_unset() {
        // The core migration-safety case: an Anthropic group with NO
        // ANTHROPIC_BASE_URL override must still resolve to the real
        // api.anthropic.com endpoint, not None.
        assert_eq!(
            model_base_url_for_provider(Some("anthropic"), None, None).as_deref(),
            Some("https://api.anthropic.com")
        );
        // The "claude" alias is normalised to "anthropic" upstream, so a
        // None/empty provider (default) also takes the Anthropic path.
        assert_eq!(
            model_base_url_for_provider(None, None, None).as_deref(),
            Some("https://api.anthropic.com")
        );
        // An empty-string ANTHROPIC_BASE_URL is treated as unset.
        assert_eq!(
            model_base_url_for_provider(Some("anthropic"), Some("   "), None).as_deref(),
            Some("https://api.anthropic.com")
        );
    }

    #[test]
    fn model_base_url_openrouter_uses_anthropic_base() {
        // OpenRouter rides the Anthropic envelope via ANTHROPIC_BASE_URL.
        assert_eq!(
            model_base_url_for_provider(
                Some("anthropic"),
                Some("https://openrouter.ai/api/v1"),
                None,
            )
            .as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        // ollama-shim also speaks the Anthropic envelope at a proxy URL.
        assert_eq!(
            model_base_url_for_provider(
                Some("ollama-shim"),
                Some("https://proxy.internal:8443"),
                Some("http://ignored:11434"),
            )
            .as_deref(),
            Some("https://proxy.internal:8443")
        );
    }

    #[test]
    fn model_base_url_ollama_uses_ollama_base_with_localhost_default() {
        // Native ollama reads OLLAMA_BASE_URL — NOT ANTHROPIC_BASE_URL.
        assert_eq!(
            model_base_url_for_provider(
                Some("ollama"),
                Some("https://api.anthropic.com"),
                Some("http://172.17.0.1:11434"),
            )
            .as_deref(),
            Some("http://172.17.0.1:11434")
        );
        // Unset OLLAMA_BASE_URL → runner's localhost fallback, never the
        // Anthropic endpoint and never None.
        assert_eq!(
            model_base_url_for_provider(Some("ollama"), Some("https://api.anthropic.com"), None)
                .as_deref(),
            Some("http://localhost:11434")
        );
    }

    #[test]
    fn model_base_url_codex_has_no_endpoint() {
        // Codex brokers a local subprocess — nothing to inject.
        assert_eq!(
            model_base_url_for_provider(Some("codex"), Some("https://api.anthropic.com"), None),
            None
        );
    }

    #[test]
    fn resolve_for_provider_openrouter_injects_openrouter() {
        let resolved = resolve_allow_list_for_provider(
            &[],
            Some("anthropic"),
            Some("https://openrouter.ai/api/v1"),
            None,
        );
        assert_eq!(resolved, vec!["openrouter.ai:443".to_string()]);
    }

    #[test]
    fn resolve_for_provider_default_anthropic_injects_api_anthropic() {
        // Default/unset Anthropic base + empty group allow-list → the model
        // endpoint is STILL injected (no black-hole), defaulting to
        // api.anthropic.com:443.
        let resolved = resolve_allow_list_for_provider(&[], Some("anthropic"), None, None);
        assert_eq!(resolved, vec!["api.anthropic.com:443".to_string()]);
        // Same for the unset/default provider.
        let resolved_default = resolve_allow_list_for_provider(&[], None, None, None);
        assert_eq!(resolved_default, vec!["api.anthropic.com:443".to_string()]);
    }

    #[test]
    fn resolve_for_provider_ollama_injects_ollama_host() {
        let resolved = resolve_allow_list_for_provider(
            &[],
            Some("ollama"),
            Some("https://api.anthropic.com"),
            Some("http://host.docker.internal:11434"),
        );
        assert_eq!(resolved, vec!["host.docker.internal:11434".to_string()]);
    }

    #[test]
    fn resolve_for_provider_empty_group_under_deny_default_still_reaches_model() {
        // The headline guarantee: an empty per-group allow-list under
        // deny-default never produces an empty resolved list for an
        // HTTP-backed provider — the model endpoint is always present.
        for (provider, anth, olla, expect) in [
            (Some("anthropic"), None, None, "api.anthropic.com:443"),
            (None, None, None, "api.anthropic.com:443"),
            (
                Some("anthropic"),
                Some("https://openrouter.ai/api/v1"),
                None,
                "openrouter.ai:443",
            ),
            (Some("ollama"), None, None, "localhost:11434"),
            (
                Some("ollama"),
                None,
                Some("http://172.17.0.1:11434"),
                "172.17.0.1:11434",
            ),
        ] {
            let resolved = resolve_allow_list_for_provider(&[], provider, anth, olla);
            assert!(
                !resolved.is_empty(),
                "empty resolved list would black-hole {provider:?}"
            );
            assert_eq!(resolved[0], expect, "wrong endpoint for {provider:?}");
        }
    }

    #[test]
    fn resolve_for_provider_unions_group_after_model_endpoint() {
        let group = vec![
            "db.local:5432".to_string(),
            // Operator already listed the model endpoint — must not double.
            "openrouter.ai:443".to_string(),
        ];
        let resolved = resolve_allow_list_for_provider(
            &group,
            Some("anthropic"),
            Some("https://openrouter.ai/api/v1"),
            None,
        );
        assert_eq!(
            resolved,
            vec!["openrouter.ai:443".to_string(), "db.local:5432".to_string(),]
        );
    }

    #[test]
    fn resolve_for_provider_codex_keeps_group_only() {
        // Codex has no HTTP endpoint, so nothing is injected; the group's
        // own list passes through unchanged.
        let group = vec!["db.local:5432".to_string()];
        let resolved = resolve_allow_list_for_provider(
            &group,
            Some("codex"),
            Some("https://api.anthropic.com"),
            Some("http://localhost:11434"),
        );
        assert_eq!(resolved, group);
    }
}
