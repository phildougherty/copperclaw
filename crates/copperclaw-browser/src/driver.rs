//! Headless-browser driver abstraction + the read-only render orchestration.
//!
//! [`BrowserDriver`] is the seam between the orchestration (pure, tested) and
//! the actual headless Chromium / CDP session (the runtime path). A real
//! driver speaks the Chrome `DevTools` Protocol against a Chromium running in
//! the dedicated child container; standing that up needs a live container +
//! Chromium binary, so the real driver is the **deferred runtime path** while
//! the orchestration that drives it is fully covered here against a mock.
//!
//! [`render`] is the orchestration: it gates on opt-in (caller's
//! responsibility, asserted upstream), runs the SSRF pre-flight + per-redirect
//! guard, invokes the driver for the requested read-only artifact, and stamps
//! the output [`crate::Provenance::Untrusted`]. No interactive actions exist —
//! Phase 5a is render-only.

use async_trait::async_trait;

use crate::error::BrowserError;
use crate::guard::NavigationGuard;
use crate::render::{RenderMode, RenderOutput, RenderRequest};

/// One navigation a driver performed, as it reports it back. Lets the
/// orchestration re-guard each redirect hop the page followed before trusting
/// the final artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Navigation {
    /// The redirect chain the page followed, in order, EXCLUDING the initial
    /// target (which the async pre-flight already guarded). Each entry is a
    /// hop URL the orchestration re-checks against the guard.
    pub redirect_chain: Vec<String>,
    /// The final URL after redirects.
    pub final_url: String,
    /// Final HTTP status, if known.
    pub status: Option<u16>,
}

/// The read-only artifact a driver produced for one render mode. The
/// orchestration wraps this into a provenance-tagged [`RenderOutput`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedArtifact {
    /// Host-side path to a written PNG (screenshot mode).
    ScreenshotPath(String),
    /// DOM text / ARIA snapshot body (text modes).
    Text(String),
}

/// Result of a driver render: the navigation it performed + the artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverRender {
    pub navigation: Navigation,
    pub artifact: RenderedArtifact,
}

/// Drives a headless browser to render a page read-only. Implementors speak
/// CDP to a Chromium in the child container; this crate ships only the trait +
/// a test double, because a real CDP session is the deferred runtime path.
#[async_trait]
pub trait BrowserDriver: Send + Sync {
    /// Navigate to `url` and produce the artifact for `mode`. The driver is
    /// responsible for the actual navigation + redirect following and reports
    /// the redirect chain so the orchestration can re-guard each hop.
    ///
    /// MUST NOT perform any interactive action (click/type/scroll) — Phase 5a
    /// is read-only.
    async fn render(&self, url: &str, mode: RenderMode) -> Result<DriverRender, BrowserError>;
}

/// Orchestrate one read-only render: validate → SSRF pre-flight → driver
/// render → per-redirect re-guard → provenance-tagged output.
///
/// The opt-in gate is enforced by the caller (the MCP tool) BEFORE this is
/// reached; `render` assumes the tool is enabled and focuses on the
/// navigation-safety + provenance contract.
pub async fn render(
    req: &RenderRequest,
    guard: &dyn NavigationGuard,
    driver: &dyn BrowserDriver,
) -> Result<RenderOutput, BrowserError> {
    req.validate()?;

    // 1. SSRF pre-flight on the navigation target (async; resolves the host).
    guard
        .guard_target(&req.url)
        .await
        .map_err(BrowserError::Blocked)?;

    // 2. Drive the read-only render.
    let DriverRender {
        navigation,
        artifact,
    } = driver.render(&req.url, req.mode).await?;

    // 3. Re-guard every redirect hop the page followed. A public target that
    //    30x-es into an internal address is the dangerous vector — reject the
    //    whole render if ANY hop lands in a blocked range, before the artifact
    //    is trusted/returned.
    for hop in &navigation.redirect_chain {
        guard.guard_redirect(hop).map_err(BrowserError::Blocked)?;
    }

    // 4. Wrap as an UNTRUSTED-tagged output. Browser content is external,
    //    attacker-influenceable — identical trust terms to a `web_fetch` body.
    let mut out = match artifact {
        RenderedArtifact::ScreenshotPath(path) => {
            RenderOutput::screenshot(&navigation.final_url, path)
        }
        RenderedArtifact::Text(body) => RenderOutput::text(&navigation.final_url, req.mode, body),
    };
    if let Some(status) = navigation.status {
        out = out.with_status(status);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::test_guards::RecordingGuard;
    use crate::render::Provenance;

    /// A driver that returns a canned navigation + artifact. The redirect
    /// chain is configurable so the per-redirect guard path is exercised.
    struct MockDriver {
        redirect_chain: Vec<String>,
        final_url: String,
        status: Option<u16>,
        artifact: RenderedArtifact,
    }

    #[async_trait]
    impl BrowserDriver for MockDriver {
        async fn render(
            &self,
            _url: &str,
            _mode: RenderMode,
        ) -> Result<DriverRender, BrowserError> {
            Ok(DriverRender {
                navigation: Navigation {
                    redirect_chain: self.redirect_chain.clone(),
                    final_url: self.final_url.clone(),
                    status: self.status,
                },
                artifact: self.artifact.clone(),
            })
        }
    }

    fn text_driver() -> MockDriver {
        MockDriver {
            redirect_chain: vec![],
            final_url: "https://example.com/".into(),
            status: Some(200),
            artifact: RenderedArtifact::Text("rendered body".into()),
        }
    }

    fn req(url: &str, mode: RenderMode) -> RenderRequest {
        RenderRequest {
            url: url.into(),
            mode,
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn render_happy_path_text_is_untrusted() {
        let guard = RecordingGuard::default(); // allows everything
        let driver = text_driver();
        let out = render(
            &req("https://example.com", RenderMode::DomText),
            &guard,
            &driver,
        )
        .await
        .unwrap();
        // Provenance contract: ALWAYS untrusted.
        assert_eq!(out.provenance, Provenance::Untrusted);
        assert!(out.is_untrusted());
        assert_eq!(out.text.as_deref(), Some("rendered body"));
        assert_eq!(out.final_url, "https://example.com/");
        assert_eq!(out.status, Some(200));
    }

    #[tokio::test]
    async fn render_screenshot_carries_path_and_is_untrusted() {
        let guard = RecordingGuard::default();
        let driver = MockDriver {
            redirect_chain: vec![],
            final_url: "https://example.com/".into(),
            status: Some(200),
            artifact: RenderedArtifact::ScreenshotPath("/data/browser/shot.png".into()),
        };
        let out = render(
            &req("https://example.com", RenderMode::Screenshot),
            &guard,
            &driver,
        )
        .await
        .unwrap();
        assert!(out.is_untrusted());
        assert_eq!(
            out.screenshot_path.as_deref(),
            Some("/data/browser/shot.png")
        );
        assert!(out.text.is_none());
    }

    #[tokio::test]
    async fn render_blocks_ssrf_target_before_driver_runs() {
        // The pre-flight rejects the metadata endpoint; the driver is never
        // reached.
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let driver = text_driver();
        let err = render(
            &req(
                "http://169.254.169.254/latest/meta-data/",
                RenderMode::DomText,
            ),
            &guard,
            &driver,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        // The target guard recorded the attempt; the redirect guard never ran.
        assert_eq!(guard.target_calls.lock().unwrap().len(), 1);
        assert!(guard.redirect_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn render_blocks_redirect_into_internal_address() {
        // Public target that 302s into the cloud-metadata endpoint: the
        // per-redirect guard must reject the whole render.
        let guard = RecordingGuard::blocking(&["169.254.169.254"]);
        let driver = MockDriver {
            redirect_chain: vec!["http://169.254.169.254/latest/".into()],
            final_url: "http://169.254.169.254/latest/".into(),
            status: Some(200),
            artifact: RenderedArtifact::Text("SECRET".into()),
        };
        let err = render(
            &req("https://public.example/redir", RenderMode::DomText),
            &guard,
            &driver,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, BrowserError::Blocked(_)));
        // Pre-flight passed (public), then the redirect hop was caught.
        assert_eq!(guard.target_calls.lock().unwrap().len(), 1);
        assert_eq!(guard.redirect_calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn render_allows_public_redirect_chain() {
        let guard = RecordingGuard::blocking(&["169.254", "127.0.0.1", "10."]);
        let driver = MockDriver {
            redirect_chain: vec![
                "https://hop1.example/".into(),
                "https://hop2.example/".into(),
            ],
            final_url: "https://hop2.example/".into(),
            status: Some(200),
            artifact: RenderedArtifact::Text("ok".into()),
        };
        let out = render(
            &req("https://start.example", RenderMode::DomText),
            &guard,
            &driver,
        )
        .await
        .unwrap();
        assert!(out.is_untrusted());
        // Both hops were re-guarded.
        assert_eq!(guard.redirect_calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn render_rejects_empty_url() {
        let guard = RecordingGuard::default();
        let driver = text_driver();
        let err = render(&req("  ", RenderMode::DomText), &guard, &driver)
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserError::Invalid(_)));
    }
}
