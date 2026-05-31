//! Telegram channel adapter — long-poll + webhook ingress.
//!
//! Implements [`TelegramFactory`] / [`TelegramAdapter`] backing
//! `ChannelType::new("telegram")`. The adapter:
//!
//! - Runs either a long-poll background task (`getUpdates`) or an axum
//!   webhook server depending on the configured ingress mode.
//! - Sends text via `sendMessage` with a configurable Markdown variant,
//!   files via multipart `sendDocument`, and typing indicators via
//!   `sendChatAction`.
//! - Validates the bot token at factory init via `getMe`.
//!
//! ## Configuration shape
//!
//! ```json
//! {
//!   "bot_token": "...",
//!   "mode": "long_poll" | "webhook",
//!   "long_poll": { "timeout_secs": 60, "limit": 100, "allowed_updates": [] },
//!   "webhook":   { "host": "0.0.0.0", "port": 8081, "path": "/telegram",
//!                  "secret_token": "..." },
//!   "api_base":  "https://api.telegram.org",
//!   "attachment_download":   true,
//!   "max_attachment_bytes":  20971520
//! }
//! ```
//!
//! Exactly one of `long_poll` / `webhook` may be populated; `mode` is
//! optional when only one block is present. `api_base` defaults to the
//! production Telegram endpoint and is overridable for tests.
//!
//! ## File handling
//!
//! Inbound `document`, `photo` (largest variant), `audio`, `video`,
//! `voice`, `video_note`, and `sticker` attachments are downloaded
//! eagerly via `getFile` + the file endpoint and written under
//! `data_dir/inbox/<msg_id>/<filename>`. The resulting event is a
//! [`MessageKind::Chat`](copperclaw_types::MessageKind::Chat) with the
//! caption (or text) plus a `content.attachment` object carrying
//! `{kind, file_id, filename, path, mime_type, size}`.
//!
//! Two knobs control this:
//!
//! - `attachment_download` (default `true`) — when `false`, the adapter
//!   falls back to the legacy metadata-only behaviour, surfacing
//!   attachments as [`MessageKind::System`](copperclaw_types::MessageKind::System)
//!   events.
//! - `max_attachment_bytes` (default 20 MB — the Bot API's own hard cap)
//!   — files exceeding this limit are surfaced as `MessageKind::System`
//!   with `reason: "too_large"`. The same fallback path is used when
//!   `getFile` or the binary download itself fails; the error is
//!   captured under `content.error` so the host can log / alert.

pub mod adapter;
pub mod api;
pub mod config;
pub mod factory;
pub mod ingress;
pub mod types;

pub use adapter::{CHANNEL_TYPE_STR, DEFAULT_PARSE_MODE, TelegramAdapter};
pub use api::TelegramApi;
pub use config::{
    DEFAULT_API_BASE, DEFAULT_LONG_POLL_LIMIT, DEFAULT_LONG_POLL_TIMEOUT_SECS,
    DEFAULT_MAX_ATTACHMENT_BYTES, DEFAULT_WEBHOOK_HOST, DEFAULT_WEBHOOK_PATH,
    DEFAULT_WEBHOOK_PORT, IngressMode, LongPollConfig, TelegramConfig, WebhookConfig,
};
pub use factory::{TelegramFactory, register};
