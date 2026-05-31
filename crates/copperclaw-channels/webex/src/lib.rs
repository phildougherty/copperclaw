//! Cisco Webex channel adapter — REST egress + webhook ingress.
//!
//! The crate exposes:
//! - [`WebexFactory`] — registers with
//!   [`copperclaw_channels_core::ChannelRegistry`].
//! - [`WebexAdapter`] — implements
//!   [`copperclaw_channels_core::ChannelAdapter`].
//! - [`WebexConfig`] — parsed configuration loaded from
//!   [`copperclaw_channels_core::ChannelSetup`] JSON.
//!
//! # Egress
//!
//! `deliver` posts to `POST /messages` and returns Webex's message id as
//! the platform-side identifier. Behaviour:
//!
//! - `OutboundMessage::content` with `{"text": "..."}` or
//!   `{"markdown": "..."}` becomes the JSON body.
//! - `{"card": {...}}` becomes an Adaptive Card attachment (
//!   `application/vnd.microsoft.card.adaptive`).
//! - Files are sent as multipart POSTs — one Webex API call per file (the
//!   platform allows only one file per call).
//! - `thread_id` is mapped to Webex's `parentId`.
//! - `MessageKind::System` rows describe edit / delete / reaction actions;
//!   see [`WebexAdapter::deliver`].
//!
//! Direct messages: [`WebexAdapter::open_dm`] returns a handle whose
//! `platform_id` is `person:<personId>`. `deliver` recognises that prefix
//! and routes via `toPersonId` instead of `roomId`.
//!
//! # Ingress
//!
//! The adapter binds an [`axum`] HTTP server at the configured host/port
//! and serves the Webex webhook at `path` (default `/webex/webhook`). Each
//! request's `X-Spark-Signature` header is verified against an HMAC of the
//! raw request body (SHA-1 by default, SHA-256 if configured). Webex's
//! payloads omit message text for security, so the router fetches the full
//! message via `GET /messages/{id}` before emitting an inbound event. For
//! `attachmentActions.created` payloads it fetches
//! `GET /attachment/actions/{id}` instead.
//!
//! Duplicate `id`s are suppressed via an in-memory ring of the most recent
//! 256 webhook ids.
//!
//! # Errors
//!
//! Status code mapping is documented on [`api::WebexApi`]: 401/403 →
//! [`AdapterError::Auth`], 404 → [`AdapterError::BadRequest`], 429 →
//! [`AdapterError::Rate`] (honouring `Retry-After`), other 4xx →
//! [`AdapterError::BadRequest`], 5xx and transport failures →
//! [`AdapterError::Transport`]. The Beta reactions endpoint additionally
//! maps 404/501 to [`AdapterError::Unsupported`].
//!
//! [`AdapterError`]: copperclaw_channels_core::AdapterError
//! [`AdapterError::Auth`]: copperclaw_channels_core::AdapterError::Auth
//! [`AdapterError::Rate`]: copperclaw_channels_core::AdapterError::Rate
//! [`AdapterError::BadRequest`]: copperclaw_channels_core::AdapterError::BadRequest
//! [`AdapterError::Transport`]: copperclaw_channels_core::AdapterError::Transport
//! [`AdapterError::Unsupported`]: copperclaw_channels_core::AdapterError::Unsupported

pub mod adapter;
pub mod api;
pub mod config;
pub mod events;
pub mod factory;
pub mod signature;

pub use adapter::{PERSON_PREFIX, WebexAdapter};
pub use api::{
    AttachmentAction, MessageView, PersonMe, PostMessageResponse, WebexApi,
    build_adaptive_breadcrumb, build_adaptive_card, build_adaptive_collapsible,
    build_adaptive_diff, build_adaptive_error, build_adaptive_thinking, build_adaptive_todo_list,
};
pub use config::{WebexConfig, WebhookConfig};
pub use events::{EventDedup, WebexEventsState, WebexWebhookEnvelope, build_events_router};
pub use factory::{CHANNEL_TYPE_STR, WebexFactory, register};
pub use signature::{SignatureAlgo, SignatureError, compute_signature, verify_signature};
