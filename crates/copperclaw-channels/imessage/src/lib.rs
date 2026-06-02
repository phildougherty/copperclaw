// "AppleScript", "Messages.app", "iMessage", and "SQLite" are everywhere
// in this crate's docs. They trip clippy's `doc_markdown` lint (which wants
// every camelCase or dotted identifier in backticks) — there are scores of
// occurrences and they make the prose noisier rather than clearer. We
// allow the lint at the crate level so the docs read naturally.
#![allow(clippy::doc_markdown)]

//! `iMessage` channel adapter — talks to the local macOS Messages.app via
//! `osascript` for outbound delivery and SQLite reads from
//! `~/Library/Messages/chat.db` for inbound polling.
//!
//! Implements [`IMessageFactory`] / [`IMessageAdapter`] backing
//! `ChannelType::new("imessage")`.
//!
//! ## Outbound (`AppleScript`)
//!
//! For 1:1 chats:
//!
//! ```applescript
//! tell application "Messages"
//!   set targetService to 1st service whose service type = iMessage
//!   set targetBuddy to buddy "<handle>" of targetService
//!   send "<text>" to targetBuddy
//! end tell
//! ```
//!
//! For groups (chat-style) we use `send <text> to chat id "<chat-guid>"`,
//! and for files we use `send POSIX file "<path>" to ...`. All user input
//! is template-substituted into a single `AppleScript` document with the
//! escaping rules in [`applescript::applescript_escape`].
//!
//! ## Inbound (`SQLite` poll)
//!
//! `Messages.app` stores its message log in
//! `~/Library/Messages/chat.db`. The adapter polls via `sqlite3` shelled out
//! to that path, reading the `message`, `handle`, `chat`, and
//! `chat_message_join` tables. The high-water mark is the `SQLite` `ROWID`
//! of the last message seen, persisted between runs as
//! `<data_dir>/imessage_since_rowid.txt`.
//!
//! The `message.date` column is in Cocoa-epoch *nanoseconds* since
//! 2001-01-01 UTC (older versions of `Messages.app` used seconds; the
//! converter in [`parse::cocoa_to_utc`] auto-detects). See the comment on
//! that function for the conversion details.
//!
//! ## `platform_id` shape
//!
//! - `"handle:<email-or-phone>"` — 1:1 with a buddy
//! - `"chat:<chat-guid>"`        — group chat
//!
//! Any other shape returns [`AdapterError::BadRequest`] from `deliver`.
//!
//! ## Unsupported operations
//!
//! - `edit_message` / `add_reaction` — `Messages.app` has tapbacks but the
//!   `AppleScript` surface for them is unreliable, so both return
//!   [`AdapterError::Unsupported`].
//! - `set_typing` — `Messages.app` has no third-party typing API, so this
//!   is a silent no-op (inherited default).
//!
//! ## Testing
//!
//! The full subprocess surface is hidden behind the [`IMessageBridge`]
//! trait. Tests construct an adapter against [`testing::MockBridge`] so
//! nothing in the unit-test suite touches `osascript` or `sqlite3`. See the
//! [`testing`] module for the mock surface.
//!
//! [`AdapterError`]: copperclaw_channels_core::AdapterError

pub mod adapter;
pub mod applescript;
pub mod bridge;
pub mod bridge_osascript;
pub mod config;
pub mod factory;
pub mod parse;
pub mod poll;

pub use adapter::IMessageAdapter;
pub use bridge::IMessageBridge;
pub use config::{
    DEFAULT_CHAT_DB_PATH, DEFAULT_POLL_INTERVAL_MS, DEFAULT_SERVICE_NAME, DEFAULT_SINCE_ROWID_FILE,
    IMessageConfig,
};
pub use factory::{CHANNEL_TYPE_STR, IMessageFactory, register};

/// Re-exported testing fixtures so downstream crates can drive the adapter
/// against a [`testing::MockBridge`] without ever touching `osascript` or
/// `sqlite3`.
pub mod testing {
    pub use crate::bridge::{MockBridge, MockMessageRow};
}
