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
///
/// `channel_type` labels the fence metrics (C2/C5b:
/// `copperclaw_delivery_fence_split_total{channel_type,kind}` and
/// `copperclaw_delivery_fence_unbalanced_input_total{channel_type}`) with the
/// calling channel; it does not affect the chunking itself.
pub fn split_into_chunks(text: &str, max: usize, channel_type: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let spans = scan_fence_spans(&chars);
    if spans.iter().any(|s| !s.closed) {
        copperclaw_metrics::inc_delivery_fence_unbalanced_input(channel_type);
    }
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
                    copperclaw_metrics::inc_delivery_fence_split(
                        channel_type,
                        span.kind.kind_label(),
                    );
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

/// Split honoring BOTH a char cap and (optionally) a UTF-8 **byte** cap.
///
/// Some platforms document their limit in bytes, not chars — Webex's
/// `POST /messages` caps `text` / `markdown` / `html` at 7 439 *bytes*.
/// [`split_into_chunks`] counts chars, so a CJK- or emoji-heavy reply can
/// pass a 7 439-char split and still arrive at ~3-4x the byte budget; the
/// 10% render headroom does not cover a 3x overshoot.
///
/// Rather than pessimize every message to `bytes / 4` chars — which would
/// fragment ordinary ASCII replies fourfold for no reason — this runs the
/// shared char splitter first and then re-splits only the chunks that
/// actually blow the byte budget, using each offending chunk's own
/// bytes-per-char density to derive a tighter char cap. ASCII text (1
/// byte/char) never triggers the second pass at all.
///
/// `max_chars` and `max_bytes` are EFFECTIVE caps — the caller applies
/// [`effective_max`] headroom to each, exactly as it does for
/// [`split_into_chunks`]. `max_bytes` of `None` (or 0) means "no byte
/// cap", and the result is identical to [`split_into_chunks`].
///
/// Fence balance is preserved: every chunk the char splitter emits parses
/// with balanced fences, and re-splitting such a chunk runs the same
/// fence-aware logic over it, so sub-chunks are balanced too.
pub fn split_into_chunks_within_bytes(
    text: &str,
    max_chars: usize,
    max_bytes: Option<usize>,
    channel_type: &str,
) -> Vec<String> {
    let chunks = split_into_chunks(text, max_chars, channel_type);
    let Some(max_bytes) = max_bytes.filter(|b| *b > 0) else {
        return chunks;
    };
    chunks
        .into_iter()
        .flat_map(|c| enforce_byte_cap(c, max_chars, max_bytes, channel_type, BYTE_SPLIT_DEPTH))
        .collect()
}

/// How many density-guided re-split passes [`split_into_chunks_within_bytes`]
/// will make before accepting an over-budget chunk. Each pass strictly
/// shrinks the char cap, so this is a belt-and-braces bound: mixed
/// ASCII/CJK text converges in one or two.
const BYTE_SPLIT_DEPTH: u8 = 8;

/// Re-split one chunk until it fits `max_bytes`, or the cap can shrink no
/// further. Returns the chunk untouched when it already fits.
fn enforce_byte_cap(
    chunk: String,
    cap_chars: usize,
    max_bytes: usize,
    channel_type: &str,
    depth: u8,
) -> Vec<String> {
    if chunk.len() <= max_bytes || depth == 0 || cap_chars <= 1 {
        return vec![chunk];
    }
    // Derive a char cap from THIS chunk's density: chars * (max_bytes /
    // bytes). Clamped strictly below the cap that produced the chunk so the
    // recursion always makes progress.
    let chars = chunk.chars().count();
    let est = (chars.saturating_mul(max_bytes) / chunk.len()).max(1);
    let next = est.min(cap_chars - 1).max(1);
    let pieces = split_into_chunks(&chunk, next, channel_type);
    if pieces.len() <= 1 {
        // The splitter cannot break this down any further (single char, or a
        // degenerate cap); emit as-is rather than spin.
        return pieces;
    }
    pieces
        .into_iter()
        .flat_map(|p| enforce_byte_cap(p, next, max_bytes, channel_type, depth - 1))
        .collect()
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

/// Percentage of a platform's declared cap the splitter is allowed to
/// fill. The remainder is render headroom — see [`effective_max`].
pub const RENDER_HEADROOM_PERCENT: usize = 90;

/// Shrink a channel's declared cap
/// ([`crate::ChannelAdapter::max_message_chars`]) by a safety margin, so a
/// chunk that fits the cap as *markdown* still fits it once the adapter
/// has *rendered* it.
///
/// Why this exists: the host splits the agent's raw markdown at exactly
/// `max` chars, but every adapter renders AFTER the split — e.g. telegram
/// runs `render(&text, Flavor::Html)` on the chunk it is handed. Rendering
/// only ever grows the string:
///
/// - HTML escaping expands single chars (`&` → `&amp;`, `<` → `&lt;`);
/// - inline markup gains tags (`**b**` → `<b>b</b>`, `[t](u)` → `<a
///   href="u">t</a>`).
///
/// So a chunk of exactly 4096 markdown chars can be 4200+ chars at the
/// API, and the platform rejects the whole message
/// (`Bad Request: message is too long`, `MESSAGE_TOO_LONG`, `text is too
/// long`) — which the delivery loop records as a permanent drop. Reserving
/// [`RENDER_HEADROOM_PERCENT`] of the cap absorbs that expansion for every
/// adapter at once, instead of each one guessing its own fudge factor.
///
/// `None` in → `None` out: a channel that opted out of splitting stays
/// opted out. A cap of 0 (degenerate) yields `Some(1)` rather than
/// `Some(0)`, since a zero-width chunk would make the splitter spin.
#[must_use]
pub fn effective_max(max: Option<usize>) -> Option<usize> {
    max.map(|m| {
        // floor(m * 90 / 100) computed without overflowing on huge caps.
        let scaled =
            (m / 100) * RENDER_HEADROOM_PERCENT + (m % 100) * RENDER_HEADROOM_PERCENT / 100;
        scaled.max(1)
    })
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
        let chunks = split_into_chunks(text, max, "test");
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
        let chunks = split_into_chunks(&text, 10, "test");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 10);
    }

    #[test]
    fn split_short_text_returns_single_chunk() {
        assert_eq!(
            split_into_chunks("short", 100, "test"),
            vec!["short".to_string()]
        );
    }

    /// REGRESSION (fix 2): Webex documents 7 439 **bytes**, and the
    /// splitter counts chars. A CJK reply (3 bytes/char in UTF-8) passes a
    /// 7 439-char split and arrives at ~3x the byte budget — the 10%
    /// render headroom does not cover a 3x overshoot. With the byte cap
    /// declared, every chunk must fit BOTH budgets.
    #[test]
    fn split_within_bytes_honours_byte_cap_on_cjk() {
        let cap = 7439;
        let text = "这是一个很长的中文回复内容需要被正确地分割。".repeat(500);
        assert!(text.len() > cap * 2, "fixture must overshoot in bytes");

        // Char-only split: passes the char cap, blows the byte budget.
        let char_only = split_into_chunks(&text, cap, "webex");
        assert!(
            char_only.iter().any(|c| c.len() > cap),
            "fixture no longer demonstrates the byte overshoot"
        );

        let chunks = split_into_chunks_within_bytes(&text, cap, Some(cap), "webex");
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.len() <= cap,
                "chunk {i} is {} bytes, over the byte cap {cap}",
                c.len()
            );
            assert!(c.chars().count() <= cap, "chunk {i} exceeds the char cap");
            assert!(is_balanced(c), "chunk {i} has unbalanced fences");
        }
        // Nothing was dropped: every source char is still accounted for.
        let joined: String = chunks.concat();
        assert!(joined.chars().count() >= text.chars().count() - chunks.len());
    }

    /// Emoji are up to 4 bytes/char — the worst case for a byte budget.
    #[test]
    fn split_within_bytes_honours_byte_cap_on_emoji() {
        let cap = 400;
        let text = "🙂🚀🎉".repeat(300); // 900 chars, 3 600 bytes
        let chunks = split_into_chunks_within_bytes(&text, cap, Some(cap), "webex");
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= cap, "chunk {i} is {} bytes > {cap}", c.len());
        }
        assert_eq!(
            chunks.concat().chars().count(),
            text.chars().count(),
            "emoji body lost characters across the split"
        );
    }

    /// The byte cap must cost ASCII nothing: a plain English reply splits
    /// exactly where the char splitter would. This is the whole reason the
    /// fix is a second cap rather than a `bytes / 4` char cap — the latter
    /// would fragment ordinary replies fourfold.
    #[test]
    fn split_within_bytes_leaves_ascii_chunking_identical() {
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(400);
        let plain = split_into_chunks(&text, 1000, "webex");
        let capped = split_into_chunks_within_bytes(&text, 1000, Some(1000), "webex");
        assert_eq!(plain, capped);
    }

    /// No byte cap declared → behaviour is exactly [`split_into_chunks`],
    /// including for multibyte text.
    #[test]
    fn split_within_bytes_without_byte_cap_matches_char_splitter() {
        let text = "漢字テスト。".repeat(200);
        assert_eq!(
            split_into_chunks_within_bytes(&text, 300, None, "t"),
            split_into_chunks(&text, 300, "t")
        );
        assert_eq!(
            split_into_chunks_within_bytes(&text, 300, Some(0), "t"),
            split_into_chunks(&text, 300, "t")
        );
    }

    /// Mixed ASCII + CJK: only the multibyte chunks get re-split, and the
    /// density estimate converges without hitting the depth guard.
    #[test]
    fn split_within_bytes_handles_mixed_density() {
        let cap = 300;
        let text = format!(
            "{}\n\n{}\n\n{}",
            "ascii paragraph here. ".repeat(40),
            "中文段落内容。".repeat(80),
            "more ascii text. ".repeat(40)
        );
        let chunks = split_into_chunks_within_bytes(&text, cap, Some(cap), "t");
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= cap, "chunk {i} is {} bytes > {cap}", c.len());
        }
    }

    /// A fenced code block of CJK must still close and reopen its fence
    /// when the byte cap forces the tighter split.
    #[test]
    fn split_within_bytes_preserves_fence_balance_under_byte_pressure() {
        let body: String = (0..40).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("行{i:02}：这是一行中文代码注释内容\n"));
            acc
        });
        let text = format!("```rust\n{body}```");
        let chunks = split_into_chunks_within_bytes(&text, 400, Some(400), "t");
        assert!(chunks.len() > 1, "byte cap must force a split");
        for (i, c) in chunks.iter().enumerate() {
            assert!(c.len() <= 400, "chunk {i} is {} bytes > 400", c.len());
            assert!(is_balanced(c), "chunk {i} has an unbalanced fence:\n{c}");
        }
    }

    /// `None` means "this channel opted out of splitting" — headroom must
    /// not silently opt it back in by inventing a cap.
    #[test]
    fn effective_max_passes_none_through() {
        assert_eq!(effective_max(None), None);
    }

    /// Table of the caps adapters actually declare, with the headroom-
    /// adjusted value the splitter should use. Every row must be strictly
    /// below its input, or the margin is not doing anything.
    #[test]
    fn effective_max_reserves_ten_percent_of_real_caps() {
        // (declared cap, expected effective cap)
        let cases = [
            (280usize, 252usize), // x
            (600, 540),           // wechat
            (2000, 1800),         // discord (2000), signal (2000)
            (4000, 3600),         // mattermost, imessage
            (4096, 3686),         // telegram, gchat, whatsapp-cloud
            (5000, 4500),         // line
            (7439, 6695),         // webex
            (28000, 25200),       // teams
            (40000, 36000),       // slack
            (65536, 58982),       // github
        ];
        for (declared, expected) in cases {
            let got = effective_max(Some(declared)).expect("Some in, Some out");
            assert_eq!(got, expected, "headroom for cap {declared}");
            assert!(got < declared, "cap {declared} must actually shrink");
        }
    }

    /// A tiny or degenerate cap must still yield a usable, non-zero
    /// window — a `Some(0)` would make the greedy splitter spin forever.
    #[test]
    fn effective_max_never_returns_zero() {
        for declared in [0usize, 1, 2, 5, 10, 11] {
            let got = effective_max(Some(declared)).expect("Some in, Some out");
            assert!(got >= 1, "cap {declared} produced a zero-width window");
        }
        assert_eq!(effective_max(Some(10)), Some(9));
        assert_eq!(effective_max(Some(100)), Some(90));
    }

    /// The overflow-safe arithmetic must agree with the naive formula
    /// everywhere it does not overflow, and must not panic where it would.
    #[test]
    fn effective_max_matches_naive_formula_and_survives_huge_caps() {
        for declared in [1usize, 7, 99, 101, 999, 4096, 123_457] {
            assert_eq!(
                effective_max(Some(declared)),
                Some((declared * 90 / 100).max(1)),
                "mismatch at {declared}"
            );
        }
        // Would overflow under `m * 90`; must still be sane and in range.
        let huge = effective_max(Some(usize::MAX)).expect("Some in, Some out");
        assert!(huge < usize::MAX && huge > usize::MAX / 2);
    }

    /// The headroom exists so a rendered chunk still fits. Sanity-check
    /// the motivating case: markdown that expands under HTML escaping.
    #[test]
    fn effective_max_absorbs_html_escape_expansion() {
        let cap = 4096;
        let eff = effective_max(Some(cap)).expect("Some in, Some out");
        // Worst realistic expansion for the escape set is `&` -> `&amp;`
        // (5x) on a minority of chars; a body that is 5% ampersands grows
        // by 20%... but a typical reply grows well under 10%.
        let chunk = "a".repeat(eff - 100) + &"&".repeat(100);
        let rendered = chunk.replace('&', "&amp;");
        assert!(
            rendered.chars().count() <= cap,
            "rendered {} chars exceeded cap {cap}",
            rendered.chars().count()
        );
    }
}
