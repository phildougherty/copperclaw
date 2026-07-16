//! LINE-flavoured renderers for the rich-surface floor (M19 U5).
//!
//! LINE has two relevant primitives:
//!
//! - **Text messages** render as plain UTF-8 with *no* Markdown, so a
//!   `**bold**` or a `> quote` from the canonical text fallback would leak
//!   its literal markers. The todo-list and diff surfaces therefore render
//!   as clean, markdown-free plaintext (mirroring the Signal / Delta Chat
//!   floors).
//! - **Template messages** (`buttons` template) give a card real,
//!   *tappable* buttons. Each `Card` button maps to a LINE action:
//!   - a `url` button   -> a `uri` action (opens the link),
//!   - a `value` button -> a `postback` action whose `data` is the
//!     button's `value` (LINE POSTs it back to the webhook — see
//!     [`crate::router`] — so in-chat approvals actually route), and
//!   - a label-only button -> a `message` action (taps send the label as
//!     a normal text message).
//!
//! LINE's buttons template caps: up to **4** actions, action `label`
//! <= 20 chars, `title` <= 40 chars, and `text` <= 160 chars (60 when a
//! title is present). We truncate to those limits and cap the action list
//! at 4; the full card text always rides `altText` (<= 400 chars) so
//! notification previews and no-template clients still get the gist. A
//! card with **no** buttons degrades to a plain-text message carrying the
//! same structured plaintext the other bare floors use.
//!
//! LINE has no message-edit and no pin API, so none of these surfaces edit
//! in place — the runner posts a fresh message on each update.

use copperclaw_channels_core::{Card, CardButton, DiffCard, TodoList};
use serde_json::{Value, json};

const MAX_ACTIONS: usize = 4;
const MAX_LABEL_CHARS: usize = 20;
const MAX_TITLE_CHARS: usize = 40;
const MAX_TEXT_CHARS: usize = 160;
const MAX_TEXT_WITH_TITLE_CHARS: usize = 60;
const MAX_ALT_CHARS: usize = 400;

/// Render a [`Card`] into the LINE message object to send.
///
/// When the card carries buttons, this builds a `buttons` **template**
/// message so the buttons are tappable (`postback` for callback buttons,
/// `uri` for links); otherwise it falls back to a plain `text` message
/// with the structured plaintext rendering. See the module docs for the
/// LINE size caps applied here.
#[must_use]
pub fn render_card_message(card: &Card) -> Value {
    if card.buttons.is_empty() {
        return crate::api::text_message(&render_card_text(card));
    }

    let actions: Vec<Value> = card
        .buttons
        .iter()
        .take(MAX_ACTIONS)
        .map(button_action)
        .collect();

    let body = card
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    let title_src = card
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());

    // `text` is required. Prefer the body; fall back to the title, then a
    // generic prompt. A title chip is only set when it wouldn't duplicate
    // the text (i.e. when a distinct body exists to be the text).
    let (title, text) = match (body, title_src) {
        (Some(body), Some(title)) => (
            Some(truncate(title, MAX_TITLE_CHARS)),
            truncate(body, MAX_TEXT_WITH_TITLE_CHARS),
        ),
        (Some(body), None) => (None, truncate(body, MAX_TEXT_CHARS)),
        (None, Some(title)) => (None, truncate(title, MAX_TEXT_CHARS)),
        (None, None) => (None, "Please choose an option:".to_string()),
    };

    let mut template = serde_json::Map::new();
    template.insert("type".into(), json!("buttons"));
    if let Some(t) = title {
        template.insert("title".into(), json!(t));
    }
    if let Some(img) = card
        .image_url
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty())
    {
        template.insert("thumbnailImageUrl".into(), json!(img));
    }
    template.insert("text".into(), json!(text));
    template.insert("actions".into(), Value::Array(actions));

    json!({
        "type": "template",
        "altText": truncate(&render_card_text(card), MAX_ALT_CHARS),
        "template": Value::Object(template),
    })
}

/// Map one [`CardButton`] to a LINE template action object.
fn button_action(b: &CardButton) -> Value {
    let label = truncate(b.label.trim(), MAX_LABEL_CHARS);
    match (
        b.value.as_deref().map(str::trim),
        b.url.as_deref().map(str::trim),
    ) {
        (_, Some(url)) if !url.is_empty() => {
            json!({ "type": "uri", "label": label, "uri": url })
        }
        (Some(v), _) if !v.is_empty() => {
            json!({ "type": "postback", "label": label, "data": v, "displayText": label })
        }
        _ => json!({ "type": "message", "label": label, "text": label }),
    }
}

/// The structured plaintext rendering of a [`Card`] — used for `altText`
/// and as the whole message when the card has no buttons. Markdown-free
/// (LINE renders none): title line, body, `Label: value` fields, a
/// `- label -> target` button list, and an `[image: url]` marker.
#[must_use]
pub fn render_card_text(card: &Card) -> String {
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

/// Render a [`DiffCard`] as fence-free LINE plaintext: a
/// `path (+adds / -removes)` header then unified hunks with `+` / `-` / ` `
/// gutters (LINE shows literal backticks, so no ` ```diff ` fence).
#[must_use]
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

/// Render a [`TodoList`] as a LINE plaintext checklist: a
/// `title (done/total)` header then one `[x]` / `[~]` / `[!]` / `[ ]` line
/// per item. A [`TodoItemStatus::Blocked`](copperclaw_channels_core::TodoItemStatus::Blocked)
/// item carries the `[!]` glyph plus its `blocked_reason` inline
/// (`— blocked: <reason>`) so a stalled step reads as blocked, not stuck
/// "in progress".
#[must_use]
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

/// Truncate `s` to at most `max` characters, appending an ellipsis when it
/// had to cut (the ellipsis counts toward `max` so the result never
/// exceeds LINE's field cap).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::{
        CardField, DiffHunk, DiffLine, DiffLineKind, TodoItemStatus, TodoListItem,
    };

    fn approval_card() -> Card {
        Card {
            title: Some("Approve deploy?".into()),
            body: Some("The agent wants to run deploy.sh.".into()),
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
                    label: "Deny".into(),
                    value: Some("deny:42".into()),
                    url: None,
                    style: None,
                },
                CardButton {
                    label: "Docs".into(),
                    value: None,
                    url: Some("https://example.com/docs".into()),
                    style: None,
                },
            ],
            image_url: None,
        }
    }

    #[test]
    fn card_with_buttons_becomes_buttons_template() {
        let msg = render_card_message(&approval_card());
        assert_eq!(msg["type"], "template");
        assert_eq!(msg["template"]["type"], "buttons");
        assert_eq!(msg["template"]["title"], "Approve deploy?");
        assert_eq!(msg["template"]["text"], "The agent wants to run deploy.sh.");
        let actions = msg["template"]["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 3);
        // Callback button -> postback carrying its value as `data`.
        assert_eq!(actions[0]["type"], "postback");
        assert_eq!(actions[0]["label"], "Approve");
        assert_eq!(actions[0]["data"], "approve:42");
        assert_eq!(actions[1]["type"], "postback");
        assert_eq!(actions[1]["data"], "deny:42");
        // URL button -> uri action.
        assert_eq!(actions[2]["type"], "uri");
        assert_eq!(actions[2]["uri"], "https://example.com/docs");
        // Fallback text always present.
        assert!(msg["altText"].as_str().unwrap().contains("Approve deploy?"));
    }

    #[test]
    fn label_only_button_becomes_message_action() {
        let card = Card {
            buttons: vec![CardButton {
                label: "Retry".into(),
                value: None,
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        let msg = render_card_message(&card);
        let actions = msg["template"]["actions"].as_array().unwrap();
        assert_eq!(actions[0]["type"], "message");
        assert_eq!(actions[0]["text"], "Retry");
    }

    #[test]
    fn card_actions_capped_at_four() {
        let card = Card {
            body: Some("pick".into()),
            buttons: (0..6)
                .map(|i| CardButton {
                    label: format!("b{i}"),
                    value: Some(format!("v{i}")),
                    url: None,
                    style: None,
                })
                .collect(),
            ..Card::default()
        };
        let msg = render_card_message(&card);
        assert_eq!(msg["template"]["actions"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn long_label_is_truncated_to_line_limit() {
        let card = Card {
            buttons: vec![CardButton {
                label: "a".repeat(40),
                value: Some("v".into()),
                url: None,
                style: None,
            }],
            ..Card::default()
        };
        let msg = render_card_message(&card);
        let label = msg["template"]["actions"][0]["label"].as_str().unwrap();
        assert_eq!(label.chars().count(), MAX_LABEL_CHARS);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn card_without_buttons_is_a_text_message() {
        let card = Card {
            title: Some("Heads up".into()),
            body: Some("no buttons here".into()),
            ..Card::default()
        };
        let msg = render_card_message(&card);
        assert_eq!(msg["type"], "text");
        let text = msg["text"].as_str().unwrap();
        assert!(text.contains("Heads up"));
        assert!(text.contains("no buttons here"));
        assert!(!text.contains("**"));
    }

    #[test]
    fn diff_is_fence_free() {
        let diff = DiffCard {
            path: "src/a.rs".into(),
            language: None,
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Remove,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Add,
                        text: "new".into(),
                    },
                ],
            }],
            added: 1,
            removed: 1,
            truncated: false,
        };
        let out = render_diff(&diff);
        assert!(out.starts_with("src/a.rs (+1 / -1)"));
        assert!(out.contains("-old"));
        assert!(out.contains("+new"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn todo_blocked_shows_glyph_and_reason() {
        let list = TodoList {
            title: Some("Build".into()),
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "Scaffold".into(),
                    status: TodoItemStatus::Completed,
                    blocked_reason: None,
                },
                TodoListItem {
                    id: 2,
                    text: "Verify".into(),
                    status: TodoItemStatus::Blocked,
                    blocked_reason: Some("no verify script".into()),
                },
            ],
        };
        let out = render_todo_list(&list);
        assert!(out.starts_with("Build (1/2)"));
        assert!(out.contains("[x] Scaffold"));
        assert!(
            out.contains("[!] Verify — blocked: no verify script"),
            "{out}"
        );
    }
}
