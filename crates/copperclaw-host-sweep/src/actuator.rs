//! Stuck-tool actuator seam — M21 S2, architecture decision (a).
//!
//! The sweep detects stuck tools from per-session DB state
//! ([`crate::checks::stuck`]) but never touches a container runtime —
//! container lifecycle has a single writer, the host's container
//! manager. Before this trait, that split meant a hung tool was
//! *detected but never recovered*: the runner keeps its heartbeat fresh
//! during tool dispatch (the runner process IS alive), so the manager's
//! own crash classification never fires, and the sweep only logged the
//! detection. The session wedged until an operator intervened.
//!
//! [`StuckActuator`] closes the loop without breaking the ownership
//! split: the host injects an implementation (its `ContainerManager`)
//! into [`crate::SweepService`] at boot, and the sweep calls it for
//! every session whose active tool has run past
//! [`crate::ABSOLUTE_CEILING_MS`]. Detections past the 60s claim
//! threshold ([`crate::CLAIM_STUCK_MS`] / a tool's declared timeout)
//! stay observe-only — long-but-legitimate tools must not be killed.
//!
//! Rejected alternatives (per the M21 plan — don't re-litigate): a
//! separate tool-progress file (duplicates the per-session DB state the
//! sweep already reads) and sweep-side direct docker kill (violates the
//! manager's single-writer ownership of container lifecycle).

use copperclaw_types::SessionId;

/// Opaque error surface for actuator failures. The sweep only logs
/// these — a failed restart is retried naturally on the next sweep pass
/// because the stuck detection re-fires until the tool state clears.
pub type ActuatorError = Box<dyn std::error::Error + Send + Sync>;

/// Restart authority for sessions whose active tool ran past the
/// absolute ceiling. Implemented by the host's `ContainerManager`;
/// injected into [`crate::SweepService`] via
/// [`crate::SweepService::set_stuck_actuator`].
#[async_trait::async_trait]
pub trait StuckActuator: Send + Sync {
    /// Restart `session_id`'s container because its active tool ran
    /// past [`crate::ABSOLUTE_CEILING_MS`].
    ///
    /// Contract for implementations:
    ///
    /// - Re-verify liveness before acting: the detection is a sweep-pass
    ///   snapshot, and the session may have stopped / restarted / been
    ///   deleted in between. A stale detection must be a quiet no-op
    ///   `Ok(())`, not an error.
    /// - Be idempotent: the sweep may re-request the same session on
    ///   consecutive passes if tool state has not cleared yet.
    /// - Surface the recovery to the user (the host rides its existing
    ///   crash-restart apology machinery) — the whole point of the
    ///   actuator is that nothing dies silently.
    async fn restart_stuck(&self, session_id: SessionId) -> Result<(), ActuatorError>;
}
