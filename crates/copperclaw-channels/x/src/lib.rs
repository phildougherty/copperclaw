//! Twitter / X channel adapter.
//!
//! Implements [`XFactory`] / [`XAdapter`] backing `ChannelType::new("x")`.
//! The v1 scope is intentionally narrow:
//!
//! - **DMs only.** Tweets are out of scope; X v2's `dm_events` polling is
//!   the most realistic agent-channel surface today.
//! - **Bearer-token auth.** The operator supplies a user-context `OAuth2`
//!   bearer with `dm.read` / `dm.write` scopes; the channel does not
//!   perform the `OAuth` dance itself.
//! - **Polling ingress.** `/2/dm_events/stream` is not generally available,
//!   so the adapter polls `/2/dm_events` on a configurable interval and
//!   persists the rolling `since_id` to disk (default
//!   `data_dir/x_dm_since_id.txt`).
//! - **Media via v1.1.** X has not migrated media upload to v2; the
//!   adapter uses `upload.twitter.com/1.1/media/upload.json` for
//!   attachments. One attachment per DM; multi-file outbound rows fan out
//!   to one DM per file (text on the first only).
//!
//! ## `platform_id` shapes
//!
//! - `"user:<participant_id>"` — POST to
//!   `/2/dm_conversations/with/<id>/messages`.
//! - `"conversation:<dm_conversation_id>"` — POST to
//!   `/2/dm_conversations/<id>/messages`.
//!
//! Other forms are rejected with [`copperclaw_channels_core::AdapterError::BadRequest`].
//!
//! ## Unsupported operations
//!
//! - `set_typing` is a no-op (X v2 has no DM typing API).
//! - `edit` and `reaction` system actions return
//!   [`copperclaw_channels_core::AdapterError::Unsupported`].

pub mod adapter;
pub mod api;
pub mod config;
pub mod factory;
pub mod parse;
pub mod poll;

pub use adapter::XAdapter;
pub use api::XApi;
pub use config::{
    DEFAULT_API_BASE, DEFAULT_MEDIA_BASE, DEFAULT_POLL_INTERVAL_MS, DEFAULT_SINCE_ID_FILENAME,
    MediaApiVersion, XConfig,
};
pub use factory::{CHANNEL_TYPE_STR, XFactory, register};
