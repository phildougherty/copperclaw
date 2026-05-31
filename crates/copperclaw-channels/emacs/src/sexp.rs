//! Minimal s-expression parser for `emacsclient -e` output.
//!
//! `emacsclient -e <sexp>` prints the printed representation of the result of
//! evaluating `<sexp>` to stdout. We only need to recognize a very small
//! subset of elisp printed forms:
//!
//! - `nil` — the empty / "no message" sentinel.
//! - A list of cons cells of the form
//!   `(("KEY" . "VALUE") ("KEY" . "VALUE") ...)` — an alist of string keys
//!   and string values. This is the convention the user's
//!   `copperclaw-pop-inbound` function returns when there is a queued inbound
//!   message.
//!
//! Strings are parsed with elisp's usual escape conventions: `\n`, `\t`,
//! `\\`, `\"`, and `\xNN` (one or two hex digits, per elisp printed form).
//!
//! Anything else (numbers, symbols other than `nil`, vectors, dotted-pair
//! cdrs that aren't strings, etc.) returns
//! [`ParseError::Unsupported`]. The adapter logs and ignores such results
//! rather than crashing the poll loop.

use std::collections::BTreeMap;

/// Errors produced by [`parse`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The input ended before a complete value was read.
    #[error("unexpected end of input")]
    UnexpectedEof,
    /// An expected character was missing.
    #[error("expected {expected} at position {pos}, found {found:?}")]
    Expected {
        /// Description of what was expected (e.g. `"\""`).
        expected: &'static str,
        /// Byte position into the input where the failure occurred.
        pos: usize,
        /// What we actually saw (single char, lossy if non-ascii).
        found: Option<char>,
    },
    /// Trailing input after a complete value was parsed.
    #[error("trailing input at position {pos}")]
    Trailing {
        /// Byte position of the first unexpected trailing byte.
        pos: usize,
    },
    /// An escape sequence was malformed (bad `\xNN` digits, dangling `\`).
    #[error("bad escape sequence at position {pos}")]
    BadEscape {
        /// Byte position of the offending backslash.
        pos: usize,
    },
    /// The shape of the parsed value is not one we recognize as an inbound
    /// payload.
    #[error("unsupported sexp shape: {0}")]
    Unsupported(String),
}

/// Parsed result of an `emacsclient -e` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SexpValue {
    /// The literal symbol `nil`.
    Nil,
    /// A list of string-key/string-value cons cells (an alist).
    Alist(Vec<(String, String)>),
}

impl SexpValue {
    /// Convenience: collect alist pairs into a `BTreeMap` for ergonomic
    /// lookup. Duplicate keys keep the last value, matching how a JSON
    /// object would round-trip.
    pub fn as_map(&self) -> Option<BTreeMap<String, String>> {
        match self {
            Self::Nil => None,
            Self::Alist(pairs) => {
                let mut out = BTreeMap::new();
                for (k, v) in pairs {
                    out.insert(k.clone(), v.clone());
                }
                Some(out)
            }
        }
    }
}

/// Parse the printed form returned by `emacsclient -e <sexp>`.
///
/// Trailing whitespace (including newlines emacsclient appends) is
/// tolerated.
pub fn parse(input: &str) -> Result<SexpValue, ParseError> {
    let bytes = input.as_bytes();
    let mut p = Parser { bytes, pos: 0 };
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != bytes.len() {
        return Err(ParseError::Trailing { pos: p.pos });
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> Result<SexpValue, ParseError> {
        match self.peek() {
            None => Err(ParseError::UnexpectedEof),
            Some(b'(') => self.parse_list(),
            Some(b) if is_symbol_start(b) => {
                let sym = self.parse_symbol();
                if sym == "nil" {
                    Ok(SexpValue::Nil)
                } else {
                    Err(ParseError::Unsupported(format!("symbol `{sym}`")))
                }
            }
            Some(_) => Err(ParseError::Expected {
                expected: "`(` or `nil`",
                pos: self.pos,
                found: self.current_char(),
            }),
        }
    }

    fn current_char(&self) -> Option<char> {
        self.bytes
            .get(self.pos)
            .map(|b| char::from_u32(u32::from(*b)).unwrap_or('?'))
    }

    fn parse_symbol(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_symbol_continue(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
        // Safe: only ASCII bytes accepted by is_symbol_continue.
        String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned()
    }

    fn parse_list(&mut self) -> Result<SexpValue, ParseError> {
        self.expect(b'(', "`(`")?;
        self.skip_ws();

        // Empty list `()` is treated as `Nil` (an empty alist with no pairs
        // is indistinguishable in elisp from nil).
        if self.peek() == Some(b')') {
            self.pos += 1;
            return Ok(SexpValue::Nil);
        }

        let mut pairs: Vec<(String, String)> = Vec::new();
        loop {
            self.skip_ws();
            // Each element must be a cons cell `("KEY" . "VALUE")`.
            self.expect(b'(', "`(` for alist cons cell")?;
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b'.', "`.` separator in cons cell")?;
            self.skip_ws();
            let value = self.parse_string()?;
            self.skip_ws();
            self.expect(b')', "`)` to close cons cell")?;
            pairs.push((key, value));

            self.skip_ws();
            match self.peek() {
                Some(b')') => {
                    self.pos += 1;
                    break;
                }
                Some(_) => {
                    // Another cons cell follows.
                }
                None => return Err(ParseError::UnexpectedEof),
            }
        }
        Ok(SexpValue::Alist(pairs))
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"', "`\"` to open string")?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(ParseError::UnexpectedEof),
                Some(b'"') => return Ok(out),
                Some(b'\\') => {
                    let esc_pos = self.pos - 1;
                    match self.bump() {
                        Some(b'n') => out.push('\n'),
                        Some(b't') => out.push('\t'),
                        Some(b'r') => out.push('\r'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'"') => out.push('"'),
                        Some(b'x') => {
                            // 1 or 2 hex digits, per elisp printed form.
                            let mut value: u32 = 0;
                            let mut digits = 0;
                            while digits < 2 {
                                match self.peek() {
                                    Some(b) if is_hex_digit(b) => {
                                        value = value * 16 + hex_value(b);
                                        self.pos += 1;
                                        digits += 1;
                                    }
                                    _ => break,
                                }
                            }
                            if digits == 0 {
                                return Err(ParseError::BadEscape { pos: esc_pos });
                            }
                            // Elisp's `\xNN` in printed strings is a raw
                            // codepoint; map it through `char::from_u32`.
                            match char::from_u32(value) {
                                Some(c) => out.push(c),
                                None => return Err(ParseError::BadEscape { pos: esc_pos }),
                            }
                        }
                        None | Some(_) => return Err(ParseError::BadEscape { pos: esc_pos }),
                    }
                }
                Some(b) => {
                    // Pass through arbitrary UTF-8 bytes by re-decoding from
                    // the source string. We previously read one byte; we
                    // need to advance the character boundary if the byte
                    // started a multibyte sequence.
                    if b < 0x80 {
                        out.push(char::from(b));
                    } else {
                        // The byte we consumed was a leading byte of a
                        // multi-byte UTF-8 char. Find the next char
                        // boundary in the original input.
                        let start = self.pos - 1;
                        // Walk to next valid utf8 boundary.
                        let mut end = self.pos;
                        while end < self.bytes.len() && (self.bytes[end] & 0xC0) == 0x80 {
                            end += 1;
                        }
                        let slice = std::str::from_utf8(&self.bytes[start..end]).map_err(|_| {
                            ParseError::BadEscape { pos: start }
                        })?;
                        out.push_str(slice);
                        self.pos = end;
                    }
                }
            }
        }
    }

    fn expect(&mut self, byte: u8, label: &'static str) -> Result<(), ParseError> {
        match self.peek() {
            Some(b) if b == byte => {
                self.pos += 1;
                Ok(())
            }
            other => Err(ParseError::Expected {
                expected: label,
                pos: self.pos,
                found: other.map(|b| char::from_u32(u32::from(b)).unwrap_or('?')),
            }),
        }
    }
}

fn is_symbol_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'_')
}

fn is_symbol_continue(b: u8) -> bool {
    matches!(
        b,
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_'
    )
}

fn is_hex_digit(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn hex_value(b: u8) -> u32 {
    match b {
        b'0'..=b'9' => u32::from(b - b'0'),
        b'a'..=b'f' => u32::from(b - b'a') + 10,
        b'A'..=b'F' => u32::from(b - b'A') + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nil() {
        assert_eq!(parse("nil").unwrap(), SexpValue::Nil);
    }

    #[test]
    fn parses_nil_with_trailing_newline() {
        assert_eq!(parse("nil\n").unwrap(), SexpValue::Nil);
    }

    #[test]
    fn parses_nil_with_leading_whitespace() {
        assert_eq!(parse("   nil").unwrap(), SexpValue::Nil);
    }

    #[test]
    fn empty_list_is_nil() {
        assert_eq!(parse("()").unwrap(), SexpValue::Nil);
    }

    #[test]
    fn parses_single_pair() {
        let v = parse("((\"buffer\" . \"*chat*\"))").unwrap();
        assert_eq!(
            v,
            SexpValue::Alist(vec![("buffer".into(), "*chat*".into())])
        );
    }

    #[test]
    fn parses_multi_pair() {
        let v = parse("((\"buffer\" . \"*chat*\") (\"text\" . \"hi\") (\"sender\" . \"alice\"))")
            .unwrap();
        assert_eq!(
            v,
            SexpValue::Alist(vec![
                ("buffer".into(), "*chat*".into()),
                ("text".into(), "hi".into()),
                ("sender".into(), "alice".into()),
            ])
        );
    }

    #[test]
    fn handles_escape_newline_tab_backslash_quote() {
        let v = parse("((\"k\" . \"line1\\nline2\\twith \\\\ and \\\"quote\\\"\"))").unwrap();
        match v {
            SexpValue::Alist(p) => {
                assert_eq!(p[0].1, "line1\nline2\twith \\ and \"quote\"");
            }
            SexpValue::Nil => panic!("expected alist"),
        }
    }

    #[test]
    fn handles_escape_carriage_return() {
        let v = parse("((\"k\" . \"a\\rb\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "a\rb");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn handles_hex_escape_one_digit() {
        let v = parse("((\"k\" . \"\\x9\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "\t");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn handles_hex_escape_two_digits() {
        let v = parse("((\"k\" . \"\\x41\\x42\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "AB");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn hex_escape_stops_at_non_hex() {
        // After two digits the parser stops, so the trailing 'g' is a
        // literal character.
        let v = parse("((\"k\" . \"\\x41g\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "Ag");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn rejects_empty_hex_escape() {
        let err = parse("((\"k\" . \"\\xZ\"))").unwrap_err();
        assert!(matches!(err, ParseError::BadEscape { .. }));
    }

    #[test]
    fn rejects_dangling_backslash() {
        let err = parse("((\"k\" . \"abc\\\"))").unwrap_err();
        // \" is interpreted as an escaped quote so the string is unterminated.
        assert!(matches!(err, ParseError::UnexpectedEof));
    }

    #[test]
    fn rejects_unknown_escape() {
        let err = parse("((\"k\" . \"\\q\"))").unwrap_err();
        assert!(matches!(err, ParseError::BadEscape { .. }));
    }

    #[test]
    fn rejects_unterminated_string() {
        let err = parse("((\"k\" . \"abc").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedEof));
    }

    #[test]
    fn rejects_missing_dot() {
        let err = parse("((\"k\" \"v\"))").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }));
    }

    #[test]
    fn rejects_missing_close_cons() {
        let err = parse("((\"k\" . \"v\"").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. } | ParseError::UnexpectedEof));
    }

    #[test]
    fn rejects_missing_close_list() {
        let err = parse("((\"k\" . \"v\")").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. } | ParseError::UnexpectedEof));
    }

    #[test]
    fn rejects_top_level_number() {
        let err = parse("123").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }));
    }

    #[test]
    fn rejects_unknown_symbol() {
        let err = parse("true").unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)));
    }

    #[test]
    fn rejects_dotted_pair_with_non_string_cdr() {
        let err = parse("((\"k\" . 5))").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }));
    }

    #[test]
    fn rejects_dotted_pair_with_non_string_car() {
        let err = parse("((5 . \"v\"))").unwrap_err();
        assert!(matches!(err, ParseError::Expected { .. }));
    }

    #[test]
    fn rejects_trailing_input() {
        let err = parse("nil garbage").unwrap_err();
        assert!(matches!(err, ParseError::Trailing { .. }));
    }

    #[test]
    fn rejects_empty_input() {
        let err = parse("").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedEof));
    }

    #[test]
    fn handles_unicode_non_ascii() {
        // Non-ASCII letter from Latin-1+ Supplement (no emoji).
        let v = parse("((\"k\" . \"caf\u{00e9}\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "caf\u{00e9}");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn handles_unicode_multibyte() {
        // Greek small letter alpha; a 2-byte UTF-8 sequence.
        let v = parse("((\"k\" . \"\u{03b1}\u{03b2}\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "\u{03b1}\u{03b2}");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn handles_unicode_three_byte() {
        // Latin extended; 2-byte char. Also include CJK 3-byte char.
        let v = parse("((\"k\" . \"\u{4e2d}\u{6587}\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "\u{4e2d}\u{6587}");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn as_map_on_nil_returns_none() {
        assert!(SexpValue::Nil.as_map().is_none());
    }

    #[test]
    fn as_map_on_alist_returns_btreemap() {
        let v = SexpValue::Alist(vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
        let m = v.as_map().unwrap();
        assert_eq!(m.get("a"), Some(&"1".to_string()));
        assert_eq!(m.get("b"), Some(&"2".to_string()));
    }

    #[test]
    fn as_map_dedups_duplicate_keys_last_wins() {
        let v = SexpValue::Alist(vec![("k".into(), "first".into()), ("k".into(), "last".into())]);
        let m = v.as_map().unwrap();
        assert_eq!(m.get("k"), Some(&"last".to_string()));
    }

    #[test]
    fn parse_error_implements_error_trait() {
        let e = ParseError::UnexpectedEof;
        let s = format!("{e}");
        assert!(!s.is_empty());
        let d = format!("{e:?}");
        assert!(d.contains("UnexpectedEof"));
    }

    #[test]
    fn parse_error_with_position_renders() {
        let e = ParseError::Expected {
            expected: "x",
            pos: 4,
            found: Some('y'),
        };
        let s = format!("{e}");
        assert!(s.contains("position 4"));
    }

    #[test]
    fn empty_string_value_is_ok() {
        let v = parse("((\"k\" . \"\"))").unwrap();
        if let SexpValue::Alist(p) = v {
            assert_eq!(p[0].1, "");
        } else {
            panic!("expected alist");
        }
    }

    #[test]
    fn whitespace_only_input_is_eof() {
        let err = parse("   \n\t ").unwrap_err();
        assert!(matches!(err, ParseError::UnexpectedEof));
    }
}
