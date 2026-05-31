//! Per-turn message formatting seam.
//!
//! The actual message-row → provider-input formatting lives in
//! `crate::formatter` (top-level module, public API: [`format_messages`]
//! and [`FormattedTurn`]). This file re-exports those names at
//! `crate::run::formatting::*` so the run-loop's call sites can refer
//! to the formatting concern via the same `run::` directory layout as
//! the other turn-loop concerns (`provider_call`, `tool_dispatch`,
//! `drive_turn`).
//!
//! Keeping the canonical definitions in `crate::formatter` preserves
//! the existing public path `copperclaw_runner::formatter::format_messages`
//! and `copperclaw_runner::format_messages` (re-exported from `lib.rs`)
//! that downstream crates already depend on.

#[allow(unused_imports)]
pub(super) use crate::formatter::{format_messages, FormattedTurn};
