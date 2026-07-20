//! Reconcile-loop classifier: decide what to do with a session this tick.

use super::crash_loop::CrashCause;
use super::spawn::container_name;
use super::{ContainerManager, ManagerError};
use copperclaw_channels_core::{ErrorCard, ErrorCardKind};
use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
use copperclaw_db::tables::messages_out::{WriteOutbound, insert as insert_outbound};
use copperclaw_db::tables::processing_ack::{self, ProcessingStatus};
use copperclaw_db::tables::{messages_in, sessions};
use copperclaw_host_sweep::APOLOGY_TRIES_MARKER;
use copperclaw_types::{ChannelType, ContainerStatus, MessageId, MessageKind, Session, SessionId};
use rusqlite::{OptionalExtension, params};
use std::time::Duration;
use tracing::{debug, info, warn};

/// User-facing apology emitted by the crash-restart path when one or
/// more inbound messages were in-flight on the dying container. Kept
/// distinct from the host-sweep `pending_too_long` apology text so an
/// operator scanning chat logs can tell which path fired. The tone is
/// deliberately concrete ("restart the agent container") so the user
/// understands the bot didn't ghost them.
pub(crate) const CRASH_RESTART_APOLOGY_TEXT: &str = "Hit a snag mid-task and need to restart the agent container. \
     Some progress may have been lost. I'll pick back up — try sending \
     a follow-up if I don't continue on my own.";

/// How many tail lines of container stdout/stderr to capture in the
/// crash-log file. ~200 covers the typical panic + immediate context
/// without bloating the session dir on a busy box.
const CRASH_LOG_TAIL_LINES: u32 = 200;

/// F2 poison-message quarantine threshold: how many times a single inbound
/// may be in flight during a crash-restart before the host gives up on it,
/// marks it terminally `failed` (so it is never re-processed), and emits the
/// one-time [`POISON_QUARANTINE_APOLOGY_TEXT`]. Set low: a message that
/// reliably crashes the runner three times running is poison (the canonical
/// case: a huge base64 screenshot plus an oversized history), and every
/// further retry is another guaranteed crash-loop iteration. Below this,
/// the message is still retried on the next spawn — a genuinely transient
/// crash (a provider blip mid-turn) recovers without losing the message.
const QUARANTINE_CRASH_ATTEMPTS: i64 = 3;

/// User-facing apology emitted exactly once when an inbound is quarantined
/// after crashing the runner [`QUARANTINE_CRASH_ATTEMPTS`] times. Distinct
/// from [`CRASH_RESTART_APOLOGY_TEXT`] (the transient-crash notice) so a user
/// scanning chat understands this message was permanently skipped, not
/// retried — and so an operator can tell the two apart in logs.
pub(crate) const POISON_QUARANTINE_APOLOGY_TEXT: &str = "One of your recent messages repeatedly crashed the agent, so I'm \
     skipping it to recover. If it had an attachment (like a large image or \
     file), try resending a smaller version or describing it in text instead.";

/// Title of the once-per-episode OOM `ErrorCard` (M21 S4).
const OOM_CARD_TITLE: &str = "Task keeps running out of memory";

/// Summary of the once-per-episode OOM `ErrorCard`. Emitted after
/// [`super::crash_loop::OOM_CARD_THRESHOLD`] OOM kills within one
/// crash-loop episode so the user learns the real cause instead of
/// watching an endless string of generic restart apologies. Names the
/// operator-side fix (`memory_mb`) concretely, per the house style of
/// actionable failure copy.
pub(crate) const OOM_CARD_SUMMARY: &str = "This task keeps crashing because the agent's container \
     runs out of memory. Restarts are being spaced out, but the task will likely keep failing at \
     the same point. An operator can raise the container's memory limit (the memory_mb field in \
     this group's container config, e.g. via `cclaw groups config edit <group-id>`).";

/// Why [`ContainerManager::restart_container`] is tearing a session's
/// container down. Drives the log wording, the crashed-containers
/// metric (crashes only), the exit-status inspection (crashes only —
/// a stuck container is still alive), and the stuck-tool state clear
/// (stuck restarts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartReason {
    /// Heartbeat stale or missing — the runner process is presumed
    /// dead ([`ReconcileAction::CrashRestart`]).
    Crash,
    /// M21 S2: the sweep found a tool past the absolute ceiling; the
    /// runner is alive but wedged ([`ReconcileAction::StuckRestart`]).
    StuckTool,
}

/// What the reconcile loop wants to do with a session this tick.
#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Nothing to do — session is in a healthy steady state.
    Noop,
    /// Spawn a fresh container for a `Stopped` session with pending
    /// inbound.
    Spawn,
    /// `Idle` session got new inbound; mark it `Stopped` so the next
    /// tick spawns. Two-step transition (rather than spawning here)
    /// because spawn needs to read the current `container_status`
    /// row and we don't want to race ourselves.
    WakeFromIdle,
    /// `Running` session has been quiet long enough — stop the
    /// container and mark `Idle`.
    IdleStop,
    /// `Running` session's heartbeat is stale — the runner has likely
    /// crashed. Stop best-effort and reset to `Stopped` for respawn.
    CrashRestart,
    /// M21 S2 (decision (a)): the sweep detected a tool running past
    /// its absolute ceiling
    /// (`copperclaw_host_sweep::ABSOLUTE_CEILING_MS`). The runner
    /// process is alive (its heartbeat stays fresh across tool
    /// dispatch — deliberately, see decision (a)), so [`Self::classify`]
    /// never produces this variant; it arrives exclusively through the
    /// manager's `StuckActuator` implementation
    /// ([`super::stuck_actuator`]). The teardown rides the same
    /// machinery as [`Self::CrashRestart`] (log capture, apologies,
    /// crash-loop backoff) plus a tool-state clear so the sweep does
    /// not re-fire against the fresh container.
    StuckRestart,
}

impl ContainerManager {
    /// Decide what to do with a single session based on its
    /// `container_status`, the inbound pending count, the heartbeat
    /// file's mtime, and the `last_active` timestamp. Pure: takes no
    /// async work and no DB writes so the state machine is unit-
    /// testable. `pub(crate)` (not `pub(super)`) so the M21 F3 boot
    /// tests can assert a reset session classifies as `Spawn`.
    pub(crate) fn classify(&self, session: &Session) -> ReconcileAction {
        let paths = SessionPaths::new(&self.cfg.data_dir, session.agent_group_id, session.id);
        // "No pending work" and "I could not read the inbound DB" are very
        // different facts that used to collapse into the same `false` via
        // `.unwrap_or(false)`. On a full disk `open_inbound` fails, every
        // Stopped session reads as idle, and the reconcile loop quietly
        // stops spawning anything — a total stall with NOT ONE log line to
        // find it by. The action is still Noop (there is nothing useful to
        // do with an unreadable DB, and spawning blind would be worse), but
        // it is now a stall you can see.
        let pending = match Self::has_pending_inbound(&paths) {
            Ok(pending) => pending,
            Err(err) => {
                warn!(
                    session = %session.id.as_uuid(),
                    ?err,
                    status = ?session.container_status,
                    "cannot read the session's inbound DB; treating as no pending work this tick \
                     (a full disk or an I/O error will stall this session until it clears)"
                );
                false
            }
        };
        match session.container_status {
            ContainerStatus::Stopped => {
                if pending {
                    // M21 S4: a session inside its crash-restart backoff
                    // window stays Stopped this tick — the teardown +
                    // apology already happened in `restart_container`;
                    // only the respawn is deferred. Sessions that never
                    // crashed have no tracker entry and spawn exactly as
                    // before.
                    if let Some(remaining) = self
                        .crash_loop
                        .spawn_delay_remaining(session.id, tokio::time::Instant::now())
                    {
                        debug!(
                            session = %session.id.as_uuid(),
                            remaining_secs = remaining.as_secs(),
                            "respawn deferred by crash-loop backoff"
                        );
                        ReconcileAction::Noop
                    } else {
                        ReconcileAction::Spawn
                    }
                } else {
                    ReconcileAction::Noop
                }
            }
            ContainerStatus::Idle => {
                if pending {
                    ReconcileAction::WakeFromIdle
                } else {
                    ReconcileAction::Noop
                }
            }
            ContainerStatus::Running => {
                if Self::heartbeat_stale(&paths, self.cfg.heartbeat_stale_secs).unwrap_or(false)
                    || Self::heartbeat_missing_and_session_old(
                        &paths,
                        session,
                        self.cfg.heartbeat_stale_secs,
                    )
                {
                    ReconcileAction::CrashRestart
                } else if Self::session_idle(session, self.cfg.idle_timeout_secs)
                    && Self::heartbeat_stale(&paths, self.cfg.idle_timeout_secs).unwrap_or(false)
                {
                    // Only idle when BOTH the session shows no recent
                    // inbound activity AND the runner's heartbeat says
                    // it's done working. Without the second clause,
                    // long-running tool loops (e.g. a research agent
                    // chaining 10+ web_search + read_file calls past
                    // 5 minutes) get killed mid-flight even though the
                    // runner is actively producing — the prior check
                    // measured time since the LAST INBOUND, not time
                    // since the runner did anything. Surfaced live as
                    // "spawned three research agents and the host
                    // killed them just before they could reply."
                    ReconcileAction::IdleStop
                } else {
                    ReconcileAction::Noop
                }
            }
        }
    }

    pub(super) async fn apply(
        &self,
        session: &Session,
        action: ReconcileAction,
    ) -> Result<(), ManagerError> {
        match action {
            ReconcileAction::Noop => Ok(()),
            ReconcileAction::Spawn => {
                // Degraded mode is a sticky, expected state — the
                // session stays Stopped and the inbound row stays
                // pending until the operator runs `./rebuild.sh`
                // and restarts. Surfacing it every tick would spam
                // the host log, so we collapse Ok(_) and
                // Err(HostDegraded) into Ok here. The startup
                // banner and the metric are the source of truth
                // for "host is degraded".
                //
                // `HostResourceExhausted` rides the same collapse for the
                // same reason, with one difference worth naming: it is
                // self-clearing. The box being out of disk is an expected,
                // transient, ALREADY-LOGGED condition (the tick's
                // `refresh_host_resources` edge-logs it once), and the
                // session stays Stopped with its inbound pending until the
                // environment recovers. Returning Err here would print one
                // "session reconcile failed" line per session per tick —
                // which is the 2026-07-18 log storm, just relabelled.
                match self.maybe_spawn(session).await {
                    Ok(_)
                    | Err(ManagerError::HostDegraded | ManagerError::HostResourceExhausted) => {
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            }
            ReconcileAction::WakeFromIdle => {
                sessions::mark_container_stopped(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                info!(session = %session.id.as_uuid(), "idle → stopped (pending inbound)");
                // Idle → Stopped is deliberately a two-tick transition
                // (the spawn wants the freshest session row). When the
                // event-driven wake is wired, chain straight into the next
                // tick so the spawn doesn't wait out another poll interval;
                // without it, the poll cadence picks the session up as
                // before.
                if let Some(wake) = &self.wake {
                    wake.notify_one();
                }
                Ok(())
            }
            ReconcileAction::IdleStop => {
                let name = container_name(session.agent_group_id, session.id);
                // Previews first: the container's bridge IP dies with the
                // container (and may be reassigned), so its proxies must not
                // outlive it.
                if let Some(preview) = &self.preview {
                    preview
                        .close_all_for_session(
                            session.id,
                            crate::preview::TeardownReason::SessionStop,
                        )
                        .await;
                }
                let _ = self
                    .runtime
                    .stop(&name, Duration::from_secs(self.cfg.stop_grace_secs))
                    .await;
                sessions::mark_container_idle(&self.central, session.id)
                    .map_err(ManagerError::Db)?;
                info!(session = %session.id.as_uuid(), "running → idle (no activity)");
                Ok(())
            }
            ReconcileAction::CrashRestart => {
                self.restart_container(session, RestartReason::Crash)
                    .await?;
                Ok(())
            }
            ReconcileAction::StuckRestart => {
                self.restart_container(session, RestartReason::StuckTool)
                    .await?;
                Ok(())
            }
        }
    }

    /// Body of [`ReconcileAction::CrashRestart`] and (M21 S2)
    /// [`ReconcileAction::StuckRestart`]. Captures container logs to a
    /// crash-log file, removes the container, emits a chat apology for
    /// every in-flight `processing_ack` row, marks those rows as Failed
    /// (so the host-sweep `processing` reset path doesn't double-fire),
    /// and stamps `messages_in.tries = APOLOGY_TRIES_MARKER` so the
    /// host-sweep `apology` `PendingTooLong` path also stays out.
    /// Finally marks the session container `Stopped` so a later
    /// reconcile tick respawns.
    ///
    /// M21 S4 additions (decision (e)): the container's exit status is
    /// inspected before removal to classify OOM kills distinctly, the
    /// crash is recorded against the per-session
    /// [`super::crash_loop::CrashLoopTracker`] (deferring the respawn on
    /// the 5s/15s/60s/300s curve via `classify`'s Stopped-arm gate), and
    /// the third OOM kill in an episode emits one user-facing `ErrorCard`.
    ///
    /// M21 S2 additions: `reason` distinguishes a genuine crash
    /// (heartbeat stale/missing — the runner is presumed dead) from a
    /// stuck-tool restart (the runner is alive but a tool ran past the
    /// sweep's absolute ceiling). A stuck restart skips the exit-status
    /// inspection (the container is still running; there is no terminal
    /// state, and a hung tool is never an OOM by construction), clears
    /// the `container_state` tool row that triggered the detection (so
    /// the next sweep pass doesn't re-fire against the fresh
    /// container), and leaves `copperclaw_containers_crashed_total`
    /// untouched — a deliberate restart is not a crash. Both reasons
    /// participate in the S4 crash-loop backoff: a session whose tool
    /// wedges immediately after every respawn must not hot-loop on the
    /// 60s sweep cadence.
    ///
    /// Idempotent: a second pass finds no `processing_ack` rows in
    /// `processing` status (the first pass marked them `Failed`) and
    /// the inbound rows are at `tries=99`, so no duplicate apology is
    /// emitted.
    pub(super) async fn restart_container(
        &self,
        session: &Session,
        reason: RestartReason,
    ) -> Result<(), ManagerError> {
        let name = container_name(session.agent_group_id, session.id);
        let paths = SessionPaths::new(&self.cfg.data_dir, session.agent_group_id, session.id);

        // 1. Capture last few hundred lines of stdout/stderr BEFORE the
        //    container disappears. Best-effort: any failure here is
        //    non-fatal — operators still have host logs + the apology
        //    row even if we can't archive the runner's last words.
        capture_crash_log(&*self.runtime, &name, &paths).await;

        // 1a. Inspect the container's terminal state BEFORE removal so an
        //     OOM kill (Docker `State.OOMKilled` / exit 137) classifies
        //     distinctly from a generic crash (M21 S4, decision (e)).
        //     Best-effort: an inspect failure or an already-gone container
        //     degrades to a generic crash — never guess OOM. Skipped for
        //     stuck-tool restarts: the container is still alive there, so
        //     it has no terminal state to inspect.
        let cause = match reason {
            RestartReason::Crash => {
                let exit_status = match self.runtime.exit_status(&name).await {
                    Ok(status) => status,
                    Err(err) => {
                        debug!(
                            container = %name,
                            ?err,
                            "could not inspect container exit status; classifying as generic crash"
                        );
                        None
                    }
                };
                CrashCause::from_exit_status(exit_status.as_ref())
            }
            RestartReason::StuckTool => CrashCause::Generic,
        };
        // Upgrade a generic crash to `ResourceExhausted` when the host's
        // disk probe says the box is full. An OOM kill keeps its own
        // classification (the memory ceiling is the more specific and more
        // actionable fact); everything else that dies on a full disk is
        // almost certainly dying *of* the full disk, and saying so here is
        // the difference between an operator following the thread and an
        // operator reading 235 identical `cause="generic"` lines.
        let disk = self.current_disk();
        let cause =
            cause.with_host_disk(disk.is_some_and(super::host_resources::DiskSpace::is_exhausted));

        // 1b. Tear down the session's previews before the container is
        //     removed — the crashed container's bridge IP is dead and may be
        //     reassigned to an unrelated container on respawn.
        if let Some(preview) = &self.preview {
            preview
                .close_all_for_session(session.id, crate::preview::TeardownReason::SessionStop)
                .await;
        }

        // 2. Remove (not just stop) so the next spawn doesn't collide
        //    on the container name. `remove` is a stop+rm that treats
        //    404 as success, so it's safe to call even when the
        //    container is already gone.
        let _ = self.runtime.remove(&name).await;

        // 2a. M21 S2, stuck restarts only: clear the `container_state`
        //     tool row that triggered the sweep's detection. Without
        //     this, the stale `tool_started_at` (already past the
        //     ceiling) would re-flag the session as stuck on every
        //     subsequent sweep pass and re-restart the fresh container
        //     as soon as it came up. Best-effort with a warn: if the
        //     clear fails, the actuator re-fires next pass — a repeat
        //     restart, not a wedge.
        if matches!(reason, RestartReason::StuckTool) {
            if let Err(err) = clear_stuck_tool_state(&paths) {
                warn!(
                    session = %session.id.as_uuid(),
                    ?err,
                    "could not clear stuck tool state; sweep may re-fire the restart"
                );
            }
        }

        // 3. Emit one chat apology per in-flight processing_ack row so
        //    the user knows the agent didn't ghost them. The host-sweep
        //    apology path used to be the only signal here, and waited up
        //    to APOLOGY_AFTER_SECS (5 min). The runner-restart path now
        //    fires the apology immediately.
        if let Err(err) = emit_crash_restart_apologies(&paths, session) {
            // Don't fail the whole tick on apology-emit failure — the
            // session still needs to be marked Stopped so the next
            // tick can respawn.
            warn!(
                session = %session.id.as_uuid(),
                ?err,
                "could not emit crash-restart apology rows"
            );
        }

        // 4. Existing behaviour: flip the session row + metric + warn.
        //    The crashed-containers metric fires only for genuine
        //    crashes — a stuck-tool restart is a deliberate recovery,
        //    and counting it as a crash would be the same flavour of lie
        //    S2's copy fix removes. Stuck restarts get their own series via
        //    the M21 M1 restart-by-reason counter below (distinct from the
        //    crash-only crashed-containers counter).
        sessions::mark_container_stopped(&self.central, session.id).map_err(ManagerError::Db)?;
        // M21 S2 (M1 rider): restarts by reason — crash vs deliberate
        // stuck-tool recovery.
        copperclaw_metrics::inc_container_restart(match reason {
            RestartReason::Crash => "crash",
            RestartReason::StuckTool => "stuck_tool",
        });
        match reason {
            RestartReason::Crash => {
                copperclaw_metrics::inc_containers_crashed();
                warn!(
                    session = %session.id.as_uuid(),
                    "heartbeat stale; running → stopped (will respawn)"
                );
            }
            RestartReason::StuckTool => {
                warn!(
                    session = %session.id.as_uuid(),
                    "stuck tool past absolute ceiling; running → stopped (will respawn)"
                );
            }
        }

        // 5. M21 S4 (decision (e)): record the crash against the
        //    per-session backoff/OOM tracker. The respawn is deferred by
        //    `classify`'s Stopped-arm gate until the backoff elapses;
        //    the third OOM kill in an episode emits the single
        //    user-facing "keeps running out of memory" ErrorCard.
        let record = self
            .crash_loop
            .record_crash(session.id, cause, tokio::time::Instant::now());
        // M21 S4 (M1 rider): OOM kills + how deep the respawn backoff got.
        if cause == CrashCause::OomKill {
            copperclaw_metrics::inc_container_oom_kill();
        }
        copperclaw_metrics::observe_crash_backoff_level(record.streak);
        // Free disk rides along on EVERY crash line, not just the ones we
        // classified as resource-exhausted: the whole failure of the
        // 2026-07-18 logs was that the crash record and the disk state
        // lived in different lines and nothing joined them.
        warn!(
            session = %session.id.as_uuid(),
            cause = cause.as_str(),
            crash_streak = record.streak,
            oom_count = record.oom_count,
            respawn_backoff_secs = record.delay.as_secs(),
            free_disk_bytes = disk.map(|d| d.free_bytes),
            "crash-restart recorded; respawn deferred by backoff"
        );
        // Feed the global circuit breaker. Per-session backoff (just
        // recorded above) answers "this session keeps crashing"; this
        // answers "several DIFFERENT sessions just crashed, so what they
        // share is broken." Only the trip edge has side effects, so a
        // fleet-wide fault costs exactly one operator alert.
        self.note_crash_for_env_breaker(session);
        if record.emit_oom_card {
            if let Err(err) = emit_oom_error_card(&paths) {
                // Non-fatal, same posture as the apology emit above: the
                // session is already Stopped and the backoff recorded;
                // the log line is the fallback signal.
                warn!(
                    session = %session.id.as_uuid(),
                    ?err,
                    "could not emit OOM crash-loop error card"
                );
            } else {
                info!(
                    session = %session.id.as_uuid(),
                    oom_count = record.oom_count,
                    "OOM crash-loop error card emitted"
                );
            }
            // M21 O4 (decision (d)): the OOM threshold is a once-per-episode
            // signal (`emit_oom_card` is true exactly once per episode), so
            // push it to the operator too when the opt-in alert destination is
            // wired + configured. A no-op otherwise. Dedup key is per-session
            // so a fleet of OOMing sessions each alerts independently but only
            // once per episode.
            if let Some(alerts) = self.operator_alerts.as_ref() {
                alerts.fire(
                    crate::operator_alerts::AlertSeverity::Warning,
                    &format!("oom:{}", session.id.as_uuid()),
                    &format!(
                        "A session keeps running out of memory ({} OOM kills this episode); \
                         restarts are backing off but the task will likely keep failing. \
                         An operator can raise `memory_mb` for agent group {}.",
                        record.oom_count,
                        session.agent_group_id.as_uuid()
                    ),
                );
            }
        }
        Ok(())
    }

    pub(super) fn has_pending_inbound(paths: &SessionPaths) -> Result<bool, ManagerError> {
        // Opening inbound here might create the DB file if it's
        // somehow missing; that's fine — `count_due` will return 0.
        let conn = open_inbound(paths).map_err(ManagerError::Db)?;
        let n = messages_in::count_due(&conn).map_err(ManagerError::Db)?;
        Ok(n > 0)
    }

    /// Whether the runner has stopped refreshing its `.heartbeat`
    /// file. Treats the file's mtime as the truth source; if the
    /// file doesn't exist yet, that's *not* stale — the runner may
    /// not have started writing it yet (containers take a moment to
    /// boot).
    pub(super) fn heartbeat_stale(
        paths: &SessionPaths,
        threshold_secs: u64,
    ) -> Result<bool, ManagerError> {
        let mtime = paths.heartbeat_mtime().map_err(ManagerError::Io)?;
        let Some(mtime) = mtime else { return Ok(false) };
        let age = std::time::SystemTime::now()
            .duration_since(mtime)
            .unwrap_or(std::time::Duration::ZERO);
        Ok(age > std::time::Duration::from_secs(threshold_secs))
    }

    /// Backstop for the case where the heartbeat file never appears at
    /// all — a container that crashed mid-boot, a short-lived child
    /// that processed its inbound and exited without ever writing the
    /// heartbeat, etc.
    ///
    /// [`Self::heartbeat_stale`] returns `false` when the file is
    /// missing (a freshly-spawned runner needs a few seconds to start
    /// writing). That's correct for newly-spawned sessions but turns
    /// into forever-stuck when a container dies before the first
    /// heartbeat is ever written: the DB stays `Running`, the manager
    /// keeps observing "no heartbeat file means not-stale," and the
    /// session never gets respawned to drain its inbound queue.
    ///
    /// This helper closes that gap: if the heartbeat file is missing
    /// AND `last_active` (which gets set at spawn time and updated by
    /// every heartbeat tick) is older than `threshold_secs`, the
    /// session is genuinely dead and the manager should reconcile.
    ///
    /// Lived through on 2026-05-24: 3 child research agents spawned,
    /// processed their kicker, exited; their containers were gone in
    /// `docker ps` but the DB held them in `Running` for 8+ minutes
    /// because the heartbeat file was never written.
    pub(super) fn heartbeat_missing_and_session_old(
        paths: &SessionPaths,
        session: &Session,
        threshold_secs: u64,
    ) -> bool {
        // Only triggers when the heartbeat file truly doesn't exist —
        // an IO error (permission, path, etc.) leaves the existing
        // `heartbeat_stale` path to handle it.
        let missing = matches!(paths.heartbeat_mtime(), Ok(None));
        missing && Self::session_idle(session, threshold_secs)
    }

    /// Whether `last_active` is older than the configured idle window.
    pub(super) fn session_idle(session: &Session, idle_window_secs: u64) -> bool {
        let now = chrono::Utc::now();
        let elapsed = now.signed_duration_since(session.last_active);
        elapsed.num_seconds() > i64::try_from(idle_window_secs).unwrap_or(i64::MAX)
    }
}

/// One in-flight inbound that needs a crash-restart apology. Pulled out
/// of the inbound DB by looking up the row matching a still-`processing`
/// `processing_ack` claim.
#[derive(Debug, Clone)]
struct InFlightRouting {
    message_id: MessageId,
    channel_type: ChannelType,
    platform_id: String,
    thread_id: Option<String>,
}

/// Capture the tail of the container's stdout/stderr to
/// `<session_root>/crash-<utc-rfc3339>.log`. Non-fatal — any failure is
/// logged at WARN and the caller continues.
///
/// The crash-log file goes in the session root dir (alongside
/// `runner.json`), NOT in `outbox/` (which is reserved for delivery
/// attachments).
async fn capture_crash_log(
    runtime: &dyn copperclaw_container_rt::ContainerRuntime,
    container_name: &str,
    paths: &SessionPaths,
) {
    // Path uses the UTC instant the host detected the crash so multiple
    // crash files don't clobber each other if a session crashes more
    // than once in a session's lifetime. RFC3339 with ':' replaced by
    // '-' keeps the filename portable across filesystems (Windows
    // doesn't allow ':').
    let now = chrono::Utc::now().to_rfc3339().replace(':', "-");
    let file_path = paths.root.join(format!("crash-{now}.log"));

    let body = match runtime.logs(container_name, CRASH_LOG_TAIL_LINES).await {
        Ok(body) => body,
        Err(err) if err.is_not_found() => {
            // The container is already gone (an operator ran `docker rm
            // -f`, or the daemon reaped it before we got here). There's
            // nothing to archive — an expected, uninteresting outcome,
            // so log at debug instead of spamming a warn every time.
            debug!(
                container = container_name,
                ?err,
                "container already gone; skipping crash-log capture"
            );
            return;
        }
        Err(err) => {
            warn!(
                container = container_name,
                ?err,
                "could not capture container logs before crash-restart removal"
            );
            return;
        }
    };

    // Empty body is legitimate (default-impl runtimes return ""); skip
    // writing the file in that case so we don't litter session dirs
    // with zero-byte placeholders.
    if body.is_empty() {
        return;
    }

    if let Err(err) = std::fs::create_dir_all(&paths.root) {
        warn!(
            path = %paths.root.display(),
            ?err,
            "could not ensure session root dir for crash log"
        );
        return;
    }

    if let Err(err) = std::fs::write(&file_path, body.as_bytes()) {
        warn!(
            path = %file_path.display(),
            ?err,
            "could not write crash log file"
        );
    }
}

/// M21 S2: null out the `container_state` tool fields in the session's
/// `outbound.db` after a stuck-tool restart. The stale row (its
/// `tool_started_at` is past the ceiling by definition) is what the
/// sweep's detection reads; clearing it is the dedupe that stops the
/// actuator re-firing against the freshly-respawned container. The
/// new runner writes fresh state on its next tool dispatch.
fn clear_stuck_tool_state(paths: &SessionPaths) -> Result<(), ManagerError> {
    let outbound = open_outbound(paths).map_err(ManagerError::Db)?;
    copperclaw_db::tables::container_state::clear_tool(&outbound).map_err(ManagerError::Db)
}

/// Scan in-flight `processing_ack` claims, emit one chat apology per
/// row with usable channel routing, mark each claim as Failed, and
/// stamp `messages_in.tries = APOLOGY_TRIES_MARKER` on the matching
/// inbound row so the host-sweep `apology` path also stays out.
///
/// All effects are idempotent across reconcile-tick repeats: the
/// scan filters by `processing_ack.status='processing'`, which the
/// first pass updates to `Failed`. A second pass therefore sees no
/// rows and emits no apologies.
fn emit_crash_restart_apologies(
    paths: &SessionPaths,
    session: &Session,
) -> Result<(), ManagerError> {
    let inbound = open_inbound(paths).map_err(ManagerError::Db)?;
    let outbound = open_outbound(paths).map_err(ManagerError::Db)?;

    let processing = list_processing_acks(&outbound)?;
    if processing.is_empty() {
        info!(
            session = %session.id.as_uuid(),
            "crash-restart: no in-flight processing_ack rows; no apology emitted"
        );
        return Ok(());
    }

    let now = chrono::Utc::now();
    let mut emitted = 0u32;
    let mut quarantined = 0u32;
    for message_id in processing {
        // F2 poison-message quarantine: bump this inbound's per-message crash
        // counter FIRST. `emit_crash_restart_apologies` runs exactly once per
        // crash-restart (and is idempotent across reconcile-tick repeats — it
        // scans `processing_ack.status='processing'` rows, which this pass
        // flips to `Failed`), so the counter advances by exactly one per crash
        // this message survives in flight. A missing row (already reaped /
        // idempotent repeat) yields no counter but still falls through to the
        // claim cleanup so we never spin on it.
        let attempts = match messages_in::increment_crash_attempts(&inbound, message_id) {
            Ok(n) => n,
            Err(err) => {
                warn!(
                    session = %session.id.as_uuid(),
                    message = %message_id.as_uuid(),
                    ?err,
                    "could not bump crash_attempts (inbound row missing?); continuing"
                );
                0
            }
        };

        // Look up the inbound row's routing. If the row is missing or
        // lacks channel routing, fall through to the mark-as-Failed
        // step so we don't loop on the same row again.
        let routing = lookup_inbound_routing(&inbound, message_id)?;

        if attempts >= QUARANTINE_CRASH_ATTEMPTS {
            // The message has crashed the runner K times: it is poison. Mark
            // the inbound row terminally `failed` so `get_pending` /
            // `count_due` (both `status='pending'`-scoped) stop returning it —
            // the retry loop is broken here, at the exact lifecycle point that
            // otherwise re-claims it on every respawn. Emit the distinct
            // one-time quarantine apology (once — after this, the row is no
            // longer pending, so no fresh claim and no repeat apology).
            if let Err(err) = messages_in::mark_failed(&inbound, message_id) {
                warn!(
                    session = %session.id.as_uuid(),
                    message = %message_id.as_uuid(),
                    ?err,
                    "could not quarantine (mark inbound failed); poison may retry"
                );
            }
            if let Some(routing) = &routing {
                let apology = WriteOutbound {
                    id: MessageId::new(),
                    in_reply_to: Some(routing.message_id),
                    timestamp: now,
                    deliver_after: None,
                    recurrence: None,
                    kind: MessageKind::Chat,
                    channel_type: Some(routing.channel_type.clone()),
                    platform_id: Some(routing.platform_id.clone()),
                    thread_id: routing.thread_id.clone(),
                    content: serde_json::json!({ "text": POISON_QUARANTINE_APOLOGY_TEXT }),
                };
                insert_outbound(&outbound, &apology).map_err(ManagerError::Db)?;
            }
            quarantined += 1;
            warn!(
                session = %session.id.as_uuid(),
                message = %message_id.as_uuid(),
                attempts,
                "poison inbound quarantined: crashed the runner too many times; skipping it"
            );
        } else {
            // Below the quarantine threshold: transient-crash path. Emit the
            // generic restart apology (if routed) and stamp the inbound so the
            // host-sweep `pending_too_long` apology path stays out. The row is
            // left `pending`, so the respawned runner retries it.
            if let Some(routing) = &routing {
                let apology = WriteOutbound {
                    id: MessageId::new(),
                    in_reply_to: Some(routing.message_id),
                    timestamp: now,
                    deliver_after: None,
                    recurrence: None,
                    kind: MessageKind::Chat,
                    channel_type: Some(routing.channel_type.clone()),
                    platform_id: Some(routing.platform_id.clone()),
                    thread_id: routing.thread_id.clone(),
                    content: serde_json::json!({ "text": CRASH_RESTART_APOLOGY_TEXT }),
                };
                insert_outbound(&outbound, &apology).map_err(ManagerError::Db)?;
                emitted += 1;
            }
            // Stamp the inbound row so the host-sweep apology path won't
            // also fire `pending_too_long` for it. We do this even when
            // routing was missing — the row stays pending but won't get a
            // second apology from the sweep.
            mark_inbound_tries(&inbound, message_id)?;
        }

        // Flip the claim to Failed so the host-sweep `processing` reset
        // path won't also fire and create a duplicate retry. The
        // runner-restart path owns this inbound from here on. Both the
        // quarantine and the retry path do this.
        if let Err(err) =
            processing_ack::update_status(&outbound, message_id, ProcessingStatus::Failed)
        {
            warn!(
                session = %session.id.as_uuid(),
                message = %message_id.as_uuid(),
                ?err,
                "could not mark processing_ack Failed; sweep may double-fire"
            );
        }
    }

    info!(
        session = %session.id.as_uuid(),
        emitted,
        quarantined,
        "crash-restart apologies emitted"
    );
    Ok(())
}

/// M21 F3: emit the host-restart recovery notice for one session during
/// the boot reset (`boot.rs::reset_stale_running_sessions`). When the
/// host itself dies mid-turn, boot flips the stale `running` row back to
/// `stopped` — but, unlike the live [`ReconcileAction::CrashRestart`]
/// path, nothing used to tell the user: the turn silently vanished and
/// the re-queued inbound processed later with no explanation.
///
/// This reuses the crash-restart apology machinery wholesale — the same
/// `processing_ack` scan, the same dedupe stamps, and the same copy
/// ([`CRASH_RESTART_APOLOGY_TEXT`], not a duplicate string) — with one
/// deliberate difference: at most ONE notice per session per boot, even
/// when several inbound rows were in flight. A host restart is a single
/// event from the user's point of view; one apology per queued message
/// would read as a malfunction.
///
/// Liveness-gated so a clean idle restart never fires it. Both gates
/// must pass:
///
/// 1. the session has pending unprocessed inbound (`count_due > 0` — the
///    same predicate the manager's respawn path uses), and
/// 2. at least one `processing_ack` claim is still `processing` — i.e. a
///    runner had actually picked a turn up when the host went down. A
///    message that merely arrived while the host was off has no claim
///    and will process normally on spawn; no apology is owed.
///
/// Dedup is the crash path's own: every in-flight claim is flipped to
/// `Failed` (so a second boot pass — and the sweep's stale-claim reset —
/// finds nothing) and its inbound row is stamped
/// `tries = APOLOGY_TRIES_MARKER` (so the sweep's `pending_too_long`
/// apology also stays out). The inbound rows are left `pending`, so the
/// respawned runner picks the turn back up.
///
/// Returns `Ok(true)` when a notice row was written.
pub fn emit_boot_recovery_notice(
    paths: &SessionPaths,
    session_id: SessionId,
) -> Result<bool, ManagerError> {
    let inbound = open_inbound(paths).map_err(ManagerError::Db)?;
    if messages_in::count_due(&inbound).map_err(ManagerError::Db)? == 0 {
        return Ok(false);
    }
    let outbound = open_outbound(paths).map_err(ManagerError::Db)?;
    let processing = list_processing_acks(&outbound)?;
    if processing.is_empty() {
        return Ok(false);
    }

    let now = chrono::Utc::now();
    let mut notice_written = false;
    for message_id in processing {
        let routing = lookup_inbound_routing(&inbound, message_id)?;
        if !notice_written {
            if let Some(routing) = &routing {
                let notice = WriteOutbound {
                    id: MessageId::new(),
                    in_reply_to: Some(routing.message_id),
                    timestamp: now,
                    deliver_after: None,
                    recurrence: None,
                    kind: MessageKind::Chat,
                    channel_type: Some(routing.channel_type.clone()),
                    platform_id: Some(routing.platform_id.clone()),
                    thread_id: routing.thread_id.clone(),
                    content: serde_json::json!({ "text": CRASH_RESTART_APOLOGY_TEXT }),
                };
                insert_outbound(&outbound, &notice).map_err(ManagerError::Db)?;
                notice_written = true;
            }
        }

        // Same dedupe stamps as `emit_crash_restart_apologies`: the
        // tries marker keeps the sweep's `pending_too_long` apology
        // out, and the Failed claim keeps both a repeat boot pass and
        // the sweep's stale-claim reset out. Rows without routing are
        // stamped too so they aren't rescanned forever.
        mark_inbound_tries(&inbound, message_id)?;
        if let Err(err) =
            processing_ack::update_status(&outbound, message_id, ProcessingStatus::Failed)
        {
            warn!(
                session = %session_id.as_uuid(),
                message = %message_id.as_uuid(),
                ?err,
                "could not mark processing_ack Failed; sweep may double-fire"
            );
        }
    }

    info!(
        session = %session_id.as_uuid(),
        notice_written,
        "boot recovery: in-flight turn found at host restart"
    );
    Ok(notice_written)
}

/// Emit the once-per-episode OOM `ErrorCard` (M21 S4): a
/// `MessageKind::Error` outbound row telling the user the task keeps
/// running out of memory and an operator can raise `memory_mb`.
///
/// Routing comes from the most recent chat-routed inbound row rather
/// than the in-flight `processing_ack` scan the apology path uses —
/// by the third OOM the first crash pass already marked those claims
/// `Failed`, so the apology scan would come up empty. When the session
/// has no chat-routed inbound at all (agent-to-agent, system-only)
/// there is nowhere to send the card and we skip, mirroring the
/// apology path's missing-routing posture. The caller's per-episode
/// dedup flag is already set either way, so a skipped card is not
/// retried every crash.
fn emit_oom_error_card(paths: &SessionPaths) -> Result<(), ManagerError> {
    let inbound = open_inbound(paths).map_err(ManagerError::Db)?;
    let Some(routing) = latest_routed_inbound(&inbound)? else {
        info!("OOM crash-loop card skipped: session has no chat-routed inbound");
        return Ok(());
    };
    let outbound = open_outbound(paths).map_err(ManagerError::Db)?;
    let card = ErrorCard::new(ErrorCardKind::Internal, OOM_CARD_SUMMARY).with_title(OOM_CARD_TITLE);
    let write = WriteOutbound {
        id: MessageId::new(),
        in_reply_to: Some(routing.message_id),
        timestamp: chrono::Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: MessageKind::Error,
        channel_type: Some(routing.channel_type),
        platform_id: Some(routing.platform_id),
        thread_id: routing.thread_id,
        content: serde_json::json!({ "error": card }),
    };
    insert_outbound(&outbound, &write).map_err(ManagerError::Db)?;
    Ok(())
}

/// The most recent inbound row that carries real channel routing
/// (non-empty `channel_type` + `platform_id`), or `None` when the
/// session has never seen a chat-routed inbound.
fn latest_routed_inbound(
    inbound: &rusqlite::Connection,
) -> Result<Option<InFlightRouting>, ManagerError> {
    let row: Option<(String, String, String, Option<String>)> = inbound
        .query_row(
            "SELECT id, channel_type, platform_id, thread_id FROM messages_in
             WHERE channel_type IS NOT NULL AND channel_type != ''
               AND platform_id IS NOT NULL AND platform_id != ''
             ORDER BY timestamp DESC, seq DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|e| ManagerError::Db(copperclaw_db::DbError::from(e)))?;
    let Some((id_str, channel_type, platform_id, thread_id)) = row else {
        return Ok(None);
    };
    let Ok(uuid) = uuid::Uuid::parse_str(&id_str) else {
        warn!(
            raw = id_str,
            "skipping unparseable messages_in.id for OOM card routing"
        );
        return Ok(None);
    };
    Ok(Some(InFlightRouting {
        message_id: MessageId(uuid),
        channel_type: ChannelType::from(channel_type),
        platform_id,
        thread_id,
    }))
}

/// Read every `processing_ack` row currently in `processing` status.
/// Returns just the `MessageId`s; downstream code does the inbound
/// lookup. Sorted by `status_changed ASC` to keep ordering
/// deterministic for tests.
fn list_processing_acks(outbound: &rusqlite::Connection) -> Result<Vec<MessageId>, ManagerError> {
    let mut stmt = outbound
        .prepare(
            "SELECT message_id FROM processing_ack
             WHERE status = 'processing'
             ORDER BY status_changed ASC",
        )
        .map_err(|e| ManagerError::Db(copperclaw_db::DbError::from(e)))?;
    let rows = stmt
        .query_map([], |row| {
            let id_str: String = row.get(0)?;
            Ok(id_str)
        })
        .map_err(|e| ManagerError::Db(copperclaw_db::DbError::from(e)))?;
    let mut out = Vec::new();
    for row in rows {
        let id_str = row.map_err(|e| ManagerError::Db(copperclaw_db::DbError::from(e)))?;
        match uuid::Uuid::parse_str(&id_str) {
            Ok(uuid) => out.push(MessageId(uuid)),
            Err(err) => {
                warn!(
                    raw = id_str,
                    ?err,
                    "skipping unparseable processing_ack.message_id"
                );
            }
        }
    }
    Ok(out)
}

/// Fetch a single inbound row by id and project to the columns the
/// crash-restart apology needs. Returns `Ok(None)` when:
///
/// - the row is missing (orphaned `processing_ack` row), or
/// - the row lacks BOTH `channel_type` and `platform_id` (not
///   chat-routed).
fn lookup_inbound_routing(
    inbound: &rusqlite::Connection,
    message_id: MessageId,
) -> Result<Option<InFlightRouting>, ManagerError> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = inbound
        .query_row(
            "SELECT channel_type, platform_id, thread_id FROM messages_in WHERE id = ?1",
            params![message_id.as_uuid().to_string()],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|e| ManagerError::Db(copperclaw_db::DbError::from(e)))?;

    let Some((channel_type, platform_id, thread_id)) = row else {
        return Ok(None);
    };

    match (channel_type, platform_id) {
        (Some(ct), Some(pid)) if !ct.is_empty() && !pid.is_empty() => Ok(Some(InFlightRouting {
            message_id,
            channel_type: ChannelType::from(ct),
            platform_id: pid,
            thread_id,
        })),
        _ => Ok(None),
    }
}

/// Stamp `tries = APOLOGY_TRIES_MARKER` on the matching inbound row.
/// Mirrors `host_sweep::apology::mark_tries_apology_sent`.
fn mark_inbound_tries(
    inbound: &rusqlite::Connection,
    message_id: MessageId,
) -> Result<(), ManagerError> {
    inbound
        .execute(
            "UPDATE messages_in SET tries = ?1 WHERE id = ?2",
            params![APOLOGY_TRIES_MARKER, message_id.as_uuid().to_string()],
        )
        .map_err(|e| ManagerError::Db(copperclaw_db::DbError::from(e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::spawn::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use std::path::PathBuf;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: SkillsMode::Inline,
            gpu_passthrough: false,
            forward_env: Vec::new(),
            egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
        }
    }

    fn fixture_session(db: &CentralDb) -> Session {
        let ag = create_ag(
            db,
            CreateAgentGroup {
                name: "demo".into(),
                folder: "demo".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        create_session(
            db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
    }

    fn make_mgr(tmp: &tempfile::TempDir) -> (ContainerManager, CentralDb) {
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        (mgr, db)
    }

    #[test]
    fn classify_stopped_without_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        // container_status defaults to Stopped per create_session.
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_stopped_with_pending_is_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::Spawn);
    }

    #[test]
    fn classify_running_with_fresh_heartbeat_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        session.last_active = chrono::Utc::now();
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_running_with_stale_heartbeat_is_crash_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // Backdate the heartbeat mtime to before the staleness window.
        // Default DEFAULT_HEARTBEAT_STALE_SECS is 120s (raised from 60s
        // to preserve a 2x margin over DEFAULT_PROVIDER_DEADLINE_MS —
        // see spawn.rs::DEFAULT_HEARTBEAT_STALE_SECS). 240s puts us
        // comfortably past the threshold with margin for test wall-clock
        // jitter instead of sitting right at the boundary.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(240);
        filetime::set_file_mtime(&paths.heartbeat, filetime::FileTime::from_system_time(old))
            .unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::CrashRestart);
    }

    #[test]
    fn classify_running_with_quiet_session_and_quiet_runner_is_idle_stop() {
        // Both signals quiet: no recent inbound AND the runner stopped
        // touching its heartbeat. This is the genuine "idle" case.
        let tmp = tempfile::tempdir().unwrap();
        let (_mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        session.last_active = chrono::Utc::now()
            - chrono::Duration::seconds(i64::try_from(DEFAULT_IDLE_TIMEOUT_SECS).unwrap() + 10);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // Backdate heartbeat past the idle window AND past the
        // crash-stale threshold's mid-zone — we want "idle, not
        // crashed," i.e. older than idle_timeout_secs but younger than
        // some fictional crash window. With defaults (120s crash, 300s
        // idle), an idle heartbeat would be ≥300s old but… in practice
        // anything older than idle_secs is also stale-as-crash. To
        // disambiguate we test a config where they're set apart:
        // crash=ridiculously long, idle=10s.
        let mut wide_cfg = manager_cfg(tmp.path().to_path_buf());
        wide_cfg.idle_timeout_secs = 10;
        wide_cfg.heartbeat_stale_secs = 86_400;
        let mgr_wide = ContainerManager::new(
            copperclaw_db::central::CentralDb::open_in_memory().unwrap(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            wide_cfg,
        );
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        filetime::set_file_mtime(
            &paths.heartbeat,
            filetime::FileTime::from_system_time(backdated),
        )
        .unwrap();
        assert_eq!(mgr_wide.classify(&session), ReconcileAction::IdleStop);
    }

    #[test]
    fn classify_running_with_missing_heartbeat_and_stale_session_is_crash_restart() {
        // Regression for the 2026-05-24 incident: a child container that
        // processed its inbound and exited WITHOUT EVER WRITING THE
        // HEARTBEAT FILE stayed in `Running` forever because
        // `heartbeat_stale()` returns `false` when the file doesn't
        // exist (treating the freshly-spawned grace window as "not
        // stale"). The backstop check `heartbeat_missing_and_session_old`
        // catches the "missing forever" case via last_active age.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        // last_active is older than heartbeat_stale_secs (120s default).
        session.last_active = chrono::Utc::now()
            - chrono::Duration::seconds(i64::try_from(DEFAULT_HEARTBEAT_STALE_SECS).unwrap() + 30);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        // Heartbeat file deliberately NOT created — this is the bug
        // condition the new helper guards against.
        assert!(!paths.heartbeat.exists());
        assert_eq!(mgr.classify(&session), ReconcileAction::CrashRestart);
    }

    #[test]
    fn classify_running_with_missing_heartbeat_but_recent_session_is_noop() {
        // Fresh spawns must NOT be crash-restarted while the runner is
        // still booting and writing its first heartbeat. The missing-file
        // backstop only fires when last_active is also stale.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        session.last_active = chrono::Utc::now();
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        assert!(!paths.heartbeat.exists());
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_running_with_quiet_session_but_active_runner_is_noop() {
        // Regression: the manager used to idle-stop any session whose
        // last_active was older than idle_timeout_secs, even if the
        // runner was actively producing work. Long-running tool loops
        // (research agents chaining 10+ web_search calls past 5 min)
        // got killed mid-flight because last_active is bumped by
        // inbound arrival, not by runner activity. The fix gates
        // IdleStop on heartbeat freshness AS WELL — if the runner is
        // ticking, it's not idle.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        // No recent inbound: backdate last_active past the idle window.
        session.last_active = chrono::Utc::now()
            - chrono::Duration::seconds(i64::try_from(DEFAULT_IDLE_TIMEOUT_SECS).unwrap() + 10);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        std::fs::write(&paths.heartbeat, b"").unwrap();
        // Heartbeat is fresh: runner is actively working.
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_idle_without_pending_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }

    #[test]
    fn classify_idle_with_pending_is_wake_from_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        assert_eq!(mgr.classify(&session), ReconcileAction::WakeFromIdle);
    }

    #[tokio::test]
    async fn apply_wake_from_idle_marks_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_idle(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Idle;
        mgr.apply(&session, ReconcileAction::WakeFromIdle)
            .await
            .unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    #[tokio::test]
    async fn apply_idle_stop_marks_idle_and_calls_runtime_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default());
        let mgr = ContainerManager::new(
            db.clone(),
            runtime.clone(),
            manager_cfg(tmp.path().to_path_buf()),
        );
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        mgr.apply(&session, ReconcileAction::IdleStop)
            .await
            .unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Idle));
    }

    #[tokio::test]
    async fn apply_crash_restart_marks_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    /// In-flight processing_ack row + chat-routed inbound → exactly
    /// one chat-kind outbound apology with the routing fields
    /// preserved. The processing_ack row flips to Failed and the
    /// inbound row's `tries` jumps to `APOLOGY_TRIES_MARKER` so the
    /// host-sweep apology + processing-reset paths both stay out.
    #[tokio::test]
    async fn crash_restart_emits_apology_for_in_flight_inbound() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;

        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();

        // Seed an inbound row + a matching processing_ack claim. This
        // is the "container picked up the message and was working on
        // it when it crashed" shape.
        let msg_id = copperclaw_types::MessageId::new();
        let inbound = open_inbound(&paths).unwrap();
        messages_in::insert(
            &inbound,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "what's the weather"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("tg-42".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("telegram")),
                thread_id: Some("thread-7".into()),
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(&outbound, msg_id, ProcessingStatus::Processing).unwrap();

        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();

        // Exactly one chat outbound row landed with the right routing.
        let chats: Vec<_> = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(chats.len(), 1, "expected exactly one apology");
        let apology = &chats[0];
        assert_eq!(apology.in_reply_to, Some(msg_id));
        assert_eq!(
            apology.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(apology.platform_id.as_deref(), Some("tg-42"));
        assert_eq!(apology.thread_id.as_deref(), Some("thread-7"));
        let text = apology
            .content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        assert!(
            text.contains("restart") && text.contains("agent container"),
            "apology text should mention restarting the agent: {text:?}"
        );

        // processing_ack row flipped to Failed so the host-sweep
        // processing-reset path won't double-fire.
        let claim = processing_ack::get(&outbound, msg_id).unwrap().unwrap();
        assert_eq!(claim.status, ProcessingStatus::Failed);

        // messages_in.tries was stamped so the host-sweep apology
        // PendingTooLong path won't double-fire.
        let tries: i64 = inbound
            .query_row(
                "SELECT tries FROM messages_in WHERE id = ?1",
                params![msg_id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tries, APOLOGY_TRIES_MARKER);

        // Idempotency: a second reconcile tick doesn't double-emit.
        // (The first pass marked the claim Failed, so the second pass
        // scans no rows and emits nothing.)
        let mut session2 = session.clone();
        sessions::mark_container_running(&db, session2.id).unwrap();
        session2.container_status = ContainerStatus::Running;
        mgr.apply(&session2, ReconcileAction::CrashRestart)
            .await
            .unwrap();
        let chats2: Vec<_> = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert_eq!(
            chats2.len(),
            1,
            "second crash-restart tick must not emit a duplicate apology"
        );
    }

    /// F2 (A) poison quarantine: an inbound that is in flight during
    /// `QUARANTINE_CRASH_ATTEMPTS` successive crashes is quarantined — marked
    /// terminally `failed` so it is never re-processed — and the user gets the
    /// distinct one-time "repeatedly crashed" apology. Below the threshold the
    /// message stays `pending` (still retried) and gets the transient-crash
    /// apology. Drives `emit_crash_restart_apologies` directly, re-establishing
    /// the `processing` claim between crashes exactly as a respawned runner
    /// re-claiming the still-pending inbound would.
    #[test]
    fn poison_inbound_quarantined_after_k_crashes_but_retries_under_k() {
        let tmp = tempfile::tempdir().unwrap();
        let (_mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();

        let msg_id = copperclaw_types::MessageId::new();
        let inbound = open_inbound(&paths).unwrap();
        messages_in::insert(
            &inbound,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "poison"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("tg-42".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("telegram")),
                thread_id: Some("thread-7".into()),
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(&outbound, msg_id, ProcessingStatus::Processing).unwrap();

        let read_status = |conn: &rusqlite::Connection| -> String {
            conn.query_row(
                "SELECT status FROM messages_in WHERE id = ?1",
                params![msg_id.as_uuid().to_string()],
                |r| r.get(0),
            )
            .unwrap()
        };

        // Crashes 1..K-1: below threshold → message stays pending (retried),
        // and it remains due for the next spawn.
        for attempt in 1..QUARANTINE_CRASH_ATTEMPTS {
            emit_crash_restart_apologies(&paths, &session).unwrap();
            assert_eq!(
                read_status(&inbound),
                "pending",
                "under K (attempt {attempt}) the message must stay pending for retry"
            );
            assert_eq!(
                messages_in::count_due(&inbound).unwrap(),
                1,
                "under K the message is still due for the next spawn"
            );
            // The respawned runner re-claims the still-pending inbound.
            processing_ack::update_status(&outbound, msg_id, ProcessingStatus::Processing).unwrap();
        }

        // The K-th crash quarantines it.
        emit_crash_restart_apologies(&paths, &session).unwrap();
        assert_eq!(
            read_status(&inbound),
            "failed",
            "at K crashes the poison message is quarantined (terminally failed)"
        );
        assert_eq!(
            messages_in::count_due(&inbound).unwrap(),
            0,
            "a quarantined message is no longer processed"
        );
        let attempts: i64 = inbound
            .query_row(
                "SELECT crash_attempts FROM messages_in WHERE id = ?1",
                params![msg_id.as_uuid().to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempts, QUARANTINE_CRASH_ATTEMPTS);

        // A distinct quarantine apology was emitted (not the transient one).
        let chats: Vec<_> = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        let quarantine_apology = chats.iter().any(|r| {
            r.content
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| t.contains("repeatedly crashed the agent"))
        });
        assert!(
            quarantine_apology,
            "expected the distinct poison-quarantine apology, got {chats:?}"
        );
    }

    /// F2 (A) idempotency: once quarantined (inbound `failed`, claim `Failed`),
    /// a further crash-restart pass finds no `processing` claim and neither
    /// re-quarantines nor emits a duplicate apology.
    #[test]
    fn quarantine_is_idempotent_across_repeat_passes() {
        let tmp = tempfile::tempdir().unwrap();
        let (_mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();

        let msg_id = copperclaw_types::MessageId::new();
        let inbound = open_inbound(&paths).unwrap();
        messages_in::insert(
            &inbound,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "poison"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("tg-9".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("telegram")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(&outbound, msg_id, ProcessingStatus::Processing).unwrap();

        // Drive it to quarantine.
        for _ in 0..QUARANTINE_CRASH_ATTEMPTS {
            emit_crash_restart_apologies(&paths, &session).unwrap();
            processing_ack::update_status(&outbound, msg_id, ProcessingStatus::Processing).unwrap();
        }
        let chats_before = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .count();

        // Now mark the claim Failed (the quarantine pass already did, but the
        // loop above re-set it to Processing on its last turn). Simulate the
        // sweep having reset it — a fresh pass with the message already
        // `failed` must not re-quarantine or re-apologise.
        processing_ack::update_status(&outbound, msg_id, ProcessingStatus::Failed).unwrap();
        emit_crash_restart_apologies(&paths, &session).unwrap();
        let chats_after = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .count();
        assert_eq!(
            chats_after, chats_before,
            "a repeat pass after quarantine must not emit more apologies"
        );
    }

    /// Empty inbound DB + no processing_ack rows → no apology row
    /// lands. Covers the corner case where the container died before
    /// picking up the inbound (or no inbound was in-flight at all).
    #[tokio::test]
    async fn crash_restart_emits_no_apology_without_in_flight_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;

        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        // Touch the per-session DBs so they exist (an inbound row may
        // exist without a processing_ack — that still means "nothing
        // was in-flight when the container died").
        let _ = open_inbound(&paths).unwrap();
        let outbound = open_outbound(&paths).unwrap();

        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();

        let chats: Vec<_> = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert!(
            chats.is_empty(),
            "no apology expected when no processing_ack rows are in-flight, got {chats:?}",
        );

        // And the session still flips to Stopped so the next tick respawns.
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    // ── M21 S4: crash-loop backoff + OOM classification ──────────────

    /// Backdate the session's heartbeat file so `classify` sees a stale
    /// runner (the crash condition).
    fn stale_heartbeat(paths: &SessionPaths) {
        std::fs::write(&paths.heartbeat, b"").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(240);
        filetime::set_file_mtime(&paths.heartbeat, filetime::FileTime::from_system_time(old))
            .unwrap();
    }

    /// Seed one chat-routed inbound row so the Stopped session wants to
    /// respawn (and the OOM card has routing to land on).
    fn seed_routed_inbound(paths: &SessionPaths) -> MessageId {
        let msg_id = MessageId::new();
        let conn = open_inbound(paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "build the thing"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("tg-42".into()),
                channel_type: Some(ChannelType::new("telegram")),
                thread_id: Some("thread-7".into()),
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        msg_id
    }

    /// Count `MessageKind::Error` rows currently in the outbound DB.
    fn count_error_rows(outbound: &rusqlite::Connection) -> usize {
        copperclaw_db::tables::messages_out::list_due(outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == MessageKind::Error)
            .count()
    }

    /// The heart of the S4 backoff acceptance: a crash-looping session
    /// is respawned at INCREASING intervals (5s, then 15s), not
    /// hot-looped once per reconcile tick. Paused tokio clock — zero
    /// real waits.
    #[tokio::test(start_paused = true)]
    async fn crash_loop_defers_respawn_at_increasing_intervals() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_routed_inbound(&paths);

        // Crash 1.
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        stale_heartbeat(&paths);
        assert_eq!(mgr.classify(&session), ReconcileAction::CrashRestart);
        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();
        session.container_status = ContainerStatus::Stopped;

        // Inside the first (5s) window: no respawn, despite pending
        // inbound. Before S4 this classified Spawn immediately — the
        // hot loop.
        assert_eq!(
            mgr.classify(&session),
            ReconcileAction::Noop,
            "respawn must be deferred right after the crash"
        );
        tokio::time::advance(std::time::Duration::from_secs(4)).await;
        assert_eq!(
            mgr.classify(&session),
            ReconcileAction::Noop,
            "still inside the 5s backoff window"
        );
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            mgr.classify(&session),
            ReconcileAction::Spawn,
            "first backoff (5s) elapsed"
        );

        // Crash 2: the interval escalates to 15s.
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;
        stale_heartbeat(&paths);
        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();
        session.container_status = ContainerStatus::Stopped;

        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        assert_eq!(
            mgr.classify(&session),
            ReconcileAction::Noop,
            "6s < the escalated 15s window: still deferred"
        );
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert_eq!(
            mgr.classify(&session),
            ReconcileAction::Spawn,
            "second backoff (15s) elapsed"
        );

        // A session that never crashed is untouched by the gate. (Same
        // group — `fixture_session` would collide on the unique group
        // folder.)
        let mut other = create_session(
            &db,
            CreateSession {
                agent_group_id: session.agent_group_id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        other.container_status = ContainerStatus::Stopped;
        let other_paths = SessionPaths::new(tmp.path(), other.agent_group_id, other.id);
        other_paths.ensure_dirs().unwrap();
        seed_routed_inbound(&other_paths);
        assert_eq!(
            mgr.classify(&other),
            ReconcileAction::Spawn,
            "never-crashed sessions spawn exactly as before"
        );
    }

    /// Three OOM kills in one episode emit exactly ONE ErrorCard; a
    /// fourth OOM does not add another. The card carries the routing of
    /// the session's latest chat inbound and names memory_mb.
    #[tokio::test(start_paused = true)]
    async fn oom_crash_loop_emits_error_card_exactly_once_per_episode() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default().with_exit_status(
            copperclaw_container_rt::ContainerExitStatus {
                exit_code: Some(137),
                oom_killed: true,
            },
        ));
        let mgr = ContainerManager::new(db.clone(), runtime, manager_cfg(tmp.path().to_path_buf()));
        let mut session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let msg_id = seed_routed_inbound(&paths);
        let outbound = open_outbound(&paths).unwrap();

        for crash in 1u32..=4 {
            sessions::mark_container_running(&db, session.id).unwrap();
            session.container_status = ContainerStatus::Running;
            mgr.apply(&session, ReconcileAction::CrashRestart)
                .await
                .unwrap();
            let expected = usize::from(crash >= 3);
            assert_eq!(
                count_error_rows(&outbound),
                expected,
                "after OOM crash {crash}: exactly one card from the third on, never two"
            );
        }

        // The card is well-formed: internal kind, memory_mb guidance,
        // routed at the latest chat inbound.
        let card_row = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .find(|r| r.kind == MessageKind::Error)
            .expect("one OOM error card");
        assert_eq!(card_row.in_reply_to, Some(msg_id));
        assert_eq!(
            card_row.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(card_row.platform_id.as_deref(), Some("tg-42"));
        assert_eq!(card_row.thread_id.as_deref(), Some("thread-7"));
        let card: copperclaw_channels_core::ErrorCard =
            serde_json::from_value(card_row.content["error"].clone()).unwrap();
        assert_eq!(card.kind, copperclaw_channels_core::ErrorCardKind::Internal);
        assert!(card.validate().is_ok(), "card must pass schema validation");
        assert!(
            card.summary.contains("out of memory") && card.summary.contains("memory_mb"),
            "summary names the cause and the operator fix: {}",
            card.summary
        );
    }

    /// Generic (non-OOM) crashes never emit the OOM card, no matter how
    /// many pile up — their user surface stays exactly the pre-S4
    /// apology machinery.
    #[tokio::test(start_paused = true)]
    async fn generic_crash_loop_never_emits_oom_card() {
        let tmp = tempfile::tempdir().unwrap();
        // Default NoopRuntime: exit_status reports None (uninspectable),
        // which must classify as a generic crash.
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        seed_routed_inbound(&paths);
        let outbound = open_outbound(&paths).unwrap();

        for _ in 0..4 {
            sessions::mark_container_running(&db, session.id).unwrap();
            session.container_status = ContainerStatus::Running;
            mgr.apply(&session, ReconcileAction::CrashRestart)
                .await
                .unwrap();
        }
        assert_eq!(
            count_error_rows(&outbound),
            0,
            "generic crashes must never surface the OOM card"
        );
        // The session still lands Stopped for the (backed-off) respawn.
        let updated = sessions::get(&db, session.id).unwrap();
        assert!(matches!(updated.container_status, ContainerStatus::Stopped));
    }

    /// A session with no chat-routed inbound (agent-to-agent / system
    /// only) has nowhere to send the OOM card: the emit is skipped
    /// cleanly and never retried within the episode.
    #[tokio::test(start_paused = true)]
    async fn oom_card_skipped_without_chat_routing() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let runtime = std::sync::Arc::new(crate::tests::NoopRuntime::default().with_exit_status(
            copperclaw_container_rt::ContainerExitStatus {
                exit_code: Some(137),
                oom_killed: false,
            },
        ));
        let mgr = ContainerManager::new(db.clone(), runtime, manager_cfg(tmp.path().to_path_buf()));
        let mut session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        // No inbound rows at all.
        let _ = open_inbound(&paths).unwrap();
        let outbound = open_outbound(&paths).unwrap();

        for _ in 0..4 {
            sessions::mark_container_running(&db, session.id).unwrap();
            session.container_status = ContainerStatus::Running;
            mgr.apply(&session, ReconcileAction::CrashRestart)
                .await
                .unwrap();
        }
        assert_eq!(count_error_rows(&outbound), 0, "no routing, no card");
    }

    /// An in-flight processing_ack row whose inbound has NO channel
    /// routing (no `channel_type` / `platform_id`) must NOT emit an
    /// apology — there's nowhere to send it — but the processing_ack
    /// row should still flip to Failed and the inbound's `tries`
    /// should still be stamped so the host-sweep paths stay out.
    #[tokio::test]
    async fn crash_restart_skips_apology_when_routing_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let mut session = fixture_session(&db);
        sessions::mark_container_running(&db, session.id).unwrap();
        session.container_status = ContainerStatus::Running;

        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let msg_id = copperclaw_types::MessageId::new();
        let inbound = open_inbound(&paths).unwrap();
        messages_in::insert(
            &inbound,
            &messages_in::WriteInbound {
                id: msg_id,
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "task"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: None,
                channel_type: None,
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        let outbound = open_outbound(&paths).unwrap();
        processing_ack::insert(&outbound, msg_id, ProcessingStatus::Processing).unwrap();

        mgr.apply(&session, ReconcileAction::CrashRestart)
            .await
            .unwrap();

        let chats: Vec<_> = copperclaw_db::tables::messages_out::list_due(&outbound)
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == copperclaw_types::MessageKind::Chat)
            .collect();
        assert!(chats.is_empty(), "no apology when routing is missing");
        let claim = processing_ack::get(&outbound, msg_id).unwrap().unwrap();
        assert_eq!(claim.status, ProcessingStatus::Failed);
        let tries: i64 = inbound
            .query_row(
                "SELECT tries FROM messages_in WHERE id = ?1",
                params![msg_id.as_uuid().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tries, APOLOGY_TRIES_MARKER);
    }
}

/// Host-resource preflight + global circuit breaker (the 2026-07-18
/// full-disk incident).
///
/// Everything here exercises a gap the pre-existing state-machine tests
/// left wide open: none of them ever ran `classify` / `maybe_spawn`
/// against a host that could not actually support a container, so the
/// storm those conditions produce was invisible to the suite.
#[cfg(test)]
mod host_resource_tests {
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::host_resources::{
        ENV_FAULT_SESSION_THRESHOLD, ENV_FAULT_WINDOW, clear_disk_override, set_disk_override,
    };
    use super::super::spawn::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use copperclaw_cclaw::disk::GIB;
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::sessions::{CreateSession, create as create_session};
    use std::path::PathBuf;

    /// A filesystem with 64 KiB left on a 100 GiB volume — the shape the
    /// host was in when `tasks_snapshot: write failed err=Os { code: 28,
    /// StorageFull }` started appearing.
    const FULL_DISK: (u64, u64) = (64 * 1024, 100 * GIB);
    /// A comfortably healthy filesystem.
    const HEALTHY_DISK: (u64, u64) = (500 * GIB, 1024 * GIB);

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: Some("https://openrouter.ai/api/v1".into()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: SkillsMode::Inline,
            gpu_passthrough: false,
            forward_env: Vec::new(),
            egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
        }
    }

    fn make_mgr(tmp: &tempfile::TempDir) -> (ContainerManager, CentralDb) {
        let db = CentralDb::open_in_memory().unwrap();
        let mgr = ContainerManager::new(
            db.clone(),
            std::sync::Arc::new(crate::tests::NoopRuntime::default()),
            manager_cfg(tmp.path().to_path_buf()),
        );
        (mgr, db)
    }

    fn fixture_session(db: &CentralDb) -> Session {
        // Unique name/folder per call: these tests deliberately create
        // SEVERAL sessions against one DB (distinct sessions being the
        // entire point of the breaker), and agent-group folders are unique.
        let slug = format!("demo-{}", uuid::Uuid::new_v4());
        let ag = create_ag(
            db,
            CreateAgentGroup {
                name: slug.clone(),
                folder: slug,
                agent_provider: None,
            },
        )
        .unwrap();
        create_session(
            db,
            CreateSession {
                agent_group_id: ag.id,
                messaging_group_id: None,
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap()
    }

    /// A session with one pending chat inbound waiting — i.e. one that
    /// `classify` will want to `Spawn`.
    fn session_with_pending_inbound(tmp: &tempfile::TempDir, db: &CentralDb) -> Session {
        let session = fixture_session(db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({"text": "hi"}),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("stdin".into()),
                channel_type: Some(copperclaw_types::ChannelType::new("cli")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        session
    }

    #[tokio::test]
    async fn spawn_preflight_refuses_a_container_when_the_disk_is_full() {
        // The core regression. Before the preflight, this session spawned
        // a container into a filesystem with no room, the container died
        // mid-boot, and the reconcile loop read the dead container as a
        // per-session crash — the first step of the storm.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = session_with_pending_inbound(&tmp, &db);
        set_disk_override(FULL_DISK.0, FULL_DISK.1);

        assert!(
            mgr.spawn_blocked_by_host_resources().is_some(),
            "a full disk must block the spawn preflight"
        );
        let err = mgr
            .maybe_spawn(&session)
            .await
            .expect_err("spawn must be refused on a full disk");
        assert!(
            matches!(err, ManagerError::HostResourceExhausted),
            "expected HostResourceExhausted, got {err:?}"
        );
        clear_disk_override();
    }

    #[tokio::test]
    async fn refused_spawn_leaves_the_session_stopped_with_its_inbound_pending() {
        // Refusing is only correct if it loses nothing: the session must
        // stay Stopped (so a later tick retries) and the inbound must stay
        // pending (so the user's message is not dropped on the floor).
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = session_with_pending_inbound(&tmp, &db);
        set_disk_override(FULL_DISK.0, FULL_DISK.1);

        assert_eq!(mgr.classify(&session), ReconcileAction::Spawn);
        // `apply` collapses the refusal to Ok — one edge-logged warning
        // per episode, NOT one "session reconcile failed" line per session
        // per tick, which would just be the old log storm relabelled.
        mgr.apply(&session, ReconcileAction::Spawn)
            .await
            .expect("resource exhaustion collapses to Ok, like HostDegraded");

        let row = copperclaw_db::tables::sessions::get(&db, session.id).unwrap();
        assert_eq!(row.container_status, ContainerStatus::Stopped);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        assert!(
            ContainerManager::has_pending_inbound(&paths).unwrap(),
            "the inbound must still be pending after a refused spawn"
        );
        clear_disk_override();
    }

    #[tokio::test]
    async fn a_healthy_disk_does_not_block_anything() {
        // Guard against the preflight over-firing: WARN-level free space
        // (low but not critical) must NOT stop work.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _db) = make_mgr(&tmp);
        set_disk_override(HEALTHY_DISK.0, HEALTHY_DISK.1);
        assert!(mgr.spawn_blocked_by_host_resources().is_none());
        // 50% free but under the 20 GiB absolute floor → WARN, not FAIL.
        set_disk_override(15 * GIB, 30 * GIB);
        mgr.refresh_host_resources();
        assert!(
            mgr.spawn_blocked_by_host_resources().is_none(),
            "a WARN-level disk must not freeze the fleet"
        );
        assert!(!mgr.is_env_exhausted());
        clear_disk_override();
    }

    #[tokio::test(start_paused = true)]
    async fn env_breaker_pauses_spawns_globally_then_re_arms_when_the_disk_recovers() {
        // The piece that actually caps the storm. N distinct sessions
        // crashing against one shared fault must produce ONE global pause,
        // not N independent per-session backoff curves all retrying
        // forever against the same full disk.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        set_disk_override(HEALTHY_DISK.0, HEALTHY_DISK.1);
        mgr.refresh_host_resources();

        let sessions: Vec<Session> = (0..ENV_FAULT_SESSION_THRESHOLD)
            .map(|_| fixture_session(&db))
            .collect();
        for (i, session) in sessions.iter().enumerate() {
            mgr.note_crash_for_env_breaker(session);
            let last = i + 1 == ENV_FAULT_SESSION_THRESHOLD;
            assert_eq!(
                mgr.is_env_exhausted(),
                last,
                "the breaker trips on the crossing, not before (crash {})",
                i + 1
            );
        }
        // Spawns are now blocked for EVERY session, including ones that
        // never crashed — the fault is the box, not the session.
        let bystander = session_with_pending_inbound(&tmp, &db);
        assert!(mgr.spawn_blocked_by_host_resources().is_some());
        let err = mgr.maybe_spawn(&bystander).await.expect_err("blocked");
        assert!(matches!(err, ManagerError::HostResourceExhausted));

        // Mid-window: still paused, even with a perfectly healthy disk.
        tokio::time::advance(ENV_FAULT_WINDOW / 2).await;
        mgr.refresh_host_resources();
        assert!(mgr.is_env_exhausted(), "the hold must survive one tick");

        // Past the hold with a healthy disk: work resumes on its own.
        tokio::time::advance(ENV_FAULT_WINDOW).await;
        mgr.refresh_host_resources();
        assert!(!mgr.is_env_exhausted(), "self-clears once the box is well");
        assert!(mgr.spawn_blocked_by_host_resources().is_none());
        clear_disk_override();
    }

    #[tokio::test]
    async fn operator_can_clear_the_environment_fault_by_hand() {
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        set_disk_override(HEALTHY_DISK.0, HEALTHY_DISK.1);
        for _ in 0..ENV_FAULT_SESSION_THRESHOLD {
            let session = fixture_session(&db);
            mgr.note_crash_for_env_breaker(&session);
        }
        assert!(mgr.is_env_exhausted());
        mgr.clear_env_exhausted();
        assert!(!mgr.is_env_exhausted());
        assert!(mgr.spawn_blocked_by_host_resources().is_none());
        clear_disk_override();
    }

    #[tokio::test]
    async fn a_full_disk_alone_flips_and_clears_the_environment_state() {
        // No crashes needed: the probe alone is enough to stop burning
        // doomed spawns, and enough to resume once space comes back.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, _db) = make_mgr(&tmp);
        set_disk_override(FULL_DISK.0, FULL_DISK.1);
        mgr.refresh_host_resources();
        assert!(mgr.is_env_exhausted());

        set_disk_override(HEALTHY_DISK.0, HEALTHY_DISK.1);
        mgr.refresh_host_resources();
        assert!(!mgr.is_env_exhausted());
        clear_disk_override();
    }

    #[test]
    fn classify_noops_and_warns_when_the_inbound_db_cannot_be_read() {
        // Silent-stall regression. `has_pending_inbound` used to be
        // `.unwrap_or(false)`, so an unreadable inbound DB looked exactly
        // like "no work to do": every Stopped session went Noop forever
        // and NOT ONE log line said why. The action is still Noop (there
        // is nothing useful to do with a DB you cannot read), but it is
        // now an observable stall rather than an invisible one.
        let tmp = tempfile::tempdir().unwrap();
        let (mgr, db) = make_mgr(&tmp);
        let session = fixture_session(&db);
        let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
        paths.ensure_dirs().unwrap();
        // Make `open_inbound` fail the way a broken filesystem does: put a
        // directory where the DB file belongs.
        std::fs::create_dir_all(&paths.inbound_db).unwrap();

        assert!(
            ContainerManager::has_pending_inbound(&paths).is_err(),
            "the fixture must actually break the inbound open"
        );
        assert_eq!(mgr.classify(&session), ReconcileAction::Noop);
    }
}
