//! Operator-alert sink seam — M21 O4, architecture decision (d).
//!
//! `copperclaw-host-sweep` cannot name `copperclaw-host` (the dependency
//! points the other way), so the operator-alert push follows the exact
//! pattern the S2 [`crate::actuator::StuckActuator`] seam established: the
//! host injects an [`OperatorAlertSink`] implementation (its
//! `Arc<OperatorAlerts>`) into [`crate::SweepService`] at boot via
//! [`crate::SweepService::set_operator_alerts`], and two sweep-side sites
//! consult it:
//!
//!   1. **O2 quarantine** — a session whose per-session DB was quarantined
//!      for corruption fires exactly one critical alert (deduped host-side
//!      on `quarantine:<session>`), so an operator hears about a silently
//!      excluded session instead of only seeing it in `cclaw doctor`.
//!   2. **Apology copy** — the S2 honest text only says "tell your
//!      operator" because nothing pushed. When a destination is wired
//!      ([`OperatorAlertSink::is_enabled`] is true) the truthful "the
//!      operator has been notified" line is restored.
//!
//! Unset — the default, and every test that does not wire a host — every
//! call is a no-op and [`OperatorAlertSink::is_enabled`] is `false`, so the
//! sweep's pre-O4 behaviour is byte-identical.

/// Opt-in operator-alert push seam. Implemented in `copperclaw-host` by
/// its `OperatorAlerts` (so `Arc<OperatorAlerts>` coerces to
/// `Arc<dyn OperatorAlertSink>`). Kept deliberately tiny — a `&str`
/// severity token the host maps to its own `AlertSeverity`, a stable
/// `dedup_key` naming the episode, and the user/operator-facing message.
pub trait OperatorAlertSink: Send + Sync {
    /// Enqueue a rate-limited, deduped operator alert. `severity` is a
    /// lowercase token (`"critical"` | `"warning"` | …) mapped host-side;
    /// `dedup_key` names the *episode* (e.g. `quarantine:<session>`) so a
    /// condition that persists across sweep passes alerts at most once per
    /// the host's re-alert window. Fire-and-forget: fail-closed host-side,
    /// never surfaced back to the sweep.
    fn fire(&self, severity: &str, dedup_key: &str, message: &str);

    /// Whether an operator-alert destination is configured. Gates the
    /// apology copy's truthful "the operator has been notified" line — with
    /// no destination the sweep keeps the S2 honest copy.
    fn is_enabled(&self) -> bool;
}
