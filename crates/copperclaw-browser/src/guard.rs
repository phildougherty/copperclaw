//! Navigation SSRF guard abstraction.
//!
//! The headless browser drives a navigation to an agent-chosen URL, then
//! follows whatever redirects the page issues — exactly the SSRF surface the
//! `web_fetch` tool already guards. Phase 5a **reuses that guard**: the
//! production navigation guard ([`crate`] consumers wire it in the
//! `copperclaw-mcp` browser tool) delegates to
//! `copperclaw_mcp::tools::net_guard::{guard_url, classify_redirect}` so the
//! navigation target AND every redirect hop are classified against the same
//! loopback / link-local (incl. cloud-metadata) / RFC1918 / unique-local
//! rejection rules.
//!
//! To keep that reuse testable inside this crate (which sits *below*
//! `copperclaw-mcp` in the dependency graph and so cannot import it without a
//! cycle), the guard is expressed as a trait. The MCP tool supplies the
//! `net_guard`-backed impl; tests here supply deterministic stand-ins. The
//! contract is identical to `net_guard`'s two enforcement points:
//!
//!   1. [`NavigationGuard::guard_target`] — async pre-flight before the
//!      browser navigates.
//!   2. [`NavigationGuard::guard_redirect`] — synchronous re-check of every
//!      redirect hop the page follows.

use async_trait::async_trait;

/// Outcome of a single guard check: either the URL is allowed, or it is
/// blocked with a reason (which is safe to surface — it is the caller's own
/// URL, classified).
pub type GuardResult = Result<(), String>;

/// SSRF guard for browser navigation. Mirrors the two `net_guard`
/// enforcement points. Implementations MUST reject loopback, link-local
/// (including `169.254.169.254`), RFC1918/CGNAT, IPv6 ULA, and the
/// unspecified address — i.e. every non-public range.
#[async_trait]
pub trait NavigationGuard: Send + Sync {
    /// Async pre-flight: resolve the target host and reject any blocked
    /// address before the browser opens a connection.
    async fn guard_target(&self, url: &str) -> GuardResult;

    /// Synchronous re-check of a single redirect-hop URL. Called for every
    /// hop the page follows so a public URL that 30x-es to an internal
    /// address is stopped mid-chain.
    fn guard_redirect(&self, url: &str) -> GuardResult;
}

#[cfg(test)]
pub(crate) mod test_guards {
    //! Deterministic guard stand-ins for the crate's own tests. These do NOT
    //! ship — production uses the `net_guard`-backed guard from
    //! `copperclaw-mcp`. They model the *same* decision so the orchestration
    //! tests exercise the allow / block branches without DNS or a real net.

    use super::{GuardResult, NavigationGuard};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A guard that blocks any URL whose host is in a fixed deny-set, and
    /// records every URL it was asked about (both target + redirect). Stands
    /// in for `net_guard` with a hand-picked internal-address set so the
    /// SSRF-rejection tests are deterministic.
    #[derive(Default)]
    pub struct RecordingGuard {
        /// Substrings that, if present in the URL, cause a block. Models the
        /// internal-address classification (e.g. `"169.254.169.254"`,
        /// `"127.0.0.1"`, `"10."`).
        pub deny_substrings: Vec<String>,
        pub target_calls: Mutex<Vec<String>>,
        pub redirect_calls: Mutex<Vec<String>>,
    }

    impl RecordingGuard {
        pub fn blocking(deny: &[&str]) -> Self {
            Self {
                deny_substrings: deny.iter().map(|s| (*s).to_string()).collect(),
                ..Self::default()
            }
        }

        fn decide(&self, url: &str) -> GuardResult {
            if let Some(bad) = self.deny_substrings.iter().find(|d| url.contains(*d)) {
                return Err(format!(
                    "refusing to navigate `{url}`: resolves to a blocked (internal) \
                     address matching `{bad}`, blocked to prevent SSRF"
                ));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl NavigationGuard for RecordingGuard {
        async fn guard_target(&self, url: &str) -> GuardResult {
            self.target_calls.lock().unwrap().push(url.to_string());
            self.decide(url)
        }

        fn guard_redirect(&self, url: &str) -> GuardResult {
            self.redirect_calls.lock().unwrap().push(url.to_string());
            self.decide(url)
        }
    }
}
