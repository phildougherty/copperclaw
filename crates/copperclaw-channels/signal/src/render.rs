//! Signal-flavoured renderers for the rich-surface floor.
//!
//! Signal is a **plaintext** surface: signal-cli's `send` takes a bare
//! `message` string and Signal clients render no Markdown, so a `**bold**`
//! from the canonical text fallback would show its literal asterisks. The
//! renderer here therefore emits clean, markdown-free plaintext.
//!
//! Scope note: only [`render_card`] lives here. The other portable card
//! types — diff, todo list, thinking, error, collapsible — already have
//! canonical *plaintext* `to_text_fallback` renderings (unified diff,
//! `[x]`/`[ ]` checklists, `[reasoning]` / `[ERROR: kind]` prefixes) that
//! are optimal for Signal as-is; a native override would reproduce them
//! byte-for-byte, so the adapter intentionally keeps the trait default
//! for those. The genuine Signal uplift is `edit_message` (the H1 Task
//! HUD) plus this markdown-free card.

use copperclaw_channels_core::Card;

/// Render a [`Card`] as clean Signal plaintext — title on its own line
/// (no `**` markdown), body, `Label: value` field lines, a
/// `- label -> target` button list, and an `[image: url]` marker. Mirrors
/// the structure of [`Card::to_text_fallback`] but drops the Markdown
/// emphasis Signal would render literally.
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

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::{CardButton, CardField};

    #[test]
    fn card_is_markdown_free() {
        let c = Card {
            title: Some("Prototype ready".into()),
            body: Some("Your app is live.".into()),
            fields: vec![CardField {
                label: "Stack".into(),
                value: "Vite".into(),
                inline: false,
            }],
            buttons: vec![CardButton {
                label: "Open".into(),
                value: None,
                url: Some("https://example.com".into()),
                style: None,
            }],
            image_url: Some("https://example.com/s.png".into()),
        };
        let out = render_card(&c);
        assert!(out.starts_with("Prototype ready"));
        assert!(
            !out.contains('*'),
            "Signal renders markdown literally: {out}"
        );
        assert!(out.contains("Your app is live."));
        assert!(out.contains("Stack: Vite"));
        assert!(out.contains("- Open -> https://example.com"));
        assert!(out.contains("[image: https://example.com/s.png]"));
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn card_title_only() {
        let c = Card {
            title: Some("Hi".into()),
            ..Card::default()
        };
        assert_eq!(render_card(&c), "Hi");
    }
}
