//! Individual sweep-pass checks. Each module exposes a `check` function that
//! takes the session context and current instant and returns its branch of
//! the [`crate::SweepReport`].

pub mod heartbeat;
pub mod processing;
pub mod recurrence;
pub mod stuck;
pub mod wake;
