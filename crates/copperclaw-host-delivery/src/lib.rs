//! Outbound delivery loops (active 1s + sweep 60s) — polls session
//! `outbound.db`s and dispatches to channel adapters.
//!
//! See `PLAN.md` § 6 (T3). The crate provides:
//!
//! - [`DeliveryService`] — the orchestrator. Construct one per host with the
//!   central DB, a [`SessionRoot`], the registered channel adapters, and a
//!   [`DeliveryDispatcher`](copperclaw_modules::DeliveryDispatcher) handle (or
//!   use [`DeliveryService::with_default_dispatcher`] to build one).
//! - [`DeliveryService::run_active_loop`] — polls every
//!   [`ACTIVE_POLL_MS`] ms; sessions whose `container_status == Running` are
//!   scanned.
//! - [`DeliveryService::run_sweep_loop`] — polls every [`SWEEP_POLL_MS`] ms;
//!   every active session is scanned regardless of container state.
//! - [`HostDispatcher`] — `Arc<dyn DeliveryDispatcher>` implementation that
//!   modules use to fire typing indicators and synthetic outbound messages.
//!
//! ## Retry / backoff schedule
//!
//! For an adapter that returns a retryable error
//! (`AdapterError::Rate | Transport | Io`) the service tracks the number of
//! attempts in memory keyed by `(session_id, message_out_id)` and defers the
//! row with an exponential delay:
//!
//! | attempt | delay         |
//! |--------:|--------------:|
//! | 1 -> 2  | `5_000` ms      |
//! | 2 -> 3  | `10_000` ms     |
//! | 3       | mark `failed` |
//!
//! Delays are capped at [`ABSOLUTE_CEILING_MS`] (30 minutes). Non-retryable
//! adapter errors mark the row as `failed` immediately. Rows that have no
//! registered adapter are left in place (the row is not consumed) so a
//! subsequent host reboot with the adapter registered can deliver them.

pub mod dispatch;
pub mod error;
pub mod loops;
pub mod service;
pub mod system_actions;

#[cfg(test)]
mod test_support;

pub use dispatch::{AdapterResolver, HostDispatcher};
pub use error::DeliveryError;
pub use service::{
    DeliveryKey, DeliveryReport, DeliveryService, FsSessionRoot, SessionPool, SessionRoot,
    ABSOLUTE_CEILING_MS, ACTIVE_POLL_MS, BACKOFF_BASE_MS, MAX_DELIVERY_ATTEMPTS, SWEEP_POLL_MS,
};
pub use system_actions::{parse_system_content, ParsedAction};
