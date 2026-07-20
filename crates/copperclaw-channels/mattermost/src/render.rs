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

use crate::factory::CHANNEL_TYPE_STR;
use copperclaw_channels_core::{
    Breadcrumb, BreadcrumbStatus, Card, DiffCard, ErrorCard, ThinkingBlock, TodoItemStatus,
    TodoList, vocab,
};

/// Render a [`Breadcrumb`] as a compact Mattermost Markdown chip. The
/// tool name rides an inline-code span (`` `shell` ``) — the closest
/// metadata-chip aesthetic Mattermost offers without committing a full
/// attachment — prefixed by an ASCII status marker (`[~]` running,
/// `[ok]` done, `[x]` failed) so it survives every client's text
/// encoding and honours the no-emoji rule.
///
/// A single tool renders one line:
/// - `[~] `shell` · cargo check`
/// - `[ok] `shell` · cargo check — passed (0.4s)`
/// - `[x] `shell` · cargo check — failed: timeout`
///
/// A rolling *activity* aggregate (`steps` non-empty) renders a bold
/// summary line plus one Markdown bullet per step, each styled with the
/// same chip shape — Mattermost has no disclosure widget, so the steps
/// stay visible (low-churn since the whole chip is edited in place).
pub fn render_breadcrumb(b: &Breadcrumb) -> String {
    if !b.steps.is_empty() {
        return render_breadcrumb_aggregate(b);
    }
    render_breadcrumb_line(b)
}

/// One chip line: `<marker> `tool` · detail — summary`.
fn render_breadcrumb_line(b: &Breadcrumb) -> String {
    let mut out = String::with_capacity(64);
    out.push_str(breadcrumb_marker(b.status));
    out.push_str(" `");
    out.push_str(&sanitize_inline_code(&b.tool_name));
    out.push('`');
    if let Some(d) = b.detail.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        out.push_str(" · ");
        out.push_str(&sanitize_inline_code(d));
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
        out.push_str(&sanitize_inline_code(s));
    }
    out
}

/// Rolling aggregate: a bold collapsed summary line + a Markdown bullet
/// list of the individual tool steps.
fn render_breadcrumb_aggregate(b: &Breadcrumb) -> String {
    let head = b
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or("working");
    let mut out = String::with_capacity(96 + b.steps.len() * 48);
    out.push_str("**");
    out.push_str(&sanitize_inline_code(head));
    out.push_str("**");
    if let Some(s) = b
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push_str(" · ");
        out.push_str(&sanitize_inline_code(s));
    }
    for step in &b.steps {
        out.push_str("\n- ");
        out.push_str(&render_breadcrumb_line(step));
    }
    out
}

/// ASCII-only status marker (no-emoji rule: Mattermost renders bare
/// Unicode check/cross glyphs as coloured emoji on mobile), looked up
/// through the transcript vocabulary ([`vocab::for_channel`] binds
/// `mattermost` to [`vocab::ASCII`], byte-identical to the literals
/// this renderer hardcoded before M22 A4).
fn breadcrumb_marker(status: BreadcrumbStatus) -> &'static str {
    vocab::for_channel(CHANNEL_TYPE_STR).rail.for_status(status)
}

/// A Mattermost inline-code span terminates on the next backtick, so a
/// stray backtick in an agent's command string would prematurely close
/// the chip. Swap backticks for apostrophes; also fold newlines so a
/// multi-line detail can't break the one-line chip.
fn sanitize_inline_code(s: &str) -> String {
    s.replace('`', "'").replace(['\n', '\r'], " ")
}

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
    // ASCII checkbox glyphs via the vocab binding, per the project's
    // no-emoji rule. Deliberate divergence for `InProgress`: Markdown
    // task lists have no half-checked box, so the active item reuses
    // the *pending* glyph (`[ ]`) plus the `_(in progress)_` suffix —
    // byte-identical to the pre-vocab output.
    let todo = vocab::for_channel(CHANNEL_TYPE_STR).todo;
    for item in &list.items {
        out.push('\n');
        out.push_str("- ");
        let text = item.text.trim();
        match item.status {
            TodoItemStatus::Completed => {
                out.push_str(todo.completed);
                out.push_str(" ~~");
                out.push_str(text);
                out.push_str("~~");
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
            title: Some("Plan".into()),
        };
        assert_eq!(
            render_todo_list(&list),
            "**Plan** (1/4)\n\
             - [x] ~~done item~~\n\
             - [ ] active item _(in progress)_\n\
             - [!] stuck item _(blocked: waiting on API key)_\n\
             - [ ] later item"
        );
    }

    #[test]
    fn breadcrumb_marker_is_byte_identical_to_pre_vocab_literals() {
        // Same M22 A4 byte-identity gate for the breadcrumb rail
        // markers: hardcoded literal expectations, not vocab constants.
        assert_eq!(breadcrumb_marker(BreadcrumbStatus::Running), "[~]");
        assert_eq!(breadcrumb_marker(BreadcrumbStatus::Done), "[ok]");
        assert_eq!(breadcrumb_marker(BreadcrumbStatus::Failed), "[x]");
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
    fn breadcrumb_running_wraps_tool_in_inline_code_with_marker() {
        let b = Breadcrumb::running("shell").with_detail("cargo check");
        let out = render_breadcrumb(&b);
        assert_eq!(out, "[~] `shell` · cargo check");
    }

    #[test]
    fn breadcrumb_done_with_summary_uses_em_dash() {
        let b = Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("passed (0.4s)".into()));
        let out = render_breadcrumb(&b);
        assert_eq!(out, "[ok] `shell` · cargo check — passed (0.4s)");
    }

    #[test]
    fn breadcrumb_failed_prefixes_summary_with_failed() {
        let b = Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(false, Some("timeout".into()));
        let out = render_breadcrumb(&b);
        assert_eq!(out, "[x] `shell` · cargo check — failed: timeout");
    }

    #[test]
    fn breadcrumb_sanitizes_backticks_in_tool_and_detail() {
        let b = Breadcrumb::running("sh`ell").with_detail("echo `id`");
        let out = render_breadcrumb(&b);
        assert!(!out.contains("sh`ell"));
        assert!(out.contains("`sh'ell`"));
        assert!(out.contains("echo 'id'"));
    }

    #[test]
    fn breadcrumb_aggregate_renders_bold_summary_and_step_bullets() {
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
        assert!(out.starts_with("**shell cargo build**"), "{out}");
        assert!(
            out.contains("\n- [ok] `read_file` · a.rs — 10 lines"),
            "{out}"
        );
        assert!(out.contains("\n- [~] `shell` · cargo build"), "{out}");
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
