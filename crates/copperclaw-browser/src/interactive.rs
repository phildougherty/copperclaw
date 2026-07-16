//! Interactive browser actions (Phase 5b, **demand-pull, opt-in**).
//!
//! Phase 5a is read-only (`render`). This module lifts the click / type /
//! scroll / wait-for-selector actions that were explicitly out of scope there —
//! but only as an *incremental* extension of the exact same live path:
//!
//!   * the **same** locked-down child container ([`crate::container`]: no broker
//!     token, deny-default egress, unprivileged user, hardened sandbox),
//!   * the **same** SSRF [`NavigationGuard`] plumbing, re-run on **every**
//!     navigation an interaction can trigger (see [`interact`]),
//!   * the **same** [`crate::Provenance::Untrusted`] tag on the output — an
//!     interactively-driven DOM is still external, attacker-influenceable
//!     content, identical in trust terms to a `web_fetch` body.
//!
//! ## Non-goals (kept explicit in code)
//!
//!   * **No always-on / autonomous browsing loop.** A single [`interact`] call
//!     runs one bounded, caller-scripted action sequence against one page, then
//!     the driver (and, in the live path, the container) is torn down. There is
//!     no persistent session and no agent-driven browse-until-done loop.
//!   * **No browser-writes-memory.** This card renders post-interaction content
//!     for the model to read; it never persists anything to the memory store.
//!
//! ## The SSRF re-guard contract (the security core)
//!
//! A click / form submit / scripted `location` change can navigate the page —
//! including to an internal address that never appeared as an HTTP 30x redirect.
//! So after **every** action (and after the initial navigation) [`interact`]:
//!
//!   1. re-runs the async, DNS-resolving [`NavigationGuard::guard_target`] on
//!      the *settled* `document.location.href` — this catches a JS navigation
//!      (`location.assign`, an anchor click, a form submit) that moved the page
//!      to a blocked address with no redirect event, and
//!   2. re-checks every redirect hop the navigation followed with
//!      [`NavigationGuard::guard_redirect`].
//!
//! Either check failing aborts the whole interaction **before** the artifact is
//! read or returned. The child container's deny-default egress is the belt to
//! this suspenders: it can physically reach only the navigation target's
//! `host:port`, so the guard is defence-in-depth over the network firewall.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::driver::{Navigation, RenderedArtifact};
use crate::error::BrowserError;
use crate::guard::NavigationGuard;
use crate::render::{RenderMode, RenderOutput};

/// Hard cap on the number of scripted actions in one interaction. Bounds the
/// re-guard loop and keeps a single demand-pull call finite (no autonomous
/// browsing loop can be smuggled in as a giant action list).
pub const MAX_ACTIONS: usize = 32;

/// Hard cap on the length of typed text, in bytes.
pub const MAX_TYPE_LEN: usize = 8192;

/// Default per-selector wait budget (milliseconds) when the caller omits one.
pub const DEFAULT_WAIT_MS: u64 = 5_000;

/// Upper bound on a wait-for-selector budget (milliseconds); the driver clamps
/// to this so a single action cannot hang the interaction unboundedly.
pub const MAX_WAIT_MS: u64 = 30_000;

/// One interactive action against the current page. Each maps to a small,
/// well-understood CDP operation. None of them is autonomous — they are the
/// exact steps the caller scripted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum InteractiveAction {
    /// Click the first element matching `selector` (scrolled into view first).
    Click {
        /// CSS selector for the target element.
        selector: String,
    },
    /// Focus the first element matching `selector` and set its value/text to
    /// `text`, dispatching `input` + `change` events so page handlers run.
    Type {
        /// CSS selector for the target input/contenteditable element.
        selector: String,
        /// The text to type.
        text: String,
    },
    /// Scroll: if `selector` is set, scroll that element into view; otherwise
    /// scroll the window by `(dx, dy)` pixels.
    Scroll {
        /// Optional element to scroll into view. `None` → window scroll.
        #[serde(default)]
        selector: Option<String>,
        /// Horizontal window-scroll delta (ignored when `selector` is set).
        #[serde(default)]
        dx: f64,
        /// Vertical window-scroll delta (ignored when `selector` is set).
        #[serde(default)]
        dy: f64,
    },
    /// Poll until an element matching `selector` exists, or `timeout_ms`
    /// (clamped to [`MAX_WAIT_MS`]) elapses. The tool for "wait for the
    /// post-click page to settle" before reading.
    WaitForSelector {
        /// CSS selector to wait for.
        selector: String,
        /// Wait budget in milliseconds; defaults to [`DEFAULT_WAIT_MS`].
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
}

impl InteractiveAction {
    /// Stable lower-case token for logs / metrics.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            InteractiveAction::Click { .. } => "click",
            InteractiveAction::Type { .. } => "type",
            InteractiveAction::Scroll { .. } => "scroll",
            InteractiveAction::WaitForSelector { .. } => "wait_for_selector",
        }
    }

    /// Shape validation for a single action (non-empty selectors, bounded text,
    /// finite scroll deltas).
    fn validate(&self) -> Result<(), BrowserError> {
        let selector_ok = |sel: &str| -> Result<(), BrowserError> {
            if sel.trim().is_empty() {
                return Err(BrowserError::Invalid("`selector` must be non-empty".into()));
            }
            Ok(())
        };
        match self {
            InteractiveAction::Click { selector }
            | InteractiveAction::WaitForSelector { selector, .. } => selector_ok(selector),
            InteractiveAction::Type { selector, text } => {
                selector_ok(selector)?;
                if text.len() > MAX_TYPE_LEN {
                    return Err(BrowserError::Invalid(format!(
                        "`text` exceeds the {MAX_TYPE_LEN}-byte cap"
                    )));
                }
                Ok(())
            }
            InteractiveAction::Scroll { selector, dx, dy } => {
                if let Some(sel) = selector {
                    selector_ok(sel)?;
                }
                if !dx.is_finite() || !dy.is_finite() {
                    return Err(BrowserError::Invalid(
                        "scroll `dx`/`dy` must be finite numbers".into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// One interactive session request: navigate to `url`, run `actions` in order,
/// then read `mode`. Deliberately single-shot — there is no persistent session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InteractRequest {
    /// The initial navigation target. SSRF-guarded before the browser opens a
    /// connection, and re-guarded after every action.
    pub url: String,
    /// The scripted actions to perform, in order. Non-empty, capped at
    /// [`MAX_ACTIONS`].
    pub actions: Vec<InteractiveAction>,
    /// What to read back after the actions run. Defaults to
    /// [`RenderMode::DomText`].
    #[serde(default = "default_mode")]
    pub mode: RenderMode,
    /// Navigation/load timeout in seconds (clamped by the driver).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn default_mode() -> RenderMode {
    RenderMode::DomText
}

impl InteractRequest {
    /// Validate the request shape: non-empty URL, a bounded non-empty action
    /// list, and each action well-formed. Scheme/host classification is the
    /// guard's job (as with [`crate::RenderRequest`]).
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.url.trim().is_empty() {
            return Err(BrowserError::Invalid("`url` must be non-empty".into()));
        }
        if self.actions.is_empty() {
            return Err(BrowserError::Invalid(
                "`actions` must contain at least one action".into(),
            ));
        }
        if self.actions.len() > MAX_ACTIONS {
            return Err(BrowserError::Invalid(format!(
                "`actions` exceeds the {MAX_ACTIONS}-action cap (demand-pull only)"
            )));
        }
        for action in &self.actions {
            action.validate()?;
        }
        Ok(())
    }
}

/// Drives a headless browser through an interactive sequence. Split from
/// [`crate::BrowserDriver`] (the read-only seam) so the read-only path is
/// untouched: implementors decompose a session into navigate / act / read so
/// [`interact`] can re-guard the navigation state after each step.
///
/// Every method MUST report the navigation state (`redirect_chain` +
/// settled `final_url` + `status`) it observed, so the orchestration can
/// re-run the SSRF guard on it. A driver MUST NOT itself trust or return
/// content past a navigation it did not report.
#[async_trait]
pub trait InteractiveDriver: Send + Sync {
    /// Enable the required CDP domains and navigate to `url`; report the
    /// navigation observed.
    async fn navigate(&self, url: &str) -> Result<Navigation, BrowserError>;

    /// Perform one interactive action, then report the navigation state
    /// *after* it — so a click/submit that navigated (even to an internal
    /// address, with no 30x) is re-guarded by [`interact`].
    async fn act(&self, action: &InteractiveAction) -> Result<Navigation, BrowserError>;

    /// Read the final read-only artifact for `mode`. MUST NOT mutate page
    /// state or navigate.
    async fn read(&self, mode: RenderMode) -> Result<RenderedArtifact, BrowserError>;
}

/// Orchestrate one interactive session: pre-flight → navigate → (act →
/// re-guard)* → read → untrusted-tagged output.
///
/// The opt-in gate is enforced by the caller (the MCP tool) BEFORE this is
/// reached; `interact` owns the navigation-safety + provenance contract.
pub async fn interact(
    req: &InteractRequest,
    guard: &dyn NavigationGuard,
    driver: &dyn InteractiveDriver,
) -> Result<RenderOutput, BrowserError> {
    req.validate()?;

    // 1. SSRF pre-flight on the initial navigation target (async; resolves the
    //    host) BEFORE the browser opens a connection.
    guard.guard_target(&req.url).await.map_err(|e| {
        copperclaw_metrics::inc_browser_ssrf_block("interactive_target_preflight");
        BrowserError::Blocked(e)
    })?;

    // 2. Navigate, then re-guard the settled navigation.
    let mut nav = driver.navigate(&req.url).await?;
    reguard_navigation(guard, &nav).await?;

    // 3. Each scripted action can navigate. Perform it, then re-guard the
    //    resulting navigation state — this is the per-navigation SSRF re-guard
    //    that mid-interaction navigation demands.
    for action in &req.actions {
        // M19 A2: meter each scripted action with its outcome. `blocked` is an
        // SSRF re-guard refusal on the resulting navigation; any other error is
        // a driver_error.
        match driver.act(action).await {
            Ok(next) => nav = next,
            Err(e) => {
                let outcome = if matches!(e, BrowserError::Blocked(_)) {
                    "blocked"
                } else {
                    "driver_error"
                };
                copperclaw_metrics::inc_browser_interactive_action(action.kind(), outcome);
                return Err(e);
            }
        }
        if let Err(e) = reguard_navigation(guard, &nav).await {
            copperclaw_metrics::inc_browser_interactive_action(action.kind(), "blocked");
            return Err(e);
        }
        copperclaw_metrics::inc_browser_interactive_action(action.kind(), "ok");
    }

    // 4. Only now — after every navigation has been re-guarded — read the
    //    artifact and wrap it UNTRUSTED (external, attacker-influenceable).
    let artifact = driver.read(req.mode).await?;
    let mut out = match artifact {
        RenderedArtifact::ScreenshotPath(path) => RenderOutput::screenshot(&nav.final_url, path),
        RenderedArtifact::Text(body) => RenderOutput::text(&nav.final_url, req.mode, body),
    };
    if let Some(status) = nav.status {
        out = out.with_status(status);
    }
    Ok(out)
}

/// Re-run the SSRF guard over one observed navigation: the settled final URL
/// (async, DNS-resolving — catches a redirect-less JS navigation to an internal
/// address) plus every redirect hop.
async fn reguard_navigation(
    guard: &dyn NavigationGuard,
    nav: &Navigation,
) -> Result<(), BrowserError> {
    // The settled URL: a click/submit/location-change may have moved the page
    // to an address that never surfaced as an HTTP 30x. Re-resolve + classify
    // it exactly as the initial target.
    guard.guard_target(&nav.final_url).await.map_err(|e| {
        copperclaw_metrics::inc_browser_ssrf_block("interactive_post_nav");
        BrowserError::Blocked(e)
    })?;
    // Plus every redirect hop the navigation followed.
    for hop in &nav.redirect_chain {
        guard.guard_redirect(hop).map_err(|e| {
            copperclaw_metrics::inc_browser_ssrf_block("interactive_redirect_hop");
            BrowserError::Blocked(e)
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::test_guards::RecordingGuard;
    use crate::render::Provenance;
    use std::sync::Mutex;

    /// A scripted interactive driver: `navigate` and each `act` return a canned
    /// [`Navigation`] (drawn in order from `navs`), and `read` returns a canned
    /// artifact. Records the actions it was asked to perform. Exercises the
    /// orchestration + per-navigation re-guard without a live browser.
    struct MockInteractiveDriver {
        /// Navigation returned by `navigate`, then by each successive `act`.
        navs: Mutex<std::collections::VecDeque<Navigation>>,
        /// Artifact `read` returns.
        artifact: RenderedArtifact,
        /// Actions performed, in order.
        acted: Mutex<Vec<String>>,
    }

    impl MockInteractiveDriver {
        fn new(navs: Vec<Navigation>, artifact: RenderedArtifact) -> Self {
            Self {
                navs: Mutex::new(navs.into()),
                artifact,
                acted: Mutex::new(Vec::new()),
            }
        }

        fn next_nav(&self) -> Navigation {
            self.navs.lock().unwrap().pop_front().unwrap_or(Navigation {
                redirect_chain: vec![],
                final_url: "https://example.com/".into(),
                status: Some(200),
            })
        }
    }

    #[async_trait]
    impl InteractiveDriver for MockInteractiveDriver {
        async fn navigate(&self, _url: &str) -> Result<Navigation, BrowserError> {
            Ok(self.next_nav())
        }
        async fn act(&self, action: &InteractiveAction) -> Result<Navigation, BrowserError> {
            self.acted.lock().unwrap().push(action.kind().to_string());
            Ok(self.next_nav())
        }
        async fn read(&self, _mode: RenderMode) -> Result<RenderedArtifact, BrowserError> {
            Ok(self.artifact.clone())
        }
    }

    fn nav(final_url: &str) -> Navigation {
        Navigation {
            redirect_chain: vec![],
            final_url: final_url.into(),
            status: Some(200),
        }
    }

    fn click_then_read() -> InteractRequest {
        InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Click {
                selector: "#go".into(),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn click_then_read_returns_post_click_dom_untrusted() {
        // Acceptance: a scripted click-then-read returns the post-click DOM, and
        // it is tagged untrusted.
        let guard = RecordingGuard::default();
        let driver = MockInteractiveDriver::new(
            vec![nav("https://example.com/"), nav("https://example.com/next")],
            RenderedArtifact::Text("post-click content".into()),
        );
        let out = interact(&click_then_read(), &guard, &driver).await.unwrap();
        assert_eq!(out.provenance, Provenance::Untrusted);
        assert!(out.is_untrusted());
        assert_eq!(out.text.as_deref(), Some("post-click content"));
        // The final_url reflects the post-click navigation.
        assert_eq!(out.final_url, "https://example.com/next");
        assert_eq!(driver.acted.lock().unwrap().as_slice(), &["click"]);
    }

    #[tokio::test]
    async fn ssrf_blocks_navigation_to_private_address_mid_interaction() {
        // Acceptance: the SSRF guard blocks a navigation to a private address
        // triggered by an interaction (a click that set location to an internal
        // host, reported as the settled final_url — no 30x involved).
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let driver = MockInteractiveDriver::new(
            vec![
                nav("https://example.com/"),               // initial nav: public
                nav("http://169.254.169.254/latest/meta"), // post-click: internal
            ],
            RenderedArtifact::Text("SECRET".into()),
        );
        let err = interact(&click_then_read(), &guard, &driver)
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        // The click ran; the post-nav target guard caught the internal address
        // and the artifact was NEVER read/returned.
        assert_eq!(driver.acted.lock().unwrap().as_slice(), &["click"]);
    }

    #[tokio::test]
    async fn ssrf_blocks_redirect_hop_triggered_by_interaction() {
        // A click that 30x-ed through an internal hop: the per-redirect re-guard
        // catches the hop even if the final URL is public.
        let guard = RecordingGuard::blocking(&["10.0.0.5"]);
        let mut post_click = nav("https://public.example/final");
        post_click.redirect_chain = vec!["http://10.0.0.5/internal".into()];
        let driver = MockInteractiveDriver::new(
            vec![nav("https://example.com/"), post_click],
            RenderedArtifact::Text("body".into()),
        );
        let err = interact(&click_then_read(), &guard, &driver)
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        assert_eq!(guard.redirect_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ssrf_blocks_initial_target_before_any_action() {
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let driver = MockInteractiveDriver::new(
            vec![nav("https://example.com/")],
            RenderedArtifact::Text("body".into()),
        );
        let mut req = click_then_read();
        req.url = "http://169.254.169.254/latest/meta-data/".into();
        let err = interact(&req, &guard, &driver).await.unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        // Pre-flight refused before navigate/act ran.
        assert!(driver.acted.lock().unwrap().is_empty());
        assert!(driver.navs.lock().unwrap().len() == 1, "navigate never ran");
    }

    #[tokio::test]
    async fn initial_navigation_redirect_into_internal_is_blocked() {
        // The initial navigate itself 302s into an internal address: re-guarded
        // before any action runs.
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let mut first = nav("http://169.254.169.254/latest/");
        first.redirect_chain = vec!["http://169.254.169.254/latest/".into()];
        let driver =
            MockInteractiveDriver::new(vec![first], RenderedArtifact::Text("SECRET".into()));
        let err = interact(&click_then_read(), &guard, &driver)
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        assert!(driver.acted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn multi_action_sequence_reguards_each_step() {
        let guard = RecordingGuard::blocking(&["127.0.0.1", "10."]);
        let driver = MockInteractiveDriver::new(
            vec![
                nav("https://example.com/"),
                nav("https://example.com/a"),
                nav("https://example.com/b"),
                nav("https://example.com/c"),
            ],
            RenderedArtifact::Text("done".into()),
        );
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![
                InteractiveAction::Type {
                    selector: "#in".into(),
                    text: "hi".into(),
                },
                InteractiveAction::Scroll {
                    selector: None,
                    dx: 0.0,
                    dy: 400.0,
                },
                InteractiveAction::Click {
                    selector: "#submit".into(),
                },
            ],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        let out = interact(&req, &guard, &driver).await.unwrap();
        assert!(out.is_untrusted());
        assert_eq!(
            driver.acted.lock().unwrap().as_slice(),
            &["type", "scroll", "click"]
        );
        // guard_target ran once pre-flight + once per navigation (initial + 3
        // actions) = 5.
        assert_eq!(guard.target_calls.lock().unwrap().len(), 5);
    }

    // ── request validation ───────────────────────────────────────────────

    #[test]
    fn validate_rejects_empty_url() {
        let mut req = click_then_read();
        req.url = "   ".into();
        assert!(matches!(req.validate(), Err(BrowserError::Invalid(_))));
    }

    #[test]
    fn validate_rejects_empty_action_list() {
        let mut req = click_then_read();
        req.actions.clear();
        assert!(matches!(req.validate(), Err(BrowserError::Invalid(_))));
    }

    #[test]
    fn validate_rejects_too_many_actions() {
        let mut req = click_then_read();
        req.actions = (0..=MAX_ACTIONS)
            .map(|_| InteractiveAction::Click {
                selector: "#x".into(),
            })
            .collect();
        assert!(matches!(req.validate(), Err(BrowserError::Invalid(_))));
    }

    #[test]
    fn validate_rejects_empty_selector() {
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Click {
                selector: "  ".into(),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        assert!(matches!(req.validate(), Err(BrowserError::Invalid(_))));
    }

    #[test]
    fn validate_rejects_oversized_type_text() {
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Type {
                selector: "#in".into(),
                text: "x".repeat(MAX_TYPE_LEN + 1),
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        assert!(matches!(req.validate(), Err(BrowserError::Invalid(_))));
    }

    #[test]
    fn validate_rejects_non_finite_scroll() {
        let req = InteractRequest {
            url: "https://example.com".into(),
            actions: vec![InteractiveAction::Scroll {
                selector: None,
                dx: f64::NAN,
                dy: 0.0,
            }],
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        assert!(matches!(req.validate(), Err(BrowserError::Invalid(_))));
    }

    #[test]
    fn action_serde_tagged_round_trip() {
        let actions = vec![
            InteractiveAction::Click {
                selector: "#a".into(),
            },
            InteractiveAction::Type {
                selector: "#b".into(),
                text: "hi".into(),
            },
            InteractiveAction::Scroll {
                selector: None,
                dx: 1.0,
                dy: 2.0,
            },
            InteractiveAction::WaitForSelector {
                selector: ".done".into(),
                timeout_ms: Some(1000),
            },
        ];
        for a in actions {
            let j = serde_json::to_string(&a).unwrap();
            let back: InteractiveAction = serde_json::from_str(&j).unwrap();
            assert_eq!(a, back);
        }
        // Wire form uses the `action` tag.
        let j = serde_json::to_value(&InteractiveAction::Click {
            selector: "#a".into(),
        })
        .unwrap();
        assert_eq!(j["action"], serde_json::json!("click"));
    }

    #[test]
    fn request_defaults_mode_to_dom_text() {
        let req: InteractRequest = serde_json::from_value(serde_json::json!({
            "url": "https://example.com",
            "actions": [ { "action": "click", "selector": "#go" } ]
        }))
        .unwrap();
        assert_eq!(req.mode, RenderMode::DomText);
    }
}
