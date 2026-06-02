//! `mattermost` — channel adapter for Mattermost servers.
//!
//! Mattermost is a self-hosted Slack alternative widely used in OSS,
//! government, and security-sensitive deployments. Its API is HTTP/JSON
//! only — no native socket protocol — so this adapter is fully
//! implemented with [`reqwest`] and [`axum`]:
//!
//! - **Inbound:** an outgoing-webhook listener (axum) bound on the
//!   configured host/port. Mattermost POSTs JSON (or
//!   `application/x-www-form-urlencoded`); both shapes parse to the
//!   same payload. A configured shared `webhook_token` is compared
//!   in constant time against the `token` field on every request.
//!   `bot_user_id` (when set) suppresses the bot's own messages so
//!   the agent doesn't reply to itself.
//! - **Egress:** Mattermost REST v4 with a Personal Access Token.
//!   Three actions are wired: plain post, in-place edit, and
//!   reaction.
//!
//! See `docs/adding-a-channel.md` § 5 for the canonical HTTP-error
//! mapping the [`api`] module follows.
//!
//! # Errors
//!
//! - HTTP 401 / 403 → [`copperclaw_channels_core::AdapterError::Auth`].
//! - HTTP 400 / 404 / 422 →
//!   [`copperclaw_channels_core::AdapterError::BadRequest`].
//! - HTTP 429 → [`copperclaw_channels_core::AdapterError::Rate`].
//! - HTTP 5xx / network errors →
//!   [`copperclaw_channels_core::AdapterError::Transport`].
//! - Outbound `action` values other than `post`, `edit`, `reaction` →
//!   [`copperclaw_channels_core::AdapterError::BadRequest`].
//! - File uploads not yet implemented →
//!   [`copperclaw_channels_core::AdapterError::Unsupported`].

#![forbid(unsafe_code)]

pub mod adapter;
pub mod api;
pub mod config;
pub mod factory;
pub mod router;

pub use adapter::MattermostAdapter;
pub use api::MattermostApi;
pub use config::{ConfigError, MattermostConfig, WebhookBind};
pub use factory::{CHANNEL_TYPE_STR, MattermostFactory, register};
pub use router::{RouterState, build_router};
