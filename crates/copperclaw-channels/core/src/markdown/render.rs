//! Canonical-markdown → per-platform flavor renderer.
//!
//! The agent writes one canonical Markdown dialect; every chat platform
//! speaks a slightly different one. This module is the single place that
//! translation lives, so adapters stop each hand-rolling their own
//! (Telegram HTML, Slack `mrkdwn`, Discord/Mattermost `CommonMark`,
//! `WhatsApp`'s `*bold*`, Signal plaintext). Give it canonical Markdown and
//! a [`Flavor`]; get back the platform's on-the-wire text.
//!
//! Supported constructs: ATX headings (`#`..`######`), fenced code
//! blocks, inline code, bold (`**`/`__`), italic (`*`/`_`),
//! strikethrough (`~~`), links (`[text](url)`), unordered / ordered
//! lists, and blockquotes. Unrecognised text passes through (HTML-escaped
//! for the [`Flavor::Html`] target). The renderer is deliberately
//! forgiving: an unbalanced marker is emitted as the literal character
//! rather than swallowing the rest of the line, matching how the
//! adapters' bespoke formatters behave on natural-language prose.

/// The per-platform Markdown dialect to render into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// Telegram `parse_mode=HTML`: `<b>`/`<i>`/`<s>`/`<code>`/`<pre>`,
    /// `<a href>`, everything else HTML-escaped.
    Html,
    /// Discord: `CommonMark` (native `**bold**`, `# headings`, fences).
    Discord,
    /// Slack `mrkdwn`: `*bold*`, `_italic_`, `~strike~`, `<url|text>`,
    /// no heading syntax (headings degrade to bold).
    Slack,
    /// Mattermost: full `CommonMark`, same as [`Flavor::Discord`] for the
    /// constructs here (kept distinct so callers name their platform).
    Mattermost,
    /// `WhatsApp`: `*bold*`, `_italic_`, `~strike~`, ```` ``` ```` monospace,
    /// no heading or link syntax.
    WhatsApp,
    /// Signal and other plaintext surfaces: strip all markup.
    Plain,
}

/// Render canonical `markdown` into the on-the-wire text for `flavor`.
pub fn render(markdown: &str, flavor: Flavor) -> String {
    let lines: Vec<&str> = markdown.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        if let Some(info) = fence_marker(lines[i]) {
            // Opening fence: gather body until a bare closing marker (a
            // marker line carrying an info string is content, per
            // CommonMark's no-info-on-close rule).
            let mut j = i + 1;
            let mut body: Vec<&str> = Vec::new();
            while j < lines.len() {
                if fence_marker(lines[j]).is_some_and(str::is_empty) {
                    j += 1;
                    break;
                }
                body.push(lines[j]);
                j += 1;
            }
            render_code_block(&mut out, info, &body, flavor);
            i = j;
            continue;
        }
        render_text_line(&mut out, lines[i], flavor);
        i += 1;
    }
    out.join("\n")
}

/// If `line` is a fence marker (≤3 leading spaces then ≥3 backticks),
/// return the trimmed info string (empty for a bare / closing fence).
fn fence_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let ticks = trimmed.chars().take_while(|&c| c == '`').count();
    if ticks < 3 {
        return None;
    }
    Some(trimmed[ticks..].trim())
}

fn render_code_block(out: &mut Vec<String>, info: &str, body: &[&str], flavor: Flavor) {
    match flavor {
        Flavor::Html => {
            let mut block = String::from("<pre><code");
            if !info.is_empty() {
                block.push_str(&format!(" class=\"language-{}\"", escape_html(info)));
            }
            block.push('>');
            block.push_str(&escape_html(&body.join("\n")));
            block.push_str("</code></pre>");
            out.push(block);
        }
        Flavor::Plain => {
            for line in body {
                out.push((*line).to_string());
            }
        }
        // CommonMark carries the info string; mrkdwn / WhatsApp fences
        // ignore any language, so drop it there.
        Flavor::Discord | Flavor::Mattermost => {
            out.push(format!("```{info}"));
            for line in body {
                out.push((*line).to_string());
            }
            out.push("```".to_string());
        }
        Flavor::Slack | Flavor::WhatsApp => {
            out.push("```".to_string());
            for line in body {
                out.push((*line).to_string());
            }
            out.push("```".to_string());
        }
    }
}

fn render_text_line(out: &mut Vec<String>, line: &str, flavor: Flavor) {
    let indent_len = line.len() - line.trim_start_matches(' ').len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];

    if let Some((level, content)) = heading(rest) {
        out.push(render_heading(level, content, flavor));
        return;
    }
    if let Some(content) = unordered_item(rest) {
        let bullet = match flavor {
            Flavor::Discord | Flavor::Mattermost => "- ",
            _ => "\u{2022} ", // • bullet glyph
        };
        let mut s = String::from(indent);
        s.push_str(bullet);
        render_inline(&parse_inline(content), flavor, &mut s);
        out.push(s);
        return;
    }
    if let Some((marker, content)) = ordered_item(rest) {
        // Ordered lists are universally understood as `N.`; keep it.
        let mut s = String::from(indent);
        s.push_str(marker);
        s.push(' ');
        render_inline(&parse_inline(content), flavor, &mut s);
        out.push(s);
        return;
    }
    if let Some(content) = blockquote(rest) {
        out.push(render_blockquote(content, flavor));
        return;
    }
    // Plain paragraph line: preserve leading indent, render inline.
    let mut s = String::from(indent);
    render_inline(&parse_inline(rest), flavor, &mut s);
    out.push(s);
}

/// `#`..`######` then a space → `(level, content)`.
fn heading(rest: &str) -> Option<(usize, &str)> {
    let hashes = rest.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
        Some((hashes, rest[hashes..].trim_start()))
    } else {
        None
    }
}

fn render_heading(level: usize, content: &str, flavor: Flavor) -> String {
    let nodes = parse_inline(content);
    match flavor {
        Flavor::Html => {
            let mut s = String::from("<b>");
            render_inline(&nodes, flavor, &mut s);
            s.push_str("</b>");
            s
        }
        Flavor::Discord | Flavor::Mattermost => {
            let mut s = "#".repeat(level);
            s.push(' ');
            render_inline(&nodes, flavor, &mut s);
            s
        }
        // No heading syntax on Slack / WhatsApp — degrade to bold.
        Flavor::Slack | Flavor::WhatsApp => {
            let (open, close) = bold_delims(flavor);
            let mut s = String::from(open);
            render_inline(&nodes, flavor, &mut s);
            s.push_str(close);
            s
        }
        Flavor::Plain => {
            let mut s = String::new();
            render_inline(&nodes, flavor, &mut s);
            s
        }
    }
}

/// `- ` / `* ` / `+ ` → the item content.
fn unordered_item(rest: &str) -> Option<&str> {
    let mut chars = rest.chars();
    match chars.next() {
        Some('-' | '*' | '+') if rest[1..].starts_with(' ') => Some(rest[1..].trim_start()),
        _ => None,
    }
}

/// `<digits>. ` → the `N.` marker and the item content.
fn ordered_item(rest: &str) -> Option<(&str, &str)> {
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let after = &rest[digits..];
    after
        .strip_prefix(". ")
        .map(|content| (&rest[..=digits], content.trim_start()))
}

/// `> ` (or bare `>`) → the quoted content.
fn blockquote(rest: &str) -> Option<&str> {
    rest.strip_prefix('>').map(str::trim_start)
}

fn render_blockquote(content: &str, flavor: Flavor) -> String {
    let nodes = parse_inline(content);
    if flavor == Flavor::Html {
        let mut s = String::from("<blockquote>");
        render_inline(&nodes, flavor, &mut s);
        s.push_str("</blockquote>");
        s
    } else {
        let mut s = String::from("> ");
        render_inline(&nodes, flavor, &mut s);
        s
    }
}

// ---- inline model ---------------------------------------------------------

enum Inline {
    Text(String),
    Code(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Strike(Vec<Inline>),
    Link { text: Vec<Inline>, url: String },
}

fn parse_inline(s: &str) -> Vec<Inline> {
    let chars: Vec<char> = s.chars().collect();
    parse_span(&chars)
}

fn parse_span(chars: &[char]) -> Vec<Inline> {
    let mut nodes: Vec<Inline> = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Inline code — highest precedence, never formatted inside.
        if c == '`' {
            if let Some(close) = find_char(chars, i + 1, '`') {
                flush(&mut nodes, &mut text);
                nodes.push(Inline::Code(chars[i + 1..close].iter().collect()));
                i = close + 1;
                continue;
            }
        }
        // Bold: ** or __.
        if (c == '*' || c == '_') && chars.get(i + 1) == Some(&c) {
            if let Some(close) = find_double(chars, i + 2, c) {
                flush(&mut nodes, &mut text);
                nodes.push(Inline::Bold(parse_span(&chars[i + 2..close])));
                i = close + 2;
                continue;
            }
        }
        // Strikethrough: ~~.
        if c == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(close) = find_double(chars, i + 2, '~') {
                flush(&mut nodes, &mut text);
                nodes.push(Inline::Strike(parse_span(&chars[i + 2..close])));
                i = close + 2;
                continue;
            }
        }
        // Italic: single * or _.
        if c == '*' || c == '_' {
            if let Some(close) = find_italic_close(chars, i, c) {
                flush(&mut nodes, &mut text);
                nodes.push(Inline::Italic(parse_span(&chars[i + 1..close])));
                i = close + 1;
                continue;
            }
        }
        // Link: [text](url).
        if c == '[' {
            if let Some((text_end, url_start, url_end)) = parse_link(chars, i) {
                flush(&mut nodes, &mut text);
                let inner = parse_span(&chars[i + 1..text_end]);
                let url: String = chars[url_start..url_end].iter().collect();
                nodes.push(Inline::Link { text: inner, url });
                i = url_end + 1;
                continue;
            }
        }
        text.push(c);
        i += 1;
    }
    flush(&mut nodes, &mut text);
    nodes
}

fn flush(nodes: &mut Vec<Inline>, text: &mut String) {
    if !text.is_empty() {
        nodes.push(Inline::Text(std::mem::take(text)));
    }
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == target)
}

/// Index of the first `dd` pair at or after `from`.
fn find_double(chars: &[char], from: usize, d: char) -> Option<usize> {
    if from >= chars.len() {
        return None;
    }
    (from..chars.len() - 1).find(|&i| chars[i] == d && chars[i + 1] == d)
}

/// Closing index for a single-char emphasis opened at `open` with
/// delimiter `d`. Conservative: the opener must be left-flanking (next
/// char present and non-space), the closer right-flanking (prev char
/// non-space), neither may be part of a `dd` pair, and `_` emphasis must
/// sit on word boundaries so `foo_bar_baz` stays literal.
fn find_italic_close(chars: &[char], open: usize, d: char) -> Option<usize> {
    let next = *chars.get(open + 1)?;
    if next.is_whitespace() || next == d {
        return None;
    }
    if d == '_' && open > 0 && chars[open - 1].is_alphanumeric() {
        return None;
    }
    let mut j = open + 1;
    while j < chars.len() {
        if chars[j] == d {
            let prev_ok = !chars[j - 1].is_whitespace() && chars[j - 1] != d;
            let not_double = chars.get(j + 1) != Some(&d);
            let word_ok = d != '_' || chars.get(j + 1).is_none_or(|c| !c.is_alphanumeric());
            if prev_ok && not_double && word_ok {
                return Some(j);
            }
        }
        j += 1;
    }
    None
}

/// Parse a `[text](url)` link starting at `open` (a `[`). Returns
/// `(text_end, url_start, url_end)` — the `]`, the char after `(`, and
/// the `)` — or `None` if the shape doesn't match.
fn parse_link(chars: &[char], open: usize) -> Option<(usize, usize, usize)> {
    let rb = find_char(chars, open + 1, ']')?;
    if chars.get(rb + 1) != Some(&'(') {
        return None;
    }
    let url_start = rb + 2;
    let rp = find_char(chars, url_start, ')')?;
    Some((rb, url_start, rp))
}

// ---- inline rendering -----------------------------------------------------

fn render_inline(nodes: &[Inline], flavor: Flavor, out: &mut String) {
    for node in nodes {
        match node {
            Inline::Text(t) => out.push_str(&render_text(t, flavor)),
            Inline::Code(c) => render_code(c, flavor, out),
            Inline::Bold(inner) => wrap(inner, flavor, bold_delims(flavor), out),
            Inline::Italic(inner) => wrap(inner, flavor, italic_delims(flavor), out),
            Inline::Strike(inner) => wrap(inner, flavor, strike_delims(flavor), out),
            Inline::Link { text, url } => render_link(text, url, flavor, out),
        }
    }
}

fn wrap(inner: &[Inline], flavor: Flavor, delims: (&str, &str), out: &mut String) {
    out.push_str(delims.0);
    render_inline(inner, flavor, out);
    out.push_str(delims.1);
}

fn render_text(t: &str, flavor: Flavor) -> String {
    match flavor {
        Flavor::Html => escape_html(t),
        _ => t.to_string(),
    }
}

fn render_code(code: &str, flavor: Flavor, out: &mut String) {
    match flavor {
        Flavor::Html => {
            out.push_str("<code>");
            out.push_str(&escape_html(code));
            out.push_str("</code>");
        }
        Flavor::Plain => out.push_str(code),
        _ => {
            out.push('`');
            out.push_str(code);
            out.push('`');
        }
    }
}

fn render_link(text: &[Inline], url: &str, flavor: Flavor, out: &mut String) {
    match flavor {
        Flavor::Html => {
            out.push_str(&format!("<a href=\"{}\">", escape_html(url)));
            render_inline(text, flavor, out);
            out.push_str("</a>");
        }
        Flavor::Discord | Flavor::Mattermost => {
            out.push('[');
            render_inline(text, flavor, out);
            out.push_str("](");
            out.push_str(url);
            out.push(')');
        }
        Flavor::Slack => {
            // mrkdwn link: <url|text>; link text carries no formatting.
            out.push('<');
            out.push_str(url);
            out.push('|');
            out.push_str(&flatten(text));
            out.push('>');
        }
        // WhatsApp / Plain have no link syntax: "text (url)", or bare url
        // when the visible text already is the url.
        Flavor::WhatsApp | Flavor::Plain => {
            let label = flatten(text);
            if label.is_empty() || label == url {
                out.push_str(url);
            } else {
                out.push_str(&format!("{label} ({url})"));
            }
        }
    }
}

/// Render nested inline nodes as bare plaintext (no delimiters).
fn flatten(nodes: &[Inline]) -> String {
    let mut s = String::new();
    render_inline(nodes, Flavor::Plain, &mut s);
    s
}

fn bold_delims(flavor: Flavor) -> (&'static str, &'static str) {
    match flavor {
        Flavor::Html => ("<b>", "</b>"),
        Flavor::Discord | Flavor::Mattermost => ("**", "**"),
        Flavor::Slack | Flavor::WhatsApp => ("*", "*"),
        Flavor::Plain => ("", ""),
    }
}

fn italic_delims(flavor: Flavor) -> (&'static str, &'static str) {
    match flavor {
        Flavor::Html => ("<i>", "</i>"),
        Flavor::Discord | Flavor::Mattermost => ("*", "*"),
        Flavor::Slack | Flavor::WhatsApp => ("_", "_"),
        Flavor::Plain => ("", ""),
    }
}

fn strike_delims(flavor: Flavor) -> (&'static str, &'static str) {
    match flavor {
        Flavor::Html => ("<s>", "</s>"),
        Flavor::Discord | Flavor::Mattermost => ("~~", "~~"),
        Flavor::Slack | Flavor::WhatsApp => ("~", "~"),
        Flavor::Plain => ("", ""),
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the per-platform output table (headings/bold/code/lists) -------

    #[test]
    fn heading_per_platform() {
        let md = "# Prototype ready";
        assert_eq!(render(md, Flavor::Html), "<b>Prototype ready</b>");
        assert_eq!(render(md, Flavor::Discord), "# Prototype ready");
        assert_eq!(render(md, Flavor::Mattermost), "# Prototype ready");
        assert_eq!(render(md, Flavor::Slack), "*Prototype ready*");
        assert_eq!(render(md, Flavor::WhatsApp), "*Prototype ready*");
        assert_eq!(render(md, Flavor::Plain), "Prototype ready");
    }

    #[test]
    fn nested_heading_level_preserved_in_commonmark() {
        assert_eq!(render("### Sub", Flavor::Discord), "### Sub");
        assert_eq!(render("###### Deep", Flavor::Mattermost), "###### Deep");
        // Seven hashes is not a heading.
        assert_eq!(render("####### x", Flavor::Discord), "####### x");
    }

    #[test]
    fn bold_per_platform() {
        let md = "say **hello** now";
        assert_eq!(render(md, Flavor::Html), "say <b>hello</b> now");
        assert_eq!(render(md, Flavor::Discord), "say **hello** now");
        assert_eq!(render(md, Flavor::Slack), "say *hello* now");
        assert_eq!(render(md, Flavor::WhatsApp), "say *hello* now");
        assert_eq!(render(md, Flavor::Plain), "say hello now");
        // __x__ is also bold.
        assert_eq!(render("__hi__", Flavor::Slack), "*hi*");
    }

    #[test]
    fn italic_per_platform() {
        let md = "an *emphasised* word";
        assert_eq!(render(md, Flavor::Html), "an <i>emphasised</i> word");
        assert_eq!(render(md, Flavor::Discord), "an *emphasised* word");
        assert_eq!(render(md, Flavor::Slack), "an _emphasised_ word");
        assert_eq!(render(md, Flavor::Plain), "an emphasised word");
        // Underscores inside a word are literal, not italic.
        assert_eq!(render("foo_bar_baz", Flavor::Slack), "foo_bar_baz");
    }

    #[test]
    fn strikethrough_per_platform() {
        let md = "~~gone~~";
        assert_eq!(render(md, Flavor::Html), "<s>gone</s>");
        assert_eq!(render(md, Flavor::Discord), "~~gone~~");
        assert_eq!(render(md, Flavor::Slack), "~gone~");
        assert_eq!(render(md, Flavor::WhatsApp), "~gone~");
        assert_eq!(render(md, Flavor::Plain), "gone");
    }

    #[test]
    fn inline_code_per_platform() {
        let md = "run `cargo test` please";
        assert_eq!(
            render(md, Flavor::Html),
            "run <code>cargo test</code> please"
        );
        assert_eq!(render(md, Flavor::Discord), "run `cargo test` please");
        assert_eq!(render(md, Flavor::Slack), "run `cargo test` please");
        assert_eq!(render(md, Flavor::Plain), "run cargo test please");
        // Markup inside code is inert.
        assert_eq!(render("`**not bold**`", Flavor::Slack), "`**not bold**`");
    }

    #[test]
    fn fenced_code_block_per_platform() {
        let md = "```rust\nlet x = 1;\nlet y = 2;\n```";
        assert_eq!(
            render(md, Flavor::Html),
            "<pre><code class=\"language-rust\">let x = 1;\nlet y = 2;</code></pre>"
        );
        assert_eq!(
            render(md, Flavor::Discord),
            "```rust\nlet x = 1;\nlet y = 2;\n```"
        );
        // mrkdwn / WhatsApp fences drop the language.
        assert_eq!(
            render(md, Flavor::Slack),
            "```\nlet x = 1;\nlet y = 2;\n```"
        );
        assert_eq!(
            render(md, Flavor::WhatsApp),
            "```\nlet x = 1;\nlet y = 2;\n```"
        );
        assert_eq!(render(md, Flavor::Plain), "let x = 1;\nlet y = 2;");
    }

    #[test]
    fn unordered_list_per_platform() {
        let md = "- one\n- two";
        assert_eq!(render(md, Flavor::Discord), "- one\n- two");
        assert_eq!(render(md, Flavor::Mattermost), "- one\n- two");
        assert_eq!(render(md, Flavor::Slack), "\u{2022} one\n\u{2022} two");
        assert_eq!(render(md, Flavor::WhatsApp), "\u{2022} one\n\u{2022} two");
        assert_eq!(render(md, Flavor::Html), "\u{2022} one\n\u{2022} two");
        // `*` and `+` bullets normalise too.
        assert_eq!(render("* star", Flavor::Slack), "\u{2022} star");
        assert_eq!(render("+ plus", Flavor::Discord), "- plus");
    }

    #[test]
    fn ordered_list_kept_across_platforms() {
        let md = "1. first\n2. second";
        for f in [Flavor::Html, Flavor::Slack, Flavor::Discord, Flavor::Plain] {
            assert_eq!(render(md, f), "1. first\n2. second", "{f:?}");
        }
    }

    #[test]
    fn nested_list_indent_preserved() {
        let md = "- top\n  - nested";
        assert_eq!(render(md, Flavor::Slack), "\u{2022} top\n  \u{2022} nested");
        assert_eq!(render(md, Flavor::Discord), "- top\n  - nested");
    }

    #[test]
    fn list_item_carries_inline_formatting() {
        let md = "- run `x` and **go**";
        assert_eq!(render(md, Flavor::Slack), "\u{2022} run `x` and *go*");
        assert_eq!(
            render(md, Flavor::Html),
            "\u{2022} run <code>x</code> and <b>go</b>"
        );
    }

    // ---- links, quotes, escaping, robustness ----------------------------

    #[test]
    fn link_per_platform() {
        let md = "[docs](https://example.com)";
        assert_eq!(
            render(md, Flavor::Html),
            "<a href=\"https://example.com\">docs</a>"
        );
        assert_eq!(render(md, Flavor::Discord), "[docs](https://example.com)");
        assert_eq!(render(md, Flavor::Slack), "<https://example.com|docs>");
        assert_eq!(render(md, Flavor::WhatsApp), "docs (https://example.com)");
        assert_eq!(render(md, Flavor::Plain), "docs (https://example.com)");
    }

    #[test]
    fn blockquote_per_platform() {
        assert_eq!(
            render("> hush", Flavor::Html),
            "<blockquote>hush</blockquote>"
        );
        assert_eq!(render("> hush", Flavor::Discord), "> hush");
        assert_eq!(render("> hush", Flavor::Slack), "> hush");
    }

    #[test]
    fn html_escapes_special_chars_outside_code() {
        assert_eq!(
            render("a < b & c > d", Flavor::Html),
            "a &lt; b &amp; c &gt; d"
        );
        // But CommonMark / plaintext flavors leave them untouched.
        assert_eq!(render("a < b & c", Flavor::Discord), "a < b & c");
    }

    #[test]
    fn unbalanced_markers_are_literal() {
        assert_eq!(render("**oops no close", Flavor::Slack), "**oops no close");
        assert_eq!(
            render("a `dangling backtick", Flavor::Html),
            "a `dangling backtick"
        );
        assert_eq!(
            render("[text](no-close", Flavor::Discord),
            "[text](no-close"
        );
    }

    #[test]
    fn multi_paragraph_and_blank_lines_preserved() {
        let md = "para one\n\npara two";
        assert_eq!(render(md, Flavor::Plain), "para one\n\npara two");
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(render("", Flavor::Slack), "");
    }
}
