//! Signal-flavoured renderers for the rich-surface floor.
//!
//! Signal is a **plaintext** surface: signal-cli's `send` takes a bare
//! `message` string and Signal clients render no Markdown, so a `**bold**`
//! or a `> quote` from the canonical text fallback would show its literal
//! markers. Every renderer here therefore emits clean, markdown-free
//! plaintext, and — for the surfaces the runner re-emits on every update
//! (the breadcrumb chip, the live todo list) — the adapter drives
//! signal-cli's `sendEditMessage` so the chip is edited **in place**
//! rather than stacked as fresh prose on each tool boundary.
//!
//! What each surface renders here vs. why it stays on the trait fallback:
//!
//! - [`render_card`], [`render_breadcrumb`], [`render_diff`],
//!   [`render_todo_list`], [`render_thinking`], [`render_error`] — native.
//!   The trait fallbacks lean on Markdown-ish markers (`**bold**`, `> `
//!   quotes) that Signal renders literally, and stack a new message on
//!   every update. These renderers strip that noise into a structured,
//!   glanceable plaintext layout; the breadcrumb + todo overrides also
//!   edit their chip in place (Signal supports `sendEditMessage`).
//! - `collapsible` — **kept on the trait fallback**. Signal has no
//!   disclosure / expandable primitive and no fenced-block styling, so
//!   the default `render_collapsible_text_fallback` (summary + preview +
//!   `…(N more lines)`) is already the optimal markdown-free plaintext
//!   shape; a native override would reproduce it byte-for-byte and
//!   collapsibles carry no in-place-edit id to improve on.

use copperclaw_channels_core::{
    Breadcrumb, BreadcrumbStatus, Card, DiffCard, ErrorCard, ThinkingBlock, TodoList,
};

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

/// Stable ASCII status marker for a breadcrumb chip. Words, not emoji —
/// Signal is plaintext and the workspace forbids emoji glyphs in source,
/// so `[running]` / `[done]` / `[failed]` carry the lifecycle unambiguously
/// on every client and survive an in-place `sendEditMessage`.
fn status_marker(status: BreadcrumbStatus) -> &'static str {
    match status {
        BreadcrumbStatus::Running => "[running]",
        BreadcrumbStatus::Done => "[done]",
        BreadcrumbStatus::Failed => "[failed]",
    }
}

/// Render a [`Breadcrumb`] as a compact one-line Signal chip:
/// `[status] tool · detail — summary`. The runner emits this on tool
/// start (`running`) and again on completion (`done` / `failed`); the
/// adapter feeds the completion through `sendEditMessage` so the user
/// watches `[running] shell · cargo check` become `[done] shell · cargo
/// check — passed (0.4s)` on the *same* line instead of two stacked
/// messages. An aggregate chip (non-empty `steps`) renders a header line
/// plus one indented single-chip line per step.
pub fn render_breadcrumb(b: &Breadcrumb) -> String {
    if !b.steps.is_empty() {
        return render_activity(b);
    }
    let mut out = String::with_capacity(64);
    out.push_str(status_marker(b.status));
    out.push(' ');
    out.push_str(b.tool_name.trim());
    if let Some(d) = b.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(" · ");
        out.push_str(d);
    }
    if let Some(s) = b
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if b.status == BreadcrumbStatus::Failed {
            out.push_str(" — failed: ");
        } else {
            out.push_str(" — ");
        }
        out.push_str(s);
    }
    out
}

/// Render the rolling aggregate "activity" chip: a collapsed one-line
/// header (current activity + optional count) followed by one indented
/// single-chip line per tool step. Signal has no expandable region, so
/// the steps render as a plain indented list — low-churn (one edited
/// message) rather than a message per step.
fn render_activity(b: &Breadcrumb) -> String {
    let mut out = String::new();
    let head = b
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("working");
    out.push_str(head);
    if let Some(s) = b
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str(" · ");
        out.push_str(s);
    }
    for step in &b.steps {
        out.push_str("\n  ");
        out.push_str(&render_breadcrumb(step));
    }
    out
}

/// Render a [`DiffCard`] as a structured Signal plaintext diff: a
/// glanceable `path (+adds / -removes)` header line, then the unified
/// hunks with `@@` ranges and `+` / `-` / ` ` gutters. No ` ```diff `
/// fence (Signal would show literal backticks) and no redundant `--- a/`
/// / `+++ b/` git file header — the path already leads the block.
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

/// Render a [`TodoList`] as a structured Signal checklist: a
/// `title (done/total)` header with the counter hoisted up top for the
/// pinned-chip glance, then one `[x]` / `[~]` / `[ ]` line per item. The
/// glyphs are ASCII (Signal renders no strikethrough / task-list
/// Markdown), and the adapter edits this chip in place on each mutation.
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
    }
    out
}

/// Render a [`ThinkingBlock`] as a `reasoning (model)` header followed by
/// the reasoning text, one line per line — no `> ` quote prefix (Signal
/// renders it as literal noise, not a blockquote). Redacted blocks emit
/// only the placeholder: the raw opaque blob never reaches the wire.
pub fn render_thinking(thinking: &ThinkingBlock) -> String {
    let header = match thinking.model.as_deref().map(str::trim) {
        Some(m) if !m.is_empty() => format!("reasoning ({m})"),
        _ => "reasoning".to_string(),
    };
    if thinking.redacted {
        return format!("{header}\n(redacted reasoning)");
    }
    let mut out = header;
    for line in thinking.text.lines() {
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// Render an [`ErrorCard`] as a structured Signal plaintext receipt:
/// an `[ERROR: kind] title` banner (Signal has no colour affordance, so
/// the bracketed kind carries the severity), the plain-language summary,
/// an optional `details:` block indented two spaces per line (not `> `
/// quotes, which Signal shows literally), and a retry footer.
pub fn render_error(err: &ErrorCard) -> String {
    let mut out = String::with_capacity(96 + err.summary.len());
    out.push_str("[ERROR: ");
    out.push_str(err.kind.label());
    out.push_str("] ");
    out.push_str(err.title.trim());
    out.push('\n');
    out.push_str(err.summary.trim());
    if let Some(d) = err
        .details
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        out.push_str("\ndetails:");
        for line in d.lines() {
            out.push_str("\n  ");
            out.push_str(line);
        }
    }
    if err.retryable {
        out.push_str("\n(will retry automatically)");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::{
        CardButton, CardField, DiffHunk, DiffLine, DiffLineKind, ErrorCardKind, TodoItemStatus,
        TodoListItem,
    };

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

    // ------- breadcrumb -------

    #[test]
    fn breadcrumb_running_is_single_line_chip() {
        let b = Breadcrumb::running("shell").with_detail("cargo check");
        assert_eq!(render_breadcrumb(&b), "[running] shell · cargo check");
    }

    #[test]
    fn breadcrumb_done_appends_summary_markdown_free() {
        let b = Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let out = render_breadcrumb(&b);
        assert_eq!(out, "[done] shell · cargo check — passed (0.4s)");
        assert!(
            !out.contains('*'),
            "signal chip must be markdown-free: {out}"
        );
    }

    #[test]
    fn breadcrumb_failed_uses_failed_prefix() {
        let b = Breadcrumb::running("shell")
            .with_detail("cargo test")
            .finished(false, Some("timeout".into()));
        assert_eq!(
            render_breadcrumb(&b),
            "[failed] shell · cargo test — failed: timeout"
        );
    }

    #[test]
    fn breadcrumb_without_detail_is_just_tool() {
        let b = Breadcrumb::running("web_search");
        assert_eq!(render_breadcrumb(&b), "[running] web_search");
    }

    #[test]
    fn breadcrumb_aggregate_renders_header_and_indented_steps() {
        let steps = vec![
            Breadcrumb::running("read_file")
                .with_detail("a.rs")
                .finished(true, Some("10 lines".into())),
            Breadcrumb::running("shell").with_detail("cargo build"),
        ];
        let agg = Breadcrumb::running("activity")
            .with_detail("shell cargo build")
            .with_steps(steps);
        let out = render_breadcrumb(&agg);
        assert!(out.starts_with("shell cargo build"), "{out}");
        assert!(
            out.contains("\n  [done] read_file · a.rs — 10 lines"),
            "{out}"
        );
        assert!(out.contains("\n  [running] shell · cargo build"), "{out}");
    }

    // ------- diff -------

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
    fn diff_renders_header_and_gutters_without_fence() {
        let out = render_diff(&sample_diff());
        assert!(out.starts_with("src/main.rs (+1 / -1)"), "{out}");
        assert!(out.contains("@@ -1,1 +1,1 @@"));
        assert!(out.contains("\n-fn old() {}"));
        assert!(out.contains("\n+fn new() {}"));
        // Signal shows backticks literally, so no fenced-diff block.
        assert!(
            !out.contains("```"),
            "signal diff must be fence-free: {out}"
        );
        // The redundant git file header is dropped — the path leads instead.
        assert!(!out.contains("--- a/"), "{out}");
    }

    #[test]
    fn diff_marks_truncated_in_header() {
        let mut d = sample_diff();
        d.truncated = true;
        let out = render_diff(&d);
        assert!(out.starts_with("src/main.rs (+1 / -1, truncated)"), "{out}");
    }

    // ------- todo list -------

    fn sample_todos() -> TodoList {
        TodoList {
            items: vec![
                TodoListItem {
                    id: 1,
                    text: "Scaffold".into(),
                    status: TodoItemStatus::Completed,
                },
                TodoListItem {
                    id: 2,
                    text: "Wire routes".into(),
                    status: TodoItemStatus::InProgress,
                },
                TodoListItem {
                    id: 3,
                    text: "Deploy".into(),
                    status: TodoItemStatus::Pending,
                },
            ],
            title: Some("Build".into()),
        }
    }

    #[test]
    fn todo_list_hoists_counter_and_uses_ascii_glyphs() {
        let out = render_todo_list(&sample_todos());
        assert!(out.starts_with("Build (1/3)"), "{out}");
        assert!(out.contains("\n[x] Scaffold"));
        assert!(out.contains("\n[~] Wire routes"));
        assert!(out.contains("\n[ ] Deploy"));
        assert!(
            !out.contains('*'),
            "signal todo must be markdown-free: {out}"
        );
        // `~` only appears inside the in-progress glyph `[~]`, never as a
        // `~strikethrough~` wrapper Signal would render literally.
        assert!(
            !out.contains("~Scaffold~"),
            "no strikethrough markdown on signal: {out}"
        );
    }

    // ------- thinking -------

    #[test]
    fn thinking_drops_quote_prefix() {
        let t = ThinkingBlock::visible("Consider the tradeoffs.\nThen decide.").with_model("opus");
        let out = render_thinking(&t);
        assert!(out.starts_with("reasoning (opus)"), "{out}");
        assert!(out.contains("\nConsider the tradeoffs."));
        assert!(out.contains("\nThen decide."));
        // No `> ` blockquote markers (Signal renders them literally).
        assert!(!out.contains("> "), "{out}");
    }

    #[test]
    fn thinking_redacted_hides_blob() {
        let t = ThinkingBlock::redacted("opaque-blob-secret");
        let out = render_thinking(&t);
        assert!(out.contains("(redacted reasoning)"));
        assert!(
            !out.contains("opaque-blob-secret"),
            "raw redacted blob must never reach the wire: {out}"
        );
    }

    // ------- error -------

    #[test]
    fn error_banner_details_and_retry() {
        let err = ErrorCard::new(ErrorCardKind::Delivery, "gateway 502")
            .with_title("Could not deliver")
            .with_details("line one\nline two")
            .retryable();
        let out = render_error(&err);
        assert!(
            out.starts_with("[ERROR: delivery] Could not deliver"),
            "{out}"
        );
        assert!(out.contains("gateway 502"));
        assert!(out.contains("\ndetails:"));
        assert!(out.contains("\n  line one"));
        assert!(out.contains("\n  line two"));
        assert!(out.contains("(will retry automatically)"));
        assert!(
            !out.contains('*'),
            "signal error must be markdown-free: {out}"
        );
        assert!(!out.contains("> "), "no quote markers on signal: {out}");
    }

    #[test]
    fn error_without_details_or_retry_is_two_lines() {
        let err = ErrorCard::new(ErrorCardKind::Internal, "the shell tool timed out");
        let out = render_error(&err);
        assert!(out.starts_with("[ERROR: tool]"), "{out}");
        assert!(out.contains("the shell tool timed out"));
        assert!(!out.contains("details:"));
        assert!(!out.contains("will retry"));
    }
}
