//! Canonical portable-breadcrumb schema shared by every channel adapter.
//!
//! A "breadcrumb" is a compact metadata-shaped chip that surfaces what a
//! tool the agent is running is doing — `shell cargo check`, `edit_file
//! src/foo.rs`, etc. — without bloating the conversation with chat rows.
//!
//! The runner emits one of these via the [`MessageKind::Breadcrumb`](
//! copperclaw_types::MessageKind::Breadcrumb) outbound path on tool start
//! (`status = Running`); the host's delivery service routes it through
//! the adapter's [`crate::ChannelAdapter::deliver_breadcrumb`] hook so
//! channels with rich native rendering (Telegram HTML `<code>`, Slack
//! Block Kit `context`, Discord embed footer, Google Chat cards v2,
//! Matrix `m.notice` with `<code>`) can render it as a small chip
//! instead of a plain chat line.
//!
//! When the tool finishes the runner emits an UPDATE that targets the
//! original breadcrumb's `external_id`; adapters with an in-place edit
//! API (Telegram `editMessageText`, Slack `chat.update`, Discord
//! `PATCH /channels/.../messages/...`, Google Chat
//! `spaces.messages.patch`, Matrix `m.replace`) edit the chip in
//! place so the user sees `Running…` → `Done (0.4s)`. Channels without
//! an edit API just emit a fresh chip on completion (visible but
//! harmless).
//!
//! # Field caps
//!
//! Tight caps so the breadcrumb stays a one-glance UX cue on mobile:
//!
//! - `tool_name` ≤ [`MAX_TOOL_NAME_CHARS`].
//! - `detail` ≤ [`MAX_DETAIL_CHARS`].
//! - `summary` ≤ [`MAX_SUMMARY_CHARS`].

use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum characters in [`Breadcrumb::tool_name`].
pub const MAX_TOOL_NAME_CHARS: usize = 64;
/// Maximum characters in [`Breadcrumb::detail`].
pub const MAX_DETAIL_CHARS: usize = 200;
/// Maximum characters in [`Breadcrumb::summary`].
pub const MAX_SUMMARY_CHARS: usize = 200;

/// Lifecycle of a single tool invocation, surfaced through the
/// breadcrumb chip.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum BreadcrumbStatus {
    /// Tool is in flight. Emitted on start.
    Running,
    /// Tool returned successfully. Emitted on completion.
    Done,
    /// Tool errored or was cancelled. Emitted on failure.
    Failed,
}

impl BreadcrumbStatus {
    /// Stable lowercase tag used by adapters that render the status
    /// inline (e.g. Slack's emoji prefix, Matrix's chip prefix).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Convenience: parse the lowercase tag back into the enum.
    /// Returns `None` for unknown strings.
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

impl fmt::Display for BreadcrumbStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One breadcrumb chip — emitted by the runner around each visible tool
/// invocation and rendered natively by every adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Breadcrumb {
    /// Tool the agent is invoking (`shell`, `edit_file`, `web_search`,
    /// …). Already lowercase / `snake_case` to match the MCP tool
    /// registry.
    pub tool_name: String,
    /// Per-tool detail (`cargo check`, `src/foo.rs`, the search query,
    /// …). Optional — when extraction fails the renderer falls back to
    /// just the tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Lifecycle state. `Running` on first emit; the runner updates to
    /// `Done` / `Failed` once the tool returns.
    pub status: BreadcrumbStatus,
    /// Post-completion summary (`passed (1.8s)`, `wrote 12 lines`,
    /// `timeout`). Only meaningful on `Done` / `Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Sub-steps for a rolling "activity" chip. When non-empty the
    /// top-level fields are the collapsed one-line summary (current
    /// activity + count) and these are the individual tool steps shown
    /// in an expandable region — Telegram `<blockquote expandable>`,
    /// Slack thread, Discord spoiler. Each step is styled on its own
    /// (bold tool, monospace detail, status marker) rather than dumped as
    /// raw text. Empty for an ordinary single-tool chip; steps never nest
    /// (a step's own `steps` stays empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Breadcrumb>,
}

/// Errors raised by [`Breadcrumb::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BreadcrumbError {
    /// `tool_name` was empty after trim.
    EmptyToolName,
    /// `tool_name` exceeded [`MAX_TOOL_NAME_CHARS`].
    ToolNameTooLong { len: usize, max: usize },
    /// `detail` exceeded [`MAX_DETAIL_CHARS`].
    DetailTooLong { len: usize, max: usize },
    /// `summary` exceeded [`MAX_SUMMARY_CHARS`].
    SummaryTooLong { len: usize, max: usize },
}

impl fmt::Display for BreadcrumbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyToolName => write!(f, "breadcrumb tool_name must be non-empty"),
            Self::ToolNameTooLong { len, max } => {
                write!(f, "tool_name is {len} chars (max {max})")
            }
            Self::DetailTooLong { len, max } => {
                write!(f, "detail is {len} chars (max {max})")
            }
            Self::SummaryTooLong { len, max } => {
                write!(f, "summary is {len} chars (max {max})")
            }
        }
    }
}

impl std::error::Error for BreadcrumbError {}

impl Breadcrumb {
    /// Build a `Running` breadcrumb for the named tool.
    pub fn running(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            detail: None,
            status: BreadcrumbStatus::Running,
            summary: None,
            steps: Vec::new(),
        }
    }

    /// Attach a per-tool detail string. Chainable on top of
    /// [`Self::running`].
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attach the rolling sub-step list (for the aggregate "activity"
    /// chip). The top-level fields stay the collapsed summary.
    #[must_use]
    pub fn with_steps(mut self, steps: Vec<Breadcrumb>) -> Self {
        self.steps = steps;
        self
    }

    /// Mark the breadcrumb as finished. `summary` is the post-completion
    /// blurb (`passed (0.4s)`, `wrote 12 lines`, `error: ENOENT`).
    #[must_use]
    pub fn finished(mut self, ok: bool, summary: Option<String>) -> Self {
        self.status = if ok {
            BreadcrumbStatus::Done
        } else {
            BreadcrumbStatus::Failed
        };
        self.summary = summary;
        self
    }

    /// Apply every schema rule. Returns the first violation so callers
    /// can surface it directly to the runner / model.
    pub fn validate(&self) -> Result<(), BreadcrumbError> {
        if self.tool_name.trim().is_empty() {
            return Err(BreadcrumbError::EmptyToolName);
        }
        let name_len = self.tool_name.chars().count();
        if name_len > MAX_TOOL_NAME_CHARS {
            return Err(BreadcrumbError::ToolNameTooLong {
                len: name_len,
                max: MAX_TOOL_NAME_CHARS,
            });
        }
        if let Some(d) = &self.detail {
            let dlen = d.chars().count();
            if dlen > MAX_DETAIL_CHARS {
                return Err(BreadcrumbError::DetailTooLong {
                    len: dlen,
                    max: MAX_DETAIL_CHARS,
                });
            }
        }
        if let Some(s) = &self.summary {
            let slen = s.chars().count();
            if slen > MAX_SUMMARY_CHARS {
                return Err(BreadcrumbError::SummaryTooLong {
                    len: slen,
                    max: MAX_SUMMARY_CHARS,
                });
            }
        }
        Ok(())
    }

    /// Plain-text rendering used by the default [`crate::ChannelAdapter::deliver_breadcrumb`]
    /// fallback. Mirrors today's `[tool_name] detail` shape so adapters
    /// without a native renderer behave like the legacy chat breadcrumb.
    ///
    /// - `Running`: `[shell] cargo check`
    /// - `Done` with summary: `[shell] cargo check — passed (0.4s)`
    /// - `Failed` with summary: `[shell] cargo check — failed: timeout`
    /// - `Done` no summary: `[shell] cargo check ✓`
    /// - `Failed` no summary: `[shell] cargo check ✗`
    pub fn to_text_fallback(&self) -> String {
        // Rolling aggregate: a summary line plus one plain line per step.
        // Channels without a native collapse affordance still get the
        // low-churn rolling view (just not collapsed).
        if !self.steps.is_empty() {
            let mut out = String::new();
            let head = match &self.detail {
                Some(d) if !d.trim().is_empty() => d.trim().to_string(),
                _ => "working".to_string(),
            };
            out.push_str(&head);
            if let Some(s) = self.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                out.push_str(" · ");
                out.push_str(s);
            }
            for step in &self.steps {
                out.push_str("\n  ");
                out.push_str(&step.to_text_fallback());
            }
            return out;
        }
        let head = match &self.detail {
            Some(d) if !d.trim().is_empty() => format!("[{}] {}", self.tool_name, d.trim()),
            _ => format!("[{}]", self.tool_name),
        };
        match (self.status, self.summary.as_deref()) {
            (BreadcrumbStatus::Running, _) => head,
            (BreadcrumbStatus::Done, Some(s)) if !s.trim().is_empty() => {
                format!("{head} — {}", s.trim())
            }
            (BreadcrumbStatus::Failed, Some(s)) if !s.trim().is_empty() => {
                format!("{head} — failed: {}", s.trim())
            }
            // The status-only suffix uses ASCII glyphs so it survives every
            // channel's text encoding — adapters that want fancier presence
            // are expected to override `deliver_breadcrumb`.
            (BreadcrumbStatus::Done, _) => format!("{head} (done)"),
            (BreadcrumbStatus::Failed, _) => format!("{head} (failed)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_shell() -> Breadcrumb {
        Breadcrumb::running("shell").with_detail("cargo check")
    }

    #[test]
    fn running_constructor_sets_status() {
        let b = Breadcrumb::running("shell");
        assert_eq!(b.status, BreadcrumbStatus::Running);
        assert!(b.detail.is_none());
        assert!(b.summary.is_none());
    }

    #[test]
    fn finished_sets_done_when_ok() {
        let b = running_shell().finished(true, Some("passed (0.4s)".into()));
        assert_eq!(b.status, BreadcrumbStatus::Done);
        assert_eq!(b.summary.as_deref(), Some("passed (0.4s)"));
    }

    #[test]
    fn finished_sets_failed_when_not_ok() {
        let b = running_shell().finished(false, Some("ENOENT".into()));
        assert_eq!(b.status, BreadcrumbStatus::Failed);
        assert_eq!(b.summary.as_deref(), Some("ENOENT"));
    }

    #[test]
    fn validate_happy_path() {
        running_shell().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_tool_name() {
        let mut b = running_shell();
        b.tool_name = "   ".into();
        assert_eq!(b.validate().unwrap_err(), BreadcrumbError::EmptyToolName);
    }

    #[test]
    fn validate_rejects_tool_name_too_long() {
        let mut b = running_shell();
        b.tool_name = "a".repeat(MAX_TOOL_NAME_CHARS + 1);
        assert!(matches!(
            b.validate(),
            Err(BreadcrumbError::ToolNameTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_detail_too_long() {
        let mut b = running_shell();
        b.detail = Some("d".repeat(MAX_DETAIL_CHARS + 1));
        assert!(matches!(
            b.validate(),
            Err(BreadcrumbError::DetailTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_summary_too_long() {
        let mut b = running_shell();
        b.summary = Some("s".repeat(MAX_SUMMARY_CHARS + 1));
        assert!(matches!(
            b.validate(),
            Err(BreadcrumbError::SummaryTooLong { .. })
        ));
    }

    #[test]
    fn text_fallback_running_with_detail() {
        let b = running_shell();
        assert_eq!(b.to_text_fallback(), "[shell] cargo check");
    }

    #[test]
    fn text_fallback_running_without_detail() {
        let b = Breadcrumb::running("shell");
        assert_eq!(b.to_text_fallback(), "[shell]");
    }

    #[test]
    fn text_fallback_done_with_summary() {
        let b = running_shell().finished(true, Some("passed (0.4s)".into()));
        assert_eq!(
            b.to_text_fallback(),
            "[shell] cargo check — passed (0.4s)"
        );
    }

    #[test]
    fn text_fallback_failed_with_summary() {
        let b = running_shell().finished(false, Some("timeout".into()));
        assert_eq!(
            b.to_text_fallback(),
            "[shell] cargo check — failed: timeout"
        );
    }

    #[test]
    fn text_fallback_done_no_summary_marks_with_done_suffix() {
        let b = running_shell().finished(true, None);
        assert_eq!(b.to_text_fallback(), "[shell] cargo check (done)");
    }

    #[test]
    fn text_fallback_failed_no_summary_marks_with_failed_suffix() {
        let b = running_shell().finished(false, None);
        assert_eq!(b.to_text_fallback(), "[shell] cargo check (failed)");
    }

    #[test]
    fn text_fallback_aggregate_renders_summary_plus_step_lines() {
        let steps = vec![
            Breadcrumb::running("read_file")
                .with_detail("a.rs")
                .finished(true, Some("10 lines".into())),
            Breadcrumb::running("shell").with_detail("cargo build"),
        ];
        let agg = Breadcrumb {
            tool_name: "activity".into(),
            detail: Some("shell cargo build".into()),
            status: BreadcrumbStatus::Running,
            summary: Some("1/2 steps".into()),
            steps,
        };
        let txt = agg.to_text_fallback();
        assert!(
            txt.starts_with("shell cargo build · 1/2 steps"),
            "summary line: {txt}"
        );
        // Each step on its own indented line, reusing the single-chip format.
        assert!(txt.contains("\n  [read_file] a.rs — 10 lines"), "{txt}");
        assert!(txt.contains("\n  [shell] cargo build"), "{txt}");
    }

    #[test]
    fn breadcrumb_status_round_trips_through_str() {
        for s in [
            BreadcrumbStatus::Running,
            BreadcrumbStatus::Done,
            BreadcrumbStatus::Failed,
        ] {
            assert_eq!(BreadcrumbStatus::parse_str(s.as_str()), Some(s));
        }
        assert_eq!(BreadcrumbStatus::parse_str("nope"), None);
    }

    #[test]
    fn breadcrumb_status_serde_lowercase() {
        let j = serde_json::to_string(&BreadcrumbStatus::Running).unwrap();
        assert_eq!(j, r#""running""#);
        let back: BreadcrumbStatus = serde_json::from_str(&j).unwrap();
        assert_eq!(back, BreadcrumbStatus::Running);
    }

    #[test]
    fn breadcrumb_serde_roundtrip_full() {
        let b = running_shell().finished(true, Some("passed".into()));
        let j = serde_json::to_string(&b).unwrap();
        let back: Breadcrumb = serde_json::from_str(&j).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn breadcrumb_serde_skips_default_fields() {
        let b = Breadcrumb::running("shell");
        let j = serde_json::to_string(&b).unwrap();
        // detail/summary should NOT appear when None — keeps row content
        // compact for the delivery loop / replay fixtures.
        assert_eq!(j, r#"{"tool_name":"shell","status":"running"}"#);
    }

    #[test]
    fn error_display_messages_are_non_empty() {
        let cases = [
            BreadcrumbError::EmptyToolName,
            BreadcrumbError::ToolNameTooLong { len: 100, max: 64 },
            BreadcrumbError::DetailTooLong { len: 999, max: 200 },
            BreadcrumbError::SummaryTooLong { len: 999, max: 200 },
        ];
        for err in cases {
            assert!(!format!("{err}").is_empty());
        }
    }

    #[test]
    fn breadcrumb_status_display_matches_as_str() {
        for s in [
            BreadcrumbStatus::Running,
            BreadcrumbStatus::Done,
            BreadcrumbStatus::Failed,
        ] {
            assert_eq!(format!("{s}"), s.as_str());
        }
    }
}
