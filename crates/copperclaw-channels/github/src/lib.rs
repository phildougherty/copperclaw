//! GitHub channel adapter — issue / PR / review comments with webhook ingress.
//!
//! Implements the GitHub channel for copperclaw, see `PLAN.md` § 6 (T6).
//!
//! The crate exposes:
//! - [`GithubFactory`] — registers with [`copperclaw_channels_core::ChannelRegistry`].
//! - [`register`] — convenience that constructs the factory and inserts it
//!   into a registry.
//! - [`CHANNEL_TYPE_STR`] — the string used as the channel-type label
//!   (`"github"`).
//!
//! # Ingress
//!
//! The adapter binds an [`axum`] HTTP server at the configured host/port and
//! serves the GitHub webhook at `path` (default `/github/webhook`). Every
//! inbound request is signature-verified with the configured shared secret
//! (HMAC-SHA256 over the raw body), and `X-GitHub-Delivery` ids are
//! deduplicated via an in-memory ring of the last 256 entries.
//!
//! Supported events (dispatched by the `X-GitHub-Event` header):
//!
//! - `ping` — handshake; responds 200 with no inbound emitted.
//! - `issue_comment` action=`created` — inbound chat message with
//!   `platform_id = "{owner}/{repo}#{issue.number}"`.
//! - `pull_request_review_comment` action=`created` — inbound chat message
//!   with `platform_id = "{owner}/{repo}#{pull_request.number}"`.
//! - `issues` action=`opened` — inbound chat message with content
//!   `title + "\n\n" + body` (body may be null → empty).
//! - Anything else — ack with 200, no inbound emitted.
//!
//! The router returns 401 on missing / bad signatures, 400 on malformed JSON
//! or missing required fields, and 200 otherwise.
//!
//! # Egress
//!
//! `deliver` translates [`copperclaw_types::OutboundMessage`] into one of three
//! REST calls keyed off the `action` field of the message content:
//!
//! - plain text → `POST /repos/{owner}/{repo}/issues/{number}/comments`,
//!   returning the new comment's id as the platform-side message id.
//! - `{"action":"edit","target_id":<id>,"text":...}` →
//!   `PATCH /repos/{owner}/{repo}/issues/comments/{id}`.
//! - `{"action":"reaction","target_id":<id>,"emoji":...}` →
//!   `POST /repos/{owner}/{repo}/issues/comments/{id}/reactions`. Emoji
//!   shortcodes are mapped to GitHub's eight reaction slugs via
//!   [`emoji::to_reaction_slug`]; unknown shortcodes surface as
//!   [`AdapterError::BadRequest`].
//!
//! File attachments on comments are unsupported and surface as
//! [`AdapterError::Unsupported`].
//!
//! # Errors
//!
//! REST responses map to [`AdapterError`] variants:
//!
//! - 401 → [`AdapterError::Auth`].
//! - 403 with `X-RateLimit-Remaining: 0` → [`AdapterError::Rate`].
//! - 403 (other) → [`AdapterError::Auth`].
//! - 404 / 422 → [`AdapterError::BadRequest`].
//! - 429 → [`AdapterError::Rate`] (honoring `Retry-After`).
//! - 5xx → [`AdapterError::Transport`].
//!
//! [`AdapterError`]: copperclaw_channels_core::AdapterError
//! [`AdapterError::Auth`]: copperclaw_channels_core::AdapterError::Auth
//! [`AdapterError::Rate`]: copperclaw_channels_core::AdapterError::Rate
//! [`AdapterError::BadRequest`]: copperclaw_channels_core::AdapterError::BadRequest
//! [`AdapterError::Unsupported`]: copperclaw_channels_core::AdapterError::Unsupported

mod adapter;
mod api;
mod config;
mod emoji;
mod events;
mod factory;
mod signature;

pub use adapter::{GithubAdapter, parse_platform_id};
pub use api::{CommentResponse, GithubApi, USER_AGENT};
pub use config::{
    DEFAULT_API_BASE, DEFAULT_HOST, DEFAULT_PATH, DEFAULT_PORT, GithubConfig, WebhookConfig,
};
pub use emoji::{VALID_REACTION_SLUGS, to_reaction_slug};
pub use events::router::{DEDUP_CAPACITY, DeliveryDedup, GithubEventsState, build_events_router};
pub use events::types::{
    Comment, IssueCommentEvent, IssueRef, IssuesEvent, PullRequestRef,
    PullRequestReviewCommentEvent, Repository, User,
};
pub use factory::{CHANNEL_TYPE_STR, GithubFactory, register};
pub use signature::{SignatureError, compute_signature, verify_signature};
