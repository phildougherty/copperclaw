//! Fence-aware greedy text splitter.
//!
//! MIGRATION (M18/C5b): the delivery loop
//! (`copperclaw-host-delivery`) used to own this splitter alongside its
//! private `fence` module (both built by C2). Per the fence module's
//! migration note it now lives here in the shared renderer, so every
//! adapter path that needs to chunk a long reply into cap-sized pieces
//! consults one fence-aware implementation. `host-delivery` delegates to
//! [`split_into_chunks`]; its own C2 splitter tests still pass, now
//! routed through this migrated logic.

use super::fence::scan_fence_spans;

/// Greedy chunker honoring `max` chars per chunk. Preference order for
/// each cut: paragraph boundary (`\n\n`) → sentence boundary → hard cut.
/// Operates on `char` indices, never on bytes.
///
/// Fence-aware: a cut never lands inside a code fence — markdown backtick
/// fences or Telegram HTML `<pre>` blocks, as scanned by
/// [`super::fence`] — because a split fence renders as garbage on
/// Telegram / Discord. When the natural cut falls inside a fence the
/// splitter prefers, in order:
///
/// 1. cutting right AFTER the fence, when the whole fence still fits the
///    window (e.g. `find_cut` picked a blank line between two code
///    paragraphs);
/// 2. cutting right BEFORE the fence, when this chunk has pre-fence
///    content (the fence then leads the next chunk);
/// 3. closing the fence at the cut and reopening it — same info string /
///    tag — at the start of the next chunk, when the fence itself
///    outruns the cap.
///
/// Every emitted chunk therefore parses with balanced fences.
pub fn split_into_chunks(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let spans = scan_fence_spans(&chars);
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    // Reopen line for a fence the previous chunk had to close mid-block.
    let mut reopen: Option<String> = None;
    while start < chars.len() {
        let prefix = reopen.take().unwrap_or_default();
        let prefix_len = prefix.chars().count();
        let budget = max.saturating_sub(prefix_len);
        if budget == 0 {
            // Degenerate cap: the reopen line alone fills the chunk. Emit
            // it bare and continue un-prefixed (giving up on balance for
            // this fence) rather than looping forever.
            out.push(prefix);
            continue;
        }
        let remaining = chars.len() - start;
        if remaining <= budget {
            let tail: String = chars[start..].iter().collect();
            out.push(format!("{prefix}{tail}"));
            break;
        }
        let window_end = start + budget;
        let mut cut = find_cut(&chars, start, window_end);
        // `Some(next_start)` when the cut closes a fence mid-block: the
        // chunk was already pushed and `next_start` preserves the next
        // code line's indentation (no generic whitespace skip).
        let mut fence_resume: Option<usize> = None;
        if let Some(span) = spans.iter().find(|s| s.start < cut && cut < s.end) {
            if span.end <= window_end {
                // Whole fence fits this window; `find_cut` just picked a
                // boundary inside it. Cut right after the fence instead.
                cut = span.end;
            } else if span.start > start {
                // A pre-fence cut exists within the limit: cut right
                // before the fence and let it lead the next chunk.
                cut = span.start;
            } else {
                // The chunk starts inside the fence and the fence outruns
                // the window: close it at the cut, reopen on the next
                // chunk. Reserve room for the closing marker.
                let closer = span.kind.closer();
                let closer_len = closer.chars().count();
                if budget > closer_len {
                    let content_end = start + (budget - closer_len);
                    // Prefer cutting at a line break so code lines stay
                    // whole; fall back to a hard mid-line cut. Consume
                    // ONLY the newline — the generic whitespace skip
                    // would eat the next code line's indentation.
                    let (body_end, next_start) = (start + 1..=content_end)
                        .rev()
                        .find(|&j| chars[j] == '\n')
                        .map_or((content_end, content_end), |j| (j, j + 1));
                    let body: String = chars[start..body_end].iter().collect();
                    out.push(format!("{prefix}{body}{closer}"));
                    reopen = Some(span.kind.reopen());
                    fence_resume = Some(next_start);
                }
                // else: cap too small to even fit the closing marker
                // (sub-8-char caps) — keep the plain hard cut.
            }
        }
        if let Some(next_start) = fence_resume {
            start = next_start;
            continue;
        }
        let chunk: String = chars[start..cut].iter().collect();
        out.push(format!("{prefix}{}", chunk.trim_end()));
        // Skip whitespace at the cut so the next chunk doesn't start with a
        // leading newline or space.
        start = cut;
        while start < chars.len()
            && (chars[start] == ' ' || chars[start] == '\n' || chars[start] == '\t')
        {
            start += 1;
        }
    }
    out.retain(|s| !s.is_empty());
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Find the best cut point in `[lo, hi)` (char indices). Tries paragraph
/// (`\n\n`) → sentence-ender then space (`. `, `! `, `? `, `。`, `！`,
/// `？`) → fallback to `hi` (hard cut).
fn find_cut(chars: &[char], lo: usize, hi: usize) -> usize {
    // Look for the last `\n\n` in the window.
    let mut i = hi.saturating_sub(1);
    while i > lo + 1 {
        if chars[i - 1] == '\n' && chars[i] == '\n' {
            return i + 1;
        }
        i -= 1;
    }
    // Sentence boundary: `.`, `!`, `?` followed by space; or a CJK
    // full-stop / exclamation / question mark.
    let mut i = hi.saturating_sub(1);
    while i > lo {
        let c = chars[i];
        if c == '。' || c == '！' || c == '？' {
            return i + 1;
        }
        if i + 1 < chars.len() && (c == '.' || c == '!' || c == '?') && chars[i + 1] == ' ' {
            return i + 1;
        }
        i -= 1;
    }
    // Last space before hi.
    let mut i = hi.saturating_sub(1);
    while i > lo {
        if chars[i] == ' ' || chars[i] == '\n' {
            return i;
        }
        i -= 1;
    }
    hi
}

#[cfg(test)]
mod tests {
    use super::super::fence::is_balanced;
    use super::*;

    /// Split `text` at `max` and assert the shared fence-splitting
    /// invariants: every chunk fits the cap and parses with balanced
    /// fences. Returns the chunks for case-specific assertions. This is
    /// the renderer-side mirror of `host-delivery`'s C2 splitter table.
    fn split_balanced(text: &str, max: usize) -> Vec<String> {
        let chunks = split_into_chunks(text, max);
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.chars().count() <= max,
                "chunk {i} exceeds cap {max}: {} chars",
                c.chars().count()
            );
            assert!(is_balanced(c), "chunk {i} has unbalanced fences:\n{c}");
        }
        chunks
    }

    #[test]
    fn split_fence_longer_than_cap_closes_and_reopens() {
        // A single fenced block longer than the cap: the splitter must
        // close the fence at the cut and reopen it — same info string —
        // on the next chunk, cutting at a line boundary.
        let code: String = (0..40).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("line_{i:03}_abcdefghij\n"));
            acc
        });
        let text = format!("```rust\n{code}```");
        let chunks = split_balanced(&text, 200);
        assert!(chunks.len() > 1, "fence must split: {chunks:?}");
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.starts_with("```rust\n"),
                "chunk {i} must open with the info string: {c}"
            );
            assert!(c.ends_with("```"), "chunk {i} must close the fence: {c}");
        }
        // No code line is torn in half: every original line appears
        // intact in exactly one chunk.
        let joined = chunks.join("\n");
        for i in 0..40 {
            let line = format!("line_{i:03}_abcdefghij");
            assert_eq!(
                joined.matches(&line).count(),
                1,
                "line {i} torn or duplicated"
            );
        }
    }

    #[test]
    fn split_prefers_pre_fence_cut_when_fence_fits_next_chunk() {
        // Intro line + a two-line fence that fits the cap on its own but
        // not together with the intro. find_cut's natural cut (the last
        // newline in the window) lands INSIDE the fence; the splitter
        // must move it BEFORE the fence, which is then delivered whole
        // in the second chunk.
        let intro = "x".repeat(60);
        let fence = format!("```py\n{}\n{}\n```", "y".repeat(30), "y".repeat(30));
        let text = format!("{intro}\n{fence}");
        let chunks = split_balanced(&text, 100);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert_eq!(chunks[0], intro);
        assert_eq!(chunks[1], fence);
    }

    #[test]
    fn split_does_not_cut_at_blank_line_inside_fitting_fence() {
        // The fence contains a blank line — a paragraph boundary that
        // find_cut picks as the natural cut — and fits the window whole;
        // the trailing text pushes the total over the cap. The cut must
        // land AFTER the fence, not at the blank line inside it.
        let fence = format!("```\n{}\n\n{}\n```", "a".repeat(20), "b".repeat(20));
        let tail = "t".repeat(80);
        let text = format!("{fence}\n{tail}");
        let chunks = split_balanced(&text, 100);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert_eq!(chunks[0], fence);
        assert_eq!(chunks[1], tail);
    }

    #[test]
    fn split_pre_block_longer_than_cap_closes_and_reopens_tag() {
        // Telegram HTML <pre> block longer than the cap: close with
        // </pre> at the cut, reopen with the original tag (attributes
        // preserved) on the next chunk.
        let body: String = (0..30).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("row_{i:02}_0123456789\n"));
            acc
        });
        let text = format!("<pre language=\"c\">{body}</pre>");
        let chunks = split_balanced(&text, 120);
        assert!(chunks.len() > 1, "{chunks:?}");
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.starts_with("<pre language=\"c\">"),
                "chunk {i} must reopen the tag: {c}"
            );
            assert!(c.ends_with("</pre>"), "chunk {i} must close the tag: {c}");
        }
    }

    #[test]
    fn split_preserves_code_indentation_across_reopen() {
        // Cutting inside a fence consumes only the newline at the cut, so
        // the next code line keeps its leading indentation.
        let code: String = (0..30).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("    indented_{i:02}_stmt();\n"));
            acc
        });
        let text = format!("```c\n{code}```");
        let chunks = split_balanced(&text, 150);
        assert!(chunks.len() > 1, "{chunks:?}");
        for (i, c) in chunks.iter().skip(1).enumerate() {
            let first_code_line = c.lines().nth(1).unwrap_or("");
            assert!(
                first_code_line.starts_with("    indented_"),
                "chunk {} lost indentation after reopen: {c}",
                i + 1
            );
        }
    }

    #[test]
    fn split_unfenced_text_behaviour_unchanged_by_fence_scan() {
        // Inline backticks and mid-line triple-backticks are not fences;
        // the paragraph cut behaves exactly as before.
        let text = format!(
            "uses `inline` and ```mid-line``` marks {}\n\n{}",
            "a".repeat(40),
            "b".repeat(50)
        );
        let chunks = split_balanced(&text, 90);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert!(chunks[1].starts_with('b'));
    }

    #[test]
    fn split_counts_chars_not_bytes() {
        // A CJK char is 3 bytes in UTF-8 but counts as 1 toward the cap.
        let text = "漢".repeat(20);
        let chunks = split_into_chunks(&text, 10);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 10);
    }

    #[test]
    fn split_short_text_returns_single_chunk() {
        assert_eq!(split_into_chunks("short", 100), vec!["short".to_string()]);
    }
}
