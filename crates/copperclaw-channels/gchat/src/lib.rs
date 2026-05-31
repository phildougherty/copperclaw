//! Google Chat channel adapter — REST egress + HTTP push event ingress.
//!
//! Implements the Google Chat channel for copperclaw, see `PLAN.md` § 6 (T6).
//!
//! The crate exposes:
//! - [`GchatFactory`] — registers with [`copperclaw_channels_core::ChannelRegistry`].
//! - [`register`] — convenience function used by the host's
//!   `channels_init::build_registry`.
//! - [`CHANNEL_TYPE_STR`] — the literal `"gchat"` registered with the
//!   registry.
//!
//! # Ingress
//!
//! Google Chat apps receive events at a configured HTTP push URL. Google
//! signs each event with a JWT in the `Authorization: Bearer <jwt>` header.
//!
//! **v1 simplification**: this adapter does **not** verify the JWT signature
//! (parsing JWKS / verifying Google's public keys is out of scope). Instead
//! the operator configures a long random `client_token` query string and the
//! webhook URL is built as `https://.../gchat/webhook?token=<client_token>`.
//! The adapter rejects any request whose `token` query parameter does not
//! match. A follow-up will implement JWT verification.
//!
//! Supported event types:
//!
//! - `MESSAGE` — produces an [`copperclaw_types::InboundEvent`] of
//!   `MessageKind::Chat` with the text body. Skipped when `user.type == "BOT"`
//!   to avoid loops.
//! - `CARD_CLICKED` — produces an inbound event with the action + parameters
//!   in `content`.
//! - `ADDED_TO_SPACE`, `REMOVED_FROM_SPACE`, etc. — acknowledged with `200 OK`
//!   but no inbound event is emitted.
//!
//! Duplicate `message.name`s are suppressed via an in-memory LRU of the last
//! 256 identifiers.
//!
//! # Egress
//!
//! `deliver` posts to `POST /v1/spaces/{space}/messages` and returns the
//! Google-Chat `name` (full resource path) as the platform-side message id.
//! Threaded replies use the `messageReplyOption=REPLY_MESSAGE_OR_FAIL` query
//! parameter plus `thread.name` in the body. Card payloads are routed to the
//! `cardsV2` field instead of `text`. The `edit` system action issues
//! `PUT /v1/.../messages/{id}?updateMask=text`; `reaction` issues
//! `POST /v1/.../messages/{id}/reactions` with the configured unicode
//! codepoint.
//!
//! **Token rotation**: this adapter receives a service-account-derived bearer
//! token via configuration; rotating that token is the operator's
//! responsibility.
//!
//! # Attachments
//!
//! Google Chat attachments require the Drive upload flow which is out of
//! scope for v1. `OutboundMessage::files` containing any entries cause
//! `deliver` to return [`AdapterError::Unsupported`].
//!
//! [`AdapterError::Unsupported`]: copperclaw_channels_core::AdapterError::Unsupported

mod adapter;
mod api;
mod config;
mod emoji;
mod events;
mod factory;

pub use adapter::GchatAdapter;
pub use api::GchatApi;
pub use config::{GchatConfig, WebhookConfig};
pub use emoji::{emoji_codepoint, EMOJI_TABLE};
pub use events::router::{build_events_router, EventDedup, GchatEventsState};
pub use events::types::{GchatEvent, GchatEventEnvelope, GchatMessage, GchatSpace, GchatUser};
pub use factory::{register, GchatFactory, CHANNEL_TYPE_STR};
