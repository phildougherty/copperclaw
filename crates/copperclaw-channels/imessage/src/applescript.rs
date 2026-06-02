//! Escaping helpers for AppleScript string literals.
//!
//! AppleScript string literals are double-quoted and use backslash-style
//! escapes — but only for two characters: `\"` and `\\`. Newlines, tabs,
//! carriage returns and other control characters do **not** have portable
//! `\n` / `\t` forms in AppleScript: the language treats those as syntax,
//! and the canonical workaround is to splice them in via concatenation
//! (`"first" & linefeed & "second"`). We sidestep that ceremony by
//! emitting the raw byte directly inside the quoted string — AppleScript
//! accepts literal newlines in `"..."` and round-trips them through
//! `osascript -e` faithfully.
//!
//! Things this module does **not** allow:
//!
//! - Null bytes (`\0`). AppleScript treats `\0` as a string terminator on
//!   some macOS releases; better to reject up front.
//! - Other C0 control characters except `\n`, `\r`, and `\t`. Anything
//!   that's not whitespace and below `0x20` is rejected — they're either
//!   noise from a broken upstream or actively dangerous (e.g. `\x1b`
//!   escape sequences could be mistaken for terminal control).
//!
//! Unicode (including emoji) is allowed through unchanged; AppleScript
//! strings are UTF-16 on disk but the source form is just text.

use thiserror::Error;

/// Errors returned by [`applescript_escape`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AppleScriptEscapeError {
    /// The input contained a null byte. Null bytes are not safe inside
    /// AppleScript strings.
    #[error("applescript: null byte in input")]
    NullByte,
    /// The input contained a C0 control character other than `\n`, `\r`, or
    /// `\t`.
    #[error("applescript: control char {0:#04x} in input")]
    ControlChar(u8),
}

/// Escape `s` so it can be embedded verbatim inside an AppleScript
/// double-quoted string literal.
///
/// The returned string does **not** include the surrounding quotes — the
/// caller wraps it in `"…"`. This keeps the helper composable with the
/// formatter machinery that builds full AppleScript documents.
///
/// # Errors
///
/// Returns [`AppleScriptEscapeError::NullByte`] if `s` contains a `\0`, and
/// [`AppleScriptEscapeError::ControlChar`] if `s` contains a C0 control
/// other than `\n` / `\r` / `\t`.
pub fn applescript_escape(s: &str) -> Result<String, AppleScriptEscapeError> {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\0' => return Err(AppleScriptEscapeError::NullByte),
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            // Permitted whitespace.
            '\n' | '\r' | '\t' => out.push(ch),
            // Everything else below U+0020 is rejected.
            c if (c as u32) < 0x20 => {
                return Err(AppleScriptEscapeError::ControlChar(c as u8));
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Convenience wrapper: escape `s` and return the surrounding quotes too.
///
/// `quote("foo")` returns `"\"foo\""`. Same errors as
/// [`applescript_escape`].
pub fn quote(s: &str) -> Result<String, AppleScriptEscapeError> {
    let escaped = applescript_escape(s)?;
    Ok(format!("\"{escaped}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_escapes_to_empty() {
        assert_eq!(applescript_escape("").unwrap(), "");
    }

    #[test]
    fn ascii_no_escapes_needed() {
        assert_eq!(applescript_escape("hello world").unwrap(), "hello world");
    }

    #[test]
    fn single_double_quote_is_escaped() {
        assert_eq!(applescript_escape("\"").unwrap(), "\\\"");
    }

    #[test]
    fn multiple_double_quotes_each_escaped() {
        assert_eq!(applescript_escape("a\"b\"c").unwrap(), "a\\\"b\\\"c",);
    }

    #[test]
    fn backslash_is_escaped_to_double_backslash() {
        assert_eq!(applescript_escape("\\").unwrap(), "\\\\");
    }

    #[test]
    fn backslash_n_is_two_chars_not_a_newline() {
        // Literal "\n" in the input — backslash followed by 'n'. We escape
        // the backslash, leave the 'n' alone.
        assert_eq!(applescript_escape("\\n").unwrap(), "\\\\n");
    }

    #[test]
    fn newline_passes_through_unchanged() {
        // Real newline. AppleScript strings tolerate raw newlines.
        let r = applescript_escape("a\nb").unwrap();
        assert_eq!(r, "a\nb");
        assert!(r.contains('\n'));
    }

    #[test]
    fn carriage_return_passes_through() {
        assert_eq!(applescript_escape("a\rb").unwrap(), "a\rb");
    }

    #[test]
    fn tab_passes_through() {
        assert_eq!(applescript_escape("a\tb").unwrap(), "a\tb");
    }

    #[test]
    fn mixed_escapes_in_one_string() {
        let s = "she said \"hello\\world\nbye\"";
        let r = applescript_escape(s).unwrap();
        assert_eq!(r, "she said \\\"hello\\\\world\nbye\\\"");
    }

    #[test]
    fn null_byte_is_rejected() {
        let err = applescript_escape("hello\0world").unwrap_err();
        assert_eq!(err, AppleScriptEscapeError::NullByte);
    }

    #[test]
    fn null_byte_at_start_is_rejected() {
        let err = applescript_escape("\0").unwrap_err();
        assert_eq!(err, AppleScriptEscapeError::NullByte);
    }

    #[test]
    fn null_byte_at_end_is_rejected() {
        let err = applescript_escape("hi\0").unwrap_err();
        assert_eq!(err, AppleScriptEscapeError::NullByte);
    }

    #[test]
    fn bell_control_char_is_rejected() {
        let err = applescript_escape("\x07").unwrap_err();
        assert_eq!(err, AppleScriptEscapeError::ControlChar(0x07));
    }

    #[test]
    fn escape_control_char_is_rejected() {
        let err = applescript_escape("\x1b[31m").unwrap_err();
        assert_eq!(err, AppleScriptEscapeError::ControlChar(0x1b));
    }

    #[test]
    fn vertical_tab_is_rejected() {
        let err = applescript_escape("\x0b").unwrap_err();
        assert_eq!(err, AppleScriptEscapeError::ControlChar(0x0b));
    }

    #[test]
    fn form_feed_is_rejected() {
        let err = applescript_escape("\x0c").unwrap_err();
        assert_eq!(err, AppleScriptEscapeError::ControlChar(0x0c));
    }

    #[test]
    fn delete_char_is_allowed_as_high_ascii() {
        // 0x7f (DEL) is technically a control character but lives above the
        // C0 range. We allow it (the rule is "below 0x20"); operators who
        // want to reject DEL can do so upstream.
        assert_eq!(applescript_escape("\x7f").unwrap(), "\x7f");
    }

    #[test]
    fn space_is_allowed() {
        assert_eq!(applescript_escape(" ").unwrap(), " ");
    }

    #[test]
    fn unicode_emoji_passes_through() {
        assert_eq!(applescript_escape("hi U+1F600").unwrap(), "hi U+1F600");
    }

    #[test]
    fn unicode_combining_passes_through() {
        // "café" with a precomposed é (U+00E9).
        assert_eq!(applescript_escape("café").unwrap(), "café");
    }

    #[test]
    fn unicode_high_codepoint_passes_through() {
        // U+1F4A9 (PILE OF POO) and U+1F1FA U+1F1F8 (regional indicator
        // pair) are normal Unicode code points; they round-trip.
        let pile = "\u{1F4A9}";
        assert_eq!(applescript_escape(pile).unwrap(), pile);
    }

    #[test]
    fn windows_line_endings_pass_through() {
        assert_eq!(applescript_escape("a\r\nb").unwrap(), "a\r\nb");
    }

    #[test]
    fn many_consecutive_quotes() {
        assert_eq!(applescript_escape("\"\"\"").unwrap(), "\\\"\\\"\\\"");
    }

    #[test]
    fn many_consecutive_backslashes() {
        assert_eq!(applescript_escape("\\\\\\").unwrap(), "\\\\\\\\\\\\");
    }

    #[test]
    fn long_string_with_mixed_content() {
        let s = "the quick brown fox said \"hello\" — and \\jumped\\ \nover the lazy dog";
        let r = applescript_escape(s).unwrap();
        assert!(r.contains("\\\"hello\\\""));
        assert!(r.contains("\\\\jumped\\\\"));
        assert!(r.contains('\n'));
    }

    #[test]
    fn quote_wraps_and_escapes() {
        assert_eq!(quote("hi").unwrap(), "\"hi\"");
        assert_eq!(quote("a\"b").unwrap(), "\"a\\\"b\"");
        assert_eq!(quote("").unwrap(), "\"\"");
    }

    #[test]
    fn quote_rejects_null_byte() {
        assert!(matches!(
            quote("\0").unwrap_err(),
            AppleScriptEscapeError::NullByte
        ));
    }

    #[test]
    fn quote_rejects_control_char() {
        assert!(matches!(
            quote("\x01").unwrap_err(),
            AppleScriptEscapeError::ControlChar(0x01)
        ));
    }

    #[test]
    fn error_display_includes_control_char_in_hex() {
        let err = AppleScriptEscapeError::ControlChar(0x1b);
        assert!(format!("{err}").contains("0x1b"));
    }

    #[test]
    fn error_display_for_null_byte() {
        let err = AppleScriptEscapeError::NullByte;
        assert!(format!("{err}").contains("null"));
    }

    #[test]
    fn error_eq_and_debug() {
        let a = AppleScriptEscapeError::NullByte;
        let b = AppleScriptEscapeError::NullByte;
        assert_eq!(a, b);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn escape_preserves_length_for_clean_inputs() {
        let s = "abc 123 hello";
        assert_eq!(applescript_escape(s).unwrap().len(), s.len());
    }

    #[test]
    fn escape_grows_for_quote_inputs() {
        let s = "\"";
        let r = applescript_escape(s).unwrap();
        assert!(r.len() > s.len());
    }
}
