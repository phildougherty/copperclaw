//! Microsoft Teams channel adapter — Microsoft Graph REST egress +
//! change-notification webhook ingress.
//!
//! Implements the Teams channel for ironclaw, see `PLAN.md` § 6 (T6).
//!
//! The crate exposes:
//! - [`TeamsFactory`] — registers with [`ironclaw_channels_core::ChannelRegistry`].
//! - [`TeamsAdapter`] — implements [`ironclaw_channels_core::ChannelAdapter`].
//! - [`TeamsConfig`] — parsed configuration loaded from
//!   [`ironclaw_channels_core::ChannelSetup`] JSON.
//!
//! # Auth model
//!
//! The adapter does not perform the Microsoft identity-platform OAuth dance
//! itself. Callers supply an already-issued Microsoft Graph application
//! access token (or a delegated bearer token) via the `bot_token`
//! configuration field. Every outbound HTTP call sets
//! `Authorization: Bearer <bot_token>`. Token refresh, scope management and
//! tenant routing are operator concerns handled outside the channel.
//!
//! # Egress — Microsoft Graph
//!
//! Outbound messages are dispatched based on the shape of `platform_id`:
//!
//! - `team/{teamId}/channel/{channelId}` — posts to
//!   `POST /teams/{tid}/channels/{cid}/messages`. When `thread_id` is set,
//!   the call is rerouted to
//!   `POST /teams/{tid}/channels/{cid}/messages/{thread_id}/replies`.
//! - `chat/{chatId}` — posts to `POST /chats/{cid}/messages`.
//!
//! [`ironclaw_types::MessageKind::System`] messages with
//! `{"action":"edit","target_id":...,"text":...}` translate to a PATCH on
//! the relevant message body. `{"action":"reaction","target_id":...,
//! "emoji":...}` translates to `POST .../setReaction`, mapping the emoji
//! shortcode onto one of Teams' six supported reaction types
//! (`like`, `heart`, `laugh`, `surprised`, `sad`, `angry`). Anything else
//! returns [`ironclaw_channels_core::AdapterError::Unsupported`].
//!
//! Outbound attachments are not currently supported (Microsoft Graph
//! requires a separate OneDrive/SharePoint upload flow first). A
//! non-empty [`ironclaw_types::OutboundMessage::files`] returns
//! [`ironclaw_channels_core::AdapterError::Unsupported`].
//!
//! # Ingress — Change Notifications
//!
//! The webhook accepts two flows on the configured path:
//!
//! 1. **Validation handshake** — when a subscription is created, Microsoft
//!    Graph POSTs with `?validationToken=<token>` and an empty body. The
//!    handler responds `200 OK` with
//!    `Content-Type: text/plain; charset=utf-8` and the token as the body.
//!    These requests are *not* signed; the channel always accepts them.
//! 2. **Notifications** — Microsoft Graph POSTs JSON of the form
//!    `{"value": [{"subscriptionId":"...","clientState":"...","resource":
//!    "teams/T/channels/C/messages","resourceData":{"id":"..."}}]}`. Each
//!    entry's `clientState` is constant-time-compared against the
//!    configured `client_state_secret`; any mismatch on any entry rejects
//!    the whole batch with `401`. Surviving entries are deduplicated using
//!    a 256-entry LRU on `(subscriptionId, resourceData.id)`. For each new
//!    entry the adapter fetches the message via Microsoft Graph, strips
//!    HTML, and emits an [`ironclaw_types::InboundEvent`]. Optional HMAC
//!    signing of notifications (configured via `lifecycleNotificationUrl`)
//!    is not currently verified.
//!
//! `platform_id` is set per the resource:
//! - Channel resources → `"team/{teamId}/channel/{channelId}"`.
//! - Chat resources → `"chat/{chatId}"`.
//!
//! Messages whose `from.user.id` matches the configured `bot_user_id` are
//! filtered out to avoid loops. `is_mention` is computed from the message's
//! `mentions[]` array; `is_group` is `true` for channels and is determined
//! from `chatType` for chats.
//!
//! # Errors
//!
//! HTTP responses are mapped onto
//! [`ironclaw_channels_core::AdapterError`] variants:
//!
//! - `401`/`403` → [`AdapterError::Auth`].
//! - `429` → [`AdapterError::Rate`] (honoring `Retry-After`).
//! - `404` / `400` / `422` → [`AdapterError::BadRequest`].
//! - `5xx` and other unexpected statuses → [`AdapterError::Transport`].
//!
//! [`AdapterError`]: ironclaw_channels_core::AdapterError
//! [`AdapterError::Auth`]: ironclaw_channels_core::AdapterError::Auth
//! [`AdapterError::Rate`]: ironclaw_channels_core::AdapterError::Rate
//! [`AdapterError::BadRequest`]: ironclaw_channels_core::AdapterError::BadRequest
//! [`AdapterError::Transport`]: ironclaw_channels_core::AdapterError::Transport

mod adapter;
mod api;
mod config;
mod emoji;
mod events;
mod factory;
mod html;

pub use adapter::TeamsAdapter;
pub use api::{
    ChatInfoResponse, ChatMessageResponse, TeamsApi, build_adaptive_breadcrumb, build_adaptive_card,
    build_adaptive_collapsible, build_adaptive_diff, build_adaptive_error, build_adaptive_message_body,
    build_adaptive_thinking, build_adaptive_todo_list,
};
pub use config::{
    DEFAULT_GRAPH_BASE, DEFAULT_HOST, DEFAULT_PATH, DEFAULT_PORT, TeamsConfig, WebhookConfig,
};
pub use emoji::{TEAMS_REACTION_TYPES, shortcode_to_reaction_type};
pub use events::router::{
    DEDUP_CAPACITY, NotificationDedup, TeamsWebhookState, build_webhook_router,
};
pub use factory::{CHANNEL_TYPE_STR, TeamsFactory, register};
pub use html::html_to_text;
