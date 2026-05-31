//! Subagent-depth constants for the `create_agent` action's nesting gate.
//!
//! Depth tracking itself lives on [`CreateAgentHandler`](super::create_agent::CreateAgentHandler)
//! (see [`create_agent`](super::create_agent)) because the look-up needs
//! access to the in-memory `spawned` cache and the central DB. This file
//! holds only the pure constants so other modules in the crate can
//! depend on them without dragging in the handler.

/// Default cap on `create_agent` nesting depth. A parent at depth N can
/// spawn a child at depth N+1; the spawn is rejected once N+1 exceeds
/// this cap. 3 lets a top-level agent delegate to a sibling that
/// delegates to a focused sub-sibling — useful for layered
/// investigations — without permitting an unbounded fork-bomb.
pub const DEFAULT_MAX_SUBAGENT_DEPTH: u8 = 3;

/// Hard ceiling on operator-configured subagent depth caps. Deeper
/// chains than this are misconfiguration: they invite the saturation
/// collapse `checked_add` guards against, and they have no real-world
/// use case beyond fork-bombs.
pub const MAX_SUBAGENT_DEPTH_CEILING: u8 = 16;
