//! SSRF guard for the outbound HTTP tools (`web_fetch`, `web_search`).
//!
//! The container the agent runs in is isolated, but it is NOT
//! network-isolated from the host: it can reach `localhost`, the
//! Docker bridge gateway, the host's RFC1918 LAN, and — most
//! dangerously — the cloud metadata endpoint at `169.254.169.254`,
//! whose unauthenticated HTTP API hands out IAM credentials on AWS /
//! GCP / Azure. An agent that can be steered into fetching an
//! attacker-chosen URL (prompt injection from a fetched page, a
//! crafted search result, a user message) is a classic SSRF pivot.
//!
//! This module resolves a target URL's host to its IP address(es) and
//! REJECTS any that land in a non-public range: loopback, link-local
//! (incl. the metadata address), RFC1918 private, IPv6 unique-local,
//! and the unspecified address. Public addresses pass.
//!
//! Two enforcement points, because one is not enough:
//!   1. [`guard_url`] — async, called before the initial request. Does
//!      a real DNS resolution of the host and checks every returned IP.
//!   2. [`redirect_policy`] — a [`reqwest::redirect::Policy`] that
//!      re-runs the same classification against EVERY redirect hop's
//!      target. `reqwest`'s default policy blindly follows up to 10
//!      redirects, so a public URL that 302s to `http://169.254.169.254/`
//!      would otherwise sail straight past the initial check. The
//!      redirect callback is synchronous, so it resolves via the
//!      blocking [`std::net::ToSocketAddrs`] path; that runs on
//!      `reqwest`'s own connection task, not the async reactor.
//!
//! DNS-rebinding note: the initial-request check resolves the host and
//! `reqwest` resolves it again when it connects, so a TOCTOU rebind is
//! theoretically possible. Closing that fully requires pinning the
//! resolved IP into the connection, which `reqwest` does not expose on
//! a per-request basis. The guard still raises the bar from "trivial"
//! to "needs a rebinding attack", and the redirect re-check covers the
//! far more common 302-to-internal vector.

use crate::error::ToolError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Typed reason a host was rejected. Carries the host and the
/// category so callers can surface a precise message without leaking
/// anything an attacker doesn't already know (it's their own URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsrfError {
    /// The URL did not parse, or used a scheme we don't fetch.
    InvalidUrl(String),
    /// The URL had no host component (e.g. `file:///etc/passwd`).
    MissingHost,
    /// DNS resolution of the host failed.
    ResolutionFailed { host: String },
    /// The host resolved (or was) an address in a blocked range.
    BlockedAddress {
        /// The host as written in the URL.
        host: String,
        /// The offending resolved IP.
        ip: IpAddr,
        /// Human-readable category, e.g. `"loopback"`.
        category: &'static str,
    },
}

impl std::fmt::Display for SsrfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(u) => write!(f, "invalid or unsupported URL: {u}"),
            Self::MissingHost => write!(f, "URL has no host"),
            Self::ResolutionFailed { host } => write!(f, "could not resolve host `{host}`"),
            Self::BlockedAddress { host, ip, category } => write!(
                f,
                "refusing to fetch `{host}`: resolves to {category} address {ip}, \
                 which is blocked to prevent server-side request forgery (SSRF)"
            ),
        }
    }
}

impl std::error::Error for SsrfError {}

impl From<SsrfError> for ToolError {
    fn from(e: SsrfError) -> Self {
        // All SSRF rejections are caller-input problems: the agent
        // asked for a URL it isn't allowed to reach. Validation is the
        // right bucket — it surfaces the reason back to the model.
        ToolError::Validation(e.to_string())
    }
}

/// Classify an IP address. Returns `Some(category)` when the address
/// is in a range we refuse to connect to, or `None` when it is a
/// public address that may be fetched.
///
/// Categories, with the ranges they cover:
///   - `"unspecified"` — `0.0.0.0`, `::`
///   - `"loopback"` — `127.0.0.0/8`, `::1`
///   - `"link-local"` — `169.254.0.0/16` (incl. the `169.254.169.254`
///     cloud-metadata address), `fe80::/10`
///   - `"private"` — `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`
///   - `"unique-local"` — `fc00::/7` (IPv6 ULA)
///
/// IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are unwrapped to their
/// embedded IPv4 and re-classified, so `::ffff:127.0.0.1` is caught as
/// loopback rather than slipping through as a "public" v6 address.
pub fn classify_ip(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => {
            // Unwrap IPv4-mapped (::ffff:0:0/96) addresses and treat
            // them with the IPv4 rules — otherwise `::ffff:10.0.0.1`
            // would dodge the RFC1918 check.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify_v4(v4);
            }
            classify_v6(v6)
        }
    }
}

fn classify_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let o = ip.octets();
    if ip.is_unspecified() {
        // 0.0.0.0 — routes to "this host" on many stacks.
        return Some("unspecified");
    }
    if ip.is_loopback() {
        // 127.0.0.0/8
        return Some("loopback");
    }
    if ip.is_link_local() {
        // 169.254.0.0/16 — includes the 169.254.169.254 metadata addr.
        return Some("link-local");
    }
    if ip.is_private() {
        // 10/8, 172.16/12, 192.168/16
        return Some("private");
    }
    // 100.64.0.0/10 — carrier-grade NAT (RFC 6598). Not strictly
    // private, but never a legitimate fetch target from a container
    // and used by some metadata fabrics; block it too.
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return Some("shared-cgnat");
    }
    None
}

fn classify_v6(ip: Ipv6Addr) -> Option<&'static str> {
    if ip.is_unspecified() {
        // ::
        return Some("unspecified");
    }
    if ip.is_loopback() {
        // ::1
        return Some("loopback");
    }
    // fe80::/10 — link-local unicast. `std`'s `Ipv6Addr` has no stable
    // `is_unicast_link_local`, so test the top 10 bits directly.
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return Some("link-local");
    }
    // fc00::/7 — unique-local addresses (the IPv6 analogue of RFC1918).
    if (ip.segments()[0] & 0xfe00) == 0xfc00 {
        return Some("unique-local");
    }
    None
}

/// Parse `url`, confirm the scheme is HTTP(S), and return the host.
/// Rejects non-HTTP schemes (`file:`, `ftp:`, `gopher:`, …) up front —
/// those are SSRF vectors in their own right and the tools only ever
/// intend to speak HTTP(S).
fn parse_http_host(url: &str) -> Result<(reqwest::Url, String), SsrfError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| SsrfError::InvalidUrl(url.to_string()))?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err(SsrfError::InvalidUrl(url.to_string())),
    }
    let host = parsed.host_str().ok_or(SsrfError::MissingHost)?.to_string();
    Ok((parsed, host))
}

/// Classify a set of already-resolved addresses for `host` and reject
/// if ANY of them lands in a blocked range. We reject on the first
/// blocked hit rather than requiring all addresses to be bad: an
/// attacker who controls DNS could return one public and one internal
/// A-record and race the connect, so a single internal address is
/// disqualifying. An empty address set is itself a resolution failure.
fn classify_addrs(
    host: &str,
    addrs: impl IntoIterator<Item = std::net::IpAddr>,
) -> Result<(), SsrfError> {
    let mut saw_any = false;
    for ip in addrs {
        saw_any = true;
        if let Some(category) = classify_ip(ip) {
            // Test harnesses (wiremock) stand the mock HTTP server up on
            // a loopback address. The `web_fetch` / `web_search` handler
            // tests opt into reaching it via [`LoopbackTestGuard`];
            // production code never flips that switch, so loopback stays
            // blocked.
            #[cfg(test)]
            if loopback_allowed_for_test() && ip.is_loopback() {
                continue;
            }
            return Err(SsrfError::BlockedAddress {
                host: host.to_string(),
                ip,
                category,
            });
        }
    }
    if saw_any {
        Ok(())
    } else {
        Err(SsrfError::ResolutionFailed {
            host: host.to_string(),
        })
    }
}

/// Synchronous resolve-and-classify. Used by the redirect-policy
/// callback, which `reqwest` invokes on its own (blocking) connection
/// task — there is no async context to await in. `host` may itself be
/// an IP literal, which `to_socket_addrs` handles without a DNS
/// round-trip.
fn check_resolved_sync(host: &str, port: u16) -> Result<(), SsrfError> {
    use std::net::ToSocketAddrs;
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| SsrfError::ResolutionFailed {
            host: host.to_string(),
        })?
        .map(|sa| sa.ip());
    classify_addrs(host, addrs)
}

/// Test-only switch: when set, [`classify_addrs`] permits loopback
/// addresses so the `web_fetch` / `web_search` handler tests can reach
/// a wiremock server bound to `127.0.0.1`. Never compiled into release
/// builds. Off by default — the `net_guard` classifier tests rely on
/// loopback staying blocked.
///
/// Access is serialised through [`loopback_test_lock`]: the bypass is
/// only ever `true` while a [`LoopbackTestGuard`] holds that lock, and
/// the `net_guard` tests that assert loopback IS blocked take the same
/// lock first, so the two never observe each other's flag in parallel.
#[cfg(test)]
static ALLOW_LOOPBACK_FOR_TEST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
fn loopback_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
fn loopback_allowed_for_test() -> bool {
    ALLOW_LOOPBACK_FOR_TEST.load(std::sync::atomic::Ordering::SeqCst)
}

/// RAII guard used by handler tests in sibling modules: takes the
/// loopback test lock, enables the bypass for its lifetime, and clears
/// the flag (releasing the lock) on drop.
#[cfg(test)]
pub(crate) struct LoopbackTestGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl LoopbackTestGuard {
    pub(crate) fn enable() -> Self {
        let lock = loopback_test_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ALLOW_LOOPBACK_FOR_TEST.store(true, std::sync::atomic::Ordering::SeqCst);
        Self(lock)
    }
}

#[cfg(test)]
impl Drop for LoopbackTestGuard {
    fn drop(&mut self) {
        ALLOW_LOOPBACK_FOR_TEST.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// RAII guard for the `net_guard` tests that assert loopback stays
/// blocked: holds the same lock so a parallel [`LoopbackTestGuard`]
/// can't flip the bypass on mid-assertion.
#[cfg(test)]
struct LoopbackBlockedTestGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl LoopbackBlockedTestGuard {
    fn acquire() -> Self {
        Self(
            loopback_test_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Default port for a URL whose scheme implies one but that omits an
/// explicit port. `to_socket_addrs` needs a port, but the value is
/// irrelevant to classification — we never actually connect here.
fn effective_port(url: &reqwest::Url) -> u16 {
    url.port_or_known_default().unwrap_or(443)
}

/// Async pre-flight check, run before the initial request. Parses the
/// URL, resolves the host via tokio's async resolver, and rejects any
/// blocked address.
///
/// [`tokio::net::lookup_host`] short-circuits IP literals (no DNS
/// round-trip) and runs real `getaddrinfo` lookups on tokio's own
/// resolver rather than a hand-rolled `spawn_blocking`, so it doesn't
/// add pressure to the shared blocking pool.
pub async fn guard_url(url: &str) -> Result<(), SsrfError> {
    let (parsed, host) = parse_http_host(url)?;
    let port = effective_port(&parsed);
    let resolved = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| SsrfError::ResolutionFailed { host: host.clone() })?;
    classify_addrs(&host, resolved.map(|sa| sa.ip()))
}

/// Build the redirect policy for the outbound HTTP clients. Every
/// redirect hop's target is re-classified against [`check_resolved_sync`];
/// a hop into a blocked range stops the redirect chain instead of
/// following it. We also cap the chain at 10 hops (matching reqwest's
/// own default) so a redirect loop can't spin forever.
///
/// The callback is synchronous (reqwest contract), so it resolves via
/// the blocking `to_socket_addrs` path. That call runs on reqwest's
/// connection task, which is acceptable for the bounded number of
/// hops involved.
pub fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error(SsrfError::InvalidUrl(
                "too many redirects (max 10)".to_string(),
            ));
        }
        // `attempt.url()` borrows `attempt`, but `error()`/`follow()`
        // consume it — so decide on owned data first, then act.
        let decision = classify_redirect(attempt.url());
        match decision {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(e),
        }
    })
}

/// Run the SSRF classification for a single redirect-hop URL. Pulled
/// out of the closure in [`redirect_policy`] so all borrows of the
/// `Attempt`'s URL are resolved into an owned `Result` before the
/// `Attempt` is consumed by `follow()` / `error()`.
fn classify_redirect(url: &reqwest::Url) -> Result<(), SsrfError> {
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err(SsrfError::InvalidUrl(url.to_string())),
    }
    let host = url.host_str().ok_or(SsrfError::MissingHost)?;
    let port = effective_port(url);
    check_resolved_sync(host, port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    // ── IPv4 classification ──────────────────────────────────────────

    #[test]
    fn v4_loopback_blocked() {
        assert_eq!(classify_ip(ip("127.0.0.1")), Some("loopback"));
        assert_eq!(classify_ip(ip("127.255.255.255")), Some("loopback"));
        assert_eq!(classify_ip(ip("127.1.2.3")), Some("loopback"));
    }

    #[test]
    fn v4_unspecified_blocked() {
        assert_eq!(classify_ip(ip("0.0.0.0")), Some("unspecified"));
    }

    #[test]
    fn v4_link_local_blocked_including_metadata() {
        assert_eq!(classify_ip(ip("169.254.0.1")), Some("link-local"));
        // The cloud-metadata endpoint — the headline SSRF target.
        assert_eq!(classify_ip(ip("169.254.169.254")), Some("link-local"));
        assert_eq!(classify_ip(ip("169.254.255.255")), Some("link-local"));
    }

    #[test]
    fn v4_rfc1918_blocked() {
        // 10.0.0.0/8
        assert_eq!(classify_ip(ip("10.0.0.1")), Some("private"));
        assert_eq!(classify_ip(ip("10.255.255.255")), Some("private"));
        // 172.16.0.0/12
        assert_eq!(classify_ip(ip("172.16.0.1")), Some("private"));
        assert_eq!(classify_ip(ip("172.31.255.255")), Some("private"));
        // 192.168.0.0/16
        assert_eq!(classify_ip(ip("192.168.0.1")), Some("private"));
        assert_eq!(classify_ip(ip("192.168.255.255")), Some("private"));
    }

    #[test]
    fn v4_just_outside_rfc1918_is_public() {
        // 172.15.x and 172.32.x are NOT in 172.16/12.
        assert_eq!(classify_ip(ip("172.15.0.1")), None);
        assert_eq!(classify_ip(ip("172.32.0.1")), None);
        // 11.x and 9.x are not in 10/8.
        assert_eq!(classify_ip(ip("11.0.0.1")), None);
        assert_eq!(classify_ip(ip("9.255.255.255")), None);
        // 192.169.x is not in 192.168/16.
        assert_eq!(classify_ip(ip("192.169.0.1")), None);
    }

    #[test]
    fn v4_cgnat_blocked() {
        assert_eq!(classify_ip(ip("100.64.0.1")), Some("shared-cgnat"));
        assert_eq!(classify_ip(ip("100.127.255.255")), Some("shared-cgnat"));
        // 100.63.x and 100.128.x are outside 100.64/10.
        assert_eq!(classify_ip(ip("100.63.255.255")), None);
        assert_eq!(classify_ip(ip("100.128.0.1")), None);
    }

    #[test]
    fn v4_public_allowed() {
        assert_eq!(classify_ip(ip("8.8.8.8")), None);
        assert_eq!(classify_ip(ip("1.1.1.1")), None);
        assert_eq!(classify_ip(ip("93.184.216.34")), None); // example.com
        assert_eq!(classify_ip(ip("140.82.112.3")), None); // github.com-ish
    }

    // ── IPv6 classification ──────────────────────────────────────────

    #[test]
    fn v6_loopback_blocked() {
        assert_eq!(classify_ip(ip("::1")), Some("loopback"));
    }

    #[test]
    fn v6_unspecified_blocked() {
        assert_eq!(classify_ip(ip("::")), Some("unspecified"));
    }

    #[test]
    fn v6_link_local_blocked() {
        assert_eq!(classify_ip(ip("fe80::1")), Some("link-local"));
        assert_eq!(
            classify_ip(ip("febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff")),
            Some("link-local")
        );
    }

    #[test]
    fn v6_unique_local_blocked() {
        // fc00::/7 covers fc00:: through fdff::
        assert_eq!(classify_ip(ip("fc00::1")), Some("unique-local"));
        assert_eq!(classify_ip(ip("fd00::1")), Some("unique-local"));
        assert_eq!(
            classify_ip(ip("fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff")),
            Some("unique-local")
        );
    }

    #[test]
    fn v6_public_allowed() {
        // Google public DNS over v6.
        assert_eq!(classify_ip(ip("2001:4860:4860::8888")), None);
        // A documentation-range global unicast.
        assert_eq!(classify_ip(ip("2606:2800:220:1:248:1893:25c8:1946")), None);
    }

    #[test]
    fn v6_mapped_v4_unwrapped_and_classified() {
        // ::ffff:127.0.0.1 must be caught as loopback, not pass as v6.
        assert_eq!(classify_ip(ip("::ffff:127.0.0.1")), Some("loopback"));
        // ::ffff:169.254.169.254 — mapped metadata address.
        assert_eq!(
            classify_ip(ip("::ffff:169.254.169.254")),
            Some("link-local")
        );
        // ::ffff:10.0.0.1 — mapped RFC1918.
        assert_eq!(classify_ip(ip("::ffff:10.0.0.1")), Some("private"));
        // ::ffff:8.8.8.8 — mapped public, still allowed.
        assert_eq!(classify_ip(ip("::ffff:8.8.8.8")), None);
    }

    // ── URL parsing / scheme rejection ───────────────────────────────

    #[test]
    fn parse_rejects_non_http_schemes() {
        for u in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com/",
            "data:text/plain;base64,AAAA",
        ] {
            assert!(
                matches!(parse_http_host(u), Err(SsrfError::InvalidUrl(_))),
                "{u}"
            );
        }
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(
            parse_http_host("not a url"),
            Err(SsrfError::InvalidUrl(_))
        ));
    }

    #[test]
    fn parse_accepts_http_and_https() {
        let (_, h) = parse_http_host("http://example.com/path").unwrap();
        assert_eq!(h, "example.com");
        let (_, h) = parse_http_host("https://example.com:8443/x?y=1").unwrap();
        assert_eq!(h, "example.com");
    }

    // ── check_resolved_sync against IP literals (no DNS needed) ──────

    #[test]
    fn check_resolved_blocks_loopback_literal() {
        let _g = LoopbackBlockedTestGuard::acquire();
        let err = check_resolved_sync("127.0.0.1", 80).unwrap_err();
        match err {
            SsrfError::BlockedAddress { category, ip, .. } => {
                assert_eq!(category, "loopback");
                assert_eq!(ip, ip_str("127.0.0.1"));
            }
            other => panic!("expected BlockedAddress, got {other:?}"),
        }
    }

    #[test]
    fn check_resolved_blocks_metadata_literal() {
        let err = check_resolved_sync("169.254.169.254", 80).unwrap_err();
        assert!(matches!(
            err,
            SsrfError::BlockedAddress {
                category: "link-local",
                ..
            }
        ));
    }

    #[test]
    fn check_resolved_blocks_v6_loopback_literal() {
        let _g = LoopbackBlockedTestGuard::acquire();
        let err = check_resolved_sync("::1", 80).unwrap_err();
        assert!(matches!(
            err,
            SsrfError::BlockedAddress {
                category: "loopback",
                ..
            }
        ));
    }

    #[test]
    fn check_resolved_allows_public_literal() {
        check_resolved_sync("8.8.8.8", 443).expect("public IP literal must pass");
    }

    fn ip_str(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    // ── guard_url end-to-end against literals ────────────────────────

    #[tokio::test]
    async fn guard_url_blocks_loopback() {
        let _g = LoopbackBlockedTestGuard::acquire();
        let err = guard_url("http://127.0.0.1:8080/admin").await.unwrap_err();
        assert!(matches!(err, SsrfError::BlockedAddress { .. }));
    }

    #[tokio::test]
    async fn guard_url_blocks_metadata() {
        let err = guard_url("http://169.254.169.254/latest/meta-data/")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SsrfError::BlockedAddress {
                category: "link-local",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn guard_url_blocks_rfc1918() {
        let err = guard_url("http://10.0.0.5/").await.unwrap_err();
        assert!(matches!(
            err,
            SsrfError::BlockedAddress {
                category: "private",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn guard_url_rejects_non_http_scheme() {
        let err = guard_url("file:///etc/passwd").await.unwrap_err();
        assert!(matches!(err, SsrfError::InvalidUrl(_)));
    }

    #[tokio::test]
    async fn guard_url_allows_public_literal() {
        guard_url("http://8.8.8.8/")
            .await
            .expect("public must pass");
    }

    #[test]
    fn ssrf_error_maps_to_validation() {
        let e = SsrfError::BlockedAddress {
            host: "metadata".into(),
            ip: ip("169.254.169.254"),
            category: "link-local",
        };
        let te: ToolError = e.into();
        assert!(matches!(te, ToolError::Validation(_)));
    }

    #[test]
    fn ssrf_error_display_mentions_ssrf_and_category() {
        let e = SsrfError::BlockedAddress {
            host: "evil.test".into(),
            ip: ip("169.254.169.254"),
            category: "link-local",
        };
        let s = e.to_string();
        assert!(s.contains("link-local"), "{s}");
        assert!(s.contains("SSRF"), "{s}");
        assert!(s.contains("evil.test"), "{s}");
    }
}
