//! Fence-state tracking for the chat-text splitter.
//!
//! The delivery loop's splitter (`service::split_text_into_chunks`) must
//! never cut a chunk in the middle of a code fence: a split fence renders
//! as garbage on Telegram / Discord (the closing marker of the first half
//! is missing, so everything after it is swallowed into the code block).
//! This module is the self-contained fence scanner the splitter consults.
//! It recognises two block shapes:
//!
//! - Markdown backtick fences: a line whose first non-space run (up to
//!   three leading spaces allowed, per `CommonMark`) is three-or-more
//!   backticks opens a fence; the rest of that line is the info string
//!   (e.g. the `rust` in ```` ```rust ````). A later line consisting of a
//!   three-or-more backtick run followed by nothing but whitespace closes
//!   it. A line with a non-empty info string does NOT close a fence
//!   (`CommonMark` forbids info strings on closing fences).
//! - Telegram HTML `<pre>` blocks: `<pre>` (attributes allowed, tag must
//!   not span lines) opens, `</pre>` closes. ASCII case-insensitive.
//!
//! Both shapes are tracked by a single state machine, so a `<pre>` literal
//! inside a backtick fence — or a fence-marker line inside a `<pre>` block — is
//! treated as fence *content*, never as a nested fence of its own.
//!
//! NOTE (M18/C5): the shared markdown-renderer card is expected to absorb
//! this logic. Keep it dependency-free and `&[char]`-indexed (matching the
//! splitter's char-indexed cut arithmetic) so it can migrate as-is.

/// The kind of fence a [`FenceSpan`] describes, carrying enough of the
/// original opener to rebuild it when a chunk boundary must close and
/// reopen the fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FenceKind {
    /// Markdown backtick fence. `opener` is the full opening line with
    /// trailing whitespace trimmed (e.g. `"```rust"`), so a reopen
    /// reproduces the info string exactly.
    Backtick { opener: String },
    /// Telegram HTML `<pre>` block. `opener` is the full opening tag
    /// including any attributes (e.g. `"<pre language=\"c\">"`).
    Pre { opener: String },
}

impl FenceKind {
    /// Text appended to a chunk that has to end inside this fence so the
    /// chunk still parses with balanced fences.
    pub(crate) fn closer(&self) -> &'static str {
        match self {
            // The closing backtick run must sit on its own line.
            FenceKind::Backtick { .. } => "\n```",
            FenceKind::Pre { .. } => "</pre>",
        }
    }

    /// Text prepended to the next chunk to re-enter the fence with the
    /// same info string / attributes.
    pub(crate) fn reopen(&self) -> String {
        match self {
            FenceKind::Backtick { opener } => format!("{opener}\n"),
            FenceKind::Pre { opener } => opener.clone(),
        }
    }
}

/// One fenced region of a scanned text, in char indices.
#[derive(Debug, Clone)]
pub(crate) struct FenceSpan {
    /// Char index of the first char of the opener (line start for
    /// backtick fences, the `<` for `<pre>`).
    pub(crate) start: usize,
    /// Char index one past the closing marker (one past the closing
    /// line's content for backtick fences — its newline excluded — or one
    /// past the `>` of `</pre>`). `chars.len()` when the fence is
    /// unclosed.
    pub(crate) end: usize,
    pub(crate) kind: FenceKind,
    /// Whether an explicit closing marker was found (`false` when the
    /// fence runs unclosed to end-of-text). Read by the test-only
    /// [`is_balanced`]; kept unconditional so the scanner's output is
    /// self-describing.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) closed: bool,
}

/// Scan `chars` and return every fenced region, in order. Spans never
/// overlap: a single state machine walks the text, so fence markers
/// inside an open fence of the other kind are treated as content.
pub(crate) fn scan_fence_spans(chars: &[char]) -> Vec<FenceSpan> {
    let len = chars.len();
    let mut spans = Vec::new();
    let mut open: Option<(usize, FenceKind)> = None;
    let mut i = 0usize;
    while i < len {
        match open.take() {
            None => {
                let at_line_start = i == 0 || chars[i - 1] == '\n';
                if at_line_start {
                    let le = line_end(chars, i);
                    if backtick_run(chars, i, le).is_some() {
                        let opener: String = chars[i..le]
                            .iter()
                            .collect::<String>()
                            .trim_end()
                            .to_string();
                        open = Some((i, FenceKind::Backtick { opener }));
                        i = le + 1;
                        continue;
                    }
                }
                if let Some(gt) = pre_open_end(chars, i) {
                    let opener: String = chars[i..=gt].iter().collect();
                    open = Some((i, FenceKind::Pre { opener }));
                    i = gt + 1;
                    continue;
                }
                i += 1;
            }
            Some((start, kind @ FenceKind::Backtick { .. })) => {
                // Inside a backtick fence `i` is always a line start (we
                // advance line by line below).
                let le = line_end(chars, i);
                if is_backtick_close(chars, i, le) {
                    spans.push(FenceSpan {
                        start,
                        end: le,
                        kind,
                        closed: true,
                    });
                } else {
                    open = Some((start, kind));
                }
                i = le + 1;
            }
            Some((start, kind @ FenceKind::Pre { .. })) => {
                if matches_ci(chars, i, "</pre>") {
                    spans.push(FenceSpan {
                        start,
                        end: i + 6,
                        kind,
                        closed: true,
                    });
                    i += 6;
                } else {
                    open = Some((start, kind));
                    i += 1;
                }
            }
        }
    }
    if let Some((start, kind)) = open {
        spans.push(FenceSpan {
            start,
            end: len,
            kind,
            closed: false,
        });
    }
    spans
}

/// `true` when every fence opened in `text` is explicitly closed —
/// i.e. the text parses with balanced fences. Test-only: the splitter's
/// unit table uses it to assert every emitted chunk is balanced.
#[cfg(test)]
pub(crate) fn is_balanced(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    scan_fence_spans(&chars).iter().all(|s| s.closed)
}

/// Char index of the `\n` terminating the line that starts at `i`, or
/// `chars.len()` for the final unterminated line.
fn line_end(chars: &[char], i: usize) -> usize {
    chars[i..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |p| i + p)
}

/// If the line `[ls, le)` begins with a fence marker — up to three
/// leading spaces then a run of three-or-more backticks — return the
/// index one past the backtick run.
fn backtick_run(chars: &[char], ls: usize, le: usize) -> Option<usize> {
    let mut i = ls;
    while i < le && chars[i] == ' ' && i - ls < 3 {
        i += 1;
    }
    let run_start = i;
    while i < le && chars[i] == '`' {
        i += 1;
    }
    (i - run_start >= 3).then_some(i)
}

/// `true` when the line `[ls, le)` closes a backtick fence: a backtick
/// run followed by nothing but whitespace (no info string).
fn is_backtick_close(chars: &[char], ls: usize, le: usize) -> bool {
    match backtick_run(chars, ls, le) {
        Some(after) => chars[after..le].iter().all(|c| c.is_whitespace()),
        None => false,
    }
}

/// If a `<pre ...>` opening tag starts at `i`, return the char index of
/// its closing `>`. The tag must be complete before the next newline.
fn pre_open_end(chars: &[char], i: usize) -> Option<usize> {
    if !matches_ci(chars, i, "<pre") {
        return None;
    }
    let after = i + 4;
    match chars.get(after) {
        Some('>') => Some(after),
        // Attribute form: `<pre language="c">`. Reject `<prefix>`-style
        // words (next char must be whitespace, not part of the tag name).
        Some(c) if c.is_whitespace() && *c != '\n' => {
            let mut j = after + 1;
            while j < chars.len() && chars[j] != '\n' {
                if chars[j] == '>' {
                    return Some(j);
                }
                j += 1;
            }
            None
        }
        _ => None,
    }
}

/// ASCII case-insensitive match of `pat` at char index `i`.
fn matches_ci(chars: &[char], i: usize, pat: &str) -> bool {
    let pat: Vec<char> = pat.chars().collect();
    if i + pat.len() > chars.len() {
        return false;
    }
    chars[i..i + pat.len()]
        .iter()
        .zip(&pat)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_of(text: &str) -> Vec<FenceSpan> {
        let chars: Vec<char> = text.chars().collect();
        scan_fence_spans(&chars)
    }

    #[test]
    fn no_fences_yields_no_spans() {
        assert!(spans_of("plain text\n\nwith paragraphs").is_empty());
    }

    #[test]
    fn backtick_fence_with_info_string() {
        let text = "before\n```rust\nlet x = 1;\n```\nafter";
        let spans = spans_of(text);
        assert_eq!(spans.len(), 1);
        let s = &spans[0];
        assert_eq!(s.start, 7); // start of the "```rust" line
        assert!(s.closed);
        // end is one past the closing run, before its newline.
        let chars: Vec<char> = text.chars().collect();
        let closing: String = chars[s.end - 3..s.end].iter().collect();
        assert_eq!(closing, "```");
        assert_eq!(
            s.kind,
            FenceKind::Backtick {
                opener: "```rust".to_string()
            }
        );
    }

    #[test]
    fn unclosed_backtick_fence_runs_to_eof() {
        let text = "intro\n```py\nprint(1)";
        let spans = spans_of(text);
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].closed);
        assert_eq!(spans[0].end, text.chars().count());
    }

    #[test]
    fn info_string_line_does_not_close_a_fence() {
        // CommonMark: closing fences carry no info string, so the inner
        // "```python" line is content, and only the bare "```" closes.
        let text = "```\n```python\nstill inside\n```\n";
        let spans = spans_of(text);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].closed);
        let chars: Vec<char> = text.chars().collect();
        // The span must extend past "still inside".
        let inner: String = chars[spans[0].start..spans[0].end].iter().collect();
        assert!(inner.contains("still inside"));
    }

    #[test]
    fn fence_marker_allows_up_to_three_leading_spaces() {
        let text = "   ```\ncode\n   ```\ntail";
        let spans = spans_of(text);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].closed);
    }

    #[test]
    fn four_space_indent_is_not_a_fence() {
        assert!(spans_of("    ```\nnot a fence").is_empty());
    }

    #[test]
    fn mid_line_backticks_are_not_a_fence() {
        assert!(spans_of("inline `code` and even ```three``` mid-line").is_empty());
    }

    #[test]
    fn pre_block_plain_and_with_attributes() {
        let text = "a <pre>one</pre> b <pre language=\"c\">two</pre> c";
        let spans = spans_of(text);
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|s| s.closed));
        assert_eq!(
            spans[1].kind,
            FenceKind::Pre {
                opener: "<pre language=\"c\">".to_string()
            }
        );
    }

    #[test]
    fn pre_is_case_insensitive_and_prefix_words_are_rejected() {
        let spans = spans_of("<PRE>x</PRE>");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].closed);
        assert!(spans_of("<preface> not a tag").is_empty());
    }

    #[test]
    fn pre_inside_backtick_fence_is_content() {
        let text = "```html\n<pre>literal</pre>\n```\n<pre>real</pre>";
        let spans = spans_of(text);
        assert_eq!(spans.len(), 2);
        assert!(matches!(spans[0].kind, FenceKind::Backtick { .. }));
        assert!(matches!(spans[1].kind, FenceKind::Pre { .. }));
    }

    #[test]
    fn backticks_inside_pre_are_content() {
        let text = "<pre>\n```\nnot a fence\n```\n</pre>";
        let spans = spans_of(text);
        assert_eq!(spans.len(), 1);
        assert!(matches!(spans[0].kind, FenceKind::Pre { .. }));
        assert!(spans[0].closed);
    }

    #[test]
    fn is_balanced_verdicts() {
        assert!(is_balanced("no fences at all"));
        assert!(is_balanced("```rust\ncode\n```"));
        assert!(is_balanced("<pre>x</pre>"));
        assert!(!is_balanced("```rust\ncode"));
        assert!(!is_balanced("<pre>x"));
    }

    #[test]
    fn closer_and_reopen_round_trip() {
        let bt = FenceKind::Backtick {
            opener: "```rust".to_string(),
        };
        assert_eq!(bt.closer(), "\n```");
        assert_eq!(bt.reopen(), "```rust\n");
        let pre = FenceKind::Pre {
            opener: "<pre>".to_string(),
        };
        assert_eq!(pre.closer(), "</pre>");
        assert_eq!(pre.reopen(), "<pre>");
    }
}
