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

/// Render a [`Card`] as clean Delta Chat plaintext. Delta Chat clients
/// render no reliable Markdown, so this uses the shared markdown-free
/// [`Card::to_plaintext`]: title line, body, `Label: value` fields, a
/// `- label -> target` button list (callback buttons surface their `value`
/// as the target — Delta Chat has no interactive buttons), and an
/// `[image: url]` marker.
pub fn render_card(card: &Card) -> String {
    card.to_plaintext()
}

/// Render a [`DiffCard`] as a structured Delta Chat plaintext diff via the
/// shared fence-free [`DiffCard::to_plaintext`]: a glanceable
/// `path (+adds / -removes)` header, then unified hunks with `@@` ranges
/// and `+` / `-` / ` ` gutters. Delta Chat would show literal backticks,
/// so there's no ` ```diff ` fence.
pub fn render_diff(diff: &DiffCard) -> String {
    diff.to_plaintext()
}

/// Render a [`TodoList`] as a structured Delta Chat checklist via the
/// shared [`TodoList::to_chip_plaintext`]: a `title (done/total)` header
/// with the counter hoisted up top, then one `[x]` / `[~]` / `[!]` / `[ ]`
/// line per item (ASCII glyphs — Delta Chat renders no task-list Markdown).
/// A [`TodoItemStatus::Blocked`](copperclaw_channels_core::TodoItemStatus::Blocked)
/// item carries its one-line `blocked_reason` inline so a user sees *why* a
/// step stalled instead of a step stuck "in progress" forever.
pub fn render_todo_list(list: &TodoList) -> String {
    list.to_chip_plaintext()
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
