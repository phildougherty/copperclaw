//! `WhatsApp`-flavoured renderers that raise `WhatsApp` Cloud to the
//! rich-surface floor.
//!
//! `WhatsApp` text messages support a small inline formatting vocabulary —
//! `*bold*`, `_italic_`, `~strikethrough~`, `` `inline mono` ``, and a
//! ` ``` `-fenced monospace block — but no headings, no Markdown links
//! (raw URLs auto-link), and no task lists or blockquotes. Each portable
//! card type maps onto that vocabulary. Renderers are pure `&T -> String`
//! so the adapter overrides stay thin and formatting is unit-testable.
//!
//! `WhatsApp` caps a single text send at 4096 chars. The portable field
//! caps keep cards/diffs/todos comfortably under that, and the
//! collapsible renderer emits only the summary + preview (never the full
//! body), so no renderer here can blow the limit.

use crate::factory::CHANNEL_TYPE_STR;
use copperclaw_channels_core::{
    Card, DiffCard, ErrorCard, ThinkingBlock, TodoItemStatus, TodoList, vocab,
};

/// Render a [`Card`] as a `WhatsApp` text message: a `*bold*` title, the
/// body, `*Label:* value` field lines, a `label — url` / `label` button
/// list (raw URLs auto-link on `WhatsApp`), and the image URL on its own
/// line.
pub fn render_card(card: &Card) -> String {
    let mut out = String::new();
    if let Some(t) = card
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        out.push('*');
        out.push_str(t);
        out.push_str("*\n");
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
            out.push('*');
            out.push_str(f.label.trim());
            out.push_str(":* ");
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
            out.push_str(b.label.trim());
            if let Some(url) = b.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                out.push_str(" — ");
                out.push_str(url);
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
        out.push_str(img);
        out.push('\n');
    }
    trim_trailing_newlines(&mut out);
    out
}

/// Render a [`DiffCard`] as a `*path* (+a / -r)` header followed by the
/// unified-diff body inside a ` ``` `-fenced monospace block.
pub fn render_diff(diff: &DiffCard) -> String {
    let mut out = String::with_capacity(64 + diff.hunks.len() * 48);
    out.push('*');
    out.push_str(diff.path.trim());
    out.push_str("* (+");
    out.push_str(&diff.added.to_string());
    out.push_str(" / -");
    out.push_str(&diff.removed.to_string());
    if diff.truncated {
        out.push_str(", truncated");
    }
    out.push_str(")\n```\n");
    out.push_str(&unified_hunks(diff));
    out.push_str("```");
    out
}

/// Render the slice-3.4 long-output expander — an `_italic_` summary
/// then the preview lines in a monospace block with a `…(N more)`
/// truncation marker. The full body stays on disk (on-the-wire shape
/// only) so this can't exceed `WhatsApp`'s 4096-char cap.
pub fn render_collapsible(text: &str, summary: &str, preview_lines: &[String]) -> String {
    let total_lines = text.lines().count();
    let remaining = total_lines.saturating_sub(preview_lines.len());
    let mut out = String::with_capacity(summary.len() + 64);
    out.push('_');
    out.push_str(summary.trim());
    out.push('_');
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

/// Render a [`TodoList`] — a `*title* (done/total)` header then one line
/// per item: `[x] ~text~` for completed (strikethrough), `[ ] text` for
/// pending, and `[ ] text _(in progress)_` for the active item.
pub fn render_todo_list(list: &TodoList) -> String {
    let done = list.completed_count();
    let total = list.items.len();
    let mut out = String::with_capacity(64 + list.items.len() * 32);
    out.push('*');
    out.push_str(list.title_or_default());
    out.push_str(&format!("* ({done}/{total})"));
    // ASCII checkbox glyphs via the vocab binding, per the project's
    // no-emoji rule. Deliberate divergence for `InProgress`: `WhatsApp`
    // text has no half-checked box, so the active item reuses the
    // *pending* glyph (`[ ]`) plus the `_(in progress)_` suffix —
    // byte-identical to the pre-vocab output.
    let todo = vocab::for_channel(CHANNEL_TYPE_STR).todo;
    for item in &list.items {
        out.push('\n');
        let text = item.text.trim();
        match item.status {
            TodoItemStatus::Completed => {
                out.push_str(todo.completed);
                out.push_str(" ~");
                out.push_str(text);
                out.push('~');
            }
            TodoItemStatus::InProgress => {
                out.push_str(todo.pending);
                out.push(' ');
                out.push_str(text);
                out.push_str(" _(in progress)_");
            }
            TodoItemStatus::Blocked => {
                out.push_str(todo.blocked);
                out.push(' ');
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
                out.push_str(todo.pending);
                out.push(' ');
                out.push_str(text);
            }
        }
    }
    out
}

/// Render a [`ThinkingBlock`] as an `_reasoning (model)_` italic header
/// followed by the reasoning text (`WhatsApp` has no blockquote). Redacted
/// blocks emit only the placeholder.
pub fn render_thinking(thinking: &ThinkingBlock) -> String {
    let header = match thinking.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => format!("_reasoning ({m})_"),
        _ => "_reasoning_".to_string(),
    };
    if thinking.redacted {
        return format!("{header}\n(redacted reasoning)");
    }
    let mut out = header;
    out.push('\n');
    out.push_str(thinking.text.trim());
    out
}

/// Render an [`ErrorCard`] — a `*[ERROR: kind] title*` bold header
/// (`WhatsApp` has no colour affordance), the summary, an optional fenced
/// detail block, and an italic retry footer.
pub fn render_error(err: &ErrorCard) -> String {
    let mut out = String::with_capacity(96 + err.summary.len());
    out.push_str("*[ERROR: ");
    out.push_str(err.kind.label());
    out.push_str("] ");
    out.push_str(err.title.trim());
    out.push_str("*\n");
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

/// Build the unified-diff hunk body (no header / totals footer) for the
/// fenced monospace block.
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

    #[test]
    fn card_uses_single_asterisk_bold() {
        let c = Card {
            title: Some("Order".into()),
            body: Some("Ready?".into()),
            fields: vec![CardField {
                label: "Price".into(),
                value: "$4.50".into(),
                inline: false,
            }],
            buttons: vec![CardButton {
                label: "Open".into(),
                value: None,
                url: Some("https://example.com".into()),
                style: None,
            }],
            image_url: None,
        };
        let out = render_card(&c);
        assert!(out.contains("*Order*"));
        assert!(out.contains("Ready?"));
        assert!(out.contains("*Price:* $4.50"));
        assert!(out.contains("- Open — https://example.com"));
        assert!(!out.contains("**"));
    }

    #[test]
    fn diff_uses_plain_fence() {
        let diff = DiffCard {
            path: "a.rs".into(),
            language: None,
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 0,
                new_start: 1,
                new_lines: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Add,
                    text: "let x = 1;".into(),
                }],
            }],
            added: 1,
            removed: 0,
            truncated: false,
        };
        let out = render_diff(&diff);
        assert!(out.starts_with("*a.rs* (+1 / -0)"));
        assert!(out.contains("```\n@@"));
        assert!(out.contains("+let x = 1;"));
        assert!(out.ends_with("```"));
    }

    #[test]
    fn collapsible_italic_summary() {
        let body = (1..=20)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview: Vec<String> = (1..=3).map(|i| format!("l{i}")).collect();
        let out = render_collapsible(&body, "20 lines", &preview);
        assert!(out.starts_with("_20 lines_"));
        assert!(out.contains("…(17 more lines)"));
    }

    #[test]
    fn todo_list_strikes_completed() {
        let list = TodoList {
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "done".into(),
                    status: TodoItemStatus::Completed,
                    blocked_reason: None,
                },
                TodoListItem {
                    id: 2,
                    text: "now".into(),
                    status: TodoItemStatus::InProgress,
                    blocked_reason: None,
                },
            ],
            title: None,
        };
        let out = render_todo_list(&list);
        assert!(out.starts_with("*Plan* (1/2)"));
        assert!(out.contains("[x] ~done~"));
        assert!(out.contains("[ ] now _(in progress)_"));
    }

    #[test]
    fn todo_list_is_byte_identical_to_pre_vocab_literals() {
        // M22 A4 byte-identity gate for the vocab rerouting: the
        // expected string is a hardcoded literal of the exact pre-vocab
        // output (all four statuses) — deliberately NOT read through
        // vocab constants, which would be circular. Note in-progress
        // deliberately reuses the unchecked box.
        let list = TodoList {
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "done item".into(),
                    status: TodoItemStatus::Completed,
                    blocked_reason: None,
                },
                TodoListItem {
                    id: 2,
                    text: "active item".into(),
                    status: TodoItemStatus::InProgress,
                    blocked_reason: None,
                },
                TodoListItem {
                    id: 3,
                    text: "stuck item".into(),
                    status: TodoItemStatus::Blocked,
                    blocked_reason: Some("waiting on API key".into()),
                },
                TodoListItem {
                    id: 4,
                    text: "later item".into(),
                    status: TodoItemStatus::Pending,
                    blocked_reason: None,
                },
            ],
            title: Some("Build".into()),
        };
        assert_eq!(
            render_todo_list(&list),
            "*Build* (1/4)\n\
             [x] ~done item~\n\
             [ ] active item _(in progress)_\n\
             [!] stuck item _(blocked: waiting on API key)_\n\
             [ ] later item"
        );
    }

    #[test]
    fn thinking_redacted_hides_blob() {
        let out = render_thinking(&ThinkingBlock::redacted("blob"));
        assert!(out.contains("(redacted reasoning)"));
        assert!(!out.contains("blob"));
    }

    #[test]
    fn error_bold_header() {
        let err = ErrorCard::new(ErrorCardKind::Provider, "upstream down")
            .with_title("Model failed")
            .retryable();
        let out = render_error(&err);
        assert!(out.starts_with("*[ERROR: provider] Model failed*"));
        assert!(out.contains("upstream down"));
        assert!(out.contains("_(will retry automatically)_"));
    }
}
