//! Canonical portable-`TodoList` schema shared by every channel adapter.
//!
//! A "todo list" is the structured shape the runner emits whenever the
//! agent invokes `todo_add` / `todo_update` / `todo_delete`. Today those
//! tools surface a plain-text list in chat on every mutation; the
//! `MessageKind::TodoList` outbound path wraps the *full* post-mutation
//! list in this schema so adapters can render it natively (Telegram
//! `editMessageText` `MarkdownV2`, Slack Block Kit `section`, Discord
//! embed fields, Google Chat Cards v2, Matrix `m.replace`) and — on
//! platforms that support edit-in-place — REPLACE the prior list chip
//! rather than spamming a new message every time the agent moves an
//! item to `in_progress`.
//!
//! The runner emits one of these via [`MessageKind::TodoList`](
//! copperclaw_types::MessageKind::TodoList) after each MCP `todo_*`
//! mutation; the host's delivery service routes it through the
//! adapter's [`crate::ChannelAdapter::deliver_todo_list`] hook with
//! the prior list's platform message id (when one exists) so the chip
//! can be edited in place.
//!
//! On the first emit per session the host asks the adapter to *pin*
//! the list (Telegram `pinChatMessage`, Slack `pins.add`, Matrix
//! `m.room.pinned_events`); on completion (every item `Completed`)
//! the adapter is asked to *unpin*. Channels that lack a pin API
//! (Discord — bots can't pin; Google Chat — no public pin API)
//! silently treat the pin hint as a no-op.
//!
//! # Field caps
//!
//! Caps keep the payload bounded so a runaway agent can't fill the
//! database with a 10 000-item list:
//!
//! - `items` ≤ [`MAX_ITEMS`].
//! - `title` ≤ [`MAX_TITLE_CHARS`].
//! - `text` per item ≤ [`MAX_ITEM_TEXT_CHARS`].
//! - Item `id`s must be unique within the list (the validator rejects
//!   duplicates so the renderer can use the id as a stable handle).

use crate::vocab::{self, Vocabulary};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// Maximum [`TodoList::items`] length.
pub const MAX_ITEMS: usize = 50;
/// Maximum characters in [`TodoList::title`].
pub const MAX_TITLE_CHARS: usize = 64;
/// Maximum characters in [`TodoListItem::text`].
pub const MAX_ITEM_TEXT_CHARS: usize = 200;
/// Default title used when the list arrives without one. Renderers use
/// this for the chip header so users see something meaningful even when
/// the agent never set a title.
pub const DEFAULT_TITLE: &str = "Plan";

/// Lifecycle of a single todo item.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TodoItemStatus {
    /// Not started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Stuck — the agent hit a wall it can't clear on its own (e.g. it
    /// burned its verify fix-cycles). Distinct from `InProgress` so a
    /// user watching the pinned checklist sees the step is stalled, not
    /// silently churning. The reason lives on
    /// [`TodoListItem::blocked_reason`].
    Blocked,
    /// Finished.
    Completed,
}

impl TodoItemStatus {
    /// Stable `snake_case` tag used by adapters that render the status
    /// inline (e.g. Slack's emoji prefix, Matrix's chip prefix).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
        }
    }

    /// Parse a `snake_case` tag back into the enum. Returns `None` for
    /// unknown strings so callers can surface the error.
    pub fn parse_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "blocked" => Self::Blocked,
            "completed" => Self::Completed,
            _ => return None,
        })
    }

    /// Plain-text glyph used by [`TodoList::to_text_fallback`].
    /// ASCII so it survives every channel's encoding without
    /// substitution surprises. The literals live in the transcript
    /// vocabulary ([`vocab::ASCII`]) — the single source of truth for
    /// these strings (M22 A2); this delegates so the bytes cannot drift.
    pub fn glyph(self) -> &'static str {
        vocab::ASCII.todo.get(self)
    }
}

impl fmt::Display for TodoItemStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One todo item in a [`TodoList`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoListItem {
    /// Stable per-list identifier. Renderers use this as a handle so
    /// item-level state survives a mutation (e.g. Slack's checkbox
    /// `accessory.action_id` references this id).
    pub id: u32,
    /// User-visible item description.
    pub text: String,
    /// Lifecycle status.
    pub status: TodoItemStatus,
    /// One-line reason the item is [`TodoItemStatus::Blocked`], surfaced
    /// to the user next to the blocked glyph on rich surfaces and in the
    /// text fallback. `None` for every other status. `#[serde(default)]`
    /// keeps older rows (which never carried the field) decoding cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

impl TodoListItem {
    /// The trimmed, non-empty block reason to surface to the user — but
    /// only when the item is actually [`TodoItemStatus::Blocked`]. Returns
    /// `None` for any other status (or a blank/absent reason), so every
    /// adapter's renderer can uniformly decide whether to append a
    /// "(blocked: …)" suffix without re-implementing the gate.
    pub fn blocked_reason_text(&self) -> Option<&str> {
        if self.status != TodoItemStatus::Blocked {
            return None;
        }
        self.blocked_reason
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty())
    }
}

/// The portable todo-list schema. One instance per outbound row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoList {
    /// Items in display order — the agent's preferred order, not a
    /// sort. Renderers MUST preserve this order.
    pub items: Vec<TodoListItem>,
    /// Optional title. When `None`, renderers default to
    /// [`DEFAULT_TITLE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Errors raised by [`TodoList::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoListError {
    /// `items` was empty.
    EmptyList,
    /// `items.len()` exceeded [`MAX_ITEMS`].
    TooManyItems { len: usize, max: usize },
    /// Two items shared the same `id`.
    DuplicateId { id: u32 },
    /// An item's `text` was empty (after trim).
    EmptyItemText { id: u32 },
    /// An item's `text` exceeded [`MAX_ITEM_TEXT_CHARS`].
    ItemTextTooLong { id: u32, len: usize, max: usize },
    /// `title` exceeded [`MAX_TITLE_CHARS`].
    TitleTooLong { len: usize, max: usize },
}

impl fmt::Display for TodoListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyList => write!(f, "todo list must contain at least one item"),
            Self::TooManyItems { len, max } => {
                write!(f, "todo list has {len} items (max {max})")
            }
            Self::DuplicateId { id } => write!(f, "todo list contains duplicate id {id}"),
            Self::EmptyItemText { id } => {
                write!(f, "todo list item id={id} has empty text")
            }
            Self::ItemTextTooLong { id, len, max } => {
                write!(f, "todo list item id={id} text is {len} chars (max {max})")
            }
            Self::TitleTooLong { len, max } => {
                write!(f, "todo list title is {len} chars (max {max})")
            }
        }
    }
}

impl std::error::Error for TodoListError {}

impl TodoList {
    /// Build an empty-titled list with no items. The validator will
    /// reject it until at least one item is pushed; provided for
    /// builder-style construction.
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            title: None,
        }
    }

    /// Resolve the title: returns the explicit value or [`DEFAULT_TITLE`].
    pub fn title_or_default(&self) -> &str {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_TITLE)
    }

    /// `true` when every item is [`TodoItemStatus::Completed`]. Used by
    /// the host's delivery service to decide whether to ask the adapter
    /// to *unpin* the chip (Telegram `unpinChatMessage`, Slack
    /// `pins.remove`).
    pub fn is_fully_completed(&self) -> bool {
        !self.items.is_empty()
            && self
                .items
                .iter()
                .all(|i| i.status == TodoItemStatus::Completed)
    }

    /// Count of items still [`TodoItemStatus::Pending`]. Used by the
    /// text-fallback footer.
    pub fn pending_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == TodoItemStatus::Pending)
            .count()
    }

    /// Count of items currently [`TodoItemStatus::InProgress`].
    pub fn in_progress_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == TodoItemStatus::InProgress)
            .count()
    }

    /// Count of items already [`TodoItemStatus::Completed`].
    pub fn completed_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == TodoItemStatus::Completed)
            .count()
    }

    /// Count of items currently [`TodoItemStatus::Blocked`].
    pub fn blocked_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.status == TodoItemStatus::Blocked)
            .count()
    }

    /// Apply every schema rule. Returns the first violation so callers
    /// can surface it directly to the runner / model.
    pub fn validate(&self) -> Result<(), TodoListError> {
        if self.items.is_empty() {
            return Err(TodoListError::EmptyList);
        }
        if self.items.len() > MAX_ITEMS {
            return Err(TodoListError::TooManyItems {
                len: self.items.len(),
                max: MAX_ITEMS,
            });
        }
        if let Some(t) = &self.title {
            let tlen = t.chars().count();
            if tlen > MAX_TITLE_CHARS {
                return Err(TodoListError::TitleTooLong {
                    len: tlen,
                    max: MAX_TITLE_CHARS,
                });
            }
        }
        let mut seen: HashSet<u32> = HashSet::with_capacity(self.items.len());
        for item in &self.items {
            if !seen.insert(item.id) {
                return Err(TodoListError::DuplicateId { id: item.id });
            }
            if item.text.trim().is_empty() {
                return Err(TodoListError::EmptyItemText { id: item.id });
            }
            let tlen = item.text.chars().count();
            if tlen > MAX_ITEM_TEXT_CHARS {
                return Err(TodoListError::ItemTextTooLong {
                    id: item.id,
                    len: tlen,
                    max: MAX_ITEM_TEXT_CHARS,
                });
            }
        }
        Ok(())
    }

    /// Plain-text rendering used by the default
    /// [`crate::ChannelAdapter::deliver_todo_list`] fallback. One line
    /// per item, each prefixed by the status glyph; trailing footer
    /// summarises how many items remain.
    ///
    /// Example:
    ///
    /// ```text
    /// Plan
    /// [x] Wash dishes
    /// [~] Dry dishes
    /// [ ] Put dishes away
    /// (1/3 done, 1 in progress, 1 pending)
    /// ```
    pub fn to_text_fallback(&self) -> String {
        self.to_text_fallback_with(&vocab::ASCII)
    }

    /// [`Self::to_text_fallback`] parameterised over a transcript
    /// [`Vocabulary`] (M22 A2). With [`vocab::ASCII`] the output is
    /// byte-identical to the historical renderer; other bindings swap
    /// the per-item status checkboxes.
    ///
    /// The `" — "` blocked-reason join stays hardcoded: `Layout`'s
    /// `separator` is the status-line *field* joiner (`" | "` on ASCII),
    /// not this em-dash reason join, so routing it through the vocabulary
    /// would change bytes. It can move once `Layout` grows a slot with
    /// the right granularity.
    pub(crate) fn to_text_fallback_with(&self, vocab: &Vocabulary) -> String {
        let mut out = String::with_capacity(64 + self.items.len() * 32);
        out.push_str(self.title_or_default());
        for item in &self.items {
            out.push('\n');
            out.push_str(vocab.todo.get(item.status));
            out.push(' ');
            out.push_str(item.text.trim());
            // Surface the one-line block reason inline so a stuck step is
            // legible even on bare / text-fallback surfaces.
            if let Some(reason) = item.blocked_reason_text() {
                out.push_str(" — ");
                out.push_str(reason);
            }
        }
        let done = self.completed_count();
        let in_prog = self.in_progress_count();
        let blocked = self.blocked_count();
        let pending = self.pending_count();
        let total = self.items.len();
        if total > 0 {
            // `blocked` is inserted only when non-zero so lists without a
            // blocked item stay byte-identical to the pre-F4 footer.
            let blocked_frag = if blocked > 0 {
                format!(", {blocked} blocked")
            } else {
                String::new()
            };
            out.push_str(&format!(
                "\n({done}/{total} done, {in_prog} in progress{blocked_frag}, {pending} pending)"
            ));
        }
        out
    }

    /// Compact pinned-chip plaintext rendering for bare adapters (Delta
    /// Chat, LINE): a `title (done/total)` header with the counter hoisted
    /// up top for the glance, then one glyph-prefixed line per item. A
    /// [`TodoItemStatus::Blocked`] item carries its `blocked_reason` inline
    /// (`— blocked: <reason>`) so a stalled step reads as blocked, not
    /// stuck "in progress". No footer — the header counter carries the
    /// summary. No trailing newline.
    pub fn to_chip_plaintext(&self) -> String {
        let done = self.completed_count();
        let total = self.items.len();
        let mut out = String::with_capacity(64 + self.items.len() * 32);
        out.push_str(self.title_or_default());
        out.push_str(&format!(" ({done}/{total})"));
        for item in &self.items {
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
}

impl Default for TodoList {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u32, text: &str, status: TodoItemStatus) -> TodoListItem {
        TodoListItem {
            id,
            text: text.into(),
            status,
            blocked_reason: None,
        }
    }

    fn blocked_item(id: u32, text: &str, reason: &str) -> TodoListItem {
        TodoListItem {
            id,
            text: text.into(),
            status: TodoItemStatus::Blocked,
            blocked_reason: Some(reason.into()),
        }
    }

    fn sample() -> TodoList {
        TodoList {
            items: vec![
                item(1, "Wash dishes", TodoItemStatus::Completed),
                item(2, "Dry dishes", TodoItemStatus::InProgress),
                item(3, "Put dishes away", TodoItemStatus::Pending),
            ],
            title: Some("Kitchen".into()),
        }
    }

    #[test]
    fn validate_happy_path() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_list() {
        assert_eq!(
            TodoList::new().validate().unwrap_err(),
            TodoListError::EmptyList
        );
    }

    #[test]
    fn validate_rejects_too_many_items() {
        let cap = u32::try_from(MAX_ITEMS).expect("MAX_ITEMS fits in u32");
        let items: Vec<_> = (0..=cap)
            .map(|i| item(i, "x", TodoItemStatus::Pending))
            .collect();
        let list = TodoList { items, title: None };
        assert!(matches!(
            list.validate(),
            Err(TodoListError::TooManyItems { .. })
        ));
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let list = TodoList {
            items: vec![
                item(1, "a", TodoItemStatus::Pending),
                item(1, "b", TodoItemStatus::Pending),
            ],
            title: None,
        };
        assert_eq!(
            list.validate().unwrap_err(),
            TodoListError::DuplicateId { id: 1 }
        );
    }

    #[test]
    fn validate_rejects_empty_item_text() {
        let list = TodoList {
            items: vec![item(1, "   ", TodoItemStatus::Pending)],
            title: None,
        };
        assert_eq!(
            list.validate().unwrap_err(),
            TodoListError::EmptyItemText { id: 1 }
        );
    }

    #[test]
    fn validate_rejects_item_text_too_long() {
        let list = TodoList {
            items: vec![item(
                7,
                &"x".repeat(MAX_ITEM_TEXT_CHARS + 1),
                TodoItemStatus::Pending,
            )],
            title: None,
        };
        assert!(matches!(
            list.validate(),
            Err(TodoListError::ItemTextTooLong { id: 7, .. })
        ));
    }

    #[test]
    fn validate_rejects_title_too_long() {
        let list = TodoList {
            items: vec![item(1, "a", TodoItemStatus::Pending)],
            title: Some("t".repeat(MAX_TITLE_CHARS + 1)),
        };
        assert!(matches!(
            list.validate(),
            Err(TodoListError::TitleTooLong { .. })
        ));
    }

    #[test]
    fn title_or_default_falls_back_to_default() {
        let list = TodoList {
            items: vec![item(1, "a", TodoItemStatus::Pending)],
            title: None,
        };
        assert_eq!(list.title_or_default(), DEFAULT_TITLE);
        let list_blank = TodoList {
            items: vec![item(1, "a", TodoItemStatus::Pending)],
            title: Some("   ".into()),
        };
        assert_eq!(list_blank.title_or_default(), DEFAULT_TITLE);
    }

    #[test]
    fn title_or_default_returns_set_title() {
        let list = TodoList {
            items: vec![item(1, "a", TodoItemStatus::Pending)],
            title: Some("Migration".into()),
        };
        assert_eq!(list.title_or_default(), "Migration");
    }

    #[test]
    fn is_fully_completed_only_when_all_completed() {
        let list = sample();
        assert!(!list.is_fully_completed());
        let done = TodoList {
            items: vec![
                item(1, "a", TodoItemStatus::Completed),
                item(2, "b", TodoItemStatus::Completed),
            ],
            title: None,
        };
        assert!(done.is_fully_completed());
        let empty = TodoList::new();
        assert!(!empty.is_fully_completed());
    }

    #[test]
    fn counts_match_items() {
        let list = sample();
        assert_eq!(list.completed_count(), 1);
        assert_eq!(list.in_progress_count(), 1);
        assert_eq!(list.pending_count(), 1);
    }

    #[test]
    fn text_fallback_includes_glyphs_and_footer() {
        let out = sample().to_text_fallback();
        assert!(out.starts_with("Kitchen\n"));
        assert!(out.contains("[x] Wash dishes"));
        assert!(out.contains("[~] Dry dishes"));
        assert!(out.contains("[ ] Put dishes away"));
        assert!(out.ends_with("(1/3 done, 1 in progress, 1 pending)"));
    }

    #[test]
    fn text_fallback_uses_default_title_when_unset() {
        let list = TodoList {
            items: vec![item(1, "single", TodoItemStatus::Pending)],
            title: None,
        };
        let out = list.to_text_fallback();
        assert!(out.starts_with(&format!("{DEFAULT_TITLE}\n")));
    }

    #[test]
    fn status_round_trips_through_str() {
        for s in [
            TodoItemStatus::Pending,
            TodoItemStatus::InProgress,
            TodoItemStatus::Completed,
        ] {
            assert_eq!(TodoItemStatus::parse_str(s.as_str()), Some(s));
        }
        assert_eq!(TodoItemStatus::parse_str("nope"), None);
    }

    #[test]
    fn status_glyphs_are_distinct() {
        let glyphs = [
            TodoItemStatus::Pending.glyph(),
            TodoItemStatus::InProgress.glyph(),
            TodoItemStatus::Blocked.glyph(),
            TodoItemStatus::Completed.glyph(),
        ];
        let unique: HashSet<&str> = glyphs.iter().copied().collect();
        assert_eq!(unique.len(), glyphs.len(), "every status glyph is distinct");
    }

    #[test]
    fn blocked_status_round_trips_and_serdes() {
        assert_eq!(
            TodoItemStatus::parse_str("blocked"),
            Some(TodoItemStatus::Blocked)
        );
        assert_eq!(TodoItemStatus::Blocked.as_str(), "blocked");
        let s = serde_json::to_string(&TodoItemStatus::Blocked).unwrap();
        assert_eq!(s, "\"blocked\"");
    }

    #[test]
    fn text_fallback_renders_blocked_glyph_reason_and_footer() {
        let list = TodoList {
            items: vec![
                item(1, "Wash dishes", TodoItemStatus::Completed),
                item(2, "Dry dishes", TodoItemStatus::InProgress),
                blocked_item(3, "Deploy", "verify failed after 3 fix cycles"),
                item(4, "Sweep floor", TodoItemStatus::Pending),
            ],
            title: Some("Chores".into()),
        };
        let out = list.to_text_fallback();
        assert!(
            out.contains("[!] Deploy — verify failed after 3 fix cycles"),
            "blocked line shows glyph + reason: {out}"
        );
        assert!(
            out.ends_with("(1/4 done, 1 in progress, 1 blocked, 1 pending)"),
            "footer counts blocked: {out}"
        );
        assert_eq!(list.blocked_count(), 1);
    }

    #[test]
    fn text_fallback_footer_omits_blocked_when_none() {
        // Byte-stability: a list with no blocked item keeps the pre-F4 footer.
        let out = sample().to_text_fallback();
        assert!(out.ends_with("(1/3 done, 1 in progress, 1 pending)"));
    }

    #[test]
    fn blocked_item_serde_carries_reason() {
        let list = TodoList {
            items: vec![blocked_item(1, "Build", "compile error")],
            title: None,
        };
        let s = serde_json::to_string(&list).unwrap();
        assert!(s.contains("\"blocked_reason\":\"compile error\""), "{s}");
        let back: TodoList = serde_json::from_str(&s).unwrap();
        assert_eq!(back, list);
        // Older rows without the field still decode (defaults to None).
        let legacy = r#"{"items":[{"id":1,"text":"x","status":"blocked"}]}"#;
        let decoded: TodoList = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.items[0].blocked_reason, None);
    }

    #[test]
    fn status_serde_snake_case() {
        let s = serde_json::to_string(&TodoItemStatus::InProgress).unwrap();
        assert_eq!(s, "\"in_progress\"");
        let back: TodoItemStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back, TodoItemStatus::InProgress);
    }

    #[test]
    fn todo_list_serde_roundtrip() {
        let list = sample();
        let s = serde_json::to_string(&list).unwrap();
        let back: TodoList = serde_json::from_str(&s).unwrap();
        assert_eq!(list, back);
    }

    #[test]
    fn todo_list_serde_skips_none_title() {
        let list = TodoList {
            items: vec![item(1, "a", TodoItemStatus::Pending)],
            title: None,
        };
        let s = serde_json::to_string(&list).unwrap();
        assert!(!s.contains("title"), "title=None should not appear: {s}");
    }

    #[test]
    fn glyph_literals_are_byte_identical_to_pre_vocab_output() {
        // Hardcoded expected literals (NOT read through vocab — that
        // would be circular) pinning `glyph()` after its M22 A2 rerouting
        // through `vocab::ASCII.todo`.
        assert_eq!(TodoItemStatus::Pending.glyph(), "[ ]");
        assert_eq!(TodoItemStatus::InProgress.glyph(), "[~]");
        assert_eq!(TodoItemStatus::Blocked.glyph(), "[!]");
        assert_eq!(TodoItemStatus::Completed.glyph(), "[x]");
    }

    #[test]
    fn text_fallback_with_ascii_is_byte_identical_to_pre_vocab_output() {
        // The full expected string is a hardcoded literal of the exact
        // pre-A2 output — byte-identity proof for the vocab rerouting.
        let expected = "Kitchen\n\
                        [x] Wash dishes\n\
                        [~] Dry dishes\n\
                        [ ] Put dishes away\n\
                        (1/3 done, 1 in progress, 1 pending)";
        assert_eq!(sample().to_text_fallback(), expected);
        assert_eq!(
            sample().to_text_fallback_with(&crate::vocab::ASCII),
            expected
        );
    }

    #[test]
    fn text_fallback_with_ascii_blocked_is_byte_identical_to_pre_vocab_output() {
        let list = TodoList {
            items: vec![
                item(1, "Wash dishes", TodoItemStatus::Completed),
                blocked_item(2, "Deploy", "verify failed"),
            ],
            title: Some("Chores".into()),
        };
        // Pins the hardcoded " — " reason join alongside the vocab-driven
        // glyphs (the em dash has no Layout slot; see to_text_fallback_with).
        let expected = "Chores\n\
                        [x] Wash dishes\n\
                        [!] Deploy — verify failed\n\
                        (1/2 done, 0 in progress, 1 blocked, 0 pending)";
        assert_eq!(list.to_text_fallback(), expected);
        assert_eq!(list.to_text_fallback_with(&crate::vocab::ASCII), expected);
    }

    #[test]
    fn error_display_messages_are_non_empty() {
        let cases = [
            TodoListError::EmptyList,
            TodoListError::TooManyItems { len: 99, max: 50 },
            TodoListError::DuplicateId { id: 3 },
            TodoListError::EmptyItemText { id: 4 },
            TodoListError::ItemTextTooLong {
                id: 5,
                len: 999,
                max: 200,
            },
            TodoListError::TitleTooLong { len: 99, max: 64 },
        ];
        for err in cases {
            assert!(!format!("{err}").is_empty());
        }
    }
}
