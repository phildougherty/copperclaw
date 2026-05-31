//! Minimal HTML to plain-text conversion for Microsoft Teams message bodies.
//!
//! Microsoft Graph returns chat message bodies as HTML by default. This module
//! provides a tiny pure-Rust converter that strips tags, decodes the common
//! named character references, and preserves anchor `href` attributes so the
//! agent sees both the link text and its URL.

/// Convert a Microsoft Teams HTML body to plain text.
///
/// Rules:
/// - Block-level tags like `<p>`, `<div>`, `<br>` introduce newlines.
/// - `<a href="X">text</a>` becomes `text (X)`.
/// - All other tags are stripped.
/// - The five XML entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;` /
///   `&apos;`) plus `&nbsp;` are decoded.
/// - Numeric character references (`&#NN;` and `&#xHH;`) are decoded for
///   the basic ASCII range; everything else is left as-is.
///
/// The function is intentionally not a general-purpose HTML parser; it covers
/// the small set of constructs Microsoft Graph actually emits for chat
/// messages.
#[must_use]
pub fn html_to_text(input: &str) -> String {
    let stripped = strip_tags(input);
    let decoded = decode_entities(&stripped);
    collapse_whitespace(&decoded)
}

fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            // Find end of tag.
            let end = find_byte(bytes, i + 1, b'>');
            let tag_start = i + 1;
            let tag_end = end.unwrap_or(bytes.len());
            let raw = &input[tag_start..tag_end];
            let lower = tag_name_lower(raw);
            match lower.as_str() {
                "br" | "br/" | "p" | "/p" | "div" | "/div" | "li" | "/li" => {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                }
                _ => {
                    if let Some(href) = anchor_href(raw) {
                        // Track href in the inner text once we hit </a>.
                        // We emit the inner text untagged and then append href.
                        // To keep this single-pass, attach a marker that
                        // we'll resolve below: emit a sentinel `\x00HREF\x01`.
                        out.push('\x00');
                        out.push_str(&href);
                        out.push('\x01');
                    } else if lower == "/a" {
                        // Closing anchor — append the held href in `( ... )`.
                        if let Some(pos) = out.rfind('\x00') {
                            if let Some(end) = out[pos..].find('\x01') {
                                let href = out[pos + 1..pos + end].to_owned();
                                out.replace_range(pos..=pos + end, "");
                                // Only add the parenthetical if href non-empty
                                // and not already present.
                                if !href.is_empty() {
                                    out.push_str(" (");
                                    out.push_str(&href);
                                    out.push(')');
                                }
                            }
                        }
                    }
                }
            }
            i = match end {
                Some(e) => e + 1,
                None => bytes.len(),
            };
        } else {
            out.push(b as char);
            i += 1;
        }
    }
    // Drop any unclosed anchor sentinels.
    let mut cleaned = String::with_capacity(out.len());
    let mut skip = false;
    for c in out.chars() {
        if c == '\x00' {
            skip = true;
            continue;
        }
        if c == '\x01' {
            skip = false;
            continue;
        }
        if !skip {
            cleaned.push(c);
        }
    }
    cleaned
}

fn find_byte(bytes: &[u8], from: usize, target: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == target)
        .map(|p| p + from)
}

/// Extract the lowercased tag name (without attributes) from the raw inner
/// portion of a `<...>` element, e.g. `p class="x"` -> `"p"`.
fn tag_name_lower(raw: &str) -> String {
    let trimmed = raw.trim();
    let name: String = trimmed
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    name.to_ascii_lowercase()
}

/// If the raw tag is an `<a>` element, return the `href` attribute value.
fn anchor_href(raw: &str) -> Option<String> {
    let lower = tag_name_lower(raw);
    if lower != "a" {
        return None;
    }
    // Find href="..." or href='...'.
    let lower_full = raw.to_ascii_lowercase();
    let idx = lower_full.find("href")?;
    let rest = &raw[idx + 4..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_owned())
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        let end = stripped.find('\'')?;
        Some(stripped[..end].to_owned())
    } else {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        if end == 0 {
            Some(String::new())
        } else {
            Some(rest[..end].to_owned())
        }
    }
}

fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = find_byte(bytes, i, b';') {
                let entity = &input[i + 1..semi];
                if let Some(decoded) = decode_named_or_numeric(entity) {
                    out.push_str(&decoded);
                    i = semi + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn decode_named_or_numeric(entity: &str) -> Option<String> {
    match entity {
        "amp" => Some("&".into()),
        "lt" => Some("<".into()),
        "gt" => Some(">".into()),
        "quot" => Some("\"".into()),
        "apos" => Some("'".into()),
        "nbsp" => Some(" ".into()),
        _ => {
            if let Some(num) = entity.strip_prefix('#') {
                let n = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    num.parse::<u32>().ok()?
                };
                if let Some(c) = char::from_u32(n) {
                    return Some(c.to_string());
                }
            }
            None
        }
    }
}

fn collapse_whitespace(input: &str) -> String {
    // Trim trailing spaces on each line and collapse multiple blank lines.
    let mut out = String::with_capacity(input.len());
    for line in input.split('\n') {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_simple_paragraph() {
        let html = "<p>Hello world</p>";
        assert_eq!(html_to_text(html), "Hello world");
    }

    #[test]
    fn paragraph_separator_inserts_newline() {
        let html = "<p>first</p><p>second</p>";
        assert_eq!(html_to_text(html), "first\nsecond");
    }

    #[test]
    fn br_becomes_newline() {
        let html = "line one<br>line two";
        assert_eq!(html_to_text(html), "line one\nline two");
    }

    #[test]
    fn self_closing_br_also_newlines() {
        let html = "line one<br/>line two";
        assert_eq!(html_to_text(html), "line one\nline two");
    }

    #[test]
    fn anchor_emits_text_and_href() {
        let html = r#"see <a href="https://example.com">example</a>."#;
        assert_eq!(html_to_text(html), "see example (https://example.com).");
    }

    #[test]
    fn anchor_with_single_quotes() {
        let html = r"<a href='https://x.test'>x</a>";
        assert_eq!(html_to_text(html), "x (https://x.test)");
    }

    #[test]
    fn anchor_without_quotes() {
        let html = "<a href=https://x.test>x</a>";
        assert_eq!(html_to_text(html), "x (https://x.test)");
    }

    #[test]
    fn anchor_without_href_strips_tag() {
        let html = "<a>x</a>";
        assert_eq!(html_to_text(html), "x");
    }

    #[test]
    fn decodes_amp_lt_gt() {
        let html = "<p>a &amp; b &lt; c &gt; d</p>";
        assert_eq!(html_to_text(html), "a & b < c > d");
    }

    #[test]
    fn decodes_quot_and_apos() {
        let html = "<p>&quot;hi&quot; &#39;there&#39;</p>";
        assert_eq!(html_to_text(html), "\"hi\" 'there'");
    }

    #[test]
    fn decodes_nbsp() {
        let html = "<p>a&nbsp;b</p>";
        assert_eq!(html_to_text(html), "a b");
    }

    #[test]
    fn decodes_numeric_entity() {
        let html = "<p>&#65;&#x42;</p>";
        assert_eq!(html_to_text(html), "AB");
    }

    #[test]
    fn unknown_entity_passed_through() {
        let html = "<p>foo &xyz; bar</p>";
        assert_eq!(html_to_text(html), "foo &xyz; bar");
    }

    #[test]
    fn nested_tags_are_stripped() {
        let html = "<div><p><strong>bold</strong></p></div>";
        assert_eq!(html_to_text(html), "bold");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(html_to_text(""), "");
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(html_to_text("just text"), "just text");
    }

    #[test]
    fn unmatched_open_tag_does_not_panic() {
        let html = "<p>oops";
        // Behavior: we strip the unterminated tag and keep the text.
        let out = html_to_text(html);
        assert!(out.contains("oops"));
    }

    #[test]
    fn list_items_create_newlines() {
        let html = "<ul><li>a</li><li>b</li></ul>";
        assert_eq!(html_to_text(html), "a\nb");
    }

    #[test]
    fn anchor_without_closing_tag_does_not_panic() {
        let html = r#"<a href="https://x.test">incomplete"#;
        let out = html_to_text(html);
        assert!(out.contains("incomplete"));
    }

    #[test]
    fn collapses_trailing_whitespace_on_lines() {
        let html = "<p>line1   </p><p>line2</p>";
        assert_eq!(html_to_text(html), "line1\nline2");
    }
}
