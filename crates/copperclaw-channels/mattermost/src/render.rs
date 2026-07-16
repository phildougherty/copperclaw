//! Markdown renderers that raise Mattermost to the rich-surface floor.
//!
//! Mattermost renders full `CommonMark` (headings, fenced code, task
//! lists, strikethrough, blockquotes), so each portable card type maps
//! to an idiomatic Markdown block the desktop / web / mobile clients
//! render natively. Every renderer is a pure `&T -> String` function so
//! the adapter's `deliver_*` overrides stay thin and the formatting is
//! unit-testable without a mock server.
//!
//! The agent's own `body` / `text` fields are markdown-intended and pass
//! through verbatim; only structural scaffolding (titles, field labels,
//! glyphs, fences) is added here.

use copperclaw_channels_core::{
    Card, DiffCard, ErrorCard, ThinkingBlock, TodoItemStatus, TodoList,
};

/// Render a [`Card`] as a Mattermost Markdown post: an `###` heading for
/// the title, the body paragraph, a `**Label:** value` list for fields,
/// a bulleted button list (URL buttons become Markdown links; callback
/// buttons render as labelled bullets since Mattermost has no wired
/// interactive-button round-trip), and an inline `![](url)` image.
pub fn render_card(card: &Card) -> String {
    let mut out = String::new();
    if let Some(t) = card
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        out.push_str("### ");
        out.push_str(t);
        out.push('\n');
    }
    if let Some(b) = card
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(b);
        out.push('\n');
    }
    if !card.fields.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        for f in &card.fields {
            out.push_str("**");
            out.push_str(f.label.trim());
            out.push_str(":** ");
            out.push_str(f.value.trim());
            out.push('\n');
        }
    }
    if !card.buttons.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        for b in &card.buttons {
            out.push_str("- ");
            match (b.value.as_deref(), b.url.as_deref()) {
                (_, Some(url)) => {
                    out.push('[');
                    out.push_str(b.label.trim());
                    out.push_str("](");
                    out.push_str(url.trim());
                    out.push(')');
                }
                (Some(_) | None, None) => out.push_str(b.label.trim()),
            }
            out.push('\n');
        }
    }
    if let Some(img) = card
        .image_url
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty())
    {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("![](");
        out.push_str(img);
        out.push_str(")\n");
    }
    trim_trailing_newlines(&mut out);
    out
}

/// Render a [`DiffCard`] as a fenced ` ```diff ` block with a bold
/// path + `(+adds / -removes)` header. Mattermost highlights the `diff`
/// language, colouring `+` / `-` gutters.
pub fn render_diff(diff: &DiffCard) -> String {
    let mut out = String::with_capacity(64 + diff.hunks.len() * 48);
    out.push_str("**");
    out.push_str(diff.path.trim());
    out.push_str("** (+");
    out.push_str(&diff.added.to_string());
    out.push_str(" / -");
    out.push_str(&diff.removed.to_string());
    if diff.truncated {
        out.push_str(", truncated");
    }
    out.push_str(")\n```diff\n");
    out.push_str(&unified_hunks(diff));
    out.push_str("```");
    out
}

/// Render the slice-3.4 long-output expander. Mattermost has no
/// disclosure widget, so we surface the host one-liner summary in bold,
/// then the preview lines inside a fenced block with a `…(N more)`
/// truncation marker — the same collapsed shape as the trait fallback,
/// Markdown-styled. The full body stays on disk (this is the on-the-wire
/// shape only).
pub fn render_collapsible(text: &str, summary: &str, preview_lines: &[String]) -> String {
    let total_lines = text.lines().count();
    let remaining = total_lines.saturating_sub(preview_lines.len());
    let mut out = String::with_capacity(summary.len() + 64);
    out.push_str("**");
    out.push_str(summary.trim());
    out.push_str("**");
    if !preview_lines.is_empty() || remaining > 0 {
        out.push_str("\n```\n");
        for line in preview_lines {
            out.push_str(line);
            out.push('\n');
        }
        if remaining > 0 {
            out.push_str(&format!("…({remaining} more lines)\n"));
        }
        out.push_str("```");
    }
    out
}

/// Render a [`TodoList`] as a Markdown task list — a bold title with a
/// `done/total` counter, then `- [x] ~~text~~` for completed items,
/// `- [ ] text` for pending, and `- [ ] text _(in progress)_` for the
/// active item (Mattermost has no half-checked glyph).
pub fn render_todo_list(list: &TodoList) -> String {
    let done = list.completed_count();
    let total = list.items.len();
    let mut out = String::with_capacity(64 + list.items.len() * 32);
    out.push_str("**");
    out.push_str(list.title_or_default());
    out.push_str(&format!("** ({done}/{total})"));
    for item in &list.items {
        out.push('\n');
        let text = item.text.trim();
        match item.status {
            TodoItemStatus::Completed => {
                out.push_str("- [x] ~~");
                out.push_str(text);
                out.push_str("~~");
            }
            TodoItemStatus::InProgress => {
                out.push_str("- [ ] ");
                out.push_str(text);
                out.push_str(" _(in progress)_");
            }
            TodoItemStatus::Blocked => {
                out.push_str("- [!] ");
                out.push_str(text);
                match item.blocked_reason_text() {
                    Some(reason) => {
                        out.push_str(" _(blocked: ");
                        out.push_str(reason);
                        out.push_str(")_");
                    }
                    None => out.push_str(" _(blocked)_"),
                }
            }
            TodoItemStatus::Pending => {
                out.push_str("- [ ] ");
                out.push_str(text);
            }
        }
    }
    out
}

/// Render a [`ThinkingBlock`] as a Markdown blockquote headed
/// `> reasoning (model)`. Redacted blocks emit the placeholder — the raw
/// opaque blob never reaches the wire.
pub fn render_thinking(thinking: &ThinkingBlock) -> String {
    let header = match thinking.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => format!("> reasoning ({m})"),
        _ => "> reasoning".to_string(),
    };
    if thinking.redacted {
        return format!("{header}\n> (redacted reasoning)");
    }
    let mut out = header;
    for line in thinking.text.lines() {
        out.push_str("\n> ");
        out.push_str(line);
    }
    out
}

/// Render an [`ErrorCard`]. Mattermost plain posts carry no colour
/// affordance, so the severity rides a bold `[ERROR: kind] title`
/// header, with the optional detail in a fenced block and an italic
/// retry footer.
pub fn render_error(err: &ErrorCard) -> String {
    let mut out = String::with_capacity(96 + err.summary.len());
    out.push_str("**[ERROR: ");
    out.push_str(err.kind.label());
    out.push_str("] ");
    out.push_str(err.title.trim());
    out.push_str("**\n");
    out.push_str(err.summary.trim());
    if let Some(d) = err
        .details
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        out.push_str("\n```\n");
        out.push_str(d);
        out.push_str("\n```");
    }
    if err.retryable {
        out.push_str("\n_(will retry automatically)_");
    }
    out
}

/// Build the unified-diff hunk body (no `--- a/` / `+++ b/` header, no
/// totals footer) for embedding inside a fenced ` ```diff ` block.
fn unified_hunks(diff: &DiffCard) -> String {
    let mut out = String::with_capacity(diff.hunks.len() * 48);
    for h in &diff.hunks {
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
    out
}

fn trim_trailing_newlines(s: &mut String) {
    while s.ends_with('\n') {
        s.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::{
        CardButton, CardField, DiffHunk, DiffLine, DiffLineKind, ErrorCardKind, TodoListItem,
    };

    fn full_card() -> Card {
        Card {
            title: Some("Prototype ready".into()),
            body: Some("Your todo app is live.".into()),
            fields: vec![CardField {
                label: "Stack".into(),
                value: "Vite + React".into(),
                inline: false,
            }],
            buttons: vec![
                CardButton {
                    label: "Open preview".into(),
                    value: None,
                    url: Some("https://example.com/app".into()),
                    style: None,
                },
                CardButton {
                    label: "Download".into(),
                    value: Some("dl:1".into()),
                    url: None,
                    style: None,
                },
            ],
            image_url: Some("https://example.com/shot.png".into()),
        }
    }

    #[test]
    fn card_renders_heading_body_fields_buttons_image() {
        let out = render_card(&full_card());
        assert!(out.contains("### Prototype ready"));
        assert!(out.contains("Your todo app is live."));
        assert!(out.contains("**Stack:** Vite + React"));
        assert!(out.contains("- [Open preview](https://example.com/app)"));
        assert!(out.contains("- Download"));
        assert!(out.contains("![](https://example.com/shot.png)"));
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn card_title_only() {
        let c = Card {
            title: Some("Just a title".into()),
            ..Card::default()
        };
        assert_eq!(render_card(&c), "### Just a title");
    }

    fn sample_diff() -> DiffCard {
        DiffCard {
            path: "src/main.rs".into(),
            language: Some("rust".into()),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Remove,
                        text: "fn old() {}".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Add,
                        text: "fn new() {}".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        }
    }

    #[test]
    fn diff_renders_fenced_diff_block() {
        let out = render_diff(&sample_diff());
        assert!(out.contains("**src/main.rs** (+1 / -1)"));
        assert!(out.contains("```diff"));
        assert!(out.contains("@@ -1,1 +1,1 @@"));
        assert!(out.contains("-fn old() {}"));
        assert!(out.contains("+fn new() {}"));
        assert!(out.ends_with("```"));
    }

    #[test]
    fn collapsible_bold_summary_and_fenced_preview() {
        let body = (1..=30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview: Vec<String> = (1..=4).map(|i| format!("line {i}")).collect();
        let out = render_collapsible(&body, "shell produced 30 lines", &preview);
        assert!(out.starts_with("**shell produced 30 lines**"));
        assert!(out.contains("line 1"));
        assert!(out.contains("…(26 more lines)"));
        assert!(out.trim_end().ends_with("```"));
    }

    #[test]
    fn todo_list_task_list_markdown() {
        let list = TodoList {
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "Scaffold".into(),
                    status: TodoItemStatus::Completed,
                    blocked_reason: None,
                },
                TodoListItem {
                    id: 2,
                    text: "Wire routes".into(),
                    status: TodoItemStatus::InProgress,
                    blocked_reason: None,
                },
                TodoListItem {
                    id: 3,
                    text: "Deploy".into(),
                    status: TodoItemStatus::Pending,
                    blocked_reason: None,
                },
            ],
            title: Some("Build".into()),
        };
        let out = render_todo_list(&list);
        assert!(out.starts_with("**Build** (1/3)"));
        assert!(out.contains("- [x] ~~Scaffold~~"));
        assert!(out.contains("- [ ] Wire routes _(in progress)_"));
        assert!(out.contains("- [ ] Deploy"));
    }

    #[test]
    fn thinking_blockquote_with_model() {
        let t = ThinkingBlock::visible("Consider the tradeoffs.\nThen decide.").with_model("opus");
        let out = render_thinking(&t);
        assert!(out.starts_with("> reasoning (opus)"));
        assert!(out.contains("> Consider the tradeoffs."));
        assert!(out.contains("> Then decide."));
    }

    #[test]
    fn thinking_redacted_hides_blob() {
        let t = ThinkingBlock::redacted("secret-blob");
        let out = render_thinking(&t);
        assert!(out.contains("(redacted reasoning)"));
        assert!(!out.contains("secret-blob"));
    }

    #[test]
    fn error_bold_header_details_and_retry() {
        let err = ErrorCard::new(ErrorCardKind::Delivery, "gateway 502")
            .with_title("Could not deliver")
            .with_details("stack trace here")
            .retryable();
        let out = render_error(&err);
        assert!(out.starts_with("**[ERROR: delivery] Could not deliver**"));
        assert!(out.contains("gateway 502"));
        assert!(out.contains("```\nstack trace here\n```"));
        assert!(out.contains("_(will retry automatically)_"));
    }
}
