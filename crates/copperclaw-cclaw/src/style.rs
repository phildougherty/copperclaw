//! Terminal color / styling for `cclaw`, gated on TTY + `NO_COLOR` + a
//! `--no-color` flag.
//!
//! The decision to emit ANSI codes is made **once** (in [`crate::run_cli`])
//! and carried through the render path as a [`Palette`]. Machine-readable
//! output (`--json`) never gets a colored [`Palette`] — JSON stays
//! byte-identical regardless of terminal.
//!
//! The predicate [`should_colorize`] is a pure function of three inputs so
//! it can be unit-tested without touching a real terminal.

use anstyle::{AnsiColor, Style};

/// Decide whether to emit ANSI color, given the three inputs that gate it.
///
/// Color is on only when **all** of these hold:
/// * the `--no-color` flag was not passed (`no_color_flag == false`),
/// * the `NO_COLOR` environment variable is absent or empty
///   (`no_color_env == false`; see <https://no-color.org>),
/// * stdout is a terminal (`is_terminal == true`).
#[must_use]
pub fn should_colorize(no_color_flag: bool, no_color_env: bool, is_terminal: bool) -> bool {
    !no_color_flag && !no_color_env && is_terminal
}

/// Resolve the color decision against the real process environment: the
/// `NO_COLOR` variable (present and non-empty disables color, per the
/// no-color.org convention) and whether stdout is a TTY.
///
/// `no_color_flag` comes from the parsed `--no-color` CLI flag.
///
/// Under `cfg(test)` (this crate's unit tests) the terminal probe is
/// forced to `false` so in-process `run_cli` tests are deterministic
/// regardless of whether `cargo test` runs attached to a TTY or with
/// `NO_COLOR` exported. The real binary path is exercised by the
/// integration test in `tests/no_ansi_when_piped.rs`.
#[must_use]
pub fn color_enabled(no_color_flag: bool) -> bool {
    use std::io::IsTerminal as _;
    let no_color_env = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let is_terminal = !cfg!(test) && std::io::stdout().is_terminal();
    should_colorize(no_color_flag, no_color_env, is_terminal)
}

/// A resolved styling decision plus the color primitives `cclaw` uses.
///
/// When `color` is false every method returns the input text verbatim, so
/// call sites can style unconditionally and rely on the palette to no-op
/// on a pipe.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    color: bool,
}

impl Palette {
    /// Construct a palette with an explicit color decision.
    #[must_use]
    pub fn new(color: bool) -> Self {
        Self { color }
    }

    /// A palette that never emits ANSI codes. Used as the default for the
    /// bare [`crate::render`] helper so existing callers stay monochrome.
    #[must_use]
    pub fn plain() -> Self {
        Self { color: false }
    }

    /// Whether this palette emits color.
    #[must_use]
    pub fn is_colored(self) -> bool {
        self.color
    }

    /// Wrap `text` in `style`'s SGR sequence + reset, or return it verbatim
    /// when color is disabled.
    fn paint(self, style: Style, text: &str) -> String {
        if self.color {
            format!("{}{text}{}", style.render(), style.render_reset())
        } else {
            text.to_string()
        }
    }

    /// Doctor OK level — green.
    #[must_use]
    pub fn ok(self, text: &str) -> String {
        self.paint(fg(AnsiColor::Green), text)
    }

    /// Doctor WARN level — yellow.
    #[must_use]
    pub fn warn(self, text: &str) -> String {
        self.paint(fg(AnsiColor::Yellow), text)
    }

    /// Doctor FAIL level / remote errors — red.
    #[must_use]
    pub fn fail(self, text: &str) -> String {
        self.paint(fg(AnsiColor::Red), text)
    }

    /// Remote `error:` lines — red. Alias of [`Palette::fail`] for call-site
    /// clarity.
    #[must_use]
    pub fn error(self, text: &str) -> String {
        self.paint(fg(AnsiColor::Red), text)
    }

    /// `fix:` hint lines — cyan.
    #[must_use]
    pub fn fix(self, text: &str) -> String {
        self.paint(fg(AnsiColor::Cyan), text)
    }

    /// Table headers and dashboard section headers — bold.
    #[must_use]
    pub fn header(self, text: &str) -> String {
        self.paint(Style::new().bold(), text)
    }
}

/// Foreground-color style helper.
fn fg(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(color.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorize_requires_all_three_conditions() {
        // The happy path: no flag, no env, is a tty.
        assert!(should_colorize(false, false, true));
    }

    #[test]
    fn no_color_flag_disables() {
        assert!(!should_colorize(true, false, true));
    }

    #[test]
    fn no_color_env_disables() {
        assert!(!should_colorize(false, true, true));
    }

    #[test]
    fn non_terminal_disables() {
        assert!(!should_colorize(false, false, false));
    }

    #[test]
    fn every_disabling_condition_wins_over_terminal() {
        // Only a bare terminal with neither opt-out enables color.
        for flag in [false, true] {
            for env in [false, true] {
                let expected = !flag && !env;
                assert_eq!(should_colorize(flag, env, true), expected);
                // Never colored when not a terminal, regardless of the rest.
                assert!(!should_colorize(flag, env, false));
            }
        }
    }

    #[test]
    fn plain_palette_never_emits_ansi() {
        let p = Palette::plain();
        assert_eq!(p.ok("OK"), "OK");
        assert_eq!(p.warn("WARN"), "WARN");
        assert_eq!(p.fail("FAIL"), "FAIL");
        assert_eq!(p.fix("fix: x"), "fix: x");
        assert_eq!(p.header("NAME"), "NAME");
        assert!(!p.is_colored());
    }

    #[test]
    fn colored_palette_wraps_text_and_resets() {
        let p = Palette::new(true);
        let out = p.ok("OK");
        // The original text survives.
        assert!(out.contains("OK"));
        // An ANSI escape is present and the string is reset afterwards.
        assert!(out.starts_with('\u{1b}'));
        assert!(out.ends_with("\u{1b}[0m"));
        assert!(p.is_colored());
    }

    #[test]
    fn colored_levels_use_distinct_sequences() {
        let p = Palette::new(true);
        let ok = p.ok("x");
        let warn = p.warn("x");
        let fail = p.fail("x");
        assert_ne!(ok, warn);
        assert_ne!(warn, fail);
        assert_ne!(ok, fail);
    }
}
