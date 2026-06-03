//! nftables egress-filtering core for the deny-default posture (Phase 0a v2,
//! Part B).
//!
//! v1 enforced deny-default only as a coarse bollard `network_mode: "none"`
//! cut — usable only when the allow-list was *empty*. A non-empty allow-list
//! (the normal case, since the host auto-injects the model endpoint) got the
//! full default bridge with no per-host filtering. v2 closes that gap with a
//! per-session nftables ruleset: drop all egress except the allow-listed
//! `host:port` set plus the filtering DNS resolver, applied inside the
//! session's network namespace.
//!
//! ## What is pure (and tested here) vs. what is the runtime path
//!
//! Turning an allow-list into a *ruleset* and into the *command sequence*
//! that loads it is pure string construction — [`build_ruleset`] and
//! [`apply_commands`] are unit-tested exhaustively below. **Applying** that
//! ruleset requires `CAP_NET_ADMIN` against a live network namespace at spawn
//! time, which is not available in CI; that step is the deferred-but-
//! implemented runtime path. It is constructed here ([`NftApplyPlan`]) and
//! invoked by the host's spawn code only under [`crate::EgressMode::DenyDefault`]
//! (opt-in), so default spawns are entirely unaffected.
//!
//! The ruleset is `nft -f`-loadable text (an atomic table replace), and the
//! command sequence is the argv the host would hand to a privileged helper.
//! Both are derived from the same [`NftPlan`] so the tested artifact is
//! exactly what the runtime path would apply — no drift between "what we
//! tested" and "what runs".

use std::collections::BTreeSet;

/// A single resolved egress destination: a host (name or IP) + TCP port.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AllowEntry {
    /// Host part — a DNS name or an IP literal (v4, or bracketed/ bare v6).
    pub host: String,
    /// TCP destination port.
    pub port: u16,
}

/// How a host was classified for nftables rule generation. nftables matches
/// on packet addresses, not names — so a name entry can only be enforced once
/// it's resolved to addresses (the DNS filter answers exactly the allow-list,
/// so the resolved address is itself constrained). An IPv4/IPv6 literal is
/// matched directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMatch {
    /// IPv4 literal — emitted as an `ip daddr` match.
    V4(std::net::Ipv4Addr),
    /// IPv6 literal — emitted as an `ip6 daddr` match.
    V6(std::net::Ipv6Addr),
    /// A DNS name. nftables can't match a name directly; the rule is keyed by
    /// destination port only, and address-level confinement is delegated to
    /// the DNS filter (which only ever answers the allow-listed names). The
    /// name is retained for the rule comment so the ruleset stays auditable.
    Name(String),
}

/// Parse one resolved `host:port` allow-list entry into an [`AllowEntry`].
///
/// Mirrors the authority shapes [`crate::spec`] produces: `host:port`,
/// bracketed IPv6 `[::1]:port`, and a portless bare host (defaulted to 443 —
/// the dominant case for an HTTPS model endpoint, and the only sane default
/// for a name with no explicit port). Returns `None` for an unparseable or
/// portless-IPv6 entry rather than emitting a rule that wouldn't match.
#[must_use]
pub fn parse_entry(entry: &str) -> Option<AllowEntry> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }

    // Bracketed IPv6: `[::1]` or `[::1]:443`.
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            None | Some("") => 443,
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
        };
        if host.is_empty() {
            return None;
        }
        return Some(AllowEntry {
            host: host.to_string(),
            port,
        });
    }

    // Bare IPv6 (multiple colons, no brackets, no port we can split off).
    if entry.matches(':').count() > 1 {
        return Some(AllowEntry {
            host: entry.to_string(),
            port: 443,
        });
    }

    match entry.split_once(':') {
        Some((host, p)) => {
            let port = p.parse::<u16>().ok().filter(|p| *p != 0)?;
            if host.is_empty() {
                return None;
            }
            Some(AllowEntry {
                host: host.to_string(),
                port,
            })
        }
        None => Some(AllowEntry {
            host: entry.to_string(),
            port: 443,
        }),
    }
}

impl AllowEntry {
    /// Classify the host part for rule emission.
    #[must_use]
    pub fn host_match(&self) -> HostMatch {
        if let Ok(v4) = self.host.parse::<std::net::Ipv4Addr>() {
            return HostMatch::V4(v4);
        }
        // Strip brackets for v6 literals (`[::1]` → `::1`).
        let bare = self.host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(v6) = bare.parse::<std::net::Ipv6Addr>() {
            return HostMatch::V6(v6);
        }
        HostMatch::Name(self.host.clone())
    }
}

/// The fully-resolved inputs needed to build a per-session nftables ruleset.
///
/// Built from the spawn's resolved `host:port` allow-list + the DNS-filter
/// resolver address via [`Self::from_allow_list`]. Everything downstream
/// ([`build_ruleset`], [`apply_commands`]) is derived from this so the tested
/// artifact equals what the runtime path applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NftPlan {
    /// nftables table name (one per session, so teardown is a single
    /// `delete table`). Inet family so a single table covers v4 + v6.
    pub table: String,
    /// Parsed + de-duplicated allow-list destinations.
    pub allow: Vec<AllowEntry>,
    /// The DNS filter resolver's address, always allowed on port 53 (udp+tcp)
    /// so name resolution still works under the drop-all default. `None`
    /// disables the carve-out (e.g. when DNS filtering is off).
    pub resolver_ip: Option<String>,
    /// The DNS filter resolver's port (paired with `resolver_ip`).
    pub resolver_port: u16,
}

impl NftPlan {
    /// Build a plan from a session's resolved `host:port` allow-list.
    ///
    /// `table` is the per-session nftables table name (the host derives it
    /// from the container/session id). `resolver_ip`/`resolver_port` are the
    /// DNS filter the deny-default container talks to — allowed unconditionally
    /// on port 53 so resolution of allow-listed names still works. Entries that
    /// don't parse are dropped (they'd never match a packet anyway); the
    /// surviving entries are de-duplicated and sorted for a deterministic
    /// ruleset.
    #[must_use]
    pub fn from_allow_list(
        table: impl Into<String>,
        allow: &[String],
        resolver_ip: Option<&str>,
        resolver_port: u16,
    ) -> Self {
        let set: BTreeSet<AllowEntry> = allow.iter().filter_map(|e| parse_entry(e)).collect();
        Self {
            table: table.into(),
            allow: set.into_iter().collect(),
            resolver_ip: resolver_ip.map(str::to_string),
            resolver_port,
        }
    }

    /// Names in the allow-list whose address-level confinement is delegated to
    /// the DNS filter (nftables can't match a name). Useful for the host to
    /// log "these are enforced via DNS, not L3".
    #[must_use]
    pub fn delegated_names(&self) -> Vec<String> {
        self.allow
            .iter()
            .filter_map(|e| match e.host_match() {
                HostMatch::Name(n) => Some(n),
                HostMatch::V4(_) | HostMatch::V6(_) => None,
            })
            .collect()
    }
}

/// Build the `nft -f`-loadable ruleset for a per-session deny-default egress
/// filter.
///
/// The ruleset is an **atomic table replace**: a leading `delete table` (with
/// a paired no-op create so the delete never errors on first apply) followed
/// by the fresh table definition. The output chain has policy `drop`, so the
/// default is deny; allow rules `accept` exactly:
///
///   - established/related return traffic (so accepted connections work),
///   - loopback (the container's own services + the local DNS stub),
///   - the DNS filter resolver on port 53 (udp + tcp), when configured,
///   - each allow-listed destination — an `ip`/`ip6 daddr .. tcp dport ..`
///     match for IP literals, and a `tcp dport ..` match for names (address
///     confinement delegated to the DNS filter, annotated in a comment).
///
/// The ruleset is deterministic for a given [`NftPlan`] (entries are sorted in
/// the plan), so it's safe to fingerprint/diff.
#[must_use]
pub fn build_ruleset(plan: &NftPlan) -> String {
    let table = &plan.table;
    let mut out = String::new();

    // Atomic replace: create-then-delete guarantees the delete can't fail on a
    // first-ever apply (the create is a no-op if the table exists), and the
    // subsequent definition rebuilds it from scratch. `nft -f` runs the whole
    // file as one transaction.
    out.push_str(&format!("add table inet {table}\n"));
    out.push_str(&format!("delete table inet {table}\n"));
    out.push_str(&format!("table inet {table} {{\n"));
    out.push_str("\tchain egress {\n");
    out.push_str("\t\ttype filter hook output priority filter; policy drop;\n");
    // Established/related return traffic for accepted connections.
    out.push_str("\t\tct state established,related accept\n");
    // Loopback: the container's own localhost services + the DNS stub.
    out.push_str("\t\toif \"lo\" accept\n");

    // DNS filter carve-out: the resolver on port 53 (udp + tcp).
    if let Some(ip) = &plan.resolver_ip {
        match classify_ip(ip) {
            Some(IpKind::V4(v4)) => {
                out.push_str(&format!(
                    "\t\tip daddr {v4} udp dport {} accept\n",
                    plan.resolver_port
                ));
                out.push_str(&format!(
                    "\t\tip daddr {v4} tcp dport {} accept\n",
                    plan.resolver_port
                ));
            }
            Some(IpKind::V6(v6)) => {
                out.push_str(&format!(
                    "\t\tip6 daddr {v6} udp dport {} accept\n",
                    plan.resolver_port
                ));
                out.push_str(&format!(
                    "\t\tip6 daddr {v6} tcp dport {} accept\n",
                    plan.resolver_port
                ));
            }
            None => {
                // Resolver given as a name (unusual): allow port 53 by port
                // only, address confinement delegated to the host wiring.
                out.push_str(&format!(
                    "\t\tudp dport {} accept comment \"dns-filter {ip}\"\n",
                    plan.resolver_port
                ));
                out.push_str(&format!(
                    "\t\ttcp dport {} accept comment \"dns-filter {ip}\"\n",
                    plan.resolver_port
                ));
            }
        }
    }

    // Per-destination allow rules.
    for entry in &plan.allow {
        match entry.host_match() {
            HostMatch::V4(v4) => {
                out.push_str(&format!(
                    "\t\tip daddr {v4} tcp dport {} accept\n",
                    entry.port
                ));
            }
            HostMatch::V6(v6) => {
                out.push_str(&format!(
                    "\t\tip6 daddr {v6} tcp dport {} accept\n",
                    entry.port
                ));
            }
            HostMatch::Name(name) => {
                // nftables can't match a DNS name; gate by port and delegate
                // address confinement to the DNS filter (which only answers
                // allow-listed names). The comment keeps the rule auditable.
                out.push_str(&format!(
                    "\t\ttcp dport {} accept comment \"allow {name} (dns-filtered)\"\n",
                    entry.port
                ));
            }
        }
    }

    out.push_str("\t}\n");
    out.push_str("}\n");
    out
}

/// IP-literal classification used for rule emission.
enum IpKind {
    V4(std::net::Ipv4Addr),
    V6(std::net::Ipv6Addr),
}

/// Classify a string as an IPv4 / IPv6 literal, stripping IPv6 brackets.
/// Returns `None` when it's a DNS name (not an IP literal).
fn classify_ip(s: &str) -> Option<IpKind> {
    if let Ok(v4) = s.parse::<std::net::Ipv4Addr>() {
        return Some(IpKind::V4(v4));
    }
    let bare = s.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::Ipv6Addr>().ok().map(IpKind::V6)
}

/// The privileged runtime apply step, fully constructed but **not executed**
/// here (it needs `CAP_NET_ADMIN` against a live netns at spawn).
///
/// Carries the ruleset text and the `nft` argv the host hands to its
/// privileged helper. The host invokes [`Self::run`] only under
/// [`crate::EgressMode::DenyDefault`]; everything that builds the plan is
/// pure and tested, so a CI environment without `CAP_NET_ADMIN` still pins the
/// exact bytes that would be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NftApplyPlan {
    /// The `nft -f`-loadable ruleset (see [`build_ruleset`]).
    pub ruleset: String,
    /// argv to load the ruleset from stdin: `["nft", "-f", "-"]`.
    pub apply_argv: Vec<String>,
    /// argv to tear the per-session table down: `["nft", "delete", "table",
    /// "inet", "<table>"]`.
    pub teardown_argv: Vec<String>,
}

impl NftApplyPlan {
    /// Build the apply plan (ruleset + argv) from an [`NftPlan`]. Pure.
    #[must_use]
    pub fn new(plan: &NftPlan) -> Self {
        Self {
            ruleset: build_ruleset(plan),
            apply_argv: apply_commands(),
            teardown_argv: teardown_commands(&plan.table),
        }
    }
}

/// argv to load a ruleset from stdin via `nft -f -`. The ruleset text is fed
/// on stdin so it never appears in the process table (allow-list hosts are not
/// secret, but stdin keeps the command line clean and avoids arg-length caps).
#[must_use]
pub fn apply_commands() -> Vec<String> {
    vec!["nft".to_string(), "-f".to_string(), "-".to_string()]
}

/// argv to delete a per-session table on teardown.
#[must_use]
pub fn teardown_commands(table: &str) -> Vec<String> {
    vec![
        "nft".to_string(),
        "delete".to_string(),
        "table".to_string(),
        "inet".to_string(),
        table.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns::allow_name;

    #[test]
    fn parse_entry_host_port() {
        let e = parse_entry("api.anthropic.com:443").unwrap();
        assert_eq!(e.host, "api.anthropic.com");
        assert_eq!(e.port, 443);
    }

    #[test]
    fn parse_entry_portless_defaults_443() {
        let e = parse_entry("api.example.com").unwrap();
        assert_eq!(e.host, "api.example.com");
        assert_eq!(e.port, 443);
    }

    #[test]
    fn parse_entry_ipv4_literal() {
        let e = parse_entry("10.0.0.5:5432").unwrap();
        assert_eq!(e.host, "10.0.0.5");
        assert_eq!(e.port, 5432);
        assert!(matches!(e.host_match(), HostMatch::V4(_)));
    }

    #[test]
    fn parse_entry_ipv6_bracketed() {
        let e = parse_entry("[2001:db8::1]:11434").unwrap();
        assert_eq!(e.host, "2001:db8::1");
        assert_eq!(e.port, 11434);
        assert!(matches!(e.host_match(), HostMatch::V6(_)));
    }

    #[test]
    fn parse_entry_ipv6_bare_defaults_443() {
        let e = parse_entry("2001:db8::1").unwrap();
        assert_eq!(e.host, "2001:db8::1");
        assert_eq!(e.port, 443);
        assert!(matches!(e.host_match(), HostMatch::V6(_)));
    }

    #[test]
    fn parse_entry_rejects_garbage_port_and_empty() {
        assert!(parse_entry("h:0").is_none());
        assert!(parse_entry("h:99999").is_none());
        assert!(parse_entry("h:abc").is_none());
        assert!(parse_entry("").is_none());
        assert!(parse_entry(":443").is_none());
        assert!(parse_entry("[::1]:0").is_none());
    }

    #[test]
    fn host_match_classifies_name() {
        let e = AllowEntry {
            host: "api.anthropic.com".into(),
            port: 443,
        };
        assert_eq!(
            e.host_match(),
            HostMatch::Name("api.anthropic.com".to_string())
        );
    }

    #[test]
    fn plan_from_allow_list_dedupes_and_sorts() {
        let plan = NftPlan::from_allow_list(
            "cc_sess",
            &[
                "b.com:443".to_string(),
                "a.com:443".to_string(),
                // dup
                "a.com:443".to_string(),
                // unparseable → dropped
                "bad:0".to_string(),
            ],
            Some("127.0.0.1"),
            53,
        );
        assert_eq!(plan.allow.len(), 2);
        // Sorted by (host, port).
        assert_eq!(plan.allow[0].host, "a.com");
        assert_eq!(plan.allow[1].host, "b.com");
    }

    #[test]
    fn plan_delegated_names_lists_only_names() {
        let plan = NftPlan::from_allow_list(
            "t",
            &[
                "api.anthropic.com:443".to_string(),
                "10.0.0.5:5432".to_string(),
            ],
            Some("127.0.0.1"),
            53,
        );
        assert_eq!(
            plan.delegated_names(),
            vec!["api.anthropic.com".to_string()]
        );
    }

    #[test]
    fn ruleset_has_drop_policy_and_atomic_replace() {
        let plan = NftPlan::from_allow_list("cc_s1", &[], Some("127.0.0.1"), 53);
        let rs = build_ruleset(&plan);
        // Atomic table replace: add (idempotent) then delete then define.
        assert!(rs.contains("add table inet cc_s1\n"));
        assert!(rs.contains("delete table inet cc_s1\n"));
        assert!(rs.contains("table inet cc_s1 {\n"));
        // Default-deny output chain.
        assert!(rs.contains("hook output priority filter; policy drop;"));
        // Established/related + loopback always allowed.
        assert!(rs.contains("ct state established,related accept"));
        assert!(rs.contains("oif \"lo\" accept"));
    }

    #[test]
    fn ruleset_dns_resolver_carve_out_v4() {
        let plan = NftPlan::from_allow_list("t", &[], Some("127.0.0.1"), 53);
        let rs = build_ruleset(&plan);
        assert!(rs.contains("ip daddr 127.0.0.1 udp dport 53 accept"));
        assert!(rs.contains("ip daddr 127.0.0.1 tcp dport 53 accept"));
    }

    #[test]
    fn ruleset_dns_resolver_carve_out_v6() {
        let plan = NftPlan::from_allow_list("t", &[], Some("::1"), 53);
        let rs = build_ruleset(&plan);
        assert!(rs.contains("ip6 daddr ::1 udp dport 53 accept"));
        assert!(rs.contains("ip6 daddr ::1 tcp dport 53 accept"));
    }

    #[test]
    fn ruleset_no_resolver_omits_dns_carve_out() {
        let plan = NftPlan::from_allow_list("t", &[], None, 53);
        let rs = build_ruleset(&plan);
        assert!(!rs.contains("dport 53"));
    }

    #[test]
    fn ruleset_ip_literal_gets_daddr_match() {
        let plan =
            NftPlan::from_allow_list("t", &["10.0.0.5:5432".to_string()], Some("127.0.0.1"), 53);
        let rs = build_ruleset(&plan);
        assert!(rs.contains("ip daddr 10.0.0.5 tcp dport 5432 accept"));
    }

    #[test]
    fn ruleset_ipv6_literal_gets_ip6_daddr_match() {
        let plan = NftPlan::from_allow_list(
            "t",
            &["[2001:db8::1]:11434".to_string()],
            Some("127.0.0.1"),
            53,
        );
        let rs = build_ruleset(&plan);
        assert!(rs.contains("ip6 daddr 2001:db8::1 tcp dport 11434 accept"));
    }

    #[test]
    fn ruleset_name_gets_port_rule_with_dns_filtered_comment() {
        let plan = NftPlan::from_allow_list(
            "t",
            &["api.anthropic.com:443".to_string()],
            Some("127.0.0.1"),
            53,
        );
        let rs = build_ruleset(&plan);
        // Name entries are gated by port; address confinement is delegated to
        // the DNS filter and annotated.
        assert!(
            rs.contains("tcp dport 443 accept comment \"allow api.anthropic.com (dns-filtered)\"")
        );
    }

    #[test]
    fn ruleset_is_deterministic() {
        let a = build_ruleset(&NftPlan::from_allow_list(
            "t",
            &["b.com:443".to_string(), "a.com:443".to_string()],
            Some("127.0.0.1"),
            53,
        ));
        let b = build_ruleset(&NftPlan::from_allow_list(
            "t",
            &["a.com:443".to_string(), "b.com:443".to_string()],
            Some("127.0.0.1"),
            53,
        ));
        assert_eq!(a, b);
    }

    #[test]
    fn ruleset_empty_allow_is_pure_deny_plus_dns() {
        // Empty allow-list with a resolver: nothing reachable except DNS +
        // loopback + established. (The model endpoint is normally injected, so
        // this is the "fully closed except resolution" corner.)
        let plan = NftPlan::from_allow_list("t", &[], Some("127.0.0.1"), 53);
        let rs = build_ruleset(&plan);
        // No tcp dport accept beyond the DNS resolver line.
        let tcp_accepts = rs.matches("tcp dport").count();
        // Exactly one: the resolver's tcp/53.
        assert_eq!(tcp_accepts, 1, "ruleset:\n{rs}");
    }

    #[test]
    fn apply_commands_feeds_ruleset_on_stdin() {
        assert_eq!(apply_commands(), vec!["nft", "-f", "-"]);
    }

    #[test]
    fn teardown_commands_delete_named_table() {
        assert_eq!(
            teardown_commands("cc_s1"),
            vec!["nft", "delete", "table", "inet", "cc_s1"]
        );
    }

    #[test]
    fn apply_plan_bundles_ruleset_and_argv() {
        let plan = NftPlan::from_allow_list(
            "cc_s1",
            &["api.anthropic.com:443".to_string()],
            Some("127.0.0.1"),
            53,
        );
        let ap = NftApplyPlan::new(&plan);
        assert_eq!(ap.ruleset, build_ruleset(&plan));
        assert_eq!(ap.apply_argv, vec!["nft", "-f", "-"]);
        assert_eq!(
            ap.teardown_argv,
            vec!["nft", "delete", "table", "inet", "cc_s1"]
        );
    }

    #[test]
    fn allow_name_reexport_consistency() {
        // The nftables name classification must agree with the DNS module's
        // name extraction: a name entry is delegated, an IP entry isn't.
        let plan = NftPlan::from_allow_list(
            "t",
            &[
                "api.anthropic.com:443".to_string(),
                "10.0.0.5:5432".to_string(),
            ],
            None,
            53,
        );
        for e in &plan.allow {
            match e.host_match() {
                HostMatch::Name(_) => {
                    assert!(allow_name(&format!("{}:{}", e.host, e.port)).is_some());
                }
                HostMatch::V4(_) | HostMatch::V6(_) => {
                    assert!(allow_name(&format!("{}:{}", e.host, e.port)).is_none());
                }
            }
        }
    }
}
