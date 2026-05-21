//! Database layer for ironclaw — central DB and per-session DBs.
//!
//! See `PLAN.md` § 4 for the canonical schema.
//!
//! ## Crate layout
//!
//! - [`migrate`] — embedded SQL migration runner (central + per-session).
//! - [`central`] — pooled connection wrapper for `data/ironclaw.db`.
//! - [`session`] — per-session `inbound.db` / `outbound.db` openers and
//!   filesystem helpers.
//! - [`attachments`] — safety-checked attachment extraction.
//! - [`tables`] — per-resource CRUD modules.

pub mod attachments;
pub mod central;
pub mod migrate;
pub mod session;
pub mod tables;

mod error;

pub use error::DbError;
