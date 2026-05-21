//! Shared types for the ironclaw workspace.
//!
//! This crate is the contract surface between all other crates. It has zero
//! I/O dependencies (no tokio, no reqwest, no rusqlite) and must compile
//! fast. Other crates depend on it for cross-boundary data shapes.

pub mod approval;
pub mod channel;
pub mod id;
pub mod message;
pub mod provider;
pub mod routing;
pub mod schedule;
pub mod session;

pub use approval::*;
pub use channel::*;
pub use id::*;
pub use message::*;
pub use provider::*;
pub use routing::*;
pub use schedule::*;
pub use session::*;
