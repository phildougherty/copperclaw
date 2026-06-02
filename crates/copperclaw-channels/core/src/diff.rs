//! Canonical portable-diff schema shared by every channel adapter.
//!
//! A "diff card" is a structured snapshot of what changed when one of the
//! file-edit MCP tools (`edit_file`, `multi_edit`, `apply_patch`,
//! `write_file` overwriting an existing file) successfully writes a file.
//! The runner computes the diff from the pre-edit snapshot vs the
//! post-edit content (using the `similar` crate) and emits a
//! [`MessageKind::Diff`](copperclaw_types::MessageKind::Diff) row carrying
//! this payload; the host's delivery service routes it through the
//! adapter's [`crate::ChannelAdapter::deliver_diff`] hook so channels
//! with rich syntax-aware rendering (Telegram `MarkdownV2` ` ```diff ``` `
//! blocks, Slack Block Kit rich-text-preformatted, Discord embeds,
//! Google Chat cards v2, Matrix `m.notice` with `<pre><code>`) can
//! draw the diff with `+`/`-` gutters natively.
//!
//! Why structured-on-the-wire rather than unified-diff text:
//!
//! - The runner already knows the structured hunks (`similar` gives them
//!   to us); reserialising to unified-diff text and re-parsing in every
//!   renderer is wasted work.
//! - Each channel wants a different shape (Slack Block Kit blocks,
//!   Discord embeds with color, Telegram fenced code blocks); a
//!   structured payload lets each renderer pick its own primitive.
//! - `to_text_fallback` reconstitutes unified-diff trivially when a
//!   channel has no native renderer.
//!
//! # Field caps
//!
//! Tight caps so the payload stays under Discord's 6 KB embed limit and
//! Telegram's 4096-char `sendMessage` budget without per-channel
//! trimming:
//!
//! - `path` ≤ [`MAX_PATH_CHARS`].
//! - `hunks.len()` ≤ [`MAX_HUNKS`]; excess hunks set `truncated = true`.
//! - per-hunk `lines.len()` ≤ [`MAX_LINES_PER_HUNK`].
//! - per-`DiffLine::text` ≤ [`MAX_LINE_CHARS`]; over-cap lines get
//!   truncated with a trailing `…` glyph.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum characters in [`DiffCard::path`].
pub const MAX_PATH_CHARS: usize = 256;
/// Maximum characters in [`DiffCard::language`].
pub const MAX_LANGUAGE_CHARS: usize = 32;
/// Maximum number of [`DiffHunk`]s in a [`DiffCard`].
pub const MAX_HUNKS: usize = 8;
/// Maximum number of [`DiffLine`]s in a single [`DiffHunk`].
pub const MAX_LINES_PER_HUNK: usize = 60;
/// Maximum characters in a single [`DiffLine::text`].
pub const MAX_LINE_CHARS: usize = 500;
/// Hard size cutoff (in bytes) above which the runner skips computing a
/// diff and emits a [`BlobReplaced`] summary card instead. Mirrors the
/// design-doc cutoff for `write_file`-overwriting-a-large-blob.
pub const BLOB_DIFF_CUTOFF_BYTES: u64 = 256 * 1024;

/// Per-line classification for a single line inside a [`DiffHunk`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineKind {
    /// Unchanged line included for surrounding context.
    Context,
    /// Line present in the new content only.
    Add,
    /// Line present in the old content only.
    Remove,
}

impl DiffLineKind {
    /// Stable lowercase tag used by renderers that route on the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }

    /// Convenience: parse the lowercase tag back into the enum. Returns
    /// `None` for unknown strings.
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "context" => Self::Context,
            "add" => Self::Add,
            "remove" => Self::Remove,
            _ => return None,
        })
    }

    /// Unified-diff line prefix (` `, `+`, `-`). Used by
    /// [`DiffCard::to_text_fallback`].
    pub fn unified_prefix(self) -> char {
        match self {
            Self::Context => ' ',
            Self::Add => '+',
            Self::Remove => '-',
        }
    }
}

impl fmt::Display for DiffLineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line inside a [`DiffHunk`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffLine {
    /// Whether the line is context, an addition, or a removal.
    pub kind: DiffLineKind,
    /// The text of the line, without the leading `+`/`-`/` ` prefix and
    /// without a trailing newline. Truncated to [`MAX_LINE_CHARS`]
    /// characters by [`DiffCard::validate`] (over-cap lines get a
    /// trailing `…`).
    pub text: String,
}

/// One change-region in a [`DiffCard`]. Mirrors the unified-diff
/// `@@ -OLDSTART,OLDLEN +NEWSTART,NEWLEN @@` shape, plus the body lines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffHunk {
    /// 1-based starting line in the OLD file (matches unified-diff
    /// convention).
    pub old_start: u32,
    /// Number of lines from the OLD file consumed by this hunk
    /// (context + remove).
    pub old_lines: u32,
    /// 1-based starting line in the NEW file.
    pub new_start: u32,
    /// Number of lines from the NEW file consumed by this hunk
    /// (context + add).
    pub new_lines: u32,
    /// Body of the hunk in source order.
    pub lines: Vec<DiffLine>,
}

/// The canonical diff payload — emitted by the runner after a successful
/// file-edit write and rendered natively by every adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffCard {
    /// Path of the file that was edited. Truncated to [`MAX_PATH_CHARS`]
    /// by the runner before emit.
    pub path: String,
    /// Optional syntax-highlight hint (`rust`, `ts`, `python`, …).
    /// Renderers that support a language tag on their code block
    /// (Telegram `MarkdownV2` ` ```rust `, Matrix `class="language-rust"`)
    /// use this when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Hunks in source order. Empty when the file changed only in
    /// trailing whitespace (rare; emitted as a single empty card so
    /// the breadcrumb still has a sibling).
    pub hunks: Vec<DiffHunk>,
    /// Total `+` lines across all hunks.
    pub added: u32,
    /// Total `-` lines across all hunks.
    pub removed: u32,
    /// True when the diff was too large to render in full and hunks /
    /// lines were dropped to fit the schema caps. Renderers surface
    /// this with a footer marker so the user knows the displayed diff
    /// is incomplete.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// A minimal summary card emitted *instead* of a full [`DiffCard`] when
/// the target file would have been too large to diff safely (the
/// runner-side cutoff is [`BLOB_DIFF_CUTOFF_BYTES`]). The renderer
/// surfaces a one-line "replaced binary blob" notice instead of a
/// massive diff.
///
/// Routes through the same [`crate::ChannelAdapter::deliver_diff`] hook
/// as a regular diff card — the wire shape is just a
/// [`DiffCard`] with no hunks and a single synthesised body line that
/// summarises the byte-size delta. Channels render either uniformly,
/// avoiding a second trait method for a degenerate case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobReplaced {
    /// Path of the file that was overwritten.
    pub path: String,
    /// Size of the file BEFORE the overwrite (in bytes).
    pub before_bytes: u64,
    /// Size of the file AFTER the overwrite (in bytes).
    pub after_bytes: u64,
}

impl BlobReplaced {
    /// Convert to the [`DiffCard`] wire shape so the same delivery /
    /// adapter plumbing handles both cases. Synthesises a single
    /// context line summarising the byte delta and flips
    /// `truncated = true` so renderers know to surface a "binary /
    /// large blob — diff suppressed" note instead of pretending the
    /// hunkless card is a clean diff.
    pub fn into_card(self) -> DiffCard {
        let delta = i128::from(self.after_bytes) - i128::from(self.before_bytes);
        let sign = if delta >= 0 { "+" } else { "" };
        let summary = format!(
            "binary or large blob — diff suppressed (before {} B, after {} B, {sign}{} B)",
            self.before_bytes, self.after_bytes, delta
        );
        DiffCard {
            path: self.path,
            language: None,
            hunks: vec![DiffHunk {
                old_start: 0,
                old_lines: 0,
                new_start: 0,
                new_lines: 0,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    text: summary,
                }],
            }],
            added: 0,
            removed: 0,
            truncated: true,
        }
    }
}

/// Errors raised by [`DiffCard::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffCardError {
    /// `path` was empty after trim.
    EmptyPath,
    /// `path` exceeded [`MAX_PATH_CHARS`].
    PathTooLong { len: usize, max: usize },
    /// `language` exceeded [`MAX_LANGUAGE_CHARS`].
    LanguageTooLong { len: usize, max: usize },
    /// `hunks.len()` exceeded [`MAX_HUNKS`].
    TooManyHunks { len: usize, max: usize },
    /// Per-hunk `lines.len()` exceeded [`MAX_LINES_PER_HUNK`].
    HunkTooLong {
        index: usize,
        len: usize,
        max: usize,
    },
}

impl fmt::Display for DiffCardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => write!(f, "diff card path must be non-empty"),
            Self::PathTooLong { len, max } => write!(f, "path is {len} chars (max {max})"),
            Self::LanguageTooLong { len, max } => {
                write!(f, "language is {len} chars (max {max})")
            }
            Self::TooManyHunks { len, max } => {
                write!(f, "diff has {len} hunks (max {max})")
            }
            Self::HunkTooLong { index, len, max } => {
                write!(f, "hunk #{index} has {len} lines (max {max})")
            }
        }
    }
}

impl std::error::Error for DiffCardError {}

impl DiffCard {
    /// Apply every schema rule. Returns the first violation so callers
    /// can surface it directly to the runner / model.
    ///
    /// Note: line-text length is enforced by *truncation* inside
    /// [`Self::clamp`] (since most diff hits will have at least one
    /// long-line), not by returning an error from `validate`. The
    /// runner is expected to call `clamp` first.
    pub fn validate(&self) -> Result<(), DiffCardError> {
        if self.path.trim().is_empty() {
            return Err(DiffCardError::EmptyPath);
        }
        let plen = self.path.chars().count();
        if plen > MAX_PATH_CHARS {
            return Err(DiffCardError::PathTooLong {
                len: plen,
                max: MAX_PATH_CHARS,
            });
        }
        if let Some(lang) = &self.language {
            let llen = lang.chars().count();
            if llen > MAX_LANGUAGE_CHARS {
                return Err(DiffCardError::LanguageTooLong {
                    len: llen,
                    max: MAX_LANGUAGE_CHARS,
                });
            }
        }
        if self.hunks.len() > MAX_HUNKS {
            return Err(DiffCardError::TooManyHunks {
                len: self.hunks.len(),
                max: MAX_HUNKS,
            });
        }
        for (i, h) in self.hunks.iter().enumerate() {
            if h.lines.len() > MAX_LINES_PER_HUNK {
                return Err(DiffCardError::HunkTooLong {
                    index: i,
                    len: h.lines.len(),
                    max: MAX_LINES_PER_HUNK,
                });
            }
        }
        Ok(())
    }

    /// Best-effort cap enforcement: trims hunks past [`MAX_HUNKS`] and
    /// per-hunk lines past [`MAX_LINES_PER_HUNK`], truncating long
    /// individual lines to [`MAX_LINE_CHARS`] with a trailing `…`.
    /// Sets `truncated = true` when any cap kicked in. The runner
    /// calls this immediately before emit so `validate` always
    /// succeeds on the wire payload.
    pub fn clamp(&mut self) {
        let mut truncated = self.truncated;
        if self.hunks.len() > MAX_HUNKS {
            self.hunks.truncate(MAX_HUNKS);
            truncated = true;
        }
        for h in &mut self.hunks {
            if h.lines.len() > MAX_LINES_PER_HUNK {
                h.lines.truncate(MAX_LINES_PER_HUNK);
                truncated = true;
            }
            for line in &mut h.lines {
                if line.text.chars().count() > MAX_LINE_CHARS {
                    // Build the truncated form by walking chars to
                    // avoid splitting a multi-byte char in the middle.
                    let mut buf = String::with_capacity(MAX_LINE_CHARS + 1);
                    for c in line.text.chars().take(MAX_LINE_CHARS.saturating_sub(1)) {
                        buf.push(c);
                    }
                    buf.push('\u{2026}'); // …
                    line.text = buf;
                    truncated = true;
                }
            }
        }
        self.truncated = truncated;
    }

    /// Plain-text rendering used by the default
    /// [`crate::ChannelAdapter::deliver_diff`] fallback. Emits a
    /// standard unified-diff body: `--- a/<path>` / `+++ b/<path>`
    /// header, `@@ -old,len +new,len @@` per hunk, `+`/`-`/` ` per
    /// line, footer `(+N / -M)` (and `[truncated]` when caps fired).
    pub fn to_text_fallback(&self) -> String {
        let mut out = String::with_capacity(128);
        out.push_str("--- a/");
        out.push_str(&self.path);
        out.push('\n');
        out.push_str("+++ b/");
        out.push_str(&self.path);
        out.push('\n');
        for h in &self.hunks {
            out.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.old_start, h.old_lines, h.new_start, h.new_lines
            ));
            for line in &h.lines {
                out.push(line.kind.unified_prefix());
                out.push_str(&line.text);
                out.push('\n');
            }
        }
        // Footer with totals so the user gets a one-glance summary
        // even when the diff is long.
        out.push_str(&format!("(+{} / -{})", self.added, self.removed));
        if self.truncated {
            out.push_str(" [truncated]");
        }
        out
    }
}

// serde's `skip_serializing_if` invokes the predicate with `&T` even
// for `Copy` types; the by-reference signature is mandatory here, not
// a clippy-pleasing choice we get to make.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DiffCard {
        DiffCard {
            path: "src/lib.rs".into(),
            language: Some("rust".into()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 2,
                new_start: 1,
                new_lines: 2,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        text: "fn main() {".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Remove,
                        text: "    println!(\"old\");".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Add,
                        text: "    println!(\"new\");".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Context,
                        text: "}".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        }
    }

    #[test]
    fn validate_happy_path() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_path() {
        let mut c = sample();
        c.path = "   ".into();
        assert_eq!(c.validate().unwrap_err(), DiffCardError::EmptyPath);
    }

    #[test]
    fn validate_rejects_path_too_long() {
        let mut c = sample();
        c.path = "a".repeat(MAX_PATH_CHARS + 1);
        assert!(matches!(
            c.validate(),
            Err(DiffCardError::PathTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_too_many_hunks() {
        let mut c = sample();
        c.hunks = std::iter::repeat_with(|| c.hunks[0].clone())
            .take(MAX_HUNKS + 1)
            .collect();
        assert!(matches!(
            c.validate(),
            Err(DiffCardError::TooManyHunks { .. })
        ));
    }

    #[test]
    fn validate_rejects_hunk_too_long() {
        let mut c = sample();
        let mut hunk = c.hunks[0].clone();
        hunk.lines = std::iter::repeat_with(|| DiffLine {
            kind: DiffLineKind::Context,
            text: "x".into(),
        })
        .take(MAX_LINES_PER_HUNK + 1)
        .collect();
        c.hunks = vec![hunk];
        assert!(matches!(
            c.validate(),
            Err(DiffCardError::HunkTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_language_too_long() {
        let mut c = sample();
        c.language = Some("x".repeat(MAX_LANGUAGE_CHARS + 1));
        assert!(matches!(
            c.validate(),
            Err(DiffCardError::LanguageTooLong { .. })
        ));
    }

    #[test]
    fn clamp_truncates_hunks_past_cap() {
        let mut c = sample();
        c.hunks = std::iter::repeat_with(|| c.hunks[0].clone())
            .take(MAX_HUNKS + 3)
            .collect();
        c.clamp();
        assert_eq!(c.hunks.len(), MAX_HUNKS);
        assert!(c.truncated);
        c.validate().unwrap();
    }

    #[test]
    fn clamp_truncates_long_lines_with_ellipsis() {
        let mut c = sample();
        c.hunks[0].lines.push(DiffLine {
            kind: DiffLineKind::Add,
            text: "x".repeat(MAX_LINE_CHARS + 50),
        });
        c.clamp();
        let last = c.hunks[0].lines.last().unwrap();
        assert_eq!(last.text.chars().count(), MAX_LINE_CHARS);
        assert!(last.text.ends_with('\u{2026}'));
        assert!(c.truncated);
    }

    #[test]
    fn clamp_truncates_lines_per_hunk_past_cap() {
        let mut c = sample();
        c.hunks[0].lines = std::iter::repeat_with(|| DiffLine {
            kind: DiffLineKind::Context,
            text: "x".into(),
        })
        .take(MAX_LINES_PER_HUNK + 5)
        .collect();
        c.clamp();
        assert_eq!(c.hunks[0].lines.len(), MAX_LINES_PER_HUNK);
        assert!(c.truncated);
    }

    #[test]
    fn to_text_fallback_includes_header_hunk_and_footer() {
        let out = sample().to_text_fallback();
        assert!(out.contains("--- a/src/lib.rs"));
        assert!(out.contains("+++ b/src/lib.rs"));
        assert!(out.contains("@@ -1,2 +1,2 @@"));
        assert!(out.contains("-    println!(\"old\");"));
        assert!(out.contains("+    println!(\"new\");"));
        assert!(out.contains("(+1 / -1)"));
        assert!(!out.contains("[truncated]"));
    }

    #[test]
    fn to_text_fallback_marks_truncated_in_footer() {
        let mut c = sample();
        c.truncated = true;
        let out = c.to_text_fallback();
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn diff_line_kind_round_trips_through_str() {
        for k in [
            DiffLineKind::Context,
            DiffLineKind::Add,
            DiffLineKind::Remove,
        ] {
            assert_eq!(DiffLineKind::parse_str(k.as_str()), Some(k));
        }
        assert_eq!(DiffLineKind::parse_str("nope"), None);
    }

    #[test]
    fn diff_line_kind_unified_prefix_matches_format() {
        assert_eq!(DiffLineKind::Context.unified_prefix(), ' ');
        assert_eq!(DiffLineKind::Add.unified_prefix(), '+');
        assert_eq!(DiffLineKind::Remove.unified_prefix(), '-');
    }

    #[test]
    fn diff_card_serde_roundtrip() {
        let c = sample();
        let j = serde_json::to_string(&c).unwrap();
        let back: DiffCard = serde_json::from_str(&j).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn diff_card_serde_skips_truncated_when_false() {
        let c = sample();
        let j = serde_json::to_string(&c).unwrap();
        assert!(!j.contains("truncated"));
    }

    #[test]
    fn blob_replaced_into_card_is_single_context_summary_with_truncated_flag() {
        let card = BlobReplaced {
            path: "data/blob.bin".into(),
            before_bytes: 1024 * 1024,
            after_bytes: 2 * 1024 * 1024,
        }
        .into_card();
        assert_eq!(card.path, "data/blob.bin");
        assert!(card.truncated);
        assert_eq!(card.hunks.len(), 1);
        assert_eq!(card.hunks[0].lines.len(), 1);
        let line = &card.hunks[0].lines[0];
        assert_eq!(line.kind, DiffLineKind::Context);
        assert!(line.text.contains("diff suppressed"));
        assert!(line.text.contains("1048576"));
        assert!(line.text.contains("2097152"));
        assert!(line.text.contains("+1048576"));
        card.validate().unwrap();
    }

    #[test]
    fn blob_replaced_negative_delta_renders_without_extra_sign() {
        let card = BlobReplaced {
            path: "data/blob.bin".into(),
            before_bytes: 4096,
            after_bytes: 100,
        }
        .into_card();
        // delta is -3996; we don't prepend an extra '+' but the natural
        // formatter already emits the leading '-'.
        let summary = &card.hunks[0].lines[0].text;
        assert!(summary.contains("-3996"), "got: {summary}");
        // No double sign.
        assert!(!summary.contains("+-"));
    }

    #[test]
    fn diff_card_error_display_messages_are_non_empty() {
        let cases = [
            DiffCardError::EmptyPath,
            DiffCardError::PathTooLong { len: 99, max: 1 },
            DiffCardError::LanguageTooLong { len: 99, max: 1 },
            DiffCardError::TooManyHunks { len: 99, max: 1 },
            DiffCardError::HunkTooLong {
                index: 0,
                len: 99,
                max: 1,
            },
        ];
        for e in cases {
            assert!(!format!("{e}").is_empty());
        }
    }

    #[test]
    fn diff_line_kind_display_matches_as_str() {
        for k in [
            DiffLineKind::Context,
            DiffLineKind::Add,
            DiffLineKind::Remove,
        ] {
            assert_eq!(format!("{k}"), k.as_str());
        }
    }
}
