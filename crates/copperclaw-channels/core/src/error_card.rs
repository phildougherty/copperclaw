//! Canonical error-card schema shared by every channel adapter.
//!
//! An [`ErrorCard`] is a visually-distinct surface used for *host-emitted*
//! errors that need to land in the user's chat — not for model-emitted
//! prose apologies. Three emit sites today:
//!
//! 1. **Internal tool errors** — a tool handler returned an error the
//!    runner couldn't paper over; the host surfaces it as an `ErrorCard`
//!    so the user sees a real "something broke" affordance instead of
//!    a normal-looking chat reply.
//! 2. **Provider terminal failures** — the runner exhausted the
//!    provider-retry budget; the `failure_reason` that plumb commit
//!    `0105626` already surfaces gets wrapped in a card.
//! 3. **Delivery retry exhaustion** — the host's delivery loop ran out
//!    of retries; instead of silently recording a `failed` row the host
//!    additionally emits an `ErrorCard` to the originating channel so the
//!    user knows the previous message didn't get out.
//!
//! Channels with native color affordances render this in red (Slack
//! `attachments.color`, Discord embed `color`, Matrix `<font
//! color="red">`); channels without color use a bold prefix +
//! monospace details (Telegram `<b>Error</b>` HTML, plain-text
//! channels prepend `[ERROR]`). Either way the message stands out from
//! ordinary chat.
//!
//! The model is *never* asked to emit one of these — there is no
//! corresponding MCP tool. Surfacing this kind of failure to the user
//! is a host responsibility because the model has no way to know the
//! retry budget exhausted, the delivery loop gave up, or the tool
//! handler returned an internal error after the runner already
//! returned control to it.
//!
//! # Field caps
//!
//! Tight caps in chars (not bytes) so the card stays a one-glance
//! affordance on mobile clients:
//!
//! - `title` ≤ [`MAX_TITLE_CHARS`] (default "Something went wrong").
//! - `summary` ≤ [`MAX_SUMMARY_CHARS`] (user-facing one-liner).
//! - `details` ≤ [`MAX_DETAILS_CHARS`] (monospace tool stderr / trace
//!   when known; optional — most call sites have no extra context).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum characters in [`ErrorCard::title`].
pub const MAX_TITLE_CHARS: usize = 120;
/// Maximum characters in [`ErrorCard::summary`].
pub const MAX_SUMMARY_CHARS: usize = 500;
/// Maximum characters in [`ErrorCard::details`].
pub const MAX_DETAILS_CHARS: usize = 2000;

/// Where the error originated. Each variant maps to a different
/// per-channel visual treatment — `Internal` and `Provider` are
/// drawn red where the platform supports it; `Delivery` is drawn
/// red as well because operators have asked for visual parity (a
/// retry-exhaustion is just as bad as a tool failing).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ErrorCardKind {
    /// An internal tool handler returned an error the runner couldn't
    /// transparently retry (filesystem unreachable, MCP server crashed,
    /// invalid runner config, …).
    Internal,
    /// The agent provider returned a terminal failure after retry
    /// exhaustion. The user-visible apology that used to surface as
    /// plain text is upgraded to a card.
    Provider,
    /// The host's delivery loop gave up on a previous outbound row
    /// (e.g. Telegram returned 403 three attempts in a row). Operators
    /// rely on this so the `cclaw dropped-messages` list isn't the
    /// only place the failure shows up.
    Delivery,
}

impl ErrorCardKind {
    /// Stable lowercase tag — used by adapters that key visual
    /// treatment off the kind and by JSON serialisation tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Provider => "provider",
            Self::Delivery => "delivery",
        }
    }

    /// Inverse of [`as_str`](Self::as_str). Returns `None` for
    /// unknown strings so callers can decide how to surface the
    /// rejection.
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "internal" => Self::Internal,
            "provider" => Self::Provider,
            "delivery" => Self::Delivery,
            _ => return None,
        })
    }

    /// Short, human-readable label spliced into the text fallback's
    /// `[ERROR: <label>]` prefix. Renderers with color don't use this
    /// (they show the visual treatment instead).
    pub fn label(self) -> &'static str {
        match self {
            Self::Internal => "tool",
            Self::Provider => "provider",
            Self::Delivery => "delivery",
        }
    }
}

impl fmt::Display for ErrorCardKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One error card — emitted by the host (NOT the model) when an
/// internal error needs to surface to the user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorCard {
    /// Short user-facing title. Default is "Something went wrong"
    /// when the call site has nothing better to say.
    pub title: String,
    /// User-facing one-liner explaining the problem in plain
    /// language. NOT raw stderr; that goes in `details`.
    pub summary: String,
    /// Where the error came from. Drives per-channel visual
    /// treatment.
    pub kind: ErrorCardKind,
    /// Optional verbatim text (tool stderr, provider response body,
    /// adapter error string) — rendered in monospace and only shown
    /// when the operator opted in via a per-card flag. Adapters with
    /// no "details slot" can drop this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// True when the host expects to automatically retry this kind
    /// of failure (e.g. provider 502 backoff, transient delivery
    /// failure). Renderers append a "(will retry automatically)"
    /// footer so the user knows they don't need to take action.
    /// False when the failure is terminal and needs human attention.
    pub retryable: bool,
}

/// Errors raised by [`ErrorCard::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCardError {
    /// `title` was empty after trim.
    EmptyTitle,
    /// `title` exceeded [`MAX_TITLE_CHARS`].
    TitleTooLong { len: usize, max: usize },
    /// `summary` was empty after trim.
    EmptySummary,
    /// `summary` exceeded [`MAX_SUMMARY_CHARS`].
    SummaryTooLong { len: usize, max: usize },
    /// `details` exceeded [`MAX_DETAILS_CHARS`].
    DetailsTooLong { len: usize, max: usize },
}

impl fmt::Display for ErrorCardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTitle => write!(f, "error card title must be non-empty"),
            Self::TitleTooLong { len, max } => {
                write!(f, "error card title is {len} chars (max {max})")
            }
            Self::EmptySummary => write!(f, "error card summary must be non-empty"),
            Self::SummaryTooLong { len, max } => {
                write!(f, "error card summary is {len} chars (max {max})")
            }
            Self::DetailsTooLong { len, max } => {
                write!(f, "error card details is {len} chars (max {max})")
            }
        }
    }
}

impl std::error::Error for ErrorCardError {}

impl ErrorCard {
    /// Build a card with the default `"Something went wrong"` title.
    pub fn new(kind: ErrorCardKind, summary: impl Into<String>) -> Self {
        Self {
            title: "Something went wrong".to_owned(),
            summary: summary.into(),
            kind,
            details: None,
            retryable: false,
        }
    }

    /// Replace the default title. Chainable on top of [`Self::new`].
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Attach optional monospace details (tool stderr, etc.).
    #[must_use]
    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    /// Mark the card as a "will retry automatically" affordance.
    #[must_use]
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// Apply every schema rule. Returns the first violation so the
    /// caller can surface it directly.
    pub fn validate(&self) -> Result<(), ErrorCardError> {
        if self.title.trim().is_empty() {
            return Err(ErrorCardError::EmptyTitle);
        }
        let title_len = self.title.chars().count();
        if title_len > MAX_TITLE_CHARS {
            return Err(ErrorCardError::TitleTooLong {
                len: title_len,
                max: MAX_TITLE_CHARS,
            });
        }
        if self.summary.trim().is_empty() {
            return Err(ErrorCardError::EmptySummary);
        }
        let summary_len = self.summary.chars().count();
        if summary_len > MAX_SUMMARY_CHARS {
            return Err(ErrorCardError::SummaryTooLong {
                len: summary_len,
                max: MAX_SUMMARY_CHARS,
            });
        }
        if let Some(d) = &self.details {
            let dlen = d.chars().count();
            if dlen > MAX_DETAILS_CHARS {
                return Err(ErrorCardError::DetailsTooLong {
                    len: dlen,
                    max: MAX_DETAILS_CHARS,
                });
            }
        }
        Ok(())
    }

    /// Plain-text rendering used by the default
    /// [`crate::ChannelAdapter::deliver_error`] fallback and by
    /// channels with no color affordance.
    ///
    /// Shape:
    ///
    /// ```text
    /// [ERROR: tool] <title>
    /// <summary>
    /// (details, indented as a "> " quote block when present)
    /// (will retry automatically)   <-- when retryable
    /// ```
    pub fn to_text_fallback(&self) -> String {
        let mut out = String::with_capacity(self.title.len() + self.summary.len() + 32);
        out.push_str("[ERROR: ");
        out.push_str(self.kind.label());
        out.push_str("] ");
        out.push_str(self.title.trim());
        out.push('\n');
        out.push_str(self.summary.trim());
        if let Some(d) = self.details.as_deref() {
            let d = d.trim();
            if !d.is_empty() {
                out.push('\n');
                // Render details as a quoted block so the visual
                // separation survives plain-text rendering. Each
                // line gets the `> ` prefix.
                for line in d.lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
                // Trim the trailing newline `lines()` left behind.
                if out.ends_with('\n') {
                    out.pop();
                }
            }
        }
        if self.retryable {
            out.push('\n');
            out.push_str("(will retry automatically)");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal() -> ErrorCard {
        ErrorCard::new(ErrorCardKind::Internal, "the shell tool timed out")
    }

    #[test]
    fn new_sets_default_title() {
        let c = internal();
        assert_eq!(c.title, "Something went wrong");
        assert_eq!(c.kind, ErrorCardKind::Internal);
        assert_eq!(c.summary, "the shell tool timed out");
        assert!(c.details.is_none());
        assert!(!c.retryable);
    }

    #[test]
    fn builders_chain() {
        let c = ErrorCard::new(ErrorCardKind::Delivery, "delivery to telegram failed")
            .with_title("Could not reach Telegram")
            .with_details("HTTP 502 from api.telegram.org after 3 retries")
            .retryable();
        assert_eq!(c.title, "Could not reach Telegram");
        assert_eq!(c.kind, ErrorCardKind::Delivery);
        assert!(c.retryable);
        assert!(c.details.as_deref().unwrap().contains("502"));
    }

    #[test]
    fn validate_happy_path() {
        internal().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_title() {
        let mut c = internal();
        c.title = "   ".into();
        assert_eq!(c.validate().unwrap_err(), ErrorCardError::EmptyTitle);
    }

    #[test]
    fn validate_rejects_title_too_long() {
        let mut c = internal();
        c.title = "x".repeat(MAX_TITLE_CHARS + 1);
        assert!(matches!(
            c.validate(),
            Err(ErrorCardError::TitleTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_empty_summary() {
        let mut c = internal();
        c.summary = String::new();
        assert_eq!(c.validate().unwrap_err(), ErrorCardError::EmptySummary);
    }

    #[test]
    fn validate_rejects_summary_too_long() {
        let mut c = internal();
        c.summary = "x".repeat(MAX_SUMMARY_CHARS + 1);
        assert!(matches!(
            c.validate(),
            Err(ErrorCardError::SummaryTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_details_too_long() {
        let mut c = internal();
        c.details = Some("x".repeat(MAX_DETAILS_CHARS + 1));
        assert!(matches!(
            c.validate(),
            Err(ErrorCardError::DetailsTooLong { .. })
        ));
    }

    #[test]
    fn text_fallback_minimal() {
        let c = internal();
        let t = c.to_text_fallback();
        assert!(t.starts_with("[ERROR: tool]"));
        assert!(t.contains("Something went wrong"));
        assert!(t.contains("the shell tool timed out"));
        assert!(!t.contains("will retry"));
    }

    #[test]
    fn text_fallback_with_details_quotes_each_line() {
        let c = internal().with_details("first line\nsecond line\nthird line");
        let t = c.to_text_fallback();
        assert!(t.contains("> first line"));
        assert!(t.contains("> second line"));
        assert!(t.contains("> third line"));
    }

    #[test]
    fn text_fallback_retryable_adds_footer() {
        let c = internal().retryable();
        let t = c.to_text_fallback();
        assert!(t.ends_with("(will retry automatically)"));
    }

    #[test]
    fn text_fallback_label_varies_per_kind() {
        for (kind, expected) in [
            (ErrorCardKind::Internal, "tool"),
            (ErrorCardKind::Provider, "provider"),
            (ErrorCardKind::Delivery, "delivery"),
        ] {
            let c = ErrorCard::new(kind, "x");
            assert!(
                c.to_text_fallback()
                    .starts_with(&format!("[ERROR: {expected}]"))
            );
        }
    }

    #[test]
    fn kind_round_trips_through_str() {
        for k in [
            ErrorCardKind::Internal,
            ErrorCardKind::Provider,
            ErrorCardKind::Delivery,
        ] {
            assert_eq!(ErrorCardKind::parse_str(k.as_str()), Some(k));
        }
        assert_eq!(ErrorCardKind::parse_str("bogus"), None);
    }

    #[test]
    fn kind_serde_lowercase() {
        let j = serde_json::to_string(&ErrorCardKind::Internal).unwrap();
        assert_eq!(j, r#""internal""#);
        let back: ErrorCardKind = serde_json::from_str(&j).unwrap();
        assert_eq!(back, ErrorCardKind::Internal);
    }

    #[test]
    fn error_card_serde_roundtrip_full() {
        let c = internal()
            .with_title("Tool failed")
            .with_details("ENOENT")
            .retryable();
        let j = serde_json::to_string(&c).unwrap();
        let back: ErrorCard = serde_json::from_str(&j).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn error_card_serde_skips_default_details() {
        let c = internal();
        let j = serde_json::to_string(&c).unwrap();
        assert!(
            !j.contains("details"),
            "serialised form must omit details when None: {j}"
        );
    }

    #[test]
    fn error_card_kind_display_matches_as_str() {
        for k in [
            ErrorCardKind::Internal,
            ErrorCardKind::Provider,
            ErrorCardKind::Delivery,
        ] {
            assert_eq!(format!("{k}"), k.as_str());
        }
    }

    #[test]
    fn error_display_messages_are_non_empty() {
        let cases = [
            ErrorCardError::EmptyTitle,
            ErrorCardError::TitleTooLong { len: 200, max: 120 },
            ErrorCardError::EmptySummary,
            ErrorCardError::SummaryTooLong { len: 600, max: 500 },
            ErrorCardError::DetailsTooLong {
                len: 3000,
                max: 2000,
            },
        ];
        for err in cases {
            assert!(!format!("{err}").is_empty());
        }
    }
}
