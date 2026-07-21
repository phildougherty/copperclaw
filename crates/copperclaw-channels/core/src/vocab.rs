//! Transcript vocabulary — the single source of truth for the glyphs
//! and layout strings the transcript renderers emit (M22 A1).
//!
//! A flat static table of `&'static str` — no trait, no dyn dispatch —
//! because the in-container runner (`copperclaw-runner`) is a separate
//! process from the host and needs the same strings without holding a
//! live adapter handle, exactly like [`crate::capabilities`]. Renderers
//! look their binding up once via [`for_channel`] and read fields; the
//! per-status helpers ([`TodoGlyphs::get`], [`RailGlyphs::for_status`])
//! exist so adopting call sites (M22 A2/A3/D) become one-line lookups.
//!
//! # Vocab is the source of truth (deliberately not `capabilities.rs`)
//!
//! [`crate::capabilities`] and this module have opposite epistemics, so
//! they stay separate on purpose. `capabilities` is a *mirror* of the
//! adapter trait impls — its failure mode is silent drift, guarded from
//! outside by the host-delivery drift tests. `vocab` is the *source of
//! truth* — once call sites adopt it, the failure mode of a bad edit is
//! a compile error (or a byte-identity test), so it needs no drift test;
//! it gets an adoption gate instead (M22 A4,
//! `host-delivery/tests/vocab_adoption.rs`).
//!
//! # Non-goal: reaction emoji are PROTOCOL VALUES
//!
//! The reaction emoji in [`crate::reaction`] (thumbs-up / check-mark /
//! eyes / cross-mark / thumbs-down) and the reaction-emoji defaults
//! documented in `discord/src/config.rs:30` and `telegram/src/config.rs:59`
//! are *protocol values* — inbound steering signals matched against
//! platform payloads and outbound `setMessageReaction` bodies. They are
//! not presentation and must NEVER route through vocab; changing them
//! changes wire behaviour, not looks.
//!
//! # Bindings
//!
//! - [`ASCII`] — byte-identical to what every existing renderer emits
//!   today (see the field comments for the exact source of each
//!   literal). Existing call sites adopting vocab must produce zero
//!   output diff.
//! - [`RAIL`] — Claude Code's transcript markers. These are geometric /
//!   box-drawing codepoints (U+23FA, U+23BF, U+25B0, ...), explicitly
//!   NOT emoji, per the plan's binding decisions.
//!
//! Channel map: `cli` and `telegram` bind [`RAIL`] (the two surfaces
//! that gain the new transcript renderers); every other / unknown
//! channel binds [`ASCII`]. The telegram binding may be flipped back to
//! ASCII after real-device verification (iOS Telegram has a history of
//! promoting symbol codepoints to color emoji) — a one-line change in
//! [`for_channel`].

use crate::breadcrumb::BreadcrumbStatus;
use crate::todo_list::TodoItemStatus;

/// Markers for the tool-step rail: the per-step lead bullet, the result
/// leader on the line under it, and the three lifecycle status markers.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RailGlyphs {
    /// Lead marker on a tool-step line (`⏺ shell(cargo check)`).
    pub step_bullet: &'static str,
    /// Leader on the result line under a step (`⎿ passed in 3.1s`).
    pub result_leader: &'static str,
    /// Status marker for a tool in flight ([`BreadcrumbStatus::Running`]).
    pub active: &'static str,
    /// Status marker for a tool that succeeded ([`BreadcrumbStatus::Done`]).
    pub done: &'static str,
    /// Status marker for a tool that failed ([`BreadcrumbStatus::Failed`]).
    pub failed: &'static str,
}

impl RailGlyphs {
    /// Status marker for a [`BreadcrumbStatus`] — the vocab-driven
    /// replacement for the telegram adapter's `breadcrumb_glyph`.
    #[must_use]
    pub const fn for_status(&self, status: BreadcrumbStatus) -> &'static str {
        match status {
            BreadcrumbStatus::Running => self.active,
            BreadcrumbStatus::Done => self.done,
            BreadcrumbStatus::Failed => self.failed,
        }
    }
}

/// Checkbox markers for the four [`TodoItemStatus`] states.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TodoGlyphs {
    /// [`TodoItemStatus::Pending`] — not started.
    pub pending: &'static str,
    /// [`TodoItemStatus::InProgress`] — being worked on.
    pub in_progress: &'static str,
    /// [`TodoItemStatus::Blocked`] — stuck, reason alongside.
    pub blocked: &'static str,
    /// [`TodoItemStatus::Completed`] — finished.
    pub completed: &'static str,
}

impl TodoGlyphs {
    /// Glyph for a [`TodoItemStatus`] — the vocab-driven body for
    /// `TodoItemStatus::glyph()` (M22 A2 adoption).
    #[must_use]
    pub const fn get(&self, status: TodoItemStatus) -> &'static str {
        match status {
            TodoItemStatus::Pending => self.pending,
            TodoItemStatus::InProgress => self.in_progress,
            TodoItemStatus::Blocked => self.blocked,
            TodoItemStatus::Completed => self.completed,
        }
    }
}

/// Inline layout strings: the field separator, the truncation marker,
/// and the progress-bar cells.
///
/// `ellipsis` is the truncation marker, and every renderer that cuts
/// to a char budget now routes through [`truncate_chars`]: telegram's
/// `truncate_chars` wrapper and the LINE renderer look the marker up
/// via [`for_channel`], discord's inline embed truncations call
/// [`truncate_chars`] directly, and the runner HUD's `cap_chars`
/// (`copperclaw-runner/src/run/hud.rs`) delegates with the ASCII
/// `"..."` (the HUD line is plain text on every channel).
///
/// `separator` joins fields on one status/summary line — telegram's
/// `render_breadcrumb_html` / `render_activity_html` read it from
/// their binding; the HUD's `" | "` join matches the ASCII binding's
/// value. Deliberately NOT a separator: `core/src/todo_list.rs`
/// (`to_text_fallback`) keeps its `" — "` blocked-reason join
/// hardcoded — it is a reason join, not a status-line field separator
/// (see the comment there).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Joins fields on one status/summary line.
    pub separator: &'static str,
    /// Appended when a string is truncated to a char budget.
    pub ellipsis: &'static str,
    /// One filled cell of the todo progress bar (M22 D3).
    pub progress_filled: &'static str,
    /// One empty cell of the todo progress bar.
    pub progress_empty: &'static str,
}

/// One complete per-surface vocabulary: rail markers, todo checkboxes,
/// and layout strings. Renderers hold `&'static Vocabulary` and never
/// mix fields across bindings.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Vocabulary {
    /// Binding name (`"ascii"` / `"rail"`), for logs and tests.
    pub name: &'static str,
    /// Tool-step rail markers.
    pub rail: RailGlyphs,
    /// Todo checkbox markers.
    pub todo: TodoGlyphs,
    /// Separator / ellipsis / progress cells.
    pub layout: Layout,
}

/// Today's exact strings — byte-identical to what the existing renderers
/// emit, verified against the literals in-tree (sources on each field).
/// Note the pre-existing collision this table preserves faithfully:
/// `[x]` means *completed* in the todo checklist but *failed* on the
/// breadcrumb rail, and `[~]` is both *in progress* and *running*.
pub static ASCII: Vocabulary = Vocabulary {
    name: "ascii",
    rail: RailGlyphs {
        // step_bullet / result_leader have no legacy counterpart (the
        // ASCII transcript rail is new); plain ASCII, no byte-identity
        // constraint.
        step_bullet: "*",
        result_leader: "->",
        // `breadcrumb_glyph` in telegram/src/adapter.rs:797-803.
        active: "[~]",
        done: "[ok]",
        failed: "[x]",
    },
    todo: TodoGlyphs {
        // `TodoItemStatus::glyph()` in core/src/todo_list.rs:99-106.
        pending: "[ ]",
        in_progress: "[~]",
        blocked: "[!]",
        completed: "[x]",
    },
    layout: Layout {
        // hud.rs `running_frame` (inconsistency 1 above).
        separator: " | ",
        // hud.rs `cap_chars` (inconsistency 4 above).
        ellipsis: "...",
        // ASCII progress cells per the M22 plan (D3).
        progress_filled: "#",
        progress_empty: ".",
    },
};

/// Claude Code's transcript markers: geometric / box-drawing codepoints
/// (NOT emoji). Used only by the new transcript renderers; bound to
/// `cli` and `telegram` in [`for_channel`].
pub static RAIL: Vocabulary = Vocabulary {
    name: "rail",
    rail: RailGlyphs {
        // U+23FA BLACK CIRCLE FOR RECORD — Claude Code's step bullet.
        step_bullet: "\u{23FA}",
        // U+23BF BOTTOM LEFT CROP — the result leader.
        result_leader: "\u{23BF}",
        // U+25CB WHITE CIRCLE — hollow = still in flight.
        active: "\u{25CB}",
        // Completed steps carry the filled bullet (status shown by the
        // result line / client colour, as in Claude Code).
        done: "\u{23FA}",
        // U+00D7 MULTIPLICATION SIGN — geometric, no emoji presentation.
        failed: "\u{00D7}",
    },
    todo: TodoGlyphs {
        // Checkboxes stay ASCII even on the rail binding — they are
        // legible everywhere and match Claude Code's own checklists.
        pending: "[ ]",
        in_progress: "[~]",
        blocked: "[!]",
        completed: "[x]",
    },
    layout: Layout {
        // Interpunct separator, matching the target status line
        // ("1:47 · 2/5 · 6 tools · ...") and telegram's existing joins.
        separator: " \u{00B7} ",
        // U+2026 HORIZONTAL ELLIPSIS, matching telegram `truncate_chars`.
        ellipsis: "\u{2026}",
        // U+25B0 / U+25B1 BLACK/WHITE PARALLELOGRAM progress cells.
        progress_filled: "\u{25B0}",
        progress_empty: "\u{25B1}",
    },
};

/// The vocabulary for a channel type. `cli` and `telegram` — the two
/// surfaces gaining the new transcript renderers — bind [`RAIL`]; every
/// other (and unknown) channel binds [`ASCII`]. If real-device checks
/// show iOS Telegram promoting the rail codepoints to color emoji, flip
/// the `"telegram"` arm to `&ASCII` here — nothing else changes.
#[must_use]
pub fn for_channel(channel_type: &str) -> &'static Vocabulary {
    match channel_type {
        "cli" | "telegram" => &RAIL,
        _ => &ASCII,
    }
}

/// Truncate `s` to at most `max` characters, appending `ellipsis` when it
/// had to cut. The ellipsis counts toward `max` — with the ASCII binding's
/// three-char `"..."` as much as the rail binding's one-char `"…"` — so the
/// result never exceeds a platform's field cap. Degenerate budgets keep
/// that guarantee: when a cut is needed but `max` is at or below the
/// ellipsis's own char count, the result is the ellipsis itself truncated
/// to `max` chars (so `max == 0` yields the empty string).
#[must_use]
pub fn truncate_chars(s: &str, max: usize, ellipsis: &str) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let ellipsis_chars = ellipsis.chars().count();
    if max <= ellipsis_chars {
        return ellipsis.chars().take(max).collect();
    }
    let mut out: String = s.chars().take(max - ellipsis_chars).collect();
    out.push_str(ellipsis);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_counts_the_ellipsis_toward_max() {
        // No cut: returned verbatim, no ellipsis.
        assert_eq!(truncate_chars("short", 10, "…"), "short");
        assert_eq!(truncate_chars("exact", 5, "…"), "exact");
        // One-char ellipsis (rail binding): result is exactly `max` chars.
        assert_eq!(truncate_chars("abcdef", 5, "…"), "abcd…");
        // Three-char ellipsis (ASCII binding): still never exceeds `max`.
        assert_eq!(truncate_chars("abcdef", 5, "..."), "ab...");
        // Multibyte input cuts on char boundaries, not bytes.
        assert_eq!(truncate_chars("héllo wörld", 6, "…"), "héllo…");
    }

    #[test]
    fn truncate_chars_never_exceeds_max_on_degenerate_budgets() {
        // max below the ellipsis length: the ellipsis itself is cut to
        // `max` chars rather than blowing the cap.
        assert_eq!(truncate_chars("abcdef", 2, "..."), "..");
        // max exactly the ellipsis length: the whole ellipsis, nothing
        // of the input.
        assert_eq!(truncate_chars("abcdef", 3, "..."), "...");
        assert_eq!(truncate_chars("abcdef", 1, "…"), "…");
        // max of zero yields the empty string.
        assert_eq!(truncate_chars("abcdef", 0, "..."), "");
        // Input already within a tiny budget is still returned verbatim.
        assert_eq!(truncate_chars("ab", 2, "..."), "ab");
    }

    #[test]
    fn for_channel_binds_cli_and_telegram_to_rail() {
        assert!(std::ptr::eq(for_channel("cli"), &RAIL));
        assert!(std::ptr::eq(for_channel("telegram"), &RAIL));
        assert_eq!(for_channel("cli").name, "rail");
        assert_eq!(for_channel("telegram").name, "rail");
    }

    #[test]
    fn for_channel_defaults_everything_else_to_ascii() {
        for ct in [
            "slack",
            "discord",
            "matrix",
            "webex",
            "signal",
            "mattermost",
            "teams",
            "email",
            "webhooks",
            "github",
            "whatsapp-cloud",
            "unknown-new-channel",
            "",
        ] {
            assert!(std::ptr::eq(for_channel(ct), &ASCII), "{ct} binds ASCII");
            assert_eq!(for_channel(ct).name, "ascii");
        }
    }

    #[test]
    fn ascii_todo_glyphs_are_byte_identical_to_todays_literals() {
        // Hardcoded expected values, NOT read through the vocab consts —
        // these pin `TodoItemStatus::glyph()` (todo_list.rs:99-106).
        assert_eq!(ASCII.todo.pending, "[ ]");
        assert_eq!(ASCII.todo.in_progress, "[~]");
        assert_eq!(ASCII.todo.blocked, "[!]");
        assert_eq!(ASCII.todo.completed, "[x]");
    }

    #[test]
    fn ascii_todo_glyphs_match_todo_item_status_glyph() {
        // Cross-check against the current in-tree authority so A2's
        // adoption (glyph() delegating to vocab) is a proven no-op.
        for status in [
            TodoItemStatus::Pending,
            TodoItemStatus::InProgress,
            TodoItemStatus::Blocked,
            TodoItemStatus::Completed,
        ] {
            assert_eq!(ASCII.todo.get(status), status.glyph(), "{status}");
        }
    }

    #[test]
    fn ascii_rail_markers_are_byte_identical_to_breadcrumb_glyphs() {
        // Hardcoded expected values pinning `breadcrumb_glyph`
        // (telegram/src/adapter.rs:797-803).
        assert_eq!(ASCII.rail.active, "[~]");
        assert_eq!(ASCII.rail.done, "[ok]");
        assert_eq!(ASCII.rail.failed, "[x]");
    }

    #[test]
    fn rail_for_status_maps_breadcrumb_lifecycle() {
        assert_eq!(ASCII.rail.for_status(BreadcrumbStatus::Running), "[~]");
        assert_eq!(ASCII.rail.for_status(BreadcrumbStatus::Done), "[ok]");
        assert_eq!(ASCII.rail.for_status(BreadcrumbStatus::Failed), "[x]");
        assert_eq!(
            RAIL.rail.for_status(BreadcrumbStatus::Running),
            RAIL.rail.active
        );
        assert_eq!(RAIL.rail.for_status(BreadcrumbStatus::Done), RAIL.rail.done);
        assert_eq!(
            RAIL.rail.for_status(BreadcrumbStatus::Failed),
            RAIL.rail.failed
        );
    }

    #[test]
    fn ascii_layout_matches_todays_separators_and_ellipsis() {
        // hud.rs running_frame joiner and cap_chars marker.
        assert_eq!(ASCII.layout.separator, " | ");
        assert_eq!(ASCII.layout.ellipsis, "...");
        assert_eq!(ASCII.layout.progress_filled, "#");
        assert_eq!(ASCII.layout.progress_empty, ".");
    }

    #[test]
    fn rail_markers_are_the_expected_codepoints() {
        // Spelled as \u escapes so the expectation is unambiguous about
        // exact codepoints (not lookalikes).
        assert_eq!(RAIL.rail.step_bullet, "\u{23FA}");
        assert_eq!(RAIL.rail.result_leader, "\u{23BF}");
        assert_eq!(RAIL.layout.progress_filled, "\u{25B0}");
        assert_eq!(RAIL.layout.progress_empty, "\u{25B1}");
        assert_eq!(RAIL.layout.ellipsis, "\u{2026}");
        assert_eq!(RAIL.layout.separator, " \u{00B7} ");
    }

    #[test]
    fn rail_status_markers_are_distinct() {
        assert_ne!(RAIL.rail.active, RAIL.rail.done);
        assert_ne!(RAIL.rail.active, RAIL.rail.failed);
        assert_ne!(RAIL.rail.done, RAIL.rail.failed);
    }

    #[test]
    fn rail_todo_glyphs_stay_ascii() {
        // The rail binding keeps ASCII checkboxes; only the rail markers
        // and layout differ between bindings.
        assert_eq!(RAIL.todo, ASCII.todo);
    }

    #[test]
    fn no_marker_is_an_emoji_presentation_codepoint() {
        // The project's no-emoji rule: every marker must be ASCII or a
        // geometric/box-drawing/punctuation codepoint. Assert nothing
        // sits in the emoji-heavy blocks (Misc Symbols & Pictographs,
        // Emoticons, Transport, Supplemental Symbols) or carries a
        // variation selector.
        let all = [
            ASCII.rail.step_bullet,
            ASCII.rail.result_leader,
            ASCII.rail.active,
            ASCII.rail.done,
            ASCII.rail.failed,
            RAIL.rail.step_bullet,
            RAIL.rail.result_leader,
            RAIL.rail.active,
            RAIL.rail.done,
            RAIL.rail.failed,
            ASCII.layout.progress_filled,
            ASCII.layout.progress_empty,
            RAIL.layout.progress_filled,
            RAIL.layout.progress_empty,
        ];
        for s in all {
            for c in s.chars() {
                let cp = c as u32;
                assert!(
                    !(0x1F000..=0x1FAFF).contains(&cp) && !(0x2600..=0x27BF).contains(&cp),
                    "{s:?} contains emoji-block codepoint U+{cp:04X}"
                );
                assert!(
                    cp != 0xFE0F && cp != 0xFE0E,
                    "{s:?} carries a variation selector"
                );
            }
        }
    }
}
