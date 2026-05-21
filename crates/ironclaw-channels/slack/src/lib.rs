//! Slack channel adapter — Events API ingress + Web API egress.
//!
//! Implements the Slack channel for ironclaw, see `PLAN.md` § 6 (T6).
//!
//! The crate exposes:
//! - [`SlackFactory`] — registers with [`ironclaw_channels_core::ChannelRegistry`].
//! - [`SlackAdapter`] — implements [`ironclaw_channels_core::ChannelAdapter`].
//! - [`SlackConfig`] — parsed configuration loaded from
//!   [`ironclaw_channels_core::ChannelSetup`] JSON.
//!
//! # Ingress
//!
//! The adapter binds an [`axum`] HTTP server at the configured host/port and
//! serves the Slack Events API webhook at `path` (default `/slack/events`).
//! Every inbound request is signature-verified with the configured signing
//! secret (HMAC-SHA256 over `v0:<timestamp>:<body>`); stale timestamps
//! (drift > 5 minutes) are rejected. The endpoint handles two payload kinds:
//!
//! - `url_verification` — returns the embedded challenge string.
//! - `event_callback` — dispatches the inner `event`. Supported events:
//!   `message` (incl. `message.channels`, `message.im`, `message.groups`,
//!   `message.mpim`, `message.app_mention`) and `app_mention`.
//!
//! Duplicate `event_id`s are suppressed via an in-memory LRU of the last 256
//! identifiers.
//!
//! # Egress
//!
//! `deliver` posts to `chat.postMessage` and returns Slack's `ts` as the
//! platform-side message id. When [`OutboundMessage::content`] carries an
//! `ephemeral_to` string the call is routed to `chat.postEphemeral` instead.
//! Attachments are uploaded with the Slack v2 file flow
//! (`files.getUploadURLExternal` → PUT to `upload_url` →
//! `files.completeUploadExternal`).
//!
//! `set_typing` calls `assistant.threads.setStatus` — this only does
//! anything in an Assistants context but is harmless otherwise.
//!
//! # Errors
//!
//! Slack returns `200 OK` with `{"ok": false, "error": "..."}` for logical
//! errors. The adapter maps:
//!
//! - `not_authed`, `invalid_auth`, `token_revoked` → [`AdapterError::Auth`].
//! - `rate_limited`, or HTTP 429 → [`AdapterError::Rate`] (honoring
//!   `Retry-After` when present).
//! - other codes → [`AdapterError::BadRequest`].
//!
//! [`AdapterError`]: ironclaw_channels_core::AdapterError
//! [`AdapterError::Auth`]: ironclaw_channels_core::AdapterError::Auth
//! [`AdapterError::Rate`]: ironclaw_channels_core::AdapterError::Rate
//! [`AdapterError::BadRequest`]: ironclaw_channels_core::AdapterError::BadRequest
//! [`OutboundMessage::content`]: ironclaw_types::OutboundMessage

mod adapter;
mod api;
mod config;
mod events;
mod factory;
mod signature;

pub use adapter::SlackAdapter;
pub use api::SlackApi;
pub use config::{SlackConfig, WebhookConfig};
pub use events::router::{EventDedup, SlackEventsState, build_events_router};
pub use events::types::{SlackEvent, SlackEventCallback, SlackEventEnvelope};
pub use factory::{CHANNEL_TYPE_STR, SlackFactory, register};
pub use signature::{SignatureError, compute_signature, verify_signature};
