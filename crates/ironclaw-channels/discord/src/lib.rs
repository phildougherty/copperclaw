//! Discord channel adapter — gateway + REST.
//!
//! See `PLAN.md` § 6 (T6c).
//!
//! This crate plugs Discord into the channel adapter trait from
//! `ironclaw-channels-core`. It speaks two protocols:
//!
//! - **Gateway** (WebSocket): the inbound side. We open a slim
//!   client over `tokio-tungstenite` (we deliberately do not pull in
//!   `twilight` — its dependency footprint is too heavy for the scope we
//!   need). We send `IDENTIFY` (or `RESUME` if a session is in hand), drive
//!   the `HEARTBEAT` loop with the prescribed jitter, and turn
//!   `MESSAGE_CREATE` dispatches into `InboundEvent`s.
//!
//! - **REST** (HTTPS): the outbound side. `reqwest` against
//!   `https://discord.com/api/v10`, with `Authorization: Bot <token>`.
//!   Errors map to `AdapterError` per the rate-limit and auth contracts.
//!
//! ## Module layout
//!
//! - [`config`] — parse the JSON config blob the host hands us.
//! - [`events`] — pure mapping from Discord dispatches to `InboundEvent`.
//! - [`rest`] — REST HTTP client (`DiscordRest`).
//! - [`gateway`] — WebSocket protocol: [`gateway::codec`] for frames,
//!   [`gateway::lifecycle`] for heartbeat/resume/backoff math.
//! - [`adapter`] — `DiscordAdapter` itself.
//! - [`factory`] — `DiscordFactory` + `register`.
//!
//! ## Thread mapping
//!
//! Discord models threads as **separate channels** with their own
//! `channel_id`. The adapter exposes `supports_threads() == false` and maps
//! `d.channel_id` to `InboundEvent::platform_id`. When a message carries a
//! `message_reference.message_id`, that id is surfaced as `thread_id` so the
//! router can keep replies grouped — but replies still address the channel
//! id directly. See [`events::message_create_to_inbound`] for details.

pub mod adapter;
pub mod config;
pub mod events;
pub mod factory;
pub mod gateway;
pub mod rest;

pub use adapter::DiscordAdapter;
pub use config::{
    DEFAULT_API_BASE, DEFAULT_GATEWAY_URL, DEFAULT_INTENTS, DiscordConfig,
};
pub use events::{CHANNEL_TYPE_STR, message_create_to_inbound};
pub use factory::{DiscordFactory, register};
pub use rest::DiscordRest;

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_channels_core::ChannelRegistry;
    use ironclaw_types::ChannelType;

    #[test]
    fn channel_type_str_is_discord() {
        assert_eq!(CHANNEL_TYPE_STR, "discord");
    }

    #[test]
    fn default_intents_constant_matches_spec() {
        // Bitwise sum of `(1<<0)|(1<<9)|(1<<10)|(1<<12)|(1<<15)`.
        assert_eq!(DEFAULT_INTENTS, 38_401);
    }

    #[test]
    fn default_endpoints_are_v10() {
        assert!(DEFAULT_API_BASE.ends_with("/v10"));
        assert!(DEFAULT_GATEWAY_URL.contains("v=10"));
    }

    #[test]
    fn register_smoke() {
        let mut reg = ChannelRegistry::new();
        register(&mut reg).unwrap();
        assert!(reg.get(&ChannelType::new("discord")).is_some());
    }
}
