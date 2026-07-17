//! Opt-in operator-alert destination — M21 O4, architecture decision (d).
//!
//! Before this module the host's critical events (a supervised background
//! loop exceeding its restart budget, a session crash-looping on OOM, a
//! quarantined per-session DB, a spawn-failure streak) were **pull-only**:
//! visible exactly when an operator happened to run `cclaw doctor` or watch
//! metrics. Decision (d) adds a **push** path without any new notification
//! infrastructure — it reuses the EXISTING outbound delivery pipeline
//! (`messages_out` → the delivery loops → the channel adapters):
//!
//! - An operator configures ONE destination (a channel + platform target)
//!   via the host `.env`. When configured, the host enqueues rate-limited,
//!   deduped **system alert rows** into an active session's `outbound.db`
//!   with that destination's `channel_type` / `platform_id` / `thread_id`.
//!   The delivery loop routes on the row's OWN routing fields (see
//!   `copperclaw_host_delivery::DeliveryService::resolve_target`), so the
//!   alert reaches the operator's channel regardless of which session's
//!   `outbound.db` physically carried it.
//! - **Disabled by default.** With no destination configured, [`fire`] does
//!   nothing but the existing log line + the M1 metric wish — ZERO new
//!   outbound, zero behavior change. This is the secure-by-default posture:
//!   the platform never emits a new outward message unless an operator with
//!   host filesystem access explicitly opted in.
//!
//! ## Rate limiting + dedup (one alert per episode, not per sweep pass)
//!
//! Several call sites (notably the O2 quarantine check) run once per 60-second
//! sweep pass. Firing an alert every pass for a persisting condition would be
//! an alert flood. [`OperatorAlerts::fire`] therefore takes a stable
//! `dedup_key` naming the *episode* (e.g. `oom:<session>`, `supervisor.degraded`,
//! `quarantine:<session>`): a repeat within [`RE_ALERT_WINDOW`] is suppressed,
//! so a still-broken condition re-alerts at most once per window rather than
//! once per pass. A separate global token cap ([`RATE_LIMIT_MAX`] alerts per
//! [`RATE_LIMIT_WINDOW`]) bounds the total even when many DISTINCT keys fire at
//! once (e.g. a fleet-wide OOM), so a burst can never amplify into an unbounded
//! outbound storm.
//!
//! ## Fail-closed
//!
//! A misconfigured or transiently-unavailable destination NEVER crashes the
//! host: a missing carrier session, a DB error, or a serialisation failure is
//! logged at WARN and dropped (the M1 `enqueue_failed` / `no_carrier` wishes
//! count it). The host keeps running; the pull-side signals (`cclaw doctor`,
//! metrics, host log) remain the source of truth.
//!
//! ## Security-review argument (new config surface + new outbound path)
//!
//! - **Opt-in / default-off:** no destination ⇒ no new outbound at all.
//! - **Who configures it:** only the host operator, via the install `.env`
//!   (`COPPERCLAW_OPERATOR_ALERT_*`). An agent or a chat user cannot set it —
//!   editing `.env` already requires host access, so this is not a privilege
//!   escalation.
//! - **No flood amplification:** dedup + the global token cap above prevent a
//!   crash-loop / fleet-wide fault from being amplified into an outbound storm
//!   against the operator's channel.
//! - **No secret / PII leakage:** alert bodies are host-authored control-plane
//!   copy naming a session UUID + a component; they carry no provider keys, no
//!   user message content, and nothing the operator cannot already see in
//!   `cclaw doctor` / the host log.
//! - **Fail-closed:** a bad destination logs and drops; it never wedges or
//!   crashes the host.
//!
//! ## Coordinator wiring for the two out-of-lane call sites (M21 O4)
//!
//! Two call sites live outside lane H and are wired by the coordinator at
//! integration; both are trivial one-liners against this module:
//!
//! 1. **O2 quarantine (lane W, `copperclaw-host-sweep`).** `copperclaw-host-sweep`
//!    cannot name `copperclaw-host` (dependency points the other way), so mirror
//!    the S2 `StuckActuator` seam: define a minimal `OperatorAlertSink` trait in
//!    `copperclaw-host-sweep` with `fn fire(&self, severity: &str, dedup_key: &str,
//!    message: &str)` and `fn is_enabled(&self) -> bool`, inject an
//!    `Option<Arc<dyn OperatorAlertSink>>` into `SweepService` (a
//!    `set_operator_alerts` setter, like `set_stuck_actuator`), and impl the
//!    trait for `Arc<OperatorAlerts>` in `copperclaw-host` (forwarding to
//!    [`OperatorAlerts::fire`] after mapping the `&str` severity, and to
//!    [`OperatorAlerts::is_enabled`]). At the quarantine site the call is:
//!
//!    ```ignore
//!    sink.fire("critical", &format!("quarantine:{session_id}"),
//!        "A per-session database was quarantined for corruption; that \
//!         session is excluded from sweeps until an operator intervenes. \
//!         See `cclaw doctor`.");
//!    ```
//!
//! 2. **Conditional apology copy (lane W, `checks/apology.rs`).** The S2 honest
//!    copy only says "tell your operator" because nothing pushed. Once a
//!    destination is wired, restore the truthful "the operator has been
//!    notified" line CONDITIONALLY on [`OperatorAlerts::is_enabled`] (routed
//!    through the same `OperatorAlertSink::is_enabled` seam):
//!
//!    ```ignore
//!    let tail = if sink.map_or(false, |s| s.is_enabled()) {
//!        " — the operator has been notified."
//!    } else {
//!        " If this keeps happening, tell your operator — `cclaw doctor` \
//!         will show what's wrong."
//!    };
//!    ```
//!
//! ## M1 metric wishes (recorded here + in the PR)
//!
//! Per the O4 plan wish ("operator alerts by severity"):
//! `// M1 metric wish:` `copperclaw_operator_alerts_total{severity, outcome}`
//! where `outcome ∈ {sent, suppressed_disabled, suppressed_deduped,
//! suppressed_rate_limited, no_carrier, enqueue_failed}` — one counter that
//! captures both the delivered alerts (by severity) and every suppression
//! reason an operator would want to alert on.

use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{SessionPaths, open_outbound};
use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_outbound};
use copperclaw_db::tables::sessions;
use copperclaw_types::{ChannelType, MessageId, MessageKind};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// A persisting condition re-alerts at most once per this window: a repeat
/// [`OperatorAlerts::fire`] with the same `dedup_key` inside the window is
/// suppressed. This is what turns "one alert per sweep pass" (the naive
/// per-pass call) into "one alert per episode".
pub const RE_ALERT_WINDOW: Duration = Duration::from_secs(900);

/// Rolling window for the global rate-limit token cap.
pub const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(300);

/// Maximum alerts enqueued within [`RATE_LIMIT_WINDOW`] across ALL dedup
/// keys. Bounds a fleet-wide fault (many distinct keys firing at once) so a
/// burst cannot amplify into an unbounded outbound storm.
pub const RATE_LIMIT_MAX: usize = 20;

/// Env var naming the destination channel type (e.g. `telegram`, `slack`,
/// `cli`). Empty / unset disables operator alerts entirely.
pub const ENV_CHANNEL: &str = "COPPERCLAW_OPERATOR_ALERT_CHANNEL";

/// Env var naming the destination platform id (the channel-specific target:
/// a chat id, user id, room id, …). Empty / unset disables operator alerts.
pub const ENV_TARGET: &str = "COPPERCLAW_OPERATOR_ALERT_TARGET";

/// Optional env var naming the destination thread id (channel-specific
/// sub-thread). Absent ⇒ the channel's default thread.
pub const ENV_THREAD: &str = "COPPERCLAW_OPERATOR_ALERT_THREAD";

/// Severity of an operator alert. Drives the metric label and a short prefix
/// in the alert body; deliberately coarse — an operator wants "is this urgent"
/// not a log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSeverity {
    /// A recoverable-but-notable condition (a spawn-failure streak, a session
    /// crash-looping) an operator should look at soon.
    Warning,
    /// A host-level fault that will not self-heal without intervention (a
    /// background loop exceeding its restart budget, a quarantined DB).
    Critical,
}

impl AlertSeverity {
    /// Stable lowercase token for logs + the M1 metrics rider.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }

    /// Map the loose `&str` severity the out-of-lane `OperatorAlertSink` seam
    /// passes back to a variant. Anything unrecognised is treated as
    /// [`Self::Critical`] — an alert that reached `fire` at all is worth the
    /// louder tier rather than being silently downgraded.
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        match token {
            "warning" | "warn" => Self::Warning,
            _ => Self::Critical,
        }
    }
}

/// The configured push target. Cloned into each enqueued row's routing fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertDestination {
    /// Channel adapter to deliver through (must have a live adapter at
    /// delivery time, else the row defers exactly as any other outbound).
    pub channel_type: ChannelType,
    /// Channel-specific target id.
    pub platform_id: String,
    /// Optional channel-specific thread.
    pub thread_id: Option<String>,
}

impl AlertDestination {
    /// Parse the destination from the `COPPERCLAW_OPERATOR_ALERT_*` env vars.
    /// Returns `None` (⇒ operator alerts disabled) when the channel or the
    /// target is unset or empty — the opt-in gate.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let channel = std::env::var(ENV_CHANNEL)
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let target = std::env::var(ENV_TARGET)
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        let thread = std::env::var(ENV_THREAD)
            .ok()
            .filter(|s| !s.trim().is_empty());
        Some(Self {
            channel_type: ChannelType::new(channel.trim()),
            platform_id: target.trim().to_string(),
            thread_id: thread,
        })
    }
}

/// Dedup + rate-limit bookkeeping. Times are `tokio::time::Instant` so tests
/// drive the windows with a paused clock — no real waits.
#[derive(Default)]
struct AlertState {
    /// Last enqueue instant per dedup key (the episode-level dedup).
    last_fired: HashMap<String, Instant>,
    /// Enqueue instants inside the rolling rate-limit window.
    recent: VecDeque<Instant>,
}

/// Why a [`OperatorAlerts::fire`] call did not enqueue a row. Returned by the
/// pure decision step so tests can assert the exact suppression reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suppressed {
    /// A prior alert for this dedup key is still inside [`RE_ALERT_WINDOW`].
    Deduped,
    /// The global token cap for [`RATE_LIMIT_WINDOW`] is exhausted.
    RateLimited,
}

/// Opt-in operator-alert enqueuer. Cheap to share via `Arc`; wired into the
/// container manager (crash-loop/OOM + spawn-failure call sites) and the
/// supervisor degraded-watch loop in `boot.rs`.
pub struct OperatorAlerts {
    /// `None` ⇒ disabled (the default). No new outbound is ever produced.
    dest: Option<AlertDestination>,
    central: CentralDb,
    /// The live sessions root (`<data_dir>/sessions`) under which each
    /// carrier session's `outbound.db` lives.
    sessions_root: PathBuf,
    state: Mutex<AlertState>,
}

impl std::fmt::Debug for OperatorAlerts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorAlerts")
            .field("enabled", &self.is_enabled())
            .finish_non_exhaustive()
    }
}

impl OperatorAlerts {
    /// Build from the environment: reads the `COPPERCLAW_OPERATOR_ALERT_*`
    /// vars for the destination (absent ⇒ disabled). `sessions_root` is the
    /// live `<data_dir>/sessions` dir the delivery pipeline reads from.
    #[must_use]
    pub fn from_env(central: CentralDb, sessions_root: PathBuf) -> Self {
        Self::with_destination(central, sessions_root, AlertDestination::from_env())
    }

    /// Build with an explicit destination (`None` ⇒ disabled). Used by tests
    /// and by any future non-env configuration source.
    #[must_use]
    pub fn with_destination(
        central: CentralDb,
        sessions_root: PathBuf,
        dest: Option<AlertDestination>,
    ) -> Self {
        Self {
            dest,
            central,
            sessions_root,
            state: Mutex::new(AlertState::default()),
        }
    }

    /// Whether a destination is configured. The apology-copy restore (the
    /// out-of-lane `checks/apology.rs` touch) reads this to decide between the
    /// honest "tell your operator" copy and the now-true "the operator has
    /// been notified" copy.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.dest.is_some()
    }

    /// Enqueue one operator alert.
    ///
    /// `dedup_key` names the *episode* (not the pass): repeats within
    /// [`RE_ALERT_WINDOW`] are suppressed so a persisting condition re-alerts
    /// at most once per window. Disabled (no destination) ⇒ this is a log
    /// line + the M1 metric wish and nothing else: ZERO new outbound.
    ///
    /// Never panics and never propagates an error — a bad destination or a DB
    /// hiccup is logged and dropped (fail-closed).
    pub fn fire(&self, severity: AlertSeverity, dedup_key: &str, message: &str) {
        let Some(dest) = self.dest.as_ref() else {
            // Disabled: the pre-O4 behaviour — log + metric only. No outbound.
            // M1 metric wish: copperclaw_operator_alerts_total{severity, outcome="suppressed_disabled"}
            info!(
                severity = severity.as_str(),
                dedup_key,
                "operator alert raised but no destination configured; not sent (set COPPERCLAW_OPERATOR_ALERT_CHANNEL + _TARGET to enable)"
            );
            return;
        };

        match self.decide(dedup_key, Instant::now()) {
            Err(Suppressed::Deduped) => {
                // M1 metric wish: copperclaw_operator_alerts_total{severity, outcome="suppressed_deduped"}
                info!(
                    severity = severity.as_str(),
                    dedup_key, "operator alert deduped (already alerted this episode)"
                );
            }
            Err(Suppressed::RateLimited) => {
                // M1 metric wish: copperclaw_operator_alerts_total{severity, outcome="suppressed_rate_limited"}
                warn!(
                    severity = severity.as_str(),
                    dedup_key,
                    rate_limit_max = RATE_LIMIT_MAX,
                    "operator alert rate-limited; dropping to avoid an alert flood"
                );
            }
            Ok(()) => match self.enqueue(dest, severity, message) {
                Ok(()) => {
                    // M1 metric wish: copperclaw_operator_alerts_total{severity, outcome="sent"}
                    info!(
                        severity = severity.as_str(),
                        dedup_key,
                        channel = dest.channel_type.as_str(),
                        "operator alert enqueued for delivery"
                    );
                }
                Err(EnqueueError::NoCarrier) => {
                    // M1 metric wish: copperclaw_operator_alerts_total{severity, outcome="no_carrier"}
                    warn!(
                        severity = severity.as_str(),
                        dedup_key,
                        "operator alert not enqueued: no active session to carry the row (fail-closed)"
                    );
                }
                Err(EnqueueError::Db(err)) => {
                    // M1 metric wish: copperclaw_operator_alerts_total{severity, outcome="enqueue_failed"}
                    warn!(
                        severity = severity.as_str(),
                        dedup_key,
                        ?err,
                        "operator alert enqueue failed; dropping (fail-closed, host keeps running)"
                    );
                }
            },
        }
    }

    /// Decide whether an alert for `dedup_key` may be enqueued at `now`,
    /// updating the dedup + rate-limit bookkeeping on success. Pure w.r.t. the
    /// DB (locks only the in-memory state) so the dedup/rate-limit semantics
    /// are unit-testable in isolation.
    fn decide(&self, dedup_key: &str, now: Instant) -> Result<(), Suppressed> {
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Episode dedup FIRST, so a suppressed repeat does not consume a
        // rate-limit token.
        if let Some(last) = st.last_fired.get(dedup_key) {
            if now.duration_since(*last) < RE_ALERT_WINDOW {
                return Err(Suppressed::Deduped);
            }
        }

        // Global rate-limit token cap over the rolling window.
        while let Some(front) = st.recent.front() {
            if now.duration_since(*front) >= RATE_LIMIT_WINDOW {
                st.recent.pop_front();
            } else {
                break;
            }
        }
        if st.recent.len() >= RATE_LIMIT_MAX {
            return Err(Suppressed::RateLimited);
        }

        st.last_fired.insert(dedup_key.to_string(), now);
        st.recent.push_back(now);
        Ok(())
    }

    /// Write the alert row into an active session's `outbound.db`. The row's
    /// own routing (`channel_type` / `platform_id` / `thread_id`) is the
    /// destination, so the delivery loop routes it to the operator regardless
    /// of which session physically carried it. Chat-kind + `{"text": ...}` so
    /// every adapter renders it with no special support.
    fn enqueue(
        &self,
        dest: &AlertDestination,
        severity: AlertSeverity,
        message: &str,
    ) -> Result<(), EnqueueError> {
        let carrier = self.pick_carrier()?;
        carrier.ensure_dirs().ok();
        let conn = open_outbound(&carrier)?;
        let body = format!("[copperclaw {}] {message}", severity.as_str());
        let row = WriteOutbound {
            id: MessageId::new(),
            in_reply_to: None,
            timestamp: chrono::Utc::now(),
            deliver_after: None,
            recurrence: None,
            kind: MessageKind::Chat,
            channel_type: Some(dest.channel_type.clone()),
            platform_id: Some(dest.platform_id.clone()),
            thread_id: dest.thread_id.clone(),
            content: serde_json::json!({ "text": body }),
        };
        insert_outbound(&conn, &row)?;
        Ok(())
    }

    /// Pick the carrier session whose `outbound.db` hosts the alert row. Any
    /// `Active`-status session works (delivery polls all of them regardless of
    /// container status); `list_active` already orders by `last_active DESC`,
    /// so the first is the most-recently-active. `None` ⇒ no active session
    /// exists, so there is nowhere to enqueue (fail-closed).
    fn pick_carrier(&self) -> Result<SessionPaths, EnqueueError> {
        let sessions = sessions::list_active(&self.central)?;
        let carrier = sessions.into_iter().next().ok_or(EnqueueError::NoCarrier)?;
        Ok(SessionPaths::new(
            &self.sessions_root,
            carrier.agent_group_id,
            carrier.id,
        ))
    }

    /// Supervisor permanent-failure watch loop (M21 S1 seam). Awaits the
    /// supervisor's `degraded` watch channel and fires exactly one Critical
    /// alert each time the flag flips to `true`. The dedup key
    /// (`supervisor.degraded`) collapses any repeated flip within the window;
    /// the watch itself only wakes on change, so a stable degraded state does
    /// not re-alert.
    ///
    /// Runs until `shutdown` fires or the watch sender is dropped. Registered
    /// as a supervised loop in `boot.rs`, so a panic here restarts on the same
    /// backoff curve as every other loop; a fresh `degraded_watch()` receiver
    /// is minted per (re)start.
    pub async fn run_degraded_watch(
        self: std::sync::Arc<Self>,
        mut rx: tokio::sync::watch::Receiver<bool>,
        shutdown: CancellationToken,
    ) {
        // Fire on the current value first if the host booted already degraded,
        // then react to every subsequent flip to `true`.
        if *rx.borrow_and_update() {
            self.fire(
                AlertSeverity::Critical,
                "supervisor.degraded",
                "A host background loop exceeded its restart budget and the host is degraded. \
                 Run `cclaw doctor` (and `host.status` on the admin socket) to see which loop.",
            );
        }
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                res = rx.changed() => {
                    if res.is_err() {
                        // Sender dropped (supervisor gone) — nothing more to watch.
                        return;
                    }
                    if *rx.borrow_and_update() {
                        self.fire(
                            AlertSeverity::Critical,
                            "supervisor.degraded",
                            "A host background loop exceeded its restart budget and the host is degraded. \
                             Run `cclaw doctor` (and `host.status` on the admin socket) to see which loop.",
                        );
                    }
                }
            }
        }
    }
}

/// M21 O4 cross-lane seam: `copperclaw-host-sweep` fires operator alerts
/// (the O2 quarantine event) and reads the enabled flag (the conditional
/// apology copy) through its own [`copperclaw_host_sweep::OperatorAlertSink`]
/// trait — it cannot name this crate. The host wires its `OperatorAlerts`
/// in via `SweepService::set_operator_alerts` at boot; because the impl is
/// on `OperatorAlerts`, an `Arc<OperatorAlerts>` coerces straight to
/// `Arc<dyn OperatorAlertSink>`. The `&str` severity token is mapped to the
/// crate's own [`AlertSeverity`] (unknown ⇒ `Critical`, per `from_token`).
impl copperclaw_host_sweep::OperatorAlertSink for OperatorAlerts {
    fn fire(&self, severity: &str, dedup_key: &str, message: &str) {
        OperatorAlerts::fire(
            self,
            AlertSeverity::from_token(severity),
            dedup_key,
            message,
        );
    }

    fn is_enabled(&self) -> bool {
        OperatorAlerts::is_enabled(self)
    }
}

/// Why [`OperatorAlerts::enqueue`] could not write the row. Both variants are
/// fail-closed at the [`OperatorAlerts::fire`] boundary — logged, never
/// propagated.
#[derive(Debug)]
enum EnqueueError {
    /// No `Active`-status session exists to carry the row.
    NoCarrier,
    /// A per-session DB open / write failed.
    Db(copperclaw_db::DbError),
}

impl From<copperclaw_db::DbError> for EnqueueError {
    fn from(e: copperclaw_db::DbError) -> Self {
        Self::Db(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messages_out;
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use std::sync::Arc;

    fn dest() -> AlertDestination {
        AlertDestination {
            channel_type: ChannelType::new("telegram"),
            platform_id: "operator-chat-123".into(),
            thread_id: None,
        }
    }

    /// A central DB with one active session, plus the temp sessions root its
    /// `outbound.db` lives under. Returns the carrier session so tests can
    /// read its outbound rows back.
    fn central_with_session(
        tmp: &tempfile::TempDir,
    ) -> (CentralDb, PathBuf, copperclaw_types::Session) {
        let db = CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let session = create_session(
            &db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        (db, tmp.path().to_path_buf(), session)
    }

    fn outbound_rows(
        sessions_root: &std::path::Path,
        session: &copperclaw_types::Session,
    ) -> Vec<copperclaw_types::MessageOutRow> {
        let paths = SessionPaths::new(sessions_root, session.agent_group_id, session.id);
        let conn = open_outbound(&paths).unwrap();
        messages_out::list_due(&conn).unwrap()
    }

    // ---- disabled-by-default: zero new outbound ------------------------

    #[tokio::test]
    async fn disabled_by_default_produces_zero_outbound() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        // No destination ⇒ disabled.
        let alerts = OperatorAlerts::with_destination(db, root.clone(), None);
        assert!(!alerts.is_enabled());

        alerts.fire(AlertSeverity::Critical, "loop.dead", "the sweep loop died");
        alerts.fire(AlertSeverity::Warning, "oom:abc", "session keeps OOMing");

        // The pre-O4 world: nothing but the log + metric. No outbound rows.
        assert!(
            outbound_rows(&root, &session).is_empty(),
            "disabled operator alerts must produce zero new outbound"
        );
    }

    #[tokio::test]
    async fn unconfigured_from_env_is_disabled() {
        // Belt-and-suspenders: with the env unset, from_env yields a disabled
        // instance. (Env is process-global; we only assert the disabled path,
        // never mutating other tests' env.)
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = OperatorAlerts::with_destination(db, root.clone(), None);
        alerts.fire(AlertSeverity::Critical, "k", "m");
        assert!(outbound_rows(&root, &session).is_empty());
    }

    // ---- configured: exactly one alert row to the destination ----------

    #[tokio::test]
    async fn configured_enqueues_exactly_one_row_to_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = OperatorAlerts::with_destination(db, root.clone(), Some(dest()));
        assert!(alerts.is_enabled());

        alerts.fire(
            AlertSeverity::Critical,
            "supervisor.degraded",
            "sweep loop degraded",
        );

        let rows = outbound_rows(&root, &session);
        assert_eq!(rows.len(), 1, "exactly one alert row enqueued");
        let row = &rows[0];
        assert_eq!(row.channel_type.as_ref().unwrap().as_str(), "telegram");
        assert_eq!(row.platform_id.as_deref(), Some("operator-chat-123"));
        assert_eq!(row.kind, MessageKind::Chat);
        let text = row.content.get("text").and_then(|t| t.as_str()).unwrap();
        assert!(
            text.contains("[copperclaw critical]"),
            "severity prefix: {text}"
        );
        assert!(text.contains("sweep loop degraded"), "message body: {text}");
    }

    // ---- dedup: one alert per episode, not per sweep pass --------------

    #[tokio::test(start_paused = true)]
    async fn dedup_collapses_repeat_fires_within_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = OperatorAlerts::with_destination(db, root.clone(), Some(dest()));

        // Ten "sweep passes" for the same still-broken condition.
        for _ in 0..10 {
            alerts.fire(AlertSeverity::Critical, "quarantine:s1", "db s1 corrupt");
            tokio::time::advance(Duration::from_secs(60)).await; // one sweep cadence
        }
        assert_eq!(
            outbound_rows(&root, &session).len(),
            1,
            "a persisting condition alerts once per episode, not once per pass"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dedup_re_alerts_after_the_window_elapses() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = OperatorAlerts::with_destination(db, root.clone(), Some(dest()));

        alerts.fire(AlertSeverity::Critical, "quarantine:s1", "db s1 corrupt");
        assert_eq!(outbound_rows(&root, &session).len(), 1);
        // Still inside the window: suppressed.
        tokio::time::advance(RE_ALERT_WINDOW - Duration::from_secs(1)).await;
        alerts.fire(AlertSeverity::Critical, "quarantine:s1", "db s1 corrupt");
        assert_eq!(outbound_rows(&root, &session).len(), 1, "still deduped");
        // Past the window: the episode re-alerts once.
        tokio::time::advance(Duration::from_secs(2)).await;
        alerts.fire(AlertSeverity::Critical, "quarantine:s1", "db s1 corrupt");
        assert_eq!(
            outbound_rows(&root, &session).len(),
            2,
            "re-alert after window"
        );
    }

    #[tokio::test]
    async fn distinct_keys_each_alert_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = OperatorAlerts::with_destination(db, root.clone(), Some(dest()));
        alerts.fire(AlertSeverity::Warning, "oom:a", "a ooming");
        alerts.fire(AlertSeverity::Warning, "oom:b", "b ooming");
        alerts.fire(AlertSeverity::Warning, "oom:a", "a ooming"); // deduped
        assert_eq!(
            outbound_rows(&root, &session).len(),
            2,
            "one per distinct key"
        );
    }

    // ---- rate limit: global token cap ---------------------------------

    #[tokio::test]
    async fn rate_limit_caps_a_burst_of_distinct_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = OperatorAlerts::with_destination(db, root.clone(), Some(dest()));
        // Far more distinct keys than the cap, all inside one window.
        for i in 0..(RATE_LIMIT_MAX + 25) {
            alerts.fire(AlertSeverity::Critical, &format!("k{i}"), "boom");
        }
        assert_eq!(
            outbound_rows(&root, &session).len(),
            RATE_LIMIT_MAX,
            "the global token cap bounds a fleet-wide burst"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_tokens_recover_after_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = OperatorAlerts::with_destination(db, root.clone(), Some(dest()));
        for i in 0..RATE_LIMIT_MAX {
            alerts.fire(AlertSeverity::Critical, &format!("k{i}"), "boom");
        }
        // Cap hit.
        alerts.fire(AlertSeverity::Critical, "extra", "boom");
        assert_eq!(outbound_rows(&root, &session).len(), RATE_LIMIT_MAX);
        // Window rolls past: tokens free up.
        tokio::time::advance(RATE_LIMIT_WINDOW + Duration::from_secs(1)).await;
        alerts.fire(AlertSeverity::Critical, "after-window", "boom");
        assert_eq!(
            outbound_rows(&root, &session).len(),
            RATE_LIMIT_MAX + 1,
            "tokens recover once the rolling window elapses"
        );
    }

    // ---- fail-closed: no carrier session ------------------------------

    #[tokio::test]
    async fn no_active_session_fails_closed_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        // Central with NO sessions at all.
        let db = CentralDb::open_in_memory().unwrap();
        let alerts = OperatorAlerts::with_destination(db, tmp.path().to_path_buf(), Some(dest()));
        // Must not panic; nothing to assert beyond "returns".
        alerts.fire(
            AlertSeverity::Critical,
            "supervisor.degraded",
            "no carrier here",
        );
    }

    // ---- integration: loop permanent-failure via degraded_watch -------

    #[tokio::test]
    async fn degraded_watch_fires_exactly_one_alert_on_the_flip() {
        let tmp = tempfile::tempdir().unwrap();
        let (db, root, session) = central_with_session(&tmp);
        let alerts = Arc::new(OperatorAlerts::with_destination(
            db,
            root.clone(),
            Some(dest()),
        ));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(Arc::clone(&alerts).run_degraded_watch(rx, shutdown.clone()));

        // Flip degraded → one alert. A second, redundant flip within the
        // window must not add a second row.
        tx.send(true).unwrap();
        // Give the watcher a chance to react.
        for _ in 0..50 {
            if !outbound_rows(&root, &session).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(false).unwrap();
        tx.send(true).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown.cancel();
        let _ = handle.await;

        assert_eq!(
            outbound_rows(&root, &session).len(),
            1,
            "a loop permanent-failure enqueues exactly one alert per episode"
        );
    }

    #[test]
    fn severity_tokens_and_parse_are_stable() {
        assert_eq!(AlertSeverity::Warning.as_str(), "warning");
        assert_eq!(AlertSeverity::Critical.as_str(), "critical");
        assert_eq!(AlertSeverity::from_token("warning"), AlertSeverity::Warning);
        assert_eq!(AlertSeverity::from_token("warn"), AlertSeverity::Warning);
        assert_eq!(
            AlertSeverity::from_token("critical"),
            AlertSeverity::Critical
        );
        assert_eq!(
            AlertSeverity::from_token("anything-else"),
            AlertSeverity::Critical
        );
    }
}
