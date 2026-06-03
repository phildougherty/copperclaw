//! Render request / output types for the read-only browser subset (Phase 5a).
//!
//! Phase 5a is **read-only**: render a page and return either a screenshot
//! (a host-side file path), the page's DOM text, or a read-only
//! ARIA/accessibility snapshot. There is NO interactive click / type / scroll
//! — that is Phase 5b and explicitly out of scope here.

use serde::{Deserialize, Serialize};

/// Which read-only artifact the caller wants back. Each maps to a distinct
/// read-only CDP operation; none of them mutates page state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderMode {
    /// A PNG screenshot of the rendered viewport. The output carries the
    /// host-side path to the written image (an artifact), not the bytes.
    Screenshot,
    /// The page's visible DOM text (the rendered `document.body.innerText`
    /// equivalent), for the model to read.
    DomText,
    /// A read-only ARIA / accessibility-tree snapshot — the structured roles
    /// and names the page exposes, useful for understanding layout without the
    /// full DOM. Read-only: it queries the a11y tree, it does not act on it.
    AriaSnapshot,
}

impl RenderMode {
    /// Stable lower-case token for logs / JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RenderMode::Screenshot => "screenshot",
            RenderMode::DomText => "dom_text",
            RenderMode::AriaSnapshot => "aria_snapshot",
        }
    }
}

/// A read-only render request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderRequest {
    /// The navigation target. Guarded by the SSRF [`crate::NavigationGuard`]
    /// before the browser opens a connection.
    pub url: String,
    /// What to return.
    pub mode: RenderMode,
    /// Navigation timeout in seconds (clamped by the driver).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl RenderRequest {
    /// Validate the request shape. Rejects an empty URL. Scheme/host
    /// validation is the guard's job (so the SSRF classifier owns it).
    pub fn validate(&self) -> Result<(), crate::BrowserError> {
        if self.url.trim().is_empty() {
            return Err(crate::BrowserError::Invalid(
                "`url` must be non-empty".into(),
            ));
        }
        Ok(())
    }
}

/// Provenance tag on render output.
///
/// A rendered page is external, attacker-influenceable content — identical in
/// trust terms to a `web_fetch` body. It enters the transcript tagged
/// [`Provenance::Untrusted`] so the runner's coarse provenance gate (the
/// Wave-4 `TurnTrust` path) treats the turn as tainted and blocks credentialed
/// external actions until fresh approval. There is no "trusted" render — the
/// tag is fixed; the type exists to make the contract explicit and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// External, attacker-influenceable content. The only value a render
    /// output ever carries.
    #[default]
    Untrusted,
}

impl Provenance {
    /// Stable wire token (matches the `web_fetch` / memory provenance forms).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Untrusted => "untrusted",
        }
    }
}

/// The artifact a render produced, alongside the always-`Untrusted`
/// provenance tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderOutput {
    /// The URL that was actually rendered (post-redirect, as the driver
    /// reports it).
    pub final_url: String,
    /// The render mode that produced this output.
    pub mode: RenderMode,
    /// Provenance — always [`Provenance::Untrusted`] for browser output.
    pub provenance: Provenance,
    /// For [`RenderMode::Screenshot`]: the host-side path to the written PNG.
    /// `None` for the text/aria modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_path: Option<String>,
    /// For [`RenderMode::DomText`] / [`RenderMode::AriaSnapshot`]: the text /
    /// snapshot body. `None` for the screenshot mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// HTTP status of the final navigation, if the driver surfaced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

impl RenderOutput {
    /// Build a screenshot output (untrusted). `path` is the host-side PNG.
    #[must_use]
    pub fn screenshot(final_url: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            final_url: final_url.into(),
            mode: RenderMode::Screenshot,
            provenance: Provenance::Untrusted,
            screenshot_path: Some(path.into()),
            text: None,
            status: None,
        }
    }

    /// Build a text/aria output (untrusted).
    #[must_use]
    pub fn text(final_url: impl Into<String>, mode: RenderMode, body: impl Into<String>) -> Self {
        Self {
            final_url: final_url.into(),
            mode,
            provenance: Provenance::Untrusted,
            screenshot_path: None,
            text: Some(body.into()),
            status: None,
        }
    }

    /// Set the HTTP status (builder).
    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// Whether this output is tagged untrusted. Always `true` — the helper
    /// exists so callers (and tests) can assert the invariant without
    /// pattern-matching.
    #[must_use]
    pub fn is_untrusted(&self) -> bool {
        self.provenance == Provenance::Untrusted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_mode_tokens_stable() {
        assert_eq!(RenderMode::Screenshot.as_str(), "screenshot");
        assert_eq!(RenderMode::DomText.as_str(), "dom_text");
        assert_eq!(RenderMode::AriaSnapshot.as_str(), "aria_snapshot");
    }

    #[test]
    fn render_mode_serde_round_trip() {
        for m in [
            RenderMode::Screenshot,
            RenderMode::DomText,
            RenderMode::AriaSnapshot,
        ] {
            let j = serde_json::to_string(&m).unwrap();
            let back: RenderMode = serde_json::from_str(&j).unwrap();
            assert_eq!(m, back);
        }
        // Wire form matches the snake_case token.
        assert_eq!(
            serde_json::to_string(&RenderMode::AriaSnapshot).unwrap(),
            "\"aria_snapshot\""
        );
    }

    #[test]
    fn request_validate_rejects_empty_url() {
        let req = RenderRequest {
            url: "   ".into(),
            mode: RenderMode::DomText,
            timeout_secs: None,
        };
        assert!(matches!(
            req.validate(),
            Err(crate::BrowserError::Invalid(_))
        ));
    }

    #[test]
    fn request_validate_accepts_nonempty_url() {
        let req = RenderRequest {
            url: "https://example.com".into(),
            mode: RenderMode::Screenshot,
            timeout_secs: Some(30),
        };
        assert!(req.validate().is_ok());
    }

    #[test]
    fn provenance_is_always_untrusted() {
        // The default and the only variant: untrusted.
        assert_eq!(Provenance::default(), Provenance::Untrusted);
        assert_eq!(Provenance::Untrusted.as_str(), "untrusted");
    }

    #[test]
    fn screenshot_output_is_untrusted_with_path() {
        let out = RenderOutput::screenshot("https://e.test/final", "/data/shot.png");
        assert!(out.is_untrusted());
        assert_eq!(out.screenshot_path.as_deref(), Some("/data/shot.png"));
        assert!(out.text.is_none());
        assert_eq!(out.mode, RenderMode::Screenshot);
    }

    #[test]
    fn text_output_is_untrusted_with_body() {
        let out = RenderOutput::text("https://e.test", RenderMode::DomText, "hello world")
            .with_status(200);
        assert!(out.is_untrusted());
        assert_eq!(out.text.as_deref(), Some("hello world"));
        assert!(out.screenshot_path.is_none());
        assert_eq!(out.status, Some(200));
    }

    #[test]
    fn output_serde_includes_untrusted_provenance() {
        let out = RenderOutput::text("https://e.test", RenderMode::AriaSnapshot, "{}");
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["provenance"], serde_json::json!("untrusted"));
        // Screenshot field omitted when absent.
        assert!(v.get("screenshot_path").is_none());
    }
}
