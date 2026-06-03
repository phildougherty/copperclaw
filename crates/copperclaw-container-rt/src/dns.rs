//! DNS-filtering core for the deny-default egress posture (Phase 0a v2,
//! Part A).
//!
//! Under [`crate::EgressMode::DenyDefault`] a session container must not be
//! able to exfiltrate data by encoding it into DNS labels and shipping those
//! to an arbitrary resolver. The defence is two-fold:
//!
//!   1. **Pin the resolver.** Rewrite the container's `/etc/resolv.conf` to
//!      point at a single host-controlled filtering resolver
//!      ([`resolv_conf_contents`]). The container can no longer pick
//!      `8.8.8.8` or any other upstream — every lookup goes through the
//!      filter.
//!   2. **Answer only the allow-list.** Configure that resolver to resolve
//!      ONLY the group's effective allow-listed names (the model endpoint
//!      host plus the operator allow-list) and to refuse everything else
//!      ([`FilterResolverConfig`] / [`dnsmasq_conf`]). A name not on the list
//!      gets `NXDOMAIN`, so the container can't even learn the address of an
//!      exfiltration host, let alone reach it.
//!
//! Everything in this module is a **pure function**: it turns the resolved
//! `host:port` allow-list (the same list [`crate::ContainerSpec::egress_allow`]
//! carries) into resolver config + resolv.conf text. The *application* of
//! those files (mounting them, running the resolver) is the runtime path
//! wired in the host's spawn code; the content generation here is what the
//! unit tests pin.

use std::collections::BTreeSet;

/// Default loopback address the filtering resolver listens on, as seen from
/// inside the container. The host wires the resolver (a dnsmasq-style filter
/// sidecar, or an in-container forwarder) to this address and pins
/// `/etc/resolv.conf` at it. Loopback keeps the resolver unreachable from
/// outside the container's network namespace.
pub const DEFAULT_FILTER_RESOLVER_IP: &str = "127.0.0.1";

/// Default UDP/TCP port the filtering resolver listens on. 53 is the standard
/// DNS port; keeping it standard means the container's stub resolver needs no
/// non-default `resolv.conf` options to find it.
pub const DEFAULT_FILTER_RESOLVER_PORT: u16 = 53;

/// Extract the bare DNS name from one resolved `host:port` allow-list entry,
/// returning `None` when the host part is an IP literal (v4 or bracketed v6)
/// rather than a name.
///
/// The egress allow-list is a list of `host:port` pairs (see
/// [`crate::ContainerSpec::egress_allow`]). Only the *name* entries need a DNS
/// answer — an IP-literal entry (`10.0.0.5:5432`, `[::1]:11434`) is reached
/// without any DNS lookup at all, so it must NOT widen the resolver's
/// allow-set (and a bare IP is not a name the resolver could meaningfully
/// "allow" anyway).
///
/// Handles the same authority shapes [`crate::spec`] entries take:
/// `host:port`, bracketed IPv6 `[::1]:port`, and a bare host with no port.
#[must_use]
pub fn allow_name(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }

    // Bracketed IPv6 authority: `[::1]` or `[::1]:53`. The inside is always an
    // IP literal, never a name — drop it.
    if entry.starts_with('[') {
        return None;
    }

    // A bare IPv6 literal (multiple colons, no brackets) is not a name.
    if entry.matches(':').count() > 1 {
        return None;
    }

    // `host[:port]` — take the host half (or the whole string when portless).
    let host = entry.split_once(':').map_or(entry, |(h, _)| h);
    if host.is_empty() {
        return None;
    }

    // An IPv4 literal is reached directly, not via DNS — not a name.
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return None;
    }

    Some(host.to_string())
}

/// Resolve the set of allow-listed DNS *names* the filtering resolver must
/// answer, from the spawn's resolved `host:port` allow-list.
///
/// IP-literal entries are dropped (they need no DNS), names are lower-cased
/// (DNS is case-insensitive) and de-duplicated. The result is sorted so the
/// generated resolver config is deterministic (stable across spawns →
/// reproducible fingerprints, clean diffs in tests).
#[must_use]
pub fn allow_list_names(allow: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = allow
        .iter()
        .filter_map(|e| allow_name(e))
        .map(|h| h.to_ascii_lowercase())
        .collect();
    set.into_iter().collect()
}

/// Configuration for the per-session DNS filtering resolver.
///
/// Built from the spawn's resolved allow-list via [`Self::from_allow_list`].
/// Carries the allow-listed names (what the resolver answers), the address it
/// listens on (what `/etc/resolv.conf` is pinned at), and the upstream the
/// resolver forwards *allowed* names to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterResolverConfig {
    /// DNS names the resolver is permitted to answer. Everything else gets
    /// `NXDOMAIN`. Sorted + de-duplicated + lower-cased.
    pub allow_names: Vec<String>,
    /// Address the resolver listens on (container-visible). `/etc/resolv.conf`
    /// is pinned here.
    pub listen_ip: String,
    /// Port the resolver listens on.
    pub listen_port: u16,
    /// Upstream resolver(s) the filter forwards *allowed* names to. Empty
    /// means "use the host's system resolvers" (the sidecar inherits them);
    /// an explicit list pins the upstream.
    pub upstreams: Vec<String>,
}

impl FilterResolverConfig {
    /// Build a resolver config from a spawn's resolved `host:port` allow-list,
    /// using the default loopback listen address and the given upstreams.
    ///
    /// `upstreams` is the resolver the filter forwards *allowed* names to
    /// (e.g. the host's real upstream `["1.1.1.1"]`); pass an empty slice to
    /// let the sidecar inherit the host's system resolvers.
    #[must_use]
    pub fn from_allow_list(allow: &[String], upstreams: &[String]) -> Self {
        Self {
            allow_names: allow_list_names(allow),
            listen_ip: DEFAULT_FILTER_RESOLVER_IP.to_string(),
            listen_port: DEFAULT_FILTER_RESOLVER_PORT,
            upstreams: upstreams.to_vec(),
        }
    }

    /// `true` when there are no allow-listed names — the resolver would answer
    /// nothing. The host treats this as "DNS is fully closed": with an empty
    /// allow-set there's no name the container can resolve, which pairs with
    /// the empty-allow-list `network_mode: none` hard cut.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.allow_names.is_empty()
    }
}

/// Render the container's pinned `/etc/resolv.conf`.
///
/// Points the container's stub resolver at the single filtering resolver and
/// nothing else, so a deny-default session cannot send lookups to an
/// arbitrary upstream. `options` are conservative: a single `nameserver`
/// line, no `search` domains (search-domain probing is itself a mild
/// exfiltration/side-channel surface), and a short timeout + single attempt
/// so a closed resolver fails fast rather than hanging the agent.
///
/// The leading comment marks the file as host-managed so an operator (or the
/// agent) inspecting it understands it was pinned deliberately.
#[must_use]
pub fn resolv_conf_contents(resolver_ip: &str, _port: u16) -> String {
    // Note: stub resolvers ignore a non-53 port in `nameserver`, so the
    // resolver is expected to listen on 53 inside the container's netns. The
    // port is part of the resolver config (for the sidecar's own bind) but is
    // intentionally not emitted here — emitting `nameserver IP:PORT` would be
    // mis-parsed by glibc. We keep the signature carrying the port so callers
    // pass the resolver config through verbatim.
    format!(
        "# Managed by copperclaw (egress deny-default DNS filter).\n\
         # Lookups are restricted to the group's effective allow-list;\n\
         # all other names return NXDOMAIN. Do not edit.\n\
         nameserver {resolver_ip}\n\
         options timeout:2 attempts:1 no-check-names\n"
    )
}

/// Render a dnsmasq-style config for the filtering resolver.
///
/// dnsmasq's `--server=/<name>/<upstream>` whitelists a name (forward it
/// upstream) and `--address=/#/` NXDOMAINs everything else. The combination
/// gives exactly the deny-default contract: allow-listed names resolve,
/// everything else returns `NXDOMAIN`.
///
/// Lines, in order:
///   - `listen-address` / `port` — bind the resolver where resolv.conf points.
///   - `no-resolv` — ignore the host `/etc/resolv.conf` (we pin upstreams).
///   - `no-hosts` — don't leak the host's `/etc/hosts`.
///   - `bind-interfaces` — don't grab interfaces beyond the listen address.
///   - one `server=/<name>/<upstream>` per allow-listed name × upstream (or a
///     bare `server=/<name>/` when no upstream is pinned → use system default).
///   - `address=/#/` — the catch-all: every other name → `0.0.0.0`/NXDOMAIN.
///
/// An empty allow-set yields a config that resolves NOTHING (only the
/// catch-all), matching [`FilterResolverConfig::is_closed`].
#[must_use]
pub fn dnsmasq_conf(cfg: &FilterResolverConfig) -> String {
    let mut out = String::new();
    out.push_str("# Managed by copperclaw (egress deny-default DNS filter).\n");
    out.push_str(&format!("listen-address={}\n", cfg.listen_ip));
    out.push_str(&format!("port={}\n", cfg.listen_port));
    out.push_str("no-resolv\n");
    out.push_str("no-hosts\n");
    out.push_str("bind-interfaces\n");
    // Don't forward names that aren't FQDNs (no dot) upstream — they can't be
    // exfiltration targets and forwarding them leaks the container's local
    // hostname searches.
    out.push_str("domain-needed\n");
    out.push_str("bogus-priv\n");

    for name in &cfg.allow_names {
        if cfg.upstreams.is_empty() {
            // No pinned upstream: forward the allowed name to the resolver's
            // default (the sidecar's own system resolver).
            out.push_str(&format!("server=/{name}/\n"));
        } else {
            for up in &cfg.upstreams {
                out.push_str(&format!("server=/{name}/{up}\n"));
            }
        }
    }

    // Catch-all: every name not matched by a `server=/<name>/` above is
    // answered as NXDOMAIN. `address=/#/` maps the wildcard to an empty
    // answer; dnsmasq returns NXDOMAIN for the wildcard sink.
    out.push_str("address=/#/\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_name_extracts_host_from_host_port() {
        assert_eq!(
            allow_name("api.anthropic.com:443").as_deref(),
            Some("api.anthropic.com")
        );
        assert_eq!(
            allow_name("openrouter.ai:8443").as_deref(),
            Some("openrouter.ai")
        );
        // Portless bare host.
        assert_eq!(allow_name("db.internal").as_deref(), Some("db.internal"));
    }

    #[test]
    fn allow_name_drops_ip_literals() {
        // IPv4 literal: reached directly, not a DNS name.
        assert_eq!(allow_name("10.0.0.5:5432"), None);
        assert_eq!(allow_name("127.0.0.1:11434"), None);
        // Bracketed IPv6.
        assert_eq!(allow_name("[::1]:11434"), None);
        assert_eq!(allow_name("[2001:db8::1]:443"), None);
        // Bare IPv6 (multiple colons, no brackets).
        assert_eq!(allow_name("2001:db8::1"), None);
    }

    #[test]
    fn allow_name_rejects_empty() {
        assert_eq!(allow_name(""), None);
        assert_eq!(allow_name("   "), None);
        assert_eq!(allow_name(":443"), None);
    }

    #[test]
    fn allow_list_names_dedupes_lowercases_and_sorts() {
        let allow = vec![
            "API.Anthropic.com:443".to_string(),
            "db.local:5432".to_string(),
            // Duplicate (different case + port) → one name.
            "api.anthropic.com:80".to_string(),
            // IP literal → dropped.
            "10.0.0.5:5432".to_string(),
            "[::1]:11434".to_string(),
        ];
        let names = allow_list_names(&allow);
        assert_eq!(
            names,
            vec!["api.anthropic.com".to_string(), "db.local".to_string()]
        );
    }

    #[test]
    fn allow_list_names_empty_input_empty_output() {
        assert!(allow_list_names(&[]).is_empty());
        // All-IP input → no names to resolve.
        assert!(allow_list_names(&["10.0.0.5:5432".to_string()]).is_empty());
    }

    #[test]
    fn resolver_config_from_allow_list_defaults() {
        let cfg =
            FilterResolverConfig::from_allow_list(&["api.anthropic.com:443".to_string()], &[]);
        assert_eq!(cfg.allow_names, vec!["api.anthropic.com".to_string()]);
        assert_eq!(cfg.listen_ip, DEFAULT_FILTER_RESOLVER_IP);
        assert_eq!(cfg.listen_port, DEFAULT_FILTER_RESOLVER_PORT);
        assert!(cfg.upstreams.is_empty());
        assert!(!cfg.is_closed());
    }

    #[test]
    fn resolver_config_empty_allow_is_closed() {
        let cfg = FilterResolverConfig::from_allow_list(&[], &[]);
        assert!(cfg.is_closed(), "no names → resolver answers nothing");
        // An all-IP allow-list is also closed (no names to resolve).
        let cfg2 = FilterResolverConfig::from_allow_list(&["10.0.0.5:5432".to_string()], &[]);
        assert!(cfg2.is_closed());
    }

    #[test]
    fn resolv_conf_pins_single_nameserver_and_no_search() {
        let out = resolv_conf_contents(DEFAULT_FILTER_RESOLVER_IP, DEFAULT_FILTER_RESOLVER_PORT);
        assert!(out.contains("nameserver 127.0.0.1\n"));
        // Exactly one nameserver line — no arbitrary upstream.
        assert_eq!(out.matches("nameserver ").count(), 1);
        // No search domains (probing surface).
        assert!(!out.contains("search "));
        // Fail-fast options.
        assert!(out.contains("timeout:2"));
        assert!(out.contains("attempts:1"));
        // Marked host-managed.
        assert!(out.contains("Managed by copperclaw"));
        // glibc requires a final newline.
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn dnsmasq_conf_allows_names_and_nxdomains_the_rest() {
        let cfg = FilterResolverConfig::from_allow_list(
            &[
                "api.anthropic.com:443".to_string(),
                "db.local:5432".to_string(),
            ],
            &["1.1.1.1".to_string()],
        );
        let conf = dnsmasq_conf(&cfg);
        // Each allowed name forwarded to the pinned upstream.
        assert!(conf.contains("server=/api.anthropic.com/1.1.1.1\n"));
        assert!(conf.contains("server=/db.local/1.1.1.1\n"));
        // Listen address + port pinned.
        assert!(conf.contains("listen-address=127.0.0.1\n"));
        assert!(conf.contains("port=53\n"));
        // Catch-all NXDOMAIN for everything else.
        assert!(conf.contains("address=/#/\n"));
        // Doesn't read the host resolv.conf / hosts.
        assert!(conf.contains("no-resolv\n"));
        assert!(conf.contains("no-hosts\n"));
    }

    #[test]
    fn dnsmasq_conf_multiple_upstreams_fan_out_per_name() {
        let cfg = FilterResolverConfig::from_allow_list(
            &["api.anthropic.com:443".to_string()],
            &["1.1.1.1".to_string(), "8.8.8.8".to_string()],
        );
        let conf = dnsmasq_conf(&cfg);
        assert!(conf.contains("server=/api.anthropic.com/1.1.1.1\n"));
        assert!(conf.contains("server=/api.anthropic.com/8.8.8.8\n"));
    }

    #[test]
    fn dnsmasq_conf_no_upstream_uses_default_forward() {
        let cfg =
            FilterResolverConfig::from_allow_list(&["api.anthropic.com:443".to_string()], &[]);
        let conf = dnsmasq_conf(&cfg);
        // Bare server line = forward to the sidecar's default resolver.
        assert!(conf.contains("server=/api.anthropic.com/\n"));
    }

    #[test]
    fn dnsmasq_conf_empty_allow_only_catch_all() {
        let cfg = FilterResolverConfig::from_allow_list(&[], &["1.1.1.1".to_string()]);
        let conf = dnsmasq_conf(&cfg);
        // No per-name server lines at all.
        assert!(!conf.contains("server=/"));
        // Only the catch-all NXDOMAIN sink.
        assert!(conf.contains("address=/#/\n"));
    }

    #[test]
    fn dnsmasq_conf_is_deterministic() {
        // Same allow-list (regardless of input order) → identical config.
        let a = dnsmasq_conf(&FilterResolverConfig::from_allow_list(
            &["b.com:443".to_string(), "a.com:443".to_string()],
            &["1.1.1.1".to_string()],
        ));
        let b = dnsmasq_conf(&FilterResolverConfig::from_allow_list(
            &["a.com:443".to_string(), "b.com:443".to_string()],
            &["1.1.1.1".to_string()],
        ));
        assert_eq!(a, b);
    }
}
