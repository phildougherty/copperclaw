//! `line` — channel adapter for the LINE Messaging API.
//!
//! LINE is the dominant mobile messenger in Japan, Taiwan, Thailand,
//! and Indonesia; the Messaging API is HTTP/JSON only so the whole
//! adapter is reqwest + axum with no native client.
//!
//! - **Inbound:** a webhook listener (axum) bound on the configured
//!   host/port. LINE signs every delivery with HMAC-SHA256 of the
//!   raw body keyed on the channel secret, base64-encoded into
//!   `X-Line-Signature` — see [`signature`].
//! - **Egress:** REST v2 (`/v2/bot/message/reply` and
//!   `/v2/bot/message/push`) authenticated with the Channel Access
//!   Token. Reply tokens captured during ingress get cached
//!   per-source so the adapter can use the free reply path when
//!   available and fall back to push otherwise.
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
//! - Outbound `action` values other than `post` →
//!   [`copperclaw_channels_core::AdapterError::BadRequest`].
//! - File uploads not yet implemented →
//!   [`copperclaw_channels_core::AdapterError::Unsupported`].

#![forbid(unsafe_code)]

pub mod adapter;
pub mod api;
pub mod config;
pub mod factory;
pub mod router;
pub mod signature;

pub use adapter::{LineAdapter, ReplyTokenCache};
pub use api::LineApi;
pub use config::{ConfigError, LineConfig, WebhookBind};
pub use factory::{register, LineFactory, CHANNEL_TYPE_STR};
pub use router::{build_router, RouterState};
pub use signature::{compute_base64, verify, SignatureOutcome};
