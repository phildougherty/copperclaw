//! Channel adapter trait + registry.
//!
//! See `PLAN.md` § 5.1.
//!
//! Public API:
//! - [`ChannelAdapter`] / [`ChannelFactory`] traits.
//! - [`ChannelRegistry`] in-process factory lookup.
//! - [`ChannelSetup`] — the per-instance init context (config, mpsc sender,
//!   per-channel data dir) every factory receives.
//! - [`ContainerContribution`] (+ local [`Mount`]) — what a channel adds to
//!   the agent container environment.
//! - [`DmHandle`] — result of [`ChannelAdapter::open_dm`].
//! - [`AdapterError`] — single error type for all adapter and factory calls.
//! - [`Card`] (+ [`CardField`], [`CardButton`], [`CardError`]) — portable
//!   card schema rendered natively by adapters with rich card support and
//!   degraded to plain text everywhere else.
//! - [`testing`] — reusable [`testing::MockAdapter`] / [`testing::MockFactory`]
//!   for downstream tests.

mod adapter;
mod card;
mod container;
mod dm;
mod error;
mod registry;
mod setup;

pub mod testing;

pub use adapter::{ChannelAdapter, ChannelFactory};
pub use card::{
    Card, CardButton, CardError, CardField, MAX_BODY_CHARS, MAX_BUTTONS, MAX_BUTTON_LABEL_CHARS,
    MAX_BUTTON_VALUE_BYTES, MAX_FIELDS, MAX_FIELD_LABEL_CHARS, MAX_FIELD_VALUE_CHARS,
    MAX_TITLE_CHARS,
};
pub use container::{ContainerContribution, Mount};
pub use dm::DmHandle;
pub use error::AdapterError;
pub use registry::ChannelRegistry;
pub use setup::ChannelSetup;
