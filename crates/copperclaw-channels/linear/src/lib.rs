//! Linear channel adapter — webhook ingress + GraphQL egress for
//! [linear.app](https://linear.app) issue comments.
//!
//! The crate exposes:
//! - [`LinearFactory`] — registers with [`copperclaw_channels_core::ChannelRegistry`].
//! - [`register`] — convenience helper for the host's `build_registry`.
//! - [`CHANNEL_TYPE_STR`] — the channel-type string (`"linear"`).
//!
//! Everything else is an implementation detail kept inside private modules.
//!
//! # Ingress
//!
//! The adapter binds an [`axum`] HTTP server at the configured host/port and
//! serves the Linear webhook at `path` (default `/linear/webhook`). Every
//! inbound request is HMAC-SHA256 verified against `Linear-Signature` using
//! the configured webhook secret. The `Linear-Delivery` header is used as
//! the dedup key (LRU 256).
//!
//! Supported events:
//!
//! - `Comment.create` → `MessageKind::Chat`, `platform_id` is the parent
//!   issue id, content text is `data.body`, sender from `data.user`.
//! - `Issue.create` → `MessageKind::Chat`, `platform_id` is the issue id,
//!   content text is `data.title + "\n\n" + data.description`.
//!
//! All other events are 200-acked without producing inbound traffic.
//!
//! # Egress
//!
//! `deliver` POSTs GraphQL mutations against `https://api.linear.app/graphql`
//! (overridable via `api_base`). The adapter routes:
//!
//! - text `OutboundMessage` → `commentCreate(input: { issueId, body, parentId })`.
//! - System `edit` action → `commentUpdate(id, input: { body })`.
//! - System `reaction` action → `reactionCreate(input: { commentId, emoji })`.
//!
//! # Errors
//!
//! Linear returns HTTP 200 with `errors[]` for logical errors; the GraphQL
//! client lifts those into typed [`AdapterError`]s:
//!
//! - HTTP 401 / 403 → [`AdapterError::Auth`].
//! - HTTP 429 (honoring `Retry-After`) → [`AdapterError::Rate`].
//! - HTTP 5xx → [`AdapterError::Transport`].
//! - GraphQL `errors[]` whose message looks rate-limited → `Rate`.
//! - GraphQL `errors[]` whose message looks auth-related → `Auth`.
//! - Other GraphQL `errors[]` → [`AdapterError::BadRequest`].
//!
//! [`AdapterError`]: copperclaw_channels_core::AdapterError
//! [`AdapterError::Auth`]: copperclaw_channels_core::AdapterError::Auth
//! [`AdapterError::Rate`]: copperclaw_channels_core::AdapterError::Rate
//! [`AdapterError::BadRequest`]: copperclaw_channels_core::AdapterError::BadRequest
//! [`AdapterError::Transport`]: copperclaw_channels_core::AdapterError::Transport

mod adapter;
mod api;
mod config;
mod events;
mod factory;
mod queries;
mod signature;

pub use adapter::LinearAdapter;
pub use api::{CommentCreateInput, CommentRef, CommentUpdateInput, LinearApi, ReactionCreateInput};
pub use config::{LinearConfig, WebhookConfig};
pub use events::router::{EventDedup, LinearEventsState, build_events_router};
pub use factory::{CHANNEL_TYPE_STR, LinearFactory, register};
pub use signature::{SignatureError, compute_signature, verify_signature};
