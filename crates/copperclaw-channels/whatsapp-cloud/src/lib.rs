//! `WhatsApp` Cloud channel adapter — REST egress + signed webhook ingress.
//!
//! Implements the `WhatsApp` Cloud channel for copperclaw, see `PLAN.md` § 6.
//!
//! # Token model
//!
//! Egress uses a long-lived **system-user access token** carried in the
//! `Authorization: Bearer` header on every call to
//! `https://graph.facebook.com/v18.0/{phone_number_id}/...`. The
//! per-channel `access_token` is loaded from the host-provided config.
//!
//! Webhook signatures are verified using the **app secret** (configured
//! separately) via HMAC-SHA256 over the raw POST body, supplied by Meta in
//! the `X-Hub-Signature-256: sha256=<hex>` header. See [`signature`].
//!
//! # Verification flow
//!
//! Meta verifies a webhook endpoint by issuing a one-time `GET` with
//! `hub.mode=subscribe&hub.verify_token=<token>&hub.challenge=<nonce>`.
//! When `hub.verify_token` matches the configured token we respond `200
//! OK` with `<nonce>` as the literal body; otherwise we return `403`.
//!
//! # Ingress
//!
//! `POST` notifications are signature-verified, JSON-decoded, and each
//! `messages[]` entry is mapped into an [`copperclaw_types::InboundEvent`].
//! Recognised message `type` values: `text`, `image`, `document`, `audio`,
//! `video`, `button`, `interactive`. `reaction` payloads and `statuses[]`
//! entries are acknowledged but produce no inbound event in v1.
//!
//! Inbound `platform_id` is encoded as `"<phone_number_id>:<wa_id>"` so
//! the egress side can route replies back to the correct sender on the
//! correct business number.
//!
//! Duplicate `messages[].id` entries are suppressed via an LRU-ish ring of
//! 256 entries (see [`events::router::EventDedup`]).
//!
//! # Egress
//!
//! [`adapter::WhatsappCloudAdapter::deliver`] parses the platform id back
//! into `(phone_number_id, recipient_e164)`, then issues one of:
//!
//! - Text → `POST /{pnid}/messages` with `type:"text"`. When `thread_id`
//!   is `Some(wamid)` the call adds `context.message_id` for a flat reply.
//! - Files → upload each via `POST /{pnid}/media` (multipart) and send as
//!   `type:"document"` with the returned media id.
//! - System action `edit` → [`copperclaw_channels_core::AdapterError::Unsupported`]
//!   (`WhatsApp` Cloud has no edit endpoint).
//! - System action `reaction` → `type:"reaction"` against the configured
//!   target message id. Empty `emoji` clears an existing reaction.
//!
//! `set_typing` is mapped to `mark_read` of the user's last message id —
//! the closest reasonable approximation of typing on this platform. It is
//! a no-op unless the caller passes the last-message id in `thread_id`
//! (otherwise we would issue a read on every typing tick).
//!
//! # Errors
//!
//! Meta error envelope:
//!
//! ```json
//! {"error": {"message": "...", "type": "...", "code": <int>, "fbtrace_id": "..."}}
//! ```
//!
//! Mapped as follows:
//!
//! | HTTP | Body `code` | Result |
//! |---|---|---|
//! | 401 / 403 | — | [`AdapterError::Auth`] |
//! | any | 190 / 200..=299 | [`AdapterError::Auth`] |
//! | 429 | — | [`AdapterError::Rate`] (with `Retry-After`) |
//! | any | 4 / 80004 / 130429 | [`AdapterError::Rate`] |
//! | 400 / 404 | — | [`AdapterError::BadRequest`] |
//! | 5xx | — | [`AdapterError::Transport`] |
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

pub use adapter::WhatsappCloudAdapter;
pub use api::WhatsappCloudApi;
pub use config::{WebhookConfig, WhatsappCloudConfig};
pub use events::router::{EventDedup, WhatsappCloudEventsState, build_events_router};
pub use factory::{CHANNEL_TYPE_STR, WhatsappCloudFactory, register};
pub use signature::{SignatureError, compute_signature, verify_signature};
