//! Delta Chat-flavoured renderers for the rich-surface floor (M19 U5).
//!
//! Delta Chat is an **e-mail transport**: `send_msg` takes a bare `text`
//! string and the clients render it as plain text (no reliable Markdown
//! across the desktop / Android / iOS clients, and no interactive
//! buttons). So — exactly like the Signal floor — every renderer here
//! emits clean, markdown-free plaintext: a `**bold**` or a `> quote` from
//! the canonical text fallback would leak its literal markers into the
//! chat.
//!
//! Delta Chat also has **no message-edit API** (`deliver_action`'s `edit`
//! arm returns `Unsupported`), so none of these surfaces edit in place;
//! the runner posts a fresh chip on each update. Each renderer is a pure
//! `&T -> String` function so the adapter's `deliver_*` overrides stay thin
//! and the formatting is unit-testable without a mock RPC server.
//!
//! Scope: the three surfaces U5 mandates for the bare-adapter floor —
//! [`render_card`], [`render_todo_list`], and [`render_diff`]. The other
//! portable surfaces (breadcrumb / collapsible / thinking / error) stay on
//! the trait's text fallback, which is already markdown-free plaintext and
//! carries no edit id Delta Chat could improve on.

use copperclaw_channels_core::{Card, DiffCard, TodoList};

/// Render a [`Card`] as clean Delta Chat plaintext — title on its own
/// line (no `**` markdown), body, `Label: value` field lines, a
/// `- label -> target` button list, and an `[image: url]` marker. Mirrors
/// the structure of [`Card::to_text_fallback`] but drops the Markdown
/// emphasis Delta Chat clients would render literally. Callback buttons
/// surface their `value` as the target since Delta Chat has no interactive
/// buttons.
pub fn render_card(card: &Card) -> String {
    let mut out = String::new();
    if let Some(t) = card
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
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
            out.push_str(f.label.trim());
            out.push_str(": ");
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
            match (b.value.as_deref(), b.url.as_deref()) {
                (_, Some(url)) => {
                    out.push_str(" -> ");
                    out.push_str(url.trim());
                }
                (Some(v), None) => {
                    out.push_str(" -> ");
                    out.push_str(v.trim());
                }
                (None, None) => {}
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
        out.push_str("[image: ");
        out.push_str(img);
        out.push_str("]\n");
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Render a [`DiffCard`] as a structured Delta Chat plaintext diff: a
/// glanceable `path (+adds / -removes)` header line, then the unified
/// hunks with `@@` ranges and `+` / `-` / ` ` gutters. No ` ```diff `
/// fence (Delta Chat would show literal backticks) and no redundant
/// `--- a/` / `+++ b/` git file header — the path already leads the block.
pub fn render_diff(diff: &DiffCard) -> String {
    let mut out = String::with_capacity(64 + diff.hunks.len() * 48);
    out.push_str(diff.path.trim());
    out.push_str(" (+");
    out.push_str(&diff.added.to_string());
    out.push_str(" / -");
    out.push_str(&diff.removed.to_string());
    if diff.truncated {
        out.push_str(", truncated");
    }
    out.push(')');
    for h in &diff.hunks {
        out.push_str(&format!(
            "\n@@ -{},{} +{},{} @@",
            h.old_start, h.old_lines, h.new_start, h.new_lines
        ));
        for line in &h.lines {
            out.push('\n');
            out.push(line.kind.unified_prefix());
            out.push_str(&line.text);
        }
    }
    out
}

/// Render a [`TodoList`] as a structured Delta Chat checklist: a
/// `title (done/total)` header with the counter hoisted up top for the
/// glance, then one `[x]` / `[~]` / `[!]` / `[ ]` line per item. The
/// glyphs are ASCII (Delta Chat renders no strikethrough / task-list
/// Markdown). A [`TodoItemStatus::Blocked`](copperclaw_channels_core::TodoItemStatus::Blocked)
/// item carries the `[!]` glyph plus its one-line `blocked_reason` inline
/// (`— blocked: <reason>`) so a user watching the list sees *why* a step
/// stalled instead of a step stuck "in progress" forever.
pub fn render_todo_list(list: &TodoList) -> String {
    let done = list.completed_count();
    let total = list.items.len();
    let mut out = String::with_capacity(64 + list.items.len() * 32);
    out.push_str(list.title_or_default());
    out.push_str(&format!(" ({done}/{total})"));
    for item in &list.items {
        out.push('\n');
        out.push_str(item.status.glyph());
        out.push(' ');
        out.push_str(item.text.trim());
        if let Some(reason) = item.blocked_reason_text() {
            out.push_str(" — blocked: ");
            out.push_str(reason.trim());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::{
        CardButton, CardField, DiffHunk, DiffLine, DiffLineKind, TodoItemStatus, TodoList,
        TodoListItem,
    };

    fn full_card() -> Card {
        Card {
            title: Some("Approve deploy?".into()),
            body: Some("The agent wants to run `deploy.sh`.".into()),
            fields: vec![CardField {
                label: "Risk".into(),
                value: "low".into(),
                inline: false,
            }],
            buttons: vec![
                CardButton {
                    label: "Approve".into(),
                    value: Some("approve:42".into()),
                    url: None,
                    style: None,
                },
                CardButton {
                    label: "Docs".into(),
                    value: None,
                    url: Some("https://example.com/d".into()),
                    style: None,
                },
            ],
            image_url: Some("https://example.com/shot.png".into()),
        }
    }

    #[test]
    fn card_is_markdown_free_plaintext() {
        let out = render_card(&full_card());
        assert!(out.starts_with("Approve deploy?"));
        assert!(out.contains("The agent wants to run `deploy.sh`."));
        assert!(out.contains("Risk: low"));
        assert!(out.contains("- Approve -> approve:42"));
        assert!(out.contains("- Docs -> https://example.com/d"));
        assert!(out.contains("[image: https://example.com/shot.png]"));
        assert!(!out.contains("**"));
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn card_title_only() {
        let c = Card {
            title: Some("Just a title".into()),
            ..Card::default()
        };
        assert_eq!(render_card(&c), "Just a title");
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
    fn diff_is_fence_free_with_gutters() {
        let out = render_diff(&sample_diff());
        assert!(out.starts_with("src/main.rs (+1 / -1)"));
        assert!(out.contains("@@ -1,1 +1,1 @@"));
        assert!(out.contains("-fn old() {}"));
        assert!(out.contains("+fn new() {}"));
        assert!(!out.contains("```"));
    }

    fn list_with(items: Vec<TodoListItem>) -> TodoList {
        TodoList {
            items,
            title: Some("Build".into()),
        }
    }

    #[test]
    fn todo_list_uses_ascii_glyphs_and_counter() {
        let list = list_with(vec![
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
        ]);
        let out = render_todo_list(&list);
        assert!(out.starts_with("Build (1/3)"));
        assert!(out.contains("[x] Scaffold"));
        assert!(out.contains("[~] Wire routes"));
        assert!(out.contains("[ ] Deploy"));
    }

    #[test]
    fn todo_blocked_item_shows_glyph_and_reason() {
        let list = list_with(vec![TodoListItem {
            id: 1,
            text: "Verify build".into(),
            status: TodoItemStatus::Blocked,
            blocked_reason: Some("burned all verify fix-cycles".into()),
        }]);
        let out = render_todo_list(&list);
        assert!(
            out.contains("[!] Verify build — blocked: burned all verify fix-cycles"),
            "{out}"
        );
    }

    #[test]
    fn todo_blocked_without_reason_still_uses_glyph() {
        let list = list_with(vec![TodoListItem {
            id: 1,
            text: "Stuck step".into(),
            status: TodoItemStatus::Blocked,
            blocked_reason: None,
        }]);
        let out = render_todo_list(&list);
        assert!(out.contains("[!] Stuck step"), "{out}");
        assert!(!out.contains("blocked:"), "{out}");
    }
}
