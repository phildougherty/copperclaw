//! Channel adapter trait + registry.
//!
//! See `PLAN.md` § 5.1.
//!
//! Public API:
//! - [`ChannelAdapter`] / [`ChannelFactory`] traits.
//! - [`ChannelRegistry`] in-process factory lookup.
//! - [`ChannelSetup`] — the per-instance init context (config, mpsc sender,
//!   per-channel data dir) every factory receives.
//! - [`ContainerContribution`] (+ local [`Mount`]) — what a channel adds to
//!   the agent container environment.
//! - [`DmHandle`] — result of [`ChannelAdapter::open_dm`].
//! - [`AdapterError`] — single error type for all adapter and factory calls.
//! - [`Card`] (+ [`CardField`], [`CardButton`], [`CardError`]) — portable
//!   card schema rendered natively by adapters with rich card support and
//!   degraded to plain text everywhere else.
//! - [`Breadcrumb`] (+ [`BreadcrumbStatus`], [`BreadcrumbError`]) — portable
//!   tool-progress chip schema rendered natively (small chips, edit-in-place)
//!   by adapters with rich support and degraded to a text line everywhere
//!   else.
//! - [`ErrorCard`] (+ [`ErrorCardKind`], [`ErrorCardError`]) — portable
//!   host-emitted error-card schema rendered red where the platform
//!   supports it (Slack `attachments.color`, Discord embed `color`,
//!   Matrix `<font color="red">`, Google Chat decorated icon) and as a
//!   `[ERROR]`-prefixed bold/monospace block everywhere else. Distinct
//!   from [`AdapterError`] (the trait-method error type) — `ErrorCard`
//!   is *the message body*, not *the call-site failure*; the re-exports
//!   below pull it in under explicit names to keep the two from
//!   shadowing each other.
//! - [`TodoList`] (+ [`TodoListItem`], [`TodoItemStatus`], [`TodoListError`]) —
//!   portable agent-todo schema rendered as a live, edit-in-place
//!   checklist chip (Telegram `editMessageText` `MarkdownV2`, Slack Block
//!   Kit `section`, Discord embed fields, Google Chat Cards v2, Matrix
//!   `m.replace`) and pinned on platforms that support it.
//! - [`DiffCard`] (+ [`DiffHunk`], [`DiffLine`], [`DiffLineKind`],
//!   [`BlobReplaced`], [`DiffCardError`]) — portable file-edit diff
//!   schema emitted by the runner after a successful `edit_file` /
//!   `multi_edit` / `apply_patch` / `write_file` write; rendered
//!   natively (with `+`/`-` gutters) by adapters with code-block
//!   support and degraded to a unified-diff text body everywhere else.
//! - [`ThinkingBlock`] (+ [`ThinkingBlockError`]) — opt-in canonical
//!   reasoning-block schema emitted by the runner when the provider
//!   streams a `thinking` (or `redacted_thinking`) content block AND
//!   the per-group `surface_thinking` flag is on; rendered as a
//!   collapsed UI primitive (Telegram `<blockquote expandable>`,
//!   Slack `context` block, Discord muted embed, Google Chat
//!   `collapsibleSection`, Matrix `<details>`) and degraded to a
//!   `[reasoning] > quoted text` block everywhere else. Default is
//!   off — surfacing model reasoning has privacy implications.
//! - [`testing`] — reusable [`testing::MockAdapter`] / [`testing::MockFactory`]
//!   for downstream tests.

mod adapter;
mod breadcrumb;
mod card;
mod container;
mod diff;
mod dm;
mod error;
mod error_card;
mod registry;
mod setup;
mod thinking;
mod todo_list;

pub mod testing;

pub use adapter::{ChannelAdapter, ChannelFactory, render_collapsible_text_fallback};
pub use breadcrumb::{
    Breadcrumb, BreadcrumbError, BreadcrumbStatus, MAX_DETAIL_CHARS, MAX_SUMMARY_CHARS,
    MAX_TOOL_NAME_CHARS,
};
pub use card::{
    Card, CardButton, CardError, CardField, MAX_BODY_CHARS, MAX_BUTTON_LABEL_CHARS,
    MAX_BUTTON_VALUE_BYTES, MAX_BUTTONS, MAX_FIELD_LABEL_CHARS, MAX_FIELD_VALUE_CHARS, MAX_FIELDS,
    MAX_TITLE_CHARS,
};
pub use container::{ContainerContribution, Mount};
pub use diff::{
    BLOB_DIFF_CUTOFF_BYTES, BlobReplaced, DiffCard, DiffCardError, DiffHunk, DiffLine,
    DiffLineKind, MAX_HUNKS as MAX_DIFF_HUNKS, MAX_LANGUAGE_CHARS as MAX_DIFF_LANGUAGE_CHARS,
    MAX_LINE_CHARS as MAX_DIFF_LINE_CHARS, MAX_LINES_PER_HUNK as MAX_DIFF_LINES_PER_HUNK,
    MAX_PATH_CHARS as MAX_DIFF_PATH_CHARS,
};
pub use dm::DmHandle;
pub use error::AdapterError;
// `error_card` exports are named to dodge `error::AdapterError` and
// `card::MAX_TITLE_CHARS` — both already in this namespace. Downstream
// callers therefore import the schema as `ErrorCard` and the caps as
// `MAX_ERROR_*` so there is no ambiguity at the call site.
pub use error_card::{
    ErrorCard, ErrorCardError, ErrorCardKind, MAX_DETAILS_CHARS as MAX_ERROR_DETAILS_CHARS,
    MAX_SUMMARY_CHARS as MAX_ERROR_SUMMARY_CHARS, MAX_TITLE_CHARS as MAX_ERROR_TITLE_CHARS,
};
pub use registry::ChannelRegistry;
pub use setup::ChannelSetup;
pub use thinking::{
    MAX_MODEL_CHARS as MAX_THINKING_MODEL_CHARS, MAX_THINKING_CHARS, ThinkingBlock,
    ThinkingBlockError,
};
pub use todo_list::{
    DEFAULT_TITLE as TODO_DEFAULT_TITLE, MAX_ITEM_TEXT_CHARS as TODO_MAX_ITEM_TEXT_CHARS,
    MAX_ITEMS as TODO_MAX_ITEMS, MAX_TITLE_CHARS as TODO_MAX_TITLE_CHARS, TodoItemStatus, TodoList,
    TodoListError, TodoListItem,
};
