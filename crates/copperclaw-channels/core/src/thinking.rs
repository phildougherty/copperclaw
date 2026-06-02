//! Canonical thinking-block schema shared by every channel adapter.
//!
//! Reasoning-capable models (Anthropic extended thinking, `Kimi K2.6`,
//! `Qwen QwQ`, `DeepSeek R1`, …) stream a chain-of-thought block before
//! their user-facing reply. Today the runner drops the text on the
//! floor: it does not pollute the agent's reply (see
//! `copperclaw_providers::anthropic::ThinkingAccumulator`) but the user
//! never sees it either.
//!
//! When the per-group `surface_thinking` flag is on, the provider
//! emits a [`ProviderEvent::Thinking`] event for each completed
//! reasoning block, the runner persists a [`MessageKind::Thinking`]
//! row carrying a canonical [`ThinkingBlock`] payload, and the host's
//! delivery loop routes the row through the adapter's
//! [`crate::ChannelAdapter::deliver_thinking`] hook. Native renderers
//! draw a collapsed UI primitive (Telegram `<blockquote expandable>`,
//! Slack `context` block, Discord embed, Google Chat
//! `collapsibleSection`, Matrix `<details>`); adapters without a
//! native renderer fall back to a quoted text block via the
//! trait-level default impl.
//!
//! # Privacy default
//!
//! `surface_thinking` defaults to **off** — surfacing model reasoning
//! has privacy implications (mid-thought speculation about the user,
//! debugging notes the model didn't intend the user to see, etc.).
//! Operators opt in per-group via `cclaw groups config edit <id>`.
//!
//! # Field caps
//!
//! - `text` ≤ [`MAX_THINKING_CHARS`]. Counts codepoints, not bytes,
//!   so non-ASCII reasoning text gets the same headroom.
//! - `redacted == true` permits `text` to be empty (the model emitted
//!   a `redacted_thinking` block the user is not allowed to see).
//! - `model` is optional provenance — a short marker like
//!   `"claude-opus-4-7"` so the user can disambiguate which model
//!   produced the reasoning when their group fans out across several.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum characters allowed in [`ThinkingBlock::text`].
///
/// 8000 codepoints is roughly two long-form mobile screens of muted
/// quoted prose; enough to give the user a real sense of the model's
/// chain-of-thought without turning the channel into a stream of
/// internal monologue.
pub const MAX_THINKING_CHARS: usize = 8_000;

/// Maximum characters allowed in [`ThinkingBlock::model`].
///
/// Hard ceiling on the provenance tag; matches typical model-name
/// shapes (`claude-opus-4-7-1m`, `kimi-k2.6-preview`, …) with
/// generous headroom.
pub const MAX_MODEL_CHARS: usize = 64;

/// One thinking block — emitted by the runner when the provider
/// streamed a `thinking` (or `redacted_thinking`) content block and
/// the per-group `surface_thinking` flag is on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThinkingBlock {
    /// The raw thinking text the model emitted. Empty when
    /// `redacted == true`. Capped at [`MAX_THINKING_CHARS`].
    pub text: String,
    /// `true` when the upstream block was `redacted_thinking` (an
    /// opaque encoded reasoning blob the user is not allowed to
    /// see). The renderer surfaces a placeholder instead of the raw
    /// text.
    #[serde(default)]
    pub redacted: bool,
    /// Optional provenance marker — the model identifier (e.g.
    /// `"claude-opus-4-7"`, `"kimi-k2.6"`). Lets the user
    /// disambiguate which reasoning model produced this block when
    /// their group fans out across several. Capped at
    /// [`MAX_MODEL_CHARS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Errors raised by [`ThinkingBlock::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingBlockError {
    /// `text` was empty AND the block was not flagged `redacted`.
    /// (A non-redacted thinking block with no text would render as a
    /// blank chip; reject it at the schema boundary.)
    EmptyTextWhenNotRedacted,
    /// `text` exceeded [`MAX_THINKING_CHARS`].
    TextTooLong { len: usize, max: usize },
    /// `model` exceeded [`MAX_MODEL_CHARS`].
    ModelTooLong { len: usize, max: usize },
}

impl fmt::Display for ThinkingBlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTextWhenNotRedacted => {
                write!(
                    f,
                    "thinking text must be non-empty unless `redacted` is true"
                )
            }
            Self::TextTooLong { len, max } => {
                write!(f, "thinking text is {len} chars (max {max})")
            }
            Self::ModelTooLong { len, max } => {
                write!(f, "thinking model is {len} chars (max {max})")
            }
        }
    }
}

impl std::error::Error for ThinkingBlockError {}

impl ThinkingBlock {
    /// Build a visible thinking block (`redacted=false`) with no
    /// provenance tag. Convenience for the runner's emit path; tests
    /// also use it to seed fixtures.
    pub fn visible(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            redacted: false,
            model: None,
        }
    }

    /// Build a redacted thinking block — `text` is the raw blob the
    /// upstream returned (kept around for audit), but renderers MUST
    /// substitute a placeholder rather than displaying it.
    pub fn redacted(blob: impl Into<String>) -> Self {
        Self {
            text: blob.into(),
            redacted: true,
            model: None,
        }
    }

    /// Attach a provenance model tag. Chainable.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Apply every schema rule. Returns the first violation so callers
    /// can surface it directly back to the runner.
    pub fn validate(&self) -> Result<(), ThinkingBlockError> {
        if !self.redacted && self.text.trim().is_empty() {
            return Err(ThinkingBlockError::EmptyTextWhenNotRedacted);
        }
        let text_len = self.text.chars().count();
        if text_len > MAX_THINKING_CHARS {
            return Err(ThinkingBlockError::TextTooLong {
                len: text_len,
                max: MAX_THINKING_CHARS,
            });
        }
        if let Some(m) = &self.model {
            let mlen = m.chars().count();
            if mlen > MAX_MODEL_CHARS {
                return Err(ThinkingBlockError::ModelTooLong {
                    len: mlen,
                    max: MAX_MODEL_CHARS,
                });
            }
        }
        Ok(())
    }

    /// Plain-text rendering used by the default
    /// [`crate::ChannelAdapter::deliver_thinking`] fallback. Each
    /// non-empty line is prefixed with `> ` and the whole block gets
    /// a `[reasoning]` (or `[reasoning: <model>]`) header so the user
    /// can tell at a glance this isn't the model's chat reply.
    ///
    /// `redacted` blocks render as a single placeholder line — the
    /// raw blob is never put on the wire even on plain-text channels.
    pub fn to_text_fallback(&self) -> String {
        let header = match self.model.as_deref() {
            Some(m) if !m.trim().is_empty() => format!("[reasoning: {}]", m.trim()),
            _ => "[reasoning]".to_string(),
        };
        if self.redacted {
            return format!("{header}\n> (redacted reasoning)");
        }
        let mut out = String::with_capacity(header.len() + self.text.len() + 16);
        out.push_str(&header);
        for line in self.text.lines() {
            out.push('\n');
            out.push_str("> ");
            out.push_str(line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_constructor_sets_defaults() {
        let t = ThinkingBlock::visible("I should reply briefly.");
        assert_eq!(t.text, "I should reply briefly.");
        assert!(!t.redacted);
        assert!(t.model.is_none());
    }

    #[test]
    fn redacted_constructor_sets_flag() {
        let t = ThinkingBlock::redacted("opaque-blob");
        assert!(t.redacted);
        assert_eq!(t.text, "opaque-blob");
    }

    #[test]
    fn with_model_attaches_provenance() {
        let t = ThinkingBlock::visible("ok").with_model("claude-opus-4-7");
        assert_eq!(t.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn validate_happy_path() {
        ThinkingBlock::visible("a chain of thought")
            .validate()
            .unwrap();
    }

    #[test]
    fn validate_rejects_empty_text_when_not_redacted() {
        let t = ThinkingBlock {
            text: "   ".into(),
            redacted: false,
            model: None,
        };
        assert_eq!(
            t.validate().unwrap_err(),
            ThinkingBlockError::EmptyTextWhenNotRedacted
        );
    }

    #[test]
    fn validate_allows_empty_text_when_redacted() {
        let t = ThinkingBlock {
            text: String::new(),
            redacted: true,
            model: None,
        };
        t.validate().unwrap();
    }

    #[test]
    fn validate_rejects_text_too_long() {
        let t = ThinkingBlock::visible("x".repeat(MAX_THINKING_CHARS + 1));
        assert!(matches!(
            t.validate(),
            Err(ThinkingBlockError::TextTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_model_too_long() {
        let t = ThinkingBlock::visible("ok").with_model("m".repeat(MAX_MODEL_CHARS + 1));
        assert!(matches!(
            t.validate(),
            Err(ThinkingBlockError::ModelTooLong { .. })
        ));
    }

    #[test]
    fn text_fallback_includes_reasoning_header_and_quoted_lines() {
        let t = ThinkingBlock::visible("first\nsecond");
        let out = t.to_text_fallback();
        assert!(out.starts_with("[reasoning]"));
        assert!(out.contains("> first"));
        assert!(out.contains("> second"));
    }

    #[test]
    fn text_fallback_includes_model_when_provided() {
        let t = ThinkingBlock::visible("ok").with_model("claude-opus-4-7");
        let out = t.to_text_fallback();
        assert!(out.starts_with("[reasoning: claude-opus-4-7]"));
    }

    #[test]
    fn text_fallback_renders_placeholder_for_redacted() {
        let t = ThinkingBlock::redacted("opaque-blob");
        let out = t.to_text_fallback();
        assert!(out.contains("(redacted reasoning)"));
        // Critical: the raw blob is NEVER emitted on the wire.
        assert!(!out.contains("opaque-blob"));
    }

    #[test]
    fn serde_roundtrip_full() {
        let t = ThinkingBlock::visible("a chain").with_model("kimi-k2.6");
        let j = serde_json::to_string(&t).unwrap();
        let back: ThinkingBlock = serde_json::from_str(&j).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn serde_skips_default_model_field() {
        let t = ThinkingBlock::visible("a chain");
        let j = serde_json::to_string(&t).unwrap();
        assert_eq!(j, r#"{"text":"a chain","redacted":false}"#);
    }

    #[test]
    fn error_display_messages_non_empty() {
        let cases = [
            ThinkingBlockError::EmptyTextWhenNotRedacted,
            ThinkingBlockError::TextTooLong {
                len: 9999,
                max: MAX_THINKING_CHARS,
            },
            ThinkingBlockError::ModelTooLong {
                len: 200,
                max: MAX_MODEL_CHARS,
            },
        ];
        for e in cases {
            assert!(!format!("{e}").is_empty());
        }
    }
}
