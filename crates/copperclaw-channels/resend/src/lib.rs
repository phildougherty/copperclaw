//! Resend (`resend.com`) email channel adapter — send-only.
//!
//! Implements the Resend channel for copperclaw. The crate exposes:
//!
//! - [`ResendFactory`] — registers with [`copperclaw_channels_core::ChannelRegistry`].
//! - [`register`] — the standard `(reg: &mut ChannelRegistry)` entry point.
//! - [`CHANNEL_TYPE_STR`] — the channel-type tag (`"resend"`).
//!
//! # Egress
//!
//! `deliver` issues a single `POST /emails` to the Resend API. The
//! `platform_id` is the recipient address, or a comma-separated list of
//! addresses (each value is trimmed; empty pieces are rejected). The
//! configured `from` address is used as the `From:` header. An outbound
//! payload that carries `subject` overrides the configured default;
//! payloads with `text` and/or `html` map straight onto the equivalent
//! Resend body fields. Attachments are base64-encoded inline; filenames
//! must be a single safe path component (no slashes, `..`, leading dots,
//! NUL, control characters, or names longer than 255 bytes).
//!
//! `set_typing`, `subscribe`, and `open_dm` use the trait defaults — Resend
//! has no equivalent surfaces.
//!
//! `System` outbound messages whose `content` carries an `action` key
//! (`edit`, `reaction`) return [`AdapterError::Unsupported`]; email cannot
//! retroactively edit or react.
//!
//! When `thread_id` is supplied the adapter adds `In-Reply-To` and
//! `References` headers via Resend's optional `headers` field. Resend has
//! no first-class thread concept, so [`ChannelAdapter::supports_threads`]
//! still reports `false`.
//!
//! # Ingress
//!
//! Resend exposes delivery / bounce / opened webhooks but does **not**
//! deliver user replies (it is an outbound-only transactional sending
//! product). This adapter implements no ingress: the factory's `init`
//! does not bind any HTTP server. Bounce signals could be added in a
//! follow-up if we ever want them.
//!
//! # Errors
//!
//! Per `docs/adding-a-channel.md` § 5 the API client maps:
//!
//! - HTTP 401 / 403 → [`AdapterError::Auth`].
//! - HTTP 400 / 404 / 422 → [`AdapterError::BadRequest`].
//! - HTTP 429 with `Retry-After` → [`AdapterError::Rate`] (`retry_after`
//!   populated when the header parses as a `u64`).
//! - HTTP 5xx / connection failures → [`AdapterError::Transport`].
//! - `System` action messages (`edit`, `reaction`) → [`AdapterError::Unsupported`].
//!
//! [`AdapterError`]: copperclaw_channels_core::AdapterError
//! [`AdapterError::Auth`]: copperclaw_channels_core::AdapterError::Auth
//! [`AdapterError::BadRequest`]: copperclaw_channels_core::AdapterError::BadRequest
//! [`AdapterError::Rate`]: copperclaw_channels_core::AdapterError::Rate
//! [`AdapterError::Transport`]: copperclaw_channels_core::AdapterError::Transport
//! [`AdapterError::Unsupported`]: copperclaw_channels_core::AdapterError::Unsupported
//! [`ChannelAdapter::supports_threads`]: copperclaw_channels_core::ChannelAdapter::supports_threads

mod adapter;
mod api;
mod config;
mod factory;

pub use adapter::ResendAdapter;
pub use api::{Attachment, Header, ResendApi, SendEmailRequest, SendEmailResponse};
pub use config::{DEFAULT_API_BASE, DEFAULT_SUBJECT, ResendConfig};
pub use factory::{CHANNEL_TYPE_STR, ResendFactory, register};

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_channels_core::ChannelRegistry;
    use copperclaw_types::ChannelType;

    #[test]
    fn re_exports_present() {
        // Touch every public re-export so removing one is a compile-time error.
        assert_eq!(CHANNEL_TYPE_STR, "resend");
        assert_eq!(DEFAULT_API_BASE, "https://api.resend.com");
        assert_eq!(DEFAULT_SUBJECT, "(no subject)");
        let _: ResendFactory = <ResendFactory as Default>::default();
        let _ = ResendApi::new(DEFAULT_API_BASE, "k");
        let _: SendEmailRequest = SendEmailRequest::default();
        // Build types via paths to make sure they actually export.
        let _ = std::any::type_name::<ResendAdapter>();
        let _ = std::any::type_name::<ResendConfig>();
        let _ = std::any::type_name::<Attachment>();
        let _ = std::any::type_name::<Header>();
        let _ = std::any::type_name::<SendEmailResponse>();
    }

    #[test]
    fn register_is_a_callable_function() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new(CHANNEL_TYPE_STR)).is_some());
    }
}
